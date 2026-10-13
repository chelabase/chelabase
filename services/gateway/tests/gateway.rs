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
            .env("CHELABASE_API_HOST", "127.0.0.1")
            .env("CHELABASE_STUDIO_HOST", STUDIO_HOST)
            .env("CHELABASE_AUTH_HOST", jwks.issuer().trim_start_matches("http://"))
            .env("CHELABASE_ORCHESTRATOR_URL", upstream_url)
            .env("CHELABASE_ZITADEL_ISSUER", jwks.issuer())
            .env("CHELABASE_ZITADEL_JWKS_URL", jwks.jwks_url())
            .env("CHELABASE_ZITADEL_AUDIENCES", "")
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
    assert_eq!(seen["x-forwarded-for"], "127.0.0.1");
    assert_eq!(seen["x-forwarded-host"], stack.base.trim_start_matches("http://"));
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
    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
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
// upgrades may stay open up to an hour.
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
