#![allow(dead_code)]
//! A stateful stand-in for Zitadel v4's Session, User and OIDC v2 REST APIs.

use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use crate::refusals::{self, Refusal};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub const TOKEN: &str = "test-login-client";
/// Also accepted everywhere; the only token allowed admin-only calls.
pub const ADMIN_TOKEN: &str = "test-admin";
pub const EMAIL_CODE: &str = "123456";
pub const RESET_CODE: &str = "654321";

#[derive(Clone, Debug)]
pub struct User {
    pub id: String,
    pub email: String,
    pub password: String,
    pub verified: bool,
    pub locked: bool,
    pub org_id: String,
    pub username: String,
    /// `human.profile.displayName`; empty when unset.
    pub display_name: String,
    pub inactive: bool,
    pub created_at: String,
}

#[derive(Default)]
pub struct Inner {
    pub users: Vec<User>,
    pub orgs: Vec<String>,
    /// `"METHOD path"` of every request, in order.
    pub calls: Vec<String>,
    pub sessions: HashMap<String, (String, String)>,
    pub auth_requests: HashMap<String, bool>,
    pub emails: Vec<(String, String)>,
    pub next: u64,
    pub down: bool,
    /// Like Zitadel's projections: a user made by `create_user` isn't visible to sessions
    /// for this many attempts.
    pub new_user_lag: u32,
    pub hidden: HashMap<String, u32>,
    /// Zitadel checks usernames, not emails, for uniqueness; this mimics a username clash-free duplicate.
    pub allow_duplicate_emails: bool,
    /// Every `DELETE /v2/sessions/{id}` waits this long first (a slow Zitadel).
    pub session_delete_delay: std::time::Duration,
    /// Every `GET /v2/users/{id}` waits this long first (a slow Zitadel).
    pub user_lookup_delay: std::time::Duration,
    /// Run when a request whose `"METHOD path"` contains the pattern arrives, before it is
    /// answered: something that happens elsewhere while Zitadel is still answering.
    pub on_request: Vec<(String, Arc<dyn Fn() + Send + Sync>)>,
    /// Pending invite codes: user id -> (code, expired).
    pub invites: HashMap<String, (String, bool)>,
    /// The URL template and application name of each user's last invite mail.
    pub invite_mails: HashMap<String, (String, String)>,
    /// Users in Zitadel's Initial state (`USER_STATE_INITIAL`).
    pub initial_users: Vec<String>,
    /// At most this many sessions per `sessions/search` page (0: no limit).
    pub session_page: usize,
    /// Forced answers: a request whose `"METHOD path"` contains the pattern gets this error
    /// (`status`, Zitadel error key) while `remaining` is above zero.
    pub faults: Vec<Fault>,
    /// The login client's token is refused on the calls it can't make in a real instance:
    /// invite verification, user update, and a password set with neither code nor current password.
    pub strict_login: bool,
    /// The email providers `POST /admin/v1/email/_search` lists: (id, active).
    pub email_providers: Vec<(String, bool)>,
    /// Authentication methods besides a password, by user (`AUTHENTICATION_METHOD_TYPE_*`).
    pub auth_methods: HashMap<String, Vec<String>>,
    /// The URL template of each user's last password-reset mail (`sendLink`).
    pub reset_mails: HashMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct Fault {
    pub pattern: String,
    pub status: u16,
    pub key: String,
    /// Answer with exactly this body instead of Zitadel's error shape.
    pub body: Option<String>,
    pub remaining: u32,
}

#[derive(Clone)]
pub struct MockZitadel {
    pub base: String,
    pub inner: Arc<Mutex<Inner>>,
}

type Reply = (StatusCode, Json<Value>);
fn ok(v: Value) -> Reply {
    (StatusCode::OK, Json(v))
}
/// A refusal by its old error key. `journal` answers it in Zitadel's shape (`zitadel_refusal`).
fn err(status: StatusCode, key: &str) -> Reply {
    (status, Json(json!({ MOCK_KEY: key })))
}

/// Where a handler's refusal keeps its key until `journal` shapes it.
const MOCK_KEY: &str = "mockRefusalKey";

/// The refusal Zitadel v4.19.1 answers for a request (`"METHOD path"`) refused with this old
/// error key, as `tests/zitadel_live.rs` captured it. `None` for one not captured: the mock
/// never answers its own refusal with an id nobody captured (see `zitadel_refusal`).
pub fn captured(call: &str, key: &str) -> Option<Refusal> {
    use refusals as r;
    let verb = |v: &str| call.starts_with("POST /v2/users/") && call.ends_with(&format!("/{v}"));
    Some(match key {
        "Errors.User.NotFound" if call.ends_with("/authentication_methods") => {
            r::AUTH_METHODS_USER_NOT_FOUND
        }
        "Errors.User.NotFound" if call.starts_with("GET /v2/users/") => r::GET_USER_NOT_FOUND,
        "Errors.User.NotFound" if call.starts_with("PATCH /v2/users/") => r::UPDATE_USER_NOT_FOUND,
        "Errors.User.NotFound" if verb("email") => r::SET_EMAIL_USER_NOT_FOUND,
        "Errors.User.NotFound" if verb("invite_code") => r::CREATE_INVITE_USER_NOT_FOUND,
        "Errors.User.NotFound" if call.starts_with("DELETE /v2/users/") => r::DELETE_USER_NOT_FOUND,
        "Errors.User.NotFound" if verb("lock") => r::LOCK_USER_NOT_FOUND,
        "Errors.User.NotFound" if verb("unlock") => r::UNLOCK_USER_NOT_FOUND,
        "Errors.User.NotFound" if verb("deactivate") => r::DEACTIVATE_USER_NOT_FOUND,
        "Errors.User.NotFound" if verb("reactivate") => r::REACTIVATE_USER_NOT_FOUND,
        "Errors.User.NotFound" if call.ends_with("/invite_code/verify") => r::INVITE_USER_NOT_FOUND,
        "Errors.User.NotFound" if call == "POST /v2/sessions" => r::SESSION_USER_NOT_FOUND,
        // State refusals belong to their own endpoint: the same key on another one isn't captured.
        "Errors.User.AlreadyLocked" | "Errors.User.NotActive" if verb("lock") => r::LOCK_NOT_ACTIVE,
        "Errors.User.NotLocked" if verb("unlock") => r::UNLOCK_NOT_LOCKED,
        "Errors.User.AlreadyInactive" if verb("deactivate") => r::DEACTIVATE_ALREADY_INACTIVE,
        "Errors.User.CannotDeactivateInitial" if verb("deactivate") => r::DEACTIVATE_INITIAL,
        "Errors.User.NotInactive" if verb("reactivate") => r::REACTIVATE_NOT_INACTIVE,
        // Zitadel answers a wrong, an expired and a missing invite code alike.
        k if k.starts_with("Errors.User.Code.") && call.ends_with("/invite_code/verify") => {
            r::INVITE_CODE_INVALID
        }
        k if k.starts_with("Errors.User.Code.") && call.ends_with("/email/verify") => {
            r::EMAIL_CODE_INVALID
        }
        k if k.starts_with("Errors.User.Code.") && call.ends_with("/password") => {
            r::PASSWORD_CODE_NOT_FOUND
        }
        "Errors.User.Password.Invalid" => r::PASSWORD_INVALID,
        "Errors.User.PasswordComplexityPolicy.MinLength" if call == "POST /v2/users/new" => {
            r::CREATE_USER_PASSWORD_TOO_SHORT
        }
        "Errors.User.PasswordComplexityPolicy.MinLength" => r::PASSWORD_TOO_SHORT,
        "Errors.User.PasswordComplexityPolicy.HasUpper" => r::PASSWORD_NO_UPPER,
        "Errors.User.PasswordComplexityPolicy.HasLower" => r::PASSWORD_NO_LOWER,
        "Errors.User.PasswordComplexityPolicy.HasNumber" => r::PASSWORD_NO_NUMBER,
        "Errors.User.PasswordComplexityPolicy.HasSymbol" => r::PASSWORD_NO_SYMBOL,
        k if k.starts_with("Errors.User.Email.NotChanged") => r::EMAIL_NOT_CHANGED,
        "Errors.User.AlreadyExists" => r::USER_ALREADY_EXISTS,
        "Errors.Org.AlreadyExists" => r::ORG_ALREADY_EXISTS,
        "Errors.Org.NotFound" if call == "POST /v2/users/new" => r::CREATE_USER_ORG_NOT_FOUND,
        "Errors.User.AlreadyInitialised" => r::INVITE_USER_INITIALIZED,
        "Errors.AuthRequest.NotFound" if call.starts_with("GET ") => r::AUTH_REQUEST_NOT_FOUND,
        "Errors.AuthRequest.NotFound" if call.starts_with("POST ") => {
            r::FINISH_AUTH_REQUEST_NOT_FOUND
        }
        "Errors.User.Locked" if call == "POST /v2/sessions" => r::SESSION_USER_NOT_ACTIVE,
        "Errors.Token.Invalid" => r::TOKEN_INVALID,
        // Zitadel's authorization check answers the same on every endpoint.
        "Errors.PermissionDenied" => r::NO_PERMISSION,
        _ => return None,
    })
}

/// Zitadel v4.19.1's error body for a refusal: the gRPC `code`, a `message` ending in the
/// detail id, and one `details[]` entry carrying the id (a `CredentialsCheckError` for a wrong
/// password, else an `ErrorDetail`). Until m2 the messages are the old error key, not
/// Zitadel's localized text, so callers that still match keys keep working.
///
/// A forced fault (`MockZitadel::fail`) whose key no capture names is the test's own stand-in
/// for a refusal no caller matches: it gets a gRPC code for its status and a `TEST-` id. A
/// refusal of the mock's own with no capture is a mock bug: the test process aborts, naming the
/// call and the key (a panic here would only drop the connection, which reads as Zitadel down).
fn zitadel_refusal(call: &str, status: StatusCode, key: &str, forced: bool) -> Response {
    use axum::response::IntoResponse;
    let (status, code, id) = match captured(call, key) {
        // A forced fault keeps the status the test chose.
        Some(r) if forced => (status, r.code, r.id.to_string()),
        Some(r) => (StatusCode::from_u16(r.status).unwrap(), r.code, r.id.to_string()),
        None if !forced => {
            eprintln!(
                "mock Zitadel: no captured refusal for `{call}` refused with `{key}`; capture it \
                 in tests/zitadel_live.rs and add it to refusals.rs and mock::captured"
            );
            std::process::abort();
        }
        None => {
            let code = match status.as_u16() {
                401 => refusals::UNAUTHENTICATED,
                403 => 7,
                404 => refusals::NOT_FOUND,
                409 => refusals::ALREADY_EXISTS,
                429 => 8,
                400 | 412 => refusals::FAILED_PRECONDITION,
                _ => 2,
            };
            (status, code, format!("TEST-{key}"))
        }
    };
    let detail = if id == refusals::PASSWORD_INVALID.id {
        "type.googleapis.com/zitadel.v1.CredentialsCheckError"
    } else {
        "type.googleapis.com/zitadel.v1.ErrorDetail"
    };
    let body = json!({
        "code": code,
        "message": format!("{key} ({id})"),
        "details": [{"@type": detail, "id": id, "message": key}],
    });
    (status, Json(body)).into_response()
}

impl MockZitadel {
    pub async fn start() -> Self {
        let inner = Arc::new(Mutex::new(Inner {
            orgs: vec!["o1".into()],
            ..Inner::default()
        }));
        let app = Router::new()
            .route(
                "/v2/oidc/auth_requests/{id}",
                get(get_auth_request).post(create_callback),
            )
            .route("/v2/organizations", post(create_org))
            .route("/v2/organizations/{id}", delete(delete_org))
            .route("/v2/users", post(list_users))
            .route("/v2/users/new", post(create_user))
            .route(
                "/v2/users/{id}",
                get(get_user).delete(delete_user).patch(update_user),
            )
            .route("/v2/users/{id}/authentication_methods", get(authentication_methods))
            .route("/v2/users/{id}/deactivate", post(deactivate))
            .route("/v2/users/{id}/reactivate", post(reactivate))
            .route("/v2/users/{id}/lock", post(lock))
            .route("/v2/users/{id}/unlock", post(unlock))
            .route("/v2/users/{id}/email", post(set_email))
            .route("/v2/users/{id}/email/verify", post(verify_email))
            .route("/v2/users/{id}/email/resend", post(resend))
            .route("/v2/users/{id}/invite_code", post(create_invite_code))
            .route("/admin/v1/email/_search", post(search_email_providers))
            .route("/v2/users/{id}/invite_code/verify", post(verify_invite_code))
            .route("/v2/users/{id}/password_reset", post(password_reset))
            .route("/v2/users/{id}/password", post(set_password))
            .route("/v2/sessions", post(create_session))
            .route("/v2/sessions/search", post(search_sessions))
            .route("/v2/sessions/{id}", delete(delete_session))
            .layer(middleware::from_fn_with_state(inner.clone(), journal))
            .with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base: format!("http://{addr}"),
            inner,
        }
    }

