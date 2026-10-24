//! Studio's console: one data API request on a branch, as the database's owner or as a named
//! role.
//!
//! - **The owner** (`role: null`) runs here, through `chelabase-query`, on a small pool per
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
use chelabase_metadata::{BranchDatabase, BranchKind, BranchStatus, DataApiState, DatabaseOrigin};
use chelabase_neon::ComputeLocator;
use chelabase_query::{Lease, Op, QueryCache, QueryFailure, RunAs, RunSettings};
use chelabase_secrets::{Purpose, open_credential};
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use chelabase_types::data_api_schema::DataApiSchema;
use chelabase_types::{ApiPath, BranchId};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use deadpool_postgres::{Manager, ManagerConfig, Pool, PoolError, RecyclingMethod, Runtime};
use tokio_postgres::NoTls;
use crate::sql_editor::run::{ConnectError, RunTls, connect_error};
use chelabase_neon::compute_tls::ClientTlsSnapshot;
use tracing::{error, info, warn};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::access::{authorize, authorize_database, deny_to_api, Target};
use crate::branches::{conflict, load_branch, parse_body, unprocessable};
use crate::{
    ApiBytes, AppState, AuthPrincipal, BACKUP_REFUSAL, JobPort, MetadataPort, ResetError,
    RestInternal, roles,
};
use chelabase_routes::orchestrator as ep;
use chelabase_types::access::{Deny, PlatformRole, Requested};

/// Connections one owner pool holds at most.
pub const OWNER_POOL_CONNECTIONS: u32 = 2;
/// RT1: how many of an owner pool's connections realtime's `/internal/realtime/visible` calls
/// may hold at once (per branch and Postgres database); the rest stay the console's.
pub const REALTIME_OWNER_CONNECTIONS: u32 = 1;
const _: () = assert!(REALTIME_OWNER_CONNECTIONS < OWNER_POOL_CONNECTIONS);
/// An owner connection (and an owner pool) unused this long is closed.
pub const OWNER_POOL_IDLE: Duration = Duration::from_secs(60);
/// Longest wait for an owner connection.
const OWNER_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a role the worker accepted is remembered.
pub const ROLE_CHECK_TTL: Duration = Duration::from_secs(300);
/// The owner's connections, as `pg_stat_activity` shows them.
pub const APPLICATION_NAME: &str = "chelabase-console";
/// Seconds a 503 asks the client to wait (the data API's value).
pub(crate) const RETRY_AFTER_SECS: u32 = 2;
/// Run on an owner connection before and after every request (`RunSettings::reset_sql`): the
/// next request starts as the owner with the connection's defaults, holding no advisory lock
/// and no temporary table (a function the owner called may have left any of them). A reset
/// that fails discards the connection.
pub const OWNER_RESET_SQL: &str =
    "SET ROLE NONE; RESET ALL; SELECT pg_catalog.pg_advisory_unlock_all(); DISCARD TEMP";
/// An owner pool's manager settings: `Fast` recycling (only a closed connection is dropped).
/// Every request resets the session itself, before and after its work
/// ([`OWNER_RESET_SQL`] as `RunSettings::reset_sql`), so a reset on checkout would only add
/// a round trip.
fn owner_manager_config() -> ManagerConfig {
    ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    }
}

/// Longest wait for rest's schema of a role.
const SCHEMA_TIMEOUT: Duration = Duration::from_secs(15);
/// The owner password isn't known to Chelabase.
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

/// An owner pool's key. `fingerprint` is SHA-256 over the sealed owner password, the
/// compute's `host:port` and the client certificate's fingerprint (when the connection uses
/// TLS), so a new password, a moved compute or a renewed certificate is a new pool.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OwnerPoolKey {
    pub branch: BranchId,
    pub database: String,
    pub fingerprint: [u8; 32],
}

