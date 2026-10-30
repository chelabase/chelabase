//! The `/auth/v1/app/*` handlers.

use super::identity::{AppCaller, unavailable as not_configured};
use super::internal::{
    AppBranchInfo, CreateSession, InternalError, RefreshRequest, RevokeUserSessions, SyncRequest,
};
use super::{AppUsers, CODE_TEMPLATE};
use crate::app::{AppState, SIGN_UP_EXTRA};
use crate::guard::{email_tag, pad};
use crate::zitadel::{EmailMatch, NewUser, UserState, ZError, ZUser};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chelabase_auth::app_token::{APP_ISSUER, AppClaims, app_audience};
use chelabase_query::app_users::{NewUserRow, UserRowError};
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use chelabase_types::api_extract::ApiJson;
use chelabase_zitadel::rules::{normalize_email, strong_password};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;
use uuid::Uuid;

type S = State<Arc<AppState>>;

pub const TAG: &str = "app-users";

pub(crate) fn api_router() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(app_sign_up))
        .routes(routes!(app_sign_in))
        .routes(routes!(app_refresh))
        .routes(routes!(app_sign_out))
        .routes(routes!(app_user))
        .routes(routes!(app_verify_email))
        .routes(routes!(app_resend_code))
        .routes(routes!(app_password_reset))
        .routes(routes!(app_password_reset_confirm))
        .routes(routes!(app_change_email))
        .routes(routes!(app_change_password))
        .routes(routes!(app_jwks))
}

/// An app user as the flows show them.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[schema(as = AppUser)]
pub struct AppUserView {
    pub id: Uuid,
    pub email: String,
    pub email_verified: bool,
    pub created_at: DateTime<Utc>,
}

/// A signed-in session: a short-lived access token and the refresh token that renews it.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[schema(as = AppSession)]
pub struct SessionResponse {
    /// Send it as the `Chelabase-User` header.
    pub access_token: String,
    /// Always `bearer`.
    pub token_type: &'static str,
    /// Seconds until `access_token` expires.
    pub expires_in: u64,
    pub refresh_token: String,
    pub user: AppUserView,
}

/// A sign-up that waits for the emailed code: no tokens yet.
#[derive(Debug, Clone, Serialize, ToSchema)]
#[schema(as = AppSignUpPending)]
pub struct SignUpPending {
    pub user: AppUserView,
    /// Always `true`.
    pub verification_required: bool,
}

/// Sign-up's answer: a session (verification off) or a pending sign-up (verification on).
#[allow(dead_code)]
#[derive(Serialize, ToSchema)]
#[serde(untagged)]
enum AppSignUpResult {
    Session(SessionResponse),
    Pending(SignUpPending),
}

#[derive(Deserialize, ToSchema)]
pub struct AppSignUpBody {
    email: String,
    password: String,
    /// Values for the users table's own columns. Only columns with
    /// `GRANT INSERT (column) ON <users table> TO authenticated` are accepted.
    #[serde(default)]
    #[schema(value_type = Option<Object>)]
    data: Option<Value>,
}

#[derive(Deserialize, ToSchema)]
pub struct AppSignInBody {
    email: String,
    password: String,
}

#[derive(Deserialize, ToSchema)]
pub struct AppRefreshBody {
    refresh_token: String,
}

#[derive(Deserialize, ToSchema)]
pub struct AppSignOutBody {
    /// Needed only without a `Chelabase-User` token.
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Deserialize, ToSchema)]
pub struct AppVerifyEmailBody {
    email: String,
    code: String,
}

#[derive(Deserialize, ToSchema)]
pub struct AppEmailBody {
    email: String,
}

#[derive(Deserialize, ToSchema)]
pub struct AppPasswordResetConfirmBody {
    email: String,
    code: String,
    password: String,
}

#[derive(Deserialize, ToSchema)]
pub struct AppChangePasswordBody {
    current_password: String,
    password: String,
}

/// The app token signing keys (JWKS, RFC 7517).
#[allow(dead_code)]
#[derive(Serialize, ToSchema)]
struct AppJwks {
    keys: Vec<AppJwk>,
}

/// One Ed25519 public key.
#[allow(dead_code)]
#[derive(Serialize, ToSchema)]
struct AppJwk {
    kty: String,
    crv: String,
    x: String,
    kid: String,
    alg: String,
    #[serde(rename = "use")]
    use_: String,
}

#[utoipa::path(
    post,
    path = "/auth/v1/app/sign-up",
    operation_id = "appSignUp",
    tag = TAG,
    description = "Creates an app user and their users-table row. With email verification off, signs them in; with it on, emails a code and returns no tokens.",
    request_body = AppSignUpBody,
    responses(
        (status = 201, description = "Signed up: a session (verification off) or a pending sign-up (verification on)", body = AppSignUpResult),
        (status = 400, description = "The body is malformed: `invalid_request`", body = ErrorEnvelope),
        (status = 401, description = "No valid public key: `unauthorized`", body = ErrorEnvelope),
        (status = 409, description = "App users are off (`app_users_disabled`), or the email is taken and verification is off (`email_taken`)", body = ErrorEnvelope),
        (status = 413, description = "The body is too large: `too_large`", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON: `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "Invalid input (`invalid_request`), a weak password (`weak_password`), a `data` column sign-up can't set (`unknown_column`) or a table constraint (`constraint_violation`)", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "Zitadel, the orchestrator or the database is unavailable: `unavailable`", body = ErrorEnvelope),
    ),
)]
async fn app_sign_up(
    State(s): S,
    caller: AppCaller,
    ApiJson(b): ApiJson<AppSignUpBody>,
) -> Response {
    let start = Instant::now();
    // Its own task: a client that disconnects mid-way would otherwise drop the future
    // between Zitadel's create and the commit, leaving a Zitadel user with no row, no
    // cleanup delete and no sync.
    let state = s.clone();
    let task = tokio::spawn(async move {
        sign_up(&state, &caller, b)
            .await
            .unwrap_or_else(IntoResponse::into_response)
    });
    let answer = task.await.unwrap_or_else(|e| {
        warn!(error = %e, "the sign-up task failed");
        fail(503, ErrorCode::Unavailable, text::UNAVAILABLE).into_response()
    });
    pad(start, s.config.min_response + SIGN_UP_EXTRA).await;
    answer
}

#[utoipa::path(
    post,
    path = "/auth/v1/app/sign-in",
    operation_id = "appSignIn",
    tag = TAG,
    description = "Signs an app user in with email and password.",
    request_body = AppSignInBody,
    responses(
        (status = 200, description = "Signed in", body = SessionResponse),
        (status = 400, description = "The body is malformed: `invalid_request`", body = ErrorEnvelope),
        (status = 401, description = "Wrong email or password (`invalid_credentials`), or no valid public key (`unauthorized`)", body = ErrorEnvelope),
        (status = 403, description = "The email isn't verified (`email_not_verified`) or the user is deactivated or locked (`user_inactive`)", body = ErrorEnvelope),
        (status = 409, description = "App users are off: `app_users_disabled`", body = ErrorEnvelope),
        (status = 413, description = "The body is too large: `too_large`", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON: `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "The body has the wrong fields: `invalid_request`", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "Zitadel or the orchestrator is unavailable: `unavailable`", body = ErrorEnvelope),
    ),
)]
async fn app_sign_in(
    State(s): S,
    caller: AppCaller,
    ApiJson(b): ApiJson<AppSignInBody>,
) -> Response {
    let start = Instant::now();
    let answer = sign_in(&s, &caller, b)
        .await
        .unwrap_or_else(IntoResponse::into_response);
    pad(start, s.config.min_response).await;
    answer
}

