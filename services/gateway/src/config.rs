//! Gateway settings from the environment.

use crate::client_addr::TrustedProxies;
use crate::registry::Site;
use crate::upstreams::Upstreams;
use chelabase_types::entry::{
    Entry, DEFAULT_DOCS_UPSTREAM, DEFAULT_LANDING_UPSTREAM, DEFAULT_MAIL_UPSTREAM, DEFAULT_STUDIO_UPSTREAM,
    DEFAULT_ZITADEL_UPSTREAM,
};
use chelabase_types::mode::Mode;
use std::net::SocketAddr;
use std::time::Duration;

/// Settings that no longer exist, with their replacement. `Entry` refuses its own.
const REMOVED: &[(&str, &str)] = &[("CHELABASE_GATEWAY_ADDR", "CHELABASE_GATEWAY_LISTEN")];

#[derive(Clone)]
pub struct GatewayConfig {
    /// Listening addresses (`CHELABASE_GATEWAY_LISTEN`, comma-separated, default `127.0.0.1:8000`).
    pub listen: Vec<String>,
    /// The public hosts and the URLs derived from them.
    pub entry: Entry,
    pub upstreams: Upstreams,
    /// Orchestrator's internal listener (`CHELABASE_INTERNAL_URL`), for key lookups.
    pub internal_url: String,
    /// Bearer for that listener (`CHELABASE_DATA_KEYS_TOKEN`).
    pub data_keys_token: String,
    /// App-user token verification key, a PEM file (`CHELABASE_APP_JWT_PUBLIC_KEY`); when unset,
    /// every `Chelabase-User` header is refused.
    pub app_jwt_public_key: Option<String>,
    /// Liveness-only bearer for `GET /internal/app/sessions/{id}`
    /// (`CHELABASE_APP_SESSIONS_INTERNAL_TOKEN`).
    pub app_sessions_token: String,
    /// Upstream read timeout (`CHELABASE_GATEWAY_UPSTREAM_TIMEOUT_SECS`, default 60).
    pub upstream_timeout: Duration,
    /// Reverse proxies whose forwarded client address is believed
    /// (`CHELABASE_GATEWAY_TRUSTED_PROXIES`, comma-separated CIDRs or addresses, default none).
    pub trusted_proxies: TrustedProxies,
}

/// Everything but the token.
impl std::fmt::Debug for GatewayConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayConfig")
            .field("listen", &self.listen)
            .field("entry", &self.entry)
            .field("upstreams", &self.upstreams)
            .field("internal_url", &self.internal_url)
            .field("data_keys_token", &"<redacted>")
            .field("app_jwt_public_key", &self.app_jwt_public_key)
            .field("app_sessions_token", &"<redacted>")
            .field("upstream_timeout", &self.upstream_timeout)
            .field("trusted_proxies", &self.trusted_proxies)
            .finish()
    }
}

