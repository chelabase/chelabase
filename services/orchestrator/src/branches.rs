//! Branch routes: list, create, reveal, reset password, reset to parent, restore, delete, and
//! the database settings `PATCH`. The worker does the Neon work; these handlers check every
//! refusal, move the row to its working status and publish the job.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::{StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use cheladb_metadata::{
    BranchDeleteOutcome, BranchKind, BranchRecord, BranchStatus, CreateBranchOutcome,
    DatabaseRecord, NewBranch, SettingsPatch,
};
use cheladb_nats::{BranchJob, DatabaseSettingsJob, JobEnvelope, PointInTime};
use cheladb_types::api_error::{ApiError, ErrorCode};
use cheladb_types::{ApiPath, BranchId};
use chrono::{DateTime, Utc};
use cheladb_types::api_error::ErrorEnvelope;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    AppState, ApiBytes, AuthPrincipal, BACKUP_REFUSAL, ConnectionDetails, JobPort, MetadataPort, OperationAccepted,
    PasswordResetResult, load_owned, proxy_address, reset_branch_password, reveal,
};

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
        .routes(routes!(delete_branch))
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
}

impl Branch {
    fn new(b: BranchRecord, proxy_domain: &str) -> Self {
        let endpoint = (b.kind == BranchKind::Normal)
            .then_some(b.endpoint_id.as_deref())
            .flatten();
        let (host, port) = proxy_address(
            if b.status == BranchStatus::Ready {
                cheladb_metadata::DatabaseStatus::Ready
            } else {
                cheladb_metadata::DatabaseStatus::Pending
            },
            endpoint,
            proxy_domain,
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
        }
    }
}

