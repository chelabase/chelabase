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
    ContainerCreateBody, ContainerStateStatusEnum, HostConfig, NetworkConnectRequest,
    RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    CreateContainerOptions, DownloadFromContainerOptions, ListContainersOptions,
    RemoveContainerOptions, StartContainerOptions, UploadToContainerOptions,
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

/// A compute's container state, as far as callers care.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComputeStatus {
    Running,
    /// Docker's state, e.g. "exited".
    NotRunning(String),
}

/// How computes are run.
#[derive(Debug, Clone)]
pub struct ComputeSettings {
    pub image: String,
    pub network: String,
    pub control_plane_uri: String,
    /// The stack the computes belong to: in their names and labels, and the only ones listed.
    pub stack: String,
    /// The network the orchestrator reaches computes' Postgres on (member logins come from
    /// its address there). A compute joins it after create, before start; when it is
    /// `network` itself there is nothing to join.
    pub control_network: String,
    /// The CA that issues each compute's server certificate at create. `None` where computes
    /// are never created (the orchestrator): [`ComputeBackend::create`] then fails.
    pub ca: Option<Arc<ComputeCa>>,
}

/// Lifecycle of computes plus admin SQL inside them.
pub trait ComputeBackend: ComputeLocator {
    fn create(
        &self,
        endpoint_id: &str,
        compute_token: &str,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn delete(&self, endpoint_id: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
    /// The contents of the file at `path` in the compute's container (at most 1 MiB).
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
    /// Endpoint ids of every one of this stack's computes (ENDPOINT_LABEL and STACK_LABEL),
    /// running or not.
    fn list(&self) -> impl Future<Output = anyhow::Result<Vec<String>>> + Send;
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
pub fn create_body(
    endpoint_id: &str,
    compute_token: &str,
    settings: &ComputeSettings,
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
            network_mode: Some(settings.network.clone()),
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                maximum_retry_count: None,
            }),
            memory: Some(MEMORY_BYTES),
            nano_cpus: Some(NANO_CPUS),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// psql as cloud_admin against `database` on the compute's own Postgres, with terse errors
/// (no statement excerpt, so a verifier in the script never reaches the error). A plain identifier
/// goes in as is; any other name as a quoted conninfo (`dbname='…'`), so psql never reads it
/// as options or connection parameters. Refused: an empty name, one over 63 bytes (Postgres'
/// limit) or one with a NUL (it can't be an argument).
pub fn admin_psql_command(database: &str) -> anyhow::Result<Vec<String>> {
    admin_psql_command_with(database, &SqlOpts::admin_default())
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

/// Environment variables for admin_sql execution.
#[cfg(test)]
fn admin_exec_env() -> Vec<String> {
    admin_exec_env_with(&SqlOpts::admin_default())
}

/// [`admin_exec_env`] plus `PGAPPNAME` when `opts` names the session. DDL forwarding is turned
/// off only for `cloud_admin`: `neon.forward_ddl` is superuser-only, so another role's
/// connection would be refused (`permission denied to set parameter`). The owner's merge
/// sessions (E15) run no role or database DDL (P8 refuses it), so forwarding has nothing to do.
fn admin_exec_env_with(opts: &SqlOpts) -> Vec<String> {
    let mut env = vec!["PGCONNECT_TIMEOUT=5".to_string()];
    if opts.user.is_none() {
        env.push("PGOPTIONS=-c neon.forward_ddl=off".to_string());
    }
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

/// [`ComputeBackend`] over the Docker Engine API.
pub struct DockerBackend {
    locator: DockerLocator,
    settings: ComputeSettings,
}

impl DockerBackend {
    /// Nothing is contacted yet.
    pub fn connect(settings: ComputeSettings) -> anyhow::Result<Self> {
        Ok(Self {
            locator: DockerLocator::connect(&settings.network, &settings.stack)?,
            settings,
        })
    }

    /// A backend over an existing Docker client (tests aim one at a mock engine).
    pub fn with_docker(settings: ComputeSettings, docker: Docker) -> Self {
        Self {
            locator: DockerLocator::with_docker(docker, &settings.network, &settings.stack),
            settings,
        }
    }

    fn docker(&self) -> &Docker {
        &self.locator.docker
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
        let options = RemoveContainerOptions {
            force: true,
            v: true,
            link: false,
        };
        match self.docker().remove_container(name, Some(options)).await {
            Err(e) if is_status(&e, 404) => Ok(()),
            other => other,
        }
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
        let body = create_body(endpoint_id, compute_token, &self.settings);
        let create = || {
            self.docker().create_container(
                Some(CreateContainerOptions {
                    name: Some(name.clone()),
                    ..Default::default()
                }),
                body.clone(),
            )
        };
        let created = match create().await {
            Err(e) if is_status(&e, 409) => {
                self.remove(&name)
                    .await
                    .with_context(|| format!("removing old container {name}"))?;
                create().await
            }
            other => other,
        }
        .with_context(|| format!("creating container {name}"))?;
        // Everything after the container exists: a failure removes it again (best effort; the
        // step's own error is what the caller sees), so no stopped container is left behind.
        let steps = async {
            if self.settings.control_network != self.settings.network {
                self.docker()
                    .connect_network(
                        &self.settings.control_network,
                        NetworkConnectRequest {
                            container: created.id.clone(),
                            endpoint_config: None,
                        },
                    )
                    .await
                    .with_context(|| {
                        format!(
                            "connecting {name} to network {}",
                            self.settings.control_network
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
            return Err(e);
        }
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
                Some(ContainerStateStatusEnum::RUNNING) => ComputeStatus::Running,
                Some(other) => ComputeStatus::NotRunning(other.to_string()),
                None => ComputeStatus::NotRunning("unknown".to_string()),
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

    async fn admin_sql_with(
        &self,
        endpoint_id: &str,
        database: &str,
        sql: &str,
        opts: &SqlOpts,
    ) -> anyhow::Result<String> {
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

/// An in-memory [`ComputeBackend`] for tests.
#[cfg(any(test, feature = "fake"))]
pub mod fake {
    use std::collections::HashMap;
    use std::net::{IpAddr, SocketAddr};
    use std::sync::{Arc, Mutex, MutexGuard};

    use std::time::Duration;

    use anyhow::bail;

    use super::{ComputeBackend, ComputeStatus, SqlOpts, SqlTimedOut};
    use crate::compute::ComputeLocator;

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
            Ok(())
        }

        async fn status(&self, endpoint_id: &str) -> anyhow::Result<Option<ComputeStatus>> {
            Ok(lock(&self.computes).get(endpoint_id).map(|c| {
                if c.running {
                    ComputeStatus::Running
                } else {
                    ComputeStatus::NotRunning("exited".to_string())
                }
            }))
        }

        async fn list(&self) -> anyhow::Result<Vec<String>> {
            let mut ids: Vec<String> = lock(&self.computes).keys().cloned().collect();
            ids.sort();
            Ok(ids)
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

    fn settings() -> ComputeSettings {
        ComputeSettings {
            image: "neon/compute-node-v17:test".to_string(),
            network: "chelabase-feat-x_default".to_string(),
            control_plane_uri: "http://orchestrator:8083".to_string(),
            stack: "feat-x".to_string(),
            control_network: "chelabase-feat-x_control".to_string(),
            ca: None,
        }
    }

    // The orchestrator's backend has no CA: creating a compute fails before Docker is asked.
    #[tokio::test]
    async fn create_without_a_ca_fails_before_docker() {
        let backend = DockerBackend::connect(settings()).unwrap();
        let err = backend.create("ep-abc123", "t").await.unwrap_err();
        assert!(format!("{err:#}").contains("no compute CA"), "{err:#}");
    }

    #[test]
    fn container_body_runs_compute_ctl_with_the_token_in_env() {
        let body = create_body("ep-abc123", "s3cret-token", &settings());
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
        let host = create_body("ep-abc123", "t", &settings())
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
    fn computes_publish_nothing_on_the_host() {
        let body = create_body("ep-abc123", "t", &settings());
        assert_eq!(body.exposed_ports, None);
        let host = body.host_config.unwrap();
        assert!(host.port_bindings.as_ref().is_none_or(|b| b.is_empty()));
        assert_ne!(host.publish_all_ports, Some(true));
    }

    #[test]
    fn computes_carry_their_stack_label() {
        let labels = create_body("ep-abc123", "t", &settings()).labels.unwrap();
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
            Some(ComputeStatus::NotRunning("exited".to_string()))
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
        assert_eq!(env[..2], admin_exec_env()[..]);
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
        assert_eq!(env.len(), 2);
        assert_eq!(env[0], "PGCONNECT_TIMEOUT=5");
        assert_eq!(env[1], "PGOPTIONS=-c neon.forward_ddl=off");
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
            !env.iter().any(|e| e.starts_with("PGOPTIONS=")),
            "{env:?}"
        );
        assert!(env.contains(&"PGCONNECT_TIMEOUT=5".to_string()), "{env:?}");
        assert!(env.contains(&"PGAPPNAME=chelabase-merge-x".to_string()), "{env:?}");
    }
}
