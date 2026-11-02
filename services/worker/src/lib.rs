//! Chelabase worker: provisioning, deletion, credential resets and reconcile.
//!
//! [`Worker`] runs the lifecycle over the ports in [`ports`], so it can be tested with
//! in-memory fakes; `main.rs` wires it to the real metadata database, storage controller,
//! Docker, Valkey and compute_ctl.

use std::sync::Mutex;
use std::time::Duration;

use chelabase_secrets::SecretKey;
use chelabase_types::BranchId;
use chelabase_types::role_settings::RoleSettings;

pub mod all_rows;
pub mod app_users;
pub mod app_users_sql;
pub mod branch_create;
pub mod branch_delete;
pub mod branch_job;
pub mod branch_switch;
#[cfg(test)]
mod c2_tests;
#[cfg(test)]
mod c3_tests;
pub mod certs;
pub mod config;
pub mod credentials;
pub mod data_api;
pub mod delete;
#[cfg(test)]
mod f2_coder_tests;
#[cfg(test)]
mod f2_tests;
#[cfg(test)]
mod f4_coder_tests;
#[cfg(test)]
mod f4_tests;
#[cfg(test)]
mod fakes;
#[cfg(test)]
mod final_a_tests;
pub mod identity_sweep;
pub mod member_roles;
pub mod schema_guard;
pub mod merge;
pub mod names;
pub mod personal_logins;
pub mod point;
pub mod project_delete;
pub mod ports;
pub mod provision;
pub mod realtime;
pub mod reconcile;
pub mod reset;
pub mod resync;
pub mod review;
pub mod role_sessions;
pub mod roles;
#[cfg(test)]
mod rt1_tests;
pub mod rt_events;
pub mod safekeepers;
pub mod schema_base;
pub mod settings;
pub mod storage_schema;
pub mod storage_sweep;
#[cfg(test)]
mod storage_sweep_tests;
#[cfg(test)]
mod t4_tests;
pub mod token_refresh;

/// The role that owns every database, and whose password is revealed.
pub const OWNER_ROLE: &str = "postgres";

/// The compute settings this worker creates computes with (R11). Reconcile re-creates a ready
/// branch's compute whose endpoint records a lower version.
/// - 1: `neon.console_url` (DDL forwarding) and `createrole_self_grant`.
/// - 2: `neon.protected_databases` (the branch's main database can't be dropped).
/// - 3: TLS (a server certificate uploaded before start), the control network and the branch
///   protection flag (`neon.branch_protected`).
/// - 4: realtime (RT1 P14): `max_replication_slots`/`max_wal_senders` 20 and
///   `max_slot_wal_keep_size` 1 GB.
pub const COMPUTE_SETTINGS_VERSION: i32 = 4;

