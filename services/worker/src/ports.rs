//! What the worker needs from the outside world, as traits the tests fake.

use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use cheladb_metadata::{
    BranchConnectionRow, BranchDatabase, BranchDeleteOutcome, BranchRecord, BranchStatus, DataKeyRecord,
    DatabaseRecord, DatabaseSettings, DatabaseStatus, EndpointLock, EndpointState,
    InsertRoleOutcome, LiveBranch, LiveDatabase, MetadataClient, MetadataError, NeonDatabase,
    NeonEndpoint, NeonRole, NewDataKey, NewEndpoint, ResyncApplied, ResyncWrite, RoleCopy,
    ServedDatabase,
};
use cheladb_neon::proxy_cache::ProxyCache;
use cheladb_neon::spec::Placement;
use cheladb_neon::storcon::LsnAtTime;
use cheladb_neon::{ComputeSigner, Lsn, NeonId, StorconClient, StorconError};
use cheladb_types::{BranchId, DatabaseId};
use chrono::{DateTime, Utc};

/// The metadata calls the worker makes. See [`MetadataClient`] for each one's guards.
pub trait WorkerMetadata: Send + Sync + 'static {
    /// Held while one endpoint's pending password is applied; dropping it releases the lock.
    type EndpointLock: Send;

    /// Waits until no one else holds `endpoint_id`'s lock (across worker processes).
    fn lock_endpoint(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<Self::EndpointLock, MetadataError>> + Send;
    fn get_database(
        &self,
        id: DatabaseId,
    ) -> impl Future<Output = Result<Option<DatabaseRecord>, MetadataError>> + Send;
    fn set_status(
        &self,
        id: DatabaseId,
        status: DatabaseStatus,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn get_branch(
        &self,
        id: BranchId,
    ) -> impl Future<Output = Result<Option<BranchRecord>, MetadataError>> + Send;
    /// The database's branches that aren't deleted, oldest first.
    fn list_branches(
        &self,
        db: DatabaseId,
    ) -> impl Future<Output = Result<Vec<BranchRecord>, MetadataError>> + Send;
    fn create_endpoint_for_branch(
        &self,
        id: BranchId,
        timeline_id: &str,
        ep: &NewEndpoint<'_>,
        parent_protected_seen: bool,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn set_neon_tenant(
        &self,
        db: DatabaseId,
        tenant: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn neon_endpoint(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<Option<NeonEndpoint>, MetadataError>> + Send;
    fn neon_roles_versioned(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<(Vec<NeonRole>, i64), MetadataError>> + Send;
    fn neon_databases(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<Vec<NeonDatabase>, MetadataError>> + Send;
    fn set_branch_ready(
        &self,
        id: BranchId,
        sealed_password: &str,
        branched_at_lsn: Option<&str>,
        history_start: Option<DateTime<Utc>>,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn set_failed(
        &self,
        id: DatabaseId,
        error: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn mark_deleted(
        &self,
        id: DatabaseId,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn mark_branch_deleted(
        &self,
        id: BranchId,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn delete_endpoint(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn replace_owner_secret(
        &self,
        id: BranchId,
        endpoint_id: &str,
        role: &str,
        verifier: &str,
        sealed: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn clear_needs_reconfigure(
        &self,
        endpoint_id: &str,
        applied_version: i64,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;

    // Login roles (see `cheladb_metadata::neon`).

    /// API create: `CapReached` at `cap` copies, `Exists` when the name is taken; the row is
    /// `pending`. Bumps the version. An unknown endpoint is a database error.
    fn insert_api_role(
        &self,
        endpoint_id: &str,
        name: &str,
        owner_level: bool,
        verifier: &str,
        password_enc: &str,
        cap: u32,
    ) -> impl Future<Output = Result<InsertRoleOutcome, MetadataError>> + Send;
    /// Any non-owner row's verifier and sealed password, `pending`; false when there's no
    /// such row. Bumps the version.
    fn replace_role_secret(
        &self,
        endpoint_id: &str,
        name: &str,
        verifier: &str,
        password_enc: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// Clears one row's `pending` while `roles_version` is still `version`.
    fn confirm_role(
        &self,
        endpoint_id: &str,
        name: &str,
        version: i64,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// Deletes a non-owner row; false when there was none. Bumps the version.
    fn delete_role_copy(
        &self,
        endpoint_id: &str,
        name: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// Every row of the endpoint (the owner's first), and `roles_version`.
    fn role_copies(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<(Vec<RoleCopy>, i64), MetadataError>> + Send;
    /// The resync's write, only while `roles_version` is still `read_version`.
    fn apply_resync(
        &self,
        endpoint_id: &str,
        branch: BranchId,
        read_version: i64,
        write: &ResyncWrite,
    ) -> impl Future<Output = Result<ResyncApplied, MetadataError>> + Send;
    /// Lifts the endpoint's resync guards (`forwarded_at`, `dbs_forwarded_at`): its catalog
    /// was replaced (C3). Unknown endpoint: a no-op.
    fn clear_forwarded(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    /// A row whose sealed password was dropped: `password_enc` NULL, `password_unreadable`
    /// true (C4).
    fn mark_password_unreadable(
        &self,
        endpoint_id: &str,
        name: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    /// Records the compute settings version the endpoint's compute was created with.
    fn set_compute_settings_version(
        &self,
        endpoint_id: &str,
        version: i32,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn replace_compute_token(
        &self,
        endpoint_id: &str,
        sha256_hex: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn fail_if_stale(
        &self,
        id: DatabaseId,
        max_age_secs: i64,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn list_live_for_reconcile(
        &self,
    ) -> impl Future<Output = Result<Vec<LiveDatabase>, MetadataError>> + Send;
    fn list_endpoints_for_reconcile(
        &self,
    ) -> impl Future<Output = Result<Vec<EndpointState>, MetadataError>> + Send;

    // Branches (see `cheladb_metadata::branches` for each one's guards).

    /// Guarded: `from` must match, and never out of deleting/deleted. True when changed.
    fn set_branch_status(
        &self,
        id: BranchId,
        from: &[BranchStatus],
        to: BranchStatus,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// Not once a delete has begun.
    fn set_branch_failed(
        &self,
        id: BranchId,
        error: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn set_branch_error(
        &self,
        id: BranchId,
        error: Option<&str>,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    /// → deleting (and the branch's backups); refused while the branch or one of its backups
    /// has live `normal` children.
    fn begin_branch_delete(
        &self,
        id: BranchId,
    ) -> impl Future<Output = Result<BranchDeleteOutcome, MetadataError>> + Send;
    fn live_children(
        &self,
        id: BranchId,
    ) -> impl Future<Output = Result<Vec<BranchRecord>, MetadataError>> + Send;
    fn live_backups_of(
        &self,
        id: BranchId,
    ) -> impl Future<Output = Result<Vec<BranchRecord>, MetadataError>> + Send;
    fn set_pending_timeline(
        &self,
        id: BranchId,
        timeline_id: Option<&str>,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn switch_to_pending_timeline(
        &self,
        id: BranchId,
        expected_pending: &str,
        history_start: Option<DateTime<Utc>>,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn switch_to_restored_timeline(
        &self,
        id: BranchId,
        expected_pending: &str,
        backup: BranchId,
        backup_name: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn clear_superseded_timeline(
        &self,
        id: BranchId,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    /// Compare-and-swap on the pending timeline (never while deleting): true when it changed.
    fn swap_pending_timeline(
        &self,
        id: BranchId,
        expected: Option<&str>,
        new: Option<&str>,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// resetting → resetting (refreshed) once the row is older than `max_age_secs`.
    fn claim_stale_reset(
        &self,
        id: BranchId,
        max_age_secs: i64,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// resetting → failed with `error` once the pending timeline was recorded more than
    /// `max_age_secs` ago; the pending timeline stays recorded.
    fn fail_stuck_restore(
        &self,
        id: BranchId,
        max_age_secs: i64,
        error: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// pending/provisioning → failed once the row is older than `max_age_secs`; never main.
    fn fail_branch_if_stale(
        &self,
        id: BranchId,
        max_age_secs: i64,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn database_settings(
        &self,
        db: DatabaseId,
    ) -> impl Future<Output = Result<Option<DatabaseSettings>, MetadataError>> + Send;
    fn list_live_branches_for_reconcile(
        &self,
    ) -> impl Future<Output = Result<Vec<LiveBranch>, MetadataError>> + Send;

    // The data API (see `cheladb_metadata::data_api`).

    /// A second live public key on the branch is [`MetadataError::Conflict`].
    fn insert_data_key(
        &self,
        key: &NewDataKey,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    /// The branch's live keys, newest first.
    fn list_data_keys(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<Vec<DataKeyRecord>, MetadataError>> + Send;
    fn has_live_public_key(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// Stores the password only when the branch has none. A no-op on a deleting, deleted
    /// or unknown branch.
    fn set_authenticator_password(
        &self,
        branch: BranchId,
        sealed: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    /// The branch's Postgres databases with their data API state: main first, then by name.
    fn branch_databases(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<Vec<BranchDatabase>, MetadataError>> + Send;
    /// The databases the data API serves (state `on`), main first, with their default schema.
    fn served_databases(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<Vec<ServedDatabase>, MetadataError>> + Send;
    /// Ends an enable: `Ok` makes the row `on`, `Err(text)` makes it `error`. Only a row still
    /// `enabling` moves; false otherwise.
    fn finish_enable_data_api(
        &self,
        branch: BranchId,
        name: &str,
        outcome: Result<(), String>,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// Switches a non-main database's data API off; false for main or an unknown name.
    fn set_data_api_off(
        &self,
        branch: BranchId,
        name: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// A new child's flags and default schemas, from its parent's rows by name (P3).
    fn copy_parent_database_flags(
        &self,
        child: BranchId,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    /// Every `(branch, database)` currently `enabling`.
    fn enabling_databases(
        &self,
    ) -> impl Future<Output = Result<Vec<(BranchId, String)>, MetadataError>> + Send;
    /// Ready `normal` branches without a live public key or an authenticator password.
    fn branches_missing_data_api(
        &self,
    ) -> impl Future<Output = Result<Vec<BranchId>, MetadataError>> + Send;
    fn branch_connection(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<Option<BranchConnectionRow>, MetadataError>> + Send;
}

// Each method calls the inherent one by its full path: a bare `self.method()` would recurse
// into this impl if the inherent method were ever renamed.
impl WorkerMetadata for MetadataClient {
    type EndpointLock = EndpointLock;

    async fn lock_endpoint(&self, endpoint_id: &str) -> Result<EndpointLock, MetadataError> {
        MetadataClient::lock_endpoint(self, endpoint_id).await
    }

    async fn get_database(&self, id: DatabaseId) -> Result<Option<DatabaseRecord>, MetadataError> {
        MetadataClient::get_database(self, id).await
    }

    async fn set_status(
        &self,
        id: DatabaseId,
        status: DatabaseStatus,
    ) -> Result<bool, MetadataError> {
        MetadataClient::set_status(self, id, status).await
    }

    async fn get_branch(&self, id: BranchId) -> Result<Option<BranchRecord>, MetadataError> {
        MetadataClient::get_branch(self, id).await
    }

    async fn list_branches(&self, db: DatabaseId) -> Result<Vec<BranchRecord>, MetadataError> {
        MetadataClient::list_branches(self, db).await
    }

    async fn create_endpoint_for_branch(
        &self,
        id: BranchId,
        timeline_id: &str,
        ep: &NewEndpoint<'_>,
        parent_protected_seen: bool,
    ) -> Result<bool, MetadataError> {
        MetadataClient::create_endpoint_for_branch(self, id, timeline_id, ep, parent_protected_seen).await
    }

    async fn set_neon_tenant(&self, db: DatabaseId, tenant: &str) -> Result<bool, MetadataError> {
        MetadataClient::set_neon_tenant(self, db, tenant).await
    }

    async fn neon_endpoint(
        &self,
        endpoint_id: &str,
    ) -> Result<Option<NeonEndpoint>, MetadataError> {
        MetadataClient::neon_endpoint(self, endpoint_id).await
    }

    async fn neon_roles_versioned(
        &self,
        endpoint_id: &str,
    ) -> Result<(Vec<NeonRole>, i64), MetadataError> {
        MetadataClient::neon_roles_versioned(self, endpoint_id).await
    }

    async fn neon_databases(&self, endpoint_id: &str) -> Result<Vec<NeonDatabase>, MetadataError> {
        MetadataClient::neon_databases(self, endpoint_id).await
    }

    async fn set_branch_ready(
        &self,
        id: BranchId,
        sealed_password: &str,
        branched_at_lsn: Option<&str>,
        history_start: Option<DateTime<Utc>>,
    ) -> Result<bool, MetadataError> {
        MetadataClient::set_branch_ready(self, id, sealed_password, branched_at_lsn, history_start)
            .await
    }

    async fn set_failed(&self, id: DatabaseId, error: &str) -> Result<(), MetadataError> {
        MetadataClient::set_failed(self, id, error).await
    }

    async fn mark_deleted(&self, id: DatabaseId) -> Result<(), MetadataError> {
        MetadataClient::mark_deleted(self, id).await
    }

    async fn mark_branch_deleted(&self, id: BranchId) -> Result<(), MetadataError> {
        MetadataClient::mark_branch_deleted(self, id).await
    }

    async fn delete_endpoint(&self, endpoint_id: &str) -> Result<(), MetadataError> {
        MetadataClient::delete_endpoint(self, endpoint_id).await
    }

    async fn replace_owner_secret(
        &self,
        id: BranchId,
        endpoint_id: &str,
        role: &str,
        verifier: &str,
        sealed: &str,
    ) -> Result<bool, MetadataError> {
        MetadataClient::replace_owner_secret(self, id, endpoint_id, role, verifier, sealed).await
    }

    async fn clear_needs_reconfigure(
        &self,
        endpoint_id: &str,
        applied_version: i64,
    ) -> Result<bool, MetadataError> {
        MetadataClient::clear_needs_reconfigure(self, endpoint_id, applied_version).await
    }

    async fn insert_api_role(
        &self,
        endpoint_id: &str,
        name: &str,
        owner_level: bool,
        verifier: &str,
        password_enc: &str,
        cap: u32,
    ) -> Result<InsertRoleOutcome, MetadataError> {
        MetadataClient::insert_api_role(
            self,
            endpoint_id,
            name,
            owner_level,
            verifier,
            password_enc,
            cap,
        )
        .await
    }

    async fn replace_role_secret(
        &self,
        endpoint_id: &str,
        name: &str,
        verifier: &str,
        password_enc: &str,
    ) -> Result<bool, MetadataError> {
        MetadataClient::replace_role_secret(self, endpoint_id, name, verifier, password_enc).await
    }

    async fn confirm_role(
        &self,
        endpoint_id: &str,
        name: &str,
        version: i64,
    ) -> Result<bool, MetadataError> {
        MetadataClient::confirm_role(self, endpoint_id, name, version).await
    }

    async fn delete_role_copy(&self, endpoint_id: &str, name: &str) -> Result<bool, MetadataError> {
        MetadataClient::delete_role_copy(self, endpoint_id, name).await
    }

    async fn role_copies(&self, endpoint_id: &str) -> Result<(Vec<RoleCopy>, i64), MetadataError> {
        MetadataClient::role_copies(self, endpoint_id).await
    }

    async fn apply_resync(
        &self,
        endpoint_id: &str,
        branch: BranchId,
        read_version: i64,
        write: &ResyncWrite,
    ) -> Result<ResyncApplied, MetadataError> {
        MetadataClient::apply_resync(self, endpoint_id, branch, read_version, write).await
    }

    async fn clear_forwarded(&self, endpoint_id: &str) -> Result<(), MetadataError> {
        MetadataClient::clear_forwarded(self, endpoint_id).await
    }

    async fn mark_password_unreadable(
        &self,
        endpoint_id: &str,
        name: &str,
    ) -> Result<(), MetadataError> {
        MetadataClient::mark_password_unreadable(self, endpoint_id, name).await
    }

    async fn set_compute_settings_version(
        &self,
        endpoint_id: &str,
        version: i32,
    ) -> Result<(), MetadataError> {
        MetadataClient::set_compute_settings_version(self, endpoint_id, version).await
    }

    async fn replace_compute_token(
        &self,
        endpoint_id: &str,
        sha256_hex: &str,
    ) -> Result<(), MetadataError> {
        MetadataClient::replace_compute_token(self, endpoint_id, sha256_hex).await
    }

    async fn fail_if_stale(
        &self,
        id: DatabaseId,
        max_age_secs: i64,
    ) -> Result<bool, MetadataError> {
        MetadataClient::fail_if_stale(self, id, max_age_secs).await
    }

    async fn list_live_for_reconcile(&self) -> Result<Vec<LiveDatabase>, MetadataError> {
        MetadataClient::list_live_for_reconcile(self).await
    }

    async fn list_endpoints_for_reconcile(&self) -> Result<Vec<EndpointState>, MetadataError> {
        MetadataClient::list_endpoints_for_reconcile(self).await
    }

    async fn set_branch_status(
        &self,
        id: BranchId,
        from: &[BranchStatus],
        to: BranchStatus,
    ) -> Result<bool, MetadataError> {
        MetadataClient::set_branch_status(self, id, from, to).await
    }

    async fn set_branch_failed(&self, id: BranchId, error: &str) -> Result<(), MetadataError> {
        MetadataClient::set_branch_failed(self, id, error).await
    }

    async fn set_branch_error(
        &self,
        id: BranchId,
        error: Option<&str>,
    ) -> Result<(), MetadataError> {
        MetadataClient::set_branch_error(self, id, error).await
    }

    async fn begin_branch_delete(
        &self,
        id: BranchId,
    ) -> Result<BranchDeleteOutcome, MetadataError> {
        MetadataClient::begin_branch_delete(self, id).await
    }

    async fn live_children(&self, id: BranchId) -> Result<Vec<BranchRecord>, MetadataError> {
        MetadataClient::live_children(self, id).await
    }

    async fn live_backups_of(&self, id: BranchId) -> Result<Vec<BranchRecord>, MetadataError> {
        MetadataClient::live_backups_of(self, id).await
    }

    async fn set_pending_timeline(
        &self,
        id: BranchId,
        timeline_id: Option<&str>,
    ) -> Result<bool, MetadataError> {
        MetadataClient::set_pending_timeline(self, id, timeline_id).await
    }

    async fn switch_to_pending_timeline(
        &self,
        id: BranchId,
        expected_pending: &str,
        history_start: Option<DateTime<Utc>>,
    ) -> Result<bool, MetadataError> {
        MetadataClient::switch_to_pending_timeline(self, id, expected_pending, history_start).await
    }

    async fn switch_to_restored_timeline(
        &self,
        id: BranchId,
        expected_pending: &str,
        backup: BranchId,
        backup_name: &str,
    ) -> Result<bool, MetadataError> {
        MetadataClient::switch_to_restored_timeline(self, id, expected_pending, backup, backup_name)
            .await
    }

    async fn clear_superseded_timeline(&self, id: BranchId) -> Result<(), MetadataError> {
        MetadataClient::clear_superseded_timeline(self, id).await
    }

    async fn swap_pending_timeline(
        &self,
        id: BranchId,
        expected: Option<&str>,
        new: Option<&str>,
    ) -> Result<bool, MetadataError> {
        MetadataClient::swap_pending_timeline(self, id, expected, new).await
    }

    async fn claim_stale_reset(
        &self,
        id: BranchId,
        max_age_secs: i64,
    ) -> Result<bool, MetadataError> {
        MetadataClient::claim_stale_reset(self, id, max_age_secs).await
    }

    async fn fail_stuck_restore(
        &self,
        id: BranchId,
        max_age_secs: i64,
        error: &str,
    ) -> Result<bool, MetadataError> {
        MetadataClient::fail_stuck_restore(self, id, max_age_secs, error).await
    }

    async fn fail_branch_if_stale(
        &self,
        id: BranchId,
        max_age_secs: i64,
    ) -> Result<bool, MetadataError> {
        MetadataClient::fail_branch_if_stale(self, id, max_age_secs).await
    }

    async fn database_settings(
        &self,
        db: DatabaseId,
    ) -> Result<Option<DatabaseSettings>, MetadataError> {
        MetadataClient::database_settings(self, db).await
    }

    async fn list_live_branches_for_reconcile(&self) -> Result<Vec<LiveBranch>, MetadataError> {
        MetadataClient::list_live_branches_for_reconcile(self).await
    }

    async fn insert_data_key(&self, key: &NewDataKey) -> Result<(), MetadataError> {
        MetadataClient::insert_data_key(self, key).await
    }

    async fn list_data_keys(&self, branch: BranchId) -> Result<Vec<DataKeyRecord>, MetadataError> {
        MetadataClient::list_data_keys(self, branch).await
    }

    async fn has_live_public_key(&self, branch: BranchId) -> Result<bool, MetadataError> {
        MetadataClient::has_live_public_key(self, branch).await
    }

    async fn set_authenticator_password(
        &self,
        branch: BranchId,
        sealed: &str,
    ) -> Result<(), MetadataError> {
        MetadataClient::set_authenticator_password(self, branch, sealed).await
    }

    async fn branch_databases(
        &self,
        branch: BranchId,
    ) -> Result<Vec<BranchDatabase>, MetadataError> {
        MetadataClient::branch_databases(self, branch).await
    }

    async fn served_databases(
        &self,
        branch: BranchId,
    ) -> Result<Vec<ServedDatabase>, MetadataError> {
        MetadataClient::served_databases(self, branch).await
    }

    async fn finish_enable_data_api(
        &self,
        branch: BranchId,
        name: &str,
        outcome: Result<(), String>,
    ) -> Result<bool, MetadataError> {
        MetadataClient::finish_enable_data_api(self, branch, name, outcome).await
    }

    async fn set_data_api_off(&self, branch: BranchId, name: &str) -> Result<bool, MetadataError> {
        MetadataClient::set_data_api_off(self, branch, name).await
    }

    async fn copy_parent_database_flags(&self, child: BranchId) -> Result<(), MetadataError> {
        MetadataClient::copy_parent_database_flags(self, child).await
    }

    async fn enabling_databases(&self) -> Result<Vec<(BranchId, String)>, MetadataError> {
        MetadataClient::enabling_databases(self).await
    }

    async fn branches_missing_data_api(&self) -> Result<Vec<BranchId>, MetadataError> {
        MetadataClient::branches_missing_data_api(self).await
    }

    async fn branch_connection(
        &self,
        branch: BranchId,
    ) -> Result<Option<BranchConnectionRow>, MetadataError> {
        MetadataClient::branch_connection(self, branch).await
    }
}

/// Tenants and timelines on the storage controller, plus where they're placed.
pub trait Storage: Placement + 'static {
    fn create_tenant(
        &self,
        tenant: &NeonId,
    ) -> impl Future<Output = Result<(), StorconError>> + Send;
    /// A fresh Postgres 17 timeline (no ancestor).
    fn create_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> impl Future<Output = Result<(), StorconError>> + Send;
    /// Waits until the tenant is gone; one that's already gone (404) counts as done.
    fn delete_tenant(
        &self,
        tenant: &NeonId,
    ) -> impl Future<Output = Result<(), StorconError>> + Send;
    fn list_tenant_ids(&self) -> impl Future<Output = Result<Vec<NeonId>, StorconError>> + Send;
    /// A Postgres 17 timeline branched from `parent` at `lsn`.
    fn create_branch_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
        parent: &NeonId,
        lsn: Lsn,
    ) -> impl Future<Output = Result<(), StorconError>> + Send;
    /// Waits until the timeline is gone; one that's already gone (404) counts as done.
    fn delete_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> impl Future<Output = Result<(), StorconError>> + Send;
    /// The timeline's head: its `last_record_lsn`.
    fn get_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> impl Future<Output = Result<Lsn, StorconError>> + Send;
    /// Where the timeline starts on its ancestor's; `None` for a timeline without one.
    fn ancestor_lsn(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> impl Future<Output = Result<Option<Lsn>, StorconError>> + Send;
    fn lsn_by_timestamp(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
        at: DateTime<Utc>,
    ) -> impl Future<Output = Result<LsnAtTime, StorconError>> + Send;
    /// `None` when no timestamp is known for the LSN.
    fn timestamp_of_lsn(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
        lsn: Lsn,
    ) -> impl Future<Output = Result<Option<DateTime<Utc>>, StorconError>> + Send;
    fn detach_ancestor(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> impl Future<Output = Result<(), StorconError>> + Send;
    fn patch_tenant_config(
        &self,
        tenant: &NeonId,
        pitr_interval: Duration,
    ) -> impl Future<Output = Result<(), StorconError>> + Send;
    /// `None` when it was never set.
    fn tenant_pitr_interval(
        &self,
        tenant: &NeonId,
    ) -> impl Future<Output = Result<Option<Duration>, StorconError>> + Send;
    fn list_timelines(
        &self,
        tenant: &NeonId,
    ) -> impl Future<Output = Result<Vec<NeonId>, StorconError>> + Send;
}

/// The Postgres major version every new timeline gets (the compute image is v17).
const PG_VERSION: u32 = 17;

impl Storage for StorconClient {
    async fn create_tenant(&self, tenant: &NeonId) -> Result<(), StorconError> {
        StorconClient::create_tenant(self, tenant).await
    }

    async fn create_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<(), StorconError> {
        StorconClient::create_timeline(self, tenant, timeline, PG_VERSION, None)
            .await
            .map(drop)
    }

    async fn delete_tenant(&self, tenant: &NeonId) -> Result<(), StorconError> {
        StorconClient::delete_tenant(self, tenant).await
    }

    async fn list_tenant_ids(&self) -> Result<Vec<NeonId>, StorconError> {
        StorconClient::list_tenant_ids(self).await
    }

    async fn create_branch_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
        parent: &NeonId,
        lsn: Lsn,
    ) -> Result<(), StorconError> {
        StorconClient::create_timeline(
            self,
            tenant,
            timeline,
            PG_VERSION,
            Some((parent, Some(lsn))),
        )
        .await
        .map(drop)
    }

    async fn delete_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<(), StorconError> {
        StorconClient::delete_timeline(self, tenant, timeline).await
    }

    async fn get_timeline(&self, tenant: &NeonId, timeline: &NeonId) -> Result<Lsn, StorconError> {
        StorconClient::get_timeline(self, tenant, timeline)
            .await
            .map(|t| t.last_record_lsn)
    }

    async fn ancestor_lsn(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<Option<Lsn>, StorconError> {
        StorconClient::get_timeline(self, tenant, timeline)
            .await
            .map(|t| t.ancestor_lsn)
    }

    async fn lsn_by_timestamp(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
        at: DateTime<Utc>,
    ) -> Result<LsnAtTime, StorconError> {
        StorconClient::lsn_by_timestamp(self, tenant, timeline, at).await
    }

    async fn timestamp_of_lsn(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
        lsn: Lsn,
    ) -> Result<Option<DateTime<Utc>>, StorconError> {
        StorconClient::timestamp_of_lsn(self, tenant, timeline, lsn).await
    }

    async fn detach_ancestor(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<(), StorconError> {
        StorconClient::detach_ancestor(self, tenant, timeline).await
    }

    async fn patch_tenant_config(
        &self,
        tenant: &NeonId,
        pitr_interval: Duration,
    ) -> Result<(), StorconError> {
        StorconClient::patch_tenant_config(self, tenant, pitr_interval).await
    }

    async fn tenant_pitr_interval(
        &self,
        tenant: &NeonId,
    ) -> Result<Option<Duration>, StorconError> {
        StorconClient::tenant_pitr_interval(self, tenant).await
    }

    async fn list_timelines(&self, tenant: &NeonId) -> Result<Vec<NeonId>, StorconError> {
        StorconClient::list_timelines(self, tenant).await
    }
}

/// Tells Neon's proxies that a role's password changed.
pub trait CacheNotifier: Send + Sync + 'static {
    /// How many proxies got the message.
    fn password_updated(
        &self,
        endpoint_id: &str,
        role: &str,
    ) -> impl Future<Output = anyhow::Result<u64>> + Send;
    /// `password_updated` for each of `roles` again after `delay`, from a background task
    /// (R22: a login between the first message and the commit may have cached the old
    /// password). Failures are logged.
    fn password_updated_later(&self, endpoint_id: &str, roles: Vec<String>, delay: Duration);
}

impl CacheNotifier for ProxyCache {
    async fn password_updated(&self, endpoint_id: &str, role: &str) -> anyhow::Result<u64> {
        ProxyCache::password_updated(self, endpoint_id, role).await
    }

    fn password_updated_later(&self, endpoint_id: &str, roles: Vec<String>, delay: Duration) {
        let (cache, endpoint_id) = (self.clone(), endpoint_id.to_string());
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            for role in roles {
                if let Err(e) = cache.password_updated(&endpoint_id, &role).await {
                    tracing::warn!(%endpoint_id, role, "publishing password_updated again: {e:#}");
                }
            }
        });
    }
}

/// Pushes a new spec to a running compute.
pub trait SpecPusher: Send + Sync + 'static {
    /// The keys computes accept control-plane tokens from.
    fn jwks(&self) -> serde_json::Value;
    fn push(
        &self,
        control_addr: SocketAddr,
        endpoint_id: &str,
        config: &serde_json::Value,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// [`SpecPusher`] over compute_ctl's `/configure`.
pub struct HttpPusher {
    pub http: reqwest::Client,
    pub signer: ComputeSigner,
}

impl SpecPusher for HttpPusher {
    fn jwks(&self) -> serde_json::Value {
        self.signer.jwks()
    }

    async fn push(
        &self,
        control_addr: SocketAddr,
        endpoint_id: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<()> {
        cheladb_neon::reconfigure::push(&self.http, control_addr, &self.signer, endpoint_id, config)
            .await
    }
}
