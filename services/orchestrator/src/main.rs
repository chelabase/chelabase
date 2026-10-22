use chelabase_auth::internal::InternalVerifier;
use chelabase_metadata::{MetadataClient, MetadataConfig};
use chelabase_query::app_users::PgUsersDb;
use chelabase_nats::{connect, ensure_control_stream, ensure_neon_events_stream, NatsConfig};
use chelabase_neon::proxy_cache::ProxyCache;
use chelabase_neon::{DockerLocator, StorconClient};
use chelabase_types::mode::Mode;
use chelabase_types::mode_policy::ModePolicy;
use chelabase_types::role_settings::RoleSettings;
use chelabase_secrets::{key_check_value, verify_key_check, SecretKey};
use orchestrator::internal::{build_internal_router, config::InternalConfig, InternalState};
use orchestrator::compute_tls::ComputeTls;
use orchestrator::{build_router, AppState, Console, NatsPorts, ProxyAddress, RestInternal};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| orchestrator::DEFAULT_LOG_FILTER.into()),
        )
        .init();

    let metadata_config = MetadataConfig::from_env().unwrap_or_else(|_| MetadataConfig {
        database_url: "postgres://chelabase:chelabase@127.0.0.1:5433/chelabase_metadata".into(),
    });
    let nats_config = NatsConfig::from_env().expect("nats config");
    let identity = InternalVerifier::from_env().expect("internal identity verify key");
    let entry = chelabase_types::entry::Entry::from_env().map_err(anyhow::Error::new)?;
    let internal_config = InternalConfig::from_env()?;
    // Unset is a startup error (P13); empty means no address rule.
    let member_logins = internal_config.member_logins()?;
    let realtime_logins = internal_config.realtime_logins()?;
    let signer = internal_config.load_signer()?;
    let mode = Mode::from_env().map_err(anyhow::Error::msg)?;
    entry.check_mode(mode).map_err(anyhow::Error::new)?;
    internal_config.check_mode(&ModePolicy::new(mode))?;
    let roles = RoleSettings::from_lookup(
        mode,
        |name| std::env::var(name).ok(),
    )
    .map_err(anyhow::Error::msg)?;

    let metadata = MetadataClient::connect(&metadata_config).await?;
    metadata.migrate().await?;

    let nats = connect(&nats_config).await?;
    let js = ensure_control_stream(&nats).await?;
    // The storage events (carried gaps T4, R10): only the orchestrator creates or fixes them.
    ensure_neon_events_stream(&js).await?;

    let secrets = SecretKey::from_env()?;
    ensure_secrets_key(&metadata, &secrets).await?;

    let data_api_database_cap = orchestrator::data_api_database_cap(
        std::env::var("CHELABASE_BRANCH_DATA_API_DATABASE_CAP").ok().as_deref(),
    )
    .map_err(anyhow::Error::msg)?;
    let table_api_dry_run_max_bytes = orchestrator::table_api_dry_run_max_bytes(
        std::env::var("CHELABASE_TABLE_API_DRY_RUN_MAX_BYTES").ok().as_deref(),
    )
    .map_err(anyhow::Error::msg)?;
    let rest = RestInternal {
        url: env_or("CHELABASE_REST_INTERNAL_URL", "http://127.0.0.1:8086"),
        token: internal_config.rest_token.clone(),
        http: reqwest::Client::new(),
    };

    // The storage controller runs without --dev: an admin-scoped NeonJWT from the compute-JWT key.
    let storcon_admin = signer.storage_token(&chelabase_neon::jwt::StorageScope::Admin)?;
    let internal_state = Arc::new(InternalState {
        metadata: metadata.clone(),
        locator: DockerLocator::connect(&internal_config.compute_network, &internal_config.stack)?,
        storcon: StorconClient::new(&internal_config.storcon_url, 0)
            .with_timeout(Duration::from_secs(10))
            .with_token(storcon_admin),
        signer,
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?,
        proxy_token: internal_config.proxy_token.clone(),
        storcon_token: internal_config.storcon_token.clone(),
        data_keys_token: internal_config.data_keys_token.clone(),
        rest_token: internal_config.rest_token.clone(),
        realtime_token: internal_config.realtime_token.clone(),
        rt_events: Arc::new(nats.clone()),
        secrets: secrets.clone(),
        roles,
        // As the `control` ACL user (CHELABASE_VALKEY_USER / CHELABASE_VALKEY_PASSWORD_FILE).
        cache: Arc::new(ProxyCache::from_lookup(&internal_config.valkey_url, |name| {
            std::env::var(name).ok()
        })?),
        resync: Arc::new(nats.clone()),
        control_plane_uri: internal_config.control_plane_uri.clone(),
        rest: rest.clone(),
        app_auth_token: internal_config.app_auth_token.clone(),
        app_sessions_token: internal_config.app_sessions_token.clone(),
        app_refresh_ttl: internal_config.app_refresh_ttl,
        actions_signing_key: internal_config.actions_signing_key.clone(),
        zitadel: internal_config.load_zitadel(),
        app_jobs: Arc::new(NatsPorts { js: js.clone(), client: nats.clone() }),
        policy: ModePolicy::new(mode),
        member_logins,
        realtime_logins,
        control_locator: DockerLocator::connect(
            &internal_config.compute_control_network,
            &internal_config.stack,
        )?,
        attach_events: Arc::new(NatsPorts { js: js.clone(), client: nats.clone() }),
    });

    // The console and the SQL editor reach computes over TLS on the control network.
    let compute_tls = Arc::new(ComputeTls::new(internal_config.compute_tls_dir.clone()));
    compute_tls.spawn_reload();

    let rt_events: Arc<dyn orchestrator::internal::realtime::RtEvents> = Arc::new(nats.clone());
    let state = AppState {
        metadata,
        realtime: orchestrator::internal::realtime::RealtimeAuth {
            token: internal_config.realtime_token.clone(),
            // The gateway's settings (issuer, JWKS URL), the same env names.
            verifier: chelabase_auth::JwtVerifier::from_env()?,
        },
        rt_events,
        js: NatsPorts { js, client: nats },
        identity,
        secrets,
        proxy: ProxyAddress::from_lookup(|name| std::env::var(name).ok())?,
        rest,
        policy: ModePolicy::new(mode),
        operator_org: std::env::var("CHELABASE_OPERATOR_ORG_ID")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty()),
        proxy_ca_file: env_or("CHELABASE_PROXY_CA_FILE", ".chelabase/neon/proxy-ca.crt"),
        data_api_database_cap,
        locator: Arc::new(DockerLocator::connect(
            &internal_config.compute_control_network,
            &internal_config.stack,
        )?),
        console: Console::default(),
        zitadel: zitadel_admin(&entry.issuer()),
        users_db: Arc::new(PgUsersDb::new()),
        sql_editor: Default::default(),
        table_api_dry_run_max_bytes,
        studio_url: entry.studio_origin(),
        compute_tls: Some(compute_tls),
    };

    let app = build_router(Arc::new(state));

    let addr: SocketAddr = std::env::var("CHELABASE_ORCHESTRATOR_ADDR")
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
    use chelabase_metadata::KeyCheck;
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
            "CHELABASE_SECRETS_KEY is not the key {sealed_rows} database(s) were encrypted with. \
             Restore the original .chelabase/secrets/secrets.key; in local dev you can instead \
             remove every database with `just sweep-dbs`."
        ),
    }
}

/// Zitadel's admin API (at `issuer`) with the admin PAT from `CHELABASE_ZITADEL_ADMIN_PAT_FILE`;
/// `None` (warned) when that is unset or the file can't be read.
fn zitadel_admin(issuer: &str) -> Option<chelabase_zitadel::client::Zitadel> {
    let Some(file) = std::env::var("CHELABASE_ZITADEL_ADMIN_PAT_FILE")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    else {
        warn!("CHELABASE_ZITADEL_ADMIN_PAT_FILE is not set; app users can't be enabled");
        return None;
    };
    match std::fs::read_to_string(&file) {
        Ok(pat) if !pat.trim().is_empty() => Some(chelabase_zitadel::client::Zitadel::new(issuer, pat.trim())),
        Ok(_) => {
            warn!("{file} is empty; app users can't be enabled");
            None
        }
        Err(err) => {
            warn!("can't read {file}: {err}; app users can't be enabled");
            None
        }
    }
}

/// The variable, or `default` when unset or blank.
fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_string())
}
