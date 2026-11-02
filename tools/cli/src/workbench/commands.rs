//! `chelabase workbench …`: one stack's lifecycle (W2 spec sections 4 and 6). Every
//! `docker compose` call goes through [`Compose`], so it names the stack's project.

use super::argv::{Compose, nextest_archive_args, nextest_run_args};
use super::env::{
    StackEnvInput, check_control_subnet, example_scheme, parse_network_inspect, render_stack_env,
};
use super::migrate::migrate_main_layout;
use super::registry::{Entry, Registry, port_is_free, with_registry};
use super::slots::{
    SlotOptions, acquire_slot, cap_from_env, drop_tickets, queued, release_slot, touch_slot,
    wait_from_env,
};
use super::slug::{Hosts, MAIN, hosts, project_name, slug_for_dir};
use super::target::{
    Target, archive_path, check_up_allowed, base_stack_slug, check_ready, read_recorded, resolve_target,
};
use anyhow::{Context as _, bail};
use chelabase_neon::compute::{ENDPOINT_LABEL, STACK_LABEL};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A site `up --with` adds (Compose profile `sites`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Site {
    Docs,
    Landing,
}

impl Site {
    fn service(self) -> &'static str {
        match self {
            Site::Docs => "docs",
            Site::Landing => "landing",
        }
    }
}

/// The Rust apps the stack runs from `<checkout>/target/debug`.
pub(super) const RUST_APPS: [&str; 6] =
    ["orchestrator", "worker", "gateway", "auth", "rest", "realtime"];
/// The apps that read the bootstrap's `demo.env` (the OIDC audiences, the demo ids).
pub(super) const DEMO_ENV_READERS: &[&str] =
    &["orchestrator", "worker", "gateway", "auth", "rest", "realtime"];
/// Every app container (the Rust apps and Studio; the sites are added by `--with`).
pub(super) const APPS: [&str; 7] = [
    "orchestrator",
    "worker",
    "gateway",
    "auth",
    "rest",
    "realtime",
    "studio",
];
/// The long-running infra services. Their one-shot helpers (`create-bucket`, `proxy-tls`,
/// `register-safekeepers`) start as dependencies of the services that wait on them.
pub(super) const INFRA: &[&str] = &[
    "metadata-db",
    "nats",
    "valkey",
    "zitadel-db",
    "seaweedfs",
    "storage-broker",
    "safekeeper1",
    "safekeeper2",
    "safekeeper3",
    "storcon-db",
    "storage-controller",
    "pageserver",
    "proxy",
];
/// Started with the infra: the bootstrap reaches Zitadel through the gateway.
const FRONT: &[&str] = &["zitadel", "mailpit", "gateway"];
/// Started after the first bootstrap.
pub(super) const LATE_APPS: &[&str] =
    &["orchestrator", "worker", "auth", "rest", "realtime", "studio"];
/// A stack that took its slot this recently is alive even with no running container: `up` is
/// still building the image and the binaries (T3 review I2).
const REAP_GRACE: u64 = 15 * 60;
/// The proxy's Postgres and HTTP ports on the stack network (the runner's view).
const PROXY_PG_PORT_INSIDE: &str = "4432";
const PROXY_HTTP_PORT_INSIDE: &str = "4444";

/// The base compose file, relative to a checkout.
pub(crate) const COMPOSE_FILE: &str = "infra/compose/docker-compose.yml";
/// The workbench overlay (the dev loop's images, commands, mounts, ports and dev-only services),
/// applied on top of [`COMPOSE_FILE`] by every workbench compose call.
pub(crate) const OVERLAY_FILE: &str = "infra/compose/docker-compose.workbench.yml";

/// One stack, as seen from the checkout that runs the command.
#[derive(Debug, Clone)]
pub struct Stack {
    pub slug: String,
    pub project: String,
    /// The stack's checkout (the current one, or the registry's for `--stack`).
    pub checkout: PathBuf,
    /// The main checkout (its `.chelabase` holds the keys, the registry and every state dir).
    pub main: PathBuf,
    /// `<main>/.chelabase/stacks`.
    pub stacks_dir: PathBuf,
    /// `<stacks_dir>/<slug>`.
    pub state: PathBuf,
    /// The base compose file (`infra/compose/docker-compose.yml`).
    pub compose_file: PathBuf,
    /// The workbench overlay on it (`infra/compose/docker-compose.workbench.yml`): every compose
    /// call passes both, base first.
    pub overlay_file: PathBuf,
    pub env_file: PathBuf,
}

impl Stack {
    pub(crate) fn new(slug: &str, checkout: &Path, main: &Path) -> Self {
        let stacks_dir = stacks_dir(main);
        let state = stacks_dir.join(slug);
        Stack {
            slug: slug.to_string(),
            project: project_name(slug),
            checkout: checkout.to_path_buf(),
            main: main.to_path_buf(),
            compose_file: checkout.join(COMPOSE_FILE),
            overlay_file: checkout.join(OVERLAY_FILE),
            env_file: state.join("env/stack.env"),
            stacks_dir,
            state,
        }
    }

    pub(crate) fn compose(&self) -> Compose<'_> {
        Compose {
            project: &self.project,
            files: vec![&self.compose_file, &self.overlay_file],
            env_file: &self.env_file,
        }
    }

    /// `docker compose -p … <rest…>` with the stack's env (see [`compose_command`]).
    fn compose_cmd(&self, rest: &[&str]) -> anyhow::Result<Command> {
        compose_command(&self.env_file, self.compose().args(rest))
    }

    /// `docker compose -p … <rest…>`, stdio inherited; a failure is an error.
    fn compose_run(&self, rest: &[&str]) -> anyhow::Result<()> {
        run(self.compose_cmd(rest)?)
    }

    /// `docker compose -p … <rest…>`'s stdout; a failure is an error.
    fn compose_output(&self, rest: &[&str]) -> anyhow::Result<String> {
        output(self.compose_cmd(rest)?)
    }

    /// Whether any of the stack's containers is running.
    fn has_running_containers(&self) -> anyhow::Result<bool> {
        Ok(!self.running_services()?.is_empty())
    }

    /// The services with a running container (none before the first `up`).
    fn running_services(&self) -> anyhow::Result<Vec<String>> {
        if !self.env_file.is_file() {
            return Ok(Vec::new());
        }
        Ok(self
            .compose_output(&[
                "--profile",
                "sites",
                "ps",
                "--services",
                "--status",
                "running",
            ])?
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect())
    }

    fn binaries_file(&self) -> PathBuf {
        self.state.join("binaries.json")
    }

    fn runner_dir(&self) -> PathBuf {
        self.state.join("runner")
    }
}

fn stacks_dir(main: &Path) -> PathBuf {
    main.join(".chelabase/stacks")
}

/// The stack of the checkout the command runs in: `git rev-parse` from the cwd. The main
/// checkout's stack is `main`; a worktree's is its `.worktrees/` directory's slug.
pub fn current_stack() -> anyhow::Result<Stack> {
    let checkout = PathBuf::from(git(&["rev-parse", "--show-toplevel"])?);
    let common = PathBuf::from(git(&[
        "rev-parse",
        "--path-format=absolute",
        "--git-common-dir",
    ])?);
    let main = common
        .parent()
        .with_context(|| format!("git's common dir {} has no parent", common.display()))?
        .to_path_buf();
    let slug = slug_for_checkout(&checkout, &main)?;
    Ok(Stack::new(&slug, &checkout, &main))
}

fn slug_for_checkout(checkout: &Path, main: &Path) -> anyhow::Result<String> {
    if checkout == main {
        return Ok(MAIN.to_string());
    }
    let dir = checkout
        .strip_prefix(main.join(".worktrees"))
        .ok()
        .filter(|rest| rest.components().count() == 1)
        .and_then(|rest| rest.to_str());
    match dir {
        Some(dir) => slug_for_dir(dir),
        None => bail!(
            "{} is neither the main checkout ({}) nor a worktree directly under {}: a workbench \
             stack belongs to one of those (`just wt-add <branch>`)",
            checkout.display(),
            main.display(),
            main.join(".worktrees").display()
        ),
    }
}

/// The stack `--stack <slug>` names (from the registry), or the current checkout's.
fn chosen_stack(here: &Stack, slug: Option<&str>) -> anyhow::Result<(Stack, Option<Entry>)> {
    let registry = read_registry(&here.stacks_dir)?;
    let slug = slug.map(str::trim).unwrap_or(&here.slug).to_string();
    let Some(entry) = registry.get(&slug).cloned() else {
        if slug == here.slug {
            return Ok((here.clone(), None));
        }
        let known: Vec<&str> = registry.stacks.iter().map(|e| e.slug.as_str()).collect();
        bail!(
            "no stack {slug:?} is registered; known stacks: {}",
            if known.is_empty() {
                "none".to_string()
            } else {
                known.join(", ")
            }
        );
    };
    let mut stack = Stack::new(&slug, &entry.checkout, &here.main);
    if !stack.compose_file.is_file() || !stack.overlay_file.is_file() {
        // The stack's checkout is gone: the compose files are the same everywhere.
        stack.compose_file = here.compose_file.clone();
        stack.overlay_file = here.overlay_file.clone();
    }
    Ok((stack, Some(entry)))
}

/// The registry without taking its lock or creating anything (`status`, lookups). A missing
/// registry is empty; the file is replaced by rename, so a read is never torn.
fn read_registry(stacks_dir: &Path) -> anyhow::Result<Registry> {
    let path = stacks_dir.join("registry.json");
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text)
            .with_context(|| format!("{} is not a valid stack registry", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Registry::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

// --- pure helpers (lifecycle_tests) ---

/// The apps whose binary changed since they were started: every app of `current` whose mtime
/// differs from (or is missing in) `recorded`, sorted.
pub fn stale_apps(recorded: &HashMap<String, u64>, current: &HashMap<String, u64>) -> Vec<String> {
    let mut stale: Vec<String> = current
        .iter()
        .filter(|(app, mtime)| recorded.get(*app) != Some(*mtime))
        .map(|(app, _)| app.clone())
        .collect();
    stale.sort();
    stale
}

/// The apps the bootstrap waits on: the orchestrator (it applies the metadata migrations and
/// is the Actions target's host). None of them may need a file only the bootstrap writes
/// (the orchestrator starts without demo.env and actions.env), since they are waited on first.
pub const BOOTSTRAP_DEPENDENCIES: &[&str] = &["orchestrator"];

/// The apps `up` recreates before the bootstrap (B3, B4): on a running stack, those of
/// [`BOOTSTRAP_DEPENDENCIES`] whose binary changed since they started, so the bootstrap never
/// waits on an old build. No other app: one that reads the bootstrap's files (auth needs
/// demo.env's org id) can't become healthy before it has run, so it is recreated after it
/// ([`recreate_after_bootstrap`]). A stack that wasn't running starts every app on its current
/// binary.
pub fn recreate_before_bootstrap(
    was_running: bool,
    recorded: &HashMap<String, u64>,
    current: &HashMap<String, u64>,
) -> Vec<String> {
    if !was_running {
        return Vec::new();
    }
    stale_apps(recorded, current)
        .into_iter()
        .filter(|a| BOOTSTRAP_DEPENDENCIES.contains(&a.as_str()))
        .collect()
}

/// The apps `up` recreates after the bootstrap: on a running stack, every app whose binary
/// changed and that wasn't recreated before it (`rebuilt`), and on any stack every reader of
/// an env file the bootstrap changed (`env_changed`), sorted and deduped.
pub fn recreate_after_bootstrap(
    was_running: bool,
    recorded: &HashMap<String, u64>,
    current: &HashMap<String, u64>,
    rebuilt: &[String],
    env_changed: &[String],
) -> Vec<String> {
    let mut apps: Vec<String> = if was_running {
        stale_apps(recorded, current)
            .into_iter()
            .filter(|a| !rebuilt.contains(a))
            .collect()
    } else {
        Vec::new()
    };
    apps.extend(env_changed.iter().cloned());
    apps.sort();
    apps.dedup();
    apps
}

/// Whether a container is a compute the W1 layout started: it has the endpoint label, no stack
/// label, and a `chelabase-compute-` name (Docker's leading `/` stripped). A worktree stack whose
/// slug starts with `compute` names its services `chelabase-compute-…` too, but they carry no
/// endpoint label, and its computes carry the stack label (T5 review I1).
pub fn is_legacy_compute(name: &str, labels: &HashMap<String, String>) -> bool {
    let name = name.strip_prefix('/').unwrap_or(name);
    labels.contains_key(ENDPOINT_LABEL)
        && !labels.contains_key(STACK_LABEL)
        && name.starts_with("chelabase-compute-")
}

/// Main may be purged only when named (`--stack main`) and confirmed (`--yes`), so a purge run
/// from a directory that resolves to main by accident (a removed worktree) can't reach it. Other
/// stacks need neither (T5 review I4).
pub fn check_purge(slug: &str, named: Option<&str>, yes: bool) -> anyhow::Result<()> {
    if slug != MAIN {
        return Ok(());
    }
    if named.map(str::trim) != Some(MAIN) {
        bail!(
            "this checkout's stack is main; purging main deletes its data (every database, its \
             Zitadel instance and its state), so it must be named: `just workbench purge --stack \
             main --yes`"
        );
    }
    if !yes {
        bail!(
            "purging main deletes its data (every database, its Zitadel instance and its state); \
             run `just workbench purge --stack main --yes` if you mean it"
        );
    }
    Ok(())
}

/// A `docker <args…>` command with every `KEY=VALUE` of the stack's env file set in its
/// environment, so a variable the calling shell exports can't override the stack's own in
/// Compose's interpolation (T5 review I5).
pub fn compose_command(env_file: &Path, args: Vec<String>) -> anyhow::Result<Command> {
    if !env_file.is_file() {
        bail!(
            "{} doesn't exist: the stack hasn't been up yet (`just workbench up`)",
            env_file.display()
        );
    }
    let mut cmd = docker(args);
    cmd.envs(crate::paths::load_dotenv_file(env_file));
    Ok(cmd)
}

/// Creates the stack's Zitadel dir private to the stack user (0700): Zitadel runs as that user
/// and writes its PATs there. Idempotent.
pub fn prepare_zitadel_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("making {} private", dir.display()))
}

