//! Hooks the storage controller calls when storage placement changes
//! (`PUT {control-plane-url}/notify-attach` and `/notify-safekeepers`, neon
//! `storage_controller/src/compute_hook.rs`). Each running compute on the tenant or timeline
//! gets a fresh spec. The storage controller retries 503 and treats 400/401/403 as fatal, so
//! our own failures are always 503, an unparseable body included (the `Json` extractor would
//! answer 400/415/422).

use super::spec::endpoint_config;
use super::{InternalState, NeonMetadataPort};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use chelabase_metadata::MetadataError;
use chelabase_neon::ComputeLocator;
use chelabase_neon::reconfigure;
use serde::Deserialize;
use std::sync::Arc;
use tracing::{error, info, warn};

/// Other fields (`shards`, `preferred_az`, `stripe_size`) aren't needed.
#[derive(Debug, Deserialize)]
pub struct NotifyAttach {
    tenant_id: String,
}

/// Other fields (`generation`, `safekeepers`) aren't needed: the spec lists the safekeepers.
#[derive(Debug, Deserialize)]
pub struct NotifySafekeepers {
    tenant_id: String,
    timeline_id: String,
}

fn parse<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, StatusCode> {
    serde_json::from_slice(body).map_err(|e| {
        error!("unparseable hook body: {e}");
        StatusCode::SERVICE_UNAVAILABLE
    })
}

pub async fn notify_attach<M: NeonMetadataPort, L: ComputeLocator>(
    State(state): State<Arc<InternalState<M, L>>>,
    body: Bytes,
) -> StatusCode {
    let req: NotifyAttach = match parse(&body) {
        Ok(r) => r,
        Err(code) => return code,
    };
    info!(tenant = %req.tenant_id, "notify-attach");
    let endpoints = state
        .metadata
        .neon_endpoints_for_tenant(&req.tenant_id)
        .await;
    reconfigure_all(&state, endpoints).await
}

pub async fn notify_safekeepers<M: NeonMetadataPort, L: ComputeLocator>(
    State(state): State<Arc<InternalState<M, L>>>,
    body: Bytes,
) -> StatusCode {
    let req: NotifySafekeepers = match parse(&body) {
        Ok(r) => r,
        Err(code) => return code,
    };
    info!(tenant = %req.tenant_id, timeline = %req.timeline_id, "notify-safekeepers");
    let endpoints = state
        .metadata
        .neon_endpoints_for_timeline(&req.timeline_id)
        .await;
    reconfigure_all(&state, endpoints).await
}

/// Every endpoint is tried, even after one fails.
async fn reconfigure_all<M: NeonMetadataPort, L: ComputeLocator>(
    state: &InternalState<M, L>,
    endpoints: Result<Vec<String>, MetadataError>,
) -> StatusCode {
    let endpoints = match endpoints {
        Ok(ids) => ids,
        Err(e) => {
            error!("looking up endpoints: {e}");
            return StatusCode::SERVICE_UNAVAILABLE;
        }
    };
    let mut ok = true;
    for id in endpoints {
        if let Err(e) = reconfigure_endpoint(state, &id).await {
            warn!(endpoint = %id, "reconfigure failed; the storage controller will retry: {e:#}");
            ok = false;
        }
    }
    if ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

/// Push the current spec to a running compute. A compute that isn't running pulls the current
/// spec when it boots, so there's nothing to do for it.
async fn reconfigure_endpoint<M: NeonMetadataPort, L: ComputeLocator>(
    state: &InternalState<M, L>,
    endpoint_id: &str,
) -> anyhow::Result<()> {
    let Some(control_addr) = state.locator.control_addr(endpoint_id).await? else {
        info!(endpoint = %endpoint_id, "compute not running; skipping reconfigure");
        return Ok(());
    };
    let Some(endpoint) = state.metadata.neon_endpoint(endpoint_id).await? else {
        // Deleted between the list lookup and now: nothing to reconfigure, nothing to retry.
        info!(endpoint = %endpoint_id, "endpoint deleted; skipping reconfigure");
        return Ok(());
    };
    let config = endpoint_config(state, &endpoint).await?;
    reconfigure::push(
        &state.http,
        control_addr,
        &state.signer,
        endpoint_id,
        &config,
    )
    .await?;
    info!(endpoint = %endpoint_id, "reconfigured compute");
    Ok(())
}
