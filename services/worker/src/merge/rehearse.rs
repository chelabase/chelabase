//! The rehearse and cancel jobs (T8b): the plan on a throwaway `rehearsal` branch of the
//! target's head (real data, so a failing constraint shows; P14), compared with the model the
//! target should have afterwards (D8).
//!
//! - The rehearsal branch is built like any branch (`branch_create_attempt`), but its endpoint is
//!   unroutable (P4) and it gets no base and no app users machinery.
//! - Phase 0 makes roles by SQL only, NOLOGIN, without a password or metadata (P5), so the
//!   expected model is read with `login = false` on the roles it creates.
//! - The rehearsal branch is always torn down, whatever happened; a teardown that fails is left
//!   to reconcile (R3). A cancel (the merge leaves `rehearsing`) stops the run at the next
//!   claim refresh, tears down, and writes nothing.
//! - Passed: `rehearsed`, with each step's timing and the drift (own SQL's differences marked
//!   `produced`). Failed: back to `draft` with the failing step and Postgres's error. The
//!   target moved since the plan was made (phase 1's guard): `stale`.

use chelabase_metadata::{
    BranchKind, BranchRecord, BranchStatus, MergeRecord, MergeStatus, MergeStatusPatch,
    MetadataError,
};
use chelabase_neon::ComputeBackend;
use chelabase_schema::SchemaModel;
use chelabase_schema::analysis::ObjKey;
use chelabase_schema::compare::{Drift, compare, with_rehearsed};
use chelabase_schema::plan::{Phase, RoleOp};
use chelabase_types::{BranchId, DatabaseId};
use serde_json::{Value, json};
use tracing::{info, warn};
use uuid::Uuid;

use super::phases::{FailureKind, PHASE1_STEP, PhaseFailure, PhaseReport, RunMode, Target};
use super::{StoredPlan, open_conflicts, plan_options, stored_plan};
use futures_util::future::BoxFuture;

use crate::Worker;
use crate::branch_job::Attempt;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};

/// A merge's rehearsal branch: `_rehearsal-<first 8 hex of the merge id>` (P14; a leading `_`
/// is invalid in user branch names).
pub(crate) fn rehearsal_name(merge_id: Uuid) -> String {
    format!("_rehearsal-{}", &merge_id.simple().to_string()[..8])
}

/// The expected model as a rehearsal reads it back: a role phase 0 creates is NOLOGIN there
/// (P5), and a role update never turns LOGIN on or off (no password).
fn rehearsal_expectation(
    expected: &SchemaModel,
    plan: &StoredPlan,
    actual: &SchemaModel,
) -> SchemaModel {
    let mut e = expected.clone();
    for op in plan.steps.iter().filter_map(|s| s.role_op.as_ref()) {
        let (name, created) = match op {
            RoleOp::Create(m) => (&m.name, true),
            RoleOp::Update { name, .. } => (name, false),
            RoleOp::Rename { .. } => continue,
        };
        let Some(role) = e.roles.iter_mut().find(|r| &r.name == name) else {
            continue;
        };
        role.login = match actual.roles.iter().find(|a| &a.name == name) {
            _ if created => false,
            Some(a) => a.login,
            None => role.login,
        };
    }
    e
}

/// The `rehearsal` column (Studio's `readRehearsal`): `steps` (each `{id, phase, outcome,
/// duration_ms}`; a failing step last, `outcome: "failed"`, no duration), `failed_step`,
/// `error` and `drift` (`chelabase_schema::compare::Drift`, own SQL's marked `produced`).
fn rehearsal_doc(
    report: &PhaseReport,
    failed: Option<(&str, Phase)>,
    error: Option<&str>,
    drift: &[Drift],
) -> Value {
    let mut steps: Vec<Value> = report
        .steps
        .iter()
        .map(|s| serde_json::to_value(s).unwrap_or(Value::Null))
        .collect();
    if let Some((id, phase)) = failed {
        steps.push(json!({ "id": id, "phase": phase, "outcome": "failed", "duration_ms": null }));
    }
    json!({
        "steps": steps,
        "failed_step": failed.map(|(id, _)| id),
        "error": error,
        "drift": serde_json::to_value(drift).unwrap_or_else(|_| json!([])),
    })
}

