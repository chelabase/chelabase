//! The apply and resume jobs (T8b): the plan on the target, in its phases (Part 4).
//!
//! - **Apply** (`applying`, set by the API, also on Retry after a failure before phase 2,
//!   E20): the API's checks again (M8: open conflicts, the data-loss and security
//!   acknowledgements; T10 minor: both branches ready), the target checked in full against
//!   compute's (I1: digest and model fingerprint; a change: `stale`, nothing run), then phases
//!   0, 1a and 1 (phase 1 one transaction with its digest guard, the backstop, and the
//!   recovery marker). A guard mismatch: `stale`, rolled back. Another failure: `failed` with
//!   its phase. Then
//!   `online` and phase 2, step by step (a failure: `failed`, `online`, which Retry resumes,
//!   E19), then `finishing` ([`super::finish`]).
//! - **Phase 1 exactly once** (T14 I1): a phase 1 that fails other than by its guard may have
//!   committed with its answer lost (a timeout, a dropped exec): the marker decides, and a
//!   committed one goes on to phase 2. An apply that an earlier attempt started (a Retry) reads
//!   the marker first too.
//! - **Recovery** (a redelivered Apply, or a Resume in `applying`): the crashed run's statements
//!   are cancelled first, then the `chelabase_merge.applied` marker decides: phase 1 committed,
//!   so on to phase 2; or it didn't: back to `rehearsed` ([`INTERRUPTED`]), unless this attempt
//!   never started (no step since the attempt's start in the log, I2), which then runs.
//! - **Resume in `online`** (Retry, or a crash in phase 2): the merge's statements are cancelled
//!   (M5), the marker must be there (E27: a target reset or restored meanwhile fails the merge for
//!   good), then phase 2 continues after the steps the log records as done.
//! - A target gone (deleted, or being deleted) fails a merge for good (E27), never an endless
//!   redelivery.

use chelabase_metadata::{BranchStatus, MergeRecord, MergeStatus, MergeStatusPatch};
use chelabase_neon::{ComputeBackend, SqlOpts};
use chelabase_schema::fingerprint;
use chelabase_schema::fingerprint::precheck_fingerprint;
use chelabase_schema::plan::{OWNER_ROLE, Phase, Plan, admin_digest_sql};
use chelabase_schema::sql::literal;
use chelabase_types::BranchId;
use futures_util::future::BoxFuture;
use tracing::{info, warn};
use uuid::Uuid;

use super::phases::{
    FailureKind, PHASE1_STEP, PhaseFailure, PhaseReport, RunMode, Target, app_name,
    attempt_start, cancel_sql, phase_name,
};
use super::{StoredPlan, open_conflicts, plan_options, stored_plan, unacknowledged};
use crate::Worker;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::schema_base::CATALOG_READ_TIMEOUT;

/// What a merge interrupted before its transaction committed says (I2, T8b M7: phases 0 and
/// 1a may have run).
pub(crate) const INTERRUPTED: &str = "interrupted before the plan's transaction committed; roles \
                                      and enum values it made before that stay on the target";

/// A target whose plan transaction isn't there any more (E27).
const TARGET_CHANGED: &str = "merge_target_changed: the plan's transaction is no longer on the \
                              target (it was reset or restored); nothing more is applied \
                              (recompute the merge)";

/// What the apply's pre-check (I1) says when the target moved; the reason follows.
const STALE_BEFORE: &str = "merge_stale: the target's schema changed since the merge was \
                            computed; nothing was applied (recompute the merge)";

/// [`STALE_BEFORE`] when an earlier attempt of the apply had started (its roles or enum values
/// may be in).
const STALE_BEFORE_RETRY: &str = "merge_stale: the target's schema changed since the merge \
                                  was computed; the plan's transaction wasn't applied \
                                  (recompute the merge)";

/// How many times the recovery checks that the crashed run's statements are gone (each
/// [`Worker::ready_poll`] apart). If they still run after the last check, it fails closed: the
/// marker isn't read and the job is redelivered.
const BACKEND_WAITS: u32 = 30;

/// Recovery's read of the marker, in the owner's session (it may read the table; a marker it
/// forged would only harm its own merge): the merge's rows in `chelabase_merge.applied`.
fn marker_read_sql(merge_id: Uuid) -> String {
    format!(
        "SET search_path = pg_catalog, pg_temp;\nSELECT count(*) FROM chelabase_merge.applied WHERE merge_id = {}::pg_catalog.uuid;\n",
        literal(&merge_id.to_string())
    )
}

/// The merge's statements still running (on `cloud_admin`), by `application_name`.
fn backends_sql(merge_id: Uuid) -> String {
    format!(
        "SET search_path = pg_catalog, pg_temp;\nSELECT count(*) FROM pg_stat_activity WHERE application_name = {} AND pid <> pg_backend_pid();\n",
        literal(&app_name(merge_id))
    )
}

