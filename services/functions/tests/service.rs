//! The functions service over HTTP: a fake directory, a fake runtime that records what it was
//! sent, the real [`Workerd`] host and an in-memory bundle store.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use axum::Router;
use axum::body::Bytes;
use axum::extract::Request;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use chelabase_auth::internal::{
    AuthMethod, DataIdentity, IDENTITY_HEADER, Identity, InternalSigner, InternalVerifier,
};
use chelabase_metadata::FunctionAuth;
use chelabase_storage::{MemoryStore, ObjectStore};
use chelabase_types::data_key::DataKeyKind;
use functions::app::{AppState, control_router, public_router, runtime_router};
use functions::bundles::Bundles;
use functions::directory::{Directory, Parts};
use functions::host::Workerd;
use functions::resolver::{Resolver, TTL};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;
use zeroize::Zeroizing;

const ORCH: &str = "orchestrator-bearer-0123456789abcdef0123";
const WORKER: &str = "worker-bearer-0123456789abcdef0123456789";
const PUBLIC_KEY: &str = "chk_pub_the_callers_public_key";
const SECRET_KEY: &str = "chk_sec_the_callers_secret_key";
const USER_TOKEN: &str = "the.users.token";

/// What the fake runtime was sent.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    uri: String,
    headers: HeaderMap,
    body: Bytes,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
    fn env(&self) -> Value {
        serde_json::from_slice(
            &STANDARD
                .decode(self.header("x-chelabase-env").unwrap())
                .unwrap(),
        )
        .unwrap()
    }
    fn caller(&self) -> Value {
        serde_json::from_str(self.header("chelabase-caller").unwrap()).unwrap()
    }
}

