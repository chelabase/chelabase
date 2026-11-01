//! The HTTP API Studio's sign-in pages call (through the gateway).

use crate::config::Config;
use crate::cookie::*;
use crate::guard::{RateLimiter, Slot, UsedTickets, client_ip, email_tag, pad};
use chelabase_zitadel::refusals;
use chelabase_zitadel::rules::{normalize_email, strong_password};
use crate::app_users::internal::OpenInvite;
use crate::zitadel::{NewUser, Session, UserState, ZError, ZUser, Zitadel};
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
    pub const SETUP_CODE_INVALID: &str =
        "The setup code is missing or wrong; the first account of a new instance needs it";
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
        .routes(routes!(instance))
        .routes(routes!(studio_config))
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

/// Serve it with `into_make_service_with_connect_info::<SocketAddr>()`: the client address
/// rate limits key on comes from the TCP peer (see [`forwarded_for`]).
pub fn router(state: Arc<AppState>) -> Router {
    let (platform, _) = platform_router().split_for_parts();
    let (app_users, _) = crate::app_users::flows::api_router().split_for_parts();
    platform
        // Readiness for compose `--wait` and k8s: not ready until the bootstrap's ids are there.
        .route("/health", axum::routing::get(health))
        // Every body here is a few short fields; this also bounds rate-limit keys.
        .layer(DefaultBodyLimit::max(8 * 1024))
        // Sign-up's `data` may carry a few columns' values.
        .merge(app_users.layer(DefaultBodyLimit::max(64 * 1024)))
        .layer(axum::middleware::from_fn_with_state(state.clone(), forwarded_for))
        .with_state(state)
}

/// Replaces every `X-Forwarded-For` with the one client address the handlers key on
/// ([`crate::guard::client_ip_from`]): the gateway's rightmost entry when the TCP peer is a
/// trusted proxy, else the peer itself. Without the peer (served without connect info) the
/// client is `unknown`, never a forwarded value (RT1 T1 ruling (e): computes reach auth directly).
async fn forwarded_for(
    State(state): S,
    mut req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let peer = req
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|c| c.0.ip());
    let client = match peer {
        Some(peer) => crate::guard::client_ip_from(
            req.headers(),
            peer,
            &state.config.trusted_proxies.current().await,
        ),
        None => "unknown".to_string(),
    };
    let headers = req.headers_mut();
    headers.remove("x-forwarded-for");
    if let Ok(value) = axum::http::HeaderValue::from_str(&client) {
        headers.insert("x-forwarded-for", value);
    }
    next.run(req).await
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
    /// This instance takes sign-ups (open sign-up is on, or the tenant has no member yet and a
    /// setup code is configured): the sign-in page may offer "Create account". Always `false`
    /// in platform mode.
    sign_up_allowed: bool,
}

/// Studio's runtime config: the six public values it signs in with. Nothing else is here (no
/// PAT, key or secret): it is public and cached.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StudioConfig {
    /// Zitadel's issuer URL (the OIDC authority).
    issuer: String,
    /// Studio's OIDC client id.
    client_id: String,
    /// The `chelabase` project id (the API audience Studio asks for).
    project_id: String,
    /// The platform org's id.
    org_id: String,
    /// The public API base URL.
    api_url: String,
    /// Studio's public host.
    studio_host: String,
}

/// Every bootstrap output auth waits for, with why it isn't usable yet (`Ok` when it is).
fn bootstrap_outputs(s: &AppState) -> Vec<Result<&str, chelabase_types::file_value::Unavailable>> {
    std::iter::once(&s.config.org_id)
        .chain(s.config.studio.client_id.as_ref())
        .chain(s.config.studio.project_id.as_ref())
        .map(|v| v.get())
        .collect()
}

/// 200 once every bootstrap output auth reads is there, else 503 naming the variables that
/// wait (never a value).
async fn health(State(s): S) -> Response {
    let waiting: Vec<String> = bootstrap_outputs(&s)
        .into_iter()
        .filter_map(|r| r.err())
        .map(|e| {
            not_ready(&e);
            e.to_string()
        })
        .collect();
    if waiting.is_empty() {
        return reply(StatusCode::OK, json!({"status": "ok"}), Vec::new());
    }
    reply(
        StatusCode::SERVICE_UNAVAILABLE,
        json!({"status": "waiting for the bootstrap", "waiting": waiting}),
        Vec::new(),
    )
}

