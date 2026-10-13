//! Branch routes: list, create, reveal, reset password, reset to parent, restore, delete, and
//! the database settings `PATCH`. The worker does the Neon work; these handlers check every
//! refusal, move the row to its working status and publish the job.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json,
    body::Bytes,
    extract::{Query, State, rejection::QueryRejection},
    http::{StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use chelabase_metadata::{
    BranchDatabase, BranchDeleteOutcome, BranchKind, BranchRecord, BranchStatus, ClaimOutcome,
    CreateBranchOutcome, DataApiState, DatabaseOrigin, DatabaseRecord, NewBranch, SettingsPatch,
};
use chelabase_nats::{BranchJob, DatabaseSettingsJob, JobEnvelope, PointInTime};
use chelabase_types::api_error::{ApiError, ErrorCode};
use chelabase_types::{ApiJson, ApiPath, BranchId};
use chrono::{DateTime, Utc};
use chelabase_types::api_error::ErrorEnvelope;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::access::{authorize_database, Target};
use crate::{
    merges, AppState, ApiBytes, AuthPrincipal, BACKUP_REFUSAL, ConnectionDetails, JobPort, MetadataPort, OperationAccepted,
    proxy_address, reset_branch_password, reveal,
};
use chelabase_routes::orchestrator as ep;
use chelabase_types::access::Requested;

/// Longest branch name, and what a backup's name must fit in.
const MAX_NAME_LEN: usize = 63;
/// Bounds of `history_window`, in seconds: one hour to 30 days.
const MIN_WINDOW_SECS: i64 = 3600;
const MAX_WINDOW_SECS: i64 = 30 * 24 * 3600;
/// Bounds of `max_branches`.
const MAX_BRANCHES_LIMIT: i64 = 20;

pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new()
        .routes(routes!(list_branches, create_branch))
        .routes(routes!(delete_branch, update_branch))
        .routes(routes!(list_branch_databases))
        .routes(routes!(branch_connection))
        .routes(routes!(branch_reset_password))
        .routes(routes!(reset_to_parent))
        .routes(routes!(restore))
}

/// A branch as the API shows it. `host` and `port` only for a ready `normal` branch with an
/// endpoint.
#[derive(Debug, Serialize, ToSchema)]
struct Branch {
    id: BranchId,
    name: String,
    #[schema(required = true)]
    parent_branch_id: Option<BranchId>,
    kind: BranchKind,
    status: BranchStatus,
    #[schema(required = true)]
    host: Option<String>,
    #[schema(required = true)]
    port: Option<u16>,
    #[schema(required = true)]
    branched_at_time: Option<DateTime<Utc>>,
    #[schema(required = true)]
    branched_at_lsn: Option<String>,
    created_at: DateTime<Utc>,
    /// Where the branch's history begins (null when unknown); Studio's lower bound for a point.
    #[schema(required = true)]
    history_start: Option<DateTime<Utc>>,
    #[schema(required = true)]
    error: Option<String>,
    /// A protected branch can't be deleted or reset to its parent (restore still works), and
    /// its database can't be deleted. Set with `updateBranch`.
    protected: bool,
    /// A ready branch with no recorded main database: it was dropped in SQL. The data API is
    /// unavailable on the branch until a database of the main name exists again.
    main_database_missing: bool,
}

impl Branch {
    fn new(b: BranchRecord, proxy: &crate::ProxyAddress, main_database_missing: bool) -> Self {
        let endpoint = (b.kind == BranchKind::Normal)
            .then_some(b.endpoint_id.as_deref())
            .flatten();
        let (host, port) = proxy_address(
            if b.status == BranchStatus::Ready {
                chelabase_metadata::DatabaseStatus::Ready
            } else {
                chelabase_metadata::DatabaseStatus::Pending
            },
            endpoint,
            proxy,
        )
        .unzip();
        Self {
            id: b.id,
            name: b.name,
            parent_branch_id: b.parent_branch_id,
            kind: b.kind,
            status: b.status,
            host,
            port,
            branched_at_time: b.branched_at_time,
            branched_at_lsn: b.branched_at_lsn,
            created_at: b.created_at,
            history_start: b.history_start,
            error: b.error,
            protected: b.protected,
            main_database_missing,
        }
    }
}

/// A ready `normal` branch with an endpoint is the one that should have a `main` row.
fn main_database_expected(b: &BranchRecord) -> bool {
    b.kind == BranchKind::Normal && b.status == BranchStatus::Ready && b.endpoint_id.is_some()
}

/// Of the list's branches that should have one, those that do, in one query. `None` when
/// nothing was asked or the lookup failed (logged): every branch then reads as present.
async fn branches_with_main_database<M, J>(
    state: &AppState<M, J>,
    branches: &[BranchRecord],
) -> Option<std::collections::HashSet<BranchId>>
where
    M: MetadataPort,
    J: JobPort,
{
    let ids: Vec<BranchId> = branches
        .iter()
        .filter(|b| main_database_expected(b))
        .map(|b| b.id)
        .collect();
    if ids.is_empty() {
        return None;
    }
    match state.metadata.branches_with_main_database(&ids).await {
        Ok(set) => Some(set),
        Err(err) => {
            warn!("load branch databases: {err}");
            None
        }
    }
}

/// Whether a ready `normal` branch has no `main` row in `branch_databases`. A branch that
/// isn't ready has nothing recorded yet, and a failed lookup is logged and reads as present.
async fn main_database_missing<M, J>(state: &AppState<M, J>, b: &BranchRecord) -> bool
where
    M: MetadataPort,
    J: JobPort,
{
    if !main_database_expected(b) {
        return false;
    }
    match state.metadata.branch_databases(b.id).await {
        Ok(rows) => !rows.iter().any(|d| d.origin == DatabaseOrigin::Main),
        Err(err) => {
            warn!(branch_id = %b.id, "load branch databases: {err}");
            false
        }
    }
}

