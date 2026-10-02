use cheladb_auth::internal::InternalVerifier;
use cheladb_metadata::{MetadataClient, MetadataConfig};
use cheladb_nats::{connect, ensure_control_stream, NatsConfig};
use cheladb_neon::proxy_cache::ProxyCache;
use cheladb_neon::{DockerLocator, StorconClient};
use cheladb_types::mode::Mode;
use cheladb_types::role_settings::RoleSettings;
use cheladb_secrets::{key_check_value, verify_key_check, SecretKey};
use orchestrator::internal::{build_internal_router, config::InternalConfig, InternalState};
use orchestrator::{build_router, AppState, NatsPorts, RestInternal};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
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
    let internal_config = InternalConfig::from_env()?;
    let signer = internal_config.load_signer()?;
    let mode = Mode::from_env().map_err(anyhow::Error::msg)?;
    let roles = RoleSettings::from_lookup(
        mode,
        |name| std::env::var(name).ok(),
    )
    .map_err(anyhow::Error::msg)?;

    let metadata = MetadataClient::connect(&metadata_config).await?;
    metadata.migrate().await?;

    let nats = connect(&nats_config).await?;
    let js = ensure_control_stream(&nats).await?;

    let secrets = SecretKey::from_env()?;
    ensure_secrets_key(&metadata, &secrets).await?;

    let internal_state = Arc::new(InternalState {
        metadata: metadata.clone(),
        locator: DockerLocator::connect(&internal_config.compute_network)?,
        storcon: StorconClient::new(&internal_config.storcon_url, 0)
            .with_timeout(Duration::from_secs(10)),
        signer,
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?,
        proxy_token: internal_config.proxy_token.clone(),
        storcon_token: internal_config.storcon_token.clone(),
        data_keys_token: internal_config.data_keys_token.clone(),
        rest_token: internal_config.rest_token.clone(),
        secrets: secrets.clone(),
        roles,
        cache: Arc::new(ProxyCache::new(&internal_config.valkey_url)?),
        control_plane_uri: internal_config.control_plane_uri.clone(),
    });

    let state = AppState {
        metadata,
        js: NatsPorts { js, client: nats },
        identity,
        secrets,
        proxy_domain: proxy_domain_from_env(),
        rest: RestInternal {
            url: env_or("CHELADB_REST_INTERNAL_URL", "http://127.0.0.1:8086"),
            token: internal_config.rest_token.clone(),
            http: reqwest::Client::new(),
        },
        mode,
        proxy_ca_file: env_or("CHELADB_PROXY_CA_FILE", ".cheladb/neon/proxy-ca.crt"),
    };

    let app = build_router(Arc::new(state));

    let addr: SocketAddr = std::env::var("CHELADB_ORCHESTRATOR_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:8081".into())
        .parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let internal_listener = tokio::net::TcpListener::bind(internal_config.addr).await?;
    info!("orchestrator listening on {addr}");
    info!(
        "internal listener (proxy, compute_ctl, storage controller) on {}",
        internal_config.addr
    );
    tokio::try_join!(
        async { axum::serve(listener, app).await },
        async { axum::serve(internal_listener, build_internal_router(internal_state)).await },
    )?;
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

/// `CHELADB_PROXY_DOMAIN`, or `localtest.me` when unset or blank.
fn proxy_domain_from_env() -> String {
    std::env::var("CHELADB_PROXY_DOMAIN")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "localtest.me".to_string())
}

/// The variable, or `default` when unset or blank.
fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}
