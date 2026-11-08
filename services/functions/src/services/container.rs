//! m11: container services. The service runner runs them ([`super::runner`]); this service is
//! the one that asks it to (only ensure creates a container, and only this service holds the
//! branch's secrets) and the one that dials them.
//!
//! - **Where a container is:** the address the runner reports on the service's own network,
//!   an IP literal, at the spec's port ([`placed`]). Nothing else is dialled through the
//!   containers' client, which has no outbound deny: the services' hard deny reads the attached
//!   networks once at startup and never sees a service network the runner attaches later, so the
//!   runner's report is the one allowed target.
//! - **The keeper** ([`Keeper`]): every [`KEEP_EVERY`], and whenever a functions change arrives,
//!   it ensures each container service enabled on a live branch with the branch's current
//!   secrets (so a deploy, an enable, a secret change, a removed container or a restart all
//!   converge), notes where each one is, and checks its health as m9 checks external services.
//!   Health is per branch's container ([`health_key`]); the metadata keeps one record per
//!   deployment, so a deployment two branches run shows the last check of either.
//! - **A rollover** ([`rollout`]): ensure the new deployment beside the old one, probe its health
//!   every [`PROBE_EVERY`] for up to [`ROLLOUT_TIMEOUT`]; the caller switches the route only once
//!   it passes. A container that never passes is removed, and the old one keeps serving; the
//!   runner's reconcile removes the old one after the switch.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chelabase_http::outbound::Url;
use uuid::Uuid;

use super::health::{CHECK_TIMEOUT, Health, Status};
use super::runner::{ContainerStatus, Ensure, RunnerApi, RunnerError};
use super::target::upstream;
use crate::directory::Directory;

pub const ROLLOUT_TIMEOUT: Duration = Duration::from_secs(60);
pub const PROBE_EVERY: Duration = Duration::from_secs(1);
pub const KEEP_EVERY: Duration = Duration::from_secs(15);

/// Where a container listens: its id (for logs) and its base URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placed {
    pub container_id: String,
    pub base: Url,
}

/// How long a rollover's placement outlives the keeper's rounds before its deployment is the
/// active one (the switch comes right after it passes).
pub const ROLLOUT_PIN: Duration = Duration::from_secs(60);

/// `http://<address>:<port>/` from what the runner reported, only when the address is an IP
/// literal and the port is the spec's: a name would be resolved again, somewhere else.
pub fn placed(status: &ContainerStatus, port: u16) -> Option<Placed> {
    let ip: IpAddr = status.address.as_deref()?.parse().ok()?;
    if status.port.is_some_and(|p| p != port) {
        return None;
    }
    let host = match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    };
    Some(Placed {
        container_id: status.id.clone(),
        base: Url::parse(&format!("http://{host}:{port}/")).ok()?,
    })
}

/// The spec's port and health path (`/` when unset).
pub fn port_and_health(spec: &serde_json::Value) -> Option<(u16, String)> {
    let port = u16::try_from(spec.get("port")?.as_u64()?).ok()?;
    let path = spec
        .get("health_path")
        .and_then(|p| p.as_str())
        .unwrap_or("/");
    Some((port, path.to_owned()))
}

/// The health URL under a container's base.
pub fn health_url(base: &Url, path: &str) -> Option<Url> {
    Some(upstream(base, path, None)?.url)
}

/// A branch's container's key in the containers' [`Health`].
pub fn health_key(deployment: Uuid, branch: Uuid) -> Uuid {
    Uuid::from_u128(deployment.as_u128() ^ branch.as_u128().rotate_left(64))
}

type Slot = (Uuid, Uuid, Uuid);

/// Where each (branch, function, deployment)'s container is, as last ensured.
#[derive(Default)]
pub struct Containers {
    placed: Mutex<HashMap<Slot, (Placed, Option<tokio::time::Instant>)>>,
}

impl Containers {
    pub fn get(&self, branch: Uuid, function: Uuid, deployment: Uuid) -> Option<Placed> {
        self.placed
            .lock()
            .expect("not poisoned")
            .get(&(branch, function, deployment))
            .map(|(p, _)| p.clone())
    }