/// Parses a JSON body; an empty, malformed or wrongly typed one is a 422 with the reason.
pub(crate) fn parse_body<T: DeserializeOwned>(body: &Bytes) -> Result<T, ApiError> {
    serde_json::from_slice(body)
        .map_err(|err| unprocessable(&format!("invalid request body: {err}")))
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
        Ok(Some(b))
            if b.database_id == database.database_id && b.status != BranchStatus::Deleted =>
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
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Cheladb tenant", body = ErrorEnvelope),
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
    let record = match load_owned(&state, &principal, id).await {
        Ok(record) => record,
        Err(resp) => return resp.into_response(),
    };
    match state.metadata.list_branches(record.database_id).await {
        Ok(branches) => Json(
            branches
                .into_iter()
                .map(|b| Branch::new(b, &state.proxy_domain))
                .collect::<Vec<_>>(),
        )
        .into_response(),
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
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body couldn't be read", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Cheladb tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the parent branch doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`name_taken`: a branch with this name exists; `branch_cap_reached`: the database has `max_branches`; `branch_not_ready`: the parent isn't ready; `database_not_ready`: the database is being deleted", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the body isn't valid JSON of the right shape, the name is invalid, or the point in time is invalid", body = ErrorEnvelope),
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
    let record = match load_owned(&state, &principal, id).await {
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
            Json(Branch::new(branch, &state.proxy_domain)),
        )
            .into_response(),
        Ok(None) => ApiError::internal().into_response(),
        Err(err) => {
            error!("load created branch: {err}");
            ApiError::internal().into_response()
        }
    }
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/connection",
    operation_id = "getBranchConnection",
    tag = "branches",
    summary = "Reveal a branch's connection details",
    description = "The owner credentials of the branch. Never cached (`Cache-Control: no-store`) and never logged.",
    params(("id" = Uuid, Path, description = "Database id"), ("branch_id" = Uuid, Path, description = "Branch id")),
    responses(
        (status = 200, description = "The owner credentials; never cached", body = ConnectionDetails),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Cheladb tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready; `credentials_unavailable`: the owner credentials are missing", body = ErrorEnvelope),
        (status = 500, description = "`credentials_unavailable`: the stored credentials can't be opened with the current secrets key; `internal`: unexpected", body = ErrorEnvelope),
    ),
)]
async fn branch_connection<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match load_owned(&state, &principal, id).await {
        Ok(record) => record,
        Err(resp) => return resp.into_response(),
    };
    let branch = match load_branch(&state, &record, branch_id).await {
        Ok(branch) => branch,
        Err(resp) => return resp.into_response(),
    };
    let mut response = reveal(&state, &principal, &record, Some(branch)).await;
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
        (status = 200, description = "The password was reset; read the new one from the connection details", body = PasswordResetResult),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Cheladb tenant", body = ErrorEnvelope),
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
    let record = match load_owned(&state, &principal, id).await {
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
) -> Result<(), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let moved = state
        .metadata
        .claim_branch_for_switch(branch.id)
        .await
        .map_err(|err| {
            error!("claim branch: {err}");
            ApiError::internal()
        })?;
    if !moved {
        return Err(conflict(
            ErrorCode::BranchNotReady,
            "branch is not ready; try again in a moment",
        ));
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
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Cheladb tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`main_branch`: main has no parent; `backup_branch`: a restore backup can't be rewritten; `branch_not_ready`: the branch isn't ready or is busy; `has_children`: it has child branches", body = ErrorEnvelope),
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
    let record = match load_owned(&state, &principal, id).await {
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
    if let Err(resp) = claim(&state, &branch, "reset").await {
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
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body couldn't be read", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Cheladb tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`backup_branch`: a restore backup can't be rewritten; `branch_not_ready`: the branch isn't ready or is busy; `has_children`: it has child branches", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the body isn't valid JSON of the right shape, or the point in time is invalid", body = ErrorEnvelope),
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
    let record = match load_owned(&state, &principal, id).await {
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
    if let Err(resp) = claim(&state, &branch, "restore").await {
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
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Cheladb tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`main_branch`: main can't be deleted; `already_deleting`: it is already being deleted; `branch_not_ready`: it is busy; `has_children`: it or one of its backups has child branches", body = ErrorEnvelope),
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
    let record = match load_owned(&state, &principal, id).await {
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
    let claimed = branch.status == BranchStatus::Ready;
    if claimed {
        // A ready branch can still gain children: hold it while checking.
        if let Err(resp) = claim(&state, &branch, "delete").await {
            return resp.into_response();
        }
    } else {
        // Not ready, so no create can pick it as a parent; existing children still block.
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
        Ok(refused @ BranchDeleteOutcome::HasChildren { .. }) => {
            if claimed {
                release(&state, branch.id).await;
            }
            let message = refused.refusal(&branch.name).unwrap_or_default();
            return conflict(ErrorCode::HasChildren, message).into_response();
        }
        Err(err) => {
            error!("begin branch delete: {err}");
            if claimed {
                release(&state, branch.id).await;
            }
            return ApiError::internal().into_response();
        }
    }
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

/// All optional; a missing one is left as it is. `history_window` is in seconds.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateDatabaseRequest {
    history_window: Option<i64>,
    max_branches: Option<i64>,
    max_rows: Option<i64>,
    statement_timeout_ms: Option<i64>,
    max_connections: Option<i64>,
    default_schema: Option<String>,
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
    default_schema: String,
}

/// Bounds of the data API settings.
const MAX_ROWS: std::ops::RangeInclusive<i64> = 1..=100_000;
const STATEMENT_TIMEOUT_MS: std::ops::RangeInclusive<i64> = 100..=120_000;
const MAX_CONNECTIONS: std::ops::RangeInclusive<i64> = 1..=50;

/// `^[a-z_][a-z0-9_]{0,62}$`.
fn valid_schema_name(name: &str) -> bool {
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
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body couldn't be read", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Cheladb tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database doesn't exist, or belongs to another tenant", body = ErrorEnvelope),
        (status = 409, description = "`database_not_ready`: the database is being deleted", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the body isn't valid JSON of the right shape, or a setting is out of range", body = ErrorEnvelope),
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
    let record = match load_owned(&state, &principal, id).await {
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
    if let Some(schema) = &request.default_schema
        && !valid_schema_name(schema)
    {
        return unprocessable(
            "default_schema must be 1-63 characters from a-z, 0-9 and _, not starting with a digit",
        )
        .into_response();
    }
    let patch = SettingsPatch {
        history_window: window,
        max_branches: max,
        max_rows,
        statement_timeout_ms,
        max_connections,
        default_schema: request.default_schema.clone(),
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
        default_schema: request.default_schema.unwrap_or(record.default_schema),
    })
    .into_response()
}
