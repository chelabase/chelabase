// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod support;

use auth::app::{AppState, router};
use auth::app_users::{AppUsers, InternalClient, REFRESH_USER_LOOKUP};
use auth::config::Config;
use auth::guard::RateLimiter;
use auth::zitadel::Zitadel;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use chelabase_auth::app_token::{AppClaims, AppTokenSigner, AppTokenVerifier};
use chelabase_auth::internal::{
    AuthMethod, DataIdentity, IDENTITY_HEADER, Identity, InternalSigner, InternalVerifier,
    generate_keypair_pem,
};
use chelabase_identity::mock::FakeIdentity;
use chelabase_identity::{CodeKind, Pool};
use chelabase_query::app_users::{NewUserRow, PendingInsert, SyncTarget, UserRowError, UsersDb};
use chelabase_secrets::SecretKey;
use chelabase_types::data_key::DataKeyKind;
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::internal::*;
use uuid::Uuid;

const PW: &str = "Str0ng!pass";
const NEW_PW: &str = "N3w!passw0rd";

// ---- the identity service, behind a proxy the tests can steer ---------------------------------

type Hook = Arc<dyn Fn() + Send + Sync>;

struct Raw {
    pattern: String,
    status: u16,
    body: String,
    remaining: u32,
}

struct Held {
    pattern: String,
    started: tokio::sync::mpsc::UnboundedSender<()>,
    release: Arc<tokio::sync::Semaphore>,
}

#[derive(Default)]
struct ProxyState {
    /// `"METHOD path"` of every call, answered here or not.
    calls: Vec<String>,
    hooks: Vec<(String, Hook)>,
    raw: Vec<Raw>,
    delays: Vec<(String, Duration)>,
    held: Option<Held>,
}

/// Stands in front of a [`FakeIdentity`]: runs hooks when a call starts, delays or holds calls,
/// or answers them itself with any status and body (an HTML page, an empty 200).
#[derive(Clone)]
struct Proxy {
    base: String,
    state: Arc<Mutex<ProxyState>>,
}

async fn proxy_handle(State((st, upstream)): State<(Arc<Mutex<ProxyState>>, String)>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = to_bytes(Body::new(body), 1 << 20).await.unwrap_or_default();
    let call = format!("{} {}", parts.method, parts.uri.path());
    let (hooks, raw, delay, held) = {
        let mut st = st.lock().unwrap();
        st.calls.push(call.clone());
        let hooks: Vec<Hook> = st
            .hooks
            .iter()
            .filter(|(p, _)| call.contains(p.as_str()))
            .map(|(_, h)| h.clone())
            .collect();
        let raw = st
            .raw
            .iter_mut()
            .find(|r| r.remaining > 0 && call.contains(r.pattern.as_str()))
            .map(|r| {
                r.remaining -= 1;
                (r.status, r.body.clone())
            });
        let delay = st
            .delays
            .iter()
            .find(|(p, _)| call.contains(p.as_str()))
            .map(|(_, d)| *d);
        let held = st
            .held
            .as_ref()
            .filter(|h| call.contains(h.pattern.as_str()))
            .map(|h| (h.started.clone(), h.release.clone()));
        (hooks, raw, delay, held)
    };
    for hook in hooks {
        hook();
    }
    if let Some(d) = delay {
        tokio::time::sleep(d).await;
    }
    if let Some((started, release)) = held {
        let _ = started.send(());
        release.acquire().await.unwrap().forget();
    }
    if let Some((status, body)) = raw {
        return (StatusCode::from_u16(status).unwrap(), body).into_response();
    }
    let path = parts.uri.path_and_query().map_or("/", |p| p.as_str());
    let mut forward = reqwest::Client::new().request(parts.method.clone(), format!("{upstream}{path}"));
    for name in ["authorization", "content-type"] {
        if let Some(v) = parts.headers.get(name) {
            forward = forward.header(name, v.clone());
        }
    }
    let answer = forward.body(bytes).send().await.unwrap();
    let status = answer.status();
    let body = answer.bytes().await.unwrap();
    (status, [("content-type", "application/json")], body).into_response()
}

