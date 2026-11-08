//! Reconcile: every 30 s, converge this stack's service containers on the metadata's desired
//! state (each branch's `kind = service` function whose active deployment is a container spec,
//! and whether it is enabled there). It never creates a container: only the functions service's
//! ensure does, with the branch's secrets. It starts, stops and removes, removes networks left
//! without a container, and attaches the functions service to the networks that have one.
//!
//! Only the runner's own containers and networks are ever compared or touched
//! ([`Runner::ours`]), so another stack's and anything unlabelled are invisible to it. A pass
//! whose desired state can't be read changes nothing, and a slot with an ensure in flight is left
//! for the next pass.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use crate::backend::{Container, Network};
use crate::runner::{Key, Runner, key_of, slot_of};
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
    Start(String),
    Stop(String),
    Remove(String),
    /// A service network with no container: detach the functions service and remove it.
    RemoveNetwork(Uuid, Uuid),
    /// A service network the functions service isn't on (its container was recreated).
    Attach(String),
}

impl Action {
    /// The (service, branch) the action touches, whose lock it needs.
    fn slot(&self, slots: &HashMap<String, (Uuid, Uuid)>) -> Option<(Uuid, Uuid)> {
        match self {
            Action::Start(id) | Action::Stop(id) | Action::Remove(id) | Action::Attach(id) => {
                slots.get(id).copied()
            }
            Action::RemoveNetwork(s, b) => Some((*s, *b)),
        }
    }
}

/// m11: a rollover's new container (a pending deployment: one the metadata has for the slot's
/// live, enabled service, not the active one) is left alone this long after it was created: a
/// rollover runs it beside the active one and switches only once it's healthy (up to 60 s). Past
/// it, or without such a row, it's removed like any stale container.
pub const PENDING_GRACE_SECS: i64 = 120;

/// Whether a container created at `created` (Unix seconds) is within the grace at `now`; one
/// created in the future (a remote engine's clock ahead) never is.
pub fn in_grace(created: i64, now: i64) -> bool {
    created > 0
        && now
            .checked_sub(created)
            .is_some_and(|age| (0..PENDING_GRACE_SECS).contains(&age))
}

/// What to do, given the desired rows (already checked: an invalid spec is left out, so its
/// container goes), the pending deployments, this stack's containers and networks, and the
/// functions containers:
/// - a container whose service and branch have no row, or a second one for the same row, is
///   removed;
/// - one of another deployment is removed, unless the row is enabled, its deployment is
///   pending, it's [`in_grace`] and it's the slot's newest such container (a rollover's new
///   container: left alone, at most one per slot beside the active one);
/// - an enabled row's stopped container is started; a disabled row's running one is stopped;
/// - a network whose slot keeps no container is removed; one that keeps a container and lacks a
///   functions container gets it attached.
pub fn plan(
    desired: &[Desired],
    pending: &HashSet<Key>,
    actual: &[Container],
    networks: &[Network],
    functions: &[String],
    now: i64,
) -> Vec<Action> {
    let rows: HashMap<(Uuid, Uuid), &Desired> = desired.iter().map(|d| (d.key.slot(), d)).collect();
    let mut kept: HashSet<(Uuid, Uuid)> = HashSet::new();
    let mut actions = Vec::new();
    // Each slot's one spared rollover container: the newest that qualifies.
    let mut spared: HashMap<(Uuid, Uuid), (i64, &str)> = HashMap::new();
    for c in actual {
        let Some(k) = key_of(c) else { continue };
        let qualifies = rows
            .get(&k.slot())
            .is_some_and(|d| d.enabled && d.key.deployment_id != k.deployment_id)
            && pending.contains(&k)
            && in_grace(c.created, now);
        if qualifies {
            let e = spared.entry(k.slot()).or_insert((c.created, c.id.as_str()));
            if (c.created, c.id.as_str()) > *e {
                *e = (c.created, c.id.as_str());
            }
        }
    }
    for c in actual {
        let Some(k) = key_of(c) else {
            actions.push(Action::Remove(c.id.clone()));
            continue;
        };
        match rows.get(&k.slot()) {
            Some(d) if d.key.deployment_id == k.deployment_id && !kept.contains(&k.slot()) => {
                kept.insert(k.slot());
                match (d.enabled, c.running()) {
                    (true, false) => actions.push(Action::Start(c.id.clone())),
                    (false, true) => actions.push(Action::Stop(c.id.clone())),
                    _ => {}
                }
            }
            Some(d) if d.key.deployment_id == k.deployment_id => {
                actions.push(Action::Remove(c.id.clone()));
            }
            Some(_) if spared.get(&k.slot()).is_some_and(|(_, id)| *id == c.id) => {}
            _ => actions.push(Action::Remove(c.id.clone())),
        }
    }
    let present: HashSet<(Uuid, Uuid)> =
        actual.iter().filter_map(key_of).map(|k| k.slot()).collect();
    for n in networks {
        let Some(slot) = slot_of(n) else { continue };
        if kept.contains(&slot) {
            if functions.iter().any(|f| !n.containers.contains(f)) {
                actions.push(Action::Attach(n.id.clone()));
            }
        } else if !present.contains(&slot) {
            // A slot whose containers are all being removed loses its network with the last one.
            actions.push(Action::RemoveNetwork(slot.0, slot.1));
        }
    }
    actions
}

