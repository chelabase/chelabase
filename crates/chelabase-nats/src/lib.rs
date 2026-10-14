//! NATS JetStream subjects, envelopes, and helpers for Chelabase control plane.

use chelabase_types::{BranchId, DatabaseId, ProjectId, TenantId};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};
use uuid::Uuid;

/// Subject for database provisioning jobs.
pub const SUBJECT_PROVISION: &str = "chelabase.provision";

/// Subject for reconciliation jobs (reserved).
pub const SUBJECT_RECONCILE: &str = "chelabase.reconcile";

/// JetStream stream name for control-plane jobs.
pub const STREAM_CONTROL: &str = "CHELABASE_CONTROL";

/// Durable consumer name for the provision worker.
pub const CONSUMER_PROVISION: &str = "chelabase-worker-provision";

/// How long a failed job waits before JetStream redelivers it.
pub const REDELIVERY_DELAY: std::time::Duration = std::time::Duration::from_secs(10);

/// Durable delete jobs (container + volume teardown).
pub const SUBJECT_DELETE: &str = "chelabase.delete";
pub const CONSUMER_DELETE: &str = "chelabase-worker-delete";

/// How long a failed branch job waits before JetStream redelivers it: longer than other jobs,
/// so that its few transient retries (the worker's `BRANCH_JOB_ATTEMPTS`) span a
/// storage-controller restart, about 2 minutes.
pub const BRANCH_REDELIVERY_DELAY: std::time::Duration = std::time::Duration::from_secs(30);

/// Durable branch jobs: create, reset to parent, restore and delete.
pub const SUBJECT_BRANCH: &str = "chelabase.branch";
pub const CONSUMER_BRANCH: &str = "chelabase-worker-branch";

/// Durable schema-merge jobs: compute, rehearse, apply, resume and cancel.
pub const SUBJECT_MERGE: &str = "chelabase.merge";
pub const CONSUMER_MERGE: &str = "chelabase-worker-merge";

/// Durable database settings jobs: push the history window to the tenant.
pub const SUBJECT_DATABASE_SETTINGS: &str = "chelabase.database_settings";
pub const CONSUMER_DATABASE_SETTINGS: &str = "chelabase-worker-settings";

/// Durable project delete jobs.
pub const SUBJECT_PROJECT_DELETE: &str = "chelabase.project_delete";
pub const CONSUMER_PROJECT_DELETE: &str = "chelabase-worker-project-delete";

/// Durable app-users jobs: enable, disable and reconcile a project's user location.
pub const SUBJECT_APP_USERS: &str = "chelabase.app_users";
pub const CONSUMER_APP_USERS: &str = "chelabase-worker-app-users";

/// Durable app-users sync jobs: copy one user row into the project's databases.
pub const SUBJECT_APP_USERS_SYNC: &str = "chelabase.app_users_sync";
pub const CONSUMER_APP_USERS_SYNC: &str = "chelabase-worker-app-users-sync";

/// Core request/reply (not JetStream): check a table can hold the app-users location.
pub const SUBJECT_APP_USERS_CHECK: &str = "chelabase.rpc.app_users.check";

/// Core request/reply (not JetStream): reset a database's owner password.
pub const SUBJECT_RESET_PASSWORD: &str = "chelabase.rpc.reset_password";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobEnvelope<T> {
    pub job_id: Uuid,
    pub payload: T,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvisionDatabaseJob {
    pub database_id: DatabaseId,
    pub project_id: ProjectId,
    pub tenant_id: TenantId,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDeleteJob {
    pub project_id: ProjectId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppUsersJob {
    Enable { project_id: ProjectId },
    Disable { project_id: ProjectId },
    Reconcile { project_id: ProjectId },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppUsersSyncJob {
    pub project_id: ProjectId,
    pub user_id: Uuid,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
    #[serde(default)]
    pub origin_branch_id: Option<BranchId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppUsersCheckRequest {
    pub database_id: DatabaseId,
    pub pg_database: String,
    pub schema: String,
    pub table: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppUsersCheckReply {
    pub ok: bool,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteDatabaseJob {
    pub database_id: DatabaseId,
}

/// A point on a branch's history: what the user asked for, before the worker resolves it to
/// an LSN.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointInTime {
    Timestamp(DateTime<Utc>),
    Lsn(String),
}

/// A schema-merge job; the merge row says what to do next.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum MergeJob {
    /// Read the three models and compute the analysis and the plan.
    Compute { merge_id: Uuid },
    /// Run the plan on a throwaway branch of the target.
    Rehearse { merge_id: Uuid },
    /// Apply the plan to the target.
    Apply { merge_id: Uuid },
    /// Continue an interrupted apply (phase 2 and finish).
    Resume { merge_id: Uuid },
    /// Stop a running merge and clean up.
    Cancel { merge_id: Uuid },
    /// Run the after-merge choice by hand on a merge applied with drift (E21).
    Finish { merge_id: Uuid, after: MergeAfter },
}

/// What happens to the source branch after a merge (D11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeAfter {
    /// Kept; the merge becomes its base.
    Keep,
    /// Reset to its parent.
    Reset,
    /// Deleted.
    Delete,
}

impl MergeAfter {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Keep => "keep",
            Self::Reset => "reset",
            Self::Delete => "delete",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchJob {
    /// The branch row exists (pending); build its timeline and compute. `at` is the point on
    /// the parent, `None` for now.
    Create {
        branch_id: BranchId,
        at: Option<PointInTime>,
    },
    /// The branch is `resetting`.
    ResetToParent { branch_id: BranchId },
    /// The branch is `resetting`; the old timeline becomes the backup `backup_id` named
    /// `backup_name`.
    Restore {
        branch_id: BranchId,
        backup_id: BranchId,
        backup_name: String,
        at: PointInTime,
    },
    /// The branch (and its backups) are `deleting`.
    Delete { branch_id: BranchId },
}

/// Push the database's settings (the history window) to its Neon tenant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseSettingsJob {
    pub database_id: DatabaseId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "ResetPasswordRequestWire")]
pub struct ResetPasswordRequest {
    pub database_id: DatabaseId,
    /// Main's id (the database's) for the old routes; a request without one means main.
    pub branch_id: BranchId,
}

/// What a request may look like on the wire: `branch_id` is absent in step 4's requests.
#[derive(Deserialize)]
struct ResetPasswordRequestWire {
    database_id: DatabaseId,
    #[serde(default)]
    branch_id: Option<BranchId>,
}

impl From<ResetPasswordRequestWire> for ResetPasswordRequest {
    fn from(wire: ResetPasswordRequestWire) -> Self {
        Self {
            database_id: wire.database_id,
            branch_id: wire.branch_id.unwrap_or(BranchId::from(wire.database_id)),
        }
    }
}

/// The worker's reply error when the new password is stored but the compute doesn't have it yet.
pub const RESET_NOT_APPLIED: &str = "the password was changed but not yet applied; it will be retried";

/// A role create's `not_applied`: the role is stored, but the compute doesn't have it yet.
pub const ROLE_CREATE_NOT_APPLIED: &str = "the role was stored but not yet applied; it will be retried";

/// A role delete's `not_applied`: the role is gone, but the compute's spec doesn't say so yet.
pub const ROLE_DELETE_NOT_APPLIED: &str =
    "the role was deleted but the change is not yet applied; it will be retried";

/// A role update's `not_applied`: the change is stored, but the compute doesn't have it yet.
pub const ROLE_UPDATE_NOT_APPLIED: &str =
    "the role's change was stored but not yet applied; it will be retried";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetPasswordReply {
    pub ok: bool,
    pub error: Option<String>,
}

/// Core request/reply (not JetStream): may a secret key use this role on this branch?
pub const SUBJECT_CHECK_DATA_ROLE: &str = "chelabase.rpc.check_data_role";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckDataRoleRequest {
    pub branch_id: BranchId,
    pub role: String,
}

/// Core publish (no reply awaited): bootstrap the data API in a database the orchestrator just
/// marked `enabling`. The worker records `on` or `error`; reconcile retries rows left `enabling`.
pub const SUBJECT_DATA_API_ENABLE: &str = "chelabase.rpc.data_api.enable";
/// Core publish (no reply awaited): roles of the branch were touched in SQL (attributes or
/// grants the DDL receiver can't mirror). The worker waits a few seconds, then runs the
/// branch's resync once however many arrived.
pub const SUBJECT_RESYNC_BRANCH: &str = "chelabase.resync.branch";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResyncBranchRequest {
    pub branch_id: BranchId,
}

/// Core request/reply: make a branch's hidden member roles (`cb_m_…`) match the metadata now
/// (members-and-roles design, part 3). Answers a [`MemberRolesReply`].
pub const SUBJECT_MEMBER_ROLES_RECONCILE: &str = "chelabase.rpc.member_roles.reconcile";
/// Core publish (no reply awaited): a branch's member roles need a reconcile soon. The worker
/// waits a moment and runs it once however many arrived, like the touched-roles resync.
pub const SUBJECT_MEMBER_ROLES_ENQUEUE: &str = "chelabase.member_roles.reconcile";

/// Core request/reply: after an act-as run, the member role's hand-over in the run's database
/// (`chelabase_types::role_name::member_hand_over_sql`). Answers a [`MemberRolesReply`]
/// (`Done` with no roles, or `Failed`).
pub const SUBJECT_MEMBER_ROLES_HAND_OVER: &str = "chelabase.rpc.member_roles.hand_over";

/// One act-as run's hand-over: the member's role on `branch_id`, the Postgres database it ran
/// in, the role it acted as and the run's backend (ended first when it's still there).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberRoleHandOverRequest {
    pub branch_id: Uuid,
    pub member_id: Uuid,
    pub database: String,
    pub acted_role: String,
    #[serde(default)]
    pub backend_pid: Option<i32>,
    /// The backend's `pg_stat_activity.backend_start`, as text: with the pid, it names this
    /// run's backend only (a pid can be reused).
    #[serde(default)]
    pub backend_start: Option<String>,
}

/// A member-role reconcile of one branch; with `member_id`, only that member's roles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberRolesReconcileRequest {
    pub branch_id: Uuid,
    #[serde(default)]
    pub member_id: Option<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemberRolesReply {
    /// The reconciled roles as they now are on the branch.
    Done { roles: Vec<MemberRoleApplied> },
    Failed { message: String },
}

/// One member role after a reconcile: the Postgres roles it may `SET ROLE` to, sorted. One per
/// (member, branch) (R17).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberRoleApplied {
    pub member_id: Uuid,
    pub role_name: String,
    pub applied_roles: Vec<String>,
}

/// Core request/reply: a branch's protection review (code owned by non-Admin roles that can run
/// in someone else's session), scanned by the worker as `cloud_admin` in every live database
/// of the branch; with `fix`, one item fixed after the hash check. Answers a [`ReviewReply`].
pub const SUBJECT_REVIEW: &str = "chelabase.rpc.review";

/// A review scan of `branch_id`, or (with `fix`) one fix.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRequest {
    pub branch_id: Uuid,
    #[serde(default)]
    pub fix: Option<ReviewFixOrder>,
}

