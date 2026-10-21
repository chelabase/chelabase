//! Fork round 4, final fix round (F2): the coder's edge cases beyond the RED set. Every
//! "unavailable" answer built from the cached route answer drops it (no certificate name, an
//! address that isn't an IP), and the next start asks the route again.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use axum::{Json, Router, routing::get};
use chelabase_neon::compute_tls::{ComputeCa, REALTIME_CN, server_name, write_client_pair};
use realtime::client_tls::ClientPair;
use realtime::pool::ConnLookup;
use realtime::slots::{PgReplicator, Replicator, SlotError, SlotTarget};
use serde_json::{Value, json};
use uuid::Uuid;

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

async fn hits_after_two_starts(answer: Value) -> usize {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let client = ca.issue_client(REALTIME_CN, now).unwrap();
    write_client_pair(dir.path(), &client, ca.cert_pem(), now).unwrap();
    let pair = Arc::new(ClientPair::new(dir.path().to_path_buf()));
    let (base, hits) = route(answer).await;
    let r = PgReplicator::new(Arc::new(ConnLookup::new(&base, "t")), pair, "pw".into());
    let target = SlotTarget {
        branch_id: Uuid::new_v4(),
        pg_database: "app".into(),
        db_oid: 1,
    };
    for attempt in 1..=2 {
        let result = tokio::time::timeout(Duration::from_secs(20), r.start(&target, "rt_x"))
            .await
            .unwrap_or_else(|_| panic!("start {attempt} took over 20 s"));
        assert!(
            matches!(result, Err(SlotError::Unavailable(_))),
            "start {attempt}"
        );
    }
    hits.load(Ordering::SeqCst)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cached_answer_without_a_certificate_name_is_dropped() {
    let hits = hits_after_two_starts(json!({
        "host": "10.0.0.5", "port": 5432, "server_name": null,
        "replication_host": "10.213.4.20", "user": "authenticator", "password": "pw",
    }))
    .await;
    assert_eq!(hits, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cached_answer_whose_address_is_not_an_ip_is_dropped() {
    let hits = hits_after_two_starts(json!({
        "host": "10.0.0.5", "port": 5432, "server_name": server_name("ep-f2"),
        "replication_host": "compute.internal", "user": "authenticator", "password": "pw",
    }))
    .await;
    assert_eq!(hits, 2);
}