/// Where desired state comes from.
#[async_trait]
pub trait DesiredSource: Send + Sync + 'static {
    async fn desired(&self) -> anyhow::Result<Vec<Desired>>;
    /// m11: the pending deployments: every other container deployment of a service that is
    /// enabled on a live branch, by (service, branch, deployment). Only these get a grace.
    async fn pending(&self) -> anyhow::Result<Vec<Key>> {
        Ok(Vec::new())
    }
}

/// m11: a slot's active deployment, which ensure keeps when it prunes the slot.
#[async_trait]
pub trait ActiveSource: Send + Sync + 'static {
    /// `None` when the service has no row on the branch.
    async fn active(&self, service: Uuid, branch: Uuid) -> anyhow::Result<Option<Uuid>>;
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

/// m11: the pending deployments ([`DesiredSource::pending`]).
pub const PENDING_SQL: &str = "
    SELECT bf.function_id, bf.branch_id, d.id
    FROM branch_functions bf
    JOIN functions f ON f.id = bf.function_id
    JOIN function_deployments d ON d.function_id = bf.function_id AND d.id <> bf.deployment_id
    JOIN branches b ON b.id = bf.branch_id
    JOIN databases db ON db.id = f.database_id
    WHERE f.kind = 'service'
      AND bf.enabled
      AND d.spec->>'backend' = 'container'
      AND b.deleted_at IS NULL
      AND db.status NOT IN ('deleting', 'deleted')";

#[async_trait]
impl ActiveSource for MetadataDesired {
    async fn active(&self, service: Uuid, branch: Uuid) -> anyhow::Result<Option<Uuid>> {
        Ok(sqlx::query_scalar(
            "SELECT deployment_id FROM branch_functions WHERE function_id = $1 AND branch_id = $2",
        )
        .bind(service)
        .bind(branch)
        .fetch_optional(&self.pool)
        .await?)
    }
}

