//! The orchestrator's `/internal/realtime/*` routes (P13, S13, S14, S15), at
//! `CHELABASE_ORCHESTRATOR_URL` with `Authorization: Bearer <CHELABASE_REALTIME_INTERNAL_TOKEN>`.
//! `/internal/data/*` (T8) goes to `CHELABASE_INTERNAL_URL` with the same token.
//!
//! - `POST /internal/realtime/authorize {token, target: {database_id, branch_id, pg_database,
//!   role}}` (`role: null` = the owner) → 200 `{allowed: true, tenant_id, member: {sub, name,
//!   email}}` or 200 `{allowed: false, reason: forbidden|not_found|token_expired|unavailable}`.
//! - `POST /internal/realtime/projects {token}` → 200 `{tenant_id, projects: [{project_id,
//!   database_ids}]}`; refusals as 403 `forbidden`, 404 `not_found`, 401 with body
//!   `{"error":{"code":"token_expired"}}`, 503 `unavailable`; a bare 401 is a wrong bearer.
//! - `POST /internal/realtime/member {sub, tenant_id}` (T8-1, D7, #119) → 200 `{allowed}`, asked before
//!   a Studio ticket is issued; anything else is "can't answer" (the ticket route's 503).
//! - S13: 400, 401 (other than `token_expired` on `/projects`), 5xx, a body that isn't the
//!   route's JSON and a timeout are `unavailable` (400/401 logged as configuration errors).
//!   An unknown refusal reason is `forbidden` (fails closed).

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::project::ProjectView;

/// S14: joins and re-authorizations.
pub const AUTHORIZE_TIMEOUT: Duration = Duration::from_secs(20);
/// S14: `/projects`.
pub const PROJECTS_TIMEOUT: Duration = Duration::from_secs(20);
/// S14: `/visible` (T8).
pub const VISIBLE_TIMEOUT: Duration = Duration::from_secs(5);

/// A table target, as `/authorize` takes it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AuthTarget {
    pub database_id: Uuid,
    pub branch_id: Uuid,
    pub pg_database: String,
    /// Sent as `null` for the owner.
    pub role: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberInfo {
    pub sub: String,
    pub name: Option<String>,
    pub email: Option<String>,
}

/// `/authorize`'s answer (P13, S13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthorizeOutcome {
    Allowed {
        tenant_id: Uuid,
        member: MemberInfo,
    },
    /// Leaves the channel.
    Forbidden,
    /// Leaves the channel.
    NotFound,
    /// Waits for the renewal (4001 at `exp`).
    TokenExpired,
    /// Degrades the channel; three in a row leave it with `unavailable`.
    Unavailable,
}

#[derive(Deserialize)]
struct AuthorizeBody {
    allowed: bool,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    tenant_id: Option<Uuid>,
    #[serde(default)]
    member: Option<MemberInfo>,
}

/// `/authorize`'s HTTP answer as an outcome (S13).
pub fn authorize_outcome(status: u16, body: &[u8]) -> AuthorizeOutcome {
    if status != 200 {
        return AuthorizeOutcome::Unavailable;
    }
    let Ok(body) = serde_json::from_slice::<AuthorizeBody>(body) else {
        return AuthorizeOutcome::Unavailable;
    };
    if body.allowed {
        return match (body.tenant_id, body.member) {
            (Some(tenant_id), Some(member)) => AuthorizeOutcome::Allowed { tenant_id, member },
            // `allowed` without its tenant or member isn't the route's answer.
            _ => AuthorizeOutcome::Unavailable,
        };
    }
    match body.reason.as_deref() {
        Some("not_found") => AuthorizeOutcome::NotFound,
        Some("token_expired") => AuthorizeOutcome::TokenExpired,
        Some("unavailable") => AuthorizeOutcome::Unavailable,
        // `forbidden`, a reason this build doesn't know, or none: fail closed.
        _ => AuthorizeOutcome::Forbidden,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectsError {
    /// 403 or 404: the person sees no projects (not an active member).
    Forbidden,
    TokenExpired,
    Unavailable,
}

/// `/projects`' HTTP answer.
pub fn projects_result(status: u16, body: &[u8]) -> Result<ProjectView, ProjectsError> {
    match status {
        200 => ProjectView::from_json(body).map_err(|_| ProjectsError::Unavailable),
        403 | 404 => Err(ProjectsError::Forbidden),
        401 => {
            let code = serde_json::from_slice::<Value>(body)
                .ok()
                .and_then(|v| v["error"]["code"].as_str().map(str::to_string));
            if code.as_deref() == Some("token_expired") {
                Err(ProjectsError::TokenExpired)
            } else {
                Err(ProjectsError::Unavailable)
            }
        }
        _ => Err(ProjectsError::Unavailable),
    }
}

#[derive(Clone)]
pub struct OrchestratorClient {
    http: chelabase_http::Client,
    base_url: String,
    internal_url: String,
    token: String,
    authorize_timeout: Duration,
    projects_timeout: Duration,
}

impl std::fmt::Debug for OrchestratorClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OrchestratorClient")
            .field("base_url", &self.base_url)
            .field("internal_url", &self.internal_url)
            .finish_non_exhaustive()
    }
}