    fn id(inner: &mut Inner, prefix: &str) -> String {
        inner.next += 1;
        format!("{prefix}{}", inner.next)
    }

    pub fn add_user(&self, email: &str, password: &str, verified: bool) -> String {
        self.add_user_in("o1", email, password, verified)
    }
    pub fn add_user_in(&self, org: &str, email: &str, password: &str, verified: bool) -> String {
        let mut inner = self.inner.lock().unwrap();
        let id = Self::id(&mut inner, "3000");
        inner.users.push(User {
            id: id.clone(),
            email: email.into(),
            password: password.into(),
            verified,
            locked: false,
            org_id: org.into(),
            username: email.into(),
            display_name: String::new(),
            inactive: false,
            created_at: NOW.into(),
        });
        id
    }
    /// Sets a user's `human.profile.displayName`.
    pub fn set_display_name(&self, user_id: &str, name: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(u) = inner.users.iter_mut().find(|u| u.id == user_id) {
            u.display_name = name.into();
        }
    }
    /// The next `times` requests whose `"METHOD path"` contains `pattern` fail with this error.
    pub fn fail(&self, pattern: &str, status: u16, key: &str, times: u32) {
        self.inner.lock().unwrap().faults.push(Fault {
            pattern: pattern.into(),
            status,
            key: key.into(),
            body: None,
            remaining: times,
        });
    }
    /// The next `times` requests whose `"METHOD path"` contains `pattern` get `status` with
    /// exactly `body` (empty, malformed or unexpected JSON), as a proxy or a changed API might.
    pub fn answer_raw(&self, pattern: &str, status: u16, body: &str, times: u32) {
        self.inner.lock().unwrap().faults.push(Fault {
            pattern: pattern.into(),
            status,
            key: String::new(),
            body: Some(body.into()),
            remaining: times,
        });
    }
    /// Refuse the login client's token on admin-only calls (see `Inner::strict_login`).
    pub fn set_strict_login(&self, on: bool) {
        self.inner.lock().unwrap().strict_login = on;
    }
    /// A passwordless, unverified user with a pending invite code: `(user id, code)`.
    pub fn add_invited_user(&self, email: &str) -> (String, String) {
        let id = self.add_user_in("o1", email, "", false);
        let mut inner = self.inner.lock().unwrap();
        let code = format!("INV{}", inner.next);
        inner.invites.insert(id.clone(), (code.clone(), false));
        (id, code)
    }
    /// Puts the user in Zitadel's Initial state.
    pub fn set_initial(&self, user_id: &str) {
        self.inner.lock().unwrap().initial_users.push(user_id.into());
    }
    /// Pages `sessions/search` answers at `n` sessions.
    pub fn set_session_page(&self, n: usize) {
        self.inner.lock().unwrap().session_page = n;
    }
    /// Sets the email providers `POST /admin/v1/email/_search` lists, as (id, active).
    pub fn set_email_providers(&self, providers: &[(&str, bool)]) {
        self.inner.lock().unwrap().email_providers =
            providers.iter().map(|(id, active)| (id.to_string(), *active)).collect();
    }
    /// The user's pending invite code, if any.
    pub fn invite_code(&self, user_id: &str) -> Option<String> {
        self.inner.lock().unwrap().invites.get(user_id).map(|(c, _)| c.clone())
    }
    /// The URL template of the user's last invite mail.
    pub fn invite_template(&self, user_id: &str) -> Option<String> {
        self.inner.lock().unwrap().invite_mails.get(user_id).map(|(t, _)| t.clone())
    }
    /// The URL template of the user's last password-reset mail.
    pub fn reset_template(&self, user_id: &str) -> Option<String> {
        self.inner.lock().unwrap().reset_mails.get(user_id).cloned()
    }
    /// The application name of the user's last invite mail.
    pub fn invite_application(&self, user_id: &str) -> Option<String> {
        self.inner.lock().unwrap().invite_mails.get(user_id).map(|(_, a)| a.clone())
    }
    /// The invite's code is now past its lifetime.
    pub fn expire_invite(&self, user_id: &str) {
        if let Some(i) = self.inner.lock().unwrap().invites.get_mut(user_id) {
            i.1 = true;
        }
    }
    /// Deletes a user without a request (as revoking an invite does).
    pub fn remove_user(&self, user_id: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.users.retain(|u| u.id != user_id);
        inner.invites.remove(user_id);
    }
    /// Registers an organization without any request (the default `o1` exists from the start).
    pub fn add_org(&self, id: &str) {
        self.inner.lock().unwrap().orgs.push(id.into());
    }
    pub fn orgs(&self) -> Vec<String> {
        self.inner.lock().unwrap().orgs.clone()
    }
    /// Every request so far as `"METHOD path"`, so tests can assert something was never called.
    pub fn calls(&self) -> Vec<String> {
        self.inner.lock().unwrap().calls.clone()
    }
    pub fn user_by_id(&self, id: &str) -> Option<User> {
        self.inner
            .lock()
            .unwrap()
            .users
            .iter()
            .find(|u| u.id == id)
            .cloned()
    }
    pub fn lock(&self, user_id: &str) {
        self.inner
            .lock()
            .unwrap()
            .users
            .iter_mut()
            .filter(|u| u.id == user_id)
            .for_each(|u| u.locked = true);
    }
    /// Gives a user an authentication method (`AUTHENTICATION_METHOD_TYPE_PASSKEY`, `_TOTP`, ...).
    pub fn add_auth_method(&self, user_id: &str, method: &str) {
        self.inner.lock().unwrap().auth_methods.entry(user_id.into()).or_default().push(method.into());
    }
    /// Deactivates or reactivates a user without a request.
    pub fn set_inactive(&self, user_id: &str, inactive: bool) {
        for u in self.inner.lock().unwrap().users.iter_mut().filter(|u| u.id == user_id) {
            u.inactive = inactive;
        }
    }
    /// Gives a user a password without a request (as setting one outside an invite would).
    pub fn set_password(&self, user_id: &str, password: &str) {
        for u in self.inner.lock().unwrap().users.iter_mut().filter(|u| u.id == user_id) {
            u.password = password.into();
        }
    }
    pub fn add_auth_request(&self, sign_up: bool) -> String {
        let mut inner = self.inner.lock().unwrap();
        let id = Self::id(&mut inner, "V2_");
        inner.auth_requests.insert(id.clone(), sign_up);
        id
    }
    pub fn emails(&self) -> Vec<(String, String)> {
        self.inner.lock().unwrap().emails.clone()
    }
    /// Hides an existing user from session creation for this many attempts (Zitadel's
    /// projections catching up after a write).
    pub fn hide_user(&self, user_id: &str, attempts: u32) {
        self.inner.lock().unwrap().hidden.insert(user_id.into(), attempts);
    }
    pub fn set_new_user_lag(&self, attempts: u32) {
        self.inner.lock().unwrap().new_user_lag = attempts;
    }
    pub fn allow_duplicate_emails(&self) {
        self.inner.lock().unwrap().allow_duplicate_emails = true;
    }
    pub fn users_with_email(&self, email: &str) -> usize {
        self.inner
            .lock()
            .unwrap()
            .users
            .iter()
            .filter(|u| u.email.eq_ignore_ascii_case(email))
            .count()
    }
    pub fn set_down(&self, down: bool) {
        self.inner.lock().unwrap().down = down;
    }
    pub fn set_user_lookup_delay(&self, delay: std::time::Duration) {
        self.inner.lock().unwrap().user_lookup_delay = delay;
    }
    /// Runs `f` whenever a request matching `pattern` arrives, before it is answered.
    pub fn on_request(&self, pattern: &str, f: impl Fn() + Send + Sync + 'static) {
        self.inner
            .lock()
            .unwrap()
            .on_request
            .push((pattern.into(), Arc::new(f)));
    }
    pub fn clear_on_request(&self) {
        self.inner.lock().unwrap().on_request.clear();
    }
    pub fn set_session_delete_delay(&self, delay: std::time::Duration) {
        self.inner.lock().unwrap().session_delete_delay = delay;
    }
    pub fn session_count(&self) -> usize {
        self.inner.lock().unwrap().sessions.len()
    }
    pub fn user(&self, email: &str) -> Option<User> {
        self.inner
            .lock()
            .unwrap()
            .users
            .iter()
            .find(|u| u.email == email)
            .cloned()
    }
}

