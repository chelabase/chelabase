//! Where each service lives (`host:port`, plain HTTP).

use crate::registry::Site;
use chelabase_routes::Service;
use std::collections::HashMap;

const DEFAULT_REST: &str = "127.0.0.1:8085";
const DEFAULT_REALTIME: &str = "127.0.0.1:8087";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstreams {
    orchestrator: String,
    auth: String,
    rest: String,
    realtime: String,
    /// The pass-through sites that have an upstream.
    sites: HashMap<Site, String>,
}

impl Upstreams {
    pub fn new(orchestrator_url: &str, auth_url: &str) -> Result<Self, String> {
        Ok(Self {
            orchestrator: authority_from_url(orchestrator_url)?,
            auth: authority_from_url(auth_url)?,
            rest: DEFAULT_REST.to_string(),
            realtime: DEFAULT_REALTIME.to_string(),
            sites: HashMap::new(),
        })
    }

    /// Where `rest` (the data API) lives.
    pub fn with_rest(mut self, rest_url: &str) -> Result<Self, String> {
        self.rest = authority_from_url(rest_url)?;
        Ok(self)
    }

    /// Where `realtime` (the websocket service) lives.
    pub fn with_realtime(mut self, realtime_url: &str) -> Result<Self, String> {
        self.realtime = authority_from_url(realtime_url)?;
        Ok(self)
    }

    /// Where `site` lives.
    pub fn with_site(mut self, site: Site, url: &str) -> Result<Self, String> {
        self.sites.insert(site, authority_from_url(url)?);
        Ok(self)
    }

    pub fn addr(&self, service: Service) -> &str {
        match service {
            Service::Orchestrator => &self.orchestrator,
            Service::Auth => &self.auth,
            Service::Rest => &self.rest,
            Service::Realtime => &self.realtime,
        }
    }

    /// `host:port` of `site`'s upstream; None when it has none.
    pub fn site_addr(&self, site: Site) -> Option<&str> {
        self.sites.get(&site).map(String::as_str)
    }
}

/// `http://host:port[/]` → `host:port`.
fn authority_from_url(url: &str) -> Result<String, String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("{url}: upstream URL must start with http://"))?;
    let authority = rest.trim_end_matches('/');
    if authority.is_empty() || authority.contains('/') {
        return Err(format!("{url}: upstream URL must be http://host:port with no path"));
    }
    let has_port = authority
        .rsplit_once(':')
        .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok());
    if !has_port {
        return Err(format!("{url}: upstream URL needs an explicit port"));
    }
    Ok(authority.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_http_url_becomes_host_port() {
        let upstreams = Upstreams::new("http://127.0.0.1:8081", "http://127.0.0.1:8084").unwrap();
        assert_eq!(upstreams.addr(Service::Orchestrator), "127.0.0.1:8081");
    }

    #[test]
    fn trailing_slash_is_accepted() {
        let upstreams = Upstreams::new("http://orchestrator:8081/", "http://127.0.0.1:8084").unwrap();
        assert_eq!(upstreams.addr(Service::Orchestrator), "orchestrator:8081");
    }

    #[test]
    fn https_is_rejected() {
        assert!(Upstreams::new("https://127.0.0.1:8081", "http://127.0.0.1:8084").is_err());
    }

    #[test]
    fn path_is_rejected() {
        assert!(Upstreams::new("http://127.0.0.1:8081/api", "http://127.0.0.1:8084").is_err());
    }

    #[test]
    fn missing_port_is_rejected() {
        assert!(Upstreams::new("http://orchestrator", "http://127.0.0.1:8084").is_err());
    }

    #[test]
    fn auth_has_its_own_upstream() {
        let upstreams = Upstreams::new("http://127.0.0.1:8081", "http://127.0.0.1:8084").unwrap();
        assert_eq!(upstreams.addr(Service::Auth), "127.0.0.1:8084");
    }

    #[test]
    fn sites_have_no_upstream_until_one_is_set() {
        let upstreams = Upstreams::new("http://127.0.0.1:8081", "http://127.0.0.1:8084").unwrap();
        assert_eq!(upstreams.site_addr(Site::Studio), None);
        let upstreams = upstreams.with_site(Site::Studio, "http://studio:3000/").unwrap();
        assert_eq!(upstreams.site_addr(Site::Studio), Some("studio:3000"));
        assert_eq!(upstreams.site_addr(Site::Docs), None);
    }

    #[test]
    fn a_site_upstream_is_checked_like_the_others() {
        let upstreams = Upstreams::new("http://127.0.0.1:8081", "http://127.0.0.1:8084").unwrap();
        for bad in ["https://studio:3000", "http://studio", "http://studio:3000/app"] {
            assert!(upstreams.clone().with_site(Site::Studio, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn realtime_has_its_own_upstream_checked_like_the_others() {
        let upstreams = Upstreams::new("http://127.0.0.1:8081", "http://127.0.0.1:8084").unwrap();
        assert_eq!(upstreams.addr(Service::Realtime), "127.0.0.1:8087");
        let set = upstreams.clone().with_realtime("http://realtime:9000/").unwrap();
        assert_eq!(set.addr(Service::Realtime), "realtime:9000");
        assert_eq!(set.addr(Service::Rest), "127.0.0.1:8085");
        for bad in ["https://realtime:9000", "http://realtime", "http://realtime:9000/x"] {
            assert!(upstreams.clone().with_realtime(bad).is_err(), "{bad}");
        }
    }
}
