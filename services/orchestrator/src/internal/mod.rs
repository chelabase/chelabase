//! The internal listener: machine-to-machine routes for Neon's proxy, compute_ctl and the
//! storage controller. It binds `CHELADB_INTERNAL_ADDR` (loopback by default) and never goes
//! through the gateway. Each caller has its own bearer token and can open only its own routes.

pub mod config;
mod data;
mod hooks;
mod proxy;
mod spec;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use cheladb_metadata::{
    BranchConnectionRow, DataKeyLookup, MetadataClient, MetadataError, NeonDatabase, NeonEndpoint,
    NeonRole,
};
use cheladb_secrets::SecretKey;
use cheladb_types::BranchId;
use uuid::Uuid;
use cheladb_neon::{ComputeLocator, ComputeSigner, StorconClient};
use std::future::Future;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tower_http::trace::TraceLayer;

/// The metadata the internal routes read. Abstracted so tests can use an in-memory fake.
pub trait NeonMetadataPort: Send + Sync + 'static {
    fn neon_endpoint(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<Option<NeonEndpoint>, MetadataError>> + Send;
    fn neon_role_secret(
        &self,
        endpoint_id: &str,
        role: &str,
    ) -> impl Future<Output = Result<Option<Option<String>>, MetadataError>> + Send;
    fn neon_roles(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<Vec<NeonRole>, MetadataError>> + Send;
    fn neon_databases(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<Vec<NeonDatabase>, MetadataError>> + Send;
    fn neon_endpoints_for_tenant(
        &self,
        tenant_id: &str,
    ) -> impl Future<Output = Result<Vec<String>, MetadataError>> + Send;
    fn neon_endpoints_for_timeline(
        &self,
        timeline_id: &str,
    ) -> impl Future<Output = Result<Vec<String>, MetadataError>> + Send;

    /// The live key with this hash, if its branch isn't deleting or deleted.
    fn lookup_data_key(
        &self,
        sha256: &[u8; 32],
    ) -> impl Future<Output = Result<Option<DataKeyLookup>, MetadataError>> + Send;

    /// Stamps the key's `last_used_at` (at most once a minute).
    fn touch_data_key(
        &self,
        key_id: Uuid,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;

    /// What the REST service needs to connect to a branch, whatever its status.
    fn branch_connection(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<Option<BranchConnectionRow>, MetadataError>> + Send;
}

// As for `MetadataPort`: each method must call `MetadataClient`'s inherent method by its full
// path, or a rename would make it call itself.
impl NeonMetadataPort for MetadataClient {
    async fn neon_endpoint(&self, id: &str) -> Result<Option<NeonEndpoint>, MetadataError> {
        MetadataClient::neon_endpoint(self, id).await
    }
    async fn neon_role_secret(
        &self,
        endpoint_id: &str,
        role: &str,
    ) -> Result<Option<Option<String>>, MetadataError> {
        MetadataClient::neon_role_secret(self, endpoint_id, role).await
    }
    async fn neon_roles(&self, endpoint_id: &str) -> Result<Vec<NeonRole>, MetadataError> {
        MetadataClient::neon_roles(self, endpoint_id).await
    }
    async fn neon_databases(&self, endpoint_id: &str) -> Result<Vec<NeonDatabase>, MetadataError> {
        MetadataClient::neon_databases(self, endpoint_id).await
    }
    async fn neon_endpoints_for_tenant(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<String>, MetadataError> {
        MetadataClient::neon_endpoints_for_tenant(self, tenant_id).await
    }
    async fn neon_endpoints_for_timeline(
        &self,
        timeline_id: &str,
    ) -> Result<Vec<String>, MetadataError> {
        MetadataClient::neon_endpoints_for_timeline(self, timeline_id).await
    }
    async fn lookup_data_key(
        &self,
        sha256: &[u8; 32],
    ) -> Result<Option<DataKeyLookup>, MetadataError> {
        MetadataClient::lookup_data_key(self, sha256).await
    }
    async fn touch_data_key(&self, key_id: Uuid) -> Result<(), MetadataError> {
        MetadataClient::touch_data_key(self, key_id).await
    }
    async fn branch_connection(
        &self,
        branch: BranchId,
    ) -> Result<Option<BranchConnectionRow>, MetadataError> {
        MetadataClient::branch_connection(self, branch).await
    }
}

pub struct InternalState<M, L> {
    pub metadata: M,
    pub locator: L,
    pub storcon: StorconClient,
    /// Signs the tokens compute_ctl accepts, and publishes their JWKS in each spec.
    pub signer: ComputeSigner,
    /// For compute_ctl's `/configure`.
    pub http: reqwest::Client,
    pub proxy_token: String,
    pub storcon_token: String,
    /// The gateway's token for key lookups.
    pub data_keys_token: String,
    /// The REST service's token for connection info.
    pub rest_token: String,
    /// Opens the `authenticator` passwords handed to the REST service.
    pub secrets: SecretKey,
}

pub fn build_internal_router<M, L>(state: Arc<InternalState<M, L>>) -> Router
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    let proxy_token: Arc<str> = state.proxy_token.as_str().into();
    let proxy = Router::new()
        .route(
            "/proxy/api/v1/get_endpoint_access_control",
            get(proxy::access_control::<M, L>),
        )
        .route(
            "/proxy/api/v1/wake_compute",
            get(proxy::wake_compute::<M, L>),
        )
        .route_layer(middleware::from_fn_with_state(proxy_token, require_bearer));
    let storcon_token: Arc<str> = state.storcon_token.as_str().into();
    let storcon = Router::new()
        .route("/storcon/notify-attach", put(hooks::notify_attach::<M, L>))
        .route(
            "/storcon/notify-safekeepers",
            put(hooks::notify_safekeepers::<M, L>),
        )
        .route_layer(middleware::from_fn_with_state(
            storcon_token,
            require_bearer,
        ));
    let data_keys_token: Arc<str> = state.data_keys_token.as_str().into();
    let data_keys = Router::new()
        .route("/internal/data/keys/{sha256}", get(data::key_lookup::<M, L>))
        .route_layer(middleware::from_fn_with_state(
            data_keys_token,
            require_bearer,
        ));
    let rest_token: Arc<str> = state.rest_token.as_str().into();
    let rest = Router::new()
        .route(
            "/internal/data/branches/{id}/connection",
            get(data::branch_connection::<M, L>),
        )
        .route_layer(middleware::from_fn_with_state(rest_token, require_bearer));
    Router::new()
        .route("/health", get(health))
        .route(
            "/compute/api/v2/computes/{id}/spec",
            get(spec::compute_spec::<M, L>),
        )
        .merge(proxy)
        .merge(storcon)
        .merge(data_keys)
        .merge(rest)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok", "service": "orchestrator-internal" }))
}

/// The bearer token from `Authorization`, if any.
pub(crate) fn bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

/// Runs before extractors, so a caller without the token learns nothing about the body format.
async fn require_bearer(State(token): State<Arc<str>>, request: Request, next: Next) -> Response {
    match bearer(request.headers()) {
        Some(t) if constant_time_eq(t.as_bytes(), token.as_bytes()) => next.run(request).await,
        _ => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// Compare secrets without an early exit, so timing doesn't show how much matched.
pub(crate) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.ct_eq(b).into()
}

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn compares_secrets() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret-longer"));
        assert!(!constant_time_eq(b"", b"x"));
    }
}