#[utoipa::path(
    post,
    path = "/auth/v1/app/refresh",
    operation_id = "appRefresh",
    tag = TAG,
    description = "Exchanges a refresh token for a new session. The old refresh token stops working (presenting it again later revokes the session).",
    request_body = AppRefreshBody,
    responses(
        (status = 200, description = "The new session", body = SessionResponse),
        (status = 400, description = "The body is malformed: `invalid_request`", body = ErrorEnvelope),
        (status = 401, description = "The refresh token is invalid (`refresh_token_invalid`) or was used already (`refresh_token_reused`; the session is revoked), or no valid public key (`unauthorized`)", body = ErrorEnvelope),
        (status = 403, description = "The user is deactivated or locked: `user_inactive`", body = ErrorEnvelope),
        (status = 409, description = "App users are off: `app_users_disabled`", body = ErrorEnvelope),
        (status = 413, description = "The body is too large: `too_large`", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON: `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "The body has the wrong fields: `invalid_request`", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "Zitadel or the orchestrator is unavailable: `unavailable`", body = ErrorEnvelope),
    ),
)]
async fn app_refresh(
    State(s): S,
    caller: AppCaller,
    ApiJson(b): ApiJson<AppRefreshBody>,
) -> Response {
    refresh(&s, &caller, b)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

#[utoipa::path(
    post,
    path = "/auth/v1/app/sign-out",
    operation_id = "appSignOut",
    tag = TAG,
    description = "Ends the session of the `Chelabase-User` token, or, without one, the session of the refresh token in the body.",
    request_body = AppSignOutBody,
    responses(
        (status = 204, description = "Signed out"),
        (status = 400, description = "The body is malformed: `invalid_request`", body = ErrorEnvelope),
        (status = 401, description = "Neither a user token nor a refresh token (`user_token_invalid`), or no valid public key (`unauthorized`)", body = ErrorEnvelope),
        (status = 413, description = "The body is too large: `too_large`", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON: `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "The body has the wrong fields: `invalid_request`", body = ErrorEnvelope),
        (status = 503, description = "The orchestrator is unavailable: `unavailable`", body = ErrorEnvelope),
    ),
)]
async fn app_sign_out(
    State(s): S,
    caller: AppCaller,
    ApiJson(b): ApiJson<AppSignOutBody>,
) -> Response {
    sign_out(&s, &caller, b)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

#[utoipa::path(
    get,
    path = "/auth/v1/app/user",
    operation_id = "appGetUser",
    tag = TAG,
    description = "The signed-in app user (needs the `Chelabase-User` token).",
    responses(
        (status = 200, description = "The user", body = AppUserView),
        (status = 401, description = "No user token (`user_token_invalid`), or no valid public key (`unauthorized`)", body = ErrorEnvelope),
        (status = 409, description = "App users are off: `app_users_disabled`", body = ErrorEnvelope),
        (status = 503, description = "Zitadel or the orchestrator is unavailable: `unavailable`", body = ErrorEnvelope),
    ),
)]
async fn app_user(State(s): S, caller: AppCaller) -> Response {
    user(&s, &caller)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

#[utoipa::path(
    post,
    path = "/auth/v1/app/verify-email",
    operation_id = "appVerifyEmail",
    tag = TAG,
    description = "Marks the email verified with the emailed code.",
    request_body = AppVerifyEmailBody,
    responses(
        (status = 204, description = "Verified"),
        (status = 400, description = "The body is malformed: `invalid_request`", body = ErrorEnvelope),
        (status = 401, description = "No valid public key: `unauthorized`", body = ErrorEnvelope),
        (status = 409, description = "App users are off: `app_users_disabled`", body = ErrorEnvelope),
        (status = 413, description = "The body is too large: `too_large`", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON: `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "The code is wrong or expired (`invalid_code`), or the body has the wrong fields (`invalid_request`)", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "Zitadel or the orchestrator is unavailable: `unavailable`", body = ErrorEnvelope),
    ),
)]
async fn app_verify_email(
    State(s): S,
    caller: AppCaller,
    ApiJson(b): ApiJson<AppVerifyEmailBody>,
) -> Response {
    let start = Instant::now();
    let answer = verify_email(&s, &caller, b)
        .await
        .unwrap_or_else(IntoResponse::into_response);
    pad(start, s.config.min_response).await;
    answer
}

#[utoipa::path(
    post,
    path = "/auth/v1/app/resend-code",
    operation_id = "appResendCode",
    tag = TAG,
    description = "Emails the verification code again. Answers the same whether or not the account exists.",
    request_body = AppEmailBody,
    responses(
        (status = 204, description = "Done"),
        (status = 400, description = "The body is malformed: `invalid_request`", body = ErrorEnvelope),
        (status = 401, description = "No valid public key: `unauthorized`", body = ErrorEnvelope),
        (status = 409, description = "App users are off: `app_users_disabled`", body = ErrorEnvelope),
        (status = 413, description = "The body is too large: `too_large`", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON: `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "The body has the wrong fields: `invalid_request`", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "The orchestrator is unavailable: `unavailable`", body = ErrorEnvelope),
    ),
)]
async fn app_resend_code(
    State(s): S,
    caller: AppCaller,
    ApiJson(b): ApiJson<AppEmailBody>,
) -> Response {
    let start = Instant::now();
    let answer = resend_code(&s, &caller, b)
        .await
        .unwrap_or_else(IntoResponse::into_response);
    pad(start, s.config.min_response).await;
    answer
}

