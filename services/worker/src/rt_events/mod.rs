//! The worker's realtime events (RT1 T5).
//!
//! - [`PublishingMetadata`] (P10, S20) wraps the worker's [`WorkerMetadata`]: the one choke
//!   point every branch and database status write goes through. Once a write returned success
//!   it publishes one [`ProjectEvent`] on `chelabase.rt.project.<project_id>` (never on a
//!   failure, never for a guarded write that changed nothing). A failed publish is logged and
//!   never fails the write. The wrapper resolves `(database_id, project_id)` with
//!   `get_branch`/`get_database`, **before** the write for deletes, and caches them per branch.
//! - [`StoppingBackend`] (P9, S19) wraps the worker's [`ComputeBackend`]: `delete(endpoint_id)`
//!   first publishes `chelabase.rt.compute.stopping.<branch_id>` for the endpoint's branch
//!   (found with [`WorkerMetadata::compute_branch`], so a deleting branch's too), then deletes
//!   (a failed publish never stops the delete). Static computes (no branch) and rehearsal
//!   branches' computes publish nothing.
//!
//! Which write publishes what (pinned by `rt1_tests::publisher`):
//!
//! | write | publishes when | event |
//! |---|---|---|
//! | `set_branch_status(id, _, to)` | `Ok(true)` | `branch.status` `to` |
//! | `set_branch_ready` | `Ok(true)` | `branch.status` `ready` |
//! | `set_branch_failed` | `Ok(())` and the row now `failed` (not once a delete began) | `branch.status` `failed` |
//! | `set_branch_error` | `Ok(())` | `branch.status`, the status the branch holds after it |
//! | `begin_branch_delete` | `Ok(Started)` | `branch.status` `deleting`, and the same for each backup the write moved along |
//! | `claim_branch_for_switch` | `Ok(Claimed)` | `branch.status` `resetting` |
//! | `claim_stale_reset` | `Ok(true)` | `branch.status` `resetting` |
//! | `fail_stuck_restore`, `fail_branch_if_stale` | `Ok(true)` | `branch.status` `failed` |
//! | `switch_to_restored_timeline(_, _, backup, ..)` | `Ok(true)` | `branch.created` (the backup) |
//! | `mark_branch_deleted` | `Ok(())`, unless the row was already a tombstone | `branch.deleted` (ids read before the write) |
//! | `set_status(db, s)` | `Ok(true)` | `branch.status` of main (its id is the database's), `s` as a branch status |
//! | `set_failed` | `Ok(())` and the database now `failed` (not once a delete began) | `branch.status` of main, `failed` |
//! | `fail_if_stale` | `Ok(true)` | `branch.status` of main, `failed` |
//! | `mark_deleted` | `Ok(())` | `database.deleted` (ids read before the write) |
//!
//! A rehearsal branch (a merge's, never shown to anyone) publishes nothing for any write.
//!
//! A lost event (a crash between the write and the publish, a NATS outage) is covered by
//! Studio's slow fallback poll (P10).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chelabase_metadata::{BranchKind, BranchStatus};
use chelabase_nats::NatsError;
use chelabase_nats::rt::ProjectEvent;
use chelabase_neon::backend::SqlOpts;
use chelabase_neon::compute_tls::IssuedCert;
use chelabase_neon::{ComputeBackend, ComputeLocator, ComputeStatus};
use chelabase_types::{BranchId, DatabaseId};
use futures_util::future::BoxFuture;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::ports::WorkerMetadata;

mod delegate;
#[cfg(test)]
mod tests;

/// How long [`NatsPublisher::compute_stopping`] waits for the signal to reach the server
/// before the compute goes anyway.
const STOPPING_FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// How long [`NatsPublisher::project`] waits for room in the client's buffer (full while NATS
/// is unreachable): a status write never waits on NATS longer than this.
const PROJECT_PUBLISH_TIMEOUT: Duration = Duration::from_secs(2);

