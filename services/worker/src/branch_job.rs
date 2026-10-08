//! Dispatch of a `BranchJob` (the branch consumer's payload) to the handlers.

use chelabase_nats::BranchJob;
use chelabase_neon::{ComputeBackend, StorconError};

use crate::Worker;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};

/// How many deliveries of a branch job a transient storage-controller failure (a 5xx, no
/// answer, or a timeout) is retried over before the branch fails with it. Spaced by
/// [`chelabase_nats::BRANCH_REDELIVERY_DELAY`], they span about 2 minutes: long enough for a
/// storage-controller restart. A refusal (4xx) fails at once.
pub const BRANCH_JOB_ATTEMPTS: u64 = 5;

/// Which delivery of a branch job this is (1 for the first).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attempt(pub u64);

impl Attempt {
    /// No retry left: every failure is recorded on the branch.
    pub const LAST: Attempt = Attempt(BRANCH_JOB_ATTEMPTS);

    pub fn retries_left(self) -> bool {
        self.0 < BRANCH_JOB_ATTEMPTS
    }
}

/// A failed step: what the branch records (the step's prefix and the full cause chain), and
/// whether a later delivery may succeed.
#[derive(Debug)]
pub(crate) struct StepFailure {
    pub message: String,
    pub transient: bool,
}

impl StepFailure {
    pub fn permanent(message: String) -> Self {
        Self {
            message,
            transient: false,
        }
    }

    /// A storage-controller failure under `prefix`: transient unless the controller refused
    /// the request (4xx).
    ///
    /// A branch create whose start LSN the pageserver hasn't ingested yet doesn't come back
    /// as a 406: the pageserver waits up to `WaitLsnTimeout::Default` (300 s), longer than the
    /// worker's `CREATE_TIMELINE_TIMEOUT` (120 s), so a catch-up timeout surfaces here as a
    /// client-side timeout (`Other`, transient: tear down and retry). A 406 comes only from
    /// a real refusal (start LSN below the ancestor's, below the GC cutoff, an archived
    /// ancestor), which is permanent.
    pub fn storage(prefix: &str, e: &StorconError) -> Self {
        Self {
            message: format!("{prefix}: {e:#}"),
            transient: matches!(e, StorconError::Other(_)),
        }
    }

    /// Whether delivery `attempt` should hand the job back for another delivery.
    pub fn retry(&self, attempt: Attempt) -> bool {
        self.transient && attempt.retries_left()
    }
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// Runs one branch job as its last attempt. `Err` means the job is retried.
    pub async fn handle_branch_job(&self, job: BranchJob) -> Result<(), String> {
        self.handle_branch_job_attempt(job, Attempt::LAST).await
    }

