//! The runner's operations, each behind the label guard: a container is the runner's only when
//! it carries this stack's [`STACK_LABEL`] and a [`SERVICE_LABEL`] that is a UUID. Anything else
//! (an unlabelled container such as `metadata-db`, a compute, another stack's service) is
//! [`RunnerError::NotFound`], whatever id or name the caller gives, and is never touched.
//!
//! Each service's container (one per service per branch) runs on its own private network,
//! labelled like the container, which only the stack's functions service joins: containers can't
//! reach each other, and the functions service listens on none of these networks (it binds
//! fixed addresses only), so traffic flows functions → container alone.
//!
//! Only [`Runner::ensure`] creates a container (the functions service calls it with the branch's
//! secrets); reconcile only starts, stops and removes. A container carries a keyed digest of its
//! environment ([`ENV_DIGEST_LABEL`]), and ensure replaces it when the digest differs (a secret
//! rotation). Ensure and reconcile are serialized per service and branch ([`Runner::lock`]).

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex};

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::backend::{Container, ContainerBackend, ContainerPlan, ImageFacts, Network};
use crate::spec::{self, Caps, SpecError, Validated};
use crate::{BRANCH_LABEL, DEPLOYMENT_LABEL, PORT_LABEL, SERVICE_LABEL, STACK_LABEL};

/// The keyed digest (HMAC-SHA256, hex) of a container's environment. Keyed, because labels are
/// visible to anyone who can list containers and a bare hash of a low-entropy secret could be
/// guessed.
pub const ENV_DIGEST_LABEL: &str = "chelabase.dev/env-digest";

/// Image label namespaces a member image may not carry: the engine copies image labels onto the
/// container, and every socket holder trusts these (computes, the runner's own, compose's).
pub const RESERVED_LABEL_PREFIXES: [&str; 2] = ["chelabase.dev/", "com.docker.compose."];

/// Who a container is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Key {
    pub service_id: Uuid,
    pub branch_id: Uuid,
    pub deployment_id: Uuid,
}

impl Key {
    /// The service and branch: one container (and one network) each.
    pub fn slot(&self) -> (Uuid, Uuid) {
        (self.service_id, self.branch_id)
    }
}

/// The uid and gid a container runs as when its image names root, nothing, or a name (a name
/// can map to uid 0 in the image's passwd); the gid, too, whenever the image's would be 0 or
/// unset.
pub const FALLBACK_USER: &str = "65532:65532";
const FALLBACK_GID: u32 = 65532;
/// Log lines the logs call returns.
pub const LOG_TAIL: usize = 200;
/// Image volumes a container may have (each a tmpfs).
pub const MAX_IMAGE_VOLUMES: usize = 16;

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("no such container")]
    NotFound,
    #[error(transparent)]
    Invalid(#[from] SpecError),
    #[error(transparent)]
    Backend(#[from] anyhow::Error),
}

/// `uid:gid` from the image's user when its uid is a non-zero number; the gid is the image's when
/// it is a non-zero number, else 65532. Anything else is [`FALLBACK_USER`].
pub fn run_as(image_user: Option<&str>) -> String {
    let user = image_user.unwrap_or("").trim();
    let (uid, gid) = match user.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (user, None),
    };
    match uid.parse::<u32>() {
        Ok(u) if u != 0 => match gid.map(str::parse::<u32>) {
            None | Some(Ok(0)) => format!("{u}:{FALLBACK_GID}"),
            Some(Ok(g)) => format!("{u}:{g}"),
            Some(Err(_)) => FALLBACK_USER.to_string(),
        },
        _ => FALLBACK_USER.to_string(),
    }
}

/// `chelabase-<stack>-svc-<service>-<branch>`: one container per service per branch, and its
/// network of the same name.
pub fn container_name(stack: &str, service_id: Uuid, branch_id: Uuid) -> String {
    format!(
        "chelabase-{stack}-svc-{}-{}",
        service_id.simple(),
        branch_id.simple()
    )
}

/// HMAC-SHA256 under `key` of the environment, each name and value length-prefixed (so no two
/// environments encode alike), hex.
pub fn env_digest(key: &[u8; 32], env: &BTreeMap<String, String>) -> String {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("any key length");
    for (k, v) in env {
        for part in [k.as_bytes(), v.as_bytes()] {
            mac.update(&(part.len() as u64).to_be_bytes());
            mac.update(part);
        }
    }
    hex::encode(mac.finalize().into_bytes())
}

/// The digest key derived from a secret the runner already holds (its functions caller's
/// bearer), so digests survive a restart without another file.
pub fn derive_digest_key(secret: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"chelabase service-runner env digest\0");
    h.update(secret.as_bytes());
    h.finalize().into()
}

/// The container the runner creates for `key`: from the inspected image id, as the image's user
/// unless it is root (or not a number), a tmpfs at `/tmp` and at each image volume (never an
/// anonymous host volume), the labels and the checked environment. Refused: an image volume that
/// isn't a plain absolute path, and an image label in a reserved namespace.
pub fn plan(
    stack: &str,
    key: &Key,
    spec: &Validated,
    image: &ImageFacts,
    env: &BTreeMap<String, String>,
    digest: &str,
) -> Result<ContainerPlan, SpecError> {
    let env = spec::environment(env, spec.port)?;
    let mut reserved: Vec<&String> = image
        .labels
        .keys()
        .filter(|k| {
            let k = k.to_ascii_lowercase();
            RESERVED_LABEL_PREFIXES.iter().any(|p| k.starts_with(p))
        })
        .collect();
    if !reserved.is_empty() {
        reserved.sort();
        return Err(SpecError(format!(
            "the image carries labels in a reserved namespace (chelabase.dev/, com.docker.compose.): {reserved:?}"
        )));
    }
    if image.id.is_empty() {
        return Err(SpecError("the image has no id".into()));
    }
    if image.volumes.len() > MAX_IMAGE_VOLUMES {
        return Err(SpecError(format!(
            "the image declares more than {MAX_IMAGE_VOLUMES} volumes"
        )));
    }
    let mut tmpfs = vec!["/tmp".to_string()];
    for v in &image.volumes {
        let path = v.trim_end_matches('/');
        let plain = path.starts_with('/')
            && path.len() <= 256
            && path
                .split('/')
                .skip(1)
                .all(|c| !c.is_empty() && c != "." && c != "..")
            && path
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b));
        if !plain {
            return Err(SpecError(format!(
                "the image's volume {v:?} isn't a plain absolute path"
            )));
        }
        if !tmpfs.iter().any(|t| t == path) {
            tmpfs.push(path.to_string());
        }
    }
    let mut labels = slot_labels(stack, key.service_id, key.branch_id);
    labels.insert(DEPLOYMENT_LABEL.to_string(), key.deployment_id.to_string());
    labels.insert(PORT_LABEL.to_string(), spec.port.to_string());
    labels.insert(ENV_DIGEST_LABEL.to_string(), digest.to_string());
    Ok(ContainerPlan {
        name: container_name(stack, key.service_id, key.branch_id),
        image: spec.image.clone(),
        image_id: image.id.clone(),
        user: run_as(image.user.as_deref()),
        env,
        labels,
        limits: spec.limits,
        tmpfs,
    })
}

