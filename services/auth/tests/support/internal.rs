#![allow(dead_code)]
//! A stand-in for the orchestrator's internal app-auth routes (`/internal/app/*`), with a
//! journal of every call and in-memory sessions.

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

pub const INTERNAL_TOKEN: &str = "chelabase-dev-app-auth";

#[derive(Clone, Debug)]
pub struct FakeSession {
    pub project_id: Uuid,
    pub user_id: Uuid,
    pub zitadel_session_id: String,
    pub token: String,
    pub revoked: bool,
}

#[derive(Default)]
pub struct FakeState {
    /// Branch id → the `AppBranchInfo` JSON to answer.
    pub branches: HashMap<Uuid, Value>,
    /// `(method path, body)` of every call, in order.
    pub calls: Vec<(String, Value)>,
    pub sessions: HashMap<Uuid, FakeSession>,
    /// Answer refreshes with 401 and this code.
    pub refresh_error: Option<String>,
    pub next: u64,
    /// The sign-up gate's answer (`allowed`, `reason`); open when unset.
    pub signup: Option<(bool, String)>,
    /// Answer the gate with this status instead.
    pub signup_status: Option<u16>,
    /// A fresh install with this setup code: the gate answers `first_member` with
    /// `CLAIM_NONCE` for exactly this code, else `setup_code_invalid` (`setup_required`).
    pub setup_code: Option<String>,
    /// Answer `claimed` with this status instead.
    pub claimed_status: Option<u16>,
    /// `claimed`'s `bound` (`true` when unset).
    pub claimed_unbound: bool,
    /// `(method path, query)` of every call.
    pub queries: Vec<(String, String)>,
    /// `(user id, token hash hex)` → the invite description JSON to answer.
    pub invites: HashMap<(String, String), Value>,
    /// Answer the invite description with this status instead.
    pub describe_status: Option<u16>,
}

#[derive(Clone)]
pub struct FakeInternal {
    pub base: String,
    pub inner: Arc<Mutex<FakeState>>,
}

type S = State<Arc<Mutex<FakeState>>>;

fn error(status: StatusCode, code: &str) -> Response {
    (
        status,
        Json(json!({"error": {"code": code, "message": code}})),
    )
        .into_response()
}

async fn journal(State(s): S, req: Request, next: Next) -> Response {
    let ok = req
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        == Some(&format!("Bearer {INTERNAL_TOKEN}"));
    if !ok {
        return error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, 1 << 20).await.unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    {
        let mut st = s.lock().unwrap();
        let call = format!("{} {}", parts.method, parts.uri.path());
        st.queries
            .push((call.clone(), parts.uri.query().unwrap_or("").to_string()));
        st.calls.push((call, value));
    }
    next.run(Request::from_parts(parts, axum::body::Body::from(bytes)))
        .await
}

fn new_token(st: &mut FakeState, sid: Uuid) -> String {
    st.next += 1;
    format!("{sid}.secret{}", st.next)
}

async fn branch(State(s): S, Path(id): Path<Uuid>) -> Response {
    match s.lock().unwrap().branches.get(&id) {
        Some(v) => Json(v.clone()).into_response(),
        None => error(StatusCode::NOT_FOUND, "app_users_disabled"),
    }
}

async fn create_session(State(s): S, Json(b): Json<Value>) -> Response {
    let mut st = s.lock().unwrap();
    let sid = Uuid::new_v4();
    let token = new_token(&mut st, sid);
    st.sessions.insert(
        sid,
        FakeSession {
            project_id: b["project_id"].as_str().unwrap().parse().unwrap(),
            user_id: b["user_id"].as_str().unwrap().parse().unwrap(),
            zitadel_session_id: b["zitadel_session_id"].as_str().unwrap().into(),
            token: token.clone(),
            revoked: false,
        },
    );
    (
        StatusCode::CREATED,
        Json(json!({"session_id": sid, "refresh_token": token, "expires_at": "2026-11-02T10:00:00Z"})),
    )
        .into_response()
}

