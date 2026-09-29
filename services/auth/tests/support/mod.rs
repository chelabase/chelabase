#![allow(dead_code)]
//! A stateful stand-in for Zitadel v4's Session, User and OIDC v2 REST APIs.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub const TOKEN: &str = "test-login-client";
pub const EMAIL_CODE: &str = "123456";
pub const RESET_CODE: &str = "654321";

#[derive(Clone, Debug)]
pub struct User {
    pub id: String,
    pub email: String,
    pub password: String,
    pub verified: bool,
    pub locked: bool,
}

#[derive(Default)]
pub struct Inner {
    pub users: Vec<User>,
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
        let inner = Arc::new(Mutex::new(Inner::default()));
        let app = Router::new()
            .route(
                "/v2/oidc/auth_requests/{id}",
                get(get_auth_request).post(create_callback),
            )
            .route("/v2/users", post(list_users))
            .route("/v2/users/new", post(create_user))
            .route("/v2/users/{id}", get(get_user))
            .route("/v2/users/{id}/email/verify", post(verify_email))
            .route("/v2/users/{id}/email/resend", post(resend))
            .route("/v2/users/{id}/password_reset", post(password_reset))
            .route("/v2/users/{id}/password", post(set_password))
            .route("/v2/sessions", post(create_session))
            .route("/v2/sessions/{id}", delete(delete_session))
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
        let mut inner = self.inner.lock().unwrap();
        let id = Self::id(&mut inner, "3000");
        inner.users.push(User {
            id: id.clone(),
            email: email.into(),
            password: password.into(),
            verified,
            locked: false,
        });
        id
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

type S = State<Arc<Mutex<Inner>>>;

fn guard(headers: &HeaderMap, inner: &Inner) -> Option<Reply> {
    if inner.down {
        return Some(err(StatusCode::SERVICE_UNAVAILABLE, "down"));
    }
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok());
    (auth != Some(&format!("Bearer {TOKEN}"))).then(|| err(StatusCode::UNAUTHORIZED, "no token"))
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

async fn list_users(State(s): S, headers: HeaderMap, Json(b): Json<Value>) -> Reply {
    let inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    let email = b
        .pointer("/queries/0/emailQuery/emailAddress")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase();
    let result: Vec<Value> = inner
        .users
        .iter()
        .filter(|u| u.email.to_lowercase() == email)
        .map(|u| json!({"userId": u.id}))
        .collect();
    ok(json!({"result": result}))
}

async fn get_user(State(s): S, headers: HeaderMap, Path(id): Path<String>) -> Reply {
    let inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
    match inner.users.iter().find(|u| u.id == id) {
        Some(u) => ok(
            json!({"user": {"userId": u.id, "human": {"email": {"email": u.email, "isVerified": u.verified}}}}),
        ),
        None => err(StatusCode::NOT_FOUND, "Errors.User.NotFound"),
    }
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
        .unwrap_or("")
        .to_string();
    if inner
        .users
        .iter()
        .any(|u| u.email.eq_ignore_ascii_case(&email))
        && !inner.allow_duplicate_emails
    {
        return err(StatusCode::CONFLICT, "Errors.User.AlreadyExists");
    }
    if password.len() < 8 {
        return err(
            StatusCode::BAD_REQUEST,
            "Errors.User.PasswordComplexityPolicy.MinLength",
        );
    }
    let verified = b
        .pointer("/human/email/isVerified")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let id = MockZitadel::id(&mut inner, "3000");
    let lag = inner.new_user_lag;
    inner.hidden.insert(id.clone(), lag);
    if b.pointer("/human/email/sendCode").is_some() {
        inner.emails.push((id.clone(), "verify".into()));
    }
    inner.users.push(User {
        id: id.clone(),
        email,
        password,
        verified,
        locked: false,
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
    if user.password != pw {
        return err(StatusCode::BAD_REQUEST, "Errors.User.Password.Invalid");
    }
    let sid = MockZitadel::id(&mut inner, "s");
    let tok = format!("tok-{sid}");
    inner
        .sessions
        .insert(sid.clone(), (uid.to_string(), tok.clone()));
    ok(json!({"sessionId": sid, "sessionToken": tok}))
}

async fn delete_session(State(s): S, headers: HeaderMap, Path(id): Path<String>) -> Reply {
    let mut inner = s.lock().unwrap();
    if let Some(e) = guard(&headers, &inner) {
        return e;
    }
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
    if b.get("verificationCode").and_then(Value::as_str) != Some(RESET_CODE) {
        return err(StatusCode::BAD_REQUEST, "Errors.User.Code.Invalid");
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
