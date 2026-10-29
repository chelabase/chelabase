//! The HTTP edge. The public listener serves the gateway (`POST /data/v1/query`,
//! `GET /data/v1/schema`, `GET /data/v1/types`) and trusts only the gateway's identity token, minted for `rest`
//! with a data key's identity. The internal listener serves the orchestrator behind a
//! static bearer: `GET /internal/schema/{branch_id}` (`?database=&role=`),
//! `POST /internal/branches/{id}/invalidate`,
//! `POST /internal/console/{branch_id}/query` and `POST /internal/console/{branch_id}/cell`.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chelabase_auth::internal::{AuthMethod, DataIdentity, IDENTITY_HEADER, InternalVerifier};
use chelabase_query::openapi::{QueryRequest, QueryResult};
use chelabase_types::data_key::DataKeyKind;
use chelabase_types::table_api::ConsoleCellRequest;
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

use crate::conn::{ConnError, ConnSource};
use crate::error::ApiError;
use crate::exec::{CellError, Engine, Failure};
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
    /// The platform CA's certificate (`CHELABASE_COMPUTE_CA_FILE`, required): every compute
    /// is verified against it (U15).
    pub compute_ca: std::path::PathBuf,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Blank values count as unset.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let non_blank = |name: &str| get(name).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
        let env_or = |name: &str, default: &str| non_blank(name).unwrap_or_else(|| default.to_owned());
        // `CHELABASE_REST_INTERNAL_TOKEN` or the file `…_FILE` names (S27).
        let internal_token = chelabase_types::secret_env::lookup_secret(&get, BEARER)
            .map_err(anyhow::Error::msg)?
            .unwrap_or_else(|| chelabase_types::dev_defaults::dev_bearer(BEARER).to_owned());
        // The internal listener runs SQL on any branch for whoever holds this bearer: never on
        // a public development value, except in the workbench (`CHELABASE_DEV_DEFAULTS=1`,
        // self-hosted mode only). Checked before anything binds.
        let mode = chelabase_types::mode::Mode::parse(get(chelabase_types::mode::Mode::ENV).as_deref())
            .map_err(anyhow::Error::msg)?;
        chelabase_types::dev_defaults::DevDefaults::from_lookup(&get)
            .map_err(anyhow::Error::msg)?
            .check_bearers(
                &chelabase_types::mode_policy::ModePolicy::new(mode),
                &[(BEARER, internal_token.as_str())],
            )
            .map_err(anyhow::Error::msg)?;
        Ok(Config {
            addr: env_or("CHELABASE_REST_ADDR", "127.0.0.1:8085").parse()?,
            internal_addr: env_or("CHELABASE_REST_INTERNAL_ADDR", "127.0.0.1:8086").parse()?,
            internal_url: env_or("CHELABASE_INTERNAL_URL", "http://127.0.0.1:8083"),
            internal_token,
            max_connections: env_or("CHELABASE_REST_MAX_CONNECTIONS", "200").parse()?,
            max_response_bytes: env_or(
                "CHELABASE_REST_MAX_RESPONSE_BYTES",
                &crate::exec::DEFAULT_MAX_RESPONSE_BYTES.to_string(),
            )
            .parse()?,
            compute_ca: non_blank("CHELABASE_COMPUTE_CA_FILE")
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "CHELABASE_COMPUTE_CA_FILE is unset: rest verifies every compute's TLS \
                         against the platform CA certificate"
                    )
                })?
                .into(),
        })
    }
}

/// The internal listener's bearer, both ways.
const BEARER: &str = "CHELABASE_REST_INTERNAL_TOKEN";

