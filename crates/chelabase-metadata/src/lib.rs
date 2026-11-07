//! Metadata store: Tenant → Project → Database lookup and provisioning state.

use chelabase_types::{DatabaseId, ProjectId, TenantId};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};
use thiserror::Error;
use tracing::info;
use uuid::Uuid;

mod app_users;
mod branches;
mod data_api;
mod ddl;
mod functions;
pub mod members;
mod merges;
mod neon;
pub mod personal_logins;
mod projects;
mod saved_queries;
mod schema_bases;
mod storage_tokens;
mod storage_webhooks;
pub mod teams;
pub use app_users::{
    AppSession, AppUsersStatus, BeginEnable, BranchAuthSync, NewAppSession, NewProjectAuth, OnDelete,
    PeekOutcome, ProjectAuth, RefreshNext, RefreshOutcome,
};
pub use projects::{
    BeginProjectDelete, ProjectRecord, ProjectStatus, RenameProject, slug_base, valid_project_name,
};
pub use branches::{
    BranchDeleteOutcome, BranchKind, BranchRecord, BranchStatus, ClaimOutcome,
    CreateBranchOutcome, DatabaseSettings, LiveBranch, NewBranch, SettingsPatch,
    merge_busy_refusal,
};
pub use saved_queries::{
    SAVED_QUERY_CAP, SavedQuery, SavedQueryEditor, SavedQueryPatch, SavedQuerySummary, SavedQueryWrite,
};
pub use storage_webhooks::{
    DeliveryTarget, FailureRun, NewStorageWebhook, STORAGE_WEBHOOK_ATTEMPTS_KEPT,
    STORAGE_WEBHOOK_CAP, STORAGE_WEBHOOK_RETRY_WINDOW, StorageWebhook, StorageWebhookAttempt,
    StorageWebhookPatch,
};
pub use ddl::DdlTx;
pub use functions::{
    Advance, BranchFunction, ClaimedSchedule, DeploymentRecord, FunctionAuth, FunctionKind,
    FunctionRecord, MAX_BUNDLE_SIZE, MAX_LAST_ERROR_CHARS, MAX_SECRET_BYTES, NewDeployment,
    NewSchedule, NewTrigger, ResolvedFunction, RunAs, Schedule, ScheduleError, ScheduleRecord,
    ServiceTarget, TriggerInstall, TriggerRecord, TriggerSource, TriggerState, advance,
    valid_function_name, valid_secret_name,
};
pub use merges::{
    APPLY_PHASES, BranchBusy, MergeAction, MergeCreate, MergeDirection, MergeDraftPatch,
    MergeEvent, MergeRecord, MergeState, MergeStatus, MergeStatusPatch, MergeWrite, NewMerge,
    ONLINE_PHASE,
};
pub use schema_bases::{BaseSource, SchemaBase};
pub use data_api::{
    BranchConnectionRow, DataApiSettings, DataKeyLookup, DataKeyRecord, KeyRoleFix, NewDataKey,
    ServedDatabase, rotated_identity,
};
pub use neon::{
    BranchDatabase, CopyUpsert, DataApiState, DatabaseOrigin, EnableStart, ENDPOINT_LOCK_TIMEOUT, EndpointLock,
    EndpointState, InsertRoleOutcome, NeonDatabase, NeonEndpoint, NeonRole, NewEndpoint,
    NewRoleCopy, NewStaticEndpoint, ObservedDatabase, PasswordState, ResyncApplied, ResyncWrite, RoleCopy,
    RoleLevel, RoleOrigin, RoleRow, SealedChange,
};
pub use neon::{
    ApiRoleChange, NewApiRole, RenameCopyOutcome, RoleAttributes, RoleWriteOutcome, SecretChange,
};

/// Fixed seed tenant for local demos (`slug = demo`).
pub const DEMO_TENANT_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0001);
/// Fixed seed project for local demos (`slug = default`).
pub const DEMO_PROJECT_ID: Uuid = Uuid::from_u128(0x0000_0000_0000_4000_8000_0000_0000_0002);

/// Connection configuration for the control-plane metadata Postgres.
#[derive(Clone)]
pub struct MetadataConfig {
    pub database_url: String,
}

/// The URL without its passwords (the user info's, from `CHELABASE_METADATA_DATABASE_PASSWORD_FILE`,
/// and any `password` query parameter's).
impl std::fmt::Debug for MetadataConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MetadataConfig")
            .field("database_url", &chelabase_types::secret_env::redact_url_passwords(&self.database_url))
            .finish()
    }
}


impl MetadataConfig {
    pub fn from_env() -> Result<Self, MetadataError> {
        let database_url = std::env::var("CHELABASE_METADATA_DATABASE_URL").map_err(|_| {
            MetadataError::Config("CHELABASE_METADATA_DATABASE_URL is required".into())
        })?;
        Ok(Self { database_url })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DatabaseRecord {
    pub database_id: DatabaseId,
    pub project_id: ProjectId,
    pub tenant_id: TenantId,
    pub name: String,
    pub status: DatabaseStatus,
    /// Actual Postgres database name (sanitized from `name`).
    pub database_name: Option<String>,
    /// The revealed owner role (`postgres`).
    pub owner_role: Option<String>,
    pub error: Option<String>,
    /// The Neon tenant holding every branch's timeline (32 hex), once provisioning chose one.
    pub neon_tenant_id: Option<String>,
    /// How far back branches can be made from; the tenant's `pitr_interval`.
    pub history_window: std::time::Duration,
    /// Live `normal` branches allowed besides main.
    pub max_branches: i32,
    /// Data API: rows a response may hold.
    pub max_rows: i32,
    /// Data API: per-statement timeout.
    pub statement_timeout_ms: i32,
    /// Data API: connections this database's pool may hold.
    pub max_connections: i32,
    /// Object storage: connections the storage service may hold to each branch.
    pub storage_max_connections: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub enum DatabaseStatus {
    Pending,
    Provisioning,
    Ready,
    Failed,
    Deleting,
    Deleted,
}

impl DatabaseStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Provisioning => "provisioning",
            Self::Ready => "ready",
            Self::Failed => "failed",
            Self::Deleting => "deleting",
            Self::Deleted => "deleted",
        }
    }

