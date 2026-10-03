//! Cheladb worker: provisioning, deletion, credential resets and reconcile.
//!
//! [`Worker`] runs the lifecycle over the ports in [`ports`], so it can be tested with
//! in-memory fakes; `main.rs` wires it to the real metadata database, storage controller,
//! Docker, Valkey and compute_ctl.

use std::sync::Mutex;
use std::time::Duration;

use cheladb_secrets::SecretKey;
use cheladb_types::BranchId;
use cheladb_types::role_settings::RoleSettings;

pub mod app_users;
pub mod app_users_sql;
pub mod branch_create;
pub mod branch_delete;
pub mod branch_job;
pub mod branch_switch;
pub mod config;
pub mod credentials;
pub mod data_api;
pub mod delete;
#[cfg(test)]
mod fakes;
pub mod names;
pub mod point;
pub mod project_delete;
pub mod ports;
pub mod provision;
pub mod reconcile;
pub mod reset;
pub mod resync;
pub mod roles;
pub mod settings;

/// The role that owns every database, and whose password is revealed.
pub const OWNER_ROLE: &str = "postgres";

/// The compute settings this worker creates computes with (R11). Reconcile re-creates a ready
/// branch's compute whose endpoint records a lower version.
/// - 1: `neon.console_url` (DDL forwarding) and `createrole_self_grant`.
/// - 2: `neon.protected_databases` (the branch's main database can't be dropped).
pub const COMPUTE_SETTINGS_VERSION: i32 = 2;

/// Everything the lifecycle needs.
pub struct Worker<M, S, C, N, P> {
    pub metadata: M,
    pub storage: S,
    pub computes: C,
    pub cache: N,
    pub pusher: P,
    pub key: SecretKey,
    /// `CHELADB_DB_OWNER_SUPERUSER` in self-hosted mode: the owner role becomes SUPERUSER.
    pub owner_superuser: bool,
    /// `CHELADB_BRANCH_ROLE_CAP` and the other login-role settings.
    pub roles: RoleSettings,
    /// `CHELADB_CONTROL_PLANE_URI`: where computes forward role and database DDL.
    pub control_plane_uri: String,
    /// How often provisioning checks whether a new compute answers (500 ms in production).
    pub ready_poll: Duration,
    /// How long provisioning waits for a new compute (60 s in production).
    pub ready_timeout: Duration,
    /// How often a long reset or restore step refreshes the branch's claim (60 s in
    /// production), well inside reconcile's stale threshold.
    pub claim_refresh: Duration,
    /// The last branch reconcile's `cheladb` schema check looked at: each pass checks the next
    /// few ready branches after it, wrapping around.
    pub schema_check_cursor: Mutex<Option<BranchId>>,
    /// When reconcile first saw each `enabling` database row: one still there after
    /// [`reconcile::ENABLING_RETRY_SECS`] has its enable bootstrap run again.
    pub enabling_seen: Mutex<std::collections::HashMap<(BranchId, String), std::time::Instant>>,
    /// Zitadel with the admin PAT (`CHELADB_ZITADEL_ADMIN_PAT_FILE`, `CHELADB_ZITADEL_ISSUER`):
    /// org create/delete, user reads and session deletes. `None` when either is unset; app
    /// users jobs that need it then fail and are retried (P2).
    pub zitadel: Option<cheladb_zitadel::client::Zitadel>,
    /// How often every enabled project's users are reconciled (`CHELADB_APP_USERS_RECONCILE_SECS`).
    pub app_users_reconcile_every: Duration,
    /// `CHELADB_WORKER_REMOVE_ORPHANS`: whether reconcile removes compute containers that no
    /// endpoint row records (off: they're logged and kept).
    pub remove_orphans: bool,
}