/// A branch as the API shows it.
async fn branch_view<M, J>(state: &AppState<M, J>, b: BranchRecord) -> Branch
where
    M: MetadataPort,
    J: JobPort,
{
    let missing = main_database_missing(state, &b).await;
    Branch::new(b, &state.proxy, missing)
}

/// Parses a JSON body with the reason on failure: text that isn't JSON (syntax error, empty or
/// cut off) is a 400; well-formed JSON of the wrong shape or with a wrong type is a 422.
pub(crate) fn parse_body<T: DeserializeOwned>(body: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|err| {
        let message = format!("invalid request body: {err}");
        match err.classify() {
            serde_json::error::Category::Data => unprocessable(&message),
            _ => ApiError::invalid_request(400, message),
        }
    })
}

pub(crate) fn unprocessable(message: &str) -> ApiError {
    ApiError::invalid_request(422, message)
}

/// A 409 with its code.
pub(crate) fn conflict(code: ErrorCode, message: impl Into<String>) -> ApiError {
    ApiError::new(409, code, message)
}

/// The job queue didn't take a job.
pub(crate) fn enqueue_failed() -> ApiError {
    ApiError::new(503, ErrorCode::WorkerUnavailable, "failed to enqueue branch job")
}

/// 1-63 characters from `[a-z0-9_-]`, starting with a letter or digit; `main` is the root's.
fn validate_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if !ok {
        return Err(format!(
            "invalid branch name {name:?}: use 1-{MAX_NAME_LEN} characters from a-z, 0-9, _ and -, \
             starting with a letter or digit"
        ));
    }
    if name == "main" {
        return Err("the name main is reserved for the database's first branch".into());
    }
    Ok(())
}

mod docs {
    use chrono::{DateTime, Utc};
    use serde::Serialize;
    use utoipa::ToSchema;

    /// Where in a branch's history: exactly one of a time or an LSN.
    #[derive(Serialize, ToSchema)]
    #[serde(untagged)]
    #[allow(dead_code)]
    pub(super) enum PointInTime {
        /// A moment, RFC 3339.
        Timestamp { timestamp: DateTime<Utc> },
        /// A Postgres LSN, `hex/hex`.
        Lsn { lsn: String },
    }
}

/// `{ "timestamp": "<rfc3339>" }` or `{ "lsn": "<hex>/<hex>" }`, exactly one.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AtBody {
    timestamp: Option<String>,
    lsn: Option<String>,
}

impl AtBody {
    fn parse(self) -> Result<PointInTime, String> {
        match (self.timestamp, self.lsn) {
            (Some(ts), None) => DateTime::parse_from_rfc3339(&ts)
                .map(|t| PointInTime::Timestamp(t.with_timezone(&Utc)))
                .map_err(|_| {
                    format!("invalid timestamp {ts:?}: use RFC 3339, like 2026-09-30T12:00:00Z")
                }),
            (None, Some(lsn)) if valid_lsn(&lsn) => Ok(PointInTime::Lsn(lsn)),
            (None, Some(lsn)) => Err(format!("invalid LSN {lsn:?}: use hex/hex, like 0/16B3748")),
            _ => Err("at needs exactly one of timestamp or lsn".into()),
        }
    }
}

/// An `X/Y` LSN as a number, to compare.
fn lsn_value(lsn: &str) -> Option<u64> {
    let (hi, lo) = lsn.split_once('/')?;
    Some((u64::from_str_radix(hi, 16).ok()? << 32) | u64::from_str_radix(lo, 16).ok()?)
}

/// T14 M8: a restore point within the branch's own history: not before its branch point (an
/// LSN) or its `history_start` (a time). Before them lies the parent's older state. The worker
/// checks the resolved point again.
fn within_history(branch: &BranchRecord, at: &PointInTime) -> Result<(), String> {
    match at {
        PointInTime::Lsn(lsn) => {
            let start = branch.branched_at_lsn.as_deref();
            match (lsn_value(lsn), start.and_then(lsn_value)) {
                (Some(point), Some(begins)) if point < begins => Err(format!(
                    "the LSN {lsn} is before {}'s history begins (its branch point, {})",
                    branch.name,
                    start.unwrap_or_default()
                )),
                _ => Ok(()),
            }
        }
        PointInTime::Timestamp(time) => match branch.history_start {
            Some(begins) if *time < begins => Err(format!(
                "the time {} is before {}'s history begins ({})",
                time.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
                branch.name,
                begins.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
            )),
            _ => Ok(()),
        },
    }
}

/// Postgres text form: two hex numbers of up to 8 digits around a slash.
fn valid_lsn(lsn: &str) -> bool {
    let part = |p: &str| (1..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_hexdigit());
    lsn.split_once('/')
        .is_some_and(|(hi, lo)| part(hi) && part(lo))
}

