//! The worker's environment, and wiring the real ports into a [`Worker`].

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, anyhow};

use chelabase_metadata::MetadataClient;
use chelabase_neon::proxy_cache::ProxyCache;
use chelabase_neon::safekeeper::SafekeeperClient;
use chelabase_neon::{ComputeSettings, DockerBackend, StorconClient};
use chelabase_secrets::SecretKey;
use chelabase_types::mode::Mode;
use chelabase_types::role_settings::RoleSettings;

use crate::Worker;
use crate::ports::{HttpPusher, RealStorage};
use crate::safekeepers::SafekeeperSet;

/// How often a running job refreshes its claim (branch jobs and merges).
pub const DEFAULT_CLAIM_REFRESH: Duration = Duration::from_secs(60);

/// The worker over the real metadata database, storage controller, Docker, Valkey and
/// compute_ctl.
pub type RealWorker = Worker<MetadataClient, RealStorage, DockerBackend, ProxyCache, HttpPusher>;

/// No secrets in here, so `Debug` is safe.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub storcon_url: String,
    /// `CHELABASE_SAFEKEEPER_HTTP`: where to reach each safekeeper's HTTP API, by the id the
    /// storage controller knows it by (`id=url`, comma separated; default
    /// `1=http://127.0.0.1:7676`, the Compose safekeeper).
    pub safekeeper_http: Vec<(u64, reqwest::Url)>,
    pub compute_jwt_dir: PathBuf,
    pub compute_network: String,
    pub control_plane_uri: String,
    pub valkey_url: String,
    pub compute_image: String,
    pub mode: chelabase_types::mode::Mode,
    pub owner_superuser_requested: bool,
    /// `CHELABASE_BRANCH_ROLE_CAP`, `CHELABASE_BRANCH_DATABASE_CAP` and
    /// `CHELABASE_PASSWORD_STRENGTH` (platform mode forces strength on).
    pub roles: RoleSettings,
    /// `CHELABASE_ZITADEL_ISSUER`: Zitadel's base URL.
    pub zitadel_issuer: Option<String>,
    /// `CHELABASE_ZITADEL_ADMIN_PAT_FILE`: the admin PAT (org create/delete, user reads,
    /// session deletes). Read by [`build`].
    pub zitadel_admin_pat_file: Option<PathBuf>,
    /// `CHELABASE_APP_USERS_RECONCILE_SECS` (default 300, 30 to 86,400).
    pub app_users_reconcile_every: Duration,
    /// `CHELABASE_WORKER_REMOVE_ORPHANS` (default on): reconcile removes compute containers no
    /// endpoint row records. Off for a worker on its own metadata database that shares the
    /// machine's Docker with another stack (a branch's separate dev database).
    pub remove_orphans: bool,
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
        let compute_image = non_blank(&get, "CHELABASE_COMPUTE_IMAGE").ok_or_else(|| {
            anyhow!(
                "CHELABASE_COMPUTE_IMAGE is not set; just dev and just worker set it from scripts/neon-images.sh"
            )
        })?;
        let mode = Mode::parse(non_blank(&get, Mode::ENV).as_deref()).map_err(|e| anyhow!(e))?;
        // Only an explicit true value counts.
        let owner_superuser_requested = matches!(
            non_blank(&get, "CHELABASE_DB_OWNER_SUPERUSER")
                .map(|v| v.to_ascii_lowercase())
                .as_deref(),
            Some("true" | "1" | "yes")
        );
        let roles = RoleSettings::from_lookup(mode, &get).map_err(|e| anyhow!(e))?;
        let config = Self {
            storcon_url: or_default("CHELABASE_STORCON_URL", "http://127.0.0.1:1234"),
            safekeeper_http: safekeeper_http(non_blank(&get, "CHELABASE_SAFEKEEPER_HTTP"))?,
            compute_jwt_dir: PathBuf::from(or_default("CHELABASE_COMPUTE_JWT_DIR", ".chelabase/neon")),
            compute_network: or_default("CHELABASE_COMPUTE_NETWORK", "chelabase_default"),
            control_plane_uri: or_default(
                "CHELABASE_CONTROL_PLANE_URI",
                "http://host.docker.internal:8083",
            ),
            valkey_url: or_default("CHELABASE_VALKEY_URL", "redis://127.0.0.1:6379"),
            compute_image,
            mode,
            owner_superuser_requested,
            roles,
            zitadel_issuer: non_blank(&get, "CHELABASE_ZITADEL_ISSUER"),
            zitadel_admin_pat_file: non_blank(&get, "CHELABASE_ZITADEL_ADMIN_PAT_FILE")
                .map(PathBuf::from),
            app_users_reconcile_every: reconcile_every(non_blank(
                &get,
                "CHELABASE_APP_USERS_RECONCILE_SECS",
            ))?,
            remove_orphans: remove_orphans(non_blank(&get, "CHELABASE_WORKER_REMOVE_ORPHANS"))?,
        };
        check_control_plane_uri(&config.control_plane_uri)?;
        Ok(config)
    }

    /// Requested, and in self-hosted mode: platform mode ignores the switch.
    pub fn owner_superuser(&self) -> bool {
        self.owner_superuser_requested && self.mode == Mode::SelfHosted
    }
}

