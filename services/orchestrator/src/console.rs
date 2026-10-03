//! Studio's console: one data API request on a branch, as the database's owner or as a named
//! role.
//!
//! - **The owner** (`role: null`) runs here, through `cheladb-query`, on a small pool per
//!   (branch, database) that connects to the compute's published Postgres port with the
//!   owner password the orchestrator already holds sealed. The pool's key carries a
//!   fingerprint of the sealed password and the compute's address (P4): a reset, a password
//!   set in SQL (which clears the sealed value) or a moved compute all pick a new pool, and
//!   the old one idles out.
//! - **Any other role** goes to rest's internal console route, after the worker's role check
//!   (once per branch and role, remembered for five minutes). rest never sees an owner
//!   password: it runs as `authenticator` and `SET ROLE`.
//!
//! Every write (an insert, update, delete or call, or a batch holding one) is logged at
//! `info` with the platform user, branch, database, role and operation kind, never a value.
//!
//! `GET …/console/schema` answers what the console may show the same way: the owner's
//! schema read here on the owner pool, any other role's from rest's internal schema route.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::State,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use cheladb_metadata::{BranchDatabase, BranchKind, BranchStatus, DataApiState, DatabaseOrigin};
use cheladb_neon::ComputeLocator;
use cheladb_query::{Op, QueryCache, QueryFailure, RunAs, RunSettings};
use cheladb_secrets::{Purpose, open_credential};
use cheladb_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use cheladb_types::data_api_schema::DataApiSchema;
use cheladb_types::{ApiPath, BranchId};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions, PgSslMode};
use tracing::{error, info, warn};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::branches::{conflict, load_branch, parse_body, unprocessable};
use crate::{
    ApiBytes, AppState, AuthPrincipal, BACKUP_REFUSAL, JobPort, MetadataPort, ResetError,
    RestInternal, load_owned, roles,
};

/// Connections one owner pool holds at most.
pub const OWNER_POOL_CONNECTIONS: u32 = 2;
/// An owner connection (and an owner pool) unused this long is closed.
pub const OWNER_POOL_IDLE: Duration = Duration::from_secs(60);
/// Longest wait for an owner connection.
const OWNER_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a role the worker accepted is remembered.
pub const ROLE_CHECK_TTL: Duration = Duration::from_secs(300);
/// The owner's connections, as `pg_stat_activity` shows them.
pub const APPLICATION_NAME: &str = "cheladb-console";
/// Seconds a 503 asks the client to wait (the data API's value).
const RETRY_AFTER_SECS: u32 = 2;
/// Run on every owner connection handed back to its pool: the next request starts as the
/// owner with the connection's defaults, holding no advisory lock and no temporary table (a
/// function the owner called may have left any of them). An error closes the connection.
pub const OWNER_RESET_SQL: &str =
    "SET ROLE NONE; RESET ALL; SELECT pg_advisory_unlock_all(); DISCARD TEMP";
/// Longest wait for rest's schema of a role.
const SCHEMA_TIMEOUT: Duration = Duration::from_secs(15);
/// The owner password isn't known to Cheladb.
pub const OWNER_PASSWORD_UNKNOWN: &str = "reset the owner's password to view as owner";

/// A future the [`PostgresAddr`] port returns.
pub type AddrFuture<'a> =
    Pin<Box<dyn Future<Output = anyhow::Result<Option<SocketAddr>>> + Send + 'a>>;

/// Where an endpoint's compute publishes Postgres for host services (`None`: no compute
/// running). Implemented for every [`ComputeLocator`].
pub trait PostgresAddr: Send + Sync + 'static {
    fn postgres_addr<'a>(&'a self, endpoint_id: &'a str) -> AddrFuture<'a>;
}

impl<L: ComputeLocator> PostgresAddr for L {
    fn postgres_addr<'a>(&'a self, endpoint_id: &'a str) -> AddrFuture<'a> {
        Box::pin(ComputeLocator::postgres_addr(self, endpoint_id))
    }
}

/// An owner pool's key. `fingerprint` is SHA-256 over the sealed owner password and the
/// compute's `host:port`, so a new password or a moved compute is a new pool.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OwnerPoolKey {
    pub branch: BranchId,
    pub database: String,
    pub fingerprint: [u8; 32],
}

