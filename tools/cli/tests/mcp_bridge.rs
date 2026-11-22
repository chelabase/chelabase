//! `chelabase mcp` against a stub `/mcp` and token endpoint, with a temporary config directory.

// Test code: a plain client and a stub server (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::Engine;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// What the stub saw on `/mcp`.
#[derive(Clone, Debug)]
struct Seen {
    headers: HeaderMap,
    body: Value,
}

/// What the stub answers: status, content type and body.
type Answer = (u16, &'static str, String);
type Script = dyn Fn(&Seen, usize) -> Answer + Send + Sync;

struct Stub {
    seen: Mutex<Vec<Seen>>,
    mints: Mutex<usize>,
    script: Box<Script>,
}

fn jwt(tag: &str, left: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let b = |v: Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
    format!(
        "{}.{}.sig{tag}",
        b(json!({"alg": "none"})),
        b(json!({"exp": now + left, "tag": tag}))
    )
}

async fn mcp(State(s): State<Arc<Stub>>, headers: HeaderMap, body: String) -> Response {
    let seen = Seen {
        headers,
        body: serde_json::from_str(&body).unwrap(),
    };
    let n = {
        let mut all = s.seen.lock().unwrap();
        all.push(seen.clone());
        all.len()
    };
    let (status, ctype, text) = (s.script)(&seen, n);
    (
        StatusCode::from_u16(status).unwrap(),
        [(header::CONTENT_TYPE, ctype)],
        text,
    )
        .into_response()
}

async fn mint(State(s): State<Arc<Stub>>) -> Response {
    let mut n = s.mints.lock().unwrap();
    *n += 1;
    axum::Json(json!({"access_token": jwt(&format!("minted{n}"), 900)})).into_response()
}

struct Run {
    stdout: String,
    stderr: String,
    code: Option<i32>,
    stub: Arc<Stub>,
    kept: String,
}

fn ok(body: Value) -> Answer {
    (200, "application/json", body.to_string())
}

fn reply(seen: &Seen) -> Answer {
    ok(json!({"jsonrpc": "2.0", "id": seen.body["id"], "result": {}}))
}

/// Runs `chelabase mcp` on `input` against a stub scripted by `script`. `kept` is the token
/// stored with the login (`None`: not logged in at all).
async fn run_with(
    args: &[&str],
    input: &str,
    login: bool,
    script: impl Fn(&Seen, usize) -> Answer + Send + Sync + 'static,
) -> Run {
    let stub = Arc::new(Stub {
        seen: Mutex::default(),
        mints: Mutex::default(),
        script: Box::new(script),
    });
    let app = Router::new()
        .route("/mcp", post(mcp))
        .route("/auth/v1/token", post(mint))
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let dir = tempfile::tempdir().unwrap();
    let kept = jwt("kept", 3600);
    if login {
        let login = json!({
            "email": "a@example.com",
            "studio_origin": "http://studio.localhost",
            "session": "sess",
            "access_token": kept,
        });
        let creds: BTreeMap<_, _> = [(url.clone(), login)].into();
        std::fs::write(
            dir.path().join("credentials.json"),
            serde_json::to_vec(&creds).unwrap(),
        )
        .unwrap();
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_chelabase"))
        .arg("mcp")
        .args(["--url", &url])
        .args(args)
        .env("CHELABASE_CONFIG_DIR", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input.as_bytes()).await.unwrap();
    drop(stdin);
    let out = child.wait_with_output().await.unwrap();
    Run {
        stdout: String::from_utf8(out.stdout).unwrap(),
        stderr: String::from_utf8(out.stderr).unwrap(),
        code: out.status.code(),
        stub,
        kept,
    }
}

impl Run {
    fn lines(&self) -> Vec<Value> {
        self.stdout
            .lines()
            .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{l:?}: {e}")))
            .collect()
    }
    fn seen(&self) -> Vec<Seen> {
        self.stub.seen.lock().unwrap().clone()
    }
}

fn header<'a>(s: &'a Seen, name: &str) -> Option<&'a str> {
    s.headers.get(name).map(|v| v.to_str().unwrap())
}

const NEW: &str = "2026-07-28";
const OLD: &str = "2025-11-25";

fn line(v: Value) -> String {
    format!("{v}\n")
}

fn init(version: &str) -> String {
    line(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": version, "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}))
}

fn list(id: u64) -> String {
    line(json!({"jsonrpc": "2.0", "id": id, "method": "tools/list"}))
}

#[tokio::test]
async fn relays_a_request_and_its_answer() {
    let input = format!("{}{}", init(OLD), list(2));
    let run = run_with(&[], &input, true, |s, _| reply(s)).await;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let lines = run.lines();
    assert_eq!(lines.len(), 2, "{}", run.stdout);
    let ids: Vec<_> = lines.iter().map(|l| l["id"].as_u64().unwrap()).collect();
    assert!(ids.contains(&1) && ids.contains(&2), "{ids:?}");
    let seen = run.seen();
    assert_eq!(seen.len(), 2);
    for s in &seen {
        assert_eq!(
            header(s, "authorization"),
            Some(format!("Bearer {}", run.kept).as_str())
        );
        assert_eq!(header(s, "content-type"), Some("application/json"));
        assert_eq!(
            header(s, "accept"),
            Some("application/json, text/event-stream")
        );
    }
    let first = seen.iter().find(|s| s.body["id"] == 1).unwrap();
    assert_eq!(header(first, "mcp-protocol-version"), Some(OLD));
    assert_eq!(header(first, "mcp-method"), None);
}

