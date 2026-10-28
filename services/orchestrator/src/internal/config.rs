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
    /// `CHELABASE_REALTIME_LOGIN_CIDRS`: where `chelabase_realtime` logins come from, read raw
    /// like [`Self::member_login_cidrs`] ([`Self::realtime_logins`] refuses to start when unset).
    pub realtime_login_cidrs: Option<Vec<String>>,
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
    /// `CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY`, or the file `CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY_FILE`
    /// names (the bootstrap's `actions-signing-key`, 0600), read when the webhook first needs
    /// it: checks the Actions webhook's signature. Unset, or the file not written yet, the
    /// webhook answers 503.
    pub actions_signing_key: Option<chelabase_types::file_value::FileValue>,
    /// `CHELABASE_DEV_DEFAULTS=1` (the workbench only): a self-hosted instance may start with
    /// the development bearers ([`Self::check_mode`]).
    pub dev_defaults: chelabase_types::dev_defaults::DevDefaults,
    /// `CHELABASE_ZITADEL_ADMIN_PAT_FILE`: the admin PAT that deletes Zitadel sessions.
    pub zitadel_admin_pat_file: Option<PathBuf>,
    /// Zitadel's base URL, from `CHELABASE_AUTH_HOST` (and `CHELABASE_PUBLIC_SCHEME`).
    pub zitadel_issuer: Option<String>,
}

