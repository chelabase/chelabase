//! The `service-runner` service: member service containers, label-scoped and hardened.

use std::sync::Arc;

use anyhow::Context as _;
use service_runner::app::{AppState, router};
use service_runner::config::Config;
use service_runner::docker::DockerBackend;
use service_runner::reconcile::{MetadataDesired, spawn_loop};
use service_runner::runner::Runner;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    chelabase_types::cli::handle(
        "service-runner",
        "Chelabase service runner: member service containers, label-scoped and hardened.",
        env!("CARGO_PKG_VERSION"),
    );
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "service_runner=info".into()),
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
    // Read only, and lazily: the runner starts while the database is coming up.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .connect_lazy(&database_url)
        .context("the metadata database URL")?;
    let backend = Arc::new(DockerBackend::connect()?);
    let runner = Arc::new(Runner::new(
        backend,
        &config.stack,
        &config.network,
        config.caps,
    ));
    spawn_loop(
        runner.clone(),
        Arc::new(MetadataDesired { pool }),
        config.reconcile_every,
    );
    let state = Arc::new(AppState {
        runner,
        functions_token: config.functions_token.clone(),
        worker_token: config.worker_token.clone(),
    });
    let mut addrs = Vec::new();
    for entry in config
        .addr
        .split(',')
        .map(str::trim)
        .filter(|e| !e.is_empty())
    {
        let found: Vec<_> = tokio::net::lookup_host(entry)
            .await
            .with_context(|| format!("resolve CHELABASE_SERVICE_RUNNER_ADDR entry {entry}"))?
            .collect();
        anyhow::ensure!(
            !found.is_empty(),
            "CHELABASE_SERVICE_RUNNER_ADDR entry {entry} has no address"
        );
        addrs.extend(found);
    }
    addrs.sort();
    addrs.dedup();
    anyhow::ensure!(
        !addrs.is_empty(),
        "CHELABASE_SERVICE_RUNNER_ADDR names no address"
    );
    info!(
        "service-runner listening on {addrs:?}, stack {}, network {}, caps {:?}",
        config.stack, config.network, config.caps
    );
    let mut servers = tokio::task::JoinSet::new();
    for addr in addrs {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let app = router(state.clone());
        servers.spawn(async move { axum::serve(listener, app).await });
    }
    if let Some(ended) = servers.join_next().await {
        ended??;
        anyhow::bail!("a listener ended");
    }
    Ok(())
}
