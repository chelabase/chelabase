//! What the worker needs from the outside world, as traits the tests fake.

use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use cheladb_metadata::{
    BranchDeleteOutcome, BranchRecord, BranchStatus, DatabaseRecord, DatabaseSettings,
    DatabaseStatus, EndpointLock, EndpointState, LiveBranch, LiveDatabase, MetadataClient,
    MetadataError, NeonDatabase, NeonEndpoint, NeonRole, NewEndpoint,
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
    fn neon_roles(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<Vec<NeonRole>, MetadataError>> + Send;
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
        role: &str,
        applied_verifier: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
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
    ) -> Result<bool, MetadataError> {
        MetadataClient::create_endpoint_for_branch(self, id, timeline_id, ep).await
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

    async fn neon_roles(&self, endpoint_id: &str) -> Result<Vec<NeonRole>, MetadataError> {
        MetadataClient::neon_roles(self, endpoint_id).await
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
        role: &str,
        applied_verifier: &str,
    ) -> Result<bool, MetadataError> {
        MetadataClient::clear_needs_reconfigure(self, endpoint_id, role, applied_verifier).await
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
}

impl CacheNotifier for ProxyCache {
    async fn password_updated(&self, endpoint_id: &str, role: &str) -> anyhow::Result<u64> {
        ProxyCache::password_updated(self, endpoint_id, role).await
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