/// The labels a service's container and its network share.
fn slot_labels(stack: &str, service_id: Uuid, branch_id: Uuid) -> HashMap<String, String> {
    HashMap::from([
        (STACK_LABEL.to_string(), stack.to_string()),
        (SERVICE_LABEL.to_string(), service_id.to_string()),
        (BRANCH_LABEL.to_string(), branch_id.to_string()),
    ])
}

/// Whether `labels` make a container (or network) this stack's service container (or network).
pub fn is_ours(stack: &str, labels: &HashMap<String, String>) -> bool {
    labels.get(STACK_LABEL).map(String::as_str) == Some(stack)
        && labels
            .get(SERVICE_LABEL)
            .is_some_and(|s| Uuid::parse_str(s).is_ok())
}

/// A container's key, from its labels.
pub fn key_of(c: &Container) -> Option<Key> {
    let id = |l| c.label(l).and_then(|v| Uuid::parse_str(v).ok());
    Some(Key {
        service_id: id(SERVICE_LABEL)?,
        branch_id: id(BRANCH_LABEL)?,
        deployment_id: id(DEPLOYMENT_LABEL)?,
    })
}

/// A network's service and branch, from its labels.
pub fn slot_of(n: &Network) -> Option<(Uuid, Uuid)> {
    let id = |l| n.labels.get(l).and_then(|v| Uuid::parse_str(v).ok());
    Some((id(SERVICE_LABEL)?, id(BRANCH_LABEL)?))
}

/// What a caller may name: an engine id or a container name, nothing that could change the
/// Engine API path it lands in.
fn valid_ref(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && id.starts_with(|c: char| c.is_ascii_alphanumeric())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

/// What ensure takes.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnsureRequest {
    pub service_id: Uuid,
    pub branch_id: Uuid,
    pub deployment_id: Uuid,
    pub spec: serde_json::Value,
    /// The branch's secrets, passed per call and never stored by the runner.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Names only: the values are the branch's secrets.
impl fmt::Debug for EnsureRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EnsureRequest")
            .field("service_id", &self.service_id)
            .field("branch_id", &self.branch_id)
            .field("deployment_id", &self.deployment_id)
            .field("spec", &self.spec)
            .field(
                "env",
                &self
                    .env
                    .keys()
                    .map(|k| (k, "<redacted>"))
                    .collect::<BTreeMap<_, _>>(),
            )
            .finish()
    }
}

/// What status and ensure report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Status {
    pub id: String,
    pub state: String,
    pub service_id: Option<String>,
    pub branch_id: Option<String>,
    pub deployment_id: Option<String>,
    pub port: Option<u16>,
}

impl From<&Container> for Status {
    fn from(c: &Container) -> Self {
        Status {
            id: c.id.clone(),
            state: c.state.clone(),
            service_id: c.label(SERVICE_LABEL).map(str::to_string),
            branch_id: c.label(BRANCH_LABEL).map(str::to_string),
            deployment_id: c.label(DEPLOYMENT_LABEL).map(str::to_string),
            port: c.label(PORT_LABEL).and_then(|p| p.parse().ok()),
        }
    }
}

/// Resolves a registry host (the engine pulls from the host's network namespace).
#[async_trait::async_trait]
pub trait Resolve: Send + Sync + 'static {
    async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<std::net::IpAddr>>;
}

/// The system resolver.
pub struct SystemResolver;

#[async_trait::async_trait]
impl Resolve for SystemResolver {
    async fn resolve(&self, host: &str, port: u16) -> std::io::Result<Vec<std::net::IpAddr>> {
        Ok(tokio::net::lookup_host((host, port))
            .await?
            .map(|a| a.ip())
            .collect())
    }
}

/// The per-(service, branch) locks ensure holds and reconcile tries.
type Slots = Mutex<HashMap<(Uuid, Uuid), Arc<tokio::sync::Mutex<()>>>>;

pub struct Runner {
    backend: Arc<dyn ContainerBackend>,
    stack: String,
    project: String,
    caps: Caps,
    digest_key: [u8; 32],
    slots: Slots,
    resolver: Arc<dyn Resolve>,
}

impl Runner {
    /// `project` is the compose project whose `functions` service joins each service network.
    /// The digest key starts random; [`Runner::with_digest_key`] sets a stable one.
    pub fn new(backend: Arc<dyn ContainerBackend>, stack: &str, project: &str, caps: Caps) -> Self {
        let mut digest_key = [0u8; 32];
        getrandom::fill(&mut digest_key).expect("OS random number generator");
        Runner {
            backend,
            stack: stack.to_string(),
            project: project.to_string(),
            caps,
            digest_key,
            slots: Mutex::new(HashMap::new()),
            // Unit tests never reach real DNS: every host resolves to a public address there.
            #[cfg(not(test))]
            resolver: Arc::new(SystemResolver),
            #[cfg(test)]
            resolver: Arc::new(fake::PublicResolver),
        }
    }

    pub fn with_resolver(mut self, resolver: Arc<dyn Resolve>) -> Self {
        self.resolver = resolver;
        self
    }