/// The fake runtime: the function name (the path's first segment) picks what it does.
async fn runtime(seen: Arc<Mutex<Vec<Seen>>>) -> String {
    let app = Router::new().fallback(move |req: Request| {
        let seen = seen.clone();
        async move {
            let (parts, body) = req.into_parts();
            let body = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            let path = parts.uri.path().to_owned();
            seen.lock().unwrap().push(Seen {
                method: parts.method.to_string(),
                uri: parts.uri.to_string(),
                headers: parts.headers,
                body,
            });
            let name = path
                .trim_start_matches('/')
                .split('/')
                .next()
                .unwrap_or_default()
                .to_owned();
            match name.as_str() {
                "slow" => {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    "late".into_response()
                }
                "boom" => (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response(),
                "nostart" => (
                    StatusCode::BAD_GATEWAY,
                    [("x-chelabase-error", "bundle failed to start")],
                    "bundle failed to start\n",
                )
                    .into_response(),
                _ => (
                    [
                        ("x-chelabase-leak", "1"),
                        ("chelabase-leak", "1"),
                        ("x-kept", "1"),
                    ],
                    format!("hi from {name}"),
                )
                    .into_response(),
            }
        }
    });
    serve(app).await
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// `(database, auth, enabled)`.
type Entry = (Uuid, FunctionAuth, bool);

/// A directory of `(branch, name) -> Entry`.
#[derive(Default)]
struct Fake {
    functions: Mutex<HashMap<(Uuid, String), Entry>>,
    live: Mutex<HashSet<Uuid>>,
    errors: Mutex<Vec<(Uuid, Uuid, String)>>,
}

fn function_id(name: &str) -> Uuid {
    Uuid::from_slice(&Sha256::digest(name.as_bytes())[..16]).unwrap()
}

#[async_trait::async_trait]
impl Directory for Fake {
    async fn resolve(&self, branch: Uuid, name: &str) -> anyhow::Result<Option<Parts>> {
        let f = self
            .functions
            .lock()
            .unwrap()
            .get(&(branch, name.to_owned()))
            .copied();
        Ok(f.map(|(database_id, auth, enabled)| Parts {
            database_id,
            function_id: function_id(name),
            bundle_sha256: "b".repeat(64),
            auth,
            enabled,
            secrets: vec![("API_KEY".into(), Zeroizing::new("s3cret".into()))],
            service_key: None,
            service: None,
        }))
    }
    async fn record_error(&self, branch: Uuid, function: Uuid, error: &str) -> anyhow::Result<()> {
        self.errors
            .lock()
            .unwrap()
            .push((branch, function, error.to_owned()));
        Ok(())
    }
    async fn database_live(&self, database: Uuid) -> anyhow::Result<bool> {
        Ok(self.live.lock().unwrap().contains(&database))
    }
}

struct H {
    public: String,
    /// The internal listener's runtime side (bundle GET only).
    runtime: String,
    /// Its control side (bundle PUT and invoke only).
    control: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    dir: Arc<Fake>,
    store: Arc<MemoryStore>,
    signer: InternalSigner,
    db: Uuid,
    branch: Uuid,
    http: chelabase_http::Client,
}

async fn harness() -> H {
    harness_with(None).await
}

async fn harness_with(runtime_url: Option<String>) -> H {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let runtime_url = match runtime_url {
        Some(u) => u,
        None => runtime(seen.clone()).await,
    };
    let (private, public) = chelabase_auth::internal::generate_keypair_pem();
    let dir = Arc::new(Fake::default());
    let (db, branch) = (Uuid::new_v4(), Uuid::new_v4());
    dir.live.lock().unwrap().insert(db);
    for (name, auth) in [
        ("hello", FunctionAuth::Key),
        ("slow", FunctionAuth::Key),
        ("boom", FunctionAuth::Key),
        ("nostart", FunctionAuth::Key),
        ("users", FunctionAuth::AppUser),
        ("open", FunctionAuth::Public),
    ] {
        dir.functions
            .lock()
            .unwrap()
            .insert((branch, name.into()), (db, auth, true));
    }
    dir.functions
        .lock()
        .unwrap()
        .insert((branch, "off".into()), (db, FunctionAuth::Public, false));
    let store = Arc::new(MemoryStore::default());
    let resolver = Arc::new(Resolver::new(
        dir.clone(),
        "https://api.example".into(),
        TTL,
    ));
    let state = Arc::new(AppState::new(
        resolver,
        Arc::new(Workerd::new(&runtime_url, Duration::from_millis(500)).unwrap()),
        InternalVerifier::from_pem(public.as_bytes()).unwrap(),
        Bundles::new(store.clone()),
        ORCH.into(),
        WORKER.into(),
    ));
    H {
        public: serve(public_router(state.clone())).await,
        runtime: serve(runtime_router(state.clone())).await,
        control: serve(control_router(state)).await,
        seen,
        dir,
        store,
        signer: InternalSigner::from_pem(private.as_bytes()).unwrap(),
        db,
        branch,
        http: chelabase_http::client_builder().build().unwrap(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Cred {
    Public,
    Secret,
    User,
    None,
}

impl H {
    /// The gateway's identity for `cred` on `branch`, for `audience`.
    fn identity(&self, cred: Cred, branch: Uuid, audience: &str) -> Option<String> {
        let (kind, role, claims) = match cred {
            Cred::Public => (DataKeyKind::Public, "anon", json!({})),
            Cred::Secret => (DataKeyKind::Secret, "service", json!({})),
            Cred::User => (
                DataKeyKind::Public,
                "authenticated",
                json!({"sub": "user-1", "role": "authenticated"}),
            ),
            Cred::None => return None,
        };
        let identity = Identity {
            sub: Uuid::new_v4().to_string(),
            auth: AuthMethod::DataKey,
            request_id: "rid".into(),
            data: Some(DataIdentity {
                branch_id: branch,
                key_id: Uuid::new_v4(),
                kind,
                role: role.into(),
                claims,
                databases: None,
                project_id: Some(Uuid::new_v4()),
                role_oid: None,
                server_vouched: false,
            }),
            email: None,
            name: None,
        };
        Some(self.signer.sign(audience, &identity).unwrap())
    }

    /// A call the way the gateway forwards it: the identity and, for a public key, the
    /// forwarded key and user token.
    fn call(&self, method: reqwest::Method, path: &str, cred: Cred) -> reqwest::RequestBuilder {
        let mut req = self.http.request(method, format!("{}{path}", self.public));
        if let Some(token) = self.identity(cred, self.branch, "functions") {
            req = req.header(IDENTITY_HEADER, token);
        }
        match cred {
            Cred::Public => req.header("chelabase-forwarded-key", PUBLIC_KEY),
            Cred::User => req
                .header("chelabase-forwarded-key", PUBLIC_KEY)
                .header("chelabase-forwarded-user", USER_TOKEN),
            _ => req,
        }
    }

    fn last(&self) -> Seen {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("the runtime was called")
    }

    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    fn errors(&self) -> Vec<String> {
        self.dir
            .errors
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, e)| e.clone())
            .collect()
    }
}

#[tokio::test]
async fn a_public_key_call_runs_the_function_with_its_env_and_caller() {
    let h = harness().await;
    let resp = h
        .call(
            reqwest::Method::POST,
            "/functions/v1/hello/sub/path?x=1",
            Cred::Public,
        )
        .header("x-other", "kept")
        .body("payload")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("x-kept").unwrap(), "1");
    assert_eq!(resp.text().await.unwrap(), "hi from hello");
    let seen = h.last();
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.uri, "/hello/sub/path?x=1");
    assert_eq!(seen.body, "payload");
    assert_eq!(seen.header("x-other"), Some("kept"));
    assert_eq!(
        seen.header("x-chelabase-bundle").unwrap(),
        format!("{}/{}", h.db, "b".repeat(64))
    );
    assert_eq!(
        seen.env(),
        json!({"API_KEY": "s3cret", "CHELABASE_URL": "https://api.example", "CHELABASE_BRANCH_ID": h.branch.to_string()})
    );
    assert_eq!(seen.caller(), json!({"kind": "public_key", "role": "anon"}));
    assert_eq!(seen.header("chelabase-caller-key"), Some(PUBLIC_KEY));
    assert_eq!(seen.header("chelabase-caller-token"), None);
    assert_eq!(
        seen.header(IDENTITY_HEADER),
        None,
        "the gateway's identity stays here"
    );
    assert_eq!(seen.header("chelabase-forwarded-key"), None);
}

/// The function sees its sub-path exactly as the client sent it: never percent-decoded, so an
/// encoded `?`, `/` or `%` stays one path segment and dot segments don't collapse.
#[tokio::test]
async fn the_sub_path_is_forwarded_raw() {
    let h = harness().await;
    for (path, want) in [
        ("/functions/v1/hello/a%3Fb%3D1", "/hello/a%3Fb%3D1"),
        (
            "/functions/v1/hello/a%3Fb%3D1?x=%3F",
            "/hello/a%3Fb%3D1?x=%3F",
        ),
        ("/functions/v1/hello/x%2F..%2Fy", "/hello/x%2F..%2Fy"),
        ("/functions/v1/hello/100%25/done", "/hello/100%25/done"),
        ("/functions/v1/hello/a%2Fb/", "/hello/a%2Fb/"),
    ] {
        let resp = h
            .call(reqwest::Method::GET, path, Cred::Public)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200, "{path}");
        assert_eq!(h.last().uri, want, "{path}");
    }
    for (rest, want) in [
        ("a%3Fb%3D1", "/open/a%3Fb%3D1"),
        ("x%2F..%2Fy", "/open/x%2F..%2Fy"),
        ("100%25", "/open/100%25"),
    ] {
        let url = format!("{}/functions/v1/public/{}/open/{rest}", h.public, h.branch);
        let resp = h.http.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 200, "{rest}");
        assert_eq!(h.last().uri, want, "{rest}");
    }
}

