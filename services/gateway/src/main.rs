//! Cheladb HTTP API gateway: the single front door on plain HTTP.

use cheladb_auth::internal::InternalSigner;
use cheladb_auth::JwtVerifier;
use gateway::config::GatewayConfig;
use gateway::data_keys::DataKeyResolver;
use gateway::pipeline::Pipeline;
use gateway::proxy::Gateway;
use gateway::user_tokens::UserTokens;
use cheladb_auth::app_token::AppTokenVerifier;
use gateway::registry::{Registry, SharedRegistry};
use pingora_core::server::configuration::ServerConf;
use pingora_core::server::Server;
use std::sync::Arc;
use tracing::info;

fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gateway=info,cheladb_auth=info,pingora=warn".into()),
        )
        .init();

    let config = GatewayConfig::from_env()?;
    let verifier = JwtVerifier::from_env()?;
    let signer = InternalSigner::from_env()?;
    let registry = Arc::new(SharedRegistry::new(
        Registry::from_config(&config.api_hosts).map_err(anyhow::Error::msg)?,
    ));
    let keys = Arc::new(DataKeyResolver::new(&config.internal_url, &config.data_keys_token));
    let app_verifier = config
        .app_jwt_public_key
        .as_deref()
        .map(|path| AppTokenVerifier::from_pem_file(std::path::Path::new(path)))
        .transpose()?;
    let users = Arc::new(UserTokens::new(
        app_verifier,
        &config.internal_url,
        &config.app_sessions_token,
    ));
    let pipeline = Pipeline::new(registry, config.studio_origins.clone(), verifier, signer, keys, users);

    // Pingora's defaults assume zero-downtime binary upgrades with long grace
    // periods. Under `just dev`, Compose, or k8s a process manager restarts us,
    // so exit promptly on SIGTERM.
    let conf = ServerConf {
        grace_period_seconds: Some(0),
        graceful_shutdown_timeout_seconds: Some(5),
        threads: std::thread::available_parallelism().map_or(1, |n| n.get()),
        ..ServerConf::default()
    };

    let mut server = Server::new_with_opt_and_conf(None, conf);
    server.bootstrap();
    let mut service = pingora_proxy::http_proxy_service(
        &server.configuration,
        Gateway::new(pipeline, config.upstreams.clone(), config.upstream_timeout),
    );
    service.add_tcp(&config.listen);
    server.add_service(service);

    info!(listen = %config.listen, "gateway listening");
    server.run_forever();
}
