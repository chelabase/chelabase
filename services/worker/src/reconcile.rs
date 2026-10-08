//! Reconcile: compare metadata rows with the computes and tenants that actually exist, and
//! repair what a lost job or a failed step left behind.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::Context;
use chelabase_metadata::{
    BranchKind, BranchStatus, DatabaseStatus, EndpointState, LiveBranch, LiveDatabase,
};
use chelabase_neon::{ComputeBackend, NeonId, StorconError};
use chelabase_types::{BranchId, DatabaseId};
use reqwest::StatusCode;
use tracing::{info, warn};

use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::{COMPUTE_SETTINGS_VERSION, Worker};

pub const STALE_PROVISION_SECS: i64 = 600;
pub const STALE_DELETE_SECS: i64 = 60;
/// How many ready branches each pass checks for the `chelabase` schema (one `admin_sql` each)
/// and resyncs the login roles of.
pub const SCHEMA_CHECKS_PER_PASS: usize = 20;
/// How long a database row may stay `enabling` before reconcile runs its enable bootstrap again
/// (the orchestrator's publish has no reply, so a lost one would leave it there). Longer than
/// [`crate::data_api::ENABLE_DEADLINE`], so a slow enable isn't started twice.
pub const ENABLING_RETRY_SECS: u64 = 150;
/// R3: a rehearsal branch older than this whose merge isn't rehearsing is torn down.
pub const STALE_REHEARSAL_SECS: i64 = 30 * 60;

/// One observation. Taken in the order computes, tenants, databases, endpoints: rows are
/// written before their compute or tenant exists, so a fresh resource never looks orphaned.
#[derive(Debug, Default)]
pub struct Snapshot {
    pub databases: Vec<LiveDatabase>,
    /// Every branch that isn't deleted, read alongside the databases.
    pub branches: Vec<LiveBranch>,
    pub endpoints: Vec<EndpointState>,
    /// Endpoint ids of every labelled compute container, running or not.
    pub computes: HashSet<String>,
    /// Tenant ids on the storage controller.
    pub tenants: HashSet<String>,
    /// Each tenant's timelines, listed with the tenants (so before the branches are read). A
    /// tenant whose listing failed is absent, and none of its timelines is reported.
    pub tenant_timelines: HashMap<String, HashSet<String>>,
    /// The `pitr_interval` of each ready database's tenant. Absent when it was never set (or
    /// couldn't be read), which counts as drift.
    pub pitr: HashMap<String, Duration>,
    /// Branches whose data API needs its bootstrap: no public key or authenticator password
    /// (from the metadata), or no `chelabase.schema_version` (from this pass's checks).
    pub needs_data_api: HashSet<BranchId>,
    /// `(branch, database)` pairs whose `chelabase` machinery failed this pass's check: only that
    /// database is bootstrapped (the branch as a whole is in `needs_data_api`).
    pub needs_data_api_databases: HashSet<(BranchId, String)>,
    /// `(branch, database)` rows that have been `enabling` for [`ENABLING_RETRY_SECS`]: their
    /// enable bootstrap runs again.
    pub stale_enabling: Vec<(BranchId, String)>,
    /// Branches in this pass's health-check batch (ready, `normal`, with a compute): their
    /// login roles are resynced.
    pub resync: HashSet<BranchId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Pending/provisioning for too long: fail it and tear down what it built.
    FailStale(DatabaseId),
    /// A branch (not main) pending/provisioning for too long: fail it and tear it down.
    FailStaleBranch(BranchId),
    /// Stuck in `deleting` (the delete job was lost): run the delete again.
    RetryDelete(DatabaseId),
    /// A branch (not main, database not deleting) stuck in `deleting`: run its delete again.
    RetryBranchDelete(BranchId),
    /// A branch `resetting` for too long: finish the reset or restore.
    FinishSwitch(BranchId),
    /// `(branch, endpoint)`: a ready `normal` branch's compute container is gone (main's
    /// included): start a new one.
    RecreateBranchCompute(BranchId, String),
    /// `(branch, endpoint)`: a ready `normal` branch's compute was created with settings older
    /// than [`COMPUTE_SETTINGS_VERSION`] (R11): remove it and create a new one.
    UpgradeCompute(BranchId, String),
    /// A reset stored a new password but didn't apply it: reset steps 2–4.
    ApplyPending(String),
    /// A ready `normal` branch with its compute lacks its public key, its authenticator
    /// password or the `chelabase` schema: run the data API bootstrap.
    /// `database` is `None` for the whole branch (every served database, the keys and the
    /// password), or one database whose machinery is broken.
    BootstrapDataApi {
        branch_id: BranchId,
        database: Option<String>,
    },
    /// A database left `enabling` past [`ENABLING_RETRY_SECS`]: run its enable bootstrap again.
    RetryEnableDataApi(BranchId, String),
    /// `(branch, endpoint)` in this pass's health-check batch: resync its login roles (it
    /// catches a DDL forward that was lost). A failure is logged, and retried at its next turn.
    Resync(BranchId, String),
    /// A compute with no endpoint row.
    RemoveCompute(String),
    /// An endpoint row whose branch is failed or deleted: a teardown that failed.
    RetryTeardown(String),
    /// A static compute's endpoint (a base read, step 6g) older than
    /// [`crate::schema_base::STALE_STATIC_COMPUTE_SECS`]: its compute and row go (R3).
    RemoveStaticCompute(String),
    /// A merge's rehearsal branch (step 6g) unchanged for [`STALE_REHEARSAL_SECS`]: torn down
    /// unless its merge is still rehearsing (R3).
    RemoveRehearsal(BranchId),
    /// A ready database's tenant has no `pitr_interval`, or not its `history_window`.
    PatchHistoryWindow(DatabaseId),
    /// A branch a restore failed while its detach was unresolved: it keeps its timeline,
    /// endpoint and pending timeline (a detach may still be running on it). Logged; its
    /// endpoint isn't torn down.
    ReportStuckRestore(BranchId),
    /// A tenant no endpoint row records. Logged, never deleted: it may hold someone's data.
    ReportUnknownTenant(String),
    /// `(tenant, timeline)`: a timeline of a live database's tenant that no branch records
    /// (as its timeline, pending or superseded). Logged, never deleted.
    ReportUnknownTimeline(String, String),
    /// An endpoint row no branch links to. Logged only.
    ReportEndpointWithoutDatabase(String),
}

impl Action {
    /// Execution order: the variants' declaration order.
    fn rank(&self) -> u8 {
        match self {
            Action::FailStale(_) => 0,
            Action::FailStaleBranch(_) => 1,
            Action::RetryDelete(_) => 2,
            Action::RetryBranchDelete(_) => 3,
            Action::FinishSwitch(_) => 4,
            Action::RecreateBranchCompute(..) => 5,
            Action::UpgradeCompute(..) => 6,
            Action::ApplyPending(_) => 7,
            Action::BootstrapDataApi { .. } => 8,
            Action::RetryEnableDataApi(..) => 9,
            Action::Resync(..) => 10,
            Action::RemoveCompute(_) => 11,
            Action::RetryTeardown(_) => 12,
            Action::RemoveStaticCompute(_) => 13,
            Action::RemoveRehearsal(_) => 14,
            Action::PatchHistoryWindow(_) => 15,
            Action::ReportStuckRestore(_) => 16,
            Action::ReportUnknownTenant(_) => 17,
            Action::ReportUnknownTimeline(..) => 18,
            Action::ReportEndpointWithoutDatabase(_) => 19,
        }
    }