/// Where the worker's realtime events go: NATS in the process, a no-op or a recorder in tests
/// (S20: `config::build` takes one and never connects NATS itself).
pub trait RtPublisher: Send + Sync + 'static {
    /// `ev` on `chelabase.rt.project.<project_id>`.
    fn project(&self, project_id: Uuid, ev: ProjectEvent) -> BoxFuture<'_, Result<(), NatsError>>;
    /// `{branch_id}` on `chelabase.rt.compute.stopping.<branch_id>`.
    fn compute_stopping(&self, branch_id: Uuid) -> BoxFuture<'_, Result<(), NatsError>>;
}

/// Publishes nothing (unit tests, and harnesses that don't watch events).
pub struct NoopPublisher;

impl RtPublisher for NoopPublisher {
    fn project(
        &self,
        _project_id: Uuid,
        _ev: ProjectEvent,
    ) -> BoxFuture<'_, Result<(), NatsError>> {
        Box::pin(async { Ok(()) })
    }

    fn compute_stopping(&self, _branch_id: Uuid) -> BoxFuture<'_, Result<(), NatsError>> {
        Box::pin(async { Ok(()) })
    }
}

/// [`RtPublisher`] over the process's NATS connection ([`chelabase_nats::rt`]).
pub struct NatsPublisher {
    client: async_nats::Client,
}

impl NatsPublisher {
    pub fn new(client: async_nats::Client) -> Self {
        Self { client }
    }
}

impl RtPublisher for NatsPublisher {
    fn project(&self, project_id: Uuid, ev: ProjectEvent) -> BoxFuture<'_, Result<(), NatsError>> {
        Box::pin(async move {
            let publish = chelabase_nats::rt::publish_project(&self.client, project_id, &ev);
            match tokio::time::timeout(PROJECT_PUBLISH_TIMEOUT, publish).await {
                Ok(result) => result,
                Err(_) => Err(NatsError::Client(
                    format!("publishing a project event: no room in {PROJECT_PUBLISH_TIMEOUT:?}")
                        .into(),
                )),
            }
        })
    }

    /// Published and flushed, both within [`STOPPING_FLUSH_TIMEOUT`]: the holder should hear of
    /// the stop before the compute goes, not after.
    fn compute_stopping(&self, branch_id: Uuid) -> BoxFuture<'_, Result<(), NatsError>> {
        Box::pin(async move {
            let signal = async {
                chelabase_nats::rt::publish_compute_stopping(&self.client, branch_id).await?;
                self.client
                    .flush()
                    .await
                    .map_err(|e| NatsError::Client(Box::new(e)))
            };
            match tokio::time::timeout(STOPPING_FLUSH_TIMEOUT, signal).await {
                Ok(result) => result,
                Err(_) => Err(NatsError::Client(
                    format!(
                        "flushing the compute-stopping signal: no answer in {STOPPING_FLUSH_TIMEOUT:?}"
                    )
                    .into(),
                )),
            }
        })
    }
}

/// The worker's metadata port with platform events (see the module docs).
pub struct PublishingMetadata<M> {
    inner: M,
    publisher: Arc<dyn RtPublisher>,
    /// Each branch's `(database, project)`: neither ever changes.
    ids: Mutex<HashMap<BranchId, (DatabaseId, Uuid)>>,
    /// Rehearsal branches seen: their writes publish nothing.
    quiet: Mutex<HashSet<BranchId>>,
}

impl<M> PublishingMetadata<M> {
    pub fn new(inner: M, publisher: Arc<dyn RtPublisher>) -> Self {
        Self {
            inner,
            publisher,
            ids: Mutex::default(),
            quiet: Mutex::default(),
        }
    }

    /// The wrapped port.
    pub fn inner(&self) -> &M {
        &self.inner
    }

