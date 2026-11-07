//! NATS JetStream subjects, envelopes, and helpers for Chelabase control plane.

use chelabase_types::{BranchId, DatabaseId, ProjectId, TenantId};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};
use uuid::Uuid;

pub mod fn_events;
pub mod rt;
pub mod storage_objects;

#[cfg(test)]
mod t4_tests;

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

/// Durable storage sweep jobs: one Chelabase database's orphaned blobs (object-storage m8).
/// One token under `chelabase.`, like every control job: the services' publish right is
/// `chelabase.*`.
pub const SUBJECT_STORAGE_SWEEP: &str = "chelabase.storage_sweep";
pub const CONSUMER_STORAGE_SWEEP: &str = "chelabase-worker-storage-sweep";

/// Object storage's cache events (m9), core NATS (not JetStream), published by the
/// orchestrator and heard by every storage replica (`chelabase.storage.>`): a data key revoked,
/// rotated or its database list changed ([`StorageEvent::Key`]: its signed URLs' cached
/// liveness goes), and a branch's settings changed ([`StorageEvent::Branch`]: its connection
/// info and pools go). Lost events are covered by storage's own short cache lifetimes.
pub const SUBJECT_STORAGE_EVENTS: &str = "chelabase.storage.events";

/// What [`SUBJECT_STORAGE_EVENTS`] carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum StorageEvent {
    Key { key_id: Uuid },
    Branch { branch_id: Uuid },
}

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

/// Sweep one Chelabase database's blobs (or, once it is deleted, its whole prefix).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageSweepJob {
    pub database_id: DatabaseId,
}

/// The JetStream message id of a database's sweep job, `storage-sweep-{db}-{tag}`: the stream
/// drops a second one with the same tag inside its duplicate window. The tag is the period
/// (the periodic job), an event's id, or `purge`, so one kind never swallows another.
pub fn storage_sweep_msg_id(database: DatabaseId, tag: &str) -> String {
    format!("storage-sweep-{database}-{tag}")
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
    /// The DDL receiver asked for this run (a `CREATE DATABASE` it forwarded may not be visible
    /// yet): the worker looks once more when the first look found nothing to open (RT3-11).
    #[serde(default)]
    pub from_ddl: bool,
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
/// T10 fix (preview, then confirm): what a role delete to a successor would expose (a
/// [`RoleDeleteRequest`] in, a [`RoleReply`] with `preview` out). A read only.
pub const SUBJECT_ROLE_DELETE_PREVIEW: &str = "chelabase.rpc.roles.delete_preview";

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

/// T10: a role delete and the role its objects go to. `successor` is a role on the branch the
/// owner can SET to, or `chelabase_orphaned` (the quarantine role); a request without one (an
/// orchestrator that predates T10) means `chelabase_orphaned`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleDeleteRequest {
    pub branch_id: BranchId,
    pub name: String,
    #[serde(default = "quarantine_successor")]
    pub successor: String,
    /// T10 fix: the preview's `confirm` token, when the Admin confirmed the exposures it
    /// listed. Absent (and never serialized) when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm: Option<String>,
}

/// T10 fix: the kind of an exposed object (`function`, `procedure`, `view`,
/// `materialized_view` on the wire). The derived order is the one exposures sort by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExposureKind {
    Function,
    Procedure,
    View,
    MaterializedView,
}

impl ExposureKind {
    /// Every kind, in order.
    pub const ALL: [ExposureKind; 4] = [Self::Function, Self::Procedure, Self::View, Self::MaterializedView];

    /// The wire name (as serde writes it).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Procedure => "procedure",
            Self::View => "view",
            Self::MaterializedView => "materialized_view",
        }
    }

    /// The kind whose wire name is exactly `name`.
    pub fn from_wire(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == name)
    }
}

/// T10 fix: an object the deleted role owns that would run with the successor's rights for
/// someone else: a `SECURITY DEFINER` routine or a view (or materialized view) without
/// `security_invoker = true`, carrying a privilege granted to a grantee other than the
/// successor (and the deleted role itself, whose own entry becomes the successor's). A routine's
/// PUBLIC grant alone doesn't count (PUBLIC EXECUTE is revoked on every routine that moves).
/// `identity`: `regprocedure` / `regclass` text; `grantees` sorted, deduplicated, PUBLIC as
/// `"PUBLIC"`. A list of them is sorted by this derived order (database, kind, identity,
/// grantees).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Exposure {
    pub database: String,
    pub kind: ExposureKind,
    pub identity: String,
    pub grantees: Vec<String>,
}

/// T10 fix: a role delete's preview (and the current state a refused delete answers with):
/// the exposures over every database, and the token that confirms them (`None` when there are
/// none or the successor is the quarantine role).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeletePreview {
    pub successor: String,
    #[serde(default)]
    pub exposures: Vec<Exposure>,
    #[serde(default)]
    pub confirm: Option<String>,
}

/// T10 fix: the confirm token: the lowercase hex SHA-256 of the canonical JSON of
/// `{successor, exposures}`: object keys sorted at every level, no whitespace, e.g.
/// `{"exposures":[{"database":"app","grantees":["anon"],"identity":"public.f()","kind":"function"}],"successor":"heir"}`.
/// Independent of how `serde_json` orders its maps (the `preserve_order` feature).
pub fn exposure_token(successor: &str, exposures: &[Exposure]) -> String {
    use sha2::{Digest, Sha256};
    let value = serde_json::json!({ "successor": successor, "exposures": exposures });
    let mut canonical = String::new();
    canonical_json(&value, &mut canonical);
    hex::encode(Sha256::digest(canonical.as_bytes()))
}

