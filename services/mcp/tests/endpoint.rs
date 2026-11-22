//! MCP core m4: the `/mcp` endpoint over real HTTP, with a sandbox double.

use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use chelabase_auth::app_token::{AppClaims, AppTokenSigner, app_audience};
use chelabase_auth::member_token::{
    MEMBER_AUDIENCE, MemberClaims, MemberTokenSigner, MemberTokenVerifier,
};
use mcp::app::{Settings, router};
use mcp::sandbox::{ErrorCode, Kind, Outcome, Run, Sandbox};
use reqwest::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

const ISSUER: &str = "http://auth.test";
const API_BASE: &str = "https://api.example.test";
const NEW: &str = "2026-07-28";
const OLD: &str = "2025-11-25";
const CHALLENGE: &str = r#"Bearer resource_metadata="https://api.example.test/.well-known/oauth-protected-resource/mcp""#;

// ---- logs: every line the service writes, at the binary's default filter ----

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

fn log_text() -> String {
    String::from_utf8_lossy(&logs().0.lock().unwrap()).into_owned()
}

fn audit_lines() -> Vec<String> {
    log_text()
        .lines()
        .filter(|l| l.contains("mcp::audit"))
        .map(str::to_owned)
        .collect()
}

// ---- the sandbox double ----

struct Double {
    runs: Mutex<Vec<Run>>,
    answer: Outcome,
}

#[async_trait]
impl Sandbox for Double {
    async fn run(&self, run: Run) -> Outcome {
        self.runs.lock().unwrap().push(run);
        self.answer.clone()
    }
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
    private: String,
    public: String,
    sandbox: Arc<Double>,
}

impl Harness {
    async fn start() -> Self {
        Self::with(Outcome::Value(json!({"ok": true})), vec![]).await
    }

