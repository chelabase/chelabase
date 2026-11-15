//! Client profiles: which [`OpKind`]s one client socket may use, on which targets, and the
//! body limits m3 enforces (networks, mount types, capabilities, bind sources, ports,
//! `container:` network mode).
//!
//! A profile is a JSON file (the contract m4 loads and the CLI renders in m5). Unknown keys,
//! unknown op kinds, unknown mount types and repeated op kinds are parse errors; every list
//! defaults to empty and every switch to off, so a missing key never widens anything.
//!
//! ```json
//! {
//!   "name": "worker",
//!   "ops": {
//!     "container_create": {},
//!     "container_start": { "target_label": "chelabase.dev/endpoint-id" }
//!   },
//!   "networks": ["chelabase-feat-x_compute", "chelabase-feat-x_control"],
//!   "mount_types": [],
//!   "cap_add": [],
//!   "binds": [],
//!   "publish_ports": false,
//!   "container_network_mode": false,
//!   "reserved_images": ["chelabase/neon-compute-v17:dev"]
//! }
//! ```

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};

use crate::Deny;
use crate::ops::{ImageRef, Op, OpKind, is_reserved_repository};
use crate::scope::Labels;

/// The compute containers' label (`chelabase-neon` `ENDPOINT_LABEL`).
pub const ENDPOINT_LABEL: &str = "chelabase.dev/endpoint-id";
/// The service containers' and networks' label (`service-runner` `SERVICE_LABEL`).
pub const SERVICE_LABEL: &str = "chelabase.dev/service-id";

/// A profile that doesn't parse or validate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileError(pub String);

impl fmt::Display for ProfileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProfileError {}

/// What one op kind needs beyond being listed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpRule {
    /// The target (the container, network or volume the path names; for exec ops, the exec's
    /// container) must carry this label with a non-empty value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_label: Option<String>,
}

/// The mount types a create body may use (m3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountType {
    Bind,
    Volume,
    Tmpfs,
}

/// One client's policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub name: String,
    /// The allowed op kinds. Anything absent is denied.
    #[serde(deserialize_with = "unique_ops")]
    pub ops: BTreeMap<OpKind, OpRule>,
    /// Network names a container may be created on or connected to (exact).
    #[serde(default)]
    pub networks: Vec<String>,
    #[serde(default)]
    pub mount_types: Vec<MountType>,
    /// Capabilities a create may add (`NET_ADMIN`).
    #[serde(default)]
    pub cap_add: Vec<String>,
    /// Exact bind sources (absolute host paths); never prefixes.
    #[serde(default)]
    pub binds: Vec<String>,
    #[serde(default)]
    pub publish_ports: bool,
    /// Whether `NetworkMode: container:<x>` is allowed.
    #[serde(default)]
    pub container_network_mode: bool,
    /// Local references a pull must never replace, on top of
    /// [`RESERVED_REPOSITORIES`](crate::ops::RESERVED_REPOSITORIES): the
    /// stack's compute and Neon images. A tag-only reference reserves its whole repository; a
    /// digest-pinned one may still be pulled in exactly its pinned form.
    #[serde(default)]
    pub reserved_images: Vec<String>,
}

/// `ops` as a map whose repeated keys are an error (serde's map would keep the last).
fn unique_ops<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<OpKind, OpRule>, D::Error> {
    struct Ops;
    impl<'de> Visitor<'de> for Ops {
        type Value = BTreeMap<OpKind, OpRule>;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a map of op kinds to rules")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            let mut out = BTreeMap::new();
            while let Some((k, v)) = map.next_entry::<OpKind, OpRule>()? {
                if out.insert(k, v).is_some() {
                    return Err(serde::de::Error::custom(format!(
                        "op {k:?} is listed twice"
                    )));
                }
            }
            Ok(out)
        }
    }
    d.deserialize_map(Ops)
}

