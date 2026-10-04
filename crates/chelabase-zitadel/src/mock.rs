#![allow(dead_code)]
//! A stateful stand-in for Zitadel v4's Session, User and OIDC v2 REST APIs.

use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
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
fn err(status: StatusCode, message: &str) -> Reply {
    (
        status,
        Json(json!({"code": status.as_u16(), "message": message})),
    )
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
            .route("/v2/users/{id}/deactivate", post(deactivate))
            .route("/v2/users/{id}/reactivate", post(reactivate))
            .route("/v2/users/{id}/email", post(set_email))
            .route("/v2/users/{id}/email/verify", post(verify_email))
            .route("/v2/users/{id}/email/resend", post(resend))
            .route("/v2/users/{id}/password_reset", post(password_reset))
            .route("/v2/users/{id}/password", post(set_password))
            .route("/v2/sessions", post(create_session))
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
            inactive: false,
            created_at: NOW.into(),
        });
        id
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
    pub fn add_auth_request(&self, sign_up: bool) -> String {
        let mut inner = self.inner.lock().unwrap();
        let id = Self::id(&mut inner, "V2_");
        inner.auth_requests.insert(id.clone(), sign_up);
        id
    }
    pub fn emails(&self) -> Vec<(String, String)> {
        self.inner.lock().unwrap().emails.clone()
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
    s.lock()
        .unwrap()
        .calls
        .push(format!("{} {}", req.method(), req.uri().path()));
    next.run(req).await
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
        .then(|| err(StatusCode::UNAUTHORIZED, "no token"))
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
    ok(json!({"callbackUrl": format!("http://localhost:3000/callback?code=code-{id}&state=s")}))
}

fn user_json(u: &User) -> Value {
    json!({"userId": u.id, "state": state_name(u),
        "details": {"resourceOwner": u.org_id, "creationDate": u.created_at},
        "username": u.username,
        "human": {"email": {"email": u.email, "isVerified": u.verified}}})
}

fn state_name(u: &User) -> &'static str {
    if u.locked {
        "USER_STATE_LOCKED"
    } else if u.inactive {
        "USER_STATE_INACTIVE"
    } else {
        "USER_STATE_ACTIVE"
    }
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
        .map(|u| user_json(u))
        .collect();
    ok(json!({"details": {"totalResult": matching.len().to_string()}, "result": result}))
}

async fn get_user(State(s): S, headers: HeaderMap, Path(id): Path<String>) -> Reply {
    let inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    match inner.users.iter().find(|u| u.id == id) {
        Some(u) => ok(json!({"user": user_json(u)})),
        None => err(StatusCode::NOT_FOUND, "Errors.User.NotFound"),
    }
}

async fn delete_user(State(s): S, headers: HeaderMap, Path(id): Path<String>) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    if let Some(e) = admin_only(&headers) {
        return e;
    }
    let before = inner.users.len();
    inner.users.retain(|u| u.id != id);
    if inner.users.len() == before {
        return err(StatusCode::NOT_FOUND, "Errors.User.NotFound");
    }
    inner.sessions.retain(|_, (uid, _)| *uid != id);
    ok(json!({}))
}

async fn set_active(s: S, headers: HeaderMap, id: String, inactive: bool) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    // Zitadel v4.19.1: a precondition refusal (400) when the user is already in that state.
    match inner.users.iter_mut().find(|u| u.id == id) {
        Some(u) if inactive && u.inactive => {
            err(StatusCode::BAD_REQUEST, "Errors.User.AlreadyInactive")
        }
        Some(u) if !inactive && !u.inactive => {
            err(StatusCode::BAD_REQUEST, "Errors.User.NotInactive")
        }
        Some(u) => {
            u.inactive = inactive;
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
    let before = inner.orgs.len();
    inner.orgs.retain(|o| *o != id);
    if inner.orgs.len() == before {
        return err(StatusCode::NOT_FOUND, "Errors.Org.NotFound");
    }
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
        return err(
            StatusCode::NOT_FOUND,
            "User could not be found (QUERY-Dfbg2)",
        );
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
    if inner.sessions.remove(&id).is_none() {
        return err(StatusCode::NOT_FOUND, "Errors.Session.NotFound");
    }
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

async fn resend(State(s): S, headers: HeaderMap, Path(id): Path<String>) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    inner.emails.push((id, "verify".into()));
    ok(json!({}))
}

async fn password_reset(State(s): S, headers: HeaderMap, Path(id): Path<String>) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
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
        (None, None) => {}
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
