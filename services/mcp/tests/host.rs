//! MCP core m11: `execute`'s API calls through the host callback, against a stub runtime (which
//! plays a script of `chelabase.request` envelopes) and a stub gateway (which records what
//! reaches it).
//!
//! A script is the `code` of a tool call: `{"calls": [envelope | {"raw": text}, …],
//! "then": "value" | "hang", "exec_id"?: id}`. The stub runtime POSTs each envelope to the
//! host's callback for the run's exec_id (or `exec_id`), and answers the run with
//! `{"answers": [{"http": host status, "answer": host body}, …]}` unless `then` is `hang`.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use chelabase_auth::member_token::{
    MEMBER_AUDIENCE, MemberClaims, MemberTokenSigner, MemberTokenVerifier,
};
use mcp::app::{Settings, router};
use mcp::host::{Host, HostConfig};
use mcp::internal::Registry;
use mcp::limits::Limits;
use mcp::routes_match::{Matcher, Refused};
use mcp::runtime::Runtime;
use reqwest::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

const ISSUER: &str = "http://auth.test";
const VERSION: &str = "2026-07-28";
const API_HOST: &str = "api.example.test";
const BRANCH: &str = "/v1/databases/db_1/branches/br_2";

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

fn all_logs() -> String {
    String::from_utf8_lossy(&logs().0.lock().unwrap()).into_owned()
}

/// Waits up to 5 s for `cond`.
async fn eventually(what: &str, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---- the stub gateway ----

#[derive(Clone, Debug)]
struct Seen {
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

#[derive(Clone)]
struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
    /// Never answer.
    hang: bool,
    delay: Duration,
}

impl Reply {
    fn json(status: u16, body: Value) -> Self {
        Reply {
            status,
            headers: vec![("content-type", "application/json".into())],
            body: body.to_string().into_bytes(),
            hang: false,
            delay: Duration::ZERO,
        }
    }

    fn hang() -> Self {
        Reply {
            hang: true,
            ..Reply::json(200, json!({}))
        }
    }
}

type Responder = Arc<dyn Fn(&Seen) -> Reply + Send + Sync>;

struct Gateway {
    seen: Mutex<Vec<Seen>>,
    responder: Mutex<Responder>,
    /// Set when a hanging request's connection was dropped (the host cut it).
    cut: AtomicBool,
}

struct CutGuard(Arc<Gateway>);

impl Drop for CutGuard {
    fn drop(&mut self) {
        self.0.cut.store(true, Ordering::SeqCst);
    }
}

async fn gateway_any(
    State(gw): State<Arc<Gateway>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let seen = Seen {
        method,
        uri,
        headers,
        body,
    };
    gw.seen.lock().unwrap().push(seen.clone());
    let responder = gw.responder.lock().unwrap().clone();
    let reply = responder(&seen);
    if reply.hang {
        let _guard = CutGuard(gw.clone());
        std::future::pending::<()>().await;
    }
    tokio::time::sleep(reply.delay).await;
    let mut response = (StatusCode::from_u16(reply.status).unwrap(), reply.body).into_response();
    response.headers_mut().remove("content-type");
    for (name, value) in reply.headers {
        response.headers_mut().append(name, value.parse().unwrap());
    }
    response
}

async fn start_gateway() -> (String, Arc<Gateway>) {
    let gw = Arc::new(Gateway {
        seen: Mutex::new(vec![]),
        responder: Mutex::new(Arc::new(|_: &Seen| Reply::json(200, json!({"ok": true})))),
        cut: AtomicBool::new(false),
    });
    let app = axum::Router::new()
        .fallback(gateway_any)
        .with_state(gw.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), gw)
}

// ---- the stub runtime ----

struct RuntimeStub {
    internal: Mutex<String>,
    http: chelabase_http::Client,
    /// Each run's exec_id.
    exec_ids: Mutex<Vec<String>>,
    /// The answers the last run's calls got (also when the tool result replaces its value).
    last_answers: Mutex<Vec<Value>>,
}

async fn runtime_run(State(stub): State<Arc<RuntimeStub>>, Json(job): Json<Value>) -> Response {
    let exec_id = job["exec_id"].as_str().unwrap().to_owned();
    stub.exec_ids.lock().unwrap().push(exec_id.clone());
    let script: Value = serde_json::from_str(job["code"].as_str().unwrap()).unwrap();
    let target = script["exec_id"].as_str().unwrap_or(&exec_id).to_owned();
    let url = format!(
        "{}/internal/exec/{target}/request",
        stub.internal.lock().unwrap()
    );
    let mut answers = vec![];
    for call in script["calls"].as_array().cloned().unwrap_or_default() {
        let body = match call.get("raw").and_then(Value::as_str) {
            Some(raw) => raw.to_owned(),
            None => call.to_string(),
        };
        let resp = stub
            .http
            .post(&url)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        let http = resp.status().as_u16();
        let text = resp.text().await.unwrap();
        let answer = serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text));
        answers.push(json!({"http": http, "answer": answer}));
    }
    *stub.last_answers.lock().unwrap() = answers.clone();
    if script["then"] == "hang" {
        std::future::pending::<()>().await;
    }
    Json(json!({"ok": true, "result": {"answers": answers}, "logs": []})).into_response()
}

async fn start_runtime() -> (String, Arc<RuntimeStub>) {
    let stub = Arc::new(RuntimeStub {
        internal: Mutex::new(String::new()),
        http: chelabase_http::client_builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap(),
        exec_ids: Mutex::new(vec![]),
        last_answers: Mutex::new(vec![]),
    });
    let app = axum::Router::new()
        .route("/run", post(runtime_run))
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
    internal: String,
    http: chelabase_http::Client,
    signer: MemberTokenSigner,
    registry: Registry,
    gateway: Arc<Gateway>,
    runtime: Arc<RuntimeStub>,
}

struct Options {
    timeout: Duration,
    config: HostConfig,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            timeout: Duration::from_secs(10),
            config: HostConfig::default(),
        }
    }
}

impl Harness {
    async fn start() -> Self {
        Self::with(Options::default()).await
    }

