//! The HTTP edge without a database: identity, the schema route's key check, connection
//! errors and the body cap. The connection source is a fake; nothing here reaches Postgres.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use cheladb_auth::internal::{
    AuthMethod, DataIdentity, IDENTITY_HEADER, Identity, InternalSigner, InternalVerifier,
    generate_keypair_pem,
};
use cheladb_types::data_key::DataKeyKind;
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

fn identity(kind: Option<DataKeyKind>) -> Identity {
    let key_id = Uuid::new_v4();
    Identity {
        sub: key_id.to_string(),
        org_id: String::new(),
        auth: if kind.is_some() {
            AuthMethod::DataKey
        } else {
            AuthMethod::PlatformJwt
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
        }),
    }
}

impl Harness {
    fn token(&self, audience: &str, kind: Option<DataKeyKind>) -> String {
        self.signer.sign(audience, &identity(kind)).unwrap()
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

#[tokio::test]
async fn missing_identity_is_401() {
    let h = harness(not_ready).await;
    let platform = h.token("rest", None);
    let other_audience = h.token("orchestrator", Some(DataKeyKind::Secret));
    for token in [
        None,
        Some("garbage"),
        Some(platform.as_str()),
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
            Some(("x-cheladb-identity", token)),
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
            Some(("x-cheladb-identity", token)),
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
