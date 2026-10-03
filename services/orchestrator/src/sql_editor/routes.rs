//! The SQL editor's run, cancel and schema routes, and the run audit.
//!
//! A run answers JSON errors until its connection is open, then `200 application/x-ndjson`:
//! the run engine's events, one per line, ending with `done`. Every run (and every
//! `sql_confirm_required`) writes one `info` line on the `orchestrator::sql_editor::audit` target
//! (enabled by [`crate::DEFAULT_LOG_FILTER`]):
//! who, where, as whom, the command tags and a SHA-256 of the script, never its text.
//! Requests answered with a JSON error (validation, limits, a compute that can't be reached)
//! write no line: nothing ran.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    Json,
    body::Body,
    extract::State,
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use cheladb_metadata::{BranchDatabase, BranchKind, BranchRecord, BranchStatus, DatabaseRecord};
use cheladb_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use cheladb_types::{ApiPath, BranchId};
use futures_util::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::{error, info, warn};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::events::{Outcome, RunEvent};
use super::registry::ReserveError;
use super::run::{self, ConnectError, Connected, Finished, RunSpec, RunTarget};
use super::{DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_MS, MIN_TIMEOUT_MS, guard, invalid_sql, schema};
use crate::branches::{conflict, load_branch, parse_body, unprocessable};
use crate::console::{
    OWNER_PASSWORD_UNKNOWN, database_not_found, owner_target, pick_database, unavailable,
};
use crate::{
    ApiBytes, AppState, AuthPrincipal, BACKUP_REFUSAL, JobPort, MetadataPort, ResetError,
    load_owned, roles,
};

/// The audit log's target.
const AUDIT_TARGET: &str = "orchestrator::sql_editor::audit";
/// Command tags an audit line keeps at most.
const AUDIT_TAGS: usize = 50;
/// Seconds a 429 asks the client to wait.
const RETRY_AFTER_SECS: u32 = 2;
/// The longest a schema read may take, its settings and 12 queries together.
const SCHEMA_DEADLINE: Duration = Duration::from_secs(35);
/// The data API's roles, granted to the owner (SET only) on first use.
const API_ROLES: [&str; 2] = ["anon", "authenticated"];
/// Roles the editor never runs as (besides any `pg_` name).
const RESERVED_ROLES: [&str; 5] = [
    "cloud_admin",
    "neon_superuser",
    "authenticator",
    "cheladb_admin",
    "cheladb_auth_sync",
];

pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new()
        .routes(routes!(run_sql))
        .routes(routes!(cancel_sql_run))
        .routes(routes!(get_sql_schema))
}

/// `POST …/sql`'s body.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RunSqlRequest {
    /// Chosen by the client; names the run for `…/sql/runs/{run_id}/cancel`.
    pub run_id: Uuid,
    /// The Postgres database; null or absent is the branch's main database.
    #[serde(default)]
    pub database: Option<String>,
    /// The role to `SET ROLE` to first; null or absent runs as the owner.
    #[serde(default)]
    pub role: Option<String>,
    /// The script, sent as one simple query: 1 byte to 1 MiB.
    pub sql: String,
    /// 100 to 900000; 60000 when absent.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// `true` runs statements the guard would hold back.
    #[serde(default)]
    pub confirm: Option<bool>,
}

/// `GET …/sql/schema`'s query.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SqlSchemaQuery {
    /// The Postgres database; the branch's main database when absent.
    pub database: Option<String>,
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

/// The database, its branch (ready, not a backup) and the Postgres database named
/// (`None`: the main one; never the system database).
async fn load_target<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    id: Uuid,
    branch_id: Uuid,
) -> Result<(DatabaseRecord, BranchRecord), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let record = load_owned(state, principal, id).await?;
    let branch = load_branch(state, &record, branch_id).await?;
    if branch.kind == BranchKind::Backup {
        return Err(conflict(ErrorCode::BackupBranch, BACKUP_REFUSAL));
    }
    if branch.status != BranchStatus::Ready {
        return Err(conflict(ErrorCode::BranchNotReady, "branch is not ready"));
    }
    Ok((record, branch))
}

async fn pick_pg_database<M, J>(
    state: &AppState<M, J>,
    branch: &BranchRecord,
    name: Option<&str>,
) -> Result<BranchDatabase, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let rows = state
        .metadata
        .branch_databases(branch.id)
        .await
        .map_err(|err| {
            error!("list branch databases: {err}");
            ApiError::internal()
        })?;
    pick_database(rows, name).ok_or_else(database_not_found)
}