#[utoipa::path(
    get,
    path = "/auth/v1/studio-config",
    operation_id = "getStudioConfig",
    tag = "login",
    description = "Studio's runtime config: the Zitadel issuer, Studio's client id, the project and org ids, the API URL and Studio's host. Public; cacheable for 60 s.",
    responses(
        (status = 200, description = "The config (`Cache-Control: public, max-age=60`)", body = StudioConfig),
        (status = 404, description = "Studio's ids aren't configured here (a dev Studio uses its build values): `not_found`", body = ErrorEnvelope),
        (status = 503, description = "The bootstrap hasn't written the ids yet: `unavailable`", body = ErrorEnvelope),
    ),
)]
async fn studio_config(State(s): S) -> Response {
    let no_store = |mut r: Response| {
        r.headers_mut()
            .insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
        r
    };
    let studio = &s.config.studio;
    let (Some(client_id), Some(project_id)) = (&studio.client_id, &studio.project_id) else {
        return no_store(error(
            StatusCode::NOT_FOUND,
            ErrorCode::NotFound,
            "Studio's runtime config isn't set up on this instance",
        ));
    };
    let values = (s.config.org_id.get(), client_id.get(), project_id.get());
    let (Ok(org_id), Ok(client_id), Ok(project_id)) = values else {
        for e in [values.0.err(), values.1.err(), values.2.err()].into_iter().flatten() {
            not_ready(&e);
        }
        return no_store(error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE));
    };
    let body = StudioConfig {
        issuer: s.config.issuer.clone(),
        client_id: client_id.to_string(),
        project_id: project_id.to_string(),
        org_id: org_id.to_string(),
        api_url: studio.api_url.clone(),
        studio_host: studio.studio_host.clone(),
    };
    let mut r = reply(StatusCode::OK, json!(body), Vec::new());
    r.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("public, max-age=60"),
    );
    r
}

/// What the sign-up page needs to know about this instance.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct InstanceInfo {
    /// The instance is unclaimed (its tenant has no member yet): the first sign-up needs the
    /// setup code that `chelabase-init secrets` printed. `false` once anyone is in, without a
    /// configured code (then no first sign-up is taken at all), and in platform mode.
    setup_required: bool,
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

/// The platform org's id, or the 503 `unavailable` every route that needs it answers while
/// the bootstrap hasn't written it (m5: `CHELABASE_ZITADEL_ORG_ID_FILE` may appear after auth
/// starts). Nothing runs against a missing org: `None` is answered with [`no_org`].
fn org_id(s: &AppState) -> Option<&str> {
    s.config.org_id.get().map_err(|e| not_ready(&e)).ok()
}

fn no_org() -> Response {
    error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE)
}

/// Logs why a bootstrap output isn't usable yet: the path, never a value.
fn not_ready(e: &chelabase_types::file_value::Unavailable) {
    match e {
        chelabase_types::file_value::Unavailable::NotYet { .. } => {
            info!("waiting for the bootstrap: {e}")
        }
        chelabase_types::file_value::Unavailable::Refused { .. } => warn!("{e}"),
    }
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
            // A fresh install waiting for its code takes a sign-up (with the code).
            let sign_up_allowed = gate(&s, None)
                .await
                .is_ok_and(|a| a.allowed || a.setup_required);
            reply(
                StatusCode::OK,
                json!(AuthRequestInfo { sign_up: asked && sign_up_allowed, sign_up_allowed }),
                Vec::new(),
            )
        }
        Err(e) => pending_sign_in_error(e),
    }
}