const NOW: &str = "2026-10-03T10:00:00Z";

type S = State<Arc<Mutex<Inner>>>;

async fn journal(State(s): S, req: Request, next: Next) -> Response {
    use axum::response::IntoResponse;
    let call = format!("{} {}", req.method(), req.uri().path());
    let hooks: Vec<_> = s
        .lock()
        .unwrap()
        .on_request
        .iter()
        .filter(|(pattern, _)| call.contains(pattern.as_str()))
        .map(|(_, f)| f.clone())
        .collect();
    for hook in hooks {
        hook();
    }
    let forced = {
        let mut inner = s.lock().unwrap();
        inner.calls.push(call.clone());
        let strict_denied = inner.strict_login
            && !is_admin(req.headers())
            && (call.ends_with("/invite_code/verify") || call.starts_with("PATCH /v2/users/"));
        if strict_denied {
            Some(zitadel_refusal(&call, StatusCode::FORBIDDEN, "Errors.PermissionDenied", false))
        } else {
            inner
                .faults
                .iter_mut()
                .find(|f| f.remaining > 0 && call.contains(&f.pattern))
                .map(|f| {
                    f.remaining -= 1;
                    let status = StatusCode::from_u16(f.status).unwrap();
                    match &f.body {
                        Some(body) => (status, body.clone()).into_response(),
                        None => zitadel_refusal(&call, status, &f.key, true),
                    }
                })
        }
    };
    match forced {
        Some(reply) => reply,
        None => shaped(&call, next.run(req).await).await,
    }
}

