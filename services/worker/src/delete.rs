//! Deleting a database: its compute, its tenant, its endpoint row, then the tombstone.

use cheladb_neon::{ComputeBackend, NeonId};
use cheladb_types::DatabaseId;
use tracing::info;

use crate::Worker;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// Tears down whatever the database has, then marks it `deleted`. `Err` (the job is
    /// retried, and the row stays `deleting`) when any of that fails.
    pub async fn handle_delete(&self, id: DatabaseId) -> Result<(), String> {
        let record = self
            .metadata
            .get_database(id)
            .await
            .map_err(|e| e.to_string())?;
        if let Some(endpoint_id) = record.and_then(|r| r.endpoint_id) {
            let endpoint = self
                .metadata
                .neon_endpoint(&endpoint_id)
                .await
                .map_err(|e| e.to_string())?;
            if let Some(endpoint) = endpoint {
                self.teardown(&endpoint.id, &endpoint.tenant_id).await?;
            }
        }
        self.metadata
            .mark_deleted(id)
            .await
            .map_err(|e| e.to_string())?;
        info!(database_id = %id, "database deleted");
        Ok(())
    }

    /// Removes the compute, the tenant and the endpoint row, in that order, stopping at the
    /// first failure so the endpoint row is left for reconcile to retry.
    pub(crate) async fn teardown(&self, endpoint_id: &str, tenant_id: &str) -> Result<(), String> {
        self.computes
            .delete(endpoint_id)
            .await
            .map_err(|e| format!("removing compute: {e:#}"))?;
        let tenant: NeonId = tenant_id
            .parse()
            .map_err(|e| format!("deleting tenant: {e}"))?;
        self.storage
            .delete_tenant(&tenant)
            .await
            .map_err(|e| format!("deleting tenant: {e:#}"))?;
        self.metadata
            .delete_endpoint(endpoint_id)
            .await
            .map_err(|e| format!("deleting endpoint: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use cheladb_metadata::DatabaseStatus;

    use super::*;
    use crate::fakes::{FakeWorker, Journal, STORAGE_CAUSE, entries, position, worker};

    fn pos(journal: &Journal, prefix: &str) -> usize {
        position(journal, prefix)
            .unwrap_or_else(|| panic!("{prefix:?} not in journal {:?}", entries(journal)))
    }

    /// A ready database, provisioned through the fakes; the journal starts empty.
    async fn ready(w: &FakeWorker, journal: &Journal) -> (DatabaseId, String, String) {
        let id = w.metadata.insert_database("app", DatabaseStatus::Pending);
        w.handle_provision(id, "app").await.unwrap();
        let ep = w.metadata.database(id).endpoint_id.unwrap();
        let tenant = w.metadata.state().endpoints[&ep].endpoint.tenant_id.clone();
        w.metadata.state().databases.get_mut(&id).unwrap().status = DatabaseStatus::Deleting;
        journal.lock().unwrap().clear();
        (id, ep, tenant)
    }

    #[tokio::test]
    async fn delete_tears_down_then_marks_deleted() {
        let (w, journal) = worker();
        let (id, ep, tenant) = ready(&w, &journal).await;
        w.handle_delete(id).await.unwrap();
        let db = w.metadata.database(id);
        assert_eq!(db.status, DatabaseStatus::Deleted);
        assert_eq!(db.owner_password_enc, None);
        assert!(w.metadata.endpoint_ids().is_empty());
        assert!(!w.storage.has_tenant(&tenant));
        assert_eq!(w.computes.status(&ep).await.unwrap(), None);
        let order = [
            pos(&journal, &format!("compute.delete {ep}")),
            pos(&journal, "storage.delete_tenant"),
            pos(&journal, "metadata.delete_endpoint"),
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
        assert_eq!(entries(&journal), ["metadata.mark_deleted"]);
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
            assert!(
                position(&journal, "metadata.mark_deleted").is_none(),
                "{step}"
            );
        }
    }
}
