//! The HTTP edge without a database: identity, the schema route's key check, connection
//! errors and the body cap. The connection source is a fake; nothing here reaches Postgres.

// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods)]

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
        email: None,
        name: None,
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
        "host": std::net::Ipv4Addr::LOCALHOST.to_string(), "port": 1, "database": "main",
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

/// T5 (P40): the full-cell route, `POST /internal/console/{branch_id}/cell`, on the internal
/// listener: the bearer first (nothing is fetched without it), then the body
/// (`deny_unknown_fields`, every field but `database` required; 400 `invalid_query` as on
/// `/query`), then the database as the console picks it, and the connection's failures as the
/// internal routes answer them.
#[tokio::test]
async fn console_cell_needs_the_bearer_and_refuses_unknown_fields() {
    let h = harness(served).await;
    let path = format!("/internal/console/{}/cell", Uuid::new_v4());
    let request = serde_json::json!({
        "role": "viewer_r", "database": "ghost", "schema": "public", "table": "notes",
        "key": { "id": 1 }, "column": "body"
    });
    for bearer in [None, Some("nope")] {
        let (status, json) = h.internal_post(&path, bearer, request.clone()).await;
        assert_eq!(status, 401, "{bearer:?}: {json}");
        assert_eq!(json["error"]["code"], "unauthorized", "{bearer:?}");
    }
    assert_eq!(h.calls.load(Ordering::SeqCst), 0);
    let with = |key: &str, value: Value| {
        let mut body = request.clone();
        body[key] = value;
        body
    };
    let without = |key: &str| {
        let mut body = request.clone();
        body.as_object_mut().unwrap().remove(key);
        body
    };
    for bad in [
        with("tree", serde_json::json!({ "v": 1, "op": "select", "from": "t" })),
        with("extra", serde_json::json!(true)),
        without("role"),
        without("schema"),
        without("table"),
        without("key"),
        without("column"),
        with("key", serde_json::json!([1])),
        with("column", serde_json::json!(1)),
        serde_json::json!([]),
    ] {
        let (status, json) = h.internal_post(&path, Some(TOKEN), bad.clone()).await;
        assert_eq!(status, 400, "{bad}: {json}");
        assert_eq!(json["error"]["code"], "invalid_query", "{bad}");
    }
    assert_eq!(h.calls.load(Ordering::SeqCst), 0, "a refused body fetches nothing");
    // A well-formed body reaches the database pick: `ghost` isn't served.
    let (status, json) = h.internal_post(&path, Some(TOKEN), request.clone()).await;
    assert_eq!(status, 404, "{json}");
    assert_eq!(json["error"]["code"], "unknown_database");
    assert_eq!(json["error"]["message"], UNKNOWN_DATABASE);
    // `database` may be left out (main).
    let mut main = request.clone();
    main.as_object_mut().unwrap().remove("database");
    let (status, json) = h.internal_post(&path, Some(TOKEN), main).await;
    assert_ne!((status, &json["error"]["code"]), (400, &Value::from("invalid_query")), "{json}");

    // The branch's states, as the internal routes answer them.
    let h = harness(not_ready).await;
    let (status, json) = h.internal_post(&path, Some(TOKEN), request.clone()).await;
    assert_eq!((status, json), (409, serde_json::json!({ "status": "resetting" })));
    let h = harness(unknown).await;
    let (status, json) = h.internal_post(&path, Some(TOKEN), request).await;
    assert_eq!((status, &json["error"]["code"]), (404, &Value::from("unknown_branch")), "{json}");
}

/// Trees that use the console-only forms (RT1 T4: `text_of` select items, `"text": true`
/// filters), each as it stands in a select.
fn console_only_trees() -> Vec<Value> {
    vec![
        serde_json::json!({ "v": 1, "op": "select", "from": "t",
            "select": [{ "text_of": "id", "as": "id_text" }, "name"] }),
        serde_json::json!({ "v": 1, "op": "select", "from": "t",
            "where": { "col": "id", "eq": "9007199254740993", "text": true } }),
        serde_json::json!({ "v": 1, "op": "count", "from": "t",
            "where": { "col": "id", "in": ["1", "2"], "text": true } }),
        serde_json::json!({ "v": 1, "op": "batch", "ops": [
            { "op": "select", "from": "t" },
            { "op": "delete", "from": "t", "where": { "col": "id", "eq": "1", "text": true } },
        ] }),
    ]
}

