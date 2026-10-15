//! U15 (fork round 3, f5b). Invariant: every connection Chelabase opens to a compute uses TLS
//! verified against the platform CA. Rest's queries as `authenticator` go over a pool whose
//! connections dial the compute's default-network IP with the certificate's DNS name
//! (`ConnInfo.server_name`, sent by the orchestrator next to `host`) as the name to verify,
//! against the CA file rest has (`ServerTls`). A failure to verify is a typed TLS error
//! (`tokio_postgres::Error::is_tls`) in `AcquireError::Db`, which rest answers as it does any
//! unreachable compute (503); a target that can't be verified is never dialled.

#[path = "../../../crates/chelabase-query/tests/common/tls_pg.rs"]
mod tls_pg;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chelabase_query::server_tls::ServerTls;
use deadpool_postgres::PoolError;
use rest::conn::{AcquireError, ConnError, ConnInfo, ConnSource, DbAcquireError, Pools};
use tls_pg::{Event, TlsPg, issue};
use uuid::Uuid;

const NAME_A: &str = "ep-a.compute.chelabase.internal";

/// What the orchestrator's `/connection` answers (JSON, so the wire shape is pinned too).
fn info(port: u16, server_name: Option<&str>) -> ConnInfo {
    let mut json = serde_json::json!({
        "host": std::net::Ipv4Addr::LOCALHOST.to_string(),
        "port": port,
        "database": "web1",
        "user": "authenticator",
        "password": "pw",
        "status": "ready",
        "settings": { "max_rows": 500, "statement_timeout_ms": 4000, "max_connections": 4 },
        "databases": [{ "name": "web1", "default_schema": "public" }],
    });
    if let Some(name) = server_name {
        json["server_name"] = serde_json::json!(name);
    }
    serde_json::from_value(json).unwrap()
}

struct Source {
    port: u16,
    server_name: Option<String>,
    fetches: Arc<AtomicUsize>,
}

impl ConnSource for Source {
    async fn fetch(&self, _branch: Uuid) -> Result<ConnInfo, ConnError> {
        self.fetches.fetch_add(1, Ordering::SeqCst);
        Ok(info(self.port, self.server_name.as_deref()))
    }
}

fn source(port: u16, server_name: Option<&str>) -> Source {
    Source {
        port,
        server_name: server_name.map(str::to_owned),
        fetches: Arc::default(),
    }
}

async fn acquire(pools: &Pools<Source>) -> Result<(), AcquireError> {
    pools.acquire(Uuid::new_v4(), "web1").await.map(|_| ())
}

fn is_tls(e: &AcquireError) -> bool {
    matches!(e, AcquireError::Db(DbAcquireError(PoolError::Backend(e))) if e.is_tls())
}

#[test]
fn conn_info_reads_the_server_name() {
    assert_eq!(info(5432, Some(NAME_A)).server_name.as_deref(), Some(NAME_A));
    assert_eq!(info(5432, None).server_name, None);
}

#[tokio::test]
async fn the_pool_connects_with_tls_verified_against_the_ca() {
    let issued = issue("ep-a");
    let server = TlsPg::start(Some((&issued.cert_pem, &issued.key_pem)));
    let ca = ServerTls::from_ca_pem(&issued.ca_pem).unwrap();
    let pools = Pools::with_server_tls(source(server.port, Some(NAME_A)), 10, ca);
    // The fake closes after authenticating, so the outcome of acquire isn't the point.
    let _ = acquire(&pools).await;
    let events = server.after_connections(1);
    assert!(
        events.contains(&Event::HandshakeOk) && events.contains(&Event::TlsStartup),
        "TLS up and verified before the startup message: {events:?}"
    );
    assert!(!events.contains(&Event::PlaintextStartup), "{events:?}");
}

