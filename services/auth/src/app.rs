//! The HTTP API Studio's sign-in pages call (through the gateway).

use crate::config::Config;
use crate::cookie::*;
use crate::guard::{RateLimiter, UsedTickets, client_ip, email_tag, pad};
use chelabase_zitadel::rules::{normalize_email, strong_password};
use crate::zitadel::{NewUser, Session, UserState, ZError, Zitadel};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use chelabase_types::mode_policy::{ModePolicy, SignUpRule};
use chelabase_types::api_error::{ApiError, ErrorCode, ErrorEnvelope};
use chelabase_types::api_extract::ApiJson;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};
use utoipa::ToSchema;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

pub mod messages {
    pub const BAD_CREDENTIALS: &str = "Email or password is incorrect";
    pub const BAD_CODE: &str = "Code is invalid or expired";
    pub const WEAK_PASSWORD: &str =
        "Use at least 8 characters, with upper- and lower-case letters, a number and a symbol";
    pub const GONE: &str = "This sign-in has expired, start again";
    pub const TOO_MANY: &str = "Too many attempts, try again later";
    pub const UNAVAILABLE: &str = "Sign-in is unavailable, try again";
    pub const ACCOUNT_EXISTS: &str = "An account with this email already exists";
    pub const SIGN_UP_CLOSED: &str = "Sign-up is closed; ask an administrator for an invite";
    pub const NOT_IN_PLATFORM: &str = "Sign-up isn't available in platform mode";
    pub const INVITE_EXPIRED: &str = "This invite has expired; ask for a new one";
    pub const INVITE_REVOKED: &str = "This invite is no longer valid";
    pub const INVITE_INCOMPLETE: &str =
        "Your invite was accepted but sign-in did not finish; try again";
    pub const BAD_INVITE: &str = "The invite link is invalid";
    pub const NO_SUCH_INVITE: &str = "No open invite matches this link";
}
use messages::*;

pub struct AppState {
    pub zitadel: Zitadel,
    pub config: Config,
    pub limiter: RateLimiter,
    /// Accept tickets already spent.
    pub used_tickets: UsedTickets,
    /// App users' flows; `None` (their routes answer 503) without the app keys.
    pub app: Option<crate::app_users::AppUsers>,
    /// Asks the orchestrator whether sign-up is open. `None` fails closed (sign-up answers 503).
    pub signup_gate: Option<crate::app_users::InternalClient>,
    /// Zitadel with the admin token (from the admin PAT, independent of the app keys), which
    /// accept-invite needs; `None` makes it answer 503 before spending a code.
    pub admin: Option<Zitadel>,
}

type S = State<Arc<AppState>>;

/// The documented routes; `openapi()` reads the document from here.
pub(crate) fn api_router() -> OpenApiRouter<Arc<AppState>> {
    platform_router().merge(crate::app_users::flows::api_router())
}

/// Studio's sign-in pages' routes.
fn platform_router() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(auth_request))
        .routes(routes!(sign_in))
        .routes(routes!(sign_up))
        .routes(routes!(accept_invite))
        .routes(routes!(describe_invite))
        .routes(routes!(verify_email))
        .routes(routes!(resend_code))
        .routes(routes!(password_reset))
        .routes(routes!(password_reset_confirm))
        .routes(routes!(logout))
}

pub fn router(state: Arc<AppState>) -> Router {
    let (platform, _) = platform_router().split_for_parts();
    let (app_users, _) = crate::app_users::flows::api_router().split_for_parts();
    platform
        // Every body here is a few short fields; this also bounds rate-limit keys.
        .layer(DefaultBodyLimit::max(8 * 1024))
        // Sign-up's `data` may carry a few columns' values.
        .merge(app_users.layer(DefaultBodyLimit::max(64 * 1024)))
        .with_state(state)
}

/// What the sign-in pages do next, as `{"next": "done", "redirect": …}` or `{"next": "verify"}`.
#[derive(Serialize, ToSchema)]
#[serde(tag = "next", rename_all = "lowercase")]
enum NextStep {
    /// Signed in: send the browser to `redirect` (Zitadel's callback for the pending sign-in).
    Done { redirect: String },
    /// The email isn't verified: show the code step.
    Verify,
}

fn next_step(step: &NextStep) -> Value {
    serde_json::to_value(step).expect("NextStep serialises")
}

// Documentation only: the handler answers through `finish`, which serialises `NextStep::Done`
// to the same JSON.
/// The signed-in answer: `next` is always `done`.
#[allow(dead_code)]
#[derive(Serialize, ToSchema)]
struct SignedIn {
    next: DoneTag,
    redirect: String,
}

#[allow(dead_code)]
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
enum DoneTag {
    Done,
}

/// An empty JSON object.
#[derive(Serialize, ToSchema)]
struct EmptyResponse {}

/// What the sign-in page needs to know about the pending sign-in.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct AuthRequestInfo {
    /// The visitor asked to create an account (`prompt=create`) and this instance allows it:
    /// Studio goes to its sign-up page. `false` for a refused sign-up request too, which then
    /// lands on the plain sign-in page.
    sign_up: bool,
    /// This instance takes sign-ups (open sign-up is on, or the tenant has no member yet): the
    /// sign-in page may offer "Create account". Always `false` in platform mode.
    sign_up_allowed: bool,
}

const MINUTE: Duration = Duration::from_secs(60);
/// Sign-up may wait up to a second for Zitadel to see the new user (`new_user_session`), so its
/// answers are padded this much beyond sign-in's, or a new email would answer slower than a taken one.
pub(crate) const SIGN_UP_EXTRA: Duration = Duration::from_secs(1);
const HOUR: Duration = Duration::from_secs(3600);

/// Counts one attempt against each key in order, stopping at the first that is over its
/// limit (so an IP over its limit never spends or creates the keys after it).
fn allowed(s: &AppState, checks: &[(String, u32, Duration)]) -> bool {
    checks
        .iter()
        .all(|(key, limit, window)| s.limiter.allow(key, *limit, *window))
}

