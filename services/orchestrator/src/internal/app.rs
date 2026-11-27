//! App users (step 6b): the `auth` service's routes (branch info, sessions, sync) and the
//! gateway's session liveness.
//!
//! A refresh token is `"{session_id}.{nonce}.{tag}"`: 32 random bytes and their HMAC-SHA256
//! tag under the app-refresh key (bound to the session), both base64url without padding.
//! Sessions from before tags have `"{session_id}.{secret}"` tokens until their next rotation.
//! Only the SHA-256 of the whole token is stored.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chelabase_auth::app_token::APP_REFRESH_GRACE;
use chelabase_metadata::{
    AppSession, BranchKind, BranchStatus, NewAppSession, PeekOutcome, ProjectStatus, RefreshNext,
    RefreshOutcome, RefreshProof,
};
use chelabase_nats::{AppUsersSyncJob, JobEnvelope};
use chelabase_neon::ComputeLocator;
use chelabase_secrets::{Purpose, SecretKey, derive_key, open, open_role_password, seal};
use chelabase_types::api_error::{ApiError, ErrorCode};
use chelabase_types::{BranchId, ProjectId};
use chelabase_identity::{IdentityError, Missing, Pool};
use chelabase_time::{Timestamp, TimestampExt};
use serde::Deserialize;
use serde_json::json;
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tracing::{error, warn};
use uuid::Uuid;

use super::config::APP_SESSION_CAP;
use super::{BoxFuture, InternalState, NeonMetadataPort, require_any_bearer, require_bearer};

/// The role the `auth` service writes the users table as (P7).
const SYNC_ROLE: &str = "chelabase_auth_sync";

/// Publishes app-user sync jobs (the worker writes the users table).
pub trait AppJobPort: Send + Sync {
    fn publish_app_users_sync(
        &self,
        job: JobEnvelope<AppUsersSyncJob>,
    ) -> BoxFuture<'_, Result<(), String>>;
}

impl AppJobPort for crate::NatsPorts {
    fn publish_app_users_sync(
        &self,
        job: JobEnvelope<AppUsersSyncJob>,
    ) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            chelabase_nats::publish_app_users_sync(&self.js, &job)
                .await
                .map_err(|e| e.to_string())
        })
    }
}

type AppState<M, L> = State<Arc<InternalState<M, L>>>;

/// The app routes: `auth`'s group and the liveness route `auth` and the gateway share (P31).
pub(super) fn routes<M, L>(state: &InternalState<M, L>) -> Router<Arc<InternalState<M, L>>>
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    let auth_token: Arc<str> = state.app_auth_token.as_str().into();
    let auth = Router::new()
        .route(
            "/internal/app/branches/{branch_id}",
            get(branch_info::<M, L>),
        )
        .route("/internal/app/sessions", post(create_session::<M, L>))
        .route("/internal/app/sessions/peek", post(peek::<M, L>))
        .route("/internal/app/sessions/refresh", post(refresh::<M, L>))
        .route(
            "/internal/app/sessions/revoke-by-token",
            post(revoke_by_token::<M, L>),
        )
        .route("/internal/app/sessions/{id}/revoke", post(revoke::<M, L>))
        .route(
            "/internal/app/users/{user_id}/revoke-sessions",
            post(revoke_user_sessions::<M, L>),
        )
        .route("/internal/app/sync", post(sync::<M, L>))
        .route_layer(middleware::from_fn_with_state(auth_token, require_bearer));
    let either: Arc<[Arc<str>]> = Arc::new([
        state.app_auth_token.as_str().into(),
        state.app_sessions_token.as_str().into(),
    ]);
    let liveness = Router::new()
        .route("/internal/app/sessions/{id}", get(liveness::<M, L>))
        .route_layer(middleware::from_fn_with_state(either, require_any_bearer));
    Router::new().merge(auth).merge(liveness)
}

