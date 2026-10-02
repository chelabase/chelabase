//! The worker's environment, and wiring the real ports into a [`Worker`].

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, anyhow};

use cheladb_metadata::MetadataClient;
use cheladb_neon::proxy_cache::ProxyCache;
use cheladb_neon::{ComputeSettings, DockerBackend, StorconClient};
use cheladb_secrets::SecretKey;
use cheladb_types::mode::Mode;
use cheladb_types::role_settings::RoleSettings;

use crate::Worker;
use crate::ports::HttpPusher;

/// The worker over the real metadata database, storage controller, Docker, Valkey and
/// compute_ctl.
pub type RealWorker = Worker<MetadataClient, StorconClient, DockerBackend, ProxyCache, HttpPusher>;

/// No secrets in here, so `Debug` is safe.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub storcon_url: String,
    pub compute_jwt_dir: PathBuf,
    pub compute_network: String,
    pub control_plane_uri: String,
    pub valkey_url: String,
    pub compute_image: String,
    pub mode: cheladb_types::mode::Mode,
    pub owner_superuser_requested: bool,
    /// `CHELADB_BRANCH_ROLE_CAP`, `CHELADB_BRANCH_DATABASE_CAP` and
    /// `CHELADB_PASSWORD_STRENGTH` (platform mode forces strength on).
    pub roles: RoleSettings,
}

/// The value of `name`, or `None` when it's unset or blank.
fn non_blank(get: &impl Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    get(name)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

impl WorkerConfig {
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let or_default = |name: &str, default: &str| {
            non_blank(&get, name).unwrap_or_else(|| default.to_string())
        };
        let compute_image = non_blank(&get, "CHELADB_COMPUTE_IMAGE").ok_or_else(|| {
            anyhow!(
                "CHELADB_COMPUTE_IMAGE is not set; just dev and just worker set it from scripts/neon-images.sh"
            )
        })?;
        let mode = Mode::parse(non_blank(&get, Mode::ENV).as_deref()).map_err(|e| anyhow!(e))?;
        // Only an explicit true value counts.
        let owner_superuser_requested = matches!(
            non_blank(&get, "CHELADB_DB_OWNER_SUPERUSER")
                .map(|v| v.to_ascii_lowercase())
                .as_deref(),
            Some("true" | "1" | "yes")
        );
        let roles = RoleSettings::from_lookup(mode, &get).map_err(|e| anyhow!(e))?;
        let config = Self {
            storcon_url: or_default("CHELADB_STORCON_URL", "http://127.0.0.1:1234"),
            compute_jwt_dir: PathBuf::from(or_default("CHELADB_COMPUTE_JWT_DIR", ".cheladb/neon")),
            compute_network: or_default("CHELADB_COMPUTE_NETWORK", "cheladb_default"),
            control_plane_uri: or_default(
                "CHELADB_CONTROL_PLANE_URI",
                "http://host.docker.internal:8083",
            ),
            valkey_url: or_default("CHELADB_VALKEY_URL", "redis://127.0.0.1:6379"),
            compute_image,
            mode,
            owner_superuser_requested,
            roles,
        };
        check_control_plane_uri(&config.control_plane_uri)?;
        Ok(config)
    }

    /// Requested, and in self-hosted mode: platform mode ignores the switch.
    pub fn owner_superuser(&self) -> bool {
        self.owner_superuser_requested && self.mode == Mode::SelfHosted
    }
}

/// Computes call this address from inside their containers, so it needs a scheme, a host and
/// an explicit port.
pub fn check_control_plane_uri(uri: &str) -> anyhow::Result<()> {
    let bad = |why: &str| {
        anyhow!(
            "CHELADB_CONTROL_PLANE_URI={uri:?}: {why} (for example http://host.docker.internal:8083)"
        )
    };
    let url = reqwest::Url::parse(uri).map_err(|e| bad(&format!("not a URL ({e})")))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(bad("the scheme must be http or https"));
    }
    if url.host_str().is_none() {
        return Err(bad("it needs a host"));
    }
    if url.port().is_none() {
        return Err(bad("it needs an explicit port"));
    }
    Ok(())
}