/// A handler's refusal (`err`) in Zitadel's shape; any other answer as it is.
async fn shaped(call: &str, reply: Response) -> Response {
    use axum::response::IntoResponse;
    let status = reply.status();
    if !status.is_client_error() {
        return reply;
    }
    let (parts, body) = reply.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap_or_default();
    match serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|v| v.get(MOCK_KEY)?.as_str().map(str::to_string))
    {
        Some(key) => zitadel_refusal(call, status, &key, false),
        None => (parts, bytes).into_response(),
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
}

fn is_admin(headers: &HeaderMap) -> bool {
    bearer(headers) == Some(ADMIN_TOKEN)
}

/// Zitadel v4.19.1's `IAM_LOGIN_CLIENT` role (cmd/defaults.yaml) has user.read/write,
/// user.credential.write, session.write/delete and org.read, but not user.delete,
/// org.create or org.delete: those need the admin token.
fn admin_only(headers: &HeaderMap) -> Option<Reply> {
    (!is_admin(headers)).then(|| err(StatusCode::FORBIDDEN, "Errors.PermissionDenied"))
}

fn guard(headers: &HeaderMap, inner: &Inner) -> Option<Reply> {
    if inner.down {
        return Some(err(StatusCode::SERVICE_UNAVAILABLE, "down"));
    }
    (!matches!(bearer(headers), Some(TOKEN | ADMIN_TOKEN)))
        .then(|| err(StatusCode::UNAUTHORIZED, "Errors.Token.Invalid"))
}

