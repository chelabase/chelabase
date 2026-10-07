mod support;

use auth::app::{AppState, router};
use auth::app_users::{AppUsers, InternalClient};
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
        "sync": {"branch_id": target, "host": "127.0.0.1", "port": 5432, "database": "app",
            "user": "chelabase_auth_sync", "password": "pw", "schema": "public", "table": "users"},
    })
}

async fn harness_opts(verification: bool, min_ms: u64, app_on: bool) -> H {
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
        AppUsers::new(
            InternalVerifier::from_pem(id_pub.as_bytes()).unwrap(),
            InternalClient::new(&internal.base, INTERNAL_TOKEN),
            AppTokenSigner::from_pem(app_priv.as_bytes(), app_pub.as_bytes()).unwrap(),
            Arc::new(db.clone()),
            Zitadel::new(&mock.base, ADMIN_TOKEN),
            Duration::from_secs(900),
        )
    });
    let config = Config {
        listen: String::new(),
        issuer: mock.base.clone(),
        token: TOKEN.into(),
        cookie_key: SecretKey::generate(),
        org_id: "o1".into(),
        studio_url: "http://localhost:3000".into(),
        skip_email_verification: true,
        secure_cookies: false,
        min_response: Duration::from_millis(min_ms),
        mode: auth::config::Mode::SelfHosted,
    };
    let state = Arc::new(AppState {
        zitadel: Zitadel::new(&mock.base, TOKEN),
        config,
        limiter: RateLimiter::default(),
        app,
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router(state)).await.unwrap() });
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
                    }),
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
                    }),
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
        let mut req = req.header("x-forwarded-for", "10.1.1.1");
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