    pub fn with_digest_key(mut self, key: [u8; 32]) -> Self {
        self.digest_key = key;
        self
    }

    pub fn stack(&self) -> &str {
        &self.stack
    }

    pub fn caps(&self) -> &Caps {
        &self.caps
    }

    fn slot_lock(&self, slot: (Uuid, Uuid)) -> Arc<tokio::sync::Mutex<()>> {
        let mut slots = self.slots.lock().expect("slots lock");
        // Drop idle entries now and then so the map doesn't grow with every service ever seen.
        if slots.len() > 1024 {
            slots.retain(|_, l| Arc::strong_count(l) > 1);
        }
        slots.entry(slot).or_default().clone()
    }

    /// Waits for the (service, branch) lock: ensure holds it for its whole call.
    pub async fn lock(&self, slot: (Uuid, Uuid)) -> tokio::sync::OwnedMutexGuard<()> {
        self.slot_lock(slot).lock_owned().await
    }

    /// The (service, branch) lock if nothing holds it: reconcile skips a slot otherwise.
    pub fn try_lock(&self, slot: (Uuid, Uuid)) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        self.slot_lock(slot).try_lock_owned().ok()
    }

    /// The container `id` names, only when it is ours. Every later call uses the engine's full
    /// id from this inspect, never the caller's string.
    pub async fn guarded(&self, id: &str) -> Result<Container, RunnerError> {
        if !valid_ref(id) {
            return Err(RunnerError::NotFound);
        }
        match self.backend.inspect(id).await? {
            Some(c) if is_ours(&self.stack, &c.labels) && !c.id.is_empty() => Ok(c),
            _ => Err(RunnerError::NotFound),
        }
    }

    /// Every container of this stack's services (labels checked here again).
    pub async fn ours(&self) -> Result<Vec<Container>, RunnerError> {
        Ok(self
            .backend
            .list(&self.stack)
            .await?
            .into_iter()
            .filter(|c| is_ours(&self.stack, &c.labels))
            .collect())
    }

    /// Every network of this stack's services (labels checked here again).
    pub async fn our_networks(&self) -> Result<Vec<Network>, RunnerError> {
        Ok(self
            .backend
            .list_networks(&self.stack)
            .await?
            .into_iter()
            .filter(|n| is_ours(&self.stack, &n.labels) && slot_of(n).is_some())
            .collect())
    }

    /// The service's network, created if missing, with the functions service attached. A
    /// network of that name that isn't this slot's is refused, never used.
    async fn service_network(
        &self,
        service_id: Uuid,
        branch_id: Uuid,
    ) -> Result<String, RunnerError> {
        let name = container_name(&self.stack, service_id, branch_id);
        let id = match self.backend.inspect_network(&name).await? {
            Some(n)
                if is_ours(&self.stack, &n.labels)
                    && slot_of(&n) == Some((service_id, branch_id)) =>
            {
                n.id
            }
            Some(_) => {
                return Err(RunnerError::Backend(anyhow::anyhow!(
                    "network {name} exists and isn't this service's"
                )));
            }
            None => {
                self.backend
                    .create_network(&name, &slot_labels(&self.stack, service_id, branch_id))
                    .await?
            }
        };
        self.attach_functions(&id).await?;
        Ok(name)
    }

    /// The stack's functions service containers.
    pub async fn functions(&self) -> Result<Vec<String>, RunnerError> {
        Ok(self.backend.functions_containers(&self.project).await?)
    }

    /// Attaches the stack's functions service to `network`.
    pub async fn attach_functions(&self, network: &str) -> Result<(), RunnerError> {
        let functions = self.backend.functions_containers(&self.project).await?;
        if functions.is_empty() {
            tracing::warn!(
                "no functions container in compose project {}: service network {network} is unreachable until it runs",
                self.project
            );
        }
        for f in functions {
            self.backend.connect(network, &f).await?;
        }
        Ok(())
    }

    /// Detaches everything from the slot's network and removes it (gone already: done). Checked
    /// again here: while one of the slot's containers exists, the network stays.
    pub async fn remove_network(
        &self,
        service_id: Uuid,
        branch_id: Uuid,
    ) -> Result<(), RunnerError> {
        let in_use = self
            .ours()
            .await?
            .iter()
            .any(|c| key_of(c).is_some_and(|k| k.slot() == (service_id, branch_id)));
        if in_use {
            return Ok(());
        }
        let name = container_name(&self.stack, service_id, branch_id);
        let Some(n) = self.backend.inspect_network(&name).await? else {
            return Ok(());
        };
        if !is_ours(&self.stack, &n.labels) || slot_of(&n) != Some((service_id, branch_id)) {
            return Ok(());
        }
        for c in &n.containers {
            self.backend.disconnect(&n.id, c).await?;
        }
        self.backend.remove_network(&n.id).await?;
        Ok(())
    }

    /// A running container for the request's service on its branch, at its deployment, with its
    /// environment. The spec, env and image are checked before anything else happens (refused:
    /// nothing is created or removed). A container at another deployment or with another env is
    /// replaced; a matching one is started if it was stopped and otherwise kept as it is.
    pub async fn ensure(&self, req: &EnsureRequest) -> Result<Container, RunnerError> {
        let spec = spec::parse(&req.spec, &self.caps)?;
        spec::environment(&req.env, spec.port)?;
        self.check_registry(&spec.image).await?;
        let key = Key {
            service_id: req.service_id,
            branch_id: req.branch_id,
            deployment_id: req.deployment_id,
        };
        let digest = env_digest(&self.digest_key, &req.env);
        let _slot = self.lock(key.slot()).await;
        let (mut keep, mut stale) = (None, Vec::new());
        for c in self.ours().await? {
            let Some(k) = key_of(&c) else { continue };
            if k.slot() != key.slot() {
                continue;
            }
            if k == key && c.label(ENV_DIGEST_LABEL) == Some(digest.as_str()) && keep.is_none() {
                keep = Some(c);
            } else {
                stale.push(c.id);
            }
        }
        if let Some(c) = keep {
            for id in &stale {
                self.backend.remove(id).await?;
            }
            self.service_network(key.service_id, key.branch_id).await?;
            if !c.running() {
                self.backend.start(&c.id).await?;
            }
            return self.guarded(&c.id).await;
        }
        // Everything that can refuse comes before the old container goes.
        let facts = self.backend.image(&spec.image).await?;
        let plan = plan(&self.stack, &key, &spec, &facts, &req.env, &digest)?;
        for id in &stale {
            self.backend.remove(id).await?;
        }
        let network = self.service_network(key.service_id, key.branch_id).await?;
        let id = self.backend.create(&plan, &network).await?;
        self.backend.start(&id).await?;
        self.guarded(&id).await
    }

    /// The registry host's every address must be public: the engine pulls from the host's
    /// network namespace, and treats a registry on a loopback address as plain HTTP. This
    /// narrows pull-side SSRF but can't close it: a registry on a public address can still send
    /// the engine anywhere through its token `realm` (an auth redirect the runner never sees),
    /// and a name can resolve differently when the engine looks it up. A host that doesn't
    /// resolve is refused too.
    async fn check_registry(&self, image: &spec::ImageRef) -> Result<(), RunnerError> {
        let Some((host, port)) = spec::registry_host(image) else {
            return Ok(());
        };
        let refuse = |why: String| Err(RunnerError::Invalid(SpecError(why)));
        match self.resolver.resolve(&host, port).await {
            Ok(addrs) if addrs.is_empty() => refuse(format!("registry {host} has no address")),
            Ok(addrs) => match addrs
                .iter()
                .find(|a| !chelabase_http::outbound::is_public(**a))
            {
                Some(a) => refuse(format!(
                    "registry {host} resolves to a non-public address ({a})"
                )),
                None => Ok(()),
            },
            Err(e) => refuse(format!("registry {host} doesn't resolve: {e}")),
        }
    }

    /// Runs `op` on the container `id` names under its slot's lock (ensure's), checking it again
    /// once the lock is held: an ensure that finished in between may have replaced it.
    async fn locked<T>(
        &self,
        id: &str,
        op: impl AsyncFnOnce(&Container) -> Result<T, RunnerError>,
    ) -> Result<T, RunnerError> {
        let c = self.guarded(id).await?;
        let _slot = match key_of(&c) {
            Some(k) => Some(self.lock(k.slot()).await),
            None => None,
        };
        let c = self.guarded(&c.id).await?;
        op(&c).await
    }

    pub async fn start(&self, id: &str) -> Result<Status, RunnerError> {
        self.locked(id, async |c| self.start_unlocked(&c.id).await)
            .await
    }

    pub async fn stop(&self, id: &str) -> Result<Status, RunnerError> {
        self.locked(id, async |c| self.stop_unlocked(&c.id).await)
            .await
    }

    /// Removes the container and then its service's network.
    pub async fn remove(&self, id: &str) -> Result<(), RunnerError> {
        self.locked(id, async |c| self.remove_unlocked(&c.id).await)
            .await
    }

    /// [`Runner::start`] for a caller already holding the slot's lock (reconcile).
    pub async fn start_unlocked(&self, id: &str) -> Result<Status, RunnerError> {
        let c = self.guarded(id).await?;
        self.backend.start(&c.id).await?;
        Ok(Status::from(&self.guarded(&c.id).await?))
    }

    /// [`Runner::stop`] for a caller already holding the slot's lock (reconcile).
    pub async fn stop_unlocked(&self, id: &str) -> Result<Status, RunnerError> {
        let c = self.guarded(id).await?;
        self.backend.stop(&c.id).await?;
        Ok(Status::from(&self.guarded(&c.id).await?))
    }

    /// [`Runner::remove`] for a caller already holding the slot's lock (reconcile).
    pub async fn remove_unlocked(&self, id: &str) -> Result<(), RunnerError> {
        let c = self.guarded(id).await?;
        self.backend.remove(&c.id).await?;
        // The network goes with the slot's last container.
        if let Some(k) = key_of(&c) {
            self.remove_network(k.service_id, k.branch_id).await?;
        }
        Ok(())
    }

    pub async fn status(&self, id: &str) -> Result<Status, RunnerError> {
        Ok(Status::from(&self.guarded(id).await?))
    }

    pub async fn logs(&self, id: &str) -> Result<String, RunnerError> {
        let c = self.guarded(id).await?;
        Ok(self.backend.logs(&c.id, LOG_TAIL).await?)
    }
}