fn is_reserved(role: &str) -> bool {
    role.to_ascii_lowercase().starts_with("pg_") || RESERVED_ROLES.contains(&role)
}

/// Checks `role` may be used on `row`: a role of the branch, or `anon`/`authenticated`
/// where the data API's machinery was installed. Returns whether the owner needs it granted.
async fn resolve_role<M, J>(
    state: &AppState<M, J>,
    branch: &BranchRecord,
    row: &BranchDatabase,
    role: &str,
) -> Result<bool, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    if is_reserved(role) {
        return Err(unprocessable(&format!(
            "the SQL editor can't run as the system role {role:?}"
        )));
    }
    if API_ROLES.contains(&role) && row.data_api_bootstrapped {
        return Ok(true);
    }
    let Some(endpoint) = branch.endpoint_id.as_deref() else {
        return Err(conflict(ErrorCode::BranchNotReady, "branch is not ready"));
    };
    let roles = state.metadata.list_roles(endpoint).await.map_err(|err| {
        error!(branch_id = %branch.id, "list roles: {err}");
        ApiError::internal()
    })?;
    if roles.iter().any(|r| r.name == role) {
        Ok(false)
    } else {
        Err(ApiError::not_found("role not found"))
    }
}

/// Grants `role` to the owner (SET only) through the worker, once per five minutes.
async fn grant_to_owner<M, J>(
    state: &AppState<M, J>,
    branch: BranchId,
    role: &str,
    owner: &str,
) -> Result<(), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let key = (branch, role.to_owned());
    if state.sql_editor.granted.contains_key(&key) {
        return Ok(());
    }
    let refusal = match state.js.grant_data_role_to_owner(branch, role, owner).await {
        Ok(reply) if reply.ok => {
            state.sql_editor.granted.insert(key, ());
            return Ok(());
        }
        Ok(reply) => match (reply.kind.as_deref(), reply.error) {
            (Some("refused"), Some(message)) => ApiError::new(403, ErrorCode::RoleRefused, message),
            (Some("not_ready"), _) => conflict(ErrorCode::BranchNotReady, "branch is not ready"),
            (kind, message) => {
                error!(branch_id = %branch, ?kind, "grant role to the owner failed: {message:?}");
                ApiError::internal()
            }
        },
        Err(ResetError::Timeout) => {
            ApiError::new(504, ErrorCode::Timeout, "worker did not respond; try again")
        }
        Err(ResetError::Transport(err)) => {
            error!(branch_id = %branch, "grant role to the owner: {err}");
            ApiError::new(503, ErrorCode::WorkerUnavailable, "worker unavailable")
        }
    };
    Err(refusal)
}

/// A fresh owner connection to `database` on the branch's compute.
async fn connect_as_owner<M, J>(
    state: &AppState<M, J>,
    branch: &BranchRecord,
    owner: &str,
    database: &str,
) -> Result<Connected, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let target = owner_target(state, branch, owner).await?;
    let target = RunTarget {
        addr: target.addr,
        user: target.user,
        password: target.password,
        database: database.to_owned(),
    };
    run::connect(&target)
        .await
        .map_err(|err| connect_failure(branch.id, err))
}

/// The answer for a failed editor connection, by the server's SQLSTATE when it answered.
fn connect_failure(branch: BranchId, err: ConnectError) -> ApiError {
    match err {
        ConnectError::Unreachable(message) => {
            warn!(branch_id = %branch, "SQL editor connect: {message}");
            unavailable("the branch's compute can't be reached right now; try again")
        }
        ConnectError::Rejected { code, message } => {
            warn!(branch_id = %branch, code = %code, "SQL editor connect refused: {message}");
            match code.as_str() {
                // The sealed password is the one Cheladb set: it was changed in SQL.
                "28P01" => conflict(ErrorCode::OwnerPasswordUnknown, OWNER_PASSWORD_UNKNOWN),
                "3D000" => database_not_found(),
                "57P03" => unavailable("the branch's compute is starting up; try again"),
                c if c.starts_with("53") => unavailable(
                    "the branch's compute is out of connections or resources; try again",
                ),
                _ => {
                    error!(branch_id = %branch, code = %code, "SQL editor connect refused: {message}");
                    ApiError::internal()
                }
            }
        }
    }
}

