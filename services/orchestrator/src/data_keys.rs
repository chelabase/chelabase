//! The data API's platform routes: a branch's keys (list, create, rotate, revoke) and its
//! schema as the data API sees it. Keys are shown once, except the public key, which is sealed
//! so Studio can show it again.

use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, State},
    http::{StatusCode, header, header::CACHE_CONTROL},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use cheladb_metadata::{BranchKind, BranchStatus, DataKeyRecord, MetadataError, NewDataKey};
use cheladb_routes::orchestrator as routes;
use cheladb_secrets::{Purpose, open_credential, seal_credential};
use cheladb_types::BranchId;
use cheladb_types::data_key::{self, DataKeyKind};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::branches::{load_branch, parse_body, unprocessable};
use crate::{
    AppState, AuthPrincipal, JobPort, MetadataPort, ResetError, internal_error, json_error,
    load_owned,
};

/// Longest key name.
const MAX_NAME_LEN: usize = 63;
/// The role a secret key gets when the request names none.
const DEFAULT_ROLE: &str = "authenticated";
/// Longest wait for the REST service's schema.
const REST_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn routes<M, J>() -> Router<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    Router::new()
        .route(
            routes::LIST_KEYS.path,
            get(list_keys::<M, J>).post(create_key::<M, J>),
        )
        .route(routes::ROTATE_KEY.path, post(rotate_key::<M, J>))
        .route(routes::REVOKE_KEY.path, delete(revoke_key::<M, J>))
        .route(routes::DATA_API_SCHEMA.path, get(data_api_schema::<M, J>))
}

/// A key as listed: `key` only for the public key.
#[derive(Debug, Serialize)]
struct KeySummary {
    id: Uuid,
    kind: DataKeyKind,
    role: String,
    name: String,
    prefix: String,
    created_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
}

/// A key just made, with its plaintext.
#[derive(Debug, Serialize)]
struct CreatedKey {
    id: Uuid,
    kind: DataKeyKind,
    role: String,
    name: String,
    prefix: String,
    key: String,
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

/// The branch's key row for the response, opening the public key's sealed value.
fn summary(record: DataKeyRecord, secrets: &cheladb_secrets::SecretKey) -> Result<KeySummary, ()> {
    let key = match (record.kind, record.sealed.as_deref()) {
        (DataKeyKind::Public, Some(sealed)) => Some(
            open_credential(secrets, &record.id.to_string(), Purpose::DataKey, sealed)
                .map_err(|err| error!(key_id = %record.id, "opening a public key: {err}"))?,
        ),
        _ => None,
    };
    Ok(KeySummary {
        id: record.id,
        kind: record.kind,
        role: record.role,
        name: record.name,
        prefix: record.prefix,
        created_at: record.created_at,
        last_used_at: record.last_used_at,
        key,
    })
}

/// Builds the row and the plaintext of a new key. Public keys are sealed for display.
fn new_key(
    secrets: &cheladb_secrets::SecretKey,
    branch: BranchId,
    kind: DataKeyKind,
    role: &str,
    name: &str,
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
        },
        generated.key,
    )
}

fn created(key: &NewDataKey, plaintext: String) -> Response {
    no_store(
        (
            StatusCode::CREATED,
            Json(CreatedKey {
                id: key.id,
                kind: key.kind,
                role: key.role.clone(),
                name: key.name.clone(),
                prefix: key.prefix.clone(),
                key: plaintext,
            }),
        )
            .into_response(),
    )
}

async fn list_keys<M, J>(
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
    let keys = match state.metadata.list_data_keys(branch.id).await {
        Ok(keys) => keys,
        Err(err) => {
            error!("list data keys: {err}");
            return internal_error();
        }
    };
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        match summary(key, &state.secrets) {
            Ok(s) => out.push(s),
            Err(()) => return internal_error(),
        }
    }
    no_store(Json(out).into_response())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateKeyRequest {
    name: String,
    role: Option<String>,
}

async fn create_key<M, J>(
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
    let request: CreateKeyRequest = match parse_body(&body) {
        Ok(request) => request,
        Err(resp) => return *resp,
    };
    if request.name.is_empty() || request.name.chars().count() > MAX_NAME_LEN {
        return *unprocessable(&format!("name must be 1-{MAX_NAME_LEN} characters"));
    }
    let role = request.role.unwrap_or_else(|| DEFAULT_ROLE.to_string());
    if role.is_empty() {
        return *unprocessable("role must not be empty");
    }
    if branch.kind == BranchKind::Backup || branch.status != BranchStatus::Ready {
        return json_error(StatusCode::CONFLICT, "branch is not ready");
    }
    match state.js.check_data_role(branch.id, &role).await {
        Ok(reply) if reply.ok => {}
        Ok(reply) => match (reply.kind.as_deref(), reply.error) {
            (Some("refused"), Some(message)) => return *unprocessable(&message),
            (Some("not_ready"), _) => {
                return json_error(StatusCode::CONFLICT, "branch is not ready");
            }
            (kind, message) => {
                error!(branch_id = %branch.id, ?kind, "check data role failed: {message:?}");
                return internal_error();
            }
        },
        Err(ResetError::Timeout) => {
            return json_error(
                StatusCode::GATEWAY_TIMEOUT,
                "worker did not respond; try again",
            );
        }
        Err(ResetError::Transport(err)) => {
            error!(branch_id = %branch.id, "check data role: {err}");
            return json_error(StatusCode::SERVICE_UNAVAILABLE, "worker unavailable");
        }
    }
    let (key, plaintext) = new_key(
        &state.secrets,
        branch.id,
        DataKeyKind::Secret,
        &role,
        &request.name,
    );
    if let Err(err) = state.metadata.insert_data_key(&key).await {
        error!("insert data key: {err}");
        return internal_error();
    }
    info!(request_id = %principal.request_id, branch_id = %branch.id, key_id = %key.id, "secret key created");
    created(&key, plaintext)
}

