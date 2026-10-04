//! The database settings job: push the history window to the tenant as its `pitr_interval`.

use chelabase_metadata::DatabaseStatus;
use chelabase_neon::{ComputeBackend, NeonId};
use chelabase_types::DatabaseId;
use tracing::{info, warn};

use crate::Worker;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// `patch_tenant_config(tenant, history_window)`. A no-op before provisioning records a
    /// tenant, and once a delete has begun. `Err` (the job is retried) when either side fails.
    pub async fn handle_database_settings(&self, db: DatabaseId) -> Result<(), String> {
        let database = self
            .metadata
            .get_database(db)
            .await
            .map_err(|e| e.to_string())?;
        let Some(database) = database else {
            warn!(database_id = %db, "no database row; dropping settings job");
            return Ok(());
        };
        if matches!(
            database.status,
            DatabaseStatus::Deleting | DatabaseStatus::Deleted
        ) {
            return Ok(());
        }
        // Provisioning applies the window once it has made the tenant.
        let Some(tenant) = database.neon_tenant_id else {
            return Ok(());
        };
        let tenant: NeonId = tenant
            .parse()
            .map_err(|e| format!("tenant id {tenant:?}: {e}"))?;
        self.storage
            .patch_tenant_config(&tenant, database.history_window)
            .await
            .map_err(|e| format!("patching the history window: {e:#}"))?;
        info!(database_id = %db, %tenant, window_secs = database.history_window.as_secs(), "history window applied");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chelabase_metadata::DatabaseStatus;
    use chelabase_neon::NeonId;

    use crate::fakes::{STORAGE_CAUSE, entries, position, ready_database, worker};

    const WEEK: Duration = Duration::from_secs(7 * 24 * 3600);

    #[tokio::test]
    async fn settings_job_patches_the_tenant() {
        let (w, journal) = worker();
        let db = ready_database(&w).await;
        let tenant: NeonId = w
            .metadata
            .database(db)
            .neon_tenant_id
            .unwrap()
            .parse()
            .unwrap();
        let day = Duration::from_secs(24 * 3600);
        w.metadata
            .state()
            .databases
            .get_mut(&db)
            .unwrap()
            .history_window = day;
        journal.lock().unwrap().clear();

        w.handle_database_settings(db).await.unwrap();

        assert_eq!(w.storage.pitr.lock().unwrap().get(&tenant), Some(&day));
        assert_eq!(
            entries(&journal),
            [format!("storage.patch_tenant_config {tenant} 86400s")]
        );
    }

    #[tokio::test]
    async fn provision_applies_the_history_window_to_its_new_tenant() {
        let (w, journal) = worker();
        let db = ready_database(&w).await;
        let tenant: NeonId = w
            .metadata
            .database(db)
            .neon_tenant_id
            .unwrap()
            .parse()
            .unwrap();

        assert_eq!(w.storage.pitr.lock().unwrap().get(&tenant), Some(&WEEK));
        let patch = position(&journal, "storage.patch_tenant_config").expect("patched");
        assert!(position(&journal, "storage.create_tenant").unwrap() < patch);
        assert!(patch < position(&journal, "storage.create_timeline").unwrap());
    }

    #[tokio::test]
    async fn settings_job_without_a_tenant_or_once_deleting_does_nothing() {
        let (w, journal) = worker();
        let pending = w.metadata.insert_database("app", DatabaseStatus::Pending);
        w.handle_database_settings(pending).await.unwrap();
        w.handle_database_settings(chelabase_types::DatabaseId::new())
            .await
            .unwrap();
        let db = ready_database(&w).await;
        w.metadata.state().databases.get_mut(&db).unwrap().status = DatabaseStatus::Deleting;
        journal.lock().unwrap().clear();
        w.handle_database_settings(db).await.unwrap();
        assert!(
            position(&journal, "storage.patch_tenant_config").is_none(),
            "{:?}",
            entries(&journal)
        );
    }

    #[tokio::test]
    async fn a_failed_patch_is_retried_with_its_cause() {
        let (w, _journal) = worker();
        let db = ready_database(&w).await;
        *w.storage.fail.lock().unwrap() = Some("patch_tenant_config");
        let err = w.handle_database_settings(db).await.unwrap_err();
        assert!(err.starts_with("patching the history window: "), "{err}");
        assert!(err.contains(STORAGE_CAUSE), "{err}");
    }
}
