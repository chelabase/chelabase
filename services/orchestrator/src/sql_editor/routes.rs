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
use chelabase_metadata::{BranchDatabase, BranchKind, BranchRecord, BranchStatus, DatabaseRecord};
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use chelabase_types::{ApiPath, BranchId};
use futures_util::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::{error, info, warn};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use super::events::{Outcome, RunEvent};
use super::registry::{Canceller, Cancelled, ReserveError};
use super::run::{self, ConnectError, Connected, Finished, RunSpec, RunTarget};
use super::{DEFAULT_TIMEOUT_MS, MAX_TIMEOUT_MS, MIN_TIMEOUT_MS, guard, invalid_sql, schema};
use crate::branches::{conflict, load_branch, parse_body, unprocessable};
use crate::console::{
    OWNER_PASSWORD_UNKNOWN, compute_addr, compute_tls_failed, database_not_found, run_tls, owner_target, pick_database,
    unavailable,
};
use crate::access::{authorize, deny_to_api, Authorized, RoleGraph, Target};
use crate::{
    ApiBytes, AppState, AuthPrincipal, BACKUP_REFUSAL, JobPort, MetadataPort, ResetError,
    roles,
};
use chelabase_metadata::members::MemberBranchRole;
use chelabase_nats::{
    MemberRoleApplied, MemberRoleHandOverRequest, MemberRolesReconcileRequest, MemberRolesReply,
};
use chelabase_routes::orchestrator as ep;
use chelabase_secrets::open_member_role_password;
use chelabase_types::access::{Access, Deny, PgRole, PlatformRole, Requested};
use chelabase_types::role_name::member_role_name;

/// The audit log's target.
const AUDIT_TARGET: &str = "orchestrator::sql_editor::audit";
/// Command tags an audit line keeps at most.
const AUDIT_TAGS: usize = 50;
/// Seconds a 429 asks the client to wait.
const RETRY_AFTER_SECS: u32 = 2;
/// The longest a schema read may take, its settings and 12 queries together.
const SCHEMA_DEADLINE: Duration = Duration::from_secs(35);
/// Roles the editor never runs as (besides any `pg_` name).
const RESERVED_ROLES: [&str; 7] = [
    "cloud_admin",
    "neon_superuser",
    "authenticator",
    "chelabase_admin",
    "chelabase_auth_sync",
    // RT1 P15: realtime's replication login.
    "chelabase_realtime",
    // Object storage m3: the storage service's BYPASSRLS role.
    "chelabase_storage",
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
    /// The role to read as (what it may use), as a run with this `role` would; the owner when
    /// absent.
    pub role: Option<String>,
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

/// What the access check resolved (the caller's grant), the database and its branch (ready,
/// not a backup).
pub(crate) async fn load_authorized<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    access: Access,
    target: Target<'_>,
    branch_id: Uuid,
) -> Result<(Authorized, DatabaseRecord, BranchRecord), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let authorized = authorize(state, principal, access, target).await?;
    let record = authorized.clone().into_record()?;
    let branch = load_branch(state, &record, branch_id).await?;
    if branch.kind == BranchKind::Backup {
        return Err(conflict(ErrorCode::BackupBranch, BACKUP_REFUSAL));
    }
    if branch.status != BranchStatus::Ready {
        return Err(conflict(ErrorCode::BranchNotReady, "branch is not ready"));
    }
    Ok((authorized, record, branch))
}

pub(crate) async fn pick_pg_database<M, J>(
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
    role.to_ascii_lowercase().starts_with("pg_")
        || chelabase_types::role_name::has_reserved_prefix(role)
        || RESERVED_ROLES.contains(&role)
}

/// The 422 for a `role` the editor never takes (empty, or a system role), or `None`.
pub(crate) fn invalid_role(role: Option<&str>) -> Option<ApiError> {
    match role {
        Some("") => Some(unprocessable("`role` must not be empty (null runs as the owner)")),
        Some(role) if is_reserved(role) => Some(unprocessable(&format!(
            "the SQL editor can't run as the system role {role:?}"
        ))),
        _ => None,
    }
}

/// An open editor connection, the role to `SET ROLE` to on it, and, on a member role's
/// connection, the member (its run is handed over afterwards, R5).
pub(crate) struct Session {
    pub(crate) connected: Connected,
    pub(crate) set_role: Option<String>,
    pub(crate) member: Option<Uuid>,
}

/// The role a session `SET ROLE`s to for the requested `role`: none for no role or the owner
/// (`owner` in any case, or the owner's real name; M2).
fn set_role_for<'a>(role: Option<&'a str>, owner: &str) -> Option<&'a str> {
    match role.map(|r| Requested::from_name(r, owner)) {
        None | Some(Requested::Owner) => None,
        Some(Requested::Role(name)) => Some(name),
    }
}

