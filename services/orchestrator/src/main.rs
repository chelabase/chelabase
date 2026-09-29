use cheladb_auth::internal::InternalVerifier;
use cheladb_metadata::{MetadataClient, MetadataConfig};
use cheladb_nats::{connect, ensure_control_stream, NatsConfig};
use cheladb_secrets::{key_check_value, verify_key_check, SecretKey};
use orchestrator::{build_router, AppState, NatsPorts};
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
    let identity = InternalVerifier::from_env().expect("internal identity verify key");

    let metadata = MetadataClient::connect(&metadata_config).await?;
    metadata.migrate().await?;

    let nats = connect(&nats_config).await?;
    let js = ensure_control_stream(&nats).await?;

    let secrets = SecretKey::from_env()?;
    ensure_secrets_key(&metadata, &secrets).await?;

    let state = AppState {
        metadata,
        js: NatsPorts { js, client: nats },
        identity,
        secrets,
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

/// Refuse to run with a secrets key that can't open existing credentials.
async fn ensure_secrets_key(metadata: &MetadataClient, key: &SecretKey) -> anyhow::Result<()> {
    use cheladb_metadata::KeyCheck;
    match metadata
        .ensure_key_check(&key_check_value(key), |stored| verify_key_check(key, stored))
        .await?
    {
        KeyCheck::Recorded | KeyCheck::Matches => Ok(()),
        KeyCheck::Replaced => {
            tracing::warn!("secrets key changed; no stored credentials existed, so the new key was adopted");
            Ok(())
        }
        KeyCheck::Mismatch { sealed_rows } => anyhow::bail!(
            "CHELADB_SECRETS_KEY is not the key {sealed_rows} database(s) were encrypted with. \
             Restore the original .cheladb/secrets/secrets.key; in local dev you can instead \
             remove every database with `just sweep-dbs`."
        ),
    }
}
