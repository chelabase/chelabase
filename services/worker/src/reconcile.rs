//! Reconcile: compare metadata rows with the computes and tenants that actually exist, and
//! repair what a lost job or a failed step left behind.

use std::collections::HashSet;

use anyhow::Context;
use cheladb_metadata::{DatabaseStatus, EndpointState, LiveDatabase};
use cheladb_neon::ComputeBackend;
use cheladb_types::DatabaseId;
use tracing::{info, warn};

use crate::Worker;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::provision::{new_compute_token, sha256_hex};

pub const STALE_PROVISION_SECS: i64 = 600;
pub const STALE_DELETE_SECS: i64 = 60;

/// One observation. Taken in the order computes, tenants, databases, endpoints: rows are
/// written before their compute or tenant exists, so a fresh resource never looks orphaned.
#[derive(Debug, Default)]
pub struct Snapshot {
    pub databases: Vec<LiveDatabase>,
    pub endpoints: Vec<EndpointState>,
    /// Endpoint ids of every labelled compute container, running or not.
    pub computes: HashSet<String>,
    /// Tenant ids on the storage controller.
    pub tenants: HashSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Pending/provisioning for too long: fail it and tear down what it built.
    FailStale(DatabaseId),
    /// Stuck in `deleting` (the delete job was lost): run the delete again.
    RetryDelete(DatabaseId),
    /// A ready database's compute container is gone: start a new one.
    RecreateCompute(String),
    /// A reset stored a new password but didn't apply it: reset steps 2–4.
    ApplyPending(String),
    /// A compute with no endpoint row.
    RemoveCompute(String),
    /// An endpoint row whose database is failed or deleted: a teardown that failed.
    RetryTeardown(String),
    /// A tenant no endpoint row records. Logged, never deleted: it may hold someone's data.
    ReportUnknownTenant(String),
    /// An endpoint row no database links to. Logged only.
    ReportEndpointWithoutDatabase(String),
}

impl Action {
    /// Execution order: the variants' declaration order.
    fn rank(&self) -> u8 {
        match self {
            Action::FailStale(_) => 0,
            Action::RetryDelete(_) => 1,
            Action::RecreateCompute(_) => 2,
            Action::ApplyPending(_) => 3,
            Action::RemoveCompute(_) => 4,
            Action::RetryTeardown(_) => 5,
            Action::ReportUnknownTenant(_) => 6,
            Action::ReportEndpointWithoutDatabase(_) => 7,
        }
    }

    fn subject(&self) -> String {
        match self {
            Action::FailStale(id) | Action::RetryDelete(id) => id.to_string(),
            Action::RecreateCompute(s)
            | Action::ApplyPending(s)
            | Action::RemoveCompute(s)
            | Action::RetryTeardown(s)
            | Action::ReportUnknownTenant(s)
            | Action::ReportEndpointWithoutDatabase(s) => s.clone(),
        }
    }
}

/// What to do about `s`, sorted by variant, then by id.
pub fn plan(s: &Snapshot) -> Vec<Action> {
    let mut actions = Vec::new();
    for db in &s.databases {
        match db.status {
            DatabaseStatus::Pending | DatabaseStatus::Provisioning
                if db.age_secs > STALE_PROVISION_SECS =>
            {
                actions.push(Action::FailStale(db.id));
            }
            DatabaseStatus::Deleting if db.age_secs > STALE_DELETE_SECS => {
                actions.push(Action::RetryDelete(db.id));
            }
            DatabaseStatus::Ready => {
                if let Some(ep) = &db.endpoint_id
                    && !s.computes.contains(ep)
                {
                    actions.push(Action::RecreateCompute(ep.clone()));
                }
            }
            _ => {}
        }
    }
    for ep in &s.endpoints {
        match ep.database_status {
            None => actions.push(Action::ReportEndpointWithoutDatabase(ep.id.clone())),
            Some(DatabaseStatus::Ready) if ep.needs_reconfigure => {
                actions.push(Action::ApplyPending(ep.id.clone()));
            }
            Some(DatabaseStatus::Failed | DatabaseStatus::Deleted) => {
                actions.push(Action::RetryTeardown(ep.id.clone()));
            }
            Some(_) => {}
        }
    }
    let endpoints: HashSet<&str> = s.endpoints.iter().map(|e| e.id.as_str()).collect();
    for compute in &s.computes {
        if !endpoints.contains(compute.as_str()) {
            actions.push(Action::RemoveCompute(compute.clone()));
        }
    }
    let known_tenants: HashSet<&str> = s.endpoints.iter().map(|e| e.tenant_id.as_str()).collect();
    for tenant in &s.tenants {
        if !known_tenants.contains(tenant.as_str()) {
            actions.push(Action::ReportUnknownTenant(tenant.clone()));
        }
    }
    actions.sort_by_cached_key(|a| (a.rank(), a.subject()));
    actions
}

