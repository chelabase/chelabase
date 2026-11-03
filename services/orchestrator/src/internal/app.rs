//! App users (step 6b): the `auth` service's routes (branch info, sessions, sync), the
//! gateway's session liveness, and Zitadel's Actions webhook (tenant members only: app users
//! live in identity pools since #123).
//!
//! A refresh token is `"{session_id}.{secret}"`: 32 random bytes, base64url without padding.
//! Only the SHA-256 of the whole token is stored.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chelabase_auth::app_token::APP_REFRESH_GRACE;
use chelabase_metadata::{
    AppSession, BranchKind, BranchStatus, NewAppSession, PeekOutcome, ProjectStatus, RefreshNext,
    RefreshOutcome,
};
use chelabase_nats::{AppUsersSyncJob, JobEnvelope};
use chelabase_neon::ComputeLocator;
use chelabase_secrets::{open, open_role_password, seal};
use chelabase_types::api_error::{ApiError, ErrorCode};
use chelabase_types::{BranchId, ProjectId};
use chelabase_identity::{IdentityError, Missing, Pool};
use chelabase_zitadel::actions::{
    ActionEvent, SIGNATURE_HEADER, UserEventKind, user_event_kind, verify_signature,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tracing::{error, warn};
use uuid::Uuid;

use super::config::APP_SESSION_CAP;
use super::{BoxFuture, InternalState, NeonMetadataPort, require_any_bearer, require_bearer};

/// The role the `auth` service writes the users table as (P7).
const SYNC_ROLE: &str = "chelabase_auth_sync";
/// Zitadel's events are small; this is generous.
const WEBHOOK_BODY_LIMIT: usize = 64 * 1024;

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

/// The app routes: `auth`'s group, the liveness route `auth` and the gateway share (P31), and
/// the webhook (its signature is its authentication).
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
    Router::new()
        .route(
            "/internal/zitadel/events",
            post(webhook::<M, L>).layer(DefaultBodyLimit::max(WEBHOOK_BODY_LIMIT)),
        )
        .merge(auth)
        .merge(liveness)
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

/// A new token for `session`.
fn new_refresh_token(session: Uuid) -> String {
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).expect("OS random number generator");
    format!("{session}.{}", URL_SAFE_NO_PAD.encode(secret))
}

fn token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