/// Zitadel ids go into URL paths, so only their exact shapes are passed on:
/// `V2_<digits>` for pending sign-ins, digits for users.
fn is_auth_request_id(id: &str) -> bool {
    id.strip_prefix("V2_")
        .is_some_and(|n| !n.is_empty() && n.len() <= 32 && n.bytes().all(|b| b.is_ascii_digit()))
}

fn is_user_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_digit())
}

fn reply(status: StatusCode, body: Value, cookies: Vec<String>) -> Response {
    let mut response = (status, Json(body)).into_response();
    for c in cookies {
        response
            .headers_mut()
            .append(header::SET_COOKIE, c.parse().expect("cookie header"));
    }
    response
}

fn error(status: StatusCode, code: ErrorCode, message: &str) -> Response {
    ApiError::new(status.as_u16(), code, message).into_response()
}

fn unavailable(e: &ZError) -> Response {
    warn!(error = %e, "zitadel call failed");
    error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE)
}

/// A pending sign-in that Zitadel no longer knows is gone; anything else is unavailability.
fn pending_sign_in_error(e: ZError) -> Response {
    match e {
        ZError::Rejected { .. } => error(StatusCode::GONE, ErrorCode::SignInExpired, GONE),
        e => unavailable(&e),
    }
}

fn verify_template(s: &AppState) -> String {
    format!("{}/verify?code={{{{.Code}}}}", s.config.studio_url)
}

fn login_cookie(s: &AppState, state: &LoginState) -> String {
    set_cookie(
        LOGIN_COOKIE,
        &seal_login(&s.config.cookie_key, state),
        LOGIN_TTL_SECS,
        s.config.secure_cookies,
    )
}

/// Finalize the pending sign-in: `done` with Zitadel's redirect, the session cookie set and
/// the step cookie cleared.
async fn finish(s: &AppState, auth_request: &str, session: Session) -> Response {
    match s.zitadel.finish(auth_request, &session).await {
        Ok(redirect) => signed_in(s, session, redirect),
        Err(e) => pending_sign_in_error(e),
    }
}

/// The `done` answer with the session cookie set and the step cookie cleared.
fn signed_in(s: &AppState, session: Session, redirect: String) -> Response {
    let kept = SessionState {
        session_id: session.id,
        session_token: session.token,
        exp: now_secs() + SESSION_TTL_SECS,
    };
    let secure = s.config.secure_cookies;
    reply(
        StatusCode::OK,
        next_step(&NextStep::Done { redirect }),
        vec![
            set_cookie(
                SESSION_COOKIE,
                &seal_session(&s.config.cookie_key, &kept),
                SESSION_TTL_SECS,
                secure,
            ),
            clear_cookie(LOGIN_COOKIE, secure),
        ],
    )
}

async fn verify_step(
    s: &AppState,
    auth_request: &str,
    user_id: String,
    session: Session,
) -> Response {
    let state = LoginState {
        auth_request: auth_request.to_string(),
        user_id: Some(user_id),
        session_id: Some(session.id),
        session_token: Some(session.token),
        step: Step::Verify,
        decoy: false,
        exp: now_secs() + LOGIN_TTL_SECS,
    };
    reply(
        StatusCode::OK,
        next_step(&NextStep::Verify),
        vec![login_cookie(s, &state)],
    )
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct AuthRequestBody {
    auth_request: String,
}

#[utoipa::path(
    post,
    path = "/auth/v1/auth-request",
    operation_id = "getAuthRequest",
    tag = "login",
    description = "Looks up a pending sign-in and reports whether sign-up is asked for and allowed. Sets no cookie.",
    request_body = AuthRequestBody,
    responses(
        (status = 200, description = "The pending sign-in is valid", body = AuthRequestInfo),
        (status = 400, description = "The body is malformed: `invalid_request`", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON (wrong content type): `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "The body is JSON but has the wrong fields: `invalid_request`", body = ErrorEnvelope),
        (status = 413, description = "The body is over 8 KiB: `too_large`", body = ErrorEnvelope),
        (status = 410, description = "This sign-in has expired or is unknown: `sign_in_expired`", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "Zitadel is unavailable: `unavailable`", body = ErrorEnvelope),
    ),
)]
async fn auth_request(State(s): S, headers: HeaderMap, ApiJson(b): ApiJson<AuthRequestBody>) -> Response {
    let ip = client_ip(&headers);
    if !allowed(&s, &[(format!("auth-request:ip:{ip}"), 60, MINUTE)]) {
        return error(StatusCode::TOO_MANY_REQUESTS, ErrorCode::RateLimited, TOO_MANY);
    }
    if !is_auth_request_id(&b.auth_request) {
        return error(StatusCode::GONE, ErrorCode::SignInExpired, GONE);
    }
    match s.zitadel.auth_request(&b.auth_request).await {
        Ok(asked) => {
            let sign_up_allowed = sign_up_gate(&s).await.is_none();
            reply(
                StatusCode::OK,
                json!(AuthRequestInfo { sign_up: asked && sign_up_allowed, sign_up_allowed }),
                Vec::new(),
            )
        }
        Err(e) => pending_sign_in_error(e),
    }
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct SignInBody {
    auth_request: String,
    email: String,
    password: String,
}

#[utoipa::path(
    post,
    path = "/auth/v1/sign-in",
    operation_id = "signIn",
    tag = "login",
    description = "Signs in with email and password. On success sets the HttpOnly session cookie, or, when the email still needs verifying, the HttpOnly step cookie that the code step reads.",
    request_body = SignInBody,
    responses(
        (status = 200, description = "Signed in (`done`, with the session cookie set and the step cookie cleared) or waiting for the email code (`verify`, with the step cookie set)", body = NextStep),
        (status = 400, description = "The body is malformed: `invalid_request`", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON (wrong content type): `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "The body is JSON but has the wrong fields: `invalid_request`", body = ErrorEnvelope),
        (status = 413, description = "The body is over 8 KiB: `too_large`", body = ErrorEnvelope),
        (status = 401, description = "Wrong email or password: `invalid_credentials`", body = ErrorEnvelope),
        (status = 410, description = "This sign-in has expired or is unknown: `sign_in_expired`", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "Zitadel is unavailable: `unavailable`", body = ErrorEnvelope),
    ),
)]
async fn sign_in(State(s): S, headers: HeaderMap, ApiJson(b): ApiJson<SignInBody>) -> Response {
    let start = Instant::now();
    let answer = sign_in_inner(&s, &headers, b).await;
    pad(start, s.config.min_response).await;
    answer
}

