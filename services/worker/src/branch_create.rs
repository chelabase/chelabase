//! Creating a branch: an endpoint row, a timeline branched from the parent's, a compute, and
//! the data API roles and bootstrap inside it.

use cheladb_metadata::{
    BranchKind, BranchRecord, BranchStatus, MetadataError, NewEndpoint, NewRoleCopy, RoleLevel,
    RoleOrigin,
};
use cheladb_nats::PointInTime;
use cheladb_neon::{ComputeBackend, Lsn, NeonId};
use cheladb_secrets::{Purpose, open_role_password, seal_credential, seal_role_password};
use cheladb_types::{BranchId, DatabaseId};
use tokio::time::Instant;
use tracing::{info, warn};

use crate::Worker;
use crate::branch_job::{Attempt, StepFailure};
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::provision::{
    bootstrap_sql, elapsed_ms, new_compute_token, new_endpoint_id, phase_done, sha256_hex,
};
use crate::resync::{FreshPasswords, SYSTEM_DATABASE};
use crate::{OWNER_ROLE, credentials};

/// Everything step 3 generated and recorded. Once it exists, a failure tears down.
struct Recorded {
    branch: BranchId,
    endpoint_id: String,
    tenant: NeonId,
    timeline: NeonId,
    /// Only ever in memory and in the container's environment.
    compute_token: String,
    database_name: String,
    password: String,
    /// The parent was protected when its copies were read: every role of the child's
    /// catalog with a password gets a fresh one (I3).
    protected: bool,
    /// `(name, verifier)` of each copy recorded with a fresh password (a protected parent's).
    fresh: Vec<(String, String)>,
}

/// One parent row as copied for the child.
struct ChildCopy {
    name: String,
    origin: RoleOrigin,
    verifier: Option<String>,
    sealed: Option<String>,
    login: bool,
    level: RoleLevel,
    api_owner_level: bool,
    /// A fresh password not yet set in the child's catalog.
    fresh: bool,
    /// The parent's sealed password didn't open: the row is marked unreadable (C4).
    unreadable: bool,
}

/// How many times step 3 re-reads the copies and inserts again after the parent's Protect
/// flag changed underneath it (C8).
const PROTECTION_RETRIES: usize = 2;

/// How [`Worker::build_branch`] ended when nothing failed.
enum Built {
    /// Steps 4-7 done.
    Compute,
    /// A delete removed the endpoint row meanwhile; stopped after the timeline.
    EndpointGone,
}

