//! The Docker backend: the hardened create body and the Engine API calls behind
//! [`ContainerBackend`].

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context as _;
use async_trait::async_trait;
use bollard::Docker;
use bollard::errors::Error as DockerError;
use bollard::models::{
    ContainerCreateBody, HostConfig, HostConfigCgroupnsModeEnum, HostConfigLogConfig, Mount,
    MountTmpfsOptions, MountType, NetworkConnectRequest, NetworkCreateRequest,
    NetworkDisconnectRequest, RestartPolicy, RestartPolicyNameEnum,
};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, ListContainersOptions, ListNetworksOptions,
    LogsOptions, RemoveContainerOptions, StartContainerOptions, StopContainerOptions,
};
use futures_util::StreamExt as _;

use crate::backend::{Container, ContainerBackend, ContainerPlan, ImageFacts, Network};
use crate::spec::ImageRef;
use crate::{SERVICE_LABEL, STACK_LABEL};

/// Processes per container.
pub const PIDS_LIMIT: i64 = 256;
/// Each tmpfs (`/tmp`, an image volume): 64 MiB, counted against the container's memory.
pub const TMPFS_BYTES: i64 = 64 * 1024 * 1024;
/// The only security option: no process gains privileges (setuid, file capabilities). The
/// engine's default seccomp and AppArmor profiles stay on: nothing here unconfines them.
pub const NO_NEW_PRIVILEGES: &str = "no-new-privileges:true";
/// How long a pull may take.
const PULL_TIMEOUT: Duration = Duration::from_secs(600);
/// The most log bytes one call returns.
const LOG_LIMIT: usize = 512 * 1024;

