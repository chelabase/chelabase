//! RT1 final fix round, batch A (coder's edges beyond `fa_gateway.rs`): the HTTP/1.0 `Upgrade`
//! refusal also covers pass-through sites, and HTTP/1.1 upgrades keep working where they did.

use chelabase_auth::internal::{InternalSigner, generate_keypair_pem};
use chelabase_auth::test_support::MemberKeys;
use chelabase_types::entry::Scheme;
use gateway::data_keys::DataKeyResolver;
use gateway::pipeline::Pipeline;
use gateway::proxy::Gateway;
use gateway::registry::{Registry, SharedRegistry, Site};
use gateway::upstreams::Upstreams;
use gateway::user_tokens::UserTokens;
use pingora_core::protocols::l4::stream::Stream;
use pingora_proxy::{ProxyHttp, Session};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const API: &str = "api.test";
const SITE: &str = "studio.test";

async fn gateway() -> (Gateway, MemberKeys) {
    let jwks = MemberKeys::new();
    let (private_pem, _public) = generate_keypair_pem();
    let pipeline = Pipeline::new(
        Arc::new(SharedRegistry::new(
            Registry::new([API]).with_site(SITE, Site::Studio),
        )),
        format!("http://{SITE}"),
        InternalSigner::from_pem(private_pem.as_bytes()).unwrap(),
        Arc::new(DataKeyResolver::new("http://127.0.0.1:1", "unused")),
        Arc::new(UserTokens::new(None, "http://127.0.0.1:1", "unused")),
    )
    .with_member_tokens(Some(jwks.verifier()));
    let upstreams = Upstreams::new("http://127.0.0.1:1", "http://127.0.0.1:1")
        .unwrap()
        .with_site(Site::Studio, "http://127.0.0.1:1")
        .unwrap();
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

const WS: &str = "Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n\
                  Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n";

#[tokio::test]
async fn an_http10_upgrade_on_a_site_is_refused() {
    let (gw, _jwks) = gateway().await;
    let (mut session, mut client) = session(&format!(
        "GET /@vite/client HTTP/1.0\r\nHost: {SITE}\r\n{WS}\r\n"
    ))
    .await;
    let mut ctx = gw.new_ctx();
    assert!(gw.request_filter(&mut session, &mut ctx).await.unwrap());
    let response = written(&mut client).await;
    assert!(
        response.lines().next().unwrap_or("").contains(" 400"),
        "{response}"
    );
}

#[tokio::test]
async fn an_http11_upgrade_on_a_site_still_passes_through() {
    let (gw, _jwks) = gateway().await;
    let (mut session, _client) = session(&format!(
        "GET /@vite/client HTTP/1.1\r\nHost: {SITE}\r\n{WS}\r\n"
    ))
    .await;
    let mut ctx = gw.new_ctx();
    assert!(!gw.request_filter(&mut session, &mut ctx).await.unwrap());
}

#[tokio::test]
async fn an_http11_websocket_on_the_socket_route_is_still_proxied() {
    let (gw, _jwks) = gateway().await;
    let (mut session, _client) = session(&format!(
        "GET /v1/realtime/socket?ticket=T HTTP/1.1\r\nHost: {API}\r\n{WS}\r\n"
    ))
    .await;
    let mut ctx = gw.new_ctx();
    assert!(!gw.request_filter(&mut session, &mut ctx).await.unwrap());
    assert!(gw.upstream_peer(&mut session, &mut ctx).await.is_ok());
}
