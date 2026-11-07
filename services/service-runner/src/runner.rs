//! The runner's operations, each behind the label guard: a container is the runner's only when
//! it carries this stack's [`STACK_LABEL`] and a [`SERVICE_LABEL`] that is a UUID. Anything else
//! (an unlabelled container such as `metadata-db`, a compute, another stack's service) is
//! [`RunnerError::NotFound`], whatever id or name the caller gives, and is never touched.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::backend::{Container, ContainerBackend, ContainerPlan, ImageFacts};
use crate::spec::{self, Caps, SpecError, Validated};
use crate::{BRANCH_LABEL, DEPLOYMENT_LABEL, PORT_LABEL, SERVICE_LABEL, STACK_LABEL};

/// Who a container is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Key {
    pub service_id: Uuid,
    pub branch_id: Uuid,
    pub deployment_id: Uuid,
}

/// The user a container runs as when its image names root, nothing, or a name (a name can map
/// to uid 0 in the image's passwd).
pub const FALLBACK_USER: &str = "65532:65532";
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

/// The image's user when it is a non-zero numeric uid (with a numeric gid, if any); otherwise
/// [`FALLBACK_USER`].
pub fn run_as(image_user: Option<&str>) -> String {
    let user = image_user.unwrap_or("").trim();
    let (uid, gid) = match user.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (user, None),
    };
    match (uid.parse::<u32>(), gid) {
        (Ok(u), None) if u != 0 => u.to_string(),
        (Ok(u), Some(g)) if u != 0 => match g.parse::<u32>() {
            Ok(g) => format!("{u}:{g}"),
            Err(_) => FALLBACK_USER.to_string(),
        },
        _ => FALLBACK_USER.to_string(),
    }
}

/// `chelabase-<stack>-svc-<service>-<branch>`: one container per service per branch.
pub fn container_name(stack: &str, key: &Key) -> String {
    format!(
        "chelabase-{stack}-svc-{}-{}",
        key.service_id.simple(),
        key.branch_id.simple()
    )
}

/// The container the runner creates for `key`: the image's user unless it is root (or not a
/// number), a tmpfs at `/tmp` and at each image volume (never an anonymous host volume), the
/// labels and the checked environment. An image volume that isn't a plain absolute path is
/// refused.
pub fn plan(
    stack: &str,
    key: &Key,
    spec: &Validated,
    image: &ImageFacts,
    env: &BTreeMap<String, String>,
) -> Result<ContainerPlan, SpecError> {
    let env = spec::environment(env, spec.port)?;
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
    let labels = HashMap::from([
        (STACK_LABEL.to_string(), stack.to_string()),
        (SERVICE_LABEL.to_string(), key.service_id.to_string()),
        (BRANCH_LABEL.to_string(), key.branch_id.to_string()),
        (DEPLOYMENT_LABEL.to_string(), key.deployment_id.to_string()),
        (PORT_LABEL.to_string(), spec.port.to_string()),
    ]);
    Ok(ContainerPlan {
        name: container_name(stack, key),
        image: spec.image.clone(),
        user: run_as(image.user.as_deref()),
        env,
        labels,
        limits: spec.limits,
        tmpfs,
    })
}

/// Whether `labels` make a container this stack's service container.
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
#[derive(Debug, Clone, Deserialize)]
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

pub struct Runner {
    backend: Arc<dyn ContainerBackend>,
    stack: String,
    network: String,
    caps: Caps,
}

impl Runner {
    pub fn new(backend: Arc<dyn ContainerBackend>, stack: &str, network: &str, caps: Caps) -> Self {
        Runner {
            backend,
            stack: stack.to_string(),
            network: network.to_string(),
            caps,
        }
    }

    pub fn stack(&self) -> &str {
        &self.stack
    }