#[utoipa::path(
    post,
    path = "/auth/v1/app/password-reset",
    operation_id = "appRequestPasswordReset",
    tag = TAG,
    description = "Emails a password-reset code. Answers the same whether or not the account exists.",
    request_body = AppEmailBody,
    responses(
        (status = 204, description = "Done"),
        (status = 400, description = "The body is malformed: `invalid_request`", body = ErrorEnvelope),
        (status = 401, description = "No valid public key: `unauthorized`", body = ErrorEnvelope),
        (status = 409, description = "App users are off: `app_users_disabled`", body = ErrorEnvelope),
        (status = 413, description = "The body is too large: `too_large`", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON: `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "The body has the wrong fields: `invalid_request`", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "The orchestrator is unavailable: `unavailable`", body = ErrorEnvelope),
    ),
)]
async fn app_password_reset(
    State(s): S,
    caller: AppCaller,
    ApiJson(b): ApiJson<AppEmailBody>,
) -> Response {
    let start = Instant::now();
    let answer = password_reset(&s, &caller, b)
        .await
        .unwrap_or_else(IntoResponse::into_response);
    pad(start, s.config.min_response).await;
    answer
}

#[utoipa::path(
    post,
    path = "/auth/v1/app/password-reset/confirm",
    operation_id = "appConfirmPasswordReset",
    tag = TAG,
    description = "Sets a new password with the emailed reset code and ends every session of the user.",
    request_body = AppPasswordResetConfirmBody,
    responses(
        (status = 204, description = "Password set"),
        (status = 400, description = "The body is malformed: `invalid_request`", body = ErrorEnvelope),
        (status = 401, description = "No valid public key: `unauthorized`", body = ErrorEnvelope),
        (status = 409, description = "App users are off: `app_users_disabled`", body = ErrorEnvelope),
        (status = 413, description = "The body is too large: `too_large`", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON: `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "The code is wrong (`invalid_code`), the password is weak (`weak_password`), or the body has the wrong fields (`invalid_request`)", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "Zitadel or the orchestrator is unavailable: `unavailable`", body = ErrorEnvelope),
    ),
)]
async fn app_password_reset_confirm(
    State(s): S,
    caller: AppCaller,
    ApiJson(b): ApiJson<AppPasswordResetConfirmBody>,
) -> Response {
    let start = Instant::now();
    let answer = password_reset_confirm(&s, &caller, b)
        .await
        .unwrap_or_else(IntoResponse::into_response);
    pad(start, s.config.min_response).await;
    answer
}

#[utoipa::path(
    post,
    path = "/auth/v1/app/change-email",
    operation_id = "appChangeEmail",
    tag = TAG,
    description = "Changes the signed-in user's email (needs the `Chelabase-User` token). With verification on, a code is emailed and the email is unverified until `verify-email`.",
    request_body = AppEmailBody,
    responses(
        (status = 204, description = "Changed"),
        (status = 400, description = "The body is malformed: `invalid_request`", body = ErrorEnvelope),
        (status = 401, description = "No user token (`user_token_invalid`), or no valid public key (`unauthorized`)", body = ErrorEnvelope),
        (status = 409, description = "The email is taken (`email_taken`), or app users are off (`app_users_disabled`)", body = ErrorEnvelope),
        (status = 413, description = "The body is too large: `too_large`", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON: `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "Invalid input: `invalid_request`", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "Zitadel or the orchestrator is unavailable: `unavailable`", body = ErrorEnvelope),
    ),
)]
async fn app_change_email(
    State(s): S,
    caller: AppCaller,
    ApiJson(b): ApiJson<AppEmailBody>,
) -> Response {
    change_email(&s, &caller, b)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

#[utoipa::path(
    post,
    path = "/auth/v1/app/change-password",
    operation_id = "appChangePassword",
    tag = TAG,
    description = "Changes the signed-in user's password (needs the `Chelabase-User` token) and returns a new session; every other session of the user ends.",
    request_body = AppChangePasswordBody,
    responses(
        (status = 200, description = "The new session", body = SessionResponse),
        (status = 400, description = "The body is malformed: `invalid_request`", body = ErrorEnvelope),
        (status = 401, description = "The current password is wrong (`invalid_credentials`), no user token (`user_token_invalid`), or no valid public key (`unauthorized`)", body = ErrorEnvelope),
        (status = 409, description = "App users are off: `app_users_disabled`", body = ErrorEnvelope),
        (status = 413, description = "The body is too large: `too_large`", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON: `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "The new password is weak (`weak_password`), or the body has the wrong fields (`invalid_request`)", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "Zitadel or the orchestrator is unavailable: `unavailable`", body = ErrorEnvelope),
    ),
)]
async fn app_change_password(
    State(s): S,
    caller: AppCaller,
    ApiJson(b): ApiJson<AppChangePasswordBody>,
) -> Response {
    let start = Instant::now();
    let answer = change_password(&s, &caller, b)
        .await
        .unwrap_or_else(IntoResponse::into_response);
    pad(start, s.config.min_response).await;
    answer
}

#[utoipa::path(
    get,
    path = "/auth/v1/app/jwks",
    operation_id = "appJwks",
    tag = TAG,
    description = "The public keys app access tokens are signed with, so an app's own server can verify them. Needs no key.",
    responses(
        (status = 200, description = "The keys", body = AppJwks),
        (status = 503, description = "App users are not configured: `unavailable`", body = ErrorEnvelope),
    ),
)]
async fn app_jwks(State(s): S) -> Response {
    match s.app.as_ref() {
        Some(app) => Json(app.signer.jwks()).into_response(),
        None => not_configured().into_response(),
    }
}

// ---- the flows -----------------------------------------------------------------------------

const MINUTE: Duration = Duration::from_secs(60);
const HOUR: Duration = Duration::from_secs(3600);

mod text {
    pub const BAD_CREDENTIALS: &str = "Email or password is incorrect";
    pub const BAD_CODE: &str = "Code is invalid or expired";
    pub const WEAK_PASSWORD: &str =
        "Use at least 8 characters, with upper- and lower-case letters, a number and a symbol";
    pub const TOO_MANY: &str = "Too many attempts, try again later";
    pub const UNAVAILABLE: &str = "Sign-in is unavailable, try again";
    pub const ACCOUNT_EXISTS: &str = "An account with this email already exists";
    pub const DISABLED: &str = "App users are not enabled for this project";
    pub const NOT_VERIFIED: &str = "Verify your email first";
    pub const INACTIVE: &str = "This account is deactivated or locked";
    pub const NO_USER: &str = "This needs a signed-in user (the Chelabase-User header)";
    pub const BAD_EMAIL: &str = "email is not a valid address";
}

type Flow<T = Response> = Result<T, ApiError>;

fn fail(status: u16, code: ErrorCode, message: impl Into<String>) -> ApiError {
    ApiError::new(status, code, message)
}