    async fn with(answer: Outcome, allowed_origins: Vec<String>) -> Self {
        logs();
        let (private, public) = chelabase_auth::internal::generate_keypair_pem();
        let signer =
            MemberTokenSigner::from_pem(private.as_bytes(), public.as_bytes(), ISSUER).unwrap();
        let verifier = MemberTokenVerifier::from_pem(public.as_bytes(), ISSUER).unwrap();
        let sandbox = Arc::new(Double {
            runs: Mutex::new(vec![]),
            answer,
        });
        let settings = Settings {
            api_base: API_BASE.into(),
            allowed_origins,
            log_code: false,
        };
        let app = router(settings, verifier, sandbox.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Harness {
            url: format!("http://{addr}"),
            http: chelabase_http::client_builder().build().unwrap(),
            signer,
            private,
            public,
            sandbox,
        }
    }

    fn claims(&self, exp_in: i64) -> MemberClaims {
        let iat = now() - 10;
        MemberClaims {
            iss: ISSUER.into(),
            aud: MEMBER_AUDIENCE.into(),
            sub: "member-1".into(),
            tid: Uuid::from_u128(7),
            sid: "sess".into(),
            email: "m@example.com".into(),
            name: "M".into(),
            iat,
            exp: (now() as i64 + exp_in) as u64,
        }
    }

    fn token(&self) -> String {
        self.signer.sign(&self.claims(300)).unwrap()
    }

    fn runs(&self) -> Vec<Run> {
        self.sandbox.runs.lock().unwrap().clone()
    }

    /// A JSON-RPC request at `version`, with the per-request metadata 2026-07-28 needs.
    fn body(version: &str, id: u64, method: &str, mut params: Value) -> Value {
        if version == NEW {
            params["_meta"] = json!({
                "io.modelcontextprotocol/protocolVersion": NEW,
                "io.modelcontextprotocol/clientInfo": {"name": "test", "version": "1"},
                "io.modelcontextprotocol/clientCapabilities": {}
            });
        }
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
    }

    fn request(&self, token: Option<&str>, version: &str, body: &Value) -> reqwest::RequestBuilder {
        let mut req = self
            .http
            .post(format!("{}/mcp", self.url))
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", version)
            .body(body.to_string());
        // 2026-07-28 mirrors the method (and a tool call's name) in headers (SEP-2243).
        if version == NEW {
            req = req.header("mcp-method", body["method"].as_str().unwrap());
            if let Some(name) = body["params"]["name"].as_str() {
                req = req.header("mcp-name", name);
            }
        }
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        req
    }

    async fn rpc_as(
        &self,
        token: Option<&str>,
        version: &str,
        method: &str,
        params: Value,
    ) -> (StatusCode, Value) {
        let body = Self::body(version, 1, method, params);
        let resp = self.request(token, version, &body).send().await.unwrap();
        let status = resp.status();
        let text = resp.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn rpc(&self, method: &str, params: Value) -> (StatusCode, Value) {
        self.rpc_as(Some(&self.token()), NEW, method, params).await
    }

    async fn call(&self, tool: &str, arguments: Value) -> (StatusCode, Value) {
        self.rpc("tools/call", json!({"name": tool, "arguments": arguments}))
            .await
    }

    /// The tool result's JSON text body.
    fn text_of(result: &Value) -> Value {
        let text = result["result"]["content"][0]["text"]
            .as_str()
            .expect("a text result");
        serde_json::from_str(text).unwrap()
    }

    async fn assert_refused(&self, token: Option<&str>) {
        for method in ["initialize", "tools/list", "tools/call"] {
            let params = match method {
                "initialize" => {
                    json!({"protocolVersion": OLD, "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}})
                }
                "tools/call" => json!({"name": "search", "arguments": {"code": "async () => 1"}}),
                _ => json!({}),
            };
            let version = if method == "initialize" { OLD } else { NEW };
            let body = Self::body(version, 1, method, params);
            let resp = self.request(token, version, &body).send().await.unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{method}");
            let challenge = resp
                .headers()
                .get("www-authenticate")
                .expect("a challenge")
                .to_str()
                .unwrap();
            assert_eq!(challenge, CHALLENGE, "{method}");
            let text = resp.text().await.unwrap();
            if let Some(t) = token {
                assert!(!text.contains(t), "the token is echoed: {text}");
            }
        }
        assert!(self.runs().is_empty(), "a tool ran");
        if let Some(t) = token {
            assert!(!log_text().contains(t), "the token reached a log line");
        }
    }
}

fn tool<'a>(list: &'a Value, name: &str) -> &'a Value {
    list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == name)
        .unwrap()
}

// ---- acceptance ----

#[tokio::test]
async fn lists_exactly_two_tools() {
    let h = Harness::start().await;
    for version in [NEW, OLD] {
        let token = h.token();
        let (status, list) = h
            .rpc_as(Some(&token), version, "tools/list", json!({}))
            .await;
        assert_eq!(status, StatusCode::OK, "{version}: {list}");
        let names: Vec<&str> = list["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["search", "execute"], "{version}");

        let search = tool(&list, "search");
        assert_eq!(
            search["inputSchema"],
            json!({"type": "object", "properties": {"code": {"type": "string"}}, "required": ["code"], "additionalProperties": false})
        );
        assert_eq!(search["annotations"]["readOnlyHint"], true);
        let desc = search["description"].as_str().unwrap();
        assert!(desc.contains("x-class") && desc.contains("spec"), "{desc}");

        let execute = tool(&list, "execute");
        assert_eq!(
            execute["inputSchema"],
            json!({
                "type": "object",
                "properties": {"code": {"type": "string"}, "destructive": {"type": "boolean"}},
                "required": ["code"],
                "additionalProperties": false
            })
        );
        assert_eq!(execute["annotations"]["destructiveHint"], true);
        assert_eq!(execute["annotations"]["openWorldHint"], false);
        assert_ne!(execute["annotations"]["readOnlyHint"], true);
        let desc = execute["description"].as_str().unwrap();
        assert!(
            desc.contains("chelabase.request") && desc.contains("destructive"),
            "{desc}"
        );
    }
}

#[tokio::test]
async fn initialize_reports_the_spec_hash() {
    let h = Harness::start().await;
    let (status, init) = h
        .rpc_as(
            Some(&h.token()),
            OLD,
            "initialize",
            json!({"protocolVersion": OLD, "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{init}");
    assert_eq!(init["result"]["protocolVersion"], OLD);
    let bundle: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../openapi/mcp.json"
    )))
    .unwrap();
    let hash = bundle["info"]["x-spec-hash"].as_str().unwrap();
    let instructions = init["result"]["instructions"].as_str().unwrap();
    assert!(instructions.contains(hash), "{instructions}");
    assert!(!instructions.contains("read-only"), "{instructions}");
}

#[tokio::test]
async fn refuses_a_missing_bearer() {
    let h = Harness::start().await;
    h.assert_refused(None).await;
    h.assert_refused(Some("not-a-jwt")).await;
}

#[tokio::test]
async fn refuses_an_expired_token() {
    let h = Harness::start().await;
    let token = h.signer.sign(&h.claims(-120)).unwrap();
    h.assert_refused(Some(&token)).await;
}

#[tokio::test]
async fn refuses_a_token_about_to_expire() {
    let h = Harness::start().await;
    // Valid for the verifier (and its leeway), but under the 60 s a tool run may need.
    let token = h.signer.sign(&h.claims(45)).unwrap();
    h.assert_refused(Some(&token)).await;
}

#[tokio::test]
async fn refuses_an_app_user_token() {
    let h = Harness::start().await;
    // Signed with the member key itself, so only the claims tell it apart.
    let signer = AppTokenSigner::from_pem(h.private.as_bytes(), h.public.as_bytes()).unwrap();
    let project = Uuid::from_u128(9);
    let token = signer
        .sign(&AppClaims {
            iss: ISSUER.into(),
            aud: app_audience(project),
            sub: Uuid::from_u128(1),
            sid: Uuid::from_u128(2),
            project,
            email: "u@example.com".into(),
            email_verified: true,
            role: "authenticated".into(),
            iat: now() - 10,
            exp: now() + 300,
        })
        .unwrap();
    h.assert_refused(Some(&token)).await;
}

#[tokio::test]
async fn refuses_a_token_from_another_key() {
    let h = Harness::start().await;
    let (private, public) = chelabase_auth::internal::generate_keypair_pem();
    let other = MemberTokenSigner::from_pem(private.as_bytes(), public.as_bytes(), ISSUER).unwrap();
    let token = other.sign(&h.claims(300)).unwrap();
    h.assert_refused(Some(&token)).await;
}

#[tokio::test]
async fn metadata_names_the_configured_origin() {
    let h = Harness::start().await;
    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/mcp",
    ] {
        let resp = h
            .http
            .get(format!("{}{path}", h.url))
            .header("host", "evil.example")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
        let doc: Value = resp.json().await.unwrap();
        assert_eq!(
            doc,
            json!({
                "resource": "https://api.example.test/mcp",
                "authorization_servers": ["https://api.example.test"],
                "bearer_methods_supported": ["header"]
            }),
            "{path}"
        );
    }
}