/// Everything the lifecycle needs.
pub struct Worker<M, S, C, N, P> {
    pub metadata: M,
    pub storage: S,
    pub computes: C,
    pub cache: N,
    pub pusher: P,
    pub key: SecretKey,
    /// `CHELABASE_DB_OWNER_SUPERUSER` in self-hosted mode: the owner role becomes SUPERUSER.
    pub owner_superuser: bool,
    /// `CHELABASE_BRANCH_ROLE_CAP` and the other login-role settings.
    pub roles: RoleSettings,
    /// `CHELABASE_CONTROL_PLANE_URI`: where computes forward role and database DDL.
    pub control_plane_uri: String,
    /// How often provisioning checks whether a new compute answers (500 ms in production).
    pub ready_poll: Duration,
    /// How long provisioning waits for a new compute (60 s in production).
    pub ready_timeout: Duration,
    /// How far the wait stretches while compute_ctl reports a live state (5 minutes in
    /// production).
    pub ready_cap: Duration,
    /// How long a provision waits for its new tenant's attach event before creating the
    /// timeline anyway (60 s in production; carried gaps T4, R8).
    pub attach_timeout: Duration,
    /// How often a long reset or restore step refreshes the branch's claim (60 s in
    /// production), well inside reconcile's stale threshold.
    pub claim_refresh: Duration,
    /// The last branch reconcile's `chelabase` schema check looked at: each pass checks the next
    /// few ready branches after it, wrapping around.
    pub schema_check_cursor: Mutex<Option<BranchId>>,
    /// When reconcile first saw each `enabling` database row: one still there after
    /// [`reconcile::ENABLING_RETRY_SECS`] has its enable bootstrap run again.
    pub enabling_seen: Mutex<std::collections::HashMap<(BranchId, String), std::time::Instant>>,
    /// Endpoints whose last recreate left a compute that never got ready (reconcile skips them
    /// until their backoff ends).
    pub recreate_backoff: Mutex<std::collections::HashMap<String, reconcile::RecreateBackoff>>,
    /// Zitadel with the admin PAT (`CHELABASE_ZITADEL_ADMIN_PAT_FILE`, `CHELABASE_AUTH_HOST`):
    /// org create/delete, user reads and session deletes. `None` when either is unset; app
    /// users jobs that need it then fail and are retried (P2).
    pub zitadel: Option<chelabase_zitadel::client::Zitadel>,
    /// How often every enabled project's users are reconciled (`CHELABASE_APP_USERS_RECONCILE_SECS`).
    pub app_users_reconcile_every: Duration,
    /// How often the member-role sweep runs (`CHELABASE_MEMBER_ROLES_SWEEP_SECS`).
    pub member_roles_sweep_every: Duration,
    /// How often the identity sweep runs (`CHELABASE_IDENTITY_SWEEP_SECS`).
    pub identity_sweep_every: Duration,
    /// EOS-4: the identity sweep said once that it can't run without a Zitadel client.
    pub identity_sweep_warned: std::sync::atomic::AtomicBool,
    /// `CHELABASE_WORKER_REMOVE_ORPHANS`: whether reconcile removes compute containers that no
    /// endpoint row records (off: they're logged and kept).
    pub remove_orphans: bool,
    /// How long a merge's phase 1 transaction (and each phase 1a step) may take before its
    /// backend is cancelled (P3: 16 min, above the script's own 15 min `statement_timeout`).
    pub merge_tx_timeout: Duration,
    /// How long one phase 2 step (a concurrent index build, a validation) may take before its
    /// backend is cancelled (P3: 6 h).
    pub merge_online_step_timeout: Duration,
    /// Branch resync requests (`chelabase.resync.branch`), connected by the process.
    pub resync_requests: resync::ResyncRequests,
    /// Member-role reconcile requests (`chelabase.member_roles.reconcile`), connected by the
    /// process.
    pub member_role_requests: member_roles::MemberRoleRequests,
    /// Who may log in as a member role, pushed with every spec: `CHELABASE_MEMBER_LOGIN_CIDRS`
    /// and the orchestrator's certificate CN.
    pub member_logins: chelabase_neon::spec::MemberLogins,
    /// What the daily certificate sweep needs; `None` where there is no CA (the fakes).
    pub cert_sweep: Option<certs::CertSweep>,
    /// Who may log in as `chelabase_realtime`, pushed with every spec:
    /// `CHELABASE_REALTIME_LOGIN_CIDRS` and realtime's certificate CN.
    pub realtime_logins: chelabase_neon::spec::MemberLogins,
    /// How often the storage-token refresh pass runs (`CHELABASE_STORAGE_TOKEN_REFRESH_SECS`).
    pub storage_token_refresh_every: Duration,
    /// Consecutive refresh failures per endpoint (reset by a success), for the log and tests.
    pub storage_token_failures: Mutex<std::collections::HashMap<String, u32>>,
    /// The `chelabase_realtime` role's password (`CHELABASE_REALTIME_PG_PASSWORD_FILE`, RT1
    /// T5). `None`: the role is neither created nor granted anything (the fakes by default).
    pub realtime_password: Option<String>,
    /// The object store user blobs live in, for the storage sweep (`CHELABASE_STORAGE_*`);
    /// `None`: no sweep runs.
    pub blob_store: Option<storage_sweep::BlobStore>,
    /// The storage sweep's grace period, period and dry run.
    pub storage_sweep: storage_sweep::SweepSettings,
    /// Storage sweep jobs (`chelabase.storage_sweep`), connected by the process.
    pub sweep_requests: storage_sweep::SweepRequests,
    /// Storage sweep passes a branch's status refused (stuck branches mean storage only grows).
    pub storage_sweep_refusals: std::sync::atomic::AtomicU64,
}
