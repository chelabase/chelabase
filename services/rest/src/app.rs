//! The HTTP edge. The public listener serves the gateway (`POST /data/v1/query`,
//! `GET /data/v1/schema`) and trusts only the gateway's identity token, minted for `rest`
//! with a data key's identity. The internal listener serves the orchestrator
//! (`GET /internal/schema/{branch_id}`) behind a static bearer.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use cheladb_auth::internal::{AuthMethod, DataIdentity, IDENTITY_HEADER, InternalVerifier};
use cheladb_types::data_key::DataKeyKind;
use serde_json::json;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::conn::{ConnError, ConnSource};
use crate::error::ApiError;
use crate::exec::{Engine, Failure};

/// Largest request body (1 MiB).
pub const MAX_BODY: usize = 1_048_576;
/// The identity token's audience.
pub const AUDIENCE: &str = "rest";

pub struct AppState<S> {
    pub engine: Engine<S>,
    pub verifier: InternalVerifier,
    /// The bearer the orchestrator sends to the internal listener.
    pub internal_token: String,
}

/// The service's settings, from the environment.
pub struct Config {
    pub addr: SocketAddr,
    pub internal_addr: SocketAddr,
    /// The orchestrator's internal listener, for connection info.
    pub internal_url: String,
    /// The bearer for the internal listener, both ways.
    pub internal_token: String,
    pub max_connections: usize,
    /// The largest query response (`CHELADB_REST_MAX_RESPONSE_BYTES`, default 16 MiB).
    pub max_response_bytes: usize,
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_owned())
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Config {
            addr: env_or("CHELADB_REST_ADDR", "127.0.0.1:8085").parse()?,
            internal_addr: env_or("CHELADB_REST_INTERNAL_ADDR", "127.0.0.1:8086").parse()?,
            internal_url: env_or("CHELADB_INTERNAL_URL", "http://127.0.0.1:8083"),
            internal_token: env_or("CHELADB_REST_INTERNAL_TOKEN", "cheladb-dev-rest-internal"),
            max_connections: env_or("CHELADB_REST_MAX_CONNECTIONS", "200").parse()?,
            max_response_bytes: env_or(
                "CHELADB_REST_MAX_RESPONSE_BYTES",
                &crate::exec::DEFAULT_MAX_RESPONSE_BYTES.to_string(),
            )
            .parse()?,
        })
    }
}

pub fn router<S: ConnSource>(state: Arc<AppState<S>>) -> Router {
    Router::new()
        .route("/data/v1/query", post(query::<S>))
        .route("/data/v1/schema", get(role_schema::<S>))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(state)
}

pub fn internal_router<S: ConnSource>(state: Arc<AppState<S>>) -> Router {
    Router::new()
        .route("/internal/schema/{branch_id}", get(branch_schema::<S>))
        .with_state(state)
}

/// The data key behind the request, and the gateway's request id.
fn identity<S>(
    state: &AppState<S>,
    headers: &HeaderMap,
) -> Result<(DataIdentity, String), ApiError> {
    let token = headers
        .get(IDENTITY_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::unauthorized("missing identity"))?;
    let claims = state
        .verifier
        .verify(token, AUDIENCE)
        .map_err(|_| ApiError::unauthorized("invalid identity"))?;
    match (claims.auth, claims.data) {
        (AuthMethod::DataKey, Some(data)) => Ok((data, claims.rid)),
        _ => Err(ApiError::unauthorized("a data API key is required")),
    }
}

fn json_response(body: String) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// The 503 message when the orchestrator can't say where a branch's database is.
pub const LOOKUP_UNAVAILABLE: &str = "the data API can't look up the branch right now; try again";

/// The internal listener couldn't be asked: what happened goes to the log only.
fn lookup_unavailable(detail: &str, request_id: &str) -> Response {
    tracing::warn!(request_id, "connection info: {detail}");
    ApiError::unavailable(LOOKUP_UNAVAILABLE).into_response()
}

/// A failure on a public route: a branch that's gone is a key that no longer works.
fn public_failure(f: Failure, request_id: &str) -> Response {
    match f {
        Failure::Api(e) => e.into_response(),
        Failure::Conn(ConnError::UnknownBranch) => {
            ApiError::unauthorized("the key's branch no longer exists").into_response()
        }
        Failure::Conn(e @ ConnError::NotReady(_)) => {
            ApiError::unavailable(e.to_string()).into_response()
        }
        Failure::Conn(ConnError::Unreachable(detail)) => lookup_unavailable(&detail, request_id),
    }
}

/// A failure on the internal schema route, which the orchestrator passes on to Studio.
fn internal_failure(f: Failure, request_id: &str) -> Response {
    match f {
        Failure::Api(e) => e.into_response(),
        Failure::Conn(ConnError::UnknownBranch) => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({ "error": { "code": "unknown_branch", "message": "the branch doesn't exist" } })),
        )
            .into_response(),
        Failure::Conn(ConnError::NotReady(status)) => {
            (StatusCode::CONFLICT, axum::Json(json!({ "status": status }))).into_response()
        }
        Failure::Conn(ConnError::Unreachable(detail)) => lookup_unavailable(&detail, request_id),
    }
}

async fn query<S: ConnSource>(
    State(state): State<Arc<AppState<S>>>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let (data, rid) = match identity(&state, &headers) {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    let body = match body {
        Ok(body) => body,
        Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return ApiError::too_large().into_response();
        }
        Err(e) => return ApiError::invalid_query(e.body_text()).into_response(),
    };
    match state.engine.query(&data, &rid, &body).await {
        Ok(result) => json_response(result),
        Err(f) => public_failure(f, &rid),
    }
}

async fn role_schema<S: ConnSource>(
    State(state): State<Arc<AppState<S>>>,
    headers: HeaderMap,
) -> Response {
    let (data, rid) = match identity(&state, &headers) {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    // The gateway already refuses public keys here; check again.
    if data.kind != DataKeyKind::Secret {
        return ApiError::unauthorized("the schema needs a secret key").into_response();
    }
    match state.engine.role_schema(&data, &rid).await {
        Ok(schema) => axum::Json(schema).into_response(),
        Err(f) => public_failure(f, &rid),
    }
}

async fn branch_schema<S: ConnSource>(
    State(state): State<Arc<AppState<S>>>,
    Path(branch): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    if !bool::from(bearer.as_bytes().ct_eq(state.internal_token.as_bytes())) {
        return ApiError::unauthorized("a valid bearer is required").into_response();
    }
    let rid = format!("internal-{}", Uuid::new_v4());
    match state.engine.branch_schema(branch, &rid).await {
        Ok(schema) => axum::Json(schema).into_response(),
        Err(f) => internal_failure(f, &rid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The internal listener's address and reqwest's text stay in the log.
    #[tokio::test]
    async fn an_unreachable_internal_listener_is_a_fixed_503() {
        let detail = "error sending request for url (http://10.9.8.7:8083/internal/data/branches/x/connection)";
        for response in [
            public_failure(
                Failure::Conn(ConnError::Unreachable(detail.into())),
                "rid-1",
            ),
            internal_failure(
                Failure::Conn(ConnError::Unreachable(detail.into())),
                "rid-1",
            ),
        ] {
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let text = String::from_utf8(body.to_vec()).unwrap();
            assert!(
                !text.contains("10.9.8.7") && !text.contains("sending"),
                "{text}"
            );
        }
    }
}
