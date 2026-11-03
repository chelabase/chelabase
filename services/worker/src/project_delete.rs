//! A project's delete, after its databases: its app sessions, the rows, then its app-user pool.

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
    /// revoked, `finish_project_delete` (the metadata rows go), then its app-user pool dropped.
    /// While databases remain, the first one's error (or how many are left) is recorded on the
    /// project and the job fails, to be redelivered; the periodic pass is the backstop.
    ///
    /// The drop comes last so nothing can recreate the pool after it (an enable needs the
    /// `project_auth` row, which is gone by then). A failed drop fails the job, and its
    /// redelivery finds the project `deleted` (or its row gone) and drops the pool again until
    /// it succeeds. The drop is idempotent and names only this project's pool.
    pub async fn handle_project_delete(&self, project: ProjectId) -> Result<(), String> {
        let record = self
            .metadata
            .project_by_id(project)
            .await
            .map_err(|e| e.to_string())?;
        match record.map(|p| p.status) {
            Some(ProjectStatus::Deleting) => {}
            Some(ProjectStatus::Deleted) | None => {
                // A redelivery after the rows went: only the pool may be left.
                return drop_pool(self.identity()?, project).await;
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
            .finish_project_delete(project)
            .await
            .map_err(|e| e.to_string())?;
        info!(%project, "project deleted");
        if let Err(e) = drop_pool(identity, project).await {
            warn!(%project, "dropping the app-user pool; the delete job is retried: {e}");
            return Err(e);
        }
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
    async fn project_delete_revokes_finishes_rows_then_drops_pool() {
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
        let finished = journal
            .iter()
            .position(|e| e == "metadata.finish_project_delete")
            .unwrap();
        assert!(revoked < finished, "{journal:?}");

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

    /// AC 2: the pool is dropped only after the metadata rows go; with the identity service
    /// down the job fails and is redelivered until the drop succeeds. Other projects' pools
    /// are untouched.
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

        // The identity service down: the rows go, the drop fails, the job is retried.
        s.fake.set_down(true);
        let err = s.w.handle_project_delete(s.project).await.unwrap_err();
        assert!(err.contains("dropping the app-user pool"), "{err}");
        assert_eq!(
            s.w.metadata.state().projects[&s.project].status,
            ProjectStatus::Deleted
        );
        assert!(s.w.metadata.auth(s.project).is_none());
        s.fake.set_down(false);
        assert!(s.pool_exists().await, "not dropped while down");

        // A refusal that isn't `Unavailable` is retried too.
        s.fake
            .fail("DELETE /internal/v1/pools/", 503, "internal", 1);
        s.w.handle_project_delete(s.project).await.unwrap_err();
        assert!(s.pool_exists().await);

        // The redelivery (the project row now `deleted`) drops it.
        s.w.handle_project_delete(s.project).await.unwrap();
        assert!(!s.pool_exists().await);
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
}