/// An HTTP answer: (status, body), or `None` when there was none in time.
async fn post(
    http: &chelabase_http::Client,
    url: String,
    token: &str,
    body: Value,
    timeout: Duration,
) -> Option<(u16, Vec<u8>)> {
    let call = async {
        let resp = http
            .post(&url)
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .ok()?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.ok()?;
        Some((status, bytes.to_vec()))
    };
    tokio::time::timeout(timeout, call).await.ok().flatten()
}

/// S13: a 400 or a bare 401 means realtime itself is misconfigured (its bearer, its JSON).
fn log_configuration_error(route: &str, status: u16) {
    if status == 400 || status == 401 {
        tracing::error!(
            route,
            status,
            "the orchestrator refused realtime's request: check CHELABASE_REALTIME_INTERNAL_TOKEN on both sides"
        );
    }
}

impl OrchestratorClient {
    /// `base_url` = `CHELABASE_ORCHESTRATOR_URL`, `internal_url` = `CHELABASE_INTERNAL_URL`.
    pub fn new(base_url: &str, internal_url: &str, token: &str) -> Self {
        Self {
            // Never follow a redirect: the bearer goes to the configured host only.
            http: chelabase_http::client_builder()
                .build()
                .expect("realtime's HTTP client builds"),
            base_url: base_url.trim_end_matches('/').to_string(),
            internal_url: internal_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            authorize_timeout: AUTHORIZE_TIMEOUT,
            projects_timeout: PROJECTS_TIMEOUT,
        }
    }
    /// Other timeouts than S14's (tests).
    pub fn with_timeouts(mut self, authorize: Duration, projects: Duration) -> Self {
        self.authorize_timeout = authorize;
        self.projects_timeout = projects;
        self
    }
    /// `CHELABASE_INTERNAL_URL` (T8's `/internal/data/*`).
    pub fn internal_url(&self) -> &str {
        &self.internal_url
    }
    pub async fn authorize(&self, member_token: &str, target: &AuthTarget) -> AuthorizeOutcome {
        let body = json!({"token": member_token, "target": target});
        match post(
            &self.http,
            format!("{}/internal/realtime/authorize", self.base_url),
            &self.token,
            body,
            self.authorize_timeout,
        )
        .await
        {
            None => {
                tracing::warn!("/internal/realtime/authorize gave no answer in time");
                AuthorizeOutcome::Unavailable
            }
            Some((status, bytes)) => {
                log_configuration_error("/internal/realtime/authorize", status);
                authorize_outcome(status, &bytes)
            }
        }
    }
    /// T8-1, D7: whether the person may get a Studio ticket now (`/internal/realtime/member`).
    /// `Some(false)` for a refusal (any `allowed: false`, fails closed on an unknown reason);
    /// `None` when the orchestrator gives no usable answer in time.
    /// #119: the member token's identity sends its `tenant_id`; one without a tenant is
    /// refused here without asking.
    pub async fn member_allowed(&self, sub: &str, tenant_id: Option<uuid::Uuid>) -> Option<bool> {
        let Some(tenant) = tenant_id else {
            return Some(false);
        };
        let body = json!({"sub": sub, "tenant_id": tenant});
        match post(
            &self.http,
            format!("{}/internal/realtime/member", self.base_url),
            &self.token,
            body,
            self.authorize_timeout,
        )
        .await
        {
            None => {
                tracing::warn!("/internal/realtime/member gave no answer in time");
                None
            }
            Some((200, bytes)) => serde_json::from_slice::<Value>(&bytes)
                .ok()
                .and_then(|v| v["allowed"].as_bool()),
            Some((status, _)) => {
                log_configuration_error("/internal/realtime/member", status);
                None
            }
        }
    }
    pub async fn projects(&self, member_token: &str) -> Result<ProjectView, ProjectsError> {
        let body = json!({"token": member_token});
        match post(
            &self.http,
            format!("{}/internal/realtime/projects", self.base_url),
            &self.token,
            body,
            self.projects_timeout,
        )
        .await
        {
            None => {
                tracing::warn!("/internal/realtime/projects gave no answer in time");
                Err(ProjectsError::Unavailable)
            }
            Some((status, bytes)) => {
                let result = projects_result(status, &bytes);
                if result != Err(ProjectsError::TokenExpired) {
                    log_configuration_error("/internal/realtime/projects", status);
                }
                result
            }
        }
    }
}