/// `value` as compact JSON with every object's keys sorted (by their UTF-8 bytes).
fn canonical_json(value: &serde_json::Value, out: &mut String) {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                canonical_json(&map[key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical_json(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// The quarantine role, a [`RoleDeleteRequest`]'s successor when none is named.
pub const QUARANTINE_SUCCESSOR: &str = "chelabase_orphaned";

fn quarantine_successor() -> String {
    QUARANTINE_SUCCESSOR.to_string()
}

/// T10: what a role delete handed to its successor, over every database: `count` objects, and
/// the `SECURITY DEFINER` routines among them (`regprocedure` text) for review.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reassigned {
    pub count: u64,
    pub definers: Vec<String>,
    /// T10 fix: the exposures the delete kept because the Admin confirmed them (empty when
    /// none, or for the quarantine role; absent on the wire then).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub confirmed: Vec<Exposure>,
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
/// `role_attribute_not_allowed`, `invalid_successor`, `exposure_unconfirmed` (with `preview`),
/// `not_applied`, `failed`. `error` is
/// a message (never a password).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleReply {
    pub ok: bool,
    pub code: Option<String>,
    pub error: Option<String>,
    #[serde(default)]
    pub keys: Vec<KeyRef>,
    /// T10: a done delete's [`Reassigned`]; absent in other replies and from workers that
    /// predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reassigned: Option<Reassigned>,
    /// T10 fix: a preview's answer, and the current exposures a delete refused with
    /// `exposure_unconfirmed` answers with; absent otherwise (and from workers that predate it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<DeletePreview>,
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

/// EOS-H1 (user ruling 2026-10-23: fail closed, via the worker): would a key create, or a
/// role's membership change, let the data API reach a role with the members hub's "all rows"
/// policy? An [`AllRowsCheckRequest`] in, an [`AllRowsCheckReply`] out. The worker reads, as
/// `cloud_admin` on the branch's compute, the cluster's memberships and the "all rows" roles of
/// every live database. A read only.
pub const SUBJECT_ALL_ROWS_CHECK: &str = "chelabase.rpc.roles.all_rows_check";

/// What [`SUBJECT_ALL_ROWS_CHECK`] checks on `branch_id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllRowsCheckRequest {
    pub branch_id: BranchId,
    /// The roles the data API reaches rows through, sorted and deduplicated: a key create's
    /// role; for a membership change, `anon`, `authenticated`, `authenticator` and the role of
    /// every live (unrevoked) key of the branch.
    pub starting: Vec<String>,
    /// A membership change: only a (starting role, "all rows" role) pair it newly connects
    /// counts. Absent (and never serialized) for a key create, where any reach counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<MembershipChange>,
}

/// A role's memberships replaced, as the worker's role update applies it: `member_of` is the
/// whole new list; the role's `pg_*` and Chelabase memberships stay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MembershipChange {
    pub role: String,
    pub member_of: Vec<String>,
}

/// The worker's answer to [`SUBJECT_ALL_ROWS_CHECK`] (`status`: `clear`, `exposed`, `failed`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AllRowsCheckReply {
    /// Nothing is (newly) exposed.
    Clear,
    /// One of the shortest chains, `[starting role, …, "all rows" role]`.
    Exposed { chain: Vec<String> },
    /// The check couldn't be completed (the branch isn't ready, the compute can't be reached,
    /// a read failed or ran past its deadline): the caller refuses, never allows.
    Failed { message: String },
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

/// NATS connection settings: the server, the service's user and password (R11: one user per
/// service, no anonymous access) and its own reply-inbox prefix (`_INBOX_<user>`), so no service
/// subscribes to another's replies.
#[derive(Clone)]
pub struct NatsConfig {
    pub url: String,
    pub user: Option<String>,
    pub password: Option<String>,
    pub inbox_prefix: Option<String>,
}

impl std::fmt::Debug for NatsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NatsConfig")
            .field("url", &self.url)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("inbox_prefix", &self.inbox_prefix)
            .finish()
    }
}

pub const DEFAULT_NATS_URL: &str = "nats://127.0.0.1:4222";

impl NatsConfig {
    /// From the process environment; see [`NatsConfig::from_lookup`].
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// `CHELABASE_NATS_URL` (default `nats://127.0.0.1:4222`), `CHELABASE_NATS_USER`, its
    /// password as `CHELABASE_NATS_PASSWORD_FILE` (a file, one trailing newline ignored) or
    /// `CHELABASE_NATS_PASSWORD`, and `CHELABASE_NATS_INBOX_PREFIX`. A user needs exactly one
    /// password form and a password needs a user; neither means anonymous (unit tests only:
    /// the stack's server refuses it). Blank values count as unset. Errors never contain a
    /// password.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let get = |name: &str| lookup(name).filter(|v| !v.trim().is_empty());
        let url = get("CHELABASE_NATS_URL")
            .map(|v| v.trim().to_string())
            .unwrap_or_else(|| DEFAULT_NATS_URL.to_string());
        let user = get("CHELABASE_NATS_USER").map(|v| v.trim().to_string());
        let password = match (
            get("CHELABASE_NATS_PASSWORD"),
            get("CHELABASE_NATS_PASSWORD_FILE"),
        ) {
            (Some(_), Some(_)) => {
                return Err(
                    "set CHELABASE_NATS_PASSWORD or CHELABASE_NATS_PASSWORD_FILE, not both"
                        .to_string(),
                );
            }
            (Some(plain), None) => Some(plain),
            (None, Some(path)) => Some(read_secret_file("CHELABASE_NATS_PASSWORD_FILE", &path)?),
            (None, None) => None,
        };
        match (&user, &password) {
            (Some(_), None) => {
                return Err("CHELABASE_NATS_USER is set without a password \
                     (CHELABASE_NATS_PASSWORD_FILE or CHELABASE_NATS_PASSWORD)"
                    .to_string());
            }
            (None, Some(_)) => {
                return Err("a NATS password is set without CHELABASE_NATS_USER".to_string());
            }
            _ => {}
        }
        let inbox_prefix = get("CHELABASE_NATS_INBOX_PREFIX").map(|v| v.trim().to_string());
        if let Some(prefix) = &inbox_prefix {
            check_inbox_prefix(prefix)?;
        }
        Ok(Self {
            url,
            user,
            password,
            inbox_prefix,
        })
    }
}

