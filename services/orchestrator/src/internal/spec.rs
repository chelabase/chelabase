//! `GET /compute/api/v2/computes/{id}/spec`: compute_ctl pulls its spec here when it boots
//! (neon `compute_tools/src/spec.rs`). The bearer token is the compute's own; only its SHA-256
//! is stored. compute_ctl retries 503.

use super::{InternalState, NeonMetadataPort, bearer, constant_time_eq};
use anyhow::Context;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use cheladb_metadata::NeonEndpoint;
use cheladb_neon::ComputeLocator;
use cheladb_neon::NeonId;
use cheladb_neon::spec::{DatabaseSpec, RoleSpec, SpecInputs, config_response};
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
    let tenant: NeonId = endpoint.tenant_id.parse().context("stored tenant id")?;
    let timeline: NeonId = endpoint.timeline_id.parse().context("stored timeline id")?;
    let pageserver_connstring = state.storcon.pageserver_connstring(&tenant).await?;
    let safekeeper_connstrings = state.storcon.active_safekeepers().await?;
    let roles = state.metadata.neon_roles(&endpoint.id).await?;
    let databases = state.metadata.neon_databases(&endpoint.id).await?;
    Ok(config_response(&SpecInputs {
        endpoint_id: &endpoint.id,
        tenant_id: &tenant,
        timeline_id: &timeline,
        pageserver_connstring,
        safekeeper_connstrings,
        roles: roles
            .into_iter()
            .map(|r| RoleSpec {
                name: r.name,
                scram_verifier: r.scram_verifier,
            })
            .collect(),
        databases: databases
            .into_iter()
            .map(|d| DatabaseSpec {
                name: d.name,
                owner: d.owner,
            })
            .collect(),
        jwks: state.signer.jwks(),
    }))
}

pub async fn compute_spec<M: NeonMetadataPort, L: ComputeLocator>(
    State(state): State<Arc<InternalState<M, L>>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    // Hash first and compare even for an unknown id, so timing doesn't show whether it exists.
    let presented = bearer(&headers).map(|t| hex::encode(Sha256::digest(t.as_bytes())));
    let endpoint = match state.metadata.neon_endpoint(&id).await {
        Err(e) => {
            warn!(endpoint = %id, "spec: {e:#}");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        Ok(endpoint) => endpoint,
    };
    let expected = endpoint
        .as_ref()
        .map_or(UNKNOWN_ENDPOINT_DIGEST, |e| e.compute_token_sha256.as_str());
    let matches = presented
        .as_deref()
        .is_some_and(|p| constant_time_eq(p.as_bytes(), expected.as_bytes()));
    let endpoint = match endpoint {
        Some(endpoint) if matches => endpoint,
        _ => return StatusCode::UNAUTHORIZED.into_response(),
    };
    match endpoint_config(&state, &endpoint).await {
        Ok(config) => Json(config).into_response(),
        Err(e) => {
            warn!(endpoint = %id, "spec: {e:#}");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}