async fn sign_in_inner(s: &AppState, headers: &HeaderMap, b: SignInBody) -> Response {
    let email = normalize_email(&b.email);
    let ip = client_ip(headers);
    // Per email, only failures count: signing in as someone takes their password, so nobody
    // can use up a known account's attempts.
    let email_key = format!("sign-in:email:{}", email_tag(&email));
    if !allowed(s, &[(format!("sign-in:ip:{ip}"), 10, MINUTE)])
        || s.limiter.over(&email_key, 5, MINUTE)
    {
        return error(StatusCode::TOO_MANY_REQUESTS, ErrorCode::RateLimited, TOO_MANY);
    }
    let refused = || {
        s.limiter.hit(&email_key, MINUTE);
        error(StatusCode::UNAUTHORIZED, ErrorCode::InvalidCredentials, BAD_CREDENTIALS)
    };
    if !is_auth_request_id(&b.auth_request) {
        return error(StatusCode::GONE, ErrorCode::SignInExpired, GONE);
    }
    if let Err(e) = s.zitadel.auth_request(&b.auth_request).await {
        return pending_sign_in_error(e);
    }
    let user_id = match s.zitadel.find_user_by_email(&s.config.org_id, &email).await {
        Ok(Some(id)) => id,
        Ok(None) => return refused(),
        Err(e) => return unavailable(&e),
    };
    let session = match s.zitadel.create_session(&user_id, &b.password).await {
        Ok(session) => session,
        Err(ZError::Rejected { .. }) => {
            info!(email = %email_tag(&email), "sign-in refused");
            return refused();
        }
        Err(e) => return unavailable(&e),
    };
    match s.zitadel.email_verified(&user_id).await {
        Ok(true) => finish(s, &b.auth_request, session).await,
        Ok(false) => {
            if let Err(e) = s.zitadel.resend_code(&user_id, &verify_template(s)).await {
                warn!(error = %e, "resending the verification code failed");
            }
            verify_step(s, &b.auth_request, user_id, session).await
        }
        Err(e) => unavailable(&e),
    }
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct SignUpBody {
    auth_request: String,
    email: String,
    password: String,
    given_name: String,
    family_name: String,
}

#[utoipa::path(
    post,
    path = "/auth/v1/sign-up",
    operation_id = "signUp",
    tag = "login",
    description = "Creates an account. Sets the HttpOnly session cookie when email verification is off, otherwise the HttpOnly step cookie that the code step reads.",
    request_body = SignUpBody,
    responses(
        (status = 200, description = "Signed in (`done`, with the session cookie set and the step cookie cleared) or waiting for the email code (`verify`, with the step cookie set)", body = NextStep),
        (status = 400, description = "The body is malformed (`invalid_request`) or the password is too weak (`weak_password`)", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON (wrong content type): `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "The body is JSON but has the wrong fields: `invalid_request`", body = ErrorEnvelope),
        (status = 413, description = "The body is over 8 KiB: `too_large`", body = ErrorEnvelope),
        (status = 403, description = "Sign-up is closed for this instance (`sign_up_closed`), or not available in platform mode (`not_available_in_platform_mode`)", body = ErrorEnvelope),
        (status = 409, description = "An account with this email exists (`email_taken`); only when email verification is off", body = ErrorEnvelope),
        (status = 410, description = "This sign-in has expired or is unknown: `sign_in_expired`", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "Zitadel or the sign-up gate is unavailable: `unavailable`", body = ErrorEnvelope),
    ),
)]
async fn sign_up(State(s): S, headers: HeaderMap, ApiJson(b): ApiJson<SignUpBody>) -> Response {
    let start = Instant::now();
    let answer = sign_up_inner(&s, &headers, b).await;
    pad(start, s.config.min_response + SIGN_UP_EXTRA).await;
    answer
}

async fn sign_up_inner(s: &Arc<AppState>, headers: &HeaderMap, b: SignUpBody) -> Response {
    let email = normalize_email(&b.email);
    let ip = client_ip(headers);
    if !allowed(
        s,
        &[
            (format!("sign-up:ip:{ip}"), 20, HOUR),
            (format!("sign-up:email:{}", email_tag(&email)), 5, HOUR),
        ],
    ) {
        return error(StatusCode::TOO_MANY_REQUESTS, ErrorCode::RateLimited, TOO_MANY);
    }
    if !is_auth_request_id(&b.auth_request) {
        return error(StatusCode::GONE, ErrorCode::SignInExpired, GONE);
    }
    if !strong_password(&b.password) {
        return error(StatusCode::BAD_REQUEST, ErrorCode::WeakPassword, WEAK_PASSWORD);
    }
    if let Err(e) = s.zitadel.auth_request(&b.auth_request).await {
        return pending_sign_in_error(e);
    }
    if let Some(refused) = sign_up_gate(s).await {
        return refused;
    }
    // Zitadel checks usernames, not emails, for uniqueness: never make a second account
    // for an email that already has one.
    // Two accounts already sharing the email (made outside this check) count as taken too.
    match s.zitadel.find_user_in_org(&s.config.org_id, &email).await {
        Ok(crate::zitadel::EmailMatch::None) => {}
        Ok(_) => return taken(s, &email, b.auth_request),
        Err(e) => return unavailable(&e),
    }
    let template = verify_template(s);
    let new = NewUser {
        org_id: &s.config.org_id,
        email: &email,
        given_name: b.given_name.trim(),
        family_name: b.family_name.trim(),
        password: Some(&b.password),
        user_id: None,
        username: None,
        send_code: true,
        verified: s.config.skip_email_verification,
        url_template: &template,
    };
    let user_id = match s.zitadel.create_user(&new).await {
        Ok(id) => id,
        Err(ZError::Rejected { status: 409, .. }) => return taken(s, &email, b.auth_request),
        Err(ZError::Rejected { status: 400, .. }) => {
            return error(StatusCode::BAD_REQUEST, ErrorCode::WeakPassword, WEAK_PASSWORD);
        }
        Err(e) => return unavailable(&e),
    };
    let session = match new_user_session(s, &user_id, &b.password).await {
        Ok(session) => session,
        Err(e) => return unavailable(&e),
    };
    if s.config.skip_email_verification {
        return finish(s, &b.auth_request, session).await;
    }
    verify_step(s, &b.auth_request, user_id, session).await
}

/// `Some(answer)` when sign-up is refused: in platform mode always, otherwise when the
/// tenant's orchestrator says no. Anything unclear refuses (503).
async fn sign_up_gate(s: &AppState) -> Option<Response> {
    if ModePolicy::new(s.config.mode).sign_up() == SignUpRule::Refused {
        return Some(error(
            StatusCode::FORBIDDEN,
            ErrorCode::NotAvailableInPlatformMode,
            NOT_IN_PLATFORM,
        ));
    }
    let Some(gate) = &s.signup_gate else {
        warn!("no sign-up gate is configured");
        return Some(error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE));
    };
    match gate.signup_allowed(&s.config.org_id).await {
        Ok(a) if a.allowed => None,
        Ok(a) if a.reason == "platform" => Some(error(
            StatusCode::FORBIDDEN,
            ErrorCode::NotAvailableInPlatformMode,
            NOT_IN_PLATFORM,
        )),
        Ok(_) => Some(error(StatusCode::FORBIDDEN, ErrorCode::SignUpClosed, SIGN_UP_CLOSED)),
        Err(e) => {
            warn!(error = %e, "the sign-up gate failed");
            Some(error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE))
        }
    }
}

/// Both fields default to empty, so a missing one is a `not_found` like a wrong one.
#[derive(Deserialize, ToSchema)]
struct DescribeInviteBody {
    /// The Zitadel user id from the invite link.
    #[serde(default)]
    user: String,
    /// The invite token from the invite link.
    #[serde(default)]
    t: String,
}

/// Who invited the visitor and to what.
#[derive(Serialize, ToSchema)]
struct InviteDescription {
    /// The inviter's display name; their email's local part when they have none; `An admin` with
    /// neither (a machine user).
    inviter_name: String,
    #[schema(required = true)]
    inviter_email: Option<String>,
    /// The workspace the invite is to.
    tenant_name: String,
}

/// An invite link's `t`: 32 random bytes as unpadded base64url.
fn is_invite_token(t: &str) -> bool {
    t.len() == 43 && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[utoipa::path(
    post,
    path = "/auth/v1/invite",
    operation_id = "describeInvite",
    tag = "login",
    description = "Describes an open invite for the accept page: who invited the visitor, and to which workspace. The invite link's own token (`t`) is the credential, so this spends no Zitadel code. It is a POST so that the user id and the token are never in a URL. A closed (accepted, revoked or expired) invite, a wrong or missing user or token, and a malformed value all answer the same `not_found`.",
    request_body = DescribeInviteBody,
    responses(
        (status = 200, description = "The invite is open and the token matches", body = InviteDescription),
        (status = 400, description = "The body is not well-formed JSON: `invalid_request`", body = ErrorEnvelope),
        (status = 404, description = "`not_found`: no open invite with this user and token", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON (wrong content type): `invalid_request`", body = ErrorEnvelope),
        (status = 413, description = "The body is over 8 KiB: `too_large`", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "The orchestrator is unavailable: `unavailable`", body = ErrorEnvelope),
    ),
)]
async fn describe_invite(State(s): S, headers: HeaderMap, ApiJson(b): ApiJson<DescribeInviteBody>) -> Response {
    let start = Instant::now();
    let mut answer = describe_invite_inner(&s, &headers, b).await;
    pad(start, s.config.min_response).await;
    answer
        .headers_mut()
        .insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    answer
}

async fn describe_invite_inner(s: &AppState, headers: &HeaderMap, q: DescribeInviteBody) -> Response {
    let ip = client_ip(headers);
    if !allowed(s, &[(format!("describe-invite:ip:{ip}"), 60, HOUR)]) {
        return error(StatusCode::TOO_MANY_REQUESTS, ErrorCode::RateLimited, TOO_MANY);
    }
    let not_found = || error(StatusCode::NOT_FOUND, ErrorCode::NotFound, NO_SUCH_INVITE);
    if !is_user_id(&q.user) || !is_invite_token(&q.t) {
        return not_found();
    }
    let Some(orchestrator) = &s.signup_gate else {
        warn!("describe-invite needs the orchestrator's internal client");
        return error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE);
    };
    let token_hash = hex::encode(Sha256::digest(q.t.as_bytes()));
    match orchestrator.describe_invite(&q.user, &token_hash).await {
        Ok(Some(d)) => (StatusCode::OK, Json(d)).into_response(),
        Ok(None) => not_found(),
        Err(e) => {
            warn!(error = %e, "describe-invite: the orchestrator failed");
            error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE)
        }
    }
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct AcceptInviteBody {
    /// The Zitadel user id from the invite link.
    user: String,
    /// The invite code from the link. Send this or `acceptTicket`.
    #[serde(default)]
    code: Option<String>,
    /// From an `invite_incomplete` error: retries the steps after the (spent) code.
    #[serde(default)]
    accept_ticket: Option<String>,
    /// The link's `reset=1`: `code` is a password-reset code (a person who already had a way to
    /// sign in, whom Zitadel gives no invite code). It sets the password itself.
    #[serde(default)]
    reset: bool,
    password: String,
    /// The pending sign-in to finish; without it the answer sends the browser to Studio.
    #[serde(default)]
    auth_request: Option<String>,
}

#[utoipa::path(
    post,
    path = "/auth/v1/accept-invite",
    operation_id = "acceptInvite",
    tag = "login",
    description = "Accepts a member invite: checks the emailed code (with `reset`, a password-reset code, which sets the password itself), sets the password, marks the email verified and signs in. Sets the HttpOnly session cookie. If a step after the code check fails, the error is `invite_incomplete` (or `weak_password`) with `error.accept_ticket`: send it back in place of `code` to finish. A pending sign-in that has expired doesn't fail the accept: `redirect` is then Studio's address.",
    request_body = AcceptInviteBody,
    responses(
        (status = 200, description = "Signed in: `next` is `done`, the session cookie is set", body = SignedIn),
        (status = 400, description = "The body is malformed, the code or ticket is wrong (`invalid_request`), or the password is refused (`weak_password`, with `accept_ticket` once the code was spent)", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON (wrong content type): `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "The body is JSON but has the wrong fields: `invalid_request`", body = ErrorEnvelope),
        (status = 413, description = "The body is over 8 KiB: `too_large`", body = ErrorEnvelope),
        (status = 410, description = "The invite expired (`invite_expired`) or was revoked (`invite_revoked`)", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "Zitadel is unavailable (`unavailable`), or a step after the code check failed (`invite_incomplete`, with `accept_ticket`)", body = ErrorEnvelope),
    ),
)]
async fn accept_invite(State(s): S, headers: HeaderMap, ApiJson(b): ApiJson<AcceptInviteBody>) -> Response {
    let start = Instant::now();
    let mut answer = accept_invite_inner(&s, &headers, b).await;
    pad(start, s.config.min_response + SIGN_UP_EXTRA).await;
    // Answers can carry a ticket or set a session: nothing may keep them.
    answer
        .headers_mut()
        .insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    answer
}