/// The branch of the database, or the 404 to send.
pub(crate) async fn load_branch<M, J>(
    state: &AppState<M, J>,
    database: &DatabaseRecord,
    id: Uuid,
) -> Result<BranchRecord, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    match state.metadata.get_branch(BranchId(id)).await {
        // A rehearsal is the merge worker's throwaway: no route sees it (P14).
        Ok(Some(b))
            if b.database_id == database.database_id
                && b.status != BranchStatus::Deleted
                && b.kind != BranchKind::Rehearsal =>
        {
            Ok(b)
        }
        Ok(_) => Err(ApiError::not_found("branch not found")),
        Err(err) => {
            error!("load branch: {err}");
            Err(ApiError::internal())
        }
    }
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches",
    operation_id = "listBranches",
    tag = "branches",
    summary = "List a database's branches",
    description = "A flat list; `parent_branch_id` makes the tree.",
    params(("id" = Uuid, Path, description = "Database id")),
    responses(
        (status = 200, description = "The branches that aren't deleted, oldest first, backups included", body = Vec<Branch>),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database doesn't exist, or belongs to another tenant", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_branches<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match authorize_database(&state, &principal, ep::LIST_BRANCHES.policy.access, Target::Database(id)).await {
        Ok(record) => record,
        Err(resp) => return resp.into_response(),
    };
    match state.metadata.list_branches(record.database_id).await {
        Ok(mut branches) => {
            // Rehearsal branches are the merge worker's (the worker lists them itself).
            branches.retain(|b| b.kind != BranchKind::Rehearsal);
            let with_main = branches_with_main_database(&state, &branches).await;
            let views: Vec<Branch> = branches
                .into_iter()
                .map(|b| {
                    let missing = main_database_expected(&b)
                        && with_main.as_ref().is_some_and(|set| !set.contains(&b.id));
                    Branch::new(b, &state.proxy, missing)
                })
                .collect();
            Json(views).into_response()
        }
        Err(err) => {
            error!("list branches: {err}");
            ApiError::internal().into_response()
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateBranchRequest {
    name: String,
    /// The branch to start from; main when omitted.
    parent_branch_id: Option<BranchId>,
    /// A point in the parent's history; its present when omitted.
    #[schema(value_type = Option<docs::PointInTime>)]
    at: Option<AtBody>,
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/branches",
    operation_id = "createBranch",
    tag = "branches",
    summary = "Create a branch",
    description = "Branches from `parent_branch_id` (main when omitted), at `at` (the parent's present when omitted).",
    params(("id" = Uuid, Path, description = "Database id")),
    request_body = CreateBranchRequest,
    responses(
        (status = 202, description = "Accepted; the branch is created in the background (poll it until `ready`)", body = Branch),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the parent branch doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`name_taken`: a branch with this name exists; `branch_cap_reached`: the database has `max_branches`; `branch_not_ready`: the parent isn't ready; `database_not_ready`: the database is being deleted", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the body is valid JSON but the wrong shape, the name is invalid, or the point in time is invalid", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`worker_unavailable`: the worker or job queue can't be reached", body = ErrorEnvelope),
    ),
)]
async fn create_branch<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
    ApiBytes(body): ApiBytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match authorize_database(&state, &principal, ep::CREATE_BRANCH.policy.access, Target::Database(id)).await {
        Ok(record) => record,
        Err(resp) => return resp.into_response(),
    };
    let request: CreateBranchRequest = match parse_body(&body) {
        Ok(request) => request,
        Err(resp) => return resp.into_response(),
    };
    if let Err(message) = validate_name(&request.name) {
        return unprocessable(&message).into_response();
    }
    let at = match request.at.map(AtBody::parse).transpose() {
        Ok(at) => at,
        Err(message) => return unprocessable(&message).into_response(),
    };
    let database_id = record.database_id;
    let parent = request
        .parent_branch_id
        .unwrap_or(BranchId::from(database_id));
    match load_branch(&state, &record, parent.as_uuid()).await {
        Ok(_) => {}
        Err(resp) if resp.status == 404 => return ApiError::not_found("parent branch not found").into_response(),
        Err(resp) => return resp.into_response(),
    }
    let branch_id = BranchId::new();
    let (branched_at_time, branched_at_lsn) = match &at {
        Some(PointInTime::Timestamp(t)) => (Some(*t), None),
        Some(PointInTime::Lsn(lsn)) => (None, Some(lsn.as_str())),
        None => (None, None),
    };
    let outcome = state
        .metadata
        .create_branch(&NewBranch {
            id: branch_id,
            database_id,
            name: &request.name,
            parent_branch_id: parent,
            branched_at_time,
            branched_at_lsn,
        })
        .await;
    match outcome {
        Ok(CreateBranchOutcome::Created) => {}
        Ok(CreateBranchOutcome::NameTaken) => {
            return conflict(
                ErrorCode::NameTaken,
                format!("a branch named {} already exists", request.name),
            )
            .into_response();
        }
        Ok(CreateBranchOutcome::CapReached { max }) => {
            return conflict(
                ErrorCode::BranchCapReached,
                format!(
                    "this database already has {max} branches besides main; raise max_branches \
                     with PATCH /v1/databases/{database_id}"
                ),
            )
            .into_response();
        }
        Ok(CreateBranchOutcome::ParentNotReady) => {
            return conflict(ErrorCode::BranchNotReady, "the parent branch is not ready")
                .into_response();
        }
        Ok(CreateBranchOutcome::DatabaseNotLive) => {
            return conflict(ErrorCode::DatabaseNotReady, "database is being deleted")
                .into_response();
        }
        Err(err) => {
            error!("create branch: {err}");
            return ApiError::internal().into_response();
        }
    }
    let job = JobEnvelope {
        job_id: Uuid::new_v4(),
        payload: BranchJob::Create { branch_id, at },
    };
    if let Err(err) = state.js.publish_branch(&job).await {
        error!(%database_id, %branch_id, "publish branch job failed: {err}");
        let _ = state
            .metadata
            .set_branch_failed(branch_id, &format!("publish failed: {err}"))
            .await;
        return enqueue_failed().into_response();
    }
    info!(request_id = %principal.request_id, %database_id, %branch_id, name = %request.name, "accepted create branch");
    match state.metadata.get_branch(branch_id).await {
        Ok(Some(branch)) => (
            StatusCode::ACCEPTED,
            Json(branch_view(&state, branch).await),
        )
            .into_response(),
        Ok(None) => ApiError::internal().into_response(),
        Err(err) => {
            error!("load created branch: {err}");
            ApiError::internal().into_response()
        }
    }
}