/// A secret file's contents without its trailing line break; empty or unreadable is an error
/// naming the variable and the path (never the contents).
fn read_secret_file(var: &str, path: &str) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{var}={path}: {e}"))?;
    let value = text.trim_end_matches(['\n', '\r']);
    if value.is_empty() {
        return Err(format!("{var}={path} is empty"));
    }
    Ok(value.to_string())
}

/// An inbox prefix is one or more plain subject tokens: no wildcard, blank or empty token.
fn check_inbox_prefix(prefix: &str) -> Result<(), String> {
    let bad = prefix.split('.').any(|token| {
        token.is_empty() || token.contains(['*', '>']) || token.chars().any(char::is_whitespace)
    });
    if bad {
        return Err(format!(
            "CHELABASE_NATS_INBOX_PREFIX={prefix:?} must be plain subject tokens (no wildcards, blanks or empty tokens)"
        ));
    }
    Ok(())
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

/// The connect options a config asks for: its credentials and its inbox prefix.
pub fn connect_options(config: &NatsConfig) -> async_nats::ConnectOptions {
    let options = match (&config.user, &config.password) {
        (Some(user), Some(password)) => {
            async_nats::ConnectOptions::with_user_and_password(user.clone(), password.clone())
        }
        _ => async_nats::ConnectOptions::new(),
    };
    match &config.inbox_prefix {
        Some(prefix) => options.custom_inbox_prefix(prefix.clone()),
        None => options,
    }
}

pub async fn connect(config: &NatsConfig) -> Result<async_nats::Client, NatsError> {
    Ok(connect_options(config).connect(&config.url).await?)
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
        SUBJECT_STORAGE_SWEEP.to_string(),
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

// --- Storage events: a tenant's shards attached (carried gaps T4, R7) -----------------------

/// The stream of storage events the storage controller's hooks report (only the orchestrator
/// creates and updates it, R10). Never part of [`control_subjects`]: a work queue would hand
/// each event to one consumer, and every waiter must see it.
pub const STREAM_NEON_EVENTS: &str = "CHELABASE_NEON_EVENTS";

/// The subjects' common prefix; one more token names the tenant.
const SUBJECT_TENANT_ATTACHED_PREFIX: &str = "chelabase.neon.attached";

/// One shard of a tenant-attached event: the pageserver it is attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantAttachedShard {
    pub node_id: u64,
    pub shard_number: u8,
}

/// A tenant's shards are attached (the storage controller's `notify-attach` body, by the same
/// names): its pageservers answer for it, so a timeline create there won't 404.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantAttachedEvent {
    pub tenant_id: String,
    #[serde(default)]
    pub shards: Vec<TenantAttachedShard>,
    #[serde(default)]
    pub stripe_size: Option<u32>,
    #[serde(default)]
    pub preferred_az: Option<String>,
}

/// `chelabase.neon.attached.<tenant>`: one token per tenant. Only a Neon id (32 lowercase hex)
/// is accepted, so a value can never widen the subject (`*`, `>`), add a token (`.`) or spell
/// another tenant's subject differently (upper case).
pub fn tenant_attached_subject(tenant: &str) -> Result<String, NatsError> {
    let valid = tenant.len() == 32
        && tenant
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !valid {
        return Err(NatsError::JetStream(format!(
            "invalid tenant id {tenant:?}: expected 32 lowercase hex characters"
        )));
    }
    Ok(format!("{SUBJECT_TENANT_ATTACHED_PREFIX}.{tenant}"))
}

/// The events stream (P8): limits retention (nothing is consumed away; every waiter reads the
/// history), the last event per tenant for an hour, bounded in count and bytes, the oldest
/// dropped first. No `Nats-Msg-Id` dedupe: a re-attach after a restart must land.
pub fn neon_events_stream_config() -> async_nats::jetstream::stream::Config {
    use async_nats::jetstream::stream::{Config, DiscardPolicy, RetentionPolicy, StorageType};
    Config {
        name: STREAM_NEON_EVENTS.to_string(),
        subjects: vec![format!("{SUBJECT_TENANT_ATTACHED_PREFIX}.*")],
        retention: RetentionPolicy::Limits,
        storage: StorageType::File,
        max_messages_per_subject: 1,
        // Events expire after 1 h (P2): a provision retried later than that finds no event, waits
        // its timeout, then proceeds (R8).
        max_age: std::time::Duration::from_secs(3600),
        max_messages: 100_000,
        max_bytes: 64 * 1024 * 1024,
        discard: DiscardPolicy::Old,
        num_replicas: 1,
        ..Default::default()
    }
}

/// Whether an existing stream's config differs from `want` in a setting P8 pins.
fn neon_events_drifted(
    have: &async_nats::jetstream::stream::Config,
    want: &async_nats::jetstream::stream::Config,
) -> bool {
    have.subjects != want.subjects
        || have.retention != want.retention
        || have.storage != want.storage
        || have.max_messages_per_subject != want.max_messages_per_subject
        || have.max_age != want.max_age
        || have.max_messages != want.max_messages
        || have.max_bytes != want.max_bytes
        || have.discard != want.discard
        || have.num_replicas != want.num_replicas
}

/// Creates the events stream, or puts back the P8 settings of one that drifted. The orchestrator
/// calls it at start (after [`ensure_control_stream`]); the control stream is never touched.
pub async fn ensure_neon_events_stream(
    js: &async_nats::jetstream::Context,
) -> Result<(), NatsError> {
    let want = neon_events_stream_config();
    match js.get_stream(STREAM_NEON_EVENTS).await {
        Ok(stream) => {
            let have = &stream.cached_info().config;
            if neon_events_drifted(have, &want) {
                js.update_stream(want)
                    .await
                    .map_err(|e| NatsError::JetStream(e.to_string()))?;
                info!(
                    stream = STREAM_NEON_EVENTS,
                    "jetstream stream config restored"
                );
            } else {
                info!(stream = STREAM_NEON_EVENTS, "jetstream stream exists");
            }
        }
        Err(err)
            if matches!(
                err.kind(),
                async_nats::jetstream::context::GetStreamErrorKind::JetStream(e)
                    if e.error_code() == async_nats::jetstream::ErrorCode::STREAM_NOT_FOUND
            ) =>
        {
            js.create_stream(want)
                .await
                .map_err(|e| NatsError::JetStream(e.to_string()))?;
            info!(stream = STREAM_NEON_EVENTS, "jetstream stream created");
        }
        Err(err) => return Err(NatsError::JetStream(err.to_string())),
    }
    Ok(())
}

