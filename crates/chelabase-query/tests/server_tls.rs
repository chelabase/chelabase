//! U15 (fork round 3, f5b). Invariant: every connection Chelabase opens to a compute uses TLS
//! verified against the platform CA. This file covers the shared piece (`server_tls`) and the
//! auth service's app-users sync (`PgUsersDb`); rest's pool is in `services/rest/tests`.
//!
//! The compute's server certificate carries one DNS name,
//! `<endpoint_id>.compute.chelabase.internal`, never an IP, and the services dial the compute's
//! default-network IP: so the target carries the certificate name next to the IP, and the
//! connection is `host = <name>`, `hostaddr = <ip>`, `sslmode = require`, verified against the
//! CA file the service has (only `ca.crt`, never `ca.key`). No client certificate, and no path
//! back to plaintext: not on a refused SSLRequest, a wrong CA or a wrong name, nor for a target
//! that has no name to verify.

#[path = "common/tls_pg.rs"]
mod tls_pg;

use chelabase_query::app_users::{NewUserRow, PgUsersDb, SyncTarget, UserRowError, UsersDb};
use chelabase_query::server_tls::ServerTls;
use tls_pg::{Event, TlsPg, issue};

const NAME_A: &str = "ep-a.compute.chelabase.internal";

fn row() -> NewUserRow {
    NewUserRow {
        id: uuid::Uuid::nil(),
        email: "a@x.test".into(),
        email_verified: true,
        data: serde_json::Map::new(),
    }
}

/// A target as the orchestrator sends it (JSON, so the test pins the wire shape too):
/// the compute's IP in `host`, the certificate's name in `server_name`.
fn target(port: u16, server_name: Option<&str>) -> SyncTarget {
    let mut json = serde_json::json!({
        "branch_id": uuid::Uuid::nil(),
        "host": std::net::Ipv4Addr::LOCALHOST.to_string(),
        "port": port,
        "database": "d",
        "user": "chelabase_auth_sync",
        "password": "pw",
        "schema": "public",
        "table": "users",
    });
    if let Some(name) = server_name {
        json["server_name"] = serde_json::json!(name);
    }
    serde_json::from_value(json).unwrap()
}

async fn insert(db: &PgUsersDb, t: &SyncTarget) -> UserRowError {
    match db.begin_insert(t, &row()).await {
        Ok(_) => panic!("the fake server takes no statements: the insert can't succeed"),
        Err(e) => e,
    }
}

// ---- the wire shape ----

#[test]
fn the_server_name_is_on_the_wire_and_optional() {
    let t = target(5432, Some(NAME_A));
    assert_eq!(t.server_name.as_deref(), Some(NAME_A));
    let json = serde_json::to_value(&t).unwrap();
    assert_eq!(json["server_name"], serde_json::json!(NAME_A), "{json}");
    let back: SyncTarget = serde_json::from_value(json).unwrap();
    assert_eq!(back.server_name.as_deref(), Some(NAME_A));
    // An older orchestrator's answer has none.
    assert_eq!(target(5432, None).server_name, None);
}

// ---- ServerTls ----

#[test]
fn a_ca_that_isnt_a_certificate_is_refused() {
    assert!(ServerTls::from_ca_pem("").is_err());
    assert!(ServerTls::from_ca_pem("not pem").is_err());
    // A key is not a CA certificate.
    let issued = issue("ep-a");
    assert!(ServerTls::from_ca_pem(&issued.key_pem).is_err());
    assert!(ServerTls::from_ca_pem(&issued.ca_pem).is_ok());
}

#[test]
fn a_missing_ca_file_is_an_error_naming_the_path() {
    let err = ServerTls::from_ca_file(std::path::Path::new("/nonexistent/compute-ca/ca.crt"))
        .expect_err("no such file");
    assert!(err.to_string().contains("/nonexistent/compute-ca/ca.crt"), "{err}");
}

#[test]
fn configure_dials_the_ip_and_verifies_the_name() {
    let tls = ServerTls::from_ca_pem(&issue("ep-a").ca_pem).unwrap();
    let mut config = tokio_postgres::Config::new();
    tls.configure(&mut config, "172.18.0.7", NAME_A).unwrap();
    assert_eq!(config.get_hosts(), &[tokio_postgres::config::Host::Tcp(NAME_A.into())]);
    assert_eq!(config.get_hostaddrs(), &["172.18.0.7".parse::<std::net::IpAddr>().unwrap()]);
    assert_eq!(config.get_ssl_mode(), tokio_postgres::config::SslMode::Require);
    // A host name is never resolved and tried: the address must be an IP.
    let mut config = tokio_postgres::Config::new();
    assert!(tls.configure(&mut config, "compute.example", NAME_A).is_err());
    // And a name is required.
    assert!(tls.configure(&mut config, "172.18.0.7", "").is_err());
}