/// The error keys in a `ZError::Rejected` message: its `" | "`-separated parts (the top-level
/// message, then each `details[]` id and message), each without a trailing `" (ID)"`.
fn keys(message: &str) -> impl Iterator<Item = &str> {
    message.split(" | ").map(|part| match part.rsplit_once(" (") {
        Some((key, id)) if id.ends_with(')') => key,
        _ => part,
    })
}

/// Whether one of Zitadel's error keys is exactly `key`.
fn has_key(message: &str, key: &str) -> bool {
    keys(message).any(|k| k == key)
}

fn invite_expired(message: &str) -> bool {
    has_key(message, "Errors.User.Code.Expired")
}

/// Zitadel's password policy keys: `Errors.User.PasswordComplexityPolicy.*` (for example
/// `.MinLength`) and `Errors.User.Password.*`.
fn password_refused(message: &str) -> bool {
    keys(message).any(|k| {
        k.starts_with("Errors.User.PasswordComplexityPolicy.") || k.starts_with("Errors.User.Password.")
    })
}

/// A step after the code was spent failed: tell the browser how to finish.
fn incomplete(s: &AppState, user: &str, status: StatusCode, code: ErrorCode, message: &str) -> Response {
    let ticket = seal_ticket(
        &s.config.cookie_key,
        &AcceptTicket {
            user_id: user.to_string(),
            exp: now_secs() + TICKET_TTL_SECS,
            nonce: uuid::Uuid::new_v4().simple().to_string(),
        },
    );
    ApiError::new(status.as_u16(), code, message)
        .with_extra("accept_ticket", json!(ticket))
        .into_response()
}

