// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

mod support;

use auth::app::{AppState, router};
use auth::app_users::{AppUsers, InternalClient, REFRESH_USER_LOOKUP};
use auth::config::Config;
use auth::guard::RateLimiter;
use auth::zitadel::Zitadel;
use chelabase_auth::app_token::{AppClaims, AppTokenSigner, AppTokenVerifier};
use chelabase_auth::internal::{
    AuthMethod, DataIdentity, IDENTITY_HEADER, Identity, InternalSigner, InternalVerifier,
    generate_keypair_pem,
};
use chelabase_query::app_users::{NewUserRow, PendingInsert, SyncTarget, UserRowError, UsersDb};
use chelabase_secrets::SecretKey;
use chelabase_types::data_key::DataKeyKind;
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::internal::*;
use support::*;
use uuid::Uuid;

const PW: &str = "Str0ng!pass";
const NEW_PW: &str = "N3w!passw0rd";

#[derive(Clone, Default, PartialEq)]
enum Fail {
    #[default]
    Nothing,
    UnknownColumn(String),
    Constraint,
    EmailTaken,
    UnavailableOnce,
    Commit,
    /// The commit takes this long (ms), then succeeds.
    SlowCommit(u64),
}

/// A `UsersDb` that journals each step with how many Zitadel calls had happened by then.
#[derive(Clone)]
struct FakeDb {
    journal: Arc<Mutex<Vec<(String, usize)>>>,
    rows: Arc<Mutex<Vec<(SyncTarget, NewUserRow)>>>,
    fail: Arc<Mutex<Fail>>,
    mock: MockZitadel,
}

struct FakePending {
    db: FakeDb,
}

impl FakeDb {
    fn note(&self, what: &str) {
        let n = self.mock.calls().len();
        self.journal.lock().unwrap().push((what.to_string(), n));
    }
    fn steps(&self) -> Vec<String> {
        self.journal
            .lock()
            .unwrap()
            .iter()
            .map(|(s, _)| s.clone())
            .collect()
    }
    fn at(&self, step: &str) -> usize {
        self.journal
            .lock()
            .unwrap()
            .iter()
            .find(|(s, _)| s == step)
            .unwrap_or_else(|| panic!("no {step}"))
            .1
    }
    fn set(&self, fail: Fail) {
        *self.fail.lock().unwrap() = fail;
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
            Fail::Nothing | Fail::Commit | Fail::SlowCommit(_) => {}
        }
        self.rows
            .lock()
            .unwrap()
            .push((target.clone(), row.clone()));
        Ok(Box::new(FakePending { db: self.clone() }))
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
        self.db.note("committed");
        Ok(())
    }
    async fn rollback(self: Box<Self>) {
        self.db.note("rollback");
    }
}

