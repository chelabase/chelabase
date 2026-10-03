//! Creating, inspecting and removing computes, and running admin SQL inside them.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use anyhow::{Context, bail};
use bollard::Docker;
use bollard::errors::Error as DockerError;
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::models::{
    ContainerCreateBody, ContainerStateStatusEnum, HostConfig, PortBinding, RestartPolicy,
    RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    CreateContainerOptions, ListContainersOptions, RemoveContainerOptions, StartContainerOptions,
};
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

use crate::compute::{
    COMPUTE_CTL_PORT, COMPUTE_TOKEN_ENV, ComputeLocator, DockerLocator, ENDPOINT_LABEL,
    POSTGRES_PORT, compute_ctl_args, container_name,
};

const ADMIN_SQL_TIMEOUT: Duration = Duration::from_secs(30);
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
}

/// Lifecycle of computes plus admin SQL inside them.
pub trait ComputeBackend: ComputeLocator {
    fn create(
        &self,
        endpoint_id: &str,
        compute_token: &str,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn delete(&self, endpoint_id: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn status(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = anyhow::Result<Option<ComputeStatus>>> + Send;
    /// Endpoint ids of every container carrying ENDPOINT_LABEL, running or not.
    fn list(&self) -> impl Future<Output = anyhow::Result<Vec<String>>> + Send;
    /// Runs `sql` (stdin) with psql as cloud_admin inside the compute; returns stdout.
    fn admin_sql(
        &self,
        endpoint_id: &str,
        database: &str,
        sql: &str,
    ) -> impl Future<Output = anyhow::Result<String>> + Send;
}

/// The container to create for an endpoint. The token goes in the environment, never argv.
/// compute_ctl and Postgres are published on loopback only, each on a random host port.
pub fn create_body(
    endpoint_id: &str,
    compute_token: &str,
    settings: &ComputeSettings,
) -> ContainerCreateBody {
    let ports = [COMPUTE_CTL_PORT, POSTGRES_PORT].map(|p| format!("{p}/tcp"));
    let loopback = || {
        Some(vec![PortBinding {
            host_ip: Some("127.0.0.1".to_string()),
            host_port: Some("0".to_string()),
        }])
    };
    ContainerCreateBody {
        image: Some(settings.image.clone()),
        entrypoint: Some(vec!["/usr/local/bin/compute_ctl".to_string()]),
        cmd: Some(compute_ctl_args(endpoint_id, &settings.control_plane_uri)),
        env: Some(vec![format!("{COMPUTE_TOKEN_ENV}={compute_token}")]),
        labels: Some([(ENDPOINT_LABEL.to_string(), endpoint_id.to_string())].into()),
        exposed_ports: Some(ports.to_vec()),
        host_config: Some(HostConfig {
            network_mode: Some(settings.network.clone()),
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                maximum_retry_count: None,
            }),
            memory: Some(MEMORY_BYTES),
            nano_cpus: Some(NANO_CPUS),
            extra_hosts: Some(vec!["host.docker.internal:host-gateway".to_string()]),
            port_bindings: Some(ports.into_iter().map(|p| (p, loopback())).collect()),
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
        "VERBOSITY=terse",
        "-h",
        "localhost",
        "-U",
        "cloud_admin",
        "-d",
    ]
    .map(String::from)
    .into();
    cmd.push(target);
    Ok(cmd)
}

fn is_status(err: &DockerError, code: u16) -> bool {
    matches!(err, DockerError::DockerResponseServerError { status_code, .. } if *status_code == code)
}

/// Environment variables for admin_sql execution.
fn admin_exec_env() -> Vec<String> {
    vec![
        "PGCONNECT_TIMEOUT=5".to_string(),
        "PGOPTIONS=-c neon.forward_ddl=off".to_string(),
    ]
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
            locator: DockerLocator::connect(&settings.network)?,
            settings,
        })
    }

    fn docker(&self) -> &Docker {
        &self.locator.docker
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
        sql: &str,
    ) -> anyhow::Result<String> {
        let docker = self.docker();
        let name = container_name(endpoint_id);
        let exec = docker
            .create_exec(
                &name,
                CreateExecOptions {
                    attach_stdin: Some(true),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    user: Some("postgres".to_string()),
                    env: Some(admin_exec_env()),
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
    async fn create(&self, endpoint_id: &str, compute_token: &str) -> anyhow::Result<()> {
        let name = container_name(endpoint_id);
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
        self.docker()
            .start_container(&created.id, None::<StartContainerOptions>)
            .await
            .with_context(|| format!("starting container {name}"))?;
        Ok(())
    }

    async fn delete(&self, endpoint_id: &str) -> anyhow::Result<()> {
        let name = container_name(endpoint_id);
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
                filters: Some([("label".to_string(), vec![ENDPOINT_LABEL.to_string()])].into()),
                ..Default::default()
            }))
            .await
            .context("listing compute containers")?;
        Ok(containers
            .into_iter()
            .filter_map(|c| c.labels?.remove(ENDPOINT_LABEL))
            .collect())
    }