/// The built-in profiles, one per socket client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Preset {
    Orchestrator,
    Worker,
    ServiceRunner,
    Runner,
}

/// The per-stack values the presets take from the proxy's config file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetConfig {
    /// `CHELABASE_COMPUTE_NETWORK` (`<project>_compute`).
    pub compute_network: String,
    /// `CHELABASE_COMPUTE_CONTROL_NETWORK` (`<project>_control`).
    pub control_network: String,
    /// The stack's Compose networks the runner's harness joins.
    #[serde(default)]
    pub stack_networks: Vec<String>,
    /// The runner's exact bind sources (m6 refines them).
    #[serde(default)]
    pub runner_binds: Vec<String>,
    /// The stack's compute and Neon images.
    #[serde(default)]
    pub reserved_images: Vec<String>,
}

impl PresetConfig {
    pub fn from_json(json: &str) -> Result<Self, ProfileError> {
        serde_json::from_str(json).map_err(|e| ProfileError(format!("preset config: {e}")))
    }
}

impl Profile {
    /// Parses and validates a profile file.
    pub fn from_json(json: &str) -> Result<Self, ProfileError> {
        let profile: Profile =
            serde_json::from_str(json).map_err(|e| ProfileError(format!("profile: {e}")))?;
        profile.validate()?;
        Ok(profile)
    }

