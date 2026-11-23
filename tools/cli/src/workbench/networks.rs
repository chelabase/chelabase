//! The stack's internal networks (compute-network-reach m1): the engine version they need, the
//! check of the live networks after `up`, and the move of a stack off the ordinary bridges of
//! earlier releases (`<project>_compute`, `<project>_control`).
//!
//! In the workbench the control subnet is tied to the stack's slot, so the legacy control network
//! must be gone before Compose creates `<project>_control-internal` on the same subnet. At that
//! point the legacy networks still hold the stack's services and a running worker that would
//! recreate computes on them, so `up` moves the stack in this order: stop the worker, stop and
//! remove the project's Compose containers (volumes stay), remove the stack's computes on a
//! legacy network, remove the legacy networks. A member that is neither stops `up` untouched.

use super::env::{compute_network, control_network, legacy_networks};
use chelabase_neon::compute::{ENDPOINT_LABEL, STACK_LABEL};

/// The oldest Docker Engine major version that has `com.docker.network.bridge.gateway_mode_*`
/// `isolated` (27 refuses the option).
pub const MIN_ENGINE_MAJOR: u32 = 28;

const PROJECT_LABEL: &str = "com.docker.compose.project";
const SERVICE_LABEL: &str = "com.docker.compose.service";
const ONEOFF_LABEL: &str = "com.docker.compose.oneoff";

/// Refuses an engine older than [`MIN_ENGINE_MAJOR`]. `found` is `docker version --format
/// '{{.Server.Version}}'`: `<major>.<minor>…`; anything else is an error, never a pass.
pub fn check_engine_version(found: &str) -> anyhow::Result<()> {
    let parsed = found.split_once('.').and_then(|(major, rest)| {
        let numeric = !major.is_empty() && major.bytes().all(|b| b.is_ascii_digit());
        let minor = rest.bytes().next().is_some_and(|b| b.is_ascii_digit());
        (numeric && minor)
            .then(|| major.parse::<u32>().ok())
            .flatten()
    });
    match parsed {
        Some(major) if major >= MIN_ENGINE_MAJOR => Ok(()),
        Some(_) => anyhow::bail!(
            "Docker Engine {found} is too old: the stack's internal networks need Docker Engine {MIN_ENGINE_MAJOR}.0 or later (an isolated bridge gateway)"
        ),
        None => anyhow::bail!(
            "can't read the Docker Engine version {found:?}; the stack's internal networks need Docker Engine {MIN_ENGINE_MAJOR}.0 or later"
        ),
    }
}

/// Fails unless each network in `wanted` is in `json` (`docker network inspect <wanted…>`) as
/// internal with both bridge gateway modes `isolated`. A network left over under the same name is
/// not trusted because the compose file is right.
pub fn check_live_networks(json: &str, wanted: &[String]) -> anyhow::Result<()> {
    let value: serde_json::Value =
        serde_json::from_str(json).map_err(|e| anyhow::anyhow!("docker network inspect: {e}"))?;
    let networks = value
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("docker network inspect: expected a JSON array"))?;
    for name in wanted {
        let net = networks
            .iter()
            .find(|n| n["Name"].as_str() == Some(name.as_str()))
            .ok_or_else(|| anyhow::anyhow!("the Docker network {name} doesn't exist after up"))?;
        if net["Internal"].as_bool() != Some(true) {
            anyhow::bail!(
                "the Docker network {name} isn't internal: its members would have a route off it; remove it (`docker network rm {name}`, after `just workbench down`) and run up again"
            );
        }
        for family in ["ipv4", "ipv6"] {
            let key = format!("com.docker.network.bridge.gateway_mode_{family}");
            let got = net["Options"][key.as_str()].as_str().unwrap_or("");
            if got != "isolated" {
                anyhow::bail!(
                    "the Docker network {name} has {key}={got:?}, not \"isolated\": its bridge would have a host address; remove it (`docker network rm {name}`, after `just workbench down`) and run up again"
                );
            }
        }
    }
    Ok(())
}

/// `docker ps -a --format` for [`plan_legacy_migration`]: name, Compose project and service, the
/// compute's endpoint and stack labels, networks (tab separated).
pub fn legacy_member_ps_format() -> String {
    format!(
        "{{{{.Names}}}}\t{{{{.Label \"{PROJECT_LABEL}\"}}}}\t{{{{.Label \"{SERVICE_LABEL}\"}}}}\t{{{{.Label \"{ENDPOINT_LABEL}\"}}}}\t{{{{.Label \"{STACK_LABEL}\"}}}}\t{{{{.Networks}}}}"
    )
}

/// One step of the move off the legacy networks, in the order [`plan_legacy_migration`] gives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MigrationStep {
    /// The project's worker containers: stopped first, so no compute is recreated meanwhile.
    StopWorker(Vec<String>),
    /// Every container with the project's Compose label (volumes stay).
    RemoveProjectContainers(Vec<String>),
    /// The stack's computes on a legacy network (the worker recreates them after `up`).
    RemoveComputes(Vec<String>),
    /// The legacy networks that exist.
    RemoveNetworks(Vec<String>),
}