impl OwnerPoolKey {
    pub fn new(branch: BranchId, database: &str, sealed_password: &str, addr: SocketAddr) -> Self {
        let mut hash = Sha256::new();
        hash.update(sealed_password.as_bytes());
        hash.update(b"\n");
        hash.update(addr.to_string().as_bytes());
        OwnerPoolKey {
            branch,
            database: database.to_owned(),
            fingerprint: hash.finalize().into(),
        }
    }
}

/// The console's state: the owner pools, the owner's schema and shape caches, and the roles
/// the worker accepted.
pub struct Console {
    pub(crate) pools: moka::future::Cache<OwnerPoolKey, PgPool>,
    pub(crate) cache: QueryCache,
    checked_roles: moka::sync::Cache<(BranchId, String), ()>,
}

impl Default for Console {
    fn default() -> Self {
        Console {
            pools: moka::future::Cache::builder()
                .max_capacity(10_000)
                .time_to_idle(OWNER_POOL_IDLE)
                .eviction_listener(|_, pool: PgPool, _| {
                    // Close its connections now rather than when the last clone drops.
                    if let Ok(rt) = tokio::runtime::Handle::try_current() {
                        rt.spawn(async move { pool.close().await });
                    }
                })
                .build(),
            cache: QueryCache::default(),
            checked_roles: moka::sync::Cache::builder()
                .max_capacity(10_000)
                .time_to_live(ROLE_CHECK_TTL)
                .build(),
        }
    }
}

impl Console {
    /// The owner pool for `key`, made with `options` on a miss. It connects lazily.
    async fn owner_pool(&self, key: OwnerPoolKey, options: PgConnectOptions) -> PgPool {
        self.pools
            .get_with(key, async move {
                PgPoolOptions::new()
                    .max_connections(OWNER_POOL_CONNECTIONS)
                    .min_connections(0)
                    .idle_timeout(OWNER_POOL_IDLE)
                    .acquire_timeout(OWNER_ACQUIRE_TIMEOUT)
                    .after_release(|conn, _| {
                        Box::pin(async move {
                            sqlx::Executor::execute(conn, sqlx::raw_sql(OWNER_RESET_SQL)).await?;
                            Ok(true)
                        })
                    })
                    .connect_lazy_with(options)
            })
            .await
    }

    /// Forgets the owner's cached schemas and plans of `branch` (its databases changed).
    pub fn invalidate_branch(&self, branch: BranchId) {
        self.cache.invalidate_branch(branch.as_uuid());
    }
}

/// `POST …/console/query`'s body.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ConsoleRequest {
    /// The role to run as; null runs as the database's owner.
    #[serde(default)]
    pub role: Option<String>,
    /// The Postgres database; null is the branch's main database. The tree may name it
    /// instead (both must then agree).
    #[serde(default)]
    pub database: Option<String>,
    /// A data API request: the same JSON `POST /data/v1/query` takes.
    #[schema(value_type = Object)]
    pub tree: Value,
}

/// The kind of a write, for the audit log; `None` for a read. A batch is a write when one of
/// its operations is.
pub fn write_kind(op: &Op) -> Option<&'static str> {
    match op {
        Op::Select(_) => None,
        Op::Insert(_) => Some("insert"),
        Op::Update(_) => Some("update"),
        Op::Delete(_) => Some("delete"),
        Op::Call(_) => Some("call"),
        Op::Batch(ops) => ops
            .iter()
            .any(|op| write_kind(op).is_some())
            .then_some("batch"),
    }
}

/// A failure in the data API's body shape (`code`, `message`, and Postgres's `detail`,
/// `hint` and `sqlstate` when there are any); a 503 asks the client to retry.
fn data_api_failure(f: QueryFailure) -> Response {
    let mut error = Map::new();
    error.insert("code".into(), json!(f.code));
    error.insert("message".into(), json!(f.message));
    for (key, value) in [
        ("detail", f.detail),
        ("hint", f.hint),
        ("sqlstate", f.sqlstate),
    ] {
        if let Some(value) = value {
            error.insert(key.into(), Value::String(value));
        }
    }
    let mut response = (f.status, axum::Json(json!({ "error": error }))).into_response();
    if f.status == StatusCode::SERVICE_UNAVAILABLE {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(RETRY_AFTER_SECS));
    }
    response
}