fn incomplete_unavailable(s: &AppState, user: &str, e: &ZError) -> Response {
    warn!(error = %e, "accept-invite: a step after the code check failed");
    incomplete(s, user, StatusCode::SERVICE_UNAVAILABLE, ErrorCode::InviteIncomplete, INVITE_INCOMPLETE)
}

async fn accept_invite_inner(s: &AppState, headers: &HeaderMap, b: AcceptInviteBody) -> Response {
    let ip = client_ip(headers);
    if !allowed(s, &[(format!("accept-invite:ip:{ip}"), 20, HOUR)]) {
        return error(StatusCode::TOO_MANY_REQUESTS, ErrorCode::RateLimited, TOO_MANY);
    }
    let bad = || error(StatusCode::BAD_REQUEST, ErrorCode::InvalidRequest, BAD_INVITE);
    // Validate before any key is built from the input.
    let code = b.code.as_deref().map(str::trim);
    if !is_user_id(&b.user)
        || code.is_some() == b.accept_ticket.is_some()
        || code.is_some_and(|c| c.is_empty() || c.len() > 20)
    {
        return bad();
    }
    if b.auth_request.as_deref().is_some_and(|ar| !is_auth_request_id(ar)) {
        return error(StatusCode::GONE, ErrorCode::SignInExpired, GONE);
    }
    // Before the code is spent.
    if !strong_password(&b.password) {
        return error(StatusCode::BAD_REQUEST, ErrorCode::WeakPassword, WEAK_PASSWORD);
    }
    // Setting a password and marking the email verified need Zitadel's admin token; without it
    // the code would be spent for nothing.
    let Some(admin) = &s.admin else {
        warn!("accept-invite needs CHELABASE_ZITADEL_ADMIN_PAT_FILE");
        return error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE);
    };
    // Only failed code attempts count against a user, so a typo'd password or a retry never
    // locks the invitee out.
    let user_key = format!("accept-invite:user:{}", b.user);
    if s.limiter.over(&user_key, 5, HOUR) {
        return error(StatusCode::TOO_MANY_REQUESTS, ErrorCode::RateLimited, TOO_MANY);
    }
    let user = match admin.get_user(&b.user).await {
        Ok(Some(u)) => u,
        Ok(None) => return error(StatusCode::GONE, ErrorCode::InviteRevoked, INVITE_REVOKED),
        Err(e) => return unavailable(&e),
    };
    // An invite code for a user of another organization never signs into the platform.
    if user.org_id != s.config.org_id {
        warn!("accept-invite for a user outside the platform org");
        s.limiter.hit(&user_key, HOUR);
        return bad();
    }
    // A reset code is checked by setting the password with it: nothing changes on a wrong one.
    let mut password_set = false;
    match (code, &b.accept_ticket) {
        (Some(code), _) if b.reset => match admin.set_password(&b.user, code, &b.password).await {
            Ok(()) => password_set = true,
            Err(ZError::Rejected { status, message }) if has_key(&message, "Errors.User.NotFound") => {
                info!(status, "accept-invite: the user is gone");
                return error(StatusCode::GONE, ErrorCode::InviteRevoked, INVITE_REVOKED);
            }
            Err(ZError::Rejected { message, .. }) if invite_expired(&message) => {
                return error(StatusCode::GONE, ErrorCode::InviteExpired, INVITE_EXPIRED);
            }
            // The code wasn't spent: no ticket.
            Err(ZError::Rejected { message, .. }) if password_refused(&message) => {
                return error(StatusCode::BAD_REQUEST, ErrorCode::WeakPassword, WEAK_PASSWORD);
            }
            Err(ZError::Rejected { status: status @ (400 | 412), message }) => {
                warn!(status, %message, "accept-invite: the reset code was refused");
                s.limiter.hit(&user_key, HOUR);
                return bad();
            }
            Err(e) => return unavailable(&e),
        },
        (Some(code), _) => match admin.verify_invite_code(&b.user, code).await {
            Ok(()) => {}
            Err(ZError::Rejected { status, message }) if has_key(&message, "Errors.User.NotFound") => {
                info!(status, "accept-invite: the user is gone");
                return error(StatusCode::GONE, ErrorCode::InviteRevoked, INVITE_REVOKED);
            }
            Err(ZError::Rejected { message, .. }) if invite_expired(&message) => {
                return error(StatusCode::GONE, ErrorCode::InviteExpired, INVITE_EXPIRED);
            }
            Err(ZError::Rejected { status: status @ (400 | 412), message }) => {
                warn!(status, %message, "accept-invite: the code was refused");
                s.limiter.hit(&user_key, HOUR);
                return bad();
            }
            Err(e) => return unavailable(&e),
        },
        (None, Some(ticket)) => {
            // Claimed up front: a failed attempt hands back a fresh ticket, so this one is
            // spent either way, and two concurrent uses can't both pass.
            match open_ticket(&s.config.cookie_key, ticket, now_secs()) {
                Some(t)
                    if t.user_id == b.user
                        && s.used_tickets.claim(&t.nonce, t.exp, now_secs()) => {}
                _ => return bad(),
            }
        }
        (None, None) => return bad(),
    }
    // From here the code is spent: any failure hands back a ticket. Every step is safe to
    // repeat.
    // I1: a deactivated user (a removed member invited again) comes back only now, with a
    // verified code; the invite itself leaves them deactivated.
    if user.state == UserState::Inactive {
        match admin.reactivate_user(&b.user).await {
            Ok(()) | Err(ZError::Rejected { status: 400 | 412, .. }) => {}
            Err(e) => return incomplete_unavailable(s, &b.user, &e),
        }
    }
    if !password_set {
        match admin.set_password_admin(&b.user, &b.password).await {
            Ok(()) => {}
            Err(ZError::Rejected { message, .. }) if password_refused(&message) => {
                return incomplete(s, &b.user, StatusCode::BAD_REQUEST, ErrorCode::WeakPassword, WEAK_PASSWORD);
            }
            Err(e) => return incomplete_unavailable(s, &b.user, &e),
        }
    }
    if let Err(e) = admin.mark_email_verified(&b.user, &user.email).await {
        return incomplete_unavailable(s, &b.user, &e);
    }
    let session = match new_user_session(s, &b.user, &b.password).await {
        Ok(session) => session,
        Err(e) => return incomplete_unavailable(s, &b.user, &e),
    };
    match b.auth_request {
        Some(ar) => match s.zitadel.finish(&ar, &session).await {
            Ok(redirect) => signed_in(s, session, redirect),
            // The account is complete; the pending sign-in just ended, so Studio's sign-in
            // takes over.
            Err(ZError::Rejected { .. }) => signed_in(s, session, s.config.studio_url.clone()),
            Err(e) => incomplete_unavailable(s, &b.user, &e),
        },
        None => signed_in(s, session, s.config.studio_url.clone()),
    }
}

