//! Creating, inspecting and removing computes, and running admin SQL inside them.

use std::future::Future;
use std::io::Read as _;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, bail};
use bollard::Docker;
use bollard::errors::Error as DockerError;
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::models::{
    ContainerCreateBody, ContainerStateStatusEnum, ContainerSummary, ContainerSummaryStateEnum,
    HostConfig, NetworkConnectRequest, NetworkInspect, RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptionsBuilder, DownloadFromContainerOptions,
    InspectNetworkOptions, ListContainersOptions, RemoveContainerOptions, StartContainerOptions,
    UploadToContainerOptions,
};
use futures_util::{StreamExt, TryStreamExt};
use tokio::io::AsyncWriteExt;

use crate::compute::{
    COMPUTE_TOKEN_ENV, ComputeLocator, DockerLocator, ENDPOINT_LABEL, STACK_LABEL,
    compute_ctl_args, container_name, list_filters,
};
use crate::compute_tls::{ComputeCa, IssuedCert, TLS_UPLOAD_ROOT, tls_files_tar};

/// The largest file [`DockerBackend::read_file`] reads (certificates are a few KiB).
const READ_FILE_LIMIT: u64 = 1024 * 1024;

const ADMIN_SQL_TIMEOUT: Duration = Duration::from_secs(30);

/// Per-call options of [`ComputeBackend::admin_sql_with`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlOpts {
    /// The whole call (connect, run, read output) gives up after this.
    pub timeout: Duration,
    /// `PGAPPNAME` of the psql session, so the caller can find and cancel its backend.
    pub app_name: Option<String>,
    /// `VERBOSITY=default` instead of `terse`: errors keep `LINE`, `DETAIL` and `HINT`. Only for
    /// scripts that carry no secret (a SCRAM verifier would show in the `LINE` excerpt).
    pub verbose_errors: bool,
    /// The role psql connects as (`-U`); `None` is `cloud_admin`. Step 6g's merge scripts run in
    /// the owner's own session (E15): the compute trusts local connections, so no password.
    pub user: Option<String>,
}

impl SqlOpts {
    /// What `admin_sql` runs with: 30 s, no application name, terse errors, `cloud_admin`.
    pub fn admin_default() -> Self {
        Self {
            timeout: ADMIN_SQL_TIMEOUT,
            app_name: None,
            verbose_errors: false,
            user: None,
        }
    }
}
/// The error of an [`ComputeBackend::admin_sql_with`] call that outlasted its timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlTimedOut {
    pub endpoint: String,
    pub after: Duration,
}

impl std::fmt::Display for SqlTimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "psql in compute {} timed out after {:?}", self.endpoint, self.after)
    }
}

impl std::error::Error for SqlTimedOut {}

/// Whether `err` is (or wraps) an admin call's timeout ([`SqlTimedOut`]).
pub fn is_timeout(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.is::<SqlTimedOut>())
}

const MEMORY_BYTES: i64 = 2 * 1024 * 1024 * 1024;
const NANO_CPUS: i64 = 1_000_000_000;

/// Why a compute that exists isn't serving.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotRunning {
    /// It stopped and stays stopped.
    Exited,
    /// It failed and can't be resumed as it is.
    Crashed,
    /// Anything else the backend reports, in its own words (e.g. "restarting").
    Other(String),
}

impl std::fmt::Display for NotRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exited => f.write_str("exited"),
            Self::Crashed => f.write_str("crashed"),
            Self::Other(other) => f.write_str(other),
        }
    }
}

/// A compute's state, as far as callers care.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComputeStatus {
    Running,
    /// The compute exists but isn't serving.
    NotRunning(NotRunning),
}

/// How computes are run, whatever the backend.
#[derive(Debug, Clone)]
pub struct ComputeSettings {
    pub image: String,
    pub control_plane_uri: String,
    /// The stack the computes belong to: in their names and labels, and the only ones listed.
    pub stack: String,
    /// The CA that issues each compute's server certificate at create. `None` where computes
    /// are never created (the orchestrator): [`ComputeBackend::create`] then fails.
    pub ca: Option<Arc<ComputeCa>>,
}

/// The Docker backend's own settings.
#[derive(Debug, Clone)]
pub struct DockerSettings {
    /// The network computes run on.
    pub network: String,
    /// The network the orchestrator reaches computes' Postgres on (member logins come from
    /// its address there). A compute joins it after create, before start; when it is
    /// `network` itself there is nothing to join.
    pub control_network: String,
    /// The networks computes ran on before they became internal
    /// (`CHELABASE_LEGACY_COMPUTE_NETWORKS`): the worker removes each once nothing is attached
    /// to it. Only these names are ever removed.
    pub legacy_networks: Vec<String>,
    /// The resolver computes are created with (`CHELABASE_COMPUTE_EGRESS_DNS`, the
    /// compute-egress service); `None` leaves Docker's default.
    pub egress_dns: Option<IpAddr>,
}

/// The option Docker reads for a bridge's IPv4 gateway mode.
pub const GATEWAY_MODE_IPV4: &str = "com.docker.network.bridge.gateway_mode_ipv4";
/// The option Docker reads for a bridge's IPv6 gateway mode.
pub const GATEWAY_MODE_IPV6: &str = "com.docker.network.bridge.gateway_mode_ipv6";

/// A network computes may run on: Docker `internal` (no default route) and both gateway modes
/// `isolated` (the bridge has no host address). `inspect` is what Docker answered for `name`;
/// its own name must be `name` exactly, since Docker also resolves an id prefix.
pub fn check_compute_network(name: &str, inspect: &NetworkInspect) -> anyhow::Result<()> {
    if inspect.name.as_deref() != Some(name) {
        bail!(
            "network {name}: Docker answered with network {:?}, not one of that name",
            inspect.name.as_deref().unwrap_or("")
        );
    }
    if inspect.internal != Some(true) {
        bail!("network {name} is not internal: computes run only on internal networks");
    }
    for option in [GATEWAY_MODE_IPV4, GATEWAY_MODE_IPV6] {
        let mode = inspect.options.as_ref().and_then(|o| o.get(option));
        if mode.map(String::as_str) != Some("isolated") {
            bail!("network {name} lacks {option}=isolated: computes run only on isolated networks");
        }
    }
    Ok(())
}

/// A network by name and the id Docker gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkRef {
    pub name: String,
    pub id: String,
}

/// One of this stack's computes and the networks it is attached to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachedCompute {
    pub endpoint_id: String,
    /// The container's id: a removal is by id, so a container created since under the same
    /// name is never the one removed.
    pub container_id: String,
    /// The container exists but was never started: a create is in flight (the control network
    /// is joined after the container exists).
    pub mid_create: bool,
    /// Its attachments: each network's name and the id it was attached by (empty when Docker
    /// doesn't say).
    pub networks: Vec<NetworkRef>,
}

/// The networks computes should be on, and where each of this stack's computes is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputeNetworks {
    /// The compute and control networks (one entry when they are the same network).
    pub expected: Vec<NetworkRef>,
    pub computes: Vec<AttachedCompute>,
}

/// A listed legacy network as Docker has it now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyNetwork {
    /// No network of that name.
    Missing { name: String },
    /// It exists, with these containers attached (their names).
    Present {
        network: NetworkRef,
        members: Vec<String>,
    },
    /// Docker couldn't say.
    Unreadable { name: String, error: String },
}

/// What removing a legacy network came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkRemoval {
    Removed,
    /// It was already gone.
    Gone,
    /// Docker refused: something is still attached.
    Occupied,
}

/// The env variable that picks the compute backend.
pub const BACKEND_ENV: &str = "CHELABASE_COMPUTE_BACKEND";

/// Which compute backend a service runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    Docker,
}

impl BackendKind {
    /// The backend `CHELABASE_COMPUTE_BACKEND` names in the process environment.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Unset or empty is `docker`; any other value that isn't a backend is an error.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        match lookup(BACKEND_ENV).as_deref().map(str::trim) {
            None | Some("") | Some("docker") => Ok(Self::Docker),
            Some(other) => bail!("{BACKEND_ENV}={other:?} is not a compute backend; accepted values: docker"),
        }
    }
}

/// The compute backend a service runs, picked at startup.
pub enum Backend {
    Docker(DockerBackend),
}

impl Backend {
    /// Nothing is contacted yet. `docker` is used only by the Docker backend.
    pub fn connect(
        kind: BackendKind,
        settings: ComputeSettings,
        docker: DockerSettings,
    ) -> anyhow::Result<Self> {
        match kind {
            BackendKind::Docker => Ok(Self::Docker(DockerBackend::connect(settings, docker)?)),
        }
    }
}

