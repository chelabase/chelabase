//! Hostname → scope resolution.
//!
//! Held behind an atomic swap so a later metadata-backed source (hosted mode,
//! per-project hostnames) can replace it without touching the request path.

use arc_swap::ArcSwap;
use chelabase_routes::Scope;
use std::collections::HashSet;
use std::sync::Arc;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Registry {
    api_hosts: HashSet<String>,
}

impl Registry {
    /// An empty `api_hosts` means every host is Api (the dev default).
    pub fn new<I, S>(api_hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            api_hosts: api_hosts
                .into_iter()
                .map(|h| normalize_host(h.as_ref()))
                .filter(|h| !h.is_empty())
                .collect(),
        }
    }

    /// Like [`Registry::new`], but rejects entries that can never match a
    /// request, so a typo fails at startup instead of 404-ing every request.
    pub fn from_config<I, S>(api_hosts: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let hosts = api_hosts
            .into_iter()
            .map(|h| h.as_ref().trim().to_string())
            .filter(|h| !h.is_empty())
            .map(|h| validate_host_entry(&h).map(|()| h))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self::new(hosts))
    }

    pub fn scope_for(&self, host: Option<&str>) -> Option<Scope> {
        if self.api_hosts.is_empty() {
            return Some(Scope::Api);
        }
        let host = normalize_host(host?);
        self.api_hosts
            .contains(&host)
            .then_some(Scope::Api)
    }
}

/// Check one `CHELABASE_API_HOSTS` entry: a hostname or IPv4 address, or a
/// bracketed IPv6 address, optionally with `:port`.
fn validate_host_entry(entry: &str) -> Result<(), String> {
    let port_ok = |port: &str| port.parse::<u16>().is_ok();
    if let Some(rest) = entry.strip_prefix('[') {
        let (addr, after) = rest
            .split_once(']')
            .ok_or_else(|| format!("api host {entry:?}: missing closing ]"))?;
        let addr_ok = !addr.is_empty()
            && addr.chars().all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.');
        let after_ok = after.is_empty() || after.strip_prefix(':').is_some_and(port_ok);
        return if addr_ok && after_ok {
            Ok(())
        } else {
            Err(format!("api host {entry:?} is not a valid [IPv6]:port"))
        };
    }
    if entry.matches(':').count() > 1 {
        return Err(format!(
            "api host {entry:?} looks like an IPv6 address; bracket it, e.g. [{entry}]"
        ));
    }
    let host = match entry.split_once(':') {
        Some((host, port)) if port_ok(port) => host,
        Some(_) => return Err(format!("api host {entry:?} has an invalid port")),
        None => entry,
    };
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty()
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return Err(format!("api host {entry:?} is not a valid hostname"));
    }
    Ok(())
}

/// Lowercase, drop any port and a single trailing dot:
/// `API.Example.com.:8000` → `api.example.com`, `[::1]:8000` → `[::1]`.
pub fn normalize_host(raw: &str) -> String {
    let raw = raw.trim();
    let without_port = if let Some(rest) = raw.strip_prefix('[') {
        match rest.find(']') {
            Some(end) => &raw[..end + 2],
            None => raw,
        }
    } else {
        raw.rsplit_once(':').map_or(raw, |(host, _)| host)
    };
    let without_dot = without_port.strip_suffix('.').unwrap_or(without_port);
    without_dot.to_ascii_lowercase()
}

/// The live registry. Each request loads it once.
pub struct SharedRegistry(ArcSwap<Registry>);

impl SharedRegistry {
    pub fn new(registry: Registry) -> Self {
        Self(ArcSwap::from_pointee(registry))
    }

    pub fn load(&self) -> Arc<Registry> {
        self.0.load_full()
    }

    pub fn replace(&self, registry: Registry) {
        self.0.store(Arc::new(registry));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_treats_every_host_as_api() {
        let registry = Registry::new(Vec::<String>::new());
        assert_eq!(registry.scope_for(Some("anything:8000")), Some(Scope::Api));
        assert_eq!(registry.scope_for(None), Some(Scope::Api));
    }

    #[test]
    fn listed_host_matches_regardless_of_port_and_case() {
        let registry = Registry::new(["api.example.com"]);
        assert_eq!(registry.scope_for(Some("API.Example.com:8000")), Some(Scope::Api));
        assert_eq!(registry.scope_for(Some("api.example.com")), Some(Scope::Api));
    }

    #[test]
    fn unlisted_host_has_no_scope() {
        let registry = Registry::new(["api.example.com"]);
        assert_eq!(registry.scope_for(Some("evil.example.com")), None);
    }

    #[test]
    fn missing_host_is_rejected_when_hosts_are_configured() {
        let registry = Registry::new(["api.example.com"]);
        assert_eq!(registry.scope_for(None), None);
    }

    #[test]
    fn ipv6_literal_keeps_brackets_and_drops_port() {
        assert_eq!(normalize_host("[::1]:8000"), "[::1]");
        let registry = Registry::new(["[::1]"]);
        assert_eq!(registry.scope_for(Some("[::1]:8000")), Some(Scope::Api));
    }

    #[test]
    fn configured_hosts_are_normalized_too() {
        let registry = Registry::new(["API.example.com:443", " "]);
        assert_eq!(registry.scope_for(Some("api.example.com")), Some(Scope::Api));
    }

    #[test]
    fn trailing_dot_in_host_is_ignored() {
        assert_eq!(normalize_host("API.example.com.:8000"), "api.example.com");
        let registry = Registry::new(["api.example.com"]);
        assert_eq!(registry.scope_for(Some("api.example.com.")), Some(Scope::Api));
    }

    #[test]
    fn from_config_accepts_hostnames_and_bracketed_ipv6() {
        let registry =
            Registry::from_config(["api.example.com", "API.example.com:443", "[::1]", "127.0.0.1"])
                .unwrap();
        assert_eq!(registry.scope_for(Some("[::1]:8000")), Some(Scope::Api));
        assert_eq!(registry.scope_for(Some("127.0.0.1:8000")), Some(Scope::Api));
    }

    #[test]
    fn from_config_rejects_unbracketed_ipv6() {
        let err = Registry::from_config(["::1"]).unwrap_err();
        assert!(err.contains("::1") && err.contains("bracket"), "{err}");
    }

    #[test]
    fn from_config_rejects_entries_that_are_not_hostnames() {
        for bad in ["http://api.example.com", "api example.com", "api/v1", "[::1"] {
            assert!(Registry::from_config([bad]).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn from_config_ignores_blank_entries() {
        let registry = Registry::from_config(["", "  "]).unwrap();
        assert_eq!(registry, Registry::default());
    }

    #[test]
    fn replace_takes_effect_for_the_next_load() {
        let shared = SharedRegistry::new(Registry::new(["a.example.com"]));
        assert_eq!(shared.load().scope_for(Some("b.example.com")), None);
        shared.replace(Registry::new(["b.example.com"]));
        assert_eq!(
            shared.load().scope_for(Some("b.example.com")),
            Some(Scope::Api)
        );
    }
}
