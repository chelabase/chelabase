//! Cheladb gateway stub.
//!
//! Future: Cloudflare Pingora proxy with Postgres wire parsing and metadata-based
//! dynamic routing. Auth will verify JWTs via JWKS (`cheladb-auth`), not introspection.
//!
//! This skeleton binds a TCP port and closes connections with a clear stub response
//! so the service shape exists without pulling in the full Pingora bootstrap yet.

use cheladb_auth::ZitadelAuthConfig;
use cheladb_metadata::MetadataConfig;
use std::net::SocketAddr;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;
use tracing::{info, warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gateway=info".into()),
        )
        .init();

    let _metadata = MetadataConfig::from_env().unwrap_or_else(|err| {
        warn!("{err}; using placeholder metadata URL");
        MetadataConfig {
            database_url: "postgres://cheladb:cheladb@127.0.0.1:5433/cheladb_metadata".into(),
        }
    });
    let _auth = ZitadelAuthConfig::from_env().expect("auth config");

    let addr: SocketAddr = std::env::var("CHELADB_GATEWAY_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:15432".into())
        .parse()?;

    let listener = TcpListener::bind(addr).await?;
    info!(
        %addr,
        "gateway listening (TCP stub — Pingora + Postgres wire protocol not wired yet)"
    );

    loop {
        let (mut socket, peer) = listener.accept().await?;
        info!(%peer, "accepted connection (stub — closing)");
        tokio::spawn(async move {
            let msg = b"cheladb gateway stub: Postgres wire routing not implemented\n";
            let _ = socket.write_all(msg).await;
            let _ = socket.shutdown().await;
        });
    }
}