    async fn with(options: Options) -> Self {
        logs();
        let (private, public) = chelabase_auth::internal::generate_keypair_pem();
        let signer =
            MemberTokenSigner::from_pem(private.as_bytes(), public.as_bytes(), ISSUER).unwrap();
        let verifier = MemberTokenVerifier::from_pem(public.as_bytes(), ISSUER).unwrap();
        let registry = Registry::default();
        let (gateway_url, gateway) = start_gateway().await;
        let (runtime_url, runtime) = start_runtime().await;

        let mut config = options.config;
        if config.api_url.is_none() {
            config.api_url = Some(gateway_url);
        }
        let api_base = format!("https://{API_HOST}");
        let host = Arc::new(Host::new(&config, &api_base, registry.clone()).unwrap());
        let internal_app = mcp::internal::router().merge(mcp::host::router(host));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let internal = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, internal_app).await.unwrap() });
        *runtime.internal.lock().unwrap() = internal.clone();

        let limits = Limits {
            timeout: options.timeout,
            ..Limits::default()
        };
        let sandbox = Runtime::new(&runtime_url, options.timeout, registry.clone()).unwrap();
        let settings = Settings {
            api_base,
            allowed_origins: vec![],
            log_code: false,
            limits,
        };
        let app = router(settings, verifier, Arc::new(sandbox));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Harness {
            url: format!("http://{addr}"),
            internal,
            http: chelabase_http::client_builder()
                .timeout(Duration::from_secs(20))
                .build()
                .unwrap(),
            signer,
            registry,
            gateway,
            runtime,
        }
    }

    fn token_for(&self, member: &str, lifetime: u64) -> String {
        let claims = MemberClaims {
            iss: ISSUER.into(),
            aud: MEMBER_AUDIENCE.into(),
            sub: member.into(),
            tid: Uuid::from_u128(7),
            sid: "sess".into(),
            email: "m@example.com".into(),
            name: "M".into(),
            iat: now() - 10,
            exp: now() + lifetime,
        };
        self.signer.sign(&claims).unwrap()
    }

    fn token(&self, member: &str) -> String {
        self.token_for(member, 300)
    }

    fn respond(&self, f: impl Fn(&Seen) -> Reply + Send + Sync + 'static) {
        *self.gateway.responder.lock().unwrap() = Arc::new(f);
    }

    fn seen(&self) -> Vec<Seen> {
        self.gateway.seen.lock().unwrap().clone()
    }

    async fn post_tool(
        &self,
        token: &str,
        tool: &str,
        arguments: Value,
        read_only: bool,
    ) -> reqwest::Response {
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {
                "name": tool,
                "arguments": arguments,
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": VERSION,
                    "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "1"},
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        });
        let mut request = self
            .http
            .post(format!("{}/mcp", self.url))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", VERSION)
            .header("mcp-method", "tools/call")
            .header("mcp-name", tool)
            .header("x-request-id", "req-abc")
            .bearer_auth(token);
        if read_only {
            request = request.header("chelabase-mcp-read-only", "true");
        }
        request.body(body.to_string()).send().await.unwrap()
    }

    /// One `execute` by `member` running `script`: the JSON-RPC result.
    async fn execute_as(
        &self,
        member: &str,
        script: Value,
        destructive: bool,
        read_only: bool,
    ) -> Value {
        let resp = self
            .post_tool(
                &self.token(member),
                "execute",
                json!({"code": script.to_string(), "destructive": destructive}),
                read_only,
            )
            .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v: Value = resp.json().await.unwrap();
        v["result"].clone()
    }

    async fn execute(&self, script: Value) -> Value {
        self.execute_as("member-1", script, false, false).await
    }

    async fn execute_destructive(&self, script: Value) -> Value {
        self.execute_as("member-1", script, true, false).await
    }
}

fn calls(calls: Vec<Value>) -> Value {
    json!({ "calls": calls })
}

fn get(path: &str) -> Value {
    json!({"method": "GET", "path": path})
}

fn call(method: &str, path: &str, body: Value) -> Value {
    json!({"method": method, "path": path, "body": body})
}

/// The answers the script collected (a successful run).
fn answers(result: &Value) -> Vec<Value> {
    assert_ne!(result["isError"], true, "{result}");
    result["structuredContent"]["answers"]
        .as_array()
        .unwrap_or_else(|| panic!("{result}"))
        .clone()
}

/// The sandbox-visible answer of one call, checking the host answered 200.
fn answer(entry: &Value) -> &Value {
    assert_eq!(entry["http"], 200, "{entry}");
    &entry["answer"]
}

/// A host refusal's error code (`body.error.code` of a synthetic answer).
fn refusal(entry: &Value) -> &str {
    answer(entry)["body"]["error"]["code"]
        .as_str()
        .unwrap_or_else(|| panic!("not a refusal: {entry}"))
}

/// The tool error's `error` object.
fn tool_error(result: &Value) -> Value {
    assert_eq!(result["isError"], true, "{result}");
    let text = result["content"][0]["text"].as_str().unwrap();
    serde_json::from_str::<Value>(text).unwrap()["error"].clone()
}

// ---- forwarding ----

