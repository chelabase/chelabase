//! Deleting a branch: its backups' and its own timelines, compute and endpoint row.

use chelabase_metadata::{BranchRecord, BranchStatus};
use chelabase_neon::{ComputeBackend, NeonId};
use chelabase_types::{BranchId, DatabaseId};
use tracing::{info, warn};

use crate::Worker;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// Tears down the branch's backups, then the branch, then marks them all deleted. `Err`
    /// (the job is retried, and the rows stay `deleting`) when any of that fails.
    pub async fn handle_branch_delete(&self, branch_id: BranchId) -> Result<(), String> {
        let branch = self
            .metadata
            .get_branch(branch_id)
            .await
            .map_err(|e| e.to_string())?;
        let Some(branch) = branch else {
            warn!(%branch_id, "no branch row; dropping delete job");
            return Ok(());
        };
        if branch.parent_branch_id.is_none() {
            warn!(%branch_id, "main is deleted with its database; dropping branch delete job");
            return Ok(());
        }
        if branch.status == BranchStatus::Deleted {
            return Ok(());
        }
        // Normally the API has claimed it already; also takes along a backup made since.
        let outcome = self
            .metadata
            .begin_branch_delete(branch_id)
            .await
            .map_err(|e| e.to_string())?;
        // Neon can't delete a timeline others branch from: refuse, leaving the branch as it
        // was (the API refuses first; this is the same guard, taken under the database lock).
        if let Some(reason) = outcome.refusal(&branch.name) {
            warn!(%branch_id, "branch delete refused: {reason}");
            return self
                .metadata
                .set_branch_error(branch_id, Some(&reason))
                .await
                .map_err(|e| e.to_string());
        }
        let branch = self
            .metadata
            .get_branch(branch_id)
            .await
            .map_err(|e| e.to_string())?;
        let Some(branch) = branch.filter(|b| b.status == BranchStatus::Deleting) else {
            return Ok(());
        };
        let tenant = self.tenant_of(branch.database_id).await?;
        let backups = self
            .metadata
            .live_backups_of(branch_id)
            .await
            .map_err(|e| e.to_string())?;
        for b in backups.iter().chain([&branch]) {
            self.teardown_branch(b, tenant.as_ref()).await?;
        }
        for b in backups.iter().chain([&branch]) {
            self.metadata
                .mark_branch_deleted(b.id)
                .await
                .map_err(|e| e.to_string())?;
        }
        info!(%branch_id, backups = backups.len(), "branch deleted");
        Ok(())
    }

    /// The Neon tenant every branch of `db` lives in; `None` before provisioning records one.
    pub(crate) async fn tenant_of(&self, db: DatabaseId) -> Result<Option<NeonId>, String> {
        let database = self
            .metadata
            .get_database(db)
            .await
            .map_err(|e| e.to_string())?;
        database
            .and_then(|d| d.neon_tenant_id)
            .map(|t| {
                t.parse::<NeonId>()
                    .map_err(|e| format!("tenant id {t:?}: {e}"))
            })
            .transpose()
    }

    /// Everything `b` has: its compute, its timelines (pending, current, superseded), then
    /// its endpoint row.
    pub(crate) async fn teardown_branch(
        &self,
        b: &BranchRecord,
        tenant: Option<&NeonId>,
    ) -> Result<(), String> {
        // Children before their ancestors: a pending timeline branches from the current one
        // until a restore detaches it, and Neon refuses to delete an ancestor (412).
        let timelines = [
            b.pending_timeline_id.as_deref(),
            b.timeline_id.as_deref(),
            b.superseded_timeline_id.as_deref(),
        ];
        let timelines: Vec<&str> = timelines.into_iter().flatten().collect();
        self.teardown_branch_parts(b.endpoint_id.as_deref(), tenant, &timelines)
            .await
    }

    /// Removes the compute (if there's an endpoint), each timeline (one already gone counts as
    /// deleted), then the endpoint row, stopping at the first failure so what's left is
    /// retried. Never the tenant: the database's other branches live in it.
    pub(crate) async fn teardown_branch_parts(
        &self,
        endpoint_id: Option<&str>,
        tenant: Option<&NeonId>,
        timelines: &[&str],
    ) -> Result<(), String> {
        if let Some(ep) = endpoint_id {
            self.computes
                .delete(ep)
                .await
                .map_err(|e| format!("removing compute: {e:#}"))?;
        }
        match tenant {
            Some(tenant) => {
                for timeline in timelines {
                    let timeline: NeonId = timeline
                        .parse()
                        .map_err(|e| format!("deleting timeline: {timeline:?}: {e}"))?;
                    self.storage
                        .delete_timeline(tenant, &timeline)
                        .await
                        .map_err(|e| format!("deleting timeline: {e:#}"))?;
                }
            }
            // Only a database that never got a tenant, so it has no timelines either.
            None if !timelines.is_empty() => {
                warn!(
                    ?timelines,
                    "no tenant recorded for these timelines; skipping them"
                );
            }
            None => {}
        }
        if let Some(ep) = endpoint_id {
            self.metadata
                .delete_endpoint(ep)
                .await
                .map_err(|e| format!("deleting endpoint: {e}"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chelabase_metadata::{BranchDeleteOutcome, BranchStatus};
    use chelabase_neon::NeonId;
    use chelabase_types::DatabaseId;

    use super::*;
    use crate::fakes::{
        FRESH_HEAD, FakeWorker, Journal, STORAGE_CAUSE, entries, position, ready_branch,
        ready_database, restored_by_hand, worker,
    };

    fn pos(journal: &Journal, prefix: &str) -> usize {
        position(journal, prefix)
            .unwrap_or_else(|| panic!("{prefix:?} not in journal {:?}", entries(journal)))
    }

    fn has(journal: &Journal, prefix: &str) -> bool {
        position(journal, prefix).is_some()
    }

    struct Setup {
        db: DatabaseId,
        dev: BranchId,
        tenant: String,
        main_timeline: String,
        main_ep: String,
    }

    /// A ready database and a ready branch `dev` of main; the journal starts empty.
    async fn setup(w: &FakeWorker, journal: &Journal) -> Setup {
        let db = ready_database(w).await;
        let main = w.metadata.main(db);
        let dev = ready_branch(w, db, "dev", main.id).await;
        journal.lock().unwrap().clear();
        Setup {
            db,
            dev,
            tenant: w.metadata.database(db).neon_tenant_id.unwrap(),
            main_timeline: main.timeline_id.unwrap(),
            main_ep: main.endpoint_id.unwrap(),
        }
    }

    /// Only main is left, untouched; the tenant was never deleted.
    async fn assert_only_main_left(w: &FakeWorker, journal: &Journal, s: &Setup) {
        assert_eq!(w.metadata.main(s.db).status, BranchStatus::Ready);
        assert_eq!(w.metadata.endpoint_ids(), std::slice::from_ref(&s.main_ep));
        assert_eq!(
            w.storage.timeline_ids(),
            std::slice::from_ref(&s.main_timeline)
        );
        assert_eq!(
            w.computes.list().await.unwrap(),
            std::slice::from_ref(&s.main_ep)
        );
        assert!(w.storage.has_tenant(&s.tenant));
        assert!(
            !has(journal, "storage.delete_tenant"),
            "{:?}",
            entries(journal)
        );
    }

    #[tokio::test]
    async fn delete_removes_backups_then_the_branch() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        let backup = restored_by_hand(&w, s.dev, "dev-before-restore-20260930T120000Z");
        let backup_timeline = w.metadata.branch(backup).timeline_id.unwrap();
        let dev = w.metadata.branch(s.dev);
        let (dev_ep, dev_timeline) = (dev.endpoint_id.unwrap(), dev.timeline_id.unwrap());
        // The API claims the branch before publishing the job.
        assert_eq!(
            futures_util::FutureExt::now_or_never(w.metadata.begin_branch_delete(s.dev))
                .unwrap()
                .unwrap(),
            BranchDeleteOutcome::Started
        );
        journal.lock().unwrap().clear();

        w.handle_branch_delete(s.dev).await.unwrap();

        for id in [backup, s.dev] {
            let b = w.metadata.branch(id);
            assert_eq!(b.status, BranchStatus::Deleted);
            assert_eq!(b.owner_password_enc, None);
        }
        let order = [
            pos(
                &journal,
                &format!("storage.delete_timeline {backup_timeline}"),
            ),
            pos(&journal, &format!("compute.delete {dev_ep}")),
            pos(&journal, &format!("storage.delete_timeline {dev_timeline}")),
            pos(&journal, "metadata.delete_endpoint"),
            pos(&journal, "metadata.mark_branch_deleted"),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
        // The backup has no compute or endpoint to remove.
        assert_eq!(
            entries(&journal)
                .iter()
                .filter(|e| e.starts_with("compute.delete"))
                .count(),
            1
        );
        assert_eq!(
            entries(&journal)
                .iter()
                .filter(|e| e.starts_with("metadata.mark_branch_deleted"))
                .count(),
            2
        );
        assert_only_main_left(&w, &journal, &s).await;
    }

    #[tokio::test]
    async fn delete_also_removes_pending_and_superseded_timelines() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        let (pending, superseded) = (NeonId::new(), NeonId::new());
        w.storage
            .insert_timeline(&s.tenant, pending.as_str(), FRESH_HEAD);
        w.storage
            .insert_timeline(&s.tenant, superseded.as_str(), FRESH_HEAD);
        {
            let mut state = w.metadata.state();
            let dev = state.branches.get_mut(&s.dev).unwrap();
            dev.pending_timeline_id = Some(pending.as_str().to_string());
            dev.superseded_timeline_id = Some(superseded.as_str().to_string());
        }
        let dev_timeline = w.metadata.branch(s.dev).timeline_id.unwrap();

        w.handle_branch_delete(s.dev).await.unwrap();

        assert_eq!(w.metadata.branch(s.dev).status, BranchStatus::Deleted);
        for timeline in [dev_timeline.as_str(), pending.as_str(), superseded.as_str()] {
            assert!(has(
                &journal,
                &format!("storage.delete_timeline {timeline}")
            ));
        }
        assert!(
            pos(&journal, "metadata.delete_endpoint") > pos(&journal, "storage.delete_timeline")
        );
        assert_only_main_left(&w, &journal, &s).await;
    }

    // Fix round 1, I2: a restore stopped between creating its pending timeline (a child of
    // the branch's own until detached) and the detach. Children go before their ancestors.
    #[tokio::test]
    async fn a_delete_after_an_interrupted_restore_removes_the_pending_child_first() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        let dev = w.metadata.branch(s.dev);
        let tenant: NeonId = s.tenant.parse().unwrap();
        let own: NeonId = dev.timeline_id.clone().unwrap().parse().unwrap();
        let pending = NeonId::new();
        w.storage
            .create_branch_timeline(&tenant, &pending, &own, FRESH_HEAD)
            .await
            .unwrap();
        {
            let mut state = w.metadata.state();
            let row = state.branches.get_mut(&s.dev).unwrap();
            row.status = BranchStatus::Resetting;
            row.pending_timeline_id = Some(pending.as_str().to_string());
        }
        journal.lock().unwrap().clear();

        w.handle_branch_delete(s.dev).await.unwrap();

        assert_eq!(w.metadata.branch(s.dev).status, BranchStatus::Deleted);
        assert!(
            pos(&journal, &format!("storage.delete_timeline {pending}"))
                < pos(&journal, &format!("storage.delete_timeline {own}")),
            "{:?}",
            entries(&journal)
        );
        assert_only_main_left(&w, &journal, &s).await;
    }

    // Fix round 1, I1: a child of the branch's backup blocks the delete: the backup's timeline
    // is its ancestor, and Neon can't delete that. Nothing is torn down, and the branch is left
    // as it was (never stuck deleting), with the reason recorded.
    #[tokio::test]
    async fn a_delete_refused_for_a_backups_child_leaves_the_branch_as_it_was() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        let backup = restored_by_hand(&w, s.dev, "dev-before-restore-20260930T120000Z");
        let x = crate::fakes::ready_branch(&w, s.db, "x", backup).await;
        let timelines = w.storage.timeline_ids();
        journal.lock().unwrap().clear();

        w.handle_branch_delete(s.dev).await.unwrap();

        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Ready);
        assert_eq!(
            dev.error.as_deref(),
            Some(
                "cannot delete dev: its backup dev-before-restore-20260930T120000Z has child \
                 branches (x); delete them first"
            )
        );
        assert_eq!(w.metadata.branch(backup).status, BranchStatus::Ready);
        assert_eq!(w.metadata.branch(x).status, BranchStatus::Ready);
        assert_eq!(w.storage.timeline_ids(), timelines);
        for entry in [
            "storage.",
            "compute.",
            "metadata.delete_endpoint",
            "metadata.mark_branch_deleted",
        ] {
            assert!(!has(&journal, entry), "{entry}: {:?}", entries(&journal));
        }
    }

    // T6: the delete's own update refuses a protected branch (protected after the API's
    // check): nothing is torn down, and the branch says why.
    #[tokio::test]
    async fn a_protected_branch_is_left_as_it_was() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        w.metadata
            .state()
            .branches
            .get_mut(&s.dev)
            .unwrap()
            .protected = true;
        let timelines = w.storage.timeline_ids();

        w.handle_branch_delete(s.dev).await.unwrap();

        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Ready);
        assert_eq!(
            dev.error.as_deref(),
            Some("cannot delete dev: it is protected; unprotect it first")
        );
        assert_eq!(w.storage.timeline_ids(), timelines);
        for entry in ["storage.", "compute.", "metadata.mark_branch_deleted"] {
            assert!(!has(&journal, entry), "{entry}: {:?}", entries(&journal));
        }
    }

    #[tokio::test]
    async fn a_failed_teardown_keeps_deleting() {
        type Arrange = fn(&FakeWorker);
        let cases: [(&str, Arrange, &str); 3] = [
            (
                "compute",
                |w| *w.computes.fail_delete.lock().unwrap() = Some("docker is stuck".into()),
                "removing compute: ",
            ),
            (
                "timeline",
                |w| *w.storage.fail.lock().unwrap() = Some("delete_timeline"),
                "deleting timeline: ",
            ),
            (
                "endpoint",
                |w| w.metadata.state().fail = Some("delete_endpoint"),
                "deleting endpoint: ",
            ),
        ];
        for (step, arrange, prefix) in cases {
            let (w, journal) = worker();
            let s = setup(&w, &journal).await;
            let backup = restored_by_hand(&w, s.dev, "dev-before-restore-20260930T120000Z");
            journal.lock().unwrap().clear();
            arrange(&w);

            let err = w.handle_branch_delete(s.dev).await.unwrap_err();

            assert!(err.starts_with(prefix), "{step}: {err:?}");
            if step == "timeline" {
                assert!(err.ends_with(STORAGE_CAUSE), "{step}: {err:?}");
            }
            assert_eq!(
                w.metadata.branch(s.dev).status,
                BranchStatus::Deleting,
                "{step}"
            );
            assert_eq!(
                w.metadata.branch(backup).status,
                BranchStatus::Deleting,
                "{step}"
            );
            assert_eq!(w.metadata.endpoint_ids().len(), 2, "{step}");
            assert!(!has(&journal, "metadata.mark_branch_deleted"), "{step}");
            assert!(!has(&journal, "storage.delete_tenant"), "{step}");

            // The retried job finishes it.
            *w.computes.fail_delete.lock().unwrap() = None;
            *w.storage.fail.lock().unwrap() = None;
            w.metadata.state().fail = None;
            w.handle_branch_delete(s.dev).await.unwrap();
            assert_eq!(
                w.metadata.branch(s.dev).status,
                BranchStatus::Deleted,
                "{step}"
            );
            assert_eq!(
                w.metadata.branch(backup).status,
                BranchStatus::Deleted,
                "{step}"
            );
            assert_only_main_left(&w, &journal, &s).await;
        }
    }

    #[tokio::test]
    async fn main_missing_and_deleted_branches_are_left_alone() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        // Main goes with its database, never as a branch.
        w.handle_branch_delete(BranchId::from(s.db)).await.unwrap();
        w.handle_branch_delete(BranchId::new()).await.unwrap();
        assert!(entries(&journal).is_empty(), "{:?}", entries(&journal));

        w.handle_branch_delete(s.dev).await.unwrap();
        journal.lock().unwrap().clear();
        w.handle_branch_delete(s.dev).await.unwrap();
        assert!(entries(&journal).is_empty(), "{:?}", entries(&journal));
        assert_eq!(w.metadata.branch(s.dev).status, BranchStatus::Deleted);
        assert_only_main_left(&w, &journal, &s).await;
    }

    #[tokio::test]
    async fn metadata_trouble_is_retried() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        w.metadata.state().fail = Some("begin_branch_delete");
        assert!(w.handle_branch_delete(s.dev).await.is_err());
        assert_eq!(w.metadata.branch(s.dev).status, BranchStatus::Ready);
    }
}
