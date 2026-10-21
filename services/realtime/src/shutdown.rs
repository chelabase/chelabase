//! Shutdown (spec §8.2): on SIGTERM (or ctrl-c) every socket is closed with 1012, then the
//! listeners stop; T8 also releases its leases and closes its replication connections. All of
//! it within one deadline, [`SHUTDOWN_BUDGET`], inside compose's stop grace period (10 s).

use std::time::Duration;

/// How long sockets get to receive their 1012 before the process exits.
pub const DRAIN: Duration = Duration::from_secs(3);

/// Every phase of a shutdown together (sockets, listeners, the change source's releases):
/// whatever still hangs past it is abandoned, with a margin to compose's 10 s grace.
pub const SHUTDOWN_BUDGET: Duration = Duration::from_secs(7);

/// Resolves on SIGTERM or ctrl-c.
pub async fn signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
    tracing::info!("shutting down: closing every socket with 1012");
}