impl GatewayConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// The settings from `get` (a variable name → its value). Refuses removed settings (naming
    /// the replacement), an invalid entry (see [`Entry::from_lookup`]), a listen address that
    /// isn't `ip:port`, and an invalid upstream URL.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let or = |name: &str, default: &str| get(name).unwrap_or_else(|| default.to_string());
        for (old, new) in REMOVED {
            if get(old).is_some_and(|v| !v.trim().is_empty()) {
                anyhow::bail!("{old} is no longer supported; use {new}");
            }
        }
        let entry = Entry::from_lookup(&get)?;
        let mode = Mode::parse(get(Mode::ENV).as_deref()).map_err(anyhow::Error::msg)?;
        entry.check_mode(mode)?;
        let listen = parse_listen(&or("CHELABASE_GATEWAY_LISTEN", ""))?;

        // Docs, landing and Mailpit have an upstream only when their host is set.
        let sites = [
            (Site::Studio, "CHELABASE_STUDIO_UPSTREAM", DEFAULT_STUDIO_UPSTREAM, true),
            (Site::Auth, "CHELABASE_ZITADEL_UPSTREAM", DEFAULT_ZITADEL_UPSTREAM, true),
            (Site::Docs, "CHELABASE_DOCS_UPSTREAM", DEFAULT_DOCS_UPSTREAM, entry.docs_host().is_some()),
            (
                Site::Landing,
                "CHELABASE_LANDING_UPSTREAM",
                DEFAULT_LANDING_UPSTREAM,
                entry.landing_host().is_some(),
            ),
            (Site::Mail, "CHELABASE_MAIL_UPSTREAM", DEFAULT_MAIL_UPSTREAM, entry.mail_host().is_some()),
        ];
        let mut upstreams = Upstreams::new(
            &or("CHELABASE_ORCHESTRATOR_URL", "http://127.0.0.1:8081"),
            &or("CHELABASE_AUTH_URL", "http://127.0.0.1:8084"),
        )
        .and_then(|u| u.with_rest(&or("CHELABASE_REST_URL", "http://127.0.0.1:8085")))
        .and_then(|u| u.with_realtime(&or("CHELABASE_REALTIME_URL", "http://127.0.0.1:8087")))
        .and_then(|u| u.with_storage(&or("CHELABASE_STORAGE_URL", "http://127.0.0.1:8089")))
        .map_err(anyhow::Error::msg)?;
        for (site, var, default, enabled) in sites {
            if enabled {
                upstreams = upstreams
                    .with_site(site, &or(var, default))
                    .map_err(|e| anyhow::anyhow!("{var}: {e}"))?;
            }
        }

        // A bearer is `<NAME>` or the file `<NAME>_FILE` names (the stack mounts generated
        // secrets, S27); unset, the development default, which only the workbench may use
        // (`CHELABASE_DEV_DEFAULTS=1`, self-hosted mode).
        let secret = |name: &str| -> anyhow::Result<String> {
            Ok(chelabase_types::secret_env::lookup_secret(&get, name)
                .map_err(anyhow::Error::msg)?
                .unwrap_or_else(|| chelabase_types::dev_defaults::dev_bearer(name).to_string()))
        };
        let data_keys_token = secret("CHELABASE_DATA_KEYS_TOKEN")?;
        let app_sessions_token = secret("CHELABASE_APP_SESSIONS_INTERNAL_TOKEN")?;
        chelabase_types::dev_defaults::DevDefaults::from_lookup(&get)
            .map_err(anyhow::Error::msg)?
            .check_bearers(
                &chelabase_types::mode_policy::ModePolicy::new(mode),
                &[
                    ("CHELABASE_DATA_KEYS_TOKEN", data_keys_token.as_str()),
                    ("CHELABASE_APP_SESSIONS_INTERNAL_TOKEN", app_sessions_token.as_str()),
                ],
            )
            .map_err(anyhow::Error::msg)?;
        Ok(Self {
            listen,
            entry,
            upstreams,
            internal_url: or("CHELABASE_INTERNAL_URL", "http://127.0.0.1:8083"),
            data_keys_token,
            app_jwt_public_key: get("CHELABASE_APP_JWT_PUBLIC_KEY").filter(|v| !v.trim().is_empty()),
            app_sessions_token,
            upstream_timeout: parse_timeout_secs(&or("CHELABASE_GATEWAY_UPSTREAM_TIMEOUT_SECS", "60"))
                .map_err(anyhow::Error::msg)?,
            trusted_proxies: parse_trusted_proxies(&or(TRUSTED_PROXIES_ENV, ""))?,
        })
    }
}

/// `CHELABASE_GATEWAY_LISTEN`: comma-separated `ip:port` addresses (`[::1]:8000` for IPv6);
/// blank means `127.0.0.1:8000`.
fn parse_listen(raw: &str) -> anyhow::Result<Vec<String>> {
    let listen = split_csv(raw);
    if listen.is_empty() {
        return Ok(vec!["127.0.0.1:8000".to_string()]);
    }
    let mut seen: Vec<SocketAddr> = Vec::with_capacity(listen.len());
    for addr in &listen {
        let Ok(parsed) = addr.parse::<SocketAddr>() else {
            anyhow::bail!(
                "CHELABASE_GATEWAY_LISTEN: {addr:?} is not an ip:port address (bracket IPv6, e.g. [::1]:8000)"
            );
        };
        if seen.contains(&parsed) {
            anyhow::bail!("CHELABASE_GATEWAY_LISTEN: {parsed} is listed more than once");
        }
        seen.push(parsed);
    }
    Ok(listen)
}

/// The variable naming the reverse proxies the gateway trusts.
pub const TRUSTED_PROXIES_ENV: &str = "CHELABASE_GATEWAY_TRUSTED_PROXIES";

