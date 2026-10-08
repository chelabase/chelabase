//! The claim on a merge in a working status (`claimed_at`): taken when a job starts, refreshed
//! while it runs, so reconcile (T8b) only re-queues a merge whose worker went silent.

use std::future::Future;

use chelabase_metadata::{MergeRecord, MergeStatus};
use chelabase_neon::ComputeBackend;
use tracing::{info, warn};
use uuid::Uuid;

use crate::Worker;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// Claims a merge still in `status` (its claim refreshed) and reads it again, so the job
    /// works from what was saved before the claim (T5: `set_merge_status` checks no version).
    /// `None` when there's no such merge or it moved on (cancelled, or finished by an earlier
    /// delivery): the job has nothing to do. `Err` is a metadata failure.
    pub(crate) async fn claim_merge(
        &self,
        id: Uuid,
        status: MergeStatus,
    ) -> Result<Option<MergeRecord>, String> {
        let claimed = self
            .metadata
            .touch_merge_claim(id, status)
            .await
            .map_err(|e| format!("claiming merge {id}: {e}"))?;
        if !claimed {
            return Ok(None);
        }
        let row = self
            .metadata
            .merge(id)
            .await
            .map_err(|e| format!("reading merge {id}: {e}"))?;
        Ok(row.filter(|m| m.status == status))
    }

    /// Runs `work`, refreshing the merge's claim every `claim_refresh` while it's still in
    /// `status`, in the style of the branch jobs' `keep_claimed`.
    pub(crate) async fn keep_merge_claimed<T>(
        &self,
        id: Uuid,
        status: MergeStatus,
        work: impl Future<Output = T>,
    ) -> T {
        tokio::pin!(work);
        let mut refresh = tokio::time::interval(self.claim_refresh);
        refresh.tick().await;
        loop {
            tokio::select! {
                done = &mut work => return done,
                _ = refresh.tick() => {
                    if let Err(e) = self.metadata.touch_merge_claim(id, status).await {
                        warn!(merge_id = %id, "refreshing the merge's claim: {e}");
                    }
                }
            }
        }
    }
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// Like [`Self::keep_merge_claimed`], but stops: `None` (the work dropped, mid-statement
    /// if need be) as soon as a refresh finds the merge no longer in `status` (cancelled). A
    /// refresh that fails (metadata trouble) doesn't stop the work.
    pub(crate) async fn run_while_claimed<T>(
        &self,
        id: Uuid,
        status: MergeStatus,
        work: impl Future<Output = T>,
    ) -> Option<T> {
        tokio::pin!(work);
        let mut refresh = tokio::time::interval(self.claim_refresh);
        refresh.tick().await;
        loop {
            tokio::select! {
                done = &mut work => return Some(done),
                _ = refresh.tick() => match self.metadata.touch_merge_claim(id, status).await {
                    Ok(true) => {}
                    Ok(false) => {
                        info!(merge_id = %id, status = status.as_str(), "the merge moved on; stopping its work");
                        return None;
                    }
                    Err(e) => warn!(merge_id = %id, "refreshing the merge's claim: {e}"),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chelabase_metadata::{MergeDirection, MergeStatus};

    use crate::fakes::{entries, ready_database, worker};

    #[tokio::test]
    async fn claim_is_refreshed_while_the_work_runs() {
        let (w, journal) = worker();
        let db = ready_database(&w).await;
        let main = w.metadata.main(db).id;
        let id = w.metadata.insert_merge(
            db,
            main,
            main,
            MergeDirection::IntoParent,
            MergeStatus::Computing,
        );
        journal.lock().unwrap().clear();

        let out = w
            .keep_merge_claimed(id, MergeStatus::Computing, async {
                tokio::time::sleep(Duration::from_millis(40)).await;
                7
            })
            .await;

        assert_eq!(out, 7);
        let touches = entries(&journal)
            .iter()
            .filter(|e| e.starts_with("metadata.touch_merge_claim"))
            .count();
        assert!(touches >= 2, "{:?}", entries(&journal));
    }

    // A cancel (or anything moving the merge on) stops the work at the next refresh; work that
    // ends first returns its value.
    #[tokio::test]
    async fn a_lost_claim_stops_the_work() {
        let (w, _journal) = worker();
        let db = ready_database(&w).await;
        let main = w.metadata.main(db).id;
        let id = w.metadata.insert_merge(
            db,
            main,
            main,
            MergeDirection::IntoParent,
            MergeStatus::Rehearsing,
        );

        let quick = w
            .run_while_claimed(id, MergeStatus::Rehearsing, async { 7 })
            .await;
        assert_eq!(quick, Some(7));

        let cancel = async {
            tokio::time::sleep(Duration::from_millis(15)).await;
            w.metadata.state().merges.get_mut(&id).unwrap().status = MergeStatus::Cancelled;
        };
        let work = w.run_while_claimed(id, MergeStatus::Rehearsing, async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            7
        });
        let (stopped, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(work, cancel)
        })
        .await
        .expect("the work is stopped, not waited for");
        assert_eq!(stopped, None);
    }

    // The claim transition: a merge still in its working status is claimed and read again (T5:
    // its inputs are read after the claim); one that moved on is left alone.
    #[tokio::test]
    async fn claim_rereads_the_row_and_refuses_one_that_moved() {
        let (w, _journal) = worker();
        let db = ready_database(&w).await;
        let main = w.metadata.main(db).id;
        let id = w.metadata.insert_merge(
            db,
            main,
            main,
            MergeDirection::IntoParent,
            MergeStatus::Computing,
        );
        w.metadata.state().merges.get_mut(&id).unwrap().claimed_at = None;

        let claimed = w.claim_merge(id, MergeStatus::Computing).await.unwrap();
        let row = claimed.expect("claimed");
        assert_eq!(row.status, MergeStatus::Computing);
        assert!(row.claimed_at.is_some());

        w.metadata.state().merges.get_mut(&id).unwrap().status = MergeStatus::Cancelled;
        assert_eq!(
            w.claim_merge(id, MergeStatus::Computing).await.unwrap(),
            None
        );
        assert_eq!(
            w.claim_merge(uuid::Uuid::new_v4(), MergeStatus::Computing)
                .await
                .unwrap(),
            None
        );
    }
}