/// One fix: the item's key, what to do, and the hash of the review the caller saw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewFixOrder {
    pub key: String,
    pub action: chelabase_types::protection::FixAction,
    pub hash: String,
}

/// The review's answer. `items` are the scan's (after the fix for `Fixed`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ReviewReply {
    Scanned {
        items: Vec<chelabase_types::protection::ReviewItem>,
    },
    Fixed {
        sql: String,
        items: Vec<chelabase_types::protection::ReviewItem>,
    },
    /// The fix's hash isn't the current review's; nothing ran.
    Changed {
        items: Vec<chelabase_types::protection::ReviewItem>,
    },
    /// The review has no item of the fix's key; nothing ran.
    NotFound {
        items: Vec<chelabase_types::protection::ReviewItem>,
    },
    /// Postgres refused the fix (`XX000` when the SQLSTATE couldn't be read).
    FixFailed {
        sqlstate: String,
        message: String,
        items: Vec<chelabase_types::protection::ReviewItem>,
    },
    /// The scan (or the branch, or the compute) failed: no partial list.
    Failed { message: String },
}

/// Core request/reply: stop serving a database (`remove` also drops the `chelabase` machinery).
/// Answers a [`CheckDataRoleReply`].
pub const SUBJECT_DATA_API_DISABLE: &str = "chelabase.rpc.data_api.disable";
/// Core request/reply: re-run the bootstrap of a served database after its `default_schema`
/// changed. Answers a [`CheckDataRoleReply`].
pub const SUBJECT_DATA_API_REBOOTSTRAP: &str = "chelabase.rpc.data_api.rebootstrap";

/// Enable, disable and rebootstrap. `remove` only matters for disable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DataApiDatabaseRequest {
    pub branch_id: BranchId,
    pub name: String,
    #[serde(default)]
    pub remove: bool,
    /// Rebootstrap only: also grant the data API roles on the schema's existing objects,
    /// whether or not a first grant ran before. Never revokes.
    #[serde(default)]
    pub grant_existing: bool,
}

/// Core request/reply (not JetStream): create a login role. The orchestrator waits up to
/// `RESET_TIMEOUT` for the worker's [`RoleReply`].
pub const SUBJECT_ROLE_CREATE: &str = "chelabase.rpc.roles.create";
/// Core request/reply: reset a login role's password.
pub const SUBJECT_ROLE_RESET: &str = "chelabase.rpc.roles.reset";
/// Core request/reply: delete a login role.
pub const SUBJECT_ROLE_DELETE: &str = "chelabase.rpc.roles.delete";
/// Core request/reply: change a role's attributes or memberships ([`RoleUpdateRequest`]).
pub const SUBJECT_ROLE_UPDATE: &str = "chelabase.rpc.roles.update";

fn yes() -> bool {
    true
}

fn no_connection_limit() -> i32 {
    -1
}

/// A field that tells "absent" (`None`) from `null` (`Some(None)`).
fn double_option<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// A role to create (U1, U2). Every field past `owner_level` is optional on the wire, with a
/// plain LOGIN role's defaults, so older senders still parse.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleCreateRequest {
    pub branch_id: BranchId,
    pub name: String,
    pub owner_level: bool,
    /// A NOLOGIN role has no password.
    #[serde(default = "yes")]
    pub login: bool,
    #[serde(default = "yes")]
    pub inherit: bool,
    /// `-1`: no limit.
    #[serde(default = "no_connection_limit")]
    pub connection_limit: i32,
    #[serde(default)]
    pub valid_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub bypass_rls: bool,
    /// Roles the new role is made a member of.
    #[serde(default)]
    pub member_of: Vec<String>,
}

