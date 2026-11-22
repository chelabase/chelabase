//! The host callback: `POST /internal/exec/{exec_id}/request` on the internal listener, where
//! `mcp-runtime` sends each `chelabase.request({method, path, query, headers, body})` of an
//! `execute` run (m3, #347). The host decides whether the call is an API operation the run may
//! make, then makes it as the member: the member's token is added here and never enters the
//! sandbox.
//!
//! For each call, in order:
//!
//! 1. The `exec_id` must name a live `execute` run (else 404, nothing else happens), and its
//!    deadline must not have passed.
//! 2. A run refused once (`destructive_flag_required`, `read_only_connection`) stays refused:
//!    this call and every later one answer the same, and the tool result is that error
//!    whatever the code returns.
//! 3. At most `CHELABASE_MCP_MAX_CALLS` calls per run.
//! 4. The path must be canonical and name an operation of the spec bundle
//!    ([`crate::routes_match`]); excluded operations are not in the bundle.
//! 5. `runSql` without the run's `destructive` flag, or on a read-only connection, gets
//!    `read_only: true` whatever the code sent. The body is then classed
//!    ([`crate::policy::classify`]) and the same re-serialized value is what is forwarded.
//! 6. A read-only connection makes `read` calls only; a `destructive` one needs the flag.
//! 7. An operation that answers with a credential is not sent: no sealer is configured yet
//!    (epic `mcp-sealed`), so it answers `secrets_unavailable`.
//! 8. Forwarded to `CHELABASE_MCP_API_URL` (the gateway) with `Host` set to the API host, the
//!    member's bearer, `Chelabase-Via-Mcp: 1`, a fresh `x-request-id`, and of the sandbox's
//!    headers only `accept` and `x-upsert`. Redirects are not followed. The call is cut at the
//!    run's deadline or when the run ends.
//! 9. The sandbox gets `{status, ok, headers, body}` with only `content-type`, `retry-after`
//!    and `x-request-id`; JSON bodies parsed, other text as text. A body over
//!    `CHELABASE_MCP_RESPONSE_MAX_BYTES` is `response_too_large`, a non-UTF-8 one
//!    `binary_body`, and one holding the member's token `secrets_unavailable`.
//!
//! A refusal the host makes is answered as an API-shaped error (the runner takes only statuses
//! 100–599): `{status, ok: false, headers: {}, body: {error: {code, message}}}`.
//!
//! An integer outside i64/u64 is refused `invalid_request` (fail closed: this build carries
//! numbers as written, through serde_json's `arbitrary_precision`, but a build without it
//! would round them), and so is a decimal serde_json would round.
//!
//! Each call, a malformed envelope included, writes one `info` audit line (target
//! `mcp::audit`, message `api call`) with no body, value, concrete path or token.
//!
//! # The invariant
//!
//! A write that happened is never silent. Precisely:
//!
//! 1. **One audit line per call.** Every call the host forwards or refuses writes exactly one
//!    audit line. A 404 for an unknown or ended run, and a handler dropped while it is still
//!    reading the body, send nothing and write none. A line is written also when the handler
//!    is dropped mid-call (the runner cancels a call still in flight
//!    when the code settles: an un-awaited `chelabase.request`, or a `Promise.all` sibling).
//!    A drop guard made before the handler's first await writes it; a dropped call's line says
//!    `refusal="cut"`, `status=0`.
//! 2. **Each forwarded call ends in one state.** A call refused before forwarding has no
//!    journal entry. Each forwarded call has one ([`Made`]), and once the run's registration
//!    has dropped it is in exactly one of these states:
//!    - *answered*: the host handed the code the API's answer (its status);
//!    - *upstream-unavailable*: the host handed the code `upstream_unavailable` (the API's
//!      connection failed; the call may or may not have run);
//!    - *unanswered*: the code never got its outcome (cut at the deadline or the run's end,
//!      or its handler dropped). It may have been applied.
//!
//!    The entry is marked when the handler hands its answer back, the last synchronous step
//!    before the answer goes out; a call answered `timeout` stays unanswered.
//! 3. **Unanswered calls make the result an error with `calls`.** Whenever an entry is
//!    unanswered, the tool result is an error listing every forwarded call (`status`: the
//!    number, `upstream_unavailable` or `unknown`), whatever the code returned or threw:
//!    the run's refusal if it was refused, `timeout` if the host's deadline passed or the
//!    runner timed out, the code's own error (`threw`, …) if it threw, else
//!    [`CALLS_UNFINISHED`]. A refused run's error and a timed-out run's error list `calls`
//!    whenever any call was forwarded.
//!
//! Calls are never kept running past the run to learn their outcome: unanswered is the end
//! state, reported as `unknown`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::internal::{Exec, Registry};
use crate::policy::{self, Class};
use crate::routes_match::Matcher;
use crate::sandbox::Kind;
use crate::secret_fields;