    pub fn parse(value: &str) -> Result<Self, MetadataError> {
        match value {
            "pending" => Ok(Self::Pending),
            "provisioning" => Ok(Self::Provisioning),
            "ready" => Ok(Self::Ready),
            "failed" => Ok(Self::Failed),
            "deleting" => Ok(Self::Deleting),
            "deleted" => Ok(Self::Deleted),
            other => Err(MetadataError::InvalidStatus(other.to_string())),
        }
    }
}

#[derive(Debug, Error)]
pub enum MetadataError {
    #[error("metadata config error: {0}")]
    Config(String),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("migrate error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("tenant not found: {0}")]
    TenantNotFound(TenantId),
    /// A person with no member row asked in while the tenant takes no sign-ups (R39); no row
    /// was written.
    #[error("not a member of tenant {0}, and sign-up is closed")]
    NotAdmitted(TenantId),
    #[error("project not found: {0}")]
    ProjectNotFound(ProjectId),
    /// The project is `deleting` or `deleted`: nothing may be added to it.
    #[error("project {0} is being deleted")]
    ProjectDeleting(ProjectId),
    #[error("project {project_id} does not belong to tenant {tenant_id}")]
    ProjectTenantMismatch {
        project_id: ProjectId,
        tenant_id: TenantId,
    },
    #[error("database name already exists in project")]
    DuplicateDatabaseName,
    #[error("invalid status: {0}")]
    InvalidStatus(String),
    /// The parent's `protected` flag changed between choosing a child's role copies and
    /// recording its endpoint; the copies must be chosen again.
    #[error("the parent branch's protection changed")]
    ParentProtectionChanged,
    #[error("invalid branch kind: {0}")]
    InvalidBranchKind(String),
    /// A branch this write would insert has the name of a live one (`branches_live_name`).
    #[error("a live branch named {0:?} already exists")]
    BranchNameTaken(String),
    /// A write would break a uniqueness rule the caller can resolve (a second live public
    /// data key on a branch).
    #[error("conflicting write")]
    Conflict,
    #[error("invalid database name: {0}")]
    InvalidDatabaseName(String),
    /// [`MetadataClient::lock_endpoint`] waited [`ENDPOINT_LOCK_TIMEOUT`] in vain.
    #[error("another password change for {0} is still being applied")]
    EndpointLockTimeout(String),
    /// A secret key's role was dropped and created again under its name (another OID): the
    /// key can't be rotated onto the new role.
    #[error("the role {0} was dropped and created again")]
    RoleRecreated(String),
    /// A function write the caller can fix: a bad name, size or hash, a deployment of another
    /// function, trigger columns that don't fit the source. Nothing was written.
    #[error("invalid function write: {0}")]
    InvalidFunction(String),
    /// The function, or the branch or database a function write names, is missing,
    /// tombstoned, being deleted or another database's. Nothing was written.
    #[error("function, branch or database not found")]
    FunctionNotFound,
    /// A sealed value didn't open (wrong key, or a value made for another row).
    #[error("secret error: {0}")]
    Secret(String),
    /// The function is active on a live protected branch: deleting it or changing its kind
    /// would change that branch, which takes the branch's own (Admin) route. Nothing was
    /// written.
    #[error("the function is active on a protected branch")]
    FunctionActiveOnProtectedBranch,
    /// The data key is a function's run-as key: only its function revokes or replaces it.
    /// Nothing was written.
    #[error("the key is managed by a function")]
    ManagedKey,
}

impl MetadataError {
    /// Postgres refused the write because the unique index `constraint` would be violated.
    pub fn is_unique_violation(&self, constraint: &str) -> bool {
        matches!(self, Self::Database(sqlx::Error::Database(e))
            if e.is_unique_violation() && e.constraint() == Some(constraint))
    }

    /// [`MetadataClient::lock_endpoint`] gave up waiting: another job held the endpoint past
    /// [`ENDPOINT_LOCK_TIMEOUT`], or no lock connection came free within it (the pool's
    /// `PoolTimedOut`). Either way the endpoint is busy, not broken.
    pub fn is_lock_wait_timeout(&self) -> bool {
        matches!(
            self,
            Self::EndpointLockTimeout(_) | Self::Database(sqlx::Error::PoolTimedOut)
        )
    }
}

/// How [`MetadataClient::begin_delete`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeginDelete {
    /// The database and its live branches are now `deleting`.
    Started,
    /// Already deleting or deleted, or missing: nothing written.
    NotReady,
    /// A live branch is protected: nothing written.
    Protected,
}

