//! Protected branches: the login-role guard's exemptions, and the review of code owned by
//! non-Admin roles that can run in someone else's session (read, acknowledge, fix).
//!
//! The review is scanned live, in every live database of the branch, by the worker as `cloud_admin`. Protecting
//! (`PATCH` on the branch), acknowledging and fixing all carry the hash of the review the
//! caller saw, and are refused with 409 `review_changed` (with the current review) when it
//! differs. Every change to the switch or the exemptions reconciles the branch's member roles,
//! which (re)installs the guard with its settings.

use std::collections::HashSet;
use std::sync::Arc;

use axum::{
    Json,
    extract::State,
    response::{IntoResponse, Response},
};
use chelabase_metadata::{BranchKind, BranchRecord, BranchStatus, DatabaseRecord};
use chelabase_nats::{ReviewFixOrder, ReviewReply, ReviewRequest};
use chelabase_routes::orchestrator as ep;
use chelabase_schema::review::review_hash;
use chelabase_types::access::Access;
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use chelabase_types::protection::{
    AcknowledgeReviewRequest, GuardExemptions, Review, ReviewFixRequest, ReviewFixResponse,
    ReviewItem,
};
use chelabase_types::{ApiJson, ApiPath, BranchId};
use serde_json::json;
use tracing::{error, info, warn};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::access::{Target, authorize_database};
use crate::branches::{conflict, load_branch, unprocessable};
use crate::console::unavailable;
use crate::{AppState, AuthPrincipal, BACKUP_REFUSAL, JobPort, MetadataPort, ResetError};

/// Longest exemption name, in bytes (a Postgres identifier).
const MAX_ROLE_BYTES: usize = 63;

pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new()
        .routes(routes!(list_guard_exemptions))
        .routes(routes!(put_guard_exemption, delete_guard_exemption))
        .routes(routes!(get_branch_review))
        .routes(routes!(acknowledge_branch_review))
        .routes(routes!(fix_branch_review_item))
}

/// Asks the member-role reconciler to bring the branch's guard (and member roles) up to date.
/// A failed enqueue is logged: the periodic sweep reconciles the branch anyway.
pub(crate) async fn reconcile_guard<M, J>(state: &AppState<M, J>, branch: BranchId)
where
    M: MetadataPort,
    J: JobPort,
{
    if let Err(err) = state.js.enqueue_member_roles(branch).await {
        warn!(branch_id = %branch, "member roles: enqueue failed; the sweep will reconcile it: {err}");
    }
}

/// 409 `review_changed`, carrying the current review as `error.review` (P8).
pub(crate) fn review_changed(review: &Review) -> ApiError {
    ApiError::new(
        409,
        ErrorCode::ReviewChanged,
        "the branch's review changed since the hash the request carries; check `error.review` and send its `hash`",
    )
    .with_extra("review", serde_json::to_value(review).unwrap_or_default())
}

/// Whether `hash` (a protect's `review_hash`) confirms `review`: it must be the current hash;
/// without one, only an empty review is confirmed.
pub(crate) fn confirms(hash: Option<&str>, review: &Review) -> bool {
    match hash {
        Some(hash) => hash == review.hash,
        None => review.items.is_empty(),
    }
}

/// The review of `items` for `branch`: their hash, the hash last acknowledged, and each item
/// `new` when a list was acknowledged and didn't hold it.
fn review_of(mut items: Vec<ReviewItem>, branch: &BranchRecord) -> Review {
    items.sort_by(|a, b| a.key.cmp(&b.key));
    let acknowledged: Option<HashSet<&str>> = branch
        .review_ack_hash
        .as_ref()
        .map(|_| branch.review_ack_keys.iter().map(String::as_str).collect());
    for item in &mut items {
        item.new = acknowledged
            .as_ref()
            .is_some_and(|keys| !keys.contains(item.key.as_str()));
    }
    Review {
        hash: review_hash(&items),
        items,
        acknowledged_hash: branch.review_ack_hash.clone(),
    }
}

/// The refusals of a review on `branch` before the worker is asked: a backup has no compute,
/// a branch that isn't ready has none to scan.
fn check_reviewable(branch: &BranchRecord) -> Result<(), ApiError> {
    if branch.kind == BranchKind::Backup {
        return Err(conflict(ErrorCode::BackupBranch, BACKUP_REFUSAL));
    }
    if branch.status != BranchStatus::Ready {
        return Err(conflict(ErrorCode::BranchNotReady, "branch is not ready"));
    }
    Ok(())
}

