//! Client for the Neon storage controller's tenant, timeline and placement API.
//!
//! The Compose storage controller runs with `--dev`, so requests carry no token. The client
//! is plain HTTP only; an `https://` base fails every request.

use std::time::Duration;

use tokio::time::Instant;

use reqwest::StatusCode;
use serde::{Deserialize, Deserializer};
use serde_json::json;

use crate::id::NeonId;
use crate::lsn::Lsn;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DELETE_POLL_INTERVAL: Duration = Duration::from_millis(500);
const DELETE_POLL_ATTEMPTS: u32 = 120;

#[derive(Debug, thiserror::Error)]
pub enum StorconError {
    /// The storage controller understood the request and refused it (4xx).
    #[error("storage controller rejected the request ({status}): {message}")]
    Rejected { status: StatusCode, message: String },
    /// No answer, a timeout, a 5xx, or an answer we couldn't parse.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl From<reqwest::Error> for StorconError {
    fn from(e: reqwest::Error) -> Self {
        Self::Other(e.into())
    }
}

/// The subset of the storage controller's timeline description we use.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TimelineInfo {
    pub ancestor_lsn: Option<Lsn>,
    pub last_record_lsn: Lsn,
}

/// `GET /control/v1/tenant/{id}`: per-shard reconcile state.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TenantDescribe {
    pub shards: Vec<ShardStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ShardStatus {
    pub is_pending_compute_notification: bool,
    pub is_reconciling: bool,
    /// Empty when there is none.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub last_error: String,
}

fn null_as_empty<'de, D: Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Deserialize)]
struct LocateResponse {
    shards: Vec<LocateShard>,
}

#[derive(Debug, Deserialize)]
struct LocateShard {
    listen_pg_addr: String,
    listen_pg_port: u16,
}

#[derive(Debug, Deserialize)]
struct Safekeeper {
    host: String,
    port: u16,
    scheduling_policy: String,
}

#[derive(Clone)]
pub struct StorconClient {
    http: reqwest::Client,
    base: String,
    /// Warm secondary locations per tenant (`PlacementPolicy::Attached(n)`). With at least
    /// one, the storage controller can move a tenant off a draining or failed pageserver.
    tenant_secondaries: u32,
    request_timeout: Duration,
    delete_poll_interval: Duration,
    delete_poll_attempts: u32,
}

fn http_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .expect("reqwest client without TLS")
}

impl StorconClient {
    pub fn new(base: &str, tenant_secondaries: u32) -> Self {
        Self {
            http: http_client(REQUEST_TIMEOUT),
            base: base.trim_end_matches('/').to_string(),
            tenant_secondaries,
            request_timeout: REQUEST_TIMEOUT,
            delete_poll_interval: DELETE_POLL_INTERVAL,
            delete_poll_attempts: DELETE_POLL_ATTEMPTS,
        }
    }