#[tokio::test]
async fn an_app_user_call_carries_the_users_token_and_claims() {
    let h = harness().await;
    let resp = h
        .call(reqwest::Method::GET, "/functions/v1/users", Cred::User)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let seen = h.last();
    assert_eq!(seen.caller()["kind"], "app_user");
    assert_eq!(seen.caller()["user_id"], "user-1");
    assert_eq!(seen.header("chelabase-caller-key"), Some(PUBLIC_KEY));
    assert_eq!(seen.header("chelabase-caller-token"), Some(USER_TOKEN));
}

#[tokio::test]
async fn a_secret_key_never_reaches_the_runtime() {
    let h = harness().await;
    // Even a forwarded-key header (forged, or a gateway slip) holding a secret key is dropped.
    let resp = h
        .call(reqwest::Method::GET, "/functions/v1/hello", Cred::Secret)
        .header("chelabase-forwarded-key", SECRET_KEY)
        .header("authorization", format!("Bearer {SECRET_KEY}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let seen = h.last();
    assert_eq!(
        seen.caller(),
        json!({"kind": "secret_key", "role": "service"})
    );
    assert_eq!(seen.header("chelabase-caller-key"), None);
    for (name, value) in &seen.headers {
        assert!(
            !value.to_str().unwrap_or_default().contains(SECRET_KEY),
            "{name}"
        );
    }
    assert!(!seen.env().to_string().contains(SECRET_KEY));
}

#[tokio::test]
async fn forged_internal_and_caller_headers_are_replaced() {
    let h = harness().await;
    let other_bundle = format!("{}/{}", Uuid::new_v4(), "c".repeat(64));
    let resp = h
        .call(reqwest::Method::GET, "/functions/v1/hello", Cred::Public)
        .header("x-chelabase-bundle", &other_bundle)
        .header(
            "x-chelabase-env",
            STANDARD.encode(r#"{"API_KEY":"forged"}"#),
        )
        .header(
            "chelabase-caller",
            r#"{"kind":"secret_key","role":"postgres"}"#,
        )
        .header("chelabase-caller-token", "forged")
        .header("chelabase-caller-key", "forged")
        .header("x-chelabase-anything", "forged")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let seen = h.last();
    assert_ne!(
        seen.header("x-chelabase-bundle"),
        Some(other_bundle.as_str())
    );
    assert_eq!(seen.env()["API_KEY"], "s3cret");
    assert_eq!(seen.caller(), json!({"kind": "public_key", "role": "anon"}));
    assert_eq!(seen.header("chelabase-caller-token"), None);
    assert_eq!(seen.header("chelabase-caller-key"), Some(PUBLIC_KEY));
    assert_eq!(seen.header("x-chelabase-anything"), None);
    // One value each: the forged copy was removed, not kept beside ours.
    for name in [
        "x-chelabase-bundle",
        "x-chelabase-env",
        "chelabase-caller",
        "chelabase-caller-key",
    ] {
        assert_eq!(seen.headers.get_all(name).iter().count(), 1, "{name}");
    }
}

#[tokio::test]
async fn our_headers_never_reach_the_caller() {
    let h = harness().await;
    let resp = h
        .call(reqwest::Method::GET, "/functions/v1/hello", Cred::Public)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("x-chelabase-leak").is_none());
    assert!(resp.headers().get("chelabase-leak").is_none());
    let resp = h
        .call(reqwest::Method::GET, "/functions/v1/nostart", Cred::Public)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
    assert!(resp.headers().get("x-chelabase-error").is_none());
}

#[tokio::test]
async fn the_branch_is_the_keys() {
    let h = harness().await;
    let other = Uuid::new_v4();
    let token = h.identity(Cred::Public, other, "functions").unwrap();
    let resp = h
        .http
        .get(format!("{}/functions/v1/hello", h.public))
        .header(IDENTITY_HEADER, token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "function_not_found");
    assert_eq!(h.calls(), 0, "branch B's function was never reached");
}

#[tokio::test]
async fn unknown_and_disabled_functions_are_404() {
    let h = harness().await;
    for path in [
        "/functions/v1/nope",
        "/functions/v1/off",
        "/functions/v1/off/x",
    ] {
        let resp = h
            .call(reqwest::Method::GET, path, Cred::Public)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "{path}");
    }
    let resp = h
        .http
        .get(format!("{}/functions/v1/public/{}/off", h.public, h.branch))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert_eq!(h.calls(), 0);
}

#[tokio::test]
async fn the_key_form_needs_the_gateways_identity_for_this_service() {
    let h = harness().await;
    let resp = h
        .http
        .get(format!("{}/functions/v1/hello", h.public))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let rest_token = h.identity(Cred::Public, h.branch, "rest").unwrap();
    let resp = h
        .http
        .get(format!("{}/functions/v1/hello", h.public))
        .header(IDENTITY_HEADER, rest_token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401, "an identity minted for another service");
    let resp = h
        .http
        .get(format!("{}/functions/v1/open", h.public))
        .header(IDENTITY_HEADER, "not.a.token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    assert_eq!(h.calls(), 0);
}

/// Every cell of the auth matrix over HTTP: (function, credential) on the key form, and the
/// keyless `/public/` form for each function.
#[tokio::test]
async fn auth_matrix_over_http() {
    let h = harness().await;
    let creds = [Cred::Public, Cred::Secret, Cred::User, Cred::None];
    let table = [
        ("hello", [200, 200, 200, 401]),
        ("users", [401, 200, 200, 401]),
        ("open", [200, 200, 200, 401]),
    ];
    for (name, want) in table {
        for (cred, want) in creds.iter().zip(want) {
            let resp = h
                .call(
                    reqwest::Method::GET,
                    &format!("/functions/v1/{name}"),
                    *cred,
                )
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), want, "{name} with {cred:?}");
        }
    }
    for (name, want) in [("hello", 404), ("users", 404), ("open", 200)] {
        for path in [
            format!("/functions/v1/public/{}/{name}", h.branch),
            format!("/functions/v1/public/{}/{name}/x", h.branch),
        ] {
            // Credentials don't matter on the keyless form: none is checked or forwarded.
            for cred in creds {
                let mut req = h.http.get(format!("{}{path}", h.public));
                if cred != Cred::None {
                    req = req.header("authorization", format!("Bearer {PUBLIC_KEY}"));
                }
                let resp = req.send().await.unwrap();
                assert_eq!(resp.status(), want, "{path} with {cred:?}");
                if want == 200 {
                    let seen = h.last();
                    assert_eq!(seen.caller(), json!({"kind": "anonymous", "role": null}));
                    assert_eq!(seen.header("authorization"), None);
                    assert_eq!(seen.header("chelabase-caller-key"), None);
                }
            }
        }
    }
    let resp = h
        .http
        .get(format!("{}/functions/v1/public/not-a-uuid/open", h.public))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn a_function_that_never_answers_is_504_and_recorded() {
    let h = harness().await;
    let started = std::time::Instant::now();
    let resp = h
        .call(reqwest::Method::GET, "/functions/v1/slow", Cred::Public)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 504);
    assert!(started.elapsed() < Duration::from_secs(5));
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "function_timeout");
    tokio::task::yield_now().await;
    wait_for(|| !h.errors().is_empty()).await;
    assert_eq!(h.errors(), vec!["timed out"]);
}

