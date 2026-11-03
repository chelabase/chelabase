//! Managing a project's app users from Studio: list, create, change, deactivate and delete.
//! The project's identity pool `app-<project>` holds the users; a create writes the users table
//! inline, and the worker's sync job keeps it in step afterwards.
//!
//! Ending a user's access always runs in one order: the `app_sessions` rows first (refresh is
//! refused from that moment, even if the identity service is down), then the identity sessions
//! (a ban or delete ends those itself), then the sync job. The sync job revokes `app_sessions`
//! again for a banned or gone user, which catches a sign-in that raced the ban, and reconcile
//! does the same for every banned user, which catches a lost job.
//!
//! A step that fails leaves a request that can be sent again: a retried email change revokes
//! again, a retried delete of a user already gone publishes the sync again.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Query, State, rejection::QueryRejection},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use chelabase_metadata::{AppUsersStatus, ProjectRecord};
use chelabase_nats::{AppUsersSyncJob, JobEnvelope};
use chelabase_query::app_users::{NewUserRow, SYNC_ROLE, SyncTarget, SyncTls, UserRowError};
use crate::console::run_tls;
use crate::sql_editor::run::RunTls;
use chelabase_secrets::open_role_password;
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use chelabase_types::{ApiJson, ApiPath, BranchId};
use chelabase_identity::rules::{normalize_email, strong_password};
use chelabase_identity::{Identity, IdentityError, Missing, NewUser, Pool, User};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::app_users::{app_users_disabled, load_live_project, read_auth, read_auth_checked};
use crate::branches::{conflict, unprocessable};
use crate::{AppState, AuthPrincipal, JobPort, MetadataPort, load_main};
use chelabase_routes::orchestrator as ep;
use chelabase_types::access::Access;

/// Users per page (P26).
const PAGE_SIZE: u32 = 50;
const MAX_EMAIL: usize = 254;