fn zitadel_down(e: &ZError) -> ApiError {
    warn!(error = %e, "zitadel call failed");
    fail(503, ErrorCode::Unavailable, text::UNAVAILABLE)
}

fn internal_down(e: &InternalError) -> ApiError {
    warn!(error = %e, "orchestrator internal call failed");
    fail(503, ErrorCode::Unavailable, text::UNAVAILABLE)
}

fn disabled() -> ApiError {
    fail(409, ErrorCode::AppUsersDisabled, text::DISABLED)
}

fn refused() -> ApiError {
    fail(401, ErrorCode::InvalidCredentials, text::BAD_CREDENTIALS)
}

fn bad_code() -> ApiError {
    fail(422, ErrorCode::InvalidCode, text::BAD_CODE)
}

fn weak_password() -> ApiError {
    fail(422, ErrorCode::WeakPassword, text::WEAK_PASSWORD)
}

fn no_user() -> ApiError {
    fail(401, ErrorCode::UserTokenInvalid, text::NO_USER)
}

fn inactive(state: UserState) -> bool {
    matches!(state, UserState::Inactive | UserState::Locked)
}

/// The extractor answered 503 already when app users aren't configured.
fn app(s: &AppState) -> &AppUsers {
    s.app
        .as_ref()
        .expect("AppCaller checks app users are configured")
}

/// A short shape check; Zitadel has the final word.
fn valid_email(email: &str) -> bool {
    email.len() <= 254
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
        && matches!(email.split_once('@'), Some((local, domain))
            if !local.is_empty() && domain.contains('.') && !domain.contains('@')
                && !domain.starts_with('.') && !domain.ends_with('.'))
}

/// Counts one attempt against each `(flow, key kind, value, limit, window)`, stopping at the
/// first over its limit. A server-vouched call (#55) skips the `"ip"` checks: its address is
/// the app server's, shared by all its users; the `"email"` and `"user"` checks still count.
fn rate_limit(
    s: &AppState,
    caller: &AppCaller,
    checks: &[(&str, &str, &str, u32, Duration)],
) -> Flow<()> {
    let applies = |kind: &str| !(caller.server_vouched && kind == "ip");
    let ok = checks
        .iter()
        .filter(|(_, kind, ..)| applies(kind))
        .all(|(flow, kind, value, limit, window)| {
            let key = format!("app:{}:{flow}:{kind}:{value}", caller.project_id);
            s.limiter.allow(&key, *limit, *window)
        });
    if ok {
        Ok(())
    } else {
        Err(fail(429, ErrorCode::RateLimited, text::TOO_MANY))
    }
}

/// The branch's app-user settings (cached for 30 s); 409 unless app users are `enabled`.
async fn branch_info(app: &AppUsers, caller: &AppCaller) -> Flow<Arc<AppBranchInfo>> {
    let info = match app.branches.get(&caller.branch_id).await {
        Some(info) => info,
        None => match app.internal.branch_info(caller.branch_id).await {
            Ok(Some(info)) => {
                let info = Arc::new(info);
                if info.status == "enabled" {
                    app.branches.insert(caller.branch_id, info.clone()).await;
                }
                info
            }
            Ok(None) => return Err(disabled()),
            Err(e) => return Err(internal_down(&e)),
        },
    };
    if info.status != "enabled"
        || info.project_id != caller.project_id
        || info.zitadel_org_id.is_none()
    {
        return Err(disabled());
    }
    Ok(info)
}

fn org(info: &AppBranchInfo) -> &str {
    info.zitadel_org_id.as_deref().unwrap_or_default()
}

/// The org's user with this email, when it has a Chelabase (uuid) id (P15). An email several
/// users share (made outside Chelabase's checks) names no one; it is logged (N2).
async fn find_user(s: &AppState, info: &AppBranchInfo, email: &str) -> Flow<Option<(Uuid, ZUser)>> {
    match s.zitadel.find_user_in_org(org(info), email).await {
        Ok(EmailMatch::One(u)) => Ok(Uuid::parse_str(&u.id).ok().map(|id| (id, u))),
        Ok(EmailMatch::Many) => {
            warn!(email = %email_tag(email), "several users of the org share this email; it signs no one in");
            Ok(None)
        }
        Ok(EmailMatch::None) => Ok(None),
        Err(e) => Err(zitadel_down(&e)),
    }
}