/// Lifecycle of computes plus admin SQL inside them.
pub trait ComputeBackend: ComputeLocator {
    fn create(
        &self,
        endpoint_id: &str,
        compute_token: &str,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn delete(&self, endpoint_id: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
    /// The contents of the file at `path` in the compute (at most 1 MiB).
    fn read_file(
        &self,
        endpoint_id: &str,
        path: &str,
    ) -> impl Future<Output = anyhow::Result<Vec<u8>>> + Send;
    /// Writes `server` and the CA certificate into the compute's TLS directory; compute_ctl
    /// notices the new certificate and reloads Postgres with it.
    fn upload_tls(
        &self,
        endpoint_id: &str,
        server: &IssuedCert,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn status(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = anyhow::Result<Option<ComputeStatus>>> + Send;
    /// Endpoint ids of every one of this stack's computes, running or not.
    fn list(&self) -> impl Future<Output = anyhow::Result<Vec<String>>> + Send;
    /// Endpoint ids of this stack's computes that exist but aren't serving: stopped, failed or
    /// in a crash loop. Never one that is mid-create or mid-removal, or is serving. A listing
    /// that fails is an error, never an empty list.
    fn list_not_running(&self) -> impl Future<Output = anyhow::Result<Vec<String>>> + Send;
    /// The compute and control networks by id, and every one of this stack's computes with
    /// its attachments. Only containers carrying both the endpoint label and this stack's
    /// label are listed. A failed listing or inspect is an error, never an empty answer.
    fn compute_networks(&self) -> impl Future<Output = anyhow::Result<ComputeNetworks>> + Send;
    /// Each configured legacy network as Docker has it now, in the configured order.
    fn legacy_networks(&self) -> impl Future<Output = Vec<LegacyNetwork>> + Send;
    /// Removes the compute container `container_id` (forced). One already gone is fine.
    fn remove_compute_container(
        &self,
        endpoint_id: &str,
        container_id: &str,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
    /// Removes the network `network` by its id; Docker refuses while anything is attached.
    fn remove_network(
        &self,
        network: &NetworkRef,
    ) -> impl Future<Output = anyhow::Result<NetworkRemoval>> + Send;
    /// Runs `sql` (stdin) with psql inside the compute, as `opts.user` (`cloud_admin` when
    /// `None`); returns stdout. A call that outlasts `opts.timeout` fails with [`SqlTimedOut`]
    /// (tell it by [`is_timeout`], never by the text), and the psql inside the compute keeps
    /// running: a caller that must stop its statement cancels the backend by its
    /// `application_name` (`opts.app_name`).
    fn admin_sql_with(
        &self,
        endpoint_id: &str,
        database: &str,
        sql: &str,
        opts: &SqlOpts,
    ) -> impl Future<Output = anyhow::Result<String>> + Send;
    /// [`ComputeBackend::admin_sql_with`] with [`SqlOpts::admin_default`].
    fn admin_sql(
        &self,
        endpoint_id: &str,
        database: &str,
        sql: &str,
    ) -> impl Future<Output = anyhow::Result<String>> + Send {
        async move {
            self.admin_sql_with(endpoint_id, database, sql, &SqlOpts::admin_default())
                .await
        }
    }
}

/// The container to create for an endpoint. The token goes in the environment, never argv.
/// Nothing is published on the host: the stack reaches a compute on the compute network.
///
/// Its resolver is `docker.egress_dns` when set. It runs with no capability and no privilege
/// gain: Postgres and compute_ctl run as the image's own unprivileged user, so nothing in it
/// needs one, and without `NET_RAW` it can't send frames for an address that isn't its own on
/// the shared bridges.
pub fn create_body(
    endpoint_id: &str,
    compute_token: &str,
    settings: &ComputeSettings,
    docker: &DockerSettings,
) -> ContainerCreateBody {
    ContainerCreateBody {
        image: Some(settings.image.clone()),
        entrypoint: Some(vec!["/usr/local/bin/compute_ctl".to_string()]),
        cmd: Some(compute_ctl_args(endpoint_id, &settings.control_plane_uri)),
        env: Some(vec![format!("{COMPUTE_TOKEN_ENV}={compute_token}")]),
        labels: Some(
            [
                (ENDPOINT_LABEL.to_string(), endpoint_id.to_string()),
                (STACK_LABEL.to_string(), settings.stack.clone()),
            ]
            .into(),
        ),
        host_config: Some(HostConfig {
            network_mode: Some(docker.network.clone()),
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                maximum_retry_count: None,
            }),
            memory: Some(MEMORY_BYTES),
            nano_cpus: Some(NANO_CPUS),
            dns: docker.egress_dns.map(|ip| vec![ip.to_string()]),
            dns_options: docker.egress_dns.map(|_| resolver_options()),
            cap_drop: Some(vec!["ALL".to_string()]),
            security_opt: Some(vec!["no-new-privileges:true".to_string()]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// glibc resolver options for a compute with a pinned resolver. Docker's embedded resolver forwards
/// to the pinned address, and with glibc's defaults (5 s, 2 attempts) a lookup of an outside name
/// in the default install, where nothing listens there, takes about 32 s to fail; with these it
/// takes about 4 s. A live egress answers in milliseconds, so allowed names are unaffected.
fn resolver_options() -> Vec<String> {
    vec!["timeout:1".to_string(), "attempts:1".to_string()]
}

/// psql as cloud_admin against `database` on the compute's own Postgres, with terse errors
/// (no statement excerpt, so a verifier in the script never reaches the error). A plain identifier
/// goes in as is; any other name as a quoted conninfo (`dbname='…'`), so psql never reads it
/// as options or connection parameters. Refused: an empty name, one over 63 bytes (Postgres'
/// limit) or one with a NUL (it can't be an argument).
pub fn admin_psql_command(database: &str) -> anyhow::Result<Vec<String>> {
    admin_psql_command_with(database, &SqlOpts::admin_default())
}

/// Refuses an admin script with a NUL byte before it is sent: Postgres has no NUL in a string
/// or an identifier, so the quoting helpers can't carry one, and what psql would make of the
/// rest of such a script isn't ours to rely on.
pub fn refuse_nul(sql: &str) -> anyhow::Result<()> {
    if sql.contains('\0') {
        bail!("admin SQL with a NUL byte is refused");
    }
    Ok(())
}

/// [`admin_psql_command`] with `opts.verbose_errors` and `opts.user` honoured.
pub fn admin_psql_command_with(database: &str, opts: &SqlOpts) -> anyhow::Result<Vec<String>> {
    if database.is_empty() || database.len() > 63 || database.contains('\0') {
        bail!("database name {database:?} can't be passed to psql");
    }
    let plain = database
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_');
    let target = if plain {
        database.to_string()
    } else {
        format!(
            "dbname='{}'",
            database.replace('\\', "\\\\").replace('\'', "\\'")
        )
    };
    let mut cmd: Vec<String> = [
        "psql",
        "-X",
        "-q",
        "-A",
        "-t",
        "-v",
        "ON_ERROR_STOP=1",
        // No `LINE n:` excerpt in errors: scripts can carry SCRAM verifiers.
        "-v",
        if opts.verbose_errors {
            "VERBOSITY=default"
        } else {
            "VERBOSITY=terse"
        },
        "-h",
    ]
    .map(String::from)
    .into();
    // `cloud_admin` keeps today's `localhost`. Another role (E15: the owner's own session)
    // connects over IPv4 loopback, which the compute's pg_hba trusts; `localhost` may resolve
    // to `::1`.
    cmd.push(
        if opts.user.is_some() {
            "127.0.0.1"
        } else {
            "localhost"
        }
        .to_string(),
    );
    cmd.push("-U".to_string());
    cmd.push(opts.user.as_deref().unwrap_or("cloud_admin").to_string());
    cmd.push("-d".to_string());
    cmd.push(target);
    // The script still comes on stdin; read as a file, psql prefixes each error with
    // `psql:<stdin>:<line>:`, which the merge maps to the failing step.
    if opts.verbose_errors {
        cmd.push("-f".to_string());
        cmd.push("-".to_string());
    }
    Ok(cmd)
}

fn is_status(err: &DockerError, code: u16) -> bool {
    matches!(err, DockerError::DockerResponseServerError { status_code, .. } if *status_code == code)
}

/// Docker's answer to a container create whose image isn't on the host.
fn is_missing_image(err: &DockerError) -> bool {
    matches!(err, DockerError::DockerResponseServerError { status_code: 404, message }
        if message.contains("No such image"))
}

/// Removes container `name` (or id), forced, with its anonymous volumes; one already gone is
/// fine.
async fn remove_container(docker: &Docker, name: &str) -> Result<(), DockerError> {
    let options = RemoveContainerOptions {
        force: true,
        v: true,
        link: false,
    };
    match docker.remove_container(name, Some(options)).await {
        Err(e) if is_status(&e, 404) => Ok(()),
        other => other,
    }
}

/// R4: removes a container `create` made if `create`'s future is dropped (a caller's timeout,
/// a cancelled job) before it returns. It can't wait in `Drop`, so the removal is spawned on
/// the runtime, best effort; [`RemoveOnDrop::disarm`] before every return.
struct RemoveOnDrop {
    /// `None` once disarmed.
    docker: Option<Docker>,
    id: String,
    name: String,
}

impl RemoveOnDrop {
    fn disarm(&mut self) {
        self.docker = None;
    }
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let Some(docker) = self.docker.take() else {
            return;
        };
        let (id, name) = (std::mem::take(&mut self.id), std::mem::take(&mut self.name));
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(container = %name, id = %id, "compute create dropped outside a runtime; its container stays");
            return;
        };
        tracing::info!(container = %name, id = %id, "compute create dropped; removing its container");
        runtime.spawn(async move {
            if let Err(e) = remove_container(&docker, &id).await {
                tracing::warn!(container = %name, id = %id, "removing the container of a dropped create: {e}");
            }
        });
    }
}

/// Environment variables for admin_sql execution.
#[cfg(test)]
fn admin_exec_env() -> Vec<String> {
    admin_exec_env_with(&SqlOpts::admin_default())
}

/// [`admin_exec_env`] plus `PGAPPNAME` when `opts` names the session.
///
/// Every session reads its script as UTF8 with `standard_conforming_strings = on`, and
/// `cloud_admin` resolves names in `pg_catalog` only: startup options come after the database's
/// and the role's own settings (`ALTER DATABASE … SET`, `ALTER ROLE … SET`), so no script
/// depends on those. The owner's session keeps the database's search path: merge user steps
/// resolve the tenant's objects through it. None of the values has a space, so `PGOPTIONS`
/// needs no escaping.
///
/// DDL forwarding is turned off only for `cloud_admin`: `neon.forward_ddl` is superuser-only,
/// so another role's connection would be refused (`permission denied to set parameter`). The
/// owner's merge sessions (E15) run no role or database DDL (P8 refuses it), so forwarding has
/// nothing to do.
fn admin_exec_env_with(opts: &SqlOpts) -> Vec<String> {
    let mut options = "PGOPTIONS=-c standard_conforming_strings=on".to_string();
    if opts.user.is_none() {
        options.push_str(" -c search_path=pg_catalog,pg_temp -c neon.forward_ddl=off");
    }
    let mut env = vec![
        "PGCONNECT_TIMEOUT=5".to_string(),
        "PGCLIENTENCODING=UTF8".to_string(),
        options,
    ];
    if let Some(name) = &opts.app_name {
        // Postgres keeps 63 bytes of an application_name and turns other than printable ASCII
        // into `?`: a longer or odder name wouldn't find its backend to cancel.
        debug_assert!(
            name.len() <= 63 && name.bytes().all(|b| b.is_ascii_graphic()),
            "application_name {name:?} isn't at most 63 printable ASCII bytes"
        );
        env.push(format!("PGAPPNAME={name}"));
    }
    env
}

/// The Docker container states [`ComputeBackend::list_not_running`] lists: `exited`, `dead` and
/// `restarting` (a crash loop under `unless-stopped`). Never `created` (an in-flight create
/// passes through it), `running`, `paused` or `removing`.
const DOCKER_NOT_RUNNING: [bollard::models::ContainerSummaryStateEnum; 3] = {
    use bollard::models::ContainerSummaryStateEnum as State;
    [State::EXITED, State::DEAD, State::RESTARTING]
};

fn docker_not_running_listed(state: bollard::models::ContainerSummaryStateEnum) -> bool {
    DOCKER_NOT_RUNNING.contains(&state)
}

/// Docker's container state as a [`ComputeStatus`].
fn docker_status(state: ContainerStateStatusEnum) -> ComputeStatus {
    match state {
        ContainerStateStatusEnum::RUNNING => ComputeStatus::Running,
        ContainerStateStatusEnum::EXITED => ComputeStatus::NotRunning(NotRunning::Exited),
        ContainerStateStatusEnum::DEAD => ComputeStatus::NotRunning(NotRunning::Crashed),
        other => ComputeStatus::NotRunning(NotRunning::Other(other.to_string())),
    }
}

/// The names of the containers attached to a network.
fn network_members(inspect: &NetworkInspect) -> Vec<String> {
    let mut members: Vec<String> = inspect
        .containers
        .iter()
        .flatten()
        .map(|(id, c)| c.name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| id.clone()))
        .collect();
    members.sort();
    members
}

/// A listed container as an [`AttachedCompute`] of `stack`: `None` unless it carries the
/// endpoint label and `stack`'s label (checked here too, so an engine that ignores a filter
/// never offers another container) and has an id.
fn attached_compute(container: ContainerSummary, stack: &str) -> Option<AttachedCompute> {
    let labels = container.labels?;
    if labels.get(STACK_LABEL).map(String::as_str) != Some(stack) {
        return None;
    }
    let endpoint_id = labels.get(ENDPOINT_LABEL).filter(|e| !e.is_empty())?.clone();
    let container_id = container.id.filter(|id| !id.is_empty())?;
    let mut networks: Vec<NetworkRef> = container
        .network_settings
        .and_then(|n| n.networks)
        .into_iter()
        .flatten()
        .map(|(name, endpoint)| NetworkRef {
            name,
            id: endpoint.network_id.unwrap_or_default(),
        })
        .collect();
    networks.sort_by(|a, b| a.name.cmp(&b.name));
    Some(AttachedCompute {
        endpoint_id,
        container_id,
        mid_create: container.state == Some(ContainerSummaryStateEnum::CREATED),
        networks,
    })
}

/// [`ComputeBackend`] over the Docker Engine API.
pub struct DockerBackend {
    locator: DockerLocator,
    settings: ComputeSettings,
    docker_settings: DockerSettings,
}

impl DockerBackend {
    /// The names of the containers, stopped ones included, that reference network `id`.
    /// Docker's network inspect lists only active endpoints, and the engine deletes a network
    /// that only stopped containers reference, which leaves them unable to start.
    async fn stopped_members(&self, id: &str) -> anyhow::Result<Vec<String>> {
        let filters = std::collections::HashMap::from([("network".to_string(), vec![id.to_string()])]);
        let containers = self
            .docker()
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters: Some(filters),
                ..Default::default()
            }))
            .await
            .with_context(|| format!("listing the containers attached to network {id}"))?;
        Ok(containers
            .into_iter()
            .map(|c| {
                c.names
                    .into_iter()
                    .flatten()
                    .next()
                    .map(|n| n.trim_start_matches('/').to_string())
                    .filter(|n| !n.is_empty())
                    .or(c.id)
                    .unwrap_or_default()
            })
            .collect())
    }

    /// Nothing is contacted yet.
    pub fn connect(settings: ComputeSettings, docker: DockerSettings) -> anyhow::Result<Self> {
        Ok(Self {
            locator: DockerLocator::connect(&docker.network, &settings.stack)?,
            settings,
            docker_settings: docker,
        })
    }

    /// A backend over an existing Docker client (tests aim one at a mock engine).
    pub fn with_docker(
        settings: ComputeSettings,
        docker_settings: DockerSettings,
        docker: Docker,
    ) -> Self {
        Self {
            locator: DockerLocator::with_docker(docker, &docker_settings.network, &settings.stack),
            settings,
            docker_settings,
        }
    }

    fn docker(&self) -> &Docker {
        &self.locator.docker
    }

    /// Pulls the compute image, waiting out Docker's progress stream (a failed pull is a line
    /// of it).
    async fn pull_image(&self) -> anyhow::Result<()> {
        let image = &self.settings.image;
        tracing::info!(image = %image, "compute image missing; pulling it");
        self.docker()
            .create_image(
                Some(CreateImageOptionsBuilder::new().from_image(image).build()),
                None,
                None,
            )
            .try_for_each(|_| async { Ok(()) })
            .await
            .with_context(|| format!("pulling {image}"))
    }

    fn ca(&self) -> anyhow::Result<&ComputeCa> {
        self.settings
            .ca
            .as_deref()
            .context("no compute CA configured: this backend can't issue compute certificates")
    }

    /// Extracts `tar` at [`TLS_UPLOAD_ROOT`] in container `name` (created or running).
    async fn upload_tls_tar(&self, name: &str, tar: Vec<u8>) -> anyhow::Result<()> {
        self.docker()
            .upload_to_container(
                name,
                Some(UploadToContainerOptions {
                    path: TLS_UPLOAD_ROOT.to_string(),
                    ..Default::default()
                }),
                bollard::body_full(tar.into()),
            )
            .await
            .with_context(|| format!("uploading the TLS files into {name}"))
    }

    async fn upload_tls_inner(&self, endpoint_id: &str, server: &IssuedCert) -> anyhow::Result<()> {
        let tar = tls_files_tar(server, self.ca()?.cert_pem())?;
        let name = container_name(&self.settings.stack, endpoint_id);
        self.upload_tls_tar(&name, tar).await
    }

    async fn read_file_inner(&self, endpoint_id: &str, path: &str) -> anyhow::Result<Vec<u8>> {
        let name = container_name(&self.settings.stack, endpoint_id);
        let mut archive = Vec::new();
        let mut stream = self.docker().download_from_container(
            &name,
            Some(DownloadFromContainerOptions {
                path: path.to_string(),
            }),
        );
        while let Some(chunk) = stream
            .try_next()
            .await
            .with_context(|| format!("reading {path} from {name}"))?
        {
            archive.extend_from_slice(&chunk);
            // The archive carries headers and padding besides the file.
            if archive.len() as u64 > READ_FILE_LIMIT + 64 * 1024 {
                bail!("{path} in {name} is over {READ_FILE_LIMIT} bytes");
            }
        }
        let mut entries = tar::Archive::new(archive.as_slice());
        for entry in entries
            .entries()
            .with_context(|| format!("reading the archive of {path} from {name}"))?
        {
            let entry = entry.with_context(|| format!("reading the archive of {path} from {name}"))?;
            if entry.header().entry_type() != tar::EntryType::Regular {
                continue;
            }
            let mut body = Vec::new();
            entry
                .take(READ_FILE_LIMIT + 1)
                .read_to_end(&mut body)
                .with_context(|| format!("reading {path} from {name}"))?;
            if body.len() as u64 > READ_FILE_LIMIT {
                bail!("{path} in {name} is over {READ_FILE_LIMIT} bytes");
            }
            return Ok(body);
        }
        bail!("{path} in {name} isn't a regular file")
    }

    async fn remove(&self, name: &str) -> Result<(), DockerError> {
        remove_container(self.docker(), name).await
    }

    /// The network named exactly `name`; `None` when there is none. Docker also answers for an
    /// id prefix: an answer for a network of another name counts as none.
    async fn inspect_network(&self, name: &str) -> anyhow::Result<Option<NetworkInspect>> {
        match self
            .docker()
            .inspect_network(name, None::<InspectNetworkOptions>)
            .await
        {
            Ok(inspect) if inspect.name.as_deref() == Some(name) => Ok(Some(inspect)),
            Ok(_) => Ok(None),
            Err(e) if is_status(&e, 404) => Ok(None),
            Err(e) => Err(anyhow::Error::new(e).context(format!("can't inspect network {name}"))),
        }
    }

    /// The compute network, then the control network unless it is the same one.
    fn configured_networks(&self) -> Vec<&str> {
        let DockerSettings {
            network,
            control_network,
            ..
        } = &self.docker_settings;
        if control_network == network {
            vec![network]
        } else {
            vec![network, control_network]
        }
    }

    /// Each configured network, inspected now, that passes [`check_compute_network`]; the
    /// first that doesn't (or can't be read) is the error.
    async fn expected_networks(&self) -> anyhow::Result<Vec<NetworkRef>> {
        let mut expected = Vec::new();
        for name in self.configured_networks() {
            let inspect = self
                .inspect_network(name)
                .await?
                .with_context(|| format!("network {name} doesn't exist"))?;
            check_compute_network(name, &inspect)?;
            let id = inspect
                .id
                .filter(|id| !id.is_empty())
                .with_context(|| format!("network {name}: Docker gave no network id"))?;
            expected.push(NetworkRef {
                name: name.to_string(),
                id,
            });
        }
        Ok(expected)
    }

    async fn run_exec(
        &self,
        endpoint_id: &str,
        cmd: Vec<String>,
        env: Vec<String>,
        sql: &str,
    ) -> anyhow::Result<String> {
        let docker = self.docker();
        let name = container_name(&self.settings.stack, endpoint_id);
        let exec = docker
            .create_exec(
                &name,
                CreateExecOptions {
                    attach_stdin: Some(true),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    user: Some("postgres".to_string()),
                    env: Some(env),
                    cmd: Some(cmd),
                    ..Default::default()
                },
            )
            .await
            .with_context(|| format!("creating exec in {name}"))?;
        let started = docker
            .start_exec(&exec.id, None)
            .await
            .with_context(|| format!("starting exec in {name}"))?;
        let StartExecResults::Attached {
            mut output,
            mut input,
        } = started
        else {
            bail!("exec in {name} started detached");
        };
        input
            .write_all(sql.as_bytes())
            .await
            .with_context(|| format!("writing SQL to {name}"))?;
        // EOF, so psql runs the script and exits.
        input
            .shutdown()
            .await
            .with_context(|| format!("closing stdin of {name}"))?;
        drop(input);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        while let Some(chunk) = output.next().await {
            match chunk.with_context(|| format!("reading output of {name}"))? {
                bollard::container::LogOutput::StdErr { message } => {
                    stderr.extend_from_slice(&message)
                }
                bollard::container::LogOutput::StdOut { message } => {
                    stdout.extend_from_slice(&message)
                }
                _ => {}
            }
        }
        let inspect = docker
            .inspect_exec(&exec.id)
            .await
            .with_context(|| format!("inspecting exec in {name}"))?;
        match inspect.exit_code {
            Some(0) => Ok(String::from_utf8_lossy(&stdout).into_owned()),
            code => bail!(
                "psql in compute {endpoint_id} exited with {}: {}",
                code.map_or_else(|| "unknown".to_string(), |c| c.to_string()),
                String::from_utf8_lossy(&stderr).trim()
            ),
        }
    }
}

impl ComputeLocator for DockerBackend {
    async fn address(&self, endpoint_id: &str) -> anyhow::Result<Option<IpAddr>> {
        self.locator.address(endpoint_id).await
    }

    async fn control_addr(&self, endpoint_id: &str) -> anyhow::Result<Option<SocketAddr>> {
        self.locator.control_addr(endpoint_id).await
    }

    async fn postgres_addr(&self, endpoint_id: &str) -> anyhow::Result<Option<SocketAddr>> {
        self.locator.postgres_addr(endpoint_id).await
    }
}

impl ComputeBackend for DockerBackend {
    /// Create → join the control network → upload the TLS files → start, so compute_ctl finds
    /// its certificate at once.
    async fn create(&self, endpoint_id: &str, compute_token: &str) -> anyhow::Result<()> {
        let name = container_name(&self.settings.stack, endpoint_id);
        // Issued before anything is created: without a CA no compute is made.
        let ca = self.ca()?;
        let server = ca.issue_server(endpoint_id, SystemTime::now())?;
        let tls = tls_files_tar(&server, ca.cert_pem())?;
        // On every create, never cached: a network can be recreated under the same name. A
        // network that isn't internal and isolated, or can't be read, refuses the create
        // before any container exists.
        self.expected_networks()
            .await
            .with_context(|| format!("refusing to create compute {endpoint_id}"))?;
        let body = create_body(endpoint_id, compute_token, &self.settings, &self.docker_settings);
        let create = || {
            self.docker().create_container(
                Some(CreateContainerOptions {
                    name: Some(name.clone()),
                    ..Default::default()
                }),
                body.clone(),
            )
        };
        // Compose pulls only the images of its services, so a fresh install has never pulled
        // the compute image: pull it once and create again.
        let created = match create().await {
            Err(e) if is_missing_image(&e) => {
                self.pull_image().await?;
                create().await
            }
            other => other,
        };
        let created = match created {
            Err(e) if is_status(&e, 409) => {
                self.remove(&name)
                    .await
                    .with_context(|| format!("removing old container {name}"))?;
                create().await
            }
            other => other,
        }
        .with_context(|| format!("creating container {name}"))?;
        // From here the container exists: if this future is dropped before it returns, the
        // guard removes it (R4). Removed by id, so a later create of the same name is safe.
        // Not covered: a future dropped while `create_container` itself is in flight (Docker
        // may finish the create after the request is abandoned), or during the name-conflict
        // path's removal or second create. Both windows are tiny (one local API round trip),
        // and a container left that way is never started, so it serves nothing; the next
        // `create` of the endpoint replaces it through the name-conflict path.
        let mut guard = RemoveOnDrop {
            docker: Some(self.docker().clone()),
            id: created.id.clone(),
            name: name.clone(),
        };
        // Everything after the container exists: a failure removes it again (best effort; the
        // step's own error is what the caller sees), so no stopped container is left behind.
        let steps = async {
            if self.docker_settings.control_network != self.docker_settings.network {
                self.docker()
                    .connect_network(
                        &self.docker_settings.control_network,
                        NetworkConnectRequest {
                            container: created.id.clone(),
                            endpoint_config: None,
                        },
                    )
                    .await
                    .with_context(|| {
                        format!(
                            "connecting {name} to network {}",
                            self.docker_settings.control_network
                        )
                    })?;
            }
            self.upload_tls_tar(&created.id, tls).await?;
            self.docker()
                .start_container(&created.id, None::<StartContainerOptions>)
                .await
                .with_context(|| format!("starting container {name}"))
        };
        if let Err(e) = steps.await {
            let _ = self.remove(&created.id).await;
            // Disarmed only once the removal ran: a drop during it still removes.
            guard.disarm();
            return Err(e);
        }
        guard.disarm();
        Ok(())
    }

    async fn read_file(&self, endpoint_id: &str, path: &str) -> anyhow::Result<Vec<u8>> {
        self.read_file_inner(endpoint_id, path).await
    }

    async fn upload_tls(&self, endpoint_id: &str, server: &IssuedCert) -> anyhow::Result<()> {
        self.upload_tls_inner(endpoint_id, server).await
    }

    async fn delete(&self, endpoint_id: &str) -> anyhow::Result<()> {
        let name = container_name(&self.settings.stack, endpoint_id);
        self.remove(&name)
            .await
            .with_context(|| format!("removing container {name}"))
    }

    async fn status(&self, endpoint_id: &str) -> anyhow::Result<Option<ComputeStatus>> {
        Ok(self.locator.inspect(endpoint_id).await?.map(|inspect| {
            match inspect.state.and_then(|s| s.status) {
                Some(state) => docker_status(state),
                None => ComputeStatus::NotRunning(NotRunning::Other("unknown".to_string())),
            }
        }))
    }

    async fn list(&self) -> anyhow::Result<Vec<String>> {
        let containers = self
            .docker()
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters: Some(list_filters(&self.settings.stack)),
                ..Default::default()
            }))
            .await
            .context("listing compute containers")?;
        Ok(containers
            .into_iter()
            .filter_map(|c| c.labels?.remove(ENDPOINT_LABEL))
            .collect())
    }

    async fn list_not_running(&self) -> anyhow::Result<Vec<String>> {
        let mut filters = list_filters(&self.settings.stack);
        filters.insert(
            "status".to_string(),
            DOCKER_NOT_RUNNING.iter().map(|s| s.to_string()).collect(),
        );
        let containers = self
            .docker()
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters: Some(filters),
                ..Default::default()
            }))
            .await
            .context("listing compute containers that aren't running")?;
        // Checked again here, so an engine that ignores a filter never makes a running (or
        // still being created) compute look stopped.
        Ok(containers
            .into_iter()
            .filter(|c| c.state.is_some_and(docker_not_running_listed))
            .filter_map(|c| c.labels?.remove(ENDPOINT_LABEL))
            .collect())
    }

    async fn compute_networks(&self) -> anyhow::Result<ComputeNetworks> {
        let expected = self.expected_networks().await?;
        let containers = self
            .docker()
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters: Some(list_filters(&self.settings.stack)),
                ..Default::default()
            }))
            .await
            .context("listing compute containers")?;
        let computes = containers
            .into_iter()
            .filter_map(|c| attached_compute(c, &self.settings.stack))
            .collect();
        Ok(ComputeNetworks { expected, computes })
    }

    async fn legacy_networks(&self) -> Vec<LegacyNetwork> {
        let mut found = Vec::new();
        for name in &self.docker_settings.legacy_networks {
            found.push(match self.inspect_network(name).await {
                Ok(None) => LegacyNetwork::Missing { name: name.clone() },
                Ok(Some(inspect)) => match inspect.id.clone().filter(|id| !id.is_empty()) {
                    Some(id) => match self.stopped_members(&id).await {
                        Ok(stopped) => {
                            let mut members = network_members(&inspect);
                            for m in stopped {
                                if !members.contains(&m) {
                                    members.push(m);
                                }
                            }
                            members.sort();
                            LegacyNetwork::Present {
                                network: NetworkRef {
                                    name: name.clone(),
                                    id,
                                },
                                members,
                            }
                        }
                        // Not knowing a network is empty keeps it.
                        Err(e) => LegacyNetwork::Unreadable {
                            name: name.clone(),
                            error: format!("{e:#}"),
                        },
                    },
                    None => LegacyNetwork::Unreadable {
                        name: name.clone(),
                        error: "Docker gave no network id".to_string(),
                    },
                },
                Err(e) => LegacyNetwork::Unreadable {
                    name: name.clone(),
                    error: format!("{e:#}"),
                },
            });
        }
        found
    }

    async fn remove_compute_container(
        &self,
        endpoint_id: &str,
        container_id: &str,
    ) -> anyhow::Result<()> {
        self.remove(container_id)
            .await
            .with_context(|| format!("removing container {container_id} of compute {endpoint_id}"))
    }

    async fn remove_network(&self, network: &NetworkRef) -> anyhow::Result<NetworkRemoval> {
        match self.docker().remove_network(&network.id).await {
            Ok(()) => Ok(NetworkRemoval::Removed),
            Err(e) if is_status(&e, 404) => Ok(NetworkRemoval::Gone),
            // Docker refuses a network with active endpoints.
            Err(e) if is_status(&e, 403) => Ok(NetworkRemoval::Occupied),
            Err(e) => Err(anyhow::Error::new(e)
                .context(format!("removing network {} ({})", network.name, network.id))),
        }
    }

    async fn admin_sql_with(
        &self,
        endpoint_id: &str,
        database: &str,
        sql: &str,
        opts: &SqlOpts,
    ) -> anyhow::Result<String> {
        refuse_nul(sql)?;
        let cmd = admin_psql_command_with(database, opts)?;
        let env = admin_exec_env_with(opts);
        tokio::time::timeout(opts.timeout, self.run_exec(endpoint_id, cmd, env, sql))
            .await
            .map_err(|_| {
                anyhow::Error::new(SqlTimedOut {
                    endpoint: endpoint_id.to_string(),
                    after: opts.timeout,
                })
            })?
    }
}

