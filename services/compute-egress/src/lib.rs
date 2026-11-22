//! `compute-egress`: the only path from a compute to the outside.
//!
//! It reads an allow-list of destinations ([`allow`]), answers DNS for the listed names only
//! ([`dns`]) and forwards TCP on one listener per listed destination ([`forward`]), dialling
//! only public addresses ([`policy`]) within per-client and total limits ([`limits`]).
//!
//! It opens ordinary TCP and UDP sockets only, every one on the listen address except the
//! health listener on loopback: no capability, no root, no raw or netlink socket and no write
//! outside stdout and stderr.

#![forbid(unsafe_code)]

pub mod allow;
pub mod dns;
pub mod forward;
pub mod limits;
pub mod policy;

#[cfg(test)]
mod tests;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use chelabase_http::outbound::HostLookup;
use tokio::net::{TcpListener, UdpSocket};
use tokio::task::JoinSet;

use crate::allow::{Entry, Settings};
use crate::dns::Names;
use crate::forward::Forwarder;
use crate::limits::Limits;
use crate::policy::Policy;

/// DNS-over-TCP connections at once, in total and per source address.
pub const DNS_TCP_MAX: usize = 128;
pub const DNS_TCP_MAX_PER_CLIENT: usize = 8;

/// A started service: what it bound, and its serve tasks (stopped when this drops). Each task
/// returns its own name, for the message when it ends.
pub struct Running {
    /// The resolver, UDP and TCP.
    pub dns: SocketAddr,
    pub health: SocketAddr,
    /// One listener per entry, in list order.
    pub listeners: Vec<(Arc<Entry>, SocketAddr)>,
    tasks: JoinSet<&'static str>,
}

impl Running {
    /// Resolves when any serve task has ended, naming it and how. They never end on their own,
    /// so an end is a failure: a panic, or a loop that returned.
    pub async fn exited(&mut self) -> String {
        match self.tasks.join_next().await {
            Some(Ok(name)) => format!("{name} stopped"),
            Some(Err(e)) if e.is_panic() => "a serve task panicked".to_string(),
            Some(Err(e)) => format!("a serve task failed: {e}"),
            // No tasks at all (never the case: the resolver and health always run).
            None => "no serve task is running".to_string(),
        }
    }
}

/// Runs until `stop` completes (`Ok`) or any serve task ends (`Err`, saying which). The
/// binary exits non-zero on `Err`, so the container restarts and a dead listener is never
/// hidden behind a healthy health port.
pub async fn serve_until(
    running: &mut Running,
    stop: impl std::future::Future<Output = ()>,
) -> Result<(), String> {
    tokio::select! {
        () = stop => Ok(()),
        why = running.exited() => Err(why),
    }
}

/// Binds every socket first (any failure: nothing serves), then serves.
pub async fn start(
    settings: &Settings,
    policy: Policy,
    lookup: Arc<dyn HostLookup>,
) -> Result<Running, String> {
    let dns_addr = SocketAddr::new(settings.listen, settings.dns_port);
    let health_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), settings.health_port);
    let bind_err = |what: String, addr: SocketAddr| {
        move |e: std::io::Error| format!("bind {what} on {addr}: {e}")
    };
    let dns_udp = UdpSocket::bind(dns_addr)
        .await
        .map_err(bind_err("the resolver (UDP)".into(), dns_addr))?;
    let dns_tcp = TcpListener::bind(dns_addr)
        .await
        .map_err(bind_err("the resolver (TCP)".into(), dns_addr))?;
    let health = TcpListener::bind(health_addr)
        .await
        .map_err(bind_err("the health port".into(), health_addr))?;
    let mut bound = Vec::with_capacity(settings.entries.len());
    for entry in &settings.entries {
        let addr = SocketAddr::new(settings.listen, entry.listen_port);
        let listener = TcpListener::bind(addr)
            .await
            .map_err(bind_err(format!("entry {:?}", entry.raw), addr))?;
        bound.push((Arc::new(entry.clone()), listener));
    }

    let names = Arc::new(Names::new(
        settings.entries.iter().map(|e| e.name.clone()),
        settings.advertise,
    ));
    let policy = Arc::new(policy);
    let limits = Limits::new(
        settings.max_connections,
        settings.max_connections_per_client,
    );
    let dns_limits = Limits::new(DNS_TCP_MAX, DNS_TCP_MAX_PER_CLIENT);
    let mut tasks = JoinSet::new();
    let udp_names = names.clone();
    tasks.spawn(async move {
        dns::serve_udp(dns_udp, udp_names).await;
        "the resolver (UDP)"
    });
    tasks.spawn(async move {
        dns::serve_tcp(dns_tcp, names, dns_limits).await;
        "the resolver (TCP)"
    });
    tasks.spawn(async move {
        serve_health(health).await;
        "the health listener"
    });
    let mut listeners = Vec::with_capacity(bound.len());
    for (entry, listener) in bound {
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        listeners.push((entry.clone(), addr));
        let fwd = Arc::new(Forwarder {
            entry,
            policy: policy.clone(),
            lookup: lookup.clone(),
            limits: limits.clone(),
            dial_timeout: settings.dial_timeout,
        });
        tasks.spawn(async move {
            forward::serve(listener, fwd).await;
            "a forwarder"
        });
    }
    Ok(Running {
        dns: dns_addr,
        health: health_addr,
        listeners,
        tasks,
    })
}

/// Answers `ok` to every connection and closes it. An accept error (descriptors exhausted)
/// backs off like the other listeners instead of retrying at full speed.
async fn serve_health(listener: TcpListener) {
    loop {
        match listener.accept().await {
            // Three bytes into a fresh socket's send buffer: never blocks.
            Ok((s, _)) => {
                let _ = s.try_write(b"ok\n");
            }
            Err(e) => {
                tracing::warn!(error = %e, "health: accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }
}
