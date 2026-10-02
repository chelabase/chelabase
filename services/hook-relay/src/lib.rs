//! Hook relay: forwards the shared storage controller's compute-notification hooks to the
//! worktree slot that knows the tenant.
//!
//! The storage controller has one `--control-plane-url`, but each worktree slot runs its
//! own orchestrator internal listener (`8083 + 100 * slot`). For every request the relay
//! re-reads the slot registry directory and tries each registered slot in slot order with
//! the same method, path, query, headers and body. The first answer that is not 404 wins;
//! all 404 (or unreachable) is 404. Request and response bodies, and the bearer, are never
//! logged.
//!
//! Trust model: whoever can write `.cheladb/slots/*.env` can point the hook bearer at
//! arbitrary local ports, which is the same trust as the dev checkout itself. The relay
//! must therefore not be published beyond the Compose network (the Compose wiring must not
//! map its port to the host).

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::State;
use axum::http::{HeaderMap, HeaderName, Method, Response, StatusCode, Uri, header};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Hooks carry small JSON documents; anything bigger is refused.
const MAX_BODY: usize = 4 * 1024 * 1024;
const PORT_BASE: u32 = 8083;
const PORT_STEP: u32 = 100;

#[derive(Clone, Debug)]
pub struct RelayConfig {
    /// Directory of `<N>.env` registry files, re-read on every request.
    pub slots_dir: PathBuf,
    /// Host the slots' internal listeners are reached on.
    pub upstream_host: String,
    /// Internal port of slot 0 (no registry file for the main checkout); `None` disables it.
    /// A `0.env` file in the registry overrides it.
    pub slot0_port: Option<u16>,
    /// Total time one slot may take to answer (connect included). Default 5 s.
    pub slot_timeout: Duration,
    /// Overall deadline for one request across all slots; 504 when it runs out. Default 10 s.
    pub deadline: Duration,
}

pub const DEFAULT_SLOT_TIMEOUT: Duration = Duration::from_secs(5);
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    pub slot: u32,
    pub port: u16,
}

fn parse_slot(text: &str, file_slot: u32) -> Option<Slot> {
    let (mut slot, mut port) = (None, None);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim().trim_matches(['"', '\'']);
        match k.trim() {
            "CHELADB_SLOT" => slot = v.parse::<u32>().ok(),
            "CHELADB_INTERNAL_PORT" => port = v.parse::<u16>().ok(),
            _ => {}
        }
    }
    let slot = slot.unwrap_or(file_slot);
    let port = match port {
        Some(p) => Some(p),
        None => PORT_STEP
            .checked_mul(slot)
            .and_then(|x| x.checked_add(PORT_BASE))
            .and_then(|x| u16::try_from(x).ok()),
    };
    match port {
        Some(p) if p >= 1024 => Some(Slot { slot, port: p }),
        _ => {
            tracing::warn!(
                slot,
                ?port,
                "slot port overflows or is outside 1024..=65535, skipped"
            );
            None
        }
    }
}

/// The registered slots in ascending slot order (numeric), plus implicit slot 0.
/// Unreadable or malformed files are skipped.
pub fn read_slots(cfg: &RelayConfig) -> Vec<Slot> {
    let mut slots: Vec<Slot> = Vec::new();
    if let Ok(dir) = std::fs::read_dir(&cfg.slots_dir) {
        for entry in dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("env") {
                continue;
            }
            let Some(n) = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            match parse_slot(&text, n) {
                Some(s) => slots.push(s),
                None => tracing::warn!(file = %path.display(), "malformed slot file, skipped"),
            }
        }
    }
    if let Some(port) = cfg.slot0_port
        && !slots.iter().any(|s| s.slot == 0)
    {
        slots.push(Slot { slot: 0, port });
    }
    slots.sort_by_key(|s| s.slot);
    slots.dedup_by_key(|s| s.slot);
    slots
}

struct Inner {
    cfg: RelayConfig,
    client: reqwest::Client,
}