pub const DEFAULT_MAX_CALLS: u32 = 50;
/// The tool error when the code returned while a call it made had no answer yet.
pub const CALLS_UNFINISHED: &str = "calls_unfinished";
pub const DEFAULT_RESPONSE_MAX_BYTES: usize = 1 << 20;
/// The largest request body forwarded (after re-serializing).
pub const MAX_REQUEST_BODY_BYTES: usize = 1 << 20;
/// The header every forwarded call carries; routes that must never be reached from `execute`
/// refuse a request with it.
pub const VIA_MCP_HEADER: &str = "chelabase-via-mcp";
/// The sandbox's request headers that are forwarded; every other one is dropped.
pub const FORWARDED_REQUEST_HEADERS: [&str; 2] = ["accept", "x-upsert"];
/// The answer headers the sandbox sees.
pub const ANSWER_HEADERS: [&str; 3] = ["content-type", "retry-after", "x-request-id"];
/// The most of an envelope read (the runner caps it at 1 MiB of JSON text).
const MAX_ENVELOPE_BYTES: usize = 4 << 20;

/// The callback's settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostConfig {
    /// `CHELABASE_MCP_API_URL`: the gateway's address (`http(s)://host[:port]`). Unset: every
    /// call answers `upstream_unavailable`.
    pub api_url: Option<String>,
    /// `CHELABASE_MCP_MAX_CALLS` (1–1000, default 50).
    pub max_calls: u32,
    /// `CHELABASE_MCP_RESPONSE_MAX_BYTES` (1 KiB–16 MiB, default 1 MiB).
    pub response_max_bytes: usize,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            api_url: None,
            max_calls: DEFAULT_MAX_CALLS,
            response_max_bytes: DEFAULT_RESPONSE_MAX_BYTES,
        }
    }
}

impl HostConfig {
    /// Blank values count as unset.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let non_blank = |name: &str| {
            get(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let api_url = non_blank("CHELABASE_MCP_API_URL");
        if let Some(url) = &api_url {
            api_url_of(url)?;
        }
        let max_calls = match non_blank("CHELABASE_MCP_MAX_CALLS") {
            None => DEFAULT_MAX_CALLS,
            Some(v) => v
                .parse::<u32>()
                .ok()
                .filter(|n| (1..=1000).contains(n))
                .with_context(|| {
                    format!("CHELABASE_MCP_MAX_CALLS must be from 1 to 1000, not {v:?}")
                })?,
        };
        let response_max_bytes = match non_blank("CHELABASE_MCP_RESPONSE_MAX_BYTES") {
            None => DEFAULT_RESPONSE_MAX_BYTES,
            Some(v) => v
                .parse::<usize>()
                .ok()
                .filter(|n| (1024..=16 << 20).contains(n))
                .with_context(|| {
                    format!(
                        "CHELABASE_MCP_RESPONSE_MAX_BYTES must be from 1024 to 16777216, not {v:?}"
                    )
                })?,
        };
        Ok(Self {
            api_url,
            max_calls,
            response_max_bytes,
        })
    }
}

fn api_url_of(url: &str) -> anyhow::Result<reqwest::Url> {
    let parsed = reqwest::Url::parse(url)
        .ok()
        .filter(|u| {
            matches!(u.scheme(), "http" | "https")
                && u.host().is_some()
                && u.path() == "/"
                && u.query().is_none()
                && u.username().is_empty()
                && u.password().is_none()
        })
        .with_context(|| {
            format!("CHELABASE_MCP_API_URL must be http(s)://host[:port], not {url:?}")
        })?;
    Ok(parsed)
}

/// A host refusal, and the error code the sandbox (and the tool result) sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Code {
    OperationNotAllowed,
    DestructiveFlagRequired,
    ReadOnlyConnection,
    TooManyCalls,
    ResponseTooLarge,
    BinaryBody,
    UpstreamUnavailable,
    SecretsUnavailable,
    RequestTooLarge,
    InvalidRequest,
    /// An integer outside i64/u64, or a decimal serde_json would round (`invalid_request`).
    InexactNumber,
    Timeout,
    /// The run ended while the call was in flight (no deadline passed).
    Cut,
}