#[tokio::test]
async fn forwards_with_the_member_token() {
    let h = Harness::start().await;
    h.respond(|seen| match seen.uri.path() {
        "/v1/projects" => Reply::json(200, json!([{"id": "p_1"}])),
        _ => Reply::json(200, json!({"id": "db_1", "name": "shop"})),
    });
    let result = h
        .execute(calls(vec![
            json!({"method": "GET", "path": "/v1/projects", "query": {"limit": 5, "all": true, "q": "a b"}}),
            get("/v1/databases/db_1"),
        ]))
        .await;
    let answers = answers(&result);
    assert_eq!(answer(&answers[0])["status"], 200);
    assert_eq!(answer(&answers[0])["ok"], true);
    assert_eq!(answer(&answers[0])["body"], json!([{"id": "p_1"}]));
    assert_eq!(answer(&answers[1])["body"]["name"], "shop");

    let token = h.token("member-1");
    let seen = h.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].method, Method::GET);
    assert_eq!(seen[0].uri.path(), "/v1/projects");
    // Query keys go out in the order serde_json keeps them (sorted).
    assert_eq!(seen[0].uri.query(), Some("all=true&limit=5&q=a+b"));
    assert_eq!(seen[1].uri.path(), "/v1/databases/db_1");
    for s in &seen {
        // The token of the caller: same member, so same claims bar `iat`/`exp` seconds.
        let auth = s.header("authorization").unwrap();
        assert!(auth.starts_with("Bearer ey"), "{auth}");
        assert_eq!(
            auth.split('.').count(),
            token.split('.').count(),
            "a member token"
        );
        assert_eq!(s.header("host"), Some(API_HOST));
        let id = s.header("x-request-id").unwrap();
        assert!(id.starts_with("req-abc."), "{id}");
        assert!(s.body.is_empty());
        assert_eq!(s.header("content-type"), None, "no body, no content type");
    }
    assert_ne!(
        seen[0].header("x-request-id"),
        seen[1].header("x-request-id")
    );
}

#[tokio::test]
async fn an_unreachable_api_is_upstream_unavailable() {
    let closed = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let h = Harness::with(Options {
        config: HostConfig {
            api_url: Some(format!("http://{closed}")),
            ..HostConfig::default()
        },
        ..Options::default()
    })
    .await;
    let result = h.execute(calls(vec![get("/v1/projects")])).await;
    assert_eq!(refusal(&answers(&result)[0]), "upstream_unavailable");
}

#[tokio::test]
async fn unknown_route_makes_no_request() {
    let h = Harness::start().await;
    let paths = [
        "/v1/nope",
        "/auth/v1/token",
        "/auth/v1/sign-in",
        "/mcp",
        "/internal/spec",
        "/internal/exec/x/request",
        "/.well-known/oauth-protected-resource",
        "https://evil.example/v1/projects",
        "http://api.example.test/v1/projects",
        "//evil.example/v1/projects",
        "/v1/databases/db_1/../db_2",
        "/v1/databases/..",
        "/v1/databases/a%2Fb",
        "/v1/databases/db_1?x=1",
        "/v1/databases/a\\b",
        "/v1/databases/a\u{1}b",
        "/v1/databases/db_1/",
        "/v1/databases//db_1",
    ];
    let script = calls(paths.iter().map(|p| get(p)).collect());
    let result = h.execute(script).await;
    let got = answers(&result);
    assert_eq!(got.len(), paths.len());
    for (path, entry) in paths.iter().zip(&got) {
        assert_eq!(refusal(entry), "operation_not_allowed", "{path:?}: {entry}");
        assert_eq!(answer(entry)["ok"], false);
    }
    // A method that is not the operation's.
    let result = h
        .execute(calls(vec![call("PUT", "/v1/projects", json!({}))]))
        .await;
    assert_eq!(refusal(&answers(&result)[0]), "operation_not_allowed");
    assert!(h.seen().is_empty(), "{:?}", h.seen());
}

#[tokio::test]
async fn excluded_operation_makes_no_request() {
    let h = Harness::start().await;
    let result = h
        .execute_destructive(calls(vec![
            call(
                "POST",
                &format!("{BRANCH}/functions/f/deployments"),
                json!({}),
            ),
            call(
                "PUT",
                &format!("{BRANCH}/storage/object/b/a.png"),
                json!({}),
            ),
            call(
                "POST",
                &format!("{BRANCH}/storage/upload/session/s_1/complete"),
                json!({}),
            ),
        ]))
        .await;
    for entry in answers(&result) {
        assert_eq!(refusal(&entry), "operation_not_allowed", "{entry}");
    }
    assert!(h.seen().is_empty());
}

#[tokio::test]
async fn mcp_account_routes_are_denied_independently_of_the_bundle() {
    // Even a bundle that held them would not reach them.
    let bundle = json!({"paths": {
        "/v1/me/mcp-grants": {"get": {"operationId": "listMcpGrants"}},
        "/v1/me/mcp-grants/{id}": {"delete": {"operationId": "revokeMcpGrant"}},
    }});
    let matcher = Matcher::from_bundle(&bundle).unwrap();
    assert_eq!(
        matcher.find("GET", "/v1/me/mcp-grants"),
        Err(Refused::Denied)
    );
    assert_eq!(
        matcher.find("DELETE", "/v1/me/mcp-grants/g_1"),
        Err(Refused::Denied)
    );

    let h = Harness::start().await;
    let result = h
        .execute_destructive(calls(vec![
            get("/v1/me/mcp-grants"),
            call("DELETE", "/v1/me/mcp-grants/g_1", Value::Null),
        ]))
        .await;
    for entry in answers(&result) {
        assert_eq!(refusal(&entry), "operation_not_allowed", "{entry}");
    }
    assert!(h.seen().is_empty());
}

// ---- the destructive gate ----

#[tokio::test]
async fn destructive_without_the_flag_is_refused_and_sticky() {
    let h = Harness::start().await;
    // The code "catches" the refusal and returns a value anyway (the stub always does).
    let result = h
        .execute(calls(vec![
            get("/v1/projects"),
            call("DELETE", "/v1/databases/db_1", Value::Null),
            get("/v1/projects"),
        ]))
        .await;
    let error = tool_error(&result);
    assert_eq!(error["code"], "destructive_flag_required", "{result}");
    assert_eq!(error["operations"], json!(["deleteDatabase"]));
    assert!(
        result.get("structuredContent").is_none() || result["structuredContent"].is_null(),
        "the code's value is not the result: {result}"
    );
    // The sandbox saw the refusal on that call and every later one.
    let seen_by_code = h.runtime.last_answers.lock().unwrap().clone();
    assert_eq!(answer(&seen_by_code[0])["status"], 200);
    assert_eq!(refusal(&seen_by_code[1]), "destructive_flag_required");
    assert_eq!(refusal(&seen_by_code[2]), "destructive_flag_required");
    // Only the read before the refusal reached the API.
    let seen = h.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].uri.path(), "/v1/projects");
    assert!(all_logs().contains("destructive_flag_required"));
}