async fn refresh(State(s): S, Json(b): Json<Value>) -> Response {
    let mut st = s.lock().unwrap();
    if let Some(code) = st.refresh_error.clone() {
        return error(StatusCode::UNAUTHORIZED, &code);
    }
    let project: Uuid = b["project_id"].as_str().unwrap().parse().unwrap();
    let token = b["refresh_token"].as_str().unwrap_or("");
    let found = st
        .sessions
        .iter()
        .find(|(_, x)| x.token == token && x.project_id == project && !x.revoked)
        .map(|(id, x)| (*id, x.user_id));
    let Some((sid, user)) = found else {
        return error(StatusCode::UNAUTHORIZED, "refresh_token_invalid");
    };
    let next = new_token(&mut st, sid);
    st.sessions.get_mut(&sid).unwrap().token = next.clone();
    Json(
        json!({"session_id": sid, "user_id": user, "refresh_token": next,
        "expires_at": "2026-11-02T10:00:00Z"}),
    )
    .into_response()
}

async fn revoke(State(s): S, Path(id): Path<Uuid>, Json(b): Json<Value>) -> StatusCode {
    let project: Uuid = b["project_id"].as_str().unwrap().parse().unwrap();
    if let Some(x) = s.lock().unwrap().sessions.get_mut(&id)
        && x.project_id == project
    {
        x.revoked = true;
    }
    StatusCode::NO_CONTENT
}

async fn revoke_by_token(State(s): S, Json(b): Json<Value>) -> StatusCode {
    let project: Uuid = b["project_id"].as_str().unwrap().parse().unwrap();
    let token = b["refresh_token"].as_str().unwrap_or("");
    for x in s.lock().unwrap().sessions.values_mut() {
        if x.token == token && x.project_id == project {
            x.revoked = true;
        }
    }
    StatusCode::NO_CONTENT
}

async fn revoke_user(State(s): S, Path(user): Path<Uuid>, Json(b): Json<Value>) -> StatusCode {
    let project: Uuid = b["project_id"].as_str().unwrap().parse().unwrap();
    let keep: Option<Uuid> = b["keep_session_id"].as_str().map(|v| v.parse().unwrap());
    for (id, x) in s.lock().unwrap().sessions.iter_mut() {
        if x.user_id == user && x.project_id == project && Some(*id) != keep {
            x.revoked = true;
        }
    }
    StatusCode::NO_CONTENT
}

/// The nonce the fake gate hands out with `first_member`.
pub const CLAIM_NONCE: &str = "abababababababababababababababababababababababababababababababab";

async fn signup_check(State(s): S, Json(b): Json<Value>) -> Response {
    let st = s.lock().unwrap();
    if let Some(status) = st.signup_status {
        return error(StatusCode::from_u16(status).unwrap(), "unavailable");
    }
    if let Some(code) = &st.setup_code {
        return if b["setup_code"].as_str() == Some(code.as_str()) {
            Json(json!({"allowed": true, "reason": "first_member", "setup_required": true, "claim_nonce": CLAIM_NONCE}))
                .into_response()
        } else {
            Json(json!({"allowed": false, "reason": "setup_code_invalid", "setup_required": true})).into_response()
        };
    }
    let (allowed, reason) = st.signup.clone().unwrap_or((true, "open".into()));
    let mut answer = json!({"allowed": allowed, "reason": reason, "setup_required": false});
    if reason == "first_member" {
        answer["claim_nonce"] = json!(CLAIM_NONCE);
    }
    Json(answer).into_response()
}

async fn signup_claimed(State(s): S) -> Response {
    let st = s.lock().unwrap();
    if let Some(status) = st.claimed_status {
        let code = if status == 403 { "setup_code_invalid" } else { "unavailable" };
        return error(StatusCode::from_u16(status).unwrap(), code);
    }
    Json(json!({"bound": !st.claimed_unbound})).into_response()
}

