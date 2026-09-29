//! What the worker needs from the outside world, as traits the tests fake.

use std::future::Future;
use std::net::SocketAddr;

use cheladb_metadata::{
    DatabaseRecord, DatabaseStatus, EndpointLock, EndpointState, LiveDatabase, MetadataClient,
    MetadataError, NeonDatabase, NeonEndpoint, NeonRole, NewEndpoint, ReadyDatabase,
};
use cheladb_neon::proxy_cache::ProxyCache;
use cheladb_neon::spec::Placement;
use cheladb_neon::{ComputeSigner, NeonId, StorconClient, StorconError};
use cheladb_types::DatabaseId;

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
    fn create_endpoint_for_database(
        &self,
        id: DatabaseId,
        ep: &NewEndpoint<'_>,
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
    fn set_ready(
        &self,
        id: DatabaseId,
        endpoint_id: &str,
        db: ReadyDatabase<'_>,
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
    fn delete_endpoint(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn replace_owner_secret(
        &self,
        id: DatabaseId,
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

    async fn create_endpoint_for_database(
        &self,
        id: DatabaseId,
        ep: &NewEndpoint<'_>,
    ) -> Result<bool, MetadataError> {
        MetadataClient::create_endpoint_for_database(self, id, ep).await
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

    async fn set_ready(
        &self,
        id: DatabaseId,
        endpoint_id: &str,
        db: ReadyDatabase<'_>,
    ) -> Result<bool, MetadataError> {
        MetadataClient::set_ready(self, id, endpoint_id, db).await
    }

    async fn set_failed(&self, id: DatabaseId, error: &str) -> Result<(), MetadataError> {
        MetadataClient::set_failed(self, id, error).await
    }

    async fn mark_deleted(&self, id: DatabaseId) -> Result<(), MetadataError> {
        MetadataClient::mark_deleted(self, id).await
    }

    async fn delete_endpoint(&self, endpoint_id: &str) -> Result<(), MetadataError> {
        MetadataClient::delete_endpoint(self, endpoint_id).await
    }

    async fn replace_owner_secret(
        &self,
        id: DatabaseId,
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