    fn subject(&self) -> String {
        match self {
            Action::FailStale(id) | Action::RetryDelete(id) | Action::PatchHistoryWindow(id) => {
                id.to_string()
            }
            Action::FailStaleBranch(id)
            | Action::RetryBranchDelete(id)
            | Action::RemoveRehearsal(id)
            | Action::FinishSwitch(id)
            | Action::ReportStuckRestore(id)
            | Action::BootstrapDataApi { branch_id: id, .. } => id.to_string(),
            Action::RetryEnableDataApi(id, name) => format!("{id}/{name}"),
            Action::ReportUnknownTimeline(tenant, timeline) => format!("{tenant}/{timeline}"),
            Action::RecreateBranchCompute(_, s)
            | Action::UpgradeCompute(_, s)
            | Action::Resync(_, s)
            | Action::ApplyPending(s)
            | Action::RemoveCompute(s)
            | Action::RetryTeardown(s)
            | Action::RemoveStaticCompute(s)
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
            // An unset `pitr_interval` counts as drift: that's how databases made before
            // branching pick up the default window.
            DatabaseStatus::Ready => {
                if let Some(tenant) = &db.neon_tenant_id
                    && s.tenants.contains(tenant)
                    && s.pitr.get(tenant) != Some(&db.history_window)
                {
                    actions.push(Action::PatchHistoryWindow(db.id));
                }
            }
            _ => {}
        }
    }
    // An endpoint missing from the list counts as current: only a known old one is upgraded.
    let settings: HashMap<&str, i32> = s
        .endpoints
        .iter()
        .map(|e| (e.id.as_str(), e.compute_settings_version))
        .collect();
    let deleting: HashSet<DatabaseId> = s
        .databases
        .iter()
        .filter(|d| d.status == DatabaseStatus::Deleting)
        .map(|d| d.id)
        .collect();
    for b in &s.branches {
        // A rehearsal outlives its job only when the job crashed or its teardown failed. Until
        // it's old it's its job's, whatever its status (T14 M7): never a normal branch's rules.
        if b.kind == BranchKind::Rehearsal {
            if b.status != BranchStatus::Deleting && b.age_secs > STALE_REHEARSAL_SECS {
                actions.push(Action::RemoveRehearsal(b.id));
            }
            continue;
        }
        // Main's provisioning and deletion are its database's (the rows above).
        let is_main = b.id == BranchId::from(b.database_id);
        match b.status {
            BranchStatus::Pending | BranchStatus::Provisioning
                if !is_main && b.age_secs > STALE_PROVISION_SECS =>
            {
                actions.push(Action::FailStaleBranch(b.id));
            }
            BranchStatus::Resetting if b.age_secs > STALE_PROVISION_SECS => {
                actions.push(Action::FinishSwitch(b.id));
            }
            BranchStatus::Deleting
                if !is_main
                    && !deleting.contains(&b.database_id)
                    && b.age_secs > STALE_DELETE_SECS =>
            {
                actions.push(Action::RetryBranchDelete(b.id));
            }
            BranchStatus::Failed if b.pending_timeline_id.is_some() => {
                actions.push(Action::ReportStuckRestore(b.id));
            }
            // Backups have no compute. Recreating a compute runs the data API bootstrap and
            // the resync too.
            BranchStatus::Ready if b.kind == BranchKind::Normal => match &b.endpoint_id {
                Some(ep) if !s.computes.contains(ep) => {
                    actions.push(Action::RecreateBranchCompute(b.id, ep.clone()));
                }
                Some(ep)
                    if settings
                        .get(ep.as_str())
                        .is_some_and(|v| *v < COMPUTE_SETTINGS_VERSION) =>
                {
                    actions.push(Action::UpgradeCompute(b.id, ep.clone()));
                }
                Some(ep) => {
                    if s.needs_data_api.contains(&b.id) {
                        actions.push(Action::BootstrapDataApi {
                            branch_id: b.id,
                            database: None,
                        });
                    } else {
                        let mut broken: Vec<&String> = s
                            .needs_data_api_databases
                            .iter()
                            .filter(|(id, _)| *id == b.id)
                            .map(|(_, name)| name)
                            .collect();
                        broken.sort();
                        for name in broken {
                            actions.push(Action::BootstrapDataApi {
                                branch_id: b.id,
                                database: Some(name.clone()),
                            });
                        }
                    }
                    for (_, name) in s.stale_enabling.iter().filter(|(id, _)| *id == b.id) {
                        actions.push(Action::RetryEnableDataApi(b.id, name.clone()));
                    }
                    if s.resync.contains(&b.id) {
                        actions.push(Action::Resync(b.id, ep.clone()));
                    }
                }
                None => {}
            },
            _ => {}
        }
    }
    // A failed restore's branch still holds its data: its endpoint isn't a failed create's.
    let kept: HashSet<BranchId> = s
        .branches
        .iter()
        .filter(|b| b.status == BranchStatus::Failed && b.pending_timeline_id.is_some())
        .map(|b| b.id)
        .collect();
    for ep in &s.endpoints {
        if ep.branch_id.is_some_and(|b| kept.contains(&b)) {
            continue;
        }
        // A base read's static compute (step 6g) is linked to no branch; one still reading is
        // left alone (R3).
        if ep.branch_id.is_none() && !ep.routable && ep.static_lsn.is_some() {
            if ep.age_secs > crate::schema_base::STALE_STATIC_COMPUTE_SECS {
                actions.push(Action::RemoveStaticCompute(ep.id.clone()));
            }
            continue;
        }
        match ep.branch_status {
            None => actions.push(Action::ReportEndpointWithoutDatabase(ep.id.clone())),
            Some(BranchStatus::Ready) if ep.needs_reconfigure => {
                actions.push(Action::ApplyPending(ep.id.clone()));
            }
            Some(BranchStatus::Failed | BranchStatus::Deleted) => {
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
    // A timeline is known when any branch names it: as its timeline, or as the pending or
    // superseded one of a reset or restore in flight.
    let known_timelines: HashSet<&str> = s
        .branches
        .iter()
        .flat_map(|b| {
            [
                b.timeline_id.as_deref(),
                b.pending_timeline_id.as_deref(),
                b.superseded_timeline_id.as_deref(),
            ]
        })
        .flatten()
        .collect();
    let live_tenants: HashSet<&str> = s
        .databases
        .iter()
        .filter(|d| d.status != DatabaseStatus::Deleting)
        .filter_map(|d| d.neon_tenant_id.as_deref())
        .collect();
    for tenant in live_tenants {
        for timeline in s.tenant_timelines.get(tenant).into_iter().flatten() {
            if !known_timelines.contains(timeline.as_str()) {
                actions.push(Action::ReportUnknownTimeline(
                    tenant.to_string(),
                    timeline.clone(),
                ));
            }
        }
    }
    actions.sort_by_cached_key(|a| (a.rank(), a.subject()));
    actions
}

/// The next `n` of `candidates` (sorted by id) after `after`, wrapping around: reconcile's
/// `chelabase` schema check covers every ready branch in turn, a few per pass.
pub fn schema_check_batch(
    candidates: &[BranchId],
    after: Option<BranchId>,
    n: usize,
) -> Vec<BranchId> {
    let start = after.map_or(0, |a| {
        candidates.partition_point(|c| c.as_uuid() <= a.as_uuid())
    });
    candidates[start..]
        .iter()
        .chain(&candidates[..start])
        .take(n)
        .copied()
        .collect()
}

/// What one pass's round-robin batch found.
#[derive(Debug, Default)]
struct RoundRobin {
    /// Branches whose data API machinery is broken.
    missing_data_api: Vec<BranchId>,
    /// `(branch, database)`: one served database whose `chelabase` machinery is broken.
    missing_databases: HashSet<(BranchId, String)>,
    /// Every branch of the batch: its login roles are resynced.
    resync: HashSet<BranchId>,
}

/// A running provision must stop (and clean up) once a delete has begun.
pub fn should_abort_provision(status: DatabaseStatus) -> bool {
    matches!(status, DatabaseStatus::Deleting | DatabaseStatus::Deleted)
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// R3: hands merge jobs back through `publish`: a `Resume` for each merge in a working
    /// status whose claim went silent for [`crate::merge::STALE_MERGE_CLAIM`] (its worker
    /// crashed, or its job was lost), and a `Finish` for each after-merge choice still waiting
    /// [`crate::merge::STALE_FINISH_REQUEST`] after its last write (its job was lost, T14 M8).
    /// Each one published is then recorded as handed back (T14 F3 M3: a failed publish records
    /// nothing, so the next pass tries again), and isn't handed back again while nothing ran it
    /// since, for [`crate::merge::REHAND_AFTER`] (T14 M10, F3 M2): a job queued behind a long one
    /// (one merge job at a time, P11) isn't duplicated every period. Returns the jobs published.
    /// A metadata failure is logged and hands back nothing more.
    pub async fn hand_back_merges<F, Fut>(&self, mut publish: F) -> Vec<chelabase_nats::MergeJob>
    where
        F: FnMut(chelabase_nats::MergeJob) -> Fut,
        Fut: std::future::Future<Output = Result<(), String>>,
    {
        use chelabase_nats::{MergeAfter, MergeJob};
        let mut due: Vec<(MergeJob, chelabase_metadata::MergeStatus)> = Vec::new();
        match self
            .metadata
            .stale_merge_claims(crate::merge::STALE_MERGE_CLAIM, crate::merge::REHAND_AFTER)
            .await
        {
            Ok(stale) => {
                for m in stale {
                    info!(merge_id = %m.id, status = m.status.as_str(), "handing a silent merge back");
                    due.push((MergeJob::Resume { merge_id: m.id }, m.status));
                }
            }
            Err(e) => warn!("listing silent merges: {e}"),
        }
        match self
            .metadata
            .stale_finish_requests(crate::merge::STALE_FINISH_REQUEST, crate::merge::REHAND_AFTER)
            .await
        {
            Ok(lost) => {
                for m in lost {
                    let after = match m.result.as_ref().and_then(|r| r["after"]["choice"].as_str()) {
                        Some("reset") => MergeAfter::Reset,
                        Some("delete") => MergeAfter::Delete,
                        _ => MergeAfter::Keep,
                    };
                    info!(merge_id = %m.id, "publishing a lost after-merge choice again");
                    due.push((MergeJob::Finish { merge_id: m.id, after }, m.status));
                }
            }
            Err(e) => warn!("listing lost after-merge choices: {e}"),
        }
        let mut published = Vec::new();
        for (job, status) in due {
            let (MergeJob::Resume { merge_id: id } | MergeJob::Finish { merge_id: id, .. }) = job
            else {
                continue;
            };
            if let Err(e) = publish(job.clone()).await {
                warn!(merge_id = %id, job = ?job, "re-queueing a merge job: {e}");
                continue;
            }
            if let Err(e) = self.metadata.mark_merge_handed_back(id, status).await {
                warn!(merge_id = %id, "recording a hand-back: {e}");
            }
            published.push(job);
        }
        published
    }

    /// One reconcile pass. `Err` only when observing fails; each action's error is logged
    /// and the next action still runs.
    ///
    /// Serial on purpose. Each recreated compute (`RecreateBranchCompute`, `FinishSwitch`) is
    /// waited for, up to `ready_timeout` (60 s in production), so after a mass outage one pass
    /// can take minutes; the 30 s timer then fires the next pass straight after.
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

    /// In this order: rows are written before their compute, tenant or timeline exists, so
    /// anything seen first already has its row by the time the rows are read. A timeline is
    /// dropped the other way round (cleared, then deleted), so one being dropped can be
    /// reported once as unknown; that's only a log line.
    async fn observe(&self) -> anyhow::Result<Snapshot> {
        let computes = self.computes.list().await.context("listing computes")?;
        let tenants = self
            .storage
            .list_tenant_ids()
            .await
            .context("listing tenants")?;
        // With the tenants, before the rows. One call per tenant, serially.
        let mut tenant_timelines = HashMap::new();
        for tenant in &tenants {
            match self.storage.list_timelines(tenant).await {
                Ok(timelines) => {
                    let timelines = timelines.iter().map(|t| t.as_str().to_string()).collect();
                    tenant_timelines.insert(tenant.as_str().to_string(), timelines);
                }
                // Deleted since it was listed.
                Err(StorconError::Rejected { status, .. }) if status == StatusCode::NOT_FOUND => {}
                Err(err) => {
                    warn!(tenant_id = %tenant, "listing timelines: {err:#}; not checking them this pass");
                }
            }
        }
        let databases = self
            .metadata
            .list_live_for_reconcile()
            .await
            .context("listing databases")?;
        let branches = self
            .metadata
            .list_live_branches_for_reconcile()
            .await
            .context("listing branches")?;
        let endpoints = self
            .metadata
            .list_endpoints_for_reconcile()
            .await
            .context("listing endpoints")?;
        let computes: HashSet<String> = computes.into_iter().collect();
        let mut needs_data_api: HashSet<BranchId> = self
            .metadata
            .branches_missing_data_api()
            .await
            .context("listing branches missing the data API")?
            .into_iter()
            .collect();
        let checked = self.round_robin_checks(&branches, &computes).await;
        needs_data_api.extend(checked.missing_data_api);
        let enabling = match self.metadata.enabling_databases().await {
            Ok(rows) => rows,
            Err(err) => {
                warn!("listing databases being enabled: {err}");
                Vec::new()
            }
        };
        let stale_enabling = self.stale_enabling_rows(enabling, std::time::Instant::now());
        let tenants: HashSet<String> = tenants.iter().map(|t| t.as_str().to_string()).collect();
        // Only the tenants `plan` may patch: ready databases' tenants on the controller.
        let mut pitr = HashMap::new();
        for tenant in databases
            .iter()
            .filter(|d| d.status == DatabaseStatus::Ready)
            .filter_map(|d| d.neon_tenant_id.as_deref())
            .filter(|t| tenants.contains(*t))
        {
            let Ok(id) = tenant.parse::<NeonId>() else {
                continue;
            };
            match self.storage.tenant_pitr_interval(&id).await {
                Ok(Some(interval)) => {
                    pitr.insert(tenant.to_string(), interval);
                }
                Ok(None) => {}
                // Left absent: `plan` patches it, which is harmless if it was in sync.
                Err(err) => warn!(tenant_id = %tenant, "reading pitr_interval: {err:#}"),
            }
        }
        Ok(Snapshot {
            databases,
            branches,
            endpoints,
            computes,
            tenants,
            tenant_timelines,
            pitr,
            needs_data_api,
            needs_data_api_databases: checked.missing_databases,
            stale_enabling,
            resync: checked.resync,
        })
    }

    /// The `enabling` rows that have been listed for [`ENABLING_RETRY_SECS`] as of `now`, in
    /// the order given. The first sighting starts a row's clock; a row due is re-armed for
    /// another period; a row that left the list is forgotten.
    pub(crate) fn stale_enabling_rows(
        &self,
        rows: Vec<(BranchId, String)>,
        now: std::time::Instant,
    ) -> Vec<(BranchId, String)> {
        let period = Duration::from_secs(ENABLING_RETRY_SECS);
        let mut seen = self
            .enabling_seen
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        seen.retain(|key, _| rows.contains(key));
        let mut stale = Vec::new();
        for row in rows {
            let since = *seen.entry(row.clone()).or_insert(now);
            if now.saturating_duration_since(since) >= period {
                seen.insert(row.clone(), now);
                stale.push(row);
            }
        }
        stale
    }

    /// The next [`SCHEMA_CHECKS_PER_PASS`] ready `normal` branches with a compute (by id, after
    /// the last pass's), checked for the data API's machinery ([`Worker::data_api_healthy`]);
    /// each is also planned a login-role resync. A failed check is logged and skipped.
    async fn round_robin_checks(
        &self,
        branches: &[LiveBranch],
        computes: &HashSet<String>,
    ) -> RoundRobin {
        let mut ready: Vec<(BranchId, &str)> = branches
            .iter()
            .filter(|b| b.status == BranchStatus::Ready && b.kind == BranchKind::Normal)
            .filter_map(|b| {
                let ep = b.endpoint_id.as_deref()?;
                computes.contains(ep).then_some((b.id, ep))
            })
            .collect();
        ready.sort_by_key(|(id, _)| id.as_uuid());
        let ids: Vec<BranchId> = ready.iter().map(|(id, _)| *id).collect();
        let batch = {
            let mut cursor = self
                .schema_check_cursor
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let batch = schema_check_batch(&ids, *cursor, SCHEMA_CHECKS_PER_PASS);
            if let Some(last) = batch.last() {
                *cursor = Some(*last);
            }
            batch
        };
        let mut checked = RoundRobin::default();
        for id in batch {
            let Some((_, ep)) = ready.iter().find(|(b, _)| *b == id) else {
                continue;
            };
            let served = match self.served_or_main(id, ep).await {
                Ok(served) => served,
                Err(err) => {
                    warn!(branch_id = %id, endpoint_id = %ep, "listing the served databases: {err:#}");
                    checked.resync.insert(id);
                    continue;
                }
            };
            let main_database = self.database_of(ep).await.ok();
            for database in served {
                match self
                    .data_api_healthy_in(ep, &database.name, &database.default_schema)
                    .await
                {
                    Ok(true) => {}
                    // Main's goes through the whole branch bootstrap: it also re-grants the
                    // secret keys' roles and ensures the public key.
                    Ok(false) if main_database.as_deref() == Some(database.name.as_str()) => {
                        checked.missing_data_api.push(id);
                    }
                    Ok(false) => {
                        checked.missing_databases.insert((id, database.name));
                    }
                    Err(err) => {
                        warn!(branch_id = %id, endpoint_id = %ep, database = %database.name, "checking the data API's machinery: {err:#}");
                    }
                }
            }
            checked.resync.insert(id);
        }
        checked
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
                let main = self
                    .metadata
                    .get_branch(BranchId::from(*id))
                    .await
                    .map_err(|e| e.to_string())?;
                match main.and_then(|b| b.endpoint_id) {
                    Some(ep) => self.retry_teardown(&ep).await,
                    None => Ok(()),
                }
            }
            Action::FailStaleBranch(id) => self.fail_stale_branch(*id).await,
            Action::RemoveRehearsal(id) => self.remove_stale_rehearsal(*id).await,
            Action::RetryDelete(id) => self.handle_delete(*id).await,
            // Refused (the refusal recorded, the branch left as it is) while it has children.
            Action::RetryBranchDelete(id) => self.handle_branch_delete(*id).await,
            Action::FinishSwitch(id) => self.finish_switch(*id).await,
            Action::RecreateBranchCompute(id, ep) => self.recreate_compute(*id, ep).await,
            // Serial like the re-creation above. A failure after the removal leaves no
            // container: the next pass plans `RecreateBranchCompute`.
            Action::UpgradeCompute(id, ep) => {
                info!(branch_id = %id, endpoint_id = %ep, "re-creating a compute with old settings");
                self.computes
                    .delete(ep)
                    .await
                    .map_err(|e| format!("removing compute: {e:#}"))?;
                self.recreate_compute(*id, ep).await
            }
            Action::BootstrapDataApi { branch_id, database } => {
                self.reconcile_data_api(*branch_id, database.as_deref()).await
            }
            Action::RetryEnableDataApi(branch_id, name) => {
                let request = chelabase_nats::DataApiDatabaseRequest {
                    branch_id: *branch_id,
                    name: name.clone(),
                    remove: false,
                    grant_existing: false,
                };
                tokio::time::timeout(
                    crate::data_api::ENABLE_DEADLINE,
                    self.enable_data_api(&request),
                )
                .await
                .map_err(|_| format!("enabling {name}: timed out"))
            }
            Action::Resync(id, _) => self.resync(*id).await,
            // After a RecreateBranchCompute in the same pass, the push can reach the new container
            // before compute_ctl listens and fail. That heals itself: the flag stays set, the
            // next pass retries, and meanwhile the new compute boots with the current spec.
            Action::ApplyPending(ep) => self.apply_pending(ep).await.map_err(|e| format!("{e:#}")),
            Action::RemoveCompute(ep) if !self.remove_orphans => {
                info!(
                    endpoint_id = %ep,
                    "compute has no endpoint row; keeping it (CHELABASE_WORKER_REMOVE_ORPHANS is off)"
                );
                Ok(())
            }
            Action::RemoveCompute(ep) => self
                .computes
                .delete(ep)
                .await
                .map_err(|e| format!("removing compute: {e:#}")),
            Action::RetryTeardown(ep) => self.retry_teardown(ep).await,
            Action::RemoveStaticCompute(ep) => self.remove_static_compute(ep).await,
            Action::PatchHistoryWindow(db) => self.handle_database_settings(*db).await,
            Action::ReportUnknownTimeline(tenant, timeline) => {
                warn!(
                    tenant_id = %tenant,
                    timeline_id = %timeline,
                    "timeline has no branch; keeping it. It may be left from a crash between \
                     clearing a pending timeline and deleting it; delete it by hand once you're \
                     sure nothing needs it"
                );
                Ok(())
            }
            Action::ReportStuckRestore(id) => {
                info!(
                    branch_id = %id,
                    "a restore of this branch failed with its detach unresolved; keeping the \
                     branch, its compute and its pending timeline. Once the storage controller \
                     shows no detach running, delete the pending timeline by hand, or delete \
                     the branch"
                );
                Ok(())
            }
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

    /// R3: tears down an old rehearsal branch unless its merge (the one its name is for, a merge
    /// of its parent) is still rehearsing.
    async fn remove_stale_rehearsal(&self, id: BranchId) -> Result<(), String> {
        let Some(b) = self
            .metadata
            .get_branch(id)
            .await
            .map_err(|e| e.to_string())?
        else {
            return Ok(());
        };
        if b.kind != BranchKind::Rehearsal || b.status == BranchStatus::Deleted {
            return Ok(());
        }
        let merges = self
            .metadata
            .list_merges(b.database_id, b.parent_branch_id)
            .await
            .map_err(|e| format!("listing the merges: {e}"))?;
        let running = merges.iter().any(|m| {
            m.status == chelabase_metadata::MergeStatus::Rehearsing
                && crate::merge::rehearsal_name(m.id) == b.name
        });
        if running {
            return Ok(());
        }
        info!(branch_id = %id, name = %b.name, "removing an old rehearsal");
        self.teardown_rehearsal(id).await
    }

    /// Fails a branch that's still stale when written (a job may have picked it up since the
    /// snapshot), then tears down what its create built. Never the tenant.
    async fn fail_stale_branch(&self, id: BranchId) -> Result<(), String> {
        let failed = self
            .metadata
            .fail_branch_if_stale(id, STALE_PROVISION_SECS)
            .await
            .map_err(|e| e.to_string())?;
        if !failed {
            return Ok(());
        }
        let branch = self
            .metadata
            .get_branch(id)
            .await
            .map_err(|e| e.to_string())?;
        let Some(branch) = branch else {
            return Ok(());
        };
        let tenant = self.tenant_of(branch.database_id).await?;
        // A failure here leaves the endpoint row; `RetryTeardown` finishes it.
        self.teardown_branch(&branch, tenant.as_ref()).await
    }

    /// Teardown for an endpoint row, if it's still there. The tenant goes with it only when
    /// it holds no other timeline (main's failed provision); a branch's endpoint takes only its
    /// own timeline, since main and the other branches live in the same tenant.
    async fn retry_teardown(&self, endpoint_id: &str) -> Result<(), String> {
        let endpoint = self
            .metadata
            .neon_endpoint(endpoint_id)
            .await
            .map_err(|e| e.to_string())?;
        let Some(endpoint) = endpoint else {
            return Ok(());
        };
        let tenant: NeonId = endpoint
            .tenant_id
            .parse()
            .map_err(|e| format!("tenant id {:?}: {e}", endpoint.tenant_id))?;
        let timelines = match self.storage.list_timelines(&tenant).await {
            Ok(timelines) => timelines,
            // The tenant is gone already.
            Err(StorconError::Rejected { status, .. }) if status == StatusCode::NOT_FOUND => {
                Vec::new()
            }
            Err(err) => return Err(format!("listing timelines: {err:#}")),
        };
        if timelines.iter().all(|t| t.as_str() == endpoint.timeline_id) {
            self.teardown(&endpoint.id, &endpoint.tenant_id).await
        } else {
            self.teardown_branch_parts(
                Some(&endpoint.id),
                Some(&tenant),
                &[endpoint.timeline_id.as_str()],
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use chelabase_metadata::BranchKind;

    use super::*;
    use crate::fakes::{
        FRESH_HEAD, FakeWorker, Journal, entries, pending_branch, position, ready_branch,
        ready_database, worker,
    };
    use crate::provision::sha256_hex;
    use crate::roles::RoleSpec;

    const EP: &str = "ep-0123456789ab";
    const EP2: &str = "ep-ba9876543210";
    /// `dev`'s endpoint.
    const EP3: &str = "ep-0000000000d1";
    const TENANT: &str = "0123456789abcdef0123456789abcdef";
    const MAIN_TL: &str = "11111111111111111111111111111111";
    const DEV_TL: &str = "22222222222222222222222222222222";
    const PENDING_TL: &str = "33333333333333333333333333333333";
    const WEEK: Duration = Duration::from_secs(7 * 24 * 3600);

    /// A database in `TENANT` with the default history window.
    fn db(status: DatabaseStatus, endpoint_id: Option<&str>, age_secs: i64) -> LiveDatabase {
        LiveDatabase {
            id: DatabaseId::new(),
            status,
            endpoint_id: endpoint_id.map(str::to_string),
            neon_tenant_id: Some(TENANT.to_string()),
            history_window: WEEK,
            age_secs,
        }
    }

    /// `id`, linked to `db`'s main branch (which has the database's status).
    fn endpoint(id: &str, db: Option<&LiveDatabase>, needs_reconfigure: bool) -> EndpointState {
        EndpointState {
            id: id.to_string(),
            tenant_id: TENANT.to_string(),
            branch_id: db.map(|d| BranchId::from(d.id)),
            branch_status: db.map(|d| BranchStatus::from(d.status)),
            needs_reconfigure,
            compute_settings_version: crate::COMPUTE_SETTINGS_VERSION,
            routable: true,
            static_lsn: None,
            age_secs: 3600,
        }
    }

    /// `db`'s main branch on `MAIN_TL`, with the database's status, endpoint and age.
    fn main_of(db: &LiveDatabase) -> LiveBranch {
        LiveBranch {
            id: BranchId::from(db.id),
            database_id: db.id,
            kind: BranchKind::Normal,
            status: BranchStatus::from(db.status),
            endpoint_id: db.endpoint_id.clone(),
            timeline_id: Some(MAIN_TL.to_string()),
            pending_timeline_id: None,
            superseded_timeline_id: None,
            age_secs: db.age_secs,
        }
    }

    /// `db` with its main branch, its endpoint `EP`, its compute, its tenant (holding main's
    /// timeline) and the tenant's history window in sync.
    fn linked(db: LiveDatabase, needs_reconfigure: bool) -> Snapshot {
        let ep = endpoint(EP, Some(&db), needs_reconfigure);
        Snapshot {
            branches: vec![main_of(&db)],
            databases: vec![db],
            endpoints: vec![ep],
            computes: [EP.to_string()].into(),
            tenants: [TENANT.to_string()].into(),
            tenant_timelines: [(TENANT.to_string(), [MAIN_TL.to_string()].into())].into(),
            pitr: [(TENANT.to_string(), WEEK)].into(),
            ..Default::default()
        }
    }

    /// A ready database, plus `dev`: a `normal` branch of main with the endpoint `EP3` (and its
    /// compute) on `DEV_TL`.
    fn with_dev(status: BranchStatus, age_secs: i64) -> (Snapshot, BranchId) {
        let mut s = linked(db(DatabaseStatus::Ready, Some(EP), 3600), false);
        let dev = LiveBranch {
            id: BranchId::new(),
            database_id: s.databases[0].id,
            kind: BranchKind::Normal,
            status,
            endpoint_id: Some(EP3.to_string()),
            timeline_id: Some(DEV_TL.to_string()),
            pending_timeline_id: None,
            superseded_timeline_id: None,
            age_secs,
        };
        s.endpoints.push(EndpointState {
            id: EP3.to_string(),
            tenant_id: TENANT.to_string(),
            branch_id: Some(dev.id),
            branch_status: Some(status),
            needs_reconfigure: false,
            compute_settings_version: crate::COMPUTE_SETTINGS_VERSION,
            routable: true,
            static_lsn: None,
            age_secs,
        });
        s.computes.insert(EP3.to_string());
        timelines(&mut s).insert(DEV_TL.to_string());
        let id = dev.id;
        s.branches.push(dev);
        (s, id)
    }

    fn branch_mut(s: &mut Snapshot, id: BranchId) -> &mut LiveBranch {
        s.branches.iter_mut().find(|b| b.id == id).unwrap()
    }

    /// `TENANT`'s timelines.
    fn timelines(s: &mut Snapshot) -> &mut HashSet<String> {
        s.tenant_timelines.get_mut(TENANT).unwrap()
    }

    // T14 M7: a rehearsal branch is its merge job's until it's old; the normal branch rules
    // (fail a stale provision, retry a delete) never touch a young one.
    #[test]
    fn a_young_rehearsal_is_left_to_its_job() {
        for status in [BranchStatus::Pending, BranchStatus::Provisioning, BranchStatus::Deleting] {
            let (mut s, dev) = with_dev(status, STALE_PROVISION_SECS + STALE_DELETE_SECS + 1);
            let b = branch_mut(&mut s, dev);
            b.kind = BranchKind::Rehearsal;
            b.age_secs = STALE_REHEARSAL_SECS - 1;
            assert!(
                !plan(&s).iter().any(|a| matches!(a, Action::FailStaleBranch(id) | Action::RetryBranchDelete(id) | Action::RemoveRehearsal(id) if *id == dev)),
                "{status:?}: {:?}",
                plan(&s)
            );
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
        let main = s.branches[0].id;
        assert_eq!(
            plan(&s),
            [Action::RecreateBranchCompute(main, EP.to_string())]
        );
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

    /// A base read's static compute (step 6g): unroutable, pinned, linked to no branch.
    fn static_endpoint(id: &str, age_secs: i64) -> EndpointState {
        EndpointState {
            routable: false,
            static_lsn: Some("0/2000000".to_string()),
            age_secs,
            ..endpoint(id, None, false)
        }
    }

    // R3: one still reading is left alone, and it's no stray endpoint either.
    #[test]
    fn static_computes_are_removed_after_ten_minutes() {
        use crate::schema_base::STALE_STATIC_COMPUTE_SECS;
        let mut s = linked(db(DatabaseStatus::Ready, Some(EP), 3600), false);
        s.endpoints.push(static_endpoint(EP2, STALE_STATIC_COMPUTE_SECS));
        s.computes.insert(EP2.to_string());
        assert!(plan(&s).is_empty(), "{:?}", plan(&s));

        s.endpoints[1].age_secs = STALE_STATIC_COMPUTE_SECS + 1;
        assert_eq!(plan(&s), [Action::RemoveStaticCompute(EP2.to_string())]);

        // An unroutable endpoint that isn't pinned isn't a static compute's.
        s.endpoints[1].static_lsn = None;
        assert_eq!(
            plan(&s),
            [Action::ReportEndpointWithoutDatabase(EP2.to_string())]
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
        let ready_main = BranchId::from(ready.id);
        let s = Snapshot {
            endpoints: vec![endpoint(EP, Some(&ready), true)],
            branches: vec![main_of(&ready)],
            databases: vec![ready, stale],
            computes: [EP2.to_string()].into(),
            tenants: ["fedcba9876543210fedcba9876543210".to_string()].into(),
            ..Default::default()
        };
        assert_eq!(
            plan(&s),
            [
                Action::FailStale(stale_id),
                Action::RecreateBranchCompute(ready_main, EP.to_string()),
                Action::ApplyPending(EP.to_string()),
                Action::RemoveCompute(EP2.to_string()),
                Action::ReportUnknownTenant("fedcba9876543210fedcba9876543210".to_string()),
            ]
        );
    }

    // --- Branches (step 5) ---------------------------------------------------------------

    #[test]
    fn a_stale_branch_create_fails() {
        for status in [BranchStatus::Pending, BranchStatus::Provisioning] {
            let (s, _) = with_dev(status, STALE_PROVISION_SECS);
            assert!(plan(&s).is_empty(), "{status:?}");
            // Its compute may not exist yet: failing it is all there is to do.
            let (mut s, dev) = with_dev(status, STALE_PROVISION_SECS + 1);
            s.computes.remove(EP3);
            assert_eq!(plan(&s), [Action::FailStaleBranch(dev)], "{status:?}");
        }
        // Main's staleness is its database's.
        let row = db(
            DatabaseStatus::Provisioning,
            Some(EP),
            STALE_PROVISION_SECS + 1,
        );
        let id = row.id;
        assert_eq!(plan(&linked(row, false)), [Action::FailStale(id)]);
    }

    #[test]
    fn a_stale_reset_is_finished() {
        let (mut s, dev) = with_dev(BranchStatus::Resetting, STALE_PROVISION_SECS);
        // Its compute is stopped mid-switch: not a missing compute to recreate.
        s.computes.remove(EP3);
        assert!(plan(&s).is_empty());
        branch_mut(&mut s, dev).age_secs = STALE_PROVISION_SECS + 1;
        assert_eq!(plan(&s), [Action::FinishSwitch(dev)]);
        // Main can be restored too.
        let mut s = linked(db(DatabaseStatus::Ready, Some(EP), 3600), false);
        let main = s.branches[0].id;
        branch_mut(&mut s, main).status = BranchStatus::Resetting;
        branch_mut(&mut s, main).age_secs = STALE_PROVISION_SECS + 1;
        assert_eq!(plan(&s), [Action::FinishSwitch(main)]);
    }

    #[test]
    fn a_lost_branch_delete_is_retried_after_a_minute() {
        let (s, _) = with_dev(BranchStatus::Deleting, STALE_DELETE_SECS);
        assert!(plan(&s).is_empty());
        let (s, dev) = with_dev(BranchStatus::Deleting, STALE_DELETE_SECS + 1);
        assert_eq!(plan(&s), [Action::RetryBranchDelete(dev)]);
        // A deleting database's branches (main's included) go with the database's delete.
        let (mut s, _) = with_dev(BranchStatus::Deleting, STALE_DELETE_SECS + 1);
        s.databases[0].status = DatabaseStatus::Deleting;
        s.databases[0].age_secs = STALE_DELETE_SECS + 1;
        let main = s.branches[0].id;
        branch_mut(&mut s, main).status = BranchStatus::Deleting;
        branch_mut(&mut s, main).age_secs = STALE_DELETE_SECS + 1;
        assert_eq!(plan(&s), [Action::RetryDelete(s.databases[0].id)]);
    }

    #[test]
    fn a_ready_branch_without_its_compute_gets_a_new_one() {
        let (mut s, dev) = with_dev(BranchStatus::Ready, 3600);
        let main = s.branches[0].id;
        s.computes.remove(EP3);
        assert_eq!(
            plan(&s),
            [Action::RecreateBranchCompute(dev, EP3.to_string())]
        );
        // Main and a branch, both gone: one action each.
        s.computes.remove(EP);
        assert_eq!(
            plan(&s),
            [
                Action::RecreateBranchCompute(dev, EP3.to_string()),
                Action::RecreateBranchCompute(main, EP.to_string()),
            ]
        );
    }

    // --- The data API (step 6) -----------------------------------------------------------

    #[test]
    fn a_branch_missing_its_data_api_is_bootstrapped() {
        let (mut s, dev) = with_dev(BranchStatus::Ready, 3600);
        let main = s.branches[0].id;
        s.needs_data_api.insert(dev);
        assert_eq!(plan(&s), [Action::BootstrapDataApi { branch_id: dev, database: None }]);
        s.needs_data_api.insert(main);
        assert_eq!(plan(&s).len(), 2);
        // Without its compute, recreating it runs the bootstrap too.
        s.computes.remove(EP3);
        assert_eq!(
            plan(&s),
            [
                Action::RecreateBranchCompute(dev, EP3.to_string()),
                Action::BootstrapDataApi { branch_id: main, database: None },
            ]
        );
        // Only ready branches.
        for status in [
            BranchStatus::Provisioning,
            BranchStatus::Resetting,
            BranchStatus::Failed,
        ] {
            let (mut s, dev) = with_dev(status, 0);
            s.needs_data_api.insert(dev);
            let planned = plan(&s);
            assert!(
                !planned
                    .iter()
                    .any(|a| matches!(a, Action::BootstrapDataApi { .. })),
                "{status:?}: {planned:?}"
            );
        }
    }

    #[test]
    fn resync_follows_the_data_api_bootstrap() {
        let (mut s, dev) = with_dev(BranchStatus::Ready, 3600);
        let main = s.branches[0].id;
        s.resync.insert(dev);
        assert_eq!(plan(&s), [Action::Resync(dev, EP3.to_string())]);
        s.needs_data_api.insert(dev);
        s.needs_data_api.insert(main);
        s.computes.insert("ep-orphan".to_string());
        let planned = plan(&s);
        let resync = planned
            .iter()
            .position(|a| *a == Action::Resync(dev, EP3.to_string()))
            .unwrap();
        assert_eq!(resync, 2, "{planned:?}");
        assert!(
            matches!(planned[3], Action::RemoveCompute(_)),
            "{planned:?}"
        );
        // Recreating the compute runs the resync itself.
        s.computes.remove(EP3);
        assert!(!plan(&s).iter().any(|a| matches!(a, Action::Resync(..))));
        // Only ready branches.
        for status in [BranchStatus::Resetting, BranchStatus::Failed] {
            let (mut s, dev) = with_dev(status, 0);
            s.resync.insert(dev);
            assert!(
                !plan(&s).iter().any(|a| matches!(a, Action::Resync(..))),
                "{status:?}"
            );
        }
    }

    #[test]
    fn computes_with_old_settings_are_upgraded() {
        let (mut s, dev) = with_dev(BranchStatus::Ready, 3600);
        s.endpoints[1].compute_settings_version = crate::COMPUTE_SETTINGS_VERSION - 1;
        assert_eq!(plan(&s), [Action::UpgradeCompute(dev, EP3.to_string())]);
        // The re-creation covers the bootstrap and the resync; it runs after a missing
        // compute's, before the rest.
        s.needs_data_api.insert(dev);
        s.resync.insert(dev);
        let main = s.branches[0].id;
        s.branches[0].endpoint_id = Some(EP2.to_string());
        s.endpoints.push(EndpointState {
            id: EP2.to_string(),
            tenant_id: TENANT.to_string(),
            branch_id: Some(main),
            branch_status: Some(BranchStatus::Ready),
            needs_reconfigure: true,
            compute_settings_version: crate::COMPUTE_SETTINGS_VERSION,
            routable: true,
            static_lsn: None,
            age_secs: 3600,
        });
        s.computes.remove(EP);
        let planned = plan(&s);
        assert_eq!(
            planned[..3],
            [
                Action::RecreateBranchCompute(main, EP2.to_string()),
                Action::UpgradeCompute(dev, EP3.to_string()),
                Action::ApplyPending(EP2.to_string()),
            ],
            "{planned:?}"
        );
        assert!(
            !planned
                .iter()
                .any(|a| matches!(a, Action::Resync(..) | Action::BootstrapDataApi { .. })),
            "{planned:?}"
        );
        // A missing compute is re-created instead.
        s.computes.remove(EP3);
        let planned = plan(&s);
        assert!(planned.contains(&Action::RecreateBranchCompute(dev, EP3.to_string())));
        assert!(
            !planned
                .iter()
                .any(|a| matches!(a, Action::UpgradeCompute(..)))
        );
        // Only ready normal branches.
        for status in [BranchStatus::Resetting, BranchStatus::Failed] {
            let (mut s, _) = with_dev(status, 0);
            s.endpoints[1].compute_settings_version = 0;
            assert!(
                !plan(&s)
                    .iter()
                    .any(|a| matches!(a, Action::UpgradeCompute(..))),
                "{status:?}"
            );
        }
        let (mut s, _) = with_dev(BranchStatus::Ready, 3600);
        s.endpoints[1].compute_settings_version = 0;
        s.branches[1].kind = BranchKind::Backup;
        assert!(
            !plan(&s)
                .iter()
                .any(|a| matches!(a, Action::UpgradeCompute(..)))
        );
    }

    #[test]
    fn the_schema_check_takes_a_few_branches_round_robin() {
        let mut ids: Vec<BranchId> = (0..25).map(|_| BranchId::new()).collect();
        ids.sort_by_key(|b| b.as_uuid());
        assert_eq!(schema_check_batch(&ids, None, 20), ids[..20]);
        assert_eq!(
            schema_check_batch(&ids, Some(ids[19]), 20),
            [&ids[20..], &ids[..15]].concat()
        );
        // Fewer than `n`: each once, starting after the cursor.
        assert_eq!(
            schema_check_batch(&ids[..3], Some(ids[1]), 20),
            [ids[2], ids[0], ids[1]]
        );
        // A cursor whose branch is gone: the next one after it.
        let rest: Vec<BranchId> = ids.iter().copied().filter(|b| *b != ids[5]).collect();
        assert_eq!(schema_check_batch(&rest, Some(ids[5]), 2), [ids[6], ids[7]]);
        assert!(schema_check_batch(&[], Some(ids[0]), 20).is_empty());
    }

    #[test]
    fn the_endpoint_of_a_failed_branch_is_torn_down() {
        let (s, _) = with_dev(BranchStatus::Failed, 3600);
        assert_eq!(plan(&s), [Action::RetryTeardown(EP3.to_string())]);
    }

    #[test]
    fn a_failed_branch_with_a_pending_timeline_is_reported_not_torn_down() {
        // A restore gave up on a detach that may still be running: its data is on DEV_TL.
        let (mut s, dev) = with_dev(BranchStatus::Failed, 3600);
        branch_mut(&mut s, dev).pending_timeline_id = Some(PENDING_TL.to_string());
        timelines(&mut s).insert(PENDING_TL.to_string());
        assert_eq!(plan(&s), [Action::ReportStuckRestore(dev)]);
    }

    #[test]
    fn a_tenant_whose_history_window_drifted_is_patched() {
        let mut s = linked(db(DatabaseStatus::Ready, Some(EP), 3600), false);
        let id = s.databases[0].id;
        s.pitr
            .insert(TENANT.to_string(), Duration::from_secs(24 * 3600));
        assert_eq!(plan(&s), [Action::PatchHistoryWindow(id)]);
        // Never set, as on every tenant made before step 5.
        s.pitr.clear();
        assert_eq!(plan(&s), [Action::PatchHistoryWindow(id)]);
        // The database's window changed.
        s.pitr.insert(TENANT.to_string(), WEEK);
        s.databases[0].history_window = Duration::from_secs(3600);
        assert_eq!(plan(&s), [Action::PatchHistoryWindow(id)]);
        // Provisioning applies it itself; a tenant that isn't there can't be patched.
        let mut s = linked(db(DatabaseStatus::Provisioning, Some(EP), 5), false);
        s.pitr.clear();
        assert!(plan(&s).is_empty());
        let mut s = linked(db(DatabaseStatus::Ready, Some(EP), 3600), false);
        s.pitr.clear();
        s.tenants.clear();
        s.tenant_timelines.clear();
        assert!(plan(&s).is_empty());
    }

    // Deleting storage on a guess can't be undone.
    #[test]
    fn unknown_timelines_are_reported_never_deleted() {
        let stray = "33333333333333333333333333333333";
        let (pending, superseded) = (
            "44444444444444444444444444444444",
            "55555555555555555555555555555555",
        );
        let (mut s, dev) = with_dev(BranchStatus::Resetting, 5);
        branch_mut(&mut s, dev).pending_timeline_id = Some(pending.to_string());
        branch_mut(&mut s, dev).superseded_timeline_id = Some(superseded.to_string());
        timelines(&mut s).extend([stray, pending, superseded].map(str::to_string));
        assert_eq!(
            plan(&s),
            [Action::ReportUnknownTimeline(
                TENANT.to_string(),
                stray.to_string()
            )]
        );
        // A deleting database's tenant is on its way out; an unknown tenant is reported whole.
        let (mut s, _) = with_dev(BranchStatus::Deleting, 5);
        s.databases[0].status = DatabaseStatus::Deleting;
        s.databases[0].age_secs = 5;
        timelines(&mut s).insert(stray.to_string());
        assert!(plan(&s).is_empty());
        let other = "fedcba9876543210fedcba9876543210";
        let mut s = linked(db(DatabaseStatus::Ready, Some(EP), 3600), false);
        s.tenants.insert(other.to_string());
        s.tenant_timelines
            .insert(other.to_string(), [stray.to_string()].into());
        assert_eq!(plan(&s), [Action::ReportUnknownTenant(other.to_string())]);
    }

    #[test]
    fn a_fresh_branch_is_left_alone() {
        // Before its endpoint and timeline are recorded.
        let (mut s, dev) = with_dev(BranchStatus::Pending, 5);
        s.endpoints.retain(|e| e.id != EP3);
        s.computes.remove(EP3);
        timelines(&mut s).remove(DEV_TL);
        branch_mut(&mut s, dev).endpoint_id = None;
        branch_mut(&mut s, dev).timeline_id = None;
        assert!(plan(&s).is_empty());
        // Recorded; the timeline is made but the compute isn't started yet.
        let (mut s, _) = with_dev(BranchStatus::Provisioning, 5);
        s.computes.remove(EP3);
        assert!(plan(&s).is_empty());
        // Recorded, not yet created: a listing taken before the row was written.
        let (mut s, _) = with_dev(BranchStatus::Provisioning, 5);
        timelines(&mut s).remove(DEV_TL);
        assert!(plan(&s).is_empty());
        // Mid-reset: the pending timeline exists and the compute is stopped.
        let (mut s, dev) = with_dev(BranchStatus::Resetting, 5);
        let pending = "44444444444444444444444444444444";
        branch_mut(&mut s, dev).pending_timeline_id = Some(pending.to_string());
        timelines(&mut s).insert(pending.to_string());
        s.computes.remove(EP3);
        assert!(plan(&s).is_empty());
        // Being deleted by its job.
        let (mut s, _) = with_dev(BranchStatus::Deleting, 5);
        s.computes.remove(EP3);
        assert!(plan(&s).is_empty());
    }

    #[test]
    fn backups_have_no_compute_and_need_nothing() {
        let backup_tl = "66666666666666666666666666666666";
        let mut s = linked(db(DatabaseStatus::Ready, Some(EP), 3600), false);
        let main = s.branches[0].clone();
        s.branches.push(LiveBranch {
            id: BranchId::new(),
            kind: BranchKind::Backup,
            status: BranchStatus::Ready,
            endpoint_id: None,
            timeline_id: Some(backup_tl.to_string()),
            age_secs: 7 * 24 * 3600,
            ..main
        });
        timelines(&mut s).insert(backup_tl.to_string());
        assert!(plan(&s).is_empty());
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
        let ep = w.metadata.main(id).endpoint_id.unwrap();
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
    async fn reconcile_removes_a_stale_static_compute() {
        use crate::schema_base::STALE_STATIC_COMPUTE_SECS;
        let (w, journal) = worker();
        let (id, _ep) = provision(&w, &journal).await;
        let tenant = w.metadata.database(id).neon_tenant_id.unwrap();
        let timeline = w.metadata.main(id).timeline_id.unwrap();
        for (ep, age) in [
            ("ep-0000000000a1", STALE_STATIC_COMPUTE_SECS + 1),
            ("ep-0000000000a2", 5),
        ] {
            w.metadata
                .create_static_endpoint(&chelabase_metadata::NewStaticEndpoint {
                    id: ep,
                    tenant_id: &tenant,
                    timeline_id: &timeline,
                    compute_token_sha256: "aa",
                    static_lsn: "0/2000000",
                })
                .await
                .unwrap();
            w.computes.insert(ep);
            w.metadata.state().endpoint_ages.insert(ep.to_string(), age);
        }
        journal.lock().unwrap().clear();

        w.reconcile_once().await.unwrap();

        assert!(
            pos(&journal, "compute.delete ep-0000000000a1")
                < pos(&journal, "metadata.delete_endpoint")
        );
        assert!(!w.metadata.state().endpoints.contains_key("ep-0000000000a1"));
        assert!(!w.computes.list().await.unwrap().contains(&"ep-0000000000a1".to_string()));
        // The one still reading stays.
        assert!(w.metadata.state().endpoints.contains_key("ep-0000000000a2"));
        assert!(w.computes.list().await.unwrap().contains(&"ep-0000000000a2".to_string()));
        assert!(position(&journal, "compute.delete ep-0000000000a2").is_none());

        // A compute that can't be removed keeps its row: the next pass tries again.
        w.metadata
            .state()
            .endpoint_ages
            .insert("ep-0000000000a2".to_string(), STALE_STATIC_COMPUTE_SECS + 1);
        *w.computes.fail_delete.lock().unwrap() = Some("docker is down".into());
        w.reconcile_once().await.unwrap();
        assert!(w.metadata.state().endpoints.contains_key("ep-0000000000a2"));
    }

    /// The jobs a reconcile pass hands back, every publish succeeding.
    async fn resumed(w: &crate::fakes::FakeWorker) -> Vec<chelabase_nats::MergeJob> {
        w.hand_back_merges(|_| async { Ok(()) }).await
    }

    // T14 F3 fix round (M3): a hand-back is recorded only once its job is published: one whose
    // publish failed is handed back on the next pass, not after the re-hand window.
    #[tokio::test]
    async fn a_failed_publish_isnt_recorded_as_a_hand_back() {
        use chelabase_metadata::{MergeDirection, MergeStatus};
        let (w, journal) = worker();
        let (id, _ep) = provision(&w, &journal).await;
        let main = w.metadata.main(id).id;
        let silent = w.metadata.insert_merge(id, main, main, MergeDirection::IntoParent, MergeStatus::Online);
        let long_ago = chrono::Utc::now() - chrono::Duration::seconds(3600);
        {
            let mut state = w.metadata.state();
            let row = state.merges.get_mut(&silent).unwrap();
            row.claimed_at = Some(long_ago);
            row.updated_at = long_ago;
        }
        let failed = w.hand_back_merges(|_| async { Err("nats is down".to_string()) }).await;
        assert!(failed.is_empty());
        assert!(!w.metadata.state().handed_back.contains_key(&silent));
        assert_eq!(resumed(&w).await, [chelabase_nats::MergeJob::Resume { merge_id: silent }]);
        assert!(w.metadata.state().handed_back.contains_key(&silent));
        assert!(resumed(&w).await.is_empty(), "recorded once published");
    }

    // R3: merges whose worker went silent are handed back as `Resume` jobs, each once per
    // stale period (the claim is refreshed when it's handed back).
    #[tokio::test]
    async fn reconcile_requeues_silent_merges() {
        use chelabase_metadata::{MergeDirection, MergeStatus};
        let (w, journal) = worker();
        let (id, _ep) = provision(&w, &journal).await;
        let main = w.metadata.main(id).id;
        let merge = |status| w.metadata.insert_merge(id, main, main, MergeDirection::IntoParent, status);
        let silent = merge(MergeStatus::Online);
        let busy = merge(MergeStatus::Applying);
        let idle = merge(MergeStatus::Draft);
        let long_ago = chrono::Utc::now() - chrono::Duration::seconds(300);
        for m in [silent, idle] {
            let mut state = w.metadata.state();
            let row = state.merges.get_mut(&m).unwrap();
            row.claimed_at = row.status.is_working().then_some(long_ago);
            row.updated_at = long_ago;
        }

        let resume = |id| chelabase_nats::MergeJob::Resume { merge_id: id };
        assert_eq!(resumed(&w).await, [resume(silent)]);
        assert_eq!(resumed(&w).await, [], "handed back once");
        let _ = busy;
    }

    // T14 M10: a merge handed back isn't handed back again while its Resume waits (no worker
    // claimed it since), until the re-hand window; once a worker claims it, it can be again.
    #[tokio::test]
    async fn a_handed_back_merge_waits_for_its_job() {
        use chelabase_metadata::{MergeDirection, MergeStatus};
        let (w, journal) = worker();
        let (id, _ep) = provision(&w, &journal).await;
        let main = w.metadata.main(id).id;
        let silent = w.metadata.insert_merge(id, main, main, MergeDirection::IntoParent, MergeStatus::Online);
        let long_ago = chrono::Utc::now() - chrono::Duration::seconds(3600);
        let age = |w: &crate::fakes::FakeWorker, at| {
            let mut state = w.metadata.state();
            let row = state.merges.get_mut(&silent).unwrap();
            row.claimed_at = Some(at);
            row.updated_at = at;
        };
        age(&w, long_ago);
        assert_eq!(resumed(&w).await.len(), 1);
        // The claim stays as old (the job waits behind a long one): not handed back again.
        assert!(resumed(&w).await.is_empty());
        assert_eq!(w.metadata.merge_row(silent).claimed_at, Some(long_ago), "the claim isn't touched");
        // A worker claimed it after the hand-back, then went silent again: handed back.
        w.metadata.state().handed_back.insert(silent, chrono::Utc::now() - chrono::Duration::seconds(1000));
        age(&w, chrono::Utc::now() - chrono::Duration::seconds(600));
        assert_eq!(resumed(&w).await.len(), 1);
        // The window passed with no claim: handed back (the job was lost).
        w.metadata.state().handed_back.insert(silent, chrono::Utc::now() - chrono::Duration::seconds(3600));
        age(&w, long_ago);
        assert_eq!(resumed(&w).await.len(), 1);
    }

    // T14 M8: a requested after-merge choice whose Finish job was lost is published again.
    #[tokio::test]
    async fn a_lost_finish_job_is_published_again() {
        use chelabase_metadata::{MergeDirection, MergeStatus};
        let (w, journal) = worker();
        let (id, _ep) = provision(&w, &journal).await;
        let main = w.metadata.main(id).id;
        let m = w.metadata.insert_merge(id, main, main, MergeDirection::IntoParent, MergeStatus::AppliedWithDrift);
        {
            let mut state = w.metadata.state();
            let row = state.merges.get_mut(&m).unwrap();
            row.result = Some(serde_json::json!({ "after": { "choice": "reset", "requested": true } }));
            row.updated_at = chrono::Utc::now() - chrono::Duration::seconds(3600);
        }
        assert_eq!(
            resumed(&w).await,
            [chelabase_nats::MergeJob::Finish { merge_id: m, after: chelabase_nats::MergeAfter::Reset }]
        );
        assert!(resumed(&w).await.is_empty(), "once per period");
    }

    /// A merge applied with drift whose `result.after` is `after`, last written an hour ago.
    async fn drifted_with_after(w: &crate::fakes::FakeWorker, journal: &crate::fakes::Journal, after: serde_json::Value) -> uuid::Uuid {
        use chelabase_metadata::{MergeDirection, MergeStatus};
        let (id, _ep) = provision(w, journal).await;
        let main = w.metadata.main(id).id;
        let m = w.metadata.insert_merge(id, main, main, MergeDirection::IntoParent, MergeStatus::AppliedWithDrift);
        let mut state = w.metadata.state();
        let row = state.merges.get_mut(&m).unwrap();
        row.result = Some(serde_json::json!({ "after": after }));
        row.updated_at = chrono::Utc::now() - chrono::Duration::seconds(3600);
        m
    }

    // T14 F3 fix round (M1): a Finish lost after its run recorded `started` (the worker went away
    // before the outcome, and the job wasn't redelivered) is published again too.
    #[tokio::test]
    async fn a_finish_lost_after_it_started_is_published_again() {
        let (w, journal) = worker();
        let m = drifted_with_after(&w, &journal, serde_json::json!({ "choice": "delete", "started": true, "timeline": null })).await;
        assert_eq!(
            resumed(&w).await,
            [chelabase_nats::MergeJob::Finish { merge_id: m, after: chelabase_nats::MergeAfter::Delete }]
        );
    }

    // T14 F3 fix round (M2): a lost Finish is re-published at most once per re-hand window while
    // nothing ran it (its job may wait behind a long one), as a silent merge's Resume is.
    #[tokio::test]
    async fn a_lost_finish_is_handed_back_once_per_rehand_window() {
        let (w, journal) = worker();
        let m = drifted_with_after(&w, &journal, serde_json::json!({ "choice": "reset", "requested": true })).await;
        let ago = |secs| chrono::Utc::now() - chrono::Duration::seconds(secs);
        w.metadata.state().handed_back.insert(m, ago(15 * 60));
        assert!(resumed(&w).await.is_empty(), "handed back 15 minutes ago");
        w.metadata.state().handed_back.insert(m, ago(31 * 60));
        assert_eq!(resumed(&w).await.len(), 1, "the window passed");
    }

    // R3: a rehearsal branch older than 30 minutes whose merge isn't rehearsing is torn down; one
    // still rehearsing, or younger, stays.
    #[tokio::test]
    async fn reconcile_deletes_old_rehearsals() {
        use chelabase_metadata::{MergeDirection, MergeStatus};
        let (w, journal) = worker();
        let (id, _ep) = provision(&w, &journal).await;
        let main = w.metadata.main(id).id;
        let merge = |status| w.metadata.insert_merge(id, main, main, MergeDirection::IntoParent, status);
        let cancelled = merge(MergeStatus::Cancelled);
        let running = merge(MergeStatus::Rehearsing);
        let mut made = Vec::new();
        for (m, age) in [
            (cancelled, STALE_REHEARSAL_SECS + 1),
            (running, STALE_REHEARSAL_SECS + 1),
            (merge(MergeStatus::Draft), 60),
        ] {
            let name = format!("_rehearsal-{}", &m.simple().to_string()[..8]);
            let b = w
                .metadata
                .create_rehearsal_branch(id, main, &name)
                .await
                .unwrap();
            w.handle_branch_create(b.id, None).await.unwrap();
            w.metadata.state().branch_ages.insert(b.id, age);
            made.push(b.id);
        }
        let old_ep = w.metadata.branch(made[0]).endpoint_id.unwrap();

        w.reconcile_once().await.unwrap();

        let status = |b| w.metadata.branch(b).status;
        assert_eq!(status(made[0]), BranchStatus::Deleted);
        assert_eq!(status(made[1]), BranchStatus::Ready, "its merge is rehearsing");
        assert_eq!(status(made[2]), BranchStatus::Ready, "younger than 30 minutes");
        assert!(!w.metadata.state().endpoints.contains_key(&old_ep));
        assert!(!w.computes.list().await.unwrap().contains(&old_ep));
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
    async fn reconcile_bootstraps_branches_missing_key_or_password() {
        let (w, journal) = worker();
        let (id, ep) = provision(&w, &journal).await;
        let main = BranchId::from(id);
        let data_api = format!("compute.sql {ep} app BEGIN;");

        // Nothing missing: no bootstrap.
        w.reconcile_once().await.unwrap();
        assert_eq!(
            position(&journal, &data_api),
            None,
            "{:?}",
            entries(&journal)
        );

        // No authenticator password.
        w.metadata
            .state()
            .branches
            .get_mut(&main)
            .unwrap()
            .authenticator_password_enc = None;
        w.reconcile_once().await.unwrap();
        assert!(w.metadata.main(id).authenticator_password_enc.is_some());
        assert!(pos(&journal, "metadata.set_authenticator_password") < pos(&journal, &data_api));

        // No public key.
        journal.lock().unwrap().clear();
        w.metadata.state().data_keys.clear();
        w.reconcile_once().await.unwrap();
        assert!(pos(&journal, &data_api) < pos(&journal, "metadata.insert_data_key"));
        assert_eq!(w.metadata.state().data_keys.len(), 1);
    }

    #[tokio::test]
    async fn reconcile_bootstraps_when_chelabase_is_missing() {
        let check = "to_regclass('chelabase.schema_version')";
        // Present: checked, nothing else.
        let (w, journal) = worker();
        let (_id, ep) = provision(&w, &journal).await;
        w.computes.answer(check, "t\n");
        w.reconcile_once().await.unwrap();
        assert!(
            position(
                &journal,
                &format!("compute.sql {ep} app SET search_path = pg_catalog, pg_temp; SELECT to_regclass")
            )
            .is_some()
        );
        assert_eq!(
            position(&journal, &format!("compute.sql {ep} app BEGIN;")),
            None
        );

        // Missing: bootstrapped.
        let (w, journal) = worker();
        let (_id, ep) = provision(&w, &journal).await;
        w.computes.answer(check, "f\n");
        w.reconcile_once().await.unwrap();
        let checked = pos(
            &journal,
            &format!("compute.sql {ep} app SET search_path = pg_catalog, pg_temp; SELECT to_regclass"),
        );
        assert!(checked < pos(&journal, &format!("compute.sql {ep} app BEGIN;")));
    }

    #[tokio::test]
    async fn reconcile_plans_resync_in_the_health_batch() {
        const CATALOG_READ: &str = "SET search_path = pg_catalog, pg_temp;";
        let (w, journal) = worker();
        let (a_db, a_ep) = provision(&w, &journal).await;
        let (_b_db, b_ep) = provision(&w, &journal).await;
        assert_eq!(
            w.create_role(BranchId::from(a_db), &RoleSpec::login("app", false)).await,
            crate::roles::RoleOutcome::Ok
        );
        w.computes
            .answer("to_regclass('chelabase.schema_version')", "t\n");
        journal.lock().unwrap().clear();
        w.computes.sql.lock().unwrap().clear();

        w.reconcile_once().await.unwrap();

        // Every branch of the batch with a compute, under its lock, through the system
        // database; the API role missing from the (empty) catalog is re-created.
        for ep in [&a_ep, &b_ep] {
            let read = pos(
                &journal,
                &format!("compute.sql {ep} postgres {CATALOG_READ}"),
            );
            assert!(pos(&journal, "metadata.lock_endpoint") < read);
        }
        assert!(
            w.computes
                .sql
                .lock()
                .unwrap()
                .iter()
                .any(|(ep, _, s)| *ep == a_ep && s.contains("CREATE ROLE \"app\"")),
        );

        // A failure is logged; the pass goes on, and the next pass tries again.
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some(("SELECT 'r',".into(), "connection reset".into()));
        journal.lock().unwrap().clear();
        w.reconcile_once().await.unwrap();
        for ep in [&a_ep, &b_ep] {
            assert!(
                position(
                    &journal,
                    &format!("compute.sql {ep} postgres {CATALOG_READ}")
                )
                .is_some()
            );
        }
        *w.computes.fail_sql_containing.lock().unwrap() = None;
        journal.lock().unwrap().clear();
        w.reconcile_once().await.unwrap();
        assert!(position(&journal, "metadata.apply_resync").is_some());
    }

    #[tokio::test]
    async fn a_failed_resync_after_an_upgrade_is_retried_without_another_upgrade() {
        let (w, journal) = worker();
        let (_id, ep) = provision(&w, &journal).await;
        w.computes
            .answer("to_regclass('chelabase.schema_version')", "t\n");
        w.metadata
            .state()
            .endpoints
            .get_mut(&ep)
            .unwrap()
            .compute_settings_version = 0;
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some(("SELECT 'r',".into(), "connection reset".into()));

        w.reconcile_once().await.unwrap();

        // Re-created once; the resync failed, but the version is recorded.
        assert!(position(&journal, &format!("compute.create {ep}")).is_some());
        assert_eq!(
            w.metadata.state().endpoints[&ep].compute_settings_version,
            crate::COMPUTE_SETTINGS_VERSION
        );

        // The next pass plans a plain resync, never another delete.
        journal.lock().unwrap().clear();
        w.reconcile_once().await.unwrap();
        assert!(
            position(&journal, "compute.delete").is_none(),
            "{:?}",
            entries(&journal)
        );
        assert!(position(&journal, "compute.create").is_none());
        let read = pos(
            &journal,
            &format!("compute.sql {ep} postgres SET search_path = pg_catalog, pg_temp;"),
        );
        assert!(pos(&journal, "metadata.lock_endpoint") < read);
    }

    #[tokio::test]
    async fn reconcile_upgrades_old_computes_once() {
        let (w, journal) = worker();
        let (_id, ep) = provision(&w, &journal).await;
        w.computes
            .answer("to_regclass('chelabase.schema_version')", "t\n");
        w.metadata
            .state()
            .endpoints
            .get_mut(&ep)
            .unwrap()
            .compute_settings_version = 0;

        w.reconcile_once().await.unwrap();

        let order = [
            pos(&journal, &format!("compute.delete {ep}")),
            pos(&journal, "metadata.replace_compute_token"),
            pos(&journal, &format!("compute.create {ep}")),
            pos(&journal, "metadata.set_compute_settings_version"),
            pos(&journal, "metadata.apply_resync"),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
        assert!(w.computes.list().await.unwrap().contains(&ep));
        assert_eq!(
            w.metadata.state().endpoints[&ep].compute_settings_version,
            crate::COMPUTE_SETTINGS_VERSION
        );

        journal.lock().unwrap().clear();
        w.reconcile_once().await.unwrap();
        assert!(
            position(&journal, "compute.delete").is_none(),
            "{:?}",
            entries(&journal)
        );
        assert!(position(&journal, "compute.create").is_none());
    }

    #[tokio::test]
    async fn reconcile_applies_a_reset_that_failed_to_apply() {
        let (w, journal) = worker();
        let (id, ep) = provision(&w, &journal).await;
        *w.pusher.fail.lock().unwrap() = Some("compute_ctl said 500".into());
        w.reset_password(BranchId::from(id)).await.unwrap_err();
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
        w.reset_password(BranchId::from(id)).await.unwrap_err();
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
            state.branches.get_mut(&BranchId::from(id)).unwrap().status =
                BranchStatus::Provisioning;
            state.ages.insert(id, STALE_PROVISION_SECS + 1);
        }

        w.reconcile_once().await.unwrap();

        assert_eq!(w.metadata.database(id).status, DatabaseStatus::Failed);
        assert_eq!(w.metadata.main(id).status, BranchStatus::Failed);
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

    /// With orphan removal off (a worker on its own metadata database sharing the machine's
    /// computes), a compute with no endpoint row is kept.
    #[tokio::test]
    async fn orphan_computes_are_kept_when_removal_is_off() {
        let (mut w, journal) = worker();
        w.remove_orphans = false;
        let (_id, ep) = provision(&w, &journal).await;
        w.computes.insert(EP2);
        journal.lock().unwrap().clear();

        w.reconcile_once().await.unwrap();

        let delete = position(&journal, &format!("compute.delete {EP2}"));
        assert!(delete.is_none(), "{:?}", entries(&journal));
        let mut listed = w.computes.list().await.unwrap();
        listed.sort();
        let mut want = vec![ep, EP2.to_string()];
        want.sort();
        assert_eq!(listed, want);
    }

    #[tokio::test]
    async fn reconcile_errs_when_it_cannot_observe() {
        let (w, _journal) = worker();
        w.metadata.state().fail = Some("list_endpoints_for_reconcile");
        assert!(w.reconcile_once().await.is_err());
    }

    // Ruling 3 (step 5): a branch's leftover endpoint is torn down with its timeline, never
    // the tenant, which main and the other branches live in.
    #[tokio::test]
    async fn a_failed_branch_teardown_keeps_the_tenant() {
        let (w, journal) = worker();
        let db = crate::fakes::ready_database(&w).await;
        let main = w.metadata.main(db);
        let dev = crate::fakes::pending_branch(&w, db, "dev", main.id);
        // The compute fails, and so does the teardown after it: the endpoint row stays.
        *w.computes.fail_create.lock().unwrap() = Some("no docker".into());
        *w.storage.fail.lock().unwrap() = Some("delete_timeline");
        w.handle_branch_create(dev, None).await.unwrap();
        let branch = w.metadata.branch(dev);
        assert_eq!(branch.status, BranchStatus::Failed);
        let (dev_ep, dev_timeline) = (branch.endpoint_id.unwrap(), branch.timeline_id.unwrap());
        *w.computes.fail_create.lock().unwrap() = None;
        *w.storage.fail.lock().unwrap() = None;
        let tenant = w.metadata.database(db).neon_tenant_id.unwrap();
        journal.lock().unwrap().clear();

        w.reconcile_once().await.unwrap();

        assert!(w.storage.has_tenant(&tenant), "{:?}", entries(&journal));
        assert!(position(&journal, "storage.delete_tenant").is_none());
        assert!(position(&journal, &format!("storage.delete_timeline {dev_timeline}")).is_some());
        let main_ep = main.endpoint_id.unwrap();
        assert_eq!(w.metadata.endpoint_ids(), std::slice::from_ref(&main_ep));
        assert_eq!(w.storage.timeline_ids(), [main.timeline_id.unwrap()]);
        assert_eq!(w.computes.list().await.unwrap(), [main_ep]);
        assert!(!w.computes.list().await.unwrap().contains(&dev_ep));
        assert_eq!(w.metadata.database(db).status, DatabaseStatus::Ready);
    }

    // --- Branches, over the fakes (step 5) -----------------------------------------------

    /// A ready database and a ready branch `dev` of main; the journal starts empty.
    struct Dev {
        db: DatabaseId,
        dev: BranchId,
        tenant: String,
        main_timeline: String,
        dev_timeline: String,
        ep: String,
    }

    async fn dev_setup(w: &FakeWorker, journal: &Journal) -> Dev {
        let db = ready_database(w).await;
        let main = w.metadata.main(db);
        let dev = ready_branch(w, db, "dev", main.id).await;
        let b = w.metadata.branch(dev);
        journal.lock().unwrap().clear();
        Dev {
            db,
            dev,
            tenant: w.metadata.database(db).neon_tenant_id.unwrap(),
            main_timeline: main.timeline_id.unwrap(),
            dev_timeline: b.timeline_id.unwrap(),
            ep: b.endpoint_id.unwrap(),
        }
    }

    fn set_branch(w: &FakeWorker, id: BranchId, status: BranchStatus, age_secs: i64) {
        let mut state = w.metadata.state();
        state.branches.get_mut(&id).unwrap().status = status;
        state.branch_ages.insert(id, age_secs);
    }

    /// What a reset of `dev` leaves when it crashes after creating its pending timeline (from
    /// main's head) and stopping the compute: returns the pending timeline.
    fn crash_before_switch(w: &FakeWorker, d: &Dev) -> String {
        let pending = NeonId::new();
        w.storage
            .insert_timeline(&d.tenant, pending.as_str(), FRESH_HEAD);
        w.metadata
            .state()
            .branches
            .get_mut(&d.dev)
            .unwrap()
            .pending_timeline_id = Some(pending.as_str().to_string());
        w.computes.remove(&d.ep);
        set_branch(w, d.dev, BranchStatus::Resetting, STALE_PROVISION_SECS + 1);
        pending.as_str().to_string()
    }

    fn endpoint_timeline(w: &FakeWorker, ep: &str) -> String {
        w.metadata.state().endpoints[ep]
            .endpoint
            .timeline_id
            .clone()
    }

    // Review Focus 2: a crash between the switch and deleting the old timeline. Reconcile
    // recreates the compute on the new timeline (if the crash came before that), deletes the
    // superseded timeline, and only then clears it. No timeline leaks, and the branch never
    // names a missing one.
    #[tokio::test]
    async fn finish_switch_after_a_crash_between_switch_and_delete() {
        for compute_restarted in [false, true] {
            let (w, journal) = worker();
            let d = dev_setup(&w, &journal).await;
            let old_hash = w.metadata.state().endpoints[&d.ep]
                .endpoint
                .compute_token_sha256
                .clone();
            let new = crash_before_switch(&w, &d);
            let switched = futures_util::FutureExt::now_or_never(
                w.metadata.switch_to_pending_timeline(d.dev, &new, None, "0/1"),
            )
            .expect("the fake never waits");
            assert!(switched.unwrap());
            if compute_restarted {
                w.computes.insert(&d.ep);
            }
            set_branch(&w, d.dev, BranchStatus::Resetting, STALE_PROVISION_SECS + 1);
            journal.lock().unwrap().clear();

            w.reconcile_once().await.unwrap();

            let dev = w.metadata.branch(d.dev);
            let ctx = format!("{compute_restarted}: {:?}", entries(&journal));
            assert_eq!(dev.status, BranchStatus::Ready, "{ctx}");
            assert_eq!(dev.error, None, "{ctx}");
            assert_eq!(dev.timeline_id.as_deref(), Some(new.as_str()), "{ctx}");
            assert_eq!(dev.pending_timeline_id, None, "{ctx}");
            assert_eq!(dev.superseded_timeline_id, None, "{ctx}");
            assert_eq!(endpoint_timeline(&w, &d.ep), new, "{ctx}");
            assert!(w.storage.timeline(&d.tenant, &new).is_some(), "{ctx}");
            assert!(
                w.storage.timeline(&d.tenant, &d.dev_timeline).is_none(),
                "{ctx}"
            );
            assert!(w.computes.list().await.unwrap().contains(&d.ep), "{ctx}");
            let delete_old = pos(
                &journal,
                &format!("storage.delete_timeline {}", d.dev_timeline),
            );
            assert!(
                delete_old < pos(&journal, "metadata.clear_superseded_timeline"),
                "{ctx}"
            );
            if compute_restarted {
                assert!(position(&journal, "compute.create").is_none(), "{ctx}");
            } else {
                // On the new timeline before the old one goes, with a fresh token.
                assert!(
                    pos(&journal, &format!("compute.create {}", d.ep)) < delete_old,
                    "{ctx}"
                );
                let token = w.computes.token_of(&d.ep).unwrap();
                assert_eq!(
                    sha256_hex(&token),
                    w.metadata.state().endpoints[&d.ep]
                        .endpoint
                        .compute_token_sha256
                );
                assert_ne!(sha256_hex(&token), old_hash);
            }
            // Only main's and dev's timelines remain.
            let mut expected = vec![d.main_timeline.clone(), new.clone()];
            expected.sort();
            assert_eq!(w.storage.timeline_ids(), expected, "{ctx}");
        }
    }

    // C3: reconcile's end of a reset or restore lifts the forwarded guards before its resync.
    #[tokio::test]
    async fn finish_switch_clears_the_guard_before_resync() {
        let (w, journal) = worker();
        let d = dev_setup(&w, &journal).await;
        let new = crash_before_switch(&w, &d);
        let switched = futures_util::FutureExt::now_or_never(
            w.metadata.switch_to_pending_timeline(d.dev, &new, None, "0/1"),
        )
        .expect("the fake never waits");
        assert!(switched.unwrap());
        set_branch(&w, d.dev, BranchStatus::Resetting, STALE_PROVISION_SECS + 1);
        journal.lock().unwrap().clear();

        w.reconcile_once().await.unwrap();

        assert_eq!(w.metadata.branch(d.dev).status, BranchStatus::Ready);
        let all = entries(&journal);
        let clear = pos(&journal, &format!("metadata.clear_forwarded {}", d.ep));
        assert_eq!(all[clear - 1], "metadata.lock_endpoint", "{all:?}");
        assert!(
            all[clear..].contains(&format!("metadata.role_copies {}", d.ep)),
            "{all:?}"
        );
    }

    // C3: an upgrade or a missing compute's re-creation keeps the catalog, so the forwarded
    // guards stay.
    #[tokio::test]
    async fn upgrade_does_not_clear_the_guard() {
        let (w, journal) = worker();
        let d = dev_setup(&w, &journal).await;
        journal.lock().unwrap().clear();

        w.execute(&Action::UpgradeCompute(d.dev, d.ep.clone()))
            .await
            .unwrap();
        w.computes.remove(&d.ep);
        w.execute(&Action::RecreateBranchCompute(d.dev, d.ep.clone()))
            .await
            .unwrap();

        let all = entries(&journal);
        assert_eq!(
            all.iter()
                .filter(|e| e.starts_with(&format!("compute.create {}", d.ep)))
                .count(),
            2,
            "{all:?}"
        );
        assert!(
            all.iter().any(|e| e.starts_with("metadata.role_copies")),
            "{all:?}"
        );
        assert!(
            !all.iter()
                .any(|e| e.starts_with("metadata.clear_forwarded")),
            "{all:?}"
        );
    }

    // A crash between creating the pending timeline and the switch: the pending timeline is
    // cleared (compare-and-swap) before it's deleted, the compute comes back on the old
    // timeline, and the branch is ready again.
    #[tokio::test]
    async fn finish_switch_after_a_crash_between_create_and_switch() {
        let (w, journal) = worker();
        let d = dev_setup(&w, &journal).await;
        let pending = crash_before_switch(&w, &d);
        w.metadata.state().branches.get_mut(&d.dev).unwrap().error =
            Some("creating timeline: earlier".into());

        w.reconcile_once().await.unwrap();

        let dev = w.metadata.branch(d.dev);
        assert_eq!(dev.status, BranchStatus::Ready, "{:?}", entries(&journal));
        assert_eq!(dev.timeline_id.as_deref(), Some(d.dev_timeline.as_str()));
        assert_eq!(dev.pending_timeline_id, None);
        // The reset didn't happen: its error stays.
        assert_eq!(dev.error.as_deref(), Some("creating timeline: earlier"));
        assert!(w.storage.timeline(&d.tenant, &pending).is_none());
        assert!(
            pos(&journal, "metadata.swap_pending_timeline")
                < pos(&journal, &format!("storage.delete_timeline {pending}"))
        );
        assert!(w.computes.list().await.unwrap().contains(&d.ep));
        assert_eq!(endpoint_timeline(&w, &d.ep), d.dev_timeline);
    }

    // The common case: the interrupted attempt recorded no error of its own, so the branch
    // says it was interrupted.
    #[tokio::test]
    async fn finish_switch_says_an_attempt_that_left_no_error_was_interrupted() {
        let (w, journal) = worker();
        let d = dev_setup(&w, &journal).await;
        crash_before_switch(&w, &d);

        w.reconcile_once().await.unwrap();

        let dev = w.metadata.branch(d.dev);
        assert_eq!(dev.status, BranchStatus::Ready, "{:?}", entries(&journal));
        assert_eq!(dev.timeline_id.as_deref(), Some(d.dev_timeline.as_str()));
        assert_eq!(
            dev.error.as_deref(),
            Some(crate::branch_switch::INTERRUPTED)
        );
        assert_eq!(
            crate::branch_switch::INTERRUPTED,
            "The last reset or restore was interrupted before it switched timelines; the \
             branch is unchanged. Try again."
        );
    }

    // What the attempt left is read under the claim: an error recorded after the first read
    // (here, just before the claim) is kept, not overwritten as an interruption.
    #[tokio::test]
    async fn finish_switch_reads_the_branch_again_under_its_claim() {
        let (w, journal) = worker();
        let d = dev_setup(&w, &journal).await;
        crash_before_switch(&w, &d);
        let dev = d.dev;
        w.metadata.state().before_claim_stale_reset = Some(Box::new(move |state| {
            state.branches.get_mut(&dev).unwrap().error =
                Some("creating timeline: recorded late".into());
        }));

        w.reconcile_once().await.unwrap();

        let dev = w.metadata.branch(d.dev);
        assert_eq!(dev.status, BranchStatus::Ready, "{:?}", entries(&journal));
        assert_eq!(
            dev.error.as_deref(),
            Some("creating timeline: recorded late")
        );
    }

    // Ruling 1: when the swap fails, another run (here a delete) owns the pending timeline,
    // and reconcile leaves it alone.
    #[tokio::test]
    async fn finish_switch_leaves_a_pending_timeline_it_could_not_clear() {
        let (w, journal) = worker();
        let d = dev_setup(&w, &journal).await;
        let pending = crash_before_switch(&w, &d);
        w.metadata.state().delete_before_write = Some(("swap_pending_timeline", d.dev));

        w.reconcile_once().await.unwrap();

        let dev = w.metadata.branch(d.dev);
        assert_eq!(dev.status, BranchStatus::Deleting);
        assert_eq!(dev.pending_timeline_id.as_deref(), Some(pending.as_str()));
        assert!(w.storage.timeline(&d.tenant, &pending).is_some());
        assert!(
            position(&journal, &format!("storage.delete_timeline {pending}")).is_none(),
            "{:?}",
            entries(&journal)
        );
        assert!(position(&journal, "compute.create").is_none());
    }

    // A pending timeline that can't be deleted is recorded again (so the next pass retries
    // it), the compute comes back on the old timeline, and the branch stays resetting.
    #[tokio::test]
    async fn finish_switch_keeps_a_pending_timeline_it_could_not_delete() {
        let (w, journal) = worker();
        let d = dev_setup(&w, &journal).await;
        let pending = crash_before_switch(&w, &d);
        *w.storage.fail.lock().unwrap() = Some("delete_timeline");

        w.reconcile_once().await.unwrap();

        let dev = w.metadata.branch(d.dev);
        assert_eq!(dev.status, BranchStatus::Resetting);
        assert_eq!(dev.pending_timeline_id.as_deref(), Some(pending.as_str()));
        let error = dev.error.unwrap_or_default();
        assert!(error.starts_with("deleting timeline: "), "{error:?}");
        assert!(error.contains(crate::fakes::STORAGE_CAUSE), "{error:?}");
        assert!(w.storage.timeline(&d.tenant, &pending).is_some());
        assert!(w.computes.list().await.unwrap().contains(&d.ep));

        *w.storage.fail.lock().unwrap() = None;
        set_branch(&w, d.dev, BranchStatus::Resetting, STALE_PROVISION_SECS + 1);
        w.reconcile_once().await.unwrap();
        let dev = w.metadata.branch(d.dev);
        assert_eq!(dev.status, BranchStatus::Ready);
        assert_eq!(dev.pending_timeline_id, None);
        assert!(w.storage.timeline(&d.tenant, &pending).is_none());
    }

    // Two passes (or a slow switch job and a pass) whose snapshots both saw the branch stale:
    // only the first claims it. Here the branch's age is refreshed, as another run's claim
    // would, before this run's `finish_switch`, which then does nothing.
    #[tokio::test]
    async fn finish_switch_leaves_a_branch_another_run_just_claimed() {
        let (w, journal) = worker();
        let d = dev_setup(&w, &journal).await;
        let pending = crash_before_switch(&w, &d);
        let hash = w.metadata.state().endpoints[&d.ep]
            .endpoint
            .compute_token_sha256
            .clone();
        set_branch(&w, d.dev, BranchStatus::Resetting, 0);
        journal.lock().unwrap().clear();

        w.finish_switch(d.dev).await.unwrap();

        let dev = w.metadata.branch(d.dev);
        assert_eq!(dev.status, BranchStatus::Resetting);
        assert_eq!(dev.pending_timeline_id.as_deref(), Some(pending.as_str()));
        assert_eq!(dev.error, None);
        assert!(w.storage.timeline(&d.tenant, &pending).is_some());
        assert!(!w.computes.list().await.unwrap().contains(&d.ep));
        assert_eq!(
            w.metadata.state().endpoints[&d.ep]
                .endpoint
                .compute_token_sha256,
            hash
        );
        let ctx = format!("{:?}", entries(&journal));
        for step in [
            "metadata.swap_pending_timeline",
            "metadata.replace_compute_token",
            "compute.create",
            "storage.delete_timeline",
        ] {
            assert!(position(&journal, step).is_none(), "{step}: {ctx}");
        }
    }

    // The claim refreshes the branch's age: a second run right after the first takes nothing.
    #[tokio::test]
    async fn finish_switch_claim_refreshes_the_branch() {
        let (w, journal) = worker();
        let d = dev_setup(&w, &journal).await;
        crash_before_switch(&w, &d);
        *w.storage.fail.lock().unwrap() = Some("delete_timeline");

        w.finish_switch(d.dev).await.unwrap_err();

        assert_eq!(w.metadata.branch(d.dev).status, BranchStatus::Resetting);
        assert_eq!(w.metadata.state().branch_ages.get(&d.dev).copied(), Some(0));
    }

    // Ruling 2: a crash between the API's claim and its publish leaves resetting with nothing
    // pending or superseded.
    #[tokio::test]
    async fn finish_switch_readies_a_claimed_branch_that_never_started() {
        for compute_missing in [false, true] {
            let (w, journal) = worker();
            let d = dev_setup(&w, &journal).await;
            if compute_missing {
                w.computes.remove(&d.ep);
            }
            set_branch(&w, d.dev, BranchStatus::Resetting, STALE_PROVISION_SECS + 1);

            w.reconcile_once().await.unwrap();

            let dev = w.metadata.branch(d.dev);
            assert_eq!(dev.status, BranchStatus::Ready, "{compute_missing}");
            assert_eq!(dev.timeline_id.as_deref(), Some(d.dev_timeline.as_str()));
            assert!(w.computes.list().await.unwrap().contains(&d.ep));
            assert_eq!(
                position(&journal, "compute.create").is_some(),
                compute_missing,
                "{:?}",
                entries(&journal)
            );
            assert!(position(&journal, "storage.delete_timeline").is_none());
        }
    }

    #[tokio::test]
    async fn reconcile_fails_a_stale_branch_create_and_tears_it_down() {
        let (w, journal) = worker();
        let d = dev_setup(&w, &journal).await;
        set_branch(
            &w,
            d.dev,
            BranchStatus::Provisioning,
            STALE_PROVISION_SECS + 1,
        );

        w.reconcile_once().await.unwrap();

        let dev = w.metadata.branch(d.dev);
        assert_eq!(dev.status, BranchStatus::Failed, "{:?}", entries(&journal));
        assert_eq!(dev.error.as_deref(), Some("provisioning timed out"));
        assert!(!w.computes.list().await.unwrap().contains(&d.ep));
        assert!(!w.metadata.endpoint_ids().contains(&d.ep));
        assert_eq!(
            w.storage.timeline_ids(),
            std::slice::from_ref(&d.main_timeline)
        );
        assert!(w.storage.has_tenant(&d.tenant));
        assert_eq!(w.metadata.database(d.db).status, DatabaseStatus::Ready);
    }

    #[tokio::test]
    async fn reconcile_retries_a_lost_branch_delete() {
        let (w, journal) = worker();
        let d = dev_setup(&w, &journal).await;
        set_branch(&w, d.dev, BranchStatus::Deleting, STALE_DELETE_SECS + 1);

        w.reconcile_once().await.unwrap();

        assert_eq!(w.metadata.branch(d.dev).status, BranchStatus::Deleted);
        assert!(!w.computes.list().await.unwrap().contains(&d.ep));
        assert!(!w.metadata.endpoint_ids().contains(&d.ep));
        assert_eq!(
            w.storage.timeline_ids(),
            std::slice::from_ref(&d.main_timeline)
        );
    }

    // A deleting branch that still has a child: Neon refuses to delete its timeline, and the
    // branch stays deleting, its timeline and its child untouched.
    #[tokio::test]
    async fn a_retried_branch_delete_leaves_a_branch_with_children() {
        let (w, journal) = worker();
        let d = dev_setup(&w, &journal).await;
        let child = ready_branch(&w, d.db, "feature", d.dev).await;
        let child_row = w.metadata.branch(child);
        set_branch(&w, d.dev, BranchStatus::Deleting, STALE_DELETE_SECS + 1);

        w.reconcile_once().await.unwrap();

        assert_eq!(w.metadata.branch(d.dev).status, BranchStatus::Deleting);
        assert!(w.storage.timeline(&d.tenant, &d.dev_timeline).is_some());
        assert_eq!(w.metadata.branch(child).status, BranchStatus::Ready);
        let child_timeline = child_row.timeline_id.unwrap();
        assert!(w.storage.timeline(&d.tenant, &child_timeline).is_some());
        let child_ep = child_row.endpoint_id.unwrap();
        assert!(w.computes.list().await.unwrap().contains(&child_ep));
    }

    #[tokio::test]
    async fn reconcile_recreates_a_missing_branch_compute() {
        let (w, journal) = worker();
        let d = dev_setup(&w, &journal).await;
        w.computes.remove(&d.ep);

        w.reconcile_once().await.unwrap();

        assert!(w.computes.list().await.unwrap().contains(&d.ep));
        assert!(
            pos(&journal, "metadata.replace_compute_token")
                < pos(&journal, &format!("compute.create {}", d.ep))
        );
    }

    #[tokio::test]
    async fn unknown_timelines_survive_reconcile() {
        let (w, journal) = worker();
        let d = dev_setup(&w, &journal).await;
        let stray = NeonId::new();
        w.storage
            .insert_timeline(&d.tenant, stray.as_str(), FRESH_HEAD);

        w.reconcile_once().await.unwrap();

        assert!(w.storage.timeline(&d.tenant, stray.as_str()).is_some());
        assert!(
            position(&journal, "storage.delete_timeline").is_none(),
            "{:?}",
            entries(&journal)
        );
    }

    // Includes a tenant whose pitr_interval was never set (every tenant made before step 5).
    #[tokio::test]
    async fn history_window_drift_is_patched() {
        let (w, journal) = worker();
        let unset = ready_database(&w).await;
        let changed = ready_database(&w).await;
        let in_sync = ready_database(&w).await;
        let tenant = |db| {
            w.metadata
                .database(db)
                .neon_tenant_id
                .unwrap()
                .parse::<NeonId>()
                .unwrap()
        };
        w.storage.pitr.lock().unwrap().remove(&tenant(unset));
        let hour = Duration::from_secs(3600);
        w.metadata
            .state()
            .databases
            .get_mut(&changed)
            .unwrap()
            .history_window = hour;
        journal.lock().unwrap().clear();

        w.reconcile_once().await.unwrap();

        let pitr = w.storage.pitr.lock().unwrap().clone();
        assert_eq!(pitr.get(&tenant(unset)), Some(&WEEK));
        assert_eq!(pitr.get(&tenant(changed)), Some(&hour));
        assert_eq!(pitr.get(&tenant(in_sync)), Some(&WEEK));
        assert!(
            position(
                &journal,
                &format!("storage.patch_tenant_config {}", tenant(unset))
            )
            .is_some()
        );
        assert!(
            position(
                &journal,
                &format!("storage.patch_tenant_config {} 3600s", tenant(changed))
            )
            .is_some()
        );
        assert!(
            position(
                &journal,
                &format!("storage.patch_tenant_config {}", tenant(in_sync))
            )
            .is_none(),
            "{:?}",
            entries(&journal)
        );
    }

    // One tenant's listing failing doesn't stop the pass.
    #[tokio::test]
    async fn a_failed_timeline_listing_reports_nothing_and_carries_on() {
        let (w, journal) = worker();
        let d = dev_setup(&w, &journal).await;
        w.computes.remove(&d.ep);
        *w.storage.fail.lock().unwrap() = Some("list_timelines");

        w.reconcile_once().await.unwrap();

        assert!(w.computes.list().await.unwrap().contains(&d.ep));
    }

    #[tokio::test]
    async fn a_fresh_branch_create_is_not_failed() {
        let (w, journal) = worker();
        let db = ready_database(&w).await;
        let dev = pending_branch(&w, db, "dev", BranchId::from(db));
        journal.lock().unwrap().clear();

        w.reconcile_once().await.unwrap();

        assert_eq!(w.metadata.branch(dev).status, BranchStatus::Pending);
        assert!(position(&journal, "metadata.fail_branch_if_stale").is_none());
    }

    #[test]
    fn a_database_missing_its_machinery_is_bootstrapped_alone() {
        let (mut s, dev) = with_dev(BranchStatus::Ready, 3600);
        s.needs_data_api_databases.insert((dev, "analytics".to_string()));
        s.needs_data_api_databases.insert((dev, "accounts".to_string()));
        assert_eq!(
            plan(&s),
            [
                Action::BootstrapDataApi { branch_id: dev, database: Some("accounts".into()) },
                Action::BootstrapDataApi { branch_id: dev, database: Some("analytics".into()) },
            ]
        );
        // The whole branch covers its databases.
        s.needs_data_api.insert(dev);
        assert_eq!(plan(&s), [Action::BootstrapDataApi { branch_id: dev, database: None }]);
    }

    #[test]
    fn stale_enabling_rows_are_retried_on_a_ready_branch_only() {
        let (mut s, dev) = with_dev(BranchStatus::Ready, 3600);
        s.stale_enabling.push((dev, "analytics".to_string()));
        assert_eq!(plan(&s), [Action::RetryEnableDataApi(dev, "analytics".to_string())]);
        let (mut s, dev) = with_dev(BranchStatus::Resetting, 0);
        s.stale_enabling.push((dev, "analytics".to_string()));
        assert!(plan(&s).iter().all(|a| !matches!(a, Action::RetryEnableDataApi(..))));
    }

    #[tokio::test]
    async fn an_enabling_row_is_stale_after_a_minute() {
        let (w, _journal) = worker();
        let a = (BranchId::new(), "a".to_string());
        let b = (BranchId::new(), "b".to_string());
        let t0 = std::time::Instant::now();
        let minute = Duration::from_secs(ENABLING_RETRY_SECS);

        assert!(w.stale_enabling_rows(vec![a.clone()], t0).is_empty(), "first seen");
        assert!(w.stale_enabling_rows(vec![a.clone(), b.clone()], t0 + minute / 2).is_empty());
        assert_eq!(w.stale_enabling_rows(vec![a.clone(), b.clone()], t0 + minute), std::slice::from_ref(&a));
        // Re-armed for another minute from the retry; `b` came due at 90 s.
        assert_eq!(w.stale_enabling_rows(vec![a.clone(), b.clone()], t0 + minute + minute / 2), std::slice::from_ref(&b));
        assert_eq!(
            w.stale_enabling_rows(vec![a.clone(), b.clone()], t0 + minute * 2 + minute / 2),
            [a.clone(), b.clone()]
        );
        // A row that left the list is forgotten, and starts over when it returns.
        assert!(w.stale_enabling_rows(vec![], t0 + minute * 3).is_empty());
        assert!(w.stale_enabling_rows(vec![a], t0 + minute * 3).is_empty());
    }

    #[tokio::test]
    async fn reconcile_retries_a_lost_enable() {
        let (w, journal) = worker();
        let (id, ep) = provision(&w, &journal).await;
        let main = BranchId::from(id);
        w.metadata.state().db_rows.get_mut(&main).unwrap().push(crate::fakes::FakeDbRow::new(
            "analytics",
            chelabase_metadata::DataApiState::Enabling,
            "reporting",
        ));
        w.computes.answer("to_regclass('chelabase.schema_version')", "t\n");
        // The resync in the same passes sees both databases.
        let d = |name: &str| format!("d|{}||{}|f|||||||\n", hex::encode(name), hex::encode("postgres"));
        w.computes.answer("FROM pg_authid", &[d("app"), d("analytics")].concat());

        // Seen just now: not yet.
        w.reconcile_once().await.unwrap();
        assert_eq!(
            w.metadata.state().db_rows[&main][1].state,
            chelabase_metadata::DataApiState::Enabling
        );
        // Seen over a minute ago: the bootstrap runs there and the row is `on`.
        let old = std::time::Instant::now()
            .checked_sub(Duration::from_secs(ENABLING_RETRY_SECS + 1))
            .unwrap();
        w.enabling_seen.lock().unwrap().insert((main, "analytics".to_string()), old);
        w.reconcile_once().await.unwrap();
        assert_eq!(w.metadata.state().db_rows[&main][1].state, chelabase_metadata::DataApiState::On);
        assert!(position(&journal, &format!("compute.sql {ep} analytics BEGIN;")).is_some());
    }

    #[tokio::test]
    async fn reconcile_checks_and_heals_each_served_database() {
        use chelabase_metadata::DataApiState::{Off, On};
        let (w, journal) = worker();
        let (id, ep) = provision(&w, &journal).await;
        let main = BranchId::from(id);
        for (name, state) in [("healthy", On), ("broken", On), ("off", Off)] {
            w.metadata
                .state()
                .db_rows
                .get_mut(&main)
                .unwrap()
                .push(crate::fakes::FakeDbRow::new(name, state, "reporting"));
        }
        let check = "to_regclass('chelabase.schema_version')";
        w.computes.answer_in("app", check, "t\n");
        w.computes.answer_in("healthy", check, "t\n");
        w.computes.answer_in("broken", check, "f\n");
        w.computes.sql.lock().unwrap().clear();

        w.reconcile_once().await.unwrap();

        let checked = |db: &str| position(&journal, &format!("compute.sql {ep} {db} SET search_path = pg_catalog, pg_temp; SELECT to_regclass")).is_some();
        assert!(checked("app") && checked("healthy") && checked("broken") && !checked("off"));
        // Each database is checked against its own default schema.
        let check_in = |db: &str| {
            w.computes
                .sql
                .lock()
                .unwrap()
                .iter()
                .find(|(_, d, s)| d == db && s.contains(check))
                .map(|(_, _, s)| s.clone())
                .unwrap()
        };
        assert!(check_in("healthy").contains("to_regnamespace('\"reporting\"')"));
        assert!(check_in("app").contains("to_regnamespace('\"public\"')"));
        let healed: Vec<String> = w
            .computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, _, s)| s.starts_with("BEGIN;\n") || s.contains("CREATE SCHEMA IF NOT EXISTS chelabase"))
            .map(|(_, d, _)| d.clone())
            .collect();
        assert_eq!(healed, ["broken"], "only the broken database is bootstrapped");
    }

    #[test]
    fn the_enabling_retry_waits_longer_than_an_enable_may_run() {
        assert!(ENABLING_RETRY_SECS > crate::data_api::ENABLE_DEADLINE.as_secs());
        assert_eq!(ENABLING_RETRY_SECS, 150);
    }

    /// The main database's health failure goes through the whole branch bootstrap, which also
    /// re-grants the secret keys' roles and ensures the public key.
    #[tokio::test]
    async fn a_failing_main_health_check_regrants_and_ensures_the_public_key() {
        let (w, journal) = worker();
        let (id, ep) = provision(&w, &journal).await;
        let main = BranchId::from(id);
        let generated = chelabase_types::data_key::generate(chelabase_types::data_key::DataKeyKind::Secret);
        w.metadata.state().data_keys.push(chelabase_metadata::NewDataKey {
            id: uuid::Uuid::new_v4(),
            branch_id: main,
            kind: chelabase_types::data_key::DataKeyKind::Secret,
            role: "reporting".to_string(),
            name: "reporting key".to_string(),
            prefix: generated.prefix,
            sha256: generated.sha256,
            sealed: None,
            databases: None,
            role_oid: None,
            role_timeline: None,
            role_bypass_rls: false,
        });
        w.computes.answer("pg_has_role", "ok 16384\n");
        w.computes.answer_in("app", "to_regclass('chelabase.schema_version')", "f\n");
        w.computes.sql.lock().unwrap().clear();

        w.reconcile_once().await.unwrap();

        let sql = w.computes.sql.lock().unwrap().clone();
        assert!(
            sql.iter().any(|(e, _, s)| *e == ep && s.starts_with("GRANT \"reporting\" TO authenticator")),
            "the secret key's role is granted again"
        );
    }
}