/// Postgres limits identifiers to 63 bytes.
const MAX_DATABASE_NAME_BYTES: usize = 63;
/// Databases every Postgres cluster already has.
const RESERVED_DATABASE_NAMES: [&str; 3] = ["postgres", "template0", "template1"];

/// The Postgres database name for a user-facing name: `[A-Za-z0-9_]` only
/// (other characters become `_`), at most 63 bytes, and not a system database.
pub fn postgres_database_name(name: &str) -> Result<String, MetadataError> {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    let cleaned = if cleaned.is_empty() { "chelabase".to_string() } else { cleaned };
    if cleaned.len() > MAX_DATABASE_NAME_BYTES {
        return Err(MetadataError::InvalidDatabaseName(format!(
            "must be at most {MAX_DATABASE_NAME_BYTES} characters"
        )));
    }
    if RESERVED_DATABASE_NAMES.contains(&cleaned.to_ascii_lowercase().as_str()) {
        return Err(MetadataError::InvalidDatabaseName(format!(
            "\"{cleaned}\" is reserved by Postgres"
        )));
    }
    Ok(cleaned)
}

/// Every `(table, column, also)` that holds a value sealed with the secrets key
/// (`chelabase_secrets::SecretKey`). A row counts as sealed when `column IS NOT NULL` and, if
/// `also` is set, that SQL condition holds too (a value that can never be opened again doesn't
/// count). [`MetadataClient::ensure_key_check`] adopts a new key only while no row counts; a
/// live test fails when a metadata column named like a sealed one is missing here.
///
/// Seal sites: `owner_password_enc` (worker provision, branch create, reset: `Purpose::Owner`);
/// `authenticator_password_enc` (worker data API, `Purpose::Authenticator`);
/// `storage_password_enc` (worker storage schema, `Purpose::Storage`); `data_api_keys.sealed`
/// (orchestrator public keys, `Purpose::DataKey` with AAD the key id; and a function's run-as
/// key, live only, sealed the same way by [`MetadataClient::set_run_as`]); `neon_roles.password_enc` (worker roles, branch
/// create, resync; orchestrator DDL: `seal_role_password`); `member_branch_roles.password_enc`
/// (worker member roles, `seal_member_role_password`); `branch_auth_sync.password_sealed`
/// (worker app users, `seal_role_password`); `app_sessions.grace_refresh_sealed` (orchestrator
/// app refresh, `seal`; opened only while `grace_until > now()`, see
/// [`MetadataClient::refresh_app_session`], and nothing clears it once the window is over);
/// `branch_function_secrets.ciphertext` (orchestrator function secrets and branch create,
/// `seal` with AAD `fn-secret:{branch}:{function}:{name}`).
pub const SECRETS_KEY_SEALED: &[(&str, &str, Option<&str>)] = &[
    ("branches", "owner_password_enc", None),
    ("branches", "authenticator_password_enc", None),
    ("branches", "storage_password_enc", None),
    ("data_api_keys", "sealed", None),
    ("neon_roles", "password_enc", None),
    ("member_branch_roles", "password_enc", None),
    ("branch_auth_sync", "password_sealed", None),
    ("app_sessions", "grace_refresh_sealed", Some("grace_until > now()")),
    ("branch_function_secrets", "ciphertext", None),
];

/// The one statement summing the sealed values in every column of [`SECRETS_KEY_SEALED`], as
/// [`MetadataClient::ensure_key_check`] counts them (tests count with it too). Built from the
/// constant list only.
pub fn sealed_count_sql() -> String {
    let counts: Vec<String> = SECRETS_KEY_SEALED
        .iter()
        .map(|(t, c, also)| {
            let also = also.map(|a| format!(" AND {a}")).unwrap_or_default();
            format!("(SELECT count(*) FROM {t} WHERE {c} IS NOT NULL{also})")
        })
        .collect();
    format!("SELECT {}", counts.join(" + "))
}

/// The key check's `LOCK TABLE … IN SHARE MODE` over every table in [`SECRETS_KEY_SEALED`]
/// and [`sealed_count_sql`].
fn sealed_check_sql() -> (String, String) {
    let mut tables: Vec<&str> = SECRETS_KEY_SEALED.iter().map(|(t, _, _)| *t).collect();
    tables.sort_unstable();
    tables.dedup();
    let lock = format!("LOCK TABLE {} IN SHARE MODE", tables.join(", "));
    (lock, sealed_count_sql())
}

/// Outcome of [`MetadataClient::ensure_key_check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCheck {
    /// First run: the check value was stored.
    Recorded,
    /// The stored check value opens with the current key.
    Matches,
    /// Different key, but nothing was encrypted yet: the new key was adopted.
    Replaced,
    /// Different key while encrypted credentials exist: they can't be read.
    Mismatch { sealed_rows: i64 },
}

/// A database that isn't deleted, as seen by the reconcile loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveDatabase {
    pub id: DatabaseId,
    pub status: DatabaseStatus,
    /// Main's endpoint.
    pub endpoint_id: Option<String>,
    /// The Neon tenant every branch lives in, once provisioning chose one.
    pub neon_tenant_id: Option<String>,
    /// The tenant's intended `pitr_interval`.
    pub history_window: std::time::Duration,
    /// Seconds since the row last changed.
    pub age_secs: i64,
}

