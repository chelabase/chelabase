//! The worker's environment, and wiring the real ports into a [`Worker`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow};

use chelabase_metadata::MetadataClient;
use chelabase_neon::compute_tls::ComputeCa;
use chelabase_neon::jwt::StorageScope;
use chelabase_neon::proxy_cache::ProxyCache;
use chelabase_neon::safekeeper::SafekeeperClient;
use chelabase_neon::{
    Backend, BackendKind, ComputeSettings, DockerSettings, StorconClient,
};
use chelabase_secrets::SecretKey;
use chelabase_types::mode::Mode;
use chelabase_types::mode_policy::ModePolicy;
use chelabase_types::role_settings::RoleSettings;

use crate::Worker;
use crate::ports::{AttachSource, HttpPusher, RealStorage};
use crate::rt_events::{PublishingMetadata, RtPublisher, StoppingBackend};
use crate::safekeepers::SafekeeperSet;

/// How often a running job refreshes its claim (branch jobs and merges).
pub const DEFAULT_CLAIM_REFRESH: Duration = Duration::from_secs(60);

/// The worker over the real metadata database, storage controller, Docker, Valkey and
/// compute_ctl, with realtime's platform events and compute-stopping signal (RT1 T5).
pub type RealWorker = Worker<
    PublishingMetadata<MetadataClient>,
    RealStorage,
    StoppingBackend<Backend, MetadataClient>,
    ProxyCache,
    HttpPusher,
