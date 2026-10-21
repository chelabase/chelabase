//! [`super::PublishingMetadata`]'s [`WorkerMetadata`]: every call goes to the wrapped port
//! unchanged; the status writes then publish (the table in [`super`]'s docs).

use chelabase_metadata::members::{GrantRows, MemberBranchRole, MemberRecord};
use chelabase_metadata::{
    ApiRoleChange, AppSession, BranchAuthSync, BranchConnectionRow, BranchDatabase,
    BranchDeleteOutcome, BranchKind, BranchRecord, BranchStatus, ClaimOutcome, DataKeyRecord,
    DatabaseRecord, DatabaseSettings, DatabaseStatus, EndpointState, InsertRoleOutcome, LiveBranch,
    LiveDatabase, MergeEvent, MergeRecord, MergeStatus, MergeStatusPatch, MetadataError,
    NeonDatabase, NeonEndpoint, NeonRole, NewApiRole, NewDataKey, NewEndpoint, NewStaticEndpoint,
    ProjectAuth, ProjectRecord, RenameCopyOutcome, ResyncApplied, ResyncWrite, RoleCopy,
    RoleWriteOutcome, SchemaBase, ServedDatabase,
};
use chelabase_types::{BranchId, DatabaseId, ProjectId, TenantId};
use chrono::{DateTime, Utc};
use std::time::Duration;
use uuid::Uuid;

use chelabase_nats::rt::ProjectEvent;

use super::PublishingMetadata;
use crate::ports::WorkerMetadata;

impl<M: WorkerMetadata> WorkerMetadata for PublishingMetadata<M> {
    type EndpointLock = M::EndpointLock;

    async fn lock_endpoint(&self, endpoint_id: &str) -> Result<Self::EndpointLock, MetadataError> {
        self.inner.lock_endpoint(endpoint_id).await
    }

    async fn get_database(&self, id: DatabaseId) -> Result<Option<DatabaseRecord>, MetadataError> {
        self.inner.get_database(id).await
    }

    async fn set_status(
        &self,
        id: DatabaseId,
        status: DatabaseStatus,
    ) -> Result<bool, MetadataError> {
        let changed = self.inner.set_status(id, status).await?;
        if changed {
            self.database_status(id, BranchStatus::from(status)).await;
        }
        Ok(changed)
    }

    async fn endpoint_branch(&self, endpoint_id: &str) -> Result<Option<BranchId>, MetadataError> {
        self.inner.endpoint_branch(endpoint_id).await
    }

    async fn compute_branch(
        &self,
        endpoint_id: &str,
    ) -> Result<Option<(BranchId, BranchKind)>, MetadataError> {
        self.inner.compute_branch(endpoint_id).await
    }

    async fn get_branch(&self, id: BranchId) -> Result<Option<BranchRecord>, MetadataError> {
        self.inner.get_branch(id).await
    }

    async fn list_branches(&self, db: DatabaseId) -> Result<Vec<BranchRecord>, MetadataError> {
        self.inner.list_branches(db).await
    }

    async fn create_endpoint_for_branch(
        &self,
        id: BranchId,
        timeline_id: &str,
        ep: &NewEndpoint<'_>,
        parent_protected_seen: bool,
    ) -> Result<bool, MetadataError> {
        self.inner
            .create_endpoint_for_branch(id, timeline_id, ep, parent_protected_seen)
            .await
    }

    async fn set_neon_tenant(&self, db: DatabaseId, tenant: &str) -> Result<bool, MetadataError> {
        self.inner.set_neon_tenant(db, tenant).await
    }

    async fn neon_endpoint(
        &self,
        endpoint_id: &str,
    ) -> Result<Option<NeonEndpoint>, MetadataError> {
        self.inner.neon_endpoint(endpoint_id).await
    }

    async fn neon_roles_versioned(
        &self,
        endpoint_id: &str,
    ) -> Result<(Vec<NeonRole>, i64), MetadataError> {
        self.inner.neon_roles_versioned(endpoint_id).await
    }

    async fn neon_databases(&self, endpoint_id: &str) -> Result<Vec<NeonDatabase>, MetadataError> {
        self.inner.neon_databases(endpoint_id).await
    }

