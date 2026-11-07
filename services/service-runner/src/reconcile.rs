//! Reconcile: every 30 s, converge this stack's service containers on the metadata's desired
//! state (each branch's `kind = service` function whose active deployment is a container spec,
//! and whether it is enabled there). Only the runner's own containers are ever compared or
//! touched ([`Runner::ours`]), so another stack's containers and anything unlabelled are
//! invisible to it. A pass whose desired state can't be read changes nothing.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use crate::backend::Container;
use crate::runner::{EnsureRequest, Key, Runner, key_of};
use crate::spec;

/// One branch's container service, as the metadata has it.
#[derive(Debug, Clone, PartialEq)]
pub struct Desired {
    pub key: Key,
    pub enabled: bool,
    pub spec: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// No container for an enabled service: create it (with no secrets: the functions service
    /// passes them with its own ensure, m11).
    Create(Desired),
    Start(String),
    Stop(String),
    Remove(String),
}

/// What to do, given the desired rows (already checked: an invalid spec is left out, so its
/// container goes) and this stack's containers:
/// - a container whose service and branch have no row, or whose deployment isn't the row's, or
///   a second one for the same row, is removed;
/// - an enabled row with no container is created, with a stopped one is started;
/// - a disabled row's running container is stopped.
pub fn plan(desired: &[Desired], actual: &[Container]) -> Vec<Action> {
    let rows: HashMap<(Uuid, Uuid), &Desired> = desired
        .iter()
        .map(|d| ((d.key.service_id, d.key.branch_id), d))
        .collect();
    let mut matched: HashSet<(Uuid, Uuid)> = HashSet::new();
    let mut actions = Vec::new();
    for c in actual {
        let Some(k) = key_of(c) else {
            actions.push(Action::Remove(c.id.clone()));
            continue;
        };
        let slot = (k.service_id, k.branch_id);
        match rows.get(&slot) {
            Some(d) if d.key.deployment_id == k.deployment_id && !matched.contains(&slot) => {
                matched.insert(slot);
                match (d.enabled, c.running()) {
                    (true, false) => actions.push(Action::Start(c.id.clone())),
                    (false, true) => actions.push(Action::Stop(c.id.clone())),
                    _ => {}
                }
            }
            _ => actions.push(Action::Remove(c.id.clone())),
        }
    }
    for d in desired {
        if d.enabled && !matched.contains(&(d.key.service_id, d.key.branch_id)) {
            actions.push(Action::Create(d.clone()));
        }
    }
    actions
}

/// Where desired state comes from.
#[async_trait]
pub trait DesiredSource: Send + Sync + 'static {
    async fn desired(&self) -> anyhow::Result<Vec<Desired>>;
}

/// The metadata database (read only).
pub struct MetadataDesired {
    pub pool: PgPool,
}

/// Each live branch's container service: a `service` function whose active deployment's spec
/// has `backend = "container"`, on a branch and database that aren't deleted.
pub const DESIRED_SQL: &str = "
    SELECT bf.function_id, bf.branch_id, bf.deployment_id, bf.enabled, d.spec
    FROM branch_functions bf
    JOIN functions f ON f.id = bf.function_id
    JOIN function_deployments d ON d.id = bf.deployment_id
    JOIN branches b ON b.id = bf.branch_id
    JOIN databases db ON db.id = f.database_id
    WHERE f.kind = 'service'
      AND d.spec->>'backend' = 'container'
      AND b.deleted_at IS NULL
      AND db.status NOT IN ('deleting', 'deleted')";

#[async_trait]
impl DesiredSource for MetadataDesired {
    async fn desired(&self) -> anyhow::Result<Vec<Desired>> {
        let rows: Vec<(Uuid, Uuid, Uuid, bool, serde_json::Value)> =
            sqlx::query_as(DESIRED_SQL).fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(
                |(service_id, branch_id, deployment_id, enabled, spec)| Desired {
                    key: Key {
                        service_id,
                        branch_id,
                        deployment_id,
                    },
                    enabled,
                    spec,
                },
            )
            .collect())
    }
}

