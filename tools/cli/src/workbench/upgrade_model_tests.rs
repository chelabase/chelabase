//! W-ENV (see `engine.rs`): every workbench entry point against every state an upgrade can leave a
//! stack in, through a fake Docker and Compose that keeps a model of the containers, networks and
//! volumes, and refuses (as Compose does) to load the project with a `stack.env` that lacks a
//! variable the compose files require.

use super::commands::{
    INFRA, LATE_APPS, Stack, down_stack, purge_stack, restart_stack, stack_for_run, up_stack,
};
use super::engine::{DockerOut, Engine, missing_required, required_vars, stack_env_current};
use super::registry::{port_is_free, with_registry};
use super::slug::project_name;
use chelabase_init::kinds::service_auth::AuthFilesChanged;
use chelabase_neon::compute::{ENDPOINT_LABEL, STACK_LABEL};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const SLUG: &str = "feat-up";
const PROJECT_LABEL: &str = "com.docker.compose.project";
const SERVICE_LABEL: &str = "com.docker.compose.service";
const ONEOFF_LABEL: &str = "com.docker.compose.oneoff";
/// The env file of the release before the internal networks.
const STALE_ENV: &str = include_str!("fixtures/stack-env-before-internal-networks.env");
/// What `scripts/tests/workbench-compose.sh` renders from: this release's env.
const CURRENT_FIXTURE: &str =
    include_str!("../../../../scripts/tests/fixtures/workbench-stack.env");

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn compose_texts() -> [String; 2] {
    [
        std::fs::read_to_string(repo().join("infra/compose/docker-compose.yml")).unwrap(),
        std::fs::read_to_string(repo().join("infra/compose/docker-compose.workbench.yml")).unwrap(),
    ]
}

// --- the pure check ---

#[test]
fn the_previous_release_env_is_stale_and_this_one_is_current() {
    let texts = compose_texts();
    let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
    let required = required_vars(&texts);
    for var in [
        "CHELABASE_COMPUTE_SUBNET",
        "CHELABASE_COMPUTE_RANGE",
        "CHELABASE_COMPUTE_EGRESS_IP",
        "CHELABASE_CONTROL_INTERNAL_SUBNET",
        "CHELABASE_ORCHESTRATOR_CONTROL_INTERNAL_IP",
    ] {
        assert!(
            required.iter().any(|r| r == var),
            "{var} isn't required: {required:?}"
        );
    }
    let mut missing = missing_required(STALE_ENV, &required);
    missing.sort();
    assert_eq!(
        missing,
        // The control network's settings were renamed with the internal networks, so the old
        // env's CHELABASE_CONTROL_SUBNET and its address and range settings don't count.
        [
            "CHELABASE_COMPUTE_EGRESS_IP",
            "CHELABASE_COMPUTE_RANGE",
            "CHELABASE_COMPUTE_SUBNET",
            "CHELABASE_CONTROL_INTERNAL_COMPUTE_RANGE",
            "CHELABASE_CONTROL_INTERNAL_SUBNET",
            "CHELABASE_ORCHESTRATOR_CONTROL_INTERNAL_IP",
            "CHELABASE_REALTIME_CONTROL_INTERNAL_IP",
            "CHELABASE_RUNNER_CONTROL_INTERNAL_IP"
        ]
    );
    assert!(!stack_env_current(STALE_ENV, &texts));
    assert!(stack_env_current(CURRENT_FIXTURE, &texts));
}

#[test]
fn required_vars_reads_both_forms_and_skips_escaped_dollars() {
    let text = "a: ${A:?x}\nb: ${B?y}\nc: ${C:-d}\nd: $${E:?no}\ne: $$${F:?yes}\nf: ${A:?again}\n";
    assert_eq!(required_vars(&[text]), ["A", "B", "F"]);
    assert_eq!(
        missing_required("A=1\nB=\n# F=1\n", &required_vars(&[text])),
        ["B", "F"]
    );
}

// --- the model ---

#[derive(Debug, Clone)]
struct Ctr {
    id: String,
    name: String,
    labels: BTreeMap<String, String>,
    networks: Vec<String>,
    running: bool,
}

#[derive(Debug, Clone)]
struct Net {
    id: String,
    name: String,
    internal: bool,
    project: Option<String>,
}

/// A failure injected into the first (`nth`: n-th) `docker` call `matches` picks, or into the
/// workbench image build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fault {
    /// `docker stop` (P3a).
    Stop,
    /// The n-th `docker ps -a --no-trunc` snapshot (P3b).
    Snapshot(usize),
    /// `docker rm -f` of the project's containers (P3c).
    RmProject,
    /// `docker rm -f` of the stack's computes (P3d).
    RmComputes,
    /// `docker network rm` (P3e).
    NetworkRm,
    /// The workbench image (P5).
    Images,
}