/// A tenant with its projects and their databases (Studio landing page).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TenantOverview {
    pub id: TenantId,
    pub slug: String,
    /// The tenant's chosen name; readers fall back to the slug.
    pub display_name: Option<String>,
    pub projects: Vec<ProjectOverview>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectOverview {
    pub id: ProjectId,
    pub name: String,
    pub slug: String,
    pub status: ProjectStatus,
    pub databases: Vec<DatabaseRecord>,
}

/// Attach each database to its project, keeping project and database order.
/// Databases whose project isn't listed are dropped.
pub fn group_overview(
    id: TenantId,
    slug: String,
    display_name: Option<String>,
    projects: Vec<ProjectRecord>,
    databases: Vec<DatabaseRecord>,
) -> TenantOverview {
    let projects = projects
        .into_iter()
        .map(|project| ProjectOverview {
            id: project.id,
            name: project.name,
            slug: project.slug,
            status: project.status,
            databases: databases
                .iter()
                .filter(|d| d.project_id == project.id)
                .cloned()
                .collect(),
        })
        .collect();
    TenantOverview { id, slug, display_name, projects }
}

#[derive(Debug, Clone)]
pub struct MetadataClient {
    pool: PgPool,
    /// Only for [`MetadataClient::lock_endpoint`]: each held lock keeps a connection, and
    /// holders need `pool` for their own queries, so locks can never starve it.
    locks: PgPool,
    /// The control-plane secrets key, for function secrets (sealed and opened here, and sealed
    /// again for a child branch inside `create_branch`). `None` refuses those.
    secrets_key: Option<std::sync::Arc<chelabase_secrets::SecretKey>>,
}

/// Connections for endpoint locks, held and waited for: a waiter keeps one while Postgres
/// queues it, for up to [`ENDPOINT_LOCK_TIMEOUT`]. More lockers wait for a connection, for as
/// long again at most; the worker's reset deadline and reconcile's next pass bound the rest.
const LOCK_CONNECTIONS: u32 = 8;

impl MetadataClient {
    pub async fn connect(config: &MetadataConfig) -> Result<Self, MetadataError> {
        let pool = Self::pool_options(10)
            .connect(&config.database_url)
            .await?;
        Ok(Self {
            pool,
            locks: Self::lock_pool(config)?,
            secrets_key: None,
        })
    }

    /// A client that connects on first use (tests that never reach the database).
    pub fn connect_lazy(config: &MetadataConfig) -> Result<Self, MetadataError> {
        let pool = Self::pool_options(10)
            .connect_lazy(&config.database_url)?;
        Ok(Self {
            pool,
            locks: Self::lock_pool(config)?,
            secrets_key: None,
        })
    }