fn internal(context: &str, err: impl std::fmt::Display) -> Response {
    error!("{context}: {err}");
    ApiError::internal().into_response()
}

fn disabled() -> Response {
    ApiError::new(
        404,
        ErrorCode::AppUsersDisabled,
        "app users are not enabled for this branch's project",
    )
    .into_response()
}

fn refresh_invalid() -> Response {
    ApiError::new(
        401,
        ErrorCode::RefreshTokenInvalid,
        "the refresh token is not valid",
    )
    .into_response()
}

// --- Refresh tokens --------------------------------------------------------------------------

/// What the tag of a refresh token is computed over, after the session and nonce.
const REFRESH_TAG_LABEL: &[u8] = b"app-refresh v1";

/// The key id the refresh tag's key is derived under ([`Purpose::AppRefresh`]). Changing it
/// rotates the key, and every refresh token issued under the old one stops verifying.
const REFRESH_KEY_ID: u32 = 0;

/// The tag of a refresh token: HMAC-SHA256 under the app-refresh key over the label, the
/// session id and the nonce. It binds the token to its session.
fn refresh_tag(secrets: &SecretKey, session: Uuid, nonce: &[u8]) -> [u8; 32] {
    let key = derive_key(secrets, Purpose::AppRefresh, REFRESH_KEY_ID);
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&key).expect("HMAC accepts any key size");
    mac.update(REFRESH_TAG_LABEL);
    mac.update(session.as_bytes());
    mac.update(nonce);
    mac.finalize().into_bytes().into()
}

/// A new token for `session`: `"{session}.{nonce}.{tag}"`.
fn new_refresh_token(secrets: &SecretKey, session: Uuid) -> String {
    let mut nonce = [0u8; 32];
    getrandom::fill(&mut nonce).expect("OS random number generator");
    let tag = refresh_tag(secrets, session, &nonce);
    format!("{session}.{}.{}", URL_SAFE_NO_PAD.encode(nonce), URL_SAFE_NO_PAD.encode(tag))
}

fn token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// The session id a token names and what it proves: `Issued` for a three-part token whose tag
/// verifies, `Unproven` for a two-part one from before tags. `None` for anything malformed,
/// a three-part token whose tag doesn't verify included.
fn token_session(secrets: &SecretKey, token: &str) -> Option<(Uuid, RefreshProof)> {
    let mut parts = token.split('.');
    let id_part = parts.next()?;
    let id = Uuid::parse_str(id_part).ok()?;
    let secret = parts.next().filter(|s| !s.is_empty())?;
    let Some(tag) = parts.next() else {
        return Some((id, RefreshProof::Unproven));
    };
    // Only the form `new_refresh_token` writes: another spelling of the same token would hash
    // differently and read as an earlier one.
    if parts.next().is_some() || id_part != id.to_string() {
        return None;
    }
    let nonce = URL_SAFE_NO_PAD.decode(secret).ok()?;
    let tag = URL_SAFE_NO_PAD.decode(tag).ok()?;
    let expected = refresh_tag(secrets, id, &nonce);
    bool::from(expected.as_slice().ct_eq(tag.as_slice())).then_some((id, RefreshProof::Issued))
}

/// What the grace answer's sealed token is bound to.
fn grace_aad(session: Uuid) -> String {
    format!("app_session:{session}")
}

/// Ends the identity sessions behind revoked app sessions in the background (T7: the answer
/// doesn't wait on the identity service, up to 10 s per session); failures are only logged.
/// The app session's revocation is what refuses its refresh token; auth's refresh also refuses
/// any app session whose identity session is gone, so a failure here only leaves a dead row.
fn end_identity_sessions_later<M, L>(state: &Arc<InternalState<M, L>>, sessions: Vec<AppSession>)
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    if sessions.is_empty() || state.identity.is_none() {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move { end_identity_sessions(&state, &sessions).await });
}

