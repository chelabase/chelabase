//! auth settings from the environment (`demo.env` in local dev).

use anyhow::{Context, Result};
use chelabase_secrets::SecretKey;
pub use chelabase_types::mode::Mode;
use chelabase_types::mode_policy::ModePolicy;
use std::path::PathBuf;
use std::time::Duration;

/// Sign-in and sign-up answers take at least this long. It must exceed Zitadel's slowest real
/// path (password hashing on sign-up, then verifying it for the session; ~0.8 s locally),
/// or timing tells existing accounts from unknown ones. `just auth-e2e` checks it live.
pub const DEFAULT_MIN_RESPONSE_MS: u64 = 1500;

#[derive(Clone)]
pub struct Config {
    pub listen: String,
    pub issuer: String,
    pub token: String,
    pub cookie_key: SecretKey,
    pub org_id: String,
    /// The first Studio origin; email links point at its pages.
    pub studio_url: String,
    pub skip_email_verification: bool,
    pub secure_cookies: bool,
    pub min_response: Duration,
    pub mode: Mode,
}

fn var(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| {
        format!(
            "{name} is not set; run `just zitadel-bootstrap` and load .chelabase/zitadel/demo.env"
        )
    })
}

fn truthy(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

fn flag(name: &str) -> bool {
    truthy(std::env::var(name).ok().as_deref())
}

/// Self-hosted mode creates sign-ups with the email already verified; platform mode sends a
/// code and requires it. A non-blank `CHELABASE_REQUIRE_EMAIL_VERIFICATION` overrides the mode.
fn requires_email_verification(mode: Mode, overridden: Option<&str>) -> bool {
    match overridden.map(str::trim).filter(|v| !v.is_empty()) {
        Some(value) => truthy(Some(value)),
        None => ModePolicy::new(mode).email_verification_default(),
    }
}

/// Cookies get `Secure` on platform (served over TLS), not in self-hosted local dev; a non-blank
/// `CHELABASE_AUTH_SECURE_COOKIES` overrides.
fn secure_cookies(mode: Mode, overridden: Option<&str>) -> bool {
    match overridden.map(str::trim).filter(|v| !v.is_empty()) {
        Some(value) => truthy(Some(value)),
        None => ModePolicy::new(mode).secure_cookies_default(),
    }
}

/// auth rate-limits on the rightmost `X-Forwarded-For` entry, which only the gateway can
/// set if nothing else can reach auth: a non-loopback address needs an explicit opt-in.
fn check_listen(listen: &str, trusted_proxy: bool) -> Result<()> {
    let host = listen.rsplit_once(':').map_or(listen, |(h, _)| h);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let loopback = host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if loopback || trusted_proxy {
        return Ok(());
    }
    anyhow::bail!(
        "CHELABASE_AUTH_ADDR={listen} is reachable without the gateway, so clients could forge \
         X-Forwarded-For. Bind to loopback, or set CHELABASE_AUTH_TRUSTED_PROXY=true if only a \
         proxy that appends X-Forwarded-For can reach it."
    )
}

/// Readable by group or others (a Unix mode).
fn too_open(mode: u32) -> bool {
    mode & 0o077 != 0
}

fn warn_if_open(path: &str) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path)
            && too_open(meta.permissions().mode())
        {
            tracing::warn!("{path} is readable by other users; chmod 600 it");
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let mode = Mode::from_env().map_err(anyhow::Error::msg)?;
        let token_file = var("CHELABASE_AUTH_TOKEN_FILE")?;
        warn_if_open(&token_file);
        let token =
            std::fs::read_to_string(&token_file).with_context(|| format!("read {token_file}"))?;
        let cookie_key_file = var("CHELABASE_AUTH_COOKIE_KEY")?;
        warn_if_open(&cookie_key_file);
        let cookie_key = SecretKey::from_file(std::path::Path::new(&cookie_key_file))
            .map_err(|e| anyhow::anyhow!("cookie key: {e}"))?;
        let studio_url = std::env::var("CHELABASE_STUDIO_ORIGINS")
            .unwrap_or_else(|_| "http://localhost:3000".into())
            .split(',')
            .map(str::trim)
            .find(|o| !o.is_empty() && *o != "*")
            .unwrap_or("http://localhost:3000")
            .to_string();
        let min_ms: u64 = std::env::var("CHELABASE_AUTH_MIN_RESPONSE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MIN_RESPONSE_MS);
        let listen =
            std::env::var("CHELABASE_AUTH_ADDR").unwrap_or_else(|_| "127.0.0.1:8084".into());
        check_listen(&listen, flag("CHELABASE_AUTH_TRUSTED_PROXY"))?;
        Ok(Self {
            listen,
            issuer: var("CHELABASE_ZITADEL_ISSUER")?,
            token: token.trim().to_string(),
            cookie_key,
            org_id: var("CHELABASE_ZITADEL_ORG_ID")?,
            studio_url,
            skip_email_verification: !requires_email_verification(
                mode,
                std::env::var("CHELABASE_REQUIRE_EMAIL_VERIFICATION")
                    .ok()
                    .as_deref(),
            ),
            mode,
            secure_cookies: secure_cookies(
                mode,
                std::env::var("CHELABASE_AUTH_SECURE_COOKIES")
                    .ok()
                    .as_deref(),
            ),
            min_response: Duration::from_millis(min_ms),
        })
    }
}