impl RoleCreateRequest {
    /// A LOGIN role with the default attributes.
    pub fn login(branch_id: BranchId, name: &str, owner_level: bool) -> Self {
        Self {
            branch_id,
            name: name.to_string(),
            owner_level,
            login: true,
            inherit: true,
            connection_limit: -1,
            valid_until: None,
            bypass_rls: false,
            member_of: Vec::new(),
        }
    }
}

/// A change to a role: `None` leaves a field as it is. `valid_until: Some(None)` (JSON
/// `null`) clears the expiry; `member_of` is the whole new list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleUpdateRequest {
    pub branch_id: BranchId,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_level: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherit: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_limit: Option<i32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "double_option"
    )]
    pub valid_until: Option<Option<DateTime<Utc>>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bypass_rls: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub member_of: Option<Vec<String>>,
}

impl RoleUpdateRequest {
    /// A change that changes nothing yet.
    pub fn new(branch_id: BranchId, name: &str) -> Self {
        Self {
            branch_id,
            name: name.to_string(),
            owner_level: None,
            login: None,
            inherit: None,
            connection_limit: None,
            valid_until: None,
            bypass_rls: None,
            member_of: None,
        }
    }
}

/// Reset and delete.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoleRequest {
    pub branch_id: BranchId,
    pub name: String,
}

/// A data API key that still uses a role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyRef {
    pub id: Uuid,
    pub name: String,
}

/// The worker's answer. `code` says why when `ok` is false: `invalid_role_name`,
/// `reserved_role_name`, `role_exists`, `role_cap_reached`, `branch_not_ready`, `not_found`,
/// `role_in_use` (with `keys`), `role_privilege_mismatch`, `role_membership_not_allowed`,
/// `role_attribute_not_allowed`, `not_applied`, `failed`. `error` is
/// a message (never a password).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleReply {
    pub ok: bool,
    pub code: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub keys: Vec<KeyRef>,
}

/// Core request/reply: the live sessions of a role on a branch's compute (a [`RoleRequest`]
/// in, a [`RoleSessionsReply`] out). A read only: a compute that isn't running answers
/// `compute_running: false` and is never started.
pub const SUBJECT_ROLE_SESSIONS: &str = "chelabase.rpc.roles.sessions";

/// One live session of a role (a `pg_stat_activity` row).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleSession {
    /// The client's address; `None` for a local (Unix socket) connection.
    pub client_addr: Option<String>,
    pub application_name: String,
    pub backend_start: DateTime<Utc>,
}

/// The worker's answer to [`SUBJECT_ROLE_SESSIONS`]. `code` says why when `ok` is false:
/// `branch_not_ready` or `failed` (`error` is for logs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleSessionsReply {
    pub ok: bool,
    pub code: Option<String>,
    pub error: Option<String>,
    /// Whether the branch's compute was running; when it wasn't, nothing was read.
    #[serde(default)]
    pub compute_running: bool,
    /// The role's sessions, oldest first.
    #[serde(default)]
    pub sessions: Vec<RoleSession>,
}

impl RoleSessionsReply {
    /// The sessions read on the running compute.
    pub fn running(sessions: Vec<RoleSession>) -> Self {
        Self { ok: true, code: None, error: None, compute_running: true, sessions }
    }

    /// No compute running: nothing to read, and nothing started.
    pub fn not_running() -> Self {
        Self { compute_running: false, ..Self::running(Vec::new()) }
    }

    pub fn not_ready() -> Self {
        Self { ok: false, code: Some("branch_not_ready".into()), ..Self::not_running() }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            code: Some("failed".into()),
            error: Some(message.into()),
            ..Self::not_running()
        }
    }
}

/// The worker's answer. `kind` says why when `ok` is false: `refused` (the caller's fault;
/// `error` says why), `not_ready` (the branch isn't ready or its compute can't be reached) or
/// `failed` (our side; `error` is for logs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckDataRoleReply {
    pub ok: bool,
    pub error: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    /// The checked role's `pg_roles.oid` (a secret key is bound to it); absent in other
    /// replies and from workers that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_oid: Option<i64>,
    /// The Neon timeline `role_oid` was read on (E26: a key is bound to OID and timeline);
    /// absent with `role_oid`, and from workers that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_timeline: Option<String>,
    /// E33: whether the checked role has BYPASSRLS (a secret key may use such a role; Studio
    /// marks the key); absent in other replies and from workers that predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_bypass_rls: Option<bool>,
}

