//! Fork round 4, C4: coder-added edges for the replication session's address. The RED set
//! covers a missing `replication_host`; a value that isn't an IP must not make the driver
//! resolve a name (it would dial whatever the certificate name or the hostname resolves to).

use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::{Json, Router, routing::get};
use chelabase_neon::compute_tls::{ComputeCa, REALTIME_CN, server_name, write_client_pair};
use realtime::client_tls::ClientPair;
use realtime::pool::ConnLookup;
use realtime::slots::{PgReplicator, Replicator, SlotError, SlotTarget};
use serde_json::json;
use uuid::Uuid;

#[tokio::test(flavor = "multi_thread")]
async fn a_replication_host_that_is_not_an_ip_is_unavailable_and_never_dialled() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let client = ca.issue_client(REALTIME_CN, now).unwrap();
    write_client_pair(dir.path(), &client, ca.cert_pem(), now).unwrap();
    let pair = Arc::new(ClientPair::new(dir.path().to_path_buf()));
    assert!(pair.get().is_some());

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    for host in ["localhost", "", "10.0.0.1:5432", "ep.compute.chelabase.internal"] {
        let answer = json!({
            "host": "127.0.0.1", "port": port, "server_name": server_name("ep-e"),
            "replication_host": host, "user": "authenticator", "password": "pw",
        });
        let app = Router::new().route(
            "/internal/data/branches/{id}/connection",
            get(move || {
                let answer = answer.clone();
                async move { Json(answer) }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        let r = PgReplicator::new(Arc::new(ConnLookup::new(&base, "t")), pair.clone(), "pw".into());
        let target = SlotTarget {
            branch_id: Uuid::new_v4(),
            pg_database: "app".into(),
            db_oid: 1,
        };
        let err = match tokio::time::timeout(Duration::from_secs(15), r.start(&target, "rt_x")).await
        {
            Ok(Ok(_)) => panic!("{host:?}: expected a failure"),
            Ok(Err(e)) => e,
            Err(_) => panic!("{host:?}: start took over 15 s"),
        };
        assert!(matches!(err, SlotError::Unavailable(_)), "{host:?}: {err:?}");
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(listener.accept().is_err(), "nothing was dialled");
}
