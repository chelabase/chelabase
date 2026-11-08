//! m11: the service runner's API as this service calls it (`CHELABASE_SERVICE_RUNNER_URL`,
//! its own bearer `CHELABASE_FUNCTIONS_SERVICE_RUNNER_TOKEN`), behind [`RunnerApi`] so the
//! rollover and the keeper are tested with a fake. Without the `services` profile the runner's
//! host doesn't resolve: every call is [`RunnerError::Unavailable`] (`runner_unavailable`).

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::Zeroizing;

/// Ensure may pull an image first.
const ENSURE_TIMEOUT: Duration = Duration::from_secs(660);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunnerError {
    /// Not configured, unreachable, or the engine failed (5xx).
    #[error("the service runner is unavailable: {0}")]
    Unavailable(String),
    /// It refused the spec, the image or the env (422): the message says why.
    #[error("{0}")]
    Refused(String),
    #[error("no such container")]
    NotFound,
}

/// A container as the runner reports it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ContainerStatus {
    pub id: String,
    pub state: String,
    pub deployment_id: Option<String>,
    pub port: Option<u16>,
    /// Its address on its service's network: the one place this service dials it.
    pub address: Option<String>,
}

/// What ensure takes: the env is the branch's secrets (wiped on drop).
pub struct Ensure<'a> {
    pub service_id: Uuid,
    pub branch_id: Uuid,
    pub deployment_id: Uuid,
    pub spec: &'a serde_json::Value,
    pub env: &'a BTreeMap<String, Zeroizing<String>>,
}

#[async_trait::async_trait]
pub trait RunnerApi: Send + Sync + 'static {
    async fn ensure(&self, req: &Ensure<'_>) -> Result<ContainerStatus, RunnerError>;
    async fn remove(&self, container: &str) -> Result<(), RunnerError>;
    async fn logs(&self, container: &str) -> Result<String, RunnerError>;
    async fn resolve_image(&self, image: &str) -> Result<String, RunnerError>;
}

/// The runner over HTTP.
pub struct HttpRunner {
    /// `None` (no URL or no bearer): every call is unavailable.
    target: Option<(String, Zeroizing<String>)>,
    http: chelabase_http::Client,
}

#[derive(Serialize)]
struct EnsureBody<'a> {
    service_id: Uuid,
    branch_id: Uuid,
    deployment_id: Uuid,
    spec: &'a serde_json::Value,
    env: BTreeMap<&'a str, &'a str>,
}

impl HttpRunner {
    pub fn new(url: Option<String>, token: Option<String>) -> reqwest::Result<Self> {
        Ok(Self {
            target: url
                .zip(token)
                .map(|(u, t)| (u.trim_end_matches('/').to_owned(), Zeroizing::new(t))),
            http: chelabase_http::client_builder()
                .connect_timeout(Duration::from_secs(5))
                .build()?,
        })
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<reqwest::Response, RunnerError> {
        let Some((url, token)) = &self.target else {
            return Err(RunnerError::Unavailable(
                "CHELABASE_SERVICE_RUNNER_URL or its bearer is unset".into(),
            ));
        };
        let mut req = self
            .http
            .request(method, format!("{url}{path}"))
            .bearer_auth(token.as_str())
            .timeout(timeout);
        if let Some(body) = body {
            req = req
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| RunnerError::Unavailable(e.without_url().to_string()))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(resp);
        }
        let message = resp
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(str::to_owned))
            .unwrap_or_else(|| status.to_string());
        Err(match status.as_u16() {
            404 => RunnerError::NotFound,
            422 => RunnerError::Refused(message),
            _ => RunnerError::Unavailable(format!("{status}: {message}")),
        })
    }
}

#[async_trait::async_trait]
impl RunnerApi for HttpRunner {
    async fn ensure(&self, req: &Ensure<'_>) -> Result<ContainerStatus, RunnerError> {
        let body = EnsureBody {
            service_id: req.service_id,
            branch_id: req.branch_id,
            deployment_id: req.deployment_id,
            spec: req.spec,
            env: req
                .env
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect(),
        };
        let json = serde_json::to_vec(&body).expect("strings serialise");
        let resp = self
            .send(
                reqwest::Method::POST,
                "/internal/containers/ensure",
                Some(json),
                ENSURE_TIMEOUT,
            )
            .await?;
        resp.json()
            .await
            .map_err(|e| RunnerError::Unavailable(format!("an unreadable answer: {e}")))
    }

    async fn remove(&self, container: &str) -> Result<(), RunnerError> {
        self.send(
            reqwest::Method::POST,
            "/internal/containers/remove",
            Some(
                serde_json::json!({ "container": container })
                    .to_string()
                    .into_bytes(),
            ),
            CALL_TIMEOUT,
        )
        .await
        .map(drop)
    }

    async fn logs(&self, container: &str) -> Result<String, RunnerError> {
        let resp = self
            .send(
                reqwest::Method::GET,
                &format!("/internal/containers/{container}/logs"),
                None,
                CALL_TIMEOUT,
            )
            .await?;
        resp.text()
            .await
            .map_err(|e| RunnerError::Unavailable(format!("an unreadable answer: {e}")))
    }

    async fn resolve_image(&self, image: &str) -> Result<String, RunnerError> {
        let resp = self
            .send(
                reqwest::Method::POST,
                "/internal/images/resolve",
                Some(
                    serde_json::json!({ "image": image })
                        .to_string()
                        .into_bytes(),
                ),
                ENSURE_TIMEOUT,
            )
            .await?;
        let v: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| RunnerError::Unavailable(format!("an unreadable answer: {e}")))?;
        v["image"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| RunnerError::Unavailable("an answer without an image".into()))
    }
}
