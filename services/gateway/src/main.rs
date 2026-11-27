//! Chelabase HTTP API gateway: the single front door on plain HTTP.

use chelabase_auth::internal::InternalSigner;
use gateway::client_addr::FunctionsRuntime;
use gateway::config::GatewayConfig;
use gateway::data_keys::DataKeyResolver;
use gateway::pipeline::Pipeline;
use gateway::proxy::Gateway;
use gateway::request_limits::RequestLimits;
use gateway::user_tokens::UserTokens;
use chelabase_auth::app_token::AppTokenVerifier;
use gateway::registry::{Registry, SharedRegistry};
use pingora_core::server::configuration::ServerConf;
use pingora_core::server::Server;
use std::sync::Arc;
use tracing::info;

/// How often the functions runtime's host names are resolved again (#426).
const FUNCTIONS_RUNTIME_REFRESH: std::time::Duration = std::time::Duration::from_secs(5);

fn main() -> anyhow::Result<()> {
    chelabase_types::cli::handle("gateway", "The Chelabase HTTP API gateway: the single front door.", env!("CARGO_PKG_VERSION"));
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(gateway::logging::env_filter(
            std::env::var("RUST_LOG").ok().as_deref(),
        ))
        .init();

    let config = GatewayConfig::from_env()?;
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
    // #119: member tokens signed by auth; unset, every member token is refused.
    let members = chelabase_auth::member_token::MemberTokenVerifier::from_lookup(|name| std::env::var(name).ok())?;
    match &members {
        Some(m) => info!(kid = m.kid(), "member tokens are verified with CHELABASE_MEMBER_JWT_PUBLIC_KEY"),
        None => tracing::warn!("CHELABASE_MEMBER_JWT_PUBLIC_KEY is not set: every bearer is refused"),
    }
    // MCP core m2: `/mcp` and its metadata exist only with an mcp upstream.
    let mcp_api_base = config
        .upstreams
        .addr(chelabase_routes::Service::Mcp)
        .map(|_| config.entry.api_base());
    let pipeline = Pipeline::new(registry, config.entry.studio_origin(), signer, keys, users)
        .with_member_tokens(members)
        .with_mcp(mcp_api_base.as_deref())
        .with_request_limits(Arc::new(RequestLimits::new(&config.request_limits)));
    info!(limits = ?config.request_limits, "data API and storage requests are limited per key and client");

    // #426: the functions runtime's addresses. A host name (compose's service name) is
    // resolved now and every few seconds, as the runtime's address changes when it is recreated.
    let functions_runtime = Arc::new(FunctionsRuntime::new(config.functions_runtime.clone()));
    if functions_runtime.has_hosts() {
        functions_runtime.refresh();
        let runtime = functions_runtime.clone();
        std::thread::Builder::new()
            .name("functions-runtime-dns".into())
            .spawn(move || loop {
                std::thread::sleep(FUNCTIONS_RUNTIME_REFRESH);
                runtime.refresh();
            })?;
    }
    if !config.functions_runtime.is_empty() {
        info!(runtime = ?config.functions_runtime, "the functions runtime's original-caller address is believed");
    }

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
        )
        .with_trusted_proxies(config.trusted_proxies.clone())
        .with_functions_runtime(functions_runtime),
    );
    for addr in &config.listen {
        service.add_tcp(addr);
    }
    server.add_service(service);

    info!(listen = %config.listen.join(","), "gateway listening");
    server.run_forever();
}
