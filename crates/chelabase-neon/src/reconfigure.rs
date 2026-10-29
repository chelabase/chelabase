//! Pushing a new spec to a running compute: `POST http://<control_addr>/configure`
//! (neon `compute_tools/src/http/routes/configure.rs`), with a [`ComputeSigner`] token only
//! that compute accepts. Also its status (`GET /status`) and a new storage token
//! (`POST /storage_token`).

use std::net::SocketAddr;

use anyhow::Context;
use serde_json::{Value, json};

use crate::jwt::ComputeSigner;

/// The most bytes of compute_ctl's answer an error of [`push`], [`status_report`] or
/// [`push_storage_token`] carries: whatever answers at a compute's address can send any amount
/// of text, and our errors end up in logs and job rows.
pub const ERROR_BODY_LIMIT: usize = 512;

/// At most [`ERROR_BODY_LIMIT`] bytes of a failed answer's body, cut on a character boundary;
/// the rest is never read. A body that can't be read is empty.
async fn error_body(mut resp: reqwest::Response) -> String {
    // A character starting before the limit ends within three more bytes.
    let want = ERROR_BODY_LIMIT + 3;
    let mut bytes = Vec::with_capacity(want);
    while bytes.len() < want {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let take = chunk.len().min(want - bytes.len());
                bytes.extend_from_slice(&chunk[..take]);
            }
            Ok(None) | Err(_) => break,
        }
    }
    // Control characters (newlines, escapes, NUL) are escaped, so the body can't forge log
    // lines or drive a terminal; the limit counts the escaped text and never cuts an escape.
    let mut text = String::with_capacity(ERROR_BODY_LIMIT);
    for c in String::from_utf8_lossy(&bytes).chars() {
        let unit = if c.is_control() {
            c.escape_default().to_string()
        } else {
            c.to_string()
        };
        if text.len() + unit.len() > ERROR_BODY_LIMIT {
            break;
        }
        text.push_str(&unit);
    }
    text
}

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
    http: &chelabase_http::Client,
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
        let text = error_body(resp).await;
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
    /// What compute_ctl can do (`ComputeStatusResponse.capabilities`); absent on older images,
    /// and `null` (an empty list serialised as null) is none either.
    #[serde(default)]
    capabilities: Option<Vec<String>>,
}

/// The `Compute-Capabilities` header compute_ctl sends with its control-plane requests.
pub const CAPABILITIES_HEADER: &str = "Compute-Capabilities";
/// The capability of accepting a new storage token through `POST /storage_token`.
pub const STORAGE_TOKEN_REFRESH: &str = "storage_token_refresh";

/// Whether the (joined) values of the [`CAPABILITIES_HEADER`] list [`STORAGE_TOKEN_REFRESH`]:
/// split on `,`, trimmed, compared exactly (case-sensitive). No header is "not capable".
pub fn advertises_storage_token_refresh(header: Option<&str>) -> bool {
    header.is_some_and(|h| h.split(',').any(|c| c.trim() == STORAGE_TOKEN_REFRESH))
}

/// compute_ctl's status and what it advertises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComputeCtlReport {
    pub status: ComputeCtlStatus,
    /// The compute accepts `POST /storage_token`.
    pub storage_token_refresh: bool,
}

/// compute_ctl's status of `endpoint_id`'s compute, with the token [`push`] uses. An
/// unreachable compute_ctl or an answer other than 2xx is an error.
pub async fn status(
    http: &chelabase_http::Client,
    control_addr: SocketAddr,
    signer: &ComputeSigner,
    endpoint_id: &str,
) -> anyhow::Result<ComputeCtlStatus> {
    Ok(status_report(http, control_addr, signer, endpoint_id)
        .await?
        .status)
}

/// [`status`] plus the capabilities compute_ctl advertises. An old compute_ctl that sends none
/// reports `storage_token_refresh: false`; an unreachable one or a body that isn't a status is
/// an error, never "no capability".
pub async fn status_report(
    http: &chelabase_http::Client,
    control_addr: SocketAddr,
    signer: &ComputeSigner,
    endpoint_id: &str,
) -> anyhow::Result<ComputeCtlReport> {
    let resp = http
        .get(format!("http://{control_addr}/status"))
        .bearer_auth(signer.token_for(endpoint_id)?)
        .send()
        .await
        .with_context(|| format!("GET /status of compute {endpoint_id}"))?;
    let code = resp.status();
    if !code.is_success() {
        let text = error_body(resp).await;
        anyhow::bail!("compute {endpoint_id} /status returned {code}: {text}");
    }
    let body: StatusBody = resp
        .json()
        .await
        .with_context(|| format!("reading the /status of compute {endpoint_id}"))?;
    let storage_token_refresh = body
        .capabilities
        .iter()
        .flatten()
        .any(|c| c == STORAGE_TOKEN_REFRESH);
    let status = match body.status.as_str() {
        "running" => ComputeCtlStatus::Running,
        "failed" => ComputeCtlStatus::Failed(
            body.error
                .unwrap_or_else(|| "compute_ctl reported failed without an error".to_string()),
        ),
        _ => ComputeCtlStatus::Other(body.status),
    };
    Ok(ComputeCtlReport {
        status,
        storage_token_refresh,
    })
}

/// Hands a running compute a new storage token (`POST /storage_token`, body `{"token": …}`),
/// with the same bearer [`push`] uses. Any 2xx is success; otherwise the error carries the
/// status and compute_ctl's body, never the token.
pub async fn push_storage_token(
    http: &chelabase_http::Client,
    control_addr: SocketAddr,
    signer: &ComputeSigner,
    endpoint_id: &str,
    token: &str,
) -> anyhow::Result<()> {
    let resp = http
        .post(format!("http://{control_addr}/storage_token"))
        .bearer_auth(signer.token_for(endpoint_id)?)
        .json(&json!({ "token": token }))
        .send()
        .await
        .map_err(|e| e.without_url())
        .with_context(|| format!("POST /storage_token to compute {endpoint_id}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = error_body(resp).await;
        anyhow::bail!("compute {endpoint_id} /storage_token returned {status}: {text}");
    }
    Ok(())
}
