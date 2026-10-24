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
/// Creating a timeline (a branch of a large one included) may take longer than a request.
pub const CREATE_TIMELINE_TIMEOUT: Duration = Duration::from_secs(120);
/// Detaching copies the ancestor's layers the timeline uses: minutes on a large one.
pub const DETACH_TIMEOUT: Duration = Duration::from_secs(600);
const DELETE_POLL_INTERVAL: Duration = Duration::from_millis(500);
const DELETE_POLL_ATTEMPTS: u32 = 120;
const SCHEDULE_RETRY_BASE: Duration = Duration::from_millis(250);
const SCHEDULE_RETRY_CAP: Duration = Duration::from_secs(2);
/// Above storcon's own 30 s wait for a reconcile, so a create it is still working on can be
/// asked again after it gave up.
const SCHEDULE_RETRY_BUDGET: Duration = Duration::from_secs(45);
/// The least an attempt may take, so a budget that is nearly (or entirely) spent still sends
/// a request that has a chance to be answered. It is the most a create can overrun the budget.
const MIN_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug, thiserror::Error)]
pub enum StorconError {
    /// The storage controller understood the request and refused it (4xx).
    #[error("storage controller rejected the request ({status}): {message}")]
    Rejected { status: StatusCode, message: String },
    /// No answer, a timeout, a 5xx, or an answer we couldn't parse.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// A 5xx answer, carried inside [`StorconError::Other`] so that every caller still sees the
/// same variant and text, while [`StorconError::is_tenant_attaching`] can read the status.
#[derive(Debug, thiserror::Error)]
#[error("storage controller returned {status}: {message}")]
struct ServerError {
    status: StatusCode,
    message: String,
}

impl StorconError {
    /// The storage controller has no pageserver to place the tenant on (right after a
    /// pageserver restart, until its next heartbeat): a 409 that clears by itself.
    pub fn is_unschedulable(&self) -> bool {
        match self {
            Self::Rejected { status, message } if *status == StatusCode::CONFLICT => {
                // `exhausted` may have appended its note.
                let message = strip_attempts_note(message);
                message.starts_with("Conflict: ")
                    && (message.ends_with(": No pageserver found matching constraint")
                        || message.ends_with(": No pageservers found"))
            }
            _ => false,
        }
    }

    /// Storcon forwarded the pageserver's 404 for the tenant (`NotFound: 1 (pageserver):
    /// NotFound: tenant <id>`): the pageserver hasn't loaded the tenant yet (right after a
    /// restart), which clears by itself. Storcon's own 404 (a tenant it doesn't know) is not.
    pub fn is_pageserver_tenant_not_found(&self) -> bool {
        match self {
            Self::Rejected { status, message } if *status == StatusCode::NOT_FOUND => {
                message.contains("(pageserver): NotFound: tenant")
            }
            _ => false,
        }
    }

    /// The request timed out (no answer within the per-request timeout).
    pub fn is_request_timeout(&self) -> bool {
        match self {
            Self::Other(e) => e
                .downcast_ref::<reqwest::Error>()
                .is_some_and(reqwest::Error::is_timeout),
            Self::Rejected { .. } => false,
        }
    }

    /// The 503 a timeline create gets while its tenant is still attaching to a pageserver.
    pub fn is_tenant_attaching(&self) -> bool {
        match self {
            Self::Other(e) => e.downcast_ref::<ServerError>().is_some_and(|s| {
                s.status == StatusCode::SERVICE_UNAVAILABLE
                    && s.message.contains("tenant active state")
                    && s.message.contains("Attaching")
            }),
            Self::Rejected { .. } => false,
        }
    }