    async fn set_branch_ready(
        &self,
        id: BranchId,
        sealed_password: &str,
        branched_at_lsn: Option<&str>,
        history_start: Option<DateTime<Utc>>,
    ) -> Result<bool, MetadataError> {
        let changed = self
            .inner
            .set_branch_ready(id, sealed_password, branched_at_lsn, history_start)
            .await?;
        if changed {
            self.branch_status(id, BranchStatus::Ready).await;
        }
        Ok(changed)
    }

    async fn set_failed(&self, id: DatabaseId, error: &str) -> Result<(), MetadataError> {
        // Ignored once a delete has begun: then it reports nothing.
        self.inner.set_failed(id, error).await?;
        match self.inner.get_database(id).await {
            Ok(Some(record)) if record.status == DatabaseStatus::Failed => {
                self.database_status(id, BranchStatus::Failed).await;
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(database_id = %id, "no realtime event: reading the database: {e}");
            }
        }
        Ok(())
    }

    async fn mark_deleted(&self, id: DatabaseId) -> Result<(), MetadataError> {
        // Read first: the row may say nothing once it's gone.
        let project = self.project_of_database(id).await;
        self.inner.mark_deleted(id).await?;
        self.forget_database(id);
        if let Some(project) = project {
            self.publish(
                project,
                ProjectEvent::DatabaseDeleted {
                    database_id: id.as_uuid(),
                },
            )
            .await;
        }
        Ok(())
    }

    async fn mark_branch_deleted(&self, id: BranchId) -> Result<(), MetadataError> {
        // Read first: the row may say nothing once it's gone, and a row that is already a
        // tombstone (a retried delete) was announced the first time.
        let tombstone = match self.inner.get_branch(id).await {
            Ok(Some(record)) => record.status == BranchStatus::Deleted,
            Ok(None) => true,
            Err(e) => {
                tracing::warn!(branch_id = %id, "no realtime event: reading the branch: {e}");
                true
            }
        };
        let ids = if tombstone {
            None
        } else {
            self.ids_of_branch(id).await
        };
        self.inner.mark_branch_deleted(id).await?;
        self.forget_branch(id);
        if let Some((database, project)) = ids {
            self.publish(
                project,
                ProjectEvent::BranchDeleted {
                    database_id: database.as_uuid(),
                    branch_id: id.as_uuid(),
                },
            )
            .await;
        }
        Ok(())
    }

    async fn delete_endpoint(&self, endpoint_id: &str) -> Result<(), MetadataError> {
        self.inner.delete_endpoint(endpoint_id).await
    }

    async fn create_static_endpoint(
        &self,
        ep: &NewStaticEndpoint<'_>,
    ) -> Result<(), MetadataError> {
        self.inner.create_static_endpoint(ep).await
    }

    async fn schema_base(
        &self,
        branch: BranchId,
        database_name: &str,
    ) -> Result<Option<SchemaBase>, MetadataError> {
        self.inner.schema_base(branch, database_name).await
    }

    async fn append_schema_base(&self, base: &SchemaBase) -> Result<i64, MetadataError> {
        self.inner.append_schema_base(base).await
    }

    async fn update_schema_base(&self, base: &SchemaBase) -> Result<bool, MetadataError> {
        self.inner.update_schema_base(base).await
    }

    async fn prune_schema_bases(
        &self,
        branch: BranchId,
        keep: &[String],
    ) -> Result<u64, MetadataError> {
        self.inner.prune_schema_bases(branch, keep).await
    }

    async fn replace_owner_secret(
        &self,
        id: BranchId,
        endpoint_id: &str,
        role: &str,
        verifier: &str,
        sealed: &str,
    ) -> Result<bool, MetadataError> {
        self.inner
            .replace_owner_secret(id, endpoint_id, role, verifier, sealed)
            .await
    }

    async fn clear_needs_reconfigure(
        &self,
        endpoint_id: &str,
        applied_version: i64,
    ) -> Result<bool, MetadataError> {
        self.inner
            .clear_needs_reconfigure(endpoint_id, applied_version)
            .await
    }