#[derive(Default)]
struct World {
    ctrs: Vec<Ctr>,
    nets: Vec<Net>,
    volumes: Vec<(String, String)>,
    next: u32,
    fault: Option<Fault>,
    seen_snapshots: usize,
    /// Every call, in order (`docker …` or `compose …`).
    log: Vec<String>,
    /// Compose calls made with a stale env (W-ENV breaks).
    stale_compose: Vec<String>,
    /// What `secrets` reports.
    nats_auth_changed: bool,
}

struct Fake {
    w: RefCell<World>,
}

fn ok(stdout: String) -> anyhow::Result<DockerOut> {
    Ok(DockerOut {
        ok: true,
        stdout,
        stderr: String::new(),
    })
}

fn fail(stderr: &str) -> anyhow::Result<DockerOut> {
    Ok(DockerOut {
        ok: false,
        stdout: String::new(),
        stderr: stderr.to_string(),
    })
}

fn render(format: &str, c: &Ctr) -> String {
    let mut out = String::new();
    let mut rest = format;
    while let Some(at) = rest.find("{{") {
        out.push_str(&rest[..at]);
        let end = rest[at..].find("}}").expect("closed") + at;
        let token = rest[at + 2..end].trim();
        let value = match token {
            ".Names" => c.name.clone(),
            ".ID" => c.id.clone(),
            ".Networks" => c.networks.join(","),
            t if t.starts_with(".Label ") => {
                let key = t[".Label ".len()..].trim().trim_matches('"');
                c.labels.get(key).cloned().unwrap_or_default()
            }
            other => panic!("the fake can't render {other}"),
        };
        out.push_str(&value);
        rest = &rest[end + 2..];
    }
    out.push_str(rest);
    out
}

fn label_filter(f: &str, labels: &BTreeMap<String, String>) -> bool {
    match f.split_once('=') {
        Some((k, v)) => labels.get(k).map(String::as_str) == Some(v),
        None => labels.contains_key(f),
    }
}

