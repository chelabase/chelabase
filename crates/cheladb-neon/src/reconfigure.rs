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
