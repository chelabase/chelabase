//! Object storage m18: a database's webhook endpoints for storage object events.
//!
//! Developer on the database manages them (a Viewer is 403, a non-member 404). A URL must be
//! `https` (or `http` when the operator allows it) and resolve only to public addresses, unless
//! the operator allowlisted its host (`chelabase_http::outbound`); the worker checks again at
//! every connection. The signing secret is returned once, at creation and at rotation, and is
//! stored sealed (`Purpose::Webhook`): no read returns it.

use std::sync::Arc;

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chelabase_metadata::{
    NewStorageWebhook, StorageWebhook, StorageWebhookAttempt, StorageWebhookPatch,
};
use chelabase_nats::storage_objects::{AttemptOutcome, ObjectEventType, TestReply, TestRequest};
use chelabase_routes::orchestrator as ep;
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use chelabase_types::{ApiJson, ApiPath};
use serde::{Deserialize, Serialize};
use tracing::{error, info};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::access::{Target, authorize_database};
use crate::branches::{conflict, unprocessable};
use crate::{AppState, AuthPrincipal, JobPort, MetadataPort, ResetError};

/// Attempts a read returns (the newest).
const ATTEMPTS_SHOWN: i64 = 100;
const MAX_BUCKET_LEN: usize = 100;
const MAX_PREFIX_LEN: usize = 1024;

pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new()
        .routes(routes!(list_storage_webhooks, create_storage_webhook))
        .routes(routes!(
            get_storage_webhook,
            update_storage_webhook,
            delete_storage_webhook
        ))
        .routes(routes!(rotate_storage_webhook_secret))
        .routes(routes!(test_storage_webhook))
        .routes(routes!(list_storage_webhook_attempts))
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct CreateStorageWebhookRequest {
    /// Where events are POSTed: `https` (`http` only where the operator allows it), at most
    /// 2048 bytes, no credentials, resolving only to public addresses.
    url: String,
    /// One or more of `object.created`, `object.updated`, `object.deleted`, `object.moved`.
    events: Vec<String>,
    /// Only objects of this bucket (by name, 1-100 bytes).
    bucket: Option<String>,
    /// Only object names starting with this (1-1024 bytes).
    prefix: Option<String>,
}

#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
struct UpdateStorageWebhookRequest {
    url: Option<String>,
    events: Option<Vec<String>>,
    /// `null` clears the filter; absent leaves it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>, nullable)]
    bucket: Option<Option<String>>,
    /// `null` clears the filter; absent leaves it.
    #[serde(default, deserialize_with = "present")]
    #[schema(value_type = Option<String>, nullable)]
    prefix: Option<Option<String>>,
    /// `false` stops delivery; `true` restarts it from now (events in between are not sent) and
    /// clears the failure state.
    enabled: Option<bool>,
}

/// A present field, `null` included, is `Some`; an absent one stays `None` (via `default`).
fn present<'de, D>(d: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(d).map(Some)
}

/// An endpoint with its signing secret: shown only here, once.
#[derive(Serialize, ToSchema)]
struct StorageWebhookWithSecret {
    webhook: StorageWebhook,
    /// The signing secret (`whsec_…`): verify each delivery's `Chelabase-Signature` with it.
    /// It can't be read again; rotate it to get a new one.
    secret: String,
}

/// How a test delivery went.
#[derive(Serialize, ToSchema)]
struct StorageWebhookTestResult {
    /// `delivered`, `http_error`, `timeout`, `connect_error` or `refused`.
    outcome: String,
    /// The endpoint's HTTP status, when it answered.
    status: Option<u16>,
    duration_ms: u64,
}

fn not_found() -> ApiError {
    ApiError::not_found("storage webhook not found")
}

fn internal(err: impl std::fmt::Display) -> ApiError {
    error!("storage webhooks: {err}");
    ApiError::internal()
}

/// A new signing secret: `whsec_` and 32 random bytes in hex.
fn new_secret() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("OS random number generator");
    format!("whsec_{}", hex::encode(bytes))
}

/// `url` if it may be registered: its form, then every address it resolves to now.
async fn valid_url<M, J>(state: &AppState<M, J>, url: &str) -> Result<String, ApiError> {
    state
        .webhooks
        .check_resolved(url)
        .await
        .map(|u| u.to_string())
        .map_err(|refused| {
            // The address at fault goes to the log only: the answer names the host alone.
            info!("storage webhook url refused: {}", refused.log_detail());
            unprocessable(&format!("`url` is refused: {refused}"))
        })
}

