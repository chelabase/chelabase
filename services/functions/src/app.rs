//! The two listeners' routes.
//!
//! Public (behind the gateway): `ANY /functions/v1/{name}[/{*rest}]` with the gateway's
//! identity token (a data key, maybe with a user token), `ANY
//! /functions/v1/public/{branch_id}/{name}[/{*rest}]` with none, `ANY
//! /services/v1/{name}[/{*rest}]` with a data key (m9, [`crate::services`]) and `GET /health`.
//! Nothing else, `/internal/*` included.
//!
//! Internal, in two sides on two networks (#181, audit S1): the runtime's side serves only
//! `GET /internal/bundles/{database_id}/{sha256}` (no bearer: the runtime has none); the
//! control side serves only `PUT` the same (the orchestrator's bearer) and `POST
//! /internal/invoke/{branch_id}/{name}` (the worker's bearer; a `{"type": "scheduled"}` body
//! reaches the runtime marked `x-chelabase-event: scheduled`, m7). A database event (m13) is
//! marked `chelabase-event: db` only by this process's dispatcher ([`crate::events`], m13b):
//! the header is stripped from every HTTP call, the worker's included.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Response, StatusCode, Uri, header};
use axum::response::IntoResponse;
use axum::routing::{any, get};
use chelabase_auth::internal::{IDENTITY_HEADER, InternalVerifier};
use moka::sync::Cache;
use serde_json::json;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::bundles::{self, Bundles, MAX_BUNDLE_BYTES, Put};
use crate::caller::{self, Caller, Form, Refusal};
use crate::directory::Resolution;
use crate::host::{ERROR_HEADER, EVENT_HEADER, FunctionHost, HostError, Invocation};
use crate::resolver::Resolver;

/// m13: marks a database event to the function (m13b: set by the dispatcher in this process only).
pub const DB_EVENT_HEADER: &str = "chelabase-event";

/// A request body may be at most 6 MiB.
pub const MAX_BODY_BYTES: usize = 6 * 1024 * 1024;
/// The gateway's identity tokens name this audience (`Service::Functions`).
pub const AUDIENCE: &str = "functions";
/// What the gateway adds for this service only, after verifying them: the caller's public key
/// and its user token.
pub const FORWARDED_KEY: &str = "chelabase-forwarded-key";
pub const FORWARDED_USER: &str = "chelabase-forwarded-user";
/// What the function gets.
pub const CALLER: &str = "chelabase-caller";
pub const CALLER_KEY: &str = "chelabase-caller-key";
pub const CALLER_TOKEN: &str = "chelabase-caller-token";
const LAST_ERROR_MAX: usize = 500;
/// The same `last_error` is written at most once in this window per function.
const LAST_ERROR_QUIET: Duration = Duration::from_secs(10);

pub struct AppState {
    pub resolver: Arc<Resolver>,
    pub host: Arc<dyn FunctionHost>,
    pub verifier: InternalVerifier,
    pub bundles: Bundles,
    pub orchestrator_token: String,
    pub worker_token: String,
    /// m9: the external services' proxy; `None` answers 503 there.
    pub services: Option<crate::services::Services>,
    recent_errors: Cache<(Uuid, Uuid), String>,
}

impl AppState {
    pub fn new(
        resolver: Arc<Resolver>,
        host: Arc<dyn FunctionHost>,
        verifier: InternalVerifier,
        bundles: Bundles,
        orchestrator_token: String,
        worker_token: String,
    ) -> Self {
        Self {
            resolver,
            host,
            verifier,
            bundles,
            orchestrator_token,
            worker_token,
            services: None,
            recent_errors: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(LAST_ERROR_QUIET)
                .build(),
        }
    }

    /// Serves `/services/v1` through `services`.
    pub fn with_services(mut self, services: crate::services::Services) -> Self {
        self.services = Some(services);
        self
    }
}

type Shared = Arc<AppState>;

pub fn public_router(state: Shared) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route(
            "/functions/v1/public/{branch_id}/{name}",
            any(invoke_public),
        )
        .route(
            "/functions/v1/public/{branch_id}/{name}/{*rest}",
            any(invoke_public_rest),
        )
        .route("/functions/v1/{name}", any(invoke_key))
        .route("/functions/v1/{name}/{*rest}", any(invoke_key_rest))
        .route("/services/v1/{name}", any(call_service))
        .route("/services/v1/{name}/{*rest}", any(call_service_rest))
        .fallback(|| async { error(StatusCode::NOT_FOUND, "not_found", "no route") })
        .with_state(state)
}

