//! The compute spec served to compute_ctl (`GET /compute/api/v2/computes/{id}/spec`).
//!
//! Field names follow neon's `compute_api::spec::ComputeSpec` and
//! `compute_api::responses::ControlPlaneConfigResponse`.

use anyhow::Context;
use serde_json::{Value, json};

use crate::id::NeonId;
use crate::storcon::{StorconClient, StorconError};

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

/// Where an attached tenant lives; `StorconClient` in production, a fake in tests.
pub trait Placement: Send + Sync {
    fn pageserver_connstring(
        &self,
        tenant: &NeonId,
    ) -> impl Future<Output = Result<String, StorconError>> + Send;
    fn active_safekeepers(&self) -> impl Future<Output = Result<Vec<String>, StorconError>> + Send;
}

impl Placement for StorconClient {
    async fn pageserver_connstring(&self, tenant: &NeonId) -> Result<String, StorconError> {
        StorconClient::pageserver_connstring(self, tenant).await
    }
    async fn active_safekeepers(&self) -> Result<Vec<String>, StorconError> {
        StorconClient::active_safekeepers(self).await
    }
}

/// An endpoint as stored: ids are the 32-hex strings.
pub struct Endpoint<'a> {
    pub id: &'a str,
    pub tenant_id: &'a str,
    pub timeline_id: &'a str,
}

/// [`endpoint_config`] for an endpoint's rows as cheladb-metadata stores them.
#[cfg(feature = "metadata")]
pub async fn stored_endpoint_config(
    placement: &impl Placement,
    endpoint: &cheladb_metadata::NeonEndpoint,
    roles: Vec<cheladb_metadata::NeonRole>,
    databases: Vec<cheladb_metadata::NeonDatabase>,
    jwks: Value,
) -> anyhow::Result<Value> {
    endpoint_config(
        placement,
        Endpoint {
            id: &endpoint.id,
            tenant_id: &endpoint.tenant_id,
            timeline_id: &endpoint.timeline_id,
        },
        roles
            .into_iter()
            .map(|r| RoleSpec {
                name: r.name,
                scram_verifier: r.scram_verifier,
            })
            .collect(),
        databases
            .into_iter()
            .map(|d| DatabaseSpec {
                name: d.name,
                owner: d.owner,
            })
            .collect(),
        jwks,
    )
    .await
}

/// `config_response` for a stored endpoint, with storage placement from `placement`.
pub async fn endpoint_config(
    placement: &impl Placement,
    endpoint: Endpoint<'_>,
    roles: Vec<RoleSpec>,
    databases: Vec<DatabaseSpec>,
    jwks: Value,
) -> anyhow::Result<Value> {
    let tenant: NeonId = endpoint.tenant_id.parse().context("stored tenant id")?;
    let timeline: NeonId = endpoint.timeline_id.parse().context("stored timeline id")?;
    let pageserver_connstring = placement.pageserver_connstring(&tenant).await?;
    let safekeeper_connstrings = placement.active_safekeepers().await?;
    Ok(config_response(&SpecInputs {
        endpoint_id: endpoint.id,
        tenant_id: &tenant,
        timeline_id: &timeline,
        pageserver_connstring,
        safekeeper_connstrings,
        roles,
        databases,
        jwks,
    }))
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

    struct FakePlacement {
        pageserver: Result<String, &'static str>,
    }

    impl Placement for FakePlacement {
        async fn pageserver_connstring(&self, _tenant: &NeonId) -> Result<String, StorconError> {
            self.pageserver
                .clone()
                .map_err(|m| StorconError::Other(anyhow::anyhow!(m)))
        }
        async fn active_safekeepers(&self) -> Result<Vec<String>, StorconError> {
            Ok(vec!["sk-0:5454".into(), "sk-1:5454".into()])
        }
    }

    const TENANT: &str = "00000000000000000000000000000001";
    const TIMELINE: &str = "00000000000000000000000000000002";

    fn endpoint(tenant_id: &str) -> Endpoint<'_> {
        Endpoint {
            id: "ep-a",
            tenant_id,
            timeline_id: TIMELINE,
        }
    }

    #[tokio::test]
    async fn endpoint_config_uses_the_placement() {
        let placement = FakePlacement {
            pageserver: Ok("postgresql://no_user@ps:6400".into()),
        };
        let v = endpoint_config(
            &placement,
            endpoint(TENANT),
            vec![RoleSpec {
                name: "app".into(),
                scram_verifier: "SCRAM-SHA-256$x".into(),
            }],
            vec![DatabaseSpec {
                name: "db".into(),
                owner: "app".into(),
            }],
            json!({"keys": [{"kid": "k"}]}),
        )
        .await
        .unwrap();
        assert_eq!(v["spec"]["tenant_id"], TENANT);
        assert_eq!(v["spec"]["timeline_id"], TIMELINE);
        assert_eq!(
            v["spec"]["pageserver_connstring"],
            "postgresql://no_user@ps:6400"
        );
        assert_eq!(v["spec"]["safekeeper_connstrings"][1], "sk-1:5454");
        assert_eq!(v["spec"]["cluster"]["cluster_id"], "ep-a");
        assert_eq!(v["spec"]["cluster"]["roles"][0]["name"], "app");
        assert_eq!(v["spec"]["cluster"]["databases"][0]["owner"], "app");
        assert_eq!(v["compute_ctl_config"]["jwks"]["keys"][0]["kid"], "k");
    }

    #[tokio::test]
    async fn a_placement_error_propagates() {
        let placement = FakePlacement {
            pageserver: Err("no pageserver"),
        };
        let err = endpoint_config(&placement, endpoint(TENANT), vec![], vec![], json!({}))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("no pageserver"), "{err:#}");
    }

    #[tokio::test]
    async fn a_bad_stored_id_is_named() {
        let placement = FakePlacement {
            pageserver: Ok(String::new()),
        };
        let err = endpoint_config(&placement, endpoint("nope"), vec![], vec![], json!({}))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("stored tenant id"), "{err:#}");
    }
}