/// Publishes `event` on its tenant's subject and waits for JetStream's ack: once this returns
/// `Ok`, the event is stored. No message id is set (see [`neon_events_stream_config`]).
pub async fn publish_tenant_attached(
    js: &async_nats::jetstream::Context,
    event: &TenantAttachedEvent,
) -> Result<(), NatsError> {
    let subject = tenant_attached_subject(&event.tenant_id)?;
    let payload = serde_json::to_vec(event)?;
    js.publish(subject, payload.into())
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;
    Ok(())
}

/// The consumer of one wait (P11): named `attached-<tenant>-<8 random hex>` (one token, so it
/// fits `CONSUMER.CREATE.<stream>.<name>.<filter>`; two waiters never share one), filtered to
/// the tenant's subject, reading its last event (published before the wait or not), no acks,
/// in memory, removed by the server 30 s after its waiter is gone.
pub fn tenant_attached_consumer_config(
    tenant: &str,
) -> Result<async_nats::jetstream::consumer::pull::Config, NatsError> {
    use async_nats::jetstream::consumer::{AckPolicy, DeliverPolicy, pull};
    let subject = tenant_attached_subject(tenant)?;
    let suffix = &Uuid::new_v4().simple().to_string()[..8];
    Ok(pull::Config {
        name: Some(format!("attached-{tenant}-{suffix}")),
        durable_name: None,
        filter_subject: subject,
        deliver_policy: DeliverPolicy::LastPerSubject,
        ack_policy: AckPolicy::None,
        memory_storage: true,
        inactive_threshold: std::time::Duration::from_secs(30),
        ..Default::default()
    })
}

/// The longest one pull waits on the server before the wait asks again.
const ATTACH_FETCH_EXPIRES: std::time::Duration = std::time::Duration::from_secs(5);
/// A pull shorter than this isn't asked for: the deadline is as good as reached.
const ATTACH_FETCH_MIN: std::time::Duration = std::time::Duration::from_millis(20);
/// How long before the wait's deadline each pull expires, so the server's empty-batch answer
/// (sent at the pull's expiry) arrives within the deadline: a pull the deadline cuts off is no
/// answer from NATS (rule 4), so a pull that ran to the deadline would turn a plain "no event"
/// into an `Err`. A short wait keeps half of what is left instead ([`attach_pull_expires`]).
const ATTACH_PULL_MARGIN: std::time::Duration = std::time::Duration::from_millis(250);
/// The pause after a failed NATS call before the next try (within the deadline).
const ATTACH_RETRY_PAUSE: std::time::Duration = std::time::Duration::from_millis(250);
/// How long the consumer's delete may take once the wait is over (the deadline may be spent by
/// then; the consumer's `inactive_threshold` removes one whose delete failed).
const ATTACH_DELETE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// How many failed NATS calls in a row (creates and pulls, no pull answering in between) make a
/// wait that has seen a pull answer a failure rather than a timeout.
const ATTACH_FAILURE_LIMIT: u32 = 3;
/// How many times a create refused for a reason that won't change is tried before the wait ends.
const ATTACH_REFUSAL_LIMIT: u32 = 2;

/// Whether a consumer create's refusal will not change within the wait: the stream doesn't exist
/// (`STREAM_NOT_FOUND`) or JetStream isn't enabled (for the server or the account). A permissions
/// violation can't be told apart: the server drops the request without an answer, so it surfaces
/// as a timeout like a slow server, and is retried until the deadline.
fn consumer_create_refusal_is_permanent(
    kind: &async_nats::jetstream::stream::ConsumerErrorKind,
) -> bool {
    use async_nats::jetstream::ErrorCode;
    use async_nats::jetstream::stream::ConsumerErrorKind;
    matches!(
        kind,
        ConsumerErrorKind::JetStream(e)
            if matches!(
                e.error_code(),
                ErrorCode::STREAM_NOT_FOUND
                    | ErrorCode::JETSTREAM_NOT_ENABLED
                    | ErrorCode::JETSTREAM_NOT_ENABLED_FOR_ACCOUNT
            )
    )
}

/// Whether a wait that found no event is a failure (`Err`) rather than a timeout (`Ok(None)`).
/// A timeout needs proof that NATS answered the question "is there an event?": some pull has
/// answered (an event or an empty batch; a successful create alone proves nothing), and the
/// latest `ATTACH_FAILURE_LIMIT` calls have not all failed since the last answered pull
/// (`trailing_failures` counts failed creates and pulls since then).
fn attach_wait_failed(pull_answered: bool, trailing_failures: u32) -> bool {
    !pull_answered || trailing_failures >= ATTACH_FAILURE_LIMIT
}

/// The `expires` of a pull asked with `left` before the wait's deadline: at most
/// [`ATTACH_FETCH_EXPIRES`], ending [`ATTACH_PULL_MARGIN`] (or half of `left`, if less) before the
/// deadline so the server answers it in time; `None` when that is shorter than
/// [`ATTACH_FETCH_MIN`] (no pull is asked).
fn attach_pull_expires(left: std::time::Duration) -> Option<std::time::Duration> {
    let expires = ATTACH_FETCH_EXPIRES.min(left - ATTACH_PULL_MARGIN.min(left / 2));
    (expires >= ATTACH_FETCH_MIN).then_some(expires)
}