/// A sign-up for a taken email. With verification on (platform), it's answered exactly like a
/// new one, with a cookie that can't verify. With it off (self-hosted), a new sign-up has no
/// code step to hide behind, so the user is told and Studio sends them to sign in.
fn taken(s: &Arc<AppState>, email: &str, auth_request: String) -> Response {
    if s.config.skip_email_verification {
        info!(email = %email_tag(email), "sign-up for a taken email");
        return error(StatusCode::CONFLICT, ErrorCode::EmailTaken, ACCOUNT_EXISTS);
    }
    decoy(s, email, auth_request)
}

/// Answer a sign-up for a taken email exactly like a new one, with a cookie that can't verify.
/// The account's owner gets a password-reset email instead, so a real owner who forgot they
/// signed up has a way back in.
fn decoy(s: &Arc<AppState>, email: &str, auth_request: String) -> Response {
    info!(email = %email_tag(email), "sign-up for a taken email (decoy)");
    spawn_reset_email(s, email.to_string());
    let decoy = LoginState {
        auth_request,
        user_id: None,
        session_id: None,
        session_token: None,
        step: Step::Verify,
        decoy: true,
        exp: now_secs() + LOGIN_TTL_SECS,
    };
    reply(
        StatusCode::OK,
        next_step(&NextStep::Verify),
        vec![login_cookie(s, &decoy)],
    )
}