impl World {
    fn id(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}{:04}", self.next)
    }

    fn find(&self, key: &str) -> Option<usize> {
        self.ctrs.iter().position(|c| c.id == key || c.name == key)
    }

    fn net(&self, key: &str) -> Option<usize> {
        self.nets.iter().position(|n| n.id == key || n.name == key)
    }

    fn add_net(&mut self, name: &str, internal: bool) {
        if self.net(name).is_none() {
            let id = self.id("n");
            self.nets.push(Net {
                id,
                name: name.to_string(),
                internal,
                project: Some(project_name(SLUG)),
            });
        }
    }

    fn add_ctr(&mut self, name: &str, labels: &[(&str, &str)], networks: &[String], running: bool) {
        let id = self.id("c");
        self.ctrs.push(Ctr {
            id,
            name: name.to_string(),
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            networks: networks.to_vec(),
            running,
        });
    }

    fn fires(&mut self, args: &[String]) -> bool {
        let Some(fault) = self.fault else {
            return false;
        };
        let hit = match (fault, args.first().map(String::as_str)) {
            (Fault::Stop, Some("stop")) => true,
            (Fault::Snapshot(n), Some("ps")) if args.iter().any(|a| a == "--no-trunc") => {
                self.seen_snapshots += 1;
                self.seen_snapshots == n
            }
            (Fault::RmProject | Fault::RmComputes, Some("rm")) => {
                let compute = args[1..]
                    .iter()
                    .filter_map(|a| self.find(a))
                    .any(|i| self.ctrs[i].labels.contains_key(ENDPOINT_LABEL));
                (fault == Fault::RmComputes) == compute
            }
            (Fault::NetworkRm, Some("network")) => args.get(1).is_some_and(|a| a == "rm"),
            _ => false,
        };
        if hit {
            self.fault = None;
        }
        hit
    }

    fn ps(&self, args: &[String]) -> anyhow::Result<DockerOut> {
        let (mut all, mut quiet, mut format, mut filters) = (false, false, None, Vec::new());
        let mut it = args[1..].iter();
        while let Some(a) = it.next() {
            match a.as_str() {
                "-a" => all = true,
                "-q" => quiet = true,
                "-aq" => (all, quiet) = (true, true),
                "--no-trunc" => {}
                "--filter" => filters.push(it.next().unwrap().clone()),
                "--format" => format = Some(it.next().unwrap().clone()),
                other => panic!("the fake's ps doesn't take {other}"),
            }
        }
        let mut out = String::new();
        for c in &self.ctrs {
            if !all && !c.running {
                continue;
            }
            let pass = filters.iter().all(|f| match f.split_once('=') {
                Some(("label", l)) => label_filter(l, &c.labels),
                Some(("status", "running")) => c.running,
                _ => panic!("the fake's ps doesn't filter by {f}"),
            });
            if !pass {
                continue;
            }
            if quiet {
                out.push_str(&c.id);
            } else {
                out.push_str(&render(format.as_deref().expect("a format"), c));
            }
            out.push('\n');
        }
        ok(out)
    }

    fn network(&mut self, args: &[String]) -> anyhow::Result<DockerOut> {
        match args.get(1).map(String::as_str) {
            Some("ls") => {
                let (mut quiet, mut filter) = (false, None);
                let mut it = args[2..].iter();
                while let Some(a) = it.next() {
                    match a.as_str() {
                        "-q" => quiet = true,
                        "--format" => assert_eq!(it.next().unwrap(), "{{.Name}}"),
                        "--filter" => filter = Some(it.next().unwrap().clone()),
                        other => panic!("the fake's network ls doesn't take {other}"),
                    }
                }
                let project = filter.map(|f| {
                    f.strip_prefix(&format!("label={PROJECT_LABEL}="))
                        .expect("a project filter")
                        .to_string()
                });
                let mut out = String::new();
                for n in &self.nets {
                    if project
                        .as_ref()
                        .is_some_and(|p| n.project.as_ref() != Some(p))
                    {
                        continue;
                    }
                    out.push_str(if quiet { &n.id } else { &n.name });
                    out.push('\n');
                }
                ok(out)
            }
            Some("inspect") => {
                let mut found = Vec::new();
                for key in &args[2..] {
                    let Some(i) = self.net(key) else {
                        return fail(&format!(
                            "Error response from daemon: network {key} not found"
                        ));
                    };
                    let n = &self.nets[i];
                    let options = if n.internal {
                        serde_json::json!({
                            "com.docker.network.bridge.gateway_mode_ipv4": "isolated",
                            "com.docker.network.bridge.gateway_mode_ipv6": "isolated",
                        })
                    } else {
                        serde_json::json!({})
                    };
                    found.push(serde_json::json!({
                        "Name": n.name, "Id": n.id, "Internal": n.internal,
                        "Options": options, "IPAM": {"Config": []},
                    }));
                }
                ok(serde_json::Value::Array(found).to_string())
            }
            Some("rm") => {
                for key in &args[2..] {
                    let Some(i) = self.net(key) else {
                        return fail(&format!(
                            "Error response from daemon: network {key} not found"
                        ));
                    };
                    let name = self.nets[i].name.clone();
                    // As Docker: only a running member holds a network; a stopped one loses it.
                    if self.ctrs.iter().any(|c| c.running && c.networks.contains(&name)) {
                        return fail(&format!(
                            "Error response from daemon: error while removing network: network {name} has active endpoints"
                        ));
                    }
                    self.nets.remove(i);
                }
                ok(String::new())
            }
            other => panic!("the fake has no network {other:?}"),
        }
    }

    fn volume(&mut self, args: &[String]) -> anyhow::Result<DockerOut> {
        match args.get(1).map(String::as_str) {
            Some("ls") => {
                let filter = args
                    .iter()
                    .position(|a| a == "--filter")
                    .map(|i| args[i + 1].clone());
                let project = filter.and_then(|f| {
                    f.strip_prefix(&format!("label={PROJECT_LABEL}="))
                        .map(str::to_string)
                });
                let out: String = self
                    .volumes
                    .iter()
                    .filter(|(_, p)| project.as_ref().is_none_or(|want| want == p))
                    .map(|(v, _)| format!("{v}\n"))
                    .collect();
                ok(out)
            }
            Some("rm") => {
                for v in &args[2..] {
                    self.volumes.retain(|(name, _)| name != v);
                }
                ok(String::new())
            }
            other => panic!("the fake has no volume {other:?}"),
        }
    }

    /// Compose's own view of `up <services>`: networks it needs (failing as Docker does when the
    /// legacy control network still holds the slot's subnet), then each service on them.
    fn compose_up(&mut self, project: &str, services: &[String]) -> anyhow::Result<()> {
        let (default, compute, control) = (
            format!("{project}_default"),
            format!("{project}_compute-internal"),
            format!("{project}_control-internal"),
        );
        if self.net(&control).is_none() && self.net(&format!("{project}_control")).is_some() {
            anyhow::bail!(
                "failed to create network {control}: Pool overlaps with other one on this address space"
            );
        }
        self.add_net(&default, false);
        self.add_net(&compute, true);
        self.add_net(&control, true);
        for svc in services {
            let mut networks = vec![default.clone(), control.clone()];
            if svc == "worker" || svc == "compute-egress" {
                networks.push(compute.clone());
            }
            let name = format!("{project}-{svc}-1");
            match self.find(&name) {
                Some(i) => {
                    self.ctrs[i].running = true;
                    self.ctrs[i].networks = networks;
                }
                None => self.add_ctr(
                    &name,
                    &[
                        (PROJECT_LABEL, project),
                        (SERVICE_LABEL, svc),
                        (ONEOFF_LABEL, "False"),
                    ],
                    &networks,
                    true,
                ),
            }
        }
        Ok(())
    }
}

