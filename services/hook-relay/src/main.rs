use anyhow::Context;
use cheladb_hook_relay::{DEFAULT_DEADLINE, DEFAULT_SLOT_TIMEOUT, RelayConfig, app};
use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;

/// Relays storage-controller hooks to the worktree slot that knows the tenant.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Address to listen on.
    #[arg(
        long,
        env = "CHELADB_HOOK_RELAY_LISTEN",
        default_value = "0.0.0.0:8099"
    )]
    listen: SocketAddr,
    /// Directory of slot registry files (`<N>.env`, `KEY=VALUE` lines), read on every request.
    #[arg(long, env = "CHELADB_HOOK_RELAY_SLOTS_DIR", default_value = "/slots")]
    slots_dir: PathBuf,
    /// Host the slots' internal listeners are reached on.
    #[arg(
        long,
        env = "CHELADB_HOOK_RELAY_UPSTREAM_HOST",
        default_value = "host.docker.internal"
    )]
    upstream_host: String,
    /// Internal port of slot 0 (the main checkout, which has no registry file); 0 disables it.
    #[arg(long, env = "CHELADB_HOOK_RELAY_SLOT0_PORT", default_value_t = 8083)]
    slot0_port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "cheladb_hook_relay=info".into()),
        )
        .init();
    let args = Args::parse();
    let cfg = RelayConfig {
        slots_dir: args.slots_dir,
        upstream_host: args.upstream_host,
        slot0_port: (args.slot0_port != 0).then_some(args.slot0_port),
        slot_timeout: DEFAULT_SLOT_TIMEOUT,
        deadline: DEFAULT_DEADLINE,
    };
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;
    tracing::info!(listen = %args.listen, "hook relay listening");
    axum::serve(listener, app(cfg)).await?;
    Ok(())
}