async fn get_auth_request(State(s): S, headers: HeaderMap, Path(id): Path<String>) -> Reply {
    let inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    match inner.auth_requests.get(&id) {
        Some(sign_up) => ok(
            json!({"authRequest": {"id": id, "prompt": if *sign_up { json!(["PROMPT_CREATE"]) } else { json!([]) }}}),
        ),
        None => err(StatusCode::NOT_FOUND, "Errors.AuthRequest.NotFound"),
    }
}

async fn create_callback(
    State(s): S,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    let sid = b
        .pointer("/session/sessionId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let tok = b
        .pointer("/session/sessionToken")
        .and_then(Value::as_str)
        .unwrap_or("");
    let valid = inner.sessions.get(&sid).is_some_and(|(_, t)| t == tok);
    if !inner.auth_requests.contains_key(&id) || !valid {
        return err(StatusCode::NOT_FOUND, "Errors.AuthRequest.NotFound");
    }
    inner.auth_requests.remove(&id);
    ok(json!({"callbackUrl": format!("http://studio.chelabase.localhost:8000/callback?code=code-{id}&state=s")}))
}

/// A user as Zitadel v4.19.1 answers it; `human.passwordChanged` only once a password is set.
fn user_json(u: &User, inner: &Inner) -> Value {
    let mut v = json!({"userId": u.id, "state": state_name(u, inner),
        "details": {"resourceOwner": u.org_id, "creationDate": u.created_at},
        "username": u.username,
        "human": {"email": {"email": u.email, "isVerified": u.verified},
                  "profile": {"displayName": u.display_name}}});
    if !u.password.is_empty() {
        v["human"]["passwordChanged"] = json!(NOW);
    }
    v
}

fn state_name(u: &User, inner: &Inner) -> &'static str {
    if inner.initial_users.contains(&u.id) {
        "USER_STATE_INITIAL"
    } else if u.locked {
        "USER_STATE_LOCKED"
    } else if u.inactive {
        "USER_STATE_INACTIVE"
    } else {
        "USER_STATE_ACTIVE"
    }
}

/// Zitadel v4.19.1's request validation of a password over 200 runes, as captured: a 400 with
/// gRPC code 3 and no `details` (see `refusals::PASSWORD_VALIDATION_MAX_RUNES`). The mock
/// doesn't copy the 500 that 73 bytes up to that limit get (`refusals::PASSWORD_MAX_BYTES`).
fn too_long(password: &str, field: &str) -> Option<Reply> {
    (password.chars().count() > refusals::PASSWORD_VALIDATION_MAX_RUNES).then(|| {
        let message = format!(
            "invalid {field}: embedded message failed validation | caused by: invalid \
             Password.Password: value length must be between 1 and 200 runes, inclusive"
        );
        (StatusCode::BAD_REQUEST, Json(json!({"code": refusals::INVALID_ARGUMENT, "message": message})))
    })
}

fn num(v: Option<&Value>) -> Option<usize> {
    v.and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()))
        .map(|n| n as usize)
}

async fn list_users(State(s): S, headers: HeaderMap, Json(b): Json<Value>) -> Reply {
    let inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    let queries = b
        .get("queries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let org = queries
        .iter()
        .find_map(|q| q.pointer("/organizationIdQuery/organizationId")?.as_str());
    let email = queries.iter().find_map(|q| q.get("emailQuery"));
    let matching: Vec<&User> = inner
        .users
        .iter()
        .filter(|u| org.is_none_or(|o| u.org_id == o))
        .filter(|u| match email {
            None => true,
            Some(q) => {
                let want = q
                    .get("emailAddress")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_lowercase();
                let have = u.email.to_lowercase();
                match q.get("method").and_then(Value::as_str) {
                    Some(m) if m.contains("CONTAINS") => have.contains(&want),
                    _ => have == want,
                }
            }
        })
        .collect();
    let offset = num(b.pointer("/query/offset")).unwrap_or(0);
    let limit = num(b.pointer("/query/limit")).unwrap_or(100);
    let result: Vec<Value> = matching
        .iter()
        .skip(offset)
        .take(limit)
        .map(|u| user_json(u, &inner))
        .collect();
    ok(json!({"details": {"totalResult": matching.len().to_string()}, "result": result}))
}

async fn get_user(State(s): S, headers: HeaderMap, Path(id): Path<String>) -> Reply {
    let delay = s.lock().unwrap().user_lookup_delay;
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    let inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    match inner.users.iter().find(|u| u.id == id) {
        Some(u) => ok(json!({"user": user_json(u, &inner)})),
        None => err(StatusCode::NOT_FOUND, "Errors.User.NotFound"),
    }
}

/// `GET /v2/users/{id}/authentication_methods` (Zitadel v4.19.1 ListAuthenticationMethodTypes):
/// `authMethodTypes`, the password (when the user has one) and any other method. As the real
/// one, a `domainQuery` (any `domainQuery.*` parameter) needs a `domainQuery.domain` of 1 to 200
/// characters; without one every method counts, whatever its domain.
async fn authentication_methods(
    State(s): S,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Reply {
    let inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    let domain = query.get("domainQuery.domain").map(|d| d.chars().count()).unwrap_or(0);
    if query.keys().any(|k| k.starts_with("domainQuery.")) && !(1..=200).contains(&domain) {
        // A request that fails validation: no error detail (captured).
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"code": 3, "message": "invalid ListAuthenticationMethodTypesRequest.\
                DomainQuery: embedded message failed validation | caused by: invalid \
                DomainQuery.Domain: value length must be between 1 and 200 runes, inclusive"})),
        );
    }
    let Some(u) = inner.users.iter().find(|u| u.id == id) else {
        return err(StatusCode::NOT_FOUND, "Errors.User.NotFound");
    };
    let mut types: Vec<String> = Vec::new();
    if !u.password.is_empty() {
        types.push("AUTHENTICATION_METHOD_TYPE_PASSWORD".into());
    }
    types.extend(inner.auth_methods.get(&id).cloned().unwrap_or_default());
    ok(json!({"details": {"totalResult": types.len().to_string()}, "authMethodTypes": types}))
}