    /// Checks the values serde can't: names, labels, capabilities, bind paths, references.
    pub fn validate(&self) -> Result<(), ProfileError> {
        let err = |what: String| Err(ProfileError(format!("profile {:?}: {what}", self.name)));
        let name_ok = |s: &str| {
            s.starts_with(|c: char| c.is_ascii_alphanumeric())
                && s.len() <= 128
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        };
        if !name_ok(&self.name) {
            return err("the name must be [A-Za-z0-9][A-Za-z0-9_.-]*".into());
        }
        for (kind, rule) in &self.ops {
            if let Some(label) = &rule.target_label
                && (label.is_empty()
                    || !label
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '/' | '_' | '-')))
            {
                return err(format!(
                    "{kind:?}: target label {label:?} is not a label key"
                ));
            }
        }
        if let Some(n) = self.networks.iter().find(|n| !name_ok(n)) {
            return err(format!("network {n:?} is not a network name"));
        }
        if let Some(c) = self.cap_add.iter().find(|c| {
            c.is_empty()
                || c.as_str() == "ALL"
                || !c
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        }) {
            return err(format!("capability {c:?} is not allowed"));
        }
        if let Some(b) = self.binds.iter().find(|b| !is_exact_path(b)) {
            return err(format!(
                "bind source {b:?} is not an absolute, normalised path"
            ));
        }
        for r in &self.reserved_images {
            if let Err(e) = ImageRef::parse(r) {
                return err(format!("reserved image: {e}"));
            }
        }
        Ok(())
    }

    /// The built-in profile of one client, inventoried from its code.
    pub fn preset(preset: Preset, config: &PresetConfig) -> Result<Self, ProfileError> {
        use OpKind::*;
        let rules = |kinds: &[(OpKind, Option<&str>)]| -> BTreeMap<OpKind, OpRule> {
            kinds
                .iter()
                .map(|(k, label)| {
                    (
                        *k,
                        OpRule {
                            target_label: label.map(str::to_string),
                        },
                    )
                })
                .collect()
        };
        let ep = Some(ENDPOINT_LABEL);
        let svc = Some(SERVICE_LABEL);
        let empty = Profile {
            name: String::new(),
            ops: BTreeMap::new(),
            networks: Vec::new(),
            mount_types: Vec::new(),
            cap_add: Vec::new(),
            binds: Vec::new(),
            publish_ports: false,
            container_network_mode: false,
            reserved_images: config.reserved_images.clone(),
        };
        let profile = match preset {
            // crates/chelabase-neon/src/compute.rs `DockerLocator`: `inspect` (compute status
            // and addresses) and the compute listing filtered by `list_filters`.
            Preset::Orchestrator => Profile {
                name: "orchestrator".into(),
                ops: rules(&[(ContainerList, None), (ContainerInspect, None)]),
                ..empty
            },
            // crates/chelabase-neon/src/backend.rs `DockerBackend`: `pull_image` (images/create),
            // `create` (containers/create on the compute network, no mounts; connect to the
            // control network; `upload_tls_tar` archive PUT; start), `read_file_inner` (archive
            // GET), `remove_container` (DELETE, force), `run_exec` (exec create/start/json),
            // `list`/`list_not_running` and `status` (containers/json, {id}/json).
            Preset::Worker => Profile {
                name: "worker".into(),
                ops: rules(&[
                    (ImageCreate, None),
                    (ContainerCreate, None),
                    (ContainerStart, ep),
                    (ContainerDelete, ep),
                    (ArchivePut, ep),
                    (ArchiveGet, ep),
                    (ExecCreate, ep),
                    (ExecStart, ep),
                    (ExecInspect, ep),
                    (NetworkConnect, None),
                    (ContainerList, None),
                    (ContainerInspect, None),
                ]),
                networks: vec![
                    config.compute_network.clone(),
                    config.control_network.clone(),
                ],
                ..empty
            },
            // services/service-runner/src/docker.rs `DockerBackend`: `image`/`repo_digests`
            // (images/{ref}/json, images/create), `list`/`inspect`, `create` (tmpfs mounts only,
            // no caps or binds, its slot network), `start`/`stop`/`remove`/`logs`, and the slot
            // networks (`list_networks`, `inspect_network`, `create_network`, `connect`,
            // `disconnect`, `remove_network`), all labelled `chelabase.dev/service-id`.
            Preset::ServiceRunner => Profile {
                name: "service-runner".into(),
                ops: rules(&[
                    (ImageInspect, None),
                    (ImageCreate, None),
                    (ContainerList, None),
                    (ContainerInspect, None),
                    (ContainerCreate, None),
                    (ContainerStart, svc),
                    (ContainerStop, svc),
                    (ContainerDelete, svc),
                    (ContainerLogs, svc),
                    (NetworkList, None),
                    (NetworkInspect, None),
                    (NetworkCreate, None),
                    (NetworkConnect, svc),
                    (NetworkDisconnect, svc),
                    (NetworkDelete, svc),
                ]),
                mount_types: vec![MountType::Tmpfs],
                ..empty
            },
            // The runner: nextest archives, the e2e scripts' docker CLI calls and `docker
            // compose up/ps/cp/config` (roles_live), `--network container:<x>`, `--cap-add
            // NET_ADMIN` (bench), `-v $state/grpc-tls:/tls` (storage-iam-e2e). Every op the
            // grammar has; m6 narrows its body rules.
            Preset::Runner => Profile {
                name: "runner".into(),
                ops: OpKind::ALL
                    .iter()
                    .map(|k| (*k, OpRule::default()))
                    .collect(),
                networks: config.stack_networks.clone(),
                mount_types: vec![MountType::Bind, MountType::Volume, MountType::Tmpfs],
                cap_add: vec!["NET_ADMIN".into()],
                binds: config.runner_binds.clone(),
                container_network_mode: true,
                ..empty
            },
        };
        profile.validate()?;
        Ok(profile)
    }

    /// The label `kind`'s target must carry, if its rule names one.
    pub fn target_label(&self, kind: OpKind) -> Option<&str> {
        self.ops.get(&kind)?.target_label.as_deref()
    }

    /// Whether the op's kind is listed (and, for a pull, its reference isn't reserved). An op
    /// with a [`target_label`](Self::target_label) still needs [`allows_target`](Self::allows_target)
    /// once the server has inspected the target.
    pub fn allows(&self, op: &Op) -> Result<(), Deny> {
        if !self.ops.contains_key(&op.kind()) {
            return Err(Deny::new(format!(
                "{:?} is not allowed for {}",
                op.kind(),
                self.name
            )));
        }
        if let Op::ImageCreate(pull) = op {
            let repo = pull.image.repository();
            if is_reserved_repository(&pull.image) {
                return Err(Deny::new(format!("{repo} is a locally built image")));
            }
            for reserved in &self.reserved_images {
                let reserved = ImageRef::parse(reserved)?;
                let exact_pin = reserved.digest.is_some() && reserved == pull.image;
                if reserved.repository() == repo && !exact_pin {
                    return Err(Deny::new(format!(
                        "{repo} is one of the stack's own images"
                    )));
                }
            }
        }
        Ok(())
    }

    /// [`allows`](Self::allows), plus the target-label rule against the target's labels. Stack
    /// scope is a separate check ([`in_scope`](crate::in_scope)).
    pub fn allows_target(&self, op: &Op, target: &Labels) -> Result<(), Deny> {
        self.allows(op)?;
        if let Some(label) = self.target_label(op.kind())
            && target.get(label).is_none_or(|v| v.is_empty())
        {
            return Err(Deny::new(format!(
                "{:?} needs a target labelled {label} for {}",
                op.kind(),
                self.name
            )));
        }
        Ok(())
    }
}

