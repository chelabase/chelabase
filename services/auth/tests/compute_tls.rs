//! U15 (fork round 3, f5b). Invariant: every connection Chelabase opens to a compute uses TLS
//! verified against the platform CA. The auth service's app-users sync target comes from the
//! orchestrator with the compute's default-network IP and the certificate's DNS name
//! (`sync.server_name`); the service verifies the compute against the CA file it has
//! (`CHELABASE_COMPUTE_CA_FILE`, only `ca.crt` mounted, never `ca.key`). The connection
//! behaviour itself (accept, wrong CA, wrong name, no TLS, no name) is tested on `PgUsersDb`
//! in `crates/chelabase-query/tests/server_tls.rs`; this file pins what is auth's own.

use auth::app_users::AppBranchInfo;
use auth::config::AppConfig;

fn vars(extra: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let mut map: std::collections::HashMap<String, String> = [
        ("CHELABASE_APP_JWT_PRIVATE_KEY", "/k/app-jwt.key"),
        ("CHELABASE_APP_JWT_PUBLIC_KEY", "/k/app-jwt.pub"),
        ("CHELABASE_INTERNAL_VERIFY_KEY", "/k/internal.pub"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    for (k, v) in extra {
        map.insert(k.to_string(), v.to_string());
    }
    move |name| map.get(name).cloned()
}

#[test]
fn the_ca_file_is_read_from_the_environment() {
    let c = AppConfig::from_vars(vars(&[("CHELABASE_COMPUTE_CA_FILE", "/k/compute-ca/ca.crt")]))
        .unwrap()
        .expect("app users are on");
    assert_eq!(c.compute_ca, std::path::PathBuf::from("/k/compute-ca/ca.crt"));
}

// Without the CA file app users stay off (as with the other missing inputs): the sync never
// starts with nothing to verify a compute against, and never falls back to plaintext.
#[test]
fn app_users_are_off_without_the_ca_file() {
    assert_eq!(AppConfig::from_vars(vars(&[])).unwrap(), None);
    assert_eq!(
        AppConfig::from_vars(vars(&[("CHELABASE_COMPUTE_CA_FILE", "  ")])).unwrap(),
        None,
        "blank is unset"
    );
}

// The orchestrator's branch info carries the name through to the sync target.
#[test]
fn branch_info_carries_the_sync_servers_name() {
    let info: AppBranchInfo = serde_json::from_value(serde_json::json!({
        "project_id": uuid::Uuid::nil(),
        "status": "enabled",
        "email_verification": false,
        "on_delete": "delete",
        "auth_database_id": uuid::Uuid::nil(),
        "pg_database": "shop",
        "schema": "public",
        "table": "users",
        "target_branch_id": uuid::Uuid::nil(),
        "sync": {
            "branch_id": uuid::Uuid::nil(),
            "host": "172.18.0.7",
            "port": 5432,
            "database": "shop",
            "server_name": "ep-main.compute.chelabase.internal",
            "user": "chelabase_auth_sync",
            "password": "pw",
            "schema": "public",
            "table": "users",
        },
    }))
    .unwrap();
    let sync = info.sync.expect("a sync target");
    assert_eq!(sync.host, "172.18.0.7");
    assert_eq!(sync.server_name.as_deref(), Some("ep-main.compute.chelabase.internal"));
}