async fn delete_user(State(s): S, headers: HeaderMap, Path(id): Path<String>) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    // Zitadel v4.19.1 answers a missing user's not-found even to a token without the
    // permission to delete.
    if !inner.users.iter().any(|u| u.id == id) {
        return err(StatusCode::NOT_FOUND, "Errors.User.NotFound");
    }
    if let Some(e) = admin_only(&headers) {
        return e;
    }
    inner.users.retain(|u| u.id != id);
    inner.sessions.retain(|_, (uid, _)| *uid != id);
    ok(json!({}))
}

async fn set_active(s: S, headers: HeaderMap, id: String, inactive: bool) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    // Zitadel v4.19.1: a precondition refusal (400) when the user is already in that state.
    // Deactivating a locked user succeeds, and it is then inactive, not locked. A user in the
    // Initial state can't be deactivated, and isn't inactive.
    let initial = inner.initial_users.contains(&id);
    match inner.users.iter_mut().find(|u| u.id == id) {
        Some(_) if inactive && initial => {
            err(StatusCode::BAD_REQUEST, "Errors.User.CannotDeactivateInitial")
        }
        Some(u) if inactive && u.inactive => {
            err(StatusCode::BAD_REQUEST, "Errors.User.AlreadyInactive")
        }
        Some(u) if !inactive && !u.inactive => {
            err(StatusCode::BAD_REQUEST, "Errors.User.NotInactive")
        }
        Some(u) => {
            u.inactive = inactive;
            u.locked = false;
            ok(json!({}))
        }
        None => err(StatusCode::NOT_FOUND, "Errors.User.NotFound"),
    }
}

async fn deactivate(s: S, headers: HeaderMap, Path(id): Path<String>) -> Reply {
    set_active(s, headers, id, true).await
}

async fn reactivate(s: S, headers: HeaderMap, Path(id): Path<String>) -> Reply {
    set_active(s, headers, id, false).await
}

/// `POST /v2/users/{id}/lock` and `/unlock`: as `set_active`, a precondition refusal (400) when
/// the user is already in that state, 404 for an unknown user. As in Zitadel v4.19.1, only an
/// active (or initial) user can be locked: an inactive one gets the same refusal as a locked
/// one. Unlocking a user locked from the Initial state makes it active.
async fn set_locked(s: S, headers: HeaderMap, id: String, locked: bool) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    let initial = inner.initial_users.contains(&id);
    match inner.users.iter_mut().find(|u| u.id == id) {
        Some(u) if locked && u.locked => err(StatusCode::BAD_REQUEST, "Errors.User.AlreadyLocked"),
        Some(u) if locked && u.inactive => err(StatusCode::BAD_REQUEST, "Errors.User.NotActive"),
        Some(u) if !locked && !u.locked => err(StatusCode::BAD_REQUEST, "Errors.User.NotLocked"),
        Some(u) => {
            u.locked = locked;
            if initial && locked {
                inner.initial_users.retain(|i| *i != id);
            }
            ok(json!({}))
        }
        None => err(StatusCode::NOT_FOUND, "Errors.User.NotFound"),
    }
}

async fn lock(s: S, headers: HeaderMap, Path(id): Path<String>) -> Reply {
    set_locked(s, headers, id, true).await
}

async fn unlock(s: S, headers: HeaderMap, Path(id): Path<String>) -> Reply {
    set_locked(s, headers, id, false).await
}

async fn set_email(
    State(s): S,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    let email = b
        .get("email")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let verified = b
        .get("isVerified")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let send = b.get("sendCode").is_some();
    let Some(u) = inner.users.iter_mut().find(|u| u.id == id) else {
        return err(StatusCode::NOT_FOUND, "Errors.User.NotFound");
    };
    // Zitadel v4.19.1 (`UserEmailEvents.Change`): the same address is a precondition refusal.
    if u.email == email {
        return err(
            StatusCode::BAD_REQUEST,
            "Errors.User.Email.NotChanged (COMMAND-Uch5e)",
        );
    }
    u.email = email;
    u.verified = verified;
    if send {
        inner.emails.push((id, "verify".into()));
    }
    ok(json!({}))
}

/// `PATCH /v2/users/{id}` (UpdateUser), the human email part only, like Zitadel v4.19.1's
/// `changeUserEmail`: a new address is changed (verified, or with a code sent); the same
/// address with `isVerified` only adds the verified flag; anything else changes nothing.
async fn update_user(
    State(s): S,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    let email = b.pointer("/human/email/email").and_then(Value::as_str);
    let verified = b
        .pointer("/human/email/isVerified")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let Some(u) = inner.users.iter_mut().find(|u| u.id == id) else {
        return err(StatusCode::NOT_FOUND, "Errors.User.NotFound");
    };
    let mut send = false;
    if let Some(email) = email.filter(|e| !e.is_empty()) {
        if email != u.email {
            u.email = email.to_string();
            u.verified = verified;
            send = !verified;
        } else if verified {
            u.verified = true;
        }
    }
    if send {
        inner.emails.push((id, "verify".into()));
    }
    ok(json!({"changeDate": NOW}))
}