#[utoipa::path(
    get,
    path = "/auth/v1/instance",
    operation_id = "getInstance",
    tag = "login",
    description = "Whether this instance is unclaimed, so the sign-up page asks for the setup code. Sets no cookie.",
    responses(
        (status = 200, description = "The instance's sign-up state", body = InstanceInfo),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "The sign-up gate is unavailable: `unavailable`", body = ErrorEnvelope),
    ),
)]
async fn instance(State(s): S, headers: HeaderMap) -> Response {
    let ip = client_ip(&headers);
    if !allowed(&s, &[(format!("instance:ip:{ip}"), 60, MINUTE)]) {
        return error(StatusCode::TOO_MANY_REQUESTS, ErrorCode::RateLimited, TOO_MANY);
    }
    if ModePolicy::new(s.config.mode).sign_up() == SignUpRule::Refused {
        return reply(StatusCode::OK, json!(InstanceInfo { setup_required: false }), Vec::new());
    }
    match gate(&s, None).await {
        Ok(a) => reply(
            StatusCode::OK,
            json!(InstanceInfo { setup_required: a.setup_required }),
            Vec::new(),
        ),
        Err(refused) => refused.response(),
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
    if !allowed(s, &[(format!("sign-in:ip:{ip}"), 10, MINUTE)]) {
        return error(StatusCode::TOO_MANY_REQUESTS, ErrorCode::RateLimited, TOO_MANY);
    }
    // Per email, only failed password checks count: signing in as someone takes their password,
    // so nobody can use up a known account's attempts. The attempt is reserved up front, so
    // concurrent requests can't all pass a check before any failure lands, and a client that
    // hangs up during the check still spends it. Only these outcomes give it back:
    //
    // | Outcome                                                        | Answer  | Email slot |
    // |----------------------------------------------------------------|---------|------------|
    // | the email's 5 a minute are used                                | 429     | none held  |
    // | `authRequest` isn't an auth request id                         | 410     | refunded   |
    // | no org yet (the bootstrap hasn't written it)                   | 503     | refunded   |
    // | Zitadel refuses the auth request, or is unavailable            | 410/503 | refunded   |
    // | the user lookup fails                                          | 503     | refunded   |
    // | no user has this email                                         | 401     | counts     |
    // | the password check is refused (any `Rejected`, see below)      | 401     | counts     |
    // | the password check is unavailable (`Unavailable`)              | 503     | refunded   |
    // | the password is right (refunded before anything else awaits)   | any     | refunded   |
    // | the request is dropped undecided (the client hung up)          | none    | counts     |
    //
    // A refused password check is any refusal of Zitadel's own: a wrong password
    // (`refusals::PASSWORD_INVALID`), no such user (`SESSION_USER_NOT_FOUND`), a locked or
    // inactive user (`SESSION_USER_LOCKED`, `SESSION_USER_NOT_ACTIVE`), or its request validation
    // (`ZError::is_invalid_request`, e.g. a password over 200 characters: the caller's input).
    // `Unavailable` is no answer, a 5xx, or a 4xx that isn't Zitadel's own error. An unknown
    // email and a wrong password both count and answer the same 401, padded alike.
    let Some(slot) = Slot::reserve(
        &s.limiter,
        format!("sign-in:email:{}", email_tag(&email)),
        5,
        MINUTE,
    ) else {
        return error(StatusCode::TOO_MANY_REQUESTS, ErrorCode::RateLimited, TOO_MANY);
    };
    // Counts: the slot is dropped without a refund.
    let refused = || error(StatusCode::UNAUTHORIZED, ErrorCode::InvalidCredentials, BAD_CREDENTIALS);
    if !is_auth_request_id(&b.auth_request) {
        slot.refund();
        return error(StatusCode::GONE, ErrorCode::SignInExpired, GONE);
    }
    let Some(org) = org_id(s) else {
        slot.refund();
        return no_org();
    };
    if let Err(e) = s.zitadel.auth_request(&b.auth_request).await {
        slot.refund();
        return pending_sign_in_error(e);
    }
    let user_id = match s.zitadel.find_user_by_email(org, &email).await {
        Ok(Some(id)) => id,
        Ok(None) => return refused(),
        Err(e) => {
            slot.refund();
            return unavailable(&e);
        }
    };
    let session = match s.zitadel.create_session(&user_id, &b.password).await {
        Ok(session) => {
            slot.refund();
            session
        }
        Err(ZError::Rejected { .. }) => {
            info!(email = %email_tag(&email), "sign-in refused");
            return refused();
        }
        Err(e @ ZError::Unavailable(_)) => {
            slot.refund();
            return unavailable(&e);
        }
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
    /// The setup code that `chelabase-init secrets` printed: the first sign-up of an unclaimed
    /// instance needs it (case and dashes don't matter). Ignored once the instance is claimed.
    #[serde(default)]
    setup_code: Option<String>,
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
        (status = 403, description = "Sign-up is closed for this instance (`sign_up_closed`), the unclaimed instance needs its setup code and it is missing or wrong (`setup_code_invalid`; no account is created), or not available in platform mode (`not_available_in_platform_mode`)", body = ErrorEnvelope),
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
    let setup_code = b.setup_code.as_deref().map(str::trim).filter(|c| !c.is_empty());
    // Code guesses count against these too. No bucket is shared by every client: anyone could
    // drain it with junk and lock the operator out, and the code's ~98 bits need no global cap.
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
    if !usable_password(&b.password) {
        return error(StatusCode::BAD_REQUEST, ErrorCode::WeakPassword, WEAK_PASSWORD);
    }
    let Some(org) = org_id(s) else {
        return no_org();
    };
    if let Err(e) = s.zitadel.auth_request(&b.auth_request).await {
        return pending_sign_in_error(e);
    }
    let nonce = match sign_up_gate(s, setup_code).await {
        Ok(nonce) => nonce,
        Err(refused) => return refused.response(),
    };
    // A claim that fails after the account exists is undone by deleting it, which needs the
    // admin client (the login client can't delete users): without it, no claim is attempted.
    if nonce.is_some() && s.admin.is_none() {
        warn!("a setup-code sign-up needs the admin Zitadel client (CHELABASE_ZITADEL_ADMIN_PAT_FILE)");
        return error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE);
    }
    // Zitadel checks usernames, not emails, for uniqueness: never make a second account
    // for an email that already has one.
    // Two accounts already sharing the email (made outside this check) count as taken too.
    match s.zitadel.find_user_in_org(org, &email).await {
        Ok(crate::zitadel::EmailMatch::None) => {}
        Ok(_) => return taken(s, &email, b.auth_request),
        Err(e) => return unavailable(&e),
    }
    let template = verify_template(s);
    let new = NewUser {
        org_id: org,
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
    if let Some(nonce) = &nonce
        && let Some(refused) = bind_claim(s, org, &user_id, nonce).await
    {
        return refused.response();
    }
    let session = match new_user_session(s, &user_id, &b.password).await {
        Ok(session) => session,
        Err(e) => return unavailable(&e),
    };
    if s.config.skip_email_verification {
        return finish(s, &b.auth_request, session).await;
    }
    verify_step(s, &b.auth_request, user_id, session).await
}

/// Why the sign-up gate refused; [`Refusal::response`] is its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Refusal {
    Platform,
    Closed,
    SetupCodeInvalid,
    Unavailable,
}

impl Refusal {
    fn response(self) -> Response {
        match self {
            Self::Platform => error(
                StatusCode::FORBIDDEN,
                ErrorCode::NotAvailableInPlatformMode,
                NOT_IN_PLATFORM,
            ),
            Self::Closed => error(StatusCode::FORBIDDEN, ErrorCode::SignUpClosed, SIGN_UP_CLOSED),
            Self::SetupCodeInvalid => error(
                StatusCode::FORBIDDEN,
                ErrorCode::SetupCodeInvalid,
                SETUP_CODE_INVALID,
            ),
            Self::Unavailable => {
                error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE)
            }
        }
    }
}

