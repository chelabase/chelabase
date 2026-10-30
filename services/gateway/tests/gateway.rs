//! End-to-end: the real gateway binary in front of a mock upstream.
//!
//! Pingora's server loop never returns, so each test spawns the compiled
//! binary on a pre-picked free port and kills it on drop.

// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use axum::extract::Request;
use axum::{Json, Router};
use chelabase_auth::internal::{generate_keypair_pem, InternalVerifier, IDENTITY_HEADER};
use chelabase_auth::test_support::MockJwks;
use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const E2E_PROJECT: &str = "33333333-3333-4333-8333-333333333333";
const STUDIO: &str = "http://127.0.0.1:3999";
const STUDIO_HOST: &str = "127.0.0.1:3999";

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

/// Ports this test process has already handed out. The kernel may give the same ephemeral
/// port to two concurrent `bind(0)` calls once the first listener is closed; two gateways
/// would then race for it and a test could talk to another test's gateway (which its owner
/// then kills mid-request).
static LEASED_PORTS: Mutex<Vec<u16>> = Mutex::new(Vec::new());

fn free_port() -> u16 {
    let mut leased = LEASED_PORTS.lock().unwrap();
    loop {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        if !leased.contains(&port) {
            leased.push(port);
            return port;
        }
    }
}

/// An address nothing listens on, so connecting is refused at once. The socket is bound but
/// never listens, and the caller keeps it for the test's duration: the port stays reserved
/// (no other test's `bind(0)` can get it, which would answer the request) and connecting
/// to it is refused. (A fixed low port like 1 is not an option: on WSL a connect to it
/// times out instead of being refused.)
fn refused_upstream() -> (String, tokio::net::TcpSocket) {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let port = socket.local_addr().unwrap().port();
    LEASED_PORTS.lock().unwrap().push(port);
    (format!("http://127.0.0.1:{port}"), socket)
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
    let (private_pem, public_pem) = generate_keypair_pem();

    // The port comes from a bind(0) that is closed again before the gateway binds it, so
    // another test's `bind(0)` can be handed the same port in between (then the gateway
    // exits with "address in use", and the other listener would answer our health check).
    // A gateway that did not get its port is therefore retried on a new one.
    for _ in 0..10 {
        // A temp working dir keeps dotenvy from picking up a repo-level .env.
        let workdir = tempfile::tempdir().unwrap();
        let signing_key = workdir.path().join("internal-signing.pem");
        std::fs::write(&signing_key, &private_pem).unwrap();

        let port = free_port();
        let mut command = Command::new(env!("CARGO_BIN_EXE_gateway"));
        let child = command
            .current_dir(workdir.path())
            .env("CHELABASE_GATEWAY_LISTEN", format!("127.0.0.1:{port}"))
            .env("CHELABASE_PUBLIC_SCHEME", "http")
            .env("CHELABASE_API_HOST", std::net::Ipv4Addr::LOCALHOST.to_string())
            .env("CHELABASE_STUDIO_HOST", STUDIO_HOST)
            .env("CHELABASE_AUTH_HOST", jwks.issuer().trim_start_matches("http://"))
            .env("CHELABASE_ORCHESTRATOR_URL", upstream_url)
            .env("CHELABASE_ZITADEL_ISSUER", jwks.issuer())
            .env("CHELABASE_ZITADEL_JWKS_URL", jwks.jwks_url())
            .env("CHELABASE_ZITADEL_AUDIENCES", chelabase_auth::test_support::MOCK_AUDIENCE)
            // The development bearers (this test stands in for the workbench).
            .env("CHELABASE_DEV_DEFAULTS", "1")
            .env("CHELABASE_INTERNAL_SIGNING_KEY", &signing_key)
            .env("RUST_LOG", "warn")
            .envs(extra_env.iter().copied())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn gateway binary");

        // Guarded from the spawn on: every way out of here (a panic included) kills it.
        let mut gateway = GatewayProcess {
            child,
            _workdir: workdir,
        };
        let base = format!("http://127.0.0.1:{port}");
        if wait_ready(&base, port, &mut gateway).await {
            return (
                base,
                InternalVerifier::from_pem(public_pem.as_bytes()).unwrap(),
                gateway,
            );
        }
        // Exited during startup (the guard reaps it); try again on another port.
    }
    panic!("the gateway never got a port to listen on (see stderr above)");
}