/// A refused run's error lists the calls already made, so a retry with the flag doesn't repeat
/// a write that happened.
#[tokio::test]
async fn a_refusal_lists_the_calls_already_made() {
    let h = Harness::start().await;
    h.respond(|_| Reply::json(201, json!({"id": "br_9"})));
    let result = h
        .execute(calls(vec![
            call(
                "POST",
                "/v1/databases/db_1/branches",
                json!({"name": "dev"}),
            ),
            call("DELETE", "/v1/databases/db_1", Value::Null),
        ]))
        .await;
    let error = tool_error(&result);
    assert_eq!(error["code"], "destructive_flag_required", "{result}");
    assert_eq!(error["operations"], json!(["deleteDatabase"]));
    assert_eq!(
        error["calls"],
        json!([{"operation": "createBranch", "status": 201}]),
        "the made write is reported: {error}"
    );
    assert_eq!(h.seen().len(), 1);
}

/// Nothing made: the refusal carries no `calls`.
#[tokio::test]
async fn a_refusal_before_any_call_has_no_calls() {
    let h = Harness::start().await;
    let result = h
        .execute(calls(vec![call(
            "DELETE",
            "/v1/databases/db_1",
            Value::Null,
        )]))
        .await;
    let error = tool_error(&result);
    assert_eq!(error["code"], "destructive_flag_required", "{result}");
    assert!(error.get("calls").is_none(), "{error}");
}

#[tokio::test]
async fn destructive_with_the_flag_is_forwarded() {
    let h = Harness::start().await;
    h.respond(|_| {
        Reply::json(
            403,
            json!({"error": {"code": "forbidden", "message": "viewers cannot delete"}}),
        )
    });
    let result = h
        .execute_destructive(calls(vec![call(
            "DELETE",
            "/v1/databases/db_1",
            Value::Null,
        )]))
        .await;
    let answers = answers(&result);
    let a = answer(&answers[0]);
    assert_eq!(a["status"], 403);
    assert_eq!(a["ok"], false);
    assert_eq!(a["body"]["error"]["code"], "forbidden");
    let seen = h.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, Method::DELETE);
}

#[tokio::test]
async fn content_rules_gate_console_sql_and_schema_changes() {
    let delete_tree = call(
        "POST",
        &format!("{BRANCH}/console/query"),
        json!({"tree": {"v": 1, "op": "delete", "from": "t", "where": {"col": "a", "eq": 2}}}),
    );
    let drop_column = call(
        "POST",
        &format!("{BRANCH}/schema-changes"),
        json!({"operations": [{"drop_column": {"table": {"name": "t"}, "column": "c"}}]}),
    );
    for (refused, operation) in [
        (delete_tree.clone(), "consoleQuery"),
        (drop_column.clone(), "applySchemaChanges"),
    ] {
        let h = Harness::start().await;
        let result = h.execute(calls(vec![refused])).await;
        let error = tool_error(&result);
        assert_eq!(error["code"], "destructive_flag_required");
        assert_eq!(error["operations"], json!([operation]));
        assert!(h.seen().is_empty());
    }
    // A select and a dry run pass without the flag.
    let h = Harness::start().await;
    let result = h
        .execute(calls(vec![
            call(
                "POST",
                &format!("{BRANCH}/console/query"),
                json!({"tree": {"v": 1, "op": "select", "from": "t"}}),
            ),
            call(
                "POST",
                &format!("{BRANCH}/schema-changes"),
                json!({"dry_run": true, "operations": [{"drop_column": {"table": {"name": "t"}, "column": "c"}}]}),
            ),
        ]))
        .await;
    for entry in answers(&result) {
        assert_eq!(answer(&entry)["status"], 200, "{entry}");
    }
    assert_eq!(h.seen().len(), 2);
    // With the flag, the delete goes through.
    let h = Harness::start().await;
    let result = h.execute_destructive(calls(vec![delete_tree])).await;
    assert_eq!(answer(&answers(&result)[0])["status"], 200);
    assert_eq!(h.seen().len(), 1);
}

// ---- runSql and read-only connections ----

#[tokio::test]
async fn run_sql_is_forced_read_only_without_the_flag() {
    let h = Harness::start().await;
    let path = format!("{BRANCH}/sql");
    let result = h
        .execute(calls(vec![
            call("POST", &path, json!({"sql": "select 1"})),
            call(
                "POST",
                &path,
                json!({"sql": "select 1", "read_only": false}),
            ),
            call("POST", &path, json!({"sql": "select 1", "read_only": "no"})),
            call("POST", &path, json!({"sql": "select 1", "read_only": true})),
        ]))
        .await;
    for entry in answers(&result) {
        assert_eq!(answer(&entry)["status"], 200, "{entry}");
    }
    let seen = h.seen();
    assert_eq!(seen.len(), 4);
    for s in &seen {
        assert_eq!(s.json(), json!({"sql": "select 1", "read_only": true}));
        assert_eq!(s.header("content-type"), Some("application/json"));
    }
}

#[tokio::test]
async fn run_sql_refusals_from_the_api_pass_through() {
    let h = Harness::start().await;
    h.respond(|_| {
        Reply::json(
            400,
            json!({"error": {"code": "read_only_script_refused", "message": "no SET"}}),
        )
    });
    let result = h
        .execute(calls(vec![call(
            "POST",
            &format!("{BRANCH}/sql"),
            json!({"sql": "set x = 1"}),
        )]))
        .await;
    let answers = answers(&result);
    let a = answer(&answers[0]);
    assert_eq!(a["status"], 400);
    assert_eq!(
        a["body"],
        json!({"error": {"code": "read_only_script_refused", "message": "no SET"}})
    );
}