// ---- tokio-postgres through the connector: accept the right one, refuse the rest ----

async fn connect(port: u16, tls: &ServerTls, name: &str) -> Result<(), tokio_postgres::Error> {
    let mut config = tokio_postgres::Config::new();
    config.port(port).user("u").password("pw").dbname("d");
    tls.configure(&mut config, &std::net::Ipv4Addr::LOCALHOST.to_string(), name).unwrap();
    let (_client, _conn) = config.connect(tls.connector()).await?;
    Ok(())
}

#[tokio::test]
async fn the_connector_accepts_the_right_certificate_and_name() {
    let issued = issue("ep-a");
    let server = TlsPg::start(Some((&issued.cert_pem, &issued.key_pem)));
    let tls = ServerTls::from_ca_pem(&issued.ca_pem).unwrap();
    connect(server.port, &tls, NAME_A).await.expect("the right CA and name");
    let events = server.after_connections(1);
    assert!(events.contains(&Event::HandshakeOk) && events.contains(&Event::TlsStartup), "{events:?}");
}

#[tokio::test]
async fn the_connector_refuses_a_wrong_ca_a_wrong_name_and_a_plaintext_server() {
    let issued = issue("ep-a");
    let other_ca = issue("ep-a").ca_pem;
    let tls_ok = ServerTls::from_ca_pem(&issued.ca_pem).unwrap();
    let tls_other = ServerTls::from_ca_pem(&other_ca).unwrap();

    let server = TlsPg::start(Some((&issued.cert_pem, &issued.key_pem)));
    // Wrong CA: the certificate is the right one, signed by a CA the service doesn't have.
    assert!(connect(server.port, &tls_other, NAME_A).await.is_err());
    // Wrong name: the right CA, another endpoint's name (the cert carries only ep-a's).
    assert!(connect(server.port, &tls_ok, "ep-b.compute.chelabase.internal").await.is_err());
    let events = server.after_connections(2);
    assert!(!events.contains(&Event::TlsStartup), "no startup (no password) after a refusal: {events:?}");
    assert!(!events.contains(&Event::PlaintextStartup), "{events:?}");
    assert_eq!(events.iter().filter(|e| **e == Event::HandshakeFailed).count(), 2, "{events:?}");

    // A compute without TLS: refused, and the startup message never goes in plaintext.
    let plain = TlsPg::start(None);
    assert!(connect(plain.port, &tls_ok, NAME_A).await.is_err());
    let events = plain.after_connections(1);
    assert!(events.contains(&Event::SslRequest), "{events:?}");
    assert!(!events.contains(&Event::PlaintextStartup), "{events:?}");
}

// The floor is TLS 1.3, like the orchestrator's client: a compute that only speaks TLS 1.2 is
// refused at the handshake (a typed TLS error), by the connector and by the sync.
#[tokio::test]
async fn a_tls_12_only_server_is_refused() {
    let issued = issue("ep-a");
    let server = TlsPg::start_tls12_only(&issued.cert_pem, &issued.key_pem);
    let tls = ServerTls::from_ca_pem(&issued.ca_pem).unwrap();
    let err = connect(server.port, &tls, NAME_A).await.expect_err("TLS 1.2 is below the floor");
    assert!(err.is_tls(), "{err:?}");
    let db = PgUsersDb::with_server_tls(tls);
    let err = insert(&db, &target(server.port, Some(NAME_A))).await;
    assert!(matches!(err, UserRowError::Unavailable(_)), "{err:?}");
    let events = server.after_connections(2);
    assert!(!events.contains(&Event::HandshakeOk), "{events:?}");
    assert!(!events.contains(&Event::TlsStartup), "{events:?}");
}

// ---- the auth service's sync ----

#[tokio::test]
async fn the_sync_connects_with_tls_verified_against_the_ca() {
    let issued = issue("ep-a");
    let server = TlsPg::start(Some((&issued.cert_pem, &issued.key_pem)));
    let db = PgUsersDb::with_server_tls(ServerTls::from_ca_pem(&issued.ca_pem).unwrap());
    let err = insert(&db, &target(server.port, Some(NAME_A))).await;
    // The fake takes no statements; what matters is how far the connection got.
    assert!(matches!(err, UserRowError::Unavailable(_)), "{err:?}");
    drop(db);
    let events = server.after_connections(1);
    assert!(
        events.contains(&Event::HandshakeOk) && events.contains(&Event::TlsStartup),
        "TLS up and verified before the startup message: {events:?}"
    );
    assert!(!events.contains(&Event::PlaintextStartup), "{events:?}");
}