impl OwnerPoolKey {
    pub fn new(
        branch: BranchId,
        database: &str,
        sealed_password: &str,
        addr: SocketAddr,
        tls: &RunTls,
    ) -> Self {
        let identity = match tls {
            RunTls::Plain => None,
            RunTls::Compute { server_name, client } => {
                Some((server_name.as_str(), client.fingerprint()))
            }
        };
        Self::with_identity(branch, database, sealed_password, addr, identity)
    }

    /// The key for a connection secured as `identity` (the compute's certificate name and the
    /// client pair's fingerprint), or plain (`None`).
    fn with_identity(
        branch: BranchId,
        database: &str,
        sealed_password: &str,
        addr: SocketAddr,
        identity: Option<(&str, [u8; 32])>,
    ) -> Self {
        let mut hash = Sha256::new();
        hash.update(sealed_password.as_bytes());
        hash.update(b"\n");
        hash.update(addr.to_string().as_bytes());
        hash.update(b"\n");
        match identity {
            None => hash.update(b"plain"),
            Some((server_name, fingerprint)) => {
                hash.update(b"tls ");
                hash.update(server_name.as_bytes());
                hash.update(b" ");
                hash.update(fingerprint);
            }
        }
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
    pub(crate) pools: moka::future::Cache<OwnerPoolKey, Pool>,
    pub(crate) cache: QueryCache,
    checked_roles: moka::sync::Cache<(BranchId, String), ()>,
    /// Realtime's share of each (branch, Postgres database)'s owner pool
    /// ([`REALTIME_OWNER_CONNECTIONS`] permits).
    realtime_slots: moka::sync::Cache<(BranchId, String), Arc<tokio::sync::Semaphore>>,
}

impl Default for Console {
    fn default() -> Self {
        Console {
            pools: moka::future::Cache::builder()
                .max_capacity(10_000)
                .time_to_idle(OWNER_POOL_IDLE)
                .eviction_listener(|_, pool: Pool, _| {
                    // Close its connections now rather than when the last clone drops. An
                    // idle pool is only ever evicted by this cache's time-to-idle, which is
                    // all the idle reaping two connections need.
                    pool.close();
                })
                .build(),
            cache: QueryCache::default(),
            checked_roles: moka::sync::Cache::builder()
                .max_capacity(10_000)
                .time_to_live(ROLE_CHECK_TTL)
                .build(),
            realtime_slots: moka::sync::Cache::builder()
                .max_capacity(10_000)
                .time_to_idle(OWNER_POOL_IDLE)
                .build(),
        }
    }
}

impl Console {
    /// The owner pool for `key`, made with `config` on a miss. It connects lazily
    /// ([`owner_manager_config`]; each request resets the session itself). Each
    /// connection's statement cache is bounded by the [`Lease`] that prepares through it
    /// ([`chelabase_query::pipeline::STATEMENT_CACHE_CAP`]).
    ///
    /// `client` is the pair the key's fingerprint was taken from (T10 m3: one snapshot, so a
    /// reload between the two reads can't pair one certificate's key with another's
    /// connector); `None` connects plain.
    async fn owner_pool(
        &self,
        key: OwnerPoolKey,
        config: tokio_postgres::Config,
        client: Option<ClientTlsSnapshot>,
    ) -> Pool {
        self.pools
            .get_with(key, async move {
                let manager = match client {
                    None => Manager::from_config(config, NoTls, owner_manager_config()),
                    // The pool's key carries the client pair's fingerprint, so a renewed
                    // pair makes a new pool with a connector of its own.
                    Some(client) => {
                        Manager::from_config(config, client.connector(), owner_manager_config())
                    }
                };
                Pool::builder(manager)
                    .max_size(OWNER_POOL_CONNECTIONS as usize)
                    .runtime(Runtime::Tokio1)
                    .wait_timeout(Some(OWNER_ACQUIRE_TIMEOUT))
                    .create_timeout(Some(OWNER_ACQUIRE_TIMEOUT))
                    .build()
                    .expect("a pool with a runtime set always builds")
            })
            .await
    }