fn valid_events(events: &[String]) -> Result<Vec<String>, ApiError> {
    let mut kinds = Vec::new();
    for e in events {
        match ObjectEventType::parse(e) {
            Some(kind) if !kinds.contains(&kind) => kinds.push(kind),
            Some(_) => {}
            None => {
                return Err(unprocessable(
                    "`events` lists object.created, object.updated, object.deleted or object.moved",
                ));
            }
        }
    }
    if kinds.is_empty() {
        return Err(unprocessable("`events` lists at least one event"));
    }
    kinds.sort();
    Ok(kinds.into_iter().map(|k| k.as_str().to_string()).collect())
}

fn valid_filter(field: &str, value: Option<&str>, max: usize) -> Result<(), ApiError> {
    match value {
        Some(v) if v.is_empty() || v.len() > max || v.contains('\0') => Err(unprocessable(
            &format!("`{field}` must be 1 to {max} bytes"),
        )),
        _ => Ok(()),
    }
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/storage/webhooks",
    operation_id = "listStorageWebhooks",
    tag = "storage",
    summary = "List storage webhooks",
    description = "The database's webhook endpoints for storage object events (never their secrets).",
    params(("id" = Uuid, Path, description = "Database id")),
    responses(
        (status = 200, description = "The endpoints, oldest first", body = Vec<StorageWebhook>),
        (status = 400, description = "`invalid_request`: the path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: Developer on the database is required", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database doesn't exist, or is another tenant's", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_storage_webhooks<M: MetadataPort, J: JobPort>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
) -> Result<Json<Vec<StorageWebhook>>, ApiError> {
    authorize_database(
        &state,
        &principal,
        ep::LIST_STORAGE_WEBHOOKS.policy.access,
        Target::Database(id),
    )
    .await?;
    Ok(Json(
        state
            .metadata
            .list_storage_webhooks(id)
            .await
            .map_err(internal)?,
    ))
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/storage/webhooks",
    operation_id = "createStorageWebhook",
    tag = "storage",
    summary = "Create a storage webhook",
    description = "Registers an endpoint for the database's storage object events (every branch's, made through the storage API; changes made with SQL directly on `storage.objects` send none). Each event is POSTed as JSON, signed with `Chelabase-Signature: t=<unix>,v1=<hex HMAC-SHA256 of \"<t>.<body>\">`, at least once, retried with backoff for up to 24 hours. The secret is in this response only. A database has at most 20 endpoints.",
    params(("id" = Uuid, Path, description = "Database id")),
    request_body = CreateStorageWebhookRequest,
    responses(
        (status = 201, description = "The endpoint and its secret (shown only now)", body = StorageWebhookWithSecret),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: Developer on the database is required", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database doesn't exist, or is another tenant's", body = ErrorEnvelope),
        (status = 409, description = "`conflict`: the database already has 20 endpoints", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: an unknown or missing field, a refused `url` (not https, credentials, a private, loopback, link-local or metadata address, or one that doesn't resolve), an unknown event, or a filter out of bounds", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn create_storage_webhook<M: MetadataPort, J: JobPort>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(request): ApiJson<CreateStorageWebhookRequest>,
) -> Result<Response, ApiError> {
    authorize_database(
        &state,
        &principal,
        ep::CREATE_STORAGE_WEBHOOK.policy.access,
        Target::Database(id),
    )
    .await?;
    let events = valid_events(&request.events)?;
    valid_filter("bucket", request.bucket.as_deref(), MAX_BUCKET_LEN)?;
    valid_filter("prefix", request.prefix.as_deref(), MAX_PREFIX_LEN)?;
    let url = valid_url(&state, &request.url).await?;
    let webhook_id = Uuid::new_v4();
    let secret = new_secret();
    let sealed = chelabase_secrets::seal_webhook_secret(&state.secrets, webhook_id, &secret);
    let created = state
        .metadata
        .create_storage_webhook(&NewStorageWebhook {
            id: webhook_id,
            database_id: id,
            url: &url,
            events: &events,
            bucket: request.bucket.as_deref(),
            prefix: request.prefix.as_deref(),
            secret_enc: &sealed,
        })
        .await
        .map_err(internal)?;
    match created {
        Some(webhook) => Ok((
            StatusCode::CREATED,
            Json(StorageWebhookWithSecret { webhook, secret }),
        )
            .into_response()),
        None => Err(conflict(
            ErrorCode::Conflict,
            format!(
                "a database has at most {} storage webhooks",
                chelabase_metadata::STORAGE_WEBHOOK_CAP
            ),
        )),
    }
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/storage/webhooks/{webhook_id}",
    operation_id = "getStorageWebhook",
    tag = "storage",
    summary = "Get a storage webhook",
    description = "One endpoint, with its failure state (never its secret).",
    params(("id" = Uuid, Path, description = "Database id"), ("webhook_id" = Uuid, Path, description = "Webhook id")),
    responses(
        (status = 200, description = "The endpoint", body = StorageWebhook),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: Developer on the database is required", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database or the endpoint doesn't exist, or isn't the caller's", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn get_storage_webhook<M: MetadataPort, J: JobPort>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, webhook_id)): ApiPath<(Uuid, Uuid)>,
) -> Result<Json<StorageWebhook>, ApiError> {
    authorize_database(
        &state,
        &principal,
        ep::GET_STORAGE_WEBHOOK.policy.access,
        Target::Database(id),
    )
    .await?;
    state
        .metadata
        .get_storage_webhook(id, webhook_id)
        .await
        .map_err(internal)?
        .map(Json)
        .ok_or_else(not_found)
}

#[utoipa::path(
    patch,
    path = "/v1/databases/{id}/storage/webhooks/{webhook_id}",
    operation_id = "updateStorageWebhook",
    tag = "storage",
    summary = "Update a storage webhook",
    description = "Changes the given fields. `bucket` and `prefix` accept `null` to clear them. Enabling a disabled endpoint restarts delivery from now and clears its failure state.",
    params(("id" = Uuid, Path, description = "Database id"), ("webhook_id" = Uuid, Path, description = "Webhook id")),
    request_body = UpdateStorageWebhookRequest,
    responses(
        (status = 200, description = "The endpoint", body = StorageWebhook),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the body isn't well-formed JSON", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: Developer on the database is required", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database or the endpoint doesn't exist, or isn't the caller's", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: an unknown field, a refused `url`, an unknown event, or a filter out of bounds", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn update_storage_webhook<M: MetadataPort, J: JobPort>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, webhook_id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(request): ApiJson<UpdateStorageWebhookRequest>,
) -> Result<Json<StorageWebhook>, ApiError> {
    authorize_database(
        &state,
        &principal,
        ep::UPDATE_STORAGE_WEBHOOK.policy.access,
        Target::Database(id),
    )
    .await?;
    let events = request.events.as_deref().map(valid_events).transpose()?;
    valid_filter(
        "bucket",
        request.bucket.as_ref().and_then(|b| b.as_deref()),
        MAX_BUCKET_LEN,
    )?;
    valid_filter(
        "prefix",
        request.prefix.as_ref().and_then(|p| p.as_deref()),
        MAX_PREFIX_LEN,
    )?;
    let url = match &request.url {
        Some(url) => Some(valid_url(&state, url).await?),
        None => None,
    };
    let patch = StorageWebhookPatch {
        url,
        events,
        bucket: request.bucket,
        prefix: request.prefix,
        enabled: request.enabled,
    };
    state
        .metadata
        .update_storage_webhook(id, webhook_id, &patch)
        .await
        .map_err(internal)?
        .map(Json)
        .ok_or_else(not_found)
}

#[utoipa::path(
    delete,
    path = "/v1/databases/{id}/storage/webhooks/{webhook_id}",
    operation_id = "deleteStorageWebhook",
    tag = "storage",
    summary = "Delete a storage webhook",
    description = "Deletes the endpoint and its attempts; deliveries in flight stop within seconds.",
    params(("id" = Uuid, Path, description = "Database id"), ("webhook_id" = Uuid, Path, description = "Webhook id")),
    responses(
        (status = 204, description = "Deleted"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: Developer on the database is required", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database or the endpoint doesn't exist, or isn't the caller's", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn delete_storage_webhook<M: MetadataPort, J: JobPort>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, webhook_id)): ApiPath<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    authorize_database(
        &state,
        &principal,
        ep::DELETE_STORAGE_WEBHOOK.policy.access,
        Target::Database(id),
    )
    .await?;
    if state
        .metadata
        .delete_storage_webhook(id, webhook_id)
        .await
        .map_err(internal)?
    {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found())
    }
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/storage/webhooks/{webhook_id}/rotate-secret",
    operation_id = "rotateStorageWebhookSecret",
    tag = "storage",
    summary = "Rotate a storage webhook's secret",
    description = "Replaces the signing secret; deliveries are signed with the new one within seconds. The new secret is in this response only.",
    params(("id" = Uuid, Path, description = "Database id"), ("webhook_id" = Uuid, Path, description = "Webhook id")),
    responses(
        (status = 200, description = "The endpoint and its new secret (shown only now)", body = StorageWebhookWithSecret),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: Developer on the database is required", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database or the endpoint doesn't exist, or isn't the caller's", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn rotate_storage_webhook_secret<M: MetadataPort, J: JobPort>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, webhook_id)): ApiPath<(Uuid, Uuid)>,
) -> Result<Json<StorageWebhookWithSecret>, ApiError> {
    authorize_database(
        &state,
        &principal,
        ep::ROTATE_STORAGE_WEBHOOK_SECRET.policy.access,
        Target::Database(id),
    )
    .await?;
    let secret = new_secret();
    let sealed = chelabase_secrets::seal_webhook_secret(&state.secrets, webhook_id, &secret);
    let webhook = state
        .metadata
        .rotate_storage_webhook_secret(id, webhook_id, &sealed)
        .await
        .map_err(internal)?
        .ok_or_else(not_found)?;
    Ok(Json(StorageWebhookWithSecret { webhook, secret }))
}

