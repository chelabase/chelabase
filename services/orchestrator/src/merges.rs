//! Schema merge routes (step 6g): create a merge between a branch and its parent, edit its
//! resolutions, steps and acknowledgements, rehearse, apply, recompute, retry, cancel and
//! export the plan. The worker does the work (`MergeJob`); these handlers check every refusal,
//! move the row to its working status with one guarded write and publish the job.
//!
//! The analysis, plan, rehearsal and result are the engine's JSON (`chelabase-schema`) and are
//! stored and served as written; the API reads only what it must to refuse an apply (the
//! analysis' conflict ids, the plan's flagged steps).

use std::collections::HashSet;
use std::sync::Arc;

use axum::{
    Json,
    extract::{Query, State, rejection::QueryRejection},
    http::{
        HeaderValue, StatusCode,
        header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
};
use chelabase_metadata::{
    BranchBusy, BranchKind, BranchRecord, BranchStatus, DatabaseOrigin, DatabaseRecord, MergeCreate,
    MergeAction, MergeDirection, MergeDraftPatch, MergeEvent, MergeRecord, MergeStatus, MergeStatusPatch,
    MergeWrite, MetadataError, NewMerge,
};
use chelabase_nats::{JobEnvelope, MergeAfter, MergeJob};
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use chelabase_types::{ApiPath, BranchId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::{error, info, warn};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::access::{authorize, authorize_database, Authorized, Target};
use crate::branches::{conflict, load_branch, parse_body, unprocessable};
use crate::{ApiBytes, AppState, AuthPrincipal, BACKUP_REFUSAL, JobPort, MetadataPort};
use chelabase_routes::orchestrator as ep;
use chelabase_types::access::{Access, PlatformRole};

/// The access check of a route on merge `merge_id` of database `id`: `Branch` routes go by the
/// branch the merge goes into. An unknown merge, or another database's, counts as protected
/// (`load_merge` answers 404 after the check).
async fn authorize_merge<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    access: Access,
    id: Uuid,
    merge_id: Uuid,
) -> Result<DatabaseRecord, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    authorize_merge_full(state, principal, access, id, merge_id).await?.into_record()
}

/// [`authorize_merge`], also returning what the check resolved (the caller's grant).
async fn authorize_merge_full<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    access: Access,
    id: Uuid,
    merge_id: Uuid,
) -> Result<Authorized, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let merged_into = match state.metadata.get_merge(merge_id).await {
        Ok(Some(merge)) if merge.database_id == id => merge.target_branch_id,
        Ok(_) => Uuid::nil(),
        Err(err) => return Err(internal("load merge", err)),
    };
    authorize(state, principal, access, Target::Branch { database: id, branch: merged_into }).await
}

pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new()
        .routes(routes!(create_merge))
        .routes(routes!(list_merges))
        .routes(routes!(get_merge, update_merge))
        .routes(routes!(rehearse_merge))
        .routes(routes!(apply_merge))
        .routes(routes!(recompute_merge))
        .routes(routes!(retry_merge))
        .routes(routes!(cancel_merge))
        .routes(routes!(abandon_merge))
        .routes(routes!(finish_merge))
        .routes(routes!(export_merge_plan))
}

/// The unique indexes of `merges` a write can run into.
const OPEN_INDEX: &str = "merges_one_open";
const APPLYING_INDEX: &str = "merges_one_applying";

/// Bounds of what a draft save may carry.
const MAX_RESOLUTIONS: usize = 2_000;
const MAX_STEPS: usize = 200;
const MAX_ACKNOWLEDGEMENTS: usize = 10_000;
const MAX_TEXT_BYTES: usize = 100_000;
const MAX_ID_BYTES: usize = 256;
/// Bounds of `options.lock_timeout_ms`.
const LOCK_TIMEOUT_MS: std::ops::RangeInclusive<u32> = 100..=600_000;

// ---- views ------------------------------------------------------------------------------

/// Which way a merge goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum Direction {
    /// The branch's changes go into its parent.
    IntoParent,
    /// The parent's changes come into the branch.
    FromParent,
}

impl From<MergeDirection> for Direction {
    fn from(d: MergeDirection) -> Self {
        match d {
            MergeDirection::IntoParent => Self::IntoParent,
            MergeDirection::FromParent => Self::FromParent,
        }
    }
}

impl From<Direction> for MergeDirection {
    fn from(d: Direction) -> Self {
        match d {
            Direction::IntoParent => Self::IntoParent,
            Direction::FromParent => Self::FromParent,
        }
    }
}

/// Where a merge is: `computing` (the worker reads the schemas), `draft` (editable),
/// `rehearsing`, `rehearsed`, `applying` / `online` / `finishing` (changing the target), and
/// the end states.
#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum MergeState {
    Computing,
    Draft,
    Rehearsing,
    Rehearsed,
    Applying,
    Online,
    Finishing,
    Done,
    AppliedWithDrift,
    Failed,
    Stale,
    Cancelled,
}

impl From<MergeStatus> for MergeState {
    fn from(s: MergeStatus) -> Self {
        match s {
            MergeStatus::Computing => Self::Computing,
            MergeStatus::Draft => Self::Draft,
            MergeStatus::Rehearsing => Self::Rehearsing,
            MergeStatus::Rehearsed => Self::Rehearsed,
            MergeStatus::Applying => Self::Applying,
            MergeStatus::Online => Self::Online,
            MergeStatus::Finishing => Self::Finishing,
            MergeStatus::Done => Self::Done,
            MergeStatus::AppliedWithDrift => Self::AppliedWithDrift,
            MergeStatus::Failed => Self::Failed,
            MergeStatus::Stale => Self::Stale,
            MergeStatus::Cancelled => Self::Cancelled,
        }
    }
}

