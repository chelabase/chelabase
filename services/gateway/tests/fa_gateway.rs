//! RT1 final fix round, batch A (T3 hardening):
//! - an `Upgrade` header on HTTP/1.0 is refused like one on HTTP/1.1 (Pingora's
//!   `is_upgrade_req` only looks at HTTP/1.1, so an HTTP/1.0 request carrying `Upgrade` was
//!   proxied with the header on);
//! - the refused-upgrade 400 carries the route's CORS headers (Studio's browser can read it);
//! - (the Pingora log cap: `fa_gateway_logging.rs`);
//! - h2 extended CONNECT (RFC 8441) is a second way to open a socket: the gateway serves
//!   HTTP/1 only, and the upgrade check says so in a comment naming it, so enabling h2 refuses
//!   it there too.

use chelabase_auth::internal::{InternalSigner, generate_keypair_pem};
use chelabase_auth::test_support::MemberKeys;
use chelabase_types::entry::Scheme;
use gateway::data_keys::DataKeyResolver;
use gateway::pipeline::Pipeline;
use gateway::proxy::Gateway;
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
const STUDIO: &str = "http://studio.test";

async fn gateway() -> (Gateway, MemberKeys) {
    let jwks = MemberKeys::new();
    let (private_pem, _public) = generate_keypair_pem();
    let pipeline = Pipeline::new(
        Arc::new(SharedRegistry::new(Registry::new([API]))),
        STUDIO.to_string(),
        InternalSigner::from_pem(private_pem.as_bytes()).unwrap(),
        Arc::new(DataKeyResolver::new("http://127.0.0.1:1", "unused")),
        Arc::new(UserTokens::new(None, "http://127.0.0.1:1", "unused")),
    )
    .with_member_tokens(Some(jwks.verifier()));
    let upstreams = Upstreams::new("http://127.0.0.1:1", "http://127.0.0.1:1").unwrap();
    (
        Gateway::new(pipeline, upstreams, Duration::from_secs(7), Scheme::Http),
        jwks,
    )
}

async fn session(raw: &str) -> (Session, TcpStream) {
    let listener = TcpListener::bind(("localhost", 0)).await.unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (server, _) = listener.accept().await.unwrap();
    client.write_all(raw.as_bytes()).await.unwrap();
    let mut session = Session::new_h1(Box::new(Stream::from(server)));
    assert!(session.read_request().await.unwrap());
    (session, client)
}

async fn written(client: &mut TcpStream) -> String {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(Ok(n)) =
        tokio::time::timeout(Duration::from_millis(500), client.read(&mut buf)).await
    {
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn status_line(response: &str) -> &str {
    response.lines().next().unwrap_or("")
}

/// The value of header `name` (case-insensitive) in a raw HTTP/1 response head.
fn header<'a>(response: &'a str, name: &str) -> Option<&'a str> {
    response
        .split("\r\n\r\n")
        .next()
        .unwrap_or("")
        .lines()
        .skip(1)
        .find_map(|l| {
            let (n, v) = l.split_once(':')?;
            n.trim().eq_ignore_ascii_case(name).then(|| v.trim())
        })
}

// ---- HTTP/1.0 Upgrade ----

#[tokio::test]
async fn an_http10_upgrade_on_a_non_websocket_route_is_refused() {
    for upgrade in ["websocket", "h2c"] {
        let (gw, _jwks) = gateway().await;
        let raw = format!(
            "GET /health HTTP/1.0\r\nHost: {API}\r\nUpgrade: {upgrade}\r\nConnection: Upgrade\r\n\r\n"
        );
        let (mut session, mut client) = session(&raw).await;
        let mut ctx = gw.new_ctx();
        let handled = gw.request_filter(&mut session, &mut ctx).await.unwrap();
        assert!(
            handled,
            "{upgrade}: an HTTP/1.0 request carrying Upgrade must be answered by the gateway"
        );
        let response = written(&mut client).await;
        assert!(
            status_line(&response).contains(" 400"),
            "{upgrade}: {response}"
        );
        assert!(
            gw.upstream_peer(&mut session, &mut ctx).await.is_err(),
            "{upgrade}: no proxy plan"
        );
    }
}

