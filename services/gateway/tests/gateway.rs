//! End-to-end: the real gateway binary in front of a mock upstream.
//!
//! Pingora's server loop never returns, so each test spawns the compiled
//! binary on a pre-picked free port and kills it on drop.

use axum::extract::Request;
use axum::{Json, Router};
use chelabase_auth::internal::{generate_keypair_pem, InternalVerifier, IDENTITY_HEADER};
use chelabase_auth::test_support::MockJwks;
use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;

const E2E_PROJECT: &str = "33333333-3333-4333-8333-333333333333";
const STUDIO: &str = "http://localhost:3000";

/// The most recent request the mock upstream received: headers plus `:uri`.
type Seen = Arc<Mutex<Option<HashMap<String, String>>>>;

struct Stack {
    base: String,
    jwks: MockJwks,
    verify: InternalVerifier,
    seen: Seen,
    _gateway: GatewayProcess,
}

struct GatewayProcess {
    child: Child,
    _workdir: tempfile::TempDir,
}

impl Drop for GatewayProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn spawn_upstream() -> (String, Seen) {
    let seen: Seen = Arc::default();
    let record = seen.clone();
    let app = Router::new().fallback(move |req: Request| {
        let record = record.clone();
        async move {
            let mut headers: HashMap<String, String> = req
                .headers()
                .iter()
                .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
                .collect();
            headers.insert(":uri".into(), req.uri().to_string());
            // The HashMap keeps one value per name; record how many identity
            // values actually arrived so duplicates can't hide.
            headers.insert(
                ":identity-count".into(),
                req.headers().get_all(IDENTITY_HEADER).iter().count().to_string(),
            );
            *record.lock().unwrap() = Some(headers);
            (
                [("vary", "Accept-Encoding")],
                Json(serde_json::json!({ "upstream": "ok" })),
            )
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Spawn the gateway binary in front of `upstream_url`, trusting `jwks`.
async fn spawn_gateway(upstream_url: &str, jwks: &MockJwks) -> (String, InternalVerifier, GatewayProcess) {
    spawn_gateway_with(upstream_url, jwks, &[]).await
}

async fn spawn_gateway_with(
    upstream_url: &str,
    jwks: &MockJwks,
    extra_env: &[(&str, &str)],
) -> (String, InternalVerifier, GatewayProcess) {
    // A temp working dir keeps dotenvy from picking up a repo-level .env.
    let workdir = tempfile::tempdir().unwrap();
    let (private_pem, public_pem) = generate_keypair_pem();
    let signing_key = workdir.path().join("internal-signing.pem");
    std::fs::write(&signing_key, private_pem).unwrap();

    let port = free_port();
    let mut command = Command::new(env!("CARGO_BIN_EXE_gateway"));
    command.envs(extra_env.iter().copied());
    let child = command
        .current_dir(workdir.path())
        .env("CHELABASE_GATEWAY_ADDR", format!("127.0.0.1:{port}"))
        .env("CHELABASE_ORCHESTRATOR_URL", upstream_url)
        .env("CHELABASE_API_HOSTS", "")
        .env("CHELABASE_STUDIO_ORIGINS", STUDIO)
        .env("CHELABASE_ZITADEL_ISSUER", jwks.issuer())
        .env("CHELABASE_ZITADEL_JWKS_URL", jwks.jwks_url())
        .env("CHELABASE_ZITADEL_AUDIENCES", "")
        .env("CHELABASE_INTERNAL_SIGNING_KEY", &signing_key)
        .env("RUST_LOG", "warn")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn gateway binary");

    let base = format!("http://127.0.0.1:{port}");
    let process = GatewayProcess {
        child,
        _workdir: workdir,
    };
    wait_ready(&base).await;
    (
        base,
        InternalVerifier::from_pem(public_pem.as_bytes()).unwrap(),
        process,
    )
}

async fn wait_ready(base: &str) {
    let client = reqwest::Client::new();
    for _ in 0..100 {
        if let Ok(resp) = client.get(format!("{base}/_gateway/health")).send().await
            && resp.status() == 200
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("gateway at {base} did not become ready within 5s (see stderr above)");
}

async fn start_stack() -> Stack {
    let jwks = MockJwks::start().await;
    let (upstream, seen) = spawn_upstream().await;
    let (base, verify, gateway) = spawn_gateway(&upstream, &jwks).await;
    Stack {
        base,
        jwks,
        verify,
        seen,
        _gateway: gateway,
    }
}

impl Stack {
    fn bearer(&self) -> String {
        format!("Bearer {}", self.jwks.token_for_org("user-1", "org-1", 300))
    }

    fn upstream_saw(&self) -> Option<HashMap<String, String>> {
        self.seen.lock().unwrap().clone()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticated_request_reaches_upstream_with_internal_identity_only() {
    let stack = start_stack().await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/databases", stack.base))
        .header("authorization", stack.bearer())
        .header(IDENTITY_HEADER, "forged")
        .json(&serde_json::json!({ "name": "demo" }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let response_rid = resp.headers()["x-request-id"].to_str().unwrap().to_string();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["upstream"], "ok");

    let seen = stack.upstream_saw().expect("upstream was called");
    assert!(!seen.contains_key("authorization"), "Authorization must not be forwarded");
    assert_eq!(seen[":identity-count"], "1", "exactly one identity value must reach the upstream");
    let claims = stack
        .verify
        .verify(&seen[IDENTITY_HEADER], "orchestrator")
        .expect("forwarded identity is the gateway-minted one, not the forged header");
    assert_eq!(claims.org_id, "org-1");
    assert_eq!(claims.rid, response_rid);
    assert_eq!(seen["x-request-id"], response_rid);
    assert_eq!(seen["x-forwarded-proto"], "http");
    assert_eq!(seen["x-forwarded-for"], "127.0.0.1");
}

// Review Focus 2.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forged_identity_header_is_stripped_on_public_route() {
    let stack = start_stack().await;
    let resp = reqwest::Client::new()
        .get(format!("{}/health", stack.base))
        .header(IDENTITY_HEADER, "forged")
        .header("authorization", "Bearer whatever")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let seen = stack.upstream_saw().expect("upstream was called");
    assert!(!seen.contains_key(IDENTITY_HEADER));
    assert!(!seen.contains_key("authorization"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incoming_request_id_is_propagated_both_ways() {
    let stack = start_stack().await;
    let resp = reqwest::Client::new()
        .get(format!("{}/health", stack.base))
        .header("x-request-id", "abc-123")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.headers()["x-request-id"], "abc-123");
    assert_eq!(stack.upstream_saw().unwrap()["x-request-id"], "abc-123");
}

// Review Focus 5.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_string_is_forwarded_unchanged() {
    let stack = start_stack().await;
    let id = "6c0f1c2e-2f7e-4c52-9a51-0d8f2f1d7c11";
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/databases/{id}?verbose=1", stack.base))
        .header("authorization", stack.bearer())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        stack.upstream_saw().unwrap()[":uri"],
        format!("/v1/databases/{id}?verbose=1")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undeclared_path_is_404_and_never_proxied() {
    let stack = start_stack().await;
    let resp = reqwest::get(format!("{}/v1/other", stack.base)).await.unwrap();
    assert_eq!(resp.status(), 404);
    assert!(resp.headers().contains_key("x-request-id"));
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body, serde_json::json!({"error": {"code": "not_found", "message": "no route"}}));
    assert!(stack.upstream_saw().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_method_is_405_with_allow() {
    let stack = start_stack().await;
    let resp = reqwest::Client::new()
        .delete(format!("{}/v1/databases", stack.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 405);
    assert_eq!(resp.headers()["allow"], "POST");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn method_not_allowed_keeps_allow() {
    let stack = start_stack().await;
    let resp = reqwest::Client::new()
        .delete(format!("{}/v1/databases", stack.base))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 405);
    assert_eq!(resp.headers()["allow"], "POST");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body,
        serde_json::json!({"error": {"code": "method_not_allowed", "message": "method not allowed"}})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_token_is_401_and_never_proxied() {
    let stack = start_stack().await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/databases", stack.base))
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    assert_eq!(resp.headers()["content-type"], "application/json");
    assert!(stack.upstream_saw().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_is_answered_by_the_gateway() {
    let stack = start_stack().await;
    let resp = reqwest::Client::new()
        .request(reqwest::Method::OPTIONS, format!("{}/v1/databases", stack.base))
        .header("origin", STUDIO)
        .header("access-control-request-method", "POST")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 204);
    assert_eq!(resp.headers()["access-control-allow-origin"], STUDIO);
    // RFC 9110 §8.6: no Content-Length on 204.
    assert!(
        !resp.headers().contains_key("content-length"),
        "204 must not carry Content-Length: {:?}",
        resp.headers().get("content-length")
    );
    assert!(stack.upstream_saw().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxied_response_carries_cors_headers_for_studio() {
    let stack = start_stack().await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/databases", stack.base))
        .header("authorization", stack.bearer())
        .header("origin", STUDIO)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["access-control-allow-origin"], STUDIO);
    let vary: Vec<&str> = resp
        .headers()
        .get_all("vary")
        .iter()
        .map(|v| v.to_str().unwrap())
        .collect();
    let vary = vary.join(", ");
    assert!(vary.contains("Accept-Encoding"), "upstream Vary lost: {vary:?}");
    assert!(vary.contains("Origin"), "CORS Vary missing: {vary:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_down_is_502_json() {
    let jwks = MockJwks::start().await;
    let dead_upstream = format!("http://127.0.0.1:{}", free_port());
    let (base, _verify, _gateway) = spawn_gateway(&dead_upstream, &jwks).await;

    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(resp.status(), 502);
    assert!(resp.headers().contains_key("x-request-id"));
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body,
        serde_json::json!({"error": {"code": "unavailable", "message": "upstream unavailable"}})
    );
}

/// An upstream that starts a chunked 200 response, then drops the connection.
async fn spawn_truncating_upstream() -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\nhello\r\n")
                .await;
            let _ = socket.flush().await;
            // Dropping the socket mid-body simulates an upstream reset.
        }
    });
    format!("http://{addr}")
}

// Final review Important #1: once the upstream response has started, the
// gateway must not splice its own JSON error into the client's body.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_failure_after_response_started_does_not_inject_error_body() {
    let jwks = MockJwks::start().await;
    let upstream = spawn_truncating_upstream().await;
    let (base, _verify, _gateway) = spawn_gateway(&upstream, &jwks).await;

    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);
    // A truncated body surfacing as a client error (Err) is the correct outcome.
    if let Ok(body) = resp.bytes().await {
        let body = String::from_utf8_lossy(&body);
        assert!(
            !body.contains("upstream"),
            "gateway error JSON was spliced into a started response: {body:?}"
        );
    }
}

// TLS terminates at the operator's proxy in front of the gateway; its
// X-Forwarded-Proto must survive rather than being overwritten with `http`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incoming_forwarded_proto_is_preserved() {
    let stack = start_stack().await;
    reqwest::Client::new()
        .get(format!("{}/health", stack.base))
        .header("x-forwarded-proto", "https")
        .send()
        .await
        .unwrap();
    assert_eq!(stack.upstream_saw().unwrap()["x-forwarded-proto"], "https");
}

/// An upstream that accepts connections and never answers.
async fn spawn_silent_upstream() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_that_never_answers_is_504_json() {
    let jwks = MockJwks::start().await;
    let upstream = spawn_silent_upstream().await;
    let (base, _verify, _gateway) = spawn_gateway_with(
        &upstream,
        &jwks,
        &[("CHELABASE_GATEWAY_UPSTREAM_TIMEOUT_SECS", "1")],
    )
    .await;

    let resp = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
        .get(format!("{base}/health"))
        .send()
        .await
        .expect("gateway must answer within its configured upstream timeout");
    assert_eq!(resp.status(), 504);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body,
        serde_json::json!({"error": {"code": "timeout", "message": "upstream timed out"}})
    );
}

// Hostname upstreams (Compose/k8s service names) resolve asynchronously.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hostname_upstream_is_resolved() {
    let jwks = MockJwks::start().await;
    let (upstream, seen) = spawn_upstream().await;
    let by_name = upstream.replace("127.0.0.1", "localhost");
    let (base, _verify, _gateway) = spawn_gateway(&by_name, &jwks).await;

    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(seen.lock().unwrap().is_some());
}

/// Fake orchestrator internal listener: every key is a known public key.
async fn spawn_internal_listener() -> String {
    let app = Router::new().fallback(|req: Request| async move {
        let bearer = req.headers().get("authorization").and_then(|v| v.to_str().ok());
        if req.uri().path().starts_with("/internal/app/sessions/") {
            return if bearer == Some("Bearer e2e-sessions-token") {
                (
                    axum::http::StatusCode::OK,
                    Json(serde_json::json!({"live": true, "project_id": E2E_PROJECT})),
                )
            } else {
                (axum::http::StatusCode::UNAUTHORIZED, Json(serde_json::json!({})))
            };
        }
        let ok = bearer == Some("Bearer e2e-data-keys-token");
        if !ok {
            return (axum::http::StatusCode::UNAUTHORIZED, Json(serde_json::json!({})));
        }
        (
            axum::http::StatusCode::OK,
            Json(serde_json::json!({
                "key_id": "11111111-1111-4111-8111-111111111111",
                "branch_id": "22222222-2222-4222-8222-222222222222",
                "project_id": E2E_PROJECT,
                "kind": "public",
                "role": "anon",
            })),
        )
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_authorization_header_is_not_forwarded_to_rest() {
    let jwks = MockJwks::start().await;
    let (rest, seen) = spawn_upstream().await;
    let internal = spawn_internal_listener().await;
    let (base, verify, _gateway) = spawn_gateway_with(
        &rest,
        &jwks,
        &[
            ("CHELABASE_REST_URL", &rest),
            ("CHELABASE_INTERNAL_URL", &internal),
            ("CHELABASE_DATA_KEYS_TOKEN", "e2e-data-keys-token"),
        ],
    )
    .await;
    let key = chelabase_types::data_key::generate(chelabase_types::data_key::DataKeyKind::Public).key;

    let resp = reqwest::Client::new()
        .post(format!("{base}/data/v1/query"))
        .header("authorization", format!("Bearer {key}"))
        .header("origin", "https://app.example")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["access-control-allow-origin"], "*");
    assert!(resp.headers().get("access-control-allow-credentials").is_none());

    let saw = seen.lock().unwrap().clone().expect("rest was called");
    assert!(!saw.contains_key("authorization"), "the key must not reach rest");
    let claims = verify.verify(&saw[IDENTITY_HEADER], "rest").unwrap();
    assert_eq!(claims.data.unwrap().role, "anon");
}

// The gateway's own flat errors (here a failed proxy) use the shared shape on data routes too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn proxy_failures_are_json_on_data_routes_too() {
    let jwks = MockJwks::start().await;
    let dead_rest = format!("http://127.0.0.1:{}", free_port());
    let internal = spawn_internal_listener().await;
    let (base, _verify, _gateway) = spawn_gateway_with(
        &dead_rest,
        &jwks,
        &[
            ("CHELABASE_REST_URL", &dead_rest),
            ("CHELABASE_INTERNAL_URL", &internal),
            ("CHELABASE_DATA_KEYS_TOKEN", "e2e-data-keys-token"),
        ],
    )
    .await;
    let key = chelabase_types::data_key::generate(chelabase_types::data_key::DataKeyKind::Public).key;

    let resp = reqwest::Client::new()
        .post(format!("{base}/data/v1/query"))
        .header("authorization", format!("Bearer {key}"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body,
        serde_json::json!({"error": {"code": "unavailable", "message": "upstream unavailable"}})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chelabase_user_header_is_stripped_upstream() {
    use chelabase_auth::app_token::{app_audience, AppClaims, AppTokenSigner, APP_ISSUER};
    let jwks = MockJwks::start().await;
    let (rest, seen) = spawn_upstream().await;
    let internal = spawn_internal_listener().await;
    let dir = tempfile::tempdir().unwrap();
    let (private_pem, public_pem) = generate_keypair_pem();
    let public_path = dir.path().join("app-jwt.pub");
    std::fs::write(&public_path, &public_pem).unwrap();
    let signer = AppTokenSigner::from_pem(private_pem.as_bytes(), public_pem.as_bytes()).unwrap();
    let project: uuid::Uuid = E2E_PROJECT.parse().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let token = signer
        .sign(&AppClaims {
            iss: APP_ISSUER.to_string(),
            aud: app_audience(project),
            sub: uuid::Uuid::new_v4(),
            sid: uuid::Uuid::new_v4(),
            project,
            email: "ada@example.com".into(),
            email_verified: true,
            role: "authenticated".into(),
            iat: now,
            exp: now + 300,
        })
        .unwrap();
    let (base, verify, _gateway) = spawn_gateway_with(
        &rest,
        &jwks,
        &[
            ("CHELABASE_REST_URL", &rest),
            ("CHELABASE_INTERNAL_URL", &internal),
            ("CHELABASE_DATA_KEYS_TOKEN", "e2e-data-keys-token"),
            ("CHELABASE_APP_SESSIONS_INTERNAL_TOKEN", "e2e-sessions-token"),
            ("CHELABASE_APP_JWT_PUBLIC_KEY", public_path.to_str().unwrap()),
        ],
    )
    .await;
    let key = chelabase_types::data_key::generate(chelabase_types::data_key::DataKeyKind::Public).key;

    let resp = reqwest::Client::new()
        .post(format!("{base}/data/v1/query"))
        .header("authorization", format!("Bearer {key}"))
        .header("chelabase-user", &token)
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let saw = seen.lock().unwrap().clone().expect("rest was called");
    assert!(!saw.contains_key("chelabase-user"), "the user token must not reach rest");
    assert!(!saw.contains_key("authorization"));
    let data = verify.verify(&saw[IDENTITY_HEADER], "rest").unwrap().data.unwrap();
    assert_eq!(data.role, "authenticated");

    // A bad token is refused by the gateway and never reaches the upstream.
    *seen.lock().unwrap() = None;
    let resp = reqwest::Client::new()
        .post(format!("{base}/data/v1/query"))
        .header("authorization", format!("Bearer {key}"))
        .header("chelabase-user", "garbage")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    assert!(seen.lock().unwrap().is_none());
}