pub(crate) fn routes<M, J>() -> OpenApiRouter<Arc<AppState<M, J>>>
where
    M: MetadataPort,
    J: JobPort,
{
    OpenApiRouter::new()
        .routes(routes!(list_app_users, create_app_user))
        .routes(routes!(get_app_user, update_app_user, delete_app_user))
        .routes(routes!(deactivate_app_user))
        .routes(routes!(reactivate_app_user))
        .routes(routes!(list_app_user_sessions, revoke_app_user_sessions))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AppUserState {
    Active,
    /// Deactivated (banned): it can't sign in.
    Inactive,
}

/// One of a project's users.
#[derive(Debug, Serialize, ToSchema)]
pub struct AppUser {
    pub id: Uuid,
    pub email: String,
    pub email_verified: bool,
    pub state: AppUserState,
    pub created_at: DateTime<Utc>,
}

impl AppUser {
    /// `None` for a user whose id isn't a UUID (a pool made before ids were UUIDs).
    fn from_identity(user: &User) -> Option<Self> {
        Some(Self {
            id: user.id.parse().ok()?,
            email: user.email.clone(),
            email_verified: user.email_verified,
            state: if user.banned { AppUserState::Inactive } else { AppUserState::Active },
            created_at: user.created_at,
        })
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AppUserPage {
    pub users: Vec<AppUser>,
    /// Pass it as `page_token` for the next page; `null` on the last.
    #[schema(required = true)]
    pub next_page_token: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateAppUserRequest {
    pub email: String,
    pub password: String,
    /// Default true: the address counts as verified and no email is sent.
    #[serde(default)]
    pub email_verified: Option<bool>,
}

#[derive(Debug, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct UpdateAppUserRequest {
    /// A new address, verified unless `email_verified` is false.
    #[serde(default)]
    pub email: Option<String>,
    /// With `email`, whether the new address is verified (default true); alone, sets the flag
    /// on the current address.
    #[serde(default)]
    pub email_verified: Option<bool>,
}

/// A live session; never its token.
#[derive(Debug, Serialize, ToSchema)]
pub struct AppUserSession {
    pub id: Uuid,
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct ListQuery {
    search: Option<String>,
    page_token: Option<String>,
}

/// 503 `identity_unavailable`: the identity service is missing, unreachable, or refused the call.
pub(crate) fn identity_unavailable() -> ApiError {
    ApiError::new(503, ErrorCode::IdentityUnavailable, "the identity service can't be reached; try again")
}

/// An identity failure the route didn't map itself: an unknown user is `404`, an email another
/// user holds `409`, a password or value the service refuses `422`. Anything else (no answer, a
/// refused credential, a missing pool, a refusal this client has no name for) is `503`.
pub(crate) fn identity_failed(what: &str, err: &IdentityError) -> ApiError {
    match err {
        IdentityError::NotFound(Missing::User) => ApiError::not_found("user not found"),
        IdentityError::Conflict(_) => email_taken(),
        IdentityError::Invalid { code, .. } if code == "weak_password" => weak_password(),
        IdentityError::Invalid { .. } => unprocessable("the identity service refused the value"),
        IdentityError::Unavailable(_) => {
            error!("identity: {what}: {err}");
            identity_unavailable()
        }
        _ => {
            error!("identity refused: {what}: {err}");
            identity_unavailable()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The identity service's refusals, by type; anything it has no answer for is `503`.
    #[test]
    fn identity_failures_map_by_type() {
        let invalid = |code: &str| IdentityError::Invalid { code: code.into(), message: String::new() };
        let cases = [
            (IdentityError::NotFound(Missing::User), 404, ErrorCode::NotFound),
            (IdentityError::Conflict("taken".into()), 409, ErrorCode::EmailTaken),
            (invalid("weak_password"), 422, ErrorCode::WeakPassword),
            (invalid("invalid_email"), 422, ErrorCode::InvalidRequest),
            (IdentityError::NotFound(Missing::Pool), 503, ErrorCode::IdentityUnavailable),
            (IdentityError::NotFound(Missing::Route), 503, ErrorCode::IdentityUnavailable),
            (IdentityError::NotFound(Missing::Session), 503, ErrorCode::IdentityUnavailable),
            (IdentityError::Unavailable("timeout".into()), 503, ErrorCode::IdentityUnavailable),
            (IdentityError::Unauthorized(String::new()), 503, ErrorCode::IdentityUnavailable),
            (IdentityError::Forbidden(String::new()), 503, ErrorCode::IdentityUnavailable),
            (
                IdentityError::Rejected { status: 404, code: "odd".into(), message: String::new() },
                503,
                ErrorCode::IdentityUnavailable,
            ),
        ];
        for (err, status, code) in cases {
            let api = identity_failed("test", &err);
            assert_eq!((api.status, api.code), (status, code), "{err}");
        }
    }

    #[test]
    fn passwords_follow_the_shared_rule() {
        assert!(checked_password("Str0ng!pass").is_ok());
        assert_eq!(checked_password("").unwrap_err().code, ErrorCode::InvalidRequest);
        for weak in ["Sh0rt!", "abcdefgh1!", "ABCDEFGH1!", "Abcdefgh!!", "Abcdefgh12"] {
            assert_eq!(checked_password(weak).unwrap_err().code, ErrorCode::WeakPassword, "{weak}");
        }
    }
}

/// A project whose app users are enabled, with its pool.
struct Ctx {
    project: ProjectRecord,
    pool: Pool,
}

async fn ctx<M, J>(state: &AppState<M, J>, principal: &AuthPrincipal, access: Access, id: Uuid) -> Result<Ctx, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let project = load_live_project(state, principal, access, id).await?;
    match read_auth_checked(state, principal, access, &project).await? {
        Some(auth) if auth.status == AppUsersStatus::Enabled => {}
        _ => return Err(app_users_disabled(409)),
    }
    // The identity client isn't required here: a route that ends access revokes in metadata
    // first, and a route that can't work without the client asks for it itself.
    Ok(Ctx { pool: Pool::App(project.id.as_uuid()), project })
}

fn identity<M, J>(state: &AppState<M, J>) -> Result<&Identity, ApiError> {
    state.identity_service.as_ref().ok_or_else(identity_unavailable)
}

/// The user, when it is in the project's pool.
async fn user_in_pool(identity: &Identity, pool: &Pool, user: Uuid) -> Result<User, ApiError> {
    identity.get_user(pool, &user.to_string()).await.map_err(|err| identity_failed("get user", &err))
}

async fn publish_sync<M, J>(state: &AppState<M, J>, project: &ProjectRecord, user: Uuid, origin: Option<BranchId>)
where
    M: MetadataPort,
    J: JobPort,
{
    let job = JobEnvelope {
        job_id: Uuid::new_v4(),
        payload: AppUsersSyncJob { project_id: project.id, user_id: user, data: None, origin_branch_id: origin },
    };
    // The worker's reconcile catches a lost message.
    if let Err(err) = state.js.publish_app_users_sync(&job).await {
        warn!(%user, "publish app users sync failed (reconcile will retry): {err}");
    }
}

/// Ends the user's `app_sessions`: from here on their refresh tokens are refused.
async fn revoke_app_sessions<M, J>(state: &AppState<M, J>, project: &ProjectRecord, user: Uuid) -> Result<(), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    state.metadata.revoke_user_sessions(project.id, user, None, None).await.map(drop).map_err(|err| {
        error!("revoke user sessions: {err}");
        ApiError::internal()
    })
}

/// Ends the user's sessions in metadata, then in the identity service. Each is attempted
/// whatever the other did; the first failure is the answer.
async fn revoke_sessions<M, J>(state: &AppState<M, J>, ctx: &Ctx, user: Uuid) -> Result<(), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let app = revoke_app_sessions(state, &ctx.project, user).await;
    let remote = match identity(state) {
        Ok(identity) => identity
            .revoke_user_sessions(&ctx.pool, &user.to_string())
            .await
            .map_err(|err| identity_failed("revoke user sessions", &err)),
        Err(err) => Err(err),
    };
    app.and(remote)
}

fn checked_email(email: &str) -> Result<String, ApiError> {
    let email = email.trim();
    let valid = !email.is_empty()
        && email.len() <= MAX_EMAIL
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
        && email.split_once('@').is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.') && !domain.contains('@'));
    if valid {
        Ok(normalize_email(email))
    } else {
        Err(unprocessable("email isn't a valid address"))
    }
}

/// The rule every caller shares, checked before the identity service sees the password.
fn checked_password(password: &str) -> Result<(), ApiError> {
    if password.is_empty() {
        return Err(unprocessable("password is required"));
    }
    if !strong_password(password) {
        return Err(weak_password());
    }
    Ok(())
}

fn weak_password() -> ApiError {
    ApiError::new(422, ErrorCode::WeakPassword, "the password doesn't meet the password policy")
}

fn email_taken() -> ApiError {
    conflict(ErrorCode::EmailTaken, "another user of this project has this email")
}

fn row_error(err: UserRowError) -> ApiError {
    match err {
        UserRowError::EmailTaken => email_taken(),
        UserRowError::Constraint { message, constraint } => {
            ApiError::new(422, ErrorCode::ConstraintViolation, message)
                .with_extra("constraint", constraint.map_or(serde_json::Value::Null, serde_json::Value::String))
        }
        UserRowError::UnknownColumn(column) => unprocessable(&format!("the users table has no column {column:?}")),
        UserRowError::Unavailable(message) => {
            error!("users table: {message}");
            ApiError::new(503, ErrorCode::Unavailable, "the users table can't be reached; try again")
        }
    }
}

#[utoipa::path(
    get,
    path = "/v1/projects/{id}/users",
    operation_id = "listAppUsers",
    tag = "app-users",
    summary = "List a project's users",
    description = "50 per page. `search` matches part of the email, ignoring case.",
    params(
        ("id" = Uuid, Path, description = "Project id"),
        ("search" = Option<String>, Query, description = "Part of an email"),
        ("page_token" = Option<String>, Query, description = "`next_page_token` of the previous page"),
    ),
    responses(
        (status = 200, description = "A page of users", body = AppUserPage),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID, or the query is malformed", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 409, description = "`project_deleting`; `app_users_disabled`: app users aren't enabled", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: `page_token` isn't one this API issued", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`identity_unavailable`: the identity service can't be reached", body = ErrorEnvelope),
    ),
)]
async fn list_app_users<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let ctx = match ctx(&state, &principal, ep::LIST_APP_USERS.policy.access, id).await {
        Ok(ctx) => ctx,
        Err(err) => return err.into_response(),
    };
    let Query(ListQuery { search, page_token }) = match query {
        Ok(q) => q,
        Err(rej) => {
            return ApiError::invalid_request(400, format!("invalid query: {}", rej.body_text())).into_response();
        }
    };
    let offset = match page_token.as_deref() {
        None | Some("") => 0,
        Some(token) => match token.parse::<u32>() {
            Ok(offset) => offset,
            Err(_) => return unprocessable("page_token isn't valid").into_response(),
        },
    };
    let identity = match identity(&state) {
        Ok(identity) => identity,
        Err(err) => return err.into_response(),
    };
    let search = search.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let page = match search {
        Some(search) => identity.search_users(&ctx.pool, search, PAGE_SIZE, offset).await,
        None => identity.list_users(&ctx.pool, PAGE_SIZE, offset).await,
    };
    match page {
        Ok(page) => {
            let next = u64::from(offset) + page.users.len() as u64;
            Json(AppUserPage {
                users: page.users.iter().filter_map(AppUser::from_identity).collect(),
                next_page_token: (!page.users.is_empty() && next < page.total).then(|| next.to_string()),
            })
            .into_response()
        }
        Err(err) => identity_failed("list users", &err).into_response(),
    }
}