/// The merge's error for a failure: the step (phase 1's own when it has none) and Postgres's
/// text.
fn failure_error(f: &PhaseFailure) -> String {
    let step = match &f.step {
        Some(step) => step.0.clone(),
        None if f.phase == Phase::Tx => PHASE1_STEP.to_string(),
        None => phase_name(f.phase).to_string(),
    };
    format!("step {step}: {}", f.error)
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// The apply job (`recovering`: a redelivery or a Resume, which may find a crashed run's
    /// work). `Err` only for metadata or compute trouble before anything is decided (the job
    /// is redelivered, and recovers).
    pub(crate) fn merge_apply(&self, id: Uuid, recovering: bool) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(self.merge_apply_inner(id, recovering))
    }

    async fn merge_apply_inner(&self, id: Uuid, recovering: bool) -> Result<(), String> {
        let Some(merge) = self.claim_merge(id, MergeStatus::Applying).await? else {
            info!(merge_id = %id, "the merge isn't applying; nothing to apply");
            return Ok(());
        };
        if recovering {
            let target = match self.recovery_target(&merge).await? {
                Ok(target) => target,
                Err(why) => return self.fail_merge(id, MergeStatus::Applying, None, why).await,
            };
            self.stop_merge_backends(&target, id).await?;
            if self.marker_present(&target, id).await? {
                info!(merge_id = %id, "the plan's transaction committed before the crash; on to the online phase");
                return self
                    .on_to_online(&merge, &target, PhaseReport::default())
                    .await;
            }
            if self.attempt_started(id).await? {
                info!(merge_id = %id, "the apply was interrupted before its transaction committed");
                return self.back_to_rehearsed(id, INTERRUPTED.to_string()).await;
            }
            // Queued, never started: the apply runs now.
        }
        self.apply_fresh(&merge).await
    }

    /// Phase 1 committed: `applying` → `online`, then phase 2 after `report`. A plan that can't
    /// be read fails the merge in `online` (half applied: retried, never recomputed; M6).
    async fn on_to_online(
        &self,
        merge: &MergeRecord,
        target: &Target,
        report: PhaseReport,
    ) -> Result<(), String> {
        let id = merge.id;
        let plan = match stored_plan(merge) {
            Ok(plan) => plan,
            Err(why) => {
                return self
                    .fail_merge(id, MergeStatus::Applying, Some(Phase::Online), why)
                    .await;
            }
        };
        if !self
            .advance(id, MergeStatus::Applying, MergeStatus::Online)
            .await?
        {
            return Ok(());
        }
        self.continue_online(merge, target, &plan, report).await
    }

    /// The apply from the start.
    fn apply_fresh<'a>(&'a self, merge: &'a MergeRecord) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(self.apply_fresh_inner(merge))
    }

    async fn apply_fresh_inner(&self, merge: &MergeRecord) -> Result<(), String> {
        let id = merge.id;
        let open = open_conflicts(merge);
        if !open.is_empty() {
            let why = format!(
                "merge_conflicts_open: {} conflict(s) have no resolution: {}",
                open.len(),
                open.join(", ")
            );
            return self.back_to_rehearsed(id, why).await;
        }
        let plan = match stored_plan(merge) {
            Ok(plan) => plan,
            Err(why) => return self.back_to_rehearsed(id, why).await,
        };
        let missing = unacknowledged(merge, &plan);
        if !missing.is_empty() {
            let why = format!(
                "merge_drops_unacknowledged: acknowledge these data-loss, security or breaking changes first: {}",
                missing.join(", ")
            );
            return self.back_to_rehearsed(id, why).await;
        }
        let branch = match self.merge_target(merge).await? {
            Ok(branch) => branch,
            Err(why) => return self.back_to_rehearsed(id, why).await,
        };
        let target = Target {
            branch: branch.id,
            endpoint: branch.endpoint_id.clone().unwrap_or_default(),
            database: merge.database_name.clone(),
        };
        // T14 I1 (E27): an earlier attempt's transaction may have committed with its answer
        // lost: the marker first (the attempt's statements stopped), and a committed one goes
        // on to phase 2.
        let started_before = self.ever_started(id).await?;
        if started_before {
            self.stop_merge_backends(&target, id).await?;
            if self.marker_present(&target, id).await? {
                info!(merge_id = %id, "an earlier attempt's transaction committed; on to the online phase");
                return self
                    .on_to_online(merge, &target, PhaseReport::default())
                    .await;
            }
        }
        // I1 (E4, E14): the whole target as the merge was computed against, before phase 0
        // changes anything (phase 1's guard leaves out roles and enum values, which phases 0
        // and 1a change first; it stays as the backstop).
        if let Some(why) = self.target_moved(merge, &target, &plan).await? {
            info!(merge_id = %id, "the target changed since the merge was computed: {why}");
            // An earlier attempt (before a retry, or an interrupted one) may have run phases 0
            // and 1a: only the plan's transaction is sure not to have been applied.
            let said = if started_before {
                STALE_BEFORE_RETRY
            } else {
                STALE_BEFORE
            };
            let patch = MergeStatusPatch {
                error: Some(Some(format!("{said}: {why}"))),
                failed_phase: Some(None),
                ..Default::default()
            };
            return self
                .end_apply(id, MergeStatus::Applying, MergeStatus::Stale, patch)
                .await;
        }
        info!(merge_id = %id, target = %target.branch, "applying the merge");
        // Phases 0, 1a and 1; the online steps run once the merge is `online`.
        let before = Plan {
            steps: plan
                .steps
                .iter()
                .filter(|s| s.phase != Phase::Online)
                .cloned()
                .collect(),
        };
        let opts = plan_options(merge);
        let run = self
            .keep_merge_claimed(
                id,
                MergeStatus::Applying,
                self.run_phases(&target, &before, &opts, &plan.target_digest, RunMode::Real),
            )
            .await;
        let report = match run {
            Ok(report) => report,
            Err(f) if f.phase == Phase::Tx && f.kind != FailureKind::Stale => {
                // T14 I1: it may have committed and lost its answer; the marker decides.
                match self.committed_after_all(&target, id).await {
                    Ok(true) => {
                        info!(merge_id = %id, "phase 1 reported a failure but committed; on to the online phase");
                        let mut report = f.report;
                        report.steps.push(super::phases::StepRun {
                            id: PHASE1_STEP.to_string(),
                            phase: Phase::Tx,
                            outcome: super::phases::StepOutcome::Ok,
                            duration_ms: 0,
                        });
                        return self.on_to_online(merge, &target, report).await;
                    }
                    Ok(false) => return self.fail_phase(id, MergeStatus::Applying, f).await,
                    Err(why) => {
                        let error = format!(
                            "{}; whether the transaction committed couldn't be checked ({why}): \
                             Retry checks it first",
                            failure_error(&f)
                        );
                        return self
                            .fail_merge(id, MergeStatus::Applying, Some(Phase::Tx), error)
                            .await;
                    }
                }
            }
            Err(f) => return self.fail_phase(id, MergeStatus::Applying, f).await,
        };
        if !self
            .advance(id, MergeStatus::Applying, MergeStatus::Online)
            .await?
        {
            return Ok(());
        }
        self.continue_online(merge, &target, &plan, report).await
    }

    /// Whether a phase 1 that reported a failure committed after all: its statements stopped,
    /// then the marker read.
    async fn committed_after_all(&self, target: &Target, id: Uuid) -> Result<bool, String> {
        self.stop_merge_backends(target, id).await?;
        self.marker_present(target, id).await
    }

    /// A Resume in `online`: Retry after an online failure, or a crash in phase 2.
    pub(crate) fn merge_online(&self, id: Uuid) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(self.merge_online_inner(id))
    }

    async fn merge_online_inner(&self, id: Uuid) -> Result<(), String> {
        let Some(merge) = self.claim_merge(id, MergeStatus::Online).await? else {
            return Ok(());
        };
        let target = match self.recovery_target(&merge).await? {
            Ok(target) => target,
            Err(why) => return self.fail_merge(id, MergeStatus::Online, None, why).await,
        };
        // M5: a build the crashed run left running isn't raced.
        self.stop_merge_backends(&target, id).await?;
        // E27: phase 2 only on top of phase 1 (a reset or restore of the target since would
        // make it a partial apply).
        if !self.marker_present(&target, id).await? {
            warn!(merge_id = %id, "the plan's transaction is no longer on the target");
            return self
                .fail_merge(id, MergeStatus::Online, None, TARGET_CHANGED.to_string())
                .await;
        }
        let plan = match stored_plan(&merge) {
            Ok(plan) => plan,
            Err(why) => {
                return self
                    .fail_merge(id, MergeStatus::Online, Some(Phase::Online), why)
                    .await;
            }
        };
        self.continue_online(&merge, &target, &plan, PhaseReport::default())
            .await
    }

    /// Phase 2 after `report`, then `finishing` and the finish.
    fn continue_online<'a>(
        &'a self,
        merge: &'a MergeRecord,
        target: &'a Target,
        plan: &'a StoredPlan,
        report: PhaseReport,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(self.continue_online_inner(merge, target, plan, report))
    }

    async fn continue_online_inner(
        &self,
        merge: &MergeRecord,
        target: &Target,
        plan: &StoredPlan,
        report: PhaseReport,
    ) -> Result<(), String> {
        let id = merge.id;
        let run = self
            .keep_merge_claimed(
                id,
                MergeStatus::Online,
                self.run_online(
                    target,
                    &plan.plan(),
                    &plan_options(merge),
                    RunMode::Real,
                    report,
                ),
            )
            .await;
        if let Err(f) = run {
            // T14 F3 (M6): a step that failed other than by its timeout (which cancelled it) may
            // still run in the compute (a dropped exec): stopped before the merge is failed, so
            // Abandon's list can't miss a build still going. Best effort: a Retry stops it too.
            if f.kind == FailureKind::Failed
                && let Err(e) = self.stop_merge_backends(target, id).await
            {
                warn!(merge_id = %id, "stopping the merge's statements after a failure: {e}");
            }
            return self.fail_phase(id, MergeStatus::Online, f).await;
        }
        if !self
            .advance(id, MergeStatus::Online, MergeStatus::Finishing)
            .await?
        {
            return Ok(());
        }
        self.merge_finish(id).await
    }

    /// Moves the merge on and logs it; `false` when it had moved on already.
    pub(crate) async fn advance(
        &self,
        id: Uuid,
        from: MergeStatus,
        to: MergeStatus,
    ) -> Result<bool, String> {
        let moved = self
            .metadata
            .set_merge_status(id, &[from], to, MergeStatusPatch::default())
            .await
            .map_err(|e| format!("moving merge {id} to {}: {e}", to.as_str()))?;
        if moved {
            self.log_status(id, to).await;
        } else {
            info!(merge_id = %id, to = to.as_str(), "the merge moved on; stopping");
        }
        Ok(moved)
    }

    /// Why the target isn't the one the merge was computed against (I1): its digest (E4) or
    /// its model's fingerprint (roles and enum values included) moved; `None` when it's the
    /// same. Both reads on `cloud_admin` with a pinned search path. `Err` (redelivered) when
    /// they can't be read.
    async fn target_moved(
        &self,
        merge: &MergeRecord,
        target: &Target,
        plan: &StoredPlan,
    ) -> Result<Option<String>, String> {
        let opts = SqlOpts {
            timeout: CATALOG_READ_TIMEOUT,
            ..SqlOpts::admin_default()
        };
        let digest = self
            .computes
            .admin_sql_with(
                &target.endpoint,
                &target.database,
                &admin_digest_sql(),
                &opts,
            )
            .await
            .map_err(|e| format!("reading the target's digest: {e:#}"))?;
        if digest.trim() != plan.target_digest {
            return Ok(Some("its catalog digest differs".to_string()));
        }
        let model = self.read_schema(&target.endpoint, &target.database).await?;
        // What the plan's phases 0 and 1a did on an earlier attempt isn't a change (fix round 2).
        let (stored, live) = match &plan.precheck_fingerprint {
            Some(stored) => (
                Some(stored.as_str()),
                precheck_fingerprint(&model, &plan.steps),
            ),
            None => (merge.target_fingerprint.as_deref(), fingerprint(&model)),
        };
        if stored != Some(live.as_str()) {
            return Ok(Some(
                "its schema (roles and enum values included) differs".to_string(),
            ));
        }
        Ok(None)
    }

    /// A failed phase: `stale` (phase 1's guard rolled back) or `failed` in its phase.
    async fn fail_phase(&self, id: Uuid, from: MergeStatus, f: PhaseFailure) -> Result<(), String> {
        if f.kind == FailureKind::Stale {
            let patch = MergeStatusPatch {
                error: Some(Some(
                    "merge_stale: the target's schema changed since the merge was computed; the \
                     plan's transaction rolled back (recompute the merge)"
                        .to_string(),
                )),
                failed_phase: Some(None),
                ..Default::default()
            };
            return self.end_apply(id, from, MergeStatus::Stale, patch).await;
        }
        self.fail_merge(id, from, Some(f.phase), failure_error(&f))
            .await
    }

    async fn fail_merge(
        &self,
        id: Uuid,
        from: MergeStatus,
        phase: Option<Phase>,
        error: String,
    ) -> Result<(), String> {
        warn!(merge_id = %id, phase = phase.map(phase_name), "the merge failed: {error}");
        let patch = MergeStatusPatch {
            error: Some(Some(error)),
            failed_phase: Some(phase.map(|p| phase_name(p).to_string())),
            ..Default::default()
        };
        self.end_apply(id, from, MergeStatus::Failed, patch).await
    }

    /// Back to `rehearsed` with `error`: nothing of the plan was applied.
    async fn back_to_rehearsed(&self, id: Uuid, error: String) -> Result<(), String> {
        info!(merge_id = %id, "back to rehearsed: {error}");
        let patch = MergeStatusPatch {
            error: Some(Some(error)),
            ..Default::default()
        };
        self.end_apply(id, MergeStatus::Applying, MergeStatus::Rehearsed, patch)
            .await
    }

    async fn end_apply(
        &self,
        id: Uuid,
        from: MergeStatus,
        to: MergeStatus,
        patch: MergeStatusPatch,
    ) -> Result<(), String> {
        let moved = self
            .metadata
            .set_merge_status(id, &[from], to, patch)
            .await
            .map_err(|e| format!("moving merge {id} to {}: {e}", to.as_str()))?;
        if moved {
            self.log_status(id, to).await;
        }
        Ok(())
    }

    /// Where a recovery runs: the target's endpoint, whatever the branch's status (its merge
    /// holds it; T10 refuses branch operations meanwhile). Inner `Err`: the target is gone
    /// (missing, deleting or deleted), and the merge fails for good (E27). `Err` (redelivered)
    /// while it has no compute.
    async fn recovery_target(&self, merge: &MergeRecord) -> Result<Result<Target, String>, String> {
        let target = self
            .metadata
            .get_branch(BranchId::from(merge.target_branch_id))
            .await
            .map_err(|e| format!("reading the target: {e}"))?;
        let target = match target {
            Some(t) if !matches!(t.status, BranchStatus::Deleting | BranchStatus::Deleted) => t,
            gone => {
                let what = gone.map_or_else(
                    || "no longer exists".to_string(),
                    |t| format!("is {}", t.status.as_str()),
                );
                return Ok(Err(format!(
                    "merge_target_gone: the merge's target {what}; nothing more is applied"
                )));
            }
        };
        let endpoint = target
            .endpoint_id
            .clone()
            .ok_or_else(|| format!("the target {} has no compute", target.name))?;
        Ok(Ok(Target {
            branch: target.id,
            endpoint,
            database: merge.database_name.clone(),
        }))
    }

    /// Cancels the merge's statements (a crashed run's psql keeps running in the compute) and
    /// waits, a bounded while, until they're gone. Fails closed (M1): `Err` (the job is
    /// redelivered) when the cancel or the check can't run, or the statements still run, so no
    /// one decides about the marker while a phase 1 may still be committing.
    async fn stop_merge_backends(&self, target: &Target, id: Uuid) -> Result<(), String> {
        self.computes
            .admin_sql(&target.endpoint, &target.database, &cancel_sql(id))
            .await
            .map_err(|e| format!("cancelling the merge's statements: {e:#}"))?;
        for _ in 0..BACKEND_WAITS {
            let out = self
                .computes
                .admin_sql(&target.endpoint, &target.database, &backends_sql(id))
                .await
                .map_err(|e| format!("checking the merge's statements: {e:#}"))?;
            match out.trim().parse::<u64>() {
                Ok(0) => return Ok(()),
                Ok(_) => tokio::time::sleep(self.ready_poll).await,
                Err(_) => {
                    return Err(format!(
                        "checking the merge's statements: unexpected answer {:?}",
                        out.trim()
                    ));
                }
            }
        }
        Err("the merge's statements are still running; trying again later".to_string())
    }

    /// Whether phase 1 committed: the marker row is there. Only a missing `chelabase_merge`
    /// schema or `applied` table is no marker (M2); any other failure, or an answer that isn't
    /// a count, is `Err` (the job is redelivered).
    async fn marker_present(&self, target: &Target, id: Uuid) -> Result<bool, String> {
        let opts = SqlOpts {
            user: Some(OWNER_ROLE.to_string()),
            ..SqlOpts::admin_default()
        };
        match self
            .computes
            .admin_sql_with(
                &target.endpoint,
                &target.database,
                &marker_read_sql(id),
                &opts,
            )
            .await
        {
            Ok(out) => out.trim().parse::<u64>().map(|n| n > 0).map_err(|_| {
                format!(
                    "reading the merge's marker: unexpected answer {:?}",
                    out.trim()
                )
            }),
            Err(e) => {
                let error = format!("{e:#}");
                if error.contains("schema \"chelabase_merge\" does not exist")
                    || error.contains("relation \"chelabase_merge.applied\" does not exist")
                {
                    Ok(false)
                } else {
                    Err(format!("reading the merge's marker: {error}"))
                }
            }
        }
    }

    /// Whether this apply attempt had started: a step in the log since the attempt's start
    /// (the last move to `applying`, which the API logs; T14 I2).
    async fn attempt_started(&self, id: Uuid) -> Result<bool, String> {
        let events = self.log(id).await?;
        Ok(events[attempt_start(&events)..]
            .iter()
            .any(|e| e.kind == "step"))
    }

    /// Whether any apply attempt of the merge had started: a step anywhere in its log.
    async fn ever_started(&self, id: Uuid) -> Result<bool, String> {
        Ok(self.log(id).await?.iter().any(|e| e.kind == "step"))
    }

    /// Whether an earlier apply attempt's plan transaction is on the target (T14 F3, I2): read
    /// only for a merge with a step in its log, after its statements are stopped. A target gone
    /// (or being deleted) has nothing left to protect. `Err` (the job is redelivered) when the
    /// marker can't be read.
    pub(crate) async fn phase1_on_target(&self, merge: &MergeRecord) -> Result<bool, String> {
        if !self.ever_started(merge.id).await? {
            return Ok(false);
        }
        let Ok(target) = self.recovery_target(merge).await? else {
            return Ok(false);
        };
        self.stop_merge_backends(&target, merge.id).await?;
        self.marker_present(&target, merge.id).await
    }

    async fn log(&self, id: Uuid) -> Result<Vec<chelabase_metadata::MergeEvent>, String> {
        self.metadata
            .merge_events(id)
            .await
            .map_err(|e| format!("reading merge {id}'s log: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use chelabase_metadata::{BranchStatus, MergeEvent, MergeStatus, RoleOrigin};
    use chelabase_nats::MergeJob;
    use serde_json::json;
    use uuid::Uuid;

    use crate::branch_job::Attempt;
    use crate::fakes::{FakeWorker, Journal, entries, worker};
    use crate::merge::testkit::{
        CATALOG_READ, DIGEST_READ, Scenario, acknowledge_all, answer_elsewhere, applying,
        before_apply, move_to, orders, setup, step_ids, table,
    };

    /// Only the recovery's marker read has this.
    const MARKER_READ: &str = "SELECT count(*) FROM chelabase_merge.applied";

    async fn job(w: &FakeWorker, job: MergeJob, attempt: u64) {
        w.handle_merge_job(job, Attempt(attempt)).await.unwrap();
    }

    async fn apply(w: &FakeWorker, id: Uuid) {
        job(w, MergeJob::Apply { merge_id: id }, 1).await;
    }

    fn pos(all: &[String], prefix: &str) -> usize {
        all.iter()
            .position(|e| e.starts_with(prefix))
            .unwrap_or_else(|| panic!("{prefix:?} not in {all:?}"))
    }

    fn ran(w: &FakeWorker, ep: &str, needle: &str) -> usize {
        w.computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _, q)| e == ep && q.contains(needle))
            .count()
    }

    fn first_sql(w: &FakeWorker, ep: &str, needle: &str) -> usize {
        w.computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _, _)| e == ep)
            .position(|(_, _, q)| q.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} never ran on {ep}"))
    }

    fn logged(journal: &Journal) -> Vec<String> {
        entries(journal)
            .into_iter()
            .filter_map(|e| {
                e.strip_prefix("metadata.append_merge_event ")
                    .map(str::to_string)
            })
            .collect()
    }

    /// A step the crashed worker logged before it went away.
    fn logged_before(w: &FakeWorker, id: Uuid, phase: &str, step: &str) {
        let mut e = MergeEvent::new(id, "step");
        e.phase = Some(phase.into());
        e.step_id = Some(step.into());
        e.outcome = Some("ok".into());
        w.metadata.state().merge_events.push(e);
    }

    #[tokio::test]
    async fn apply_happy_path_journal_order() {
        let (w, journal) = worker();
        let (s, id) = applying(&w).await;
        journal.lock().unwrap().clear();

        apply(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Done, "{:?} {:?}", m.error, m.result);
        assert_eq!((m.error, m.failed_phase, m.claimed_at), (None, None, None));
        let result = m.result.expect("a result");
        assert_eq!(
            result["created_roles"],
            json!([{ "name": "reporter", "branch_id": s.main.id.to_string() }])
        );
        assert_eq!(result["drift"], json!([]));
        assert_eq!(result["after"]["choice"], json!("keep"));

        let roles = step_ids(&w, id, "roles")[0].clone();
        let online = step_ids(&w, id, "online")[0].clone();
        let all = entries(&journal);
        let read = format!(
            "compute.sql {} app BEGIN ISOLATION LEVEL REPEATABLE READ",
            s.main_ep()
        );
        let reads: Vec<usize> = all
            .iter()
            .enumerate()
            .filter(|(_, e)| e.starts_with(&read))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(reads.len(), 2, "the pre-check (I1) and the finish: {all:?}");
        let order = [
            reads[0],
            pos(&all, "metadata.insert_api_role"),
            pos(
                &all,
                &format!("metadata.append_merge_event step roles {roles} ok"),
            ),
            pos(&all, "metadata.append_merge_event step tx phase1 ok"),
            pos(&all, &format!("metadata.set_merge_status {id} online")),
            pos(
                &all,
                &format!("metadata.append_merge_event step online {online} ok"),
            ),
            pos(&all, &format!("metadata.set_merge_status {id} finishing")),
            reads[1],
            pos(&all, "metadata.append_schema_base"),
            pos(&all, &format!("metadata.set_merge_status {id} done")),
        ];
        assert!(order.is_sorted(), "{all:?}");
        // Phase 1 runs once, with its guard and marker, before the online step.
        let main = s.main_ep();
        assert_eq!(ran(&w, main, "$chelabase_guard$"), 1);
        assert_eq!(ran(&w, main, "INSERT INTO chelabase_merge.applied"), 1);
        assert!(
            first_sql(&w, main, "$chelabase_guard$")
                < first_sql(&w, main, "CREATE INDEX CONCURRENTLY")
        );
        // Status lines in the log, beside the steps.
        let statuses: Vec<String> = w
            .metadata
            .state()
            .merge_events
            .iter()
            .filter(|e| e.merge_id == id && e.kind == "status")
            .filter_map(|e| e.detail.as_ref()?["to"].as_str().map(str::to_string))
            .collect();
        assert_eq!(statuses, ["online", "finishing", "done"]);
    }

    // P9 and U2: a BYPASSRLS role is created through the control plane once acknowledged; M8:
    // the apply checks the acknowledgements again itself.
    #[tokio::test]
    async fn security_role_created_through_create_role_with_bypass_rls() {
        use crate::merge::testkit::{answer_catalog, answer_catalog_with_roles, orders, role};
        let (w, _journal) = worker();
        let s = setup(&w).await;
        answer_catalog(&w, s.main_ep(), &[orders()]);
        let auditor = role(16460, "auditor", true, true);
        answer_catalog_with_roles(&w, s.dev_ep(), &[orders()], std::slice::from_ref(&auditor));
        let id = w.metadata.insert_merge(
            s.db,
            s.dev.id,
            s.main.id,
            chelabase_metadata::MergeDirection::IntoParent,
            MergeStatus::Computing,
        );
        job(&w, MergeJob::Compute { merge_id: id }, 1).await;
        let plan = w.metadata.merge_row(id).plan.expect("a plan");
        assert!(
            plan["steps"][0]["security"].as_bool().unwrap(),
            "BYPASSRLS is flagged: {plan}"
        );
        answer_elsewhere(&w, &[orders()], &[role(16800, "auditor", false, true)]);
        move_to(&w, id, MergeStatus::Rehearsing);
        job(&w, MergeJob::Rehearse { merge_id: id }, 1).await;
        assert_eq!(w.metadata.merge_row(id).status, MergeStatus::Rehearsed);
        answer_catalog_with_roles(
            &w,
            s.main_ep(),
            &[orders()],
            &[role(16800, "auditor", true, true)],
        );

        // Not acknowledged (the API's check raced a draft save): refused, nothing ran.
        move_to(&w, id, MergeStatus::Applying);
        apply(&w, id).await;
        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Rehearsed);
        assert!(
            m.error.clone().unwrap_or_default().starts_with(
                "merge_drops_unacknowledged: acknowledge these data-loss, security or breaking changes first: "
            ),
            "{:?}",
            m.error
        );
        assert_eq!(ran(&w, s.main_ep(), "$chelabase_guard$"), 0);

        acknowledge_all(&w, id);
        move_to(&w, id, MergeStatus::Applying);
        before_apply(&w, s.main_ep(), &[orders()], &[]);
        apply(&w, id).await;
        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Done, "{:?}", m.error);
        let (copies, _) = {
            use crate::ports::WorkerMetadata;
            w.metadata.role_copies(s.main_ep()).await.unwrap()
        };
        let row = copies
            .iter()
            .find(|c| c.name == "auditor")
            .expect("its row");
        assert_eq!(row.origin, RoleOrigin::Api);
        assert!(row.attributes.bypass_rls);
    }

    // Review Focus 2: the target changed since the rehearsal; phase 1 rolls back and the merge
    // is stale, with nothing after it.
    #[tokio::test]
    async fn phase1_recheck_mismatch_rolls_back_and_marks_stale() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        *w.computes.fail_sql_containing.lock().unwrap() = Some((
            "$chelabase_guard$".into(),
            "psql:<stdin>:4: ERROR:  chelabase_merge_stale".into(),
        ));

        apply(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Stale);
        let error = m.error.unwrap_or_default();
        assert!(error.contains("rolled back"), "{error}");
        assert!(error.contains("since the merge was computed"), "{error}");
        assert_eq!(ran(&w, s.main_ep(), "CREATE INDEX CONCURRENTLY"), 0);
        assert!(
            !w.metadata
                .state()
                .schema_bases
                .iter()
                .any(|b| b.source == chelabase_metadata::BaseSource::Merge)
        );
    }

    #[tokio::test]
    async fn phase1_failure_marks_failed_tx() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        *w.computes.fail_sql_containing.lock().unwrap() = Some((
            "$chelabase_guard$".into(),
            "psql:<stdin>:3: ERROR:  out of shared memory".into(),
        ));

        apply(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Failed);
        assert_eq!(m.failed_phase.as_deref(), Some("tx"));
        assert!(m.error.unwrap_or_default().contains("out of shared memory"));
        assert_eq!(ran(&w, s.main_ep(), "CREATE INDEX CONCURRENTLY"), 0);
    }

    // E19 and Review Focus 3: an online failure fails the merge in `online`; Retry (the API
    // moves it to `online` and publishes Resume) cancels any leftover backend first (M5), then
    // continues after the steps done, and finishes.
    #[tokio::test]
    async fn online_failure_then_retry_finishes() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        *w.computes.fail_sql_containing.lock().unwrap() = Some((
            "CREATE INDEX CONCURRENTLY".into(),
            "psql:<stdin>:2: ERROR:  canceling statement due to lock timeout".into(),
        ));

        apply(&w, id).await;

        let m = w.metadata.merge_row(id);
        let online = step_ids(&w, id, "online")[0].clone();
        assert_eq!(m.status, MergeStatus::Failed);
        assert_eq!(m.failed_phase.as_deref(), Some("online"));
        let error = m.error.unwrap_or_default();
        assert!(
            error.contains(&online) && error.contains("lock timeout"),
            "{error}"
        );

        *w.computes.fail_sql_containing.lock().unwrap() = None;
        w.computes.sql.lock().unwrap().clear();
        move_to(&w, id, MergeStatus::Online);
        // Phase 1 committed: its marker is there (E27: checked first).
        w.computes.answer_at(s.main_ep(), MARKER_READ, "1\n");
        job(&w, MergeJob::Resume { merge_id: id }, 1).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Done, "{:?}", m.error);
        let main = s.main_ep();
        assert_eq!(
            ran(&w, main, "$chelabase_guard$"),
            0,
            "phase 1 isn't run again"
        );
        assert!(
            first_sql(&w, main, "pg_cancel_backend")
                < first_sql(&w, main, "CREATE INDEX CONCURRENTLY")
        );
    }

    // Recovery: the marker says phase 1 committed; the redelivery goes on to phase 2.
    #[tokio::test]
    async fn crash_after_commit_resumes_from_marker() {
        let (w, journal) = worker();
        let (s, id) = applying(&w).await;
        let roles = step_ids(&w, id, "roles")[0].clone();
        logged_before(&w, id, "roles", &roles);
        logged_before(&w, id, "tx", "phase1");
        w.computes.answer_at(s.main_ep(), MARKER_READ, "1\n");
        // A recovery doesn't pre-check (main already has phase 1's changes).
        w.computes.answers_once.lock().unwrap().clear();
        journal.lock().unwrap().clear();

        job(&w, MergeJob::Apply { merge_id: id }, 2).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Done, "{:?}", m.error);
        let main = s.main_ep();
        assert_eq!(ran(&w, main, "$chelabase_guard$"), 0);
        assert!(
            !entries(&journal)
                .iter()
                .any(|e| e.starts_with("metadata.insert_api_role"))
        );
        // The crashed worker's statements are cancelled before the marker is read.
        assert!(first_sql(&w, main, "pg_cancel_backend") < first_sql(&w, main, MARKER_READ));
        let online = step_ids(&w, id, "online")[0].clone();
        assert!(logged(&journal).contains(&format!("step online {online} ok")));
    }

    // Recovery: no marker, and the crashed worker had started: nothing of the plan's
    // transaction committed, so the merge goes back to `rehearsed`.
    #[tokio::test]
    async fn crash_before_commit_returns_to_rehearsed() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        let roles = step_ids(&w, id, "roles")[0].clone();
        logged_before(&w, id, "roles", &roles);
        w.computes.answer_at(s.main_ep(), MARKER_READ, "0\n");

        job(&w, MergeJob::Resume { merge_id: id }, 1).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Rehearsed);
        // T14 I2 / M22: phase 0 ran, so the wording doesn't claim nothing was applied.
        assert_eq!(m.error.as_deref(), Some(super::INTERRUPTED));
        assert!(super::INTERRUPTED.contains("transaction"), "{}", super::INTERRUPTED);
        assert!(!super::INTERRUPTED.contains("nothing applied"));
        assert_eq!(m.claimed_at, None);
        assert_eq!(ran(&w, s.main_ep(), "$chelabase_guard$"), 0);
        // A marker table that doesn't exist yet reads as no marker too.
        move_to(&w, id, MergeStatus::Applying);
        *w.computes.fail_sql_containing.lock().unwrap() = Some((
            MARKER_READ.into(),
            "psql:<stdin>:2: ERROR:  relation \"chelabase_merge.applied\" does not exist".into(),
        ));
        job(&w, MergeJob::Resume { merge_id: id }, 1).await;
        assert_eq!(w.metadata.merge_row(id).status, MergeStatus::Rehearsed);
    }

    // A Resume that reaches an `applying` merge whose apply never started (queued behind
    // another merge): the apply runs.
    #[tokio::test]
    async fn an_apply_that_never_started_runs_on_resume() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        w.computes.answer_at(s.main_ep(), MARKER_READ, "0\n");

        job(&w, MergeJob::Resume { merge_id: id }, 1).await;

        assert_eq!(w.metadata.merge_row(id).status, MergeStatus::Done);
        assert_eq!(ran(&w, s.main_ep(), "$chelabase_guard$"), 1);
    }

    // T10 minor: both branches must be ready; the merge stays rehearsed with the reason.
    #[tokio::test]
    async fn apply_needs_both_branches_ready() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        w.metadata
            .state()
            .branches
            .get_mut(&s.main.id)
            .unwrap()
            .status = BranchStatus::Resetting;

        apply(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Rehearsed);
        assert!(m.error.unwrap_or_default().starts_with("branch_not_ready"));
        assert_eq!(ran(&w, s.main_ep(), "$chelabase_guard$"), 0);
    }

    // A merge that moved on (an earlier delivery finished it) is left alone.
    #[tokio::test]
    async fn an_apply_of_a_finished_merge_does_nothing() {
        let (w, journal) = worker();
        let (_s, id) = applying(&w).await;
        move_to(&w, id, MergeStatus::Done);
        journal.lock().unwrap().clear();

        apply(&w, id).await;
        job(&w, MergeJob::Resume { merge_id: id }, 1).await;

        assert_eq!(w.metadata.merge_row(id).status, MergeStatus::Done);
        assert!(
            !entries(&journal)
                .iter()
                .any(|e| e.starts_with("compute.sql"))
        );
    }

    /// Nothing of the plan ran on main: no role, no enum value, no transaction, no online step.
    fn nothing_ran(w: &FakeWorker, journal: &Journal, main: &str) {
        let all = entries(journal);
        assert!(
            !all.iter()
                .any(|e| e.starts_with("metadata.insert_api_role")),
            "{all:?}"
        );
        for needle in [
            "CREATE ROLE",
            "ADD VALUE",
            "$chelabase_guard$",
            "CONCURRENTLY",
        ] {
            assert_eq!(ran(w, main, needle), 0, "{needle}");
        }
    }

    // I1 (E4, E14): the target is checked in full before phase 0, roles and enum values
    // included (the phase 1 guard's digest leaves them out): a change since compute makes the
    // merge stale with nothing run.
    #[tokio::test]
    async fn a_changed_target_is_stale_before_phase_0() {
        let (w, journal) = worker();
        let (s, id) = applying(&w).await;
        w.computes.answers_once.lock().unwrap().clear();
        let extra = table(16950, "added_on_main", &[(1, "id", "bigint", 20)]);
        before_apply(&w, s.main_ep(), &[orders(), extra], &[]);
        journal.lock().unwrap().clear();
        w.computes.sql.lock().unwrap().clear();

        apply(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Stale);
        let error = m.error.unwrap_or_default();
        assert!(error.starts_with("merge_stale: "), "{error}");
        assert!(error.contains("since the merge was computed"), "{error}");
        assert!(error.contains("nothing was applied"), "{error}");
        assert_eq!(m.failed_phase, None);
        nothing_ran(&w, &journal, s.main_ep());

        // A role the plan doesn't touch, too.
        let (w, journal) = worker();
        let (s, id) = applying(&w).await;
        w.computes.answers_once.lock().unwrap().clear();
        before_apply(
            &w,
            s.main_ep(),
            &[orders()],
            &[crate::merge::testkit::role(16990, "intruder", true, false)],
        );
        journal.lock().unwrap().clear();
        w.computes.sql.lock().unwrap().clear();
        apply(&w, id).await;
        assert_eq!(w.metadata.merge_row(id).status, MergeStatus::Stale);
        nothing_ran(&w, &journal, s.main_ep());

        // The digest too, with the model unchanged.
        let (w, journal) = worker();
        let (s, id) = applying(&w).await;
        w.computes.answer_once_at(
            s.main_ep(),
            DIGEST_READ,
            "ffffffffffffffffffffffffffffffff\n",
        );
        journal.lock().unwrap().clear();
        w.computes.sql.lock().unwrap().clear();
        apply(&w, id).await;
        assert_eq!(w.metadata.merge_row(id).status, MergeStatus::Stale);
        nothing_ran(&w, &journal, s.main_ep());
    }

    // M1: recovery fails closed. While the crashed run's statements may still be running (they
    // don't stop, or can't be checked), the marker isn't read: the job is redelivered.
    #[tokio::test]
    async fn recovery_fails_closed_while_statements_may_run() {
        const BACKENDS: &str = "SELECT count(*) FROM pg_stat_activity";
        for still_running in [true, false] {
            let (w, _journal) = worker();
            let (s, id) = applying(&w).await;
            if still_running {
                w.computes.answer_at(s.main_ep(), BACKENDS, "1\n");
            } else {
                *w.computes.fail_sql_containing.lock().unwrap() =
                    Some((BACKENDS.into(), "connection refused".into()));
            }

            let err = w
                .handle_merge_job(MergeJob::Apply { merge_id: id }, Attempt(2))
                .await
                .expect_err("redelivered");

            assert!(err.contains("statements"), "{still_running}: {err}");
            assert_eq!(w.metadata.merge_row(id).status, MergeStatus::Applying);
            assert_eq!(ran(&w, s.main_ep(), MARKER_READ), 0, "{still_running}");
        }
        // A cancel that can't run fails closed too.
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some(("pg_cancel_backend".into(), "connection refused".into()));
        w.handle_merge_job(MergeJob::Resume { merge_id: id }, Attempt(1))
            .await
            .expect_err("redelivered");
        assert_eq!(ran(&w, s.main_ep(), MARKER_READ), 0);
    }

    // M2: only a missing marker table (or schema) means "no marker"; any other error is
    // redelivered, never read as "nothing committed".
    #[tokio::test]
    async fn only_a_missing_marker_table_means_no_marker() {
        for (error, missing) in [
            (
                "psql:<stdin>:2: ERROR:  schema \"chelabase_merge\" does not exist",
                true,
            ),
            (
                "psql:<stdin>:2: ERROR:  relation \"chelabase_merge.applied\" does not exist",
                true,
            ),
            (
                "psql:<stdin>:2: ERROR:  function pg_catalog.count() does not exist",
                false,
            ),
            ("could not connect: connection refused", false),
        ] {
            let (w, _journal) = worker();
            let (s, id) = applying(&w).await;
            let roles = step_ids(&w, id, "roles")[0].clone();
            logged_before(&w, id, "roles", &roles);
            *w.computes.fail_sql_containing.lock().unwrap() =
                Some((MARKER_READ.into(), error.into()));

            let out = w
                .handle_merge_job(MergeJob::Resume { merge_id: id }, Attempt(1))
                .await;

            let m = w.metadata.merge_row(id);
            if missing {
                assert!(out.is_ok(), "{error}");
                assert_eq!(m.status, MergeStatus::Rehearsed, "{error}");
            } else {
                assert!(out.is_err(), "{error}");
                assert_eq!(m.status, MergeStatus::Applying, "{error}");
            }
            let _ = s;
        }
    }

    // E20: Retry after a failure before phase 2 (`roles`, `enum_add`, `tx`) runs the apply from
    // the start, pre-check included (the API moves the merge `failed` → `applying` and
    // publishes Apply).
    #[tokio::test]
    async fn a_retry_after_a_tx_failure_applies_from_the_start() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        *w.computes.fail_sql_containing.lock().unwrap() = Some((
            "$chelabase_guard$".into(),
            "psql:<stdin>:3: ERROR:  out of shared memory".into(),
        ));
        apply(&w, id).await;
        assert_eq!(w.metadata.merge_row(id).status, MergeStatus::Failed);

        *w.computes.fail_sql_containing.lock().unwrap() = None;
        {
            let mut state = w.metadata.state();
            let m = state.merges.get_mut(&id).unwrap();
            m.error = None;
            m.failed_phase = None;
        }
        move_to(&w, id, MergeStatus::Applying);
        // Main as the first run left it: phase 0 created `reporter` (fix round 2).
        before_apply(&w, s.main_ep(), &[orders()], &Scenario::merged_roles());
        w.computes.sql.lock().unwrap().clear();
        apply(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Done, "{:?}", m.error);
        assert_eq!(ran(&w, s.main_ep(), "$chelabase_guard$"), 1);
        assert_eq!(
            ran(&w, s.main_ep(), CATALOG_READ),
            2,
            "the pre-check, then the finish's read"
        );
        assert!(
            first_sql(&w, s.main_ep(), CATALOG_READ)
                < first_sql(&w, s.main_ep(), "$chelabase_guard$")
        );
    }

    /// Back to `applying` for a retry, as `retryMerge` leaves it: with the attempt's start in
    /// the log (I2).
    fn retried(w: &FakeWorker, id: Uuid) {
        {
            let mut state = w.metadata.state();
            let m = state.merges.get_mut(&id).unwrap();
            m.error = None;
            m.failed_phase = None;
        }
        move_to(w, id, MergeStatus::Applying);
        let mut e = MergeEvent::new(id, "status");
        e.detail = Some(json!({ "to": "applying" }));
        w.metadata.state().merge_events.push(e);
    }

    // T14 I1: phase 1 committed but its answer was lost (the call timed out after the commit):
    // the marker says so, and the merge goes on to the online phase, not `failed` in `tx`.
    #[tokio::test]
    async fn a_phase1_timeout_after_commit_goes_on_to_online() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        w.computes.time_out_when("$chelabase_guard$", true);
        w.computes.answer_at(s.main_ep(), MARKER_READ, "1\n");

        apply(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Done, "{:?}", m.error);
        assert_eq!(ran(&w, s.main_ep(), "$chelabase_guard$"), 1);
        // The marker is read once the merge's statements are gone.
        assert!(first_sql(&w, s.main_ep(), "pg_cancel_backend") < first_sql(&w, s.main_ep(), MARKER_READ));
    }

    // T14 I1: a failure the marker can't settle (no marker) stays `failed` in `tx`.
    #[tokio::test]
    async fn a_phase1_failure_without_the_marker_stays_failed_in_tx() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        w.computes.time_out_when("$chelabase_guard$", false);
        w.computes.answer_at(s.main_ep(), MARKER_READ, "0\n");
        apply(&w, id).await;
        let m = w.metadata.merge_row(id);
        assert_eq!((m.status, m.failed_phase.as_deref()), (MergeStatus::Failed, Some("tx")));
    }

    // T14 I1 (E27: "Retry checks the marker first"): a retry after a `tx` failure whose
    // transaction did commit goes on to the online phase; it isn't stale on its own changes.
    #[tokio::test]
    async fn a_retry_after_a_tx_failure_with_the_marker_goes_on_to_online() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        *w.computes.fail_sql_containing.lock().unwrap() = Some((
            "$chelabase_guard$".into(),
            "psql:<stdin>:3: ERROR:  out of shared memory".into(),
        ));
        w.computes.answer_once_at(s.main_ep(), MARKER_READ, "0\n");
        apply(&w, id).await;
        assert_eq!(w.metadata.merge_row(id).status, MergeStatus::Failed);
        *w.computes.fail_sql_containing.lock().unwrap() = None;
        retried(&w, id);
        // Main has phase 1's changes: a pre-check would call it stale.
        w.computes.answer_at(s.main_ep(), MARKER_READ, "1\n");
        w.computes.sql.lock().unwrap().clear();

        apply(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Done, "{:?}", m.error);
        assert_eq!(ran(&w, s.main_ep(), "$chelabase_guard$"), 0, "phase 1 isn't run again");
        assert_eq!(ran(&w, s.main_ep(), DIGEST_READ), 0, "no pre-check");
    }

    /// Merge `id` failed in `tx` after its phase 1 ran (a step in its log), then recomputed as
    /// `recomputeMerge` leaves it (`computing`, no phase, no error).
    fn recomputed_after_a_tx_failure(w: &FakeWorker, id: Uuid) {
        logged_before(w, id, "tx", super::PHASE1_STEP);
        {
            let mut state = w.metadata.state();
            let m = state.merges.get_mut(&id).unwrap();
            m.error = None;
            m.failed_phase = None;
        }
        move_to(w, id, MergeStatus::Computing);
    }

    // T14 F3 fix round (I2, E27): a recompute of a merge whose phase 1 committed (a `tx` failure
    // the marker couldn't settle) never plans again: the compute stops the merge's statements,
    // reads the marker, and finding it leaves the merge failed in `online` (retry or abandon),
    // its plan unchanged and nothing read for a new one.
    #[tokio::test]
    async fn a_recompute_after_a_committed_phase1_fails_in_online() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        let plan = w.metadata.merge_row(id).plan;
        recomputed_after_a_tx_failure(&w, id);
        w.computes.answer_at(s.main_ep(), MARKER_READ, "1\n");
        w.computes.sql.lock().unwrap().clear();

        job(&w, MergeJob::Compute { merge_id: id }, 1).await;

        let m = w.metadata.merge_row(id);
        assert_eq!((m.status, m.failed_phase.as_deref()), (MergeStatus::Failed, Some("online")), "{:?}", m.error);
        assert_eq!(m.plan, plan, "the plan isn't computed again");
        let error = m.error.unwrap_or_default();
        assert!(error.contains("retry") && error.contains("abandon"), "{error}");
        assert_eq!(ran(&w, s.main_ep(), DIGEST_READ), 0, "nothing computed");
        assert!(first_sql(&w, s.main_ep(), "pg_cancel_backend") < first_sql(&w, s.main_ep(), MARKER_READ));
    }

    // I2's other cases: no marker, the compute runs (a draft with a new plan); a marker that
    // can't be read redelivers the job (still `computing`). A merge that never applied reads
    // no marker.
    #[tokio::test]
    async fn a_recompute_reads_the_marker_only_after_an_apply() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        recomputed_after_a_tx_failure(&w, id);
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some((MARKER_READ.into(), "connection refused".into()));
        let err = w
            .handle_merge_job(MergeJob::Compute { merge_id: id }, Attempt(1))
            .await
            .unwrap_err();
        assert!(err.contains("marker"), "{err}");
        assert_eq!(w.metadata.merge_row(id).status, MergeStatus::Computing);

        *w.computes.fail_sql_containing.lock().unwrap() = None;
        job(&w, MergeJob::Compute { merge_id: id }, 2).await;
        assert_eq!(w.metadata.merge_row(id).status, MergeStatus::Draft);

        let (w, _journal) = worker();
        let s2 = crate::merge::testkit::setup(&w).await;
        crate::merge::testkit::computed(&w, &s2).await;
        assert_eq!(ran(&w, s2.main_ep(), MARKER_READ), 0);
        let _ = s;
    }

    // T14 F3 fix round (M6): a phase 2 step that fails other than by a timeout (the exec
    // dropped, the statement may still run in the compute) has the merge's statements stopped
    // before the merge is failed, so Abandon can't miss a build that's still going.
    #[tokio::test]
    async fn an_online_failure_stops_the_merge_s_statements_first() {
        let (w, journal) = worker();
        let (s, id) = applying(&w).await;
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some(("CREATE INDEX CONCURRENTLY".into(), "docker exec: connection reset".into()));

        apply(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!((m.status, m.failed_phase.as_deref()), (MergeStatus::Failed, Some("online")));
        assert_eq!(ran(&w, s.main_ep(), "pg_cancel_backend"), 1);
        // Between the step's failure in the log and the merge's failed write: the cancel and the
        // check that the statements are gone.
        let entries = entries(&journal);
        let failed = entries
            .iter()
            .position(|e| *e == format!("metadata.set_merge_status {id} failed"))
            .expect("the failed write");
        let step_logged = entries[..failed]
            .iter()
            .rposition(|e| e.starts_with("metadata.append_merge_event"))
            .expect("the step's failure logged");
        let stops = entries[step_logged..failed]
            .iter()
            .filter(|e| e.starts_with(&format!("compute.sql {}", s.main_ep())))
            .count();
        assert_eq!(stops, 2, "{entries:?}");
    }

    // T14 I2: an Apply redelivered after a Retry, before it ran, runs: the first attempt's
    // steps belong to that attempt, not this one.
    #[tokio::test]
    async fn a_redelivered_apply_after_a_retry_runs() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        let roles = step_ids(&w, id, "roles")[0].clone();
        logged_before(&w, id, "roles", &roles);
        retried(&w, id);
        w.computes.answer_at(s.main_ep(), MARKER_READ, "0\n");

        job(&w, MergeJob::Apply { merge_id: id }, 2).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Done, "{:?}", m.error);
        assert_eq!(ran(&w, s.main_ep(), "$chelabase_guard$"), 1);
    }

    // E27: a Resume in `online` whose transaction isn't on the target any more (it was reset or
    // restored) fails for good: not retryable, not holding the branches.
    #[tokio::test]
    async fn a_resume_in_online_without_the_marker_fails_for_good() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        move_to(&w, id, MergeStatus::Online);
        w.computes.answer_at(s.main_ep(), MARKER_READ, "0\n");

        job(&w, MergeJob::Resume { merge_id: id }, 1).await;

        let m = w.metadata.merge_row(id);
        assert_eq!((m.status, m.failed_phase.as_deref()), (MergeStatus::Failed, None));
        assert!(m.error.clone().unwrap_or_default().starts_with("merge_target_changed: "));
        assert!(!m.state().holds_branches());
        assert_eq!(ran(&w, s.main_ep(), "CREATE INDEX CONCURRENTLY"), 0);
    }

    // E27, I3 scenario B: a target gone (deleted) fails the merge for good instead of an
    // endless redelivery.
    #[tokio::test]
    async fn a_resume_with_the_target_gone_fails_for_good() {
        for status in [BranchStatus::Deleting, BranchStatus::Deleted] {
            let (w, _journal) = worker();
            let (s, id) = applying(&w).await;
            move_to(&w, id, MergeStatus::Online);
            w.metadata.state().branches.get_mut(&s.main.id).unwrap().status = status;

            job(&w, MergeJob::Resume { merge_id: id }, 1).await;

            let m = w.metadata.merge_row(id);
            assert_eq!((m.status, m.failed_phase.as_deref()), (MergeStatus::Failed, None), "{status:?}");
            assert!(m.error.unwrap_or_default().starts_with("merge_target_gone: "), "{status:?}");
        }
        // Also from `applying` (a recovery).
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        w.metadata.state().branches.remove(&s.main.id);
        job(&w, MergeJob::Apply { merge_id: id }, 2).await;
        let m = w.metadata.merge_row(id);
        assert_eq!((m.status, m.failed_phase.as_deref()), (MergeStatus::Failed, None));
    }

    // T14 M6: a plan that can't be read once phase 1 committed fails the merge in `online` (half
    // applied: never recomputed).
    #[tokio::test]
    async fn an_unreadable_plan_after_the_marker_fails_in_online() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        w.computes.answer_at(s.main_ep(), MARKER_READ, "1\n");
        w.metadata.state().merges.get_mut(&id).unwrap().plan = Some(json!({ "steps": "nope" }));

        job(&w, MergeJob::Apply { merge_id: id }, 2).await;

        let m = w.metadata.merge_row(id);
        assert_eq!((m.status, m.failed_phase.as_deref()), (MergeStatus::Failed, Some("online")));
    }

    // Fix round 2: a genuine change found on a retry is stale, but phase 0 ran on the first
    // attempt, so the error doesn't claim nothing was applied.
    #[tokio::test]
    async fn a_retry_that_finds_a_real_change_is_stale_without_claiming_nothing_ran() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        *w.computes.fail_sql_containing.lock().unwrap() = Some((
            "$chelabase_guard$".into(),
            "psql:<stdin>:3: ERROR:  out of shared memory".into(),
        ));
        apply(&w, id).await;
        *w.computes.fail_sql_containing.lock().unwrap() = None;
        retried(&w, id);
        let extra = table(16950, "added_on_main", &[(1, "id", "bigint", 20)]);
        before_apply(
            &w,
            s.main_ep(),
            &[orders(), extra],
            &Scenario::merged_roles(),
        );

        apply(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Stale);
        let error = m.error.unwrap_or_default();
        assert!(!error.contains("nothing was applied"), "{error}");
        assert!(
            error.contains("the plan's transaction wasn't applied"),
            "{error}"
        );
    }

    // Fix round 2: a re-apply after "interrupted, nothing applied" (phase 0 had run) isn't
    // stale because of the merge's own role.
    #[tokio::test]
    async fn a_reapply_after_an_interruption_runs() {
        let (w, _journal) = worker();
        let (s, id) = applying(&w).await;
        w.computes.answers_once.lock().unwrap().clear();
        let roles = step_ids(&w, id, "roles")[0].clone();
        logged_before(&w, id, "roles", &roles);
        w.computes.answer_once_at(s.main_ep(), MARKER_READ, "0\n");
        job(&w, MergeJob::Resume { merge_id: id }, 1).await;
        assert_eq!(w.metadata.merge_row(id).status, MergeStatus::Rehearsed);

        // Applied again (`applyMerge`), main has the role the interrupted run made.
        move_to(&w, id, MergeStatus::Applying);
        before_apply(&w, s.main_ep(), &[orders()], &Scenario::merged_roles());
        apply(&w, id).await;

        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Done, "{:?}", m.error);
    }

    // Fix round 2: a retry after a phase 1a failure, with one of the plan's enum values in
    // already, runs; an enum the plan doesn't add to is still a change.
    #[tokio::test]
    async fn a_retry_after_an_enum_add_failure_runs() {
        use crate::merge::testkit::{answer_elsewhere, catalog_all, enum_type, move_to as moved};
        let status = |labels: &[(u32, &str)]| enum_type(16300, "status", labels);
        let other = |labels: &[(u32, &str)]| enum_type(16310, "mood", labels);
        let items_with_status = |oid: u32| {
            let mut t = table(oid, "items", &[(1, "id", "bigint", 20)]);
            t["columns"].as_array_mut().unwrap().push(json!({
                "attnum": 2, "name": "s", "data_type": "public.status", "type_oid": 16300,
                "collation": null, "default_expr": "'c'::public.status", "generated": "",
                "identity": "", "identity_sequence": null, "not_null": false, "comment": null,
                "acl": [],
            }));
            t
        };
        let (w, _journal) = worker();
        let s = setup(&w).await;
        let main_before = [status(&[(16301, "a")]), other(&[(16311, "x")])];
        w.computes.answer_at(
            s.main_ep(),
            CATALOG_READ,
            &catalog_all(&[orders()], &[], &main_before),
        );
        w.computes.answer_at(
            s.dev_ep(),
            CATALOG_READ,
            &catalog_all(
                &[orders(), items_with_status(16500)],
                &[],
                &[
                    status(&[(16301, "a"), (16302, "c"), (16303, "d")]),
                    other(&[(16311, "x")]),
                ],
            ),
        );
        // Dev branched from main with both enums: that's its base.
        {
            let base = chelabase_schema::catalog::parse_catalog(&catalog_all(
                &[orders()],
                &[],
                &main_before,
            ))
            .unwrap();
            let mut state = w.metadata.state();
            let entry = state
                .schema_bases
                .iter_mut()
                .rev()
                .find(|b| b.branch_id == s.dev.id)
                .unwrap();
            entry.model = Some(serde_json::to_value(&base).unwrap());
            entry.identity_map =
                serde_json::to_value(chelabase_schema::IdentityMap::self_mapped(&base)).unwrap();
        }
        let id = w.metadata.insert_merge(
            s.db,
            s.dev.id,
            s.main.id,
            chelabase_metadata::MergeDirection::IntoParent,
            MergeStatus::Computing,
        );
        job(&w, MergeJob::Compute { merge_id: id }, 1).await;
        let m = w.metadata.merge_row(id);
        assert!(
            m.plan.is_some(),
            "{:?} {:?}",
            m.error,
            m.analysis.map(|a| a["conflicts"].clone())
        );
        let enum_adds = step_ids(&w, id, "enum_add");
        assert_eq!(enum_adds.len(), 2, "{:?}", w.metadata.merge_row(id).plan);
        answer_elsewhere(&w, &[orders()], &[]);
        moved(&w, id, MergeStatus::Rehearsing);
        job(&w, MergeJob::Rehearse { merge_id: id }, 1).await;
        assert_eq!(w.metadata.merge_row(id).status, MergeStatus::Rehearsed);
        acknowledge_all(&w, id);
        let merged = [
            status(&[(16401, "a"), (16402, "c"), (16403, "d")]),
            other(&[(16311, "x")]),
        ];
        let main_after = catalog_all(&[orders(), items_with_status(16600)], &[], &merged);

        // The first run adds `c`, then `d` times out on its lock (E19).
        moved(&w, id, MergeStatus::Applying);
        w.computes.answer_once_at(
            s.main_ep(),
            CATALOG_READ,
            &catalog_all(&[orders()], &[], &main_before),
        );
        *w.computes.fail_sql_containing.lock().unwrap() = Some((
            "ADD VALUE IF NOT EXISTS 'd'".into(),
            "psql:<stdin>:2: ERROR:  canceling statement due to lock timeout".into(),
        ));
        apply(&w, id).await;
        let m = w.metadata.merge_row(id);
        assert_eq!(
            (m.status, m.failed_phase.as_deref()),
            (MergeStatus::Failed, Some("enum_add")),
            "{:?}",
            m.error
        );
        *w.computes.fail_sql_containing.lock().unwrap() = None;

        // An unrelated enum changed meanwhile: still stale.
        retried(&w, id);
        let moved_other = [
            status(&[(16301, "a"), (16302, "c")]),
            other(&[(16311, "x"), (16312, "y")]),
        ];
        w.computes.answer_once_at(
            s.main_ep(),
            CATALOG_READ,
            &catalog_all(&[orders()], &[], &moved_other),
        );
        apply(&w, id).await;
        assert_eq!(w.metadata.merge_row(id).status, MergeStatus::Stale);

        // With only the plan's own value in, the retry runs.
        retried(&w, id);
        let after_c = [
            status(&[(16301, "a"), (16302, "c")]),
            other(&[(16311, "x")]),
        ];
        w.computes.answer_once_at(
            s.main_ep(),
            CATALOG_READ,
            &catalog_all(&[orders()], &[], &after_c),
        );
        w.computes
            .answers_at
            .lock()
            .unwrap()
            .retain(|(e, n, _)| !(e == s.main_ep() && n == CATALOG_READ));
        w.computes.answer_at(s.main_ep(), CATALOG_READ, &main_after);
        apply(&w, id).await;
        let m = w.metadata.merge_row(id);
        assert_eq!(m.status, MergeStatus::Done, "{:?} {:?}", m.error, m.result);
    }
}
