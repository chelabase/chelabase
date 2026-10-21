//! Fork round 4, C4 (W31, SP9, SP11): realtime's replication session dials the connection
//! route's `replication_host` (the compute's address on the `control` network) with the
//! realtime client certificate, TLS verified as the compute's name (`verify-full`). A missing
//! `replication_host` is `SlotError::Unavailable`, never a fallback to `host`; the check pools
//! keep `host`; the client pair is reloaded every 60 s.
//!
//! The compute is a fake Postgres that offers TLS, demands a client certificate signed by the
//! platform CA and records what each connection presented.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use axum::Json;
use axum::Router;
use axum::routing::get;
use chelabase_neon::compute_tls::{ComputeCa, REALTIME_CN, server_name, write_client_pair};
use chelabase_query::server_tls::ServerTls;
use realtime::client_tls::{ClientPair, RELOAD_EVERY};
use realtime::pool::{ConnInfo, ConnLookup, connect_config};
use realtime::slots::{PgReplicator, Replicator, SlotError, SlotTarget};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde_json::{Value, json};
use uuid::Uuid;

const EP: &str = "ep-c4";
const PASSWORD: &str = "rt-pw";

// --- the route's field -----------------------------------------------------------------------

#[test]
fn the_lookup_type_takes_an_optional_replication_host() {
    let base = json!({
        "host": "10.0.0.5", "port": 5432, "server_name": "ep.compute.chelabase.internal",
        "user": "authenticator", "password": "pw",
    });
    let with = |extra: Value| {
        let mut v = base.clone();
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        serde_json::from_value::<ConnInfo>(v).unwrap()
    };
    assert_eq!(
        with(json!({ "replication_host": "10.213.4.20" }))
            .replication_host
            .as_deref(),
        Some("10.213.4.20")
    );
    assert_eq!(
        with(json!({ "replication_host": null })).replication_host,
        None
    );
    // `#[serde(default)]`: a route without the field (an older orchestrator) is still read.
    let without: ConnInfo = serde_json::from_value(base).unwrap();
    assert_eq!(without.replication_host, None);
}

#[test]
fn the_check_pools_keep_host() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let tls = ServerTls::from_ca_pem(ca.cert_pem()).unwrap();
    let info = ConnInfo {
        host: "10.0.0.5".into(),
        port: 5432,
        server_name: Some(server_name(EP)),
        replication_host: Some("10.213.4.20".into()),
        user: "authenticator".into(),
        password: "pw".into(),
    };
    let config = connect_config(&info, "app", Some(&tls)).unwrap();
    let addrs: Vec<String> = config
        .get_hostaddrs()
        .iter()
        .map(|a| a.to_string())
        .collect();
    assert_eq!(
        addrs,
        ["10.0.0.5"],
        "the pools dial the compute-network address"
    );
    // And the authenticator login carries no replication mode.
    assert_eq!(config.get_user(), Some("authenticator"));
}

// --- the fake compute ------------------------------------------------------------------------

/// What one connection did.
#[derive(Clone, Debug, Default)]
struct Seen {
    /// The TLS handshake finished.
    handshake_ok: bool,
    /// The first certificate the client presented (DER).
    client_cert: Option<Vec<u8>>,
    /// The server name the client asked for (SNI).
    sni: Option<String>,
}

struct FakeCompute {
    port: u16,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl FakeCompute {
    /// Listens on `127.0.0.1`. It offers `server_cert`/`server_key` (PEM) and demands a client
    /// certificate signed by `client_ca_pem`.
    fn start(server_cert: &str, server_key: &str, client_ca_pem: &str) -> Self {
        let chain: Vec<CertificateDer<'static>> =
            CertificateDer::pem_slice_iter(server_cert.as_bytes())
                .collect::<Result<_, _>>()
                .unwrap();
        let key = PrivateKeyDer::from_pem_slice(server_key.as_bytes()).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        for ca in CertificateDer::pem_slice_iter(client_ca_pem.as_bytes()) {
            roots.add(ca.unwrap()).unwrap();
        }
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
            Arc::new(roots),
            provider.clone(),
        )
        .build()
        .unwrap();
        let config = Arc::new(
            rustls::ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_client_cert_verifier(verifier)
                .with_single_cert(chain, key)
                .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let (config, log) = (config.clone(), log.clone());
                std::thread::spawn(move || {
                    let mut one = Seen::default();
                    let _ = serve(stream, config, &mut one);
                    log.lock().unwrap().push(one);
                });
            }
        });
        FakeCompute { port, seen }
    }

    /// Waits for `n` finished connections (at most 10 s).
    fn after(&self, n: usize) -> Vec<Seen> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let seen = self.seen.lock().unwrap().clone();
            if seen.len() >= n || Instant::now() > deadline {
                return seen;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The connections so far, after giving a wrongly placed dial time to arrive.
    fn none_came(&self) -> bool {
        std::thread::sleep(Duration::from_millis(300));
        self.seen.lock().unwrap().is_empty()
    }
}