/// Waits up to `timeout` for `tenant`'s attach event (P11): `Some` as soon as one is stored
/// (one published before the wait included), `None` at the deadline. A NATS failure is retried
/// within the deadline; `Err` per [`attach_wait_failed`] (NATS down, pulls failing, no stream:
/// the caller says why it didn't wait), and early, after `ATTACH_REFUSAL_LIMIT` tries, when the
/// consumer create is refused for a reason that won't change
/// ([`consumer_create_refusal_is_permanent`]). Every NATS call is bounded by what is left of the
/// deadline; the wait's consumer is deleted before it returns (also when its create timed out,
/// which may still have been applied). It never looks the stream up (`STREAM.INFO`): the worker
/// may only create, pull from and delete its own consumers.
pub async fn wait_tenant_attached(
    js: &async_nats::jetstream::Context,
    tenant: &str,
    timeout: std::time::Duration,
) -> Result<Option<TenantAttachedEvent>, NatsError> {
    use async_nats::jetstream::consumer::PullConsumer;
    use tokio::time::{Instant, timeout_at};

    let config = tenant_attached_consumer_config(tenant)?;
    let name = config.name.clone().unwrap_or_default();
    let deadline = Instant::now() + timeout;
    // Whether a pull answered, the failed calls since the last answered pull, the refusals that
    // won't change, and the last failure.
    let mut pull_answered = false;
    let mut trailing_failures: u32 = 0;
    let mut refusals: u32 = 0;
    let mut gave_up = false;
    let mut last_error: Option<String> = None;
    let mut consumer: Option<PullConsumer> = None;
    let mut created = false;

    let found = loop {
        let now = Instant::now();
        if deadline.saturating_duration_since(now) < ATTACH_FETCH_MIN {
            break None;
        }
        if consumer.is_none() {
            match timeout_at(
                deadline,
                js.create_consumer_on_stream(config.clone(), STREAM_NEON_EVENTS),
            )
            .await
            {
                Ok(Ok(c)) => {
                    created = true;
                    consumer = Some(c);
                }
                Ok(Err(e)) => {
                    last_error = Some(format!("creating the wait's consumer: {e}"));
                    trailing_failures += 1;
                    if consumer_create_refusal_is_permanent(&e.kind()) {
                        refusals += 1;
                        if refusals >= ATTACH_REFUSAL_LIMIT {
                            gave_up = true;
                            break None;
                        }
                    }
                    pause_until(deadline).await;
                    continue;
                }
                Err(_) => {
                    // The create may have been applied after the deadline cut it off.
                    created = true;
                    break None;
                }
            }
        }
        let Some(c) = consumer.as_ref() else {
            continue;
        };
        let Some(expires) = attach_pull_expires(deadline.saturating_duration_since(Instant::now()))
        else {
            break None;
        };
        let pulled = timeout_at(deadline, async {
            let mut batch = c
                .batch()
                .max_messages(1)
                .expires(expires)
                .messages()
                .await
                .map_err(|e| format!("pulling: {e}"))?;
            match batch.next().await {
                Some(Ok(message)) => Ok(Some(message)),
                Some(Err(e)) => Err(format!("pulling: {e}")),
                None => Ok(None),
            }
        })
        .await;
        match pulled {
            Ok(Ok(Some(message))) => {
                pull_answered = true;
                trailing_failures = 0;
                match serde_json::from_slice::<TenantAttachedEvent>(&message.payload) {
                    Ok(event) if event.tenant_id == tenant => break Some(event),
                    Ok(event) => warn!(
                        tenant,
                        other = %event.tenant_id,
                        "an attach event for another tenant on this tenant's subject; ignored"
                    ),
                    Err(e) => warn!(tenant, "an unreadable attach event; ignored: {e}"),
                }
                // Not the event wanted: pause, then ask again (the create is by name, so it is
                // the same consumer, which still delivers a later event).
                consumer = None;
                pause_until(deadline).await;
            }
            Ok(Ok(None)) => {
                // An empty batch; a server that ends batches at once must not be spun on.
                pull_answered = true;
                trailing_failures = 0;
                pause_until(deadline).await;
            }
            Ok(Err(e)) => {
                trailing_failures += 1;
                // The consumer may be gone (deleted, or removed while NATS was away): a fresh
                // one is created on the next turn (create is create-or-update by name).
                last_error = Some(e);
                consumer = None;
                pause_until(deadline).await;
            }
            // Cut off by the deadline although it expired before it: NATS didn't answer in time.
            Err(_) => break None,
        }
    };

    if created {
        let deleted = tokio::time::timeout(
            ATTACH_DELETE_TIMEOUT,
            js.delete_consumer_from_stream(&name, STREAM_NEON_EVENTS),
        )
        .await;
        match deleted {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => warn!(consumer = %name, "deleting the attach wait's consumer: {e}"),
            Err(_) => warn!(consumer = %name, "deleting the attach wait's consumer timed out"),
        }
    }

    match found {
        Some(event) => Ok(Some(event)),
        None if !gave_up && !attach_wait_failed(pull_answered, trailing_failures) => Ok(None),
        None => Err(NatsError::JetStream(last_error.unwrap_or_else(|| {
            format!("no answer from NATS within {timeout:?}")
        }))),
    }
}

