//! RT4-3: the storage-token refresh. Every `CHELABASE_STORAGE_TOKEN_REFRESH_SECS` each running
//! compute that advertises `storage_token_refresh` gets a new expiring token through
//! `POST /storage_token` (never a spec push), under the endpoint's lock.

use std::sync::Arc;

use anyhow::{Context, anyhow};
use chelabase_neon::reconfigure::ComputeCtlStatus;
use chelabase_neon::{ComputeBackend, NeonId};
use futures_util::FutureExt;
use tracing::{debug, error, warn};

use crate::Worker;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::reset::DOCKER_TIMEOUT;

/// What one refresh pass did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RefreshPass {
    pub refreshed: usize,
    pub failed: usize,
    /// Not running, no `storage_token_refresh` capability, or no endpoint row.
    pub skipped: usize,
}

enum Outcome {
    Refreshed,
    Skipped,
}

/// Seconds since the epoch by the system clock (0 for a clock before 1970, which the loop
/// refuses before a pass).
fn system_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// [`Worker::refresh_storage_tokens_with`] with a clock that always answers `now` (seconds
    /// since the epoch).
    pub async fn refresh_storage_tokens_once(&self, now: u64) -> RefreshPass {
        self.refresh_storage_tokens_with(move || now).await
    }

    /// One pass over every compute container: stopped ones, ones compute_ctl doesn't call
    /// `running` or `configuration_pending`, ones that don't advertise the capability, ones
    /// without an endpoint row and ones whose lock another job keeps past
    /// [`chelabase_metadata::ENDPOINT_LOCK_TIMEOUT`] are skipped; a failure is counted per
    /// endpoint (reset by a success), logged, and retried at the next pass. `clock` (seconds
    /// since the epoch) is read for each endpoint once its lock is held, just before minting.
    /// After a successful listing, the counts of endpoints no longer listed are dropped.
    pub async fn refresh_storage_tokens_with(
        &self,
        clock: impl Fn() -> u64 + Send + Sync,
    ) -> RefreshPass {
        let mut pass = RefreshPass::default();
        let endpoints = match tokio::time::timeout(DOCKER_TIMEOUT, self.computes.list()).await {
            Ok(Ok(endpoints)) => endpoints,
            Ok(Err(e)) => {
                warn!("storage token refresh: listing the computes: {e:#}");
                pass.failed += 1;
                return pass;
            }
            Err(_) => {
                warn!("storage token refresh: Docker didn't list the computes in time");
                pass.failed += 1;
                return pass;
            }
        };
        {
            let listed: std::collections::HashSet<&str> =
                endpoints.iter().map(String::as_str).collect();
            self.failures().retain(|ep, _| listed.contains(ep.as_str()));
        }
        for ep in endpoints {
            match self.refresh_storage_token(&ep, &clock).await {
                Ok(Outcome::Refreshed) => {
                    pass.refreshed += 1;
                    self.failures().remove(&ep);
                }
                Ok(Outcome::Skipped) => pass.skipped += 1,
                Err(e) => {
                    pass.failed += 1;
                    let count = {
                        let mut failures = self.failures();
                        let count = failures.entry(ep.clone()).or_insert(0);
                        *count = count.saturating_add(1);
                        *count
                    };
                    warn!(endpoint_id = %ep, failures = count, "storage token refresh: {e:#}");
                }
            }
        }
        pass
    }

    /// Consecutive failed refreshes of `endpoint_id` (0 after a success or for a compute never
    /// refreshed).
    pub fn storage_token_failure_count(&self, endpoint_id: &str) -> u32 {
        self.failures().get(endpoint_id).copied().unwrap_or(0)
    }

    fn failures(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, u32>> {
        self.storage_token_failures
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    async fn refresh_storage_token(
        &self,
        ep: &str,
        clock: &(impl Fn() -> u64 + Send + Sync),
    ) -> anyhow::Result<Outcome> {
        let _lock = match self.metadata.lock_endpoint(ep).await {
            Ok(lock) => lock,
            // Another job holds the endpoint past the bounded wait, or every lock connection is
            // taken past the pool's wait (both bounded by `ENDPOINT_LOCK_TIMEOUT`): not a
            // failure, the next pass tries again.
            Err(e) if e.is_lock_wait_timeout() => {
                debug!(endpoint_id = %ep, "storage token refresh: the endpoint is busy");
                return Ok(Outcome::Skipped);
            }
            Err(e) => return Err(anyhow::Error::new(e).context("locking the endpoint")),
        };
        let Some(control_addr) = self.control_addr_bounded(ep).await? else {
            debug!(endpoint_id = %ep, "storage token refresh: not running");
            return Ok(Outcome::Skipped);
        };
        let Some(endpoint) = self
            .metadata
            .neon_endpoint(ep)
            .await
            .context("loading the endpoint")?
        else {
            debug!(endpoint_id = %ep, "storage token refresh: no endpoint row");
            return Ok(Outcome::Skipped);
        };
        let report = self.report_bounded(control_addr, ep).await?;
        if !report.storage_token_refresh {
            return Ok(Outcome::Skipped);
        }
        // compute_ctl takes `POST /storage_token` only in these two states (SP7): any other
        // (starting, terminating, failed) is skipped, not a counted refusal.
        let takes_tokens = match &report.status {
            ComputeCtlStatus::Running => true,
            ComputeCtlStatus::Other(s) => s == "configuration_pending",
            ComputeCtlStatus::Failed(_) => false,
        };
        if !takes_tokens {
            debug!(endpoint_id = %ep, status = ?report.status, "storage token refresh: not taking tokens");
            return Ok(Outcome::Skipped);
        }
        let tenant: NeonId = endpoint.tenant_id.parse().context("stored tenant id")?;
        let now = clock();
        let token = self
            .pusher
            .signer()
            .tenant_token(&tenant, now, true)
            .context("signing the storage token")?;
        tokio::time::timeout(
            DOCKER_TIMEOUT,
            self.pusher.push_storage_token(control_addr, ep, &token),
        )
        .await
        .map_err(|_| anyhow!("compute_ctl didn't take the storage token in time"))?
        .context("pushing the storage token")?;
        Ok(Outcome::Refreshed)
    }
}

/// A pass at start, then one every `storage_token_refresh_every`, each with the system clock;
/// neither a failing pass nor a panicking one ends the loop.
pub async fn storage_token_refresh_loop<M, S, C, N, P>(worker: Arc<Worker<M, S, C, N, P>>)
where
    M: WorkerMetadata,
    S: Storage,
    C: ComputeBackend,
    N: CacheNotifier,
    P: SpecPusher,
{
    let mut ticks = tokio::time::interval(worker.storage_token_refresh_every);
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        if let Err(e) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            warn!("storage token refresh: the system clock is before 1970: {e}");
            continue;
        }
        // A bug anywhere in a pass must not stop every later refresh: the panic unwinds out of
        // this pass only (its endpoint lock is dropped on the way), and the next tick runs.
        let pass = std::panic::AssertUnwindSafe(worker.refresh_storage_tokens_with(system_now))
            .catch_unwind()
            .await;
        let pass = match pass {
            Ok(pass) => pass,
            Err(panic) => {
                let message = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "a non-text panic".into());
                error!("storage token refresh: a pass panicked ({message}); the next pass retries");
                continue;
            }
        };
        if pass.failed > 0 {
            warn!(
                refreshed = pass.refreshed,
                failed = pass.failed,
                "storage token refresh: some computes failed; the next pass retries"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::fakes::worker;

    #[tokio::test]
    async fn an_orphan_without_a_row_is_skipped_not_failed() {
        let (w, _journal) = worker();
        w.computes.insert("ep-ffffffffffff");
        w.pusher.set_capability("ep-ffffffffffff", true);
        let pass = w.refresh_storage_tokens_once(1_800_000_000).await;
        assert_eq!((pass.refreshed, pass.failed, pass.skipped), (0, 0, 1));
        assert_eq!(w.storage_token_failure_count("ep-ffffffffffff"), 0);
    }

    #[tokio::test]
    async fn a_failure_count_only_grows_while_failures_are_consecutive() {
        let (w, _journal) = worker();
        let ep = crate::c2_tests::ready_named(&w, "app").await;
        w.pusher.set_capability(&ep, true);
        w.pusher.fail_storage_token_for(&ep, Some("boom".into()));
        w.refresh_storage_tokens_once(1_800_000_000).await;
        w.refresh_storage_tokens_once(1_800_000_000).await;
        assert_eq!(w.storage_token_failure_count(&ep), 2);
        w.pusher.fail_storage_token_for(&ep, None);
        w.refresh_storage_tokens_once(1_800_000_000).await;
        w.pusher.fail_storage_token_for(&ep, Some("boom".into()));
        w.refresh_storage_tokens_once(1_800_000_000).await;
        assert_eq!(w.storage_token_failure_count(&ep), 1, "restarted from 0");
    }

    // F2 edge: a busy endpoint's earlier failures are neither reset nor grown by a skip.
    #[tokio::test(start_paused = true)]
    async fn a_busy_endpoint_keeps_its_failure_count() {
        use crate::ports::WorkerMetadata;
        let (w, _journal) = worker();
        let ep = crate::c2_tests::ready_named(&w, "app").await;
        w.pusher.set_capability(&ep, true);
        w.pusher.fail_storage_token_for(&ep, Some("boom".into()));
        w.refresh_storage_tokens_once(1_800_000_000).await;
        assert_eq!(w.storage_token_failure_count(&ep), 1);
        let held = w.metadata.lock_endpoint(&ep).await.unwrap();
        let pass = w.refresh_storage_tokens_once(1_800_000_000).await;
        assert_eq!((pass.refreshed, pass.failed, pass.skipped), (0, 0, 1));
        assert_eq!(w.storage_token_failure_count(&ep), 1);
        drop(held);
    }

    // F2 edge: a compute that left the list and came back starts its count from 0.
    #[tokio::test]
    async fn a_compute_back_in_the_list_starts_from_zero() {
        let (w, _journal) = worker();
        let ep = crate::c2_tests::ready_named(&w, "app").await;
        w.pusher.set_capability(&ep, true);
        w.pusher.fail_storage_token_for(&ep, Some("boom".into()));
        w.refresh_storage_tokens_once(1_800_000_000).await;
        w.refresh_storage_tokens_once(1_800_000_000).await;
        assert_eq!(w.storage_token_failure_count(&ep), 2);
        w.computes.remove(&ep);
        w.refresh_storage_tokens_once(1_800_000_000).await;
        w.computes.insert(&ep);
        w.refresh_storage_tokens_once(1_800_000_000).await;
        assert_eq!(w.storage_token_failure_count(&ep), 1);
    }

    // F2 edge: a failed status read is a failure, never a skip, whatever the state rule says.
    #[tokio::test]
    async fn a_failed_status_read_is_still_a_failure() {
        let (w, _journal) = worker();
        let ep = crate::c2_tests::ready_named(&w, "app").await;
        w.pusher.set_capability(&ep, true);
        *w.pusher.fail_report.lock().unwrap() = Some("unreachable".into());
        let pass = w.refresh_storage_tokens_once(1_800_000_000).await;
        assert_eq!((pass.refreshed, pass.failed, pass.skipped), (0, 1, 0));
        assert_eq!(w.storage_token_failure_count(&ep), 1);
    }

    // F2 edge: several panicking passes in a row don't end the loop either.
    #[tokio::test(start_paused = true)]
    async fn the_loop_survives_panics_in_a_row() {
        use std::sync::Arc;
        use std::time::Duration;
        let (mut w, _journal) = worker();
        let ep = crate::c2_tests::ready_named(&w, "app").await;
        w.pusher.set_capability(&ep, true);
        w.storage_token_refresh_every = Duration::from_secs(60);
        *w.pusher.panic_report.lock().unwrap() = true;
        let w = Arc::new(w);
        let handle = tokio::spawn(super::storage_token_refresh_loop(w.clone()));
        tokio::time::sleep(Duration::from_secs(60 * 3 + 1)).await;
        assert!(!handle.is_finished());
        *w.pusher.panic_report.lock().unwrap() = false;
        tokio::time::sleep(Duration::from_secs(60)).await;
        assert_eq!(w.pusher.storage_tokens.lock().unwrap().len(), 1);
        handle.abort();
    }
}