impl ComputeLocator for Backend {
    async fn address(&self, endpoint_id: &str) -> anyhow::Result<Option<IpAddr>> {
        match self {
            Self::Docker(b) => b.address(endpoint_id).await,
        }
    }

    async fn control_addr(&self, endpoint_id: &str) -> anyhow::Result<Option<SocketAddr>> {
        match self {
            Self::Docker(b) => b.control_addr(endpoint_id).await,
        }
    }

    async fn postgres_addr(&self, endpoint_id: &str) -> anyhow::Result<Option<SocketAddr>> {
        match self {
            Self::Docker(b) => b.postgres_addr(endpoint_id).await,
        }
    }
}

impl ComputeBackend for Backend {
    async fn create(&self, endpoint_id: &str, compute_token: &str) -> anyhow::Result<()> {
        match self {
            Self::Docker(b) => b.create(endpoint_id, compute_token).await,
        }
    }

    async fn delete(&self, endpoint_id: &str) -> anyhow::Result<()> {
        match self {
            Self::Docker(b) => b.delete(endpoint_id).await,
        }
    }

    async fn read_file(&self, endpoint_id: &str, path: &str) -> anyhow::Result<Vec<u8>> {
        match self {
            Self::Docker(b) => b.read_file(endpoint_id, path).await,
        }
    }

