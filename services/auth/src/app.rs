//! The HTTP API Studio's sign-in pages call (through the gateway).

use crate::config::Config;
use crate::cookie::*;
use crate::guard::{RateLimiter, Slot, UsedTickets, client_ip, email_tag, pad};
use chelabase_zitadel::refusals;
use chelabase_identity::{
    CodeKind, CodeRequest, Delivery, Identity, IdentityError, Missing, NewUser, Pool, SignedIn,
};
use chelabase_identity::rules::MEMBER_REMOVED_BAN_REASON;
use chelabase_zitadel::rules::{normalize_email, strong_password};
use crate::app_users::internal::OpenInvite;
use crate::session::{ABSOLUTE_LIFETIME, Choice, choose, may_issue_code};
use crate::zitadel::Zitadel;
use axum::extract::{DefaultBodyLimit, Query, State};
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
use utoipa::{IntoParams, ToSchema};
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
    pub const BAD_NAME: &str = "Enter a first and a last name, up to 200 characters each";
    pub const BAD_DETAILS: &str = "Check the details you entered";
    pub const SIGN_IN_AGAIN: &str = "Your session has ended; sign in again";
    pub const NOT_FROM_STUDIO: &str = "This request must come from Studio";
    pub const CHOOSE_TENANT: &str = "Choose a workspace to sign in to";
    pub const MEMBER_SUSPENDED: &str = "Your access is suspended; ask an administrator to reinstate you";
}
use messages::*;

pub struct AppState {
    /// No member flow calls Zitadel since #124; m13 removes this with Zitadel.
    pub zitadel: Zitadel,
    pub config: Config,
    pub limiter: RateLimiter,
    /// Accept tickets already spent.
    pub used_tickets: UsedTickets,
    /// App users' flows; `None` (their routes answer 503) without the app keys.
    pub app: Option<crate::app_users::AppUsers>,
    /// Asks the orchestrator whether sign-up is open. `None` fails closed (sign-up answers 503).
    pub signup_gate: Option<crate::app_users::InternalClient>,
    /// Zitadel with the admin token. Unused since #124 (accept-invite runs on the identity
    /// service); m13 removes it with Zitadel.
    pub admin: Option<Zitadel>,
    /// The member-token signer (`CHELABASE_MEMBER_JWT_*`): `/auth/v1/token` mints with it.
    pub member_tokens: crate::member_token::MemberTokens,
    /// The identity service (`CHELABASE_IDENTITY_URL`, `CHELABASE_IDENTITY_TOKEN_FILE`): members
    /// sign in, up and out against it.
    pub identity: Identity,
    /// What `/auth/v1/token` measures a session's age with.
    pub clock: crate::session::Clock,
}

type S = State<Arc<AppState>>;

/// The documented routes; `openapi()` reads the document from here.
pub(crate) fn api_router() -> OpenApiRouter<Arc<AppState>> {
    platform_router().merge(crate::app_users::flows::api_router())
}

/// Studio's sign-in pages' routes.
fn platform_router() -> OpenApiRouter<Arc<AppState>> {
    OpenApiRouter::new()
        .routes(routes!(instance))
        .routes(routes!(studio_config))
        .routes(routes!(sign_in))
        .routes(routes!(sign_up))
        .routes(routes!(token))
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

/// Where a sign-in or sign-up goes next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
enum SignInStep {
    /// Signed in, with the session cookie set: `POST /auth/v1/token` works.
    Studio,
    /// The email isn't verified: show the code step (the step cookie is set).
    Verify,
    /// The credentials are right, but an admin hasn't approved this person yet. No session.
    Pending,
}

/// A sign-in's or sign-up's answer.
#[derive(Serialize, ToSchema)]
struct SignInAnswer {
    next: SignInStep,
}

/// An empty JSON object.
#[derive(Serialize, ToSchema)]
struct EmptyResponse {}

/// Studio's runtime config: the public values it starts with. Nothing else is here (no PAT, key
/// or secret): it is public and cached.
#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct StudioConfig {
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
    description = "Studio's runtime config: the API URL and Studio's host. Public; cacheable for 60 s.",
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
    let (Ok(_), Ok(_), Ok(_)) = values else {
        for e in [values.0.err(), values.1.err(), values.2.err()].into_iter().flatten() {
            not_ready(&e);
        }
        return no_store(error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE));
    };
    let body = StudioConfig {
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
    /// Whether the sign-in page offers "Create account": sign-up is open, or the unclaimed
    /// instance waits for its first account. `false` in platform mode.
    sign_up_allowed: bool,
}

const MINUTE: Duration = Duration::from_secs(60);
/// Sign-up and accept-invite do more than sign-in (the user, its row, its session), so their
/// answers are padded this much beyond sign-in's, or a new email would answer slower than a
/// taken one.
pub(crate) const SIGN_UP_EXTRA: Duration = Duration::from_secs(1);
const HOUR: Duration = Duration::from_secs(3600);

/// Counts one attempt against each key in order, stopping at the first that is over its
/// limit (so an IP over its limit never spends or creates the keys after it).
fn allowed(s: &AppState, checks: &[(String, u32, Duration)]) -> bool {
    checks
        .iter()
        .all(|(key, limit, window)| s.limiter.allow(key, *limit, *window))
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

fn login_cookie(s: &AppState, state: &LoginState) -> String {
    set_cookie(
        LOGIN_COOKIE,
        &seal_login(&s.config.cookie_key, state),
        LOGIN_TTL_SECS,
        s.config.secure_cookies,
    )
}

/// The code step: the identity session waits in the step cookie, and no session cookie is set.
/// The identity session is kept, sealed in the 10-minute step cookie: a verified code finishes
/// the sign-in with it, since nothing at that step can make a session without the password.
/// An abandoned step leaves it to expire on its own.
fn verify_step(s: &AppState, signed: SignedIn) -> Response {
    let state = LoginState {
        auth_request: String::new(),
        user_id: Some(signed.user.id),
        session_id: Some(signed.session_id),
        session_token: Some(signed.session_token),
        step: Step::Verify,
        decoy: false,
        exp: now_secs() + LOGIN_TTL_SECS,
    };
    reply(
        StatusCode::OK,
        json!(SignInAnswer { next: SignInStep::Verify }),
        vec![login_cookie(s, &state)],
    )
}

#[utoipa::path(
    get,
    path = "/auth/v1/instance",
    operation_id = "getInstance",
    tag = "login",
    description = "Whether this instance is unclaimed, so the sign-up page asks for the setup code, and whether it takes sign-ups, so the sign-in page offers one. Sets no cookie.",
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
        let closed = InstanceInfo { setup_required: false, sign_up_allowed: false };
        return reply(StatusCode::OK, json!(closed), Vec::new());
    }
    match gate(&s, None).await {
        Ok(a) => reply(
            StatusCode::OK,
            json!(InstanceInfo {
                setup_required: a.setup_required,
                sign_up_allowed: a.allowed || a.setup_required,
            }),
            Vec::new(),
        ),
        Err(refused) => refused.response(),
    }
}

#[derive(Deserialize, ToSchema)]
struct SignInBody {
    email: String,
    password: String,
}

/// The tenant asked for.
#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
struct TenantQuery {
    /// The tenant's id. Needed to sign in to one of several; `/auth/v1/token` refuses any
    /// but the session's. Never trusted: it is checked against the memberships.
    tenant: Option<String>,
}