/// The worker's answer to a review request; a transport failure as other worker RPCs answer it.
async fn ask_worker<M, J>(
    state: &AppState<M, J>,
    branch: &BranchRecord,
    fix: Option<ReviewFixOrder>,
) -> Result<ReviewReply, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    check_reviewable(branch)?;
    let request = ReviewRequest {
        branch_id: branch.id.as_uuid(),
        fix,
    };
    match state.js.review(request).await {
        Ok(ReviewReply::Failed { message }) => {
            warn!(branch_id = %branch.id, "the review failed: {message}");
            Err(unavailable(&format!("the review couldn't run: {message}")))
        }
        Ok(reply) => Ok(reply),
        Err(ResetError::Timeout) => Err(ApiError::new(
            504,
            ErrorCode::Timeout,
            "the worker did not answer in time; try again",
        )),
        Err(ResetError::Transport(err)) => {
            error!(branch_id = %branch.id, "review: {err}");
            Err(ApiError::new(
                503,
                ErrorCode::WorkerUnavailable,
                "worker unavailable",
            ))
        }
    }
}

/// The branch's current review: the worker scans every live database of the branch as
/// `cloud_admin` (a failed scan is 503 `unavailable`, never a partial list).
pub(crate) async fn current_review<M, J>(
    state: &AppState<M, J>,
    branch: &BranchRecord,
) -> Result<Review, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    match ask_worker(state, branch, None).await? {
        ReviewReply::Scanned { items } => Ok(review_of(items, branch)),
        other => {
            error!(branch_id = %branch.id, "a review scan answered {other:?}");
            Err(ApiError::internal())
        }
    }
}

/// A fix the worker refused: by SQLSTATE (2BP01 `has_dependents`, 55P03 `lock_timeout`, else
/// 400 `schema_change_failed`), with the SQLSTATE and Postgres's message.
fn fix_failed(sqlstate: &str, message: &str) -> ApiError {
    let (status, code) = match sqlstate {
        "2BP01" => (409, ErrorCode::HasDependents),
        "55P03" => (409, ErrorCode::LockTimeout),
        _ => (400, ErrorCode::SchemaChangeFailed),
    };
    ApiError::new(status, code, message)
        .with_extra("sqlstate", json!(sqlstate))
        .with_extra("message", json!(message))
}

/// The database and the branch, after the access check.
async fn authorized_branch<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    access: Access,
    id: Uuid,
    branch_id: Uuid,
) -> Result<(DatabaseRecord, BranchRecord), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let record = authorize_database(
        state,
        principal,
        access,
        Target::Branch {
            database: id,
            branch: branch_id,
        },
    )
    .await?;
    let branch = load_branch(state, &record, branch_id).await?;
    Ok((record, branch))
}

/// The refusals of a settings write: a backup has none, and a deleting branch takes none.
fn check_settable(branch: &BranchRecord) -> Result<(), ApiError> {
    if branch.kind == BranchKind::Backup {
        return Err(conflict(ErrorCode::BackupBranch, BACKUP_REFUSAL));
    }
    if branch.status == BranchStatus::Deleting {
        return Err(conflict(
            ErrorCode::BranchNotReady,
            "branch is being deleted",
        ));
    }
    Ok(())
}

/// P10: an exemption names 1-63 bytes without a NUL (the role need not exist).
fn check_exemption_name(role: &str) -> Result<(), ApiError> {
    if role.is_empty() || role.len() > MAX_ROLE_BYTES || role.contains('\0') {
        return Err(unprocessable(
            "a guard exemption names a role of 1-63 bytes without a NUL byte",
        ));
    }
    Ok(())
}

/// The branch's exemptions as the routes answer them.
async fn exemptions<M, J>(
    state: &AppState<M, J>,
    branch: BranchId,
) -> Result<GuardExemptions, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    match state.metadata.list_guard_exemptions(branch).await {
        Ok(exemptions) => Ok(GuardExemptions { exemptions }),
        Err(err) => {
            error!(branch_id = %branch, "list guard exemptions: {err}");
            Err(ApiError::internal())
        }
    }
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

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/guard-exemptions",
    operation_id = "listGuardExemptions",
    tag = "branches",
    summary = "List a branch's guard exemptions",
    description = "The login roles, by name, that the branch's \"guard login roles\" switch leaves alone: on a protected branch with `guard_logins` on, they may still change the schema. Sorted by name (byte order). A new branch copies its parent's.",
    params(("id" = Uuid, Path, description = "Database id"), ("branch_id" = Uuid, Path, description = "Branch id")),
    responses(
        (status = 200, description = "The exemptions", body = GuardExemptions),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_guard_exemptions<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let (_, branch) = or_answer!(
        authorized_branch(
            &state,
            &principal,
            ep::LIST_GUARD_EXEMPTIONS.policy.access,
            id,
            branch_id
        )
        .await
    );
    Json(or_answer!(exemptions(&state, branch.id).await)).into_response()
}