#[tokio::test]
async fn run_sql_is_forwarded_unchanged_with_the_flag() {
    let h = Harness::start().await;
    let path = format!("{BRANCH}/sql");
    // A duplicate key: the host forwards the body it classified (the last value), once.
    let raw = format!(
        r#"{{"method":"POST","path":"{path}","body":{{"sql":"delete from t","read_only":true,"read_only":false}}}}"#
    );
    let result = h
        .execute_destructive(calls(vec![
            call(
                "POST",
                &path,
                json!({"sql": "delete from t", "read_only": false}),
            ),
            call("POST", &path, json!({"sql": "delete from t"})),
            json!({ "raw": raw }),
        ]))
        .await;
    for entry in answers(&result) {
        assert_eq!(answer(&entry)["status"], 200, "{entry}");
    }
    let seen = h.seen();
    assert_eq!(seen.len(), 3);
    assert_eq!(
        seen[0].json(),
        json!({"sql": "delete from t", "read_only": false})
    );
    assert_eq!(seen[1].json(), json!({"sql": "delete from t"}));
    let text = String::from_utf8(seen[2].body.to_vec()).unwrap();
    assert_eq!(text.matches("read_only").count(), 1, "{text}");
    assert_eq!(
        seen[2].json(),
        json!({"sql": "delete from t", "read_only": false})
    );
}

#[tokio::test]
async fn a_read_only_connection_refuses_every_write_whatever_the_flag() {
    for destructive in [false, true] {
        let h = Harness::start().await;
        let result = h
            .execute_as(
                "member-1",
                calls(vec![
                    get("/v1/projects"),
                    call(
                        "POST",
                        "/v1/databases/db_1/branches",
                        json!({"name": "preview"}),
                    ),
                    get("/v1/projects"),
                ]),
                destructive,
                true,
            )
            .await;
        let error = tool_error(&result);
        assert_eq!(error["code"], "read_only_connection", "{result}");
        assert_eq!(error["operations"], json!(["createBranch"]));
        assert_eq!(h.seen().len(), 1, "only the first read");

        let h = Harness::start().await;
        let result = h
            .execute_as(
                "member-1",
                calls(vec![call("DELETE", "/v1/databases/db_1", Value::Null)]),
                destructive,
                true,
            )
            .await;
        assert_eq!(tool_error(&result)["code"], "read_only_connection");
        assert!(h.seen().is_empty());
    }
}

#[tokio::test]
async fn a_read_only_connection_still_reads() {
    let h = Harness::start().await;
    let result = h
        .execute_as(
            "member-1",
            calls(vec![
                get("/v1/projects"),
                call(
                    "POST",
                    &format!("{BRANCH}/sql"),
                    json!({"sql": "select 1", "read_only": false}),
                ),
            ]),
            true,
            true,
        )
        .await;
    for entry in answers(&result) {
        assert_eq!(answer(&entry)["status"], 200, "{entry}");
    }
    let seen = h.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(
        seen[1].json(),
        json!({"sql": "select 1", "read_only": true})
    );
}

// ---- credentials ----

#[tokio::test]
async fn credential_answering_operations_are_not_sent_without_a_sealer() {
    let h = Harness::start().await;
    let matcher = Matcher::bundle();
    let mut script = vec![];
    for (operation, _) in mcp::secret_fields::CREDENTIAL_FIELDS {
        let (method, template, _) = matcher
            .operations()
            .find(|(_, _, id)| id == operation)
            .unwrap_or_else(|| panic!("{operation} is not in the bundle"));
        let path = template
            .split('/')
            .map(|s| if s.starts_with('{') { "x" } else { s })
            .collect::<Vec<_>>()
            .join("/");
        script.push(call(method, &path, json!({})));
    }
    let n = script.len();
    let result = h.execute_destructive(calls(script)).await;
    let answers = answers(&result);
    assert_eq!(answers.len(), n);
    for entry in &answers {
        assert_eq!(refusal(entry), "secrets_unavailable", "{entry}");
    }
    assert!(h.seen().is_empty(), "{:?}", h.seen());
}

// ---- headers ----

#[tokio::test]
async fn sandbox_headers_are_filtered() {
    let h = Harness::start().await;
    let result = h
        .execute(calls(vec![json!({
            "method": "GET",
            "path": "/v1/projects",
            "headers": {
                "authorization": "Bearer stolen",
                "Cookie": "session=1",
                "host": "evil.example",
                "x-forwarded-for": "1.2.3.4",
                "x-forwarded-host": "evil.example",
                "x-chelabase-studio": "1",
                "x-chelabase-anything": "1",
                "origin": "https://evil.example",
                "chelabase-identity": "forged",
                "x-request-id": "forged-id",
                "accept": "application/json",
                "x-upsert": "true"
            }
        })]))
        .await;
    assert_eq!(answer(&answers(&result)[0])["status"], 200);
    let seen = h.seen();
    assert_eq!(seen.len(), 1);
    let s = &seen[0];
    assert!(s.header("authorization").unwrap().starts_with("Bearer ey"));
    assert_eq!(s.headers.get_all("authorization").iter().count(), 1);
    assert_eq!(s.header("host"), Some(API_HOST));
    for name in [
        "cookie",
        "x-forwarded-for",
        "x-forwarded-host",
        "x-chelabase-studio",
        "x-chelabase-anything",
        "origin",
        "chelabase-identity",
    ] {
        assert_eq!(s.header(name), None, "{name}");
    }
    assert!(s.header("x-request-id").unwrap().starts_with("req-abc."));
    assert_eq!(s.header("accept"), Some("application/json"));
    assert_eq!(s.header("x-upsert"), Some("true"));
}

#[tokio::test]
async fn every_forwarded_call_carries_the_via_mcp_marker() {
    let h = Harness::start().await;
    let result = h
        .execute(calls(vec![
            get("/v1/projects"),
            json!({"method": "GET", "path": "/v1/projects", "headers": {"chelabase-via-mcp": ""}}),
            json!({"method": "GET", "path": "/v1/projects", "headers": {"Chelabase-Via-Mcp": "0"}}),
        ]))
        .await;
    assert_eq!(answers(&result).len(), 3);
    let seen = h.seen();
    assert_eq!(seen.len(), 3);
    for s in &seen {
        let values: Vec<_> = s.headers.get_all("chelabase-via-mcp").iter().collect();
        assert_eq!(values, ["1"]);
    }
}