#[utoipa::path(
    post,
    path = "/auth/v1/sign-in",
    operation_id = "signIn",
    tag = "login",
    description = "Signs in with email and password. `studio` sets the HttpOnly session cookie, which seals the identity session and the tenant. `verify` sets the HttpOnly step cookie that the code step reads. `pending` sets no cookie: an admin hasn't approved this person yet. A wrong password, an unknown email and no active membership answer alike.",
    params(TenantQuery),
    request_body = SignInBody,
    responses(
        (status = 200, description = "`studio`, `verify` or `pending`", body = SignInAnswer),
        (status = 400, description = "The body is malformed: `invalid_request`", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON (wrong content type): `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "The body is JSON but has the wrong fields: `invalid_request`", body = ErrorEnvelope),
        (status = 413, description = "The body is over 8 KiB: `too_large`", body = ErrorEnvelope),
        (status = 401, description = "Wrong email or password, no active membership, or a `tenant` this person isn't a member of: `invalid_credentials`", body = ErrorEnvelope),
        (status = 409, description = "The credentials are right and this person has several tenants: `tenant_required`, with their ids in `error.tenants`. Sign in again with `?tenant=`", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "The identity service or the orchestrator is unavailable: `unavailable`", body = ErrorEnvelope),
    ),
)]
async fn sign_in(
    State(s): S,
    Query(q): Query<TenantQuery>,
    headers: HeaderMap,
    ApiJson(b): ApiJson<SignInBody>,
) -> Response {
    let start = Instant::now();
    let answer = sign_in_inner(&s, q, &headers, b).await;
    pad(start, s.config.min_response).await;
    answer
}

async fn sign_in_inner(s: &AppState, q: TenantQuery, headers: &HeaderMap, b: SignInBody) -> Response {
    let email = normalize_email(&b.email);
    let ip = client_ip(headers);
    if !allowed(s, &[(format!("sign-in:ip:{ip}"), 10, MINUTE)]) {
        return error(StatusCode::TOO_MANY_REQUESTS, ErrorCode::RateLimited, TOO_MANY);
    }
    // Per email, only refused sign-ins count: signing in as someone takes their password, so
    // nobody can use up a known account's attempts. The attempt is reserved up front, so
    // concurrent requests can't all pass a check before any failure lands, and a client that
    // hangs up during the check still spends it. Only these outcomes give it back:
    //
    // | Outcome                                                        | Answer  | Email slot |
    // |----------------------------------------------------------------|---------|------------|
    // | the email's 5 a minute are used                                | 429     | none held  |
    // | wrong password, unknown email, banned, input identity refused  | 401     | counts     |
    // | right password, but no tenant to land in                       | 401     | counts     |
    // | identity or the orchestrator can't answer                      | 503     | refunded   |
    // | right password, any other outcome                              | 200/409 | refunded   |
    // | the request is dropped undecided (the client hung up)          | none    | counts     |
    //
    // "No tenant" counts so that it can't be told from a wrong password by when the 429 comes.
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
    let signed = match s.identity.sign_in(&Pool::Members, &email, &b.password).await {
        Ok(signed) => signed,
        Err(
            IdentityError::BadCredentials
            | IdentityError::Banned
            | IdentityError::SessionRevoked
            | IdentityError::Invalid { .. },
        ) => {
            info!(email = %email_tag(&email), "sign-in refused");
            return refused();
        }
        Err(e) => {
            slot.refund();
            return identity_unavailable(&e);
        }
    };
    match conclude(s, signed, q.tenant.as_deref()).await {
        Concluded::Answer(answer) => {
            slot.refund();
            answer
        }
        Concluded::Refused => {
            info!(email = %email_tag(&email), "sign-in refused: no tenant");
            refused()
        }
    }
}

fn identity_unavailable(e: &IdentityError) -> Response {
    warn!(error = %e, "identity call failed");
    error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE)
}

/// What a right password led to.
enum Concluded {
    Answer(Response),
    /// No tenant to land in: the caller gives its generic refusal. The session is gone.
    Refused,
}

/// A session nobody will use.
async fn drop_session(s: &AppState, session_id: &str) {
    if let Err(e) = s.identity.revoke_session(&Pool::Members, session_id).await {
        warn!(error = %e, "revoking an unused session failed; it expires on its own");
    }
}

/// From a new identity session to the sign-in's answer. Only `studio` and `verify` keep the
/// session; `pending` and `tenant_required` revoke it (a retry signs in again).
async fn conclude(s: &AppState, signed: SignedIn, wanted: Option<&str>) -> Concluded {
    if !signed.user.email_verified {
        send_code(s, &signed.user.id, CodeKind::Verify).await;
        return Concluded::Answer(verify_step(s, signed));
    }
    let Some(orchestrator) = &s.signup_gate else {
        warn!("sign-in needs the orchestrator's internal client");
        drop_session(s, &signed.session_id).await;
        return Concluded::Answer(error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE));
    };
    let tenants = match orchestrator.member_tenants(&signed.user.id).await {
        Ok(tenants) => tenants,
        Err(e) => {
            warn!(error = %e, "sign-in: reading the memberships failed");
            drop_session(s, &signed.session_id).await;
            return Concluded::Answer(error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE));
        }
    };
    let tenant = match choose(&tenants, wanted) {
        Choice::Tenant(tenant) => tenant,
        Choice::Pending => {
            drop_session(s, &signed.session_id).await;
            return Concluded::Answer(reply(
                StatusCode::OK,
                json!(SignInAnswer { next: SignInStep::Pending }),
                Vec::new(),
            ));
        }
        Choice::Several(list) => {
            drop_session(s, &signed.session_id).await;
            return Concluded::Answer(
                ApiError::new(StatusCode::CONFLICT.as_u16(), ErrorCode::TenantRequired, CHOOSE_TENANT)
                    .with_extra("tenants", json!(list))
                    .into_response(),
            );
        }
        Choice::Nowhere => {
            drop_session(s, &signed.session_id).await;
            return Concluded::Refused;
        }
    };
    let kept = SessionState::Identity {
        session_id: signed.session_id,
        session_token: signed.session_token,
        tenant,
        exp: now_secs() + SESSION_TTL_SECS,
    };
    let secure = s.config.secure_cookies;
    Concluded::Answer(reply(
        StatusCode::OK,
        json!(SignInAnswer { next: SignInStep::Studio }),
        vec![
            set_cookie(
                SESSION_COOKIE,
                &seal_session(&s.config.cookie_key, &kept),
                SESSION_TTL_SECS,
                secure,
            ),
            clear_cookie(LOGIN_COOKIE, secure),
        ],
    ))
}

/// Mails a code, within [`may_issue_code`] (5 an hour per user and kind, whichever flow asks).
/// The answer never says whether one went out.
async fn send_code(s: &AppState, user_id: &str, kind: CodeKind) {
    if !may_issue_code(&s.limiter, kind, user_id) {
        info!(kind = kind.as_str(), "a user's code limit is used up; nothing sent");
        return;
    }
    // The page where the code is typed: never the code or an id in the link.
    let page = match kind {
        CodeKind::Reset => "password-reset",
        _ => "verify",
    };
    let link = format!("{}/{page}", s.config.studio_url);
    let mut request = CodeRequest::new(user_id, kind, Delivery::Email);
    request.link = Some(&link);
    if let Err(e) = s.identity.create_code(&Pool::Members, &request).await {
        warn!(error = %e, "sending a code failed");
    }
}

/// The answer to a call to `/auth/v1/token` that isn't Studio's: Studio sends both, and a form
/// or another site can't. Neither says anything about the session.
fn from_studio(s: &AppState, headers: &HeaderMap) -> bool {
    let header_is = |name: &str, want: &str| headers.get(name).is_some_and(|v| v.as_bytes() == want.as_bytes());
    header_is("x-chelabase-studio", "1") && header_is("origin", &s.config.studio_url)
}

/// A member's short-lived token.
#[derive(Serialize, ToSchema)]
struct MemberToken {
    access_token: String,
    /// Seconds until it expires.
    expires_in: u64,
}

#[utoipa::path(
    post,
    path = "/auth/v1/token",
    operation_id = "mintMemberToken",
    tag = "login",
    description = "Mints a member access token from the HttpOnly session cookie, after checking that the identity session is live and younger than 30 days and that the membership is active. Takes no body. Studio must send `Origin` and `X-Chelabase-Studio: 1`. Never cached.",
    params(TenantQuery),
    responses(
        (status = 200, description = "The token, for the tenant the session is bound to", body = MemberToken),
        (status = 401, description = "No live session, an older one, another tenant, or no active membership there: `unauthorized`", body = ErrorEnvelope),
        (status = 403, description = "Not from Studio: `forbidden`", body = ErrorEnvelope),
        (status = 429, description = "Over 120 a minute for this session: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "The identity service or the orchestrator is unavailable: `unavailable`", body = ErrorEnvelope),
    ),
)]
async fn token(State(s): S, Query(q): Query<TenantQuery>, headers: HeaderMap) -> Response {
    let mut answer = token_inner(&s, q, &headers).await;
    answer
        .headers_mut()
        .insert(header::CACHE_CONTROL, header::HeaderValue::from_static("no-store"));
    answer
}

async fn token_inner(s: &AppState, q: TenantQuery, headers: &HeaderMap) -> Response {
    if !from_studio(s, headers) {
        return error(StatusCode::FORBIDDEN, ErrorCode::Forbidden, NOT_FROM_STUDIO);
    }
    let refused = || error(StatusCode::UNAUTHORIZED, ErrorCode::Unauthorized, SIGN_IN_AGAIN);
    let down = || error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE);
    let Some(SessionState::Identity { session_id, tenant, .. }) = read_cookie(headers, SESSION_COOKIE)
        .and_then(|raw| open_session(&s.config.cookie_key, raw, now_secs()))
    else {
        return refused();
    };
    if !allowed(s, &[(format!("token:session:{session_id}"), 120, MINUTE)]) {
        return error(StatusCode::TOO_MANY_REQUESTS, ErrorCode::RateLimited, TOO_MANY);
    }
    // The session is sealed to its tenant: asking for another never moves it.
    if q.tenant.as_deref().is_some_and(|t| t.parse() != Ok(tenant)) {
        return refused();
    }
    // By id, never by token: identity's lookup by token slides the expiry.
    let live = match s.identity.session_by_id(&Pool::Members, &session_id).await {
        Ok(live) => live,
        Err(IdentityError::NotFound(Missing::Session)) => return refused(),
        Err(e) => {
            warn!(error = %e, "identity call failed");
            return down();
        }
    };
    if s.clock.now() - live.session.created_at > ABSOLUTE_LIFETIME {
        drop_session(s, &session_id).await;
        return refused();
    }
    let Some(orchestrator) = &s.signup_gate else {
        warn!("/auth/v1/token needs the orchestrator's internal client");
        return down();
    };
    match orchestrator.member_tenants(&live.user.id).await {
        Ok(tenants) if tenants.active.contains(&tenant) => {}
        Ok(_) => return refused(),
        Err(e) => {
            warn!(error = %e, "/auth/v1/token: reading the memberships failed");
            return down();
        }
    }
    let minted = s.member_tokens.mint(&crate::member_token::MemberSubject {
        user_id: &live.user.id,
        tenant,
        session_id: &session_id,
        email: &live.user.email,
        name: &live.user.name,
    });
    match minted {
        Ok((access_token, _)) => reply(
            StatusCode::OK,
            json!(MemberToken { access_token, expires_in: s.member_tokens.ttl().as_secs() }),
            Vec::new(),
        ),
        Err(e) => {
            warn!(error = %e, "minting a member token failed");
            down()
        }
    }
}

#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct SignUpBody {
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
    description = "Creates an account: the fresh-install admin with the setup code, otherwise a person waiting for an admin's approval. With email verification off it signs them in, so the answer is `studio` (the session cookie) or `pending` (no cookie). With it on, `verify` and the HttpOnly step cookie that the code step reads.",
    request_body = SignUpBody,
    responses(
        (status = 200, description = "`studio`, `verify` or `pending`", body = SignInAnswer),
        (status = 400, description = "The body is malformed, a name is blank or over 200 characters, or the identity service refuses a field (`invalid_request`), or the password is too weak (`weak_password`)", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON (wrong content type): `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "The body is JSON but has the wrong fields: `invalid_request`", body = ErrorEnvelope),
        (status = 413, description = "The body is over 8 KiB: `too_large`", body = ErrorEnvelope),
        (status = 403, description = "Sign-up is closed for this instance (`sign_up_closed`), the unclaimed instance needs its setup code and it is missing or wrong (`setup_code_invalid`; no account is created), or not available in platform mode (`not_available_in_platform_mode`)", body = ErrorEnvelope),
        (status = 409, description = "An account with this email exists (`email_taken`); only when email verification is off", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "The identity service or the sign-up gate is unavailable: `unavailable`", body = ErrorEnvelope),
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
    if !usable_password(&b.password) {
        return error(StatusCode::BAD_REQUEST, ErrorCode::WeakPassword, WEAK_PASSWORD);
    }
    // A name is 1–200 runes after trimming, checked here so a bad name is never answered as a
    // password problem.
    let usable_name = |n: &str| (1..=200).contains(&n.trim().chars().count());
    if !usable_name(&b.given_name) || !usable_name(&b.family_name) {
        return error(StatusCode::BAD_REQUEST, ErrorCode::InvalidRequest, BAD_NAME);
    }
    let Some(org) = org_id(s) else {
        return no_org();
    };
    let nonce = match sign_up_gate(s, setup_code).await {
        Ok(nonce) => nonce,
        Err(refused) => return refused.response(),
    };
    let name = format!("{} {}", b.given_name.trim(), b.family_name.trim());
    let new = NewUser {
        email: &email,
        password: Some(&b.password),
        name: Some(&name),
        email_verified: s.config.skip_email_verification,
    };
    let user = match s.identity.create_user(&Pool::Members, &new).await {
        Ok(user) => user,
        Err(IdentityError::Conflict(_)) => {
            if let Some(nonce) = &nonce
                && let Some(healed) = heal_claim(s, org, &email, &b.password, nonce).await
            {
                return healed;
            }
            return taken(s, &email);
        }
        Err(IdentityError::Invalid { code, .. }) if code == "weak_password" => {
            return error(StatusCode::BAD_REQUEST, ErrorCode::WeakPassword, WEAK_PASSWORD);
        }
        Err(IdentityError::Invalid { .. }) => {
            return error(StatusCode::BAD_REQUEST, ErrorCode::InvalidRequest, BAD_DETAILS);
        }
        Err(e) => return identity_unavailable(&e),
    };
    if let Some(refused) = enroll(s, org, &user.id, nonce.as_ref()).await {
        return refused.response();
    }
    let signed = match s.identity.sign_in(&Pool::Members, &email, &b.password).await {
        Ok(signed) => signed,
        Err(e) => return identity_unavailable(&e),
    };
    signed_up(s, signed).await
}

