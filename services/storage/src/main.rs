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
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| storage::DEFAULT_LOG_FILTER.into()),
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
        urls: storage::control::Urls::new(Arc::new(storage::control::Orchestrator::new(
            &config.internal_url,
            &config.internal_token,
        ))),
        inflight: storage::limits::InFlight::new(config.max_inflight_bytes)
            .with_body_idle(config.body_idle),
        // m18: object events, published on the NATS connection `events::spawn` makes.
        events: storage::events::ObjectEvents::nats(),
    });
    // #154: a totals line while events go unpublished.
    state.events.spawn_totals();
    // m9: NATS cache invalidation as storage's own user (`CHELABASE_NATS_*`), in the
    // background: storage serves on its cache lifetimes while NATS is away.
    storage::events::spawn(
        state.clone(),
        chelabase_nats::NatsConfig::from_env().map_err(anyhow::Error::msg)?,
    );
    let listener = tokio::net::TcpListener::bind(config.addr).await?;
    let internal = tokio::net::TcpListener::bind(config.internal_addr).await?;
    info!(
        "storage listening on {} and {} for the console (bucket {})",
        config.addr, config.internal_addr, config.store.bucket
    );
    let console = storage::internal::internal_router(state.clone(), &config.internal_token);
    tokio::try_join!(
        async { axum::serve(listener, router(state)).await },
        async { axum::serve(internal, console).await },
    )?;
    Ok(())
}