#[utoipa::path(
    post,
    path = "/v1/projects/{id}/users",
    operation_id = "createAppUser",
    tag = "app-users",
    summary = "Create a user",
    description = "Creates the identity user, then writes the user's row to the users table and commits. When the table refuses the row (its own constraints) the identity user is deleted again. Columns of your own get their defaults.",
    params(("id" = Uuid, Path, description = "Project id")),
    request_body = CreateAppUserRequest,
    responses(
        (status = 201, description = "The new user", body = AppUser),
        (status = 400, description = "`invalid_request`: the body isn't well-formed JSON, or a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project (another tenant's answers the same)", body = ErrorEnvelope),
        (status = 409, description = "`project_deleting`; `app_users_disabled`: app users aren't enabled; `email_taken`: another user has this email; `branch_not_ready`: the auth database's main branch has no compute", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: the email or password is missing or invalid; `weak_password`: the password doesn't meet the policy; `constraint_violation`: the users table refused the row (`constraint` names the constraint)", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs; `credentials_unavailable`: the stored sync credentials can't be decrypted", body = ErrorEnvelope),
        (status = 503, description = "`identity_unavailable`: the identity service can't be reached; `unavailable`: the users table can't be reached", body = ErrorEnvelope),
    ),
)]
async fn create_app_user<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(body): ApiJson<CreateAppUserRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    match create(&state, &principal, id, body).await {
        Ok(user) => (StatusCode::CREATED, Json(user)).into_response(),
        Err(err) => err.into_response(),
    }
}