#[tokio::test]
async fn search_reaches_the_sandbox() {
    let h = Harness::start().await;
    let code = "async () => spec.paths['/v1/projects'] /* distinctive-code-7f3a */";
    let token = h.token();
    let (status, result) = h
        .rpc_as(
            Some(&token),
            NEW,
            "tools/call",
            json!({"name": "search", "arguments": {"code": code}}),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{result}");
    let runs = h.runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].kind, Kind::Search);
    assert_eq!(runs[0].code, code);
    assert!(!runs[0].destructive);
    assert_eq!(runs[0].caller.member(), "member-1");
    assert_eq!(runs[0].caller.tenant(), Uuid::from_u128(7));
    assert!(!runs[0].caller.read_only());
    assert_ne!(result["result"]["isError"], true, "{result}");
    assert_eq!(Harness::text_of(&result), json!({"ok": true}));
    assert_eq!(result["result"]["structuredContent"], json!({"ok": true}));

    let audit = audit_lines();
    assert_eq!(audit.len(), 1, "{audit:?}");
    let line = &audit[0];
    for field in [
        "request_id=",
        "member=\"member-1\"",
        "tenant=00000000-0000-0000-0000-000000000007",
        "credential=\"member\"",
        "tool=\"search\"",
        "code_sha256=",
        &format!("code_bytes={}", code.len()),
        "destructive=false",
        "read_only=false",
        "duration_ms=",
        "outcome=\"ok\"",
        "result_bytes=",
    ] {
        assert!(line.contains(field), "{field} missing from {line}");
    }
    let logs = log_text();
    assert!(
        !logs.contains("distinctive-code-7f3a"),
        "the code reached a log line: {logs}"
    );
    assert!(!logs.contains(&token), "the token reached a log line");
}

#[tokio::test]
async fn a_sandbox_error_is_a_tool_error() {
    for code in [ErrorCode::Timeout, ErrorCode::Threw] {
        let h = Harness::with(
            Outcome::Error {
                code,
                message: "nope".into(),
            },
            vec![],
        )
        .await;
        let (status, result) = h.call("execute", json!({"code": "async () => 1"})).await;
        assert_eq!(status, StatusCode::OK, "{result}");
        assert_eq!(result["result"]["isError"], true, "{result}");
        assert_eq!(
            Harness::text_of(&result),
            json!({"error": {"code": code.as_str(), "message": "nope"}})
        );
    }
}

#[tokio::test]
async fn unknown_tool_is_invalid_params() {
    let h = Harness::start().await;
    let (_, result) = h
        .call("drop_everything", json!({"code": "async () => 1"}))
        .await;
    assert_eq!(result["error"]["code"], -32602, "{result}");
    assert!(h.runs().is_empty());
}

