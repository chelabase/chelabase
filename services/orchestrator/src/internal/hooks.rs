//! Hooks the storage controller calls when storage placement changes
//! (`PUT {control-plane-url}/notify-attach` and `/notify-safekeepers`, neon
//! `storage_controller/src/compute_hook.rs`). Each running compute on the tenant or timeline
//! gets a fresh spec. The storage controller retries 503 and treats 400/401/403 as fatal, so
//! our own failures are always 503, an unparseable body included (the `Json` extractor would
//! answer 400/415/422).
//!
//! `notify-attach` also records the attach as a JetStream event (carried gaps T4, R7): a fresh
//! tenant's provision waits for it before creating its timeline.

use super::spec::endpoint_config;
use super::{BoxFuture, InternalState, NeonMetadataPort};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use chelabase_metadata::MetadataError;
use chelabase_nats::TenantAttachedEvent;
use chelabase_neon::reconfigure;
use chelabase_neon::{ComputeLocator, NeonId};
use futures_util::future::join_all;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{Instant, timeout_at};
use tracing::{error, info, warn};

/// Records that a tenant's shards are attached (`chelabase.neon.attached.<tenant>` on the
/// `CHELABASE_NEON_EVENTS` stream in production, a fake in tests). `Ok` only once the event is
/// stored (JetStream acked it).
pub trait AttachEvents: Send + Sync + 'static {
    fn tenant_attached(&self, event: TenantAttachedEvent) -> BoxFuture<'_, anyhow::Result<()>>;
}

impl AttachEvents for crate::NatsPorts {
    fn tenant_attached(&self, event: TenantAttachedEvent) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async move {
            chelabase_nats::publish_tenant_attached(&self.js, &event).await?;
            Ok(())
        })
    }
}

/// Other fields (`generation`, `safekeepers`) aren't needed: the spec lists the safekeepers.
#[derive(Debug, Deserialize)]
pub struct NotifySafekeepers {
    tenant_id: String,
    timeline_id: String,
}

fn parse<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, StatusCode> {
    serde_json::from_slice(body).map_err(|e| {
        error!("unparseable hook body: {e}");
        StatusCode::SERVICE_UNAVAILABLE
    })
}

