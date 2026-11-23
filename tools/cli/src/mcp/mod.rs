//! `chelabase mcp`: a stdio bridge to the instance's `/mcp` endpoint. Each stdin line is one
//! JSON-RPC message, posted with the signed-in member's token; each answer is written whole as
//! one stdout line. Only protocol messages go to stdout; diagnostics go to stderr, and a token
//! is never printed.

use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::task::JoinSet;

use crate::dev::token::seconds_left;
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

/// The headers for `msg`: its revision (its `_meta`, else the one the server negotiated), and
/// from 2026-07-28 on its method and, for a call, the tool, prompt or resource. `initialize`
/// carries no revision header: the server negotiates from the body, so a client asking for a
/// revision the server doesn't serve gets one it does instead of a 400.
fn headers_for(msg: &Value, negotiated: Option<&str>) -> Headers {
    let method = msg["method"].as_str();
    let params = &msg["params"];
    let initialize = method == Some("initialize");
    let version = if initialize {
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
        protocol_version: version.filter(|_| !initialize).map(str::to_owned),
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
    /// Held while a token is read, refreshed or stored, so pipelined requests share one refresh.
    tokens: tokio::sync::Mutex<()>,
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

    /// A member token. With `refused` (a token the instance rejected), the kept token is
    /// dropped first, unless another request already replaced it.
    async fn token(&self, refused: Option<&str>) -> Result<String> {
        let _held = self.tokens.lock().await;
        if let Some(refused) = refused {
            let dir = login::config_dir();
            let mut creds = login::load(&dir)?;
            if let Some(l) = creds.get_mut(&self.instance)
                && l.access_token.as_deref() == Some(refused)
            {
                l.access_token = None;
                login::store(&dir, &creds)?;
            }
        }
        login::bearer(&self.http, &self.instance).await
    }

    /// Posts `msg` and returns the response, retrying once with a fresh token on a 401.
    async fn post(&self, msg: &Value) -> Result<reqwest::Response> {
        let negotiated = self.version.lock().unwrap().clone();
        let headers = headers_for(msg, negotiated.as_deref());
        let mut token = self.token(None).await?;
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
                token = self.token(Some(&token)).await?;
            } else {
                // The token just obtained, refused again: say why when signing in can't help.
                let body: Value = resp.json().await.unwrap_or_default();
                let needed = body["error"]["min_remaining_secs"].as_u64();
                let left = seconds_left(&token, now());
                if let Some(needed) = needed.filter(|n| left > 0 && left < *n) {
                    anyhow::bail!(
                        "the instance refused a freshly issued member token: it has {left} seconds \
left and the MCP server needs at least {needed}. Signing in again gives the same lifetime: ask the \
operator to raise CHELABASE_MEMBER_ACCESS_TTL_SECS above {needed}, or to lower \
CHELABASE_MCP_TIMEOUT_MS"
                    );
                }
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
            let mut events = Events::default();
            while let Some(chunk) = resp.chunk().await.context("reading the event stream")? {
                for event in events.push(&chunk) {
                    self.emit(sse_data(&event), initialize).await;
                }
            }
            if let Some(event) = events.finish() {
                self.emit(sse_data(&event), initialize).await;
            }
            return Ok(());
        }
        let body = resp.text().await.context("reading the answer")?;
        if status == reqwest::StatusCode::ACCEPTED || body.trim().is_empty() && status.is_success()
        {
            return Ok(());
        }
        if let Ok(mut answer) = serde_json::from_str::<Value>(&body)
            && answer["jsonrpc"] == "2.0"
        {
            // A JSON-RPC answer, also on an error status (a refused revision, say). An error
            // the endpoint sent without an id (its transport checks run before it reads one)
            // gets the request's, so the client doesn't wait for an answer forever.
            if answer.get("error").is_some()
                && answer.get("id").is_none_or(Value::is_null)
                && let Some(id) = msg.get("id").filter(|id| !id.is_null())
            {
                answer["id"] = id.clone();
            }
            self.emit(Some(answer.to_string()), initialize).await;
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

/// Splits an event stream's bytes into events (blank-line separated), decoding each whole event
/// so a character split across chunks survives.
#[derive(Default)]
struct Events(Vec<u8>);

impl Events {
    fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.0.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((end, sep)) = self.boundary() {
            let event: Vec<u8> = self.0.drain(..end + sep).collect();
            out.push(String::from_utf8_lossy(&event[..end]).replace("\r\n", "\n"));
        }
        out
    }

    /// What is left when the stream ends (an event with no closing blank line).
    fn finish(&mut self) -> Option<String> {
        let rest = std::mem::take(&mut self.0);
        let text = String::from_utf8_lossy(&rest).replace("\r\n", "\n");
        (!text.trim().is_empty()).then_some(text)
    }

    /// The first blank line: its offset and length.
    fn boundary(&self) -> Option<(usize, usize)> {
        (0..self.0.len()).find_map(|i| {
            let rest = &self.0[i..];
            if rest.starts_with(b"\r\n\r\n") {
                Some((i, 4))
            } else if rest.starts_with(b"\n\n") {
                Some((i, 2))
            } else {
                None
            }
        })
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

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
        tokens: tokio::sync::Mutex::new(()),
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

        // The older revision carries no method or name headers; the version comes from what
        // the server negotiated.
        let init = json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-11-25"}});
        let h = headers_for(&init, None);
        assert_eq!(h.protocol_version, None, "initialize negotiates from the body");
        assert_eq!(h.method, None);
        let list = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        let h = headers_for(&list, Some("2025-11-25"));
        assert_eq!(h.protocol_version.as_deref(), Some("2025-11-25"));
        assert_eq!(h.method, None);
        let h = headers_for(&list, Some("2026-07-28"));
        assert_eq!(h.method.as_deref(), Some("tools/list"));
        assert_eq!(h.name, None);
    }

    #[test]
    fn events_split_across_chunks_keep_their_characters() {
        let text = "data: {\"t\":\"h\u{e9}llo \u{1f600}\"}\r\n\r\ndata: two\n\n";
        let bytes = text.as_bytes();
        // Every split point, including inside a multibyte character and inside "\r\n".
        for cut in 0..bytes.len() {
            let mut events = Events::default();
            let mut got = events.push(&bytes[..cut]);
            got.extend(events.push(&bytes[cut..]));
            assert_eq!(events.finish(), None);
            let data: Vec<_> = got.iter().map(|e| sse_data(e).unwrap()).collect();
            assert_eq!(
                data,
                ["{\"t\":\"h\u{e9}llo \u{1f600}\"}", "two"],
                "cut {cut}"
            );
        }
    }
}
