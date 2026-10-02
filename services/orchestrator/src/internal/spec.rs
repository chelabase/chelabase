//! `GET /compute/api/v2/computes/{id}/spec`: compute_ctl pulls its spec here when it boots
//! (neon `compute_tools/src/spec.rs`). The bearer token is the compute's own; only its SHA-256
//! is stored. compute_ctl retries 503. [`authorize_compute`] is shared with the DDL receiver.

use super::{InternalState, NeonMetadataPort, bearer, constant_time_eq};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use cheladb_metadata::NeonEndpoint;
use cheladb_neon::ComputeLocator;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tracing::warn;

/// Stands in for the stored digest of an unknown id (64 hex characters, like a real one).
const UNKNOWN_ENDPOINT_DIGEST: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// `spec::config_response` for an endpoint: storage placement from the storage controller;
/// roles and databases from metadata.
pub(crate) async fn endpoint_config<M: NeonMetadataPort, L: ComputeLocator>(
    state: &InternalState<M, L>,
    endpoint: &NeonEndpoint,
) -> anyhow::Result<Value> {
    let roles = state.metadata.neon_roles(&endpoint.id).await?;
    let databases = state.metadata.neon_databases(&endpoint.id).await?;
    cheladb_neon::spec::stored_endpoint_config(
        &state.storcon,
        endpoint,
        roles,
        databases,
        state.signer.jwks(),
        Some(cheladb_neon::spec::ddl_url(
            &state.control_plane_uri,
            &endpoint.id,
        )),
    )
    .await
}

/// The endpoint whose compute token `headers` carries. 401 for no token, a wrong one or an
/// unknown id (the same path and timing for each); 503 when metadata can't be read.
// The refusal is the handler's answer as is; it's built once per request, never stored.
#[allow(clippy::result_large_err)]
pub(crate) async fn authorize_compute<M: NeonMetadataPort, L: ComputeLocator>(
    state: &InternalState<M, L>,
    id: &str,
    headers: &HeaderMap,
) -> Result<NeonEndpoint, Response> {
    // Hash first and compare even for an unknown id, so timing doesn't show whether it exists.
    let presented = bearer(headers).map(|t| hex::encode(Sha256::digest(t.as_bytes())));
    let endpoint = match state.metadata.neon_endpoint(id).await {
        Err(e) => {
            warn!(endpoint = %id, "compute token check: {e:#}");
            return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
        }
        Ok(endpoint) => endpoint,
    };
    let expected = endpoint
        .as_ref()
        .map_or(UNKNOWN_ENDPOINT_DIGEST, |e| e.compute_token_sha256.as_str());
    let matches = presented
        .as_deref()
        .is_some_and(|p| constant_time_eq(p.as_bytes(), expected.as_bytes()));
    match endpoint {
        Some(endpoint) if matches => Ok(endpoint),
        _ => Err(StatusCode::UNAUTHORIZED.into_response()),
    }
}

pub async fn compute_spec<M: NeonMetadataPort, L: ComputeLocator>(
    State(state): State<Arc<InternalState<M, L>>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let endpoint = match authorize_compute(&state, &id, &headers).await {
        Ok(endpoint) => endpoint,
        Err(refused) => return refused,
    };
    match endpoint_config(&state, &endpoint).await {
        Ok(config) => Json(config).into_response(),
        Err(e) => {
            warn!(endpoint = %id, "spec: {e:#}");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}
