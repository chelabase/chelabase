//! Host name → target resolution: the API, or one of the pass-through sites.
//!
//! Held behind an atomic swap so a later metadata-backed source (hosted mode,
//! per-project hostnames) can replace it without touching the request path.

use arc_swap::ArcSwap;
use chelabase_types::entry::{host_port, normalize_host, validate_host, Entry};
use std::collections::HashMap;
use std::sync::Arc;

/// A host the gateway passes through to its own upstream, with no routes, CORS or auth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Site {
    Studio,
    Docs,
    Landing,
    Mail,
}

impl Site {
    pub fn name(self) -> &'static str {
        match self {
            Site::Studio => "studio",
            Site::Docs => "docs",
            Site::Landing => "landing",
            Site::Mail => "mail",
        }
    }
}

/// What a request's `Host` selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The route table, CORS, data keys and user tokens.
    Api,
    Site(Site),
}

/// P1: a configured host with a port matches only that port; one without a port matches any
/// port. An exact `host:port` match wins over a name-only one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Registry {
    /// `name:port` (normalized name) → target.
    exact: HashMap<String, Target>,
    /// Normalized name → target, for hosts configured without a port.
    any_port: HashMap<String, Target>,
}

impl Registry {
    pub fn new<I, S>(api_hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        api_hosts
            .into_iter()
            .fold(Self::default(), |r, host| r.with(host.as_ref(), Target::Api))
    }

    pub fn with_site(self, host: &str, site: Site) -> Self {
        self.with(host, Target::Site(site))
    }

    /// Every routed host of the entry: the API, Studio, and docs, landing and Mailpit when set.
    /// The auth host only names the member tokens' issuer: nothing is served there.
    pub fn from_entry(entry: &Entry) -> Self {
        let optional = [
            (entry.docs_host(), Site::Docs),
            (entry.landing_host(), Site::Landing),
            (entry.mail_host(), Site::Mail),
        ];
        optional.into_iter().fold(
            Self::new([entry.api_host()])
                .with_site(entry.studio_host(), Site::Studio),
            |r, (host, site)| match host {
                Some(host) => r.with_site(host, site),
                None => r,
            },
        )
    }

    fn with(mut self, host: &str, target: Target) -> Self {
        let name = normalize_host(host);
        if name.is_empty() {
            return self;
        }
        match host_port(host) {
            Some(port) => self.exact.insert(format!("{name}:{port}"), target),
            None => self.any_port.insert(name, target),
        };
        self
    }

    /// P1. None for no `Host`, a malformed one, or no match.
    pub fn target_for(&self, host: Option<&str>) -> Option<Target> {
        let host = host?.trim();
        validate_host(host).ok()?;
        let name = normalize_host(host);
        let exact = host_port(host).and_then(|port| self.exact.get(&format!("{name}:{port}")));
        exact.or_else(|| self.any_port.get(&name)).copied()
    }
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
    use chelabase_types::entry::Entry;

    #[test]
    fn an_empty_registry_matches_no_host() {
        let r = Registry::new(Vec::<String>::new());
        assert_eq!(r.target_for(Some("anything:8000")), None);
        assert_eq!(r.target_for(None), None);
    }

    #[test]
    fn a_host_without_a_port_matches_any_port_and_case() {
        let r = Registry::new(["api.example.com"]);
        assert_eq!(r.target_for(Some("API.Example.com:8000")), Some(Target::Api));
        assert_eq!(r.target_for(Some("api.example.com.")), Some(Target::Api));
        assert_eq!(r.target_for(Some("evil.example.com")), None);
        assert_eq!(r.target_for(None), None);
    }

    #[test]
    fn a_host_with_a_port_matches_only_that_port_and_wins() {
        let r = Registry::new(["127.0.0.1"]).with_site("127.0.0.1:3999", Site::Studio);
        assert_eq!(r.target_for(Some("127.0.0.1:3999")), Some(Target::Site(Site::Studio)));
        assert_eq!(r.target_for(Some("127.0.0.1:8000")), Some(Target::Api));
        let r = Registry::new(["api.chelabase.localhost:8000"]);
        assert_eq!(r.target_for(Some("api.chelabase.localhost:9000")), None);
        assert_eq!(r.target_for(Some("api.chelabase.localhost")), None);
    }

    #[test]
    fn ipv6_literals_keep_their_brackets() {
        let r = Registry::new(["[::1]"]);
        assert_eq!(r.target_for(Some("[::1]:8000")), Some(Target::Api));
    }

    #[test]
    fn from_entry_maps_every_configured_host() {
        let vars = [
            ("CHELABASE_PUBLIC_SCHEME", "http"),
            ("CHELABASE_STUDIO_HOST", "studio.chelabase.localhost:8000"),
            ("CHELABASE_API_HOST", "api.chelabase.localhost:8000"),
            ("CHELABASE_AUTH_HOST", "auth.chelabase.localhost:8000"),
            ("CHELABASE_DOCS_HOST", "docs.chelabase.localhost:8000"),
            ("CHELABASE_MAIL_HOST", "mail.chelabase.localhost:8000"),
        ];
        let entry = Entry::from_lookup(|k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())).unwrap();
        let r = Registry::from_entry(&entry);
        for (host, target) in [
            ("studio.chelabase.localhost:8000", Some(Target::Site(Site::Studio))),
            ("api.chelabase.localhost:8000", Some(Target::Api)),
            // #126: the auth host is routed nowhere (no fall-through to another upstream).
            ("auth.chelabase.localhost:8000", None),
            ("docs.chelabase.localhost:8000", Some(Target::Site(Site::Docs))),
            ("mail.chelabase.localhost:8000", Some(Target::Site(Site::Mail))),
            ("landing.chelabase.localhost:8000", None),
        ] {
            assert_eq!(r.target_for(Some(host)), target, "{host}");
        }
    }

    #[test]
    fn replace_takes_effect_for_the_next_load() {
        let shared = SharedRegistry::new(Registry::new(["a.example.com"]));
        assert_eq!(shared.load().target_for(Some("b.example.com")), None);
        shared.replace(Registry::new(["b.example.com"]));
        assert_eq!(shared.load().target_for(Some("b.example.com")), Some(Target::Api));
    }
}
