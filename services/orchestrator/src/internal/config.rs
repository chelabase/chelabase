//! Settings for the internal listener, all with local-dev defaults.

use anyhow::{Context, Result, anyhow};
use cheladb_neon::ComputeSigner;
use std::net::SocketAddr;
use std::path::PathBuf;

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
    /// `CHELADB_CONTROL_PLANE_URI`: how computes reach this listener; the DDL receiver's URL
    /// (`neon.console_url`) is built from it.
    pub control_plane_uri: String,
    /// `CHELADB_VALKEY_URL`: where the receiver tells the proxies a role's secret changed.
    pub valkey_url: String,
}

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
            addr: or("CHELADB_INTERNAL_ADDR", "127.0.0.1:8083")
                .parse()
                .context("CHELADB_INTERNAL_ADDR is not a socket address")?,
            proxy_token: or("CHELADB_PROXY_API_TOKEN", "cheladb-dev-proxy-api"),
            storcon_token: or("CHELADB_STORCON_HOOK_TOKEN", "cheladb-dev-storcon-hook"),
            data_keys_token: or("CHELADB_DATA_KEYS_TOKEN", "cheladb-dev-data-keys"),
            rest_token: or("CHELADB_REST_INTERNAL_TOKEN", "cheladb-dev-rest-internal"),
            storcon_url: or("CHELADB_STORCON_URL", "http://127.0.0.1:1234"),
            compute_jwt_dir: PathBuf::from(or("CHELADB_COMPUTE_JWT_DIR", ".cheladb/neon")),
            compute_network: or("CHELADB_COMPUTE_NETWORK", "cheladb_default"),
            control_plane_uri: check_control_plane_uri(or(
                "CHELADB_CONTROL_PLANE_URI",
                "http://host.docker.internal:8083",
            ))?,
            valkey_url: or("CHELADB_VALKEY_URL", "redis://127.0.0.1:6379"),
        })
    }

    /// The key pair `cheladb dev bootstrap` writes.
    pub fn load_signer(&self) -> Result<ComputeSigner> {
        cheladb_neon::jwt::load_signer(&self.compute_jwt_dir)
    }
}

/// Computes call this address from inside their containers, so it needs a scheme, a host and
/// an explicit port (the worker's check).
fn check_control_plane_uri(uri: String) -> Result<String> {
    let bad = |why: &str| {
        anyhow!(
            "CHELADB_CONTROL_PLANE_URI={uri:?}: {why} (for example http://host.docker.internal:8083)"
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
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cheladb_neon::jwt::{PRIVATE_KEY_FILE, PUBLIC_KEY_FILE};

    #[test]
    fn defaults_are_for_local_dev() {
        let c = InternalConfig::from_lookup(|_| None).unwrap();
        assert_eq!(c.addr, "127.0.0.1:8083".parse().unwrap());
        assert_eq!(c.proxy_token, "cheladb-dev-proxy-api");
        assert_eq!(c.storcon_token, "cheladb-dev-storcon-hook");
        assert_eq!(c.data_keys_token, "cheladb-dev-data-keys");
        assert_eq!(c.rest_token, "cheladb-dev-rest-internal");
        assert_eq!(c.storcon_url, "http://127.0.0.1:1234");
        assert_eq!(c.compute_jwt_dir, PathBuf::from(".cheladb/neon"));
        assert_eq!(c.compute_network, "cheladb_default");
    }

    #[test]
    fn set_values_win_and_blank_ones_do_not() {
        let c = InternalConfig::from_lookup(|name| match name {
            "CHELADB_INTERNAL_ADDR" => Some("127.0.0.1:9083".into()),
            "CHELADB_PROXY_API_TOKEN" => Some("  ".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(c.addr.port(), 9083);
        assert_eq!(c.proxy_token, "cheladb-dev-proxy-api");
    }

    #[test]
    fn a_bad_address_names_the_variable() {
        let err = InternalConfig::from_lookup(|name| {
            (name == "CHELADB_INTERNAL_ADDR").then(|| "nope".to_string())
        })
        .unwrap_err()
        .to_string();
        assert!(err.contains("CHELADB_INTERNAL_ADDR"), "{err}");
    }

    #[test]
    fn config_reads_control_plane_uri_and_valkey() {
        let d = InternalConfig::from_lookup(|_| None).unwrap();
        assert_eq!(d.control_plane_uri, "http://host.docker.internal:8083");
        assert_eq!(d.valkey_url, "redis://127.0.0.1:6379");
        let c = InternalConfig::from_lookup(|name| match name {
            "CHELADB_CONTROL_PLANE_URI" => Some("http://172.17.0.1:9083".into()),
            "CHELADB_VALKEY_URL" => Some("redis://valkey:6379".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(c.control_plane_uri, "http://172.17.0.1:9083");
        assert_eq!(c.valkey_url, "redis://valkey:6379");
        let blank = InternalConfig::from_lookup(|name| {
            (name == "CHELADB_VALKEY_URL").then(|| " ".to_string())
        })
        .unwrap();
        assert_eq!(blank.valkey_url, "redis://127.0.0.1:6379");
        for bad in ["host.docker.internal:8083", "http://h", "ftp://h:1"] {
            let err = InternalConfig::from_lookup(|name| {
                (name == "CHELADB_CONTROL_PLANE_URI").then(|| bad.to_string())
            })
            .unwrap_err()
            .to_string();
            assert!(err.contains("CHELADB_CONTROL_PLANE_URI"), "{bad}: {err}");
        }
    }

    #[test]
    fn debug_hides_the_tokens() {
        let c = InternalConfig::from_lookup(|_| None).unwrap();
        let shown = format!("{c:?}");
        assert!(!shown.contains("cheladb-dev-proxy-api"), "{shown}");
        assert!(!shown.contains("cheladb-dev-storcon-hook"), "{shown}");
        assert!(!shown.contains("cheladb-dev-data-keys"), "{shown}");
        assert!(!shown.contains("cheladb-dev-rest-internal"), "{shown}");
        assert!(shown.contains("<redacted>"), "{shown}");
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
        let (pem, x) = cheladb_neon::jwt::generate_keypair();
        std::fs::write(dir.path().join(PRIVATE_KEY_FILE), pem).unwrap();
        std::fs::write(dir.path().join(PUBLIC_KEY_FILE), format!("{x}\n")).unwrap();
        let c = InternalConfig {
            compute_jwt_dir: dir.path().to_path_buf(),
            ..InternalConfig::from_lookup(|_| None).unwrap()
        };
        assert!(c.load_signer().is_ok());
    }
}