#[async_trait]
impl DesiredSource for MetadataDesired {
    async fn pending(&self) -> anyhow::Result<Vec<Key>> {
        let rows: Vec<(Uuid, Uuid, Uuid)> =
            sqlx::query_as(PENDING_SQL).fetch_all(&self.pool).await?;
        Ok(rows
            .into_iter()
            .map(|(service_id, branch_id, deployment_id)| Key {
                service_id,
                branch_id,
                deployment_id,
            })
            .collect())
    }

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

/// One pass: list this stack's containers and networks, then read the desired state (in that
/// order, so a container ensured for a new deployment before the listing meets metadata that
/// already names it; an error ends the pass before anything is touched), drop rows whose spec
/// the runner refuses, plan, and apply each action under its slot's lock, skipping a slot an
/// ensure holds. Returns what it applied; one action's failure is logged and the rest still run.
pub async fn reconcile_once(
    runner: &Runner,
    source: &dyn DesiredSource,
) -> anyhow::Result<Vec<Action>> {
    let actual = runner.ours().await?;
    let networks = runner.our_networks().await?;
    let functions = runner.functions().await?;
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
    let pending: HashSet<Key> = source.pending().await?.into_iter().collect();
    let mut slots: HashMap<String, (Uuid, Uuid)> = actual
        .iter()
        .filter_map(|c| Some((c.id.clone(), key_of(c)?.slot())))
        .collect();
    slots.extend(
        networks
            .iter()
            .filter_map(|n| Some((n.id.clone(), slot_of(n)?))),
    );
    let mut applied = Vec::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    for action in plan(&desired, &pending, &actual, &networks, &functions, now) {
        let guard = match action.slot(&slots) {
            Some(slot) => match runner.try_lock(slot) {
                Some(g) => Some(g),
                None => {
                    tracing::info!("reconcile: {action:?} skipped: an ensure is in flight");
                    continue;
                }
            },
            None => None,
        };
        let result = match &action {
            // Under the slot's lock already (taken above).
            Action::Start(id) => runner.start_unlocked(id).await.map(drop),
            Action::Stop(id) => runner.stop_unlocked(id).await.map(drop),
            Action::Remove(id) => runner.remove_unlocked(id).await,
            Action::RemoveNetwork(s, b) => runner.remove_network(*s, *b).await,
            Action::Attach(id) => runner.attach_functions(id).await,
        };
        drop(guard);
        match result {
            Ok(()) => {
                tracing::info!("reconcile: {action:?}");
                applied.push(action);
            }
            Err(e) => tracing::warn!("reconcile: {action:?} failed: {e:#}"),
        }
    }
    Ok(applied)
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
    use crate::runner::{EnsureRequest, container_name};
    use crate::spec::Caps;
    use crate::{BRANCH_LABEL, DEPLOYMENT_LABEL, SERVICE_LABEL, STACK_LABEL};

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

    fn labels(key: &Key) -> HashMap<String, String> {
        [
            (STACK_LABEL, "a".to_string()),
            (SERVICE_LABEL, key.service_id.to_string()),
            (BRANCH_LABEL, key.branch_id.to_string()),
            (DEPLOYMENT_LABEL, key.deployment_id.to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }

    fn container(id: &str, key: &Key, state: &str) -> Container {
        Container {
            id: id.into(),
            labels: labels(key),
            state: state.into(),
            ..Default::default()
        }
    }

    fn network(id: &str, key: &Key, attached: &[&str]) -> Network {
        let mut l = labels(key);
        l.remove(DEPLOYMENT_LABEL);
        Network {
            id: id.into(),
            name: container_name("a", key.service_id, key.branch_id),
            labels: l,
            containers: attached.iter().map(|c| c.to_string()).collect(),
        }
    }

    fn ensure_req(k: &Key, env: &[(&str, &str)]) -> EnsureRequest {
        EnsureRequest {
            service_id: k.service_id,
            branch_id: k.branch_id,
            deployment_id: k.deployment_id,
            spec: spec(),
            env: env
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
        }
    }

    /// Audit B1 (human decision): reconcile never creates; an enabled row with no container is
    /// left for the functions service's ensure.
    #[test]
    fn the_diff() {
        let (on, off, gone, moved, idle, fresh) = (key(), key(), key(), key(), key(), key());
        let orphan = key();
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
        let f = "functions".to_string();
        let networks = [
            network("n-on", &on, &["functions", "on-stopped"]),
            network("n-off", &off, &["off-running"]),
            network("n-gone", &gone, &["functions", "gone"]),
            network("n-orphan", &orphan, &["functions"]),
        ];
        let actions = plan(
            &rows,
            &none(),
            &actual,
            &networks,
            std::slice::from_ref(&f),
            NOW,
        );
        let k = |k: &Key| (k.service_id, k.branch_id);
        assert_eq!(
            actions,
            [
                Action::Start("on-stopped".into()),
                Action::Remove("on-dup".into()),
                Action::Stop("off-running".into()),
                Action::Remove("gone".into()),
                Action::Remove("moved".into()),
                // The functions container was recreated: attached again.
                Action::Attach("n-off".into()),
                // An orphan network (no container at all) goes; `gone`'s goes with its container.
                Action::RemoveNetwork(orphan.service_id, orphan.branch_id),
            ]
        );
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::RemoveNetwork(s, b) if (*s, *b) == k(&gone)))
        );
        let mut unlabelled = container("half", &on, "running");
        unlabelled.labels.remove(DEPLOYMENT_LABEL);
        assert_eq!(
            plan(&[], &none(), &[unlabelled], &[], &[], NOW),
            [Action::Remove("half".into())]
        );
        assert!(
            plan(
                &[desired(on, true)],
                &none(),
                &[container("x", &on, "running")],
                &[],
                &[],
                NOW
            )
            .is_empty()
        );
        assert!(
            plan(&[desired(fresh, true)], &none(), &[], &[], &[], NOW).is_empty(),
            "never a create"
        );
    }