impl Proxy {
    async fn start(upstream: &str) -> Self {
        let state = Arc::new(Mutex::new(ProxyState::default()));
        let app = axum::Router::new()
            .fallback(proxy_handle)
            .with_state((state.clone(), upstream.to_string()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base: format!("http://{addr}"),
            state,
        }
    }

    fn calls(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.clone()
    }

    fn on_request(&self, pattern: &str, hook: impl Fn() + Send + Sync + 'static) {
        self.state.lock().unwrap().hooks.push((pattern.into(), Arc::new(hook)));
    }

    /// The next `times` calls matching `pattern` get this answer, never reaching the fake.
    fn answer_raw(&self, pattern: &str, status: u16, body: &str, times: u32) {
        self.state.lock().unwrap().raw.push(Raw {
            pattern: pattern.into(),
            status,
            body: body.into(),
            remaining: times,
        });
    }

    /// A refusal in the service's own envelope.
    fn refuse(&self, pattern: &str, status: u16, code: &str, times: u32) {
        self.answer_raw(pattern, status, &json!({"error": code, "message": code}).to_string(), times);
    }

    fn delay(&self, pattern: &str, by: Duration) {
        self.state.lock().unwrap().delays.push((pattern.into(), by));
    }

    /// Holds each call matching `pattern` until released.
    fn hold(&self, pattern: &str) -> HoldHandle {
        let (started, rx) = tokio::sync::mpsc::unbounded_channel();
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        self.state.lock().unwrap().held = Some(Held {
            pattern: pattern.into(),
            started,
            release: release.clone(),
        });
        HoldHandle {
            rx,
            release,
            proxy: self.clone(),
        }
    }

    /// Hooks, raw answers, delays and holds are over.
    fn clear(&self) {
        let mut st = self.state.lock().unwrap();
        st.hooks.clear();
        st.raw.clear();
        st.delays.clear();
        st.held = None;
    }
}

struct HoldHandle {
    rx: tokio::sync::mpsc::UnboundedReceiver<()>,
    release: Arc<tokio::sync::Semaphore>,
    proxy: Proxy,
}

impl HoldHandle {
    async fn started(&mut self) {
        self.rx.recv().await;
    }
    fn release(&self) {
        self.release.add_permits(1);
    }
}

impl Drop for HoldHandle {
    fn drop(&mut self) {
        self.proxy.state.lock().unwrap().held = None;
        self.release.add_permits(1000);
    }
}

// ---- the users table -----------------------------------------------------------------------

#[derive(Clone, Default, PartialEq)]
enum Fail {
    #[default]
    Nothing,
    UnknownColumn(String),
    Constraint,
    EmailTaken,
    UnavailableOnce,
    /// The insert after the identity user exists fails (the probe passes).
    SecondInsert,
    Commit,
    /// The commit takes this long (ms), then succeeds.
    SlowCommit(u64),
}

/// A `UsersDb` that journals each step with how many identity calls had happened by then.
#[derive(Clone)]
struct FakeDb {
    journal: Arc<Mutex<Vec<(String, usize)>>>,
    rows: Arc<Mutex<Vec<(SyncTarget, NewUserRow)>>>,
    fail: Arc<Mutex<Fail>>,
    proxy: Proxy,
}

struct FakePending {
    db: FakeDb,
    row: (SyncTarget, NewUserRow),
}

impl FakeDb {
    fn note(&self, what: &str) {
        let n = self.proxy.calls().len();
        self.journal.lock().unwrap().push((what.to_string(), n));
    }
    fn steps(&self) -> Vec<String> {
        self.journal.lock().unwrap().iter().map(|(s, _)| s.clone()).collect()
    }
    /// The identity calls made before each step, in order.
    fn journal(&self) -> Vec<(String, usize)> {
        self.journal.lock().unwrap().clone()
    }
    fn set(&self, fail: Fail) {
        *self.fail.lock().unwrap() = fail;
    }
    /// The committed rows.
    fn committed(&self) -> Vec<(SyncTarget, NewUserRow)> {
        self.rows.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl UsersDb for FakeDb {
    async fn begin_insert(
        &self,
        target: &SyncTarget,
        row: &NewUserRow,
    ) -> Result<Box<dyn PendingInsert>, UserRowError> {
        self.note("begin");
        let begins = self.steps().iter().filter(|s| *s == "begin").count();
        let fail = self.fail.lock().unwrap().clone();
        match fail {
            Fail::UnknownColumn(c) => return Err(UserRowError::UnknownColumn(c)),
            Fail::Constraint => {
                return Err(UserRowError::Constraint {
                    message: "new row violates check constraint \"age_ok\"".into(),
                    constraint: Some("age_ok".into()),
                });
            }
            Fail::EmailTaken => return Err(UserRowError::EmailTaken),
            Fail::UnavailableOnce => {
                self.set(Fail::Nothing);
                return Err(UserRowError::Unavailable("connection refused".into()));
            }
            Fail::SecondInsert if begins >= 2 => {
                return Err(UserRowError::Constraint {
                    message: "duplicate key value violates unique constraint \"nick_key\"".into(),
                    constraint: Some("nick_key".into()),
                });
            }
            _ => {}
        }
        Ok(Box::new(FakePending {
            db: self.clone(),
            row: (target.clone(), row.clone()),
        }))
    }
}

#[async_trait::async_trait]
impl PendingInsert for FakePending {
    async fn commit(self: Box<Self>) -> Result<(), UserRowError> {
        self.db.note("commit");
        let fail = self.db.fail.lock().unwrap().clone();
        match fail {
            Fail::Commit => return Err(UserRowError::Unavailable("connection lost".into())),
            Fail::SlowCommit(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
            _ => {}
        }
        self.db.rows.lock().unwrap().push(self.row.clone());
        self.db.note("committed");
        Ok(())
    }
    async fn rollback(self: Box<Self>) {
        self.db.note("rollback");
    }
}

// ---- the harness -----------------------------------------------------------------------------

struct H {
    fake: FakeIdentity,
    proxy: Proxy,
    internal: FakeInternal,
    /// In front of `internal`, to fail an orchestrator call.
    iproxy: Proxy,
    db: FakeDb,
    base: String,
    http: reqwest::Client,
    identity: InternalSigner,
    tokens: AppTokenVerifier,
    signer: AppTokenSigner,
    project: Uuid,
    branch: Uuid,
    key: Uuid,
}

struct Ans {
    status: StatusCode,
    body: Value,
}

fn info(project: Uuid, target: Uuid, verification: bool) -> Value {
    json!({
        "project_id": project, "status": "enabled", "zitadel_org_id": null,
        "email_verification": verification, "on_delete": "delete",
        "auth_database_id": Uuid::nil(), "pg_database": "app", "schema": "public",
        "table": "users", "target_branch_id": target,
        "sync": {"branch_id": target, "host": std::net::Ipv4Addr::LOCALHOST.to_string(), "port": 5432, "database": "app",
            "user": "chelabase_auth_sync", "password": "pw", "schema": "public", "table": "users"},
    })
}

async fn harness_opts(verification: bool, min_ms: u64, app_on: bool) -> H {
    harness_full(verification, min_ms, app_on, REFRESH_USER_LOOKUP).await
}

/// A harness whose refreshes wait at most `lookup` for the identity session's answer.
async fn harness_lookup(lookup: Duration) -> H {
    harness_full(false, 1, true, lookup).await
}

async fn harness_full(verification: bool, min_ms: u64, app_on: bool, lookup: Duration) -> H {
    let fake = FakeIdentity::start().await;
    let proxy = Proxy::start(&fake.base).await;
    let internal = FakeInternal::start().await;
    let iproxy = Proxy::start(&internal.base).await;
    let project = Uuid::new_v4();
    let branch = Uuid::new_v4();
    fake.with_pool(&Pool::App(project));
    internal.set_branch(branch, info(project, branch, verification));
    let (id_priv, id_pub) = generate_keypair_pem();
    let (app_priv, app_pub) = generate_keypair_pem();
    let db = FakeDb {
        journal: Arc::default(),
        rows: Arc::default(),
        fail: Arc::default(),
        proxy: proxy.clone(),
    };
    // Nothing of app users reaches Zitadel any more: its address answers nothing.
    let nowhere = "http://127.0.0.1:9";
    let app = app_on.then(|| {
        let mut app = AppUsers::new(
            InternalVerifier::from_pem(id_pub.as_bytes()).unwrap(),
            InternalClient::new(&iproxy.base, INTERNAL_TOKEN),
            AppTokenSigner::from_pem(app_priv.as_bytes(), app_pub.as_bytes()).unwrap(),
            Arc::new(db.clone()),
            Zitadel::new(nowhere, "unused"),
            Duration::from_secs(900),
        );
        app.refresh_user_lookup = lookup;
        app
    });
    let config = Config {
        listen: String::new(),
        issuer: "http://auth.chelabase.localhost:8000".into(),
        zitadel: chelabase_types::entry::ZitadelAt { base: nowhere.into(), host: None },
        token: "unused".into(),
        cookie_key: SecretKey::generate(),
        org_id: "o1".into(),
        studio_url: "http://studio.chelabase.localhost:8000".into(),
        studio: Default::default(),
        skip_email_verification: true,
        secure_cookies: false,
        min_response: Duration::from_millis(min_ms),
        mode: auth::config::Mode::SelfHosted,
        // The test client connects from loopback: it stands in for the gateway.
        trusted_proxies: auth::guard::TrustedProxies::parse("127.0.0.1,::1").unwrap(),
    };
    let keys = tempfile::tempdir().unwrap();
    let (private, public) = chelabase_auth::internal::generate_keypair_pem();
    let member_config = auth::config::MemberTokenConfig {
        private_key: keys.path().join("member-jwt.key"),
        public_key: keys.path().join("member-jwt.pub"),
        ttl: Duration::from_secs(300),
    };
    std::fs::write(&member_config.private_key, &private).unwrap();
    std::fs::write(&member_config.public_key, &public).unwrap();
    let member_tokens = auth::member_token::MemberTokens::from_config(&member_config, &config.issuer).unwrap();
    let state = Arc::new(AppState {
        zitadel: Zitadel::new(nowhere, "unused"),
        config,
        limiter: RateLimiter::default(),
        app,
        signup_gate: None,
        used_tickets: Default::default(),
        admin: None,
        member_tokens,
        identity: chelabase_identity::Identity::new(&proxy.base, chelabase_identity::mock::TOKEN),
        clock: Default::default(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap()
    });
    H {
        fake,
        proxy,
        internal,
        iproxy,
        db,
        base: format!("http://{addr}"),
        http: reqwest::Client::new(),
        identity: InternalSigner::from_pem(id_priv.as_bytes()).unwrap(),
        tokens: AppTokenVerifier::from_pem(app_pub.as_bytes()).unwrap(),
        signer: AppTokenSigner::from_pem(app_priv.as_bytes(), app_pub.as_bytes()).unwrap(),
        project,
        branch,
        key: Uuid::new_v4(),
    }
}

async fn harness() -> H {
    harness_opts(false, 1, true).await
}

impl H {
    fn pool(&self) -> Pool {
        Pool::App(self.project)
    }

    fn pool_path(&self) -> String {
        format!("/internal/v1/pools/{}", self.pool())
    }

    /// The identity the gateway would mint for this request (a public key, and the user
    /// from `access` when given).
    fn identity_for(&self, access: Option<&str>, kind: DataKeyKind) -> String {
        self.identity_opts(access, kind, false)
    }

    /// [`Self::identity_for`], marked server-vouched as the gateway marks it (#55).
    fn identity_opts(&self, access: Option<&str>, kind: DataKeyKind, server_vouched: bool) -> String {
        self.identity_in(self.project, self.branch, access, kind, server_vouched)
    }

    /// The identity for a key of `project`'s `branch`.
    fn identity_in(
        &self,
        project: Uuid,
        branch: Uuid,
        access: Option<&str>,
        kind: DataKeyKind,
        server_vouched: bool,
    ) -> String {
        let (role, claims) = match access {
            None => ("anon".to_string(), json!({})),
            Some(t) => {
                let c = self.tokens.verify(t, project).expect("a valid app token");
                (
                    "authenticated".to_string(),
                    json!({"sub": c.sub, "sid": c.sid, "project": c.project, "email": c.email,
                        "email_verified": c.email_verified, "role": c.role, "exp": c.exp}),
                )
            }
        };
        self.identity
            .sign(
                "auth",
                &Identity {
                    sub: format!("key:{}", self.key),
                    org_id: String::new(),
                    auth: AuthMethod::DataKey,
                    request_id: "rid-1".into(),
                    data: Some(DataIdentity {
                        branch_id: branch,
                        key_id: self.key,
                        kind,
                        role,
                        claims,
                        databases: None,
                        project_id: Some(project),
                        role_oid: None,
                        server_vouched,
                    }),
                    email: None,
                    name: None,
                },
            )
            .unwrap()
    }

    fn identity_raw(&self, role: &str, claims: Value) -> String {
        self.identity
            .sign(
                "auth",
                &Identity {
                    sub: format!("key:{}", self.key),
                    org_id: String::new(),
                    auth: AuthMethod::DataKey,
                    request_id: "rid-1".into(),
                    data: Some(DataIdentity {
                        branch_id: self.branch,
                        key_id: self.key,
                        kind: DataKeyKind::Public,
                        role: role.into(),
                        claims,
                        databases: None,
                        project_id: Some(self.project),
                        role_oid: None,
                        server_vouched: false,
                    }),
                    email: None,
                    name: None,
                },
            )
            .unwrap()
    }

    fn claims(&self, access: &str) -> AppClaims {
        self.tokens
            .verify(access, self.project)
            .expect("a valid app token")
    }

    async fn send(&self, req: reqwest::RequestBuilder, identity: Option<String>) -> Ans {
        self.send_from(req, identity, "10.1.1.1").await
    }

    async fn send_from(&self, req: reqwest::RequestBuilder, identity: Option<String>, ip: &str) -> Ans {
        let mut req = req.header("x-forwarded-for", ip);
        if let Some(i) = identity {
            req = req.header(IDENTITY_HEADER, i);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status();
        let text = resp.text().await.unwrap();
        Ans {
            status,
            body: serde_json::from_str(&text).unwrap_or(Value::Null),
        }
    }

    async fn post(&self, path: &str, body: Value, access: Option<&str>) -> Ans {
        let identity = self.identity_for(access, DataKeyKind::Public);
        self.send(
            self.http
                .post(format!("{}/auth/v1/app/{path}", self.base))
                .json(&body),
            Some(identity),
        )
        .await
    }

    /// A call with a key of another project's branch.
    async fn post_in(&self, project: Uuid, branch: Uuid, path: &str, body: Value, access: Option<&str>) -> Ans {
        let identity = self.identity_in(project, branch, access, DataKeyKind::Public, false);
        self.send(
            self.http
                .post(format!("{}/auth/v1/app/{path}", self.base))
                .json(&body),
            Some(identity),
        )
        .await
    }

    /// A call the app's server vouched for with its secret key and no client address.
    async fn post_vouched(&self, path: &str, body: Value, access: Option<&str>) -> Ans {
        let identity = self.identity_opts(access, DataKeyKind::Public, true);
        self.send(
            self.http
                .post(format!("{}/auth/v1/app/{path}", self.base))
                .json(&body),
            Some(identity),
        )
        .await
    }

    async fn get(&self, path: &str, access: Option<&str>) -> Ans {
        let identity = self.identity_for(access, DataKeyKind::Public);
        self.send(
            self.http.get(format!("{}/auth/v1/app/{path}", self.base)),
            Some(identity),
        )
        .await
    }

    async fn sign_in(&self, email: &str, password: &str) -> Ans {
        self.post(
            "sign-in",
            json!({"email": email, "password": password}),
            None,
        )
        .await
    }

    /// A user in the project's pool, made directly in the fake, with password [`PW`].
    async fn add_user(&self, email: &str, verified: bool) -> String {
        self.add_user_in(self.pool(), email, PW, verified).await
    }

    async fn add_user_in(&self, pool: Pool, email: &str, password: &str, verified: bool) -> String {
        self.fake.with_pool(&pool);
        self.fake
            .client()
            .create_user(
                &pool,
                &chelabase_identity::NewUser {
                    email,
                    password: Some(password),
                    name: None,
                    email_verified: verified,
                },
            )
            .await
            .unwrap()
            .id
    }

    async fn user(&self, id: &str) -> Option<chelabase_identity::User> {
        self.fake.client().get_user(&self.pool(), id).await.ok()
    }

    /// Whether `password` signs `email` in, checked in the fake directly (the session the
    /// check makes is ended again, so session counts are unchanged).
    async fn password_is(&self, email: &str, password: &str) -> bool {
        let client = self.fake.client();
        match client.sign_in(&self.pool(), email, password).await {
            Ok(signed) => {
                client.revoke_session(&self.pool(), &signed.session_id).await.unwrap();
                true
            }
            Err(_) => false,
        }
    }

    fn session_id(&self, s: &Ans) -> Uuid {
        self.claims(s.body["access_token"].as_str().unwrap()).sid
    }

    /// The identity session behind app session `sid`.
    fn identity_session(&self, sid: Uuid) -> String {
        self.internal.session(sid).unwrap().identity_session_id
    }

    async fn identity_session_live(&self, id: &str) -> bool {
        self.fake.client().session_by_id(&self.pool(), id).await.is_ok()
    }

    fn identity_creates(&self) -> usize {
        let path = format!("POST {}/users", self.pool_path());
        self.proxy.calls().iter().filter(|c| **c == path).count()
    }

    /// The codes of `kind` the fake mailed, as `(user id, code)`.
    fn codes(&self, kind: CodeKind) -> Vec<(String, String)> {
        self.fake
            .codes_sent()
            .into_iter()
            .filter(|c| c.kind == kind)
            .map(|c| (c.user_id, c.code))
            .collect()
    }

    fn last_code(&self, user: &str, kind: CodeKind) -> String {
        self.codes(kind)
            .into_iter()
            .rev()
            .find(|(u, _)| u == user)
            .unwrap_or_else(|| panic!("no {kind:?} code for {user}"))
            .1
    }

    /// A live app session of `user` backed by identity session `identity_session_id`.
    fn add_app_session(&self, project: Uuid, user: Uuid, identity_session_id: &str, rotated: bool) -> (Uuid, String) {
        let (sid, token) = self.internal.add_session(project, user, rotated);
        self.internal
            .inner
            .lock()
            .unwrap()
            .sessions
            .get_mut(&sid)
            .unwrap()
            .identity_session_id = identity_session_id.into();
        (sid, token)
    }
}

fn access(a: &Ans) -> &str {
    a.body["access_token"].as_str().expect("an access token")
}

fn code(a: &Ans) -> &str {
    a.body["error"]["code"].as_str().unwrap_or("")
}

fn keys(v: &Value) -> Vec<String> {
    let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    k.sort();
    k
}

// ---- sign-up ---------------------------------------------------------------------------

#[tokio::test]
async fn sign_up_with_verification_off_returns_tokens() {
    let h = harness().await;
    let a = h
        .post(
            "sign-up",
            json!({"email": " Ada@Example.com ", "password": PW, "data": {"nickname": "ada"}}),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
    assert_eq!(a.body["token_type"], "bearer");
    assert_eq!(a.body["expires_in"], 900);
    assert!(
        a.body["refresh_token"]
            .as_str()
            .is_some_and(|t| !t.is_empty())
    );
    let user = &a.body["user"];
    assert_eq!(user["email"], "ada@example.com");
    assert_eq!(user["email_verified"], true);
    assert!(user["created_at"].is_string());
    let id: Uuid = user["id"].as_str().unwrap().parse().unwrap();
    let c = h.claims(access(&a));
    assert_eq!(c.sub, id);
    assert_eq!(c.project, h.project);
    assert_eq!(c.role, "authenticated");
    assert_eq!(c.email, "ada@example.com");
    assert!(c.email_verified);
    assert_eq!(c.exp - c.iat, 900);
    // Identity: the user is in the project's pool, already verified, with the password.
    let u = h.user(&id.to_string()).await.expect("identity user");
    assert!(u.email_verified);
    assert!(h.password_is("ada@example.com", PW).await);
    // The row: identity's id, managed columns plus data, on the key's branch.
    let rows = h.db.committed();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0.branch_id, h.branch);
    assert_eq!(rows[0].1.id, id);
    assert!(rows[0].1.email_verified);
    assert_eq!(rows[0].1.data.get("nickname"), Some(&json!("ada")));
    // The sync, then the app session, linked to an identity session of the user.
    let sync = h.internal.bodies("POST /internal/app/sync");
    assert_eq!(sync.len(), 1);
    assert_eq!(sync[0]["user_id"], json!(id));
    assert_eq!(sync[0]["project_id"], json!(h.project));
    assert_eq!(sync[0]["origin_branch_id"], json!(h.branch));
    assert_eq!(sync[0]["data"], json!({"nickname": "ada"}));
    let sessions = h.internal.bodies("POST /internal/app/sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["user_id"], json!(id));
    let s = h.internal.session(c.sid).unwrap();
    assert_eq!(s.user_id, id);
    assert!(h.identity_session_live(&s.identity_session_id).await);
}

/// #123: identity picks the user's id, so a probe insert (rolled back) checks the row before
/// identity is called, and the real insert with identity's id commits after.
#[tokio::test]
async fn sign_up_probes_the_row_before_identity_and_commits_after() {
    let h = harness().await;
    let a = h
        .post(
            "sign-up",
            json!({"email": "ada@example.com", "password": PW}),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
    let journal = h.db.journal();
    let steps: Vec<&str> = journal.iter().map(|(s, _)| s.as_str()).collect();
    assert_eq!(steps, ["begin", "rollback", "begin", "commit", "committed"]);
    let create = format!("POST {}/users", h.pool_path());
    let created = h.proxy.calls().iter().position(|c| *c == create).expect("created");
    assert_eq!(journal[0].1, 0, "the probe comes before any identity call");
    assert_eq!(journal[1].1, 0, "and is rolled back before any identity call");
    assert!(journal[2].1 > created, "the real insert comes after identity's create");
    let id: Uuid = a.body["user"]["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(h.db.committed()[0].1.id, id, "the row has identity's id");
}

#[tokio::test]
async fn constraint_failure_never_calls_identity() {
    let h = harness().await;
    h.db.set(Fail::Constraint);
    let a = h
        .post(
            "sign-up",
            json!({"email": "ada@example.com", "password": PW, "data": {"age": -1}}),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", a.body);
    assert_eq!(code(&a), "constraint_violation");
    assert_eq!(a.body["error"]["constraint"], "age_ok");
    assert!(
        a.body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("age_ok")
    );
    assert_eq!(h.proxy.calls(), Vec::<String>::new(), "identity is never called");
}

#[tokio::test]
async fn unknown_column_is_422_before_identity() {
    let h = harness().await;
    h.db.set(Fail::UnknownColumn("nickname".into()));
    let a = h
        .post(
            "sign-up",
            json!({"email": "ada@example.com", "password": PW, "data": {"nickname": "x"}}),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", a.body);
    assert_eq!(code(&a), "unknown_column");
    let message = a.body["error"]["message"].as_str().unwrap();
    assert!(message.contains("nickname"), "{message}");
    assert!(message.contains("GRANT INSERT (nickname)"), "{message}");
    assert!(message.contains("public.users"), "{message}");
    // `data` must be an object; a weak password and a bad email are refused before anything.
    for body in [
        json!({"email": "ada@example.com", "password": PW, "data": 5}),
        json!({"email": "ada@example.com", "password": PW, "data": ["a"]}),
        json!({"email": "not-an-email", "password": PW}),
    ] {
        let a = h.post("sign-up", body.clone(), None).await;
        assert_eq!(a.status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(code(&a), "invalid_request", "{body}");
    }
    let a = h
        .post(
            "sign-up",
            json!({"email": "ada@example.com", "password": "weak"}),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code(&a), "weak_password");
    assert_eq!(h.proxy.calls(), Vec::<String>::new(), "identity is never called");
    assert_eq!(h.db.steps(), ["begin"]);
}

#[tokio::test]
async fn commit_failure_deletes_the_identity_user() {
    let h = harness().await;
    h.db.set(Fail::Commit);
    let a = h
        .post(
            "sign-up",
            json!({"email": "ada@example.com", "password": PW}),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE, "{}", a.body);
    assert_eq!(code(&a), "unavailable");
    assert_eq!(h.identity_creates(), 1);
    let deletes: Vec<String> = h
        .proxy
        .calls()
        .into_iter()
        .filter(|c| c.starts_with(&format!("DELETE {}/users/", h.pool_path())))
        .collect();
    assert_eq!(deletes.len(), 1, "{:?}", h.proxy.calls());
    assert!(!h.password_is("ada@example.com", PW).await, "the identity user is gone");
    assert!(h.internal.bodies("POST /internal/app/sync").is_empty());
    // The same sign-up again works.
    h.db.set(Fail::Nothing);
    let a = h
        .post("sign-up", json!({"email": "ada@example.com", "password": PW}), None)
        .await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
}

/// #123: the insert with identity's id fails after the identity user exists: the user is
/// deleted, and the answer is the row's own refusal.
#[tokio::test]
async fn a_failed_insert_after_identity_deletes_the_identity_user() {
    let h = harness().await;
    h.db.set(Fail::SecondInsert);
    let a = h
        .post("sign-up", json!({"email": "ada@example.com", "password": PW}), None)
        .await;
    assert_eq!(
        (a.status, code(&a)),
        (StatusCode::UNPROCESSABLE_ENTITY, "constraint_violation"),
        "{}",
        a.body
    );
    assert_eq!(h.identity_creates(), 1);
    assert!(!h.password_is("ada@example.com", PW).await, "the identity user is gone");
    assert!(h.db.committed().is_empty());
    assert!(h.internal.bodies("POST /internal/app/sync").is_empty());
    assert!(h.internal.bodies("POST /internal/app/sessions").is_empty());
}

#[tokio::test]
async fn unavailable_database_refetches_the_branch_and_retries_once() {
    let h = harness().await;
    h.db.set(Fail::UnavailableOnce);
    let a = h
        .post(
            "sign-up",
            json!({"email": "ada@example.com", "password": PW}),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
    assert_eq!(h.db.steps(), ["begin", "begin", "rollback", "begin", "commit", "committed"]);
    let fetches = h
        .internal
        .calls()
        .iter()
        .filter(|(c, _)| c.starts_with("GET /internal/app/branches/"))
        .count();
    assert_eq!(fetches, 2, "the cached settings were dropped");
}

#[tokio::test]
async fn taken_email_409_when_verification_off() {
    let h = harness().await;
    h.add_user("ada@example.com", true).await;
    let a = h
        .post(
            "sign-up",
            json!({"email": "Ada@example.com", "password": PW}),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::CONFLICT, "{}", a.body);
    assert_eq!(code(&a), "email_taken");
    assert_eq!(h.db.steps(), ["begin", "rollback"]);
    assert!(h.db.committed().is_empty());
    // A unique violation on the email index counts as taken too.
    h.db.set(Fail::EmailTaken);
    let a = h
        .post(
            "sign-up",
            json!({"email": "bob@example.com", "password": PW}),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::CONFLICT, "{}", a.body);
    assert_eq!(code(&a), "email_taken");
}

#[tokio::test]
async fn taken_email_decoy_when_verification_on() {
    let h = harness_opts(true, 1, true).await;
    let existing = h.add_user("ada@example.com", true).await;
    let fresh = h
        .post(
            "sign-up",
            json!({"email": "bob@example.com", "password": PW}),
            None,
        )
        .await;
    let decoy = h
        .post(
            "sign-up",
            json!({"email": "ada@example.com", "password": PW}),
            None,
        )
        .await;
    assert_eq!(fresh.status, StatusCode::CREATED, "{}", fresh.body);
    assert_eq!(decoy.status, StatusCode::CREATED, "{}", decoy.body);
    assert_eq!(keys(&decoy.body), keys(&fresh.body));
    assert_eq!(keys(&decoy.body["user"]), keys(&fresh.body["user"]));
    assert_eq!(decoy.body["verification_required"], true);
    assert_eq!(decoy.body["user"]["email_verified"], false);
    assert_eq!(decoy.body["user"]["email"], "ada@example.com");
    assert_ne!(decoy.body["user"]["id"], json!(existing));
    assert!(h.password_is("ada@example.com", PW).await, "the owner's account is untouched");
    // The owner gets a notice (a reset code).
    assert_eq!(h.codes(CodeKind::Reset).len(), 1);
    assert_eq!(h.codes(CodeKind::Reset)[0].0, existing);
}

#[tokio::test]
async fn bad_data_is_422_even_when_the_email_is_taken() {
    let h = harness_opts(true, 1, true).await;
    h.add_user("ada@example.com", true).await;
    h.db.set(Fail::Constraint);
    let a = h
        .post(
            "sign-up",
            json!({"email": "ada@example.com", "password": PW, "data": {"age": -1}}),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", a.body);
    assert_eq!(code(&a), "constraint_violation");
    assert!(h.fake.codes_sent().is_empty());
}

#[tokio::test]
async fn sign_up_from_other_database_key_targets_auth_main() {
    let h = harness().await;
    let main = Uuid::new_v4();
    h.internal
        .set_branch(h.branch, info(h.project, main, false));
    let a = h
        .post(
            "sign-up",
            json!({"email": "ada@example.com", "password": PW}),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
    assert_eq!(h.db.committed()[0].0.branch_id, main);
    let sync = h.internal.bodies("POST /internal/app/sync");
    assert_eq!(sync[0]["origin_branch_id"], json!(main));
}

#[tokio::test]
async fn sign_up_with_verification_on_returns_pending_and_sends_code() {
    let h = harness_opts(true, 1, true).await;
    let a = h
        .post(
            "sign-up",
            json!({"email": "ada@example.com", "password": PW}),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
    assert_eq!(a.body["verification_required"], true);
    assert!(a.body.get("access_token").is_none());
    let id = a.body["user"]["id"].as_str().unwrap().to_string();
    assert_eq!(a.body["user"]["email_verified"], false);
    assert!(!h.user(&id).await.unwrap().email_verified);
    assert!(!h.db.committed()[0].1.email_verified);
    let sent = h.last_code(&id, CodeKind::Verify);
    assert!(h.internal.bodies("POST /internal/app/sessions").is_empty());
    // Not yet: sign-in waits for the code.
    let a = h.sign_in("ada@example.com", PW).await;
    assert_eq!(a.status, StatusCode::FORBIDDEN);
    assert_eq!(code(&a), "email_not_verified");
    // A wrong code, then the right one.
    let wrong = if sent == "000000" { "111111" } else { "000000" };
    let bad = h
        .post(
            "verify-email",
            json!({"email": "ada@example.com", "code": wrong}),
            None,
        )
        .await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code(&bad), "invalid_code");
    let ok = h
        .post(
            "verify-email",
            json!({"email": "ada@example.com", "code": format!(" {sent} ")}),
            None,
        )
        .await;
    assert_eq!(ok.status, StatusCode::NO_CONTENT, "{}", ok.body);
    assert!(h.user(&id).await.unwrap().email_verified);
    let syncs = h.internal.bodies("POST /internal/app/sync");
    assert_eq!(syncs.last().unwrap()["user_id"], json!(id));
    let a = h.sign_in("ada@example.com", PW).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    // A used code, and an unknown email's code, are refused the same way.
    for email in ["ada@example.com", "zed@example.com"] {
        let again = h
            .post("verify-email", json!({"email": email, "code": sent}), None)
            .await;
        assert_eq!(again.status, StatusCode::UNPROCESSABLE_ENTITY, "{email}");
        assert_eq!(code(&again), "invalid_code", "{email}");
    }
}

/// #123: identity's refusals of a new user: a taken email is taken, its password and email
/// checks are the caller's input, anything else is unavailable.
#[tokio::test]
async fn app_sign_up_reads_identity_refusals() {
    let h = harness().await;
    let users = format!("POST {}/users", h.pool_path());
    let sign_up = async |email: &str| h.post("sign-up", json!({"email": email, "password": PW}), None).await;
    h.proxy.refuse(&users, 409, "conflict", 1);
    let a = sign_up("a1@example.com").await;
    assert_eq!((a.status, code(&a)), (StatusCode::CONFLICT, "email_taken"), "{}", a.body);
    h.proxy.refuse(&users, 400, "weak_password", 1);
    let a = sign_up("a2@example.com").await;
    assert_eq!((a.status, code(&a)), (StatusCode::UNPROCESSABLE_ENTITY, "weak_password"), "{}", a.body);
    h.proxy.refuse(&users, 400, "invalid_email", 1);
    let a = sign_up("a3@example.com").await;
    assert_eq!((a.status, code(&a)), (StatusCode::UNPROCESSABLE_ENTITY, "invalid_request"), "{}", a.body);
    for (status, refusal) in [(404, "pool_not_found"), (403, "forbidden"), (412, "something_else"), (500, "internal")] {
        h.proxy.refuse(&users, status, refusal, 1);
        let a = sign_up(&format!("{refusal}@example.com")).await;
        assert_eq!((a.status, code(&a)), (StatusCode::SERVICE_UNAVAILABLE, "unavailable"), "{refusal}: {}", a.body);
    }
    assert!(h.db.committed().is_empty());
    assert!(h.internal.bodies("POST /internal/app/sync").is_empty());
}

// ---- sign-in -----------------------------------------------------------------------------

#[tokio::test]
async fn sign_in_returns_a_session() {
    let h = harness().await;
    let id = h.add_user("ada@example.com", true).await;
    let a = h.sign_in("ADA@example.com", PW).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(a.body["user"]["id"], json!(id));
    let c = h.claims(access(&a));
    assert_eq!(c.sub.to_string(), id);
    let s = h.internal.session(c.sid).expect("app session");
    assert_eq!(s.project_id, h.project);
    assert_eq!(a.body["refresh_token"], json!(s.token));
    // The app session names the identity session the sign-in made; no token is kept.
    let body = &h.internal.bodies("POST /internal/app/sessions")[0];
    assert_eq!(keys(body), ["identity_session_id", "project_id", "user_id"]);
    assert!(h.identity_session_live(&s.identity_session_id).await);
}

#[tokio::test]
async fn sign_in_banned_check_comes_after_the_password() {
    let h = harness().await;
    let banned = h.add_user("banned@example.com", true).await;
    h.fake.ban(&h.pool(), &banned);
    let a = h.sign_in("banned@example.com", "Wr0ng!pass").await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    assert_eq!(code(&a), "invalid_credentials");
}

/// AC 5: an unknown email, a wrong password, and a wrong password for an account that still
/// has to verify its email answer alike: status, body and padding.
#[tokio::test]
async fn sign_in_failures_indistinguishable() {
    let h = harness_opts(true, 150, true).await;
    h.add_user("ada@example.com", true).await;
    h.add_user("new@example.com", false).await;
    let mut answers = Vec::new();
    for (email, password) in [
        ("ada@example.com", "Wr0ng!pass"),
        ("zed@example.com", PW),
        ("new@example.com", "Wr0ng!pass"),
    ] {
        let start = Instant::now();
        let a = h.sign_in(email, password).await;
        assert!(start.elapsed() >= Duration::from_millis(150), "{email}: not padded");
        answers.push(a);
    }
    assert_eq!(answers[0].status, StatusCode::UNAUTHORIZED);
    assert_eq!(code(&answers[0]), "invalid_credentials");
    for a in &answers[1..] {
        assert_eq!((a.status, &a.body), (answers[0].status, &answers[0].body));
    }
    assert!(h.internal.bodies("POST /internal/app/sessions").is_empty());
}

#[tokio::test]
async fn sign_in_unverified_403() {
    let h = harness_opts(true, 1, true).await;
    h.add_user("ada@example.com", false).await;
    let a = h.sign_in("ada@example.com", PW).await;
    assert_eq!(a.status, StatusCode::FORBIDDEN, "{}", a.body);
    assert_eq!(code(&a), "email_not_verified");
    assert_eq!(h.fake.session_count(&h.pool()), 0, "the identity session is ended");
    assert!(h.internal.bodies("POST /internal/app/sessions").is_empty());
}

#[tokio::test]
async fn sign_in_banned_403() {
    let h = harness().await;
    let id = h.add_user("ada@example.com", true).await;
    h.fake.ban(&h.pool(), &id);
    let a = h.sign_in("ada@example.com", PW).await;
    assert_eq!(a.status, StatusCode::FORBIDDEN, "{}", a.body);
    assert_eq!(code(&a), "user_inactive");
    assert_eq!(h.fake.session_count(&h.pool()), 0);
    assert!(h.internal.bodies("POST /internal/app/sessions").is_empty());
}

/// The app session can't be stored: the identity session made for it is ended, and nothing
/// is half signed in; a retry works.
#[tokio::test]
async fn sign_in_ends_the_identity_session_when_the_app_session_fails() {
    let h = harness().await;
    h.add_user("ada@example.com", true).await;
    h.iproxy.answer_raw("POST /internal/app/sessions", 503, "", 1);
    let a = h.sign_in("ada@example.com", PW).await;
    assert_eq!((a.status, code(&a)), (StatusCode::SERVICE_UNAVAILABLE, "unavailable"), "{}", a.body);
    assert_eq!(h.fake.session_count(&h.pool()), 0);
    let a = h.sign_in("ada@example.com", PW).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
}

/// AC 2: one email, two projects: each has its own account and password, and a token of one
/// project is refused by the other.
#[tokio::test]
async fn same_email_two_projects_isolated() {
    let h = harness().await;
    let (project_b, branch_b) = (Uuid::new_v4(), Uuid::new_v4());
    h.fake.with_pool(&Pool::App(project_b));
    h.internal.set_branch(branch_b, info(project_b, branch_b, false));
    let up = |project, branch, pw: &'static str| {
        let h = &h;
        async move {
            h.post_in(project, branch, "sign-up", json!({"email": "ada@example.com", "password": pw}), None)
                .await
        }
    };
    let a = up(h.project, h.branch, PW).await;
    let b = up(project_b, branch_b, NEW_PW).await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
    assert_eq!(b.status, StatusCode::CREATED, "{}", b.body);
    assert_ne!(a.body["user"]["id"], b.body["user"]["id"]);
    // Each project's password, and only that one.
    let sign_in = |project, branch, pw: &'static str| {
        let h = &h;
        async move {
            h.post_in(project, branch, "sign-in", json!({"email": "ada@example.com", "password": pw}), None)
                .await
                .status
        }
    };
    assert_eq!(sign_in(h.project, h.branch, PW).await, StatusCode::OK);
    assert_eq!(sign_in(h.project, h.branch, NEW_PW).await, StatusCode::UNAUTHORIZED);
    assert_eq!(sign_in(project_b, branch_b, NEW_PW).await, StatusCode::OK);
    assert_eq!(sign_in(project_b, branch_b, PW).await, StatusCode::UNAUTHORIZED);
    // A's access token is not B's: B's data API (the gateway verifies the audience) refuses it.
    let token_a = access(&a).to_string();
    assert!(h.tokens.verify(&token_a, project_b).is_err());
    assert!(h.tokens.verify(&token_a, h.project).is_ok());
    // A's refresh token, presented with B's key, is not B's session.
    let r = h
        .post_in(project_b, branch_b, "refresh", json!({"refresh_token": a.body["refresh_token"]}), None)
        .await;
    assert_eq!((r.status, code(&r)), (StatusCode::UNAUTHORIZED, "refresh_token_invalid"), "{}", r.body);
}

// ---- sessions ------------------------------------------------------------------------------

#[tokio::test]
async fn refresh_passes_reused_through() {
    let h = harness().await;
    h.add_user("ada@example.com", true).await;
    let s = h.sign_in("ada@example.com", PW).await;
    h.internal.set_refresh_error(Some("refresh_token_reused"));
    let a = h
        .post(
            "refresh",
            json!({"refresh_token": s.body["refresh_token"]}),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED, "{}", a.body);
    assert_eq!(code(&a), "refresh_token_reused");
    h.internal.set_refresh_error(None);
    let a = h
        .post("refresh", json!({"refresh_token": "nope"}), None)
        .await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    assert_eq!(code(&a), "refresh_token_invalid");
}

/// Only the orchestrator's `refresh_token_invalid` ends a session for good; a bare 401 (a
/// mismatched internal bearer), a 404 (a missing route) or a 422 (a body the orchestrator
/// rejects mid-deploy) is a fault on our side, and the client keeps its cookies.
#[tokio::test]
async fn refresh_faults_are_unavailable_not_invalid() {
    let h = harness().await;
    h.add_user("ada@example.com", true).await;
    let s = h.sign_in("ada@example.com", PW).await;
    let cases = [
        (StatusCode::UNAUTHORIZED, None),
        (StatusCode::UNAUTHORIZED, Some("unauthorized")),
        (StatusCode::NOT_FOUND, None),
        (StatusCode::NOT_FOUND, Some("not_found")),
        (StatusCode::UNPROCESSABLE_ENTITY, None),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Some("refresh_token_invalid"),
        ),
    ];
    for (status, answer) in cases {
        h.internal.set_refresh_answer(status, answer);
        let a = h
            .post(
                "refresh",
                json!({"refresh_token": s.body["refresh_token"]}),
                None,
            )
            .await;
        assert_eq!(
            a.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{status} {answer:?}: {}",
            a.body
        );
        assert_eq!(code(&a), "unavailable", "{status} {answer:?}");
    }
    h.internal
        .set_refresh_answer(StatusCode::UNAUTHORIZED, Some("refresh_token_invalid"));
    let a = h
        .post(
            "refresh",
            json!({"refresh_token": s.body["refresh_token"]}),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED, "{}", a.body);
    assert_eq!(code(&a), "refresh_token_invalid");
}

/// What the orchestrator answers a peek with (#59).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Peek {
    /// The fake's own answer from the session: its user (the token is current or in grace).
    Live,
    Invalid401,
    Reused401,
    Bare401,
    NotFound404,
    Unprocessable422,
    Internal500,
    /// A 200 with no body: no session, no user.
    Empty200,
}

/// What the orchestrator answers a refresh with.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Orch {
    Rotated,
    Grace,
    Invalid401,
    Reused401,
    Bare401,
    NotFound404,
    Unprocessable422,
    Internal500,
}

/// What the identity service answers the session lookup with (#123).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Ident {
    /// The session is live, and its user is the app session's.
    Live,
    /// The user is banned: the session is gone (the ban ended it, or the read dropped it).
    Banned,
    /// The session was revoked (a password change, a reset, a sign-out elsewhere).
    SessionGone,
    /// The user was deleted.
    UserGone,
    /// A live session, but of another user than the app session's.
    OtherUser,
    /// A live session whose user the answer marks banned.
    BannedInAnswer,
    /// The pool doesn't exist (yet, or any more): no verdict on the user.
    PoolGone404,
    /// A 404 that isn't the service's (a misrouted proxy's page).
    Html404,
    /// The route doesn't exist (an older service).
    Route404,
    /// Auth may not call the route.
    Forbidden403,
    Internal500,
    Empty200,
    Malformed200,
}

/// When the grace of the session's previous token ends (#59 audit B1).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Timing {
    /// Not during the refresh.
    Steady,
    /// While identity answers the session lookup. Only a grace token (`Orch::Grace`) has a
    /// previous token whose grace can end; a current token rotated by a parallel request
    /// during the lookup has its own tests
    /// (`refresh_of_a_current_token_rotated_during_the_lookup_gets_the_grace_answer`).
    GraceEndsInLookup,
}

/// What a refresh does, as the client and the orchestrator see it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Outcome {
    status: StatusCode,
    /// The error code, `""` on success.
    code: &'static str,
    /// Auth revoked the session.
    revoke: bool,
    /// Auth asked the orchestrator to refresh (rotate) the token.
    refresh: bool,
}

/// #57's invariant on `refresh`, as a model, with #59's peek in front and #123's identity
/// session check. Only a definitive verdict ends a session: the orchestrator's 401
/// `refresh_token_invalid`/`refresh_token_reused`, or identity saying the session is gone (its
/// user banned: 403 `user_inactive`; otherwise 401 `refresh_token_invalid`), belongs to another
/// user, or has a banned user. Anything else is 503 `unavailable` and revokes nothing. A peek
/// that finds the token's session checks it in identity before anything rotates, so an identity
/// fault never follows a rotation; a peek verdict goes on to `refresh`, which applies the
/// orchestrator's own rules, as before #59. So does a grace token (`Orch::Grace`): its refresh
/// writes nothing, and it must reach the orchestrator before the grace ends (#59 audit B1).
fn refresh_model(p: Peek, o: Orch, i: Ident) -> Outcome {
    let answer = |status, code, revoke, refresh| Outcome {
        status,
        code,
        revoke,
        refresh,
    };
    let down = |refresh| {
        answer(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            false,
            refresh,
        )
    };
    // The session check, `Ok` when the session may refresh.
    let check = || match i {
        Ident::Live => Ok(()),
        Ident::Banned | Ident::BannedInAnswer => Err((StatusCode::FORBIDDEN, "user_inactive", true)),
        Ident::SessionGone | Ident::UserGone | Ident::OtherUser => {
            Err((StatusCode::UNAUTHORIZED, "refresh_token_invalid", true))
        }
        Ident::PoolGone404
        | Ident::Html404
        | Ident::Route404
        | Ident::Forbidden403
        | Ident::Internal500
        | Ident::Empty200
        | Ident::Malformed200 => Err((StatusCode::SERVICE_UNAVAILABLE, "unavailable", false)),
    };
    // The refresh call: `Ok` when it rotated or answered the grace.
    let rotate = || match o {
        Orch::Rotated | Orch::Grace => Ok(()),
        Orch::Invalid401 => Err(answer(
            StatusCode::UNAUTHORIZED,
            "refresh_token_invalid",
            false,
            true,
        )),
        Orch::Reused401 => Err(answer(
            StatusCode::UNAUTHORIZED,
            "refresh_token_reused",
            false,
            true,
        )),
        Orch::Bare401 | Orch::NotFound404 | Orch::Unprocessable422 | Orch::Internal500 => {
            Err(down(true))
        }
    };
    match p {
        Peek::Bare401
        | Peek::NotFound404
        | Peek::Unprocessable422
        | Peek::Internal500
        | Peek::Empty200 => down(false),
        Peek::Live if o != Orch::Grace => match check() {
            Err((status, code, revoke)) => answer(status, code, revoke, false),
            Ok(()) => match rotate() {
                Ok(()) => answer(StatusCode::OK, "", false, true),
                Err(refused) => refused,
            },
        },
        // As before #59: refresh, then the session of a token it accepted.
        Peek::Live | Peek::Invalid401 | Peek::Reused401 => match rotate() {
            Err(refused) => refused,
            Ok(()) => match check() {
                Ok(()) => answer(StatusCode::OK, "", false, true),
                Err((status, code, revoke)) => answer(status, code, revoke, true),
            },
        },
    }
}

/// One case of the matrix: the first refresh against the model, and, when it answered 503,
/// a retry with the same token once every fault is over and (for a current token) the grace
/// has ended, as after a long outage (#59). Returns what differs from the model.
async fn refresh_case(p: Peek, o: Orch, i: Ident, t: Timing) -> Option<String> {
    use Ident as I;
    use Orch as O;
    let h = harness().await;
    let pool = h.pool();
    let lookup_pattern = format!("GET {}/sessions/", h.pool_path());
    if t == Timing::GraceEndsInLookup {
        let internal = h.internal.clone();
        h.proxy.on_request(&lookup_pattern, move || internal.end_grace());
    }
    let user = h.add_user("ada@example.com", true).await;
    let other = h.add_user("bob@example.com", true).await;
    let owner = if i == I::OtherUser { &other } else { &user };
    let (identity_session, _) = h.fake.with_session(&pool, owner);
    let lookup = format!("GET {}/sessions/{identity_session}", h.pool_path());
    let client = h.fake.client();
    match i {
        I::Live | I::OtherUser => {}
        I::Banned => h.fake.ban(&pool, &user),
        I::SessionGone => client.revoke_session(&pool, &identity_session).await.unwrap(),
        I::UserGone => client.delete_user(&pool, &user).await.unwrap(),
        I::BannedInAnswer => {
            let now = "2026-11-03T10:00:00.000Z";
            let body = json!({
                "session": {"id": identity_session, "user_id": user, "expires_at": "2027-01-01T00:00:00.000Z", "created_at": now},
                "user": {"id": user, "email": "ada@example.com", "name": "", "email_verified": true,
                    "banned": true, "created_at": now, "updated_at": now},
            });
            h.proxy.answer_raw(&lookup, 200, &body.to_string(), 1);
        }
        I::PoolGone404 => h.proxy.refuse(&lookup, 404, "pool_not_found", 1),
        I::Html404 => h.proxy.answer_raw(&lookup, 404, "<html>not found</html>", 1),
        I::Route404 => h.proxy.refuse(&lookup, 404, "not_found", 1),
        I::Forbidden403 => h.proxy.refuse(&lookup, 403, "forbidden", 1),
        I::Internal500 => h.proxy.refuse(&lookup, 500, "internal", 1),
        I::Empty200 => h.proxy.answer_raw(&lookup, 200, "", 1),
        I::Malformed200 => h.proxy.answer_raw(&lookup, 200, r#"{"session": {"id""#, 1),
    }
    let grace = o == O::Grace;
    let (sid, token) = h.add_app_session(h.project, user.parse().unwrap(), &identity_session, grace);
    let current = h.internal.session(sid).unwrap().token;
    let refused = |status, code| h.internal.set_refresh_answer(status, code);
    match o {
        O::Rotated | O::Grace => {}
        O::Invalid401 => refused(StatusCode::UNAUTHORIZED, Some("refresh_token_invalid")),
        O::Reused401 => refused(StatusCode::UNAUTHORIZED, Some("refresh_token_reused")),
        O::Bare401 => refused(StatusCode::UNAUTHORIZED, None),
        O::NotFound404 => refused(StatusCode::NOT_FOUND, None),
        O::Unprocessable422 => refused(StatusCode::UNPROCESSABLE_ENTITY, None),
        O::Internal500 => refused(StatusCode::INTERNAL_SERVER_ERROR, Some("internal")),
    }
    let peeked = |status, code| h.internal.set_peek_answer(status, code);
    match p {
        Peek::Live => {}
        Peek::Invalid401 => peeked(StatusCode::UNAUTHORIZED, Some("refresh_token_invalid")),
        Peek::Reused401 => peeked(StatusCode::UNAUTHORIZED, Some("refresh_token_reused")),
        Peek::Bare401 => peeked(StatusCode::UNAUTHORIZED, None),
        Peek::NotFound404 => peeked(StatusCode::NOT_FOUND, None),
        Peek::Unprocessable422 => peeked(StatusCode::UNPROCESSABLE_ENTITY, None),
        Peek::Internal500 => peeked(StatusCode::INTERNAL_SERVER_ERROR, Some("internal")),
        Peek::Empty200 => peeked(StatusCode::OK, None),
    }
    let refresh_calls = || h.internal.count("POST /internal/app/sessions/refresh");
    let a = h
        .post("refresh", json!({"refresh_token": token}), None)
        .await;
    let revoked = h
        .internal
        .calls()
        .iter()
        .any(|(call, _)| call.contains("revoke"));
    let after = h.internal.session(sid).unwrap();
    // (status, code, a revoke call was made, a refresh call was made, the session is revoked)
    let got = (
        a.status,
        code(&a),
        revoked,
        refresh_calls() > 0,
        after.revoked,
    );
    let want = refresh_model(p, o, i);
    let want_tuple = (
        want.status,
        want.code,
        want.revoke,
        want.refresh,
        want.revoke,
    );
    if got != want_tuple {
        return Some(format!(
            "{p:?} x {o:?} x {i:?} x {t:?}: got {got:?}, want {want_tuple:?}: {}",
            a.body
        ));
    }
    if !want.refresh && after.token != current {
        return Some(format!(
            "{p:?} x {o:?} x {i:?} x {t:?}: rotated without a refresh call"
        ));
    }
    // Only a peek verdict followed by a rotation (an orchestrator that contradicts itself)
    // rotates before the session check; its token is the previous one now and may not outlive
    // the grace. A grace token whose grace ended during the lookup is past it on any retry.
    if want.status != StatusCode::SERVICE_UNAVAILABLE
        || (want.refresh && o == O::Rotated)
        || (grace && t == Timing::GraceEndsInLookup)
    {
        return None;
    }
    // The faults are over; a client that held a current token retries after the grace.
    h.proxy.clear();
    h.internal.clear_answers();
    if !grace {
        h.internal.end_grace();
    }
    let healthy = match i {
        I::Live | I::Banned | I::SessionGone | I::UserGone | I::OtherUser => i,
        _ => I::Live,
    };
    let natural = if grace { O::Grace } else { O::Rotated };
    let want = refresh_model(Peek::Live, natural, healthy);
    let b = h
        .post("refresh", json!({"refresh_token": token}), None)
        .await;
    let got = (b.status, code(&b), h.internal.session(sid).unwrap().revoked);
    let want = (want.status, want.code, want.revoke);
    (got != want).then(|| {
        format!(
            "{p:?} x {o:?} x {i:?} x {t:?}, retried later: got {got:?}, want {want:?}: {}",
            b.body
        )
    })
}

/// #57, #59, #123: every peek answer against every refresh answer against every identity
/// answer (8 x 8 x 13), checked against [`refresh_model`], each 503 followed by a later retry;
/// and again for a grace token with its grace ending during the session lookup (8 x 13), 936
/// cases in all.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn refresh_ends_a_session_only_on_a_definitive_verdict() {
    use Ident as I;
    use Orch as O;
    let peeks = [
        Peek::Live,
        Peek::Invalid401,
        Peek::Reused401,
        Peek::Bare401,
        Peek::NotFound404,
        Peek::Unprocessable422,
        Peek::Internal500,
        Peek::Empty200,
    ];
    let orchs = [
        O::Rotated,
        O::Grace,
        O::Invalid401,
        O::Reused401,
        O::Bare401,
        O::NotFound404,
        O::Unprocessable422,
        O::Internal500,
    ];
    let idents = [
        I::Live,
        I::Banned,
        I::SessionGone,
        I::UserGone,
        I::OtherUser,
        I::BannedInAnswer,
        I::PoolGone404,
        I::Html404,
        I::Route404,
        I::Forbidden403,
        I::Internal500,
        I::Empty200,
        I::Malformed200,
    ];
    let total = peeks.len() * idents.len() * (orchs.len() + 1);
    // One task per peek answer; each runs its cases in turn.
    let tasks: Vec<_> = peeks
        .into_iter()
        .map(|p| {
            tokio::spawn(async move {
                let mut wrong = Vec::new();
                for o in orchs {
                    for i in idents {
                        wrong.extend(refresh_case(p, o, i, Timing::Steady).await);
                    }
                }
                for i in idents {
                    wrong.extend(refresh_case(p, O::Grace, i, Timing::GraceEndsInLookup).await);
                }
                wrong
            })
        })
        .collect();
    let mut wrong = Vec::new();
    for task in tasks {
        wrong.extend(task.await.unwrap());
    }
    assert!(
        wrong.is_empty(),
        "{} of {total} cases differ from the model:\n{}",
        wrong.len(),
        wrong.join("\n")
    );
}

/// #59: identity is down for a minute while a user's access token expires. The refreshes
/// during the outage answer 503 and rotate nothing, so the token the client holds still works
/// after it, long past the 10 s grace.
#[tokio::test]
async fn refresh_survives_an_identity_outage() {
    let h = harness().await;
    h.add_user("ada@example.com", true).await;
    let s = h.sign_in("ada@example.com", PW).await;
    let sid = h.session_id(&s);
    let token = s.body["refresh_token"].clone();
    let held = h.internal.session(sid).unwrap().token;
    h.fake.set_down(true);
    for _ in 0..3 {
        let a = h
            .post("refresh", json!({"refresh_token": token}), None)
            .await;
        assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE, "{}", a.body);
        assert_eq!(code(&a), "unavailable");
        // The orchestrator saw a peek but no rotation.
        assert_eq!(h.internal.count("POST /internal/app/sessions/refresh"), 0);
        assert_eq!(
            h.internal.session(sid).unwrap().token,
            held,
            "nothing rotated"
        );
        h.internal.end_grace();
    }
    assert_eq!(h.internal.count("POST /internal/app/sessions/peek"), 3);
    h.fake.set_down(false);
    let a = h
        .post("refresh", json!({"refresh_token": token}), None)
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_ne!(a.body["refresh_token"], token);
    assert!(!h.internal.session(sid).unwrap().revoked);
}

/// AC 4: an admin bans the user (m7): identity ends their sessions, and the `app_sessions` row
/// survives. Its refresh is refused before anything rotates, and the row is revoked.
#[tokio::test]
async fn refresh_refused_after_identity_ban() {
    let h = harness().await;
    let user = h.add_user("ada@example.com", true).await;
    let s = h.sign_in("ada@example.com", PW).await;
    let sid = h.session_id(&s);
    h.fake.ban(&h.pool(), &user);
    assert!(!h.internal.session(sid).unwrap().revoked, "the row survived the ban");
    let a = h
        .post("refresh", json!({"refresh_token": s.body["refresh_token"]}), None)
        .await;
    assert_eq!(
        (a.status, code(&a)),
        (StatusCode::FORBIDDEN, "user_inactive"),
        "{}",
        a.body
    );
    assert!(h.internal.session(sid).unwrap().revoked);
    assert_eq!(h.internal.count("POST /internal/app/sessions/refresh"), 0);
}

/// The ban-race criterion: a sign-in in flight while the user is banned leaves an identity
/// session made after the ban's sweep and an app session after the metadata revocation. The
/// refresh is refused, the app session revoked, and the identity session gone.
#[tokio::test]
async fn refresh_refused_for_session_created_after_ban() {
    let h = harness().await;
    let user = h.add_user("ada@example.com", true).await;
    h.fake.ban(&h.pool(), &user);
    // What the in-flight sign-in leaves behind, both written after the ban.
    let (raced, _) = h.fake.with_session(&h.pool(), &user);
    let (sid, token) = h.add_app_session(h.project, user.parse().unwrap(), &raced, false);
    let a = h.post("refresh", json!({"refresh_token": token}), None).await;
    assert_eq!(
        (a.status, code(&a)),
        (StatusCode::FORBIDDEN, "user_inactive"),
        "{}",
        a.body
    );
    assert!(h.internal.session(sid).unwrap().revoked);
    assert_eq!(h.fake.session_count(&h.pool()), 0, "the raced identity session is gone");
    assert_eq!(h.internal.count("POST /internal/app/sessions/refresh"), 0);
}

/// An app session whose identity session belongs to another user, or lives in another
/// project's pool, is refused and revoked.
#[tokio::test]
async fn refresh_refused_on_user_or_pool_mismatch() {
    let h = harness().await;
    let ada: Uuid = h.add_user("ada@example.com", true).await.parse().unwrap();
    let bob = h.add_user("bob@example.com", true).await;
    // Bob's session, recorded as Ada's.
    let (bobs, _) = h.fake.with_session(&h.pool(), &bob);
    let (sid, token) = h.add_app_session(h.project, ada, &bobs, false);
    let a = h.post("refresh", json!({"refresh_token": token}), None).await;
    assert_eq!(
        (a.status, code(&a)),
        (StatusCode::UNAUTHORIZED, "refresh_token_invalid"),
        "{}",
        a.body
    );
    assert!(h.internal.session(sid).unwrap().revoked);
    // Ada's live session in project B's pool, recorded for project A: A's pool has no such
    // session, whatever B has.
    let pool_b = Pool::App(Uuid::new_v4());
    let ada_in_b = h.add_user_in(pool_b, "ada@example.com", PW, true).await;
    let (in_b, _) = h.fake.with_session(&pool_b, &ada_in_b);
    let (sid, token) = h.add_app_session(h.project, ada, &in_b, false);
    let a = h.post("refresh", json!({"refresh_token": token}), None).await;
    assert_eq!(
        (a.status, code(&a)),
        (StatusCode::UNAUTHORIZED, "refresh_token_invalid"),
        "{}",
        a.body
    );
    assert!(h.internal.session(sid).unwrap().revoked);
    assert!(
        !h.proxy.calls().iter().any(|c| c.contains(&pool_b.to_string())),
        "the lookup stays in A's pool: {:?}",
        h.proxy.calls()
    );
    assert_eq!(h.fake.session_count(&pool_b), 1, "B's session is untouched");
}

/// #59: a deactivated (banned) user is refused, and the check comes before any rotation;
/// a session revoked in identity is invalid, and the user's other sessions are untouched.
#[tokio::test]
async fn refresh_refuses_a_session_identity_ended() {
    let h = harness().await;
    h.add_user("ada@example.com", true).await;
    let s1 = h.sign_in("ada@example.com", PW).await;
    let s2 = h.sign_in("ada@example.com", PW).await;
    let (sid1, sid2) = (h.session_id(&s1), h.session_id(&s2));
    h.fake
        .client()
        .revoke_session(&h.pool(), &h.identity_session(sid1))
        .await
        .unwrap();
    let a = h
        .post("refresh", json!({"refresh_token": s1.body["refresh_token"]}), None)
        .await;
    assert_eq!(
        (a.status, code(&a)),
        (StatusCode::UNAUTHORIZED, "refresh_token_invalid"),
        "{}",
        a.body
    );
    assert!(h.internal.session(sid1).unwrap().revoked);
    let b = h
        .post("refresh", json!({"refresh_token": s2.body["refresh_token"]}), None)
        .await;
    assert_eq!(b.status, StatusCode::OK, "{}", b.body);
    assert!(!h.internal.session(sid2).unwrap().revoked);
}

/// #123: the session is gone and the follow-up user read fails: the session's end is still
/// definitive (revoked, 401), only the code can't say whether a ban did it.
#[tokio::test]
async fn refresh_of_a_gone_session_is_invalid_even_if_the_user_read_fails() {
    let h = harness().await;
    let user = h.add_user("ada@example.com", true).await;
    let s = h.sign_in("ada@example.com", PW).await;
    let sid = h.session_id(&s);
    h.fake.client().revoke_session(&h.pool(), &h.identity_session(sid)).await.unwrap();
    h.proxy.refuse(&format!("GET {}/users/{user}", h.pool_path()), 500, "internal", 1);
    let a = h
        .post("refresh", json!({"refresh_token": s.body["refresh_token"]}), None)
        .await;
    assert_eq!(
        (a.status, code(&a)),
        (StatusCode::UNAUTHORIZED, "refresh_token_invalid"),
        "{}",
        a.body
    );
    assert!(h.internal.session(sid).unwrap().revoked);
}

/// #59: an identity fault after peek found the session leaves the session's current token as
/// it was, for every kind of fault; a later refresh with it still works.
#[tokio::test]
async fn refresh_identity_fault_rotates_nothing() {
    let h = harness().await;
    h.add_user("ada@example.com", true).await;
    let s = h.sign_in("ada@example.com", PW).await;
    let sid = h.session_id(&s);
    let lookup = format!("GET {}/sessions/{}", h.pool_path(), h.identity_session(sid));
    let token = s.body["refresh_token"].clone();
    let held = h.internal.session(sid).unwrap().token;
    h.proxy.refuse(&lookup, 500, "internal", 1);
    h.proxy.answer_raw(&lookup, 200, "", 1);
    h.proxy.answer_raw(&lookup, 404, "<html>not found</html>", 1);
    for _ in 0..3 {
        let a = h
            .post("refresh", json!({"refresh_token": token}), None)
            .await;
        assert_eq!(
            (a.status, code(&a)),
            (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
            "{}",
            a.body
        );
    }
    assert_eq!(h.internal.count("POST /internal/app/sessions/peek"), 3);
    assert_eq!(h.internal.count("POST /internal/app/sessions/refresh"), 0);
    assert_eq!(h.internal.session(sid).unwrap().token, held);
    h.internal.end_grace();
    let a = h
        .post("refresh", json!({"refresh_token": token}), None)
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
}

/// #59 audit B1: two parallel requests share a cookie; one rotated the token and the other
/// presents the previous one inside the grace. The grace ends while identity answers the
/// session lookup; the grace token was answered before that, so the session lives. A current
/// token doesn't care when the old grace ends.
#[tokio::test]
async fn refresh_survives_the_grace_ending_during_the_session_lookup() {
    let h = harness().await;
    h.add_user("ada@example.com", true).await;
    let s = h.sign_in("ada@example.com", PW).await;
    let sid = h.session_id(&s);
    let t0 = s.body["refresh_token"].clone();
    let a = h.post("refresh", json!({"refresh_token": t0}), None).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    let internal = h.internal.clone();
    h.proxy
        .on_request(&format!("GET {}/sessions/", h.pool_path()), move || internal.end_grace());
    let b = h.post("refresh", json!({"refresh_token": t0}), None).await;
    assert_eq!(b.status, StatusCode::OK, "{}", b.body);
    assert_eq!(
        b.body["refresh_token"], a.body["refresh_token"],
        "the grace answer"
    );
    assert!(!h.internal.session(sid).unwrap().revoked);
    // The current token, with the grace ending during its lookup too.
    let c = h
        .post(
            "refresh",
            json!({"refresh_token": a.body["refresh_token"]}),
            None,
        )
        .await;
    assert_eq!(c.status, StatusCode::OK, "{}", c.body);
    assert_ne!(c.body["refresh_token"], a.body["refresh_token"]);
    assert!(!h.internal.session(sid).unwrap().revoked);
}

/// The number of identity session lookups so far.
fn session_lookups(h: &H) -> usize {
    let prefix = format!("GET {}/sessions/", h.pool_path());
    h.proxy.calls().iter().filter(|c| c.starts_with(&prefix)).count()
}

/// #59 second audit S1: peek finds the token current, then a parallel request rotates the
/// session while identity answers this request's lookup. When this request reaches `refresh`
/// its token is the previous one, inside the grace: it gets the grace answer (the parallel
/// request's successor), and the session is looked up only once.
#[tokio::test]
async fn refresh_of_a_current_token_rotated_during_the_lookup_gets_the_grace_answer() {
    let h = harness().await;
    h.add_user("ada@example.com", true).await;
    let s = h.sign_in("ada@example.com", PW).await;
    let sid = h.session_id(&s);
    let t0 = s.body["refresh_token"].clone();
    let internal = h.internal.clone();
    let parallel = Arc::new(Mutex::new(None::<String>));
    let seen = parallel.clone();
    h.proxy.on_request(&format!("GET {}/sessions/", h.pool_path()), move || {
        let mut seen = seen.lock().unwrap();
        if seen.is_none() {
            *seen = Some(internal.rotate(sid));
        }
    });
    let before = session_lookups(&h);
    let b = h.post("refresh", json!({"refresh_token": t0}), None).await;
    assert_eq!(b.status, StatusCode::OK, "{}", b.body);
    let parallel = parallel.lock().unwrap().clone().expect("the hook rotated");
    assert_eq!(
        b.body["refresh_token"].as_str(),
        Some(parallel.as_str()),
        "the grace answer: the parallel request's successor"
    );
    assert_eq!(session_lookups(&h) - before, 1, "one session lookup");
    assert_eq!(
        h.internal.session(sid).unwrap().token,
        parallel,
        "nothing else rotated"
    );
    assert!(!h.internal.session(sid).unwrap().revoked);
}

/// #59 second audit S1, the accepted residual: as above, but the parallel rotation's grace
/// also ends before this request reaches `refresh`. Its token is reuse now: 401
/// `refresh_token_reused`, and the orchestrator revokes the session.
#[tokio::test]
async fn refresh_of_a_current_token_rotated_and_past_its_grace_during_the_lookup_is_reuse() {
    let h = harness().await;
    h.add_user("ada@example.com", true).await;
    let s = h.sign_in("ada@example.com", PW).await;
    let sid = h.session_id(&s);
    let t0 = s.body["refresh_token"].clone();
    let internal = h.internal.clone();
    let rotated = Arc::new(Mutex::new(false));
    let seen = rotated.clone();
    h.proxy.on_request(&format!("GET {}/sessions/", h.pool_path()), move || {
        let mut seen = seen.lock().unwrap();
        if !*seen {
            internal.rotate(sid);
            internal.end_grace();
            *seen = true;
        }
    });
    let before = session_lookups(&h);
    let b = h.post("refresh", json!({"refresh_token": t0}), None).await;
    assert!(*rotated.lock().unwrap(), "the hook rotated");
    assert_eq!(
        (b.status, code(&b)),
        (StatusCode::UNAUTHORIZED, "refresh_token_reused"),
        "{}",
        b.body
    );
    assert_eq!(session_lookups(&h) - before, 1, "one session lookup");
    assert!(h.internal.session(sid).unwrap().revoked);
}

/// #59 audit B1: a session lookup slower than the refresh bound is a fault: 503, and the
/// token the client holds is still current.
#[tokio::test]
async fn refresh_session_lookup_slower_than_the_bound_is_unavailable() {
    let h = harness_lookup(Duration::from_millis(100)).await;
    h.add_user("ada@example.com", true).await;
    let s = h.sign_in("ada@example.com", PW).await;
    let sid = h.session_id(&s);
    let token = s.body["refresh_token"].clone();
    let held = h.internal.session(sid).unwrap().token;
    h.proxy.delay(&format!("GET {}/sessions/", h.pool_path()), Duration::from_millis(400));
    let a = h
        .post("refresh", json!({"refresh_token": token}), None)
        .await;
    assert_eq!(
        (a.status, code(&a)),
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        "{}",
        a.body
    );
    assert_eq!(h.internal.count("POST /internal/app/sessions/refresh"), 0);
    assert_eq!(
        h.internal.session(sid).unwrap().token,
        held,
        "nothing rotated"
    );
    assert!(!h.internal.session(sid).unwrap().revoked);
    h.proxy.clear();
    let b = h
        .post("refresh", json!({"refresh_token": token}), None)
        .await;
    assert_eq!(b.status, StatusCode::OK, "{}", b.body);
}

#[tokio::test]
async fn refresh_reads_current_email() {
    let h = harness().await;
    let id = h.add_user("ada@example.com", true).await;
    let s = h.sign_in("ada@example.com", PW).await;
    h.fake
        .client()
        .set_email(&h.pool(), &id, "ada@new.example", true)
        .await
        .unwrap();
    let a = h
        .post(
            "refresh",
            json!({"refresh_token": s.body["refresh_token"]}),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(a.body["user"]["email"], "ada@new.example");
    assert_ne!(a.body["refresh_token"], s.body["refresh_token"]);
    let c = h.claims(access(&a));
    assert_eq!(c.email, "ada@new.example");
    assert_eq!(c.sid, h.session_id(&s));
    // A banned user can't refresh, and their session ends.
    h.fake.ban(&h.pool(), &id);
    let b = h
        .post(
            "refresh",
            json!({"refresh_token": a.body["refresh_token"]}),
            None,
        )
        .await;
    assert_eq!(b.status, StatusCode::FORBIDDEN, "{}", b.body);
    assert_eq!(code(&b), "user_inactive");
    assert!(h.internal.session(c.sid).unwrap().revoked);
}

#[tokio::test]
async fn sign_out_by_header_and_by_refresh_token() {
    let h = harness().await;
    h.add_user("ada@example.com", true).await;
    let s1 = h.sign_in("ada@example.com", PW).await;
    let s2 = h.sign_in("ada@example.com", PW).await;
    let a = h.post("sign-out", json!({}), Some(access(&s1))).await;
    assert_eq!(a.status, StatusCode::NO_CONTENT, "{}", a.body);
    assert!(h.internal.session(h.session_id(&s1)).unwrap().revoked);
    assert!(!h.internal.session(h.session_id(&s2)).unwrap().revoked);
    let a = h
        .post(
            "sign-out",
            json!({"refresh_token": s2.body["refresh_token"]}),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::NO_CONTENT, "{}", a.body);
    assert!(h.internal.session(h.session_id(&s2)).unwrap().revoked);
    let a = h.post("sign-out", json!({}), None).await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    assert_eq!(code(&a), "user_token_invalid");
}

#[tokio::test]
async fn user_needs_a_user_token() {
    let h = harness().await;
    let id = h.add_user("ada@example.com", true).await;
    let a = h.get("user", None).await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    assert_eq!(code(&a), "user_token_invalid");
    let s = h.sign_in("ada@example.com", PW).await;
    let a = h.get("user", Some(access(&s))).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    let created = h.user(&id).await.unwrap().created_at;
    assert_eq!(
        a.body,
        json!({"id": id, "email": "ada@example.com", "email_verified": true,
            "created_at": created})
    );
    // A user identity no longer has.
    h.fake.client().delete_user(&h.pool(), &id).await.unwrap();
    let a = h.get("user", Some(access(&s))).await;
    assert_eq!((a.status, code(&a)), (StatusCode::UNAUTHORIZED, "user_token_invalid"), "{}", a.body);
}

// ---- passwords and email ---------------------------------------------------------------------

#[tokio::test]
async fn password_reset_always_204() {
    let h = harness().await;
    let id = h.add_user("ada@example.com", true).await;
    for email in ["ada@example.com", "zed@example.com"] {
        let a = h
            .post("password-reset", json!({"email": email}), None)
            .await;
        assert_eq!(a.status, StatusCode::NO_CONTENT, "{email}");
        assert_eq!(a.body, Value::Null);
    }
    let resets: Vec<String> = h.codes(CodeKind::Reset).into_iter().map(|(u, _)| u).collect();
    assert_eq!(resets, vec![id.clone()]);
    for email in ["ada@example.com", "zed@example.com"] {
        let a = h.post("resend-code", json!({"email": email}), None).await;
        assert_eq!(a.status, StatusCode::NO_CONTENT, "{email}");
    }
    // Ada is verified: nothing to resend.
    assert!(h.codes(CodeKind::Verify).is_empty());
    // Identity down, or no email configured: the same 204.
    h.fake.set_email_configured(false);
    let a = h.post("password-reset", json!({"email": "ada@example.com"}), None).await;
    assert_eq!(a.status, StatusCode::NO_CONTENT);
    h.fake.set_down(true);
    let a = h.post("password-reset", json!({"email": "ada@example.com"}), None).await;
    assert_eq!(a.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn password_reset_confirm_revokes_all() {
    let h = harness().await;
    let id = h.add_user("ada@example.com", true).await;
    let s1 = h.sign_in("ada@example.com", PW).await;
    let s2 = h.sign_in("ada@example.com", PW).await;
    let a = h.post("password-reset", json!({"email": "ada@example.com"}), None).await;
    assert_eq!(a.status, StatusCode::NO_CONTENT);
    let sent = h.last_code(&id, CodeKind::Reset);
    let weak = h
        .post(
            "password-reset/confirm",
            json!({"email": "ada@example.com", "code": sent, "password": "weak"}),
            None,
        )
        .await;
    assert_eq!(weak.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code(&weak), "weak_password");
    let wrong = if sent == "000000" { "111111" } else { "000000" };
    for (email, guess) in [("ada@example.com", wrong), ("zed@example.com", sent.as_str())] {
        let bad = h
            .post(
                "password-reset/confirm",
                json!({"email": email, "code": guess, "password": NEW_PW}),
                None,
            )
            .await;
        assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY, "{email}");
        assert_eq!(code(&bad), "invalid_code", "{email}");
    }
    let ok = h
        .post(
            "password-reset/confirm",
            json!({"email": "ada@example.com", "code": sent, "password": NEW_PW}),
            None,
        )
        .await;
    assert_eq!(ok.status, StatusCode::NO_CONTENT, "{}", ok.body);
    assert!(h.password_is("ada@example.com", NEW_PW).await);
    let revokes = h
        .internal
        .bodies(&format!("POST /internal/app/users/{id}/revoke-sessions"));
    assert_eq!(revokes, vec![json!({"project_id": h.project})]);
    for s in [&s1, &s2] {
        assert!(h.internal.session(h.session_id(s)).unwrap().revoked);
    }
    assert_eq!(h.fake.session_count(&h.pool()), 0, "identity ended its sessions too");
}

/// Reset confirm, partial failure: the code is spent and the password set, then the metadata
/// revocation fails. The answer is still 204: identity ended the user's sessions with the
/// reset, so every old app session is refused at its next refresh.
#[tokio::test]
async fn reset_confirm_holds_when_the_metadata_revoke_fails() {
    let h = harness().await;
    let id = h.add_user("ada@example.com", true).await;
    let s = h.sign_in("ada@example.com", PW).await;
    h.post("password-reset", json!({"email": "ada@example.com"}), None).await;
    let sent = h.last_code(&id, CodeKind::Reset);
    h.iproxy.answer_raw(&format!("POST /internal/app/users/{id}/revoke-sessions"), 503, "", 1);
    let ok = h
        .post(
            "password-reset/confirm",
            json!({"email": "ada@example.com", "code": sent, "password": NEW_PW}),
            None,
        )
        .await;
    assert_eq!(ok.status, StatusCode::NO_CONTENT, "{}", ok.body);
    let sid = h.session_id(&s);
    assert!(!h.internal.session(sid).unwrap().revoked, "the metadata revoke failed");
    let a = h
        .post("refresh", json!({"refresh_token": s.body["refresh_token"]}), None)
        .await;
    assert_eq!(
        (a.status, code(&a)),
        (StatusCode::UNAUTHORIZED, "refresh_token_invalid"),
        "{}",
        a.body
    );
    assert!(h.internal.session(sid).unwrap().revoked);
}

#[tokio::test]
async fn change_password_returns_a_new_session_and_revokes_others() {
    let h = harness().await;
    let id = h.add_user("ada@example.com", true).await;
    let s1 = h.sign_in("ada@example.com", PW).await;
    let s2 = h.sign_in("ada@example.com", PW).await;
    let wrong = h
        .post(
            "change-password",
            json!({"current_password": "Wr0ng!pass", "password": NEW_PW}),
            Some(access(&s1)),
        )
        .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED, "{}", wrong.body);
    assert_eq!(code(&wrong), "invalid_credentials");
    assert!(h.password_is("ada@example.com", PW).await);
    let a = h
        .post(
            "change-password",
            json!({"current_password": PW, "password": NEW_PW}),
            Some(access(&s1)),
        )
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    let s3 = h.session_id(&a);
    assert!(h.password_is("ada@example.com", NEW_PW).await);
    let revokes = h
        .internal
        .bodies(&format!("POST /internal/app/users/{id}/revoke-sessions"));
    assert_eq!(revokes, vec![json!({"project_id": h.project})]);
    assert!(h.internal.session(h.session_id(&s1)).unwrap().revoked);
    assert!(h.internal.session(h.session_id(&s2)).unwrap().revoked);
    assert!(!h.internal.session(s3).unwrap().revoked);
    // Only the new session's identity session is live.
    assert_eq!(h.fake.session_count(&h.pool()), 1);
    assert!(h.identity_session_live(&h.identity_session(s3)).await);
    // Without a user token.
    let a = h
        .post(
            "change-password",
            json!({"current_password": PW, "password": NEW_PW}),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    assert_eq!(code(&a), "user_token_invalid");
}

/// AC 3: after a password change on one device, another device's refresh is refused, even when
/// one of the two revocations failed. Only when both fail is the answer 503.
#[tokio::test]
async fn change_password_revokes_other_sessions() {
    for case in ["both", "identity revoke fails", "metadata revoke fails", "both fail"] {
        let h = harness().await;
        let id = h.add_user("ada@example.com", true).await;
        let me = h.sign_in("ada@example.com", PW).await;
        let other = h.sign_in("ada@example.com", PW).await;
        let other_sid = h.session_id(&other);
        let identity_revoke = format!("DELETE {}/users/{id}/sessions", h.pool_path());
        if matches!(case, "identity revoke fails" | "both fail") {
            h.proxy.refuse(&identity_revoke, 503, "internal", 1);
        }
        if matches!(case, "metadata revoke fails" | "both fail") {
            h.iproxy.answer_raw(&format!("POST /internal/app/users/{id}/revoke-sessions"), 503, "", 1);
        }
        let a = h
            .post(
                "change-password",
                json!({"current_password": PW, "password": NEW_PW}),
                Some(access(&me)),
            )
            .await;
        h.proxy.clear();
        h.iproxy.clear();
        if case == "both fail" {
            assert_eq!((a.status, code(&a)), (StatusCode::SERVICE_UNAVAILABLE, "unavailable"), "{case}: {}", a.body);
            continue;
        }
        assert_eq!(a.status, StatusCode::OK, "{case}: {}", a.body);
        let r = h
            .post("refresh", json!({"refresh_token": other.body["refresh_token"]}), None)
            .await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{case}: {}", r.body);
        assert!(h.internal.session(other_sid).unwrap().revoked, "{case}");
        // The new session refreshes.
        let r = h
            .post("refresh", json!({"refresh_token": a.body["refresh_token"]}), None)
            .await;
        assert_eq!(r.status, StatusCode::OK, "{case}: {}", r.body);
    }
}

/// Change-password, partial failure before the change: the password stays, nothing is revoked,
/// and a retry completes it.
#[tokio::test]
async fn change_password_failing_before_the_change_changes_nothing_and_a_retry_works() {
    let h = harness().await;
    let id = h.add_user("ada@example.com", true).await;
    let me = h.sign_in("ada@example.com", PW).await;
    let other = h.sign_in("ada@example.com", PW).await;
    h.proxy.refuse(&format!("PUT {}/users/{id}/password", h.pool_path()), 503, "internal", 1);
    let body = json!({"current_password": PW, "password": NEW_PW});
    let a = h.post("change-password", body.clone(), Some(access(&me))).await;
    assert_eq!((a.status, code(&a)), (StatusCode::SERVICE_UNAVAILABLE, "unavailable"), "{}", a.body);
    assert!(h.password_is("ada@example.com", PW).await);
    assert!(!h.internal.session(h.session_id(&other)).unwrap().revoked);
    assert!(h.internal.bodies(&format!("POST /internal/app/users/{id}/revoke-sessions")).is_empty());
    // The password check's identity session was ended; the two signed-in ones are left.
    assert_eq!(h.fake.session_count(&h.pool()), 2);
    let a = h.post("change-password", body, Some(access(&me))).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert!(h.password_is("ada@example.com", NEW_PW).await);
    assert!(h.internal.session(h.session_id(&other)).unwrap().revoked);
}

#[tokio::test]
async fn change_email_syncs() {
    let h = harness().await;
    let id = h.add_user("ada@example.com", true).await;
    let s = h.sign_in("ada@example.com", PW).await;
    let a = h
        .post(
            "change-email",
            json!({"email": "Ada@New.example"}),
            Some(access(&s)),
        )
        .await;
    assert_eq!(a.status, StatusCode::NO_CONTENT, "{}", a.body);
    let u = h.user(&id).await.unwrap();
    assert_eq!(u.email, "ada@new.example");
    assert!(u.email_verified, "verification is off: the new email is verified");
    assert!(h.codes(CodeKind::Verify).is_empty());
    let sync = h.internal.bodies("POST /internal/app/sync");
    assert_eq!(sync, vec![json!({"project_id": h.project, "user_id": id})]);
    // The same address again is not a change.
    let a = h
        .post("change-email", json!({"email": "ada@new.example"}), Some(access(&s)))
        .await;
    assert_eq!((a.status, code(&a)), (StatusCode::CONFLICT, "conflict"), "{}", a.body);
}

/// With verification on, the new email is unverified and gets a code; verifying it marks it.
#[tokio::test]
async fn change_email_resets_verification() {
    let h = harness_opts(true, 1, true).await;
    let id = h.add_user("ada@example.com", true).await;
    let s = h.sign_in("ada@example.com", PW).await;
    let a = h
        .post("change-email", json!({"email": "ada@new.example"}), Some(access(&s)))
        .await;
    assert_eq!(a.status, StatusCode::NO_CONTENT, "{}", a.body);
    let u = h.user(&id).await.unwrap();
    assert_eq!(u.email, "ada@new.example");
    assert!(!u.email_verified, "the new address is unverified");
    let sent = h.last_code(&id, CodeKind::Verify);
    let a = h
        .post("verify-email", json!({"email": "ada@new.example", "code": sent}), None)
        .await;
    assert_eq!(a.status, StatusCode::NO_CONTENT, "{}", a.body);
    assert!(h.user(&id).await.unwrap().email_verified);
}

#[tokio::test]
async fn change_email_taken_409() {
    let h = harness().await;
    h.add_user("ada@example.com", true).await;
    h.add_user("bob@example.com", true).await;
    let s = h.sign_in("ada@example.com", PW).await;
    let a = h
        .post(
            "change-email",
            json!({"email": "BOB@example.com"}),
            Some(access(&s)),
        )
        .await;
    assert_eq!(a.status, StatusCode::CONFLICT, "{}", a.body);
    assert_eq!(code(&a), "email_taken");
    // "bob@example.com" contains "ob@example.com": only an exact match is taken.
    let a = h
        .post("change-email", json!({"email": "ob@example.com"}), Some(access(&s)))
        .await;
    assert_eq!(a.status, StatusCode::NO_CONTENT, "{}", a.body);
    let a = h
        .post("change-email", json!({"email": "x@example.com"}), None)
        .await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    assert_eq!(code(&a), "user_token_invalid");
}

/// #123: change-email reads identity's refusals: a taken address (a race past the lookup), a
/// user gone, its email check; anything else is unavailable, and nothing is synced.
#[tokio::test]
async fn change_email_maps_identity_refusals() {
    let h = harness().await;
    // Each case as its own user: change-email allows a user five an hour.
    let mut n = 0;
    let mut change = async |refusal: Option<(u16, &str)>| {
        n += 1;
        let own = format!("u{n}@example.com");
        let id = h.add_user(&own, true).await;
        let s = h.sign_in(&own, PW).await;
        if let Some((status, code)) = refusal {
            h.proxy.refuse(&format!("PUT {}/users/{id}/email", h.pool_path()), status, code, 1);
        }
        h.post("change-email", json!({"email": "new@example.com"}), Some(access(&s))).await
    };
    let a = change(Some((409, "conflict"))).await;
    assert_eq!((a.status, code(&a)), (StatusCode::CONFLICT, "email_taken"), "{}", a.body);
    let a = change(Some((404, "user_not_found"))).await;
    assert_eq!((a.status, code(&a)), (StatusCode::UNAUTHORIZED, "user_token_invalid"), "{}", a.body);
    let a = change(Some((400, "invalid_email"))).await;
    assert_eq!((a.status, code(&a)), (StatusCode::UNPROCESSABLE_ENTITY, "invalid_request"), "{}", a.body);
    for (status, refusal) in [(404, "pool_not_found"), (403, "forbidden"), (412, "something_else"), (500, "internal")] {
        let a = change(Some((status, refusal))).await;
        assert_eq!((a.status, code(&a)), (StatusCode::SERVICE_UNAVAILABLE, "unavailable"), "{refusal}: {}", a.body);
    }
    assert!(h.internal.bodies("POST /internal/app/sync").is_empty());
}

/// Codes are limited per user and kind (m2 audit: a new code resets the guess counter, so
/// issuance is what bounds the guesses): whatever mix of flows asks, a user gets at most five
/// codes of a kind an hour.
#[tokio::test]
async fn code_issuance_is_limited_per_user_and_kind() {
    let h = harness_opts(true, 1, true).await;
    let ada = h.add_user("ada@example.com", false).await;
    let s = {
        // Ada can't sign in unverified: mint her a user token for a session made directly.
        let id: Uuid = ada.parse().unwrap();
        let (identity_session, _) = h.fake.with_session(&h.pool(), &ada);
        let (sid, _) = h.add_app_session(h.project, id, &identity_session, false);
        let claims = AppClaims {
            iss: chelabase_auth::app_token::APP_ISSUER.into(),
            aud: chelabase_auth::app_token::app_audience(h.project),
            sub: id,
            sid,
            project: h.project,
            email: "ada@example.com".into(),
            email_verified: false,
            role: "authenticated".into(),
            iat: 0,
            exp: u64::MAX / 2,
        };
        h.signer.sign(&claims).unwrap()
    };
    // Verify codes: three resends (the email allows five), then three email changes (the user
    // allows five): six asks, five codes.
    for _ in 0..3 {
        let a = h.post("resend-code", json!({"email": "ada@example.com"}), None).await;
        assert_eq!(a.status, StatusCode::NO_CONTENT);
    }
    for n in 0..3 {
        let a = h
            .post("change-email", json!({"email": format!("ada{n}@example.com")}), Some(&s))
            .await;
        assert_eq!(a.status, StatusCode::NO_CONTENT, "{}", a.body);
    }
    assert_eq!(h.codes(CodeKind::Verify).iter().filter(|(u, _)| *u == ada).count(), 5);
    // Reset codes: three resets, then three sign-ups for the taken address (each a notice to
    // the owner): six asks, five codes.
    let bob = h.add_user("bob@example.com", true).await;
    for _ in 0..3 {
        h.post("password-reset", json!({"email": "bob@example.com"}), None).await;
    }
    for _ in 0..3 {
        let a = h
            .post("sign-up", json!({"email": "bob@example.com", "password": PW}), None)
            .await;
        assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
    }
    assert_eq!(h.codes(CodeKind::Reset).iter().filter(|(u, _)| *u == bob).count(), 5);
    // Another kind, and another user, have their own five.
    let a = h.post("password-reset", json!({"email": "ada2@example.com"}), None).await;
    assert_eq!(a.status, StatusCode::NO_CONTENT);
    assert_eq!(h.codes(CodeKind::Reset).iter().filter(|(u, _)| *u == ada).count(), 1);
}

// ---- guards ----------------------------------------------------------------------------------

#[tokio::test]
async fn app_users_disabled_409() {
    let h = harness().await;
    h.add_user("ada@example.com", true).await;
    let mut enabling = info(h.project, h.branch, false);
    enabling["status"] = json!("enabling");
    h.internal.set_branch(h.branch, enabling);
    let a = h.sign_in("ada@example.com", PW).await;
    assert_eq!(a.status, StatusCode::CONFLICT, "{}", a.body);
    assert_eq!(code(&a), "app_users_disabled");
    h.internal.inner.lock().unwrap().branches.clear();
    let a = h
        .post(
            "sign-up",
            json!({"email": "bob@example.com", "password": PW}),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::CONFLICT, "{}", a.body);
    assert_eq!(code(&a), "app_users_disabled");
}

/// #123: the branch's app users no longer need a Zitadel org: an enabled project without one
/// signs in (the failing live test, `zitadel_policy_refusals_are_weak_passwords_for_app_users`,
/// was this). A branch of another project is still disabled.
#[tokio::test]
async fn enabled_without_a_zitadel_org_works_and_another_project_does_not() {
    let h = harness().await;
    h.add_user("ada@example.com", true).await;
    assert!(h.internal.inner.lock().unwrap().branches[&h.branch]["zitadel_org_id"].is_null());
    let a = h.sign_in("ada@example.com", PW).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    // A branch whose settings name another project (the key's project is checked).
    let fresh = harness().await;
    fresh.internal.set_branch(fresh.branch, info(Uuid::new_v4(), fresh.branch, false));
    let a = fresh.sign_in("ada@example.com", PW).await;
    assert_eq!((a.status, code(&a)), (StatusCode::CONFLICT, "app_users_disabled"), "{}", a.body);
    assert!(fresh.proxy.calls().is_empty(), "no pool is chosen for another project's branch");
}

#[tokio::test]
async fn secret_key_identity_refused() {
    let h = harness().await;
    h.add_user("ada@example.com", true).await;
    let secret = h.identity_for(None, DataKeyKind::Secret);
    let a = h
        .send(
            h.http
                .post(format!("{}/auth/v1/app/sign-in", h.base))
                .json(&json!({"email": "ada@example.com", "password": PW})),
            Some(secret),
        )
        .await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED, "{}", a.body);
    assert_eq!(code(&a), "unauthorized");
    let none = h
        .send(
            h.http
                .post(format!("{}/auth/v1/app/sign-in", h.base))
                .json(&json!({"email": "ada@example.com", "password": PW})),
            None,
        )
        .await;
    assert_eq!(none.status, StatusCode::UNAUTHORIZED);
    assert_eq!(code(&none), "unauthorized");
    assert!(!h.proxy.calls().iter().any(|c| c.contains("/sessions")));
}

#[tokio::test]
async fn jwks_matches_the_signer() {
    let h = harness().await;
    let a = h
        .send(h.http.get(format!("{}/auth/v1/app/jwks", h.base)), None)
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(a.body, h.signer.jwks());
}

#[tokio::test]
async fn app_routes_503_without_keys() {
    let h = harness_opts(false, 1, false).await;
    let a = h.sign_in("ada@example.com", PW).await;
    assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE, "{}", a.body);
    assert_eq!(code(&a), "unavailable");
    let a = h
        .send(h.http.get(format!("{}/auth/v1/app/jwks", h.base)), None)
        .await;
    assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn a_dropped_sign_up_still_finishes() {
    let h = harness().await;
    h.db.set(Fail::SlowCommit(400));
    // The client gives up while the commit (after identity's create) is in flight.
    let gave_up = h
        .http
        .post(format!("{}/auth/v1/app/sign-up", h.base))
        .timeout(Duration::from_millis(150))
        .header(IDENTITY_HEADER, h.identity_for(None, DataKeyKind::Public))
        .json(&json!({"email": "ada@example.com", "password": PW}))
        .send()
        .await;
    assert!(gave_up.is_err(), "the client timed out");
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let committed = h.db.steps().contains(&"committed".to_string());
        let synced = !h.internal.bodies("POST /internal/app/sync").is_empty();
        let deleted = !h.password_is("ada@example.com", PW).await;
        if h.identity_creates() == 1 && ((committed && synced) || deleted) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "neither committed and synced nor cleaned up: {:?}",
            h.db.steps()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_public_key_with_role_authenticated_is_still_anonymous() {
    let h = harness().await;
    h.add_user("ada@example.com", true).await;
    let identity = h.identity_raw("authenticated", json!({}));
    let a = h
        .send(
            h.http
                .post(format!("{}/auth/v1/app/sign-in", h.base))
                .json(&json!({"email": "ada@example.com", "password": PW})),
            Some(identity),
        )
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    // Without a user, user-only routes say so.
    let identity = h.identity_raw("authenticated", json!({}));
    let a = h
        .send(
            h.http.get(format!("{}/auth/v1/app/user", h.base)),
            Some(identity),
        )
        .await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    assert_eq!(code(&a), "user_token_invalid");
}

#[tokio::test]
async fn change_password_with_an_unverified_email_changes_nothing() {
    let h = harness_opts(true, 1, true).await;
    h.add_user("ada@example.com", true).await;
    let s = h.sign_in("ada@example.com", PW).await;
    assert_eq!(s.status, StatusCode::OK, "{}", s.body);
    let a = h
        .post(
            "change-email",
            json!({"email": "ada@new.example"}),
            Some(access(&s)),
        )
        .await;
    assert_eq!(a.status, StatusCode::NO_CONTENT, "{}", a.body);
    let a = h
        .post(
            "change-password",
            json!({"current_password": PW, "password": NEW_PW}),
            Some(access(&s)),
        )
        .await;
    assert_eq!(a.status, StatusCode::FORBIDDEN, "{}", a.body);
    assert_eq!(code(&a), "email_not_verified");
    assert!(h.password_is("ada@new.example", PW).await, "the password is unchanged");
}

// ---- server-vouched calls (#55) ----------------------------------------------------------

const LIMITED: StatusCode = StatusCode::TOO_MANY_REQUESTS;

/// The statuses of `n` calls made by `call(i)`.
async fn statuses<F: AsyncFn(usize) -> Ans>(n: usize, call: F) -> Vec<StatusCode> {
    let mut out = Vec::new();
    for i in 0..n {
        out.push(call(i).await.status);
    }
    out
}

#[tokio::test]
async fn server_vouched_sign_ins_skip_the_ip_limit() {
    let h = harness().await;
    let body = |i: usize| json!({"email": format!("u{i}@example.com"), "password": PW});
    // 11 sign-ins within a minute from one address, each for a different email.
    let got = statuses(11, async |i| h.post_vouched("sign-in", body(i), None).await).await;
    assert!(!got.contains(&LIMITED), "{got:?}");
    // Not vouched (a browser, or an identity without the claim): the 11th is refused.
    let h = harness().await;
    let got = statuses(11, async |i| h.post("sign-in", body(i), None).await).await;
    assert!(!got[..10].contains(&LIMITED), "{got:?}");
    assert_eq!(got[10], LIMITED);
}

#[tokio::test]
async fn server_vouched_sign_in_keeps_the_per_email_failure_limit() {
    let h = harness().await;
    h.add_user("ada@example.com", true).await;
    let wrong = json!({"email": "ada@example.com", "password": "Wr0ng!pass"});
    let got = statuses(6, async |_| h.post_vouched("sign-in", wrong.clone(), None).await).await;
    assert!(got[..5].iter().all(|s| *s == StatusCode::UNAUTHORIZED), "{got:?}");
    assert_eq!(got[5], LIMITED);
}

/// How many of these sign-ups, sent at once (each is padded to about a second), are refused
/// for a limit.
async fn limited_sign_ups(h: &H, emails: &[String], vouched: bool) -> usize {
    let identity = h.identity_opts(None, DataKeyKind::Public, vouched);
    let mut calls = tokio::task::JoinSet::new();
    for email in emails {
        let req = h
            .http
            .post(format!("{}/auth/v1/app/sign-up", h.base))
            .header("x-forwarded-for", "10.1.1.1")
            .header(IDENTITY_HEADER, identity.clone())
            .json(&json!({"email": email, "password": PW}));
        calls.spawn(async move { req.send().await.unwrap().status() });
    }
    calls.join_all().await.into_iter().filter(|s| *s == LIMITED).count()
}

#[tokio::test]
async fn server_vouched_sign_ups_skip_the_ip_limit_but_keep_the_email_one() {
    let h = harness().await;
    let distinct: Vec<String> = (0..21).map(|i| format!("u{i}@example.com")).collect();
    // Sign-up allows 20 an hour per address; 21 different emails all get through.
    assert_eq!(limited_sign_ups(&h, &distinct, true).await, 0);
    // Five an hour per email: one of six for the same email is refused.
    let same = vec!["same@example.com".to_string(); 6];
    assert_eq!(limited_sign_ups(&h, &same, true).await, 1);
    // Not vouched: one of the address's 21 is refused.
    let h = harness().await;
    assert_eq!(limited_sign_ups(&h, &distinct, false).await, 1);
}

#[tokio::test]
async fn server_vouched_refreshes_skip_the_ip_limit() {
    let h = harness().await;
    let refresh = json!({"refresh_token": "nope"});
    // Refresh allows 120 a minute per address.
    let got = statuses(121, async |_| h.post_vouched("refresh", refresh.clone(), None).await).await;
    assert!(!got.contains(&LIMITED), "{got:?}");
    assert!(got.iter().all(|s| *s == StatusCode::UNAUTHORIZED));
    let got = statuses(121, async |_| h.post("refresh", refresh.clone(), None).await).await;
    assert!(!got[..120].contains(&LIMITED));
    assert_eq!(got[120], LIMITED);
}

#[tokio::test]
async fn server_vouched_change_password_keeps_the_per_user_limit() {
    let h = harness().await;
    h.add_user("ada@example.com", true).await;
    let s = h.sign_in("ada@example.com", PW).await;
    let token = access(&s).to_string();
    let wrong = json!({"current_password": "Wr0ng!pass", "password": NEW_PW});
    // Five a minute per user (per address it is ten).
    let got = statuses(6, async |_| h.post_vouched("change-password", wrong.clone(), Some(&token)).await).await;
    assert!(got[..5].iter().all(|s| *s == StatusCode::UNAUTHORIZED), "{got:?}");
    assert_eq!(got[5], LIMITED);
}

// ---- #74: the sign-in email limit under concurrency and hang-ups ------------------------------

impl H {
    /// A sign-in from `ip`, vouched for by the app's server or not.
    async fn sign_in_from(&self, email: &str, password: &str, ip: &str, vouched: bool) -> Ans {
        let identity = self.identity_opts(None, DataKeyKind::Public, vouched);
        self.send_from(
            self.http
                .post(format!("{}/auth/v1/app/sign-in", self.base))
                .json(&json!({"email": email, "password": password})),
            Some(identity),
            ip,
        )
        .await
    }

    /// The identity sign-in call (`POST …/sessions`).
    fn sign_in_call(&self) -> String {
        format!("POST {}/sessions", self.pool_path())
    }

    /// How many password checks reached identity.
    fn password_checks(&self) -> usize {
        let call = self.sign_in_call();
        self.proxy.calls().iter().filter(|c| **c == call).count()
    }
}

/// `n` wrong-password sign-ins for ada at once, each from its own address; (401s, 429s).
async fn wrong_sign_in_burst(h: &Arc<H>, n: usize, vouched: bool) -> (usize, usize) {
    // A slow password check, so every request is in flight before the first answer.
    h.proxy.delay(&h.sign_in_call(), Duration::from_millis(300));
    let mut tasks = Vec::new();
    for i in 0..n {
        let h = h.clone();
        tasks.push(tokio::spawn(async move {
            h.sign_in_from("ada@example.com", "Wr0ng!pass", &format!("10.2.{i}.1"), vouched).await
        }));
    }
    let (mut refused, mut limited) = (0, 0);
    for t in tasks {
        let a = t.await.unwrap();
        match a.status {
            StatusCode::UNAUTHORIZED => refused += 1,
            LIMITED => limited += 1,
            other => panic!("unexpected {other}: {}", a.body),
        }
    }
    h.proxy.clear();
    (refused, limited)
}

/// The budget is whole: five wrong passwords are checked (401), and the sixth is refused.
async fn assert_app_budget_unspent(h: &H, tag: &str) {
    let mut got = Vec::new();
    for i in 0..6 {
        got.push(h.sign_in_from("ada@example.com", "Wr0ng!pass", &format!("10.6.{tag}.{i}"), false).await.status);
    }
    assert!(got[..5].iter().all(|s| *s == StatusCode::UNAUTHORIZED), "{tag}: {got:?}");
    assert_eq!(got[5], LIMITED, "{tag}: {got:?}");
}

// AC 2: 30 wrong passwords at once from 30 addresses get at most 5 password checks.
#[tokio::test(flavor = "multi_thread", worker_threads = 64)]
async fn app_sign_in_email_limit_holds_under_concurrency() {
    let h = Arc::new(harness().await);
    h.add_user("ada@example.com", true).await;
    let (refused, limited) = wrong_sign_in_burst(&h, 30, false).await;
    let checks = h.password_checks();
    assert!(checks <= 5, "{checks} password checks reached identity ({refused} got 401, {limited} got 429)");
    assert_eq!((refused, limited), (5, 25));
}

// AC 4: a server-vouched caller has no address limit; the email limit still holds at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 64)]
async fn server_vouched_sign_in_burst_is_limited() {
    let h = Arc::new(harness().await);
    h.add_user("ada@example.com", true).await;
    let (refused, limited) = wrong_sign_in_burst(&h, 30, true).await;
    let checks = h.password_checks();
    assert!(checks <= 5, "{checks} password checks reached identity ({refused} got 401, {limited} got 429)");
    assert_eq!((refused, limited), (5, 25));
}

// AC 3: a client that hangs up while identity checks the password still spends the attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn app_sign_in_cancelled_password_checks_still_count() {
    let h = harness().await;
    h.add_user("ada@example.com", true).await;
    // Identity holds each password check until the client has hung up.
    let mut hold = h.proxy.hold(&h.sign_in_call());
    let client = reqwest::Client::new();
    for i in 0..5 {
        let send = client
            .post(format!("{}/auth/v1/app/sign-in", h.base))
            .json(&json!({"email": "ada@example.com", "password": "Wr0ng!pass"}))
            .header("x-forwarded-for", format!("10.3.{i}.1"))
            .header(IDENTITY_HEADER, h.identity_for(None, DataKeyKind::Public))
            .send();
        tokio::select! {
            r = send => panic!("attempt {i}: answered before the password check reached identity: {r:?}"),
            () = hold.started() => {}
        }
        // The request was dropped with the select: the client has hung up mid-check.
        hold.release();
    }
    drop(hold);
    h.proxy.clear();
    // A patient client (no timeout).
    let a = h.sign_in_from("ada@example.com", "Wr0ng!pass", "10.3.9.1", false).await;
    assert_eq!(a.status, LIMITED, "{}", a.body);
    assert_eq!(h.password_checks(), 5);
}

// AC 6: the orchestrator down or the branch disabled spend nothing.
#[tokio::test]
async fn app_sign_in_internal_down_or_disabled_does_not_spend() {
    let h = harness().await;
    h.add_user("ada@example.com", true).await;
    let mut disabled = info(h.project, h.branch, false);
    disabled["status"] = json!("enabling");
    h.internal.set_branch(h.branch, disabled);
    for i in 0..6 {
        let a = h.sign_in_from("ada@example.com", "Wr0ng!pass", &format!("10.4.0.{i}"), false).await;
        assert_eq!(a.status, StatusCode::CONFLICT, "disabled {i}: {}", a.body);
    }
    // An answer the client can't read is the orchestrator being unavailable.
    h.internal.set_branch(h.branch, json!("not branch info"));
    for i in 0..6 {
        let a = h.sign_in_from("ada@example.com", "Wr0ng!pass", &format!("10.4.1.{i}"), false).await;
        assert_eq!(a.status, StatusCode::SERVICE_UNAVAILABLE, "internal down {i}: {}", a.body);
    }
    h.internal.set_branch(h.branch, info(h.project, h.branch, false));
    assert_app_budget_unspent(&h, "1").await;
}

// AC 7: correct passwords at once all sign in and spend nothing; nor does a correct password
// whose account then can't sign in (unverified, or banned).
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn app_concurrent_correct_sign_ins_do_not_spend_the_email_budget() {
    let h = Arc::new(harness().await);
    h.add_user("ada@example.com", true).await;
    h.proxy.delay(&h.sign_in_call(), Duration::from_millis(200));
    let mut tasks = Vec::new();
    for i in 0..5 {
        let h = h.clone();
        tasks.push(tokio::spawn(async move {
            h.sign_in_from("ada@example.com", PW, &format!("10.5.{i}.1"), false).await
        }));
    }
    for t in tasks {
        let a = t.await.unwrap();
        assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    }
    h.proxy.clear();
    assert_app_budget_unspent(&h, "2").await;

    let h = harness_opts(true, 1, true).await;
    h.add_user("ada@example.com", false).await;
    for i in 0..6 {
        let a = h.sign_in_from("ada@example.com", PW, &format!("10.5.9.{i}"), false).await;
        assert_eq!(a.status, StatusCode::FORBIDDEN, "unverified {i}: {}", a.body);
    }
    assert_app_budget_unspent(&h, "3").await;

    let h = harness().await;
    let id = h.add_user("ada@example.com", true).await;
    h.fake.ban(&h.pool(), &id);
    for i in 0..6 {
        let a = h.sign_in_from("ada@example.com", PW, &format!("10.5.8.{i}"), false).await;
        assert_eq!(a.status, StatusCode::FORBIDDEN, "banned {i}: {}", a.body);
    }
    assert_app_budget_unspent(&h, "5").await;
}

// AC 8: identity outages spend nothing; its request validation counts like a wrong password.
#[tokio::test]
async fn app_sign_in_outages_do_not_spend_the_email_budget() {
    let h = harness().await;
    h.add_user("ada@example.com", true).await;
    let mut ip = 0;
    let mut sign_in = async |expect: StatusCode, why: &str| {
        ip += 1;
        let a = h.sign_in_from("ada@example.com", "Wr0ng!pass", &format!("10.7.0.{ip}"), false).await;
        assert_eq!(a.status, expect, "{why}: {}", a.body);
    };
    h.fake.set_down(true);
    for _ in 0..6 {
        sign_in(StatusCode::SERVICE_UNAVAILABLE, "down").await;
    }
    h.fake.set_down(false);
    // The password check itself fails: a 5xx, a 4xx that isn't the service's own answer, a
    // missing pool, a forbidden caller.
    let call = h.sign_in_call();
    h.proxy.refuse(&call, 503, "internal", 2);
    h.proxy.answer_raw(&call, 404, "<html>not found</html>", 2);
    h.proxy.refuse(&call, 404, "pool_not_found", 1);
    h.proxy.refuse(&call, 403, "forbidden", 1);
    for _ in 0..6 {
        sign_in(StatusCode::SERVICE_UNAVAILABLE, "check down").await;
    }
    assert_app_budget_unspent(&h, "4").await;

    let h = harness().await;
    h.add_user("ada@example.com", true).await;
    h.proxy.refuse(&h.sign_in_call(), 400, "invalid_request", 5);
    let mut got = Vec::new();
    for i in 0..6 {
        got.push(h.sign_in_from("ada@example.com", "Wr0ng!pass", &format!("10.7.1.{i}"), false).await.status);
    }
    assert!(got[..5].iter().all(|s| *s == StatusCode::UNAUTHORIZED), "{got:?}");
    assert_eq!(got[5], LIMITED, "{got:?}");
}

/// An unknown email spends the budget like a wrong password: otherwise six wrong sign-ins
/// would get a 429 only for real accounts, so anyone could enumerate an app's users.
#[tokio::test]
async fn app_unknown_email_counts_like_a_wrong_password() {
    let h = harness().await;
    let mut got = Vec::new();
    for i in 0..6 {
        got.push(h.sign_in_from("nobody@example.com", "Wr0ng!pass", &format!("10.9.0.{i}"), false).await.status);
    }
    assert!(got[..5].iter().all(|s| *s == StatusCode::UNAUTHORIZED), "{got:?}");
    assert_eq!(got[5], LIMITED, "{got:?}");
}

// ---- password refusals (#61) -----------------------------------------------------------------

// The password rule (72 bytes at most, as before) is checked before identity sees anything, so
// an answer never depends on whether an account exists.
#[tokio::test]
async fn passwords_over_72_bytes_are_refused_before_identity() {
    let h = harness().await;
    let long = format!("Aa1!{}", "é".repeat(35));
    assert_eq!((long.chars().count(), long.len()), (39, 74));
    let a = h
        .post("sign-up", json!({"email": "ada@example.com", "password": long}), None)
        .await;
    assert_eq!(a.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", a.body);
    assert_eq!(code(&a), "weak_password");
    assert_eq!(h.proxy.calls(), Vec::<String>::new(), "identity is never called");
    let id = h.add_user("bob@example.com", true).await;
    h.post("password-reset", json!({"email": "bob@example.com"}), None).await;
    let sent = h.last_code(&id, CodeKind::Reset);
    let a = h
        .post(
            "password-reset/confirm",
            json!({"email": "bob@example.com", "code": sent, "password": long}),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", a.body);
    assert_eq!(code(&a), "weak_password");
    assert!(h.password_is("bob@example.com", PW).await);
    let s = h.sign_in("bob@example.com", PW).await;
    let a = h
        .post("change-password", json!({"current_password": PW, "password": long}), Some(access(&s)))
        .await;
    assert_eq!(a.status, StatusCode::UNPROCESSABLE_ENTITY, "{}", a.body);
    assert_eq!(code(&a), "weak_password");
    assert!(!h.proxy.calls().iter().any(|c| c.ends_with("/password")), "{:?}", h.proxy.calls());
}

// Identity's own password refusal (`weak_password`) is `weak_password` on every path that sets
// one; its other refusals are not. (Retires the Zitadel-policy test, and with it the live
// `zitadel_policy_refusals_are_weak_passwords_for_app_users`.)
#[tokio::test]
async fn identity_password_refusals_are_weak_passwords() {
    let h = harness().await;
    let id = h.add_user("bob@example.com", true).await;
    h.post("password-reset", json!({"email": "bob@example.com"}), None).await;
    let sent = h.last_code(&id, CodeKind::Reset);
    h.proxy.refuse(&format!("POST {}/codes/verify", h.pool_path()), 400, "weak_password", 1);
    let a = h
        .post(
            "password-reset/confirm",
            json!({"email": "bob@example.com", "code": sent, "password": NEW_PW}),
            None,
        )
        .await;
    assert_eq!((a.status, code(&a)), (StatusCode::UNPROCESSABLE_ENTITY, "weak_password"), "{}", a.body);
    let s = h.sign_in("bob@example.com", PW).await;
    h.proxy.refuse(&format!("PUT {}/users/{id}/password", h.pool_path()), 400, "weak_password", 1);
    let a = h
        .post("change-password", json!({"current_password": PW, "password": NEW_PW}), Some(access(&s)))
        .await;
    assert_eq!((a.status, code(&a)), (StatusCode::UNPROCESSABLE_ENTITY, "weak_password"), "{}", a.body);
    assert!(h.password_is("bob@example.com", PW).await);
    assert!(!h.internal.session(h.session_id(&s)).unwrap().revoked, "nothing was revoked");
}

// B1 of the m10 audit: an email lookup on a public route is one exact identity call, never a
// substring search paged to the end. 650 users contain `a@example.com`; none is it. A blank
// address is refused by the shape check before identity is asked at all.
#[tokio::test]
async fn lookup_of_an_unknown_email_is_one_identity_call() {
    let h = harness().await;
    for i in 0..650 {
        h.add_user(&format!("u{i}a@example.com"), true).await;
    }
    let list = format!("GET {}/users", h.pool_path());
    let mut worst = Vec::new();
    for (path, email) in [
        ("password-reset", "a@example.com"),
        ("resend-code", "a@example.com"),
        ("verify-email", "a@example.com"),
        ("password-reset/confirm", "a@example.com"),
        ("password-reset", ""),
        ("resend-code", ""),
        ("verify-email", ""),
        ("password-reset/confirm", ""),
    ] {
        let before = h.proxy.calls().len();
        let body = match path {
            "verify-email" => json!({"email": email, "code": "123456"}),
            "password-reset/confirm" => json!({"email": email, "code": "123456", "password": NEW_PW}),
            _ => json!({"email": email}),
        };
        h.post(path, body, None).await;
        let calls = &h.proxy.calls()[before..];
        let lists = calls.iter().filter(|c| **c == list).count();
        worst.push((path, email, lists, calls.len()));
    }
    assert!(
        worst.iter().all(|&(_, email, lists, total)| lists <= 1 && total == usize::from(!email.is_empty())),
        "(route, email, user-list reads, identity calls) per request: {worst:?}"
    );
}