/// The query of the connection reveal; other parameters are ignored.
#[derive(Debug, Deserialize)]
struct ConnectionQuery {
    role: Option<String>,
    database: Option<String>,
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/connection",
    operation_id = "getBranchConnection",
    tag = "branches",
    summary = "Reveal a branch's connection details",
    description = "The connection details of the branch's owner, or of the role named by `role`, for its main database or the one named by `database`. `password` is null, with the reason in `password_state`, when it can't be shown: a role whose password was set in SQL (only its hash is stored) or has an MD5 hash the proxy can't use; the `uri` then has no password. Never cached (`Cache-Control: no-store`) and never logged.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("role" = Option<String>, Query, description = "The role to connect as: any role of the branch. Absent, or the owner's name, gives the owner"),
        ("database" = Option<String>, Query, description = "The Postgres database to connect to, one of the branch's databases (the system `postgres` one included). Absent gives the main database"),
    ),
    responses(
        (status = 200, description = "The role's credentials; never cached", body = ConnectionDetails),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch, the requested role or the requested database doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready; `credentials_unavailable`: the branch has no endpoint", body = ErrorEnvelope),
        (status = 422, description = "`invalid_role_name`: `role` is empty, over 63 bytes or has a NUL", body = ErrorEnvelope),
        (status = 500, description = "`credentials_unavailable`: the stored credentials can't be opened with the current secrets key; `internal`: unexpected", body = ErrorEnvelope),
    ),
)]
async fn branch_connection<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
    query: Result<Query<ConnectionQuery>, QueryRejection>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let Query(ConnectionQuery { role, database }) = match query {
        Ok(query) => query,
        Err(rej) => {
            return ApiError::invalid_request(400, format!("invalid query: {}", rej.body_text()))
                .into_response();
        }
    };
    // Acting as the role (the owner by default) on that database.
    let target = Target::Role {
        database: id,
        branch: branch_id,
        pg_database: database.as_deref().unwrap_or(""),
        role: role.as_deref().map_or(Requested::Owner, Requested::Role),
    };
    let record = match authorize_database(&state, &principal, ep::BRANCH_CONNECTION.policy.access, target).await {
        Ok(record) => record,
        Err(resp) => return resp.into_response(),
    };
    let branch = match load_branch(&state, &record, branch_id).await {
        Ok(branch) => branch,
        Err(resp) => return resp.into_response(),
    };
    let mut response = reveal(
        &state,
        &principal,
        &record,
        Some(branch),
        role.as_deref(),
        database.as_deref(),
    )
    .await;
    if response.status().is_success() {
        response
            .headers_mut()
            .insert(CACHE_CONTROL, "no-store".parse().unwrap());
    }
    response
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/branches/{branch_id}/reset-password",
    operation_id = "resetBranchPassword",
    tag = "branches",
    summary = "Reset a branch's owner password",
    params(("id" = Uuid, Path, description = "Database id"), ("branch_id" = Uuid, Path, description = "Branch id")),
    responses(
        (status = 204, description = "The password was reset; read the new one from the connection details"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`branch_not_ready`: the branch isn't ready; `credentials_unavailable`: the owner credentials are missing; `backup_branch`: a restore backup has no compute", body = ErrorEnvelope),
        (status = 500, description = "`internal`: the worker failed to reset the password", body = ErrorEnvelope),
        (status = 503, description = "`not_applied`: the new password is stored but not applied yet and will be retried; `worker_unavailable`: the worker can't be reached", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the worker or database didn't answer in time", body = ErrorEnvelope),
    ),
)]
async fn branch_reset_password<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match authorize_database(&state, &principal, ep::BRANCH_RESET_PASSWORD.policy.access, Target::Branch { database: id, branch: branch_id }).await {
        Ok(record) => record,
        Err(resp) => return resp.into_response(),
    };
    let branch = match load_branch(&state, &record, branch_id).await {
        Ok(branch) => branch,
        Err(resp) => return resp.into_response(),
    };
    reset_branch_password(&state, &principal, &record, Some(branch)).await
}

