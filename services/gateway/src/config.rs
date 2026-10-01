//! Gateway settings from the environment.

use crate::upstreams::Upstreams;
use std::time::Duration;

#[derive(Clone)]
pub struct GatewayConfig {
    pub listen: String,
    pub upstreams: Upstreams,
    pub platform_hosts: Vec<String>,
    pub studio_origins: Vec<String>,
    /// Orchestrator's internal listener (`CHELADB_INTERNAL_URL`), for key lookups.
    pub internal_url: String,
    /// Bearer for that listener (`CHELADB_DATA_KEYS_TOKEN`).
    pub data_keys_token: String,
    /// Upstream read timeout (`CHELADB_GATEWAY_UPSTREAM_TIMEOUT_SECS`, default 60).
    pub upstream_timeout: Duration,
}

/// Everything but the token.
impl std::fmt::Debug for GatewayConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayConfig")
            .field("listen", &self.listen)
            .field("upstreams", &self.upstreams)
            .field("platform_hosts", &self.platform_hosts)
            .field("studio_origins", &self.studio_origins)
            .field("internal_url", &self.internal_url)
            .field("data_keys_token", &"<redacted>")
            .field("upstream_timeout", &self.upstream_timeout)
            .finish()
    }
}

impl GatewayConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        Ok(Self {
            listen: env_or("CHELADB_GATEWAY_ADDR", "127.0.0.1:8000"),
            upstreams: Upstreams::new(
                &env_or("CHELADB_ORCHESTRATOR_URL", "http://127.0.0.1:8081"),
                &env_or("CHELADB_AUTH_URL", "http://127.0.0.1:8084"),
            )
            .and_then(|u| u.with_rest(&env_or("CHELADB_REST_URL", "http://127.0.0.1:8085")))
            .map_err(anyhow::Error::msg)?,
            platform_hosts: split_csv(&env_or("CHELADB_PLATFORM_HOSTS", "")),
            studio_origins: split_csv(&env_or("CHELADB_STUDIO_ORIGINS", "http://localhost:3000")),
            internal_url: env_or("CHELADB_INTERNAL_URL", "http://127.0.0.1:8083"),
            data_keys_token: env_or("CHELADB_DATA_KEYS_TOKEN", "cheladb-dev-data-keys"),
            upstream_timeout: parse_timeout_secs(&env_or(
                "CHELADB_GATEWAY_UPSTREAM_TIMEOUT_SECS",
                "60",
            ))
            .map_err(anyhow::Error::msg)?,
        })
    }
}

/// `"a, b,,c "` → `["a", "b", "c"]`.
pub fn split_csv(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// A positive whole number of seconds.
pub fn parse_timeout_secs(raw: &str) -> Result<Duration, String> {
    match raw.trim().parse::<u64>() {
        Ok(secs) if secs > 0 => Ok(Duration::from_secs(secs)),
        _ => Err(format!("{raw:?} is not a positive number of seconds")),
    }
}

fn env_or(var: &str, default: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| default.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_the_data_keys_token() {
        let config = GatewayConfig {
            listen: "127.0.0.1:0".into(),
            upstreams: Upstreams::new("http://127.0.0.1:1", "http://127.0.0.1:2").unwrap(),
            platform_hosts: vec![],
            studio_origins: vec![],
            internal_url: "http://127.0.0.1:3".into(),
            data_keys_token: "super-secret-token".into(),
            upstream_timeout: std::time::Duration::from_secs(1),
        };
        let shown = format!("{config:?}");
        assert!(!shown.contains("super-secret-token"), "{shown}");
        assert!(shown.contains("data_keys_token"), "{shown}");
    }

    #[test]
    fn timeout_secs_parses_positive_integers() {
        assert_eq!(parse_timeout_secs("7"), Ok(std::time::Duration::from_secs(7)));
    }

    #[test]
    fn timeout_secs_rejects_zero_and_garbage() {
        assert!(parse_timeout_secs("0").is_err());
        assert!(parse_timeout_secs("soon").is_err());
    }

    #[test]
    fn split_csv_trims_and_drops_empty_entries() {
        assert_eq!(split_csv("a, b,,c "), vec!["a", "b", "c"]);
        assert!(split_csv("").is_empty());
    }
}