    /// The same error with `(after N attempts in Xs)` appended to the storage controller's
    /// message.
    fn exhausted(self, attempts: u32, elapsed: Duration) -> Self {
        let note = format!(
            "(after {attempts} attempts in {:.1}s)",
            elapsed.as_secs_f64()
        );
        match self {
            Self::Rejected { status, message } => Self::Rejected {
                status,
                message: format!("{message} {note}"),
            },
            Self::Other(e) => match e.downcast::<ServerError>() {
                Ok(s) => Self::Other(
                    ServerError {
                        status: s.status,
                        message: format!("{} {note}", s.message),
                    }
                    .into(),
                ),
                Err(e) => Self::Other(e),
            },
        }
    }
}

/// `message` without a trailing ` (after N attempts in X.Xs)` note added by
/// [`StorconError::exhausted`].
fn strip_attempts_note(message: &str) -> &str {
    let Some(start) = message.rfind(" (after ") else {
        return message;
    };
    let Some(inner) = message[start + " (after ".len()..].strip_suffix("s)") else {
        return message;
    };
    let is_note = inner.split_once(" attempts in ").is_some_and(|(n, secs)| {
        !n.is_empty()
            && n.bytes().all(|b| b.is_ascii_digit())
            && secs.split_once('.').is_some_and(|(whole, frac)| {
                !whole.is_empty()
                    && !frac.is_empty()
                    && whole.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit())
            })
    });
    if is_note { &message[..start] } else { message }
}

impl From<reqwest::Error> for StorconError {
    fn from(e: reqwest::Error) -> Self {
        Self::Other(e.into())
    }
}

/// The subset of the storage controller's timeline description we use.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TimelineInfo {
    /// The timeline this one branched from; `None` for a root or a detached timeline.
    #[serde(default)]
    pub ancestor_timeline_id: Option<String>,
    pub ancestor_lsn: Option<Lsn>,
    pub last_record_lsn: Lsn,
    /// How far GC has advanced: the pageserver refuses reads below it (`check_lsn_is_in_scope`).
    /// `None` when the reply doesn't carry it.
    #[serde(default)]
    pub applied_gc_cutoff_lsn: Option<Lsn>,
}