    async fn upload_tls(&self, endpoint_id: &str, server: &IssuedCert) -> anyhow::Result<()> {
        match self {
            Self::Docker(b) => b.upload_tls(endpoint_id, server).await,
        }
    }

    async fn status(&self, endpoint_id: &str) -> anyhow::Result<Option<ComputeStatus>> {
        match self {
            Self::Docker(b) => b.status(endpoint_id).await,
        }
    }

    async fn list(&self) -> anyhow::Result<Vec<String>> {
        match self {
            Self::Docker(b) => b.list().await,
        }
    }

    async fn list_not_running(&self) -> anyhow::Result<Vec<String>> {
        match self {
            Self::Docker(b) => b.list_not_running().await,
        }
    }

    async fn compute_networks(&self) -> anyhow::Result<ComputeNetworks> {
        match self {
            Self::Docker(b) => b.compute_networks().await,
        }
    }

    async fn legacy_networks(&self) -> Vec<LegacyNetwork> {
        match self {
            Self::Docker(b) => b.legacy_networks().await,
        }
    }

    async fn remove_compute_container(
        &self,
        endpoint_id: &str,
        container_id: &str,
    ) -> anyhow::Result<()> {
        match self {
            Self::Docker(b) => b.remove_compute_container(endpoint_id, container_id).await,
        }
    }

    async fn remove_network(&self, network: &NetworkRef) -> anyhow::Result<NetworkRemoval> {
        match self {
            Self::Docker(b) => b.remove_network(network).await,
        }
    }