struct H {
    mock: MockZitadel,
    internal: FakeInternal,
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
        "project_id": project, "status": "enabled", "zitadel_org_id": project.to_string(),
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

/// A harness whose refreshes wait at most `lookup` for Zitadel's user answer.
async fn harness_lookup(lookup: Duration) -> H {
    harness_full(false, 1, true, lookup).await
}

async fn harness_full(verification: bool, min_ms: u64, app_on: bool, lookup: Duration) -> H {
    let mock = MockZitadel::start().await;
    let internal = FakeInternal::start().await;
    let project = Uuid::new_v4();
    let branch = Uuid::new_v4();
    mock.add_org(&project.to_string());
    internal.set_branch(branch, info(project, branch, verification));
    let (id_priv, id_pub) = generate_keypair_pem();
    let (app_priv, app_pub) = generate_keypair_pem();
    let db = FakeDb {
        journal: Arc::default(),
        rows: Arc::default(),
        fail: Arc::default(),
        mock: mock.clone(),
    };
    let app = app_on.then(|| {
        let mut app = AppUsers::new(
            InternalVerifier::from_pem(id_pub.as_bytes()).unwrap(),
            InternalClient::new(&internal.base, INTERNAL_TOKEN),
            AppTokenSigner::from_pem(app_priv.as_bytes(), app_pub.as_bytes()).unwrap(),
            Arc::new(db.clone()),
            Zitadel::new(&mock.base, ADMIN_TOKEN),
            Duration::from_secs(900),
        );
        app.refresh_user_lookup = lookup;
        app
    });
    let config = Config {
        listen: String::new(),
        issuer: mock.base.clone(),
        zitadel: chelabase_types::entry::ZitadelAt { base: mock.base.clone(), host: None },
        token: TOKEN.into(),
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
    let state = Arc::new(AppState {
        zitadel: Zitadel::new(&mock.base, TOKEN),
        config,
        limiter: RateLimiter::default(),
        app,
        signup_gate: None,
        used_tickets: Default::default(),
        admin: None,
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
        mock,
        internal,
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
    fn org(&self) -> String {
        self.project.to_string()
    }

    /// The identity the gateway would mint for this request (a public key, and the user
    /// from `access` when given).
    fn identity_for(&self, access: Option<&str>, kind: DataKeyKind) -> String {
        self.identity_opts(access, kind, false)
    }

    /// [`Self::identity_for`], marked server-vouched as the gateway marks it (#55).
    fn identity_opts(&self, access: Option<&str>, kind: DataKeyKind, server_vouched: bool) -> String {
        let (role, claims) = match access {
            None => ("anon".to_string(), json!({})),
            Some(t) => {
                let c = self.claims(t);
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
                        branch_id: self.branch,
                        key_id: self.key,
                        kind,
                        role,
                        claims,
                        databases: None,
                        project_id: Some(self.project),
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

    /// A user in the project's org, made directly in the mock, with a uuid id.
    fn add_user(&self, email: &str, verified: bool) -> String {
        let id = Uuid::new_v4().to_string();
        let mut inner = self.mock.inner.lock().unwrap();
        inner.users.push(User {
            id: id.clone(),
            email: email.into(),
            password: PW.into(),
            verified,
            locked: false,
            org_id: self.org(),
            username: id.clone(),
            display_name: String::new(),
            inactive: false,
            created_at: "2026-10-03T10:00:00Z".into(),
        });
        id
    }

    fn set_user(&self, id: &str, f: impl Fn(&mut User)) {
        let mut inner = self.mock.inner.lock().unwrap();
        inner.users.iter_mut().filter(|u| u.id == id).for_each(f);
    }

    fn session_id(&self, s: &Ans) -> Uuid {
        self.claims(s.body["access_token"].as_str().unwrap()).sid
    }

    fn zitadel_creates(&self) -> usize {
        self.mock
            .calls()
            .iter()
            .filter(|c| *c == "POST /v2/users/new")
            .count()
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
    // Zitadel: id and username are the uuid, in the project's org, already verified.
    let z = h.mock.user_by_id(&id.to_string()).expect("zitadel user");
    assert_eq!(z.username, id.to_string());
    assert_eq!(z.org_id, h.org());
    assert!(z.verified);
    // The row: managed columns plus data, on the key's branch.
    let rows = h.db.rows.lock().unwrap().clone();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0.branch_id, h.branch);
    assert_eq!(rows[0].1.id, id);
    assert!(rows[0].1.email_verified);
    assert_eq!(rows[0].1.data.get("nickname"), Some(&json!("ada")));
    // The sync, then the app session.
    let sync = h.internal.bodies("POST /internal/app/sync");
    assert_eq!(sync.len(), 1);
    assert_eq!(sync[0]["user_id"], json!(id));
    assert_eq!(sync[0]["project_id"], json!(h.project));
    assert_eq!(sync[0]["origin_branch_id"], json!(h.branch));
    assert_eq!(sync[0]["data"], json!({"nickname": "ada"}));
    let sessions = h.internal.bodies("POST /internal/app/sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["user_id"], json!(id));
    assert_eq!(h.internal.session(c.sid).unwrap().user_id, id);
}

#[tokio::test]
async fn sign_up_inserts_before_zitadel_and_commits_after() {
    let h = harness().await;
    let a = h
        .post(
            "sign-up",
            json!({"email": "ada@example.com", "password": PW}),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::CREATED, "{}", a.body);
    assert_eq!(h.db.steps(), ["begin", "commit", "committed"]);
    let calls = h.mock.calls();
    let created = calls
        .iter()
        .position(|c| c == "POST /v2/users/new")
        .expect("created");
    assert_eq!(
        h.db.at("begin"),
        0,
        "the insert comes before any Zitadel call"
    );
    assert!(h.db.at("begin") <= created);
    assert!(
        h.db.at("commit") > created,
        "the commit comes after Zitadel"
    );
}

#[tokio::test]
async fn constraint_failure_never_calls_zitadel() {
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
    assert_eq!(
        h.mock.calls(),
        Vec::<String>::new(),
        "Zitadel is never called"
    );
}

#[tokio::test]
async fn unknown_column_is_422_before_zitadel() {
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
    assert_eq!(
        h.mock.calls(),
        Vec::<String>::new(),
        "Zitadel is never called"
    );
    assert_eq!(h.db.steps(), ["begin"]);
}

#[tokio::test]
async fn commit_failure_deletes_the_zitadel_user() {
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
    assert_eq!(h.zitadel_creates(), 1);
    let id = h.db.rows.lock().unwrap()[0].1.id;
    assert!(
        h.mock.calls().contains(&format!("DELETE /v2/users/{id}")),
        "{:?}",
        h.mock.calls()
    );
    // The mock lets only the admin token delete users (P32).
    assert_eq!(h.mock.users_with_email("ada@example.com"), 0);
    assert!(h.internal.bodies("POST /internal/app/sync").is_empty());
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
    assert_eq!(h.db.steps(), ["begin", "begin", "commit", "committed"]);
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
    h.add_user("ada@example.com", true);
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
    assert_eq!(h.zitadel_creates(), 0);
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
    let existing = h.add_user("ada@example.com", true);
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
    assert_eq!(h.mock.users_with_email("ada@example.com"), 1);
    // The owner gets a notice (a reset code).
    assert!(h.mock.emails().contains(&(existing, "reset".into())));
}

#[tokio::test]
async fn bad_data_is_422_even_when_the_email_is_taken() {
    let h = harness_opts(true, 1, true).await;
    h.add_user("ada@example.com", true);
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
    assert!(h.mock.emails().is_empty());
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
    assert_eq!(h.db.rows.lock().unwrap()[0].0.branch_id, main);
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
    assert!(!h.mock.user_by_id(&id).unwrap().verified);
    assert!(!h.db.rows.lock().unwrap()[0].1.email_verified);
    assert!(h.mock.emails().contains(&(id.clone(), "verify".into())));
    assert!(h.internal.bodies("POST /internal/app/sessions").is_empty());
    // Not yet: sign-in waits for the code.
    let a = h.sign_in("ada@example.com", PW).await;
    assert_eq!(a.status, StatusCode::FORBIDDEN);
    assert_eq!(code(&a), "email_not_verified");
    // A wrong code, then the right one.
    let bad = h
        .post(
            "verify-email",
            json!({"email": "ada@example.com", "code": "000000"}),
            None,
        )
        .await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code(&bad), "invalid_code");
    let ok = h
        .post(
            "verify-email",
            json!({"email": "ada@example.com", "code": EMAIL_CODE}),
            None,
        )
        .await;
    assert_eq!(ok.status, StatusCode::NO_CONTENT, "{}", ok.body);
    assert!(h.mock.user_by_id(&id).unwrap().verified);
    let syncs = h.internal.bodies("POST /internal/app/sync");
    assert_eq!(syncs.last().unwrap()["user_id"], json!(id));
    let a = h.sign_in("ada@example.com", PW).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    // An unknown email's code is refused the same way.
    let unknown = h
        .post(
            "verify-email",
            json!({"email": "zed@example.com", "code": EMAIL_CODE}),
            None,
        )
        .await;
    assert_eq!(unknown.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code(&unknown), "invalid_code");
}

// ---- sign-in -----------------------------------------------------------------------------

#[tokio::test]
async fn sign_in_returns_a_session() {
    let h = harness().await;
    let id = h.add_user("ada@example.com", true);
    let a = h.sign_in("ADA@example.com", PW).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(a.body["user"]["id"], json!(id));
    let c = h.claims(access(&a));
    assert_eq!(c.sub.to_string(), id);
    let s = h.internal.session(c.sid).expect("app session");
    assert_eq!(s.project_id, h.project);
    assert_eq!(a.body["refresh_token"], json!(s.token));
}

#[tokio::test]
async fn sign_in_inactive_check_comes_after_the_password() {
    let h = harness().await;
    let locked = h.add_user("locked@example.com", true);
    let inactive = h.add_user("inactive@example.com", true);
    h.set_user(&locked, |u| u.locked = true);
    h.set_user(&inactive, |u| u.inactive = true);
    for email in ["locked@example.com", "inactive@example.com"] {
        let a = h.sign_in(email, "Wr0ng!pass").await;
        assert_eq!(a.status, StatusCode::UNAUTHORIZED, "{email}");
        assert_eq!(code(&a), "invalid_credentials", "{email}");
    }
}

#[tokio::test]
async fn sign_in_wrong_password_and_unknown_email_look_the_same() {
    let h = harness_opts(false, 150, true).await;
    h.add_user("ada@example.com", true);
    let start = Instant::now();
    let wrong = h.sign_in("ada@example.com", "Wr0ng!pass").await;
    assert!(start.elapsed() >= Duration::from_millis(150));
    let start = Instant::now();
    let unknown = h.sign_in("zed@example.com", PW).await;
    assert!(start.elapsed() >= Duration::from_millis(150));
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong.status, unknown.status);
    assert_eq!(wrong.body, unknown.body);
    assert_eq!(code(&wrong), "invalid_credentials");
}

#[tokio::test]
async fn sign_in_unverified_403() {
    let h = harness_opts(true, 1, true).await;
    h.add_user("ada@example.com", false);
    let a = h.sign_in("ada@example.com", PW).await;
    assert_eq!(a.status, StatusCode::FORBIDDEN, "{}", a.body);
    assert_eq!(code(&a), "email_not_verified");
    assert_eq!(h.mock.session_count(), 0, "the Zitadel session is ended");
    assert!(h.internal.bodies("POST /internal/app/sessions").is_empty());
}

#[tokio::test]
async fn sign_in_inactive_403() {
    let h = harness().await;
    let id = h.add_user("ada@example.com", true);
    h.set_user(&id, |u| u.inactive = true);
    let a = h.sign_in("ada@example.com", PW).await;
    assert_eq!(a.status, StatusCode::FORBIDDEN, "{}", a.body);
    assert_eq!(code(&a), "user_inactive");
    assert_eq!(h.mock.session_count(), 0, "the Zitadel session is ended");
    assert!(h.internal.bodies("POST /internal/app/sessions").is_empty());
}

// ---- sessions ------------------------------------------------------------------------------

#[tokio::test]
async fn refresh_passes_reused_through() {
    let h = harness().await;
    h.add_user("ada@example.com", true);
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
    h.add_user("ada@example.com", true);
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

/// What Zitadel answers the user lookup with.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Zit {
    Active,
    Inactive,
    OtherOrg,
    NotFound404,
    /// A 404 that isn't Zitadel's (a misrouted proxy's HTML page).
    Html404,
    Internal500,
    Empty200,
    NoUserId200,
    /// The right `userId`, active, but no `details.resourceOwner`.
    NoOrg200,
    /// Another user of the project's org, inactive (a proxy or cache mix-up).
    OtherUser200,
    Malformed200,
}

/// When the grace of the session's previous token ends (#59 audit B1).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Timing {
    /// Not during the refresh.
    Steady,
    /// While Zitadel is answering the user lookup. Only a grace token (`Orch::Grace`) has a
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

/// #57's invariant on `refresh`, as a model, with #59's peek in front. Only a definitive verdict
/// ends a session: the orchestrator's 401 `refresh_token_invalid`/`refresh_token_reused`,
/// Zitadel's 404 or a user of another org, or an inactive user. Anything else is 503
/// `unavailable` and revokes nothing. A peek that finds the token's user checks that user in
/// Zitadel before anything rotates, so a Zitadel fault never follows a rotation; a peek
/// verdict goes on to `refresh`, which applies the orchestrator's own rules, as before #59.
/// So does a grace token (the previous token within its grace, `Orch::Grace`): its refresh
/// writes nothing, and it must reach the orchestrator before the grace ends (#59 audit B1).
/// When a grace token's grace ends during the lookup changes nothing, so [`Timing`] isn't an
/// input.
fn refresh_model(p: Peek, o: Orch, z: Zit) -> Outcome {
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
    // The user check, `Ok` when the user may refresh.
    let user = || match z {
        Zit::Active => Ok(()),
        Zit::Inactive => Err((StatusCode::FORBIDDEN, "user_inactive", true)),
        Zit::OtherOrg | Zit::NotFound404 => {
            Err((StatusCode::UNAUTHORIZED, "refresh_token_invalid", true))
        }
        Zit::Html404
        | Zit::Internal500
        | Zit::Empty200
        | Zit::NoUserId200
        | Zit::NoOrg200
        | Zit::OtherUser200
        | Zit::Malformed200 => Err((StatusCode::SERVICE_UNAVAILABLE, "unavailable", false)),
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
        Peek::Live if o != Orch::Grace => match user() {
            Err((status, code, revoke)) => answer(status, code, revoke, false),
            Ok(()) => match rotate() {
                Ok(()) => answer(StatusCode::OK, "", false, true),
                Err(refused) => refused,
            },
        },
        // As before #59: refresh, then the user of a token it accepted.
        Peek::Live | Peek::Invalid401 | Peek::Reused401 => match rotate() {
            Err(refused) => refused,
            Ok(()) => match user() {
                Ok(()) => answer(StatusCode::OK, "", false, true),
                Err((status, code, revoke)) => answer(status, code, revoke, true),
            },
        },
    }
}

/// One case of the matrix: the first refresh against the model, and, when it answered 503,
/// a retry with the same token once every fault is over and (for a current token) the grace
/// has ended, as after a long outage (#59). Returns what differs from the model.
async fn refresh_case(p: Peek, o: Orch, z: Zit, t: Timing) -> Option<String> {
    use Orch as O;
    use Zit as Z;
    let h = harness().await;
    if t == Timing::GraceEndsInLookup {
        let internal = h.internal.clone();
        h.mock
            .on_request("GET /v2/users/", move || internal.end_grace());
    }
    let user = match z {
        // No such user in the mock: its lookup is a real 404.
        Z::NotFound404 => Uuid::new_v4(),
        _ => h.add_user("ada@example.com", true).parse().unwrap(),
    };
    let get = format!("GET /v2/users/{user}");
    match z {
        Z::Active | Z::NotFound404 => {}
        Z::Inactive => h.set_user(&user.to_string(), |u| u.inactive = true),
        Z::OtherOrg => h.set_user(&user.to_string(), |u| u.org_id = "other".into()),
        Z::Html404 => h.mock.answer_raw(&get, 404, "<html>not found</html>", 1),
        Z::Internal500 => h.mock.fail(&get, 500, "Errors.Internal", 1),
        Z::Empty200 => h.mock.answer_raw(&get, 200, "", 1),
        Z::NoUserId200 => h.mock.answer_raw(
            &get,
            200,
            r#"{"user": {"state": "USER_STATE_ACTIVE", "details": {"resourceOwner": "o"}}}"#,
            1,
        ),
        Z::NoOrg200 => h.mock.answer_raw(
            &get,
            200,
            &format!(r#"{{"user": {{"userId": "{user}", "state": "USER_STATE_ACTIVE"}}}}"#),
            1,
        ),
        Z::OtherUser200 => {
            let other = Uuid::new_v4();
            let org = h.org();
            h.mock.answer_raw(
                &get,
                200,
                &format!(
                    r#"{{"user": {{"userId": "{other}", "state": "USER_STATE_INACTIVE", "details": {{"resourceOwner": "{org}"}}}}}}"#
                ),
                1,
            )
        }
        Z::Malformed200 => h.mock.answer_raw(&get, 200, r#"{"user": {"userId""#, 1),
    }
    let grace = o == O::Grace;
    let (sid, token) = h.internal.add_session(h.project, user, grace);
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
    let want = refresh_model(p, o, z);
    let want_tuple = (
        want.status,
        want.code,
        want.revoke,
        want.refresh,
        want.revoke,
    );
    if got != want_tuple {
        return Some(format!(
            "{p:?} x {o:?} x {z:?} x {t:?}: got {got:?}, want {want_tuple:?}: {}",
            a.body
        ));
    }
    if !want.refresh && after.token != current {
        return Some(format!(
            "{p:?} x {o:?} x {z:?} x {t:?}: rotated without a refresh call"
        ));
    }
    // Only a peek verdict followed by a rotation (an orchestrator that contradicts itself)
    // rotates before the user check; its token is the previous one now and may not outlive
    // the grace.
    // A grace token whose grace ended during the lookup is past it on any retry.
    if want.status != StatusCode::SERVICE_UNAVAILABLE
        || (want.refresh && o == O::Rotated)
        || (grace && t == Timing::GraceEndsInLookup)
    {
        return None;
    }
    // The faults are over; a client that held a current token retries after the grace.
    h.mock.clear_on_request();
    h.internal.clear_answers();
    h.mock.inner.lock().unwrap().faults.clear();
    if !grace {
        h.internal.end_grace();
    }
    let healthy = match z {
        Z::Active | Z::Inactive | Z::OtherOrg | Z::NotFound404 => z,
        _ => Z::Active,
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
            "{p:?} x {o:?} x {z:?} x {t:?}, retried later: got {got:?}, want {want:?}: {}",
            b.body
        )
    })
}

/// #57, #59: every peek answer against every refresh answer against every Zitadel answer
/// (8 x 8 x 11), checked against [`refresh_model`], each 503 followed by a later retry; and
/// again for a grace token with its grace ending during the user lookup (8 x 11), 792 cases
/// in all.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn refresh_ends_a_session_only_on_a_definitive_verdict() {
    use Orch as O;
    use Zit as Z;
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
    let zits = [
        Z::Active,
        Z::Inactive,
        Z::OtherOrg,
        Z::NotFound404,
        Z::Html404,
        Z::Internal500,
        Z::Empty200,
        Z::NoUserId200,
        Z::NoOrg200,
        Z::OtherUser200,
        Z::Malformed200,
    ];
    let total = peeks.len() * zits.len() * (orchs.len() + 1);
    // One task per peek answer; each runs its cases in turn.
    let tasks: Vec<_> = peeks
        .into_iter()
        .map(|p| {
            tokio::spawn(async move {
                let mut wrong = Vec::new();
                for o in orchs {
                    for z in zits {
                        wrong.extend(refresh_case(p, o, z, Timing::Steady).await);
                    }
                }
                for z in zits {
                    wrong.extend(refresh_case(p, O::Grace, z, Timing::GraceEndsInLookup).await);
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

/// #59: Zitadel is down for a minute while a user's access token expires. The refreshes during
/// the outage answer 503 and rotate nothing, so the token the client holds still works after
/// it, long past the 10 s grace.
#[tokio::test]
async fn refresh_survives_a_zitadel_outage() {
    let h = harness().await;
    h.add_user("ada@example.com", true);
    let s = h.sign_in("ada@example.com", PW).await;
    let sid = h.session_id(&s);
    let token = s.body["refresh_token"].clone();
    let held = h.internal.session(sid).unwrap().token;
    h.mock.set_down(true);
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
    h.mock.set_down(false);
    let a = h
        .post("refresh", json!({"refresh_token": token}), None)
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_ne!(a.body["refresh_token"], token);
    assert!(!h.internal.session(sid).unwrap().revoked);
}

/// #59: a Zitadel fault after peek found the user leaves the session's current token as it
/// was, for every kind of fault; a later refresh with it still works.
#[tokio::test]
async fn refresh_zitadel_fault_rotates_nothing() {
    let h = harness().await;
    let user = h.add_user("ada@example.com", true);
    let get = format!("GET /v2/users/{user}");
    let s = h.sign_in("ada@example.com", PW).await;
    let sid = h.session_id(&s);
    let token = s.body["refresh_token"].clone();
    let held = h.internal.session(sid).unwrap().token;
    h.mock.fail(&get, 500, "Errors.Internal", 1);
    h.mock.answer_raw(&get, 200, "", 1);
    h.mock.answer_raw(&get, 404, "<html>not found</html>", 1);
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

/// #59: a deactivated user is refused as before when Zitadel answers, and the check comes
/// before any rotation.
#[tokio::test]
async fn refresh_refuses_a_deactivated_user_before_rotating() {
    let h = harness().await;
    let user = h.add_user("ada@example.com", true);
    let s = h.sign_in("ada@example.com", PW).await;
    let sid = h.session_id(&s);
    h.set_user(&user, |u| u.inactive = true);
    let a = h
        .post(
            "refresh",
            json!({"refresh_token": s.body["refresh_token"]}),
            None,
        )
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

/// #59 audit B1: two parallel requests share a cookie; one rotated the token and the other
/// presents the previous one inside the grace. The grace ends while Zitadel answers the user
/// lookup; the grace token was answered before that, so the session lives. A current token
/// doesn't care when the old grace ends.
#[tokio::test]
async fn refresh_survives_the_grace_ending_during_the_user_lookup() {
    let h = harness().await;
    h.add_user("ada@example.com", true);
    let s = h.sign_in("ada@example.com", PW).await;
    let sid = h.session_id(&s);
    let t0 = s.body["refresh_token"].clone();
    let a = h.post("refresh", json!({"refresh_token": t0}), None).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    let internal = h.internal.clone();
    h.mock
        .on_request("GET /v2/users/", move || internal.end_grace());
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

/// The number of Zitadel user lookups so far.
fn user_lookups(h: &H) -> usize {
    h.mock
        .calls()
        .iter()
        .filter(|c| c.starts_with("GET /v2/users/"))
        .count()
}

/// #59 second audit S1: peek finds the token current, then a parallel request rotates the
/// session while Zitadel answers this request's user lookup. When this request reaches
/// `refresh` its token is the previous one, inside the grace: it gets the grace answer (the
/// parallel request's successor), and the user is looked up only once.
#[tokio::test]
async fn refresh_of_a_current_token_rotated_during_the_lookup_gets_the_grace_answer() {
    let h = harness().await;
    h.add_user("ada@example.com", true);
    let s = h.sign_in("ada@example.com", PW).await;
    let sid = h.session_id(&s);
    let t0 = s.body["refresh_token"].clone();
    let internal = h.internal.clone();
    let parallel = Arc::new(Mutex::new(None::<String>));
    let seen = parallel.clone();
    h.mock.on_request("GET /v2/users/", move || {
        let mut seen = seen.lock().unwrap();
        if seen.is_none() {
            *seen = Some(internal.rotate(sid));
        }
    });
    let before = user_lookups(&h);
    let b = h.post("refresh", json!({"refresh_token": t0}), None).await;
    assert_eq!(b.status, StatusCode::OK, "{}", b.body);
    let parallel = parallel.lock().unwrap().clone().expect("the hook rotated");
    assert_eq!(
        b.body["refresh_token"].as_str(),
        Some(parallel.as_str()),
        "the grace answer: the parallel request's successor"
    );
    assert_eq!(user_lookups(&h) - before, 1, "one user lookup");
    assert_eq!(
        h.internal.session(sid).unwrap().token,
        parallel,
        "nothing else rotated"
    );
    assert!(!h.internal.session(sid).unwrap().revoked);
}

/// #59 second audit S1, the accepted residual: as above, but the parallel rotation's grace
/// also ends before this request reaches `refresh`. Its token is reuse now: 401
/// `refresh_token_reused`, and the orchestrator revokes the session. Only timing keeps this
/// away: the lookup is cut at `REFRESH_USER_LOOKUP` (at most half of `APP_REFRESH_GRACE`),
/// which leaves the rest of the grace for the peek's reply, the `refresh` call and its row
/// lock.
#[tokio::test]
async fn refresh_of_a_current_token_rotated_and_past_its_grace_during_the_lookup_is_reuse() {
    let h = harness().await;
    h.add_user("ada@example.com", true);
    let s = h.sign_in("ada@example.com", PW).await;
    let sid = h.session_id(&s);
    let t0 = s.body["refresh_token"].clone();
    let internal = h.internal.clone();
    let rotated = Arc::new(Mutex::new(false));
    let seen = rotated.clone();
    h.mock.on_request("GET /v2/users/", move || {
        let mut seen = seen.lock().unwrap();
        if !*seen {
            internal.rotate(sid);
            internal.end_grace();
            *seen = true;
        }
    });
    let before = user_lookups(&h);
    let b = h.post("refresh", json!({"refresh_token": t0}), None).await;
    assert!(*rotated.lock().unwrap(), "the hook rotated");
    assert_eq!(
        (b.status, code(&b)),
        (StatusCode::UNAUTHORIZED, "refresh_token_reused"),
        "{}",
        b.body
    );
    assert_eq!(user_lookups(&h) - before, 1, "one user lookup");
    assert!(h.internal.session(sid).unwrap().revoked);
}

/// #59 audit B1: a Zitadel user lookup slower than the refresh bound is a fault: 503, and the
/// token the client holds is still current.
#[tokio::test]
async fn refresh_user_lookup_slower_than_the_bound_is_unavailable() {
    let h = harness_lookup(Duration::from_millis(100)).await;
    h.add_user("ada@example.com", true);
    let s = h.sign_in("ada@example.com", PW).await;
    let sid = h.session_id(&s);
    let token = s.body["refresh_token"].clone();
    let held = h.internal.session(sid).unwrap().token;
    h.mock.set_user_lookup_delay(Duration::from_millis(400));
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
    h.mock.set_user_lookup_delay(Duration::ZERO);
    let b = h
        .post("refresh", json!({"refresh_token": token}), None)
        .await;
    assert_eq!(b.status, StatusCode::OK, "{}", b.body);
}

#[tokio::test]
async fn refresh_reads_current_email() {
    let h = harness().await;
    let id = h.add_user("ada@example.com", true);
    let s = h.sign_in("ada@example.com", PW).await;
    h.set_user(&id, |u| u.email = "ada@new.example".into());
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
    // A deactivated user can't refresh, and their session ends.
    h.set_user(&id, |u| u.inactive = true);
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
    h.add_user("ada@example.com", true);
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
    let id = h.add_user("ada@example.com", true);
    let a = h.get("user", None).await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    assert_eq!(code(&a), "user_token_invalid");
    let s = h.sign_in("ada@example.com", PW).await;
    let a = h.get("user", Some(access(&s))).await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    assert_eq!(
        a.body,
        json!({"id": id, "email": "ada@example.com", "email_verified": true,
            "created_at": "2026-10-03T10:00:00Z"})
    );
}

// ---- passwords and email ---------------------------------------------------------------------

#[tokio::test]
async fn password_reset_always_204() {
    let h = harness().await;
    let id = h.add_user("ada@example.com", true);
    for email in ["ada@example.com", "zed@example.com"] {
        let a = h
            .post("password-reset", json!({"email": email}), None)
            .await;
        assert_eq!(a.status, StatusCode::NO_CONTENT, "{email}");
        assert_eq!(a.body, Value::Null);
    }
    assert_eq!(h.mock.emails(), vec![(id, "reset".to_string())]);
    for email in ["ada@example.com", "zed@example.com"] {
        let a = h.post("resend-code", json!({"email": email}), None).await;
        assert_eq!(a.status, StatusCode::NO_CONTENT, "{email}");
    }
}

#[tokio::test]
async fn password_reset_confirm_revokes_all() {
    let h = harness().await;
    let id: Uuid = h.add_user("ada@example.com", true).parse().unwrap();
    let s1 = h.sign_in("ada@example.com", PW).await;
    let s2 = h.sign_in("ada@example.com", PW).await;
    let weak = h
        .post(
            "password-reset/confirm",
            json!({"email": "ada@example.com", "code": RESET_CODE, "password": "weak"}),
            None,
        )
        .await;
    assert_eq!(weak.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code(&weak), "weak_password");
    let bad = h
        .post(
            "password-reset/confirm",
            json!({"email": "ada@example.com", "code": "000000", "password": NEW_PW}),
            None,
        )
        .await;
    assert_eq!(bad.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(code(&bad), "invalid_code");
    let ok = h
        .post(
            "password-reset/confirm",
            json!({"email": "ada@example.com", "code": RESET_CODE, "password": NEW_PW}),
            None,
        )
        .await;
    assert_eq!(ok.status, StatusCode::NO_CONTENT, "{}", ok.body);
    assert_eq!(h.mock.user_by_id(&id.to_string()).unwrap().password, NEW_PW);
    let revokes = h
        .internal
        .bodies(&format!("POST /internal/app/users/{id}/revoke-sessions"));
    assert_eq!(revokes, vec![json!({"project_id": h.project})]);
    for s in [&s1, &s2] {
        assert!(h.internal.session(h.session_id(s)).unwrap().revoked);
    }
}

#[tokio::test]
async fn change_password_returns_a_new_session_and_revokes_others() {
    let h = harness().await;
    let id: Uuid = h.add_user("ada@example.com", true).parse().unwrap();
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
    let a = h
        .post(
            "change-password",
            json!({"current_password": PW, "password": NEW_PW}),
            Some(access(&s1)),
        )
        .await;
    assert_eq!(a.status, StatusCode::OK, "{}", a.body);
    let s3 = h.session_id(&a);
    assert_eq!(h.mock.user_by_id(&id.to_string()).unwrap().password, NEW_PW);
    let revokes = h
        .internal
        .bodies(&format!("POST /internal/app/users/{id}/revoke-sessions"));
    assert_eq!(
        revokes,
        vec![json!({"project_id": h.project, "keep_session_id": s3})]
    );
    assert!(h.internal.session(h.session_id(&s1)).unwrap().revoked);
    assert!(h.internal.session(h.session_id(&s2)).unwrap().revoked);
    assert!(!h.internal.session(s3).unwrap().revoked);
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

#[tokio::test]
async fn change_email_syncs() {
    let h = harness().await;
    let id = h.add_user("ada@example.com", true);
    let s = h.sign_in("ada@example.com", PW).await;
    let a = h
        .post(
            "change-email",
            json!({"email": "Ada@New.example"}),
            Some(access(&s)),
        )
        .await;
    assert_eq!(a.status, StatusCode::NO_CONTENT, "{}", a.body);
    let z = h.mock.user_by_id(&id).unwrap();
    assert_eq!(z.email, "ada@new.example");
    assert!(z.verified, "verification is off: the new email is verified");
    let sync = h.internal.bodies("POST /internal/app/sync");
    assert_eq!(sync, vec![json!({"project_id": h.project, "user_id": id})]);
}

#[tokio::test]
async fn change_email_taken_409() {
    let h = harness().await;
    h.add_user("ada@example.com", true);
    h.add_user("bob@example.com", true);
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
    let a = h
        .post("change-email", json!({"email": "x@example.com"}), None)
        .await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED);
    assert_eq!(code(&a), "user_token_invalid");
}

/// N2: an email two users of the org already share (made outside Chelabase's checks) is taken:
/// sign-up makes no third account, change-email refuses it, and sign-in matches no one.
#[tokio::test]
async fn an_ambiguous_email_is_taken_and_signs_no_one_in() {
    let h = harness().await;
    h.add_user("dup@example.com", true);
    h.add_user("DUP@example.com", true);
    let a = h
        .post(
            "sign-up",
            json!({"email": "dup@example.com", "password": PW}),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::CONFLICT, "{}", a.body);
    assert_eq!(code(&a), "email_taken");
    assert_eq!(h.zitadel_creates(), 0);
    assert_eq!(h.mock.users_with_email("dup@example.com"), 2);
    let a = h.sign_in("dup@example.com", PW).await;
    assert_eq!(a.status, StatusCode::UNAUTHORIZED, "{}", a.body);
    assert_eq!(code(&a), "invalid_credentials");
    let ada = h.add_user("ada@example.com", true);
    let s = h.sign_in("ada@example.com", PW).await;
    let a = h
        .post("change-email", json!({"email": "Dup@example.com"}), Some(access(&s)))
        .await;
    assert_eq!(a.status, StatusCode::CONFLICT, "{}", a.body);
    assert_eq!(code(&a), "email_taken");
    assert_eq!(h.mock.user_by_id(&ada).unwrap().email, "ada@example.com");
}

// ---- guards ----------------------------------------------------------------------------------

#[tokio::test]
async fn app_users_disabled_409() {
    let h = harness().await;
    h.add_user("ada@example.com", true);
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

#[tokio::test]
async fn secret_key_identity_refused() {
    let h = harness().await;
    h.add_user("ada@example.com", true);
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
    assert_eq!(h.mock.calls(), Vec::<String>::new());
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
    // The platform flows still answer.
    let a = h
        .send(
            h.http
                .post(format!("{}/auth/v1/auth-request", h.base))
                .json(&json!({"authRequest": "V2_1"})),
            None,
        )
        .await;
    assert_eq!(a.status, StatusCode::GONE, "{}", a.body);
}

#[tokio::test]
async fn a_dropped_sign_up_still_finishes() {
    let h = harness().await;
    h.db.set(Fail::SlowCommit(400));
    // The client gives up while the commit (after Zitadel's create) is in flight.
    let gave_up = h
        .http
        .post(format!("{}/auth/v1/app/sign-up", h.base))
        .timeout(Duration::from_millis(150))
        .header(IDENTITY_HEADER, h.identity_for(None, DataKeyKind::Public))
        .json(&json!({"email": "ada@example.com", "password": PW}))
        .send()
        .await;
    assert!(gave_up.is_err(), "the client timed out");
    assert_eq!(h.zitadel_creates(), 1);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let committed = h.db.steps().contains(&"committed".to_string());
        let synced = !h.internal.bodies("POST /internal/app/sync").is_empty();
        let deleted = h.mock.users_with_email("ada@example.com") == 0;
        if (committed && synced) || deleted {
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
    h.add_user("ada@example.com", true);
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
    let id = h.add_user("ada@example.com", true);
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
    assert_eq!(
        h.mock.user_by_id(&id).unwrap().password,
        PW,
        "the password is unchanged"
    );
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
    h.add_user("ada@example.com", true);
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
    h.add_user("ada@example.com", true);
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

    /// How many password checks (`POST /v2/sessions`, not its `/search`) reached Zitadel.
    fn password_checks(&self) -> usize {
        self.mock.calls().iter().filter(|c| *c == "POST /v2/sessions").count()
    }
}

/// `n` wrong-password sign-ins for ada at once, each from its own address; (401s, 429s).
async fn wrong_sign_in_burst(h: &Arc<H>, n: usize, vouched: bool) -> (usize, usize) {
    // A slow password check, so every request is in flight before the first answer.
    h.mock.on_request("POST /v2/sessions", || std::thread::sleep(Duration::from_millis(300)));
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
    h.mock.clear_on_request();
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
    h.add_user("ada@example.com", true);
    let (refused, limited) = wrong_sign_in_burst(&h, 30, false).await;
    let checks = h.password_checks();
    assert!(checks <= 5, "{checks} password checks reached Zitadel ({refused} got 401, {limited} got 429)");
    assert_eq!((refused, limited), (5, 25));
}

// AC 4: a server-vouched caller has no address limit; the email limit still holds at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 64)]
async fn server_vouched_sign_in_burst_is_limited() {
    let h = Arc::new(harness().await);
    h.add_user("ada@example.com", true);
    let (refused, limited) = wrong_sign_in_burst(&h, 30, true).await;
    let checks = h.password_checks();
    assert!(checks <= 5, "{checks} password checks reached Zitadel ({refused} got 401, {limited} got 429)");
    assert_eq!((refused, limited), (5, 25));
}

// AC 3: a client that hangs up while Zitadel checks the password still spends the attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn app_sign_in_cancelled_password_checks_still_count() {
    let h = harness().await;
    h.add_user("ada@example.com", true);
    // Only the password check is slow: 300 ms, and the client gives up at 100 ms.
    h.mock.on_request("POST /v2/sessions", || std::thread::sleep(Duration::from_millis(300)));
    let impatient = reqwest::Client::builder().timeout(Duration::from_millis(100)).build().unwrap();
    for i in 0..5 {
        let r = impatient
            .post(format!("{}/auth/v1/app/sign-in", h.base))
            .json(&json!({"email": "ada@example.com", "password": "Wr0ng!pass"}))
            .header("x-forwarded-for", format!("10.3.{i}.1"))
            .header(IDENTITY_HEADER, h.identity_for(None, DataKeyKind::Public))
            .send()
            .await
            .map(|r| r.status());
        assert!(r.as_ref().is_err_and(|e| e.is_timeout()), "attempt {i}: {r:?}");
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    h.mock.clear_on_request();
    // A patient client (no timeout).
    let a = h.sign_in_from("ada@example.com", "Wr0ng!pass", "10.3.9.1", false).await;
    assert_eq!(a.status, LIMITED, "{}", a.body);
    assert_eq!(h.password_checks(), 5);
}

// AC 6: the orchestrator down or the branch disabled spend nothing.
#[tokio::test]
async fn app_sign_in_internal_down_or_disabled_does_not_spend() {
    let h = harness().await;
    h.add_user("ada@example.com", true);
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
// whose account then can't sign in (unverified).
#[tokio::test(flavor = "multi_thread", worker_threads = 16)]
async fn app_concurrent_correct_sign_ins_do_not_spend_the_email_budget() {
    let h = Arc::new(harness().await);
    h.add_user("ada@example.com", true);
    h.mock.on_request("POST /v2/sessions", || std::thread::sleep(Duration::from_millis(200)));
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
    h.mock.clear_on_request();
    assert_app_budget_unspent(&h, "2").await;

    let h = harness_opts(true, 1, true).await;
    h.add_user("ada@example.com", false);
    for i in 0..6 {
        let a = h.sign_in_from("ada@example.com", PW, &format!("10.5.9.{i}"), false).await;
        assert_eq!(a.status, StatusCode::FORBIDDEN, "unverified {i}: {}", a.body);
    }
    assert_app_budget_unspent(&h, "3").await;
}

// AC 8: Zitadel outages spend nothing; its request validation counts like a wrong password.
#[tokio::test]
async fn app_sign_in_outages_do_not_spend_the_email_budget() {
    let h = harness().await;
    h.add_user("ada@example.com", true);
    let mut ip = 0;
    let mut sign_in = async |expect: StatusCode, why: &str| {
        ip += 1;
        let a = h.sign_in_from("ada@example.com", "Wr0ng!pass", &format!("10.7.0.{ip}"), false).await;
        assert_eq!(a.status, expect, "{why}: {}", a.body);
    };
    h.mock.set_down(true);
    for _ in 0..6 {
        sign_in(StatusCode::SERVICE_UNAVAILABLE, "down").await;
    }
    h.mock.set_down(false);
    // The password check itself fails: a 5xx, and a 4xx that isn't Zitadel's own answer.
    h.mock.fail("POST /v2/sessions", 503, "Errors.Internal", 3);
    h.mock.answer_raw("POST /v2/sessions", 404, "<html>not found</html>", 3);
    for _ in 0..6 {
        sign_in(StatusCode::SERVICE_UNAVAILABLE, "check down").await;
    }
    assert_app_budget_unspent(&h, "4").await;

    let h = harness().await;
    h.add_user("ada@example.com", true);
    h.mock.answer_raw(
        "POST /v2/sessions",
        400,
        r#"{"code":3,"message":"invalid CreateSessionRequest.Checks: embedded message failed validation","details":[]}"#,
        5,
    );
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