/// Where the sync role reaches the auth database's main branch.
async fn sync_target<M, J>(state: &AppState<M, J>, project: &ProjectRecord) -> Result<SyncTarget, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let auth = read_auth(state, project).await?.ok_or_else(|| app_users_disabled(409))?;
    let main = load_main(state, auth.database_id)
        .await?
        .ok_or_else(|| conflict(ErrorCode::BranchNotReady, "the auth database has no main branch"))?;
    let sync = state.metadata.branch_auth_sync(main.id).await.map_err(|err| {
        error!("branch auth sync: {err}");
        ApiError::internal()
    })?;
    let Some(sync) = sync else {
        return Err(conflict(ErrorCode::BranchNotReady, "app users aren't set up on the main branch yet"));
    };
    let Some(endpoint) = main.endpoint_id.as_deref() else {
        return Err(conflict(ErrorCode::BranchNotReady, "the main branch is not ready"));
    };
    let password = open_role_password(&state.secrets, &main.id.to_string(), SYNC_ROLE, &sync.password_sealed)
        .map_err(|err| {
            error!(branch_id = %main.id, "decrypt the sync role's password: {err}");
            ApiError::new(
                500,
                ErrorCode::CredentialsUnavailable,
                "stored credentials can't be decrypted with the current secrets key; restore the original CHELABASE_SECRETS_KEY file",
            )
        })?;
    // T10 I3: the compute's control address over the same TLS as every other compute
    // connection (its certificate name, our client pair); no pair loaded is 503
    // `compute_tls_failed` (R-T10a), never a plaintext connection.
    let tls = sync_tls(run_tls(state, main.id, endpoint)?);
    let addr = match state.locator.postgres_addr(endpoint).await {
        Ok(Some(addr)) => addr,
        Ok(None) => return Err(ApiError::new(503, ErrorCode::Unavailable, "the main branch's compute isn't running; try again")),
        Err(err) => {
            warn!(branch_id = %main.id, "locate the compute: {err:#}");
            return Err(ApiError::new(503, ErrorCode::Unavailable, "the main branch's compute can't be reached; try again"));
        }
    };
    Ok(SyncTarget {
        branch_id: main.id.as_uuid(),
        host: addr.ip().to_string(),
        port: addr.port(),
        database: sync.pg_database,
        user: SYNC_ROLE.to_string(),
        password,
        schema: auth.schema_name,
        table: auth.table_name,
        // Its own client pair (`tls`) verifies the compute on the control network.
        server_name: None,
        tls,
    })
}