const SSL_REQUEST: [u8; 8] = [0, 0, 0, 8, 0x04, 0xd2, 0x16, 0x2f];

fn serve(
    mut tcp: std::net::TcpStream,
    config: Arc<rustls::ServerConfig>,
    seen: &mut Seen,
) -> std::io::Result<()> {
    tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut first = [0u8; 8];
    tcp.read_exact(&mut first)?;
    if first != SSL_REQUEST {
        return Ok(());
    }
    tcp.write_all(b"S")?;
    let mut conn = rustls::ServerConnection::new(config).map_err(std::io::Error::other)?;
    while conn.is_handshaking() {
        conn.complete_io(&mut tcp)?;
    }
    seen.handshake_ok = true;
    seen.sni = conn.server_name().map(str::to_string);
    seen.client_cert = conn
        .peer_certificates()
        .and_then(|c| c.first())
        .map(|c| c.as_ref().to_vec());
    // Hang up: the replication session then fails as "closed", after the TLS checks.
    let _ = tcp.shutdown(std::net::Shutdown::Both);
    Ok(())
}

fn der_of(cert_pem: &str) -> Vec<u8> {
    CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .next()
        .unwrap()
        .unwrap()
        .as_ref()
        .to_vec()
}

// --- the orchestrator's route, as realtime reads it -------------------------------------------

