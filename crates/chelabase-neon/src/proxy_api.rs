//! Bodies for the control-plane API that Neon's proxy calls with `--auth-backend=control-plane`.
//! The shapes follow neon `proxy/src/control_plane/messages.rs` at `infra/neon/NEON_REF`:
//! `GetEndpointAccessControl`, `WakeCompute` and `ControlPlaneErrorMessage`.

use std::net::{IpAddr, SocketAddr};

use serde_json::{Value, json};

use crate::compute::POSTGRES_PORT;

/// `reason` values we send; the proxy reads any value it doesn't know as `UNKNOWN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    EndpointNotFound,
    Unknown,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EndpointNotFound => "ENDPOINT_NOT_FOUND",
            Self::Unknown => "UNKNOWN",
        }
    }
}

/// `GET get_endpoint_access_control`. With an empty `role_secret`, the proxy fails the login
/// exactly as it does for a wrong password, so role names can't be probed.
pub fn access_control(project_id: &str, role_secret: &str) -> Value {
    json!({
        "role_secret": role_secret,
        "project_id": project_id,
        "account_id": null,
        "allowed_ips": null,
        "allowed_vpc_endpoint_ids": null,
        "block_public_connections": null,
        "block_vpc_connections": null,
    })
}

/// `GET wake_compute`. Until step 4 has real ids, `project_id` carries the endpoint id and
/// `branch_id` the timeline id.
pub fn wake_compute(ip: IpAddr, endpoint_id: &str, timeline_id: &str) -> Value {
    json!({
        "address": SocketAddr::new(ip, POSTGRES_PORT).to_string(),
        "server_name": null,
        "aux": {
            "endpoint_id": endpoint_id,
            "project_id": endpoint_id,
            "branch_id": timeline_id,
            "compute_id": endpoint_id,
            "cold_start_info": "warm",
        },
    })
}

/// An error body. With `retry_ms`, the proxy retries after that delay instead of failing
/// the connection.
pub fn error(reason: Reason, message: &str, retry_ms: Option<u64>) -> Value {
    json!({
        "error": message,
        "status": {
            "code": reason.as_str(),
            "message": message,
            "details": {
                "error_info": { "reason": reason.as_str() },
                "retry_info": retry_ms.map(|ms| json!({ "retry_delay_ms": ms })),
                "user_facing_message": { "message": message },
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_control_carries_the_fields_the_proxy_reads() {
        let v = access_control("ep-a", "SCRAM-SHA-256$x");
        assert_eq!(v["role_secret"], "SCRAM-SHA-256$x");
        assert_eq!(v["project_id"], "ep-a");
        for key in [
            "account_id",
            "allowed_ips",
            "allowed_vpc_endpoint_ids",
            "block_public_connections",
            "block_vpc_connections",
        ] {
            assert!(v[key].is_null(), "{key}");
            assert!(v.as_object().unwrap().contains_key(key), "{key} present");
        }
    }

    #[test]
    fn wake_compute_points_at_postgres_on_the_compute() {
        let v = wake_compute("172.20.0.9".parse().unwrap(), "ep-a", "0123");
        assert_eq!(v["address"], "172.20.0.9:5432");
        assert!(v["server_name"].is_null());
        assert_eq!(v["aux"]["endpoint_id"], "ep-a");
        assert_eq!(v["aux"]["project_id"], "ep-a");
        assert_eq!(v["aux"]["branch_id"], "0123");
        assert_eq!(v["aux"]["compute_id"], "ep-a");
        assert_eq!(v["aux"]["cold_start_info"], "warm");
    }

    #[test]
    fn errors_carry_a_reason_and_an_optional_retry() {
        let v = error(Reason::Unknown, "compute is starting", Some(1000));
        assert_eq!(v["error"], "compute is starting");
        assert_eq!(v["status"]["details"]["error_info"]["reason"], "UNKNOWN");
        assert_eq!(v["status"]["details"]["retry_info"]["retry_delay_ms"], 1000);
        assert_eq!(
            v["status"]["details"]["user_facing_message"]["message"],
            "compute is starting"
        );
        assert!(v["status"]["code"].is_string());
        assert!(v["status"]["message"].is_string());

        let v = error(Reason::EndpointNotFound, "gone", None);
        assert_eq!(
            v["status"]["details"]["error_info"]["reason"],
            "ENDPOINT_NOT_FOUND"
        );
        assert!(v["status"]["details"]["retry_info"].is_null());
    }
}
