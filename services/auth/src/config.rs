//! auth settings from the environment (`demo.env` in local dev).

use anyhow::{Context, Result};
use chelabase_secrets::SecretKey;
use chelabase_types::dev_defaults::DevDefaults;
use chelabase_types::entry::Scheme;
pub use chelabase_types::mode::Mode;
use chelabase_types::mode_policy::ModePolicy;
use std::path::PathBuf;
use std::time::Duration;

/// Sign-in and sign-up answers take at least this long. It must exceed the identity service's
/// slowest real path (password hashing on sign-up, then verifying it for the session),
/// or timing tells existing accounts from unknown ones. `just auth-e2e` checks it live.
pub const DEFAULT_MIN_RESPONSE_MS: u64 = 1500;

#[derive(Clone)]
pub struct Config {
    pub listen: String,
    /// The public issuer (what Studio is told; a token's `iss`).
    pub issuer: String,
    pub cookie_key: SecretKey,
    /// The first Studio origin; email links point at its pages.
    pub studio_url: String,
    /// What `GET /auth/v1/studio-config` serves.
    pub studio: StudioSettings,
    pub skip_email_verification: bool,
    pub secure_cookies: bool,
    pub min_response: Duration,
    pub mode: Mode,
    /// `CHELABASE_AUTH_TRUSTED_PROXY`: the proxies (the gateway) whose `X-Forwarded-For` counts.
    pub trusted_proxies: crate::guard::TrustedProxies,
}

/// Studio's runtime config (`GET /auth/v1/studio-config`): the public URLs.
#[derive(Clone, Debug, Default)]
pub struct StudioSettings {
    /// The public API base (`<scheme>://<api host>`).
    pub api_url: String,
    /// Studio's public host.
    pub studio_host: String,
}

fn var(name: &str) -> Result<String> {
    std::env::var(name).with_context(|| {
        format!(
            "{name} is not set; a workbench stack sets it in its stack.env (`just workbench env` prints the path)"
        )
    })
}

fn truthy(value: Option<&str>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Self-hosted mode creates sign-ups with the email already verified; platform mode sends a
/// code and requires it. A non-blank `CHELABASE_REQUIRE_EMAIL_VERIFICATION` overrides the mode.
fn requires_email_verification(mode: Mode, overridden: Option<&str>) -> bool {
    match overridden.map(str::trim).filter(|v| !v.is_empty()) {
        Some(value) => truthy(Some(value)),
        None => ModePolicy::new(mode).email_verification_default(),
    }
}

/// Cookies get `Secure` when the public entry is https, in every mode; a non-blank
/// `CHELABASE_AUTH_SECURE_COOKIES` overrides.
fn secure_cookies(scheme: Scheme, overridden: Option<&str>) -> bool {
    match overridden.map(str::trim).filter(|v| !v.is_empty()) {
        Some(value) => truthy(Some(value)),
        None => scheme == Scheme::Https,
    }
}

/// auth rate-limits on the rightmost `X-Forwarded-For` entry from a trusted proxy and on the TCP
/// peer otherwise: a non-loopback address needs the proxy named, or every gateway request would
/// share the gateway's address.
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
        "CHELABASE_AUTH_ADDR={listen} is reachable without the gateway. Bind to loopback, or \
         set CHELABASE_AUTH_TRUSTED_PROXY to the proxy that appends X-Forwarded-For (its host \
         name or address, e.g. gateway): only its forwarded header is believed."
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
        let cookie_key_file = var("CHELABASE_AUTH_COOKIE_KEY")?;
        warn_if_open(&cookie_key_file);
        let cookie_key = SecretKey::from_file(std::path::Path::new(&cookie_key_file))
            .map_err(|e| anyhow::anyhow!("cookie key: {e}"))?;
        let entry = chelabase_types::entry::Entry::from_env().map_err(anyhow::Error::new)?;
        entry.check_mode(mode).map_err(anyhow::Error::new)?;
        let secure_cookies = secure_cookies(
            entry.scheme(),
            std::env::var("CHELABASE_AUTH_SECURE_COOKIES")
                .ok()
                .as_deref(),
        );
        if entry.scheme() == Scheme::Https && !secure_cookies {
            tracing::warn!(
                "CHELABASE_AUTH_SECURE_COOKIES turns Secure off although the entry is https; \
                 session cookies may then travel over plain http"
            );
        }
        let min_ms: u64 = std::env::var("CHELABASE_AUTH_MIN_RESPONSE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_MIN_RESPONSE_MS);
        let listen =
            std::env::var("CHELABASE_AUTH_ADDR").unwrap_or_else(|_| "127.0.0.1:8084".into());
        let trusted_proxies = crate::guard::TrustedProxies::parse(
            &std::env::var("CHELABASE_AUTH_TRUSTED_PROXY").unwrap_or_default(),
        )
        .map_err(anyhow::Error::msg)?;
        check_listen(&listen, !trusted_proxies.is_empty())?;
        Ok(Self {
            listen,
            issuer: entry.issuer(),
            cookie_key,
            studio_url: entry.studio_origin(),
            studio: StudioSettings {
                api_url: entry.api_base(),
                studio_host: entry.studio_host().to_string(),
            },
            skip_email_verification: !requires_email_verification(
                mode,
                std::env::var("CHELABASE_REQUIRE_EMAIL_VERIFICATION")
                    .ok()
                    .as_deref(),
            ),
            mode,
            secure_cookies,
            min_response: Duration::from_millis(min_ms),
            trusted_proxies,
        })
    }
}