impl Engine for Fake {
    fn docker(&self, args: &[String]) -> anyhow::Result<DockerOut> {
        let mut w = self.w.borrow_mut();
        w.log.push(format!("docker {}", args.join(" ")));
        if w.fires(args) {
            return fail("injected failure");
        }
        match args.first().map(String::as_str) {
            Some("version") => ok("28.1.1\n".into()),
            Some("ps") => w.ps(args),
            Some("stop") => {
                for key in &args[1..] {
                    let Some(i) = w.find(key) else {
                        return fail(&format!(
                            "Error response from daemon: No such container: {key}"
                        ));
                    };
                    w.ctrs[i].running = false;
                }
                ok(String::new())
            }
            Some("rm") => {
                let force = args.get(1).is_some_and(|a| a == "-f");
                for key in &args[if force { 2 } else { 1 }..] {
                    let Some(i) = w.find(key) else {
                        return fail(&format!(
                            "Error response from daemon: No such container: {key}"
                        ));
                    };
                    if w.ctrs[i].running && !force {
                        return fail(
                            "Error response from daemon: cannot remove a running container",
                        );
                    }
                    w.ctrs.remove(i);
                }
                ok(String::new())
            }
            Some("network") => w.network(args),
            Some("volume") => w.volume(args),
            other => panic!("the fake has no docker {other:?} ({args:?})"),
        }
    }

    fn compose(&self, stack: &Stack, args: &[String], _capture: bool) -> anyhow::Result<String> {
        let mut w = self.w.borrow_mut();
        let line = format!("compose {}", args.join(" "));
        w.log.push(line.clone());
        // Compose's interpolation, on this checkout's files.
        let env = std::fs::read_to_string(&stack.env_file).unwrap_or_default();
        let texts = [
            std::fs::read_to_string(&stack.compose_file).unwrap(),
            std::fs::read_to_string(&stack.overlay_file).unwrap(),
        ];
        let required = required_vars(&[&texts[0], &texts[1]]);
        if let Some(var) = missing_required(&env, &required).first() {
            w.stale_compose.push(line);
            anyhow::bail!("required variable {var} is missing a value");
        }
        let mut rest: Vec<String> = Vec::new();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if a == "--profile" {
                it.next();
            } else {
                rest.push(a.clone());
            }
        }
        let project = stack.project.clone();
        match rest.first().map(String::as_str) {
            Some("up") => {
                let mut services = Vec::new();
                let mut it = rest[1..].iter();
                while let Some(a) = it.next() {
                    match a.as_str() {
                        "-d" | "--wait" | "--force-recreate" | "--no-deps" => {}
                        "--wait-timeout" => {
                            it.next();
                        }
                        s => services.push(s.to_string()),
                    }
                }
                w.compose_up(&project, &services)?;
            }
            Some("stop") => {
                for c in &mut w.ctrs {
                    if c.labels.get(PROJECT_LABEL) == Some(&project) {
                        c.running = false;
                    }
                }
            }
            Some("down") => {
                w.ctrs
                    .retain(|c| c.labels.get(PROJECT_LABEL) != Some(&project));
                let held: Vec<String> = w.ctrs.iter().flat_map(|c| c.networks.clone()).collect();
                w.nets
                    .retain(|n| n.project.as_ref() != Some(&project) || held.contains(&n.name));
                if rest.iter().any(|a| a == "-v") {
                    w.volumes.retain(|(_, p)| p != &project);
                }
            }
            Some("restart" | "cp" | "logs" | "ps" | "run" | "exec") => {}
            other => panic!("the fake has no compose {other:?}"),
        }
        Ok(String::new())
    }

    fn workbench_image(&self, _: &Stack) -> anyhow::Result<String> {
        let mut w = self.w.borrow_mut();
        w.log.push("image workbench".into());
        if w.fault == Some(Fault::Images) {
            w.fault = None;
            anyhow::bail!("injected image build failure");
        }
        Ok("chelabase-workbench:model".into())
    }

    fn neon_images(&self, _: &Stack) -> anyhow::Result<Vec<(String, String)>> {
        Ok(vec![
            ("NEON_IMAGE".into(), "neon:model".into()),
            ("COMPUTE_NODE_IMAGE".into(), "compute:model".into()),
            ("CHELABASE_COMPUTE_IMAGE".into(), "compute:model".into()),
        ])
    }

    fn functions_runtime_image(&self, _: &Stack) -> anyhow::Result<String> {
        Ok("wb-model".into())
    }

    fn cargo_build(&self, _: &Stack, apps: &[&str], _: bool) -> anyhow::Result<()> {
        self.w
            .borrow_mut()
            .log
            .push(format!("cargo build {}", apps.join(" ")));
        Ok(())
    }

    fn secrets(&self, _: &Stack) -> anyhow::Result<AuthFilesChanged> {
        Ok(AuthFilesChanged {
            nats: self.w.borrow().nats_auth_changed,
            valkey: false,
        })
    }

    fn bootstrap(&self, _: &Stack) -> anyhow::Result<()> {
        self.w.borrow_mut().log.push("bootstrap".into());
        Ok(())
    }

    fn docker_gid(&self) -> anyhow::Result<u32> {
        Ok(999)
    }
}

