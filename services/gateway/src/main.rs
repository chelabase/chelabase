//! Chelabase HTTP API gateway: the single front door on plain HTTP.

use chelabase_auth::internal::InternalSigner;
use chelabase_auth::JwtVerifier;
use gateway::config::GatewayConfig;
use gateway::data_keys::DataKeyResolver;
use gateway::pipeline::Pipeline;
use gateway::proxy::Gateway;
use gateway::user_tokens::UserTokens;
use chelabase_auth::app_token::AppTokenVerifier;
use gateway::registry::{Registry, SharedRegistry};
use pingora_core::server::configuration::ServerConf;
use pingora_core::server::Server;
use std::sync::Arc;
use tracing::info;

fn main() -> anyhow::Result<()> {
    chelabase_types::cli::handle("gateway", "The Chelabase HTTP API gateway: the single front door.", env!("CARGO_PKG_VERSION"));
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(gateway::logging::env_filter(
            std::env::var("RUST_LOG").ok().as_deref(),
        ))
        .init();

    let config = GatewayConfig::from_env()?;
    let verifier = JwtVerifier::from_env()?;
    let signer = InternalSigner::from_env()?;
    let registry = Arc::new(SharedRegistry::new(Registry::from_entry(&config.entry)));
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
    let pipeline = Pipeline::new(registry, config.entry.studio_origin(), verifier, signer, keys, users);

    // Pingora's defaults assume zero-downtime binary upgrades with long grace
    // periods. Under Compose (the workbench) or k8s a process manager restarts us,
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
        Gateway::new(
            pipeline,
            config.upstreams.clone(),
            config.upstream_timeout,
            config.entry.scheme(),
        ),
    );
    for addr in &config.listen {
        service.add_tcp(addr);
    }
    server.add_service(service);

    info!(listen = %config.listen.join(","), "gateway listening");
    server.run_forever();
}