/// A merge without its analysis and plan: what lists and the answers to actions carry.
#[derive(Debug, Serialize, ToSchema)]
struct MergeSummary {
    id: Uuid,
    source_branch_id: BranchId,
    target_branch_id: BranchId,
    direction: Direction,
    /// The Postgres database being merged.
    database: String,
    status: MergeState,
    /// The phase the merge failed in (`online` can be retried); null otherwise.
    #[schema(required = true)]
    failed_phase: Option<String>,
    /// Changes with every draft save and every recompute: send it back with an edit.
    version: i64,
    /// The base was missing and the target's schema was used instead.
    rebaselined: bool,
    #[schema(required = true)]
    error: Option<String>,
    created_by: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl From<&MergeRecord> for MergeSummary {
    fn from(r: &MergeRecord) -> Self {
        Self {
            id: r.id,
            source_branch_id: BranchId(r.source_branch_id),
            target_branch_id: BranchId(r.target_branch_id),
            direction: r.direction.into(),
            database: r.database_name.clone(),
            status: r.status.into(),
            failed_phase: r.failed_phase.clone(),
            version: r.version,
            rebaselined: r.rebaselined,
            error: r.error.clone(),
            created_by: r.created_by.clone(),
            created_at: r.created_at,
            updated_at: r.updated_at,
        }
    }
}

/// A merge with everything the worker computed and the user chose. `analysis`, `plan`,
/// `rehearsal` and `result` are the schema engine's documents, served as stored.
#[derive(Debug, Serialize, ToSchema)]
struct Merge {
    id: Uuid,
    source_branch_id: BranchId,
    target_branch_id: BranchId,
    direction: Direction,
    database: String,
    status: MergeState,
    #[schema(required = true)]
    failed_phase: Option<String>,
    version: i64,
    rebaselined: bool,
    #[schema(required = true)]
    error: Option<String>,
    created_by: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    /// SHA-256 of each normalized schema the analysis is based on; null until computed.
    #[schema(required = true)]
    base_fingerprint: Option<String>,
    #[schema(required = true)]
    source_fingerprint: Option<String>,
    #[schema(required = true)]
    target_fingerprint: Option<String>,
    /// Every changed object with its outcome, the conflicts (base, target and source side by
    /// side) and the reported objects; null while the first compute runs.
    #[schema(value_type = Option<Object>, required = true)]
    analysis: Option<Value>,
    #[schema(value_type = Vec<ResolutionBody>)]
    resolutions: Value,
    #[schema(value_type = Vec<UserStepBody>)]
    steps: Value,
    /// Ids of the items (or, for a step that has none, of the step) whose data loss or
    /// security change the user accepts.
    #[schema(value_type = Vec<String>)]
    acknowledgements: Value,
    #[schema(value_type = Object)]
    options: Value,
    /// The ordered steps by phase (`{steps}`); null until computed, and while conflicts are open.
    #[schema(value_type = Option<Object>, required = true)]
    plan: Option<Value>,
    /// Timings, drift and own-SQL outcomes of the last rehearsal; null when there is none.
    #[schema(value_type = Option<Object>, required = true)]
    rehearsal: Option<Value>,
    /// Created roles, drift and the after-merge outcome; null until the merge finishes. An
    /// abandoned merge (E27) has `leftovers`: what phase 1 left on the target that phase 2
    /// didn't finish (`{kind: not_valid_constraint | not_null_check, schema, table, name}`).
    #[schema(value_type = Option<Object>, required = true)]
    result: Option<Value>,
}

impl From<MergeRecord> for Merge {
    fn from(r: MergeRecord) -> Self {
        let summary = MergeSummary::from(&r);
        Self {
            id: summary.id,
            source_branch_id: summary.source_branch_id,
            target_branch_id: summary.target_branch_id,
            direction: summary.direction,
            database: summary.database,
            status: summary.status,
            failed_phase: summary.failed_phase,
            version: summary.version,
            rebaselined: summary.rebaselined,
            error: summary.error,
            created_by: summary.created_by,
            created_at: summary.created_at,
            updated_at: summary.updated_at,
            base_fingerprint: r.base_fingerprint,
            source_fingerprint: r.source_fingerprint,
            target_fingerprint: r.target_fingerprint,
            analysis: r.analysis,
            resolutions: r.resolutions,
            steps: r.user_steps,
            acknowledgements: r.acknowledgements,
            options: r.options,
            // T14 M11: the steps only; the rest is the worker's (the expected model alone can
            // be ~100 KB, and Studio polls).
            plan: r.plan.map(|p| json!({ "steps": p.get("steps").cloned().unwrap_or_else(|| json!([])) })),
            rehearsal: r.rehearsal,
            result: r.result,
        }
    }
}

/// One line of a merge's log.
#[derive(Debug, Serialize, ToSchema)]
struct MergeLogEntry {
    at: DateTime<Utc>,
    /// `status`, `step` or `note`.
    kind: String,
    #[schema(required = true)]
    step_id: Option<String>,
    #[schema(required = true)]
    phase: Option<String>,
    /// `ok`, `failed` or `skipped` for a step.
    #[schema(required = true)]
    outcome: Option<String>,
    #[schema(required = true)]
    duration_ms: Option<i64>,
    #[schema(value_type = Option<Object>, required = true)]
    detail: Option<Value>,
}

impl From<MergeEvent> for MergeLogEntry {
    fn from(e: MergeEvent) -> Self {
        Self {
            at: e.at,
            kind: e.kind,
            step_id: e.step_id,
            phase: e.phase,
            outcome: e.outcome,
            duration_ms: e.duration_ms,
            detail: e.detail,
        }
    }
}

/// A merge and its log, oldest line first.
#[derive(Debug, Serialize, ToSchema)]
struct MergeDetail {
    merge: Merge,
    events: Vec<MergeLogEntry>,
}

// ---- request bodies -----------------------------------------------------------------------

/// How the user settles one conflict.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum ChoiceBody {
    /// The item stays out of the plan.
    KeepTarget,
    /// The plan moves the target's version to the source's (data loss is flagged).
    TakeSource,
    /// Only for column order conflicts: the target's columns first, then the source's.
    TargetColumnsFirst,
    /// Replaces the item in the plan.
    OwnSql(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct ResolutionBody {
    /// The conflict's id, from the analysis.
    conflict: String,
    choice: ChoiceBody,
}

/// SQL the user inserts into the plan.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct UserStepBody {
    /// The user's own id for the step: without `:`, `.` or control characters, and not
    /// `phase1` (the plan's own ids and the merge's log use them).
    id: String,
    /// The step it follows (generated or user); null puts it at the start of its phase.
    after: Option<String>,
    sql: String,
}

/// What the merge does when it is done with the branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
enum AfterMerge {
    Keep,
    Reset,
    Delete,
}

impl AfterMerge {
    fn as_str(self) -> &'static str {
        match self {
            Self::Keep => "keep",
            Self::Reset => "reset",
            Self::Delete => "delete",
        }
    }

    fn job(self) -> MergeAfter {
        match self {
            Self::Keep => MergeAfter::Keep,
            Self::Reset => MergeAfter::Reset,
            Self::Delete => MergeAfter::Delete,
        }
    }
}

/// The after-merge choice run by hand on a merge applied with drift (E21).
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct FinishRequest {
    after: AfterMerge,
}

/// Both optional; a missing one stays as it is.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct MergeOptionsBody {
    /// `SET LOCAL lock_timeout` of the plan's transaction, 100 to 600000 ms.
    lock_timeout_ms: Option<u32>,
    /// What happens to the source branch once the merge is done: kept (default), reset to its
    /// parent or deleted.
    after: Option<AfterMerge>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateMergeRequest {
    direction: Direction,
    /// The Postgres database to merge; the branch's main one when omitted.
    database: Option<String>,
}

/// Every field but `version` is optional; a missing one stays as it is.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateMergeRequest {
    /// The `version` you last saw; another one is a 409 `merge_version_conflict`.
    version: i64,
    resolutions: Option<Vec<ResolutionBody>>,
    steps: Option<Vec<UserStepBody>>,
    acknowledgements: Option<Vec<String>>,
    options: Option<MergeOptionsBody>,
}

#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct RecomputeRequest {
    /// Use the target's current schema as the base (a two-way diff), when the base is gone.
    rebaseline: Option<bool>,
}

#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct ApplyRequest {
    /// Required to merge into a protected branch.
    confirm: Option<bool>,
    /// The `version` you last saw (send it: a save since, such as a new after-merge choice,
    /// keeps the merge rehearsed); another one is a 409 `merge_version_conflict`.
    version: Option<i64>,
}

#[derive(Debug, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct RehearseRequest {
    /// The `version` you last saw; another one is a 409 `merge_version_conflict`.
    version: Option<i64>,
}

/// The 409 for an action on a merge that changed since the caller read it (`version`), with
/// the merge as it is now in `error.current`.
fn version_conflict(current: &MergeRecord) -> ApiError {
    conflict(ErrorCode::MergeVersionConflict, "the merge changed since you loaded it")
        .with_extra("current", serde_json::to_value(Merge::from(current.clone())).unwrap_or_default())
}

/// The version the action runs on: the one the caller saw, or the one just read; a caller's
/// version that isn't current is the 409.
fn seen_version(merge: &MergeRecord, sent: Option<i64>) -> Result<i64, ApiError> {
    match sent {
        Some(v) if v != merge.version => Err(version_conflict(merge)),
        _ => Ok(merge.version),
    }
}

/// An absent body is the default; anything else must parse.
fn optional_body<T: serde::de::DeserializeOwned + Default>(body: &[u8]) -> Result<T, ApiError> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    parse_body(&axum::body::Bytes::copy_from_slice(body))
}

// ---- shared steps -------------------------------------------------------------------------

fn internal<E: std::fmt::Display>(what: &str, err: E) -> ApiError {
    error!("{what}: {err}");
    ApiError::internal()
}

fn merge_enqueue_failed() -> ApiError {
    ApiError::new(503, ErrorCode::WorkerUnavailable, "failed to enqueue merge job")
}

/// The merge of this database, or the 404 to send.
async fn load_merge<M, J>(
    state: &AppState<M, J>,
    database: &DatabaseRecord,
    id: Uuid,
) -> Result<MergeRecord, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    match state.metadata.get_merge(id).await {
        Ok(Some(m)) if m.database_id == database.database_id.as_uuid() => Ok(m),
        Ok(_) => Err(ApiError::not_found("merge not found")),
        Err(err) => Err(internal("load merge", err)),
    }
}

async fn publish<M, J>(state: &AppState<M, J>, job: MergeJob) -> bool
where
    M: MetadataPort,
    J: JobPort,
{
    let job = JobEnvelope { job_id: Uuid::new_v4(), payload: job };
    match state.js.publish_merge(&job).await {
        Ok(()) => true,
        Err(err) => {
            error!("publish merge job failed: {err}");
            false
        }
    }
}

/// `err` is the write running into the unique index `constraint` (`Conflict` is how a test
/// double raises the same refusal).
fn refused_by(err: &MetadataError, constraint: &str) -> bool {
    err.is_unique_violation(constraint) || matches!(err, MetadataError::Conflict)
}

fn busy(id: Option<Uuid>) -> ApiError {
    let err = conflict(
        ErrorCode::MergeBusy,
        "another merge is changing the target branch, or failed half applied on it; wait for it, \
         or retry or abandon it",
    );
    match id {
        Some(id) => err.with_extra("merge_id", json!(id)),
        None => err,
    }
}

fn wrong_state(action: &str, status: MergeStatus) -> ApiError {
    conflict(
        ErrorCode::Conflict,
        format!("cannot {action} a merge that is {}", status.as_str()),
    )
}

/// The merge applying on `target` other than `me`.
async fn applying_on<M, J>(
    state: &AppState<M, J>,
    database: &DatabaseRecord,
    target: Uuid,
    me: Uuid,
) -> Result<Option<Uuid>, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let rows = state
        .metadata
        .list_merges(database.database_id, Some(BranchId(target)))
        .await
        .map_err(|e| internal("list merges", e))?;
    Ok(rows
        .iter()
        .find(|r| r.id != me && r.target_branch_id == target && r.state().is_applying())
        .map(|r| r.id))
}

/// A running merge holds its branches: reset, restore and delete are refused meanwhile.
pub(crate) async fn refuse_if_merge_busy<M, J>(
    state: &AppState<M, J>,
    branch: BranchId,
) -> Result<(), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    match state.metadata.branch_has_busy_merge(branch).await {
        Ok(None) => Ok(()),
        Ok(Some(busy)) => Err(merge_busy_refusal(busy)),
        Err(err) => Err(internal("branch busy merge", err)),
    }
}

