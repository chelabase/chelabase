//! Where functions run: [`FunctionHost`], with one implementation, [`Workerd`] (the runtime in
//! `infra/functions-runtime`). The Cloudflare deployer adds another later.

use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, Response};
use bytes::Bytes;
use futures_util::StreamExt;

use crate::directory::Resolution;

/// The runtime's request headers (`infra/functions-runtime/supervisor.js`).
pub const BUNDLE_HEADER: &str = "x-chelabase-bundle";
pub const ENV_HEADER: &str = "x-chelabase-env";
/// Set on the worker's invocations only: `scheduled` (m7), the runtime runs the bundle's
/// `scheduled` handler with the body's `cron` and `scheduledTime`; `storage` (m14), it hands
/// the body's `event` to `fetch`, marked `chelabase-event: storage`.
pub const EVENT_HEADER: &str = "x-chelabase-event";
/// Why the runtime refused (a bundle that can't start, a bad manifest): the `last_error`.
pub const ERROR_HEADER: &str = "x-chelabase-error";

/// A request for a function: headers already cleaned and the caller's added.
pub struct Invocation {
    pub method: Method,
    /// `/{name}[/{rest}][?query]`: what the function sees as its path.
    pub path_and_query: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

#[derive(Debug, PartialEq, Eq)]
pub enum HostError {
    /// No response head within the timeout (504).
    Timeout,
    /// The runtime didn't answer (502): down, restarting (the busy-loop watchdog) or refusing.
    Unreachable,
}

#[async_trait::async_trait]
pub trait FunctionHost: Send + Sync + 'static {
    /// Runs the function. The response body streams; it is cut off at the deadline.
    async fn invoke(
        &self,
        resolution: &Resolution,
        invocation: Invocation,
    ) -> Result<Response<Body>, HostError>;
}

pub struct Workerd {
    client: chelabase_http::Client,
    base: String,
    timeout: Duration,
}

impl Workerd {
    pub fn new(runtime_url: &str, timeout: Duration) -> anyhow::Result<Self> {
        // No redirects (chelabase_http): the runtime's answer goes back as it came.
        let client = chelabase_http::client_builder()
            .connect_timeout(Duration::from_secs(2))
            .build()?;
        Ok(Self {
            client,
            base: runtime_url.trim_end_matches('/').to_owned(),
            timeout,
        })
    }
}

#[async_trait::async_trait]
impl FunctionHost for Workerd {
    async fn invoke(&self, r: &Resolution, inv: Invocation) -> Result<Response<Body>, HostError> {
        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut headers = inv.headers;
        let bundle = format!("{}/{}", r.database_id, r.bundle_sha256);
        headers.insert(
            BUNDLE_HEADER,
            HeaderValue::from_str(&bundle).map_err(|_| HostError::Unreachable)?,
        );
        let mut env = HeaderValue::from_str(&r.env_header).map_err(|_| HostError::Unreachable)?;
        env.set_sensitive(true);
        headers.insert(ENV_HEADER, env);
        let request = self
            .client
            .request(inv.method, format!("{}{}", self.base, inv.path_and_query))
            .headers(headers)
            .body(inv.body);
        let response = match tokio::time::timeout_at(deadline, request.send()).await {
            Err(_) => return Err(HostError::Timeout),
            Ok(Err(e)) if e.is_timeout() => return Err(HostError::Timeout),
            Ok(Err(e)) => {
                tracing::warn!("the functions runtime didn't answer: {}", e.without_url());
                return Err(HostError::Unreachable);
            }
            Ok(Ok(response)) => response,
        };
        let mut out = Response::builder().status(response.status());
        if let Some(h) = out.headers_mut() {
            *h = response.headers().clone();
        }
        // Past the deadline the body ends (the caller sees a truncated response).
        let stream = response
            .bytes_stream()
            .take_until(tokio::time::sleep_until(deadline))
            .map(|chunk| chunk.map_err(std::io::Error::other));
        out.body(Body::from_stream(stream))
            .map_err(|_| HostError::Unreachable)
    }
}
