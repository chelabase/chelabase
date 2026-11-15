//! s3-endpoint m2: a branch's S3 access keys (list, create, revoke) under
//! `/v1/databases/{id}/branches/{branch_id}/storage/s3-keys`.
//!
//! A key acts as one role on one Postgres database of the branch, so creating one follows the
//! secret data keys' rules ([`crate::data_keys`]): never a personal login (MH-12); Developer for
//! a role the member may use, Admin for any ([`authorize_key_role`]); a ready, non-backup
//! branch; never an owner-level role (storage refuses every request of one, so such a key is
//! useless; refused with 422 even for an Admin); a database of the branch; and the worker's
//! check on the compute, whose role OID and timeline the key is bound to. Without that
//! identity there is no key: never one bound by name only.
//!
//! The secret is shown once, in the create response (`Cache-Control: no-store`), and never
//! logged. Revoking a key tells storage at once over NATS (`StorageEvent::Key`), so its cache
//! drops the key before its lifetime ends. With the endpoint off (`CHELABASE_S3_HOST` unset)
//! the list says so and creating is refused; revoking still works.

use std::sync::Arc;

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chelabase_metadata::{
    BranchKind, BranchRecord, BranchStatus, DatabaseOrigin, MetadataError, NewS3Key, RoleLevel,
    S3KeyRecord,
};
use chelabase_nats::StorageEvent;
use chelabase_routes::orchestrator as ep;
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use chelabase_types::{ApiPath, BranchId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{error, info};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::access::{
    Target, authorize, authorize_database, authorize_key_role, refuse_personal_login_key,
};
use crate::branches::{conflict, load_branch, parse_body, unprocessable};
use crate::data_api_databases::validate_key_databases;
use crate::data_keys::{DEFAULT_ROLE, no_store};
use crate::storage_console::tell_storage;
use crate::{ApiBytes, AppState, AuthPrincipal, BACKUP_REFUSAL, JobPort, MetadataPort, ResetError};

/// Longest key name.
const MAX_NAME_LEN: usize = 63;
/// The region S3 clients sign for: the endpoint has one.
pub(crate) const S3_REGION: &str = "us-east-1";

pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new()
        .routes(routes!(list_s3_keys, create_s3_key))
        .routes(routes!(revoke_s3_key))
}

/// An S3 access key as listed: never its secret.
#[derive(Debug, Serialize, ToSchema)]
struct S3Key {
    id: Uuid,
    name: String,
    /// The key's public half (`CBAK…`, 20 characters): the S3 client's access key id.
    access_key_id: String,
    /// The Postgres database on the branch the key reaches.
    database: String,
    /// The role the key acts as.
    role: String,
    created_at: DateTime<Utc>,
    /// When the key last signed a request (stamped at most once a minute); null if never.
    #[schema(required = true)]
    last_used_at: Option<DateTime<Utc>>,
}

impl From<S3KeyRecord> for S3Key {
    fn from(r: S3KeyRecord) -> Self {
        S3Key {
            id: r.id,
            name: r.name,
            access_key_id: r.access_key_id,
            database: r.database,
            role: r.role,
            created_at: r.created_at,
            last_used_at: r.last_used_at,
        }
    }
}

/// The branch's S3 access keys and where clients reach the endpoint.
#[derive(Debug, Serialize, ToSchema)]
struct S3KeyList {
    /// Whether the S3 endpoint is on (`CHELABASE_S3_HOST` is set). When off, keys can't be
    /// created; existing ones are listed and can be revoked.
    enabled: bool,
    /// The endpoint's URL (`{scheme}://{CHELABASE_S3_HOST}`); null when the endpoint is off.
    #[schema(required = true)]
    endpoint: Option<String>,
    /// The region S3 clients sign for: always `us-east-1`.
    region: String,
    /// The branch's live keys, newest first.
    keys: Vec<S3Key>,
}