/// Settings for app users' flows (`/auth/v1/app/*`). Without the app key pair, the identity
/// verify key or the compute CA, those routes answer 503 and the platform flows run as before.
#[derive(Clone, Debug, PartialEq)]
pub struct AppConfig {
    pub private_key: PathBuf,
    pub public_key: PathBuf,
    pub access_ttl: Duration,
    pub internal_url: String,
    pub internal_token: String,
    /// The gateway's identity verify key (tokens with `aud = "auth"`).
    pub verify_key: PathBuf,
    /// The platform CA's certificate (`CHELABASE_COMPUTE_CA_FILE`): the app-users sync verifies
    /// every compute's TLS against it (U15).
    pub compute_ca: PathBuf,
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
    pub fn internal_from_env() -> Result<(String, String)> {
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
        let token = chelabase_types::secret_env::env_secret("CHELABASE_APP_AUTH_INTERNAL_TOKEN")
            .map_err(anyhow::Error::msg)?
            .unwrap_or_else(|| {
                tracing::warn!(
                    "CHELABASE_APP_AUTH_INTERNAL_TOKEN(_FILE) is not set; the sign-up gate uses the development token"
                );
                DEFAULT_APP_AUTH_INTERNAL_TOKEN.into()
            });
        Ok((url, token))
    }

    /// M9: a platform instance doesn't start with the development bearer (the variable unset);
    /// m5: nor does a self-hosted one, unless `CHELABASE_DEV_DEFAULTS=1` (the workbench).
    pub fn check_internal_token(mode: Mode, dev: DevDefaults, token: &str) -> Result<()> {
        dev.check_bearer(
            &ModePolicy::new(mode),
            "CHELABASE_APP_AUTH_INTERNAL_TOKEN",
            token,
            &[DEFAULT_APP_AUTH_INTERNAL_TOKEN],
        )
        .map_err(anyhow::Error::msg)
    }

    pub fn from_env() -> Result<Option<Self>> {
        Self::from_vars(|name| std::env::var(name).ok())
    }

    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        let internal_token =
            chelabase_types::secret_env::lookup_secret(&get, "CHELABASE_APP_AUTH_INTERNAL_TOKEN")
                .map_err(anyhow::Error::msg)?;
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
        let compute_ca = get("CHELABASE_COMPUTE_CA_FILE");
        if compute_ca.is_none() {
            missing.push("CHELABASE_COMPUTE_CA_FILE");
        }
        let (Some(verify_key), Some(compute_ca)) = (verify_key, compute_ca)
        else {
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
            internal_token: internal_token
                .unwrap_or_else(|| DEFAULT_APP_AUTH_INTERNAL_TOKEN.into()),
            verify_key: verify_key.into(),
            compute_ca: compute_ca.into(),
        }))
    }
}

/// #119: auth's member-token keys (`auth/member-jwt.{key,pub}`) and lifetime.
#[derive(Debug, Clone)]
pub struct MemberTokenConfig {
    /// `CHELABASE_MEMBER_JWT_PRIVATE_KEY`: the PKCS#8 signing key (auth only).
    pub private_key: PathBuf,
    /// `CHELABASE_MEMBER_JWT_PUBLIC_KEY`: its public half (gives the tokens' `kid`).
    pub public_key: PathBuf,
    /// `CHELABASE_MEMBER_ACCESS_TTL_SECS`: 300 unless set, from 60 to 900.
    pub ttl: Duration,
}

pub const DEFAULT_MEMBER_ACCESS_TTL_SECS: u64 = chelabase_auth::member_token::MEMBER_DEFAULT_TTL_SECS;

