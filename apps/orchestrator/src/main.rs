use cheladb_auth::JwtVerifier;
use cheladb_metadata::{MetadataClient, MetadataConfig};
use cheladb_nats::{connect, ensure_control_stream, NatsConfig};
use orchestrator::{build_router, AppState};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "orchestrator=info,tower_http=info,cheladb_auth=info".into()),
        )
        .init();

    let metadata_config = MetadataConfig::from_env().unwrap_or_else(|_| MetadataConfig {
        database_url: "postgres://cheladb:cheladb@127.0.0.1:5433/cheladb_metadata".into(),
    });
    let nats_config = NatsConfig::from_env().expect("nats config");
    let verifier = JwtVerifier::from_env().expect("zitadel auth config");

    let metadata = MetadataClient::connect(&metadata_config).await?;
    metadata.migrate().await?;

    let nats = connect(&nats_config).await?;
    let js = ensure_control_stream(&nats).await?;

    let state = AppState {
        metadata,
        js,
        verifier,
    };

    let app = build_router(Arc::new(state));

    let addr: SocketAddr = std::env::var("CHELADB_ORCHESTRATOR_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8081".into())
        .parse()?;
    info!("orchestrator listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
