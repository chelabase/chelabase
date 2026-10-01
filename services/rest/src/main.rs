//! The `rest` service: the data API engine behind the gateway.

use std::sync::Arc;

use cheladb_auth::internal::InternalVerifier;
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
    let state = Arc::new(AppState {
        engine: Engine::new(
            InternalSource::new(&config.internal_url, &config.internal_token),
            config.max_connections,
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