    /// Realtime's share of the owner pool of `database` on `branch`: a `/visible` call holds a
    /// permit while it holds an owner connection, so realtime never takes the whole pool and
    /// the console's own requests always find a connection.
    pub fn realtime_slot(&self, branch: BranchId, database: &str) -> Arc<tokio::sync::Semaphore> {
        self.realtime_slots.get_with((branch, database.to_owned()), || {
            Arc::new(tokio::sync::Semaphore::new(REALTIME_OWNER_CONNECTIONS as usize))
        })
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
        Op::Select(_) | Op::Count(_) => None,
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

/// 503 `compute_tls_failed`: the TLS or certificate check against the compute failed (the
/// reason is logged, not sent).
pub(crate) fn compute_tls_failed() -> ApiError {
    ApiError::new(
        503,
        ErrorCode::ComputeTlsFailed,
        "the secure connection to the branch's compute failed; try again",
    )
    .retry_after(RETRY_AFTER_SECS)
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
    description = "Runs one data API request (`tree`, the JSON `POST /data/v1/query` takes; every operation, batches included) on a branch, in one transaction with the data API's `max_rows` and statement timeout. `role: null` runs as the database's owner; any other role runs as that role, after a check that it exists and isn't owner-level (superuser, CREATEROLE, CREATEDB, REPLICATION or a member of `neon_superuser`); a role with BYPASSRLS passes (Admin only), and the console then reads and writes past row-level security. The answer is the data API's: its JSON on success, and its error body (`invalid_query`, `unknown_table`, `permission_denied`, `conflict`, …) when the request fails in the database. Every write is logged without its values.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
    ),
    request_body = ConsoleRequest,
    responses(
        (status = 200, description = "The data API's answer: the rows, the written rows or count, a call's result, or a batch's results in order", body = serde_json::Value),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON; the data API's `invalid_query` or `invalid_value`: the tree is malformed, the request and its tree name different databases, or Postgres refused a value", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant; `role_refused`: the worker's check refused the role: it doesn't exist, or it is owner-level (the message says which); the data API's `permission_denied` or `role_not_allowed`; a Viewer's write (`insert`, `update`, `delete`, `call`, or a batch holding one) is refused with `required: \"developer\"`", body = ErrorEnvelope),
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

/// T2-1 (D1): whether the caller may write as `role` (`None`: the owner): a named role held at
/// Developer or above, or reached (R21) through the chain of one held there; the owner at the
/// caller's own level.
async fn may_write<M, J>(
    state: &AppState<M, J>,
    authorized: &crate::access::Authorized,
    role: Option<&str>,
    branch: Uuid,
) -> Result<bool, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let Some(grant) = authorized.grant.as_ref() else {
        return Ok(false);
    };
    if grant.platform_role < PlatformRole::Developer {
        return Ok(false);
    }
    let Some(name) = role else {
        return Ok(true);
    };
    if let Some(level) = grant.level_of(name) {
        return Ok(level >= PlatformRole::Developer);
    }
    let Some(record) = authorized.record.as_ref() else {
        return Ok(false);
    };
    let graph = authorized.role_graph(state, record, branch).await?;
    Ok(graph.is_some_and(|g| g.chain_grants(&grant.roles_at_least(PlatformRole::Developer), name)))
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
    // The request is read first: the access check needs the role and the Postgres database.
    let request: ConsoleRequest = or_answer!(parse_body(body));
    if request.role.as_deref().is_some_and(str::is_empty) {
        return unprocessable("role must not be empty (null runs as the owner)").into_response();
    }
    // The tree is checked here for both paths: a bad one never reaches a database or rest.
    let tree =
        or_answer!(serde_json::to_vec(&request.tree).map_err(|e| {
            data_api_failure(QueryFailure::invalid_query(format!("the tree: {e}")))
        }));
    // The console's grammar (the route's choice, never the body's) adds `text_of` and
    // `"text": true`.
    let parsed = or_answer!(
        chelabase_query::parse_with(&tree, chelabase_query::ParseMode::Console)
            .map_err(|e| data_api_failure(QueryFailure::from(e)))
    );
    let name = match (request.database.as_deref(), parsed.database.as_deref()) {
        (Some(a), Some(b)) if a != b => {
            return data_api_failure(QueryFailure::invalid_query(
                "the request and its tree name different databases",
            ));
        }
        (a, b) => a.or(b),
    };
    let target = Target::Role {
        database: id,
        branch: branch_id,
        pg_database: name.unwrap_or(""),
        role: request.role.as_deref().map_or(Requested::Owner, Requested::Role),
    };
    let authorized = or_answer!(authorize(state, principal, ep::CONSOLE_QUERY.policy.access, target).await);
    // A Viewer only reads: a write (or a batch holding one) is refused before it reaches rest.
    // T2-1 (D1): the role's own level counts, not the member's top level.
    if write_kind(&parsed.op).is_some()
        && !or_answer!(may_write(state, &authorized, request.role.as_deref(), branch_id).await)
    {
        return deny_to_api(Deny::Forbidden { required: PlatformRole::Developer.as_str().to_string() })
            .into_response();
    }
    let record = or_answer!(authorized.into_record());
    let branch = or_answer!(load_branch(state, &record, branch_id).await);
    if branch.kind == BranchKind::Backup {
        return conflict(ErrorCode::BackupBranch, BACKUP_REFUSAL).into_response();
    }
    if branch.status != BranchStatus::Ready {
        return conflict(ErrorCode::BranchNotReady, "branch is not ready").into_response();
    }
    if request.role.as_deref() == Some(roles::owner_name(&record)) {
        return unprocessable("use role: null to run as the owner").into_response();
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
    pub tls: RunTls,
}

/// The owner's address and credentials on `branch`: 409 `owner_password_unknown` when
/// Chelabase doesn't know the owner password, 409 `branch_not_ready`, 500
/// `credentials_unavailable` when the sealed password can't be opened, 503 when the compute
/// can't be found.
pub(crate) async fn owner_target<M, J>(
    state: &AppState<M, J>,
    branch: &chelabase_metadata::BranchRecord,
    owner: &str,
) -> Result<OwnerTarget, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    // A password set in SQL leaves nothing sealed: Chelabase doesn't know it.
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
                 restore the original CHELABASE_SECRETS_KEY file",
            ));
        }
    };
    let addr = compute_addr(state, branch.id, endpoint_id).await?;
    let tls = run_tls(state, branch.id, endpoint_id)?;
    Ok(OwnerTarget {
        addr,
        user: owner.to_string(),
        password,
        sealed: sealed.to_string(),
        tls,
    })
}