/// `fut`, or 504 `timeout` once `limit` passes.
async fn within<T>(
    limit: Duration,
    fut: impl std::future::Future<Output = T>,
) -> Result<T, ApiError> {
    tokio::time::timeout(limit, fut).await.map_err(|_| {
        ApiError::new(
            504,
            ErrorCode::Timeout,
            "reading the schema took too long; try again",
        )
    })
}

fn outcome_name(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Ok => "ok",
        Outcome::Error => "error",
        Outcome::Cancelled => "cancelled",
        Outcome::Timeout => "timeout",
        Outcome::TruncatedResponse => "truncated_response",
    }
}

/// One run's audit facts; logs once, when `done` passes or when dropped first (with the
/// engine's outcome if it finished, else `cancelled`).
struct Audit {
    user: String,
    database_id: Uuid,
    branch_id: BranchId,
    pg_database: String,
    role: String,
    run_id: Uuid,
    sql_sha256: String,
    protected: bool,
    confirmed: bool,
    started: Instant,
    statements: usize,
    tags: Vec<String>,
    logged: bool,
    finished: Finished,
}

/// The outcome a dropped [`Audit`] logs: the engine's when it finished (its `done` never
/// passed), `cancelled` (after `elapsed_ms`) when the stream went first.
fn closing_outcome(finished: Option<&(Outcome, u64)>, elapsed_ms: u64) -> (&'static str, u64) {
    match finished {
        Some((outcome, duration_ms)) => (outcome_name(*outcome), *duration_ms),
        None => ("cancelled", elapsed_ms),
    }
}

impl Audit {
    #[allow(clippy::too_many_arguments)]
    fn new(
        principal: &AuthPrincipal,
        database_id: Uuid,
        branch: &BranchRecord,
        pg_database: &str,
        role: Option<&str>,
        run_id: Uuid,
        sql_sha256: &str,
        confirmed: bool,
    ) -> Self {
        Audit {
            user: principal.sub.clone(),
            database_id,
            branch_id: branch.id,
            pg_database: pg_database.to_owned(),
            role: role.unwrap_or("owner").to_owned(),
            run_id,
            sql_sha256: sql_sha256.to_owned(),
            protected: branch.protected,
            confirmed,
            started: Instant::now(),
            statements: 0,
            tags: Vec::new(),
            logged: false,
            finished: Finished::default(),
        }
    }

    fn observe(&mut self, event: &RunEvent) {
        match event {
            RunEvent::Complete { tag, .. } => {
                self.statements += 1;
                if self.tags.len() < AUDIT_TAGS {
                    self.tags.push(tag.clone());
                }
            }
            RunEvent::Done {
                outcome,
                duration_ms,
            } => self.log(outcome_name(*outcome), *duration_ms),
            _ => {}
        }
    }

    /// The 409 `sql_confirm_required` line: nothing ran; `held` lists the held kinds.
    fn log_held(&mut self, held: &str) {
        self.logged = true;
        info!(
            target: AUDIT_TARGET,
            user = %self.user,
            database_id = %self.database_id,
            branch_id = %self.branch_id,
            pg_database = %self.pg_database,
            role = %self.role,
            run_id = %self.run_id,
            outcome = %"confirm_required",
            held = %held,
            sql_sha256 = %self.sql_sha256,
            protected = self.protected,
            confirmed = self.confirmed,
            "sql run"
        );
    }

    fn log(&mut self, outcome: &str, duration_ms: u64) {
        if self.logged {
            return;
        }
        self.logged = true;
        info!(
            target: AUDIT_TARGET,
            user = %self.user,
            database_id = %self.database_id,
            branch_id = %self.branch_id,
            pg_database = %self.pg_database,
            role = %self.role,
            run_id = %self.run_id,
            statements = self.statements,
            tags = %self.tags.join(","),
            duration_ms,
            outcome = %outcome,
            sql_sha256 = %self.sql_sha256,
            protected = self.protected,
            confirmed = self.confirmed,
            "sql run"
        );
    }
}