fn view(id: Uuid, u: &ZUser) -> AppUserView {
    AppUserView {
        id,
        email: u.email.clone(),
        email_verified: u.email_verified,
        created_at: u.created_at.unwrap_or_else(Utc::now),
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn session_response(
    app: &AppUsers,
    project: Uuid,
    sid: Uuid,
    refresh_token: String,
    user: AppUserView,
) -> Flow<SessionResponse> {
    let iat = now_secs();
    let ttl = app.access_ttl.as_secs();
    let claims = AppClaims {
        iss: APP_ISSUER.to_string(),
        aud: app_audience(project),
        sub: user.id,
        sid,
        project,
        email: user.email.clone(),
        email_verified: user.email_verified,
        role: "authenticated".to_string(),
        iat,
        exp: iat + ttl,
    };
    let access_token = app.signer.sign(&claims).map_err(|e| {
        warn!(error = %e, "signing an app token failed");
        fail(503, ErrorCode::Unavailable, text::UNAVAILABLE)
    })?;
    Ok(SessionResponse {
        access_token,
        token_type: "bearer",
        expires_in: ttl,
        refresh_token,
        user,
    })
}

enum SignInError {
    /// Wrong password (or a user Zitadel refuses a session for).
    Credentials,
    Answer(ApiError),
}

impl From<ApiError> for SignInError {
    fn from(r: ApiError) -> Self {
        Self::Answer(r)
    }
}

/// Checks the password with a new Zitadel session, then the user's state, then opens the app
/// session. A just-created user may take a moment to be visible to sessions (`fresh`).
async fn open_session(
    s: &AppState,
    info: &AppBranchInfo,
    user_id: Uuid,
    password: &str,
    fresh: bool,
) -> Result<(SessionResponse, Uuid), SignInError> {
    let app = app(s);
    let zid = user_id.to_string();
    let mut attempt = 0;
    let session = loop {
        match s.zitadel.create_session(&zid, password).await {
            Ok(session) => break session,
            Err(ZError::Rejected { status: 404, .. }) if fresh && attempt < 10 => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(ZError::Rejected { .. }) => return Err(SignInError::Credentials),
            Err(e) => return Err(zitadel_down(&e).into()),
        }
    };
    let end = |why: ApiError| async {
        if let Err(e) = s.zitadel.delete_session(&session).await {
            warn!(error = %e, "ending the Zitadel session failed");
        }
        SignInError::Answer(why)
    };
    // Only now, with the password checked, may the answer say anything about the account.
    let current = match s.zitadel.get_user(&zid).await {
        Ok(Some(u)) if u.org_id == org(info) => u,
        Ok(_) => return Err(end(refused()).await),
        Err(e) => return Err(end(zitadel_down(&e)).await),
    };
    if inactive(current.state) {
        return Err(end(fail(403, ErrorCode::UserInactive, text::INACTIVE)).await);
    }
    if info.email_verification && !current.email_verified {
        return Err(end(fail(403, ErrorCode::EmailNotVerified, text::NOT_VERIFIED)).await);
    }
    let created = match app
        .internal
        .create_session(&CreateSession {
            project_id: info.project_id,
            user_id,
            zitadel_session_id: session.id.clone(),
        })
        .await
    {
        Ok(c) => c,
        Err(e) => return Err(end(internal_down(&e)).await),
    };
    let response = session_response(
        app,
        info.project_id,
        created.session_id,
        created.refresh_token,
        view(user_id, &current),
    )?;
    Ok((response, created.session_id))
}

async fn publish_sync(app: &AppUsers, req: SyncRequest) {
    if let Err(e) = app.internal.sync(&req).await {
        // The reconcile heals a missed sync.
        warn!(error = %e, user = %req.user_id, "publishing the app user sync failed");
    }
}

fn pending(id: Uuid, email: &str) -> Response {
    let body = SignUpPending {
        user: AppUserView {
            id,
            email: email.to_string(),
            email_verified: false,
            created_at: Utc::now(),
        },
        verification_required: true,
    };
    (StatusCode::CREATED, Json(body)).into_response()
}

/// A sign-up for a taken email (P12). With verification off, the user is told; with it on,
/// the answer looks like a new sign-up and the owner gets a reset code as a notice.
async fn taken(s: &AppState, info: &AppBranchInfo, email: &str, owner: Option<String>) -> Flow {
    if !info.email_verification {
        info!(email = %email_tag(email), "app sign-up for a taken email");
        return Err(fail(409, ErrorCode::EmailTaken, text::ACCOUNT_EXISTS));
    }
    info!(email = %email_tag(email), "app sign-up for a taken email (decoy)");
    let owner = match owner {
        Some(id) => Some(id),
        None => s
            .zitadel
            .find_user_in_org(org(info), email)
            .await
            .ok()
            .and_then(|m| match m {
                EmailMatch::One(u) => Some(u.id),
                _ => None,
            }),
    };
    if let Some(id) = owner
        && let Err(e) = s.zitadel.send_password_reset_code(&id, CODE_TEMPLATE).await
    {
        warn!(error = %e, "sending the taken-email notice failed");
    }
    Ok(pending(Uuid::new_v4(), email))
}

fn row_error(e: UserRowError, info: &AppBranchInfo) -> ApiError {
    match e {
        UserRowError::UnknownColumn(c) => fail(
            422,
            ErrorCode::UnknownColumn,
            format!(
                "unknown column {c}: sign-up data may only name columns of {schema}.{table} \
                 that sign-up may set (GRANT INSERT ({c}) ON {schema}.{table} TO authenticated)",
                schema = info.schema,
                table = info.table,
            ),
        ),
        UserRowError::Constraint {
            message,
            constraint,
        } => {
            let mut e = ApiError::new(422, ErrorCode::ConstraintViolation, message);
            if let Some(c) = constraint {
                e = e.with_extra("constraint", json!(c));
            }
            e
        }
        // The caller answers a taken email itself (P12); this is only a fallback.
        UserRowError::EmailTaken => fail(409, ErrorCode::EmailTaken, text::ACCOUNT_EXISTS),
        UserRowError::Unavailable(m) => {
            warn!(error = %m, "the users table is unavailable");
            fail(503, ErrorCode::Unavailable, text::UNAVAILABLE)
        }
    }
}

async fn sign_up(s: &AppState, caller: &AppCaller, b: AppSignUpBody) -> Flow {
    let app = app(s);
    let email = normalize_email(&b.email);
    rate_limit(
        s,
        caller,
        &[
            ("sign-up", "ip", &caller.ip, 20, HOUR),
            ("sign-up", "email", &email_tag(&email), 5, HOUR),
        ],
    )?;
    if !valid_email(&email) {
        return Err(fail(422, ErrorCode::InvalidRequest, text::BAD_EMAIL));
    }
    if !strong_password(&b.password) {
        return Err(weak_password());
    }
    let data = match b.data {
        None | Some(Value::Null) => serde_json::Map::new(),
        Some(Value::Object(m)) => m,
        Some(_) => {
            return Err(fail(
                422,
                ErrorCode::InvalidRequest,
                "data must be an object of column values",
            ));
        }
    };
    let mut info = branch_info(app, caller).await?;
    let id = Uuid::new_v4();
    let row = NewUserRow {
        id,
        email: email.clone(),
        email_verified: !info.email_verification,
        data: data.clone(),
    };
    // The row first: a bad `data` value never reaches Zitadel (and is 422 whatever the email).
    let mut retried = false;
    let (pending_insert, target) = loop {
        let Some(target) = info.sync.clone() else {
            warn!(branch = %info.target_branch_id, "the users table's branch has no running compute");
            return Err(fail(503, ErrorCode::Unavailable, text::UNAVAILABLE));
        };
        match app.users_db.begin_insert(&target, &row).await {
            Ok(p) => break (p, target),
            Err(UserRowError::Unavailable(m)) if !retried => {
                // The compute may have moved or its password changed: ask again.
                warn!(error = %m, "the users table is unavailable; retrying with fresh settings");
                retried = true;
                app.branches.invalidate(&caller.branch_id).await;
                info = branch_info(app, caller).await?;
            }
            Err(UserRowError::EmailTaken) => return taken(s, &info, &email, None).await,
            Err(e) => return Err(row_error(e, &info)),
        }
    };
    let ev = info.email_verification;
    // Any user with this email counts, a non-uuid one made outside Chelabase too.
    match s.zitadel.find_user_in_org(org(&info), &email).await {
        Ok(EmailMatch::One(owner)) => {
            pending_insert.rollback().await;
            return taken(s, &info, &email, Some(owner.id)).await;
        }
        Ok(EmailMatch::Many) => {
            pending_insert.rollback().await;
            return taken(s, &info, &email, None).await;
        }
        Ok(EmailMatch::None) => {}
        Err(e) => {
            pending_insert.rollback().await;
            return Err(zitadel_down(&e));
        }
    }
    let zid = id.to_string();
    let given = email.split('@').next().unwrap_or("user");
    let given: String = given.chars().take(200).collect();
    let created = s
        .zitadel
        .create_user(&NewUser {
            org_id: org(&info),
            email: &email,
            given_name: &given,
            family_name: "-",
            password: Some(&b.password),
            user_id: Some(&zid),
            username: Some(&zid),
            send_code: ev,
            verified: !ev,
            url_template: CODE_TEMPLATE,
        })
        .await;
    match created {
        Ok(_) => {}
        Err(ZError::Rejected { status: 409, .. }) => {
            pending_insert.rollback().await;
            return taken(s, &info, &email, None).await;
        }
        Err(ZError::Rejected {
            status: 400,
            message,
        }) => {
            pending_insert.rollback().await;
            info!(%message, "zitadel refused the new app user");
            return Err(if policy_refusal(&message) {
                weak_password()
            } else {
                fail(
                    422,
                    ErrorCode::InvalidRequest,
                    "Zitadel refused this email or password",
                )
            });
        }
        Err(e) => {
            pending_insert.rollback().await;
            return Err(zitadel_down(&e));
        }
    }
    if let Err(e) = pending_insert.commit().await {
        warn!(error = %e, user = %id, "committing the app user's row failed; deleting the Zitadel user");
        if let Err(e) = app.zitadel_admin.delete_user(&zid).await {
            warn!(error = %e, user = %id, "deleting the Zitadel user after a failed commit failed");
        }
        return Err(fail(503, ErrorCode::Unavailable, text::UNAVAILABLE));
    }
    publish_sync(
        app,
        SyncRequest {
            project_id: info.project_id,
            user_id: id,
            data: (!data.is_empty()).then_some(data),
            origin_branch_id: Some(target.branch_id),
        },
    )
    .await;
    if ev {
        return Ok(pending(id, &email));
    }
    match open_session(s, &info, id, &b.password, true).await {
        Ok((session, _)) => Ok((StatusCode::CREATED, Json(session)).into_response()),
        Err(SignInError::Answer(r)) => Err(r),
        Err(SignInError::Credentials) => Err(fail(503, ErrorCode::Unavailable, text::UNAVAILABLE)),
    }
}

async fn sign_in(s: &AppState, caller: &AppCaller, b: AppSignInBody) -> Flow {
    let email = normalize_email(&b.email);
    // Per email only failures count, so nobody can use up a known account's attempts.
    let email_key = format!(
        "app:{}:sign-in:email:{}",
        caller.project_id,
        email_tag(&email)
    );
    rate_limit(s, caller, &[("sign-in", "ip", &caller.ip, 10, MINUTE)])?;
    if s.limiter.over(&email_key, 5, MINUTE) {
        return Err(fail(429, ErrorCode::RateLimited, text::TOO_MANY));
    }
    let wrong = || {
        s.limiter.hit(&email_key, MINUTE);
        refused()
    };
    let info = branch_info(app(s), caller).await?;
    let Some((user_id, _)) = find_user(s, &info, &email).await? else {
        return Err(wrong());
    };
    match open_session(s, &info, user_id, &b.password, false).await {
        Ok((session, _)) => Ok(Json(session).into_response()),
        Err(SignInError::Credentials) => {
            info!(email = %email_tag(&email), "app sign-in refused");
            Err(wrong())
        }
        Err(SignInError::Answer(r)) => Err(r),
    }
}

/// Checks the token's user, then rotates the refresh token.
///
/// Invariant (#57): clients drop their cookies on `refresh_token_invalid`,
/// `refresh_token_reused` and `user_inactive`, so a refresh revokes the session or answers one
/// of them only on a definitive verdict: the orchestrator's 401 `refresh_token_invalid` or
/// `refresh_token_reused`, Zitadel's definite "no such user" (a 404) or a user of another org,
/// or an inactive user. Every other upstream outcome (any other orchestrator status or code,
/// a Zitadel 5xx or other refusal, a timeout, an unreadable or partial success answer) answers
/// 503 `unavailable` and revokes nothing.
///
/// And (#59): nothing rotates before the user check. The orchestrator's peek names the token's
/// user without writing, and says whether the token is the current one or the previous one
/// inside its 10 s grace.
/// - A current token: Zitadel is asked about the user first, and only then does `refresh`
///   rotate. A fault before the rotation (peek, Zitadel, or the lookup taking longer than
///   [`REFRESH_USER_LOOKUP`](super::REFRESH_USER_LOOKUP)) answers 503 and leaves the token the client holds current, so
///   a later retry with it works, however late, unless another request rotates it meanwhile.
/// - A grace token goes to `refresh` first, then the user is checked, as before #59: its
///   grace answer writes nothing, and holding it through a slow Zitadel could outlast the
///   grace and turn it into reuse (#59 audit B1). A fault after that answers 503; the client
///   keeps a grace token, which works again only until its grace ends.
/// - A peek verdict goes on to `refresh`, so the orchestrator applies its own rules (it
///   revokes on reuse); a user that `refresh` names but peek didn't check is checked after it.
///
/// A request that peeked a current token just before a parallel one rotated it reaches
/// `refresh` holding a grace token. [`REFRESH_USER_LOOKUP`](super::REFRESH_USER_LOOKUP) caps
/// the lookup at half of [`APP_REFRESH_GRACE`](chelabase_auth::app_token::APP_REFRESH_GRACE)
/// or less (4 s of 10 s, checked at compile time), so it gets the grace answer provided the
/// peek's reply, the `refresh` call and its row lock take under the remaining ~6 s. If they
/// take longer, the grace has ended: `refresh` answers `refresh_token_reused` and revokes the
/// session, an accepted residual (#59 second audit).
/// `refresh_ends_a_session_only_on_a_definitive_verdict` in `tests/app_flows.rs` checks every
/// combination against a model of this.
async fn refresh(s: &AppState, caller: &AppCaller, b: AppRefreshBody) -> Flow {
    let app = app(s);
    rate_limit(s, caller, &[("refresh", "ip", &caller.ip, 120, MINUTE)])?;
    let info = branch_info(app, caller).await?;
    let request = RefreshRequest {
        project_id: info.project_id,
        refresh_token: b.refresh_token,
    };
    let peeked = match app.internal.peek(&request).await {
        Ok(p) => Some(p),
        Err(InternalError::Refused { status: 401, code })
            if code == "refresh_token_invalid" || code == "refresh_token_reused" =>
        {
            None
        }
        Err(e) => return Err(internal_down(&e)),
    };
    let checked = match peeked {
        Some(p) if !p.grace => Some((
            p.user_id,
            current_user(s, &info, p.user_id, p.session_id).await?,
        )),
        _ => None,
    };
    let refreshed = match app.internal.refresh(&request).await {
        Ok(r) => r,
        Err(InternalError::Refused { status: 401, code }) if code == "refresh_token_reused" => {
            return Err(fail(
                401,
                ErrorCode::RefreshTokenReused,
                "This refresh token was used already; the session is ended, sign in again",
            ));
        }
        // Only the orchestrator's own verdict ends the session: clients drop their cookies on
        // it. A bare 401 (a mismatched internal bearer), a 404 or a 422 is our fault, not the
        // token's, so it answers 503 and the client keeps the session.
        Err(InternalError::Refused { status: 401, code }) if code == "refresh_token_invalid" => {
            return Err(fail(
                401,
                ErrorCode::RefreshTokenInvalid,
                "The refresh token is invalid or expired",
            ));
        }
        Err(e) => return Err(internal_down(&e)),
    };
    let current = match checked {
        Some((user, current)) if user == refreshed.user_id => current,
        // A grace token, a peek verdict that refresh then didn't give, or another user: check
        // the user `refresh` answered for.
        _ => current_user(s, &info, refreshed.user_id, refreshed.session_id).await?,
    };
    let session = session_response(
        app,
        info.project_id,
        refreshed.session_id,
        refreshed.refresh_token,
        view(refreshed.user_id, &current),
    )?;
    Ok(Json(session).into_response())
}

/// The session's user as Zitadel has it now, when they may still refresh. Revokes the session
/// and answers 401 `refresh_token_invalid` when Zitadel definitely has no such user (a 404) or
/// they belong to another org, and 403 `user_inactive` when inactive; any Zitadel fault,
/// including no answer within [`REFRESH_USER_LOOKUP`](super::REFRESH_USER_LOOKUP), answers 503 and revokes nothing.
async fn current_user(
    s: &AppState,
    info: &AppBranchInfo,
    user_id: Uuid,
    session_id: Uuid,
) -> Flow<ZUser> {
    let app = app(s);
    let revoke = || async {
        if let Err(e) = app.internal.revoke(session_id, info.project_id).await {
            warn!(error = %e, "revoking the app session failed");
        }
    };
    // `None` is a definite 404; an unreadable answer is an error (503), never "gone".
    let id = user_id.to_string();
    let answer = tokio::time::timeout(app.refresh_user_lookup, s.zitadel.get_user(&id))
        .await
        .unwrap_or_else(|_| Err(ZError::Unavailable("the user lookup timed out".into())));
    let current = match answer {
        Ok(Some(u)) if u.org_id == org(info) => u,
        Ok(_) => {
            revoke().await;
            return Err(fail(
                401,
                ErrorCode::RefreshTokenInvalid,
                "The refresh token is invalid or expired",
            ));
        }
        Err(e) => return Err(zitadel_down(&e)),
    };
    if inactive(current.state) {
        revoke().await;
        return Err(fail(403, ErrorCode::UserInactive, text::INACTIVE));
    }
    Ok(current)
}

async fn sign_out(s: &AppState, caller: &AppCaller, b: AppSignOutBody) -> Flow {
    let app = app(s);
    let done = if let Some(user) = &caller.user {
        app.internal.revoke(user.sid, caller.project_id).await
    } else if let Some(token) = b.refresh_token.filter(|t| !t.trim().is_empty()) {
        app.internal
            .revoke_by_token(&RefreshRequest {
                project_id: caller.project_id,
                refresh_token: token,
            })
            .await
    } else {
        return Err(fail(
            401,
            ErrorCode::UserTokenInvalid,
            "Sign-out needs the user token (Chelabase-User) or a refresh_token",
        ));
    };
    done.map_err(|e| internal_down(&e))?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn user(s: &AppState, caller: &AppCaller) -> Flow {
    let Some(claims) = &caller.user else {
        return Err(no_user());
    };
    let info = branch_info(app(s), caller).await?;
    match s.zitadel.get_user(&claims.sub.to_string()).await {
        Ok(Some(u)) if u.org_id == org(&info) => Ok(Json(view(claims.sub, &u)).into_response()),
        Ok(_) => Err(no_user()),
        Err(e) => Err(zitadel_down(&e)),
    }
}

async fn verify_email(s: &AppState, caller: &AppCaller, b: AppVerifyEmailBody) -> Flow {
    let app = app(s);
    let email = normalize_email(&b.email);
    rate_limit(
        s,
        caller,
        &[
            ("verify", "ip", &caller.ip, 10, MINUTE),
            ("verify", "email", &email_tag(&email), 10, HOUR),
        ],
    )?;
    let info = branch_info(app, caller).await?;
    let Some((user_id, _)) = find_user(s, &info, &email).await? else {
        return Err(bad_code());
    };
    match s
        .zitadel
        .verify_email(&user_id.to_string(), b.code.trim())
        .await
    {
        Ok(()) => {}
        Err(ZError::Rejected { .. }) => return Err(bad_code()),
        Err(e) => return Err(zitadel_down(&e)),
    }
    publish_sync(
        app,
        SyncRequest {
            project_id: info.project_id,
            user_id,
            data: None,
            origin_branch_id: None,
        },
    )
    .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn resend_code(s: &AppState, caller: &AppCaller, b: AppEmailBody) -> Flow {
    let email = normalize_email(&b.email);
    rate_limit(
        s,
        caller,
        &[
            ("resend", "ip", &caller.ip, 20, HOUR),
            ("resend", "email", &email_tag(&email), 5, HOUR),
        ],
    )?;
    let info = branch_info(app(s), caller).await?;
    match find_user(s, &info, &email).await {
        Ok(Some((id, u))) if !u.email_verified => {
            if let Err(e) = s.zitadel.resend_code(&id.to_string(), CODE_TEMPLATE).await {
                warn!(error = %e, "resending the app verification code failed");
            }
        }
        Ok(_) => info!(email = %email_tag(&email), "app resend-code with nothing to send"),
        Err(_) => {}
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn password_reset(s: &AppState, caller: &AppCaller, b: AppEmailBody) -> Flow {
    let email = normalize_email(&b.email);
    rate_limit(
        s,
        caller,
        &[
            ("reset", "ip", &caller.ip, 20, HOUR),
            ("reset", "email", &email_tag(&email), 5, HOUR),
        ],
    )?;
    let info = branch_info(app(s), caller).await?;
    match find_user(s, &info, &email).await {
        Ok(Some((id, _))) => {
            if let Err(e) = s
                .zitadel
                .send_password_reset_code(&id.to_string(), CODE_TEMPLATE)
                .await
            {
                warn!(error = %e, "sending the app password reset failed");
            }
        }
        Ok(None) => info!(email = %email_tag(&email), "app password reset for an unknown email"),
        Err(_) => {}
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn password_reset_confirm(
    s: &AppState,
    caller: &AppCaller,
    b: AppPasswordResetConfirmBody,
) -> Flow {
    let app = app(s);
    let email = normalize_email(&b.email);
    rate_limit(
        s,
        caller,
        &[
            ("confirm", "ip", &caller.ip, 10, MINUTE),
            ("confirm", "email", &email_tag(&email), 10, HOUR),
        ],
    )?;
    // Before Zitadel sees the email, so this answer can't tell accounts apart.
    if !strong_password(&b.password) {
        return Err(weak_password());
    }
    let info = branch_info(app, caller).await?;
    let Some((user_id, _)) = find_user(s, &info, &email).await? else {
        return Err(bad_code());
    };
    match s
        .zitadel
        .set_password(&user_id.to_string(), b.code.trim(), &b.password)
        .await
    {
        Ok(()) => {}
        Err(ZError::Rejected { message, .. }) if policy_refusal(&message) => {
            return Err(weak_password());
        }
        // A wrong code, or a user gone meanwhile: the same answer as an unknown email.
        Err(ZError::Rejected { .. }) => return Err(bad_code()),
        Err(e) => return Err(zitadel_down(&e)),
    }
    // The password-changed webhook revokes them too (P11); this doesn't wait for it.
    if let Err(e) = app
        .internal
        .revoke_user_sessions(
            user_id,
            &RevokeUserSessions {
                project_id: info.project_id,
                keep_session_id: None,
                created_before: None,
            },
        )
        .await
    {
        warn!(error = %e, user = %user_id, "revoking the app sessions after a reset failed");
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn change_email(s: &AppState, caller: &AppCaller, b: AppEmailBody) -> Flow {
    let app = app(s);
    let Some(claims) = &caller.user else {
        return Err(no_user());
    };
    let email = normalize_email(&b.email);
    rate_limit(
        s,
        caller,
        &[
            ("change-email", "ip", &caller.ip, 10, MINUTE),
            ("change-email", "user", &claims.sub.to_string(), 5, HOUR),
        ],
    )?;
    if !valid_email(&email) {
        return Err(fail(422, ErrorCode::InvalidRequest, text::BAD_EMAIL));
    }
    let info = branch_info(app, caller).await?;
    match s.zitadel.find_user_in_org(org(&info), &email).await {
        Ok(EmailMatch::Many) => {
            return Err(fail(409, ErrorCode::EmailTaken, text::ACCOUNT_EXISTS));
        }
        Ok(EmailMatch::One(other)) if other.id != claims.sub.to_string() => {
            return Err(fail(409, ErrorCode::EmailTaken, text::ACCOUNT_EXISTS));
        }
        Ok(_) => {}
        Err(e) => return Err(zitadel_down(&e)),
    }
    let ev = info.email_verification;
    match s
        .zitadel
        .set_email(&claims.sub.to_string(), &email, !ev, Some(CODE_TEMPLATE))
        .await
    {
        Ok(()) => {}
        Err(ZError::Rejected { status: 404, .. }) => return Err(no_user()),
        Err(ZError::Rejected { status: 409, .. }) => {
            return Err(fail(409, ErrorCode::EmailTaken, text::ACCOUNT_EXISTS));
        }
        Err(ZError::Rejected { message, .. }) => {
            info!(%message, "zitadel refused the new email");
            return Err(fail(422, ErrorCode::InvalidRequest, text::BAD_EMAIL));
        }
        Err(e) => return Err(zitadel_down(&e)),
    }
    publish_sync(
        app,
        SyncRequest {
            project_id: info.project_id,
            user_id: claims.sub,
            data: None,
            origin_branch_id: None,
        },
    )
    .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn change_password(s: &AppState, caller: &AppCaller, b: AppChangePasswordBody) -> Flow {
    let app = app(s);
    let Some(claims) = &caller.user else {
        return Err(no_user());
    };
    let sub = claims.sub.to_string();
    rate_limit(
        s,
        caller,
        &[
            ("change-password", "ip", &caller.ip, 10, MINUTE),
            ("change-password", "user", &sub, 5, MINUTE),
        ],
    )?;
    if !strong_password(&b.password) {
        return Err(weak_password());
    }
    let info = branch_info(app, caller).await?;
    // The new session below needs a verified email when verification is on: refuse first,
    // so a 403 never means "changed anyway".
    if info.email_verification {
        match s.zitadel.get_user(&sub).await {
            Ok(Some(u)) if u.email_verified => {}
            Ok(Some(_)) => {
                return Err(fail(403, ErrorCode::EmailNotVerified, text::NOT_VERIFIED));
            }
            Ok(None) => return Err(no_user()),
            Err(e) => return Err(zitadel_down(&e)),
        }
    }
    match s
        .zitadel
        .change_password(&sub, &b.current_password, &b.password)
        .await
    {
        Ok(()) => {}
        Err(ZError::Rejected { message, .. }) if policy_refusal(&message) => {
            return Err(weak_password());
        }
        // A wrong current password, or a user Zitadel no longer has.
        Err(ZError::Rejected { .. }) => return Err(refused()),
        Err(e) => return Err(zitadel_down(&e)),
    }
    let (session, sid) = match open_session(s, &info, claims.sub, &b.password, false).await {
        Ok(opened) => opened,
        Err(SignInError::Answer(r)) => return Err(r),
        Err(SignInError::Credentials) => {
            return Err(fail(503, ErrorCode::Unavailable, text::UNAVAILABLE));
        }
    };
    // The webhook revokes sessions older than the change too (P11); this keeps the new one.
    if let Err(e) = app
        .internal
        .revoke_user_sessions(
            claims.sub,
            &RevokeUserSessions {
                project_id: info.project_id,
                keep_session_id: Some(sid),
                created_before: None,
            },
        )
        .await
    {
        warn!(error = %e, user = %claims.sub, "revoking the other app sessions failed");
    }
    Ok(Json(session).into_response())
}

/// Zitadel's password-policy refusals (`Errors.User.PasswordComplexityPolicy.*`).
fn policy_refusal(message: &str) -> bool {
    message.contains("PasswordComplexityPolicy")
}

#[cfg(test)]
mod tests {
    use super::policy_refusal;

    #[test]
    fn password_policy_rejections_are_told_apart() {
        assert!(policy_refusal(
            "Errors.User.PasswordComplexityPolicy.MinLength"
        ));
        assert!(policy_refusal(
            "Errors.User.PasswordComplexityPolicy.HasUpper"
        ));
        assert!(!policy_refusal("Errors.User.Password.Invalid"));
        assert!(!policy_refusal("Errors.User.Code.Invalid"));
        assert!(!policy_refusal("Errors.User.NotFound"));
    }
}