    /// Per-request timeout (default 30 s).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.http = http_client(timeout);
        self.request_timeout = timeout;
        self
    }

    /// How deletes wait for the storage controller (default 120 × 500 ms). At least one
    /// request is always sent.
    pub fn with_delete_poll(mut self, interval: Duration, attempts: u32) -> Self {
        self.delete_poll_interval = interval;
        self.delete_poll_attempts = attempts.max(1);
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    pub async fn create_tenant(&self, tenant: &NeonId) -> Result<(), StorconError> {
        let resp = self
            .http
            .post(self.url("/v1/tenant"))
            .json(&json!({
                "new_tenant_id": tenant,
                "placement_policy": { "Attached": self.tenant_secondaries },
            }))
            .send()
            .await?;
        check(resp).await.map(drop)
    }

    pub async fn describe_tenant(&self, tenant: &NeonId) -> Result<TenantDescribe, StorconError> {
        let resp = self
            .http
            .get(self.url(&format!("/control/v1/tenant/{tenant}")))
            .send()
            .await?;
        Ok(check(resp).await?.json().await?)
    }

    pub async fn delete_tenant(&self, tenant: &NeonId) -> Result<(), StorconError> {
        self.delete_until_gone(self.url(&format!("/v1/tenant/{tenant}")))
            .await
    }

    /// Create a timeline: a fresh one (`ancestor` is None) or a branch of `ancestor`,
    /// optionally at a specific LSN.
    pub async fn create_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
        pg_version: u32,
        ancestor: Option<(&NeonId, Option<Lsn>)>,
    ) -> Result<TimelineInfo, StorconError> {
        let body = match ancestor {
            None => json!({ "new_timeline_id": timeline, "pg_version": pg_version }),
            Some((ancestor_timeline, lsn)) => json!({
                "new_timeline_id": timeline,
                "ancestor_timeline_id": ancestor_timeline,
                "ancestor_start_lsn": lsn,
                "pg_version": pg_version,
            }),
        };
        let resp = self
            .http
            .post(self.url(&format!("/v1/tenant/{tenant}/timeline")))
            .json(&body)
            .send()
            .await?;
        Ok(check(resp).await?.json().await?)
    }

    pub async fn get_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<TimelineInfo, StorconError> {
        let resp = self
            .http
            .get(self.url(&format!("/v1/tenant/{tenant}/timeline/{timeline}")))
            .send()
            .await?;
        Ok(check(resp).await?.json().await?)
    }

    pub async fn delete_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<(), StorconError> {
        self.delete_until_gone(self.url(&format!("/v1/tenant/{tenant}/timeline/{timeline}")))
            .await
    }

    /// `postgresql://no_user@host:port` of the pageserver an (unsharded) tenant is attached to.
    pub async fn pageserver_connstring(&self, tenant: &NeonId) -> Result<String, StorconError> {
        let resp = self
            .http
            .get(self.url(&format!("/debug/v1/tenant/{tenant}/locate")))
            .send()
            .await?;
        let located: LocateResponse = check(resp).await?.json().await?;
        let shard = located
            .shards
            .first()
            .ok_or_else(|| anyhow::anyhow!("tenant {tenant} has no attached shards"))?;
        Ok(format!(
            "postgresql://no_user@{}:{}",
            shard.listen_pg_addr, shard.listen_pg_port
        ))
    }

    /// Every tenant the controller knows: `GET /control/v1/tenant` without a query lists all.
    pub async fn list_tenant_ids(&self) -> Result<Vec<NeonId>, StorconError> {
        #[derive(Deserialize)]
        struct Listed {
            tenant_id: String,
        }
        let resp = self.http.get(self.url("/control/v1/tenant")).send().await?;
        let listed: Vec<Listed> = check(resp).await?.json().await?;
        listed
            .into_iter()
            .map(|t| {
                t.tenant_id.parse::<NeonId>().map_err(|e| {
                    StorconError::Other(anyhow::anyhow!("tenant id {:?}: {e}", t.tenant_id))
                })
            })
            .collect()
    }

    /// `host:port` of every active safekeeper.
    pub async fn active_safekeepers(&self) -> Result<Vec<String>, StorconError> {
        let resp = self
            .http
            .get(self.url("/control/v1/safekeeper"))
            .send()
            .await?;
        let all: Vec<Safekeeper> = check(resp).await?.json().await?;
        Ok(all
            .into_iter()
            .filter(|sk| sk.scheduling_policy == "Active")
            .map(|sk| format!("{}:{}", sk.host, sk.port))
            .collect())
    }

    /// Deletion is asynchronous: the storage controller answers 202 while it is in
    /// progress and 200 once the object is gone (404, or any other 2xx, also means gone).
    /// The whole wait is bounded by attempts × interval plus one request timeout, so a
    /// controller that answers 202 slowly can't stretch it out.
    async fn delete_until_gone(&self, url: String) -> Result<(), StorconError> {
        let deadline = Instant::now()
            + self.delete_poll_interval * self.delete_poll_attempts
            + self.request_timeout;
        for attempt in 1..=self.delete_poll_attempts {
            let resp = tokio::time::timeout_at(deadline, self.http.delete(&url).send())
                .await
                .map_err(|_| anyhow::anyhow!("timed out waiting for deletion of {url}"))??;
            match resp.status() {
                StatusCode::ACCEPTED => {
                    if attempt < self.delete_poll_attempts {
                        tokio::time::sleep_until(
                            (Instant::now() + self.delete_poll_interval).min(deadline),
                        )
                        .await;
                    }
                }
                StatusCode::NOT_FOUND => return Ok(()),
                status if status.is_success() => return Ok(()),
                _ => {
                    check(resp).await?;
                }
            }
        }
        Err(anyhow::anyhow!("timed out waiting for deletion of {url}").into())
    }
}

async fn check(resp: reqwest::Response) -> Result<reqwest::Response, StorconError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    // Errors come back as {"msg": "..."}; fall back to the raw body.
    let message = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("msg").and_then(|m| m.as_str()).map(str::to_string))
        .unwrap_or(body);
    if status.is_client_error() {
        Err(StorconError::Rejected { status, message })
    } else {
        Err(anyhow::anyhow!("storage controller returned {status}: {message}").into())
    }
}