async fn rotate_key<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    Path((id, branch_id, key_id)): Path<(Uuid, Uuid, Uuid)>,
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
    let old = match find_live_key(&state, branch.id, key_id).await {
        Ok(Some(old)) => old,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "key not found"),
        Err(resp) => return *resp,
    };
    // The new key is the old one's kind, role and name; the old one stops working with it.
    let (key, plaintext) = new_key(&state.secrets, branch.id, old.kind, &old.role, &old.name);
    match state
        .metadata
        .rotate_data_key(branch.id, old.id, &key)
        .await
    {
        Ok(true) => {
            info!(request_id = %principal.request_id, branch_id = %branch.id, old_key_id = %old.id, key_id = %key.id, "key rotated");
            created(&key, plaintext)
        }
        Ok(false) => json_error(StatusCode::NOT_FOUND, "key not found"),
        Err(MetadataError::Conflict) => {
            json_error(StatusCode::CONFLICT, "the branch already has a public key")
        }
        Err(err) => {
            error!("rotate data key: {err}");
            internal_error()
        }
    }
}

async fn revoke_key<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    Path((id, branch_id, key_id)): Path<(Uuid, Uuid, Uuid)>,
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
    let key = match find_live_key(&state, branch.id, key_id).await {
        Ok(Some(key)) => key,
        Ok(None) => return json_error(StatusCode::NOT_FOUND, "key not found"),
        Err(resp) => return *resp,
    };
    // A branch always has a public key; revoking it would leave the Studio and the SDK without one.
    if key.kind == DataKeyKind::Public {
        return json_error(StatusCode::CONFLICT, "rotate the public key instead");
    }
    match state.metadata.revoke_data_key(branch.id, key.id).await {
        Ok(true) => {
            info!(request_id = %principal.request_id, branch_id = %branch.id, key_id = %key.id, "key revoked");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => json_error(StatusCode::NOT_FOUND, "key not found"),
        Err(err) => {
            error!("revoke data key: {err}");
            internal_error()
        }
    }
}

/// The branch's live key with this id.
async fn find_live_key<M, J>(
    state: &AppState<M, J>,
    branch: BranchId,
    key_id: Uuid,
) -> Result<Option<DataKeyRecord>, Box<Response>>
where
    M: MetadataPort,
    J: JobPort,
{
    match state.metadata.list_data_keys(branch).await {
        Ok(keys) => Ok(keys.into_iter().find(|k| k.id == key_id)),
        Err(err) => {
            error!("list data keys: {err}");
            Err(Box::new(internal_error()))
        }
    }
}

/// The REST service's view of the branch's schema, passed through. The REST service is
/// internal: when it is down the answer is a 503 that doesn't say where it lives.
async fn data_api_schema<M, J>(
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
    let url = format!(
        "{}/internal/schema/{}",
        state.rest.url.trim_end_matches('/'),
        branch.id.as_uuid()
    );
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
            return json_error(StatusCode::SERVICE_UNAVAILABLE, "data API unavailable");
        }
    };
    let status = response.status();
    let retry_after = response.headers().get(header::RETRY_AFTER).cloned();
    let body: Option<serde_json::Value> = response.json().await.ok();
    match (status, body) {
        (s, Some(body)) if s.is_success() => no_store(Json(body).into_response()),
        (StatusCode::NOT_FOUND | StatusCode::CONFLICT, Some(body)) => {
            (status, Json(body)).into_response()
        }
        // The branch's database is unreachable or its schema is being reinstalled: Studio
        // retries. Rest's body is already a fixed message.
        (StatusCode::SERVICE_UNAVAILABLE, Some(body)) => {
            warn!(branch_id = %branch.id, "data API schema: rest is unavailable for the branch");
            let mut resp = (status, Json(body)).into_response();
            if let Some(v) = retry_after {
                resp.headers_mut().insert(header::RETRY_AFTER, v);
            }
            resp
        }
        (status, _) => {
            error!(branch_id = %branch.id, %status, "data API schema: unexpected answer from rest");
            json_error(
                StatusCode::BAD_GATEWAY,
                "data API returned an unexpected answer",
            )
        }
    }
}