impl MemberTokenConfig {
    pub fn from_env() -> Result<Option<Self>> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Both keys or neither (`None`: auth signs no member tokens); one alone refuses to start.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<Self>> {
        const PRIVATE: &str = "CHELABASE_MEMBER_JWT_PRIVATE_KEY";
        const PUBLIC: &str = "CHELABASE_MEMBER_JWT_PUBLIC_KEY";
        const TTL: &str = "CHELABASE_MEMBER_ACCESS_TTL_SECS";
        let get = |name: &str| get(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let (private_key, public_key) = match (get(PRIVATE), get(PUBLIC)) {
            (None, None) => return Ok(None),
            (Some(k), Some(p)) => (PathBuf::from(k), PathBuf::from(p)),
            (Some(_), None) => anyhow::bail!("{PRIVATE} is set without {PUBLIC}"),
            (None, Some(_)) => anyhow::bail!("{PUBLIC} is set without {PRIVATE}"),
        };
        let max = chelabase_auth::member_token::MEMBER_MAX_TTL_SECS;
        let ttl = match get(TTL) {
            None => DEFAULT_MEMBER_ACCESS_TTL_SECS,
            Some(v) => v
                .parse::<u64>()
                .ok()
                .filter(|n| (60..=max).contains(n))
                .with_context(|| format!("{TTL}={v}: expected seconds from 60 to {max}"))?,
        };
        Ok(Some(Self { private_key, public_key, ttl: Duration::from_secs(ttl) }))
    }
}

/// #119: auth's identity-service client, from `CHELABASE_IDENTITY_URL` and
/// `CHELABASE_IDENTITY_TOKEN_FILE` (`tokens/auth-identity.token`): both or neither (`None`);
/// one alone, or an unreadable or empty token file, refuses to start.
pub fn identity_from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Option<chelabase_identity::Identity>> {
    const URL: &str = "CHELABASE_IDENTITY_URL";
    const TOKEN: &str = "CHELABASE_IDENTITY_TOKEN_FILE";
    let get = |name: &str| get(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    match (get(URL), get(TOKEN)) {
        (None, None) => Ok(None),
        (Some(url), Some(file)) => chelabase_identity::Identity::from_token_file(&url, std::path::Path::new(&file))
            .map(Some)
            .map_err(|e| anyhow::anyhow!("{TOKEN}: {e}")),
        (Some(_), None) => anyhow::bail!("{URL} is set without {TOKEN}"),
        (None, Some(_)) => anyhow::bail!("{TOKEN} is set without {URL}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member_vars(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let pairs: Vec<(&'static str, &'static str)> = pairs.to_vec();
        move |name: &str| pairs.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
    }

    // #119: the member-token pair is both or neither; the TTL is 300 s unless set, 60..=900.
    #[test]
    fn member_token_config_is_both_keys_and_a_bounded_ttl() {
        assert!(MemberTokenConfig::from_lookup(member_vars(&[])).unwrap().is_none());
        let keys = [
            ("CHELABASE_MEMBER_JWT_PRIVATE_KEY", "/s/auth/member-jwt.key"),
            ("CHELABASE_MEMBER_JWT_PUBLIC_KEY", "/s/auth/member-jwt.pub"),
        ];
        let c = MemberTokenConfig::from_lookup(member_vars(&keys)).unwrap().unwrap();
        assert_eq!(c.private_key, PathBuf::from("/s/auth/member-jwt.key"));
        assert_eq!(c.public_key, PathBuf::from("/s/auth/member-jwt.pub"));
        assert_eq!(c.ttl, Duration::from_secs(300));
        for (one, missing) in [(keys[0], keys[1].0), (keys[1], keys[0].0)] {
            let err = MemberTokenConfig::from_lookup(member_vars(&[one])).unwrap_err().to_string();
            assert!(err.contains(missing), "{err}");
        }
        for (ttl, ok) in [("60", true), ("900", true), ("901", false), ("59", false), ("soon", false)] {
            let mut v = keys.to_vec();
            v.push(("CHELABASE_MEMBER_ACCESS_TTL_SECS", ttl));
            let got = MemberTokenConfig::from_lookup(member_vars(&v));
            assert_eq!(got.is_ok(), ok, "{ttl}");
            if ok {
                assert_eq!(got.unwrap().unwrap().ttl.as_secs(), ttl.parse::<u64>().unwrap());
            }
        }
    }

    // #119: auth's identity client is the URL and its token file, both or neither.
    #[test]
    fn the_identity_client_needs_its_url_and_token_file() {
        assert!(identity_from_lookup(member_vars(&[])).unwrap().is_none());
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("auth-identity.token");
        std::fs::write(&file, "tok\n").unwrap();
        let path: &'static str = Box::leak(file.to_str().unwrap().to_string().into_boxed_str());
        let both = [("CHELABASE_IDENTITY_URL", "http://identity:8080"), ("CHELABASE_IDENTITY_TOKEN_FILE", path)];
        assert!(identity_from_lookup(member_vars(&both)).unwrap().is_some());
        for (one, missing) in [(both[0], both[1].0), (both[1], both[0].0)] {
            let err = identity_from_lookup(member_vars(&[one])).unwrap_err().to_string();
            assert!(err.contains(missing), "{err}");
        }
        let gone = [("CHELABASE_IDENTITY_URL", "http://identity:8080"), ("CHELABASE_IDENTITY_TOKEN_FILE", "/nonexistent/t")];
        assert!(identity_from_lookup(vars(&gone)).is_err());
    }

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
        ("CHELABASE_COMPUTE_CA_FILE", "/k/compute-ca/ca.crt"),
    ];

    // M9: a platform instance refuses the development internal bearer, whatever
    // CHELABASE_DEV_DEFAULTS says.
    #[test]
    fn platform_refuses_the_default_internal_token() {
        for dev in [DevDefaults::ON, DevDefaults::OFF] {
            assert!(AppConfig::check_internal_token(Mode::Platform, dev, DEFAULT_APP_AUTH_INTERNAL_TOKEN).is_err());
            assert!(AppConfig::check_internal_token(Mode::Platform, dev, "a-long-random-secret").is_ok());
        }
        let c = AppConfig::from_vars(vars(&FULL)).unwrap().unwrap();
        assert!(
            AppConfig::check_internal_token(Mode::Platform, DevDefaults::ON, &c.internal_token).is_err(),
            "unset means the default"
        );
    }

    // m5 (AC 4): self-hosted refuses the development bearer (unset or set to it) unless
    // CHELABASE_DEV_DEFAULTS=1; the message names the variable, never the token.
    #[test]
    fn dev_bearer_refused_without_dev_defaults() {
        let err = AppConfig::check_internal_token(Mode::SelfHosted, DevDefaults::OFF, DEFAULT_APP_AUTH_INTERNAL_TOKEN)
            .unwrap_err()
            .to_string();
        assert!(err.contains("CHELABASE_APP_AUTH_INTERNAL_TOKEN"), "{err}");
        assert!(!err.contains(DEFAULT_APP_AUTH_INTERNAL_TOKEN), "{err}");
        let unset = AppConfig::from_vars(vars(&FULL)).unwrap().unwrap();
        assert!(AppConfig::check_internal_token(Mode::SelfHosted, DevDefaults::OFF, &unset.internal_token).is_err());
        assert!(AppConfig::check_internal_token(Mode::SelfHosted, DevDefaults::ON, DEFAULT_APP_AUTH_INTERNAL_TOKEN).is_ok());
        assert!(AppConfig::check_internal_token(Mode::SelfHosted, DevDefaults::OFF, "a-long-random-secret").is_ok());
    }

    #[test]
    fn app_config_defaults() {
        let c = AppConfig::from_vars(vars(&FULL)).unwrap().unwrap();
        assert_eq!(c.access_ttl, Duration::from_secs(900));
        assert_eq!(c.internal_url, "http://127.0.0.1:8083");
        assert_eq!(c.internal_token, "chelabase-dev-app-auth");
        let mut more = FULL.to_vec();
        more.push(("CHELABASE_APP_ACCESS_TTL_SECS", "300"));
        more.push(("CHELABASE_INTERNAL_URL", "http://10.0.0.1:9000"));
        let c = AppConfig::from_vars(vars(&more)).unwrap().unwrap();
        assert_eq!(c.access_ttl, Duration::from_secs(300));
        assert_eq!(c.internal_url, "http://10.0.0.1:9000");
    }

    #[test]
    fn app_config_is_off_without_keys_verify_key_or_compute_ca() {
        assert_eq!(AppConfig::from_vars(vars(&[])).unwrap(), None);
        for (name, _) in FULL {
            let some: Vec<_> = FULL.iter().filter(|(n, _)| *n != name).copied().collect();
            assert_eq!(AppConfig::from_vars(vars(&some)).unwrap(), None, "without {name}");
        }
        let mut bad = FULL.to_vec();
        bad.push(("CHELABASE_APP_ACCESS_TTL_SECS", "soon"));
        assert!(AppConfig::from_vars(vars(&bad)).is_err());
    }

    // End-of-step review (user, 2026-10-13): Secure follows the public scheme in every mode;
    // CHELABASE_AUTH_SECURE_COOKIES still overrides.
    #[test]
    fn cookies_are_secure_when_the_entry_is_https_unless_overridden() {
        use chelabase_types::entry::Scheme;
        assert!(secure_cookies(Scheme::Https, None));
        assert!(!secure_cookies(Scheme::Http, None));
        assert!(secure_cookies(Scheme::Http, Some("true")));
        assert!(!secure_cookies(Scheme::Https, Some("false")));
        assert!(secure_cookies(Scheme::Https, Some(" ")));
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