// --- the states ---

/// A checkout with this release's compose files and `.env.example`, and a main checkout whose
/// `.chelabase` holds the registry and the stack's state.
struct Bench {
    _tmp: tempfile::TempDir,
    stack: Stack,
    /// Another checkout, for `--stack`.
    other: Stack,
    fake: Fake,
}

impl Bench {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let checkout = tmp.path().join("w/feat-up");
        let compose = checkout.join("infra/compose");
        std::fs::create_dir_all(&compose).unwrap();
        for f in [
            "docker-compose.yml",
            "docker-compose.workbench.yml",
            ".env.example",
        ] {
            std::fs::copy(repo().join("infra/compose").join(f), compose.join(f)).unwrap();
        }
        let main = tmp.path().join("m");
        std::fs::create_dir_all(&main).unwrap();
        let other_checkout = tmp.path().join("w/other");
        std::fs::create_dir_all(&other_checkout).unwrap();
        Bench {
            stack: Stack::new(SLUG, &checkout, &main),
            other: Stack::new("other", &other_checkout, &main),
            _tmp: tmp,
            fake: Fake {
                w: RefCell::new(World::default()),
            },
        }
    }

    fn project(&self) -> String {
        self.stack.project.clone()
    }

    /// The stack as the earlier release left it running: registered with its slot, the old
    /// `stack.env`, every service on the legacy networks, two computes on them, a data volume.
    fn legacy(&self) {
        let p = self.project();
        with_registry(&self.stack.stacks_dir, |r| {
            r.ensure(SLUG, &self.stack.checkout, 1, port_is_free)?;
            r.get_mut(SLUG).unwrap().running_since = Some(now());
            Ok(())
        })
        .unwrap();
        std::fs::create_dir_all(self.stack.env_file.parent().unwrap()).unwrap();
        std::fs::write(&self.stack.env_file, STALE_ENV).unwrap();
        let mut w = self.fake.w.borrow_mut();
        let (default, compute, control) = (
            format!("{p}_default"),
            format!("{p}_compute"),
            format!("{p}_control"),
        );
        w.add_net(&default, false);
        w.add_net(&compute, false);
        w.add_net(&control, false);
        let services: Vec<&str> = INFRA
            .iter()
            .chain(LATE_APPS)
            .copied()
            .chain(["gateway", "identity", "mailpit"])
            .collect();
        for svc in services {
            let mut nets = vec![default.clone(), control.clone()];
            if svc == "worker" || svc == "compute-egress" {
                nets.push(compute.clone());
            }
            w.add_ctr(
                &format!("{p}-{svc}-1"),
                &[
                    (PROJECT_LABEL, &p),
                    (SERVICE_LABEL, svc),
                    (ONEOFF_LABEL, "False"),
                ],
                &nets,
                true,
            );
        }
        for ep in ["ep-one", "ep-two"] {
            w.add_ctr(
                &format!("{p}-compute-{ep}"),
                &[(ENDPOINT_LABEL, ep), (STACK_LABEL, SLUG)],
                &[compute.clone(), control.clone()],
                true,
            );
        }
        w.volumes.push((format!("{p}_metadata-db"), p.clone()));
    }

    /// A stopped container of nobody's on the stack's compute network (a hand-made attachment).
    fn stopped_foreign(&self) {
        let net = super::env::compute_network(&self.project());
        let mut w = self.fake.w.borrow_mut();
        w.add_ctr("someone-else", &[], std::slice::from_ref(&net), false);
    }

    /// A container of nobody's on the legacy compute network.
    fn foreign(&self) {
        let p = self.project();
        self.fake
            .w
            .borrow_mut()
            .add_ctr("someone-else", &[], &[format!("{p}_compute")], true);
    }

    fn up(&self) -> anyhow::Result<()> {
        up_stack(&self.fake, &self.stack, &[])
    }

    fn fault(&self, f: Fault) {
        let mut w = self.fake.w.borrow_mut();
        w.fault = Some(f);
        w.seen_snapshots = 0;
    }

    /// Removes `var` from `stack.env`, as a release that newly requires it would find it.
    fn drop_env_var(&self, var: &str) {
        let env = std::fs::read_to_string(&self.stack.env_file).unwrap();
        let kept: String = env
            .lines()
            .filter(|l| !l.starts_with(&format!("{var}=")))
            .map(|l| format!("{l}\n"))
            .collect();
        assert_ne!(kept.len(), env.len(), "{var} wasn't in stack.env");
        std::fs::write(&self.stack.env_file, kept).unwrap();
        assert!(!self.env_current());
    }

    fn env_current(&self) -> bool {
        let env = std::fs::read_to_string(&self.stack.env_file).unwrap_or_default();
        let texts = compose_texts();
        stack_env_current(&env, &[&texts[0], &texts[1]])
    }

    fn running(&self) -> Option<u64> {
        super::registry::with_registry(&self.stack.stacks_dir, |r| Ok(r.get(SLUG).cloned()))
            .unwrap()
            .and_then(|e| e.running_since)
    }

    fn registered(&self) -> bool {
        with_registry(&self.stack.stacks_dir, |r| Ok(r.get(SLUG).is_some())).unwrap()
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// A running stack of the earlier release, its env stale.
    Legacy,
    /// `up` on Legacy failed at this fault.
    Failed(Fault),
    /// Legacy with a foreign container on a legacy network.
    Foreign,
    /// A stack this release brought up.
    Current,
    /// A stack this release brought up, running with every service, no legacy network, and a
    /// `stack.env` lacking a required variable (the checkout moved to a release that requires
    /// a new one).
    CurrentStaleEnv,
    /// Never up.
    Never,
}

