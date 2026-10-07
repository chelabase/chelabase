//! The HTTP edge without a database: identity, the schema route's key check, connection
//! errors and the body cap. The connection source is a fake; nothing here reaches Postgres.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chelabase_auth::internal::{
    AuthMethod, DataIdentity, IDENTITY_HEADER, Identity, InternalSigner, InternalVerifier,
    generate_keypair_pem,
};
use chelabase_types::data_key::DataKeyKind;
use rest::app::{AppState, internal_router, router};
use rest::conn::{ConnError, ConnInfo, ConnSource};
use rest::exec::Engine;
use serde_json::Value;
use uuid::Uuid;

const TOKEN: &str = "test-rest-internal";

struct FakeSource {
    answer: fn() -> Result<ConnInfo, ConnError>,
    calls: Arc<AtomicUsize>,
}

impl ConnSource for FakeSource {
    async fn fetch(&self, _branch: Uuid) -> Result<ConnInfo, ConnError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        (self.answer)()
    }
}

struct Harness {
    addr: SocketAddr,
    internal: SocketAddr,
    signer: InternalSigner,
    calls: Arc<AtomicUsize>,
    http: reqwest::Client,
}

async fn serve(app: axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

async fn harness(answer: fn() -> Result<ConnInfo, ConnError>) -> Harness {
    let (private, public) = generate_keypair_pem();
    let calls = Arc::new(AtomicUsize::new(0));
    let source = FakeSource {
        answer,
        calls: calls.clone(),
    };
    let state = Arc::new(AppState {
        engine: Engine::new(source, 10),
        verifier: InternalVerifier::from_pem(public.as_bytes()).unwrap(),
        internal_token: TOKEN.into(),
    });
    Harness {
        addr: serve(router(state.clone())).await,
        internal: serve(internal_router(state)).await,
        signer: InternalSigner::from_pem(private.as_bytes()).unwrap(),
        calls,
        http: reqwest::Client::new(),
    }
}

fn identity(kind: Option<DataKeyKind>, databases: Option<Vec<String>>) -> Identity {
    let key_id = Uuid::new_v4();
    Identity {
        sub: key_id.to_string(),
        org_id: String::new(),
        auth: if kind.is_some() {
            AuthMethod::DataKey
        } else {
            AuthMethod::ManagementJwt
        },
        request_id: "req-1".into(),
        data: kind.map(|kind| DataIdentity {
            branch_id: Uuid::new_v4(),
            key_id,
            kind,
            role: if kind == DataKeyKind::Public {
                "anon".into()
            } else {
                "authenticated".into()
            },
            claims: serde_json::json!({}),
            databases,
            project_id: None,
            role_oid: None,
        }),
    }
}

impl Harness {
    fn token(&self, audience: &str, kind: Option<DataKeyKind>) -> String {
        self.signer.sign(audience, &identity(kind, None)).unwrap()
    }

    /// A data key's identity with an allow-list.
    fn listed(&self, kind: DataKeyKind, databases: &[&str]) -> String {
        let databases = databases.iter().map(|d| d.to_string()).collect();
        self.signer
            .sign("rest", &identity(Some(kind), Some(databases)))
            .unwrap()
    }

    async fn internal_post(&self, path: &str, bearer: Option<&str>, body: Value) -> (u16, Value) {
        let mut req = self
            .http
            .post(format!("http://{}{path}", self.internal))
            .json(&body);
        if let Some(bearer) = bearer {
            req = req.bearer_auth(bearer);
        }
        let resp = req.send().await.unwrap();
        (
            resp.status().as_u16(),
            resp.json().await.unwrap_or(Value::Null),
        )
    }

    async fn post(&self, token: Option<&str>, body: Vec<u8>) -> (u16, Option<String>, Value) {
        let mut req = self
            .http
            .post(format!("http://{}/data/v1/query", self.addr))
            .header("content-type", "application/json")
            .body(body);
        if let Some(token) = token {
            req = req.header(IDENTITY_HEADER, token);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let retry = resp
            .headers()
            .get("retry-after")
            .map(|v| v.to_str().unwrap().to_owned());
        (status, retry, resp.json().await.unwrap_or(Value::Null))
    }

    async fn get(
        &self,
        addr: SocketAddr,
        path: &str,
        header: Option<(&str, String)>,
    ) -> (u16, Value) {
        let mut req = self.http.get(format!("http://{addr}{path}"));
        if let Some((name, value)) = header {
            req = req.header(name, value);
        }
        let resp = req.send().await.unwrap();
        (
            resp.status().as_u16(),
            resp.json().await.unwrap_or(Value::Null),
        )
    }
}

const SELECT: &[u8] = br#"{"v":1,"op":"select","from":"t"}"#;

fn not_ready() -> Result<ConnInfo, ConnError> {
    Err(ConnError::NotReady("resetting".into()))
}

fn unknown() -> Result<ConnInfo, ConnError> {
    Err(ConnError::UnknownBranch)
}

/// A branch serving `main` and `analytics`, at an address nothing listens on.
fn served() -> Result<ConnInfo, ConnError> {
    Ok(serde_json::from_value(serde_json::json!({
        "host": "127.0.0.1", "port": 1, "database": "main",
        "user": "authenticator", "password": "pw", "status": "ready",
        "settings": { "max_rows": 1000, "statement_timeout_ms": 8000, "max_connections": 2 },
        "databases": [
            { "name": "main", "default_schema": "public" },
            { "name": "analytics", "default_schema": "reporting" },
        ],
    }))
    .unwrap())
}

const UNKNOWN_DATABASE: &str = "the database doesn't exist or isn't served by this key";

#[tokio::test]
async fn missing_identity_is_401() {
    let h = harness(not_ready).await;
    let management = h.token("rest", None);
    let other_audience = h.token("orchestrator", Some(DataKeyKind::Secret));
    for token in [
        None,
        Some("garbage"),
        Some(management.as_str()),
        Some(other_audience.as_str()),
    ] {
        let (status, _, body) = h.post(token, SELECT.to_vec()).await;
        assert_eq!(status, 401, "{token:?}");
        assert_eq!(body["error"]["code"], "unauthorized", "{token:?}");
    }
    let (status, body) = h.get(h.addr, "/data/v1/schema", None).await;
    assert_eq!(
        (status, &body["error"]["code"]),
        (401, &Value::from("unauthorized"))
    );
    assert_eq!(h.calls.load(Ordering::SeqCst), 0);
}

/// The gateway already refuses a public key on the schema route; rest checks again.
#[tokio::test]
async fn public_key_identity_cannot_read_schema() {
    let h = harness(not_ready).await;
    let token = h.token("rest", Some(DataKeyKind::Public));
    let (status, body) = h
        .get(
            h.addr,
            "/data/v1/schema",
            Some(("x-chelabase-identity", token)),
        )
        .await;
    assert_eq!(status, 401);
    assert_eq!(body["error"]["code"], "unauthorized");
    assert_eq!(h.calls.load(Ordering::SeqCst), 0);

    // A secret key gets through to the connection (here: not ready).
    let token = h.token("rest", Some(DataKeyKind::Secret));
    let (status, _) = h
        .get(
            h.addr,
            "/data/v1/schema",
            Some(("x-chelabase-identity", token)),
        )
        .await;
    assert_eq!(status, 503);
}

#[tokio::test]
async fn connection_409_is_503_with_retry_after() {
    let h = harness(not_ready).await;
    let token = h.token("rest", Some(DataKeyKind::Public));
    let (status, retry, body) = h.post(Some(&token), SELECT.to_vec()).await;
    assert_eq!(status, 503);
    assert_eq!(retry.as_deref(), Some("2"));
    assert_eq!(body["error"]["code"], "unavailable");
    // Not cached: the next request asks again.
    h.post(Some(&token), SELECT.to_vec()).await;
    assert_eq!(h.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn unknown_branch_is_401() {
    let h = harness(unknown).await;
    let token = h.token("rest", Some(DataKeyKind::Secret));
    let (status, _, body) = h.post(Some(&token), SELECT.to_vec()).await;
    assert_eq!(status, 401);
    assert_eq!(body["error"]["code"], "unauthorized");
}

#[tokio::test]
async fn invalid_query_is_400_before_any_connection() {
    let h = harness(not_ready).await;
    let token = h.token("rest", Some(DataKeyKind::Public));
    let (status, _, body) = h
        .post(
            Some(&token),
            br#"{"v":2,"op":"select","from":"t"}"#.to_vec(),
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(body["error"]["code"], "invalid_query");
    assert_eq!(h.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn body_over_1mib_is_413() {
    let h = harness(not_ready).await;
    let token = h.token("rest", Some(DataKeyKind::Public));
    // Exactly 1 MiB: a valid query padded with spaces gets through to the connection.
    let mut body = SELECT.to_vec();
    body.resize(1_048_576, b' ');
    let (status, _, _) = h.post(Some(&token), body.clone()).await;
    assert_eq!(status, 503);
    body.push(b' ');
    let (status, _, json) = h.post(Some(&token), body).await;
    assert_eq!(status, 413);
    assert_eq!(json["error"]["code"], "too_large");
}

#[tokio::test]
async fn internal_schema_needs_the_bearer_and_passes_branch_states_through() {
    let h = harness(not_ready).await;
    let path = format!("/internal/schema/{}", Uuid::new_v4());
    let (status, _) = h.get(h.internal, &path, None).await;
    assert_eq!(status, 401);
    let (status, _) = h
        .get(
            h.internal,
            &path,
            Some(("authorization", "Bearer nope".into())),
        )
        .await;
    assert_eq!(status, 401);
    let (status, body) = h
        .get(
            h.internal,
            &path,
            Some(("authorization", format!("Bearer {TOKEN}"))),
        )
        .await;
    assert_eq!(status, 409, "{body}");
    assert_eq!(body["status"], "resetting");

    let h = harness(unknown).await;
    let (status, _) = h
        .get(
            h.internal,
            &path,
            Some(("authorization", format!("Bearer {TOKEN}"))),
        )
        .await;
    assert_eq!(status, 404);
}

/// A database that doesn't exist, isn't served, or isn't on the key's list: one answer for
/// all, before any connection is opened.
#[tokio::test]
async fn unknown_database_in_all_three_cases_has_one_text() {
    let h = harness(served).await;
    let query = |database: Option<&str>| {
        let mut tree = serde_json::json!({ "v": 1, "op": "select", "from": "t" });
        if let Some(d) = database {
            tree["database"] = d.into();
        }
        serde_json::to_vec(&tree).unwrap()
    };
    let unlisted = h.token("rest", Some(DataKeyKind::Public));
    let cases = [
        // Doesn't exist.
        (unlisted.clone(), query(Some("ghost"))),
        // Served, but not on the key's list (main by default, too).
        (
            h.listed(DataKeyKind::Public, &["main"]),
            query(Some("analytics")),
        ),
        (h.listed(DataKeyKind::Secret, &["analytics"]), query(None)),
        // On the key's list, but not served.
        (
            h.listed(DataKeyKind::Secret, &["main", "archive"]),
            query(Some("archive")),
        ),
        // A list left empty reaches nothing.
        (h.listed(DataKeyKind::Secret, &[]), query(None)),
    ];
    let mut bodies = Vec::new();
    for (token, body) in cases {
        let (status, _, json) = h.post(Some(&token), body).await;
        assert_eq!(status, 404, "{json}");
        bodies.push(json);
    }
    for (path, token) in [
        (
            "/data/v1/schema?database=ghost",
            h.token("rest", Some(DataKeyKind::Secret)),
        ),
        (
            "/data/v1/schema?database=analytics",
            h.listed(DataKeyKind::Secret, &["main"]),
        ),
        (
            "/data/v1/schema",
            h.listed(DataKeyKind::Secret, &["analytics"]),
        ),
    ] {
        let (status, json) = h.get(h.addr, path, Some((IDENTITY_HEADER, token))).await;
        assert_eq!(status, 404, "{path}: {json}");
        bodies.push(json);
    }
    for body in &bodies {
        assert_eq!(
            body,
            &serde_json::json!({ "error": { "code": "unknown_database", "message": UNKNOWN_DATABASE } })
        );
    }
}

/// Only the top level names a database: a batch can't span two.
#[tokio::test]
async fn a_batch_cannot_name_a_database_inside() {
    let h = harness(served).await;
    let token = h.token("rest", Some(DataKeyKind::Secret));
    let body = serde_json::json!({
        "v": 1, "op": "batch", "database": "main",
        "ops": [
            { "op": "select", "from": "t" },
            { "op": "select", "from": "t", "database": "analytics" },
        ],
    });
    let (status, _, json) = h
        .post(Some(&token), serde_json::to_vec(&body).unwrap())
        .await;
    assert_eq!(status, 400, "{json}");
    assert_eq!(json["error"]["code"], "invalid_query");
    assert_eq!(
        json["error"]["message"],
        "ops[1]: only the top level may name a database"
    );
    assert_eq!(h.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn invalidate_needs_the_bearer() {
    let h = harness(served).await;
    let branch = Uuid::new_v4();
    let path = format!("/internal/branches/{branch}/invalidate");
    for bearer in [None, Some("nope")] {
        let (status, json) = h.internal_post(&path, bearer, Value::Null).await;
        assert_eq!(status, 401, "{bearer:?}");
        assert_eq!(json["error"]["code"], "unauthorized");
    }
    let (status, _) = h.internal_post(&path, Some(TOKEN), Value::Null).await;
    assert_eq!(status, 204);
}

/// The connection info is cached until the orchestrator says the branch changed.
#[tokio::test]
async fn invalidate_makes_the_next_request_ask_again() {
    let h = harness(served).await;
    let mut data = identity(Some(DataKeyKind::Secret), None);
    let branch = data.data.as_ref().unwrap().branch_id;
    data.data.as_mut().unwrap().databases = Some(vec!["ghost".into()]);
    let token = h.signer.sign("rest", &data).unwrap();
    for _ in 0..2 {
        let (status, _, _) = h.post(Some(&token), SELECT.to_vec()).await;
        assert_eq!(status, 404);
    }
    assert_eq!(h.calls.load(Ordering::SeqCst), 1);
    let path = format!("/internal/branches/{branch}/invalidate");
    let (status, _) = h.internal_post(&path, Some(TOKEN), Value::Null).await;
    assert_eq!(status, 204);
    let (status, _, _) = h.post(Some(&token), SELECT.to_vec()).await;
    assert_eq!(status, 404);
    assert_eq!(h.calls.load(Ordering::SeqCst), 2);
}

/// The console route: the bearer, a well-formed body, a served database, one database.
#[tokio::test]
async fn console_route_checks_the_bearer_the_body_and_the_database() {
    let h = harness(served).await;
    let path = format!("/internal/console/{}/query", Uuid::new_v4());
    let tree = serde_json::json!({ "v": 1, "op": "select", "from": "t" });
    let request = serde_json::json!({ "role": "editor", "database": "ghost", "tree": tree });
    for bearer in [None, Some("nope")] {
        let (status, _) = h.internal_post(&path, bearer, request.clone()).await;
        assert_eq!(status, 401, "{bearer:?}");
    }
    assert_eq!(h.calls.load(Ordering::SeqCst), 0);
    for bad in [
        serde_json::json!({ "database": null, "tree": tree }),
        serde_json::json!({ "role": "editor", "database": null, "tree": [] }),
        serde_json::json!({ "role": "editor", "database": "main", "tree": { "v": 2, "op": "select", "from": "t" } }),
        serde_json::json!({ "role": "editor", "database": "main",
            "tree": { "v": 1, "op": "select", "from": "t", "database": "analytics" } }),
    ] {
        let (status, json) = h.internal_post(&path, Some(TOKEN), bad.clone()).await;
        assert_eq!(status, 400, "{bad}: {json}");
        assert_eq!(json["error"]["code"], "invalid_query", "{bad}");
    }
    let (status, json) = h.internal_post(&path, Some(TOKEN), request).await;
    assert_eq!(status, 404, "{json}");
    assert_eq!(json["error"]["code"], "unknown_database");
    assert_eq!(json["error"]["message"], UNKNOWN_DATABASE);
}
