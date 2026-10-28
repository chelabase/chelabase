//! Pingora adapter. Only this module and `main.rs` touch Pingora types; the
//! decisions live in `pipeline`.

use crate::client_addr::{client_address, TrustedProxies};
use crate::pipeline::{error_json, GatewayResponse, Outcome, Pipeline, ProxyPlan, RequestParts, REQUEST_ID_HEADER};
use crate::registry::Site;
use crate::upstreams::Upstreams;
use async_trait::async_trait;
use bytes::Bytes;
use chelabase_types::entry::Scheme;
use pingora_core::upstreams::peer::HttpPeer;
use pingora_error::{Error, ErrorSource, ErrorType, Result};
use pingora_http::{RequestHeader, ResponseHeader};
use pingora_proxy::{FailToProxy, ProxyHttp, Session};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};
use tracing::info;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// A site websocket's idle limit, both ways: long enough for a quiet socket (Vite's hot
/// reload). A site event stream gets it as its upstream read timeout too. Every other
/// request, API or site, uses the configured upstream read timeout.
const WEBSOCKET_READ_TIMEOUT: Duration = Duration::from_secs(3600);

pub struct Gateway {
    pipeline: Pipeline,
    upstreams: Upstreams,
    /// The upstream read timeout of every request but a site websocket or event stream
    /// ([`WEBSOCKET_READ_TIMEOUT`]).
    read_timeout: Duration,
    /// `X-Forwarded-Proto`: the public scheme (`CHELABASE_PUBLIC_SCHEME`), never the client's.
    forwarded_proto: &'static str,
    /// Peers whose forwarded client address is believed (`CHELABASE_GATEWAY_TRUSTED_PROXIES`).
    trusted_proxies: TrustedProxies,
}

impl Gateway {
    pub fn new(pipeline: Pipeline, upstreams: Upstreams, read_timeout: Duration, scheme: Scheme) -> Self {
        Self {
            pipeline,
            upstreams,
            read_timeout,
            forwarded_proto: match scheme {
                Scheme::Http => "http",
                Scheme::Https => "https",
            },
            trusted_proxies: TrustedProxies::default(),
        }
    }

    /// Believe the forwarded client address from these peers (none by default).
    pub fn with_trusted_proxies(mut self, trusted_proxies: TrustedProxies) -> Self {
        self.trusted_proxies = trusted_proxies;
        self
    }
}