impl Code {
    pub fn as_str(self) -> &'static str {
        match self {
            Code::OperationNotAllowed => "operation_not_allowed",
            Code::DestructiveFlagRequired => "destructive_flag_required",
            Code::ReadOnlyConnection => "read_only_connection",
            Code::TooManyCalls => "too_many_calls",
            Code::ResponseTooLarge => "response_too_large",
            Code::BinaryBody => "binary_body",
            Code::UpstreamUnavailable => "upstream_unavailable",
            Code::SecretsUnavailable => "secrets_unavailable",
            Code::RequestTooLarge => "request_too_large",
            Code::InvalidRequest | Code::InexactNumber => "invalid_request",
            Code::Timeout => "timeout",
            Code::Cut => "cut",
        }
    }

    /// The status of the API-shaped answer the sandbox gets.
    fn status(self) -> u16 {
        match self {
            Code::OperationNotAllowed
            | Code::DestructiveFlagRequired
            | Code::ReadOnlyConnection
            | Code::SecretsUnavailable => 403,
            Code::TooManyCalls => 429,
            Code::RequestTooLarge => 413,
            Code::InvalidRequest | Code::InexactNumber => 400,
            Code::ResponseTooLarge | Code::BinaryBody | Code::UpstreamUnavailable => 502,
            Code::Timeout | Code::Cut => 504,
        }
    }

    pub fn message(self) -> &'static str {
        match self {
            Code::OperationNotAllowed => {
                "not an operation execute may call: use a path and method from search"
            }
            Code::DestructiveFlagRequired => {
                "a destructive operation needs execute's destructive: true; set it only when the user asked for this change"
            }
            Code::ReadOnlyConnection => "this connection is read-only: only read operations run",
            Code::TooManyCalls => "too many API calls in one execute",
            Code::ResponseTooLarge => "the answer is too large: page through it",
            Code::BinaryBody => "the answer is not text: binary transfer is not available here",
            Code::UpstreamUnavailable => "the API is unavailable; try again shortly",
            Code::SecretsUnavailable => {
                "this answer holds a credential, which execute cannot return"
            }
            Code::RequestTooLarge => "the request body is over 1 MiB",
            Code::InvalidRequest => "query values must be strings, numbers or booleans",
            Code::InexactNumber => {
                "a number in the request is an integer outside the 64-bit range or a decimal that can't be carried exactly: send it as a string"
            }
            Code::Timeout | Code::Cut => "the execution's time is up",
        }
    }
}

/// A sticky refusal: the code and the operations that caused it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: Code,
    pub operations: Vec<String>,
}

/// A call the run forwarded (see the invariant in the module doc).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Made {
    pub operation: String,
    /// The API's status, once its headers came back.
    pub status: Option<u16>,
    /// The host handed the code its answer: the API's, or a refusal made after the API
    /// answered. Unset: the code never got the outcome.
    pub answered: bool,
    /// The answer the code got was `upstream_unavailable`.
    pub upstream_unavailable: bool,
}

impl Made {
    /// Whether the code never got the call's outcome: it may have been applied.
    pub fn unknown(&self) -> bool {
        !self.answered
    }

    /// The call's `status` in a tool error's `calls`.
    pub fn label(&self) -> Value {
        match (self.answered, self.upstream_unavailable, self.status) {
            (false, ..) => json!("unknown"),
            (true, true, _) => json!("upstream_unavailable"),
            (true, false, Some(status)) => json!(status),
            (true, false, None) => json!("unknown"),
        }
    }
}

#[derive(Debug, Default)]
struct Calls {
    calls: u32,
    refusal: Option<Refusal>,
    made: Vec<Made>,
    /// The host's deadline passed during the run (a call was cut at it, or came after it).
    timed_out: bool,
}

/// What a run's API calls did, shared by the tool call and the host.
#[derive(Debug, Default)]
pub struct Journal {
    request_id: String,
    state: Mutex<Calls>,
}