    /// m11 (re-audit B1, B2, S1): only a pending deployment's container of an enabled row
    /// gets the grace, and only the slot's newest one; the slot's network stays meanwhile. Past
    /// the grace, with no creation time, without a pending row, with no row at all (a first deploy
    /// never rolls over) or with the row disabled, it goes. A same-deployment duplicate goes
    /// however young.
    #[test]
    fn a_rollovers_new_container_has_a_grace() {
        let active = key();
        let pending = Key {
            deployment_id: Uuid::new_v4(),
            ..active
        };
        let older = Key {
            deployment_id: Uuid::new_v4(),
            ..active
        };
        let young = |id: &str, k: &Key, age: i64| Container {
            created: NOW - age,
            ..container(id, k, "running")
        };
        let known: HashSet<Key> = [pending, older].into();
        let net = network("n", &active, &["functions", "old", "new"]);
        let f = ["functions".to_string()];
        let rows = [desired(active, true)];
        let actual = [young("old", &active, 3600), young("new", &pending, 30)];
        assert!(plan(&rows, &known, &actual, std::slice::from_ref(&net), &f, NOW).is_empty());
        assert_eq!(
            plan(&rows, &none(), &actual, std::slice::from_ref(&net), &f, NOW),
            [Action::Remove("new".into())],
            "not a pending deployment"
        );
        assert_eq!(
            plan(&[desired(active, false)], &known, &actual, &[], &f, NOW),
            [Action::Stop("old".into()), Action::Remove("new".into())],
            "disabled"
        );
        let actual = [
            young("old", &active, 3600),
            young("new", &pending, PENDING_GRACE_SECS),
        ];
        assert_eq!(
            plan(&rows, &known, &actual, std::slice::from_ref(&net), &f, NOW),
            [Action::Remove("new".into())]
        );
        // Two young pending containers: only the newest stays.
        let actual = [
            young("old", &active, 3600),
            young("p1", &older, 20),
            young("new", &pending, 10),
        ];
        assert_eq!(
            plan(&rows, &known, &actual, &[], &f, NOW),
            [Action::Remove("p1".into())]
        );
        assert_eq!(
            plan(&[], &known, &[young("new", &pending, 5)], &[], &f, NOW),
            [Action::Remove("new".into())],
            "no row"
        );
        assert_eq!(
            plan(
                &rows,
                &known,
                &[container("new", &pending, "running")],
                &[],
                &f,
                NOW
            ),
            [Action::Remove("new".into())],
            "no creation time"
        );
        let actual = [young("old", &active, 3600), young("dup", &active, 1)];
        assert_eq!(
            plan(&rows, &known, &actual, &[], &f, NOW),
            [Action::Remove("dup".into())]
        );
    }