/// How to secure a connection to `endpoint_id`'s compute: TLS with the orchestrator's client
/// pair, or 503 `compute_tls_failed` when no pair is loaded. `Plain` only when the state has no
/// compute TLS at all (tests; `main` always sets it).
pub(crate) fn run_tls<M, J>(
    state: &AppState<M, J>,
    branch: chelabase_types::BranchId,
    endpoint_id: &str,
) -> Result<RunTls, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let Some(compute_tls) = &state.compute_tls else {
        return Ok(RunTls::Plain);
    };
    match compute_tls.get() {
        Some(client) => Ok(RunTls::Compute {
            server_name: chelabase_neon::compute_tls::server_name(endpoint_id),
            client,
        }),
        None => {
            warn!(branch_id = %branch, %endpoint_id, "no compute client certificate loaded");
            Err(compute_tls_failed())
        }
    }
}

/// The Postgres address of the branch's compute: 503 when it isn't running or can't be found.
pub(crate) async fn compute_addr<M, J>(
    state: &AppState<M, J>,
    branch: chelabase_types::BranchId,
    endpoint_id: &str,
) -> Result<SocketAddr, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    match state.locator.postgres_addr(endpoint_id).await {
        Ok(Some(addr)) => Ok(addr),
        Ok(None) => Err(unavailable("the branch's compute isn't running; try again")),
        Err(err) => {
            warn!(branch_id = %branch, "locate the compute: {err:#}");
            Err(unavailable(
                "the branch's compute can't be reached right now; try again",
            ))
        }
    }
}

