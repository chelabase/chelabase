//! The `functions` service: edge function invocations behind the gateway, and their bundles.

use std::sync::Arc;

use anyhow::Context as _;
use chelabase_auth::internal::InternalVerifier;
use chelabase_metadata::{MetadataClient, MetadataConfig};
use chelabase_secrets::SecretKey;
use functions::app::{AppState, control_router, public_router, runtime_router};
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
    let public_addrs = resolve_all("CHELABASE_FUNCTIONS_ADDR", &config.addr).await?;
    // The internal listener's two sides: compose names the service's alias on the runtime's
    // network (bundle GETs) and on the orchestrator's and the worker's (uploads, invocations),
    // so each side listens there and nowhere else.
    let internal_addrs = resolve_all("CHELABASE_FUNCTIONS_INTERNAL_ADDR", &config.internal_addr).await?;
    let control_addrs = resolve_all("CHELABASE_FUNCTIONS_CONTROL_ADDR", &config.control_addr).await?;
    anyhow::ensure!(
        internal_addrs.iter().all(|a| !control_addrs.contains(a)),
        "CHELABASE_FUNCTIONS_INTERNAL_ADDR and CHELABASE_FUNCTIONS_CONTROL_ADDR must name different addresses"
    );
    let mut public = Vec::new();
    for addr in &public_addrs {
        public.push(tokio::net::TcpListener::bind(addr).await?);
    }
    let mut internal = Vec::new();
    for addr in &internal_addrs {
        internal.push(tokio::net::TcpListener::bind(addr).await?);
    }
    let mut control = Vec::new();
    for addr in &control_addrs {
        control.push(tokio::net::TcpListener::bind(addr).await?);
    }
    info!(
        "functions listening on {public_addrs:?}, {internal_addrs:?} (bundles) and {control_addrs:?} (uploads, invocations), runtime {}, bucket {}",
        config.runtime_url, config.store.bucket
    );
    let mut servers = tokio::task::JoinSet::new();
    for listener in public {
        let app = public_router(state.clone());
        servers.spawn(async move { axum::serve(listener, app).await });
    }
    for listener in internal {
        let app = runtime_router(state.clone());
        servers.spawn(async move { axum::serve(listener, app).await });
    }
    for listener in control {
        let app = control_router(state.clone());
        servers.spawn(async move { axum::serve(listener, app).await });
    }
    // The first listener to end ends the service.
    if let Some(ended) = servers.join_next().await {
        ended??;
        anyhow::bail!("a listener ended");
    }
    Ok(())
}

/// Every address the comma-separated `host:port` entries of `var` name, each resolved now.
async fn resolve_all(var: &str, list: &str) -> anyhow::Result<Vec<std::net::SocketAddr>> {
    let mut addrs = Vec::new();
    for entry in list.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let found: Vec<_> = tokio::net::lookup_host(entry)
            .await
            .with_context(|| format!("resolve {var} entry {entry}"))?
            .collect();
        anyhow::ensure!(!found.is_empty(), "{var} entry {entry} has no address");
        addrs.extend(found);
    }
    addrs.sort();
    addrs.dedup();
    anyhow::ensure!(!addrs.is_empty(), "{var} names no address");
    Ok(addrs)
}
