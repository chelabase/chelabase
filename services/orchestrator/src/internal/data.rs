//! Routes the gateway and the REST service call: a key's hash to its branch and role, and a
//! branch's connection info. Neither serves data for a branch that isn't ready.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chelabase_metadata::BranchStatus;
use chelabase_neon::ComputeLocator;
use chelabase_secrets::{Purpose, open_credential};
use chelabase_types::BranchId;
use serde_json::json;
use tracing::{error, warn};
use uuid::Uuid;

use super::{InternalState, NeonMetadataPort};

/// How long the connection route waits for the compute's control-network address (SP11)
/// before answering with `replication_host: null`.
const CONTROL_LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// `{"key_id","branch_id","project_id","kind","role","databases","role_oid"}` (`databases` is null for every served
/// database, else the key's allow-list; `role_oid` the secret key's role identity, or null) for the live key with this hash (64 lowercase hex
/// characters); 404 for anything else. `last_used_at` is stamped after the answer is on its way.
pub(super) async fn key_lookup<M, L>(
    State(state): State<Arc<InternalState<M, L>>>,
    Path(hash): Path<String>,
) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    let Some(sha256) = parse_hash(&hash) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match state.metadata.lookup_data_key(&sha256).await {
        Ok(Some(key)) => {
            let touch = state.clone();
            tokio::spawn(async move {
                if let Err(err) = touch.metadata.touch_data_key(key.key_id).await {
                    warn!(key_id = %key.key_id, "touching a data key: {err}");
                }
            });
            Json(json!({
                "key_id": key.key_id,
                "branch_id": key.branch_id,
                "project_id": key.project_id,
                "kind": key.kind,
                "role": key.role,
                "databases": key.databases,
                "role_oid": key.role_oid,
            }))
            .into_response()
        }
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            error!("data key lookup: {err}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// 32 bytes from exactly 64 lowercase hex characters.
fn parse_hash(hash: &str) -> Option<[u8; 32]> {
    if hash.len() != 64
        || !hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    hex::decode(hash).ok()?.try_into().ok()
}

/// Where and as whom the REST service connects to a branch, and which databases it serves. 404 for an unknown or deleted
/// branch; 409 `{"status"}` when the branch isn't ready, has no endpoint, has no published
/// Postgres address or has no `authenticator` password yet.
pub(super) async fn branch_connection<M, L>(
    State(state): State<Arc<InternalState<M, L>>>,
    Path(id): Path<Uuid>,
) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    let branch = BranchId(id);
    let row = match state.metadata.branch_connection(branch).await {
        Ok(Some(row)) => row,
        Ok(None) => return StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            error!(branch_id = %branch, "branch connection: {err}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    if matches!(row.status, BranchStatus::Deleting | BranchStatus::Deleted) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let not_ready = || {
        (
            StatusCode::CONFLICT,
            Json(json!({ "status": row.status.as_str() })),
        )
            .into_response()
    };
    if row.status != BranchStatus::Ready {
        return not_ready();
    }
    let (Some(endpoint), Some(database), Some(sealed)) = (
        row.endpoint_id.as_deref(),
        row.database_name.as_deref(),
        row.authenticator_password_enc.as_deref(),
    ) else {
        return not_ready();
    };
    let addr = match state.locator.postgres_addr(endpoint).await {
        Ok(Some(addr)) => addr,
        Ok(None) => return not_ready(),
        Err(err) => {
            error!(branch_id = %branch, "locating the compute's Postgres: {err:#}");
            return not_ready();
        }
    };
    let password = match open_credential(
        &state.secrets,
        &branch.to_string(),
        Purpose::Authenticator,
        sealed,
    ) {
        Ok(password) => password,
        Err(err) => {
            error!(branch_id = %branch, "opening the authenticator password: {err}");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    // SP11: realtime's replication connection dials the control network (the role only logs in
    // there). Rest ignores it; a locator that finds nothing, fails or hangs (a stuck Docker
    // daemon: bounded by `CONTROL_LOOKUP_TIMEOUT`) must not change or hold up the answer.
    let replication_host = match tokio::time::timeout(
        CONTROL_LOOKUP_TIMEOUT,
        state.control_locator.address(endpoint),
    )
    .await
    {
        Ok(Ok(ip)) => ip.map(|ip| ip.to_string()),
        Ok(Err(err)) => {
            warn!(branch_id = %branch, "locating the compute on the control network: {err:#}");
            None
        }
        Err(_) => {
            warn!(
                branch_id = %branch,
                "locating the compute on the control network took over {}s; answering without it",
                CONTROL_LOOKUP_TIMEOUT.as_secs()
            );
            None
        }
    };
    let mut response = Json(json!({
        "host": addr.ip().to_string(),
        "port": addr.port(),
        "database": database,
        // U15: rest dials `host` and verifies the compute as this name, the only one its
        // certificate carries.
        "server_name": chelabase_neon::compute_tls::server_name(endpoint),
        "replication_host": replication_host,
        "user": "authenticator",
        "password": password,
        "status": row.status.as_str(),
        "settings": {
            "max_rows": row.settings.max_rows,
            "statement_timeout_ms": row.settings.statement_timeout_ms,
            "max_connections": row.settings.max_connections,
        },
        // The databases the data API serves, main first.
        "databases": row
            .databases
            .iter()
            .map(|d| json!({ "name": d.name, "default_schema": d.default_schema }))
            .collect::<Vec<_>>(),
    }))
    .into_response();
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        "no-store".parse().unwrap(),
    );
    response
}