/// The owner pool of `row`'s database on `branch` and how its connections are secured (for
/// [`pool_failure`]); errors as [`owner_target`].
pub(crate) async fn owner_pool_for<M, J>(
    state: &AppState<M, J>,
    branch: &chelabase_metadata::BranchRecord,
    row: &BranchDatabase,
    owner: &str,
) -> Result<(Pool, RunTls), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let target = owner_target(state, branch, owner).await?;
    let mut config = tokio_postgres::Config::new();
    config
        .port(target.addr.port())
        .user(&target.user)
        .password(&target.password)
        .dbname(&row.name)
        .application_name(APPLICATION_NAME);
    // One snapshot of the client pair for both the key and the connector (T10 m3).
    let (identity, client) = match &target.tls {
        RunTls::Plain => {
            config.host(target.addr.ip().to_string());
            (None, None)
        }
        RunTls::Compute { server_name, client } => {
            config
                .host(server_name)
                .hostaddr(target.addr.ip())
                .ssl_mode(tokio_postgres::config::SslMode::Require)
                .channel_binding(tokio_postgres::config::ChannelBinding::Require);
            let snapshot = client.snapshot();
            (Some((server_name.as_str(), snapshot.fingerprint())), Some(snapshot))
        }
    };
    let key = OwnerPoolKey::with_identity(branch.id, &row.name, &target.sealed, target.addr, identity);
    let pool = state.console.owner_pool(key, config, client).await;
    Ok((pool, target.tls))
}

/// A pooled connection failure as the data API answers it: a pool that timed out or was
/// closed under the request is a 503 (`Retry-After`), like any moment the database is out of
/// reach; a driver error from connecting or recycling goes through [`QueryFailure::from_pg`],
/// except a compute connection's TLS failure (`connect_error`'s classification over `tls`, T10
/// m1): 503 `compute_tls_failed`, its reason logged with the request id.
pub(crate) fn pool_failure(err: PoolError, request_id: &str, tls: &RunTls) -> QueryFailure {
    match err {
        PoolError::Backend(e) => match connect_error(&e, tls) {
            ConnectError::Tls(reason) => {
                warn!(request_id, "owner pool, the TLS connection to the compute failed: {reason}");
                let api = compute_tls_failed();
                QueryFailure::new(StatusCode::SERVICE_UNAVAILABLE, api.code.as_str(), api.message)
            }
            _ => QueryFailure::from_pg(&e, request_id),
        },
        PoolError::Timeout(_) | PoolError::Closed => QueryFailure::unreachable(request_id, err),
        other => QueryFailure::internal(request_id, other),
    }
}