struct Row<'a> {
    name: &'a str,
    project: &'a str,
    service: &'a str,
    endpoint: &'a str,
    stack: &'a str,
    networks: Vec<&'a str>,
}

fn rows(ps: &str) -> impl Iterator<Item = Row<'_>> {
    ps.lines().filter_map(|line| {
        let mut cols = line.split('\t');
        let name = cols.next()?.trim().trim_start_matches('/');
        if name.is_empty() {
            return None;
        }
        let mut next = || cols.next().unwrap_or("").trim();
        let (project, service, endpoint, stack) = (next(), next(), next(), next());
        let networks = next()
            .split(',')
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .collect();
        Some(Row {
            name,
            project,
            service,
            endpoint,
            stack,
            networks,
        })
    })
}

/// The steps that move stack `slug` (Compose project `project`) off the legacy networks in
/// `present` (those of its two legacy networks that exist), from `ps` (every container, in
/// [`legacy_member_ps_format`]). Nothing to do when none exists. Fails, naming the container,
/// when a legacy network has a member that is neither one of this stack's computes nor a
/// container of this project: such a container is never stopped or removed.
pub fn plan_legacy_migration(
    slug: &str,
    project: &str,
    ps: &str,
    present: &[String],
) -> anyhow::Result<Vec<MigrationStep>> {
    let legacy = legacy_networks(project);
    if let Some(other) = present.iter().find(|n| !legacy.contains(n)) {
        anyhow::bail!(
            "{other} isn't one of the stack's legacy networks ({})",
            legacy.join(", ")
        );
    }
    if present.is_empty() {
        return Ok(Vec::new());
    }
    let (mut worker, mut services, mut computes) = (Vec::new(), Vec::new(), Vec::new());
    for row in rows(ps) {
        let on_legacy = row.networks.iter().find(|n| legacy.iter().any(|l| l == *n));
        let ours = row.project == project;
        let our_compute = !row.endpoint.is_empty() && row.stack == slug;
        if ours {
            if row.service == "worker" {
                worker.push(row.name.to_string());
            }
            services.push(row.name.to_string());
        } else if our_compute {
            if on_legacy.is_some() {
                computes.push(row.name.to_string());
            }
        } else if let Some(net) = on_legacy {
            anyhow::bail!(
                "the container {} is on {net}, a network of this stack's earlier release, but is neither a compute of stack {slug} nor a container of project {project}; disconnect or remove it (`docker network disconnect {net} {}`) and run up again",
                row.name,
                row.name
            );
        }
    }
    Ok(vec![
        MigrationStep::StopWorker(worker),
        MigrationStep::RemoveProjectContainers(services),
        MigrationStep::RemoveComputes(computes),
        MigrationStep::RemoveNetworks(present.to_vec()),
    ])
}

/// Before the first Compose call: the stack's internal networks that already exist (a leftover or
/// hand-made network under the same name is reused by Compose without a check) must be internal
/// with both gateway modes `isolated`. `names` is `docker network ls --format {{.Name}}`;
/// `inspect` returns `docker network inspect <names>` for the ones that exist and isn't called
/// when none does.
pub fn precheck_existing_networks(
    project: &str,
    names: &str,
    inspect: impl FnOnce(&[String]) -> anyhow::Result<String>,
) -> anyhow::Result<()> {
    let wanted = [compute_network(project), control_network(project)];
    let names: Vec<&str> = names.lines().map(str::trim).collect();
    let existing: Vec<String> = wanted
        .into_iter()
        .filter(|n| names.contains(&n.as_str()))
        .collect();
    if existing.is_empty() {
        return Ok(());
    }
    check_live_networks(&inspect(&existing)?, &existing)
}

/// Whether `ensure_current` may treat the stack as running (rebuild and recreate in place):
/// not when a legacy network of the project exists, which only `up` migrates.
pub fn may_skip_up(slot_running: bool, missing: &[String], legacy_present: &[String]) -> bool {
    slot_running && missing.is_empty() && legacy_present.is_empty()
}

/// `docker ps` arguments listing the services of the project's running containers, one per
/// container: by Compose's project label, so no compose file or env is read (W-ENV), and without
/// one-off containers (`compose run`: the runner of a `test`/`run` aimed at this stack), which
/// `compose ps` doesn't count as services either.
pub fn running_ps_args(project: &str) -> Vec<String> {
    vec![
        "ps".into(),
        "--filter".into(),
        format!("label={PROJECT_LABEL}={project}"),
        "--filter".into(),
        format!("label={ONEOFF_LABEL}=False"),
        "--filter".into(),
        "status=running".into(),
        "--format".into(),
        format!("{{{{.Label \"{SERVICE_LABEL}\"}}}}"),
    ]
}

/// `docker ps` arguments listing the project's running worker containers.
pub fn worker_ps_args(project: &str) -> Vec<String> {
    vec![
        "ps".into(),
        "-q".into(),
        "--filter".into(),
        format!("label={PROJECT_LABEL}={project}"),
        "--filter".into(),
        format!("label={SERVICE_LABEL}=worker"),
    ]
}