#[tokio::test]
async fn the_sync_refuses_a_wrong_ca_a_wrong_name_and_a_server_without_tls() {
    let issued = issue("ep-a");
    let wrong_ca = ServerTls::from_ca_pem(&issue("ep-a").ca_pem).unwrap();
    let server = TlsPg::start(Some((&issued.cert_pem, &issued.key_pem)));

    // Wrong CA.
    let db = PgUsersDb::with_server_tls(wrong_ca);
    let err = insert(&db, &target(server.port, Some(NAME_A))).await;
    assert!(matches!(err, UserRowError::Unavailable(_)), "{err:?}");
    // Wrong name (the certificate is another endpoint's), right CA.
    let db = PgUsersDb::with_server_tls(ServerTls::from_ca_pem(&issued.ca_pem).unwrap());
    let err = insert(&db, &target(server.port, Some("ep-b.compute.chelabase.internal"))).await;
    assert!(matches!(err, UserRowError::Unavailable(_)), "{err:?}");
    let events = server.after_connections(2);
    assert!(!events.contains(&Event::TlsStartup) && !events.contains(&Event::PlaintextStartup), "{events:?}");
    assert!(!events.contains(&Event::HandshakeOk), "{events:?}");

    // A compute without TLS.
    let plain = TlsPg::start(None);
    let err = insert(&db, &target(plain.port, Some(NAME_A))).await;
    assert!(matches!(err, UserRowError::Unavailable(_)), "{err:?}");
    let events = plain.after_connections(1);
    assert!(events.contains(&Event::SslRequest), "TLS asked for first: {events:?}");
    assert!(!events.contains(&Event::PlaintextStartup), "never a plaintext startup: {events:?}");
}

// A pool made for one name isn't used for another: the same host, port and password with a
// different certificate name is another pool (so a name that stops matching is refused, not
// served from a connection that once verified).
#[tokio::test]
async fn the_pool_is_keyed_by_the_certificate_name() {
    let issued = issue("ep-a");
    let server = TlsPg::start(Some((&issued.cert_pem, &issued.key_pem)));
    let db = PgUsersDb::with_server_tls(ServerTls::from_ca_pem(&issued.ca_pem).unwrap());
    let _ = insert(&db, &target(server.port, Some(NAME_A))).await;
    let _ = insert(&db, &target(server.port, Some("ep-b.compute.chelabase.internal"))).await;
    let events = server.after_connections(2);
    assert_eq!(
        events.iter().filter(|e| **e == Event::HandshakeFailed).count(),
        1,
        "the second name was checked on its own connection and refused: {events:?}"
    );
    assert_eq!(events.iter().filter(|e| **e == Event::TlsStartup).count(), 1, "{events:?}");
}

// No name, no connection: a target without `server_name` cannot be verified, so it isn't
// dialled at all (not in plaintext, not in TLS without a check).
#[tokio::test]
async fn a_target_without_a_name_is_refused_before_any_connection() {
    let issued = issue("ep-a");
    let server = TlsPg::start(Some((&issued.cert_pem, &issued.key_pem)));
    let db = PgUsersDb::with_server_tls(ServerTls::from_ca_pem(&issued.ca_pem).unwrap());
    let err = insert(&db, &target(server.port, None)).await;
    assert!(matches!(&err, UserRowError::Unavailable(m) if m.contains("TLS")), "{err:?}");
    assert_eq!(server.first_packets(), 0, "nothing was dialled");
}

// A service without a CA (`PgUsersDb::new`, for the fakes' plaintext targets) cannot verify a
// named target either, and doesn't fall back: it refuses a target that carries a name.
#[tokio::test]
async fn a_db_without_a_ca_refuses_a_named_target() {
    let issued = issue("ep-a");
    let server = TlsPg::start(Some((&issued.cert_pem, &issued.key_pem)));
    let db = PgUsersDb::new();
    let err = insert(&db, &target(server.port, Some(NAME_A))).await;
    assert!(matches!(&err, UserRowError::Unavailable(m) if m.contains("TLS")), "{err:?}");
    assert_eq!(server.first_packets(), 0, "nothing was dialled");
}

// A host name instead of an IP is refused before dialling, with TLS as with the orchestrator's
// own sync: it is never resolved and tried.
#[tokio::test]
async fn a_named_target_needs_an_ip_address() {
    let issued = issue("ep-a");
    let db = PgUsersDb::with_server_tls(ServerTls::from_ca_pem(&issued.ca_pem).unwrap());
    let mut t = target(5432, Some(NAME_A));
    t.host = "compute.example".into();
    let err = insert(&db, &t).await;
    assert!(
        matches!(&err, UserRowError::Unavailable(m) if m.contains("isn't an IP address")),
        "{err:?}"
    );
}