#[tokio::test]
async fn a_5xx_is_passed_on_and_recorded_with_the_runtimes_reason() {
    let h = harness().await;
    let resp = h
        .call(reqwest::Method::GET, "/functions/v1/boom", Cred::Public)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 500);
    assert_eq!(resp.text().await.unwrap(), "boom");
    let resp = h
        .call(reqwest::Method::GET, "/functions/v1/nostart", Cred::Public)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
    wait_for(|| h.errors().len() == 2).await;
    let mut errors = h.errors();
    errors.sort();
    assert_eq!(
        errors,
        vec!["bundle failed to start", "the function answered 500"]
    );
    // The same error again within the quiet window isn't written again.
    h.call(reqwest::Method::GET, "/functions/v1/boom", Cred::Public)
        .send()
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.errors().len(), 2);
}

#[tokio::test]
async fn an_unreachable_runtime_is_502() {
    let h = harness_with(Some("http://127.0.0.1:1".into())).await;
    let resp = h
        .call(reqwest::Method::GET, "/functions/v1/hello", Cred::Public)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
}

#[tokio::test]
async fn bodies_over_6_mib_are_413() {
    let h = harness().await;
    let big = vec![b'x'; 6 * 1024 * 1024 + 1];
    let resp = h
        .call(reqwest::Method::POST, "/functions/v1/hello", Cred::Public)
        .body(big)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413);
    // Streamed without a length, too.
    let chunks = futures_util::stream::iter(
        (0..7).map(|_| Ok::<_, std::io::Error>(vec![b'x'; 1024 * 1024])),
    );
    let resp = h
        .call(reqwest::Method::POST, "/functions/v1/hello", Cred::Public)
        .body(reqwest::Body::wrap_stream(chunks))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413);
    assert_eq!(h.calls(), 0);
    let exact = vec![b'x'; 6 * 1024 * 1024];
    let resp = h
        .call(reqwest::Method::POST, "/functions/v1/hello", Cred::Public)
        .body(exact)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn the_public_listener_serves_no_internal_route() {
    let h = harness().await;
    let sha = "a".repeat(64);
    for (method, path) in [
        (
            reqwest::Method::GET,
            format!("/internal/bundles/{}/{sha}", h.db),
        ),
        (
            reqwest::Method::PUT,
            format!("/internal/bundles/{}/{sha}", h.db),
        ),
        (
            reqwest::Method::POST,
            format!("/internal/invoke/{}/hello", h.branch),
        ),
    ] {
        let resp = h
            .http
            .request(method, format!("{}{path}", h.public))
            .bearer_auth(WORKER)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "{path}");
    }
    // And the internal listener serves no public route.
    let resp = h
        .call(reqwest::Method::GET, "/functions/v1/hello", Cred::Public)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let resp = h
        .http
        .get(format!(
            "{}/functions/v1/public/{}/open",
            h.runtime, h.branch
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

fn sha(body: &[u8]) -> String {
    hex::encode(Sha256::digest(body))
}

#[tokio::test]
async fn bundle_put_and_get() {
    let h = harness().await;
    let body =
        br#"{"compatibilityDate":"2026-10-01","mainModule":"main.js","modules":[]}"#.to_vec();
    let hash = sha(&body);
    let url = format!("{}/internal/bundles/{}/{hash}", h.control, h.db);
    let get_url = format!("{}/internal/bundles/{}/{hash}", h.runtime, h.db);
    let put = |bearer: Option<&str>, body: Vec<u8>, url: String| {
        let mut req = h.http.put(url).body(body);
        if let Some(b) = bearer {
            req = req.bearer_auth(b);
        }
        req.send()
    };
    // Per-caller rights: none or an unknown bearer is 401, the worker's 403.
    assert_eq!(
        put(None, body.clone(), url.clone()).await.unwrap().status(),
        401
    );
    assert_eq!(
        put(Some("nope"), body.clone(), url.clone())
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        put(Some(WORKER), body.clone(), url.clone())
            .await
            .unwrap()
            .status(),
        403
    );
    assert!(h.store.list("").await.unwrap().is_empty());

    // A body that doesn't hash to the path: 422, nothing written.
    let resp = put(Some(ORCH), b"{}".to_vec(), url.clone()).await.unwrap();
    assert_eq!(resp.status(), 422);
    assert!(h.store.list("").await.unwrap().is_empty());

    // Twice: both succeed, one object, the second write's time.
    let first = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
    let second = first + Duration::from_secs(60);
    h.store.set_now(first);
    assert_eq!(
        put(Some(ORCH), body.clone(), url.clone())
            .await
            .unwrap()
            .status(),
        204
    );
    h.store.set_now(second);
    assert_eq!(
        put(Some(ORCH), body.clone(), url.clone())
            .await
            .unwrap()
            .status(),
        204
    );
    let listed = h.store.list_dated("").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].key, format!("{}/{hash}", h.db));
    assert_eq!(listed[0].modified, second);

    // GET needs no bearer and serves the exact bytes.
    let resp = h.http.get(&get_url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap(), body);

    // Keys are per database: another database's id with this hash names nothing.
    let other = Uuid::new_v4();
    h.dir.live.lock().unwrap().insert(other);
    let resp = h
        .http
        .get(format!("{}/internal/bundles/{other}/{hash}", h.runtime))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    for bad in [
        format!("{}/{}", h.db.to_string().to_uppercase(), hash),
        format!("{}/{}", h.db, hash.to_uppercase()),
    ] {
        let resp = h
            .http
            .get(format!("{}/internal/bundles/{bad}", h.runtime))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "{bad}");
    }

    // A tombstoned or unknown database: 404, nothing written.
    let gone = Uuid::new_v4();
    let resp = put(
        Some(ORCH),
        body.clone(),
        format!("{}/internal/bundles/{gone}/{hash}", h.control),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 404);
    assert_eq!(h.store.list("").await.unwrap().len(), 1);

    // Over 10 MiB: 413.
    let big = vec![b' '; 10 * 1024 * 1024 + 1];
    let resp = put(
        Some(ORCH),
        big.clone(),
        format!("{}/internal/bundles/{}/{}", h.control, h.db, sha(&big)),
    )
    .await
    .unwrap();
    assert_eq!(resp.status(), 413);
}