/// One pass: read the desired state (an error ends the pass before anything is touched), drop
/// rows whose spec the runner refuses, plan, and apply. Returns what it did; one action's
/// failure is logged and the rest still run.
pub async fn reconcile_once(
    runner: &Runner,
    source: &dyn DesiredSource,
) -> anyhow::Result<Vec<Action>> {
    let desired: Vec<Desired> = source
        .desired()
        .await?
        .into_iter()
        .filter(|d| match spec::parse(&d.spec, runner.caps()) {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(
                    "service {} on branch {}: its deployment's spec is refused: {e}",
                    d.key.service_id,
                    d.key.branch_id
                );
                false
            }
        })
        .collect();
    let actual = runner.ours().await?;
    let actions = plan(&desired, &actual);
    for action in &actions {
        let result = match action {
            Action::Create(d) => runner
                .ensure(&EnsureRequest {
                    service_id: d.key.service_id,
                    branch_id: d.key.branch_id,
                    deployment_id: d.key.deployment_id,
                    spec: d.spec.clone(),
                    env: Default::default(),
                })
                .await
                .map(drop),
            Action::Start(id) => runner.start(id).await.map(drop),
            Action::Stop(id) => runner.stop(id).await.map(drop),
            Action::Remove(id) => runner.remove(id).await,
        };
        match result {
            Ok(()) => tracing::info!("reconcile: {action:?}"),
            Err(e) => tracing::warn!("reconcile: {action:?} failed: {e:#}"),
        }
    }
    Ok(actions)
}

