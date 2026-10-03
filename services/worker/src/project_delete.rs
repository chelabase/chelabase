//! A project's delete, after its databases: its app sessions, its Zitadel org, then the rows.

use cheladb_metadata::ProjectStatus;
use cheladb_neon::ComputeBackend;
use cheladb_types::ProjectId;
use tracing::{info, warn};

use crate::Worker;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// Finishes a `deleting` project once none of its databases is left: its app sessions
    /// revoked (and their Zitadel sessions deleted), its Zitadel org deleted, then
    /// `finish_project_delete`. While databases remain, the first one's error (or how many
    /// are left) is recorded on the project and the job fails, to be redelivered; the
    /// periodic pass is the backstop.
    pub async fn handle_project_delete(&self, project: ProjectId) -> Result<(), String> {
        let record = self
            .metadata
            .project_by_id(project)
            .await
            .map_err(|e| e.to_string())?;
        match record.map(|p| p.status) {
            Some(ProjectStatus::Deleting) => {}
            Some(status) => {
                info!(%project, status = status.as_str(), "project not deleting; dropping the delete job");
                return Ok(());
            }
            None => {
                warn!(%project, "no project row; dropping the delete job");
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
        let org = self
            .metadata
            .project_auth(project)
            .await
            .map_err(|e| e.to_string())?
            .and_then(|a| a.zitadel_org_id);
        let zitadel = match (&org, &self.zitadel) {
            (Some(_), None) => {
                return Err(
                    "the project has a Zitadel organization but Zitadel admin access isn't configured (CHELADB_ZITADEL_ISSUER and CHELADB_ZITADEL_ADMIN_PAT_FILE)"
                        .to_string(),
                );
            }
            (_, zitadel) => zitadel.as_ref(),
        };
        let revoked = self
            .metadata
            .revoke_project_sessions(project)
            .await
            .map_err(|e| format!("revoking the app sessions: {e}"))?;
        if let Some(zitadel) = zitadel {
            for session in revoked {
                if let Err(e) = zitadel
                    .delete_session_admin(&session.zitadel_session_id)
                    .await
                {
                    warn!(%project, session = %session.id, "deleting the Zitadel session: {e}");
                }
            }
        }
        if let (Some(org), Some(zitadel)) = (&org, zitadel)
            && let Err(e) = zitadel.delete_org(org).await
        {
            let error = format!("deleting the Zitadel organization: {e}");
            if let Err(e) = self.metadata.set_project_error(project, Some(&error)).await {
                warn!(%project, "recording the project's error: {e}");
            }
            return Err(error);
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
    use cheladb_metadata::{AppUsersStatus, DatabaseStatus};
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
        assert!(s.mock.calls().is_empty());

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

    #[tokio::test]
    async fn project_delete_finishes_org_and_rows() {
        let mut s = setup(AppUsersStatus::Enabled).await;
        s.with_org();
        set_deleting(&s);
        s.w.metadata
            .state()
            .databases
            .get_mut(&s.db)
            .unwrap()
            .status = DatabaseStatus::Deleted;
        let user = s.add_user("ada@example.com");
        s.mock
            .inner
            .lock()
            .unwrap()
            .sessions
            .insert("zs1".into(), (user.to_string(), "t".into()));
        let session = s.w.metadata.add_session(s.project, user, "zs1");
        s.w.metadata
            .set_branch_auth_sync(s.main.id, "app", "sealed")
            .await
            .unwrap();

        // An org without the admin client: retried, nothing changed.
        let zitadel = s.w.zitadel.take();
        s.w.handle_project_delete(s.project).await.unwrap_err();
        assert_eq!(
            s.w.metadata.state().projects[&s.project].status,
            ProjectStatus::Deleting
        );
        assert!(s.w.metadata.state().app_sessions[0].revoked_at.is_none());
        s.w.zitadel = zitadel;

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
        assert!(!s.mock.orgs().contains(&s.org()));
        assert!(
            s.mock
                .calls()
                .contains(&"DELETE /v2/sessions/zs1".to_string())
        );

        // Again (a redelivery): done already.
        s.w.handle_project_delete(s.project).await.unwrap();

        // A project without app users needs no Zitadel at all.
        let mut s = setup(AppUsersStatus::Enabled).await;
        s.w.metadata.state().project_auth.clear();
        set_deleting(&s);
        s.w.metadata
            .state()
            .databases
            .get_mut(&s.db)
            .unwrap()
            .status = DatabaseStatus::Deleted;
        s.w.zitadel = None;
        s.w.metadata.add_session(s.project, Uuid::new_v4(), "zs9");
        s.w.handle_project_delete(s.project).await.unwrap();
        assert_eq!(
            s.w.metadata.state().projects[&s.project].status,
            ProjectStatus::Deleted
        );
    }
}
