use axum::{
    extract::{FromRequestParts, State},
    http::{header::CACHE_CONTROL, request::Parts, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use chelabase_auth::internal::{InternalVerifier, IDENTITY_HEADER};
use chelabase_routes::{orchestrator as routes, Service};
use chelabase_metadata::{
    AppSession, AppUsersStatus, BeginDelete, BeginEnable, BeginProjectDelete, BranchAuthSync, BranchDatabase, BranchDeleteOutcome, BranchKind, BranchRecord, BranchStatus, ClaimOutcome, CreateBranchOutcome, DatabaseRecord, DatabaseStatus, EnableStart,
    DataKeyRecord, MetadataClient, MetadataError, NewBranch, NewDataKey, ProjectOverview,
    NewProjectAuth, OnDelete, PasswordState, ProjectAuth, ProjectRecord, RenameProject, RoleRow, SavedQuery, SavedQueryEditor, SavedQueryPatch, SavedQuerySummary, SavedQueryWrite, SettingsPatch, TenantOverview,
    MergeCreate, MergeDraftPatch, MergeEvent, MergeRecord, MergeStatus, MergeStatusPatch, MergeWrite, NewMerge,
};
use chelabase_nats::{
    AppUsersCheckReply, AppUsersCheckRequest, AppUsersJob, AppUsersSyncJob, BranchJob, CheckDataRoleReply, MergeJob, CheckDataRoleRequest, DataApiDatabaseRequest, DatabaseSettingsJob, DeleteDatabaseJob,
    JetStreamContext, JobEnvelope,
    ProjectDeleteJob, ProvisionDatabaseJob, ResetPasswordReply, ResetPasswordRequest, RESET_NOT_APPLIED,
    RoleCreateRequest, RoleReply, RoleRequest, RoleSessionsReply, RoleUpdateRequest, SUBJECT_CHECK_DATA_ROLE, SUBJECT_DATA_API_DISABLE,
    SUBJECT_DATA_API_ENABLE, SUBJECT_DATA_API_REBOOTSTRAP, SUBJECT_RESET_PASSWORD,
    SUBJECT_APP_USERS_CHECK, SUBJECT_ROLE_CREATE, SUBJECT_ROLE_DELETE, SUBJECT_ROLE_RESET, SUBJECT_ROLE_SESSIONS, SUBJECT_ROLE_UPDATE,
};
use chelabase_secrets::{open_credential, open_role_password, Purpose, SecretKey};
use chelabase_types::api_error::{ApiError, ErrorCode};
use chelabase_metadata::members::{Admission, GrantRow, GrantRows, MemberRecord, MemberStatus, OverrideRow};
use chelabase_types::mode_policy::ModePolicy;
use chelabase_types::{ApiJson, ApiPath, BranchId, DatabaseId, ProjectId, TenantId};
use chelabase_types::api_error::ErrorEnvelope;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use tracing::{error, info};
use uuid::Uuid;

pub mod access;
mod branches;
mod console;
mod data_api_databases;
mod app_user_admin;
mod app_users;
mod data_keys;
pub mod internal;
pub mod members;
mod merges;
mod openapi;
pub mod projects;
mod roles;
pub mod sql_editor;
pub use console::{AddrFuture, Console, ConsoleRequest, OwnerPoolKey, PostgresAddr, RestReply};
pub use openapi::openapi;
use access::{authorize, authorize_database, Target};

/// The orchestrator's log filter when `RUST_LOG` is unset.
pub const DEFAULT_LOG_FILTER: &str = "orchestrator=info,tower_http=info,chelabase_auth=info";

/// Port the Neon proxy serves the Postgres protocol on (default of `CHELABASE_PROXY_PG_PORT`).
pub const PROXY_PG_PORT: u16 = 4432;
/// Port the Neon proxy serves SQL-over-HTTP on (default of `CHELABASE_PROXY_HTTP_PORT`).
pub const PROXY_HTTP_PORT: u16 = 4444;

/// Where clients reach the Neon proxy: `<ep>.<domain>` on these ports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyAddress {
    pub domain: String,
    pub pg_port: u16,
    pub http_port: u16,
}

impl ProxyAddress {
    /// `domain` on the default ports.
    pub fn local(domain: &str) -> Self {
        Self {
            domain: domain.to_string(),
            pg_port: PROXY_PG_PORT,
            http_port: PROXY_HTTP_PORT,
        }
    }

    /// `CHELABASE_PROXY_DOMAIN` (default `localtest.me`), `CHELABASE_PROXY_PG_PORT` and
    /// `CHELABASE_PROXY_HTTP_PORT` (defaults 4432 and 4444). Blank values count as unset.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let value = |name: &str| {
            get(name)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let port = |name: &str, default: u16| -> anyhow::Result<u16> {
            match value(name) {
                None => Ok(default),
                Some(v) => v
                    .parse::<u16>()
                    .ok()
                    .filter(|p| *p != 0)
                    .ok_or_else(|| anyhow::anyhow!("{name}={v:?} is not a port (1-65535)")),
            }
        };
        Ok(Self {
            domain: value("CHELABASE_PROXY_DOMAIN").unwrap_or_else(|| "localtest.me".to_string()),
            pg_port: port("CHELABASE_PROXY_PG_PORT", PROXY_PG_PORT)?,
            http_port: port("CHELABASE_PROXY_HTTP_PORT", PROXY_HTTP_PORT)?,
        })
    }
}

/// The proxy's host name for an endpoint.
fn proxy_host(endpoint_id: &str, domain: &str) -> String {
    format!("{endpoint_id}.{domain}")
}

