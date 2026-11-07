//! The `functions` service: edge function invocations behind the gateway, and their bundles.

use std::sync::Arc;

use anyhow::Context as _;
use chelabase_auth::internal::InternalVerifier;
use chelabase_metadata::{MetadataClient, MetadataConfig};
use chelabase_secrets::SecretKey;
use functions::app::{AppState, internal_router, public_router};
use functions::bundles::Bundles;
use functions::config::Config;
use functions::directory::Metadata;
use functions::host::Workerd;
use functions::resolver::{Resolver, TTL, spawn_listener};
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    chelabase_types::cli::handle(
        "functions",
        "Chelabase edge functions: invocations behind the gateway, and their bundles.",
        env!("CARGO_PKG_VERSION"),
    );
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "functions=info,chelabase_nats=info,chelabase_storage=info".into()
            }),
        )
        .init();
    let config = Config::from_env()?;
    let database_url = chelabase_types::secret_env::url_with_password_file(
        |name| std::env::var(name).ok(),
        "CHELABASE_METADATA_DATABASE_URL",
        "CHELABASE_METADATA_DATABASE_PASSWORD_FILE",
    )
    .map_err(anyhow::Error::msg)?
    .context("CHELABASE_METADATA_DATABASE_URL is required")?;
    let key = SecretKey::from_env()?;
    // The orchestrator migrates the metadata database; this service only reads and records
    // `last_error`, so it connects lazily and starts while the database is coming up.
    let client =
        MetadataClient::connect_lazy(&MetadataConfig { database_url })?.with_secrets_key(key);
    let resolver = Arc::new(Resolver::new(
        Arc::new(Metadata { client }),
        config.public_url.clone(),
        TTL,
    ));
    spawn_listener(
        resolver.clone(),
        chelabase_nats::NatsConfig::from_env().map_err(anyhow::Error::msg)?,
    );
    let store = Arc::new(chelabase_storage::S3Store::new(&config.store).await);
    let state = Arc::new(AppState::new(
        resolver,
        Arc::new(Workerd::new(&config.runtime_url, config.timeout)?),
        InternalVerifier::from_env()?,
        Bundles::new(store),
        config.orchestrator_token.clone(),
        config.worker_token.clone(),
    ));
    let internal_addr = tokio::net::lookup_host(&config.internal_addr)
        .await
        .with_context(|| {
            format!(
                "resolve CHELABASE_FUNCTIONS_INTERNAL_ADDR {}",
                config.internal_addr
            )
        })?
        .next()
        .with_context(|| {
            format!(
                "CHELABASE_FUNCTIONS_INTERNAL_ADDR {} has no address",
                config.internal_addr
            )
        })?;
    let public = tokio::net::TcpListener::bind(config.addr).await?;
    let internal = tokio::net::TcpListener::bind(internal_addr).await?;
    info!(
        "functions listening on {} and {internal_addr} (internal), runtime {}, bucket {}",
        config.addr, config.runtime_url, config.store.bucket
    );
    let (public_app, internal_app) = (public_router(state.clone()), internal_router(state));
    tokio::try_join!(async { axum::serve(public, public_app).await }, async {
        axum::serve(internal, internal_app).await
    },)?;
    Ok(())
}
