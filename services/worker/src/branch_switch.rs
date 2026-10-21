//! Reset to parent and restore: build a new timeline, stop the compute, switch the branch
//! (and its endpoint) to the new timeline in one transaction, then recreate the compute.
//! `compute_ctl` can't move a running compute to another timeline, so the compute is
//! recreated, keeping its endpoint id, host and password.
//!
//! Before the switch, a failure deletes what the run made and puts the branch back to ready
//! on its old timeline, with the error recorded. After the switch, a failure leaves the branch
//! `resetting` on the new timeline, and reconcile finishes the flow from
//! `pending_timeline_id` and `superseded_timeline_id`. A guarded write that returns false
//! means a delete landed: the run removes what it made, and the delete job owns the rest.

use chelabase_metadata::{BranchKind, BranchRecord, BranchStatus, MetadataError};
use chelabase_nats::PointInTime;
use chelabase_neon::{ComputeBackend, Lsn, NeonId, StorconError};
use chelabase_types::BranchId;
use futures_util::future::BoxFuture;
use tracing::{info, warn};

use crate::branch_create::deleted;
use crate::branch_job::{Attempt, StepFailure};
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::provision::{bootstrap_sql, new_compute_token, sha256_hex};
use crate::reconcile::STALE_PROVISION_SECS;
use crate::resync::SYSTEM_DATABASE;
use crate::{COMPUTE_SETTINGS_VERSION, Worker};

/// The API's refusal, repeated by the worker.
pub const MAIN_RESET_REFUSAL: &str = "main has no parent to reset to; use restore to rewind it";
/// The API's refusal, repeated by the worker.
pub const BACKUP_REFUSAL: &str =
    "this is a restore backup with no compute; create a branch from it to connect";

/// How long after a restore records its pending timeline an unresolved detach is retried
/// before the branch fails: twice the detach's own timeout.
///
/// One detach copies every ancestor level, at any depth. Before copying it waits for each
/// ancestor's ingested WAL to reach its child's branch point, about 30 s per level at most,
/// one level after another, and answers 503 when one still lags: that's retried like a
/// timeout. So one attempt can spend up to 30 s × depth waiting, but the ancestors catch up
/// in parallel, and a retry waits only for those still behind. The bound is on wall time
/// since the pending timeline was recorded, not on attempts, so it needs no depth term. One
/// attempt is cut at [`chelabase_neon::storcon::DETACH_TIMEOUT`] (600 s, 20 lagging levels
/// at 30 s each) and then retried; the 20 minutes are the total across attempts. Ancestors
/// still behind after that mean a stuck pageserver, and the restore fails with the cause.
pub const STUCK_DETACH_SECS: i64 = 2 * chelabase_neon::storcon::DETACH_TIMEOUT.as_secs() as i64;

/// Recorded when reconcile puts a branch back on its old timeline after an attempt that left
/// no error of its own (a crash before the switch).
pub const INTERRUPTED: &str = "The last reset or restore was interrupted before it switched \
                                timelines; the branch is unchanged. Try again.";

/// Whether a re-created compute's catalog is the one it had before (C3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Catalog {
    /// The same timeline: forwarded DDL may still be in flight, the guards stay.
    Kept,
    /// A reset or restore switched the timeline: nothing forwarded applies any more.
    Replaced,
}

/// The claimed branch's endpoint, tenant and current timeline.
struct Target {
    id: BranchId,
    tenant: NeonId,
    ep: String,
    timeline: NeonId,
}

/// What a run did before its switch, for a failure to undo.
#[derive(Default)]
struct Undo {
    /// Recorded as the branch's `pending_timeline_id` before it was created.
    pending: Option<NeonId>,
    /// The compute was stopped, by this run or maybe by an interrupted one.
    compute_stopped: bool,
}

/// How a restore starts, given what an earlier delivery of its job left.
enum Resume {
    /// Nothing left (or it was dropped): resolve, create and detach.
    Fresh,
    /// The timeline an earlier attempt made, branched at the LSN (read before its detach), now
    /// detached: switch to it.
    Adopt(NeonId, Lsn),
    /// The state of the pending timeline (`pending`) is unknown: hand the job back.
    Retry { err: String, pending: String },
    /// Nothing more to do here (handled, or another run owns it).
    Stop,
}

/// How a detach ended.
enum Detached {
    Yes,
    /// Maybe still running: hand the job back, deleting nothing.
    Retry(String),
    /// Refused, with nothing running: safe to undo.
    Refused(String),
}

/// What reconcile finds of a pending timeline before it drops one (T14 M9).
enum PendingState {
    /// Detached, gone, or a reset's (hanging off the parent's timeline): safe to drop.
    Droppable,
    /// A restore's, still attached to the branch's timeline: a detach may be running on it.
    Detaching(String),
    /// Its state can't be read: left for the next pass.
    Unknown(String),
}

/// What became of a pending timeline a run set out to drop.
enum Dropped {
    /// Cleared from the branch, then deleted.
    Yes,
    /// The compare-and-swap failed: another run (or a delete) owns it now. Left alone.
    NotOurs,
    /// Couldn't be deleted (the error); recorded again when possible, so it's retried.
    Failed(String),
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// Moves the branch onto a new timeline branched from its parent's head. Failures are
    /// recorded on the branch; `Err` means metadata trouble, and the job is retried.
    pub async fn handle_branch_reset_to_parent(&self, branch_id: BranchId) -> Result<(), String> {
        self.branch_reset_attempt(branch_id, Attempt::LAST).await
    }

