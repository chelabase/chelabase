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
//!     "container_start": { "target_label": "chelabase.dev/endpoint-id" },
//!     "network_connect": { "container_labels": [{ "key": "chelabase.dev/endpoint-id" }] }
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
/// Compose's service label: the service-runner attaches the stack's `functions` service to its
/// slot networks.
pub const COMPOSE_SERVICE_LABEL: &str = "com.docker.compose.service";

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
    /// Network connect and disconnect only: the container the body names must match one of
    /// these (checked by [`Profile::allows_connect_container`] once the server has resolved
    /// it). Empty means any in-scope container.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub container_labels: Vec<LabelMatch>,
    /// Network connect and disconnect only: any in-scope network, not just
    /// [`Profile::networks`] or networks carrying [`target_label`](Self::target_label).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub any_network: bool,
}

/// One label an object must carry: `key` with exactly `value`, or with any non-empty value
/// when `value` is absent. Keys compare byte for byte.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabelMatch {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

impl LabelMatch {
    /// `key` with any non-empty value.
    pub fn key(key: &str) -> Self {
        Self {
            key: key.to_string(),
            value: None,
        }
    }

    /// `key` with exactly `value`.
    pub fn exact(key: &str, value: &str) -> Self {
        Self {
            key: key.to_string(),
            value: Some(value.to_string()),
        }
    }

    pub fn matches(&self, labels: &Labels) -> bool {
        match (labels.get(&self.key), &self.value) {
            (Some(v), None) => !v.is_empty(),
            (Some(v), Some(want)) => v == want,
            (None, _) => false,
        }
    }
}