#[utoipa::path(
    post,
    path = "/v1/databases/{id}/storage/webhooks/{webhook_id}/test",
    operation_id = "testStorageWebhook",
    tag = "storage",
    summary = "Send a test event to a storage webhook",
    description = "Sends one signed `webhook.test` event now (enabled or not), without retries and without counting toward the endpoint's failures, and answers how it went.",
    params(("id" = Uuid, Path, description = "Database id"), ("webhook_id" = Uuid, Path, description = "Webhook id")),
    responses(
        (status = 200, description = "The attempt", body = StorageWebhookTestResult),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: Developer on the database is required", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database or the endpoint doesn't exist, or isn't the caller's", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`worker_unavailable`: no worker answered", body = ErrorEnvelope),
    ),
)]
async fn test_storage_webhook<M: MetadataPort, J: JobPort>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, webhook_id)): ApiPath<(Uuid, Uuid)>,
) -> Result<Json<StorageWebhookTestResult>, ApiError> {
    authorize_database(
        &state,
        &principal,
        ep::TEST_STORAGE_WEBHOOK.policy.access,
        Target::Database(id),
    )
    .await?;
    if state
        .metadata
        .get_storage_webhook(id, webhook_id)
        .await
        .map_err(internal)?
        .is_none()
    {
        return Err(not_found());
    }
    let unavailable = || {
        ApiError::new(
            503,
            ErrorCode::WorkerUnavailable,
            "no worker answered the test",
        )
    };
    match state
        .js
        .test_storage_webhook(TestRequest {
            database_id: id,
            webhook_id,
        })
        .await
    {
        Ok(TestReply::Sent {
            outcome,
            status,
            duration_ms,
        }) => Ok(Json(StorageWebhookTestResult {
            outcome: AttemptOutcome::as_str(outcome).to_string(),
            status,
            duration_ms,
        })),
        Ok(TestReply::NotFound) => Err(not_found()),
        Ok(TestReply::Failed { message }) => Err(internal(message)),
        Err(ResetError::Timeout | ResetError::Transport(_)) => Err(unavailable()),
    }
}