/// The sync's TLS for `tls`: the compute's certificate name, and the connector and fingerprint
/// of one snapshot of the client pair (T10 m3). `None` for a plain connection (tests).
fn sync_tls(tls: RunTls) -> Option<SyncTls> {
    match tls {
        RunTls::Plain => None,
        RunTls::Compute { server_name, client } => {
            let snapshot = client.snapshot();
            Some(SyncTls {
                server_name,
                connector: snapshot.connector(),
                fingerprint: snapshot.fingerprint(),
            })
        }
    }
}

async fn create<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    id: Uuid,
    body: CreateAppUserRequest,
) -> Result<AppUser, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let ctx = ctx(state, principal, ep::CREATE_APP_USER.policy.access, id).await?;
    let email = checked_email(&body.email)?;
    checked_password(&body.password)?;
    let verified = body.email_verified.unwrap_or(true);
    let identity = identity(state)?;
    // Resolved first: a branch that can't take the row makes no identity user.
    let target = sync_target(state, &ctx.project).await?;
    let main = BranchId(target.branch_id);
    // 1. The identity: the pool refuses an email it has, whatever the table knows so far.
    let new = NewUser { email: &email, password: Some(&body.password), name: None, email_verified: verified };
    let user = identity.create_user(&ctx.pool, &new).await.map_err(|err| identity_failed("create user", &err))?;
    // 2. The row and the commit; without the row the identity must not stay.
    let written = match user.id.parse::<Uuid>() {
        Ok(user_id) => {
            let row = NewUserRow { id: user_id, email: user.email.clone(), email_verified: user.email_verified, data: serde_json::Map::new() };
            match state.users_db.begin_insert(&target, &row).await {
                Ok(pending) => pending.commit().await.map_err(row_error),
                Err(err) => Err(row_error(err)),
            }
            .map(|()| user_id)
        }
        // A pool made before ids were UUIDs.
        Err(_) => {
            error!(user_id = %user.id, "the identity service made a user without a UUID id");
            Err(identity_unavailable())
        }
    };
    let user_id = match written {
        Ok(user_id) => user_id,
        Err(err) => {
            if let Err(del) = identity.delete_user(&ctx.pool, &user.id).await {
                error!(user_id = %user.id, "delete the identity user after a refused row: {del}");
            }
            return Err(err);
        }
    };
    publish_sync(state, &ctx.project, user_id, Some(main)).await;
    info!(request_id = %principal.request_id, project_id = %ctx.project.id, %user_id, "app user created");
    AppUser::from_identity(&user).ok_or_else(ApiError::internal)
}