    /// Every connection pins `search_path=public`, so no role-named schema can shadow `public`.
    fn pool_options(max_connections: u32) -> PgPoolOptions {
        PgPoolOptions::new().max_connections(max_connections).after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::query("SET search_path = public").execute(&mut *conn).await?;
                Ok(())
            })
        })
    }

    /// Connects on first use: only the worker takes endpoint locks.
    fn lock_pool(config: &MetadataConfig) -> Result<PgPool, MetadataError> {
        Ok(Self::pool_options(LOCK_CONNECTIONS)
            .acquire_timeout(ENDPOINT_LOCK_TIMEOUT)
            .connect_lazy(&config.database_url)?)
    }

    pub async fn migrate(&self) -> Result<(), MetadataError> {
        // Embedded at build time: a new migration file alone doesn't rebuild this, so touch
        // this file when adding one (last: 20261107000004).
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        info!(
            demo_tenant = %DEMO_TENANT_ID,
            demo_project = %DEMO_PROJECT_ID,
            "metadata migrations applied"
        );
        Ok(())
    }

    /// This client with the secrets key, which function secrets (and copying them to a new
    /// branch) need.
    pub fn with_secrets_key(mut self, key: chelabase_secrets::SecretKey) -> Self {
        self.secrets_key = Some(std::sync::Arc::new(key));
        self
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn ensure_project_in_tenant(
        &self,
        tenant_id: TenantId,
        project_id: ProjectId,
    ) -> Result<(), MetadataError> {
        let tenant_exists: Option<(Uuid,)> =
            sqlx::query_as("SELECT id FROM tenants WHERE id = $1")
                .bind(tenant_id.as_uuid())
                .fetch_optional(&self.pool)
                .await?;
        if tenant_exists.is_none() {
            return Err(MetadataError::TenantNotFound(tenant_id));
        }

        let project: Option<(Uuid, Uuid)> =
            sqlx::query_as("SELECT id, tenant_id FROM projects WHERE id = $1")
                .bind(project_id.as_uuid())
                .fetch_optional(&self.pool)
                .await?;

        let Some((pid, project_tenant)) = project else {
            return Err(MetadataError::ProjectNotFound(project_id));
        };
        if pid != project_id.as_uuid() || project_tenant != tenant_id.as_uuid() {
            return Err(MetadataError::ProjectTenantMismatch {
                project_id,
                tenant_id,
            });
        }
        Ok(())
    }

    pub async fn insert_database_pending(
        &self,
        database_id: DatabaseId,
        tenant_id: TenantId,
        project_id: ProjectId,
        name: &str,
    ) -> Result<(), MetadataError> {
        self.ensure_project_in_tenant(tenant_id, project_id).await?;

        // The database and its main branch (same id, no parent), together.
        let mut tx = self.pool.begin().await?;
        // Lock order: the project row first. A project that isn't `active` takes no databases.
        let project_status: Option<String> =
            sqlx::query_scalar("SELECT status FROM projects WHERE id = $1 FOR SHARE")
                .bind(project_id.as_uuid())
                .fetch_optional(&mut *tx)
                .await?;
        match project_status.as_deref() {
            None => return Err(MetadataError::ProjectNotFound(project_id)),
            Some("active") => {}
            Some(_) => return Err(MetadataError::ProjectDeleting(project_id)),
        }
        let result = sqlx::query(
            r#"
            INSERT INTO databases (id, project_id, name, status)
            VALUES ($1, $2, $3, $4)
            "#,
        )
        .bind(database_id.as_uuid())
        .bind(project_id.as_uuid())
        .bind(name)
        .bind(DatabaseStatus::Pending.as_str())
        .execute(&mut *tx)
        .await;

        match result {
            Ok(_) => {}
            Err(sqlx::Error::Database(err))
                if err.constraint() == Some("databases_live_name_key") =>
            {
                return Err(MetadataError::DuplicateDatabaseName);
            }
            Err(err) => return Err(err.into()),
        }
        sqlx::query(
            "INSERT INTO branches (id, database_id, name, parent_branch_id, kind, status)
             VALUES ($1, $1, 'main', NULL, 'normal', 'pending')",
        )
        .bind(database_id.as_uuid())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Update a live database's status, and its main branch's with it (ruling 4). False
    /// (nothing written) once a delete has begun: provisioning must never overwrite
    /// `deleting`/`deleted`.
    pub async fn set_status(
        &self,
        database_id: DatabaseId,
        status: DatabaseStatus,
    ) -> Result<bool, MetadataError> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            r#"
            UPDATE databases
            SET status = $2, error = NULL, updated_at = now()
            WHERE id = $1 AND status NOT IN ('deleting', 'deleted')
            "#,
        )
        .bind(database_id.as_uuid())
        .bind(status.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated != 1 {
            return Ok(false);
        }
        mirror_on_main(&mut tx, database_id, BranchStatus::from(status), None).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Move a database and every live branch of it to `deleting`, in one transaction.
    /// `NotReady` (nothing written) when it's already deleting, deleted, or missing;
    /// `Protected` (nothing written) while any of its live branches is protected: the check
    /// locks every live branch row (waiting for a protect in flight and reading its result), and
    /// a protect that lands later waits for the new `deleting` status.
    pub async fn begin_delete(&self, database_id: DatabaseId) -> Result<BeginDelete, MetadataError> {
        let mut tx = self.pool.begin().await?;
        let moved = sqlx::query(
            r#"
            UPDATE databases
            SET status = 'deleting', updated_at = now()
            WHERE id = $1 AND status NOT IN ('deleting', 'deleted')
            "#,
        )
        .bind(database_id.as_uuid())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if moved != 1 {
            return Ok(BeginDelete::NotReady);
        }
        let flags: Vec<bool> = sqlx::query_scalar(
            "SELECT protected FROM branches
             WHERE database_id = $1 AND status NOT IN ('deleting', 'deleted')
             FOR NO KEY UPDATE",
        )
        .bind(database_id.as_uuid())
        .fetch_all(&mut *tx)
        .await?;
        if flags.contains(&true) {
            return Ok(BeginDelete::Protected);
        }
        sqlx::query(
            "UPDATE branches SET status = 'deleting', updated_at = now()
             WHERE database_id = $1 AND status NOT IN ('deleting', 'deleted')",
        )
        .bind(database_id.as_uuid())
        .execute(&mut *tx)
        .await?;
        // EOS-6, T3-M3: no personal login outlives its database (MH-9).
        personal_logins::revoke_database_logins(&mut tx, &[database_id.as_uuid()]).await?;
        tx.commit().await?;
        Ok(BeginDelete::Started)
    }

    /// Turn the database row into a tombstone, revoking any personal login still live on it
    /// (MH-9) and purging every branch's function state. Its branches are marked deleted one
    /// by one ([`MetadataClient::mark_branch_deleted`]), each once its resources are gone.
    pub async fn mark_deleted(&self, database_id: DatabaseId) -> Result<(), MetadataError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"
            UPDATE databases
            SET status = 'deleted', deleted_at = now(), updated_at = now()
            WHERE id = $1
            "#,
        )
        .bind(database_id.as_uuid())
        .execute(&mut *tx)
        .await?;
        personal_logins::revoke_database_logins(&mut tx, &[database_id.as_uuid()]).await?;
        // Every branch's function state goes with the database (its branch rows stay).
        let branches: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM branches WHERE database_id = $1")
                .bind(database_id.as_uuid())
                .fetch_all(&mut *tx)
                .await?;
        functions::purge_branch_functions(&mut tx, &branches).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Fail a database stuck provisioning, but only if it is *still* pending or
    /// provisioning and untouched for `max_age_secs` at write time. Reconcile
    /// acts on a snapshot; this re-check keeps it from failing a database the
    /// provision consumer just picked up. True when it was failed; main fails with it.
    pub async fn fail_if_stale(
        &self,
        database_id: DatabaseId,
        max_age_secs: i64,
    ) -> Result<bool, MetadataError> {
        const TIMED_OUT: &str = "provisioning timed out";
        let mut tx = self.pool.begin().await?;
        let failed = sqlx::query(
            r#"
            UPDATE databases
            SET status = 'failed', error = $3, updated_at = now()
            WHERE id = $1
              AND status IN ('pending', 'provisioning')
              AND updated_at < now() - make_interval(secs => $2)
            "#,
        )
        .bind(database_id.as_uuid())
        .bind(max_age_secs as f64)
        .bind(TIMED_OUT)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if failed != 1 {
            return Ok(false);
        }
        mirror_on_main(&mut tx, database_id, BranchStatus::Failed, Some(TIMED_OUT)).await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Compare the current secrets key with the one existing credentials were
    /// sealed with. `candidate` is a fresh check value for the current key and
    /// `opens` tells whether a stored check value opens with it.
    ///
    /// A key that doesn't open the stored check is adopted only while no column in
    /// [`SECRETS_KEY_SEALED`] holds a value. The count and the replace run in one transaction
    /// holding `SHARE` locks on those tables, so no sealed row can be written in between.
    /// Those locks can deadlock with a writer that holds one listed table and wants another:
    /// Postgres aborts one side (40P01), and if it is this check the start fails closed and is
    /// retried on restart. `lock_timeout` bounds the wait behind a long transaction.
    pub async fn ensure_key_check(
        &self,
        candidate: &str,
        opens: impl Fn(&str) -> bool,
    ) -> Result<KeyCheck, MetadataError> {
        const KEY: &str = "secrets_key_check";
        let inserted = sqlx::query(
            "INSERT INTO control_settings (key, value) VALUES ($1, $2) ON CONFLICT (key) DO NOTHING",
        )
        .bind(KEY)
        .bind(candidate)
        .execute(&self.pool)
        .await?;
        if inserted.rows_affected() == 1 {
            return Ok(KeyCheck::Recorded);
        }
        let stored: String = sqlx::query_scalar("SELECT value FROM control_settings WHERE key = $1")
            .bind(KEY)
            .fetch_one(&self.pool)
            .await?;
        if opens(&stored) {
            return Ok(KeyCheck::Matches);
        }
        let mut tx = self.pool.begin().await?;
        // A start racing this one may have adopted the same key meanwhile.
        let stored: String =
            sqlx::query_scalar("SELECT value FROM control_settings WHERE key = $1 FOR UPDATE")
                .bind(KEY)
                .fetch_one(&mut *tx)
                .await?;
        if opens(&stored) {
            tx.commit().await?;
            return Ok(KeyCheck::Matches);
        }
        // SHARE waits for in-flight writers to commit and holds new ones off until the replace
        // commits. A long transaction fails the start instead of hanging it.
        sqlx::query("SET LOCAL lock_timeout = '30s'").execute(&mut *tx).await?;
        let (lock, count) = sealed_check_sql();
        sqlx::query(sqlx::AssertSqlSafe(lock)).execute(&mut *tx).await?;
        let sealed_rows: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(count))
            .fetch_one(&mut *tx)
            .await?;
        if sealed_rows > 0 {
            tx.rollback().await?;
            return Ok(KeyCheck::Mismatch { sealed_rows });
        }
        sqlx::query("UPDATE control_settings SET value = $2, updated_at = now() WHERE key = $1")
            .bind(KEY)
            .bind(candidate)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(KeyCheck::Replaced)
    }

    /// Every database that isn't deleted, with its main branch's endpoint and the age of its
    /// last change.
    pub async fn list_live_for_reconcile(&self) -> Result<Vec<LiveDatabase>, MetadataError> {
        let rows = sqlx::query(
            r#"
            SELECT d.id, d.status, b.endpoint_id, d.neon_tenant_id,
                   EXTRACT(EPOCH FROM d.history_window)::BIGINT AS history_window_secs,
                   EXTRACT(EPOCH FROM now() - d.updated_at)::BIGINT AS age_secs
            FROM databases d
            LEFT JOIN branches b ON b.id = d.id
            WHERE d.status <> 'deleted'
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(LiveDatabase {
                    id: DatabaseId(r.get("id")),
                    status: DatabaseStatus::parse(r.get("status"))?,
                    endpoint_id: r.get("endpoint_id"),
                    neon_tenant_id: r.get("neon_tenant_id"),
                    history_window: std::time::Duration::from_secs(
                        r.get::<i64, _>("history_window_secs").max(0) as u64,
                    ),
                    age_secs: r.get("age_secs"),
                })
            })
            .collect()
    }

    /// Record a failure on the database and its main branch (ignored once a delete has begun).
    pub async fn set_failed(
        &self,
        database_id: DatabaseId,
        error: &str,
    ) -> Result<(), MetadataError> {
        let mut tx = self.pool.begin().await?;
        let failed = sqlx::query(
            r#"
            UPDATE databases
            SET status = $2, error = $3, updated_at = now()
            WHERE id = $1 AND status NOT IN ('deleting', 'deleted')
            "#,
        )
        .bind(database_id.as_uuid())
        .bind(DatabaseStatus::Failed.as_str())
        .bind(error)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if failed == 1 {
            mirror_on_main(&mut tx, database_id, BranchStatus::Failed, Some(error)).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Whether the tenant exists.
    pub async fn tenant_exists(&self, tenant: TenantId) -> Result<bool, MetadataError> {
        Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM tenants WHERE id = $1)")
            .bind(tenant.as_uuid())
            .fetch_one(&self.pool)
            .await?)
    }

    pub async fn get_database(
        &self,
        database_id: DatabaseId,
    ) -> Result<Option<DatabaseRecord>, MetadataError> {
        let row = sqlx::query(
            r#"
            SELECT
                d.id,
                d.project_id,
                p.tenant_id,
                d.name,
                d.status,
                d.database_name,
                d.owner_role,
                d.error,
                d.neon_tenant_id,
                EXTRACT(EPOCH FROM d.history_window)::BIGINT AS history_window_secs,
                d.max_branches, d.max_rows, d.statement_timeout_ms, d.max_connections,
                d.storage_max_connections
            FROM databases d
            JOIN projects p ON p.id = d.project_id
            WHERE d.id = $1
            "#,
        )
        .bind(database_id.as_uuid())
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.as_ref().map(database_from_row))
    }

    /// The tenant, its projects (by slug) and their databases (oldest first).
    pub async fn tenant_overview(
        &self,
        tenant_id: TenantId,
    ) -> Result<Option<TenantOverview>, MetadataError> {
        let Some(tenant) = sqlx::query("SELECT slug, display_name FROM tenants WHERE id = $1")
            .bind(tenant_id.as_uuid())
            .fetch_optional(&self.pool)
            .await?
        else {
            return Ok(None);
        };
        let projects = sqlx::query(
            "SELECT id, tenant_id, name, slug, status, error, created_at, updated_at
             FROM projects WHERE tenant_id = $1 AND status <> 'deleted' ORDER BY slug",
        )
        .bind(tenant_id.as_uuid())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(projects::project_from_row)
        .collect::<Result<Vec<_>, _>>()?;
        let databases = sqlx::query(
            r#"
            SELECT d.id, d.project_id, p.tenant_id, d.name, d.status, d.database_name,
                   d.owner_role, d.error, d.neon_tenant_id,
                   EXTRACT(EPOCH FROM d.history_window)::BIGINT AS history_window_secs,
                   d.max_branches, d.max_rows, d.statement_timeout_ms, d.max_connections,
                d.storage_max_connections
            FROM databases d
            JOIN projects p ON p.id = d.project_id
            WHERE p.tenant_id = $1 AND d.status <> 'deleted'
            ORDER BY d.created_at
            "#,
        )
        .bind(tenant_id.as_uuid())
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(database_from_row)
        .collect();
        Ok(Some(group_overview(
            tenant_id,
            tenant.get("slug"),
            tenant.get("display_name"),
            projects,
            databases,
        )))
    }
}