fn names_of(children: &[BranchRecord]) -> String {
    children
        .iter()
        .map(|c| c.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Moves a ready branch to `resetting` unless it has live `normal` children, clearing its last
/// error so each attempt starts clean. The status change comes first: a create needs a ready
/// parent, so once it holds no child can slip in behind the check. `action` is the verb for
/// the refusal ("reset", "restore", "delete").
async fn claim<M, J>(
    state: &AppState<M, J>,
    branch: &BranchRecord,
    action: &str,
    allow_protected: bool,
) -> Result<(), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let moved = state
        .metadata
        .claim_branch_for_switch(branch.id, allow_protected)
        .await
        .map_err(|err| {
            error!("claim branch: {err}");
            ApiError::internal()
        })?;
    match moved {
        ClaimOutcome::Claimed => {}
        ClaimOutcome::Protected => return Err(protected_refusal()),
        // T14 M12: a merge that took the branch after the caller's pre-check.
        ClaimOutcome::MergeBusy(busy) => return Err(merges::merge_busy_refusal(busy)),
        ClaimOutcome::NotReady => {
            return Err(conflict(
                ErrorCode::BranchNotReady,
                "branch is not ready; try again in a moment",
            ));
        }
    }
    match state.metadata.live_children(branch.id).await {
        Ok(children) if children.is_empty() => Ok(()),
        Ok(children) => {
            release(state, branch.id).await;
            Err(conflict(
                ErrorCode::HasChildren,
                format!(
                    "cannot {action} {}: it has child branches ({}); delete them first",
                    branch.name,
                    names_of(&children)
                ),
            ))
        }
        Err(err) => {
            error!("live children: {err}");
            release(state, branch.id).await;
            Err(ApiError::internal())
        }
    }
}

/// The 409 for a delete or reset of a protected branch.
fn protected_refusal() -> ApiError {
    conflict(ErrorCode::BranchProtected, "this branch is protected; unprotect it first")
}

/// Puts a claimed branch back to ready.
async fn release<M, J>(state: &AppState<M, J>, id: BranchId)
where
    M: MetadataPort,
    J: JobPort,
{
    if let Err(err) = state
        .metadata
        .set_branch_status(id, &[BranchStatus::Resetting], BranchStatus::Ready)
        .await
    {
        warn!(branch_id = %id, "couldn't release a claimed branch: {err}");
    }
}

/// Refusals shared by reset-to-parent and restore, before anything changes.
fn check_rewritable(branch: &BranchRecord) -> Result<(), ApiError> {
    if branch.kind == BranchKind::Backup {
        return Err(conflict(ErrorCode::BackupBranch, BACKUP_REFUSAL));
    }
    if branch.status != BranchStatus::Ready {
        return Err(conflict(
            ErrorCode::BranchNotReady,
            format!("branch is not ready (it is {})", branch.status.as_str()),
        ));
    }
    Ok(())
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/branches/{branch_id}/reset-to-parent",
    operation_id = "resetBranchToParent",
    tag = "branches",
    summary = "Reset a branch to its parent",
    params(("id" = Uuid, Path, description = "Database id"), ("branch_id" = Uuid, Path, description = "Branch id")),
    responses(
        (status = 202, description = "Accepted; the branch moves to `resetting` and is rewritten in the background", body = OperationAccepted),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`main_branch`: main has no parent; `backup_branch`: a restore backup can't be rewritten; `branch_not_ready`: the branch isn't ready or is busy; `has_children`: it has child branches; `branch_protected`: the branch is protected, unprotect it first; `branch_merge_busy`: a merge holds the branch as its source or target, rehearsing, applying or failed half applied (`error.merge_id`: wait for it, or retry or abandon it), or a merge's rehearsal of the branch is still being removed (`error.rehearsal_branch_id`: try again in a few minutes)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`worker_unavailable`: the worker or job queue can't be reached", body = ErrorEnvelope),
    ),
)]
async fn reset_to_parent<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match authorize_database(&state, &principal, ep::BRANCH_RESET_TO_PARENT.policy.access, Target::Branch { database: id, branch: branch_id }).await {
        Ok(record) => record,
        Err(resp) => return resp.into_response(),
    };
    let branch = match load_branch(&state, &record, branch_id).await {
        Ok(branch) => branch,
        Err(resp) => return resp.into_response(),
    };
    if branch.kind == BranchKind::Normal && branch.parent_branch_id.is_none() {
        return conflict(
            ErrorCode::MainBranch,
            "main has no parent to reset to; use restore to rewind it",
        )
        .into_response();
    }
    if let Err(resp) = check_rewritable(&branch) {
        return resp.into_response();
    }
    if let Err(resp) = merges::refuse_if_merge_busy(&state, branch.id).await {
        return resp.into_response();
    }
    if let Err(resp) = claim(&state, &branch, "reset", false).await {
        return resp.into_response();
    }
    let job = JobEnvelope {
        job_id: Uuid::new_v4(),
        payload: BranchJob::ResetToParent {
            branch_id: branch.id,
        },
    };
    if let Err(err) = state.js.publish_branch(&job).await {
        error!(branch_id = %branch.id, "publish reset job failed: {err}");
        release(&state, branch.id).await;
        return enqueue_failed().into_response();
    }
    info!(request_id = %principal.request_id, branch_id = %branch.id, "reset to parent requested");
    (
        StatusCode::ACCEPTED,
        Json(OperationAccepted { status: "resetting" }),
    )
        .into_response()
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct RestoreRequest {
    /// The point in the branch's history to rewind to.
    #[schema(value_type = docs::PointInTime)]
    at: AtBody,
}

/// The 202 body of a restore: the branch is `resetting`, and its previous state is kept as a
/// backup branch.
#[derive(Debug, Serialize, ToSchema)]
struct RestoreAccepted {
    status: &'static str,
    /// The backup branch holding the state before the restore.
    backup_branch_id: BranchId,
    backup_name: String,
}