    async fn insert_api_role(
        &self,
        endpoint_id: &str,
        role: &NewApiRole<'_>,
        cap: u32,
    ) -> Result<InsertRoleOutcome, MetadataError> {
        self.inner.insert_api_role(endpoint_id, role, cap).await
    }

    async fn update_api_role(
        &self,
        endpoint_id: &str,
        name: &str,
        change: &ApiRoleChange,
        cap: u32,
    ) -> Result<RoleWriteOutcome, MetadataError> {
        self.inner
            .update_api_role(endpoint_id, name, change, cap)
            .await
    }

    async fn store_role_secret(
        &self,
        endpoint_id: &str,
        name: &str,
        verifier: &str,
        password_enc: &str,
        cap: u32,
    ) -> Result<RoleWriteOutcome, MetadataError> {
        self.inner
            .store_role_secret(endpoint_id, name, verifier, password_enc, cap)
            .await
    }

    async fn rename_role_copy(
        &self,
        endpoint_id: &str,
        branch: BranchId,
        old: &str,
        new: &str,
        resealed: Option<&str>,
        unreadable: Option<bool>,
    ) -> Result<RenameCopyOutcome, MetadataError> {
        self.inner
            .rename_role_copy(endpoint_id, branch, old, new, resealed, unreadable)
            .await
    }

    async fn replace_role_secret(
        &self,
        endpoint_id: &str,
        name: &str,
        verifier: &str,
        password_enc: &str,
    ) -> Result<bool, MetadataError> {
        self.inner
            .replace_role_secret(endpoint_id, name, verifier, password_enc)
            .await
    }

    async fn confirm_role(
        &self,
        endpoint_id: &str,
        name: &str,
        version: i64,
        attributes_change: Option<Uuid>,
    ) -> Result<bool, MetadataError> {
        self.inner
            .confirm_role(endpoint_id, name, version, attributes_change)
            .await
    }

    async fn delete_role_copy(&self, endpoint_id: &str, name: &str) -> Result<bool, MetadataError> {
        self.inner.delete_role_copy(endpoint_id, name).await
    }

    async fn role_copies(&self, endpoint_id: &str) -> Result<(Vec<RoleCopy>, i64), MetadataError> {
        self.inner.role_copies(endpoint_id).await
    }

    async fn apply_resync(
        &self,
        endpoint_id: &str,
        branch: BranchId,
        read_version: i64,
        write: &ResyncWrite,
    ) -> Result<ResyncApplied, MetadataError> {
        self.inner
            .apply_resync(endpoint_id, branch, read_version, write)
            .await
    }

    async fn clear_forwarded(&self, endpoint_id: &str) -> Result<(), MetadataError> {
        self.inner.clear_forwarded(endpoint_id).await
    }

    async fn mark_password_unreadable(
        &self,
        endpoint_id: &str,
        name: &str,
    ) -> Result<(), MetadataError> {
        self.inner.mark_password_unreadable(endpoint_id, name).await
    }

    async fn set_compute_settings_version(
        &self,
        endpoint_id: &str,
        version: i32,
    ) -> Result<(), MetadataError> {
        self.inner
            .set_compute_settings_version(endpoint_id, version)
            .await
    }

    async fn replace_compute_token(
        &self,
        endpoint_id: &str,
        sha256_hex: &str,
    ) -> Result<(), MetadataError> {
        self.inner
            .replace_compute_token(endpoint_id, sha256_hex)
            .await
    }

    async fn fail_if_stale(
        &self,
        id: DatabaseId,
        max_age_secs: i64,
    ) -> Result<bool, MetadataError> {
        let failed = self.inner.fail_if_stale(id, max_age_secs).await?;
        if failed {
            self.database_status(id, BranchStatus::Failed).await;
        }
        Ok(failed)
    }

    async fn list_live_for_reconcile(&self) -> Result<Vec<LiveDatabase>, MetadataError> {
        self.inner.list_live_for_reconcile().await
    }

    async fn list_endpoints_for_reconcile(&self) -> Result<Vec<EndpointState>, MetadataError> {
        self.inner.list_endpoints_for_reconcile().await
    }