/// Sleeps [`ATTACH_RETRY_PAUSE`], or until `deadline` if that comes first.
async fn pause_until(deadline: tokio::time::Instant) {
    let until = deadline.min(tokio::time::Instant::now() + ATTACH_RETRY_PAUSE);
    tokio::time::sleep_until(until).await;
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

/// Queue a storage sweep of `job`'s database; one already queued inside the stream's duplicate
/// window ([`storage_sweep_msg_id`]) makes this a no-op.
pub async fn publish_storage_sweep(
    js: &async_nats::jetstream::Context,
    job: &StorageSweepJob,
    tag: &str,
) -> Result<(), NatsError> {
    let envelope = JobEnvelope {
        job_id: Uuid::new_v4(),
        payload: job.clone(),
    };
    let payload = serde_json::to_vec(&envelope)?;
    let mut headers = async_nats::HeaderMap::new();
    headers.insert("Nats-Msg-Id", storage_sweep_msg_id(job.database_id, tag).as_str());
    js.publish_with_headers(SUBJECT_STORAGE_SWEEP.to_string(), headers, payload.into())
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;
    Ok(())
}

/// Run the storage sweep consumer loop, calling `handler` for each job (one at a time) with
/// its delivery attempt (1-based), so the handler can give up on a job that keeps failing.
pub async fn run_storage_sweep_consumer<F, Fut>(
    js: async_nats::jetstream::Context,
    handler: F,
) -> Result<(), NatsError>
where
    F: FnMut(JobEnvelope<StorageSweepJob>, u64) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    consume(
        js,
        ConsumerSpec::control(CONSUMER_STORAGE_SWEEP, SUBJECT_STORAGE_SWEEP),
        handler,
    )
    .await
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

/// How long a job may go silent before NATS redelivers it. It is not a job's total time: while
/// a handler runs, the runner sends an in-progress ack every `ack_wait / 3`, which restarts the
/// timer, so only a job that stops acking (a worker that died, a runner that hung) is
/// redelivered after this long.
///
/// For scale, a single provision can run long: the tenant create about 46 s (its 45 s budget
/// plus at most one 250 ms attempt), the wait for the tenant's attach event up to about 61 s
/// (its 60 s timeout plus the 1 s consumer delete; R8 then creates the timeline anyway), the
/// timeline create up to about 121 s (its own 120 s budget), the 60 s readiness wait, up to
/// 30 s of admin SQL, and a teardown of about 70 s. The in-progress acks keep such a job alive
/// through all of it.
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

    // T4 coder: the drift check sees a change in each setting P8 pins, and none in the
    // config itself (an existing stream with it is left alone).
    #[test]
    fn the_events_stream_drift_check_covers_every_pinned_setting() {
        use async_nats::jetstream::stream::{DiscardPolicy, RetentionPolicy, StorageType};
        let want = neon_events_stream_config();
        assert!(!neon_events_drifted(&want, &want));
        // Server-filled settings P8 doesn't pin are no drift.
        let mut filled = want.clone();
        filled.duplicate_window = std::time::Duration::from_secs(120);
        assert!(!neon_events_drifted(&filled, &want));
        let changes: Vec<fn(&mut async_nats::jetstream::stream::Config)> = vec![
            |c| c.subjects = vec!["chelabase.neon.attached.>".into()],
            |c| c.retention = RetentionPolicy::Interest,
            |c| c.storage = StorageType::Memory,
            |c| c.max_messages_per_subject = 5,
            |c| c.max_age = std::time::Duration::from_secs(600),
            |c| c.max_messages = 10,
            |c| c.max_bytes = 1024,
            |c| c.discard = DiscardPolicy::New,
            |c| c.num_replicas = 3,
        ];
        for (i, change) in changes.into_iter().enumerate() {
            let mut have = want.clone();
            change(&mut have);
            assert!(neon_events_drifted(&have, &want), "change {i} not seen");
        }
    }

    // T4 coder: an event for an invalid tenant is refused before NATS is asked (nothing
    // listens there, so a publish would hang until JetStream's timeout).
    #[tokio::test]
    async fn an_event_for_an_invalid_tenant_is_refused_before_publishing() {
        let client = async_nats::ConnectOptions::new()
            .retry_on_initial_connect()
            .connect("nats://127.0.0.1:1")
            .await
            .unwrap();
        let js = async_nats::jetstream::new(client);
        let started = std::time::Instant::now();
        for tenant in ["", "*", ">", "0123456789ABCDEF0123456789abcdef", "a.b"] {
            let event = TenantAttachedEvent {
                tenant_id: tenant.into(),
                shards: vec![],
                stripe_size: None,
                preferred_az: None,
            };
            assert!(
                publish_tenant_attached(&js, &event).await.is_err(),
                "{tenant:?}"
            );
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

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
            reassigned: None,
            preview: None,
        };
        let back: RoleReply = serde_json::from_slice(&serde_json::to_vec(&in_use).unwrap()).unwrap();
        assert_eq!(back, in_use);
    }

    // T10: a delete names its successor; one without (an older orchestrator) means the
    // quarantine role, never the owner.
    #[test]
    fn a_delete_without_a_successor_means_the_quarantine_role() {
        let branch = BranchId::new();
        let old = serde_json::json!({ "branch_id": branch, "name": "app" });
        let request: RoleDeleteRequest = serde_json::from_value(old).unwrap();
        assert_eq!(request.successor, "chelabase_orphaned");
        let named = RoleDeleteRequest { branch_id: branch, name: "app".into(), successor: "heir".into(), confirm: None };
        let json = serde_json::to_value(&named).unwrap();
        assert_eq!(json["successor"], "heir");
        let back: RoleDeleteRequest = serde_json::from_value(json).unwrap();
        assert_eq!(back, named);
    }

    // T10: a done delete carries what moved; other replies (and older workers) carry nothing.
    #[test]
    fn a_delete_reply_carries_what_was_reassigned() {
        let done = RoleReply {
            ok: true,
            code: None,
            error: None,
            keys: vec![],
            reassigned: Some(Reassigned { count: 3, definers: vec!["public.f(integer)".into()], confirmed: vec![] }),
            preview: None,
        };
        let json = serde_json::to_value(&done).unwrap();
        assert_eq!(json["reassigned"], serde_json::json!({ "count": 3, "definers": ["public.f(integer)"] }));
        let back: RoleReply = serde_json::from_value(json).unwrap();
        assert_eq!(back, done);
        let plain = RoleReply { reassigned: None, ..done };
        assert!(serde_json::to_value(&plain).unwrap().get("reassigned").is_none());
        let old: RoleReply = serde_json::from_str(r#"{"ok":true,"code":null,"error":null}"#).unwrap();
        assert_eq!(old.reassigned, None);
    }

    // --- T10 fix: preview, then confirm --------------------------------------------------

    fn exposure(kind: ExposureKind, identity: &str, grantees: &[&str]) -> Exposure {
        Exposure {
            database: "app".into(),
            kind,
            identity: identity.into(),
            grantees: grantees.iter().map(|g| g.to_string()).collect(),
        }
    }

    #[test]
    fn the_preview_subject_is_its_own() {
        assert_eq!(SUBJECT_ROLE_DELETE_PREVIEW, "chelabase.rpc.roles.delete_preview");
    }

    // The wire shapes: kinds in snake_case, `confirm` on a request and `preview` on a reply
    // absent when unset (and an older peer's JSON without them parses).
    #[test]
    fn exposure_wire_shapes() {
        let kinds = [
            (ExposureKind::Function, "function"),
            (ExposureKind::Procedure, "procedure"),
            (ExposureKind::View, "view"),
            (ExposureKind::MaterializedView, "materialized_view"),
        ];
        for (kind, name) in kinds {
            assert_eq!(serde_json::to_value(kind).unwrap(), name);
            let back: ExposureKind = serde_json::from_value(serde_json::json!(name)).unwrap();
            assert_eq!(back, kind);
        }
        let e = exposure(ExposureKind::View, "public.v", &["PUBLIC"]);
        assert_eq!(
            serde_json::to_value(&e).unwrap(),
            serde_json::json!({ "database": "app", "kind": "view", "identity": "public.v", "grantees": ["PUBLIC"] })
        );

        let branch = BranchId::new();
        let bare = RoleDeleteRequest { branch_id: branch, name: "app".into(), successor: "heir".into(), confirm: None };
        assert!(serde_json::to_value(&bare).unwrap().get("confirm").is_none());
        let confirmed = RoleDeleteRequest { confirm: Some("ab".repeat(32)), ..bare.clone() };
        let json = serde_json::to_value(&confirmed).unwrap();
        assert_eq!(json["confirm"], "ab".repeat(32));
        assert_eq!(serde_json::from_value::<RoleDeleteRequest>(json).unwrap(), confirmed);

        let plain = RoleReply { ok: true, code: None, error: None, keys: vec![], reassigned: None, preview: None };
        assert!(serde_json::to_value(&plain).unwrap().get("preview").is_none());
        let preview = DeletePreview {
            successor: "heir".into(),
            exposures: vec![e.clone()],
            confirm: Some("cd".repeat(32)),
        };
        let refused = RoleReply {
            ok: false,
            code: Some("exposure_unconfirmed".into()),
            error: Some("confirm".into()),
            keys: vec![],
            reassigned: None,
            preview: Some(preview.clone()),
        };
        let json = serde_json::to_value(&refused).unwrap();
        assert_eq!(json["preview"]["confirm"], "cd".repeat(32));
        assert_eq!(json["preview"]["exposures"][0]["kind"], "view");
        assert_eq!(serde_json::from_value::<RoleReply>(json).unwrap(), refused);

        // A done delete lists what it kept on confirmation; none is absent, and an older
        // worker's `reassigned` (no `confirmed`) parses.
        let none = Reassigned { count: 1, definers: vec![], confirmed: vec![] };
        assert_eq!(serde_json::to_value(&none).unwrap(), serde_json::json!({ "count": 1, "definers": [] }));
        let kept = Reassigned { confirmed: vec![e], ..none.clone() };
        let json = serde_json::to_value(&kept).unwrap();
        assert_eq!(json["confirmed"][0]["identity"], "public.v");
        assert_eq!(serde_json::from_value::<Reassigned>(json).unwrap(), kept);
        let old: Reassigned = serde_json::from_str(r#"{"count":2,"definers":["public.f()"]}"#).unwrap();
        assert!(old.confirmed.is_empty());
    }

    // The derived order the worker sorts by: database, kind, identity, grantees.
    #[test]
    fn exposures_sort_by_database_kind_identity() {
        let mut list = [
            Exposure { database: "b".into(), ..exposure(ExposureKind::Function, "public.a()", &["anon"]) },
            exposure(ExposureKind::View, "public.a", &["anon"]),
            exposure(ExposureKind::Function, "public.z()", &["anon"]),
            exposure(ExposureKind::MaterializedView, "public.a", &["anon"]),
            exposure(ExposureKind::Procedure, "public.a()", &["anon"]),
        ];
        list.sort();
        let order: Vec<(&str, ExposureKind, &str)> =
            list.iter().map(|e| (e.database.as_str(), e.kind, e.identity.as_str())).collect();
        assert_eq!(
            order,
            [
                ("app", ExposureKind::Function, "public.z()"),
                ("app", ExposureKind::Procedure, "public.a()"),
                ("app", ExposureKind::View, "public.a"),
                ("app", ExposureKind::MaterializedView, "public.a"),
                ("b", ExposureKind::Function, "public.a()"),
            ]
        );
    }

    // The token: hex SHA-256 of the canonical JSON (keys sorted at every level, no whitespace),
    // pinned against hashes computed outside Rust (`sha256sum` of the literal texts below).
    #[test]
    fn the_confirm_token_is_the_canonical_json_hash() {
        // {"exposures":[],"successor":"heir"}
        assert_eq!(
            exposure_token("heir", &[]),
            "888703465941594e4bc108b738214543ab838cc0ab2dfede7e90ca5f44df21ae"
        );
        // {"exposures":[{"database":"app","grantees":["anon"],"identity":"public.planted()","kind":"function"},
        //  {"database":"app","grantees":["PUBLIC","web"],"identity":"public.\"Odd \"\"v\"\"\"","kind":"materialized_view"}],
        //  "successor":"heir"} (one line)
        let list = [
            exposure(ExposureKind::Function, "public.planted()", &["anon"]),
            exposure(ExposureKind::MaterializedView, "public.\"Odd \"\"v\"\"\"", &["PUBLIC", "web"]),
        ];
        let token = exposure_token("heir", &list);
        assert_eq!(token, "46384bb8aaf0639d087b2d521c15896ffffd02807c75efd9021867bc69ac5b55");
        // Anything that changes the exposure changes the token: a grantee, the successor, the
        // database, the kind.
        let mut more = list.to_vec();
        more[0].grantees.push("authenticated".into());
        assert_ne!(exposure_token("heir", &more), token);
        assert_ne!(exposure_token("heir2", &list), token);
        let mut moved = list.to_vec();
        moved[1].database = "other".into();
        assert_ne!(exposure_token("heir", &moved), token);
        let mut kind = list.to_vec();
        kind[1].kind = ExposureKind::View;
        assert_ne!(exposure_token("heir", &kind), token);
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

    // EOS-H1 (fail closed, via the worker): the all-rows check's subject and wire shapes. A
    // key create's request carries no `change`; a reply is tagged by `status`, and one the
    // orchestrator can't parse is a transport failure there (refused, never allowed).
    #[test]
    fn all_rows_check_wire_shapes() {
        assert_eq!(SUBJECT_ALL_ROWS_CHECK, "chelabase.rpc.roles.all_rows_check");
        let branch = BranchId::new();
        let key = AllRowsCheckRequest { branch_id: branch, starting: vec!["k".into()], change: None };
        let json = serde_json::to_value(&key).unwrap();
        assert_eq!(json, serde_json::json!({ "branch_id": branch, "starting": ["k"] }));
        let parsed: AllRowsCheckRequest = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, key);
        let change = AllRowsCheckRequest {
            branch_id: branch,
            starting: vec!["anon".into(), "authenticated".into(), "authenticator".into(), "k".into()],
            change: Some(MembershipChange { role: "m".into(), member_of: vec![] }),
        };
        let json = serde_json::to_value(&change).unwrap();
        assert_eq!(json["change"], serde_json::json!({ "role": "m", "member_of": [] }));
        assert_eq!(serde_json::from_value::<AllRowsCheckRequest>(json).unwrap(), change);

        let replies = [
            (AllRowsCheckReply::Clear, serde_json::json!({ "status": "clear" })),
            (
                AllRowsCheckReply::Exposed { chain: vec!["k".into(), "m".into(), "r".into()] },
                serde_json::json!({ "status": "exposed", "chain": ["k", "m", "r"] }),
            ),
            (
                AllRowsCheckReply::Failed { message: "compute unreachable".into() },
                serde_json::json!({ "status": "failed", "message": "compute unreachable" }),
            ),
        ];
        for (reply, wire) in replies {
            assert_eq!(serde_json::to_value(&reply).unwrap(), wire);
            assert_eq!(serde_json::from_value::<AllRowsCheckReply>(wire).unwrap(), reply);
        }
        for bad in [
            serde_json::json!({}),
            serde_json::json!({ "status": "allowed" }),
            serde_json::json!({ "status": "exposed" }),
            serde_json::json!({ "ok": true }),
        ] {
            assert!(serde_json::from_value::<AllRowsCheckReply>(bad.clone()).is_err(), "{bad}");
        }
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
                SUBJECT_STORAGE_SWEEP.to_string(),
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
                SUBJECT_STORAGE_SWEEP.to_string(),
            ]
        );
    }

    // object-storage m8: one token under `chelabase.` (the services' `chelabase.*` publish
    // right), on the control stream; one message id per database, so a burst of events for
    // one database is one job.
    #[test]
    fn storage_sweep_jobs_are_control_jobs_deduplicated_per_database() {
        assert_eq!(SUBJECT_STORAGE_SWEEP, "chelabase.storage_sweep");
        assert_eq!(SUBJECT_STORAGE_SWEEP.matches('.').count(), 1);
        assert_eq!(CONSUMER_STORAGE_SWEEP, "chelabase-worker-storage-sweep");
        assert!(control_subjects().contains(&SUBJECT_STORAGE_SWEEP.to_string()));
        let (a, b) = (DatabaseId::new(), DatabaseId::new());
        assert_eq!(storage_sweep_msg_id(a, "7"), storage_sweep_msg_id(a, "7"));
        assert_ne!(storage_sweep_msg_id(a, "7"), storage_sweep_msg_id(b, "7"));
        // Audit B2: a period's, an event's and a purge's ids never collide.
        assert_ne!(storage_sweep_msg_id(a, "7"), storage_sweep_msg_id(a, "8"));
        assert_ne!(storage_sweep_msg_id(a, "7"), storage_sweep_msg_id(a, "purge"));
        assert_eq!(storage_sweep_msg_id(a, "purge"), format!("storage-sweep-{a}-purge"));
        let job = StorageSweepJob { database_id: a };
        let json = serde_json::to_value(&job).unwrap();
        assert_eq!(json, serde_json::json!({ "database_id": a }));
        assert_eq!(serde_json::from_value::<StorageSweepJob>(json).unwrap(), job);
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
        let request = MemberRolesReconcileRequest { branch_id, member_id: Some(member_id), from_ddl: false };
        let back: MemberRolesReconcileRequest =
            serde_json::from_slice(&serde_json::to_vec(&request).unwrap()).unwrap();
        assert_eq!((back.branch_id, back.member_id), (branch_id, Some(member_id)));
        let whole: MemberRolesReconcileRequest =
            serde_json::from_value(serde_json::json!({ "branch_id": branch_id, "member_id": null }))
                .unwrap();
        assert_eq!(whole.member_id, None);
        // T9 I2: `from_ddl` marks a run the DDL receiver asked for (the RT3-11 retry applies to
        // it only). An old publisher, which never sends it, means false.
        assert!(!whole.from_ddl, "absent means false");
        let from_ddl: MemberRolesReconcileRequest = serde_json::from_value(
            serde_json::json!({ "branch_id": branch_id, "member_id": null, "from_ddl": true }),
        )
        .unwrap();
        assert!(from_ddl.from_ddl);
        let wire = serde_json::to_value(&MemberRolesReconcileRequest {
            branch_id,
            member_id: None,
            from_ddl: true,
        })
        .unwrap();
        assert_eq!(wire["from_ddl"], serde_json::json!(true), "the field's name on the wire");
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