#[tokio::test]
async fn splits_an_event_stream_into_lines() {
    let sse = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\n\n\
               event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n\n"
        .to_string();
    let run = run_with(&[], &list(2), true, move |_, _| {
        (200, "text/event-stream", sse.clone())
    })
    .await;
    let lines = run.lines();
    assert_eq!(lines.len(), 2, "{}", run.stdout);
    assert_eq!(lines[1]["id"], 2);
}

#[tokio::test]
async fn writes_nothing_for_a_202() {
    let input = line(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    let run = run_with(&[], &input, true, |_, _| (202, "text/plain", String::new())).await;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    assert_eq!(run.stdout, "");
    assert_eq!(run.seen().len(), 1);
}

#[tokio::test]
async fn retries_once_on_401() {
    let run = run_with(&[], &list(7), true, |s, n| {
        if n == 1 {
            (401, "application/json", "{}".into())
        } else {
            reply(s)
        }
    })
    .await;
    let lines = run.lines();
    assert_eq!(lines.len(), 1, "{}", run.stdout);
    assert_eq!(lines[0]["id"], 7);
    assert!(lines[0]["result"].is_object());
    assert_eq!(*run.stub.mints.lock().unwrap(), 1);
    let seen = run.seen();
    assert_eq!(seen.len(), 2);
    assert_ne!(
        header(&seen[0], "authorization"),
        header(&seen[1], "authorization")
    );
}

#[tokio::test]
async fn two_401s_become_a_json_rpc_error() {
    let run = run_with(&[], &list(7), true, |_, _| {
        (401, "application/json", "{}".into())
    })
    .await;
    let lines = run.lines();
    assert_eq!(lines.len(), 1, "{}", run.stdout);
    assert_eq!(lines[0]["id"], 7);
    assert!(lines[0]["error"]["code"].is_i64());
    assert!(
        lines[0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("chelabase login")
    );
    assert_eq!(run.seen().len(), 2);
    assert_eq!(*run.stub.mints.lock().unwrap(), 1);
}

#[tokio::test]
async fn no_login_exits_non_zero() {
    let run = run_with(&[], &list(1), false, |s, _| reply(s)).await;
    assert_ne!(run.code, Some(0));
    assert_eq!(run.stdout, "");
    assert!(run.stderr.contains("not signed in"), "{}", run.stderr);
    assert!(run.seen().is_empty());
}

#[tokio::test]
async fn a_non_json_line_is_a_parse_error() {
    let input = format!("this is not json\n{}", list(3));
    let run = run_with(&[], &input, true, |s, _| reply(s)).await;
    assert_eq!(run.code, Some(0), "{}", run.stderr);
    let lines = run.lines();
    assert_eq!(lines.len(), 2, "{}", run.stdout);
    let parse = lines.iter().find(|l| l["error"]["code"] == -32700).unwrap();
    assert_eq!(parse["id"], Value::Null);
    assert!(
        lines
            .iter()
            .any(|l| l["id"] == 3 && l["result"].is_object())
    );
    assert_eq!(run.seen().len(), 1);
}

#[tokio::test]
async fn finishes_in_flight_requests_at_eof() {
    let run = run_with(&[], &list(5), true, |s, _| {
        std::thread::sleep(std::time::Duration::from_millis(400));
        reply(s)
    })
    .await;
    let lines = run.lines();
    assert_eq!(lines.len(), 1, "{}", run.stdout);
    assert_eq!(lines[0]["id"], 5);
}

#[tokio::test]
async fn never_prints_a_token() {
    // Both a kept token and a minted one, and a failing run.
    let ok_run = run_with(&[], &list(1), true, |s, _| reply(s)).await;
    let failed = run_with(&[], &list(1), true, |_, _| {
        (401, "application/json", "{}".into())
    })
    .await;
    for run in [&ok_run, &failed] {
        for text in [&run.stdout, &run.stderr] {
            assert!(!text.contains(&run.kept), "{text}");
            assert!(!text.contains("minted"), "{text}");
            assert!(!text.contains("sig"), "{text}");
        }
    }
}

#[tokio::test]
async fn read_only_sends_the_header_on_every_request() {
    let input = format!("{}{}", init(OLD), list(2));
    let on = run_with(&["--read-only"], &input, true, |s, _| reply(s)).await;
    let seen = on.seen();
    assert_eq!(seen.len(), 2);
    for s in &seen {
        assert_eq!(header(s, "chelabase-mcp-read-only"), Some("true"));
    }
    let off = run_with(&[], &input, true, |s, _| reply(s)).await;
    for s in &off.seen() {
        assert_eq!(header(s, "chelabase-mcp-read-only"), None);
    }
}

#[tokio::test]
async fn the_new_revision_sends_method_and_name_headers() {
    let call = line(json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
        "params": {"name": "search", "arguments": {},
            "_meta": {"io.modelcontextprotocol/protocolVersion": NEW}}}));
    let run = run_with(&[], &call, true, |s, _| reply(s)).await;
    let seen = run.seen();
    assert_eq!(header(&seen[0], "mcp-protocol-version"), Some(NEW));
    assert_eq!(header(&seen[0], "mcp-method"), Some("tools/call"));
    assert_eq!(header(&seen[0], "mcp-name"), Some("search"));
}