/// A key just made, with its secret: shown only here.
#[derive(Serialize, ToSchema)]
struct CreatedS3Key {
    id: Uuid,
    name: String,
    access_key_id: String,
    /// The secret access key (40 characters). It is never shown again.
    secret_access_key: String,
    database: String,
    role: String,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateS3KeyRequest {
    /// 1 to 63 characters.
    name: String,
    /// The Postgres database on the branch the key reaches; absent is the branch's main
    /// database. The system `postgres` database can't be named.
    database: Option<String>,
    /// The role the key acts as; absent is `authenticated`.
    role: Option<String>,
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/storage/s3-keys",
    operation_id = "listS3Keys",
    tag = "storage",
    summary = "List a branch's S3 access keys",
    params(("id" = Uuid, Path, description = "Database id"), ("branch_id" = Uuid, Path, description = "Branch id")),
    responses(
        (status = 200, description = "Whether the S3 endpoint is on, its URL and region, and the branch's live keys, newest first (never their secrets)", body = S3KeyList),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: you aren't a Viewer on the database", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_s3_keys<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let record = match authorize_database(
        &state,
        &principal,
        ep::LIST_S3_KEYS.policy.access,
        Target::Branch {
            database: id,
            branch: branch_id,
        },
    )
    .await
    {
        Ok(record) => record,
        Err(resp) => return resp.into_response(),
    };
    let branch = match load_branch(&state, &record, branch_id).await {
        Ok(branch) => branch,
        Err(resp) => return resp.into_response(),
    };
    let keys = match state.metadata.list_live_s3_keys(branch.id).await {
        Ok(keys) => keys,
        Err(err) => {
            error!(branch_id = %branch.id, "list S3 keys: {err}");
            return ApiError::internal().into_response();
        }
    };
    no_store(
        Json(S3KeyList {
            enabled: state.s3_endpoint.is_some(),
            endpoint: state.s3_endpoint.clone(),
            region: S3_REGION.to_string(),
            keys: keys.into_iter().map(S3Key::from).collect(),
        })
        .into_response(),
    )
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/branches/{branch_id}/storage/s3-keys",
    operation_id = "createS3Key",
    tag = "storage",
    summary = "Create an S3 access key",
    description = "A key for the S3 endpoint, acting as one role on one Postgres database of the branch. Like a secret data key: Developer for a role you may use (in your roles, or reachable through a granted role), Admin for any role; never a personal login. An owner-level role is refused even for an Admin (storage would refuse every request). The key is bound to the role's identity (its OID on the branch's timeline) as the worker reads it on the compute. The secret is in this response only (`Cache-Control: no-store`).",
    params(("id" = Uuid, Path, description = "Database id"), ("branch_id" = Uuid, Path, description = "Branch id")),
    request_body = CreateS3KeyRequest,
    responses(
        (status = 201, description = "The new key, with its secret access key; the secret is shown only here", body = CreatedS3Key),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: you aren't a Developer on the database; `required` is `role:<name>` when the key's role isn't one you may use (or is a personal login), or `admin` for a LOGIN or owner-level role", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`s3_disabled`: the S3 endpoint is off (`CHELABASE_S3_HOST` unset); `backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready; `conflict`: the branch already has 100 live S3 keys", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the body is valid JSON but the wrong shape, the name or role is invalid, or the database isn't one of the branch's (or the branch's main database is gone); `role_refused`: the role is owner-level, or the worker refused it (it doesn't exist or may not back a key; the message says why)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`worker_unavailable`: the worker can't be reached; `unavailable`: the role's identity couldn't be read on the compute, or the branch's roles are being changed (retry after `Retry-After`); nothing was created", body = ErrorEnvelope, headers(("Retry-After" = u32, description = "Seconds to wait before retrying"))),
        (status = 504, description = "`timeout`: the worker didn't answer in time", body = ErrorEnvelope),
    ),
)]
async fn create_s3_key<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
    ApiBytes(body): ApiBytes,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let authorized = match authorize(
        &state,
        &principal,
        ep::CREATE_S3_KEY.policy.access,
        Target::Branch {
            database: id,
            branch: branch_id,
        },
    )
    .await
    {
        Ok(authorized) => authorized,
        Err(resp) => return resp.into_response(),
    };
    let Ok(record) = authorized.clone().into_record() else {
        return ApiError::internal().into_response();
    };
    let branch = match load_branch(&state, &record, branch_id).await {
        Ok(branch) => branch,
        Err(resp) => return resp.into_response(),
    };
    if state.s3_endpoint.is_none() {
        return conflict(
            ErrorCode::S3Disabled,
            "the S3 endpoint is off on this instance (CHELABASE_S3_HOST is unset)",
        )
        .into_response();
    }
    let request: CreateS3KeyRequest = match parse_body(&body) {
        Ok(request) => request,
        Err(resp) => return resp.into_response(),
    };
    if request.name.is_empty() || request.name.chars().count() > MAX_NAME_LEN {
        return unprocessable(&format!("name must be 1-{MAX_NAME_LEN} characters")).into_response();
    }
    let role = request.role.unwrap_or_else(|| DEFAULT_ROLE.to_string());
    if role.is_empty() {
        return unprocessable("role must not be empty").into_response();
    }
    if request.database.as_deref() == Some("") {
        return unprocessable("database must not be empty (omit it for the main database)")
            .into_response();
    }
    // The role's checks first, as for a secret data key: a personal login, then the caller's
    // right to key the role.
    if let Err(err) = refuse_personal_login_key(&state, &record, &role).await {
        return err.into_response();
    }
    if let Err(err) = authorize_key_role(&state, &authorized, branch.id.as_uuid(), &role).await {
        return err.into_response();
    }
    if branch.kind == BranchKind::Backup {
        return conflict(ErrorCode::BackupBranch, BACKUP_REFUSAL).into_response();
    }
    if branch.kind != BranchKind::Normal || branch.status != BranchStatus::Ready {
        return conflict(ErrorCode::BranchNotReady, "branch is not ready").into_response();
    }
    if let Err(err) = refuse_owner_level(&state, &branch, &role).await {
        return err.into_response();
    }
    let database = match key_database(&state, branch.id, request.database).await {
        Ok(database) => database,
        Err(err) => return err.into_response(),
    };
    // Bound to the role's OID on the timeline the check read it on: a role dropped or
    // recreated after the check, or a timeline switch, leaves a dead key, never one of another
    // role.
    let (role_oid, role_timeline) = match role_identity(&state, branch.id, &role).await {
        Ok(identity) => identity,
        Err(err) => return err.into_response(),
    };
    let pair = chelabase_types::s3_key::generate();
    let key_id = Uuid::new_v4();
    let new = NewS3Key {
        id: key_id,
        branch_id: branch.id,
        database: &database,
        role: &role,
        name: &request.name,
        access_key_id: &pair.access_key_id,
        secret: &pair.secret,
        role_oid,
        role_timeline: &role_timeline,
    };
    if let Err(err) = state.metadata.create_s3_key(&new).await {
        return write_error(&state, branch.id, err).await.into_response();
    }
    info!(
        request_id = %principal.request_id,
        branch_id = %branch.id,
        key_id = %key_id,
        access_key_id = %pair.access_key_id,
        "S3 key created"
    );
    no_store(
        (
            StatusCode::CREATED,
            Json(CreatedS3Key {
                id: key_id,
                name: request.name,
                access_key_id: pair.access_key_id.clone(),
                secret_access_key: pair.secret.clone(),
                database,
                role,
                created_at: Utc::now(),
            }),
        )
            .into_response(),
    )
}

