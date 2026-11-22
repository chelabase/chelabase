//! `chelabase mcp`: a stdio bridge to the instance's `/mcp` endpoint. Each stdin line is one
//! JSON-RPC message, posted with the signed-in member's token; each answer is written whole as
//! one stdout line. Only protocol messages go to stdout; diagnostics go to stderr, and a token
//! is never printed.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::task::JoinSet;

use crate::login;

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const INTERNAL_ERROR: i64 = -32603;
/// The first revision that mirrors the method and name in headers.
const HEADERS_FROM: &str = "2026-07-28";
const VERSION_META: &str = "io.modelcontextprotocol/protocolVersion";

/// The transport headers a message needs.
#[derive(Debug, PartialEq, Eq)]
struct Headers {
    protocol_version: Option<String>,
    method: Option<String>,
    name: Option<String>,
}

/// The headers for `msg`: its revision (from `initialize`, its `_meta`, else the one the server
/// negotiated), and from 2026-07-28 on its method and, for a call, the tool, prompt or resource.
fn headers_for(msg: &Value, negotiated: Option<&str>) -> Headers {
    let method = msg["method"].as_str();
    let params = &msg["params"];
    let version = if method == Some("initialize") {
        params["protocolVersion"].as_str()
    } else {
        params["_meta"][VERSION_META].as_str().or(negotiated)
    };
    let mirrored = version.is_some_and(|v| v >= HEADERS_FROM);
    let name = match method {
        Some("tools/call" | "prompts/get") => params["name"].as_str(),
        Some("resources/read") => params["uri"].as_str(),
        _ => None,
    };
    Headers {
        protocol_version: version.map(str::to_owned),
        method: method.filter(|_| mirrored).map(str::to_owned),
        name: name.filter(|_| mirrored).map(str::to_owned),
    }
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

struct Bridge {
    http: chelabase_http::Client,
    endpoint: String,
    instance: String,
    read_only: bool,
    version: Mutex<Option<String>>,
    out: tokio::sync::Mutex<tokio::io::Stdout>,
}

impl Bridge {
    /// Writes `message` as one line, whole.
    async fn write(&self, message: &Value) {
        let mut line = message.to_string();
        line.push('\n');
        let mut out = self.out.lock().await;
        if let Err(e) = out.write_all(line.as_bytes()).await {
            eprintln!("chelabase mcp: writing stdout: {e}");
        }
        let _ = out.flush().await;
    }

    /// A fresh token: the kept one is dropped so the session mints another.
    async fn fresh_bearer(&self) -> Result<String> {
        let dir = login::config_dir();
        let mut creds = login::load(&dir)?;
        if let Some(l) = creds.get_mut(&self.instance) {
            l.access_token = None;
            login::store(&dir, &creds)?;
        }
        login::bearer(&self.http, &self.instance).await
    }

    /// Posts `msg` and returns the response, retrying once with a fresh token on a 401.
    async fn post(&self, msg: &Value) -> Result<reqwest::Response> {
        let negotiated = self.version.lock().unwrap().clone();
        let headers = headers_for(msg, negotiated.as_deref());
        let mut token = login::bearer(&self.http, &self.instance).await?;
        for attempt in 0..2 {
            let mut req = self
                .http
                .post(&self.endpoint)
                .header("accept", "application/json, text/event-stream")
                .header("content-type", "application/json")
                .bearer_auth(&token)
                .body(msg.to_string());
            for (name, value) in [
                ("mcp-protocol-version", &headers.protocol_version),
                ("mcp-method", &headers.method),
                ("mcp-name", &headers.name),
            ] {
                if let Some(v) = value {
                    req = req.header(name, v);
                }
            }
            if self.read_only {
                req = req.header("chelabase-mcp-read-only", "true");
            }
            let resp = req.send().await.context("reaching the MCP endpoint")?;
            if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
                return Ok(resp);
            }
            if attempt == 0 {
                token = self.fresh_bearer().await?;
            }
        }
        anyhow::bail!(
            "the instance refused the member token: run `chelabase login --url {}`",
            self.instance
        )
    }

    /// Relays one message and writes what comes back.
    async fn relay(&self, msg: Value) {
        let id = msg.get("id").cloned();
        let initialize = msg["method"] == "initialize";
        let result = self.answer(&msg, initialize).await;
        if let Err(e) = result {
            let text = format!("{e:#}");
            match id {
                Some(id) => self.write(&rpc_error(&id, INTERNAL_ERROR, &text)).await,
                None => eprintln!("chelabase mcp: {text}"),
            }
        }
    }

    async fn answer(&self, msg: &Value, initialize: bool) -> Result<()> {
        let mut resp = self.post(msg).await?;
        let status = resp.status();
        let is_stream = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.starts_with("text/event-stream"));
        if is_stream && status.is_success() {
            let mut buffer = String::new();
            while let Some(chunk) = resp.chunk().await.context("reading the event stream")? {
                buffer.push_str(&String::from_utf8_lossy(&chunk).replace("\r\n", "\n"));
                while let Some(end) = buffer.find("\n\n") {
                    let event: String = buffer.drain(..end + 2).collect();
                    self.emit(sse_data(&event), initialize).await;
                }
            }
            self.emit(sse_data(&buffer), initialize).await;
            return Ok(());
        }
        let body = resp.text().await.context("reading the answer")?;
        if status == reqwest::StatusCode::ACCEPTED || body.trim().is_empty() && status.is_success()
        {
            return Ok(());
        }
        if serde_json::from_str::<Value>(&body).is_ok_and(|v| v["jsonrpc"] == "2.0") {
            // A JSON-RPC answer, also on an error status (a refused revision, say).
            self.emit(Some(body), initialize).await;
            return Ok(());
        }
        if status.is_success() {
            anyhow::bail!("the MCP endpoint answered with something that is not JSON-RPC");
        }
        anyhow::bail!("the MCP endpoint answered {status}")
    }

    /// Writes one payload as a line (compact), remembering the negotiated revision.
    async fn emit(&self, payload: Option<String>, initialize: bool) {
        let Some(payload) = payload else { return };
        let Ok(value) = serde_json::from_str::<Value>(&payload) else {
            eprintln!("chelabase mcp: dropped a non-JSON event from the endpoint");
            return;
        };
        if initialize && let Some(v) = value["result"]["protocolVersion"].as_str() {
            *self.version.lock().unwrap() = Some(v.to_owned());
        }
        self.write(&value).await;
    }
}