    async fn set_branch_status(
        &self,
        id: BranchId,
        from: &[BranchStatus],
        to: BranchStatus,
    ) -> Result<bool, MetadataError> {
        let changed = self.inner.set_branch_status(id, from, to).await?;
        if changed {
            self.branch_status(id, to).await;
        }
        Ok(changed)
    }

    async fn set_branch_failed(&self, id: BranchId, error: &str) -> Result<(), MetadataError> {
        // Ignored once a delete has begun: then it reports nothing.
        self.inner.set_branch_failed(id, error).await?;
        self.branch_status_as_stored(id, Some(BranchStatus::Failed))
            .await;
        Ok(())
    }

    async fn set_branch_error(
        &self,
        id: BranchId,
        error: Option<&str>,
    ) -> Result<(), MetadataError> {
        self.inner.set_branch_error(id, error).await?;
        self.branch_status_as_stored(id, None).await;
        Ok(())
    }

    async fn begin_branch_delete(
        &self,
        id: BranchId,
        except: Option<Uuid>,
    ) -> Result<BranchDeleteOutcome, MetadataError> {
        // The write moves the branch's live backups along: read which are not deleting yet.
        let backups: Vec<BranchId> = if self.is_quiet(id) {
            Vec::new()
        } else {
            match self.inner.live_backups_of(id).await {
                Ok(backups) => backups
                    .into_iter()
                    .filter(|b| b.status != BranchStatus::Deleting)
                    .map(|b| b.id)
                    .collect(),
                Err(e) => {
                    tracing::warn!(branch_id = %id, "no realtime event for its backups: reading them: {e}");
                    Vec::new()
                }
            }
        };
        let outcome = self.inner.begin_branch_delete(id, except).await?;
        if outcome == BranchDeleteOutcome::Started {
            self.branch_status(id, BranchStatus::Deleting).await;
            for backup in backups {
                self.branch_status_as_stored(backup, Some(BranchStatus::Deleting))
                    .await;
            }
        }
        Ok(outcome)
    }

    async fn claim_branch_for_switch(
        &self,
        id: BranchId,
        allow_protected: bool,
        except: Option<Uuid>,
    ) -> Result<ClaimOutcome, MetadataError> {
        let outcome = self
            .inner
            .claim_branch_for_switch(id, allow_protected, except)
            .await?;
        if outcome == ClaimOutcome::Claimed {
            self.branch_status(id, BranchStatus::Resetting).await;
        }
        Ok(outcome)
    }

    async fn live_children(&self, id: BranchId) -> Result<Vec<BranchRecord>, MetadataError> {
        self.inner.live_children(id).await
    }

    async fn live_backups_of(&self, id: BranchId) -> Result<Vec<BranchRecord>, MetadataError> {
        self.inner.live_backups_of(id).await
    }

    async fn set_pending_timeline(
        &self,
        id: BranchId,
        timeline_id: Option<&str>,
    ) -> Result<bool, MetadataError> {
        self.inner.set_pending_timeline(id, timeline_id).await
    }

    async fn switch_to_pending_timeline(
        &self,
        id: BranchId,
        expected_pending: &str,
        history_start: Option<DateTime<Utc>>,
        branched_at_lsn: &str,
    ) -> Result<bool, MetadataError> {
        self.inner
            .switch_to_pending_timeline(id, expected_pending, history_start, branched_at_lsn)
            .await
    }

    async fn switch_to_restored_timeline(
        &self,
        id: BranchId,
        expected_pending: &str,
        backup: BranchId,
        backup_name: &str,
        restore_lsn: &str,
    ) -> Result<bool, MetadataError> {
        let switched = self
            .inner
            .switch_to_restored_timeline(id, expected_pending, backup, backup_name, restore_lsn)
            .await?;
        if switched && let Some((database, project)) = self.ids_of_branch(id).await {
            // The backup shares its branch's database.
            self.remember(backup, database, project);
            self.publish(
                project,
                ProjectEvent::BranchCreated {
                    database_id: database.as_uuid(),
                    branch_id: backup.as_uuid(),
                },
            )
            .await;
        }
        Ok(switched)
    }

