//! The `storage` service: object storage behind the gateway.

use std::sync::Arc;

use chelabase_auth::internal::InternalVerifier;
use chelabase_pgsession::{InternalSource, Pools};
use chelabase_query::server_tls::ServerTls;
use storage::app::{AppState, router};
use storage::config::Config;
use storage::engine::Engine;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    chelabase_types::cli::handle(
        "storage",
        "Chelabase object storage behind the gateway.",
        env!("CARGO_PKG_VERSION"),
    );
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "storage=info".into()),
        )
        .init();
    let config = Config::from_env()?;
    // U15: every compute is verified against the platform CA; no CA, no start.
    let server_tls = ServerTls::from_ca_file(&config.compute_ca)?;
    // Role sessions connect as `authenticator`; the managed pool as `chelabase_storage`.
    let pools = Pools::with_server_tls(
        InternalSource::new(&config.internal_url, &config.internal_token),
        config.max_connections,
        server_tls,
    );
    let managed = InternalSource::storage_login(&config.internal_url, &config.internal_token);
    let state = Arc::new(AppState {
        engine: Engine::new(pools, managed),
        verifier: InternalVerifier::from_env()?,
        store: Arc::new(chelabase_storage::S3Store::new(&config.store).await),
    });
    let listener = tokio::net::TcpListener::bind(config.addr).await?;
    info!(
        "storage listening on {} (bucket {})",
        config.addr, config.store.bucket
    );
    axum::serve(listener, router(state)).await?;
    Ok(())
}