#[utoipa::path(
    get,
    path = "/v1/databases/{id}/storage/webhooks/{webhook_id}/attempts",
    operation_id = "listStorageWebhookAttempts",
    tag = "storage",
    summary = "List a storage webhook's recent attempts",
    description = "The endpoint's newest delivery attempts (at most 100), newest first: status and duration only.",
    params(("id" = Uuid, Path, description = "Database id"), ("webhook_id" = Uuid, Path, description = "Webhook id")),
    responses(
        (status = 200, description = "The attempts", body = Vec<StorageWebhookAttempt>),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`forbidden`: Developer on the database is required", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: the database or the endpoint doesn't exist, or isn't the caller's", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
    ),
)]
async fn list_storage_webhook_attempts<M: MetadataPort, J: JobPort>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, webhook_id)): ApiPath<(Uuid, Uuid)>,
) -> Result<Json<Vec<StorageWebhookAttempt>>, ApiError> {
    authorize_database(
        &state,
        &principal,
        ep::LIST_STORAGE_WEBHOOK_ATTEMPTS.policy.access,
        Target::Database(id),
    )
    .await?;
    if state
        .metadata
        .get_storage_webhook(id, webhook_id)
        .await
        .map_err(internal)?
        .is_none()
    {
        return Err(not_found());
    }
    Ok(Json(
        state
            .metadata
            .list_storage_webhook_attempts(id, webhook_id, ATTEMPTS_SHOWN)
            .await
            .map_err(internal)?,
    ))
}