/// Whether process `pid` holds the TCP socket listening on 127.0.0.1:`port` (Linux: the
/// socket's inode from /proc/net/tcp, looked up among the process's open fds). This proves
/// that whatever answers on the port is our child, not another test binary's gateway or
/// mock that got the port first.
#[cfg(target_os = "linux")]
fn process_owns_listener(pid: u32, port: u16) -> bool {
    let Ok(table) = std::fs::read_to_string("/proc/net/tcp") else {
        return false;
    };
    let local = format!("0100007F:{port:04X}");
    let inodes: Vec<&str> = table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            // f[1] local address, f[3] state (0A = LISTEN), f[9] inode
            (f.len() > 9 && f[1] == local && f[3] == "0A").then_some(f[9])
        })
        .collect();
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    fds.flatten()
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .any(|link| {
            let link = link.to_string_lossy();
            inodes.iter().any(|inode| link == format!("socket:[{inode}]"))
        })
}

/// Other targets have no /proc: readiness falls back to the health check alone, so the
/// cross-process guarantee (that the answer comes from our child, not another test binary's
/// listener on the same port) does not hold there; within one process the port lease and
/// the child-exit retry still cover it.
#[cfg(not(target_os = "linux"))]
fn process_owns_listener(_pid: u32, _port: u16) -> bool {
    true
}

async fn health_is_ok(client: &reqwest::Client, base: &str) -> bool {
    match client.get(format!("{base}/_gateway/health")).send().await {
        Ok(resp) => resp.status() == 200,
        Err(_) => false,
    }
}

/// Waits until our gateway child listens on `port` and answers its health check: `true`
/// then, `false` when the process exited first (it lost the race for its port). A 200 alone
/// proves nothing (a listener that took over the port answers 200 too), so the listening
/// socket must belong to the child. A debug binary can take long to start on a loaded
/// machine, so the budget is generous; failing paths panic and the guard kills the child.
async fn wait_ready(base: &str, port: u16, gateway: &mut GatewayProcess) -> bool {
    let client = reqwest::Client::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while std::time::Instant::now() < deadline {
        let exited = gateway.child.try_wait().expect("poll the gateway child").is_some();
        if exited {
            return false;
        }
        if process_owns_listener(gateway.child.id(), port) && health_is_ok(&client, base).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("gateway at {base} did not become ready within 60s (see stderr above)");
}

#[cfg(target_os = "linux")]
#[test]
fn listener_owner_check_matches_only_the_owning_process() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(process_owns_listener(std::process::id(), port));
    // pid 1 (init) certainly doesn't hold this socket.
    assert!(!process_owns_listener(1, port));
    drop(listener);
    // Closed: nobody listens on it any more.
    assert!(!process_owns_listener(std::process::id(), port));
}

#[cfg(target_os = "linux")]
#[test]
fn dropping_the_guard_kills_a_child_that_never_became_ready() {
    // A child that lives on (it never listens); the guard must kill it on drop even when
    // the caller bails out early (a panic in `wait_ready`'s failure paths).
    let child = Command::new("tail").args(["-f", "/dev/null"]).spawn().unwrap();
    let pid = child.id();
    let guard = GatewayProcess {
        child,
        _workdir: tempfile::tempdir().unwrap(),
    };
    assert!(std::path::Path::new(&format!("/proc/{pid}")).exists());
    drop(guard);
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists(), "child outlived its guard");
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
    assert_eq!(seen["x-forwarded-for"], std::net::Ipv4Addr::LOCALHOST.to_string());
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

// RT1 S15: the orchestrator's `/internal/realtime/*` routes sit on its public listener, which
// the gateway proxies to; the gateway never routes them (no endpoint declares them), with or
// without a bearer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn realtime_internal_routes_are_404_and_never_proxied() {
    let stack = start_stack().await;
    for route in ["authorize", "projects", "visible"] {
        for bearer in [None, Some("realtime-internal-token")] {
            let mut req = reqwest::Client::new()
                .post(format!("{}/internal/realtime/{route}", stack.base))
                .json(&serde_json::json!({ "token": "x" }));
            if let Some(bearer) = bearer {
                req = req.bearer_auth(bearer);
            }
            let resp = req.send().await.unwrap();
            assert_eq!(resp.status(), 404, "{route} {bearer:?}");
        }
    }
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
    let (dead_upstream, _reserved) = refused_upstream();
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