/// The 409 for a branch a merge holds (T14 F3): `error.merge_id`, or `error.rehearsal_branch_id`
/// for a rehearsal not torn down yet.
pub(crate) fn merge_busy_refusal(busy: BranchBusy) -> ApiError {
    match busy {
        BranchBusy::Merge(id) => conflict(
            ErrorCode::BranchMergeBusy,
            "a merge is rehearsing or applying on this branch, or failed half applied; wait for \
             it, cancel it, or retry or abandon it",
        )
        .with_extra("merge_id", json!(id)),
        BranchBusy::Rehearsal(id) => conflict(
            ErrorCode::BranchMergeBusy,
            "a merge's rehearsal of this branch is still being removed; try again in a few minutes",
        )
        .with_extra("rehearsal_branch_id", json!(id)),
    }
}

/// Called after a branch delete started: its merges nothing is running are cancelled.
pub(crate) async fn cancel_drafts<M, J>(state: &AppState<M, J>, branch: BranchId)
where
    M: MetadataPort,
    J: JobPort,
{
    match state.metadata.cancel_drafts_for_branch(branch).await {
        Ok(0) => {}
        Ok(n) => info!(%branch, cancelled = n, "cancelled merges of a deleted branch"),
        Err(err) => warn!(%branch, "cancel merges of a deleted branch: {err}"),
    }
}

/// Undoes a move after its job couldn't be published; a failed undo is logged (T14 M11), and
/// reconcile hands a stranded working merge back.
async fn revert<M, J>(state: &AppState<M, J>, id: Uuid, from: &[MergeStatus], to: MergeStatus, patch: MergeStatusPatch)
where
    M: MetadataPort,
    J: JobPort,
{
    match state.metadata.set_merge_status(id, from, to, patch).await {
        Ok(true) => {}
        Ok(false) => error!(merge_id = %id, to = to.as_str(), "undoing a move after a failed publish: the merge moved on"),
        Err(err) => error!(merge_id = %id, to = to.as_str(), "undoing a move after a failed publish: {err}"),
    }
}

/// Both branches of `merge` are ready, or the 409 `branch_not_ready` naming the one that isn't
/// (T10 minor: rehearse and apply; the worker checks again).
async fn branches_ready<M, J>(state: &AppState<M, J>, merge: &MergeRecord) -> Result<(), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    for id in [merge.source_branch_id, merge.target_branch_id] {
        let branch = state.metadata.get_branch(BranchId(id)).await.map_err(|e| internal("load branch", e))?;
        match branch {
            Some(b) if b.status == BranchStatus::Ready => {}
            Some(b) => {
                return Err(conflict(
                    ErrorCode::BranchNotReady,
                    format!("branch {} is not ready (it is {})", b.name, b.status.as_str()),
                ));
            }
            None => return Err(conflict(ErrorCode::BranchNotReady, "a branch of the merge no longer exists")),
        }
    }
    Ok(())
}

/// The 409 for a guarded move that wrote nothing: a branch stopped being ready, or the merge
/// changed.
async fn not_moved<M, J>(state: &AppState<M, J>, merge: &MergeRecord, changed: ApiError) -> ApiError
where
    M: MetadataPort,
    J: JobPort,
{
    match branches_ready(state, merge).await {
        Ok(()) => changed,
        Err(err) => err,
    }
}

/// `options` with `f` applied to its object.
fn options_with(current: &Value, f: impl FnOnce(&mut serde_json::Map<String, Value>)) -> Value {
    let mut map = current.as_object().cloned().unwrap_or_default();
    f(&mut map);
    Value::Object(map)
}

// ---- create -------------------------------------------------------------------------------

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/branches/{branch_id}/merges",
    operation_id = "createMerge",
    tag = "merges",
    summary = "Start a schema merge between a branch and its parent",
    description = "`into_parent` merges the branch's schema changes into its parent; `from_parent` brings the parent's changes into the branch. Both branches must be ready and `normal`; main has no parent. The merge starts as `computing`: the worker reads both schemas and the branch point and fills in the analysis and the plan (poll `getMerge` until it is a `draft`). `database` is the Postgres database to merge (the branch's main one when omitted). One merge per pair and Postgres database can be open at a time.",
    params(("id" = Uuid, Path, description = "Database id"), ("branch_id" = Uuid, Path, description = "The branch that merges with its parent")),
    request_body = CreateMergeRequest,
    responses(
        (status = 202, description = "Accepted; the merge is `computing`", body = MergeSummary),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the Postgres database doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`backup_branch`: a restore backup can't be merged; `branch_not_ready`: either branch isn't ready; `merge_open_exists`: the pair has an open merge for this database (`error.merge_id`); `merge_busy`: another merge is applying to the target, or failed half applied on it (`error.merge_id`; retry or abandon that one), or one finished a moment ago (retry)", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the body is the wrong shape, or the branch is main (no parent)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`worker_unavailable`: the job queue can't be reached", body = ErrorEnvelope),
    ),
)]
async fn create_merge<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
    ApiBytes(body): ApiBytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    match create(&state, &principal, id, branch_id, &body).await {
        Ok(record) => (StatusCode::ACCEPTED, Json(MergeSummary::from(&record))).into_response(),
        Err(err) => err.into_response(),
    }
}

/// The Postgres database to merge, which both branches must have.
async fn pick_database<M, J>(
    state: &AppState<M, J>,
    source: &BranchRecord,
    parent: &BranchRecord,
    requested: Option<&str>,
) -> Result<String, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let on_branch = |b: &BranchRecord| {
        let state = &state.metadata;
        let id = b.id;
        async move { state.branch_databases(id).await.map_err(|e| internal("branch databases", e)) }
    };
    let mine = on_branch(source).await?;
    let name = match requested {
        Some(name) => name.to_string(),
        None => mine
            .iter()
            .find(|d| d.origin == DatabaseOrigin::Main)
            .map(|d| d.name.clone())
            .ok_or_else(|| conflict(ErrorCode::Conflict, "the branch has no main database"))?,
    };
    let Some(found) = mine.iter().find(|d| d.name == name) else {
        return Err(ApiError::not_found(format!("database {name} not found on branch {}", source.name)));
    };
    if found.origin == DatabaseOrigin::System {
        return Err(unprocessable("the system database can't be merged"));
    }
    let theirs = on_branch(parent).await?;
    if !theirs.iter().any(|d| d.name == name) {
        return Err(ApiError::not_found(format!("database {name} not found on branch {}", parent.name)));
    }
    Ok(name)
}

async fn create<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    id: Uuid,
    branch_id: Uuid,
    body: &[u8],
) -> Result<MergeRecord, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let request: CreateMergeRequest = parse_body(&axum::body::Bytes::copy_from_slice(body))?;
    let direction: MergeDirection = request.direction.into();
    // The access check reads the protection of the branch merged into: the path's branch's
    // parent, or the path's branch itself when merging from the parent. One the database
    // doesn't hold (or a missing parent) counts as protected; the checks below answer it.
    let merged_into = match direction {
        MergeDirection::FromParent => branch_id,
        MergeDirection::IntoParent => match state.metadata.get_branch(BranchId(branch_id)).await {
            Ok(Some(b)) if b.database_id.as_uuid() == id => {
                b.parent_branch_id.map_or(Uuid::nil(), |p| p.as_uuid())
            }
            Ok(_) => Uuid::nil(),
            Err(err) => return Err(internal("load branch", err)),
        },
    };
    let record = authorize_database(
        state,
        principal,
        ep::CREATE_MERGE.policy.access,
        Target::Branch { database: id, branch: merged_into },
    )
    .await?;
    let branch = load_branch(state, &record, branch_id).await?;
    if branch.kind != BranchKind::Normal {
        return Err(conflict(ErrorCode::BackupBranch, BACKUP_REFUSAL));
    }
    let Some(parent_id) = branch.parent_branch_id else {
        return Err(unprocessable("main has no parent to merge with"));
    };
    let parent = load_branch(state, &record, parent_id.as_uuid()).await?;
    if parent.kind != BranchKind::Normal {
        return Err(conflict(ErrorCode::BackupBranch, BACKUP_REFUSAL));
    }
    for b in [&branch, &parent] {
        if b.status != BranchStatus::Ready {
            return Err(conflict(
                ErrorCode::BranchNotReady,
                format!("branch {} is not ready (it is {})", b.name, b.status.as_str()),
            ));
        }
    }
    let database_name = pick_database(state, &branch, &parent, request.database.as_deref()).await?;
    let (source, target) = match direction {
        MergeDirection::IntoParent => (branch.id, parent.id),
        MergeDirection::FromParent => (parent.id, branch.id),
    };
    let new = NewMerge {
        id: Uuid::new_v4(),
        database_id: record.database_id,
        source_branch_id: source,
        target_branch_id: target,
        direction,
        database_name,
        options: None,
        created_by: principal.sub.clone(),
    };
    let created = match state.metadata.create_merge(&new).await {
        Ok(MergeCreate::Created(m)) => m,
        Ok(MergeCreate::OpenExists(open)) => {
            return Err(conflict(
                ErrorCode::MergeOpenExists,
                "this pair already has an open merge for this database",
            )
            .with_extra("merge_id", json!(open)));
        }
        Ok(MergeCreate::ApplyingExists(applying)) => return Err(busy(Some(applying))),
        // The open merge finished between the insert's refusal and the lookup.
        Err(MetadataError::Conflict) => {
            return Err(conflict(ErrorCode::MergeBusy, "a merge of this pair just finished; try again")
                .retry_after(1));
        }
        Err(err) => return Err(internal("create merge", err)),
    };
    if !publish(state, MergeJob::Compute { merge_id: created.id }).await {
        let failed = MergeStatusPatch { error: Some(Some("publish failed".into())), ..Default::default() };
        revert(state, created.id, &[MergeStatus::Computing], MergeStatus::Failed, failed).await;
        return Err(merge_enqueue_failed());
    }
    info!(request_id = %principal.request_id, database_id = %record.database_id, merge_id = %created.id, "accepted create merge");
    Ok(created)
}