/// Runs [`reconcile_once`] every `every`, forever.
pub fn spawn_loop(runner: Arc<Runner>, source: Arc<dyn DesiredSource>, every: Duration) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = reconcile_once(&runner, source.as_ref()).await {
                tracing::warn!("reconcile pass skipped: {e:#}");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;
    use crate::runner::fake::Fake;
    use crate::spec::Caps;
    use crate::{SERVICE_LABEL, STACK_LABEL};

    fn spec() -> serde_json::Value {
        json!({"backend": "container", "image": "ghcr.io/acme/echo:1", "port": 8080})
    }

    fn key() -> Key {
        Key {
            service_id: Uuid::new_v4(),
            branch_id: Uuid::new_v4(),
            deployment_id: Uuid::new_v4(),
        }
    }

    fn desired(key: Key, enabled: bool) -> Desired {
        Desired {
            key,
            enabled,
            spec: spec(),
        }
    }

    fn container(id: &str, key: &Key, state: &str) -> Container {
        Container {
            id: id.into(),
            labels: [
                (STACK_LABEL, "a".to_string()),
                (SERVICE_LABEL, key.service_id.to_string()),
                (crate::BRANCH_LABEL, key.branch_id.to_string()),
                (crate::DEPLOYMENT_LABEL, key.deployment_id.to_string()),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
            state: state.into(),
        }
    }

    #[test]
    fn the_diff() {
        let (on, off, gone, moved, idle, fresh) = (key(), key(), key(), key(), key(), key());
        let moved_now = Key {
            deployment_id: Uuid::new_v4(),
            ..moved
        };
        let rows = [
            desired(on, true),
            desired(off, false),
            desired(moved_now, true),
            desired(idle, false),
            desired(fresh, true),
        ];
        let actual = [
            container("on-stopped", &on, "exited"),
            container("on-dup", &on, "running"),
            container("off-running", &off, "running"),
            container("gone", &gone, "running"),
            container("moved", &moved, "running"),
            container("idle", &idle, "exited"),
        ];
        let actions = plan(&rows, &actual);
        assert_eq!(
            actions,
            [
                Action::Start("on-stopped".into()),
                Action::Remove("on-dup".into()),
                Action::Stop("off-running".into()),
                Action::Remove("gone".into()),
                Action::Remove("moved".into()),
                Action::Create(desired(moved_now, true)),
                Action::Create(desired(fresh, true)),
            ]
        );
        // Converged: nothing to do.
        let mut unlabelled = container("half", &on, "running");
        unlabelled.labels.remove(crate::DEPLOYMENT_LABEL);
        assert_eq!(plan(&[], &[unlabelled]), [Action::Remove("half".into())]);
        assert!(plan(&[desired(on, true)], &[container("x", &on, "running")]).is_empty());
        assert!(plan(&[desired(off, false)], &[]).is_empty());
    }

    struct Rows(Mutex<anyhow::Result<Vec<Desired>>>);

    #[async_trait]
    impl DesiredSource for Rows {
        async fn desired(&self) -> anyhow::Result<Vec<Desired>> {
            match &*self.0.lock().unwrap() {
                Ok(rows) => Ok(rows.clone()),
                Err(e) => Err(anyhow::anyhow!("{e}")),
            }
        }
    }

    /// Two stacks on one engine: stack A's pass with no desired rows removes A's containers
    /// and never sees B's (nor anything unlabelled).
    #[tokio::test]
    async fn a_pass_touches_only_its_own_stack() {
        let fake = Arc::new(Fake::default());
        let svc = Uuid::new_v4().to_string();
        let a = fake.add("a", &[(STACK_LABEL, "a"), (SERVICE_LABEL, &svc)], "running");
        let b = fake.add("b", &[(STACK_LABEL, "b"), (SERVICE_LABEL, &svc)], "running");
        let plain = fake.add("metadata-db", &[], "running");
        let runner = Runner::new(fake.clone(), "a", "net", Caps::default());
        let done = reconcile_once(&runner, &Rows(Mutex::new(Ok(vec![]))))
            .await
            .unwrap();
        assert_eq!(done, [Action::Remove(a.clone())]);
        assert!(fake.get(&a).is_none());
        assert_eq!(fake.get(&b).unwrap().state, "running");
        assert_eq!(fake.get(&plain).unwrap().state, "running");
        assert!(!fake.calls().iter().any(|(_, id)| *id == b || *id == plain));
    }

    /// A pass that can't read the desired state removes nothing.
    #[tokio::test]
    async fn an_unreadable_desired_state_changes_nothing() {
        let fake = Arc::new(Fake::default());
        let svc = Uuid::new_v4().to_string();
        let a = fake.add("a", &[(STACK_LABEL, "a"), (SERVICE_LABEL, &svc)], "running");
        let runner = Runner::new(fake.clone(), "a", "net", Caps::default());
        let rows = Rows(Mutex::new(Err(anyhow::anyhow!("metadata down"))));
        assert!(reconcile_once(&runner, &rows).await.is_err());
        assert!(fake.calls().is_empty());
        assert!(fake.get(&a).is_some());
    }

    /// Enabled → created and running; disabled → stopped; row gone → removed; an invalid spec
    /// is treated as no row.
    #[tokio::test]
    async fn passes_converge() {
        let fake = Arc::new(Fake::default());
        let runner = Runner::new(fake.clone(), "a", "net", Caps::default());
        let k = key();
        let rows = Rows(Mutex::new(Ok(vec![desired(k, true)])));
        reconcile_once(&runner, &rows).await.unwrap();
        let ours = runner.ours().await.unwrap();
        assert_eq!(ours.len(), 1);
        assert!(ours[0].running());
        assert!(reconcile_once(&runner, &rows).await.unwrap().is_empty());
        *rows.0.lock().unwrap() = Ok(vec![desired(k, false)]);
        reconcile_once(&runner, &rows).await.unwrap();
        assert!(!runner.ours().await.unwrap()[0].running());
        let mut bad = desired(k, true);
        bad.spec["privileged"] = json!(true);
        *rows.0.lock().unwrap() = Ok(vec![bad]);
        reconcile_once(&runner, &rows).await.unwrap();
        assert!(runner.ours().await.unwrap().is_empty());
    }
}