/// Ends the identity sessions behind revoked app sessions, each in its own project's pool; one
/// already gone is fine. Failures are only logged.
async fn end_identity_sessions<M, L>(state: &InternalState<M, L>, sessions: &[AppSession]) {
    let Some(identity) = &state.identity else {
        return;
    };
    for s in sessions {
        let pool = Pool::App(s.project_id.as_uuid());
        match identity.revoke_session(&pool, &s.identity_session_id).await {
            Ok(()) | Err(IdentityError::NotFound(Missing::Session)) => {}
            Err(err) => warn!(session_id = %s.id, "ending the identity session: {err}"),
        }
    }
}

// --- Branch info -----------------------------------------------------------------------------

/// What `auth` needs for a branch: the project's app-user settings and name (app-user mail
/// shows it), the branch the users table lives on (`target_branch_id`, R5) and how to reach it
/// as the sync role (`sync`, or null when that branch has no running compute or no sync
/// password yet).
async fn branch_info<M, L>(State(state): AppState<M, L>, Path(branch_id): Path<Uuid>) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    let m = &state.metadata;
    let branch = match m.get_branch(BranchId(branch_id)).await {
        Ok(Some(b)) if !matches!(b.status, BranchStatus::Deleting | BranchStatus::Deleted) => b,
        Ok(_) => return disabled(),
        Err(e) => return internal("app branch info", e),
    };
    let project = match m.database_project(branch.database_id).await {
        Ok(Some(p)) => p,
        Ok(None) => return disabled(),
        Err(e) => return internal("app branch info", e),
    };
    let project_name = match m.project_by_id(project).await {
        Ok(Some(p)) if p.status == ProjectStatus::Active => p.name,
        Ok(_) => return disabled(),
        Err(e) => return internal("app branch info", e),
    };
    let auth = match m.project_auth(project).await {
        Ok(Some(a)) => a,
        Ok(None) => return disabled(),
        Err(e) => return internal("app branch info", e),
    };
    let target = if branch.database_id == auth.database_id && branch.kind == BranchKind::Normal {
        branch
    } else {
        match m.main_branch(auth.database_id).await {
            Ok(Some(main)) => main,
            Ok(None) => {
                warn!(project_id = %project, "the auth database has no main branch");
                return disabled();
            }
            Err(e) => return internal("app branch info", e),
        }
    };
    let sync = match sync_target(&state, target.id, target.endpoint_id.as_deref(), &auth).await {
        Ok(sync) => sync,
        Err(err) => return internal("app branch info", err),
    };
    Json(json!({
        "project_id": project,
        "status": auth.status.as_str(),
        "email_verification": auth.email_verification,
        "on_delete": auth.on_delete.as_str(),
        "auth_database_id": auth.database_id,
        "pg_database": auth.pg_database,
        "schema": auth.schema_name,
        "table": auth.table_name,
        "target_branch_id": target.id,
        "project_name": project_name,
        "sync": sync,
    }))
    .into_response()
}

/// The `SyncTarget` for `branch`, or null. `Err` is what went wrong, for the log.
async fn sync_target<M, L>(
    state: &InternalState<M, L>,
    branch: BranchId,
    endpoint: Option<&str>,
    auth: &chelabase_metadata::ProjectAuth,
) -> Result<serde_json::Value, String>
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    let Some(endpoint) = endpoint else {
        return Ok(serde_json::Value::Null);
    };
    let row = match state.metadata.branch_auth_sync(branch).await {
        // A row for another Postgres database is from a location that has since moved.
        Ok(Some(row)) if row.pg_database == auth.pg_database => row,
        Ok(_) => return Ok(serde_json::Value::Null),
        Err(e) => return Err(e.to_string()),
    };
    let addr = match state.locator.postgres_addr(endpoint).await {
        Ok(Some(addr)) => addr,
        Ok(None) => return Ok(serde_json::Value::Null),
        Err(err) => {
            warn!(branch_id = %branch, "locating the compute's Postgres: {err:#}");
            return Ok(serde_json::Value::Null);
        }
    };
    let password = open_role_password(
        &state.secrets,
        &branch.to_string(),
        SYNC_ROLE,
        &row.password_sealed,
    )
    .map_err(|e| format!("opening the sync role's password: {e}"))?;
    Ok(json!({
        "branch_id": branch,
        "host": addr.ip().to_string(),
        "port": addr.port(),
        "database": row.pg_database,
        // U15: the auth service dials `host` and verifies the compute as this name, the
        // only one its certificate carries.
        "server_name": chelabase_neon::compute_tls::server_name(endpoint),
        "user": SYNC_ROLE,
        "password": password,
        "schema": auth.schema_name,
        "table": auth.table_name,
    }))
}