/// Zitadel's session API sees a new user only once its projections catch up (a 404 until then).
async fn new_user_session(s: &AppState, user_id: &str, password: &str) -> Result<Session, ZError> {
    let mut attempt = 0;
    loop {
        match s.zitadel.create_session(user_id, password).await {
            Err(ZError::Rejected { status: 404, .. }) if attempt < 10 => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            other => return other,
        }
    }
}

/// The rate-limit key for a pending code step: the user, or for a decoy its pending sign-in,
/// so real and decoy cookies are limited alike.
fn step_key(state: &LoginState) -> String {
    state
        .user_id
        .clone()
        .unwrap_or_else(|| state.auth_request.clone())
}

fn current_login(s: &AppState, headers: &HeaderMap) -> Option<LoginState> {
    read_cookie(headers, LOGIN_COOKIE)
        .and_then(|raw| open_login(&s.config.cookie_key, raw, now_secs()))
}

#[derive(Deserialize, ToSchema)]
struct CodeBody {
    code: String,
}

#[utoipa::path(
    post,
    path = "/auth/v1/verify-email",
    operation_id = "verifyEmail",
    tag = "login",
    description = "Checks the emailed code. Reads the HttpOnly step cookie; on success sets the HttpOnly session cookie and clears the step cookie.",
    request_body = CodeBody,
    responses(
        (status = 200, description = "Email verified and signed in: `next` is `done`, the session cookie is set and the step cookie cleared", body = SignedIn),
        (status = 400, description = "The body is malformed (`invalid_request`) or the code is wrong (`invalid_code`)", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON (wrong content type): `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "The body is JSON but has the wrong fields: `invalid_request`", body = ErrorEnvelope),
        (status = 413, description = "The body is over 8 KiB: `too_large`", body = ErrorEnvelope),
        (status = 410, description = "This sign-in has expired or is unknown: `sign_in_expired`", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "Zitadel is unavailable: `unavailable`", body = ErrorEnvelope),
    ),
)]
async fn verify_email(State(s): S, headers: HeaderMap, ApiJson(b): ApiJson<CodeBody>) -> Response {
    let start = Instant::now();
    let answer = verify_email_inner(&s, &headers, b).await;
    pad(start, s.config.min_response).await;
    answer
}

async fn verify_email_inner(s: &AppState, headers: &HeaderMap, b: CodeBody) -> Response {
    let state = current_login(s, headers).filter(|st| st.step == Step::Verify);
    let mut checks = vec![(format!("verify:ip:{}", client_ip(headers)), 10, MINUTE)];
    if let Some(st) = &state {
        checks.push((format!("verify:step:{}", step_key(st)), 10, HOUR));
    }
    if !allowed(s, &checks) {
        return error(StatusCode::TOO_MANY_REQUESTS, ErrorCode::RateLimited, TOO_MANY);
    }
    // No (or an expired) step cookie: the sign-in is over, whatever the code.
    let Some(state) = state else {
        return error(StatusCode::GONE, ErrorCode::SignInExpired, GONE);
    };
    let (Some(user_id), Some(session_id), Some(session_token), false) = (
        state.user_id,
        state.session_id,
        state.session_token,
        state.decoy,
    ) else {
        return error(StatusCode::BAD_REQUEST, ErrorCode::InvalidCode, BAD_CODE);
    };
    match s.zitadel.verify_email(&user_id, b.code.trim()).await {
        Ok(()) => {
            finish(
                s,
                &state.auth_request,
                Session {
                    id: session_id,
                    token: session_token,
                },
            )
            .await
        }
        Err(ZError::Rejected { .. }) => error(StatusCode::BAD_REQUEST, ErrorCode::InvalidCode, BAD_CODE),
        Err(e) => unavailable(&e),
    }
}

#[derive(Deserialize, ToSchema)]
struct EmptyRequest {}

#[utoipa::path(
    post,
    path = "/auth/v1/resend-code",
    operation_id = "resendCode",
    tag = "login",
    description = "Sends the verification code again. Reads the HttpOnly step cookie. Always answers the same, whether or not a code was sent.",
    request_body = EmptyRequest,
    responses(
        (status = 200, description = "Done", body = EmptyResponse),
        (status = 400, description = "The body is malformed: `invalid_request`", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON (wrong content type): `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "The body is JSON but has the wrong fields: `invalid_request`", body = ErrorEnvelope),
        (status = 413, description = "The body is over 8 KiB: `too_large`", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
    ),
)]
async fn resend_code(State(s): S, headers: HeaderMap, ApiJson(_): ApiJson<EmptyRequest>) -> Response {
    let start = Instant::now();
    let answer = resend_code_inner(&s, &headers).await;
    pad(start, s.config.min_response).await;
    answer
}

async fn resend_code_inner(s: &AppState, headers: &HeaderMap) -> Response {
    let state = current_login(s, headers).filter(|st| st.step == Step::Verify);
    let mut checks = vec![(format!("resend:ip:{}", client_ip(headers)), 20, HOUR)];
    if let Some(st) = &state {
        checks.push((format!("resend:step:{}", step_key(st)), 5, HOUR));
    }
    if !allowed(s, &checks) {
        return error(StatusCode::TOO_MANY_REQUESTS, ErrorCode::RateLimited, TOO_MANY);
    }
    if let Some(user) = state.and_then(|st| st.user_id)
        && let Err(e) = s.zitadel.resend_code(&user, &verify_template(s)).await
    {
        warn!(error = %e, "resending the verification code failed");
    }
    reply(StatusCode::OK, json!(EmptyResponse {}), Vec::new())
}

#[derive(Deserialize, ToSchema)]
struct EmailBody {
    email: String,
}