// ---- read ---------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ListQuery {
    /// Only the merges where this branch is the source or the target.
    branch: Option<Uuid>,
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/merges",
    operation_id = "listMerges",
    tag = "merges",
    summary = "List a database's merges",
    description = "History and open merges, newest first, without their analysis and plan (read one with `getMerge`).",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch" = Option<Uuid>, Query, description = "Only the merges where this branch is the source or the target"),
    ),
    responses(
        (status = 200, description = "The merges, newest first", body = Vec<MergeSummary>),
        (status = 400, description = "`invalid_request`: a path id or `branch` isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database doesn't exist, or belongs to another tenant", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_merges<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match authorize_database(&state, &principal, ep::LIST_MERGES.policy.access, Target::Database(id)).await {
        Ok(record) => record,
        Err(resp) => return resp.into_response(),
    };
    let Query(ListQuery { branch }) = match query {
        Ok(query) => query,
        Err(rej) => {
            return ApiError::invalid_request(400, format!("invalid query: {}", rej.body_text())).into_response();
        }
    };
    match state.metadata.list_merges(record.database_id, branch.map(BranchId)).await {
        Ok(rows) => Json(rows.iter().map(MergeSummary::from).collect::<Vec<_>>()).into_response(),
        Err(err) => internal("list merges", err).into_response(),
    }
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/merges/{merge_id}",
    operation_id = "getMerge",
    tag = "merges",
    summary = "Read a merge",
    description = "The merge with its analysis (every changed object, the conflicts with base, target and source side by side), the plan, the last rehearsal, the result and the log. Poll it while the status is `computing`, `rehearsing`, `applying`, `online` or `finishing`, and while a requested after-merge choice is pending (`result.after.requested` or `started`).",
    params(("id" = Uuid, Path, description = "Database id"), ("merge_id" = Uuid, Path, description = "Merge id")),
    responses(
        (status = 200, description = "The merge and its log", body = MergeDetail),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database or the merge doesn't exist, or belongs to another tenant", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn get_merge<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, merge_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let result = async {
        let record = authorize_database(&state, &principal, ep::GET_MERGE.policy.access, Target::Database(id)).await?;
        let merge = load_merge(&state, &record, merge_id).await?;
        let events = state.metadata.merge_events(merge_id).await.map_err(|e| internal("merge events", e))?;
        Ok::<_, ApiError>(MergeDetail {
            merge: merge.into(),
            events: events.into_iter().map(MergeLogEntry::from).collect(),
        })
    }
    .await;
    match result {
        Ok(detail) => Json(detail).into_response(),
        Err(err) => err.into_response(),
    }
}

// ---- edit ---------------------------------------------------------------------------------

fn short(what: &str, text: &str) -> Result<(), ApiError> {
    if text.trim().is_empty() {
        return Err(unprocessable(&format!("{what} can't be empty")));
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(unprocessable(&format!("{what} is over {MAX_TEXT_BYTES} bytes")));
    }
    Ok(())
}

fn short_id(what: &str, text: &str) -> Result<(), ApiError> {
    if text.is_empty() || text.len() > MAX_ID_BYTES {
        return Err(unprocessable(&format!("{what} must be 1 to {MAX_ID_BYTES} bytes")));
    }
    Ok(())
}

/// What the request sets, validated, as the patch (without `start_compute`).
fn draft_patch(request: &UpdateMergeRequest, current_options: &Value) -> Result<MergeDraftPatch, ApiError> {
    let mut patch = MergeDraftPatch::default();
    if let Some(resolutions) = &request.resolutions {
        if resolutions.len() > MAX_RESOLUTIONS {
            return Err(unprocessable(&format!("at most {MAX_RESOLUTIONS} resolutions")));
        }
        for r in resolutions {
            short_id("a conflict id", &r.conflict)?;
            if let ChoiceBody::OwnSql(sql) = &r.choice {
                short("own SQL", sql)?;
            }
        }
        patch.resolutions = Some(serde_json::to_value(resolutions).map_err(|e| internal("encode resolutions", e))?);
    }
    if let Some(steps) = &request.steps {
        if steps.len() > MAX_STEPS {
            return Err(unprocessable(&format!("at most {MAX_STEPS} steps")));
        }
        let mut ids = HashSet::new();
        for s in steps {
            short_id("a step id", &s.id)?;
            // The plan's own ids use `:` and `.` (`<kind>:<path>`, `<id>.<n>`): a user id with
            // them could collide (T14 apply minor 3).
            if s.id.chars().any(|c| c == ':' || c == '.' || c.is_control()) {
                return Err(unprocessable(&format!(
                    "step id {:?} can't hold ':', '.' or control characters",
                    s.id
                )));
            }
            // The merge's log names the plan's transaction so (T14 F3 fix round, M8).
            if s.id == chelabase_schema::plan::PHASE1_STEP_ID {
                return Err(unprocessable(&format!("step id {:?} is reserved", s.id)));
            }
            short("a step's SQL", &s.sql)?;
            if !ids.insert(s.id.as_str()) {
                return Err(unprocessable(&format!("step id {} is used twice", s.id)));
            }
        }
        patch.user_steps = Some(serde_json::to_value(steps).map_err(|e| internal("encode steps", e))?);
    }
    if let Some(acks) = &request.acknowledgements {
        if acks.len() > MAX_ACKNOWLEDGEMENTS {
            return Err(unprocessable(&format!("at most {MAX_ACKNOWLEDGEMENTS} acknowledgements")));
        }
        for a in acks {
            short_id("an acknowledgement", a)?;
        }
        patch.acknowledgements = Some(json!(acks));
    }
    if let Some(options) = &request.options {
        if options.lock_timeout_ms.is_some_and(|ms| !LOCK_TIMEOUT_MS.contains(&ms)) {
            return Err(unprocessable(&format!(
                "lock_timeout_ms must be between {} and {}",
                LOCK_TIMEOUT_MS.start(),
                LOCK_TIMEOUT_MS.end()
            )));
        }
        patch.options = Some(options_with(current_options, |map| {
            if let Some(ms) = options.lock_timeout_ms {
                map.insert("lock_timeout_ms".into(), json!(ms));
            }
            if let Some(after) = options.after {
                map.insert("after".into(), json!(after.as_str()));
            }
        }));
    }
    // E34: acknowledgements and the after-merge choice don't change the plan; anything else
    // is recomputed.
    let plan_inputs = request.resolutions.is_some()
        || request.steps.is_some()
        || request.options.as_ref().is_some_and(|o| o.lock_timeout_ms.is_some());
    patch.start_compute = plan_inputs;
    patch.keep_plan = !plan_inputs;
    Ok(patch)
}

#[utoipa::path(
    patch,
    path = "/v1/databases/{id}/merges/{merge_id}",
    operation_id = "updateMerge",
    tag = "merges",
    summary = "Edit a draft merge",
    description = "Saves the resolutions, own steps, acknowledgements and options of a `draft` or `rehearsed` merge. A save of only `acknowledgements` and `options.after` keeps the merge as it is (a rehearsed merge stays rehearsed). Any other save (resolutions, steps, `options.lock_timeout_ms`) moves it to `computing` (a rehearsed one loses its rehearsal): the worker recomputes the analysis and the plan from what was saved, keeping a resolution whose conflict is unchanged, and returns it to `draft`. `version` must be the one you last read.",
    params(("id" = Uuid, Path, description = "Database id"), ("merge_id" = Uuid, Path, description = "Merge id")),
    request_body = UpdateMergeRequest,
    responses(
        (status = 200, description = "The merge after the save (`computing`, or as it was for an acknowledgement or after-merge choice)", body = Merge),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant; `forbidden`: acknowledging (or un-acknowledging) a security step needs an Admin (`required: \"admin\"`)", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database or the merge doesn't exist, or belongs to another tenant", body = ErrorEnvelope),
        (status = 409, description = "`merge_version_conflict`: `version` is stale (the response carries the merge as `error.current`); `conflict`: the merge isn't a draft or rehearsed", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: an unknown or missing field, an empty or oversized text, or an option out of range", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`worker_unavailable`: the job queue can't be reached (the merge is `failed`; recompute it)", body = ErrorEnvelope),
    ),
)]
async fn update_merge<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, merge_id)): ApiPath<(Uuid, Uuid)>,
    ApiBytes(body): ApiBytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    match update(&state, &principal, id, merge_id, &body).await {
        Ok(record) => Json(Merge::from(record)).into_response(),
        Err(err) => err.into_response(),
    }
}

