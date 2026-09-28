//! The compute spec served to compute_ctl (`GET /compute/api/v2/computes/{id}/spec`).
//!
//! Field names follow neon's `compute_api::spec::ComputeSpec` and
//! `compute_api::responses::ControlPlaneConfigResponse`.

use serde_json::{Value, json};

use crate::id::NeonId;

pub struct RoleSpec {
    pub name: String,
    pub scram_verifier: String,
}

pub struct DatabaseSpec {
    pub name: String,
    pub owner: String,
}

pub struct SpecInputs<'a> {
    /// Our endpoint id; compute_ctl uses it as the cluster id and name.
    pub endpoint_id: &'a str,
    pub tenant_id: &'a NeonId,
    pub timeline_id: &'a NeonId,
    /// `postgresql://no_user@host:port` of the attached pageserver.
    pub pageserver_connstring: String,
    /// `host:port` of each safekeeper.
    pub safekeeper_connstrings: Vec<String>,
    pub roles: Vec<RoleSpec>,
    pub databases: Vec<DatabaseSpec>,
    /// Keys compute_ctl accepts control-plane tokens from.
    pub jwks: Value,
}

fn setting(name: &str, value: &str, vartype: &str) -> Value {
    json!({ "name": name, "value": value, "vartype": vartype })
}

/// `{spec, status, compute_ctl_config}` for an attached compute.
pub fn config_response(inputs: &SpecInputs<'_>) -> Value {
    let roles: Vec<Value> = inputs
        .roles
        .iter()
        .map(|r| json!({ "name": r.name, "encrypted_password": r.scram_verifier, "options": null }))
        .collect();
    let databases: Vec<Value> = inputs
        .databases
        .iter()
        .map(|d| json!({ "name": d.name, "owner": d.owner, "options": null }))
        .collect();
    json!({
        "status": "attached",
        "spec": {
            "format_version": 1.0,
            "suspend_timeout_seconds": -1,
            "tenant_id": inputs.tenant_id,
            "timeline_id": inputs.timeline_id,
            "pageserver_connstring": inputs.pageserver_connstring,
            "safekeeper_connstrings": inputs.safekeeper_connstrings,
            "cluster": {
                "cluster_id": inputs.endpoint_id,
                "name": inputs.endpoint_id,
                "state": "restarted",
                "roles": roles,
                "databases": databases,
                "settings": [
                    setting("port", "5432", "integer"),
                    setting("listen_addresses", "0.0.0.0", "string"),
                    setting("shared_preload_libraries", "neon", "string"),
                    setting("wal_level", "logical", "enum"),
                    setting("wal_log_hints", "on", "bool"),
                    setting("max_connections", "100", "integer"),
                    setting("max_wal_senders", "10", "integer"),
                    setting("max_replication_slots", "10", "integer"),
                    setting("wal_keep_size", "0", "integer"),
                    setting("restart_after_crash", "off", "bool"),
                    setting("synchronous_standby_names", "walproposer", "string"),
                    setting("password_encryption", "scram-sha-256", "enum"),
                ],
            },
            "delta_operations": [],
        },
        "compute_ctl_config": { "jwks": inputs.jwks },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn builds_an_attached_spec() {
        let tenant = NeonId::from(Uuid::from_u128(1));
        let timeline = NeonId::from(Uuid::from_u128(2));
        let v = config_response(&SpecInputs {
            endpoint_id: "ep-a",
            tenant_id: &tenant,
            timeline_id: &timeline,
            pageserver_connstring: "postgresql://no_user@ps:6400".into(),
            safekeeper_connstrings: vec!["sk-0:5454".into(), "sk-1:5454".into()],
            roles: vec![RoleSpec {
                name: "app".into(),
                scram_verifier: "SCRAM-SHA-256$x".into(),
            }],
            databases: vec![DatabaseSpec {
                name: "app".into(),
                owner: "app".into(),
            }],
            jwks: json!({ "keys": [{ "kid": "k" }] }),
        });
        assert_eq!(v["status"], "attached");
        assert_eq!(v["spec"]["tenant_id"], "00000000000000000000000000000001");
        assert_eq!(v["spec"]["timeline_id"], "00000000000000000000000000000002");
        assert_eq!(
            v["spec"]["pageserver_connstring"],
            "postgresql://no_user@ps:6400"
        );
        assert_eq!(v["spec"]["safekeeper_connstrings"][1], "sk-1:5454");
        assert_eq!(v["spec"]["cluster"]["cluster_id"], "ep-a");
        assert_eq!(
            v["spec"]["cluster"]["roles"][0]["encrypted_password"],
            "SCRAM-SHA-256$x"
        );
        assert_eq!(v["spec"]["cluster"]["databases"][0]["owner"], "app");
        assert_eq!(v["compute_ctl_config"]["jwks"]["keys"][0]["kid"], "k");
        let settings = v["spec"]["cluster"]["settings"].as_array().unwrap();
        let get = |name: &str| {
            settings
                .iter()
                .find(|s| s["name"] == name)
                .map(|s| s["value"].as_str().unwrap().to_string())
        };
        assert_eq!(get("shared_preload_libraries").as_deref(), Some("neon"));
        assert_eq!(
            get("synchronous_standby_names").as_deref(),
            Some("walproposer")
        );
        assert_eq!(get("password_encryption").as_deref(), Some("scram-sha-256"));
    }
}