/// The data API's JSON text, sent on unparsed.
fn json_text(status: StatusCode, body: impl Into<axum::body::Body>) -> Response {
    let mut response = (status, body.into()).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

pub(crate) fn data_api_disabled(name: &str) -> ApiError {
    conflict(
        ErrorCode::DataApiDisabled,
        format!("the data API is off for the database {name:?}; enable it to use the console"),
    )
}

pub(crate) fn database_not_found() -> ApiError {
    ApiError::not_found("database not found")
}

pub(crate) fn unavailable(message: &str) -> ApiError {
    ApiError::new(503, ErrorCode::Unavailable, message).retry_after(RETRY_AFTER_SECS)
}

/// The branch's database the request names (`None`: its main one); the system database is
/// never served, so it's missing here too.
pub(crate) fn pick_database(rows: Vec<BranchDatabase>, name: Option<&str>) -> Option<BranchDatabase> {
    rows.into_iter().find(|d| match name {
        None => d.origin == DatabaseOrigin::Main,
        Some(name) => d.name == name && d.origin != DatabaseOrigin::System,
    })
}

pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new()
        .routes(routes!(console_query))
        .routes(routes!(console_schema))
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/branches/{branch_id}/console/query",
    operation_id = "consoleQuery",
    tag = "data-api",
    summary = "Run a data API request from the console",
    description = "Runs one data API request (`tree`, the JSON `POST /data/v1/query` takes; every operation, batches included) on a branch, in one transaction with the data API's `max_rows` and statement timeout. `role: null` runs as the database's owner; any other role runs as that role, after a check that it exists and can't bypass row-level security. The answer is the data API's: its JSON on success, and its error body (`invalid_query`, `unknown_table`, `permission_denied`, `conflict`, …) when the request fails in the database. Every write is logged without its values.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
    ),
    request_body = ConsoleRequest,
    responses(
        (status = 200, description = "The data API's answer: the rows, the written rows or count, a call's result, or a batch's results in order", body = serde_json::Value),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON; the data API's `invalid_query` or `invalid_value`: the tree is malformed, the request and its tree name different databases, or Postgres refused a value", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Cheladb tenant; `role_refused`: the worker's check refused the role: it doesn't exist, or it can bypass row-level security (the message says which); the data API's `permission_denied` or `role_not_allowed`", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the Postgres database doesn't exist; the data API's `unknown_table` or `unknown_function`, or `unknown_role` when the role was dropped after the check (a passed check is remembered five minutes)", body = ErrorEnvelope),
        (status = 409, description = "`data_api_disabled`: the Postgres database isn't served by the data API (enable it first); `owner_password_unknown`: the owner's password was set in SQL (reset it to view as owner); `backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready; the data API's `conflict` or `retry`", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit, or the answer is over the data API's response cap", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the body is valid JSON but not the console's shape, `role` is empty, or `role` names the owner (use `role: null`)", body = ErrorEnvelope),
        (status = 500, description = "`credentials_unavailable`: the stored owner password can't be opened with the current secrets key; `internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: the compute or the data API can't be reached right now (`Retry-After`); `worker_unavailable`: the worker can't be reached", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the statement or transaction timeout passed, or the worker didn't answer in time", body = ErrorEnvelope),
    ),
)]
async fn console_query<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
    ApiBytes(body): ApiBytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    console(&state, &principal, id, branch_id, &body).await
}

/// The value, or the error to answer with instead.
macro_rules! or_answer {
    ($result:expr) => {
        match $result {
            Ok(value) => value,
            Err(err) => return IntoResponse::into_response(err),
        }
    };
}

