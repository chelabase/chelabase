//! What Neon's proxy calls (`--auth-backend=control-plane`, neon
//! `proxy/src/control_plane/client/cplane_proxy_v1.rs`): the role's SCRAM secret, and
//! the address of the endpoint's compute.

use super::{InternalState, NeonMetadataPort};
use axum::Json;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chelabase_neon::ComputeLocator;
use chelabase_neon::proxy_api::{self, Reason};
use serde::Deserialize;
use std::sync::Arc;
use tracing::{error, warn};

/// The proxy also sends `session_id`, `application_name` and `options`; they're ignored.
#[derive(Debug, Deserialize)]
pub struct AccessControlQuery {
    endpointish: String,
    role: String,
}

#[derive(Debug, Deserialize)]
pub struct WakeQuery {
    endpointish: String,
}

const RETRY_MS: u64 = 1000;

fn reply(status: StatusCode, body: serde_json::Value) -> Response {
    (status, Json(body)).into_response()
}

fn not_found(endpoint: &str) -> Response {
    reply(
        StatusCode::NOT_FOUND,
        proxy_api::error(
            Reason::EndpointNotFound,
            &format!("endpoint {endpoint} not found"),
            None,
        ),
    )
}

fn internal(err: impl std::fmt::Display) -> Response {
    error!("proxy API: {err}");
    reply(
        StatusCode::INTERNAL_SERVER_ERROR,
        proxy_api::error(Reason::Unknown, "internal error", Some(RETRY_MS)),
    )
}

/// 503 with `retry_ms`: the proxy retries, for example while a compute boots.
fn retry_later(message: &str) -> Response {
    reply(
        StatusCode::SERVICE_UNAVAILABLE,
        proxy_api::error(Reason::Unknown, message, Some(RETRY_MS)),
    )
}

pub async fn access_control<M: NeonMetadataPort, L: ComputeLocator>(
    State(state): State<Arc<InternalState<M, L>>>,
    Query(q): Query<AccessControlQuery>,
) -> Response {
    match state
        .metadata
        .routable_role_secret(&q.endpointish, &q.role)
        .await
    {
        Err(e) => internal(e),
        Ok(None) => not_found(&q.endpointish),
        Ok(Some(secret)) => Json(proxy_api::access_control(
            &q.endpointish,
            secret.as_deref().unwrap_or(""),
        ))
        .into_response(),
    }
}

pub async fn wake_compute<M: NeonMetadataPort, L: ComputeLocator>(
    State(state): State<Arc<InternalState<M, L>>>,
    Query(q): Query<WakeQuery>,
) -> Response {
    let endpoint = match state.metadata.routable_endpoint(&q.endpointish).await {
        Err(e) => return internal(e),
        Ok(None) => return not_found(&q.endpointish),
        Ok(Some(endpoint)) => endpoint,
    };
    match state.locator.address(&endpoint.id).await {
        Ok(Some(ip)) => Json(proxy_api::wake_compute(
            ip,
            &endpoint.id,
            &endpoint.timeline_id,
        ))
        .into_response(),
        Ok(None) => retry_later("compute is starting"),
        Err(e) => {
            warn!(endpoint = %endpoint.id, "locating compute: {e:#}");
            retry_later("compute is unavailable")
        }
    }
}