    async fn admin_sql_with(
        &self,
        endpoint_id: &str,
        database: &str,
        sql: &str,
        opts: &SqlOpts,
    ) -> anyhow::Result<String> {
        match self {
            Self::Docker(b) => b.admin_sql_with(endpoint_id, database, sql, opts).await,
        }
    }
}

/// An in-memory [`ComputeBackend`] for tests.
#[cfg(any(test, feature = "fake"))]
pub mod fake {
    use std::collections::HashMap;
    use std::net::{IpAddr, SocketAddr};
    use std::sync::{Arc, Mutex, MutexGuard};

    use std::time::Duration;

    use anyhow::bail;

    use super::{
        AttachedCompute, ComputeBackend, ComputeNetworks, ComputeStatus, LegacyNetwork,
        NetworkRef, NetworkRemoval, NotRunning, SqlOpts, SqlTimedOut,
    };
    use crate::compute::ComputeLocator;

    /// The fake's networks: where its computes are attached and which legacy networks exist.
    pub struct FakeNetworks {
        /// The compute and control networks. Default: `compute` (`net-compute`) and `control`
        /// (`net-control`).
        pub expected: Vec<NetworkRef>,
        /// Computes attached elsewhere than `expected` (the rest are on `expected`). A create
        /// puts its compute back on `expected`.
        pub attached: HashMap<String, Vec<NetworkRef>>,
        /// Computes whose container is mid-create (never started).
        pub mid_create: std::collections::HashSet<String>,
        /// What `legacy_networks` answers. A removed one becomes `Missing`.
        pub legacy: Vec<LegacyNetwork>,
        /// `compute_networks` fails with this.
        pub fail_listing: Option<String>,
        /// Removing these computes' containers fails.
        pub fail_remove: std::collections::HashSet<String>,
        /// `legacy_networks` answers `Unreadable` for these names.
        pub unreadable: std::collections::HashSet<String>,
        /// Removing these networks (by name) fails.
        pub fail_remove_network: std::collections::HashSet<String>,
        /// Docker refuses to remove these networks (by name) as in use, whatever their members.
        pub occupied: std::collections::HashSet<String>,
    }

    impl Default for FakeNetworks {
        fn default() -> Self {
            Self {
                expected: vec![net("compute"), net("control")],
                attached: HashMap::new(),
                mid_create: Default::default(),
                legacy: Vec::new(),
                fail_listing: None,
                fail_remove: Default::default(),
                unreadable: Default::default(),
                fail_remove_network: Default::default(),
                occupied: Default::default(),
            }
        }
    }

    /// A network named `name` whose id is `net-<name>`.
    pub fn net(name: &str) -> NetworkRef {
        NetworkRef {
            name: name.to_string(),
            id: format!("net-{name}"),
        }
    }

    /// The fake's container id of `endpoint_id`'s compute.
    pub fn container_id(endpoint_id: &str) -> String {
        format!("cid-{endpoint_id}")
    }

    pub type Journal = Arc<Mutex<Vec<String>>>;

    struct Compute {
        token: String,
        running: bool,
    }

    #[derive(Default)]
    pub struct FakeBackend {
        computes: Mutex<HashMap<String, Compute>>,
        /// `(endpoint, database, sql)` for every admin_sql call that reached the compute.
        pub sql: Mutex<Vec<(String, String, String)>>,
        /// The [`SqlOpts`] of every admin_sql call that reached the compute, like `sql`.
        pub sql_opts: Mutex<Vec<SqlOpts>>,
        /// Each admin_sql call takes this long: one with a shorter timeout fails with
        /// "timed out after" (no real waiting).
        pub sql_delay: Mutex<Option<Duration>>,
        pub journal: Journal,
        pub fail_create: Mutex<Option<String>>,
        /// Each admin_sql call fails with "not ready" and decrements this while it's above 0.
        pub sql_failures_left: Mutex<u32>,
        /// `(needle, message)`: SQL containing the needle fails with the message.
        pub fail_sql_containing: Mutex<Option<(String, String)>>,
        /// Needles set by [`FakeBackend::fail_when`].
        fail_needles: Mutex<Vec<String>>,
        /// `(needle, runs)` set by [`FakeBackend::time_out_when`].
        timeout_needles: Mutex<Vec<(String, bool)>>,
        pub fail_delete: Mutex<Option<String>>,
        /// `control_addr` never answers, like a hung Docker daemon.
        pub hang_control_addr: Mutex<bool>,
        /// `(needle, stdout)`: SQL containing the needle answers with stdout (the first match);
        /// everything else answers "".
        pub answers: Mutex<Vec<(String, String)>>,
        /// `(database, needle, stdout)`: like `answers`, for one database only; tried first.
        pub answers_in: Mutex<Vec<(String, String, String)>>,
        /// `(endpoint, needle, stdout)`: like `answers`, for one endpoint only; tried before
        /// `answers_in`.
        pub answers_at: Mutex<Vec<(String, String, String)>>,
        /// `(endpoint, needle, stdout)`: like `answers_at`, but each answers once (the first
        /// match is used up); tried before every other answer.
        pub answers_once: Mutex<Vec<(String, String, String)>>,
        /// `(endpoint, path)` → the bytes `read_file` returns (see [`FakeBackend::set_file`]);
        /// a path not set is an error, like a file missing in the container.
        pub files: Mutex<HashMap<(String, String), Vec<u8>>>,
        /// Every `upload_tls` that got through: the endpoint and the server certificate.
        pub tls_uploads: Mutex<Vec<(String, crate::compute_tls::IssuedCert)>>,
        /// `upload_tls` fails with this message.
        pub fail_upload: Mutex<Option<String>>,
        /// `(database, needle)` set by [`FakeBackend::fail_in`].
        fail_in_needles: Mutex<Vec<(String, String)>>,
        /// `(database, needle, message)` set by [`FakeBackend::fail_in_with`].
        fail_in_messages: Mutex<Vec<(String, String, String)>>,
        /// Where the computes are attached, and the legacy networks.
        pub networks: Mutex<FakeNetworks>,
    }

    fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    impl FakeBackend {
        pub fn with_journal(journal: Journal) -> Self {
            Self {
                journal,
                ..Default::default()
            }
        }

        /// SQL containing `needle` answers `stdout` from now on.
        pub fn answer(&self, needle: &str, stdout: &str) {
            lock(&self.answers).push((needle.to_string(), stdout.to_string()));
        }

        /// SQL run in `database` containing `needle` answers `stdout` from now on (before any
        /// [`FakeBackend::answer`]).
        pub fn answer_in(&self, database: &str, needle: &str, stdout: &str) {
            lock(&self.answers_in).push((
                database.to_string(),
                needle.to_string(),
                stdout.to_string(),
            ));
        }

        /// SQL run on `endpoint` containing `needle` answers `stdout` from now on (before any
        /// other answer).
        pub fn answer_at(&self, endpoint: &str, needle: &str, stdout: &str) {
            lock(&self.answers_at).push((
                endpoint.to_string(),
                needle.to_string(),
                stdout.to_string(),
            ));
        }

        /// The next SQL run on `endpoint` containing `needle` answers `stdout`, once (before any
        /// other answer); later ones answer as before.
        pub fn answer_once_at(&self, endpoint: &str, needle: &str, stdout: &str) {
            lock(&self.answers_once).push((
                endpoint.to_string(),
                needle.to_string(),
                stdout.to_string(),
            ));
        }

        /// From now on, SQL containing `needle` fails with "injected failure" (on top of any
        /// earlier needles).
        pub fn fail_when(&self, needle: &str) {
            lock(&self.fail_needles).push(needle.to_string());
        }

        /// From now on, SQL run in `database` containing `needle` fails with "injected failure"
        /// (other databases are unaffected).
        pub fn fail_in(&self, database: &str, needle: &str) {
            lock(&self.fail_in_needles).push((database.to_string(), needle.to_string()));
        }

        /// From now on, SQL run in `database` containing `needle` fails with `message` (like
        /// psql's error output), other databases unaffected; tried before [`Self::fail_in`].
        pub fn fail_in_with(&self, database: &str, needle: &str, message: &str) {
            lock(&self.fail_in_messages).push((
                database.to_string(),
                needle.to_string(),
                message.to_string(),
            ));
        }

        /// The file `path` of `endpoint_id`'s container reads as `contents`.
        pub fn set_file(&self, endpoint_id: &str, path: &str, contents: &[u8]) {
            lock(&self.files).insert(
                (endpoint_id.to_string(), path.to_string()),
                contents.to_vec(),
            );
        }

        /// From now on, SQL containing `needle` times out ([`SqlTimedOut`]); with `runs`, after
        /// it ran (its answer lost: recorded in `sql` like a call that reached the compute).
        pub fn time_out_when(&self, needle: &str, runs: bool) {
            lock(&self.timeout_needles).push((needle.to_string(), runs));
        }

        fn record(&self, entry: String) {
            lock(&self.journal).push(entry);
        }

        pub fn token_of(&self, endpoint_id: &str) -> Option<String> {
            lock(&self.computes)
                .get(endpoint_id)
                .map(|c| c.token.clone())
        }

        pub fn set_running(&self, endpoint_id: &str, running: bool) {
            if let Some(c) = lock(&self.computes).get_mut(endpoint_id) {
                c.running = running;
            }
        }

        /// A running compute that appeared behind the backend's back.
        pub fn insert(&self, endpoint_id: &str) {
            lock(&self.computes).insert(
                endpoint_id.to_string(),
                Compute {
                    token: String::new(),
                    running: true,
                },
            );
        }

        /// The compute vanished.
        pub fn remove(&self, endpoint_id: &str) {
            lock(&self.computes).remove(endpoint_id);
        }

        fn is_running(&self, endpoint_id: &str) -> bool {
            lock(&self.computes)
                .get(endpoint_id)
                .is_some_and(|c| c.running)
        }
    }

    impl ComputeLocator for FakeBackend {
        async fn address(&self, endpoint_id: &str) -> anyhow::Result<Option<IpAddr>> {
            Ok(self
                .is_running(endpoint_id)
                .then(|| "172.20.0.2".parse().unwrap()))
        }

        async fn control_addr(&self, endpoint_id: &str) -> anyhow::Result<Option<SocketAddr>> {
            if *lock(&self.hang_control_addr) {
                std::future::pending::<()>().await;
            }
            Ok(self
                .is_running(endpoint_id)
                .then(|| "127.0.0.1:3080".parse().unwrap()))
        }

        async fn postgres_addr(&self, endpoint_id: &str) -> anyhow::Result<Option<SocketAddr>> {
            Ok(self
                .is_running(endpoint_id)
                .then(|| "127.0.0.1:55432".parse().unwrap()))
        }
    }

    impl ComputeBackend for FakeBackend {
        async fn create(&self, endpoint_id: &str, compute_token: &str) -> anyhow::Result<()> {
            self.record(format!("compute.create {endpoint_id}"));
            if let Some(message) = lock(&self.fail_create).clone() {
                bail!(message);
            }
            lock(&self.computes).insert(
                endpoint_id.to_string(),
                Compute {
                    token: compute_token.to_string(),
                    running: true,
                },
            );
            let mut networks = lock(&self.networks);
            networks.attached.remove(endpoint_id);
            networks.mid_create.remove(endpoint_id);
            Ok(())
        }

