//! A project's delete, after its databases: its app sessions, its app users settings, its
//! app-user pool, then the rest of its rows.

use chelabase_metadata::ProjectStatus;
use chelabase_neon::ComputeBackend;
use chelabase_types::ProjectId;
use tracing::{info, warn};

use crate::Worker;
use crate::app_users::drop_pool;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// Finishes a `deleting` project once none of its databases is left: its app sessions
    /// revoked, its `project_auth` row deleted, its app-user pool dropped, then
    /// `finish_project_delete`. While databases remain, the first one's error (or how many are
    /// left) is recorded on the project and the job fails, to be redelivered; the periodic pass
    /// is the backstop.
    ///
    /// The project stays `deleting` until the drop succeeded, so a failed drop is retried by a
    /// redelivery and by the periodic pass alike (which has no message to redeliver). The row
    /// goes before the drop so nothing starts an enable after it; an enable already past its
    /// read checks the row again after creating the pool, and drops it when the row is gone
    /// (`enable_steps`). The drop is idempotent and names only this project's pool.
    pub async fn handle_project_delete(&self, project: ProjectId) -> Result<(), String> {
        let record = self
            .metadata
            .project_by_id(project)
            .await
            .map_err(|e| e.to_string())?;
        match record.map(|p| p.status) {
            Some(ProjectStatus::Deleting) => {}
            Some(ProjectStatus::Deleted) | None => {
                // A redelivery of a finished delete, or one that finished before the drop
                // moved ahead of the rows: only the pool may be left. Without an identity
                // client there is nothing to retry for: acked, so the job doesn't sit in the
                // queue for ever.
                return match self.identity.as_ref() {
                    Some(identity) => drop_pool(identity, project).await,
                    None => {
                        warn!(%project, "project already deleted and no identity service is configured; a pool left behind isn't dropped");
                        Ok(())
                    }
                };
            }
            Some(status) => {
                info!(%project, status = status.as_str(), "project not deleting; dropping the delete job");
                return Ok(());
            }
        }
        let left = self
            .metadata
            .project_databases(project)
            .await
            .map_err(|e| e.to_string())?;
        if !left.is_empty() {
            let waiting = match left.len() {
                1 => "waiting for 1 database".to_string(),
                n => format!("waiting for {n} databases"),
            };
            let error = left
                .iter()
                .find_map(|(_, _, error)| error.clone())
                .unwrap_or(waiting);
            self.metadata
                .set_project_error(project, Some(&error))
                .await
                .map_err(|e| e.to_string())?;
            return Err(error);
        }
        // Checked before anything changes, so a missing client leaves the project as it was.
        let identity = self.identity()?;
        self.metadata
            .revoke_project_sessions(project)
            .await
            .map_err(|e| format!("revoking the app sessions: {e}"))?;
        self.metadata
            .delete_project_auth(project)
            .await
            .map_err(|e| e.to_string())?;
        if let Err(e) = drop_pool(identity, project).await {
            warn!(%project, "dropping the app-user pool; the delete is retried: {e}");
            self.metadata
                .set_project_error(project, Some(&e))
                .await
                .map_err(|e| e.to_string())?;
            return Err(e);
        }
        self.metadata
            .finish_project_delete(project)
            .await
            .map_err(|e| e.to_string())?;
        info!(%project, "project deleted");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chelabase_metadata::{AppUsersStatus, DatabaseStatus};
    use uuid::Uuid;

    use super::*;
    use crate::app_users::tests::setup;

    fn set_deleting(s: &crate::app_users::tests::Setup) {
        s.w.metadata
            .state()
            .projects
            .get_mut(&s.project)
            .unwrap()
            .status = ProjectStatus::Deleting;
    }

    #[tokio::test]
    async fn project_delete_waits_for_databases_and_records_error() {
        let s = setup(AppUsersStatus::Enabled).await;
        set_deleting(&s);
        {
            let mut state = s.w.metadata.state();
            let db = state.databases.get_mut(&s.db).unwrap();
            db.status = DatabaseStatus::Deleting;
            db.error = Some("deleting the tenant: storage controller unavailable".into());
        }
        let err = s.w.handle_project_delete(s.project).await.unwrap_err();
        assert_eq!(err, "deleting the tenant: storage controller unavailable");
        let project = s.w.metadata.state().projects[&s.project].clone();
        assert_eq!(project.status, ProjectStatus::Deleting);
        assert_eq!(project.error.as_deref(), Some(err.as_str()));

        // Without an error, it says how many are left.
        s.w.metadata.state().databases.get_mut(&s.db).unwrap().error = None;
        let err = s.w.handle_project_delete(s.project).await.unwrap_err();
        assert_eq!(err, "waiting for 1 database");
        assert_eq!(
            s.w.metadata.state().projects[&s.project].error.as_deref(),
            Some("waiting for 1 database")
        );
        assert!(s.w.metadata.auth(s.project).is_some());
        assert!(s.fake.calls().is_empty());

        // A project that isn't deleting (or is gone) is left alone.
        s.w.metadata
            .state()
            .projects
            .get_mut(&s.project)
            .unwrap()
            .status = ProjectStatus::Active;
        s.w.handle_project_delete(s.project).await.unwrap();
        s.w.handle_project_delete(ProjectId::new()).await.unwrap();
        assert_eq!(
            s.w.metadata.state().projects[&s.project].status,
            ProjectStatus::Active
        );
    }

    fn databases_gone(s: &crate::app_users::tests::Setup) {
        s.w.metadata
            .state()
            .databases
            .get_mut(&s.db)
            .unwrap()
            .status = DatabaseStatus::Deleted;
    }

    #[tokio::test]
    async fn project_delete_revokes_drops_pool_then_finishes_rows() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_pool();
        set_deleting(&s);
        databases_gone(&s);
        let user = s.add_user("ada@example.com");
        let session = s.w.metadata.add_session(s.project, user, "s1");
        s.w.metadata
            .set_branch_auth_sync(s.main.id, "app", "sealed")
            .await
            .unwrap();

        s.w.handle_project_delete(s.project).await.unwrap();

        {
            let state = s.w.metadata.state();
            assert_eq!(state.projects[&s.project].status, ProjectStatus::Deleted);
            assert_eq!(state.projects[&s.project].error, None);
            assert!(!state.project_auth.contains_key(&s.project));
            assert!(state.auth_sync.is_empty());
            let revoked = state.app_sessions.iter().find(|x| x.id == session).unwrap();
            assert!(revoked.revoked_at.is_some());
        }
        assert!(!s.pool_exists().await);
        let journal = crate::fakes::entries(&s.journal);
        let revoked = journal
            .iter()
            .position(|e| e == "metadata.revoke_project_sessions")
            .unwrap();
        let auth_gone = journal
            .iter()
            .position(|e| e == "metadata.delete_project_auth")
            .unwrap();
        let finished = journal
            .iter()
            .position(|e| e == "metadata.finish_project_delete")
            .unwrap();
        assert!(revoked < auth_gone && auth_gone < finished, "{journal:?}");

        // Again (a redelivery): done already; the drop is idempotent.
        s.w.handle_project_delete(s.project).await.unwrap();

        // A project without app users still has its (absent) pool dropped: one rule for
        // every delete, so a pool an enable raced in never outlives the project.
        let s = setup(AppUsersStatus::Enabled).await;
        s.w.metadata.state().project_auth.clear();
        set_deleting(&s);
        databases_gone(&s);
        s.w.handle_project_delete(s.project).await.unwrap();
        assert_eq!(
            s.w.metadata.state().projects[&s.project].status,
            ProjectStatus::Deleted
        );
        assert_eq!(s.pool_calls(), ["DELETE "]);
    }

    /// AC 2: the pool is dropped before the project is `deleted`; with the identity service
    /// down the job fails and is redelivered until the drop succeeds, the project `deleting`
    /// with the error on it. Other projects' pools are untouched.
    #[tokio::test]
    async fn drops_pool_and_retries_unavailable() {
        let mut s = setup(AppUsersStatus::Enabled).await;
        s.with_pool();
        s.add_user("ada@example.com");
        let other = chelabase_identity::Pool::App(Uuid::new_v4());
        let neighbour = s.add_user_in(&other, "bob@example.com");
        set_deleting(&s);
        databases_gone(&s);

        // Without the identity client nothing changes: retried.
        let identity = s.w.identity.take();
        s.w.handle_project_delete(s.project).await.unwrap_err();
        assert_eq!(
            s.w.metadata.state().projects[&s.project].status,
            ProjectStatus::Deleting
        );
        assert!(s.w.metadata.auth(s.project).is_some());
        s.w.identity = identity;

        // The identity service down: the settings go, the drop fails, the job is retried.
        s.fake.set_down(true);
        let err = s.w.handle_project_delete(s.project).await.unwrap_err();
        assert!(err.contains("dropping the app-user pool"), "{err}");
        {
            let state = s.w.metadata.state();
            assert_eq!(state.projects[&s.project].status, ProjectStatus::Deleting);
            assert_eq!(state.projects[&s.project].error.as_deref(), Some(err.as_str()));
        }
        assert!(s.w.metadata.auth(s.project).is_none());
        s.fake.set_down(false);
        assert!(s.pool_exists().await, "not dropped while down");

        // A refusal that isn't `Unavailable` is retried too.
        s.fake
            .fail("DELETE /internal/v1/pools/", 503, "internal", 1);
        s.w.handle_project_delete(s.project).await.unwrap_err();
        assert!(s.pool_exists().await);

        // The redelivery drops it and finishes.
        s.w.handle_project_delete(s.project).await.unwrap();
        assert!(!s.pool_exists().await);
        assert_eq!(
            s.w.metadata.state().projects[&s.project].status,
            ProjectStatus::Deleted
        );
        assert_eq!(s.w.metadata.state().projects[&s.project].error, None);
        s.fake
            .client()
            .get_user(&other, &neighbour.to_string())
            .await
            .expect("another project's pool is untouched");
        assert!(
            s.fake
                .calls()
                .iter()
                .filter(|c| c.starts_with("DELETE "))
                .all(|c| c.ends_with(&s.pool().to_string())),
            "{:?}",
            s.fake.calls()
        );

        // A redelivery whose project row is gone altogether still drops the pool.
        s.with_pool();
        s.w.metadata.state().projects.remove(&s.project);
        s.w.handle_project_delete(s.project).await.unwrap();
        assert!(!s.pool_exists().await);

        // A project that isn't deleting keeps its pool.
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_pool();
        s.w.handle_project_delete(s.project).await.unwrap();
        assert!(s.pool_exists().await);
        assert!(s.pool_calls().iter().all(|c| !c.starts_with("DELETE")));
    }

    /// Re-audit nit: a redelivered delete for a project already `deleted` (or gone) with no
    /// identity client configured is acked with a warning, not redelivered for ever; one for
    /// a project still `deleting` is retried as before.
    #[tokio::test]
    async fn a_delete_of_a_deleted_project_without_identity_acks() {
        let mut s = setup(AppUsersStatus::Enabled).await;
        s.w.identity = None;
        s.w.metadata
            .state()
            .projects
            .get_mut(&s.project)
            .unwrap()
            .status = ProjectStatus::Deleted;
        s.w.handle_project_delete(s.project).await.unwrap();
        s.w.metadata.state().projects.remove(&s.project);
        s.w.handle_project_delete(s.project).await.unwrap();

        let s = setup(AppUsersStatus::Enabled).await;
        set_deleting(&s);
        databases_gone(&s);
        let mut s = s;
        s.w.identity = None;
        s.w.handle_project_delete(s.project).await.unwrap_err();
        assert_eq!(
            s.w.metadata.state().projects[&s.project].status,
            ProjectStatus::Deleting
        );
    }

    /// B4: the periodic pass has no message to redeliver: the project stays `deleting` until
    /// the drop succeeded, so the next pass tries again.
    #[tokio::test]
    async fn periodic_backstop_retries_the_pool_drop() {
        let s = setup(AppUsersStatus::Enabled).await;
        s.with_pool();
        s.add_user("ada@example.com");
        let age = |s: &crate::app_users::tests::Setup| {
            s.w.metadata
                .state()
                .projects
                .get_mut(&s.project)
                .unwrap()
                .updated_at = chrono::Utc::now() - chrono::Duration::minutes(10);
        };
        set_deleting(&s);
        databases_gone(&s);
        age(&s);

        s.fake.set_down(true);
        s.w.app_users_reconcile_once().await;
        s.fake.set_down(false);
        assert_eq!(
            s.w.metadata.state().projects[&s.project].status,
            ProjectStatus::Deleting,
            "orphaned pool: nothing would retry the drop"
        );
        assert!(s.pool_exists().await);

        age(&s);
        s.w.app_users_reconcile_once().await;
        assert!(!s.pool_exists().await);
        assert_eq!(
            s.w.metadata.state().projects[&s.project].status,
            ProjectStatus::Deleted
        );
    }
}