#[tokio::test]
async fn a_wrong_ca_a_wrong_name_and_a_server_without_tls_are_typed_tls_errors() {
    let issued = issue("ep-a");
    let right = || ServerTls::from_ca_pem(&issued.ca_pem).unwrap();

    // Wrong CA: rest has another platform's CA.
    let server = TlsPg::start(Some((&issued.cert_pem, &issued.key_pem)));
    let other = ServerTls::from_ca_pem(&issue("ep-a").ca_pem).unwrap();
    let pools = Pools::with_server_tls(source(server.port, Some(NAME_A)), 10, other);
    let err = acquire(&pools).await.expect_err("a certificate from another CA");
    assert!(is_tls(&err), "wrong CA: {err:?}");

    // Wrong name: the orchestrator named another endpoint than the certificate's.
    let name = "ep-b.compute.chelabase.internal";
    let pools = Pools::with_server_tls(source(server.port, Some(name)), 10, right());
    let err = acquire(&pools).await.expect_err("a certificate for another name");
    assert!(is_tls(&err), "wrong name: {err:?}");

    // Each attempt (the first and the one retry after fetching the info again) was refused at
    // the handshake: no startup message, no password, on any of them.
    let events = server.after_connections(4);
    assert!(
        !events.contains(&Event::TlsStartup) && !events.contains(&Event::PlaintextStartup),
        "{events:?}"
    );
    assert!(!events.contains(&Event::HandshakeOk), "{events:?}");

    // A compute without TLS.
    let plain = TlsPg::start(None);
    let pools = Pools::with_server_tls(source(plain.port, Some(NAME_A)), 10, right());
    let err = acquire(&pools).await.expect_err("a compute without TLS");
    assert!(is_tls(&err), "no TLS: {err:?}");
    let events = plain.after_connections(2);
    assert!(events.contains(&Event::SslRequest), "TLS asked for first: {events:?}");
    assert!(!events.contains(&Event::PlaintextStartup), "never plaintext: {events:?}");
}

// No name, no connection: info without `server_name` can't be verified, so it isn't dialled
// (not in plaintext, not in TLS without a name check).
#[tokio::test]
async fn info_without_a_name_is_refused_before_any_connection() {
    let issued = issue("ep-a");
    let server = TlsPg::start(Some((&issued.cert_pem, &issued.key_pem)));
    let ca = ServerTls::from_ca_pem(&issued.ca_pem).unwrap();
    let pools = Pools::with_server_tls(source(server.port, None), 10, ca);
    let err = acquire(&pools).await.expect_err("nothing to verify the compute against");
    assert!(
        matches!(&err, AcquireError::Conn(ConnError::Unreachable(m)) if m.contains("TLS")),
        "{err:?}"
    );
    assert_eq!(server.first_packets(), 0, "nothing was dialled");
}

// `Pools::new` (no CA; the fakes' plaintext computes) can't verify a named target and doesn't
// fall back to plaintext for it.
#[tokio::test]
async fn a_pool_without_a_ca_refuses_a_named_target() {
    let issued = issue("ep-a");
    let server = TlsPg::start(Some((&issued.cert_pem, &issued.key_pem)));
    let pools = Pools::new(source(server.port, Some(NAME_A)), 10);
    let err = acquire(&pools).await.expect_err("no CA to verify against");
    assert!(
        matches!(&err, AcquireError::Conn(ConnError::Unreachable(m)) if m.contains("TLS")),
        "{err:?}"
    );
    assert_eq!(server.first_packets(), 0, "nothing was dialled");
}

// The orchestrator moved the compute behind another name (a reset or a restore makes a new
// endpoint): the pool built for the old name is replaced, and the new name is verified on its
// own connections.
#[tokio::test]
async fn a_new_server_name_is_a_new_pool() {
    struct Moving {
        port: u16,
        name: Arc<std::sync::Mutex<String>>,
    }
    impl ConnSource for Moving {
        async fn fetch(&self, _branch: Uuid) -> Result<ConnInfo, ConnError> {
            Ok(info(self.port, Some(&self.name.lock().unwrap())))
        }
    }
    let issued = issue("ep-a");
    let server = TlsPg::start(Some((&issued.cert_pem, &issued.key_pem)));
    let ca = ServerTls::from_ca_pem(&issued.ca_pem).unwrap();
    let name = Arc::new(std::sync::Mutex::new(NAME_A.to_owned()));
    let source = Moving { port: server.port, name: name.clone() };
    let pools = Pools::with_server_tls(source, 10, ca);
    let branch = Uuid::new_v4();
    let _ = pools.acquire(branch, "web1").await;
    *name.lock().unwrap() = "ep-b.compute.chelabase.internal".to_owned();
    pools.invalidate(branch).await;
    let err = pools
        .acquire(branch, "web1")
        .await
        .map(|_| ())
        .expect_err("the second name isn't the certificate's");
    assert!(is_tls(&err), "{err:?}");
}
