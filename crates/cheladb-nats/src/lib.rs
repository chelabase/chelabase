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

pub async fn ensure_control_stream(
    client: &async_nats::Client,
) -> Result<async_nats::jetstream::Context, NatsError> {
    let js = async_nats::jetstream::new(client.clone());
    match js.get_stream(STREAM_CONTROL).await {
        Ok(_) => {
            info!(stream = STREAM_CONTROL, "jetstream stream exists");
        }
        Err(_) => {
            js.create_stream(async_nats::jetstream::stream::Config {
                name: STREAM_CONTROL.to_string(),
                subjects: vec![
                    SUBJECT_PROVISION.to_string(),
                    SUBJECT_RECONCILE.to_string(),
                ],
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

/// Run the provision consumer loop, calling `handler` for each job.
pub async fn run_provision_consumer<F, Fut>(
    js: async_nats::jetstream::Context,
    mut handler: F,
) -> Result<(), NatsError>
where
    F: FnMut(JobEnvelope<ProvisionDatabaseJob>) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let stream = js
        .get_stream(STREAM_CONTROL)
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;

    let consumer = stream
        .create_consumer(async_nats::jetstream::consumer::pull::Config {
            durable_name: Some(CONSUMER_PROVISION.to_string()),
            filter_subject: SUBJECT_PROVISION.to_string(),
            ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
            ack_wait: std::time::Duration::from_secs(120),
            ..Default::default()
        })
        .await
        .map_err(|e| NatsError::JetStream(e.to_string()))?;

    info!(consumer = CONSUMER_PROVISION, "provision consumer ready");

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

        let job: JobEnvelope<ProvisionDatabaseJob> = match serde_json::from_slice(&message.payload)
        {
            Ok(j) => j,
            Err(err) => {
                warn!("invalid provision payload: {err}");
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
                warn!("provision handler failed: {err}");
                if let Err(nak_err) = message
                    .ack_with(async_nats::jetstream::AckKind::Nak(None))
                    .await
                {
                    warn!("nak failed: {nak_err}");
                }
            }
        }
    }

    Ok(())
}