/// An in-memory backend for the unit tests: it records every call that names a container or a
/// network.
#[cfg(test)]
pub mod fake {
    use std::sync::Mutex;

    use super::*;
    use crate::spec::ImageRef;

    #[derive(Default)]
    pub struct Fake {
        pub containers: Mutex<Vec<(Container, Option<ContainerPlan>)>>,
        pub networks: Mutex<Vec<Network>>,
        /// `(call, id)` for every start, stop, remove, create, logs and network change.
        pub calls: Mutex<Vec<(String, String)>>,
        pub image: Mutex<ImageFacts>,
        /// The compose project's functions containers.
        pub functions: Mutex<Vec<String>>,
        /// When set, `image` waits for a permit (to hold an ensure mid-call).
        pub image_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
        next: Mutex<u32>,
    }

    /// Every host is a public address.
    pub struct PublicResolver;

    #[async_trait::async_trait]
    impl Resolve for PublicResolver {
        async fn resolve(&self, _: &str, _: u16) -> std::io::Result<Vec<std::net::IpAddr>> {
            Ok(vec!["93.184.216.34".parse().unwrap()])
        }
    }

    /// Fixed answers by host; anything else fails to resolve.
    pub struct MapResolver(pub HashMap<String, Vec<std::net::IpAddr>>);

    #[async_trait::async_trait]
    impl Resolve for MapResolver {
        async fn resolve(&self, host: &str, _: u16) -> std::io::Result<Vec<std::net::IpAddr>> {
            self.0
                .get(host)
                .cloned()
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no such host"))
        }
    }

    impl Fake {
        fn next_id(&self) -> String {
            let mut n = self.next.lock().unwrap();
            *n += 1;
            format!("{:064x}", *n)
        }