/// How the plan went on the rehearsal branch.
enum Rehearsed {
    Passed {
        report: PhaseReport,
        drift: Vec<Drift>,
        /// E31: the plan with the rehearsal's version of each object following a rename as the
        /// expected one; `None` when the plan has none.
        rehearsed_plan: Option<Value>,
    },
    Failed(PhaseFailure),
    /// Reading the result back failed.
    Unread(PhaseReport, String),
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// The rehearse job: `rehearsing` → `rehearsed`, `draft` or `stale`. `Err` only for
    /// metadata trouble (the job is redelivered and runs again from the start).
    pub(crate) fn merge_rehearse(&self, id: Uuid) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(self.merge_rehearse_inner(id))
    }

    async fn merge_rehearse_inner(&self, id: Uuid) -> Result<(), String> {
        let Some(merge) = self.claim_merge(id, MergeStatus::Rehearsing).await? else {
            info!(merge_id = %id, "the merge isn't rehearsing; nothing to rehearse");
            return Ok(());
        };
        let back = |error: String| MergeStatusPatch {
            error: Some(Some(error)),
            ..Default::default()
        };
        // The API checked these; a recompute or a branch operation may have raced it.
        let open = open_conflicts(&merge);
        if !open.is_empty() {
            let why = format!(
                "merge_conflicts_open: {} conflict(s) have no resolution: {}",
                open.len(),
                open.join(", ")
            );
            return self.end_rehearsal(id, MergeStatus::Draft, back(why)).await;
        }
        let plan = match stored_plan(&merge) {
            Ok(plan) => plan,
            Err(why) => return self.end_rehearsal(id, MergeStatus::Draft, back(why)).await,
        };
        let target = match self.merge_target(&merge).await? {
            Ok(target) => target,
            Err(why) => return self.end_rehearsal(id, MergeStatus::Draft, back(why)).await,
        };
        // A crashed run's rehearsal goes first: the new one takes its name.
        if let Err(e) = self.remove_rehearsals(&merge).await {
            let why = format!("a previous rehearsal couldn't be removed: {e}");
            return self.end_rehearsal(id, MergeStatus::Draft, back(why)).await;
        }
        // T14 M5: a cancel during the build stops it at the next claim refresh; what was built
        // goes.
        let made = self
            .run_while_claimed(
                id,
                MergeStatus::Rehearsing,
                self.make_rehearsal(&merge, &target),
            )
            .await;
        let Some(made) = made else {
            info!(merge_id = %id, "the merge was cancelled while its rehearsal was built");
            if let Err(e) = self.remove_rehearsals(&merge).await {
                warn!(merge_id = %id, "removing the cancelled rehearsal; reconcile retries: {e}");
            }
            return Ok(());
        };
        let rehearsal = match made {
            Ok(Ok(rehearsal)) => rehearsal,
            Ok(Err(why)) => {
                return self
                    .end_rehearsal(
                        id,
                        MergeStatus::Draft,
                        back(format!("creating the rehearsal: {why}")),
                    )
                    .await;
            }
            Err(e) => return Err(e),
        };
        let run = self
            .run_while_claimed(
                id,
                MergeStatus::Rehearsing,
                self.rehearse_on(&merge, &rehearsal, &plan),
            )
            .await;
        // Always, whatever happened (and before the result, so a cancel landing meanwhile
        // finds nothing left).
        if let Err(e) = self.teardown_rehearsal(rehearsal.id).await {
            warn!(merge_id = %id, branch_id = %rehearsal.id, "tearing down the rehearsal; reconcile retries: {e}");
        }
        let Some(run) = run else {
            info!(merge_id = %id, "the merge was cancelled during its rehearsal");
            return Ok(());
        };
        let (to, patch) = match run {
            Rehearsed::Passed {
                report,
                drift,
                rehearsed_plan,
            } => (
                MergeStatus::Rehearsed,
                MergeStatusPatch {
                    rehearsal: Some(rehearsal_doc(&report, None, None, &drift)),
                    error: Some(None),
                    plan: rehearsed_plan,
                    ..Default::default()
                },
            ),
            Rehearsed::Unread(report, error) => (
                MergeStatus::Draft,
                MergeStatusPatch {
                    rehearsal: Some(rehearsal_doc(&report, None, Some(&error), &[])),
                    error: Some(Some(format!("the rehearsal: {error}"))),
                    ..Default::default()
                },
            ),
            Rehearsed::Failed(f) => {
                let step = match &f.step {
                    Some(step) => step.0.clone(),
                    None if f.phase == Phase::Tx => PHASE1_STEP.to_string(),
                    None => super::phases::phase_name(f.phase).to_string(),
                };
                let doc = rehearsal_doc(&f.report, Some((&step, f.phase)), Some(&f.error), &[]);
                if f.kind == FailureKind::Stale {
                    (
                        MergeStatus::Stale,
                        MergeStatusPatch {
                            rehearsal: Some(doc),
                            error: Some(Some(
                                "merge_stale: the target's schema changed since the merge was \
                                 computed; recompute it"
                                    .to_string(),
                            )),
                            ..Default::default()
                        },
                    )
                } else {
                    (
                        MergeStatus::Draft,
                        MergeStatusPatch {
                            rehearsal: Some(doc),
                            error: Some(Some(format!(
                                "the rehearsal failed at step {step}: {}",
                                f.error
                            ))),
                            ..Default::default()
                        },
                    )
                }
            }
        };
        self.end_rehearsal(id, to, patch).await
    }

    /// Moves a rehearsing merge to `to` with `patch`; a merge that moved on meanwhile (a
    /// cancel) is left as it is.
    async fn end_rehearsal(
        &self,
        id: Uuid,
        to: MergeStatus,
        patch: MergeStatusPatch,
    ) -> Result<(), String> {
        match self
            .metadata
            .set_merge_status(id, &[MergeStatus::Rehearsing], to, patch)
            .await
        {
            Ok(true) => {
                info!(merge_id = %id, status = to.as_str(), "rehearsal ended");
                Ok(())
            }
            Ok(false) => {
                info!(merge_id = %id, "the merge moved on during its rehearsal; its result is dropped");
                Ok(())
            }
            Err(e) => Err(format!("storing merge {id}'s rehearsal: {e}")),
        }
    }

    /// The rehearsal branch of `merge`, built from `target`'s head, ready; inner `Err` when it
    /// couldn't be (anything built is torn down). `Err` is a metadata failure.
    async fn make_rehearsal(
        &self,
        merge: &MergeRecord,
        target: &BranchRecord,
    ) -> Result<Result<BranchRecord, String>, String> {
        let name = rehearsal_name(merge.id);
        let created = self
            .metadata
            .create_rehearsal_branch(DatabaseId::from(merge.database_id), target.id, &name)
            .await;
        let pending = match created {
            Ok(b) => b,
            Err(MetadataError::Conflict) => {
                return Ok(Err(format!(
                    "branch_not_ready: branch {} isn't ready",
                    target.name
                )));
            }
            Err(e) => return Ok(Err(e.to_string())),
        };
        info!(merge_id = %merge.id, branch_id = %pending.id, %name, "creating the rehearsal");
        // At the target's head; a failure is recorded on the rehearsal branch, torn down.
        self.branch_create_attempt(pending.id, None, Attempt::LAST)
            .await?;
        let built = self
            .metadata
            .get_branch(pending.id)
            .await
            .map_err(|e| format!("reading the rehearsal: {e}"))?;
        match built {
            Some(b) if b.status == BranchStatus::Ready && b.endpoint_id.is_some() => Ok(Ok(b)),
            other => {
                let why = other
                    .and_then(|b| b.error)
                    .unwrap_or_else(|| "it isn't ready".to_string());
                if let Err(e) = self.teardown_rehearsal(pending.id).await {
                    warn!(merge_id = %merge.id, "tearing down the failed rehearsal; reconcile retries: {e}");
                }
                Ok(Err(why))
            }
        }
    }

    /// The phases on the rehearsal, then its schema read back and compared.
    async fn rehearse_on(
        &self,
        merge: &MergeRecord,
        rehearsal: &BranchRecord,
        plan: &StoredPlan,
    ) -> Rehearsed {
        let endpoint = rehearsal.endpoint_id.clone().unwrap_or_default();
        let target = Target {
            branch: rehearsal.id,
            endpoint: endpoint.clone(),
            database: merge.database_name.clone(),
        };
        let report = match self
            .run_phases(
                &target,
                &plan.plan(),
                &plan_options(merge),
                &plan.target_digest,
                RunMode::Rehearsal,
            )
            .await
        {
            Ok(report) => report,
            Err(failure) => return Rehearsed::Failed(failure),
        };
        let actual = match self.read_schema(&endpoint, &merge.database_name).await {
            Ok(actual) => actual,
            Err(e) => return Rehearsed::Unread(report, e),
        };
        let Some(expected) = &plan.expected else {
            return Rehearsed::Unread(report, "the plan has no expected model".to_string());
        };
        // E31: an object whose text follows a rename is compared with what the rehearsal made
        // of it (shown as produced), and that version becomes the apply's expectation.
        let follows: Vec<ObjKey> = plan.follows_rename.iter().map(|f| f.key.clone()).collect();
        let seen = with_rehearsed(
            &rehearsal_expectation(expected, plan, &actual),
            &plan.follows_rename,
            &actual,
        );
        let mut produced = plan.own_sql.clone();
        produced.extend(follows.iter().cloned());
        let drift = compare(&seen, &actual, &produced);
        let rehearsed_plan = if plan.follows_rename.is_empty() {
            None
        } else {
            let stored = StoredPlan {
                expected: Some(with_rehearsed(expected, &plan.follows_rename, &actual)),
                expected_rehearsed: true,
                ..plan.clone()
            };
            match serde_json::to_value(&stored) {
                Ok(v) => Some(v),
                Err(e) => return Rehearsed::Unread(report, format!("storing the plan: {e}")),
            }
        };
        Rehearsed::Passed {
            report,
            drift,
            rehearsed_plan,
        }
    }

    /// Tears down a rehearsal branch (its compute, timelines and endpoint row) and marks it
    /// deleted. Only the rehearsal's own parts: the target may have been reset or deleted
    /// meanwhile (T10 frees it at a cancel).
    pub(crate) async fn teardown_rehearsal(&self, id: BranchId) -> Result<(), String> {
        let Some(b) = self
            .metadata
            .get_branch(id)
            .await
            .map_err(|e| format!("reading the rehearsal: {e}"))?
        else {
            return Ok(());
        };
        if b.status == BranchStatus::Deleted {
            return Ok(());
        }
        let tenant = self.tenant_of(b.database_id).await?;
        self.teardown_branch(&b, tenant.as_ref()).await?;
        self.metadata
            .mark_branch_deleted(b.id)
            .await
            .map_err(|e| format!("marking the rehearsal deleted: {e}"))?;
        info!(branch_id = %b.id, name = %b.name, "rehearsal torn down");
        Ok(())
    }

    /// Tears down every live rehearsal branch of `merge` (a crashed run's).
    pub(crate) async fn remove_rehearsals(&self, merge: &MergeRecord) -> Result<(), String> {
        let name = rehearsal_name(merge.id);
        let branches = self
            .metadata
            .list_branches(DatabaseId::from(merge.database_id))
            .await
            .map_err(|e| format!("listing the branches: {e}"))?;
        for b in branches
            .iter()
            .filter(|b| b.kind == BranchKind::Rehearsal && b.name == name)
        {
            self.teardown_rehearsal(b.id).await?;
        }
        Ok(())
    }

    /// The cancel job: the API already made the merge `cancelled`; a rehearsal a crashed or
    /// stopped run left is torn down (a running rehearsal stops by itself at its next claim
    /// refresh). A teardown that fails is left to reconcile (R3).
    pub(crate) async fn merge_cancel(&self, id: Uuid) -> Result<(), String> {
        let Some(merge) = self
            .metadata
            .merge(id)
            .await
            .map_err(|e| format!("reading merge {id}: {e}"))?
        else {
            return Ok(());
        };
        if merge.status == MergeStatus::Rehearsing {
            return Ok(());
        }
        if let Err(e) = self.remove_rehearsals(&merge).await {
            warn!(merge_id = %id, "removing the cancelled merge's rehearsal; reconcile retries: {e}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chelabase_metadata::{BranchStatus, MergeStatus};
    use chelabase_nats::MergeJob;
    use serde_json::{Value, json};
    use uuid::Uuid;

    use crate::branch_job::Attempt;
    use crate::fakes::{FakeWorker, entries, position, ready_branch, worker};
    use crate::merge::testkit::{
        Scenario, Setup, answer_catalog, answer_elsewhere, computed, move_to, orders,
        other_endpoints, rehearsals, setup, step_ids, table,
    };
    use crate::ports::WorkerMetadata;

    async fn rehearse(w: &FakeWorker, id: Uuid) {
        w.handle_merge_job(MergeJob::Rehearse { merge_id: id }, Attempt(1))
            .await
            .unwrap();
    }

    /// The [`Scenario`] computed and moved to `rehearsing`; a rehearsal reads the merged
    /// schema back.
    async fn rehearsing(w: &FakeWorker) -> (Setup, Uuid) {
        let s = setup(w).await;
        let id = computed(w, &s).await;
        answer_elsewhere(w, &Scenario::merged_tables(), &Scenario::rehearsed_roles());
        move_to(w, id, MergeStatus::Rehearsing);
        (s, id)
    }

    fn pos(all: &[String], prefix: &str) -> usize {
        all.iter()
            .position(|e| e.starts_with(prefix))
            .unwrap_or_else(|| panic!("{prefix:?} not in {all:?}"))
    }

    /// Every SQL run on `ep`.
    fn sql_on(w: &FakeWorker, ep: &str) -> Vec<String> {
        w.computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _, _)| e == ep)
            .map(|(_, _, q)| q.clone())
            .collect()
    }

    fn outcomes(rehearsal: &Value) -> Vec<(String, String)> {
        rehearsal["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| {
                (
                    s["id"].as_str().unwrap().to_string(),
                    s["outcome"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn rehearse_creates_unroutable_rehearsal_and_tears_it_down() {
        let (w, journal) = worker();
        let (s, id) = rehearsing(&w).await;
        journal.lock().unwrap().clear();

        rehearse(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(
            m.status,
            MergeStatus::Rehearsed,
            "{:?} {:?}",
            m.error,
            m.rehearsal
        );
        assert_eq!(m.error, None);
        assert_eq!(m.claimed_at, None);
        let r = m.rehearsal.expect("a rehearsal");
        assert_eq!(r["failed_step"], Value::Null);
        assert_eq!(r["error"], Value::Null);
        assert_eq!(r["drift"], json!([]), "{r}");
        // Timings per step: phase 0, the transaction, phase 2.
        let ran = outcomes(&r);
        let roles = step_ids(&w, id, "roles");
        let online = step_ids(&w, id, "online");
        assert_eq!(roles.len(), 1, "{ran:?}");
        assert_eq!(online.len(), 1, "{ran:?}");
        assert_eq!(
            ran,
            [
                (roles[0].clone(), "ok".to_string()),
                ("phase1".to_string(), "ok".to_string()),
                (online[0].clone(), "ok".to_string())
            ]
        );
        assert!(r["steps"][0]["duration_ms"].is_u64(), "{r}");

        // A rehearsal branch of main, named for the merge, gone again.
        let made = rehearsals(&w, s.db);
        assert_eq!(made.len(), 1);
        let rb = &made[0];
        assert_eq!(
            rb.name,
            format!("_rehearsal-{}", &id.simple().to_string()[..8])
        );
        assert_eq!(rb.parent_branch_id, Some(s.main.id));
        assert_eq!(rb.status, BranchStatus::Deleted);
        assert_eq!(rb.endpoint_id, None, "its endpoint row is gone");
        let ep = other_endpoints(&w, &s).remove(0);
        let timeline = rb.timeline_id.clone().expect("it had a timeline");
        assert!(!w.metadata.state().endpoints.contains_key(&ep));

        let all = entries(&journal);
        let order = [
            pos(&all, "metadata.create_rehearsal_branch _rehearsal-"),
            pos(&all, &format!("compute.create {ep}")),
            pos(&all, &format!("compute.delete {ep}")),
            pos(&all, &format!("storage.delete_timeline {timeline}")),
            pos(&all, "metadata.mark_branch_deleted"),
            pos(&all, &format!("metadata.set_merge_status {id} rehearsed")),
        ];
        assert!(order.is_sorted(), "{all:?}");
        // The plan ran on the rehearsal, never on main; a rehearsal logs nothing.
        let on_rehearsal = sql_on(&w, &ep);
        assert!(on_rehearsal.iter().any(|q| q.contains("chelabase_guard")));
        assert!(
            on_rehearsal
                .iter()
                .any(|q| q.contains("CREATE INDEX CONCURRENTLY"))
        );
        assert!(
            !sql_on(&w, s.main_ep())
                .iter()
                .any(|q| q.contains("chelabase_guard") || q.contains("CONCURRENTLY"))
        );
        assert!(
            !all.iter()
                .any(|e| e.starts_with("metadata.append_merge_event")),
            "{all:?}"
        );
    }

    // P5: phase 0 in a rehearsal creates roles by SQL, NOLOGIN, without a password or a row.
    #[tokio::test]
    async fn rehearsal_phase0_uses_sql_without_password() {
        let (w, journal) = worker();
        let (s, id) = rehearsing(&w).await;
        journal.lock().unwrap().clear();

        rehearse(&w, id).await;

        let ep = other_endpoints(&w, &s).remove(0);
        let create = sql_on(&w, &ep)
            .into_iter()
            .find(|q| q.contains("CREATE ROLE \"reporter\""))
            .expect("the role is created on the rehearsal");
        assert!(create.contains("NOLOGIN"), "{create}");
        assert!(!create.contains("PASSWORD"), "{create}");
        assert!(position(&journal, "metadata.insert_api_role").is_none());
    }

    // The rehearsal reads `login = false` on the roles phase 0 created (P5): no drift for it;
    // a table the rehearsal lacks is drift, recorded with the rehearsal.
    #[tokio::test]
    async fn rehearsal_drift_is_recorded() {
        let (w, _journal) = worker();
        let (_s, id) = rehearsing(&w).await;
        let without_items = vec![Scenario::merged_tables()[0].clone()];
        answer_elsewhere(&w, &without_items, &Scenario::rehearsed_roles());

        rehearse(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Rehearsed, "{:?}", m.error);
        let drift = m.rehearsal.unwrap()["drift"].clone();
        let drift = drift.as_array().unwrap();
        assert_eq!(drift.len(), 1, "{drift:?}");
        assert_eq!(drift[0]["key"]["name"], json!("items"));
        assert_eq!(drift[0]["kind"], json!("missing"));
        assert_eq!(drift[0]["produced"], json!(false));
    }

    // Review Focus 4: a failing step returns the merge to draft with the step and Postgres's
    // error; the rehearsal is still torn down.
    #[tokio::test]
    async fn rehearsal_failure_returns_to_draft_with_step_and_error() {
        let (w, _journal) = worker();
        let (s, id) = rehearsing(&w).await;
        *w.computes.fail_sql_containing.lock().unwrap() = Some((
            "CREATE INDEX CONCURRENTLY".into(),
            "psql:<stdin>:2: ERROR:  could not create index \"orders_id_idx\"".into(),
        ));

        rehearse(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Draft);
        assert!(m.plan.is_some(), "the draft keeps its plan");
        let online = step_ids(&w, id, "online")[0].clone();
        let r = m.rehearsal.expect("the failed rehearsal");
        assert_eq!(r["failed_step"], json!(online));
        let error = r["error"].as_str().unwrap();
        assert!(error.contains("could not create index"), "{error}");
        let ran = outcomes(&r);
        assert_eq!(ran.last().unwrap(), &(online.clone(), "failed".to_string()));
        assert_eq!(ran[ran.len() - 2].0, "phase1");
        let merge_error = m.error.unwrap_or_default();
        assert!(merge_error.contains(&online), "{merge_error}");
        assert_eq!(rehearsals(&w, s.db)[0].status, BranchStatus::Deleted);
    }

    // The target changed since the plan was made: the rehearsal's guard says stale.
    #[tokio::test]
    async fn rehearsal_of_a_changed_target_is_stale() {
        let (w, _journal) = worker();
        let (s, id) = rehearsing(&w).await;
        *w.computes.fail_sql_containing.lock().unwrap() = Some((
            "chelabase_guard".into(),
            "psql:<stdin>:4: ERROR:  chelabase_merge_stale".into(),
        ));

        rehearse(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Stale);
        assert!(m.error.unwrap_or_default().contains("recompute"));
        assert_eq!(rehearsals(&w, s.db)[0].status, BranchStatus::Deleted);
    }

    // A conflict opened by a recompute racing `rehearseMerge` (T10): back to draft, nothing
    // made.
    #[tokio::test]
    async fn rehearse_refused_with_open_conflicts() {
        let (w, _journal) = worker();
        let s = setup(&w).await;
        answer_catalog(
            &w,
            s.main_ep(),
            &[orders(), table(16500, "t", &[(1, "id", "bigint", 20)])],
        );
        answer_catalog(
            &w,
            s.dev_ep(),
            &[orders(), table(16500, "t", &[(1, "id", "text", 25)])],
        );
        let id = w.metadata.insert_merge(
            s.db,
            s.dev.id,
            s.main.id,
            chelabase_metadata::MergeDirection::IntoParent,
            MergeStatus::Computing,
        );
        w.handle_merge_job(MergeJob::Compute { merge_id: id }, Attempt(1))
            .await
            .unwrap();
        move_to(&w, id, MergeStatus::Rehearsing);

        rehearse(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Draft);
        assert!(
            m.error
                .unwrap_or_default()
                .starts_with("merge_conflicts_open"),
            "{:?}",
            w.metadata.merge_row(id).error
        );
        assert!(rehearsals(&w, s.db).is_empty());
    }

    // T10 minor: both branches must be ready.
    #[tokio::test]
    async fn rehearsal_needs_both_branches_ready() {
        let (w, _journal) = worker();
        let (s, id) = rehearsing(&w).await;
        w.metadata
            .state()
            .branches
            .get_mut(&s.dev.id)
            .unwrap()
            .status = BranchStatus::Resetting;

        rehearse(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Draft);
        assert!(
            m.error.unwrap_or_default().starts_with("branch_not_ready"),
            "{:?}",
            w.metadata.merge_row(id).error
        );
        assert!(rehearsals(&w, s.db).is_empty());
    }

    // A cancel landing during the rehearsal wins: nothing is written, the rehearsal goes.
    #[tokio::test]
    async fn cancel_tears_down_a_running_rehearsal() {
        let (w, _journal) = worker();
        let (s, id) = rehearsing(&w).await;
        w.metadata.state().before_merge_write = Some(Box::new(move |state| {
            state.merges.get_mut(&id).unwrap().status = MergeStatus::Cancelled;
        }));

        rehearse(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Cancelled);
        assert_eq!(m.rehearsal, None);
        assert_eq!(rehearsals(&w, s.db)[0].status, BranchStatus::Deleted);
    }

    /// A rehearsal branch of merge `id` left by a crashed run: built and ready.
    async fn leftover(w: &FakeWorker, s: &Setup, id: Uuid) -> chelabase_types::BranchId {
        let name = format!("_rehearsal-{}", &id.simple().to_string()[..8]);
        let b = w
            .metadata
            .create_rehearsal_branch(s.db, s.main.id, &name)
            .await
            .unwrap();
        w.handle_branch_create(b.id, None).await.unwrap();
        assert_eq!(w.metadata.branch(b.id).status, BranchStatus::Ready);
        b.id
    }

    // The cancel job removes what a crashed rehearsal left.
    #[tokio::test]
    async fn cancel_job_removes_a_leftover_rehearsal() {
        use chelabase_neon::ComputeBackend;
        let (w, _journal) = worker();
        let (s, id) = rehearsing(&w).await;
        let old = leftover(&w, &s, id).await;
        let ep = w.metadata.branch(old).endpoint_id.unwrap();
        move_to(&w, id, MergeStatus::Cancelled);

        w.handle_merge_job(MergeJob::Cancel { merge_id: id }, Attempt(1))
            .await
            .unwrap();

        assert_eq!(w.metadata.branch(old).status, BranchStatus::Deleted);
        assert!(!w.computes.list().await.unwrap().contains(&ep));
        // Another branch is left alone.
        let other = ready_branch(&w, s.db, "other", s.main.id).await;
        w.handle_merge_job(MergeJob::Cancel { merge_id: id }, Attempt(1))
            .await
            .unwrap();
        assert_eq!(w.metadata.branch(other).status, BranchStatus::Ready);
    }

    // A redelivered rehearsal replaces the one its crashed run left.
    #[tokio::test]
    async fn a_redelivered_rehearsal_replaces_a_leftover() {
        let (w, _journal) = worker();
        let (s, id) = rehearsing(&w).await;
        let old = leftover(&w, &s, id).await;

        rehearse(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Rehearsed, "{:?}", m.error);
        let made = rehearsals(&w, s.db);
        assert_eq!(made.len(), 2);
        assert!(made.iter().all(|b| b.status == BranchStatus::Deleted));
        assert_eq!(made[0].id, old);
    }

    // E31 (wired by F3): dev renames `t.a` to `b`; main meanwhile indexed `a`. Postgres rewrites
    // the index when the rename runs, so the plan lists it as following the rename; the
    // rehearsal stores what it produced as the expected index, and the apply reads back
    // without drift.
    #[tokio::test]
    async fn an_object_following_a_rename_is_checked_against_the_rehearsal() {
        use crate::merge::testkit::{applying_from_rehearsed, catalog, with_index};
        let (w, _journal) = worker();
        let s = crate::merge::testkit::setup(&w).await;
        let t = |col: &str| table(16500, "t", &[(1, "id", "bigint", 20), (2, col, "bigint", 20)]);
        let base = chelabase_schema::catalog::parse_catalog(&catalog(&[orders(), t("a")])).unwrap();
        {
            let mut state = w.metadata.state();
            let entry = state.schema_bases.iter_mut().rev().find(|b| b.branch_id == s.dev.id).unwrap();
            entry.model = Some(serde_json::to_value(&base).unwrap());
            entry.identity_map =
                serde_json::to_value(chelabase_schema::IdentityMap::self_mapped(&base)).unwrap();
        }
        answer_catalog(&w, s.main_ep(), &[orders(), with_index(t("a"), 16700, "ix", "a")]);
        answer_catalog(&w, s.dev_ep(), &[orders(), t("b")]);
        let id = w.metadata.insert_merge(
            s.db,
            s.dev.id,
            s.main.id,
            chelabase_metadata::MergeDirection::IntoParent,
            MergeStatus::Computing,
        );
        w.handle_merge_job(MergeJob::Compute { merge_id: id }, Attempt(1)).await.unwrap();
        let plan = w.metadata.merge_row(id).plan.expect("a plan");
        assert_eq!(plan["follows_rename"][0]["key"]["name"], json!("ix"), "{plan}");
        assert_eq!(plan["expected_rehearsed"], json!(false));

        // The rehearsal produces the index on `b`.
        let renamed = [orders(), with_index(t("b"), 16800, "ix", "b")];
        answer_elsewhere(&w, &renamed, &[]);
        move_to(&w, id, MergeStatus::Rehearsing);
        rehearse(&w, id).await;
        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Rehearsed, "{:?} {:?}", m.error, m.rehearsal);
        let drift = m.rehearsal.unwrap()["drift"].clone();
        assert!(drift.as_array().unwrap().iter().all(|d| d["produced"] == json!(true)), "{drift}");
        let plan = m.plan.unwrap();
        assert_eq!(plan["expected_rehearsed"], json!(true));
        let expected: chelabase_schema::SchemaModel =
            serde_json::from_value(plan["expected"].clone()).unwrap();
        let ix = &expected.tables.iter().find(|x| x.name == "t").unwrap().indexes[0];
        assert_eq!(ix.keys, ["b"], "the rehearsal's version");

        // The apply reads main back as the rehearsal had it: no drift.
        applying_from_rehearsed(&w, &s, id, &[orders(), with_index(t("a"), 16700, "ix", "a")], &renamed);
        w.handle_merge_job(MergeJob::Apply { merge_id: id }, Attempt(1)).await.unwrap();
        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Done, "{:?} {:?}", m.error, m.result);
    }

    // T14 M5: a cancel during the rehearsal's build stops the build at its next claim refresh;
    // what was built is torn down and nothing is written.
    #[tokio::test]
    async fn a_cancel_during_the_build_stops_it() {
        let (w, _journal) = worker();
        let (s, id) = rehearsing(&w).await;
        // The build waits on a slow storage controller.
        *w.storage.create_branch_delay.lock().unwrap() = Some(std::time::Duration::from_secs(30));
        let cancel = async {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            w.metadata.state().merges.get_mut(&id).unwrap().status = MergeStatus::Cancelled;
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(rehearse(&w, id), cancel)
        })
        .await
        .expect("stopped, not waited for");
        let m = w.metadata.merge_row(id);
        assert_eq!((m.status, m.rehearsal), (MergeStatus::Cancelled, None));
        assert!(rehearsals(&w, s.db).iter().all(|b| b.status == BranchStatus::Deleted));
    }

    // A teardown that fails leaves the unroutable rehearsal for reconcile (R3); the result
    // is still recorded.
    #[tokio::test]
    async fn a_failed_teardown_is_left_for_reconcile() {
        let (w, _journal) = worker();
        let (s, id) = rehearsing(&w).await;
        *w.computes.fail_delete.lock().unwrap() = Some("docker is down".into());

        rehearse(&w, id).await;

        assert_eq!(w.metadata.merge_row(id).status, MergeStatus::Rehearsed);
        let rb = rehearsals(&w, s.db).remove(0);
        assert_ne!(rb.status, BranchStatus::Deleted);
        let ep = rb.endpoint_id.unwrap();
        assert!(!w.metadata.state().endpoints[&ep].endpoint.routable);
    }
}
