//! Object storage's URL routes (m6), behind storage's bearer: the key its signed URLs are
//! signed with, whether the key that minted one is still live (a data key, or the member who
//! minted it from the console, m9), and the one-time upload tokens.

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chelabase_metadata::{BranchKind, BranchStatus, DataKeyLookup, DatabaseStatus};
use chelabase_types::access::{PlatformRole, Requested};
use chelabase_types::{BranchId, ProjectId};
use chelabase_neon::ComputeLocator;
use chelabase_secrets::{Purpose, derive_key};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::error;
use uuid::Uuid;

use super::{ConsoleUrlFacts, InternalState, NeonMetadataPort};
use crate::access::{RoleGraph, act_as_allowed, act_as_reads_graph, member_facts};
use crate::roles;

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

/// `?branch=`: the branch the URL was signed on; for a console URL, how it was minted.
#[derive(Deserialize)]
struct LiveQuery {
    branch: Option<Uuid>,
    /// The URL was minted in owner mode (m9): its member must still be an Admin.
    #[serde(default)]
    owner: bool,
    /// The role the URL was minted as (#100): its member must still hold it at Developer.
    #[serde(default)]
    role: Option<String>,
}

/// How a console URL was minted, as its signed claims say.
#[derive(Clone, Copy, Debug)]
pub enum ConsoleMinter<'a> {
    Owner,
    Role(&'a str),
}

/// The live key with this id (as the gateway's lookup answers it); else, for a URL minted from
/// the console (m9, the member as its key), the member while [`console_url_live`] says so,
/// on `?branch=`; 404 for anything else.
async fn live_key<M, L>(
    State(state): St<M, L>,
    Path(key_id): Path<Uuid>,
    Query(q): Query<LiveQuery>,
) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    match state.metadata.live_data_key(key_id).await {
        Ok(Some(key)) => return Json(key_json(&key)).into_response(),
        Ok(None) => {}
        Err(err) => return failed("storage key liveness", err),
    }
    let Some(branch) = q.branch else {
        return StatusCode::NOT_FOUND.into_response();
    };
    // A console URL names exactly one of owner mode and its role; one naming neither (minted
    // before #100) or both is never live.
    let minted = match (q.owner, q.role.as_deref()) {
        (true, None) => ConsoleMinter::Owner,
        (false, Some(role)) if !role.is_empty() => ConsoleMinter::Role(role),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    match console_url_live(&state.metadata, key_id, BranchId(branch), minted).await {
        Ok(Some(project)) => Json(json!({
            "key_id": key_id,
            "branch_id": branch,
            "project_id": project,
            "kind": "secret",
            "role": "",
            "databases": null,
            "role_oid": null,
        }))
        .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(err) => failed("console URL liveness", err),
    }
}