#[tokio::test]
async fn an_http10_upgrade_on_the_socket_route_is_refused_too() {
    // A websocket needs HTTP/1.1 (RFC 6455); on 1.0 the header is refused, never forwarded.
    let (gw, _jwks) = gateway().await;
    let raw = format!(
        "GET /v1/realtime/socket?ticket=T HTTP/1.0\r\nHost: {API}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
    );
    let (mut session, mut client) = session(&raw).await;
    let mut ctx = gw.new_ctx();
    assert!(gw.request_filter(&mut session, &mut ctx).await.unwrap());
    let response = written(&mut client).await;
    assert!(status_line(&response).contains(" 400"), "{response}");
    assert!(gw.upstream_peer(&mut session, &mut ctx).await.is_err());
}

#[tokio::test]
async fn an_http10_request_without_upgrade_is_still_proxied() {
    // Guard against a blanket HTTP/1.0 refusal.
    let (gw, _jwks) = gateway().await;
    let (mut session, _client) =
        session(&format!("GET /health HTTP/1.0\r\nHost: {API}\r\n\r\n")).await;
    let mut ctx = gw.new_ctx();
    assert!(!gw.request_filter(&mut session, &mut ctx).await.unwrap());
}

// ---- CORS on the refused-upgrade 400 ----

#[tokio::test]
async fn the_refused_upgrade_answer_carries_the_routes_cors_headers() {
    let (gw, jwks) = gateway().await;
    let token = jwks.token("user-1", uuid::Uuid::from_u128(1), 300);
    let raw = format!(
        "POST /v1/databases HTTP/1.1\r\nHost: {API}\r\nContent-Length: 0\r\nAuthorization: Bearer {token}\r\nOrigin: {STUDIO}\r\n\
         Upgrade: websocket\r\nConnection: Upgrade\r\n\r\n"
    );
    let (mut session, mut client) = session(&raw).await;
    let mut ctx = gw.new_ctx();
    assert!(gw.request_filter(&mut session, &mut ctx).await.unwrap());
    let response = written(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    assert_eq!(
        header(&response, "access-control-allow-origin"),
        Some(STUDIO),
        "Studio's browser can't read a 400 without the route's CORS headers: {response}"
    );
    assert_eq!(header(&response, "vary"), Some("Origin"), "{response}");
}

#[tokio::test]
async fn the_refused_upgrade_answer_gives_another_origin_no_cors_grant() {
    let (gw, jwks) = gateway().await;
    let token = jwks.token("user-1", uuid::Uuid::from_u128(1), 300);
    let raw = format!(
        "POST /v1/databases HTTP/1.1\r\nHost: {API}\r\nContent-Length: 0\r\nAuthorization: Bearer {token}\r\nOrigin: http://evil.example\r\n\
         Upgrade: websocket\r\nConnection: Upgrade\r\n\r\n"
    );
    let (mut session, mut client) = session(&raw).await;
    let mut ctx = gw.new_ctx();
    assert!(gw.request_filter(&mut session, &mut ctx).await.unwrap());
    let response = written(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 400"), "{response}");
    assert_eq!(
        header(&response, "access-control-allow-origin"),
        None,
        "{response}"
    );
}

// ---- h2 extended CONNECT: a note where the upgrade is checked ----

#[test]
fn the_upgrade_check_names_h2_extended_connect() {
    let source = include_str!("../src/proxy.rs");
    assert!(
        source.contains("extended CONNECT") && source.contains("8441"),
        "proxy.rs must say, at the upgrade check, that the gateway serves HTTP/1 only and that \
         enabling h2 must refuse extended CONNECT (RFC 8441) on every non-websocket route"
    );
}