const STATES: [State; 12] = [
    State::Legacy,
    State::Failed(Fault::Stop),
    State::Failed(Fault::Snapshot(1)),
    State::Failed(Fault::Snapshot(2)),
    State::Failed(Fault::RmProject),
    State::Failed(Fault::RmComputes),
    State::Failed(Fault::NetworkRm),
    State::Failed(Fault::Images),
    State::Foreign,
    State::Current,
    State::CurrentStaleEnv,
    State::Never,
];

fn bench(state: State) -> Bench {
    let b = Bench::new();
    match state {
        State::Never => {}
        State::Current => b.up().expect("a first up"),
        State::CurrentStaleEnv => {
            b.up().expect("a first up");
            b.drop_env_var("CHELABASE_COMPUTE_SUBNET");
        }
        State::Legacy => b.legacy(),
        State::Foreign => {
            b.legacy();
            b.foreign();
        }
        State::Failed(f) => {
            b.legacy();
            b.fault(f);
            // A fault this code never meets leaves whatever `up` made.
            let _ = b.up();
        }
    }
    b.fake.w.borrow_mut().log.clear();
    b
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Entry {
    Up,
    TestRun,
    Down,
    Purge,
    Restart,
    OtherStack,
}

const ENTRIES: [Entry; 6] = [
    Entry::Up,
    Entry::TestRun,
    Entry::Down,
    Entry::Purge,
    Entry::Restart,
    Entry::OtherStack,
];

fn run(b: &Bench, e: Entry) -> anyhow::Result<()> {
    match e {
        Entry::Up => b.up(),
        Entry::TestRun => stack_for_run(&b.fake, &b.stack, None).map(drop),
        Entry::Down => down_stack(&b.fake, &b.stack, None),
        Entry::Purge => purge_stack(&b.fake, &b.stack, None, false),
        Entry::Restart => restart_stack(&b.fake, &b.stack, &[]),
        Entry::OtherStack => stack_for_run(&b.fake, &b.other, Some(SLUG)).map(drop),
    }
}

/// What must hold after entry point `e` ran on state `s` and returned `r`; the broken
/// expectations.
fn check(b: &Bench, s: State, e: Entry, r: &anyhow::Result<()>) -> Vec<String> {
    let mut bad = Vec::new();
    let w = b.fake.w.borrow();
    let p = b.project();
    let legacy = [format!("{p}_compute"), format!("{p}_control")];
    if !w.stale_compose.is_empty() {
        bad.push(format!(
            "Compose ran with a stale env: {:?}",
            w.stale_compose
        ));
    }
    let msg = r
        .as_ref()
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap_or_default();
    if msg.contains("required variable") {
        bad.push(format!("a raw interpolation error: {msg}"));
    }
    let ours = |c: &Ctr| c.labels.get(PROJECT_LABEL) == Some(&p);
    let our_compute = |c: &Ctr| c.labels.get(STACK_LABEL).map(String::as_str) == Some(SLUG);
    let migrated = |bad: &mut Vec<String>| {
        if let Some(n) = w.nets.iter().find(|n| legacy.contains(&n.name)) {
            bad.push(format!("legacy network {} is left", n.name));
        }
        if let Some(c) = w
            .ctrs
            .iter()
            .find(|c| c.networks.iter().any(|n| legacy.contains(n)))
        {
            bad.push(format!("{} is still on a legacy network", c.name));
        }
        if !b.env_current() {
            bad.push("stack.env is still stale".into());
        }
        let running = |svc: &str| {
            w.ctrs.iter().any(|c| {
                ours(c) && c.running && c.labels.get(SERVICE_LABEL).map(String::as_str) == Some(svc)
            })
        };
        if let Some(svc) = LATE_APPS.iter().chain(INFRA).find(|s| !running(s)) {
            bad.push(format!("{svc} isn't running"));
        }
    };
    let names_recovery = |bad: &mut Vec<String>, want: &str| {
        if r.is_ok() {
            bad.push(format!("succeeded; expected a refusal naming `{want}`"));
        } else if !msg.contains(want) {
            bad.push(format!("the error doesn't name `{want}`: {msg}"));
        }
    };
    match (e, s) {
        (Entry::Up | Entry::TestRun, State::Foreign) => {
            names_recovery(&mut bad, "docker network disconnect");
            // AC 9: nothing stopped, the worker included.
            if let Some(c) = w.ctrs.iter().find(|c| ours(c) && !c.running) {
                bad.push(format!("{} was stopped", c.name));
            }
            if w.find("someone-else").is_none() {
                bad.push("the foreign container was removed".into());
            }
        }
        (Entry::Up | Entry::TestRun, _) => match r {
            Ok(()) => migrated(&mut bad),
            Err(_) => bad.push(format!("failed: {msg}")),
        },
        (Entry::OtherStack, State::Never) => names_recovery(&mut bad, "is registered"),
        (Entry::Down | Entry::Purge, State::Never) => names_recovery(&mut bad, "isn't registered"),
        (Entry::Down | Entry::Purge, _) => {
            if r.is_err() {
                bad.push(format!("failed: {msg}"));
            }
            if let Some(c) = w.ctrs.iter().find(|c| ours(c) || our_compute(c)) {
                bad.push(format!("{} is left", c.name));
            }
            if s == State::Foreign && w.find("someone-else").is_none() {
                bad.push("the foreign container was removed".into());
            }
            if e == Entry::Down && b.running().is_some() {
                bad.push("the slot is still held".into());
            }
            if e == Entry::Purge {
                if b.registered() {
                    bad.push("the stack is still registered".into());
                }
                if b.stack.state.exists() {
                    bad.push("the state dir is left".into());
                }
                if !w.volumes.is_empty() {
                    bad.push(format!("volumes left: {:?}", w.volumes));
                }
            }
        }
        (Entry::Restart | Entry::OtherStack, State::Current) => {
            if r.is_err() {
                bad.push(format!("failed: {msg}"));
            }
        }
        (Entry::Restart | Entry::OtherStack, _) => names_recovery(&mut bad, "just workbench up"),
    }
    bad
}

/// The table: every entry point on every state either converges with no manual step or fails
/// naming the one command that recovers it, and Compose never runs on a stale env.
#[test]
fn every_entry_point_on_every_upgrade_state() {
    let mut broken = Vec::new();
    for s in STATES {
        for e in ENTRIES {
            let b = bench(s);
            let r = run(&b, e);
            let bad = check(&b, s, e, &r);
            let verdict = if bad.is_empty() {
                "ok".to_string()
            } else {
                bad.join("; ")
            };
            eprintln!("{s:?} × {e:?}: {verdict}");
            if !bad.is_empty() {
                broken.push(format!("{s:?} × {e:?}: {verdict}"));
            }
        }
    }
    assert!(
        broken.is_empty(),
        "{} broken cells:\n{}",
        broken.len(),
        broken.join("\n")
    );
}

/// After any failure of `up` on a legacy stack, `down` (what `up`'s failure message offers) and
/// a clean `up` both work, and `up` converges to the migrated state.
#[test]
fn a_clean_up_after_any_failure_converges() {
    for f in [
        Fault::Stop,
        Fault::Snapshot(1),
        Fault::Snapshot(2),
        Fault::RmProject,
        Fault::RmComputes,
        Fault::NetworkRm,
        Fault::Images,
    ] {
        let b = bench(State::Failed(f));
        let r = b.up();
        let bad = check(&b, State::Failed(f), Entry::Up, &r);
        assert!(bad.is_empty(), "{f:?}: {bad:?}");
        // Idempotent: another up changes no container.
        let before: Vec<String> = b
            .fake
            .w
            .borrow()
            .ctrs
            .iter()
            .map(|c| c.id.clone())
            .collect();
        b.up().unwrap();
        let after: Vec<String> = b
            .fake
            .w
            .borrow()
            .ctrs
            .iter()
            .map(|c| c.id.clone())
            .collect();
        assert_eq!(before, after, "{f:?}");
    }
}

/// A2: a `compose run` container (another checkout's runner aimed at this stack) doesn't make a
/// stopped stack count as running: `up` doesn't restart NATS for an auth change (the stopped
/// NATS starts from the new file anyway).
#[test]
fn a_one_off_runner_does_not_make_the_stack_running() {
    let b = bench(State::Current);
    let p = b.project();
    {
        let mut w = b.fake.w.borrow_mut();
        for c in &mut w.ctrs {
            c.running = false;
        }
        w.add_ctr(
            &format!("{p}-runner-run-3f2a"),
            &[
                (PROJECT_LABEL, &p),
                (SERVICE_LABEL, "runner"),
                (ONEOFF_LABEL, "True"),
            ],
            &[format!("{p}_default")],
            true,
        );
        w.nats_auth_changed = true;
        w.log.clear();
    }
    b.up().unwrap();
    let log = b.fake.w.borrow().log.clone();
    assert!(
        !log.iter().any(|l| l.starts_with("compose restart nats")),
        "a stopped stack restarted NATS: {log:#?}"
    );
    // And the other way: a running stack does restart it.
    b.fake.w.borrow_mut().log.clear();
    b.up().unwrap();
    let log = b.fake.w.borrow().log.clone();
    assert!(
        log.iter().any(|l| l.starts_with("compose restart nats")),
        "{log:#?}"
    );
}

/// S3 / AC 9: a foreign member on a legacy network stops `up` before anything is stopped.
#[test]
fn a_foreign_member_stops_up_with_the_worker_running() {
    let b = bench(State::Foreign);
    let err = b.up().unwrap_err();
    assert!(format!("{err:#}").contains("someone-else"), "{err:#}");
    let w = b.fake.w.borrow();
    assert!(
        !w.log
            .iter()
            .any(|l| l.starts_with("docker stop") || l.starts_with("docker rm")),
        "{:#?}",
        w.log
    );
    let worker = format!("{}-worker-1", b.project());
    assert!(w.ctrs[w.find(&worker).unwrap()].running);
}

/// N1-A: Docker removes a network whose only member is stopped, and that container can't start
/// again; `down` and `purge` by label keep a network that a foreign container (stopped or not)
/// is on.
#[test]
fn a_stopped_foreign_container_keeps_its_network_through_down_and_purge() {
    for e in [Entry::Down, Entry::Purge] {
        // Stale env: both go by label.
        let b = bench(State::CurrentStaleEnv);
        b.stopped_foreign();
        run(&b, e).unwrap();
        let w = b.fake.w.borrow();
        let net = super::env::compute_network(&b.project());
        assert!(w.find("someone-else").is_some(), "{e:?}: the foreign container was removed");
        assert!(
            w.net(&net).is_some(),
            "{e:?}: the network of a stopped foreign container was removed"
        );
        assert!(
            w.ctrs.iter().all(|c| c.name == "someone-else"),
            "{e:?}: {:?}",
            w.ctrs
        );
    }
}