/// The public routes with their OpenAPI annotations ([`crate::openapi::openapi`]).
pub fn api_router<S: ConnSource>() -> OpenApiRouter<Arc<AppState<S>>> {
    OpenApiRouter::new()
        .routes(routes!(query))
        .routes(routes!(role_schema))
        .routes(routes!(types))
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
        .route("/internal/types/{branch_id}", get(internal_types::<S>))
        .route(
            "/internal/branches/{branch_id}/invalidate",
            post(invalidate::<S>),
        )
        .route("/internal/console/{branch_id}/query", post(console::<S>))
        .route("/internal/console/{branch_id}/cell", post(console_cell::<S>))
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
        (status = 403, description = "`permission_denied`, `role_not_allowed`: Postgres refused the role (`permission_denied`), or the secret key's role is owner-level or has system privileges: superuser, CREATEROLE, CREATEDB, REPLICATION, or a member of a superuser role, `neon_superuser` or a system role (`role_not_allowed`; BYPASSRLS alone is allowed)", body = DataErrorEnvelope),
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
        (status = 403, description = "`role_not_allowed`: the role is owner-level or has system privileges: superuser, CREATEROLE, CREATEDB, REPLICATION, or a member of a superuser role, `neon_superuser` or a system role (BYPASSRLS alone is allowed)", body = DataErrorEnvelope),
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

/// `GET /data/v1/types`' query, for the document: [`types_options`] parses it.
#[derive(utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[allow(dead_code)]
struct TypesParams {
    /// `ts` (the default): the TypeScript module `chelabase gen types` writes. `json`: the
    /// runtime `types` map alone, exactly what the module's `types` constant holds.
    #[param(inline)]
    format: Option<TypesFormat>,
    /// Generate this database (not the main one) as `Database`. It must be served and, when
    /// the key has a database list, on it. Not with `all_databases`.
    database: Option<String>,
    /// `true`: every database the key reaches, as `Databases` and a `types` map keyed by
    /// name (main as `main`). With only main reachable, the one-database module.
    all_databases: Option<bool>,
}

#[derive(utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)]
enum TypesFormat {
    Ts,
    Json,
}

/// `format`, `database` and `all_databases` with `gen types`' meanings; anything else, a
/// repeat, or `database` with `all_databases=true` is refused (`invalid_query`).
fn types_options(
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
) -> Result<(chelabase_typegen::Which, chelabase_typegen::Format), ApiError> {
    types_options_with(query, None)
}

/// [`types_options`], also taking one `role` into `role` when it is given (the internal
/// route's).
fn types_options_with(
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
    mut role: Option<&mut Option<String>>,
) -> Result<(chelabase_typegen::Which, chelabase_typegen::Format), ApiError> {
    let Query(pairs) = query.map_err(|_| ApiError::invalid_query("the query string isn't valid"))?;
    let (mut format, mut database, mut all) = (None, None, None);
    for (name, value) in pairs {
        let slot = match (name.as_str(), role.as_deref_mut()) {
            ("format", _) => &mut format,
            ("database", _) => &mut database,
            ("all_databases", _) => &mut all,
            ("role", Some(role)) => role,
            _ => {
                return Err(ApiError::invalid_query(
                    "the types route takes `format`, `database` and `all_databases` only",
                ));
            }
        };
        if slot.replace(value).is_some() {
            return Err(ApiError::invalid_query(format!("`{name}` is given more than once")));
        }
    }
    let format = match format.as_deref() {
        None | Some("ts") => chelabase_typegen::Format::Ts,
        Some("json") => chelabase_typegen::Format::Json,
        Some(_) => return Err(ApiError::invalid_query("`format` is `ts` or `json`")),
    };
    let all = match all.as_deref() {
        None | Some("false") => false,
        Some("true") => true,
        Some(_) => return Err(ApiError::invalid_query("`all_databases` is `true` or `false`")),
    };
    let which = chelabase_typegen::Which::from_options(database, all)
        .map_err(|e| ApiError::invalid_query(e.to_string()))?;
    Ok((which, format))
}

/// The ETag of a body: a strong tag, its SHA-256 in hex. It depends on the bytes alone.
fn types_etag(body: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(body);
    let mut tag = String::with_capacity(66);
    tag.push('"');
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(tag, "{b:02x}");
    }
    tag.push('"');
    tag
}

/// Whether `If-None-Match` names `etag` (weak comparison, as RFC 9110 has for it), or is `*`.
fn none_match(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .any(|t| t == "*" || t.strip_prefix("W/").unwrap_or(t) == etag)
}