/// What a refused member-role login means.
enum MemberLogin {
    /// The role is stale (28P01: a password Chelabase didn't set; 28000: it may not log in,
    /// e.g. locked out during a removal): one forced reconcile, then one more try.
    Retry,
    /// The answer to give.
    Refused(ApiError),
}

/// What a refused member-role login on `endpoint_id` means; every log line names the branch
/// and the endpoint (RT3-7).
fn member_login_refusal(branch: BranchId, endpoint_id: &str, err: ConnectError) -> MemberLogin {
    match &err {
        // RT3-7: the compute's TLS or address/certificate rule refused us (U9: a 28P01 after a
        // verified SCRAM final message is the certificate check's, and `connect_error` makes
        // it `Tls`); a forced reconcile can't help, so none is run.
        ConnectError::Tls(_) => {
            warn!(branch_id = %branch, %endpoint_id, "member login: {err}");
            MemberLogin::Refused(compute_tls_failed())
        }
        ConnectError::Rejected { code, message }
            if code == "28000"
                && (message.contains("pg_hba.conf rejects connection")
                    || message.contains("certificate")) =>
        {
            warn!(
                branch_id = %branch,
                %endpoint_id,
                "member login refused by the compute's TLS rules: {err}"
            );
            MemberLogin::Refused(compute_tls_failed())
        }
        ConnectError::Rejected { code, .. } if code == "28P01" || code == "28000" => MemberLogin::Retry,
        // No CONNECT on this Postgres database: the member's own limit, not a broken role.
        ConnectError::Rejected { code, .. } if code == "42501" => MemberLogin::Refused(
            ApiError::new(
                403,
                ErrorCode::Forbidden,
                "your database role may not connect to this Postgres database",
            )
            .with_extra("required", serde_json::json!("connect")),
        ),
        _ => MemberLogin::Refused(connect_failure(branch, endpoint_id, err)),
    }
}

/// Opens the connection a run or schema read acting as `role` uses (`authorize` has allowed it):
/// - no role, or the owner by name: the owner's connection, no `SET ROLE` (Admin);
/// - any other role: the caller's member role ([`member_session_target`]), then `SET ROLE`.
///   The member role can only become the roles it was granted and their chains, so a script's
///   `RESET ROLE` lands on it, never on the owner;
/// - a role a member role can't hold (LOGIN, owner-level or a chain through a reserved role,
///   R16, R22, R23; or one the reconciler left out): for an effective Admin, the owner's
///   connection then `SET ROLE` (I2); below Admin, 403 `forbidden{required: "role:<name>"}`
///   (without asking the worker when 6a already shows it, M5).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn open_session<M, J>(
    state: &AppState<M, J>,
    authorized: &Authorized,
    principal: &AuthPrincipal,
    record: &DatabaseRecord,
    branch: &BranchRecord,
    pg_database: &str,
    role: Option<&str>,
    record_acted: bool,
) -> Result<Session, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let owner = roles::owner_name(record);
    let as_owner = |set_role: Option<&str>| {
        let set_role = set_role.map(str::to_owned);
        async move {
            let connected = connect_as_owner(state, branch, owner, pg_database).await?;
            Ok::<_, ApiError>(Session { connected, set_role, member: None })
        }
    };
    let Some(role) = set_role_for(role, owner) else {
        return as_owner(None).await;
    };
    let graph = authorized.role_graph(state, record, branch.id.as_uuid()).await?;
    let admin = authorized
        .grant
        .as_ref()
        .is_some_and(|g| g.platform_role == PlatformRole::Admin);
    // A role the branch has: an Admin the access check let act as it may always (R22, login
    // roles included). The data API's roles needed a grant naming them there (R18).
    let admin_may = admin && graph.as_ref().is_some_and(|g| g.has(role));
    if graph.as_ref().is_some_and(|g| g.refusal(role).is_some()) {
        return if admin_may { as_owner(Some(role)).await } else { Err(role_forbidden(role)) };
    }
    let endpoint_id = branch.endpoint_id.as_deref().unwrap_or_default();
    for force in [false, true] {
        let target = match member_session_target(
            state, authorized, principal, graph.as_deref(), branch, pg_database, role, force,
            record_acted,
        )
        .await
        {
            Ok(target) => target,
            Err(err) if admin_may && err.code == ErrorCode::Forbidden => {
                return as_owner(Some(role)).await;
            }
            Err(err) => return Err(err),
        };
        let member = principal.member.as_ref().map(|m| m.id);
        match run::connect(&target).await {
            Ok(connected) => {
                return Ok(Session { connected, set_role: Some(role.to_owned()), member });
            }
            Err(err) => match member_login_refusal(branch.id, endpoint_id, err) {
                MemberLogin::Retry => {
                    warn!(
                        branch_id = %branch.id,
                        %endpoint_id,
                        "member role login refused; reconciling it"
                    );
                }
                MemberLogin::Refused(api) => return Err(api),
            },
        }
    }
    Err(reconcile_failed())
}