/// The internal listener's runtime side (the runtime's `functions` network): bundle GETs only.
/// Any other method or path is 404, never 405, so it says nothing of the control side.
pub fn runtime_router(state: Shared) -> Router {
    Router::new()
        .route("/internal/bundles/{database_id}/{sha256}", get(get_bundle))
        .method_not_allowed_fallback(|| async { error(StatusCode::NOT_FOUND, "not_found", "no route") })
        .fallback(|| async { error(StatusCode::NOT_FOUND, "not_found", "no route") })
        .with_state(state)
}

/// Its control side (the orchestrator's and the worker's `functions-control` network): bundle
/// PUTs (the orchestrator's bearer) and invocations (the worker's), nothing else.
pub fn control_router(state: Shared) -> Router {
    Router::new()
        .route("/internal/bundles/{database_id}/{sha256}", axum::routing::put(put_bundle))
        .route("/internal/invoke/{branch_id}/{name}", axum::routing::post(invoke_internal))
        .method_not_allowed_fallback(|| async { error(StatusCode::NOT_FOUND, "not_found", "no route") })
        .fallback(|| async { error(StatusCode::NOT_FOUND, "not_found", "no route") })
        .with_state(state)
}

pub(crate) fn error(status: StatusCode, code: &str, message: &str) -> Response<Body> {
    (
        status,
        axum::Json(json!({"error": {"code": code, "message": message}})),
    )
        .into_response()
}

fn not_found() -> Response<Body> {
    error(
        StatusCode::NOT_FOUND,
        "function_not_found",
        "no such function",
    )
}

/// The request as it came, for an invocation.
pub(crate) struct Incoming {
    pub(crate) method: Method,
    /// What the function sees as its path: `/{name}[/{rest}]` (a service: `[/{rest}]`), exactly
    /// as the client sent it (still percent-encoded, dot segments kept).
    pub(crate) path: String,
    pub(crate) headers: HeaderMap,
    pub(crate) query: Option<String>,
    pub(crate) body: Body,
}

/// The request path without its first `skip` segments, exactly as sent: the route's own
/// segments go and the function's `/{name}[/{rest}]` stays percent-encoded. The decoded `Path`
/// would turn `%3F` into a query and `%2F..%2F` into dot segments the client never sent.
fn raw_tail(path: &str, skip: usize) -> String {
    let mut rest = path;
    for _ in 0..skip {
        let s = rest.strip_prefix('/').unwrap_or(rest);
        rest = s.find('/').map_or("", |i| &s[i..]);
    }
    rest.to_owned()
}

/// `/functions/v1/{name}…`: two route segments.
const KEY_PREFIX: usize = 2;
/// `/functions/v1/public/{branch_id}/{name}…`: four.
const PUBLIC_PREFIX: usize = 4;
/// `/services/v1/{name}…`: three (a service sees only the rest).
const SERVICE_PREFIX: usize = 3;
/// `/internal/invoke/{branch_id}/{name}`: three.
const INTERNAL_PREFIX: usize = 3;