/// The answer to a sign-up whose account is enrolled, from its first session.
async fn signed_up(s: &AppState, signed: SignedIn) -> Response {
    match conclude(s, signed, None).await {
        Concluded::Answer(answer) => answer,
        // Enrolled a moment ago, and already out of the tenant.
        Concluded::Refused => error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE),
    }
}

/// A code-gated sign-up for an email that has an account: the claimant whose earlier attempt
/// left the account behind (#121 audit S2: the bind or its row failed, and so did deleting
/// the account). With that account's password, the claim is asked again for it, which writes
/// the row that is missing, so the install isn't left without an admin. `None` for a wrong
/// password (the caller answers as for any taken email); on a failed claim the account stays,
/// because this sign-up didn't make it.
async fn heal_claim(
    s: &AppState,
    org: &str,
    email: &str,
    password: &str,
    nonce: &crate::app_users::internal::ClaimNonce,
) -> Option<Response> {
    let signed = match s.identity.sign_in(&Pool::Members, email, password).await {
        Ok(signed) => signed,
        Err(
            IdentityError::BadCredentials
            | IdentityError::Banned
            | IdentityError::SessionRevoked
            | IdentityError::Invalid { .. },
        ) => return None,
        Err(e) => return Some(identity_unavailable(&e)),
    };
    let claimed = match &s.signup_gate {
        Some(gate) => gate.signup_claimed(org, &signed.user.id, nonce).await,
        None => Err(crate::app_users::internal::InternalError::Unavailable("no sign-up gate".into())),
    };
    match claimed {
        Ok(bound) => info!(user_id = %signed.user.id, bound, "fresh-install claim, asked again for an existing account"),
        Err(failed) => {
            warn!(user_id = %signed.user.id, error = %failed, "asking the claim again failed");
            drop_session(s, &signed.session_id).await;
            return Some(refusal(failed).response());
        }
    }
    Some(signed_up(s, signed).await)
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

/// Gives the account just created its place: the fresh-install claim with a nonce, otherwise
/// a `pending` row (a member token never makes one). `None` when that's done, the loser of the
/// claim race included (they wait as pending). On any failure the account is deleted, so a
/// retry with the same email can succeed, and the answer is why.
///
/// With email verification on, an open sign-up gets no row yet (#121 audit S1): a `pending`
/// row for an address nobody proved would squat that email in the admins' request list.
/// verify-email joins it once the code has marked the email verified ([`join_verified`]).
async fn enroll(
    s: &AppState,
    org: &str,
    user_id: &str,
    nonce: Option<&crate::app_users::internal::ClaimNonce>,
) -> Option<Refusal> {
    use crate::app_users::internal::InternalError;
    let done = match (&s.signup_gate, nonce) {
        (Some(gate), Some(nonce)) => gate.signup_claimed(org, user_id, nonce).await.map(|bound| {
            info!(user_id, bound, "fresh-install claim");
        }),
        // The join waits for the verified email: see above.
        (Some(_), None) if !s.config.skip_email_verification => {
            info!(user_id, "open sign-up: the row waits for the verified email");
            Ok(())
        }
        (Some(gate), None) => gate.signup_joined(org, user_id).await.and_then(|joined| {
            if joined {
                Ok(())
            } else {
                Err(InternalError::Refused { status: 403, code: "sign_up_closed".into() })
            }
        }),
        (None, _) => Err(InternalError::Unavailable("no sign-up gate".into())),
    };
    let Err(failed) = done else {
        return None;
    };
    warn!(user_id, error = %failed, "enrolling the new account failed; deleting it");
    if let Err(e) = s.identity.delete_user(&Pool::Members, user_id).await {
        warn!(user_id, error = %e, "deleting the unenrolled account failed");
    }
    Some(refusal(failed))
}

/// Why the orchestrator refused to enroll an account.
fn refusal(failed: crate::app_users::internal::InternalError) -> Refusal {
    use crate::app_users::internal::InternalError;
    match failed {
        InternalError::Refused { code, .. } if code == "sign_up_closed" => Refusal::Closed,
        InternalError::Refused { .. } => Refusal::SetupCodeInvalid,
        InternalError::Unavailable(_) => Refusal::Unavailable,
    }
}

/// Both fields default to empty, so a missing one is a `not_found` like a wrong one.
#[derive(Deserialize, ToSchema)]
struct DescribeInviteBody {
    /// The identity user id from the invite link.
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

/// An identity user id: a hyphenated lower-case UUID, as every id of the service's is. Ids go into
/// identity's URL paths and limit keys, so only this exact shape is passed on.
fn is_identity_id(id: &str) -> bool {
    uuid::Uuid::parse_str(id).is_ok_and(|u| u.hyphenated().to_string() == id)
}

/// An invite or reset code: six digits.
fn is_code(code: &str) -> bool {
    code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit())
}

#[utoipa::path(
    post,
    path = "/auth/v1/invite",
    operation_id = "describeInvite",
    tag = "login",
    description = "Describes an open invite for the accept page: who invited the visitor, and to which workspace. The invite link's own token (`t`) is the credential, so this spends no code. It is a POST so that the user id and the token are never in a URL. A closed (accepted, revoked or expired) invite, a wrong or missing user or token, and a malformed value all answer the same `not_found`.",
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
    if !is_identity_id(&q.user) || !is_invite_token(&q.t) {
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

/// One of three shapes: `user` + `code` (a new person, or an account nobody proved: the code sets
/// the first password), `user` + `acceptTicket` (a retry after the code was spent), or `email`
/// (an existing account, from the code-less notice: `password` is its own). `t` is always the
/// link's token.
#[derive(Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
struct AcceptInviteBody {
    /// The invite link's token (`t`).
    t: String,
    /// The invitee's identity user id, from a code link (`?user=`).
    #[serde(default)]
    user: Option<String>,
    /// The six-digit invite code from the invite email, or from the admin's link.
    #[serde(default)]
    code: Option<String>,
    /// From an `invite_incomplete` error: finishes the steps after the (spent) code. Send it in
    /// place of `code`, with the same password.
    #[serde(default)]
    accept_ticket: Option<String>,
    /// An existing account's email (the notice's link carries only `t`).
    #[serde(default)]
    email: Option<String>,
    /// The new password with `code` or `acceptTicket`; the account's own with `email`.
    password: String,
}

/// What proves the visitor may accept.
enum Proof<'a> {
    Code { user: &'a str, code: &'a str },
    Ticket { user: &'a str, ticket: &'a str },
    Account { email: String },
}

fn proof_of(b: &AcceptInviteBody) -> Option<Proof<'_>> {
    if !is_invite_token(&b.t) {
        return None;
    }
    let user = b.user.as_deref().filter(|u| is_identity_id(u));
    match (user, b.code.as_deref().map(str::trim), b.accept_ticket.as_deref(), b.email.as_deref()) {
        (Some(user), Some(code), None, None) if is_code(code) => Some(Proof::Code { user, code }),
        (Some(user), None, Some(ticket), None) if b.user.is_some() => Some(Proof::Ticket { user, ticket }),
        (None, None, None, Some(email)) if b.user.is_none() && !email.trim().is_empty() => {
            Some(Proof::Account { email: normalize_email(email) })
        }
        _ => None,
    }
}