async fn update<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    id: Uuid,
    merge_id: Uuid,
    body: &[u8],
) -> Result<MergeRecord, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let authorized = authorize_merge_full(state, principal, ep::UPDATE_MERGE.policy.access, id, merge_id).await?;
    let record = authorized.clone().into_record()?;
    let merge = load_merge(state, &record, merge_id).await?;
    let request: UpdateMergeRequest = parse_body(&axum::body::Bytes::copy_from_slice(body))?;
    // R25: acknowledging a security step is an Admin's act, adding or removing it.
    if let Some(acks) = &request.acknowledgements
        && !is_admin(&authorized)
    {
        let secure = security_items(&merge);
        let old: HashSet<&str> = merge.acknowledgements.as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let new: HashSet<&str> = acks.iter().map(String::as_str).collect();
        if new.symmetric_difference(&old).any(|item| secure.contains(*item)) {
            return Err(admin_only());
        }
    }
    let patch = draft_patch(&request, &merge.options)?;
    // One statement saves, bumps the version and moves to `computing`: of two saves of one
    // version only one wins, and only the winner publishes.
    match state.metadata.update_merge_draft(merge_id, request.version, &patch).await {
        Ok(MergeWrite::Saved(saved)) => {
            if patch.start_compute && !publish(state, MergeJob::Compute { merge_id }).await {
                let failed = MergeStatusPatch { error: Some(Some("publish failed".into())), ..Default::default() };
                revert(state, merge_id, &[MergeStatus::Computing], MergeStatus::Failed, failed).await;
                return Err(merge_enqueue_failed());
            }
            info!(request_id = %principal.request_id, %merge_id, version = saved.version, "merge saved");
            Ok(saved)
        }
        Ok(MergeWrite::Stale(current)) => Err(version_conflict(&current)),
        Ok(MergeWrite::NotFound) => Err(ApiError::not_found("merge not found")),
        Ok(MergeWrite::NotEditable(status)) => Err(wrong_state("edit", status)),
        Err(err) => Err(internal("update merge", err)),
    }
}

// ---- actions ------------------------------------------------------------------------------

/// Whether the plan has a security step (BYPASSRLS, owner-level; E28): applying it needs Admin.
fn has_security_steps(merge: &MergeRecord) -> bool {
    merge
        .plan
        .as_ref()
        .and_then(|p| p["steps"].as_array())
        .into_iter()
        .flatten()
        .any(|step| match serde_json::from_value::<chelabase_schema::plan::Step>(step.clone()) {
            Ok(s) => s.security,
            // A step this build can't read: by its flag, as the engine writes it; without one it
            // counts as a security step (an access check fails closed).
            Err(_) => step["security"].as_bool().unwrap_or(true),
        })
}

/// The items an acknowledgement of a security step goes by: the step's items, or its id when
/// it has none (as `unacknowledged` reads them).
fn security_items(merge: &MergeRecord) -> HashSet<String> {
    let mut items = HashSet::new();
    for step in merge.plan.as_ref().and_then(|p| p["steps"].as_array()).into_iter().flatten() {
        let secure = match serde_json::from_value::<chelabase_schema::plan::Step>(step.clone()) {
            Ok(s) => s.security,
            Err(_) => step["security"].as_bool().unwrap_or(true),
        };
        if !secure {
            continue;
        }
        let listed: Vec<&str> = step["items"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        if listed.is_empty() {
            items.extend(step["id"].as_str().map(str::to_string));
        } else {
            items.extend(listed.into_iter().map(str::to_string));
        }
    }
    items
}

fn admin_only() -> ApiError {
    crate::access::deny_to_api(chelabase_types::access::Deny::Forbidden {
        required: PlatformRole::Admin.as_str().to_string(),
    })
}

fn is_admin(authorized: &Authorized) -> bool {
    authorized.grant.as_ref().is_some_and(|g| g.platform_role == PlatformRole::Admin)
}

/// The conflict ids the analysis lists that no resolution answers.
fn open_conflicts(merge: &MergeRecord) -> Vec<String> {
    let resolved: HashSet<&str> = merge
        .resolutions
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["conflict"].as_str())
        .collect();
    merge
        .analysis
        .as_ref()
        .and_then(|a| a["conflicts"].as_array())
        .into_iter()
        .flatten()
        .filter_map(|c| c["id"].as_str())
        .filter(|id| !resolved.contains(id))
        .map(str::to_string)
        .collect()
}