/// 403 `forbidden{required: "role:<name>"}`.
fn role_forbidden(role: &str) -> ApiError {
    deny_to_api(Deny::Forbidden { required: format!("role:{role}") })
}

/// Asks the worker for an act-as run's hand-over once it's over (R5; the member-role
/// invariant, `chelabase_types::role_name::member_hand_over_sql`). A failure is logged: what
/// stays with the member role is dropped by its next narrowing or removal.
pub(crate) fn hand_over_after<M, J>(
    state: Arc<AppState<M, J>>,
    branch: BranchId,
    member: Uuid,
    database: String,
    acted: String,
    backend: (i32, String),
) -> run::AfterRun
where
    M: MetadataPort,
    J: JobPort,
{
    Box::pin(async move {
        let request = MemberRoleHandOverRequest {
            branch_id: branch.as_uuid(),
            member_id: member,
            database,
            acted_role: acted,
            backend_pid: Some(backend.0),
            backend_start: Some(backend.1),
        };
        match state.js.hand_over_member_role(request).await {
            Ok(MemberRolesReply::Done { .. }) => {}
            Ok(MemberRolesReply::Failed { message }) => {
                warn!(branch_id = %branch, %member, "member role hand-over failed: {message}");
            }
            Err(err) => warn!(branch_id = %branch, %member, "member role hand-over: {err:?}"),
        }
    })
}

/// 503 `member_role_reconcile_failed`: the caller's member role couldn't be brought up to date.
fn reconcile_failed() -> ApiError {
    ApiError::new(
        503,
        ErrorCode::MemberRoleReconcileFailed,
        "your database role on this branch couldn't be prepared; try again",
    )
    .retry_after(RETRY_AFTER_SECS)
}

/// Asks the worker to reconcile `member`'s role on `branch` now
/// (`chelabase.rpc.member_roles.reconcile`) and returns what it applied for that member (none
/// when the member has no roles to act as there). A failure or timeout is 503
/// `member_role_reconcile_failed`. The SQL editor's lazy check and the table API's reads (C1)
/// both ask through here.
pub(crate) async fn reconcile_member_role<M, J>(
    state: &AppState<M, J>,
    branch: BranchId,
    member: Uuid,
) -> Result<Option<MemberRoleApplied>, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let request = MemberRolesReconcileRequest { branch_id: branch.as_uuid(), member_id: Some(member), from_ddl: false };
    match state.js.reconcile_member_roles(request).await {
        Ok(MemberRolesReply::Done { roles }) => Ok(roles.into_iter().find(|r| r.member_id == member)),
        Ok(MemberRolesReply::Failed { message }) => {
            warn!(branch_id = %branch, %member, "member-role reconcile failed: {message}");
            Err(reconcile_failed())
        }
        Err(ResetError::Timeout) => {
            warn!(branch_id = %branch, %member, "member-role reconcile timed out");
            Err(reconcile_failed())
        }
        Err(ResetError::Transport(err)) => {
            warn!(branch_id = %branch, %member, "member-role reconcile: {err}");
            Err(reconcile_failed())
        }
    }
}

/// Whether a member role holding `applied` can `SET ROLE` to `role`: one of them, or reached
/// through one's memberships as 6a records them (R21; Postgres checks the same at `SET ROLE`).
fn reaches(applied: &[String], graph: Option<&RoleGraph>, role: &str) -> bool {
    applied
        .iter()
        .any(|g| g == role || graph.is_some_and(|gr| gr.closure(g).contains(role)))
}