/// The create body for `plan` on `network`. Everything that could reach the host is set off or
/// left empty explicitly: privileged, binds, volumes, devices, host namespaces, published ports,
/// extra capabilities. Mounts are tmpfs only. Memory has no swap on top.
pub fn create_body(plan: &ContainerPlan, network: &str) -> ContainerCreateBody {
    let mounts = plan
        .tmpfs
        .iter()
        .map(|target| Mount {
            target: Some(target.clone()),
            typ: Some(MountType::TMPFS),
            read_only: Some(false),
            tmpfs_options: Some(MountTmpfsOptions {
                size_bytes: Some(TMPFS_BYTES),
                mode: Some(0o1777),
                options: None,
            }),
            ..Default::default()
        })
        .collect();
    ContainerCreateBody {
        // The inspected content id, never the tag again (S2).
        image: Some(plan.image_id.clone()),
        user: Some(plan.user.clone()),
        env: Some(plan.env.clone()),
        labels: Some(plan.labels.clone()),
        attach_stdin: Some(false),
        attach_stdout: Some(false),
        attach_stderr: Some(false),
        tty: Some(false),
        open_stdin: Some(false),
        host_config: Some(HostConfig {
            privileged: Some(false),
            readonly_rootfs: Some(true),
            cap_drop: Some(vec!["ALL".to_string()]),
            cap_add: Some(Vec::new()),
            security_opt: Some(vec![NO_NEW_PRIVILEGES.to_string()]),
            memory: Some(plan.limits.memory_bytes),
            memory_swap: Some(plan.limits.memory_bytes),
            nano_cpus: Some(plan.limits.nano_cpus),
            pids_limit: Some(PIDS_LIMIT),
            network_mode: Some(network.to_string()),
            mounts: Some(mounts),
            binds: Some(Vec::new()),
            devices: Some(Vec::new()),
            ipc_mode: Some("private".to_string()),
            cgroupns_mode: Some(HostConfigCgroupnsModeEnum::PRIVATE),
            publish_all_ports: Some(false),
            oom_kill_disable: Some(false),
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                maximum_retry_count: None,
            }),
            // Bounded logs: a chatty service can't fill the host's disk.
            log_config: Some(HostConfigLogConfig {
                typ: Some("json-file".to_string()),
                config: Some(HashMap::from([
                    ("max-size".to_string(), "10m".to_string()),
                    ("max-file".to_string(), "3".to_string()),
                ])),
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// [`ContainerBackend`] on the Docker socket.
pub struct DockerBackend {
    docker: Docker,
}

impl DockerBackend {
    pub fn connect() -> anyhow::Result<Self> {
        Ok(DockerBackend {
            docker: Docker::connect_with_socket_defaults().context("connecting to Docker")?,
        })
    }

    pub fn docker(&self) -> &Docker {
        &self.docker
    }
}

fn is_not_found(e: &DockerError) -> bool {
    matches!(
        e,
        DockerError::DockerResponseServerError {
            status_code: 404,
            ..
        }
    )
}

/// Not found and already-in-that-state answers count as done.
fn tolerate(r: Result<(), DockerError>, what: &str, id: &str) -> anyhow::Result<()> {
    match r {
        Ok(()) => Ok(()),
        Err(DockerError::DockerResponseServerError {
            status_code: 304 | 404,
            ..
        }) => Ok(()),
        Err(e) => Err(anyhow::Error::new(e).context(format!("{what} container {id}"))),
    }
}

#[async_trait]
impl ContainerBackend for DockerBackend {
    async fn inspect(&self, id: &str) -> anyhow::Result<Option<Container>> {
        match self.docker.inspect_container(id, None).await {
            Ok(c) => Ok(Some(Container {
                id: c.id.unwrap_or_default(),
                labels: c.config.and_then(|c| c.labels).unwrap_or_default(),
                state: c
                    .state
                    .and_then(|s| s.status)
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
            })),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(anyhow::Error::new(e).context(format!("inspecting container {id}"))),
        }
    }

    async fn list(&self, stack: &str) -> anyhow::Result<Vec<Container>> {
        let filters = HashMap::from([(
            "label".to_string(),
            vec![format!("{STACK_LABEL}={stack}"), SERVICE_LABEL.to_string()],
        )]);
        let list = self
            .docker
            .list_containers(Some(ListContainersOptions {
                all: true,
                filters: Some(filters),
                ..Default::default()
            }))
            .await
            .context("listing service containers")?;
        Ok(list
            .into_iter()
            .map(|c| Container {
                id: c.id.unwrap_or_default(),
                labels: c.labels.unwrap_or_default(),
                state: c.state.map(|s| s.to_string()).unwrap_or_default(),
            })
            .collect())
    }

    async fn image(&self, image: &ImageRef) -> anyhow::Result<ImageFacts> {
        let reference = image.to_string();
        let found = match self.docker.inspect_image(&reference).await {
            Ok(i) => i,
            Err(e) if is_not_found(&e) => {
                let options = CreateImageOptions {
                    from_image: Some(image.name.clone()),
                    // Never empty: an empty tag pulls every tag.
                    tag: Some(image.tag_or_digest().to_string()),
                    ..Default::default()
                };
                let pull = async {
                    let mut stream = self.docker.create_image(Some(options), None, None);
                    while let Some(step) = stream.next().await {
                        step.with_context(|| format!("pulling {reference}"))?;
                    }
                    anyhow::Ok(())
                };
                tokio::time::timeout(PULL_TIMEOUT, pull)
                    .await
                    .map_err(|_| anyhow::anyhow!("pulling {reference} timed out"))??;
                self.docker
                    .inspect_image(&reference)
                    .await
                    .with_context(|| format!("inspecting {reference} after the pull"))?
            }
            Err(e) => return Err(anyhow::Error::new(e).context(format!("inspecting {reference}"))),
        };
        let config = found.config.unwrap_or_default();
        Ok(ImageFacts {
            id: found.id.unwrap_or_default(),
            user: config.user.filter(|u| !u.trim().is_empty()),
            volumes: config.volumes.unwrap_or_default(),
            labels: config.labels.unwrap_or_default(),
        })
    }

    async fn create(&self, plan: &ContainerPlan, network: &str) -> anyhow::Result<String> {
        let created = self
            .docker
            .create_container(
                Some(CreateContainerOptions {
                    name: Some(plan.name.clone()),
                    ..Default::default()
                }),
                create_body(plan, network),
            )
            .await
            .with_context(|| format!("creating container {}", plan.name))?;
        Ok(created.id)
    }

    async fn start(&self, id: &str) -> anyhow::Result<()> {
        tolerate(
            self.docker
                .start_container(id, None::<StartContainerOptions>)
                .await,
            "starting",
            id,
        )
    }

    async fn stop(&self, id: &str) -> anyhow::Result<()> {
        tolerate(
            self.docker
                .stop_container(
                    id,
                    Some(StopContainerOptions {
                        t: Some(10),
                        signal: None,
                    }),
                )
                .await,
            "stopping",
            id,
        )
    }

    async fn remove(&self, id: &str) -> anyhow::Result<()> {
        tolerate(
            self.docker
                .remove_container(
                    id,
                    Some(RemoveContainerOptions {
                        force: true,
                        v: true,
                        link: false,
                    }),
                )
                .await,
            "removing",
            id,
        )
    }

    async fn logs(&self, id: &str, tail: usize) -> anyhow::Result<String> {
        let mut stream = self.docker.logs(
            id,
            Some(LogsOptions {
                stdout: true,
                stderr: true,
                tail: tail.to_string(),
                ..Default::default()
            }),
        );
        let mut out: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.with_context(|| format!("reading the logs of container {id}"))?;
            out.extend_from_slice(&chunk.into_bytes());
            if out.len() > LOG_LIMIT {
                // Keep the newest bytes.
                out.drain(..out.len() - LOG_LIMIT);
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    async fn list_networks(&self, stack: &str) -> anyhow::Result<Vec<Network>> {
        let filters = HashMap::from([(
            "label".to_string(),
            vec![format!("{STACK_LABEL}={stack}"), SERVICE_LABEL.to_string()],
        )]);
        let list = self
            .docker
            .list_networks(Some(ListNetworksOptions {
                filters: Some(filters),
            }))
            .await
            .context("listing service networks")?;
        let mut out = Vec::new();
        // The listing has no endpoints: each is inspected (one per service, few).
        for n in list {
            if let Some(n) = self.inspect_network(&n.id.unwrap_or_default()).await? {
                out.push(n);
            }
        }
        Ok(out)
    }

    async fn inspect_network(&self, name: &str) -> anyhow::Result<Option<Network>> {
        if name.is_empty() {
            return Ok(None);
        }
        match self.docker.inspect_network(name, None).await {
            Ok(n) => Ok(Some(Network {
                id: n.id.unwrap_or_default(),
                name: n.name.unwrap_or_default(),
                labels: n.labels.unwrap_or_default(),
                containers: n.containers.unwrap_or_default().into_keys().collect(),
            })),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(anyhow::Error::new(e).context(format!("inspecting network {name}"))),
        }
    }

    async fn create_network(
        &self,
        name: &str,
        labels: &HashMap<String, String>,
    ) -> anyhow::Result<String> {
        let created = self
            .docker
            .create_network(NetworkCreateRequest {
                name: name.to_string(),
                driver: Some("bridge".to_string()),
                // Not internal: services reach the internet. Not attachable by others.
                internal: Some(false),
                attachable: Some(false),
                labels: Some(labels.clone()),
                ..Default::default()
            })
            .await
            .with_context(|| format!("creating network {name}"))?;
        Ok(created.id)
    }

    async fn remove_network(&self, id: &str) -> anyhow::Result<()> {
        match self.docker.remove_network(id).await {
            Ok(()) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(anyhow::Error::new(e).context(format!("removing network {id}"))),
        }
    }

    async fn connect(&self, network: &str, container: &str) -> anyhow::Result<()> {
        let attached = self
            .inspect_network(network)
            .await?
            .is_some_and(|n| n.containers.iter().any(|c| c == container));
        if attached {
            return Ok(());
        }
        self.docker
            .connect_network(
                network,
                NetworkConnectRequest {
                    container: container.to_string(),
                    endpoint_config: None,
                },
            )
            .await
            .with_context(|| format!("attaching {container} to network {network}"))
    }

    async fn disconnect(&self, network: &str, container: &str) -> anyhow::Result<()> {
        match self
            .docker
            .disconnect_network(
                network,
                NetworkDisconnectRequest {
                    container: container.to_string(),
                    force: Some(true),
                },
            )
            .await
        {
            Ok(()) => Ok(()),
            Err(e) if is_not_found(&e) => Ok(()),
            Err(e) => Err(anyhow::Error::new(e)
                .context(format!("detaching {container} from network {network}"))),
        }
    }

    async fn functions_containers(&self, project: &str) -> anyhow::Result<Vec<String>> {
        let filters = HashMap::from([(
            "label".to_string(),
            vec![
                format!("com.docker.compose.project={project}"),
                "com.docker.compose.service=functions".to_string(),
            ],
        )]);
        let list = self
            .docker
            .list_containers(Some(ListContainersOptions {
                filters: Some(filters),
                ..Default::default()
            }))
            .await
            .context("listing the functions containers")?;
        // Never one of the runner's own (an image can't carry these labels: plan refuses it).
        Ok(list
            .into_iter()
            .filter(|c| {
                !c.labels
                    .as_ref()
                    .is_some_and(|l| l.contains_key(SERVICE_LABEL))
            })
            .filter_map(|c| c.id)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::runner::{Key, plan};
    use crate::spec::{Caps, ContainerSpec, Resources, validate};

    const NET: &str = "chelabase-feat-x_services";

    /// Whether `user` names uid 0 in any form the engine would accept.
    fn root_user(user: &str) -> bool {
        let uid = user.split(':').next().unwrap_or("");
        uid.is_empty()
            || uid == "root"
            || uid.parse::<u64>().ok() == Some(0)
            || uid.parse::<u64>().is_err()
    }

    /// Every invariant of the hardening, on one body.
    fn assert_hardened(body: &ContainerCreateBody, caps: &Caps) {
        let h = body.host_config.as_ref().expect("a host config");
        assert_eq!(h.privileged, Some(false));
        assert_eq!(h.readonly_rootfs, Some(true));
        assert_eq!(h.cap_drop.as_deref(), Some(&["ALL".to_string()][..]));
        assert!(h.cap_add.as_ref().is_none_or(|c| c.is_empty()));
        assert_eq!(
            h.security_opt.as_deref(),
            Some(&[NO_NEW_PRIVILEGES.to_string()][..])
        );
        assert!(h.binds.as_ref().is_none_or(|b| b.is_empty()), "no bind");
        assert!(h.volumes_from.is_none());
        assert!(h.volume_driver.is_none());
        assert!(h.devices.as_ref().is_none_or(|d| d.is_empty()), "no device");
        assert!(h.device_requests.is_none() && h.device_cgroup_rules.is_none());
        for m in h.mounts.as_deref().unwrap_or_default() {
            assert_eq!(m.typ, Some(MountType::TMPFS), "{m:?}");
            assert!(m.source.is_none() && m.bind_options.is_none() && m.volume_options.is_none());
            assert!(
                m.target
                    .as_deref()
                    .is_some_and(|t| t.starts_with('/') && t != "/"),
                "{m:?}"
            );
        }
        assert!(
            body.volumes.as_ref().is_none_or(|v| v.is_empty()),
            "no anonymous volume"
        );
        assert_eq!(h.network_mode.as_deref(), Some(NET));
        assert!(h.pid_mode.is_none() && h.userns_mode.is_none() && h.uts_mode.is_none());
        assert_eq!(h.ipc_mode.as_deref(), Some("private"));
        assert_eq!(h.cgroupns_mode, Some(HostConfigCgroupnsModeEnum::PRIVATE));
        assert!(h.port_bindings.is_none() && h.publish_all_ports == Some(false));
        assert!(h.extra_hosts.is_none() && h.sysctls.is_none() && h.runtime.is_none());
        assert!(h.cgroup_parent.is_none() && h.cgroup.is_none() && h.links.is_none());
        let cap = caps.limits();
        let memory = h.memory.expect("a memory limit");
        assert!(memory > 0 && memory <= cap.memory_bytes, "{memory}");
        assert_eq!(h.memory_swap, Some(memory), "no swap on top");
        let cpus = h.nano_cpus.expect("a cpu limit");
        assert!(cpus > 0 && cpus <= cap.nano_cpus, "{cpus}");
        assert_eq!(h.pids_limit, Some(PIDS_LIMIT));
        let user = body.user.as_deref().expect("a user");
        assert!(!root_user(user), "non-root: {user}");
        let labels = body.labels.as_ref().unwrap();
        assert_eq!(labels.get(STACK_LABEL).map(String::as_str), Some("feat-x"));
        assert!(labels.contains_key(SERVICE_LABEL));
        assert!(body.entrypoint.is_none() && body.cmd.is_none());
    }

    fn key() -> Key {
        Key {
            service_id: Uuid::new_v4(),
            branch_id: Uuid::new_v4(),
            deployment_id: Uuid::new_v4(),
        }
    }

    #[test]
    fn a_plain_spec_is_hardened() {
        let caps = Caps::default();
        let v = crate::spec::parse(
            &json!({"backend": "container", "image": "ghcr.io/acme/echo:1", "port": 8080}),
            &caps,
        )
        .unwrap();
        let facts = ImageFacts {
            id: format!("sha256:{}", "b".repeat(64)),
            user: None,
            volumes: vec!["/data".into()],
            labels: HashMap::from([("org.opencontainers.image.title".into(), "echo".into())]),
        };
        let p = plan("feat-x", &key(), &v, &facts, &BTreeMap::new(), "digest").unwrap();
        let body = create_body(&p, NET);
        assert_hardened(&body, &caps);
        assert_eq!(body.user.as_deref(), Some("65532:65532"));
        // Audit S2: created from the inspected image id, never the tag.
        assert_eq!(body.image.as_deref(), Some(facts.id.as_str()));
        assert_eq!(
            body.labels.unwrap()[crate::runner::ENV_DIGEST_LABEL],
            "digest"
        );
        let targets: Vec<_> = body
            .host_config
            .unwrap()
            .mounts
            .unwrap()
            .into_iter()
            .map(|m| m.target.unwrap())
            .collect();
        assert_eq!(targets, ["/tmp", "/data"]);
    }

    fn image_user() -> impl Strategy<Value = Option<String>> {
        prop_oneof![
            Just(None),
            Just(Some("root".to_string())),
            Just(Some("0".to_string())),
            Just(Some("0:0".to_string())),
            Just(Some("00".to_string())),
            Just(Some("root:root".to_string())),
            Just(Some("1000:0".to_string())),
            (0u32..70000).prop_map(|u| Some(u.to_string())),
            (0u32..70000, 0u32..70000).prop_map(|(u, g)| Some(format!("{u}:{g}"))),
            ".{0,12}".prop_map(Some),
        ]
    }

    fn volume() -> impl Strategy<Value = String> {
        prop_oneof![
            "/[a-z/]{0,12}",
            Just("/".to_string()),
            Just("/tmp".to_string()),
            Just("/data/../..".to_string()),
            ".{0,16}",
        ]
    }

    fn env_map() -> impl Strategy<Value = BTreeMap<String, String>> {
        proptest::collection::btree_map("[A-Za-z_][A-Za-z0-9_]{0,8}", ".{0,8}", 0..6)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        /// Arbitrary specs, image users, image volumes and env: whatever comes out is hardened,
        /// and a limit above a cap is always refused, never clamped.
        #[test]
        fn arbitrary_inputs_never_escape(
            memory in proptest::option::of(any::<u32>()),
            cpus in proptest::option::of(prop_oneof![any::<f64>(), 0.0f64..4.0]),
            cap_memory in 16u32..4096,
            cap_cpus in 0.01f64..8.0,
            port in any::<u16>(),
            user in image_user(),
            volumes in proptest::collection::vec(volume(), 0..20),
            env in env_map(),
            labels in proptest::collection::hash_map(prop_oneof![
                "[a-z.]{1,12}",
                Just("chelabase.dev/endpoint-id".to_string()),
                Just("Chelabase.Dev/stack".to_string()),
                Just("com.docker.compose.service".to_string()),
            ], ".{0,8}", 0..4),
        ) {
            let caps = Caps { memory_mb: cap_memory, cpus: cap_cpus };
            let spec = ContainerSpec {
                backend: "container".into(),
                image: "ghcr.io/acme/echo:1".into(),
                port,
                health_path: "/".into(),
                resources: Resources { memory_mb: memory, cpus },
            };
            if let Ok(v) = validate(&spec, &caps) {
                prop_assert!(memory.is_none_or(|m| m <= cap_memory));
                prop_assert!(cpus.is_none_or(|c| c <= cap_cpus));
                let reserved = labels.keys().any(|k| {
                    let k = k.to_ascii_lowercase();
                    k.starts_with("chelabase.dev/") || k.starts_with("com.docker.compose.")
                });
                let facts = ImageFacts { id: "sha256:x".into(), user, volumes, labels };
                if let Ok(p) = plan("feat-x", &key(), &v, &facts, &env, "d") {
                    prop_assert!(!reserved, "an image with a reserved label is refused");
                    assert_hardened(&create_body(&p, NET), &caps);
                }
            }
            if memory.is_some_and(|m| m > cap_memory) || cpus.is_some_and(|c| c > cap_cpus) {
                prop_assert!(validate(&spec, &caps).is_err());
            }
        }

        /// Any extra key, at the top level or in `resources`, is refused.
        #[test]
        fn arbitrary_extra_keys_are_refused(key in "[a-z_]{1,16}", nested in any::<bool>()) {
            let known = ["backend", "image", "port", "health_path", "resources"];
            let known_res = ["memory_mb", "cpus"];
            let mut s = json!({"backend": "container", "image": "busybox:1", "port": 80, "resources": {}});
            if nested {
                prop_assume!(!known_res.contains(&key.as_str()));
                s["resources"][&key] = json!(true);
            } else {
                prop_assume!(!known.contains(&key.as_str()));
                s[&key] = json!(true);
            }
            prop_assert!(crate::spec::parse(&s, &Caps::default()).is_err());
        }
    }
}