        async fn read_file(&self, endpoint_id: &str, path: &str) -> anyhow::Result<Vec<u8>> {
            match lock(&self.files).get(&(endpoint_id.to_string(), path.to_string())) {
                Some(bytes) => Ok(bytes.clone()),
                None => bail!("no such file {path} in {endpoint_id}"),
            }
        }

        async fn upload_tls(
            &self,
            endpoint_id: &str,
            server: &crate::compute_tls::IssuedCert,
        ) -> anyhow::Result<()> {
            if let Some(message) = lock(&self.fail_upload).clone() {
                bail!(message);
            }
            lock(&self.tls_uploads).push((endpoint_id.to_string(), server.clone()));
            Ok(())
        }

        async fn delete(&self, endpoint_id: &str) -> anyhow::Result<()> {
            self.record(format!("compute.delete {endpoint_id}"));
            if let Some(message) = lock(&self.fail_delete).clone() {
                bail!(message);
            }
            lock(&self.computes).remove(endpoint_id);
            lock(&self.networks).attached.remove(endpoint_id);
            Ok(())
        }

        async fn status(&self, endpoint_id: &str) -> anyhow::Result<Option<ComputeStatus>> {
            Ok(lock(&self.computes).get(endpoint_id).map(|c| {
                if c.running {
                    ComputeStatus::Running
                } else {
                    ComputeStatus::NotRunning(NotRunning::Exited)
                }
            }))
        }

        async fn list(&self) -> anyhow::Result<Vec<String>> {
            let mut ids: Vec<String> = lock(&self.computes).keys().cloned().collect();
            ids.sort();
            Ok(ids)
        }

        /// The computes set not running ([`FakeBackend::set_running`]), sorted.
        async fn list_not_running(&self) -> anyhow::Result<Vec<String>> {
            let mut ids: Vec<String> = lock(&self.computes)
                .iter()
                .filter(|(_, c)| !c.running)
                .map(|(id, _)| id.clone())
                .collect();
            ids.sort();
            Ok(ids)
        }

        /// The computes sorted, each on `expected` unless [`FakeNetworks::attached`] says
        /// otherwise.
        async fn compute_networks(&self) -> anyhow::Result<ComputeNetworks> {
            let networks = lock(&self.networks);
            if let Some(message) = networks.fail_listing.clone() {
                bail!(message);
            }
            let mut ids: Vec<String> = lock(&self.computes).keys().cloned().collect();
            ids.sort();
            let computes = ids
                .into_iter()
                .map(|ep| AttachedCompute {
                    container_id: container_id(&ep),
                    mid_create: networks.mid_create.contains(&ep),
                    networks: networks
                        .attached
                        .get(&ep)
                        .cloned()
                        .unwrap_or_else(|| networks.expected.clone()),
                    endpoint_id: ep,
                })
                .collect();
            Ok(ComputeNetworks {
                expected: networks.expected.clone(),
                computes,
            })
        }

        async fn legacy_networks(&self) -> Vec<LegacyNetwork> {
            let networks = lock(&self.networks);
            networks
                .legacy
                .iter()
                .map(|l| {
                    let name = match l {
                        LegacyNetwork::Missing { name } | LegacyNetwork::Unreadable { name, .. } => name,
                        LegacyNetwork::Present { network, .. } => &network.name,
                    };
                    if networks.unreadable.contains(name) {
                        LegacyNetwork::Unreadable {
                            name: name.clone(),
                            error: "injected inspect failure".into(),
                        }
                    } else {
                        l.clone()
                    }
                })
                .collect()
        }

        /// Removes the compute if `container_id` is still its container, and its name (any
        /// member ending in the endpoint id) from the legacy networks.
        async fn remove_compute_container(
            &self,
            endpoint_id: &str,
            container_id: &str,
        ) -> anyhow::Result<()> {
            self.record(format!("compute.remove_container {endpoint_id}"));
            let mut networks = lock(&self.networks);
            if networks.fail_remove.contains(endpoint_id) {
                bail!("injected removal failure");
            }
            if container_id != self::container_id(endpoint_id) {
                return Ok(());
            }
            lock(&self.computes).remove(endpoint_id);
            networks.attached.remove(endpoint_id);
            networks.mid_create.remove(endpoint_id);
            for legacy in &mut networks.legacy {
                if let LegacyNetwork::Present { members, .. } = legacy {
                    members.retain(|m| !m.ends_with(endpoint_id));
                }
            }
            Ok(())
        }

        async fn remove_network(&self, network: &NetworkRef) -> anyhow::Result<NetworkRemoval> {
            self.record(format!("network.remove {}", network.name));
            let mut networks = lock(&self.networks);
            if networks.fail_remove_network.contains(&network.name) {
                bail!("injected network removal failure");
            }
            if networks.occupied.contains(&network.name) {
                return Ok(NetworkRemoval::Occupied);
            }
            for legacy in &mut networks.legacy {
                if let LegacyNetwork::Present { network: n, members } = legacy
                    && n.id == network.id
                {
                    if !members.is_empty() {
                        return Ok(NetworkRemoval::Occupied);
                    }
                    *legacy = LegacyNetwork::Missing {
                        name: n.name.clone(),
                    };
                    return Ok(NetworkRemoval::Removed);
                }
            }
            Ok(NetworkRemoval::Gone)
        }

        async fn admin_sql_with(
            &self,
            endpoint_id: &str,
            database: &str,
            sql: &str,
            opts: &SqlOpts,
        ) -> anyhow::Result<String> {
            self.record(format!(
                "compute.sql {endpoint_id} {database} {}",
                sql.lines().next().unwrap_or("")
            ));
            {
                let mut left = lock(&self.sql_failures_left);
                if *left > 0 {
                    *left -= 1;
                    bail!("not ready");
                }
            }
            if let Some((needle, message)) = lock(&self.fail_sql_containing).clone()
                && sql.contains(&needle)
            {
                bail!(message);
            }
            if lock(&self.fail_needles).iter().any(|n| sql.contains(n)) {
                bail!("injected failure");
            }
            if let Some((_, _, message)) = lock(&self.fail_in_messages)
                .iter()
                .find(|(db, n, _)| db == database && sql.contains(n.as_str()))
                .cloned()
            {
                bail!(message);
            }
            if lock(&self.fail_in_needles)
                .iter()
                .any(|(db, n)| db == database && sql.contains(n))
            {
                bail!("injected failure");
            }
            let timeout = lock(&self.timeout_needles)
                .iter()
                .find(|(n, _)| sql.contains(n))
                .map(|(_, runs)| *runs);
            if let Some(runs) = timeout {
                if runs {
                    lock(&self.sql).push((
                        endpoint_id.to_string(),
                        database.to_string(),
                        sql.to_string(),
                    ));
                    lock(&self.sql_opts).push(opts.clone());
                }
                return Err(anyhow::Error::new(SqlTimedOut {
                    endpoint: endpoint_id.to_string(),
                    after: opts.timeout,
                }));
            }
            if let Some(delay) = *lock(&self.sql_delay)
                && delay > opts.timeout
            {
                return Err(anyhow::Error::new(SqlTimedOut {
                    endpoint: endpoint_id.to_string(),
                    after: opts.timeout,
                }));
            }
            lock(&self.sql).push((
                endpoint_id.to_string(),
                database.to_string(),
                sql.to_string(),
            ));
            lock(&self.sql_opts).push(opts.clone());
            {
                let mut once = lock(&self.answers_once);
                if let Some(i) = once
                    .iter()
                    .position(|(ep, needle, _)| ep == endpoint_id && sql.contains(needle))
                {
                    return Ok(once.remove(i).2);
                }
            }
            if let Some((_, _, stdout)) = lock(&self.answers_at)
                .iter()
                .find(|(ep, needle, _)| ep == endpoint_id && sql.contains(needle))
            {
                return Ok(stdout.clone());
            }
            if let Some((_, _, stdout)) = lock(&self.answers_in)
                .iter()
                .find(|(db, needle, _)| db == database && sql.contains(needle))
            {
                return Ok(stdout.clone());
            }
            Ok(lock(&self.answers)
                .iter()
                .find(|(needle, _)| sql.contains(needle))
                .map(|(_, stdout)| stdout.clone())
                .unwrap_or_default())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{FakeBackend, Journal};
    use super::*;

    #[test]
    fn backend_from_env_default_docker() {
        assert_eq!(BackendKind::from_lookup(|_| None).unwrap(), BackendKind::Docker);
        assert_eq!(
            BackendKind::from_lookup(|_| Some(String::new())).unwrap(),
            BackendKind::Docker
        );
        assert_eq!(
            BackendKind::from_lookup(|_| Some("docker".to_string())).unwrap(),
            BackendKind::Docker
        );
    }

    #[test]
    fn backend_from_env_rejects_unknown() {
        let err = BackendKind::from_lookup(|_| Some("k8s".to_string())).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("CHELABASE_COMPUTE_BACKEND"), "{text}");
        assert!(text.contains("docker"), "{text}");
        assert!(text.contains("k8s"), "{text}");
    }

    #[test]
    fn docker_status_maps_exited_dead_restarting() {
        use bollard::models::{ContainerStateStatusEnum as S, ContainerSummaryStateEnum as L};
        assert_eq!(docker_status(S::RUNNING), ComputeStatus::Running);
        assert_eq!(docker_status(S::EXITED), ComputeStatus::NotRunning(NotRunning::Exited));
        assert_eq!(docker_status(S::DEAD), ComputeStatus::NotRunning(NotRunning::Crashed));
        assert_eq!(
            docker_status(S::RESTARTING),
            ComputeStatus::NotRunning(NotRunning::Other("restarting".to_string()))
        );
        // The listing keeps today's set.
        for (state, listed) in [
            (L::EXITED, true),
            (L::DEAD, true),
            (L::RESTARTING, true),
            (L::CREATED, false),
            (L::RUNNING, false),
            (L::PAUSED, false),
            (L::REMOVING, false),
        ] {
            assert_eq!(docker_not_running_listed(state), listed, "{state}");
        }
    }

    fn settings() -> ComputeSettings {
        ComputeSettings {
            image: "neon/compute-node-v17:test".to_string(),
            control_plane_uri: "http://orchestrator:8083".to_string(),
            stack: "feat-x".to_string(),
            ca: None,
        }
    }

    fn docker_settings() -> DockerSettings {
        DockerSettings {
            network: "chelabase-feat-x_default".to_string(),
            control_network: "chelabase-feat-x_control".to_string(),
            legacy_networks: Vec::new(),
            egress_dns: None,
        }
    }