#[utoipa::path(
    post,
    path = "/auth/v1/password-reset",
    operation_id = "requestPasswordReset",
    tag = "login",
    description = "Emails a password-reset link if the account exists. Always answers the same, whether or not it does.",
    request_body = EmailBody,
    responses(
        (status = 200, description = "Done", body = EmptyResponse),
        (status = 400, description = "The body is malformed: `invalid_request`", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON (wrong content type): `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "The body is JSON but has the wrong fields: `invalid_request`", body = ErrorEnvelope),
        (status = 413, description = "The body is over 8 KiB: `too_large`", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
    ),
)]
async fn password_reset(State(s): S, headers: HeaderMap, ApiJson(b): ApiJson<EmailBody>) -> Response {
    let email = normalize_email(&b.email);
    let ip = client_ip(&headers);
    if !allowed(
        &s,
        &[
            (format!("reset:ip:{ip}"), 20, HOUR),
            (format!("reset:email:{}", email_tag(&email)), 5, HOUR),
        ],
    ) {
        return error(StatusCode::TOO_MANY_REQUESTS, ErrorCode::RateLimited, TOO_MANY);
    }
    spawn_reset_email(&s, email);
    reply(StatusCode::OK, json!(EmptyResponse {}), Vec::new())
}

/// Send a reset link to `email`'s account, if it has one, in the background, so the answer
/// time doesn't reveal whether the user exists.
fn spawn_reset_email(s: &Arc<AppState>, email: String) {
    let s2 = s.clone();
    tokio::spawn(async move {
        let template = format!(
            "{}/password-reset?userId={{{{.UserID}}}}&code={{{{.Code}}}}",
            s2.config.studio_url
        );
        match s2
            .zitadel
            .find_user_by_email(&s2.config.org_id, &email)
            .await
        {
            Ok(Some(user_id)) => {
                if let Err(e) = s2.zitadel.send_password_reset(&user_id, &template).await {
                    warn!(error = %e, "sending the password reset failed");
                }
            }
            Ok(None) => info!(email = %email_tag(&email), "password reset for an unknown email"),
            Err(e) => warn!(error = %e, "password reset lookup failed"),
        }
    });
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct ConfirmBody {
    user_id: String,
    code: String,
    password: String,
}

#[utoipa::path(
    post,
    path = "/auth/v1/password-reset/confirm",
    operation_id = "confirmPasswordReset",
    tag = "login",
    description = "Sets a new password with the code from the reset email.",
    request_body = ConfirmBody,
    responses(
        (status = 200, description = "Done", body = EmptyResponse),
        (status = 400, description = "The body is malformed (`invalid_request`), the password is too weak (`weak_password`) or the code is wrong (`invalid_code`)", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON (wrong content type): `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "The body is JSON but has the wrong fields: `invalid_request`", body = ErrorEnvelope),
        (status = 413, description = "The body is over 8 KiB: `too_large`", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "Zitadel is unavailable: `unavailable`", body = ErrorEnvelope),
    ),
)]
async fn password_reset_confirm(
    State(s): S,
    headers: HeaderMap,
    ApiJson(b): ApiJson<ConfirmBody>,
) -> Response {
    let start = Instant::now();
    let answer = confirm_inner(&s, &headers, b).await;
    pad(start, s.config.min_response).await;
    answer
}

async fn confirm_inner(s: &AppState, headers: &HeaderMap, b: ConfirmBody) -> Response {
    let user_id = b.user_id.trim();
    let mut checks = vec![(format!("confirm:ip:{}", client_ip(headers)), 10, MINUTE)];
    if is_user_id(user_id) {
        checks.push((format!("confirm:user:{user_id}"), 10, HOUR));
    }
    if !allowed(s, &checks) {
        return error(StatusCode::TOO_MANY_REQUESTS, ErrorCode::RateLimited, TOO_MANY);
    }
    // Checked before Zitadel sees the user id, so this answer can't tell ids apart.
    if !strong_password(&b.password) {
        return error(StatusCode::BAD_REQUEST, ErrorCode::WeakPassword, WEAK_PASSWORD);
    }
    if !is_user_id(user_id) {
        return error(StatusCode::BAD_REQUEST, ErrorCode::InvalidCode, BAD_CODE);
    }
    match s
        .zitadel
        .set_password(user_id, b.code.trim(), &b.password)
        .await
    {
        Ok(()) => reply(StatusCode::OK, json!(EmptyResponse {}), Vec::new()),
        Err(ZError::Rejected { .. }) => error(StatusCode::BAD_REQUEST, ErrorCode::InvalidCode, BAD_CODE),
        Err(e) => unavailable(&e),
    }
}

#[utoipa::path(
    post,
    path = "/auth/v1/logout",
    operation_id = "logout",
    tag = "login",
    description = "Ends the session and clears the HttpOnly session and step cookies.",
    request_body = EmptyRequest,
    responses(
        (status = 200, description = "Done", body = EmptyResponse),
        (status = 400, description = "The body is malformed: `invalid_request`", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON (wrong content type): `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "The body is JSON but has the wrong fields: `invalid_request`", body = ErrorEnvelope),
        (status = 413, description = "The body is over 8 KiB: `too_large`", body = ErrorEnvelope),
    ),
)]
async fn logout(State(s): S, headers: HeaderMap, ApiJson(_): ApiJson<EmptyRequest>) -> Response {
    if let Some(kept) = read_cookie(&headers, SESSION_COOKIE)
        .and_then(|raw| open_session(&s.config.cookie_key, raw, now_secs()))
        && let Err(e) = s
            .zitadel
            .delete_session(&Session {
                id: kept.session_id,
                token: kept.session_token,
            })
            .await
    {
        warn!(error = %e, "ending the Zitadel session failed");
    }
    let secure = s.config.secure_cookies;
    reply(
        StatusCode::OK,
        json!(EmptyResponse {}),
        vec![
            clear_cookie(LOGIN_COOKIE, secure),
            clear_cookie(SESSION_COOKIE, secure),
        ],
    )
}