#[tokio::test]
async fn response_headers_are_filtered() {
    let h = Harness::start().await;
    let token = Arc::new(Mutex::new(String::new()));
    let seen_token = token.clone();
    h.respond(move |seen| {
        // The upstream echoes the bearer in a header: it must not come back.
        *seen_token.lock().unwrap() = seen.header("authorization").unwrap().to_owned();
        let mut reply = Reply::json(429, json!({"error": {"code": "rate_limited"}}));
        reply.headers.extend([
            ("retry-after", "3".to_owned()),
            ("x-request-id", "up-1".to_owned()),
            ("set-cookie", "s=1".to_owned()),
            ("www-authenticate", "Bearer".to_owned()),
            (
                "x-echo-authorization",
                seen.header("authorization").unwrap().to_owned(),
            ),
            ("location", "https://elsewhere.example".to_owned()),
        ]);
        reply
    });
    let result = h.execute(calls(vec![get("/v1/projects")])).await;
    let answers = answers(&result);
    let a = answer(&answers[0]);
    assert_eq!(a["status"], 429);
    assert_eq!(
        a["headers"],
        json!({"content-type": "application/json", "retry-after": "3", "x-request-id": "up-1"})
    );
    let bearer = token.lock().unwrap().clone();
    let raw_token = bearer.strip_prefix("Bearer ").unwrap();
    assert!(!result.to_string().contains(raw_token));
    assert!(!all_logs().contains(raw_token));
}

#[tokio::test]
async fn an_answer_that_echoes_the_token_is_refused() {
    let h = Harness::start().await;
    h.respond(|seen| {
        let bearer = seen.header("authorization").unwrap().to_owned();
        Reply::json(200, json!({ "echo": bearer }))
    });
    let result = h.execute(calls(vec![get("/v1/projects")])).await;
    let answers = answers(&result);
    assert_eq!(refusal(&answers[0]), "secrets_unavailable");
    assert!(!result.to_string().contains("Bearer ey"));
}

// ---- runs ----