    pub fn put(&self, branch: Uuid, function: Uuid, deployment: Uuid, placed: Placed) {
        self.placed
            .lock()
            .expect("not poisoned")
            .insert((branch, function, deployment), (placed, None));
    }

    /// A rollover's placement: kept through the keeper's rounds for [`ROLLOUT_PIN`], until its
    /// deployment is active and the keeper places it itself.
    pub fn pin(&self, branch: Uuid, function: Uuid, deployment: Uuid, placed: Placed) {
        let until = tokio::time::Instant::now() + ROLLOUT_PIN;
        self.placed
            .lock()
            .expect("not poisoned")
            .insert((branch, function, deployment), (placed, Some(until)));
    }

    fn retain(&self, keep: &HashSet<Slot>) {
        let now = tokio::time::Instant::now();
        self.placed
            .lock()
            .expect("not poisoned")
            .retain(|k, (_, until)| keep.contains(k) || until.is_some_and(|u| u > now));
    }
}

/// Whether a health URL answers 2xx in time.
#[async_trait::async_trait]
pub trait Probe: Send + Sync + 'static {
    async fn passes(&self, url: &Url) -> bool;
}

/// The containers' client: no redirects, no outbound policy (it is only ever given a
/// runner-reported address).
pub fn client(connect: Duration, idle: Duration) -> reqwest::Result<chelabase_http::Client> {
    chelabase_http::client_builder()
        .connect_timeout(connect)
        .read_timeout(idle)
        .build()
}

pub struct HttpProbe(pub chelabase_http::Client);