/// `up`'s key phase, before any container mounts the files: every key, secret and auth file
/// through `chelabase_init::ensure_secrets` (created once, never overwritten). The machine part
/// goes to the shared keys dir (`<main>/.chelabase`), the stack's own part (the NATS and Valkey
/// auth files, the database passwords, the Zitadel files, the setup code) to its state dir.
/// Reports which auth file changed: a running NATS or Valkey must restart to read it (R9).
pub fn ensure_stack_secrets(
    main: &Path,
    state: &Path,
) -> anyhow::Result<chelabase_init::kinds::service_auth::AuthFilesChanged> {
    crate::paths::migrate_auth_dir(main).context("moving .chelabase/ui-auth to .chelabase/auth")?;
    let report = chelabase_init::ensure_secrets(
        &main.join(".chelabase"),
        &chelabase_init::InitSecrets::workbench(state),
    )?;
    ensure_object_store_dev_files(state).context("writing the object store's dev keys")?;
    if report.setup_code.is_some() {
        eprintln!(
            "[workbench] the stack's setup code is in {}",
            state.join("setup/setup.code").display()
        );
    }
    Ok(report.auth_files)
}

/// The fixed dev S3 keys of SeaweedFS's `storage` identity (bucket `chelabase-storage`) and its
/// `admin` identity (create-bucket only), as files in the stack's state dir beside Neon's `s3/`
/// pair. The workbench's compose overlay mounts them as secrets, so they are not in
/// `docker inspect` (`scripts/tests/workbench-compose.sh` pins the mounts).
const OBJECT_STORE_DEV_KEYS: [(&str, &str); 4] = [
    ("storage-s3/access-key", "chelabase-storage-dev"),
    ("storage-s3/secret-key", "chelabase-storage-dev-secret"),
    ("s3-admin/access-key", "chelabase-admin-dev"),
    ("s3-admin/secret-key", "chelabase-admin-dev-secret"),
];

/// Writes [`OBJECT_STORE_DEV_KEYS`] under `state` (dirs 0700, files 0600) when missing; a file
/// that exists is never touched, so a stack made before these files gains them and nothing else.
fn ensure_object_store_dev_files(state: &Path) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
    for (rel, value) in OBJECT_STORE_DEV_KEYS {
        let path = state.join(rel);
        if path.exists() {
            continue;
        }
        let dir = path.parent().expect("a file below the state dir");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("creating {}", dir.display()))?;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut f| f.write_all(value.as_bytes()))
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

/// Sets each PAT file to 0600; a missing file is an error.
pub fn secure_pats(paths: &[PathBuf]) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    for p in paths {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting {} to 0600", p.display()))?;
    }
    Ok(())
}

/// `CHELABASE_NEXTEST_BIN` in `stack.env`: the host's cargo-nextest, else `/dev/null`, so the
/// runner's bind mount always has a source and the runner starts (T5 review M2).
pub fn nextest_bin_for_env(found: Option<&Path>) -> String {
    found.map_or_else(|| "/dev/null".to_string(), |p| p.display().to_string())
}

/// The services of `expected` with no running container, in `expected`'s order (T5 review M3).
pub fn missing_services(expected: &[&str], running: &[String]) -> Vec<String> {
    expected
        .iter()
        .filter(|s| !running.iter().any(|r| r == *s))
        .map(|s| s.to_string())
        .collect()
}

/// `docker` arguments listing a project's running containers by Compose's project label: no
/// compose file is needed, so a stack whose checkout changed is still seen (T5 review M4).
pub fn alive_ps_args(project: &str) -> Vec<String> {
    [
        "ps".to_string(),
        "-q".to_string(),
        "--filter".to_string(),
        format!("label=com.docker.compose.project={project}"),
        "--filter".to_string(),
        "status=running".to_string(),
    ]
    .into()
}

/// Stops `pids`: SIGTERM, then a wait of up to `grace` for each to exit (gone, or a zombie), then
/// SIGKILL for the rest and a short wait. A pid that doesn't exist is fine (T5 review I3).
pub fn stop_processes(pids: &[u32], grace: Duration) -> anyhow::Result<()> {
    signal(pids, "TERM")?;
    if wait_gone(pids, grace) {
        return Ok(());
    }
    let left: Vec<u32> = pids.iter().copied().filter(|p| process_lives(*p)).collect();
    signal(&left, "KILL")?;
    if !wait_gone(&left, Duration::from_secs(2)) {
        let still: Vec<String> = left
            .iter()
            .filter(|p| process_lives(**p))
            .map(u32::to_string)
            .collect();
        bail!("pids {} survived SIGKILL", still.join(", "));
    }
    Ok(())
}

/// `kill -<sig> <pid>` for each pid; a pid that is already gone is fine.
fn signal(pids: &[u32], sig: &str) -> anyhow::Result<()> {
    for pid in pids {
        Command::new("kill")
            .arg(format!("-{sig}"))
            .arg(pid.to_string())
            .stderr(Stdio::null())
            .status()
            .context("running kill")?;
    }
    Ok(())
}

/// Whether `pid` exists and isn't a zombie (state `Z` in `/proc/<pid>/stat`).
fn process_lives(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // The comm field may hold spaces and parentheses: the state follows its last `)`.
    let state = stat
        .rfind(')')
        .and_then(|i| stat[i + 1..].split_whitespace().next());
    state != Some("Z")
}

/// Polls until none of `pids` lives, up to `timeout`; whether they all went.
fn wait_gone(pids: &[u32], timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if !pids.iter().any(|p| process_lives(*p)) {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The runner's `.curlrc` (`CURL_HOME`): curl resolves every `*.localhost` name to loopback
/// itself, never asking Docker's DNS, so each of the stack's host names is pointed at the
/// gateway service explicitly.
pub fn curlrc(hosts: &Hosts, gateway_port: u16) -> String {
    let mut rc = String::from(
        "# Generated by chelabase workbench: curl sends the stack's host names to its gateway.\n",
    );
    for name in hosts.names() {
        rc.push_str(&format!(
            "connect-to = \"{name}:{gateway_port}:gateway:{gateway_port}\"\n"
        ));
    }
    rc
}

/// The workbench image's tag: `chelabase-workbench:<first 12 hex of the Dockerfile's SHA-256>`.
fn image_tag(dockerfile: &[u8]) -> String {
    let hash = hex::encode(Sha256::digest(dockerfile));
    format!("chelabase-workbench:{}", &hash[..12])
}

/// `KEY=VALUE` lines of `scripts/neon-images.sh`'s stdout.
fn parse_image_lines(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|l| l.trim().split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .collect()
}

/// `K=V` pairs of `run --env`; a pair without `=` or with an empty key is an error.
fn parse_env_pairs(pairs: &[String]) -> anyhow::Result<Vec<(String, String)>> {
    pairs
        .iter()
        .map(|p| match p.split_once('=') {
            Some((k, v)) if !k.trim().is_empty() => Ok((k.trim().to_string(), v.to_string())),
            _ => bail!("--env {p:?} must be KEY=VALUE"),
        })
        .collect()
}

/// The services named by `restart` (every Rust app when none), checked.
pub(super) fn restart_targets(apps: &[String]) -> anyhow::Result<Vec<String>> {
    if apps.is_empty() {
        return Ok(RUST_APPS.iter().map(|a| a.to_string()).collect());
    }
    let known: Vec<&str> = APPS.iter().copied().chain(["docs", "landing"]).collect();
    for app in apps {
        if !known.contains(&app.as_str()) {
            bail!(
                "{app:?} isn't an app of the stack; restart one of: {}",
                known.join(", ")
            );
        }
    }
    Ok(apps.to_vec())
}

/// The Zitadel PATs (under the stack's state dir `state`) that `apps` mount as Compose secrets,
/// each once: admin.pat for the orchestrator, the worker and auth, login-client.pat for auth.
fn pat_files(state: &Path, apps: &[String]) -> Vec<PathBuf> {
    let reads = |file: &str| {
        apps.iter().any(|a| match file {
            "admin.pat" => matches!(a.as_str(), "orchestrator" | "worker" | "auth"),
            _ => a == "auth",
        })
    };
    ["admin.pat", "login-client.pat"]
        .into_iter()
        .filter(|f| reads(f))
        .map(|f| state.join("zitadel").join(f))
        .collect()
}

/// The services of `ps --format json` that failed: unhealthy, restarting, or exited non-zero.
/// Compose prints one object per line (an array in older releases).
fn failed_services(ps_json: &str) -> Vec<String> {
    let mut rows: Vec<serde_json::Value> = Vec::new();
    for line in ps_json.lines().map(str::trim).filter(|l| !l.is_empty()) {
        match serde_json::from_str::<serde_json::Value>(line) {
            Ok(serde_json::Value::Array(items)) => rows.extend(items),
            Ok(v) => rows.push(v),
            Err(_) => {}
        }
    }
    let mut failed: Vec<String> = rows
        .iter()
        .filter(|r| {
            let s = |k: &str| r.get(k).and_then(|v| v.as_str()).unwrap_or_default();
            let exit = r.get("ExitCode").and_then(|v| v.as_i64()).unwrap_or(0);
            s("Health") == "unhealthy"
                || s("State") == "restarting"
                || (s("State") == "exited" && exit != 0)
        })
        .filter_map(|r| {
            r.get("Service")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .collect();
    failed.sort();
    failed.dedup();
    failed
}

fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let s = d.as_secs();
        format!("{}m {:02}s", s / 60, s % 60)
    }
}

fn fmt_uptime(secs: u64) -> String {
    let (h, m) = (secs / 3600, secs % 3600 / 60);
    if h > 0 {
        format!("{h}h {m}m")
    } else {
        format!("{m}m")
    }
}

// --- process helpers ---

fn docker(args: Vec<String>) -> Command {
    let mut cmd = Command::new("docker");
    cmd.args(args);
    cmd
}

fn describe(cmd: &Command) -> String {
    let mut s = cmd.get_program().to_string_lossy().into_owned();
    for a in cmd.get_args() {
        s.push(' ');
        s.push_str(&a.to_string_lossy());
    }
    s
}

fn run(mut cmd: Command) -> anyhow::Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("running {}", describe(&cmd)))?;
    if !status.success() {
        bail!("{} failed ({status})", describe(&cmd));
    }
    Ok(())
}