#[utoipa::path(
    get,
    path = "/v1/projects/{id}/users/{user_id}",
    operation_id = "getAppUser",
    tag = "app-users",
    summary = "Get a user",
    params(("id" = Uuid, Path, description = "Project id"), ("user_id" = Uuid, Path, description = "User id")),
    responses(
        (status = 200, description = "The user", body = AppUser),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project or user (another project's user answers the same)", body = ErrorEnvelope),
        (status = 409, description = "`project_deleting`; `app_users_disabled`: app users aren't enabled", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`identity_unavailable`: the identity service can't be reached", body = ErrorEnvelope),
    ),
)]
async fn get_app_user<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, user_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let result = async {
        let ctx = ctx(&state, &principal, ep::GET_APP_USER.policy.access, id).await?;
        let user = user_in_pool(identity(&state)?, &ctx.pool, user_id).await?;
        AppUser::from_identity(&user).ok_or_else(ApiError::internal)
    }
    .await;
    match result {
        Ok(user) => Json(user).into_response(),
        Err(err) => err.into_response(),
    }
}

#[utoipa::path(
    patch,
    path = "/v1/projects/{id}/users/{user_id}",
    operation_id = "updateAppUser",
    tag = "app-users",
    summary = "Change a user's email",
    description = "Sending `email` ends the user's sessions, also when it's the address they already have, so a retry finishes a change whose answer was an error. A `5xx` for a new address may mean the address changed: send the same request again. `email_verified: true` alone marks the current address verified; a verified address can only become unverified with a new `email`. A password can't be changed here. The users table follows through the sync job.",
    params(("id" = Uuid, Path, description = "Project id"), ("user_id" = Uuid, Path, description = "User id")),
    request_body = UpdateAppUserRequest,
    responses(
        (status = 200, description = "The user after the change", body = AppUser),
        (status = 400, description = "`invalid_request`: the body isn't well-formed JSON, or a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project or user", body = ErrorEnvelope),
        (status = 409, description = "`project_deleting`; `app_users_disabled`: app users aren't enabled; `email_taken`: another user has this email", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: nothing to change, the email is invalid, an unknown field such as `password`, or `email_verified: false` without a new email for a verified address", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`identity_unavailable`: the identity service can't be reached", body = ErrorEnvelope),
    ),
)]
async fn update_app_user<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, user_id)): ApiPath<(Uuid, Uuid)>,
    ApiJson(body): ApiJson<UpdateAppUserRequest>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    match update(&state, &principal, id, user_id, body).await {
        Ok(user) => Json(user).into_response(),
        Err(err) => err.into_response(),
    }
}

async fn update<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    id: Uuid,
    user_id: Uuid,
    body: UpdateAppUserRequest,
) -> Result<AppUser, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let ctx = ctx(state, principal, ep::UPDATE_APP_USER.policy.access, id).await?;
    if body.email.is_none() && body.email_verified.is_none() {
        return Err(unprocessable("nothing to change: send email or email_verified"));
    }
    let sent_email = body.email.as_deref().map(checked_email).transpose()?;
    let identity = identity(state)?;
    let user = user_in_pool(identity, &ctx.pool, user_id).await?;
    let uid = user_id.to_string();
    // The address the user already has isn't a change.
    let new_email = sent_email.clone().filter(|email| *email != user.email);
    if new_email.is_none() && body.email_verified == Some(false) && user.email_verified {
        return Err(unprocessable(
            "a verified email can't be marked unverified without changing it; set a new email with email_verified false",
        ));
    }
    let verified_before = user.email_verified;
    let changed = if let Some(email) = &new_email {
        identity
            .set_email(&ctx.pool, &uid, email, body.email_verified.unwrap_or(true))
            .await
            .map_err(|err| identity_failed("set email", &err))?
    } else if body.email_verified == Some(true) && !user.email_verified {
        identity
            .set_email(&ctx.pool, &uid, &user.email, true)
            .await
            .map_err(|err| identity_failed("mark the email verified", &err))?
    } else {
        user
    };
    // Any request naming an address revokes, even the address the user already has: nothing
    // else tells a retry from a no-op, and a retry of a change whose revocation failed has to
    // finish it. A refused change returned above and ended nothing.
    let revoked = if sent_email.is_some() {
        revoke_sessions(state, &ctx, user_id).await
    } else {
        Ok(())
    };
    // The sync still runs when the revocation failed: the address did change.
    if new_email.is_some() || changed.email_verified != verified_before {
        publish_sync(state, &ctx.project, user_id, None).await;
    }
    revoked?;
    info!(request_id = %principal.request_id, project_id = %ctx.project.id, %user_id, "app user changed");
    AppUser::from_identity(&changed).ok_or_else(ApiError::internal)
}