/// `<name>-before-restore-<yyyymmddThhmmssZ>`, with `<name>` cut so the whole fits in 63.
fn backup_name(name: &str, now: DateTime<Utc>) -> String {
    let suffix = format!("-before-restore-{}", now.format("%Y%m%dT%H%M%SZ"));
    let keep = MAX_NAME_LEN.saturating_sub(suffix.len());
    let base: String = name.chars().take(keep).collect();
    format!("{base}{suffix}")
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/branches/{branch_id}/restore",
    operation_id = "restoreBranch",
    tag = "branches",
    summary = "Restore a branch to a point in its history",
    params(("id" = Uuid, Path, description = "Database id"), ("branch_id" = Uuid, Path, description = "Branch id")),
    request_body = RestoreRequest,
    responses(
        (status = 202, description = "Accepted; the branch moves to `resetting`, and a backup of its current state is kept as a new branch", body = RestoreAccepted),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`backup_branch`: a restore backup can't be rewritten; `branch_not_ready`: the branch isn't ready or is busy; `has_children`: it has child branches; `branch_merge_busy`: a merge holds the branch as its source or target, rehearsing, applying or failed half applied (`error.merge_id`: wait for it, or retry or abandon it), or a merge's rehearsal of the branch is still being removed (`error.rehearsal_branch_id`: try again in a few minutes)", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the body is valid JSON but the wrong shape, the point in time is invalid, or it's before the branch's history begins (its branch point, or its `history_start`)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`worker_unavailable`: the worker or job queue can't be reached", body = ErrorEnvelope),
    ),
)]
async fn restore<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
    ApiBytes(body): ApiBytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match authorize_database(&state, &principal, ep::BRANCH_RESTORE.policy.access, Target::Branch { database: id, branch: branch_id }).await {
        Ok(record) => record,
        Err(resp) => return resp.into_response(),
    };
    let branch = match load_branch(&state, &record, branch_id).await {
        Ok(branch) => branch,
        Err(resp) => return resp.into_response(),
    };
    if let Err(resp) = check_rewritable(&branch) {
        return resp.into_response();
    }
    let request: RestoreRequest = match parse_body(&body) {
        Ok(request) => request,
        Err(resp) => return resp.into_response(),
    };
    let at = match request.at.parse() {
        Ok(at) => at,
        Err(message) => return unprocessable(&message).into_response(),
    };
    if let Err(message) = within_history(&branch, &at) {
        return unprocessable(&message).into_response();
    }
    if let Err(resp) = merges::refuse_if_merge_busy(&state, branch.id).await {
        return resp.into_response();
    }
    if let Err(resp) = claim(&state, &branch, "restore", true).await {
        return resp.into_response();
    }
    let backup_id = BranchId::new();
    let backup_name = backup_name(&branch.name, Utc::now());
    let job = JobEnvelope {
        job_id: Uuid::new_v4(),
        payload: BranchJob::Restore {
            branch_id: branch.id,
            backup_id,
            backup_name: backup_name.clone(),
            at,
        },
    };
    if let Err(err) = state.js.publish_branch(&job).await {
        error!(branch_id = %branch.id, "publish restore job failed: {err}");
        release(&state, branch.id).await;
        return enqueue_failed().into_response();
    }
    info!(request_id = %principal.request_id, branch_id = %branch.id, %backup_id, "restore requested");
    (
        StatusCode::ACCEPTED,
        Json(RestoreAccepted {
            status: "resetting",
            backup_branch_id: backup_id,
            backup_name,
        }),
    )
        .into_response()
}

#[utoipa::path(
    delete,
    path = "/v1/databases/{id}/branches/{branch_id}",
    operation_id = "deleteBranch",
    tag = "branches",
    summary = "Delete a branch",
    params(("id" = Uuid, Path, description = "Database id"), ("branch_id" = Uuid, Path, description = "Branch id")),
    responses(
        (status = 202, description = "Accepted; the branch moves to `deleting` and is removed in the background", body = OperationAccepted),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`main_branch`: main can't be deleted; `already_deleting`: it is already being deleted; `branch_not_ready`: it is busy; `has_children`: it or one of its backups has child branches; `branch_protected`: the branch is protected, unprotect it first; `branch_merge_busy`: a merge holds the branch as its source or target, rehearsing, applying or failed half applied (`error.merge_id`: wait for it, or retry or abandon it), or a merge's rehearsal of the branch is still being removed (`error.rehearsal_branch_id`: try again in a few minutes)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn delete_branch<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match authorize_database(&state, &principal, ep::DELETE_BRANCH.policy.access, Target::Branch { database: id, branch: branch_id }).await {
        Ok(record) => record,
        Err(resp) => return resp.into_response(),
    };
    let branch = match load_branch(&state, &record, branch_id).await {
        Ok(branch) => branch,
        Err(resp) => return resp.into_response(),
    };
    if branch.kind == BranchKind::Normal && branch.parent_branch_id.is_none() {
        return conflict(
            ErrorCode::MainBranch,
            "main can't be deleted; delete the database instead",
        )
        .into_response();
    }
    if branch.status == BranchStatus::Deleting {
        return conflict(ErrorCode::AlreadyDeleting, "branch is already being deleted")
            .into_response();
    }
    if let Err(resp) = merges::refuse_if_merge_busy(&state, branch.id).await {
        return resp.into_response();
    }
    let claimed = branch.status == BranchStatus::Ready;
    if claimed {
        // A ready branch can still gain children: hold it while checking.
        if let Err(resp) = claim(&state, &branch, "delete", false).await {
            return resp.into_response();
        }
    } else {
        // Not ready, so no create can pick it as a parent; existing children still block.
        // The claim above checks the flag for a ready branch; here it's read from the row.
        if branch.protected {
            return protected_refusal().into_response();
        }
        match state.metadata.live_children(branch.id).await {
            Ok(children) if children.is_empty() => {}
            Ok(children) => {
                return conflict(
                    ErrorCode::HasChildren,
                    format!(
                        "cannot delete {}: it has child branches ({}); delete them first",
                        branch.name,
                        names_of(&children)
                    ),
                )
                .into_response();
            }
            Err(err) => {
                error!("live children: {err}");
                return ApiError::internal().into_response();
            }
        }
    }
    match state.metadata.begin_branch_delete(branch.id).await {
        Ok(BranchDeleteOutcome::Started) => {}
        Ok(BranchDeleteOutcome::NotMoved) => {
            return conflict(ErrorCode::AlreadyDeleting, "branch is already being deleted")
            .into_response();
        }
        // The branch's own children were checked above; this is a child of one of its backups
        // (or a child created in between), found under the database row's lock.
        // Protected after the check above: the delete's own update refused it.
        Ok(BranchDeleteOutcome::Protected) => {
            if claimed {
                release(&state, branch.id).await;
            }
            return protected_refusal().into_response();
        }
        Ok(refused @ BranchDeleteOutcome::HasChildren { .. }) => {
            if claimed {
                release(&state, branch.id).await;
            }
            let message = refused.refusal(&branch.name).unwrap_or_default();
            return conflict(ErrorCode::HasChildren, message).into_response();
        }
        // T14 F3 (I1): a merge that took the branch after the pre-check, found under its row
        // lock (a branch that isn't ready has no claim to catch it).
        Ok(BranchDeleteOutcome::MergeBusy(busy)) => {
            if claimed {
                release(&state, branch.id).await;
            }
            return merges::merge_busy_refusal(busy).into_response();
        }
        Err(err) => {
            error!("begin branch delete: {err}");
            if claimed {
                release(&state, branch.id).await;
            }
            return ApiError::internal().into_response();
        }
    }
    merges::cancel_drafts(&state, branch.id).await;
    let job = JobEnvelope {
        job_id: Uuid::new_v4(),
        payload: BranchJob::Delete {
            branch_id: branch.id,
        },
    };
    // The row says deleting either way: reconcile finishes a delete whose job was lost.
    if let Err(err) = state.js.publish_branch(&job).await {
        error!(branch_id = %branch.id, "publish delete failed (reconcile will retry): {err}");
    }
    info!(request_id = %principal.request_id, branch_id = %branch.id, "delete branch requested");
    (
        StatusCode::ACCEPTED,
        Json(OperationAccepted { status: "deleting" }),
    )
        .into_response()
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateBranchRequest {
    /// `true` refuses deleting the branch, resetting it to its parent and deleting its
    /// database; restore still works.
    protected: bool,
}

#[utoipa::path(
    patch,
    path = "/v1/databases/{id}/branches/{branch_id}",
    operation_id = "updateBranch",
    tag = "branches",
    summary = "Protect or unprotect a branch",
    description = "Sets the branch's `protected` flag. A protected branch can't be deleted or reset to its parent, and its database can't be deleted, until it is unprotected. Restore still works. Only `normal` branches can be protected, main included.",
    params(("id" = Uuid, Path, description = "Database id"), ("branch_id" = Uuid, Path, description = "Branch id")),
    request_body = UpdateBranchRequest,
    responses(
        (status = 200, description = "The branch with its new flag", body = Branch),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`backup_branch`: a restore backup can't be protected; `branch_not_ready`: the branch is being deleted", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: an unknown or missing field, or `protected` isn't a boolean", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn update_branch<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(request): ApiJson<UpdateBranchRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match authorize_database(&state, &principal, ep::UPDATE_BRANCH.policy.access, Target::Branch { database: id, branch: branch_id }).await {
        Ok(record) => record,
        Err(resp) => return resp.into_response(),
    };
    let branch = match load_branch(&state, &record, branch_id).await {
        Ok(branch) => branch,
        Err(resp) => return resp.into_response(),
    };
    if branch.kind == BranchKind::Backup {
        return conflict(ErrorCode::BackupBranch, BACKUP_REFUSAL).into_response();
    }
    if branch.status == BranchStatus::Deleting {
        return conflict(ErrorCode::BranchNotReady, "branch is being deleted").into_response();
    }
    match state.metadata.set_branch_protected(branch.id, request.protected).await {
        Ok(Some(updated)) => {
            info!(
                request_id = %principal.request_id,
                branch_id = %branch.id,
                protected = request.protected,
                "branch protection changed"
            );
            // The DDL guard follows protection: the member-role reconciler locks (or unlocks)
            // the branch's non-Admin member roles and installs the guard.
            if let Err(err) = state.js.enqueue_member_roles(branch.id).await {
                warn!(branch_id = %branch.id, "member roles: enqueue failed; the sweep will reconcile it: {err}");
            }
            Json(branch_view(&state, updated).await).into_response()
        }
        // The branch started deleting between the load and the update.
        Ok(None) => conflict(ErrorCode::BranchNotReady, "branch is being deleted").into_response(),
        Err(err) => {
            error!("set branch protected: {err}");
            ApiError::internal().into_response()
        }
    }
}