/// An absolute path with no empty, `.` or `..` segment and no trailing `/` (and not `/`).
fn is_exact_path(p: &str) -> bool {
    p.len() > 1
        && p.starts_with('/')
        && !p.chars().any(char::is_control)
        && p[1..]
            .split('/')
            .all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::classify;

    fn config() -> PresetConfig {
        PresetConfig {
            compute_network: "chelabase-feat-x_compute".into(),
            control_network: "chelabase-feat-x_control".into(),
            stack_networks: vec!["chelabase-feat-x_default".into()],
            runner_binds: vec!["/home/dev/chelabase/.chelabase/stacks/feat-x/grpc-tls".into()],
            reserved_images: vec!["chelabase/neon-compute-v17:dev".into()],
        }
    }

    fn labels(pairs: &[(&str, &str)]) -> Labels {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn unknown_key_rejected() {
        let ok = r#"{"name":"t","ops":{"container_list":{}}}"#;
        let p = Profile::from_json(ok).unwrap();
        assert_eq!(p.ops.len(), 1);
        assert!(p.networks.is_empty() && p.binds.is_empty() && p.cap_add.is_empty());
        assert!(!p.publish_ports && !p.container_network_mode);
        for bad in [
            // An unknown top-level key.
            r#"{"name":"t","ops":{},"privileged":true}"#,
            r#"{"name":"t","ops":{},"Networks":["x"]}"#,
            // An unknown key in an op rule, an unknown op kind, an unknown mount type.
            r#"{"name":"t","ops":{"container_list":{"any_target":true}}}"#,
            r#"{"name":"t","ops":{"container_update":{}}}"#,
            r#"{"name":"t","ops":{"ContainerList":{}}}"#,
            r#"{"name":"t","ops":{},"mount_types":["npipe"]}"#,
            // A repeated op kind or key.
            r#"{"name":"t","ops":{"container_start":{"target_label":"a"},"container_start":{}}}"#,
            r#"{"name":"t","name":"u","ops":{}}"#,
            // Missing required keys, wrong types.
            r#"{"ops":{}}"#,
            r#"{"name":"t"}"#,
            r#"{"name":"t","ops":{},"publish_ports":"yes"}"#,
            // Values that parse but don't validate.
            r#"{"name":"","ops":{}}"#,
            r#"{"name":"t","ops":{"container_start":{"target_label":""}}}"#,
            r#"{"name":"t","ops":{},"binds":["relative/path"]}"#,
            r#"{"name":"t","ops":{},"binds":["/a/../b"]}"#,
            r#"{"name":"t","ops":{},"binds":["/a/"]}"#,
            r#"{"name":"t","ops":{},"binds":["/"]}"#,
            r#"{"name":"t","ops":{},"binds":["/a//b"]}"#,
            r#"{"name":"t","ops":{},"cap_add":["ALL"]}"#,
            r#"{"name":"t","ops":{},"cap_add":["net_admin"]}"#,
            r#"{"name":"t","ops":{},"networks":[""]}"#,
            r#"{"name":"t","ops":{},"reserved_images":["Not A Ref"]}"#,
            "not json",
        ] {
            assert!(Profile::from_json(bad).is_err(), "{bad} loaded");
        }
        assert!(
            PresetConfig::from_json(r#"{"compute_network":"a","control_network":"b","extra":1}"#)
                .is_err()
        );
        assert!(
            PresetConfig::from_json(r#"{"compute_network":"a","control_network":"b"}"#).is_ok()
        );
    }

    #[test]
    fn presets_round_trip_through_json() {
        for preset in [
            Preset::Orchestrator,
            Preset::Worker,
            Preset::ServiceRunner,
            Preset::Runner,
        ] {
            let p = Profile::preset(preset, &config()).unwrap();
            let json = serde_json::to_string(&p).unwrap();
            assert_eq!(Profile::from_json(&json).unwrap(), p, "{preset:?}");
        }
    }

    #[test]
    fn worker_exec_archive_need_endpoint_label() {
        let worker = Profile::preset(Preset::Worker, &config()).unwrap();
        let compute = labels(&[("chelabase.dev/stack", "feat-x"), (ENDPOINT_LABEL, "ep1")]);
        let other = labels(&[
            ("chelabase.dev/stack", "feat-x"),
            ("com.docker.compose.service", "pageserver"),
        ]);
        let lookalike = labels(&[
            ("chelabase.dev/stack", "feat-x"),
            ("Chelabase.dev/endpoint-id", "ep1"),
        ]);
        let empty = labels(&[("chelabase.dev/stack", "feat-x"), (ENDPOINT_LABEL, "")]);
        for (m, p, q) in [
            ("POST", "/containers/x/exec", None),
            ("POST", "/exec/e/start", None),
            ("GET", "/exec/e/json", None),
            ("PUT", "/containers/x/archive", Some("path=%2Ftls")),
            ("GET", "/containers/x/archive", Some("path=%2Fx")),
            ("POST", "/containers/x/start", None),
            ("DELETE", "/containers/x", Some("force=true")),
        ] {
            let op = classify(m, p, q).unwrap();
            assert_eq!(
                worker.target_label(op.kind()),
                Some(ENDPOINT_LABEL),
                "{op:?}"
            );
            worker.allows_target(&op, &compute).unwrap();
            for bad in [&other, &lookalike, &empty, &Labels::new()] {
                assert!(worker.allows_target(&op, bad).is_err(), "{op:?} on {bad:?}");
            }
        }
        // Reads and the control-network connect need no endpoint label.
        let inspect = classify("GET", "/containers/x/json", None).unwrap();
        worker.allows_target(&inspect, &other).unwrap();
        // Not in the worker's inventory at all.
        for (m, p) in [
            ("POST", "/containers/x/stop"),
            ("POST", "/containers/x/kill"),
            ("HEAD", "/containers/x/archive"),
            ("GET", "/containers/x/logs"),
        ] {
            let op = classify(m, p, None).unwrap();
            assert!(worker.allows_target(&op, &compute).is_err(), "{op:?}");
        }
    }

    #[test]
    fn service_runner_lifecycle_needs_service_label() {
        let runner = Profile::preset(Preset::ServiceRunner, &config()).unwrap();
        let service = labels(&[("chelabase.dev/stack", "feat-x"), (SERVICE_LABEL, "s1")]);
        let compute = labels(&[("chelabase.dev/stack", "feat-x"), (ENDPOINT_LABEL, "ep1")]);
        for (m, p) in [
            ("POST", "/containers/x/start"),
            ("POST", "/containers/x/stop"),
            ("DELETE", "/containers/x"),
            ("GET", "/containers/x/logs"),
            ("DELETE", "/networks/n"),
            ("POST", "/networks/n/connect"),
            ("POST", "/networks/n/disconnect"),
        ] {
            let op = classify(m, p, None).unwrap();
            runner.allows_target(&op, &service).unwrap();
            assert!(
                runner.allows_target(&op, &compute).is_err(),
                "{op:?} on a compute"
            );
        }
        for (m, p) in [
            ("POST", "/containers/x/exec"),
            ("PUT", "/containers/x/archive"),
            ("POST", "/containers/x/kill"),
        ] {
            let op = classify(m, p, None).unwrap();
            assert!(runner.allows(&op).is_err(), "{op:?}");
        }
    }

    #[test]
    fn presets_stay_inside_their_inventory() {
        let cfg = config();
        let kinds = |p: Preset| -> Vec<OpKind> {
            Profile::preset(p, &cfg)
                .unwrap()
                .ops
                .keys()
                .copied()
                .collect()
        };
        use OpKind::*;
        assert_eq!(
            kinds(Preset::Orchestrator),
            vec![ContainerList, ContainerInspect]
        );
        let mut worker = vec![
            ContainerList,
            ContainerCreate,
            ContainerInspect,
            ContainerStart,
            ContainerDelete,
            ArchiveGet,
            ArchivePut,
            ExecCreate,
            ExecStart,
            ExecInspect,
            ImageCreate,
            NetworkConnect,
        ];
        worker.sort();
        assert_eq!(kinds(Preset::Worker), worker);
        let mut sr = vec![
            ContainerList,
            ContainerCreate,
            ContainerInspect,
            ContainerStart,
            ContainerStop,
            ContainerDelete,
            ContainerLogs,
            ImageCreate,
            ImageInspect,
            NetworkList,
            NetworkInspect,
            NetworkCreate,
            NetworkConnect,
            NetworkDisconnect,
            NetworkDelete,
        ];
        sr.sort();
        assert_eq!(kinds(Preset::ServiceRunner), sr);
        assert_eq!(kinds(Preset::Runner), OpKind::ALL.to_vec());
        let worker = Profile::preset(Preset::Worker, &cfg).unwrap();
        assert_eq!(
            worker.networks,
            vec![cfg.compute_network.clone(), cfg.control_network.clone()]
        );
        assert!(
            worker.mount_types.is_empty() && worker.binds.is_empty() && worker.cap_add.is_empty()
        );
        let sr = Profile::preset(Preset::ServiceRunner, &cfg).unwrap();
        assert_eq!(sr.mount_types, vec![MountType::Tmpfs]);
        assert!(sr.binds.is_empty() && sr.cap_add.is_empty() && !sr.publish_ports);
        let runner = Profile::preset(Preset::Runner, &cfg).unwrap();
        assert_eq!(runner.binds, cfg.runner_binds);
        assert_eq!(runner.cap_add, vec!["NET_ADMIN".to_string()]);
        assert!(runner.container_network_mode);
        for p in [
            Preset::Orchestrator,
            Preset::Worker,
            Preset::ServiceRunner,
            Preset::Runner,
        ] {
            assert_eq!(
                Profile::preset(p, &cfg).unwrap().reserved_images,
                cfg.reserved_images
            );
        }
        // A config that doesn't validate makes no preset.
        let bad = PresetConfig {
            reserved_images: vec!["BAD REF".into()],
            ..cfg.clone()
        };
        assert!(Profile::preset(Preset::Worker, &bad).is_err());
        let bad = PresetConfig {
            compute_network: String::new(),
            ..cfg
        };
        assert!(Profile::preset(Preset::Worker, &bad).is_err());
    }
}