    // The orchestrator's backend has no CA: creating a compute fails before Docker is asked.
    #[tokio::test]
    async fn create_without_a_ca_fails_before_docker() {
        let backend = DockerBackend::connect(settings(), docker_settings()).unwrap();
        let err = backend.create("ep-abc123", "t").await.unwrap_err();
        assert!(format!("{err:#}").contains("no compute CA"), "{err:#}");
    }

    #[test]
    fn container_body_runs_compute_ctl_with_the_token_in_env() {
        let body = create_body("ep-abc123", "s3cret-token", &settings(), &docker_settings());
        assert_eq!(body.image.as_deref(), Some("neon/compute-node-v17:test"));
        assert_eq!(
            body.entrypoint,
            Some(vec!["/usr/local/bin/compute_ctl".to_string()])
        );
        let cmd = body.cmd.unwrap();
        assert_eq!(
            cmd,
            compute_ctl_args("ep-abc123", "http://orchestrator:8083")
        );
        assert_eq!(
            body.env,
            Some(vec!["NEON_CONTROL_PLANE_TOKEN=s3cret-token".to_string()])
        );
        assert_eq!(
            body.labels.unwrap().get("chelabase.dev/endpoint-id"),
            Some(&"ep-abc123".to_string())
        );
        assert!(cmd.iter().all(|arg| !arg.contains("s3cret-token")));
    }

    #[test]
    fn container_body_limits_restarts_and_networks() {
        let host = create_body("ep-abc123", "t", &settings(), &docker_settings())
            .host_config
            .unwrap();
        assert_eq!(
            host.network_mode.as_deref(),
            Some("chelabase-feat-x_default")
        );
        assert_eq!(
            host.restart_policy.unwrap().name,
            Some(RestartPolicyNameEnum::UNLESS_STOPPED)
        );
        assert_eq!(host.memory, Some(2 * 1024 * 1024 * 1024));
        assert_eq!(host.nano_cpus, Some(1_000_000_000));
        assert_eq!(host.extra_hosts, None);
    }

    #[test]
    fn the_create_body_carries_the_egress_resolver() {
        let docker = DockerSettings {
            egress_dns: Some("10.214.8.2".parse().unwrap()),
            ..docker_settings()
        };
        let host = create_body("ep-abc123", "t", &settings(), &docker)
            .host_config
            .unwrap();
        assert_eq!(host.dns, Some(vec!["10.214.8.2".to_string()]));
        assert_eq!(
            host.dns_options,
            Some(vec!["timeout:1".to_string(), "attempts:1".to_string()])
        );
        assert_eq!(host.extra_hosts, None);
    }

    #[test]
    fn the_create_body_has_no_resolver_when_unset() {
        let host = create_body("ep-abc123", "t", &settings(), &docker_settings())
            .host_config
            .unwrap();
        assert_eq!(host.dns, None);
        assert_eq!(host.dns_options, None);
        assert_eq!(host.extra_hosts, None);
    }

    // No capability at all (so no `NET_RAW`) and no privilege gain through setuid binaries.
    #[test]
    fn the_create_body_drops_raw_sockets() {
        for docker in [
            docker_settings(),
            DockerSettings {
                egress_dns: Some("fd00::53".parse().unwrap()),
                ..docker_settings()
            },
        ] {
            let host = create_body("ep-abc123", "t", &settings(), &docker)
                .host_config
                .unwrap();
            assert_eq!(host.cap_drop, Some(vec!["ALL".to_string()]));
            assert_eq!(host.cap_add, None);
            assert_eq!(
                host.security_opt,
                Some(vec!["no-new-privileges:true".to_string()])
            );
            assert_ne!(host.privileged, Some(true));
        }
    }

    fn network(name: &str, internal: Option<bool>, options: &[(&str, &str)]) -> NetworkInspect {
        NetworkInspect {
            name: Some(name.to_string()),
            id: Some(format!("id-{name}")),
            internal,
            options: Some(
                options
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
            ..Default::default()
        }
    }

    const ISOLATED: [(&str, &str); 2] = [(GATEWAY_MODE_IPV4, "isolated"), (GATEWAY_MODE_IPV6, "isolated")];

    #[test]
    fn an_internal_isolated_network_passes_the_check() {
        check_compute_network("n", &network("n", Some(true), &ISOLATED)).unwrap();
    }

    #[test]
    fn the_check_names_the_network_and_what_it_lacks() {
        let cases: [(NetworkInspect, &str); 6] = [
            (network("n", Some(false), &ISOLATED), "is not internal"),
            (network("n", None, &ISOLATED), "is not internal"),
            (network("n", Some(true), &[(GATEWAY_MODE_IPV6, "isolated")]), GATEWAY_MODE_IPV4),
            (network("n", Some(true), &[(GATEWAY_MODE_IPV4, "isolated")]), GATEWAY_MODE_IPV6),
            (
                network("n", Some(true), &[(GATEWAY_MODE_IPV4, "nat"), (GATEWAY_MODE_IPV6, "isolated")]),
                GATEWAY_MODE_IPV4,
            ),
            // Docker resolves an id prefix too: the answer must be the network of that name.
            (network("other", Some(true), &ISOLATED), "not one of that name"),
        ];
        for (inspect, lacks) in cases {
            let err = check_compute_network("n", &inspect).unwrap_err();
            let text = format!("{err:#}");
            assert!(text.contains("network n"), "{text}");
            assert!(text.contains(lacks), "{lacks}: {text}");
        }
        let mut no_options = network("n", Some(true), &[]);
        no_options.options = None;
        assert!(check_compute_network("n", &no_options).is_err());
    }

    #[test]
    fn computes_publish_nothing_on_the_host() {
        let body = create_body("ep-abc123", "t", &settings(), &docker_settings());
        assert_eq!(body.exposed_ports, None);
        let host = body.host_config.unwrap();
        assert!(host.port_bindings.as_ref().is_none_or(|b| b.is_empty()));
        assert_ne!(host.publish_all_ports, Some(true));
    }

    #[test]
    fn computes_carry_their_stack_label() {
        let labels = create_body("ep-abc123", "t", &settings(), &docker_settings()).labels.unwrap();
        assert_eq!(labels.get("chelabase.dev/stack"), Some(&"feat-x".to_string()));
        assert_eq!(
            labels.get("chelabase.dev/endpoint-id"),
            Some(&"ep-abc123".to_string())
        );
        assert_eq!(labels.len(), 2);
    }

    #[test]
    fn psql_command_uses_cloud_admin_on_localhost() {
        assert_eq!(
            admin_psql_command("web_1").unwrap(),
            [
                "psql",
                "-X",
                "-q",
                "-A",
                "-t",
                "-v",
                "ON_ERROR_STOP=1",
                "-v",
                "VERBOSITY=terse",
                "-h",
                "localhost",
                "-U",
                "cloud_admin",
                "-d",
                "web_1"
            ]
        );
    }

    // Terse errors carry no `LINE n:` excerpt of the statement, so a SCRAM verifier in a
    // script never reaches the error text (logs, role replies, branch errors).
    #[test]
    fn psql_command_prints_terse_errors() {
        let cmd = admin_psql_command("web_1").unwrap();
        assert!(
            cmd.windows(2)
                .any(|w| w[0] == "-v" && w[1] == "VERBOSITY=terse"),
            "{cmd:?}"
        );
    }

    #[test]
    fn psql_command_refuses_names_it_cant_pass() {
        for bad in ["", "a\0b", &"a".repeat(64), &"é".repeat(32)] {
            assert!(admin_psql_command(bad).is_err(), "{bad:?}");
        }
        assert!(admin_psql_command("1abc").is_ok());
        assert!(admin_psql_command(&"a".repeat(63)).is_ok());
    }

    // Any other name goes in as a quoted conninfo, so psql never reads it as options or
    // connection parameters.
    #[test]
    fn psql_command_quotes_other_names_as_a_conninfo() {
        let db = |name: &str| admin_psql_command(name).unwrap().last().unwrap().clone();
        assert_eq!(db("my-app"), "dbname='my-app'");
        assert_eq!(db("a=b host=evil"), "dbname='a=b host=evil'");
        assert_eq!(db("-c"), "dbname='-c'");
        assert_eq!(db("it's"), r"dbname='it\'s'");
        assert_eq!(db(r"back\slash"), r"dbname='back\\slash'");
        assert_eq!(db("new\nline"), "dbname='new\nline'");
        assert_eq!(db("postgres://evil"), "dbname='postgres://evil'");
    }

    #[tokio::test]
    async fn the_fake_tracks_computes_and_journals_calls() {
        let journal = Journal::default();
        let fake = FakeBackend::with_journal(journal.clone());
        fake.create("ep-1", "tok").await.unwrap();
        assert_eq!(
            fake.status("ep-1").await.unwrap(),
            Some(ComputeStatus::Running)
        );
        assert_eq!(fake.list().await.unwrap(), ["ep-1"]);
        assert_eq!(fake.token_of("ep-1").as_deref(), Some("tok"));
        assert_eq!(
            fake.address("ep-1").await.unwrap(),
            Some("172.20.0.2".parse().unwrap())
        );
        assert_eq!(
            fake.control_addr("ep-1").await.unwrap(),
            Some("127.0.0.1:3080".parse().unwrap())
        );
        assert_eq!(
            fake.postgres_addr("ep-1").await.unwrap(),
            Some("127.0.0.1:55432".parse().unwrap())
        );
        fake.admin_sql("ep-1", "postgres", "SELECT 1;\nSELECT 2;")
            .await
            .unwrap();
        fake.set_running("ep-1", false);
        assert_eq!(
            fake.status("ep-1").await.unwrap(),
            Some(ComputeStatus::NotRunning(NotRunning::Exited))
        );
        assert_eq!(fake.address("ep-1").await.unwrap(), None);
        assert_eq!(fake.postgres_addr("ep-1").await.unwrap(), None);
        fake.delete("ep-1").await.unwrap();
        assert_eq!(fake.status("ep-1").await.unwrap(), None);
        assert_eq!(
            *journal.lock().unwrap(),
            [
                "compute.create ep-1",
                "compute.sql ep-1 postgres SELECT 1;",
                "compute.delete ep-1"
            ]
        );
    }

    #[tokio::test]
    async fn the_fake_fails_on_demand() {
        let fake = FakeBackend::default();
        *fake.sql_failures_left.lock().unwrap() = 2;
        for _ in 0..2 {
            let err = fake.admin_sql("ep-1", "postgres", "x").await.unwrap_err();
            assert_eq!(err.to_string(), "not ready");
        }
        fake.admin_sql("ep-1", "postgres", "x").await.unwrap();
        *fake.fail_sql_containing.lock().unwrap() = Some(("ROLE".into(), "boom".into()));
        let err = fake
            .admin_sql("ep-1", "postgres", "CREATE ROLE a")
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "boom");
        *fake.fail_create.lock().unwrap() = Some("no docker".into());
        assert!(fake.create("ep-2", "t").await.is_err());
        assert_eq!(fake.status("ep-2").await.unwrap(), None);
        *fake.fail_delete.lock().unwrap() = Some("stuck".into());
        assert!(fake.delete("ep-1").await.is_err());
        fake.insert("ep-3");
        fake.remove("ep-3");
        // Canned output for SQL containing a needle; the rest answers "".
        fake.answer("to_regclass", "f\n");
        assert_eq!(
            fake.admin_sql("ep-1", "app", "SELECT to_regclass('x')")
                .await
                .unwrap(),
            "f\n"
        );
        assert_eq!(fake.admin_sql("ep-1", "app", "SELECT 1").await.unwrap(), "");
        assert_eq!(fake.status("ep-3").await.unwrap(), None);
    }