impl Journal {
    /// A journal for the tool call `request_id`.
    pub fn new(request_id: &str) -> Self {
        Self {
            request_id: request_id.to_owned(),
            state: Mutex::default(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, Calls> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The run's sticky refusal, if any.
    pub fn refusal(&self) -> Option<Refusal> {
        self.state().refusal.clone()
    }

    /// The calls forwarded so far, in order.
    pub fn made(&self) -> Vec<Made> {
        self.state().made.clone()
    }

    /// Whether a `runSql` call ended without the code learning how.
    pub fn sql_cut(&self) -> bool {
        self.state()
            .made
            .iter()
            .any(|m| m.operation == "runSql" && m.unknown())
    }

    /// Whether the host's deadline passed during the run: the run is a timeout whatever the
    /// code returns.
    pub fn timed_out(&self) -> bool {
        self.state().timed_out
    }

    fn time_out(&self) {
        self.state().timed_out = true;
    }

    /// Marks entry `index` answered: the code is getting its outcome.
    fn deliver(&self, delivery: Delivery) {
        if let Some(made) = self.state().made.get_mut(delivery.index) {
            made.answered = true;
            made.upstream_unavailable = delivery.upstream_unavailable;
        }
    }

    /// Sets the sticky refusal (the first one wins) or adds `operation` to it.
    fn refuse(&self, code: Code, operation: &str) -> Code {
        let mut state = self.state();
        let refusal = state.refusal.get_or_insert_with(|| Refusal {
            code,
            operations: vec![],
        });
        if !refusal.operations.iter().any(|o| o == operation) {
            refusal.operations.push(operation.to_owned());
        }
        refusal.code
    }

    /// The `x-request-id` of call `n`: the tool call's id (its safe characters) and `.n`.
    fn request_id(&self, n: u32) -> String {
        let base: String = self
            .request_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            .take(96)
            .collect();
        let base = if base.is_empty() { "mcp" } else { &base };
        format!("{base}.{n}")
    }
}

/// The callback's state.
pub struct Host {
    registry: Registry,
    http: chelabase_http::Client,
    api_url: Option<reqwest::Url>,
    /// The `Host` forwarded calls carry: the API host of the entry.
    api_host: HeaderValue,
    max_calls: u32,
    response_max_bytes: usize,
}

impl Host {
    /// The host for `config`, with the API host taken from `api_base` (`<scheme>://<host>`).
    pub fn new(config: &HostConfig, api_base: &str, registry: Registry) -> anyhow::Result<Self> {
        let api_url = config.api_url.as_deref().map(api_url_of).transpose()?;
        let api_host = api_base
            .split_once("://")
            .map(|(_, h)| h.trim_end_matches('/'))
            .filter(|h| !h.is_empty() && !h.contains('/'))
            .with_context(|| format!("the API origin {api_base:?} has no host"))?;
        let api_host = HeaderValue::from_str(api_host).context("the API host")?;
        let http = chelabase_http::client_builder()
            .connect_timeout(Duration::from_secs(2))
            .build()
            .context("build the API client")?;
        Ok(Self {
            registry,
            http,
            api_url,
            api_host,
            max_calls: config.max_calls,
            response_max_bytes: config.response_max_bytes,
        })
    }
}

/// The internal listener's callback route.
pub fn router(host: Arc<Host>) -> Router {
    Router::new()
        .route("/internal/exec/{exec_id}/request", post(callback))
        .layer(DefaultBodyLimit::max(MAX_ENVELOPE_BYTES))
        .with_state(host)
}

/// What the runner sends.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    method: String,
    path: String,
    #[serde(default)]
    query: Option<Map<String, Value>>,
    #[serde(default)]
    headers: Option<Map<String, Value>>,
    #[serde(default)]
    body: Option<Value>,
}

/// One call's audit facts.
struct Line<'a> {
    exec_id: &'a str,
    exec: &'a Exec,
    operation: &'a str,
    class: &'a str,
    method: &'a str,
    template: &'a str,
    status: u16,
    request_bytes: usize,
    response_bytes: usize,
    refusal: &'a str,
}

fn audit(line: &Line<'_>) {
    tracing::info!(
        target: "mcp::audit",
        exec_id = line.exec_id,
        member = line.exec.caller.member(),
        operation = line.operation,
        class = line.class,
        method = line.method,
        path_template = line.template,
        status = line.status,
        request_bytes = line.request_bytes,
        response_bytes = line.response_bytes,
        refusal = line.refusal,
        "api call"
    );
}

/// The API-shaped answer of a host refusal.
fn refused(code: Code) -> Response {
    axum::Json(json!({
        "status": code.status(),
        "ok": false,
        "headers": {},
        "body": {"error": {"code": code.as_str(), "message": code.message()}},
    }))
    .into_response()
}

/// What the code is told about a forwarded call (marked by [`Journal::deliver`]).
#[derive(Debug, Clone, Copy)]
struct Delivery {
    index: usize,
    upstream_unavailable: bool,
}

/// Writes a call's audit line exactly once: when the handler ends, or when it is dropped
/// mid-call (then as `cut`).
struct Audited<'a> {
    line: Line<'a>,
    finished: bool,
}

