//! The compute spec served to compute_ctl (`GET /compute/api/v2/computes/{id}/spec`).
//!
//! Field names follow neon's `compute_api::spec::ComputeSpec` and
//! `compute_api::responses::ControlPlaneConfigResponse`.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::compute_tls::{CA_CERT_PATH, SERVER_CERT_PATH, SERVER_KEY_PATH};
use crate::id::NeonId;
use crate::jwt::ComputeSigner;
// The unit tests below build tokens by scope.
#[cfg(test)]
use crate::jwt::StorageScope;
use crate::storcon::{StorconClient, StorconError};

pub struct RoleSpec {
    pub name: String,
    pub scram_verifier: String,
}

pub struct DatabaseSpec {
    pub name: String,
    pub owner: String,
}

/// The compute's mode, as the fork's `compute_api::spec::ComputeMode` (externally tagged;
/// the LSN is its `X/Y` text): a read-write primary, or a read-only node pinned at an LSN.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ComputeMode {
    #[default]
    Primary,
    Static {
        lsn: String,
    },
}

impl ComputeMode {
    /// A stored endpoint's mode: static when it has a pinned LSN, which must be Postgres's
    /// `X/Y` (hex) form, as compute_ctl reads it; anything else is an error (T14 apply minor
    /// 21), never passed on.
    pub fn from_static_lsn(static_lsn: Option<&str>) -> anyhow::Result<Self> {
        let Some(lsn) = static_lsn else {
            return Ok(Self::Primary);
        };
        lsn.parse::<crate::Lsn>()
            .map_err(|e| anyhow::anyhow!("the endpoint's static LSN: {e}"))?;
        Ok(Self::Static {
            lsn: lsn.to_string(),
        })
    }

    fn to_json(&self) -> Value {
        match self {
            Self::Primary => json!("Primary"),
            Self::Static { lsn } => json!({ "Static": lsn }),
        }
    }
}

/// Where member roles (`cb_m_*`) may log in from, as the fork's `ComputeSpec.member_logins`:
/// compute_ctl writes one `hostssl … clientcert=verify-full map=chelabase_member` line per CIDR
/// (`all` when the list is empty: no address rule) and one `pg_ident.conf` line per CN.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberLogins {
    pub allowed_cidrs: Vec<String>,
    pub client_cns: Vec<String>,
}

impl MemberLogins {
    /// The orchestrator's certificate CN and the given CIDRs.
    pub fn orchestrator(allowed_cidrs: Vec<String>) -> Self {
        Self {
            allowed_cidrs,
            client_cns: vec![crate::compute_tls::ORCHESTRATOR_CN.to_string()],
        }
    }

    /// Realtime's certificate CN ([`crate::compute_tls::REALTIME_CN`]) and the given CIDRs: who
    /// may log in as `chelabase_realtime`. An empty list means any address (with the certificate).
    pub fn realtime(allowed_cidrs: Vec<String>) -> Self {
        Self {
            allowed_cidrs,
            client_cns: vec![crate::compute_tls::REALTIME_CN.to_string()],
        }
    }
}

/// `CHELABASE_MEMBER_LOGIN_CIDRS`: a comma-separated list of `<ipv4|ipv6>/<prefix>`. Unset is
/// an error (the services refuse to start); empty (or blank) is the empty list, "no address
/// rule". Entries are trimmed; an empty entry or anything else is an error naming it.
pub fn parse_member_login_cidrs(raw: Option<&str>) -> anyhow::Result<Vec<String>> {
    parse_login_cidrs(
        raw,
        "CHELABASE_MEMBER_LOGIN_CIDRS",
        "the addresses member logins come from",
    )
}

/// `CHELABASE_REALTIME_LOGIN_CIDRS`: the same grammar as [`parse_member_login_cidrs`], for
/// realtime's logins.
pub fn parse_realtime_login_cidrs(raw: Option<&str>) -> anyhow::Result<Vec<String>> {
    parse_login_cidrs(
        raw,
        "CHELABASE_REALTIME_LOGIN_CIDRS",
        "the addresses realtime's logins come from",
    )
}

fn parse_login_cidrs(raw: Option<&str>, var: &str, what: &str) -> anyhow::Result<Vec<String>> {
    let Some(raw) = raw else {
        anyhow::bail!(
            "{var} is unset: set it to {what} (comma-separated CIDRs), or to an empty value for \
             no address rule"
        );
    };
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    raw.split(',')
        .map(|entry| {
            let entry = entry.trim();
            check_cidr(entry).with_context(|| format!("{var} entry {entry:?}"))?;
            Ok(entry.to_string())
        })
        .collect()
}