#[utoipa::path(
    post,
    path = "/auth/v1/accept-invite",
    operation_id = "acceptInvite",
    tag = "login",
    description = "Accepts a member invite and signs in (the HttpOnly session cookie). With `user` and `code` (the code from the invite email), the code sets the first password and verifies the email; an account someone else registered without proving the email has its old password replaced and every session ended. With `email` (an existing account, from the invite notice), the account's own password signs in and no password is set. The link's token `t` must match an open invite. A removed member's ban is lifted only then; any other ban is `member_suspended`. If a step after the code fails, the error is `invite_incomplete` with `error.accept_ticket`: send it back in place of `code`, with the same password, to finish. A weak password is refused before the code is used.",
    request_body = AcceptInviteBody,
    responses(
        (status = 200, description = "Signed in: `studio` with the session cookie set (or `pending`, with none)", body = SignInAnswer),
        (status = 400, description = "The body is malformed, or the link, code or ticket is wrong, used or closed (`invalid_request`), or the password is too weak (`weak_password`; nothing was used)", body = ErrorEnvelope),
        (status = 401, description = "With `email`: the email or password is wrong (`invalid_credentials`)", body = ErrorEnvelope),
        (status = 403, description = "The account is banned outside Chelabase, or the member is suspended (`member_suspended`): no sign-in", body = ErrorEnvelope),
        (status = 410, description = "The invite expired (`invite_expired`) or its account is gone (`invite_revoked`)", body = ErrorEnvelope),
        (status = 413, description = "The body is over 8 KiB: `too_large`", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON (wrong content type): `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "The body is JSON but has the wrong fields: `invalid_request`", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "The identity service or the orchestrator is unavailable (`unavailable`), or a step after the code failed (`invite_incomplete`, with `accept_ticket`)", body = ErrorEnvelope),
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

/// A password identity can take: our complexity rule, and at most 72 bytes (counted in bytes, so
/// 39 characters can be too many).
pub(crate) fn usable_password(password: &str) -> bool {
    password.len() <= refusals::PASSWORD_MAX_BYTES && strong_password(password)
}

fn bad_invite() -> Response {
    error(StatusCode::BAD_REQUEST, ErrorCode::InvalidRequest, BAD_INVITE)
}

fn suspended() -> Response {
    error(StatusCode::FORBIDDEN, ErrorCode::MemberSuspended, MEMBER_SUSPENDED)
}

/// A link whose invite isn't open (any more): 410 `invite_expired` when the user's stored invite
/// expired, else (none, a closed one, a wrong token, the check failing) 400.
async fn closed_invite(orchestrator: &crate::app_users::InternalClient, user: &str) -> Response {
    match orchestrator.open_invite(user).await {
        Ok(OpenInvite { open: false, expires_at: Some(_) }) => {
            error(StatusCode::GONE, ErrorCode::InviteExpired, INVITE_EXPIRED)
        }
        Ok(_) => bad_invite(),
        Err(e) => {
            warn!(error = %e, "accept-invite: the open-invite check failed");
            bad_invite()
        }
    }
}

/// A step after the code was spent failed: tell the browser how to finish.
fn incomplete(s: &AppState, user: &str) -> Response {
    let ticket = seal_ticket(
        &s.config.cookie_key,
        &AcceptTicket {
            user_id: user.to_string(),
            exp: now_secs() + TICKET_TTL_SECS,
            nonce: uuid::Uuid::new_v4().simple().to_string(),
        },
    );
    ApiError::new(StatusCode::SERVICE_UNAVAILABLE.as_u16(), ErrorCode::InviteIncomplete, INVITE_INCOMPLETE)
        .with_extra("accept_ticket", json!(ticket))
        .into_response()
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
    // Validated before any key is built or any service is asked.
    let Some(proof) = proof_of(&b) else {
        return bad_invite();
    };
    // Before the code is spent. An existing account's own password is whatever it is.
    if !matches!(proof, Proof::Account { .. }) && !usable_password(&b.password) {
        slot.refund();
        return error(StatusCode::BAD_REQUEST, ErrorCode::WeakPassword, WEAK_PASSWORD);
    }
    let Some(org) = org_id(s) else {
        slot.refund();
        return no_org();
    };
    let Some(orchestrator) = &s.signup_gate else {
        warn!("accept-invite needs the orchestrator's internal client");
        slot.refund();
        return error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE);
    };
    let token_hash = hex::encode(Sha256::digest(b.t.as_bytes()));
    let accepting = |user_id, may_unban, retry| Accepting {
        org,
        orchestrator,
        user_id,
        token_hash: &token_hash,
        password: &b.password,
        may_unban,
        retry,
    };
    match proof {
        Proof::Code { user, code } => {
            let Some(user_slot) =
                Slot::reserve(&s.limiter, format!("accept-invite:user:{user}"), 5, HOUR)
            else {
                slot.refund();
                return error(StatusCode::TOO_MANY_REQUESTS, ErrorCode::RateLimited, TOO_MANY);
            };
            match check_code(s, orchestrator, user, &token_hash, code, &b.password).await {
                Checked::Spent => user_slot.refund(),
                Checked::Refused(answer) => return answer,
                Checked::NotCounted(answer) => {
                    user_slot.refund();
                    slot.refund();
                    return answer;
                }
                Checked::AddressCountsOnly(answer) => {
                    user_slot.refund();
                    return answer;
                }
            }
            // From here the code is spent, and no outcome counts against the address.
            let answer = finish_accept(s, &accepting(user, true, Retry::Ticket), None).await;
            slot.refund();
            answer
        }
        Proof::Ticket { user, ticket } => {
            // Claimed up front: a failed attempt hands back a fresh ticket, so this one is spent
            // either way, and two concurrent uses can't both pass. Per process: the barrier
            // across replicas is the code itself, which identity uses once.
            match open_ticket(&s.config.cookie_key, ticket, now_secs()) {
                Some(t) if t.user_id == user && s.used_tickets.claim(&t.nonce, t.exp, now_secs()) => {}
                _ => return bad_invite(),
            }
            let answer = finish_accept(s, &accepting(user, true, Retry::Ticket), None).await;
            slot.refund();
            answer
        }
        Proof::Account { email } => {
            // A password guess like a sign-in's, so it spends the same per-email budget.
            let Some(email_slot) = Slot::reserve(
                &s.limiter,
                format!("sign-in:email:{}", email_tag(&email)),
                5,
                MINUTE,
            ) else {
                return error(StatusCode::TOO_MANY_REQUESTS, ErrorCode::RateLimited, TOO_MANY);
            };
            let refused = || error(StatusCode::UNAUTHORIZED, ErrorCode::InvalidCredentials, BAD_CREDENTIALS);
            // The password before anything about the invite is told. Identity checks it before
            // the ban, so `Banned` means the password is right.
            let signed = match s.identity.sign_in(&Pool::Members, &email, &b.password).await {
                Ok(signed) => Some(signed),
                Err(IdentityError::Banned) => None,
                Err(IdentityError::BadCredentials | IdentityError::SessionRevoked | IdentityError::Invalid { .. }) => {
                    return refused();
                }
                Err(e) => {
                    email_slot.refund();
                    slot.refund();
                    return identity_unavailable(&e);
                }
            };
            email_slot.refund();
            let (user_id, may_unban) = match &signed {
                // The link's token must match this account's open invite: without it, the
                // orchestrator's retry for an active member would sign anyone in with any `t`.
                Some(signed) => match orchestrator.describe_invite(&signed.user.id, &token_hash).await {
                    Ok(Some(_)) => (signed.user.id.clone(), false),
                    Ok(None) => {
                        drop_session(s, &signed.session_id).await;
                        return bad_invite();
                    }
                    Err(e) => {
                        warn!(error = %e, "accept-invite: the invite check failed");
                        drop_session(s, &signed.session_id).await;
                        slot.refund();
                        return error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE);
                    }
                },
                // A banned account: our ban is lifted only for an open invite this link matches.
                // Anything else answers as sign-in does for a banned account (a wrong password),
                // so this is no oracle for a banned account's password.
                None => {
                    let user = match s.identity.find_user_by_email(&Pool::Members, &email).await {
                        Ok(Some(user)) => user,
                        Ok(None) => return refused(),
                        Err(e) => {
                            slot.refund();
                            return identity_unavailable(&e);
                        }
                    };
                    match s.identity.user_detail(&Pool::Members, &user.id).await {
                        Ok(d) if d.ban_reason.as_deref() == Some(MEMBER_REMOVED_BAN_REASON) => {}
                        Ok(_) | Err(IdentityError::NotFound(Missing::User)) => return refused(),
                        Err(e) => {
                            slot.refund();
                            return identity_unavailable(&e);
                        }
                    }
                    match orchestrator.describe_invite(&user.id, &token_hash).await {
                        Ok(Some(_)) => {}
                        Ok(None) => return refused(),
                        Err(e) => {
                            warn!(error = %e, "accept-invite: the invite check failed");
                            slot.refund();
                            return error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE);
                        }
                    }
                    (user.id, true)
                }
            };
            let answer = finish_accept(s, &accepting(&user_id, may_unban, Retry::Again), signed).await;
            if answer.status() != StatusCode::BAD_REQUEST {
                slot.refund();
            }
            answer
        }
    }
}

/// How a code check ended, and what it counts against.
enum Checked {
    /// The code is used: the first password is set and the email verified.
    Spent,
    /// Counts against the address and the invitee (a wrong code, a closed link).
    Refused(Response),
    /// Counts against nothing (an outage, a weak password, a foreign ban).
    NotCounted(Response),
    /// Counts against the address only (the invitee is gone).
    AddressCountsOnly(Response),
}

/// The link, then the account, then the code. Nothing changes unless the code is right; a weak
/// password, a foreign ban and a closed link leave the code unused.
async fn check_code(
    s: &AppState,
    orchestrator: &crate::app_users::InternalClient,
    user: &str,
    token_hash: &str,
    code: &str,
    password: &str,
) -> Checked {
    let down = || error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE);
    let revoked = || error(StatusCode::GONE, ErrorCode::InviteRevoked, INVITE_REVOKED);
    match orchestrator.describe_invite(user, token_hash).await {
        Ok(Some(_)) => {}
        Ok(None) => return Checked::Refused(closed_invite(orchestrator, user).await),
        Err(e) => {
            warn!(error = %e, "accept-invite: the invite check failed");
            return Checked::NotCounted(down());
        }
    }
    let detail = match s.identity.user_detail(&Pool::Members, user).await {
        Ok(detail) => detail,
        Err(IdentityError::NotFound(Missing::User)) => {
            info!("accept-invite: the user is gone");
            return Checked::AddressCountsOnly(revoked());
        }
        Err(e) => return Checked::NotCounted(identity_unavailable(&e)),
    };
    // Only our own ban is ours to lift; any other stays, and the code stays unused.
    if detail.user.banned && detail.ban_reason.as_deref() != Some(MEMBER_REMOVED_BAN_REASON) {
        info!("accept-invite: the account is banned outside Chelabase");
        return Checked::NotCounted(suspended());
    }
    match s
        .identity
        .verify_code(&Pool::Members, user, CodeKind::Invite, code, Some(password))
        .await
    {
        Ok(_) => Checked::Spent,
        Err(IdentityError::Invalid { code, .. }) if code == "weak_password" => {
            Checked::NotCounted(error(StatusCode::BAD_REQUEST, ErrorCode::WeakPassword, WEAK_PASSWORD))
        }
        // A wrong, used, replaced or locked code, or an account an invite can't set (an
        // established one): counted.
        Err(IdentityError::CodeInvalid | IdentityError::Invalid { .. }) => Checked::Refused(bad_invite()),
        Err(IdentityError::NotFound(Missing::User)) => Checked::AddressCountsOnly(revoked()),
        Err(e @ IdentityError::Unavailable(_)) => Checked::NotCounted(identity_unavailable(&e)),
        // Any other refusal is identity answering about the code, not an outage: it counts.
        Err(e) => Checked::Refused(identity_unavailable(&e)),
    }
}

/// What a failed step after the proof answers.
#[derive(Clone, Copy)]
enum Retry {
    /// The code is spent: a ticket finishes the rest.
    Ticket,
    /// Nothing was spent: the same request again.
    Again,
}

#[derive(Clone, Copy)]
struct Accepting<'a> {
    org: &'a str,
    orchestrator: &'a crate::app_users::InternalClient,
    user_id: &'a str,
    token_hash: &'a str,
    password: &'a str,
    /// Our ban may be lifted: an open invite this link matches was confirmed.
    may_unban: bool,
    retry: Retry,
}

/// The steps after the proof, each safe to repeat: the member row (the orchestrator activates the
/// invited row, closes the invite and clears the offboarded mark; an active row is a retry), then
/// our ban lifted, then the session. The row comes before the unban, so an invite revoked or a
/// member removed meanwhile lifts nothing. `signed` is the account's session when the proof made
/// one; it is ended unless the answer keeps it.
async fn finish_accept(s: &AppState, a: &Accepting<'_>, signed: Option<SignedIn>) -> Response {
    let session = signed.as_ref().map(|s| s.session_id.clone());
    let answer = finish_steps(s, a, signed).await;
    if let Some(id) = session
        && answer.status() != StatusCode::OK
    {
        drop_session(s, &id).await;
    }
    answer
}

async fn finish_steps(s: &AppState, a: &Accepting<'_>, signed: Option<SignedIn>) -> Response {
    use crate::app_users::internal::InternalError;
    let Accepting { org, orchestrator, user_id, token_hash, password, may_unban, retry } = *a;
    let fail = |what: &str, e: &dyn std::fmt::Display| {
        warn!(error = %e, "accept-invite: {what} failed");
        match retry {
            Retry::Ticket => incomplete(s, user_id),
            Retry::Again => error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE),
        }
    };
    let detail = match s.identity.user_detail(&Pool::Members, user_id).await {
        Ok(detail) => detail,
        Err(IdentityError::NotFound(Missing::User)) => {
            return error(StatusCode::GONE, ErrorCode::InviteRevoked, INVITE_REVOKED);
        }
        Err(e) => return fail("reading the account", &e),
    };
    let ours = detail.ban_reason.as_deref() == Some(MEMBER_REMOVED_BAN_REASON);
    if detail.user.banned && !(ours && may_unban) {
        return if ours { bad_invite() } else { suspended() };
    }
    let tenant = match orchestrator
        .accept_invite(org, user_id, token_hash, &detail.user.email, &detail.user.name)
        .await
    {
        Ok(tenant) => tenant,
        Err(InternalError::Refused { status: 403, code }) if code == "member_suspended" => return suspended(),
        Err(InternalError::Refused { status: 410, .. }) => {
            return error(StatusCode::GONE, ErrorCode::InviteExpired, INVITE_EXPIRED);
        }
        // No open invite matches the link, and the person isn't a member already.
        Err(InternalError::Refused { .. }) => return bad_invite(),
        Err(e) => return fail("accepting the invite", &e),
    };
    if detail.user.banned
        && let Err(e) = s.identity.unban(&Pool::Members, user_id).await
    {
        return fail("lifting the removal's ban", &e);
    }
    let signed = match signed {
        Some(signed) => signed,
        None => match s.identity.sign_in(&Pool::Members, &detail.user.email, password).await {
            Ok(signed) => signed,
            // A ticket with another password than the code set: the ticket is spent.
            Err(IdentityError::BadCredentials | IdentityError::Invalid { .. }) => return bad_invite(),
            Err(IdentityError::Banned) => return suspended(),
            Err(e) => return fail("signing in", &e),
        },
    };
    let tenant = tenant.to_string();
    match conclude(s, signed, Some(&tenant)).await {
        Concluded::Answer(answer) if answer.status() == StatusCode::SERVICE_UNAVAILABLE => {
            fail("reading the memberships", &"unavailable")
        }
        Concluded::Answer(answer) => answer,
        Concluded::Refused => fail("choosing the tenant", &"no active membership"),
    }
}

/// A sign-up for a taken email. With verification on (platform), it's answered exactly like a
/// new one, with a cookie that can't verify. With it off (self-hosted), a new sign-up has no
/// code step to hide behind, so the user is told and Studio sends them to sign in.
fn taken(s: &Arc<AppState>, email: &str) -> Response {
    if s.config.skip_email_verification {
        info!(email = %email_tag(email), "sign-up for a taken email");
        return error(StatusCode::CONFLICT, ErrorCode::EmailTaken, ACCOUNT_EXISTS);
    }
    decoy(s, email)
}

/// Answer a sign-up for a taken email exactly like a new one, with a cookie that can't verify.
/// The account's owner gets a password-reset email instead, so a real owner who forgot they
/// signed up has a way back in.
fn decoy(s: &Arc<AppState>, email: &str) -> Response {
    info!(email = %email_tag(email), "sign-up for a taken email (decoy)");
    spawn_reset_email(s, email.to_string());
    let decoy = LoginState {
        // Only a limit key here ([`step_key`]): a decoy has no user.
        auth_request: uuid::Uuid::new_v4().to_string(),
        user_id: None,
        session_id: None,
        session_token: None,
        step: Step::Verify,
        decoy: true,
        exp: now_secs() + LOGIN_TTL_SECS,
    };
    reply(
        StatusCode::OK,
        json!(SignInAnswer { next: SignInStep::Verify }),
        vec![login_cookie(s, &decoy)],
    )
}

/// The rate-limit key for a pending code step: the user, or a decoy's own id, so real and decoy
/// cookies are limited alike.
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
    description = "Checks the emailed code. Reads the HttpOnly step cookie and clears it once the email is verified. An open sign-up joins its tenant now, as a person waiting for approval. The answer is a sign-in's: `studio` sets the HttpOnly session cookie, `pending` sets none. If sign-up closed since the account was made, the answer is `sign_up_closed` and the account is deleted. A retry after a failure past the code finishes with the same code.",
    request_body = CodeBody,
    responses(
        (status = 200, description = "Email verified: `studio` (the session cookie is set) or `pending`; the step cookie is cleared", body = SignInAnswer),
        (status = 400, description = "The body is malformed (`invalid_request`) or the code is wrong (`invalid_code`)", body = ErrorEnvelope),
        (status = 403, description = "Sign-up closed since the account was made: `sign_up_closed`; the account is deleted", body = ErrorEnvelope),
        (status = 409, description = "The person has several tenants: `tenant_required`, with their ids in `error.tenants`; sign in with `?tenant=`", body = ErrorEnvelope),
        (status = 410, description = "This sign-in has expired or is unknown: `sign_in_expired`", body = ErrorEnvelope),
        (status = 413, description = "The body is over 8 KiB: `too_large`", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON (wrong content type): `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "The body is JSON but has the wrong fields: `invalid_request`", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "The identity service or the orchestrator is unavailable: `unavailable`; the step stays", body = ErrorEnvelope),
    ),
)]
async fn verify_email(State(s): S, headers: HeaderMap, ApiJson(b): ApiJson<CodeBody>) -> Response {
    let start = Instant::now();
    let answer = verify_email_inner(&s, &headers, b).await;
    pad(start, s.config.min_response).await;
    answer
}