pub struct RequestCtx {
    started: Instant,
    request_id: String,
    host: Option<String>,
    plan: Option<ProxyPlan>,
    /// Set when the host is a pass-through site.
    site: Option<Site>,
    /// Resolved upstream addresses, and the one currently being tried.
    candidates: Vec<SocketAddr>,
    attempt: usize,
    /// The client's address ([`client_address`]), set by `request_filter`.
    client_ip: Option<IpAddr>,
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
            site: None,
            candidates: Vec::new(),
            attempt: 0,
            client_ip: None,
        }
    }

    async fn request_filter(&self, session: &mut Session, ctx: &mut RequestCtx) -> Result<bool> {
        // Copy what the pipeline needs so no borrow of `session` is held across `.await`.
        let req = session.req_header();
        let method = req.method.as_str().to_owned();
        let path = req.uri.path().to_owned();
        let headers = req.headers.clone();
        let host = request_host(req);

        // The TCP peer, or the address a trusted reverse proxy forwarded for it.
        let peer = session
            .client_addr()
            .and_then(|addr| addr.as_inet())
            .map(|inet| inet.ip());
        let client_ip = client_address(peer, &self.trusted_proxies, &headers);
        ctx.client_ip = client_ip;

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
                // W17: only a websocket route takes an upgrade, and only a websocket one, and
                // only on HTTP/1.1. Pingora's `is_upgrade_req` looks at HTTP/1.1 alone, so the
                // `Upgrade` header is checked on every version: on HTTP/1.0 it is refused,
                // never forwarded.
                //
                // The gateway serves HTTP/1 only. HTTP/2 has a second way to open a socket,
                // extended CONNECT (RFC 8441: `:method CONNECT` with a `:protocol` pseudo-header,
                // no `Upgrade` header at all), which this check doesn't see; enabling h2 must
                // refuse extended CONNECT here on every non-websocket route.
                if asks_upgrade(session.req_header()) {
                    let req = session.req_header();
                    if !plan.endpoint.websocket
                        || req.version != http::Version::HTTP_11
                        || !is_websocket_upgrade(&req.headers)
                    {
                        // The route's CORS headers, so Studio's browser can read the 400.
                        let resp = GatewayResponse {
                            status: 400,
                            headers: plan.response_headers.clone(),
                            body: Some(error_json("invalid_request", "this route does not take a protocol upgrade")),
                        };
                        write_gateway_response(session, &resp, &ctx.request_id).await?;
                        return Ok(true);
                    }
                    // Both directions of the socket may sit idle up to an hour (S18); the
                    // client side is read through the body reader, whose default is 60 s.
                    session.set_read_timeout(Some(WEBSOCKET_READ_TIMEOUT));
                }
                ctx.plan = Some(plan);
                Ok(false)
            }
            Outcome::Site(site) => {
                ctx.site = Some(site);
                // A site takes any upgrade, but only on HTTP/1.1 (Pingora would forward an
                // HTTP/1.0 `Upgrade` without treating the connection as upgraded).
                if asks_upgrade(session.req_header()) && session.req_header().version != http::Version::HTTP_11 {
                    write_plain_response(session, 400, &ctx.request_id).await?;
                    return Ok(true);
                }
                // An upgraded connection (a websocket) reads the client through the body
                // reader, whose default idle limit is 60 s.
                if session.is_upgrade_req() {
                    session.set_read_timeout(Some(WEBSOCKET_READ_TIMEOUT));
                }
                Ok(false)
            }
            Outcome::Respond(resp) => {
                write_gateway_response(session, &resp, &ctx.request_id).await?;
                Ok(true)
            }
        }
    }

    async fn upstream_peer(&self, session: &mut Session, ctx: &mut RequestCtx) -> Result<Box<HttpPeer>> {
        let (addr, read_timeout) = match (ctx.site, &ctx.plan) {
            (Some(site), _) => {
                let addr = self.upstreams.site_addr(site).ok_or_else(|| {
                    Error::explain(ErrorType::ConnectError, format!("no upstream for the {} site", site.name()))
                })?;
                // Spec §2: a websocket or an event stream may sit idle up to an hour; any
                // other site request that gets no answer times out like the API's (504).
                let timeout = site_read_timeout(
                    session.is_upgrade_req(),
                    &session.req_header().headers,
                    self.read_timeout,
                );
                (addr, timeout)
            }
            (None, Some(plan)) => {
                let timeout = if plan.endpoint.websocket && session.is_upgrade_req() {
                    WEBSOCKET_READ_TIMEOUT
                } else {
                    self.read_timeout
                };
                (self.upstreams.addr(plan.endpoint.service), timeout)
            }
            (None, None) => return Err(Error::explain(ErrorType::InternalError, "no proxy plan")),
        };
        if ctx.candidates.is_empty() {
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
        peer.options.read_timeout = Some(read_timeout);
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
        _session: &mut Session,
        upstream: &mut RequestHeader,
        ctx: &mut RequestCtx,
    ) -> Result<()> {
        // A site gets the client's request as it came (Host, Authorization, cookies,
        // Upgrade/Connection); the API gets the plan's edits.
        if ctx.site.is_none() {
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
        }

        // P4, on every target: the forwarded headers are the gateway's own, never the client's
        // (`X-Forwarded-For` is the one address `request_filter` settled on).
        for (name, value) in forwarded_headers(self.forwarded_proto, ctx.client_ip, ctx.host.as_deref()) {
            match value {
                Some(value) => upstream.insert_header(name, value)?,
                None => {
                    upstream.remove_header(name);
                }
            }
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
        let can_answer = status > 0 && session.response_written().is_none();
        if can_answer && ctx.site.is_some() {
            // A site is a web page, not the API: a plain-text error, no JSON.
            let _ = write_plain_response(session, status, &ctx.request_id).await;
        } else if can_answer {
            let (code, message) = match status {
                502 => ("unavailable", "upstream unavailable"),
                504 => ("timeout", "upstream timed out"),
                400 => ("invalid_request", "bad request"),
                408 => ("timeout", "request timed out"),
                413 => ("too_large", "request body too large"),
                _ => ("internal", "internal error"),
            };
            let headers = ctx
                .plan
                .as_ref()
                .map(|p| p.response_headers.clone())
                .unwrap_or_default();
            let resp = GatewayResponse {
                status,
                headers,
                body: Some(error_json(code, message)),
            };
            let _ = write_gateway_response(session, &resp, &ctx.request_id).await;
        }
        FailToProxy {
            error_code: status,
            can_reuse_downstream: false,
        }
    }

    /// W16: what pingora puts in its error logs. The default prints the request line, query
    /// included, and the realtime ticket travels in the query; this is method, path, host.
    fn request_summary(&self, session: &Session, _ctx: &RequestCtx) -> String {
        let req = session.req_header();
        let host = request_host(req);
        format!("{} {}, Host: {}", req.method, req.uri.path(), host.as_deref().unwrap_or("-"))
    }

    async fn logging(&self, session: &mut Session, _e: Option<&Error>, ctx: &mut RequestCtx) {
        let req = session.req_header();
        let status = session.response_written().map_or(0, |r| r.status.as_u16());
        let (route, service) = match (ctx.site, &ctx.plan) {
            (Some(site), _) => ("-", site.name()),
            (None, Some(p)) => (p.endpoint.name, p.endpoint.service.name()),
            (None, None) => ("-", "-"),
        };
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

/// The request's host: its `Host` header, else an absolute-form target's authority.
fn request_host(req: &RequestHeader) -> Option<String> {
    req.headers
        .get(http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| req.uri.authority().map(|a| a.as_str().to_string()))
}

/// Whether the request carries an `Upgrade` header, on any HTTP version.
fn asks_upgrade(req: &RequestHeader) -> bool {
    req.headers.contains_key(http::header::UPGRADE)
}

/// Whether the request's `Upgrade` header asks for exactly a websocket.
fn is_websocket_upgrade(headers: &http::HeaderMap) -> bool {
    let mut values = headers.get_all(http::header::UPGRADE).iter();
    match (values.next(), values.next()) {
        (Some(v), None) => v.to_str().is_ok_and(|v| v.trim().eq_ignore_ascii_case("websocket")),
        _ => false,
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

/// A site's own error page: `<status> <reason>\n` as `text/plain`.
async fn write_plain_response(session: &mut Session, status: u16, request_id: &str) -> Result<()> {
    let body = Bytes::from(plain_error_body(status));
    let mut header = ResponseHeader::build(status, None)?;
    header.insert_header(REQUEST_ID_HEADER, request_id)?;
    header.insert_header("content-type", "text/plain; charset=utf-8")?;
    header.set_content_length(body.len())?;
    session.write_response_header(Box::new(header), false).await?;
    session.write_response_body(Some(body), true).await?;
    Ok(())
}

fn plain_error_body(status: u16) -> String {
    let reason = http::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("error")
        .to_ascii_lowercase();
    format!("{status} {reason}\n")
}

/// P4: `X-Forwarded-Proto` (the public scheme), `-For` (the client's address only, see
/// [`client_address`]) and `-Host` (the request's `Host`), each replacing whatever the client
/// sent; None removes the header.
fn forwarded_headers(
    proto: &'static str,
    client_ip: Option<IpAddr>,
    host: Option<&str>,
) -> [(&'static str, Option<String>); 3] {
    [
        ("x-forwarded-proto", Some(proto.to_string())),
        ("x-forwarded-for", client_ip.map(|ip| ip.to_string())),
        ("x-forwarded-host", host.map(str::to_string)),
    ]
}

/// Resolve `host:port` without blocking the runtime; every address, in
/// resolver order.
async fn resolve_upstream(addr: &str) -> Vec<SocketAddr> {
    match tokio::net::lookup_host(addr).await {
        Ok(addrs) => addrs.collect(),
        Err(_) => Vec::new(),
    }
}

/// A site request's upstream read timeout: [`WEBSOCKET_READ_TIMEOUT`] for a websocket
/// upgrade or a request whose `Accept` lists `text/event-stream` (a dev server's SSE, which
/// may stay quiet for minutes); `configured` for everything else.
fn site_read_timeout(upgrade: bool, headers: &http::HeaderMap, configured: Duration) -> Duration {
    let event_stream = headers
        .get_all(http::header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|range| {
            let media = range.split(';').next().unwrap_or_default().trim();
            media.eq_ignore_ascii_case("text/event-stream")
        });
    if upgrade || event_stream {
        WEBSOCKET_READ_TIMEOUT
    } else {
        configured
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
    use crate::client_addr::{client_address, TrustedProxies, MAX_FORWARDED_BYTES, MAX_FORWARDED_HOPS};

    fn upgrade_headers(values: &[&str]) -> http::HeaderMap {
        let mut h = http::HeaderMap::new();
        for v in values {
            h.append(http::header::UPGRADE, http::HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn only_a_single_websocket_upgrade_value_is_accepted() {
        assert!(is_websocket_upgrade(&upgrade_headers(&["websocket"])));
        assert!(is_websocket_upgrade(&upgrade_headers(&["WebSocket"])));
        assert!(is_websocket_upgrade(&upgrade_headers(&[" websocket "])));
        assert!(!is_websocket_upgrade(&upgrade_headers(&["h2c"])));
        assert!(!is_websocket_upgrade(&upgrade_headers(&["websocket, h2c"])));
        assert!(!is_websocket_upgrade(&upgrade_headers(&["websocket", "h2c"])));
        assert!(!is_websocket_upgrade(&upgrade_headers(&[""])));
        assert!(!is_websocket_upgrade(&http::HeaderMap::new()));
    }

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
    fn site_errors_are_plain_text_status_lines() {
        assert_eq!(plain_error_body(502), "502 bad gateway\n");
        assert_eq!(plain_error_body(504), "504 gateway timeout\n");
    }

    #[test]
    fn forwarded_headers_are_the_gateways_own() {
        let set = forwarded_headers("https", Some("10.0.0.7".parse().unwrap()), Some("studio.acme.com"));
        assert_eq!(
            set,
            [
                ("x-forwarded-proto", Some("https".to_string())),
                ("x-forwarded-for", Some("10.0.0.7".to_string())),
                ("x-forwarded-host", Some("studio.acme.com".to_string())),
            ]
        );
        // Unknown values remove the client's rather than keep them.
        let unset = forwarded_headers("http", None, None);
        assert_eq!((unset[1].1.as_deref(), unset[2].1.as_deref()), (None, None));
    }

    // ---- m14 (#25): a trusted reverse proxy's forwarded client address ----

    fn trust(raw: &str) -> TrustedProxies {
        TrustedProxies::parse(raw).unwrap()
    }

    /// The `X-Forwarded-For` the apps see for a request from `peer` carrying `headers`.
    fn apps_see(peer: &str, trusted: &TrustedProxies, headers: &[(&str, &[u8])]) -> Option<String> {
        let mut map = http::HeaderMap::new();
        for (name, value) in headers {
            map.append(
                http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                http::HeaderValue::from_bytes(value).unwrap(),
            );
        }
        let client = client_address(Some(peer.parse().unwrap()), trusted, &map);
        let set = forwarded_headers("https", client, Some("api.acme.com"));
        assert_eq!(set[1].0, "x-forwarded-for");
        set[1].1.clone()
    }

    fn xff(value: &str) -> (&'static str, &[u8]) {
        ("x-forwarded-for", value.as_bytes())
    }

    fn fwd(value: &str) -> (&'static str, &[u8]) {
        ("forwarded", value.as_bytes())
    }

    fn seen(ip: &str) -> Option<String> {
        Some(ip.to_string())
    }

    #[test]
    fn no_trusted_proxies_is_todays_behaviour() {
        let none = TrustedProxies::default();
        assert!(none.is_empty());
        for headers in [
            &[][..],
            &[xff("1.2.3.4")],
            &[xff("1.2.3.4, 10.0.0.2")],
            &[fwd("for=1.2.3.4")],
            &[xff("1.2.3.4"), fwd("for=5.6.7.8")],
        ] {
            assert_eq!(apps_see("10.0.0.2", &none, headers), seen("10.0.0.2"), "{headers:?}");
            assert_eq!(apps_see("2001:db8::9", &none, headers), seen("2001:db8::9"), "{headers:?}");
        }
        // The peer itself, exactly one address, even with no header at all.
        assert_eq!(apps_see("203.0.113.5", &none, &[]), seen("203.0.113.5"));
    }

    #[test]
    fn a_trusted_proxy_forwards_the_client_address() {
        let t = trust("10.0.0.0/8");
        assert_eq!(apps_see("10.0.0.2", &t, &[xff("1.2.3.4, 10.0.0.2")]), seen("1.2.3.4"));
        assert_eq!(apps_see("10.0.0.2", &t, &[xff("1.2.3.4")]), seen("1.2.3.4"));
        // OWS and tabs around entries.
        assert_eq!(apps_see("10.0.0.2", &t, &[xff("  1.2.3.4\t,\t10.0.0.3 ")]), seen("1.2.3.4"));
        // A bare IP entry trusts exactly that address.
        let one = trust("192.168.1.5");
        assert_eq!(apps_see("192.168.1.5", &one, &[xff("1.2.3.4")]), seen("1.2.3.4"));
        assert_eq!(apps_see("192.168.1.6", &one, &[xff("1.2.3.4")]), seen("192.168.1.6"));
    }

    #[test]
    fn the_right_most_untrusted_address_wins_over_what_the_client_wrote() {
        let t = trust("10.0.0.0/8");
        // The client's own claim is left of what the proxy appended.
        assert_eq!(apps_see("10.0.0.2", &t, &[xff("6.6.6.6, 1.2.3.4")]), seen("1.2.3.4"));
        assert_eq!(apps_see("10.0.0.2", &t, &[xff("6.6.6.6, 7.7.7.7, 1.2.3.4, 10.0.0.9")]), seen("1.2.3.4"));
        // Several header lines are one list, in order.
        assert_eq!(apps_see("10.0.0.2", &t, &[xff("6.6.6.6"), xff("1.2.3.4")]), seen("1.2.3.4"));
        assert_eq!(apps_see("10.0.0.2", &t, &[xff("6.6.6.6, 1.2.3.4"), xff("10.0.0.7")]), seen("1.2.3.4"));
        // Junk the client put left of the proxy's entry is never read.
        assert_eq!(apps_see("10.0.0.2", &t, &[xff("not-an-ip, 1.2.3.4")]), seen("1.2.3.4"));
    }

    #[test]
    fn a_chain_of_trusted_proxies_is_walked() {
        let t = trust("10.0.0.0/8, 192.168.1.5, fd00::/8");
        assert_eq!(
            apps_see("10.0.0.2", &t, &[xff("6.6.6.6, 1.2.3.4, 192.168.1.5, fd00::3, 10.0.0.9")]),
            seen("1.2.3.4")
        );
        // An untrusted hop in the middle stops the walk there.
        assert_eq!(
            apps_see("10.0.0.2", &t, &[xff("1.2.3.4, 198.51.100.7, 10.0.0.9")]),
            seen("198.51.100.7")
        );
        // Every hop trusted: the furthest one (the left-most) is the client.
        assert_eq!(apps_see("10.0.0.2", &t, &[xff("10.0.0.5, 10.0.0.6")]), seen("10.0.0.5"));
    }

    #[test]
    fn ipv6_peers_and_clients() {
        let t = trust("fd00::/8, 2001:db8:aa::/48");
        assert_eq!(apps_see("fd00::1", &t, &[xff("2001:db8::7, fd00::2")]), seen("2001:db8::7"));
        assert_eq!(apps_see("2001:db8:aa::1", &t, &[xff("[2001:db8::7]")]), seen("2001:db8::7"));
        assert_eq!(apps_see("fd00::1", &t, &[xff("[2001:db8::7]:443")]), seen("2001:db8::7"));
        assert_eq!(apps_see("fd00::1", &t, &[xff("1.2.3.4")]), seen("1.2.3.4"));
        // Untrusted IPv6 peer.
        assert_eq!(apps_see("2001:db8:bb::1", &t, &[xff("1.2.3.4")]), seen("2001:db8:bb::1"));
        // A dual-stack socket's IPv4-mapped peer matches its IPv4 network, and a mapped client
        // is reported as IPv4.
        let v4 = trust("10.0.0.0/8");
        assert_eq!(apps_see("::ffff:10.0.0.2", &v4, &[xff("1.2.3.4")]), seen("1.2.3.4"));
        assert_eq!(apps_see("10.0.0.2", &v4, &[xff("::ffff:1.2.3.4")]), seen("1.2.3.4"));
        assert_eq!(apps_see("10.0.0.2", &v4, &[xff("1.2.3.4, ::ffff:10.0.0.3")]), seen("1.2.3.4"));
    }

    #[test]
    fn ports_on_forwarded_entries_are_dropped() {
        let t = trust("10.0.0.0/8");
        assert_eq!(apps_see("10.0.0.2", &t, &[xff("1.2.3.4:5555")]), seen("1.2.3.4"));
        assert_eq!(apps_see("10.0.0.2", &t, &[xff("1.2.3.4, 10.0.0.3:80")]), seen("1.2.3.4"));
    }

    #[test]
    fn the_forwarded_header_is_read_too() {
        let t = trust("10.0.0.0/8");
        for value in [
            "for=1.2.3.4",
            "For=1.2.3.4",
            "for=\"1.2.3.4\"",
            "for=\"1.2.3.4:4711\"",
            "for=1.2.3.4;proto=https;by=10.0.0.2",
            "proto=https; for=1.2.3.4",
            "for=6.6.6.6, for=1.2.3.4",
            "for=1.2.3.4, for=10.0.0.3",
            "for=6.6.6.6,for=1.2.3.4 , for=\"10.0.0.3:80\"",
        ] {
            assert_eq!(apps_see("10.0.0.2", &t, &[fwd(value)]), seen("1.2.3.4"), "{value}");
        }
        for value in ["for=\"[2001:db8::7]\"", "for=\"[2001:db8::7]:4711\"", "for=1.2.3.4, for=\"[2001:db8::7]\""] {
            assert_eq!(apps_see("10.0.0.2", &t, &[fwd(value)]), seen("2001:db8::7"), "{value}");
        }
        assert_eq!(apps_see("10.0.0.2", &t, &[fwd("for=6.6.6.6"), fwd("for=1.2.3.4")]), seen("1.2.3.4"));
    }

    #[test]
    fn a_spoofed_header_from_an_untrusted_peer_changes_nothing() {
        let t = trust("10.0.0.0/8, fd00::/8");
        for headers in [
            &[xff("1.2.3.4, 10.0.0.2")][..],
            &[xff("1.2.3.4")],
            &[xff("10.0.0.5")],
            &[fwd("for=1.2.3.4")],
            &[xff("1.2.3.4"), xff("10.0.0.9")],
        ] {
            assert_eq!(apps_see("203.0.113.5", &t, headers), seen("203.0.113.5"), "{headers:?}");
            assert_eq!(apps_see("2001:db8::9", &t, headers), seen("2001:db8::9"), "{headers:?}");
        }
    }

    #[test]
    fn a_malformed_header_from_a_trusted_peer_falls_back_to_the_peer() {
        let t = trust("10.0.0.0/8");
        let peer = seen("10.0.0.2");
        for value in [
            "",
            " ",
            ",",
            "1.2.3.4,",
            "1.2.3.4, ,10.0.0.3",
            "unknown",
            "garbage",
            "1.2.3.4 5.6.7.8",
            "1.2.3.256",
            "01.2.3.4",
            "fe80::1%eth0",
            "2001:db8::7]:443",
            "[2001:db8::7",
            "[1.2.3.4]",
            "[1.2.3.4]:80",
            "1.2.3.4:",
            "1.2.3.4:99999",
            "1.2.3.4:http",
            "\"1.2.3.4\"",
            "for=1.2.3.4",
            "1.2.3.4;10.0.0.3",
        ] {
            assert_eq!(apps_see("10.0.0.2", &t, &[xff(value)]), peer, "x-forwarded-for: {value:?}");
        }
        for value in [
            "",
            "for=",
            "for=\"\"",
            "for=unknown",
            "for=_hidden",
            "for=\"_hidden:4711\"",
            "proto=https",
            "for=1.2.3.4;for=5.6.7.8",
            "for=\"1.2.3.4",
            "for=1.2.3.4\"",
            "for=\"1.2.\\3.4\"",
            "for=\"1.2.3.4\"x",
            "for=1.2.3.4,",
            "for=1.2.3.4, , for=10.0.0.3",
            "for=1.2.3.4;",
            "for 1.2.3.4",
            "=1.2.3.4",
            "for=[2001:db8::7]",
            "for=2001:db8::7",
            "for=\"2001:db8::7\"",
            "for=\"1.2.3.4, for=6.6.6.6\"",
        ] {
            assert_eq!(apps_see("10.0.0.2", &t, &[fwd(value)]), peer, "forwarded: {value:?}");
        }
        // Bytes that are not visible ASCII.
        assert_eq!(apps_see("10.0.0.2", &t, &[("x-forwarded-for", b"1.2.3.4\xff")]), peer);
        assert_eq!(apps_see("10.0.0.2", &t, &[("forwarded", b"for=\"1.2.3.4\xe9\"")]), peer);
    }

    #[test]
    fn both_forwarding_headers_from_a_trusted_peer_fall_back_to_the_peer() {
        // Which one the proxy wrote can't be told apart from which one the client sent.
        let t = trust("10.0.0.0/8");
        assert_eq!(apps_see("10.0.0.2", &t, &[xff("1.2.3.4"), fwd("for=1.2.3.4")]), seen("10.0.0.2"));
        assert_eq!(apps_see("10.0.0.2", &t, &[fwd("for=6.6.6.6"), xff("1.2.3.4")]), seen("10.0.0.2"));
    }

    #[test]
    fn a_trusted_peer_without_a_forwarding_header_is_the_client() {
        let t = trust("10.0.0.0/8");
        assert_eq!(apps_see("10.0.0.2", &t, &[]), seen("10.0.0.2"));
        assert_eq!(apps_see("10.0.0.2", &t, &[("x-real-ip", b"1.2.3.4")]), seen("10.0.0.2"));
    }

    #[test]
    fn an_overlong_header_from_a_trusted_peer_falls_back_to_the_peer() {
        let t = trust("10.0.0.0/8");
        let peer = seen("10.0.0.2");
        // Too many hops, even when every one parses.
        let hops = vec!["10.0.0.9"; MAX_FORWARDED_HOPS].join(", ");
        assert_eq!(apps_see("10.0.0.2", &t, &[xff(&format!("1.2.3.4, {hops}"))]), peer);
        let within = vec!["10.0.0.9"; MAX_FORWARDED_HOPS - 1].join(", ");
        assert_eq!(apps_see("10.0.0.2", &t, &[xff(&format!("1.2.3.4, {within}"))]), seen("1.2.3.4"));
        // Too many bytes, split over lines or not.
        let long = format!("{}1.2.3.4", " ".repeat(MAX_FORWARDED_BYTES));
        assert_eq!(apps_see("10.0.0.2", &t, &[xff(&long)]), peer);
        let half = " ".repeat(MAX_FORWARDED_BYTES / 2);
        assert_eq!(apps_see("10.0.0.2", &t, &[xff(&half), xff(&half), xff("1.2.3.4")]), peer);
        let fwd_hops = vec!["for=10.0.0.9"; MAX_FORWARDED_HOPS].join(", ");
        assert_eq!(apps_see("10.0.0.2", &t, &[fwd(&format!("for=1.2.3.4, {fwd_hops}"))]), peer);
        // A huge number of header lines.
        let many: Vec<_> = (0..200).map(|_| xff("10.0.0.9")).collect();
        assert_eq!(apps_see("10.0.0.2", &t, &many), peer);
    }

    #[test]
    fn an_unknown_peer_stays_unknown() {
        let t = trust("10.0.0.0/8");
        let mut map = http::HeaderMap::new();
        map.insert("x-forwarded-for", "1.2.3.4".parse().unwrap());
        assert_eq!(client_address(None, &t, &map), None);
    }

    fn accept(values: &[&str]) -> http::HeaderMap {
        let mut headers = http::HeaderMap::new();
        for v in values {
            headers.append(http::header::ACCEPT, v.parse().unwrap());
        }
        headers
    }

    const CONFIGURED: Duration = Duration::from_secs(60);

    #[test]
    fn a_site_websocket_gets_the_long_idle_limit() {
        assert_eq!(site_read_timeout(true, &accept(&[]), CONFIGURED), WEBSOCKET_READ_TIMEOUT);
    }

    #[test]
    fn a_site_event_stream_gets_the_long_idle_limit() {
        for values in [
            &["text/event-stream"][..],
            &["text/event-stream, */*"],
            &["application/json;q=0.9, Text/Event-Stream ;q=1"],
            &["application/json", "text/event-stream"],
        ] {
            assert_eq!(
                site_read_timeout(false, &accept(values), CONFIGURED),
                WEBSOCKET_READ_TIMEOUT,
                "{values:?}"
            );
        }
    }

    #[test]
    fn a_plain_site_request_gets_the_configured_timeout() {
        for values in [
            &[][..],
            &["*/*"],
            &["text/html, application/xhtml+xml"],
            &["text/event-streams"],
            &["text/*"],
        ] {
            assert_eq!(site_read_timeout(false, &accept(values), CONFIGURED), CONFIGURED, "{values:?}");
        }
    }

    #[test]
    fn explicit_status_is_kept_and_internal_errors_are_500() {
        assert_eq!(failure_status(&ErrorSource::Internal, &ErrorType::HTTPStatus(413)), 413);
        assert_eq!(failure_status(&ErrorSource::Internal, &ErrorType::InternalError), 500);
    }
}
