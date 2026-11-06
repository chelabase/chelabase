//! RT1 T3 (S18, W16, W17): the gateway's realtime rules, driven in process through the
//! `ProxyHttp` callbacks on a real (loopback) Pingora session, so timeouts and log text can
//! be read back without a running gateway binary.

use chelabase_auth::internal::{generate_keypair_pem, InternalSigner, IDENTITY_HEADER};
use chelabase_auth::test_support::MemberKeys;
use chelabase_types::entry::Scheme;
use gateway::data_keys::DataKeyResolver;
use gateway::pipeline::Pipeline;
use gateway::proxy::{Gateway, RequestCtx};
use gateway::registry::{Registry, SharedRegistry};
use gateway::upstreams::Upstreams;
use gateway::user_tokens::UserTokens;
use pingora_core::protocols::l4::stream::Stream;
use pingora_proxy::{ProxyHttp, Session};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const API: &str = "api.test";
const ONE_HOUR: Duration = Duration::from_secs(3600);
/// Deliberately not 60 s or 1 h, so a timeout can only come from the code under test.
const API_TIMEOUT: Duration = Duration::from_secs(7);

async fn gateway() -> (Gateway, MemberKeys) {
    let jwks = MemberKeys::new();
    let (private_pem, _public) = generate_keypair_pem();
    let pipeline = Pipeline::new(
        Arc::new(SharedRegistry::new(Registry::new([API]))),
        "http://studio.test".to_string(),
        InternalSigner::from_pem(private_pem.as_bytes()).unwrap(),
        Arc::new(DataKeyResolver::new("http://127.0.0.1:1", "unused")),
        Arc::new(UserTokens::new(None, "http://127.0.0.1:1", "unused")),
    )
    .with_member_tokens(Some(jwks.verifier()));
    let upstreams = Upstreams::new("http://127.0.0.1:1", "http://127.0.0.1:1").unwrap();
    (Gateway::new(pipeline, upstreams, API_TIMEOUT, Scheme::Http), jwks)
}

/// A server-side session that has read `raw` from a loopback client; the client end is
/// returned so a test can read what the gateway wrote.
async fn session(raw: &str) -> (Session, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
    let (server, _) = listener.accept().await.unwrap();
    client.write_all(raw.as_bytes()).await.unwrap();
    let mut session = Session::new_h1(Box::new(Stream::from(server)));
    assert!(session.read_request().await.unwrap(), "the request head was not read");
    (session, client)
}

fn upgrade_request(method_and_target: &str, extra: &str) -> String {
    format!(
        "{method_and_target} HTTP/1.1\r\nHost: {API}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Protocol: chelabase.realtime.v1\r\n{extra}\r\n"
    )
}

