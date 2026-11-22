//! The service's settings, from the environment.

use std::net::SocketAddr;

use anyhow::Context;
use chelabase_auth::member_token::MemberTokenVerifier;
use chelabase_types::entry::Entry;

use crate::app::Settings;
use crate::limits::Limits;

pub struct Config {
    /// `CHELABASE_MCP_LISTEN`: comma-separated `host:port` entries for the public listener
    /// (default `127.0.0.1:8095`). Compose names the service's alias on each network it serves.
    pub listen: Vec<String>,
    /// `CHELABASE_MCP_INTERNAL_LISTEN`: the same, for the listener only `mcp-runtime` reaches
    /// (default `127.0.0.1:8096`).
    pub internal_listen: Vec<String>,
    /// `CHELABASE_MCP_RUNTIME_URL` (`http(s)://host[:port]`). Unset: every tool call answers
    /// `runtime_unavailable`.
    pub runtime_url: Option<String>,
    pub settings: Settings,
    /// `CHELABASE_MEMBER_JWT_PUBLIC_KEY` with the issuer from `CHELABASE_AUTH_HOST`. Required:
    /// without it no request could be authenticated.
    pub verifier: MemberTokenVerifier,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Blank values count as unset.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let non_blank = |name: &str| {
            get(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let listen = listen_list(
            "CHELABASE_MCP_LISTEN",
            &non_blank("CHELABASE_MCP_LISTEN").unwrap_or_else(|| "127.0.0.1:8095".into()),
        )?;
        let internal_listen = listen_list(
            "CHELABASE_MCP_INTERNAL_LISTEN",
            &non_blank("CHELABASE_MCP_INTERNAL_LISTEN").unwrap_or_else(|| "127.0.0.1:8096".into()),
        )?;
        let runtime_url = non_blank("CHELABASE_MCP_RUNTIME_URL");
        if let Some(url) = &runtime_url {
            let parsed = reqwest::Url::parse(url)
                .ok()
                .filter(|u| matches!(u.scheme(), "http" | "https") && u.host().is_some());
            anyhow::ensure!(
                parsed.is_some(),
                "CHELABASE_MCP_RUNTIME_URL must be http(s)://host[:port], not {url:?}"
            );
        }
        let limits = Limits::from_lookup(&get)?;
        // The same entry the gateway reads, so the challenge it sends and this one agree.
        let entry = Entry::from_lookup(&get).map_err(|e| anyhow::anyhow!(e.0))?;
        let verifier = MemberTokenVerifier::from_lookup(&get)
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .context("CHELABASE_MEMBER_JWT_PUBLIC_KEY is required: the MCP endpoint takes member tokens only")?;
        let allowed_origins = non_blank("CHELABASE_MCP_ALLOWED_ORIGINS")
            .map(|v| {
                v.split(',')
                    .map(|o| o.trim().to_owned())
                    .filter(|o| !o.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let log_code = match non_blank("CHELABASE_MCP_LOG_CODE").as_deref() {
            None | Some("0") => false,
            Some("1") => true,
            Some(other) => anyhow::bail!("CHELABASE_MCP_LOG_CODE must be 0 or 1, not {other:?}"),
        };
        Ok(Self {
            listen,
            internal_listen,
            runtime_url,
            settings: Settings {
                api_base: entry.api_base(),
                allowed_origins,
                log_code,
                limits,
            },
            verifier,
        })
    }
}

/// `value`'s comma-separated `host:port` entries, each checked for shape (resolved at startup by
/// [`resolve_all`]).
fn listen_list(var: &str, value: &str) -> anyhow::Result<Vec<String>> {
    value
        .split(',')
        .map(str::trim)
        .map(|entry| {
            let ok = entry
                .rsplit_once(':')
                .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok());
            anyhow::ensure!(ok, "{var} entries must be host:port, not {entry:?}");
            Ok(entry.to_owned())
        })
        .collect()
}

/// Every address `entries` (from `var`) name, each resolved now, without duplicates. An entry
/// that resolves to nothing fails: a listener never falls back to the any-address.
pub async fn resolve_all(var: &str, entries: &[String]) -> anyhow::Result<Vec<SocketAddr>> {
    let mut addrs = Vec::new();
    for entry in entries {
        let found: Vec<_> = tokio::net::lookup_host(entry.as_str())
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
    use std::collections::HashMap;

    /// A lookup with a member key and an entry, plus `extra`.
    fn env(extra: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let (_, public) = chelabase_auth::internal::generate_keypair_pem();
        let path = std::env::temp_dir().join(format!("mcp-config-{}.pub", uuid::Uuid::new_v4()));
        std::fs::write(&path, public).unwrap();
        let mut vars: HashMap<String, String> = [
            ("CHELABASE_MEMBER_JWT_PUBLIC_KEY", path.to_str().unwrap()),
            ("CHELABASE_STUDIO_HOST", "studio.example.test"),
            ("CHELABASE_API_HOST", "api.example.test"),
            ("CHELABASE_AUTH_HOST", "auth.example.test"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        for (k, v) in extra {
            vars.insert((*k).to_owned(), (*v).to_owned());
        }
        move |name| vars.get(name).cloned()
    }

    #[test]
    fn listeners_take_comma_separated_host_port_lists() {
        let config = Config::from_lookup(env(&[
            ("CHELABASE_MCP_LISTEN", "mcp-public:8095, 127.0.0.1:8095"),
            ("CHELABASE_MCP_INTERNAL_LISTEN", "mcp-internal:8096"),
            ("CHELABASE_MCP_RUNTIME_URL", "http://mcp-runtime:8080"),
        ]))
        .unwrap();
        assert_eq!(config.listen, ["mcp-public:8095", "127.0.0.1:8095"]);
        assert_eq!(config.internal_listen, ["mcp-internal:8096"]);
        assert_eq!(
            config.runtime_url.as_deref(),
            Some("http://mcp-runtime:8080")
        );

        // Blank means the defaults: loopback only, no runtime.
        let config = Config::from_lookup(env(&[
            ("CHELABASE_MCP_LISTEN", " "),
            ("CHELABASE_MCP_INTERNAL_LISTEN", ""),
            ("CHELABASE_MCP_RUNTIME_URL", ""),
            ("CHELABASE_MCP_TIMEOUT_MS", ""),
            ("CHELABASE_MCP_RESULT_MAX_BYTES", ""),
            ("CHELABASE_MCP_CALLS_PER_MINUTE", ""),
            ("CHELABASE_MCP_MEMBER_CONCURRENCY", ""),
            ("CHELABASE_MCP_CONCURRENCY", ""),
        ]))
        .unwrap();
        assert_eq!(config.listen, ["127.0.0.1:8095"]);
        assert_eq!(config.internal_listen, ["127.0.0.1:8096"]);
        assert_eq!(config.runtime_url, None);
        assert_eq!(config.settings.limits, crate::limits::Limits::default());

        for bad in [
            "8095",
            "mcp-public",
            "mcp-public:",
            ":8095",
            "host:99999",
            "a:1,,b",
        ] {
            assert!(
                Config::from_lookup(env(&[("CHELABASE_MCP_LISTEN", bad)])).is_err(),
                "{bad}"
            );
        }
        assert!(
            Config::from_lookup(env(&[("CHELABASE_MCP_RUNTIME_URL", "mcp-runtime:8080")])).is_err()
        );
        assert!(Config::from_lookup(env(&[("CHELABASE_MCP_TIMEOUT_MS", "120000")])).is_err());
    }

    #[tokio::test]
    async fn every_entry_resolves_and_nothing_falls_back_to_any_address() {
        let entries = vec!["127.0.0.1:8095".to_owned(), "localhost:8095".to_owned()];
        let addrs = resolve_all("CHELABASE_MCP_LISTEN", &entries).await.unwrap();
        assert!(
            addrs.contains(&"127.0.0.1:8095".parse().unwrap()),
            "{addrs:?}"
        );
        assert!(addrs.iter().all(|a| a.ip().is_loopback()), "{addrs:?}");
        let mut sorted = addrs.clone();
        sorted.dedup();
        assert_eq!(sorted, addrs, "duplicates are bound once");

        // A name that does not resolve fails startup; it never becomes 0.0.0.0.
        let unknown = vec!["no-such-host.invalid:8095".to_owned()];
        assert!(resolve_all("CHELABASE_MCP_LISTEN", &unknown).await.is_err());
        assert!(resolve_all("CHELABASE_MCP_LISTEN", &[]).await.is_err());
    }
}
