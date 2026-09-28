//! The HTTP API Studio's sign-in pages call (through the gateway).

use crate::config::Config;
use crate::cookie::*;
use crate::guard::{RateLimiter, client_ip, email_tag, normalize_email, pad};
use crate::zitadel::{NewUser, Session, ZError, Zitadel};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use cheladb_routes::ui_auth as routes;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{info, warn};

pub mod messages {
    pub const BAD_CREDENTIALS: &str = "Email or password is incorrect";
    pub const BAD_CODE: &str = "Code is invalid or expired";
    pub const WEAK_PASSWORD: &str =
        "Use at least 8 characters, with upper- and lower-case letters, a number and a symbol";
    pub const GONE: &str = "This sign-in has expired, start again";
    pub const TOO_MANY: &str = "Too many attempts, try again later";
    pub const UNAVAILABLE: &str = "Sign-in is unavailable, try again";
    pub const ACCOUNT_EXISTS: &str = "An account with this email already exists";
}
use messages::*;

pub struct AppState {
    pub zitadel: Zitadel,
    pub config: Config,
    pub limiter: RateLimiter,
}

type S = State<Arc<AppState>>;

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route(routes::AUTH_REQUEST.path, post(auth_request))
        .route(routes::SIGN_IN.path, post(sign_in))
        .route(routes::SIGN_UP.path, post(sign_up))
        .route(routes::VERIFY_EMAIL.path, post(verify_email))
        .route(routes::RESEND_CODE.path, post(resend_code))
        .route(routes::PASSWORD_RESET.path, post(password_reset))
        .route(
            routes::PASSWORD_RESET_CONFIRM.path,
            post(password_reset_confirm),
        )
        .route(routes::LOGOUT.path, post(logout))
        // Every body here is a few short fields; this also bounds rate-limit keys.
        .layer(DefaultBodyLimit::max(8 * 1024))
        .with_state(state)
}

const MINUTE: Duration = Duration::from_secs(60);
/// Sign-up may wait up to a second for Zitadel to see the new user (`new_user_session`), so its
/// answers are padded this much beyond sign-in's, or a new email would answer slower than a taken one.
const SIGN_UP_EXTRA: Duration = Duration::from_secs(1);
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

