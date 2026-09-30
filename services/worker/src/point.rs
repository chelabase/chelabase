//! Resolving a branch point (a time, an LSN, or now) to an LSN on a source timeline.

use cheladb_metadata::BranchKind;
use cheladb_nats::PointInTime;
use cheladb_neon::storcon::LsnKind;
use cheladb_neon::{ComputeBackend, Lsn, NeonId};
use cheladb_types::BranchId;
use chrono::{DateTime, SecondsFormat, Utc};
use tracing::warn;

use crate::Worker;
use crate::branch_job::StepFailure;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// The LSN on `source_timeline` (the timeline of branch `source`) that `at` names; `None`
    /// is the timeline's head. Errors carry the messages a branch fails with, prefixed
    /// `resolving the point:`; a storage controller that didn't answer (or answered 5xx) is a
    /// transient one.
    pub(crate) async fn resolve_point(
        &self,
        tenant: &NeonId,
        source: BranchId,
        source_timeline: &NeonId,
        at: Option<&PointInTime>,
    ) -> Result<Lsn, StepFailure> {
        let resolving =
            |message: String| StepFailure::permanent(format!("resolving the point: {message}"));
        let head = || async {
            self.storage
                .get_timeline(tenant, source_timeline)
                .await
                .map_err(|e| StepFailure::storage("resolving the point", &e))
        };
        match at {
            None => head().await,
            Some(PointInTime::Timestamp(time)) => {
                let found = self
                    .storage
                    .lsn_by_timestamp(tenant, source_timeline, *time)
                    .await
                    .map_err(|e| StepFailure::storage("resolving the point", &e))?;
                match found.kind {
                    LsnKind::Present => Ok(found.lsn),
                    // Later than anything written: the latest state is what's asked for.
                    LsnKind::Future => head().await,
                    LsnKind::Past => Err(resolving(
                        self.too_old(tenant, source, source_timeline, found.lsn)
                            .await,
                    )),
                    LsnKind::Nodata => Err(resolving("no data at that time".to_string())),
                }
            }
            Some(PointInTime::Lsn(text)) => {
                let lsn: Lsn = text.parse().map_err(resolving)?;
                let head = head().await?;
                if lsn > head {
                    return Err(resolving(format!(
                        "the LSN {lsn} is ahead of the branch (at {head})"
                    )));
                }
                Ok(lsn)
            }
        }
    }

    /// Where the history of a timeline branched from `timeline` at `lsn` begins: the commit
    /// time there. `None` when there's no commit at or before `lsn`, or the lookup fails (then
    /// the metadata falls back to when the timeline was made).
    pub(crate) async fn history_start_at(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
        lsn: Lsn,
    ) -> Option<DateTime<Utc>> {
        match self.storage.timestamp_of_lsn(tenant, timeline, lsn).await {
            Ok(at) => at,
            Err(err) => {
                warn!(%timeline, %lsn, "looking up where the history begins: {err:#}");
                None
            }
        }
    }

    /// Why a time before `oldest` (the oldest LSN Neon has for the timeline) can't be used.
    /// When `oldest` is where the timeline starts on its ancestor's, the point is before the
    /// source's history begins: a backup's is only named, a child's comes with its parent to
    /// branch from instead. Otherwise it's outside the history window. The time shown is the
    /// source's `history_start`, else the commit time at `oldest`, else (a tenant younger than
    /// its window has no commit there) the source's creation time. Lookups that fail only make
    /// the message vaguer.
    async fn too_old(
        &self,
        tenant: &NeonId,
        source: BranchId,
        source_timeline: &NeonId,
        oldest: Lsn,
    ) -> String {
        let rfc3339 = |t: DateTime<Utc>| t.to_rfc3339_opts(SecondsFormat::AutoSi, true);
        let branch = match self.metadata.get_branch(source).await {
            Ok(branch) => branch,
            Err(err) => {
                warn!(branch_id = %source, "reading the source branch: {err}");
                None
            }
        };
        let ancestor = match self.storage.ancestor_lsn(tenant, source_timeline).await {
            Ok(ancestor) => ancestor,
            Err(err) => {
                warn!("looking up where the timeline starts: {err:#}");
                None
            }
        };
        let commit = || async {
            match self
                .storage
                .timestamp_of_lsn(tenant, source_timeline, oldest)
                .await
            {
                Ok(at) => at,
                Err(err) => {
                    warn!("looking up the oldest available point: {err:#}");
                    None
                }
            }
        };
        if let Some(branch) = &branch
            && ancestor == Some(oldest)
            && let Some(parent_id) = branch.parent_branch_id
        {
            let begins = match branch.history_start {
                Some(start) => start,
                None => commit().await.unwrap_or(branch.created_at),
            };
            let begins = rfc3339(begins);
            if branch.kind == BranchKind::Backup {
                return format!("before the history of {} begins ({begins})", branch.name);
            }
            let parent = match self.metadata.get_branch(parent_id).await {
                Ok(Some(parent)) => parent.name,
                Ok(None) => "its parent".to_string(),
                Err(err) => {
                    warn!(branch_id = %parent_id, "reading the parent branch: {err}");
                    "its parent".to_string()
                }
            };
            return format!(
                "before {}'s history begins ({begins}); branch from {parent} instead",
                branch.name
            );
        }
        let oldest = commit()
            .await
            .or(branch.map(|b| b.created_at))
            .map_or_else(|| "unknown".to_string(), rfc3339);
        format!("outside the history window; the oldest available point is {oldest}")
    }
}
