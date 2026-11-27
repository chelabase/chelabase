//! End-to-end: the real gateway binary in front of a mock upstream.
//!
//! Pingora's server loop never returns, so each test spawns the compiled
//! binary on a pre-picked free port and kills it on drop.

// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use axum::extract::Request;
use axum::{Json, Router};
use chelabase_auth::internal::{generate_keypair_pem, InternalVerifier, IDENTITY_HEADER};
use chelabase_auth::test_support::MemberKeys;
use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const E2E_PROJECT: &str = "33333333-3333-4333-8333-333333333333";
const STUDIO: &str = "http://127.0.0.1:3999";
/// The tenant every management token here names.
const TENANT: uuid::Uuid = uuid::Uuid::from_u128(0x7e7e);
const STUDIO_HOST: &str = "127.0.0.1:3999";

/// The most recent request the mock upstream received: headers plus `:uri`.
type Seen = Arc<Mutex<Option<HashMap<String, String>>>>;

struct Stack {
    base: String,
    keys: MemberKeys,
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

/// Spawn the gateway binary in front of `upstream_url`, trusting `keys`' member tokens.
async fn spawn_gateway(upstream_url: &str, keys: &MemberKeys) -> (String, InternalVerifier, GatewayProcess) {
    spawn_gateway_with(upstream_url, keys, &[]).await
}

async fn spawn_gateway_with(
    upstream_url: &str,
    keys: &MemberKeys,
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
        // Only its owner may read a private key (#308).
        std::fs::set_permissions(&signing_key, std::os::unix::fs::PermissionsExt::from_mode(0o600))
            .unwrap();
        let member_key = workdir.path().join("member-jwt.pub");
        std::fs::write(&member_key, &keys.public_pem).unwrap();

        let port = free_port();
        let mut command = Command::new(env!("CARGO_BIN_EXE_gateway"));
        let child = command
            .current_dir(workdir.path())
            .env("CHELABASE_GATEWAY_LISTEN", format!("127.0.0.1:{port}"))
            .env("CHELABASE_PUBLIC_SCHEME", "http")
            .env("CHELABASE_API_HOST", std::net::Ipv4Addr::LOCALHOST.to_string())
            .env("CHELABASE_STUDIO_HOST", STUDIO_HOST)
            .env("CHELABASE_AUTH_HOST", keys.issuer().trim_start_matches("http://"))
            .env("CHELABASE_ORCHESTRATOR_URL", upstream_url)
            .env("CHELABASE_MEMBER_JWT_PUBLIC_KEY", &member_key)
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
    let keys = MemberKeys::new();
    let (upstream, seen) = spawn_upstream().await;
    let (base, verify, gateway) = spawn_gateway(&upstream, &keys).await;
    Stack {
        base,
        keys,
        verify,
        seen,
        _gateway: gateway,
    }
}

impl Stack {
    fn bearer(&self) -> String {
        format!("Bearer {}", self.keys.token("user-1", TENANT, 300))
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
    assert_eq!(claims.tenant_id, Some(TENANT));
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

/// RFC 7231 IMF-fixdate: `Sun, 06 Nov 1994 08:49:37 GMT`, with a two-digit day.
fn is_imf_fixdate(value: &str) -> bool {
    let bytes = value.as_bytes();
    let days = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let digits = |range: std::ops::Range<usize>| {
        bytes.get(range).is_some_and(|b| b.iter().all(u8::is_ascii_digit))
    };
    value.len() == 29
        && days.iter().any(|d| value.starts_with(&format!("{d}, ")))
        && digits(5..7)
        && bytes[7] == b' '
        && months.iter().any(|m| bytes.get(8..11) == Some(m.as_bytes()))
        && bytes[11] == b' '
        && digits(12..16)
        && bytes[16] == b' '
        && digits(17..19)
        && bytes[19] == b':'
        && digits(20..22)
        && bytes[22] == b':'
        && digits(23..25)
        && value.ends_with(" GMT")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_carry_an_imf_fixdate_date_header() {
    // The validator must reject the non-padded day that chrono-free formatting could emit.
    assert!(!is_imf_fixdate("Sun, 6 Nov 1994 08:49:37 GMT"));
    assert!(is_imf_fixdate("Sun, 06 Nov 1994 08:49:37 GMT"));
    let stack = start_stack().await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/databases", stack.base))
        .header("authorization", stack.bearer())
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    let date = resp.headers()["date"].to_str().unwrap().to_string();
    assert!(is_imf_fixdate(&date), "{date:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_down_is_502_json() {
    let jwks = MemberKeys::new();
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
    let jwks = MemberKeys::new();
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
    let jwks = MemberKeys::new();
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
    let jwks = MemberKeys::new();
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

// #126: the auth host names the member tokens' issuer only; the gateway routes it nowhere, so
// nothing there falls through to another upstream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_auth_host_is_routed_nowhere() {
    let jwks = MemberKeys::new();
    let (api, seen) = spawn_upstream().await;
    let (base, _verify, _gateway) = spawn_gateway(&api, &jwks).await;
    let auth_host = jwks.issuer().trim_start_matches("http://").to_string();
    for path in ["/", "/auth/v1/sign-in", "/oauth/v2/keys", "/v1/databases"] {
        let resp = reqwest::Client::new()
            .get(format!("{base}{path}"))
            .header(reqwest::header::HOST, &auth_host)
            .header("authorization", format!("Bearer {}", jwks.token("user-1", TENANT, 300)))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "{path}");
    }
    assert!(seen.lock().unwrap().is_none(), "nothing was proxied");
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
    let jwks = MemberKeys::new();
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
    let jwks = MemberKeys::new();
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
    let jwks = MemberKeys::new();
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
    let jwks = MemberKeys::new();
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
    let jwks = MemberKeys::new();
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
    let jwks = MemberKeys::new();
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
    let jwks = MemberKeys::new();
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
    let jwks = MemberKeys::new();
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
    let jwks = MemberKeys::new();
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
    let jwks = MemberKeys::new();
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
    let jwks = MemberKeys::new();
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
    let jwks = MemberKeys::new();
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
    let jwks = MemberKeys::new();
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
    let management = format!("Bearer {}", jwks.token("user-1", TENANT, 300));
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
/// the peer's. #55: the secret key without a usable address marks the identity server-vouched.
/// Neither header reaches auth.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_app_server_forwards_its_users_address_with_the_secret_key() {
    use chelabase_types::data_key::{generate, DataKeyKind};
    let jwks = MemberKeys::new();
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
    let sign_in = |server_key: Option<&str>, client_ip: Option<&str>| {
        let mut req = reqwest::Client::new()
            .post(format!("{base}/auth/v1/app/sign-in"))
            .header("authorization", format!("Bearer {public}"));
        if let Some(key) = server_key {
            req = req.header("chelabase-server-key", key);
        }
        if let Some(ip) = client_ip {
            req = req.header("chelabase-client-ip", ip);
        }
        req.body("{}").send()
    };

    // `None`: the header is ignored and auth sees the loopback peer. The last column is
    // whether the identity is server-vouched (#55).
    for (server_key, client_ip, vouched, server_vouched) in [
        (Some(secret.as_str()), Some("203.0.113.9"), Some("203.0.113.9"), false),
        (Some(secret.as_str()), Some("2001:db8::1"), Some("2001:db8::1"), false),
        (Some(other_project.as_str()), Some("203.0.113.9"), None, false),
        (Some(public.as_str()), Some("203.0.113.9"), None, false),
        (Some(secret.as_str()), Some("203.0.113.9:443"), None, true),
        (Some(secret.as_str()), None, None, true),
        (Some(other_project.as_str()), None, None, false),
        (None, None, None, false),
    ] {
        let client_ip = client_ip.unwrap_or("-");
        *seen.lock().unwrap() = None;
        let resp = sign_in(server_key, (client_ip != "-").then_some(client_ip)).await.unwrap();
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
        assert_eq!(data.server_vouched, server_vouched, "{client_ip}");
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

// #119: member tokens signed by auth (`auth/member-jwt.key`), verified with the public key.
mod member_tokens {
    use super::*;
    use chelabase_auth::app_token::{app_audience, AppClaims, AppTokenSigner, APP_ISSUER};
    use chelabase_auth::member_token::{MemberClaims, MemberTokenSigner, MEMBER_AUDIENCE};
    use uuid::Uuid;

    struct MemberStack {
        base: String,
        keys: MemberKeys,
        verify: InternalVerifier,
        seen: Seen,
        _gateway: GatewayProcess,
    }

    fn now() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
    }

    /// A gateway with `CHELABASE_MEMBER_JWT_PUBLIC_KEY` set (`with_key`) or unset.
    async fn member_stack(with_key: bool) -> MemberStack {
        let keys = MemberKeys::new();
        let (upstream, seen) = spawn_upstream().await;
        // Blank is unset: no member verify key.
        let env: Vec<(&str, &str)> =
            if with_key { vec![] } else { vec![("CHELABASE_MEMBER_JWT_PUBLIC_KEY", "")] };
        let (base, verify, gateway) = spawn_gateway_with(&upstream, &keys, &env).await;
        MemberStack { base, keys, verify, seen, _gateway: gateway }
    }

    impl MemberStack {
        fn claims(&self, tid: Uuid) -> MemberClaims {
            MemberClaims {
                iss: self.keys.issuer().to_string(),
                aud: MEMBER_AUDIENCE.into(),
                sub: "user_ana".into(),
                tid,
                sid: "sess_1".into(),
                email: "ana@example.com".into(),
                name: "Ana Lima".into(),
                iat: now(),
                exp: now() + 300,
            }
        }

        async fn post(&self, bearer: &str) -> reqwest::Response {
            reqwest::Client::new()
                .post(format!("{}/v1/databases", self.base))
                .header("authorization", format!("Bearer {bearer}"))
                .json(&serde_json::json!({ "name": "demo" }))
                .send()
                .await
                .unwrap()
        }

        fn upstream_saw(&self) -> Option<HashMap<String, String>> {
            self.seen.lock().unwrap().clone()
        }
    }

    // AC 1: the request reaches the orchestrator as that member of tenant `tid`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn member_token_reaches_orchestrator() {
        let stack = member_stack(true).await;
        let tid = Uuid::new_v4();
        let token = stack.keys.sign(&stack.claims(tid));
        let resp = stack.post(&token).await;
        assert_eq!(resp.status(), 200);
        let seen = stack.upstream_saw().expect("upstream was called");
        assert!(!seen.contains_key("authorization"));
        let claims = stack.verify.verify(&seen[IDENTITY_HEADER], "orchestrator").unwrap();
        assert_eq!(claims.sub, "user_ana");
        assert_eq!(claims.tenant_id, Some(tid));
        // Review fix 1: the member token's session, which the orchestrator checks is live.
        assert_eq!(claims.sid.as_deref(), Some("sess_1"));
        assert_eq!(claims.auth, chelabase_auth::internal::AuthMethod::ManagementJwt);
        assert_eq!(claims.email.as_deref(), Some("ana@example.com"));
        assert_eq!(claims.name.as_deref(), Some("Ana Lima"));
    }

    // AC 2, AC 3, AC 6 at the gateway: each is 401 and never proxied.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bad_member_tokens_are_401_and_never_proxied() {
        let stack = member_stack(true).await;
        let tid = Uuid::new_v4();
        let mut bad: Vec<(&str, String)> = Vec::new();

        let (other_private, other_public) = generate_keypair_pem();
        let foreign = MemberTokenSigner::from_pem(other_private.as_bytes(), other_public.as_bytes(), stack.keys.issuer()).unwrap();
        bad.push(("foreign key", foreign.sign(&stack.claims(tid)).unwrap()));

        let mut wrong_aud = stack.claims(tid);
        wrong_aud.aud = app_audience(Uuid::new_v4());
        bad.push(("aud", stack.keys.sign(&wrong_aud)));
        let mut wrong_iss = stack.claims(tid);
        wrong_iss.iss = "http://evil.example".into();
        bad.push(("iss", stack.keys.sign(&wrong_iss)));
        let mut expired = stack.claims(tid);
        expired.iat = now() - 400;
        expired.exp = now() - 31;
        bad.push(("expired", stack.keys.sign(&expired)));

        let project = Uuid::new_v4();
        let app = AppClaims {
            iss: APP_ISSUER.into(),
            aud: app_audience(project),
            sub: Uuid::new_v4(),
            sid: Uuid::new_v4(),
            project,
            email: "a@example.com".into(),
            email_verified: true,
            role: "authenticated".into(),
            iat: now(),
            exp: now() + 300,
        };
        let (app_private, app_public) = generate_keypair_pem();
        let app_signer = AppTokenSigner::from_pem(app_private.as_bytes(), app_public.as_bytes()).unwrap();
        bad.push(("app token", app_signer.sign(&app).unwrap()));

        // The gateway's own internal token, minted by another gateway key.
        let (gw_private, _) = generate_keypair_pem();
        let internal = chelabase_auth::internal::InternalSigner::from_pem(gw_private.as_bytes()).unwrap();
        let identity = chelabase_auth::internal::Identity {
            sub: "user_ana".into(),
            auth: chelabase_auth::internal::AuthMethod::ManagementJwt,
            request_id: "r".into(),
            data: None,
            email: None,
            name: None,
        };
        bad.push(("internal token", internal.sign_for_tenant("orchestrator", &identity, tid, "sess_1").unwrap()));

        // `alg: none` (`{"alg":"none"}`) over a good token's payload.
        let good = stack.keys.sign(&stack.claims(tid));
        let payload = good.split('.').nth(1).unwrap();
        bad.push(("alg none", format!("eyJhbGciOiJub25lIn0.{payload}.")));

        for (what, token) in bad {
            let resp = stack.post(&token).await;
            assert_eq!(resp.status(), 401, "{what}");
            assert!(stack.upstream_saw().is_none(), "{what} was proxied");
        }
    }

    // Without the member key, every bearer is refused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn member_token_without_the_key_is_401() {
        let stack = member_stack(false).await;
        let token = stack.keys.sign(&stack.claims(Uuid::new_v4()));
        assert_eq!(stack.post(&token).await.status(), 401);
        assert!(stack.upstream_saw().is_none());
    }
}

// ---- MCP core m2: `/mcp` through the gateway ----

/// A raw upstream that answers one request with an event stream: a first event at once, the
/// second only after `pause`. Returns the request head it saw.
async fn spawn_mcp_stream_upstream(pause: Duration) -> (String, Arc<Mutex<Option<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(None));
    let record = seen.clone();
    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let head = read_head(&mut sock).await;
        *record.lock().unwrap() = Some(head);
        sock.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\
              Transfer-Encoding: chunked\r\n\r\n",
        )
        .await
        .unwrap();
        sock.write_all(&chunk("event: message\ndata: {\"id\":1}\n\n")).await.unwrap();
        tokio::time::sleep(pause).await;
        sock.write_all(&chunk("event: message\ndata: {\"id\":2}\n\n")).await.unwrap();
        sock.write_all(b"0\r\n\r\n").await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
    });
    (format!("http://{addr}"), seen)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn event_stream_from_mcp_is_not_buffered() {
    let keys = MemberKeys::new();
    let (api, _) = spawn_upstream().await;
    let (mcp, seen) = spawn_mcp_stream_upstream(Duration::from_secs(6)).await;
    let (base, _verify, _gateway) = spawn_gateway_with(&api, &keys, &[("CHELABASE_MCP_URL", mcp.as_str())]).await;
    let mut resp = reqwest::Client::new()
        .post(format!("{base}/mcp"))
        .header("authorization", "Bearer x")
        .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
        .body(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "text/event-stream");
    // The first event arrives while the upstream still holds the second.
    let first = tokio::time::timeout(Duration::from_secs(3), resp.chunk())
        .await
        .expect("the first event reaches the client before the stream ends")
        .unwrap()
        .expect("a first chunk");
    assert!(String::from_utf8_lossy(&first).contains(r#"data: {"id":1}"#), "{first:?}");
    let head = seen.lock().unwrap().clone().expect("the upstream was called").to_ascii_lowercase();
    assert!(head.contains("\r\nauthorization: bearer x\r\n"), "{head}");
    assert!(!head.contains("x-chelabase-identity"), "{head}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn www_authenticate_from_mcp_reaches_the_client() {
    let keys = MemberKeys::new();
    let (api, _) = spawn_upstream().await;
    let challenge = r#"Bearer error="invalid_token", resource_metadata="http://127.0.0.1/.well-known/oauth-protected-resource/mcp""#;
    let app = Router::new().fallback(move || async move {
        (
            axum::http::StatusCode::UNAUTHORIZED,
            [("www-authenticate", challenge)],
            Json(serde_json::json!({ "error": "invalid_token" })),
        )
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mcp = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (base, _verify, _gateway) = spawn_gateway_with(&api, &keys, &[("CHELABASE_MCP_URL", mcp.as_str())]).await;

    let resp = reqwest::Client::new()
        .post(format!("{base}/mcp"))
        .header("authorization", "Bearer expired")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let got: Vec<_> = resp.headers().get_all("www-authenticate").iter().map(|v| v.to_str().unwrap().to_string()).collect();
    assert_eq!(got, vec![challenge.to_string()]);

    // Without a bearer the gateway answers itself, with the challenge naming the API origin.
    let resp = reqwest::Client::new().post(format!("{base}/mcp")).body("{}").send().await.unwrap();
    assert_eq!(resp.status(), 401);
    assert_eq!(
        resp.headers()["www-authenticate"],
        r#"Bearer resource_metadata="http://127.0.0.1/.well-known/oauth-protected-resource/mcp""#
    );
}

// ---- #395: refused `/mcp` bearers, limited per client address ----

/// An mcp upstream that refuses every bearer but `Bearer good` with a 401, and counts calls.
async fn spawn_mcp_refusing_upstream() -> (String, Arc<Mutex<usize>>) {
    let calls = Arc::new(Mutex::new(0usize));
    let count = calls.clone();
    let app = Router::new().fallback(move |req: Request| {
        let count = count.clone();
        async move {
            *count.lock().unwrap() += 1;
            let good = req.headers().get("authorization").is_some_and(|v| v == "Bearer good");
            if good {
                (axum::http::StatusCode::OK, Json(serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": {} })))
            } else {
                (axum::http::StatusCode::UNAUTHORIZED, Json(serde_json::json!({ "error": "invalid_token" })))
            }
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), calls)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_bad_mcp_bearers_are_limited_per_client_through_the_gateway() {
    const BURST: usize = gateway::bad_bearers::BURST as usize;
    let keys = MemberKeys::new();
    let (api, _) = spawn_upstream().await;
    let (mcp, calls) = spawn_mcp_refusing_upstream().await;
    // The test client is 127.0.0.1: trusting it lets `X-Forwarded-For` stand for other clients.
    let (base, _verify, _gateway) = spawn_gateway_with(
        &api,
        &keys,
        &[("CHELABASE_MCP_URL", mcp.as_str()), ("CHELABASE_GATEWAY_TRUSTED_PROXIES", "127.0.0.0/8")],
    )
    .await;
    let client = reqwest::Client::new();
    let post = |from: &'static str, bearer: Option<&'static str>| {
        let mut req = client.post(format!("{base}/mcp")).header("x-forwarded-for", from).body("{}");
        if let Some(bearer) = bearer {
            req = req.header("authorization", bearer);
        }
        async move { req.send().await.unwrap() }
    };

    const BAD: &str = "203.0.113.9";
    // Half the burst missing bearers (the gateway's 401), half bad ones (the upstream's), with
    // a good call between each: every refusal is a 401, so none was counted twice, and the
    // good calls took nothing.
    for i in 0..BURST {
        let bearer = if i % 2 == 0 { None } else { Some("Bearer bad") };
        assert_eq!(post(BAD, bearer).await.status(), 401, "refusal {i}");
        if i + 1 < BURST {
            assert_eq!(post(BAD, Some("Bearer good")).await.status(), 200, "good call after refusal {i}");
        }
    }
    // Over the limit: refused at the gateway, a good bearer from that address included.
    let upstream_calls = *calls.lock().unwrap();
    for bearer in [Some("Bearer good"), Some("Bearer bad"), None] {
        let resp = post(BAD, bearer).await;
        assert_eq!(resp.status(), 429, "{bearer:?}");
        let wait: u64 = resp.headers()["retry-after"].to_str().unwrap().parse().unwrap();
        assert!((1..=2).contains(&wait), "{wait}");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["error"]["code"], "rate_limited");
    }
    assert_eq!(*calls.lock().unwrap(), upstream_calls, "a limited request never reaches mcp");

    // Another client is served.
    assert_eq!(post("203.0.113.10", Some("Bearer good")).await.status(), 200);
    assert_eq!(post("203.0.113.10", Some("Bearer bad")).await.status(), 401);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_spoofed_forwarded_address_does_not_dodge_the_mcp_limit() {
    const BURST: usize = gateway::bad_bearers::BURST as usize;
    let keys = MemberKeys::new();
    let (api, _) = spawn_upstream().await;
    let (mcp, _calls) = spawn_mcp_refusing_upstream().await;
    // No trusted proxy: the client's `X-Forwarded-For` is not believed.
    let (base, _verify, _gateway) = spawn_gateway_with(&api, &keys, &[("CHELABASE_MCP_URL", mcp.as_str())]).await;
    let client = reqwest::Client::new();
    for i in 0..BURST {
        let resp = client
            .post(format!("{base}/mcp"))
            .header("x-forwarded-for", format!("198.18.0.{i}"))
            .header("authorization", "Bearer bad")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "refusal {i}");
    }
    let resp = client
        .post(format!("{base}/mcp"))
        .header("x-forwarded-for", "198.18.1.1")
        .header("authorization", "Bearer good")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);
}

/// Sends a bad-bearer `POST /mcp` whose body is cut short, hangs up its side and reads to the
/// end: the client goes away before the gateway reads mcp's answer.
async fn abandon_bad_mcp_bearer(base: &str) {
    let addr = base.trim_start_matches("http://");
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer bad\r\nContent-Type: application/json\r\nContent-Length: 100\r\n\r\n{{"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.shutdown().await.unwrap();
    let mut answer = Vec::new();
    let _ = stream.read_to_end(&mut answer).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_mcp_bearers_abandoned_before_the_answer_still_count() {
    const BURST: usize = gateway::bad_bearers::BURST as usize;
    let keys = MemberKeys::new();
    let (api, _) = spawn_upstream().await;
    let (mcp, calls) = spawn_mcp_refusing_upstream().await;
    let (base, _verify, _gateway) = spawn_gateway_with(&api, &keys, &[("CHELABASE_MCP_URL", mcp.as_str())]).await;
    // Twice the allowance: a refusal is counted when the gateway logs the request, which can
    // come just after the client saw the connection close.
    for _ in 0..2 * BURST {
        abandon_bad_mcp_bearer(&base).await;
    }
    let reached = *calls.lock().unwrap();
    let resp = reqwest::Client::new()
        .post(format!("{base}/mcp"))
        .header("authorization", "Bearer good")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        429,
        "mcp saw {reached} of {} abandoned bad bearers; each must count",
        2 * BURST
    );
}

/// An mcp upstream that reads the request and hangs up without answering.
async fn spawn_mcp_hanging_up_upstream() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
            });
        }
    });
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_upstream_failures_never_count_as_refusals() {
    const BURST: usize = gateway::bad_bearers::BURST as usize;
    let keys = MemberKeys::new();
    let (api, _) = spawn_upstream().await;
    // A port nothing listens on: every request is a connect failure.
    let unreachable = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let hanging_up = spawn_mcp_hanging_up_upstream().await;
    for (what, mcp) in [("unreachable", unreachable), ("hanging up", hanging_up)] {
        let (base, _verify, _gateway) = spawn_gateway_with(&api, &keys, &[("CHELABASE_MCP_URL", mcp.as_str())]).await;
        // A fresh connection per request: the gateway may close one after a 502, and a pooled
        // connection it already closed would fail the next send with a reset.
        let client = reqwest::Client::builder().pool_max_idle_per_host(0).build().unwrap();
        for i in 0..2 * BURST + 1 {
            let resp = client
                .post(format!("{base}/mcp"))
                .header("authorization", "Bearer bad")
                .body("{}")
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 502, "{what}: request {i} is an upstream failure, never a 429");
        }
    }
}

// ---- #322: data API and storage requests, limited per key and client ----

/// A gateway in front of one mock upstream for every service, with three keys known (two
/// public, one secret, each its own id) and `extra` settings. The test client is 127.0.0.1:
/// trusting it lets `X-Forwarded-For` stand for other clients.
async fn spawn_limited_gateway(extra: &[(&str, &str)]) -> (String, [String; 3], GatewayProcess) {
    use chelabase_types::data_key::{generate, DataKeyKind};
    let jwks = MemberKeys::new();
    let (upstream, _seen) = spawn_upstream().await;
    let keys = [generate(DataKeyKind::Public).key, generate(DataKeyKind::Public).key, generate(DataKeyKind::Secret).key];
    let answer = |n: u128, kind: &str, role: &str| {
        serde_json::json!({
            "key_id": uuid::Uuid::from_u128(n).to_string(),
            "branch_id": "22222222-2222-4222-8222-222222222222",
            "project_id": E2E_PROJECT,
            "kind": kind,
            "role": role,
        })
    };
    let internal = spawn_key_listener(vec![
        (keys[0].clone(), answer(1, "public", "anon")),
        (keys[1].clone(), answer(2, "public", "anon")),
        (keys[2].clone(), answer(3, "secret", "service")),
    ])
    .await;
    let mut env = vec![
        ("CHELABASE_REST_URL", upstream.as_str()),
        ("CHELABASE_STORAGE_URL", upstream.as_str()),
        ("CHELABASE_INTERNAL_URL", internal.as_str()),
        ("CHELABASE_DATA_KEYS_TOKEN", "e2e-data-keys-token"),
        ("CHELABASE_GATEWAY_TRUSTED_PROXIES", "127.0.0.0/8"),
    ];
    env.extend_from_slice(extra);
    let (base, _verify, gateway) = spawn_gateway_with(&upstream, &jwks, &env).await;
    (base, keys, gateway)
}

/// One request on its own connection (the gateway may close one after its own error answer)
/// from client `from` with `key`.
async fn keyed(base: &str, method: &str, path: &str, key: &str, from: &str) -> reqwest::Response {
    let client = reqwest::Client::builder().pool_max_idle_per_host(0).build().unwrap();
    let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
    let mut req = client
        .request(method.clone(), format!("{base}{path}"))
        .header("authorization", format!("Bearer {key}"))
        .header("origin", "https://app.example")
        .header("x-forwarded-for", from);
    if method == reqwest::Method::POST {
        req = req.body("{}");
    }
    req.send().await.unwrap()
}

/// After a client used up its burst: the next few requests (a token may refill in between on
/// a slow run) include a 429 with `Retry-After`, the route's CORS headers and `rate_limited`.
async fn expect_limited(base: &str, method: &str, path: &str, key: &str, from: &str) {
    for _ in 0..3 {
        let resp = keyed(base, method, path, key, from).await;
        if resp.status() == 200 {
            continue;
        }
        assert_eq!(resp.status(), 429, "{method} {path}");
        let h = resp.headers();
        assert_eq!(h["retry-after"], "1", "{method} {path}");
        assert_eq!(h["access-control-allow-origin"], "*", "{method} {path}");
        assert!(h["access-control-expose-headers"].to_str().unwrap().contains("retry-after"), "{method} {path}");
        if method != "HEAD" {
            let body: serde_json::Value = resp.json().await.unwrap();
            assert_eq!(body["error"]["code"], "rate_limited", "{method} {path}");
        }
        return;
    }
    panic!("{method} {path} from {from} was never limited");
}

const DATA_QUERY: &str = "/data/v1/query";
const STORAGE_OBJECT: &str = "/storage/v1/object/files/a.txt";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_past_the_key_limit_get_429() {
    let (base, [key, other, secret], _gateway) = spawn_limited_gateway(&[
        ("CHELABASE_GATEWAY_DATA_RATE_PER_SECOND", "1"),
        ("CHELABASE_GATEWAY_DATA_RATE_BURST", "3"),
        ("CHELABASE_GATEWAY_STORAGE_RATE_PER_SECOND", "1"),
        ("CHELABASE_GATEWAY_STORAGE_RATE_BURST", "3"),
    ])
    .await;
    const A: &str = "203.0.113.9";
    for (method, path) in [("POST", DATA_QUERY), ("GET", STORAGE_OBJECT)] {
        for i in 0..3 {
            assert_eq!(keyed(&base, method, path, &key, A).await.status(), 200, "{method} {path} request {i}");
        }
        expect_limited(&base, method, path, &key, A).await;
    }
    // A secret key is never limited.
    for _ in 0..10 {
        assert_eq!(keyed(&base, "POST", DATA_QUERY, &secret, A).await.status(), 200);
    }
    // Limits are per key and per client: another key from that client, and the key from
    // another client, are served.
    assert_eq!(keyed(&base, "POST", DATA_QUERY, &other, A).await.status(), 200);
    assert_eq!(keyed(&base, "POST", DATA_QUERY, &key, "203.0.113.10").await.status(), 200);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn limits_are_per_key() {
    let (base, [key, other, _secret], _gateway) =
        spawn_limited_gateway(&[("CHELABASE_GATEWAY_DATA_RATE_PER_SECOND", "1"), ("CHELABASE_GATEWAY_DATA_RATE_BURST", "2")]).await;
    const A: &str = "203.0.113.9";
    for _ in 0..2 {
        assert_eq!(keyed(&base, "POST", DATA_QUERY, &key, A).await.status(), 200);
    }
    expect_limited(&base, "POST", DATA_QUERY, &key, A).await;
    for _ in 0..2 {
        assert_eq!(keyed(&base, "POST", DATA_QUERY, &other, A).await.status(), 200);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_and_data_budgets() {
    const A: &str = "203.0.113.9";
    // Separate (the default): storage's budget is untouched by data API requests.
    let (base, [key, ..], _gateway) = spawn_limited_gateway(&[
        ("CHELABASE_GATEWAY_DATA_RATE_PER_SECOND", "1"),
        ("CHELABASE_GATEWAY_DATA_RATE_BURST", "2"),
        ("CHELABASE_GATEWAY_STORAGE_RATE_PER_SECOND", "1"),
        ("CHELABASE_GATEWAY_STORAGE_RATE_BURST", "2"),
    ])
    .await;
    for _ in 0..2 {
        assert_eq!(keyed(&base, "POST", DATA_QUERY, &key, A).await.status(), 200);
    }
    expect_limited(&base, "POST", DATA_QUERY, &key, A).await;
    for _ in 0..2 {
        assert_eq!(keyed(&base, "GET", STORAGE_OBJECT, &key, A).await.status(), 200);
    }
    // Shared: storage requests draw on the data API's budget.
    let (base, [key, ..], _gateway) = spawn_limited_gateway(&[
        ("CHELABASE_GATEWAY_DATA_RATE_PER_SECOND", "1"),
        ("CHELABASE_GATEWAY_DATA_RATE_BURST", "4"),
        ("CHELABASE_GATEWAY_STORAGE_RATE_SHARED", "true"),
    ])
    .await;
    for (method, path) in [("POST", DATA_QUERY), ("POST", DATA_QUERY), ("GET", STORAGE_OBJECT), ("GET", STORAGE_OBJECT)] {
        assert_eq!(keyed(&base, method, path, &key, A).await.status(), 200, "{method} {path}");
    }
    expect_limited(&base, "GET", STORAGE_OBJECT, &key, A).await;
}

// ---- #426: function traffic, limited per original caller ----

/// [`keyed`] with the functions runtime's original-caller header set to `original`.
async fn keyed_original(base: &str, key: &str, from: &str, original: &str) -> reqwest::Response {
    let client = reqwest::Client::builder().pool_max_idle_per_host(0).build().unwrap();
    client
        .post(format!("{base}{DATA_QUERY}"))
        .header("authorization", format!("Bearer {key}"))
        .header("x-forwarded-for", from)
        .header("chelabase-original-client-ip", original)
        .body("{}")
        .send()
        .await
        .unwrap()
}

/// `n` requests, each with a new original address, from client `from`: how many were served.
async fn served_with_new_originals(base: &str, key: &str, from: &str, n: u32) -> u32 {
    let mut ok = 0;
    for i in 0..n {
        let original = format!("198.51.100.{}", i + 1);
        if keyed_original(base, key, from, &original).await.status() == 200 {
            ok += 1;
        }
    }
    ok
}

/// The test client (loopback, the TCP peer) is the runtime: each original caller it names
/// has their own bucket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_runtimes_original_caller_has_their_own_bucket() {
    let peer = std::net::Ipv4Addr::LOCALHOST.to_string();
    let (base, [key, ..], _gateway) = spawn_limited_gateway(&[
        ("CHELABASE_GATEWAY_DATA_RATE_PER_SECOND", "1"),
        ("CHELABASE_GATEWAY_DATA_RATE_BURST", "2"),
        ("CHELABASE_GATEWAY_TRUSTED_PROXIES", ""),
        ("CHELABASE_GATEWAY_FUNCTIONS_RUNTIME", &peer),
    ])
    .await;
    let peer = peer.as_str();
    for _ in 0..2 {
        assert_eq!(keyed_original(&base, &key, peer, "203.0.113.9").await.status(), 200);
    }
    let mut limited = false;
    for _ in 0..3 {
        limited |= keyed_original(&base, &key, peer, "203.0.113.9").await.status() == 429;
    }
    assert!(limited, "the first caller's bucket ran out");
    assert_eq!(served_with_new_originals(&base, &key, peer, 6).await, 6, "each new caller is served");
}

/// Refusals: the header from a peer that isn't the runtime (none configured, or another
/// address) is ignored, and so is a trusted proxy forwarding the runtime's address. Every
/// request then draws on its client's one bucket, whatever original it names.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_original_caller_header_is_ignored_from_any_other_peer() {
    let limits = [("CHELABASE_GATEWAY_DATA_RATE_PER_SECOND", "1"), ("CHELABASE_GATEWAY_DATA_RATE_BURST", "2")];
    let loopback = std::net::Ipv4Addr::LOCALHOST.to_string();
    let loopback = loopback.as_str();
    type Case<'a> = (&'a str, [(&'a str, &'a str); 2], &'a str);
    let cases: [Case; 3] = [
        ("no runtime configured", [("CHELABASE_GATEWAY_TRUSTED_PROXIES", ""), ("CHELABASE_GATEWAY_FUNCTIONS_RUNTIME", "")], loopback),
        ("another peer", [("CHELABASE_GATEWAY_TRUSTED_PROXIES", ""), ("CHELABASE_GATEWAY_FUNCTIONS_RUNTIME", "192.0.2.1")], loopback),
        // The peer is a trusted proxy whose forwarded client is the runtime's address.
        ("a forwarded runtime address", [("CHELABASE_GATEWAY_TRUSTED_PROXIES", "127.0.0.0/8"), ("CHELABASE_GATEWAY_FUNCTIONS_RUNTIME", "192.0.2.1")], "192.0.2.1"),
    ];
    for (case, settings, from) in cases {
        let mut env = limits.to_vec();
        env.extend_from_slice(&settings);
        let (base, [key, ..], _gateway) = spawn_limited_gateway(&env).await;
        let served = served_with_new_originals(&base, &key, from, 6).await;
        assert!(served <= 3, "{case}: {served} of 6 served; the header was believed");
    }
}