impl Drop for Audited<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.line.status = 0;
            self.line.refusal = "cut";
        }
        audit(&self.line);
    }
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, "no such run").into_response()
}

async fn callback(
    State(host): State<Arc<Host>>,
    Path(exec_id): Path<String>,
    bytes: Bytes,
) -> Response {
    let Some(exec) = host.registry.get(&exec_id) else {
        return not_found();
    };
    if exec.kind != Kind::Execute {
        return not_found();
    }
    let Ok(envelope) = serde_json::from_slice::<Envelope>(&bytes) else {
        audit(&Line {
            exec_id: &exec_id,
            exec: &exec,
            operation: "",
            class: "",
            method: "",
            template: "",
            status: StatusCode::BAD_REQUEST.as_u16(),
            request_bytes: bytes.len(),
            response_bytes: 0,
            refusal: Code::InvalidRequest.as_str(),
        });
        return (StatusCode::BAD_REQUEST, "not a request envelope").into_response();
    };
    let exact = !lossy_number(&bytes);
    // Made before the first await: a dropped handler still writes its line.
    let mut audited = Audited {
        line: Line {
            exec_id: &exec_id,
            exec: &exec,
            operation: "",
            class: "",
            method: &envelope.method,
            template: "",
            status: 0,
            request_bytes: 0,
            response_bytes: 0,
            refusal: "",
        },
        finished: false,
    };
    let (response, delivery) = handle(&host, &exec, &envelope, exact, &mut audited.line).await;
    audited.finished = true;
    if let Some(delivery) = delivery {
        exec.journal.deliver(delivery);
    }
    response
}

async fn handle<'a>(
    host: &Host,
    exec: &'a Exec,
    envelope: &'a Envelope,
    exact: bool,
    line: &mut Line<'a>,
) -> (Response, Option<Delivery>) {
    let refuse = |line: &mut Line<'a>, code: Code| {
        line.refusal = code.as_str();
        line.status = code.status();
        (refused(code), None)
    };
    let journal = &exec.journal;
    if Instant::now() >= exec.deadline {
        journal.time_out();
        return refuse(line, Code::Timeout);
    }
    let matched = Matcher::bundle().find(&envelope.method, &envelope.path);
    let read_only = exec.caller.read_only();

    // The body as classed and forwarded: re-serialized, with `read_only` forced for `runSql`.
    let mut body = envelope.body.clone().filter(|b| !b.is_null());
    let (operation, class) = match &matched {
        Ok(m) => {
            if m.operation_id == "runSql" && (!exec.destructive || read_only) {
                match &mut body {
                    Some(Value::Object(map)) => {
                        map.insert("read_only".into(), Value::Bool(true));
                    }
                    None => body = Some(json!({"read_only": true})),
                    Some(_) => {}
                }
            }
            let class = policy::classify(m.operation_id, &envelope.method, body.as_ref());
            line.operation = m.operation_id;
            line.template = m.template;
            line.class = class.as_str();
            (Some(m.operation_id), class)
        }
        Err(_) => (None, Class::Excluded),
    };
    let gate = |class: Class| {
        if read_only && !policy::allowed_in_read_only_mode(class) {
            Some(Code::ReadOnlyConnection)
        } else if class == Class::Destructive && !exec.destructive {
            Some(Code::DestructiveFlagRequired)
        } else {
            None
        }
    };

    // A refused run stays refused; a later call the gates would refuse is named too.
    if let Some(refusal) = journal.refusal() {
        if let (Some(op), Some(_)) = (operation, gate(class).filter(|_| class != Class::Excluded)) {
            journal.refuse(refusal.code, op);
        }
        return refuse(line, refusal.code);
    }
    {
        let mut state = journal.state();
        state.calls += 1;
        if state.calls > host.max_calls {
            drop(state);
            return refuse(line, Code::TooManyCalls);
        }
    }
    let Some(operation) = operation else {
        return refuse(line, Code::OperationNotAllowed);
    };
    if class == Class::Excluded {
        return refuse(line, Code::OperationNotAllowed);
    }
    if let Some(code) = gate(class) {
        let code = journal.refuse(code, operation);
        return refuse(line, code);
    }
    if secret_fields::answers_with_credential(operation) {
        return refuse(line, Code::SecretsUnavailable);
    }
    // serde_json (without `arbitrary_precision`) would forward such a number rounded.
    if !exact {
        return refuse(line, Code::InexactNumber);
    }
    let body = match body.as_ref().map(serde_json::to_vec).transpose() {
        Ok(body) => body,
        Err(_) => return refuse(line, Code::InvalidRequest),
    };
    if body
        .as_ref()
        .is_some_and(|b| b.len() > MAX_REQUEST_BODY_BYTES)
    {
        return refuse(line, Code::RequestTooLarge);
    }
    line.request_bytes = body.as_ref().map_or(0, Vec::len);
    let (index, outcome) = forward(host, exec, envelope, operation, body).await;
    match outcome {
        Ok((status, answer, bytes)) => {
            line.status = status;
            line.response_bytes = bytes;
            let delivery = index.map(|index| Delivery {
                index,
                upstream_unavailable: false,
            });
            (axum::Json(answer).into_response(), delivery)
        }
        Err((code, bytes)) => {
            line.response_bytes = bytes;
            let (response, _) = refuse(line, code);
            if code == Code::Cut {
                line.status = 0;
            }
            // A cut call stays unanswered: the code never learns its outcome.
            let delivery = index
                .filter(|_| !matches!(code, Code::Timeout | Code::Cut))
                .map(|index| Delivery {
                    index,
                    upstream_unavailable: code == Code::UpstreamUnavailable,
                });
            (response, delivery)
        }
    }
}