#[tokio::test]
async fn internal_invoke_takes_the_workers_bearer_only() {
    let h = harness().await;
    let url = format!("{}/internal/invoke/{}/users", h.control, h.branch);
    assert_eq!(h.http.post(&url).send().await.unwrap().status(), 401);
    assert_eq!(
        h.http
            .post(&url)
            .bearer_auth(ORCH)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(h.calls(), 0);
    let resp = h
        .http
        .post(&url)
        .bearer_auth(WORKER)
        .body(r#"{"event":1}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let seen = h.last();
    assert_eq!(seen.uri, "/users");
    assert_eq!(seen.caller(), json!({"kind": "internal", "role": null}));
    assert_eq!(
        seen.header("authorization"),
        None,
        "the worker's bearer stays here"
    );
    let off = format!("{}/internal/invoke/{}/off", h.control, h.branch);
    assert_eq!(
        h.http
            .post(&off)
            .bearer_auth(WORKER)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}

/// Edge functions m7: the worker's `{"type": "scheduled"}` body is marked for the runtime with
/// `x-chelabase-event: scheduled` (its supervisor then runs the `scheduled` handler); any other
/// body, and the same body on a public call (even with a forged header), is a plain request.
#[tokio::test]
async fn a_scheduled_body_from_the_worker_is_marked_for_the_runtime() {
    let h = harness().await;
    let url = format!("{}/internal/invoke/{}/users", h.control, h.branch);
    let body = r#"{"type":"scheduled","cron":"* * * * *","scheduledTime":1767225600000}"#;
    let resp = h
        .http
        .post(&url)
        .bearer_auth(WORKER)
        .header("x-chelabase-event", "forged")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let seen = h.last();
    assert_eq!(seen.header("x-chelabase-event"), Some("scheduled"));
    assert_eq!(seen.headers.get_all("x-chelabase-event").iter().count(), 1);
    assert_eq!(seen.body, body.as_bytes());

    for other in [r#"{"type":"table"}"#, "not json", ""] {
        let resp = h
            .http
            .post(&url)
            .bearer_auth(WORKER)
            .body(other)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(h.last().header("x-chelabase-event"), None, "{other}");
    }

    let resp = h
        .call(reqwest::Method::POST, "/functions/v1/hello", Cred::Public)
        .header("x-chelabase-event", "scheduled")
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(h.last().header("x-chelabase-event"), None);
}

/// Polls `cond` for up to 2 s (the error is written in the background).
async fn wait_for(cond: impl Fn() -> bool) {
    for _ in 0..200 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("condition not met");
}

/// #181 (audit S1): the runtime's side of the internal listener serves bundle GETs only, so a
/// runtime (or anything on its network) can't upload a bundle or invoke a function, bearer or
/// not; the control side (the orchestrator's and the worker's network) serves no GET.
#[tokio::test]
async fn each_internal_side_serves_only_its_own_calls() {
    let h = harness().await;
    let body = br#"{"compatibilityDate":"2026-10-01","mainModule":"main.js","modules":[]}"#.to_vec();
    let hash = sha(&body);
    let path = format!("/internal/bundles/{}/{hash}", h.db);
    let put = |base: &str, bearer: &str| h.http.put(format!("{base}{path}")).bearer_auth(bearer).body(body.clone()).send();
    for bearer in [ORCH, WORKER, "none"] {
        assert_eq!(put(&h.runtime, bearer).await.unwrap().status(), 404, "runtime-side PUT with {bearer}");
    }
    assert!(h.store.list("").await.unwrap().is_empty(), "nothing was written");
    let invoke = format!("/internal/invoke/{}/users", h.branch);
    for bearer in [WORKER, ORCH, "none"] {
        let resp = h.http.post(format!("{}{invoke}", h.runtime)).bearer_auth(bearer).send().await.unwrap();
        assert_eq!(resp.status(), 404, "runtime-side invoke with {bearer}");
    }
    assert_eq!(h.calls(), 0, "no function ran");
    // The control side takes them, and serves no bundle.
    assert_eq!(put(&h.control, ORCH).await.unwrap().status(), 204);
    assert_eq!(h.http.get(format!("{}{path}", h.control)).send().await.unwrap().status(), 404);
    assert_eq!(h.http.get(format!("{}{path}", h.runtime)).send().await.unwrap().status(), 200);
}