/// Wires the real ports. Nothing is contacted: Docker, Valkey and the storage controller are
/// reached on first use.
pub fn build(
    config: &WorkerConfig,
    metadata: MetadataClient,
    key: SecretKey,
) -> anyhow::Result<RealWorker> {
    // First, so a missing key pair names `just zitadel-bootstrap` before anything else fails.
    let signer = cheladb_neon::jwt::load_signer(&config.compute_jwt_dir)?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("building the compute_ctl HTTP client")?;
    Ok(Worker {
        metadata,
        storage: StorconClient::new(&config.storcon_url, 0).with_timeout(Duration::from_secs(10)),
        computes: DockerBackend::connect(ComputeSettings {
            image: config.compute_image.clone(),
            network: config.compute_network.clone(),
            control_plane_uri: config.control_plane_uri.clone(),
        })?,
        cache: ProxyCache::new(&config.valkey_url)?,
        pusher: HttpPusher { http, signer },
        key,
        owner_superuser: config.owner_superuser(),
        roles: config.roles,
        control_plane_uri: config.control_plane_uri.clone(),
        ready_poll: Duration::from_millis(500),
        ready_timeout: Duration::from_secs(60),
        claim_refresh: Duration::from_secs(60),
        schema_check_cursor: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cheladb_metadata::MetadataConfig;
    use cheladb_types::mode::Mode;
    use cheladb_types::role_settings::RoleSettings;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    const IMAGE: (&str, &str) = ("CHELADB_COMPUTE_IMAGE", "cheladb/compute-node-v17:test");

    #[test]
    fn defaults_are_for_local_dev() {
        let c = WorkerConfig::from_lookup(lookup(&[IMAGE])).unwrap();
        assert_eq!(c.storcon_url, "http://127.0.0.1:1234");
        assert_eq!(c.compute_jwt_dir, PathBuf::from(".cheladb/neon"));
        assert_eq!(c.compute_network, "cheladb_default");
        assert_eq!(c.control_plane_uri, "http://host.docker.internal:8083");
        assert_eq!(c.valkey_url, "redis://127.0.0.1:6379");
        assert_eq!(c.compute_image, "cheladb/compute-node-v17:test");
        assert_eq!(c.mode, Mode::SelfHosted);
        assert!(!c.owner_superuser_requested);
        assert!(!c.owner_superuser());
    }

    #[test]
    fn a_missing_image_names_the_recipe() {
        let err = WorkerConfig::from_lookup(lookup(&[]))
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "CHELADB_COMPUTE_IMAGE is not set; just dev and just worker set it from scripts/neon-images.sh"
        );
    }

    #[test]
    fn blank_values_count_as_unset() {
        let err = WorkerConfig::from_lookup(lookup(&[("CHELADB_COMPUTE_IMAGE", "  ")]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("CHELADB_COMPUTE_IMAGE is not set"), "{err}");
        let c = WorkerConfig::from_lookup(lookup(&[
            IMAGE,
            ("CHELADB_STORCON_URL", ""),
            ("CHELADB_VALKEY_URL", "   "),
            ("CHELADB_MODE", ""),
        ]))
        .unwrap();
        assert_eq!(c.storcon_url, "http://127.0.0.1:1234");
        assert_eq!(c.valkey_url, "redis://127.0.0.1:6379");
        assert_eq!(c.mode, Mode::SelfHosted);
    }

    #[test]
    fn superuser_only_in_self_hosted_mode() {
        for (mode, flag, expected) in [
            ("self-hosted", "true", true),
            ("self-hosted", "false", false),
            ("platform", "true", false),
            ("platform", "false", false),
        ] {
            let c = WorkerConfig::from_lookup(lookup(&[
                IMAGE,
                ("CHELADB_MODE", mode),
                ("CHELADB_DB_OWNER_SUPERUSER", flag),
            ]))
            .unwrap();
            assert_eq!(c.owner_superuser_requested, flag == "true", "{mode} {flag}");
            assert_eq!(c.owner_superuser(), expected, "{mode} {flag}");
        }
        for on in ["1", "YES", " True "] {
            let c = WorkerConfig::from_lookup(lookup(&[IMAGE, ("CHELADB_DB_OWNER_SUPERUSER", on)]))
                .unwrap();
            assert!(c.owner_superuser(), "{on}");
        }
        let c = WorkerConfig::from_lookup(lookup(&[IMAGE, ("CHELADB_DB_OWNER_SUPERUSER", "nope")]))
            .unwrap();
        assert!(!c.owner_superuser());
        assert!(WorkerConfig::from_lookup(lookup(&[IMAGE, ("CHELADB_MODE", "cloud")])).is_err());
    }

    #[test]
    fn config_reads_role_settings() {
        let c = WorkerConfig::from_lookup(lookup(&[IMAGE])).unwrap();
        assert_eq!(
            c.roles,
            RoleSettings {
                role_cap: 50,
                database_cap: 20,
                strength: false,
                prehashed_allowed: true
            }
        );
        let c = WorkerConfig::from_lookup(lookup(&[
            IMAGE,
            ("CHELADB_BRANCH_ROLE_CAP", "7"),
            ("CHELADB_BRANCH_DATABASE_CAP", "3"),
            ("CHELADB_PASSWORD_STRENGTH", "on"),
        ]))
        .unwrap();
        assert_eq!(
            c.roles,
            RoleSettings {
                role_cap: 7,
                database_cap: 3,
                strength: true,
                prehashed_allowed: true
            }
        );
        let c = WorkerConfig::from_lookup(lookup(&[IMAGE, ("CHELADB_MODE", "platform")])).unwrap();
        assert!(c.roles.strength && !c.roles.prehashed_allowed);
        for (name, bad) in [
            ("CHELADB_BRANCH_ROLE_CAP", "501"),
            ("CHELADB_BRANCH_DATABASE_CAP", "0"),
            ("CHELADB_PASSWORD_STRENGTH", "maybe"),
        ] {
            let err = WorkerConfig::from_lookup(lookup(&[IMAGE, (name, bad)]))
                .unwrap_err()
                .to_string();
            assert!(err.contains(name), "{name}={bad}: {err}");
        }
    }

    #[test]
    fn control_plane_uri_needs_scheme_host_and_port() {
        for bad in ["host.docker.internal:8083", "http://h", "ftp://h:1"] {
            let err = check_control_plane_uri(bad).unwrap_err().to_string();
            assert!(err.contains("CHELADB_CONTROL_PLANE_URI"), "{bad}: {err}");
        }
        check_control_plane_uri("http://172.17.0.1:8083").unwrap();
    }

    #[tokio::test]
    async fn build_without_jwt_keys_names_the_bootstrap() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = WorkerConfig::from_lookup(lookup(&[IMAGE])).unwrap();
        config.compute_jwt_dir = dir.path().to_path_buf();
        let metadata = MetadataClient::connect_lazy(&MetadataConfig {
            database_url: "postgres://u:p@127.0.0.1:1/none".into(),
        })
        .unwrap();
        let err = match build(&config, metadata, SecretKey::generate()) {
            Ok(_) => panic!("build succeeded without JWT keys"),
            Err(err) => format!("{err:#}"),
        };
        assert!(err.contains("just zitadel-bootstrap"), "{err}");
    }
}