/// The data API route refuses the console-only forms as plain invalid queries (like any
/// unknown key), before any connection, for every kind of key and however the body is shaped:
/// the grammar mode is the route's, not the body's.
#[tokio::test]
async fn the_data_route_refuses_the_console_only_forms_before_any_connection() {
    let h = harness(served).await;
    for kind in [DataKeyKind::Public, DataKeyKind::Secret] {
        let token = h.token("rest", Some(kind));
        for tree in console_only_trees() {
            let mut bodies = vec![tree.clone()];
            // A body that tries to ask for the console grammar.
            for (field, value) in [
                ("mode", serde_json::json!("console")),
                ("console", serde_json::json!(true)),
                ("parse_mode", serde_json::json!("Console")),
            ] {
                let mut with_field = tree.clone();
                with_field[field] = value;
                bodies.push(with_field);
            }
            let mut named = tree.clone();
            named["database"] = serde_json::json!("main");
            bodies.push(named);
            for body in bodies {
                let (status, _, json) = h.post(Some(&token), serde_json::to_vec(&body).unwrap()).await;
                assert_eq!(status, 400, "{body}: {json}");
                assert_eq!(json["error"]["code"], "invalid_query", "{body}");
            }
        }
    }
    assert_eq!(h.calls.load(Ordering::SeqCst), 0, "a refusal must not reach a connection");
}

/// The console route takes the same trees: they get past the parser and on to the database
/// lookup (an unserved database is a 404). Sibling fields in the request or the tree can't
/// change which grammar is used.
#[tokio::test]
async fn the_console_route_accepts_the_console_only_forms() {
    let h = harness(served).await;
    let path = format!("/internal/console/{}/query", Uuid::new_v4());
    for tree in console_only_trees() {
        let request = serde_json::json!({ "role": "editor", "database": "ghost", "tree": tree });
        let (status, json) = h.internal_post(&path, Some(TOKEN), request).await;
        assert_eq!(status, 404, "{tree}: {json}");
        assert_eq!(json["error"]["code"], "unknown_database", "{tree}");
        // A request field naming the data API grammar changes nothing (it isn't a field).
        let request = serde_json::json!({
            "role": "editor", "database": "ghost", "tree": tree, "mode": "DataApi",
        });
        let (status, json) = h.internal_post(&path, Some(TOKEN), request).await;
        assert_eq!(status, 404, "{tree}: {json}");
        // Inside the tree an unknown field stays an unknown field.
        let mut with_field = tree.clone();
        with_field["mode"] = serde_json::json!("DataApi");
        let request = serde_json::json!({ "role": "editor", "database": "ghost", "tree": with_field });
        let (status, json) = h.internal_post(&path, Some(TOKEN), request).await;
        assert_eq!(status, 400, "{with_field}: {json}");
        assert_eq!(json["error"]["code"], "invalid_query");
    }
    // Still the console's grammar, no looser: a bad form is a 400 here too.
    for bad in [
        serde_json::json!({ "v": 1, "op": "select", "from": "t", "select": [{ "text_of": "id" }] }),
        serde_json::json!({ "v": 1, "op": "select", "from": "t",
            "where": { "col": "id", "gt": "1", "text": true } }),
        serde_json::json!({ "v": 1, "op": "select", "from": "t",
            "where": { "col": "id", "eq": 1, "text": true } }),
    ] {
        let request = serde_json::json!({ "role": "editor", "database": "main", "tree": bad });
        let (status, json) = h.internal_post(&path, Some(TOKEN), request).await;
        assert_eq!(status, 400, "{bad}: {json}");
        assert_eq!(json["error"]["code"], "invalid_query", "{bad}");
    }
}

