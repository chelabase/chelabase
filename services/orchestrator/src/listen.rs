//! Where the management API listens (#318): `CHELABASE_ORCHESTRATOR_ADDR` is a comma-separated
//! list of `host:port` entries, each resolved once at start. Compose names the orchestrator's
//! alias on the default network (`orchestrator-api`, declared there only) and loopback (the
//! healthcheck), so the listener has no socket on `compute-internal` or `control-internal` and a
//! compute can't connect to it. An entry that resolves to nothing fails the start: a listener
//! never falls back to the any-address.

use anyhow::Context;
use std::net::SocketAddr;

/// Every address the comma-separated `host:port` entries of `list` (from `var`) name, each
/// resolved now, sorted and without duplicates.
pub async fn resolve_listen_addrs(var: &str, list: &str) -> anyhow::Result<Vec<SocketAddr>> {
    let mut addrs = Vec::new();
    for entry in list.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let found: Vec<_> = tokio::net::lookup_host(entry)
            .await
            .with_context(|| format!("resolve {var} entry {entry}"))?
            .collect();
        anyhow::ensure!(!found.is_empty(), "{var} entry {entry} has no address");
        addrs.extend(found);
    }
    addrs.sort();
    addrs.dedup();
    anyhow::ensure!(!addrs.is_empty(), "{var} names no address");
    Ok(addrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VAR: &str = "CHELABASE_ORCHESTRATOR_ADDR";

    #[tokio::test]
    async fn binds_each_named_address_once() {
        let addrs = resolve_listen_addrs(VAR, "127.0.0.1:8081, localhost:8081,127.0.0.1:8081")
            .await
            .unwrap();
        let v4: SocketAddr = "127.0.0.1:8081".parse().unwrap();
        assert!(addrs.contains(&v4), "{addrs:?}");
        assert_eq!(addrs.iter().filter(|a| **a == v4).count(), 1, "{addrs:?}");
        assert!(addrs.iter().all(|a| a.ip().is_loopback()), "{addrs:?}");
    }

    #[tokio::test]
    async fn a_single_address_is_one_listener() {
        let addrs = resolve_listen_addrs(VAR, "127.0.0.1:8081").await.unwrap();
        assert_eq!(addrs, vec!["127.0.0.1:8081".parse::<SocketAddr>().unwrap()]);
    }

    #[tokio::test]
    async fn an_unresolvable_entry_fails_instead_of_falling_back() {
        let err = resolve_listen_addrs(VAR, "127.0.0.1:8081,no-such-host.invalid:8081")
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("no-such-host.invalid:8081"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn an_empty_list_fails() {
        for list in ["", " , "] {
            let err = resolve_listen_addrs(VAR, list).await.unwrap_err();
            assert!(err.to_string().contains(VAR), "{err:#}");
        }
    }

    #[tokio::test]
    async fn an_entry_without_a_port_fails() {
        assert!(resolve_listen_addrs(VAR, "127.0.0.1").await.is_err());
    }
}