#[utoipa::path(
    post,
    path = "/v1/projects/{id}/users/{user_id}/deactivate",
    operation_id = "deactivateAppUser",
    tag = "app-users",
    summary = "Deactivate a user",
    description = "Bans the user: they can't sign in, and their sessions end. The sessions are revoked first, so a `503` still ended them.",
    params(("id" = Uuid, Path, description = "Project id"), ("user_id" = Uuid, Path, description = "User id")),
    responses(
        (status = 200, description = "The user, `inactive`", body = AppUser),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project or user", body = ErrorEnvelope),
        (status = 409, description = "`project_deleting`; `app_users_disabled`: app users aren't enabled; `conflict`: the user is already inactive", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`identity_unavailable`: the identity service can't be reached", body = ErrorEnvelope),
    ),
)]
async fn deactivate_app_user<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, user_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    match set_active(&state, &principal, id, user_id, false).await {
        Ok(user) => Json(user).into_response(),
        Err(err) => err.into_response(),
    }
}

#[utoipa::path(
    post,
    path = "/v1/projects/{id}/users/{user_id}/reactivate",
    operation_id = "reactivateAppUser",
    tag = "app-users",
    summary = "Reactivate a user",
    params(("id" = Uuid, Path, description = "Project id"), ("user_id" = Uuid, Path, description = "User id")),
    responses(
        (status = 200, description = "The user, `active`", body = AppUser),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project or user", body = ErrorEnvelope),
        (status = 409, description = "`project_deleting`; `app_users_disabled`: app users aren't enabled; `conflict`: the user is already active", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`identity_unavailable`: the identity service can't be reached", body = ErrorEnvelope),
    ),
)]
async fn reactivate_app_user<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, user_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    match set_active(&state, &principal, id, user_id, true).await {
        Ok(user) => Json(user).into_response(),
        Err(err) => err.into_response(),
    }
}

async fn set_active<M, J>(
    state: &AppState<M, J>,
    principal: &AuthPrincipal,
    id: Uuid,
    user_id: Uuid,
    active: bool,
) -> Result<AppUser, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let ctx = ctx(state, principal, if active { ep::REACTIVATE_APP_USER } else { ep::DEACTIVATE_APP_USER }.policy.access, id).await?;
    // Before the identity service is asked, or even known to be there, so a deactivate that
    // can't reach it still ends the user's refresh tokens.
    if !active {
        revoke_app_sessions(state, &ctx.project, user_id).await?;
    }
    let identity = identity(state)?;
    let uid = user_id.to_string();
    // Already in the state asked for is a 409.
    let user = user_in_pool(identity, &ctx.pool, user_id).await?;
    if user.banned != active {
        let state = if active { "active" } else { "inactive" };
        return Err(conflict(ErrorCode::Conflict, format!("the user is already {state}")));
    }
    // A ban ends the identity sessions too.
    let done = if active { identity.unban(&ctx.pool, &uid).await } else { identity.ban(&ctx.pool, &uid, None).await };
    let user = done.map_err(|err| identity_failed("change the user's state", &err))?;
    publish_sync(state, &ctx.project, user_id, None).await;
    info!(request_id = %principal.request_id, project_id = %ctx.project.id, %user_id, active, "app user state changed");
    AppUser::from_identity(&user).ok_or_else(ApiError::internal)
}

