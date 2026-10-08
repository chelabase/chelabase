//! Schema merges (step 6g): the merge consumer's jobs.
//!
//! - [`compute`]: `computing` → `draft`: reads the base, the source and the target, analyzes
//!   them, keeps the resolutions that still apply and plans the merge when nothing is open.
//! - [`rehearse`]: `rehearsing` → `rehearsed` (or back to `draft`): the plan on a throwaway
//!   branch of the target, and the cancel job's cleanup.
//! - [`apply`]: `applying` → `online` → `finishing`: the plan on the target, its recovery after
//!   a crash, and the online phase's resume.
//! - [`finish`]: `finishing` → `done` or `applied_with_drift`: the drift check, the created
//!   roles and the after-merge choice.
//! - [`phases`]: the phase executor ([`Worker::run_phases`], [`Worker::run_online`]).
//! - [`claim`]: the claim on a merge in a working status, refreshed while a job runs.
//!
//! The API moves a merge to its working status before publishing its job (T10), so a job
//! starts from a row already in that status, and does nothing when it isn't (cancelled meanwhile,
//! or handled by an earlier delivery). A `Resume` (reconcile's re-queue, or the API's retry)
//! carries on from whatever working status the merge is in.

use std::time::Duration;

use chelabase_metadata::{BranchRecord, BranchStatus, MergeEvent, MergeRecord, MergeStatus};
use chelabase_nats::MergeJob;
use chelabase_neon::ComputeBackend;
use chelabase_schema::SchemaModel;
use chelabase_schema::analysis::{FollowsRename, ObjKey};
use chelabase_schema::plan::{Plan, PlanOptions, Step};
use chelabase_types::BranchId;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use uuid::Uuid;

use futures_util::future::BoxFuture;

use crate::Worker;
use crate::branch_job::Attempt;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};

mod apply;
mod claim;
mod compute;
mod finish;
pub(crate) mod phases;
mod rehearse;
#[cfg(test)]
mod testkit;

pub(crate) use rehearse::rehearsal_name;

/// P3: the admin timeout of phase 1 (and each phase 1a step), above the script's own 15 min
/// `statement_timeout`.
pub const MERGE_TX_TIMEOUT: Duration = Duration::from_secs(16 * 60);

/// P3: the admin timeout of one phase 2 step.
pub const MERGE_ONLINE_STEP_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);

/// R3: reconcile hands a merge back (`Resume`) once its claim is this old: three of the
/// worker's claim refreshes (T14 M10), so a slow refresh never hands back a running merge.
pub const STALE_MERGE_CLAIM: Duration = Duration::from_secs(180);

/// T14 M10: a merge handed back isn't handed back again for this long while no worker claimed
/// it since (its `Resume` waits behind a long job on the one-at-a-time merge consumer, P11);
/// after it, a lost job is replaced.
pub const REHAND_AFTER: Duration = Duration::from_secs(30 * 60);

/// T14 M8: a requested after-merge choice (`finishMerge`) still waiting after this long lost
/// its `Finish` job; reconcile publishes it again.
pub const STALE_FINISH_REQUEST: Duration = Duration::from_secs(10 * 60);

/// A merge's `plan` column: the planner's steps and the target's digest read at compute time
/// (E4), which phase 1's guard compares with; and what the rehearsal and the apply compare the
/// result with: the model the target should read back afterwards (P7: the merged model plus
/// the grants the target's default privileges give new objects) and the keys own SQL replaced
/// (their differences are what it produced, not drift). The API reads the same document as a
/// `chelabase_schema::plan::Plan` (the other fields are extra).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct StoredPlan {
    pub steps: Vec<Step>,
    pub target_digest: String,
    /// `None` in a plan computed before T8b: such a merge is recomputed first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<SchemaModel>,
    #[serde(default)]
    pub own_sql: Vec<ObjKey>,
    /// The apply's pre-check fingerprint of the target (I1, fix round 2):
    /// `chelabase_schema::fingerprint::precheck_fingerprint` of the target's model and these
    /// steps, so a retry doesn't count what the plan's phases 0 and 1a already did as a change.
    /// `None` in an older plan: the full `target_fingerprint` is compared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub precheck_fingerprint: Option<String>,
    /// E31: target objects whose text names something the merge renames
    /// (`MergedModel::follows_rename`): Postgres rewrites their text, so they're checked
    /// against what the rehearsal produced, not `expected`'s text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub follows_rename: Vec<FollowsRename>,
    /// E31: `expected` already holds the rehearsal's version of each [`Self::follows_rename`]
    /// object (`compare::with_rehearsed`, written by the rehearsal); until then the finish
    /// counts their differences as produced.
    #[serde(default)]
    pub expected_rehearsed: bool,
}

