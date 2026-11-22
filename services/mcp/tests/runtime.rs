//! MCP core m7: tool calls through the runtime client against a stub `mcp-runtime`, the limits,
//! the exec_id registry and the internal listener. The live tests (ignored) need the real
//! runtime on the workbench (m12, #356).

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::extract::State;
use axum::routing::post;
use chelabase_auth::member_token::{
    MEMBER_AUDIENCE, MemberClaims, MemberTokenSigner, MemberTokenVerifier,
};
use mcp::app::{Settings, router};
use mcp::internal::{Registry, bundle_sha256};
use mcp::limits::Limits;
use mcp::runtime::{Runtime, new_exec_id};
use reqwest::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const ISSUER: &str = "http://auth.test";
const VERSION: &str = "2026-07-28";
const TIMEOUT: Duration = Duration::from_millis(300);

// ---- logs ----

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn logs() -> &'static Buf {
    static LOGS: OnceLock<Buf> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buf = Buf::default();
        let writer = buf.clone();
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(mcp::DEFAULT_LOG_FILTER))
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .init();
        buf
    })
}

fn audit_lines() -> Vec<String> {
    String::from_utf8_lossy(&logs().0.lock().unwrap())
        .lines()
        .filter(|l| l.contains("mcp::audit"))
        .map(str::to_owned)
        .collect()
}

/// Waits up to 5 s for `cond`.
async fn eventually(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---- the stub runtime ----

#[derive(Clone)]
enum Mode {
    /// Answer 200 with this body.
    Answer(Value),
    /// Answer with this status and a non-JSON body.
    Status(u16),
    /// Never answer.
    Hang,
}

struct Stub {
    mode: Mutex<Mode>,
    registry: Registry,
    /// Each `/run` body, and whether its exec_id was registered when it arrived.
    seen: Mutex<Vec<(Value, bool)>>,
    in_flight: AtomicUsize,
}

struct InFlight(Arc<Stub>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn stub_run(
    State(stub): State<Arc<Stub>>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let registered = stub
        .registry
        .contains(body["exec_id"].as_str().unwrap_or(""));
    stub.seen.lock().unwrap().push((body, registered));
    stub.in_flight.fetch_add(1, Ordering::SeqCst);
    let _guard = InFlight(stub.clone());
    let mode = stub.mode.lock().unwrap().clone();
    match mode {
        Mode::Answer(v) => Json(v).into_response(),
        Mode::Status(s) => (StatusCode::from_u16(s).unwrap(), "nope").into_response(),
        Mode::Hang => std::future::pending().await,
    }
}

async fn start_stub(registry: Registry, mode: Mode) -> (String, Arc<Stub>) {
    let stub = Arc::new(Stub {
        mode: Mutex::new(mode),
        registry,
        seen: Mutex::new(vec![]),
        in_flight: AtomicUsize::new(0),
    });
    let app = axum::Router::new()
        .route("/run", post(stub_run))
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), stub)
}

// ---- the harness ----

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

struct Harness {
    url: String,
    http: chelabase_http::Client,
    signer: MemberTokenSigner,
    registry: Registry,
    stub: Arc<Stub>,
}

impl Harness {
    async fn start(mode: Mode) -> Self {
        Self::with(mode, Limits::default(), None).await
    }

    async fn with(mode: Mode, limits: Limits, runtime_url: Option<String>) -> Self {
        logs();
        let (private, public) = chelabase_auth::internal::generate_keypair_pem();
        let signer =
            MemberTokenSigner::from_pem(private.as_bytes(), public.as_bytes(), ISSUER).unwrap();
        let verifier = MemberTokenVerifier::from_pem(public.as_bytes(), ISSUER).unwrap();
        let registry = Registry::default();
        let (stub_url, stub) = start_stub(registry.clone(), mode).await;
        let runtime = Runtime::new(
            runtime_url.as_deref().unwrap_or(&stub_url),
            TIMEOUT,
            registry.clone(),
        )
        .unwrap();
        let settings = Settings {
            api_base: "https://api.example.test".into(),
            allowed_origins: vec![],
            log_code: false,
            limits,
        };
        let app = router(settings, verifier, Arc::new(runtime));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Harness {
            url: format!("http://{addr}"),
            // A call that never answers fails the test instead of hanging it.
            http: chelabase_http::client_builder()
                .timeout(Duration::from_secs(15))
                .build()
                .unwrap(),
            signer,
            registry,
            stub,
        }
    }

