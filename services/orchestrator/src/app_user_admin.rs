//! Managing a project's app users from Studio: list, create, change, deactivate and delete.
//! Zitadel holds the identity; the project's users table is written first on create and is
//! kept in step by the worker's sync job afterwards.

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
use chelabase_zitadel::client::{EmailMatch, NewUser, UserState, ZError, ZUser, Zitadel};
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
const MAX_PASSWORD: usize = 256;

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
    /// Deactivated: it can't sign in.
    Inactive,
    /// Locked after too many failed sign-ins.
    Locked,
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
    fn from_zitadel(user: &ZUser) -> Option<Self> {
        Some(Self {
            id: user.id.parse().ok()?,
            email: user.email.clone(),
            email_verified: user.email_verified,
            state: match user.state {
                UserState::Active => AppUserState::Active,
                UserState::Locked => AppUserState::Locked,
                UserState::Inactive | UserState::Other => AppUserState::Inactive,
            },
            created_at: user.created_at.unwrap_or_else(Utc::now),
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
    /// on the current address (false sends the user a verification code).
    #[serde(default)]
    pub email_verified: Option<bool>,
    /// A new password; the user's sessions end.
    #[serde(default)]
    pub password: Option<String>,
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

pub(crate) fn zitadel_unavailable() -> ApiError {
    ApiError::new(503, ErrorCode::Unavailable, "the user service isn't configured")
}

/// A Zitadel failure the route didn't map itself. Only an unreachable Zitadel, or one
/// refusing Chelabase's own credential, is `503`: a precondition refusal (the user is already in
/// that state, an unchanged value) is `409 conflict`, a refused value `422`, an unknown user
/// `404`. Zitadel's message is an error id, logged and never shown.
pub(crate) fn zitadel_failed(what: &str, err: &ZError) -> ApiError {
    match err {
        ZError::Rejected { status, message } => {
            warn!("zitadel refused: {what}: {err}");
            match *status {
                404 => ApiError::not_found("user not found"),
                401 | 403 => {
                    error!("zitadel refused Chelabase's credential: {what}: {err}");
                    ApiError::new(503, ErrorCode::Unavailable, "the user service can't be reached; try again")
                }
                409 => conflict(ErrorCode::Conflict, "the user service refused the change: it conflicts with the user's current state"),
                s if is_state_precondition(s, message) => {
                    conflict(ErrorCode::Conflict, "the user service refused the change: it conflicts with the user's current state")
                }
                _ => unprocessable("the user service refused the value"),
            }
        }
        ZError::Unavailable(_) => {
            error!("zitadel: {what}: {err}");
            ApiError::new(503, ErrorCode::Unavailable, "the user service can't be reached; try again")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rejected(status: u16, message: &str) -> ZError {
        ZError::Rejected { status, message: message.into() }
    }

    /// Only an unreachable Zitadel (or one refusing Chelabase's own credential) is `503`.
    #[test]
    fn zitadel_refusals_map_by_kind() {
        let cases = [
            (rejected(400, "Errors.User.Email.NotChanged (COMMAND-Uch5e)"), 409, ErrorCode::Conflict),
            (rejected(400, "Errors.User.AlreadyInactive"), 409, ErrorCode::Conflict),
            (rejected(412, "something"), 409, ErrorCode::Conflict),
            (rejected(409, "Errors.User.AlreadyExists"), 409, ErrorCode::Conflict),
            (rejected(404, "Errors.User.NotFound"), 404, ErrorCode::NotFound),
            (rejected(400, "Errors.User.Email.Invalid"), 422, ErrorCode::InvalidRequest),
            (rejected(401, ""), 503, ErrorCode::Unavailable),
            (rejected(403, "Errors.PermissionDenied"), 503, ErrorCode::Unavailable),
            (ZError::Unavailable("timeout".into()), 503, ErrorCode::Unavailable),
        ];
        for (err, status, code) in cases {
            let api = zitadel_failed("test", &err);
            assert_eq!((api.status, api.code), (status, code), "{err}");
        }
    }
}

/// A project whose app users are enabled, with its Zitadel org.
struct Ctx {
    project: ProjectRecord,
    org: String,
}

async fn ctx<M, J>(state: &AppState<M, J>, principal: &AuthPrincipal, access: Access, id: Uuid) -> Result<Ctx, ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let project = load_live_project(state, principal, access, id).await?;
    let auth = match read_auth_checked(state, principal, access, &project).await? {
        Some(auth) if auth.status == AppUsersStatus::Enabled => auth,
        _ => return Err(app_users_disabled(409)),
    };
    let Some(org) = auth.zitadel_org_id else {
        return Err(app_users_disabled(409));
    };
    if state.zitadel.is_none() {
        return Err(zitadel_unavailable());
    }
    Ok(Ctx { project, org })
}

fn zitadel<M, J>(state: &AppState<M, J>) -> Result<&Zitadel, ApiError> {
    state.zitadel.as_ref().ok_or_else(zitadel_unavailable)
}

/// The user, when it is in the project's org.
async fn user_in_org(z: &Zitadel, org: &str, user: Uuid) -> Result<ZUser, ApiError> {
    match z.get_user(&user.to_string()).await {
        Ok(Some(u)) if u.org_id == org => Ok(u),
        Ok(_) => Err(ApiError::not_found("user not found")),
        Err(err) => Err(zitadel_failed("get user", &err)),
    }
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

/// Ends the user's sessions in the database, then in Zitadel (best effort).
async fn revoke_sessions<M, J>(state: &AppState<M, J>, z: &Zitadel, project: &ProjectRecord, user: Uuid) -> Result<(), ApiError>
where
    M: MetadataPort,
    J: JobPort,
{
    let revoked = state.metadata.revoke_user_sessions(project.id, user, None, None).await.map_err(|err| {
        error!("revoke user sessions: {err}");
        ApiError::internal()
    })?;
    for session in revoked {
        if let Err(err) = z.delete_session_admin(&session.zitadel_session_id).await {
            warn!(%user, "end zitadel session: {err}");
        }
    }
    Ok(())
}

fn checked_email(email: &str) -> Result<String, ApiError> {
    let email = email.trim();
    let valid = !email.is_empty()
        && email.len() <= MAX_EMAIL
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
        && email.split_once('@').is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.') && !domain.contains('@'));
    if valid {
        Ok(email.to_string())
    } else {
        Err(unprocessable("email isn't a valid address"))
    }
}

fn checked_password(password: &str) -> Result<(), ApiError> {
    if password.is_empty() || password.len() > MAX_PASSWORD {
        return Err(unprocessable("password must be 1 to 256 bytes"));
    }
    Ok(())
}

/// Zitadel's refusal of a password: its password policy error ids all contain this.
fn is_password_policy(message: &str) -> bool {
    message.contains("PasswordComplexityPolicy") || message.contains("Password.Invalid") || message.contains("Errors.User.Password")
}

/// A precondition refusal: the user is already in the state asked for, or the value is
/// unchanged (Zitadel answers `FailedPrecondition` as HTTP 400, so the error id tells).
fn is_state_precondition(status: u16, message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    status == 412
        || m.contains("already")
        || m.contains("notinactive")
        || m.contains("notactive")
        || m.contains("notchanged")
        || m.contains("precondition")
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
        (status = 503, description = "`unavailable`: the user service can't be reached", body = ErrorEnvelope),
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
        Some(token) => match token.parse::<u64>() {
            Ok(offset) => offset,
            Err(_) => return unprocessable("page_token isn't valid").into_response(),
        },
    };
    let z = match zitadel(&state) {
        Ok(z) => z,
        Err(err) => return err.into_response(),
    };
    let search = search.as_deref().map(str::trim).filter(|s| !s.is_empty());
    match z.list_users(&ctx.org, search, offset, PAGE_SIZE).await {
        Ok(page) => Json(AppUserPage {
            users: page.users.iter().filter_map(AppUser::from_zitadel).collect(),
            next_page_token: page.next_offset.map(|o| o.to_string()),
        })
        .into_response(),
        Err(err) => zitadel_failed("list users", &err).into_response(),
    }
}

#[utoipa::path(
    post,
    path = "/v1/projects/{id}/users",
    operation_id = "createAppUser",
    tag = "app-users",
    summary = "Create a user",
    description = "Writes the user's row to the users table first (the table's own constraints can refuse it), then creates the Zitadel user, then commits. Columns of your own get their defaults.",
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
        (status = 503, description = "`unavailable`: the user service or the users table can't be reached", body = ErrorEnvelope),
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
    let z = zitadel(state)?;
    let target = sync_target(state, &ctx.project).await?;
    let main = BranchId(target.branch_id);
    let user_id = Uuid::new_v4();
    // 1. The row, uncommitted: the table's constraints speak first.
    let row = NewUserRow { id: user_id, email: email.clone(), email_verified: verified, data: serde_json::Map::new() };
    let pending = state.users_db.begin_insert(&target, &row).await.map_err(row_error)?;
    // The table's unique index misses a user whose row hasn't reached main (sync lag, or an
    // app column that refused the sync's insert), so the org is asked too (N2).
    match z.find_user_in_org(&ctx.org, &email).await {
        Ok(EmailMatch::None) => {}
        Ok(EmailMatch::One(_) | EmailMatch::Many) => {
            pending.rollback().await;
            return Err(email_taken());
        }
        Err(err) => {
            pending.rollback().await;
            return Err(zitadel_failed("find user by email", &err));
        }
    }
    // 2. The identity.
    let user_id_text = user_id.to_string();
    let new = NewUser {
        org_id: &ctx.org,
        email: &email,
        given_name: email.split('@').next().unwrap_or("-"),
        family_name: "-",
        password: Some(&body.password),
        user_id: Some(&user_id_text),
        username: Some(&user_id_text),
        send_code: false,
        verified,
        url_template: "",
    };
    if let Err(err) = z.create_user(&new).await {
        pending.rollback().await;
        return Err(match err {
            ZError::Rejected { status: 409, .. } => email_taken(),
            ZError::Rejected { status: 400, message } if is_password_policy(&message) => weak_password(),
            err => zitadel_failed("create user", &err),
        });
    }
    // 3. Commit; without the row the identity must not stay.
    if let Err(err) = pending.commit().await {
        if let Err(del) = z.delete_user(&user_id_text).await {
            error!(%user_id, "delete the zitadel user after a failed commit: {del}");
        }
        return Err(row_error(err));
    }
    publish_sync(state, &ctx.project, user_id, Some(main)).await;
    info!(request_id = %principal.request_id, project_id = %ctx.project.id, %user_id, "app user created");
    Ok(AppUser {
        id: user_id,
        email,
        email_verified: verified,
        state: AppUserState::Active,
        created_at: Utc::now(),
    })
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
        (status = 503, description = "`unavailable`: the user service can't be reached", body = ErrorEnvelope),
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
        let user = user_in_org(zitadel(&state)?, &ctx.org, user_id).await?;
        AppUser::from_zitadel(&user).ok_or_else(ApiError::internal)
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
    summary = "Change a user's email or password",
    description = "A new password ends the user's sessions. `email_verified: true` alone marks the current address verified; a verified address can only become unverified with a new `email`. The users table follows through the sync job.",
    params(("id" = Uuid, Path, description = "Project id"), ("user_id" = Uuid, Path, description = "User id")),
    request_body = UpdateAppUserRequest,
    responses(
        (status = 200, description = "The user after the change", body = AppUser),
        (status = 400, description = "`invalid_request`: the body isn't well-formed JSON, or a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project or user", body = ErrorEnvelope),
        (status = 409, description = "`project_deleting`; `app_users_disabled`: app users aren't enabled; `email_taken`: another user has this email; `conflict`: the user service refused the change in the user's current state", body = ErrorEnvelope),
        (status = 413, description = "`too_large`: the body is over the 2 MB limit", body = ErrorEnvelope),
        (status = 415, description = "`invalid_request`: the request isn't `Content-Type: application/json`", body = ErrorEnvelope),
        (status = 422, description = "`invalid_request`: nothing to change, the email or password is invalid, or `email_verified: false` without a new email for a verified address; `weak_password`: the password doesn't meet the policy", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: the user service can't be reached", body = ErrorEnvelope),
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
    if body.email.is_none() && body.email_verified.is_none() && body.password.is_none() {
        return Err(unprocessable("nothing to change: send email, email_verified or password"));
    }
    let new_email = body.email.as_deref().map(checked_email).transpose()?;
    if let Some(password) = &body.password {
        checked_password(password)?;
    }
    let z = zitadel(state)?;
    let user = user_in_org(z, &ctx.org, user_id).await?;
    let uid = user_id.to_string();
    // The address the user already has isn't a change: Zitadel refuses setting it again.
    let new_email = new_email.filter(|email| *email != user.email);
    if let Some(email) = &new_email {
        match z.find_user_in_org(&ctx.org, email).await {
            Ok(EmailMatch::One(other)) if other.id != user.id => return Err(email_taken()),
            Ok(EmailMatch::Many) => return Err(email_taken()),
            Ok(_) => {}
            Err(err) => return Err(zitadel_failed("find user by email", &err)),
        }
    }
    // Zitadel can only drop the verification with a new address (it has no "unverify").
    if new_email.is_none() && body.email_verified == Some(false) && user.email_verified {
        return Err(unprocessable(
            "a verified email can't be marked unverified without changing it; set a new email with email_verified false",
        ));
    }
    // The password goes first: a refusal (weak password) leaves the user as it was.
    if let Some(password) = &body.password {
        match z.set_password_admin(&uid, password).await {
            Ok(()) => {}
            Err(ZError::Rejected { status: 400, message }) if is_password_policy(&message) => return Err(weak_password()),
            Err(err) => return Err(zitadel_failed("set password", &err)),
        }
        revoke_sessions(state, z, &ctx.project, user_id).await?;
    }
    if let Some(email) = &new_email {
        z.set_email(&uid, email, body.email_verified.unwrap_or(true), None)
            .await
            .map_err(|err| zitadel_failed("set email", &err))?;
    } else if body.email_verified == Some(true) && !user.email_verified {
        z.mark_email_verified(&uid, &user.email)
            .await
            .map_err(|err| zitadel_failed("mark the email verified", &err))?;
    }
    publish_sync(state, &ctx.project, user_id, None).await;
    info!(request_id = %principal.request_id, project_id = %ctx.project.id, %user_id, "app user changed");
    fresh_user(z, &uid).await
}

/// The user as Zitadel has it now.
async fn fresh_user(z: &Zitadel, user_id: &str) -> Result<AppUser, ApiError> {
    match z.get_user(user_id).await {
        Ok(Some(user)) => AppUser::from_zitadel(&user).ok_or_else(ApiError::internal),
        Ok(None) => Err(ApiError::not_found("user not found")),
        Err(err) => Err(zitadel_failed("get user", &err)),
    }
}

#[utoipa::path(
    post,
    path = "/v1/projects/{id}/users/{user_id}/deactivate",
    operation_id = "deactivateAppUser",
    tag = "app-users",
    summary = "Deactivate a user",
    description = "The user can't sign in, and their sessions end.",
    params(("id" = Uuid, Path, description = "Project id"), ("user_id" = Uuid, Path, description = "User id")),
    responses(
        (status = 200, description = "The user, `inactive`", body = AppUser),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project or user", body = ErrorEnvelope),
        (status = 409, description = "`project_deleting`; `app_users_disabled`: app users aren't enabled; `conflict`: the user is already inactive", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: the user service can't be reached", body = ErrorEnvelope),
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
        (status = 503, description = "`unavailable`: the user service can't be reached", body = ErrorEnvelope),
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
    let z = zitadel(state)?;
    user_in_org(z, &ctx.org, user_id).await?;
    let uid = user_id.to_string();
    let done = if active { z.reactivate_user(&uid).await } else { z.deactivate_user(&uid).await };
    match done {
        Ok(()) => {}
        Err(ZError::Rejected { status: 404, .. }) => return Err(ApiError::not_found("user not found")),
        Err(ZError::Rejected { status, message }) if is_state_precondition(status, &message) => {
            let state = if active { "active" } else { "inactive" };
            return Err(conflict(ErrorCode::Conflict, format!("the user is already {state}")));
        }
        Err(err) => return Err(zitadel_failed("change the user's state", &err)),
    }
    if !active {
        revoke_sessions(state, z, &ctx.project, user_id).await?;
    }
    publish_sync(state, &ctx.project, user_id, None).await;
    info!(request_id = %principal.request_id, project_id = %ctx.project.id, %user_id, active, "app user state changed");
    fresh_user(z, &uid).await
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
        (status = 503, description = "`unavailable`: the user service can't be reached", body = ErrorEnvelope),
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
        user_in_org(zitadel(&state)?, &ctx.org, user_id).await?;
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
        (status = 503, description = "`unavailable`: the user service can't be reached", body = ErrorEnvelope),
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
        let z = zitadel(&state)?;
        user_in_org(z, &ctx.org, user_id).await?;
        revoke_sessions(&state, z, &ctx.project, user_id).await
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
    description = "Deletes the Zitadel user and ends their sessions; the users table's row follows the project's `on_delete` setting.",
    params(("id" = Uuid, Path, description = "Project id"), ("user_id" = Uuid, Path, description = "User id")),
    responses(
        (status = 204, description = "The user is deleted"),
        (status = 400, description = "`invalid_request`: a path id isn't a valid UUID", body = ErrorEnvelope),
        (status = 401, description = "`unauthorized`: no credential, or a bad token or internal identity", body = ErrorEnvelope),
        (status = 403, description = "`tenant_not_bound`: the caller's org isn't bound to a Chelabase tenant", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no such project or user", body = ErrorEnvelope),
        (status = 409, description = "`project_deleting`; `app_users_disabled`: app users aren't enabled", body = ErrorEnvelope),
        (status = 500, description = "`internal`: unexpected; details only in the logs", body = ErrorEnvelope),
        (status = 503, description = "`unavailable`: the user service can't be reached", body = ErrorEnvelope),
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
        let z = zitadel(&state)?;
        user_in_org(z, &ctx.org, user_id).await?;
        z.delete_user(&user_id.to_string()).await.map_err(|err| zitadel_failed("delete user", &err))?;
        // The user is gone: a failed revoke mustn't hide that or skip the row's sync.
        if let Err(err) = revoke_sessions(&state, z, &ctx.project, user_id).await {
            error!(%user_id, "revoke sessions after delete: {}", err.message);
        }
        publish_sync(&state, &ctx.project, user_id, None).await;
        info!(request_id = %principal.request_id, project_id = %ctx.project.id, %user_id, "app user deleted");
        Ok::<(), ApiError>(())
    }
    .await;
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => err.into_response(),
    }
}