impl StoredPlan {
    pub fn plan(&self) -> Plan {
        Plan {
            steps: self.steps.clone(),
        }
    }
}

/// A merge's `options.lock_timeout_ms` (R2), as the planner and the export read it.
fn lock_timeout(options: &serde_json::Value) -> u32 {
    chelabase_schema::plan::lock_timeout_ms(options)
}

/// The planner's options for `merge`: its id and `options.lock_timeout_ms` (R2).
pub(crate) fn plan_options(merge: &MergeRecord) -> PlanOptions {
    PlanOptions {
        lock_timeout_ms: lock_timeout(&merge.options),
        merge_id: merge.id,
    }
}

/// The merge's stored plan, or why it can't be run.
pub(crate) fn stored_plan(merge: &MergeRecord) -> Result<StoredPlan, String> {
    let value = merge
        .plan
        .clone()
        .ok_or_else(|| "the merge has no plan".to_string())?;
    let stored: StoredPlan =
        serde_json::from_value(value).map_err(|e| format!("the stored plan is unreadable: {e}"))?;
    if stored.expected.is_none() {
        return Err("the plan predates the drift check; recompute the merge".to_string());
    }
    Ok(stored)
}

/// The conflicts of the merge's analysis that no stored resolution answers (the API's
/// `open_conflicts`, M8).
pub(crate) fn open_conflicts(merge: &MergeRecord) -> Vec<String> {
    let resolved: Vec<&str> = merge
        .resolutions
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["conflict"].as_str())
        .collect();
    merge
        .analysis
        .as_ref()
        .and_then(|a| a["conflicts"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|c| c["id"].as_str())
        .filter(|id| !resolved.contains(id))
        .map(str::to_string)
        .collect()
}

/// The items of the plan's steps that need an acknowledgement (`Step::needs_acknowledgement`:
/// data loss, security, a breaking role rename, E29a) no acknowledgement covers; a step with no
/// items is covered by its own id (the API's `unacknowledged`, M8).
pub(crate) fn unacknowledged(merge: &MergeRecord, plan: &StoredPlan) -> Vec<String> {
    let acked: Vec<&str> = merge
        .acknowledgements
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .collect();
    let mut missing: Vec<String> = Vec::new();
    for step in plan.steps.iter().filter(|s| s.needs_acknowledgement()) {
        let wanted: Vec<&str> = if step.items.is_empty() {
            vec![step.id.0.as_str()]
        } else {
            step.items.iter().map(|i| i.0.as_str()).collect()
        };
        missing.extend(
            wanted
                .into_iter()
                .filter(|w| !acked.contains(w))
                .map(str::to_string),
        );
    }
    missing.sort();
    missing.dedup();
    missing
}

