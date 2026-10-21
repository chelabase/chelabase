//! Realtime subjects and payloads (RT1 T1): shared by every publisher and by realtime.
//!
//! Subjects end in the canonical hyphenated lowercase UUID. The NATS users' permissions
//! (`tools/cli/src/workbench/service_auth.rs`) are written against exactly these prefixes:
//! the orchestrator publishes [`PROJECT`] and [`ACCESS`], the worker [`PROJECT`] and
//! [`COMPUTE_STOPPING`], realtime [`ROWS`], [`WATCH`] and [`PRESENCE`] and its two buckets.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::NatsError;

/// `.<tenant_id>`: a tenant's members or their rights changed.
pub const ACCESS: &str = "chelabase.rt.access";
/// `.<project_id>`: a branch, database or Postgres database of the project changed.
pub const PROJECT: &str = "chelabase.rt.project";
/// `.<branch_id>`: the platform is about to stop or restart the branch's compute.
pub const COMPUTE_STOPPING: &str = "chelabase.rt.compute.stopping";
/// `.<branch_id>.<db_oid>`: watch heartbeats for a database.
pub const WATCH: &str = "chelabase.rt.watch";
/// `.<branch_id>.<db_oid>.<rel_oid>`: row changes of a table.
pub const ROWS: &str = "chelabase.rt.rows";
/// `.<channel_hash>`, `.hb.<instance_id>` or `.sync.<channel_hash>`: presence.
pub const PRESENCE: &str = "chelabase.rt.presence";
/// The KV bucket of decoder leases (one holder per compute database).
pub const LEASES_BUCKET: &str = "CHELABASE_RT_LEASES";
/// The KV bucket of replication-slot index keys (the per-compute slot cap).
pub const SLOTS_BUCKET: &str = "CHELABASE_RT_SLOTS";

/// A change to a project's branches or databases, tagged by `event`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum ProjectEvent {
    #[serde(rename = "branch.status")]
    BranchStatus {
        database_id: Uuid,
        branch_id: Uuid,
        status: String,
    },
    #[serde(rename = "branch.created")]
    BranchCreated { database_id: Uuid, branch_id: Uuid },
    #[serde(rename = "branch.deleted")]
    BranchDeleted { database_id: Uuid, branch_id: Uuid },
    #[serde(rename = "database.created")]
    DatabaseCreated { database_id: Uuid },
    #[serde(rename = "database.deleted")]
    DatabaseDeleted { database_id: Uuid },
    #[serde(rename = "pg_database.created")]
    PgDatabaseCreated {
        database_id: Uuid,
        branch_id: Uuid,
        name: String,
    },
    #[serde(rename = "pg_database.dropped")]
    PgDatabaseDropped {
        database_id: Uuid,
        branch_id: Uuid,
        name: String,
    },
}

/// A tenant's access changed. `member_sub: None` (sent as `null`) means the whole tenant: a
/// writer that doesn't know the member (an invite, a tenant-wide change) sends that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessEvent {
    pub tenant_id: Uuid,
    pub member_sub: Option<String>,
}

/// The branch's compute is about to stop or restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputeStopping {
    pub branch_id: Uuid,
}

/// `<prefix>.<id>`, the id in its canonical hyphenated lowercase form.
fn subject(prefix: &str, id: Uuid) -> String {
    format!("{prefix}.{}", id.hyphenated())
}

async fn publish_json(
    client: &async_nats::Client,
    subject: String,
    body: &impl Serialize,
) -> Result<(), NatsError> {
    let payload = serde_json::to_vec(body)?;
    client
        .publish(subject, payload.into())
        .await
        .map_err(|e| NatsError::Client(Box::new(e)))
}

/// Publishes `ev` on `chelabase.rt.project.<project_id>`. Callers log a failure and carry on:
/// an event never fails the write it reports.
pub async fn publish_project(
    client: &async_nats::Client,
    project_id: Uuid,
    ev: &ProjectEvent,
) -> Result<(), NatsError> {
    publish_json(client, subject(PROJECT, project_id), ev).await
}

/// Publishes `ev` on `chelabase.rt.access.<tenant_id>`. Callers log a failure and carry on.
pub async fn publish_access(
    client: &async_nats::Client,
    ev: &AccessEvent,
) -> Result<(), NatsError> {
    publish_json(client, subject(ACCESS, ev.tenant_id), ev).await
}

/// Publishes `{branch_id}` on `chelabase.rt.compute.stopping.<branch_id>`. Callers log a
/// failure and carry on.
pub async fn publish_compute_stopping(
    client: &async_nats::Client,
    branch_id: Uuid,
) -> Result<(), NatsError> {
    publish_json(
        client,
        subject(COMPUTE_STOPPING, branch_id),
        &ComputeStopping { branch_id },
    )
    .await
}