// P4: forwarded headers are the gateway's own, never the client's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwarded_headers_are_replaced_not_trusted() {
    let stack = start_stack().await;
    let resp = reqwest::Client::new()
        .get(format!("{}/health", stack.base))
        .header("x-forwarded-proto", "https")
        .header("x-forwarded-for", "203.0.113.9")
        .header("x-forwarded-host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let seen = stack.upstream_saw().unwrap();
    assert_eq!(seen["x-forwarded-proto"], "http");
    assert_eq!(seen["x-forwarded-for"], std::net::Ipv4Addr::LOCALHOST.to_string());
    assert_eq!(seen["x-forwarded-host"], stack.base.trim_start_matches("http://"));
}

// m14 (#25): from a trusted proxy (here loopback), the apps see the right-most untrusted
// `X-Forwarded-For` address, one address only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_trusted_proxy_forwards_the_client_address() {
    let jwks = MockJwks::start().await;
    let (upstream, seen) = spawn_upstream().await;
    let (base, _verify, _gateway) =
        spawn_gateway_with(&upstream, &jwks, &[("CHELABASE_GATEWAY_TRUSTED_PROXIES", "127.0.0.0/8, ::1")]).await;
    let client = reqwest::Client::new();
    let resp = client
        .get(format!("{base}/health"))
        .header("x-forwarded-for", "6.6.6.6, 203.0.113.9, 127.0.0.5")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(seen.lock().unwrap().clone().unwrap()["x-forwarded-for"], "203.0.113.9");
    // A malformed header falls back to the peer and still answers.
    let resp = client
        .get(format!("{base}/health"))
        .header("x-forwarded-for", "203.0.113.9, garbage")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(seen.lock().unwrap().clone().unwrap()["x-forwarded-for"], "127.0.0.1");
}