/// A delete has begun (or finished) on the branch.
pub(crate) fn deleted(branch: Option<&BranchRecord>) -> bool {
    branch.is_none_or(|b| matches!(b.status, BranchStatus::Deleting | BranchStatus::Deleted))
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// Builds the pending branch `branch_id` at `at` on its parent. A failure is recorded on
    /// the branch (and everything built is torn down); `Err` only means metadata trouble, and
    /// the job is retried.
    pub async fn handle_branch_create(
        &self,
        branch_id: BranchId,
        at: Option<PointInTime>,
    ) -> Result<(), String> {
        self.branch_create_attempt(branch_id, at, Attempt::LAST)
            .await
    }

    /// [`Self::handle_branch_create`] as delivery `attempt`.
    pub(crate) async fn branch_create_attempt(
        &self,
        branch_id: BranchId,
        at: Option<PointInTime>,
        attempt: Attempt,
    ) -> Result<(), String> {
        let started = Instant::now();
        let branch = self
            .metadata
            .get_branch(branch_id)
            .await
            .map_err(|e| e.to_string())?;
        let Some(branch) = branch else {
            warn!(%branch_id, "no branch row; dropping create job");
            return Ok(());
        };
        let Some(parent_id) = branch.parent_branch_id else {
            warn!(%branch_id, "main is provisioned with its database; dropping create job");
            return Ok(());
        };
        if branch.kind != BranchKind::Normal {
            warn!(%branch_id, "a backup is made by a restore; dropping create job");
            return Ok(());
        }
        if !matches!(
            branch.status,
            BranchStatus::Pending | BranchStatus::Provisioning
        ) {
            info!(%branch_id, status = branch.status.as_str(), "branch not pending; skipping");
            return Ok(());
        }
        // Step 1. Provisioning again is a redelivered job whose first attempt was cut short.
        let claimed = self
            .metadata
            .set_branch_status(
                branch_id,
                &[BranchStatus::Pending, BranchStatus::Provisioning],
                BranchStatus::Provisioning,
            )
            .await
            .map_err(|e| e.to_string())?;
        if !claimed {
            info!(%branch_id, "branch no longer pending; skipping");
            return Ok(());
        }
        info!(%branch_id, name = %branch.name, "creating branch");

        // Step 2. Nothing exists yet when it fails, so there's nothing to clean.
        let (tenant, parent_timeline) = match self.parent_timeline(&branch, parent_id).await? {
            Ok(found) => found,
            Err(err) => return self.fail_branch(branch_id, &err).await,
        };
        let lsn = match self
            .resolve_point(&tenant, parent_id, &parent_timeline, at.as_ref())
            .await
        {
            Ok(lsn) => lsn,
            Err(f) if f.retry(attempt) => return Err(f.message),
            Err(f) => return self.fail_branch(branch_id, &f.message).await,
        };
        // Step 3.
        let rec = match self.record_branch_endpoint(&branch, tenant).await {
            Ok(rec) => rec,
            Err(err) => return self.fail_branch(branch_id, &err).await,
        };
        // Steps 4-7.
        match self.build_branch(&rec, &parent_timeline, lsn).await {
            Ok(Built::Compute) => {
                self.finish_branch(branch_id, &rec, &parent_timeline, lsn, started)
                    .await
            }
            Ok(Built::EndpointGone) => {
                info!(%branch_id, "endpoint row gone while creating; tearing down");
                self.abandon_branch_gone(branch_id, &rec).await
            }
            Err(f) => {
                // Torn down first either way; a retry starts again from step 3.
                if self.teardown_branch_logged(&rec).await && f.retry(attempt) {
                    return Err(f.message);
                }
                self.fail_branch(branch_id, &f.message).await
            }
        }
    }

    /// Records a failure on the branch (a no-op once a delete began) and acks the job.
    async fn fail_branch(&self, id: BranchId, err: &str) -> Result<(), String> {
        warn!(branch_id = %id, "branch create failed: {err}");
        self.metadata
            .set_branch_failed(id, err)
            .await
            .map_err(|e| e.to_string())
    }

    /// The database's tenant and the parent's current timeline, or the branch's error.
    /// `Err` only for metadata trouble.
    pub(crate) async fn parent_timeline(
        &self,
        branch: &BranchRecord,
        parent_id: BranchId,
    ) -> Result<Result<(NeonId, NeonId), String>, String> {
        let tenant = self.tenant_of(branch.database_id).await?;
        let parent = self
            .metadata
            .get_branch(parent_id)
            .await
            .map_err(|e| e.to_string())?;
        let timeline = parent.and_then(|p| p.timeline_id);
        let (Some(tenant), Some(timeline)) = (tenant, timeline) else {
            return Ok(Err(
                "resolving the point: the parent branch has no timeline".to_string(),
            ));
        };
        Ok(timeline
            .parse()
            .map(|timeline| (tenant, timeline))
            .map_err(|e| format!("resolving the point: parent timeline {timeline:?}: {e}")))
    }

    /// Step 3: ids and secrets; the endpoint, its role and database, and the link to the
    /// branch (with its new timeline id), in one transaction. The insert re-checks the
    /// parent's Protect flag; when it changed since the copies were read, they're read again
    /// (at most [`PROTECTION_RETRIES`] more times, C8).
    async fn record_branch_endpoint(
        &self,
        branch: &BranchRecord,
        tenant: NeonId,
    ) -> Result<Recorded, String> {
        let database_name = self.main_database_name(branch.database_id).await?;
        let mut retries = 0;
        loop {
            match self
                .try_record_branch_endpoint(branch, &tenant, &database_name)
                .await
            {
                Err(MetadataError::ParentProtectionChanged) if retries < PROTECTION_RETRIES => {
                    retries += 1;
                    info!(branch_id = %branch.id, retries, "the parent's protection changed; reading its roles again");
                }
                Err(err) => return Err(format!("recording the endpoint: {err}")),
                Ok(Ok(rec)) => return Ok(rec),
                Ok(Err(err)) => return Err(err),
            }
        }
    }

    /// One try of [`Self::record_branch_endpoint`]: `Err` is the insert's error, `Ok(Err)`
    /// any other failure.
    async fn try_record_branch_endpoint(
        &self,
        branch: &BranchRecord,
        tenant: &NeonId,
        database_name: &str,
    ) -> Result<Result<Recorded, String>, MetadataError> {
        // The parent's role copies (the owner's excluded: it gets its own password).
        let (copies, protected) = match self.child_role_copies(branch).await {
            Ok(found) => found,
            Err(err) => return Ok(Err(err)),
        };
        let rec = Recorded {
            branch: branch.id,
            endpoint_id: new_endpoint_id(),
            tenant: tenant.clone(),
            timeline: NeonId::new(),
            compute_token: new_compute_token(),
            database_name: database_name.to_string(),
            password: credentials::generate_password(),
            protected,
            fresh: copies
                .iter()
                .filter(|c| c.fresh)
                .filter_map(|c| Some((c.name.clone(), c.verifier.clone()?)))
                .collect(),
        };
        let verifier = cheladb_neon::scram::verifier(&rec.password);
        let token_hash = sha256_hex(&rec.compute_token);
        let role_copies: Vec<NewRoleCopy<'_>> = copies
            .iter()
            .map(|c| NewRoleCopy {
                name: &c.name,
                origin: c.origin,
                verifier: c.verifier.as_deref(),
                password_enc: c.sealed.as_deref(),
                login: c.login,
                level: c.level,
                api_owner_level: c.api_owner_level,
                pending: c.fresh,
            })
            .collect();
        let endpoint = NewEndpoint {
            id: &rec.endpoint_id,
            tenant_id: rec.tenant.as_str(),
            timeline_id: rec.timeline.as_str(),
            compute_token_sha256: &token_hash,
            owner_role: OWNER_ROLE,
            owner_verifier: &verifier,
            database_name: &rec.database_name,
            role_copies: &role_copies,
        };
        let recorded = self
            .metadata
            .create_endpoint_for_branch(branch.id, rec.timeline.as_str(), &endpoint, rec.protected)
            .await?;
        if !recorded {
            // Already linked by an interrupted attempt (or a delete began, and then the failure
            // writes nothing). Reconcile tears down the endpoint of a failed branch.
            return Ok(Err(
                "recording the endpoint: a previous attempt was interrupted; its resources \
                 will be cleaned up"
                    .to_string(),
            ));
        }
        for copy in copies.iter().filter(|c| c.unreadable) {
            // Only the reveal is at stake: the row reads as "no password stored" meanwhile.
            if let Err(e) = self
                .metadata
                .mark_password_unreadable(&rec.endpoint_id, &copy.name)
                .await
            {
                warn!(branch_id = %branch.id, role = %copy.name, "marking the password unreadable: {e}");
            }
        }
        Ok(Ok(rec))
    }

    /// Each non-owner row of `branch`'s parent, copied for `branch` (none when the parent has
    /// no endpoint). The owner rule: a copy keeps the parent's verifier, its sealed password
    /// sealed again for `branch`; a protected parent's copies get a fresh password each
    /// (`fresh`). MD5 rows are copied as they are. Also whether the parent is protected.
    async fn child_role_copies(
        &self,
        branch: &BranchRecord,
    ) -> Result<(Vec<ChildCopy>, bool), String> {
        let Some(parent_id) = branch.parent_branch_id else {
            return Ok((Vec::new(), false));
        };
        // Read now (R17): a flag set after this point protects only later children.
        let parent = self
            .metadata
            .get_branch(parent_id)
            .await
            .map_err(|e| format!("recording the endpoint: reading the parent: {e}"))?;
        let protected = parent.as_ref().is_some_and(|p| p.protected);
        let Some(parent_ep) = parent.and_then(|p| p.endpoint_id) else {
            return Ok((Vec::new(), protected));
        };
        let (rows, _) = self
            .metadata
            .role_copies(&parent_ep)
            .await
            .map_err(|e| format!("recording the endpoint: reading the parent's roles: {e}"))?;
        let (parent_aad, child_aad) = (parent_id.to_string(), branch.id.to_string());
        let seal =
            |name: &str, password: &str| seal_role_password(&self.key, &child_aad, name, password);
        let copies = rows
            .into_iter()
            .filter(|r| r.origin != RoleOrigin::Owner && r.name != OWNER_ROLE)
            .map(|r| {
                let mut copy = ChildCopy {
                    name: r.name,
                    origin: r.origin,
                    verifier: r.scram_verifier,
                    sealed: None,
                    login: r.login,
                    level: r.level,
                    api_owner_level: r.api_owner_level,
                    fresh: false,
                    unreadable: false,
                };
                if protected && copy.verifier.is_some() {
                    let password = credentials::generate_password();
                    copy.verifier = Some(cheladb_neon::scram::verifier(&password));
                    copy.sealed = Some(seal(&copy.name, &password));
                    copy.fresh = true;
                } else if let Some(sealed) = r.password_enc {
                    match open_role_password(&self.key, &parent_aad, &copy.name, &sealed) {
                        Ok(password) => copy.sealed = Some(seal(&copy.name, &password)),
                        // The copy still works; only its reveal is lost.
                        Err(e) => {
                            warn!(
                                branch_id = %branch.id,
                                role = %copy.name,
                                "the parent's sealed password doesn't open ({e}); copied without it"
                            );
                            copy.unreadable = true;
                        }
                    }
                }
                copy
            })
            .collect();
        Ok((copies, protected))
    }

    /// The database name every branch shares: main's, from its endpoint (the database row's
    /// copy when main has none).
    async fn main_database_name(&self, db: DatabaseId) -> Result<String, String> {
        let main = self
            .metadata
            .get_branch(BranchId::from(db))
            .await
            .map_err(|e| format!("recording the endpoint: {e}"))?;
        if let Some(ep) = main.and_then(|m| m.endpoint_id) {
            let databases = self
                .metadata
                .neon_databases(&ep)
                .await
                .map_err(|e| format!("recording the endpoint: {e}"))?;
            if let Some(first) = databases.into_iter().next() {
                return Ok(first.name);
            }
        }
        self.metadata
            .get_database(db)
            .await
            .map_err(|e| format!("recording the endpoint: {e}"))?
            .and_then(|d| d.database_name)
            .ok_or_else(|| "recording the endpoint: main has no database name".to_string())
    }

    /// Steps 4-7: timeline, compute, readiness, bootstrap. Only the timeline's failure can be
    /// transient.
    async fn build_branch(
        &self,
        rec: &Recorded,
        parent_timeline: &NeonId,
        lsn: Lsn,
    ) -> Result<Built, StepFailure> {
        let phase = Instant::now();
        self.storage
            .create_branch_timeline(&rec.tenant, &rec.timeline, parent_timeline, lsn)
            .await
            .map_err(|e| StepFailure::storage("creating timeline", &e))?;
        phase_done(&rec.endpoint_id, "create_timeline", phase);
        self.build_compute(rec)
            .await
            .map_err(StepFailure::permanent)
    }

    /// Steps 5-7: compute, readiness, bootstrap.
    async fn build_compute(&self, rec: &Recorded) -> Result<Built, String> {
        let ep = rec.endpoint_id.as_str();
        // A delete that finished since step 3 found no timeline to delete, and took the
        // endpoint row with it: nothing else would ever remove this timeline.
        let endpoint = self
            .metadata
            .neon_endpoint(ep)
            .await
            .map_err(|e| format!("checking the endpoint: {e}"))?;
        if endpoint.is_none() {
            return Ok(Built::EndpointGone);
        }
        let phase = Instant::now();
        self.computes
            .create(ep, &rec.compute_token)
            .await
            .map_err(|e| format!("starting compute: {e:#}"))?;
        phase_done(ep, "computes.create", phase);
        let phase = Instant::now();
        self.wait_until_ready(ep, &rec.database_name).await?;
        phase_done(ep, "wait_until_ready", phase);
        let phase = Instant::now();
        // Only matters when the branch point predates the roles.
        self.computes
            .admin_sql(ep, &rec.database_name, &bootstrap_sql(self.owner_superuser))
            .await
            .map_err(|e| format!("bootstrapping roles: {e:#}"))?;
        let bootstrapped = self
            .bootstrap_served_on(rec.branch, ep, &rec.database_name, &[])
            .await
            .map_err(|e| format!("bootstrapping the data API: {e:#}"))?;
        phase_done(ep, "bootstrap", phase);
        // Before the resync, as in `recreate_compute`.
        self.record_compute_settings(ep).await;
        let phase = Instant::now();
        self.sync_child_roles(rec).await?;
        phase_done(ep, "resync", phase);
        // The resync wrote the child's `branch_databases` rows (all `off`): they take the
        // parent's flags and schemas now (P3), and the databases it serves are bootstrapped
        // (main again only when its schema changed).
        let phase = Instant::now();
        self.metadata
            .copy_parent_database_flags(rec.branch)
            .await
            .map_err(|e| format!("copying the parent's data API flags: {e}"))?;
        self.bootstrap_served_on(rec.branch, ep, &rec.database_name, &bootstrapped)
            .await
            .map_err(|e| format!("bootstrapping the data API: {e:#}"))?;
        phase_done(ep, "bootstrap_databases", phase);
        Ok(Built::Compute)
    }

    /// Under the endpoint's lock, the resync, which takes the catalog's SQL roles as they were
    /// at the branch point and re-creates API roles. A protected parent's child first gives
    /// every role of its own catalog with a password a fresh one (I3): the copies recorded
    /// with one keep theirs, any other role (one the parent dropped since a past branch point,
    /// one made after the copies were read, an MD5 role) gets a new one, stored sealed.
    async fn sync_child_roles(&self, rec: &Recorded) -> Result<(), String> {
        let ep = rec.endpoint_id.as_str();
        let _lock = self
            .metadata
            .lock_endpoint(ep)
            .await
            .map_err(|e| format!("syncing login roles: locking the endpoint: {e}"))?;
        if rec.protected {
            let mut fresh = FreshPasswords::new(rec.fresh.iter().cloned());
            self.resync_locked_with(rec.branch, ep, SYSTEM_DATABASE, Some(&mut fresh))
                .await
        } else {
            self.resync_locked(rec.branch, ep, SYSTEM_DATABASE).await
        }
    }

    /// The branch's teardown: its compute, its timeline, its endpoint row; never the tenant.
    /// A failure is logged only: the endpoint row stays, and reconcile retries.
    async fn teardown_branch_logged(&self, rec: &Recorded) -> bool {
        match self
            .teardown_branch_parts(
                Some(&rec.endpoint_id),
                Some(&rec.tenant),
                &[rec.timeline.as_str()],
            )
            .await
        {
            Ok(()) => true,
            Err(err) => {
                warn!(endpoint_id = %rec.endpoint_id, "teardown failed; reconcile will retry: {err}");
                false
            }
        }
    }

    /// Step 8: re-check for a delete that landed meanwhile, then seal and `set_branch_ready`,
    /// with where the history begins (the commit time at `lsn` on `parent_timeline`).
    async fn finish_branch(
        &self,
        id: BranchId,
        rec: &Recorded,
        parent_timeline: &NeonId,
        lsn: Lsn,
        started: Instant,
    ) -> Result<(), String> {
        let current = match self.metadata.get_branch(id).await {
            Ok(current) => current,
            Err(err) => {
                // Not ready yet, so tearing down is safe; the retried job starts afresh.
                self.teardown_branch_logged(rec).await;
                return Err(err.to_string());
            }
        };
        if deleted(current.as_ref()) {
            info!(branch_id = %id, "deleted while creating; tearing down");
            return self.abandon_branch(id, rec).await;
        }
        let sealed = seal_credential(&self.key, &id.to_string(), Purpose::Owner, &rec.password);
        let history_start = self
            .history_start_at(&rec.tenant, parent_timeline, lsn)
            .await;
        // An error here may hide a commit, so nothing is torn down: the retried job either
        // skips a ready branch, or fails to record a second endpoint and reconcile cleans up.
        let set = self
            .metadata
            .set_branch_ready(id, &sealed, Some(&lsn.to_string()), history_start)
            .await
            .map_err(|e| e.to_string())?;
        if !set {
            info!(branch_id = %id, "no longer provisioning just before ready; tearing down");
            return self.abandon_branch_gone(id, rec).await;
        }
        info!(branch_id = %id, endpoint_id = %rec.endpoint_id, %lsn, elapsed_ms = elapsed_ms(started), "branch ready");
        Ok(())
    }

    /// The endpoint row vanished after step 3, or `set_branch_ready` refused: a delete landed,
    /// or reconcile failed a stalled create. Tear down what this create built; mark the branch
    /// deleted only for a delete. A failed branch stays as reconcile left it.
    async fn abandon_branch_gone(&self, id: BranchId, rec: &Recorded) -> Result<(), String> {
        if !self.teardown_branch_logged(rec).await {
            return Ok(());
        }
        match self.metadata.get_branch(id).await {
            Ok(current) if deleted(current.as_ref()) => self.mark_branch_gone(id).await,
            Ok(current) => {
                let status = current.map(|b| b.status.as_str()).unwrap_or("missing");
                info!(branch_id = %id, status, "not a delete; leaving the branch as it is");
                Ok(())
            }
            Err(err) => {
                // Acked, not retried: a deleting branch is finished by its delete job.
                warn!(branch_id = %id, "re-reading the branch after teardown failed: {err}");
                Ok(())
            }
        }
    }

    /// A delete won the race: tear down, then mark the branch deleted. A failed teardown leaves
    /// it `deleting` for the delete job or reconcile to finish.
    async fn abandon_branch(&self, id: BranchId, rec: &Recorded) -> Result<(), String> {
        if self.teardown_branch_logged(rec).await {
            self.mark_branch_gone(id).await?;
        }
        Ok(())
    }

    async fn mark_branch_gone(&self, id: BranchId) -> Result<(), String> {
        self.metadata
            .mark_branch_deleted(id)
            .await
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use cheladb_metadata::{BranchStatus, DatabaseStatus};
    use cheladb_neon::storcon::{LsnAtTime, LsnKind};
    use cheladb_neon::{Lsn, NeonId};
    use cheladb_secrets::{Purpose, open_credential};
    use cheladb_types::DatabaseId;

    use super::*;
    use crate::fakes::{
        FRESH_HEAD, FakeWorker, Journal, STORAGE_CAUSE, entries, pending_branch, position,
        ready_database, worker,
    };
    use crate::provision::sha256_hex;
    use crate::roles::RoleOutcome;

    fn pos(journal: &Journal, prefix: &str) -> usize {
        position(journal, prefix)
            .unwrap_or_else(|| panic!("{prefix:?} not in journal {:?}", entries(journal)))
    }

    fn has(journal: &Journal, prefix: &str) -> bool {
        position(journal, prefix).is_some()
    }

    /// A ready database with a pending branch `dev` of main; the journal starts empty.
    struct Setup {
        db: DatabaseId,
        dev: BranchId,
        tenant: String,
        main_timeline: String,
        main_ep: String,
    }

    async fn setup(w: &FakeWorker, journal: &Journal) -> Setup {
        let db = ready_database(w).await;
        let main = w.metadata.main(db);
        let dev = pending_branch(w, db, "dev", main.id);
        journal.lock().unwrap().clear();
        Setup {
            db,
            dev,
            tenant: w.metadata.database(db).neon_tenant_id.unwrap(),
            main_timeline: main.timeline_id.unwrap(),
            main_ep: main.endpoint_id.unwrap(),
        }
    }

    /// Main, its compute and its timeline are untouched, and the tenant was never deleted.
    async fn assert_main_untouched(w: &FakeWorker, journal: &Journal, s: &Setup) {
        let main = w.metadata.main(s.db);
        assert_eq!(main.status, BranchStatus::Ready);
        assert_eq!(w.metadata.database(s.db).status, DatabaseStatus::Ready);
        assert!(w.storage.has_tenant(&s.tenant));
        assert!(w.storage.timeline(&s.tenant, &s.main_timeline).is_some());
        assert!(w.computes.list().await.unwrap().contains(&s.main_ep));
        assert!(
            !has(journal, "storage.delete_tenant"),
            "{:?}",
            entries(journal)
        );
        assert!(!has(journal, &format!("compute.delete {}", s.main_ep)));
        assert!(!has(
            journal,
            &format!("storage.delete_timeline {}", s.main_timeline)
        ));
    }

    /// The parts of a SCRAM verifier, to check it against a password.
    fn verifier_matches(verifier: &str, password: &str) -> bool {
        use base64::Engine;
        let rest = verifier.strip_prefix("SCRAM-SHA-256$").unwrap();
        let (iterations, rest) = rest.split_once(':').unwrap();
        let (salt, _) = rest.split_once('$').unwrap();
        let salt = base64::engine::general_purpose::STANDARD
            .decode(salt)
            .unwrap();
        cheladb_neon::scram::verifier_with_salt(password, &salt, iterations.parse().unwrap())
            == verifier
    }

    #[tokio::test]
    async fn create_branch_happy_path() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;

        w.handle_branch_create(s.dev, None).await.unwrap();

        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Ready, "{:?}", dev.error);
        assert_eq!(dev.error, None);
        assert_eq!(dev.branched_at_lsn.as_deref(), Some("0/2000000"));
        let ep = dev.endpoint_id.clone().unwrap();
        let timeline = dev.timeline_id.clone().unwrap();
        assert_ne!(ep, s.main_ep);
        assert_ne!(timeline, s.main_timeline);

        // The timeline branches from main's at the resolved LSN (main's head).
        let stored = w.storage.timeline(&s.tenant, &timeline).unwrap();
        let main_tl: NeonId = s.main_timeline.parse().unwrap();
        assert_eq!(stored.ancestor, Some((main_tl, FRESH_HEAD)));

        // The endpoint lives in the database's tenant, on the new timeline, with main's
        // database name and the owner role.
        let (verifier, token_hash) = {
            let state = w.metadata.state();
            let endpoint = &state.endpoints[&ep];
            assert_eq!(endpoint.endpoint.tenant_id, s.tenant);
            assert_eq!(endpoint.endpoint.timeline_id, timeline);
            assert_eq!(endpoint.databases.len(), 1);
            assert_eq!(endpoint.databases[0].name, "app");
            assert_eq!(endpoint.roles.len(), 1);
            assert_eq!(endpoint.roles[0].name, "postgres");
            (
                endpoint.roles[0].scram_verifier.clone(),
                endpoint.endpoint.compute_token_sha256.clone(),
            )
        };
        let token = w.computes.token_of(&ep).unwrap();
        assert_eq!(sha256_hex(&token), token_hash);

        // The sealed password opens with AAD = the branch id, and the verifier is its own.
        let password = open_credential(
            &w.key,
            &s.dev.to_string(),
            Purpose::Owner,
            dev.owner_password_enc.as_deref().unwrap(),
        )
        .unwrap();
        assert_eq!(password.len(), 32);
        assert!(verifier_matches(&verifier, &password));
        assert!(
            open_credential(
                &w.key,
                &s.db.to_string(),
                Purpose::Owner,
                dev.owner_password_enc.as_deref().unwrap()
            )
            .is_err(),
            "sealed for the branch, not main"
        );

        let order = [
            pos(&journal, "metadata.create_endpoint_for_branch"),
            pos(
                &journal,
                &format!(
                    "storage.create_branch_timeline {timeline} from {} at 0/2000000",
                    s.main_timeline
                ),
            ),
            pos(&journal, &format!("compute.create {ep}")),
            pos(&journal, &format!("compute.sql {ep} app SELECT 1")),
            pos(&journal, &format!("compute.sql {ep} app DO $$")),
            pos(&journal, "metadata.set_branch_ready"),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
        assert_main_untouched(&w, &journal, &s).await;
    }

    #[tokio::test]
    async fn history_starts_at_the_branch_points_commit_or_else_at_creation() {
        // A child made at a past point: its history starts at the commit at that point, before
        // the branch was created.
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        let commit: chrono::DateTime<chrono::Utc> = "2026-09-28T10:00:00Z".parse().unwrap();
        *w.storage.timestamp_at.lock().unwrap() = Some(commit);
        let at = PointInTime::Timestamp("2026-09-28T11:00:00Z".parse().unwrap());
        w.handle_branch_create(s.dev, Some(at)).await.unwrap();
        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Ready, "{:?}", dev.error);
        assert_eq!(dev.history_start, Some(commit));
        // Main has no ancestor: its history starts when it was created.
        let main = w.metadata.main(s.db);
        assert_eq!(main.history_start, Some(main.created_at));

        // No commit at the point, or no answer: when the branch was created.
        for failing in [false, true] {
            let (w, journal) = worker();
            let s = setup(&w, &journal).await;
            if failing {
                *w.storage.fail.lock().unwrap() = Some("timestamp_of_lsn");
            }
            w.handle_branch_create(s.dev, None).await.unwrap();
            let dev = w.metadata.branch(s.dev);
            assert_eq!(
                dev.status,
                BranchStatus::Ready,
                "{failing}: {:?}",
                dev.error
            );
            assert_eq!(dev.history_start, Some(dev.created_at), "{failing}");
        }
    }

    #[tokio::test]
    async fn create_at_each_point_kind() {
        let at_time = || {
            Some(PointInTime::Timestamp(
                "2026-09-30T11:00:00Z".parse().unwrap(),
            ))
        };
        type Arrange = fn(&FakeWorker);
        /// (case, arrange, at, Ok(the resolved LSN) or Err(the branch's error))
        type Case = (
            &'static str,
            Arrange,
            Option<PointInTime>,
            Result<&'static str, &'static str>,
        );
        let cases: [Case; 10] = [
            (
                "present",
                |w| {
                    *w.storage.lsn_at.lock().unwrap() = Some(LsnAtTime {
                        lsn: Lsn(0x0150_0000),
                        kind: LsnKind::Present,
                    })
                },
                at_time(),
                Ok("0/1500000"),
            ),
            (
                "future (the head)",
                |w| {
                    *w.storage.lsn_at.lock().unwrap() = Some(LsnAtTime {
                        lsn: Lsn(0x0900_0000),
                        kind: LsnKind::Future,
                    })
                },
                at_time(),
                Ok("0/2000000"),
            ),
            (
                "past",
                |w| {
                    *w.storage.lsn_at.lock().unwrap() = Some(LsnAtTime {
                        lsn: Lsn(0x0100_0000),
                        kind: LsnKind::Past,
                    });
                    *w.storage.timestamp_at.lock().unwrap() =
                        Some("2026-09-23T11:00:05Z".parse().unwrap());
                },
                at_time(),
                Err(
                    "resolving the point: outside the history window; the oldest available \
                     point is 2026-09-23T11:00:05Z",
                ),
            ),
            (
                // A tenant younger than its window: the oldest LSN is the bootstrap one, which
                // has no commit. The source's creation time stands in.
                "past, no timestamp at the oldest point",
                |w| {
                    *w.storage.lsn_at.lock().unwrap() = Some(LsnAtTime {
                        lsn: Lsn(0x0100_0000),
                        kind: LsnKind::Past,
                    });
                    created_all_at(w, "2026-09-29T08:00:00Z");
                },
                at_time(),
                Err(
                    "resolving the point: outside the history window; the oldest available \
                     point is 2026-09-29T08:00:00Z",
                ),
            ),
            (
                "past, the timestamp lookup fails",
                |w| {
                    *w.storage.lsn_at.lock().unwrap() = Some(LsnAtTime {
                        lsn: Lsn(0x0100_0000),
                        kind: LsnKind::Past,
                    });
                    *w.storage.fail.lock().unwrap() = Some("timestamp_of_lsn");
                    created_all_at(w, "2026-09-29T08:00:00Z");
                },
                at_time(),
                Err(
                    "resolving the point: outside the history window; the oldest available \
                     point is 2026-09-29T08:00:00Z",
                ),
            ),
            (
                "nodata",
                |w| {
                    *w.storage.lsn_at.lock().unwrap() = Some(LsnAtTime {
                        lsn: Lsn(0),
                        kind: LsnKind::Nodata,
                    })
                },
                at_time(),
                Err("resolving the point: no data at that time"),
            ),
            (
                "an LSN before the head",
                |_| {},
                Some(PointInTime::Lsn("0/1a2b3c".into())),
                Ok("0/1A2B3C"),
            ),
            (
                "the head's LSN",
                |_| {},
                Some(PointInTime::Lsn("0/2000000".into())),
                Ok("0/2000000"),
            ),
            (
                "an LSN ahead of the head",
                |_| {},
                Some(PointInTime::Lsn("0/2000001".into())),
                Err("resolving the point: the LSN 0/2000001 is ahead of the branch (at 0/2000000)"),
            ),
            (
                "Neon unreachable",
                |w| *w.storage.fail.lock().unwrap() = Some("lsn_by_timestamp"),
                at_time(),
                Err("resolving the point: injected lsn_by_timestamp failure: connection refused"),
            ),
        ];
        for (case, arrange, at, expected) in cases {
            let (w, journal) = worker();
            let s = setup(&w, &journal).await;
            arrange(&w);

            w.handle_branch_create(s.dev, at).await.unwrap();

            let dev = w.metadata.branch(s.dev);
            match expected {
                Ok(lsn) => {
                    assert_eq!(dev.status, BranchStatus::Ready, "{case}: {:?}", dev.error);
                    assert_eq!(dev.branched_at_lsn.as_deref(), Some(lsn), "{case}");
                    let timeline = dev.timeline_id.unwrap();
                    let stored = w.storage.timeline(&s.tenant, &timeline).unwrap();
                    assert_eq!(
                        stored.ancestor.map(|(_, at)| at.to_string()).as_deref(),
                        Some(lsn),
                        "{case}"
                    );
                }
                Err(message) => {
                    assert_eq!(dev.status, BranchStatus::Failed, "{case}");
                    assert_eq!(dev.error.as_deref(), Some(message), "{case}");
                    // Nothing was made, so there's nothing half-made left behind.
                    assert_eq!(dev.endpoint_id, None, "{case}");
                    assert_eq!(dev.timeline_id, None, "{case}");
                    assert_eq!(
                        w.metadata.endpoint_ids(),
                        std::slice::from_ref(&s.main_ep),
                        "{case}"
                    );
                    assert_eq!(
                        w.storage.timeline_ids(),
                        std::slice::from_ref(&s.main_timeline),
                        "{case}"
                    );
                    for entry in [
                        "metadata.create_endpoint_for_branch",
                        "storage.",
                        "compute.",
                        "metadata.set_branch_ready",
                    ] {
                        assert!(!has(&journal, entry), "{case}: {:?}", entries(&journal));
                    }
                }
            }
            assert_main_untouched(&w, &journal, &s).await;
        }
    }

    /// Sets every branch's `created_at`.
    fn created_all_at(w: &FakeWorker, at: &str) {
        let at = at.parse().unwrap();
        for b in w.metadata.state().branches.values_mut() {
            b.created_at = at;
        }
    }

    /// A ready `dev` of main, a pending `feat` of `dev`, every branch created at 08:00, and a
    /// storage controller that finds the asked-for time before dev's timeline starts on main's.
    async fn before_dev(w: &FakeWorker, journal: &Journal) -> (Setup, BranchId) {
        let s = setup(w, journal).await;
        w.handle_branch_create(s.dev, None).await.unwrap();
        let feat = pending_branch(w, s.db, "feat", s.dev);
        created_all_at(w, "2026-09-29T08:00:00Z");
        // The oldest LSN is where dev's timeline starts on main's: its ancestor LSN.
        *w.storage.lsn_at.lock().unwrap() = Some(LsnAtTime {
            lsn: FRESH_HEAD,
            kind: LsnKind::Past,
        });
        (s, feat)
    }

    fn history_start(w: &FakeWorker, id: BranchId, at: Option<&str>) {
        w.metadata
            .state()
            .branches
            .get_mut(&id)
            .unwrap()
            .history_start = at.map(|at| at.parse().unwrap());
    }

    async fn error_creating(w: &FakeWorker, feat: BranchId) -> String {
        let at = PointInTime::Timestamp("2026-09-28T11:00:00Z".parse().unwrap());
        w.handle_branch_create(feat, Some(at)).await.unwrap();
        let feat = w.metadata.branch(feat);
        assert_eq!(feat.status, BranchStatus::Failed);
        feat.error.unwrap()
    }

    #[tokio::test]
    async fn a_point_before_the_parents_history_says_to_branch_from_its_parent() {
        // The time is dev's history_start, else the commit at its branch point, else its
        // created_at.
        let cases = [
            (
                Some("2026-09-27T06:00:00Z"),
                Some("2026-09-26T00:00:00Z"),
                "2026-09-27T06:00:00Z",
            ),
            (None, Some("2026-09-26T00:00:00Z"), "2026-09-26T00:00:00Z"),
            (None, None, "2026-09-29T08:00:00Z"),
        ];
        for (start, commit, shown) in cases {
            let (w, journal) = worker();
            let (s, feat) = before_dev(&w, &journal).await;
            history_start(&w, s.dev, start);
            *w.storage.timestamp_at.lock().unwrap() = commit.map(|c| c.parse().unwrap());
            assert_eq!(
                error_creating(&w, feat).await,
                format!(
                    "resolving the point: before dev's history begins ({shown}); branch from \
                     main instead"
                ),
                "{start:?} {commit:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_point_before_a_backups_history_gives_no_advice() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        w.handle_branch_create(s.dev, None).await.unwrap();
        history_start(&w, s.dev, Some("2026-09-27T06:00:00Z"));
        let backup =
            crate::fakes::restored_by_hand(&w, s.dev, "dev-before-restore-20260930T120000Z");
        let feat = pending_branch(&w, s.db, "feat", backup);
        // The backup holds dev's old timeline, which starts at FRESH_HEAD on main's.
        *w.storage.lsn_at.lock().unwrap() = Some(LsnAtTime {
            lsn: FRESH_HEAD,
            kind: LsnKind::Past,
        });
        assert_eq!(
            error_creating(&w, feat).await,
            "resolving the point: before the history of dev-before-restore-20260930T120000Z \
             begins (2026-09-27T06:00:00Z)"
        );
    }

    #[tokio::test]
    async fn a_failure_at_each_step_cleans_up_and_names_the_step() {
        type Arrange = fn(&FakeWorker);
        let cases: [(&str, Arrange, &str); 5] = [
            (
                "create_endpoint",
                |w| w.metadata.state().fail = Some("create_endpoint_for_branch"),
                "recording the endpoint: ",
            ),
            (
                "create_branch_timeline",
                |w| *w.storage.fail.lock().unwrap() = Some("create_branch_timeline"),
                "creating timeline: ",
            ),
            (
                "compute create",
                |w| *w.computes.fail_create.lock().unwrap() = Some("no docker".into()),
                "starting compute: ",
            ),
            (
                "readiness",
                |w| *w.computes.sql_failures_left.lock().unwrap() = u32::MAX,
                "waiting for the compute: ",
            ),
            (
                "bootstrap",
                |w| {
                    *w.computes.fail_sql_containing.lock().unwrap() =
                        Some(("DO $$".into(), "syntax error".into()))
                },
                "bootstrapping roles: ",
            ),
        ];
        for (step, arrange, prefix) in cases {
            let (w, journal) = worker();
            let s = setup(&w, &journal).await;
            arrange(&w);

            w.handle_branch_create(s.dev, None).await.unwrap();

            let dev = w.metadata.branch(s.dev);
            assert_eq!(dev.status, BranchStatus::Failed, "{step}");
            let error = dev.error.unwrap_or_default();
            assert!(error.starts_with(prefix), "{step}: {error:?}");
            if step == "create_branch_timeline" {
                assert!(error.ends_with(STORAGE_CAUSE), "{step}: {error:?}");
            }
            // Everything the branch had is gone; main's are all there.
            assert_eq!(
                w.metadata.endpoint_ids(),
                std::slice::from_ref(&s.main_ep),
                "{step}"
            );
            assert_eq!(
                w.storage.timeline_ids(),
                std::slice::from_ref(&s.main_timeline),
                "{step}"
            );
            assert_eq!(
                w.computes.list().await.unwrap(),
                std::slice::from_ref(&s.main_ep),
                "{step}"
            );
            assert!(!has(&journal, "metadata.set_branch_ready"), "{step}");
            if step == "create_endpoint" {
                assert!(!has(&journal, "compute.delete"), "{step}");
                assert!(!has(&journal, "storage."), "{step}");
                assert!(!has(&journal, "metadata.delete_endpoint"), "{step}");
            } else {
                // The timeline is deleted, never the tenant.
                let order = [
                    pos(&journal, "compute.delete"),
                    pos(&journal, "storage.delete_timeline"),
                    pos(&journal, "metadata.delete_endpoint"),
                    pos(&journal, "metadata.set_branch_failed"),
                ];
                assert!(order.is_sorted(), "{step}: {:?}", entries(&journal));
            }
            assert_main_untouched(&w, &journal, &s).await;
        }
    }

    #[tokio::test]
    async fn a_teardown_failure_leaves_the_endpoint_for_reconcile() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        *w.computes.fail_create.lock().unwrap() = Some("no docker".into());
        *w.storage.fail.lock().unwrap() = Some("delete_timeline");

        w.handle_branch_create(s.dev, None).await.unwrap();

        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Failed);
        assert!(dev.error.unwrap().starts_with("starting compute: "));
        assert_eq!(w.metadata.endpoint_ids().len(), 2, "the branch's is left");
        assert!(!has(&journal, "metadata.delete_endpoint"));
        assert_main_untouched(&w, &journal, &s).await;
    }

    /// The branch ends deleted, and nothing it had is left.
    async fn assert_torn_down_and_deleted(w: &FakeWorker, journal: &Journal, s: &Setup) {
        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Deleted, "{:?}", entries(journal));
        assert_eq!(dev.owner_password_enc, None);
        assert_eq!(w.metadata.endpoint_ids(), std::slice::from_ref(&s.main_ep));
        assert_eq!(
            w.storage.timeline_ids(),
            std::slice::from_ref(&s.main_timeline)
        );
        assert_eq!(
            w.computes.list().await.unwrap(),
            std::slice::from_ref(&s.main_ep)
        );
        let order = [
            pos(journal, "storage.delete_timeline"),
            pos(journal, "metadata.delete_endpoint"),
            pos(journal, "metadata.mark_branch_deleted"),
        ];
        assert!(order.is_sorted(), "{:?}", entries(journal));
        assert!(!has(journal, "metadata.set_branch_failed"));
        assert_main_untouched(w, journal, s).await;
    }

    // Review Focus 1.
    #[tokio::test]
    async fn a_delete_seen_at_the_recheck_tears_down() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        // The first get_branch of dev is the skip check; the second is the re-check.
        w.metadata.state().deleting_on_get_branch = Some((s.dev, 2));

        w.handle_branch_create(s.dev, None).await.unwrap();

        assert!(has(&journal, "compute.create"));
        assert!(pos(&journal, "compute.create") < pos(&journal, "compute.delete"));
        assert!(!has(&journal, "metadata.set_branch_ready"));
        assert_torn_down_and_deleted(&w, &journal, &s).await;
    }

    // Review Focus 1.
    #[tokio::test]
    async fn a_delete_racing_set_ready_tears_down() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        w.metadata.state().deleting_before_set_ready = true;

        w.handle_branch_create(s.dev, None).await.unwrap();

        assert!(pos(&journal, "metadata.set_branch_ready") < pos(&journal, "compute.delete"));
        assert_torn_down_and_deleted(&w, &journal, &s).await;
    }

    // Review Focus 1: the delete job ran to completion between recording the endpoint and
    // creating the timeline, so it found no timeline to delete.
    #[tokio::test]
    async fn a_delete_before_the_timeline_exists_leaves_no_timeline() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        let state = w.metadata.state.clone();
        let dev = s.dev;
        *w.storage.on_create_branch_timeline.lock().unwrap() = Some(Box::new(move || {
            // What handle_branch_delete leaves: no endpoint row, the branch a tombstone.
            let mut state = state.lock().unwrap();
            let branch = state.branches.get_mut(&dev).unwrap();
            let ep = branch.endpoint_id.take().unwrap();
            branch.status = BranchStatus::Deleted;
            state.endpoints.remove(&ep);
        }));

        w.handle_branch_create(s.dev, None).await.unwrap();

        assert!(
            pos(&journal, "storage.create_branch_timeline")
                < pos(&journal, "storage.delete_timeline"),
            "{:?}",
            entries(&journal)
        );
        assert!(!has(&journal, "compute.create"));
        assert!(!has(&journal, "metadata.set_branch_ready"));
        assert_torn_down_and_deleted(&w, &journal, &s).await;
    }

    // Reconcile failed a branch that ran past the stale limit: create tears down what it built,
    // and the branch stays failed rather than becoming a tombstone.
    #[tokio::test]
    async fn a_stale_failure_before_set_ready_keeps_the_branch_failed() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        let state = w.metadata.state.clone();
        let dev = s.dev;
        *w.storage.on_create_branch_timeline.lock().unwrap() = Some(Box::new(move || {
            let mut state = state.lock().unwrap();
            let branch = state.branches.get_mut(&dev).unwrap();
            branch.status = BranchStatus::Failed;
            branch.error = Some("provisioning timed out".into());
        }));

        w.handle_branch_create(s.dev, None).await.unwrap();

        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Failed);
        assert_eq!(dev.error.as_deref(), Some("provisioning timed out"));
        assert!(has(&journal, "metadata.set_branch_ready"));
        assert!(!has(&journal, "metadata.mark_branch_deleted"));
        assert_eq!(w.metadata.endpoint_ids(), std::slice::from_ref(&s.main_ep));
        assert_eq!(
            w.storage.timeline_ids(),
            std::slice::from_ref(&s.main_timeline)
        );
        assert_main_untouched(&w, &journal, &s).await;
    }

    // A failed teardown inside abandon leaves the branch deleting and its endpoint row, for the
    // delete job or reconcile to finish; the job itself is done.
    #[tokio::test]
    async fn a_teardown_failure_while_abandoning_leaves_the_branch_deleting() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        w.metadata.state().deleting_before_set_ready = true;
        *w.storage.fail.lock().unwrap() = Some("delete_timeline");

        w.handle_branch_create(s.dev, None).await.unwrap();

        assert_eq!(w.metadata.branch(s.dev).status, BranchStatus::Deleting);
        assert_eq!(w.metadata.endpoint_ids().len(), 2);
        assert!(!has(&journal, "metadata.mark_branch_deleted"));
        assert!(!has(&journal, "metadata.set_branch_failed"));
        assert_main_untouched(&w, &journal, &s).await;
    }

    #[tokio::test]
    async fn branches_not_pending_main_and_missing_rows_are_skipped() {
        for status in [
            BranchStatus::Ready,
            BranchStatus::Resetting,
            BranchStatus::Failed,
            BranchStatus::Deleting,
            BranchStatus::Deleted,
        ] {
            let (w, journal) = worker();
            let s = setup(&w, &journal).await;
            w.metadata.state().branches.get_mut(&s.dev).unwrap().status = status;
            w.handle_branch_create(s.dev, None).await.unwrap();
            assert_eq!(w.metadata.branch(s.dev).status, status);
            assert!(
                entries(&journal).is_empty(),
                "{status:?}: {:?}",
                entries(&journal)
            );
        }
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        w.handle_branch_create(BranchId::new(), None).await.unwrap();
        // Main is provisioned with its database, never as a branch.
        w.handle_branch_create(BranchId::from(s.db), None)
            .await
            .unwrap();
        assert!(entries(&journal).is_empty(), "{:?}", entries(&journal));
        assert_main_untouched(&w, &journal, &s).await;
    }

    #[tokio::test]
    async fn metadata_trouble_is_retried() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        w.metadata.state().fail = Some("get_branch");
        assert!(w.handle_branch_create(s.dev, None).await.is_err());
        w.metadata.state().fail = Some("set_branch_status");
        assert!(w.handle_branch_create(s.dev, None).await.is_err());
    }

    // --- Login roles -------------------------------------------------------------------

    /// One `neon_roles` row (owner excluded) as the fake holds it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Row {
        origin: RoleOrigin,
        level: RoleLevel,
        api_owner_level: bool,
        login: bool,
        verifier: Option<String>,
        sealed: Option<String>,
        pending: bool,
    }

    fn row(w: &FakeWorker, ep: &str, name: &str) -> Option<Row> {
        let state = w.metadata.state();
        let endpoint = &state.endpoints[ep];
        let login = endpoint.logins.get(name)?;
        Some(Row {
            origin: login.origin,
            level: login.level,
            api_owner_level: login.api_owner_level,
            login: login.login,
            verifier: endpoint.verifier(name),
            sealed: login.password_enc.clone(),
            pending: login.pending,
        })
    }

    /// Adds an `sql` row to `ep` (sealed for `branch` when `password` is given; `None`
    /// verifier for an MD5 role).
    fn sql_row(
        w: &FakeWorker,
        branch: BranchId,
        ep: &str,
        name: &str,
        verifier: Option<&str>,
        password: Option<&str>,
    ) {
        let sealed = password.map(|p| seal_role_password(&w.key, &branch.to_string(), name, p));
        let mut state = w.metadata.state();
        let endpoint = state.endpoints.get_mut(ep).unwrap();
        endpoint.logins.insert(
            name.to_string(),
            crate::fakes::FakeLogin {
                origin: RoleOrigin::Sql,
                password_enc: sealed,
                pending: false,
                login: true,
                level: RoleLevel::Plain,
                api_owner_level: false,
            },
        );
        endpoint.set_verifier(name, verifier);
    }

    /// One `r` line of the resync's catalog read, as psql prints it.
    fn r_line(name: &str, password: &str, owner_level: bool) -> String {
        let tf = if owner_level { "t" } else { "f" };
        format!(
            "r|{}|t|{}|{tf}|t|{tf}\n",
            hex::encode(name),
            hex::encode(password),
        )
    }

    const MD5: &str = "md5ffffffffffffffffffffffffffffffff";
    /// Needle: the resync's catalog read.
    const CATALOG_READ: &str = "SELECT 'r',";

    /// Main with an API `app` (plain) and `admin` (owner-level), an SQL `sqlrole` (revealable:
    /// it was reset through the API) and an MD5 `md5role`, and a pending `dev` of main. The
    /// child's catalog read answers the parent's roles and hashes.
    async fn with_login_roles(w: &FakeWorker, journal: &Journal) -> Setup {
        let s = setup(w, journal).await;
        let main = BranchId::from(s.db);
        assert_eq!(w.create_role(main, "app", false).await, RoleOutcome::Ok);
        assert_eq!(w.create_role(main, "admin", true).await, RoleOutcome::Ok);
        let sql_verifier = cheladb_neon::scram::verifier("sql-password");
        sql_row(
            w,
            main,
            &s.main_ep,
            "sqlrole",
            Some(&sql_verifier),
            Some("sql-password"),
        );
        sql_row(w, main, &s.main_ep, "md5role", None, None);
        let verifier = |name: &str| row(w, &s.main_ep, name).unwrap().verifier.unwrap();
        let catalog = [
            r_line("admin", &verifier("admin"), true),
            r_line("app", &verifier("app"), false),
            r_line("md5role", MD5, false),
            r_line("sqlrole", &sql_verifier, false),
        ]
        .concat();
        w.computes.answer(CATALOG_READ, &catalog);
        journal.lock().unwrap().clear();
        s
    }

    fn open(w: &FakeWorker, branch: BranchId, name: &str, sealed: &str) -> Option<String> {
        cheladb_secrets::open_role_password(&w.key, &branch.to_string(), name, sealed).ok()
    }

    #[tokio::test]
    async fn child_copies_keep_parent_passwords() {
        let (w, journal) = worker();
        let s = with_login_roles(&w, &journal).await;
        let main = BranchId::from(s.db);

        w.handle_branch_create(s.dev, None).await.unwrap();

        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Ready, "{:?}", dev.error);
        let dev_ep = dev.endpoint_id.unwrap();
        for name in ["app", "admin", "sqlrole"] {
            let parent = row(&w, &s.main_ep, name).unwrap();
            let child = row(&w, &dev_ep, name).unwrap();
            let shape = |r: &Row| (r.origin, r.level, r.api_owner_level, r.login);
            assert_eq!(shape(&child), shape(&parent), "{name}");
            assert_eq!(child.verifier, parent.verifier, "{name}");
            assert!(!child.pending, "{name}");
            let (p_sealed, c_sealed) = (parent.sealed.unwrap(), child.sealed.unwrap());
            assert_ne!(c_sealed, p_sealed, "{name}");
            // Sealed again for the child: only the child's AAD opens it, to the same password.
            let password = open(&w, s.dev, name, &c_sealed).expect(name);
            assert_eq!(Some(password), open(&w, main, name, &p_sealed), "{name}");
            assert!(open(&w, main, name, &c_sealed).is_none(), "{name}");
        }
        // The MD5 role is copied as it is.
        let md5 = row(&w, &dev_ep, "md5role").unwrap();
        assert_eq!(
            (md5.origin, md5.verifier, md5.sealed),
            (RoleOrigin::Sql, None, None)
        );
        // Nothing was set in SQL: the child's catalog already has the parent's hashes.
        let sql = w.computes.sql.lock().unwrap().clone();
        assert!(
            !sql.iter()
                .any(|(ep, _, s)| *ep == dev_ep && s.contains("PASSWORD 'SCRAM")),
            "{sql:?}"
        );
    }

    fn protect(w: &FakeWorker, branch: BranchId) {
        w.metadata
            .state()
            .branches
            .get_mut(&branch)
            .unwrap()
            .protected = true;
    }

    /// The scripts that set the protected child's fresh passwords, on `dev_ep` (one per role,
    /// C1), joined.
    fn fresh_script(w: &FakeWorker, dev_ep: &str) -> String {
        let sql = w.computes.sql.lock().unwrap().clone();
        let scripts: Vec<&String> = sql
            .iter()
            .filter(|(ep, _, s)| {
                ep == dev_ep
                    && s.starts_with("SET search_path = pg_catalog, pg_temp;\nALTER ROLE \"")
            })
            .map(|(_, _, s)| s)
            .collect();
        assert!(!scripts.is_empty(), "no script: {sql:?}");
        for script in &scripts {
            assert_eq!(script.lines().count(), 2, "one role per script: {script}");
        }
        scripts
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The child's row for `name`: its verifier is new, its sealed password (the child's AAD)
    /// opens to a password that matches it, it's confirmed, and `script` set it.
    fn assert_fresh(
        w: &FakeWorker,
        dev: BranchId,
        dev_ep: &str,
        name: &str,
        old: &str,
        script: &str,
    ) -> Row {
        let child = row(w, dev_ep, name).unwrap_or_else(|| panic!("{name} has a row"));
        let verifier = child
            .verifier
            .clone()
            .unwrap_or_else(|| panic!("{name}: a verifier"));
        assert_ne!(verifier, old, "{name}");
        let sealed = child
            .sealed
            .clone()
            .unwrap_or_else(|| panic!("{name}: revealable"));
        let password = open(w, dev, name, &sealed).expect(name);
        assert!(verifier_matches(&verifier, &password), "{name}");
        assert!(!child.pending, "{name}: the resync confirms it");
        assert!(
            script.contains(&format!("ALTER ROLE \"{name}\" PASSWORD '{verifier}';")),
            "{name}: {script}"
        );
        child
    }

    // I3: every role with a password in the CHILD's catalog gets a fresh one, not only the
    // parent's copies read before the branch point: a role the copies don't have (dropped on
    // the parent since a past point, or made after the read), an MD5 role and a NOLOGIN role
    // with a password too. A role without a password is left alone; a missing API role is
    // re-created by the resync with its fresh password.
    #[tokio::test]
    async fn protected_parent_gives_fresh_passwords_and_alters_them() {
        let (w, journal) = worker();
        let s = with_login_roles(&w, &journal).await;
        let main = BranchId::from(s.db);
        protect(&w, main);
        let parent: Vec<(&str, String)> = ["app", "admin", "sqlrole"]
            .iter()
            .map(|n| (*n, row(&w, &s.main_ep, n).unwrap().verifier.unwrap()))
            .collect();
        let old = |name: &str| parent.iter().find(|(n, _)| *n == name).unwrap().1.clone();
        let late = cheladb_neon::scram::verifier("late-password");
        let group = cheladb_neon::scram::verifier("group-password");
        // The child's catalog at the branch point: `admin` is missing; `late` and `grp` aren't
        // among the parent's copies.
        w.computes.answers.lock().unwrap().clear();
        let catalog = [
            r_line("app", &old("app"), false),
            r_line("late", &late, false),
            r_line("md5role", MD5, false),
            r_line("sqlrole", &old("sqlrole"), false),
            format!("r|{}|f|{}|f|t|f\n", hex::encode("grp"), hex::encode(&group)),
            format!("r|{}|t||f|t|f\n", hex::encode("nopass")),
        ]
        .concat();
        w.computes.answer(CATALOG_READ, &catalog);

        w.handle_branch_create(s.dev, None).await.unwrap();

        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Ready, "{:?}", dev.error);
        let dev_ep = dev.endpoint_id.unwrap();
        let script = fresh_script(&w, &dev_ep);
        assert!(
            script.starts_with("SET search_path = pg_catalog, pg_temp;\n"),
            "{script}"
        );
        for (name, old, origin) in [
            ("app", old("app"), RoleOrigin::Api),
            ("sqlrole", old("sqlrole"), RoleOrigin::Sql),
            ("md5role", MD5.to_string(), RoleOrigin::Sql),
            ("late", late.clone(), RoleOrigin::Sql),
            ("grp", group.clone(), RoleOrigin::Sql),
        ] {
            let child = assert_fresh(&w, s.dev, &dev_ep, name, &old, &script);
            assert_eq!(child.origin, origin, "{name}");
        }
        assert!(!row(&w, &dev_ep, "grp").unwrap().login);
        assert!(!script.contains("\"admin\""), "{script}");
        assert!(!script.contains("nopass"), "{script}");
        assert!(row(&w, &dev_ep, "nopass").is_none());
        // The missing API role is re-created with its fresh password.
        let admin = row(&w, &dev_ep, "admin").unwrap();
        let admin_verifier = admin.verifier.unwrap();
        assert_ne!(admin_verifier, old("admin"));
        let password = open(&w, s.dev, "admin", &admin.sealed.unwrap()).unwrap();
        assert!(verifier_matches(&admin_verifier, &password));
        let sql = w.computes.sql.lock().unwrap().clone();
        assert!(
            sql.iter().any(|(ep, _, s)| *ep == dev_ep
                && s.contains(&format!("CREATE ROLE \"admin\" LOGIN CREATEROLE CREATEDB BYPASSRLS REPLICATION PASSWORD '{admin_verifier}'"))),
            "{sql:?}"
        );
        // The catalog read, then the fresh passwords, all in `postgres`, before the write.
        let on_dev = |needle: &str| {
            sql.iter()
                .position(|(ep, _, s)| *ep == dev_ep && s.contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} not in {sql:?}"))
        };
        assert!(on_dev(CATALOG_READ) < on_dev("ALTER ROLE \"app\" PASSWORD"));
        assert_eq!(sql[on_dev("ALTER ROLE \"app\" PASSWORD")].1, "postgres");
        // In the journal: the catalog read and the script (both start with the search path),
        // then the write.
        let all = entries(&journal);
        let on_postgres = format!("compute.sql {dev_ep} postgres SET search_path");
        let reads: Vec<usize> = (0..all.len())
            .filter(|&i| all[i].starts_with(&on_postgres))
            .collect();
        assert!(reads.len() >= 2, "{all:?}");
        assert!(reads[1] < pos(&journal, "metadata.apply_resync"), "{all:?}");
        // The parent keeps its passwords.
        for (name, verifier) in &parent {
            assert_eq!(
                row(&w, &s.main_ep, name).unwrap().verifier.as_ref(),
                Some(verifier)
            );
        }
    }

    // A protected parent whose copies were empty when they were read still protects: the
    // child's catalog is what counts.
    #[tokio::test]
    async fn protected_parent_without_copies_still_gives_fresh_passwords() {
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        protect(&w, BranchId::from(s.db));
        let late = cheladb_neon::scram::verifier("late-password");
        w.computes
            .answer(CATALOG_READ, &r_line("late", &late, false));

        w.handle_branch_create(s.dev, None).await.unwrap();

        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Ready, "{:?}", dev.error);
        let dev_ep = dev.endpoint_id.unwrap();
        let script = fresh_script(&w, &dev_ep);
        let child = assert_fresh(&w, s.dev, &dev_ep, "late", &late, &script);
        assert_eq!(child.origin, RoleOrigin::Sql);
    }

    #[tokio::test]
    async fn protected_copy_names_with_quotes_are_quoted() {
        const NAME: &str = "it's \"q\"";
        let (w, journal) = worker();
        let s = setup(&w, &journal).await;
        let main = BranchId::from(s.db);
        protect(&w, main);
        let verifier = cheladb_neon::scram::verifier("sql-password");
        sql_row(&w, main, &s.main_ep, NAME, Some(&verifier), None);
        w.computes
            .answer(CATALOG_READ, &r_line(NAME, &verifier, false));

        w.handle_branch_create(s.dev, None).await.unwrap();

        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Ready, "{:?}", dev.error);
        let dev_ep = dev.endpoint_id.unwrap();
        let script = fresh_script(&w, &dev_ep);
        assert!(
            script.contains("\nALTER ROLE \"it's \"\"q\"\"\" PASSWORD 'SCRAM-SHA-256$"),
            "{script}"
        );
        assert!(!script.contains(&verifier), "a fresh verifier: {script}");
    }

    fn count(journal: &Journal, prefix: &str) -> usize {
        entries(journal)
            .iter()
            .filter(|e| e.starts_with(prefix))
            .count()
    }

    // C8: Protect toggled between the copies' read and the insert refuses the insert; the
    // copies are read again (now protected: fresh passwords) and the insert retried.
    #[tokio::test]
    async fn child_create_retries_on_changed_protection() {
        let (w, journal) = worker();
        let s = with_login_roles(&w, &journal).await;
        let parent: Vec<(&str, String)> = ["app", "sqlrole"]
            .iter()
            .map(|n| (*n, row(&w, &s.main_ep, n).unwrap().verifier.unwrap()))
            .collect();
        w.metadata.state().protection_flips = 1;

        w.handle_branch_create(s.dev, None).await.unwrap();

        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Ready, "{:?}", dev.error);
        assert_eq!(count(&journal, "metadata.create_endpoint_for_branch"), 2);
        let dev_ep = dev.endpoint_id.unwrap();
        let script = fresh_script(&w, &dev_ep);
        for (name, old) in &parent {
            assert_fresh(&w, s.dev, &dev_ep, name, old, &script);
        }
        assert_eq!(w.metadata.endpoint_ids().len(), 2);
    }

    // C8: at most two retries; then the branch fails and nothing was recorded.
    #[tokio::test]
    async fn child_create_gives_up_when_protection_keeps_changing() {
        let (w, journal) = worker();
        let s = with_login_roles(&w, &journal).await;
        w.metadata.state().protection_flips = 3;

        w.handle_branch_create(s.dev, None).await.unwrap();

        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Failed);
        let error = dev.error.unwrap();
        assert!(error.starts_with("recording the endpoint: "), "{error}");
        assert_eq!(count(&journal, "metadata.create_endpoint_for_branch"), 3);
        assert_eq!(w.metadata.endpoint_ids(), std::slice::from_ref(&s.main_ep));
    }

    // C4: a parent's sealed password that doesn't open is dropped from the copy (the copy
    // still works), and the child's row is marked unreadable after the insert.
    #[tokio::test]
    async fn child_create_marks_unopenable_passwords_unreadable() {
        let (w, journal) = worker();
        let s = with_login_roles(&w, &journal).await;
        // Sealed for another branch: the parent's AAD doesn't open it.
        let wrong = seal_role_password(&w.key, &BranchId::new().to_string(), "sqlrole", "pw");
        w.metadata
            .state()
            .endpoints
            .get_mut(&s.main_ep)
            .unwrap()
            .logins
            .get_mut("sqlrole")
            .unwrap()
            .password_enc = Some(wrong);

        w.handle_branch_create(s.dev, None).await.unwrap();

        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Ready, "{:?}", dev.error);
        let dev_ep = dev.endpoint_id.unwrap();
        let mark = pos(
            &journal,
            &format!("metadata.mark_password_unreadable {dev_ep} sqlrole"),
        );
        assert!(pos(&journal, "metadata.create_endpoint_for_branch") < mark);
        assert_eq!(count(&journal, "metadata.mark_password_unreadable"), 1);
        let sqlrole = row(&w, &dev_ep, "sqlrole").unwrap();
        assert_eq!(sqlrole.sealed, None);
        assert!(sqlrole.verifier.is_some(), "the copy still works");
        let state = w.metadata.state();
        let unreadable = &state.endpoints[&dev_ep].unreadable;
        assert!(unreadable.contains("sqlrole"), "{unreadable:?}");
        assert_eq!(unreadable.len(), 1, "{unreadable:?}");
    }

    #[tokio::test]
    async fn child_create_runs_the_resync_before_ready() {
        let (w, journal) = worker();
        let s = with_login_roles(&w, &journal).await;

        w.handle_branch_create(s.dev, None).await.unwrap();

        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Ready, "{:?}", dev.error);
        let dev_ep = dev.endpoint_id.unwrap();
        let order = [
            pos(&journal, "metadata.insert_data_key"),
            pos(&journal, "metadata.set_compute_settings_version"),
            pos(&journal, "metadata.lock_endpoint"),
            pos(
                &journal,
                &format!("compute.sql {dev_ep} postgres SET search_path = pg_catalog, pg_temp;"),
            ),
            pos(&journal, "metadata.apply_resync"),
            pos(&journal, "metadata.set_branch_ready"),
        ];
        assert!(order.is_sorted(), "{order:?} {:?}", entries(&journal));
        let state = w.metadata.state();
        let endpoint = &state.endpoints[&dev_ep];
        assert_eq!(
            endpoint.compute_settings_version,
            crate::COMPUTE_SETTINGS_VERSION
        );
        // The child's catalog wins for its SQL roles: the MD5 role stays a row without a
        // verifier, and the databases come from the read.
        assert!(endpoint.logins.contains_key("md5role"));
    }

    #[tokio::test]
    async fn child_create_fails_with_syncing_prefix() {
        let (w, journal) = worker();
        let s = with_login_roles(&w, &journal).await;
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some((CATALOG_READ.into(), "connection reset".into()));

        w.handle_branch_create(s.dev, None).await.unwrap();

        let dev = w.metadata.branch(s.dev);
        assert_eq!(dev.status, BranchStatus::Failed);
        let error = dev.error.unwrap();
        assert!(error.starts_with("syncing login roles: "), "{error}");
        assert!(!has(&journal, "metadata.set_branch_ready"));
        assert_eq!(w.metadata.endpoint_ids(), std::slice::from_ref(&s.main_ep));
        assert_main_untouched(&w, &journal, &s).await;
    }
}
