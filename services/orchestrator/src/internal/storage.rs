//! Object storage's URL routes (m6), behind storage's bearer: the key its signed URLs are
//! signed with, whether the key that minted one is still live, and the one-time upload tokens.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chelabase_metadata::DataKeyLookup;
use chelabase_neon::ComputeLocator;
use chelabase_secrets::{Purpose, derive_key};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::error;
use uuid::Uuid;

use super::{InternalState, NeonMetadataPort};

/// The URL-signing key's current generation. A new one (a rotation) makes every URL signed
/// with an older one fail.
pub const URL_KEY_ID: u32 = 1;

type St<M, L> = State<Arc<InternalState<M, L>>>;

pub(super) fn routes<M, L>() -> Router<Arc<InternalState<M, L>>>
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    Router::new()
        .route("/internal/storage/url-key", get(url_key::<M, L>))
        .route("/internal/storage/keys/{key_id}", get(live_key::<M, L>))
        .route(
            "/internal/storage/upload-tokens",
            post(insert_token::<M, L>),
        )
        .route(
            "/internal/storage/upload-tokens/{token_id}/consume",
            post(consume_token::<M, L>),
        )
}

fn key_json(key: &DataKeyLookup) -> Value {
    json!({
        "key_id": key.key_id,
        "branch_id": key.branch_id,
        "project_id": key.project_id,
        "kind": key.kind,
        "role": key.role,
        "databases": key.databases,
        "role_oid": key.role_oid,
    })
}

fn failed(context: &str, err: impl std::fmt::Display) -> Response {
    error!("{context}: {err}");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

/// `{"kid", "key"}`: the URL-signing key (32 bytes, base64), derived from the secrets key for
/// [`Purpose::StorageUrl`] and [`URL_KEY_ID`].
async fn url_key<M, L>(State(state): St<M, L>) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    let key = derive_key(&state.secrets, Purpose::StorageUrl, URL_KEY_ID);
    Json(json!({ "kid": URL_KEY_ID, "key": STANDARD.encode(key) })).into_response()
}

/// The live key with this id (as the gateway's lookup answers it); 404 for anything else.
async fn live_key<M, L>(State(state): St<M, L>, Path(key_id): Path<Uuid>) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    match state.metadata.live_data_key(key_id).await {
        Ok(Some(key)) => Json(key_json(&key)).into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(err) => failed("storage key liveness", err),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewToken {
    token_id: Uuid,
    key_id: Uuid,
    claims: Value,
    expires_at: DateTime<Utc>,
}

/// Records a minted upload token: 204, or 409 when its id is taken or its key isn't live.
async fn insert_token<M, L>(State(state): St<M, L>, Json(t): Json<NewToken>) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    match state
        .metadata
        .insert_upload_token(t.token_id, t.key_id, &t.claims, t.expires_at)
        .await
    {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => StatusCode::CONFLICT.into_response(),
        Err(err) => failed("recording an upload token", err),
    }
}

/// Spends an upload token: `{"key", "claims"}` (the key as it is now) once; 404 after, or for
/// an unknown or expired token or a dead key.
async fn consume_token<M, L>(State(state): St<M, L>, Path(token_id): Path<Uuid>) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    match state.metadata.consume_upload_token(token_id).await {
        Ok(Some((key, claims))) => {
            Json(json!({ "key": key_json(&key), "claims": claims })).into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(err) => failed("spending an upload token", err),
    }
}