#[tokio::test]
async fn extra_argument_is_invalid_params() {
    let h = Harness::start().await;
    for arguments in [
        json!({"code": "async () => 1", "destructve": true}),
        json!({}),
        json!({"code": "async () => 1", "destructive": "yes"}),
        json!({"code": 5}),
    ] {
        let (_, result) = h.call("execute", arguments.clone()).await;
        assert_eq!(result["error"]["code"], -32602, "{arguments}: {result}");
    }
    let (_, result) = h
        .call(
            "search",
            json!({"code": "async () => 1", "destructive": true}),
        )
        .await;
    assert_eq!(result["error"]["code"], -32602, "{result}");
    assert!(h.runs().is_empty());
}

#[tokio::test]
async fn get_is_405() {
    let h = Harness::start().await;
    for method in [reqwest::Method::GET, reqwest::Method::DELETE] {
        let resp = h
            .http
            .request(method.clone(), format!("{}/mcp", h.url))
            .bearer_auth(h.token())
            .header("accept", "text/event-stream")
            .header("mcp-protocol-version", NEW)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED, "{method}");
    }
}

#[tokio::test]
async fn unsupported_revision_is_400() {
    let h = Harness::start().await;
    for version in ["2025-06-18", "2025-03-26", "1999-01-01"] {
        let token = h.token();
        let (status, _) = h
            .rpc_as(Some(&token), version, "tools/list", json!({}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{version}");
    }
}

#[tokio::test]
async fn a_foreign_origin_is_403() {
    let h = Harness::with(
        Outcome::Value(json!(1)),
        vec!["https://allowed.example".into()],
    )
    .await;
    let body = Harness::body(NEW, 1, "tools/list", json!({}));
    let resp = h
        .request(Some(&h.token()), NEW, &body)
        .header("origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = h
        .request(Some(&h.token()), NEW, &body)
        .header("origin", "https://allowed.example")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The default allow-list is empty: every Origin is refused.
    let h = Harness::start().await;
    let resp = h
        .request(Some(&h.token()), NEW, &body)
        .header("origin", "https://allowed.example")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn oversized_code_is_refused() {
    let h = Harness::start().await;
    let code = format!("async () => '{}'", "x".repeat(64 * 1024));
    let (status, result) = h.call("search", json!({"code": code})).await;
    assert_eq!(status, StatusCode::OK, "{result}");
    assert_eq!(result["result"]["isError"], true, "{result}");
    assert_eq!(Harness::text_of(&result)["error"]["code"], "invalid_code");
    assert!(h.runs().is_empty());
}

#[tokio::test]
async fn the_read_only_header_marks_the_context() {
    let h = Harness::start().await;
    let call = Harness::body(
        NEW,
        1,
        "tools/call",
        json!({"name": "execute", "arguments": {"code": "async () => 1"}}),
    );
    for (value, read_only) in [
        (Some("true"), true),
        (Some("false"), false),
        (Some("1"), false),
        (Some("TRUEISH"), false),
        (None, false),
    ] {
        let mut req = h.request(Some(&h.token()), NEW, &call);
        if let Some(v) = value {
            req = req.header("chelabase-mcp-read-only", v);
        }
        assert_eq!(req.send().await.unwrap().status(), StatusCode::OK);
        let run = h.runs().pop().unwrap();
        assert_eq!(run.caller.read_only(), read_only, "{value:?}");
    }
    let audit = audit_lines();
    assert!(audit[0].contains("read_only=true"), "{audit:?}");
    assert!(audit[1].contains("read_only=false"), "{audit:?}");
}

#[tokio::test]
async fn read_only_changes_the_execute_annotations() {
    let h = Harness::start().await;
    let body = Harness::body(NEW, 1, "tools/list", json!({}));
    let resp = h
        .request(Some(&h.token()), NEW, &body)
        .header("chelabase-mcp-read-only", "true")
        .send()
        .await
        .unwrap();
    let list: Value = resp.json().await.unwrap();
    let execute = tool(&list, "execute");
    assert_eq!(execute["annotations"]["readOnlyHint"], true, "{execute}");
    assert_eq!(
        execute["annotations"]["destructiveHint"], false,
        "{execute}"
    );
    assert!(
        execute["description"].as_str().unwrap().contains("refused"),
        "{execute}"
    );

    let init = Harness::body(
        OLD,
        1,
        "initialize",
        json!({"protocolVersion": OLD, "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}),
    );
    let resp = h
        .request(Some(&h.token()), OLD, &init)
        .header("chelabase-mcp-read-only", "true")
        .send()
        .await
        .unwrap();
    let init: Value = resp.json().await.unwrap();
    assert!(
        init["result"]["instructions"]
            .as_str()
            .unwrap()
            .contains("read-only"),
        "{init}"
    );
}