/// Whether `member` may still use a URL they minted from the console on `branch` (#100): the
/// branch is a normal one not being deleted, and console `sign` by the member, as the role the
/// URL was minted as (or in owner mode), on that branch, would be authorized now. That is
/// [`act_as_allowed`] at Developer (`sign`'s level) on the same facts [`authorize`] reads: the
/// database (not deleted, in the member's tenant), the member (active, not offboarded; a
/// tenant admin as [`tenant_admin_effective`]), their own and team grants with the database's
/// override, the branch's role graph (chains, R21; owner-level, LOGIN and ADMIN roles, R16,
/// R22, R23) and the personal logins (MH-21). The project when so.
///
/// Nothing here is cached: storage keeps each answer for `LIVENESS_TTL` (5 s) and drops them
/// on an access event, so a grant change counts at once and a role graph change (DDL, which
/// sends no access event) within 5 s.
///
/// [`authorize`]: crate::access::authorize
/// [`tenant_admin_effective`]: chelabase_types::access::tenant_admin_effective
pub async fn console_url_live<F: ConsoleUrlFacts>(
    facts: &F,
    member: Uuid,
    branch: BranchId,
    minted: ConsoleMinter<'_>,
) -> Result<Option<ProjectId>, chelabase_metadata::MetadataError> {
    let Some(b) = facts.get_branch(branch).await? else {
        return Ok(None);
    };
    if b.kind != BranchKind::Normal || matches!(b.status, BranchStatus::Deleting | BranchStatus::Deleted) {
        return Ok(None);
    }
    let Some(db) = facts.get_database(b.database_id).await? else {
        return Ok(None);
    };
    if db.status == DatabaseStatus::Deleted {
        return Ok(None);
    }
    let Some((m, sources)) = facts.member_access(member, db.project_id, db.database_id).await? else {
        return Ok(None);
    };
    if m.tenant_id != db.tenant_id {
        return Ok(None);
    }
    let caller = member_facts(Some(&m));
    let grant = sources.effective();
    let requested = match minted {
        ConsoleMinter::Owner => Requested::Owner,
        ConsoleMinter::Role(role) => Requested::Role(role),
    };
    let owner = roles::owner_name(&db);
    let min = PlatformRole::Developer;
    let graph = match (&b.endpoint_id, act_as_reads_graph(caller, grant.as_ref(), min, requested, owner)) {
        (Some(endpoint), true) => Some(RoleGraph::new(owner, facts.list_roles(endpoint).await?)),
        _ => None,
    };
    let personal_login = match minted {
        ConsoleMinter::Owner => false,
        ConsoleMinter::Role(role) => match Requested::from_name(role, owner) {
            Requested::Role(name) => facts.is_personal_login_name(db.database_id, name).await?,
            Requested::Owner => false,
        },
    };
    let allowed = act_as_allowed(caller, grant.as_ref(), min, requested, owner, graph.as_ref(), personal_login);
    Ok(allowed.is_ok().then_some(db.project_id))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewToken {
    token_id: Uuid,
    key_id: Uuid,
    claims: Value,
    expires_at: DateTime<Utc>,
}

/// Why an upload token's app session doesn't let it through.
enum NotLive {
    /// Not live, not the key's project's, or not a session id.
    Refused,
    /// The metadata store failed (logged).
    Failed,
}

/// The app session an upload token's claims name (`sid`, an app user's token), if any: its
/// expiry while it is live and in the key's project.
async fn session_of<M, L>(
    state: &InternalState<M, L>,
    claims: &Value,
    key_id: Uuid,
) -> Result<Option<DateTime<Utc>>, NotLive>
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    let Some(sid) = claims.get("sid") else {
        return Ok(None);
    };
    let sid: Uuid = sid
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or(NotLive::Refused)?;
    let key = match state.metadata.live_data_key(key_id).await {
        Ok(Some(key)) => key,
        Ok(None) => return Err(NotLive::Refused),
        Err(err) => {
            error!("storage key liveness: {err}");
            return Err(NotLive::Failed);
        }
    };
    match state.metadata.app_session(sid).await {
        Ok(Some(s))
            if s.revoked_at.is_none()
                && s.expires_at > Utc::now()
                && s.project_id == key.project_id =>
        {
            Ok(Some(s.expires_at))
        }
        Ok(_) => Err(NotLive::Refused),
        Err(err) => {
            error!("an upload token's app session: {err}");
            Err(NotLive::Failed)
        }
    }
}

/// Records a minted upload token: `{"expires_at"}` (seconds since the epoch), or 409 when its
/// id is taken or its key isn't live. An app user's token (its claims carry the session's
/// `sid`) needs that session live and in the key's project, and never outlives it: its expiry
/// is cut to the session's.
async fn insert_token<M, L>(State(state): St<M, L>, Json(t): Json<NewToken>) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    let expires_at = match session_of(&state, &t.claims, t.key_id).await {
        Ok(session) => session.map_or(t.expires_at, |s| s.min(t.expires_at)),
        Err(NotLive::Refused) => return StatusCode::CONFLICT.into_response(),
        Err(NotLive::Failed) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    match state
        .metadata
        .insert_upload_token(t.token_id, t.key_id, &t.claims, expires_at)
        .await
    {
        Ok(true) => Json(json!({ "expires_at": expires_at.timestamp() })).into_response(),
        Ok(false) => StatusCode::CONFLICT.into_response(),
        Err(err) => failed("recording an upload token", err),
    }
}

/// Spends an upload token: `{"key", "claims"}` (the key as it is now) once; 404 after, or for
/// an unknown or expired token, a dead key, or an app user's session no longer live.
async fn consume_token<M, L>(State(state): St<M, L>, Path(token_id): Path<Uuid>) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    match state.metadata.consume_upload_token(token_id).await {
        Ok(Some((key, claims))) => match session_of(&state, &claims, key.key_id).await {
            Ok(_) => Json(json!({ "key": key_json(&key), "claims": claims })).into_response(),
            Err(NotLive::Refused) => StatusCode::NOT_FOUND.into_response(),
            Err(NotLive::Failed) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        },
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(err) => failed("spending an upload token", err),
    }
}