/// Zitadel's default complexity policy, checked here first so the answer never depends on
/// whether an account exists.
fn strong_password(password: &str) -> bool {
    password.chars().count() >= 8
        && password.chars().any(|c| c.is_lowercase())
        && password.chars().any(|c| c.is_uppercase())
        && password.chars().any(|c| c.is_ascii_digit())
        && password.chars().any(|c| !c.is_alphanumeric())
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

fn error(status: StatusCode, message: &str) -> Response {
    reply(status, json!({ "error": message }), Vec::new())
}

fn unavailable(e: &ZError) -> Response {
    warn!(error = %e, "zitadel call failed");
    error(StatusCode::SERVICE_UNAVAILABLE, UNAVAILABLE)
}

/// A pending sign-in that Zitadel no longer knows is gone; anything else is unavailability.
fn pending_sign_in_error(e: ZError) -> Response {
    match e {
        ZError::Rejected { .. } => error(StatusCode::GONE, GONE),
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
        Ok(redirect) => {
            let kept = SessionState {
                session_id: session.id,
                session_token: session.token,
                exp: now_secs() + SESSION_TTL_SECS,
            };
            let secure = s.config.secure_cookies;
            reply(
                StatusCode::OK,
                json!({"next": "done", "redirect": redirect}),
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
        Err(e) => pending_sign_in_error(e),
    }
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
        json!({"next": "verify"}),
        vec![login_cookie(s, &state)],
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthRequestBody {
    auth_request: String,
}

async fn auth_request(State(s): S, headers: HeaderMap, Json(b): Json<AuthRequestBody>) -> Response {
    let ip = client_ip(&headers);
    if !allowed(&s, &[(format!("auth-request:ip:{ip}"), 60, MINUTE)]) {
        return error(StatusCode::TOO_MANY_REQUESTS, TOO_MANY);
    }
    if !is_auth_request_id(&b.auth_request) {
        return error(StatusCode::GONE, GONE);
    }
    match s.zitadel.auth_request(&b.auth_request).await {
        Ok(sign_up) => reply(StatusCode::OK, json!({"signUp": sign_up}), Vec::new()),
        Err(e) => pending_sign_in_error(e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignInBody {
    auth_request: String,
    email: String,
    password: String,
}

async fn sign_in(State(s): S, headers: HeaderMap, Json(b): Json<SignInBody>) -> Response {
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
        return error(StatusCode::TOO_MANY_REQUESTS, TOO_MANY);
    }
    let refused = || {
        s.limiter.hit(&email_key, MINUTE);
        error(StatusCode::UNAUTHORIZED, BAD_CREDENTIALS)
    };
    if !is_auth_request_id(&b.auth_request) {
        return error(StatusCode::GONE, GONE);
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SignUpBody {
    auth_request: String,
    email: String,
    password: String,
    given_name: String,
    family_name: String,
}

async fn sign_up(State(s): S, headers: HeaderMap, Json(b): Json<SignUpBody>) -> Response {
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
        return error(StatusCode::TOO_MANY_REQUESTS, TOO_MANY);
    }
    if !is_auth_request_id(&b.auth_request) {
        return error(StatusCode::GONE, GONE);
    }
    if !strong_password(&b.password) {
        return error(StatusCode::BAD_REQUEST, WEAK_PASSWORD);
    }
    if let Err(e) = s.zitadel.auth_request(&b.auth_request).await {
        return pending_sign_in_error(e);
    }
    // Zitadel checks usernames, not emails, for uniqueness: never make a second account
    // for an email that already has one.
    match s.zitadel.find_user_by_email(&s.config.org_id, &email).await {
        Ok(None) => {}
        Ok(Some(_)) => return taken(s, &email, b.auth_request),
        Err(e) => return unavailable(&e),
    }
    let template = verify_template(s);
    let new = NewUser {
        org_id: &s.config.org_id,
        email: &email,
        given_name: b.given_name.trim(),
        family_name: b.family_name.trim(),
        password: &b.password,
        verified: s.config.skip_email_verification,
        url_template: &template,
    };
    let user_id = match s.zitadel.create_user(&new).await {
        Ok(id) => id,
        Err(ZError::Rejected { status: 409, .. }) => return taken(s, &email, b.auth_request),
        Err(ZError::Rejected { status: 400, .. }) => {
            return error(StatusCode::BAD_REQUEST, WEAK_PASSWORD);
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

/// A sign-up for a taken email. With verification on (platform), it's answered exactly like a
/// new one, with a cookie that can't verify. With it off (self-hosted), a new sign-up has no
/// code step to hide behind, so the user is told and Studio sends them to sign in.
fn taken(s: &Arc<AppState>, email: &str, auth_request: String) -> Response {
    if s.config.skip_email_verification {
        info!(email = %email_tag(email), "sign-up for a taken email");
        return error(StatusCode::CONFLICT, ACCOUNT_EXISTS);
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
        json!({"next": "verify"}),
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

#[derive(Deserialize)]
struct CodeBody {
    code: String,
}

async fn verify_email(State(s): S, headers: HeaderMap, Json(b): Json<CodeBody>) -> Response {
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
        return error(StatusCode::TOO_MANY_REQUESTS, TOO_MANY);
    }
    // No (or an expired) step cookie: the sign-in is over, whatever the code.
    let Some(state) = state else {
        return error(StatusCode::GONE, GONE);
    };
    let (Some(user_id), Some(session_id), Some(session_token), false) = (
        state.user_id,
        state.session_id,
        state.session_token,
        state.decoy,
    ) else {
        return error(StatusCode::BAD_REQUEST, BAD_CODE);
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
        Err(ZError::Rejected { .. }) => error(StatusCode::BAD_REQUEST, BAD_CODE),
        Err(e) => unavailable(&e),
    }
}

#[derive(Deserialize)]
struct Empty {}

async fn resend_code(State(s): S, headers: HeaderMap, Json(_): Json<Empty>) -> Response {
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
        return error(StatusCode::TOO_MANY_REQUESTS, TOO_MANY);
    }
    if let Some(user) = state.and_then(|st| st.user_id)
        && let Err(e) = s.zitadel.resend_code(&user, &verify_template(s)).await
    {
        warn!(error = %e, "resending the verification code failed");
    }
    reply(StatusCode::OK, json!({}), Vec::new())
}

#[derive(Deserialize)]
struct EmailBody {
    email: String,
}

async fn password_reset(State(s): S, headers: HeaderMap, Json(b): Json<EmailBody>) -> Response {
    let email = normalize_email(&b.email);
    let ip = client_ip(&headers);
    if !allowed(
        &s,
        &[
            (format!("reset:ip:{ip}"), 20, HOUR),
            (format!("reset:email:{}", email_tag(&email)), 5, HOUR),
        ],
    ) {
        return error(StatusCode::TOO_MANY_REQUESTS, TOO_MANY);
    }
    spawn_reset_email(&s, email);
    reply(StatusCode::OK, json!({}), Vec::new())
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ConfirmBody {
    user_id: String,
    code: String,
    password: String,
}

async fn password_reset_confirm(
    State(s): S,
    headers: HeaderMap,
    Json(b): Json<ConfirmBody>,
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
        return error(StatusCode::TOO_MANY_REQUESTS, TOO_MANY);
    }
    // Checked before Zitadel sees the user id, so this answer can't tell ids apart.
    if !strong_password(&b.password) {
        return error(StatusCode::BAD_REQUEST, WEAK_PASSWORD);
    }
    if !is_user_id(user_id) {
        return error(StatusCode::BAD_REQUEST, BAD_CODE);
    }
    match s
        .zitadel
        .set_password(user_id, b.code.trim(), &b.password)
        .await
    {
        Ok(()) => reply(StatusCode::OK, json!({}), Vec::new()),
        Err(ZError::Rejected { .. }) => error(StatusCode::BAD_REQUEST, BAD_CODE),
        Err(e) => unavailable(&e),
    }
}

async fn logout(State(s): S, headers: HeaderMap, Json(_): Json<Empty>) -> Response {
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
        json!({}),
        vec![
            clear_cookie(LOGIN_COOKIE, secure),
            clear_cookie(SESSION_COOKIE, secure),
        ],
    )
}