/// The items of plan steps that need an acknowledgement (`Step::needs_acknowledgement`: data
/// loss, a security change, a breaking change such as a role rename, E29a) that no
/// acknowledgement covers; a step with no items is covered by its own id.
fn unacknowledged(merge: &MergeRecord) -> Vec<String> {
    let acked: HashSet<&str> = merge.acknowledgements.as_array().into_iter().flatten().filter_map(Value::as_str).collect();
    let mut missing = Vec::new();
    for step in merge.plan.as_ref().and_then(|p| p["steps"].as_array()).into_iter().flatten() {
        let flagged = match serde_json::from_value::<chelabase_schema::plan::Step>(step.clone()) {
            Ok(s) => s.needs_acknowledgement(),
            // A step this build can't read: by its flags, as the engine writes them.
            Err(_) => {
                step["data_loss"].as_bool().unwrap_or(false)
                    || step["security"].as_bool().unwrap_or(false)
                    || step["breaking"].is_object()
            }
        };
        if !flagged {
            continue;
        }
        let items: Vec<&str> = step["items"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let wanted = if items.is_empty() { step["id"].as_str().into_iter().collect() } else { items };
        missing.extend(wanted.into_iter().filter(|w| !acked.contains(w)).map(str::to_string));
    }
    missing.sort();
    missing.dedup();
    missing
}

fn accepted(status: &MergeRecord, to: MergeStatus) -> Response {
    let mut summary = MergeSummary::from(status);
    summary.status = to.into();
    (StatusCode::ACCEPTED, Json(summary)).into_response()
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/merges/{merge_id}/rehearse",
    operation_id = "rehearseMerge",
    tag = "merges",
    summary = "Rehearse a draft merge",
    description = "Runs the plan on a throwaway branch of the target (real data, so a failing constraint shows) and compares the result with the merged schema. Needs a `draft` with a plan and no open conflicts. Success makes the merge `rehearsed`; a failure returns it to `draft` with the failing step and the Postgres error in `rehearsal`. Send the `version` you last saw: the rehearsal then runs that draft.",
    params(("id" = Uuid, Path, description = "Database id"), ("merge_id" = Uuid, Path, description = "Merge id")),
    request_body(content = RehearseRequest, description = "Optional; an empty body is `{}`"),
    responses(
        (status = 202, description = "Accepted; the merge is `rehearsing`", body = MergeSummary),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database or the merge doesn't exist, or belongs to another tenant", body = ErrorEnvelope),
        (status = 409, description = "`merge_version_conflict`: `version` isn't the merge's (the response carries the merge as `error.current`); `merge_conflicts_open`: a conflict has no resolution; `branch_not_ready`: either branch isn't ready; `conflict`: the merge isn't a draft, has no plan yet, or changed since it was read", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: an unknown field, or `version` isn't an integer", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`worker_unavailable`: the job queue can't be reached", body = ErrorEnvelope),
    ),
)]
async fn rehearse_merge<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, merge_id)): ApiPath<(Uuid, Uuid)>,
    ApiBytes(body): ApiBytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let result = async {
        let record = authorize_merge(&state, &principal, ep::REHEARSE_MERGE.policy.access, id, merge_id).await?;
        let merge = load_merge(&state, &record, merge_id).await?;
        let request: RehearseRequest = optional_body(&body)?;
        if merge.state().after(MergeAction::Rehearse).is_none() {
            return Err(wrong_state("rehearse", merge.status));
        }
        let version = seen_version(&merge, request.version)?;
        let open = open_conflicts(&merge);
        if !open.is_empty() {
            return Err(conflict(
                ErrorCode::MergeConflictsOpen,
                format!("{} conflict(s) have no resolution: {}", open.len(), open.join(", ")),
            ));
        }
        if merge.plan.is_none() {
            return Err(conflict(ErrorCode::Conflict, "the merge has no plan yet"));
        }
        branches_ready(&state, &merge).await?;
        // The draft the user saw (T14 M11), with both branches still ready under their locks.
        let guarded = MergeStatusPatch {
            guard_version: Some(version),
            guard_branches_ready: true,
            ..Default::default()
        };
        let moved = state
            .metadata
            .set_merge_status(merge_id, &[MergeStatus::Draft], MergeStatus::Rehearsing, guarded)
            .await
            .map_err(|e| internal("start rehearsal", e))?;
        if !moved {
            return Err(not_moved(&state, &merge, conflict(ErrorCode::Conflict, "the merge changed; read it again")).await);
        }
        if !publish(&state, MergeJob::Rehearse { merge_id }).await {
            revert(&state, merge_id, &[MergeStatus::Rehearsing], MergeStatus::Draft, MergeStatusPatch::default()).await;
            return Err(merge_enqueue_failed());
        }
        info!(request_id = %principal.request_id, %merge_id, "rehearsal requested");
        Ok(accepted(&merge, MergeStatus::Rehearsing))
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/merges/{merge_id}/apply",
    operation_id = "applyMerge",
    tag = "merges",
    summary = "Apply a rehearsed merge to its target",
    description = "Refuses, in this order: a merge that isn't `rehearsed` (`merge_not_rehearsed`, or `merge_stale` for a stale one); a `version` that isn't the merge's (`merge_version_conflict`: a save of acknowledgements or the after-merge choice keeps a merge rehearsed, so send the version you last saw to apply what you saw); conflicts without a resolution (`merge_conflicts_open`); data-loss, security (BYPASSRLS, owner-level) or breaking (a role rename) steps without an acknowledgement (`merge_drops_unacknowledged`); a protected target without `confirm: true` (`merge_confirm_required`); another merge applying to the target, or failed half applied on it (`merge_busy`). The worker then runs the plan (`applying`, then `online` for the concurrent index and validation steps, then `finishing`). The request doesn't read the target: before the plan's transaction the worker compares the target with the schema the plan was computed from, and if it changed, ends the merge `stale` without applying the transaction.",
    params(("id" = Uuid, Path, description = "Database id"), ("merge_id" = Uuid, Path, description = "Merge id")),
    request_body(content = ApplyRequest, description = "Optional; an empty body is `{}`"),
    responses(
        (status = 202, description = "Accepted; the merge is `applying`", body = MergeSummary),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant; `forbidden`: a plan with a security step (BYPASSRLS, owner-level) needs an Admin (`required: \"admin\"`)", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database or the merge doesn't exist, or belongs to another tenant", body = ErrorEnvelope),
        (status = 409, description = "`merge_not_rehearsed`, `merge_stale`, `merge_version_conflict` (`error.current`), `merge_conflicts_open`, `merge_drops_unacknowledged`, `merge_confirm_required`, `merge_busy` (`error.merge_id`): see the description; `branch_not_ready`: either branch isn't ready", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: an unknown field, `confirm` isn't a boolean, or `version` isn't an integer", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`worker_unavailable`: the job queue can't be reached (the merge stays `rehearsed`)", body = ErrorEnvelope),
    ),
)]
async fn apply_merge<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, merge_id)): ApiPath<(Uuid, Uuid)>,
    ApiBytes(body): ApiBytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let result = async {
        let authorized = authorize_merge_full(&state, &principal, ep::APPLY_MERGE.policy.access, id, merge_id).await?;
        let record = authorized.clone().into_record()?;
        let merge = load_merge(&state, &record, merge_id).await?;
        // Security steps (BYPASSRLS, owner-level) are acknowledged by an Admin, on any target.
        if has_security_steps(&merge) && !is_admin(&authorized) {
            return Err(admin_only());
        }
        let request: ApplyRequest = optional_body(&body)?;
        match merge.status {
            _ if merge.state().after(MergeAction::Apply).is_some() => {}
            MergeStatus::Stale => {
                return Err(conflict(ErrorCode::MergeStale, "the merge is stale: a schema changed since it was computed; recompute the merge"));
            }
            other => {
                return Err(conflict(
                    ErrorCode::MergeNotRehearsed,
                    format!("the merge is {}; only a rehearsed merge can be applied", other.as_str()),
                ));
            }
        }
        // T14 F3 fix round (M4): what the user saw, after-merge choice included.
        let version = seen_version(&merge, request.version)?;
        let open = open_conflicts(&merge);
        if !open.is_empty() {
            return Err(conflict(
                ErrorCode::MergeConflictsOpen,
                format!("{} conflict(s) have no resolution: {}", open.len(), open.join(", ")),
            ));
        }
        let missing = unacknowledged(&merge);
        if !missing.is_empty() {
            return Err(conflict(
                ErrorCode::MergeDropsUnacknowledged,
                format!("acknowledge these data-loss, security or breaking changes first: {}", missing.join(", ")),
            ));
        }
        let target = state
            .metadata
            .get_branch(BranchId(merge.target_branch_id))
            .await
            .map_err(|e| internal("load target branch", e))?
            .ok_or_else(|| ApiError::not_found("target branch not found"))?;
        if target.protected && request.confirm != Some(true) {
            return Err(conflict(
                ErrorCode::MergeConfirmRequired,
                format!("branch {} is protected; send confirm: true to merge into it", target.name),
            ));
        }
        if let Some(other) = applying_on(&state, &record, merge.target_branch_id, merge_id).await? {
            return Err(busy(Some(other)));
        }
        branches_ready(&state, &merge).await?;
        // I2: the move logs the attempt's start; M12: both branches still ready under their locks.
        let start = MergeStatusPatch {
            error: Some(None),
            guard_version: Some(version),
            guard_branches_ready: true,
            log_status: true,
            ..Default::default()
        };
        let moved = state.metadata.set_merge_status(merge_id, &[MergeStatus::Rehearsed], MergeStatus::Applying, start).await;
        match moved {
            Ok(true) => {}
            Ok(false) => {
                return Err(not_moved(&state, &merge, conflict(ErrorCode::MergeNotRehearsed, "the merge changed; read it again")).await);
            }
            Err(err) if refused_by(&err, APPLYING_INDEX) => {
                let other = applying_on(&state, &record, merge.target_branch_id, merge_id).await.ok().flatten();
                return Err(busy(other));
            }
            Err(err) => return Err(internal("start apply", err)),
        }
        if !publish(&state, MergeJob::Apply { merge_id }).await {
            revert(&state, merge_id, &[MergeStatus::Applying], MergeStatus::Rehearsed, MergeStatusPatch::default()).await;
            return Err(merge_enqueue_failed());
        }
        info!(request_id = %principal.request_id, %merge_id, target = %target.id, "apply requested");
        Ok(accepted(&merge, MergeStatus::Applying))
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/merges/{merge_id}/recompute",
    operation_id = "recomputeMerge",
    tag = "merges",
    summary = "Recompute a merge from the schemas as they are now",
    description = "From `draft`, `stale` or `failed` (not a failure in the online phase: retry that one). The worker reads both schemas again and returns a new draft that keeps the resolutions whose conflicts are unchanged. `rebaseline: true` uses the target's current schema as the base when the real one is gone (`merge_base_unavailable` in `error`): every difference then counts as a source change, so the target's own recent changes show as things to undo.",
    params(("id" = Uuid, Path, description = "Database id"), ("merge_id" = Uuid, Path, description = "Merge id")),
    request_body(content = RecomputeRequest, description = "Optional; an empty body is `{}`"),
    responses(
        (status = 202, description = "Accepted; the merge is `computing`", body = MergeSummary),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database or the merge doesn't exist, or belongs to another tenant", body = ErrorEnvelope),
        (status = 409, description = "`conflict`: the merge is in a state that can't be recomputed; `merge_open_exists`: a newer open merge exists for the pair (`error.merge_id`)", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: an unknown field, or `rebaseline` isn't a boolean", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`worker_unavailable`: the job queue can't be reached (the merge is `failed`; try again)", body = ErrorEnvelope),
    ),
)]
async fn recompute_merge<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, merge_id)): ApiPath<(Uuid, Uuid)>,
    ApiBytes(body): ApiBytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let result = async {
        let record = authorize_merge(&state, &principal, ep::RECOMPUTE_MERGE.policy.access, id, merge_id).await?;
        let merge = load_merge(&state, &record, merge_id).await?;
        let request: RecomputeRequest = optional_body(&body)?;
        match merge.status {
            _ if merge.state().after(MergeAction::Recompute).is_some() => {}
            MergeStatus::Failed => {
                return Err(conflict(
                    ErrorCode::Conflict,
                    "the merge failed in the online phase and is half applied; retry it, or abandon it",
                ));
            }
            other => return Err(wrong_state("recompute", other)),
        }
        let rebaseline = request.rebaseline == Some(true);
        let options = options_with(&merge.options, |map| {
            if rebaseline {
                map.insert("rebaseline".into(), json!(true));
            } else {
                map.remove("rebaseline");
            }
        });
        let patch = MergeStatusPatch {
            options: Some(options),
            error: Some(None),
            failed_phase: Some(None),
            // A failed merge only as it was read: a retry that ran meanwhile may have left it
            // failed in the online phase, half applied.
            guard_failed_phase: Some(merge.failed_phase.clone()),
            ..Default::default()
        };
        let moved = state
            .metadata
            .set_merge_status(merge_id, &[MergeStatus::Draft, MergeStatus::Stale, MergeStatus::Failed], MergeStatus::Computing, patch)
            .await;
        match moved {
            Ok(true) => {}
            Ok(false) => return Err(conflict(ErrorCode::Conflict, "the merge changed; read it again")),
            // A stale or failed merge is closed; a newer one for the pair may be open.
            Err(err) if refused_by(&err, OPEN_INDEX) => {
                let open = state
                    .metadata
                    .list_merges(record.database_id, Some(BranchId(merge.source_branch_id)))
                    .await
                    .ok()
                    .and_then(|rows| {
                        rows.into_iter().find(|r| {
                            r.id != merge_id
                                && r.state().is_open()
                                && r.source_branch_id == merge.source_branch_id
                                && r.target_branch_id == merge.target_branch_id
                                && r.database_name == merge.database_name
                        })
                    })
                    .map(|r| r.id);
                let err = conflict(ErrorCode::MergeOpenExists, "this pair already has an open merge for this database");
                return Err(match open {
                    Some(id) => err.with_extra("merge_id", json!(id)),
                    None => err,
                });
            }
            Err(err) => return Err(internal("start recompute", err)),
        }
        if !publish(&state, MergeJob::Compute { merge_id }).await {
            let failed = MergeStatusPatch { error: Some(Some("publish failed".into())), ..Default::default() };
            revert(&state, merge_id, &[MergeStatus::Computing], MergeStatus::Failed, failed).await;
            return Err(merge_enqueue_failed());
        }
        info!(request_id = %principal.request_id, %merge_id, rebaseline, "recompute requested");
        Ok(accepted(&merge, MergeStatus::Computing))
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/merges/{merge_id}/retry",
    operation_id = "retryMerge",
    tag = "merges",
    summary = "Retry a merge that failed while applying",
    description = "For a `failed` merge whose `failed_phase` is `roles`, `enum_add`, `tx` or `online`. Failed in `online`: the plan's transaction is committed and the target changed; only the online steps (concurrent indexes, constraint validation) are left, and the worker continues where it stopped (a valid index is skipped, an invalid one is dropped and rebuilt); the merge is `online` again. If the plan's transaction is no longer on the target, or the target is gone, the merge fails for good. Failed before (`roles`, `enum_add`, `tx`): the worker first checks whether the plan's transaction committed after all (its answer may have been lost); if it did, it goes on to the online steps; if not, the apply runs again from the start, with its checks (a target that changed since the merge was computed makes it `stale`; roles and enum values the first attempt made stay); the merge is `applying` again.",
    params(("id" = Uuid, Path, description = "Database id"), ("merge_id" = Uuid, Path, description = "Merge id")),
    responses(
        (status = 202, description = "Accepted; the merge is `online` (after an online failure) or `applying` again", body = MergeSummary),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant; `forbidden`: a retry that runs a plan with a security step again needs an Admin (`required: \"admin\"`)", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database or the merge doesn't exist, or belongs to another tenant", body = ErrorEnvelope),
        (status = 409, description = "`conflict`: the merge didn't fail while applying; `merge_busy`: another merge is applying to the target, or failed half applied on it (`error.merge_id`); `branch_not_ready`: either branch isn't ready (a retry before the online phase)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`worker_unavailable`: the job queue can't be reached (the merge stays `failed`)", body = ErrorEnvelope),
    ),
)]
async fn retry_merge<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, merge_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let result = async {
        let authorized = authorize_merge_full(&state, &principal, ep::RETRY_MERGE.policy.access, id, merge_id).await?;
        let record = authorized.clone().into_record()?;
        let merge = load_merge(&state, &record, merge_id).await?;
        // E20: a failure in phase 2 resumes there (phase 1 committed); one before it (roles,
        // enum values, the transaction) applies again from the start, pre-checks included.
        let failed_phase = merge.failed_phase.clone();
        let (to, job) = match merge.state().after(MergeAction::Retry) {
            Some(MergeStatus::Online) => (MergeStatus::Online, MergeJob::Resume { merge_id }),
            Some(to) => (to, MergeJob::Apply { merge_id }),
            None => {
                return Err(conflict(
                    ErrorCode::Conflict,
                    "only a merge that failed while applying (roles, enum_add, tx or online) can be retried",
                ));
            }
        };
        // R25: running the plan again re-runs its security steps (the online resume doesn't).
        if matches!(job, MergeJob::Apply { .. }) && has_security_steps(&merge) && !is_admin(&authorized) {
            return Err(admin_only());
        }
        if let Some(other) = applying_on(&state, &record, merge.target_branch_id, merge_id).await? {
            return Err(busy(Some(other)));
        }
        let start = MergeStatusPatch {
            error: Some(None),
            failed_phase: Some(None),
            guard_failed_phase: Some(failed_phase.clone()),
            guard_branches_ready: to == MergeStatus::Applying,
            // I2: a new apply attempt starts.
            log_status: to == MergeStatus::Applying,
            ..Default::default()
        };
        let moved = state.metadata.set_merge_status(merge_id, &[MergeStatus::Failed], to, start).await;
        match moved {
            Ok(true) => {}
            Ok(false) => {
                return Err(not_moved(&state, &merge, conflict(ErrorCode::Conflict, "the merge changed; read it again")).await);
            }
            Err(err) if refused_by(&err, APPLYING_INDEX) => {
                let other = applying_on(&state, &record, merge.target_branch_id, merge_id).await.ok().flatten();
                return Err(busy(other));
            }
            Err(err) => return Err(internal("start retry", err)),
        }
        if !publish(&state, job).await {
            let back = MergeStatusPatch {
                error: Some(Some("publish failed".into())),
                failed_phase: Some(failed_phase),
                ..Default::default()
            };
            revert(&state, merge_id, &[to], MergeStatus::Failed, back).await;
            return Err(merge_enqueue_failed());
        }
        info!(request_id = %principal.request_id, %merge_id, to = to.as_str(), "retry requested");
        Ok(accepted(&merge, to))
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/merges/{merge_id}/cancel",
    operation_id = "cancelMerge",
    tag = "merges",
    summary = "Cancel a merge",
    description = "Stops a merge that isn't changing the target: `computing`, `draft`, `rehearsing` (the worker deletes the rehearsal branch; until it is gone the target's reset, restore and delete answer `branch_merge_busy` with `error.rehearsal_branch_id`) or `rehearsed`. An applying merge can't be cancelled, and a merge failed in `online` is abandoned instead.",
    params(("id" = Uuid, Path, description = "Database id"), ("merge_id" = Uuid, Path, description = "Merge id")),
    responses(
        (status = 202, description = "Accepted; the merge is `cancelled`", body = MergeSummary),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database or the merge doesn't exist, or belongs to another tenant", body = ErrorEnvelope),
        (status = 409, description = "`conflict`: the merge is applying or already finished", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn cancel_merge<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, merge_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    const CANCELLABLE: [MergeStatus; 4] =
        [MergeStatus::Computing, MergeStatus::Draft, MergeStatus::Rehearsing, MergeStatus::Rehearsed];
    let result = async {
        let record = authorize_merge(&state, &principal, ep::CANCEL_MERGE.policy.access, id, merge_id).await?;
        let merge = load_merge(&state, &record, merge_id).await?;
        if merge.state().after(MergeAction::Cancel).is_none() {
            return Err(wrong_state("cancel", merge.status));
        }
        let moved = state
            .metadata
            .set_merge_status(merge_id, &CANCELLABLE, MergeStatus::Cancelled, MergeStatusPatch::default())
            .await
            .map_err(|e| internal("cancel merge", e))?;
        if !moved {
            return Err(conflict(ErrorCode::Conflict, "the merge changed; read it again"));
        }
        // A worker holds a computing or rehearsing merge: it tears the rehearsal down. If the
        // job is lost, reconcile deletes the old rehearsal branch.
        if matches!(merge.status, MergeStatus::Computing | MergeStatus::Rehearsing)
            && !publish(&state, MergeJob::Cancel { merge_id }).await
        {
            warn!(%merge_id, "cancel job not published (reconcile will clean up)");
        }
        info!(request_id = %principal.request_id, %merge_id, "merge cancelled");
        Ok(accepted(&merge, MergeStatus::Cancelled))
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
}

/// The online steps of the merge's current apply attempt the log records as done (`ok` or
/// `skipped`): those after the last move to `applying` (I2).
fn online_done(events: &[MergeEvent]) -> HashSet<String> {
    let start = events
        .iter()
        .rposition(|e| e.kind == "status" && e.detail.as_ref().is_some_and(|d| d["to"] == "applying"))
        .map_or(0, |i| i + 1);
    events[start..]
        .iter()
        .filter(|e| {
            e.kind == "step" && e.phase.as_deref() == Some("online") && matches!(e.outcome.as_deref(), Some("ok" | "skipped"))
        })
        .filter_map(|e| e.step_id.clone())
        .collect()
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/merges/{merge_id}/abandon",
    operation_id = "abandonMerge",
    tag = "merges",
    summary = "Give up a merge that failed half applied",
    description = "For a merge `failed` in the `online` phase: its transaction is on the target, its online steps didn't all run, and it holds both branches until it is retried to the end or abandoned. Abandoning makes it `cancelled` and frees the branches. Nothing on the target is undone: `result.leftovers` lists what the merge left there that the online phase didn't finish, each `{kind, schema, table, name}`: `not_valid_constraint` (a constraint added `NOT VALID`, never validated; it still checks new rows); `not_null_check` (a temporary `chelabase_merge_nn_*` check that wasn't dropped); `invalid_index` (the index the online phase was building, or dropping concurrently, when it stopped: Postgres may have left it INVALID, unused by queries but still updated by writes; it may also not be there); `unattached_index` (a valid index the merge built that its next step didn't put into place: a temporary `chelabase_merge_ix_*` index not renamed over the one it replaces, or a unique constraint's index not attached to its constraint; nothing uses it, and it costs on every write). Validate, attach or drop them by hand (`DROP INDEX CONCURRENTLY IF EXISTS` for an index).",
    params(("id" = Uuid, Path, description = "Database id"), ("merge_id" = Uuid, Path, description = "Merge id")),
    responses(
        (status = 200, description = "The merge, `cancelled`, with `result.leftovers`", body = Merge),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database or the merge doesn't exist, or belongs to another tenant", body = ErrorEnvelope),
        (status = 409, description = "`conflict`: the merge didn't fail in the online phase, or changed since it was read", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn abandon_merge<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, merge_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let result = async {
        let record = authorize_merge(&state, &principal, ep::ABANDON_MERGE.policy.access, id, merge_id).await?;
        let merge = load_merge(&state, &record, merge_id).await?;
        if merge.state().after(MergeAction::Abandon).is_none() {
            return Err(conflict(
                ErrorCode::Conflict,
                format!(
                    "only a merge that failed in the online phase can be abandoned; this one is {}",
                    merge.status.as_str()
                ),
            ));
        }
        let plan: chelabase_schema::plan::Plan = match &merge.plan {
            Some(p) => serde_json::from_value(json!({ "steps": p["steps"] })).map_err(|e| internal("read stored plan", e))?,
            None => chelabase_schema::plan::Plan { steps: Vec::new() },
        };
        let events = state.metadata.merge_events(merge_id).await.map_err(|e| internal("merge events", e))?;
        let mut result = merge.result.clone().unwrap_or_else(|| json!({}));
        let left = chelabase_schema::plan::leftovers(&plan, &online_done(&events));
        result["leftovers"] = serde_json::to_value(left).map_err(|e| internal("encode leftovers", e))?;
        let patch = MergeStatusPatch {
            result: Some(result),
            // `cancelled` has no phase; `result.leftovers` says it was given up half applied.
            failed_phase: Some(None),
            guard_failed_phase: Some(merge.failed_phase.clone()),
            log_status: true,
            ..Default::default()
        };
        let moved = state
            .metadata
            .set_merge_status(merge_id, &[MergeStatus::Failed], MergeStatus::Cancelled, patch)
            .await
            .map_err(|e| internal("abandon merge", e))?;
        if !moved {
            return Err(conflict(ErrorCode::Conflict, "the merge changed; read it again"));
        }
        let merge = load_merge(&state, &record, merge_id).await?;
        info!(request_id = %principal.request_id, %merge_id, "merge abandoned");
        Ok(Json(Merge::from(merge)).into_response())
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/merges/{merge_id}/finish",
    operation_id = "finishMerge",
    tag = "merges",
    summary = "Run the after-merge choice of a merge applied with drift",
    description = "A merge whose target doesn't read back as merged ends `applied_with_drift` without running its after-merge choice (`result.after.skipped`). Once you've looked at the drift, this runs it: `keep` (the merge becomes the branch's base, if the source is still the schema that was merged; otherwise `result.after.base` is `not_advanced`), `reset` or `delete` of the source branch, with their usual refusals recorded in `result.after`. Only once per merge; an update from the parent only keeps. The merge stays `applied_with_drift`; `result.after` holds `requested: true` until the worker records the outcome.",
    params(("id" = Uuid, Path, description = "Database id"), ("merge_id" = Uuid, Path, description = "Merge id")),
    request_body(content = FinishRequest, description = "The after-merge choice"),
    responses(
        (status = 202, description = "Accepted; the merge stays `applied_with_drift`", body = MergeSummary),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database or the merge doesn't exist, or belongs to another tenant", body = ErrorEnvelope),
        (status = 409, description = "`conflict`: the merge isn't `applied_with_drift`, its choice was already run or requested, or it's an update from the parent and the choice isn't `keep`", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: an unknown field, or `after` is missing or isn't `keep`, `reset` or `delete`", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`worker_unavailable`: the job queue can't be reached (nothing is recorded)", body = ErrorEnvelope),
    ),
)]
async fn finish_merge<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, merge_id)): ApiPath<(Uuid, Uuid)>,
    ApiBytes(body): ApiBytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let result = async {
        let record = authorize_merge(&state, &principal, ep::FINISH_MERGE.policy.access, id, merge_id).await?;
        let merge = load_merge(&state, &record, merge_id).await?;
        let request: FinishRequest = parse_body(&axum::body::Bytes::copy_from_slice(&body))?;
        if merge.state().after(MergeAction::Finish).is_none() {
            return Err(wrong_state("finish", merge.status));
        }
        // The worker skipped the choice (drift); anything else means it ran or was asked for.
        let previous = merge.result.clone().unwrap_or_else(|| json!({}));
        if !previous["after"]["skipped"].is_string() {
            return Err(conflict(
                ErrorCode::Conflict,
                "the merge's after-merge choice was already run or requested",
            ));
        }
        if merge.direction == MergeDirection::FromParent && request.after != AfterMerge::Keep {
            return Err(conflict(
                ErrorCode::Conflict,
                "an update from the parent only keeps the branch",
            ));
        }
        let mut requested = previous.clone();
        requested["after"] = json!({ "choice": request.after.as_str(), "requested": true });
        let moved = state
            .metadata
            .set_merge_status(
                merge_id,
                &[MergeStatus::AppliedWithDrift],
                MergeStatus::AppliedWithDrift,
                // T14 M8: only over the result read here (the choice still skipped), so of two
                // requests one wins.
                MergeStatusPatch { result: Some(requested.clone()), guard_result: Some(merge.result.clone()), ..Default::default() },
            )
            .await
            .map_err(|e| internal("request the after-merge choice", e))?;
        if !moved {
            return Err(conflict(ErrorCode::Conflict, "the merge's after-merge choice was already run or requested"));
        }
        if !publish(&state, MergeJob::Finish { merge_id, after: request.after.job() }).await {
            let back = MergeStatusPatch { result: Some(previous), guard_result: Some(Some(requested)), ..Default::default() };
            revert(&state, merge_id, &[MergeStatus::AppliedWithDrift], MergeStatus::AppliedWithDrift, back).await;
            return Err(merge_enqueue_failed());
        }
        info!(request_id = %principal.request_id, %merge_id, after = request.after.as_str(), "after-merge choice requested");
        Ok(accepted(&merge, MergeStatus::AppliedWithDrift))
    }
    .await;
    result.unwrap_or_else(IntoResponse::into_response)
}

// ---- export -------------------------------------------------------------------------------

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/merges/{merge_id}/plan.sql",
    operation_id = "exportMergePlan",
    tag = "merges",
    summary = "Export a merge's plan as SQL",
    description = "The final plan as a `.sql` file: phase headers as comments, the plan's transaction in `BEGIN`/`COMMIT` starting with `SET LOCAL lock_timeout` and `statement_timeout` from the merge's options (its stale guard and recovery marker are left out), the online phase marked \"run outside a transaction\", roles as comments (never passwords). Never cached.",
    params(("id" = Uuid, Path, description = "Database id"), ("merge_id" = Uuid, Path, description = "Merge id")),
    responses(
        (status = 200, description = "The plan as SQL, as an attachment named `merge-<id>.sql`", content_type = "text/plain", body = String),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database or the merge doesn't exist, or belongs to another tenant", body = ErrorEnvelope),
        (status = 409, description = "`conflict`: the merge has no plan yet", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs, or the stored plan can't be read", body = ErrorEnvelope),
    ),
)]
async fn export_merge_plan<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, merge_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let result = async {
        let record = authorize_database(&state, &principal, ep::EXPORT_MERGE_PLAN.policy.access, Target::Database(id)).await?;
        let merge = load_merge(&state, &record, merge_id).await?;
        let Some(plan) = merge.plan else {
            return Err(conflict(ErrorCode::Conflict, "the merge has no plan yet"));
        };
        let plan: chelabase_schema::plan::Plan =
            serde_json::from_value(plan).map_err(|e| internal("read stored plan", e))?;
        // E35: the phase 1 block carries the merge's own lock_timeout.
        let opts = chelabase_schema::plan::PlanOptions {
            lock_timeout_ms: chelabase_schema::plan::lock_timeout_ms(&merge.options),
            merge_id,
        };
        Ok(chelabase_schema::export::export_sql(&plan, &opts))
    }
    .await;
    match result {
        Ok(sql) => (
            [
                (CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8")),
                (
                    CONTENT_DISPOSITION,
                    HeaderValue::from_str(&format!("attachment; filename=\"merge-{merge_id}.sql\""))
                        .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
                ),
                (CACHE_CONTROL, HeaderValue::from_static("no-store")),
            ],
            sql,
        )
            .into_response(),
        Err(err) => err.into_response(),
    }
}