/// Every step of the console route.
async fn console<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    id: Uuid,
    branch_id: Uuid,
    body: &axum::body::Bytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = or_answer!(load_owned(state, principal, id).await);
    let branch = or_answer!(load_branch(state, &record, branch_id).await);
    if branch.kind == BranchKind::Backup {
        return conflict(ErrorCode::BackupBranch, BACKUP_REFUSAL).into_response();
    }
    if branch.status != BranchStatus::Ready {
        return conflict(ErrorCode::BranchNotReady, "branch is not ready").into_response();
    }
    let request: ConsoleRequest = or_answer!(parse_body(body));
    if request.role.as_deref().is_some_and(str::is_empty) {
        return unprocessable("role must not be empty (null runs as the owner)").into_response();
    }
    if request.role.as_deref() == Some(roles::owner_name(&record)) {
        return unprocessable("use role: null to run as the owner").into_response();
    }
    // The tree is checked here for both paths: a bad one never reaches a database or rest.
    let tree =
        or_answer!(serde_json::to_vec(&request.tree).map_err(|e| {
            data_api_failure(QueryFailure::invalid_query(format!("the tree: {e}")))
        }));
    let parsed = or_answer!(
        cheladb_query::parse(&tree).map_err(|e| data_api_failure(QueryFailure::from(e)))
    );
    let name = match (request.database.as_deref(), parsed.database.as_deref()) {
        (Some(a), Some(b)) if a != b => {
            return data_api_failure(QueryFailure::invalid_query(
                "the request and its tree name different databases",
            ));
        }
        (a, b) => a.or(b),
    };
    let rows = or_answer!(
        state
            .metadata
            .branch_databases(branch.id)
            .await
            .map_err(|err| {
                error!("list branch databases: {err}");
                ApiError::internal()
            })
    );
    let Some(row) = pick_database(rows, name) else {
        return database_not_found().into_response();
    };
    if row.data_api != DataApiState::On {
        return data_api_disabled(&row.name).into_response();
    }

    let owner = roles::owner_name(&record).to_owned();
    let (role, response) = match request.role {
        None => {
            let response =
                run_as_owner(state, principal, &record, &branch, &row, &owner, &parsed.op).await;
            (owner, response)
        }
        Some(role) => {
            or_answer!(check_role(state, branch.id, &role).await);
            let timeout = Duration::from_millis(record.statement_timeout_ms.max(0) as u64)
                + Duration::from_secs(15);
            let response =
                forward(state, branch.id, &role, &row.name, &request.tree, timeout).await;
            (role, response)
        }
    };
    if let Some(op) = write_kind(&parsed.op) {
        info!(
            request_id = %principal.request_id,
            user = %principal.sub,
            branch = %branch.id,
            database = %row.name,
            role = %role,
            op = %op,
            status = response.status().as_u16(),
            "console write"
        );
    }
    response
}

/// Where and as whom to reach a branch's compute as its owner.
pub(crate) struct OwnerTarget {
    pub addr: SocketAddr,
    pub user: String,
    pub password: String,
    pub sealed: String,
}

/// The owner's address and credentials on `branch`: 409 `owner_password_unknown` when
/// Cheladb doesn't know the owner password, 409 `branch_not_ready`, 500
/// `credentials_unavailable` when the sealed password can't be opened, 503 when the compute
/// can't be found.
pub(crate) async fn owner_target<M, J>(
    state: &AppState<M, J>,
    branch: &cheladb_metadata::BranchRecord,
    owner: &str,
) -> Result<OwnerTarget, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    // A password set in SQL leaves nothing sealed: Cheladb doesn't know it.
    let Some(sealed) = branch
        .owner_password_enc
        .as_deref()
        .filter(|s| !s.is_empty())
    else {
        return Err(conflict(
            ErrorCode::OwnerPasswordUnknown,
            OWNER_PASSWORD_UNKNOWN,
        ));
    };
    let Some(endpoint_id) = branch.endpoint_id.as_deref() else {
        return Err(conflict(ErrorCode::BranchNotReady, "branch is not ready"));
    };
    let password = match open_credential(
        &state.secrets,
        &branch.id.to_string(),
        Purpose::Owner,
        sealed,
    ) {
        Ok(password) => password,
        Err(err) => {
            error!(branch_id = %branch.id, "decrypt owner password: {err}");
            return Err(ApiError::new(
                500,
                ErrorCode::CredentialsUnavailable,
                "stored credentials can't be decrypted with the current secrets key; \
                 restore the original CHELADB_SECRETS_KEY file",
            ));
        }
    };
    let addr = match state.locator.postgres_addr(endpoint_id).await {
        Ok(Some(addr)) => addr,
        Ok(None) => {
            return Err(unavailable("the branch's compute isn't running; try again"));
        }
        Err(err) => {
            warn!(branch_id = %branch.id, "locate the compute: {err:#}");
            return Err(unavailable(
                "the branch's compute can't be reached right now; try again",
            ));
        }
    };
    Ok(OwnerTarget {
        addr,
        user: owner.to_string(),
        password,
        sealed: sealed.to_string(),
    })
}

