//! Creating a branch: an endpoint row, a timeline branched from the parent's, a compute, and
//! the data API roles and bootstrap inside it.

use cheladb_metadata::{BranchKind, BranchRecord, BranchStatus, NewEndpoint};
use cheladb_nats::PointInTime;
use cheladb_neon::{ComputeBackend, Lsn, NeonId};
use cheladb_secrets::{Purpose, seal_credential};
use cheladb_types::{BranchId, DatabaseId};
use tracing::{info, warn};

use crate::Worker;
use crate::branch_job::{Attempt, StepFailure};
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::provision::{bootstrap_sql, new_compute_token, new_endpoint_id, sha256_hex};
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
}

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
                self.finish_branch(branch_id, &rec, &parent_timeline, lsn)
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
    /// branch (with its new timeline id), in one transaction.
    async fn record_branch_endpoint(
        &self,
        branch: &BranchRecord,
        tenant: NeonId,
    ) -> Result<Recorded, String> {
        let database_name = self.main_database_name(branch.database_id).await?;
        let rec = Recorded {
            branch: branch.id,
            endpoint_id: new_endpoint_id(),
            tenant,
            timeline: NeonId::new(),
            compute_token: new_compute_token(),
            database_name,
            password: credentials::generate_password(),
        };
        let verifier = cheladb_neon::scram::verifier(&rec.password);
        let token_hash = sha256_hex(&rec.compute_token);
        let endpoint = NewEndpoint {
            id: &rec.endpoint_id,
            tenant_id: rec.tenant.as_str(),
            timeline_id: rec.timeline.as_str(),
            compute_token_sha256: &token_hash,
            owner_role: OWNER_ROLE,
            owner_verifier: &verifier,
            database_name: &rec.database_name,
        };
        match self
            .metadata
            .create_endpoint_for_branch(branch.id, rec.timeline.as_str(), &endpoint)
            .await
        {
            Ok(true) => Ok(rec),
            // Already linked by an interrupted attempt (or a delete began, and then the failure
            // writes nothing). Reconcile tears down the endpoint of a failed branch.
            Ok(false) => Err(
                "recording the endpoint: a previous attempt was interrupted; its resources \
                 will be cleaned up"
                    .to_string(),
            ),
            Err(err) => Err(format!("recording the endpoint: {err}")),
        }
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
        self.storage
            .create_branch_timeline(&rec.tenant, &rec.timeline, parent_timeline, lsn)
            .await
            .map_err(|e| StepFailure::storage("creating timeline", &e))?;
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
        self.computes
            .create(ep, &rec.compute_token)
            .await
            .map_err(|e| format!("starting compute: {e:#}"))?;
        self.wait_until_ready(ep, &rec.database_name).await?;
        // Only matters when the branch point predates the roles.
        self.computes
            .admin_sql(ep, &rec.database_name, &bootstrap_sql(self.owner_superuser))
            .await
            .map_err(|e| format!("bootstrapping roles: {e:#}"))?;
        self.bootstrap_data_api_on(rec.branch, ep, &rec.database_name)
            .await
            .map_err(|e| format!("bootstrapping the data API: {e:#}"))?;
        Ok(Built::Compute)
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
        info!(branch_id = %id, endpoint_id = %rec.endpoint_id, %lsn, "branch ready");
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
}