/// `CHELABASE_GATEWAY_TRUSTED_PROXIES`: comma-separated CIDRs (`10.0.0.0/8`, `fd00::/8`) or
/// single addresses; blank trusts no proxy. Anything else refuses startup, naming the variable.
fn parse_trusted_proxies(raw: &str) -> anyhow::Result<TrustedProxies> {
    TrustedProxies::parse(raw).map_err(|e| anyhow::anyhow!("{TRUSTED_PROXIES_ENV}: {e}"))
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

#[cfg(test)]
mod listen_tests {
    use super::*;

    #[test]
    fn a_listen_address_must_be_ip_and_port() {
        assert_eq!(parse_listen(" , ").unwrap(), vec!["127.0.0.1:8000"]);
        for bad in ["localhost:8000", "::1:8000", "127.0.0.1", "[::1]"] {
            let msg = parse_listen(bad).unwrap_err().to_string();
            assert!(msg.contains("CHELABASE_GATEWAY_LISTEN"), "{bad}: {msg}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Site;

    // The workbench's switch: these tests are about other settings (the dev-bearer rule has
    // its own test, which turns it off).
    const ENTRY: &[(&str, &str)] = &[
        ("CHELABASE_DEV_DEFAULTS", "1"),
        ("CHELABASE_PUBLIC_SCHEME", "http"),
        ("CHELABASE_STUDIO_HOST", "studio.chelabase.localhost:8000"),
        ("CHELABASE_API_HOST", "api.chelabase.localhost:8000"),
        ("CHELABASE_AUTH_HOST", "auth.chelabase.localhost:8000"),
    ];

    fn with(extra: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        let all: Vec<(&str, &str)> = ENTRY.iter().chain(extra.iter()).copied().collect();
        move |k: &str| all.iter().rev().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    // Selfhost-install m5 fix round, S1: the gateway's two internal bearers (data-key lookups,
    // session liveness) never default to their public development values outside the
    // workbench. Unset, set to a default, `=0` and platform refuse at startup, naming the
    // variable and never the token; `=1` (self-hosted) keeps today's behaviour.
    #[test]
    fn dev_bearer_refused_without_dev_defaults() {
        const OWN: [(&str, &str); 2] = [
            ("CHELABASE_DATA_KEYS_TOKEN", "generated-data-keys-0123"),
            ("CHELABASE_APP_SESSIONS_INTERNAL_TOKEN", "generated-app-sessions-0123"),
        ];
        let own: Vec<_> = OWN.iter().copied().chain([("CHELABASE_DEV_DEFAULTS", "")]).collect();
        let c = GatewayConfig::from_lookup(with(&own)).unwrap();
        assert_eq!(c.data_keys_token, "generated-data-keys-0123");
        let pf: Vec<_> = own.iter().copied().chain([("CHELABASE_MODE", "platform"), ("CHELABASE_PUBLIC_SCHEME", "https")]).collect();
        assert!(GatewayConfig::from_lookup(with(&pf)).is_ok());
        for (var, _) in OWN {
            // The test entry turns the switch on; blank is unset.
            let other: Vec<(&'static str, &'static str)> = OWN
                .iter()
                .copied()
                .filter(|(v, _)| *v != var)
                .chain([("CHELABASE_DEV_DEFAULTS", "")])
                .collect();
            let dev = chelabase_types::dev_defaults::dev_bearer(var);
            let cases: [(&str, Vec<(&'static str, &'static str)>); 4] = [
                ("unset", other.clone()),
                ("the default", other.iter().copied().chain([(var, dev)]).collect()),
                ("=0", other.iter().copied().chain([("CHELABASE_DEV_DEFAULTS", "0")]).collect()),
                (
                    "platform",
                    other
                        .iter()
                        .copied()
                        .chain([("CHELABASE_DEV_DEFAULTS", "1"), ("CHELABASE_MODE", "platform"), ("CHELABASE_PUBLIC_SCHEME", "https")])
                        .collect(),
                ),
            ];
            for (how, pairs) in cases {
                let err = match GatewayConfig::from_lookup(with(&pairs)) {
                    Err(e) => format!("{e:#}"),
                    Ok(_) => panic!("{var} {how}: the gateway started on a development bearer"),
                };
                assert!(err.contains(var), "{var} {how}: {err}");
                assert!(!err.contains("chelabase-dev-"), "{var} {how}: {err}");
            }
            let dev_on: Vec<_> = other.iter().copied().chain([("CHELABASE_DEV_DEFAULTS", "1")]).collect();
            let c = GatewayConfig::from_lookup(with(&dev_on)).unwrap();
            assert_eq!(if var == OWN[0].0 { &c.data_keys_token } else { &c.app_sessions_token }, dev, "{var}");
        }
    }

    #[test]
    fn debug_hides_the_tokens() {
        let config = GatewayConfig::from_lookup(with(&[
            ("CHELABASE_DATA_KEYS_TOKEN", "super-secret-token"),
            ("CHELABASE_APP_SESSIONS_INTERNAL_TOKEN", "sessions-secret"),
        ]))
        .unwrap();
        let shown = format!("{config:?}");
        assert!(!shown.contains("super-secret-token"), "{shown}");
        assert!(!shown.contains("sessions-secret"), "{shown}");
        assert!(shown.contains("data_keys_token"), "{shown}");
    }

    #[test]
    fn listeners_default_and_take_a_list() {
        assert_eq!(GatewayConfig::from_lookup(with(&[])).unwrap().listen, vec!["127.0.0.1:8000"]);
        let c = GatewayConfig::from_lookup(with(&[("CHELABASE_GATEWAY_LISTEN", "127.0.0.1:8000, [::1]:8000")])).unwrap();
        assert_eq!(c.listen, vec!["127.0.0.1:8000", "[::1]:8000"]);
    }

    #[test]
    fn removed_variables_are_refused_with_their_replacement() {
        for (old, new) in [
            ("CHELABASE_GATEWAY_ADDR", "CHELABASE_GATEWAY_LISTEN"),
            ("CHELABASE_API_HOSTS", "CHELABASE_API_HOST"),
            ("CHELABASE_STUDIO_ORIGINS", "CHELABASE_STUDIO_HOST"),
        ] {
            let msg = GatewayConfig::from_lookup(with(&[(old, "x")])).unwrap_err().to_string();
            assert!(msg.contains(new), "{old}: {msg}");
        }
    }

    #[test]
    fn site_upstreams_default_to_loopback_and_exist_only_with_their_host() {
        let c = GatewayConfig::from_lookup(with(&[])).unwrap();
        assert_eq!(c.upstreams.site_addr(Site::Studio), DEFAULT_STUDIO_UPSTREAM.strip_prefix("http://"));
        assert_eq!(c.upstreams.site_addr(Site::Auth), DEFAULT_ZITADEL_UPSTREAM.strip_prefix("http://"));
        assert_eq!(c.upstreams.site_addr(Site::Docs), None);
        assert_eq!(c.upstreams.site_addr(Site::Mail), None);
        let c = GatewayConfig::from_lookup(with(&[
            ("CHELABASE_DOCS_HOST", "docs.chelabase.localhost:8000"),
            ("CHELABASE_MAIL_HOST", "mail.chelabase.localhost:8000"),
            ("CHELABASE_MAIL_UPSTREAM", "http://mailpit:8025"),
        ]))
        .unwrap();
        assert_eq!(c.upstreams.site_addr(Site::Docs), DEFAULT_DOCS_UPSTREAM.strip_prefix("http://"));
        assert_eq!(c.upstreams.site_addr(Site::Mail), Some("mailpit:8025"));
        assert_eq!(c.upstreams.site_addr(Site::Landing), None);
        assert_eq!(c.entry.studio_origin(), "http://studio.chelabase.localhost:8000");
    }

    #[test]
    fn platform_mode_refuses_an_http_entry() {
        let msg = GatewayConfig::from_lookup(with(&[("CHELABASE_MODE", "platform")]))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("CHELABASE_PUBLIC_SCHEME"), "{msg}");
        // Platform mode needs its own bearers (m5 fix round).
        assert!(GatewayConfig::from_lookup(with(&[
            ("CHELABASE_MODE", "platform"),
            ("CHELABASE_PUBLIC_SCHEME", "https"),
            ("CHELABASE_DATA_KEYS_TOKEN", "generated-data-keys-0123"),
            ("CHELABASE_APP_SESSIONS_INTERNAL_TOKEN", "generated-app-sessions-0123"),
        ]))
        .is_ok());
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

    #[test]
    fn a_repeated_listen_address_is_refused() {
        let msg = GatewayConfig::from_lookup(with(&[("CHELABASE_GATEWAY_LISTEN", "127.0.0.1:8000, 127.0.0.1:8000")]))
            .unwrap_err()
            .to_string();
        assert!(msg.contains("CHELABASE_GATEWAY_LISTEN"), "{msg}");
    }

    // ---- m14 (#25): trusted reverse proxies ----

    #[test]
    fn no_trusted_proxies_by_default() {
        assert!(GatewayConfig::from_lookup(with(&[])).unwrap().trusted_proxies.is_empty());
        let blank = GatewayConfig::from_lookup(with(&[(TRUSTED_PROXIES_ENV, " , ")])).unwrap();
        assert!(blank.trusted_proxies.is_empty());
    }

    #[test]
    fn trusted_proxies_take_cidrs_and_addresses() {
        let c = GatewayConfig::from_lookup(with(&[(TRUSTED_PROXIES_ENV, "10.0.0.0/8, fd00::/8 ,192.168.1.5,::1")])).unwrap();
        let t = &c.trusted_proxies;
        for ip in ["10.1.2.3", "fd12::1", "192.168.1.5", "::1"] {
            assert!(t.contains(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["11.0.0.1", "fe80::1", "192.168.1.6", "::2", "127.0.0.1"] {
            assert!(!t.contains(ip.parse().unwrap()), "{ip}");
        }
        assert!(format!("{c:?}").contains("trusted_proxies"));
    }

    #[test]
    fn an_invalid_trusted_proxy_refuses_startup_naming_the_variable() {
        for bad in [
            "nope",
            "10.0.0.0/33",
            "fd00::/129",
            "10.0.0.0/",
            "/8",
            "10.0.0.0/+8",
            "10.0.0.0/-1",
            "10.0.0.0/8/8",
            "10.0.0.1/8",
            "fd00::1/8",
            "0.0.0.0/0",
            "::/0",
            "10.0.0.0/8, nope",
            "[::1]",
            "10.0.0.1:80",
            "localhost",
        ] {
            let msg = match GatewayConfig::from_lookup(with(&[(TRUSTED_PROXIES_ENV, bad)])) {
                Err(e) => format!("{e:#}"),
                Ok(_) => panic!("{bad:?} was accepted"),
            };
            assert!(msg.contains(TRUSTED_PROXIES_ENV), "{bad}: {msg}");
        }
    }

    // ---- RT1 T3 (S18): the realtime upstream ----

    #[test]
    fn realtime_upstream_defaults_to_loopback_8087() {
        let c = GatewayConfig::from_lookup(with(&[])).unwrap();
        assert_eq!(c.upstreams.addr(chelabase_routes::Service::Realtime), "127.0.0.1:8087");
    }

    #[test]
    fn realtime_upstream_is_set_by_chelabase_realtime_url() {
        let c = GatewayConfig::from_lookup(with(&[("CHELABASE_REALTIME_URL", "http://realtime:8087/")])).unwrap();
        assert_eq!(c.upstreams.addr(chelabase_routes::Service::Realtime), "realtime:8087");
        // The other upstreams are untouched.
        assert_eq!(c.upstreams.addr(chelabase_routes::Service::Rest), "127.0.0.1:8085");
        assert_eq!(c.upstreams.addr(chelabase_routes::Service::Orchestrator), "127.0.0.1:8081");
    }

    #[test]
    fn a_bad_realtime_url_is_refused_naming_the_variable() {
        for bad in ["https://realtime:8087", "http://realtime", "http://realtime:8087/ws"] {
            let msg = GatewayConfig::from_lookup(with(&[("CHELABASE_REALTIME_URL", bad)]))
                .unwrap_err()
                .to_string();
            assert!(msg.contains("CHELABASE_REALTIME_URL") || msg.contains(bad), "{bad}: {msg}");
        }
    }

    #[test]
    fn upstreams_new_resolves_realtime_to_the_pinned_default() {
        let upstreams = Upstreams::new("http://127.0.0.1:8081", "http://127.0.0.1:8084").unwrap();
        assert_eq!(upstreams.addr(chelabase_routes::Service::Realtime), "127.0.0.1:8087");
    }
}