/// A Postgres database of a branch.
#[derive(Debug, Serialize, ToSchema)]
pub(crate) struct BranchDatabaseView {
    name: String,
    /// The Postgres role that owns it.
    owner: String,
    origin: DatabaseOrigin,
    /// The branch's main database: always served by the data API.
    main: bool,
    /// Whether the data API serves it: `off`, `enabling` (the bootstrap is running or will be
    /// retried), `on` or `error` (see `data_api_error`).
    data_api: DataApiState,
    /// Why the last enable failed; null otherwise.
    #[schema(required = true)]
    data_api_error: Option<String>,
    /// The schema a request to this database uses when it names none.
    default_schema: String,
}

impl From<BranchDatabase> for BranchDatabaseView {
    fn from(d: BranchDatabase) -> Self {
        Self {
            main: d.origin == DatabaseOrigin::Main,
            name: d.name,
            owner: d.owner,
            origin: d.origin,
            data_api: d.data_api,
            data_api_error: d.data_api_error,
            default_schema: d.default_schema,
        }
    }
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/databases",
    operation_id = "listBranchDatabases",
    tag = "branches",
    summary = "List a branch's Postgres databases",
    description = "The databases of the branch's Postgres cluster: the main one first, then by name. The cluster's own `postgres` database is listed with origin `system`. Pass a name as `database` to the connection details. Each carries its data API state and default schema.",
    params(("id" = Uuid, Path, description = "Database id"), ("branch_id" = Uuid, Path, description = "Branch id")),
    responses(
        (status = 200, description = "The databases, main first", body = Vec<BranchDatabaseView>),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`backup_branch`: a restore backup has no compute", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_branch_databases<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match authorize_database(&state, &principal, ep::LIST_BRANCH_DATABASES.policy.access, Target::Branch { database: id, branch: branch_id }).await {
        Ok(record) => record,
        Err(resp) => return resp.into_response(),
    };
    let branch = match load_branch(&state, &record, branch_id).await {
        Ok(branch) => branch,
        Err(resp) => return resp.into_response(),
    };
    if branch.kind == BranchKind::Backup {
        return conflict(ErrorCode::BackupBranch, BACKUP_REFUSAL).into_response();
    }
    match state.metadata.branch_databases(branch.id).await {
        Ok(rows) => {
            Json(rows.into_iter().map(BranchDatabaseView::from).collect::<Vec<_>>()).into_response()
        }
        Err(err) => {
            error!("list branch databases: {err}");
            ApiError::internal().into_response()
        }
    }
}