/// Main follows its database while provisioning (ruling 4): the same status and error, but
/// only while main is itself pending, provisioning or failed. A main that moved on (ready,
/// resetting, deleting, deleted) is never overwritten by a database write.
///
/// Lock order (see `branches.rs`): callers have already updated the `databases` row, so it is
/// locked before main's branch row, as in every transaction touching both tables.
async fn mirror_on_main(
    tx: &mut sqlx::Transaction<'static, sqlx::Postgres>,
    database_id: DatabaseId,
    status: BranchStatus,
    error: Option<&str>,
) -> Result<(), MetadataError> {
    sqlx::query(
        "UPDATE branches SET status = $2, error = $3, updated_at = now()
         WHERE id = $1 AND status IN ('pending', 'provisioning', 'failed')",
    )
    .bind(database_id.as_uuid())
    .bind(status.as_str())
    .bind(error)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Map a `databases ⋈ projects` row (see `get_database`) to a record.
fn database_from_row(r: &sqlx::postgres::PgRow) -> DatabaseRecord {
    DatabaseRecord {
        database_id: DatabaseId(r.get("id")),
        project_id: ProjectId(r.get("project_id")),
        tenant_id: TenantId(r.get("tenant_id")),
        name: r.get("name"),
        status: DatabaseStatus::parse(r.get("status")).unwrap_or(DatabaseStatus::Failed),
        database_name: r.get("database_name"),
        owner_role: r.get("owner_role"),
        error: r.get("error"),
        neon_tenant_id: r.get("neon_tenant_id"),
        history_window: std::time::Duration::from_secs(
            r.get::<i64, _>("history_window_secs").max(0) as u64,
        ),
        max_branches: r.get("max_branches"),
        max_rows: r.get("max_rows"),
        statement_timeout_ms: r.get("statement_timeout_ms"),
        max_connections: r.get("max_connections"),
        storage_max_connections: r.get("storage_max_connections"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // #38 review: the key check counts every listed column, with its condition, and locks
    // every listed table first.
    #[test]
    fn sealed_check_sql_counts_every_listed_column() {
        let (lock, count) = sealed_check_sql();
        let terms: Vec<&str> = count
            .strip_prefix("SELECT ")
            .expect("one SELECT")
            .split(" + ")
            .collect();
        let expected: Vec<String> = SECRETS_KEY_SEALED
            .iter()
            .map(|(t, c, also)| match also {
                Some(a) => format!("(SELECT count(*) FROM {t} WHERE {c} IS NOT NULL AND {a})"),
                None => format!("(SELECT count(*) FROM {t} WHERE {c} IS NOT NULL)"),
            })
            .collect();
        assert_eq!(terms, expected);
        assert!(count.contains("grace_refresh_sealed IS NOT NULL AND grace_until > now()"));
        let tables = lock
            .strip_prefix("LOCK TABLE ")
            .and_then(|l| l.strip_suffix(" IN SHARE MODE"))
            .expect("one LOCK TABLE … IN SHARE MODE");
        let tables: Vec<&str> = tables.split(", ").collect();
        for (t, _, _) in SECRETS_KEY_SEALED {
            assert_eq!(tables.iter().filter(|x| *x == t).count(), 1, "{t} locked once");
        }
        assert_eq!(count, sealed_count_sql());
    }

    // m5 re-audit S2: `Debug` hides a password in the user info and in a `password` query
    // parameter (any position, a percent-encoded key too), and keeps everything else.
    #[test]
    fn debug_hides_every_url_password() {
        for (url, shown) in [
            ("postgres://u:in-url-pw@db:5432/m", "postgres://u:<redacted>@db:5432/m"),
            ("postgres://u@db:5432/m?password=in-url-pw", "postgres://u@db:5432/m?password=<redacted>"),
            (
                "postgres://u@db/m?sslmode=disable&password=in-url-pw&application_name=x",
                "postgres://u@db/m?sslmode=disable&password=<redacted>&application_name=x",
            ),
            ("postgres://u@db/m?%70assword=in-url-pw#f", "postgres://u@db/m?%70assword=<redacted>#f"),
            (
                "postgres://u:a@db/m?PASSWORD=in-url-pw",
                "postgres://u:<redacted>@db/m?PASSWORD=<redacted>",
            ),
            ("postgres://u@db/m?passwordless=1", "postgres://u@db/m?passwordless=1"),
        ] {
            let c = MetadataConfig { database_url: url.into() };
            let got = format!("{c:?}");
            assert_eq!(got, format!("MetadataConfig {{ database_url: {shown:?} }}"), "{url}");
            assert!(!got.contains("in-url-pw"), "{got}");
        }
    }

    fn record(project_id: ProjectId, name: &str) -> DatabaseRecord {
        DatabaseRecord {
            database_id: DatabaseId::new(),
            project_id,
            tenant_id: TenantId(DEMO_TENANT_ID),
            name: name.into(),
            status: DatabaseStatus::Ready,
            database_name: None,
            owner_role: None,
            error: None,
            neon_tenant_id: None,
            history_window: std::time::Duration::from_secs(7 * 24 * 3600),
            max_branches: 5,
            max_rows: 1000,
            statement_timeout_ms: 8000,
            max_connections: 5,
            storage_max_connections: 2,
        }
    }

    fn project(id: ProjectId, slug: &str) -> ProjectRecord {
        ProjectRecord {
            id,
            tenant_id: TenantId(DEMO_TENANT_ID),
            name: slug.to_uppercase(),
            slug: slug.into(),
            status: ProjectStatus::Active,
            error: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn database_names_are_sanitized_like_before() {
        assert_eq!(postgres_database_name("web-1 prod").unwrap(), "web_1_prod");
        assert_eq!(postgres_database_name("").unwrap(), "chelabase");
    }

    #[test]
    fn reserved_database_names_are_rejected() {
        for reserved in ["postgres", "template0", "template1", "POSTGRES", "Template1"] {
            assert!(matches!(
                postgres_database_name(reserved),
                Err(MetadataError::InvalidDatabaseName(_))
            ), "{reserved}");
        }
    }

    #[test]
    fn database_names_longer_than_63_bytes_are_rejected() {
        assert!(postgres_database_name(&"a".repeat(63)).is_ok());
        assert!(matches!(
            postgres_database_name(&"a".repeat(64)),
            Err(MetadataError::InvalidDatabaseName(_))
        ));
    }

    #[test]
    fn deleted_status_round_trips() {
        assert_eq!(DatabaseStatus::Deleted.as_str(), "deleted");
        assert_eq!(DatabaseStatus::parse("deleted").unwrap(), DatabaseStatus::Deleted);
    }

    #[test]
    fn group_overview_attaches_databases_to_their_projects() {
        let (a, b) = (ProjectId::new(), ProjectId::new());
        let overview = group_overview(
            TenantId(DEMO_TENANT_ID),
            "demo".into(),
            None,
            vec![project(a, "alpha"), project(b, "beta")],
            vec![record(b, "b1"), record(a, "a1"), record(b, "b2")],
        );
        assert_eq!(overview.slug, "demo");
        assert_eq!(overview.display_name, None);
        let names = |i: usize| -> Vec<String> {
            overview.projects[i]
                .databases
                .iter()
                .map(|d| d.name.clone())
                .collect()
        };
        assert_eq!(overview.projects[0].slug, "alpha");
        assert_eq!(overview.projects[0].name, "ALPHA");
        assert_eq!(overview.projects[0].status, ProjectStatus::Active);
        assert_eq!(names(0), vec!["a1"]);
        assert_eq!(names(1), vec!["b1", "b2"]);
    }

    #[test]
    fn group_overview_keeps_projects_without_databases() {
        let a = ProjectId::new();
        let overview = group_overview(
            TenantId(DEMO_TENANT_ID),
            "demo".into(),
            None,
            vec![project(a, "alpha")],
            vec![],
        );
        assert_eq!(overview.projects.len(), 1);
        assert!(overview.projects[0].databases.is_empty());
    }

    #[test]
    fn group_overview_drops_databases_of_unlisted_projects() {
        let a = ProjectId::new();
        let overview = group_overview(
            TenantId(DEMO_TENANT_ID),
            "demo".into(),
            None,
            vec![project(a, "alpha")],
            vec![record(ProjectId::new(), "stray")],
        );
        assert!(overview.projects[0].databases.is_empty());
    }
}