impl Drop for Audit {
    fn drop(&mut self) {
        let elapsed = self.started.elapsed().as_millis() as u64;
        let (outcome, duration_ms) = closing_outcome(self.finished.get(), elapsed);
        self.log(outcome, duration_ms);
    }
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/branches/{branch_id}/sql",
    operation_id = "runSql",
    tag = "sql-editor",
    summary = "Run SQL on a branch",
    description = "Runs a script on one of the branch's Postgres databases on a fresh connection as the database's owner (or, with `role`, after `SET ROLE` to it: a preview of that role's view, not a security boundary). The script is sent unchanged as one simple query, so multi-statement scripts, explicit transactions and commands like `VACUUM` work. Unless `confirm` is true, risky statements (drops, truncates, `DELETE`/`UPDATE` without `WHERE`, also under `EXPLAIN ANALYZE` or `PREPARE`, and anything that doesn't parse) and, on a protected branch, every statement that isn't a read are held back with 409 `sql_confirm_required`. Errors before the connection opens are JSON; then the answer is `200 application/x-ndjson`, one JSON event per line: `notice`, `columns` (`statement`, `columns: [{name, type}]`), `rows` (`statement`, at most 200 rows of text-or-null values), `complete` (`statement`, `tag`, `rows`, `truncated`: over 1,000 rows), `error` (`statement`, `code`, `message`, `detail`, `hint`, `position`: 1-based in the sent text), `heartbeat` (after 15 s without another event) and, always last, `done` (`outcome`: `ok`, `error`, `cancelled`, `timeout` or `truncated_response` past 10 MiB; `duration_ms`). Closing the stream cancels the run. Every run is logged with its command tags and a SHA-256 of the script, never the script.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
    ),
    request_body = RunSqlRequest,
    responses(
        (status = 200, description = "The run's events as NDJSON, ending with `done`", content_type = "application/x-ndjson", body = String),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Cheladb tenant; `role_refused`: the worker couldn't let the owner use `anon` or `authenticated` (the message says why)", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch, the Postgres database or the role doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`sql_confirm_required`: the guard held statements back; `error.statements` lists them (`index`, `kind`: pg_query's node name or `unparsed`, `text_preview`: the first 200 characters, `reason`: `risky` or `protected`); send again with `confirm: true` to run them. `run_id_in_use`: you have a run with this id in flight; `owner_password_unknown`: the owner's password was set in SQL (reset it); `backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the body isn't the run's shape, `sql` isn't 1 byte to 1 MiB or holds a NUL byte, `timeout_ms` isn't 100 to 900000, `role` is empty, or `role` is a system role", body = ErrorEnvelope),
        (status = 429, description = "`sql_run_limit`: you have 4 runs in flight, or the Postgres database has 4 (`Retry-After`)", body = ErrorEnvelope),
        (status = 500, description = "`credentials_unavailable`: the stored owner password can't be opened with the current secrets key; `internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: the compute isn't running, can't be reached, is starting up or is out of connections right now (`Retry-After`); `worker_unavailable`: the worker can't be reached", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the worker didn't answer in time", body = ErrorEnvelope),
    ),
)]
async fn run_sql<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
    ApiBytes(body): ApiBytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let (record, branch) = or_answer!(load_target(&state, &principal, id, branch_id).await);
    let request: RunSqlRequest = or_answer!(parse_body(&body));
    if let Some(message) = invalid_sql(&request.sql) {
        return unprocessable(message).into_response();
    }
    let timeout_ms = request.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
        return unprocessable("`timeout_ms` must be 100 to 900000").into_response();
    }
    if request.role.as_deref() == Some("") {
        return unprocessable("`role` must not be empty (null runs as the owner)").into_response();
    }
    let row = or_answer!(pick_pg_database(&state, &branch, request.database.as_deref()).await);
    let needs_grant = match request.role.as_deref() {
        None => false,
        Some(role) => or_answer!(resolve_role(&state, &branch, &row, role).await),
    };

    let confirmed = request.confirm == Some(true);
    let sql_sha256 = hex::encode(Sha256::digest(request.sql.as_bytes()));
    let audit = || {
        Audit::new(
            &principal,
            id,
            &branch,
            &row.name,
            request.role.as_deref(),
            request.run_id,
            &sql_sha256,
            confirmed,
        )
    };
    if !confirmed {
        let sql = request.sql.clone();
        let protected = branch.protected;
        let held = match tokio::task::spawn_blocking(move || guard::check(&sql, protected)).await {
            Ok(held) => held,
            Err(err) => {
                error!("SQL guard: {err}");
                return ApiError::internal().into_response();
            }
        };
        if !held.is_empty() {
            let kinds: Vec<&str> = held.iter().map(|h| h.kind.as_str()).collect();
            audit().log_held(&kinds.join(","));
            return conflict(
                ErrorCode::SqlConfirmRequired,
                "some statements need confirmation; send again with confirm: true to run them",
            )
            .with_extra(
                "statements",
                serde_json::to_value(&held).unwrap_or_default(),
            )
            .into_response();
        }
    }

    let owner = roles::owner_name(&record).to_owned();
    if needs_grant {
        let role = request.role.as_deref().unwrap_or_default();
        or_answer!(grant_to_owner(&state, branch.id, role, &owner).await);
    }
    let guard = match state.sql_editor.runs.reserve(
        &principal.sub,
        request.run_id,
        branch.id,
        &row.name,
    ) {
        Ok(guard) => guard,
        Err(err) => return reserve_refusal(err),
    };
    let connected = or_answer!(connect_as_owner(&state, &branch, &owner, &row.name).await);

    let mut audit = audit();
    let spec = RunSpec {
        sql: request.sql,
        role: request.role,
        timeout: Duration::from_millis(timeout_ms),
    };
    let finished = audit.finished.clone();
    let events = run::run_reporting(connected, spec, guard, finished).map(move |event| {
        audit.observe(&event);
        Ok::<_, Infallible>(event.to_line())
    });
    let mut response = Response::new(Body::from_stream(events));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