// m14 (#25) N4: with trusted proxies configured, a peer that isn't one of them gets nothing
// from a forged header (one naming a trusted hop too): the apps see the peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forged_header_from_an_untrusted_peer_is_ignored() {
    let jwks = MockJwks::start().await;
    let (upstream, seen) = spawn_upstream().await;
    let (base, _verify, _gateway) =
        spawn_gateway_with(&upstream, &jwks, &[("CHELABASE_GATEWAY_TRUSTED_PROXIES", "10.0.0.0/8")]).await;
    let resp = reqwest::Client::new()
        .get(format!("{base}/health"))
        .header("x-forwarded-for", "1.2.3.4, 10.0.0.2")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(seen.lock().unwrap().clone().unwrap()["x-forwarded-for"], "127.0.0.1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_host_is_404_with_an_empty_body() {
    let stack = start_stack().await;
    let resp = reqwest::Client::new()
        .get(format!("{}/health", stack.base))
        .header(reqwest::header::HOST, "nope.example")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert_eq!(resp.text().await.unwrap(), "");
    assert!(stack.upstream_saw().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_auth_host_reaches_zitadel_with_host_and_bearer_unchanged() {
    let jwks = MockJwks::start().await;
    let (api, _) = spawn_upstream().await;
    let (zitadel, seen) = spawn_upstream().await;
    let (base, _verify, _gateway) =
        spawn_gateway_with(&api, &jwks, &[("CHELABASE_ZITADEL_UPSTREAM", zitadel.as_str())]).await;
    let auth_host = jwks.issuer().trim_start_matches("http://").to_string();
    let resp = reqwest::Client::new()
        .get(format!("{base}/management/v1/orgs/me"))
        .header(reqwest::header::HOST, &auth_host)
        .header("authorization", "Bearer zitadel-pat")
        .header("origin", STUDIO)
        .header("chelabase-server-key", "chk_sec_never-forwarded")
        .header("chelabase-client-ip", "203.0.113.9")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("access-control-allow-origin").is_none());
    let seen = seen.lock().unwrap().clone().expect("the Zitadel upstream was reached");
    assert_eq!(seen["host"], auth_host);
    assert_eq!(seen["authorization"], "Bearer zitadel-pat");
    assert_eq!(seen[":uri"], "/management/v1/orgs/me");
    assert!(!seen.contains_key(IDENTITY_HEADER));
    // #49: the app-server headers are the gateway's alone, on a site too.
    assert!(!seen.contains_key("chelabase-server-key") && !seen.contains_key("chelabase-client-ip"));
    assert_eq!(seen["x-forwarded-for"], "127.0.0.1");
    assert_eq!(seen["x-forwarded-proto"], "http");
    assert_eq!(seen["x-forwarded-host"], auth_host);
}

async fn read_head(sock: &mut tokio::net::TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        sock.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    String::from_utf8(head).unwrap()
}

/// A raw upstream that answers one websocket upgrade with 101, then echoes 4 bytes.
async fn spawn_upgrade_upstream() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let head = read_head(&mut sock).await.to_ascii_lowercase();
        assert!(head.contains("upgrade: websocket"), "{head}");
        sock.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n")
            .await
            .unwrap();
        let mut buf = [0u8; 4];
        sock.read_exact(&mut buf).await.unwrap();
        sock.write_all(&buf).await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_studio_host_passes_websocket_upgrades() {
    let jwks = MockJwks::start().await;
    let (api, _) = spawn_upstream().await;
    let studio = spawn_upgrade_upstream().await;
    let (base, _verify, _gateway) =
        spawn_gateway_with(&api, &jwks, &[("CHELABASE_STUDIO_UPSTREAM", studio.as_str())]).await;
    let port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();
    let mut sock = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await.unwrap();
    let request = format!(
        "GET /@vite/client HTTP/1.1\r\nHost: {STUDIO_HOST}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    sock.write_all(request.as_bytes()).await.unwrap();
    let head = tokio::time::timeout(Duration::from_secs(10), read_head(&mut sock)).await.unwrap();
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    sock.write_all(b"ping").await.unwrap();
    let mut echo = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(10), sock.read_exact(&mut echo)).await.unwrap().unwrap();
    assert_eq!(&echo, b"ping");
}

/// A raw upstream whose 101 is already in the gateway's receive buffer when the gateway
/// starts proxying: it answers every connection with 101 before reading the request (from
/// the gateway's side, the same as a fast upstream answering while the gateway thread is
/// preempted between writing the request head and its first read). Then it reads the
/// request head and echoes 4 bytes.
async fn spawn_eager_upgrade_upstream() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                sock.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n")
                    .await
                    .unwrap();
                let head = read_head(&mut sock).await.to_ascii_lowercase();
                assert!(head.contains("upgrade: websocket"), "{head}");
                let mut buf = [0u8; 4];
                sock.read_exact(&mut buf).await.unwrap();
                sock.write_all(&buf).await.unwrap();
                tokio::time::sleep(Duration::from_secs(5)).await;
            });
        }
    });
    format!("http://{addr}")
}