fn check_cidr(entry: &str) -> anyhow::Result<()> {
    let (addr, prefix) = entry
        .split_once('/')
        .context("not <address>/<prefix>")?;
    let addr: std::net::IpAddr = addr.parse().context("not an IP address")?;
    let max: u8 = if addr.is_ipv4() { 32 } else { 128 };
    let digits = !prefix.is_empty() && prefix.bytes().all(|b| b.is_ascii_digit());
    match prefix.parse::<u8>() {
        Ok(p) if digits && p <= max => Ok(()),
        _ => anyhow::bail!("the prefix isn't a number from 0 to {max}"),
    }
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
    /// URL to the Chelabase console (neon.console_url setting if Some).
    pub console_url: Option<String>,
    pub mode: ComputeMode,
    /// The member-login rules every compute gets.
    pub member_logins: MemberLogins,
    /// Who may log in as `chelabase_realtime` (JSON key `realtime_logins`).
    pub realtime_logins: MemberLogins,
    /// The branch is protected: the compute refuses databases born open (`neon.branch_protected`).
    pub branch_protected: bool,
    /// The compute's NeonJWT for its own tenant (scope `tenant`): compute_ctl writes it to its
    /// storage token file, the password of every pageserver and safekeeper connection.
    pub storage_auth_token: String,
}

/// What differs between callers of [`endpoint_config`]: the login rules, branch protection and
/// whether the compute gets an expiring storage token.
pub struct SpecPolicy<'a> {
    pub member_logins: &'a MemberLogins,
    pub realtime_logins: &'a MemberLogins,
    pub branch_protected: bool,
    pub storage_token_expiring: bool,
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
    pub mode: ComputeMode,
}