async fn create_org(State(s): S, headers: HeaderMap, Json(b): Json<Value>) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    if let Some(e) = admin_only(&headers) {
        return e;
    }
    let id = match b.get("organizationId").and_then(Value::as_str) {
        Some(id) => id.to_string(),
        None => MockZitadel::id(&mut inner, "4000"),
    };
    if inner.orgs.contains(&id) {
        return err(StatusCode::CONFLICT, "Errors.Org.AlreadyExists");
    }
    inner.orgs.push(id.clone());
    ok(json!({"organizationId": id}))
}

async fn delete_org(State(s): S, headers: HeaderMap, Path(id): Path<String>) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    if let Some(e) = admin_only(&headers) {
        return e;
    }
    // Zitadel v4.19.1 answers 200 for an org deleted before or never created.
    inner.orgs.retain(|o| *o != id);
    inner.users.retain(|u| u.org_id != id);
    let users: Vec<String> = inner.users.iter().map(|u| u.id.clone()).collect();
    inner.sessions.retain(|_, (uid, _)| users.contains(uid));
    ok(json!({}))
}

async fn create_user(State(s): S, headers: HeaderMap, Json(b): Json<Value>) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    let email = b
        .pointer("/human/email/email")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let password = b
        .pointer("/human/password/password")
        .and_then(Value::as_str)
        .map(str::to_string);
    let org_id = b
        .get("organizationId")
        .and_then(Value::as_str)
        .unwrap_or("o1")
        .to_string();
    let wanted_id = b.get("userId").and_then(Value::as_str).map(str::to_string);
    let username = b
        .get("username")
        .and_then(Value::as_str)
        .unwrap_or(&email)
        .to_string();
    if let Some(e) = password.as_deref().and_then(|p| too_long(p, "CreateUserRequest_Human.Password")) {
        return e;
    }
    if !inner.orgs.contains(&org_id) {
        return err(StatusCode::NOT_FOUND, "Errors.Org.NotFound");
    }
    // Ids and usernames are unique across the whole instance; the mock also refuses a
    // repeated email within an organization.
    let taken = inner.users.iter().any(|u| {
        wanted_id.as_deref() == Some(&u.id)
            || u.username.eq_ignore_ascii_case(&username)
            || (u.org_id == org_id && u.email.eq_ignore_ascii_case(&email))
    });
    if taken && !inner.allow_duplicate_emails {
        return err(StatusCode::CONFLICT, "Errors.User.AlreadyExists");
    }
    if password.as_ref().is_some_and(|p| p.len() < 8) {
        return err(
            StatusCode::BAD_REQUEST,
            "Errors.User.PasswordComplexityPolicy.MinLength",
        );
    }
    let verified = b
        .pointer("/human/email/isVerified")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let display_name = b
        .pointer("/human/profile/displayName")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let id = match wanted_id {
        Some(id) => id,
        None => MockZitadel::id(&mut inner, "3000"),
    };
    let lag = inner.new_user_lag;
    inner.hidden.insert(id.clone(), lag);
    if b.pointer("/human/email/sendCode").is_some() {
        inner.emails.push((id.clone(), "verify".into()));
    }
    inner.users.push(User {
        id: id.clone(),
        email,
        password: password.unwrap_or_default(),
        verified,
        locked: false,
        org_id,
        username,
        display_name,
        inactive: false,
        created_at: NOW.into(),
    });
    ok(json!({"id": id}))
}

async fn create_session(State(s): S, headers: HeaderMap, Json(b): Json<Value>) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    let uid = b
        .pointer("/checks/user/userId")
        .and_then(Value::as_str)
        .unwrap_or("");
    let pw = b
        .pointer("/checks/password/password")
        .and_then(Value::as_str)
        .unwrap_or("");
    if let Some(left) = inner.hidden.get_mut(uid).filter(|n| **n > 0) {
        *left -= 1;
        return err(StatusCode::NOT_FOUND, "Errors.User.NotFound");
    }
    let Some(user) = inner.users.iter().find(|u| u.id == uid).cloned() else {
        return err(StatusCode::NOT_FOUND, "Errors.User.NotFound");
    };
    if user.locked {
        return err(StatusCode::PRECONDITION_FAILED, "Errors.User.Locked");
    }
    if user.password.is_empty() || user.password != pw {
        return err(StatusCode::BAD_REQUEST, "Errors.User.Password.Invalid");
    }
    let sid = MockZitadel::id(&mut inner, "s");
    let tok = format!("tok-{sid}");
    inner
        .sessions
        .insert(sid.clone(), (uid.to_string(), tok.clone()));
    ok(json!({"sessionId": sid, "sessionToken": tok}))
}