/// Abstraction over tenant/database metadata lookups, so tests can inject an
/// in-memory fake instead of a real Postgres-backed `MetadataClient`. The member API's reads
/// and writes are [`members::MembersPort`].
pub trait MetadataPort: members::MembersPort + Send + Sync + 'static {
    fn get_tenant_by_zitadel_org_id(
        &self,
        zitadel_org_id: &str,
    ) -> impl Future<Output = Result<Option<TenantId>, MetadataError>> + Send;

    fn insert_database_pending(
        &self,
        database_id: DatabaseId,
        tenant_id: TenantId,
        project_id: ProjectId,
        name: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;

    fn set_failed(
        &self,
        database_id: DatabaseId,
        error: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;

    fn get_database(
        &self,
        database_id: DatabaseId,
    ) -> impl Future<Output = Result<Option<DatabaseRecord>, MetadataError>> + Send;

    fn tenant_overview(
        &self,
        tenant_id: TenantId,
    ) -> impl Future<Output = Result<Option<TenantOverview>, MetadataError>> + Send;

    /// A branch; a database's main has the database's id and holds its endpoint and password.
    fn get_branch(
        &self,
        branch_id: BranchId,
    ) -> impl Future<Output = Result<Option<BranchRecord>, MetadataError>> + Send;

    /// Move to `deleting`; `NotReady` when already deleting, deleted, or missing, `Protected`
    /// while a live branch is protected.
    fn begin_delete(
        &self,
        database_id: DatabaseId,
    ) -> impl Future<Output = Result<BeginDelete, MetadataError>> + Send;

    /// The database's branches that aren't deleted, oldest first (backups included).
    fn list_branches(
        &self,
        database_id: DatabaseId,
    ) -> impl Future<Output = Result<Vec<BranchRecord>, MetadataError>> + Send;

    /// Main of each of the tenant's databases with the database's live `normal` branch count
    /// (main included), in one query.
    fn main_branches_with_counts(
        &self,
        tenant_id: TenantId,
    ) -> impl Future<Output = Result<Vec<(BranchRecord, i64)>, MetadataError>> + Send;

    /// Inserts a pending branch, enforcing the name, the cap and a ready parent.
    fn create_branch(
        &self,
        branch: &NewBranch<'_>,
    ) -> impl Future<Output = Result<CreateBranchOutcome, MetadataError>> + Send;

    /// Live `normal` children (backups excluded).
    fn live_children(
        &self,
        branch_id: BranchId,
    ) -> impl Future<Output = Result<Vec<BranchRecord>, MetadataError>> + Send;

    /// Guarded status change; true when it happened.
    fn set_branch_status(
        &self,
        branch_id: BranchId,
        from: &[BranchStatus],
        to: BranchStatus,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;

    /// ready → resetting, clearing the last error. With `allow_protected = false` a protected
    /// branch isn't claimed (`Protected`).
    fn claim_branch_for_switch(
        &self,
        branch_id: BranchId,
        allow_protected: bool,
    ) -> impl Future<Output = Result<ClaimOutcome, MetadataError>> + Send;

    /// Sets a `normal` branch's protected flag; `None` for an unknown, backup or deleting
    /// branch.
    fn set_branch_protected(
        &self,
        branch_id: BranchId,
        protected: bool,
    ) -> impl Future<Output = Result<Option<BranchRecord>, MetadataError>> + Send;

    fn set_branch_failed(
        &self,
        branch_id: BranchId,
        error: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;

    /// Move the branch and its backups to `deleting`; refused while the branch or one of its
    /// backups has live `normal` children.
    fn begin_branch_delete(
        &self,
        branch_id: BranchId,
    ) -> impl Future<Output = Result<BranchDeleteOutcome, MetadataError>> + Send;

    /// Changes the given settings; false once the database is deleting, or missing.
    fn update_database_settings(
        &self,
        database_id: DatabaseId,
        patch: &SettingsPatch,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;

    /// Stores a key; a second live public key on the branch is `MetadataError::Conflict`.
    fn insert_data_key(
        &self,
        key: &NewDataKey,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;

    /// Stores a secret key bound to the role OID the worker's check found, under the
    /// endpoint's lock (a DDL DROP or rename either sees the key or ran first).
    fn create_secret_key(
        &self,
        key: &NewDataKey,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;

    /// The branch's live keys, newest first.
    fn list_data_keys(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<Vec<DataKeyRecord>, MetadataError>> + Send;

    /// Revokes a live secret key; false for an unknown or revoked key, and for the public key.
    fn revoke_data_key(
        &self,
        branch: BranchId,
        key_id: Uuid,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;

    /// Revokes `old` and stores `new` in one step; false when `old` isn't a live key of the
    /// branch.
    fn rotate_data_key(
        &self,
        branch: BranchId,
        old: Uuid,
        new: &NewDataKey,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;

    /// [`Self::rotate_data_key`] for a secret key, under the endpoint's lock like
    /// [`Self::create_secret_key`]; the new key takes the old row's current role and OID.
    /// `MetadataError::RoleRecreated` when `new.role_oid` (the check's) differs from the old
    /// key's.
    fn rotate_secret_key(
        &self,
        branch: BranchId,
        old: Uuid,
        new: &NewDataKey,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;

    /// The endpoint's roles, the owner first.
    fn list_roles(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<Vec<RoleRow>, MetadataError>> + Send;

    /// The member roles the reconciler recorded on a branch (one per member, R17).
    fn member_branch_roles(
        &self,
        branch: Uuid,
    ) -> impl Future<Output = Result<Vec<chelabase_metadata::members::MemberBranchRole>, MetadataError>>
           + Send;

    /// R31: records `role` as the last role `member` acted as on `branch` (an act-as run is
    /// starting); false when the member has no member role row there.
    fn record_member_acted_role(
        &self,
        member: Uuid,
        branch: Uuid,
        role: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;

    /// A non-owner role's sealed password; `None` when there's no such row or nothing sealed.
    fn role_password(
        &self,
        endpoint_id: &str,
        name: &str,
    ) -> impl Future<Output = Result<Option<String>, MetadataError>> + Send;

    /// The branch's Postgres databases: the main one first, then by name.
    fn branch_databases(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<Vec<BranchDatabase>, MetadataError>> + Send;

    /// Of `ids`, the branches with a `main` database row, in one query.
    fn branches_with_main_database(
        &self,
        ids: &[BranchId],
    ) -> impl Future<Output = Result<HashSet<BranchId>, MetadataError>> + Send;

    /// Claims a database of the branch for the data API: its row becomes `enabling`.
    fn begin_enable_data_api(
        &self,
        branch: BranchId,
        name: &str,
        cap: u32,
    ) -> impl Future<Output = Result<EnableStart, MetadataError>> + Send;

    /// Stores the schema requests to a database use when they name none; false for an unknown
    /// database.
    fn set_database_default_schema(
        &self,
        branch: BranchId,
        name: &str,
        schema: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;

    /// Replaces a live key's database list (`None` is every served database); false for an
    /// unknown or revoked key.
    fn set_key_databases(
        &self,
        branch: BranchId,
        key_id: Uuid,
        databases: Option<Vec<String>>,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;

    /// The tenant's projects that aren't deleted, oldest first.
    fn list_projects(
        &self,
        tenant: TenantId,
    ) -> impl Future<Output = Result<Vec<ProjectRecord>, MetadataError>> + Send;

    /// `None` for another tenant's project and for a deleted one.
    fn get_project(
        &self,
        tenant: TenantId,
        id: ProjectId,
    ) -> impl Future<Output = Result<Option<ProjectRecord>, MetadataError>> + Send;

    /// Any tenant's project that isn't deleted.
    fn project_by_id(
        &self,
        id: ProjectId,
    ) -> impl Future<Output = Result<Option<ProjectRecord>, MetadataError>> + Send;

    /// Creates a project with the first free slug.
    fn create_project(
        &self,
        tenant: TenantId,
        name: &str,
    ) -> impl Future<Output = Result<ProjectRecord, MetadataError>> + Send;

    /// Changes the name only; the slug stays.
    fn rename_project(
        &self,
        tenant: TenantId,
        id: ProjectId,
        name: &str,
    ) -> impl Future<Output = Result<RenameProject, MetadataError>> + Send;

    /// Starts deleting a project and its ready or failed databases.
    fn begin_project_delete(
        &self,
        tenant: TenantId,
        id: ProjectId,
    ) -> impl Future<Output = Result<BeginProjectDelete, MetadataError>> + Send;

    /// The project's databases that aren't deleted, with their last error.
    fn project_databases(
        &self,
        id: ProjectId,
    ) -> impl Future<Output = Result<Vec<(DatabaseId, DatabaseStatus, Option<String>)>, MetadataError>> + Send;

    /// The project's app-users settings, if any.
    fn project_auth(
        &self,
        project: ProjectId,
    ) -> impl Future<Output = Result<Option<ProjectAuth>, MetadataError>> + Send;

    /// The app-users status of each of the tenant's projects that has settings, in one query.
    fn project_auth_statuses(
        &self,
        tenant: TenantId,
    ) -> impl Future<Output = Result<HashMap<ProjectId, AppUsersStatus>, MetadataError>> + Send;

    /// Starts enabling app users for a project.
    fn begin_enable_app_users(
        &self,
        new: &NewProjectAuth<'_>,
    ) -> impl Future<Output = Result<BeginEnable, MetadataError>> + Send;

    /// Changes the given app-users settings; the row, or `None` without one.
    fn update_app_users_settings(
        &self,
        project: ProjectId,
        email_verification: Option<bool>,
        on_delete: Option<OnDelete>,
    ) -> impl Future<Output = Result<Option<ProjectAuth>, MetadataError>> + Send;

    /// Moves `enabled` or `failed` to `disabling`; true when it moved.
    fn begin_disable_app_users(
        &self,
        project: ProjectId,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;

    /// The branch's sync-role record, if any.
    fn branch_auth_sync(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<Option<BranchAuthSync>, MetadataError>> + Send;

    /// The user's sessions that aren't revoked, oldest first.
    fn list_user_sessions(
        &self,
        project: ProjectId,
        user: Uuid,
    ) -> impl Future<Output = Result<Vec<AppSession>, MetadataError>> + Send;

    /// Revokes the user's live sessions except `keep` (and only those created before `created_before` when given); returns them.
    fn revoke_user_sessions(
        &self,
        project: ProjectId,
        user: Uuid,
        keep: Option<Uuid>,
        created_before: Option<chrono::DateTime<chrono::Utc>>,
    ) -> impl Future<Output = Result<Vec<AppSession>, MetadataError>> + Send;

    /// The `project_auth` row whose auth database is `db`, if any.
    fn app_auth_for_database(
        &self,
        db: DatabaseId,
    ) -> impl Future<Output = Result<Option<ProjectAuth>, MetadataError>> + Send;

    fn list_saved_queries(
        &self,
        database: Uuid,
    ) -> impl Future<Output = Result<Vec<SavedQuerySummary>, MetadataError>> + Send;

    fn get_saved_query(
        &self,
        database: Uuid,
        id: Uuid,
    ) -> impl Future<Output = Result<Option<SavedQuery>, MetadataError>> + Send;

    #[allow(clippy::too_many_arguments)]
    fn create_saved_query(
        &self,
        database: Uuid,
        user: &str,
        author: Option<Uuid>,
        id: Uuid,
        name: &str,
        sql: &str,
        pg_database: Option<&str>,
        role: Option<&str>,
    ) -> impl Future<Output = Result<SavedQueryWrite, MetadataError>> + Send;

    fn update_saved_query(
        &self,
        database: Uuid,
        by: SavedQueryEditor,
        id: Uuid,
        version: i32,
        patch: &SavedQueryPatch,
    ) -> impl Future<Output = Result<SavedQueryWrite, MetadataError>> + Send;

    fn delete_saved_query(
        &self,
        database: Uuid,
        by: SavedQueryEditor,
        id: Uuid,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// Inserts a `computing` merge, or says which merge is in the way.
    fn create_merge(
        &self,
        merge: &NewMerge,
    ) -> impl Future<Output = Result<MergeCreate, MetadataError>> + Send;

    fn get_merge(
        &self,
        id: Uuid,
    ) -> impl Future<Output = Result<Option<MergeRecord>, MetadataError>> + Send;

    /// The database's merges, newest first; with `branch`, those where it is source or target.
    fn list_merges(
        &self,
        database_id: DatabaseId,
        branch: Option<BranchId>,
    ) -> impl Future<Output = Result<Vec<MergeRecord>, MetadataError>> + Send;

    /// Saves a draft's edits if `expected_version` is current (see `MergeDraftPatch`).
    fn update_merge_draft(
        &self,
        id: Uuid,
        expected_version: i64,
        patch: &MergeDraftPatch,
    ) -> impl Future<Output = Result<MergeWrite, MetadataError>> + Send;

    /// Guarded status change; true when it happened.
    fn set_merge_status(
        &self,
        id: Uuid,
        from: &[MergeStatus],
        to: MergeStatus,
        patch: MergeStatusPatch,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;

    fn merge_events(
        &self,
        id: Uuid,
    ) -> impl Future<Output = Result<Vec<MergeEvent>, MetadataError>> + Send;

    /// Cancels the branch's merges that nothing is running; how many.
    fn cancel_drafts_for_branch(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<u64, MetadataError>> + Send;

    /// A merge holding the branch as source or target, or a live rehearsal of it (T14 F3).
    fn branch_has_busy_merge(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<Option<chelabase_metadata::BranchBusy>, MetadataError>> + Send;

    /// The tenant's member with this Zitadel user id, whatever its status.
    fn member_by_zitadel(
        &self,
        tenant: TenantId,
        zitadel_user_id: &str,
    ) -> impl Future<Output = Result<Option<MemberRecord>, MetadataError>> + Send;

    /// The caller's member row, made (or an invited one activated) on its first request, with
    /// the email and name refreshed from the claims; a removed or pending member comes back as
    /// is. A person with no row becomes what `admission` says, unless the tenant has no active
    /// admin yet (then they claim it): `Closed` writes nothing and is `NotAdmitted` (R39).
    fn ensure_member(
        &self,
        tenant: TenantId,
        zitadel_user_id: &str,
        email: &str,
        name: &str,
        admission: Admission,
    ) -> impl Future<Output = Result<MemberRecord, MetadataError>> + Send;

    /// Every stored grant level for one (member, project, Chelabase database).
    fn grant_rows(
        &self,
        member: Uuid,
        project: Uuid,
        database: Option<Uuid>,
    ) -> impl Future<Output = Result<GrantRows, MetadataError>> + Send;

    /// The member's project grants on live projects, by project id.
    fn member_projects(
        &self,
        member: Uuid,
    ) -> impl Future<Output = Result<Vec<(Uuid, GrantRow)>, MetadataError>> + Send;

    /// The member's overrides on live databases, as (project, database, override).
    fn member_overrides(
        &self,
        member: Uuid,
    ) -> impl Future<Output = Result<Vec<(Uuid, Uuid, OverrideRow)>, MetadataError>> + Send;
}

// Each method below must delegate to `MetadataClient`'s inherent method of the
// same name (not to itself): renaming an inherent method here without updating
// the call site would silently make this resolve to the trait method instead,
// recursing forever.
impl MetadataPort for MetadataClient {
    async fn get_tenant_by_zitadel_org_id(
        &self,
        zitadel_org_id: &str,
    ) -> Result<Option<TenantId>, MetadataError> {
        MetadataClient::get_tenant_by_zitadel_org_id(self, zitadel_org_id).await
    }

    async fn insert_database_pending(
        &self,
        database_id: DatabaseId,
        tenant_id: TenantId,
        project_id: ProjectId,
        name: &str,
    ) -> Result<(), MetadataError> {
        MetadataClient::insert_database_pending(self, database_id, tenant_id, project_id, name)
            .await
    }

    async fn set_failed(&self, database_id: DatabaseId, error: &str) -> Result<(), MetadataError> {
        MetadataClient::set_failed(self, database_id, error).await
    }

    async fn get_database(
        &self,
        database_id: DatabaseId,
    ) -> Result<Option<DatabaseRecord>, MetadataError> {
        MetadataClient::get_database(self, database_id).await
    }

    async fn tenant_overview(
        &self,
        tenant_id: TenantId,
    ) -> Result<Option<TenantOverview>, MetadataError> {
        MetadataClient::tenant_overview(self, tenant_id).await
    }

    async fn get_branch(&self, branch_id: BranchId) -> Result<Option<BranchRecord>, MetadataError> {
        MetadataClient::get_branch(self, branch_id).await
    }

    async fn begin_delete(&self, database_id: DatabaseId) -> Result<BeginDelete, MetadataError> {
        MetadataClient::begin_delete(self, database_id).await
    }

    async fn list_branches(&self, database_id: DatabaseId) -> Result<Vec<BranchRecord>, MetadataError> {
        MetadataClient::list_branches(self, database_id).await
    }

    async fn main_branches_with_counts(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<(BranchRecord, i64)>, MetadataError> {
        MetadataClient::main_branches_with_counts(self, tenant_id).await
    }

    async fn create_branch(&self, branch: &NewBranch<'_>) -> Result<CreateBranchOutcome, MetadataError> {
        MetadataClient::create_branch(self, branch).await
    }

    async fn live_children(&self, branch_id: BranchId) -> Result<Vec<BranchRecord>, MetadataError> {
        MetadataClient::live_children(self, branch_id).await
    }

    async fn set_branch_status(
        &self,
        branch_id: BranchId,
        from: &[BranchStatus],
        to: BranchStatus,
    ) -> Result<bool, MetadataError> {
        MetadataClient::set_branch_status(self, branch_id, from, to).await
    }

    async fn claim_branch_for_switch(
        &self,
        branch_id: BranchId,
        allow_protected: bool,
    ) -> Result<ClaimOutcome, MetadataError> {
        MetadataClient::claim_branch_for_switch(self, branch_id, allow_protected).await
    }

    async fn set_branch_protected(
        &self,
        branch_id: BranchId,
        protected: bool,
    ) -> Result<Option<BranchRecord>, MetadataError> {
        MetadataClient::set_branch_protected(self, branch_id, protected).await
    }

    async fn set_branch_failed(&self, branch_id: BranchId, error: &str) -> Result<(), MetadataError> {
        MetadataClient::set_branch_failed(self, branch_id, error).await
    }

    async fn begin_branch_delete(
        &self,
        branch_id: BranchId,
    ) -> Result<BranchDeleteOutcome, MetadataError> {
        MetadataClient::begin_branch_delete(self, branch_id).await
    }

    async fn update_database_settings(
        &self,
        database_id: DatabaseId,
        patch: &SettingsPatch,
    ) -> Result<bool, MetadataError> {
        MetadataClient::update_database_settings(self, database_id, patch).await
    }

    async fn insert_data_key(&self, key: &NewDataKey) -> Result<(), MetadataError> {
        MetadataClient::insert_data_key(self, key).await
    }

    async fn create_secret_key(&self, key: &NewDataKey) -> Result<(), MetadataError> {
        MetadataClient::create_secret_key(self, key).await
    }

    async fn list_data_keys(&self, branch: BranchId) -> Result<Vec<DataKeyRecord>, MetadataError> {
        MetadataClient::list_data_keys(self, branch).await
    }

    async fn revoke_data_key(&self, branch: BranchId, key_id: Uuid) -> Result<bool, MetadataError> {
        MetadataClient::revoke_data_key(self, branch, key_id).await
    }

    async fn rotate_data_key(
        &self,
        branch: BranchId,
        old: Uuid,
        new: &NewDataKey,
    ) -> Result<bool, MetadataError> {
        MetadataClient::rotate_data_key(self, branch, old, new).await
    }

    async fn rotate_secret_key(
        &self,
        branch: BranchId,
        old: Uuid,
        new: &NewDataKey,
    ) -> Result<bool, MetadataError> {
        MetadataClient::rotate_secret_key(self, branch, old, new).await
    }

    async fn list_roles(&self, endpoint_id: &str) -> Result<Vec<RoleRow>, MetadataError> {
        MetadataClient::list_roles(self, endpoint_id).await
    }

    async fn member_branch_roles(
        &self,
        branch: Uuid,
    ) -> Result<Vec<chelabase_metadata::members::MemberBranchRole>, MetadataError> {
        MetadataClient::member_branch_roles(self, branch).await
    }

    async fn record_member_acted_role(
        &self,
        member: Uuid,
        branch: Uuid,
        role: &str,
    ) -> Result<bool, MetadataError> {
        MetadataClient::record_member_acted_role(self, member, branch, role).await
    }

    async fn role_password(
        &self,
        endpoint_id: &str,
        name: &str,
    ) -> Result<Option<String>, MetadataError> {
        MetadataClient::role_password(self, endpoint_id, name).await
    }

    async fn branch_databases(
        &self,
        branch: BranchId,
    ) -> Result<Vec<BranchDatabase>, MetadataError> {
        MetadataClient::branch_databases(self, branch).await
    }

    async fn branches_with_main_database(
        &self,
        ids: &[BranchId],
    ) -> Result<HashSet<BranchId>, MetadataError> {
        MetadataClient::branches_with_main_database(self, ids).await
    }

    async fn begin_enable_data_api(
        &self,
        branch: BranchId,
        name: &str,
        cap: u32,
    ) -> Result<EnableStart, MetadataError> {
        MetadataClient::begin_enable_data_api(self, branch, name, cap).await
    }

    async fn set_database_default_schema(
        &self,
        branch: BranchId,
        name: &str,
        schema: &str,
    ) -> Result<bool, MetadataError> {
        MetadataClient::set_database_default_schema(self, branch, name, schema).await
    }

    async fn set_key_databases(
        &self,
        branch: BranchId,
        key_id: Uuid,
        databases: Option<Vec<String>>,
    ) -> Result<bool, MetadataError> {
        MetadataClient::set_key_databases(self, branch, key_id, databases).await
    }

    async fn list_projects(
        &self,
        tenant: TenantId,
    ) -> Result<Vec<ProjectRecord>, MetadataError> {
        MetadataClient::list_projects(self, tenant).await
    }

    async fn get_project(
        &self,
        tenant: TenantId,
        id: ProjectId,
    ) -> Result<Option<ProjectRecord>, MetadataError> {
        MetadataClient::get_project(self, tenant, id).await
    }

    async fn project_by_id(
        &self,
        id: ProjectId,
    ) -> Result<Option<ProjectRecord>, MetadataError> {
        MetadataClient::project_by_id(self, id).await
    }

    async fn create_project(
        &self,
        tenant: TenantId,
        name: &str,
    ) -> Result<ProjectRecord, MetadataError> {
        MetadataClient::create_project(self, tenant, name).await
    }

    async fn rename_project(
        &self,
        tenant: TenantId,
        id: ProjectId,
        name: &str,
    ) -> Result<RenameProject, MetadataError> {
        MetadataClient::rename_project(self, tenant, id, name).await
    }

    async fn begin_project_delete(
        &self,
        tenant: TenantId,
        id: ProjectId,
    ) -> Result<BeginProjectDelete, MetadataError> {
        MetadataClient::begin_project_delete(self, tenant, id).await
    }

    async fn project_databases(
        &self,
        id: ProjectId,
    ) -> Result<Vec<(DatabaseId, DatabaseStatus, Option<String>)>, MetadataError> {
        MetadataClient::project_databases(self, id).await
    }

    async fn project_auth(
        &self,
        project: ProjectId,
    ) -> Result<Option<ProjectAuth>, MetadataError> {
        MetadataClient::project_auth(self, project).await
    }

    async fn project_auth_statuses(
        &self,
        tenant: TenantId,
    ) -> Result<HashMap<ProjectId, AppUsersStatus>, MetadataError> {
        MetadataClient::project_auth_statuses(self, tenant).await
    }

    async fn begin_enable_app_users(
        &self,
        new: &NewProjectAuth<'_>,
    ) -> Result<BeginEnable, MetadataError> {
        MetadataClient::begin_enable_app_users(self, new).await
    }

    async fn update_app_users_settings(
        &self,
        project: ProjectId,
        email_verification: Option<bool>,
        on_delete: Option<OnDelete>,
    ) -> Result<Option<ProjectAuth>, MetadataError> {
        MetadataClient::update_app_users_settings(self, project, email_verification, on_delete).await
    }

    async fn begin_disable_app_users(
        &self,
        project: ProjectId,
    ) -> Result<bool, MetadataError> {
        MetadataClient::begin_disable_app_users(self, project).await
    }

    async fn branch_auth_sync(
        &self,
        branch: BranchId,
    ) -> Result<Option<BranchAuthSync>, MetadataError> {
        MetadataClient::branch_auth_sync(self, branch).await
    }

    async fn list_user_sessions(
        &self,
        project: ProjectId,
        user: Uuid,
    ) -> Result<Vec<AppSession>, MetadataError> {
        MetadataClient::list_user_sessions(self, project, user).await
    }

    async fn revoke_user_sessions(
        &self,
        project: ProjectId,
        user: Uuid,
        keep: Option<Uuid>,
        created_before: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Vec<AppSession>, MetadataError> {
        MetadataClient::revoke_user_sessions(self, project, user, keep, created_before).await
    }

    async fn app_auth_for_database(
        &self,
        db: DatabaseId,
    ) -> Result<Option<ProjectAuth>, MetadataError> {
        MetadataClient::app_auth_for_database(self, db).await
    }

    async fn list_saved_queries(
        &self,
        database: Uuid,
    ) -> Result<Vec<SavedQuerySummary>, MetadataError> {
        MetadataClient::list_saved_queries(self, database).await
    }

    async fn get_saved_query(
        &self,
        database: Uuid,
        id: Uuid,
    ) -> Result<Option<SavedQuery>, MetadataError> {
        MetadataClient::get_saved_query(self, database, id).await
    }

    async fn create_saved_query(
        &self,
        database: Uuid,
        user: &str,
        author: Option<Uuid>,
        id: Uuid,
        name: &str,
        sql: &str,
        pg_database: Option<&str>,
        role: Option<&str>,
    ) -> Result<SavedQueryWrite, MetadataError> {
        MetadataClient::create_saved_query(self, database, user, author, id, name, sql, pg_database, role).await
    }

    async fn update_saved_query(
        &self,
        database: Uuid,
        by: SavedQueryEditor,
        id: Uuid,
        version: i32,
        patch: &SavedQueryPatch,
    ) -> Result<SavedQueryWrite, MetadataError> {
        MetadataClient::update_saved_query(self, database, by, id, version, patch).await
    }

    async fn delete_saved_query(
        &self,
        database: Uuid,
        by: SavedQueryEditor,
        id: Uuid,
    ) -> Result<bool, MetadataError> {
        MetadataClient::delete_saved_query(self, database, by, id).await
    }
    async fn create_merge(&self, merge: &NewMerge) -> Result<MergeCreate, MetadataError> {
        MetadataClient::create_merge(self, merge).await
    }

    async fn get_merge(&self, id: Uuid) -> Result<Option<MergeRecord>, MetadataError> {
        MetadataClient::merge(self, id).await
    }

    async fn list_merges(
        &self,
        database_id: DatabaseId,
        branch: Option<BranchId>,
    ) -> Result<Vec<MergeRecord>, MetadataError> {
        MetadataClient::list_merges(self, database_id, branch).await
    }

    async fn update_merge_draft(
        &self,
        id: Uuid,
        expected_version: i64,
        patch: &MergeDraftPatch,
    ) -> Result<MergeWrite, MetadataError> {
        MetadataClient::update_merge_draft(self, id, expected_version, patch).await
    }

    async fn set_merge_status(
        &self,
        id: Uuid,
        from: &[MergeStatus],
        to: MergeStatus,
        patch: MergeStatusPatch,
    ) -> Result<bool, MetadataError> {
        MetadataClient::set_merge_status(self, id, from, to, patch).await
    }

    async fn merge_events(&self, id: Uuid) -> Result<Vec<MergeEvent>, MetadataError> {
        MetadataClient::merge_events(self, id).await
    }

    async fn cancel_drafts_for_branch(&self, branch: BranchId) -> Result<u64, MetadataError> {
        MetadataClient::cancel_drafts_for_branch(self, branch).await
    }

    async fn branch_has_busy_merge(&self, branch: BranchId) -> Result<Option<chelabase_metadata::BranchBusy>, MetadataError> {
        MetadataClient::branch_has_busy_merge(self, branch).await
    }

    async fn member_by_zitadel(
        &self,
        tenant: TenantId,
        zitadel_user_id: &str,
    ) -> Result<Option<MemberRecord>, MetadataError> {
        MetadataClient::member_by_zitadel(self, tenant, zitadel_user_id).await
    }

    async fn ensure_member(
        &self,
        tenant: TenantId,
        zitadel_user_id: &str,
        email: &str,
        name: &str,
        admission: Admission,
    ) -> Result<MemberRecord, MetadataError> {
        MetadataClient::ensure_member(self, tenant, zitadel_user_id, email, name, admission).await
    }

    async fn grant_rows(
        &self,
        member: Uuid,
        project: Uuid,
        database: Option<Uuid>,
    ) -> Result<GrantRows, MetadataError> {
        MetadataClient::grant_rows(self, member, project, database).await
    }

    async fn member_projects(&self, member: Uuid) -> Result<Vec<(Uuid, GrantRow)>, MetadataError> {
        MetadataClient::member_projects(self, member).await
    }

    async fn member_overrides(&self, member: Uuid) -> Result<Vec<(Uuid, Uuid, OverrideRow)>, MetadataError> {
        MetadataClient::member_overrides(self, member).await
    }
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Why a reset-password request got no answer from the worker.
#[derive(Debug, Clone)]
pub enum ResetError {
    /// A worker is listening but didn't answer in time.
    Timeout,
    /// No worker listening, or the request couldn't be sent or parsed.
    Transport(String),
}

/// Talking to the worker: durable jobs and the reset-password RPC. Abstracted
/// so tests can inject an in-memory fake instead of NATS. Member-role reconciles are
/// [`members::MemberRolesQueue`].
pub trait JobPort: members::MemberRolesQueue + Send + Sync + 'static {
    fn publish_provision(
        &self,
        job: &JobEnvelope<ProvisionDatabaseJob>,
    ) -> impl Future<Output = Result<(), BoxError>> + Send;

    fn publish_delete(
        &self,
        job: &JobEnvelope<DeleteDatabaseJob>,
    ) -> impl Future<Output = Result<(), BoxError>> + Send;

    fn publish_branch(
        &self,
        job: &JobEnvelope<BranchJob>,
    ) -> impl Future<Output = Result<(), BoxError>> + Send;

    fn publish_database_settings(
        &self,
        job: &JobEnvelope<DatabaseSettingsJob>,
    ) -> impl Future<Output = Result<(), BoxError>> + Send;

    fn publish_merge(
        &self,
        job: &JobEnvelope<MergeJob>,
    ) -> impl Future<Output = Result<(), BoxError>> + Send;

    /// Asks the worker whether a secret key may use `role` on the branch (and grants it to
    /// `authenticator`).
    fn check_data_role(
        &self,
        branch_id: BranchId,
        role: &str,
    ) -> impl Future<Output = Result<CheckDataRoleReply, ResetError>> + Send;

    /// Asks the worker to make a branch's member roles (only `member_id`'s, when given) match
    /// the metadata now (`chelabase.rpc.member_roles.reconcile`); the SQL editor's lazy check.
    fn reconcile_member_roles(
        &self,
        request: chelabase_nats::MemberRolesReconcileRequest,
    ) -> impl Future<Output = Result<chelabase_nats::MemberRolesReply, ResetError>> + Send;

    /// Asks the worker for the hand-over after an act-as run
    /// (`chelabase.rpc.member_roles.hand_over`; R5 and the member-role invariant).
    fn hand_over_member_role(
        &self,
        request: chelabase_nats::MemberRoleHandOverRequest,
    ) -> impl Future<Output = Result<chelabase_nats::MemberRolesReply, ResetError>> + Send;

    /// Tells the worker to bootstrap the data API in a database the orchestrator marked
    /// `enabling` (a core publish: no reply; reconcile retries a row left `enabling`).
    fn enable_data_api(
        &self,
        branch_id: BranchId,
        name: &str,
    ) -> impl Future<Output = Result<(), BoxError>> + Send;

    /// Stops serving a database; `remove` also drops the `chelabase` machinery there.
    fn disable_data_api(
        &self,
        branch_id: BranchId,
        name: &str,
        remove: bool,
    ) -> impl Future<Output = Result<CheckDataRoleReply, ResetError>> + Send;

    /// Re-runs the bootstrap of a served database after its `default_schema` changed;
    /// `grant_existing` also grants the data API roles on the schema's existing objects.
    fn rebootstrap_data_api(
        &self,
        branch_id: BranchId,
        name: &str,
        grant_existing: bool,
    ) -> impl Future<Output = Result<CheckDataRoleReply, ResetError>> + Send;

    /// Resets the owner password of one branch (main's id is the database's).
    fn reset_password(
        &self,
        database_id: DatabaseId,
        branch_id: BranchId,
    ) -> impl Future<Output = Result<ResetPasswordReply, ResetError>> + Send;

    /// Creates a login role on a branch.
    fn create_role(
        &self,
        request: RoleCreateRequest,
    ) -> impl Future<Output = Result<RoleReply, ResetError>> + Send;

    /// Resets a login role's password.
    fn reset_role(
        &self,
        request: RoleRequest,
    ) -> impl Future<Output = Result<RoleReply, ResetError>> + Send;

    /// Changes a role's LOGIN, owner-level, attributes or memberships.
    fn update_role(
        &self,
        request: RoleUpdateRequest,
    ) -> impl Future<Output = Result<RoleReply, ResetError>> + Send;

    /// Deletes a login role.
    fn delete_role(
        &self,
        request: RoleRequest,
    ) -> impl Future<Output = Result<RoleReply, ResetError>> + Send;

    /// Reads a role's live sessions on the branch's compute (E29a); never starts the compute.
    fn role_sessions(
        &self,
        request: RoleRequest,
    ) -> impl Future<Output = Result<RoleSessionsReply, ResetError>> + Send;

    fn publish_project_delete(
        &self,
        job: &JobEnvelope<ProjectDeleteJob>,
    ) -> impl Future<Output = Result<(), BoxError>> + Send;

    fn publish_app_users(
        &self,
        job: &JobEnvelope<AppUsersJob>,
    ) -> impl Future<Output = Result<(), BoxError>> + Send;

    fn publish_app_users_sync(
        &self,
        job: &JobEnvelope<AppUsersSyncJob>,
    ) -> impl Future<Output = Result<(), BoxError>> + Send;

    /// Asks the worker whether a table can hold the app-users location (a request with a
    /// 30 s timeout).
    fn check_app_users_location(
        &self,
        request: &AppUsersCheckRequest,
    ) -> impl Future<Output = Result<AppUsersCheckReply, BoxError>> + Send;
}

/// How long the orchestrator waits for the worker to check an app-users location.
const APP_USERS_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// How long the orchestrator waits for the worker to reset a password.
const RESET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// One request/reply to the worker on `subject`, waiting up to [`RESET_TIMEOUT`].
async fn role_rpc<T: Serialize>(
    client: &async_nats::Client,
    subject: &'static str,
    request: &T,
) -> Result<RoleReply, ResetError> {
    use async_nats::RequestErrorKind;
    let body = serde_json::to_vec(request).map_err(|e| ResetError::Transport(e.to_string()))?;
    let request = async_nats::Request::new()
        .payload(body.into())
        .timeout(Some(RESET_TIMEOUT));
    let reply = client
        .send_request(subject, request)
        .await
        .map_err(|e| match e.kind() {
            RequestErrorKind::TimedOut => ResetError::Timeout,
            RequestErrorKind::NoResponders => ResetError::Transport("no worker listening".into()),
            _ => ResetError::Transport(e.to_string()),
        })?;
    serde_json::from_slice(&reply.payload).map_err(|e| ResetError::Transport(e.to_string()))
}

/// One member-role request/reply to the worker on `subject`, waiting up to [`RESET_TIMEOUT`].
async fn member_roles_rpc<T: Serialize>(
    client: &async_nats::Client,
    subject: &'static str,
    request: &T,
) -> Result<chelabase_nats::MemberRolesReply, ResetError> {
    use async_nats::RequestErrorKind;
    let body = serde_json::to_vec(request).map_err(|e| ResetError::Transport(e.to_string()))?;
    let request = async_nats::Request::new()
        .payload(body.into())
        .timeout(Some(RESET_TIMEOUT));
    let reply = client
        .send_request(subject, request)
        .await
        .map_err(|e| match e.kind() {
            RequestErrorKind::TimedOut => ResetError::Timeout,
            RequestErrorKind::NoResponders => ResetError::Transport("no worker listening".into()),
            _ => ResetError::Transport(e.to_string()),
        })?;
    serde_json::from_slice(&reply.payload).map_err(|e| ResetError::Transport(e.to_string()))
}

/// One request/reply to the worker on `subject` answering a [`CheckDataRoleReply`].
async fn data_api_rpc(
    client: &async_nats::Client,
    subject: &'static str,
    request: &DataApiDatabaseRequest,
) -> Result<CheckDataRoleReply, ResetError> {
    use async_nats::RequestErrorKind;
    let body = serde_json::to_vec(request).map_err(|e| ResetError::Transport(e.to_string()))?;
    let request = async_nats::Request::new()
        .payload(body.into())
        .timeout(Some(RESET_TIMEOUT));
    let reply = client
        .send_request(subject, request)
        .await
        .map_err(|e| match e.kind() {
            RequestErrorKind::TimedOut => ResetError::Timeout,
            RequestErrorKind::NoResponders => ResetError::Transport("no worker listening".into()),
            _ => ResetError::Transport(e.to_string()),
        })?;
    serde_json::from_slice(&reply.payload).map_err(|e| ResetError::Transport(e.to_string()))
}

/// NATS-backed [`JobPort`]: JetStream for jobs, core request/reply for resets.
#[derive(Clone)]
pub struct NatsPorts {
    pub js: JetStreamContext,
    pub client: async_nats::Client,
}

impl JobPort for NatsPorts {
    async fn publish_provision(&self, job: &JobEnvelope<ProvisionDatabaseJob>) -> Result<(), BoxError> {
        chelabase_nats::publish_provision(&self.js, job)
            .await
            .map_err(|e| Box::new(e) as BoxError)
    }

    async fn publish_delete(&self, job: &JobEnvelope<DeleteDatabaseJob>) -> Result<(), BoxError> {
        chelabase_nats::publish_delete(&self.js, job)
            .await
            .map_err(|e| Box::new(e) as BoxError)
    }

    async fn publish_branch(&self, job: &JobEnvelope<BranchJob>) -> Result<(), BoxError> {
        chelabase_nats::publish_branch(&self.js, job)
            .await
            .map_err(|e| Box::new(e) as BoxError)
    }

    async fn publish_database_settings(
        &self,
        job: &JobEnvelope<DatabaseSettingsJob>,
    ) -> Result<(), BoxError> {
        chelabase_nats::publish_database_settings(&self.js, job)
            .await
            .map_err(|e| Box::new(e) as BoxError)
    }

    async fn publish_merge(&self, job: &JobEnvelope<MergeJob>) -> Result<(), BoxError> {
        chelabase_nats::publish_merge(&self.js, job)
            .await
            .map_err(|e| Box::new(e) as BoxError)
    }

    async fn check_data_role(
        &self,
        branch_id: BranchId,
        role: &str,
    ) -> Result<CheckDataRoleReply, ResetError> {
        use async_nats::RequestErrorKind;
        let body = serde_json::to_vec(&CheckDataRoleRequest {
            branch_id,
            role: role.to_string(),
        })
        .map_err(|e| ResetError::Transport(e.to_string()))?;
        let request = async_nats::Request::new()
            .payload(body.into())
            .timeout(Some(RESET_TIMEOUT));
        let reply = self
            .client
            .send_request(SUBJECT_CHECK_DATA_ROLE, request)
            .await
            .map_err(|e| match e.kind() {
                RequestErrorKind::TimedOut => ResetError::Timeout,
                RequestErrorKind::NoResponders => ResetError::Transport("no worker listening".into()),
                _ => ResetError::Transport(e.to_string()),
            })?;
        serde_json::from_slice(&reply.payload).map_err(|e| ResetError::Transport(e.to_string()))
    }

    async fn reconcile_member_roles(
        &self,
        request: chelabase_nats::MemberRolesReconcileRequest,
    ) -> Result<chelabase_nats::MemberRolesReply, ResetError> {
        member_roles_rpc(&self.client, chelabase_nats::SUBJECT_MEMBER_ROLES_RECONCILE, &request).await
    }

    async fn hand_over_member_role(
        &self,
        request: chelabase_nats::MemberRoleHandOverRequest,
    ) -> Result<chelabase_nats::MemberRolesReply, ResetError> {
        member_roles_rpc(&self.client, chelabase_nats::SUBJECT_MEMBER_ROLES_HAND_OVER, &request).await
    }

    async fn enable_data_api(&self, branch_id: BranchId, name: &str) -> Result<(), BoxError> {
        let body = serde_json::to_vec(&DataApiDatabaseRequest {
            branch_id,
            name: name.to_string(),
            remove: false,
            grant_existing: false,
        })?;
        self.client.publish(SUBJECT_DATA_API_ENABLE, body.into()).await?;
        Ok(())
    }

    async fn disable_data_api(
        &self,
        branch_id: BranchId,
        name: &str,
        remove: bool,
    ) -> Result<CheckDataRoleReply, ResetError> {
        let request = DataApiDatabaseRequest { branch_id, name: name.to_string(), remove, grant_existing: false };
        data_api_rpc(&self.client, SUBJECT_DATA_API_DISABLE, &request).await
    }

    async fn rebootstrap_data_api(
        &self,
        branch_id: BranchId,
        name: &str,
        grant_existing: bool,
    ) -> Result<CheckDataRoleReply, ResetError> {
        let request = DataApiDatabaseRequest { branch_id, name: name.to_string(), remove: false, grant_existing };
        data_api_rpc(&self.client, SUBJECT_DATA_API_REBOOTSTRAP, &request).await
    }

    async fn reset_password(
        &self,
        database_id: DatabaseId,
        branch_id: BranchId,
    ) -> Result<ResetPasswordReply, ResetError> {
        use async_nats::RequestErrorKind;
        let body = serde_json::to_vec(&ResetPasswordRequest { database_id, branch_id })
            .map_err(|e| ResetError::Transport(e.to_string()))?;
        // A per-request timeout: the client's default (10 s) would otherwise
        // cut the documented 15 s short.
        let request = async_nats::Request::new()
            .payload(body.into())
            .timeout(Some(RESET_TIMEOUT));
        let reply = self
            .client
            .send_request(SUBJECT_RESET_PASSWORD, request)
            .await
            .map_err(|e| match e.kind() {
            RequestErrorKind::TimedOut => ResetError::Timeout,
            RequestErrorKind::NoResponders => ResetError::Transport("no worker listening".into()),
            _ => ResetError::Transport(e.to_string()),
        })?;
        serde_json::from_slice(&reply.payload).map_err(|e| ResetError::Transport(e.to_string()))
    }

    async fn create_role(&self, request: RoleCreateRequest) -> Result<RoleReply, ResetError> {
        role_rpc(&self.client, SUBJECT_ROLE_CREATE, &request).await
    }

    async fn reset_role(&self, request: RoleRequest) -> Result<RoleReply, ResetError> {
        role_rpc(&self.client, SUBJECT_ROLE_RESET, &request).await
    }

    async fn update_role(&self, request: RoleUpdateRequest) -> Result<RoleReply, ResetError> {
        role_rpc(&self.client, SUBJECT_ROLE_UPDATE, &request).await
    }

    async fn delete_role(&self, request: RoleRequest) -> Result<RoleReply, ResetError> {
        role_rpc(&self.client, SUBJECT_ROLE_DELETE, &request).await
    }

    async fn role_sessions(&self, request: RoleRequest) -> Result<RoleSessionsReply, ResetError> {
        use async_nats::RequestErrorKind;
        let body = serde_json::to_vec(&request).map_err(|e| ResetError::Transport(e.to_string()))?;
        let request = async_nats::Request::new()
            .payload(body.into())
            .timeout(Some(RESET_TIMEOUT));
        let reply = self
            .client
            .send_request(SUBJECT_ROLE_SESSIONS, request)
            .await
            .map_err(|e| match e.kind() {
                RequestErrorKind::TimedOut => ResetError::Timeout,
                RequestErrorKind::NoResponders => ResetError::Transport("no worker listening".into()),
                _ => ResetError::Transport(e.to_string()),
            })?;
        serde_json::from_slice(&reply.payload).map_err(|e| ResetError::Transport(e.to_string()))
    }

    async fn publish_project_delete(&self, job: &JobEnvelope<ProjectDeleteJob>) -> Result<(), BoxError> {
        chelabase_nats::publish_project_delete(&self.js, job)
            .await
            .map_err(|e| Box::new(e) as BoxError)
    }

    async fn publish_app_users(&self, job: &JobEnvelope<AppUsersJob>) -> Result<(), BoxError> {
        chelabase_nats::publish_app_users(&self.js, job)
            .await
            .map_err(|e| Box::new(e) as BoxError)
    }

    async fn publish_app_users_sync(&self, job: &JobEnvelope<AppUsersSyncJob>) -> Result<(), BoxError> {
        chelabase_nats::publish_app_users_sync(&self.js, job)
            .await
            .map_err(|e| Box::new(e) as BoxError)
    }

    async fn check_app_users_location(
        &self,
        request: &AppUsersCheckRequest,
    ) -> Result<AppUsersCheckReply, BoxError> {
        let body = serde_json::to_vec(request)?;
        let request = async_nats::Request::new()
            .payload(body.into())
            .timeout(Some(APP_USERS_CHECK_TIMEOUT));
        let reply = self.client.send_request(SUBJECT_APP_USERS_CHECK, request).await?;
        Ok(serde_json::from_slice(&reply.payload)?)
    }
}

pub struct AppState<M, J> {
    pub metadata: M,
    pub js: J,
    /// Verifies gateway-minted internal identity tokens.
    pub identity: InternalVerifier,
    /// Decrypts stored database credentials for reveal.
    pub secrets: SecretKey,
    /// Where clients reach the Neon proxy: `<ep>.<domain>` on its ports.
    pub proxy: ProxyAddress,
    /// The REST service's internal listener (the data API's schema).
    pub rest: RestInternal,
    /// Every self-hosted/platform decision (the dev CA path is revealed only self-hosted).
    pub policy: ModePolicy,
    /// `CHELABASE_OPERATOR_ORG_ID`: in platform mode, members of this Zitadel org are instance
    /// admins. `None` (unset or blank) means nobody is.
    pub operator_org: Option<String>,
    /// Where the dev proxy's CA certificate is (`CHELABASE_PROXY_CA_FILE`); self-hosted only.
    pub proxy_ca_file: String,
    /// `CHELABASE_BRANCH_DATA_API_DATABASE_CAP`: databases a branch serves, main included.
    pub data_api_database_cap: u32,
    /// Where a compute publishes Postgres: the console's owner pools connect there.
    pub locator: Arc<dyn PostgresAddr>,
    /// Studio's console: owner pools, the owner's query caches, checked roles.
    pub console: Console,
    /// Zitadel's admin API (admin PAT); `None` when `CHELABASE_ZITADEL_ADMIN_PAT_FILE` or
    /// `CHELABASE_ZITADEL_ISSUER` isn't set.
    pub zitadel: Option<chelabase_zitadel::client::Zitadel>,
    /// The one write path to a project's users table.
    pub users_db: Arc<dyn chelabase_query::app_users::UsersDb>,
    /// Studio's SQL editor: runs in flight and the data-role grant cache.
    pub sql_editor: sql_editor::SqlEditor,
    /// `CHELABASE_STUDIO_URL` (no trailing `/`): invite links go to its `/accept-invite`.
    pub studio_url: String,
}

/// `CHELABASE_BRANCH_DATA_API_DATABASE_CAP`: 10 when unset or blank, otherwise 1 to 100.
pub fn data_api_database_cap(value: Option<&str>) -> Result<u32, String> {
    const NAME: &str = "CHELABASE_BRANCH_DATA_API_DATABASE_CAP";
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok(10),
        Some(v) => v
            .parse::<u32>()
            .ok()
            .filter(|n| (1..=100).contains(n))
            .ok_or_else(|| format!("{NAME}={v:?}: expected a whole number from 1 to 100")),
    }
}

/// Where the REST service's internal listener is, and the token it takes.
#[derive(Clone)]
pub struct RestInternal {
    /// `http://host:port`, no path.
    pub url: String,
    pub token: String,
    pub http: reqwest::Client,
}

/// Longest wait for rest to acknowledge an invalidation.
const INVALIDATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl RestInternal {
    /// Tells rest to forget everything it holds for the branch (pools, connection info,
    /// cached shapes). A failure is logged and never fails the caller's request: rest's own
    /// entries expire within 60 seconds anyway.
    pub async fn invalidate(&self, branch: BranchId) {
        let url = format!(
            "{}/internal/branches/{}/invalidate",
            self.url.trim_end_matches('/'),
            branch.as_uuid()
        );
        match self
            .http
            .post(url)
            .bearer_auth(&self.token)
            .timeout(INVALIDATE_TIMEOUT)
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => {}
            Ok(response) => {
                tracing::warn!(branch_id = %branch, status = %response.status(), "rest invalidate refused");
            }
            Err(err) => tracing::warn!(branch_id = %branch, "rest invalidate failed: {err}"),
        }
    }
}

/// Caller verified from the gateway-minted internal identity token. Does not
/// require the caller's org to be bound to a tenant.
#[derive(Debug, Clone)]
struct Caller {
    sub: String,
    org_id: String,
    request_id: String,
    /// The token's email and name claims, when it carries them.
    email: Option<String>,
    name: Option<String>,
}

impl<M, J> FromRequestParts<Arc<AppState<M, J>>> for Caller
where
    M: MetadataPort,
    J: JobPort,
{
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState<M, J>>,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(IDENTITY_HEADER)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                ApiError::unauthorized("missing internal identity; call the API through the gateway")
            })?;

        let claims = state
            .identity
            .verify(token, Service::Orchestrator.name())
            .map_err(|err| ApiError::unauthorized(err.to_string()))?;

        Ok(Caller {
            sub: claims.sub,
            org_id: claims.org_id,
            request_id: claims.rid,
            email: claims.email,
            name: claims.name,
        })
    }
}

/// Caller whose org is bound to a Chelabase tenant (soft-bind), with their member row.
#[derive(Debug, Clone)]
pub struct AuthPrincipal {
    pub sub: String,
    pub org_id: String,
    pub tenant_id: TenantId,
    pub request_id: String,
    /// The caller's member row in the tenant: `None` only in platform mode, which makes no
    /// members on its own. [`access::authorize`] checks its status on every request.
    pub member: Option<MemberRecord>,
    /// Whether the caller's org is `CHELABASE_OPERATOR_ORG_ID` (platform instance routes).
    pub in_operator_org: bool,
}

/// The caller's member row in `tenant`, read once per request in the common case.
///
/// - Self-hosted (`ModePolicy::auto_create_members`), a person in the tenant's org who has no
///   row yet is admitted on their first request (R39): the first one claims the tenant's admin;
///   an invited one is activated; anyone else becomes `pending` when the tenant's open sign-up
///   is on, and otherwise gets no row (`None`: 403 `no_access_yet`). Platform mode makes no
///   members and only refreshes existing ones.
/// - The email and name come from the token's claims. A claim the token lacks keeps the stored
///   value; when there is none (a new member, or one with no email or name stored), the user is
///   read once through the admin Zitadel client (ruling R18). A Zitadel failure isn't fatal:
///   the value stays unknown and a warning is logged.
/// - An active member whose stored values the token doesn't change isn't written.
async fn resolve_member<M: MetadataPort, J>(
    state: &AppState<M, J>,
    tenant: TenantId,
    caller: &Caller,
) -> Result<Option<MemberRecord>, ApiError> {
    let failed = |err: MetadataError| {
        error!("resolve member: {err}");
        ApiError::internal()
    };
    let auto_create = state.policy.auto_create_members();
    // Both claims at hand: `ensure_member` reads, refreshes or makes the row in one go.
    if auto_create && let (Some(email), Some(name)) = (&caller.email, &caller.name) {
        return admit(state, tenant, &caller.sub, email, name).await.map_err(failed);
    }
    let existing = state
        .metadata
        .member_by_zitadel(tenant, &caller.sub)
        .await
        .map_err(failed)?;
    let existing = match existing {
        None if !auto_create => return Ok(None),
        // A removed or pending member is returned as is: neither refreshes its claims.
        Some(m) if matches!(m.status, MemberStatus::Removed | MemberStatus::Pending) => return Ok(Some(m)),
        other => other,
    };
    let mut email = caller
        .email
        .clone()
        .or_else(|| existing.as_ref().and_then(|m| m.email.clone()));
    let mut name = caller.name.clone().or_else(|| {
        existing
            .as_ref()
            .map(|m| m.display_name.clone())
            .filter(|n| !n.is_empty())
    });
    if email.is_none() || name.is_none() {
        let (z_email, z_name) = zitadel_claims(state, &caller.sub).await;
        email = email.or(z_email);
        name = name.or(z_name);
    }
    if let Some(m) = &existing
        && m.status == MemberStatus::Active
        && email.as_deref().map(chelabase_zitadel::rules::normalize_email) == m.email
        && name.as_deref().unwrap_or("") == m.display_name
    {
        return Ok(existing);
    }
    admit(
        state,
        tenant,
        &caller.sub,
        email.as_deref().unwrap_or(""),
        name.as_deref().unwrap_or(""),
    )
    .await
    .map_err(failed)
}

/// How long after a tenant's admin claim a person with no row is still taken as the other half of
/// the fresh-install race (R41, M7): the sign-up gate lets everyone in while the tenant has no
/// member, so two concurrent sign-ups both get accounts, and the second one's first request
/// arrives after the first has claimed the admin. Long enough for an email verification.
const FIRST_MEMBER_WINDOW: chrono::Duration = chrono::Duration::hours(1);

/// [`MetadataPort::ensure_member`] with the tenant's sign-up rule: asked with `Closed` first, so
/// a member who already has a row costs no extra read; only a newcomer reads `open_signup`, and
/// is then admitted as `pending` or not at all (`None`). M7: a newcomer is also `pending` while
/// the tenant's admin claim is within [`FIRST_MEMBER_WINDOW`], so the loser of the fresh-install
/// race isn't stranded with an account and no way in: the admin sees the request.
async fn admit<M: MetadataPort, J>(
    state: &AppState<M, J>,
    tenant: TenantId,
    sub: &str,
    email: &str,
    name: &str,
) -> Result<Option<MemberRecord>, MetadataError> {
    match state.metadata.ensure_member(tenant, sub, email, name, Admission::Closed).await {
        Err(MetadataError::NotAdmitted(_)) => {
            let race = || state.metadata.admin_claimed_since(tenant, chrono::Utc::now() - FIRST_MEMBER_WINDOW);
            if state.metadata.open_signup(tenant).await? || race().await? {
                state.metadata.ensure_member(tenant, sub, email, name, Admission::Pending).await.map(Some)
            } else {
                Ok(None)
            }
        }
        other => other.map(Some),
    }
}

/// The user's email and display name from Zitadel (`GET /v2/users/{id}`), each `None` when
/// unknown. Failures are logged and give `None`s.
async fn zitadel_claims<M, J>(state: &AppState<M, J>, user_id: &str) -> (Option<String>, Option<String>) {
    let Some(zitadel) = state.zitadel.as_ref() else {
        return (None, None);
    };
    match zitadel.get_user(user_id).await {
        Ok(Some(user)) => {
            let text = |s: String| Some(s).filter(|s| !s.trim().is_empty());
            (text(user.email), text(user.display_name))
        }
        Ok(None) => {
            tracing::warn!(user_id, "member claims: no such Zitadel user; email and name stay unknown");
            (None, None)
        }
        Err(err) => {
            tracing::warn!(user_id, "member claims: Zitadel lookup failed, email and name stay unknown: {err}");
            (None, None)
        }
    }
}

fn in_operator_org<M, J>(state: &AppState<M, J>, org_id: &str) -> bool {
    state.operator_org.as_deref().is_some_and(|org| !org.is_empty() && org == org_id)
}

/// A caller allowed on an `Instance` route. Checked before the tenant binding (ruling R19): in
/// platform mode a member of the operator org passes whether or not the org is a tenant's.
/// Anyone else needs a bound org (else `tenant_not_bound`) and, self-hosted, to be an active
/// tenant admin. Instance handlers take this instead of [`AuthPrincipal`].
#[derive(Debug, Clone)]
pub struct InstancePrincipal {
    pub sub: String,
    pub org_id: String,
    pub request_id: String,
    /// The caller's tenant; `None` for an operator whose org is no tenant's.
    pub tenant_id: Option<TenantId>,
    pub member: Option<MemberRecord>,
}

impl<M, J> FromRequestParts<Arc<AppState<M, J>>> for InstancePrincipal
where
    M: MetadataPort,
    J: JobPort,
{
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState<M, J>>,
    ) -> Result<Self, Self::Rejection> {
        use chelabase_types::access::{decide, Access, CallerFacts};
        let caller = Caller::from_request_parts(parts, state).await?;
        let operator = in_operator_org(state, &caller.org_id);
        if state.policy.instance_admin(false, operator) {
            return Ok(InstancePrincipal {
                sub: caller.sub,
                org_id: caller.org_id,
                request_id: caller.request_id,
                tenant_id: None,
                member: None,
            });
        }
        let tenant_id = lookup_tenant(state, &caller.org_id)
            .await
            .map_err(|_| ApiError::internal())?
            .ok_or_else(tenant_not_bound)?;
        let member = resolve_member(state, tenant_id, &caller).await?;
        let facts = CallerFacts { member: access::member_facts(member.as_ref()), in_operator_org: operator };
        decide(&state.policy, &facts, None, None, None, None, Access::Instance).map_err(access::deny_to_api)?;
        Ok(InstancePrincipal {
            sub: caller.sub,
            org_id: caller.org_id,
            request_id: caller.request_id,
            tenant_id: Some(tenant_id),
            member,
        })
    }
}

fn tenant_not_bound() -> ApiError {
    ApiError::new(
        403,
        ErrorCode::TenantNotBound,
        "no Chelabase tenant bound to this Zitadel org; run just zitadel-bootstrap",
    )
}

impl<M, J> FromRequestParts<Arc<AppState<M, J>>> for AuthPrincipal
where
    M: MetadataPort,
    J: JobPort,
{
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState<M, J>>,
    ) -> Result<Self, Self::Rejection> {
        let caller = Caller::from_request_parts(parts, state).await?;
        let tenant_id = lookup_tenant(state, &caller.org_id)
            .await
            .map_err(|_| ApiError::internal())?
            .ok_or_else(|| {
                ApiError::new(
                    403,
                    ErrorCode::TenantNotBound,
                    "no Chelabase tenant bound to this Zitadel org; run just zitadel-bootstrap",
                )
            })?;

        let member = resolve_member(state, tenant_id, &caller).await?;
        Ok(AuthPrincipal {
            in_operator_org: in_operator_org(state, &caller.org_id),
            sub: caller.sub,
            org_id: caller.org_id,
            tenant_id,
            request_id: caller.request_id,
            member,
        })
    }
}

/// The tenant bound to `org_id`, if any (lookup failures are logged).
async fn lookup_tenant<M: MetadataPort, J>(
    state: &AppState<M, J>,
    org_id: &str,
) -> Result<Option<TenantId>, MetadataError> {
    state
        .metadata
        .get_tenant_by_zitadel_org_id(org_id)
        .await
        .inspect_err(|err| error!("tenant lookup by zitadel org: {err}"))
}

/// The body of a JSON route that parses it by hand: the extractor only reads the bytes, so
/// a body over the limit answers in our shape instead of axum's plain text.
pub(crate) struct ApiBytes(pub(crate) axum::body::Bytes);

impl<S: Send + Sync> axum::extract::FromRequest<S> for ApiBytes {
    type Rejection = ApiError;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, ApiError> {
        match axum::body::Bytes::from_request(req, state).await {
            Ok(bytes) => Ok(Self(bytes)),
            Err(rej) => {
                let status = rej.status();
                let code = if status == StatusCode::PAYLOAD_TOO_LARGE {
                    ErrorCode::TooLarge
                } else {
                    ErrorCode::InvalidRequest
                };
                Err(ApiError::new(status.as_u16(), code, rej.body_text()))
            }
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateDatabaseRequest {
    /// The project to create it in; the caller needs Admin on it.
    pub project_id: ProjectId,
    pub name: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CreateDatabaseAccepted {
    pub job_id: Uuid,
    pub database_id: DatabaseId,
    pub status: DatabaseStatus,
}

#[derive(Debug, Serialize, ToSchema)]
struct Database {
    database_id: DatabaseId,
    project_id: ProjectId,
    tenant_id: TenantId,
    name: String,
    /// The main Postgres database's name: the display name with every character outside
    /// `[A-Za-z0-9_]` turned into `_` ("web-1 prod" is `web_1_prod`). Merges
    /// (`MergeSummary.database`) and SQL name this one. `null` only for a name Postgres can't use.
    #[schema(required = true)]
    postgres_database: Option<String>,
    status: DatabaseStatus,
    #[schema(required = true)]
    host: Option<String>,
    #[schema(required = true)]
    port: Option<u16>,
    #[schema(required = true)]
    error: Option<String>,
    /// Live `normal` branches, main included (backups and deleted branches don't count).
    branch_count: i64,
    /// Seconds of history the database keeps.
    history_window: u64,
    max_branches: i32,
    max_rows: i32,
    statement_timeout_ms: i32,
    max_connections: i32,
}

/// The proxy's host and Postgres port, once the database is ready: until then its endpoint
/// may have no compute yet, or be torn down.
fn proxy_address(
    status: DatabaseStatus,
    endpoint_id: Option<&str>,
    proxy: &ProxyAddress,
) -> Option<(String, u16)> {
    match endpoint_id {
        Some(ep) if status == DatabaseStatus::Ready => {
            Some((proxy_host(ep, &proxy.domain), proxy.pg_port))
        }
        _ => None,
    }
}

/// The main Postgres database's name: the one the worker recorded, else the name it derives
/// from the display name (before provisioning records it). `None` when no name is usable.
fn postgres_database(r: &DatabaseRecord) -> Option<String> {
    r.database_name
        .clone()
        .or_else(|| chelabase_metadata::postgres_database_name(&r.name).ok())
}

/// Main's endpoint, if main has one.
fn main_endpoint(main: Option<&BranchRecord>) -> Option<&str> {
    main.and_then(|b| b.endpoint_id.as_deref())
}

/// Each database's main branch and branch count, for the summaries: one query for the tenant.
async fn main_branches<M: MetadataPort>(
    metadata: &M,
    tenant_id: TenantId,
) -> Result<HashMap<DatabaseId, (BranchRecord, i64)>, MetadataError> {
    Ok(metadata
        .main_branches_with_counts(tenant_id)
        .await?
        .into_iter()
        .map(|(main, count)| (main.database_id, (main, count)))
        .collect())
}

impl Database {
    fn new(
        r: DatabaseRecord,
        main: Option<&BranchRecord>,
        branch_count: i64,
        proxy: &ProxyAddress,
    ) -> Self {
        let (host, port) = proxy_address(r.status, main_endpoint(main), proxy).unzip();
        Self {
            postgres_database: postgres_database(&r),
            database_id: r.database_id,
            project_id: r.project_id,
            tenant_id: r.tenant_id,
            name: r.name,
            status: r.status,
            host,
            port,
            error: r.error,
            branch_count,
            history_window: r.history_window.as_secs(),
            max_branches: r.max_branches,
            max_rows: r.max_rows,
            statement_timeout_ms: r.statement_timeout_ms,
            max_connections: r.max_connections,
        }
    }
}

/// The caller and what they may do. Studio only hides what this rules out; the API decides.
#[derive(Debug, Serialize, ToSchema)]
struct MeResponse {
    sub: String,
    /// The member's email (null when unknown), else the token's.
    #[schema(required = true)]
    email: Option<String>,
    /// The member's name (null when unknown), else the token's.
    #[schema(required = true)]
    name: Option<String>,
    /// `None` when the caller's org isn't bound to a Chelabase tenant yet.
    #[schema(required = true)]
    tenant_id: Option<TenantId>,
    /// The caller's membership in the tenant; `None` when they aren't a member (platform mode
    /// makes no members) or the org isn't bound.
    #[schema(required = true)]
    member: Option<MeMember>,
    /// Whether the caller may use instance routes (self-hosted: an active tenant admin;
    /// platform: a member of the operator organisation).
    instance_admin: bool,
    /// Each project the caller may use, with their platform role and the Postgres roles they
    /// may act as there. A tenant admin is Admin with every role on every project.
    projects: Vec<MeProject>,
    user: MeUser,
    /// The tenant with the projects the caller may use; `None` when the caller's org isn't
    /// bound to a Chelabase tenant yet.
    #[schema(required = true)]
    tenant: Option<Tenant>,
    /// The tenant's active admins, for an invited member (an invite that expired before it was
    /// accepted; R49), who can't read the member list but must know whom to ask. Empty for
    /// everyone else.
    tenant_admins: Vec<MeAdmin>,
}

/// A tenant admin to ask for a new invite.
#[derive(Debug, Serialize, ToSchema)]
struct MeAdmin {
    /// Their display name; empty when they have none.
    name: String,
    #[schema(required = true)]
    email: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
struct MeMember {
    /// The caller's member id (as `listMembers` names them).
    id: Uuid,
    status: MemberStatusView,
    tenant_admin: bool,
}

/// Where a member is: `invited` (not signed in yet), `pending` (signed up, waiting for an
/// admin to approve), `active`, or `removed`.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MemberStatusView {
    Invited,
    Pending,
    Active,
    Removed,
}

impl From<MemberStatus> for MemberStatusView {
    fn from(status: MemberStatus) -> Self {
        match status {
            MemberStatus::Invited => Self::Invited,
            MemberStatus::Pending => Self::Pending,
            MemberStatus::Active => Self::Active,
            MemberStatus::Removed => Self::Removed,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
struct MeProject {
    project_id: ProjectId,
    platform_role: chelabase_types::access::PlatformRole,
    /// The Postgres roles the caller may act as on the project: `"all"` for a tenant admin,
    /// else the names (`owner`, the database's owner, comes with the admin role). A database
    /// the grant is narrowed on may allow less: see `databases`.
    roles: MeRoles,
    /// The databases of the project the caller's grant is narrowed on, with what they may do
    /// there. A database not listed has the project's `platform_role` and `roles`.
    databases: Vec<MeDatabase>,
}

/// The caller's access to one database of a project, where an override narrows it.
#[derive(Debug, Serialize, ToSchema)]
struct MeDatabase {
    database_id: DatabaseId,
    /// The effective platform role: the grant's, or the lower one the override sets.
    platform_role: chelabase_types::access::PlatformRole,
    /// The effective Postgres roles (`owner` only with the admin role).
    roles: MeRoles,
}

#[derive(Debug, Serialize, ToSchema)]
#[serde(untagged)]
enum MeRoles {
    All(AllRoles),
    Only(Vec<String>),
}

/// Every role, the owner included.
#[derive(Debug, Serialize, ToSchema)]
enum AllRoles {
    #[serde(rename = "all")]
    All,
}

#[derive(Debug, Serialize, ToSchema)]
struct MeUser {
    sub: String,
    org_id: String,
}

#[derive(Debug, Serialize, ToSchema)]
struct Tenant {
    id: TenantId,
    slug: String,
    /// The tenant's chosen name; `null` means show the slug.
    #[schema(required = true)]
    display_name: Option<String>,
    projects: Vec<Project>,
}

#[derive(Debug, Serialize, ToSchema)]
struct Project {
    id: ProjectId,
    name: String,
    slug: String,
    status: projects::ProjectStatusView,
    databases: Vec<DatabaseSummary>,
}

/// Public database fields only: never credentials or container ids.
#[derive(Debug, Serialize, ToSchema)]
struct DatabaseSummary {
    database_id: DatabaseId,
    name: String,
    /// The main Postgres database's name: the display name with every character outside
    /// `[A-Za-z0-9_]` turned into `_` ("web-1 prod" is `web_1_prod`). Merges
    /// (`MergeSummary.database`) and SQL name this one. `null` only for a name Postgres can't use.
    #[schema(required = true)]
    postgres_database: Option<String>,
    status: DatabaseStatus,
    #[schema(required = true)]
    host: Option<String>,
    #[schema(required = true)]
    port: Option<u16>,
    /// Live `normal` branches, main included.
    branch_count: i64,
}

impl Tenant {
    fn new(
        t: TenantOverview,
        mains: &HashMap<DatabaseId, (BranchRecord, i64)>,
        proxy: &ProxyAddress,
    ) -> Self {
        Self {
            id: t.id,
            slug: t.slug,
            display_name: t.display_name,
            projects: t
                .projects
                .into_iter()
                .map(|p| Project::new(p, mains, proxy))
                .collect(),
        }
    }
}

impl Project {
    fn new(
        p: ProjectOverview,
        mains: &HashMap<DatabaseId, (BranchRecord, i64)>,
        proxy: &ProxyAddress,
    ) -> Self {
        Self {
            id: p.id,
            name: p.name,
            slug: p.slug,
            status: p.status.into(),
            databases: p
                .databases
                .into_iter()
                .map(|d| {
                    let main = mains.get(&d.database_id);
                    let (host, port) =
                        proxy_address(d.status, main_endpoint(main.map(|m| &m.0)), proxy).unzip();
                    DatabaseSummary {
                        postgres_database: postgres_database(&d),
                        database_id: d.database_id,
                        name: d.name,
                        status: d.status,
                        host,
                        port,
                        branch_count: main.map_or(0, |m| m.1),
                    }
                })
                .collect(),
        }
    }
}

/// The documented routes: everything but `/health`. `build_router` serves them and
/// `openapi()` describes them.
pub(crate) fn api_router<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new()
        .routes(routes!(create_database))
        // GET, PATCH and DELETE share one path (chelabase-routes GET_DATABASE / UPDATE_DATABASE /
        // DELETE_DATABASE).
        .routes(routes!(get_database, branches::update_database, delete_database))
        .routes(routes!(database_connection))
        .routes(routes!(reset_password))
        .routes(routes!(me))
        .merge(branches::routes())
        .merge(merges::routes())
        .merge(data_keys::routes())
        .merge(data_api_databases::routes())
        .merge(console::routes())
        .merge(sql_editor::routes())
        .merge(roles::routes())
        .merge(projects::routes())
        .merge(app_users::routes())
        .merge(app_user_admin::routes())
        .merge(members::routes())
}

/// Build the orchestrator's axum router over the given (possibly-fake) state.
pub fn build_router<M, J>(state: Arc<AppState<M, J>>) -> Router
where
    M: MetadataPort,
    J: JobPort,
{
    let (router, _) = api_router().split_for_parts();
    router
        .route(routes::HEALTH.path, get(health))
        .merge(openapi::router())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok", "service": "orchestrator" }))
}

#[utoipa::path(
    get,
    path = "/v1/me",
    operation_id = "getMe",
    tag = "me",
    summary = "The caller, their capabilities and their tenant",
    responses(
        (status = 200, description = "The signed-in user, their membership and what they may use, and, once their org is bound, the tenant with the projects and databases they may use", body = MeResponse),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn me<M, J>(State(state): State<Arc<AppState<M, J>>>, caller: Caller) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    match me_response(&state, caller).await {
        Ok(me) => Json(me).into_response(),
        Err(err) => err.into_response(),
    }
}

async fn me_response<M, J>(state: &AppState<M, J>, caller: Caller) -> Result<MeResponse, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    use chelabase_types::access::{effective, PlatformRole, RoleSet};
    let failed = |what: &str, err: MetadataError| {
        error!("{what}: {err}");
        ApiError::internal()
    };
    let tenant_id = lookup_tenant(state, &caller.org_id).await.map_err(|_| ApiError::internal())?;
    let member = match tenant_id {
        Some(tenant) => resolve_member(state, tenant, &caller).await?,
        None => None,
    };
    let active = member.as_ref().filter(|m| m.status == MemberStatus::Active);
    let tenant_admin = active.is_some_and(|m| m.tenant_admin);
    let instance_admin = state.policy.instance_admin(tenant_admin, in_operator_org(state, &caller.org_id));

    // The projects the caller may use: all of them for a tenant admin, else their grants.
    let projects = match (tenant_id, active) {
        (Some(tenant), Some(_)) if tenant_admin => state
            .metadata
            .list_projects(tenant)
            .await
            .map_err(|e| failed("list projects", e))?
            .into_iter()
            .map(|p| MeProject {
                project_id: p.id,
                platform_role: PlatformRole::Admin,
                roles: MeRoles::All(AllRoles::All),
                databases: Vec::new(),
            })
            .collect(),
        (Some(_), Some(m)) => {
            let grants = state
                .metadata
                .member_projects(m.id)
                .await
                .map_err(|e| failed("member projects", e))?;
            let overrides = state
                .metadata
                .member_overrides(m.id)
                .await
                .map_err(|e| failed("member overrides", e))?;
            let roles_of = |roles: RoleSet| match roles {
                RoleSet::All => MeRoles::All(AllRoles::All),
                RoleSet::Only(set) => MeRoles::Only(set.iter().map(|r| r.as_str().to_string()).collect()),
            };
            grants
                .into_iter()
                .map(|(project, row)| {
                    let base = access::grant_of(&row);
                    let grant = effective(&base, None);
                    let databases = overrides
                        .iter()
                        .filter(|(p, _, _)| *p == project)
                        .map(|(_, database, o)| {
                            let narrowed = effective(&base, Some(&access::narrowing_of(o)));
                            MeDatabase {
                                database_id: DatabaseId(*database),
                                platform_role: narrowed.platform_role,
                                roles: roles_of(narrowed.roles),
                            }
                        })
                        .collect();
                    MeProject {
                        project_id: ProjectId(project),
                        platform_role: grant.platform_role,
                        roles: roles_of(grant.roles),
                        databases,
                    }
                })
                .collect()
        }
        _ => Vec::new(),
    };

    let tenant = match tenant_id {
        None => None,
        Some(tenant_id) => match state.metadata.tenant_overview(tenant_id).await {
            Ok(None) => None,
            Ok(Some(mut overview)) => {
                let visible: HashSet<ProjectId> = projects.iter().map(|p| p.project_id).collect();
                overview.projects.retain(|p| tenant_admin || visible.contains(&p.id));
                let mains = main_branches(&state.metadata, overview.id)
                    .await
                    .map_err(|e| failed("main branches", e))?;
                Some(Tenant::new(overview, &mains, &state.proxy))
            }
            Err(err) => return Err(failed("tenant overview", err)),
        },
    };
    let (email, name) = match &member {
        Some(m) => (m.email.clone(), Some(m.display_name.clone()).filter(|n| !n.is_empty())),
        None => (caller.email.clone(), caller.name.clone()),
    };
    // R49: an invited member (their invite expired before it was accepted) learns whom to ask.
    let tenant_admins = match (tenant_id, &member) {
        (Some(tenant), Some(m)) if m.status == MemberStatus::Invited => state
            .metadata
            .list_members(tenant)
            .await
            .map_err(|e| failed("tenant admins", e))?
            .into_iter()
            .filter(|l| l.tenant_admin && l.status == MemberStatus::Active)
            .map(|l| MeAdmin { name: l.display_name.clone(), email: l.email.clone() })
            .collect(),
        _ => Vec::new(),
    };
    Ok(MeResponse {
        sub: caller.sub.clone(),
        email,
        name,
        tenant_id,
        member: member.as_ref().map(|m| MeMember { id: m.id, status: m.status.into(), tenant_admin: m.tenant_admin }),
        instance_admin,
        projects,
        user: MeUser { sub: caller.sub, org_id: caller.org_id },
        tenant,
        tenant_admins,
    })
}

#[utoipa::path(
    post,
    path = "/v1/databases",
    operation_id = "createDatabase",
    tag = "databases",
    summary = "Create a database",
    description = "Returns 202 at once. The database is provisioned by a background job.",
    request_body = CreateDatabaseRequest,
    responses(
        (status = 202, description = "Accepted; the database is provisioned in the background (poll it until `ready`)", body = CreateDatabaseAccepted),
        (status = 400, description = "`invalid_request`: the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "the project doesn't exist or is another tenant's", body = ErrorEnvelope),
        (status = 409, description = "`name_taken`: a database with this name exists in the project; `project_deleting`: the project is being deleted", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the body is valid JSON but a field is missing, has the wrong type, or the name is blank or invalid", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`worker_unavailable`: the worker or job queue can't be reached", body = ErrorEnvelope),
    ),
)]
async fn create_database<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiJson(body): ApiJson<CreateDatabaseRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    if body.name.trim().is_empty() {
        return ApiError::invalid_request(422, "name is required").into_response();
    }
    if let Err(err) = chelabase_metadata::postgres_database_name(&body.name) {
        return ApiError::invalid_request(422, err.to_string()).into_response();
    }
    // The project comes from the body: a missing or another tenant's one is 404.
    if let Err(err) = authorize(
        &state,
        &principal,
        routes::CREATE_DATABASE.policy.access,
        Target::Project(body.project_id.as_uuid()),
    )
    .await
    {
        return err.into_response();
    }

    let job_id = Uuid::new_v4();
    let database_id = DatabaseId::new();

    if let Err(err) = state
        .metadata
        .insert_database_pending(database_id, principal.tenant_id, body.project_id, &body.name)
        .await
    {
        return map_metadata_error(err);
    }

    let job = JobEnvelope {
        job_id,
        payload: ProvisionDatabaseJob {
            database_id,
            project_id: body.project_id,
            tenant_id: principal.tenant_id,
            name: body.name.clone(),
        },
    };

    if let Err(err) = state.js.publish_provision(&job).await {
        error!("publish failed: {err}");
        let _ = state
            .metadata
            .set_failed(database_id, &format!("publish failed: {err}"))
            .await;
        return ApiError::new(
            503,
            ErrorCode::WorkerUnavailable,
            "failed to enqueue provision job",
        )
        .into_response();
    }

    info!(
        %job_id,
        %database_id,
        name = %body.name,
        request_id = %principal.request_id,
        "accepted create database"
    );

    (
        StatusCode::ACCEPTED,
        Json(CreateDatabaseAccepted {
            job_id,
            database_id,
            status: DatabaseStatus::Pending,
        }),
    )
        .into_response()
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}",
    operation_id = "getDatabase",
    tag = "databases",
    summary = "Get a database",
    params(("id" = Uuid, Path, description = "Database id")),
    responses(
        (status = 200, description = "The database with its settings and main endpoint", body = Database),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database doesn't exist, or belongs to another tenant", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn get_database<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    // Tombstones, another tenant's database and one the caller has no grant on all answer
    // exactly what a missing one does.
    let record = match authorize_database(
        &state,
        &principal,
        routes::GET_DATABASE.policy.access,
        Target::Database(id),
    )
    .await
    {
        Ok(record) => record,
        Err(err) => return err.into_response(),
    };
    let database_id = record.database_id;
    let branches = match state.metadata.list_branches(database_id).await {
        Ok(branches) => branches,
        Err(err) => {
            error!("list branches: {err}");
            return ApiError::internal().into_response();
        }
    };
    let count = branches.iter().filter(|b| b.kind == BranchKind::Normal).count() as i64;
    let main = branches.iter().find(|b| b.id == BranchId::from(database_id));
    (
        StatusCode::OK,
        Json(Database::new(record, main, count, &state.proxy)),
    )
        .into_response()
}

/// The database's main branch, if it has one.
async fn load_main<M, J>(
    state: &AppState<M, J>,
    database_id: DatabaseId,
) -> Result<Option<BranchRecord>, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    state
        .metadata
        .get_branch(BranchId::from(database_id))
        .await
        .map_err(|err| {
            error!("load main branch: {err}");
            ApiError::internal()
        })
}

/// The 202 body of the routes that start background work: the status the row moved to.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct OperationAccepted {
    pub(crate) status: &'static str,
}

/// How to connect to a branch as one of its roles.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct ConnectionDetails {
    host: String,
    port: u16,
    database: String,
    user: String,
    /// Null when it can't be shown; `password_state` says why.
    #[schema(required = true)]
    password: Option<String>,
    /// `generated`: made by Chelabase, shown here. `set_in_sql`: set in SQL, only its hash is
    /// stored; reset it to get a shown one. `md5_unusable`: an MD5 hash, which Chelabase's proxy
    /// can't use; set a new password. `unreadable`: the stored password could not be opened
    /// (a rename or branch copy dropped it); reset it.
    password_state: PasswordState,
    /// Without a password when `password` is null. The user and database are
    /// percent-encoded.
    uri: String,
    http_url: String,
    /// Self-hosted mode only: the file holding the dev proxy's CA certificate, for
    /// `sslrootcert=` (the URI keeps `sslmode=verify-full`). Null in platform mode.
    #[schema(required = true)]
    ca_file: Option<String>,
}

/// Percent-encodes everything but RFC 3986's unreserved characters.
fn uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// What a backup answers to reveal, reset-password, reset-to-parent and restore.
const BACKUP_REFUSAL: &str =
    "this is a restore backup with no compute; create a branch from it to connect";

/// Reveal the owner credentials of a ready database (main's). Never cached, never logged.
#[utoipa::path(
    get,
    path = "/v1/databases/{id}/connection",
    operation_id = "getDatabaseConnection",
    tag = "databases",
    summary = "Reveal a database's connection details",
    description = "The owner credentials of the database's main branch. Never cached (`Cache-Control: no-store`) and never logged.",
    params(("id" = Uuid, Path, description = "Database id")),
    responses(
        (status = 200, description = "The owner credentials; never cached", body = ConnectionDetails),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database doesn't exist, or belongs to another tenant", body = ErrorEnvelope),
        (status = 409, description = "`database_not_ready`: the database isn't ready; `branch_not_ready`: its main isn't ready; `credentials_unavailable`: the owner credentials are missing", body = ErrorEnvelope),
        (status = 500, description = "`credentials_unavailable`: the stored credentials can't be opened with the current secrets key; `internal`: unexpected", body = ErrorEnvelope),
    ),
)]
async fn database_connection<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match authorize_database(
        &state,
        &principal,
        routes::DATABASE_CONNECTION.policy.access,
        Target::Database(id),
    )
    .await
    {
        Ok(record) => record,
        Err(resp) => return resp.into_response(),
    };
    if record.status != DatabaseStatus::Ready {
        return ApiError::new(
            409,
            ErrorCode::DatabaseNotReady,
            format!(
                "database is {}; connection details are available once it is ready",
                record.status.as_str()
            ),
        )
        .into_response();
    }
    // Main holds the database's endpoint and sealed password (AAD = its id = the database's).
    let main = match load_main(&state, record.database_id).await {
        Ok(main) => main,
        Err(resp) => return resp.into_response(),
    };
    reveal(&state, &principal, &record, main, None, None).await
}

/// The connection details of `branch`, a branch of `record`'s database, as `role` (the owner
/// by default) for `database` (the recorded main database by default).
async fn reveal<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    record: &DatabaseRecord,
    branch: Option<BranchRecord>,
    role: Option<&str>,
    database: Option<&str>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let Some(branch) = branch else {
        return ApiError::new(
            409,
            ErrorCode::CredentialsUnavailable,
            "credentials unavailable; recreate this database",
        )
        .into_response();
    };
    if branch.kind == BranchKind::Backup {
        return ApiError::new(409, ErrorCode::BackupBranch, BACKUP_REFUSAL).into_response();
    }
    if branch.status != BranchStatus::Ready {
        return ApiError::new(
            409,
            ErrorCode::BranchNotReady,
            format!(
                "branch is {}; connection details are available once it is ready",
                branch.status.as_str()
            ),
        )
        .into_response();
    }
    let owner = roles::owner_name(record).to_string();
    // The owner by default or by name; any other name is a role of the branch.
    let other_role = role.filter(|name| *name != owner);
    if let Some(name) = other_role
        && let Err(err) = roles::check_any_name(name)
    {
        return err.into_response();
    }
    let sealed = branch.owner_password_enc.filter(|s| !s.is_empty());
    // The recorded main database is the default, even while it's missing (R24).
    let (Some(endpoint_id), Some(main_database)) = (branch.endpoint_id, record.database_name.clone())
    else {
        return ApiError::new(
            409,
            ErrorCode::CredentialsUnavailable,
            "credentials unavailable; recreate this database",
        )
        .into_response();
    };
    let credentials_unreadable = || {
        ApiError::new(
            500,
            ErrorCode::CredentialsUnavailable,
            "stored credentials can't be decrypted with the current secrets key; \
             restore the original CHELABASE_SECRETS_KEY file",
        )
        .into_response()
    };
    let (user, password, password_state) = match other_role {
        None => match sealed {
            // No sealed password: the owner's was set in SQL.
            None => (owner, None, PasswordState::SetInSql),
            Some(sealed) => {
                match open_credential(&state.secrets, &branch.id.to_string(), Purpose::Owner, &sealed) {
                    Ok(password) => (owner, Some(password), PasswordState::Generated),
                    Err(err) => {
                        error!(database_id = %record.database_id, branch_id = %branch.id, "decrypt owner password: {err}");
                        return credentials_unreadable();
                    }
                }
            }
        },
        Some(name) => {
            let rows = match state.metadata.list_roles(&endpoint_id).await {
                Ok(rows) => rows,
                Err(err) => {
                    error!(branch_id = %branch.id, "list roles: {err}");
                    return ApiError::internal().into_response();
                }
            };
            let Some(row) = rows.into_iter().find(|r| r.name == name) else {
                return ApiError::not_found("role not found").into_response();
            };
            let password = if row.password == PasswordState::Generated {
                let sealed = match state.metadata.role_password(&endpoint_id, name).await {
                    Ok(sealed) => sealed,
                    Err(err) => {
                        error!(branch_id = %branch.id, "load role password: {err}");
                        return ApiError::internal().into_response();
                    }
                };
                match sealed {
                    Some(sealed) => {
                        match open_role_password(&state.secrets, &branch.id.to_string(), name, &sealed) {
                            Ok(password) => Some(password),
                            Err(err) => {
                                error!(branch_id = %branch.id, role = %name, "decrypt role password: {err}");
                                return credentials_unreadable();
                            }
                        }
                    }
                    None => None,
                }
            } else {
                None
            };
            (name.to_string(), password, row.password)
        }
    };
    let database = match database {
        None => main_database,
        Some(name) => match state.metadata.branch_databases(branch.id).await {
            Ok(rows) if rows.iter().any(|d| d.name == name) => name.to_string(),
            Ok(_) => return ApiError::not_found("database not found").into_response(),
            Err(err) => {
                error!(branch_id = %branch.id, "list branch databases: {err}");
                return ApiError::internal().into_response();
            }
        },
    };
    info!(
        request_id = %principal.request_id,
        sub = %principal.sub,
        database_id = %record.database_id,
        branch_id = %branch.id,
        role = %user,
        "revealed connection details"
    );
    let host = proxy_host(&endpoint_id, &state.proxy.domain);
    let port = state.proxy.pg_port;
    let userinfo = match &password {
        Some(password) => format!("{}:{}", uri_component(&user), uri_component(password)),
        None => uri_component(&user),
    };
    let uri = format!(
        "postgresql://{userinfo}@{host}:{port}/{}?sslmode=verify-full",
        uri_component(&database)
    );
    let http_url = format!("https://{host}:{}/sql", state.proxy.http_port);
    (
        [(CACHE_CONTROL, "no-store")],
        Json(ConnectionDetails {
            host,
            port,
            database,
            user,
            password,
            password_state,
            uri,
            http_url,
            ca_file: state.policy.ca_file_on_reveal().then(|| state.proxy_ca_file.clone()),
        }),
    )
        .into_response()
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/reset-password",
    operation_id = "resetDatabasePassword",
    tag = "databases",
    summary = "Reset a database's owner password",
    params(("id" = Uuid, Path, description = "Database id")),
    responses(
        (status = 204, description = "The password was reset; read the new one from the connection details"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database doesn't exist, or belongs to another tenant", body = ErrorEnvelope),
        (status = 409, description = "`database_not_ready`: the database isn't ready; `branch_not_ready`: the branch isn't ready; `credentials_unavailable`: the owner credentials are missing", body = ErrorEnvelope),
        (status = 500, description = "`internal`: the worker failed to reset the password", body = ErrorEnvelope),
        (status = 503, description = "`not_applied`: the new password is stored but not applied yet and will be retried; `worker_unavailable`: the worker can't be reached", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the worker or database didn't answer in time", body = ErrorEnvelope),
    ),
)]
async fn reset_password<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match authorize_database(
        &state,
        &principal,
        routes::RESET_PASSWORD.policy.access,
        Target::Database(id),
    )
    .await
    {
        Ok(record) => record,
        Err(resp) => return resp.into_response(),
    };
    if record.status != DatabaseStatus::Ready {
        return ApiError::new(409, ErrorCode::DatabaseNotReady, "database is not ready")
            .into_response();
    }
    let main = match load_main(&state, record.database_id).await {
        Ok(main) => main,
        Err(resp) => return resp.into_response(),
    };
    reset_branch_password(&state, &principal, &record, main).await
}

/// Resets the owner password of `branch`, a branch of `record`'s database, through the worker.
async fn reset_branch_password<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    record: &DatabaseRecord,
    branch: Option<BranchRecord>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    // A branch without an endpoint has nothing to reset; say so instead of failing inside
    // the worker.
    let Some(branch) = branch else {
        return ApiError::new(
            409,
            ErrorCode::CredentialsUnavailable,
            "credentials unavailable; recreate this database",
        )
        .into_response();
    };
    if branch.kind == BranchKind::Backup {
        return ApiError::new(409, ErrorCode::BackupBranch, BACKUP_REFUSAL).into_response();
    }
    if branch.status != BranchStatus::Ready {
        return ApiError::new(409, ErrorCode::BranchNotReady, "branch is not ready").into_response();
    }
    if branch.endpoint_id.is_none() {
        return ApiError::new(
            409,
            ErrorCode::CredentialsUnavailable,
            "credentials unavailable; recreate this database",
        )
        .into_response();
    }
    let (database_id, branch_id) = (record.database_id, branch.id);
    match state.js.reset_password(database_id, branch_id).await {
        Ok(reply) if reply.ok => {
            info!(request_id = %principal.request_id, %database_id, %branch_id, "owner password reset");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(reply) if reply.error.as_deref() == Some("not ready") => {
            ApiError::new(409, ErrorCode::BranchNotReady, "branch is not ready").into_response()
        }
        Ok(reply) if reply.error.as_deref() == Some(RESET_NOT_APPLIED) => {
            ApiError::new(503, ErrorCode::NotApplied, RESET_NOT_APPLIED).into_response()
        }
        Ok(reply) if reply.error.as_deref() == Some("timed out") => ApiError::new(
            504,
            ErrorCode::Timeout,
            "the database did not respond; try again",
        )
        .into_response(),
        Ok(reply) => {
            error!(%database_id, %branch_id, "reset password failed: {:?}", reply.error);
            ApiError::new(500, ErrorCode::Internal, "password reset failed").into_response()
        }
        Err(ResetError::Timeout) => {
            ApiError::new(504, ErrorCode::Timeout, "worker did not respond; try again")
                .into_response()
        }
        Err(ResetError::Transport(err)) => {
            error!(%database_id, %branch_id, "reset password: {err}");
            ApiError::new(503, ErrorCode::WorkerUnavailable, "worker unavailable").into_response()
        }
    }
}

/// Start deleting a database (tombstone). The worker removes the container and
/// its data; a lost job is retried by the worker's reconcile loop.
#[utoipa::path(
    delete,
    path = "/v1/databases/{id}",
    operation_id = "deleteDatabase",
    tag = "databases",
    summary = "Delete a database",
    params(("id" = Uuid, Path, description = "Database id")),
    responses(
        (status = 202, description = "Accepted; the database moves to `deleting` and is removed in the background", body = OperationAccepted),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database doesn't exist, or belongs to another tenant", body = ErrorEnvelope),
        (status = 409, description = "`already_deleting`: the database is already being deleted; `branch_protected`: a branch of the database is protected, unprotect it first; `conflict`: the database holds the project's app users, disable app users first", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn delete_database<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match authorize_database(
        &state,
        &principal,
        routes::DELETE_DATABASE.policy.access,
        Target::Database(id),
    )
    .await
    {
        Ok(record) => record,
        Err(resp) => return resp.into_response(),
    };
    // The database that holds a project's app users can't go while they are enabled (R20: the
    // app-user routes act on it, and disabling needs it).
    match state.metadata.app_auth_for_database(record.database_id).await {
        Ok(None) => {}
        Ok(Some(_)) => {
            return ApiError::new(
                409,
                ErrorCode::Conflict,
                "this database holds the project's app users; disable app users first",
            )
            .into_response();
        }
        Err(err) => {
            error!("app auth for database: {err}");
            return ApiError::internal().into_response();
        }
    }
    match state.metadata.begin_delete(record.database_id).await {
        Ok(BeginDelete::Started) => {}
        Ok(BeginDelete::Protected) => {
            return ApiError::new(
                409,
                ErrorCode::BranchProtected,
                "a branch of this database is protected; unprotect it first",
            )
            .into_response()
        }
        Ok(BeginDelete::NotReady) => {
            return ApiError::new(409, ErrorCode::AlreadyDeleting, "database is already being deleted")
                .into_response()
        }
        Err(err) => {
            error!("begin delete: {err}");
            return ApiError::internal().into_response();
        }
    }
    let job = JobEnvelope {
        job_id: Uuid::new_v4(),
        payload: DeleteDatabaseJob {
            database_id: record.database_id,
        },
    };
    if let Err(err) = state.js.publish_delete(&job).await {
        error!(database_id = %record.database_id, "publish delete failed (reconcile will retry): {err}");
    }
    info!(request_id = %principal.request_id, database_id = %record.database_id, "delete requested");
    (
        StatusCode::ACCEPTED,
        Json(OperationAccepted { status: "deleting" }),
    )
        .into_response()
}

fn map_metadata_error(err: MetadataError) -> Response {
    match err {
        MetadataError::TenantNotFound(_) | MetadataError::ProjectNotFound(_) => {
            ApiError::not_found(err.to_string()).into_response()
        }
        // Another tenant's project looks exactly like a missing one.
        MetadataError::ProjectTenantMismatch { project_id, .. } => {
            ApiError::not_found(MetadataError::ProjectNotFound(project_id).to_string())
                .into_response()
        }
        MetadataError::ProjectDeleting(_) => {
            ApiError::new(409, ErrorCode::ProjectDeleting, "project is being deleted").into_response()
        }
        MetadataError::DuplicateDatabaseName => ApiError::new(
            409,
            ErrorCode::NameTaken,
            "database name already exists in project",
        )
        .into_response(),
        other => {
            error!("metadata error: {other}");
            ApiError::internal().into_response()
        }
    }
}

#[cfg(test)]
mod proxy_address_tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn the_proxy_defaults_to_4432_and_4444() {
        let p = ProxyAddress::from_lookup(lookup(&[])).unwrap();
        assert_eq!(p, ProxyAddress::local("localtest.me"));
        assert_eq!((p.pg_port, p.http_port), (4432, 4444));
        assert_eq!(p.domain, "localtest.me");
    }

    #[test]
    fn the_proxy_ports_and_domain_come_from_the_environment() {
        let p = ProxyAddress::from_lookup(lookup(&[
            ("CHELABASE_PROXY_PG_PORT", "4532"),
            ("CHELABASE_PROXY_HTTP_PORT", "4544"),
            ("CHELABASE_PROXY_DOMAIN", "example.test"),
        ]))
        .unwrap();
        assert_eq!((p.pg_port, p.http_port), (4532, 4544));
        assert_eq!(p.domain, "example.test");
        assert_eq!(
            proxy_address(DatabaseStatus::Ready, Some("ep-1"), &p),
            Some(("ep-1.example.test".to_string(), 4532))
        );
    }

    #[test]
    fn a_proxy_port_that_isnt_a_port_is_refused() {
        for bad in ["x", "0", "70000"] {
            let err = ProxyAddress::from_lookup(lookup(&[("CHELABASE_PROXY_PG_PORT", bad)]))
                .unwrap_err()
                .to_string();
            assert!(err.contains("CHELABASE_PROXY_PG_PORT"), "{bad}: {err}");
        }
        let blank = ProxyAddress::from_lookup(lookup(&[("CHELABASE_PROXY_HTTP_PORT", " ")]));
        assert_eq!(blank.unwrap().http_port, 4444);
    }
}