/// Adds the step cookie's clearing to an answer that doesn't already set it.
fn step_ended(s: &AppState, mut answer: Response) -> Response {
    let set = answer
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .any(|c| c.as_bytes().starts_with(format!("{LOGIN_COOKIE}=").as_bytes()));
    if !set {
        let clear = clear_cookie(LOGIN_COOKIE, s.config.secure_cookies);
        answer
            .headers_mut()
            .append(header::SET_COOKIE, clear.parse().expect("cookie header"));
    }
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
    let bad_code = || error(StatusCode::BAD_REQUEST, ErrorCode::InvalidCode, BAD_CODE);
    let (Some(user_id), Some(session_id), Some(session_token), false) = (
        state.user_id,
        state.session_id,
        state.session_token,
        state.decoy,
    ) else {
        return bad_code();
    };
    let Some(org) = org_id(s) else {
        return no_org();
    };
    let user = match s
        .identity
        .verify_code(&Pool::Members, &user_id, CodeKind::Verify, b.code.trim(), None)
        .await
    {
        Ok(user) => user,
        // Refused, or spent by an earlier attempt whose later step failed: an email already
        // verified carries on (the step cookie is the password's proof), so a retry finishes.
        Err(IdentityError::CodeInvalid | IdentityError::Invalid { .. } | IdentityError::NotFound(Missing::User)) => {
            match s.identity.get_user(&Pool::Members, &user_id).await {
                Ok(user) if user.email_verified => user,
                Ok(_) | Err(IdentityError::NotFound(Missing::User)) => return bad_code(),
                Err(e) => return identity_unavailable(&e),
            }
        }
        Err(e) => return identity_unavailable(&e),
    };
    // On both arms, before anything changes: the step's session must still be live and the
    // account's. A reset or an accepted invite ends every session of the account, and a step
    // cookie from before it proves nothing any more, so it may not delete, join or sign in.
    match s.identity.session_by_id(&Pool::Members, &session_id).await {
        Ok(live) if live.user.id == user_id => {}
        Ok(_) | Err(IdentityError::NotFound(Missing::Session | Missing::User)) => {
            return step_ended(s, error(StatusCode::GONE, ErrorCode::SignInExpired, GONE));
        }
        Err(e) => return identity_unavailable(&e),
    }
    match join_verified(s, org, &user.id).await {
        Ok(true) => {}
        Ok(false) => return step_ended(s, sign_up_closed_meanwhile(s, &user.id, &session_id).await),
        Err(()) => return error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE),
    }
    let signed = SignedIn { session_id, session_token, user };
    match conclude(s, signed, None).await {
        Concluded::Answer(answer) if answer.status().is_server_error() => answer,
        Concluded::Answer(answer) => step_ended(s, answer),
        // Joined a moment ago, and already out of the tenant.
        Concluded::Refused => error(StatusCode::SERVICE_UNAVAILABLE, ErrorCode::Unavailable, UNAVAILABLE),
    }
}