    fn cached(&self, branch: BranchId) -> Option<(DatabaseId, Uuid)> {
        self.ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&branch)
            .copied()
    }

    fn remember(&self, branch: BranchId, database: DatabaseId, project: Uuid) {
        self.ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(branch, (database, project));
    }

    fn forget_branch(&self, branch: BranchId) {
        self.ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&branch);
        self.quiet
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&branch);
    }

    /// Whether `branch` is a rehearsal branch seen before.
    fn is_quiet(&self, branch: BranchId) -> bool {
        self.quiet
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&branch)
    }

    /// Marks `branch` a rehearsal branch: nothing is published for it.
    fn hush(&self, branch: BranchId) {
        self.quiet
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(branch);
    }

    fn forget_database(&self, database: DatabaseId) {
        self.ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, (db, _)| *db != database);
    }

    /// Publishes `ev`; a failure is logged, never returned.
    async fn publish(&self, project: Uuid, ev: ProjectEvent) {
        if let Err(e) = self.publisher.project(project, ev).await {
            warn!(project_id = %project, "publishing a realtime project event failed: {e}");
        }
    }
}

impl<M: WorkerMetadata> PublishingMetadata<M> {
    /// The project of `database` (cached under its main branch); `None` (logged) when it can't
    /// be read.
    async fn project_of_database(&self, database: DatabaseId) -> Option<Uuid> {
        let main = BranchId::from(database);
        if let Some((_, project)) = self.cached(main) {
            return Some(project);
        }
        match self.inner.get_database(database).await {
            Ok(Some(record)) => {
                let project = record.project_id.as_uuid();
                self.remember(main, database, project);
                Some(project)
            }
            Ok(None) => {
                debug!(database_id = %database, "no realtime event: the database is gone");
                None
            }
            Err(e) => {
                warn!(database_id = %database, "no realtime event: reading the database: {e}");
                None
            }
        }
    }

    /// `branch`'s database and project; `None` (logged) when they can't be read.
    async fn ids_of_branch(&self, branch: BranchId) -> Option<(DatabaseId, Uuid)> {
        if self.is_quiet(branch) {
            return None;
        }
        if let Some(ids) = self.cached(branch) {
            return Some(ids);
        }
        let database = match self.inner.get_branch(branch).await {
            Ok(Some(record)) if record.kind == BranchKind::Rehearsal => {
                self.hush(branch);
                return None;
            }
            Ok(Some(record)) => record.database_id,
            Ok(None) => {
                debug!(branch_id = %branch, "no realtime event: the branch is gone");
                return None;
            }
            Err(e) => {
                warn!(branch_id = %branch, "no realtime event: reading the branch: {e}");
                return None;
            }
        };
        let project = self.project_of_database(database).await?;
        self.remember(branch, database, project);
        Some((database, project))
    }

    /// `branch.status` for `branch`, now `status`.
    async fn branch_status(&self, branch: BranchId, status: BranchStatus) {
        if let Some((database, project)) = self.ids_of_branch(branch).await {
            self.publish(
                project,
                ProjectEvent::BranchStatus {
                    database_id: database.as_uuid(),
                    branch_id: branch.as_uuid(),
                    status: status.as_str().to_string(),
                },
            )
            .await;
        }
    }

    /// `branch.status` for `branch` with the status its row holds now: a write that sets only
    /// the error, or (`only`) a guarded write that reports nothing unless the row now holds
    /// that status (a failure recorded after a delete began changes nothing).
    async fn branch_status_as_stored(&self, branch: BranchId, only: Option<BranchStatus>) {
        let record = match self.inner.get_branch(branch).await {
            Ok(Some(record)) => record,
            Ok(None) => return,
            Err(e) => {
                warn!(branch_id = %branch, "no realtime event: reading the branch: {e}");
                return;
            }
        };
        if record.kind == BranchKind::Rehearsal {
            self.hush(branch);
            return;
        }
        if only.is_some_and(|status| status != record.status) {
            return;
        }
        let Some(project) = self.project_of_database(record.database_id).await else {
            return;
        };
        self.remember(branch, record.database_id, project);
        self.publish(
            project,
            ProjectEvent::BranchStatus {
                database_id: record.database_id.as_uuid(),
                branch_id: branch.as_uuid(),
                status: record.status.as_str().to_string(),
            },
        )
        .await;
    }

