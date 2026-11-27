//! The [`Sandbox`] over HTTP to `mcp-runtime` (`CHELABASE_MCP_RUNTIME_URL`): one `POST /run`
//! per tool call, on m3 (#347)'s contract (`infra/mcp-runtime/runner.js`).
//!
//! The run's `exec_id` is registered before `/run` is sent and removed before the outcome is
//! returned, whatever it is; a future dropped mid-run (a client disconnect) removes it too. The
//! host's clock is the one that counts: it gives up at `timeout_ms` + [`GRACE`] whatever the
//! runtime does.

use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::internal::{Exec, Registry, bundle_sha256};
use crate::sandbox::{ErrorCode, Kind, Outcome, Ran, Run, Sandbox};

/// How long past `timeout_ms` the host waits for the runtime's own answer.
pub const GRACE: Duration = Duration::from_secs(2);
/// The most of the runtime's answer read: its own cap is 6 MiB (a 4 MiB result plus logs).
const MAX_ANSWER_BYTES: usize = 8 << 20;

/// A fresh run id: 256 random bits as lowercase hex (64 characters, within the runtime's
/// `[A-Za-z0-9_-]{1,128}`).
pub fn new_exec_id() -> String {
    let bytes: [u8; 32] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The runtime client.
pub struct Runtime {
    http: chelabase_http::Client,
    run_url: String,
    timeout: Duration,
    registry: Registry,
}

impl Runtime {
    /// A client for the runtime at `base_url` (`http(s)://host[:port]`), giving each run
    /// `timeout`, registering runs in `registry`.
    pub fn new(base_url: &str, timeout: Duration, registry: Registry) -> anyhow::Result<Self> {
        let base = reqwest::Url::parse(base_url.trim())
            .with_context(|| format!("the runtime URL {base_url:?} is not a URL"))?;
        anyhow::ensure!(
            matches!(base.scheme(), "http" | "https") && base.host().is_some(),
            "the runtime URL must be http(s)://host[:port], not {base_url:?}"
        );
        let run_url = base.join("/run").context("the runtime URL")?.to_string();
        let http = chelabase_http::client_builder()
            .connect_timeout(Duration::from_secs(2))
            .build()
            .context("build the runtime client")?;
        Ok(Self {
            http,
            run_url,
            timeout,
            registry,
        })
    }

    async fn post(&self, run: &Run) -> Ran {
        let mut body = json!({
            "exec_id": run.exec_id,
            "kind": run.kind.as_str(),
            "code": run.code,
            "timeout_ms": self.timeout.as_millis() as u64,
        });
        if run.kind == Kind::Search {
            body["spec_hash"] = bundle_sha256().into();
        }
        let unavailable = |message: String| -> Ran {
            Outcome::Error {
                code: ErrorCode::RuntimeUnavailable,
                message,
            }
            .into()
        };
        let mut response = match self.http.post(&self.run_url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "the code runtime is unreachable");
                return unavailable("the code runtime is unreachable; try again shortly".into());
            }
        };
        let status = response.status();
        if !status.is_success() {
            tracing::warn!(%status, "the code runtime refused a run");
            return unavailable(format!(
                "the code runtime answered {}; try again shortly",
                status.as_u16()
            ));
        }
        let mut bytes = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if bytes.len() + chunk.len() > MAX_ANSWER_BYTES {
                        return unavailable("the code runtime's answer is too large".into());
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "the code runtime dropped a run");
                    return unavailable(
                        "the code runtime stopped mid-run; try again shortly".into(),
                    );
                }
            }
        }
        match serde_json::from_slice::<Answer>(&bytes) {
            Ok(answer) => {
                run.journal.unreceived(&answer.unreceived);
                answer.into_ran()
            }
            Err(_) => unavailable("the code runtime's answer is malformed".into()),
        }
    }
}

/// The runtime's 200 answer.
#[derive(Deserialize)]
struct Answer {
    ok: bool,
    #[serde(default)]
    result: Value,
    error: Option<AnswerError>,
    #[serde(default)]
    logs: Vec<String>,
    /// `execute`: the runner's numbers of the calls whose answer the runner never handed back.
    #[serde(default)]
    unreceived: Vec<u32>,
}

#[derive(Deserialize)]
struct AnswerError {
    code: String,
    message: String,
    stack: Option<String>,
}

impl Answer {
    fn into_ran(self) -> Ran {
        let outcome = match (self.ok, self.error) {
            (true, _) => Outcome::Value(self.result),
            (false, Some(e)) => {
                let code = match e.code.as_str() {
                    "invalid_code" => ErrorCode::InvalidCode,
                    "threw" => ErrorCode::Threw,
                    "timeout" => ErrorCode::Timeout,
                    "not_serializable" => ErrorCode::NotSerializable,
                    _ => ErrorCode::RuntimeUnavailable,
                };
                // The stack points into the caller's own code: it helps the model fix it.
                let message = match e.stack {
                    Some(stack) if !stack.is_empty() => format!("{}\n{stack}", e.message),
                    _ => e.message,
                };
                Outcome::Error { code, message }
            }
            (false, None) => Outcome::Error {
                code: ErrorCode::RuntimeUnavailable,
                message: "the code runtime's answer is malformed".into(),
            },
        };
        Ran {
            outcome,
            logs: self.logs,
        }
    }
}

#[async_trait]
impl Sandbox for Runtime {
    async fn run(&self, run: Run) -> Ran {
        let registration = self.registry.register(
            &run.exec_id,
            Exec::new(
                run.kind,
                run.caller.clone(),
                run.destructive,
                run.journal.clone(),
                std::time::Instant::now() + self.timeout,
            ),
        );
        let ran = match tokio::time::timeout(self.timeout + GRACE, self.post(&run)).await {
            Ok(ran) => ran,
            Err(_) => Outcome::Error {
                code: ErrorCode::Timeout,
                message: format!("the run took over {} ms", self.timeout.as_millis()),
            }
            .into(),
        };
        // Gone before the outcome is returned: no callback for this run is accepted after.
        drop(registration);
        ran
    }
}