/// The gateway must not end an upgraded connection on the client request's own end of body
/// (an empty GET's), whichever of that and the upstream's 101 it handles first. Each round
/// trip used to lose that race about half the time; 32 sequential ones prove the fix.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_websocket_upgrade_survives_a_101_that_beats_the_request_end() {
    let jwks = MockJwks::start().await;
    let (api, _) = spawn_upstream().await;
    let studio = spawn_eager_upgrade_upstream().await;
    let (base, _verify, _gateway) =
        spawn_gateway_with(&api, &jwks, &[("CHELABASE_STUDIO_UPSTREAM", studio.as_str())]).await;
    let port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();
    for round in 0..32 {
        let mut sock = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await.unwrap();
        let request = format!(
            "GET /@vite/client HTTP/1.1\r\nHost: {STUDIO_HOST}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        sock.write_all(request.as_bytes()).await.unwrap();
        let head = tokio::time::timeout(Duration::from_secs(10), read_head(&mut sock)).await.unwrap();
        assert!(head.starts_with("HTTP/1.1 101"), "round {round}: {head}");
        sock.write_all(b"ping").await.unwrap();
        let mut echo = [0u8; 4];
        let read = tokio::time::timeout(Duration::from_secs(10), sock.read_exact(&mut echo)).await.unwrap();
        assert!(read.is_ok(), "round {round}: the upgraded connection was closed: {read:?}");
        assert_eq!(&echo, b"ping", "round {round}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_site_upstream_down_is_502_plain_text() {
    let jwks = MockJwks::start().await;
    let (api, _) = spawn_upstream().await;
    let (down, _socket) = refused_upstream();
    let (base, _verify, _gateway) =
        spawn_gateway_with(&api, &jwks, &[("CHELABASE_STUDIO_UPSTREAM", down.as_str())]).await;
    let resp = reqwest::Client::new()
        .get(format!("{base}/"))
        .header(reqwest::header::HOST, STUDIO_HOST)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
    let content_type = resp.headers()["content-type"].to_str().unwrap().to_string();
    assert!(content_type.starts_with("text/plain"), "{content_type}");
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

/// One HTTP/1.1 chunk.
fn chunk(data: &str) -> Vec<u8> {
    format!("{:x}\r\n{data}\r\n", data.len()).into_bytes()
}

/// A raw upstream that answers one event-stream request: its head and a first event, then 3 s of
/// silence, then a second event and the end of the stream.
async fn spawn_event_stream_upstream() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        read_head(&mut sock).await;
        sock.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\
              Transfer-Encoding: chunked\r\n\r\n",
        )
        .await
        .unwrap();
        sock.write_all(&chunk("data: one\n\n")).await.unwrap();
        tokio::time::sleep(Duration::from_secs(3)).await;
        sock.write_all(&chunk("data: two\n\n")).await.unwrap();
        sock.write_all(b"0\r\n\r\n").await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    format!("http://{addr}")
}

// workbench-stack (user, 2026-10-14): a site's event stream (the request asks for
// `text/event-stream`) may sit idle like a websocket, past the upstream read timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_site_event_stream_may_idle_past_the_upstream_timeout() {
    let jwks = MockJwks::start().await;
    let (api, _) = spawn_upstream().await;
    let stream = spawn_event_stream_upstream().await;
    let (base, _verify, _gateway) = spawn_gateway_with(
        &api,
        &jwks,
        &[("CHELABASE_STUDIO_UPSTREAM", stream.as_str()), ("CHELABASE_GATEWAY_UPSTREAM_TIMEOUT_SECS", "1")],
    )
    .await;
    let resp = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap()
        .get(format!("{base}/__tsd/console-pipe/sse"))
        .header(reqwest::header::HOST, STUDIO_HOST)
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.expect("the stream isn't cut by the 1 s upstream timeout");
    assert!(body.contains("data: one") && body.contains("data: two"), "{body}");
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
    let by_name = upstream.replace(&std::net::Ipv4Addr::LOCALHOST.to_string(), "localhost");
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
    let (dead_rest, _reserved) = refused_upstream();
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

// Spec §2: a site request that gets no answer times out like the API's; only websocket
// upgrades and event streams (`Accept: text/event-stream`) may stay open up to an hour.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hung_site_request_times_out_like_the_api() {
    let jwks = MockJwks::start().await;
    let (api, _) = spawn_upstream().await;
    let silent = spawn_silent_upstream().await;
    let (base, _verify, _gateway) = spawn_gateway_with(
        &api,
        &jwks,
        &[("CHELABASE_STUDIO_UPSTREAM", silent.as_str()), ("CHELABASE_GATEWAY_UPSTREAM_TIMEOUT_SECS", "1")],
    )
    .await;
    let resp = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
        .get(format!("{base}/"))
        .header(reqwest::header::HOST, STUDIO_HOST)
        .send()
        .await
        .expect("the gateway answers before the client gives up");
    assert_eq!(resp.status(), 504);
}

// ---- RT1 T3 (S18, W17): the realtime socket route and upgrades elsewhere ----

fn upgrade_head(target: &str, host: &str, extra: &str) -> String {
    format!(
        "GET {target} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n{extra}\r\n"
    )
}