/// #121: with verification on, an open sign-up's `pending` row waits for the verified email.
/// `Ok(true)` once the person has a row to land in: an active or pending one already (the
/// fresh-install admin, an approved member), or the join the gate takes now. `Ok(false)` when
/// the gate refuses the join (sign-up closed meanwhile).
async fn join_verified(s: &AppState, org: &str, user_id: &str) -> Result<bool, ()> {
    let Some(orchestrator) = &s.signup_gate else {
        warn!("verify-email needs the orchestrator's internal client");
        return Err(());
    };
    match orchestrator.member_tenants(user_id).await {
        Ok(t) if !t.active.is_empty() || !t.pending.is_empty() => return Ok(true),
        Ok(_) => {}
        Err(e) => {
            warn!(error = %e, "verify-email: reading the memberships failed");
            return Err(());
        }
    }
    orchestrator.signup_joined(org, user_id).await.map_err(|e| {
        warn!(error = %e, "verify-email: joining the open sign-up failed");
    })
}

/// Sign-up closed between the account and its code: the session ends and the account is
/// deleted, as a refused sign-up's is, so a sign-up once it reopens starts afresh. An account an
/// invite holds (or one the check can't clear) is kept: deleting it would break the invite.
async fn sign_up_closed_meanwhile(s: &AppState, user_id: &str, session_id: &str) -> Response {
    drop_session(s, session_id).await;
    let invited = match &s.signup_gate {
        Some(orchestrator) => orchestrator
            .open_invite(user_id)
            .await
            .map(|i| i.expires_at.is_some())
            .unwrap_or(true),
        None => true,
    };
    if invited {
        info!(user_id, "sign-up closed before the email was verified; the account stays for its invite");
    } else if let Err(e) = s.identity.delete_user(&Pool::Members, user_id).await {
        warn!(user_id, error = %e, "deleting the account of a closed sign-up failed");
    } else {
        info!(user_id, "sign-up closed before the email was verified; the account is deleted");
    }
    Refusal::Closed.response()
}

