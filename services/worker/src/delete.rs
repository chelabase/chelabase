//! Deleting a database: every branch's compute, the tenant, the endpoint rows, then the
//! tombstones.

use chelabase_neon::{ComputeBackend, NeonId};
use chelabase_types::DatabaseId;
use tracing::info;

use crate::Worker;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// Tears down whatever the database's branches have (backups included): every compute,
    /// then the tenant (which takes every timeline with it), then the endpoint rows. Then marks
    /// each branch and the database `deleted`. `Err` (the job is retried, and the rows stay
    /// `deleting`) when any of that fails.
    pub async fn handle_delete(&self, id: DatabaseId) -> Result<(), String> {
        // object-storage m8: once the database is gone, its blobs' prefix goes too. The marker
        // first, so the periodic job queues the purge until it is done, whatever happens to
        // the request below.
        self.mark_purge_pending(id).await?;
        let result = self.delete_database(id).await;
        if result.is_ok() {
            self.sweep_requests
                .request(id, crate::storage_sweep::SweepCause::Purge);
        }
        result
    }

    async fn delete_database(&self, id: DatabaseId) -> Result<(), String> {
        let branches = self
            .metadata
            .list_branches(id)
            .await
            .map_err(|e| e.to_string())?;
        let mut endpoints = Vec::new();
        for branch in &branches {
            let Some(endpoint_id) = &branch.endpoint_id else {
                continue;
            };
            let endpoint = self
                .metadata
                .neon_endpoint(endpoint_id)
                .await
                .map_err(|e| e.to_string())?;
            endpoints.extend(endpoint);
        }
        for endpoint in &endpoints {
            self.computes
                .delete(&endpoint.id)
                .await
                .map_err(|e| format!("removing compute: {e:#}"))?;
        }
        // The database's tenant, and any other an endpoint names (a provision that recorded
        // its endpoint but not yet its tenant when the delete began).
        let mut tenants: Vec<String> = self
            .metadata
            .get_database(id)
            .await
            .map_err(|e| e.to_string())?
            .and_then(|d| d.neon_tenant_id)
            .into_iter()
            .collect();
        for endpoint in &endpoints {
            if !tenants.contains(&endpoint.tenant_id) {
                tenants.push(endpoint.tenant_id.clone());
            }
        }
        for tenant in &tenants {
            self.delete_tenant(tenant).await?;
        }
        for endpoint in &endpoints {
            self.metadata
                .delete_endpoint(&endpoint.id)
                .await
                .map_err(|e| format!("deleting endpoint: {e}"))?;
        }
        for branch in &branches {
            self.metadata
                .mark_branch_deleted(branch.id)
                .await
                .map_err(|e| e.to_string())?;
        }
        self.metadata
            .mark_deleted(id)
            .await
            .map_err(|e| e.to_string())?;
        info!(database_id = %id, branches = branches.len(), "database deleted");
        Ok(())
    }

    async fn delete_tenant(&self, tenant_id: &str) -> Result<(), String> {
        let tenant: NeonId = tenant_id
            .parse()
            .map_err(|e| format!("deleting tenant: {e}"))?;
        self.storage
            .delete_tenant(&tenant)
            .await
            .map_err(|e| format!("deleting tenant: {e:#}"))
    }

    /// Removes the compute, the tenant and the endpoint row, in that order, stopping at the
    /// first failure so the endpoint row is left for reconcile to retry.
    pub(crate) async fn teardown(&self, endpoint_id: &str, tenant_id: &str) -> Result<(), String> {
        self.computes
            .delete(endpoint_id)
            .await
            .map_err(|e| format!("removing compute: {e:#}"))?;
        self.delete_tenant(tenant_id).await?;
        self.metadata
            .delete_endpoint(endpoint_id)
            .await
            .map_err(|e| format!("deleting endpoint: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use chelabase_metadata::{BranchStatus, DatabaseStatus};

    use super::*;
    use crate::fakes::{
        FakeWorker, Journal, STORAGE_CAUSE, entries, position, ready_branch, restored_by_hand,
        worker,
    };

    fn pos(journal: &Journal, prefix: &str) -> usize {
        position(journal, prefix)
            .unwrap_or_else(|| panic!("{prefix:?} not in journal {:?}", entries(journal)))
    }

    /// A ready database, provisioned through the fakes; the journal starts empty.
    async fn ready(w: &FakeWorker, journal: &Journal) -> (DatabaseId, String, String) {
        let id = w.metadata.insert_database("app", DatabaseStatus::Pending);
        w.handle_provision(id, "app").await.unwrap();
        let ep = w.metadata.main(id).endpoint_id.unwrap();
        let tenant = w.metadata.state().endpoints[&ep].endpoint.tenant_id.clone();
        {
            let mut state = w.metadata.state();
            state.databases.get_mut(&id).unwrap().status = DatabaseStatus::Deleting;
            state
                .branches
                .get_mut(&chelabase_types::BranchId::from(id))
                .unwrap()
                .status = BranchStatus::Deleting;
        }
        journal.lock().unwrap().clear();
        (id, ep, tenant)
    }

    #[tokio::test]
    async fn delete_tears_down_then_marks_deleted() {
        let (w, journal) = worker();
        let (id, ep, tenant) = ready(&w, &journal).await;
        w.handle_delete(id).await.unwrap();
        let db = w.metadata.database(id);
        let main = w.metadata.main(id);
        assert_eq!(db.status, DatabaseStatus::Deleted);
        assert_eq!(main.status, BranchStatus::Deleted);
        assert_eq!(main.owner_password_enc, None);
        assert!(w.metadata.endpoint_ids().is_empty());
        assert!(!w.storage.has_tenant(&tenant));
        assert_eq!(w.computes.status(&ep).await.unwrap(), None);
        let order = [
            pos(&journal, &format!("compute.delete {ep}")),
            pos(&journal, "storage.delete_tenant"),
            pos(&journal, "metadata.delete_endpoint"),
            pos(&journal, "metadata.mark_branch_deleted"),
            pos(&journal, "metadata.mark_deleted"),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
    }

    #[tokio::test]
    async fn delete_without_an_endpoint_marks_deleted() {
        let (w, journal) = worker();
        let id = w.metadata.insert_database("app", DatabaseStatus::Deleting);
        w.handle_delete(id).await.unwrap();
        assert_eq!(w.metadata.database(id).status, DatabaseStatus::Deleted);
        assert_eq!(w.metadata.main(id).status, BranchStatus::Deleted);
        assert_eq!(
            entries(&journal),
            ["metadata.mark_branch_deleted", "metadata.mark_deleted"]
        );
    }

    #[tokio::test]
    async fn a_failed_teardown_keeps_the_row_deleting_and_errs() {
        type Arrange = fn(&FakeWorker);
        let cases: [(&str, Arrange, &str); 3] = [
            (
                "compute",
                |w| *w.computes.fail_delete.lock().unwrap() = Some("docker is stuck".into()),
                "removing compute: ",
            ),
            (
                "tenant",
                |w| *w.storage.fail.lock().unwrap() = Some("delete_tenant"),
                "deleting tenant: ",
            ),
            (
                "endpoint",
                |w| w.metadata.state().fail = Some("delete_endpoint"),
                "deleting endpoint: ",
            ),
        ];
        for (step, arrange, prefix) in cases {
            let (w, journal) = worker();
            let (id, ep, _tenant) = ready(&w, &journal).await;
            arrange(&w);
            let err = w.handle_delete(id).await.unwrap_err();
            assert!(err.starts_with(prefix), "{step}: {err:?}");
            if step == "tenant" {
                assert!(err.ends_with(STORAGE_CAUSE), "{step}: {err:?}");
            }
            assert_eq!(
                w.metadata.database(id).status,
                DatabaseStatus::Deleting,
                "{step}"
            );
            assert_eq!(w.metadata.endpoint_ids(), [ep], "{step}");
            assert_eq!(w.metadata.main(id).status, BranchStatus::Deleting, "{step}");
            assert!(
                position(&journal, "metadata.mark_deleted").is_none(),
                "{step}"
            );
            assert!(
                position(&journal, "metadata.mark_branch_deleted").is_none(),
                "{step}"
            );
        }
    }

    #[tokio::test]
    async fn database_delete_removes_every_branch_compute_first() {
        let (w, journal) = worker();
        let id = w.metadata.insert_database("app", DatabaseStatus::Pending);
        w.handle_provision(id, "app").await.unwrap();
        let main = chelabase_types::BranchId::from(id);
        let dev = ready_branch(&w, id, "dev", main).await;
        let backup = restored_by_hand(&w, dev, "dev-before-restore-20260930T120000Z");
        let other = ready_branch(&w, id, "other", main).await;
        let tenant = w.metadata.database(id).neon_tenant_id.unwrap();
        let eps: Vec<String> = [main, dev, other]
            .map(|b| w.metadata.branch(b).endpoint_id.unwrap())
            .to_vec();
        assert_eq!(
            w.storage.timeline_ids().len(),
            4,
            "main, dev (restored), its backup, other"
        );
        {
            let mut state = w.metadata.state();
            state.databases.get_mut(&id).unwrap().status = DatabaseStatus::Deleting;
            for b in state.branches.values_mut() {
                b.status = BranchStatus::Deleting;
            }
        }
        journal.lock().unwrap().clear();

        w.handle_delete(id).await.unwrap();

        assert_eq!(w.metadata.database(id).status, DatabaseStatus::Deleted);
        for b in [main, dev, backup, other] {
            let row = w.metadata.branch(b);
            assert_eq!(row.status, BranchStatus::Deleted, "{}", row.name);
            assert_eq!(row.owner_password_enc, None, "{}", row.name);
        }
        assert!(w.metadata.endpoint_ids().is_empty());
        assert!(w.computes.list().await.unwrap().is_empty());
        assert!(!w.storage.has_tenant(&tenant));
        assert!(w.storage.timeline_ids().is_empty());
        let journal_entries = entries(&journal);
        let count = |prefix: &str| {
            journal_entries
                .iter()
                .filter(|e| e.starts_with(prefix))
                .count()
        };
        assert_eq!(count("storage.delete_tenant"), 1, "{journal_entries:?}");
        assert_eq!(
            count("storage.delete_timeline"),
            0,
            "the tenant takes every timeline"
        );
        assert_eq!(count("metadata.delete_endpoint"), 3);
        assert_eq!(count("metadata.mark_branch_deleted"), 4);
        let tenant_at = pos(&journal, "storage.delete_tenant");
        for ep in &eps {
            assert!(
                pos(&journal, &format!("compute.delete {ep}")) < tenant_at,
                "{journal_entries:?}"
            );
        }
        let order = [
            tenant_at,
            pos(&journal, "metadata.delete_endpoint"),
            pos(&journal, "metadata.mark_branch_deleted"),
            pos(&journal, "metadata.mark_deleted"),
        ];
        assert!(order.is_sorted(), "{journal_entries:?}");
    }
}
