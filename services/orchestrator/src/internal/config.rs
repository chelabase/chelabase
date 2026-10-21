//! Settings for the internal listener, all with local-dev defaults.

use anyhow::{Context, Result, anyhow};
use chelabase_neon::ComputeSigner;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone)]
pub struct InternalConfig {
    pub addr: SocketAddr,
    pub proxy_token: String,
    pub storcon_token: String,
    pub data_keys_token: String,
    pub rest_token: String,
    /// `CHELABASE_REALTIME_INTERNAL_TOKEN[_FILE]`: realtime's bearer (its `/internal/realtime/*`
    /// routes, and connection info and key lookups here).
    pub realtime_token: String,
    pub storcon_url: String,
    pub compute_jwt_dir: PathBuf,
    pub compute_network: String,
    /// `CHELABASE_COMPUTE_CONTROL_NETWORK` (default `chelabase_control`): the network the
    /// console and the SQL editor reach computes on, over TLS.
    pub compute_control_network: String,
    /// `CHELABASE_COMPUTE_TLS_DIR` (default `.chelabase/orchestrator-tls`): the client pair the
    /// worker renews; the editor presents it to computes.
    pub compute_tls_dir: PathBuf,
    /// `CHELABASE_MEMBER_LOGIN_CIDRS`: where member logins come from, as the computes' spec
    /// carries it. `None` when unset ([`Self::member_logins`] refuses to start); read raw, so an
    /// empty value is `Some([])`, "no address rule".
    pub member_login_cidrs: Option<Vec<String>>,
    /// `CHELABASE_CONTROL_PLANE_URI`: how computes reach this listener; the DDL receiver's URL
    /// (`neon.console_url`) is built from it.
    pub control_plane_uri: String,
    /// `CHELABASE_STACK` (default `main`): the stack whose computes this orchestrator locates.
    pub stack: String,
    /// `CHELABASE_VALKEY_URL`: where the receiver tells the proxies a role's secret changed.
    pub valkey_url: String,
    /// `CHELABASE_APP_AUTH_INTERNAL_TOKEN`: the `auth` service's bearer for the app routes.
    pub app_auth_token: String,
    /// `CHELABASE_APP_SESSIONS_INTERNAL_TOKEN`: the gateway's bearer, good only for a session's
    /// liveness.
    pub app_sessions_token: String,
    /// `CHELABASE_APP_REFRESH_TTL_SECS`: how long a refresh keeps an app session alive.
    pub app_refresh_ttl: Duration,
    /// `CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY`: checks the Actions webhook's signature; without
    /// it the webhook answers 503.
    pub actions_signing_key: Option<String>,
    /// `CHELABASE_ZITADEL_ADMIN_PAT_FILE`: the admin PAT that deletes Zitadel sessions.
    pub zitadel_admin_pat_file: Option<PathBuf>,
    /// Zitadel's base URL, from `CHELABASE_AUTH_HOST` (and `CHELABASE_PUBLIC_SCHEME`).
    pub zitadel_issuer: Option<String>,
}

/// Every bearer the internal listener checks, with its development default (public: a platform
/// instance refuses each, [`InternalConfig::check_mode`]).
pub const DEV_BEARERS: [(&str, &str); 7] = [
    ("CHELABASE_PROXY_API_TOKEN", "chelabase-dev-proxy-api"),
    ("CHELABASE_STORCON_HOOK_TOKEN", "chelabase-dev-storcon-hook"),
    ("CHELABASE_DATA_KEYS_TOKEN", "chelabase-dev-data-keys"),
    ("CHELABASE_REST_INTERNAL_TOKEN", "chelabase-dev-rest-internal"),
    ("CHELABASE_REALTIME_INTERNAL_TOKEN", "chelabase-dev-realtime-internal"),
    ("CHELABASE_APP_AUTH_INTERNAL_TOKEN", chelabase_types::mode_policy::DEV_APP_AUTH_INTERNAL_TOKEN),
    ("CHELABASE_APP_SESSIONS_INTERNAL_TOKEN", "chelabase-dev-app-sessions"),
];