/// Settings for app users' flows (`/auth/v1/app/*`). Without the app key pair, the identity
/// verify key or the admin token file, those routes answer 503 and the platform flows run as
/// before.
#[derive(Clone, Debug, PartialEq)]
pub struct AppConfig {
    pub private_key: PathBuf,
    pub public_key: PathBuf,
    pub access_ttl: Duration,
    pub internal_url: String,
    pub internal_token: String,
    /// The gateway's identity verify key (tokens with `aud = "auth"`).
    pub verify_key: PathBuf,
    /// Zitadel's admin token: the login client can't delete users (P32), and a sign-up whose
    /// commit fails deletes the user it made.
    pub admin_pat_file: PathBuf,
}

pub const DEFAULT_APP_ACCESS_TTL_SECS: u64 = 900;
pub const DEFAULT_INTERNAL_URL: &str = "http://127.0.0.1:8083";
pub const DEFAULT_APP_AUTH_INTERNAL_TOKEN: &str = chelabase_types::mode_policy::DEV_APP_AUTH_INTERNAL_TOKEN;

impl AppConfig {
    /// `(url, bearer)` of the orchestrator's internal listener, for the sign-up gate; it needs
    /// none of the app-user keys.
    ///
    /// Each falls back to its development default, with a warning, because a deployment that
    /// forgets them would otherwise talk to the wrong address or use a publicly known bearer.
    pub fn internal_from_env() -> (String, String) {
        let get = |name: &str| {
            std::env::var(name)
                .ok()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let url = get("CHELABASE_INTERNAL_URL").unwrap_or_else(|| {
            tracing::warn!("CHELABASE_INTERNAL_URL is not set; the sign-up gate uses {DEFAULT_INTERNAL_URL}");
            DEFAULT_INTERNAL_URL.into()
        });
        let token = get("CHELABASE_APP_AUTH_INTERNAL_TOKEN").unwrap_or_else(|| {
            tracing::warn!(
                "CHELABASE_APP_AUTH_INTERNAL_TOKEN is not set; the sign-up gate uses the development token"
            );
            DEFAULT_APP_AUTH_INTERNAL_TOKEN.into()
        });
        (url, token)
    }

    /// M9: a platform instance doesn't start with the development bearer (the variable unset).
    pub fn check_internal_token(mode: Mode, token: &str) -> Result<()> {
        ModePolicy::new(mode)
            .check_internal_token(token)
            .map_err(anyhow::Error::msg)
    }

    /// The admin Zitadel client from `CHELABASE_ZITADEL_ADMIN_PAT_FILE`, independent of the app
    /// keys; `None` (with a warning) when the variable is unset. Accept-invite needs it.
    pub fn admin_from_env(issuer: &str) -> Result<Option<crate::zitadel::Zitadel>> {
        let Some(path) = std::env::var("CHELABASE_ZITADEL_ADMIN_PAT_FILE")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
        else {
            tracing::warn!(
                "CHELABASE_ZITADEL_ADMIN_PAT_FILE is not set: accepting invites answers 503"
            );
            return Ok(None);
        };
        let pat = std::fs::read_to_string(&path).with_context(|| format!("read {path}"))?;
        Ok(Some(crate::zitadel::Zitadel::new(issuer, pat.trim())))
    }

    pub fn from_env() -> Result<Option<Self>> {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        let get = |name: &str| get(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let (Some(private_key), Some(public_key)) = (
            get("CHELABASE_APP_JWT_PRIVATE_KEY"),
            get("CHELABASE_APP_JWT_PUBLIC_KEY"),
        ) else {
            tracing::info!("app users are off: CHELABASE_APP_JWT_PRIVATE_KEY/PUBLIC_KEY are not set");
            return Ok(None);
        };
        let mut missing = Vec::new();
        let verify_key = get("CHELABASE_INTERNAL_VERIFY_KEY");
        if verify_key.is_none() {
            missing.push("CHELABASE_INTERNAL_VERIFY_KEY");
        }
        let admin_pat_file = get("CHELABASE_ZITADEL_ADMIN_PAT_FILE");
        if admin_pat_file.is_none() {
            missing.push("CHELABASE_ZITADEL_ADMIN_PAT_FILE");
        }
        let (Some(verify_key), Some(admin_pat_file)) = (verify_key, admin_pat_file) else {
            tracing::warn!("app users are off: {} not set", missing.join(" and "));
            return Ok(None);
        };
        let access_ttl = match get("CHELABASE_APP_ACCESS_TTL_SECS") {
            None => DEFAULT_APP_ACCESS_TTL_SECS,
            Some(v) => v
                .parse::<u64>()
                .ok()
                .filter(|n| (60..=86_400).contains(n))
                .with_context(|| {
                    format!("CHELABASE_APP_ACCESS_TTL_SECS={v}: expected seconds from 60 to 86400")
                })?,
        };
        Ok(Some(Self {
            private_key: private_key.into(),
            public_key: public_key.into(),
            access_ttl: Duration::from_secs(access_ttl),
            internal_url: get("CHELABASE_INTERNAL_URL").unwrap_or_else(|| DEFAULT_INTERNAL_URL.into()),
            internal_token: get("CHELABASE_APP_AUTH_INTERNAL_TOKEN")
                .unwrap_or_else(|| DEFAULT_APP_AUTH_INTERNAL_TOKEN.into()),
            verify_key: verify_key.into(),
            admin_pat_file: admin_pat_file.into(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| map.get(name).cloned()
    }

    const FULL: [(&str, &str); 4] = [
        ("CHELABASE_APP_JWT_PRIVATE_KEY", "/k/app-jwt.key"),
        ("CHELABASE_APP_JWT_PUBLIC_KEY", "/k/app-jwt.pub"),
        ("CHELABASE_INTERNAL_VERIFY_KEY", "/k/internal.pub"),
        ("CHELABASE_ZITADEL_ADMIN_PAT_FILE", "/k/admin.pat"),
    ];

    // M9: a platform instance refuses the development internal bearer; self-hosted allows it.
    #[test]
    fn platform_refuses_the_default_internal_token() {
        assert!(AppConfig::check_internal_token(Mode::SelfHosted, DEFAULT_APP_AUTH_INTERNAL_TOKEN).is_ok());
        assert!(AppConfig::check_internal_token(Mode::Platform, DEFAULT_APP_AUTH_INTERNAL_TOKEN).is_err());
        assert!(AppConfig::check_internal_token(Mode::Platform, "a-long-random-secret").is_ok());
        let c = AppConfig::from_vars(vars(&FULL)).unwrap().unwrap();
        assert!(AppConfig::check_internal_token(Mode::Platform, &c.internal_token).is_err(), "unset means the default");
    }

    #[test]
    fn app_config_defaults() {
        let c = AppConfig::from_vars(vars(&FULL)).unwrap().unwrap();
        assert_eq!(c.access_ttl, Duration::from_secs(900));
        assert_eq!(c.internal_url, "http://127.0.0.1:8083");
        assert_eq!(c.internal_token, "chelabase-dev-app-auth");
        assert_eq!(c.admin_pat_file, PathBuf::from("/k/admin.pat"));
        let mut more = FULL.to_vec();
        more.push(("CHELABASE_APP_ACCESS_TTL_SECS", "300"));
        more.push(("CHELABASE_INTERNAL_URL", "http://10.0.0.1:9000"));
        let c = AppConfig::from_vars(vars(&more)).unwrap().unwrap();
        assert_eq!(c.access_ttl, Duration::from_secs(300));
        assert_eq!(c.internal_url, "http://10.0.0.1:9000");
    }

    #[test]
    fn app_config_is_off_without_keys_verify_key_or_admin_token() {
        assert_eq!(AppConfig::from_vars(vars(&[])).unwrap(), None);
        for (name, _) in FULL {
            let some: Vec<_> = FULL.iter().filter(|(n, _)| *n != name).copied().collect();
            assert_eq!(AppConfig::from_vars(vars(&some)).unwrap(), None, "without {name}");
        }
        let mut bad = FULL.to_vec();
        bad.push(("CHELABASE_APP_ACCESS_TTL_SECS", "soon"));
        assert!(AppConfig::from_vars(vars(&bad)).is_err());
    }

    #[test]
    fn cookies_are_secure_on_platform_unless_overridden() {
        assert!(!secure_cookies(Mode::SelfHosted, None));
        assert!(secure_cookies(Mode::Platform, None));
        assert!(secure_cookies(Mode::SelfHosted, Some("true")));
        assert!(!secure_cookies(Mode::Platform, Some("false")));
    }

    // X-Forwarded-For is only trustworthy when the gateway is the only way in.
    #[test]
    fn a_non_loopback_listen_needs_a_trusted_proxy() {
        assert!(check_listen("127.0.0.1:8084", false).is_ok());
        assert!(check_listen("[::1]:8084", false).is_ok());
        assert!(check_listen("localhost:8084", false).is_ok());
        let err = check_listen("0.0.0.0:8084", false).unwrap_err().to_string();
        assert!(err.contains("CHELABASE_AUTH_TRUSTED_PROXY"), "{err}");
        assert!(check_listen("0.0.0.0:8084", true).is_ok());
    }

    #[test]
    fn group_or_world_readable_secrets_are_flagged() {
        assert!(!too_open(0o600));
        assert!(!too_open(0o400));
        assert!(too_open(0o640));
        assert!(too_open(0o644));
    }

    // Self-hosted (the default mode) skips verification; platform requires it.
    // CHELABASE_REQUIRE_EMAIL_VERIFICATION, when set, overrides either way.
    #[test]
    fn email_verification_follows_the_mode_unless_overridden() {
        assert!(!requires_email_verification(Mode::SelfHosted, None));
        assert!(requires_email_verification(Mode::Platform, None));
        assert!(requires_email_verification(Mode::SelfHosted, Some("true")));
        assert!(!requires_email_verification(Mode::Platform, Some("false")));
        assert!(!requires_email_verification(Mode::SelfHosted, Some("")));
        assert!(requires_email_verification(Mode::Platform, Some(" ")));
    }
}