    async fn admin_sql(
        &self,
        endpoint_id: &str,
        database: &str,
        sql: &str,
    ) -> anyhow::Result<String> {
        let cmd = admin_psql_command(database)?;
        tokio::time::timeout(ADMIN_SQL_TIMEOUT, self.run_exec(endpoint_id, cmd, sql))
            .await
            .with_context(|| format!("psql in compute {endpoint_id} timed out"))?
    }
}

/// An in-memory [`ComputeBackend`] for tests.
#[cfg(any(test, feature = "fake"))]
pub mod fake {
    use std::collections::HashMap;
    use std::net::{IpAddr, SocketAddr};
    use std::sync::{Arc, Mutex, MutexGuard};

    use anyhow::bail;

    use super::{ComputeBackend, ComputeStatus};
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
        pub journal: Journal,
        pub fail_create: Mutex<Option<String>>,
        /// Each admin_sql call fails with "not ready" and decrements this while it's above 0.
        pub sql_failures_left: Mutex<u32>,
        /// `(needle, message)`: SQL containing the needle fails with the message.
        pub fail_sql_containing: Mutex<Option<(String, String)>>,
        /// Needles set by [`FakeBackend::fail_when`].
        fail_needles: Mutex<Vec<String>>,
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

        /// From now on, SQL containing `needle` fails with "injected failure" (on top of any
        /// earlier needles).
        pub fn fail_when(&self, needle: &str) {
            lock(&self.fail_needles).push(needle.to_string());
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

        async fn admin_sql(
            &self,
            endpoint_id: &str,
            database: &str,
            sql: &str,
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
            lock(&self.sql).push((
                endpoint_id.to_string(),
                database.to_string(),
                sql.to_string(),
            ));
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
            network: "cheladb_default".to_string(),
            control_plane_uri: "http://host.docker.internal:8083".to_string(),
        }
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
            compute_ctl_args("ep-abc123", "http://host.docker.internal:8083")
        );
        assert_eq!(
            body.env,
            Some(vec!["NEON_CONTROL_PLANE_TOKEN=s3cret-token".to_string()])
        );
        assert_eq!(
            body.labels.unwrap().get("cheladb.dev/endpoint-id"),
            Some(&"ep-abc123".to_string())
        );
        assert!(cmd.iter().all(|arg| !arg.contains("s3cret-token")));
    }

    #[test]
    fn container_body_limits_restarts_and_networks() {
        let host = create_body("ep-abc123", "t", &settings())
            .host_config
            .unwrap();
        assert_eq!(host.network_mode.as_deref(), Some("cheladb_default"));
        assert_eq!(
            host.restart_policy.unwrap().name,
            Some(RestartPolicyNameEnum::UNLESS_STOPPED)
        );
        assert_eq!(host.memory, Some(2 * 1024 * 1024 * 1024));
        assert_eq!(host.nano_cpus, Some(1_000_000_000));
        assert!(
            host.extra_hosts
                .unwrap()
                .contains(&"host.docker.internal:host-gateway".to_string())
        );
    }

    #[test]
    fn compute_ctl_is_published_on_loopback_only() {
        let body = create_body("ep-abc123", "t", &settings());
        assert!(
            body.exposed_ports
                .unwrap()
                .contains(&"3080/tcp".to_string())
        );
        let bindings = body.host_config.unwrap().port_bindings.unwrap();
        let binding = &bindings["3080/tcp"].as_ref().unwrap()[0];
        assert_eq!(binding.host_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(binding.host_port.as_deref(), Some("0"));
    }

    #[test]
    fn create_body_publishes_postgres_on_loopback() {
        let body = create_body("ep-abc123", "t", &settings());
        assert_eq!(
            body.exposed_ports,
            Some(vec!["3080/tcp".to_string(), "5432/tcp".to_string()])
        );
        let bindings = body.host_config.unwrap().port_bindings.unwrap();
        assert_eq!(bindings.len(), 2);
        let binding = &bindings["5432/tcp"].as_ref().unwrap()[0];
        assert_eq!(binding.host_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(binding.host_port.as_deref(), Some("0"));
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

    #[test]
    fn admin_exec_turns_forwarding_off() {
        let env = admin_exec_env();
        assert_eq!(env.len(), 2);
        assert_eq!(env[0], "PGCONNECT_TIMEOUT=5");
        assert_eq!(env[1], "PGOPTIONS=-c neon.forward_ddl=off");
    }
}