/// `chelabase gen types`' output for the key, built from the schema `GET /data/v1/schema`
/// gives it, through the same generator.
#[utoipa::path(
    get,
    path = "/data/v1/types",
    operation_id = "getDataTypes",
    tag = "data",
    summary = "The generated types for the key's schema",
    description = "What `chelabase gen types` writes for the same secret key and options, byte for byte: the TypeScript module (`Database` and the runtime `types` map for `@chelabase/client`), or with `format=json` the runtime `types` map alone. It is built from exactly what `GET /data/v1/schema` gives the key. The `ETag` is the body's hash: send it back as `If-None-Match` for a 304 while the schema is unchanged. Not cached (`Cache-Control: no-store`): the schema follows the key.",
    params(TypesParams),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "The module (`format=ts`, `text/typescript`) or the runtime `types` map (`format=json`)",
            content((String = "text/typescript"), (Object = "application/json")),
            headers(
                ("ETag" = String, description = "A strong tag: the body's SHA-256"),
                ("Cache-Control" = String, description = "`no-store`"),
            )),
        (status = 304, description = "`If-None-Match` names the current `ETag`: the body is unchanged (and not sent)",
            headers(
                ("ETag" = String, description = "The current tag"),
                ("Cache-Control" = String, description = "`no-store`"),
            )),
        (status = 400, description = "`invalid_query`, `user_token_with_secret_key`: an option isn't `format=ts|json`, `database=<name>` or `all_databases=true|false`, is repeated, or `database` came with `all_databases=true` (`invalid_query`); or a `Chelabase-User` token came with the secret key", body = DataErrorEnvelope),
        (status = 401, description = "`unauthorized`: the key is missing, invalid, revoked or not a secret key, or its branch is gone", body = DataErrorEnvelope),
        (status = 403, description = "`role_not_allowed`: the role is owner-level or has system privileges: superuser, CREATEROLE, CREATEDB, REPLICATION, or a member of a superuser role, `neon_superuser` or a system role (BYPASSRLS alone is allowed)", body = DataErrorEnvelope),
        (status = 404, description = "`unknown_database`: the database (or, with `all_databases`, main) doesn't exist, isn't served or isn't on the key's list", body = DataErrorEnvelope),
        (status = 409, description = "`conflict`: the schema can't be typed: a schema the role reaches has a dot in its name (rename it, or revoke the role's USAGE on it)", body = DataErrorEnvelope),
        (status = 429, description = "`rate_limited`: too many requests", body = DataErrorEnvelope),
        (status = 500, description = "`internal`: an unexpected error; the message carries the request id", body = DataErrorEnvelope),
        (status = 503, description = "`unavailable`: the database can't be reached right now; retry after `Retry-After`", body = DataErrorEnvelope,
            headers(("Retry-After" = u32, description = "Seconds to wait before retrying"))),
        (status = 504, description = "`timeout`: reading the schema ran past the statement timeout", body = DataErrorEnvelope),
    ),
)]
async fn types<S: ConnSource>(
    State(state): State<Arc<AppState<S>>>,
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let (data, rid) = match identity(&state, &headers) {
        Ok(id) => id,
        Err(e) => return e.into_response(),
    };
    // As on the schema route: the gateway refuses public keys here; check again.
    if data.kind != DataKeyKind::Secret {
        return ApiError::unauthorized("the schema needs a secret key").into_response();
    }
    let (which, format) = match types_options(query) {
        Ok(options) => options,
        Err(e) => return e.into_response(),
    };
    let (engine, data_ref, rid_ref) = (&state.engine, &data, rid.as_str());
    types_answer(&which, format, &headers, rid_ref, |database: Option<String>| async move {
        engine
            .role_schema(data_ref, database.as_deref(), rid_ref)
            .await
            .map_err(|f| public_failure(f, rid_ref))
    })
    .await
}