// --- Sessions --------------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateSession {
    project_id: ProjectId,
    user_id: Uuid,
    identity_session_id: String,
}

async fn create_session<M, L>(
    State(state): AppState<M, L>,
    Json(body): Json<CreateSession>,
) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    let id = Uuid::new_v4();
    let token = new_refresh_token(&state.secrets, id);
    let ttl = state.app_refresh_ttl.min(APP_SESSION_CAP);
    let expires_at = Timestamp::now() + ttl;
    let row = NewAppSession {
        id,
        project_id: body.project_id,
        user_id: body.user_id,
        identity_session_id: &body.identity_session_id,
        refresh_hash: token_hash(&token),
        expires_at,
    };
    if let Err(e) = state.metadata.insert_app_session(&row).await {
        return internal("creating an app session", e);
    }
    (
        StatusCode::CREATED,
        Json(json!({ "session_id": id, "refresh_token": token, "expires_at": expires_at })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct TokenBody {
    project_id: ProjectId,
    refresh_token: String,
}

async fn refresh<M, L>(State(state): AppState<M, L>, Json(body): Json<TokenBody>) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    let Some((id, proof)) = token_session(&state.secrets, &body.refresh_token) else {
        return refresh_invalid();
    };
    let next_token = new_refresh_token(&state.secrets, id);
    let next = RefreshNext {
        new_hash: token_hash(&next_token),
        new_sealed: seal(&state.secrets, &grace_aad(id), &next_token),
        sliding: state.app_refresh_ttl,
        cap: APP_SESSION_CAP,
        // How long the previous refresh token still gets the new one; auth's user lookup is
        // bounded by it.
        grace: APP_REFRESH_GRACE,
    };
    // The project is checked inside the locked transaction, before any write (I5).
    let outcome = match state
        .metadata
        .refresh_app_session(id, body.project_id, token_hash(&body.refresh_token), proof, &next)
        .await
    {
        Ok(outcome) => outcome,
        Err(e) => return internal("refreshing an app session", e),
    };
    let (session, token) = match outcome {
        RefreshOutcome::Rotated(session) => (session, next_token),
        RefreshOutcome::Grace { session, sealed } => {
            match open(&state.secrets, &grace_aad(id), &sealed) {
                Ok(token) => (session, token),
                Err(e) => return internal("opening the grace refresh token", e),
            }
        }
        // The user and project are the token's own session's, read under its lock: a reused
        // token ends that user's sessions in that project and nobody else's (#311).
        RefreshOutcome::Reused(sessions) => {
            warn!(
                session_id = %id,
                revoked = sessions.len(),
                "an earlier refresh token was reused; every session of its user is revoked"
            );
            end_identity_sessions_later(&state, sessions);
            return ApiError::new(
                401,
                ErrorCode::RefreshTokenReused,
                "the refresh token was already used; sign in again",
            )
            .into_response();
        }
        RefreshOutcome::Invalid => return refresh_invalid(),
    };
    Json(json!({
        "session_id": session.id,
        "user_id": session.user_id,
        "identity_session_id": session.identity_session_id,
        "refresh_token": token,
        "expires_at": session.expires_at,
    }))
    .into_response()
}

/// What `refresh` would answer for this token, read without writing (#59): the session, its
/// user and whether the token is the previous one within the grace, else `refresh`'s 401
/// verdict. Unlike `refresh`, it revokes nothing: not on reuse, not on expiry. `auth` checks
/// the user with it before anything rotates, and sends a verdict on to `refresh`.
async fn peek<M, L>(State(state): AppState<M, L>, Json(body): Json<TokenBody>) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    let Some((id, proof)) = token_session(&state.secrets, &body.refresh_token) else {
        return refresh_invalid();
    };
    // The project is checked in the same read (I5).
    match state
        .metadata
        .peek_app_session(
            id,
            body.project_id,
            token_hash(&body.refresh_token),
            proof,
            APP_SESSION_CAP,
        )
        .await
    {
        Ok(PeekOutcome::Live { session, grace }) => Json(json!({
            "session_id": session.id,
            "user_id": session.user_id,
            // #123: auth checks this identity session before anything rotates.
            "identity_session_id": session.identity_session_id,
            "grace": grace,
        }))
        .into_response(),
        Ok(PeekOutcome::Reused) => ApiError::new(
            401,
            ErrorCode::RefreshTokenReused,
            "the refresh token was already used; sign in again",
        )
        .into_response(),
        Ok(PeekOutcome::Invalid) => refresh_invalid(),
        Err(e) => internal("peeking at an app session", e),
    }
}