    /// Runs delivery `attempt` of a branch job. `Err` means the job is retried: metadata
    /// trouble, or a transient storage-controller failure with retries left.
    pub async fn handle_branch_job_attempt(
        &self,
        job: BranchJob,
        attempt: Attempt,
    ) -> Result<(), String> {
        match job {
            BranchJob::Create { branch_id, at } => {
                self.branch_create_attempt(branch_id, at, attempt).await
            }
            BranchJob::ResetToParent { branch_id } => {
                self.branch_reset_attempt(branch_id, attempt).await
            }
            BranchJob::Restore {
                branch_id,
                backup_id,
                backup_name,
                at,
            } => {
                self.branch_restore_attempt(branch_id, backup_id, &backup_name, at, attempt)
                    .await
            }
            BranchJob::Delete { branch_id } => self.handle_branch_delete(branch_id).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use chelabase_metadata::BranchStatus;
    use chelabase_nats::PointInTime;
    use chelabase_types::BranchId;

    use super::*;
    use std::time::Duration;

    use crate::fakes::{
        FakeDetach, FakeWorker, UNAVAILABLE_CAUSE, entries, pending_branch, position, ready_branch,
        ready_database, worker,
    };

    // --- Transient storage-controller failures (M3) ---------------------------------------

    #[test]
    fn transient_failures_are_retried_over_about_two_minutes() {
        // A storage-controller restart can take a minute or two; the deliveries (spaced by the
        // branch consumer's nak delay) must outlast it.
        let gaps = u32::try_from(BRANCH_JOB_ATTEMPTS - 1).unwrap();
        let window = chelabase_nats::BRANCH_REDELIVERY_DELAY * gaps;
        assert!(window >= Duration::from_secs(120), "{window:?}");
    }

    fn at_time() -> Option<PointInTime> {
        Some(PointInTime::Timestamp(
            "2026-09-30T11:00:00Z".parse().unwrap(),
        ))
    }

    fn unavailable(w: &FakeWorker, method: &'static str, times: u32) {
        w.storage.unavailable.lock().unwrap().insert(method, times);
    }

    /// A ready database with a pending branch `dev`; returns (dev, main's endpoint).
    async fn pending_dev(w: &FakeWorker) -> (BranchId, String) {
        let db = ready_database(w).await;
        let main = w.metadata.main(db);
        (
            pending_branch(w, db, "dev", main.id),
            main.endpoint_id.unwrap(),
        )
    }

    /// A ready database with a ready branch `dev`, claimed for a reset or restore as the API
    /// does; returns (dev, its timeline).
    async fn claimed_dev(w: &FakeWorker) -> (BranchId, String) {
        let db = ready_database(w).await;
        let main = w.metadata.main(db).id;
        let dev = ready_branch(w, db, "dev", main).await;
        w.metadata
            .set_branch_status(dev, &[BranchStatus::Ready], BranchStatus::Resetting)
            .await
            .unwrap();
        (dev, w.metadata.branch(dev).timeline_id.unwrap())
    }

    fn create(id: BranchId) -> BranchJob {
        BranchJob::Create {
            branch_id: id,
            at: at_time(),
        }
    }

    fn restore(id: BranchId) -> BranchJob {
        BranchJob::Restore {
            branch_id: id,
            backup_id: BranchId::new(),
            backup_name: "dev-before-restore-20260930T120000Z".into(),
            at: PointInTime::Timestamp("2026-09-30T11:00:00Z".parse().unwrap()),
        }
    }

    #[tokio::test]
    async fn a_create_retries_a_503_and_then_succeeds() {
        for method in ["lsn_by_timestamp", "create_branch_timeline"] {
            let (w, _journal) = worker();
            let (dev, main_ep) = pending_dev(&w).await;
            unavailable(&w, method, 1);

            let first = w.handle_branch_job_attempt(create(dev), Attempt(1)).await;

            let err = first.expect_err(method);
            assert!(err.contains(UNAVAILABLE_CAUSE), "{method}: {err}");
            let b = w.metadata.branch(dev);
            assert_eq!(b.status, BranchStatus::Provisioning, "{method}");
            assert_eq!(b.error, None, "{method}");
            // Nothing half-made waits for the retry.
            assert_eq!(w.metadata.endpoint_ids(), vec![main_ep.clone()], "{method}");
            assert_eq!(w.storage.timeline_ids().len(), 1, "{method}: only main's");

            w.handle_branch_job_attempt(create(dev), Attempt(2))
                .await
                .unwrap();
            let b = w.metadata.branch(dev);
            assert_eq!(b.status, BranchStatus::Ready, "{method}: {:?}", b.error);
        }
    }

    #[tokio::test]
    async fn a_create_fails_with_the_cause_on_its_last_attempt() {
        let (w, _journal) = worker();
        let (dev, _) = pending_dev(&w).await;
        unavailable(&w, "create_branch_timeline", u32::MAX);

        for attempt in 1..BRANCH_JOB_ATTEMPTS {
            let err = w
                .handle_branch_job_attempt(create(dev), Attempt(attempt))
                .await
                .unwrap_err();
            assert!(err.starts_with("creating timeline: "), "{err}");
        }
        w.handle_branch_job_attempt(create(dev), Attempt(BRANCH_JOB_ATTEMPTS))
            .await
            .unwrap();

        let b = w.metadata.branch(dev);
        assert_eq!(b.status, BranchStatus::Failed);
        assert_eq!(
            b.error.as_deref(),
            Some(
                "creating timeline: storage controller returned 503 Service Unavailable: shard \
                 is reconfiguring"
            )
        );
    }

    #[tokio::test]
    async fn a_create_refused_with_a_4xx_fails_at_once() {
        for method in ["lsn_by_timestamp", "create_branch_timeline"] {
            let (w, _journal) = worker();
            let (dev, _) = pending_dev(&w).await;
            *w.storage.reject.lock().unwrap() = Some(method);

            w.handle_branch_job_attempt(create(dev), Attempt(1))
                .await
                .unwrap();

            let b = w.metadata.branch(dev);
            assert_eq!(b.status, BranchStatus::Failed, "{method}");
            let error = b.error.unwrap_or_default();
            assert!(
                error.contains("rejected the request (400"),
                "{method}: {error}"
            );
        }
    }

    #[tokio::test]
    async fn a_reset_or_restore_retries_a_503_before_its_switch() {
        type Job = fn(BranchId) -> BranchJob;
        let reset: Job = |id| BranchJob::ResetToParent { branch_id: id };
        let cases: [(&str, Job, &'static str); 3] = [
            ("reset", reset, "create_branch_timeline"),
            ("restore", restore, "lsn_by_timestamp"),
            ("restore", restore, "create_branch_timeline"),
        ];
        for (flow, job, method) in cases {
            let ctx = format!("{flow} {method}");
            let (w, _journal) = worker();
            let (dev, old_timeline) = claimed_dev(&w).await;
            let timelines = w.storage.timeline_ids();
            unavailable(&w, method, 1);

            let err = w
                .handle_branch_job_attempt(job(dev), Attempt(1))
                .await
                .expect_err(&ctx);
            assert!(err.contains(UNAVAILABLE_CAUSE), "{ctx}: {err}");
            let b = w.metadata.branch(dev);
            // Still claimed for the retry, on its old timeline, with nothing pending.
            assert_eq!(b.status, BranchStatus::Resetting, "{ctx}");
            assert_eq!(
                b.timeline_id.as_deref(),
                Some(old_timeline.as_str()),
                "{ctx}"
            );
            assert_eq!(b.pending_timeline_id, None, "{ctx}");
            assert_eq!(w.storage.timeline_ids(), timelines, "{ctx}");

            w.handle_branch_job_attempt(job(dev), Attempt(2))
                .await
                .unwrap();
            let b = w.metadata.branch(dev);
            assert_eq!(b.status, BranchStatus::Ready, "{ctx}: {:?}", b.error);
            assert_eq!(b.error, None, "{ctx}");
            assert_ne!(
                b.timeline_id.as_deref(),
                Some(old_timeline.as_str()),
                "{ctx}"
            );
        }
    }

    // --- A detach that outlasts its timeout (M5) -------------------------------------------

    /// The pending timeline a restore of `dev` recorded.
    fn pending_of(w: &FakeWorker, dev: BranchId) -> String {
        w.metadata
            .branch(dev)
            .pending_timeline_id
            .expect("a pending timeline")
    }

    fn tenant_of(w: &FakeWorker, dev: BranchId) -> String {
        let db = w.metadata.branch(dev).database_id;
        w.metadata.database(db).neon_tenant_id.unwrap()
    }

    #[tokio::test]
    async fn a_detach_timeout_is_retried_and_the_retry_completes_the_restore() {
        // The retry finds the detach finished meanwhile, or still attached (and detaches it).
        for finished_meanwhile in [true, false] {
            let ctx = format!("finished meanwhile: {finished_meanwhile}");
            let (w, journal) = worker();
            let (dev, old_timeline) = claimed_dev(&w).await;
            *w.storage.detach.lock().unwrap() = FakeDetach::TimesOut;

            let err = w
                .handle_branch_job_attempt(restore(dev), Attempt(1))
                .await
                .expect_err(&ctx);
            assert!(err.starts_with("detaching: "), "{ctx}: {err}");
            // Nothing undone: the detach may still be running.
            let b = w.metadata.branch(dev);
            assert_eq!(b.status, BranchStatus::Resetting, "{ctx}");
            assert_eq!(
                b.timeline_id.as_deref(),
                Some(old_timeline.as_str()),
                "{ctx}"
            );
            let pending = pending_of(&w, dev);
            let tenant = tenant_of(&w, dev);
            assert!(w.storage.timeline(&tenant, &pending).is_some(), "{ctx}");
            assert!(
                position(&journal, "storage.delete_timeline").is_none(),
                "{ctx}"
            );

            *w.storage.detach.lock().unwrap() = FakeDetach::Works;
            if finished_meanwhile {
                let key = (tenant.parse().unwrap(), pending.parse().unwrap());
                w.storage
                    .timelines
                    .lock()
                    .unwrap()
                    .get_mut(&key)
                    .unwrap()
                    .ancestor = None;
            }
            journal.lock().unwrap().clear();
            w.handle_branch_job_attempt(restore(dev), Attempt(2))
                .await
                .unwrap();

            let b = w.metadata.branch(dev);
            assert_eq!(b.status, BranchStatus::Ready, "{ctx}: {:?}", b.error);
            let restored = b.timeline_id.unwrap();
            assert_ne!(restored, old_timeline, "{ctx}");
            assert_eq!(
                w.storage.timeline(&tenant, &restored).unwrap().ancestor,
                None
            );
            let ctx = format!("{ctx}: {:?}", entries(&journal));
            if finished_meanwhile {
                // Detached, so nothing runs on it: dropped, and the restore starts afresh.
                assert!(w.storage.timeline(&tenant, &pending).is_none(), "{ctx}");
                assert!(
                    position(&journal, "storage.create_branch_timeline").is_some(),
                    "{ctx}"
                );
            } else {
                // Still attached: detached again (never deleted) and used.
                assert_eq!(restored, pending, "{ctx}");
                assert!(
                    position(&journal, "storage.create_branch_timeline").is_none(),
                    "{ctx}"
                );
                assert!(
                    position(&journal, "storage.delete_timeline").is_none(),
                    "{ctx}"
                );
                assert!(
                    position(&journal, "storage.detach_ancestor").is_some(),
                    "{ctx}"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_detach_that_times_out_after_finishing_completes_at_once() {
        let (w, _journal) = worker();
        let (dev, _) = claimed_dev(&w).await;
        *w.storage.detach.lock().unwrap() = FakeDetach::TimesOutAfterFinishing;

        w.handle_branch_job_attempt(restore(dev), Attempt(1))
            .await
            .unwrap();

        let b = w.metadata.branch(dev);
        assert_eq!(b.status, BranchStatus::Ready, "{:?}", b.error);
        assert_eq!(b.error, None);
    }

    #[tokio::test]
    async fn a_detach_in_an_unknown_state_is_never_undone() {
        // Not even on the last attempt: deleting the timeline could race the detach.
        let (w, journal) = worker();
        let (dev, _) = claimed_dev(&w).await;
        *w.storage.detach.lock().unwrap() = FakeDetach::TimesOut;
        unavailable(&w, "ancestor_lsn", u32::MAX);

        let err = w
            .handle_branch_job_attempt(restore(dev), Attempt::LAST)
            .await
            .unwrap_err();

        assert!(err.starts_with("detaching: "), "{err}");
        let b = w.metadata.branch(dev);
        assert_eq!(b.status, BranchStatus::Resetting);
        assert!(b.pending_timeline_id.is_some());
        assert!(position(&journal, "storage.delete_timeline").is_none());

        // The redelivery can't read it either: again no undo.
        w.handle_branch_job_attempt(restore(dev), Attempt(BRANCH_JOB_ATTEMPTS + 1))
            .await
            .unwrap_err();
        assert!(position(&journal, "storage.delete_timeline").is_none());
        assert!(w.metadata.branch(dev).pending_timeline_id.is_some());
    }

    #[tokio::test]
    async fn a_refused_detach_is_undone_at_once() {
        let (w, _journal) = worker();
        let (dev, old_timeline) = claimed_dev(&w).await;
        let timelines = w.storage.timeline_ids();
        *w.storage.reject.lock().unwrap() = Some("detach_ancestor");

        w.handle_branch_job_attempt(restore(dev), Attempt(1))
            .await
            .unwrap();

        let b = w.metadata.branch(dev);
        assert_eq!(b.status, BranchStatus::Ready);
        assert_eq!(b.timeline_id.as_deref(), Some(old_timeline.as_str()));
        assert_eq!(b.pending_timeline_id, None);
        let error = b.error.unwrap_or_default();
        assert!(
            error.starts_with("detaching: ") && error.contains("(400"),
            "{error}"
        );
        assert_eq!(w.storage.timeline_ids(), timelines);
    }

    #[tokio::test]
    async fn a_long_detach_keeps_the_claim_fresh() {
        let (w, journal) = worker();
        let (dev, _) = claimed_dev(&w).await;
        *w.storage.detach_delay.lock().unwrap() = Duration::from_millis(40);
        journal.lock().unwrap().clear();

        w.handle_branch_job_attempt(restore(dev), Attempt(1))
            .await
            .unwrap();

        let all = entries(&journal);
        let detach = position(&journal, "storage.detach_ancestor").unwrap();
        let stop = position(&journal, "compute.delete").unwrap();
        let refreshes = all[detach..stop]
            .iter()
            .filter(|e| e.starts_with("metadata.set_branch_status"))
            .count();
        assert!(refreshes >= 2, "{all:?}");
        assert_eq!(w.metadata.branch(dev).status, BranchStatus::Ready);
    }

    /// A grandchild (main → dev → feat) restores with one detach of the restored timeline,
    /// which copies every ancestor level. While an ancestor's WAL lags, the detach answers 503
    /// (the pageserver waits about 30 s per level): the pending timeline is kept, and the
    /// redelivery detaches that same timeline and finishes. The old timelines are never
    /// detached.
    #[tokio::test]
    async fn a_grandchild_restore_takes_one_detach_and_retries_a_503() {
        let (w, journal) = worker();
        let db = ready_database(&w).await;
        let main = w.metadata.main(db).id;
        let dev = ready_branch(&w, db, "dev", main).await;
        let feat = ready_branch(&w, db, "feat", dev).await;
        w.metadata
            .set_branch_status(feat, &[BranchStatus::Ready], BranchStatus::Resetting)
            .await
            .unwrap();
        let old_timeline = w.metadata.branch(feat).timeline_id.unwrap();
        let tenant = tenant_of(&w, feat);
        let dev_timeline: chelabase_neon::NeonId =
            w.metadata.branch(dev).timeline_id.unwrap().parse().unwrap();
        let old_ancestor = w.storage.timeline(&tenant, &old_timeline).unwrap().ancestor;
        let (parent, _) = old_ancestor.clone().expect("feat is dev's child");
        assert_eq!(parent, dev_timeline);
        unavailable(&w, "detach_ancestor", 1);
        journal.lock().unwrap().clear();

        let err = w
            .handle_branch_job_attempt(restore(feat), Attempt(1))
            .await
            .unwrap_err();
        assert!(
            err.starts_with("detaching: ") && err.contains(UNAVAILABLE_CAUSE),
            "{err}"
        );
        let b = w.metadata.branch(feat);
        assert_eq!(b.status, BranchStatus::Resetting);
        assert_eq!(b.timeline_id.as_deref(), Some(old_timeline.as_str()));
        let pending = pending_of(&w, feat);
        assert!(position(&journal, "storage.delete_timeline").is_none());

        w.handle_branch_job_attempt(restore(feat), Attempt(2))
            .await
            .unwrap();

        let b = w.metadata.branch(feat);
        assert_eq!(b.status, BranchStatus::Ready, "{:?}", b.error);
        assert_eq!(b.timeline_id.as_deref(), Some(pending.as_str()));
        assert_eq!(
            w.storage.timeline(&tenant, &pending).unwrap().ancestor,
            None
        );
        let detaches: Vec<String> = entries(&journal)
            .into_iter()
            .filter(|e| e.starts_with("storage.detach_ancestor"))
            .collect();
        let expected = format!("storage.detach_ancestor {pending}");
        assert_eq!(
            detaches,
            [expected.clone(), expected],
            "only the restored one"
        );
        // The old timeline (now the backup's) still hangs off dev's, at the same point.
        assert_eq!(
            w.storage.timeline(&tenant, &old_timeline).unwrap().ancestor,
            old_ancestor
        );
        assert!(position(&journal, "storage.delete_timeline").is_none());
    }

    #[tokio::test]
    async fn a_reset_gives_up_on_its_last_attempt_and_a_4xx_at_once() {
        let reset = |id| BranchJob::ResetToParent { branch_id: id };
        // A persistent 503: retried, then back to ready with the cause.
        let (w, _journal) = worker();
        let (dev, old_timeline) = claimed_dev(&w).await;
        unavailable(&w, "create_branch_timeline", u32::MAX);
        for attempt in 1..BRANCH_JOB_ATTEMPTS {
            w.handle_branch_job_attempt(reset(dev), Attempt(attempt))
                .await
                .unwrap_err();
        }
        w.handle_branch_job_attempt(reset(dev), Attempt(BRANCH_JOB_ATTEMPTS))
            .await
            .unwrap();
        let b = w.metadata.branch(dev);
        assert_eq!(b.status, BranchStatus::Ready);
        assert_eq!(b.timeline_id.as_deref(), Some(old_timeline.as_str()));
        let error = b.error.unwrap_or_default();
        assert!(
            error.starts_with("creating timeline: ") && error.contains(UNAVAILABLE_CAUSE),
            "{error}"
        );

        // A 4xx: back to ready on the first attempt.
        let (w, _journal) = worker();
        let (dev, _) = claimed_dev(&w).await;
        *w.storage.reject.lock().unwrap() = Some("create_branch_timeline");
        w.handle_branch_job_attempt(reset(dev), Attempt(1))
            .await
            .unwrap();
        let b = w.metadata.branch(dev);
        assert_eq!(b.status, BranchStatus::Ready);
        assert!(
            b.error
                .unwrap_or_default()
                .contains("rejected the request (400")
        );
    }

    #[tokio::test]
    async fn create_and_delete_jobs_reach_their_handlers() {
        let (w, _journal) = worker();
        let db = ready_database(&w).await;
        let main = w.metadata.main(db).id;
        let id = pending_branch(&w, db, "dev", main);

        w.handle_branch_job(BranchJob::Create {
            branch_id: id,
            at: None,
        })
        .await
        .unwrap();
        assert_eq!(w.metadata.branch(id).status, BranchStatus::Ready);

        // The API begins the delete before publishing the job.
        w.metadata.begin_branch_delete(id, None).await.unwrap();
        w.handle_branch_job(BranchJob::Delete { branch_id: id })
            .await
            .unwrap();
        assert_eq!(w.metadata.branch(id).status, BranchStatus::Deleted);
    }

    #[tokio::test]
    async fn reset_job_reaches_its_handler() {
        let (w, _journal) = worker();
        let db = ready_database(&w).await;
        let main = w.metadata.main(db).id;
        let id = ready_branch(&w, db, "dev", main).await;
        let before = w.metadata.branch(id).timeline_id;
        w.metadata
            .set_branch_status(id, &[BranchStatus::Ready], BranchStatus::Resetting)
            .await
            .unwrap();

        w.handle_branch_job(BranchJob::ResetToParent { branch_id: id })
            .await
            .unwrap();
        let after = w.metadata.branch(id);
        assert_eq!(after.status, BranchStatus::Ready, "{:?}", after.error);
        assert_ne!(after.timeline_id, before);
    }
}
