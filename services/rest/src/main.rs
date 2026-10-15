//! The `rest` service: the data API engine behind the gateway.

use std::sync::Arc;

use chelabase_auth::internal::InternalVerifier;
use chelabase_query::server_tls::ServerTls;
use rest::app::{AppState, Config, internal_router, router};
use rest::conn::InternalSource;
use rest::exec::Engine;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rest=info".into()),
        )
        .init();
    let config = Config::from_env()?;
    // U15: every compute is verified against the platform CA; no CA, no start.
    let server_tls = ServerTls::from_ca_file(&config.compute_ca)?;
    let state = Arc::new(AppState {
        engine: Engine::with_server_tls(
            InternalSource::new(&config.internal_url, &config.internal_token),
            config.max_connections,
            server_tls,
        )
        .with_max_response_bytes(config.max_response_bytes),
        verifier: InternalVerifier::from_env()?,
        internal_token: config.internal_token.clone(),
    });
    let listener = tokio::net::TcpListener::bind(config.addr).await?;
    let internal_listener = tokio::net::TcpListener::bind(config.internal_addr).await?;
    info!(
        "rest listening on {} (internal on {})",
        config.addr, config.internal_addr
    );
    let (public, internal) = (router(state.clone()), internal_router(state));
    tokio::try_join!(async { axum::serve(listener, public).await }, async {
        axum::serve(internal_listener, internal).await
    },)?;
    Ok(())
}
