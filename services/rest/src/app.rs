//! The HTTP edge. The public listener serves the gateway (`POST /data/v1/query`,
//! `GET /data/v1/schema`) and trusts only the gateway's identity token, minted for `rest`
//! with a data key's identity. The internal listener serves the orchestrator behind a
//! static bearer: `GET /internal/schema/{branch_id}` (`?database=&role=`),
//! `POST /internal/branches/{id}/invalidate`
//! and `POST /internal/console/{branch_id}/query`.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chelabase_auth::internal::{AuthMethod, DataIdentity, IDENTITY_HEADER, InternalVerifier};
use chelabase_query::openapi::{QueryRequest, QueryResult};
use chelabase_types::data_key::DataKeyKind;
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use crate::conn::{ConnError, ConnSource};
use crate::error::ApiError;
use crate::exec::{Engine, Failure};
use crate::openapi::{DataErrorEnvelope, DataSchemaResponse};

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
    /// The largest query response (`CHELABASE_REST_MAX_RESPONSE_BYTES`, default 16 MiB).
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
            addr: env_or("CHELABASE_REST_ADDR", "127.0.0.1:8085").parse()?,
            internal_addr: env_or("CHELABASE_REST_INTERNAL_ADDR", "127.0.0.1:8086").parse()?,
            internal_url: env_or("CHELABASE_INTERNAL_URL", "http://127.0.0.1:8083"),
            internal_token: env_or("CHELABASE_REST_INTERNAL_TOKEN", "chelabase-dev-rest-internal"),
            max_connections: env_or("CHELABASE_REST_MAX_CONNECTIONS", "200").parse()?,
            max_response_bytes: env_or(
                "CHELABASE_REST_MAX_RESPONSE_BYTES",
                &crate::exec::DEFAULT_MAX_RESPONSE_BYTES.to_string(),
            )
            .parse()?,
        })
    }
}

/// The public routes with their OpenAPI annotations ([`crate::openapi::openapi`]).
pub fn api_router<S: ConnSource>() -> OpenApiRouter<Arc<AppState<S>>> {
    OpenApiRouter::new()
        .routes(routes!(query))
        .routes(routes!(role_schema))
}

pub fn router<S: ConnSource>(state: Arc<AppState<S>>) -> Router {
    api_router()
        .split_for_parts()
        .0
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(state)
}

pub fn internal_router<S: ConnSource>(state: Arc<AppState<S>>) -> Router {
    Router::new()
        .route("/internal/schema/{branch_id}", get(branch_schema::<S>))
        .route(
            "/internal/branches/{branch_id}/invalidate",
            post(invalidate::<S>),
        )
        .route("/internal/console/{branch_id}/query", post(console::<S>))
        .layer(DefaultBodyLimit::max(MAX_BODY))
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

/// The internal listener's bearer, compared in constant time.
fn check_bearer<S>(state: &AppState<S>, headers: &HeaderMap) -> Result<(), ApiError> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    if bool::from(bearer.as_bytes().ct_eq(state.internal_token.as_bytes())) {
        Ok(())
    } else {
        Err(ApiError::unauthorized("a valid bearer is required"))
    }
}

/// `?database=` on the schema routes.
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct DatabaseParam {
    /// The Postgres database to describe; the key's main database when left out. It must be
    /// served and, when the key has a database list, on it.
    database: Option<String>,
}

/// `?database=&role=` on the internal schema route (`role`: the console's).
#[derive(Deserialize)]
struct InternalSchemaParam {
    database: Option<String>,
    role: Option<String>,
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

/// Runs one operation, or a batch, in one transaction as the key's role.
#[utoipa::path(
    post,
    path = "/data/v1/query",
    operation_id = "runQuery",
    tag = "data",
    summary = "Run a query",
    description = "Runs one operation (select, insert, update, delete or a function call), or a batch of them in order, in one transaction as the key's role. With a public key and a `Chelabase-User` token it runs as `authenticated` with the user's claims. Row-level security applies throughout.",
    request_body(content = QueryRequest, content_type = "application/json"),
    security(("bearer" = []), ("bearer" = [], "user" = [])),
    responses(
        (status = 200, description = "The result as JSON: an array of row objects for a select and for writes (`[]` without `returning`), the function's result for a call, and for a batch the array of each operation's result", body = QueryResult),
        (status = 400, description = "`invalid_query`, `unknown_column`, `invalid_value`, `raised`, `user_token_with_secret_key`: the body isn't a valid query or doesn't plan (`invalid_query`), names a column the role can't see (`unknown_column`), carries a value Postgres rejects (`invalid_value`), a function raised an exception (`raised`), or a `Chelabase-User` token came with a secret key (`user_token_with_secret_key`)", body = DataErrorEnvelope),
        (status = 401, description = "`unauthorized`, `user_token_invalid`, `user_token_expired`, `user_session_revoked`: the key is missing, invalid or revoked, or its branch is gone (`unauthorized`); or the `Chelabase-User` token is invalid, expired or from a revoked session", body = DataErrorEnvelope),
        (status = 403, description = "`permission_denied`, `role_not_allowed`: Postgres refused the role (`permission_denied`), or the secret key's role can bypass row-level security (`role_not_allowed`)", body = DataErrorEnvelope),
        (status = 404, description = "`unknown_table`, `unknown_function`, `unknown_database`: the role can't see the table or function, or the database doesn't exist, isn't served or isn't on the key's list", body = DataErrorEnvelope),
        (status = 409, description = "`conflict`, `retry`: a unique, foreign-key or exclusion constraint failed (`conflict`), or a serialization failure or deadlock: run it again (`retry`)", body = DataErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over 1 MiB, or the response is over the server's limit", body = DataErrorEnvelope),
        (status = 429, description = "`rate_limited`: too many requests", body = DataErrorEnvelope),
        (status = 500, description = "`internal`: an unexpected error; the message carries the request id", body = DataErrorEnvelope),
        (status = 503, description = "`unavailable`: the database can't be reached right now; retry after `Retry-After`", body = DataErrorEnvelope,
            headers(("Retry-After" = u32, description = "Seconds to wait before retrying"))),
        (status = 504, description = "`timeout`: a statement ran past its timeout", body = DataErrorEnvelope),
    ),
)]
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