    fn token(&self, member: &str) -> String {
        let claims = MemberClaims {
            iss: ISSUER.into(),
            aud: MEMBER_AUDIENCE.into(),
            sub: member.into(),
            tid: Uuid::from_u128(7),
            sid: "sess".into(),
            email: "m@example.com".into(),
            name: "M".into(),
            iat: now() - 10,
            exp: now() + 300,
        };
        self.signer.sign(&claims).unwrap()
    }

    fn request(&self, member: &str, tool: &str, code: &str) -> reqwest::RequestBuilder {
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {
                "name": tool,
                "arguments": {"code": code},
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": VERSION,
                    "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "1"},
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        });
        self.http
            .post(format!("{}/mcp", self.url))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", VERSION)
            .header("mcp-method", "tools/call")
            .header("mcp-name", tool)
            .bearer_auth(self.token(member))
            .body(body.to_string())
    }

    /// One `tools/call` by `member`: its JSON-RPC result.
    async fn call_as(&self, member: &str, tool: &str, code: &str) -> Value {
        let resp = self.request(member, tool, code).send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v: Value = resp.json().await.unwrap();
        v["result"].clone()
    }

    async fn search(&self, code: &str) -> Value {
        self.call_as("member-1", "search", code).await
    }

    fn seen(&self) -> Vec<(Value, bool)> {
        self.stub.seen.lock().unwrap().clone()
    }

    fn set_mode(&self, mode: Mode) {
        *self.stub.mode.lock().unwrap() = mode;
    }
}

fn text(result: &Value, i: usize) -> &str {
    result["content"][i]["text"].as_str().expect("a text block")
}

/// The tool error's `error` object.
fn tool_error(result: &Value) -> Value {
    assert_eq!(result["isError"], true, "{result}");
    serde_json::from_str::<Value>(text(result, 0)).unwrap()["error"].clone()
}