/// `CHELABASE_SAFEKEEPER_HTTP`: `id=url` entries, comma separated, each id once and each URL
/// plain `http://` or `https://`.
fn safekeeper_http(value: Option<String>) -> anyhow::Result<Vec<(u64, reqwest::Url)>> {
    const NAME: &str = "CHELABASE_SAFEKEEPER_HTTP";
    let value = value.unwrap_or_else(|| "1=http://127.0.0.1:7676".to_string());
    let mut out: Vec<(u64, reqwest::Url)> = Vec::new();
    for entry in value.split(',').map(str::trim) {
        let bad = |why: &str| {
            anyhow!(
                "{NAME}={value:?}: {entry:?} {why}; expected id=url, e.g. 1=http://127.0.0.1:7676"
            )
        };
        let (id, url) = entry.split_once('=').ok_or_else(|| bad("has no '='"))?;
        let id: u64 = id.trim().parse().map_err(|_| bad("has no numeric id"))?;
        let url: reqwest::Url = url.trim().parse().map_err(|_| bad("has no valid URL"))?;
        if !matches!(url.scheme(), "http" | "https") || url.host().is_none() {
            return Err(bad("needs an http(s) URL with a host"));
        }
        if out.iter().any(|(seen, _)| *seen == id) {
            return Err(bad("repeats an id"));
        }
        out.push((id, url));
    }
    Ok(out)
}

/// `CHELABASE_APP_USERS_RECONCILE_SECS`: whole seconds from 30 to 86,400; 300 when unset.
fn reconcile_every(value: Option<String>) -> anyhow::Result<Duration> {
    const NAME: &str = "CHELABASE_APP_USERS_RECONCILE_SECS";
    let Some(value) = value else {
        return Ok(Duration::from_secs(300));
    };
    match value.parse::<u64>() {
        Ok(secs) if (30..=86_400).contains(&secs) => Ok(Duration::from_secs(secs)),
        _ => Err(anyhow!("{NAME}={value:?}: expected whole seconds from 30 to 86400")),
    }
}

/// `CHELABASE_WORKER_REMOVE_ORPHANS`: on when unset; `true/1/yes/on` or `false/0/no/off`.
fn remove_orphans(value: Option<String>) -> anyhow::Result<bool> {
    const NAME: &str = "CHELABASE_WORKER_REMOVE_ORPHANS";
    match value.map(|v| v.to_ascii_lowercase()).as_deref() {
        None | Some("true" | "1" | "yes" | "on") => Ok(true),
        Some("false" | "0" | "no" | "off") => Ok(false),
        Some(other) => Err(anyhow!("{NAME}={other:?}: expected true or false")),
    }
}

