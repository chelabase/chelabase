//! The `realtime` service behind the gateway's `/v1/realtime/*` routes.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    chelabase_types::cli::handle("realtime", "The Chelabase realtime service behind the gateway.", env!("CARGO_PKG_VERSION"));
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "realtime=info".into()),
        )
        .init();
    let config = realtime::config::Config::from_env()?;
    let service = realtime::app::build(config).await?;
    realtime::shutdown::signal().await;
    service.shutdown();
    service.wait().await
}