/// Where and as whom the caller's member role connects to act as `role` (never the owner) in
/// `pg_database` on `branch`, after the lazy check (members and roles, part 3):
/// 1. the member's `member_branch_roles` row is read;
/// 2. it's stale when missing, when it isn't `member_role_name(member)`, when its password
///    won't open, when it holds a role the effective grant no longer lists (a revoke the
///    reconciler hasn't applied yet), or when it can't reach `role`; a stale row (or `force`)
///    asks the worker to reconcile this member's role now (`chelabase.rpc.member_roles.reconcile`).
///    A failure or timeout is 503 `member_role_reconcile_failed`;
/// 3. a role the reconciled member role still can't reach (missing on the branch, or left out
///    as unsafe) is 403 `forbidden{required: "role:<name>"}`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn member_session_target<M, J>(
    state: &AppState<M, J>,
    authorized: &Authorized,
    principal: &AuthPrincipal,
    graph: Option<&RoleGraph>,
    branch: &BranchRecord,
    pg_database: &str,
    role: &str,
    force: bool,
    record_acted: bool,
) -> Result<RunTarget, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let (Some(member), Some(grant)) = (principal.member.as_ref(), authorized.grant.as_ref()) else {
        error!(branch_id = %branch.id, "act-as without a member or a grant");
        return Err(ApiError::internal());
    };
    let member = member.id;
    let Some(endpoint) = branch.endpoint_id.as_deref() else {
        return Err(conflict(ErrorCode::BranchNotReady, "branch is not ready"));
    };
    let role_name = member_role_name(member);
    let read_row = || async {
        let rows = state
            .metadata
            .member_branch_roles(branch.id.as_uuid())
            .await
            .map_err(|err| {
                error!(branch_id = %branch.id, "read member roles: {err}");
                ApiError::internal()
            })?;
        Ok::<_, ApiError>(rows.into_iter().find(|r| r.member_id == member && r.role_name == role_name))
    };
    let open = |row: &MemberBranchRole| {
        open_member_role_password(&state.secrets, branch.id.as_uuid(), &role_name, &row.password_enc).ok()
    };

    let row = read_row().await?;
    let fresh = row.as_ref().filter(|row| {
        !force
            && row.applied_roles.iter().all(|r| {
                grant.roles.contains(&PgRole::Named(r.clone()))
                    // M1: a role that has become unsafe since (LOGIN, or a chain through an
                    // owner-level or reserved role) must go.
                    && !graph.is_some_and(|g| g.refusal(r).is_some())
            })
            && reaches(&row.applied_roles, graph, role)
    });
    let password = match fresh.and_then(open) {
        Some(password) => password,
        None => {
            let mine = reconcile_member_role(state, branch.id, member).await?;
            if !mine.is_some_and(|r| reaches(&r.applied_roles, graph, role)) {
                return Err(role_forbidden(role));
            }
            match read_row().await?.as_ref().and_then(open) {
                Some(password) => password,
                None => {
                    warn!(branch_id = %branch.id, %member, "no usable member role after a reconcile");
                    return Err(reconcile_failed());
                }
            }
        }
    };
    // R31: a run records the role it acts as before it starts, so a reconcile can hand what
    // the member role makes to it if this run's hand-over never happens.
    if record_acted {
        match state
            .metadata
            .record_member_acted_role(member, branch.id.as_uuid(), role)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                warn!(branch_id = %branch.id, %member, "no member role row to record the acted role on");
            }
            Err(err) => {
                error!(branch_id = %branch.id, %member, "record the acted role: {err}");
                return Err(ApiError::internal());
            }
        }
    }
    let addr = compute_addr(state, branch.id, endpoint).await?;
    let tls = run_tls(state, branch.id, endpoint)?;
    Ok(RunTarget {
        addr,
        user: role_name,
        password,
        database: pg_database.to_owned(),
        tls,
    })
}

/// A fresh owner connection to `database` on the branch's compute.
pub(crate) async fn connect_as_owner<M, J>(
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
        tls: target.tls,
    };
    let endpoint_id = branch.endpoint_id.as_deref().unwrap_or_default();
    run::connect(&target)
        .await
        .map_err(|err| connect_failure(branch.id, endpoint_id, err))
}