    async fn clear_superseded_timeline(&self, id: BranchId) -> Result<(), MetadataError> {
        self.inner.clear_superseded_timeline(id).await
    }

    async fn swap_pending_timeline(
        &self,
        id: BranchId,
        expected: Option<&str>,
        new: Option<&str>,
    ) -> Result<bool, MetadataError> {
        self.inner.swap_pending_timeline(id, expected, new).await
    }

    async fn claim_stale_reset(
        &self,
        id: BranchId,
        max_age_secs: i64,
    ) -> Result<bool, MetadataError> {
        let claimed = self.inner.claim_stale_reset(id, max_age_secs).await?;
        if claimed {
            self.branch_status(id, BranchStatus::Resetting).await;
        }
        Ok(claimed)
    }

    async fn fail_stuck_restore(
        &self,
        id: BranchId,
        max_age_secs: i64,
        error: &str,
    ) -> Result<bool, MetadataError> {
        let failed = self
            .inner
            .fail_stuck_restore(id, max_age_secs, error)
            .await?;
        if failed {
            self.branch_status(id, BranchStatus::Failed).await;
        }
        Ok(failed)
    }

    async fn fail_branch_if_stale(
        &self,
        id: BranchId,
        max_age_secs: i64,
    ) -> Result<bool, MetadataError> {
        let failed = self.inner.fail_branch_if_stale(id, max_age_secs).await?;
        if failed {
            self.branch_status(id, BranchStatus::Failed).await;
        }
        Ok(failed)
    }

    async fn database_settings(
        &self,
        db: DatabaseId,
    ) -> Result<Option<DatabaseSettings>, MetadataError> {
        self.inner.database_settings(db).await
    }

    async fn list_live_branches_for_reconcile(&self) -> Result<Vec<LiveBranch>, MetadataError> {
        self.inner.list_live_branches_for_reconcile().await
    }

    async fn insert_data_key(&self, key: &NewDataKey) -> Result<(), MetadataError> {
        self.inner.insert_data_key(key).await
    }

    async fn list_data_keys(&self, branch: BranchId) -> Result<Vec<DataKeyRecord>, MetadataError> {
        self.inner.list_data_keys(branch).await
    }

    async fn fix_key_roles(
        &self,
        branch: BranchId,
        fixes: &[chelabase_metadata::KeyRoleFix],
    ) -> Result<u64, MetadataError> {
        self.inner.fix_key_roles(branch, fixes).await
    }

    async fn has_live_public_key(&self, branch: BranchId) -> Result<bool, MetadataError> {
        self.inner.has_live_public_key(branch).await
    }

    async fn set_authenticator_password(
        &self,
        branch: BranchId,
        sealed: &str,
    ) -> Result<(), MetadataError> {
        self.inner.set_authenticator_password(branch, sealed).await
    }

    async fn branch_databases(
        &self,
        branch: BranchId,
    ) -> Result<Vec<BranchDatabase>, MetadataError> {
        self.inner.branch_databases(branch).await
    }

    async fn served_databases(
        &self,
        branch: BranchId,
    ) -> Result<Vec<ServedDatabase>, MetadataError> {
        self.inner.served_databases(branch).await
    }

    async fn list_members(&self, tenant: TenantId) -> Result<Vec<MemberRecord>, MetadataError> {
        self.inner.list_members(tenant).await
    }

    async fn database_grant_rows(
        &self,
        project: Uuid,
        database: Uuid,
    ) -> Result<Vec<(Uuid, GrantRows)>, MetadataError> {
        self.inner.database_grant_rows(project, database).await
    }

    async fn member_role_branches(&self) -> Result<Vec<Uuid>, MetadataError> {
        self.inner.member_role_branches().await
    }

    async fn delete_branch_member_roles(&self, branch: Uuid) -> Result<u64, MetadataError> {
        self.inner.delete_branch_member_roles(branch).await
    }

    async fn member_branch_roles(
        &self,
        branch: Uuid,
    ) -> Result<Vec<MemberBranchRole>, MetadataError> {
        self.inner.member_branch_roles(branch).await
    }

