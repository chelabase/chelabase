//! RT1 T3 edges beyond the RED set: what an upgrade may and may not do on the socket route.

use chelabase_auth::internal::{generate_keypair_pem, InternalSigner};
use chelabase_auth::test_support::MockJwks;
use chelabase_auth::JwtVerifier;
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

async fn gateway() -> (Gateway, MockJwks) {
    let jwks = MockJwks::start().await;
    let (private_pem, _public) = generate_keypair_pem();
    let pipeline = Pipeline::new(
        Arc::new(SharedRegistry::new(Registry::new([API]))),
        "http://studio.test".to_string(),
        JwtVerifier::new(jwks.config(vec![])),
        InternalSigner::from_pem(private_pem.as_bytes()).unwrap(),
        Arc::new(DataKeyResolver::new("http://127.0.0.1:1", "unused")),
        Arc::new(UserTokens::new(None, "http://127.0.0.1:1", "unused")),
    );
    let upstreams = Upstreams::new("http://127.0.0.1:1", "http://127.0.0.1:1").unwrap();
    (Gateway::new(pipeline, upstreams, Duration::from_secs(7), Scheme::Http), jwks)
}

async fn session(raw: &str) -> (Session, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
    let (server, _) = listener.accept().await.unwrap();
    client.write_all(raw.as_bytes()).await.unwrap();
    let mut session = Session::new_h1(Box::new(Stream::from(server)));
    assert!(session.read_request().await.unwrap());
    (session, client)
}

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

#[tokio::test]
async fn a_non_websocket_upgrade_on_the_socket_route_is_refused() {
    for upgrade in ["h2c", "websocket, h2c"] {
        let (gw, _jwks) = gateway().await;
        let raw = format!(
            "GET /v1/realtime/socket?ticket=T HTTP/1.1\r\nHost: {API}\r\nUpgrade: {upgrade}\r\nConnection: Upgrade\r\n\r\n"
        );
        let (mut session, mut client) = session(&raw).await;
        let mut ctx = gw.new_ctx();
        assert!(gw.request_filter(&mut session, &mut ctx).await.unwrap(), "{upgrade}");
        assert!(written(&mut client).await.starts_with("HTTP/1.1 400"), "{upgrade}");
        assert!(gw.upstream_peer(&mut session, &mut ctx).await.is_err(), "{upgrade}");
    }
}

#[tokio::test]
async fn a_plain_get_on_the_socket_route_keeps_the_configured_timeouts() {
    // No upgrade, no hour: realtime answers it (426) and the connection is an ordinary one.
    let (gw, _jwks) = gateway().await;
    let raw = format!("GET /v1/realtime/socket HTTP/1.1\r\nHost: {API}\r\n\r\n");
    let (mut session, _client) = session(&raw).await;
    let mut ctx = gw.new_ctx();
    assert!(!gw.request_filter(&mut session, &mut ctx).await.unwrap());
    let peer = gw.upstream_peer(&mut session, &mut ctx).await.unwrap();
    assert_eq!(peer.options.read_timeout, Some(Duration::from_secs(7)));
}

#[tokio::test]
async fn an_upgrade_on_the_ticket_route_is_refused() {
    let (gw, _jwks) = gateway().await;
    let raw = format!(
        "POST /v1/realtime/tickets HTTP/1.1\r\nHost: {API}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nContent-Length: 0\r\n\r\n"
    );
    let (mut session, mut client) = session(&raw).await;
    let mut ctx = gw.new_ctx();
    assert!(gw.request_filter(&mut session, &mut ctx).await.unwrap());
    let out = written(&mut client).await;
    // Management auth answers first (401) when there is no bearer; either way it is not proxied.
    assert!(out.starts_with("HTTP/1.1 401") || out.starts_with("HTTP/1.1 400"), "{out}");
}