/// Every bearer the internal listener checks, with its development default (public: a platform
/// instance refuses each, [`InternalConfig::check_mode`]).
pub const DEV_BEARERS: [(&str, &str); 7] = chelabase_types::dev_defaults::DEV_BEARERS;

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
            realtime_login_cidrs: get("CHELABASE_REALTIME_LOGIN_CIDRS")
                .map(|raw| chelabase_neon::spec::parse_realtime_login_cidrs(Some(&raw)))
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
            actions_signing_key: chelabase_types::file_value::FileValue::from_lookup(
                &get,
                "CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY",
                chelabase_types::file_value::Sensitivity::Secret,
            )
            .map_err(|e| anyhow!(e))?,
            dev_defaults: chelabase_types::dev_defaults::DevDefaults::from_lookup(&get)
                .map_err(|e| anyhow!(e))?,
            zitadel_admin_pat_file: get("CHELABASE_ZITADEL_ADMIN_PAT_FILE")
                .filter(|v| !v.trim().is_empty())
                .map(PathBuf::from),
            zitadel_issuer: chelabase_types::entry::issuer_from_lookup(&get)
                .map_err(anyhow::Error::new)?,
        })
    }

    /// Startup refusals that depend on the mode (M9): the internal listener runs with none of
    /// its development bearers (each is public), whether its variable is unset or set to one of
    /// them: never in platform mode, and in self-hosted mode only with `CHELABASE_DEV_DEFAULTS=1`
    /// (the workbench, m5). The error names the variable, never the token.
    pub fn check_mode(&self, policy: &chelabase_types::mode_policy::ModePolicy) -> Result<()> {
        let dev_values: Vec<&str> = DEV_BEARERS.iter().map(|(_, value)| *value).collect();
        for (var, token) in self.bearers() {
            self.dev_defaults
                .check_bearer(policy, var, token, &dev_values)
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

    /// What every compute's spec says about realtime logins: the configured CIDRs and realtime's
    /// certificate CN. Unset `CHELABASE_REALTIME_LOGIN_CIDRS` is the startup error.
    pub fn realtime_logins(&self) -> Result<chelabase_neon::spec::MemberLogins> {
        match &self.realtime_login_cidrs {
            Some(cidrs) => Ok(chelabase_neon::spec::MemberLogins::realtime(cidrs.clone())),
            None => chelabase_neon::spec::parse_realtime_login_cidrs(None)
                .map(chelabase_neon::spec::MemberLogins::realtime),
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
                &self.actions_signing_key.as_ref().map(|k| k.var().to_string()),
            )
            .field("dev_defaults", &self.dev_defaults)
            .field("zitadel_admin_pat_file", &self.zitadel_admin_pat_file)
            .field("zitadel_issuer", &self.zitadel_issuer)
            .finish()
    }
}

/// `CHELABASE_METADATA_DATABASE_URL`: where the metadata database is, with its password from
/// `CHELABASE_METADATA_DATABASE_PASSWORD_FILE` when that is set (m1's `db/metadata.password`;
/// the URL then names the user only). Required: there is no development fallback (the
/// workbench's stack.env always sets it), so a fixed credential is never used by accident.
/// Errors name the variables, never the URL or the password.
pub fn metadata_config(
    get: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<chelabase_metadata::MetadataConfig> {
    let database_url = chelabase_types::secret_env::url_with_password_file(
        get,
        "CHELABASE_METADATA_DATABASE_URL",
        "CHELABASE_METADATA_DATABASE_PASSWORD_FILE",
    )
    .map_err(anyhow::Error::msg)?
    .ok_or_else(|| {
        anyhow::anyhow!(
            "CHELABASE_METADATA_DATABASE_URL is not set: the metadata database's URL \
             (postgres://user@host:port/database, the password in \
             CHELABASE_METADATA_DATABASE_PASSWORD_FILE)"
        )
    })?;
    Ok(chelabase_metadata::MetadataConfig { database_url })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_neon::jwt::{PRIVATE_KEY_FILE, PUBLIC_KEY_FILE};

    // m5 fix round: no fixed development credential outside the workbench. An unset (or blank)
    // CHELABASE_METADATA_DATABASE_URL is a startup error naming it, never a fallback to
    // `postgres://chelabase:chelabase@…`; the workbench always sets it (its stack.env).
    // m5 fix round: the metadata password from CHELABASE_METADATA_DATABASE_PASSWORD_FILE (m1's
    // `db/metadata.password`), so it is never in the environment. It goes into the URL's user
    // info, percent-encoded (as the bootstrap does); the URL then carries none. Both forms, a
    // missing, empty, symlinked or loosely-moded file are startup errors naming the variables,
    // and the password is in no error and no `Debug`.
    #[test]
    fn the_metadata_password_comes_from_its_file() {
        use std::os::unix::fs::PermissionsExt;
        use std::str::FromStr;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("metadata.password");
        let write = |contents: &str, mode: u32| {
            let _ = std::fs::remove_file(&file);
            std::fs::write(&file, contents).unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
        };
        const SECRET: &str = "p@ss/w:rd%Zz";
        let path = file.to_str().unwrap().to_string();
        let config = |url: &str| {
            let (url, path) = (url.to_string(), path.clone());
            metadata_config(move |k| match k {
                "CHELABASE_METADATA_DATABASE_URL" => Some(url.clone()),
                "CHELABASE_METADATA_DATABASE_PASSWORD_FILE" => Some(path.clone()),
                _ => None,
            })
        };
        const URL: &str = "postgres://chelabase@metadata-db:5432/chelabase_metadata?sslmode=disable";
        for mode in [0o600, 0o400, 0o640] {
            write(&format!("{SECRET}\n"), mode);
            let c = config(URL).unwrap_or_else(|e| panic!("{mode:o}: {e:#}"));
            assert_eq!(
                c.database_url,
                "postgres://chelabase:p%40ss%2Fw%3Ard%25Zz@metadata-db:5432/chelabase_metadata?sslmode=disable"
            );
            // What sqlx connects with: the same user, host and database.
            let opts = sqlx::postgres::PgConnectOptions::from_str(&c.database_url).unwrap();
            assert_eq!((opts.get_username(), opts.get_host(), opts.get_database()),
                ("chelabase", "metadata-db", Some("chelabase_metadata")));
            assert!(!format!("{c:?}").contains("p%40ss") && !format!("{c:?}").contains(SECRET), "{c:?}");
        }
        let refused = |what: &str, url: &str, needles: &[&str]| {
            let err = match config(url) {
                Err(e) => format!("{e:#}"),
                Ok(c) => panic!("{what}: accepted: {c:?}"),
            };
            for n in needles {
                assert!(err.contains(n), "{what}: {err}");
            }
            assert!(!err.contains(SECRET) && !err.contains("p%40ss") && !err.contains("in-url-pw"), "{what}: {err}");
        };
        write(SECRET, 0o600);
        refused(
            "a password in both",
            "postgres://chelabase:in-url-pw@metadata-db:5432/m",
            &["CHELABASE_METADATA_DATABASE_URL", "CHELABASE_METADATA_DATABASE_PASSWORD_FILE"],
        );
        refused("no user", "postgres://metadata-db:5432/m", &["CHELABASE_METADATA_DATABASE_URL"]);
        for mode in [0o644, 0o604, 0o660, 0o666] {
            write(SECRET, mode);
            refused(&format!("mode {mode:o}"), URL, &["CHELABASE_METADATA_DATABASE_PASSWORD_FILE"]);
        }
        write("\n", 0o600);
        refused("empty", URL, &["CHELABASE_METADATA_DATABASE_PASSWORD_FILE"]);
        std::fs::remove_file(&file).unwrap();
        refused("missing", URL, &["CHELABASE_METADATA_DATABASE_PASSWORD_FILE"]);
        let real = dir.path().join("real");
        std::fs::write(&real, SECRET).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&real, &file).unwrap();
        refused("a symlink", URL, &["CHELABASE_METADATA_DATABASE_PASSWORD_FILE", "symlink"]);
        // A URL with its own password, and no file, still works (the workbench).
        let c = metadata_config(|k| {
            (k == "CHELABASE_METADATA_DATABASE_URL").then(|| "postgres://u:in-url-pw@db:5432/m".to_string())
        })
        .unwrap();
        assert!(!format!("{c:?}").contains("in-url-pw"), "{c:?}");
    }

    #[test]
    fn the_metadata_url_is_required() {
        for pairs in [vec![], vec![("CHELABASE_METADATA_DATABASE_URL", " ")], vec![("CHELABASE_DEV_DEFAULTS", "1")]] {
            let map: std::collections::HashMap<&str, &str> = pairs.iter().copied().collect();
            let err = match metadata_config(|k| map.get(k).map(|v| v.to_string())) {
                Err(e) => format!("{e:#}"),
                Ok(c) => panic!("{pairs:?}: a fallback metadata URL: {}", c.database_url),
            };
            assert!(err.contains("CHELABASE_METADATA_DATABASE_URL"), "{err}");
        }
        let c = metadata_config(|k| {
            (k == "CHELABASE_METADATA_DATABASE_URL").then(|| " postgres://u:p@db:5432/m ".to_string())
        })
        .unwrap();
        assert_eq!(c.database_url, "postgres://u:p@db:5432/m");
    }

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
        assert!(c.actions_signing_key.is_none());
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
        // m5: self-hosted keeps the defaults only with CHELABASE_DEV_DEFAULTS=1.
        let dev = InternalConfig::from_lookup(|name| {
            (name == "CHELABASE_DEV_DEFAULTS").then(|| "1".to_string())
        })
        .unwrap();
        assert!(dev.check_mode(&sh).is_ok());
        assert!(dev.check_mode(&pf).is_err());
        assert!(default.check_mode(&sh).is_err());
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
        assert_eq!(c.actions_signing_key.as_ref().map(|k| k.get().unwrap()), Some("sign-key"));
        assert_eq!(
            c.zitadel_admin_pat_file,
            Some(PathBuf::from(".chelabase/stacks/main/zitadel/admin.pat"))
        );
        assert_eq!(c.zitadel_issuer.as_deref(), Some("http://auth.chelabase.localhost:8000"));
        let blank = InternalConfig::from_lookup(|name| {
            (name == "CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY").then(|| "  ".to_string())
        })
        .unwrap();
        assert!(blank.actions_signing_key.is_none());
    }

    // m5 (AC 3): the Actions signing key is the variable or the file it names, not both; the
    // error names the variable and never the key.
    #[test]
    fn config_refuses_env_and_file_both() {
        let err = InternalConfig::from_lookup(|name| match name {
            "CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY" => Some("sign-key-secret".into()),
            "CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY_FILE" => Some("/out/actions-signing-key".into()),
            _ => None,
        })
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY ") && msg.contains("not both"), "{msg}");
        assert!(!msg.contains("sign-key-secret"), "{msg}");
    }

    // m5: the key file is read when the webhook first needs it, not at startup: a missing file
    // starts the orchestrator (its readiness doesn't wait for the key) and is "not yet".
    #[test]
    fn the_actions_key_file_is_read_late() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("actions-signing-key");
        let p = path.to_str().unwrap().to_string();
        let c = InternalConfig::from_lookup(move |name| {
            (name == "CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY_FILE").then(|| p.clone())
        })
        .unwrap();
        let key = c.actions_signing_key.clone().expect("configured");
        assert!(key.get().is_err(), "not written yet");
        std::fs::write(&path, "late-key\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(key.get(), Ok("late-key"));
        assert!(!format!("{c:?}").contains("late-key"));
    }

    // m5 (AC 4): self-hosted without CHELABASE_DEV_DEFAULTS=1 refuses every bearer that is
    // unset or set to a development default, naming the variable; with it, today's behaviour.
    #[test]
    fn dev_bearer_refused_without_dev_defaults() {
        use chelabase_types::mode::Mode;
        use chelabase_types::mode_policy::ModePolicy;
        let sh = ModePolicy::new(Mode::SelfHosted);
        for (var, dev) in DEV_BEARERS {
            for (how, value) in [("unset", None), ("set to the default", Some(dev))] {
                let lookup = |dev_defaults: Option<&'static str>| {
                    let value = value.map(str::to_string);
                    move |name: &str| {
                        if name == "CHELABASE_DEV_DEFAULTS" {
                            return dev_defaults.map(str::to_string);
                        }
                        if name == var {
                            return value.clone();
                        }
                        DEV_BEARERS
                            .iter()
                            .any(|(v, _)| *v == name)
                            .then(|| format!("generated-secret-for-{name}"))
                    }
                };
                let c = InternalConfig::from_lookup(lookup(None)).unwrap();
                let msg = format!("{:#}", c.check_mode(&sh).expect_err(var));
                assert!(msg.contains(var), "{var} {how}: {msg}");
                assert!(!msg.contains(dev), "{var} {how}: the message repeats the token: {msg}");
                let c = InternalConfig::from_lookup(lookup(Some("0"))).unwrap();
                assert!(c.check_mode(&sh).is_err(), "{var} {how}: 0 is off");
                let c = InternalConfig::from_lookup(lookup(Some("1"))).unwrap();
                assert!(c.check_mode(&sh).is_ok(), "{var} {how}: the workbench keeps it");
            }
        }
        // Every bearer generated: no switch needed.
        let own = InternalConfig::from_lookup(|name| {
            DEV_BEARERS.iter().any(|(v, _)| *v == name).then(|| format!("generated-{name}"))
        })
        .unwrap();
        assert!(own.check_mode(&sh).is_ok());
        let typo = InternalConfig::from_lookup(|name| {
            (name == "CHELABASE_DEV_DEFAULTS").then(|| "true".to_string())
        });
        assert!(format!("{:#}", typo.unwrap_err()).contains("CHELABASE_DEV_DEFAULTS"));
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
