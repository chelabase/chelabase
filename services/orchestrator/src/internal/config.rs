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
    pub storcon_url: String,
    pub compute_jwt_dir: PathBuf,
    pub compute_network: String,
    /// `CHELABASE_CONTROL_PLANE_URI`: how computes reach this listener; the DDL receiver's URL
    /// (`neon.console_url`) is built from it.
    pub control_plane_uri: String,
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

/// A session never lasts longer than this after it was created (and so neither can a refresh).
pub const APP_SESSION_CAP: Duration = Duration::from_secs(90 * 24 * 60 * 60);

impl InternalConfig {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Blank values count as unset.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let or = |name: &str, default: &str| {
            get(name)
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        Ok(Self {
            addr: or("CHELABASE_INTERNAL_ADDR", "127.0.0.1:8083")
                .parse()
                .context("CHELABASE_INTERNAL_ADDR is not a socket address")?,
            proxy_token: or("CHELABASE_PROXY_API_TOKEN", "chelabase-dev-proxy-api"),
            storcon_token: or("CHELABASE_STORCON_HOOK_TOKEN", "chelabase-dev-storcon-hook"),
            data_keys_token: or("CHELABASE_DATA_KEYS_TOKEN", "chelabase-dev-data-keys"),
            rest_token: or("CHELABASE_REST_INTERNAL_TOKEN", "chelabase-dev-rest-internal"),
            storcon_url: or("CHELABASE_STORCON_URL", "http://127.0.0.1:1234"),
            compute_jwt_dir: PathBuf::from(or("CHELABASE_COMPUTE_JWT_DIR", ".chelabase/neon")),
            compute_network: or("CHELABASE_COMPUTE_NETWORK", "chelabase_default"),
            control_plane_uri: check_control_plane_uri(or(
                "CHELABASE_CONTROL_PLANE_URI",
                "http://host.docker.internal:8083",
            ))?,
            valkey_url: or("CHELABASE_VALKEY_URL", "redis://127.0.0.1:6379"),
            app_auth_token: or("CHELABASE_APP_AUTH_INTERNAL_TOKEN", "chelabase-dev-app-auth"),
            app_sessions_token: or(
                "CHELABASE_APP_SESSIONS_INTERNAL_TOKEN",
                "chelabase-dev-app-sessions",
            ),
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

    /// Startup refusals that depend on the mode (M9): a platform instance doesn't run with the
    /// development bearer (`CHELABASE_APP_AUTH_INTERNAL_TOKEN` unset), which is public.
    pub fn check_mode(&self, policy: &chelabase_types::mode_policy::ModePolicy) -> Result<()> {
        policy.check_internal_token(&self.app_auth_token).map_err(|e| anyhow!(e))
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
            "CHELABASE_CONTROL_PLANE_URI={uri:?}: {why} (for example http://host.docker.internal:8083)"
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
            .field("storcon_url", &self.storcon_url)
            .field("compute_jwt_dir", &self.compute_jwt_dir)
            .field("compute_network", &self.compute_network)
            .field("control_plane_uri", &self.control_plane_uri)
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
        assert_eq!(c.compute_network, "chelabase_default");
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
        assert_eq!(d.control_plane_uri, "http://host.docker.internal:8083");
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

    // M9: a platform instance refuses to start with the development bearer.
    #[test]
    fn platform_refuses_the_default_app_auth_token() {
        use chelabase_types::mode::Mode;
        use chelabase_types::mode_policy::ModePolicy;
        let default = InternalConfig::from_lookup(|_| None).unwrap();
        let own = InternalConfig::from_lookup(|name| {
            (name == "CHELABASE_APP_AUTH_INTERNAL_TOKEN").then(|| "a-long-random-secret".to_string())
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
            "CHELABASE_ZITADEL_ADMIN_PAT_FILE" => Some(".chelabase/zitadel/admin.pat".into()),
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
            Some(PathBuf::from(".chelabase/zitadel/admin.pat"))
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