#[derive(Deserialize, ToSchema)]
struct EmptyRequest {}

#[utoipa::path(
    post,
    path = "/auth/v1/resend-code",
    operation_id = "resendCode",
    tag = "login",
    description = "Sends the verification code again. Reads the HttpOnly step cookie. Always answers the same, whether or not a code was sent; at most 5 codes go out per person an hour.",
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
    if let Some(user) = state.filter(|st| !st.decoy).and_then(|st| st.user_id) {
        send_code(s, &user, CodeKind::Verify).await;
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
    description = "Emails a password-reset code if the account exists, in the background. Always answers the same, in the same padded time, whether or not it does. At most 5 codes go out per person an hour.",
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
    let start = Instant::now();
    let answer = password_reset_inner(&s, &headers, b);
    pad(start, s.config.min_response).await;
    answer
}

fn password_reset_inner(s: &Arc<AppState>, headers: &HeaderMap, b: EmailBody) -> Response {
    let email = normalize_email(&b.email);
    let ip = client_ip(headers);
    if !allowed(
        s,
        &[
            (format!("reset:ip:{ip}"), 20, HOUR),
            (format!("reset:email:{}", email_tag(&email)), 5, HOUR),
        ],
    ) {
        return error(StatusCode::TOO_MANY_REQUESTS, ErrorCode::RateLimited, TOO_MANY);
    }
    // Answered the same whether or not the account exists, but never "sent" while the instance
    // isn't set up.
    if org_id(s).is_none() {
        return no_org();
    }
    spawn_reset_email(s, email);
    reply(StatusCode::OK, json!(EmptyResponse {}), Vec::new())
}

