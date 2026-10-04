//! Where each service lives (`host:port`, plain HTTP).

use chelabase_routes::Service;

const DEFAULT_REST: &str = "127.0.0.1:8085";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstreams {
    orchestrator: String,
    auth: String,
    rest: String,
}

impl Upstreams {
    pub fn new(orchestrator_url: &str, auth_url: &str) -> Result<Self, String> {
        Ok(Self {
            orchestrator: authority_from_url(orchestrator_url)?,
            auth: authority_from_url(auth_url)?,
            rest: DEFAULT_REST.to_string(),
        })
    }

    /// Where `rest` (the data API) lives.
    pub fn with_rest(mut self, rest_url: &str) -> Result<Self, String> {
        self.rest = authority_from_url(rest_url)?;
        Ok(self)
    }

    pub fn addr(&self, service: Service) -> &str {
        match service {
            Service::Orchestrator => &self.orchestrator,
            Service::Auth => &self.auth,
            Service::Rest => &self.rest,
        }
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
}