#[utoipa::path(
    delete,
    path = "/v1/databases/{id}/branches/{branch_id}/storage/s3-keys/{key_id}",
    operation_id = "revokeS3Key",
    tag = "storage",
    summary = "Revoke an S3 access key",
    description = "The key stops working at once: its secret is erased and storage is told to drop it from its cache. Developer for a key whose role you may use, Admin for any key. Works with the S3 endpoint off.",
    params(("id" = Uuid, Path, description = "Database id"), ("branch_id" = Uuid, Path, description = "Branch id"), ("key_id" = Uuid, Path, description = "S3 key id")),
    responses(
        (status = 204, description = "Revoked"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: you aren't a Developer on the database; `required` is `role:<name>` when the key's role isn't one you may use, or `admin` for a LOGIN or owner-level role", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the key doesn't exist on this branch, or the key is already revoked", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn revoke_s3_key<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, key_id)): ApiPath<(Uuid, Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let authorized = match authorize(
        &state,
        &principal,
        ep::REVOKE_S3_KEY.policy.access,
        Target::Branch {
            database: id,
            branch: branch_id,
        },
    )
    .await
    {
        Ok(authorized) => authorized,
        Err(resp) => return resp.into_response(),
    };
    let Ok(record) = authorized.clone().into_record() else {
        return ApiError::internal().into_response();
    };
    let branch = match load_branch(&state, &record, branch_id).await {
        Ok(branch) => branch,
        Err(resp) => return resp.into_response(),
    };
    // Only this branch's live keys: another branch's (or database's) id is not found.
    let key = match state.metadata.list_live_s3_keys(branch.id).await {
        Ok(keys) => keys.into_iter().find(|k| k.id == key_id),
        Err(err) => {
            error!(branch_id = %branch.id, "list S3 keys: {err}");
            return ApiError::internal().into_response();
        }
    };
    let Some(key) = key else {
        return ApiError::not_found("S3 key not found").into_response();
    };
    if let Err(err) = authorize_key_role(&state, &authorized, branch.id.as_uuid(), &key.role).await
    {
        return err.into_response();
    }
    match state.metadata.revoke_s3_key(branch.id, key.id).await {
        Ok(true) => {
            info!(request_id = %principal.request_id, branch_id = %branch.id, key_id = %key.id, "S3 key revoked");
            // Storage drops the key from its cache now, not when the cache entry ends.
            tell_storage(&state, StorageEvent::Key { key_id: key.id }).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => ApiError::not_found("S3 key not found").into_response(),
        Err(err) => {
            error!(branch_id = %branch.id, key_id = %key.id, "revoke S3 key: {err}");
            ApiError::internal().into_response()
        }
    }
}

/// 422 `role_refused` for a role the metadata knows as owner-level (superuser, CREATEROLE,
/// CREATEDB, REPLICATION or a `neon_superuser` member), whoever asks: storage refuses every
/// request of such a role. The worker's check refuses it too, on the compute.
async fn refuse_owner_level<M, J>(
    state: &AppState<M, J>,
    branch: &BranchRecord,
    role: &str,
) -> Result<(), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let Some(endpoint) = branch.endpoint_id.as_deref() else {
        return Err(conflict(ErrorCode::BranchNotReady, "branch is not ready"));
    };
    let rows = state.metadata.list_roles(endpoint).await.map_err(|err| {
        error!(branch_id = %branch.id, "list roles for an S3 key: {err}");
        ApiError::internal()
    })?;
    if rows
        .iter()
        .any(|r| r.name == role && r.level == RoleLevel::OwnerLevel)
    {
        return Err(ApiError::new(
            422,
            ErrorCode::RoleRefused,
            format!(
                "{role:?} is an owner-level role; storage refuses such roles, so an S3 key can't use it"
            ),
        ));
    }
    Ok(())
}