#[derive(Deserialize)]
struct ProjectBody {
    project_id: ProjectId,
}

/// Idempotent; another project's session is left alone (I5).
async fn revoke<M, L>(
    State(state): AppState<M, L>,
    Path(id): Path<Uuid>,
    Json(body): Json<ProjectBody>,
) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    match state.metadata.revoke_app_session(id, body.project_id).await {
        Ok(Some(session)) => end_identity_sessions_later(&state, vec![session]),
        // Already revoked (a repeated sign-out): its identity session is ended again, so a
        // retry finishes what a failed first try left (one already gone is fine).
        Ok(None) => match state.metadata.app_session(id).await {
            Ok(Some(session)) if session.project_id == body.project_id && session.revoked_at.is_some() => {
                end_identity_sessions_later(&state, vec![session]);
            }
            Ok(_) => {}
            Err(e) => return internal("revoking an app session", e),
        },
        Err(e) => return internal("revoking an app session", e),
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Sign-out with a refresh token: revokes only when the token is the session's current or
/// previous one and the project matches; anything else does nothing.
async fn revoke_by_token<M, L>(
    State(state): AppState<M, L>,
    Json(body): Json<TokenBody>,
) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    // Hash-only: a token that matches neither stored hash does nothing, whatever it proves.
    let Some((id, _)) = token_session(&state.secrets, &body.refresh_token) else {
        return StatusCode::NO_CONTENT.into_response();
    };
    match state
        .metadata
        .revoke_app_session_by_hash(id, body.project_id, token_hash(&body.refresh_token))
        .await
    {
        Ok(Some(session)) => end_identity_sessions_later(&state, vec![session]),
        Ok(None) => {}
        Err(e) => return internal("revoking an app session", e),
    }
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
struct RevokeUserSessions {
    project_id: ProjectId,
    #[serde(default)]
    keep_session_id: Option<Uuid>,
    #[serde(default)]
    created_before: Option<Timestamp>,
}

async fn revoke_user_sessions<M, L>(
    State(state): AppState<M, L>,
    Path(user_id): Path<Uuid>,
    Json(body): Json<RevokeUserSessions>,
) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    match state
        .metadata
        .revoke_user_sessions(
            body.project_id,
            user_id,
            body.keep_session_id,
            body.created_before,
        )
        .await
    {
        Ok(revoked) => end_identity_sessions_later(&state, revoked),
        Err(e) => return internal("revoking a user's app sessions", e),
    }
    StatusCode::NO_CONTENT.into_response()
}

