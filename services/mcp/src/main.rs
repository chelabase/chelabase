//! The `mcp` service: the Chelabase MCP server behind the gateway's `/mcp` route.

use std::sync::Arc;

use mcp::app::router;
use mcp::config::{Config, resolve_all};
use mcp::host::Host;
use mcp::internal::Registry;
use mcp::runtime::Runtime;
use mcp::sandbox::{Sandbox, Unavailable};
use tracing::{info, warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    chelabase_types::cli::handle(
        "mcp",
        "The Chelabase MCP server behind the gateway's /mcp route.",
        env!("CARGO_PKG_VERSION"),
    );
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::new(mcp::log_filter(
                std::env::var("RUST_LOG").ok().as_deref(),
            )),
        )
        .init();
    let config = Config::from_env()?;
    if config.settings.log_code {
        warn!("CHELABASE_MCP_LOG_CODE=1: every tool call's code is written to the audit log");
    }
    let registry = Registry::default();
    let sandbox: Arc<dyn Sandbox> = match &config.runtime_url {
        Some(url) => Arc::new(Runtime::new(
            url,
            config.settings.limits.timeout,
            registry.clone(),
        )?),
        None => {
            warn!(
                "CHELABASE_MCP_RUNTIME_URL is unset: every tool call answers runtime_unavailable"
            );
            Arc::new(Unavailable)
        }
    };
    // Each listener binds exactly the addresses its entries resolve to: the runtime's network
    // reaches the internal one only.
    let public_addrs = resolve_all("CHELABASE_MCP_LISTEN", &config.listen).await?;
    let internal_addrs =
        resolve_all("CHELABASE_MCP_INTERNAL_LISTEN", &config.internal_listen).await?;
    anyhow::ensure!(
        internal_addrs.iter().all(|a| !public_addrs.contains(a)),
        "CHELABASE_MCP_LISTEN and CHELABASE_MCP_INTERNAL_LISTEN must name different addresses"
    );
    let host = Arc::new(Host::new(
        &config.host,
        &config.settings.api_base,
        registry.clone(),
    )?);
    if config.host.api_url.is_none() {
        warn!("CHELABASE_MCP_API_URL is unset: every API call from execute answers upstream_unavailable");
    }
    let app = router(config.settings, config.verifier, sandbox);
    let mut servers = tokio::task::JoinSet::new();
    for addr in &public_addrs {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let app = app.clone();
        servers.spawn(async move { axum::serve(listener, app).await });
    }
    for addr in &internal_addrs {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let app = mcp::internal::router().merge(mcp::host::router(host.clone()));
        servers.spawn(async move { axum::serve(listener, app).await });
    }
    info!("mcp listening on {public_addrs:?}, internal {internal_addrs:?}");
    // The first listener to end ends the service.
    if let Some(ended) = servers.join_next().await {
        ended??;
        anyhow::bail!("a listener ended");
    }
    Ok(())
}