>;

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
    /// `CHELABASE_STACK` (default `main`): the stack this worker's computes belong to.
    pub stack: String,
    pub valkey_url: String,
    /// `CHELABASE_VALKEY_USER` and `CHELABASE_VALKEY_PASSWORD_FILE` (the `control` ACL user in
    /// the stack): both or neither. The password itself is read by [`build`].
    pub valkey_user: Option<String>,
    pub valkey_password_file: Option<PathBuf>,
    pub compute_image: String,
    pub mode: Mode,
    pub policy: ModePolicy,
    pub owner_superuser_requested: bool,
    /// `CHELABASE_BRANCH_ROLE_CAP`, `CHELABASE_BRANCH_DATABASE_CAP` and
    /// `CHELABASE_PASSWORD_STRENGTH` (platform mode forces strength on).
    pub roles: RoleSettings,
    /// `CHELABASE_IDENTITY_URL`: the identity service (app-user pools and users). Read with
    /// [`Self::identity_token_file`] by [`identity_client`].
    pub identity_url: Option<String>,
    /// `CHELABASE_IDENTITY_TOKEN_FILE`: the worker's bearer for the identity service.
    pub identity_token_file: Option<PathBuf>,
    /// `CHELABASE_APP_USERS_RECONCILE_SECS` (default 300, 30 to 86,400).
    pub app_users_reconcile_every: Duration,
    /// `CHELABASE_MEMBER_ROLES_SWEEP_SECS` (default 300, 30 to 86,400): how often every branch's
    /// member roles are reconciled, the backstop for lost or failed reconcile publishes.
    pub member_roles_sweep_every: Duration,
    /// `CHELABASE_WORKER_REMOVE_ORPHANS` (default on): reconcile removes this stack's compute
    /// containers no endpoint row records. Listing is scoped to the stack, so this is an
    /// opt-out for debugging, not a guard against other stacks.
    pub remove_orphans: bool,
    /// `CHELABASE_COMPUTE_CONTROL_NETWORK` (default `chelabase_control`): the network the
    /// orchestrator reaches computes on; member logins come from its addresses there.
    pub compute_control_network: String,
    /// `CHELABASE_COMPUTE_CA_DIR` (default `.chelabase/compute-ca`): `ca.crt` and `ca.key`.
    pub compute_ca_dir: PathBuf,
    /// `CHELABASE_COMPUTE_TLS_DIR` (default `.chelabase/orchestrator-tls`): where the worker
    /// writes the orchestrator's client pair.
    pub compute_tls_dir: PathBuf,
    /// `CHELABASE_MEMBER_LOGIN_CIDRS`: the addresses member logins may come from. `None` when
    /// unset ([`build`] refuses to start); empty means no address rule.
    pub member_login_cidrs: Option<Vec<String>>,
    /// `CHELABASE_STORAGE_TOKEN_REFRESH_SECS` (default 3600, whole seconds, 1 to 43200,
    /// half the 24 h token lifetime): how often computes get a fresh storage token.
    pub storage_token_refresh_every: Duration,
    /// `CHELABASE_REALTIME_LOGIN_CIDRS`: the addresses `chelabase_realtime` may log in from.
    /// `None` when unset ([`WorkerConfig::realtime_logins`] refuses); empty means no address
    /// rule.
    pub realtime_login_cidrs: Option<Vec<String>>,
    /// `CHELABASE_REALTIME_TLS_DIR` (default `.chelabase/realtime-tls`): where the worker
    /// writes realtime's client pair.
    pub realtime_tls_dir: PathBuf,
    /// `CHELABASE_REALTIME_PG_PASSWORD_FILE` (RT1 T5, S1): the `chelabase_realtime` role's
    /// password, read by [`build`]. `None` when unset or blank.
    pub realtime_pg_password_file: Option<PathBuf>,
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
                "CHELABASE_COMPUTE_IMAGE is not set; `just workbench up` sets it from scripts/neon-images.sh"
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
            compute_network: or_default("CHELABASE_COMPUTE_NETWORK", "chelabase_compute"),
            control_plane_uri: or_default(
                "CHELABASE_CONTROL_PLANE_URI",
                "http://orchestrator:8083",
            ),
            stack: chelabase_neon::check_stack(&or_default(
                "CHELABASE_STACK",
                chelabase_neon::DEFAULT_STACK,
            ))?,
            valkey_url: or_default("CHELABASE_VALKEY_URL", "redis://127.0.0.1:6379"),
            valkey_user: non_blank(&get, "CHELABASE_VALKEY_USER"),
            valkey_password_file: non_blank(&get, "CHELABASE_VALKEY_PASSWORD_FILE")
                .map(PathBuf::from),
            compute_image,
            mode,
            policy: ModePolicy::new(mode),
            owner_superuser_requested,
            roles,
            identity_url: non_blank(&get, "CHELABASE_IDENTITY_URL"),
            identity_token_file: non_blank(&get, "CHELABASE_IDENTITY_TOKEN_FILE")
                .map(PathBuf::from),
            app_users_reconcile_every: reconcile_every(
                "CHELABASE_APP_USERS_RECONCILE_SECS",
                non_blank(&get, "CHELABASE_APP_USERS_RECONCILE_SECS"),
            )?,
            member_roles_sweep_every: reconcile_every(
                "CHELABASE_MEMBER_ROLES_SWEEP_SECS",
                non_blank(&get, "CHELABASE_MEMBER_ROLES_SWEEP_SECS"),
            )?,
            remove_orphans: remove_orphans(non_blank(&get, "CHELABASE_WORKER_REMOVE_ORPHANS"))?,
            compute_control_network: or_default(
                "CHELABASE_COMPUTE_CONTROL_NETWORK",
                "chelabase_control",
            ),
            compute_ca_dir: PathBuf::from(or_default(
                "CHELABASE_COMPUTE_CA_DIR",
                ".chelabase/compute-ca",
            )),
            compute_tls_dir: PathBuf::from(or_default(
                "CHELABASE_COMPUTE_TLS_DIR",
                ".chelabase/orchestrator-tls",
            )),
            // Read raw: an empty value is "no address rule", not "unset".
            member_login_cidrs: match get("CHELABASE_MEMBER_LOGIN_CIDRS") {
                None => None,
                Some(raw) => Some(chelabase_neon::spec::parse_member_login_cidrs(Some(&raw))?),
            },
            storage_token_refresh_every: storage_token_refresh_every(non_blank(
                &get,
                "CHELABASE_STORAGE_TOKEN_REFRESH_SECS",
            ))?,
            realtime_login_cidrs: match get("CHELABASE_REALTIME_LOGIN_CIDRS") {
                None => None,
                Some(raw) => Some(chelabase_neon::spec::parse_realtime_login_cidrs(Some(&raw))?),
            },
            realtime_tls_dir: PathBuf::from(or_default(
                "CHELABASE_REALTIME_TLS_DIR",
                ".chelabase/realtime-tls",
            )),
            realtime_pg_password_file: non_blank(&get, REALTIME_PG_PASSWORD_FILE)
                .map(PathBuf::from),
        };
        check_control_plane_uri(&config.control_plane_uri)?;
        // The sweep writes realtime's pair and the orchestrator's pair each into its own
        // directory: one directory for both would make each pair replace the other.
        if same_dir(&config.realtime_tls_dir, &config.compute_tls_dir) {
            return Err(anyhow!(
                "CHELABASE_REALTIME_TLS_DIR ({}) and CHELABASE_COMPUTE_TLS_DIR ({}) name the same \
                 directory: realtime's client pair and the orchestrator's need one each",
                config.realtime_tls_dir.display(),
                config.compute_tls_dir.display()
            ));
        }
        if config.valkey_user.is_some() != config.valkey_password_file.is_some() {
            return Err(anyhow!(
                "set both CHELABASE_VALKEY_USER and CHELABASE_VALKEY_PASSWORD_FILE, or neither"
            ));
        }
        Ok(config)
    }

    /// The spec's realtime logins; an unset `CHELABASE_REALTIME_LOGIN_CIDRS` is an error (a
    /// missing setting must not silently mean "any address").
    pub fn realtime_logins(&self) -> anyhow::Result<chelabase_neon::spec::MemberLogins> {
        let cidrs = self.realtime_login_cidrs.clone().ok_or_else(|| {
            anyhow!(
                "CHELABASE_REALTIME_LOGIN_CIDRS is unset: set it to the addresses realtime's \
                 logins come from (comma-separated CIDRs), or to an empty value for no address rule"
            )
        })?;
        Ok(chelabase_neon::spec::MemberLogins::realtime(cidrs))
    }

    /// Requested, and in self-hosted mode: platform mode ignores the switch.
    pub fn owner_superuser(&self) -> bool {
        self.owner_superuser_requested && self.policy.owner_superuser_allowed()
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

/// `CHELABASE_APP_USERS_RECONCILE_SECS` or `CHELABASE_MEMBER_ROLES_SWEEP_SECS` (`name`): whole
/// seconds from 30 to 86,400; 300 when unset.
fn reconcile_every(name: &str, value: Option<String>) -> anyhow::Result<Duration> {
    let Some(value) = value else {
        return Ok(Duration::from_secs(300));
    };
    match value.parse::<u64>() {
        Ok(secs) if (30..=86_400).contains(&secs) => Ok(Duration::from_secs(secs)),
        _ => Err(anyhow!("{name}={value:?}: expected whole seconds from 30 to 86400")),
    }
}

/// Whether two directory settings name the same directory: by their real paths when both
/// exist, otherwise (they may not exist yet) spelled alike once `.` components, repeated and
/// trailing slashes are normalised away.
fn same_dir(a: &std::path::Path, b: &std::path::Path) -> bool {
    use std::path::Component;
    // Both exist: their real paths (`..`, symlinks, relative against absolute).
    if let (Ok(a), Ok(b)) = (a.canonicalize(), b.canonicalize()) {
        return a == b;
    }
    let not_dot = |c: &Component<'_>| *c != Component::CurDir;
    a.components().filter(not_dot).eq(b.components().filter(not_dot))
}

/// `CHELABASE_STORAGE_TOKEN_REFRESH_SECS`: whole seconds, above 0 and at most half the token's
/// lifetime (`STORAGE_TOKEN_TTL`, 24 h): a pass that fails once still leaves a retry before the
/// token expires.
fn storage_token_refresh_every(value: Option<String>) -> anyhow::Result<Duration> {
    const NAME: &str = "CHELABASE_STORAGE_TOKEN_REFRESH_SECS";
    let Some(value) = value else {
        return Ok(Duration::from_secs(3600));
    };
    let max = chelabase_neon::jwt::STORAGE_TOKEN_TTL.as_secs() / 2;
    match value.parse::<u64>() {
        Ok(secs) if secs > 0 && secs <= max => Ok(Duration::from_secs(secs)),
        _ => Err(anyhow!(
            "{NAME}={value:?}: expected whole seconds from 1 to {max} (half a storage token's life)"
        )),
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
            "CHELABASE_CONTROL_PLANE_URI={uri:?}: {why} (for example http://orchestrator:8083)"
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

/// The identity service's client, when both settings are given and the token file reads;
/// otherwise `None` (logged), and app users and project delete jobs fail and are retried until
/// it's configured.
pub fn identity_client(config: &WorkerConfig) -> Option<chelabase_identity::Identity> {
    let (Some(url), Some(file)) = (&config.identity_url, &config.identity_token_file) else {
        tracing::warn!(
            "CHELABASE_IDENTITY_URL or CHELABASE_IDENTITY_TOKEN_FILE is unset: app users and project delete jobs wait until both are set"
        );
        return None;
    };
    match chelabase_identity::Identity::from_token_file(url, file) {
        Ok(identity) => Some(identity),
        Err(e) => {
            tracing::warn!("{e}; app users and project delete jobs wait until it's readable");
            None
        }
    }
}

/// The env var naming the `chelabase_realtime` role's password file (S1).
const REALTIME_PG_PASSWORD_FILE: &str = "CHELABASE_REALTIME_PG_PASSWORD_FILE";

/// The `chelabase_realtime` password in `file` (one trailing newline ignored). An unreadable or
/// empty file is an error: a set variable means the stack wants the role.
fn read_realtime_password(file: &Path) -> anyhow::Result<String> {
    let raw = std::fs::read_to_string(file)
        .with_context(|| format!("{REALTIME_PG_PASSWORD_FILE}={}", file.display()))?;
    let password = raw.trim_end_matches(['\n', '\r']);
    if password.is_empty() {
        return Err(anyhow!(
            "{REALTIME_PG_PASSWORD_FILE}={} is empty",
            file.display()
        ));
    }
    Ok(password.to_string())
}

/// Wires the real ports. Nothing is contacted: Docker, Valkey and the storage controller are
/// reached on first use. `publisher` carries realtime's events (S20: the process's NATS, or a
/// no-op or the runner's NATS in tests); `attach` is where provisions read the tenant-attached
/// events (the process's JetStream, or [`AttachSource::Disabled`]: given explicitly, A8). `build`
/// never connects NATS itself.
pub fn build(
    config: &WorkerConfig,
    metadata: MetadataClient,
    key: SecretKey,
    publisher: Arc<dyn RtPublisher>,
    attach: AttachSource,
) -> anyhow::Result<RealWorker> {
    let kind = BackendKind::from_env()?;
    build_with(config, metadata, key, publisher, attach, |settings, docker| {
        Backend::connect(kind, settings, docker)
    })
}

/// [`build`] with the backend's constructor given. The real one needs the engine's
/// socket file to exist (so `build` fails while Docker is restarting); tests that don't
/// exercise Docker pass a backend over an unreachable engine instead.
pub fn build_with(
    config: &WorkerConfig,
    metadata: MetadataClient,
    key: SecretKey,
    publisher: Arc<dyn RtPublisher>,
    attach: AttachSource,
    connect: impl FnOnce(ComputeSettings, DockerSettings) -> anyhow::Result<Backend>,
) -> anyhow::Result<RealWorker> {
    // Edge functions m8: the child's run-as keys are sealed with the secrets key.
    let metadata = metadata.with_secrets_key(key.clone());
    // First, so a missing key pair names its fix (`init-secrets` or `just workbench up`) before anything else fails.
    let signer = chelabase_neon::jwt::load_signer(&config.compute_jwt_dir)?;
    let cidrs = config.member_login_cidrs.clone().ok_or_else(|| {
        anyhow!(
            "CHELABASE_MEMBER_LOGIN_CIDRS is unset: set it to the addresses member logins come \
             from (comma-separated CIDRs), or to an empty value for no address rule"
        )
    })?;
    let realtime_logins = config.realtime_logins()?;
    let ca = Arc::new(ComputeCa::load(&config.compute_ca_dir).with_context(|| {
        format!(
            "loading the compute CA from {} (CHELABASE_COMPUTE_CA_DIR; self-hosted: `init-secrets` creates it, see `docker compose logs init-secrets`; \
             workbench: `just workbench up`)",
            config.compute_ca_dir.display()
        )
    })?);
    // Never a redirect (#26): compute_ctl's JWT goes to the compute only.
    let http = chelabase_http::client_builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("building the compute_ctl HTTP client")?;
    // The storage controller (no --dev) and the safekeepers check NeonJWT: scoped tokens from the
    // compute-JWT key pair.
    let storcon_token = signer.storage_token(&StorageScope::Admin)?;
    let safekeeper_token = signer.storage_token(&StorageScope::SafekeeperData)?;
    let cache = match (&config.valkey_user, &config.valkey_password_file) {
        (Some(user), Some(file)) => {
            let password = std::fs::read_to_string(file).with_context(|| {
                format!("CHELABASE_VALKEY_PASSWORD_FILE={}", file.display())
            })?;
            ProxyCache::with_auth(&config.valkey_url, user, password.trim_end_matches(['\n', '\r']))?
        }
        _ => ProxyCache::new(&config.valkey_url)?,
    };
    let realtime_password = config
        .realtime_pg_password_file
        .as_deref()
        .map(read_realtime_password)
        .transpose()?;
    Ok(Worker {
        metadata: PublishingMetadata::new(metadata.clone(), publisher.clone()),
        storage: RealStorage {
            storcon: StorconClient::new(&config.storcon_url, 0)
                .with_timeout(Duration::from_secs(10))
                .with_token(storcon_token),
            safekeepers: SafekeeperSet::new(
                config
                    .safekeeper_http
                    .iter()
                    .map(|(id, url)| {
                        (
                            *id,
                            SafekeeperClient::new(url.clone()).with_token(safekeeper_token.clone()),
                        )
                    })
                    .collect(),
            ),
            attach,
        },
        computes: StoppingBackend::new(
            connect(
                ComputeSettings {
                    image: config.compute_image.clone(),
                    control_plane_uri: config.control_plane_uri.clone(),
                    stack: config.stack.clone(),
                    ca: Some(ca.clone()),
                },
                DockerSettings {
                    network: config.compute_network.clone(),
                    control_network: config.compute_control_network.clone(),
                },
            )?,
            metadata,
            publisher,
        ),
        cache,
        pusher: HttpPusher { http, signer },
        key,
        owner_superuser: config.owner_superuser(),
        roles: config.roles,
        control_plane_uri: config.control_plane_uri.clone(),
        ready_poll: Duration::from_millis(500),
        ready_timeout: crate::provision::READY_TIMEOUT,
        ready_cap: crate::provision::READY_CAP,
        attach_timeout: crate::provision::ATTACH_TIMEOUT,
        claim_refresh: DEFAULT_CLAIM_REFRESH,
        schema_check_cursor: Default::default(),
        enabling_seen: Default::default(),
        recreate_backoff: Default::default(),
        fn_sweeps: Default::default(),
        identity: identity_client(config),
        app_users_reconcile_every: config.app_users_reconcile_every,
        member_roles_sweep_every: config.member_roles_sweep_every,
        remove_orphans: config.remove_orphans,
        merge_tx_timeout: crate::merge::MERGE_TX_TIMEOUT,
        merge_online_step_timeout: crate::merge::MERGE_ONLINE_STEP_TIMEOUT,
        resync_requests: Default::default(),
        member_role_requests: Default::default(),
        member_logins: chelabase_neon::spec::MemberLogins::orchestrator(cidrs),
        cert_sweep: Some(crate::certs::CertSweep {
            ca,
            tls_dir: config.compute_tls_dir.clone(),
            realtime_tls_dir: config.realtime_tls_dir.clone(),
        }),
        realtime_logins,
        storage_token_refresh_every: config.storage_token_refresh_every,
        storage_token_failures: Default::default(),
        realtime_password,
        blob_store: None,
        storage_sweep: Default::default(),
        sweep_requests: Default::default(),
        storage_sweep_refusals: Default::default(),
        storage_sweep_restore_voids: Default::default(),
    })
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

/// Edge functions m7: what scheduled runs and the bundle sweep need.
#[derive(Debug, Default)]
pub struct FunctionsSettings {
    /// `CHELABASE_FUNCTIONS_INTERNAL_URL` and the worker's bearer
    /// (`CHELABASE_WORKER_FUNCTIONS_TOKEN[_FILE]`); `None` when either is unset (no scheduled
    /// runs).
    pub invoke: Option<(String, String)>,
    /// The bundles bucket as the `functions` S3 identity: `CHELABASE_FUNCTIONS_S3_BUCKET` and
    /// its own `CHELABASE_FUNCTIONS_S3_{ACCESS,SECRET}_KEY_FILE`, over the
    /// `CHELABASE_STORAGE_*` backend, endpoint and region; `None` when the bucket is unset (no
    /// bundle sweep).
    pub bundles: Option<chelabase_storage::StorageConfig>,
}

pub const FUNCTIONS_BUCKET: &str = "CHELABASE_FUNCTIONS_S3_BUCKET";
pub const FUNCTIONS_ACCESS_KEY_FILE: &str = "CHELABASE_FUNCTIONS_S3_ACCESS_KEY_FILE";
pub const FUNCTIONS_SECRET_KEY_FILE: &str = "CHELABASE_FUNCTIONS_S3_SECRET_KEY_FILE";

pub fn functions_settings(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<FunctionsSettings> {
    use chelabase_storage::config::{ENV_ACCESS_KEY_FILE, ENV_BUCKET, ENV_SECRET_KEY_FILE};
    let token = chelabase_types::secret_env::lookup_secret(&get, "CHELABASE_WORKER_FUNCTIONS_TOKEN")
        .map_err(anyhow::Error::msg)?;
    let invoke = non_blank(&get, "CHELABASE_FUNCTIONS_INTERNAL_URL").zip(token);
    let bundles = match non_blank(&get, FUNCTIONS_BUCKET) {
        None => None,
        Some(_) => {
            let names = |k: &str| match k {
                ENV_BUCKET => FUNCTIONS_BUCKET,
                ENV_ACCESS_KEY_FILE => FUNCTIONS_ACCESS_KEY_FILE,
                ENV_SECRET_KEY_FILE => FUNCTIONS_SECRET_KEY_FILE,
                _ => "",
            };
            let config = chelabase_storage::StorageConfig::from_lookup(|k| match names(k) {
                "" => get(k),
                ours => get(ours),
            })
            .map_err(|e| {
                // The storage crate names its own variables; say which ones the worker reads.
                let message = e
                    .to_string()
                    .replace(ENV_ACCESS_KEY_FILE, FUNCTIONS_ACCESS_KEY_FILE)
                    .replace(ENV_SECRET_KEY_FILE, FUNCTIONS_SECRET_KEY_FILE)
                    .replace(ENV_BUCKET, FUNCTIONS_BUCKET);
                anyhow!("the bundle sweep's store: {message}")
            })?;
            Some(config)
        }
    };
    Ok(FunctionsSettings { invoke, bundles })
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

    // Selfhost-install m5 (AC 4): a service with an internal bearer at its development default
    // refuses to start outside the workbench. The worker holds none (it signs storage JWTs
    // with its key pair and reaches NATS with its own password, which has no default), so
    // there is nothing to refuse. This pins that two ways (m5 fix round, N3):
    // - it reads none of the real bearer variables (`DEV_BEARERS`, plain or `_FILE`), nor
    //   anything named like one (`_TOKEN`, `_SECRET`, `_BEARER`; a heuristic for new ones);
    // - its config is destructured field by field, with no `..`: a field added to
    //   `WorkerConfig` fails to compile here until someone says whether it is a credential
    //   (and, if it has a development default, adds `DevDefaults::check_bearers`).
    #[test]
    fn dev_bearer_refused_without_dev_defaults() {
        let asked = std::cell::RefCell::new(Vec::<String>::new());
        let get = lookup(&[IMAGE]);
        let c = WorkerConfig::from_lookup(|name| {
            asked.borrow_mut().push(name.to_string());
            get(name)
        })
        .unwrap();
        let asked = asked.into_inner();
        assert!(asked.iter().any(|n| n == "CHELABASE_COMPUTE_IMAGE"), "{asked:?}");
        let real: Vec<String> = chelabase_types::dev_defaults::DEV_BEARERS
            .iter()
            .flat_map(|(var, _)| [var.to_string(), format!("{var}_FILE")])
            .collect();
        // The identity service's token is a file the stack mounts and has no default at all
        // (unset, the client is absent and its jobs wait; checked below).
        let no_default = ["CHELABASE_IDENTITY_TOKEN_FILE"];
        let bearers: Vec<&String> = asked
            .iter()
            .filter(|n| !no_default.contains(&n.as_str()))
            .filter(|n| {
                real.contains(n)
                    || ["_TOKEN", "_TOKEN_FILE", "_SECRET", "_SECRET_FILE", "_BEARER", "_BEARER_FILE"]
                        .iter()
                        .any(|suffix| n.ends_with(suffix))
            })
            .collect();
        assert!(bearers.is_empty(), "the worker reads a bearer without the dev-default check: {bearers:?}");
        // Every field, none a bearer. Credentials it does hold have no default: the Valkey
        // password, realtime's role password and the identity token are files the stack mounts.
        let WorkerConfig {
            storcon_url: _,
            safekeeper_http: _,
            compute_jwt_dir: _,
            compute_network: _,
            control_plane_uri: _,
            stack: _,
            valkey_url: _,
            valkey_user: _,
            valkey_password_file,
            compute_image: _,
            mode: _,
            policy: _,
            owner_superuser_requested: _,
            roles: _,
            identity_url: _,
            identity_token_file,
            app_users_reconcile_every: _,
            member_roles_sweep_every: _,
            remove_orphans: _,
            compute_control_network: _,
            compute_ca_dir: _,
            compute_tls_dir: _,
            member_login_cidrs: _,
            storage_token_refresh_every: _,
            realtime_login_cidrs: _,
            realtime_tls_dir: _,
            realtime_pg_password_file,
        } = c;
        assert_eq!(
            (
                valkey_password_file,
                identity_token_file,
                realtime_pg_password_file
            ),
            (None, None, None),
            "unset, a credential file is absent, never a default"
        );
    }

    #[test]
    fn defaults_are_for_local_dev() {
        let c = WorkerConfig::from_lookup(lookup(&[IMAGE])).unwrap();
        assert_eq!(c.storcon_url, "http://127.0.0.1:1234");
        assert_eq!(c.compute_jwt_dir, PathBuf::from(".chelabase/neon"));
        assert_eq!(c.compute_network, "chelabase_compute");
        assert_eq!(c.control_plane_uri, "http://orchestrator:8083");
        assert_eq!(c.stack, "main");
        assert_eq!(c.valkey_url, "redis://127.0.0.1:6379");
        assert_eq!(c.compute_image, "chelabase/compute-node-v17:test");
        assert_eq!(c.mode, Mode::SelfHosted);
        assert!(!c.owner_superuser_requested);
        assert!(!c.owner_superuser());
    }

    #[test]
    fn the_stack_comes_from_the_env() {
        let c = WorkerConfig::from_lookup(lookup(&[IMAGE, ("CHELABASE_STACK", "feat-x")])).unwrap();
        assert_eq!(c.stack, "feat-x");
        let blank = WorkerConfig::from_lookup(lookup(&[IMAGE, ("CHELABASE_STACK", " ")])).unwrap();
        assert_eq!(blank.stack, "main");
    }

    // T1 review I1
    #[test]
    fn a_padded_stack_is_trimmed_and_an_invalid_one_refused() {
        let c = WorkerConfig::from_lookup(lookup(&[IMAGE, ("CHELABASE_STACK", " feat-x\n")])).unwrap();
        assert_eq!(c.stack, "feat-x");
        assert!(WorkerConfig::from_lookup(lookup(&[IMAGE, ("CHELABASE_STACK", "feat/x")])).is_err());
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
            "CHELABASE_COMPUTE_IMAGE is not set; `just workbench up` sets it from scripts/neon-images.sh"
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
        let c = WorkerConfig::from_lookup(lookup(&[
            IMAGE,
            ("CHELABASE_APP_USERS_RECONCILE_SECS", "45"),
        ]))
        .unwrap();
        assert_eq!(c.app_users_reconcile_every, Duration::from_secs(45));
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
        ]))
        .unwrap();
        assert_eq!(c.app_users_reconcile_every, Duration::from_secs(300));
    }

    /// T8 fix round (I2): the member-role sweep, the backstop for lost reconcile publishes.
    #[test]
    fn config_reads_member_roles_sweep_interval() {
        const VAR: &str = "CHELABASE_MEMBER_ROLES_SWEEP_SECS";
        let c = WorkerConfig::from_lookup(lookup(&[IMAGE])).unwrap();
        assert_eq!(c.member_roles_sweep_every, Duration::from_secs(300));
        let c = WorkerConfig::from_lookup(lookup(&[IMAGE, (VAR, "60")])).unwrap();
        assert_eq!(c.member_roles_sweep_every, Duration::from_secs(60));
        let c = WorkerConfig::from_lookup(lookup(&[IMAGE, (VAR, " ")])).unwrap();
        assert_eq!(c.member_roles_sweep_every, Duration::from_secs(300));
        for bad in ["29", "86401", "x"] {
            let err = WorkerConfig::from_lookup(lookup(&[IMAGE, (VAR, bad)]))
                .unwrap_err()
                .to_string();
            assert!(err.contains(VAR), "{bad}: {err}");
        }
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
    fn identity_client_needs_url_and_token_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = WorkerConfig::from_lookup(lookup(&[IMAGE])).unwrap();
        assert!(identity_client(&c).is_none());
        let token = dir.path().join("worker-identity.token");
        let c2 = WorkerConfig::from_lookup(lookup(&[
            IMAGE,
            ("CHELABASE_IDENTITY_URL", "http://identity:8080"),
            ("CHELABASE_IDENTITY_TOKEN_FILE", token.to_str().unwrap()),
        ]))
        .unwrap();
        assert_eq!(c2.identity_url.as_deref(), Some("http://identity:8080"));
        assert_eq!(c2.identity_token_file.as_deref(), Some(token.as_path()));
        // Missing, then empty: no client.
        assert!(identity_client(&c2).is_none());
        std::fs::write(&token, " \n").unwrap();
        assert!(identity_client(&c2).is_none());
        std::fs::write(&token, "secret\n").unwrap();
        assert!(identity_client(&c2).is_some());
        c.identity_token_file = Some(token);
        assert!(identity_client(&c).is_none(), "the URL is needed too");
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
        let err = match build(
            &config,
            metadata,
            SecretKey::generate(),
            Arc::new(crate::rt_events::NoopPublisher),
            crate::ports::AttachSource::Disabled,
        ) {
            Ok(_) => panic!("build succeeded without JWT keys"),
            Err(err) => format!("{err:#}"),
        };
        assert!(err.contains("init-secrets"), "{err}");
        assert!(err.contains("just workbench up"), "{err}");
    }

    use std::path::PathBuf;

    #[test]
    fn the_storage_token_refresh_interval_accepts_its_bounds() {
        for ok in ["1", "43200"] {
            let c = WorkerConfig::from_lookup(lookup(&[
                IMAGE,
                ("CHELABASE_STORAGE_TOKEN_REFRESH_SECS", ok),
            ]))
            .unwrap();
            assert_eq!(c.storage_token_refresh_every, Duration::from_secs(ok.parse().unwrap()));
        }
    }

    // F2 edge: a leading `./` names the same directory as the bare relative path.
    #[test]
    fn a_leading_dot_slash_still_names_the_same_directory() {
        for (realtime, compute) in [("./tls", "tls"), ("tls", "./tls/"), ("././a/./b", "a/b")] {
            let err = WorkerConfig::from_lookup(lookup(&[
                IMAGE,
                ("CHELABASE_REALTIME_TLS_DIR", realtime),
                ("CHELABASE_COMPUTE_TLS_DIR", compute),
            ]))
            .map(|_| ())
            .expect_err(realtime)
            .to_string();
            assert!(err.contains("CHELABASE_REALTIME_TLS_DIR"), "{err}");
        }
        // Relative and absolute spellings aren't resolved: they're different settings.
        WorkerConfig::from_lookup(lookup(&[
            IMAGE,
            ("CHELABASE_REALTIME_TLS_DIR", "tls"),
            ("CHELABASE_COMPUTE_TLS_DIR", "/tls"),
        ]))
        .unwrap();
    }

    // F2 round 2: when both directories exist, their real paths are compared, so another
    // spelling of one directory (`..`, a symlink, an absolute and a relative path) is caught.
    #[test]
    fn existing_directories_are_compared_by_their_real_paths() {
        let root = tempfile::tempdir().unwrap();
        let tls = root.path().join("tls");
        std::fs::create_dir(&tls).unwrap();
        std::fs::create_dir(root.path().join("other")).unwrap();
        std::os::unix::fs::symlink(&tls, root.path().join("link")).unwrap();
        let s = |p: std::path::PathBuf| p.to_str().unwrap().to_string();
        let relative = {
            let cwd = std::env::current_dir().unwrap();
            let up = cwd.components().count() - 1;
            let mut p = std::path::PathBuf::new();
            for _ in 0..up {
                p.push("..");
            }
            p.push(tls.strip_prefix("/").unwrap());
            s(p)
        };
        for other in [
            s(root.path().join("other/../tls")),
            s(root.path().join("link")),
            relative,
        ] {
            let err = WorkerConfig::from_lookup(lookup(&[
                IMAGE,
                ("CHELABASE_REALTIME_TLS_DIR", s(tls.clone()).as_str()),
                ("CHELABASE_COMPUTE_TLS_DIR", other.as_str()),
            ]))
            .map(|_| ())
            .expect_err(&other)
            .to_string();
            assert!(err.contains("CHELABASE_COMPUTE_TLS_DIR"), "{other}: {err}");
        }
        WorkerConfig::from_lookup(lookup(&[
            IMAGE,
            ("CHELABASE_REALTIME_TLS_DIR", s(tls.clone()).as_str()),
            ("CHELABASE_COMPUTE_TLS_DIR", s(root.path().join("other")).as_str()),
        ]))
        .unwrap();
    }

    #[test]
    fn config_reads_the_round_3_settings() {
        let d = WorkerConfig::from_lookup(lookup(&[IMAGE])).unwrap();
        assert_eq!(d.compute_control_network, "chelabase_control");
        assert_eq!(d.compute_ca_dir, PathBuf::from(".chelabase/compute-ca"));
        assert_eq!(d.compute_tls_dir, PathBuf::from(".chelabase/orchestrator-tls"));
        assert_eq!(d.member_login_cidrs, None, "unset: build() refuses to start");
        let c = WorkerConfig::from_lookup(lookup(&[
            IMAGE,
            ("CHELABASE_COMPUTE_CONTROL_NETWORK", "chelabase-feat-x_control"),
            ("CHELABASE_COMPUTE_CA_DIR", "/m/.chelabase/compute-ca"),
            ("CHELABASE_COMPUTE_TLS_DIR", "/m/.chelabase/orchestrator-tls"),
            ("CHELABASE_MEMBER_LOGIN_CIDRS", "10.213.4.10/32,10.213.4.11/32"),
        ]))
        .unwrap();
        assert_eq!(c.compute_control_network, "chelabase-feat-x_control");
        assert_eq!(c.compute_ca_dir, PathBuf::from("/m/.chelabase/compute-ca"));
        assert_eq!(c.compute_tls_dir, PathBuf::from("/m/.chelabase/orchestrator-tls"));
        assert_eq!(c.member_login_cidrs, Some(vec!["10.213.4.10/32".to_string(), "10.213.4.11/32".to_string()]));
        let empty = WorkerConfig::from_lookup(lookup(&[IMAGE, ("CHELABASE_MEMBER_LOGIN_CIDRS", "")])).unwrap();
        assert_eq!(empty.member_login_cidrs, Some(vec![]), "empty: no address rule");
        assert!(WorkerConfig::from_lookup(lookup(&[IMAGE, ("CHELABASE_MEMBER_LOGIN_CIDRS", "nope")])).is_err());
    }

    /// Edge functions m7: scheduled runs need the functions service's control URL and the
    /// worker's bearer; the bundle sweep reads the bundles bucket with the `functions` identity's
    /// own key files, never the storage identity's.
    #[test]
    fn functions_settings_use_their_own_identity() {
        let none = functions_settings(lookup(&[])).unwrap();
        assert!(none.invoke.is_none() && none.bundles.is_none());
        let dir = tempfile::tempdir().unwrap();
        let file = |name: &str, body: &str| {
            let p = dir.path().join(name);
            std::fs::write(&p, body).unwrap();
            p.display().to_string()
        };
        let token = "w".repeat(40);
        let (token_file, fa, fs, sa, ss) = (
            file("token", &token),
            file("fa", "fn-access"),
            file("fs", "fn-secret"),
            file("sa", "st-access"),
            file("ss", "st-secret"),
        );
        let got = functions_settings(lookup(&[
            ("CHELABASE_FUNCTIONS_INTERNAL_URL", "http://functions-control:8080/"),
            ("CHELABASE_WORKER_FUNCTIONS_TOKEN_FILE", &token_file),
            ("CHELABASE_STORAGE_BACKEND", "seaweedfs"),
            ("CHELABASE_STORAGE_S3_ENDPOINT", "http://seaweedfs:8333"),
            ("CHELABASE_STORAGE_S3_BUCKET", "chelabase-storage"),
            ("CHELABASE_STORAGE_S3_ACCESS_KEY_FILE", &sa),
            ("CHELABASE_STORAGE_S3_SECRET_KEY_FILE", &ss),
            ("CHELABASE_FUNCTIONS_S3_BUCKET", "chelabase-functions"),
            ("CHELABASE_FUNCTIONS_S3_ACCESS_KEY_FILE", &fa),
            ("CHELABASE_FUNCTIONS_S3_SECRET_KEY_FILE", &fs),
        ]))
        .unwrap();
        assert_eq!(
            got.invoke,
            Some(("http://functions-control:8080/".to_string(), token))
        );
        let store = got.bundles.unwrap();
        assert_eq!(store.bucket, "chelabase-functions");
        assert_eq!(store.endpoint.as_deref(), Some("http://seaweedfs:8333"));
        assert_eq!(
            store.credentials,
            chelabase_storage::CredentialSource::KeyFiles {
                access_key_file: fa.into(),
                secret_key_file: fs.into(),
            }
        );
        // The bucket without its own key files is an error, not the storage identity's keys.
        let err = functions_settings(lookup(&[
            ("CHELABASE_STORAGE_S3_ENDPOINT", "http://seaweedfs:8333"),
            ("CHELABASE_STORAGE_S3_ACCESS_KEY_FILE", &sa),
            ("CHELABASE_STORAGE_S3_SECRET_KEY_FILE", &ss),
            ("CHELABASE_FUNCTIONS_S3_BUCKET", "chelabase-functions"),
        ]))
        .unwrap_err();
        assert!(err.to_string().contains("CHELABASE_FUNCTIONS_S3_ACCESS_KEY_FILE"), "{err}");
    }
}