/// Whether `kind` connects a container to a network or disconnects it.
fn is_connect(kind: OpKind) -> bool {
    matches!(kind, OpKind::NetworkConnect | OpKind::NetworkDisconnect)
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
        let key_ok = |label: &str| {
            !label.is_empty()
                && label
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '/' | '_' | '-'))
        };
        for (kind, rule) in &self.ops {
            if let Some(label) = &rule.target_label
                && !key_ok(label)
            {
                return err(format!(
                    "{kind:?}: target label {label:?} is not a label key"
                ));
            }
            if !is_connect(*kind) && (!rule.container_labels.is_empty() || rule.any_network) {
                return err(format!(
                    "{kind:?}: container_labels and any_network apply to network connect and disconnect only"
                ));
            }
            for m in &rule.container_labels {
                if !key_ok(&m.key) {
                    return err(format!(
                        "{kind:?}: container label {:?} is not a label key",
                        m.key
                    ));
                }
                if let Some(v) = &m.value
                    && (v.is_empty() || v.chars().any(char::is_control))
                {
                    return err(format!(
                        "{kind:?}: container label value {v:?} is not allowed"
                    ));
                }
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
                            ..OpRule::default()
                        },
                    )
                })
                .collect()
        };
        let with_containers =
            |mut ops: BTreeMap<OpKind, OpRule>, kinds: &[OpKind], matches: &[LabelMatch]| {
                for k in kinds {
                    if let Some(rule) = ops.get_mut(k) {
                        rule.container_labels = matches.to_vec();
                    }
                }
                ops
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
            // crates/chelabase-neon/src/compute.rs `DockerLocator`: `inspect_container`
            // (compute status and addresses). It never lists.
            Preset::Orchestrator => Profile {
                name: "orchestrator".into(),
                ops: rules(&[(ContainerInspect, None)]),
                ..empty
            },
            // crates/chelabase-neon/src/backend.rs `DockerBackend`: `pull_image` (images/create),
            // `create` (containers/create on the compute network, no mounts; connect to the
            // control network; `upload_tls_tar` archive PUT; start), `read_file_inner` (archive
            // GET), `remove_container` (DELETE, force), `run_exec` (exec create/start/json),
            // `list`/`list_not_running` and `status` (containers/json, {id}/json). Its one
            // connect attaches a compute to the control network: the network must be one of
            // `networks`, and the container an endpoint.
            Preset::Worker => Profile {
                name: "worker".into(),
                ops: with_containers(
                    rules(&[
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
                    &[NetworkConnect],
                    &[LabelMatch::key(ENDPOINT_LABEL)],
                ),
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
            // `disconnect`, `remove_network`), all labelled `chelabase.dev/service-id`. It
            // connects and disconnects its service containers and the stack's `functions`
            // service (runner.rs `attach_functions`, `remove_network`).
            Preset::ServiceRunner => Profile {
                name: "service-runner".into(),
                ops: with_containers(
                    rules(&[
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
                    &[NetworkConnect, NetworkDisconnect],
                    &[
                        LabelMatch::key(SERVICE_LABEL),
                        LabelMatch::exact(COMPOSE_SERVICE_LABEL, "functions"),
                    ],
                ),
                mount_types: vec![MountType::Tmpfs],
                ..empty
            },
            // The runner: nextest archives, the e2e scripts' docker CLI calls and `docker
            // compose up/ps/cp/config` (roles_live), `--network container:<x>`, `--cap-add
            // NET_ADMIN` (bench), `-v $state/grpc-tls:/tls` (storage-iam-e2e). Every op the
            // grammar has, on any in-scope network; m6 narrows its body rules.
            Preset::Runner => Profile {
                name: "runner".into(),
                ops: OpKind::ALL
                    .iter()
                    .map(|k| {
                        let rule = OpRule {
                            any_network: is_connect(*k),
                            ..OpRule::default()
                        };
                        (*k, rule)
                    })
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

    /// The labels a network connect's or disconnect's container must match one of (empty: no
    /// rule beyond scope).
    pub fn container_labels(&self, kind: OpKind) -> &[LabelMatch] {
        self.ops
            .get(&kind)
            .map_or(&[], |r| r.container_labels.as_slice())
    }

    /// The network a connect or disconnect path names, when it is one of
    /// [`networks`](Self::networks) by exact name.
    fn listed_network(&self, op: &Op) -> bool {
        match op {
            Op::NetworkConnect(n) | Op::NetworkDisconnect(n) => self.networks.contains(n),
            _ => false,
        }
    }

    /// Whether the op's kind is listed (and, for a pull, its reference isn't reserved). An op
    /// with a [`target_label`](Self::target_label) still needs [`allows_target`](Self::allows_target)
    /// once the server has inspected the target.
    ///
    /// A network connect or disconnect also needs its path's network to be one of
    /// [`networks`](Self::networks) by exact name, unless its rule has a `target_label` (then
    /// [`allows_target`](Self::allows_target) checks the network's label) or `any_network`.
    /// Its body's container still needs
    /// [`allows_connect_container`](Self::allows_connect_container).
    pub fn allows(&self, op: &Op) -> Result<(), Deny> {
        let Some(rule) = self.ops.get(&op.kind()) else {
            return Err(Deny::new(format!(
                "{:?} is not allowed for {}",
                op.kind(),
                self.name
            )));
        };
        if let Op::NetworkConnect(n) | Op::NetworkDisconnect(n) = op
            && rule.target_label.is_none()
            && !rule.any_network
            && !self.listed_network(op)
        {
            return Err(Deny::new(format!(
                "network {n} is not one of {}'s networks",
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
    /// A connect or disconnect on one of [`networks`](Self::networks) needs no target label.
    pub fn allows_target(&self, op: &Op, target: &Labels) -> Result<(), Deny> {
        self.allows(op)?;
        if let Some(label) = self.target_label(op.kind())
            && !self.listed_network(op)
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

impl Profile {
    /// For a network connect or disconnect: [`allows`](Self::allows), plus the container the
    /// body names (`Container`, resolved and scope-checked by the server) against the rule's
    /// [`container_labels`](Self::container_labels). Any other op is refused.
    pub fn allows_connect_container(&self, op: &Op, container: &Labels) -> Result<(), Deny> {
        if !is_connect(op.kind()) {
            return Err(Deny::new(format!(
                "{:?} names no container to connect",
                op.kind()
            )));
        }
        self.allows(op)?;
        let matches = self.container_labels(op.kind());
        if !matches.is_empty() && !matches.iter().any(|m| m.matches(container)) {
            return Err(Deny::new(format!(
                "{:?} needs a container labelled {} for {}",
                op.kind(),
                matches
                    .iter()
                    .map(|m| match &m.value {
                        Some(v) => format!("{}={v}", m.key),
                        None => m.key.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(" or "),
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
    fn worker_connect_only_profile_networks_and_computes() {
        let worker = Profile::preset(Preset::Worker, &config()).unwrap();
        // The audit's case: a network outside the worker's list (the stack's default network,
        // another stack's, the host's).
        for n in [
            "chelabase-a_default",
            "chelabase-feat-x_default",
            "bridge",
            "host",
        ] {
            let op = classify("POST", &format!("/networks/{n}/connect"), None).unwrap();
            assert!(worker.allows(&op).is_err(), "worker connects to {n}");
        }
        // Its own networks pass the path check, need no network label, and take only computes.
        let compute = labels(&[("chelabase.dev/stack", "feat-x"), (ENDPOINT_LABEL, "ep1")]);
        let network = labels(&[("com.docker.compose.project", "chelabase-feat-x")]);
        for n in ["chelabase-feat-x_compute", "chelabase-feat-x_control"] {
            let op = classify("POST", &format!("/networks/{n}/connect"), None).unwrap();
            worker.allows(&op).unwrap();
            worker.allows_target(&op, &network).unwrap();
            assert_eq!(
                worker.container_labels(op.kind()),
                &[LabelMatch::key(ENDPOINT_LABEL)]
            );
            worker.allows_connect_container(&op, &compute).unwrap();
            for bad in [
                // An in-stack service (metadata-db, pageserver).
                labels(&[
                    ("com.docker.compose.project", "chelabase-feat-x"),
                    (COMPOSE_SERVICE_LABEL, "metadata-db"),
                ]),
                labels(&[("chelabase.dev/stack", "feat-x"), (ENDPOINT_LABEL, "")]),
                labels(&[
                    ("chelabase.dev/stack", "feat-x"),
                    ("Chelabase.dev/endpoint-id", "ep1"),
                ]),
                labels(&[("chelabase.dev/stack", "feat-x"), (SERVICE_LABEL, "s1")]),
                Labels::new(),
            ] {
                assert!(
                    worker.allows_connect_container(&op, &bad).is_err(),
                    "worker connects {bad:?} to {n}"
                );
            }
        }
        // A listed network by a name it isn't (case, ID) is not listed.
        for n in ["Chelabase-feat-x_control", "chelabase-feat-x_control2"] {
            let op = classify("POST", &format!("/networks/{n}/connect"), None).unwrap();
            assert!(
                worker.allows_connect_container(&op, &compute).is_err(),
                "{n}"
            );
        }
        // The container check refuses ops that aren't connects, and the worker never
        // disconnects.
        let start = classify("POST", "/containers/x/start", None).unwrap();
        assert!(worker.allows_connect_container(&start, &compute).is_err());
        let op = classify(
            "POST",
            "/networks/chelabase-feat-x_control/disconnect",
            None,
        )
        .unwrap();
        assert!(worker.allows(&op).is_err());
        assert!(worker.allows_connect_container(&op, &compute).is_err());
    }

    #[test]
    fn service_runner_connects_only_services_and_functions() {
        let runner = Profile::preset(Preset::ServiceRunner, &config()).unwrap();
        let slot = labels(&[("chelabase.dev/stack", "feat-x"), (SERVICE_LABEL, "s1")]);
        let service = slot.clone();
        let functions = labels(&[
            ("com.docker.compose.project", "chelabase-feat-x"),
            (COMPOSE_SERVICE_LABEL, "functions"),
        ]);
        for path in ["/networks/n/connect", "/networks/n/disconnect"] {
            let op = classify("POST", path, None).unwrap();
            runner.allows_target(&op, &slot).unwrap();
            runner.allows_connect_container(&op, &service).unwrap();
            runner.allows_connect_container(&op, &functions).unwrap();
            for bad in [
                labels(&[
                    ("com.docker.compose.project", "chelabase-feat-x"),
                    (COMPOSE_SERVICE_LABEL, "pageserver"),
                ]),
                labels(&[
                    ("com.docker.compose.project", "chelabase-feat-x"),
                    (COMPOSE_SERVICE_LABEL, "Functions"),
                ]),
                labels(&[("chelabase.dev/stack", "feat-x"), (ENDPOINT_LABEL, "ep1")]),
                labels(&[("chelabase.dev/stack", "feat-x"), (SERVICE_LABEL, "")]),
            ] {
                assert!(
                    runner.allows_connect_container(&op, &bad).is_err(),
                    "{path} with {bad:?}"
                );
            }
            // The network still needs the service label (its `networks` list is empty).
            let compose_net = labels(&[("com.docker.compose.project", "chelabase-feat-x")]);
            assert!(runner.allows_target(&op, &compose_net).is_err(), "{path}");
        }
        // The runner may connect anything in scope to any in-scope network.
        let all = Profile::preset(Preset::Runner, &config()).unwrap();
        let op = classify("POST", "/networks/chelabase-feat-x_control/connect", None).unwrap();
        all.allows_connect_container(&op, &Labels::new()).unwrap();
    }

    #[test]
    fn connect_rules_parse_and_validate() {
        let ok = r#"{"name":"t","ops":{"network_connect":{"container_labels":[{"key":"a.b/c"},{"key":"x","value":"y"}]},"network_disconnect":{"any_network":true}}}"#;
        let p = Profile::from_json(ok).unwrap();
        assert_eq!(
            p.container_labels(OpKind::NetworkConnect),
            &[LabelMatch::key("a.b/c"), LabelMatch::exact("x", "y")]
        );
        // Without `networks`, a target label or any_network, a connect is refused.
        let p = Profile::from_json(r#"{"name":"t","ops":{"network_connect":{}}}"#).unwrap();
        let op = classify("POST", "/networks/n/connect", None).unwrap();
        assert!(p.allows(&op).is_err());
        for bad in [
            // Connect-only fields on other ops.
            r#"{"name":"t","ops":{"container_start":{"container_labels":[{"key":"a"}]}}}"#,
            r#"{"name":"t","ops":{"network_create":{"any_network":true}}}"#,
            // Bad keys and values, unknown fields.
            r#"{"name":"t","ops":{"network_connect":{"container_labels":[{"key":""}]}}}"#,
            r#"{"name":"t","ops":{"network_connect":{"container_labels":[{"key":"a b"}]}}}"#,
            r#"{"name":"t","ops":{"network_connect":{"container_labels":[{"key":"a","value":""}]}}}"#,
            r#"{"name":"t","ops":{"network_connect":{"container_labels":[{"key":"a","any":1}]}}}"#,
            r#"{"name":"t","ops":{"network_connect":{"container_labels":"a"}}}"#,
            r#"{"name":"t","ops":{"network_connect":{"any_network":"yes"}}}"#,
        ] {
            assert!(Profile::from_json(bad).is_err(), "{bad} loaded");
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
        assert_eq!(kinds(Preset::Orchestrator), vec![ContainerInspect]);
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
