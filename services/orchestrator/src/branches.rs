//! Branch routes: list, create, reveal, reset password, reset to parent, restore, delete, and
//! the database settings `PATCH`. The worker does the Neon work; these handlers check every
//! refusal, move the row to its working status and publish the job.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{StatusCode, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use cheladb_metadata::{
    BranchDeleteOutcome, BranchKind, BranchRecord, BranchStatus, CreateBranchOutcome,
    DatabaseRecord, NewBranch, SettingsPatch,
};
use cheladb_nats::{BranchJob, DatabaseSettingsJob, JobEnvelope, PointInTime};
use cheladb_routes::orchestrator as routes;
use cheladb_types::BranchId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    AppState, AuthPrincipal, BACKUP_REFUSAL, JobPort, MetadataPort, internal_error, json_error,
    load_owned, proxy_address, reset_branch_password, reveal,
};

/// Longest branch name, and what a backup's name must fit in.
const MAX_NAME_LEN: usize = 63;
/// Bounds of `history_window`, in seconds: one hour to 30 days.
const MIN_WINDOW_SECS: i64 = 3600;
const MAX_WINDOW_SECS: i64 = 30 * 24 * 3600;
/// Bounds of `max_branches`.
const MAX_BRANCHES_LIMIT: i64 = 20;

pub(crate) fn routes<M, J>() -> Router<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    Router::new()
        .route(
            routes::LIST_BRANCHES.path,
            get(list_branches::<M, J>).post(create_branch::<M, J>),
        )
        .route(
            routes::DELETE_BRANCH.path,
            axum::routing::delete(delete_branch::<M, J>),
        )
        .route(
            routes::BRANCH_CONNECTION.path,
            get(branch_connection::<M, J>),
        )
        .route(
            routes::BRANCH_RESET_PASSWORD.path,
            post(branch_reset_password::<M, J>),
        )
        .route(
            routes::BRANCH_RESET_TO_PARENT.path,
            post(reset_to_parent::<M, J>),
        )
        .route(routes::BRANCH_RESTORE.path, post(restore::<M, J>))
}

/// A branch as the API shows it. `host` and `port` only for a ready `normal` branch with an
/// endpoint.
#[derive(Debug, Serialize)]
struct BranchSummary {
    id: BranchId,
    name: String,
    parent_branch_id: Option<BranchId>,
    kind: BranchKind,
    status: BranchStatus,
    host: Option<String>,
    port: Option<u16>,
    branched_at_time: Option<DateTime<Utc>>,
    branched_at_lsn: Option<String>,
    created_at: DateTime<Utc>,
    /// Where the branch's history begins (null when unknown); Studio's lower bound for a point.
    history_start: Option<DateTime<Utc>>,
    error: Option<String>,
}

impl BranchSummary {
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
pub(crate) fn parse_body<T: DeserializeOwned>(body: &Bytes) -> Result<T, Box<Response>> {
    serde_json::from_slice(body).map_err(|err| {
        Box::new(json_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            &format!("invalid request body: {err}"),
        ))
    })
}

pub(crate) fn unprocessable(message: &str) -> Box<Response> {
    Box::new(json_error(StatusCode::UNPROCESSABLE_ENTITY, message))
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
) -> Result<BranchRecord, Box<Response>>
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
        Ok(_) => Err(Box::new(json_error(
            StatusCode::NOT_FOUND,
            "branch not found",
        ))),
        Err(err) => {
            error!("load branch: {err}");
            Err(Box::new(internal_error()))
        }
    }
}