async fn delete_session(
    State(s): S,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Reply {
    let delay = s.lock().unwrap().session_delete_delay;
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    // Zitadel v4.19.1 answers 200 for a session ended before or never created.
    inner.sessions.remove(&id);
    ok(json!({}))
}

async fn verify_email(
    State(s): S,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    if b.get("verificationCode").and_then(Value::as_str) != Some(EMAIL_CODE) {
        return err(StatusCode::BAD_REQUEST, "Errors.User.Code.Invalid");
    }
    inner
        .users
        .iter_mut()
        .filter(|u| u.id == id)
        .for_each(|u| u.verified = true);
    ok(json!({}))
}

/// `POST /v2/users/{id}/invite_code` (Zitadel v4.19.1 CreateInviteCode): a fresh code replaces
/// any pending one. `sendCode` mails it (journaled as an `invite` email with its template and
/// application name); `returnCode` returns it as `inviteCode`. As the real one, a user who
/// already has a way to sign in (a password, or another method) gets none: 400 "User is already
/// initialized".
async fn create_invite_code(
    State(s): S,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    let Some(user) = inner.users.iter().find(|u| u.id == id) else {
        return err(StatusCode::NOT_FOUND, "Errors.User.NotFound");
    };
    if !user.password.is_empty() || inner.auth_methods.get(&id).is_some_and(|m| !m.is_empty()) {
        return err(StatusCode::BAD_REQUEST, "Errors.User.AlreadyInitialised");
    }
    inner.next += 1;
    let code = format!("INV{}", inner.next);
    inner.invites.insert(id.clone(), (code.clone(), false));
    if b.get("returnCode").is_some() {
        return ok(json!({"details": {}, "inviteCode": code}));
    }
    let text = |key: &str| {
        b.pointer(&format!("/sendCode/{key}"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let mail = (text("urlTemplate"), text("applicationName"));
    inner.invite_mails.insert(id.clone(), mail);
    inner.emails.push((id, "invite".into()));
    ok(json!({"details": {}}))
}

/// `POST /admin/v1/email/_search` (Zitadel v4.19.1 ListEmailProviders; admin only): each
/// provider with its `state`, `EMAIL_PROVIDER_ACTIVE` or `EMAIL_PROVIDER_INACTIVE`.
async fn search_email_providers(State(s): S, headers: HeaderMap, Json(_): Json<Value>) -> Reply {
    let inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    if let Some(e) = admin_only(&headers) {
        return e;
    }
    let result: Vec<Value> = inner
        .email_providers
        .iter()
        .map(|(id, active)| {
            json!({
                "id": id,
                "state": if *active { "EMAIL_PROVIDER_ACTIVE" } else { "EMAIL_PROVIDER_INACTIVE" },
                "smtp": {"host": "smtp.example.com:587"},
            })
        })
        .collect();
    ok(json!({"details": {"totalResult": result.len().to_string()}, "result": result}))
}

/// `POST /v2/sessions/search`: the sessions of the `userIdQuery`'s user (all without one).
async fn search_sessions(State(s): S, headers: HeaderMap, Json(b): Json<Value>) -> Reply {
    let inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    let user = b
        .get("queries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|q| q.pointer("/userIdQuery/id")?.as_str());
    let mut ids: Vec<&String> = inner
        .sessions
        .iter()
        .filter(|(_, (uid, _))| user.is_none_or(|u| u == uid))
        .map(|(sid, _)| sid)
        .collect();
    ids.sort();
    let total = ids.len();
    let limit = num(b.pointer("/query/limit")).unwrap_or(100);
    let page = if inner.session_page > 0 { limit.min(inner.session_page) } else { limit };
    let sessions: Vec<Value> = ids.iter().take(page).map(|id| json!({"id": id})).collect();
    ok(json!({"details": {"totalResult": total.to_string()}, "sessions": sessions}))
}

/// `POST /v2/users/{id}/invite_code/verify` with `{"verificationCode": …}`: a right code is
/// consumed.
async fn verify_invite_code(
    State(s): S,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    if !inner.users.iter().any(|u| u.id == id) {
        return err(StatusCode::NOT_FOUND, "Errors.User.NotFound");
    }
    let Some((code, expired)) = inner.invites.get(&id).cloned() else {
        return err(StatusCode::BAD_REQUEST, "Errors.User.Code.NotFound");
    };
    if b.get("verificationCode").and_then(Value::as_str) != Some(code.as_str()) {
        return err(StatusCode::BAD_REQUEST, "Errors.User.Code.Invalid");
    }
    if expired {
        return err(StatusCode::PRECONDITION_FAILED, "Errors.User.Code.Expired");
    }
    inner.invites.remove(&id);
    ok(json!({"details": {}}))
}

async fn resend(State(s): S, headers: HeaderMap, Path(id): Path<String>) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    inner.emails.push((id, "verify".into()));
    ok(json!({}))
}

/// `POST /v2/users/{id}/password_reset`: the code is always `RESET_CODE`; `sendLink`'s
/// `urlTemplate` is journaled with the `reset` email. A deactivated user gets one too.
async fn password_reset(
    State(s): S,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Option<Json<Value>>,
) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    if let Some(t) = body.as_ref().and_then(|b| b.pointer("/sendLink/urlTemplate")).and_then(Value::as_str) {
        inner.reset_mails.insert(id.clone(), t.to_string());
    }
    inner.emails.push((id, "reset".into()));
    ok(json!({}))
}

async fn set_password(
    State(s): S,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(b): Json<Value>,
) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    let new = b.pointer("/newPassword/password").and_then(Value::as_str).unwrap_or("");
    if let Some(e) = too_long(new, "SetPasswordRequest.NewPassword") {
        return e;
    }
    let current = b.get("currentPassword").and_then(Value::as_str);
    let code = b.get("verificationCode").and_then(Value::as_str);
    match (current, code) {
        (Some(c), _) => {
            let right = inner.users.iter().any(|u| u.id == id && u.password == c);
            if !right {
                return err(StatusCode::BAD_REQUEST, "Errors.User.Password.Invalid");
            }
        }
        (None, Some(c)) if c != RESET_CODE => {
            return err(StatusCode::BAD_REQUEST, "Errors.User.Code.Invalid");
        }
        (None, Some(_)) => {}
        // Neither: an administrator sets it.
        // (the login client holds user.credential.write, so either token may)
        (None, None) => {
            if inner.strict_login && !is_admin(&headers) {
                return err(StatusCode::FORBIDDEN, "Errors.PermissionDenied");
            }
        }
    }
    let pw = b
        .pointer("/newPassword/password")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if pw.len() < 8 {
        return err(
            StatusCode::BAD_REQUEST,
            "Errors.User.PasswordComplexityPolicy.MinLength",
        );
    }
    inner
        .users
        .iter_mut()
        .filter(|u| u.id == id)
        .for_each(|u| u.password = pw.clone());
    ok(json!({}))
}