/// The Postgres database the key reaches: the one named (a database of the branch, never the
/// system one), or the branch's main database.
async fn key_database<M, J>(
    state: &AppState<M, J>,
    branch: BranchId,
    named: Option<String>,
) -> Result<String, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    if let Some(name) = named {
        validate_key_databases(state, branch, std::slice::from_ref(&name)).await?;
        return Ok(name);
    }
    let rows = state
        .metadata
        .branch_databases(branch)
        .await
        .map_err(|err| {
            error!(branch_id = %branch, "list branch databases: {err}");
            ApiError::internal()
        })?;
    rows.into_iter()
        .find(|d| d.origin == DatabaseOrigin::Main)
        .map(|d| d.name)
        .ok_or_else(|| unprocessable("the branch has no main database; name the database"))
}

/// The worker's check of `role` on the branch's compute (as for a secret data key), giving the
/// role's `(pg_roles.oid, timeline)`. A refusal (a missing role, an owner-level one) is 422;
/// a compute that can't be reached, or an answer without the identity, 503: there is no key
/// without it.
async fn role_identity<M, J>(
    state: &AppState<M, J>,
    branch: BranchId,
    role: &str,
) -> Result<(i64, String), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let unavailable = |why: &str| {
        ApiError::new(
            503,
            ErrorCode::Unavailable,
            format!("{why}; nothing was created, try again"),
        )
        .retry_after(1)
    };
    match state.js.check_data_role(branch, role).await {
        Ok(reply) if reply.ok => match (reply.role_oid, reply.role_timeline) {
            (Some(oid), Some(timeline)) => Ok((oid, timeline)),
            _ => {
                error!(branch_id = %branch, "the worker's role check gave no role identity");
                Err(unavailable(
                    "the role's identity couldn't be read on the compute",
                ))
            }
        },
        Ok(reply) => match (reply.kind.as_deref(), reply.error) {
            (Some("refused"), Some(message)) => {
                Err(ApiError::new(422, ErrorCode::RoleRefused, message))
            }
            (Some("not_ready"), _) => {
                Err(conflict(ErrorCode::BranchNotReady, "branch is not ready"))
            }
            (kind, message) => {
                error!(branch_id = %branch, ?kind, "check role for an S3 key failed: {message:?}");
                Err(unavailable("the role couldn't be checked on the compute"))
            }
        },
        Err(ResetError::Timeout) => Err(ApiError::new(
            504,
            ErrorCode::Timeout,
            "worker did not respond; try again",
        )),
        Err(ResetError::Transport(err)) => {
            error!(branch_id = %branch, "check role for an S3 key: {err}");
            Err(ApiError::new(
                503,
                ErrorCode::WorkerUnavailable,
                "worker unavailable",
            ))
        }
    }
}

/// A failed store: the cap is 409, a busy endpoint 503, and a branch that stopped being usable
/// since it was read (the store refuses it) 409.
async fn write_error<M, J>(state: &AppState<M, J>, branch: BranchId, err: MetadataError) -> ApiError
where
    M: MetadataPort,
    J: JobPort,
{
    match err {
        MetadataError::Conflict => conflict(
            ErrorCode::Conflict,
            format!(
                "the branch already has {} live S3 keys; revoke one first",
                chelabase_metadata::STORAGE_S3_KEY_CAP
            ),
        ),
        MetadataError::EndpointLockTimeout(_) => ApiError::new(
            503,
            ErrorCode::Unavailable,
            "the branch's roles are being changed; try again",
        )
        .retry_after(1),
        err => {
            let usable = matches!(
                state.metadata.get_branch(branch).await,
                Ok(Some(b)) if b.kind == BranchKind::Normal && b.status == BranchStatus::Ready
            );
            if !usable {
                return conflict(ErrorCode::BranchNotReady, "branch is not ready");
            }
            error!(branch_id = %branch, "create S3 key: {err}");
            ApiError::internal()
        }
    }
}