/// Computes call this address from inside their containers, so it needs a scheme, a host and
/// an explicit port.
pub fn check_control_plane_uri(uri: &str) -> anyhow::Result<()> {
    let bad = |why: &str| {
        anyhow!(
            "CHELABASE_CONTROL_PLANE_URI={uri:?}: {why} (for example http://host.docker.internal:8083)"
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

/// Zitadel with the admin PAT, when both settings are given and the file reads; otherwise
/// `None` (logged), and app users jobs fail and are retried until it's configured.
pub fn zitadel_client(config: &WorkerConfig) -> Option<chelabase_zitadel::client::Zitadel> {
    let (Some(issuer), Some(file)) = (&config.zitadel_issuer, &config.zitadel_admin_pat_file)
    else {
        tracing::warn!(
            "CHELABASE_ZITADEL_ISSUER or CHELABASE_ZITADEL_ADMIN_PAT_FILE is unset: app users jobs wait until both are set"
        );
        return None;
    };
    match std::fs::read_to_string(file) {
        Ok(pat) if !pat.trim().is_empty() => {
            Some(chelabase_zitadel::client::Zitadel::new(issuer, pat.trim()))
        }
        Ok(_) => {
            tracing::warn!(file = %file.display(), "the Zitadel admin PAT file is empty: app users jobs wait until it's set");
            None
        }
        Err(e) => {
            tracing::warn!(file = %file.display(), "reading the Zitadel admin PAT: {e}; app users jobs wait until it's readable");
            None
        }
    }
}

/// Wires the real ports. Nothing is contacted: Docker, Valkey and the storage controller are
/// reached on first use.
pub fn build(
    config: &WorkerConfig,
    metadata: MetadataClient,
    key: SecretKey,
) -> anyhow::Result<RealWorker> {
    // First, so a missing key pair names `just zitadel-bootstrap` before anything else fails.
    let signer = chelabase_neon::jwt::load_signer(&config.compute_jwt_dir)?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("building the compute_ctl HTTP client")?;
    Ok(Worker {
        metadata,
        storage: RealStorage {
            storcon: StorconClient::new(&config.storcon_url, 0)
                .with_timeout(Duration::from_secs(10)),
            safekeepers: SafekeeperSet::new(
                config
                    .safekeeper_http
                    .iter()
                    .map(|(id, url)| (*id, SafekeeperClient::new(url.clone())))
                    .collect(),
            ),
        },
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
        ready_timeout: crate::provision::READY_TIMEOUT,
        claim_refresh: DEFAULT_CLAIM_REFRESH,
        schema_check_cursor: Default::default(),
        enabling_seen: Default::default(),
        zitadel: zitadel_client(config),
        app_users_reconcile_every: config.app_users_reconcile_every,
        remove_orphans: config.remove_orphans,
        merge_tx_timeout: crate::merge::MERGE_TX_TIMEOUT,
        merge_online_step_timeout: crate::merge::MERGE_ONLINE_STEP_TIMEOUT,
        resync_requests: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_metadata::MetadataConfig;
    use chelabase_types::mode::Mode;
    use chelabase_types::role_settings::RoleSettings;
    use std::collections::HashMap;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| map.get(k).cloned()
    }

    const IMAGE: (&str, &str) = ("CHELABASE_COMPUTE_IMAGE", "chelabase/compute-node-v17:test");

    #[test]
    fn defaults_are_for_local_dev() {
        let c = WorkerConfig::from_lookup(lookup(&[IMAGE])).unwrap();
        assert_eq!(c.storcon_url, "http://127.0.0.1:1234");
        assert_eq!(c.compute_jwt_dir, PathBuf::from(".chelabase/neon"));
        assert_eq!(c.compute_network, "chelabase_default");
        assert_eq!(c.control_plane_uri, "http://host.docker.internal:8083");
        assert_eq!(c.valkey_url, "redis://127.0.0.1:6379");
        assert_eq!(c.compute_image, "chelabase/compute-node-v17:test");
        assert_eq!(c.mode, Mode::SelfHosted);
        assert!(!c.owner_superuser_requested);
        assert!(!c.owner_superuser());
    }

    #[test]
    fn safekeeper_http_default() {
        let c = WorkerConfig::from_lookup(lookup(&[IMAGE])).unwrap();
        assert_eq!(
            c.safekeeper_http,
            vec![(1, "http://127.0.0.1:7676".parse().unwrap())]
        );
    }

    #[test]
    fn safekeeper_http_parses_list() {
        let c = WorkerConfig::from_lookup(lookup(&[
            IMAGE,
            (
                "CHELABASE_SAFEKEEPER_HTTP",
                " 1=http://10.0.0.1:7676, 2=http://sk2:7676/ ",
            ),
        ]))
        .unwrap();
        assert_eq!(
            c.safekeeper_http,
            vec![
                (1, "http://10.0.0.1:7676".parse().unwrap()),
                (2, "http://sk2:7676/".parse().unwrap())
            ]
        );
    }

    #[test]
    fn safekeeper_http_rejects_garbage() {
        for bad in [
            "http://127.0.0.1:7676",
            "x=http://a:1",
            "1=not a url",
            "1=",
            "1=http://a:1,1=http://b:1",
            "1=ftp://a:1",
        ] {
            let err =
                WorkerConfig::from_lookup(lookup(&[IMAGE, ("CHELABASE_SAFEKEEPER_HTTP", bad)]))
                    .unwrap_err()
                    .to_string();
            assert!(err.contains("CHELABASE_SAFEKEEPER_HTTP"), "{bad}: {err}");
        }
    }

    #[test]
    fn a_missing_image_names_the_recipe() {
        let err = WorkerConfig::from_lookup(lookup(&[]))
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "CHELABASE_COMPUTE_IMAGE is not set; just dev and just worker set it from scripts/neon-images.sh"
        );
    }

    #[test]
    fn blank_values_count_as_unset() {
        let err = WorkerConfig::from_lookup(lookup(&[("CHELABASE_COMPUTE_IMAGE", "  ")]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("CHELABASE_COMPUTE_IMAGE is not set"), "{err}");
        let c = WorkerConfig::from_lookup(lookup(&[
            IMAGE,
            ("CHELABASE_STORCON_URL", ""),
            ("CHELABASE_VALKEY_URL", "   "),
            ("CHELABASE_MODE", ""),
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
                ("CHELABASE_MODE", mode),
                ("CHELABASE_DB_OWNER_SUPERUSER", flag),
            ]))
            .unwrap();
            assert_eq!(c.owner_superuser_requested, flag == "true", "{mode} {flag}");
            assert_eq!(c.owner_superuser(), expected, "{mode} {flag}");
        }
        for on in ["1", "YES", " True "] {
            let c = WorkerConfig::from_lookup(lookup(&[IMAGE, ("CHELABASE_DB_OWNER_SUPERUSER", on)]))
                .unwrap();
            assert!(c.owner_superuser(), "{on}");
        }
        let c = WorkerConfig::from_lookup(lookup(&[IMAGE, ("CHELABASE_DB_OWNER_SUPERUSER", "nope")]))
            .unwrap();
        assert!(!c.owner_superuser());
        assert!(WorkerConfig::from_lookup(lookup(&[IMAGE, ("CHELABASE_MODE", "cloud")])).is_err());
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
            ("CHELABASE_BRANCH_ROLE_CAP", "7"),
            ("CHELABASE_BRANCH_DATABASE_CAP", "3"),
            ("CHELABASE_PASSWORD_STRENGTH", "on"),
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
        let c = WorkerConfig::from_lookup(lookup(&[IMAGE, ("CHELABASE_MODE", "platform")])).unwrap();
        assert!(c.roles.strength && !c.roles.prehashed_allowed);
        for (name, bad) in [
            ("CHELABASE_BRANCH_ROLE_CAP", "501"),
            ("CHELABASE_BRANCH_DATABASE_CAP", "0"),
            ("CHELABASE_PASSWORD_STRENGTH", "maybe"),
        ] {
            let err = WorkerConfig::from_lookup(lookup(&[IMAGE, (name, bad)]))
                .unwrap_err()
                .to_string();
            assert!(err.contains(name), "{name}={bad}: {err}");
        }
    }

    #[test]
    fn config_reads_reconcile_interval() {
        let c = WorkerConfig::from_lookup(lookup(&[IMAGE])).unwrap();
        assert_eq!(c.app_users_reconcile_every, Duration::from_secs(300));
        assert_eq!(c.zitadel_issuer, None);
        assert_eq!(c.zitadel_admin_pat_file, None);
        let c = WorkerConfig::from_lookup(lookup(&[
            IMAGE,
            ("CHELABASE_APP_USERS_RECONCILE_SECS", "45"),
            ("CHELABASE_ZITADEL_ISSUER", "http://localhost:8080"),
            ("CHELABASE_ZITADEL_ADMIN_PAT_FILE", ".chelabase/zitadel/admin.pat"),
        ]))
        .unwrap();
        assert_eq!(c.app_users_reconcile_every, Duration::from_secs(45));
        assert_eq!(c.zitadel_issuer.as_deref(), Some("http://localhost:8080"));
        assert_eq!(
            c.zitadel_admin_pat_file,
            Some(PathBuf::from(".chelabase/zitadel/admin.pat"))
        );
        for edge in ["30", "86400"] {
            let c = WorkerConfig::from_lookup(lookup(&[
                IMAGE,
                ("CHELABASE_APP_USERS_RECONCILE_SECS", edge),
            ]))
            .unwrap();
            assert_eq!(c.app_users_reconcile_every.as_secs().to_string(), edge);
        }
        for bad in ["29", "86401", "0", "five", "-1"] {
            let err = WorkerConfig::from_lookup(lookup(&[
                IMAGE,
                ("CHELABASE_APP_USERS_RECONCILE_SECS", bad),
            ]))
            .unwrap_err()
            .to_string();
            assert!(err.contains("CHELABASE_APP_USERS_RECONCILE_SECS"), "{bad}: {err}");
        }
        // Blank counts as unset.
        let c = WorkerConfig::from_lookup(lookup(&[
            IMAGE,
            ("CHELABASE_APP_USERS_RECONCILE_SECS", " "),
            ("CHELABASE_ZITADEL_ADMIN_PAT_FILE", ""),
        ]))
        .unwrap();
        assert_eq!(c.app_users_reconcile_every, Duration::from_secs(300));
        assert_eq!(c.zitadel_admin_pat_file, None);
    }

    /// `CHELABASE_WORKER_REMOVE_ORPHANS`: on unless explicitly false (a worker sharing the
    /// machine's computes with another metadata database must keep the ones it doesn't know).
    #[test]
    fn config_reads_remove_orphans() {
        const VAR: &str = "CHELABASE_WORKER_REMOVE_ORPHANS";
        let with = |v: &str| WorkerConfig::from_lookup(lookup(&[IMAGE, (VAR, v)]));
        let c = WorkerConfig::from_lookup(lookup(&[IMAGE])).unwrap();
        assert!(c.remove_orphans);
        for off in ["false", "0", "no", "off", " FALSE "] {
            assert!(!with(off).unwrap().remove_orphans, "{off:?}");
        }
        for on in ["true", "1", "yes", "on", ""] {
            assert!(with(on).unwrap().remove_orphans, "{on:?}");
        }
        let err = with("maybe").unwrap_err().to_string();
        assert!(err.contains(VAR), "{err}");
    }

    /// Both settings and a readable file give a client; anything less gives none.
    #[test]
    fn zitadel_client_needs_issuer_and_pat_file() {
        let dir = tempfile::tempdir().unwrap();
        let pat = dir.path().join("admin.pat");
        std::fs::write(&pat, "secret-pat\n").unwrap();
        let mut c = WorkerConfig::from_lookup(lookup(&[IMAGE])).unwrap();
        assert!(zitadel_client(&c).is_none());
        c.zitadel_issuer = Some("http://localhost:8080".into());
        assert!(zitadel_client(&c).is_none());
        c.zitadel_admin_pat_file = Some(dir.path().join("missing.pat"));
        assert!(zitadel_client(&c).is_none());
        c.zitadel_admin_pat_file = Some(pat);
        assert!(zitadel_client(&c).is_some());
    }

    #[test]
    fn control_plane_uri_needs_scheme_host_and_port() {
        for bad in ["host.docker.internal:8083", "http://h", "ftp://h:1"] {
            let err = check_control_plane_uri(bad).unwrap_err().to_string();
            assert!(err.contains("CHELABASE_CONTROL_PLANE_URI"), "{bad}: {err}");
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
