//! The `compute-egress` service. `compute-egress health` probes a running one (its health
//! port on loopback) and exits 0 when it answers.

use std::io::Read as _;
use std::net::{Ipv4Addr, TcpStream};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use chelabase_http::outbound::SystemLookup;
use compute_egress::allow::{ENV_HEALTH_PORT, Settings, health_port};
use compute_egress::policy::Policy;
use tracing::{error, info};

fn health() -> ExitCode {
    let port = match health_port(std::env::var(ENV_HEALTH_PORT).ok().as_deref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let answer =
        TcpStream::connect_timeout(&(Ipv4Addr::LOCALHOST, port).into(), Duration::from_secs(2))
            .and_then(|s| {
                s.set_read_timeout(Some(Duration::from_secs(2)))?;
                let mut buf = String::new();
                s.take(16).read_to_string(&mut buf)?;
                Ok(buf)
            });
    match answer {
        Ok(a) if a == "ok\n" => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    if std::env::args().skip(1).collect::<Vec<_>>() == ["health"] {
        return health();
    }
    chelabase_types::cli::handle(
        "compute-egress",
        "Chelabase compute egress: allow-listed TCP destinations for computes, and a resolver for the listed names only.",
        env!("CARGO_PKG_VERSION"),
    );
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "compute_egress=info".into()),
        )
        .init();
    let settings = match Settings::from_lookup(|k| std::env::var(k).ok()) {
        Ok(s) => s,
        Err(e) => {
            error!("{e}");
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let policy = Policy::for_settings(&settings);
    let mut running = match compute_egress::start(&settings, policy, Arc::new(SystemLookup)).await {
        Ok(r) => r,
        Err(e) => {
            error!("{e}");
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    info!(dns = %running.dns, health = %running.health, entries = running.listeners.len(), denied_networks = settings.deny.len(), "compute-egress started");
    for (entry, addr) in &running.listeners {
        info!(entry = %entry.raw, name = %entry.name, listen = %addr, "listening");
    }
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("SIGTERM handler: {e}");
            return ExitCode::FAILURE;
        }
    };
    let stop = async {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    };
    // A serve task that ends takes the process with it: the container restarts, and health
    // never reports `ok` for a service that has lost a listener.
    if let Err(why) = compute_egress::serve_until(&mut running, stop).await {
        error!("{why}; exiting");
        eprintln!("{why}; exiting");
        return ExitCode::FAILURE;
    }
    info!("compute-egress stopping");
    drop(running);
    ExitCode::SUCCESS
}
