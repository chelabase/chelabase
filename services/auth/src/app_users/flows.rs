//! The `/auth/v1/app/*` handlers.

use super::identity::{AppCaller, unavailable as not_configured};
use super::internal::{
    AppBranchInfo, CreateSession, InternalError, RefreshRequest, RevokeUserSessions, SyncRequest,
};
use super::AppUsers;
use crate::app::{AppState, SIGN_UP_EXTRA, usable_password};
use crate::guard::{Slot, email_tag, pad};
use crate::session::may_issue_code;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chelabase_auth::app_token::{APP_ISSUER, AppClaims, app_audience};
use chelabase_identity::rules::normalize_email;
use chelabase_identity::{
    CodeKind, CodeRequest, Delivery, IdentityError, Missing, NewUser, Pool, SignedIn, User,
};
use chelabase_query::app_users::{NewUserRow, UserRowError};
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use chelabase_types::api_extract::ApiJson;
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
        (status = 503, description = "The identity service, the orchestrator or the database is unavailable: `unavailable`", body = ErrorEnvelope),
    ),
)]
async fn app_sign_up(
    State(s): S,
    caller: AppCaller,
    ApiJson(b): ApiJson<AppSignUpBody>,
) -> Response {
    let start = Instant::now();
    // Its own task: a client that disconnects mid-way would otherwise drop the future
    // between identity's create and the commit, leaving an identity user with no row, no
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
        (status = 503, description = "The identity service or the orchestrator is unavailable: `unavailable`", body = ErrorEnvelope),
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
        (status = 503, description = "The identity service or the orchestrator is unavailable: `unavailable`", body = ErrorEnvelope),
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
        (status = 503, description = "The identity service or the orchestrator is unavailable: `unavailable`", body = ErrorEnvelope),
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
        (status = 503, description = "The identity service or the orchestrator is unavailable: `unavailable`", body = ErrorEnvelope),
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
        (status = 503, description = "The identity service or the orchestrator is unavailable: `unavailable`", body = ErrorEnvelope),
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
        (status = 409, description = "The email is taken (`email_taken`), it is the user's email already (`conflict`), or app users are off (`app_users_disabled`)", body = ErrorEnvelope),
        (status = 413, description = "The body is too large: `too_large`", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON: `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "Invalid input: `invalid_request`", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "The identity service or the orchestrator is unavailable: `unavailable`", body = ErrorEnvelope),
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
        (status = 503, description = "The identity service or the orchestrator is unavailable: `unavailable`", body = ErrorEnvelope),
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
    pub const EMAIL_NOT_CHANGED: &str = "This is already the account's email";
    pub const REFUSED_INPUT: &str = "This email or password was refused";
    pub const REFRESH_INVALID: &str = "The refresh token is invalid or expired";
}

type Flow<T = Response> = Result<T, ApiError>;

fn fail(status: u16, code: ErrorCode, message: impl Into<String>) -> ApiError {
    ApiError::new(status, code, message)
}

fn unavailable() -> ApiError {
    fail(503, ErrorCode::Unavailable, text::UNAVAILABLE)
}

fn identity_down(e: &IdentityError) -> ApiError {
    warn!(error = %e, "identity call failed");
    unavailable()
}

fn internal_down(e: &InternalError) -> ApiError {
    warn!(error = %e, "orchestrator internal call failed");
    unavailable()
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

fn inactive() -> ApiError {
    fail(403, ErrorCode::UserInactive, text::INACTIVE)
}

fn refresh_invalid() -> ApiError {
    fail(401, ErrorCode::RefreshTokenInvalid, text::REFRESH_INVALID)
}

/// Identity refused a password it was given (`weak_password`).
fn is_weak(e: &IdentityError) -> bool {
    matches!(e, IdentityError::Invalid { code, .. } if code == "weak_password")
}

/// The extractor answered 503 already when app users aren't configured.
fn app(s: &AppState) -> &AppUsers {
    s.app
        .as_ref()
        .expect("AppCaller checks app users are configured")
}

/// The project's pool. `info.project_id` is the caller's project: [`branch_info`] refuses any
/// other, so a key of project A's branch never reaches pool B.
fn pool(info: &AppBranchInfo) -> Pool {
    Pool::App(info.project_id)
}

/// A short shape check; identity has the final word.
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

/// The branch's app-user settings (cached for 30 s); 409 unless app users are `enabled` for
/// the caller's own project.
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
    if info.status != "enabled" || info.project_id != caller.project_id {
        return Err(disabled());
    }
    Ok(info)
}

/// The pool's user with exactly this (normalized) email: one exact identity lookup. An address
/// that fails the shape check is no one, and identity is not asked, so every route answers it
/// as it answers an unknown email. A lookup identity refuses is no one too, and so is a user
/// whose id isn't a uuid.
async fn find_user(s: &AppState, info: &AppBranchInfo, email: &str) -> Flow<Option<(Uuid, User)>> {
    if !valid_email(email) {
        return Ok(None);
    }
    match s.identity.find_user_by_email(&pool(info), email).await {
        Ok(Some(u)) => Ok(Uuid::parse_str(&u.id).ok().map(|id| (id, u))),
        Ok(None) | Err(IdentityError::Invalid { .. }) => Ok(None),
        Err(e) => Err(identity_down(&e)),
    }
}

fn view(id: Uuid, u: &User) -> AppUserView {
    AppUserView {
        id,
        email: u.email.clone(),
        email_verified: u.email_verified,
        created_at: u.created_at,
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
        unavailable()
    })?;
    Ok(SessionResponse {
        access_token,
        token_type: "bearer",
        expires_in: ttl,
        refresh_token,
        user,
    })
}

/// Ends one identity session; one already gone is fine, a failure is only logged (the app
/// session it would back was never made, or is revoked).
async fn end_identity_session(s: &AppState, pool: &Pool, session_id: &str) {
    match s.identity.revoke_session(pool, session_id).await {
        Ok(()) | Err(IdentityError::NotFound(Missing::Session)) => {}
        Err(e) => warn!(error = %e, "ending an identity session failed"),
    }
}

/// With the password checked (`signed`, a new identity session): the user's state, then the
/// app session, linked to the identity session by its id (the token is never kept). Any
/// refusal ends the identity session.
async fn open_session(
    s: &AppState,
    info: &AppBranchInfo,
    signed: SignedIn,
) -> Flow<(SessionResponse, Uuid)> {
    let app = app(s);
    let pool = pool(info);
    let end = |why: ApiError| async {
        end_identity_session(s, &pool, &signed.session_id).await;
        why
    };
    let Ok(user_id) = Uuid::parse_str(&signed.user.id) else {
        warn!("identity answered a user id that isn't a uuid");
        return Err(end(unavailable()).await);
    };
    if signed.user.banned {
        return Err(end(inactive()).await);
    }
    if info.email_verification && !signed.user.email_verified {
        return Err(end(fail(403, ErrorCode::EmailNotVerified, text::NOT_VERIFIED)).await);
    }
    let created = match app
        .internal
        .create_session(&CreateSession {
            project_id: info.project_id,
            user_id,
            identity_session_id: signed.session_id.clone(),
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
        view(user_id, &signed.user),
    )?;
    Ok((response, created.session_id))
}

async fn publish_sync(app: &AppUsers, req: SyncRequest) {
    if let Err(e) = app.internal.sync(&req).await {
        // The reconcile heals a missed sync.
        warn!(error = %e, user = %req.user_id, "publishing the app user sync failed");
    }
}

/// Mails the user a code of `kind`, within [`may_issue_code`]: at most five of a kind an hour
/// per user, whatever flow asks (m2's guess limit resets with each new code, so this is what
/// bounds the guesses). The answer never says whether one went out; a failure is only logged.
async fn send_code(s: &AppState, info: &AppBranchInfo, user_id: Uuid, kind: CodeKind) {
    let user = user_id.to_string();
    if !may_issue_code(&s.limiter, kind, &user) {
        info!(kind = kind.as_str(), "an app user's code limit is used up; nothing sent");
        return;
    }
    let request = CodeRequest::new(&user, kind, Delivery::Email);
    if let Err(e) = s.identity.create_code(&pool(info), &request).await {
        warn!(error = %e, kind = kind.as_str(), "sending an app user's code failed");
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
async fn taken(s: &AppState, info: &AppBranchInfo, email: &str) -> Flow {
    if !info.email_verification {
        info!(email = %email_tag(email), "app sign-up for a taken email");
        return Err(fail(409, ErrorCode::EmailTaken, text::ACCOUNT_EXISTS));
    }
    info!(email = %email_tag(email), "app sign-up for a taken email (decoy)");
    if let Ok(Some((owner, _))) = find_user(s, info, email).await {
        send_code(s, info, owner, CodeKind::Reset).await;
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
            unavailable()
        }
    }
}

/// Deletes the identity user a sign-up made when its row can't be written. If this fails too,
/// the reconcile writes the row from the pool.
async fn undo_identity_user(s: &AppState, info: &AppBranchInfo, id: Uuid) {
    match s.identity.delete_user(&pool(info), &id.to_string()).await {
        Ok(()) | Err(IdentityError::NotFound(Missing::User)) => {}
        Err(e) => warn!(error = %e, user = %id, "deleting the identity user after a failed insert failed"),
    }
}

/// Sign-up. Identity picks the user's id, and the row needs it, so the row is tried twice:
///
/// 1. a probe insert with a provisional id, rolled back: a bad `data` value never reaches
///    identity (and is 422 whatever the email);
/// 2. identity creates the user (a taken email is P12's answer);
/// 3. the insert with identity's id, then the commit. If either fails, the identity user is
///    deleted, so a retry starts clean; if that delete fails too, the reconcile writes the row
///    from the pool.
///
/// Then the sync, and a code (verification on) or a session (off).
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
    if !usable_password(&b.password) {
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
    let ev = info.email_verification;
    let row = |id| NewUserRow {
        id,
        email: email.clone(),
        email_verified: !ev,
        data: data.clone(),
    };
    // 1. The probe.
    let mut retried = false;
    loop {
        let Some(target) = info.sync.clone() else {
            warn!(branch = %info.target_branch_id, "the users table's branch has no running compute");
            return Err(unavailable());
        };
        match app.users_db.begin_insert(&target, &row(Uuid::new_v4())).await {
            Ok(probe) => {
                probe.rollback().await;
                break;
            }
            Err(UserRowError::Unavailable(m)) if !retried => {
                // The compute may have moved or its password changed: ask again.
                warn!(error = %m, "the users table is unavailable; retrying with fresh settings");
                retried = true;
                app.branches.invalidate(&caller.branch_id).await;
                info = branch_info(app, caller).await?;
            }
            Err(UserRowError::EmailTaken) => return taken(s, &info, &email).await,
            Err(e) => return Err(row_error(e, &info)),
        }
    }
    // Review S5: the status as the orchestrator has it now, not the cached one. A disable that
    // began within the cache's 30 s checked the pool was empty; a user made after that would
    // be dropped with the pool. (A retry above has just read it afresh.)
    if !retried {
        app.branches.invalidate(&caller.branch_id).await;
        info = branch_info(app, caller).await?;
    }
    // 2. The identity user.
    let created = s
        .identity
        .create_user(
            &pool(&info),
            &NewUser {
                email: &email,
                password: Some(&b.password),
                name: None,
                email_verified: !ev,
            },
        )
        .await;
    let user = match created {
        Ok(u) => u,
        Err(IdentityError::Conflict(_)) => return taken(s, &info, &email).await,
        Err(e) if is_weak(&e) => return Err(weak_password()),
        Err(IdentityError::Invalid { code, message }) => {
            info!(code, message, "identity refused the new app user");
            return Err(fail(422, ErrorCode::InvalidRequest, text::REFUSED_INPUT));
        }
        Err(e) => return Err(identity_down(&e)),
    };
    let Ok(id) = Uuid::parse_str(&user.id) else {
        warn!("identity made a user whose id isn't a uuid");
        return Err(unavailable());
    };
    // 3. The row, with identity's id.
    let Some(target) = info.sync.clone() else {
        undo_identity_user(s, &info, id).await;
        return Err(unavailable());
    };
    let written = match app.users_db.begin_insert(&target, &row(id)).await {
        Ok(pending) => pending.commit().await,
        Err(e) => Err(e),
    };
    if let Err(e) = written {
        warn!(user = %id, "writing the app user's row failed; deleting the identity user");
        undo_identity_user(s, &info, id).await;
        return Err(match e {
            UserRowError::EmailTaken => return taken(s, &info, &email).await,
            e => row_error(e, &info),
        });
    }
    publish_sync(
        app,
        SyncRequest {
            project_id: info.project_id,
            user_id: id,
            data: (!data.is_empty()).then_some(data.clone()),
            origin_branch_id: Some(target.branch_id),
        },
    )
    .await;
    if ev {
        send_code(s, &info, id, CodeKind::Verify).await;
        return Ok(pending(id, &email));
    }
    // The account exists and is written; a sign-in that fails here leaves it for the next.
    let signed = s
        .identity
        .sign_in(&pool(&info), &email, &b.password)
        .await
        .map_err(|e| identity_down(&e))?;
    let (session, _) = open_session(s, &info, signed).await?;
    Ok((StatusCode::CREATED, Json(session)).into_response())
}

async fn sign_in(s: &AppState, caller: &AppCaller, b: AppSignInBody) -> Flow {
    let email = normalize_email(&b.email);
    rate_limit(s, caller, &[("sign-in", "ip", &caller.ip, 10, MINUTE)])?;
    // A disabled branch (409) or the orchestrator down (503) is answered before the email's
    // attempt is reserved, so it costs nothing.
    let info = branch_info(app(s), caller).await?;
    // Per email, only failed password checks count, so nobody can use up a known account's
    // attempts; a server-vouched caller (no address limit) is held to it too. The attempt is
    // reserved up front, so concurrent requests can't all pass a check before any failure
    // lands, and a client that hangs up during the check still spends it. Only these outcomes
    // give it back:
    //
    // | Outcome                                                         | Answer | Email slot |
    // |-----------------------------------------------------------------|--------|------------|
    // | the email's 5 a minute are used                                 | 429    | none held  |
    // | `BadCredentials`: an unknown email or a wrong password          | 401    | counts     |
    // | `Invalid`: identity's request validation (the caller's input)   | 401    | counts     |
    // | `Banned`: the password is right, the user banned                | 403    | refunded   |
    // | the password is right (refunded before anything else awaits)    | any    | refunded   |
    // | identity unavailable, or any other answer of its                | 503    | refunded   |
    // | the request is dropped undecided (the client hung up)           | none   | counts     |
    //
    // An unknown email and a wrong password are one answer of identity's, so they count and
    // answer the same 401, padded alike. Whatever [`open_session`] answers afterwards (403
    // unverified, 503) comes after the refund.
    let key = format!(
        "app:{}:sign-in:email:{}",
        caller.project_id,
        email_tag(&email)
    );
    let Some(slot) = Slot::reserve(&s.limiter, key, 5, MINUTE) else {
        return Err(fail(429, ErrorCode::RateLimited, text::TOO_MANY));
    };
    let signed = match s.identity.sign_in(&pool(&info), &email, &b.password).await {
        Ok(signed) => {
            slot.refund();
            signed
        }
        Err(IdentityError::BadCredentials | IdentityError::Invalid { .. }) => {
            info!(email = %email_tag(&email), "app sign-in refused");
            return Err(refused());
        }
        Err(IdentityError::Banned) => {
            slot.refund();
            return Err(inactive());
        }
        Err(e) => {
            slot.refund();
            return Err(identity_down(&e));
        }
    };
    let (session, _) = open_session(s, &info, signed).await?;
    Ok(Json(session).into_response())
}

/// Checks the token's session, then rotates the refresh token.
///
/// Invariant (#57): clients drop their cookies on `refresh_token_invalid`,
/// `refresh_token_reused` and `user_inactive`, so a refresh revokes the session or answers one
/// of them only on a definitive verdict: the orchestrator's 401 `refresh_token_invalid` or
/// `refresh_token_reused`, or identity's verdict on the app session's identity session (see
/// [`current_user`]). Every other upstream outcome (any other orchestrator status or code, an
/// identity 5xx or other refusal, a timeout, an unreadable or partial success answer) answers
/// 503 `unavailable` and revokes nothing.
///
/// And (#59): nothing rotates before the session check. The orchestrator's peek names the
/// token's session, user and identity session without writing, and says whether the token is
/// the current one or the previous one inside its 10 s grace.
/// - A current token: identity is asked about the session first, and only then does `refresh`
///   rotate. A fault before the rotation (peek, identity, or the lookup taking longer than
///   [`REFRESH_USER_LOOKUP`](super::REFRESH_USER_LOOKUP)) answers 503 and leaves the token the
///   client holds current, so a later retry with it works, however late, unless another
///   request rotates it meanwhile.
/// - A grace token goes to `refresh` first, then the session is checked: its grace answer
///   writes nothing, and holding it through a slow identity could outlast the grace and turn it
///   into reuse (#59 audit B1). A fault after that answers 503; the client keeps a grace
///   token, which works again only until its grace ends.
/// - A peek verdict goes on to `refresh`, so the orchestrator applies its own rules (it
///   revokes on reuse); a session that `refresh` names but peek didn't check is checked after.
///
/// A request that peeked a current token just before a parallel one rotated it reaches
/// `refresh` holding a grace token. [`REFRESH_USER_LOOKUP`](super::REFRESH_USER_LOOKUP) caps
/// the lookup at half of [`APP_REFRESH_GRACE`](chelabase_auth::app_token::APP_REFRESH_GRACE)
/// or less, so it gets the grace answer provided the peek's reply, the `refresh` call and its
/// row lock take under the rest. If they take longer, the grace has ended: `refresh` answers
/// `refresh_token_reused` and revokes the session, an accepted residual (#59 second audit).
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
            p.session_id,
            current_user(s, &info, p.user_id, &p.identity_session_id, p.session_id).await?,
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
            return Err(refresh_invalid());
        }
        Err(e) => return Err(internal_down(&e)),
    };
    let current = match checked {
        Some((session, current)) if session == refreshed.session_id => current,
        // A grace token, a peek verdict that refresh then didn't give, or another session:
        // check the one `refresh` answered for.
        _ => {
            current_user(
                s,
                &info,
                refreshed.user_id,
                &refreshed.identity_session_id,
                refreshed.session_id,
            )
            .await?
        }
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

/// The app session's user, as identity has them now, when the session may still refresh.
///
/// Identity is asked for the identity session behind the app session, by id, in the project's
/// own pool (never by token: none is kept). Its definitive verdicts revoke the app session:
/// - the session is gone (revoked, expired, its user deleted or banned: identity drops a banned
///   user's sessions whenever it finds one, so one made after a ban never reads as live): 403
///   `user_inactive` when the user is banned, else 401 `refresh_token_invalid` (also when that
///   follow-up read fails: the session's end is already certain);
/// - the session belongs to another user than the app session's: 401;
/// - its user is banned: 403.
///
/// Any other answer (a missing pool, a refusal of ours, a 5xx, an unreadable answer, or none
/// within [`REFRESH_USER_LOOKUP`](super::REFRESH_USER_LOOKUP)) answers 503 and revokes nothing.
async fn current_user(
    s: &AppState,
    info: &AppBranchInfo,
    user_id: Uuid,
    identity_session_id: &str,
    session_id: Uuid,
) -> Flow<User> {
    let app = app(s);
    let pool = pool(info);
    let revoke = || async {
        if let Err(e) = app.internal.revoke(session_id, info.project_id).await {
            warn!(error = %e, "revoking the app session failed");
        }
    };
    let lookup = app.refresh_user_lookup;
    let timed_out = || IdentityError::Unavailable("the session lookup timed out".into());
    // An app session always names its identity session; an empty one names nothing.
    let answer = if identity_session_id.is_empty() {
        Err(IdentityError::NotFound(Missing::Session))
    } else {
        tokio::time::timeout(lookup, s.identity.session_by_id(&pool, identity_session_id))
            .await
            .unwrap_or_else(|_| Err(timed_out()))
    };
    let found = match answer {
        Ok(found) => found,
        Err(IdentityError::NotFound(Missing::Session)) => {
            revoke().await;
            let user = tokio::time::timeout(lookup, s.identity.get_user(&pool, &user_id.to_string()))
                .await
                .unwrap_or_else(|_| Err(timed_out()));
            return Err(match user {
                Ok(u) if u.banned => inactive(),
                _ => refresh_invalid(),
            });
        }
        Err(e) => return Err(identity_down(&e)),
    };
    let mine = user_id.to_string();
    if found.session.user_id != mine || found.user.id != mine {
        warn!(session = %session_id, "an app session's identity session belongs to another user");
        revoke().await;
        return Err(refresh_invalid());
    }
    if found.user.banned {
        revoke().await;
        return Err(inactive());
    }
    Ok(found.user)
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
    match s.identity.get_user(&pool(&info), &claims.sub.to_string()).await {
        Ok(u) => Ok(Json(view(claims.sub, &u)).into_response()),
        Err(IdentityError::NotFound(Missing::User)) => Err(no_user()),
        Err(e) => Err(identity_down(&e)),
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
        .identity
        .verify_code(&pool(&info), &user_id.to_string(), CodeKind::Verify, b.code.trim(), None)
        .await
    {
        Ok(_) => {}
        // A wrong, used, expired or locked code, a malformed one, or a user gone meanwhile:
        // the same answer as an unknown email.
        Err(
            IdentityError::CodeInvalid
            | IdentityError::Invalid { .. }
            | IdentityError::NotFound(Missing::User),
        ) => return Err(bad_code()),
        Err(e) => return Err(identity_down(&e)),
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
        Ok(Some((id, u))) if !u.email_verified => send_code(s, &info, id, CodeKind::Verify).await,
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
        Ok(Some((id, _))) => send_code(s, &info, id, CodeKind::Reset).await,
        Ok(None) => info!(email = %email_tag(&email), "app password reset for an unknown email"),
        Err(_) => {}
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// The code sets the password and ends the user's identity sessions in one step (identity's
/// `verify_code`), then the app sessions are revoked. If that revocation fails, the answer is
/// still 204: each old app session is refused at its next refresh, because the identity session
/// behind it is gone. A retry can't redo it (the code is spent), and needn't.
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
    // Before identity sees the email, so this answer can't tell accounts apart.
    if !usable_password(&b.password) {
        return Err(weak_password());
    }
    let info = branch_info(app, caller).await?;
    let Some((user_id, _)) = find_user(s, &info, &email).await? else {
        return Err(bad_code());
    };
    match s
        .identity
        .verify_code(
            &pool(&info),
            &user_id.to_string(),
            CodeKind::Reset,
            b.code.trim(),
            Some(&b.password),
        )
        .await
    {
        Ok(_) => {}
        Err(e) if is_weak(&e) => return Err(weak_password()),
        // A wrong code, or a user gone meanwhile: the same answer as an unknown email.
        Err(
            IdentityError::CodeInvalid
            | IdentityError::Invalid { .. }
            | IdentityError::NotFound(Missing::User),
        ) => return Err(bad_code()),
        Err(e) => return Err(identity_down(&e)),
    }
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
        warn!(error = %e, user = %user_id, "revoking the app sessions after a reset failed; their refresh is refused anyway");
    }
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Sets the new address, unverified when verification is on (then a code goes to it), and
/// syncs. Sessions continue. A failure at `set_email` changes nothing; after it, a lost code is
/// what `resend-code` is for and a lost sync is the reconcile's (a retry answers 409
/// `conflict`: the address is the account's already).
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
    match find_user(s, &info, &email).await? {
        Some((id, _)) if id == claims.sub => {
            return Err(fail(409, ErrorCode::Conflict, text::EMAIL_NOT_CHANGED));
        }
        Some(_) => return Err(fail(409, ErrorCode::EmailTaken, text::ACCOUNT_EXISTS)),
        None => {}
    }
    let ev = info.email_verification;
    match s
        .identity
        .set_email(&pool(&info), &claims.sub.to_string(), &email, !ev)
        .await
    {
        Ok(_) => {}
        // Taken between the lookup and the write.
        Err(IdentityError::Conflict(_)) => {
            return Err(fail(409, ErrorCode::EmailTaken, text::ACCOUNT_EXISTS));
        }
        Err(IdentityError::NotFound(Missing::User)) => return Err(no_user()),
        // Identity's check of the address: the caller's input.
        Err(IdentityError::Invalid { code, message }) if code != "weak_password" => {
            info!(code, message, "identity refused the new email");
            return Err(fail(422, ErrorCode::InvalidRequest, text::BAD_EMAIL));
        }
        Err(e) => return Err(identity_down(&e)),
    }
    if ev {
        send_code(s, &info, claims.sub, CodeKind::Verify).await;
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

/// Changes the password and returns a new session; every other session ends. In order:
///
/// 1. the current password is checked with an identity sign-in;
/// 2. `set_password`. A failure up to here changes nothing (the check's identity session is
///    ended), and a retry works;
/// 3. the user's sessions end in both stores, identity's then the app sessions, each attempted
///    whatever the other did. Either alone refuses every old refresh token: a revoked app
///    session directly, one whose identity session is gone at its next refresh. Only when both
///    fail is the answer 503, with the password changed and the old sessions alive;
/// 4. a sign-in with the new password, and its app session. If that fails, the answer is 503
///    with the change done and every session ended: the user signs in with the new password.
///
/// After step 2, a retry with the same body is refused (the current password is the new one).
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
    if !usable_password(&b.password) {
        return Err(weak_password());
    }
    let info = branch_info(app, caller).await?;
    let pool = pool(&info);
    let current = match s.identity.get_user(&pool, &sub).await {
        Ok(u) => u,
        Err(IdentityError::NotFound(Missing::User)) => return Err(no_user()),
        Err(e) => return Err(identity_down(&e)),
    };
    // The new session needs a verified email when verification is on: refuse first, so a 403
    // never means "changed anyway".
    if info.email_verification && !current.email_verified {
        return Err(fail(403, ErrorCode::EmailNotVerified, text::NOT_VERIFIED));
    }
    // 1.
    let check = match s.identity.sign_in(&pool, &current.email, &b.current_password).await {
        Ok(signed) if signed.user.id == sub => signed,
        Ok(signed) => {
            end_identity_session(s, &pool, &signed.session_id).await;
            return Err(refused());
        }
        Err(IdentityError::BadCredentials | IdentityError::Banned | IdentityError::Invalid { .. }) => {
            return Err(refused());
        }
        Err(e) => return Err(identity_down(&e)),
    };
    // 2.
    if let Err(e) = s.identity.set_password(&pool, &sub, &b.password).await {
        end_identity_session(s, &pool, &check.session_id).await;
        return Err(match e {
            e if is_weak(&e) => weak_password(),
            IdentityError::NotFound(Missing::User) => no_user(),
            e => identity_down(&e),
        });
    }
    // 3.
    let identity_ended = s.identity.revoke_user_sessions(&pool, &sub).await;
    let app_ended = app
        .internal
        .revoke_user_sessions(
            claims.sub,
            &RevokeUserSessions {
                project_id: info.project_id,
                keep_session_id: None,
                created_before: None,
            },
        )
        .await;
    match (&identity_ended, &app_ended) {
        (Err(i), Err(a)) => {
            warn!(identity = %i, app = %a, user = %sub, "the password changed but no session was ended");
            return Err(unavailable());
        }
        (Err(e), Ok(())) => {
            warn!(error = %e, user = %sub, "ending the identity sessions after a password change failed");
        }
        (Ok(()), Err(e)) => {
            warn!(error = %e, user = %sub, "revoking the app sessions after a password change failed; their refresh is refused anyway");
        }
        (Ok(()), Ok(())) => {}
    }
    // 4.
    let signed = match s.identity.sign_in(&pool, &current.email, &b.password).await {
        Ok(signed) => signed,
        Err(e) => return Err(identity_down(&e)),
    };
    let (session, _) = open_session(s, &info, signed).await?;
    Ok(Json(session).into_response())
}
