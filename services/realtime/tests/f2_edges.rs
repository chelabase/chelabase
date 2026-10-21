//! Fork round 4, final fix round (F2, part 3): a replication start that fails as unavailable
//! (no usable address in the cached route answer) or can't connect drops the branch's cached
//! connection-route answer, so the next start asks the route again (a restarted compute has a
//! new address); and a replication target's `Debug` never shows the password.

use std::net::{Ipv4Addr, TcpListener};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use axum::{Json, Router, routing::get};
use chelabase_neon::compute_tls::{ComputeCa, REALTIME_CN, server_name, write_client_pair};
use realtime::client_tls::ClientPair;
use realtime::decoder::ReplicationTarget;
use realtime::pool::ConnLookup;
use realtime::slots::{PgReplicator, Replicator, SlotError, SlotTarget};
use serde_json::{Value, json};
use uuid::Uuid;

/// A connection route answering `answer` for every branch; how many times it was asked.
async fn route(answer: Value) -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let seen = hits.clone();
    let app = Router::new().route(
        "/internal/data/branches/{id}/connection",
        get(move || {
            let (answer, seen) = (answer.clone(), seen.clone());
            async move {
                seen.fetch_add(1, Ordering::SeqCst);
                Json(answer)
            }
        }),
    );
    let l = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (base, hits)
}

/// A loaded realtime client pair.
fn pair() -> (tempfile::TempDir, Arc<ClientPair>) {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let client = ca.issue_client(REALTIME_CN, now).unwrap();
    write_client_pair(dir.path(), &client, ca.cert_pem(), now).unwrap();
    let pair = Arc::new(ClientPair::new(dir.path().to_path_buf()));
    assert!(pair.get().is_some());
    (dir, pair)
}

/// Two starts on the same branch through one lookup: both fail, and the route's hit count.
async fn two_failed_starts(answer: Value) -> usize {
    let (_dir, pair) = pair();
    let (base, hits) = route(answer).await;
    let r = PgReplicator::new(Arc::new(ConnLookup::new(&base, "t")), pair, "pw".into());
    let target = SlotTarget {
        branch_id: Uuid::new_v4(),
        pg_database: "app".into(),
        db_oid: 1,
    };
    for attempt in 1..=2 {
        let err =
            match tokio::time::timeout(Duration::from_secs(20), r.start(&target, "rt_x")).await {
                Ok(Ok(_)) => panic!("start {attempt}: expected a failure"),
                Ok(Err(e)) => e,
                Err(_) => panic!("start {attempt} took over 20 s"),
            };
        assert!(
            matches!(err, SlotError::Unavailable(_)),
            "start {attempt}: {err:?}"
        );
    }
    hits.load(Ordering::SeqCst)
}

// The cached answer has no control-network address (the compute was restarting): the next
// start must not reuse it for the rest of the cache's life.
#[tokio::test(flavor = "multi_thread")]
async fn an_unavailable_start_drops_the_cached_route_answer() {
    let hits = two_failed_starts(json!({
        "host": "10.0.0.5", "port": 5432, "server_name": server_name("ep-f2"),
        "replication_host": null, "user": "authenticator", "password": "pw",
    }))
    .await;
    assert_eq!(hits, 2, "the second start asked the route again");
}

// The cached address doesn't answer (the compute moved): the next start asks the route again.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_connect_drops_the_cached_route_answer() {
    let closed = {
        let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        l.local_addr().unwrap().port()
    };
    let hits = two_failed_starts(json!({
        "host": Ipv4Addr::LOCALHOST.to_string(), "port": closed, "server_name": server_name("ep-f2"),
        "replication_host": Ipv4Addr::LOCALHOST.to_string(), "user": "authenticator", "password": "pw",
    }))
    .await;
    assert_eq!(hits, 2, "the second start asked the route again");
}

#[test]
fn a_replication_targets_debug_never_shows_the_password() {
    let target = ReplicationTarget {
        host: "10.213.4.20".into(),
        port: 5432,
        server_name: Some(server_name("ep-f2")),
        database: "app".into(),
        password: "s3cret-rt-password".into(),
    };
    for shown in [format!("{target:?}"), format!("{target:#?}")] {
        assert!(!shown.contains("s3cret-rt-password"), "{shown}");
        assert!(
            shown.contains("10.213.4.20"),
            "the rest is still shown: {shown}"
        );
    }
}