async fn list_branches<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    Path(id): Path<Uuid>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match load_owned(&state, &principal, id).await {
        Ok(record) => record,
        Err(resp) => return *resp,
    };
    match state.metadata.list_branches(record.database_id).await {
        Ok(branches) => Json(
            branches
                .into_iter()
                .map(|b| BranchSummary::new(b, &state.proxy_domain))
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(err) => {
            error!("list branches: {err}");
            internal_error()
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBranchRequest {
    name: String,
    parent_branch_id: Option<Uuid>,
    at: Option<AtBody>,
}

async fn create_branch<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    Path(id): Path<Uuid>,
    body: Bytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match load_owned(&state, &principal, id).await {
        Ok(record) => record,
        Err(resp) => return *resp,
    };
    let request: CreateBranchRequest = match parse_body(&body) {
        Ok(request) => request,
        Err(resp) => return *resp,
    };
    if let Err(message) = validate_name(&request.name) {
        return *unprocessable(&message);
    }
    let at = match request.at.map(AtBody::parse).transpose() {
        Ok(at) => at,
        Err(message) => return *unprocessable(&message),
    };
    let database_id = record.database_id;
    let parent = request
        .parent_branch_id
        .map_or(BranchId::from(database_id), BranchId);
    match load_branch(&state, &record, parent.as_uuid()).await {
        Ok(_) => {}
        Err(_) => return json_error(StatusCode::NOT_FOUND, "parent branch not found"),
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
            return json_error(
                StatusCode::CONFLICT,
                &format!("a branch named {} already exists", request.name),
            );
        }
        Ok(CreateBranchOutcome::CapReached { max }) => {
            return json_error(
                StatusCode::CONFLICT,
                &format!(
                    "this database already has {max} branches besides main; raise max_branches \
                     with PATCH /v1/databases/{database_id}"
                ),
            );
        }
        Ok(CreateBranchOutcome::ParentNotReady) => {
            return json_error(StatusCode::CONFLICT, "the parent branch is not ready");
        }
        Ok(CreateBranchOutcome::DatabaseNotLive) => {
            return json_error(StatusCode::CONFLICT, "database is being deleted");
        }
        Err(err) => {
            error!("create branch: {err}");
            return internal_error();
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
        return json_error(StatusCode::BAD_GATEWAY, "failed to enqueue branch job");
    }
    info!(request_id = %principal.request_id, %database_id, %branch_id, name = %request.name, "accepted create branch");
    match state.metadata.get_branch(branch_id).await {
        Ok(Some(branch)) => (
            StatusCode::ACCEPTED,
            Json(BranchSummary::new(branch, &state.proxy_domain)),
        )
            .into_response(),
        Ok(None) => internal_error(),
        Err(err) => {
            error!("load created branch: {err}");
            internal_error()
        }
    }
}

async fn branch_connection<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    Path((id, branch_id)): Path<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match load_owned(&state, &principal, id).await {
        Ok(record) => record,
        Err(resp) => return *resp,
    };
    let branch = match load_branch(&state, &record, branch_id).await {
        Ok(branch) => branch,
        Err(resp) => return *resp,
    };
    let mut response = reveal(&state, &principal, &record, Some(branch)).await;
    if response.status().is_success() {
        response
            .headers_mut()
            .insert(CACHE_CONTROL, "no-store".parse().unwrap());
    }
    response
}

async fn branch_reset_password<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    Path((id, branch_id)): Path<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match load_owned(&state, &principal, id).await {
        Ok(record) => record,
        Err(resp) => return *resp,
    };
    let branch = match load_branch(&state, &record, branch_id).await {
        Ok(branch) => branch,
        Err(resp) => return *resp,
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
) -> Result<(), Box<Response>>
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
            Box::new(internal_error())
        })?;
    if !moved {
        return Err(Box::new(json_error(
            StatusCode::CONFLICT,
            "branch is not ready; try again in a moment",
        )));
    }
    match state.metadata.live_children(branch.id).await {
        Ok(children) if children.is_empty() => Ok(()),
        Ok(children) => {
            release(state, branch.id).await;
            Err(Box::new(json_error(
                StatusCode::CONFLICT,
                &format!(
                    "cannot {action} {}: it has child branches ({}); delete them first",
                    branch.name,
                    names_of(&children)
                ),
            )))
        }
        Err(err) => {
            error!("live children: {err}");
            release(state, branch.id).await;
            Err(Box::new(internal_error()))
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
fn check_rewritable(branch: &BranchRecord) -> Result<(), Box<Response>> {
    if branch.kind == BranchKind::Backup {
        return Err(Box::new(json_error(StatusCode::CONFLICT, BACKUP_REFUSAL)));
    }
    if branch.status != BranchStatus::Ready {
        return Err(Box::new(json_error(
            StatusCode::CONFLICT,
            &format!("branch is not ready (it is {})", branch.status.as_str()),
        )));
    }
    Ok(())
}

async fn reset_to_parent<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    Path((id, branch_id)): Path<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match load_owned(&state, &principal, id).await {
        Ok(record) => record,
        Err(resp) => return *resp,
    };
    let branch = match load_branch(&state, &record, branch_id).await {
        Ok(branch) => branch,
        Err(resp) => return *resp,
    };
    if branch.kind == BranchKind::Normal && branch.parent_branch_id.is_none() {
        return json_error(
            StatusCode::CONFLICT,
            "main has no parent to reset to; use restore to rewind it",
        );
    }
    if let Err(resp) = check_rewritable(&branch) {
        return *resp;
    }
    if let Err(resp) = claim(&state, &branch, "reset").await {
        return *resp;
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
        return json_error(StatusCode::BAD_GATEWAY, "failed to enqueue branch job");
    }
    info!(request_id = %principal.request_id, branch_id = %branch.id, "reset to parent requested");
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "status": "resetting" })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RestoreRequest {
    at: AtBody,
}