fn gateway_port(base: &str) -> u16 {
    base.rsplit(':').next().unwrap().parse().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_upgrade_on_a_non_websocket_api_route_is_400_and_never_reaches_the_upstream() {
    let stack = start_stack().await;
    let port = gateway_port(&stack.base);
    let host = format!("127.0.0.1:{port}");
    for target in ["/v1/me", "/health"] {
        let mut sock = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await.unwrap();
        let extra = format!("Authorization: {}\r\n", stack.bearer());
        sock.write_all(upgrade_head(target, &host, &extra).as_bytes()).await.unwrap();
        let head = tokio::time::timeout(Duration::from_secs(10), read_head(&mut sock)).await.unwrap();
        assert!(head.starts_with("HTTP/1.1 400"), "{target}: {head}");
        assert!(stack.upstream_saw().is_none(), "{target}: the upgrade reached the upstream");
    }
    // The same route without an upgrade is proxied as before.
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/me", stack.base))
        .header("authorization", stack.bearer())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(stack.upstream_saw().is_some());
}

/// A raw upstream that records the request head it gets, answers it with 101 and echoes 4 bytes.
async fn spawn_recording_upgrade_upstream() -> (String, Arc<Mutex<Option<String>>>) {
    let head: Arc<Mutex<Option<String>>> = Arc::default();
    let record = head.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        *record.lock().unwrap() = Some(read_head(&mut sock).await);
        sock.write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n")
            .await
            .unwrap();
        let mut buf = [0u8; 4];
        sock.read_exact(&mut buf).await.unwrap();
        sock.write_all(&buf).await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });
    (format!("http://{addr}"), head)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_realtime_socket_route_passes_an_upgrade_with_its_ticket_and_no_client_identity() {
    let jwks = MockJwks::start().await;
    let (api, api_seen) = spawn_upstream().await;
    let (realtime, realtime_head) = spawn_recording_upgrade_upstream().await;
    let (base, _verify, _gateway) =
        spawn_gateway_with(&api, &jwks, &[("CHELABASE_REALTIME_URL", realtime.as_str())]).await;
    let port = gateway_port(&base);
    let mut sock = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await.unwrap();
    let extra = format!(
        "{IDENTITY_HEADER}: forged\r\nAuthorization: Bearer nope\r\nSec-WebSocket-Protocol: chelabase.realtime.v1\r\n"
    );
    sock.write_all(upgrade_head("/v1/realtime/socket?ticket=T1", &format!("127.0.0.1:{port}"), &extra).as_bytes())
        .await
        .unwrap();
    let head = tokio::time::timeout(Duration::from_secs(10), read_head(&mut sock)).await.unwrap();
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    sock.write_all(b"ping").await.unwrap();
    let mut echo = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(10), sock.read_exact(&mut echo)).await.unwrap().unwrap();
    assert_eq!(&echo, b"ping");

    let seen = realtime_head.lock().unwrap().clone().expect("realtime was called").to_ascii_lowercase();
    assert!(seen.starts_with("get /v1/realtime/socket?ticket=t1 http/1.1"), "{seen}");
    assert!(!seen.contains(IDENTITY_HEADER), "a forged identity reached realtime: {seen}");
    assert!(!seen.contains("authorization"), "{seen}");
    assert!(seen.contains("sec-websocket-protocol: chelabase.realtime.v1"), "{seen}");
    assert!(api_seen.lock().unwrap().is_none(), "the socket route must not go to the orchestrator");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dead_realtime_upstream_answers_a_socket_upgrade_with_502() {
    // The W16 failure path (the log text itself is asserted in realtime_proxy.rs).
    let jwks = MockJwks::start().await;
    let (api, _) = spawn_upstream().await;
    let (dead, _socket) = refused_upstream();
    let (base, _verify, _gateway) = spawn_gateway_with(&api, &jwks, &[("CHELABASE_REALTIME_URL", dead.as_str())]).await;
    let port = gateway_port(&base);
    let mut sock = tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port)).await.unwrap();
    sock.write_all(upgrade_head("/v1/realtime/socket?ticket=SECRET", &format!("127.0.0.1:{port}"), "").as_bytes())
        .await
        .unwrap();
    let head = tokio::time::timeout(Duration::from_secs(10), read_head(&mut sock)).await.unwrap();
    assert!(head.starts_with("HTTP/1.1 502"), "{head}");
}