pub async fn notify_attach<M: NeonMetadataPort, L: ComputeLocator>(
    State(state): State<Arc<InternalState<M, L>>>,
    body: Bytes,
) -> StatusCode {
    // One deadline for everything the hook does, taken first (A2).
    let deadline = Instant::now() + HOOK_DEADLINE;
    // The body is the event: `tenant_id` is required, the rest defaults (A3).
    let event: TenantAttachedEvent = match parse(&body) {
        Ok(r) => r,
        Err(code) => return code,
    };
    // A3: anything but a Neon id would name another subject (or none): never published.
    if let Err(e) = event.tenant_id.parse::<NeonId>() {
        error!("notify-attach: {e}");
        return StatusCode::SERVICE_UNAVAILABLE;
    }
    let tenant = event.tenant_id.clone();
    info!(%tenant, shards = event.shards.len(), "notify-attach");
    // The event and the reconfigures run side by side, so a hung NATS doesn't hold up the
    // computes (nor they the event); the event is published whatever the tenant's endpoints or
    // a failed metadata lookup (A4): a fresh provision's tenant has no running compute yet.
    let publish = async {
        match timeout_at(deadline, state.attach_events.tenant_attached(event)).await {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                warn!(%tenant, "publishing the tenant-attached event failed; the storage controller will retry: {e:#}");
                false
            }
            Err(_) => {
                warn!(%tenant, "the tenant-attached event wasn't acked before the hook's deadline; the storage controller will retry");
                false
            }
        }
    };
    let reconfigure = async {
        let endpoints =
            match timeout_at(deadline, state.metadata.neon_endpoints_for_tenant(&tenant)).await {
                Ok(endpoints) => endpoints,
                Err(_) => {
                    error!(%tenant, "looking up endpoints: no answer before the hook's deadline");
                    return StatusCode::SERVICE_UNAVAILABLE;
                }
            };
        reconfigure_all(&state, endpoints, deadline).await
    };
    let (published, reconfigured) = tokio::join!(publish, reconfigure);
    if published && reconfigured == StatusCode::OK {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

pub async fn notify_safekeepers<M: NeonMetadataPort, L: ComputeLocator>(
    State(state): State<Arc<InternalState<M, L>>>,
    body: Bytes,
) -> StatusCode {
    let deadline = Instant::now() + HOOK_DEADLINE;
    let req: NotifySafekeepers = match parse(&body) {
        Ok(r) => r,
        Err(code) => return code,
    };
    info!(tenant = %req.tenant_id, timeline = %req.timeline_id, "notify-safekeepers");
    let endpoints = state
        .metadata
        .neon_endpoints_for_timeline(&req.timeline_id)
        .await;
    reconfigure_all(&state, endpoints, deadline).await
}

/// The most one endpoint's reconfigure may take (locating it, its `/status`, building the spec,
/// the push).
const ENDPOINT_RECONFIGURE_TIMEOUT: Duration = Duration::from_secs(8);
/// How many endpoints one hook reconfigures at once.
const HOOK_CONCURRENCY: usize = 16;
/// When the hook answers, whatever is still running: the storage controller gives up on it
/// after 10 s and retries. An endpoint not started by then isn't started (it is retried with
/// the hook), and one still running then is cut short.
const HOOK_DEADLINE: Duration = Duration::from_secs(9);

/// Every endpoint is tried, even after one fails, concurrently, until the hook's `deadline`
/// (see [`each_bounded_until`]).
async fn reconfigure_all<M: NeonMetadataPort, L: ComputeLocator>(
    state: &InternalState<M, L>,
    endpoints: Result<Vec<String>, MetadataError>,
    deadline: Instant,
) -> StatusCode {
    let endpoints = match endpoints {
        Ok(ids) => ids,
        Err(e) => {
            error!("looking up endpoints: {e}");
            return StatusCode::SERVICE_UNAVAILABLE;
        }
    };
    each_bounded_until(deadline, &endpoints, |id| reconfigure_endpoint(state, id)).await
}

/// [`each_bounded_until`] with a deadline [`HOOK_DEADLINE`] from now.
#[cfg(test)]
async fn each_bounded<'a, F, Fut>(ids: &'a [String], reconfigure: F) -> StatusCode
where
    F: Fn(&'a str) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    each_bounded_until(Instant::now() + HOOK_DEADLINE, ids, reconfigure).await
}