#[utoipa::path(
    put,
    path = "/v1/databases/{id}/branches/{branch_id}/guard-exemptions/{role}",
    operation_id = "putGuardExemption",
    tag = "branches",
    summary = "Exempt a login role from a branch's guard",
    description = "Adds `role`, by name, to the branch's guard exemptions (idempotent). The role need not exist: a role dropped and made again in SQL keeps its exemption, a renamed one loses it. Reconciles the branch's guard.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("role" = String, Path, description = "The role's name"),
    ),
    responses(
        (status = 200, description = "The exemptions after the change", body = GuardExemptions),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`backup_branch`: a restore backup has no guard; `branch_not_ready`: the branch is being deleted", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: `role` is empty, over 63 bytes or has a NUL", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn put_guard_exemption<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, role)): ApiPath<(Uuid, Uuid, String)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    change_exemption(&state, &principal, (id, branch_id, &role), true).await
}

#[utoipa::path(
    delete,
    path = "/v1/databases/{id}/branches/{branch_id}/guard-exemptions/{role}",
    operation_id = "deleteGuardExemption",
    tag = "branches",
    summary = "Remove a login role's guard exemption",
    description = "Removes `role` from the branch's guard exemptions (idempotent). Reconciles the branch's guard.",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        ("role" = String, Path, description = "The role's name"),
    ),
    responses(
        (status = 200, description = "The exemptions after the change", body = GuardExemptions),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`backup_branch`: a restore backup has no guard; `branch_not_ready`: the branch is being deleted", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: `role` is empty, over 63 bytes or has a NUL", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn delete_guard_exemption<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, role)): ApiPath<(Uuid, Uuid, String)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    change_exemption(&state, &principal, (id, branch_id, &role), false).await
}

