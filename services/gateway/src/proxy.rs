//! Pingora adapter. Only this module and `main.rs` touch Pingora types; the
//! decisions live in `pipeline`.

use crate::pipeline::{GatewayResponse, Outcome, Pipeline, ProxyPlan, RequestParts, REQUEST_ID_HEADER};
use crate::upstreams::Upstreams;
use async_trait::async_trait;
use bytes::Bytes;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_error::{Error, ErrorSource, ErrorType, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{FailToProxy, ProxyHttp, Session};
use serde_json::json;
use std::net::SocketAddr;
use std::time::{Duration, Instant};
use tracing::info;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

pub struct Gateway {
    pipeline: Pipeline,
    upstreams: Upstreams,
    read_timeout: Duration,
}

impl Gateway {
    pub fn new(pipeline: Pipeline, upstreams: Upstreams, read_timeout: Duration) -> Self {
        Self {
            pipeline,
            upstreams,
            read_timeout,
        }
    }
}

pub struct RequestCtx {
    started: Instant,
    request_id: String,
    host: Option<String>,
    plan: Option<ProxyPlan>,
    /// Resolved upstream addresses, and the one currently being tried.
    candidates: Vec<SocketAddr>,
    attempt: usize,
}

#[async_trait]
impl ProxyHttp for Gateway {
    type CTX = RequestCtx;

    fn new_ctx(&self) -> RequestCtx {
        RequestCtx {
            started: Instant::now(),
            request_id: String::new(),
            host: None,
            plan: None,
            candidates: Vec::new(),
            attempt: 0,
        }
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut RequestCtx) -> Result<bool> {
        // Copy what the pipeline needs so no borrow of `session` is held across `.await`.
        let req = session.req_header();
        let method = req.method.as_str().to_owned();
        let path = req.uri.path().to_owned();
        let headers = req.headers.clone();
        let host = headers
            .get(http::header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .or_else(|| req.uri.authority().map(|a| a.as_str().to_string()));

        let client_ip = session
            .client_addr()
            .and_then(|addr| addr.as_inet())
            .map(|inet| inet.ip());

        let handled = self
            .pipeline
            .handle(&RequestParts {
                method: &method,
                path: &path,
                host: host.as_deref(),
                headers: &headers,
                client_ip,
            })
            .await;

        ctx.request_id = handled.request_id;
        ctx.host = host;
        match handled.outcome {
            Outcome::Proxy(plan) => {
                ctx.plan = Some(plan);
                Ok(false)
            }
            Outcome::Respond(resp) => {
                write_gateway_response(session, &resp, &ctx.request_id).await?;
                Ok(true)
            }
        }
    }

    async fn upstream_peer(&self, _session: &mut Session, ctx: &mut RequestCtx) -> Result<Box<HttpPeer>> {
        let plan = ctx
            .plan
            .as_ref()
            .ok_or_else(|| Error::explain(ErrorType::InternalError, "no proxy plan"))?;
        if ctx.candidates.is_empty() {
            let addr = self.upstreams.addr(plan.endpoint.service);
            ctx.candidates = resolve_upstream(addr).await;
            if ctx.candidates.is_empty() {
                return Err(Error::explain(
                    ErrorType::ConnectError,
                    format!("resolve upstream {addr}"),
                ));
            }
        }
        let socket = ctx.candidates[ctx.attempt.min(ctx.candidates.len() - 1)];
        let mut peer = HttpPeer::new(socket, false, String::new());
        peer.options.connection_timeout = Some(CONNECT_TIMEOUT);
        peer.options.read_timeout = Some(self.read_timeout);
        Ok(Box::new(peer))
    }

    /// A hostname can resolve to several addresses (e.g. `localhost` → `::1`,
    /// `127.0.0.1`); on a connect failure retry with the next one.
    fn fail_to_connect(
        &self,
        _session: &mut Session,
        _peer: &HttpPeer,
        ctx: &mut RequestCtx,
        mut e: Box<Error>,
    ) -> Box<Error> {
        if let Some(next) = next_attempt(ctx.attempt, ctx.candidates.len()) {
            ctx.attempt = next;
            e.set_retry(true);
        }
        e
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream: &mut RequestHeader,
        ctx: &mut RequestCtx,
    ) -> Result<()> {
        let plan = ctx
            .plan
            .as_ref()
            .ok_or_else(|| Error::explain(ErrorType::InternalError, "no proxy plan"))?;
        let edits = plan.upstream_headers(&ctx.request_id);
        for name in edits.remove {
            upstream.remove_header(*name);
        }
        for (name, value) in edits.set {
            upstream.insert_header(name, value)?;
        }

        // X-Forwarded-For is appended, never trusted: it carries whatever the
        // client (or an operator proxy) sent before us. Don't authorize on it.
        if let Some(ip) = session
            .client_addr()
            .and_then(|a| a.as_inet())
            .map(|a| a.ip().to_string())
        {
            let forwarded_for = match upstream
                .headers
                .get("x-forwarded-for")
                .and_then(|v| v.to_str().ok())
            {
                Some(previous) => format!("{previous}, {ip}"),
                None => ip,
            };
            upstream.insert_header("x-forwarded-for", forwarded_for)?;
        }
        // Keep a scheme set by an operator's TLS-terminating proxy; the gateway
        // itself only speaks plain HTTP.
        if !upstream.headers.contains_key("x-forwarded-proto") {
            upstream.insert_header("x-forwarded-proto", "http")?;
        }
        if let Some(host) = &ctx.host {
            upstream.insert_header("x-forwarded-host", host.as_str())?;
        }
        Ok(())
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        response: &mut ResponseHeader,
        ctx: &mut RequestCtx,
    ) -> Result<()> {
        response.insert_header(REQUEST_ID_HEADER, ctx.request_id.as_str())?;
        if let Some(plan) = &ctx.plan {
            for (name, value) in &plan.response_headers {
                if name.eq_ignore_ascii_case("vary") {
                    merge_vary(response, value)?;
                } else {
                    response.insert_header(*name, value.as_str())?;
                }
            }
        }
        Ok(())
    }

    async fn fail_to_proxy(&self, session: &mut Session, e: &Error, ctx: &mut RequestCtx) -> FailToProxy {
        let status = failure_status(e.esource(), e.etype());
        // Once the upstream response has started (headers sent, or a websocket
        // upgraded), writing our own error would splice JSON into it. Let the
        // connection close so the client sees a truncated response instead.
        if status > 0 && session.response_written().is_none() {
            let message = match status {
                502 => "upstream unavailable",
                504 => "upstream timed out",
                400 => "bad request",
                408 => "request timed out",
                _ => "internal error",
            };
            let headers = ctx
                .plan
                .as_ref()
                .map(|p| p.response_headers.clone())
                .unwrap_or_default();
            let resp = GatewayResponse {
                status,
                headers,
                body: Some(json!({ "error": message })),
            };
            let _ = write_gateway_response(session, &resp, &ctx.request_id).await;
        }
        FailToProxy {
            error_code: status,
            can_reuse_downstream: false,
        }
    }

    async fn logging(&self, session: &mut Session, _e: Option<&Error>, ctx: &mut RequestCtx) {
        let req = session.req_header();
        let status = session.response_written().map_or(0, |r| r.status.as_u16());
        let (route, service) = ctx
            .plan
            .as_ref()
            .map_or(("-", "-"), |p| (p.endpoint.name, p.endpoint.service.name()));
        info!(
            request_id = %ctx.request_id,
            method = %req.method,
            host = ctx.host.as_deref().unwrap_or("-"),
            path = req.uri.path(),
            route,
            service,
            status,
            duration_ms = ctx.started.elapsed().as_millis() as u64,
            "request"
        );
    }
}

/// Add `value` to the response's `Vary` unless an existing entry already covers it.
fn merge_vary(response: &mut ResponseHeader, value: &str) -> Result<()> {
    let already = response.headers.get_all("vary").iter().any(|v| {
        v.to_str().is_ok_and(|list| {
            list.split(',')
                .map(str::trim)
                .any(|item| item == "*" || item.eq_ignore_ascii_case(value))
        })
    });
    if !already {
        response.append_header("vary", value)?;
    }
    Ok(())
}

async fn write_gateway_response(session: &mut Session, resp: &GatewayResponse, request_id: &str) -> Result<()> {
    let body = resp.body.as_ref().map(|b| Bytes::from(b.to_string()));
    let mut header = ResponseHeader::build(resp.status, None)?;
    header.insert_header(REQUEST_ID_HEADER, request_id)?;
    for (name, value) in &resp.headers {
        header.insert_header(*name, value.as_str())?;
    }
    match &body {
        Some(b) => {
            header.insert_header("content-type", "application/json")?;
            header.set_content_length(b.len())?;
        }
        // RFC 9110 §8.6: a 204 (or 1xx) response must not carry Content-Length.
        None if resp.status == 204 || resp.status < 200 => {}
        None => header.set_content_length(0)?,
    }
    session
        .write_response_header(Box::new(header), body.is_none())
        .await?;
    if let Some(b) = body {
        session.write_response_body(Some(b), true).await?;
    }
    Ok(())
}

/// Resolve `host:port` without blocking the runtime; every address, in
/// resolver order.
async fn resolve_upstream(addr: &str) -> Vec<SocketAddr> {
    match tokio::net::lookup_host(addr).await {
        Ok(addrs) => addrs.collect(),
        Err(_) => Vec::new(),
    }
}

/// After a connect failure on candidate `attempt`, the next one to try.
fn next_attempt(attempt: usize, candidates: usize) -> Option<usize> {
    let next = attempt + 1;
    (next < candidates).then_some(next)
}

/// HTTP status for a proxying failure; 0 means the client is gone and
/// nothing should be written.
fn failure_status(source: &ErrorSource, etype: &ErrorType) -> u16 {
    match (source, etype) {
        (
            ErrorSource::Downstream,
            ErrorType::ReadError | ErrorType::WriteError | ErrorType::ConnectionClosed,
        ) => 0,
        (ErrorSource::Downstream, ErrorType::ReadTimedout) => 408,
        (ErrorSource::Downstream, _) => 400,
        (_, ErrorType::HTTPStatus(code)) => *code,
        (_, ErrorType::ConnectTimedout | ErrorType::ReadTimedout | ErrorType::WriteTimedout) => 504,
        (_, ErrorType::ConnectRefused | ErrorType::ConnectNoRoute | ErrorType::ConnectError) => 502,
        (ErrorSource::Upstream, _) => 502,
        _ => 500,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolves_hostnames_to_socket_addresses() {
        let addrs = resolve_upstream("localhost:8081").await;
        assert!(
            addrs.contains(&"127.0.0.1:8081".parse().unwrap()),
            "{addrs:?}"
        );
    }

    #[tokio::test]
    async fn unresolvable_host_yields_no_candidates() {
        assert!(resolve_upstream("no-such-host.invalid:8081").await.is_empty());
    }

    #[test]
    fn connect_failures_move_through_every_candidate_once() {
        assert_eq!(next_attempt(0, 2), Some(1));
        assert_eq!(next_attempt(1, 2), None);
        assert_eq!(next_attempt(0, 1), None);
        assert_eq!(next_attempt(0, 0), None);
    }

    #[test]
    fn client_gone_writes_nothing() {
        for etype in [ErrorType::ReadError, ErrorType::WriteError, ErrorType::ConnectionClosed] {
            assert_eq!(failure_status(&ErrorSource::Downstream, &etype), 0);
        }
    }

    #[test]
    fn slow_client_is_408() {
        assert_eq!(failure_status(&ErrorSource::Downstream, &ErrorType::ReadTimedout), 408);
    }

    #[test]
    fn other_client_errors_are_400() {
        assert_eq!(failure_status(&ErrorSource::Downstream, &ErrorType::InvalidHTTPHeader), 400);
    }

    #[test]
    fn upstream_timeouts_are_504() {
        for etype in [ErrorType::ConnectTimedout, ErrorType::ReadTimedout, ErrorType::WriteTimedout] {
            assert_eq!(failure_status(&ErrorSource::Upstream, &etype), 504);
        }
    }

    #[test]
    fn upstream_connect_failures_are_502() {
        for etype in [ErrorType::ConnectRefused, ErrorType::ConnectNoRoute, ErrorType::ConnectError] {
            assert_eq!(failure_status(&ErrorSource::Unset, &etype), 502);
        }
        assert_eq!(failure_status(&ErrorSource::Upstream, &ErrorType::H1Error), 502);
    }

    #[test]
    fn explicit_status_is_kept_and_internal_errors_are_500() {
        assert_eq!(failure_status(&ErrorSource::Internal, &ErrorType::HTTPStatus(413)), 413);
        assert_eq!(failure_status(&ErrorSource::Internal, &ErrorType::InternalError), 500);
    }
}