#[tokio::test]
async fn unknown_exec_id_is_404() {
    let h = Harness::start().await;
    // An id no run has.
    let resp = h
        .http
        .post(format!(
            "{}/internal/exec/{}/request",
            h.internal,
            "ab".repeat(32)
        ))
        .json(&get("/v1/projects"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // The id of a run that has ended.
    let result = h.execute(calls(vec![])).await;
    assert_eq!(answers(&result).len(), 0);
    let ended = h.runtime.exec_ids.lock().unwrap()[0].clone();
    assert!(h.registry.is_empty());
    let resp = h
        .http
        .post(format!("{}/internal/exec/{ended}/request", h.internal))
        .json(&get("/v1/projects"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // A live run naming another run's id (the stub sends it).
    let result = h
        .execute(json!({"calls": [get("/v1/projects")], "exec_id": ended}))
        .await;
    assert_eq!(answers(&result)[0]["http"], 404);
    assert!(h.seen().is_empty());
}

#[tokio::test]
async fn search_runs_cannot_call_the_host() {
    let h = Harness::start().await;
    let resp = h
        .post_tool(
            &h.token("member-1"),
            "search",
            json!({"code": calls(vec![get("/v1/projects")]).to_string()}),
            false,
        )
        .await;
    let v: Value = resp.json().await.unwrap();
    let answers = answers(&v["result"]);
    assert_eq!(answers[0]["http"], 404, "{answers:?}");
    assert!(h.seen().is_empty());
}

#[tokio::test]
async fn call_cap() {
    let h = Harness::start().await;
    let script = calls((0..51).map(|_| get("/v1/projects")).collect());
    let result = h.execute(script).await;
    let answers = answers(&result);
    for entry in &answers[..50] {
        assert_eq!(answer(entry)["status"], 200);
    }
    assert_eq!(refusal(&answers[50]), "too_many_calls");
    assert_eq!(h.seen().len(), 50);
}

#[tokio::test]
async fn response_cap() {
    let h = Harness::with(Options {
        config: HostConfig {
            response_max_bytes: 1024,
            ..HostConfig::default()
        },
        ..Options::default()
    })
    .await;
    h.respond(|seen| match seen.uri.path() {
        "/v1/projects" => Reply::json(200, json!({"pad": "x".repeat(2000)})),
        _ => Reply::json(200, json!({"pad": "x".repeat(900)})),
    });
    let result = h
        .execute(calls(vec![get("/v1/projects"), get("/v1/teams")]))
        .await;
    let answers = answers(&result);
    assert_eq!(refusal(&answers[0]), "response_too_large");
    assert_eq!(answer(&answers[1])["status"], 200);
}

#[tokio::test]
async fn binary_body_is_refused() {
    let h = Harness::start().await;
    h.respond(|seen| match seen.uri.path() {
        "/v1/projects" => Reply {
            headers: vec![("content-type", "application/octet-stream".into())],
            body: vec![0xff, 0xfe, 0x00, 0x81],
            ..Reply::json(200, json!({}))
        },
        _ => Reply {
            headers: vec![("content-type", "application/x-ndjson".into())],
            body: b"{\"a\":1}\n{\"b\":2}\n".to_vec(),
            ..Reply::json(200, json!({}))
        },
    });
    let result = h
        .execute(calls(vec![get("/v1/projects"), get("/v1/teams")]))
        .await;
    let answers = answers(&result);
    assert_eq!(refusal(&answers[0]), "binary_body");
    // NDJSON stays text.
    assert_eq!(answer(&answers[1])["body"], "{\"a\":1}\n{\"b\":2}\n");
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let h = Harness::start().await;
    h.respond(|seen| match seen.uri.path() {
        "/v1/projects" => Reply {
            status: 302,
            headers: vec![("location", "/v1/teams".into())],
            body: vec![],
            ..Reply::json(200, json!({}))
        },
        _ => Reply::json(200, json!({"followed": true})),
    });
    let result = h.execute(calls(vec![get("/v1/projects")])).await;
    let answers = answers(&result);
    assert_eq!(answer(&answers[0])["status"], 302);
    assert_eq!(answer(&answers[0])["ok"], false);
    assert_eq!(h.seen().len(), 1);
}

#[tokio::test]
async fn tokens_do_not_cross_executions() {
    let h = Arc::new(Harness::start().await);
    h.respond(|_| Reply {
        delay: Duration::from_millis(15),
        ..Reply::json(200, json!({}))
    });
    let run = |member: &'static str, prefix: &'static str| {
        let h = h.clone();
        tokio::spawn(async move {
            let script = calls(
                (0..8)
                    .map(|i| get(&format!("/v1/databases/{prefix}_{i}")))
                    .collect(),
            );
            h.execute_as(member, script, false, false).await
        })
    };
    let (a, b) = tokio::join!(run("member-a", "a"), run("member-b", "b"));
    assert_eq!(answers(&a.unwrap()).len(), 8);
    assert_eq!(answers(&b.unwrap()).len(), 8);
    let seen = h.seen();
    assert_eq!(seen.len(), 16);
    for s in &seen {
        let bearer = s.header("authorization").unwrap();
        let payload = bearer
            .strip_prefix("Bearer ")
            .unwrap()
            .split('.')
            .nth(1)
            .unwrap();
        let claims = String::from_utf8(base64_decode(payload)).unwrap();
        let member = if s.uri.path().starts_with("/v1/databases/a_") {
            "member-a"
        } else {
            "member-b"
        };
        assert!(
            claims.contains(&format!("\"sub\":\"{member}\"")),
            "{} carried {claims}",
            s.uri
        );
    }
}

fn base64_decode(text: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = vec![];
    let mut buf = 0u32;
    let mut bits = 0;
    for c in text.bytes().filter(|c| *c != b'=') {
        let v = ALPHABET.iter().position(|a| *a == c).unwrap() as u32;
        buf = (buf << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    out
}

#[tokio::test]
async fn a_short_lived_token_is_refused_up_front() {
    let h = Harness::with(Options {
        timeout: Duration::from_secs(60),
        ..Options::default()
    })
    .await;
    // 65 s left: over the 60 s floor, under the 60 s run plus 10 s.
    let resp = h
        .post_tool(
            &h.token_for("member-1", 65),
            "execute",
            json!({"code": calls(vec![get("/v1/projects")]).to_string()}),
            false,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert!(h.runtime.exec_ids.lock().unwrap().is_empty(), "nothing ran");
    assert!(h.seen().is_empty());
    // Enough time left: it runs.
    let resp = h
        .post_tool(
            &h.token_for("member-1", 75),
            "execute",
            json!({"code": calls(vec![]).to_string()}),
            false,
        )
        .await;
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn audit_lines_have_no_bodies() {
    let h = Harness::start().await;
    h.respond(|_| Reply::json(200, json!({"secret": "answer-marker-77"})));
    let result = h
        .execute_destructive(calls(vec![
            call(
                "POST",
                &format!("{BRANCH}/sql"),
                json!({"sql": "select 'body-marker-42'"}),
            ),
            get("/v1/databases/db_secret_id_value"),
            get("/v1/nope/marker-path-99"),
        ]))
        .await;
    assert_eq!(answers(&result).len(), 3);
    let exec_id = h.runtime.exec_ids.lock().unwrap()[0].clone();
    let logs = all_logs();
    let lines: Vec<&str> = logs
        .lines()
        .filter(|l| l.contains("api call") && l.contains(&exec_id))
        .collect();
    assert_eq!(lines.len(), 3, "{logs}");
    assert!(lines[0].contains("operation=\"runSql\""), "{}", lines[0]);
    assert!(lines[0].contains("class=\"destructive\""), "{}", lines[0]);
    assert!(lines[0].contains("status=200"), "{}", lines[0]);
    assert!(lines[0].contains("member=\"member-1\""), "{}", lines[0]);
    assert!(
        lines[0].contains("/v1/databases/{id}/branches/{branch_id}/sql"),
        "{}",
        lines[0]
    );
    assert!(lines[2].contains("operation_not_allowed"), "{}", lines[2]);
    let token = h.token("member-1");
    let token_body = token.split('.').nth(2).unwrap();
    for line in &lines {
        for marker in [
            "body-marker-42",
            "answer-marker-77",
            "marker-path-99",
            "db_secret_id_value",
            "Bearer",
        ] {
            assert!(!line.contains(marker), "{marker} in {line}");
        }
        assert!(!line.contains(token_body));
    }
}

/// A body that isn't a request envelope still writes its audit line.
#[tokio::test]
async fn a_malformed_envelope_is_audited() {
    let h = Harness::start().await;
    let result = h
        .execute(calls(vec![
            json!({"raw": "not json"}),
            json!({"method": "GET", "path": "/v1/projects", "extra": "marker-extra-31"}),
        ]))
        .await;
    let answers = answers(&result);
    assert_eq!(answers[0]["http"], 400, "{result}");
    assert_eq!(answers[1]["http"], 400, "{result}");
    assert!(h.seen().is_empty());
    let exec_id = h.runtime.exec_ids.lock().unwrap()[0].clone();
    let logs = all_logs();
    let lines: Vec<&str> = logs
        .lines()
        .filter(|l| l.contains("api call") && l.contains(&exec_id))
        .collect();
    assert_eq!(lines.len(), 2, "{logs}");
    for line in lines {
        assert!(line.contains("refusal=\"invalid_request\""), "{line}");
        assert!(line.contains("status=400"), "{line}");
        assert!(!line.contains("marker-extra-31"), "{line}");
    }
}

/// An integer outside i64/u64 is refused, not forwarded (maybe rounded).
#[tokio::test]
async fn a_number_that_would_lose_precision_is_refused() {
    let h = Harness::start().await;
    let raw = |n: &str| {
        json!({"raw": format!(
            r#"{{"method":"POST","path":"{BRANCH}/sql","body":{{"sql":"select 1","params":[{n}]}}}}"#
        )})
    };
    let result = h
        .execute_destructive(calls(vec![
            raw("18446744073709551616"),
            raw("-9223372036854775809"),
            raw("18446744073709551615"),
        ]))
        .await;
    let answers = answers(&result);
    for entry in &answers[..2] {
        assert_eq!(refusal(entry), "invalid_request", "{entry}");
    }
    assert_eq!(answer(&answers[2])["status"], 200, "{}", answers[2]);
    let seen = h.seen();
    assert_eq!(seen.len(), 1, "only the exact number went out: {seen:?}");
    assert_eq!(seen[0].json()["params"], json!([18446744073709551615u64]));
}

// ---- deadlines ----

#[tokio::test]
async fn a_timeout_lists_the_calls_already_made() {
    let timeout = Duration::from_millis(400);
    let h = Harness::with(Options {
        timeout,
        ..Options::default()
    })
    .await;
    h.respond(|seen| match seen.uri.path() {
        "/v1/projects" => Reply::json(200, json!([])),
        _ => Reply::hang(),
    });
    let result = h
        .execute(json!({
            "calls": [
                get("/v1/projects"),
                call("POST", &format!("{BRANCH}/sql"), json!({"sql": "select 1; select 2"})),
            ],
            "then": "hang",
        }))
        .await;
    let error = tool_error(&result);
    assert_eq!(error["code"], "timeout", "{result}");
    assert_eq!(
        error["calls"],
        json!([
            {"operation": "listProjects", "status": 200},
            {"operation": "runSql", "status": "unknown"},
        ])
    );
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("may have been applied"), "{message}");
    // The cut call was cancelled at the deadline, not left running.
    assert!(
        h.gateway.cut.load(Ordering::SeqCst),
        "the upstream call is still open"
    );
    assert!(h.registry.is_empty());
}

/// The host's deadline comes before the runner's, so the code can get the cut call's 504 and
/// return a value: the tool result is still the timeout error, listing the cut call.
#[tokio::test]
async fn audit_a_cut_write_is_not_silent_when_the_code_returns() {
    let h = Harness::with(Options {
        timeout: Duration::from_millis(400),
        ..Options::default()
    })
    .await;
    h.respond(|_| Reply::hang());
    let result = h
        .execute_destructive(calls(vec![call(
            "POST",
            &format!("{BRANCH}/sql"),
            json!({"sql": "insert into t values (1); insert into t values (2)"}),
        )]))
        .await;
    assert_eq!(h.seen().len(), 1, "the write reached the API");
    let error = tool_error(&result);
    assert_eq!(error["code"], "timeout", "{result}");
    assert_eq!(
        error["calls"],
        json!([{"operation": "runSql", "status": "unknown"}])
    );
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("may have been applied"), "{message}");
}

/// A gateway that answers `200` with its headers and then never finishes the body (a SQL
/// run's NDJSON starts before the script ends): its address.
async fn headers_then_hang() -> std::net::SocketAddr {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 64 * 1024];
                let _ = socket.read(&mut buf).await;
                let head = "HTTP/1.1 200 OK\r\ncontent-type: application/x-ndjson\r\n\
                            content-length: 1000\r\n\r\n{\"statement\":1}\n";
                let _ = socket.write_all(head.as_bytes()).await;
                // Hold the connection open until the host drops it.
                let _ = socket.read(&mut buf).await;
            });
        }
    });
    addr
}