async fn describe_invite(State(s): S, Json(q): Json<HashMap<String, String>>) -> Response {
    let st = s.lock().unwrap();
    if let Some(status) = st.describe_status {
        return error(StatusCode::from_u16(status).unwrap(), "unavailable");
    }
    let key = (
        q.get("user").cloned().unwrap_or_default(),
        q.get("token_hash").cloned().unwrap_or_default(),
    );
    match st.invites.get(&key) {
        Some(v) => Json(v.clone()).into_response(),
        None => error(StatusCode::NOT_FOUND, "not_found"),
    }
}

async fn sync() -> StatusCode {
    StatusCode::ACCEPTED
}

impl FakeInternal {
    pub async fn start() -> Self {
        let inner = Arc::new(Mutex::new(FakeState::default()));
        let app = Router::new()
            .route("/internal/app/branches/{id}", get(branch))
            .route("/internal/app/sessions", post(create_session))
            .route("/internal/app/sessions/refresh", post(refresh))
            .route(
                "/internal/app/sessions/revoke-by-token",
                post(revoke_by_token),
            )
            .route("/internal/app/sessions/{id}/revoke", post(revoke))
            .route(
                "/internal/app/users/{id}/revoke-sessions",
                post(revoke_user),
            )
            .route("/internal/app/sync", post(sync))
            .route("/internal/signup/check", post(signup_check))
            .route("/internal/signup/claimed", post(signup_claimed))
            .route("/internal/invites/describe", post(describe_invite))
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

    pub fn set_signup(&self, allowed: bool, reason: &str) {
        self.inner.lock().unwrap().signup = Some((allowed, reason.into()));
    }

    /// An open invite for this user, whose link token hashes to `token_hash` (hex).
    pub fn set_invite(&self, user: &str, token_hash: &str, description: Value) {
        self.inner
            .lock()
            .unwrap()
            .invites
            .insert((user.into(), token_hash.into()), description);
    }

    pub fn set_describe_status(&self, status: Option<u16>) {
        self.inner.lock().unwrap().describe_status = status;
    }

    pub fn set_setup_code(&self, code: &str) {
        self.inner.lock().unwrap().setup_code = Some(code.into());
    }

    pub fn set_claimed_status(&self, status: Option<u16>) {
        self.inner.lock().unwrap().claimed_status = status;
    }

    pub fn set_claimed_unbound(&self) {
        self.inner.lock().unwrap().claimed_unbound = true;
    }

    pub fn set_signup_status(&self, status: Option<u16>) {
        self.inner.lock().unwrap().signup_status = status;
    }

    /// The query strings of calls to `call` (`"POST /internal/invites/describe"`).
    pub fn queries(&self, call: &str) -> Vec<String> {
        self.inner
            .lock()
            .unwrap()
            .queries
            .iter()
            .filter(|(c, _)| c == call)
            .map(|(_, q)| q.clone())
            .collect()
    }

    pub fn set_branch(&self, branch: Uuid, info: Value) {
        self.inner.lock().unwrap().branches.insert(branch, info);
    }

    pub fn calls(&self) -> Vec<(String, Value)> {
        self.inner.lock().unwrap().calls.clone()
    }

    /// The bodies of calls to `path` (`"POST /internal/app/sync"`).
    pub fn bodies(&self, call: &str) -> Vec<Value> {
        self.calls()
            .into_iter()
            .filter(|(c, _)| c == call)
            .map(|(_, b)| b)
            .collect()
    }

    pub fn session(&self, id: Uuid) -> Option<FakeSession> {
        self.inner.lock().unwrap().sessions.get(&id).cloned()
    }

    pub fn sessions(&self) -> Vec<(Uuid, FakeSession)> {
        self.inner
            .lock()
            .unwrap()
            .sessions
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect()
    }

    pub fn set_refresh_error(&self, code: Option<&str>) {
        self.inner.lock().unwrap().refresh_error = code.map(str::to_string);
    }
}