        /// Adds a container with `labels` (as if made by someone else) and returns its id.
        pub fn add(&self, name: &str, labels: &[(&str, &str)], state: &str) -> String {
            let id = self.next_id();
            let mut l: HashMap<String, String> = labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            l.insert("name".into(), name.into());
            self.containers.lock().unwrap().push((
                Container {
                    id: id.clone(),
                    labels: l,
                    state: state.into(),
                },
                None,
            ));
            id
        }

        pub fn get(&self, id: &str) -> Option<Container> {
            self.containers
                .lock()
                .unwrap()
                .iter()
                .find(|(c, _)| c.id == id || c.label("name") == Some(id))
                .map(|(c, _)| c.clone())
        }

        pub fn network(&self, name_or_id: &str) -> Option<Network> {
            self.networks
                .lock()
                .unwrap()
                .iter()
                .find(|n| n.id == name_or_id || n.name == name_or_id)
                .cloned()
        }

        pub fn plans(&self) -> Vec<ContainerPlan> {
            self.containers
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(_, p)| p.clone())
                .collect()
        }

        pub fn calls(&self) -> Vec<(String, String)> {
            self.calls.lock().unwrap().clone()
        }

        fn record(&self, call: &str, id: &str) {
            self.calls.lock().unwrap().push((call.into(), id.into()));
        }