/// The owner pool of `row`'s database on `branch`; errors as [`owner_target`].
async fn owner_pool_for<M, J>(
    state: &AppState<M, J>,
    branch: &cheladb_metadata::BranchRecord,
    row: &BranchDatabase,
    owner: &str,
) -> Result<PgPool, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let target = owner_target(state, branch, owner).await?;
    let options = PgConnectOptions::new()
        .host(&target.addr.ip().to_string())
        .port(target.addr.port())
        .username(&target.user)
        .password(&target.password)
        .database(&row.name)
        .ssl_mode(PgSslMode::Prefer)
        .application_name(APPLICATION_NAME);
    Ok(state
        .console
        .owner_pool(
            OwnerPoolKey::new(branch.id, &row.name, &target.sealed, target.addr),
            options,
        )
        .await)
}

/// Runs `op` as the database's owner on the owner pool, in one transaction.
async fn run_as_owner<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    record: &cheladb_metadata::DatabaseRecord,
    branch: &cheladb_metadata::BranchRecord,
    row: &BranchDatabase,
    owner: &str,
    op: &Op,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let pool = match owner_pool_for(state, branch, row, owner).await {
        Ok(pool) => pool,
        Err(err) => return err.into_response(),
    };
    let settings = RunSettings {
        max_rows: i64::from(record.max_rows),
        statement_timeout_ms: i64::from(record.statement_timeout_ms),
        default_schema: row.default_schema.clone(),
    };
    let rid = principal.request_id.as_str();
    let result: Result<String, QueryFailure> = async {
        let mut tx = pool
            .begin()
            .await
            .map_err(|e| QueryFailure::from_sqlx(e, rid))?;
        let out = cheladb_query::run(
            &mut tx,
            &state.console.cache,
            (branch.id.as_uuid(), &row.name),
            RunAs::Owner,
            &settings,
            cheladb_query::engine::DEFAULT_MAX_RESPONSE_BYTES,
            op,
            rid,
        )
        .await?;
        tx.commit()
            .await
            .map_err(|e| QueryFailure::from_sqlx(e, rid))?;
        Ok(out)
    }
    .await;
    match result {
        Ok(out) => json_text(StatusCode::OK, out),
        Err(failure) => data_api_failure(failure),
    }
}

