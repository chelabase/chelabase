//! NATS JetStream subjects, envelopes, and helpers for Cheladb control plane.

use cheladb_types::{DatabaseId, ProjectId, TenantId};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};
use uuid::Uuid;

/// Subject for database provisioning jobs.
pub const SUBJECT_PROVISION: &str = "cheladb.provision";

/// Subject for reconciliation jobs (reserved).
pub const SUBJECT_RECONCILE: &str = "cheladb.reconcile";

/// JetStream stream name for control-plane jobs.
pub const STREAM_CONTROL: &str = "CHELADB_CONTROL";

/// Durable consumer name for the provision worker.
pub const CONSUMER_PROVISION: &str = "cheladb-worker-provision";

/// How long a failed job waits before JetStream redelivers it.
pub const REDELIVERY_DELAY: std::time::Duration = std::time::Duration::from_secs(10);

/// Durable delete jobs (container + volume teardown).
pub const SUBJECT_DELETE: &str = "cheladb.delete";
pub const CONSUMER_DELETE: &str = "cheladb-worker-delete";

/// Core request/reply (not JetStream): reset a database's owner password.
pub const SUBJECT_RESET_PASSWORD: &str = "cheladb.rpc.reset_password";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobEnvelope<T> {
    pub job_id: Uuid,
    pub payload: T,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvisionDatabaseJob {
    pub database_id: DatabaseId,
    pub project_id: ProjectId,
    pub tenant_id: TenantId,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteDatabaseJob {
    pub database_id: DatabaseId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResetPasswordRequest {
    pub database_id: DatabaseId,
}

/// The worker's reply error when the new password is stored but the compute doesn't have it yet.
pub const RESET_NOT_APPLIED: &str = "the password was changed but not yet applied; it will be retried";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResetPasswordReply {
    pub ok: bool,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconcileDatabaseJob {
    pub database_id: DatabaseId,
}

/// NATS connection settings.
#[derive(Debug, Clone)]
pub struct NatsConfig {
    pub url: String,
}

impl NatsConfig {
    pub fn from_env() -> Result<Self, String> {
        let url = std::env::var("CHELADB_NATS_URL")
            .unwrap_or_else(|_| "nats://127.0.0.1:4222".to_string());
        Ok(Self { url })
    }
}

#[derive(Debug, Error)]
pub enum NatsError {
    #[error("nats connect error: {0}")]
    Connect(#[from] async_nats::ConnectError),
    #[error("nats error: {0}")]
    Client(#[from] async_nats::Error),
    #[error("jetstream error: {0}")]
    JetStream(String),
    #[error("serialize error: {0}")]
    Serialize(#[from] serde_json::Error),
}

pub use async_nats::jetstream::Context as JetStreamContext;

pub async fn connect(config: &NatsConfig) -> Result<async_nats::Client, NatsError> {
    Ok(async_nats::connect(&config.url).await?)
}

/// Every subject the control stream must carry.
pub fn control_subjects() -> Vec<String> {
    vec![
        SUBJECT_PROVISION.to_string(),
        SUBJECT_RECONCILE.to_string(),
        SUBJECT_DELETE.to_string(),
    ]
}

/// Control subjects an existing stream doesn't carry yet.
pub fn missing_subjects(existing: &[String]) -> Vec<String> {
    control_subjects()
        .into_iter()
        .filter(|s| !existing.contains(s))
        .collect()
}

/// Create the control stream, or add subjects an older stream is missing.
pub async fn ensure_control_stream(
    client: &async_nats::Client,
) -> Result<async_nats::jetstream::Context, NatsError> {
    let js = async_nats::jetstream::new(client.clone());
    match js.get_stream(STREAM_CONTROL).await {
        Ok(stream) => {
            let mut config = stream.cached_info().config.clone();
            let missing = missing_subjects(&config.subjects);
            if missing.is_empty() {
                info!(stream = STREAM_CONTROL, "jetstream stream exists");
            } else {
                config.subjects.extend(missing.iter().cloned());
                js.update_stream(config)
                    .await
                    .map_err(|e| NatsError::JetStream(e.to_string()))?;
                info!(stream = STREAM_CONTROL, ?missing, "jetstream stream subjects added");
            }
        }
        Err(_) => {
            js.create_stream(async_nats::jetstream::stream::Config {
                name: STREAM_CONTROL.to_string(),
                subjects: control_subjects(),
                retention: async_nats::jetstream::stream::RetentionPolicy::WorkQueue,
                ..Default::default()
            })
            .await
            .map_err(|e| NatsError::JetStream(e.to_string()))?;
            info!(stream = STREAM_CONTROL, "jetstream stream created");
        }
    }
    Ok(js)
}

pub async fn publish_provision(
    js: &async_nats::jetstream::Context,
    job: &JobEnvelope<ProvisionDatabaseJob>,
) -> Result<(), NatsError> {
    let payload = serde_json::to_vec(job)?;
    js.publish(SUBJECT_PROVISION.to_string(), payload.into())
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;
    Ok(())
}

pub async fn publish_delete(
    js: &async_nats::jetstream::Context,
    job: &JobEnvelope<DeleteDatabaseJob>,
) -> Result<(), NatsError> {
    let payload = serde_json::to_vec(job)?;
    js.publish(SUBJECT_DELETE.to_string(), payload.into())
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;
    Ok(())
}

/// Run the delete consumer loop, calling `handler` for each job.
pub async fn run_delete_consumer<F, Fut>(
    js: async_nats::jetstream::Context,
    handler: F,
) -> Result<(), NatsError>
where
    F: FnMut(JobEnvelope<DeleteDatabaseJob>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    run_consumer(js, CONSUMER_DELETE, SUBJECT_DELETE, handler).await
}

/// Run the provision consumer loop, calling `handler` for each job.
pub async fn run_provision_consumer<F, Fut>(
    js: async_nats::jetstream::Context,
    handler: F,
) -> Result<(), NatsError>
where
    F: FnMut(JobEnvelope<ProvisionDatabaseJob>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    run_consumer(js, CONSUMER_PROVISION, SUBJECT_PROVISION, handler).await
}

/// How long a job may run before NATS redelivers it. The worst case is a provision that fails
/// late and tears down: two storage-controller calls (10 s each), the 60 s readiness wait, up
/// to 30 s of admin SQL, then the teardown (a tenant delete waits up to 60 s, plus 10 s per
/// request), about 180 s. A redelivery in the middle would run a second copy of the job.
///
/// Limits:
/// - Docker calls (bollard) have no timeout of their own, so a hung Docker can outlast it.
/// - The consumer prefetches messages and handles them one at a time. Each one's timer starts
///   when it's delivered, so a job queued behind slow ones can be redelivered before it runs.
/// - A redelivered provision whose first copy failed provisions again: `failed` rows aren't
///   skipped, so the database can still become ready.
const ACK_WAIT: std::time::Duration = std::time::Duration::from_secs(300);

/// Pull `subject` with durable consumer `durable`, acking handled jobs and
/// nak-ing failures for redelivery. Unparseable payloads are acked (dropped).
async fn run_consumer<T, F, Fut>(
    js: async_nats::jetstream::Context,
    durable: &str,
    subject: &str,
    mut handler: F,
) -> Result<(), NatsError>
where
    T: serde::de::DeserializeOwned,
    F: FnMut(JobEnvelope<T>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let stream = js
        .get_stream(STREAM_CONTROL)
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;

    let consumer = stream
        .create_consumer(async_nats::jetstream::consumer::pull::Config {
            durable_name: Some(durable.to_string()),
            filter_subject: subject.to_string(),
            ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
            ack_wait: ACK_WAIT,
            ..Default::default()
        })
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;

    info!(consumer = durable, "consumer ready");

    let mut messages = consumer
        .messages()
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;

    while let Some(message) = messages.next().await {
        let message = match message {
            Ok(m) => m,
            Err(err) => {
                warn!("consumer message error: {err}");
                continue;
            }
        };

        let job: JobEnvelope<T> = match serde_json::from_slice(&message.payload)
        {
            Ok(j) => j,
            Err(err) => {
                warn!(consumer = durable, "invalid job payload: {err}");
                let _ = message.ack().await;
                continue;
            }
        };

        match handler(job).await {
            Ok(()) => {
                if let Err(err) = message.ack().await {
                    warn!("ack failed: {err}");
                }
            }
            Err(err) => {
                warn!(consumer = durable, "job handler failed: {err}");
                // Back off before redelivery: a failing dependency (e.g. Docker
                // down) would otherwise spin through redeliveries.
                if let Err(nak_err) = message
                    .ack_with(async_nats::jetstream::AckKind::Nak(Some(REDELIVERY_DELAY)))
                    .await
                {
                    warn!("nak failed: {nak_err}");
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_subjects_include_delete() {
        assert!(control_subjects().contains(&SUBJECT_DELETE.to_string()));
        assert_eq!(
            missing_subjects(&["cheladb.provision".into()]),
            vec![SUBJECT_RECONCILE.to_string(), SUBJECT_DELETE.to_string()]
        );
        assert!(missing_subjects(&control_subjects()).is_empty());
    }
}