/// The orchestrator's sign-up answer, asked with the visitor's setup code (if any). `Err`
/// in platform mode and whenever the gate can't answer: anything unclear refuses.
async fn gate(
    s: &AppState,
    setup_code: Option<&str>,
) -> Result<crate::app_users::internal::SignUpAnswer, Refusal> {
    if ModePolicy::new(s.config.mode).sign_up() == SignUpRule::Refused {
        return Err(Refusal::Platform);
    }
    let Some(gate) = &s.signup_gate else {
        warn!("no sign-up gate is configured");
        return Err(Refusal::Unavailable);
    };
    let org = s.config.org_id.get().map_err(|e| {
        not_ready(&e);
        Refusal::Unavailable
    })?;
    gate.signup_check(org, setup_code).await.map_err(|e| {
        warn!(error = %e, "the sign-up gate failed");
        Refusal::Unavailable
    })
}

/// `Ok` when sign-up may go on, with the claim nonce of a fresh install's code-gated sign-up;
/// `Err` when it is refused: in platform mode always, otherwise when the tenant's
/// orchestrator says no. A `first_member` answer without a nonce is unclear: unavailable.
async fn sign_up_gate(
    s: &AppState,
    setup_code: Option<&str>,
) -> Result<Option<crate::app_users::internal::ClaimNonce>, Refusal> {
    let a = gate(s, setup_code).await?;
    if a.allowed {
        if a.reason != "first_member" {
            return Ok(None);
        }
        return match a.claim_nonce {
            Some(nonce) => Ok(Some(nonce)),
            None => {
                warn!("the sign-up gate allowed a first member without a claim nonce");
                Err(Refusal::Unavailable)
            }
        };
    }
    Err(match a.reason.as_str() {
        "platform" => Refusal::Platform,
        "setup_code_invalid" => Refusal::SetupCodeInvalid,
        _ => Refusal::Closed,
    })
}