/// The types of the schemas `schema` gives (one call per database, `gen types`' own plan),
/// rendered, with the ETag, `no-store` and the 304: the public and the internal route's
/// shared answer, so both are the same bytes for the same role.
async fn types_answer<F, Fut>(
    which: &chelabase_typegen::Which,
    format: chelabase_typegen::Format,
    headers: &HeaderMap,
    rid: &str,
    schema: F,
) -> Response
where
    F: FnMut(Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<Value, Response>>,
{
    let mut schema = schema;
    let schemas = chelabase_typegen::collect(which, |database: Option<String>| {
        let fetched = schema(database);
        async move {
            serde_json::from_value::<chelabase_typegen::Schema>(fetched.await?).map_err(|e| {
                ApiError::internal(rid, format!("the schema isn't the generator's: {e}")).into_response()
            })
        }
    })
    .await;
    let schemas = match schemas {
        Ok(schemas) => schemas,
        Err(resp) => return resp,
    };
    let body = match chelabase_typegen::render(&schemas, format) {
        Ok(body) => body,
        Err(e) => {
            // The names in it are the role's own schema's.
            let mut message = e.to_string();
            let mut source = std::error::Error::source(&e);
            while let Some(s) = source {
                message.push_str(": ");
                message.push_str(&s.to_string());
                source = s.source();
            }
            return ApiError::new(StatusCode::CONFLICT, "conflict", message).into_response();
        }
    };
    let etag = types_etag(body.as_bytes());
    let content_type = match format {
        chelabase_typegen::Format::Ts => "text/typescript; charset=utf-8",
        chelabase_typegen::Format::Json => "application/json",
    };
    let cache = [(header::ETAG, etag.clone()), (header::CACHE_CONTROL, "no-store".to_string())];
    if none_match(headers, &etag) {
        return (StatusCode::NOT_MODIFIED, cache).into_response();
    }
    (cache, [(header::CONTENT_TYPE, content_type)], body).into_response()
}

/// `GET /internal/types/{branch}?role=&format=&database=&all_databases=`: what
/// `GET /data/v1/types` gives a secret key of `role` with no database list, through the same
/// code. The orchestrator's types download, after its member access check; the role is held
/// to the console's check (it must exist and not be owner-level).
async fn internal_types<S: ConnSource>(
    State(state): State<Arc<AppState<S>>>,
    Path(branch): Path<Uuid>,
    query: Result<Query<Vec<(String, String)>>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = check_bearer(&state, &headers) {
        return e.into_response();
    }
    let mut role = None;
    let (which, format) = match types_options_with(query, Some(&mut role)) {
        Ok(options) => options,
        Err(e) => return e.into_response(),
    };
    let Some(role) = role.filter(|r| !r.is_empty()) else {
        return ApiError::invalid_query("`role` is required").into_response();
    };
    let rid = format!("internal-{}", Uuid::new_v4());
    let (engine, role_ref, rid_ref) = (&state.engine, role.as_str(), rid.as_str());
    types_answer(&which, format, &headers, rid_ref, |database: Option<String>| async move {
        engine
            .key_role_schema(branch, role_ref, database.as_deref(), rid_ref)
            .await
            .map_err(|f| internal_failure(f, rid_ref))
    })
    .await
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

/// The full-cell read as a role (Q1, P40): `POST /internal/console/{branch_id}/cell`. Answers
/// the management API's `CellValue` and error body, as the orchestrator's member-role path
/// does.
async fn console_cell<S: ConnSource>(
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
    let request: ConsoleCellRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => return ApiError::invalid_query(format!("the request: {e}")).into_response(),
    };
    let rid = format!("console-{}", Uuid::new_v4());
    match state.engine.console_cell(branch, &request, &rid).await {
        Ok(cell) => axum::Json(cell).into_response(),
        Err(CellError::Cell(e)) => e.into_response(),
        Err(CellError::Failure(f)) => internal_failure(f, &rid),
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

#[cfg(test)]
mod types_tests {
    use super::*;

    fn with(values: &[&str]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for v in values {
            h.append(header::IF_NONE_MATCH, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn the_etag_is_the_bodys_hash() {
        let tag = types_etag(b"x");
        assert_eq!(tag, types_etag(b"x"));
        assert_ne!(tag, types_etag(b"y"));
        assert_eq!(tag.len(), 66);
        assert!(tag.starts_with('"') && tag.ends_with('"'));
        // SHA-256 of "x".
        assert_eq!(
            tag,
            "\"2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881\""
        );
    }

    #[test]
    fn if_none_match_compares_weakly_and_takes_lists() {
        let tag = types_etag(b"x");
        assert!(none_match(&with(&[&tag]), &tag));
        assert!(none_match(&with(&[&format!("W/{tag}")]), &tag));
        assert!(none_match(&with(&[&format!("\"a\", {tag}")]), &tag));
        assert!(none_match(&with(&["\"a\"", &tag]), &tag));
        assert!(none_match(&with(&["*"]), &tag));
        assert!(!none_match(&with(&[]), &tag));
        assert!(!none_match(&with(&["\"a\""]), &tag));
        assert!(!none_match(&with(&[&tag[1..65]]), &tag), "unquoted");
    }
}
