//! The service's settings, from the environment (blank values count as unset).

use std::net::SocketAddr;
use std::path::PathBuf;

use chelabase_storage::StorageConfig;

/// Storage's bearer for the orchestrator's internal listener (its login's connection info,
/// and the data connection info its role sessions use).
pub const BEARER: &str = "CHELABASE_STORAGE_INTERNAL_TOKEN";

pub struct Config {
    /// `CHELABASE_STORAGE_ADDR` (default `127.0.0.1:8089`): the public listener, behind the
    /// gateway.
    pub addr: SocketAddr,
    /// `CHELABASE_INTERNAL_URL`: the orchestrator's internal listener, for connection info.
    pub internal_url: String,
    /// [`BEARER`], or the file `…_FILE` names.
    pub internal_token: String,
    /// `CHELABASE_STORAGE_MAX_CONNECTIONS` (default 200): connections across every branch.
    pub max_connections: usize,
    /// `CHELABASE_COMPUTE_CA_FILE` (required): every compute is verified against it (U15).
    pub compute_ca: PathBuf,
    /// `CHELABASE_STORAGE_MAX_INFLIGHT_BYTES` (default 1 GiB): upload buffers held at once in
    /// this process (a part is held whole; a single upload holds one stream part). Past it,
    /// 503 `busy`.
    pub max_inflight_bytes: usize,
    /// The object store (`CHELABASE_STORAGE_BACKEND`, `CHELABASE_STORAGE_S3_*`).
    pub store: StorageConfig,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let non_blank = |name: &str| {
            get(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        let env_or =
            |name: &str, default: &str| non_blank(name).unwrap_or_else(|| default.to_owned());
        let internal_token = chelabase_types::secret_env::lookup_secret(&get, BEARER)
            .map_err(anyhow::Error::msg)?
            .unwrap_or_else(|| chelabase_types::dev_defaults::dev_bearer(BEARER).to_owned());
        // The bearer opens the storage login's password: never a public development value,
        // except with `CHELABASE_DEV_DEFAULTS=1` (self-hosted mode only).
        let mode =
            chelabase_types::mode::Mode::parse(get(chelabase_types::mode::Mode::ENV).as_deref())
                .map_err(anyhow::Error::msg)?;
        chelabase_types::dev_defaults::DevDefaults::from_lookup(&get)
            .map_err(anyhow::Error::msg)?
            .check_bearers(
                &chelabase_types::mode_policy::ModePolicy::new(mode),
                &[(BEARER, internal_token.as_str())],
            )
            .map_err(anyhow::Error::msg)?;
        Ok(Config {
            addr: env_or("CHELABASE_STORAGE_ADDR", "127.0.0.1:8089").parse()?,
            internal_url: env_or("CHELABASE_INTERNAL_URL", "http://127.0.0.1:8083"),
            internal_token,
            max_connections: env_or("CHELABASE_STORAGE_MAX_CONNECTIONS", "200").parse()?,
            compute_ca: non_blank("CHELABASE_COMPUTE_CA_FILE")
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "CHELABASE_COMPUTE_CA_FILE is unset: storage verifies every compute's TLS \
                         against the platform CA certificate"
                    )
                })?
                .into(),
            max_inflight_bytes: env_or("CHELABASE_STORAGE_MAX_INFLIGHT_BYTES", "1073741824").parse()?,
            store: StorageConfig::from_lookup(&get)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        }
    }

    const BASE: &[(&str, &str)] = &[
        (
            "CHELABASE_STORAGE_INTERNAL_TOKEN",
            "a-generated-storage-token",
        ),
        ("CHELABASE_COMPUTE_CA_FILE", "/ca.crt"),
        // `s3` with the default credential chain: no key files to read in a unit test.
        ("CHELABASE_STORAGE_BACKEND", "s3"),
        ("CHELABASE_STORAGE_S3_REGION", "us-east-1"),
        ("CHELABASE_STORAGE_S3_BUCKET", "chelabase-storage"),
    ];

    #[test]
    fn reads_the_settings_with_their_defaults() {
        let c = Config::from_lookup(lookup(BASE)).unwrap();
        assert_eq!(c.addr, "127.0.0.1:8089".parse().unwrap());
        assert_eq!(c.internal_token, "a-generated-storage-token");
        assert_eq!(c.max_connections, 200);
        assert_eq!(c.max_inflight_bytes, 1 << 30);
        assert_eq!(c.store.bucket, "chelabase-storage");
    }

    /// The development bearer is refused without `CHELABASE_DEV_DEFAULTS=1`; the CA is required.
    #[test]
    fn refuses_the_dev_bearer_and_a_missing_ca() {
        let err = Config::from_lookup(lookup(&BASE[1..]))
            .err()
            .expect("the dev bearer");
        assert!(format!("{err:#}").contains(BEARER), "{err:#}");
        const TOKEN_ONLY: &[(&str, &str)] = &[BASE[0]];
        let err = Config::from_lookup(lookup(TOKEN_ONLY))
            .err()
            .expect("no CA");
        assert!(
            format!("{err:#}").contains("CHELABASE_COMPUTE_CA_FILE"),
            "{err:#}"
        );
    }
}