/// Adds (`add`) or removes the exemption, then reconciles the guard.
async fn change_exemption<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    (id, branch_id, role): (Uuid, Uuid, &str),
    add: bool,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    or_answer!(check_exemption_name(role));
    let access = if add {
        ep::PUT_GUARD_EXEMPTION.policy.access
    } else {
        ep::DELETE_GUARD_EXEMPTION.policy.access
    };
    let (_, branch) = or_answer!(authorized_branch(state, principal, access, id, branch_id).await);
    or_answer!(check_settable(&branch));
    let changed = if add {
        state.metadata.add_guard_exemption(branch.id, role).await
    } else {
        state.metadata.remove_guard_exemption(branch.id, role).await
    };
    let changed = match changed {
        Ok(changed) => changed,
        Err(err) => {
            error!(branch_id = %branch.id, "change a guard exemption: {err}");
            return ApiError::internal().into_response();
        }
    };
    info!(
        request_id = %principal.request_id,
        branch_id = %branch.id,
        %role,
        exempt = add,
        changed,
        "guard exemption changed"
    );
    reconcile_guard(state, branch.id).await;
    Json(or_answer!(exemptions(state, branch.id).await)).into_response()
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/review",
    operation_id = "getBranchReview",
    tag = "branches",
    summary = "Review the code that can run in someone else's session",
    description = "The worker scans every live database of the branch, as `cloud_admin`, for code owned by a non-Admin role that can run in someone else's session (an Admin's included): triggers whose function or table such a role owns, rules on its relations, and its functions that are SECURITY DEFINER or reached from a policy, a default or generated column, a check, an index, a view, a trigger, a rule, a range type or another such function. Each item has a stable `key`, its `reasons`, and `new` when a list was acknowledged that didn't hold it. `hash` is what protecting (`review_hash`), acknowledging and fixing confirm. Needs the branch's compute running.",
    params(("id" = Uuid, Path, description = "Database id"), ("branch_id" = Uuid, Path, description = "Branch id")),
    responses(
        (status = 200, description = "The review", body = Review),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: the scan couldn't run (the compute isn't running or can't be reached; the message says why; `Retry-After`); `worker_unavailable`: the worker can't be reached", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the worker didn't answer in time", body = ErrorEnvelope),
    ),
)]
async fn get_branch_review<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let (_, branch) = or_answer!(
        authorized_branch(
            &state,
            &principal,
            ep::GET_BRANCH_REVIEW.policy.access,
            id,
            branch_id
        )
        .await
    );
    Json(or_answer!(current_review(&state, &branch).await)).into_response()
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/branches/{branch_id}/review/acknowledge",
    operation_id = "acknowledgeBranchReview",
    tag = "branches",
    summary = "Acknowledge a branch's review",
    description = "Stores the current review as acknowledged: its `hash` becomes the branch's `review_acknowledged_hash`, and its items stop being `new`. `hash` must be the current review's.",
    params(("id" = Uuid, Path, description = "Database id"), ("branch_id" = Uuid, Path, description = "Branch id")),
    request_body = AcknowledgeReviewRequest,
    responses(
        (status = 200, description = "The review as acknowledged", body = Review),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`review_changed`: the review's hash isn't `hash` (`error.review`: the current review); `backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the body is valid JSON but not `{ \"hash\" }`", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: the scan couldn't run (the compute isn't running or can't be reached; the message says why; `Retry-After`); `worker_unavailable`: the worker can't be reached", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the worker didn't answer in time", body = ErrorEnvelope),
    ),
)]
async fn acknowledge_branch_review<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(request): ApiJson<AcknowledgeReviewRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let (_, branch) = or_answer!(
        authorized_branch(
            &state,
            &principal,
            ep::ACKNOWLEDGE_BRANCH_REVIEW.policy.access,
            id,
            branch_id
        )
        .await
    );
    let review = or_answer!(current_review(&state, &branch).await);
    if request.hash != review.hash {
        return review_changed(&review).into_response();
    }
    let keys: Vec<String> = review.items.iter().map(|i| i.key.clone()).collect();
    match state
        .metadata
        .acknowledge_review(branch.id, &review.hash, &keys)
        .await
    {
        Ok(Some(updated)) => {
            info!(request_id = %principal.request_id, branch_id = %branch.id, hash = %review.hash, "review acknowledged");
            Json(review_of(review.items, &updated)).into_response()
        }
        Ok(None) => conflict(ErrorCode::BranchNotReady, "branch is being deleted").into_response(),
        Err(err) => {
            error!(branch_id = %branch.id, "acknowledge the review: {err}");
            ApiError::internal().into_response()
        }
    }
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/branches/{branch_id}/review/fix",
    operation_id = "fixBranchReviewItem",
    tag = "branches",
    summary = "Fix one item of a branch's review",
    description = "The worker scans the branch again, checks `hash` against it, and runs one fix in the item's database in its own transaction with `lock_timeout` 5 s: `reassign` gives the item's function (a trigger's function; a rule's relation) to the owner (`ALTER … OWNER TO`), after which the Admin answers for its code and a SECURITY DEFINER function runs as the owner; `drop` drops the function, trigger or rule `RESTRICT` (never cascading). Answers with the SQL it ran and the review after it.",
    params(("id" = Uuid, Path, description = "Database id"), ("branch_id" = Uuid, Path, description = "Branch id")),
    request_body = ReviewFixRequest,
    responses(
        (status = 200, description = "The SQL that ran and the review after it", body = ReviewFixResponse),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON; `schema_change_failed`: Postgres refused the fix, with `sqlstate` and `message`", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist, or the review has no item `key`", body = ErrorEnvelope),
        (status = 409, description = "`review_changed`: the review's hash isn't `hash` (`error.review`: the current review); `has_dependents`: other objects depend on what a drop removes (2BP01; `message` names them); `lock_timeout`: a lock wasn't granted within 5 s (retry); `backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the body is valid JSON but not `{ \"key\", \"action\", \"hash\" }` (`action`: `reassign` or `drop`)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: the scan couldn't run (the compute isn't running or can't be reached; the message says why; `Retry-After`); `worker_unavailable`: the worker can't be reached", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the worker didn't answer in time", body = ErrorEnvelope),
    ),
)]
async fn fix_branch_review_item<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(request): ApiJson<ReviewFixRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let (_, branch) = or_answer!(
        authorized_branch(
            &state,
            &principal,
            ep::FIX_BRANCH_REVIEW_ITEM.policy.access,
            id,
            branch_id
        )
        .await
    );
    let order = ReviewFixOrder {
        key: request.key.clone(),
        action: request.action,
        hash: request.hash,
    };
    let outcome = match ask_worker(&state, &branch, Some(order)).await {
        Ok(ReviewReply::Fixed { sql, items }) => Ok(ReviewFixResponse {
            sql,
            review: review_of(items, &branch),
        }),
        Ok(ReviewReply::Changed { items }) => Err(review_changed(&review_of(items, &branch))),
        Ok(ReviewReply::NotFound { .. }) => Err(ApiError::not_found(format!(
            "the review has no item {}",
            request.key
        ))),
        Ok(ReviewReply::FixFailed {
            sqlstate, message, ..
        }) => Err(fix_failed(&sqlstate, &message)),
        Ok(other) => {
            error!(branch_id = %branch.id, "a review fix answered {other:?}");
            Err(ApiError::internal())
        }
        Err(err) => Err(err),
    };
    info!(
        request_id = %principal.request_id,
        user = %principal.sub,
        branch_id = %branch.id,
        key = %request.key,
        action = ?request.action,
        status = outcome.as_ref().map_or_else(|e| e.status, |_| 200),
        "review fix"
    );
    Json(or_answer!(outcome)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_types::protection::{ReviewItemKind, ReviewReason, ReviewReasonKind};

    fn item(key: &str) -> ReviewItem {
        ReviewItem {
            key: key.into(),
            database: "app".into(),
            kind: ReviewItemKind::Function,
            schema: "public".into(),
            name: key.into(),
            identity: String::new(),
            function: Some(format!("public.{key}()")),
            owner: "dev".into(),
            reasons: vec![ReviewReason {
                kind: ReviewReasonKind::SecurityDefiner,
                via: None,
            }],
            opaque_body: false,
            digest: String::new(),
            new: false,
        }
    }

    fn branch(ack: Option<(&str, &[&str])>) -> BranchRecord {
        let now = chrono::Utc::now();
        BranchRecord {
            id: BranchId::new(),
            database_id: chelabase_types::DatabaseId::new(),
            name: "dev".into(),
            parent_branch_id: None,
            kind: BranchKind::Normal,
            status: BranchStatus::Ready,
            timeline_id: None,
            pending_timeline_id: None,
            pending_timeline_at: None,
            superseded_timeline_id: None,
            endpoint_id: None,
            owner_password_enc: None,
            branched_at_time: None,
            branched_at_lsn: None,
            error: None,
            created_at: now,
            updated_at: now,
            history_start: None,
            authenticator_password_enc: None,
            protected: false,
            guard_logins: true,
            review_ack_hash: ack.map(|(h, _)| h.to_string()),
            review_ack_keys: ack
                .map(|(_, k)| k.iter().map(|s| s.to_string()).collect())
                .unwrap_or_default(),
        }
    }

    #[test]
    fn nothing_is_new_until_a_list_was_acknowledged() {
        let review = review_of(vec![item("b"), item("a")], &branch(None));
        assert_eq!(
            review
                .items
                .iter()
                .map(|i| (i.key.as_str(), i.new))
                .collect::<Vec<_>>(),
            [("a", false), ("b", false)]
        );
        assert_eq!(review.acknowledged_hash, None);
        assert_eq!(review.hash, review_hash(&review.items));
        // Acknowledged (even an empty list): what it didn't hold is new.
        let review = review_of(vec![item("a"), item("b")], &branch(Some(("h", &["a"]))));
        assert_eq!(
            review.items.iter().map(|i| i.new).collect::<Vec<_>>(),
            [false, true]
        );
        assert_eq!(review.acknowledged_hash.as_deref(), Some("h"));
        let review = review_of(vec![item("a")], &branch(Some(("h", &[]))));
        assert!(review.items[0].new);
    }

    #[test]
    fn a_protect_confirms_the_current_hash_and_an_empty_list_needs_none() {
        let empty = review_of(Vec::new(), &branch(None));
        let listed = review_of(vec![item("a")], &branch(None));
        assert!(confirms(None, &empty));
        assert!(confirms(Some(&empty.hash), &empty));
        assert!(!confirms(Some("0"), &empty));
        assert!(!confirms(None, &listed));
        assert!(!confirms(Some(&empty.hash), &listed));
        assert!(confirms(Some(&listed.hash), &listed));
    }

    #[test]
    fn exemption_names_are_one_to_63_bytes_without_nul() {
        assert!(check_exemption_name("deploy").is_ok());
        assert!(check_exemption_name("Ci Bot").is_ok());
        assert!(check_exemption_name(&"a".repeat(63)).is_ok());
        // 63 bytes in multibyte characters, and 64.
        assert!(check_exemption_name(&"é".repeat(31)).is_ok());
        assert!(check_exemption_name(&"é".repeat(32)).is_err());
        for bad in [String::new(), "a".repeat(64), "a\0b".into()] {
            assert_eq!(
                check_exemption_name(&bad).unwrap_err().status,
                422,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn review_changed_carries_the_review() {
        let review = review_of(vec![item("a")], &branch(None));
        let err = review_changed(&review);
        assert_eq!((err.status, err.code), (409, ErrorCode::ReviewChanged));
        let body = err.body();
        assert_eq!(body["error"]["review"]["hash"], review.hash.as_str());
    }
}