/// The `data:` payload of one SSE event (its lines joined), if any.
fn sse_data(event: &str) -> Option<String> {
    let lines: Vec<&str> = event
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|d| d.strip_prefix(' ').unwrap_or(d))
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

pub async fn run(url: Option<String>, read_only: bool) -> Result<()> {
    let instance = login::instance(url.as_deref(), None)?;
    let http = chelabase_http::client_builder()
        .build()
        .context("an HTTP client")?;
    // Fail before reading anything when there is no usable sign-in.
    login::bearer(&http, &instance).await?;
    let bridge = Arc::new(Bridge {
        http,
        endpoint: format!("{instance}/mcp"),
        instance,
        read_only,
        version: Mutex::new(None),
        out: tokio::sync::Mutex::new(tokio::io::stdout()),
    });
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut inflight = JoinSet::new();
    while let Some(line) = lines.next_line().await.context("reading stdin")? {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Err(e) => {
                let err = rpc_error(&Value::Null, PARSE_ERROR, &format!("parse error: {e}"));
                bridge.write(&err).await;
            }
            Ok(msg) if !msg.is_object() => {
                let err = rpc_error(
                    &Value::Null,
                    INVALID_REQUEST,
                    "send one JSON object per line",
                );
                bridge.write(&err).await;
            }
            Ok(msg) => {
                let bridge = bridge.clone();
                inflight.spawn(async move { bridge.relay(msg).await });
            }
        }
    }
    while inflight.join_next().await.is_some() {}
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn headers_follow_the_message() {
        let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "execute", "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}}});
        let h = headers_for(&call, None);
        assert_eq!(h.protocol_version.as_deref(), Some("2026-07-28"));
        assert_eq!(h.method.as_deref(), Some("tools/call"));
        assert_eq!(h.name.as_deref(), Some("execute"));

        // The older revision carries no method or name headers; the version comes from
        // `initialize`, then from what the server negotiated.
        let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-11-25"}});
        let h = headers_for(&init, None);
        assert_eq!(h.protocol_version.as_deref(), Some("2025-11-25"));
        assert_eq!(h.method, None);
        let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let h = headers_for(&list, Some("2025-11-25"));
        assert_eq!(h.protocol_version.as_deref(), Some("2025-11-25"));
        assert_eq!(h.method, None);
        let h = headers_for(&list, Some("2026-07-28"));
        assert_eq!(h.method.as_deref(), Some("tools/list"));
        assert_eq!(h.name, None);
    }
}