/// `GET …/console/schema`'s query.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ConsoleSchemaQuery {
    /// The Postgres database; the branch's main database when absent. It must be served by the
    /// data API.
    pub database: Option<String>,
    /// The role whose view to describe; absent: the database's owner.
    pub role: Option<String>,
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/console/schema",
    operation_id = "getConsoleSchema",
    tag = "data-api",
    summary = "The schema the console can show, as the owner or a role",
    description = "What the console may show for one Postgres database of a branch, in the data API schema's shape (schemas, tables with their primary keys and the role's privileges, functions, enums). Without `role`, everything the database's owner reaches; with it, what that role reaches, after the same check the console's queries pass (it exists and can't bypass row-level security).",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ConsoleSchemaQuery,
    ),
    responses(
        (status = 200, description = "The schemas, tables, functions and enums the owner (or the role) reaches; each table's `privileges` has one entry, for that role", body = DataApiSchema),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Cheladb tenant; `role_refused`: the worker's check refused the role: it doesn't exist, or it can bypass row-level security (the message says which); the data API's `role_not_allowed`", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the Postgres database doesn't exist; the data API's `unknown_role` when the role was dropped after the check (a passed check is remembered five minutes)", body = ErrorEnvelope),
        (status = 409, description = "`data_api_disabled`: the Postgres database isn't served by the data API; `owner_password_unknown`: the owner's password was set in SQL (reset it to view as owner); `backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the query string is malformed, `role` is empty, or `role` names the owner (omit it)", body = ErrorEnvelope),
        (status = 500, description = "`credentials_unavailable`: the stored owner password can't be opened with the current secrets key; `internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: the compute or the data API can't be reached right now, or the database's data API schema is being reinstalled (`Retry-After`); `worker_unavailable`: the worker can't be reached", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the worker didn't answer in time", body = ErrorEnvelope),
    ),
)]
async fn console_schema<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
    query: Result<axum::extract::Query<ConsoleSchemaQuery>, axum::extract::rejection::QueryRejection>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = or_answer!(load_owned(&state, &principal, id).await);
    let branch = or_answer!(load_branch(&state, &record, branch_id).await);
    if branch.kind == BranchKind::Backup {
        return conflict(ErrorCode::BackupBranch, BACKUP_REFUSAL).into_response();
    }
    if branch.status != BranchStatus::Ready {
        return conflict(ErrorCode::BranchNotReady, "branch is not ready").into_response();
    }
    let Ok(axum::extract::Query(query)) = query else {
        return unprocessable("the query string is malformed").into_response();
    };
    let owner = roles::owner_name(&record).to_owned();
    match query.role.as_deref() {
        Some("") => {
            return unprocessable("role must not be empty (omit it to view the schema as the owner)")
                .into_response();
        }
        Some(role) if role == owner => {
            return unprocessable("omit role to view the schema as the owner").into_response();
        }
        _ => {}
    }
    let rows = or_answer!(
        state
            .metadata
            .branch_databases(branch.id)
            .await
            .map_err(|err| {
                error!("list branch databases: {err}");
                ApiError::internal()
            })
    );
    let Some(row) = pick_database(rows, query.database.as_deref()) else {
        return database_not_found().into_response();
    };
    if row.data_api != DataApiState::On {
        return data_api_disabled(&row.name).into_response();
    }
    let response = match query.role {
        None => owner_schema(&state, &principal, &record, &branch, &row, &owner).await,
        Some(role) => {
            or_answer!(check_role(&state, branch.id, &role).await);
            let reply = state.rest.role_schema(branch.id, &role, &row.name).await;
            pass_on(&state, branch.id, &row.name, reply).await
        }
    };
    crate::data_keys::no_store(response)
}

/// Everything the owner reaches in `row`'s database, read on the owner pool through the
/// console's schema cache.
async fn owner_schema<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    record: &cheladb_metadata::DatabaseRecord,
    branch: &cheladb_metadata::BranchRecord,
    row: &BranchDatabase,
    owner: &str,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let pool = match owner_pool_for(state, branch, row, owner).await {
        Ok(pool) => pool,
        Err(err) => return err.into_response(),
    };
    let settings = RunSettings {
        max_rows: i64::from(record.max_rows),
        statement_timeout_ms: i64::from(record.statement_timeout_ms),
        default_schema: row.default_schema.clone(),
    };
    let rid = principal.request_id.as_str();
    let result: Result<Value, QueryFailure> = async {
        let mut tx = pool
            .begin()
            .await
            .map_err(|e| QueryFailure::from_sqlx(e, rid))?;
        let (schema, view) = cheladb_query::schema_for(
            &mut tx,
            &state.console.cache,
            (branch.id.as_uuid(), &row.name),
            RunAs::Owner,
            &settings,
            rid,
        )
        .await?;
        tx.commit()
            .await
            .map_err(|e| QueryFailure::from_sqlx(e, rid))?;
        Ok(schema.to_json(&row.default_schema, &[(owner, &view.visibility)], true))
    }
    .await;
    match result {
        Ok(doc) => axum::Json(doc).into_response(),
        Err(failure) => data_api_failure(failure),
    }
}