#[async_trait::async_trait]
impl Probe for HttpProbe {
    async fn passes(&self, url: &Url) -> bool {
        let req = self.0.get(url.as_str()).timeout(CHECK_TIMEOUT).send();
        matches!(tokio::time::timeout(CHECK_TIMEOUT, req).await, Ok(Ok(r)) if r.status().is_success())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RolloutError {
    #[error(transparent)]
    Runner(#[from] RunnerError),
    /// The spec has no port, or the runner reported no usable address.
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Unhealthy(String),
}

/// Ensures `req`'s container and waits for it to pass its health check (at most `timeout`).
/// One that never passes (or has no address) is removed before the error comes back.
pub async fn rollout(
    runner: &dyn RunnerApi,
    probe: &dyn Probe,
    req: &Ensure<'_>,
    timeout: Duration,
) -> Result<Placed, RolloutError> {
    let (port, path) = port_and_health(req.spec)
        .ok_or_else(|| RolloutError::Invalid("the spec has no port".into()))?;
    let status = runner.ensure(req).await?;
    let found = placed(&status, port).and_then(|p| Some((health_url(&p.base, &path)?, p)));
    let Some((url, at)) = found else {
        let _ = runner.remove(&status.id).await;
        return Err(RolloutError::Invalid(
            "the service runner reported no address for the container".into(),
        ));
    };
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if probe.passes(&url).await {
            return Ok(at);
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(PROBE_EVERY).await;
    }
    if let Err(e) = runner.remove(&status.id).await {
        tracing::warn!(
            container = status.id,
            "remove a container that never passed: {e}"
        );
    }
    Err(RolloutError::Unhealthy(format!(
        "the new container didn't pass GET {path} within {} s; the previous deployment still serves",
        timeout.as_secs()
    )))
}

/// Ensures and checks every enabled container service ([module docs](self)).
pub struct Keeper {
    pub directory: Arc<dyn Directory>,
    pub runner: Arc<dyn RunnerApi>,
    pub probe: Arc<dyn Probe>,
    pub containers: Arc<Containers>,
    pub health: Arc<Health>,
    pub wake: Arc<tokio::sync::Notify>,
}

impl Keeper {
    /// One round over every target, one at a time.
    pub async fn round(&self) {
        let targets = match self.directory.container_targets().await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("list container services: {e}");
                return;
            }
        };
        let mut keep = HashSet::new();
        let mut keep_health = HashSet::new();
        for t in &targets {
            let (branch, function, deployment) =
                (t.branch_id.as_uuid(), t.function_id, t.deployment_id);
            keep.insert((branch, function, deployment));
            let key = health_key(deployment, branch);
            keep_health.insert(key);
            let Some((port, path)) = port_and_health(&t.spec) else {
                continue;
            };
            let env = match self.directory.container_env(branch, function).await {
                Ok(env) => env,
                Err(e) => {
                    tracing::warn!(%branch, %function, "a container service's env: {e}");
                    continue;
                }
            };
            let req = Ensure {
                service_id: function,
                branch_id: branch,
                deployment_id: deployment,
                spec: &t.spec,
                env: &env,
            };
            let at = match self.runner.ensure(&req).await {
                Ok(st) => placed(&st, port),
                Err(e) => {
                    tracing::warn!(%branch, %function, %deployment, "ensure a container service: {e}");
                    if let RunnerError::Refused(why) = &e {
                        let _ = self.directory.record_error(branch, function, why).await;
                    }
                    None
                }
            };
            let passed = match &at {
                Some(p) => match health_url(&p.base, &path) {
                    Some(url) => self.probe.passes(&url).await,
                    None => false,
                },
                None => false,
            };
            if let Some(p) = at {
                self.containers.put(branch, function, deployment, p);
            }
            if let Some(status) = self.health.observe(key, passed) {
                tracing::info!(%branch, %deployment, ?status, "container service health changed");
            }
            match self.health.status(key) {
                Status::Unknown => {}
                status => {
                    if let Err(e) = self
                        .directory
                        .record_health(deployment, status == Status::Healthy)
                        .await
                    {
                        tracing::warn!(%deployment, "record service health: {e}");
                    }
                }
            }
        }
        self.containers.retain(&keep);
        self.health.retain(&keep_health);
    }

    /// Rounds every [`KEEP_EVERY`] and on each wake, for as long as the service runs.
    pub fn spawn(self: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                self.round().await;
                tokio::select! {
                    () = tokio::time::sleep(KEEP_EVERY) => {}
                    () = self.wake.notified() => {}
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chelabase_metadata::ContainerTarget;
    use chelabase_types::BranchId;
    use serde_json::json;
    use zeroize::Zeroizing;

    use super::*;

    /// What an ensure was given: branch, deployment and env.
    pub(crate) type Ensured = (Uuid, Uuid, BTreeMap<String, String>);

    /// A runner that answers ensure as set, and records removes.
    #[derive(Default)]
    pub(crate) struct FakeRunner {
        pub ensure: Mutex<Option<Result<ContainerStatus, RunnerError>>>,
        pub ensured: Mutex<Vec<Ensured>>,
        pub removed: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl RunnerApi for FakeRunner {
        async fn ensure(&self, req: &Ensure<'_>) -> Result<ContainerStatus, RunnerError> {
            self.ensured.lock().unwrap().push((
                req.branch_id,
                req.deployment_id,
                req.env
                    .iter()
                    .map(|(k, v)| (k.clone(), v.to_string()))
                    .collect(),
            ));
            self.ensure.lock().unwrap().clone().unwrap_or_else(|| {
                Ok(status(
                    &format!("c-{}", req.deployment_id.simple()),
                    Some("10.9.0.2"),
                ))
            })
        }
        async fn remove(&self, container: &str) -> Result<(), RunnerError> {
            self.removed.lock().unwrap().push(container.into());
            Ok(())
        }
        async fn logs(&self, _: &str) -> Result<String, RunnerError> {
            Ok(String::new())
        }
        async fn resolve_image(&self, image: &str) -> Result<String, RunnerError> {
            Ok(image.into())
        }
    }

    pub(crate) fn status(id: &str, address: Option<&str>) -> ContainerStatus {
        ContainerStatus {
            id: id.into(),
            state: "running".into(),
            deployment_id: None,
            port: Some(8080),
            address: address.map(Into::into),
        }
    }

    /// Passes from the `after`-th probe on (never when `None`), counting probes.
    struct FakeProbe {
        after: Option<usize>,
        probes: AtomicUsize,
        urls: Mutex<Vec<String>>,
    }

    impl FakeProbe {
        fn new(after: Option<usize>) -> Self {
            Self {
                after,
                probes: AtomicUsize::new(0),
                urls: Mutex::default(),
            }
        }
    }

    #[async_trait::async_trait]
    impl Probe for FakeProbe {
        async fn passes(&self, url: &Url) -> bool {
            self.urls.lock().unwrap().push(url.to_string());
            let n = self.probes.fetch_add(1, Ordering::SeqCst) + 1;
            self.after.is_some_and(|a| n >= a)
        }
    }

    fn spec() -> serde_json::Value {
        json!({"backend": "container", "image": "ghcr.io/acme/echo@sha256:aa", "port": 8080, "health_path": "/healthz"})
    }

    fn env() -> BTreeMap<String, Zeroizing<String>> {
        BTreeMap::from([("API_KEY".to_string(), Zeroizing::new("k".to_string()))])
    }

    #[test]
    fn only_a_reported_ip_at_the_specs_port_is_placed() {
        let p = placed(&status("c", Some("10.9.0.2")), 8080).unwrap();
        assert_eq!(p.base.as_str(), "http://10.9.0.2:8080/");
        assert_eq!(p.container_id, "c");
        assert_eq!(
            placed(&status("c", Some("fd00::2")), 8080)
                .unwrap()
                .base
                .as_str(),
            "http://[fd00::2]:8080/"
        );
        for bad in [
            None,
            Some("metadata-db"),
            Some("10.9.0.2:22"),
            Some(""),
            Some("example.com"),
        ] {
            assert_eq!(placed(&status("c", bad), 8080), None, "{bad:?}");
        }
        assert_eq!(
            placed(&status("c", Some("10.9.0.2")), 9090),
            None,
            "another port"
        );
        assert_eq!(
            health_url(&p.base, "/../../admin").unwrap().as_str(),
            "http://10.9.0.2:8080/admin"
        );
    }

    /// The rollover state machine: healthy after a few probes → placed, nothing removed;
    /// never healthy → removed after the timeout, `Unhealthy`; runner refusal or outage → that
    /// error, nothing probed; no address → removed, `Invalid`.
    #[tokio::test(start_paused = true)]
    async fn rollouts_switch_only_when_healthy() {
        let (svc, branch, dep) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let (spec, env) = (spec(), env());
        let req = Ensure {
            service_id: svc,
            branch_id: branch,
            deployment_id: dep,
            spec: &spec,
            env: &env,
        };

        let runner = FakeRunner::default();
        let probe = FakeProbe::new(Some(3));
        let at = rollout(&runner, &probe, &req, ROLLOUT_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(at.base.as_str(), "http://10.9.0.2:8080/");
        assert_eq!(probe.probes.load(Ordering::SeqCst), 3);
        assert_eq!(
            probe.urls.lock().unwrap()[0],
            "http://10.9.0.2:8080/healthz"
        );
        assert!(runner.removed.lock().unwrap().is_empty());
        assert_eq!(runner.ensured.lock().unwrap()[0].2["API_KEY"], "k");

        let runner = FakeRunner::default();
        let probe = FakeProbe::new(None);
        let started = tokio::time::Instant::now();
        let err = rollout(&runner, &probe, &req, ROLLOUT_TIMEOUT)
            .await
            .unwrap_err();
        assert!(matches!(err, RolloutError::Unhealthy(_)), "{err:?}");
        assert!(started.elapsed() >= ROLLOUT_TIMEOUT);
        assert!(started.elapsed() < ROLLOUT_TIMEOUT + Duration::from_secs(5));
        assert_eq!(
            *runner.removed.lock().unwrap(),
            [format!("c-{}", dep.simple())]
        );

        for e in [
            RunnerError::Refused("bad image".into()),
            RunnerError::Unavailable("down".into()),
        ] {
            let runner = FakeRunner::default();
            *runner.ensure.lock().unwrap() = Some(Err(e.clone()));
            let probe = FakeProbe::new(Some(1));
            assert_eq!(
                rollout(&runner, &probe, &req, ROLLOUT_TIMEOUT)
                    .await
                    .unwrap_err(),
                RolloutError::Runner(e)
            );
            assert_eq!(probe.probes.load(Ordering::SeqCst), 0);
            assert!(runner.removed.lock().unwrap().is_empty());
        }

        let runner = FakeRunner::default();
        *runner.ensure.lock().unwrap() = Some(Ok(status("nowhere", None)));
        let probe = FakeProbe::new(Some(1));
        let err = rollout(&runner, &probe, &req, ROLLOUT_TIMEOUT)
            .await
            .unwrap_err();
        assert!(matches!(err, RolloutError::Invalid(_)));
        assert_eq!(*runner.removed.lock().unwrap(), ["nowhere"]);
    }

    #[derive(Default)]
    struct Dir {
        targets: Mutex<Vec<ContainerTarget>>,
        health: Mutex<Vec<(Uuid, bool)>>,
        errors: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl Directory for Dir {
        async fn resolve(
            &self,
            _: Uuid,
            _: &str,
        ) -> anyhow::Result<Option<crate::directory::Parts>> {
            Ok(None)
        }
        async fn record_error(&self, _: Uuid, _: Uuid, error: &str) -> anyhow::Result<()> {
            self.errors.lock().unwrap().push(error.into());
            Ok(())
        }
        async fn database_live(&self, _: Uuid) -> anyhow::Result<bool> {
            Ok(true)
        }
        async fn container_targets(&self) -> anyhow::Result<Vec<ContainerTarget>> {
            Ok(self.targets.lock().unwrap().clone())
        }
        async fn container_env(
            &self,
            branch: Uuid,
            _: Uuid,
        ) -> anyhow::Result<BTreeMap<String, Zeroizing<String>>> {
            Ok(BTreeMap::from([(
                "CHELABASE_BRANCH_ID".to_string(),
                Zeroizing::new(branch.to_string()),
            )]))
        }
        async fn record_health(&self, deployment: Uuid, healthy: bool) -> anyhow::Result<()> {
            self.health.lock().unwrap().push((deployment, healthy));
            Ok(())
        }
    }

    /// A round ensures each target with its branch's env, notes where it is, records each
    /// decided health check, and forgets targets that went; a refused ensure is the service's
    /// `last_error`.
    #[tokio::test]
    async fn the_keeper_ensures_places_and_checks() {
        let dir = Arc::new(Dir::default());
        let runner = Arc::new(FakeRunner::default());
        let (b1, b2, f, d) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let target = |b: Uuid| ContainerTarget {
            branch_id: BranchId(b),
            function_id: f,
            deployment_id: d,
            spec: spec(),
        };
        *dir.targets.lock().unwrap() = vec![target(b1), target(b2)];
        let keeper = Keeper {
            directory: dir.clone(),
            runner: runner.clone(),
            probe: Arc::new(FakeProbe::new(Some(1))),
            containers: Arc::default(),
            health: Arc::default(),
            wake: Arc::default(),
        };
        keeper.round().await;
        let ensured = runner.ensured.lock().unwrap().clone();
        assert_eq!(ensured.len(), 2);
        assert_eq!(ensured[0].2["CHELABASE_BRANCH_ID"], b1.to_string());
        assert_eq!(
            keeper.containers.get(b1, f, d).unwrap().base.as_str(),
            "http://10.9.0.2:8080/"
        );
        assert!(keeper.containers.get(b2, f, d).is_some());
        assert_eq!(keeper.health.status(health_key(d, b1)), Status::Healthy);
        assert_ne!(health_key(d, b1), health_key(d, b2));
        assert_eq!(*dir.health.lock().unwrap(), [(d, true), (d, true)]);

        *dir.targets.lock().unwrap() = vec![target(b1)];
        *runner.ensure.lock().unwrap() = Some(Err(RunnerError::Refused(
            "PORT is set by the runner".into(),
        )));
        keeper.round().await;
        assert!(
            keeper.containers.get(b2, f, d).is_none(),
            "a target that went is forgotten"
        );
        assert_eq!(*dir.errors.lock().unwrap(), ["PORT is set by the runner"]);
        *runner.ensure.lock().unwrap() = None;
        let next = Uuid::new_v4();
        keeper.containers.pin(
            b1,
            f,
            next,
            placed(&status("new", Some("10.9.0.3")), 8080).unwrap(),
        );
        keeper.round().await;
        assert!(
            keeper.containers.get(b1, f, next).is_some(),
            "a rollover's placement is pinned"
        );
        assert!(
            keeper.containers.get(b1, f, d).is_some(),
            "kept until the next placement"
        );
    }
}