/// `<name>-before-restore-<yyyymmddThhmmssZ>`, with `<name>` cut so the whole fits in 63.
fn backup_name(name: &str, now: DateTime<Utc>) -> String {
    let suffix = format!("-before-restore-{}", now.format("%Y%m%dT%H%M%SZ"));
    let keep = MAX_NAME_LEN.saturating_sub(suffix.len());
    let base: String = name.chars().take(keep).collect();
    format!("{base}{suffix}")
}

async fn restore<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    Path((id, branch_id)): Path<(Uuid, Uuid)>,
    body: Bytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match load_owned(&state, &principal, id).await {
        Ok(record) => record,
        Err(resp) => return *resp,
    };
    let branch = match load_branch(&state, &record, branch_id).await {
        Ok(branch) => branch,
        Err(resp) => return *resp,
    };
    if let Err(resp) = check_rewritable(&branch) {
        return *resp;
    }
    let request: RestoreRequest = match parse_body(&body) {
        Ok(request) => request,
        Err(resp) => return *resp,
    };
    let at = match request.at.parse() {
        Ok(at) => at,
        Err(message) => return *unprocessable(&message),
    };
    if let Err(resp) = claim(&state, &branch, "restore").await {
        return *resp;
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
        return json_error(StatusCode::BAD_GATEWAY, "failed to enqueue branch job");
    }
    info!(request_id = %principal.request_id, branch_id = %branch.id, %backup_id, "restore requested");
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "status": "resetting",
            "backup_branch_id": backup_id,
            "backup_name": backup_name,
        })),
    )
        .into_response()
}