/// The member's `Authorization` value.
fn bearer(token: &str) -> Option<HeaderValue> {
    let mut value = HeaderValue::from_str(&format!("Bearer {token}")).ok()?;
    value.set_sensitive(true);
    Some(value)
}

/// Whether the JSON text `bytes` (already parsed) holds a number serde_json can't carry
/// exactly: an integer outside i64/u64, or a number whose re-serialized form is another
/// decimal value.
fn lossy_number(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i += 1;
                while i < bytes.len() && bytes[i] != b'"' {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'-' | b'0'..=b'9' => {
                let start = i;
                while i < bytes.len()
                    && matches!(bytes[i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                {
                    i += 1;
                }
                let Ok(token) = std::str::from_utf8(&bytes[start..i]) else {
                    return true;
                };
                if !exact_number(token) {
                    return true;
                }
            }
            _ => i += 1,
        }
    }
    false
}

/// Whether serde_json carries the JSON number `token` exactly.
fn exact_number(token: &str) -> bool {
    if !token.contains(['.', 'e', 'E']) {
        return token.parse::<i64>().is_ok() || token.parse::<u64>().is_ok();
    }
    let Ok(Value::Number(n)) = serde_json::from_str::<Value>(token) else {
        return false;
    };
    decimal(token) == decimal(&n.to_string())
}

/// A JSON number as `(negative, significant digits, exponent)` of `0.digits × 10^exponent`;
/// zero is `(false, "", 0)`.
fn decimal(token: &str) -> (bool, String, i64) {
    let (negative, rest) = match token.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, token),
    };
    let (mantissa, exponent) = match rest.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i64>().unwrap_or(i64::MAX / 2)),
        None => (rest, 0),
    };
    let (int, frac) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{int}{frac}");
    let leading = digits.len() - digits.trim_start_matches('0').len();
    let digits = digits.trim_start_matches('0').trim_end_matches('0');
    if digits.is_empty() {
        return (false, String::new(), 0);
    }
    let exponent = exponent + int.len() as i64 - leading as i64;
    (negative, digits.to_owned(), exponent)
}

/// The URL of a call: the gateway's address, the checked path, the query.
fn url_of(
    base: &reqwest::Url,
    path: &str,
    query: Option<&Map<String, Value>>,
) -> Option<reqwest::Url> {
    let mut url = base.clone();
    url.set_path(path);
    // The matcher only accepts paths the URL keeps as they are; check it anyway.
    if url.path() != path {
        return None;
    }
    url.set_query(None);
    if let Some(query) = query.filter(|q| !q.is_empty()) {
        let mut pairs = url.query_pairs_mut();
        for (name, value) in query {
            let value = match value {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                _ => return None,
            };
            pairs.append_pair(name, &value);
        }
    }
    Some(url)
}

/// Makes the call: its journal entry (once one is made) and the outcome.
async fn forward(
    host: &Host,
    exec: &Exec,
    envelope: &Envelope,
    operation: &str,
    body: Option<Vec<u8>>,
) -> (Option<usize>, Result<(u16, Value, usize), (Code, usize)>) {
    let mut index = None;
    let outcome = send(host, exec, envelope, operation, body, &mut index).await;
    (index, outcome)
}