/// m16: an upstream serving types as rest and the orchestrator do: a strong `ETag`, and a 304
/// for an `If-None-Match` naming it. It records the `If-None-Match` it got.
async fn spawn_types_upstream() -> (String, Arc<Mutex<Vec<Option<String>>>>) {
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let record = seen.clone();
    let app = Router::new().fallback(move |req: Request| {
        let record = record.clone();
        async move {
            use axum::response::IntoResponse;
            let inm = req.headers().get("if-none-match").map(|v| v.to_str().unwrap().to_string());
            record.lock().unwrap().push(inm.clone());
            let cache = [("etag", "\"t1\""), ("cache-control", "no-store")];
            if inm.as_deref() == Some("\"t1\"") {
                return (axum::http::StatusCode::NOT_MODIFIED, cache).into_response();
            }
            (cache, [("content-type", "text/typescript; charset=utf-8")], "export type Database = {};\n").into_response()
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

/// The data key lookup, answering every key as a secret key (the types route takes no other).
async fn spawn_secret_key_listener() -> String {
    let app = Router::new().fallback(|req: Request| async move {
        let bearer = req.headers().get("authorization").and_then(|v| v.to_str().ok());
        if bearer != Some("Bearer e2e-data-keys-token") {
            return (axum::http::StatusCode::UNAUTHORIZED, Json(serde_json::json!({})));
        }
        (
            axum::http::StatusCode::OK,
            Json(serde_json::json!({
                "key_id": "11111111-1111-4111-8111-111111111111",
                "branch_id": "22222222-2222-4222-8222-222222222222",
                "project_id": E2E_PROJECT,
                "kind": "secret",
                "role": "authenticated",
            })),
        )
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// m16: `ETag`, `If-None-Match` and the 304 pass through the gateway unchanged, on the data
/// API's `GET /data/v1/types` and on the management API's types download.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn types_etag_if_none_match_and_304_pass_through() {
    let jwks = MockJwks::start().await;
    let (upstream, seen) = spawn_types_upstream().await;
    let internal = spawn_secret_key_listener().await;
    let (base, _verify, _gateway) = spawn_gateway_with(
        &upstream,
        &jwks,
        &[
            ("CHELABASE_REST_URL", &upstream),
            ("CHELABASE_INTERNAL_URL", &internal),
            ("CHELABASE_DATA_KEYS_TOKEN", "e2e-data-keys-token"),
        ],
    )
    .await;
    let key = chelabase_types::data_key::generate(chelabase_types::data_key::DataKeyKind::Secret).key;
    let management = format!("Bearer {}", jwks.token_for_org("user-1", "org-1", 300));
    let routes = [
        (format!("{base}/data/v1/types"), format!("Bearer {key}")),
        (
            format!("{base}/v1/databases/6c0f1c2e-2f7e-4c52-9a51-0d8f2f1d7c11/branches/0d8f2f1d-7c11-4c52-9a51-6c0f1c2e2f7e/data-api/types"),
            management,
        ),
    ];
    for (url, auth) in routes {
        let client = reqwest::Client::new();
        let resp = client.get(&url).header("authorization", &auth).send().await.unwrap();
        assert_eq!(resp.status(), 200, "{url}");
        assert_eq!(resp.headers()["etag"], "\"t1\"", "{url}");
        assert_eq!(resp.headers()["cache-control"], "no-store", "{url}");
        assert_eq!(resp.headers()["content-type"], "text/typescript; charset=utf-8", "{url}");
        assert_eq!(resp.text().await.unwrap(), "export type Database = {};\n", "{url}");
        assert_eq!(seen.lock().unwrap().last().unwrap(), &None, "{url}");

        let resp = client
            .get(&url)
            .header("authorization", &auth)
            .header("if-none-match", "\"t1\"")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 304, "{url}");
        assert_eq!(resp.headers()["etag"], "\"t1\"", "{url}");
        assert_eq!(resp.headers()["cache-control"], "no-store", "{url}");
        assert_eq!(resp.bytes().await.unwrap().len(), 0, "{url}");
        assert_eq!(seen.lock().unwrap().last().unwrap().as_deref(), Some("\"t1\""), "{url}");
    }
}

/// The data key lookup answering from `keys` (key -> its answer); any other key is unknown.
async fn spawn_key_listener(keys: Vec<(String, serde_json::Value)>) -> String {
    let by_hash: HashMap<String, serde_json::Value> = keys
        .into_iter()
        .map(|(key, answer)| (hex::encode(chelabase_types::data_key::hash(&key)), answer))
        .collect();
    let by_hash = Arc::new(by_hash);
    let app = Router::new().fallback(move |req: Request| {
        let by_hash = by_hash.clone();
        async move {
            let bearer = req.headers().get("authorization").and_then(|v| v.to_str().ok());
            if bearer != Some("Bearer e2e-data-keys-token") {
                return (axum::http::StatusCode::UNAUTHORIZED, Json(serde_json::json!({})));
            }
            let hash = req.uri().path().rsplit('/').next().unwrap_or_default();
            match by_hash.get(hash) {
                Some(answer) => (axum::http::StatusCode::OK, Json(answer.clone())),
                None => (axum::http::StatusCode::NOT_FOUND, Json(serde_json::json!({}))),
            }
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// #49: an app's server signs its user in with its public key, vouches for the user's address
/// with the branch's secret key, and auth sees that address; a key that doesn't vouch leaves
/// the peer's. Neither header reaches auth.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_app_server_forwards_its_users_address_with_the_secret_key() {
    use chelabase_types::data_key::{generate, DataKeyKind};
    let jwks = MockJwks::start().await;
    let (auth, seen) = spawn_upstream().await;
    let public = generate(DataKeyKind::Public).key;
    let secret = generate(DataKeyKind::Secret).key;
    let other_project = generate(DataKeyKind::Secret).key;
    let branch = "22222222-2222-4222-8222-222222222222";
    let answer = |key_id: &str, project: &str, kind: &str| {
        serde_json::json!({
            "key_id": key_id, "branch_id": branch, "project_id": project, "kind": kind, "role": "anon",
        })
    };
    let internal = spawn_key_listener(vec![
        (public.clone(), answer("11111111-1111-4111-8111-111111111111", E2E_PROJECT, "public")),
        (secret.clone(), answer("44444444-4444-4444-8444-444444444444", E2E_PROJECT, "secret")),
        (
            other_project.clone(),
            answer("55555555-5555-4555-8555-555555555555", "66666666-6666-4666-8666-666666666666", "secret"),
        ),
    ])
    .await;
    let (base, verify, _gateway) = spawn_gateway_with(
        &auth,
        &jwks,
        &[
            ("CHELABASE_AUTH_URL", &auth),
            ("CHELABASE_INTERNAL_URL", &internal),
            ("CHELABASE_DATA_KEYS_TOKEN", "e2e-data-keys-token"),
        ],
    )
    .await;
    let sign_in = |server_key: &str, client_ip: &str| {
        reqwest::Client::new()
            .post(format!("{base}/auth/v1/app/sign-in"))
            .header("authorization", format!("Bearer {public}"))
            .header("chelabase-server-key", server_key)
            .header("chelabase-client-ip", client_ip)
            .body("{}")
            .send()
    };

    // `None`: the header is ignored and auth sees the loopback peer.
    for (server_key, client_ip, vouched) in [
        (secret.as_str(), "203.0.113.9", Some("203.0.113.9")),
        (secret.as_str(), "2001:db8::1", Some("2001:db8::1")),
        (other_project.as_str(), "203.0.113.9", None),
        (public.as_str(), "203.0.113.9", None),
        (secret.as_str(), "203.0.113.9:443", None),
    ] {
        *seen.lock().unwrap() = None;
        let resp = sign_in(server_key, client_ip).await.unwrap();
        assert_eq!(resp.status(), 200, "{client_ip}");
        let saw = seen.lock().unwrap().clone().expect("auth was called");
        match vouched {
            Some(ip) => assert_eq!(saw["x-forwarded-for"], ip, "{client_ip}"),
            None => assert_eq!(saw["x-forwarded-for"], "127.0.0.1", "{client_ip}"),
        }
        assert!(!saw.contains_key("chelabase-server-key"), "the secret key must not reach auth");
        assert!(!saw.contains_key("chelabase-client-ip"));
        assert!(!saw.contains_key("authorization"));
        let data = verify.verify(&saw[IDENTITY_HEADER], "auth").unwrap().data.unwrap();
        assert_eq!(data.key_id.to_string(), "11111111-1111-4111-8111-111111111111", "the public key's identity");
    }

    // The two headers never reach any upstream, even on a public route.
    *seen.lock().unwrap() = None;
    let resp = reqwest::Client::new()
        .get(format!("{base}/health"))
        .header("chelabase-server-key", &secret)
        .header("chelabase-client-ip", "203.0.113.9")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let saw = seen.lock().unwrap().clone().expect("the upstream was called");
    assert!(!saw.contains_key("chelabase-server-key") && !saw.contains_key("chelabase-client-ip"));
    assert_eq!(saw["x-forwarded-for"], "127.0.0.1");
}