    async fn guard_exemptions(&self, branch: BranchId) -> Result<Vec<String>, MetadataError> {
        self.inner.guard_exemptions(branch).await
    }

    async fn upsert_member_branch_role(&self, row: &MemberBranchRole) -> Result<(), MetadataError> {
        self.inner.upsert_member_branch_role(row).await
    }

    async fn member_acted_roles(&self, branch: Uuid) -> Result<Vec<(Uuid, String)>, MetadataError> {
        self.inner.member_acted_roles(branch).await
    }

    async fn delete_member_branch_role(
        &self,
        member: Uuid,
        branch: Uuid,
    ) -> Result<(), MetadataError> {
        self.inner.delete_member_branch_role(member, branch).await
    }

    async fn finish_enable_data_api(
        &self,
        branch: BranchId,
        name: &str,
        outcome: Result<(), String>,
    ) -> Result<bool, MetadataError> {
        self.inner
            .finish_enable_data_api(branch, name, outcome)
            .await
    }

    async fn set_data_api_off(&self, branch: BranchId, name: &str) -> Result<bool, MetadataError> {
        self.inner.set_data_api_off(branch, name).await
    }

    async fn copy_parent_database_flags(&self, child: BranchId) -> Result<(), MetadataError> {
        self.inner.copy_parent_database_flags(child).await
    }

    async fn enabling_databases(&self) -> Result<Vec<(BranchId, String)>, MetadataError> {
        self.inner.enabling_databases().await
    }

    async fn branches_missing_data_api(&self) -> Result<Vec<BranchId>, MetadataError> {
        self.inner.branches_missing_data_api().await
    }

    async fn branch_connection(
        &self,
        branch: BranchId,
    ) -> Result<Option<BranchConnectionRow>, MetadataError> {
        self.inner.branch_connection(branch).await
    }

    async fn deleting_projects(
        &self,
        older_than: Duration,
    ) -> Result<Vec<ProjectId>, MetadataError> {
        self.inner.deleting_projects(older_than).await
    }

    async fn app_users_in_transition(
        &self,
        older_than: Duration,
    ) -> Result<Vec<ProjectAuth>, MetadataError> {
        self.inner.app_users_in_transition(older_than).await
    }

    async fn project_by_id(&self, id: ProjectId) -> Result<Option<ProjectRecord>, MetadataError> {
        self.inner.project_by_id(id).await
    }

    async fn project_databases(
        &self,
        id: ProjectId,
    ) -> Result<Vec<(DatabaseId, DatabaseStatus, Option<String>)>, MetadataError> {
        self.inner.project_databases(id).await
    }

    async fn set_project_error(
        &self,
        id: ProjectId,
        error: Option<&str>,
    ) -> Result<(), MetadataError> {
        self.inner.set_project_error(id, error).await
    }

    async fn finish_project_delete(&self, id: ProjectId) -> Result<(), MetadataError> {
        self.inner.finish_project_delete(id).await
    }

    async fn project_auth(&self, project: ProjectId) -> Result<Option<ProjectAuth>, MetadataError> {
        self.inner.project_auth(project).await
    }

    async fn app_auth_for_database(
        &self,
        db: DatabaseId,
    ) -> Result<Option<ProjectAuth>, MetadataError> {
        self.inner.app_auth_for_database(db).await
    }

    async fn set_app_users_org(
        &self,
        project: ProjectId,
        org_id: &str,
    ) -> Result<(), MetadataError> {
        self.inner.set_app_users_org(project, org_id).await
    }

    async fn finish_enable_app_users(
        &self,
        project: ProjectId,
        outcome: Result<(), String>,
    ) -> Result<bool, MetadataError> {
        self.inner.finish_enable_app_users(project, outcome).await
    }

    async fn delete_project_auth(&self, project: ProjectId) -> Result<(), MetadataError> {
        self.inner.delete_project_auth(project).await
    }

    async fn enabled_app_projects(&self) -> Result<Vec<ProjectAuth>, MetadataError> {
        self.inner.enabled_app_projects().await
    }