/// Asks the worker whether the console may run as `role` on the branch (it refuses owner-level
/// roles and grants the role to `authenticator`), once per five minutes.
async fn check_role<M, J>(
    state: &AppState<M, J>,
    branch: BranchId,
    role: &str,
) -> Result<(), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let key = (branch, role.to_owned());
    if state.console.checked_roles.contains_key(&key) {
        return Ok(());
    }
    let refusal = match state.js.check_data_role(branch, role).await {
        Ok(reply) if reply.ok => {
            state.console.checked_roles.insert(key, ());
            return Ok(());
        }
        Ok(reply) => match (reply.kind.as_deref(), reply.error) {
            (Some("refused"), Some(message)) => ApiError::new(403, ErrorCode::RoleRefused, message),
            (Some("not_ready"), _) => conflict(ErrorCode::BranchNotReady, "branch is not ready"),
            (kind, message) => {
                error!(branch_id = %branch, ?kind, "check console role failed: {message:?}");
                ApiError::internal()
            }
        },
        Err(ResetError::Timeout) => {
            ApiError::new(504, ErrorCode::Timeout, "worker did not respond; try again")
        }
        Err(ResetError::Transport(err)) => {
            error!(branch_id = %branch, "check console role: {err}");
            ApiError::new(503, ErrorCode::WorkerUnavailable, "worker unavailable")
        }
    };
    Err(refusal)
}

/// Sends the request to rest's console route and passes its answer on. rest answering
/// `unknown_database` means the database stopped being served since the check above: the
/// row is read again to say which.
async fn forward<M, J>(
    state: &AppState<M, J>,
    branch: BranchId,
    role: &str,
    database: &str,
    tree: &Value,
    timeout: Duration,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let reply = state
        .rest
        .console(branch, role, database, tree, timeout)
        .await;
    pass_on(state, branch, database, reply).await
}

/// rest's answer as it is (status, JSON body, `Retry-After`). `unknown_database` means the
/// database stopped being served since the route's check: the row is read again to say
/// which.
async fn pass_on<M, J>(
    state: &AppState<M, J>,
    branch: BranchId,
    database: &str,
    reply: Result<RestReply, reqwest::Error>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let reply = match reply {
        Ok(reply) => reply,
        Err(err) => {
            warn!(branch_id = %branch, "rest: {err}");
            return unavailable("the data API can't be reached right now; try again")
                .into_response();
        }
    };
    if reply.status == StatusCode::NOT_FOUND && is_unknown_database(&reply.body) {
        return match state.metadata.branch_databases(branch).await {
            Ok(rows) => match pick_database(rows, Some(database)) {
                Some(row) if row.data_api != DataApiState::On => data_api_disabled(database),
                _ => database_not_found(),
            },
            Err(err) => {
                error!("list branch databases: {err}");
                ApiError::internal()
            }
        }
        .into_response();
    }
    let mut response = json_text(reply.status, reply.body);
    if let Some(retry_after) = reply.retry_after {
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, retry_after);
    }
    response
}

pub(crate) fn is_unknown_database(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .is_some_and(|v| v["error"]["code"] == "unknown_database")
}

/// rest's answer to a console request.
pub struct RestReply {
    pub status: StatusCode,
    pub retry_after: Option<HeaderValue>,
    pub body: axum::body::Bytes,
}

impl RestInternal {
    /// `POST {rest}/internal/console/{branch}/query` with `{ role, database, tree }`: what
    /// rest answers, status and body as they are. Never an owner password: rest runs the role
    /// through `authenticator`.
    pub async fn console(
        &self,
        branch: BranchId,
        role: &str,
        database: &str,
        tree: &Value,
        timeout: Duration,
    ) -> Result<RestReply, reqwest::Error> {
        let url = format!(
            "{}/internal/console/{}/query",
            self.url.trim_end_matches('/'),
            branch.as_uuid()
        );
        let response = self
            .http
            .post(url)
            .bearer_auth(&self.token)
            .timeout(timeout)
            .json(&json!({ "role": role, "database": database, "tree": tree }))
            .send()
            .await?;
        let status =
            StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| HeaderValue::from_bytes(v.as_bytes()).ok());
        let body = response.bytes().await?;
        Ok(RestReply {
            status,
            retry_after,
            body: axum::body::Bytes::from(body.to_vec()),
        })
    }
}