/// Why a merge's branch can't take part: `branch_not_ready: …` (T10 minor).
fn not_ready(branch: Option<&BranchRecord>, id: Uuid) -> Option<String> {
    match branch {
        None => Some(format!("branch_not_ready: branch {id} no longer exists")),
        Some(b) if b.status != BranchStatus::Ready => Some(format!(
            "branch_not_ready: branch {} is {}",
            b.name,
            b.status.as_str()
        )),
        Some(b) if b.endpoint_id.is_none() => Some(format!(
            "branch_not_ready: branch {} has no compute",
            b.name
        )),
        Some(_) => None,
    }
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// Runs delivery `attempt` of a merge job. `Err` means the job is redelivered: metadata
    /// trouble, or a transient failure with retries left.
    pub async fn handle_merge_job(&self, job: MergeJob, attempt: Attempt) -> Result<(), String> {
        match job {
            MergeJob::Compute { merge_id } => self.merge_compute(merge_id, attempt).await,
            MergeJob::Rehearse { merge_id } => self.merge_rehearse(merge_id).await,
            // A redelivery may find the first delivery's work half done (recovery).
            MergeJob::Apply { merge_id } => self.merge_apply(merge_id, attempt.0 > 1).await,
            MergeJob::Resume { merge_id } => self.merge_resume(merge_id, attempt).await,
            MergeJob::Cancel { merge_id } => self.merge_cancel(merge_id).await,
            MergeJob::Finish { merge_id, after } => {
                self.merge_finish_by_hand(merge_id, after).await
            }
        }
    }

    /// Carries on from the merge's working status (R3's re-queue, the API's retry).
    fn merge_resume(&self, id: Uuid, attempt: Attempt) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(self.merge_resume_inner(id, attempt))
    }

    async fn merge_resume_inner(&self, id: Uuid, attempt: Attempt) -> Result<(), String> {
        let Some(merge) = self
            .metadata
            .merge(id)
            .await
            .map_err(|e| format!("reading merge {id}: {e}"))?
        else {
            return Ok(());
        };
        match merge.status {
            MergeStatus::Computing => self.merge_compute(id, attempt).await,
            MergeStatus::Rehearsing => self.merge_rehearse(id).await,
            MergeStatus::Applying => self.merge_apply(id, true).await,
            MergeStatus::Online => self.merge_online(id).await,
            MergeStatus::Finishing => self.merge_finish(id).await,
            other => {
                info!(merge_id = %id, status = other.as_str(), "nothing to resume");
                Ok(())
            }
        }
    }

    /// The merge's target branch once both branches are ready, or (inner `Err`) why one can't
    /// take part (`branch_not_ready`). `Err` is a metadata failure.
    pub(crate) async fn merge_target(
        &self,
        merge: &MergeRecord,
    ) -> Result<Result<BranchRecord, String>, String> {
        let read = |id: Uuid| async move {
            self.metadata
                .get_branch(BranchId::from(id))
                .await
                .map_err(|e| format!("reading branch {id}: {e}"))
        };
        let source = read(merge.source_branch_id).await?;
        let target = read(merge.target_branch_id).await?;
        if let Some(why) = not_ready(source.as_ref(), merge.source_branch_id)
            .or_else(|| not_ready(target.as_ref(), merge.target_branch_id))
        {
            return Ok(Err(why));
        }
        Ok(Ok(target.expect("checked")))
    }

    /// Logs a status change the worker made (best effort; Studio's log shows `detail.to`).
    pub(crate) async fn log_status(&self, id: Uuid, to: MergeStatus) {
        let mut event = MergeEvent::new(id, "status");
        event.detail = Some(serde_json::json!({ "to": to.as_str() }));
        if let Err(e) = self.metadata.append_merge_event(&event).await {
            warn!(merge_id = %id, "logging the status: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use chelabase_metadata::{MergeDirection, MergeStatus};
    use serde_json::json;

    use super::*;
    use crate::fakes::{ready_database, worker};

    // E29a (for F3): a breaking step (a role rename) needs an acknowledgement in the worker's
    // own check too (`Step::needs_acknowledgement`).
    #[tokio::test]
    async fn a_breaking_step_needs_an_acknowledgement() {
        let (w, _journal) = worker();
        let db = ready_database(&w).await;
        let main = w.metadata.main(db).id;
        let id = w.metadata.insert_merge(db, main, main, MergeDirection::IntoParent, MergeStatus::Applying);
        let mut merge = w.metadata.merge_row(id);
        let step: Step = serde_json::from_value(json!({
            "id": "role:rename:app", "kind": "generated", "phase": "roles", "sql": "-- rename",
            "items": ["role:name:web"], "data_loss": false, "rewrite": false, "security": false,
            "locks": [], "role_op": null, "online": null,
            "breaking": { "role_rename": { "role": "app", "new_name": "web" } }
        }))
        .unwrap();
        let plan = StoredPlan {
            steps: vec![step],
            target_digest: String::new(),
            expected: None,
            own_sql: Vec::new(),
            precheck_fingerprint: None,
            follows_rename: Vec::new(),
            expected_rehearsed: false,
        };
        assert_eq!(unacknowledged(&merge, &plan), ["role:name:web"]);
        merge.acknowledgements = json!(["role:name:web"]);
        assert!(unacknowledged(&merge, &plan).is_empty());
    }

    // For F3: the planner's lock timeout reading is the one `plan::lock_timeout_ms` does.
    #[test]
    fn plan_options_reads_the_lock_timeout_as_the_planner_does() {
        for options in [json!({}), json!({ "lock_timeout_ms": 2000 }), json!({ "lock_timeout_ms": -1 })] {
            assert_eq!(
                lock_timeout(&options),
                chelabase_schema::plan::lock_timeout_ms(&options),
                "{options}"
            );
        }
    }

    // T14 M10: reconcile waits three claim refreshes before it calls a merge silent.
    #[test]
    fn a_merge_is_silent_after_three_claim_refreshes() {
        let refresh = crate::config::DEFAULT_CLAIM_REFRESH;
        assert!(STALE_MERGE_CLAIM >= refresh * 3, "{STALE_MERGE_CLAIM:?} vs {refresh:?}");
        assert!(REHAND_AFTER > STALE_MERGE_CLAIM);
    }
}
