//! The forwarder: one listener per entry. For each accepted connection it takes a slot,
//! resolves the entry's destination once, checks every candidate address, connects to the
//! first checked address that answers (never a second lookup) and copies bytes both ways
//! until both sides are done, passing a half-close through. Resolving and connecting share
//! one dial timeout; a forwarded connection has no idle timeout, only TCP keepalive.
//!
//! Each forwarded connection logs one line when it opens (the client, the entry and the
//! address dialled) and one when it ends (the same, with the outcome and the byte counts), so
//! a connection that stays up for weeks is visible while it lasts. A refused one logs one line.
//! Never any forwarded bytes.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::Duration;

use chelabase_http::outbound::HostLookup;
use socket2::{SockRef, TcpKeepalive};
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, warn};

use crate::allow::{Dest, Entry};
use crate::limits::{Limits, Permit};
use crate::policy::Policy;

/// Keepalive on both sides: first probe after this much silence.
pub const KEEPALIVE_TIME: Duration = Duration::from_secs(60);
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
pub const KEEPALIVE_RETRIES: u32 = 4;

/// What one entry's listener needs.
pub struct Forwarder {
    pub entry: Arc<Entry>,
    pub policy: Arc<Policy>,
    pub lookup: Arc<dyn HostLookup>,
    pub limits: Arc<Limits>,
    pub dial_timeout: Duration,
}

/// Accepts on `listener` until the task is stopped.
pub async fn serve(listener: TcpListener, fwd: Arc<Forwarder>) {
    loop {
        let (client, peer) = match listener.accept().await {
            Ok(r) => r,
            Err(e) => {
                warn!(entry = %fwd.entry.raw, error = %e, "accept failed");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let Some(permit) = fwd.limits.try_acquire(peer.ip()) else {
            warn!(client = %peer, entry = %fwd.entry.raw, outcome = "refused", reason = "connection limit", "connection");
            drop(client);
            continue;
        };
        tokio::spawn(handle(client, peer, fwd.clone(), permit));
    }
}

async fn handle(mut client: TcpStream, peer: SocketAddr, fwd: Arc<Forwarder>, permit: Permit) {
    let _permit = permit;
    let entry = &fwd.entry.raw;
    let dialled = match tokio::time::timeout(fwd.dial_timeout, dial(&fwd)).await {
        Ok(Ok(d)) => d,
        Ok(Err(why)) => {
            warn!(client = %peer, entry = %entry, outcome = "refused", reason = %why, "connection");
            return;
        }
        Err(_) => {
            warn!(client = %peer, entry = %entry, outcome = "refused", reason = "dial timeout", "connection");
            return;
        }
    };
    let Dialled {
        mut upstream,
        addr,
        skipped,
    } = dialled;
    keepalive(&client);
    keepalive(&upstream);
    let skipped = skipped.join(", ");
    info!(client = %peer, entry = %entry, outcome = "opened", dialled = %addr, skipped = %skipped, "connection");
    match tokio::io::copy_bidirectional(&mut client, &mut upstream).await {
        Ok((sent, received)) => {
            info!(client = %peer, entry = %entry, outcome = "forwarded", dialled = %addr, skipped = %skipped, sent, received, "connection");
        }
        Err(e) => {
            info!(client = %peer, entry = %entry, outcome = "forwarded", dialled = %addr, skipped = %skipped, error = %e, "connection");
        }
    }
}

/// A connected destination: the checked address it was dialled at, and the candidates the
/// policy refused (address and reason).
struct Dialled {
    upstream: TcpStream,
    addr: SocketAddrV4,
    skipped: Vec<String>,
}

/// Resolves once, checks every candidate, and connects to the first checked address that
/// answers. Only checked addresses are ever passed to `connect`.
async fn dial(fwd: &Forwarder) -> Result<Dialled, String> {
    let candidates = candidates(fwd)
        .await
        .map_err(|e| format!("{} does not resolve: {e}", fwd.entry.dest))?;
    if candidates.is_empty() {
        return Err(format!("{} does not resolve", fwd.entry.dest));
    }
    let mut checked: Vec<Ipv4Addr> = Vec::new();
    let mut skipped = Vec::new();
    for ip in candidates {
        match fwd.policy.check(ip) {
            Ok(v4) if !checked.contains(&v4) => checked.push(v4),
            Ok(_) => {}
            Err(why) => skipped.push(format!("{ip} {}", why.as_str())),
        }
    }
    if checked.is_empty() {
        return Err(format!("no address may be dialled: {}", skipped.join(", ")));
    }
    let mut failed = Vec::new();
    for v4 in checked {
        let addr = SocketAddrV4::new(v4, fwd.entry.port);
        match TcpStream::connect(addr).await {
            Ok(upstream) => {
                return Ok(Dialled {
                    upstream,
                    addr,
                    skipped,
                });
            }
            Err(e) => failed.push(format!("{addr} {e}")),
        }
    }
    Err(format!("connect failed: {}", failed.join(", ")))
}

/// The candidate addresses for this connection: one lookup for a name, the address itself
/// otherwise, in order and without repeats. The name is looked up as an absolute name
/// (trailing dot), so the system resolver never tries it under a search domain.
async fn candidates(fwd: &Forwarder) -> std::io::Result<Vec<IpAddr>> {
    let found = match &fwd.entry.dest {
        Dest::Name(name) => fwd.lookup.lookup(&format!("{name}.")).await?,
        Dest::Addr(addr) => vec![IpAddr::V4(*addr)],
    };
    let mut unique = Vec::with_capacity(found.len());
    for ip in found {
        if !unique.contains(&ip) {
            unique.push(ip);
        }
    }
    Ok(unique)
}

/// TCP keepalive and no delay; neither needs a privilege. A failure leaves the defaults.
fn keepalive(s: &TcpStream) {
    let params = TcpKeepalive::new()
        .with_time(KEEPALIVE_TIME)
        .with_interval(KEEPALIVE_INTERVAL)
        .with_retries(KEEPALIVE_RETRIES);
    let _ = SockRef::from(s).set_tcp_keepalive(&params);
    let _ = s.set_nodelay(true);
}
