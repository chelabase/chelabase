//! The `mcp` service: the Chelabase MCP server behind the gateway's `/mcp` route.

use std::sync::Arc;

use mcp::app::router;
use mcp::config::Config;
use mcp::sandbox::Unavailable;
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
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| mcp::DEFAULT_LOG_FILTER.into()),
        )
        .init();
    let config = Config::from_env()?;
    if config.settings.log_code {
        warn!("CHELABASE_MCP_LOG_CODE=1: every tool call's code is written to the audit log");
    }
    // The code runtime is wired in m7 (#351); until then every tool call answers
    // `runtime_unavailable`.
    let app = router(config.settings, config.verifier, Arc::new(Unavailable));
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    info!("mcp listening on {}", config.listen);
    axum::serve(listener, app).await?;
    Ok(())
}