    fn none() -> HashSet<Key> {
        HashSet::new()
    }

    const NOW: i64 = 1_800_000_000;

    /// Re-audit B1: a deleted function's container (no row, no pending deployment) goes on the
    /// first pass, however young.
    #[test]
    fn audit_deleted_function_container_removed_at_once() {
        let k = key();
        let c = Container {
            created: NOW - 5,
            ..container("c", &k, "exited")
        };
        assert_eq!(
            plan(&[], &none(), &[c], &[], &[], NOW),
            [Action::Remove("c".into())]
        );
    }

    /// Re-audit B2: a disabled row's young container of another deployment is removed at once.
    #[test]
    fn audit_disabled_row_stops_a_young_pending_container() {
        let active = key();
        let pending = Key {
            deployment_id: Uuid::new_v4(),
            ..active
        };
        let young = Container {
            created: NOW - 10,
            ..container("new", &pending, "running")
        };
        let old = Container {
            created: NOW - 3600,
            ..container("old", &active, "exited")
        };
        let actions = plan(
            &[desired(active, false)],
            &[pending].into(),
            &[old, young],
            &[],
            &[],
            NOW,
        );
        assert!(
            actions
                .iter()
                .any(|a| matches!(a, Action::Stop(id) | Action::Remove(id) if id == "new")),
            "disabled service's pending container left running: {actions:?}"
        );
    }