/// What the gateway wrote to the client so far (up to `idle` of silence).
async fn written(client: &mut TcpStream) -> String {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(Ok(n)) = tokio::time::timeout(Duration::from_millis(500), client.read(&mut buf)).await {
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---- S18 / W17: the socket route's timeouts ----

#[tokio::test]
async fn a_socket_upgrade_gets_one_hour_read_timeouts_both_ways() {
    let (gw, _jwks) = gateway().await;
    let (mut session, _client) = session(&upgrade_request("GET /v1/realtime/socket?ticket=T", "")).await;
    let mut ctx = gw.new_ctx();

    let handled = gw.request_filter(&mut session, &mut ctx).await.unwrap();
    assert!(!handled, "the socket route must be proxied, not answered by the gateway");
    assert_eq!(
        session.get_read_timeout(),
        Some(ONE_HOUR),
        "the downstream (client) read timeout is raised to 1 h"
    );

    let peer = gw.upstream_peer(&mut session, &mut ctx).await.unwrap();
    assert_eq!(peer.options.read_timeout, Some(ONE_HOUR), "the upstream read timeout is 1 h");
    assert_ne!(API_TIMEOUT, ONE_HOUR);
    // The upstream is CHELABASE_REALTIME_URL's default (S18).
    assert!(peer.to_string().contains("127.0.0.1:8087"), "{peer}");
}

#[tokio::test]
async fn an_ordinary_api_request_keeps_the_configured_timeouts() {
    // Guard: the 1 h limits belong to the socket route alone.
    let (gw, _jwks) = gateway().await;
    let (mut session, _client) = session(&format!("GET /health HTTP/1.1\r\nHost: {API}\r\n\r\n")).await;
    let mut ctx = gw.new_ctx();
    assert!(!gw.request_filter(&mut session, &mut ctx).await.unwrap());
    assert_ne!(session.get_read_timeout(), Some(ONE_HOUR));
    let peer = gw.upstream_peer(&mut session, &mut ctx).await.unwrap();
    assert_eq!(peer.options.read_timeout, Some(API_TIMEOUT));
}

// ---- W17: no other API route takes an upgrade ----

#[tokio::test]
async fn an_upgrade_on_a_non_websocket_api_route_is_answered_400_without_a_proxy_plan() {
    for (target, extra) in [("GET /health", ""), ("GET /v1/openapi.json", "")] {
        let (gw, _jwks) = gateway().await;
        let (mut session, mut client) = session(&upgrade_request(target, extra)).await;
        let mut ctx = gw.new_ctx();

        let handled = gw.request_filter(&mut session, &mut ctx).await.unwrap();
        assert!(handled, "{target}: an upgrade on a non-websocket route must be answered by the gateway");
        let response = written(&mut client).await;
        assert!(response.starts_with("HTTP/1.1 400"), "{target}: {response}");
        // Nothing was planned, so no upstream is ever chosen or connected.
        assert!(
            gw.upstream_peer(&mut session, &mut ctx).await.is_err(),
            "{target}: a refused upgrade must not reach upstream_peer with a plan"
        );
    }
}

#[tokio::test]
async fn an_upgrade_without_a_connection_header_on_a_non_websocket_route_is_refused_too() {
    let (gw, _jwks) = gateway().await;
    let raw = format!("GET /health HTTP/1.1\r\nHost: {API}\r\nUpgrade: h2c\r\n\r\n");
    let (mut session, mut client) = session(&raw).await;
    let mut ctx = gw.new_ctx();
    assert!(gw.request_filter(&mut session, &mut ctx).await.unwrap());
    let response = written(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
}

#[tokio::test]
async fn a_request_without_upgrade_on_a_non_websocket_route_is_still_proxied() {
    // Guard against a blanket refusal.
    let (gw, _jwks) = gateway().await;
    let (mut session, _client) = session(&format!("GET /health HTTP/1.1\r\nHost: {API}\r\n\r\n")).await;
    let mut ctx = gw.new_ctx();
    assert!(!gw.request_filter(&mut session, &mut ctx).await.unwrap());
}

// ---- Public route: client identity headers never reach realtime ----

#[tokio::test]
async fn identity_and_credential_headers_are_stripped_on_the_public_socket_route() {
    let (gw, _jwks) = gateway().await;
    let extra = format!("{IDENTITY_HEADER}: forged\r\nAuthorization: Bearer nope\r\nChelabase-User: u\r\n");
    let (mut session, _client) = session(&upgrade_request("GET /v1/realtime/socket?ticket=T", &extra)).await;
    let mut ctx = gw.new_ctx();
    let handled = gw.request_filter(&mut session, &mut ctx).await.unwrap();
    assert!(!handled, "the socket route must be proxied");

    let mut upstream = session.req_header().clone();
    gw.upstream_request_filter(&mut session, &mut upstream, &mut ctx).await.unwrap();
    assert!(upstream.headers.get(IDENTITY_HEADER).is_none(), "{:?}", upstream.headers);
    assert!(upstream.headers.get("authorization").is_none());
    assert!(upstream.headers.get("chelabase-user").is_none());
    // The query (the ticket) is forwarded untouched: that is how realtime gets it.
    assert_eq!(upstream.uri.query(), Some("ticket=T"));
    // The subprotocol offer is the client's to make and realtime's to check.
    assert_eq!(
        upstream.headers.get("sec-websocket-protocol").map(|v| v.to_str().unwrap()),
        Some("chelabase.realtime.v1")
    );
}

// ---- W16: the ticket never reaches the logs ----

fn summary(gw: &Gateway, session: &Session, ctx: &RequestCtx) -> String {
    gw.request_summary(session, ctx)
}

#[tokio::test]
async fn the_request_summary_has_method_path_and_host_but_no_query() {
    let (gw, _jwks) = gateway().await;
    let (session, _client) = session(&upgrade_request("GET /v1/realtime/socket?ticket=SECRET", "")).await;
    let ctx = gw.new_ctx();
    let text = summary(&gw, &session, &ctx);
    assert!(!text.contains("SECRET"), "the ticket leaked into the error-log summary: {text}");
    assert!(!text.contains("ticket"), "{text}");
    assert!(!text.contains('?'), "{text}");
    assert!(text.contains("GET"), "{text}");
    assert!(text.contains("/v1/realtime/socket"), "{text}");
    assert!(text.contains(API), "{text}");
}

#[tokio::test]
async fn the_request_summary_hides_every_query_shape() {
    for target in [
        "GET /v1/realtime/socket?ticket=SECRET&x=1",
        "GET /v1/realtime/socket?x=1&ticket=SECRET",
        "GET /v1/realtime/socket?ticket=SECRET%20%3F#frag",
        "GET /v1/realtime/socket?ticket=",
        "GET /v1/realtime/socket??ticket=SECRET",
    ] {
        let (gw, _jwks) = gateway().await;
        let (session, _client) = session(&format!("{target} HTTP/1.1\r\nHost: {API}\r\n\r\n")).await;
        let text = summary(&gw, &session, &gw.new_ctx());
        assert!(!text.contains("SECRET") && !text.contains("ticket") && !text.contains('?'), "{target}: {text}");
        assert!(text.contains("/v1/realtime/socket"), "{target}: {text}");
    }
}

#[tokio::test]
async fn the_request_summary_of_an_absolute_form_target_uses_its_authority_and_drops_the_query() {
    let (gw, _jwks) = gateway().await;
    let raw = "GET http://abs.test/v1/realtime/socket?ticket=SECRET HTTP/1.1\r\nHost: abs.test\r\n\r\n";
    let (session, _client) = session(raw).await;
    let text = summary(&gw, &session, &gw.new_ctx());
    assert!(!text.contains("SECRET"), "{text}");
    assert!(text.contains("abs.test"), "{text}");
    assert!(text.contains("/v1/realtime/socket"), "{text}");
}

#[tokio::test]
async fn the_request_summary_of_a_query_free_request_keeps_its_path() {
    // Guard: no query, nothing to cut.
    let (gw, _jwks) = gateway().await;
    let (session, _client) = session(&format!("POST /v1/databases HTTP/1.1\r\nHost: {API}\r\n\r\n")).await;
    let text = summary(&gw, &session, &gw.new_ctx());
    assert!(text.contains("POST") && text.contains("/v1/databases") && text.contains(API), "{text}");
}