/// The answer when a run (or a schema read) can't have a slot.
fn reserve_refusal(err: ReserveError) -> Response {
    let message = match err {
        ReserveError::InUse => {
            return conflict(
                ErrorCode::RunIdInUse,
                "you already have a run with this id in flight",
            )
            .into_response();
        }
        ReserveError::UserLimit => "you have 4 runs in flight; wait for one to end",
        ReserveError::TargetLimit => "this database has 4 runs in flight; wait for one to end",
    };
    ApiError::new(429, ErrorCode::SqlRunLimit, message)
        .retry_after(RETRY_AFTER_SECS)
        .into_response()
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/branches/{branch_id}/sql/runs/{run_id}/cancel",
    operation_id = "cancelSqlRun",
    tag = "sql-editor",
    summary = "Cancel a SQL run",
    description = "Cancels one of your runs in flight: Postgres's cancel request is sent and the run's stream ends with `done` (`outcome: cancelled`). Closing the stream cancels too.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("run_id" = Uuid, Path, description = "The run's id, as the client chose it"),
    ),
    responses(
        (status = 202, description = "The cancel was sent", body = Object),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Cheladb tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist, or you have no run with this id in flight", body = ErrorEnvelope),
        (status = 409, description = "`backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn cancel_sql_run<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, run_id)): ApiPath<(Uuid, Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    or_answer!(load_target(&state, &principal, id, branch_id).await);
    if state.sql_editor.runs.cancel(&principal.sub, run_id) {
        (StatusCode::ACCEPTED, Json(serde_json::json!({}))).into_response()
    } else {
        ApiError::not_found("run not found").into_response()
    }
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/sql/schema",
    operation_id = "getSqlSchema",
    tag = "sql-editor",
    summary = "The schema for the SQL editor's completion",
    description = "Everything the database's owner sees in one Postgres database of the branch, read on a fresh owner connection with postgres-language-server's own introspection (the version `PGLS_VERSION` pins), as the JSON its WASM build's `setSchema` takes. Not cached on the server.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        SqlSchemaQuery,
    ),
    responses(
        (status = 200, description = "postgres-language-server's schema JSON (at most 8 MiB)", body = Object),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Cheladb tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the Postgres database doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`owner_password_unknown`: the owner's password was set in SQL (reset it); `backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the query string is malformed; `schema_too_large`: the schema is over 8 MiB (use keyword completion)", body = ErrorEnvelope),
        (status = 429, description = "`sql_run_limit`: a schema read takes a run slot while it lasts, and you have 4 runs in flight, or the Postgres database has 4 (`Retry-After`)", body = ErrorEnvelope),
        (status = 500, description = "`credentials_unavailable`: the stored owner password can't be opened with the current secrets key; `internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: the compute isn't running, can't be reached, is starting up or is out of connections right now (`Retry-After`)", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: reading the schema took over 35 s", body = ErrorEnvelope),
    ),
)]
async fn get_sql_schema<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
    query: Result<axum::extract::Query<SqlSchemaQuery>, axum::extract::rejection::QueryRejection>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let (record, branch) = or_answer!(load_target(&state, &principal, id, branch_id).await);
    let Ok(axum::extract::Query(query)) = query else {
        return unprocessable("the query string is malformed").into_response();
    };
    let row = or_answer!(pick_pg_database(&state, &branch, query.database.as_deref()).await);
    // A schema read holds an owner connection for up to 35 s: it takes a run slot meanwhile.
    let _slot = match state.sql_editor.runs.reserve(
        &principal.sub,
        Uuid::new_v4(),
        branch.id,
        &row.name,
    ) {
        Ok(slot) => slot,
        Err(err) => return reserve_refusal(err),
    };
    let owner = roles::owner_name(&record);
    let connected = or_answer!(connect_as_owner(&state, &branch, owner, &row.name).await);
    let result = or_answer!(within(SCHEMA_DEADLINE, schema::introspect(connected.client())).await);
    drop(connected);
    let json = match result {
        Ok(json) => json,
        Err(schema::SchemaError::TooLarge(bytes)) => {
            return ApiError::new(
                422,
                ErrorCode::SchemaTooLarge,
                format!("the schema is {bytes} bytes, over the 8 MiB the editor takes"),
            )
            .into_response();
        }
        Err(schema::SchemaError::Query(message)) => {
            error!(branch_id = %branch.id, "SQL editor schema: {message}");
            return ApiError::internal().into_response();
        }
    };
    let mut response = (StatusCode::OK, json).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejected(code: &str) -> ConnectError {
        ConnectError::Rejected {
            code: code.to_owned(),
            message: "server said no".to_owned(),
        }
    }

    #[test]
    fn connect_failures_map_by_sqlstate() {
        let branch = BranchId::new();
        for (err, status, code) in [
            (rejected("28P01"), 409, ErrorCode::OwnerPasswordUnknown),
            (rejected("3D000"), 404, ErrorCode::NotFound),
            (rejected("57P03"), 503, ErrorCode::Unavailable),
            (rejected("53300"), 503, ErrorCode::Unavailable),
            (rejected("53200"), 503, ErrorCode::Unavailable),
            (rejected("28000"), 500, ErrorCode::Internal),
            (rejected("XX000"), 500, ErrorCode::Internal),
            (
                ConnectError::Unreachable("refused".into()),
                503,
                ErrorCode::Unavailable,
            ),
        ] {
            let label = format!("{err:?}");
            let api = connect_failure(branch, err);
            assert_eq!((api.status, api.code), (status, code), "{label}");
            if status == 503 {
                assert_eq!(api.retry_after, Some(RETRY_AFTER_SECS), "{label}");
            }
        }
        assert_eq!(
            connect_failure(branch, rejected("3D000")).message,
            "database not found"
        );
        assert_eq!(
            connect_failure(branch, rejected("28P01")).message,
            OWNER_PASSWORD_UNKNOWN
        );
    }

    #[test]
    fn a_dropped_audit_keeps_the_engine_outcome() {
        // The engine finished but its `done` never passed (a slow reader): its outcome stands.
        assert_eq!(closing_outcome(Some(&(Outcome::Ok, 12)), 900), ("ok", 12));
        assert_eq!(
            closing_outcome(Some(&(Outcome::TruncatedResponse, 7)), 900),
            ("truncated_response", 7)
        );
        // The stream went first: the run was cancelled.
        assert_eq!(closing_outcome(None, 900), ("cancelled", 900));
    }

    #[test]
    fn default_log_filter_enables_the_audit() {
        use tracing_subscriber::layer::SubscriberExt;
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new(crate::DEFAULT_LOG_FILTER));
        tracing::subscriber::with_default(subscriber, || {
            assert!(tracing::enabled!(target: AUDIT_TARGET, tracing::Level::INFO));
        });
    }

    #[tokio::test]
    async fn schema_deadline_is_504() {
        let err = within(Duration::from_millis(10), std::future::pending::<()>())
            .await
            .unwrap_err();
        assert_eq!((err.status, err.code), (504, ErrorCode::Timeout));
        assert_eq!(
            within(Duration::from_secs(1), async { 7 }).await.unwrap(),
            7
        );
    }
}