    async fn branch_auth_sync(
        &self,
        branch: BranchId,
    ) -> Result<Option<BranchAuthSync>, MetadataError> {
        self.inner.branch_auth_sync(branch).await
    }

    async fn set_branch_auth_sync(
        &self,
        branch: BranchId,
        pg_database: &str,
        sealed: &str,
    ) -> Result<(), MetadataError> {
        self.inner
            .set_branch_auth_sync(branch, pg_database, sealed)
            .await
    }

    async fn claim_branch_auth_sync(
        &self,
        branch: BranchId,
        pg_database: &str,
        sealed: &str,
    ) -> Result<String, MetadataError> {
        self.inner
            .claim_branch_auth_sync(branch, pg_database, sealed)
            .await
    }

    async fn swap_branch_auth_sync(
        &self,
        branch: BranchId,
        pg_database: &str,
        expected: &str,
        sealed: &str,
    ) -> Result<String, MetadataError> {
        self.inner
            .swap_branch_auth_sync(branch, pg_database, expected, sealed)
            .await
    }

    async fn revoke_user_sessions(
        &self,
        project: ProjectId,
        user: Uuid,
        keep: Option<Uuid>,
        created_before: Option<DateTime<Utc>>,
    ) -> Result<Vec<AppSession>, MetadataError> {
        self.inner
            .revoke_user_sessions(project, user, keep, created_before)
            .await
    }

    async fn revoke_project_sessions(
        &self,
        project: ProjectId,
    ) -> Result<Vec<AppSession>, MetadataError> {
        self.inner.revoke_project_sessions(project).await
    }

    async fn purge_app_sessions(&self, older_than: Duration) -> Result<u64, MetadataError> {
        self.inner.purge_app_sessions(older_than).await
    }

    async fn merge(&self, id: Uuid) -> Result<Option<MergeRecord>, MetadataError> {
        self.inner.merge(id).await
    }

    async fn set_merge_status(
        &self,
        id: Uuid,
        from: &[MergeStatus],
        to: MergeStatus,
        patch: MergeStatusPatch,
    ) -> Result<bool, MetadataError> {
        self.inner.set_merge_status(id, from, to, patch).await
    }

    async fn touch_merge_claim(
        &self,
        id: Uuid,
        status: MergeStatus,
    ) -> Result<bool, MetadataError> {
        self.inner.touch_merge_claim(id, status).await
    }

    async fn append_merge_event(&self, event: &MergeEvent) -> Result<(), MetadataError> {
        self.inner.append_merge_event(event).await
    }

    async fn merge_events(&self, id: Uuid) -> Result<Vec<MergeEvent>, MetadataError> {
        self.inner.merge_events(id).await
    }

    async fn create_rehearsal_branch(
        &self,
        database_id: DatabaseId,
        parent: BranchId,
        name: &str,
    ) -> Result<BranchRecord, MetadataError> {
        let record = self
            .inner
            .create_rehearsal_branch(database_id, parent, name)
            .await?;
        self.hush(record.id);
        Ok(record)
    }

    async fn stale_merge_claims(
        &self,
        older_than: Duration,
        rehand_after: Duration,
    ) -> Result<Vec<MergeRecord>, MetadataError> {
        self.inner
            .stale_merge_claims(older_than, rehand_after)
            .await
    }

    async fn stale_finish_requests(
        &self,
        older_than: Duration,
        rehand_after: Duration,
    ) -> Result<Vec<MergeRecord>, MetadataError> {
        self.inner
            .stale_finish_requests(older_than, rehand_after)
            .await
    }

    async fn mark_merge_handed_back(
        &self,
        id: Uuid,
        status: MergeStatus,
    ) -> Result<bool, MetadataError> {
        self.inner.mark_merge_handed_back(id, status).await
    }

    async fn cancel_drafts_for_branch(&self, branch: BranchId) -> Result<u64, MetadataError> {
        self.inner.cancel_drafts_for_branch(branch).await
    }

    async fn list_merges(
        &self,
        database_id: DatabaseId,
        branch: Option<BranchId>,
    ) -> Result<Vec<MergeRecord>, MetadataError> {
        self.inner.list_merges(database_id, branch).await
    }
}