    /// `branch.status` for `database`'s main branch (its status mirrors the database's).
    async fn database_status(&self, database: DatabaseId, status: BranchStatus) {
        if let Some(project) = self.project_of_database(database).await {
            self.publish(
                project,
                ProjectEvent::BranchStatus {
                    database_id: database.as_uuid(),
                    branch_id: BranchId::from(database).as_uuid(),
                    status: status.as_str().to_string(),
                },
            )
            .await;
        }
    }
}

/// The worker's compute backend with the compute-stopping signal (see the module docs).
/// `metadata` resolves an endpoint's branch.
pub struct StoppingBackend<B, M> {
    inner: B,
    metadata: M,
    publisher: Arc<dyn RtPublisher>,
}

impl<B, M> StoppingBackend<B, M> {
    pub fn new(inner: B, metadata: M, publisher: Arc<dyn RtPublisher>) -> Self {
        Self {
            inner,
            metadata,
            publisher,
        }
    }

    /// The wrapped backend.
    pub fn inner(&self) -> &B {
        &self.inner
    }
}

impl<B: ComputeBackend, M: WorkerMetadata> StoppingBackend<B, M> {
    /// Announces that `endpoint_id`'s compute stops. Nothing for a compute without a branch or
    /// a rehearsal's; a lookup or publish failure is logged only (the stop goes ahead).
    async fn announce_stop(&self, endpoint_id: &str) {
        let branch = match self.metadata.compute_branch(endpoint_id).await {
            Ok(Some((branch, kind))) if kind != BranchKind::Rehearsal => branch,
            Ok(_) => return,
            Err(e) => {
                warn!(
                    endpoint_id,
                    "no compute-stopping signal: reading the endpoint's branch: {e}"
                );
                return;
            }
        };
        if let Err(e) = self.publisher.compute_stopping(branch.as_uuid()).await {
            warn!(endpoint_id, branch_id = %branch, "publishing the compute-stopping signal failed: {e}");
        }
    }
}

impl<B: ComputeBackend, M: WorkerMetadata> ComputeLocator for StoppingBackend<B, M> {
    async fn address(&self, endpoint_id: &str) -> anyhow::Result<Option<std::net::IpAddr>> {
        self.inner.address(endpoint_id).await
    }

    async fn control_addr(
        &self,
        endpoint_id: &str,
    ) -> anyhow::Result<Option<std::net::SocketAddr>> {
        self.inner.control_addr(endpoint_id).await
    }

    async fn postgres_addr(
        &self,
        endpoint_id: &str,
    ) -> anyhow::Result<Option<std::net::SocketAddr>> {
        self.inner.postgres_addr(endpoint_id).await
    }
}

impl<B: ComputeBackend, M: WorkerMetadata> ComputeBackend for StoppingBackend<B, M> {
    async fn create(&self, endpoint_id: &str, compute_token: &str) -> anyhow::Result<()> {
        self.inner.create(endpoint_id, compute_token).await
    }

    async fn delete(&self, endpoint_id: &str) -> anyhow::Result<()> {
        self.announce_stop(endpoint_id).await;
        self.inner.delete(endpoint_id).await
    }

    async fn read_file(&self, endpoint_id: &str, path: &str) -> anyhow::Result<Vec<u8>> {
        self.inner.read_file(endpoint_id, path).await
    }

    async fn upload_tls(&self, endpoint_id: &str, server: &IssuedCert) -> anyhow::Result<()> {
        self.inner.upload_tls(endpoint_id, server).await
    }

    async fn status(&self, endpoint_id: &str) -> anyhow::Result<Option<ComputeStatus>> {
        self.inner.status(endpoint_id).await
    }

    async fn list(&self) -> anyhow::Result<Vec<String>> {
        self.inner.list().await
    }

    async fn list_not_running(&self) -> anyhow::Result<Vec<String>> {
        self.inner.list_not_running().await
    }

    async fn admin_sql_with(
        &self,
        endpoint_id: &str,
        database: &str,
        sql: &str,
        opts: &SqlOpts,
    ) -> anyhow::Result<String> {
        self.inner
            .admin_sql_with(endpoint_id, database, sql, opts)
            .await
    }
}