impl CheckDataRoleReply {
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            kind: None,
            role_oid: None,
            role_timeline: None,
            role_bypass_rls: None,
        }
    }

    /// A role check that passed: the role's OID and the timeline it was read on.
    pub fn ok_with_oid(oid: i64, timeline: &str) -> Self {
        Self { role_oid: Some(oid), role_timeline: Some(timeline.to_string()), ..Self::ok() }
    }

    /// This reply, saying whether the checked role has BYPASSRLS (E33).
    pub fn bypassing_rls(self, bypass_rls: bool) -> Self {
        Self { role_bypass_rls: Some(bypass_rls), ..self }
    }

    pub fn refused(message: impl Into<String>) -> Self {
        Self { ok: false, error: Some(message.into()), kind: Some("refused".into()), ..Self::ok() }
    }

    pub fn not_ready() -> Self {
        Self { ok: false, error: Some("not ready".into()), kind: Some("not_ready".into()), ..Self::ok() }
    }

    pub fn failed(message: impl Into<String>) -> Self {
        Self { ok: false, error: Some(message.into()), kind: Some("failed".into()), ..Self::ok() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconcileDatabaseJob {
    pub database_id: DatabaseId,
}

/// NATS connection settings.
#[derive(Debug, Clone)]
pub struct NatsConfig {
    pub url: String,
}

impl NatsConfig {
    pub fn from_env() -> Result<Self, String> {
        let url = std::env::var("CHELABASE_NATS_URL")
            .unwrap_or_else(|_| "nats://127.0.0.1:4222".to_string());
        Ok(Self { url })
    }
}

#[derive(Debug, Error)]
pub enum NatsError {
    #[error("nats connect error: {0}")]
    Connect(#[from] async_nats::ConnectError),
    #[error("nats error: {0}")]
    Client(#[from] async_nats::Error),
    #[error("jetstream error: {0}")]
    JetStream(String),
    #[error("serialize error: {0}")]
    Serialize(#[from] serde_json::Error),
}

pub use async_nats::jetstream::Context as JetStreamContext;

pub async fn connect(config: &NatsConfig) -> Result<async_nats::Client, NatsError> {
    Ok(async_nats::connect(&config.url).await?)
}

/// Every subject the control stream must carry.
pub fn control_subjects() -> Vec<String> {
    vec![
        SUBJECT_PROVISION.to_string(),
        SUBJECT_RECONCILE.to_string(),
        SUBJECT_DELETE.to_string(),
        SUBJECT_BRANCH.to_string(),
        SUBJECT_DATABASE_SETTINGS.to_string(),
        SUBJECT_PROJECT_DELETE.to_string(),
        SUBJECT_APP_USERS.to_string(),
        SUBJECT_APP_USERS_SYNC.to_string(),
        SUBJECT_MERGE.to_string(),
    ]
}

/// Control subjects an existing stream doesn't carry yet.
pub fn missing_subjects(existing: &[String]) -> Vec<String> {
    control_subjects()
        .into_iter()
        .filter(|s| !existing.contains(s))
        .collect()
}

/// Create the control stream, or add subjects an older stream is missing.
pub async fn ensure_control_stream(
    client: &async_nats::Client,
) -> Result<async_nats::jetstream::Context, NatsError> {
    let js = async_nats::jetstream::new(client.clone());
    match js.get_stream(STREAM_CONTROL).await {
        Ok(stream) => {
            let mut config = stream.cached_info().config.clone();
            let missing = missing_subjects(&config.subjects);
            if missing.is_empty() {
                info!(stream = STREAM_CONTROL, "jetstream stream exists");
            } else {
                config.subjects.extend(missing.iter().cloned());
                js.update_stream(config)
                    .await
                    .map_err(|e| NatsError::JetStream(e.to_string()))?;
                info!(stream = STREAM_CONTROL, ?missing, "jetstream stream subjects added");
            }
        }
        Err(_) => {
            js.create_stream(async_nats::jetstream::stream::Config {
                name: STREAM_CONTROL.to_string(),
                subjects: control_subjects(),
                retention: async_nats::jetstream::stream::RetentionPolicy::WorkQueue,
                ..Default::default()
            })
            .await
            .map_err(|e| NatsError::JetStream(e.to_string()))?;
            info!(stream = STREAM_CONTROL, "jetstream stream created");
        }
    }
    Ok(js)
}

pub async fn publish_provision(
    js: &async_nats::jetstream::Context,
    job: &JobEnvelope<ProvisionDatabaseJob>,
) -> Result<(), NatsError> {
    let payload = serde_json::to_vec(job)?;
    js.publish(SUBJECT_PROVISION.to_string(), payload.into())
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;
    Ok(())
}

pub async fn publish_delete(
    js: &async_nats::jetstream::Context,
    job: &JobEnvelope<DeleteDatabaseJob>,
) -> Result<(), NatsError> {
    let payload = serde_json::to_vec(job)?;
    js.publish(SUBJECT_DELETE.to_string(), payload.into())
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;
    Ok(())
}

pub async fn publish_branch(
    js: &async_nats::jetstream::Context,
    job: &JobEnvelope<BranchJob>,
) -> Result<(), NatsError> {
    let payload = serde_json::to_vec(job)?;
    js.publish(SUBJECT_BRANCH.to_string(), payload.into())
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;
    Ok(())
}

pub async fn publish_merge(
    js: &async_nats::jetstream::Context,
    job: &JobEnvelope<MergeJob>,
) -> Result<(), NatsError> {
    let payload = serde_json::to_vec(job)?;
    js.publish(SUBJECT_MERGE.to_string(), payload.into())
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;
    Ok(())
}

pub async fn publish_database_settings(
    js: &async_nats::jetstream::Context,
    job: &JobEnvelope<DatabaseSettingsJob>,
) -> Result<(), NatsError> {
    let payload = serde_json::to_vec(job)?;
    js.publish(SUBJECT_DATABASE_SETTINGS.to_string(), payload.into())
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;
    Ok(())
}

/// Run the branch consumer loop, calling `handler` for each job with its delivery attempt
/// (1 for the first delivery), so a handler can bound its own retries.
pub async fn run_branch_consumer<F, Fut>(
    js: async_nats::jetstream::Context,
    handler: F,
) -> Result<(), NatsError>
where
    F: FnMut(JobEnvelope<BranchJob>, u64) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    consume(js, ConsumerSpec::branch(), handler).await
}

/// Run the merge consumer loop, like [`run_branch_consumer`]: one job at a time.
pub async fn run_merge_consumer<F, Fut>(
    js: async_nats::jetstream::Context,
    handler: F,
) -> Result<(), NatsError>
where
    F: FnMut(JobEnvelope<MergeJob>, u64) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    consume(js, ConsumerSpec::merge(), handler).await
}

/// Run the settings consumer loop, calling `handler` for each job.
pub async fn run_settings_consumer<F, Fut>(
    js: async_nats::jetstream::Context,
    handler: F,
) -> Result<(), NatsError>
where
    F: FnMut(JobEnvelope<DatabaseSettingsJob>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    run_consumer(js, CONSUMER_DATABASE_SETTINGS, SUBJECT_DATABASE_SETTINGS, handler).await
}

pub async fn publish_project_delete(
    js: &async_nats::jetstream::Context,
    job: &JobEnvelope<ProjectDeleteJob>,
) -> Result<(), NatsError> {
    let payload = serde_json::to_vec(job)?;
    js.publish(SUBJECT_PROJECT_DELETE.to_string(), payload.into())
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;
    Ok(())
}

pub async fn publish_app_users(
    js: &async_nats::jetstream::Context,
    job: &JobEnvelope<AppUsersJob>,
) -> Result<(), NatsError> {
    let payload = serde_json::to_vec(job)?;
    js.publish(SUBJECT_APP_USERS.to_string(), payload.into())
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;
    Ok(())
}

pub async fn publish_app_users_sync(
    js: &async_nats::jetstream::Context,
    job: &JobEnvelope<AppUsersSyncJob>,
) -> Result<(), NatsError> {
    let payload = serde_json::to_vec(job)?;
    js.publish(SUBJECT_APP_USERS_SYNC.to_string(), payload.into())
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;
    Ok(())
}

/// Run the project delete consumer loop, calling `handler` for each job.
pub async fn run_project_delete_consumer<F, Fut>(
    js: async_nats::jetstream::Context,
    handler: F,
) -> Result<(), NatsError>
where
    F: FnMut(JobEnvelope<ProjectDeleteJob>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    run_consumer(js, CONSUMER_PROJECT_DELETE, SUBJECT_PROJECT_DELETE, handler).await
}

/// Run the app users consumer loop, calling `handler` for each job.
pub async fn run_app_users_consumer<F, Fut>(
    js: async_nats::jetstream::Context,
    handler: F,
) -> Result<(), NatsError>
where
    F: FnMut(JobEnvelope<AppUsersJob>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    run_consumer(js, CONSUMER_APP_USERS, SUBJECT_APP_USERS, handler).await
}

/// Run the app users sync consumer loop, calling `handler` for each job.
pub async fn run_app_users_sync_consumer<F, Fut>(
    js: async_nats::jetstream::Context,
    handler: F,
) -> Result<(), NatsError>
where
    F: FnMut(JobEnvelope<AppUsersSyncJob>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    run_consumer(js, CONSUMER_APP_USERS_SYNC, SUBJECT_APP_USERS_SYNC, handler).await
}

/// Run the delete consumer loop, calling `handler` for each job.
pub async fn run_delete_consumer<F, Fut>(
    js: async_nats::jetstream::Context,
    handler: F,
) -> Result<(), NatsError>
where
    F: FnMut(JobEnvelope<DeleteDatabaseJob>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    run_consumer(js, CONSUMER_DELETE, SUBJECT_DELETE, handler).await
}

/// Run the provision consumer loop, calling `handler` for each job.
pub async fn run_provision_consumer<F, Fut>(
    js: async_nats::jetstream::Context,
    handler: F,
) -> Result<(), NatsError>
where
    F: FnMut(JobEnvelope<ProvisionDatabaseJob>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    run_consumer(js, CONSUMER_PROVISION, SUBJECT_PROVISION, handler).await
}

/// How long a job may run before NATS redelivers it. The worst case is a provision that fails
/// late and tears down: two storage-controller calls (10 s each), the 60 s readiness wait, up
/// to 30 s of admin SQL, then the teardown (a tenant delete waits up to 60 s, plus 10 s per
/// request), about 180 s. A redelivery in the middle would run a second copy of the job.
///
/// Limits:
/// - Docker calls (bollard) have no timeout of their own, so a hung Docker can outlast it.
/// - The consumer prefetches messages and handles them one at a time. Each one's timer starts
///   when it's delivered, so a job queued behind slow ones can be redelivered before it runs.
/// - A redelivered provision whose first copy failed provisions again: `failed` rows aren't
///   skipped, so the database can still become ready.
const ACK_WAIT: std::time::Duration = std::time::Duration::from_secs(300);

/// Where and how a consumer pulls: its stream, durable name and subject, how long a delivery
/// may run before it's redelivered, and how long a failed job waits before redelivery.
struct ConsumerSpec<'a> {
    stream: &'a str,
    durable: &'a str,
    subject: &'a str,
    ack_wait: std::time::Duration,
    nak_delay: std::time::Duration,
}

impl<'a> ConsumerSpec<'a> {
    fn control(durable: &'a str, subject: &'a str) -> Self {
        Self {
            stream: STREAM_CONTROL,
            durable,
            subject,
            ack_wait: ACK_WAIT,
            nak_delay: REDELIVERY_DELAY,
        }
    }

    /// The branch consumer: a control consumer that waits [`BRANCH_REDELIVERY_DELAY`] before
    /// a failed job's redelivery.
    fn branch() -> ConsumerSpec<'static> {
        ConsumerSpec {
            nak_delay: BRANCH_REDELIVERY_DELAY,
            ..ConsumerSpec::control(CONSUMER_BRANCH, SUBJECT_BRANCH)
        }
    }

    /// The merge consumer: the same spec as the branch consumer's.
    fn merge() -> ConsumerSpec<'static> {
        ConsumerSpec {
            nak_delay: BRANCH_REDELIVERY_DELAY,
            ..ConsumerSpec::control(CONSUMER_MERGE, SUBJECT_MERGE)
        }
    }
}