/// `{live, project_id, user_id}`: live unless revoked or past `expires_at`. 404 when unknown.
async fn liveness<M, L>(State(state): AppState<M, L>, Path(id): Path<Uuid>) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    match state.metadata.app_session(id).await {
        Ok(Some(s)) => Json(json!({
            "live": s.revoked_at.is_none() && s.expires_at > Timestamp::now(),
            "project_id": s.project_id,
            "user_id": s.user_id,
        }))
        .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => internal("app session liveness", e),
    }
}

// --- Sync ------------------------------------------------------------------------------------

async fn sync<M, L>(State(state): AppState<M, L>, Json(job): Json<AppUsersSyncJob>) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    let envelope = JobEnvelope {
        job_id: Uuid::new_v4(),
        payload: job,
    };
    match state.app_jobs.publish_app_users_sync(envelope).await {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(err) => {
            error!("publishing an app-users sync job: {err}");
            ApiError::new(503, ErrorCode::Unavailable, "the sync queue is unavailable")
                .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issued_tokens_carry_a_valid_tag() {
        let key = SecretKey::generate();
        let id = Uuid::new_v4();
        let token = new_refresh_token(&key, id);
        assert_eq!(token.len(), 36 + 1 + 43 + 1 + 43, "{token}");
        assert_eq!(token_session(&key, &token), Some((id, RefreshProof::Issued)));
        assert_ne!(token, new_refresh_token(&key, id), "every token has its own nonce");
        let (nonce, tag) = token[37..].split_once('.').unwrap();
        let nonce = URL_SAFE_NO_PAD.decode(nonce).unwrap();
        assert_eq!(URL_SAFE_NO_PAD.decode(tag).unwrap(), refresh_tag(&key, id, &nonce));
    }

    #[test]
    fn a_token_with_a_wrong_tag_is_malformed() {
        let key = SecretKey::generate();
        let (id, other) = (Uuid::new_v4(), Uuid::new_v4());
        let token = new_refresh_token(&key, id);
        let rest = &token[37..];
        let (nonce, tag) = rest.split_once('.').unwrap();
        let mut flipped = URL_SAFE_NO_PAD.decode(tag).unwrap();
        flipped[0] ^= 1;
        let bad = [
            // Another key's token.
            new_refresh_token(&SecretKey::generate(), id),
            // The tag binds the session: moved to another id, it fails.
            format!("{other}.{rest}"),
            format!("{id}.{nonce}.{}", URL_SAFE_NO_PAD.encode(flipped)),
            format!("{id}.{nonce}.{}", &tag[..42]),
            format!("{id}.{nonce}."),
            format!("{id}.{nonce}.not base64"),
            format!("{id}..{tag}"),
            format!("{token}.x"),
            // Another spelling of the same session id.
            format!("{}.{rest}", id.to_string().to_uppercase()),
            format!("{}.{rest}", id.simple()),
        ];
        for t in bad {
            assert_eq!(token_session(&key, &t), None, "{t}");
        }
    }

    #[test]
    fn legacy_tokens_are_unproven() {
        let key = SecretKey::generate();
        let id = Uuid::new_v4();
        let legacy = format!("{id}.{}", URL_SAFE_NO_PAD.encode([7u8; 32]));
        assert_eq!(token_session(&key, &legacy), Some((id, RefreshProof::Unproven)));
        assert_eq!(token_session(&key, &format!("{id}.anything")), Some((id, RefreshProof::Unproven)));
        assert_eq!(token_session(&key, &id.to_string()), None);
        assert_eq!(token_session(&key, &format!("{id}.")), None);
        assert_eq!(token_session(&key, "x.y"), None);
        assert_eq!(token_session(&key, ""), None);
    }
}