async fn delete_branch<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    Path((id, branch_id)): Path<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match load_owned(&state, &principal, id).await {
        Ok(record) => record,
        Err(resp) => return *resp,
    };
    let branch = match load_branch(&state, &record, branch_id).await {
        Ok(branch) => branch,
        Err(resp) => return *resp,
    };
    if branch.kind == BranchKind::Normal && branch.parent_branch_id.is_none() {
        return json_error(
            StatusCode::CONFLICT,
            "main can't be deleted; delete the database instead",
        );
    }
    if branch.status == BranchStatus::Deleting {
        return json_error(StatusCode::CONFLICT, "branch is already being deleted");
    }
    let claimed = branch.status == BranchStatus::Ready;
    if claimed {
        // A ready branch can still gain children: hold it while checking.
        if let Err(resp) = claim(&state, &branch, "delete").await {
            return *resp;
        }
    } else {
        // Not ready, so no create can pick it as a parent; existing children still block.
        match state.metadata.live_children(branch.id).await {
            Ok(children) if children.is_empty() => {}
            Ok(children) => {
                return json_error(
                    StatusCode::CONFLICT,
                    &format!(
                        "cannot delete {}: it has child branches ({}); delete them first",
                        branch.name,
                        names_of(&children)
                    ),
                );
            }
            Err(err) => {
                error!("live children: {err}");
                return internal_error();
            }
        }
    }
    match state.metadata.begin_branch_delete(branch.id).await {
        Ok(BranchDeleteOutcome::Started) => {}
        Ok(BranchDeleteOutcome::NotMoved) => {
            return json_error(StatusCode::CONFLICT, "branch is already being deleted");
        }
        // The branch's own children were checked above; this is a child of one of its backups
        // (or a child created in between), found under the database row's lock.
        Ok(refused @ BranchDeleteOutcome::HasChildren { .. }) => {
            if claimed {
                release(&state, branch.id).await;
            }
            let message = refused.refusal(&branch.name).unwrap_or_default();
            return json_error(StatusCode::CONFLICT, &message);
        }
        Err(err) => {
            error!("begin branch delete: {err}");
            if claimed {
                release(&state, branch.id).await;
            }
            return internal_error();
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
        Json(serde_json::json!({ "status": "deleting" })),
    )
        .into_response()
}

/// All optional; a missing one is left as it is. `history_window` is in seconds.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateDatabaseRequest {
    history_window: Option<i64>,
    max_branches: Option<i64>,
    max_rows: Option<i64>,
    statement_timeout_ms: Option<i64>,
    max_connections: Option<i64>,
    default_schema: Option<String>,
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
) -> Result<Option<i32>, Box<Response>> {
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

pub(crate) async fn update_database<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    Path(id): Path<Uuid>,
    body: Bytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match load_owned(&state, &principal, id).await {
        Ok(record) => record,
        Err(resp) => return *resp,
    };
    let request: UpdateDatabaseRequest = match parse_body(&body) {
        Ok(request) => request,
        Err(resp) => return *resp,
    };
    let window = match request.history_window {
        Some(secs) if !(MIN_WINDOW_SECS..=MAX_WINDOW_SECS).contains(&secs) => {
            return *unprocessable(&format!(
                "history_window must be between {MIN_WINDOW_SECS} (1 hour) and {MAX_WINDOW_SECS} \
                 (30 days) seconds"
            ));
        }
        Some(secs) => Some(Duration::from_secs(secs as u64)),
        None => None,
    };
    let max = match in_range("max_branches", request.max_branches, &(0..=MAX_BRANCHES_LIMIT)) {
        Ok(max) => max,
        Err(resp) => return *resp,
    };
    let max_rows = match in_range("max_rows", request.max_rows, &MAX_ROWS) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let statement_timeout_ms = match in_range(
        "statement_timeout_ms",
        request.statement_timeout_ms,
        &STATEMENT_TIMEOUT_MS,
    ) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    let max_connections = match in_range("max_connections", request.max_connections, &MAX_CONNECTIONS) {
        Ok(v) => v,
        Err(resp) => return *resp,
    };
    if let Some(schema) = &request.default_schema
        && !valid_schema_name(schema)
    {
        return *unprocessable(
            "default_schema must be 1-63 characters from a-z, 0-9 and _, not starting with a digit",
        );
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
        Ok(false) => return json_error(StatusCode::CONFLICT, "database is deleting"),
        Err(err) => {
            error!("update settings: {err}");
            return internal_error();
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
    Json(serde_json::json!({
        "history_window": window.unwrap_or(record.history_window).as_secs(),
        "max_branches": max.unwrap_or(record.max_branches),
        "max_rows": max_rows.unwrap_or(record.max_rows),
        "statement_timeout_ms": statement_timeout_ms.unwrap_or(record.statement_timeout_ms),
        "max_connections": max_connections.unwrap_or(record.max_connections),
        "default_schema": request.default_schema.unwrap_or(record.default_schema),
    }))
    .into_response()
}