/// Pull `subject` with durable consumer `durable`, acking handled jobs and
/// nak-ing failures for redelivery. Unparseable payloads are acked (dropped).
async fn run_consumer<T, F, Fut>(
    js: async_nats::jetstream::Context,
    durable: &str,
    subject: &str,
    mut handler: F,
) -> Result<(), NatsError>
where
    T: serde::de::DeserializeOwned,
    F: FnMut(JobEnvelope<T>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    consume(js, ConsumerSpec::control(durable, subject), move |job, _| {
        handler(job)
    })
    .await
}

/// The consumer loop: each job goes to `handler` with its delivery attempt (1-based, from
/// JetStream's count, so it survives a worker restart).
async fn consume<T, F, Fut>(
    js: async_nats::jetstream::Context,
    spec: ConsumerSpec<'_>,
    mut handler: F,
) -> Result<(), NatsError>
where
    T: serde::de::DeserializeOwned,
    F: FnMut(JobEnvelope<T>, u64) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let durable = spec.durable;
    let stream = js
        .get_stream(spec.stream)
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;

    let consumer = stream
        .create_consumer(async_nats::jetstream::consumer::pull::Config {
            durable_name: Some(durable.to_string()),
            filter_subject: spec.subject.to_string(),
            ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
            ack_wait: spec.ack_wait,
            ..Default::default()
        })
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;

    info!(consumer = durable, "consumer ready");

    let mut messages = consumer
        .messages()
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;

    while let Some(message) = messages.next().await {
        let message = match message {
            Ok(m) => m,
            Err(err) => {
                warn!("consumer message error: {err}");
                continue;
            }
        };

        let job: JobEnvelope<T> = match serde_json::from_slice(&message.payload)
        {
            Ok(j) => j,
            Err(err) => {
                warn!(consumer = durable, "invalid job payload: {err}");
                let _ = message.ack().await;
                continue;
            }
        };
        let attempt = message
            .info()
            .map(|info| u64::try_from(info.delivered).unwrap_or(1).max(1))
            .unwrap_or(1);

        // While the handler runs, tell JetStream it's still in progress (every third of the
        // ack wait), so a long job (a detach can take minutes) isn't delivered to a second
        // copy of the handler meanwhile.
        let result = {
            let run = handler(job, attempt);
            tokio::pin!(run);
            let mut beat = tokio::time::interval(spec.ack_wait / 3);
            beat.tick().await;
            loop {
                tokio::select! {
                    result = &mut run => break result,
                    _ = beat.tick() => {
                        if let Err(err) = message.ack_with(async_nats::jetstream::AckKind::Progress).await {
                            warn!(consumer = durable, "in-progress ack failed: {err}");
                        }
                    }
                }
            }
        };
        match result {
            Ok(()) => {
                if let Err(err) = message.ack().await {
                    warn!("ack failed: {err}");
                }
            }
            Err(err) => {
                warn!(consumer = durable, attempt, "job handler failed: {err}");
                // Back off before redelivery: a failing dependency (e.g. Docker
                // down) would otherwise spin through redeliveries.
                if let Err(nak_err) = message
                    .ack_with(async_nats::jetstream::AckKind::Nak(Some(spec.nak_delay)))
                    .await
                {
                    warn!("nak failed: {nak_err}");
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_create_request_uses_owner_level() {
        let create = RoleCreateRequest::login(BranchId::new(), "app", true);
        let json = serde_json::to_value(&create).unwrap();
        assert_eq!(json["owner_level"], true);
        assert!(json.get("privileged").is_none());
    }

    #[test]
    fn role_rpc_wire_shapes() {
        assert_eq!(SUBJECT_ROLE_CREATE, "chelabase.rpc.roles.create");
        assert_eq!(SUBJECT_ROLE_RESET, "chelabase.rpc.roles.reset");
        assert_eq!(SUBJECT_ROLE_DELETE, "chelabase.rpc.roles.delete");
        let create = RoleCreateRequest::login(BranchId::new(), "app", true);
        let json = serde_json::to_value(&create).unwrap();
        assert_eq!(json["name"], "app");
        assert_eq!(json["owner_level"], true);
        let request = RoleRequest { branch_id: BranchId::new(), name: "app".into() };
        assert!(serde_json::to_value(&request).unwrap()["branch_id"].is_string());
        // A reply without `keys` parses, with none.
        let reply: RoleReply = serde_json::from_str(r#"{"ok":false,"code":"not_found","error":"x"}"#).unwrap();
        assert_eq!(reply.code.as_deref(), Some("not_found"));
        assert!(reply.keys.is_empty());
        let in_use = RoleReply {
            ok: false,
            code: Some("role_in_use".into()),
            error: Some("in use".into()),
            keys: vec![KeyRef { id: Uuid::nil(), name: "k".into() }],
        };
        let back: RoleReply = serde_json::from_slice(&serde_json::to_vec(&in_use).unwrap()).unwrap();
        assert_eq!(back, in_use);
    }

    #[test]
    fn role_sessions_wire_shapes() {
        assert_eq!(SUBJECT_ROLE_SESSIONS, "chelabase.rpc.roles.sessions");
        let session = RoleSession {
            client_addr: Some("10.0.0.7".into()),
            application_name: "api".into(),
            backend_start: "2026-10-08T10:00:00Z".parse().unwrap(),
        };
        let running = RoleSessionsReply::running(vec![session.clone()]);
        assert!(running.ok && running.compute_running);
        let back: RoleSessionsReply = serde_json::from_slice(&serde_json::to_vec(&running).unwrap()).unwrap();
        assert_eq!(back, running);
        let idle = RoleSessionsReply::not_running();
        assert!(idle.ok && !idle.compute_running && idle.sessions.is_empty());
        let not_ready = RoleSessionsReply::not_ready();
        assert_eq!((not_ready.ok, not_ready.code.as_deref()), (false, Some("branch_not_ready")));
        let failed = RoleSessionsReply::failed("boom");
        assert_eq!((failed.ok, failed.code.as_deref(), failed.error.as_deref()), (false, Some("failed"), Some("boom")));
        // A bare failure parses, with no sessions.
        let bare: RoleSessionsReply = serde_json::from_str(r#"{"ok":false,"code":"failed","error":"x"}"#).unwrap();
        assert!(!bare.compute_running && bare.sessions.is_empty());
    }

    #[test]
    fn role_create_request_attributes_default_to_a_plain_login_role() {
        let branch = BranchId::new();
        let old: RoleCreateRequest =
            serde_json::from_value(serde_json::json!({ "branch_id": branch, "name": "app", "owner_level": false }))
                .unwrap();
        assert_eq!(old, RoleCreateRequest::login(branch, "app", false));
        assert!(old.login && old.inherit && !old.bypass_rls);
        assert_eq!((old.connection_limit, old.valid_until, old.member_of.len()), (-1, None, 0));
        let full = RoleCreateRequest {
            login: false,
            inherit: false,
            connection_limit: 3,
            valid_until: Some(DateTime::<Utc>::UNIX_EPOCH),
            bypass_rls: true,
            member_of: vec!["g".into()],
            ..RoleCreateRequest::login(branch, "r", false)
        };
        let back: RoleCreateRequest = serde_json::from_slice(&serde_json::to_vec(&full).unwrap()).unwrap();
        assert_eq!(back, full);
    }

    #[test]
    fn role_update_request_tells_unchanged_from_cleared() {
        assert_eq!(SUBJECT_ROLE_UPDATE, "chelabase.rpc.roles.update");
        let branch = BranchId::new();
        let parse = |v: serde_json::Value| serde_json::from_value::<RoleUpdateRequest>(v).unwrap();
        let none = parse(serde_json::json!({ "branch_id": branch, "name": "r" }));
        assert_eq!(none, RoleUpdateRequest::new(branch, "r"));
        assert_eq!(none.valid_until, None);
        let cleared = parse(serde_json::json!({ "branch_id": branch, "name": "r", "valid_until": null }));
        assert_eq!(cleared.valid_until, Some(None));
        let set = RoleUpdateRequest {
            owner_level: Some(true),
            login: Some(false),
            inherit: Some(false),
            connection_limit: Some(5),
            valid_until: Some(Some(DateTime::<Utc>::UNIX_EPOCH)),
            bypass_rls: Some(true),
            member_of: Some(vec!["g".into()]),
            ..RoleUpdateRequest::new(branch, "r")
        };
        for request in [none, cleared, set] {
            let back: RoleUpdateRequest = serde_json::from_slice(&serde_json::to_vec(&request).unwrap()).unwrap();
            assert_eq!(back, request);
        }
        assert!(ROLE_UPDATE_NOT_APPLIED.contains("retried"));
    }

    #[test]
    fn resync_branch_wire_shape() {
        assert_eq!(SUBJECT_RESYNC_BRANCH, "chelabase.resync.branch");
        let branch_id = BranchId::new();
        let body = serde_json::to_vec(&ResyncBranchRequest { branch_id }).unwrap();
        let back: ResyncBranchRequest = serde_json::from_slice(&body).unwrap();
        assert_eq!(back.branch_id, branch_id);
    }

    #[test]
    fn data_api_database_wire_shapes() {
        assert_eq!(SUBJECT_DATA_API_ENABLE, "chelabase.rpc.data_api.enable");
        assert_eq!(SUBJECT_DATA_API_DISABLE, "chelabase.rpc.data_api.disable");
        assert_eq!(SUBJECT_DATA_API_REBOOTSTRAP, "chelabase.rpc.data_api.rebootstrap");
        let request = DataApiDatabaseRequest { branch_id: BranchId::new(), name: "analytics".into(), remove: true, grant_existing: true };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["name"], "analytics");
        assert_eq!(json["remove"], true);
        assert_eq!(json["grant_existing"], true);
        let back: DataApiDatabaseRequest =
            serde_json::from_str(&format!(r#"{{"branch_id":{},"name":"a"}}"#, json["branch_id"])).unwrap();
        assert!(!back.remove);
        assert!(!back.grant_existing);
    }

    #[test]
    fn check_data_role_wire_shapes() {
        let request = CheckDataRoleRequest {
            branch_id: BranchId::new(),
            role: "reporting".into(),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["role"], "reporting");
        assert!(json["branch_id"].is_string());
        assert_eq!(SUBJECT_CHECK_DATA_ROLE, "chelabase.rpc.check_data_role");

        let ok = serde_json::to_value(CheckDataRoleReply::ok()).unwrap();
        assert_eq!(ok, serde_json::json!({ "ok": true, "error": null, "kind": null }));
        let refused = CheckDataRoleReply::refused("the role x does not exist");
        assert_eq!(
            serde_json::to_value(&refused).unwrap(),
            serde_json::json!({ "ok": false, "error": "the role x does not exist", "kind": "refused" })
        );
        assert_eq!(CheckDataRoleReply::not_ready().kind.as_deref(), Some("not_ready"));
        assert_eq!(CheckDataRoleReply::failed("boom").kind.as_deref(), Some("failed"));
        // A reply without `kind` (the two-field shape) still parses.
        let old: CheckDataRoleReply = serde_json::from_str(r#"{"ok":false,"error":"x"}"#).unwrap();
        assert_eq!(old.kind, None);
        assert_eq!(old.role_oid, None);
        assert_eq!(old.role_timeline, None);
        // A secret key's check answers the role's OID and the timeline it was read on (E26);
        // replies without them omit them.
        let with_oid = serde_json::to_value(CheckDataRoleReply::ok_with_oid(16390, "abc")).unwrap();
        assert_eq!(
            with_oid,
            serde_json::json!({
                "ok": true, "error": null, "kind": null, "role_oid": 16390, "role_timeline": "abc"
            })
        );
        let back: CheckDataRoleReply = serde_json::from_value(with_oid).unwrap();
        assert_eq!(back.role_oid, Some(16390));
        assert_eq!(back.role_timeline.as_deref(), Some("abc"));
        assert_eq!(back.role_bypass_rls, None, "omitted by workers that predate E33");
        // E33: whether the role has BYPASSRLS, when it does or doesn't.
        let bypass = CheckDataRoleReply::ok_with_oid(16390, "abc").bypassing_rls(true);
        let json = serde_json::to_value(&bypass).unwrap();
        assert_eq!(json["role_bypass_rls"], true);
        let back: CheckDataRoleReply = serde_json::from_value(json).unwrap();
        assert_eq!(back.role_bypass_rls, Some(true));
        let plain = CheckDataRoleReply::ok_with_oid(16390, "abc").bypassing_rls(false);
        assert_eq!(serde_json::to_value(&plain).unwrap()["role_bypass_rls"], false);
    }

    /// Opt-in, against the stack's NATS (`just workbench test`): a stream and consumer of their own,
    /// deleted afterwards.
    mod live {
        use std::sync::{Arc, Mutex};
        use std::time::Duration;

        use super::*;

        async fn js() -> async_nats::jetstream::Context {
            let client = connect(&NatsConfig::from_env().unwrap())
                .await
                .expect("NATS reachable (run through just workbench test)");
            async_nats::jetstream::new(client)
        }

        /// A work-queue stream `CHELABASE_TEST_<rand>` on subject `chelabase.test.<rand>`.
        async fn test_stream(js: &async_nats::jetstream::Context) -> (String, String) {
            let tag = Uuid::new_v4().simple().to_string();
            let (stream, subject) = (format!("CHELABASE_TEST_{tag}"), format!("chelabase.test.{tag}"));
            js.create_stream(async_nats::jetstream::stream::Config {
                name: stream.clone(),
                subjects: vec![subject.clone()],
                retention: async_nats::jetstream::stream::RetentionPolicy::WorkQueue,
                ..Default::default()
            })
            .await
            .unwrap();
            (stream, subject)
        }

        async fn publish_one(js: &async_nats::jetstream::Context, subject: &str) {
            let payload = DatabaseSettingsJob { database_id: DatabaseId::new() };
            let job = JobEnvelope { job_id: Uuid::new_v4(), payload };
            js.publish(subject.to_string(), serde_json::to_vec(&job).unwrap().into())
                .await
                .unwrap()
                .await
                .unwrap();
        }

        /// Runs a consumer on a fresh stream for `run_for`, with `handler` answering each
        /// delivery (by attempt), and returns the attempts it saw.
        async fn attempts_seen(
            ack_wait: Duration,
            run_for: Duration,
            handler: impl Fn(u64) -> Result<(), String> + Send + Sync + 'static,
            slow: Duration,
        ) -> Vec<u64> {
            let js = js().await;
            let (stream, subject) = test_stream(&js).await;
            publish_one(&js, &subject).await;
            let seen = Arc::new(Mutex::new(Vec::new()));
            let handler = Arc::new(handler);
            let task = {
                let (js, stream, subject, seen) =
                    (js.clone(), stream.clone(), subject.clone(), seen.clone());
                tokio::spawn(async move {
                    let spec = ConsumerSpec {
                        stream: &stream,
                        durable: "test",
                        subject: &subject,
                        ack_wait,
                        nak_delay: Duration::from_millis(100),
                    };
                    consume(js, spec, move |_job: JobEnvelope<DatabaseSettingsJob>, attempt| {
                        let (seen, handler) = (seen.clone(), handler.clone());
                        async move {
                            seen.lock().unwrap().push(attempt);
                            tokio::time::sleep(slow).await;
                            handler(attempt)
                        }
                    })
                    .await
                })
            };
            tokio::time::sleep(run_for).await;
            task.abort();
            js.delete_stream(&stream).await.unwrap();
            seen.lock().unwrap().clone()
        }

        #[tokio::test]
        #[ignore]
        async fn each_redelivery_counts_its_attempt() {
            let seen = attempts_seen(
                Duration::from_secs(30),
                Duration::from_secs(3),
                |attempt| if attempt < 3 { Err("not yet".into()) } else { Ok(()) },
                Duration::ZERO,
            )
            .await;
            assert_eq!(seen, vec![1, 2, 3]);
        }

        #[tokio::test]
        #[ignore]
        async fn a_job_running_past_its_ack_wait_is_not_delivered_again() {
            let seen = attempts_seen(
                Duration::from_secs(1),
                Duration::from_secs(6),
                |_| Ok(()),
                Duration::from_secs(3),
            )
            .await;
            assert_eq!(seen, vec![1], "the running job keeps its delivery");
        }
    }

    #[test]
    fn branch_jobs_back_off_longer_than_other_jobs() {
        let spec = ConsumerSpec::branch();
        assert_eq!((spec.durable, spec.subject), (CONSUMER_BRANCH, SUBJECT_BRANCH));
        assert_eq!(spec.nak_delay, BRANCH_REDELIVERY_DELAY);
        assert!(BRANCH_REDELIVERY_DELAY > REDELIVERY_DELAY);
        assert_eq!(spec.ack_wait, ACK_WAIT);
    }

    #[test]
    fn stream_subjects_include_delete() {
        assert!(control_subjects().contains(&SUBJECT_DELETE.to_string()));
        assert_eq!(
            missing_subjects(&["chelabase.provision".into()]),
            vec![
                SUBJECT_RECONCILE.to_string(),
                SUBJECT_DELETE.to_string(),
                SUBJECT_BRANCH.to_string(),
                SUBJECT_DATABASE_SETTINGS.to_string(),
                SUBJECT_PROJECT_DELETE.to_string(),
                SUBJECT_APP_USERS.to_string(),
                SUBJECT_APP_USERS_SYNC.to_string(),
                SUBJECT_MERGE.to_string(),
            ]
        );
        assert!(missing_subjects(&control_subjects()).is_empty());
    }

    #[test]
    fn stream_carries_the_branch_and_settings_subjects() {
        assert_eq!(SUBJECT_BRANCH, "chelabase.branch");
        assert_eq!(CONSUMER_BRANCH, "chelabase-worker-branch");
        assert_eq!(SUBJECT_DATABASE_SETTINGS, "chelabase.database_settings");
        let subjects = control_subjects();
        assert!(subjects.contains(&SUBJECT_BRANCH.to_string()));
        assert!(subjects.contains(&SUBJECT_DATABASE_SETTINGS.to_string()));
        // An older stream gets exactly the new ones.
        let old = vec![
            SUBJECT_PROVISION.to_string(),
            SUBJECT_RECONCILE.to_string(),
            SUBJECT_DELETE.to_string(),
        ];
        assert_eq!(
            missing_subjects(&old),
            vec![
                SUBJECT_BRANCH.to_string(),
                SUBJECT_DATABASE_SETTINGS.to_string(),
                SUBJECT_PROJECT_DELETE.to_string(),
                SUBJECT_APP_USERS.to_string(),
                SUBJECT_APP_USERS_SYNC.to_string(),
                SUBJECT_MERGE.to_string(),
            ]
        );
    }

    #[test]
    fn control_subjects_include_merge() {
        assert_eq!(SUBJECT_MERGE, "chelabase.merge");
        assert_eq!(CONSUMER_MERGE, "chelabase-worker-merge");
        assert!(control_subjects().contains(&SUBJECT_MERGE.to_string()));
        let spec = ConsumerSpec::merge();
        assert_eq!((spec.durable, spec.subject), (CONSUMER_MERGE, SUBJECT_MERGE));
        assert_eq!(spec.ack_wait, ACK_WAIT);
    }

    #[test]
    fn merge_job_serde_shape() {
        let merge_id = uuid::Uuid::new_v4();
        let json = serde_json::to_value(MergeJob::Apply { merge_id }).unwrap();
        assert_eq!(json, serde_json::json!({"kind": "apply", "merge_id": merge_id}));
        for job in [
            MergeJob::Compute { merge_id },
            MergeJob::Rehearse { merge_id },
            MergeJob::Apply { merge_id },
            MergeJob::Resume { merge_id },
            MergeJob::Cancel { merge_id },
        ] {
            let back: MergeJob = serde_json::from_value(serde_json::to_value(&job).unwrap()).unwrap();
            assert_eq!(back, job);
        }
    }

    // E21: the after-merge choice run by hand on a merge applied with drift.
    #[test]
    fn merge_finish_job_serde_shape() {
        let merge_id = uuid::Uuid::new_v4();
        let job = MergeJob::Finish {
            merge_id,
            after: MergeAfter::Reset,
        };
        let json = serde_json::to_value(&job).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"kind": "finish", "merge_id": merge_id, "after": "reset"})
        );
        let back: MergeJob = serde_json::from_value(json).unwrap();
        assert_eq!(back, job);
        for (after, text) in [
            (MergeAfter::Keep, "keep"),
            (MergeAfter::Reset, "reset"),
            (MergeAfter::Delete, "delete"),
        ] {
            assert_eq!(after.as_str(), text);
            assert_eq!(serde_json::to_value(after).unwrap(), serde_json::json!(text));
        }
    }

    #[test]
    fn control_subjects_include_app_jobs() {
        let subjects = control_subjects();
        for s in ["chelabase.project_delete", "chelabase.app_users", "chelabase.app_users_sync"] {
            assert!(subjects.contains(&s.to_string()), "{s}");
        }
        // The check subject is core request/reply, not on the stream.
        assert_eq!(SUBJECT_APP_USERS_CHECK, "chelabase.rpc.app_users.check");
        assert!(!subjects.contains(&SUBJECT_APP_USERS_CHECK.to_string()));
        assert_eq!(CONSUMER_PROJECT_DELETE, "chelabase-worker-project-delete");
        assert_eq!(CONSUMER_APP_USERS, "chelabase-worker-app-users");
        assert_eq!(CONSUMER_APP_USERS_SYNC, "chelabase-worker-app-users-sync");
    }

    #[test]
    fn app_users_job_round_trips() {
        let project_id = ProjectId::new();
        assert_eq!(
            serde_json::to_value(AppUsersJob::Enable { project_id }).unwrap(),
            serde_json::json!({ "enable": { "project_id": project_id } })
        );
        for job in [
            AppUsersJob::Enable { project_id },
            AppUsersJob::Disable { project_id },
            AppUsersJob::Reconcile { project_id },
        ] {
            let back: AppUsersJob =
                serde_json::from_slice(&serde_json::to_vec(&job).unwrap()).unwrap();
            assert_eq!(back, job);
        }
    }

    #[test]
    fn sync_job_defaults_missing_fields() {
        let (project_id, user_id) = (ProjectId::new(), Uuid::new_v4());
        let job: AppUsersSyncJob = serde_json::from_value(
            serde_json::json!({ "project_id": project_id, "user_id": user_id }),
        )
        .unwrap();
        assert_eq!(job.user_id, user_id);
        assert!(job.data.is_none());
        assert!(job.origin_branch_id.is_none());
        let reply: AppUsersCheckReply = serde_json::from_value(serde_json::json!({ "ok": true })).unwrap();
        assert!(reply.ok && reply.code.is_none() && reply.error.is_none());
    }

    #[test]
    fn point_in_time_is_snake_case_tagged() {
        let at = PointInTime::Lsn("0/16B3748".into());
        assert_eq!(serde_json::to_value(&at).unwrap(), serde_json::json!({ "lsn": "0/16B3748" }));
        let time: DateTime<Utc> = "2026-09-30T12:00:00Z".parse().unwrap();
        assert_eq!(
            serde_json::to_value(PointInTime::Timestamp(time)).unwrap(),
            serde_json::json!({ "timestamp": "2026-09-30T12:00:00Z" })
        );
    }

    #[test]
    fn branch_jobs_round_trip() {
        let branch_id = BranchId::new();
        let jobs = vec![
            BranchJob::Create { branch_id, at: None },
            BranchJob::Create { branch_id, at: Some(PointInTime::Lsn("0/1".into())) },
            BranchJob::ResetToParent { branch_id },
            BranchJob::Restore {
                branch_id,
                backup_id: BranchId::new(),
                backup_name: "dev-before-restore-20260930T120000Z".into(),
                at: PointInTime::Timestamp(Utc::now()),
            },
            BranchJob::Delete { branch_id },
        ];
        for job in jobs {
            let envelope = JobEnvelope { job_id: Uuid::new_v4(), payload: job.clone() };
            let back: JobEnvelope<BranchJob> =
                serde_json::from_slice(&serde_json::to_vec(&envelope).unwrap()).unwrap();
            assert_eq!(back.payload, job);
        }
    }

    #[test]
    fn reset_request_carries_the_branch() {
        let (database_id, branch_id) = (DatabaseId::new(), BranchId::new());
        let json = serde_json::to_value(ResetPasswordRequest { database_id, branch_id }).unwrap();
        assert_eq!(json["branch_id"], serde_json::json!(branch_id));
        assert_eq!(json["database_id"], serde_json::json!(database_id));
    }

    #[test]
    fn reset_request_without_a_branch_means_main() {
        let database_id = DatabaseId::new();
        let back: ResetPasswordRequest =
            serde_json::from_value(serde_json::json!({ "database_id": database_id })).unwrap();
        assert_eq!(back.database_id, database_id);
        assert_eq!(back.branch_id, BranchId::from(database_id));
    }

    #[test]
    fn member_role_hand_over_wire_shape() {
        assert_eq!(SUBJECT_MEMBER_ROLES_HAND_OVER, "chelabase.rpc.member_roles.hand_over");
        assert_eq!(SUBJECT_REVIEW, "chelabase.rpc.review");
        let request = MemberRoleHandOverRequest {
            branch_id: Uuid::new_v4(),
            member_id: Uuid::new_v4(),
            database: "app".into(),
            acted_role: "billing".into(),
            backend_pid: Some(77),
            backend_start: Some("2026-10-09 10:00:00+00".into()),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["backend_start"], "2026-10-09 10:00:00+00");
        assert_eq!((json["database"].as_str(), json["acted_role"].as_str(), json["backend_pid"].as_i64()), (Some("app"), Some("billing"), Some(77)));
        assert_eq!(serde_json::from_value::<MemberRoleHandOverRequest>(json).unwrap(), request);
    }

    #[test]
    fn member_roles_wire_shapes() {
        assert_eq!(SUBJECT_MEMBER_ROLES_RECONCILE, "chelabase.rpc.member_roles.reconcile");
        assert_eq!(SUBJECT_MEMBER_ROLES_ENQUEUE, "chelabase.member_roles.reconcile");
        let (branch_id, member_id) = (Uuid::new_v4(), Uuid::new_v4());
        let request = MemberRolesReconcileRequest { branch_id, member_id: Some(member_id) };
        let back: MemberRolesReconcileRequest =
            serde_json::from_slice(&serde_json::to_vec(&request).unwrap()).unwrap();
        assert_eq!((back.branch_id, back.member_id), (branch_id, Some(member_id)));
        let whole: MemberRolesReconcileRequest =
            serde_json::from_value(serde_json::json!({ "branch_id": branch_id, "member_id": null }))
                .unwrap();
        assert_eq!(whole.member_id, None);
        let done = MemberRolesReply::Done {
            roles: vec![MemberRoleApplied {
                member_id,
                role_name: "cb_m_x".into(),
                applied_roles: vec!["billing".into()],
            }],
        };
        let back: MemberRolesReply =
            serde_json::from_slice(&serde_json::to_vec(&done).unwrap()).unwrap();
        assert_eq!(back, done);
        // R17: one member role per (member, branch), so no Postgres database in the reply.
        let json = serde_json::to_value(&done).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "Done": { "roles": [{
                "member_id": member_id,
                "role_name": "cb_m_x",
                "applied_roles": ["billing"],
            }] } })
        );
        let failed = MemberRolesReply::Failed { message: "boom".into() };
        let back: MemberRolesReply =
            serde_json::from_slice(&serde_json::to_vec(&failed).unwrap()).unwrap();
        assert_eq!(back, failed);
    }
}