/// Mails a reset code to `email`'s account, if it has one, in the background, so the answer time
/// doesn't reveal whether it exists. The mail links to the reset page, which asks for the code:
/// no id and no code is ever in the link.
fn spawn_reset_email(s: &Arc<AppState>, email: String) {
    let s2 = s.clone();
    tokio::spawn(async move {
        match s2.identity.find_user_by_email(&Pool::Members, &email).await {
            Ok(Some(user)) => send_code(&s2, &user.id, CodeKind::Reset).await,
            Ok(None) => info!(email = %email_tag(&email), "password reset for an unknown email"),
            Err(e) => warn!(error = %e, "password reset lookup failed"),
        }
    });
}

#[derive(Deserialize, ToSchema)]
struct ConfirmBody {
    email: String,
    /// The six-digit code from the reset email.
    code: String,
    password: String,
}

#[utoipa::path(
    post,
    path = "/auth/v1/password-reset/confirm",
    operation_id = "confirmPasswordReset",
    tag = "login",
    description = "Sets a new password with the code from the reset email, verifies the email and ends every session of the account. A wrong code and an unknown email answer alike. Sets no cookie: sign in with the new password.",
    request_body = ConfirmBody,
    responses(
        (status = 200, description = "Done", body = EmptyResponse),
        (status = 400, description = "The body is malformed (`invalid_request`), the password is too weak (`weak_password`; the code is not used) or the code is wrong (`invalid_code`)", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON (wrong content type): `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "The body is JSON but has the wrong fields: `invalid_request`", body = ErrorEnvelope),
        (status = 413, description = "The body is over 8 KiB: `too_large`", body = ErrorEnvelope),
        (status = 429, description = "Too many attempts: `rate_limited`", body = ErrorEnvelope),
        (status = 503, description = "The identity service is unavailable: `unavailable`", body = ErrorEnvelope),
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
    let email = normalize_email(&b.email);
    if !allowed(
        s,
        &[
            (format!("confirm:ip:{}", client_ip(headers)), 10, MINUTE),
            (format!("confirm:email:{}", email_tag(&email)), 10, HOUR),
        ],
    ) {
        return error(StatusCode::TOO_MANY_REQUESTS, ErrorCode::RateLimited, TOO_MANY);
    }
    let weak = || error(StatusCode::BAD_REQUEST, ErrorCode::WeakPassword, WEAK_PASSWORD);
    let bad_code = || error(StatusCode::BAD_REQUEST, ErrorCode::InvalidCode, BAD_CODE);
    // Checked before identity sees the email, so this answer can't tell accounts apart.
    if !usable_password(&b.password) {
        return weak();
    }
    let code = b.code.trim();
    if !is_code(code) {
        return bad_code();
    }
    let user = match s.identity.find_user_by_email(&Pool::Members, &email).await {
        Ok(Some(user)) => user,
        Ok(None) => return bad_code(),
        Err(e) => return identity_unavailable(&e),
    };
    // Sets the password, verifies the email and ends every session of the user, in one step.
    match s
        .identity
        .verify_code(&Pool::Members, &user.id, CodeKind::Reset, code, Some(&b.password))
        .await
    {
        Ok(_) => reply(StatusCode::OK, json!(EmptyResponse {}), Vec::new()),
        Err(IdentityError::Invalid { code, .. }) if code == "weak_password" => weak(),
        Err(IdentityError::CodeInvalid | IdentityError::Invalid { .. } | IdentityError::NotFound(Missing::User)) => {
            bad_code()
        }
        Err(e) => identity_unavailable(&e),
    }
}

#[utoipa::path(
    post,
    path = "/auth/v1/logout",
    operation_id = "logout",
    tag = "login",
    description = "Revokes the session and clears the HttpOnly session and step cookies. If the identity service can't be reached the cookies stay, so the client can try again.",
    request_body = EmptyRequest,
    responses(
        (status = 200, description = "Done", body = EmptyResponse),
        (status = 400, description = "The body is malformed: `invalid_request`", body = ErrorEnvelope),
        (status = 415, description = "The body is not JSON (wrong content type): `invalid_request`", body = ErrorEnvelope),
        (status = 422, description = "The body is JSON but has the wrong fields: `invalid_request`", body = ErrorEnvelope),
        (status = 413, description = "The body is over 8 KiB: `too_large`", body = ErrorEnvelope),
        (status = 503, description = "The identity service is unavailable: `unavailable`; the session is still live", body = ErrorEnvelope),
    ),
)]
async fn logout(State(s): S, headers: HeaderMap, ApiJson(_): ApiJson<EmptyRequest>) -> Response {
    if let Some(SessionState::Identity { session_id, .. }) = read_cookie(&headers, SESSION_COOKIE)
        .and_then(|raw| open_session(&s.config.cookie_key, raw, now_secs()))
    {
        match s.identity.revoke_session(&Pool::Members, &session_id).await {
            Ok(()) | Err(IdentityError::NotFound(Missing::Session)) => {}
            Err(e) => return identity_unavailable(&e),
        }
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