/// All optional; a missing one is left as it is. `history_window` is in seconds.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateDatabaseRequest {
    history_window: Option<i64>,
    max_branches: Option<i64>,
    max_rows: Option<i64>,
    statement_timeout_ms: Option<i64>,
    max_connections: Option<i64>,
}

/// A database's settings after a change.
#[derive(Debug, Serialize, ToSchema)]
struct DatabaseSettings {
    /// Seconds of history the database keeps.
    history_window: u64,
    max_branches: i32,
    max_rows: i32,
    statement_timeout_ms: i32,
    max_connections: i32,
}

/// Bounds of the data API settings.
const MAX_ROWS: std::ops::RangeInclusive<i64> = 1..=100_000;
const STATEMENT_TIMEOUT_MS: std::ops::RangeInclusive<i64> = 100..=120_000;
const MAX_CONNECTIONS: std::ops::RangeInclusive<i64> = 1..=50;

/// `^[a-z_][a-z0-9_]{0,62}$`.
pub(crate) fn valid_schema_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    match bytes.split_first() {
        Some((first, rest)) => {
            rest.len() <= 62
                && (first.is_ascii_lowercase() || *first == b'_')
                && rest
                    .iter()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
        }
        None => false,
    }
}

fn in_range(
    name: &str,
    value: Option<i64>,
    range: &std::ops::RangeInclusive<i64>,
) -> Result<Option<i32>, ApiError> {
    match value {
        Some(n) if !range.contains(&n) => Err(unprocessable(&format!(
            "{name} must be between {} and {}",
            range.start(),
            range.end()
        ))),
        Some(n) => Ok(Some(n as i32)),
        None => Ok(None),
    }
}

#[utoipa::path(
    patch,
    path = "/v1/databases/{id}",
    operation_id = "updateDatabase",
    tag = "databases",
    summary = "Change a database's settings",
    description = "Every field is optional; a missing one stays as it is. `history_window` is in seconds.",
    params(("id" = Uuid, Path, description = "Database id")),
    request_body = UpdateDatabaseRequest,
    responses(
        (status = 200, description = "The settings after the change", body = DatabaseSettings),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database doesn't exist, or belongs to another tenant", body = ErrorEnvelope),
        (status = 409, description = "`database_not_ready`: the database is being deleted", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the body is valid JSON but the wrong shape, or a setting is out of range", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
pub(crate) async fn update_database<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
    ApiBytes(body): ApiBytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match authorize_database(&state, &principal, ep::UPDATE_DATABASE.policy.access, Target::Database(id)).await {
        Ok(record) => record,
        Err(resp) => return resp.into_response(),
    };
    let request: UpdateDatabaseRequest = match parse_body(&body) {
        Ok(request) => request,
        Err(resp) => return resp.into_response(),
    };
    let window = match request.history_window {
        Some(secs) if !(MIN_WINDOW_SECS..=MAX_WINDOW_SECS).contains(&secs) => {
            return unprocessable(&format!(
                "history_window must be between {MIN_WINDOW_SECS} (1 hour) and {MAX_WINDOW_SECS} \
                 (30 days) seconds"
            ))
            .into_response();
        }
        Some(secs) => Some(Duration::from_secs(secs as u64)),
        None => None,
    };
    let max = match in_range("max_branches", request.max_branches, &(0..=MAX_BRANCHES_LIMIT)) {
        Ok(max) => max,
        Err(resp) => return resp.into_response(),
    };
    let max_rows = match in_range("max_rows", request.max_rows, &MAX_ROWS) {
        Ok(v) => v,
        Err(resp) => return resp.into_response(),
    };
    let statement_timeout_ms = match in_range(
        "statement_timeout_ms",
        request.statement_timeout_ms,
        &STATEMENT_TIMEOUT_MS,
    ) {
        Ok(v) => v,
        Err(resp) => return resp.into_response(),
    };
    let max_connections = match in_range("max_connections", request.max_connections, &MAX_CONNECTIONS) {
        Ok(v) => v,
        Err(resp) => return resp.into_response(),
    };
    let patch = SettingsPatch {
        history_window: window,
        max_branches: max,
        max_rows,
        statement_timeout_ms,
        max_connections,
    };
    match state
        .metadata
        .update_database_settings(record.database_id, &patch)
        .await
    {
        Ok(true) => {}
        Ok(false) => {
            return conflict(ErrorCode::DatabaseNotReady, "database is deleting").into_response();
        }
        Err(err) => {
            error!("update settings: {err}");
            return ApiError::internal().into_response();
        }
    }
    if window.is_some_and(|w| w != record.history_window) {
        let job = JobEnvelope {
            job_id: Uuid::new_v4(),
            payload: DatabaseSettingsJob {
                database_id: record.database_id,
            },
        };
        // Reconcile compares each tenant's window with the setting, so a lost job heals.
        if let Err(err) = state.js.publish_database_settings(&job).await {
            warn!(database_id = %record.database_id, "publish settings job failed (reconcile will retry): {err}");
        }
    }
    info!(request_id = %principal.request_id, database_id = %record.database_id, "database settings updated");
    Json(DatabaseSettings {
        history_window: window.unwrap_or(record.history_window).as_secs(),
        max_branches: max.unwrap_or(record.max_branches),
        max_rows: max_rows.unwrap_or(record.max_rows),
        statement_timeout_ms: statement_timeout_ms.unwrap_or(record.statement_timeout_ms),
        max_connections: max_connections.unwrap_or(record.max_connections),
    })
    .into_response()
}