impl RestInternal {
    /// `GET {rest}/internal/schema/{branch}?database=&role=`: what `role` can reach in
    /// `database`, status and body as rest answers them.
    pub async fn role_schema(
        &self,
        branch: BranchId,
        role: &str,
        database: &str,
    ) -> Result<RestReply, reqwest::Error> {
        let url = format!(
            "{}/internal/schema/{}?database={}&role={}",
            self.url.trim_end_matches('/'),
            branch.as_uuid(),
            urlencoding::encode(database),
            urlencoding::encode(role),
        );
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .timeout(SCHEMA_TIMEOUT)
            .send()
            .await?;
        let status =
            StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| HeaderValue::from_bytes(v.as_bytes()).ok());
        let body = response.bytes().await?;
        Ok(RestReply {
            status,
            retry_after,
            body: axum::body::Bytes::from(body.to_vec()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn op(tree: Value) -> Op {
        cheladb_query::parse(tree.to_string().as_bytes())
            .unwrap()
            .op
    }

    #[test]
    fn pool_key_changes_with_the_sealed_password_and_the_address() {
        let branch = BranchId::new();
        let addr: SocketAddr = "127.0.0.1:55001".parse().unwrap();
        let key = OwnerPoolKey::new(branch, "web1", "v1:sealed-a", addr);
        assert_eq!(key, OwnerPoolKey::new(branch, "web1", "v1:sealed-a", addr));
        // A reset (a new sealed value), a moved compute, another database or branch: a new pool.
        for other in [
            OwnerPoolKey::new(branch, "web1", "v1:sealed-b", addr),
            OwnerPoolKey::new(
                branch,
                "web1",
                "v1:sealed-a",
                "127.0.0.1:55002".parse().unwrap(),
            ),
            OwnerPoolKey::new(branch, "analytics", "v1:sealed-a", addr),
            OwnerPoolKey::new(BranchId::new(), "web1", "v1:sealed-a", addr),
        ] {
            assert_ne!(key, other);
        }
        // The key holds a hash, never the sealed value itself.
        assert!(!format!("{key:?}").contains("sealed-a"));
    }

    /// A connection handed back to an owner pool starts the next request clean: no role a
    /// function set, no setting, no advisory lock, no temporary table (rest's reset, plus
    /// what an owner may leave behind).
    #[test]
    fn owner_connections_are_reset_when_released() {
        assert_eq!(
            OWNER_RESET_SQL,
            "SET ROLE NONE; RESET ALL; SELECT pg_advisory_unlock_all(); DISCARD TEMP"
        );
    }

    /// An owner pool closed under the request (evicted while the request held it) answers
    /// 503 with `Retry-After`, like any moment the database is out of reach.
    #[test]
    fn a_closed_owner_pool_is_503_with_retry_after() {
        let response = data_api_failure(QueryFailure::from_sqlx(sqlx::Error::PoolClosed, "rid"));
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "2");
    }

    #[test]
    fn writes_are_every_kind_but_a_read() {
        let select = json!({ "v": 1, "op": "select", "from": "t" });
        let insert = json!({ "v": 1, "op": "insert", "into": "t", "values": [{ "a": 1 }] });
        assert_eq!(write_kind(&op(select.clone())), None);
        assert_eq!(write_kind(&op(insert.clone())), Some("insert"));
        assert_eq!(
            write_kind(&op(
                json!({ "v": 1, "op": "update", "table": "t", "set": { "a": 1 }, "all": true })
            )),
            Some("update")
        );
        assert_eq!(
            write_kind(&op(
                json!({ "v": 1, "op": "delete", "from": "t", "all": true })
            )),
            Some("delete")
        );
        assert_eq!(
            write_kind(&op(json!({ "v": 1, "op": "call", "fn": "f" }))),
            Some("call")
        );
        assert_eq!(
            write_kind(&op(
                json!({ "v": 1, "op": "batch", "ops": [select.clone(), insert] })
            )),
            Some("batch")
        );
        assert_eq!(
            write_kind(&op(json!({ "v": 1, "op": "batch", "ops": [select] }))),
            None
        );
    }
}