fn output(mut cmd: Command) -> anyhow::Result<String> {
    let out = cmd
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("running {}", describe(&cmd)))?;
    if !out.status.success() {
        bail!("{} failed ({})", describe(&cmd), out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Runs `cmd` with inherited stdio and exits this process with its code when it fails (for
/// pass-through commands: logs, psql, exec, test, run).
fn pass_through(mut cmd: Command) -> anyhow::Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("running {}", describe(&cmd)))?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

fn git(args: &[&str]) -> anyhow::Result<String> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    Ok(output(cmd)
        .context("chelabase workbench runs inside a checkout of the repo")?
        .trim()
        .to_string())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn id(flag: &str) -> anyhow::Result<u32> {
    let mut cmd = Command::new("id");
    cmd.arg(flag);
    let out = output(cmd)?;
    out.trim()
        .parse()
        .with_context(|| format!("`id {flag}` printed {out:?}"))
}

fn docker_gid() -> anyhow::Result<u32> {
    use std::os::unix::fs::MetadataExt as _;
    let sock = Path::new("/var/run/docker.sock");
    Ok(std::fs::metadata(sock)
        .with_context(|| format!("reading {} (is Docker running?)", sock.display()))?
        .gid())
}

/// `cargo-nextest` on `PATH`, if there is one.
fn host_nextest() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join("cargo-nextest"))
        .find(|p| p.is_file())
}

fn mtime_nanos(path: &Path) -> Option<u64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let d = modified.duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(d.as_nanos()).ok()
}

/// One `up` phase at a time on stderr, each with its duration.
struct Phases;

impl Phases {
    fn run<T>(&self, name: &str, f: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
        eprintln!("[workbench] {name}…");
        let start = Instant::now();
        let out = f();
        let took = fmt_duration(start.elapsed());
        match &out {
            Ok(_) => eprintln!("[workbench] {name}: done in {took}"),
            Err(_) => eprintln!("[workbench] {name}: failed after {took}"),
        }
        out
    }
}

// --- up ---

/// `workbench up [--with …]`.
pub fn up(with: &[Site]) -> anyhow::Result<()> {
    let stack = current_stack()?;
    check_up_allowed(read_recorded(&stack.checkout)?.as_deref(), &stack.slug)?;
    up_stack(&stack, with)
}

fn up_stack(stack: &Stack, with: &[Site]) -> anyhow::Result<()> {
    let phases = Phases;
    let started = Instant::now();
    if stack.slug == MAIN {
        phases.run("moving main to the workbench layout", || {
            stop_legacy_dev_processes(&stack.main)?;
            for line in migrate_main_layout(&stack.main)? {
                eprintln!("[workbench]   {line}");
            }
            // Before any app container starts: main's worker no longer sees unlabelled computes
            // and would start a second compute on the same timeline (T1 review).
            remove_legacy_computes()
        })?;
    }

    let example_path = stack.checkout.join("infra/compose/.env.example");
    let example = std::fs::read_to_string(&example_path)
        .with_context(|| format!("reading {}", example_path.display()))?;
    let scheme = example_scheme(&example)?;

    let entry = with_registry(&stack.stacks_dir, |r| {
        r.ensure(&stack.slug, &stack.checkout, now(), port_is_free)
    })?;

    phases.run("taking a workbench slot", || take_slot(stack))?;

    let result = start_stack(stack, &entry, &example, with, &phases);
    match result {
        Ok(()) => {
            eprintln!(
                "[workbench] {} is up ({})",
                stack.slug,
                fmt_duration(started.elapsed())
            );
            print_urls(stack, &entry, scheme, with);
            Ok(())
        }
        Err(e) => {
            eprintln!(
                "[workbench] {} is partly up and keeps its slot; fix the error and run up again, \
                 or release the slot with `just workbench down`",
                stack.slug
            );
            Err(e)
        }
    }
}

/// Before anything is built: no other Docker network may overlap the slot's control subnet.
fn check_control_network(stack: &Stack, slot: u8) -> anyhow::Result<()> {
    let ids = output(docker(vec!["network".into(), "ls".into(), "-q".into()]))?;
    let ids: Vec<String> = ids.split_whitespace().map(str::to_string).collect();
    if ids.is_empty() {
        return Ok(());
    }
    // One network at a time: one removed since `ls` must not fail `up`.
    let mut networks = Vec::new();
    for id in ids {
        let out = docker(vec!["network".into(), "inspect".into(), id.clone()])
            .output()
            .context("running docker network inspect")?;
        networks.extend(parse_network_inspect(
            &id,
            out.status.success(),
            &String::from_utf8_lossy(&out.stdout),
            &String::from_utf8_lossy(&out.stderr),
        )?);
    }
    check_control_subnet(
        &stack.slug,
        slot,
        &format!("{}_control", stack.project),
        &networks,
    )
}

fn take_slot(stack: &Stack) -> anyhow::Result<()> {
    let opts = SlotOptions {
        cap: cap_from_env(std::env::var("CHELABASE_WORKBENCH_SLOTS").ok().as_deref())?,
        timeout: wait_from_env(std::env::var("CHELABASE_WORKBENCH_WAIT").ok().as_deref())?,
        poll: Duration::from_secs(5),
        status_every: Duration::from_secs(60),
    };
    let alive = |e: &Entry| entry_alive(e);
    acquire_slot(
        &stack.stacks_dir,
        &stack.slug,
        &opts,
        &alive,
        &now,
        &mut |line| eprintln!("[workbench] {line}"),
    )
}

/// W-1: a running entry is alive while its checkout exists and it either took its slot less
/// than [`REAP_GRACE`] ago (`up` refreshes it after each long build) or its project has a running
/// container, asked of Docker by project label ([`alive_ps_args`]). A failed check counts as
/// alive (a slot is never taken away on an error).
fn entry_alive(e: &Entry) -> bool {
    if !e.checkout.exists() {
        return false;
    }
    if e.running_since
        .is_some_and(|since| now().saturating_sub(since) < REAP_GRACE)
    {
        return true;
    }
    output(docker(alive_ps_args(&project_name(&e.slug))))
        .map_or(true, |ids| !ids.trim().is_empty())
}

fn start_stack(
    stack: &Stack,
    entry: &Entry,
    example: &str,
    with: &[Site],
    phases: &Phases,
) -> anyhow::Result<()> {
    phases.run("control network", || {
        let slot = entry
            .control_slot
            .context("the registry entry has no control slot")?;
        check_control_network(stack, slot)
    })?;
    // Whether the apps were already running: `compose up` leaves a running container as it is,
    // so its binary and env files are only picked up by a recreate (T5 review I2).
    let was_running = stack.has_running_containers()?;
    let image = phases.run("workbench image", || ensure_image(stack))?;
    touch_slot(&stack.stacks_dir, &stack.slug, now())?;
    let mut extra = phases.run("Neon images", || neon_images(stack))?;
    let nextest = host_nextest();
    if nextest.is_none() {
        eprintln!(
            "[workbench] no cargo-nextest on PATH: `workbench test` needs it (cargo install \
             cargo-nextest); `workbench run` works without it"
        );
    }
    extra.push((
        "CHELABASE_NEXTEST_BIN".into(),
        nextest_bin_for_env(nextest.as_deref()),
    ));

    phases.run("stack.env", || {
        for dir in ["env", "neon", "logs", "test", "runner"] {
            let d = stack.state.join(dir);
            std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
        }
        prepare_zitadel_dir(&stack.state.join("zitadel"))?;
        let input = StackEnvInput {
            slug: &stack.slug,
            project: &stack.project,
            checkout: &stack.checkout,
            main: &stack.main,
            state: &stack.state,
            gateway_port: entry.gateway_port,
            proxy_port: entry.proxy_port,
            uid: id("-u")?,
            gid: id("-g")?,
            docker_gid: docker_gid()?,
            image: &image,
            extra: &extra,
            control_slot: entry
                .control_slot
                .context("the registry entry has no control slot")?,
        };
        write_file(&stack.env_file, &render_stack_env(example, &input))?;
        let h = hosts(&stack.slug, entry.gateway_port);
        write_file(
            &stack.runner_dir().join(".curlrc"),
            &curlrc(&h, entry.gateway_port),
        )
    })?;

    phases.run("cargo build", || cargo_build(stack, &RUST_APPS, true))?;
    touch_slot(&stack.stacks_dir, &stack.slug, now())?;

    let auth_changed = phases.run("machine keys", || {
        let auth_changed = ensure_stack_secrets(&stack.main, &stack.state)?;
        // A demo.env from before still names the public bearers, which the apps would now
        // refuse next to their <NAME>_FILE: drop them before the front tier is created.
        let demo = stack.state.join("zitadel/demo.env");
        if let Ok(text) = std::fs::read_to_string(&demo)
            && let Some(scrubbed) = crate::dev::env_files::without_legacy_bearers(&text)
        {
            std::fs::write(&demo, scrubbed)
                .with_context(|| format!("writing {}", demo.display()))?;
        }
        Ok(auth_changed)
    })?;

    // The env files the apps read at create time; recreated below when the bootstrap changed
    // them (T4 review M3).
    let before_bootstrap = app_env_files(stack);

    let mut front: Vec<&str> = INFRA.to_vec();
    front.extend_from_slice(FRONT);
    phases.run("infra, Zitadel, Mailpit and the gateway", || {
        compose_up(stack, &[], &front)
    })?;
    // R9: a running NATS reads its auth.conf, and a running Valkey its users.acl, through a
    // single-file bind mount, which keeps the old inode after the rename above: only a restart
    // loads new users or permissions. (Valkey's `ACL LOAD` would re-read the same stale inode, so
    // it can't replace the restart.) Before the apps, so they connect with the rights they now
    // need. The restart briefly disconnects the running apps; they reconnect, and unacked
    // work-queue jobs are redelivered. Valkey is a cache (`--save ""`): its keys are short-lived
    // and a restart only empties them.
    if nats_needs_restart(was_running, auth_changed.nats) {
        phases.run("restarting NATS (its auth.conf changed)", || {
            stack.compose_run(&["restart", "nats"])?;
            compose_up(stack, &[], &["nats"])
        })?;
    }
    if valkey_needs_restart(was_running, auth_changed.valkey) {
        phases.run("restarting Valkey (its users.acl changed)", || {
            stack.compose_run(&["restart", "valkey"])?;
            compose_up(stack, &[], &["valkey"])
        })?;
    }

    phases.run("Zitadel's PATs", || {
        let zitadel = stack.state.join("zitadel");
        let pats = [zitadel.join("admin.pat"), zitadel.join("login-client.pat")];
        wait_for_files(&pats, Duration::from_secs(180))?;
        secure_pats(&pats)
    })?;

    let mut late: Vec<&str> = LATE_APPS.to_vec();
    let profiles: Vec<&str> = if with.is_empty() {
        Vec::new()
    } else {
        late.extend(with.iter().map(|s| s.service()));
        vec!["--profile", "sites"]
    };
    // Not waited on: on a fresh stack an app that needs the bootstrap's files (auth) restarts
    // until it is recreated below with them.
    phases.run("apps", || compose_start(stack, &profiles, &late))?;
    // After the apps: the worker that would recreate a removed compute is now the one that
    // starts computes on the compute network.
    phases.run("computes off the compute network", || {
        remove_computes_off_network(&stack.slug, &super::env::compute_network(&stack.project))
    })?;

    // What the bootstrap waits on must not run an old binary: an old orchestrator never
    // applies a new migration (B3). Only that: the rest may need the bootstrap's files (B4).
    let (recorded, current) = (read_binaries(stack), current_binaries(stack, &RUST_APPS));
    let rebuilt = recreate_before_bootstrap(was_running, &recorded, &current);
    if !rebuilt.is_empty() {
        phases.run(&format!("recreating {} (new binaries)", rebuilt.join(", ")), || {
            recreate(stack, &rebuilt)
        })?;
    }

    // One run, after the apps: it waits for the orchestrator (the metadata migrations, the
    // Actions target's host) itself.
    phases.run("bootstrap", || bootstrap(stack, None))?;

    // Then every other app on a new binary, and the readers of the files the bootstrap changed
    // (every app container predates them).
    let stale = recreate_after_bootstrap(
        was_running,
        &recorded,
        &current,
        &rebuilt,
        &changed_env_users(&before_bootstrap, &app_env_files(stack)),
    );
    if !stale.is_empty() {
        phases.run(&format!("recreating {}", stale.join(", ")), || {
            recreate(stack, &stale)
        })?;
    }

    phases.run("apps healthy", || compose_up(stack, &profiles, &late))?;

    phases.run("proxy CA", || {
        let to = stack.state.join("neon/proxy-ca.crt");
        stack.compose_run(&["cp", "proxy:/tls/ca.crt", &to.display().to_string()])
    })?;

    record_binaries(stack, &RUST_APPS)?;
    Ok(())
}