/// A session never lasts longer than this after it was created (and so neither can a refresh).
pub const APP_SESSION_CAP: Duration = Duration::from_secs(90 * 24 * 60 * 60);

impl InternalConfig {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Blank values count as unset. Each bearer is `<NAME>` or the file `<NAME>_FILE` names (the
    /// stack mounts generated secrets, S27); unset, it is the development default.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let or = |name: &str, default: &str| {
            get(name)
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        let secret = |name: &str, default: &str| -> Result<String> {
            Ok(chelabase_types::secret_env::lookup_secret(&get, name)
                .map_err(|e| anyhow!(e))?
                .unwrap_or_else(|| default.to_string()))
        };
        Ok(Self {
            addr: or("CHELABASE_INTERNAL_ADDR", "127.0.0.1:8083")
                .parse()
                .context("CHELABASE_INTERNAL_ADDR is not a socket address")?,
            proxy_token: secret(DEV_BEARERS[0].0, DEV_BEARERS[0].1)?,
            storcon_token: secret(DEV_BEARERS[1].0, DEV_BEARERS[1].1)?,
            data_keys_token: secret(DEV_BEARERS[2].0, DEV_BEARERS[2].1)?,
            rest_token: secret(DEV_BEARERS[3].0, DEV_BEARERS[3].1)?,
            realtime_token: secret(DEV_BEARERS[4].0, DEV_BEARERS[4].1)?,
            storcon_url: or("CHELABASE_STORCON_URL", "http://127.0.0.1:1234"),
            compute_jwt_dir: PathBuf::from(or("CHELABASE_COMPUTE_JWT_DIR", ".chelabase/neon")),
            compute_network: or("CHELABASE_COMPUTE_NETWORK", "chelabase_compute"),
            compute_control_network: or(
                "CHELABASE_COMPUTE_CONTROL_NETWORK",
                "chelabase_control",
            ),
            compute_tls_dir: PathBuf::from(or(
                "CHELABASE_COMPUTE_TLS_DIR",
                ".chelabase/orchestrator-tls",
            )),
            member_login_cidrs: get("CHELABASE_MEMBER_LOGIN_CIDRS")
                .map(|raw| chelabase_neon::spec::parse_member_login_cidrs(Some(&raw)))
                .transpose()?,
            control_plane_uri: check_control_plane_uri(or(
                "CHELABASE_CONTROL_PLANE_URI",
                "http://orchestrator:8083",
            ))?,
            stack: chelabase_neon::check_stack(&or(
                "CHELABASE_STACK",
                chelabase_neon::DEFAULT_STACK,
            ))?,
            valkey_url: or("CHELABASE_VALKEY_URL", "redis://127.0.0.1:6379"),
            app_auth_token: secret(DEV_BEARERS[5].0, DEV_BEARERS[5].1)?,
            app_sessions_token: secret(DEV_BEARERS[6].0, DEV_BEARERS[6].1)?,
            app_refresh_ttl: refresh_ttl(&or("CHELABASE_APP_REFRESH_TTL_SECS", "2592000"))?,
            actions_signing_key: get("CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY")
                .filter(|v| !v.trim().is_empty()),
            zitadel_admin_pat_file: get("CHELABASE_ZITADEL_ADMIN_PAT_FILE")
                .filter(|v| !v.trim().is_empty())
                .map(PathBuf::from),
            zitadel_issuer: chelabase_types::entry::issuer_from_lookup(&get)
                .map_err(anyhow::Error::new)?,
        })
    }

    /// Startup refusals that depend on the mode (M9): a platform instance runs with none of the
    /// internal listener's development bearers (each is public), whether its variable is unset
    /// or set to one of them. The error names the variable, never the token.
    pub fn check_mode(&self, policy: &chelabase_types::mode_policy::ModePolicy) -> Result<()> {
        let dev_values: Vec<&str> = DEV_BEARERS.iter().map(|(_, value)| *value).collect();
        for (var, token) in self.bearers() {
            policy
                .check_dev_bearer(var, token, &dev_values)
                .map_err(|e| anyhow!(e))?;
        }
        Ok(())
    }