    // `fail_when`: every SQL containing one of the needles fails; the rest still runs.
    #[tokio::test]
    async fn the_fake_fails_sql_containing_any_needle() {
        let fake = FakeBackend::default();
        fake.fail_when("ALTER ROLE \"a\"");
        fake.fail_when("ALTER ROLE \"b\"");
        for name in ["a", "b"] {
            let sql = format!("SET x;\nALTER ROLE \"{name}\" PASSWORD 'p';");
            let err = fake.admin_sql("ep-1", "postgres", &sql).await.unwrap_err();
            assert_eq!(err.to_string(), "injected failure");
        }
        fake.admin_sql("ep-1", "postgres", "ALTER ROLE \"c\" PASSWORD 'p';")
            .await
            .unwrap();
        assert_eq!(fake.sql.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn admin_sql_with_a_nul_is_refused_before_it_is_sent() {
        // No engine is reachable here: a script that got as far as the exec would fail with
        // a connection error, not with this refusal.
        let err = refuse_nul("SELECT 1;\0DROP TABLE t;").unwrap_err();
        assert!(format!("{err:#}").contains("NUL"), "{err:#}");
        assert!(refuse_nul("SELECT 'a b';\n").is_ok());
    }

    #[tokio::test]
    async fn admin_sql_with_passes_app_name_and_timeout() {
        let journal = Journal::default();
        let fake = FakeBackend::with_journal(journal.clone());
        let opts = SqlOpts {
            timeout: Duration::from_secs(960),
            app_name: Some("chelabase-merge-1".into()),
            verbose_errors: true,
            user: None,
        };
        fake.admin_sql_with("ep-1", "app", "SELECT 1;\nSELECT 2;", &opts)
            .await
            .unwrap();
        assert_eq!(
            *journal.lock().unwrap(),
            ["compute.sql ep-1 app SELECT 1;"]
        );
        assert_eq!(
            fake.sql_opts.lock().unwrap().as_slice(),
            std::slice::from_ref(&opts)
        );
        // A call slower than its timeout fails like the real backend.
        *fake.sql_delay.lock().unwrap() = Some(Duration::from_secs(5));
        let short = SqlOpts {
            timeout: Duration::from_secs(1),
            ..SqlOpts::admin_default()
        };
        let err = fake
            .admin_sql_with("ep-1", "app", "SELECT 3", &short)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out after"), "{err}");
        // T14 apply minor 1: the timeout is typed, never told apart by its text.
        assert!(super::is_timeout(&err), "{err}");
        assert!(fake.admin_sql_with("ep-1", "app", "SELECT 4", &opts).await.is_ok());
        // An error that only says so (a user's RAISE, say) isn't one.
        *fake.sql_delay.lock().unwrap() = None;
        *fake.fail_sql_containing.lock().unwrap() =
            Some(("SELECT 5".into(), "ERROR:  timed out after 1s".into()));
        let raised = fake.admin_sql_with("ep-1", "app", "SELECT 5", &short).await.unwrap_err();
        assert!(!super::is_timeout(&raised), "{raised}");
    }

    // T14 apply minor 21: the application name is one Postgres keeps whole and psql passes as is.
    #[test]
    #[should_panic(expected = "application_name")]
    fn an_overlong_app_name_is_caught() {
        let opts = SqlOpts {
            app_name: Some("x".repeat(64)),
            ..SqlOpts::admin_default()
        };
        let _ = super::admin_exec_env_with(&opts);
    }

    #[tokio::test]
    async fn admin_sql_is_admin_sql_with_defaults() {
        let fake = FakeBackend::default();
        fake.admin_sql("ep-1", "app", "SELECT 1").await.unwrap();
        assert_eq!(
            fake.sql_opts.lock().unwrap().as_slice(),
            [SqlOpts::admin_default()]
        );
        let d = SqlOpts::admin_default();
        assert_eq!(d.timeout, Duration::from_secs(30));
        assert_eq!(d.app_name, None);
        assert!(!d.verbose_errors);
    }

    #[test]
    fn sql_opts_shape_the_psql_command_and_env() {
        let verbose = SqlOpts {
            verbose_errors: true,
            app_name: Some("chelabase-merge-7".into()),
            ..SqlOpts::admin_default()
        };
        let cmd = admin_psql_command_with("web_1", &verbose).unwrap();
        assert!(cmd.contains(&"VERBOSITY=default".to_string()), "{cmd:?}");
        assert!(!cmd.contains(&"VERBOSITY=terse".to_string()), "{cmd:?}");
        let env = admin_exec_env_with(&verbose);
        assert_eq!(env[..3], admin_exec_env()[..]);
        assert!(env.contains(&"PGAPPNAME=chelabase-merge-7".to_string()), "{env:?}");
        let plain = SqlOpts::admin_default();
        assert_eq!(
            admin_psql_command_with("web_1", &plain).unwrap(),
            admin_psql_command("web_1").unwrap()
        );
        assert_eq!(admin_exec_env_with(&plain), admin_exec_env());
    }

    // E15: merge scripts run in the owner's own session (`-U postgres`); `None` stays
    // `cloud_admin`.
    #[test]
    fn sql_opts_user_runs_psql_as_that_role() {
        let owner = SqlOpts {
            user: Some("postgres".into()),
            ..SqlOpts::admin_default()
        };
        let cmd = admin_psql_command_with("web_1", &owner).unwrap();
        assert!(
            cmd.windows(2).any(|w| w[0] == "-U" && w[1] == "postgres"),
            "{cmd:?}"
        );
        assert!(!cmd.contains(&"cloud_admin".to_string()), "{cmd:?}");
        // Fix round 1 (M4): IPv4 loopback, which the compute's pg_hba trusts; `localhost` may
        // resolve to `::1`.
        assert!(
            cmd.windows(2).any(|w| w[0] == "-h" && w[1] == "127.0.0.1"),
            "{cmd:?}"
        );
        assert!(!cmd.contains(&"localhost".to_string()), "{cmd:?}");
        assert_eq!(SqlOpts::admin_default().user, None);
        assert!(
            admin_psql_command("web_1")
                .unwrap()
                .windows(2)
                .any(|w| w[0] == "-U" && w[1] == "cloud_admin")
        );
    }

    #[test]
    fn admin_exec_turns_forwarding_off() {
        let env = admin_exec_env();
        assert_eq!(env.len(), 3);
        assert_eq!(env[0], "PGCONNECT_TIMEOUT=5");
        assert_eq!(env[1], "PGCLIENTENCODING=UTF8");
        assert_eq!(
            env[2],
            "PGOPTIONS=-c standard_conforming_strings=on -c search_path=pg_catalog,pg_temp \
             -c neon.forward_ddl=off"
        );
    }

    /// The one `PGOPTIONS` entry of `env`, split into its `-c` settings.
    fn pg_options(env: &[String]) -> Vec<&str> {
        let mut entries = env.iter().filter_map(|e| e.strip_prefix("PGOPTIONS="));
        let options = entries.next().expect("a PGOPTIONS entry");
        assert_eq!(entries.next(), None, "{env:?}");
        let words: Vec<&str> = options.split(' ').collect();
        assert!(words.len().is_multiple_of(2), "{options:?}");
        words
            .chunks(2)
            .map(|pair| {
                assert_eq!(pair[0], "-c", "{options:?}");
                pair[1]
            })
            .collect()
    }

    fn owner_opts() -> SqlOpts {
        SqlOpts {
            user: Some("postgres".into()),
            ..SqlOpts::admin_default()
        }
    }

    /// Every session reads its script as UTF8 with standard-conforming strings, whatever the
    /// database or the role sets: the worker's quoting holds under exactly those two.
    #[test]
    fn admin_session_pins_client_encoding_and_literals() {
        for opts in [SqlOpts::admin_default(), owner_opts()] {
            let env = admin_exec_env_with(&opts);
            assert_eq!(
                env.iter().filter(|e| e.starts_with("PGCLIENTENCODING=")).collect::<Vec<_>>(),
                ["PGCLIENTENCODING=UTF8"],
                "{env:?}"
            );
            assert!(
                pg_options(&env).contains(&"standard_conforming_strings=on"),
                "{env:?}"
            );
        }
    }

    /// `cloud_admin` resolves names in `pg_catalog` only; the owner's session keeps the
    /// database's search path (merge user steps resolve the tenant's objects through it).
    #[test]
    fn search_path_is_pinned_for_cloud_admin_only() {
        let admin = admin_exec_env();
        let admin = pg_options(&admin);
        assert!(admin.contains(&"search_path=pg_catalog,pg_temp"), "{admin:?}");
        assert!(admin.contains(&"neon.forward_ddl=off"), "{admin:?}");
        let owner = admin_exec_env_with(&owner_opts());
        let owner = pg_options(&owner);
        assert_eq!(owner, ["standard_conforming_strings=on"]);
    }

    /// Verbose sessions read the script with `-f -`, so psql prefixes an error with
    /// `psql:<stdin>:<line>:` (it doesn't for plain stdin), which the merge maps to its step
    /// (found live in T13: phase 1 failures were never named).
    #[test]
    fn verbose_sessions_name_the_failing_line() {
        let verbose = SqlOpts {
            verbose_errors: true,
            user: Some("postgres".into()),
            ..SqlOpts::admin_default()
        };
        let cmd = admin_psql_command_with("web_1", &verbose).unwrap();
        assert!(
            cmd.windows(2).any(|w| w[0] == "-f" && w[1] == "-"),
            "{cmd:?}"
        );
        assert!(!admin_psql_command("web_1").unwrap().contains(&"-f".to_string()));
    }

    /// `neon.forward_ddl` is superuser-only: the owner's session (E15) can't set it, and its
    /// connection would fail with `permission denied to set parameter` (found live in T13).
    #[test]
    fn the_owner_session_leaves_forwarding_alone() {
        let owner = SqlOpts {
            user: Some("postgres".into()),
            app_name: Some("chelabase-merge-x".into()),
            ..SqlOpts::admin_default()
        };
        let env = admin_exec_env_with(&owner);
        assert!(
            !env.iter().any(|e| e.contains("neon.forward_ddl")),
            "{env:?}"
        );
        assert!(env.contains(&"PGCONNECT_TIMEOUT=5".to_string()), "{env:?}");
        assert!(env.contains(&"PGAPPNAME=chelabase-merge-x".to_string()), "{env:?}");
    }
}