    pub fn caps(&self) -> &Caps {
        &self.caps
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

    /// A running container for the request's service on its branch, at its deployment. The
    /// spec and env are checked before anything else happens (refused: nothing is created or
    /// removed). A container at another deployment is replaced; one at this deployment is
    /// started if it was stopped and otherwise kept as it is.
    pub async fn ensure(&self, req: &EnsureRequest) -> Result<Container, RunnerError> {
        let spec = spec::parse(&req.spec, &self.caps)?;
        spec::environment(&req.env, spec.port)?;
        let key = Key {
            service_id: req.service_id,
            branch_id: req.branch_id,
            deployment_id: req.deployment_id,
        };
        let (mut keep, mut stale) = (None, Vec::new());
        for c in self.ours().await? {
            let Some(k) = key_of(&c) else { continue };
            if k.service_id != key.service_id || k.branch_id != key.branch_id {
                continue;
            }
            if k == key && keep.is_none() {
                keep = Some(c);
            } else {
                stale.push(c.id);
            }
        }
        if let Some(c) = keep {
            for id in &stale {
                self.backend.remove(id).await?;
            }
            if !c.running() {
                self.backend.start(&c.id).await?;
            }
            return self.guarded(&c.id).await;
        }
        // Everything that can refuse comes before the old container goes.
        let facts = self.backend.image(&spec.image).await?;
        let plan = plan(&self.stack, &key, &spec, &facts, &req.env)?;
        for id in &stale {
            self.backend.remove(id).await?;
        }
        let id = self.backend.create(&plan, &self.network).await?;
        self.backend.start(&id).await?;
        self.guarded(&id).await
    }

    pub async fn start(&self, id: &str) -> Result<Status, RunnerError> {
        let c = self.guarded(id).await?;
        self.backend.start(&c.id).await?;
        Ok(Status::from(&self.guarded(&c.id).await?))
    }

    pub async fn stop(&self, id: &str) -> Result<Status, RunnerError> {
        let c = self.guarded(id).await?;
        self.backend.stop(&c.id).await?;
        Ok(Status::from(&self.guarded(&c.id).await?))
    }

    pub async fn remove(&self, id: &str) -> Result<(), RunnerError> {
        let c = self.guarded(id).await?;
        self.backend.remove(&c.id).await?;
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

/// An in-memory backend for the unit tests: it records every call that names a container.
#[cfg(test)]
pub mod fake {
    use std::sync::Mutex;

    use super::*;
    use crate::spec::ImageRef;

    #[derive(Default)]
    pub struct Fake {
        pub containers: Mutex<Vec<(Container, Option<ContainerPlan>)>>,
        /// `(call, id)` for every start, stop, remove and create.
        pub calls: Mutex<Vec<(String, String)>>,
        pub image: Mutex<ImageFacts>,
        next: Mutex<u32>,
    }

    impl Fake {
        /// Adds a container with `labels` (as if made by someone else) and returns its id.
        pub fn add(&self, name: &str, labels: &[(&str, &str)], state: &str) -> String {
            let id = format!("{:064x}", {
                let mut n = self.next.lock().unwrap();
                *n += 1;
                *n
            });
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
        async fn image(&self, _: &ImageRef) -> anyhow::Result<ImageFacts> {
            Ok(self.image.lock().unwrap().clone())
        }
        async fn create(&self, plan: &ContainerPlan, _network: &str) -> anyhow::Result<String> {
            let labels: Vec<(&str, &str)> = plan
                .labels
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let id = self.add(&plan.name, &labels, "created");
            self.containers.lock().unwrap().last_mut().unwrap().1 = Some(plan.clone());
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
            Ok(())
        }
        async fn logs(&self, id: &str, tail: usize) -> anyhow::Result<String> {
            self.record("logs", id);
            Ok(format!("last {tail} lines"))
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
            (Some("1000"), "1000"),
            (Some("1000:0"), "1000:0"),
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
        let c = runner(&fake, "a").ensure(&request(&key)).await.unwrap();
        assert!(c.running());
        assert_eq!(key_of(&c), Some(key));
        let plans = fake.plans();
        assert_eq!(plans.len(), 1);
        let p = &plans[0];
        assert_eq!(p.name, container_name("a", &key));
        assert_eq!(p.user, FALLBACK_USER);
        assert_eq!(p.env, ["SECRET=s3cret", "PORT=8080"]);
        assert_eq!(p.labels[STACK_LABEL], "a");
        assert_eq!(p.labels[PORT_LABEL], "8080");
        // Again: kept, nothing new.
        let again = runner(&fake, "a").ensure(&request(&key)).await.unwrap();
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
            user: None,
            volumes: vec!["/data/../../etc".into()],
        };
        assert!(matches!(
            r.ensure(&request(&other)).await,
            Err(RunnerError::Invalid(_))
        ));
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        assert!(
            fake.get(&existing.id).is_some(),
            "the running deployment stays"
        );
    }
}
