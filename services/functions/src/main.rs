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
    // Every `host:port` in the list, each name resolved at start: compose names the service's
    // aliases on the internal networks only (the runtime's, and the orchestrator's and the
    // worker's), so the internal listener is on those and nowhere else.
    let mut internal_addrs = Vec::new();
    for entry in config.internal_addr.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let found: Vec<_> = tokio::net::lookup_host(entry)
            .await
            .with_context(|| format!("resolve CHELABASE_FUNCTIONS_INTERNAL_ADDR entry {entry}"))?
            .collect();
        anyhow::ensure!(!found.is_empty(), "CHELABASE_FUNCTIONS_INTERNAL_ADDR entry {entry} has no address");
        internal_addrs.extend(found);
    }
    internal_addrs.sort();
    internal_addrs.dedup();
    anyhow::ensure!(!internal_addrs.is_empty(), "CHELABASE_FUNCTIONS_INTERNAL_ADDR names no address");
    let public = tokio::net::TcpListener::bind(config.addr).await?;
    let mut internal = Vec::new();
    for addr in &internal_addrs {
        internal.push(tokio::net::TcpListener::bind(addr).await?);
    }
    info!(
        "functions listening on {} and {internal_addrs:?} (internal), runtime {}, bucket {}",
        config.addr, config.runtime_url, config.store.bucket
    );
    let mut servers = tokio::task::JoinSet::new();
    let public_app = public_router(state.clone());
    servers.spawn(async move { axum::serve(public, public_app).await });
    for listener in internal {
        let app = internal_router(state.clone());
        servers.spawn(async move { axum::serve(listener, app).await });
    }
    // The first listener to end ends the service.
    if let Some(ended) = servers.join_next().await {
        ended??;
        anyhow::bail!("a listener ended");
    }
    Ok(())
}