        fn set_state(&self, id: &str, state: &str) {
            for (c, _) in self.containers.lock().unwrap().iter_mut() {
                if c.id == id {
                    c.state = state.into();
                }
            }
        }
    }

    #[async_trait::async_trait]
    impl ContainerBackend for Fake {
        async fn inspect(&self, id: &str) -> anyhow::Result<Option<Container>> {
            Ok(self.get(id))
        }
        async fn list(&self, _stack: &str) -> anyhow::Result<Vec<Container>> {
            // Deliberately loose (every container, any stack, labelled or not): the runner's
            // own label check is what keeps it to its containers.
            Ok(self
                .containers
                .lock()
                .unwrap()
                .iter()
                .map(|(c, _)| c.clone())
                .collect())
        }
        async fn image(&self, image: &ImageRef) -> anyhow::Result<ImageFacts> {
            let gate = self.image_gate.lock().unwrap().clone();
            if let Some(gate) = gate {
                gate.acquire().await.unwrap().forget();
            }
            let mut facts = self.image.lock().unwrap().clone();
            if facts.id.is_empty() {
                facts.id = format!("sha256:{:064x}", image.to_string().len());
            }
            Ok(facts)
        }
        async fn create(&self, plan: &ContainerPlan, network: &str) -> anyhow::Result<String> {
            anyhow::ensure!(self.network(network).is_some(), "no network {network}");
            anyhow::ensure!(self.get(&plan.name).is_none(), "name {} taken", plan.name);
            let labels: Vec<(&str, &str)> = plan
                .labels
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let id = self.add(&plan.name, &labels, "created");
            self.containers.lock().unwrap().last_mut().unwrap().1 = Some(plan.clone());
            for n in self.networks.lock().unwrap().iter_mut() {
                if n.name == network {
                    n.containers.push(id.clone());
                }
            }
            self.record("create", &id);
            Ok(id)
        }
        async fn start(&self, id: &str) -> anyhow::Result<()> {
            self.record("start", id);
            self.set_state(id, "running");
            Ok(())
        }
        async fn stop(&self, id: &str) -> anyhow::Result<()> {
            self.record("stop", id);
            self.set_state(id, "exited");
            Ok(())
        }
        async fn remove(&self, id: &str) -> anyhow::Result<()> {
            self.record("remove", id);
            self.containers.lock().unwrap().retain(|(c, _)| c.id != id);
            for n in self.networks.lock().unwrap().iter_mut() {
                n.containers.retain(|c| c != id);
            }
            Ok(())
        }
        async fn logs(&self, id: &str, tail: usize) -> anyhow::Result<String> {
            self.record("logs", id);
            Ok(format!("last {tail} lines"))
        }
        async fn list_networks(&self, _stack: &str) -> anyhow::Result<Vec<Network>> {
            Ok(self.networks.lock().unwrap().clone())
        }
        async fn inspect_network(&self, name: &str) -> anyhow::Result<Option<Network>> {
            Ok(self.network(name))
        }
        async fn create_network(
            &self,
            name: &str,
            labels: &HashMap<String, String>,
        ) -> anyhow::Result<String> {
            anyhow::ensure!(self.network(name).is_none(), "network {name} taken");
            let id = self.next_id();
            self.networks.lock().unwrap().push(Network {
                id: id.clone(),
                name: name.into(),
                labels: labels.clone(),
                containers: vec![],
            });
            self.record("create_network", &id);
            Ok(id)
        }
        async fn remove_network(&self, id: &str) -> anyhow::Result<()> {
            let n = self.network(id).expect("a network");
            anyhow::ensure!(n.containers.is_empty(), "network {id} has endpoints");
            self.record("remove_network", id);
            self.networks.lock().unwrap().retain(|n| n.id != id);
            Ok(())
        }
        async fn connect(&self, network: &str, container: &str) -> anyhow::Result<()> {
            let mut nets = self.networks.lock().unwrap();
            let n = nets
                .iter_mut()
                .find(|n| n.id == network || n.name == network)
                .expect("a network");
            if !n.containers.iter().any(|c| c == container) {
                n.containers.push(container.into());
                self.calls
                    .lock()
                    .unwrap()
                    .push(("connect".into(), format!("{}:{container}", n.name)));
            }
            Ok(())
        }
        async fn disconnect(&self, network: &str, container: &str) -> anyhow::Result<()> {
            let mut nets = self.networks.lock().unwrap();
            if let Some(n) = nets
                .iter_mut()
                .find(|n| n.id == network || n.name == network)
            {
                n.containers.retain(|c| c != container);
            }
            Ok(())
        }
        async fn functions_containers(&self, _project: &str) -> anyhow::Result<Vec<String>> {
            Ok(self.functions.lock().unwrap().clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::fake::Fake;
    use super::*;

    pub(crate) fn runner(fake: &Arc<Fake>, stack: &str) -> Runner {
        Runner::new(fake.clone(), stack, "net", Caps::default())
    }

    fn request(key: &Key) -> EnsureRequest {
        EnsureRequest {
            service_id: key.service_id,
            branch_id: key.branch_id,
            deployment_id: key.deployment_id,
            spec: json!({"backend": "container", "image": "ghcr.io/acme/echo:1", "port": 8080}),
            env: [("SECRET".to_string(), "s3cret".to_string())].into(),
        }
    }

    fn new_key() -> Key {
        Key {
            service_id: Uuid::new_v4(),
            branch_id: Uuid::new_v4(),
            deployment_id: Uuid::new_v4(),
        }
    }

    #[test]
    fn image_users() {
        for (image, want) in [
            (None, FALLBACK_USER),
            (Some("root"), FALLBACK_USER),
            (Some("0"), FALLBACK_USER),
            (Some("0:0"), FALLBACK_USER),
            (Some("000"), FALLBACK_USER),
            (Some("app"), FALLBACK_USER),
            (Some("1000:app"), FALLBACK_USER),
            (Some("-1"), FALLBACK_USER),
            (Some("1000"), "1000:65532"),
            (Some("1000:0"), "1000:65532"),
            (Some("1000:100"), "1000:100"),
            (Some("65532:65532"), "65532:65532"),
        ] {
            assert_eq!(run_as(image), want, "{image:?}");
        }
    }

    /// The label guard, on every operation that names a container: an unlabelled one (like
    /// `metadata-db`), a compute (stack label only), another stack's service and one whose
    /// service label isn't a UUID are all "not found", and nothing is called on them.
    #[tokio::test]
    async fn every_operation_is_label_guarded() {
        let fake = Arc::new(Fake::default());
        let svc = Uuid::new_v4().to_string();
        let foreign = [
            fake.add(
                "metadata-db",
                &[("com.docker.compose.service", "metadata-db")],
                "running",
            ),
            fake.add(
                "compute",
                &[(STACK_LABEL, "a"), ("chelabase.dev/endpoint-id", "ep-1")],
                "running",
            ),
            fake.add(
                "other-stack",
                &[(STACK_LABEL, "b"), (SERVICE_LABEL, &svc)],
                "running",
            ),
            fake.add(
                "bad-service",
                &[(STACK_LABEL, "a"), (SERVICE_LABEL, "x")],
                "running",
            ),
            fake.add("no-stack", &[(SERVICE_LABEL, &svc)], "running"),
        ];
        let r = runner(&fake, "a");
        for id in foreign.iter().map(String::as_str).chain([
            "metadata-db",
            "other-stack",
            "../x",
            "",
            "a/b",
        ]) {
            assert!(
                matches!(r.start(id).await, Err(RunnerError::NotFound)),
                "{id}"
            );
            assert!(
                matches!(r.stop(id).await, Err(RunnerError::NotFound)),
                "{id}"
            );
            assert!(
                matches!(r.remove(id).await, Err(RunnerError::NotFound)),
                "{id}"
            );
            assert!(
                matches!(r.status(id).await, Err(RunnerError::NotFound)),
                "{id}"
            );
            assert!(
                matches!(r.logs(id).await, Err(RunnerError::NotFound)),
                "{id}"
            );
        }
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        for id in &foreign {
            assert_eq!(fake.get(id).unwrap().state, "running");
        }
        assert!(r.ours().await.unwrap().is_empty());
    }

    /// The runner's own containers can be driven by id or by name; calls use the full id.
    #[tokio::test]
    async fn own_containers_are_managed() {
        let fake = Arc::new(Fake::default());
        let svc = Uuid::new_v4().to_string();
        let id = fake.add(
            "mine",
            &[(STACK_LABEL, "a"), (SERVICE_LABEL, &svc)],
            "running",
        );
        let r = runner(&fake, "a");
        assert_eq!(r.stop("mine").await.unwrap().state, "exited");
        assert_eq!(r.start(&id).await.unwrap().state, "running");
        assert_eq!(r.logs(&id).await.unwrap(), "last 200 lines");
        r.remove("mine").await.unwrap();
        assert_eq!(
            fake.calls(),
            [
                ("stop", &id),
                ("start", &id),
                ("logs", &id),
                ("remove", &id)
            ]
            .map(|(c, i)| (c.to_string(), i.to_string()))
        );
        assert!(fake.get(&id).is_none());
    }

    #[tokio::test]
    async fn ensure_creates_a_hardened_labelled_container() {
        let fake = Arc::new(Fake::default());
        let key = new_key();
        let digest_key = derive_digest_key("functions-bearer");
        let r = runner(&fake, "a").with_digest_key(digest_key);
        let c = r.ensure(&request(&key)).await.unwrap();
        assert!(c.running());
        assert_eq!(key_of(&c), Some(key));
        let plans = fake.plans();
        assert_eq!(plans.len(), 1);
        let p = &plans[0];
        assert_eq!(p.name, container_name("a", key.service_id, key.branch_id));
        assert_eq!(p.user, FALLBACK_USER);
        assert_eq!(p.env, ["SECRET=s3cret", "PORT=8080"]);
        assert_eq!(p.labels[STACK_LABEL], "a");
        assert_eq!(p.labels[PORT_LABEL], "8080");
        assert_eq!(
            p.labels[ENV_DIGEST_LABEL],
            env_digest(&digest_key, &request(&key).env)
        );
        assert!(p.labels.values().all(|v| !v.contains("s3cret")));
        // Again, even from a restarted runner with the same derived key: kept, nothing new.
        let again = runner(&fake, "a")
            .with_digest_key(digest_key)
            .ensure(&request(&key))
            .await
            .unwrap();
        assert_eq!(again.id, c.id);
        assert_eq!(fake.plans().len(), 1);
    }

    #[tokio::test]
    async fn ensure_replaces_another_deployment_and_starts_a_stopped_one() {
        let fake = Arc::new(Fake::default());
        let r = runner(&fake, "a");
        let key = new_key();
        let first = r.ensure(&request(&key)).await.unwrap();
        r.stop(&first.id).await.unwrap();
        assert!(r.ensure(&request(&key)).await.unwrap().running());
        let next = Key {
            deployment_id: Uuid::new_v4(),
            ..key
        };
        let second = r.ensure(&request(&next)).await.unwrap();
        assert_ne!(second.id, first.id);
        assert!(
            fake.get(&first.id).is_none(),
            "the old deployment's container is gone"
        );
        assert_eq!(r.ours().await.unwrap().len(), 1);
    }

    /// Audit B3: each service's container gets its own labelled network, created first, with the
    /// functions service attached; removing the slot's last container removes the network.
    #[tokio::test]
    async fn each_service_gets_its_own_network_with_the_functions_service() {
        let fake = Arc::new(Fake::default());
        fake.functions.lock().unwrap().push("functions-ctr".into());
        let r = runner(&fake, "a");
        let (k1, k2) = (new_key(), new_key());
        let c1 = r.ensure(&request(&k1)).await.unwrap();
        let c2 = r.ensure(&request(&k2)).await.unwrap();
        let n1 = fake
            .network(&container_name("a", k1.service_id, k1.branch_id))
            .unwrap();
        let n2 = fake
            .network(&container_name("a", k2.service_id, k2.branch_id))
            .unwrap();
        assert_ne!(n1.id, n2.id);
        assert_eq!(n1.containers, ["functions-ctr".to_string(), c1.id.clone()]);
        assert_eq!(n2.containers, ["functions-ctr".to_string(), c2.id.clone()]);
        assert_eq!(n1.labels[STACK_LABEL], "a");
        assert_eq!(n1.labels[SERVICE_LABEL], k1.service_id.to_string());
        assert_eq!(n1.labels[BRANCH_LABEL], k1.branch_id.to_string());
        // A new deployment in the same slot reuses the slot's network.
        let next = Key {
            deployment_id: Uuid::new_v4(),
            ..k1
        };
        let c1b = r.ensure(&request(&next)).await.unwrap();
        assert_eq!(
            fake.network(&n1.id).unwrap().containers,
            ["functions-ctr".to_string(), c1b.id.clone()]
        );
        r.remove(&c1b.id).await.unwrap();
        assert!(
            fake.network(&n1.id).is_none(),
            "the network goes with its last container"
        );
        assert!(fake.network(&n2.id).is_some());
        // A foreign network under the slot's name is never used.
        let k3 = new_key();
        let name = container_name("a", k3.service_id, k3.branch_id);
        fake.networks.lock().unwrap().push(Network {
            id: "foreign".into(),
            name,
            labels: HashMap::new(),
            containers: vec![],
        });
        assert!(matches!(
            r.ensure(&request(&k3)).await,
            Err(RunnerError::Backend(_))
        ));
        assert!(fake.network("foreign").unwrap().containers.is_empty());
    }

    /// Re-audit S2: a registry host that resolves to any loopback, link-local, private, CGNAT,
    /// unspecified or multicast address (or doesn't resolve) is refused with nothing touched,
    /// including the auditor's four DNS names.
    #[tokio::test]
    async fn registries_resolving_to_local_addresses_are_refused() {
        let ip = |s: &str| -> std::net::IpAddr { s.parse().unwrap() };
        let answers: HashMap<String, Vec<std::net::IpAddr>> = [
            ("127.0.0.1.nip.io", vec![ip("127.0.0.1")]),
            ("localtest.me", vec![ip("127.0.0.1"), ip("::1")]),
            ("169.254.169.254.nip.io", vec![ip("169.254.169.254")]),
            ("0x7f000001.nip.io", vec![ip("127.0.0.1")]),
            ("ten.example", vec![ip("10.1.2.3")]),
            ("cgnat.example", vec![ip("100.64.0.9")]),
            ("ula.example", vec![ip("fd00::1")]),
            ("mapped.example", vec![ip("::ffff:127.0.0.1")]),
            ("multicast.example", vec![ip("239.1.1.1")]),
            ("zero.example", vec![ip("0.0.0.0")]),
            (
                "mixed.example",
                vec![ip("93.184.216.34"), ip("192.168.1.1")],
            ),
            ("empty.example", vec![]),
            ("ghcr.io", vec![ip("140.82.112.33")]),
            (
                "registry.example.com",
                vec![ip("93.184.216.34"), ip("2606:2800:220:1::1")],
            ),
        ]
        .into_iter()
        .map(|(h, a)| (h.to_string(), a))
        .collect();
        let fake = Arc::new(Fake::default());
        let r = runner(&fake, "a").with_resolver(Arc::new(fake::MapResolver(answers)));
        for image in [
            "127.0.0.1.nip.io:8100/echo:1",
            "localtest.me:4500/echo:1",
            "169.254.169.254.nip.io/echo:1",
            "0x7f000001.nip.io/echo:1",
            "ten.example/echo:1",
            "cgnat.example/echo:1",
            "ula.example/echo:1",
            "mapped.example/echo:1",
            "multicast.example/echo:1",
            "zero.example/echo:1",
            "mixed.example/echo:1",
            "empty.example/echo:1",
            "nxdomain.example/echo:1",
        ] {
            let mut req = request(&new_key());
            req.spec["image"] = json!(image);
            assert!(
                matches!(r.ensure(&req).await, Err(RunnerError::Invalid(_))),
                "{image}"
            );
        }
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        for image in [
            "ghcr.io/acme/echo:1",
            "registry.example.com:5000/a/b:1",
            "busybox:1",
            "acme/echo:1",
        ] {
            let mut req = request(&new_key());
            req.spec["image"] = json!(image);
            assert!(r.ensure(&req).await.is_ok(), "{image}");
        }
    }

    /// Re-audit N2: start, stop and remove wait for the slot's lock, like ensure.
    #[tokio::test]
    async fn lifecycle_calls_wait_for_the_slot() {
        let fake = Arc::new(Fake::default());
        let r = Arc::new(runner(&fake, "a"));
        let key = new_key();
        let c = r.ensure(&request(&key)).await.unwrap();
        let held = r.lock(key.slot()).await;
        let (r2, id) = (r.clone(), c.id.clone());
        let stop = tokio::spawn(async move { r2.stop(&id).await.map(|s| s.state) });
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            fake.get(&c.id).unwrap().state,
            "running",
            "stop must wait for the slot"
        );
        drop(held);
        assert_eq!(stop.await.unwrap().unwrap(), "exited");
    }

    /// Re-audit N2: a remove racing an ensure on the same slot waits for it. The ensure (a new
    /// env, so a replacement) is held mid-call; the old container stays until it finishes, and
    /// afterwards the remove finds it already replaced (404) and leaves the new container and its
    /// network, with the functions service attached, alone.
    #[tokio::test]
    async fn a_remove_racing_an_ensure_waits_for_it() {
        let fake = Arc::new(Fake::default());
        fake.functions.lock().unwrap().push("functions-ctr".into());
        let r = Arc::new(runner(&fake, "a"));
        let key = new_key();
        let old = r.ensure(&request(&key)).await.unwrap();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        *fake.image_gate.lock().unwrap() = Some(gate.clone());
        let mut rotated = request(&key);
        rotated.env.insert("SECRET".into(), "rotated".into());
        let r1 = r.clone();
        let ensure = tokio::spawn(async move { r1.ensure(&rotated).await.map(|c| c.id) });
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        let (r2, old_id) = (r.clone(), old.id.clone());
        let remove = tokio::spawn(async move { r2.remove(&old_id).await });
        for _ in 0..5 {
            tokio::task::yield_now().await;
        }
        assert!(
            fake.get(&old.id).is_some(),
            "the remove must wait for the ensure in flight"
        );
        gate.add_permits(1);
        let new_id = ensure.await.unwrap().unwrap();
        assert!(matches!(remove.await.unwrap(), Err(RunnerError::NotFound)));
        assert_eq!(fake.get(&new_id).unwrap().state, "running");
        let net = fake
            .network(&container_name("a", key.service_id, key.branch_id))
            .unwrap();
        assert!(net.containers.contains(&"functions-ctr".to_string()));
        assert!(net.containers.contains(&new_id));
    }

    /// Audit N2: the branch's secrets never reach a log through `Debug`.
    #[test]
    fn ensure_request_debug_hides_values() {
        let text = format!("{:?}", request(&new_key()));
        assert!(
            text.contains("SECRET") && !text.contains("s3cret"),
            "{text}"
        );
    }

    #[test]
    fn env_digests_are_keyed_and_unambiguous() {
        let env = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let k = derive_digest_key("functions-bearer");
        let d = env_digest(&k, &env(&[("A", "1")]));
        assert_eq!(d.len(), 64);
        assert_eq!(d, env_digest(&k, &env(&[("A", "1")])));
        assert_ne!(
            d,
            env_digest(&derive_digest_key("other"), &env(&[("A", "1")]))
        );
        assert_ne!(
            env_digest(&k, &env(&[("AB", "")])),
            env_digest(&k, &env(&[("A", "B")]))
        );
        assert_ne!(
            env_digest(&k, &env(&[])),
            env_digest(&k, &env(&[("A", "")]))
        );
    }

    /// A secret rotation: the same key with a new env replaces the container; the same env
    /// again keeps it.
    #[tokio::test]
    async fn ensure_with_a_new_env_replaces_the_container() {
        let fake = Arc::new(Fake::default());
        let r = runner(&fake, "a");
        let key = new_key();
        let first = r.ensure(&request(&key)).await.unwrap();
        assert_eq!(r.ensure(&request(&key)).await.unwrap().id, first.id);
        let mut rotated = request(&key);
        rotated.env.insert("SECRET".into(), "rotated".into());
        let second = r.ensure(&rotated).await.unwrap();
        assert_ne!(second.id, first.id);
        assert!(fake.get(&first.id).is_none());
        assert!(
            fake.plans()
                .last()
                .unwrap()
                .env
                .contains(&"SECRET=rotated".to_string())
        );
        assert_eq!(r.ours().await.unwrap().len(), 1);
    }

    /// A refused spec, env or image creates and removes nothing.
    #[tokio::test]
    async fn refused_ensure_touches_nothing() {
        let fake = Arc::new(Fake::default());
        let r = runner(&fake, "a");
        let key = new_key();
        let existing = r.ensure(&request(&key)).await.unwrap();
        fake.calls.lock().unwrap().clear();
        let other = Key {
            deployment_id: Uuid::new_v4(),
            ..key
        };
        for spec in [
            json!({"backend": "container", "image": "ghcr.io/acme/echo:1", "port": 8080, "privileged": true}),
            json!({"backend": "container", "image": "ghcr.io/acme/echo:1", "port": 8080, "binds": ["/:/h"]}),
            json!({"backend": "container", "image": "ghcr.io/acme/echo:1", "port": 8080, "network_mode": "host"}),
            json!({"backend": "container", "image": "ghcr.io/acme/echo:1", "port": 8080, "resources": {"memory_mb": 4096}}),
        ] {
            let mut req = request(&other);
            req.spec = spec;
            assert!(matches!(r.ensure(&req).await, Err(RunnerError::Invalid(_))));
        }
        let mut req = request(&other);
        req.env.insert("PORT".into(), "1".into());
        assert!(matches!(r.ensure(&req).await, Err(RunnerError::Invalid(_))));
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        assert!(fake.get(&existing.id).is_some());
        // An image volume that isn't a plain path is refused before create.
        *fake.image.lock().unwrap() = ImageFacts {
            volumes: vec!["/data/../../etc".into()],
            ..Default::default()
        };
        assert!(matches!(
            r.ensure(&request(&other)).await,
            Err(RunnerError::Invalid(_))
        ));
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        // Audit B2: an image label in a reserved namespace is refused before create.
        for label in [
            "chelabase.dev/endpoint-id",
            "com.docker.compose.service",
            "Chelabase.dev/role",
        ] {
            *fake.image.lock().unwrap() = ImageFacts {
                labels: HashMap::from([(label.to_string(), "x".to_string())]),
                ..Default::default()
            };
            assert!(
                matches!(
                    r.ensure(&request(&other)).await,
                    Err(RunnerError::Invalid(_))
                ),
                "{label}"
            );
        }
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        assert!(
            fake.get(&existing.id).is_some(),
            "the running deployment stays"
        );
    }
}