/// Runs `op` as the database's owner on the owner pool, in one transaction, on a leased
/// connection ([`chelabase_query::Lease`]: closed rather than pooled when the call doesn't
/// finish or its session can't be reset).
async fn run_as_owner<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    record: &chelabase_metadata::DatabaseRecord,
    branch: &chelabase_metadata::BranchRecord,
    row: &BranchDatabase,
    owner: &str,
    op: &Op,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let (pool, tls) = match owner_pool_for(state, branch, row, owner).await {
        Ok(pool) => pool,
        Err(err) => return err.into_response(),
    };
    let settings = RunSettings {
        max_rows: i64::from(record.max_rows),
        statement_timeout_ms: i64::from(record.statement_timeout_ms),
        default_schema: row.default_schema.clone(),
        reset_sql: OWNER_RESET_SQL,
    };
    let rid = principal.request_id.as_str();
    let result: Result<String, QueryFailure> = async {
        let mut lease = Lease::new(pool.get().await.map_err(|e| pool_failure(e, rid, &tls))?);
        let out = chelabase_query::run(
            &lease,
            &state.console.cache,
            (branch.id.as_uuid(), &row.name),
            // The owner pool logs in as `owner` (`owner_pool_for`).
            RunAs::Owner { user: owner },
            &settings,
            chelabase_query::engine::DEFAULT_MAX_RESPONSE_BYTES,
            op,
            rid,
        )
        .await;
        lease.done();
        out
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
    description = "What the console may show for one Postgres database of a branch, in the data API schema's shape (schemas, tables with their primary keys and the role's privileges, functions, enums). Without `role`, everything the database's owner reaches; with it, what that role reaches, after the same check the console's queries pass (it exists and isn't owner-level; BYPASSRLS is allowed, Admin only).",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ConsoleSchemaQuery,
    ),
    responses(
        (status = 200, description = "The schemas, tables, functions and enums the owner (or the role) reaches; each table's `privileges` has one entry, for that role", body = DataApiSchema),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant; `role_refused`: the worker's check refused the role: it doesn't exist, or it is owner-level (the message says which); the data API's `role_not_allowed`", body = ErrorEnvelope),
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
    let Ok(axum::extract::Query(query)) = query else {
        return unprocessable("the query string is malformed").into_response();
    };
    if query.role.as_deref() == Some("") {
        return unprocessable("role must not be empty (omit it to view the schema as the owner)")
            .into_response();
    }
    let target = Target::Role {
        database: id,
        branch: branch_id,
        pg_database: query.database.as_deref().unwrap_or(""),
        role: query.role.as_deref().map_or(Requested::Owner, Requested::Role),
    };
    let record = or_answer!(authorize_database(&state, &principal, ep::CONSOLE_SCHEMA.policy.access, target).await);
    let branch = or_answer!(load_branch(&state, &record, branch_id).await);
    if branch.kind == BranchKind::Backup {
        return conflict(ErrorCode::BackupBranch, BACKUP_REFUSAL).into_response();
    }
    if branch.status != BranchStatus::Ready {
        return conflict(ErrorCode::BranchNotReady, "branch is not ready").into_response();
    }
    let owner = roles::owner_name(&record).to_owned();
    match query.role.as_deref() {
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
/// console's schema cache, on a leased connection (see [`run_as_owner`]).
async fn owner_schema<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    record: &chelabase_metadata::DatabaseRecord,
    branch: &chelabase_metadata::BranchRecord,
    row: &BranchDatabase,
    owner: &str,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let (pool, tls) = match owner_pool_for(state, branch, row, owner).await {
        Ok(pool) => pool,
        Err(err) => return err.into_response(),
    };
    let settings = RunSettings {
        max_rows: i64::from(record.max_rows),
        statement_timeout_ms: i64::from(record.statement_timeout_ms),
        default_schema: row.default_schema.clone(),
        reset_sql: OWNER_RESET_SQL,
    };
    let rid = principal.request_id.as_str();
    let result: Result<Value, QueryFailure> = async {
        let mut lease = Lease::new(pool.get().await.map_err(|e| pool_failure(e, rid, &tls))?);
        let read = chelabase_query::schema_for(
            &lease,
            &state.console.cache,
            (branch.id.as_uuid(), &row.name),
            // The owner pool logs in as `owner` (`owner_pool_for`).
            RunAs::Owner { user: owner },
            &settings,
            rid,
        )
        .await;
        lease.done();
        let (schema, view) = read?;
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
pub(crate) async fn check_role<M, J>(
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
        chelabase_query::parse(tree.to_string().as_bytes())
            .unwrap()
            .op
    }

    #[test]
    fn pool_key_changes_with_the_sealed_password_and_the_address() {
        let branch = BranchId::new();
        let addr: SocketAddr = "127.0.0.1:55001".parse().unwrap();
        let plain = RunTls::Plain;
        let key = OwnerPoolKey::new(branch, "web1", "v1:sealed-a", addr, &plain);
        assert_eq!(key, OwnerPoolKey::new(branch, "web1", "v1:sealed-a", addr, &plain));
        // A reset (a new sealed value), a moved compute, another database or branch: a new pool.
        for other in [
            OwnerPoolKey::new(branch, "web1", "v1:sealed-b", addr, &plain),
            OwnerPoolKey::new(
                branch,
                "web1",
                "v1:sealed-a",
                "127.0.0.1:55002".parse().unwrap(),
                &plain,
            ),
            OwnerPoolKey::new(branch, "analytics", "v1:sealed-a", addr, &plain),
            OwnerPoolKey::new(BranchId::new(), "web1", "v1:sealed-a", addr, &plain),
        ] {
            assert_ne!(key, other);
        }
        // The key holds a hash, never the sealed value itself.
        assert!(!format!("{key:?}").contains("sealed-a"));
    }

    /// A connection handed back to an owner pool starts the next request clean: no role a
    /// function set, no setting, no advisory lock, no temporary table (rest's reset, plus
    /// what an owner may leave behind). The unlock runs after `RESET ALL`, under the login
    /// role's `search_path` (a database-level setting the owner controls), so it is named by
    /// schema (RT1 final fix round).
    #[test]
    fn owner_connections_are_reset_when_released() {
        assert_eq!(
            OWNER_RESET_SQL,
            "SET ROLE NONE; RESET ALL; SELECT pg_catalog.pg_advisory_unlock_all(); DISCARD TEMP"
        );
    }

    /// The reset runs inside each request (before and after it, `RunSettings::reset_sql`),
    /// so the pool's recycling only drops closed connections: no extra round trip per
    /// checkout.
    #[test]
    fn owner_pools_recycle_fast() {
        assert!(matches!(
            owner_manager_config().recycling_method,
            RecyclingMethod::Fast
        ));
    }

    /// An owner pool closed under the request (evicted while the request held it) answers
    /// 503 with `Retry-After`, like any moment the database is out of reach.
    #[test]
    fn a_closed_owner_pool_is_503_with_retry_after() {
        let response = data_api_failure(pool_failure(PoolError::Closed, "rid", &RunTls::Plain));
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "2");
    }

    /// Waiting too long for an owner connection is the same 503.
    #[test]
    fn an_owner_pool_timeout_is_503_with_retry_after() {
        let failure = pool_failure(PoolError::Timeout(deadpool_postgres::TimeoutType::Wait), "rid", &RunTls::Plain);
        let response = data_api_failure(failure);
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "2");
    }

    // T10 m1, under the classification invariant: an owner pool's connect failure on a
    // compute connection is 503 `compute_tls_failed` exactly when `connect_error` calls it a
    // TLS failure; every other failure keeps `QueryFailure::from_pg`'s answer.
    #[tokio::test]
    async fn owner_pool_tls_failures_are_compute_tls_failed() {
        use crate::sql_editor::fake_pg::{self, Want};
        let compute = fake_pg::compute_tls();
        for &(case, on_compute, on_plain) in fake_pg::TABLE {
            for (tls, want) in [(&compute, on_compute), (&RunTls::Plain, on_plain)] {
                let failure = pool_failure(PoolError::Backend(case.error().await), "rid", tls);
                if want == Want::Tls {
                    assert_eq!(
                        (failure.status, failure.code),
                        (StatusCode::SERVICE_UNAVAILABLE, "compute_tls_failed"),
                        "{case:?}"
                    );
                    let response = data_api_failure(failure);
                    assert_eq!(response.headers()[header::RETRY_AFTER], "2", "{case:?}");
                } else {
                    let before = QueryFailure::from_pg(&case.error().await, "rid");
                    assert_eq!(
                        (failure.status, failure.code),
                        (before.status, before.code),
                        "{case:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn writes_are_every_kind_but_a_read() {
        let select = json!({ "v": 1, "op": "select", "from": "t" });
        let insert = json!({ "v": 1, "op": "insert", "into": "t", "values": [{ "a": 1 }] });
        assert_eq!(write_kind(&op(select.clone())), None);
        // A count is a read: Viewers may run it.
        assert_eq!(write_kind(&op(json!({ "v": 1, "op": "count", "from": "t" }))), None);
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