/// The session id a token names; `None` for anything malformed.
fn token_session(token: &str) -> Option<Uuid> {
    let (id, secret) = token.split_once('.')?;
    if secret.is_empty() {
        return None;
    }
    Uuid::parse_str(id).ok()
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

/// What `auth` needs for a branch: the project's app-user settings, the branch the users table
/// lives on (`target_branch_id`, R5) and how to reach it as the sync role (`sync`, or null
/// when that branch has no running compute or no sync password yet).
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
    match m.project_by_id(project).await {
        Ok(Some(p)) if p.status == ProjectStatus::Active => {}
        Ok(_) => return disabled(),
        Err(e) => return internal("app branch info", e),
    }
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
        "zitadel_org_id": auth.zitadel_org_id,
        "email_verification": auth.email_verification,
        "on_delete": auth.on_delete.as_str(),
        "auth_database_id": auth.database_id,
        "pg_database": auth.pg_database,
        "schema": auth.schema_name,
        "table": auth.table_name,
        "target_branch_id": target.id,
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
    let token = new_refresh_token(id);
    let ttl = state.app_refresh_ttl.min(APP_SESSION_CAP);
    let expires_at = Utc::now() + chrono::Duration::from_std(ttl).expect("ttl fits");
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
    let Some(id) = token_session(&body.refresh_token) else {
        return refresh_invalid();
    };
    let next_token = new_refresh_token(id);
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
        .refresh_app_session(id, body.project_id, token_hash(&body.refresh_token), &next)
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
        RefreshOutcome::Reused(session) => {
            warn!(session_id = %id, "a replaced refresh token was reused; the session is revoked");
            end_identity_sessions_later(&state, vec![session]);
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
    let Some(id) = token_session(&body.refresh_token) else {
        return refresh_invalid();
    };
    // The project is checked in the same read (I5).
    match state
        .metadata
        .peek_app_session(
            id,
            body.project_id,
            token_hash(&body.refresh_token),
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
        Ok(None) => {}
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
    let Some(id) = token_session(&body.refresh_token) else {
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
    created_before: Option<DateTime<Utc>>,
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
            "live": s.revoked_at.is_none() && s.expires_at > Utc::now(),
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

// --- Zitadel Actions webhook -----------------------------------------------------------------

/// Zitadel's user events (Actions v2). 401 for a bad signature, 503 without a signing key,
/// otherwise 200 (events that aren't ours are ignored). The answer is decided first; the sync
/// publish and the Zitadel session deletes run after it.
async fn webhook<M, L>(State(state): AppState<M, L>, headers: HeaderMap, body: Bytes) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    // No key (unset, or its file not written or refused) is 503: nothing is checked against
    // an absent key, and nothing below runs.
    let Some(key) = state.actions_signing_key.as_ref() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let key = match key.get() {
        Ok(key) => key,
        Err(e) => {
            match e {
                chelabase_types::file_value::Unavailable::NotYet { .. } => {
                    tracing::debug!("the Actions webhook waits for its signing key: {e}")
                }
                chelabase_types::file_value::Unavailable::Refused { .. } => {
                    warn!("the Actions webhook can't use its signing key: {e}")
                }
            }
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    let signature = headers
        .get(SIGNATURE_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if verify_signature(signature, &body, key, Utc::now().timestamp()).is_err() {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let event: ActionEvent = match serde_json::from_slice(&body) {
        Ok(event) => event,
        Err(err) => {
            warn!("ignoring a Zitadel event that doesn't parse: {err}");
            return StatusCode::OK.into_response();
        }
    };
    let Some(kind) = user_event_kind(&event.event_type) else {
        return StatusCode::OK.into_response();
    };
    // W18: a tenant member's Zitadel user removed, deactivated or locked loses what realtime
    // lets them watch: an access event naming them (`sub` = the Zitadel user id). Tenant orgs
    // have no app users, so nothing below applies to them.
    if matches!(kind, UserEventKind::Gone | UserEventKind::Deactivated) {
        match state.metadata.tenant_by_org(&event.resource_owner).await {
            Ok(Some(tenant)) => {
                // D4, P17: the member's personal logins are revoked first; a failed write is
                // 500, so Zitadel delivers the event again (nothing after it has run).
                let rows = match state
                    .metadata
                    .revoke_logins_for_identity_user(tenant, &event.aggregate_id)
                    .await
                {
                    Ok(rows) => rows,
                    Err(e) => return internal("revoking an offboarded member's personal logins", e),
                };
                offboarded_logins_changed(&state, rows).await;
                let event = chelabase_nats::rt::AccessEvent {
                    tenant_id: tenant.as_uuid(),
                    member_sub: Some(event.aggregate_id.clone()),
                };
                super::realtime::spawn_access(state.rt_events.clone(), event);
                return StatusCode::OK.into_response();
            }
            Ok(None) => {}
            // T8-2, P17: a 200 here would skip the revocation for good; Zitadel delivers a
            // failed event again.
            Err(e) => return internal("Zitadel event: the org's tenant lookup", e),
        }
    }
    // D7: a tenant member's user reactivated or unlocked loses its offboarded mark (its logins
    // stay revoked; it makes new ones). A failed clear is 500, so Zitadel delivers it again.
    if kind == UserEventKind::Reactivated {
        match state.metadata.tenant_by_org(&event.resource_owner).await {
            Ok(Some(tenant)) => {
                if let Err(e) = state
                    .metadata
                    .clear_offboarded(tenant, &event.aggregate_id)
                    .await
                {
                    return internal("clearing a reactivated member's offboarded mark", e);
                }
                let event = chelabase_nats::rt::AccessEvent {
                    tenant_id: tenant.as_uuid(),
                    member_sub: Some(event.aggregate_id.clone()),
                };
                super::realtime::spawn_access(state.rt_events.clone(), event);
                return StatusCode::OK.into_response();
            }
            Ok(None) => {}
            Err(e) => return internal("Zitadel event: the org's tenant lookup", e),
        }
    }
    // #123: app users live in identity pools. Every write to them goes through auth or the
    // orchestrator, which revoke and sync inline, so an event for any other org is ignored.
    StatusCode::OK.into_response()
}

/// After an offboarding revocation (D4): the proxies forget every personal row of the member
/// (`rows` holds revoked ones from earlier deliveries too, so a redelivery still clears them),
/// and each branch holding one reconciles its member roles, which drops the logins. Failures
/// are logged: the verifier is already gone from the metadata, and the worker's sweep
/// reconciles every branch anyway.
async fn offboarded_logins_changed<M, L>(
    state: &InternalState<M, L>,
    rows: Vec<chelabase_metadata::personal_logins::LoginEndpoint>,
) {
    let branches: std::collections::BTreeSet<Uuid> = rows.iter().map(|r| r.branch_id).collect();
    super::ddl::publish_login_rows(&state.cache, rows);
    for branch in branches {
        if let Err(e) = state.resync.reconcile_member_roles(BranchId(branch)).await {
            warn!(branch_id = %branch, "Zitadel event: member-role reconcile not asked for: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_tokens_name_their_session() {
        let id = Uuid::new_v4();
        let token = new_refresh_token(id);
        assert_eq!(token_session(&token), Some(id));
        assert_eq!(token.len(), 36 + 1 + 43);
        assert_eq!(token_session(&id.to_string()), None);
        assert_eq!(token_session(&format!("{id}.")), None);
        assert_eq!(token_session("x.y"), None);
    }
}