/// A timeline's head: its `last_record_lsn`, or the safekeepers' commit LSN (read from them
/// by [`crate::safekeeper::SafekeeperClient`]) when that's ahead (the pageserver hasn't
/// ingested it yet, e.g. right after a tenant reload).
pub fn head_lsn(last_record: Lsn, safekeeper_commit: Option<Lsn>) -> Lsn {
    safekeeper_commit.map_or(last_record, |commit| commit.max(last_record))
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
    id: u64,
    host: String,
    port: u16,
    scheduling_policy: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LsnKind {
    Present,
    Future,
    Past,
    Nodata,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LsnAtTime {
    pub lsn: Lsn,
    pub kind: LsnKind,
}

#[derive(Clone)]
pub struct StorconClient {
    http: reqwest::Client,
    base: String,
    /// Warm secondary locations per tenant (`PlacementPolicy::Attached(n)`). With at least
    /// one, the storage controller can move a tenant off a draining or failed pageserver.
    tenant_secondaries: u32,
    request_timeout: Duration,
    create_timeline_timeout: Duration,
    detach_timeout: Duration,
    delete_poll_interval: Duration,
    delete_poll_attempts: u32,
    schedule_retry_base: Duration,
    schedule_retry_cap: Duration,
    schedule_retry_budget: Duration,
    timeline_retry_budget: Duration,
    /// The NeonJWT sent as `Authorization: Bearer` on every request (scope `admin`).
    token: Option<String>,
}

/// A client sending `token` as a bearer on every request.
pub(crate) fn http_client_with(timeout: Duration, token: Option<&str>) -> reqwest::Client {
    let mut builder = reqwest::Client::builder().timeout(timeout);
    if let Some(token) = token {
        let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
            .expect("a JWT is a valid header value");
        value.set_sensitive(true);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::AUTHORIZATION, value);
        builder = builder.default_headers(headers);
    }
    builder.build().expect("reqwest client without TLS")
}

impl StorconClient {
    pub fn new(base: &str, tenant_secondaries: u32) -> Self {
        Self {
            http: http_client_with(REQUEST_TIMEOUT, None),
            token: None,
            base: base.trim_end_matches('/').to_string(),
            tenant_secondaries,
            request_timeout: REQUEST_TIMEOUT,
            create_timeline_timeout: CREATE_TIMELINE_TIMEOUT,
            detach_timeout: DETACH_TIMEOUT,
            delete_poll_interval: DELETE_POLL_INTERVAL,
            delete_poll_attempts: DELETE_POLL_ATTEMPTS,
            schedule_retry_base: SCHEDULE_RETRY_BASE,
            schedule_retry_cap: SCHEDULE_RETRY_CAP,
            schedule_retry_budget: SCHEDULE_RETRY_BUDGET,
            timeline_retry_budget: CREATE_TIMELINE_TIMEOUT,
        }
    }

    /// Per-request timeout (default 30 s).
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.http = http_client_with(timeout, self.token.as_deref());
        self.request_timeout = timeout;
        self
    }

    /// Sends `token` (a NeonJWT with scope `admin`) as the bearer on every request: the
    /// storage controller runs without `--dev` and refuses unauthenticated calls.
    pub fn with_token(mut self, token: String) -> Self {
        self.http = http_client_with(self.request_timeout, Some(&token));
        self.token = Some(token);
        self
    }

    /// Per-request timeouts of creating a timeline and of detaching one from its ancestor
    /// (default [`CREATE_TIMELINE_TIMEOUT`] and [`DETACH_TIMEOUT`]), which replace the
    /// request timeout for those two calls.
    pub fn with_timeline_timeouts(mut self, create: Duration, detach: Duration) -> Self {
        self.create_timeline_timeout = create;
        self.detach_timeout = detach;
        self
    }

    /// How deletes wait for the storage controller (default 120 × 500 ms). At least one
    /// request is always sent.
    pub fn with_delete_poll(mut self, interval: Duration, attempts: u32) -> Self {
        self.delete_poll_interval = interval;
        self.delete_poll_attempts = attempts.max(1);
        self
    }

    /// How a tenant create and a timeline create retry the refusals that clear by themselves
    /// (no schedulable pageserver; tenant still attaching): exponential backoff from `base`
    /// doubling up to `cap`, each wait random between half the step and all of it, for at
    /// most `budget` in all (defaults 250 ms, 2 s, 45 s, above storcon's own 30 s reconcile
    /// wait; this sets the timeline create's budget too, whose default is 120 s, see
    /// [`Self::with_timeline_retry_budget`]). Each attempt's timeout is capped at what is left of the budget (at least
    /// 250 ms), so a create overruns the budget by at most that. A tenant create also retries
    /// a request timeout (it is idempotent, a lost success included). A zero budget sends one
    /// request.
    pub fn with_schedule_retry(mut self, base: Duration, cap: Duration, budget: Duration) -> Self {
        self.schedule_retry_base = base;
        self.schedule_retry_cap = cap;
        self.schedule_retry_budget = budget;
        self.timeline_retry_budget = budget;
        self
    }

    /// The budget of a timeline create alone (default 120 s, [`CREATE_TIMELINE_TIMEOUT`]: one
    /// slow request may use all of it, and the Attaching 503 retries share it). Set it after
    /// [`Self::with_schedule_retry`], which sets both budgets.
    pub fn with_timeline_retry_budget(mut self, budget: Duration) -> Self {
        self.timeline_retry_budget = budget;
        self
    }

    /// Runs `op` (one request per call) again while `retryable` holds for its error, with the
    /// backoff of [`Self::with_schedule_retry`], until the budget is spent. Dropping the
    /// future between attempts leaves nothing behind.
    async fn retry_transient<T, Fut>(
        &self,
        what: &str,
        tenant: &NeonId,
        retryable: fn(&StorconError) -> bool,
        budget: Duration,
        request_timeout: Duration,
        mut op: impl FnMut(Duration) -> Fut,
    ) -> Result<T, StorconError>
    where
        Fut: std::future::Future<Output = Result<T, StorconError>>,
    {
        let started = Instant::now();
        let mut attempt: u32 = 1;
        loop {
            let left = budget.saturating_sub(started.elapsed());
            let attempt_timeout = request_timeout.min(left.max(MIN_ATTEMPT_TIMEOUT));
            let err = match op(attempt_timeout).await {
                Err(e) if retryable(&e) => e,
                other => return other,
            };
            let elapsed = started.elapsed();
            if elapsed >= budget {
                if attempt == 1 {
                    return Err(err);
                }
                tracing::warn!(%tenant, attempt, ?elapsed, "{what}: giving up: {err}");
                return Err(err.exhausted(attempt, elapsed));
            }
            tracing::info!(%tenant, attempt, ?elapsed, "{what}: retrying: {err}");
            let shift = (attempt - 1).min(20);
            let ceiling = self
                .schedule_retry_base
                .saturating_mul(1 << shift)
                .min(self.schedule_retry_cap);
            let delay = (ceiling / 2 + jitter(ceiling / 2)).min(budget - elapsed);
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Creates the tenant; idempotent, so it is retried while the controller has no
    /// schedulable pageserver (see [`Self::with_schedule_retry`]).
    pub async fn create_tenant(&self, tenant: &NeonId) -> Result<(), StorconError> {
        let body = json!({
            "new_tenant_id": tenant,
            "placement_policy": { "Attached": self.tenant_secondaries },
        });
        let body = &body;
        self.retry_transient(
            "create tenant",
            tenant,
            |e| e.is_unschedulable() || e.is_request_timeout(),
            self.schedule_retry_budget,
            self.request_timeout,
            |timeout| async move {
                let resp = self
                    .http
                    .post(self.url("/v1/tenant"))
                    .timeout(timeout)
                    .json(&body)
                    .send()
                    .await?;
                check(resp).await.map(drop)
            },
        )
        .await
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
        // The Attaching 503 clears once the pageserver has loaded the tenant.
        let body = &body;
        self.retry_transient(
            "create timeline",
            tenant,
            StorconError::is_tenant_attaching,
            self.timeline_retry_budget,
            self.create_timeline_timeout,
            |timeout| async move {
                let resp = self
                    .http
                    .post(self.url(&format!("/v1/tenant/{tenant}/timeline")))
                    .timeout(timeout)
                    .json(&body)
                    .send()
                    .await?;
                Ok(check(resp).await?.json().await?)
            },
        )
        .await
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
        Ok(self
            .active_safekeeper_list()
            .await?
            .into_iter()
            .map(|sk| format!("{}:{}", sk.host, sk.port))
            .collect())
    }

    async fn active_safekeeper_list(&self) -> Result<Vec<Safekeeper>, StorconError> {
        let resp = self
            .http
            .get(self.url("/control/v1/safekeeper"))
            .send()
            .await?;
        let all: Vec<Safekeeper> = check(resp).await?.json().await?;
        Ok(all
            .into_iter()
            .filter(|sk| sk.scheduling_policy == "Active")
            .collect())
    }

    /// The ids of the active safekeepers (the same filter as [`Self::active_safekeepers`]).
    pub async fn active_safekeeper_ids(&self) -> Result<Vec<u64>, StorconError> {
        Ok(self
            .active_safekeeper_list()
            .await?
            .into_iter()
            .map(|sk| sk.id)
            .collect())
    }

    /// GET /v1/tenant/{t}/timeline/{tl}/get_lsn_by_timestamp?timestamp=<rfc3339>&with_lease=true
    pub async fn lsn_by_timestamp(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
        at: chrono::DateTime<chrono::Utc>,
    ) -> Result<LsnAtTime, StorconError> {
        #[derive(Deserialize)]
        struct Reply {
            lsn: Lsn,
            kind: LsnKind,
        }
        let ts = at.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true);
        let resp = self
            .http
            .get(self.url(&format!(
                "/v1/tenant/{tenant}/timeline/{timeline}/get_lsn_by_timestamp?timestamp={}&with_lease=true",
                urlencoding::encode(&ts)
            )))
            .send()
            .await?;
        let reply: Reply = check(resp).await?.json().await?;
        Ok(LsnAtTime {
            lsn: reply.lsn,
            kind: reply.kind,
        })
    }

    /// GET …/get_timestamp_of_lsn?lsn=<lsn>; None when no timestamp is known.
    pub async fn timestamp_of_lsn(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
        lsn: Lsn,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>, StorconError> {
        let resp = self
            .http
            .get(self.url(&format!(
                "/v1/tenant/{tenant}/timeline/{timeline}/get_timestamp_of_lsn?lsn={}",
                urlencoding::encode(&lsn.to_string())
            )))
            .send()
            .await?;
        // The pageserver answers 412 when it knows no timestamp for the LSN.
        if matches!(
            resp.status(),
            StatusCode::NOT_FOUND | StatusCode::PRECONDITION_FAILED
        ) {
            return Ok(None);
        }
        let text: String = check(resp).await?.json().await?;
        let parsed = chrono::DateTime::parse_from_rfc3339(&text)
            .map_err(|e| anyhow::anyhow!("timestamp {text:?}: {e}"))?;
        Ok(Some(parsed.with_timezone(&chrono::Utc)))
    }

    /// PUT /v1/tenant/{t}/timeline/{tl}/detach_ancestor?detach_behavior=multi_level_and_no_reparent
    pub async fn detach_ancestor(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<(), StorconError> {
        let resp = self
            .http
            .put(self.url(&format!(
                "/v1/tenant/{tenant}/timeline/{timeline}/detach_ancestor?detach_behavior=multi_level_and_no_reparent"
            )))
            .timeout(self.detach_timeout)
            .send()
            .await?;
        check(resp).await.map(drop)
    }

    /// PATCH /v1/tenant/config {"tenant_id": t, "pitr_interval": "<secs>s"}
    pub async fn patch_tenant_config(
        &self,
        tenant: &NeonId,
        pitr_interval: Duration,
    ) -> Result<(), StorconError> {
        let resp = self
            .http
            .patch(self.url("/v1/tenant/config"))
            .json(&json!({
                "tenant_id": tenant,
                "pitr_interval": format!("{}s", pitr_interval.as_secs()),
            }))
            .send()
            .await?;
        check(resp).await.map(drop)
    }

    /// The tenant's effective pitr_interval; None when it was never set.
    pub async fn tenant_pitr_interval(
        &self,
        tenant: &NeonId,
    ) -> Result<Option<Duration>, StorconError> {
        let resp = self
            .http
            .get(self.url(&format!("/v1/tenant/{tenant}/config")))
            .send()
            .await?;
        let body: serde_json::Value = check(resp).await?.json().await?;
        let Some(text) = body
            .pointer("/effective_config/pitr_interval")
            .and_then(|v| v.as_str())
        else {
            return Ok(None);
        };
        let parsed = humantime::parse_duration(text)
            .map_err(|e| anyhow::anyhow!("pitr_interval {text:?}: {e}"))?;
        Ok(Some(parsed))
    }

    /// GET /v1/tenant/{t}/timeline: every timeline id in the tenant.
    pub async fn list_timelines(&self, tenant: &NeonId) -> Result<Vec<NeonId>, StorconError> {
        #[derive(Deserialize)]
        struct Listed {
            timeline_id: String,
        }
        let resp = self
            .http
            .get(self.url(&format!("/v1/tenant/{tenant}/timeline")))
            .send()
            .await?;
        let listed: Vec<Listed> = check(resp).await?.json().await?;
        listed
            .into_iter()
            .map(|t| {
                t.timeline_id.parse::<NeonId>().map_err(|e| {
                    StorconError::Other(anyhow::anyhow!("timeline id {:?}: {e}", t.timeline_id))
                })
            })
            .collect()
    }

    /// Deletion is asynchronous: the storage controller answers 202 while it is in
    /// progress and 200 once the object is gone (404, or any other 2xx, also means gone). A
    /// 409 saying no pageserver is schedulable (right after a restart) counts as a 202.
    /// The whole wait is bounded by attempts × interval plus one request timeout, so a
    /// controller that answers 202 slowly can't stretch it out.
    async fn delete_until_gone(&self, url: String) -> Result<(), StorconError> {
        let mut last_conflict: Option<StorconError> = None;
        let deadline = Instant::now()
            + self.delete_poll_interval * self.delete_poll_attempts
            + self.request_timeout;
        for attempt in 1..=self.delete_poll_attempts {
            let resp = tokio::time::timeout_at(deadline, self.http.delete(&url).send())
                .await
                .map_err(|_| timed_out(&url, last_conflict.as_ref()))??;
            match resp.status() {
                StatusCode::ACCEPTED => {}
                StatusCode::NOT_FOUND => return Ok(()),
                status if status.is_success() => return Ok(()),
                _ => {
                    let err = tokio::time::timeout_at(deadline, check(resp))
                        .await
                        .map_err(|_| timed_out(&url, last_conflict.as_ref()))?
                        .expect_err("a non-success status is an error");
                    if !err.is_unschedulable() {
                        return Err(err);
                    }
                    tracing::info!(%url, attempt, "delete: no schedulable pageserver, asking again");
                    last_conflict = Some(err);
                }
            }
            if attempt < self.delete_poll_attempts {
                tokio::time::sleep_until(
                    (Instant::now() + self.delete_poll_interval).min(deadline),
                )
                .await;
            }
        }
        Err(timed_out(&url, last_conflict.as_ref()).into())
    }
}

/// The error of a delete that ran out of time, with the last "no schedulable pageserver" 409
/// it saw, if any.
fn timed_out(url: &str, last_conflict: Option<&StorconError>) -> anyhow::Error {
    match last_conflict {
        Some(e) => anyhow::anyhow!("timed out waiting for deletion of {url} (last answer: {e})"),
        None => anyhow::anyhow!("timed out waiting for deletion of {url}"),
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
        Err(StorconError::Other(ServerError { status, message }.into()))
    }
}

/// A uniformly random duration in `0..=ceiling`.
fn jitter(ceiling: Duration) -> Duration {
    let nanos = u64::try_from(ceiling.as_nanos()).unwrap_or(u64::MAX);
    if nanos == 0 {
        return Duration::ZERO;
    }
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).expect("OS random number generator");
    Duration::from_nanos(u64::from_le_bytes(bytes) % nanos.saturating_add(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_lsn_none_is_last_record() {
        assert_eq!(head_lsn(Lsn(0x100), None), Lsn(0x100));
    }

    #[test]
    fn head_lsn_lower_commit_is_last_record() {
        assert_eq!(head_lsn(Lsn(0x100), Some(Lsn(0x80))), Lsn(0x100));
    }

    #[test]
    fn head_lsn_higher_commit_wins() {
        assert_eq!(head_lsn(Lsn(0x100), Some(Lsn(0x180))), Lsn(0x180));
    }

    fn not_found(status: StatusCode, message: &str) -> StorconError {
        StorconError::Rejected {
            status,
            message: message.into(),
        }
    }

    #[test]
    fn only_the_pageservers_forwarded_tenant_404_is_pageserver_tenant_not_found() {
        let forwarded = "NotFound: 1 (pageserver): NotFound: tenant 7b1d";
        assert!(not_found(StatusCode::NOT_FOUND, forwarded).is_pageserver_tenant_not_found());
        let noted = format!("{forwarded} (after 3 attempts in 1.5s)");
        assert!(not_found(StatusCode::NOT_FOUND, &noted).is_pageserver_tenant_not_found());
        // Storcon's own 404, a timeline 404, other statuses, other variants.
        for (status, message) in [
            (StatusCode::NOT_FOUND, "NotFound: Tenant 7b1d not found"),
            (StatusCode::NOT_FOUND, "NotFound: 1 (pageserver): NotFound: timeline 9"),
            (StatusCode::CONFLICT, forwarded),
            (StatusCode::BAD_REQUEST, forwarded),
        ] {
            assert!(
                !not_found(status, message).is_pageserver_tenant_not_found(),
                "{status} {message}"
            );
        }
        assert!(!attaching().is_pageserver_tenant_not_found());
    }

    fn attaching() -> StorconError {
        StorconError::Other(
            ServerError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message:
                    "Timed out waiting 5s for tenant active state. Latest state: Some(Attaching)"
                        .into(),
            }
            .into(),
        )
    }

    #[test]
    fn a_5xx_keeps_its_text_and_variant() {
        let err = attaching();
        assert!(matches!(err, StorconError::Other(_)));
        assert!(
            err.to_string()
                .starts_with("storage controller returned 503 Service Unavailable: ")
        );
    }

    #[test]
    fn an_exhausted_error_keeps_its_class() {
        let err = attaching().exhausted(4, Duration::from_secs(30));
        assert!(err.is_tenant_attaching(), "{err}");
        assert!(
            err.to_string().ends_with("(after 4 attempts in 30.0s)"),
            "{err}"
        );
        let conflict = StorconError::Rejected {
            status: StatusCode::CONFLICT,
            message: "Conflict: x: No pageservers found".into(),
        }
        .exhausted(2, Duration::from_millis(1500));
        assert!(matches!(conflict, StorconError::Rejected { .. }));
        assert!(
            conflict
                .to_string()
                .contains("No pageservers found (after 2 attempts in 1.5s)")
        );
    }

    /// M2: the appended note must not change what the predicates see.
    #[test]
    fn an_exhausted_error_is_still_unschedulable() {
        for tail in ["No pageserver found matching constraint", "No pageservers found"] {
            let err = StorconError::Rejected {
                status: StatusCode::CONFLICT,
                message: format!("Conflict: Failed to schedule shard(s): {tail}"),
            };
            assert!(err.is_unschedulable());
            let err = err.exhausted(3, Duration::from_secs(2));
            assert!(err.is_unschedulable(), "{err}");
            assert!(err.to_string().ends_with("(after 3 attempts in 2.0s)"), "{err}");
        }
        // Other conflicts stay what they are.
        let other = StorconError::Rejected {
            status: StatusCode::CONFLICT,
            message: "Conflict: tenant already exists".into(),
        }
        .exhausted(3, Duration::from_secs(2));
        assert!(!other.is_unschedulable());
    }

    #[test]
    fn jitter_stays_within_the_ceiling() {
        assert_eq!(jitter(Duration::ZERO), Duration::ZERO);
        for _ in 0..1000 {
            assert!(jitter(Duration::from_millis(5)) <= Duration::from_millis(5));
        }
        assert!(jitter(Duration::MAX) <= Duration::from_nanos(u64::MAX));
    }

    /// I2/P6/H1 under a paused clock (no I/O, so no wall-clock flakiness): a retried error that
    /// takes a whole request timeout each time (a tenant create timing out against the
    /// `storcon_mock` test's server) stops at the budget, overrunning it by at most one
    /// attempt, after more than one attempt, each with the request timeout (below what is left
    /// of the budget), and the error says how many attempts it took.
    #[tokio::test(start_paused = true)]
    async fn retries_of_a_slow_failure_stop_at_the_budget() {
        let request_timeout = Duration::from_millis(50);
        let budget = Duration::from_millis(400);
        let client = StorconClient::new("http://unused.invalid", 1).with_schedule_retry(
            Duration::from_millis(1),
            Duration::from_millis(5),
            budget,
        );
        let tenant = NeonId::new();
        let started = Instant::now();
        let attempts = std::sync::Mutex::new(Vec::new());
        let err = client
            .retry_transient::<(), _>(
                "create tenant",
                &tenant,
                |_| true,
                budget,
                request_timeout,
                |timeout| {
                    attempts.lock().unwrap().push((started.elapsed(), timeout));
                    async move {
                        tokio::time::sleep(timeout).await;
                        Err(StorconError::Other(
                            ServerError {
                                status: StatusCode::GATEWAY_TIMEOUT,
                                message: "timed out".into(),
                            }
                            .into(),
                        ))
                    }
                },
            )
            .await
            .unwrap_err();
        let elapsed = started.elapsed();
        let attempts = attempts.into_inner().unwrap();
        assert!(attempts.len() > 1, "{attempts:?}");
        assert!(
            attempts.iter().all(|(_, t)| *t == request_timeout),
            "{attempts:?}"
        );
        assert!(
            attempts.iter().all(|(at, _)| *at <= budget),
            "no attempt starts past the budget: {attempts:?}"
        );
        assert!(elapsed >= budget, "{elapsed:?}");
        assert!(elapsed <= budget + request_timeout, "{elapsed:?}");
        let text = err.to_string();
        assert!(
            text.contains(&format!("(after {} attempts", attempts.len())),
            "{text}"
        );
    }
}