/// Runs `reconfigure` for every id, [`HOOK_CONCURRENCY`] at a time, each bounded by
/// [`ENDPOINT_RECONFIGURE_TIMEOUT`] and all by `deadline` (the hook's, taken at its entry): 200
/// when all succeed, else 503 (the storage controller retries).
async fn each_bounded_until<'a, F, Fut>(
    deadline: Instant,
    ids: &'a [String],
    reconfigure: F,
) -> StatusCode
where
    F: Fn(&'a str) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    // A fair semaphore: endpoints start in order, `HOOK_CONCURRENCY` at a time.
    let permits = tokio::sync::Semaphore::new(HOOK_CONCURRENCY);
    let results = join_all(ids.iter().map(|id| {
        let permits = &permits;
        let attempt = reconfigure(id);
        async move {
            let result = match tokio::time::timeout_at(deadline, permits.acquire()).await {
                Ok(Ok(_permit)) => {
                    let now = tokio::time::Instant::now();
                    let until = deadline.min(now + ENDPOINT_RECONFIGURE_TIMEOUT);
                    match tokio::time::timeout_at(until, attempt).await {
                        Ok(result) => result,
                        Err(_) => Err(anyhow::anyhow!(
                            "no answer within {:?}",
                            until.duration_since(now)
                        )),
                    }
                }
                Ok(Err(_)) | Err(_) => Err(anyhow::anyhow!("not started before the hook's deadline")),
            };
            if let Err(e) = &result {
                warn!(endpoint = %id, "reconfigure failed; the storage controller will retry: {e:#}");
            }
            result.is_ok()
        }
    }))
    .await;
    if results.into_iter().all(|ok| ok) {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

/// Push the current spec to a running compute. A compute that isn't running pulls the current
/// spec when it boots, so there's nothing to do for it.
async fn reconfigure_endpoint<M: NeonMetadataPort, L: ComputeLocator>(
    state: &InternalState<M, L>,
    endpoint_id: &str,
) -> anyhow::Result<()> {
    let Some(control_addr) = state.locator.control_addr(endpoint_id).await? else {
        info!(endpoint = %endpoint_id, "compute not running; skipping reconfigure");
        return Ok(());
    };
    let Some(endpoint) = state.metadata.neon_endpoint(endpoint_id).await? else {
        // Deleted between the list lookup and now: nothing to reconfigure, nothing to retry.
        info!(endpoint = %endpoint_id, "endpoint deleted; skipping reconfigure");
        return Ok(());
    };
    // SP1: the same capability read as the spec fetch, from compute_ctl's own `/status`. A
    // failed report fails this reconfigure (the storage controller retries it); never a silent
    // push with a non-expiring token.
    let report =
        reconfigure::status_report(&state.http, control_addr, &state.signer, endpoint_id).await?;
    let config = endpoint_config(state, &endpoint, report.storage_token_refresh).await?;
    // The spec (its storage token kind) was built from this compute's report: re-check that the
    // compute at `control_addr` is still the endpoint's before pushing. A compute replaced at
    // another address is retried (its status is read again); none at all pulls its spec when it
    // boots. A restart at the same address isn't detected here: the worker's storage-token
    // refresh gives that compute the right token kind within its interval.
    match state.locator.control_addr(endpoint_id).await? {
        Some(now) if now == control_addr => {}
        Some(now) => anyhow::bail!(
            "the compute moved from {control_addr} to {now} while its status was read; retrying"
        ),
        None => {
            info!(endpoint = %endpoint_id, "compute stopped while its status was read; skipping reconfigure");
            return Ok(());
        }
    }
    reconfigure::push(
        &state.http,
        control_addr,
        &state.signer,
        endpoint_id,
        &config,
    )
    .await?;
    info!(endpoint = %endpoint_id, "reconfigured compute");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::time::Instant;

    fn ids(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// The storage controller's hook timeout.
    const STORCON_TIMEOUT: Duration = Duration::from_secs(10);

    // Several hung computes in one hook: they're waited on together, so the hook still answers
    // (503) before the storage controller's 10 s, and every endpoint was tried.
    #[tokio::test(start_paused = true)]
    async fn two_hung_endpoints_still_answer_the_hook_in_time() {
        let tried = Mutex::new(Vec::new());
        let started = Instant::now();
        let code = each_bounded(&ids(&["ep-a", "ep-b"]), |id| {
            tried.lock().unwrap().push(id.to_string());
            async {
                std::future::pending::<()>().await;
                Ok(())
            }
        })
        .await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert!(started.elapsed() < STORCON_TIMEOUT, "{:?}", started.elapsed());
        assert_eq!(*tried.lock().unwrap(), ["ep-a", "ep-b"]);
    }

    // Many hung computes, more than run at once: the hook still answers before the storage
    // controller gives up.
    #[tokio::test(start_paused = true)]
    async fn many_hung_endpoints_still_answer_in_time() {
        let names: Vec<String> = (0..50).map(|i| format!("ep-{i}")).collect();
        let started = Instant::now();
        let code = each_bounded(&names, |_| async {
            std::future::pending::<()>().await;
            Ok(())
        })
        .await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert!(started.elapsed() < STORCON_TIMEOUT, "{:?}", started.elapsed());
    }

    // F2 round 2: at most `HOOK_CONCURRENCY` reconfigures run at once; more endpoints than that,
    // each answering in a second, are all reconfigured within the storage controller's 10 s.
    #[tokio::test(start_paused = true)]
    async fn more_endpoints_than_the_cap_run_at_most_the_cap_at_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let names: Vec<String> = (0..40).map(|i| format!("ep-{i}")).collect();
        let running = AtomicUsize::new(0);
        let most = AtomicUsize::new(0);
        let done = AtomicUsize::new(0);
        let started = Instant::now();
        let code = each_bounded(&names, |_| {
            let (running, most, done) = (&running, &most, &done);
            async move {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                most.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(1)).await;
                running.fetch_sub(1, Ordering::SeqCst);
                done.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        })
        .await;
        assert_eq!(code, StatusCode::OK);
        assert_eq!(done.load(Ordering::SeqCst), 40);
        assert_eq!(most.load(Ordering::SeqCst), HOOK_CONCURRENCY);
        assert!(started.elapsed() < STORCON_TIMEOUT, "{:?}", started.elapsed());
    }

    // Endpoints that never got a turn before the hook's deadline aren't started late: the hook
    // answers 503 in time and the storage controller retries them.
    #[tokio::test(start_paused = true)]
    async fn endpoints_left_at_the_deadline_are_not_started() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let names: Vec<String> = (0..(HOOK_CONCURRENCY * 3)).map(|i| format!("ep-{i}")).collect();
        let started_count = AtomicUsize::new(0);
        let started = Instant::now();
        let code = each_bounded(&names, |_| {
            let started_count = &started_count;
            async move {
                started_count.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<()>().await;
                Ok(())
            }
        })
        .await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert!(started.elapsed() < STORCON_TIMEOUT, "{:?}", started.elapsed());
        let n = started_count.load(Ordering::SeqCst);
        assert!(n >= HOOK_CONCURRENCY && n < names.len(), "{n} started");
    }

    // A hung compute doesn't keep the others from being reconfigured; the hook is still 503.
    #[tokio::test(start_paused = true)]
    async fn a_hung_endpoint_does_not_hold_up_the_others() {
        let done = Mutex::new(Vec::new());
        let code = each_bounded(&ids(&["ep-hung", "ep-ok"]), |id| {
            let done = &done;
            async move {
                if id == "ep-hung" {
                    std::future::pending::<()>().await;
                }
                done.lock().unwrap().push(id.to_string());
                Ok(())
            }
        })
        .await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(*done.lock().unwrap(), ["ep-ok"]);
    }

    // T4 coder (A2): the reconfigures share the deadline the hook took at its entry: time the
    // hook already spent (its metadata lookup, while the event publishes) isn't given back.
    #[tokio::test(start_paused = true)]
    async fn the_reconfigures_end_at_a_deadline_taken_before_them() {
        let entry = Instant::now();
        let deadline = entry + HOOK_DEADLINE;
        tokio::time::sleep(Duration::from_secs(7)).await;
        let code = each_bounded_until(deadline, &ids(&["ep-hung"]), |_| async {
            std::future::pending::<()>().await;
            Ok(())
        })
        .await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        // Not 7 s + the endpoint's own 8 s.
        assert!(entry.elapsed() < STORCON_TIMEOUT, "{:?}", entry.elapsed());
    }

    // No endpoint, or all reconfigured: 200. One failure: 503.
    #[tokio::test(start_paused = true)]
    async fn the_answer_is_ok_only_when_every_endpoint_is() {
        assert_eq!(each_bounded(&[], |_| async { Ok(()) }).await, StatusCode::OK);
        assert_eq!(
            each_bounded(&ids(&["a", "b"]), |_| async { Ok(()) }).await,
            StatusCode::OK
        );
        assert_eq!(
            each_bounded(&ids(&["a", "b"]), |id| async move {
                anyhow::ensure!(id == "a", "b failed");
                Ok(())
            })
            .await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