/// Serves the connection route with this answer; returns the base URL.
async fn route(answer: Value) -> String {
    let app = Router::new().route(
        "/internal/data/branches/{id}/connection",
        get(move || {
            let answer = answer.clone();
            async move { Json(answer) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

struct World {
    ca: ComputeCa,
    dir: tempfile::TempDir,
    compute: FakeCompute,
    pair: Arc<ClientPair>,
    client_cert_pem: String,
}

/// A CA, the realtime pair in a temp dir, and a fake compute with a certificate for `EP` (or
/// `cert_for`'s name / another CA's, to be refused).
fn world(cert_for: &str, own_ca: bool) -> World {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let client = ca.issue_client(REALTIME_CN, now).unwrap();
    write_client_pair(dir.path(), &client, ca.cert_pem(), now).unwrap();
    let issuer = if own_ca {
        ca.clone()
    } else {
        ComputeCa::generate(now).unwrap()
    };
    let server = issuer.issue_server(cert_for, now).unwrap();
    let compute = FakeCompute::start(&server.cert_pem, &server.key_pem, ca.cert_pem());
    let pair = Arc::new(ClientPair::new(dir.path().to_path_buf()));
    World {
        ca,
        dir,
        compute,
        pair,
        client_cert_pem: client.cert_pem,
    }
}

fn target() -> SlotTarget {
    SlotTarget {
        branch_id: Uuid::new_v4(),
        pg_database: "app".into(),
        db_oid: 16384,
    }
}

/// A replicator whose route answers `host` (the compute-network address: refused to be dialled
/// in these tests, as nothing listens on `127.0.0.2`), `server_name` and `replication_host`.
async fn replicator(
    world: &World,
    server_name: Option<&str>,
    replication_host: Option<&str>,
) -> PgReplicator {
    let base = route(json!({
        "host": "127.0.0.2",
        "port": world.compute.port,
        "server_name": server_name,
        "replication_host": replication_host,
        "user": "authenticator",
        "password": "pw",
        "database": "app",
        "status": "ready",
    }))
    .await;
    PgReplicator::new(
        Arc::new(ConnLookup::new(&base, "rt-token")),
        world.pair.clone(),
        PASSWORD.to_string(),
    )
}

async fn start_err(r: &PgReplicator) -> SlotError {
    match tokio::time::timeout(Duration::from_secs(15), r.start(&target(), "rt_abc123")).await {
        Ok(Ok(_)) => panic!("the session was expected to fail"),
        Ok(Err(e)) => e,
        Err(_) => panic!("start took over 15 s"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_replicator_dials_replication_host_with_the_client_certificate() {
    let w = world(EP, true);
    // `host` is 127.0.0.2 (nothing listens there on this port); only `replication_host` works.
    let r = replicator(&w, Some(&server_name(EP)), Some("127.0.0.1")).await;
    // The fake hangs up after the handshake, so the session ends as unavailable.
    let _ = start_err(&r).await;
    let seen = w.compute.after(1);
    assert_eq!(seen.len(), 1, "one dial, at replication_host: {seen:?}");
    assert!(seen[0].handshake_ok, "{seen:?}");
    assert_eq!(
        seen[0].client_cert.as_deref(),
        Some(der_of(&w.client_cert_pem).as_slice()),
        "the realtime client certificate was presented"
    );
    assert_eq!(
        seen[0].sni.as_deref(),
        Some(server_name(EP).as_str()),
        "verified as the compute's name, not its address"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn no_replication_host_is_unavailable_and_never_falls_back_to_host() {
    let w = world(EP, true);
    // `host` here is the fake's own address: a fallback would reach it.
    let base = route(json!({
        "host": "127.0.0.1", "port": w.compute.port, "server_name": server_name(EP),
        "replication_host": null, "user": "authenticator", "password": "pw",
    }))
    .await;
    let r = PgReplicator::new(
        Arc::new(ConnLookup::new(&base, "t")),
        w.pair.clone(),
        PASSWORD.into(),
    );
    assert!(matches!(start_err(&r).await, SlotError::Unavailable(_)));
    // The same when the field is absent altogether (an older orchestrator).
    let base = route(json!({
        "host": "127.0.0.1", "port": w.compute.port, "server_name": server_name(EP),
        "user": "authenticator", "password": "pw",
    }))
    .await;
    let r = PgReplicator::new(
        Arc::new(ConnLookup::new(&base, "t")),
        w.pair.clone(),
        PASSWORD.into(),
    );
    assert!(matches!(start_err(&r).await, SlotError::Unavailable(_)));
    assert!(w.compute.none_came(), "nothing was dialled");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_certificate_for_another_name_is_refused() {
    let w = world("ep-other", true);
    let r = replicator(&w, Some(&server_name(EP)), Some("127.0.0.1")).await;
    let _ = start_err(&r).await;
    let seen = w.compute.after(1);
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(
        !seen[0].handshake_ok,
        "verify-full: the name must match: {seen:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_certificate_from_another_ca_is_refused() {
    let w = world(EP, false);
    let r = replicator(&w, Some(&server_name(EP)), Some("127.0.0.1")).await;
    let _ = start_err(&r).await;
    let seen = w.compute.after(1);
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert!(!seen[0].handshake_ok, "{seen:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_certificate_name_there_is_no_plaintext_path() {
    let w = world(EP, true);
    let r = replicator(&w, None, Some("127.0.0.1")).await;
    assert!(matches!(start_err(&r).await, SlotError::Unavailable(_)));
    assert!(w.compute.none_came(), "nothing was dialled");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_loaded_pair_nothing_is_dialled() {
    let w = world(EP, true);
    let empty = tempfile::tempdir().unwrap();
    let pair = Arc::new(ClientPair::new(empty.path().to_path_buf()));
    assert!(pair.get().is_none(), "no pair yet");
    let base = route(json!({
        "host": "127.0.0.2", "port": w.compute.port, "server_name": server_name(EP),
        "replication_host": "127.0.0.1", "user": "authenticator", "password": "pw",
    }))
    .await;
    let r = PgReplicator::new(Arc::new(ConnLookup::new(&base, "t")), pair, PASSWORD.into());
    assert!(matches!(start_err(&r).await, SlotError::Unavailable(_)));
    assert!(w.compute.none_came(), "nothing was dialled");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_renewed_pair_is_used_by_the_next_session() {
    let w = world(EP, true);
    let r = replicator(&w, Some(&server_name(EP)), Some("127.0.0.1")).await;
    let _ = start_err(&r).await;
    let first = w.compute.after(1);
    assert_eq!(
        first[0].client_cert.as_deref(),
        Some(der_of(&w.client_cert_pem).as_slice())
    );

    let later = SystemTime::now() + Duration::from_secs(5);
    let renewed = w.ca.issue_client(REALTIME_CN, later).unwrap();
    write_client_pair(w.dir.path(), &renewed, w.ca.cert_pem(), later).unwrap();
    w.pair.refresh();
    let _ = start_err(&r).await;
    let second = w.compute.after(2);
    assert_eq!(second.len(), 2, "{second:?}");
    let presented: Vec<_> = second
        .iter()
        .filter_map(|s| s.client_cert.clone())
        .collect();
    assert!(
        presented.contains(&der_of(&renewed.cert_pem)),
        "the second session presents the renewed certificate"
    );
}

// --- the pair's reload ------------------------------------------------------------------------

fn write_pair(dir: &Path, ca: &ComputeCa, at: SystemTime) {
    let client = ca.issue_client(REALTIME_CN, at).unwrap();
    write_client_pair(dir, &client, ca.cert_pem(), at).unwrap();
}

#[test]
fn the_pair_is_checked_every_60_seconds() {
    assert_eq!(RELOAD_EVERY, Duration::from_secs(60));
}

#[test]
fn a_pair_that_appears_later_is_loaded_and_a_renewal_is_swapped_in() {
    let dir = tempfile::tempdir().unwrap();
    let pair = ClientPair::new(dir.path().to_path_buf());
    assert!(pair.get().is_none(), "the worker hasn't written one yet");
    pair.refresh();
    assert!(pair.get().is_none(), "still none, and no panic");
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    write_pair(dir.path(), &ca, now);
    pair.refresh();
    let loaded = pair.get().expect("loaded by refresh");
    let before = loaded.fingerprint();
    write_pair(dir.path(), &ca, now + Duration::from_secs(1));
    pair.refresh();
    assert_ne!(
        pair.get().unwrap().fingerprint(),
        before,
        "the renewed pair is swapped in"
    );
}

#[test]
fn a_broken_renewal_keeps_the_loaded_pair() {
    let dir = tempfile::tempdir().unwrap();
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    write_pair(dir.path(), &ca, now);
    let pair = ClientPair::new(dir.path().to_path_buf());
    let before = pair.get().expect("loaded").fingerprint();
    // `current` points at a version whose files are garbage.
    let bad = dir.path().join("vbad");
    std::fs::create_dir(&bad).unwrap();
    for f in ["client.crt", "client.key", "ca.crt"] {
        std::fs::write(bad.join(f), "garbage").unwrap();
    }
    let current = dir.path().join("current");
    std::fs::remove_file(&current).unwrap();
    std::os::unix::fs::symlink("vbad", &current).unwrap();
    pair.refresh();
    assert_eq!(pair.get().expect("still loaded").fingerprint(), before);
}

#[tokio::test(start_paused = true)]
async fn the_reload_task_swaps_a_renewed_pair_within_a_minute() {
    let dir = tempfile::tempdir().unwrap();
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    write_pair(dir.path(), &ca, now);
    let pair = Arc::new(ClientPair::new(dir.path().to_path_buf()));
    let before = pair.get().expect("loaded").fingerprint();
    pair.spawn_reload();
    // Let the task start (and take a first tick, if it takes one at once) before the renewal.
    for _ in 0..20 {
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(10));
    }
    write_pair(dir.path(), &ca, now + Duration::from_secs(1));

    // Not yet: 30 s in, the 60 s task hasn't looked.
    tokio::time::advance(Duration::from_secs(30)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        pair.get().unwrap().fingerprint(),
        before,
        "not before the period is up"
    );

    // Within 60 s of the start it has: the swap happens on a blocking thread, so poll.
    tokio::time::advance(Duration::from_secs(31)).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while pair.get().unwrap().fingerprint() == before {
        assert!(
            Instant::now() < deadline,
            "the renewed pair was never swapped in"
        );
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(10));
    }
}