/// [`endpoint_config`] for an endpoint's rows as chelabase-metadata stores them.
#[cfg(feature = "metadata")]
#[allow(clippy::too_many_arguments)]
pub async fn stored_endpoint_config(
    placement: &impl Placement,
    endpoint: &chelabase_metadata::NeonEndpoint,
    roles: Vec<chelabase_metadata::NeonRole>,
    databases: Vec<chelabase_metadata::NeonDatabase>,
    jwks: Value,
    console_url: Option<String>,
    policy: &SpecPolicy<'_>,
    signer: &ComputeSigner,
) -> anyhow::Result<Value> {
    endpoint_config(
        placement,
        Endpoint {
            id: &endpoint.id,
            tenant_id: &endpoint.tenant_id,
            timeline_id: &endpoint.timeline_id,
            mode: ComputeMode::from_static_lsn(endpoint.static_lsn.as_deref())?,
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
        console_url,
        policy,
        signer,
    )
    .await
}

/// `config_response` for a stored endpoint, with storage placement from `placement`.
#[allow(clippy::too_many_arguments)]
pub async fn endpoint_config(
    placement: &impl Placement,
    endpoint: Endpoint<'_>,
    roles: Vec<RoleSpec>,
    databases: Vec<DatabaseSpec>,
    jwks: Value,
    console_url: Option<String>,
    policy: &SpecPolicy<'_>,
    signer: &ComputeSigner,
) -> anyhow::Result<Value> {
    let tenant: NeonId = endpoint.tenant_id.parse().context("stored tenant id")?;
    let timeline: NeonId = endpoint.timeline_id.parse().context("stored timeline id")?;
    let pageserver_connstring = placement.pageserver_connstring(&tenant).await?;
    let safekeeper_connstrings = placement.active_safekeepers().await?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("the system clock is before 1970")?
        .as_secs();
    let storage_auth_token = signer
        .tenant_token(&tenant, now, policy.storage_token_expiring)
        .context("signing the compute's storage token")?;
    Ok(config_response(&SpecInputs {
        endpoint_id: endpoint.id,
        tenant_id: &tenant,
        timeline_id: &timeline,
        pageserver_connstring,
        safekeeper_connstrings,
        roles,
        databases,
        jwks,
        console_url,
        mode: endpoint.mode,
        member_logins: policy.member_logins.clone(),
        realtime_logins: policy.realtime_logins.clone(),
        branch_protected: policy.branch_protected,
        storage_auth_token,
    }))
}

fn setting(name: &str, value: &str, vartype: &str) -> Value {
    json!({ "name": name, "value": value, "vartype": vartype })
}

/// DDL forwarding endpoint URL for a control plane URI and endpoint id.
pub fn ddl_url(control_plane_uri: &str, endpoint_id: &str) -> String {
    let base = control_plane_uri.trim_end_matches('/');
    format!("{}/compute/api/v2/computes/{}/ddl", base, endpoint_id)
}

/// `{spec, status, compute_ctl_config}` for an attached compute.
pub fn config_response(inputs: &SpecInputs<'_>) -> Value {
    let roles: Vec<Value> = inputs
        .roles
        .iter()
        .map(|role| {
            json!({ "name": role.name, "encrypted_password": role.scram_verifier, "options": null })
        })
        .collect();
    let databases: Vec<Value> = inputs
        .databases
        .iter()
        .map(|d| json!({ "name": d.name, "owner": d.owner, "options": null }))
        .collect();

    let mut settings = vec![
        setting("port", "5432", "integer"),
        setting("listen_addresses", "0.0.0.0", "string"),
        setting("shared_preload_libraries", "neon", "string"),
        setting("wal_level", "logical", "enum"),
        setting("wal_log_hints", "on", "bool"),
        setting("max_connections", "100", "integer"),
        // Room for realtime's temporary logical slots (P14), one walsender each.
        setting("max_wal_senders", "20", "integer"),
        setting("max_replication_slots", "20", "integer"),
        setting("wal_keep_size", "0", "integer"),
        // W8: a slot that falls this far behind is invalidated instead of filling the disk.
        setting("max_slot_wal_keep_size", "1GB", "integer"),
        setting("restart_after_crash", "off", "bool"),
        setting("synchronous_standby_names", "walproposer", "string"),
        setting("password_encryption", "scram-sha-256", "enum"),
        setting("createrole_self_grant", "set, inherit", "string"),
        // Server messages in English, whatever the image's locale: the table API's cascade
        // check (R4) reads Postgres's `drop cascades to …` notices by their English text.
        // `lc_messages` is superuser-only, so no role of the branch can change it.
        setting("lc_messages", "C", "string"),
        // pg_partman 5.5 requires the background worker's role; the library isn't preloaded, so
        // this is only the value it would use.
        setting("pg_partman_bgw.role", "postgres", "string"),
    ];

    if let Some(ref console_url) = inputs.console_url {
        settings.push(setting("neon.console_url", console_url, "string"));
    }
    // The fork refuses a non-superuser DROP DATABASE of a listed name (comma-separated exact
    // names). The spec's databases are the endpoint's recorded main database (one row; the
    // first by name, as the DDL receiver reads it); its name comes from our validated create.
    if let Some(main) = inputs.databases.first() {
        settings.push(setting("neon.protected_databases", &main.name, "string"));
    }

    json!({
        "status": "attached",
        "spec": {
            "format_version": 1.0,
            "suspend_timeout_seconds": -1,
            "tenant_id": inputs.tenant_id,
            "timeline_id": inputs.timeline_id,
            "pageserver_connstring": inputs.pageserver_connstring,
            "safekeeper_connstrings": inputs.safekeeper_connstrings,
            "mode": inputs.mode.to_json(),
            "cluster": {
                "cluster_id": inputs.endpoint_id,
                "name": inputs.endpoint_id,
                "state": "restarted",
                "roles": roles,
                "databases": databases,
                "settings": settings,
            },
            "delta_operations": [],
            // Every compute serves TLS with the certificate the worker uploads before start.
            "features": ["tls_experimental"],
            "member_logins": inputs.member_logins,
            "realtime_logins": inputs.realtime_logins,
            "branch_protected": inputs.branch_protected,
            "storage_auth_token": inputs.storage_auth_token,
        },
        "compute_ctl_config": {
            "jwks": inputs.jwks,
            "tls": {
                "key_path": SERVER_KEY_PATH,
                "cert_path": SERVER_CERT_PATH,
                "ca_path": CA_CERT_PATH,
            },
        },
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
            console_url: None,
            mode: ComputeMode::Primary,
            member_logins: logins(),
            branch_protected: false,
            storage_auth_token: "tok".into(),
            realtime_logins: realtime_logins(),
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
        // TE-A R4: the cascade check reads Postgres's English `drop cascades to` notices.
        assert_eq!(get("lc_messages").as_deref(), Some("C"));
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

    fn test_signer() -> ComputeSigner {
        let (pem, x) = crate::jwt::generate_keypair();
        ComputeSigner::new(&pem, &x).unwrap()
    }

    fn endpoint(tenant_id: &str) -> Endpoint<'_> {
        Endpoint {
            id: "ep-a",
            tenant_id,
            timeline_id: TIMELINE,
            mode: ComputeMode::Primary,
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
            None,
            &policy(&logins(), &realtime_logins(), false, false),
            &test_signer(),
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
        let err = endpoint_config(&placement, endpoint(TENANT), vec![], vec![], json!({}), None, &policy(&logins(), &realtime_logins(), false, false), &test_signer())
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("no pageserver"), "{err:#}");
    }

    #[tokio::test]
    async fn a_bad_stored_id_is_named() {
        let placement = FakePlacement {
            pageserver: Ok(String::new()),
        };
        let err = endpoint_config(&placement, endpoint("nope"), vec![], vec![], json!({}), None, &policy(&logins(), &realtime_logins(), false, false), &test_signer())
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("stored tenant id"), "{err:#}");
    }

    #[test]
    fn roles_carry_no_privileged_key() {
        let tenant = NeonId::from(Uuid::from_u128(1));
        let timeline = NeonId::from(Uuid::from_u128(2));
        // Roles no longer carry privileged field
        let v = config_response(&SpecInputs {
            endpoint_id: "ep-a",
            tenant_id: &tenant,
            timeline_id: &timeline,
            pageserver_connstring: "postgresql://no_user@ps:6400".into(),
            safekeeper_connstrings: vec!["sk-0:5454".into()],
            roles: vec![
                RoleSpec {
                    name: "app".into(),
                    scram_verifier: "SCRAM-SHA-256$x".into(),
                },
                RoleSpec {
                    name: "owner".into(),
                    scram_verifier: "SCRAM-SHA-256$y".into(),
                },
            ],
            databases: vec![],
            jwks: json!({}),
            console_url: None,
            mode: ComputeMode::Primary,
            member_logins: logins(),
            branch_protected: false,
            storage_auth_token: "tok".into(),
            realtime_logins: realtime_logins(),
        });
        let roles = v["spec"]["cluster"]["roles"].as_array().unwrap();

        // Find the plain role and verify no privileged key
        let app_role = roles.iter().find(|r| r["name"] == "app").unwrap();
        assert!(
            app_role.get("privileged").is_none(),
            "roles should not have privileged key"
        );

        // Find the owner role and verify no privileged key
        let owner_role = roles.iter().find(|r| r["name"] == "owner").unwrap();
        assert!(
            owner_role.get("privileged").is_none(),
            "roles should not have privileged key"
        );
    }

    #[test]
    fn console_url_is_a_string_setting() {
        let tenant = NeonId::from(Uuid::from_u128(1));
        let timeline = NeonId::from(Uuid::from_u128(2));

        // With console_url
        let v = config_response(&SpecInputs {
            endpoint_id: "ep-a",
            tenant_id: &tenant,
            timeline_id: &timeline,
            pageserver_connstring: "postgresql://no_user@ps:6400".into(),
            safekeeper_connstrings: vec!["sk-0:5454".into()],
            roles: vec![],
            databases: vec![],
            jwks: json!({}),
            console_url: Some("http://studio.chelabase.localhost:8000".into()),
            mode: ComputeMode::Primary,
            member_logins: logins(),
            branch_protected: false,
            storage_auth_token: "tok".into(),
            realtime_logins: realtime_logins(),
        });
        let settings = v["spec"]["cluster"]["settings"].as_array().unwrap();
        let console_setting = settings.iter().find(|s| s["name"] == "neon.console_url");
        assert!(console_setting.is_some(), "console_url setting should be present");
        assert_eq!(console_setting.unwrap()["value"], "http://studio.chelabase.localhost:8000");

        // Without console_url
        let v2 = config_response(&SpecInputs {
            endpoint_id: "ep-a",
            tenant_id: &tenant,
            timeline_id: &timeline,
            pageserver_connstring: "postgresql://no_user@ps:6400".into(),
            safekeeper_connstrings: vec!["sk-0:5454".into()],
            roles: vec![],
            databases: vec![],
            jwks: json!({}),
            console_url: None,
            mode: ComputeMode::Primary,
            member_logins: logins(),
            branch_protected: false,
            storage_auth_token: "tok".into(),
            realtime_logins: realtime_logins(),
        });
        let settings2 = v2["spec"]["cluster"]["settings"].as_array().unwrap();
        let console_setting2 = settings2.iter().find(|s| s["name"] == "neon.console_url");
        assert!(console_setting2.is_none(), "console_url setting should not be present when None");
    }

    #[test]
    fn createrole_self_grant_is_set_inherit() {
        let tenant = NeonId::from(Uuid::from_u128(1));
        let timeline = NeonId::from(Uuid::from_u128(2));

        let v = config_response(&SpecInputs {
            endpoint_id: "ep-a",
            tenant_id: &tenant,
            timeline_id: &timeline,
            pageserver_connstring: "postgresql://no_user@ps:6400".into(),
            safekeeper_connstrings: vec!["sk-0:5454".into()],
            roles: vec![],
            databases: vec![],
            jwks: json!({}),
            console_url: None,
            mode: ComputeMode::Primary,
            member_logins: logins(),
            branch_protected: false,
            storage_auth_token: "tok".into(),
            realtime_logins: realtime_logins(),
        });
        let settings = v["spec"]["cluster"]["settings"].as_array().unwrap();
        let grant_setting = settings.iter().find(|s| s["name"] == "createrole_self_grant");
        assert!(grant_setting.is_some(), "createrole_self_grant setting should be present");
        assert_eq!(grant_setting.unwrap()["value"], "set, inherit");
    }

    /// RT1 T5 (P14, W8): room for realtime's slots and walsenders (20 each), and a slot that
    /// falls more than 1 GB behind is invalidated instead of filling the compute's disk.
    #[test]
    fn replication_settings_leave_room_for_realtime_and_bound_slot_wal() {
        let tenant = NeonId::from(Uuid::from_u128(1));
        let timeline = NeonId::from(Uuid::from_u128(2));
        let v = config_response(&SpecInputs {
            endpoint_id: "ep-a",
            tenant_id: &tenant,
            timeline_id: &timeline,
            pageserver_connstring: "postgresql://no_user@ps:6400".into(),
            safekeeper_connstrings: vec![],
            roles: vec![],
            databases: vec![],
            jwks: json!({}),
            console_url: None,
            mode: ComputeMode::Primary,
            member_logins: logins(),
            branch_protected: false,
            storage_auth_token: "tok".into(),
            realtime_logins: realtime_logins(),
        });
        let settings = v["spec"]["cluster"]["settings"].as_array().unwrap();
        let get = |name: &str| {
            let found: Vec<&Value> = settings.iter().filter(|s| s["name"] == name).collect();
            assert_eq!(found.len(), 1, "{name} once: {settings:?}");
            (found[0]["value"].clone(), found[0]["vartype"].clone())
        };
        assert_eq!(get("max_replication_slots"), (json!("20"), json!("integer")));
        assert_eq!(get("max_wal_senders"), (json!("20"), json!("integer")));
        assert_eq!(get("max_slot_wal_keep_size"), (json!("1GB"), json!("integer")));
        // Unchanged: no WAL kept for walsenders beyond what slots retain.
        assert_eq!(get("wal_keep_size"), (json!("0"), json!("integer")));
        assert_eq!(get("max_connections"), (json!("100"), json!("integer")));
    }

    #[test]
    fn the_main_database_is_protected() {
        let tenant = NeonId::from(Uuid::from_u128(1));
        let timeline = NeonId::from(Uuid::from_u128(2));
        let inputs = |databases: Vec<DatabaseSpec>| SpecInputs {
            endpoint_id: "ep-a",
            tenant_id: &tenant,
            timeline_id: &timeline,
            pageserver_connstring: "postgresql://no_user@ps:6400".into(),
            safekeeper_connstrings: vec!["sk-0:5454".into()],
            roles: vec![],
            databases,
            jwks: json!({}),
            console_url: None,
            mode: ComputeMode::Primary,
            member_logins: logins(),
            branch_protected: false,
            storage_auth_token: "tok".into(),
            realtime_logins: realtime_logins(),
        };
        let protected = |v: &Value| {
            v["spec"]["cluster"]["settings"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["name"] == "neon.protected_databases")
                .cloned()
        };

        let v = config_response(&inputs(vec![DatabaseSpec {
            name: "my-app".into(),
            owner: "postgres".into(),
        }]));
        assert_eq!(
            protected(&v),
            Some(json!({
                "name": "neon.protected_databases",
                "value": "my-app",
                "vartype": "string"
            }))
        );

        // No recorded main database: nothing is protected.
        assert_eq!(protected(&config_response(&inputs(vec![]))), None);
    }

    #[test]
    fn ddl_url_joins_without_double_slash() {
        let url = ddl_url("http://control-plane:8083", "ep-abc");
        assert_eq!(url, "http://control-plane:8083/compute/api/v2/computes/ep-abc/ddl");

        let url_trailing = ddl_url("http://control-plane:8083/", "ep-xyz");
        assert_eq!(url_trailing, "http://control-plane:8083/compute/api/v2/computes/ep-xyz/ddl");
    }

    #[test]
    fn static_mode_serializes_like_the_fork() {
        // The fork's `ComputeMode` is externally tagged and its `Lsn` serializes as "X/Y"
        // (libs/compute_api/src/spec.rs, libs/utils/src/lsn.rs).
        assert_eq!(
            ComputeMode::Static {
                lsn: "0/16B3748".into()
            }
            .to_json(),
            json!({ "Static": "0/16B3748" })
        );
        assert_eq!(ComputeMode::Primary.to_json(), json!("Primary"));
        assert_eq!(
            ComputeMode::from_static_lsn(Some("0/16B3748")).unwrap(),
            ComputeMode::Static {
                lsn: "0/16B3748".into()
            }
        );
        assert_eq!(ComputeMode::from_static_lsn(None).unwrap(), ComputeMode::Primary);
    }

    // T14 apply minor 21: a stored static LSN that isn't `X/Y` never reaches compute_ctl.
    #[test]
    fn a_static_lsn_must_be_x_slash_y() {
        for bad in ["", "16B3748", "0/16B3748/1", "0/xyz", "0/16B3748 "] {
            assert!(ComputeMode::from_static_lsn(Some(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn primary_spec_unchanged() {
        let tenant = NeonId::from(Uuid::from_u128(1));
        let timeline = NeonId::from(Uuid::from_u128(2));
        let v = config_response(&SpecInputs {
            endpoint_id: "ep-a",
            tenant_id: &tenant,
            timeline_id: &timeline,
            pageserver_connstring: "postgresql://no_user@ps:6400".into(),
            safekeeper_connstrings: vec![],
            roles: vec![],
            databases: vec![],
            jwks: json!({}),
            console_url: None,
            mode: ComputeMode::Primary,
            member_logins: logins(),
            branch_protected: false,
            storage_auth_token: "tok".into(),
            realtime_logins: realtime_logins(),
        });
        // Primary is the fork's default: the only addition to the old spec is `mode`.
        assert_eq!(v["spec"]["mode"], "Primary");
        let mut keys: Vec<&str> = v["spec"].as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "branch_protected",
                "cluster",
                "delta_operations",
                "features",
                "format_version",
                "member_logins",
                "mode",
                "pageserver_connstring",
                "realtime_logins",
                "safekeeper_connstrings",
                "storage_auth_token",
                "suspend_timeout_seconds",
                "tenant_id",
                "timeline_id"
            ]
        );
    }

    #[tokio::test]
    async fn endpoint_config_carries_the_member_logins() {
        let placement = FakePlacement {
            pageserver: Ok("postgresql://no_user@ps:6400".into()),
        };
        let logins = MemberLogins::orchestrator(vec!["10.213.0.10/32".into()]);
        let v = endpoint_config(&placement, endpoint(TENANT), vec![], vec![], json!({}), None, &policy(&logins, &realtime_logins(), false, false), &test_signer())
            .await
            .unwrap();
        assert_eq!(
            v["spec"]["member_logins"],
            json!({"allowed_cidrs": ["10.213.0.10/32"], "client_cns": ["chelabase-orchestrator"]})
        );
    }

    #[test]
    fn cidr_prefixes_are_plain_numbers() {
        for bad in ["10.0.0.1/+32", "10.0.0.1/", "/32", "fd00::1/129", "10.0.0.1/32/1", ",10.0.0.1/32"] {
            assert!(parse_member_login_cidrs(Some(bad)).is_err(), "{bad:?}");
        }
        assert_eq!(parse_member_login_cidrs(Some("  ")).unwrap(), Vec::<String>::new());
        assert_eq!(parse_member_login_cidrs(Some("0.0.0.0/0")).unwrap(), ["0.0.0.0/0"]);
    }

    fn logins() -> MemberLogins {
        MemberLogins {
            allowed_cidrs: vec!["10.213.4.10/32".into(), "10.213.4.11/32".into()],
            client_cns: vec!["chelabase-orchestrator".into()],
        }
    }

    fn realtime_logins() -> MemberLogins {
        MemberLogins::realtime(vec!["10.213.4.12/32".into(), "10.213.4.11/32".into()])
    }

    fn policy<'a>(
        member: &'a MemberLogins,
        realtime: &'a MemberLogins,
        branch_protected: bool,
        storage_token_expiring: bool,
    ) -> SpecPolicy<'a> {
        SpecPolicy {
            member_logins: member,
            realtime_logins: realtime,
            branch_protected,
            storage_token_expiring,
        }
    }

    fn inputs_for_tests<'a>(tenant: &'a NeonId, timeline: &'a NeonId) -> SpecInputs<'a> {
        SpecInputs {
            endpoint_id: "ep-a",
            tenant_id: tenant,
            timeline_id: timeline,
            pageserver_connstring: "postgresql://no_user@ps:6400".into(),
            safekeeper_connstrings: vec![],
            roles: vec![],
            databases: vec![],
            jwks: json!({}),
            console_url: None,
            mode: ComputeMode::Primary,
            member_logins: logins(),
            branch_protected: false,
            storage_auth_token: "tok".into(),
            realtime_logins: realtime_logins(),
        }
    }

    // U14: the branch's protection reaches the compute as `spec.branch_protected` (the fork's
    // `ComputeSpec.branch_protected: bool`, `#[serde(default)]`), always present, from the
    // branch's protection.
    #[test]
    fn the_spec_carries_the_branchs_protection_under_the_forks_field_name() {
        let tenant = NeonId::from(Uuid::from_u128(1));
        let timeline = NeonId::from(Uuid::from_u128(2));
        for protected in [true, false] {
            let v = config_response(&SpecInputs {
                branch_protected: protected,
                ..inputs_for_tests(&tenant, &timeline)
            });
            assert_eq!(v["spec"]["branch_protected"], json!(protected));
            // A top-level `spec` field, not a cluster setting (compute_ctl writes the GUC itself).
            let settings = v["spec"]["cluster"]["settings"].as_array().unwrap();
            assert!(
                !settings.iter().any(|s| s["name"] == "neon.branch_protected"),
                "{settings:?}"
            );
        }
    }

    #[tokio::test]
    async fn endpoint_config_passes_the_protection_through() {
        let placement = FakePlacement {
            pageserver: Ok("postgresql://no_user@ps:6400".into()),
        };
        for protected in [true, false] {
            let v = endpoint_config(
                &placement,
                endpoint(TENANT),
                vec![],
                vec![],
                json!({}),
                None,
                &policy(&logins(), &realtime_logins(), protected, false),
                &test_signer(),
            )
            .await
            .unwrap();
            assert_eq!(v["spec"]["branch_protected"], json!(protected));
        }
    }

    #[test]
    fn every_compute_gets_tls_and_the_member_login_rules() {
        let tenant = NeonId::from(Uuid::from_u128(1));
        let timeline = NeonId::from(Uuid::from_u128(2));
        let v = config_response(&SpecInputs {
            endpoint_id: "ep-a",
            tenant_id: &tenant,
            timeline_id: &timeline,
            pageserver_connstring: "postgresql://no_user@ps:6400".into(),
            safekeeper_connstrings: vec![],
            roles: vec![],
            databases: vec![],
            jwks: json!({}),
            console_url: None,
            mode: ComputeMode::Primary,
            member_logins: logins(),
            branch_protected: false,
            storage_auth_token: "tok".into(),
            realtime_logins: realtime_logins(),
        });
        assert_eq!(v["spec"]["features"], json!(["tls_experimental"]));
        assert_eq!(
            v["compute_ctl_config"]["tls"],
            json!({
                "key_path": "/var/db/chelabase/tls/server.key",
                "cert_path": "/var/db/chelabase/tls/server.crt",
                "ca_path": "/var/db/chelabase/tls/ca.crt",
            })
        );
        assert_eq!(
            v["spec"]["member_logins"],
            json!({
                "allowed_cidrs": ["10.213.4.10/32", "10.213.4.11/32"],
                "client_cns": ["chelabase-orchestrator"],
            })
        );
        // An empty list is "no address rule" (Kubernetes), still sent as an empty list.
        let mut empty = logins();
        empty.allowed_cidrs.clear();
        let v = config_response(&SpecInputs { member_logins: empty, ..inputs_for_tests(&tenant, &timeline) });
        assert_eq!(v["spec"]["member_logins"]["allowed_cidrs"], json!([]));
    }

    // --- round 4 (C1): realtime logins (W31), expiring storage tokens (item 7), S21 ----------

    fn payload(token: &str) -> Value {
        use base64::Engine;
        let part = token.split('.').nth(1).expect("a JWT");
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(part)
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn realtime_logins_use_the_realtime_certificate_cn() {
        let rt = MemberLogins::realtime(vec!["10.213.4.12/32".into()]);
        assert_eq!(rt.allowed_cidrs, ["10.213.4.12/32"]);
        assert_eq!(rt.client_cns, ["chelabase-realtime"]);
        assert_eq!(rt.client_cns, [crate::compute_tls::REALTIME_CN]);
        // Not the orchestrator's: a member login and a realtime login are different rules.
        assert_ne!(rt.client_cns, MemberLogins::orchestrator(vec![]).client_cns);
        assert_eq!(MemberLogins::realtime(Vec::new()).allowed_cidrs, Vec::<String>::new());
    }

    #[test]
    fn the_spec_carries_realtime_logins_beside_the_member_logins() {
        let tenant = NeonId::from(Uuid::from_u128(1));
        let timeline = NeonId::from(Uuid::from_u128(2));
        let v = config_response(&inputs_for_tests(&tenant, &timeline));
        assert_eq!(
            v["spec"]["realtime_logins"],
            json!({
                "allowed_cidrs": ["10.213.4.12/32", "10.213.4.11/32"],
                "client_cns": ["chelabase-realtime"],
            })
        );
        assert_eq!(
            v["spec"]["member_logins"]["client_cns"],
            json!(["chelabase-orchestrator"]),
            "the member block is untouched"
        );
        // An empty list is "no address rule" (spec §10): sent as an empty list, never omitted,
        // and the CN stays.
        let v = config_response(&SpecInputs {
            realtime_logins: MemberLogins::realtime(Vec::new()),
            ..inputs_for_tests(&tenant, &timeline)
        });
        assert_eq!(
            v["spec"]["realtime_logins"],
            json!({"allowed_cidrs": [], "client_cns": ["chelabase-realtime"]})
        );
    }

    #[tokio::test]
    async fn endpoint_config_takes_both_login_rules_from_the_policy() {
        let placement = FakePlacement {
            pageserver: Ok("postgresql://no_user@ps:6400".into()),
        };
        let member = MemberLogins::orchestrator(vec!["10.213.0.10/32".into()]);
        let realtime = MemberLogins::realtime(vec!["10.213.0.12/32".into()]);
        let v = endpoint_config(
            &placement,
            endpoint(TENANT),
            vec![],
            vec![],
            json!({}),
            None,
            &policy(&member, &realtime, true, false),
            &test_signer(),
        )
        .await
        .unwrap();
        assert_eq!(
            v["spec"]["member_logins"],
            json!({"allowed_cidrs": ["10.213.0.10/32"], "client_cns": ["chelabase-orchestrator"]})
        );
        assert_eq!(
            v["spec"]["realtime_logins"],
            json!({"allowed_cidrs": ["10.213.0.12/32"], "client_cns": ["chelabase-realtime"]})
        );
        assert_eq!(v["spec"]["branch_protected"], json!(true));
    }

    #[test]
    fn realtime_login_cidrs_parse_like_the_member_ones() {
        let err = parse_realtime_login_cidrs(None).unwrap_err().to_string();
        assert!(err.contains("CHELABASE_REALTIME_LOGIN_CIDRS"), "{err}");
        assert!(!err.contains("MEMBER"), "{err}");
        assert_eq!(parse_realtime_login_cidrs(Some("")).unwrap(), Vec::<String>::new());
        assert_eq!(parse_realtime_login_cidrs(Some("  ")).unwrap(), Vec::<String>::new());
        assert_eq!(
            parse_realtime_login_cidrs(Some(" 10.213.4.12/32, 10.213.4.11/32 ")).unwrap(),
            ["10.213.4.12/32", "10.213.4.11/32"]
        );
        assert_eq!(parse_realtime_login_cidrs(Some("fd00::1/128")).unwrap(), ["fd00::1/128"]);
        assert_eq!(parse_realtime_login_cidrs(Some("0.0.0.0/0")).unwrap(), ["0.0.0.0/0"]);
        for bad in [
            "10.0.0.1/+32",
            "10.0.0.1/",
            "/32",
            "10.0.0.1",
            "10.0.0.1/33",
            "fd00::1/129",
            "10.0.0.1/32/1",
            ",10.0.0.1/32",
            "10.0.0.1/32,",
            "10.0.0.1/32 # c",
            "x",
        ] {
            let err = parse_realtime_login_cidrs(Some(bad)).unwrap_err();
            assert!(
                format!("{err:#}").contains("CHELABASE_REALTIME_LOGIN_CIDRS"),
                "{bad:?}: {err:#}"
            );
        }
    }

    // The two parsers agree on every input (same grammar), only the variable's name differs.
    #[test]
    fn the_realtime_and_member_cidr_parsers_accept_the_same_inputs() {
        for raw in [
            None,
            Some(""),
            Some("10.0.0.1/32"),
            Some("10.0.0.1/32,fd00::/8"),
            Some("10.0.0.1/33"),
            Some("a,b"),
            Some(" , "),
        ] {
            assert_eq!(
                parse_realtime_login_cidrs(raw).ok(),
                parse_member_login_cidrs(raw).ok(),
                "{raw:?}"
            );
        }
    }

    // S21: pg_partman 5.5 requires `pg_partman_bgw.role`; the branch owner role.
    #[test]
    fn pg_partman_bgw_role_is_the_owner_role() {
        let tenant = NeonId::from(Uuid::from_u128(1));
        let timeline = NeonId::from(Uuid::from_u128(2));
        let v = config_response(&inputs_for_tests(&tenant, &timeline));
        let settings = v["spec"]["cluster"]["settings"].as_array().unwrap();
        let found: Vec<&Value> = settings
            .iter()
            .filter(|s| s["name"] == "pg_partman_bgw.role")
            .collect();
        assert_eq!(found.len(), 1, "{settings:?}");
        assert_eq!(
            found[0],
            &json!({"name": "pg_partman_bgw.role", "value": "postgres", "vartype": "string"})
        );
        // Only a placeholder: the library is not preloaded, so the worker stays off.
        let preload = settings.iter().find(|s| s["name"] == "shared_preload_libraries");
        assert_eq!(preload.unwrap()["value"], "neon");
    }

    // SP3: `endpoint_config` reads the clock itself, so the token changes per mint and the test
    // asserts the claims, not a fixed token.
    #[tokio::test]
    async fn an_expiring_policy_mints_a_24h_tenant_token_from_the_clock() {
        let placement = FakePlacement {
            pageserver: Ok("postgresql://no_user@ps:6400".into()),
        };
        let signer = test_signer();
        let (member, realtime) = (logins(), realtime_logins());
        let before = now_secs();
        let v = endpoint_config(
            &placement,
            endpoint(TENANT),
            vec![],
            vec![],
            json!({}),
            None,
            &policy(&member, &realtime, false, true),
            &signer,
        )
        .await
        .unwrap();
        let after = now_secs();
        let claims = payload(v["spec"]["storage_auth_token"].as_str().unwrap());
        assert_eq!(claims["scope"], "tenant");
        assert_eq!(claims["tenant_id"], TENANT);
        let (iat, exp) = (claims["iat"].as_u64().unwrap(), claims["exp"].as_u64().unwrap());
        assert_eq!(exp - iat, 86_400);
        assert!((before..=after).contains(&iat), "{before} <= {iat} <= {after}");
        let mut keys: Vec<_> = claims.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["exp", "iat", "scope", "tenant_id"]);
    }

    #[tokio::test]
    async fn a_non_expiring_policy_keeps_todays_tenant_token() {
        let placement = FakePlacement {
            pageserver: Ok("postgresql://no_user@ps:6400".into()),
        };
        let signer = test_signer();
        let (member, realtime) = (logins(), realtime_logins());
        let v = endpoint_config(
            &placement,
            endpoint(TENANT),
            vec![],
            vec![],
            json!({}),
            None,
            &policy(&member, &realtime, false, false),
            &signer,
        )
        .await
        .unwrap();
        let token = v["spec"]["storage_auth_token"].as_str().unwrap();
        assert_eq!(payload(token), json!({"tenant_id": TENANT, "scope": "tenant"}));
        // For a compute that never refreshes (an older image) the token is byte for byte today's.
        let tenant: NeonId = TENANT.parse().unwrap();
        assert_eq!(token, signer.storage_token(&StorageScope::Tenant(tenant)).unwrap());
    }
}