/// Whether `up` restarts NATS (R9): only on a stack that was running (its NATS predates the
/// render) and only when the render changed `nats/auth.conf`. A stack that wasn't running starts
/// NATS from the new file anyway.
pub fn nats_needs_restart(was_running: bool, auth_changed: bool) -> bool {
    was_running && auth_changed
}

/// Same rule for Valkey's `valkey/users.acl`.
pub fn valkey_needs_restart(was_running: bool, acl_changed: bool) -> bool {
    was_running && acl_changed
}

fn write_file(path: &Path, contents: &str) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
}

/// An env file the apps read at create time: its path, its contents (if any) and its readers.
type EnvFile = (PathBuf, Option<Vec<u8>>, &'static [&'static str]);

/// The env files bootstrap writes and the services that read them at create time.
fn app_env_files(stack: &Stack) -> Vec<EnvFile> {
    const DEMO_USERS: &[&str] = DEMO_ENV_READERS;
    const ACTIONS_USERS: &[&str] = &["orchestrator"];
    const STUDIO_USERS: &[&str] = &["studio"];
    [
        (stack.state.join("zitadel/demo.env"), DEMO_USERS),
        (stack.state.join("zitadel/actions.env"), ACTIONS_USERS),
        (stack.state.join("studio.env"), STUDIO_USERS),
    ]
    .into_iter()
    .map(|(path, users)| {
        let contents = std::fs::read(&path).ok();
        (path, contents, users)
    })
    .collect()
}

/// The services reading an env file whose contents changed, in [`APPS`] order.
fn changed_env_users(before: &[EnvFile], after: &[EnvFile]) -> Vec<String> {
    let mut users: Vec<&str> = Vec::new();
    for ((_, old, list), (_, new, _)) in before.iter().zip(after) {
        if old != new {
            users.extend(list.iter());
        }
    }
    APPS.iter()
        .filter(|a| users.contains(a))
        .map(|a| a.to_string())
        .collect()
}

fn ensure_image(stack: &Stack) -> anyhow::Result<String> {
    let dir = stack.checkout.join("infra/workbench");
    let dockerfile = dir.join("Dockerfile");
    let bytes =
        std::fs::read(&dockerfile).with_context(|| format!("reading {}", dockerfile.display()))?;
    let tag = image_tag(&bytes);
    let present = Command::new("docker")
        .args(["image", "inspect", &tag])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if present {
        eprintln!("[workbench]   {tag} is there");
    } else {
        let mut cmd = Command::new("docker");
        cmd.arg("build")
            .args(["-t", &tag, "-f"])
            .arg(&dockerfile)
            .arg(&dir);
        run(cmd)?;
    }
    Ok(tag)
}

/// `scripts/neon-images.sh`'s choice (a local `just neon-build`, else the pinned images), plus
/// the compute image the worker starts.
fn neon_images(stack: &Stack) -> anyhow::Result<Vec<(String, String)>> {
    let mut cmd = Command::new(stack.checkout.join("scripts/neon-images.sh"));
    cmd.env("COMPOSE_FILE", &stack.compose_file)
        .current_dir(&stack.checkout);
    let mut images = parse_image_lines(&output(cmd)?);
    let compute = images
        .iter()
        .find(|(k, _)| k == "COMPUTE_NODE_IMAGE")
        .map(|(_, v)| v.clone())
        .context("scripts/neon-images.sh printed no COMPUTE_NODE_IMAGE")?;
    images.push(("CHELABASE_COMPUTE_IMAGE".into(), compute));
    Ok(images)
}

/// `cargo build -p <apps…>` in the stack's checkout (and the CLI when `cli`: scripts run it in
/// the runner as `CHELABASE_CLI`).
fn cargo_build(stack: &Stack, apps: &[&str], cli: bool) -> anyhow::Result<()> {
    let mut cmd = Command::new("cargo");
    cmd.arg("build").current_dir(&stack.checkout);
    for app in apps {
        cmd.args(["-p", app]);
    }
    if cli {
        // `chelabase-init` too: the bootstrap runs it.
        cmd.args(["-p", "chelabase", "-p", "init"]);
    }
    run(cmd)
}

/// How long any `compose up --wait` in `up` waits for its services to be healthy.
pub const WAIT_SECS: u64 = 600;

/// `--wait --wait-timeout <WAIT_SECS>`: no wait in `up` is unbounded.
pub fn wait_args() -> Vec<String> {
    vec!["--wait".into(), "--wait-timeout".into(), WAIT_SECS.to_string()]
}

/// The error of a wait that failed or timed out, naming the services that never became healthy.
pub fn never_healthy(services: &[String]) -> String {
    if services.is_empty() {
        format!("the services didn't become healthy within {WAIT_SECS}s")
    } else {
        format!(
            "{} never became healthy (waited up to {WAIT_SECS}s)",
            services.join(", ")
        )
    }
}

/// Runs a waiting `compose` call; on failure shows `ps` and the failing services' last 50 log
/// lines, and names them in the error.
fn run_waiting(stack: &Stack, args: &[String]) -> anyhow::Result<()> {
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    stack.compose_run(&refs).map_err(|e| {
        show_failure(stack);
        let failed = stack
            .compose_output(&["--profile", "sites", "ps", "-a", "--format", "json"])
            .map(|json| failed_services(&json))
            .unwrap_or_default();
        e.context(never_healthy(&failed))
    })
}

/// `[profiles] up -d --wait --wait-timeout … <services>`.
pub fn compose_up_args(profiles: &[&str], services: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = profiles.iter().map(|s| s.to_string()).collect();
    args.extend(["up".to_string(), "-d".to_string()]);
    args.extend(wait_args());
    args.extend(services.iter().map(|s| s.to_string()));
    args
}

/// `compose [profiles] up -d --wait <services>`, bounded by [`WAIT_SECS`].
fn compose_up(stack: &Stack, profiles: &[&str], services: &[&str]) -> anyhow::Result<()> {
    run_waiting(stack, &compose_up_args(profiles, services))
}

/// `compose [profiles] up -d <services>`, not waiting for them to be healthy.
fn compose_start(stack: &Stack, profiles: &[&str], services: &[&str]) -> anyhow::Result<()> {
    let mut args: Vec<&str> = profiles.to_vec();
    args.extend(["up", "-d"]);
    args.extend_from_slice(services);
    stack
        .compose_run(&args)
        .inspect_err(|_| show_failure(stack))
}

/// `compose up -d --force-recreate --no-deps --wait <apps>` (bounded by [`WAIT_SECS`]): env files are read when a container
/// is created, so a restart would keep the old ones (T4 review M3). The Zitadel PATs are Compose
/// secrets of the apps that read them: a missing one (Zitadel rewriting them after `just
/// zitadel-reset`) would fail the create, so it first waits for them, as `up` does.
fn recreate(stack: &Stack, apps: &[String]) -> anyhow::Result<()> {
    let pats = pat_files(&stack.state, apps);
    if !pats.is_empty() {
        wait_for_files(&pats, Duration::from_secs(180))?;
        secure_pats(&pats)?;
    }
    run_waiting(stack, &recreate_args(apps))
}

/// `--profile sites up -d --force-recreate --no-deps --wait --wait-timeout … <apps>`.
pub fn recreate_args(apps: &[String]) -> Vec<String> {
    let mut args: Vec<String> = ["--profile", "sites", "up", "-d", "--force-recreate", "--no-deps"]
        .map(str::to_string)
        .to_vec();
    args.extend(wait_args());
    args.extend(apps.iter().cloned());
    args
}

fn show_failure(stack: &Stack) {
    let _ = stack.compose_run(&["--profile", "sites", "ps", "-a"]);
    let Ok(json) = stack.compose_output(&["--profile", "sites", "ps", "-a", "--format", "json"])
    else {
        return;
    };
    for service in failed_services(&json) {
        eprintln!("[workbench] last 50 log lines of {service}:");
        let _ = stack.compose_run(&["logs", "--no-color", "--tail", "50", &service]);
    }
}