    /// [`Self::handle_branch_reset_to_parent`] as delivery `attempt`.
    pub(crate) fn branch_reset_attempt(
        &self,
        branch_id: BranchId,
        attempt: Attempt,
    ) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(self.branch_reset_attempt_inner(branch_id, attempt))
    }

    async fn branch_reset_attempt_inner(
        &self,
        branch_id: BranchId,
        attempt: Attempt,
    ) -> Result<(), String> {
        let Some(branch) = self.claim_for_switch(branch_id, "reset").await? else {
            return Ok(());
        };
        let target = match self.target_of(&branch).await? {
            Ok(target) => target,
            Err(err) => return self.give_back(branch_id, &err).await.map(drop),
        };
        // An interrupted attempt already switched: finish it rather than reset again.
        if let Some(superseded) = branch.superseded_timeline_id.as_deref() {
            info!(%branch_id, "finishing a reset that was cut short after its switch");
            return match superseded.parse::<NeonId>() {
                Ok(superseded) => self.finish_reset(&target, &superseded).await,
                Err(e) => {
                    let err = format!("deleting timeline: {superseded:?}: {e}");
                    self.fail_after_switch(&target, err).await
                }
            };
        }
        let mut undo = Undo::default();
        if !self
            .drop_leftover_pending(&branch, &target, &mut undo)
            .await?
        {
            return Ok(());
        }
        let parent_id = branch.parent_branch_id.expect("the claim refuses main");
        let parent_timeline = match self.parent_timeline(&branch, parent_id).await? {
            Ok((_, timeline)) => timeline,
            Err(err) => return self.undo_before_switch(&target, &undo, err).await,
        };
        let lsn = match self
            .resolve_point(&target.tenant, parent_id, &parent_timeline, None)
            .await
        {
            Ok(lsn) => lsn,
            Err(f) => return self.failed_before_switch(&target, &undo, f, attempt).await,
        };
        let Some(pending) = self.record_pending(&target).await? else {
            return Ok(());
        };
        undo.pending = Some(pending.clone());
        if let Err(e) = self
            .storage
            .create_branch_timeline(&target.tenant, &pending, &parent_timeline, lsn)
            .await
        {
            let f = StepFailure::storage("creating timeline", &e);
            return self.failed_before_switch(&target, &undo, f, attempt).await;
        }
        let history_start = self
            .history_start_at(&target.tenant, &parent_timeline, lsn)
            .await;
        if let Err(err) = self.stop_compute(&target, &mut undo).await {
            return self.undo_before_switch(&target, &undo, err).await;
        }
        match self
            .metadata
            .switch_to_pending_timeline(
                branch_id,
                pending.as_str(),
                history_start,
                &lsn.to_string(),
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.switch_refused(&target, &pending).await,
            // It may have committed: the retried job finds either the pending timeline (and
            // starts again) or the superseded one (and finishes).
            Err(e) => return Err(format!("switching timeline: {e}")),
        }
        info!(%branch_id, timeline = %pending, %lsn, "switched to the parent's head");
        self.finish_reset(&target, &target.timeline).await
    }

    /// Moves the branch onto a new timeline branched from its own at `at` and detached, and
    /// keeps the old timeline as the backup `backup_id` named `backup_name`. Failures are
    /// recorded on the branch; `Err` means metadata trouble, and the job is retried.
    pub async fn handle_branch_restore(
        &self,
        branch_id: BranchId,
        backup_id: BranchId,
        backup_name: &str,
        at: PointInTime,
    ) -> Result<(), String> {
        self.branch_restore_attempt(branch_id, backup_id, backup_name, at, Attempt::LAST)
            .await
    }

    /// [`Self::handle_branch_restore`] as delivery `attempt`.
    pub(crate) async fn branch_restore_attempt(
        &self,
        branch_id: BranchId,
        backup_id: BranchId,
        backup_name: &str,
        at: PointInTime,
        attempt: Attempt,
    ) -> Result<(), String> {
        let Some(branch) = self.claim_for_switch(branch_id, "restore").await? else {
            return Ok(());
        };
        let target = match self.target_of(&branch).await? {
            Ok(target) => target,
            Err(err) => return self.give_back(branch_id, &err).await.map(drop),
        };
        // An interrupted attempt already switched (the backup exists): finish it.
        let backup = self
            .metadata
            .get_branch(backup_id)
            .await
            .map_err(|e| e.to_string())?;
        if backup.is_some_and(|b| b.parent_branch_id == Some(branch_id)) {
            info!(%branch_id, %backup_id, "finishing a restore that was cut short after its switch");
            return self.finish_restore(&target).await;
        }
        let mut undo = Undo::default();
        // The point the restored timeline was branched at (step 6g: the bases current there
        // are copied with the switch).
        let (pending, restore_lsn) = match self.resume_restore(&branch, &target, &mut undo).await? {
            Resume::Adopt(pending, lsn) => (pending, lsn),
            Resume::Retry { err, pending } => {
                return self.detach_unresolved(&target, &pending, err).await;
            }
            Resume::Stop => return Ok(()),
            Resume::Fresh => {
                let lsn = match self
                    .resolve_point(&target.tenant, branch_id, &target.timeline, Some(&at))
                    .await
                {
                    Ok(lsn) => lsn,
                    Err(f) => return self.failed_before_switch(&target, &undo, f, attempt).await,
                };
                // T14 M8: the branch's history begins at its branch point; before it lies the
                // parent's older state.
                if let Some(start) = branch
                    .branched_at_lsn
                    .as_deref()
                    .and_then(|l| l.parse::<Lsn>().ok())
                    && lsn < start
                {
                    let f = StepFailure::permanent(format!(
                        "resolving the point: the LSN {lsn} is before {}'s history begins (its \
                         branch point, {start})",
                        branch.name
                    ));
                    return self.failed_before_switch(&target, &undo, f, attempt).await;
                }
                let Some(pending) = self.record_pending(&target).await? else {
                    return Ok(());
                };
                undo.pending = Some(pending.clone());
                if let Err(e) = self
                    .storage
                    .create_branch_timeline(&target.tenant, &pending, &target.timeline, lsn)
                    .await
                {
                    let f = StepFailure::storage("creating timeline", &e);
                    return self.failed_before_switch(&target, &undo, f, attempt).await;
                }
                info!(%branch_id, timeline = %pending, %lsn, "restored timeline created");
                // The restored timeline must not depend on the backup's (or the parents').
                match self.detach(&target, &pending, true).await? {
                    Detached::Yes => {}
                    Detached::Retry(err) => {
                        return self.detach_unresolved(&target, pending.as_str(), err).await;
                    }
                    Detached::Refused(err) => {
                        return self.undo_before_switch(&target, &undo, err).await;
                    }
                }
                (pending, lsn)
            }
        };
        if let Err(err) = self.stop_compute(&target, &mut undo).await {
            return self.undo_before_switch(&target, &undo, err).await;
        }
        match self
            .metadata
            .switch_to_restored_timeline(
                branch_id,
                pending.as_str(),
                backup_id,
                backup_name,
                &restore_lsn.to_string(),
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => return self.switch_refused(&target, &pending).await,
            // Nothing was written; a later restore gets a later name.
            Err(e @ MetadataError::BranchNameTaken(_)) => {
                let err = format!("switching timeline: {e}");
                return self.undo_before_switch(&target, &undo, err).await;
            }
            // It may have committed: the retried job finds either the pending timeline (and
            // starts again) or the backup (and finishes).
            Err(e) => return Err(format!("switching timeline: {e}")),
        }
        info!(%branch_id, timeline = %pending, %backup_id, "switched to the restored timeline");
        self.finish_restore(&target).await
    }

    /// A new compute for `branch`'s endpoint `ep` on whatever timeline its row names: the new
    /// token's hash is written first (the compute's first spec request must already pass), then
    /// the container is created and waited for, then the role and data API bootstraps run (a
    /// restore can rewind both); then the endpoint records [`COMPUTE_SETTINGS_VERSION`], and
    /// last the login-role resync runs and, for a kept catalog, a member-role reconcile is
    /// asked for (PB R12: the guard follows the branch's settings). The catalog is the one the compute had: the forwarded
    /// guards stay (an upgrade, a missing compute, a restart on the old timeline).
    pub(crate) async fn recreate_compute(&self, branch: BranchId, ep: &str) -> Result<(), String> {
        self.recreate_compute_with(branch, ep, Catalog::Kept).await
    }

    /// [`Self::recreate_compute`]; with [`Catalog::Replaced`] (after a reset or restore's
    /// switch), the resync first lifts the forwarded guards (C3).
    pub(crate) fn recreate_compute_with<'a>(
        &'a self,
        branch: BranchId,
        ep: &'a str,
        catalog: Catalog,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(self.recreate_compute_inner(branch, ep, catalog))
    }

    async fn recreate_compute_inner(
        &self,
        branch: BranchId,
        ep: &str,
        catalog: Catalog,
    ) -> Result<(), String> {
        let database_name = self
            .metadata
            .neon_databases(ep)
            .await
            .map_err(|e| format!("starting compute: {e}"))?
            .into_iter()
            .next()
            .map(|d| d.name)
            .ok_or_else(|| format!("starting compute: endpoint {ep} has no database"))?;
        let token = new_compute_token();
        self.metadata
            .replace_compute_token(ep, &sha256_hex(&token))
            .await
            .map_err(|e| format!("starting compute: replacing the compute token: {e}"))?;
        self.computes
            .create(ep, &token)
            .await
            .map_err(|e| format!("starting compute: {e:#}"))?;
        self.wait_until_ready(ep, &database_name).await?;
        self.computes
            .admin_sql(ep, &database_name, &bootstrap_sql(self.owner_superuser))
            .await
            .map_err(|e| format!("bootstrapping roles: {e:#}"))?;
        // A reset or restore can bring back a catalog without the realtime role, or with an
        // older password.
        self.ensure_realtime(ep, &database_name).await;
        self.bootstrap_served_on(branch, ep, &database_name, &[])
            .await
            .map(|_| ())
            .map_err(|e| format!("bootstrapping the data API: {e:#}"))?;
        // A reset or restore brings back the timeline's own `chelabase_auth_sync` password:
        // the branch's stored one is set again (with the rest of the users machinery).
        let database = self
            .metadata
            .get_branch(branch)
            .await
            .map_err(|e| format!("bootstrapping app users: reading the branch: {e}"))?
            .map(|b| b.database_id);
        if let Some(database) = database {
            self.bootstrap_users_for(branch, database, ep)
                .await
                .map_err(|e| format!("bootstrapping app users: {e:#}"))?;
        }
        // Recorded before the resync: the compute now runs these settings whatever the
        // resync does, and a resync that keeps failing gets a plain `Resync` from reconcile,
        // not a re-creation every pass.
        self.record_compute_settings(ep).await;
        let resynced = self.resync_endpoint(branch, ep, catalog).await;
        // PB R12: a restarted compute's guard may be missing or stale (a restart after a failed
        // reconcile): the member-role reconcile brings it current. A replaced catalog (reset,
        // restore) asks once the branch is ready again (`finish_ready`).
        if catalog == Catalog::Kept {
            self.member_role_requests.request(branch);
        }
        resynced
    }

    /// Records that `ep`'s compute runs [`COMPUTE_SETTINGS_VERSION`]'s settings. A failure
    /// only logs: reconcile's `UpgradeCompute` then re-creates the compute once more.
    pub(crate) async fn record_compute_settings(&self, ep: &str) {
        if let Err(e) = self
            .metadata
            .set_compute_settings_version(ep, COMPUTE_SETTINGS_VERSION)
            .await
        {
            warn!(endpoint_id = %ep, "recording the compute settings version: {e}");
        }
    }

    /// The resync under `ep`'s lock, through the system database (the main one may be
    /// dropped). A [`Catalog::Replaced`] catalog has nothing in flight: the forwarded guards
    /// are lifted first, under the same lock (C3).
    pub(crate) async fn resync_endpoint(
        &self,
        branch: BranchId,
        ep: &str,
        catalog: Catalog,
    ) -> Result<(), String> {
        let _lock = self
            .metadata
            .lock_endpoint(ep)
            .await
            .map_err(|e| format!("syncing login roles: locking the endpoint: {e}"))?;
        if catalog == Catalog::Replaced {
            self.metadata
                .clear_forwarded(ep)
                .await
                .map_err(|e| format!("syncing login roles: clearing the forwarded marks: {e}"))?;
        }
        self.resync_locked(branch, ep, SYSTEM_DATABASE).await
    }

    /// Reconcile's end of a reset or restore left `resetting` (a crash, or a failure the run
    /// couldn't undo): drop any pending timeline (cleared before it's deleted), recreate the
    /// compute if it's missing, delete any superseded timeline, then resetting → ready.
    ///
    /// Any failure is recorded on the branch, which stays `resetting` for the next pass, and
    /// returned for the log.
    pub(crate) async fn finish_switch(&self, id: BranchId) -> Result<(), String> {
        let branch = self
            .metadata
            .get_branch(id)
            .await
            .map_err(|e| e.to_string())?;
        if branch.is_none_or(|b| b.status != BranchStatus::Resetting) {
            return Ok(());
        }
        // Claimed only while still stale, and the claim refreshes the row: of two passes (or a
        // slow switch job and a pass) that both saw it stale, only the first takes it, so only
        // one run replaces the compute token and creates the compute.
        let claimed = self
            .metadata
            .claim_stale_reset(id, STALE_PROVISION_SECS)
            .await
            .map_err(|e| e.to_string())?;
        if !claimed {
            return Ok(());
        }
        // Read again under the claim: what the attempt left (its pending and superseded
        // timelines, its error) may have changed since the first read.
        let branch = self
            .metadata
            .get_branch(id)
            .await
            .map_err(|e| e.to_string())?;
        let Some(branch) = branch.filter(|b| b.status == BranchStatus::Resetting) else {
            return Ok(());
        };
        let target = self.target_of(&branch).await??;
        info!(branch_id = %id, pending = ?branch.pending_timeline_id, superseded = ?branch.superseded_timeline_id, "finishing a reset or restore left resetting");
        let mut failure = None;
        let mut dropped_pending = false;
        if let Some(pending) = branch.pending_timeline_id.as_deref() {
            // T14 M9: a restore's timeline still attached to the branch's may have a detach
            // running on it (its job lost or slow): it isn't deleted while that's possible.
            // The job's own bound, plus one more detach attempt: a detach started just before
            // the bound runs up to `DETACH_TIMEOUT` past it.
            let bound = STUCK_DETACH_SECS + chelabase_neon::storcon::DETACH_TIMEOUT.as_secs() as i64;
            let young = branch
                .pending_timeline_at
                .is_some_and(|at| chrono::Utc::now() - at < chrono::Duration::seconds(bound));
            match self.restore_maybe_detaching(&target, pending).await {
                PendingState::Unknown(err) => return self.still_resetting(id, err).await,
                PendingState::Detaching(err) if young => {
                    return self.still_resetting(id, err).await;
                }
                // Past that, any detach has ended: the last one could start no later than
                // `STUCK_DETACH_SECS`, and each is cut at `DETACH_TIMEOUT`.
                PendingState::Detaching(_) | PendingState::Droppable => {}
            }
            match self.drop_pending(id, &target.tenant, pending).await? {
                Dropped::Yes => dropped_pending = true,
                Dropped::NotOurs => {
                    info!(branch_id = %id, timeline = pending, "another run or a delete owns the pending timeline; leaving it");
                    return Ok(());
                }
                Dropped::Failed(err) => failure = Some(err),
            }
        }
        // Even when the pending timeline stays: the branch keeps working on its current one.
        let restarted = match self.computes.status(&target.ep).await {
            Ok(Some(_)) => Ok(()),
            Ok(None) => {
                self.recreate_compute_with(target.id, &target.ep, Catalog::Replaced)
                    .await
            }
            Err(e) => Err(format!("starting compute: {e:#}")),
        };
        if self.gone_meanwhile(&target).await? {
            return Ok(());
        }
        if let Err(err) = restarted {
            failure.get_or_insert(err);
        }
        if let Some(err) = failure {
            return self.still_resetting(id, err).await;
        }
        if let Some(superseded) = branch.superseded_timeline_id.as_deref() {
            // The switch moved it off the branch; never delete the timeline the branch is on.
            if branch.timeline_id.as_deref() != Some(superseded) {
                let deleted = match superseded.parse::<NeonId>() {
                    Ok(timeline) => self
                        .storage
                        .delete_timeline(&target.tenant, &timeline)
                        .await
                        .map_err(|e| format!("deleting timeline: {e:#}")),
                    Err(e) => Err(format!("deleting timeline: {superseded:?}: {e}")),
                };
                if let Err(err) = deleted {
                    return self.still_resetting(id, err).await;
                }
            }
            self.metadata
                .clear_superseded_timeline(id)
                .await
                .map_err(|e| e.to_string())?;
            // Only a reset leaves a superseded timeline: its new branch point is the base
            // (step 6g), as `finish_reset` records it.
            if !dropped_pending {
                self.record_reset_bases(id, &target.ep).await;
            }
        }
        if !dropped_pending {
            // The switch happened (or never started): the flow is done, its old error with it.
            return self.finish_ready(&target).await;
        }
        // Back on the old timeline: the reset or restore didn't happen, and the error says so.
        // The API's claim cleared the error, so any error here is this attempt's own.
        if branch.error.is_none() {
            self.metadata
                .set_branch_error(id, Some(INTERRUPTED))
                .await
                .map_err(|e| e.to_string())?;
        }
        let ready = self
            .metadata
            .set_branch_status(id, &[BranchStatus::Resetting], BranchStatus::Ready)
            .await
            .map_err(|e| e.to_string())?;
        if !ready {
            self.gone_meanwhile(&target).await?;
        }
        Ok(())
    }

    /// Whether `pending` may still have a detach running on it.
    async fn restore_maybe_detaching(&self, target: &Target, pending: &str) -> PendingState {
        let Ok(timeline) = pending.parse::<NeonId>() else {
            return PendingState::Droppable;
        };
        match self.storage.ancestor(&target.tenant, &timeline).await {
            Ok(Some((parent, _))) if parent == target.timeline => PendingState::Detaching(format!(
                "detaching: the restored timeline {pending} is still attached; a detach may \
                 still be running on it"
            )),
            Ok(_) => PendingState::Droppable,
            Err(StorconError::Rejected { status, .. })
                if status == reqwest::StatusCode::NOT_FOUND =>
            {
                PendingState::Droppable
            }
            Err(e) => PendingState::Unknown(format!("reading the pending timeline {pending}: {e:#}")),
        }
    }

    /// Records `err` on a branch reconcile leaves `resetting`, and returns it for the log.
    async fn still_resetting(&self, id: BranchId, err: String) -> Result<(), String> {
        warn!(branch_id = %id, "left resetting for the next reconcile pass: {err}");
        self.metadata
            .set_branch_error(id, Some(&err))
            .await
            .map_err(|e| e.to_string())?;
        Err(err)
    }

    /// Clears `pending` from the branch by compare-and-swap, then deletes it. The clear comes
    /// first so that two runs never both act on one pending timeline: the one whose swap fails
    /// leaves it alone. A failed delete records it again so it's retried; a crash between the
    /// clear and the delete leaks it unrecorded, and reconcile reports it (never deletes it).
    async fn drop_pending(
        &self,
        id: BranchId,
        tenant: &NeonId,
        pending: &str,
    ) -> Result<Dropped, String> {
        let timeline = match pending.parse::<NeonId>() {
            Ok(timeline) => timeline,
            Err(e) => {
                return Ok(Dropped::Failed(format!(
                    "deleting timeline: {pending:?}: {e}"
                )));
            }
        };
        let cleared = self
            .metadata
            .swap_pending_timeline(id, Some(pending), None)
            .await
            .map_err(|e| e.to_string())?;
        if !cleared {
            return Ok(Dropped::NotOurs);
        }
        let Err(e) = self.storage.delete_timeline(tenant, &timeline).await else {
            return Ok(Dropped::Yes);
        };
        let err = format!("deleting timeline: {e:#}");
        let recorded = self
            .metadata
            .swap_pending_timeline(id, None, Some(pending))
            .await
            .map_err(|e| e.to_string())?;
        if !recorded {
            warn!(branch_id = %id, timeline = pending, "couldn't record the undeleted pending timeline again; reconcile reports it");
        }
        Ok(Dropped::Failed(err))
    }

    /// Reads the branch (the API left it `resetting`), refuses what can't be reset or
    /// restored, re-claims it, and re-checks for children. `None` when there's nothing to do.
    async fn claim_for_switch(
        &self,
        id: BranchId,
        verb: &str,
    ) -> Result<Option<BranchRecord>, String> {
        let branch = self
            .metadata
            .get_branch(id)
            .await
            .map_err(|e| e.to_string())?;
        let Some(branch) = branch else {
            warn!(branch_id = %id, "no branch row; dropping {verb} job");
            return Ok(None);
        };
        // The API claims ready → resetting before publishing, so a job finding anything else
        // is stale: a redelivery after a success whose ack was lost, or a publish that timed
        // out after the API released its claim. Redoing it would wipe writes made since.
        if branch.status != BranchStatus::Resetting {
            info!(branch_id = %id, status = branch.status.as_str(), "branch not resetting; stale {verb} job, skipping");
            return Ok(None);
        }
        let refusal = if branch.kind == BranchKind::Backup {
            Some(BACKUP_REFUSAL)
        } else if verb == "reset" && branch.parent_branch_id.is_none() {
            Some(MAIN_RESET_REFUSAL)
        } else {
            None
        };
        if let Some(refusal) = refusal {
            self.give_back(id, refusal).await?;
            return Ok(None);
        }
        let claimed = self
            .metadata
            .set_branch_status(id, &[BranchStatus::Resetting], BranchStatus::Resetting)
            .await
            .map_err(|e| e.to_string())?;
        if !claimed {
            info!(branch_id = %id, "branch no longer resetting; skipping {verb}");
            return Ok(None);
        }
        let children = self
            .metadata
            .live_children(id)
            .await
            .map_err(|e| e.to_string())?;
        if !children.is_empty() {
            let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
            let refusal = format!(
                "cannot {verb} {}: it has child branches ({}); delete them first",
                branch.name,
                names.join(", ")
            );
            self.give_back(id, &refusal).await?;
            return Ok(None);
        }
        info!(branch_id = %id, name = %branch.name, "{verb} started");
        Ok(Some(branch))
    }

    /// The branch's tenant, endpoint and timeline, or the error it goes back to ready with.
    /// `Err` only for metadata trouble.
    async fn target_of(&self, branch: &BranchRecord) -> Result<Result<Target, String>, String> {
        let tenant = self.tenant_of(branch.database_id).await?;
        let (Some(tenant), Some(timeline), Some(ep)) = (
            tenant,
            branch.timeline_id.as_deref(),
            branch.endpoint_id.clone(),
        ) else {
            return Ok(Err(
                "resolving the point: the branch has no timeline or compute".to_string(),
            ));
        };
        Ok(timeline
            .parse()
            .map(|timeline| Target {
                id: branch.id,
                tenant,
                ep,
                timeline,
            })
            .map_err(|e| format!("resolving the point: timeline {timeline:?}: {e}")))
    }

    /// A restore's pending timeline left by an earlier delivery of the job (its detach timed
    /// out, or the worker stopped). A detached one, or one never created, is dropped and the
    /// restore starts afresh: nothing runs on it. An attached one may have a detach still
    /// running (one that timed out), so it's never deleted: it gets the detach again (the
    /// storage controller runs it after any still in progress) and is then adopted; it was
    /// branched at the point asked for. One whose state can't be read is left alone and the
    /// job retried later.
    async fn resume_restore(
        &self,
        branch: &BranchRecord,
        target: &Target,
        undo: &mut Undo,
    ) -> Result<Resume, String> {
        let Some(leftover) = branch.pending_timeline_id.as_deref() else {
            return Ok(Resume::Fresh);
        };
        let state = match leftover.parse::<NeonId>() {
            Ok(timeline) => {
                let ancestor = self.storage.ancestor_lsn(&target.tenant, &timeline).await;
                Some((timeline, ancestor))
            }
            Err(_) => None,
        };
        match state {
            Some((timeline, Ok(Some(lsn)))) => {
                info!(branch_id = %target.id, %timeline, "finishing the detach of the timeline an earlier attempt made");
                undo.pending = Some(timeline.clone());
                // That attempt may have stopped the compute too.
                undo.compute_stopped = true;
                match self.detach(target, &timeline, false).await? {
                    Detached::Yes => Ok(Resume::Adopt(timeline, lsn)),
                    Detached::Retry(err) => Ok(Resume::Retry {
                        err,
                        pending: leftover.to_string(),
                    }),
                    Detached::Refused(err) => {
                        self.undo_before_switch(target, undo, err).await?;
                        Ok(Resume::Stop)
                    }
                }
            }
            Some((_, Ok(None))) => self.fresh_after_dropping(branch, target, undo).await,
            Some((_, Err(StorconError::Rejected { status, .. })))
                if status == reqwest::StatusCode::NOT_FOUND =>
            {
                self.fresh_after_dropping(branch, target, undo).await
            }
            None => self.fresh_after_dropping(branch, target, undo).await,
            Some((_, Err(e))) => {
                let err = format!("detaching: {e:#}");
                warn!(branch_id = %target.id, "can't tell whether the detach is still running; retrying later: {err}");
                Ok(Resume::Retry {
                    err,
                    pending: leftover.to_string(),
                })
            }
        }
    }

    async fn fresh_after_dropping(
        &self,
        branch: &BranchRecord,
        target: &Target,
        undo: &mut Undo,
    ) -> Result<Resume, String> {
        Ok(if self.drop_leftover_pending(branch, target, undo).await? {
            Resume::Fresh
        } else {
            Resume::Stop
        })
    }

    /// Detaches `pending` from its ancestors, keeping the branch's claim fresh meanwhile (a
    /// detach can take minutes). After a failure the timeline is read again: detached means
    /// done. Otherwise the detach may still be running, so nothing may be deleted: `Retry`.
    /// Only a refusal (4xx) of the `first` detach of a timeline is `Refused`, safe to undo;
    /// a later one may be refused because an earlier detach is still running.
    async fn detach(
        &self,
        target: &Target,
        pending: &NeonId,
        first: bool,
    ) -> Result<Detached, String> {
        let detached = self
            .keep_claimed(
                target.id,
                self.storage.detach_ancestor(&target.tenant, pending),
            )
            .await;
        let Err(e) = detached else {
            return Ok(Detached::Yes);
        };
        let err = format!("detaching: {e:#}");
        if first && matches!(e, StorconError::Rejected { .. }) {
            return Ok(Detached::Refused(err));
        }
        match self.storage.ancestor_lsn(&target.tenant, pending).await {
            Ok(None) => {
                info!(branch_id = %target.id, timeline = %pending, "detached after all: {err}");
                Ok(Detached::Yes)
            }
            Ok(Some(_)) => {
                warn!(branch_id = %target.id, timeline = %pending, "still attached, maybe still detaching; retrying later: {err}");
                Ok(Detached::Retry(err))
            }
            Err(read) => {
                warn!(branch_id = %target.id, timeline = %pending, "can't read the timeline ({read:#}); retrying later: {err}");
                Ok(Detached::Retry(err))
            }
        }
    }

    /// A detach whose outcome is unknown (it may still be running on `pending`): `err` is
    /// recorded on the branch, which stays `resetting`, and the job is handed back. Every
    /// delivery renews the claim, so reconcile never steps in; instead, once `pending` was
    /// recorded more than [`STUCK_DETACH_SECS`] ago, the branch fails with the cause. Nothing is
    /// deleted either way: the branch keeps its old timeline, and `pending` stays recorded.
    async fn detach_unresolved(
        &self,
        target: &Target,
        pending: &str,
        err: String,
    ) -> Result<(), String> {
        let gave_up = format!(
            "{err}; still unresolved {} minutes after the restored timeline was made, so the \
             restore stopped. The branch stays on its old timeline, and the restored timeline \
             {pending} was kept, since a detach may still be running on it; delete the branch \
             to remove it",
            STUCK_DETACH_SECS / 60
        );
        let failed = self
            .metadata
            .fail_stuck_restore(target.id, STUCK_DETACH_SECS, &gave_up)
            .await
            .map_err(|e| e.to_string())?;
        if failed {
            warn!(branch_id = %target.id, timeline = pending, "restore failed: {gave_up}");
            return Ok(());
        }
        self.metadata
            .set_branch_error(target.id, Some(&err))
            .await
            .map_err(|e| e.to_string())?;
        Err(err)
    }

    /// Runs `work`, refreshing the branch's claim (resetting → resetting) every
    /// `claim_refresh` meanwhile, so reconcile never takes a long step for a stalled one.
    async fn keep_claimed<T>(&self, id: BranchId, work: impl Future<Output = T>) -> T {
        tokio::pin!(work);
        let mut refresh = tokio::time::interval(self.claim_refresh);
        refresh.tick().await;
        loop {
            tokio::select! {
                done = &mut work => return done,
                _ = refresh.tick() => {
                    if let Err(e) = self
                        .metadata
                        .set_branch_status(id, &[BranchStatus::Resetting], BranchStatus::Resetting)
                        .await
                    {
                        warn!(branch_id = %id, "refreshing the claim: {e}");
                    }
                }
            }
        }
    }

    /// Deletes a pending timeline an interrupted attempt left, so recording a new one can't
    /// leak it; it's cleared from the branch first (see [`Self::drop_pending`]). False (stop)
    /// when another run or a delete owns it, or when it can't be deleted: then the error is
    /// recorded and the branch left `resetting` for reconcile.
    async fn drop_leftover_pending(
        &self,
        branch: &BranchRecord,
        target: &Target,
        undo: &mut Undo,
    ) -> Result<bool, String> {
        let Some(leftover) = branch.pending_timeline_id.as_deref() else {
            return Ok(true);
        };
        info!(branch_id = %target.id, timeline = leftover, "deleting a pending timeline an interrupted attempt left");
        // That attempt may have stopped the compute too.
        undo.compute_stopped = true;
        match self
            .drop_pending(target.id, &target.tenant, leftover)
            .await?
        {
            Dropped::Yes => Ok(true),
            Dropped::NotOurs => {
                info!(branch_id = %target.id, "another run or a delete owns that timeline; stopping");
                Ok(false)
            }
            Dropped::Failed(err) => {
                // Recorded again, so reconcile deletes it; meanwhile the compute runs on the
                // old timeline.
                warn!(branch_id = %target.id, "leaving the branch resetting for reconcile: {err}");
                if !self.restart_unless_deleted(target).await? {
                    return Ok(false);
                }
                self.metadata
                    .set_branch_error(target.id, Some(&err))
                    .await
                    .map_err(|e| e.to_string())?;
                Ok(false)
            }
        }
    }

    /// Records a new timeline id as pending before it's created. `None` once a delete began.
    async fn record_pending(&self, target: &Target) -> Result<Option<NeonId>, String> {
        let pending = NeonId::new();
        let recorded = self
            .metadata
            .set_pending_timeline(target.id, Some(pending.as_str()))
            .await
            .map_err(|e| e.to_string())?;
        if !recorded {
            info!(branch_id = %target.id, "deleted meanwhile; stopping");
            return Ok(None);
        }
        Ok(Some(pending))
    }

    async fn stop_compute(&self, target: &Target, undo: &mut Undo) -> Result<(), String> {
        self.computes
            .delete(&target.ep)
            .await
            .map_err(|e| format!("removing compute: {e:#}"))?;
        undo.compute_stopped = true;
        Ok(())
    }

    /// A failed step before the switch: undone, then retried (a transient failure with
    /// deliveries left) or given back.
    async fn failed_before_switch(
        &self,
        target: &Target,
        undo: &Undo,
        f: StepFailure,
        attempt: Attempt,
    ) -> Result<(), String> {
        let retry = f.retry(attempt);
        self.undo_then(target, undo, f.message, retry).await
    }

    /// A failure before the switch: delete the pending timeline, clear it, restart the
    /// compute if it was stopped, and go back to ready on the old timeline with `err`.
    async fn undo_before_switch(
        &self,
        target: &Target,
        undo: &Undo,
        err: String,
    ) -> Result<(), String> {
        self.undo_then(target, undo, err, false).await
    }

    /// [`Self::undo_before_switch`], except that with `retry` the branch stays `resetting` and
    /// `Err(err)` hands the job back for another delivery.
    async fn undo_then(
        &self,
        target: &Target,
        undo: &Undo,
        err: String,
        retry: bool,
    ) -> Result<(), String> {
        warn!(branch_id = %target.id, retry, "failed before the switch: {err}");
        if let Some(pending) = &undo.pending {
            if let Err(e) = self.storage.delete_timeline(&target.tenant, pending).await {
                // Still recorded, so it can't leak: reconcile deletes it and finishes.
                let err = format!("{err}; then deleting timeline: {e:#}");
                warn!(branch_id = %target.id, "leaving the branch resetting for reconcile: {err}");
                if undo.compute_stopped && !self.restart_unless_deleted(target).await? {
                    return Ok(());
                }
                return self
                    .metadata
                    .set_branch_error(target.id, Some(&err))
                    .await
                    .map_err(|e| e.to_string());
            }
            // Only this run's own: never a pending timeline another run has recorded since.
            let cleared = self
                .metadata
                .swap_pending_timeline(target.id, Some(pending.as_str()), None)
                .await
                .map_err(|e| e.to_string())?;
            if !cleared {
                info!(branch_id = %target.id, "deleted meanwhile, or another run took over; stopping");
                return Ok(());
            }
        }
        if undo.compute_stopped && !self.restart_unless_deleted(target).await? {
            return Ok(());
        }
        if retry {
            return Err(err);
        }
        if !self.give_back(target.id, &err).await? && undo.compute_stopped {
            self.gone_meanwhile(target).await?;
        }
        Ok(())
    }

    /// Records `err` and puts a resetting branch back to ready. True when it moved.
    async fn give_back(&self, id: BranchId, err: &str) -> Result<bool, String> {
        warn!(branch_id = %id, "back to ready: {err}");
        self.metadata
            .set_branch_error(id, Some(err))
            .await
            .map_err(|e| e.to_string())?;
        self.metadata
            .set_branch_status(id, &[BranchStatus::Resetting], BranchStatus::Ready)
            .await
            .map_err(|e| e.to_string())
    }

    /// The switch was refused: the branch is no longer resetting on `pending`. Either a delete
    /// landed (abandon), or another run of the same flow replaced the pending timeline and owns
    /// the flow now: stop, touching nothing of its. `pending` is removed either way; nothing
    /// switched to it, and in the second case nothing records it any more.
    async fn switch_refused(&self, target: &Target, pending: &NeonId) -> Result<(), String> {
        let current = self
            .metadata
            .get_branch(target.id)
            .await
            .map_err(|e| e.to_string())?;
        if deleted(current.as_ref()) {
            return self.abandon_before_switch(target, pending).await;
        }
        let theirs = current.and_then(|b| b.pending_timeline_id);
        info!(branch_id = %target.id, mine = %pending, ?theirs, "another run owns this flow now; stopping");
        if let Err(e) = self.storage.delete_timeline(&target.tenant, pending).await {
            warn!(branch_id = %target.id, timeline = %pending, "couldn't remove this run's unrecorded timeline: {e:#}");
        }
        Ok(())
    }

    /// A delete landed before the switch. Remove the pending timeline (the delete job also
    /// finds it recorded) and stop; the delete job owns the rest.
    async fn abandon_before_switch(&self, target: &Target, pending: &NeonId) -> Result<(), String> {
        info!(branch_id = %target.id, "deleted before the switch; removing the pending timeline");
        if let Err(e) = self.storage.delete_timeline(&target.tenant, pending).await {
            warn!(branch_id = %target.id, timeline = %pending, "the delete job retries this timeline: {e:#}");
        }
        Ok(())
    }

    /// Steps 7-9 of reset: the compute on the new timeline, then the old timeline goes.
    fn finish_reset<'a>(
        &'a self,
        target: &'a Target,
        superseded: &'a NeonId,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(self.finish_reset_inner(target, superseded))
    }

    async fn finish_reset_inner(&self, target: &Target, superseded: &NeonId) -> Result<(), String> {
        if let Err(err) = self.forget_member_roles(target).await {
            return self.fail_after_switch(target, err).await;
        }
        if let Err(err) = self
            .recreate_compute_with(target.id, &target.ep, Catalog::Replaced)
            .await
        {
            return self.fail_after_switch(target, err).await;
        }
        if self.gone_meanwhile(target).await? {
            return Ok(());
        }
        // Step 6g: the parent's head is the branch's new base (also on a resumed run).
        self.record_reset_bases(target.id, &target.ep).await;
        if let Err(e) = self
            .storage
            .delete_timeline(&target.tenant, superseded)
            .await
        {
            let err = format!("deleting timeline: {e:#}");
            return self.fail_after_switch(target, err).await;
        }
        self.metadata
            .clear_superseded_timeline(target.id)
            .await
            .map_err(|e| e.to_string())?;
        self.finish_ready(target).await
    }

    /// Steps 6-7 of restore: the compute on the restored timeline.
    async fn finish_restore(&self, target: &Target) -> Result<(), String> {
        if let Err(err) = self.forget_member_roles(target).await {
            return self.fail_after_switch(target, err).await;
        }
        if let Err(err) = self
            .recreate_compute_with(target.id, &target.ep, Catalog::Replaced)
            .await
        {
            return self.fail_after_switch(target, err).await;
        }
        if self.gone_meanwhile(target).await? {
            return Ok(());
        }
        self.finish_ready(target).await
    }

    /// T8 C2: the new timeline's catalog has other member roles and passwords (a restore's
    /// past ones, or the parent's), so the branch's member-role rows go before its compute
    /// serves: the next act-as finds no row and reconciles, and every role gets a fresh
    /// password. Idempotent.
    async fn forget_member_roles(&self, target: &Target) -> Result<(), String> {
        self.metadata
            .delete_branch_member_roles(target.id.as_uuid())
            .await
            .map(drop)
            .map_err(|e| format!("forgetting the member roles: {e}"))
    }

    /// resetting → ready, clearing any old error. The member roles are forgotten first (a run
    /// resumed after the switch may not have), and reconciled once ready.
    async fn finish_ready(&self, target: &Target) -> Result<(), String> {
        self.forget_member_roles(target).await?;
        self.metadata
            .set_branch_error(target.id, None)
            .await
            .map_err(|e| e.to_string())?;
        let ready = self
            .metadata
            .set_branch_status(target.id, &[BranchStatus::Resetting], BranchStatus::Ready)
            .await
            .map_err(|e| e.to_string())?;
        if !ready {
            self.gone_meanwhile(target).await?;
            return Ok(());
        }
        info!(branch_id = %target.id, "branch ready on its new timeline");
        self.member_role_requests.request(target.id);
        Ok(())
    }

    /// A failure after the switch: the branch stays `resetting` on its new timeline, and
    /// reconcile finishes the flow.
    async fn fail_after_switch(&self, target: &Target, err: String) -> Result<(), String> {
        if self.gone_meanwhile(target).await? {
            return Ok(());
        }
        warn!(branch_id = %target.id, "failed after the switch; reconcile finishes it: {err}");
        self.metadata
            .set_branch_error(target.id, Some(&err))
            .await
            .map_err(|e| e.to_string())
    }

    /// True when a delete began: the compute this run may have started is removed, since the
    /// delete job may already have passed it.
    async fn gone_meanwhile(&self, target: &Target) -> Result<bool, String> {
        let current = self
            .metadata
            .get_branch(target.id)
            .await
            .map_err(|e| e.to_string())?;
        if !deleted(current.as_ref()) {
            return Ok(false);
        }
        info!(branch_id = %target.id, "deleted meanwhile; removing the compute");
        if let Err(e) = self.computes.delete(&target.ep).await {
            warn!(endpoint_id = %target.ep, "reconcile removes this compute: {e:#}");
        }
        Ok(true)
    }

    /// Recreates the compute unless a delete began (false then: stop, the delete job owns the
    /// rest). A failed recreate is logged, and reconcile recreates it.
    async fn restart_unless_deleted(&self, target: &Target) -> Result<bool, String> {
        let current = self
            .metadata
            .get_branch(target.id)
            .await
            .map_err(|e| e.to_string())?;
        if deleted(current.as_ref()) {
            info!(branch_id = %target.id, "deleted meanwhile; not restarting the compute");
            return Ok(false);
        }
        if let Err(err) = self.recreate_compute(target.id, &target.ep).await {
            warn!(endpoint_id = %target.ep, "restarting the compute failed; reconcile retries: {err}");
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use chelabase_metadata::{BranchKind, BranchRecord, BranchStatus, NewBranch};
    use chelabase_neon::{Lsn, NeonId};
    use chelabase_types::DatabaseId;

    use super::*;
    use crate::fakes::{
        FRESH_HEAD, FakeWorker, Journal, STORAGE_CAUSE, entries, pending_branch, position,
        ready_branch, ready_database, restored_by_hand, worker,
    };
    use crate::provision::sha256_hex;

    const BACKUP_NAME: &str = "dev-before-restore-20260930T120000Z";

    fn pos(journal: &Journal, prefix: &str) -> usize {
        position(journal, prefix)
            .unwrap_or_else(|| panic!("{prefix:?} not in journal {:?}", entries(journal)))
    }

    /// Index of the last journal entry starting with `prefix`.
    fn last(journal: &Journal, prefix: &str) -> usize {
        entries(journal)
            .iter()
            .rposition(|e| e.starts_with(prefix))
            .unwrap_or_else(|| panic!("{prefix:?} not in journal {:?}", entries(journal)))
    }

    fn has(journal: &Journal, prefix: &str) -> bool {
        position(journal, prefix).is_some()
    }

    fn count(journal: &Journal, prefix: &str) -> usize {
        entries(journal)
            .iter()
            .filter(|e| e.starts_with(prefix))
            .count()
    }

    /// The id of the timeline the flow created (the first `create_branch_timeline`).
    fn created_timeline(journal: &Journal) -> String {
        let entry = &entries(journal)[pos(journal, "storage.create_branch_timeline ")];
        entry
            .strip_prefix("storage.create_branch_timeline ")
            .and_then(|rest| rest.split(' ').next())
            .unwrap()
            .to_string()
    }

    /// A ready database, and a ready branch `dev` of main; the journal starts empty.
    struct Setup {
        db: DatabaseId,
        main: BranchId,
        dev: BranchId,
        tenant: String,
        main_timeline: String,
        main_ep: String,
        dev_timeline: String,
        dev_ep: String,
    }

    async fn setup(w: &FakeWorker, journal: &Journal) -> Setup {
        let db = ready_database(w).await;
        let main = w.metadata.main(db);
        let dev = ready_branch(w, db, "dev", main.id).await;
        let dev_row = w.metadata.branch(dev);
        journal.lock().unwrap().clear();
        Setup {
            db,
            main: main.id,
            dev,
            tenant: w.metadata.database(db).neon_tenant_id.unwrap(),
            main_timeline: main.timeline_id.unwrap(),
            main_ep: main.endpoint_id.unwrap(),
            dev_timeline: dev_row.timeline_id.unwrap(),
            dev_ep: dev_row.endpoint_id.unwrap(),
        }
    }

    /// What the API does before publishing: ready → resetting.
    fn claim(w: &FakeWorker, id: BranchId) {
        let mut state = w.metadata.state();
        let b = state.branches.get_mut(&id).unwrap();
        assert_eq!(b.status, BranchStatus::Ready);
        b.status = BranchStatus::Resetting;
    }

    fn set_head(w: &FakeWorker, tenant: &str, timeline: &str, head: Lsn) {
        let key = (tenant.parse().unwrap(), timeline.parse().unwrap());
        w.storage
            .timelines
            .lock()
            .unwrap()
            .get_mut(&key)
            .unwrap()
            .head = head;
    }

    fn endpoint_timeline(w: &FakeWorker, ep: &str) -> String {
        w.metadata.state().endpoints[ep]
            .endpoint
            .timeline_id
            .clone()
    }

    fn token_hash(w: &FakeWorker, ep: &str) -> String {
        w.metadata.state().endpoints[ep]
            .endpoint
            .compute_token_sha256
            .clone()
    }

    fn backups_of(w: &FakeWorker, id: BranchId) -> Vec<BranchRecord> {
        let state = w.metadata.state();
        state
            .branches
            .values()
            .filter(|b| b.parent_branch_id == Some(id) && b.kind == BranchKind::Backup)
            .cloned()
            .collect()
    }

    fn at_lsn(lsn: &str) -> PointInTime {
        PointInTime::Lsn(lsn.to_string())
    }

    async fn reset(w: &FakeWorker, id: BranchId) -> Result<(), String> {
        w.handle_branch_reset_to_parent(id).await
    }

    async fn restore(w: &FakeWorker, id: BranchId, at: PointInTime) -> Result<(), String> {
        w.handle_branch_restore(id, BranchId::new(), BACKUP_NAME, at)
            .await
    }

    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Flow {
        Reset,
        Restore,
    }

    async fn run(w: &FakeWorker, flow: Flow, id: BranchId) -> Result<(), String> {
        match flow {
            Flow::Reset => reset(w, id).await,
            Flow::Restore => restore(w, id, at_lsn("0/2000000")).await,
        }
    }

    // --- Journal order ---------------------------------------------------------------

    #[tokio::test]
    async fn reset_to_parent_journal_order() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        // Main moved on since dev was made; the reset takes main's head.
        set_head(&w, &s.tenant, &s.main_timeline, Lsn(0x0300_0000));
        w.metadata.state().branches.get_mut(&s.dev).unwrap().error = Some("old".into());
        let old_hash = token_hash(&w, &s.dev_ep);
        claim(&w, s.dev);

        reset(&w, s.dev).await.unwrap();

        let new = created_timeline(&journal);
        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Ready, "{:?}", dev.error);
        assert_eq!(dev.error, None, "a success clears the old error");
        assert_eq!(dev.timeline_id.as_deref(), Some(new.as_str()));
        assert_eq!(dev.pending_timeline_id, None);
        assert_eq!(dev.superseded_timeline_id, None);
        // The reset's branch point is recorded with the switch (a merge's base location).
        assert_eq!(dev.branched_at_lsn.as_deref(), Some("0/3000000"));
        assert_eq!(endpoint_timeline(&w, &s.dev_ep), new);
        let main_tl: NeonId = s.main_timeline.parse().unwrap();
        assert_eq!(
            w.storage.timeline(&s.tenant, &new).unwrap().ancestor,
            Some((main_tl, Lsn(0x0300_0000)))
        );
        assert!(w.storage.timeline(&s.tenant, &s.dev_timeline).is_none());
        let token = w.computes.token_of(&s.dev_ep).unwrap();
        assert_eq!(sha256_hex(&token), token_hash(&w, &s.dev_ep));
        assert_ne!(token_hash(&w, &s.dev_ep), old_hash);

        let order = [
            pos(&journal, "metadata.set_pending_timeline"),
            pos(
                &journal,
                &format!(
                    "storage.create_branch_timeline {new} from {} at 0/3000000",
                    s.main_timeline
                ),
            ),
            pos(&journal, &format!("compute.delete {}", s.dev_ep)),
            pos(&journal, "metadata.switch_to_pending_timeline"),
            pos(&journal, "metadata.replace_compute_token"),
            pos(&journal, &format!("compute.create {}", s.dev_ep)),
            pos(&journal, &format!("compute.sql {} app SELECT 1", s.dev_ep)),
            pos(
                &journal,
                &format!("storage.delete_timeline {}", s.dev_timeline),
            ),
            pos(&journal, "metadata.clear_superseded_timeline"),
            last(&journal, "metadata.set_branch_status"),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
        // Main is untouched.
        assert!(!has(&journal, &format!("compute.delete {}", s.main_ep)));
        assert!(w.storage.timeline(&s.tenant, &s.main_timeline).is_some());
    }

    // E26: a reset or restore brings a catalog whose OID counter diverged at the branch
    // point. The resync at the end of the switch judges the keys on the new timeline: a key
    // whose OID another role holds there is revoked (never moved to that role), one whose OID
    // still carries its name is kept and stamped with the new timeline.
    #[tokio::test]
    async fn a_switch_judges_secret_keys_on_the_new_timeline() {
        use chelabase_metadata::KeyRoleFix;
        for flow in [Flow::Reset, Flow::Restore] {
            let (w, journal) = worker();
            let s = setup(&w, &journal).await;
            let key = |role: &str, oid: i64| chelabase_metadata::NewDataKey {
                id: uuid::Uuid::new_v4(),
                branch_id: s.dev,
                kind: chelabase_types::data_key::DataKeyKind::Secret,
                role: role.into(),
                name: format!("{role}-key"),
                prefix: "chk_sec_xxxx".into(),
                sha256: [0; 32],
                sealed: None,
                databases: None,
                role_oid: Some(oid),
                role_timeline: Some(s.dev_timeline.clone()),
                role_bypass_rls: false,
            };
            let (app, kept) = (key("app", 16390), key("keep", 16384));
            w.metadata.state().data_keys.extend([app.clone(), kept.clone()]);
            // The new timeline's catalog: `x` holds the OID `app` had.
            w.computes.answer(
                "chelabase: key roles",
                &format!("16384 {}\n16390 {}\n", hex::encode("keep"), hex::encode("x")),
            );
            claim(&w, s.dev);

            run(&w, flow, s.dev).await.unwrap();

            let dev = w.metadata.branch(s.dev);
            assert_eq!(dev.status, BranchStatus::Ready, "{flow:?}: {:?}", dev.error);
            let new = dev.timeline_id.unwrap();
            assert_ne!(new, s.dev_timeline, "{flow:?}");
            let fixes: Vec<KeyRoleFix> = w
                .metadata
                .state()
                .key_fixes
                .iter()
                .filter(|(b, _)| *b == s.dev)
                .flat_map(|(_, f)| f.clone())
                .collect();
            assert_eq!(
                fixes,
                vec![
                    KeyRoleFix::Revoke { id: app.id, role: "app".into(), oid: Some(16390) },
                    KeyRoleFix::Restamp {
                        id: kept.id,
                        role: "keep".into(),
                        oid: 16384,
                        from: Some(s.dev_timeline.clone()),
                        timeline: new,
                    },
                ],
                "{flow:?}"
            );
            assert!(
                !fixes.iter().any(|f| matches!(f, KeyRoleFix::Follow { .. })),
                "{flow:?}: never followed across a switch"
            );
        }
    }

    // T8 C2: a reset or restore replaces the catalog, member roles and their passwords
    // included. The branch's member-role rows go before the compute serves on the new timeline
    // (so the next act-as reconciles and every role gets a fresh password), and a reconcile is
    // asked for once the branch is ready.
    #[tokio::test]
    async fn a_switch_forgets_member_roles_and_asks_for_a_reconcile() {
        for flow in [Flow::Reset, Flow::Restore] {
            let (w, journal) = worker();
            let s = setup(&w, &journal).await;
            for (branch, name) in [(s.dev, "cb_m_dev"), (s.main, "cb_m_main")] {
                w.metadata.state().member_roles.push(chelabase_metadata::members::MemberBranchRole {
                    member_id: uuid::Uuid::new_v4(),
                    branch_id: branch.as_uuid(),
                    role_name: name.into(),
                    password_enc: "sealed".into(),
                    applied_roles: vec!["billing".into()],
                });
            }
            claim(&w, s.dev);
            let asked = w.member_role_requests.sent().len();

            run(&w, flow, s.dev).await.unwrap();

            assert_eq!(w.metadata.branch(s.dev).status, BranchStatus::Ready, "{flow:?}");
            let rows = w.metadata.state().member_roles.clone();
            assert_eq!(rows.len(), 1, "{flow:?}: only the other branch's row stays");
            assert_eq!(rows[0].branch_id, s.main.as_uuid());
            let sent = w.member_role_requests.sent();
            assert_eq!(sent.len(), asked + 1, "{flow:?}");
            assert_eq!(sent.last(), Some(&s.dev));
            let order = [
                pos(&journal, "metadata.delete_branch_member_roles"),
                pos(&journal, &format!("compute.create {}", s.dev_ep)),
            ];
            assert!(order.is_sorted(), "{flow:?}: {:?}", entries(&journal));
        }
    }

    #[tokio::test]
    async fn restore_journal_order() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        claim(&w, s.dev);
        let backup = BranchId::new();

        w.handle_branch_restore(s.dev, backup, BACKUP_NAME, at_lsn("0/2000000"))
            .await
            .unwrap();

        let new = created_timeline(&journal);
        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Ready, "{:?}", dev.error);
        assert_eq!(dev.timeline_id.as_deref(), Some(new.as_str()));
        assert_eq!(dev.pending_timeline_id, None);
        assert_eq!(dev.superseded_timeline_id, None);
        assert_eq!(endpoint_timeline(&w, &s.dev_ep), new);
        // Detached: the restored timeline no longer depends on the old one.
        assert_eq!(w.storage.timeline(&s.tenant, &new).unwrap().ancestor, None);
        // The old timeline lives on in the backup.
        assert!(w.storage.timeline(&s.tenant, &s.dev_timeline).is_some());
        assert_eq!(
            w.metadata.branch(backup).timeline_id.as_deref(),
            Some(s.dev_timeline.as_str())
        );

        let order = [
            pos(&journal, "metadata.set_pending_timeline"),
            pos(
                &journal,
                &format!(
                    "storage.create_branch_timeline {new} from {} at 0/2000000",
                    s.dev_timeline
                ),
            ),
            pos(&journal, &format!("storage.detach_ancestor {new}")),
            pos(&journal, &format!("compute.delete {}", s.dev_ep)),
            pos(&journal, "metadata.switch_to_restored_timeline"),
            pos(&journal, "metadata.replace_compute_token"),
            pos(&journal, &format!("compute.create {}", s.dev_ep)),
            pos(&journal, &format!("compute.sql {} app SELECT 1", s.dev_ep)),
            last(&journal, "metadata.set_branch_status"),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
        assert!(!has(&journal, "storage.delete_timeline"));
    }

    /// C3: the entry for `ep`'s guard clear comes right after a `lock_endpoint`, and the
    /// resync's first read of the copies follows it.
    fn assert_cleared_before_resync(journal: &Journal, ep: &str) {
        let all = entries(journal);
        let clear = pos(journal, &format!("metadata.clear_forwarded {ep}"));
        assert!(clear > 0, "{all:?}");
        assert_eq!(all[clear - 1], "metadata.lock_endpoint", "{all:?}");
        let resync = all[clear..]
            .iter()
            .position(|e| *e == format!("metadata.role_copies {ep}"))
            .unwrap_or_else(|| panic!("no resync after the clear: {all:?}"));
        assert!(resync > 0, "{all:?}");
        assert_eq!(
            all.iter()
                .filter(|e| e.starts_with("metadata.clear_forwarded"))
                .count(),
            1,
            "{all:?}"
        );
    }

    // C3: a reset or restore replaced the catalog, so the forwarded guards are lifted under
    // the endpoint's lock before the resync reads it.
    #[tokio::test]
    async fn restore_clears_the_forwarded_guard_before_resync() {
        for flow in [Flow::Restore, Flow::Reset] {
            let (w, journal) = worker();
            let s = setup(&w, &journal).await;
            claim(&w, s.dev);

            run(&w, flow, s.dev).await.unwrap();

            let dev = w.metadata.branch(s.dev);
            assert_eq!(dev.status, BranchStatus::Ready, "{flow:?}: {:?}", dev.error);
            assert_cleared_before_resync(&journal, &s.dev_ep);
        }
    }

    // Restarting the compute on the old timeline (a failure before the switch) keeps the
    // guards: the catalog wasn't replaced.
    #[tokio::test]
    async fn a_restart_before_the_switch_keeps_the_guard() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        claim(&w, s.dev);

        w.recreate_compute(s.dev, &s.dev_ep).await.unwrap();

        assert!(position(&journal, "metadata.role_copies").is_some());
        assert!(
            position(&journal, "metadata.clear_forwarded").is_none(),
            "{:?}",
            entries(&journal)
        );
    }

    fn created_all_at(w: &FakeWorker, at: &str) {
        let at = at.parse().unwrap();
        for b in w.metadata.state().branches.values_mut() {
            b.created_at = at;
        }
    }

    fn past_at(w: &FakeWorker, lsn: Lsn) {
        *w.storage.lsn_at.lock().unwrap() = Some(chelabase_neon::storcon::LsnAtTime {
            lsn,
            kind: chelabase_neon::storcon::LsnKind::Past,
        });
    }

    #[tokio::test]
    async fn restore_to_before_the_branchs_history_says_to_branch_from_its_parent() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        created_all_at(&w, "2026-09-29T08:00:00Z");
        w.metadata
            .state()
            .branches
            .get_mut(&s.dev)
            .unwrap()
            .history_start = None;
        // dev's timeline starts at FRESH_HEAD on main's: nothing of its own is older.
        past_at(&w, FRESH_HEAD);
        claim(&w, s.dev);

        let at = PointInTime::Timestamp("2026-09-28T11:00:00Z".parse().unwrap());
        restore(&w, s.dev, at).await.unwrap();

        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Ready);
        assert_eq!(dev.timeline_id.as_deref(), Some(s.dev_timeline.as_str()));
        assert_eq!(
            dev.error.as_deref(),
            Some(
                "resolving the point: before dev's history begins (2026-09-29T08:00:00Z); \
                 branch from main instead"
            )
        );
    }

    #[tokio::test]
    async fn restore_outside_the_window_names_the_oldest_point_even_without_a_commit() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        created_all_at(&w, "2026-09-29T08:00:00Z");
        // Main has no ancestor; its oldest LSN (the bootstrap one) has no timestamp.
        past_at(&w, Lsn(0x0100_0000));
        claim(&w, s.dev);
        // dev's ancestor LSN isn't the oldest here: it's the window, not dev's start.
        let at = PointInTime::Timestamp("2026-09-28T11:00:00Z".parse().unwrap());
        restore(&w, s.dev, at).await.unwrap();
        assert_eq!(
            w.metadata.branch(s.dev).error.as_deref(),
            Some(
                "resolving the point: outside the history window; the oldest available point \
                 is 2026-09-29T08:00:00Z"
            )
        );
    }

    #[tokio::test]
    async fn a_restore_hands_the_old_history_to_the_backup_and_keeps_it_on_the_branch() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        let old: chrono::DateTime<chrono::Utc> = "2026-09-20T09:00:00Z".parse().unwrap();
        w.metadata
            .state()
            .branches
            .get_mut(&s.dev)
            .unwrap()
            .history_start = Some(old);
        let commit: chrono::DateTime<chrono::Utc> = "2026-09-29T10:00:00Z".parse().unwrap();
        *w.storage.timestamp_at.lock().unwrap() = Some(commit);
        claim(&w, s.dev);
        let backup = BranchId::new();

        w.handle_branch_restore(s.dev, backup, BACKUP_NAME, at_lsn("0/2000000"))
            .await
            .unwrap();

        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Ready, "{:?}", dev.error);
        assert_eq!(w.metadata.branch(backup).history_start, Some(old));
        // Neon copies the old layers into the restored timeline, so points before the restore
        // point stay reachable (checked live).
        assert_eq!(dev.history_start, Some(old));
    }

    #[tokio::test]
    async fn a_restore_resuming_a_detach_keeps_the_branchs_history_start() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        let old: chrono::DateTime<chrono::Utc> = "2026-09-20T09:00:00Z".parse().unwrap();
        w.metadata
            .state()
            .branches
            .get_mut(&s.dev)
            .unwrap()
            .history_start = Some(old);
        claim(&w, s.dev);
        *w.storage.detach.lock().unwrap() = crate::fakes::FakeDetach::TimesOut;
        restore(&w, s.dev, at_lsn("0/2000000")).await.unwrap_err();
        *w.storage.detach.lock().unwrap() = crate::fakes::FakeDetach::Works;
        let commit: chrono::DateTime<chrono::Utc> = "2026-09-29T10:00:00Z".parse().unwrap();
        *w.storage.timestamp_at.lock().unwrap() = Some(commit);

        restore(&w, s.dev, at_lsn("0/2000000")).await.unwrap();

        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Ready, "{:?}", dev.error);
        assert_eq!(dev.history_start, Some(old));
    }

    #[tokio::test]
    async fn a_reset_starts_the_history_at_the_parents_head_or_else_now() {
        for commit in [Some("2026-09-30T10:00:00Z"), None] {
            let (w, journal) = worker();
            let s = setup(&w, &journal).await;
            let commit = commit.map(|c| c.parse::<chrono::DateTime<chrono::Utc>>().unwrap());
            *w.storage.timestamp_at.lock().unwrap() = commit;
            created_all_at(&w, "2026-09-01T00:00:00Z");
            claim(&w, s.dev);
            let before = chrono::Utc::now();

            reset(&w, s.dev).await.unwrap();

            let dev = w.metadata.branch(s.dev);
            assert_eq!(dev.status, BranchStatus::Ready, "{:?}", dev.error);
            let start = dev.history_start.expect("set");
            match commit {
                Some(commit) => assert_eq!(start, commit),
                // The new timeline was made just now; created_at is long before it.
                None => assert!(start >= before, "{start}"),
            }
        }
    }

    #[tokio::test]
    async fn restore_keeps_the_endpoint_host_and_password() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        // Main can be restored too (it has only the backup as a child afterwards).
        {
            let mut state = w.metadata.state();
            state.branches.remove(&s.dev);
            state.endpoints.remove(&s.dev_ep);
        }
        let before = w.metadata.main(s.db);
        let (roles, databases, old_hash) = {
            let state = w.metadata.state();
            let ep = &state.endpoints[&s.main_ep];
            (
                ep.roles.clone(),
                ep.databases.clone(),
                ep.endpoint.compute_token_sha256.clone(),
            )
        };
        claim(&w, s.main);

        w.handle_branch_restore(
            s.main,
            BranchId::new(),
            "main-before-restore-20260930T120000Z",
            PointInTime::Timestamp("2026-09-30T11:00:00Z".parse().unwrap()),
        )
        .await
        .unwrap();

        let after = w.metadata.main(s.db);
        assert_eq!(after.status, BranchStatus::Ready, "{:?}", after.error);
        assert_ne!(after.timeline_id, before.timeline_id);
        assert_eq!(after.endpoint_id.as_deref(), Some(s.main_ep.as_str()));
        assert_eq!(after.owner_password_enc, before.owner_password_enc);
        {
            let state = w.metadata.state();
            let ep = &state.endpoints[&s.main_ep];
            assert_eq!(ep.roles, roles, "same verifier, so the same password");
            assert_eq!(ep.databases, databases);
        }
        assert_eq!(w.metadata.endpoint_ids(), std::slice::from_ref(&s.main_ep));
        // A fresh compute token, never the old one.
        let token = w.computes.token_of(&s.main_ep).unwrap();
        assert_eq!(sha256_hex(&token), token_hash(&w, &s.main_ep));
        assert_ne!(token_hash(&w, &s.main_ep), old_hash);
        assert!(w.computes.list().await.unwrap().contains(&s.main_ep));
    }

    #[tokio::test]
    async fn restore_creates_a_backup_that_does_not_count() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        claim(&w, s.dev);
        let backup = BranchId::new();

        w.handle_branch_restore(s.dev, backup, BACKUP_NAME, at_lsn("0/2000000"))
            .await
            .unwrap();

        let b = w.metadata.branch(backup);
        assert_eq!(b.kind, BranchKind::Backup);
        assert_eq!(b.status, BranchStatus::Ready);
        assert_eq!(b.name, BACKUP_NAME);
        assert_eq!(b.parent_branch_id, Some(s.dev));
        assert_eq!(b.database_id, s.db);
        assert_eq!(b.timeline_id.as_deref(), Some(s.dev_timeline.as_str()));
        assert_eq!(b.endpoint_id, None, "a backup has no compute");
        assert_eq!(w.metadata.endpoint_ids().len(), 2);
        // Not a child: dev can be restored again.
        assert!(w.metadata.live_children(s.dev).await.unwrap().is_empty());
        claim(&w, s.dev);
        w.handle_branch_restore(
            s.dev,
            BranchId::new(),
            "dev-before-restore-20260930T120500Z",
            at_lsn("0/2000000"),
        )
        .await
        .unwrap();
        assert_eq!(w.metadata.branch(s.dev).status, BranchStatus::Ready);
        assert_eq!(backups_of(&w, s.dev).len(), 2);
        // Not counted against the cap: dev alone fills max_branches = 2 with one more.
        w.metadata
            .state()
            .databases
            .get_mut(&s.db)
            .unwrap()
            .max_branches = 2;
        let other = BranchId::new();
        assert_eq!(
            w.metadata.create_branch(&NewBranch {
                id: other,
                database_id: s.db,
                name: "other",
                parent_branch_id: s.main,
                branched_at_time: None,
                branched_at_lsn: None,
            }),
            chelabase_metadata::CreateBranchOutcome::Created
        );
    }

    // --- Refusals ----------------------------------------------------------------------

    #[tokio::test]
    async fn reset_refused_for_main_backup_and_with_children() {
        // Main has no parent.
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        claim(&w, s.main);
        reset(&w, s.main).await.unwrap();
        let main = w.metadata.main(s.db);
        assert_eq!(main.status, BranchStatus::Ready);
        assert_eq!(
            main.error.as_deref(),
            Some("main has no parent to reset to; use restore to rewind it")
        );
        assert!(!has(&journal, "storage."), "{:?}", entries(&journal));
        assert!(!has(&journal, "compute."), "{:?}", entries(&journal));

        // A backup has no compute to reset or restore: refused, back to ready (were one ever
        // claimed).
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        let backup = restored_by_hand(&w, s.dev, BACKUP_NAME);
        journal.lock().unwrap().clear();
        claim(&w, backup);
        reset(&w, backup).await.unwrap();
        assert_eq!(w.metadata.branch(backup).status, BranchStatus::Ready);
        claim(&w, backup);
        restore(&w, backup, at_lsn("0/2000000")).await.unwrap();
        let b = w.metadata.branch(backup);
        assert_eq!(b.status, BranchStatus::Ready);
        assert_eq!(
            b.error.as_deref(),
            Some("this is a restore backup with no compute; create a branch from it to connect")
        );
        assert!(!has(&journal, "storage."), "{:?}", entries(&journal));
        assert!(!has(&journal, "compute."), "{:?}", entries(&journal));

        // Live children block both, re-checked in the worker.
        for (flow, verb) in [(Flow::Reset, "reset"), (Flow::Restore, "restore")] {
            let (w, journal) = worker();
            let s = setup(&w, &journal).await;
            ready_branch(&w, s.db, "feature", s.dev).await;
            journal.lock().unwrap().clear();
            claim(&w, s.dev);
            run(&w, flow, s.dev).await.unwrap();
            let dev = w.metadata.branch(s.dev);
            assert_eq!(dev.status, BranchStatus::Ready, "{flow:?}");
            assert_eq!(
                dev.error.as_deref(),
                Some(
                    format!(
                        "cannot {verb} dev: it has child branches (feature); delete them first"
                    )
                    .as_str()
                ),
                "{flow:?}"
            );
            assert_eq!(dev.timeline_id.as_deref(), Some(s.dev_timeline.as_str()));
            assert!(
                !has(&journal, "storage."),
                "{flow:?}: {:?}",
                entries(&journal)
            );
            assert!(
                !has(&journal, "compute."),
                "{flow:?}: {:?}",
                entries(&journal)
            );
            assert!(!has(&journal, "metadata.set_pending_timeline"), "{flow:?}");
        }
    }

    // --- Failures ----------------------------------------------------------------------

    type Arrange = fn(&FakeWorker, &Setup);

    #[tokio::test]
    async fn a_failure_before_the_switch_returns_to_ready_on_the_old_timeline() {
        /// (flow, step, arrange, at for a restore, the error's prefix, compute stopped first)
        type Case = (
            Flow,
            &'static str,
            Arrange,
            &'static str,
            &'static str,
            bool,
        );
        let cases: [Case; 8] = [
            (
                Flow::Reset,
                "resolve",
                |w, _| *w.storage.fail.lock().unwrap() = Some("get_timeline"),
                "",
                "resolving the point: injected get_timeline failure: connection refused",
                false,
            ),
            (
                Flow::Reset,
                "create timeline",
                |w, _| *w.storage.fail.lock().unwrap() = Some("create_branch_timeline"),
                "",
                "creating timeline: ",
                false,
            ),
            (
                Flow::Reset,
                "remove compute",
                |w, _| *w.computes.fail_delete.lock().unwrap() = Some("docker down".into()),
                "",
                "removing compute: docker down",
                false,
            ),
            (
                Flow::Restore,
                "resolve",
                |_, _| {},
                "0/2000001",
                "resolving the point: the LSN 0/2000001 is ahead of the branch (at 0/2000000)",
                false,
            ),
            (
                Flow::Restore,
                "create timeline",
                |w, _| *w.storage.fail.lock().unwrap() = Some("create_branch_timeline"),
                "0/2000000",
                "creating timeline: ",
                false,
            ),
            (
                // Refused: nothing is running on it. (No answer is retried; see branch_job.)
                Flow::Restore,
                "detach",
                |w, _| *w.storage.reject.lock().unwrap() = Some("detach_ancestor"),
                "0/2000000",
                "detaching: storage controller rejected the request (400",
                false,
            ),
            (
                Flow::Restore,
                "remove compute",
                |w, _| *w.computes.fail_delete.lock().unwrap() = Some("docker down".into()),
                "0/2000000",
                "removing compute: docker down",
                false,
            ),
            (
                Flow::Restore,
                "backup name taken",
                |w, s| {
                    pending_branch(w, s.db, BACKUP_NAME, s.main);
                },
                "0/2000000",
                "switching timeline: a live branch named \"dev-before-restore-20260930T120000Z\" \
                 already exists",
                true,
            ),
        ];
        for (flow, step, arrange, at, message, stopped) in cases {
            let case = format!("{flow:?} {step}");
            let (w, journal) = worker();
            let s = setup(&w, &journal).await;
            let timelines = w.storage.timeline_ids();
            arrange(&w, &s);
            claim(&w, s.dev);

            match flow {
                Flow::Reset => reset(&w, s.dev).await.unwrap(),
                Flow::Restore => restore(&w, s.dev, at_lsn(at)).await.unwrap(),
            }

            let dev = w.metadata.branch(s.dev);
            assert_eq!(dev.status, BranchStatus::Ready, "{case}");
            let error = dev.error.clone().unwrap_or_default();
            assert!(error.starts_with(message), "{case}: {error:?}");
            if message.ends_with(": ") {
                assert!(error.ends_with(STORAGE_CAUSE), "{case}: {error:?}");
            }
            // Still on the old timeline, with nothing half-made left.
            assert_eq!(
                dev.timeline_id.as_deref(),
                Some(s.dev_timeline.as_str()),
                "{case}"
            );
            assert_eq!(dev.pending_timeline_id, None, "{case}");
            assert_eq!(dev.superseded_timeline_id, None, "{case}");
            assert_eq!(endpoint_timeline(&w, &s.dev_ep), s.dev_timeline, "{case}");
            assert_eq!(w.storage.timeline_ids(), timelines, "{case}");
            assert!(backups_of(&w, s.dev).is_empty(), "{case}");
            // The compute runs, recreated if it had been stopped.
            assert!(
                w.computes.list().await.unwrap().contains(&s.dev_ep),
                "{case}"
            );
            if stopped {
                assert!(
                    pos(&journal, &format!("compute.delete {}", s.dev_ep))
                        < last(&journal, &format!("compute.create {}", s.dev_ep)),
                    "{case}: {:?}",
                    entries(&journal)
                );
                let token = w.computes.token_of(&s.dev_ep).unwrap();
                assert_eq!(sha256_hex(&token), token_hash(&w, &s.dev_ep), "{case}");
            } else {
                assert!(!has(&journal, "compute.create"), "{case}");
            }
            assert!(!has(&journal, "metadata.switch_to_pending"), "{case}");
            assert!(w.storage.timeline(&s.tenant, &s.main_timeline).is_some());
        }
    }

    // Review Focus 2: past the switch, the branch stays on the new timeline, `resetting`, and
    // reconcile finishes the flow from what the row records.
    #[tokio::test]
    async fn a_failure_after_the_switch_leaves_resetting_for_reconcile() {
        type Case = (Flow, &'static str, Arrange, &'static str);
        let cases: [Case; 5] = [
            (
                Flow::Reset,
                "compute create",
                |w, _| *w.computes.fail_create.lock().unwrap() = Some("no docker".into()),
                "starting compute: no docker",
            ),
            (
                Flow::Reset,
                "readiness",
                |w, _| *w.computes.sql_failures_left.lock().unwrap() = u32::MAX,
                "waiting for the compute: ",
            ),
            (
                Flow::Reset,
                "delete superseded",
                |w, _| *w.storage.fail.lock().unwrap() = Some("delete_timeline"),
                "deleting timeline: ",
            ),
            (
                Flow::Restore,
                "compute create",
                |w, _| *w.computes.fail_create.lock().unwrap() = Some("no docker".into()),
                "starting compute: no docker",
            ),
            (
                Flow::Restore,
                "readiness",
                |w, _| *w.computes.sql_failures_left.lock().unwrap() = u32::MAX,
                "waiting for the compute: ",
            ),
        ];
        for (flow, step, arrange, message) in cases {
            let case = format!("{flow:?} {step}");
            let (w, journal) = worker();
            let s = setup(&w, &journal).await;
            arrange(&w, &s);
            claim(&w, s.dev);

            run(&w, flow, s.dev).await.unwrap();

            let new = created_timeline(&journal);
            let dev = w.metadata.branch(s.dev);
            assert_eq!(dev.status, BranchStatus::Resetting, "{case}");
            assert!(
                dev.error.as_deref().unwrap_or("").starts_with(message),
                "{case}: {:?}",
                dev.error
            );
            // On the new timeline, which exists: never a missing one.
            assert_eq!(dev.timeline_id.as_deref(), Some(new.as_str()), "{case}");
            assert!(w.storage.timeline(&s.tenant, &new).is_some(), "{case}");
            assert_eq!(endpoint_timeline(&w, &s.dev_ep), new, "{case}");
            assert_eq!(dev.pending_timeline_id, None, "{case}");
            // The old timeline is recorded for reconcile (reset), or kept by the backup.
            assert!(
                w.storage.timeline(&s.tenant, &s.dev_timeline).is_some(),
                "{case}"
            );
            match flow {
                Flow::Reset => assert_eq!(
                    dev.superseded_timeline_id.as_deref(),
                    Some(s.dev_timeline.as_str()),
                    "{case}"
                ),
                Flow::Restore => {
                    assert_eq!(dev.superseded_timeline_id, None, "{case}");
                    let backups = backups_of(&w, s.dev);
                    assert_eq!(backups.len(), 1, "{case}");
                    assert_eq!(
                        backups[0].timeline_id.as_deref(),
                        Some(s.dev_timeline.as_str()),
                        "{case}"
                    );
                }
            }
            assert_eq!(
                count(&journal, "storage.create_branch_timeline"),
                1,
                "{case}"
            );
        }
    }

    // Review Focus 2: a crash before the switch (the switch's transaction errs) or after it
    // (the compute never comes back). The redelivered job neither leaks a timeline nor makes a
    // second one past the switch.
    #[tokio::test]
    async fn a_redelivered_job_after_a_crash_finishes_without_leaking() {
        for (flow, crash, before_switch) in [
            (Flow::Reset, "switch_to_pending_timeline", true),
            (Flow::Reset, "replace_compute_token", false),
            (Flow::Restore, "switch_to_restored_timeline", true),
            (Flow::Restore, "replace_compute_token", false),
        ] {
            let case = format!("{flow:?} crash at {crash}");
            let (w, journal) = worker();
            let s = setup(&w, &journal).await;
            claim(&w, s.dev);
            let backup = BranchId::new();
            let job = || async {
                match flow {
                    Flow::Reset => reset(&w, s.dev).await,
                    Flow::Restore => {
                        w.handle_branch_restore(s.dev, backup, BACKUP_NAME, at_lsn("0/2000000"))
                            .await
                    }
                }
            };
            w.metadata.state().fail = Some(crash);
            let first = job().await;
            w.metadata.state().fail = None;
            let first_timeline = created_timeline(&journal);
            let dev = w.metadata.branch(s.dev);
            assert_eq!(dev.status, BranchStatus::Resetting, "{case}");
            if before_switch {
                // Retried: the pending timeline is recorded, so it can't leak.
                assert!(first.is_err(), "{case}");
                assert_eq!(
                    dev.pending_timeline_id.as_deref(),
                    Some(first_timeline.as_str())
                );
                assert_eq!(dev.timeline_id.as_deref(), Some(s.dev_timeline.as_str()));
            } else {
                assert_eq!(dev.timeline_id.as_deref(), Some(first_timeline.as_str()));
            }
            assert!(
                w.storage.timeline(&s.tenant, &first_timeline).is_some(),
                "{case}"
            );

            job().await.unwrap();

            let dev = w.metadata.branch(s.dev);
            assert_eq!(dev.status, BranchStatus::Ready, "{case}: {:?}", dev.error);
            assert_eq!(dev.error, None, "{case}");
            assert_eq!(dev.pending_timeline_id, None, "{case}");
            assert_eq!(dev.superseded_timeline_id, None, "{case}");
            let current = dev.timeline_id.clone().unwrap();
            if before_switch {
                assert_ne!(current, first_timeline, "{case}: started again");
                assert!(
                    w.storage.timeline(&s.tenant, &first_timeline).is_none(),
                    "{case}"
                );
            } else {
                assert_eq!(current, first_timeline, "{case}: finished, not redone");
                assert_eq!(
                    count(&journal, "storage.create_branch_timeline"),
                    1,
                    "{case}"
                );
            }
            let mut expected = vec![s.main_timeline.clone(), current.clone()];
            if flow == Flow::Restore {
                expected.push(s.dev_timeline.clone());
                assert_eq!(backups_of(&w, s.dev).len(), 1, "{case}");
            }
            expected.sort();
            assert_eq!(w.storage.timeline_ids(), expected, "{case}");
            assert_eq!(endpoint_timeline(&w, &s.dev_ep), current, "{case}");
            assert!(
                w.computes.list().await.unwrap().contains(&s.dev_ep),
                "{case}"
            );
        }
    }

    // Review Focus 1: a delete lands while the flow runs. What the flow made is removed, and
    // once the delete job has run the branch is deleted with nothing left behind.
    #[tokio::test]
    async fn a_delete_during_reset_or_restore_leaves_no_pending_timeline() {
        #[derive(Debug, Clone, Copy)]
        enum When {
            /// Begun as the timeline is created; the delete job runs afterwards.
            BegunAtTimeline,
            /// The delete job finished before the timeline was created.
            FinishedBeforeTimeline,
            /// Begun after the switch, seen at the re-check after the compute is back.
            BegunAfterSwitch,
        }
        for flow in [Flow::Reset, Flow::Restore] {
            for when in [
                When::BegunAtTimeline,
                When::FinishedBeforeTimeline,
                When::BegunAfterSwitch,
            ] {
                let case = format!("{flow:?} {when:?}");
                let (w, journal) = worker();
                let s = setup(&w, &journal).await;
                claim(&w, s.dev);
                let state = w.metadata.state.clone();
                let dev = s.dev;
                match when {
                    When::BegunAtTimeline => {
                        *w.storage.on_create_branch_timeline.lock().unwrap() =
                            Some(Box::new(move || {
                                let mut state = state.lock().unwrap();
                                state.branches.get_mut(&dev).unwrap().status =
                                    BranchStatus::Deleting;
                            }));
                    }
                    When::FinishedBeforeTimeline => {
                        *w.storage.on_create_branch_timeline.lock().unwrap() =
                            Some(Box::new(move || {
                                // What handle_branch_delete leaves in the metadata.
                                let mut state = state.lock().unwrap();
                                let branch = state.branches.get_mut(&dev).unwrap();
                                let ep = branch.endpoint_id.take().unwrap();
                                branch.status = BranchStatus::Deleted;
                                state.endpoints.remove(&ep);
                            }));
                    }
                    When::BegunAfterSwitch => {
                        // The first get_branch of dev is the job's own read.
                        w.metadata.state().delete_at_get_branch(s.dev, 2);
                    }
                }

                run(&w, flow, s.dev).await.unwrap();

                let pending = created_timeline(&journal);
                match when {
                    When::BegunAtTimeline | When::FinishedBeforeTimeline => {
                        // Never switched, never restarted.
                        assert!(
                            pos(&journal, "metadata.switch_to_")
                                < pos(&journal, &format!("storage.delete_timeline {pending}")),
                            "{case}: {:?}",
                            entries(&journal)
                        );
                        assert!(!has(&journal, "compute.create"), "{case}");
                        assert!(backups_of(&w, s.dev).is_empty(), "{case}");
                        assert_eq!(
                            w.metadata.branch(s.dev).timeline_id.as_deref(),
                            Some(s.dev_timeline.as_str())
                        );
                    }
                    When::BegunAfterSwitch => {
                        // The recreated compute is removed again.
                        assert!(
                            last(&journal, &format!("compute.create {}", s.dev_ep))
                                < last(&journal, &format!("compute.delete {}", s.dev_ep)),
                            "{case}: {:?}",
                            entries(&journal)
                        );
                    }
                }
                assert!(
                    w.storage.timeline(&s.tenant, &pending).is_none()
                        || matches!(when, When::BegunAfterSwitch),
                    "{case}"
                );
                assert!(
                    !w.computes.list().await.unwrap().contains(&s.dev_ep),
                    "{case}"
                );
                assert!(!has(&journal, "metadata.set_branch_error"), "{case}");

                if w.metadata.branch(s.dev).status == BranchStatus::Deleting {
                    w.handle_branch_delete(s.dev).await.unwrap();
                }
                let dev = w.metadata.branch(s.dev);
                assert_eq!(dev.status, BranchStatus::Deleted, "{case}");
                assert!(w.storage.timeline(&s.tenant, &pending).is_none(), "{case}");
                assert_eq!(
                    w.computes.list().await.unwrap(),
                    std::slice::from_ref(&s.main_ep),
                    "{case}"
                );
                assert_eq!(
                    w.metadata.endpoint_ids(),
                    std::slice::from_ref(&s.main_ep),
                    "{case}"
                );
                if !matches!(when, When::FinishedBeforeTimeline) {
                    // (The simulated finished delete leaves the fake storage alone.)
                    assert_eq!(
                        w.storage.timeline_ids(),
                        std::slice::from_ref(&s.main_timeline),
                        "{case}"
                    );
                }
                for b in backups_of(&w, s.dev) {
                    assert_eq!(b.status, BranchStatus::Deleted, "{case}");
                }
            }
        }
    }

    // --- Fix round 1 ------------------------------------------------------------------

    /// The API claims ready → resetting before publishing; a job finding the branch ready is
    /// stale (a redelivery after a success whose ack was lost, or a publish that timed out
    /// after the API released its claim), and does nothing.
    async fn assert_stale_job_does_nothing(flow: Flow) {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        w.metadata.state().branches.get_mut(&s.dev).unwrap().error = Some("old".into());

        run(&w, flow, s.dev).await.unwrap();

        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Ready);
        assert_eq!(dev.error.as_deref(), Some("old"), "the error is untouched");
        assert_eq!(dev.timeline_id.as_deref(), Some(s.dev_timeline.as_str()));
        assert!(entries(&journal).is_empty(), "{:?}", entries(&journal));
    }

    #[tokio::test]
    async fn a_stale_reset_job_on_a_ready_branch_does_nothing() {
        assert_stale_job_does_nothing(Flow::Reset).await;
    }

    #[tokio::test]
    async fn a_stale_restore_job_on_a_ready_branch_does_nothing() {
        assert_stale_job_does_nothing(Flow::Restore).await;
    }

    // Two runs overlap: between this run's create and its switch, another run dropped this
    // run's pending id as a leftover and recorded its own. This run must not switch to the
    // other's timeline, nor delete it; it removes only its own, which nothing records.
    #[tokio::test]
    async fn another_run_replacing_the_pending_timeline_stops_this_one() {
        for flow in [Flow::Reset, Flow::Restore] {
            let (w, journal) = worker();
            let s = setup(&w, &journal).await;
            let theirs = NeonId::new();
            w.storage
                .insert_timeline(&s.tenant, theirs.as_str(), FRESH_HEAD);
            claim(&w, s.dev);
            let state = w.metadata.state.clone();
            let (dev, theirs_id) = (s.dev, theirs.as_str().to_string());
            *w.storage.on_create_branch_timeline.lock().unwrap() = Some(Box::new(move || {
                let mut state = state.lock().unwrap();
                state.branches.get_mut(&dev).unwrap().pending_timeline_id = Some(theirs_id);
            }));

            run(&w, flow, s.dev).await.unwrap();

            let mine = created_timeline(&journal);
            let dev = w.metadata.branch(s.dev);
            assert_eq!(
                dev.status,
                BranchStatus::Resetting,
                "{flow:?}: the other run owns it"
            );
            assert_eq!(
                dev.timeline_id.as_deref(),
                Some(s.dev_timeline.as_str()),
                "{flow:?}"
            );
            assert_eq!(
                dev.pending_timeline_id.as_deref(),
                Some(theirs.as_str()),
                "{flow:?}"
            );
            assert_eq!(endpoint_timeline(&w, &s.dev_ep), s.dev_timeline, "{flow:?}");
            assert!(
                w.storage.timeline(&s.tenant, theirs.as_str()).is_some(),
                "{flow:?}: never delete the other run's timeline"
            );
            assert!(
                w.storage.timeline(&s.tenant, &mine).is_none(),
                "{flow:?}: this run's own, unrecorded timeline doesn't leak"
            );
            assert!(
                !has(&journal, &format!("storage.delete_timeline {theirs}")),
                "{flow:?}"
            );
            assert!(backups_of(&w, s.dev).is_empty(), "{flow:?}");
            assert!(!has(&journal, "metadata.set_branch_error"), "{flow:?}");
            assert!(!has(&journal, "compute.create"), "{flow:?}");
        }
    }

    // Minor 2: an interrupted attempt left a pending timeline that can't be deleted. The branch
    // stays resetting for reconcile, but its compute (maybe stopped by that attempt) runs.
    #[tokio::test]
    async fn a_leftover_pending_that_cannot_be_deleted_restarts_the_compute() {
        for flow in [Flow::Reset, Flow::Restore] {
            let (w, journal) = worker();
            let s = setup(&w, &journal).await;
            let leftover = NeonId::new();
            w.storage
                .insert_timeline(&s.tenant, leftover.as_str(), FRESH_HEAD);
            claim(&w, s.dev);
            w.metadata
                .state()
                .branches
                .get_mut(&s.dev)
                .unwrap()
                .pending_timeline_id = Some(leftover.as_str().to_string());
            // The interrupted attempt had stopped the compute.
            w.computes.remove(&s.dev_ep);
            *w.storage.fail.lock().unwrap() = Some("delete_timeline");

            run(&w, flow, s.dev).await.unwrap();

            let dev = w.metadata.branch(s.dev);
            assert_eq!(dev.status, BranchStatus::Resetting, "{flow:?}");
            assert!(
                dev.error
                    .as_deref()
                    .unwrap_or("")
                    .starts_with("deleting timeline: "),
                "{flow:?}: {:?}",
                dev.error
            );
            assert_eq!(
                dev.pending_timeline_id.as_deref(),
                Some(leftover.as_str()),
                "{flow:?}: still recorded"
            );
            assert!(
                w.computes.list().await.unwrap().contains(&s.dev_ep),
                "{flow:?}: {:?}",
                entries(&journal)
            );
            assert!(!has(&journal, "storage.create_branch_timeline"), "{flow:?}");
        }
    }

    // Ruling 1 (Task 6): a leftover pending timeline is cleared by compare-and-swap before
    // it's deleted.
    #[tokio::test]
    async fn a_leftover_pending_is_cleared_before_it_is_deleted() {
        for flow in [Flow::Reset, Flow::Restore] {
            let (w, journal) = worker();
            let s = setup(&w, &journal).await;
            let leftover = NeonId::new();
            w.storage
                .insert_timeline(&s.tenant, leftover.as_str(), FRESH_HEAD);
            claim(&w, s.dev);
            w.metadata
                .state()
                .branches
                .get_mut(&s.dev)
                .unwrap()
                .pending_timeline_id = Some(leftover.as_str().to_string());

            run(&w, flow, s.dev).await.unwrap();

            assert!(
                pos(&journal, "metadata.swap_pending_timeline")
                    < pos(&journal, &format!("storage.delete_timeline {leftover}")),
                "{flow:?}: {:?}",
                entries(&journal)
            );
            assert!(w.storage.timeline(&s.tenant, leftover.as_str()).is_none());
            assert_eq!(
                w.metadata.branch(s.dev).status,
                BranchStatus::Ready,
                "{flow:?}"
            );
        }
    }

    // Ruling 1 (Task 6): when the swap fails, another run (here a delete) owns the leftover:
    // this run leaves it and stops.
    #[tokio::test]
    async fn a_leftover_pending_that_cannot_be_cleared_is_left_alone() {
        for flow in [Flow::Reset, Flow::Restore] {
            let (w, journal) = worker();
            let s = setup(&w, &journal).await;
            let leftover = NeonId::new();
            w.storage
                .insert_timeline(&s.tenant, leftover.as_str(), FRESH_HEAD);
            claim(&w, s.dev);
            w.metadata
                .state()
                .branches
                .get_mut(&s.dev)
                .unwrap()
                .pending_timeline_id = Some(leftover.as_str().to_string());
            w.metadata.state().delete_before_write = Some(("swap_pending_timeline", s.dev));

            run(&w, flow, s.dev).await.unwrap();

            assert!(
                !has(&journal, &format!("storage.delete_timeline {leftover}")),
                "{flow:?}: {:?}",
                entries(&journal)
            );
            assert!(!has(&journal, "storage.create_branch_timeline"), "{flow:?}");
            let dev = w.metadata.branch(s.dev);
            assert_eq!(dev.status, BranchStatus::Deleting, "{flow:?}");
            assert_eq!(dev.pending_timeline_id.as_deref(), Some(leftover.as_str()));
        }
    }

    // Minor 3: a delete lands after a failure before the switch, just before the compute is
    // restarted: no compute is started for a branch being deleted.
    #[tokio::test]
    async fn a_delete_before_the_restart_starts_no_compute() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        pending_branch(&w, s.db, BACKUP_NAME, s.main);
        claim(&w, s.dev);
        // The first get_branch of dev is the job's own read; the second is the check before
        // the restart.
        w.metadata.state().delete_at_get_branch(s.dev, 2);

        restore(&w, s.dev, at_lsn("0/2000000")).await.unwrap();

        assert!(has(&journal, &format!("compute.delete {}", s.dev_ep)));
        assert!(!has(&journal, "compute.create"), "{:?}", entries(&journal));
        assert!(!w.computes.list().await.unwrap().contains(&s.dev_ep));
        assert_eq!(w.metadata.branch(s.dev).status, BranchStatus::Deleting);
        let pending = created_timeline(&journal);
        assert!(w.storage.timeline(&s.tenant, &pending).is_none());
    }

    // Minor 6: before the switch, the pending timeline can't be deleted. It stays recorded (so
    // reconcile deletes it), the branch stays resetting with the error, and the compute runs.
    #[tokio::test]
    async fn a_pending_timeline_that_cannot_be_deleted_leaves_resetting_with_the_compute_back() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        pending_branch(&w, s.db, BACKUP_NAME, s.main);
        claim(&w, s.dev);
        *w.storage.fail.lock().unwrap() = Some("delete_timeline");

        restore(&w, s.dev, at_lsn("0/2000000")).await.unwrap();

        let pending = created_timeline(&journal);
        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Resetting);
        let error = dev.error.clone().unwrap_or_default();
        assert!(error.starts_with("switching timeline: "), "{error:?}");
        assert!(error.contains("; then deleting timeline: "), "{error:?}");
        assert_eq!(dev.pending_timeline_id.as_deref(), Some(pending.as_str()));
        assert_eq!(dev.timeline_id.as_deref(), Some(s.dev_timeline.as_str()));
        assert_eq!(endpoint_timeline(&w, &s.dev_ep), s.dev_timeline);
        assert!(
            pos(&journal, &format!("compute.delete {}", s.dev_ep))
                < last(&journal, &format!("compute.create {}", s.dev_ep)),
            "{:?}",
            entries(&journal)
        );
        assert!(w.computes.list().await.unwrap().contains(&s.dev_ep));
    }

    // --- recreate_compute --------------------------------------------------------------

    #[tokio::test]
    async fn recreate_compute_writes_the_token_hash_first_then_waits() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        let old = token_hash(&w, &s.dev_ep);
        w.computes.remove(&s.dev_ep);

        w.recreate_compute(s.dev, &s.dev_ep).await.unwrap();

        let order = [
            pos(&journal, "metadata.replace_compute_token"),
            pos(&journal, &format!("compute.create {}", s.dev_ep)),
            pos(&journal, &format!("compute.sql {} app SELECT 1", s.dev_ep)),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
        let token = w.computes.token_of(&s.dev_ep).unwrap();
        assert_eq!(sha256_hex(&token), token_hash(&w, &s.dev_ep));
        assert_ne!(token_hash(&w, &s.dev_ep), old);

        *w.computes.sql_failures_left.lock().unwrap() = u32::MAX;
        let err = w.recreate_compute(s.dev, &s.dev_ep).await.unwrap_err();
        assert!(err.starts_with("waiting for the compute: "), "{err}");
    }

    // PB end-of-step R12: a compute started for a branch (missing, upgraded, after a reset or
    // restore whose catalog may lack the guard) asks for a member-role reconcile, which also
    // brings the protected-branch guard to the current settings.
    #[tokio::test]
    async fn recreate_compute_asks_for_a_member_reconcile() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        w.computes.remove(&s.dev_ep);
        let before = w.member_role_requests.sent().len();
        w.recreate_compute(s.dev, &s.dev_ep).await.unwrap();
        assert_eq!(w.member_role_requests.sent()[before..], [s.dev]);
    }

    #[tokio::test]
    async fn recreate_compute_resyncs_and_records_the_settings_version() {
        const CATALOG_READ: &str = "SELECT 'r',";
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        w.metadata
            .state()
            .endpoints
            .get_mut(&s.dev_ep)
            .unwrap()
            .compute_settings_version = 0;
        w.computes.remove(&s.dev_ep);
        journal.lock().unwrap().clear();

        w.recreate_compute(s.dev, &s.dev_ep).await.unwrap();

        // After the data API bootstrap, under the lock, through the system database.
        let order = [
            pos(&journal, &format!("compute.create {}", s.dev_ep)),
            pos(
                &journal,
                &format!(
                    "compute.sql {} app BEGIN;",
                    s.dev_ep
                ),
            ),
            // Recorded before the resync: a resync that keeps failing must not make every
            // pass re-create the compute.
            pos(&journal, "metadata.set_compute_settings_version"),
            pos(&journal, "metadata.lock_endpoint"),
            pos(
                &journal,
                &format!(
                    "compute.sql {} postgres SET search_path = pg_catalog, pg_temp;",
                    s.dev_ep
                ),
            ),
            pos(&journal, "metadata.apply_resync"),
        ];
        assert!(order.is_sorted(), "{order:?} {:?}", entries(&journal));
        assert_eq!(
            w.metadata.state().endpoints[&s.dev_ep].compute_settings_version,
            crate::COMPUTE_SETTINGS_VERSION
        );

        // A resync that fails fails the step; the version is recorded all the same.
        w.metadata
            .state()
            .endpoints
            .get_mut(&s.dev_ep)
            .unwrap()
            .compute_settings_version = 0;
        w.computes.remove(&s.dev_ep);
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some((CATALOG_READ.into(), "connection reset".into()));
        let err = w.recreate_compute(s.dev, &s.dev_ep).await.unwrap_err();
        assert!(err.starts_with("syncing login roles: "), "{err}");
        assert_eq!(
            w.metadata.state().endpoints[&s.dev_ep].compute_settings_version,
            crate::COMPUTE_SETTINGS_VERSION
        );
    }

    // --- Skips and retries -------------------------------------------------------------

    #[tokio::test]
    async fn branches_not_ready_and_missing_rows_are_skipped() {
        for status in [
            BranchStatus::Pending,
            BranchStatus::Provisioning,
            BranchStatus::Failed,
            BranchStatus::Deleting,
            BranchStatus::Deleted,
        ] {
            for flow in [Flow::Reset, Flow::Restore] {
                let (w, journal) = worker();
                let s = setup(&w, &journal).await;
                w.metadata.state().branches.get_mut(&s.dev).unwrap().status = status;
                run(&w, flow, s.dev).await.unwrap();
                assert_eq!(w.metadata.branch(s.dev).status, status);
                assert!(
                    entries(&journal).is_empty(),
                    "{flow:?} {status:?}: {:?}",
                    entries(&journal)
                );
            }
        }
        let (w, journal) = worker();
        let _s = setup(&w, &journal).await;
        reset(&w, BranchId::new()).await.unwrap();
        restore(&w, BranchId::new(), at_lsn("0/1")).await.unwrap();
        assert!(entries(&journal).is_empty(), "{:?}", entries(&journal));
    }

    // --- A detach whose state can't be read (I-1) --------------------------------------

    /// A restore of `dev` whose detach times out, with the timeline's state unreadable
    /// (`ancestor_lsn` keeps failing), so every delivery hands the job back.
    async fn stuck_restore(w: &FakeWorker, journal: &Journal) -> Setup {
        let s = setup(w, journal).await;
        claim(w, s.dev);
        *w.storage.detach.lock().unwrap() = crate::fakes::FakeDetach::TimesOut;
        *w.storage.fail.lock().unwrap() = Some("ancestor_lsn");
        s
    }

    #[tokio::test]
    async fn each_retry_of_a_stuck_detach_records_its_cause_and_stays_resetting() {
        let (w, journal) = worker();
        let s = stuck_restore(&w, &journal).await;
        for delivery in 1..=3 {
            // Each attempt records its own cause, not a leftover one.
            w.metadata.state().branches.get_mut(&s.dev).unwrap().error = None;
            let err = restore(&w, s.dev, at_lsn("0/2000000"))
                .await
                .expect_err("handed back");
            assert!(err.starts_with("detaching: "), "{delivery}: {err}");
            let dev = w.metadata.branch(s.dev);
            assert_eq!(dev.status, BranchStatus::Resetting, "{delivery}");
            assert_eq!(dev.error.as_deref(), Some(err.as_str()), "{delivery}");
            assert!(dev.pending_timeline_id.is_some(), "{delivery}");
        }
        assert!(!has(&journal, "storage.delete_timeline"));
    }

    #[tokio::test]
    async fn a_detach_stuck_past_the_bound_fails_the_branch_and_deletes_nothing() {
        let (w, journal) = worker();
        let s = stuck_restore(&w, &journal).await;
        restore(&w, s.dev, at_lsn("0/2000000")).await.unwrap_err();
        let pending = w.metadata.branch(s.dev).pending_timeline_id.unwrap();
        // Recorded just inside the bound: still retried.
        let bound = chrono::Duration::seconds(STUCK_DETACH_SECS);
        let recorded = |ago: chrono::Duration| {
            w.metadata
                .state()
                .branches
                .get_mut(&s.dev)
                .unwrap()
                .pending_timeline_at = Some(chrono::Utc::now() - ago);
        };
        recorded(bound - chrono::Duration::seconds(30));
        restore(&w, s.dev, at_lsn("0/2000000")).await.unwrap_err();
        assert_eq!(w.metadata.branch(s.dev).status, BranchStatus::Resetting);

        recorded(bound + chrono::Duration::seconds(1));
        restore(&w, s.dev, at_lsn("0/2000000"))
            .await
            .expect("given up: the job is acked");

        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Failed);
        let error = dev.error.unwrap();
        assert!(error.starts_with("detaching: "), "{error}");
        // The cause: the timeline's state couldn't be read.
        assert!(error.contains(STORAGE_CAUSE), "names the cause: {error}");
        assert!(error.contains(&format!("{pending} was kept")), "{error}");
        assert!(error.ends_with("; delete the branch to remove it"), "{error}");
        // Nothing deleted: the branch keeps its timeline, the pending one is still recorded.
        assert_eq!(dev.timeline_id.as_deref(), Some(s.dev_timeline.as_str()));
        assert_eq!(dev.pending_timeline_id.as_deref(), Some(pending.as_str()));
        assert!(w.storage.timeline(&s.tenant, &pending).is_some());
        assert!(w.storage.timeline(&s.tenant, &s.dev_timeline).is_some());
        assert!(!has(&journal, "storage.delete_timeline"));
        assert!(backups_of(&w, s.dev).is_empty());
        // A later delivery of the same job is stale.
        journal.lock().unwrap().clear();
        restore(&w, s.dev, at_lsn("0/2000000")).await.unwrap();
        assert!(entries(&journal).iter().all(|e| e.starts_with("metadata.")));
    }

    /// T14 M8: a branch's history begins at its branch point. A restore to an earlier LSN (or
    /// a time that resolves to one) would bring back the parent's older state, so it's refused
    /// before anything is made; main has no branch point and no such bound.
    #[tokio::test]
    async fn a_restore_before_the_branch_point_is_refused() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        assert_eq!(w.metadata.branch(s.dev).branched_at_lsn.as_deref(), Some("0/2000000"));
        claim(&w, s.dev);

        restore(&w, s.dev, at_lsn("0/1FFFFFF")).await.unwrap();

        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Ready);
        assert_eq!(
            dev.error.as_deref(),
            Some(
                "resolving the point: the LSN 0/1FFFFFF is before dev's history begins (its \
                 branch point, 0/2000000)"
            )
        );
        assert_eq!(dev.timeline_id.as_deref(), Some(s.dev_timeline.as_str()));
        assert!(!has(&journal, "storage.create_branch_timeline"));

        // At the branch point itself: fine.
        journal.lock().unwrap().clear();
        w.metadata.state().branches.get_mut(&s.dev).unwrap().error = None;
        claim(&w, s.dev);
        restore(&w, s.dev, at_lsn("0/2000000")).await.unwrap();
        let dev = w.metadata.branch(s.dev);
        assert_eq!((dev.status, dev.error), (BranchStatus::Ready, None));
        assert!(has(&journal, "storage.create_branch_timeline"));
    }

    /// T14 M9: a restore's pending timeline still attached to the branch's timeline may have a
    /// detach running on it. Reconcile (the job lost, the branch stale) doesn't delete it
    /// while it's younger than the detach bound: the branch stays resetting. Past the bound
    /// any detach has ended, and it's dropped as before (the branch back on its timeline).
    #[tokio::test]
    async fn reconcile_leaves_a_restores_detaching_timeline_alone() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        claim(&w, s.dev);
        *w.storage.detach.lock().unwrap() = crate::fakes::FakeDetach::TimesOut;
        restore(&w, s.dev, at_lsn("0/2000000")).await.unwrap_err();
        let pending = w.metadata.branch(s.dev).pending_timeline_id.unwrap();
        let stale = |w: &FakeWorker| {
            w.metadata.state().branch_ages.insert(s.dev, STALE_PROVISION_SECS + 1);
        };
        stale(&w);
        journal.lock().unwrap().clear();

        let _ = w.finish_switch(s.dev).await;

        assert!(!has(&journal, "storage.delete_timeline"), "{:?}", entries(&journal));
        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Resetting);
        assert_eq!(dev.pending_timeline_id.as_deref(), Some(pending.as_str()));
        assert!(w.storage.timeline(&s.tenant, &pending).is_some());

        assert!(dev.error.as_deref().is_some_and(|e| e.contains("still attached")), "{:?}", dev.error);

        // Just past the job's bound a detach started at the end may still run: left alone.
        let recorded = |w: &FakeWorker, secs: i64| {
            w.metadata.state().branches.get_mut(&s.dev).unwrap().pending_timeline_at =
                Some(chrono::Utc::now() - chrono::Duration::seconds(secs));
        };
        recorded(&w, STUCK_DETACH_SECS + 1);
        stale(&w);
        let _ = w.finish_switch(s.dev).await;
        assert!(!has(&journal, "storage.delete_timeline"), "{:?}", entries(&journal));
        assert_eq!(w.metadata.branch(s.dev).status, BranchStatus::Resetting);

        // Past it plus one detach's timeout: dropped, the branch back on its own timeline.
        let detach = chelabase_neon::storcon::DETACH_TIMEOUT.as_secs() as i64;
        recorded(&w, STUCK_DETACH_SECS + detach + 1);
        stale(&w);
        w.finish_switch(s.dev).await.unwrap();
        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Ready);
        assert_eq!(dev.pending_timeline_id, None);
        assert_eq!(dev.timeline_id.as_deref(), Some(s.dev_timeline.as_str()));
        assert!(has(&journal, &format!("storage.delete_timeline {pending}")), "{:?}", entries(&journal));
    }

    #[tokio::test]
    async fn metadata_trouble_is_retried() {
        for flow in [Flow::Reset, Flow::Restore] {
            for method in ["get_branch", "set_branch_status", "set_pending_timeline"] {
                let (w, journal) = worker();
                let s = setup(&w, &journal).await;
                claim(&w, s.dev);
                w.metadata.state().fail = Some(method);
                assert!(run(&w, flow, s.dev).await.is_err(), "{flow:?} {method}");
                assert!(
                    !has(&journal, "storage.create_branch_timeline"),
                    "{flow:?} {method}"
                );
            }
        }
    }
}