pub fn app(cfg: RelayConfig) -> Router {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(2))
        .timeout(cfg.slot_timeout)
        // Slots are plain HTTP and the image has no CA bundle: trust no roots at all, so the
        // build never reads the system store (and any https URL would fail verification).
        .tls_certs_only(std::iter::empty())
        .build()
        .expect("reqwest client");
    Router::new()
        .fallback(relay)
        .with_state(Arc::new(Inner { cfg, client }))
}

fn hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "host"
            | "content-length"
    )
}

async fn relay(
    State(st): State<Arc<Inner>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Body,
) -> Response<Body> {
    let body: Bytes = match to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(_) => return status(StatusCode::PAYLOAD_TOO_LARGE),
    };
    // Only the storage controller's hooks are relayed; anything else is not ours to forward.
    let path = uri.path();
    if !path.starts_with("/storcon/") {
        return status(StatusCode::NOT_FOUND);
    }
    let slots = read_slots(&st.cfg);
    match tokio::time::timeout(
        st.cfg.deadline,
        try_slots(&st, &slots, &method, &uri, &headers, body),
    )
    .await
    {
        Ok(Some(resp)) => resp,
        Ok(None) => {
            tracing::info!(%method, path = uri.path(), slots = slots.len(), "no slot knows this request");
            Response::builder()
                .status(StatusCode::NOT_FOUND)
                .header(header::CONTENT_TYPE, "text/plain")
                .body(Body::from("no slot knows this request\n"))
                .unwrap()
        }
        Err(_) => {
            tracing::warn!(%method, path = uri.path(), "request deadline exceeded");
            status(StatusCode::GATEWAY_TIMEOUT)
        }
    }
}

/// The first non-404 answer in slot order; `None` when every slot said 404 or was unreachable.
async fn try_slots(
    st: &Inner,
    slots: &[Slot],
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: Bytes,
) -> Option<Response<Body>> {
    let path_q = uri.path_and_query().map_or("/", |p| p.as_str());
    for slot in slots {
        let url = format!("http://{}:{}{}", st.cfg.upstream_host, slot.port, path_q);
        let mut req = st.client.request(method.clone(), &url).body(body.clone());
        for (k, v) in headers {
            if !hop_by_hop(k) {
                req = req.header(k, v);
            }
        }
        match req.send().await {
            Ok(resp) if resp.status() == StatusCode::NOT_FOUND => {
                tracing::debug!(%method, path = uri.path(), slot = slot.slot, "slot answered 404");
            }
            Ok(resp) => {
                tracing::info!(%method, path = uri.path(), slot = slot.slot, status = resp.status().as_u16(), "relayed");
                let mut out = Response::builder().status(resp.status());
                for (k, v) in resp.headers() {
                    if !hop_by_hop(k) {
                        out = out.header(k, v);
                    }
                }
                return Some(match resp.bytes().await {
                    Ok(b) => out
                        .body(Body::from(b))
                        .unwrap_or_else(|_| status(StatusCode::BAD_GATEWAY)),
                    Err(_) => status(StatusCode::BAD_GATEWAY),
                });
            }
            Err(e) if e.is_connect() => {
                tracing::warn!(%method, path = uri.path(), slot = slot.slot, error = %e.without_url(), "slot unreachable, skipped");
            }
            // The slot took the request and then went quiet: it may own the tenant, so the
            // next slot must not answer for it. The storage controller retries.
            Err(e) => {
                let timed_out = e.is_timeout();
                tracing::warn!(%method, path = uri.path(), slot = slot.slot, error = %e.without_url(), "slot failed after connecting");
                return Some(status(if timed_out {
                    StatusCode::GATEWAY_TIMEOUT
                } else {
                    StatusCode::BAD_GATEWAY
                }));
            }
        }
    }
    None
}

fn status(code: StatusCode) -> Response<Body> {
    Response::builder()
        .status(code)
        .body(Body::empty())
        .unwrap()
}
