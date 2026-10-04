//! The data API's management routes: a branch's keys (list, create, update, rotate, revoke) and its
//! schema as the data API sees it. Keys are shown once, except the public key, which is sealed
//! so Studio can show it again.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json,
    extract::State,
    http::{StatusCode, header, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
};
use chelabase_metadata::{
    BranchKind, BranchRecord, BranchStatus, DataKeyRecord, MetadataError, NewDataKey, RoleLevel,
};
use chelabase_secrets::{Purpose, open_credential, seal_credential};
use chelabase_types::api_error::{ApiError, ErrorCode};
use chelabase_types::{ApiPath, BranchId};
use chelabase_types::data_key::{self, DataKeyKind};
use chrono::{DateTime, Utc};
use chelabase_types::api_error::ErrorEnvelope;
use chelabase_types::data_api_schema::DataApiSchema;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::branches::{conflict, load_branch, parse_body, unprocessable};
use crate::data_api_databases::validate_key_databases;
use crate::{AppState, ApiBytes, BACKUP_REFUSAL, AuthPrincipal, JobPort, MetadataPort, ResetError, load_owned};

/// Longest key name.
const MAX_NAME_LEN: usize = 63;
/// The role a secret key gets when the request names none.
const DEFAULT_ROLE: &str = "authenticated";
/// Longest wait for the REST service's schema.
const REST_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new()
        .routes(routes!(list_keys, create_key))
        .routes(routes!(rotate_key))
        .routes(routes!(update_key, revoke_key))
        .routes(routes!(data_api_schema))
}

/// A key as listed: `key` only for the public key.
#[derive(Debug, Serialize, ToSchema)]
struct DataKey {
    id: Uuid,
    kind: DataKeyKind,
    role: String,
    name: String,
    prefix: String,
    created_at: DateTime<Utc>,
    #[schema(required = true)]
    last_used_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    /// A secret key whose role is owner-level: the data API refuses it. Always false for the
    /// public key.
    role_blocked: bool,
    /// The Postgres databases the key may reach; null is every served database.
    #[schema(required = true)]
    databases: Option<Vec<String>>,
}

/// A key just made, with its plaintext.
#[derive(Debug, Serialize, ToSchema)]
struct CreatedDataKey {
    id: Uuid,
    kind: DataKeyKind,
    role: String,
    name: String,
    prefix: String,
    key: String,
    /// The Postgres databases the key may reach; null is every served database.
    #[schema(required = true)]
    databases: Option<Vec<String>>,
}

pub(crate) fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

/// The branch's key row for the response, opening the public key's sealed value.
fn summary(
    record: DataKeyRecord,
    secrets: &chelabase_secrets::SecretKey,
    role_blocked: bool,
) -> Result<DataKey, ()> {
    let key = match (record.kind, record.sealed.as_deref()) {
        (DataKeyKind::Public, Some(sealed)) => Some(
            open_credential(secrets, &record.id.to_string(), Purpose::DataKey, sealed)
                .map_err(|err| error!(key_id = %record.id, "opening a public key: {err}"))?,
        ),
        _ => None,
    };
    Ok(DataKey {
        id: record.id,
        kind: record.kind,
        role: record.role,
        name: record.name,
        prefix: record.prefix,
        created_at: record.created_at,
        last_used_at: record.last_used_at,
        key,
        role_blocked,
        databases: record.databases,
    })
}

/// Builds the row and the plaintext of a new key. Public keys are sealed for display.
fn new_key(
    secrets: &chelabase_secrets::SecretKey,
    branch: BranchId,
    kind: DataKeyKind,
    role: &str,
    name: &str,
    databases: Option<Vec<String>>,
) -> (NewDataKey, String) {
    let generated = data_key::generate(kind);
    let id = Uuid::new_v4();
    let sealed = (kind == DataKeyKind::Public)
        .then(|| seal_credential(secrets, &id.to_string(), Purpose::DataKey, &generated.key));
    (
        NewDataKey {
            id,
            branch_id: branch,
            kind,
            role: role.to_string(),
            name: name.to_string(),
            prefix: generated.prefix,
            sha256: generated.sha256,
            sealed,
            databases,
        },
        generated.key,
    )
}