#[utoipa::path(
    get,
    path = "/v1/projects/{id}/users/{user_id}/sessions",
    operation_id = "listAppUserSessions",
    tag = "app-users",
    summary = "List a user's live sessions",
    params(("id" = Uuid, Path, description = "Project id"), ("user_id" = Uuid, Path, description = "User id")),
    responses(
        (status = 200, description = "The user's sessions that aren't revoked, oldest first", body = Vec<AppUserSession>),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project or user", body = ErrorEnvelope),
        (status = 409, description = "`project_deleting`; `app_users_disabled`: app users aren't enabled", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`identity_unavailable`: the identity service can't be reached", body = ErrorEnvelope),
    ),
)]
async fn list_app_user_sessions<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, user_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let result = async {
        let ctx = ctx(&state, &principal, ep::LIST_APP_USER_SESSIONS.policy.access, id).await?;
        user_in_pool(identity(&state)?, &ctx.pool, user_id).await?;
        state.metadata.list_user_sessions(ctx.project.id, user_id).await.map_err(|err| {
            error!("list user sessions: {err}");
            ApiError::internal()
        })
    }
    .await;
    match result {
        Ok(sessions) => Json(
            sessions
                .into_iter()
                .map(|s| AppUserSession {
                    id: s.id,
                    created_at: s.created_at,
                    last_used_at: s.last_used_at,
                    expires_at: s.expires_at,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(err) => err.into_response(),
    }
}

#[utoipa::path(
    delete,
    path = "/v1/projects/{id}/users/{user_id}/sessions",
    operation_id = "revokeAppUserSessions",
    tag = "app-users",
    summary = "Sign a user out everywhere",
    params(("id" = Uuid, Path, description = "Project id"), ("user_id" = Uuid, Path, description = "User id")),
    responses(
        (status = 204, description = "The user's sessions are revoked"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project or user", body = ErrorEnvelope),
        (status = 409, description = "`project_deleting`; `app_users_disabled`: app users aren't enabled", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`identity_unavailable`: the identity service can't be reached", body = ErrorEnvelope),
    ),
)]
async fn revoke_app_user_sessions<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, user_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let result = async {
        let ctx = ctx(&state, &principal, ep::REVOKE_APP_USER_SESSIONS.policy.access, id).await?;
        revoke_sessions(&state, &ctx, user_id).await
    }
    .await;
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => err.into_response(),
    }
}

#[utoipa::path(
    delete,
    path = "/v1/projects/{id}/users/{user_id}",
    operation_id = "deleteAppUser",
    tag = "app-users",
    summary = "Delete a user",
    description = "Ends the user's sessions and deletes the identity user; the users table's row follows the project's `on_delete` setting. The sessions are revoked first, so a `503` still ended them.",
    params(("id" = Uuid, Path, description = "Project id"), ("user_id" = Uuid, Path, description = "User id")),
    responses(
        (status = 204, description = "The user is deleted"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project or user", body = ErrorEnvelope),
        (status = 409, description = "`project_deleting`; `app_users_disabled`: app users aren't enabled", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`identity_unavailable`: the identity service can't be reached", body = ErrorEnvelope),
    ),
)]
async fn delete_app_user<M, J>(
    State(state): State<Arc<AppState<M, J>>>,
    principal: AuthPrincipal,
    ApiPath((id, user_id)): ApiPath<(Uuid, Uuid)>,
) -> Response
where
    M: MetadataPort,
    J: JobPort,
{
    let result = async {
        let ctx = ctx(&state, &principal, ep::DELETE_APP_USER.policy.access, id).await?;
        revoke_app_sessions(&state, &ctx.project, user_id).await?;
        let deleted = identity(&state)?.delete_user(&ctx.pool, &user_id.to_string()).await;
        // Not in the pool: an earlier delete may have lost its answer or its sync, and
        // reconcile won't remove the row where most of the table would go. The job repairs
        // that; for an id that never was a user it changes nothing.
        if matches!(deleted, Ok(()) | Err(IdentityError::NotFound(Missing::User))) {
            publish_sync(&state, &ctx.project, user_id, None).await;
        }
        deleted.map_err(|err| identity_failed("delete user", &err))?;
        info!(request_id = %principal.request_id, project_id = %ctx.project.id, %user_id, "app user deleted");
        Ok::<(), ApiError>(())
    }
    .await;
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => err.into_response(),
    }
}