    /// Re-audit N1: a container created in the future (a remote engine's clock ahead) gets no
    /// grace.
    #[test]
    fn audit_future_created_gets_no_grace() {
        let active = key();
        let pending = Key {
            deployment_id: Uuid::new_v4(),
            ..active
        };
        let future = Container {
            created: NOW + 86_400,
            ..container("new", &pending, "running")
        };
        let old = Container {
            created: NOW - 3600,
            ..container("old", &active, "running")
        };
        assert_eq!(
            plan(
                &[desired(active, true)],
                &[pending].into(),
                &[old, future],
                &[],
                &[],
                NOW
            ),
            [Action::Remove("new".into())]
        );
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

    /// Audit B1: an enabled row and then the functions service's ensure with the branch's
    /// secrets: the running container has them.
    #[tokio::test]
    async fn ensure_after_a_pass_applies_the_callers_env() {
        let fake = Arc::new(Fake::default());
        let runner = Runner::new(fake.clone(), "a", "net", Caps::default());
        let k = key();
        let rows = Rows(Mutex::new(Ok(vec![desired(k, true)])));
        reconcile_once(&runner, &rows).await.unwrap();
        runner
            .ensure(&ensure_req(&k, &[("API_KEY", "s3cret")]))
            .await
            .unwrap();
        let live = fake.plans().last().unwrap().clone();
        assert!(
            live.env.contains(&"API_KEY=s3cret".to_string()),
            "{:?}",
            live.env
        );
        assert_eq!(runner.ours().await.unwrap().len(), 1);
    }

    /// Ensured → kept; disabled → stopped; enabled again → started; row gone → container and
    /// network removed; an invalid spec counts as no row.
    #[tokio::test]
    async fn passes_converge() {
        let fake = Arc::new(Fake::default());
        fake.functions.lock().unwrap().push("functions-ctr".into());
        let runner = Runner::new(fake.clone(), "a", "net", Caps::default());
        let k = key();
        let rows = Rows(Mutex::new(Ok(vec![desired(k, true)])));
        assert!(reconcile_once(&runner, &rows).await.unwrap().is_empty());
        assert!(
            runner.ours().await.unwrap().is_empty(),
            "reconcile never creates"
        );
        runner.ensure(&ensure_req(&k, &[])).await.unwrap();
        assert!(reconcile_once(&runner, &rows).await.unwrap().is_empty());
        *rows.0.lock().unwrap() = Ok(vec![desired(k, false)]);
        reconcile_once(&runner, &rows).await.unwrap();
        assert!(!runner.ours().await.unwrap()[0].running());
        *rows.0.lock().unwrap() = Ok(vec![desired(k, true)]);
        reconcile_once(&runner, &rows).await.unwrap();
        assert!(runner.ours().await.unwrap()[0].running());
        // The functions container was recreated (detached): the next pass attaches it again.
        let net = container_name("a", k.service_id, k.branch_id);
        for n in fake.networks.lock().unwrap().iter_mut() {
            n.containers.retain(|c| c != "functions-ctr");
        }
        reconcile_once(&runner, &rows).await.unwrap();
        assert!(
            fake.network(&net)
                .unwrap()
                .containers
                .contains(&"functions-ctr".to_string())
        );
        let mut bad = desired(k, true);
        bad.spec["privileged"] = json!(true);
        *rows.0.lock().unwrap() = Ok(vec![bad]);
        reconcile_once(&runner, &rows).await.unwrap();
        assert!(runner.ours().await.unwrap().is_empty());
        assert!(
            fake.network(&net).is_none(),
            "the network went with the container"
        );
    }

    /// An orphaned service network (its container gone by other means) is detached and removed.
    #[tokio::test]
    async fn orphan_networks_are_removed() {
        let fake = Arc::new(Fake::default());
        fake.functions.lock().unwrap().push("functions-ctr".into());
        let runner = Runner::new(fake.clone(), "a", "net", Caps::default());
        let k = key();
        let c = runner.ensure(&ensure_req(&k, &[])).await.unwrap();
        fake.containers
            .lock()
            .unwrap()
            .retain(|(x, _)| x.id != c.id);
        let other = network("n-b", &key(), &["functions-ctr"]);
        let mut foreign = other.clone();
        foreign.labels.insert(STACK_LABEL.into(), "b".into());
        fake.networks.lock().unwrap().push(foreign);
        let done = reconcile_once(&runner, &Rows(Mutex::new(Ok(vec![]))))
            .await
            .unwrap();
        assert_eq!(done, [Action::RemoveNetwork(k.service_id, k.branch_id)]);
        assert!(
            fake.network(&container_name("a", k.service_id, k.branch_id))
                .is_none()
        );
        assert!(
            fake.network("n-b").is_some(),
            "another stack's network is never touched"
        );
    }

    /// Audit S1: a slot whose lock an ensure holds is skipped by the pass, which would otherwise
    /// remove the container ensure is working on.
    #[tokio::test]
    async fn a_pass_skips_a_slot_with_an_ensure_in_flight() {
        let fake = Arc::new(Fake::default());
        let runner = Runner::new(fake.clone(), "a", "net", Caps::default());
        let k = key();
        let c = runner.ensure(&ensure_req(&k, &[])).await.unwrap();
        let held = runner.lock(k.slot()).await;
        let done = reconcile_once(&runner, &Rows(Mutex::new(Ok(vec![]))))
            .await
            .unwrap();
        assert!(done.is_empty(), "{done:?}");
        assert!(fake.get(&c.id).is_some());
        drop(held);
        let done = reconcile_once(&runner, &Rows(Mutex::new(Ok(vec![]))))
            .await
            .unwrap();
        assert_eq!(done, [Action::Remove(c.id.clone())]);
    }

    /// Audit S1: ensure waits for the slot's lock (a pass or another ensure holding it).
    #[tokio::test]
    async fn ensure_waits_for_the_slot() {
        let fake = Arc::new(Fake::default());
        let runner = Arc::new(Runner::new(fake.clone(), "a", "net", Caps::default()));
        let k = key();
        let held = runner.lock(k.slot()).await;
        let r = runner.clone();
        let task = tokio::spawn(async move { r.ensure(&ensure_req(&k, &[])).await.map(|c| c.id) });
        tokio::task::yield_now().await;
        tokio::task::yield_now().await;
        assert!(fake.plans().is_empty(), "ensure must wait for the slot");
        drop(held);
        task.await.unwrap().unwrap();
        assert_eq!(fake.plans().len(), 1);
    }
}