fn created(key: &NewDataKey, plaintext: String) -> Response {
    no_store(
        (
            StatusCode::CREATED,
            Json(CreatedDataKey {
                id: key.id,
                kind: key.kind,
                role: key.role.clone(),
                name: key.name.clone(),
                prefix: key.prefix.clone(),
                key: plaintext,
                databases: key.databases.clone(),
            }),
        )
            .into_response(),
    )
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/keys",
    operation_id = "listKeys",
    tag = "keys",
    summary = "List a branch's data API keys",
    params(("id" = Uuid, Path, description = "Database id"), ("branch_id" = Uuid, Path, description = "Branch id")),
    responses(
        (status = 200, description = "The branch's live keys, newest first; only the public key carries its full value", body = Vec<DataKey>),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_keys<M, J>(
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
    let keys = match state.metadata.list_data_keys(branch.id).await {
        Ok(keys) => keys,
        Err(err) => {
            error!("list data keys: {err}");
            return ApiError::internal().into_response();
        }
    };
    let owner_level = owner_level_roles(&state, &branch).await;
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        let blocked = key.kind == DataKeyKind::Secret && owner_level.contains(&key.role);
        match summary(key, &state.secrets, blocked) {
            Ok(s) => out.push(s),
            Err(()) => return ApiError::internal().into_response(),
        }
    }
    no_store(Json(out).into_response())
}

/// The branch's owner-level roles, the same check as `blocked_keys` in `listRoles`. A failed
/// lookup is logged and gives none: the flag is a hint for Studio, the keys themselves are fine.
async fn owner_level_roles<M, J>(state: &AppState<M, J>, branch: &BranchRecord) -> HashSet<String>
where
    M: MetadataPort,
    J: JobPort,
{
    match branch.endpoint_id.as_deref() {
        None => HashSet::new(),
        Some(endpoint) => match state.metadata.list_roles(endpoint).await {
            Ok(rows) => rows
                .into_iter()
                .filter(|r| r.level == RoleLevel::OwnerLevel)
                .map(|r| r.name)
                .collect(),
            Err(err) => {
                warn!(branch_id = %branch.id, "list roles for data keys failed: {err}");
                HashSet::new()
            }
        },
    }
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateKeyRequest {
    name: String,
    role: Option<String>,
    /// The Postgres databases the key may reach: 1 to 20 databases of the branch, disabled ones
    /// included. Absent or null is every served database.
    databases: Option<Vec<String>>,
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/branches/{branch_id}/keys",
    operation_id = "createKey",
    tag = "keys",
    summary = "Create a secret key",
    params(("id" = Uuid, Path, description = "Database id"), ("branch_id" = Uuid, Path, description = "Branch id")),
    request_body = CreateKeyRequest,
    responses(
        (status = 201, description = "The new key; its value is shown only here", body = CreatedDataKey),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's) or the branch doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`backup_branch`: a restore backup has no compute; `branch_not_ready`: the branch isn't ready", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the body is valid JSON but the wrong shape, or the name, role or databases list is invalid; `role_refused`: the worker refused the role (the message says why)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`worker_unavailable`: the worker or job queue can't be reached", body = ErrorEnvelope),
        (status = 504, description = "`timeout`: the worker didn't answer in time", body = ErrorEnvelope),
    ),
)]
async fn create_key<M, J>(
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
    let request: CreateKeyRequest = match parse_body(&body) {
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
    if let Some(names) = &request.databases
        && let Err(err) = validate_key_databases(&state, branch.id, names).await
    {
        return err.into_response();
    }
    if branch.kind == BranchKind::Backup {
        return conflict(ErrorCode::BackupBranch, BACKUP_REFUSAL).into_response();
    }
    if branch.status != BranchStatus::Ready {
        return conflict(ErrorCode::BranchNotReady, "branch is not ready").into_response();
    }
    match state.js.check_data_role(branch.id, &role).await {
        Ok(reply) if reply.ok => {}
        Ok(reply) => match (reply.kind.as_deref(), reply.error) {
            (Some("refused"), Some(message)) => {
                return ApiError::new(422, ErrorCode::RoleRefused, message).into_response();
            }
            (Some("not_ready"), _) => {
                return conflict(ErrorCode::BranchNotReady, "branch is not ready").into_response();
            }
            (kind, message) => {
                error!(branch_id = %branch.id, ?kind, "check data role failed: {message:?}");
                return ApiError::internal().into_response();
            }
        },
        Err(ResetError::Timeout) => {
            return ApiError::new(504, ErrorCode::Timeout, "worker did not respond; try again")
                .into_response();
        }
        Err(ResetError::Transport(err)) => {
            error!(branch_id = %branch.id, "check data role: {err}");
            return ApiError::new(503, ErrorCode::WorkerUnavailable, "worker unavailable")
                .into_response();
        }
    }
    let (key, plaintext) = new_key(
        &state.secrets,
        branch.id,
        DataKeyKind::Secret,
        &role,
        &request.name,
        request.databases,
    );
    if let Err(err) = state.metadata.insert_data_key(&key).await {
        error!("insert data key: {err}");
        return ApiError::internal().into_response();
    }
    info!(request_id = %principal.request_id, branch_id = %branch.id, key_id = %key.id, "secret key created");
    created(&key, plaintext)
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/branches/{branch_id}/keys/{key_id}/rotate",
    operation_id = "rotateKey",
    tag = "keys",
    summary = "Rotate a key",
    params(("id" = Uuid, Path, description = "Database id"), ("branch_id" = Uuid, Path, description = "Branch id"), ("key_id" = Uuid, Path, description = "Key id")),
    responses(
        (status = 201, description = "The replacement key (same kind, role and name); the old one stops working", body = CreatedDataKey),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the key doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`public_key_exists`: the branch already has a public key", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn rotate_key<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, key_id)): ApiPath<(Uuid, Uuid, Uuid)>,
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
    let old = match find_live_key(&state, branch.id, key_id).await {
        Ok(Some(old)) => old,
        Ok(None) => return ApiError::not_found("key not found").into_response(),
        Err(resp) => return resp.into_response(),
    };
    // The new key is the old one's kind, role and name; the old one stops working with it.
    let (key, plaintext) = new_key(
        &state.secrets,
        branch.id,
        old.kind,
        &old.role,
        &old.name,
        old.databases.clone(),
    );
    match state
        .metadata
        .rotate_data_key(branch.id, old.id, &key)
        .await
    {
        Ok(true) => {
            info!(request_id = %principal.request_id, branch_id = %branch.id, old_key_id = %old.id, key_id = %key.id, "key rotated");
            created(&key, plaintext)
        }
        Ok(false) => ApiError::not_found("key not found").into_response(),
        Err(MetadataError::Conflict) => {
            conflict(ErrorCode::PublicKeyExists, "the branch already has a public key")
                .into_response()
        }
        Err(err) => {
            error!("rotate data key: {err}");
            ApiError::internal().into_response()
        }
    }
}

/// Which databases a key may reach. `databases` has no default, so an empty body can't widen a
/// key to every database by accident.
#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateKeyRequest {
    /// The Postgres databases the key may reach (1 to 20 databases of the branch, disabled ones
    /// included), or null for every served database. Required.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<Vec<String>>, required = true)]
    databases: Option<Option<Vec<String>>>,
}

/// Distinguishes a missing field (outer `None`) from an explicit null (`Some(None)`).
fn present<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[utoipa::path(
    patch,
    path = "/v1/databases/{id}/branches/{branch_id}/keys/{key_id}",
    operation_id = "updateKey",
    tag = "keys",
    summary = "Change which databases a key may reach",
    description = "Replaces the key's database list on a secret or the public key. `databases` is required: a list of 1 to 20 databases of the branch (a disabled one is allowed; it is just not served), or null for every served database. Rest forgets the branch's cached state when the list changes. The gateway caches a key for up to 30 seconds, so a narrowed list applies to new requests within 30 seconds of the answer, not at once.",
    params(("id" = Uuid, Path, description = "Database id"), ("branch_id" = Uuid, Path, description = "Branch id"), ("key_id" = Uuid, Path, description = "Key id")),
    request_body = UpdateKeyRequest,
    responses(
        (status = 200, description = "The key after the change; only the public key carries its full value", body = DataKey),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the key doesn't exist", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: `databases` is missing or isn't null or a list of 1 to 20 distinct names of the branch's databases", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn update_key<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, key_id)): ApiPath<(Uuid, Uuid, Uuid)>,
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
    let request: UpdateKeyRequest = match parse_body(&body) {
        Ok(request) => request,
        Err(resp) => return resp.into_response(),
    };
    let Some(databases) = request.databases else {
        return unprocessable("databases is required (null is every served database)").into_response();
    };
    let old = match find_live_key(&state, branch.id, key_id).await {
        Ok(Some(old)) => old,
        Ok(None) => return ApiError::not_found("key not found").into_response(),
        Err(resp) => return resp.into_response(),
    };
    if let Some(names) = &databases
        && let Err(err) = validate_key_databases(&state, branch.id, names).await
    {
        return err.into_response();
    }
    match state
        .metadata
        .set_key_databases(branch.id, old.id, databases.clone())
        .await
    {
        Ok(true) => {}
        Ok(false) => return ApiError::not_found("key not found").into_response(),
        Err(err) => {
            error!("set key databases: {err}");
            return ApiError::internal().into_response();
        }
    }
    info!(request_id = %principal.request_id, branch_id = %branch.id, key_id = %old.id, "key databases changed");
    // Tokens already minted carry the old list; rest drops what it holds for the branch.
    if old.databases != databases {
        state.rest.invalidate(branch.id).await;
    }
    let blocked = old.kind == DataKeyKind::Secret
        && owner_level_roles(&state, &branch).await.contains(&old.role);
    match summary(DataKeyRecord { databases, ..old }, &state.secrets, blocked) {
        Ok(key) => no_store(Json(key).into_response()),
        Err(()) => ApiError::internal().into_response(),
    }
}

#[utoipa::path(
    delete,
    path = "/v1/databases/{id}/branches/{branch_id}/keys/{key_id}",
    operation_id = "revokeKey",
    tag = "keys",
    summary = "Revoke a secret key",
    params(("id" = Uuid, Path, description = "Database id"), ("branch_id" = Uuid, Path, description = "Branch id"), ("key_id" = Uuid, Path, description = "Key id")),
    responses(
        (status = 204, description = "The key is revoked"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the key doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`public_key_not_revocable`: a public key can only be rotated", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn revoke_key<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id, key_id)): ApiPath<(Uuid, Uuid, Uuid)>,
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
    let key = match find_live_key(&state, branch.id, key_id).await {
        Ok(Some(key)) => key,
        Ok(None) => return ApiError::not_found("key not found").into_response(),
        Err(resp) => return resp.into_response(),
    };
    // A branch always has a public key; revoking it would leave the Studio and the SDK without one.
    if key.kind == DataKeyKind::Public {
        return conflict(ErrorCode::PublicKeyNotRevocable, "rotate the public key instead")
            .into_response();
    }
    match state.metadata.revoke_data_key(branch.id, key.id).await {
        Ok(true) => {
            info!(request_id = %principal.request_id, branch_id = %branch.id, key_id = %key.id, "key revoked");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => ApiError::not_found("key not found").into_response(),
        Err(err) => {
            error!("revoke data key: {err}");
            ApiError::internal().into_response()
        }
    }
}

/// The branch's live key with this id.
async fn find_live_key<M, J>(
    state: &AppState<M, J>,
    branch: BranchId,
    key_id: Uuid,
) -> Result<Option<DataKeyRecord>, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    match state.metadata.list_data_keys(branch).await {
        Ok(keys) => Ok(keys.into_iter().find(|k| k.id == key_id)),
        Err(err) => {
            error!("list data keys: {err}");
            Err(ApiError::internal())
        }
    }
}

/// `?database=` on the schema route.
#[derive(Debug, Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct SchemaQuery {
    /// The Postgres database to describe; main when absent. It must be served by the data API.
    database: Option<String>,
}

/// The REST service's view of the branch's schema, passed through. The REST service is
/// internal: when it is down the answer is a 503 that doesn't say where it lives.
#[utoipa::path(
    get,
    path = "/v1/databases/{id}/branches/{branch_id}/data-api/schema",
    operation_id = "getDataApiSchema",
    tag = "data-api",
    summary = "The branch's schema as the data API sees it",
    params(
        ("id" = Uuid, Path, description = "Database id"),
        ("branch_id" = Uuid, Path, description = "Branch id"),
        SchemaQuery,
    ),
    responses(
        (status = 200, description = "The schemas, tables, functions and enums the data API exposes, with each table's role privileges", body = DataApiSchema),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database (or another tenant's), the branch or the named Postgres database doesn't exist", body = ErrorEnvelope),
        (status = 409, description = "`branch_not_ready`: the branch isn't ready; `data_api_disabled`: the named Postgres database isn't served by the data API", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 502, description = "`unavailable`: the data API answered something unexpected", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: the data API or the branch's database can't be reached; retry after `Retry-After`", body = ErrorEnvelope, headers(("Retry-After" = u32, description = "Seconds to wait before retrying"))),
    ),
)]
async fn data_api_schema<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, branch_id)): ApiPath<(Uuid, Uuid)>,
    query: Result<axum::extract::Query<SchemaQuery>, axum::extract::rejection::QueryRejection>,
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
    let Ok(axum::extract::Query(SchemaQuery { database })) = query else {
        return unprocessable("the query string is malformed").into_response();
    };
    if let Some(name) = &database {
        // Known and served, from metadata, before rest is asked.
        match state.metadata.branch_databases(branch.id).await {
            Ok(rows) => match rows.into_iter().find(|d| &d.name == name) {
                None => return crate::console::database_not_found().into_response(),
                Some(row) if row.data_api != chelabase_metadata::DataApiState::On => {
                    return crate::console::data_api_disabled(name).into_response();
                }
                Some(_) => {}
            },
            Err(err) => {
                error!("list branch databases: {err}");
                return ApiError::internal().into_response();
            }
        }
    }
    let url = format!(
        "{}/internal/schema/{}",
        state.rest.url.trim_end_matches('/'),
        branch.id.as_uuid()
    );
    let url = match &database {
        Some(name) => format!("{url}?database={}", urlencoding::encode(name)),
        None => url,
    };
    let response = match state
        .rest
        .http
        .get(url)
        .bearer_auth(&state.rest.token)
        .timeout(REST_TIMEOUT)
        .send()
        .await
    {
        Ok(response) => response,
        Err(err) => {
            error!(branch_id = %branch.id, "data API schema: rest unreachable: {err}");
            return ApiError::new(503, ErrorCode::Unavailable, "data API unavailable")
                .into_response();
        }
    };
    let status = response.status();
    let retry_after = response.headers().get(header::RETRY_AFTER).cloned();
    let body: Option<serde_json::Value> = response.json().await.ok();
    match (status, body) {
        (s, Some(body)) if s.is_success() => no_store(Json(body).into_response()),
        // Rest's own bodies are rewritten into ours; the 404 is its `unknown_branch`.
        (StatusCode::NOT_FOUND, Some(body)) => {
            if let Some(name) = &database
                && body["error"]["code"] == "unknown_database"
            {
                // It stopped being served (or went) since the check above.
                return match state.metadata.branch_databases(branch.id).await {
                    Ok(rows) => match rows.into_iter().find(|d| &d.name == name) {
                        Some(row) if row.data_api != chelabase_metadata::DataApiState::On => {
                            crate::console::data_api_disabled(name)
                        }
                        _ => crate::console::database_not_found(),
                    },
                    Err(err) => {
                        error!("list branch databases: {err}");
                        ApiError::internal()
                    }
                }
                .into_response();
            }
            ApiError::not_found("branch not found").into_response()
        }
        (StatusCode::CONFLICT, Some(_)) => {
            conflict(ErrorCode::BranchNotReady, "branch is not ready").into_response()
        }
        // The branch's database is unreachable or its schema is being reinstalled: Studio
        // retries. Rest's message is a fixed one; its `Retry-After` is kept.
        (StatusCode::SERVICE_UNAVAILABLE, Some(body)) => {
            warn!(branch_id = %branch.id, "data API schema: rest is unavailable for the branch");
            let message = body["error"]["message"]
                .as_str()
                .unwrap_or("data API unavailable");
            let mut err = ApiError::new(503, ErrorCode::Unavailable, message);
            if let Some(secs) = retry_after
                .as_ref()
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u32>().ok())
            {
                err = err.retry_after(secs);
            }
            err.into_response()
        }
        (status, _) => {
            error!(branch_id = %branch.id, %status, "data API schema: unexpected answer from rest");
            ApiError::new(
                502,
                ErrorCode::Unavailable,
                "data API returned an unexpected answer",
            )
            .into_response()
        }
    }
}
