//! Pushing a new spec to a running compute: `POST http://<control_addr>/configure`
//! (neon `compute_tools/src/http/routes/configure.rs`), with a [`ComputeSigner`] token only
//! that compute accepts.

use std::net::SocketAddr;

use anyhow::Context;
use serde_json::{Value, json};

use crate::jwt::ComputeSigner;

/// The `/configure` body (`ConfigurationRequest`) from a [`crate::spec::config_response`].
pub fn configure_body(config_response: &Value) -> Value {
    json!({
        "spec": config_response["spec"],
        "compute_ctl_config": config_response["compute_ctl_config"],
    })
}

/// compute_ctl answers once the compute has applied the spec. Anything but 2xx is an error:
/// for example 412 while the compute is still starting.
pub async fn push(
    http: &reqwest::Client,
    control_addr: SocketAddr,
    signer: &ComputeSigner,
    endpoint_id: &str,
    config_response: &Value,
) -> anyhow::Result<()> {
    let resp = http
        .post(format!("http://{control_addr}/configure"))
        .bearer_auth(signer.token_for(endpoint_id)?)
        .json(&configure_body(config_response))
        .send()
        .await
        .with_context(|| format!("POST /configure to compute {endpoint_id}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("compute {endpoint_id} /configure returned {status}: {text}");
    }
    Ok(())
}

/// compute_ctl's own view of the compute (`GET /status`, `ComputeStatusResponse.status`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComputeCtlStatus {
    /// Postgres is up and compute_ctl finished starting it (migrations included).
    Running,
    /// The start failed; compute_ctl's `error`.
    Failed(String),
    /// Any other state (`init`, `configuration_pending`, …), as compute_ctl names it.
    Other(String),
}

#[derive(serde::Deserialize)]
struct StatusBody {
    status: String,
    #[serde(default)]
    error: Option<String>,
}

/// compute_ctl's status of `endpoint_id`'s compute, with the token [`push`] uses. An
/// unreachable compute_ctl or an answer other than 2xx is an error.
pub async fn status(
    http: &reqwest::Client,
    control_addr: SocketAddr,
    signer: &ComputeSigner,
    endpoint_id: &str,
) -> anyhow::Result<ComputeCtlStatus> {
    let resp = http
        .get(format!("http://{control_addr}/status"))
        .bearer_auth(signer.token_for(endpoint_id)?)
        .send()
        .await
        .with_context(|| format!("GET /status of compute {endpoint_id}"))?;
    let code = resp.status();
    if !code.is_success() {
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("compute {endpoint_id} /status returned {code}: {text}");
    }
    let body: StatusBody = resp
        .json()
        .await
        .with_context(|| format!("reading the /status of compute {endpoint_id}"))?;
    Ok(match body.status.as_str() {
        "running" => ComputeCtlStatus::Running,
        "failed" => ComputeCtlStatus::Failed(
            body.error
                .unwrap_or_else(|| "compute_ctl reported failed without an error".to_string()),
        ),
        _ => ComputeCtlStatus::Other(body.status),
    })
}