/// A running provision must stop (and clean up) once a delete has begun.
pub fn should_abort_provision(status: DatabaseStatus) -> bool {
    matches!(status, DatabaseStatus::Deleting | DatabaseStatus::Deleted)
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// One reconcile pass. `Err` only when observing fails; each action's error is logged
    /// and the next action still runs.
    pub async fn reconcile_once(&self) -> Result<(), String> {
        let snapshot = self.observe().await.map_err(|e| format!("{e:#}"))?;
        for action in plan(&snapshot) {
            info!(?action, "reconcile");
            if let Err(err) = self.execute(&action).await {
                warn!(?action, "reconcile action failed: {err}");
            }
        }
        Ok(())
    }

    /// In this order: rows are written before their compute or tenant exists, so anything
    /// seen first already has its row by the time the rows are read.
    async fn observe(&self) -> anyhow::Result<Snapshot> {
        let computes = self.computes.list().await.context("listing computes")?;
        let tenants = self
            .storage
            .list_tenant_ids()
            .await
            .context("listing tenants")?;
        let databases = self
            .metadata
            .list_live_for_reconcile()
            .await
            .context("listing databases")?;
        let endpoints = self
            .metadata
            .list_endpoints_for_reconcile()
            .await
            .context("listing endpoints")?;
        Ok(Snapshot {
            databases,
            endpoints,
            computes: computes.into_iter().collect(),
            tenants: tenants.iter().map(|t| t.as_str().to_string()).collect(),
        })
    }

    async fn execute(&self, action: &Action) -> Result<(), String> {
        match action {
            Action::FailStale(id) => {
                // Re-checked at write time: only a row that really is stale is failed.
                let failed = self
                    .metadata
                    .fail_if_stale(*id, STALE_PROVISION_SECS)
                    .await
                    .map_err(|e| e.to_string())?;
                if !failed {
                    return Ok(());
                }
                let record = self
                    .metadata
                    .get_database(*id)
                    .await
                    .map_err(|e| e.to_string())?;
                match record.and_then(|r| r.endpoint_id) {
                    Some(ep) => self.retry_teardown(&ep).await,
                    None => Ok(()),
                }
            }
            Action::RetryDelete(id) => self.handle_delete(*id).await,
            Action::RecreateCompute(ep) => {
                // The hash first: the new compute's first spec request must already pass.
                let token = new_compute_token();
                self.metadata
                    .replace_compute_token(ep, &sha256_hex(&token))
                    .await
                    .map_err(|e| format!("replacing the compute token: {e}"))?;
                self.computes
                    .create(ep, &token)
                    .await
                    .map_err(|e| format!("starting compute: {e:#}"))
            }
            // After a RecreateCompute in the same pass, the push can reach the new container
            // before compute_ctl listens and fail. That heals itself: the flag stays set, the
            // next pass retries, and meanwhile the new compute boots with the current spec.
            Action::ApplyPending(ep) => self.apply_pending(ep).await.map_err(|e| format!("{e:#}")),
            Action::RemoveCompute(ep) => self
                .computes
                .delete(ep)
                .await
                .map_err(|e| format!("removing compute: {e:#}")),
            Action::RetryTeardown(ep) => self.retry_teardown(ep).await,
            Action::ReportUnknownTenant(tenant) => {
                warn!(
                    tenant_id = %tenant,
                    "storage-controller tenant has no endpoint row; keeping it. Restore the \
                     metadata, or delete the tenant by hand if it's disposable (`just \
                     sweep-dbs` only removes tenants that have an endpoint row)"
                );
                Ok(())
            }
            Action::ReportEndpointWithoutDatabase(ep) => {
                warn!(
                    endpoint_id = %ep,
                    "endpoint row has no database; keeping it. In local dev, `just sweep-dbs` \
                     removes every database, endpoint and their tenants"
                );
                Ok(())
            }
        }
    }

    /// Teardown for an endpoint row, if it's still there.
    async fn retry_teardown(&self, endpoint_id: &str) -> Result<(), String> {
        let endpoint = self
            .metadata
            .neon_endpoint(endpoint_id)
            .await
            .map_err(|e| e.to_string())?;
        match endpoint {
            Some(endpoint) => self.teardown(&endpoint.id, &endpoint.tenant_id).await,
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use cheladb_neon::NeonId;

    use super::*;
    use crate::fakes::{FakeWorker, Journal, entries, position, worker};

    const EP: &str = "ep-0123456789ab";
    const EP2: &str = "ep-ba9876543210";
    const TENANT: &str = "0123456789abcdef0123456789abcdef";

    fn db(status: DatabaseStatus, endpoint_id: Option<&str>, age_secs: i64) -> LiveDatabase {
        LiveDatabase {
            id: DatabaseId::new(),
            status,
            endpoint_id: endpoint_id.map(str::to_string),
            age_secs,
        }
    }

    fn endpoint(id: &str, db: Option<&LiveDatabase>, needs_reconfigure: bool) -> EndpointState {
        EndpointState {
            id: id.to_string(),
            tenant_id: TENANT.to_string(),
            database_id: db.map(|d| d.id),
            database_status: db.map(|d| d.status),
            needs_reconfigure,
        }
    }

    /// `db` with its endpoint `EP`, its compute and its tenant.
    fn linked(db: LiveDatabase, needs_reconfigure: bool) -> Snapshot {
        let ep = endpoint(EP, Some(&db), needs_reconfigure);
        Snapshot {
            databases: vec![db],
            endpoints: vec![ep],
            computes: [EP.to_string()].into(),
            tenants: [TENANT.to_string()].into(),
        }
    }

    #[test]
    fn a_stale_provision_fails() {
        for status in [DatabaseStatus::Pending, DatabaseStatus::Provisioning] {
            let row = db(status, None, STALE_PROVISION_SECS);
            assert!(
                plan(&Snapshot {
                    databases: vec![row],
                    ..Default::default()
                })
                .is_empty()
            );
            let row = db(status, None, STALE_PROVISION_SECS + 1);
            let id = row.id;
            assert_eq!(
                plan(&Snapshot {
                    databases: vec![row],
                    ..Default::default()
                }),
                [Action::FailStale(id)]
            );
        }
    }

    #[test]
    fn a_lost_delete_is_retried_after_a_minute() {
        let row = db(DatabaseStatus::Deleting, Some(EP), STALE_DELETE_SECS);
        assert!(plan(&linked(row, false)).is_empty());
        let row = db(DatabaseStatus::Deleting, Some(EP), STALE_DELETE_SECS + 1);
        let id = row.id;
        assert_eq!(plan(&linked(row, false)), [Action::RetryDelete(id)]);
    }

    #[test]
    fn a_ready_database_without_its_compute_gets_a_new_one() {
        let mut s = linked(db(DatabaseStatus::Ready, Some(EP), 3600), false);
        s.computes.clear();
        assert_eq!(plan(&s), [Action::RecreateCompute(EP.to_string())]);
    }

    #[test]
    fn a_pending_reset_is_applied_only_to_a_ready_database() {
        let s = linked(db(DatabaseStatus::Ready, Some(EP), 3600), true);
        assert_eq!(plan(&s), [Action::ApplyPending(EP.to_string())]);
        let s = linked(db(DatabaseStatus::Provisioning, Some(EP), 5), true);
        assert!(plan(&s).is_empty());
    }

    #[test]
    fn the_endpoint_of_a_failed_or_deleted_database_is_torn_down() {
        for status in [DatabaseStatus::Failed, DatabaseStatus::Deleted] {
            let s = linked(db(status, Some(EP), 3600), false);
            assert_eq!(
                plan(&s),
                [Action::RetryTeardown(EP.to_string())],
                "{status:?}"
            );
        }
    }

    #[test]
    fn an_endpoint_without_a_database_is_reported() {
        let s = Snapshot {
            endpoints: vec![endpoint(EP, None, false)],
            computes: [EP.to_string()].into(),
            tenants: [TENANT.to_string()].into(),
            ..Default::default()
        };
        assert_eq!(
            plan(&s),
            [Action::ReportEndpointWithoutDatabase(EP.to_string())]
        );
    }

    #[test]
    fn a_compute_without_an_endpoint_row_is_removed() {
        let mut s = linked(db(DatabaseStatus::Ready, Some(EP), 3600), false);
        s.computes.insert(EP2.to_string());
        assert_eq!(plan(&s), [Action::RemoveCompute(EP2.to_string())]);
    }

    // Deleting storage on a guess can't be undone.
    #[test]
    fn unknown_tenants_are_reported_never_removed() {
        let other = "fedcba9876543210fedcba9876543210";
        let mut s = linked(db(DatabaseStatus::Ready, Some(EP), 3600), false);
        s.tenants.insert(other.to_string());
        assert_eq!(plan(&s), [Action::ReportUnknownTenant(other.to_string())]);
        let s = Snapshot {
            tenants: [TENANT.to_string()].into(),
            ..Default::default()
        };
        assert_eq!(plan(&s), [Action::ReportUnknownTenant(TENANT.to_string())]);
    }

    // Docker's `unless-stopped` owns restarts; an exited container is still listed.
    #[test]
    fn a_live_endpoint_with_its_compute_needs_nothing() {
        assert!(plan(&linked(db(DatabaseStatus::Ready, Some(EP), 3600), false)).is_empty());
    }

    #[test]
    fn a_fresh_provision_is_left_alone() {
        for status in [DatabaseStatus::Pending, DatabaseStatus::Provisioning] {
            // Before step 2: no endpoint yet.
            assert!(
                plan(&Snapshot {
                    databases: vec![db(status, None, 5)],
                    ..Default::default()
                })
                .is_empty()
            );
            // Recorded, tenant made, compute not started yet.
            let mut s = linked(db(status, Some(EP), 5), false);
            s.computes.clear();
            assert!(plan(&s).is_empty(), "{status:?}");
            // Compute started.
            assert!(plan(&linked(db(status, Some(EP), 5), false)).is_empty());
        }
    }

    #[test]
    fn actions_are_sorted() {
        let stale = db(DatabaseStatus::Provisioning, None, STALE_PROVISION_SECS + 1);
        let ready = db(DatabaseStatus::Ready, Some(EP), 3600);
        let stale_id = stale.id;
        let s = Snapshot {
            endpoints: vec![endpoint(EP, Some(&ready), true)],
            databases: vec![ready, stale],
            computes: [EP2.to_string()].into(),
            tenants: ["fedcba9876543210fedcba9876543210".to_string()].into(),
        };
        assert_eq!(
            plan(&s),
            [
                Action::FailStale(stale_id),
                Action::RecreateCompute(EP.to_string()),
                Action::ApplyPending(EP.to_string()),
                Action::RemoveCompute(EP2.to_string()),
                Action::ReportUnknownTenant("fedcba9876543210fedcba9876543210".to_string()),
            ]
        );
    }

    #[test]
    fn provisioning_aborts_once_delete_has_started() {
        assert!(should_abort_provision(DatabaseStatus::Deleting));
        assert!(should_abort_provision(DatabaseStatus::Deleted));
        assert!(!should_abort_provision(DatabaseStatus::Provisioning));
        assert!(!should_abort_provision(DatabaseStatus::Pending));
    }

    // --- Execution, over the fakes -------------------------------------------------------

    fn pos(journal: &Journal, prefix: &str) -> usize {
        position(journal, prefix)
            .unwrap_or_else(|| panic!("{prefix:?} not in journal {:?}", entries(journal)))
    }

    async fn provision(w: &FakeWorker, journal: &Journal) -> (DatabaseId, String) {
        let id = w.metadata.insert_database("app", DatabaseStatus::Pending);
        w.handle_provision(id, "app").await.unwrap();
        let ep = w.metadata.database(id).endpoint_id.unwrap();
        journal.lock().unwrap().clear();
        (id, ep)
    }

    fn token_hash(w: &FakeWorker, ep: &str) -> String {
        w.metadata.state().endpoints[ep]
            .endpoint
            .compute_token_sha256
            .clone()
    }

    #[tokio::test]
    async fn reconcile_recreates_a_missing_compute_with_a_fresh_token() {
        let (w, journal) = worker();
        let (_id, ep) = provision(&w, &journal).await;
        let old_hash = token_hash(&w, &ep);
        w.computes.remove(&ep);

        w.reconcile_once().await.unwrap();

        assert!(
            pos(&journal, "metadata.replace_compute_token")
                < pos(&journal, &format!("compute.create {ep}"))
        );
        let token = w.computes.token_of(&ep).unwrap();
        assert_eq!(sha256_hex(&token), token_hash(&w, &ep));
        assert_ne!(token_hash(&w, &ep), old_hash);
    }

    #[tokio::test]
    async fn reconcile_applies_a_reset_that_failed_to_apply() {
        let (w, journal) = worker();
        let (id, ep) = provision(&w, &journal).await;
        *w.pusher.fail.lock().unwrap() = Some("compute_ctl said 500".into());
        w.reset_password(id).await.unwrap_err();
        assert!(w.metadata.state().endpoints[&ep].needs_reconfigure);
        *w.pusher.fail.lock().unwrap() = None;
        journal.lock().unwrap().clear();

        w.reconcile_once().await.unwrap();

        assert!(!w.metadata.state().endpoints[&ep].needs_reconfigure);
        assert_eq!(w.pusher.pushed.lock().unwrap().len(), 1);
        // Serialized with resets, like theirs.
        let lock = position(&journal, "metadata.lock_endpoint").expect("locked");
        assert!(lock < position(&journal, "pusher.push").unwrap());
    }

    // Another apply_pending holds the endpoint's lock past the lock timeout: the pass gives
    // up on that endpoint, leaves its flag for the next pass, and carries on.
    #[tokio::test(start_paused = true)]
    async fn reconcile_moves_on_when_the_endpoint_lock_times_out() {
        let (w, journal) = worker();
        let (id, ep) = provision(&w, &journal).await;
        *w.pusher.fail.lock().unwrap() = Some("compute_ctl said 500".into());
        w.reset_password(id).await.unwrap_err();
        *w.pusher.fail.lock().unwrap() = None;
        w.computes.insert(EP2);
        let held = w.metadata.lock_endpoint(&ep).await.unwrap();
        journal.lock().unwrap().clear();

        tokio::time::timeout(std::time::Duration::from_secs(60), w.reconcile_once())
            .await
            .expect("the pass waited on the lock forever")
            .unwrap();

        assert!(w.metadata.state().endpoints[&ep].needs_reconfigure);
        assert!(w.pusher.pushed.lock().unwrap().is_empty());
        assert!(position(&journal, &format!("compute.delete {EP2}")).is_some());
        drop(held);
        w.reconcile_once().await.unwrap();
        assert!(!w.metadata.state().endpoints[&ep].needs_reconfigure);
    }

    #[tokio::test]
    async fn reconcile_fails_a_stale_provision_and_tears_it_down() {
        let (w, journal) = worker();
        let (id, ep) = provision(&w, &journal).await;
        let tenant = w.metadata.state().endpoints[&ep].endpoint.tenant_id.clone();
        {
            let mut state = w.metadata.state();
            state.databases.get_mut(&id).unwrap().status = DatabaseStatus::Provisioning;
            state.ages.insert(id, STALE_PROVISION_SECS + 1);
        }

        w.reconcile_once().await.unwrap();

        assert_eq!(w.metadata.database(id).status, DatabaseStatus::Failed);
        assert!(w.metadata.endpoint_ids().is_empty());
        assert!(!w.storage.has_tenant(&tenant));
        assert_eq!(w.computes.list().await.unwrap(), Vec::<String>::new());
    }

    // Review Focus 3.
    #[tokio::test]
    async fn reconcile_retries_a_failed_teardown() {
        let (w, journal) = worker();
        // The timeline fails, and so does the teardown after it: the endpoint row stays.
        *w.storage.fail.lock().unwrap() = Some("create_timeline");
        *w.computes.fail_delete.lock().unwrap() = Some("docker is stuck".into());
        let id = w.metadata.insert_database("app", DatabaseStatus::Pending);
        w.handle_provision(id, "app").await.unwrap();
        assert_eq!(w.metadata.database(id).status, DatabaseStatus::Failed);
        let [ep] = <[String; 1]>::try_from(w.metadata.endpoint_ids()).unwrap();
        let tenant = w.metadata.state().endpoints[&ep].endpoint.tenant_id.clone();
        assert!(w.storage.has_tenant(&tenant));
        *w.storage.fail.lock().unwrap() = None;

        // Still failing: the row stays for the next pass.
        journal.lock().unwrap().clear();
        w.reconcile_once().await.unwrap();
        assert!(position(&journal, &format!("compute.delete {ep}")).is_some());
        assert_eq!(w.metadata.endpoint_ids(), std::slice::from_ref(&ep));

        *w.computes.fail_delete.lock().unwrap() = None;
        w.reconcile_once().await.unwrap();
        assert!(w.metadata.endpoint_ids().is_empty());
        assert!(!w.storage.has_tenant(&tenant));
        assert_eq!(w.metadata.database(id).status, DatabaseStatus::Failed);
    }

    #[tokio::test]
    async fn one_failing_action_does_not_stop_the_rest() {
        let (w, journal) = worker();
        let (_id, ep) = provision(&w, &journal).await;
        w.computes.remove(&ep);
        *w.computes.fail_create.lock().unwrap() = Some("image missing".into());
        // An orphan compute, and an unknown tenant.
        w.computes.insert(EP2);
        w.storage.tenants.lock().unwrap().insert(NeonId::new());

        w.reconcile_once().await.unwrap();

        assert!(
            pos(&journal, &format!("compute.create {ep}"))
                < pos(&journal, &format!("compute.delete {EP2}"))
        );
        assert_eq!(w.computes.list().await.unwrap(), Vec::<String>::new());
        assert_eq!(w.storage.tenants.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn reconcile_errs_when_it_cannot_observe() {
        let (w, _journal) = worker();
        w.metadata.state().fail = Some("list_endpoints_for_reconcile");
        assert!(w.reconcile_once().await.is_err());
    }
}