/// The answer for a failed editor connection to `endpoint_id`, by the server's SQLSTATE when it
/// answered; every log line names the branch and the endpoint (RT3-7).
fn connect_failure(branch: BranchId, endpoint_id: &str, err: ConnectError) -> ApiError {
    match err {
        ConnectError::Tls(message) => {
            warn!(branch_id = %branch, %endpoint_id, "SQL editor connect, TLS: {message}");
            compute_tls_failed()
        }
        ConnectError::Unreachable(message) => {
            warn!(branch_id = %branch, %endpoint_id, "SQL editor connect: {message}");
            unavailable("the branch's compute can't be reached right now; try again")
        }
        ConnectError::Rejected { code, message } => {
            warn!(
                branch_id = %branch,
                %endpoint_id,
                code = %code,
                "SQL editor connect refused: {message}"
            );
            match code.as_str() {
                // The sealed password is the one Chelabase set: it was changed in SQL.
                "28P01" => conflict(ErrorCode::OwnerPasswordUnknown, OWNER_PASSWORD_UNKNOWN),
                "3D000" => database_not_found(),
                "57P03" => unavailable("the branch's compute is starting up; try again"),
                c if c.starts_with("53") => unavailable(
                    "the branch's compute is out of connections or resources; try again",
                ),
                _ => {
                    error!(
                        branch_id = %branch,
                        %endpoint_id,
                        code = %code,
                        "SQL editor connect refused: {message}"
                    );
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
    description = "Runs a script on one of the branch's Postgres databases on a fresh connection as the database's owner (Admin only), or, with `role`, as that role: the connection logs in as your own hidden member role on the branch, which can only become the roles you were granted (and what they can become), then runs `SET ROLE` to `role`, so a script that resets its role is never the owner. When the run ends (however it ends), its session is ended, whatever the script made as the member role itself is handed to `role`, and every privilege, default privilege or policy naming the member role is dropped. An Admin acting as a role a member role can't hold (LOGIN, owner-level, or reaching a reserved role) uses the owner's connection and `SET ROLE`; naming the owner runs as the owner. The script is sent unchanged as one simple query, so multi-statement scripts, explicit transactions and commands like `VACUUM` work. Unless `confirm` is true, risky statements (drops, truncates, `DELETE`/`UPDATE` without `WHERE`, also under `EXPLAIN ANALYZE` or `PREPARE`, and anything that doesn't parse) and, on a protected branch, every statement that isn't a read are held back with 409 `sql_confirm_required`. Errors before the connection opens are JSON; then the answer is `200 application/x-ndjson`, one JSON event per line: `notice`, `columns` (`statement`, `columns: [{name, type}]`), `rows` (`statement`, at most 200 rows of text-or-null values), `complete` (`statement`, `tag`, `rows`, `truncated`: over 1,000 rows), `error` (`statement`, `code`, `message`, `detail`, `hint`, `position`: 1-based in the sent text), `heartbeat` (after 15 s without another event) and, always last, `done` (`outcome`: `ok`, `error`, `cancelled`, `timeout` or `truncated_response` past 10 MiB; `duration_ms`). Closing the stream cancels the run. Every run is logged with its command tags and a SHA-256 of the script, never the script.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
    ),
    request_body = RunSqlRequest,
    responses(
        (status = 200, description = "The run's events as NDJSON, ending with `done`", content_type = "application/x-ndjson", body = String),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant. Besides the access refusals: `required: \"role:<name>\"` also when the role doesn't exist on the branch or your member role can't hold it; `required: \"connect\"` when your member role may not connect to this Postgres database", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the Postgres database doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`sql_confirm_required`: the guard held statements back; `error.statements` lists them (`index`, `kind`: pg_query's node name or `unparsed`, `text_preview`: the first 200 characters, `reason`: `risky` or `protected`); send again with `confirm: true` to run them. `run_id_in_use`: you have a run with this id in flight; `owner_password_unknown`: the owner's password was set in SQL (reset it); `backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the body isn't the run's shape, `sql` isn't 1 byte to 1 MiB or holds a NUL byte, `timeout_ms` isn't 100 to 900000, `role` is empty, or `role` is a system role", body = ErrorEnvelope),
        (status = 429, description = "`sql_run_limit`: you have 4 runs in flight, or the Postgres database has 4 (`Retry-After`)", body = ErrorEnvelope),
        (status = 500, description = "`credentials_unavailable`: the stored owner password can't be opened with the current secrets key; `internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: the compute isn't running, can't be reached, is starting up or is out of connections right now (`Retry-After`); `member_role_reconcile_failed`: your member role on the branch couldn't be brought up to date (`Retry-After`)", body = ErrorEnvelope),
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
    // The request is read first: the access check needs the role and the Postgres database.
    let request: RunSqlRequest = or_answer!(parse_body(&body));
    if let Some(message) = invalid_sql(&request.sql) {
        return unprocessable(message).into_response();
    }
    let timeout_ms = request.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS);
    if !(MIN_TIMEOUT_MS..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
        return unprocessable("`timeout_ms` must be 100 to 900000").into_response();
    }
    if let Some(refusal) = invalid_role(request.role.as_deref()) {
        return refusal.into_response();
    }
    let target = Target::Role {
        database: id,
        branch: branch_id,
        pg_database: request.database.as_deref().unwrap_or(""),
        role: request.role.as_deref().map_or(Requested::Owner, Requested::Role),
    };
    let (authorized, record, branch) = or_answer!(
        load_authorized(&state, &principal, ep::RUN_SQL.policy.access, target, branch_id).await
    );
    let row = or_answer!(pick_pg_database(&state, &branch, request.database.as_deref()).await);

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

    let guard = match state.sql_editor.runs.reserve(
        &principal.sub,
        request.run_id,
        principal.member.as_ref().map(|m| m.id),
        branch.id,
        &row.name,
    ) {
        Ok(guard) => guard,
        Err(err) => return reserve_refusal(err),
    };
    let session = or_answer!(
        open_session(
            &state,
            &authorized,
            &principal,
            &record,
            &branch,
            &row.name,
            request.role.as_deref(),
            true,
        )
        .await
    );

    let mut audit = audit();
    let after = match (session.member, session.set_role.as_ref()) {
        (Some(member), Some(acted)) => Some(hand_over_after(
            state.clone(),
            branch.id,
            member,
            row.name.clone(),
            acted.clone(),
            (
                session.connected.backend_pid(),
                session.connected.backend_start().to_owned(),
            ),
        )),
        _ => None,
    };
    let spec = RunSpec {
        sql: request.sql,
        role: session.set_role,
        timeout: Duration::from_millis(timeout_ms),
        after,
    };
    let finished = audit.finished.clone();
    let events = run::run_reporting(session.connected, spec, guard, finished).map(move |event| {
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
    description = "Cancels a run in flight that you started (an Admin may cancel any): Postgres's cancel request is sent and the run's stream ends with `done` (`outcome: cancelled`). Closing the stream cancels too.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("run_id" = Uuid, Path, description = "The run's id, as the client chose it"),
    ),
    responses(
        (status = 202, description = "The cancel was sent", body = Object),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant; `forbidden`: the run was started by someone else and you aren't an Admin (`required: \"admin\"`)", body = ErrorEnvelope),
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
    let target = Target::Branch { database: id, branch: branch_id };
    let (authorized, _, _) = or_answer!(
        load_authorized(&state, &principal, ep::CANCEL_SQL_RUN.policy.access, target, branch_id).await
    );
    let by = match (principal.member.as_ref(), authorized.grant.as_ref()) {
        (_, Some(grant)) if grant.platform_role == PlatformRole::Admin => Canceller::Admin,
        (Some(member), _) => Canceller::Member(member.id),
        (None, _) => return ApiError::internal().into_response(),
    };
    match state.sql_editor.runs.cancel(run_id, BranchId(branch_id), by) {
        Cancelled::Done => (StatusCode::ACCEPTED, Json(serde_json::json!({}))).into_response(),
        Cancelled::NotYours => ApiError::new(
            403,
            ErrorCode::Forbidden,
            "only the run's starter or an Admin may cancel it",
        )
        .with_extra("required", serde_json::json!("admin"))
        .into_response(),
        Cancelled::Missing => ApiError::not_found("run not found").into_response(),
    }
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/sql/schema",
    operation_id = "getSqlSchema",
    tag = "sql-editor",
    summary = "The schema for the SQL editor's completion",
    description = "Everything the database's owner (or, with `role`, that role) can use in one Postgres database of the branch, read on the same kind of fresh connection a run with that `role` uses, with postgres-language-server's own introspection (the version `PGLS_VERSION` pins), as the JSON its WASM build's `setSchema` takes. Not cached on the server.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        SqlSchemaQuery,
    ),
    responses(
        (status = 200, description = "postgres-language-server's schema JSON (at most 8 MiB)", body = Object),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant. Besides the access refusals: `required: \"role:<name>\"` also when the role doesn't exist on the branch or your member role can't hold it; `required: \"connect\"` when your member role may not connect to this Postgres database", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the Postgres database doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`owner_password_unknown`: the owner's password was set in SQL (reset it); `backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the query string is malformed, `role` is empty or a system role; `schema_too_large`: the schema is over 8 MiB (use keyword completion)", body = ErrorEnvelope),
        (status = 429, description = "`sql_run_limit`: a schema read takes a run slot while it lasts, and you have 4 runs in flight, or the Postgres database has 4 (`Retry-After`)", body = ErrorEnvelope),
        (status = 500, description = "`credentials_unavailable`: the stored owner password can't be opened with the current secrets key; `internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: the compute isn't running, can't be reached, is starting up or is out of connections right now (`Retry-After`); `member_role_reconcile_failed`: your member role on the branch couldn't be brought up to date (`Retry-After`)", body = ErrorEnvelope),
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
    let Ok(axum::extract::Query(query)) = query else {
        return unprocessable("the query string is malformed").into_response();
    };
    if let Some(refusal) = invalid_role(query.role.as_deref()) {
        return refusal.into_response();
    }
    // Read on the same session a run as this role would use.
    let target = Target::Role {
        database: id,
        branch: branch_id,
        pg_database: query.database.as_deref().unwrap_or(""),
        role: query.role.as_deref().map_or(Requested::Owner, Requested::Role),
    };
    let (authorized, record, branch) = or_answer!(
        load_authorized(&state, &principal, ep::GET_SQL_SCHEMA.policy.access, target, branch_id).await
    );
    let row = or_answer!(pick_pg_database(&state, &branch, query.database.as_deref()).await);
    // A schema read holds a connection for up to 35 s: it takes a run slot meanwhile.
    let _slot = match state.sql_editor.runs.reserve(
        &principal.sub,
        Uuid::new_v4(),
        principal.member.as_ref().map(|m| m.id),
        branch.id,
        &row.name,
    ) {
        Ok(slot) => slot,
        Err(err) => return reserve_refusal(err),
    };
    let session = or_answer!(
        open_session(
            &state,
            &authorized,
            &principal,
            &record,
            &branch,
            &row.name,
            query.role.as_deref(),
            false,
        )
        .await
    );
    // The pgls queries are fixed reads: they make nothing, so a schema read needs no hand-over.
    let client = session.connected.client();
    let result = or_answer!(
        within(SCHEMA_DEADLINE, schema::introspect(client, session.set_role.as_deref())).await
    );
    drop(session);
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
        Err(schema::SchemaError::Role(message)) => {
            warn!(branch_id = %branch.id, "SQL editor schema: {message}");
            return role_forbidden(query.role.as_deref().unwrap_or_default()).into_response();
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
    use crate::sql_editor::fake_pg::{self, Case};

    /// The endpoint every refusal's log line names (RT3-7, T10 m2).
    const EP: &str = "ep-log-1";

    // RT1 S23/P15: realtime's replication login is a system role the editor never runs as,
    // named in the explicit list (not only caught by the `chelabase_` prefix).
    #[test]
    fn the_realtime_role_is_reserved() {
        assert!(RESERVED_ROLES.contains(&"chelabase_realtime"), "{RESERVED_ROLES:?}");
        assert!(invalid_role(Some("chelabase_realtime")).is_some());
    }

    // Object storage m3: the storage service's BYPASSRLS role (and any `chelabase_storage…`
    // name), named in the explicit list too.
    #[test]
    fn the_storage_role_is_reserved() {
        assert!(RESERVED_ROLES.contains(&"chelabase_storage"), "{RESERVED_ROLES:?}");
        for role in ["chelabase_storage", "chelabase_storage_x", "Chelabase_Storage"] {
            assert!(invalid_role(Some(role)).is_some(), "{role}");
        }
    }

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
            let api = connect_failure(branch, EP, err);
            assert_eq!((api.status, api.code), (status, code), "{label}");
            if status == 503 {
                assert_eq!(api.retry_after, Some(RETRY_AFTER_SECS), "{label}");
            }
        }
        assert_eq!(
            connect_failure(branch, EP, rejected("3D000")).message,
            "database not found"
        );
        assert_eq!(
            connect_failure(branch, EP, rejected("28P01")).message,
            OWNER_PASSWORD_UNKNOWN
        );
    }

    // M2 (T9 review): naming the owner (`owner` in any case, or its real name) runs as the owner
    // with no `SET ROLE`.
    #[test]
    fn owner_by_name_sets_no_role() {
        for (role, want) in [
            (None, None),
            (Some("owner"), None),
            (Some("OWNER"), None),
            (Some("Owner"), None),
            (Some("app_owner"), None),
            (Some("billing"), Some("billing")),
            (Some("ownerish"), Some("ownerish")),
        ] {
            assert_eq!(set_role_for(role, "app_owner"), want, "{role:?}");
        }
    }

    // M6 (T9 review): a refused member-role login. A password (28P01) or a login the role may
    // not make (28000: locked out during a removal) is a stale role: one forced reconcile, then
    // one more try. No CONNECT on the database (42501) is the member's own limit: 403.
    #[test]
    fn member_login_refusals_map() {
        let rejected = |code: &str| ConnectError::Rejected { code: code.into(), message: "no".into() };
        let branch = BranchId::new();
        for code in ["28P01", "28000"] {
            assert!(matches!(member_login_refusal(branch, EP, rejected(code)), MemberLogin::Retry), "{code}");
        }
        let MemberLogin::Refused(api) = member_login_refusal(branch, EP, rejected("42501")) else {
            panic!("42501 isn't a refusal");
        };
        assert_eq!((api.status, api.code), (403, ErrorCode::Forbidden));
        assert_eq!(api.extra, vec![("required".to_string(), serde_json::json!("connect"))]);
        for (code, status) in [("3D000", 404), ("57P03", 503), ("XX000", 500)] {
            let MemberLogin::Refused(api) = member_login_refusal(branch, EP, rejected(code)) else {
                panic!("{code}");
            };
            assert_eq!(api.status, status, "{code}");
        }
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

    // RT3-7: a TLS or certificate problem is a typed 503 without the forced reconcile (it
    // can't help); any other refused login keeps today's path.
    // U9 (replaces T10's third message, `certificate validation … CN mismatch`, which
    // Postgres never sends: a CN mismatch reaches the client only as 28P01, after a SCRAM final
    // message that verified, `auth.c:294-298`, `:2789-2800`).
    #[tokio::test]
    async fn tls_and_hba_refusals_are_compute_tls_failed() {
        let branch = BranchId::new();
        let tls = |e: ConnectError| match member_login_refusal(branch, EP, e) {
            MemberLogin::Refused(api) => (api.status, api.code, api.retry_after.is_some()),
            MemberLogin::Retry => panic!("a TLS problem isn't a stale role"),
        };
        assert_eq!(
            tls(ConnectError::Tls("invalid peer certificate: UnknownIssuer".into())),
            (503, ErrorCode::ComputeTlsFailed, true)
        );
        for message in [
            "pg_hba.conf rejects connection for host \"10.213.4.12\", user \"cb_m_ab\", database \"app\", SSL encryption",
            "connection requires a valid client certificate",
        ] {
            let rejected = ConnectError::Rejected { code: "28000".into(), message: message.into() };
            assert_eq!(tls(rejected), (503, ErrorCode::ComputeTlsFailed, true), "{message}");
        }
        let compute = fake_pg::compute_tls();
        // U9: the password verified, the certificate check refused us.
        let cn_mismatch = run::connect_error(&Case::VerifiedThen28P01.error().await, &compute);
        assert_eq!(tls(cn_mismatch), (503, ErrorCode::ComputeTlsFailed, true));
        // A 28P01 without a verified final message is a wrong password: a stale role.
        let wrong = run::connect_error(&Case::WrongPassword.error().await, &compute);
        assert!(matches!(member_login_refusal(branch, EP, wrong), MemberLogin::Retry));
        let stale = ConnectError::Rejected { code: "28000".into(), message: "role \"cb_m_ab\" is not permitted to log in".into() };
        assert!(matches!(member_login_refusal(branch, EP, stale), MemberLogin::Retry));
    }

    /// How a refusal ends, for the invariant table.
    #[derive(Debug, PartialEq)]
    enum Answer {
        /// 503 `compute_tls_failed` with `retry_after`, no reconcile.
        TlsFailed,
        /// A stale role: one forced reconcile.
        Retry,
        /// Any other answer.
        Other(u16, ErrorCode),
    }

    fn answer(api: ApiError) -> Answer {
        if api.code == ErrorCode::ComputeTlsFailed {
            assert_eq!((api.status, api.retry_after.is_some()), (503, true), "{api:?}");
            Answer::TlsFailed
        } else {
            Answer::Other(api.status, api.code)
        }
    }

    // Invariant (fork round 3, final fix round; T10 I1, I2, m1, m4 and U9): a compute connection
    // failure is `compute_tls_failed` iff the TLS layer or the post-password certificate check
    // refused it; a wrong password or a stale role is never `compute_tls_failed`, and a TLS
    // failure never triggers a reconcile. Every case of `fake_pg::TABLE`, through
    // `connect_error` as a compute connection, for a member login and for the owner's.
    #[tokio::test]
    async fn compute_tls_failed_iff_tls_or_the_certificate_check_refused() {
        use Case::*;
        use Answer::{Other, Retry, TlsFailed};
        let branch = BranchId::new();
        let compute = fake_pg::compute_tls();
        for &(case, _, _) in fake_pg::TABLE {
            let (member, owner) = match case {
                TlsRefusedByServer | HandshakeUnknownIssuer | HandshakeNested
                | AlertAfterHandshake | ChannelBindingRefused | VerifiedThen28P01 => {
                    (TlsFailed, TlsFailed)
                }
                Refused | Timeout | HandshakeReset | ResetAfterHandshake | Closed
                | UnexpectedMessage | Malformed | NoPassword | BadServerSignature => (
                    Other(503, ErrorCode::Unavailable),
                    Other(503, ErrorCode::Unavailable),
                ),
                WrongPassword => (Retry, Other(409, ErrorCode::OwnerPasswordUnknown)),
                // The compute's address rule (RT3-7) for a member; the owner's line has none.
                HbaRejects => (TlsFailed, Other(500, ErrorCode::Internal)),
                VerifiedThenNotPermitted => (Retry, Other(500, ErrorCode::Internal)),
                VerifiedThenNoConnect => {
                    (Other(403, ErrorCode::Forbidden), Other(500, ErrorCode::Internal))
                }
                VerifiedThenTooMany => (
                    Other(503, ErrorCode::Unavailable),
                    Other(503, ErrorCode::Unavailable),
                ),
                VerifiedThenNoDatabase => {
                    (Other(404, ErrorCode::NotFound), Other(404, ErrorCode::NotFound))
                }
            };
            let err = run::connect_error(&case.error().await, &compute);
            let got = match member_login_refusal(branch, EP, err) {
                MemberLogin::Retry => Retry,
                MemberLogin::Refused(api) => answer(api),
            };
            assert_eq!(got, member, "{case:?}, member login");
            let err = run::connect_error(&case.error().await, &compute);
            assert_eq!(answer(connect_failure(branch, EP, err)), owner, "{case:?}, owner");
        }
    }

    /// A `tracing` writer into a shared buffer.
    #[derive(Clone, Default)]
    struct Logs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Logs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // RT3-7, T10 m2: every refused connection's log line names the branch and the endpoint.
    #[test]
    fn refusal_logs_name_the_endpoint() {
        let logs = Logs::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || writer.clone())
            .finish();
        let branch = BranchId::new();
        tracing::subscriber::with_default(subscriber, || {
            let errors = || {
                [
                    ConnectError::Tls("invalid peer certificate: UnknownIssuer".into()),
                    ConnectError::Unreachable("refused".into()),
                    ConnectError::Rejected {
                        code: "28000".into(),
                        message: "pg_hba.conf rejects connection for host \"10.213.4.12\"".into(),
                    },
                    rejected("28P01"),
                    rejected("XX000"),
                    rejected("53300"),
                ]
            };
            for err in errors() {
                let _ = member_login_refusal(branch, EP, err);
            }
            for err in errors() {
                let _ = connect_failure(branch, EP, err);
            }
        });
        let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.len() >= 8, "{text}");
        for line in lines {
            assert!(line.contains(&format!("branch_id={branch}")), "{line}");
            assert!(line.contains(&format!("endpoint_id={EP}")), "{line}");
        }
    }
}