/// `GET path` with an identity: status, headers and the raw body.
async fn get_raw(
    h: &Harness,
    path: &str,
    token: Option<&str>,
) -> (u16, reqwest::header::HeaderMap, String) {
    let mut req = h.http.get(format!("http://{}{path}", h.addr));
    if let Some(token) = token {
        req = req.header(IDENTITY_HEADER, token);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    (status, headers, resp.text().await.unwrap())
}

/// No key, a management identity and a public key: refused exactly as the schema route
/// refuses them, before any connection.
#[tokio::test]
async fn types_refuses_a_public_key() {
    let h = harness(served).await;
    let management = h.token("rest", None);
    let public = h.token("rest", Some(DataKeyKind::Public));
    let other_audience = h.token("orchestrator", Some(DataKeyKind::Secret));
    for token in [
        None,
        Some(management.as_str()),
        Some(public.as_str()),
        Some(other_audience.as_str()),
    ] {
        let schema = get_raw(&h, "/data/v1/schema", token).await;
        assert_eq!(schema.0, 401, "{token:?}");
        for path in [
            "/data/v1/types",
            "/data/v1/types?format=json",
            "/data/v1/types?all_databases=true",
            "/data/v1/types?database=analytics",
        ] {
            let types = get_raw(&h, path, token).await;
            assert_eq!((types.0, &types.2), (schema.0, &schema.2), "{path} {token:?}");
            assert_eq!(
                types.1.get("content-type"),
                schema.1.get("content-type"),
                "{path}"
            );
            assert!(types.1.get("etag").is_none(), "{path}");
        }
    }
    assert_eq!(h.calls.load(Ordering::SeqCst), 0);
}

/// A key whose branch rest can't find gets the schema route's answer; no parameter can
/// name another branch.
#[tokio::test]
async fn types_refuses_a_key_for_another_branch() {
    let h = harness(unknown).await;
    let token = h.token("rest", Some(DataKeyKind::Secret));
    let schema = get_raw(&h, "/data/v1/schema", Some(&token)).await;
    assert_eq!(schema.0, 401);
    for path in [
        "/data/v1/types",
        "/data/v1/types?format=json",
        "/data/v1/types?all_databases=true",
    ] {
        let types = get_raw(&h, path, Some(&token)).await;
        assert_eq!((types.0, &types.2), (schema.0, &schema.2), "{path}");
    }
    let calls = h.calls.load(Ordering::SeqCst);
    let other = Uuid::new_v4();
    for path in [
        format!("/data/v1/types?branch={other}"),
        format!("/data/v1/types?branch_id={other}"),
    ] {
        let (status, _, body) = get_raw(&h, &path, Some(&token)).await;
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!((status, &body["error"]["code"]), (400, &Value::from("invalid_query")), "{path}");
    }
    assert_eq!(h.calls.load(Ordering::SeqCst), calls, "refused before any connection");
}

/// A database the key can't reach: the schema route's 404, for one database and for main
/// under `all_databases`.
#[tokio::test]
async fn types_answers_unknown_databases_as_the_schema_route_does() {
    let h = harness(served).await;
    for (types, schema, token) in [
        (
            "/data/v1/types?database=ghost",
            "/data/v1/schema?database=ghost",
            h.token("rest", Some(DataKeyKind::Secret)),
        ),
        (
            "/data/v1/types?database=analytics&format=json",
            "/data/v1/schema?database=analytics",
            h.listed(DataKeyKind::Secret, &["main"]),
        ),
        (
            "/data/v1/types?all_databases=true",
            "/data/v1/schema",
            h.listed(DataKeyKind::Secret, &["analytics"]),
        ),
    ] {
        let want = get_raw(&h, schema, Some(&token)).await;
        let got = get_raw(&h, types, Some(&token)).await;
        assert_eq!(want.0, 404, "{schema}");
        assert_eq!((got.0, &got.2), (want.0, &want.2), "{types}");
    }
}

/// The CLI's refused combinations, a format that isn't `ts` or `json`, and parameters it
/// doesn't know: 400 `invalid_query` before any connection, with no schema in the text.
#[tokio::test]
async fn types_refuses_options_the_cli_refuses() {
    let h = harness(served).await;
    let token = h.token("rest", Some(DataKeyKind::Secret));
    for path in [
        "/data/v1/types?database=analytics&all_databases=true",
        "/data/v1/types?all_databases=true&database=main",
        "/data/v1/types?format=xml",
        "/data/v1/types?format=TS",
        "/data/v1/types?format=",
        "/data/v1/types?all_databases=yes",
        "/data/v1/types?all_databases=",
        "/data/v1/types?allDatabases=true",
        "/data/v1/types?format=ts&format=json",
        "/data/v1/types?database=main&database=analytics",
    ] {
        let (status, headers, text) = get_raw(&h, path, Some(&token)).await;
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        assert_eq!(status, 400, "{path}: {text}");
        assert_eq!(body["error"]["code"], "invalid_query", "{path}: {text}");
        assert_eq!(
            headers.get("content-type").unwrap(),
            "application/json",
            "{path}"
        );
        assert!(!text.contains("reporting") && !text.contains("public"), "{path}: {text}");
    }
    assert_eq!(h.calls.load(Ordering::SeqCst), 0);
    // `all_databases=false` is the default; the request reaches the connection (none here).
    let (status, _, _) = get_raw(&h, "/data/v1/types?all_databases=false&format=ts", Some(&token)).await;
    assert_eq!(status, 503);
}

/// `GET /internal/types/{branch_id}?role=…`, as the internal listener answers it.
async fn internal_types(h: &Harness, query: &str, bearer: Option<&str>) -> (u16, Value) {
    let path = format!("/internal/types/{}{query}", Uuid::new_v4());
    let header = bearer.map(|b| ("authorization", format!("Bearer {b}")));
    h.get(h.internal, &path, header).await
}

/// The orchestrator's types route: only with the internal bearer, only on the internal
/// listener, and with exactly one non-empty `role` beside the public route's options. Each
/// refusal comes before any connection.
#[tokio::test]
async fn internal_types_needs_the_bearer_and_one_role() {
    let h = harness(served).await;
    for bearer in [None, Some("nope")] {
        let (status, _) = internal_types(&h, "?role=authenticated", bearer).await;
        assert_eq!(status, 401, "{bearer:?}");
    }
    // Not on the public listener, whatever the identity.
    let token = h.token("rest", Some(DataKeyKind::Secret));
    let (status, _, _) =
        get_raw(&h, &format!("/internal/types/{}?role=authenticated", Uuid::new_v4()), Some(&token)).await;
    assert_eq!(status, 404);
    for query in [
        "",
        "?role=",
        "?format=ts",
        "?role=a&role=b",
        "?role=authenticated&format=xml",
        "?role=authenticated&database=main&all_databases=true",
        "?role=authenticated&format=ts&format=json",
        "?role=authenticated&branch=x",
    ] {
        let (status, body) = internal_types(&h, query, Some(TOKEN)).await;
        assert_eq!((status, &body["error"]["code"]), (400, &Value::from("invalid_query")), "{query}: {body}");
    }
    // The unknown-option message lists what this route takes: `role` too.
    let (_, body) = internal_types(&h, "?role=authenticated&branch=x", Some(TOKEN)).await;
    let message = body["error"]["message"].as_str().unwrap();
    for option in ["`role`", "`format`", "`database`", "`all_databases`"] {
        assert!(message.contains(option), "{option} missing: {message}");
    }
    let (status, _, text) = get_raw(&h, "/data/v1/types?branch=x", Some(&h.token("rest", Some(DataKeyKind::Secret)))).await;
    assert_eq!(status, 400);
    assert!(!text.contains("`role`"), "the public route takes no role: {text}");
    assert_eq!(h.calls.load(Ordering::SeqCst), 0);
    // A valid request reaches the connection (nothing listens: 503).
    let (status, _) = internal_types(&h, "?role=authenticated&format=json", Some(TOKEN)).await;
    assert_eq!(status, 503);
}

/// The branch's states, as the internal schema route answers them.
#[tokio::test]
async fn internal_types_passes_branch_states_through() {
    let h = harness(not_ready).await;
    let (status, body) = internal_types(&h, "?role=authenticated", Some(TOKEN)).await;
    assert_eq!((status, &body["status"]), (409, &Value::from("resetting")), "{body}");
    let h = harness(unknown).await;
    let (status, body) = internal_types(&h, "?role=authenticated", Some(TOKEN)).await;
    assert_eq!((status, &body["error"]["code"]), (404, &Value::from("unknown_branch")), "{body}");
    // A database the branch doesn't serve: the public route's answer.
    let h = harness(served).await;
    let (status, body) = internal_types(&h, "?role=authenticated&database=ghost", Some(TOKEN)).await;
    assert_eq!((status, &body["error"]["code"]), (404, &Value::from("unknown_database")), "{body}");
}