    /// Every bearer the internal listener checks, with its variable, in [`DEV_BEARERS`]' order.
    fn bearers(&self) -> [(&'static str, &str); 7] {
        let values = [
            self.proxy_token.as_str(),
            self.storcon_token.as_str(),
            self.data_keys_token.as_str(),
            self.rest_token.as_str(),
            self.realtime_token.as_str(),
            self.app_auth_token.as_str(),
            self.app_sessions_token.as_str(),
        ];
        std::array::from_fn(|i| (DEV_BEARERS[i].0, values[i]))
    }

    /// The Zitadel admin client (P2), or `None` (with a warning) when the PAT file or the
    /// issuer is missing: then Zitadel sessions aren't deleted on revoke.
    pub fn load_zitadel(&self) -> Option<chelabase_zitadel::client::Zitadel> {
        let (Some(file), Some(issuer)) = (&self.zitadel_admin_pat_file, &self.zitadel_issuer)
        else {
            tracing::warn!(
                "CHELABASE_ZITADEL_ADMIN_PAT_FILE or CHELABASE_AUTH_HOST is unset: \
                 revoked app sessions keep their Zitadel sessions"
            );
            return None;
        };
        match std::fs::read_to_string(file) {
            Ok(pat) if !pat.trim().is_empty() => {
                Some(chelabase_zitadel::client::Zitadel::new(issuer, pat.trim()))
            }
            Ok(_) => {
                tracing::warn!(file = %file.display(), "the Zitadel admin PAT file is empty");
                None
            }
            Err(err) => {
                tracing::warn!(file = %file.display(), "reading the Zitadel admin PAT: {err}");
                None
            }
        }
    }

    /// What every compute's spec says about member logins: the configured CIDRs and the
    /// orchestrator's certificate CN. Unset `CHELABASE_MEMBER_LOGIN_CIDRS` is the startup error.
    pub fn member_logins(&self) -> Result<chelabase_neon::spec::MemberLogins> {
        match &self.member_login_cidrs {
            Some(cidrs) => Ok(chelabase_neon::spec::MemberLogins::orchestrator(cidrs.clone())),
            None => chelabase_neon::spec::parse_member_login_cidrs(None)
                .map(chelabase_neon::spec::MemberLogins::orchestrator),
        }
    }

    /// The key pair `chelabase dev bootstrap` writes.
    pub fn load_signer(&self) -> Result<ComputeSigner> {
        chelabase_neon::jwt::load_signer(&self.compute_jwt_dir)
    }
}

/// Whole seconds, at least 1 and at most the session cap.
fn refresh_ttl(value: &str) -> Result<Duration> {
    let secs: u64 = value
        .trim()
        .parse()
        .ok()
        .filter(|s| (1..=APP_SESSION_CAP.as_secs()).contains(s))
        .ok_or_else(|| {
            anyhow!(
                "CHELABASE_APP_REFRESH_TTL_SECS={value:?}: expected whole seconds from 1 to {}",
                APP_SESSION_CAP.as_secs()
            )
        })?;
    Ok(Duration::from_secs(secs))
}

/// Computes call this address from inside their containers, so it needs a scheme, a host and
/// an explicit port (the worker's check).
fn check_control_plane_uri(uri: String) -> Result<String> {
    let bad = |why: &str| {
        anyhow!(
            "CHELABASE_CONTROL_PLANE_URI={uri:?}: {why} (for example http://orchestrator:8083)"
        )
    };
    let url = reqwest::Url::parse(&uri).map_err(|e| bad(&format!("not a URL ({e})")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(bad("the scheme must be http or https"));
    }
    if url.host_str().is_none() {
        return Err(bad("it needs a host"));
    }
    if url.port().is_none() {
        return Err(bad("it needs an explicit port"));
    }
    Ok(uri)
}

/// By hand, so the bearer tokens never reach a log.
impl std::fmt::Debug for InternalConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InternalConfig")
            .field("addr", &self.addr)
            .field("proxy_token", &"<redacted>")
            .field("storcon_token", &"<redacted>")
            .field("data_keys_token", &"<redacted>")
            .field("rest_token", &"<redacted>")
            .field("realtime_token", &"<redacted>")
            .field("storcon_url", &self.storcon_url)
            .field("compute_jwt_dir", &self.compute_jwt_dir)
            .field("compute_network", &self.compute_network)
            .field("compute_control_network", &self.compute_control_network)
            .field("compute_tls_dir", &self.compute_tls_dir)
            .field("member_login_cidrs", &self.member_login_cidrs)
            .field("control_plane_uri", &self.control_plane_uri)
            .field("stack", &self.stack)
            // A URL with credentials (`redis://:pw@host`) isn't shown.
            .field(
                "valkey_url",
                &if self.valkey_url.contains('@') {
                    "<redacted>"
                } else {
                    self.valkey_url.as_str()
                },
            )
            .field("app_auth_token", &"<redacted>")
            .field("app_sessions_token", &"<redacted>")
            .field("app_refresh_ttl", &self.app_refresh_ttl)
            .field(
                "actions_signing_key",
                &self.actions_signing_key.as_ref().map(|_| "<redacted>"),
            )
            .field("zitadel_admin_pat_file", &self.zitadel_admin_pat_file)
            .field("zitadel_issuer", &self.zitadel_issuer)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_neon::jwt::{PRIVATE_KEY_FILE, PUBLIC_KEY_FILE};

    #[test]
    fn defaults_are_for_local_dev() {
        let c = InternalConfig::from_lookup(|_| None).unwrap();
        assert_eq!(c.addr, "127.0.0.1:8083".parse().unwrap());
        assert_eq!(c.proxy_token, "chelabase-dev-proxy-api");
        assert_eq!(c.storcon_token, "chelabase-dev-storcon-hook");
        assert_eq!(c.data_keys_token, "chelabase-dev-data-keys");
        assert_eq!(c.rest_token, "chelabase-dev-rest-internal");
        assert_eq!(c.storcon_url, "http://127.0.0.1:1234");
        assert_eq!(c.compute_jwt_dir, PathBuf::from(".chelabase/neon"));
        assert_eq!(c.compute_network, "chelabase_compute");
        assert_eq!(c.stack, "main");
        assert_eq!(c.control_plane_uri.as_str(), "http://orchestrator:8083");
    }

    #[test]
    fn compute_tls_settings_and_member_login_cidrs() {
        let c = InternalConfig::from_lookup(|_| None).unwrap();
        assert_eq!(c.compute_control_network, "chelabase_control");
        assert_eq!(c.compute_tls_dir, PathBuf::from(".chelabase/orchestrator-tls"));
        // Unset: read as None, and asking for the logins is the startup error.
        assert_eq!(c.member_login_cidrs, None);
        assert!(format!("{:#}", c.member_logins().unwrap_err()).contains("CHELABASE_MEMBER_LOGIN_CIDRS"));
        let with = |value: &'static str| {
            InternalConfig::from_lookup(move |k| {
                (k == "CHELABASE_MEMBER_LOGIN_CIDRS").then(|| value.to_string())
            })
        };
        // Empty (even blank) is "no address rule", not unset.
        let empty = with("").unwrap().member_logins().unwrap();
        assert!(empty.allowed_cidrs.is_empty());
        assert_eq!(empty.client_cns, ["chelabase-orchestrator"]);
        let listed = with("10.0.0.0/8, fd00::/8").unwrap().member_logins().unwrap();
        assert_eq!(listed.allowed_cidrs, ["10.0.0.0/8", "fd00::/8"]);
        assert!(with("not-a-cidr").is_err());
        let c = InternalConfig::from_lookup(|k| match k {
            "CHELABASE_COMPUTE_CONTROL_NETWORK" => Some("s_control".into()),
            "CHELABASE_COMPUTE_TLS_DIR" => Some("/m/tls".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(c.compute_control_network, "s_control");
        assert_eq!(c.compute_tls_dir, PathBuf::from("/m/tls"));
    }

    #[test]
    fn the_stack_comes_from_the_env() {
        let c = InternalConfig::from_lookup(|k| (k == "CHELABASE_STACK").then(|| "feat-x".to_string()))
            .unwrap();
        assert_eq!(c.stack, "feat-x");
    }

    // T1 review I1: the orchestrator and the worker read the stack the same way.
    #[test]
    fn a_padded_stack_is_trimmed_and_an_invalid_one_refused() {
        let with = |v: &'static str| {
            InternalConfig::from_lookup(move |k| (k == "CHELABASE_STACK").then(|| v.to_string()))
        };
        assert_eq!(with(" feat-x\n").unwrap().stack, "feat-x");
        assert_eq!(with("  ").unwrap().stack, "main");
        assert!(with("feat/x").is_err());
    }

    #[test]
    fn set_values_win_and_blank_ones_do_not() {
        let c = InternalConfig::from_lookup(|name| match name {
            "CHELABASE_INTERNAL_ADDR" => Some("127.0.0.1:9083".into()),
            "CHELABASE_PROXY_API_TOKEN" => Some("  ".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(c.addr.port(), 9083);
        assert_eq!(c.proxy_token, "chelabase-dev-proxy-api");
    }

    #[test]
    fn a_bad_address_names_the_variable() {
        let err = InternalConfig::from_lookup(|name| {
            (name == "CHELABASE_INTERNAL_ADDR").then(|| "nope".to_string())
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("CHELABASE_INTERNAL_ADDR"), "{err}");
    }

    #[test]
    fn config_reads_control_plane_uri_and_valkey() {
        let d = InternalConfig::from_lookup(|_| None).unwrap();
        assert_eq!(d.control_plane_uri, "http://orchestrator:8083");
        assert_eq!(d.valkey_url, "redis://127.0.0.1:6379");
        let c = InternalConfig::from_lookup(|name| match name {
            "CHELABASE_CONTROL_PLANE_URI" => Some("http://172.17.0.1:9083".into()),
            "CHELABASE_VALKEY_URL" => Some("redis://valkey:6379".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(c.control_plane_uri, "http://172.17.0.1:9083");
        assert_eq!(c.valkey_url, "redis://valkey:6379");
        let blank = InternalConfig::from_lookup(|name| {
            (name == "CHELABASE_VALKEY_URL").then(|| " ".to_string())
        })
        .unwrap();
        assert_eq!(blank.valkey_url, "redis://127.0.0.1:6379");
        for bad in ["host.docker.internal:8083", "http://h", "ftp://h:1"] {
            let err = InternalConfig::from_lookup(|name| {
                (name == "CHELABASE_CONTROL_PLANE_URI").then(|| bad.to_string())
            })
            .unwrap_err()
            .to_string();
            assert!(err.contains("CHELABASE_CONTROL_PLANE_URI"), "{bad}: {err}");
        }
    }

    #[test]
    fn debug_hides_the_tokens() {
        let c = InternalConfig::from_lookup(|_| None).unwrap();
        let shown = format!("{c:?}");
        assert!(!shown.contains("chelabase-dev-proxy-api"), "{shown}");
        assert!(!shown.contains("chelabase-dev-storcon-hook"), "{shown}");
        assert!(!shown.contains("chelabase-dev-data-keys"), "{shown}");
        assert!(!shown.contains("chelabase-dev-rest-internal"), "{shown}");
        assert!(shown.contains("<redacted>"), "{shown}");
    }

    // RT1 T6 (S1): realtime's bearer, `<NAME>` or the file `<NAME>_FILE` names (the stack
    // mounts `realtime/internal.token`); unset, the development default.
    #[test]
    fn the_realtime_token_is_read_like_the_other_bearers() {
        let c = InternalConfig::from_lookup(|_| None).unwrap();
        assert_eq!(c.realtime_token, "chelabase-dev-realtime-internal");
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("internal.token");
        std::fs::write(&file, "generated-realtime-token\n").unwrap();
        let path = file.to_str().unwrap().to_string();
        let c = InternalConfig::from_lookup(move |k| {
            (k == "CHELABASE_REALTIME_INTERNAL_TOKEN_FILE").then(|| path.clone())
        })
        .unwrap();
        assert_eq!(c.realtime_token, "generated-realtime-token");
        let c = InternalConfig::from_lookup(|k| {
            (k == "CHELABASE_REALTIME_INTERNAL_TOKEN").then(|| "plain-token".to_string())
        })
        .unwrap();
        assert_eq!(c.realtime_token, "plain-token");
        let shown = format!("{c:?}");
        assert!(!shown.contains("plain-token"), "{shown}");
    }

    #[test]
    fn app_auth_settings_have_dev_defaults() {
        let c = InternalConfig::from_lookup(|_| None).unwrap();
        assert_eq!(c.app_auth_token, "chelabase-dev-app-auth");
        assert_eq!(c.app_sessions_token, "chelabase-dev-app-sessions");
        assert_eq!(c.app_refresh_ttl, std::time::Duration::from_secs(2_592_000));
        assert_eq!(c.actions_signing_key, None);
        assert_eq!(c.zitadel_admin_pat_file, None);
        assert_eq!(c.zitadel_issuer, None);
    }

    // M9: a platform instance refuses to start with a development bearer (every one of them:
    // `tests/fa_dev_bearers.rs`).
    #[test]
    fn platform_refuses_the_default_app_auth_token() {
        use chelabase_types::mode::Mode;
        use chelabase_types::mode_policy::ModePolicy;
        let default = InternalConfig::from_lookup(|_| None).unwrap();
        let own = InternalConfig::from_lookup(|name| {
            DEV_BEARERS
                .iter()
                .any(|(var, _)| *var == name)
                .then(|| format!("a-long-random-secret-for-{name}"))
        })
        .unwrap();
        let (sh, pf) = (ModePolicy::new(Mode::SelfHosted), ModePolicy::new(Mode::Platform));
        assert!(default.check_mode(&sh).is_ok());
        assert!(default.check_mode(&pf).is_err());
        assert!(own.check_mode(&sh).is_ok() && own.check_mode(&pf).is_ok());
    }

    #[test]
    fn app_auth_settings_are_read() {
        let c = InternalConfig::from_lookup(|name| match name {
            "CHELABASE_APP_AUTH_INTERNAL_TOKEN" => Some("auth-tok".into()),
            "CHELABASE_APP_SESSIONS_INTERNAL_TOKEN" => Some("gw-tok".into()),
            "CHELABASE_APP_REFRESH_TTL_SECS" => Some("3600".into()),
            "CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY" => Some("sign-key".into()),
            "CHELABASE_ZITADEL_ADMIN_PAT_FILE" => Some(".chelabase/stacks/main/zitadel/admin.pat".into()),
            "CHELABASE_PUBLIC_SCHEME" => Some("http".into()),
            "CHELABASE_AUTH_HOST" => Some("auth.chelabase.localhost:8000".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(c.app_auth_token, "auth-tok");
        assert_eq!(c.app_sessions_token, "gw-tok");
        assert_eq!(c.app_refresh_ttl, std::time::Duration::from_secs(3600));
        assert_eq!(c.actions_signing_key.as_deref(), Some("sign-key"));
        assert_eq!(
            c.zitadel_admin_pat_file,
            Some(PathBuf::from(".chelabase/stacks/main/zitadel/admin.pat"))
        );
        assert_eq!(c.zitadel_issuer.as_deref(), Some("http://auth.chelabase.localhost:8000"));
        let blank = InternalConfig::from_lookup(|name| {
            (name == "CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY").then(|| "  ".to_string())
        })
        .unwrap();
        assert_eq!(blank.actions_signing_key, None);
    }

    #[test]
    fn a_bad_refresh_ttl_names_the_variable() {
        for bad in ["0", "-1", "soon", "7776001"] {
            let err = InternalConfig::from_lookup(|name| {
                (name == "CHELABASE_APP_REFRESH_TTL_SECS").then(|| bad.to_string())
            })
            .unwrap_err()
            .to_string();
            assert!(err.contains("CHELABASE_APP_REFRESH_TTL_SECS"), "{bad}: {err}");
        }
    }

    #[test]
    fn debug_hides_the_app_auth_secrets() {
        let c = InternalConfig::from_lookup(|name| match name {
            "CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY" => Some("sign-key-secret".into()),
            _ => None,
        })
        .unwrap();
        let shown = format!("{c:?}");
        assert!(!shown.contains("chelabase-dev-app-auth"), "{shown}");
        assert!(!shown.contains("chelabase-dev-app-sessions"), "{shown}");
        assert!(!shown.contains("sign-key-secret"), "{shown}");
    }

    #[test]
    fn zitadel_needs_the_pat_file_and_the_issuer() {
        let dir = tempfile::tempdir().unwrap();
        let pat = dir.path().join("admin.pat");
        std::fs::write(&pat, "the-pat\n").unwrap();
        let base = InternalConfig::from_lookup(|_| None).unwrap();
        assert!(base.load_zitadel().is_none());
        let only_file = InternalConfig {
            zitadel_admin_pat_file: Some(pat.clone()),
            ..base.clone()
        };
        assert!(only_file.load_zitadel().is_none());
        let missing_file = InternalConfig {
            zitadel_admin_pat_file: Some(dir.path().join("nope.pat")),
            zitadel_issuer: Some("http://127.0.0.1:9".into()),
            ..base.clone()
        };
        assert!(missing_file.load_zitadel().is_none());
        let both = InternalConfig {
            zitadel_admin_pat_file: Some(pat),
            zitadel_issuer: Some("http://127.0.0.1:9".into()),
            ..base
        };
        assert!(both.load_zitadel().is_some());
    }

    // Review Focus 4: an orchestrator started before bootstrap says what to run.
    #[test]
    fn missing_keys_say_to_run_the_bootstrap() {
        let dir = tempfile::tempdir().unwrap();
        let c = InternalConfig {
            compute_jwt_dir: dir.path().to_path_buf(),
            ..InternalConfig::from_lookup(|_| None).unwrap()
        };
        let err = format!("{:#}", c.load_signer().err().expect("no keys"));
        assert!(err.contains("just zitadel-bootstrap"), "{err}");
    }

    #[test]
    fn corrupt_keys_say_to_delete_them_and_rerun_the_bootstrap() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PRIVATE_KEY_FILE), "not a key").unwrap();
        std::fs::write(dir.path().join(PUBLIC_KEY_FILE), "not a key").unwrap();
        let c = InternalConfig {
            compute_jwt_dir: dir.path().to_path_buf(),
            ..InternalConfig::from_lookup(|_| None).unwrap()
        };
        let err = format!("{:#}", c.load_signer().err().expect("corrupt keys"));
        assert!(err.contains("corrupt"), "{err}");
        assert!(err.contains("just zitadel-bootstrap"), "{err}");
    }

    #[test]
    fn keys_from_the_bootstrap_load() {
        let dir = tempfile::tempdir().unwrap();
        let (pem, x) = chelabase_neon::jwt::generate_keypair();
        std::fs::write(dir.path().join(PRIVATE_KEY_FILE), pem).unwrap();
        std::fs::write(dir.path().join(PUBLIC_KEY_FILE), format!("{x}\n")).unwrap();
        let c = InternalConfig {
            compute_jwt_dir: dir.path().to_path_buf(),
            ..InternalConfig::from_lookup(|_| None).unwrap()
        };
        assert!(c.load_signer().is_ok());
    }

    #[test]
    fn the_zitadel_issuer_comes_from_the_auth_host() {
        let c = InternalConfig::from_lookup(|name| match name {
            "CHELABASE_PUBLIC_SCHEME" => Some("http".into()),
            "CHELABASE_AUTH_HOST" => Some("auth.chelabase.localhost:8000".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(c.zitadel_issuer.as_deref(), Some("http://auth.chelabase.localhost:8000"));
        assert_eq!(InternalConfig::from_lookup(|_| None).unwrap().zitadel_issuer, None);
        let err = InternalConfig::from_lookup(|name| {
            (name == "CHELABASE_ZITADEL_ISSUER").then(|| "http://old-issuer.example".to_string())
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("CHELABASE_AUTH_HOST"), "{err}");
    }
}