/// [`forward`]'s work. `Ok` is the API's status, the sandbox's answer and the answer's bytes;
/// `Err` a refusal and the bytes read. `entry` is set once the journal entry is made.
async fn send(
    host: &Host,
    exec: &Exec,
    envelope: &Envelope,
    operation: &str,
    body: Option<Vec<u8>>,
    entry: &mut Option<usize>,
) -> Result<(u16, Value, usize), (Code, usize)> {
    let Some(base) = &host.api_url else {
        return Err((Code::UpstreamUnavailable, 0));
    };
    let Some(url) = url_of(base, &envelope.path, envelope.query.as_ref()) else {
        return Err((Code::InvalidRequest, 0));
    };
    let Ok(method) = reqwest::Method::from_bytes(envelope.method.as_bytes()) else {
        return Err((Code::OperationNotAllowed, 0));
    };
    let token = exec.caller.credential().expose();
    let Some(bearer) = bearer(token) else {
        return Err((Code::UpstreamUnavailable, 0));
    };

    // The journal entry, and the call's number for its request id.
    let (index, n) = {
        let mut state = exec.journal.state();
        state.made.push(Made {
            operation: operation.to_owned(),
            status: None,
            answered: false,
            upstream_unavailable: false,
        });
        (state.made.len() - 1, state.calls)
    };
    *entry = Some(index);

    let mut request = host
        .http
        .request(method, url)
        .header(header::HOST, host.api_host.clone())
        .header(header::AUTHORIZATION, bearer)
        .header(VIA_MCP_HEADER, HeaderValue::from_static("1"))
        .header("x-request-id", exec.journal.request_id(n));
    for (name, value) in envelope.headers.iter().flatten() {
        let name = name.to_ascii_lowercase();
        if !FORWARDED_REQUEST_HEADERS.contains(&name.as_str()) {
            continue;
        }
        if let Some(value) = value.as_str().and_then(|v| HeaderValue::from_str(v).ok()) {
            request = request.header(name, value);
        }
    }
    if let Some(body) = body {
        request = request
            .header(header::CONTENT_TYPE, "application/json")
            .body(body);
    }

    let max = host.response_max_bytes;
    let work = async {
        let mut response = request
            .send()
            .await
            .map_err(|_| (Code::UpstreamUnavailable, 0))?;
        let status = response.status().as_u16();
        let mut headers = Map::new();
        for name in ANSWER_HEADERS {
            if let Some(v) = response.headers().get(name).and_then(|v| v.to_str().ok()) {
                headers.insert(name.into(), v.into());
            }
        }
        exec.journal.state().made[index].status = Some(status);
        let mut bytes = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if bytes.len() + chunk.len() > max {
                        return Err((Code::ResponseTooLarge, bytes.len() + chunk.len()));
                    }
                    bytes.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(_) => return Err((Code::UpstreamUnavailable, bytes.len())),
            }
        }
        Ok((status, headers, bytes))
    };
    let (status, headers, bytes) = tokio::select! {
        biased;
        () = exec.ended() => return Err((Code::Cut, 0)),
        () = tokio::time::sleep_until(exec.deadline.into()) => {
            exec.journal.time_out();
            return Err((Code::Timeout, 0));
        }
        done = work => done?,
    };
    let size = bytes.len();
    let Ok(text) = String::from_utf8(bytes) else {
        return Err((Code::BinaryBody, size));
    };
    let is_json = headers
        .get("content-type")
        .and_then(Value::as_str)
        .and_then(|ct| ct.split(';').next())
        .map(|ct| {
            let ct = ct.trim().to_ascii_lowercase();
            ct == "application/json" || ct.ends_with("+json")
        })
        .unwrap_or(false);
    let body = if text.is_empty() {
        Value::Null
    } else if is_json {
        serde_json::from_str(&text).unwrap_or(Value::String(text))
    } else {
        Value::String(text)
    };
    let answer = json!({
        "status": status,
        "ok": (200..300).contains(&status),
        "headers": headers,
        "body": body,
    });
    // The member's token never reaches the sandbox, even when an upstream echoes it.
    if answer.to_string().contains(token) {
        return Err((Code::SecretsUnavailable, size));
    }
    Ok((status, answer, size))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bearer_is_sensitive() {
        let value = bearer("tok").unwrap();
        assert_eq!(value, "Bearer tok");
        assert!(value.is_sensitive());
    }

    #[test]
    fn integers_outside_i64_and_u64_are_lossy() {
        let lossy = |n: &str| lossy_number(format!(r#"{{"a":[{n}]}}"#).as_bytes());
        for n in [
            "18446744073709551616",
            "-9223372036854775809",
            "123456789012345678901234567890",
            // Outside the range even though its f64 is exact.
            "100000000000000000000",
        ] {
            assert!(lossy(n), "{n}");
        }
        for n in [
            "18446744073709551615",
            "-9223372036854775808",
            "0",
            "-0",
            "1.5",
            "1e3",
            "1E-7",
            "0.1",
            "1.0",
            "-2.50",
        ] {
            assert!(!lossy(n), "{n}");
        }
        // Digits inside strings and keys are not numbers.
        assert!(!lossy_number(
            br#"{"99999999999999999999999": "123456789012345678901234567890", "e": "1e400\"9"}"#
        ));
    }

    /// A decimal that serde_json would round is lossy. The build decides: `chelabase-query`
    /// turns on serde_json's `arbitrary_precision` for this crate, and then numbers are carried
    /// as written and none is.
    #[test]
    fn a_decimal_serde_json_would_round_is_lossy() {
        for n in ["0.10000000000000000001", "12345678901234567890123.0"] {
            let rounds = serde_json::from_str::<Value>(n)
                .unwrap()
                .to_string()
                .as_str()
                != n;
            assert_eq!(!exact_number(n), rounds, "{n}");
        }
        assert_eq!(decimal("0.10"), decimal("1e-1"));
        assert_eq!(decimal("-2.50"), decimal("-25E-1"));
        assert_eq!(decimal("1200"), decimal("1.2e3"));
        assert_eq!(decimal("0"), decimal("-0.0e5"));
        assert_ne!(decimal("0.10000000000000000001"), decimal("0.1"));
        assert_ne!(
            decimal("12345678901234567890123.0"),
            decimal("1.2345678901234568e22")
        );
        assert_ne!(decimal("-1"), decimal("1"));
    }

    #[test]
    fn config_reads_the_three_variables() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        assert_eq!(
            HostConfig::from_lookup(env(&[])).unwrap(),
            HostConfig::default()
        );
        let config = HostConfig::from_lookup(env(&[
            ("CHELABASE_MCP_API_URL", "http://gateway:8000"),
            ("CHELABASE_MCP_MAX_CALLS", "10"),
            ("CHELABASE_MCP_RESPONSE_MAX_BYTES", "2048"),
        ]))
        .unwrap();
        assert_eq!(config.api_url.as_deref(), Some("http://gateway:8000"));
        assert_eq!(config.max_calls, 10);
        assert_eq!(config.response_max_bytes, 2048);
        for (name, bad) in [
            ("CHELABASE_MCP_API_URL", "gateway:8000"),
            ("CHELABASE_MCP_API_URL", "http://gateway:8000/v1"),
            ("CHELABASE_MCP_API_URL", "http://u:p@gateway:8000"),
            ("CHELABASE_MCP_API_URL", "ftp://gateway"),
            ("CHELABASE_MCP_MAX_CALLS", "0"),
            ("CHELABASE_MCP_MAX_CALLS", "1001"),
            ("CHELABASE_MCP_RESPONSE_MAX_BYTES", "10"),
        ] {
            let lookup = move |n: &str| (n == name).then(|| bad.to_owned());
            assert!(HostConfig::from_lookup(lookup).is_err(), "{name}={bad}");
        }
    }

    #[test]
    fn request_ids_keep_safe_characters_only() {
        assert_eq!(Journal::new("req-1").request_id(3), "req-1.3");
        assert_eq!(Journal::new("\"x y\"").request_id(1), "xy.1");
        assert_eq!(Journal::new("\"\"").request_id(1), "mcp.1");
        assert!(Journal::new(&"a".repeat(500)).request_id(50).len() <= 128);
    }

    #[test]
    fn the_url_keeps_the_checked_path() {
        let base = reqwest::Url::parse("http://gateway:8000").unwrap();
        let url = url_of(&base, "/v1/databases/a%20b", None).unwrap();
        assert_eq!(url.as_str(), "http://gateway:8000/v1/databases/a%20b");
        let mut query = Map::new();
        query.insert("q".into(), json!("a&b=c"));
        query.insert("n".into(), json!(2));
        let url = url_of(&base, "/v1/projects", Some(&query)).unwrap();
        assert_eq!(url.query(), Some("n=2&q=a%26b%3Dc"));
        query.insert("bad".into(), json!([1]));
        assert!(url_of(&base, "/v1/projects", Some(&query)).is_none());
    }
}