async fn invoke_key(
    State(s): State<Shared>,
    Path(name): Path<String>,
    uri: Uri,
    RawQuery(query): RawQuery,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response<Body> {
    let path = raw_tail(uri.path(), KEY_PREFIX);
    key_form(
        s,
        name,
        Incoming {
            method,
            path,
            headers,
            query,
            body,
        },
    )
    .await
}

async fn invoke_key_rest(
    State(s): State<Shared>,
    Path((name, _rest)): Path<(String, String)>,
    uri: Uri,
    RawQuery(query): RawQuery,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response<Body> {
    invoke_key(
        State(s),
        Path(name),
        uri,
        RawQuery(query),
        method,
        headers,
        body,
    )
    .await
}

async fn invoke_public(
    State(s): State<Shared>,
    Path((branch, name)): Path<(String, String)>,
    uri: Uri,
    RawQuery(query): RawQuery,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response<Body> {
    let path = raw_tail(uri.path(), PUBLIC_PREFIX);
    public_form(
        s,
        branch,
        name,
        Incoming {
            method,
            path,
            headers,
            query,
            body,
        },
    )
    .await
}

async fn invoke_public_rest(
    State(s): State<Shared>,
    Path((branch, name, _rest)): Path<(String, String, String)>,
    uri: Uri,
    RawQuery(query): RawQuery,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response<Body> {
    invoke_public(
        State(s),
        Path((branch, name)),
        uri,
        RawQuery(query),
        method,
        headers,
        body,
    )
    .await
}

/// The caller and branch of the gateway's identity token: a data key's.
fn key_caller(s: &AppState, headers: &HeaderMap) -> Result<(Caller, Uuid), Box<Response<Body>>> {
    let claims = headers
        .get(IDENTITY_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|token| s.verifier.verify(token, AUDIENCE).ok());
    claims
        .and_then(|c| Some((Caller::from_claims(&c)?, c.data?.branch_id)))
        .ok_or_else(|| {
            Box::new(error(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "a data API key is required",
            ))
        })
}

/// The branch is the key's, never anything in the request.
async fn key_form(s: Shared, name: String, req: Incoming) -> Response<Body> {
    match key_caller(&s, &req.headers) {
        Ok((caller, branch)) => invoke(&s, branch, &name, Form::Key, caller, req).await,
        Err(resp) => *resp,
    }
}

async fn call_service(
    State(s): State<Shared>,
    Path(name): Path<String>,
    uri: Uri,
    RawQuery(query): RawQuery,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response<Body> {
    let (caller, branch) = match key_caller(&s, &headers) {
        Ok(found) => found,
        Err(resp) => return *resp,
    };
    let req = Incoming {
        method,
        path: raw_tail(uri.path(), SERVICE_PREFIX),
        headers,
        query,
        body,
    };
    crate::services::call(&s, branch, &name, caller, req).await
}

async fn call_service_rest(
    State(s): State<Shared>,
    Path((name, _rest)): Path<(String, String)>,
    uri: Uri,
    RawQuery(query): RawQuery,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Response<Body> {
    call_service(State(s), Path(name), uri, RawQuery(query), method, headers, body).await
}

async fn public_form(s: Shared, branch: String, name: String, req: Incoming) -> Response<Body> {
    let Ok(branch) = Uuid::parse_str(&branch) else {
        return not_found();
    };
    invoke(&s, branch, &name, Form::Public, Caller::Anonymous, req).await
}

async fn invoke_internal(
    State(s): State<Shared>,
    Path((branch, name)): Path<(String, String)>,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response<Body> {
    if let Err(resp) = require(&s, &headers, Peer::Worker) {
        return *resp;
    }
    let Ok(branch) = Uuid::parse_str(&branch) else {
        return not_found();
    };
    // m13b: database events come from this process's dispatcher only, never over HTTP.
    let mut headers = headers;
    headers.remove(DB_EVENT_HEADER);
    let req = Incoming {
        method: Method::POST,
        path: raw_tail(uri.path(), INTERNAL_PREFIX),
        headers,
        query: None,
        body,
    };
    invoke(&s, branch, &name, Form::Internal, Caller::Internal, req).await
}

async fn invoke(
    s: &AppState,
    branch: Uuid,
    name: &str,
    form: Form,
    caller: Caller,
    req: Incoming,
) -> Response<Body> {
    let resolution = match s.resolver.resolve(branch, name).await {
        // A service is called on /services/v1 only.
        Ok(Some(r)) if r.enabled && r.service.is_none() => r,
        Ok(_) => return not_found(),
        Err(e) => {
            tracing::warn!("{e}");
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "function lookup is unavailable; retry shortly",
            );
        }
    };
    match caller::check(resolution.auth, form, &caller) {
        Ok(()) => {}
        Err(Refusal::NotFound) => return not_found(),
        Err(Refusal::Unauthorized) => {
            return error(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "this function needs another credential",
            );
        }
    }
    let too_big = || {
        error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            "a request body is at most 6 MiB",
        )
    };
    let declared = req
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    if declared.is_some_and(|n| n > MAX_BODY_BYTES as u64) {
        return too_big();
    }
    let Ok(body) = to_bytes(req.body, MAX_BODY_BYTES).await else {
        return too_big();
    };
    let mut path_and_query = req.path;
    if let Some(q) = req.query {
        path_and_query.push('?');
        path_and_query.push_str(&q);
    }
    let mut headers = function_headers(&req.headers, &caller);
    if form == Form::Internal && is_scheduled(&body) {
        headers.insert(EVENT_HEADER, HeaderValue::from_static("scheduled"));
    } else if form == Form::Internal
        && req.headers.get(DB_EVENT_HEADER).is_some_and(|v| v == "db")
    {
        // m13b: the dispatcher's database event goes to `fetch`, marked (every HTTP caller's
        // copy of the header was stripped).
        headers.insert(DB_EVENT_HEADER, HeaderValue::from_static("db"));
    }
    let inv = Invocation {
        method: req.method,
        path_and_query,
        headers,
        body,
    };
    match s.host.invoke(&resolution, inv).await {
        Ok(mut resp) => {
            if resp.status().is_server_error() {
                let why = resp
                    .headers()
                    .get(ERROR_HEADER)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("the function answered {}", resp.status().as_u16()));
                record_error(s, branch, &resolution, why);
            }
            strip(resp.headers_mut(), false);
            resp
        }
        Err(HostError::Timeout) => {
            record_error(s, branch, &resolution, "timed out".into());
            error(
                StatusCode::GATEWAY_TIMEOUT,
                "function_timeout",
                "the function didn't answer in time",
            )
        }
        Err(HostError::Unreachable) => {
            record_error(s, branch, &resolution, "the runtime didn't answer".into());
            error(
                StatusCode::BAD_GATEWAY,
                "runtime_unavailable",
                "the functions runtime is unavailable; retry shortly",
            )
        }
    }
}

/// m13b: the dispatcher's invocations, in this process: the function's `fetch` handler as the
/// internal invoke reaches it, marked `chelabase-event: db`, body the event's JSON.
#[async_trait::async_trait]
impl crate::events::dispatch::Invoker for AppState {
    async fn deliver(
        &self,
        branch: chelabase_types::BranchId,
        function: &str,
        body: Vec<u8>,
    ) -> Result<u16, String> {
        let mut headers = HeaderMap::new();
        headers.insert(DB_EVENT_HEADER, HeaderValue::from_static("db"));
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let req = Incoming {
            method: Method::POST,
            path: format!("/{function}"),
            headers,
            query: None,
            body: Body::from(body),
        };
        let resp = invoke(self, branch.as_uuid(), function, Form::Internal, Caller::Internal, req).await;
        Ok(resp.status().as_u16())
    }
}

/// The worker's scheduled event (m7): a JSON object whose `type` is `scheduled`. The runtime
/// checks the rest of it.
fn is_scheduled(body: &[u8]) -> bool {
    #[derive(serde::Deserialize)]
    struct Event<'a> {
        #[serde(borrow, rename = "type")]
        kind: Option<std::borrow::Cow<'a, str>>,
    }
    serde_json::from_slice::<Event>(body).is_ok_and(|e| e.kind.as_deref() == Some("scheduled"))
}

/// Drops the headers that are ours or the hop's: every `x-chelabase-*` and `chelabase-*` (the
/// gateway's identity and forwarded headers, a client's forgeries, the runtime's own), the
/// hop-by-hop ones, and on a request also `host`, `authorization` and `content-length`.
pub(crate) fn strip(headers: &mut HeaderMap, request: bool) {
    let ours = |name: &HeaderName| {
        let n = name.as_str();
        n.starts_with("x-chelabase-")
            || n.starts_with("chelabase-")
            || matches!(
                n,
                "connection"
                    | "keep-alive"
                    | "proxy-connection"
                    | "te"
                    | "trailer"
                    | "transfer-encoding"
                    | "upgrade"
            )
            || (request
                && matches!(
                    n,
                    "host" | "authorization" | "content-length" | "proxy-authorization"
                ))
    };
    let drop: Vec<HeaderName> = headers.keys().filter(|n| ours(n)).cloned().collect();
    for name in drop {
        headers.remove(name);
    }
}

/// The function's request headers: the client's, cleaned, plus who called.
fn function_headers(incoming: &HeaderMap, caller: &Caller) -> HeaderMap {
    let mut out = incoming.clone();
    strip(&mut out, true);
    let single = |name: &str| {
        let mut values = incoming.get_all(name).iter();
        let v = values.next()?;
        values.next().is_none().then(|| v.clone())
    };
    if let Ok(v) = HeaderValue::from_str(&caller.header()) {
        out.insert(CALLER, v);
    }
    if caller.may_forward_key()
        && let Some(mut key) = single(FORWARDED_KEY)
    {
        key.set_sensitive(true);
        out.insert(CALLER_KEY, key);
    }
    if caller.is_app_user()
        && let Some(mut token) = single(FORWARDED_USER)
    {
        token.set_sensitive(true);
        out.insert(CALLER_TOKEN, token);
    }
    out
}

fn record_error(s: &AppState, branch: Uuid, r: &Resolution, why: String) {
    let mut why = why;
    if why.len() > LAST_ERROR_MAX {
        let mut cut = LAST_ERROR_MAX;
        while !why.is_char_boundary(cut) {
            cut -= 1;
        }
        why.truncate(cut);
    }
    let key = (branch, r.function_id);
    if s.recent_errors.get(&key).as_deref() == Some(why.as_str()) {
        return;
    }
    s.recent_errors.insert(key, why.clone());
    let directory = s.resolver.directory().clone();
    let function = r.function_id;
    tokio::spawn(async move {
        if let Err(e) = directory.record_error(branch, function, &why).await {
            tracing::warn!(%branch, %function, "couldn't record the function's last error: {e}");
        }
    });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Peer {
    Orchestrator,
    Worker,
}

/// The bearer must be `want`'s: no or an unknown bearer is 401, the other peer's 403.
fn require(s: &AppState, headers: &HeaderMap, want: Peer) -> Result<(), Box<Response<Body>>> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default()
        .as_bytes();
    let orchestrator = bool::from(bearer.ct_eq(s.orchestrator_token.as_bytes()));
    let worker = bool::from(bearer.ct_eq(s.worker_token.as_bytes()));
    let peer = match (orchestrator, worker) {
        (true, _) => Peer::Orchestrator,
        (_, true) => Peer::Worker,
        _ => {
            return Err(Box::new(error(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "a valid bearer is required",
            )));
        }
    };
    if peer == want {
        Ok(())
    } else {
        Err(Box::new(error(
            StatusCode::FORBIDDEN,
            "forbidden",
            "this bearer can't make this call",
        )))
    }
}

async fn get_bundle(
    State(s): State<Shared>,
    Path((database, sha)): Path<(String, String)>,
) -> Response<Body> {
    let Some((_, key)) = bundles::key(&database, &sha) else {
        return error(StatusCode::NOT_FOUND, "not_found", "no such bundle");
    };
    match s.bundles.get(&key).await {
        Ok(Some(body)) => ([(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "not_found", "no such bundle"),
        Err(e) => {
            tracing::warn!("bundle {key}: {e}");
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "the bundle store is unavailable",
            )
        }
    }
}

async fn put_bundle(
    State(s): State<Shared>,
    Path((database, sha)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response<Body> {
    if let Err(resp) = require(&s, &headers, Peer::Orchestrator) {
        return *resp;
    }
    let Some((database, key)) = bundles::key(&database, &sha) else {
        return error(
            StatusCode::NOT_FOUND,
            "not_found",
            "no such database or bundle",
        );
    };
    let Ok(body) = to_bytes(body, MAX_BUNDLE_BYTES).await else {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            "a bundle is at most 10 MiB",
        );
    };
    match s
        .bundles
        .put(s.resolver.directory().as_ref(), database, &sha, &key, body)
        .await
    {
        Ok(Put::Stored) => StatusCode::NO_CONTENT.into_response(),
        Ok(Put::Mismatch) => error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "sha256_mismatch",
            "the bundle doesn't hash to the sha256 in the path",
        ),
        Ok(Put::NoDatabase) => error(
            StatusCode::NOT_FOUND,
            "not_found",
            "no such database or bundle",
        ),
        Err(e) => {
            tracing::warn!("store bundle {key}: {e}");
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "the bundle store is unavailable",
            )
        }
    }
}