fn is_exec_id(id: &str) -> bool {
    id.len() == 64
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// ---- tests ----

#[tokio::test]
async fn a_value_and_its_logs_come_back() {
    let h = Harness::start(Mode::Answer(
        json!({"ok": true, "result": {"paths": 3}, "logs": ["looked at 3 paths"]}),
    ))
    .await;
    let result = h.search("async () => ({paths: 3})").await;
    assert_ne!(result["isError"], true, "{result}");
    assert_eq!(result["structuredContent"], json!({"paths": 3}));
    assert_eq!(text(&result, 0), r#"{"paths":3}"#);
    assert!(text(&result, 1).contains("looked at 3 paths"), "{result}");

    let seen = h.seen();
    let body = &seen[0].0;
    assert_eq!(body["kind"], "search");
    assert_eq!(body["code"], "async () => ({paths: 3})");
    assert_eq!(body["timeout_ms"], 300);
    assert_eq!(body["spec_hash"], bundle_sha256());

    h.set_mode(Mode::Answer(json!({
        "ok": false, "error": {"code": "threw", "message": "boom"}, "logs": []
    })));
    let result = h.call_as("member-1", "execute", "async () => 1").await;
    let error = tool_error(&result);
    assert_eq!(error["code"], "threw");
    assert!(error["message"].as_str().unwrap().contains("boom"));
    assert!(
        h.seen()[1].0.get("spec_hash").is_none(),
        "execute sends no spec_hash"
    );
}

#[tokio::test]
async fn timeout_gives_up_and_unregisters() {
    let h = Harness::start(Mode::Hang).await;
    let started = Instant::now();
    let result = h.search("async () => { for (;;) {} }").await;
    let took = started.elapsed();
    assert_eq!(tool_error(&result)["code"], "timeout");
    assert!(took < TIMEOUT + Duration::from_secs(3), "took {took:?}");
    assert!(took >= TIMEOUT + Duration::from_secs(2), "took {took:?}");
    let seen = h.seen();
    assert!(seen[0].1, "the exec_id was registered before /run");
    assert!(h.registry.is_empty(), "the exec_id is still registered");
}

#[tokio::test]
async fn unreachable_runtime_is_runtime_unavailable() {
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let h = Harness::with(
        Mode::Hang,
        Limits::default(),
        Some(format!("http://{closed}")),
    )
    .await;
    let result = h.search("async () => 1").await;
    assert_eq!(tool_error(&result)["code"], "runtime_unavailable");
    assert!(h.registry.is_empty());

    // A runtime that answers but not with a run's answer (a restart, a spec it can't load).
    for mode in [
        Mode::Status(502),
        Mode::Status(500),
        Mode::Answer(json!([1])),
    ] {
        let h = Harness::start(mode).await;
        let result = h.search("async () => 1").await;
        assert_eq!(tool_error(&result)["code"], "runtime_unavailable");
        assert!(h.registry.is_empty());
    }
}

#[tokio::test]
async fn exec_ids_are_unique_and_random() {
    let h = Harness::start(Mode::Answer(json!({"ok": true, "result": 1, "logs": []}))).await;
    for _ in 0..5 {
        h.search("async () => 1").await;
    }
    let ids: Vec<String> = h
        .seen()
        .iter()
        .map(|(b, _)| b["exec_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids.len(), 5);
    for id in &ids {
        assert!(is_exec_id(id), "{id}");
        assert!(
            audit_lines().iter().any(|l| l.contains(id.as_str())),
            "the audit line names {id}"
        );
    }
    let mut many: Vec<String> = (0..10_000).map(|_| new_exec_id()).collect();
    many.extend(ids);
    let n = many.len();
    many.sort();
    many.dedup();
    assert_eq!(many.len(), n, "an exec_id repeated");
    // 256 random bits: every hex digit turns up in every position across many ids.
    for pos in 0..64 {
        let mut digits: Vec<u8> = many.iter().map(|id| id.as_bytes()[pos]).collect();
        digits.sort();
        digits.dedup();
        assert_eq!(digits.len(), 16, "position {pos}");
    }
}

#[tokio::test]
async fn the_public_listener_has_no_internal_routes() {
    let h = Harness::start(Mode::Hang).await;
    let get = h
        .http
        .get(format!("{}/internal/spec", h.url))
        .send()
        .await
        .unwrap();
    assert_eq!(get.status(), StatusCode::NOT_FOUND);
    let post = h
        .http
        .post(format!("{}/internal/exec/abc/request", h.url))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(post.status(), StatusCode::NOT_FOUND);

    // The internal listener serves the bundle whose bytes hash to x-spec-hash.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, mcp::internal::router())
            .await
            .unwrap()
    });
    let resp = h
        .http
        .get(format!("http://{addr}/internal/spec"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let header = resp.headers()["x-spec-hash"].to_str().unwrap().to_owned();
    let body = resp.bytes().await.unwrap();
    let digest: String = Sha256::digest(&body)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(header, digest);
    assert_eq!(header, bundle_sha256());
    let bundle: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(bundle["info"]["x-spec-hash"], mcp::tools::spec_hash());

    let with_hash = h
        .http
        .get(format!(
            "http://{addr}/internal/spec?hash={}",
            bundle_sha256()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(with_hash.status(), StatusCode::OK);
    let wrong = h
        .http
        .get(format!(
            "http://{addr}/internal/spec?hash={}",
            "0".repeat(64)
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_exec_id_is_gone_before_the_result_returns() {
    let h = Harness::start(Mode::Answer(json!({"ok": true, "result": 1, "logs": []}))).await;
    let result = h.search("async () => 1").await;
    assert_ne!(result["isError"], true, "{result}");
    let seen = h.seen();
    let id = seen[0].0["exec_id"].as_str().unwrap();
    assert!(seen[0].1, "registered while the run was in the runtime");
    assert!(
        !h.registry.contains(id),
        "still registered after the result"
    );
}

#[tokio::test]
async fn a_client_disconnect_ends_the_run_and_unregisters() {
    let limits = Limits {
        member_concurrency: 1,
        ..Limits::default()
    };
    let h = Harness::with(Mode::Hang, limits, None).await;
    let gone = h
        .request("member-1", "search", "async () => { for (;;) {} }")
        .timeout(Duration::from_millis(400))
        .send()
        .await;
    assert!(gone.is_err(), "the call should not have finished");
    assert_eq!(h.seen().len(), 1);
    // The run ends with the client: its exec_id goes, and the runtime request is dropped,
    // long before the run's own timeout.
    eventually("the exec_id to go", || h.registry.is_empty()).await;
    eventually("the runtime request to end", || {
        h.stub.in_flight.load(Ordering::SeqCst) == 0
    })
    .await;
    // Its slot is free: with one slot per member, the next call runs.
    h.set_mode(Mode::Answer(json!({"ok": true, "result": 1, "logs": []})));
    let result = h.search("async () => 1").await;
    assert_ne!(result["isError"], true, "{result}");
    assert!(
        audit_lines()
            .iter()
            .any(|l| l.contains("outcome=\"client_disconnected\"")),
        "{:?}",
        audit_lines()
    );
}

#[tokio::test]
async fn a_large_result_is_truncated_and_audited() {
    let big = "x".repeat(100 * 1024);
    let h = Harness::start(Mode::Answer(
        json!({"ok": true, "result": {"rows": big}, "logs": []}),
    ))
    .await;
    let result = h.search("async () => spec").await;
    assert_ne!(result["isError"], true, "{result}");
    let body = text(&result, 0);
    let total = json!({"rows": big}).to_string().len();
    let marker = format!(
        "\n--- TRUNCATED: 24000 of {total} bytes; return less (select fields, filter, page) ---"
    );
    assert!(body.ends_with(&marker), "{}", &body[body.len() - 200..]);
    assert!(body.len() <= 24_000 + marker.len());
    assert!(result.get("structuredContent").is_none_or(Value::is_null));
    let audit = audit_lines();
    assert!(
        audit.iter().any(|l| l.contains("truncated=true")),
        "{audit:?}"
    );
}

#[tokio::test]
async fn a_third_concurrent_call_is_rate_limited_at_once() {
    let h = Arc::new(Harness::start(Mode::Hang).await);
    let mut held = vec![];
    for _ in 0..2 {
        let h = h.clone();
        held.push(tokio::spawn(async move {
            h.search("async () => { for (;;) {} }").await
        }));
    }
    eventually("two runs in flight", || {
        h.stub.in_flight.load(Ordering::SeqCst) == 2
    })
    .await;
    let started = Instant::now();
    let result = h.search("async () => 1").await;
    assert!(started.elapsed() < Duration::from_secs(1));
    let error = tool_error(&result);
    assert_eq!(error["code"], "rate_limited");
    assert!(error["retry_after_ms"].as_u64().unwrap() > 0, "{error}");
    assert_eq!(h.seen().len(), 2, "the refused call reached the runtime");

    // Another member's call still runs.
    let other = h.clone();
    let _b =
        tokio::spawn(async move { other.call_as("member-2", "search", "async () => 2").await });
    eventually("the other member's run", || h.seen().len() == 3).await;
    assert!(
        audit_lines()
            .iter()
            .any(|l| l.contains("outcome=\"rate_limited\"")),
    );
}

// ---- live: the real runtime on the workbench (m12, #356) ----

/// The workbench's MCP endpoint and a member token for it, from the environment
/// `just workbench test` sets.
fn live() -> (String, String) {
    let url = std::env::var("CHELABASE_MCP_LIVE_URL")
        .expect("CHELABASE_MCP_LIVE_URL: run through `just workbench test`");
    let token = std::env::var("CHELABASE_MCP_LIVE_TOKEN")
        .expect("CHELABASE_MCP_LIVE_TOKEN: run through `just workbench test`");
    (url, token)
}

async fn live_call(url: &str, token: &str, code: &str) -> Value {
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {
            "name": "search", "arguments": {"code": code},
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": VERSION,
                "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "1"},
                "io.modelcontextprotocol/clientCapabilities": {}
            }
        }
    });
    let resp = chelabase_http::client_builder()
        .build()
        .unwrap()
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-protocol-version", VERSION)
        .header("mcp-method", "tools/call")
        .header("mcp-name", "search")
        .bearer_auth(token)
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    resp.json::<Value>().await.unwrap()["result"].clone()
}

#[tokio::test]
#[ignore = "live: needs mcp-runtime on the workbench (m12, #356)"]
async fn search_runs_in_the_runtime() {
    let (url, token) = live();
    let result = live_call(
        &url,
        &token,
        "async () => Object.keys(spec.paths).filter(p => p.startsWith('/v1/databases')).length",
    )
    .await;
    assert_ne!(result["isError"], true, "{result}");
    let n: u64 = text(&result, 0).parse().unwrap();
    assert!(n > 0, "{result}");
}

#[tokio::test]
#[ignore = "live: needs mcp-runtime on the workbench (m12, #356); restarts mcp-runtime"]
async fn a_busy_loop_times_out_and_the_next_call_works() {
    let (url, token) = live();
    let result = live_call(&url, &token, "async () => { for (;;) {} }").await;
    let code = tool_error(&result)["code"].clone();
    assert!(
        code == "timeout" || code == "runtime_unavailable",
        "{result}"
    );
    // The watchdog restarts the runtime within seconds; the next call works once it is back.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let result = live_call(&url, &token, "async () => spec.openapi").await;
        if result["isError"] != true {
            break;
        }
        assert_eq!(
            tool_error(&result)["code"],
            "runtime_unavailable",
            "{result}"
        );
        assert!(Instant::now() < deadline, "the runtime did not come back");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}