/// What the key's role can reach in a database, and the databases the key may reach.
#[utoipa::path(
    get,
    path = "/data/v1/schema",
    operation_id = "getDataSchema",
    tag = "data",
    summary = "The schema as the key's role sees it",
    description = "The tables, views, columns, functions and enums the secret key's role can reach in the database (without privileges), and the databases the key may reach. `chelabase gen types` reads it.",
    params(DatabaseParam),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "The schema, and the reachable databases as `databases`", body = DataSchemaResponse),
        (status = 400, description = "`user_token_with_secret_key`: a `Chelabase-User` token came with the secret key", body = DataErrorEnvelope),
        (status = 401, description = "`unauthorized`: the key is missing, invalid, revoked or not a secret key, or its branch is gone", body = DataErrorEnvelope),
        (status = 403, description = "`role_not_allowed`: the key's role can bypass row-level security", body = DataErrorEnvelope),
        (status = 404, description = "`unknown_database`: the database doesn't exist, isn't served or isn't on the key's list", body = DataErrorEnvelope),
        (status = 429, description = "`rate_limited`: too many requests", body = DataErrorEnvelope),
        (status = 500, description = "`internal`: an unexpected error; the message carries the request id", body = DataErrorEnvelope),
        (status = 503, description = "`unavailable`: the database can't be reached right now; retry after `Retry-After`", body = DataErrorEnvelope,
            headers(("Retry-After" = u32, description = "Seconds to wait before retrying"))),
        (status = 504, description = "`timeout`: reading the schema ran past the statement timeout", body = DataErrorEnvelope),
    ),
)]
async fn role_schema<S: ConnSource>(
    State(state): State<Arc<AppState<S>>>,
    Query(param): Query<DatabaseParam>,
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
    match state
        .engine
        .role_schema(&data, param.database.as_deref(), &rid)
        .await
    {
        Ok(schema) => axum::Json(schema).into_response(),
        Err(f) => public_failure(f, &rid),
    }
}

async fn branch_schema<S: ConnSource>(
    State(state): State<Arc<AppState<S>>>,
    Path(branch): Path<Uuid>,
    Query(param): Query<InternalSchemaParam>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = check_bearer(&state, &headers) {
        return e.into_response();
    }
    let rid = format!("internal-{}", Uuid::new_v4());
    match state
        .engine
        .branch_schema(
            branch,
            param.database.as_deref(),
            param.role.as_deref(),
            &rid,
        )
        .await
    {
        Ok(schema) => axum::Json(schema).into_response(),
        Err(f) => internal_failure(f, &rid),
    }
}

/// The orchestrator changed what the branch serves: forget it at once.
async fn invalidate<S: ConnSource>(
    State(state): State<Arc<AppState<S>>>,
    Path(branch): Path<Uuid>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = check_bearer(&state, &headers) {
        return e.into_response();
    }
    state.engine.invalidate(branch).await;
    StatusCode::NO_CONTENT.into_response()
}

/// `POST /internal/console/{branch_id}/query`'s body.
#[derive(Deserialize)]
struct ConsoleRequest {
    role: String,
    #[serde(default)]
    database: Option<String>,
    tree: Value,
}

/// Studio's console, as a plain role: the data API's answers, without a key's allow-list.
async fn console<S: ConnSource>(
    State(state): State<Arc<AppState<S>>>,
    Path(branch): Path<Uuid>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    if let Err(e) = check_bearer(&state, &headers) {
        return e.into_response();
    }
    let body = match body {
        Ok(body) => body,
        Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return ApiError::too_large().into_response();
        }
        Err(e) => return ApiError::invalid_query(e.body_text()).into_response(),
    };
    let request: ConsoleRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return ApiError::invalid_query(format!("the request: {e}")).into_response(),
    };
    let rid = format!("console-{}", Uuid::new_v4());
    match state
        .engine
        .console(
            branch,
            &request.role,
            request.database.as_deref(),
            &request.tree,
            &rid,
        )
        .await
    {
        Ok(result) => json_response(result),
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