/// A call cut after its status came back but before its body ended is cut all the same.
#[tokio::test]
async fn a_call_cut_mid_answer_is_reported() {
    let addr = headers_then_hang().await;
    let h = Harness::with(Options {
        timeout: Duration::from_millis(400),
        config: HostConfig {
            api_url: Some(format!("http://{addr}")),
            ..HostConfig::default()
        },
    })
    .await;
    let result = h
        .execute_destructive(calls(vec![call(
            "POST",
            &format!("{BRANCH}/sql"),
            json!({"sql": "insert into t values (1); insert into t values (2)"}),
        )]))
        .await;
    let error = tool_error(&result);
    assert_eq!(error["code"], "timeout", "{result}");
    assert_eq!(
        error["calls"],
        json!([{"operation": "runSql", "status": "unknown"}])
    );
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("may have been applied"), "{message}");
}

/// A run with a cut call is a timeout whatever the code returns, and lists every call made,
/// answered or cut; the call after the deadline never leaves the host.
#[tokio::test]
async fn a_cut_run_lists_every_call_when_the_code_returns() {
    let h = Harness::with(Options {
        timeout: Duration::from_millis(400),
        ..Options::default()
    })
    .await;
    h.respond(|seen| {
        let mut reply = Reply::json(200, json!([]));
        if seen.uri.path() == "/v1/teams" {
            reply.delay = Duration::from_millis(600);
        }
        reply
    });
    let result = h
        .execute(calls(vec![
            get("/v1/projects"),
            get("/v1/teams"),
            get("/v1/projects"),
        ]))
        .await;
    let error = tool_error(&result);
    assert_eq!(error["code"], "timeout", "{result}");
    assert_eq!(
        error["calls"],
        json!([
            {"operation": "listProjects", "status": 200},
            {"operation": "listTeams", "status": "unknown"},
        ])
    );
    assert_eq!(h.seen().len(), 2);
}

#[tokio::test]
async fn no_call_is_forwarded_after_the_deadline() {
    let timeout = Duration::from_millis(400);
    let h = Harness::with(Options {
        timeout,
        ..Options::default()
    })
    .await;
    h.respond(|seen| match seen.uri.path() {
        "/v1/teams" => Reply::hang(),
        _ => Reply::json(200, json!([])),
    });
    let started = Instant::now();
    let result = h
        .execute(json!({
            "calls": [get("/v1/teams"), get("/v1/projects"), get("/v1/projects")],
            "then": "hang",
        }))
        .await;
    assert_eq!(tool_error(&result)["code"], "timeout");
    // The hanging call was cut at the deadline; the later ones never left the host.
    eventually("the cut", || h.gateway.cut.load(Ordering::SeqCst)).await;
    let seen = h.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(seen[0].uri.path(), "/v1/teams");
    assert!(started.elapsed() >= timeout);
}