/// Binds the fresh-install claim to the account just created. `None` when bound, or when the
/// instance was claimed meanwhile (the account stays and waits as a newcomer). On any
/// failure the account is deleted, so a retry with the same email can succeed, and the
/// answer is `setup_code_invalid` (refused) or `unavailable`.
async fn bind_claim(
    s: &AppState,
    org: &str,
    user_id: &str,
    nonce: &crate::app_users::internal::ClaimNonce,
) -> Option<Refusal> {
    let failed = match &s.signup_gate {
        Some(gate) => match gate.signup_claimed(org, user_id, nonce).await {
            Ok(bound) => {
                info!(user_id, bound, "fresh-install claim");
                return None;
            }
            Err(e) => e,
        },
        None => crate::app_users::internal::InternalError::Unavailable("no sign-up gate".into()),
    };
    warn!(user_id, error = %failed, "binding the fresh-install claim failed; deleting the new account");
    // `sign_up_inner` checked the admin client before creating the account.
    if let Some(admin) = &s.admin
        && let Err(e) = admin.delete_user(user_id).await
    {
        warn!(user_id, error = %e, "deleting the unclaimed account failed");
    }
    Some(match failed {
        crate::app_users::internal::InternalError::Refused { .. } => Refusal::SetupCodeInvalid,
        crate::app_users::internal::InternalError::Unavailable(_) => Refusal::Unavailable,
    })
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

/// Zitadel's password-policy refusals on `POST /v2/users/{id}/password` (a reset, a change or an
/// admin set), by their captured ids: Zitadel localizes the message.
pub(crate) const SET_PASSWORD_POLICY: [refusals::Refusal; 5] = [
    refusals::PASSWORD_TOO_SHORT,
    refusals::PASSWORD_NO_UPPER,
    refusals::PASSWORD_NO_LOWER,
    refusals::PASSWORD_NO_NUMBER,
    refusals::PASSWORD_NO_SYMBOL,
];

/// The same checks on `POST /v2/users/new`, with that endpoint's ids.
pub(crate) const CREATE_PASSWORD_POLICY: [refusals::Refusal; 5] = [
    refusals::CREATE_USER_PASSWORD_TOO_SHORT,
    refusals::CREATE_USER_PASSWORD_NO_UPPER,
    refusals::CREATE_USER_PASSWORD_NO_LOWER,
    refusals::CREATE_USER_PASSWORD_NO_NUMBER,
    refusals::CREATE_USER_PASSWORD_NO_SYMBOL,
];

/// A password Zitadel can take: the policy's checks, and at most bcrypt's 72 bytes (counted in
/// bytes, so 39 characters can be too many). A longer one passes Zitadel's validation and then
/// fails with a 500 (`refusals::PASSWORD_MAX_BYTES`).
pub(crate) fn usable_password(password: &str) -> bool {
    password.len() <= refusals::PASSWORD_MAX_BYTES && strong_password(password)
}

/// Zitadel refused a new password on `POST /v2/users/{id}/password`: its policy, or its own
/// validation of the field.
fn set_password_refused(e: &ZError) -> bool {
    e.is_any(&SET_PASSWORD_POLICY) || e.is_invalid_request()
}

/// What the orchestrator stores about the user's latest member invite. Unconfigured or failing
/// is an error: callers fail closed.
async fn open_invite(s: &AppState, user: &str) -> Result<OpenInvite, ()> {
    let Some(orchestrator) = &s.signup_gate else {
        warn!("accept-invite needs the orchestrator's internal client");
        return Err(());
    };
    orchestrator.open_invite(user).await.map_err(|e| {
        warn!(error = %e, "accept-invite: the open-invite check failed");
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
    // Only failed attempts count against an address, so an office behind one NAT can accept many
    // invites an hour while guessing codes from it still stops at 20 failures. The attempt is
    // counted up front (concurrent requests can't all pass a check before any failure lands) and
    // handed back only by the outcomes that aren't a failure of the caller's.
    let Some(slot) = Slot::reserve(&s.limiter, format!("accept-invite:ip:{ip}"), 20, HOUR) else {
        return error(StatusCode::TOO_MANY_REQUESTS, ErrorCode::RateLimited, TOO_MANY);
    };
    let bad = || error(StatusCode::BAD_REQUEST, ErrorCode::InvalidRequest, BAD_INVITE);
    let gone = |code, message| error(StatusCode::GONE, code, message);
    let too_many = || error(StatusCode::TOO_MANY_REQUESTS, ErrorCode::RateLimited, TOO_MANY);
    let weak = || error(StatusCode::BAD_REQUEST, ErrorCode::WeakPassword, WEAK_PASSWORD);
    // Validate before any key is built from the input.
    let code = b.code.as_deref().map(str::trim);
    if !is_user_id(&b.user)
        || code.is_some() == b.accept_ticket.is_some()
        || code.is_some_and(|c| c.is_empty() || c.len() > 20)
    {
        return bad();
    }
    if b.auth_request.as_deref().is_some_and(|ar| !is_auth_request_id(ar)) {
        return gone(ErrorCode::SignInExpired, GONE);
    }
    // Before the code is spent.
    if !usable_password(&b.password) {
        slot.refund();
        return weak();
    }
    // Setting a password and marking the email verified need Zitadel's admin token; without it
    // the code would be spent for nothing.
    let Some(admin) = &s.admin else {
        warn!("accept-invite needs CHELABASE_ZITADEL_ADMIN_PAT_FILE");
        slot.refund();
        return error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE);
    };
    let Some(org) = org_id(s) else {
        slot.refund();
        return no_org();
    };
    // Only failed code attempts count against a user, so a typo'd password or a retry never
    // locks the invitee out.
    let user_key = format!("accept-invite:user:{}", b.user);
    if s.limiter.over(&user_key, 5, HOUR) {
        slot.refund();
        return too_many();
    }
    let user = match admin.get_user(&b.user).await {
        Ok(Some(u)) => u,
        Ok(None) => return gone(ErrorCode::InviteRevoked, INVITE_REVOKED),
        // A refusal is Zitadel's answer and counts; only an outage is free.
        Err(e @ ZError::Rejected { .. }) => return unavailable(&e),
        Err(e) => {
            slot.refund();
            return unavailable(&e);
        }
    };
    // An invite code for a user of another organization never signs into the platform.
    if user.org_id != org {
        warn!("accept-invite for a user outside the platform org");
        s.limiter.hit(&user_key, HOUR);
        return bad();
    }
    let mut password_set = false;
    match (code, &b.accept_ticket) {
        (Some(code), _) => {
            // Reserved before the check like the address's slot, so a hang-up can't skip it.
            let Some(user_slot) = Slot::reserve(&s.limiter, user_key, 5, HOUR) else {
                slot.refund();
                return too_many();
            };
            // A reset code is checked by setting the password with it: nothing changes on a
            // wrong one.
            let checked = if b.reset {
                admin.set_password(&b.user, code, &b.password).await
            } else {
                admin.verify_invite_code(&b.user, code).await
            };
            match checked {
                Ok(()) => {
                    user_slot.refund();
                    password_set = b.reset;
                }
                // An arm that hands an attempt back matches one exact captured refusal. A wrong
                // code is `INVITE_CODE_INVALID` or `PASSWORD_CODE_NOT_FOUND`, never one of them,
                // so guessing stays counted.
                Err(e) if !b.reset && e.is(refusals::INVITE_USER_NOT_FOUND) => {
                    info!("accept-invite: the user is gone");
                    user_slot.refund();
                    return gone(ErrorCode::InviteRevoked, INVITE_REVOKED);
                }
                // The policy refused the password: the code wasn't spent, so no ticket.
                Err(e) if b.reset && e.is_any(&SET_PASSWORD_POLICY) => {
                    user_slot.refund();
                    slot.refund();
                    return weak();
                }
                // Zitadel's validation of the field has no id to match exactly: it counts.
                Err(e) if b.reset && e.is_invalid_request() => return weak(),
                // A wrong, an expired and a missing code get the same answer from Zitadel; the
                // stored invite tells an expired one apart. Read only now, and counted either way.
                Err(e) if !b.reset && e.is(refusals::INVITE_CODE_INVALID) => {
                    return match open_invite(s, &b.user).await {
                        Ok(OpenInvite { open: false, expires_at: Some(_) }) => {
                            gone(ErrorCode::InviteExpired, INVITE_EXPIRED)
                        }
                        _ => bad(),
                    };
                }
                Err(ZError::Rejected { status: status @ (400 | 412), message, .. }) => {
                    warn!(status, %message, reset = b.reset, "accept-invite: the code was refused");
                    return bad();
                }
                // Any other refusal is Zitadel answering about the code, not an outage: it counts
                // against the address (the invitee's count stays as before).
                Err(e @ ZError::Rejected { .. }) => {
                    user_slot.refund();
                    return unavailable(&e);
                }
                Err(e) => {
                    user_slot.refund();
                    slot.refund();
                    return unavailable(&e);
                }
            }
        }
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
    // From here the code is spent, and no outcome counts against the address.
    let response = finish_accept(s, admin, b, user, password_set).await;
    slot.refund();
    response
}

/// The steps after a verified code: any failure hands back a ticket, and every step is safe to
/// repeat.
async fn finish_accept(
    s: &AppState,
    admin: &Zitadel,
    b: AcceptInviteBody,
    user: ZUser,
    password_set: bool,
) -> Response {
    // I1: a deactivated user (a removed member invited again) comes back only now, with a
    // verified code; the invite itself leaves them deactivated. #78: and only while Chelabase has
    // an open invite for them. Anyone can have a reset code mailed, a deactivated user too, so a
    // checked code alone never reactivates. A suspended member is locked, not inactive, and stays
    // locked: Zitadel refuses their session below.
    if user.state == UserState::Inactive {
        match open_invite(s, &b.user).await {
            Ok(OpenInvite { open: true, .. }) => {}
            Ok(_) => {
                info!("accept-invite: an inactive user with no open invite stays inactive");
                return error(StatusCode::BAD_REQUEST, ErrorCode::InvalidRequest, BAD_INVITE);
            }
            Err(()) => {
                return incomplete(s, &b.user, StatusCode::SERVICE_UNAVAILABLE, ErrorCode::InviteIncomplete, INVITE_INCOMPLETE);
            }
        }
        if let Err(e) = admin.reactivate_user_settled(&b.user).await {
            return incomplete_unavailable(s, &b.user, &e);
        }
    }
    if !password_set {
        match admin.set_password_admin(&b.user, &b.password).await {
            Ok(()) => {}
            Err(e) if set_password_refused(&e) => {
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
        // A suspended member: locked until reinstated.
        Err(e) if e.is_any(&[refusals::SESSION_USER_NOT_ACTIVE, refusals::SESSION_USER_LOCKED]) => {
            info!("accept-invite: the user is locked; no session");
            return incomplete(s, &b.user, StatusCode::SERVICE_UNAVAILABLE, ErrorCode::InviteIncomplete, INVITE_INCOMPLETE);
        }
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
        (status = 503, description = "The instance isn't set up yet (its org id isn't written): `unavailable`", body = ErrorEnvelope),
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
    // Answered the same whether or not the account exists, but never "sent" when the org
    // (and so the lookup) isn't there yet.
    if org_id(&s).is_none() {
        return no_org();
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
        // Every caller has the org already (it is cached once read).
        let Ok(org) = s2.config.org_id.get() else {
            warn!("password reset without the org id; nothing sent");
            return;
        };
        match s2.zitadel.find_user_by_email(org, &email).await {
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
    if !usable_password(&b.password) {
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