fn wait_for_files(files: &[PathBuf], timeout: Duration) -> anyhow::Result<()> {
    let start = Instant::now();
    loop {
        let missing: Vec<&PathBuf> = files
            .iter()
            .filter(|f| std::fs::metadata(f).map_or(true, |m| m.len() == 0))
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        if start.elapsed() >= timeout {
            bail!(
                "Zitadel hasn't written {} after {}: it writes them only on a fresh volume (`just \
                 zitadel-reset`); its logs: `just workbench logs zitadel`",
                missing
                    .iter()
                    .map(|f| f.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                fmt_duration(timeout)
            );
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// `chelabase-init bootstrap --demo` on the stack's network, as a one-shot container of the
/// workbench image: the host can't reach the metadata database, and the bootstrap reaches
/// Zitadel, the metadata database and the orchestrator by their service names. It mounts only
/// its binary (read-only) and the output dir (the stack's `zitadel/`, where Zitadel writes the
/// PATs); its env is the stack env's values of what it reads (`bootstrap::ENV_KEYS`). Then the
/// apps' env files are rendered from its outputs.
fn bootstrap(stack: &Stack, admin_email: Option<&str>) -> anyhow::Result<()> {
    let env = crate::paths::load_dotenv_file(&stack.env_file);
    let out = stack.state.join("zitadel");
    crate::dev::bootstrap::adopt_actions_key(&out)?;
    let bin = stack.checkout.join("target/debug/chelabase-init");
    let bin = bin
        .canonicalize()
        .with_context(|| format!("{} isn't built (cargo build -p init)", bin.display()))?;
    let mut cmd = Command::new("docker");
    cmd.args(bootstrap_args(&stack.project, &env, &bin, &out, admin_email)?)
        // The values reach the container through docker's own env, never its argv.
        .envs(bootstrap_env(&env, &out));
    run(cmd)?;
    crate::dev::bootstrap::render_workbench_files(&out, &stack.state.join("studio.env"), &env)
}

/// `docker run …` for [`bootstrap`]: see there.
pub fn bootstrap_args(
    project: &str,
    env: &HashMap<String, String>,
    bin: &Path,
    out: &Path,
    admin_email: Option<&str>,
) -> anyhow::Result<Vec<String>> {
    let get = |k: &str| {
        env.get(k)
            .cloned()
            .with_context(|| format!("the stack env has no {k}"))
    };
    let (out, bin) = (out.display().to_string(), bin.display().to_string());
    let tool = "/usr/local/bin/chelabase-init";
    let mut args: Vec<String> = [
        "run",
        "--rm",
        "--network",
        &format!("{project}_default"),
        "--user",
        &format!("{}:{}", get("CHELABASE_UID")?, get("CHELABASE_GID")?),
        "-v",
        &format!("{bin}:{tool}:ro"),
        "-v",
        &format!("{out}:{out}"),
        "--entrypoint",
        tool,
    ]
    .map(str::to_string)
    .to_vec();
    for (k, _) in bootstrap_env(env, Path::new(&out)) {
        // `-e NAME`: docker takes the value from its own environment (see [`bootstrap`]), so
        // no value (the metadata URL carries its password) is on the command line.
        args.extend(["-e".to_string(), k]);
    }
    args.push(get("CHELABASE_WORKBENCH_IMAGE")?);
    args.extend(["bootstrap", "--demo", "--out", &out].map(str::to_string));
    if let Some(email) = admin_email.map(str::trim).filter(|e| !e.is_empty()) {
        args.extend(["--admin-email".to_string(), email.to_string()]);
    }
    Ok(args)
}

/// The env [`bootstrap`]'s container gets: the stack env's values of what the bootstrap reads,
/// and the admin PAT's path in the output dir.
pub fn bootstrap_env(env: &HashMap<String, String>, out: &Path) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = chelabase_init::bootstrap::ENV_KEYS
        .iter()
        .filter(|k| **k != "CHELABASE_ZITADEL_ADMIN_PAT_FILE")
        .filter_map(|k| env.get(*k).map(|v| (k.to_string(), v.clone())))
        .collect();
    pairs.push((
        "CHELABASE_ZITADEL_ADMIN_PAT_FILE".into(),
        format!("{}/admin.pat", out.display()),
    ));
    pairs
}

/// `chelabase dev bootstrap [--admin-email e]`: the current checkout's stack's bootstrap, as
/// `up` runs it (`just zitadel-bootstrap`). The stack must be up.
pub fn dev_bootstrap(admin_email: Option<&str>) -> anyhow::Result<()> {
    let stack = current_stack()?;
    if !stack.env_file.is_file() {
        bail!(
            "stack {} has no stack env ({}): bring it up with `just workbench up`",
            stack.slug,
            stack.env_file.display()
        );
    }
    bootstrap(&stack, admin_email)
}

fn record_binaries(stack: &Stack, apps: &[&str]) -> anyhow::Result<()> {
    let mut recorded = read_binaries(stack);
    recorded.extend(current_binaries(stack, apps));
    let text = serde_json::to_string_pretty(&recorded).context("serializing binaries.json")?;
    write_file(&stack.binaries_file(), &format!("{text}\n"))
}

fn read_binaries(stack: &Stack) -> HashMap<String, u64> {
    std::fs::read_to_string(stack.binaries_file())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn current_binaries(stack: &Stack, apps: &[&str]) -> HashMap<String, u64> {
    apps.iter()
        .filter_map(|app| {
            let path = stack.checkout.join("target/debug").join(app);
            Some((app.to_string(), mtime_nanos(&path)?))
        })
        .collect()
}

fn print_urls(stack: &Stack, entry: &Entry, scheme: &str, with: &[Site]) {
    let h = hosts(&stack.slug, entry.gateway_port);
    let url = |host: &str| format!("{scheme}://{host}");
    println!();
    println!("=== Chelabase workbench stack {} ===", stack.slug);
    println!("Studio:          {}", url(&h.studio));
    println!("API:             {}", url(&h.api));
    println!("Auth (Zitadel):  {}/ui/console", url(&h.auth));
    println!("Mail (Mailpit):  {}", url(&h.mail));
    if with.contains(&Site::Docs) {
        println!("Docs:            {}", url(&h.docs));
    }
    if with.contains(&Site::Landing) {
        println!("Landing:         {}", url(&h.landing));
    }
    println!(
        "Postgres:        <endpoint>.localtest.me:{} (the proxy; localhost:{})",
        entry.proxy_port, entry.proxy_port
    );
    println!("State:           {}", stack.state.display());
}

/// How long `just dev`'s processes get to exit on SIGTERM before SIGKILL.
const LEGACY_GRACE: Duration = Duration::from_secs(10);

/// Stops the processes `just dev` recorded in `.chelabase/dev/pids`, with `dev-down`'s check: a
/// pid is touched only while its command line is one of our cargo, pnpm, debug-binary or Vite
/// processes (a reused pid is left alone). Each was started under `setsid`, so its group gets the
/// SIGTERM too; the leaders are then waited for (SIGKILL after [`LEGACY_GRACE`]), and only then
/// does the caller remove the computes the old worker started (T5 review I3).
fn stop_legacy_dev_processes(main: &Path) -> anyhow::Result<()> {
    let pids = main.join(".chelabase/dev/pids");
    let Ok(text) = std::fs::read_to_string(&pids) else {
        return Ok(());
    };
    let mut ours: Vec<u32> = Vec::new();
    for pid in text.lines().map(str::trim).filter(|p| !p.is_empty()) {
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|b| String::from_utf8_lossy(&b).replace('\0', " "))
            .unwrap_or_default();
        let is_ours = ["cargo", "pnpm", "target/debug/", "vite"]
            .iter()
            .any(|m| cmdline.contains(m));
        if !is_ours {
            continue;
        }
        // The group first (best effort: a pid that leads no group just fails here).
        let _ = Command::new("kill")
            .args(["-TERM", "--", &format!("-{pid}")])
            .stderr(Stdio::null())
            .status();
        eprintln!(
            "[workbench]   stopping just dev's pid {pid} ({})",
            cmdline.trim()
        );
        ours.push(pid);
    }
    stop_processes(&ours, LEGACY_GRACE)?;
    for pid in &ours {
        // Whatever of the groups outlived their leaders.
        let _ = Command::new("kill")
            .args(["-KILL", "--", &format!("-{pid}")])
            .stderr(Stdio::null())
            .status();
    }
    Ok(())
}

/// Removes the computes the W1 layout started (see [`is_legacy_compute`]).
fn remove_legacy_computes() -> anyhow::Result<()> {
    let mut cmd = Command::new("docker");
    cmd.args([
        "ps",
        "-a",
        "--filter",
        &format!("label={ENDPOINT_LABEL}"),
        "--format",
        &format!(
            "{{{{.Names}}}}\t{{{{.Label \"{ENDPOINT_LABEL}\"}}}}\t{{{{.Label \"{STACK_LABEL}\"}}}}"
        ),
    ]);
    let legacy: Vec<String> = output(cmd)?
        .lines()
        .filter_map(|l| {
            let mut fields = l.split('\t');
            let name = fields.next()?.trim();
            let mut labels = HashMap::new();
            for key in [ENDPOINT_LABEL, STACK_LABEL] {
                let value = fields.next().unwrap_or("").trim();
                if !value.is_empty() {
                    labels.insert(key.to_string(), value.to_string());
                }
            }
            is_legacy_compute(name, &labels)
                .then(|| name.trim_start_matches('/').to_string())
        })
        .collect();
    remove_containers(&legacy)
}

/// The computes in `ps` (`docker ps` lines `<name>\t<network,network…>`) that aren't attached to
/// `network`.
pub fn computes_off_network(ps: &str, network: &str) -> Vec<String> {
    ps.lines()
        .filter_map(|line| {
            let (name, networks) = line.split_once('\t').unwrap_or((line, ""));
            let name = name.trim().trim_start_matches('/');
            (!name.is_empty() && !networks.split(',').any(|n| n.trim() == network))
                .then(|| name.to_string())
        })
        .collect()
}

/// Removes the computes of `slug` that aren't on its compute network (`network`). A stack
/// started before RT1 T1 ran its computes on `<project>_default`, without a storage token in
/// their spec: once the storage services check NeonJWT and the worker locates computes on the
/// compute network, such a compute can neither read its pages nor be reached. The worker's
/// reconcile (every 30 s) creates each missing compute again, on the compute network and with
/// a fresh spec; its data is in the pageserver and safekeepers, not in the container.
fn remove_computes_off_network(slug: &str, network: &str) -> anyhow::Result<()> {
    let mut cmd = Command::new("docker");
    cmd.args([
        "ps",
        "-a",
        "--filter",
        &format!("label={ENDPOINT_LABEL}"),
        "--filter",
        &format!("label={STACK_LABEL}={slug}"),
        "--format",
        "{{.Names}}\t{{.Networks}}",
    ]);
    remove_containers(&computes_off_network(&output(cmd)?, network))
}

/// Removes every compute of `slug` (by the stack label), running or not.
fn remove_stack_computes(slug: &str) -> anyhow::Result<()> {
    let mut cmd = Command::new("docker");
    cmd.args([
        "ps",
        "-aq",
        "--filter",
        &format!("label={ENDPOINT_LABEL}"),
        "--filter",
        &format!("label={STACK_LABEL}={slug}"),
    ]);
    let ids: Vec<String> = output(cmd)?
        .split_whitespace()
        .map(str::to_string)
        .collect();
    remove_containers(&ids)
}

fn remove_containers(names: &[String]) -> anyhow::Result<()> {
    if names.is_empty() {
        return Ok(());
    }
    let mut cmd = Command::new("docker");
    cmd.args(["rm", "-f"]).args(names).stdout(Stdio::null());
    run(cmd)?;
    eprintln!("[workbench]   removed {} compute(s)", names.len());
    Ok(())
}

// --- restart, down, purge ---

/// `workbench restart [app…]`: rebuild the named Rust apps (all five when none) and recreate
/// their containers; Studio and the sites are recreated without a build.
pub fn restart(apps: &[String]) -> anyhow::Result<()> {
    let stack = current_stack()?;
    let targets = restart_targets(apps)?;
    let rust: Vec<&str> = targets
        .iter()
        .map(String::as_str)
        .filter(|a| RUST_APPS.contains(a))
        .collect();
    let phases = Phases;
    if !rust.is_empty() {
        phases.run("cargo build", || cargo_build(&stack, &rust, false))?;
    }
    phases.run("recreating", || recreate(&stack, &targets))?;
    record_binaries(&stack, &rust)
}

/// `workbench down [--stack s]`: the stack's containers and computes go (volumes stay) and its
/// slot is released.
pub fn down(slug: Option<&str>) -> anyhow::Result<()> {
    let here = current_stack()?;
    let (stack, entry) = chosen_stack(&here, slug)?;
    if entry.is_none() {
        let known = read_registry(&here.stacks_dir)?;
        bail!(
            "stack {} isn't registered (it was never up); known stacks: {}",
            stack.slug,
            describe_known(&known)
        );
    }
    stop_stack(&stack, false)?;
    release_slot(&stack.stacks_dir, &stack.slug)?;
    eprintln!(
        "[workbench] {} is down (its data stays; purge removes it)",
        stack.slug
    );
    Ok(())
}

/// Stops the apps (so the worker starts no compute), removes the stack's computes, then
/// `compose down` (with `-v` when `volumes`).
fn stop_stack(stack: &Stack, volumes: bool) -> anyhow::Result<()> {
    if stack.env_file.is_file() {
        stack.compose_run(&["--profile", "sites", "stop"])?;
    }
    remove_stack_computes(&stack.slug)?;
    if stack.env_file.is_file() {
        let mut args = vec![
            "--profile",
            "sites",
            "--profile",
            "runner",
            "down",
            "--remove-orphans",
        ];
        if volumes {
            args.push("-v");
        }
        stack.compose_run(&args)?;
    }
    Ok(())
}

fn describe_known(registry: &Registry) -> String {
    let known: Vec<&str> = registry.stacks.iter().map(|e| e.slug.as_str()).collect();
    if known.is_empty() {
        "none".to_string()
    } else {
        known.join(", ")
    }
}

/// `workbench purge [--stack s] [--yes]`: `down`, then the volumes, the computes, the state dir,
/// the registry entry and any queue ticket go. Main needs `--stack main --yes` ([`check_purge`]).
pub fn purge(slug: Option<&str>, yes: bool) -> anyhow::Result<()> {
    let here = current_stack()?;
    let (stack, entry) = chosen_stack(&here, slug)?;
    check_purge(&stack.slug, slug, yes)?;
    if entry.is_none() && !stack.state.exists() {
        let known = read_registry(&here.stacks_dir)?;
        bail!(
            "stack {} isn't registered and has no state; known stacks: {}",
            stack.slug,
            describe_known(&known)
        );
    }
    stop_stack(&stack, true)?;
    if stack.state.exists() {
        std::fs::remove_dir_all(&stack.state)
            .with_context(|| format!("removing {}", stack.state.display()))?;
    }
    with_registry(&stack.stacks_dir, |r| {
        r.remove(&stack.slug);
        Ok(())
    })?;
    drop_tickets(&stack.stacks_dir, &stack.slug)?;
    eprintln!("[workbench] {} is purged", stack.slug);
    Ok(())
}

// --- status, env, logs, psql, exec ---

/// `workbench status`: every registered stack, then the queue.
pub fn status() -> anyhow::Result<()> {
    let here = current_stack()?;
    let registry = read_registry(&here.stacks_dir)?;
    if registry.stacks.is_empty() {
        println!(
            "no workbench stack is registered yet (`just workbench up` starts this checkout's)"
        );
    }
    let t = now();
    for e in &registry.stacks {
        let marker = if e.slug == here.slug {
            " (this checkout)"
        } else {
            ""
        };
        let state = match e.running_since {
            Some(since) => format!("running, up {}", fmt_uptime(t.saturating_sub(since))),
            None => "stopped".to_string(),
        };
        let h = hosts(&e.slug, e.gateway_port);
        println!("{}{marker}: {state}", e.slug);
        println!("  checkout  {}", e.checkout.display());
        println!("  gateway   {}  proxy {}", e.gateway_port, e.proxy_port);
        println!("  studio    {}", h.studio);
        println!("  api       {}", h.api);
    }
    let queue = queued(&here.stacks_dir)?;
    if queue.is_empty() {
        println!("queue: empty");
    } else {
        let slugs: Vec<&str> = queue.iter().map(|(_, s)| s.as_str()).collect();
        println!("queue (oldest first): {}", slugs.join(", "));
    }
    Ok(())
}

/// The stack `env`, `psql`, `logs` and `exec` act on: the recorded one in a task worktree, else
/// this checkout's own. Never starts anything.
fn followed_stack() -> anyhow::Result<Stack> {
    let here = current_stack()?;
    let recorded = read_recorded(&here.checkout)?;
    match resolve_target(None, recorded.as_deref(), &here.slug)? {
        Target::Own => Ok(here),
        Target::Other(slug) => Ok(chosen_stack(&here, Some(&slug))?.0),
    }
}

/// `workbench env`: the path of the stack's `stack.env` (it exists once `up` has run).
pub fn env() -> anyhow::Result<()> {
    println!("{}", followed_stack()?.env_file.display());
    Ok(())
}

/// `workbench logs [-f] [service…]`.
pub fn logs(follow: bool, services: &[String]) -> anyhow::Result<()> {
    let stack = followed_stack()?;
    let mut args = vec!["--profile", "sites", "logs"];
    if follow {
        args.push("-f");
    }
    args.extend(services.iter().map(String::as_str));
    pass_through(stack.compose_cmd(&args)?)
}

/// `workbench psql [db]`: psql in metadata-db (default `chelabase_metadata`).
pub fn psql(db: Option<&str>) -> anyhow::Result<()> {
    let stack = followed_stack()?;
    let db = db.unwrap_or("chelabase_metadata");
    pass_through(stack.compose_cmd(&[
        "exec",
        "metadata-db",
        "psql",
        "-U",
        "chelabase",
        "-d",
        db,
    ])?)
}

/// `workbench exec <service> <cmd…>`.
pub fn exec(service: &str, cmd: &[String]) -> anyhow::Result<()> {
    let stack = followed_stack()?;
    let mut args = vec!["--profile", "sites", "exec", service];
    args.extend(cmd.iter().map(String::as_str));
    pass_through(stack.compose_cmd(&args)?)
}

// --- test, run ---

/// `workbench test [--stack <slug>] [selection…] [-- run-args…]`: archive on the host (the runner
/// has no cargo), run the archive in the runner on the stack's network. On another checkout's
/// stack (`--stack`, or the one `wt-add` recorded) the stack is never started, rebuilt or
/// recreated; this checkout's files are what runs.
pub fn test(stack: Option<&str>, selection: &[String], run_args: &[String]) -> anyhow::Result<()> {
    let here = current_stack()?;
    let (target, entry) = stack_for_run(&here, stack)?;
    let archive = archive_path(&target.state, &target.slug, &here.slug);
    let phases = Phases;
    phases.run("nextest archive", || {
        let mut cmd = Command::new("cargo");
        cmd.args(nextest_archive_args(selection, &archive))
            .current_dir(&here.checkout);
        run(cmd)
    })?;
    let mut cmd = vec!["cargo-nextest".to_string()];
    cmd.extend(nextest_run_args(run_args, &archive, &here.checkout));
    in_runner(&target, &here.checkout, &entry, &[], &cmd)
}

/// `workbench run [--stack <slug>] [--env K=V]… <script> [args…]`: a repo script in the runner.
pub fn run_script(
    stack: Option<&str>,
    env: &[String],
    script: &str,
    args: &[String],
) -> anyhow::Result<()> {
    let pairs = parse_env_pairs(env)?;
    let here = current_stack()?;
    let (target, entry) = stack_for_run(&here, stack)?;
    let mut cmd = vec![script.to_string()];
    cmd.extend(args.iter().cloned());
    in_runner(&target, &here.checkout, &entry, &pairs, &cmd)
}

/// The stack a `test`/`run` uses, with its registry entry: this checkout's own (started or brought
/// current), or another checkout's, checked ready and left untouched.
fn stack_for_run(here: &Stack, flag: Option<&str>) -> anyhow::Result<(Stack, Entry)> {
    match resolve_stack_target(here, flag)? {
        None => Ok((here.clone(), ensure_current(here)?)),
        Some(other) => Ok(other),
    }
}

/// `None` for this checkout's own stack; else another stack, checked ready (registered, holding
/// its slot, every app running), with its registry entry.
fn resolve_stack_target(
    here: &Stack,
    flag: Option<&str>,
) -> anyhow::Result<Option<(Stack, Entry)>> {
    let recorded = read_recorded(&here.checkout)?;
    match resolve_target(flag, recorded.as_deref(), &here.slug)? {
        Target::Own => Ok(None),
        Target::Other(slug) => {
            let (stack, entry) = chosen_stack(here, Some(&slug))?;
            let running = if entry.is_some() {
                stack.running_services()?
            } else {
                Vec::new()
            };
            check_ready(&slug, entry.as_ref(), &APPS, &running)?;
            let entry = entry.with_context(|| format!("stack {slug} isn't registered"))?;
            Ok(Some((stack, entry)))
        }
    }
}

/// `workbench slug-of <branch>`: the registered stack slug of a base branch, or an empty line.
pub fn slug_of(branch: &str) -> anyhow::Result<()> {
    let here = current_stack()?;
    let registry = read_registry(&here.stacks_dir)?;
    println!("{}", base_stack_slug(branch, &registry)?);
    Ok(())
}

/// W-6: up (idempotent) when the stack isn't running or any app service is missing (a half-up
/// stack, T5 review M3); else rebuild the apps and recreate the ones whose binary changed since
/// their start.
fn ensure_current(stack: &Stack) -> anyhow::Result<Entry> {
    let registry = read_registry(&stack.stacks_dir)?;
    let slot = registry
        .get(&stack.slug)
        .is_some_and(|e| e.running_since.is_some());
    let running = slot && {
        let missing = missing_services(&APPS, &stack.running_services()?);
        if !missing.is_empty() {
            eprintln!(
                "[workbench] {} is only partly up (not running: {}): running up",
                stack.slug,
                missing.join(", ")
            );
        }
        missing.is_empty()
    };
    if !running {
        up_stack(stack, &[])?;
    } else {
        let phases = Phases;
        phases.run("cargo build", || cargo_build(stack, &RUST_APPS, true))?;
        let stale = stale_apps(&read_binaries(stack), &current_binaries(stack, &RUST_APPS));
        if !stale.is_empty() {
            phases.run(&format!("recreating {}", stale.join(", ")), || {
                recreate(stack, &stale)
            })?;
            record_binaries(stack, &RUST_APPS)?;
        }
    }
    read_registry(&stack.stacks_dir)?
        .get(&stack.slug)
        .cloned()
        .with_context(|| format!("stack {} isn't registered", stack.slug))
}

/// The runner's `docker compose … --profile runner run --rm --no-deps [-e K=V]… runner <cmd…>`
/// for `stack`, run on `checkout`'s files: when `checkout` isn't `stack.checkout`, the process env
/// gets `CHELABASE_CHECKOUT=<checkout>` (set after `compose_command`'s env-file values, so it wins
/// the interpolation) and `-e CHELABASE_CHECKOUT=<checkout>` and `-e CHELABASE_CLI=<checkout>/target/debug/chelabase`
/// (the stack env's value points into the stack's checkout, which the runner doesn't mount) come
/// before `env`'s pairs. Always
/// `-e CHELABASE_STACK_CHECKOUT=<stack.checkout>`: a suite that recreates the stack's apps through
/// Compose interpolates that, never the task checkout.
pub(crate) fn runner_command(
    stack: &Stack,
    checkout: &Path,
    env: &[(String, String)],
    cmd: &[String],
) -> anyhow::Result<Command> {
    let mut pairs: Vec<String> = vec![format!(
        "CHELABASE_STACK_CHECKOUT={}",
        stack.checkout.display()
    )];
    let other = checkout != stack.checkout;
    if other {
        pairs.push(format!("CHELABASE_CHECKOUT={}", checkout.display()));
        pairs.push(format!(
            "CHELABASE_CLI={}",
            checkout.join("target").join("debug").join("chelabase").display()
        ));
    }
    pairs.extend(env.iter().map(|(k, v)| format!("{k}={v}")));
    let mut args: Vec<&str> = vec!["--profile", "runner", "run", "--rm", "--no-deps"];
    for p in &pairs {
        args.extend(["-e", p.as_str()]);
    }
    args.push("runner");
    args.extend(cmd.iter().map(String::as_str));
    let mut command = stack.compose_cmd(&args)?;
    if other {
        command.env("CHELABASE_CHECKOUT", checkout);
    }
    Ok(command)
}

/// `compose run --rm --no-deps` of the runner with the pinned env, then `extra`; `checkout` is the
/// checkout whose files run (the current one).
fn in_runner(
    stack: &Stack,
    checkout: &Path,
    entry: &Entry,
    extra: &[(String, String)],
    cmd: &[String],
) -> anyhow::Result<()> {
    let scheme = crate::paths::load_dotenv_file(&stack.env_file)
        .get("CHELABASE_PUBLIC_SCHEME")
        .cloned()
        .unwrap_or_else(|| "https".to_string());
    let h = hosts(&stack.slug, entry.gateway_port);
    let mut env: Vec<(String, String)> = vec![
        ("GATEWAY_URL".into(), format!("{scheme}://{}", h.api)),
        ("CHELABASE_PROXY_HOSTADDR".into(), proxy_ip(stack)?),
        (
            "CHELABASE_PROXY_PG_PORT".into(),
            PROXY_PG_PORT_INSIDE.into(),
        ),
        (
            "CHELABASE_PROXY_HTTP_PORT".into(),
            PROXY_HTTP_PORT_INSIDE.into(),
        ),
        (
            "STORCON_URL".into(),
            "http://storage-controller:1234".into(),
        ),
        ("PAGESERVER_URL".into(), "http://pageserver:9898".into()),
        ("CURL_HOME".into(), stack.runner_dir().display().to_string()),
    ];
    env.extend(extra.iter().cloned());
    let mut ps = Command::new("docker");
    ps.args(runner_ps_args(&stack.project));
    busy_runner(&stack.slug, &output(ps)?)?;
    pass_through(runner_command(stack, checkout, &env, cmd)?)
}

/// The `docker` arguments that list `project`'s running runner containers, one name per line.
pub(crate) fn runner_ps_args(project: &str) -> Vec<String> {
    [
        "ps",
        "--filter",
        &format!("label=com.docker.compose.project={project}"),
        "--filter",
        "label=com.docker.compose.service=runner",
        "--format",
        "{{.Names}}",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Refuses a run while `ps_output` ([`runner_ps_args`]) lists a running runner: the runner has
/// one fixed address on the control network (the member-login rule lists it), so a second one
/// would fail inside Docker.
pub(crate) fn busy_runner(slug: &str, ps_output: &str) -> anyhow::Result<()> {
    let running: Vec<&str> = ps_output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if running.is_empty() {
        return Ok(());
    }
    bail!(
        "the stack {slug} is busy: its runner ({}) is running another `workbench test`/`run`. \
         A stack has one runner (it has one fixed address on the control network), so only one \
         run at a time; wait for that run to finish, then try again",
        running.join(", ")
    )
}

/// The proxy container's address on the stack network (`<project>_default`).
fn proxy_ip(stack: &Stack) -> anyhow::Result<String> {
    let network = format!("{}_default", stack.project);
    let mut cmd = Command::new("docker");
    cmd.args([
        "inspect",
        "-f",
        &format!("{{{{(index .NetworkSettings.Networks \"{network}\").IPAddress}}}}"),
        &format!("{}-proxy-1", stack.project),
    ]);
    let ip = output(cmd)?.trim().to_string();
    if ip.is_empty() || ip == "<no value>" {
        bail!("the proxy of {} has no address on {network}", stack.slug);
    }
    Ok(ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_checkout_maps_to_its_slug() {
        let main = Path::new("/r");
        assert_eq!(slug_for_checkout(main, main).unwrap(), "main");
        assert_eq!(
            slug_for_checkout(Path::new("/r/.worktrees/Feat_X"), main).unwrap(),
            "feat-x"
        );
        let err = slug_for_checkout(Path::new("/elsewhere/x"), main)
            .unwrap_err()
            .to_string();
        assert!(err.contains("/elsewhere/x"), "{err}");
        assert!(slug_for_checkout(Path::new("/r/.worktrees/a/b"), main).is_err());
    }

    #[test]
    fn the_image_tag_is_the_dockerfiles_hash() {
        let tag = image_tag(b"FROM x\n");
        let hash = hex::encode(Sha256::digest(b"FROM x\n"));
        assert_eq!(tag, format!("chelabase-workbench:{}", &hash[..12]));
    }

    #[test]
    fn image_lines_and_env_pairs_parse() {
        assert_eq!(
            parse_image_lines("NEON_IMAGE=a:1\n\nCOMPUTE_NODE_IMAGE=b@sha256:x\n"),
            [
                ("NEON_IMAGE".to_string(), "a:1".to_string()),
                ("COMPUTE_NODE_IMAGE".to_string(), "b@sha256:x".to_string()),
            ]
        );
        assert_eq!(
            parse_env_pairs(&["A=1".into(), "B=x=y".into()]).unwrap(),
            [
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "x=y".to_string())
            ]
        );
        assert!(parse_env_pairs(&["nope".into()]).is_err());
        assert!(parse_env_pairs(&["=1".into()]).is_err());
    }

    #[test]
    fn restart_defaults_to_the_rust_apps_and_checks_names() {
        assert_eq!(restart_targets(&[]).unwrap(), RUST_APPS);
        assert_eq!(restart_targets(&["studio".into()]).unwrap(), ["studio"]);
        assert!(restart_targets(&["blog".into()]).is_err());
    }

    #[test]
    fn recreate_waits_for_the_pats_of_the_apps_that_mount_them() {
        let state = Path::new("/s");
        let names = |apps: &[&str]| -> Vec<PathBuf> {
            let apps: Vec<String> = apps.iter().map(|a| a.to_string()).collect();
            pat_files(state, &apps)
        };
        assert!(names(&["gateway", "rest", "realtime", "studio", "docs"]).is_empty());
        assert_eq!(names(&["worker"]), [state.join("zitadel/admin.pat")]);
        // Each PAT once, whatever the order or the number of readers.
        assert_eq!(
            names(&["orchestrator", "auth", "worker"]),
            [
                state.join("zitadel/admin.pat"),
                state.join("zitadel/login-client.pat")
            ]
        );
        assert_eq!(names(&[]), Vec::<PathBuf>::new());

        // The table matches the compose files (the base and the workbench overlay): every service
        // listing a PAT secret, and only those.
        let compose = concat!(
            include_str!("../../../../infra/compose/docker-compose.yml"),
            "\n",
            include_str!("../../../../infra/compose/docker-compose.workbench.yml")
        );
        let mut mounted: Vec<(String, &str)> = Vec::new();
        let mut service = None;
        let mut in_services = false;
        for line in compose.lines() {
            if !line.starts_with(' ') && !line.is_empty() && !line.starts_with('#') {
                in_services = line == "services:";
                service = None;
            } else if in_services
                && let Some(name) = line.strip_prefix("  ").and_then(|l| l.strip_suffix(':'))
                && !name.starts_with([' ', '#'])
            {
                service = Some(name.to_string());
            }
            for (secret, file) in [
                ("zitadel-admin-pat", "admin.pat"),
                ("zitadel-login-client-pat", "login-client.pat"),
            ] {
                if let Some(svc) = &service
                    && line.contains(&format!("{{source: {secret}, "))
                {
                    mounted.push((svc.clone(), file));
                }
            }
        }
        mounted.sort();
        // A service that mounts a PAT must be one `recreate()` can be asked to recreate, or its
        // PAT wait is skipped and its create fails on the missing file after a zitadel reset.
        let recreatable: Vec<&str> = APPS
            .iter()
            .copied()
            .chain(["docs", "landing", "runner"])
            .collect();
        for (svc, _) in &mounted {
            assert!(
                recreatable.contains(&svc.as_str()),
                "service `{svc}` mounts a Zitadel PAT but recreate() can't be asked for it"
            );
        }
        let mut table: Vec<(String, &str)> = Vec::new();
        for app in APPS.iter().chain(&["docs", "landing", "runner"]) {
            for f in pat_files(Path::new(""), &[app.to_string()]) {
                let file = if f.ends_with("admin.pat") {
                    "admin.pat"
                } else {
                    "login-client.pat"
                };
                table.push((app.to_string(), file));
            }
        }
        table.sort();
        assert_eq!(mounted, table);
        assert!(!table.is_empty());
    }

    fn mode_of(p: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn the_zitadel_dir_is_private_to_the_stack_user() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        // Created fresh (parents included).
        let fresh = tmp.path().join("state/zitadel");
        prepare_zitadel_dir(&fresh).unwrap();
        assert_eq!(mode_of(&fresh), 0o700, "a new dir");
        // A dir left at 0777 by an older stack is tightened, and the call is idempotent.
        let old = tmp.path().join("old");
        std::fs::create_dir(&old).unwrap();
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o777)).unwrap();
        std::fs::write(old.join("admin.pat"), "x").unwrap();
        prepare_zitadel_dir(&old).unwrap();
        prepare_zitadel_dir(&old).unwrap();
        assert_eq!(mode_of(&old), 0o700, "an existing 0777 dir");
        assert!(old.join("admin.pat").exists(), "existing files are kept");
    }

    #[test]
    fn secure_pats_makes_each_pat_0600() {
        use std::os::unix::fs::PermissionsExt as _;
        let tmp = tempfile::tempdir().unwrap();
        let admin = tmp.path().join("admin.pat");
        let login = tmp.path().join("login-client.pat");
        for f in [&admin, &login] {
            std::fs::write(f, "token").unwrap();
            std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        secure_pats(&[admin.clone(), login.clone()]).unwrap();
        assert_eq!(mode_of(&admin), 0o600);
        assert_eq!(mode_of(&login), 0o600);
        assert_eq!(std::fs::read_to_string(&admin).unwrap(), "token");
        // Idempotent, and an empty list is fine (recreate() may wait for no PAT).
        secure_pats(&[admin.clone(), login.clone()]).unwrap();
        secure_pats(&[]).unwrap();
        assert_eq!(mode_of(&admin), 0o600);
        // A PAT that isn't there is an error, not a silent skip.
        assert!(secure_pats(&[tmp.path().join("nope.pat")]).is_err());
    }

    #[test]
    fn failing_services_are_found_in_ps_json() {
        let ndjson = r#"{"Service":"gateway","State":"running","Health":"unhealthy","ExitCode":0}
{"Service":"proxy-tls","State":"exited","Health":"","ExitCode":0}
{"Service":"create-bucket","State":"exited","Health":"","ExitCode":1}
{"Service":"worker","State":"running","Health":"healthy","ExitCode":0}"#;
        assert_eq!(failed_services(ndjson), ["create-bucket", "gateway"]);
        let array = r#"[{"Service":"auth","State":"restarting","ExitCode":0}]"#;
        assert_eq!(failed_services(array), ["auth"]);
    }

    #[test]
    fn changed_env_files_name_their_readers() {
        const DEMO: &[&str] = &["gateway", "auth"];
        const STUDIO: &[&str] = &["studio"];
        let p = PathBuf::from;
        let before = vec![
            (p("demo.env"), Some(b"a".to_vec()), DEMO),
            (p("studio.env"), None, STUDIO),
        ];
        let after = vec![
            (p("demo.env"), Some(b"a".to_vec()), DEMO),
            (p("studio.env"), Some(b"s".to_vec()), STUDIO),
        ];
        assert_eq!(changed_env_users(&before, &after), ["studio"]);
        assert!(changed_env_users(&before, &before).is_empty());
    }

    // The workbench's one bootstrap: `chelabase-init bootstrap --demo` on the stack network,
    // with its binary read-only and the output dir the only writable mount (no docker socket,
    // no keys dir), and the env it reads taken from the stack env.
    // B3 (live, `up` on an existing stack): the app the bootstrap waits on is recreated on its
    // new binary first; an orchestrator left on its old binary never applies a new migration.
    // B4: and only that app. A fresh stack whose first `up` failed has no binaries.json, so
    // every app is "stale"; recreating `auth` (it needs demo.env's org id) and waiting on it
    // before the bootstrap writes demo.env never ends.
    #[test]
    fn the_pre_bootstrap_recreate_waits_on_no_bootstrap_reader() {
        let current: HashMap<String, u64> = RUST_APPS.iter().map(|a| (a.to_string(), 2)).collect();
        let rebuilt = recreate_before_bootstrap(true, &HashMap::new(), &current);
        assert_eq!(rebuilt, ["orchestrator"], "recreated and waited on before demo.env: {rebuilt:?}");
        let map = |pairs: &[(&str, u64)]| -> HashMap<String, u64> {
            pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
        };
        let recorded = map(&[("orchestrator", 1), ("worker", 1), ("auth", 1)]);
        let changed = map(&[("orchestrator", 2), ("worker", 1), ("auth", 1), ("rest", 1)]);
        assert_eq!(recreate_before_bootstrap(true, &recorded, &changed), ["orchestrator"]);
        assert!(recreate_before_bootstrap(true, &map(&[("rest", 1)]), &map(&[("rest", 2)])).is_empty());
        // A stack that wasn't running starts every app on its current binary.
        assert!(recreate_before_bootstrap(false, &recorded, &changed).is_empty());
    }

    // B4: after the bootstrap, every other app on a new binary, and every reader of an env file
    // the bootstrap changed (auth once demo.env appears), the orchestrator again only when its
    // env changed.
    #[test]
    fn the_bootstrap_readers_are_recreated_after_it() {
        let current: HashMap<String, u64> = RUST_APPS.iter().map(|a| (a.to_string(), 2)).collect();
        let rebuilt = vec!["orchestrator".to_string()];
        let demo_readers: Vec<String> = DEMO_ENV_READERS.iter().map(|a| a.to_string()).collect();
        let after = recreate_after_bootstrap(true, &HashMap::new(), &current, &rebuilt, &demo_readers);
        for app in RUST_APPS {
            assert!(after.contains(&app.to_string()), "{app} not recreated: {after:?}");
        }
        let after = recreate_after_bootstrap(true, &HashMap::new(), &current, &rebuilt, &[]);
        assert!(!after.contains(&"orchestrator".to_string()), "recreated twice: {after:?}");
        assert!(after.contains(&"auth".to_string()));
        // A fresh stack: only what the bootstrap's files changed.
        assert_eq!(
            recreate_after_bootstrap(false, &HashMap::new(), &current, &[], &["studio".to_string()]),
            ["studio"]
        );
    }

    // B4: every wait in `up` has a deadline.
    #[test]
    fn every_up_wait_is_bounded() {
        let wait = wait_args();
        assert_eq!(wait[0], "--wait");
        assert_eq!(wait[1], "--wait-timeout");
        assert!(wait[2].parse::<u64>().unwrap() > 0);
        for args in [
            compose_up_args(&[], &["orchestrator"]),
            recreate_args(&["auth".to_string()]),
        ] {
            let i = args.iter().position(|a| a == "--wait").expect("waits");
            assert_eq!(args[i + 1], "--wait-timeout", "{args:?}");
        }
        let msg = never_healthy(&["auth".to_string(), "rest".to_string()]);
        assert!(msg.contains("auth, rest") && msg.contains("never became healthy"), "{msg}");
    }

    // N1: no value reaches `docker run`'s argv (host `ps`, `docker inspect` of the command):
    // the metadata URL carries the database password.
    #[test]
    fn the_bootstrap_argv_holds_no_secret() {
        let env: HashMap<String, String> = [
            ("CHELABASE_UID", "1000"),
            ("CHELABASE_GID", "1001"),
            ("CHELABASE_WORKBENCH_IMAGE", "chelabase-workbench:abc"),
            ("CHELABASE_METADATA_DATABASE_URL", "postgres://chelabase:s3cret@metadata-db:5432/m"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let args = bootstrap_args(
            "chelabase-x",
            &env,
            Path::new("/c/target/debug/chelabase-init"),
            Path::new("/s/zitadel"),
            None,
        )
        .unwrap();
        let joined = args.join(" ");
        assert!(!joined.contains("s3cret") && !joined.contains("postgres://"), "{joined}");
        assert!(args.windows(2).any(|w| w[0] == "-e" && w[1] == "CHELABASE_METADATA_DATABASE_URL"), "{joined}");
    }

    #[test]
    fn the_bootstrap_runs_once_in_the_stack_with_only_its_output_dir() {
        let env: HashMap<String, String> = [
            ("CHELABASE_UID", "1000"),
            ("CHELABASE_GID", "1001"),
            ("CHELABASE_WORKBENCH_IMAGE", "chelabase-workbench:abc"),
            ("CHELABASE_AUTH_HOST", "auth.x.chelabase.localhost:8100"),
            ("CHELABASE_METADATA_DATABASE_URL", "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata"),
            ("CHELABASE_ZITADEL_UPSTREAM", "http://zitadel:8080"),
            ("CHELABASE_ZITADEL_ADMIN_PAT_FILE", "/elsewhere/admin.pat"),
            ("CHELABASE_KEYS_DIR", "/m/.chelabase"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let args = bootstrap_args(
            "chelabase-x",
            &env,
            Path::new("/c/target/debug/chelabase-init"),
            Path::new("/s/zitadel"),
            Some(" a@b.co "),
        )
        .unwrap();
        let joined = args.join(" ");
        assert!(joined.starts_with("run --rm --network chelabase-x_default --user 1000:1001 "), "{joined}");
        let volumes: Vec<&str> = args
            .windows(2)
            .filter(|w| w[0] == "-v")
            .map(|w| w[1].as_str())
            .collect();
        assert_eq!(
            volumes,
            ["/c/target/debug/chelabase-init:/usr/local/bin/chelabase-init:ro", "/s/zitadel:/s/zitadel"]
        );
        let names: Vec<&str> = args.windows(2).filter(|w| w[0] == "-e").map(|w| w[1].as_str()).collect();
        let envs: Vec<String> = bootstrap_env(&env, Path::new("/s/zitadel"))
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        assert_eq!(names.len(), envs.len(), "every value is named on the argv: {names:?}");
        assert!(envs.contains(&"CHELABASE_ZITADEL_ADMIN_PAT_FILE=/s/zitadel/admin.pat".to_string()), "{envs:?}");
        assert!(envs.contains(&"CHELABASE_ZITADEL_UPSTREAM=http://zitadel:8080".to_string()), "{envs:?}");
        assert!(!envs.iter().any(|e| e.starts_with("CHELABASE_KEYS_DIR")), "{envs:?}");
        assert!(!joined.contains("docker.sock"));
        assert!(
            joined.ends_with("chelabase-workbench:abc bootstrap --demo --out /s/zitadel --admin-email a@b.co"),
            "{joined}"
        );
    }
}

#[cfg(test)]
mod key_phase_tests {
    use super::ensure_stack_secrets;
    use std::collections::BTreeMap;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    /// Every regular file below `root` (but `skip` and the lock file): bytes, mtime and mode.
    fn snapshot(root: &Path, skip: &Path) -> BTreeMap<PathBuf, (Vec<u8>, i64, i64, u32)> {
        let mut out = BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            for entry in std::fs::read_dir(&d).unwrap() {
                let p = entry.unwrap().path();
                let meta = std::fs::symlink_metadata(&p).unwrap();
                if p == skip || p.ends_with(chelabase_init::secrets::LOCK_FILE) {
                    continue;
                }
                if meta.is_dir() {
                    stack.push(p);
                } else if meta.is_file() {
                    let bytes = std::fs::read(&p).unwrap();
                    out.insert(p, (bytes, meta.mtime(), meta.mtime_nsec(), meta.mode()));
                }
            }
        }
        out
    }

    // m2 of epic object-storage: the `storage` and `admin` S3 dev keys appear on `up` for a stack
    // made without them, with the fixed dev values at 0600, and an existing file is never changed.
    #[test]
    fn the_object_store_dev_keys_are_added_and_never_overwritten() {
        let t = tempfile::tempdir().unwrap();
        let state = t.path().join("state");
        super::ensure_object_store_dev_files(&state).unwrap();
        let read = |p: &str| std::fs::read_to_string(state.join(p)).unwrap();
        assert_eq!(read("storage-s3/access-key"), "chelabase-storage-dev");
        assert_eq!(read("storage-s3/secret-key"), "chelabase-storage-dev-secret");
        assert_eq!(read("s3-admin/access-key"), "chelabase-admin-dev");
        assert_eq!(read("s3-admin/secret-key"), "chelabase-admin-dev-secret");
        for p in ["storage-s3/access-key", "s3-admin/secret-key"] {
            let mode = std::fs::metadata(state.join(p)).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{p}");
        }
        let dir_mode = std::fs::metadata(state.join("s3-admin")).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        std::fs::write(state.join("storage-s3/access-key"), "kept").unwrap();
        super::ensure_object_store_dev_files(&state).unwrap();
        assert_eq!(read("storage-s3/access-key"), "kept", "an existing key is left alone");
        assert_eq!(read("s3-admin/access-key"), "chelabase-admin-dev");
    }

    // AC 7: a stack whose keys the old key phase made (under umask 002, like the real shared keys
    // dir) starts with the same files and values; only the stack's new files are added.
    #[test]
    fn the_key_phase_keeps_what_the_old_one_made() {
        let t = tempfile::tempdir().unwrap();
        let main = t.path().join("main");
        let keys = main.join(".chelabase");
        let state = keys.join("stacks/x");
        crate::dev::bootstrap::ensure_machine_keys(
            &keys.join("gateway"),
            &keys.join("secrets"),
            &keys.join("auth"),
            &keys.join("neon"),
            &keys.join("compute-ca"),
            &keys.join("orchestrator-tls"),
            &keys.join("realtime-tls"),
        )
        .unwrap();
        crate::dev::stack_secrets::ensure(&keys).unwrap();
        super::super::service_auth::write_storage_auth(&keys).unwrap();
        super::super::service_auth::write_auth_files(&keys, &state).unwrap();
        for d in ["auth", "gateway", "neon", "secrets", "orchestrator-tls"] {
            std::fs::set_permissions(keys.join(d), std::fs::Permissions::from_mode(0o775)).unwrap();
        }
        for f in ["compute-ca/ca.crt", "neon/compute-jwt.pub", "gateway/internal-verify.pem"] {
            std::fs::set_permissions(keys.join(f), std::fs::Permissions::from_mode(0o664)).unwrap();
        }
        let (keys_before, state_before) = (snapshot(&keys, &state), snapshot(&state, &state.join("none")));

        let changed = ensure_stack_secrets(&main, &state).unwrap();
        assert!(!changed.nats && !changed.valkey, "{changed:?}");
        assert_eq!(snapshot(&keys, &state), keys_before, "the shared keys dir is untouched");
        let state_after = snapshot(&state, &state.join("none"));
        for (p, v) in &state_before {
            assert_eq!(state_after.get(p), Some(v), "{}", p.display());
        }
        // The stack's own new files, with the fixed dev values its volumes were made with.
        let read = |p: &str| std::fs::read_to_string(state.join(p)).unwrap();
        assert_eq!(read("db/metadata.password"), "chelabase");
        assert_eq!(read("db/zitadel-admin.password"), "postgres");
        assert_eq!(read("db/zitadel-user.password"), "zitadel");
        assert_eq!(read("db/storcon.password"), "storcon");
        assert_eq!(read("s3/access-key"), "chelabase-dev");
        assert_eq!(read("zitadel/masterkey"), "MasterkeyNeedsToHave32Characters");
        assert!(chelabase_init::read_setup_code(&state).is_ok());

        // And a second `up` changes nothing at all.
        let (k, s) = (snapshot(&keys, &state), snapshot(&state, &state.join("none")));
        let changed = ensure_stack_secrets(&main, &state).unwrap();
        assert!(!changed.nats && !changed.valkey);
        assert_eq!((snapshot(&keys, &state), snapshot(&state, &state.join("none"))), (k, s));
    }
}
