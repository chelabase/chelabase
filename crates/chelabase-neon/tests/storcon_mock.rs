//! `StorconClient` against an in-process mock storage controller that answers from a
//! script and records every request.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode as AxumStatus;
use axum::response::{IntoResponse, Response};
use chelabase_neon::storcon::{LsnAtTime, LsnKind, TimelineInfo};
use chelabase_neon::{Lsn, NeonId, StorconClient, StorconError};
use reqwest::StatusCode;
use serde_json::{Value, json};

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    query: String,
    body: Value,
}

#[derive(Default)]
struct Script {
    replies: Mutex<VecDeque<(u16, &'static str)>>,
    seen: Mutex<Vec<Seen>>,
}

struct Mock {
    base: String,
    script: Arc<Script>,
}

impl Mock {
    fn seen(&self) -> Vec<Seen> {
        self.script.seen.lock().unwrap().clone()
    }
}

async fn serve(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

/// Answers requests in order with `replies` (status, JSON or text body); anything past the
/// end gets 500 "unexpected request".
async fn mock(replies: Vec<(u16, &'static str)>) -> Mock {
    let script = Arc::new(Script {
        replies: Mutex::new(replies.into()),
        seen: Mutex::default(),
    });
    let app = Router::new().fallback(answer).with_state(script.clone());
    Mock {
        base: serve(app).await,
        script,
    }
}

async fn answer(State(script): State<Arc<Script>>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    script.seen.lock().unwrap().push(Seen {
        method: parts.method.to_string(),
        path: parts.uri.path().to_string(),
        query: parts.uri.query().unwrap_or_default().to_string(),
        body,
    });
    let (status, reply) = script
        .replies
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or((500, "unexpected request"));
    (
        AxumStatus::from_u16(status).unwrap(),
        [("content-type", "application/json")],
        reply,
    )
        .into_response()
}

fn fast_deletes(client: StorconClient, attempts: u32) -> StorconClient {
    client.with_delete_poll(Duration::from_millis(1), attempts)
}

fn id(n: u128) -> NeonId {
    NeonId::from(uuid::Uuid::from_u128(n))
}

fn lsn(s: &str) -> Lsn {
    s.parse().unwrap()
}

#[tokio::test]
async fn create_tenant_sends_id_and_placement() {
    let m = mock(vec![(201, "{}")]).await;
    let tenant = id(1);
    StorconClient::new(&m.base, 1)
        .create_tenant(&tenant)
        .await
        .unwrap();
    let seen = m.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path, "/v1/tenant");
    assert_eq!(
        seen[0].body,
        json!({ "new_tenant_id": tenant.as_str(), "placement_policy": { "Attached": 1 } })
    );
}

#[tokio::test]
async fn create_timeline_fresh_and_branch() {
    let m = mock(vec![
        (
            201,
            r#"{"timeline_id":"x","ancestor_lsn":null,"last_record_lsn":"0/14EE0B8"}"#,
        ),
        (
            201,
            r#"{"ancestor_timeline_id":"00000000000000000000000000000002","ancestor_lsn":"0/14EE0B8","last_record_lsn":"0/14EE0B8"}"#,
        ),
        (
            201,
            r#"{"ancestor_lsn":"0/14EE0B8","last_record_lsn":"0/14EE0B8"}"#,
        ),
    ])
    .await;
    let client = StorconClient::new(&m.base, 0);
    let (tenant, main, dev, head) = (id(1), id(2), id(3), id(4));

    let fresh = client
        .create_timeline(&tenant, &main, 17, None)
        .await
        .unwrap();
    assert_eq!(
        fresh,
        TimelineInfo {
            ancestor_timeline_id: None,
            ancestor_lsn: None,
            last_record_lsn: lsn("0/14EE0B8"),
            applied_gc_cutoff_lsn: None,
        }
    );
    let branch = client
        .create_timeline(&tenant, &dev, 17, Some((&main, Some(lsn("0/14EE0B8")))))
        .await
        .unwrap();
    assert_eq!(branch.ancestor_lsn, Some(lsn("0/14EE0B8")));
    assert_eq!(branch.ancestor_timeline_id.as_deref(), Some(main.as_str()));
    client
        .create_timeline(&tenant, &head, 17, Some((&main, None)))
        .await
        .unwrap();

    let seen = m.seen();
    let path = format!("/v1/tenant/{tenant}/timeline");
    assert!(seen.iter().all(|s| s.method == "POST" && s.path == path));
    assert_eq!(
        seen[0].body,
        json!({ "new_timeline_id": main.as_str(), "pg_version": 17 })
    );
    assert_eq!(
        seen[1].body,
        json!({
            "new_timeline_id": dev.as_str(),
            "ancestor_timeline_id": main.as_str(),
            "ancestor_start_lsn": "0/14EE0B8",
            "pg_version": 17,
        })
    );
    assert_eq!(seen[2].body["ancestor_start_lsn"], Value::Null);
}

#[tokio::test]
async fn get_timeline_reads_it_back() {
    let m = mock(vec![(
        200,
        r#"{"ancestor_lsn":null,"last_record_lsn":"0/1F98488"}"#,
    )])
    .await;
    let (tenant, timeline) = (id(1), id(2));
    let info = StorconClient::new(&m.base, 0)
        .get_timeline(&tenant, &timeline)
        .await
        .unwrap();
    assert_eq!(info.last_record_lsn, lsn("0/1F98488"));
    let seen = m.seen();
    assert_eq!(seen[0].method, "GET");
    assert_eq!(
        seen[0].path,
        format!("/v1/tenant/{tenant}/timeline/{timeline}")
    );
    // A reply without the GC cutoff (an older pageserver) reads as none.
    assert_eq!(info.applied_gc_cutoff_lsn, None);
}

// Step 6g: a base's static compute can't read below the timeline's GC cutoff.
#[tokio::test]
async fn get_timeline_reads_the_gc_cutoff() {
    let m = mock(vec![(
        200,
        r#"{"ancestor_lsn":"0/1000000","last_record_lsn":"0/1F98488","applied_gc_cutoff_lsn":"0/1800000"}"#,
    )])
    .await;
    let info = StorconClient::new(&m.base, 0)
        .get_timeline(&id(1), &id(2))
        .await
        .unwrap();
    assert_eq!(info.applied_gc_cutoff_lsn, Some(lsn("0/1800000")));
}

#[tokio::test]
async fn a_4xx_with_msg_is_rejected_with_that_message() {
    let m = mock(vec![(409, r#"{"msg":"tenant exists"}"#)]).await;
    let err = StorconClient::new(&m.base, 0)
        .create_tenant(&id(1))
        .await
        .unwrap_err();
    match err {
        StorconError::Rejected { status, message } => {
            assert_eq!(status, StatusCode::CONFLICT);
            assert_eq!(message, "tenant exists");
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[tokio::test]
async fn a_4xx_with_a_plain_body_keeps_the_body() {
    let m = mock(vec![(400, "bad tenant id")]).await;
    let err = StorconClient::new(&m.base, 0)
        .create_tenant(&id(1))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, StorconError::Rejected { status, message }
            if *status == StatusCode::BAD_REQUEST && message == "bad tenant id"),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_5xx_is_other_and_names_status_and_message() {
    let m = mock(vec![(500, r#"{"msg":"boom"}"#)]).await;
    let err = StorconClient::new(&m.base, 0)
        .create_tenant(&id(1))
        .await
        .unwrap_err();
    assert!(matches!(err, StorconError::Other(_)), "{err:?}");
    let text = err.to_string();
    assert!(text.contains("500") && text.contains("boom"), "{text}");
}

#[tokio::test]
async fn delete_polls_while_accepted() {
    let m = mock(vec![(202, ""), (202, ""), (200, "")]).await;
    let tenant = id(1);
    fast_deletes(StorconClient::new(&m.base, 0), 5)
        .delete_tenant(&tenant)
        .await
        .unwrap();
    let seen = m.seen();
    assert_eq!(seen.len(), 3);
    let path = format!("/v1/tenant/{tenant}");
    assert!(seen.iter().all(|s| s.method == "DELETE" && s.path == path));
}

#[tokio::test]
async fn delete_of_a_missing_object_succeeds_at_once() {
    let m = mock(vec![(404, r#"{"msg":"not found"}"#)]).await;
    let (tenant, timeline) = (id(1), id(2));
    fast_deletes(StorconClient::new(&m.base, 0), 5)
        .delete_timeline(&tenant, &timeline)
        .await
        .unwrap();
    let seen = m.seen();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].path,
        format!("/v1/tenant/{tenant}/timeline/{timeline}")
    );
}

#[tokio::test]
async fn delete_gives_up_after_the_attempts() {
    let m = mock(vec![(202, ""), (202, ""), (202, "")]).await;
    let err = fast_deletes(StorconClient::new(&m.base, 0), 3)
        .delete_tenant(&id(1))
        .await
        .unwrap_err();
    assert!(matches!(err, StorconError::Other(_)), "{err:?}");
    assert!(err.to_string().contains("timed out"), "{err}");
    assert_eq!(m.seen().len(), 3);
}

#[tokio::test]
async fn delete_treats_any_other_success_as_gone() {
    let m = mock(vec![(204, "")]).await;
    fast_deletes(StorconClient::new(&m.base, 0), 5)
        .delete_tenant(&id(1))
        .await
        .unwrap();
    assert_eq!(m.seen().len(), 1);
}

#[tokio::test]
async fn delete_with_zero_attempts_still_asks_once() {
    let m = mock(vec![(200, "")]).await;
    fast_deletes(StorconClient::new(&m.base, 0), 0)
        .delete_tenant(&id(1))
        .await
        .unwrap();
    assert_eq!(m.seen().len(), 1);
}

/// A storage controller that answers 202 slowly must not stretch a delete far past its
/// poll budget (attempts × interval, plus one request timeout).
#[tokio::test]
async fn delete_polling_has_an_overall_deadline() {
    let app = Router::new().fallback(|| async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        AxumStatus::ACCEPTED
    });
    let base = serve(app).await;
    let started = Instant::now();
    // Budget: 1000 × 1 ms + 300 ms ≈ 1.3 s. Without a deadline: 1000 × ~100 ms ≈ 100 s.
    let err = StorconClient::new(&base, 0)
        .with_timeout(Duration::from_millis(300))
        .with_delete_poll(Duration::from_millis(1), 1000)
        .delete_tenant(&id(1))
        .await
        .unwrap_err();
    assert!(matches!(err, StorconError::Other(_)), "{err:?}");
    assert!(err.to_string().contains("timed out"), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn delete_maps_other_statuses_as_errors() {
    let m = mock(vec![(409, r#"{"msg":"has children"}"#)]).await;
    let err = fast_deletes(StorconClient::new(&m.base, 0), 5)
        .delete_timeline(&id(1), &id(2))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, StorconError::Rejected { message, .. } if message == "has children"),
        "{err:?}"
    );
}

#[tokio::test]
async fn pageserver_connstring_uses_the_first_shard() {
    let m = mock(vec![
        (
            200,
            r#"{"shards":[{"shard_id":"s","node_id":1,"listen_pg_addr":"pageserver","listen_pg_port":6400,"listen_http_addr":"pageserver","listen_http_port":9898}]}"#,
        ),
        (200, r#"{"shards":[]}"#),
    ])
    .await;
    let client = StorconClient::new(&m.base, 0);
    let tenant = id(1);
    assert_eq!(
        client.pageserver_connstring(&tenant).await.unwrap(),
        "postgresql://no_user@pageserver:6400"
    );
    assert_eq!(
        m.seen()[0].path,
        format!("/debug/v1/tenant/{tenant}/locate")
    );
    let err = client.pageserver_connstring(&tenant).await.unwrap_err();
    assert!(matches!(err, StorconError::Other(_)), "{err:?}");
}

#[tokio::test]
async fn active_safekeepers_skips_the_rest() {
    let m = mock(vec![(
        200,
        r#"[{"id":1,"host":"safekeeper1","port":5454,"scheduling_policy":"Active"},
            {"id":2,"host":"safekeeper2","port":5454,"scheduling_policy":"Pause"}]"#,
    )])
    .await;
    let sks = StorconClient::new(&m.base, 0)
        .active_safekeepers()
        .await
        .unwrap();
    assert_eq!(sks, vec!["safekeeper1:5454".to_string()]);
    assert_eq!(m.seen()[0].path, "/control/v1/safekeeper");
}

#[tokio::test]
async fn active_safekeeper_ids_filters_like_active_safekeepers() {
    let body = r#"[{"id":1,"host":"safekeeper1","port":5454,"http_port":7676,"scheduling_policy":"Active"},
            {"id":2,"host":"safekeeper2","port":5454,"http_port":7676,"scheduling_policy":"Pause"},
            {"id":3,"host":"safekeeper3","port":5454,"http_port":7676,"scheduling_policy":"Active"}]"#;
    let m = mock(vec![(200, body), (200, body)]).await;
    let client = StorconClient::new(&m.base, 0);
    assert_eq!(client.active_safekeeper_ids().await.unwrap(), vec![1, 3]);
    assert_eq!(
        client.active_safekeepers().await.unwrap(),
        vec![
            "safekeeper1:5454".to_string(),
            "safekeeper3:5454".to_string()
        ]
    );
    assert_eq!(m.seen()[0].path, "/control/v1/safekeeper");
}

#[tokio::test]
async fn describe_tenant_parses_shards() {
    let m = mock(vec![(
        200,
        r#"{"tenant_id":"t","shards":[
            {"tenant_shard_id":"t","is_pending_compute_notification":true,"is_reconciling":false,"last_error":"hook failed"},
            {"tenant_shard_id":"t","is_pending_compute_notification":false,"is_reconciling":true,"last_error":null}]}"#,
    )])
    .await;
    let tenant = id(1);
    let d = StorconClient::new(&m.base, 0)
        .describe_tenant(&tenant)
        .await
        .unwrap();
    assert_eq!(d.shards.len(), 2);
    assert!(d.shards[0].is_pending_compute_notification);
    assert!(!d.shards[0].is_reconciling);
    assert_eq!(d.shards[0].last_error, "hook failed");
    assert!(d.shards[1].is_reconciling);
    assert_eq!(d.shards[1].last_error, "");
    assert_eq!(m.seen()[0].path, format!("/control/v1/tenant/{tenant}"));
}

#[tokio::test]
async fn unreachable_server_is_other() {
    // Port 1 on loopback: nothing listens, so the connection is refused. The short timeout
    // keeps the test quick on a host that drops the packets instead.
    let err = StorconClient::new("http://127.0.0.1:1", 0)
        .with_timeout(Duration::from_secs(2))
        .active_safekeepers()
        .await
        .unwrap_err();
    assert!(matches!(err, StorconError::Other(_)), "{err:?}");
}

#[tokio::test]
async fn trailing_slash_in_base_is_ignored() {
    let m = mock(vec![(200, "[]")]).await;
    StorconClient::new(&format!("{}/", m.base), 0)
        .active_safekeepers()
        .await
        .unwrap();
    assert_eq!(m.seen()[0].path, "/control/v1/safekeeper");
}

#[tokio::test]
async fn malformed_success_body_is_other() {
    let m = mock(vec![(200, r#"{"last_record_lsn":"nope"}"#), (200, "{}")]).await;
    let client = StorconClient::new(&m.base, 0);
    for _ in 0..2 {
        let err = client.get_timeline(&id(1), &id(2)).await.unwrap_err();
        assert!(matches!(err, StorconError::Other(_)), "{err:?}");
    }
}

#[tokio::test]
async fn hanging_server_times_out() {
    let app = Router::new().fallback(|| async {
        tokio::time::sleep(Duration::from_secs(60)).await;
        "late"
    });
    let base = serve(app).await;
    let started = Instant::now();
    let err = StorconClient::new(&base, 0)
        .with_timeout(Duration::from_millis(200))
        .active_safekeepers()
        .await
        .unwrap_err();
    assert!(matches!(err, StorconError::Other(_)), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn list_tenant_ids_reads_every_tenant() {
    let m = mock(vec![(
        200,
        r#"[{"tenant_id":"00000000000000000000000000000001","shards":[]},
            {"tenant_id":"00000000000000000000000000000002","extra":1}]"#,
    )])
    .await;
    let ids = StorconClient::new(&m.base, 0)
        .list_tenant_ids()
        .await
        .unwrap();
    assert_eq!(ids, vec![id(1), id(2)]);
    assert_eq!(m.seen()[0].method, "GET");
    assert_eq!(m.seen()[0].path, "/control/v1/tenant");
}

#[tokio::test]
async fn list_tenant_ids_rejects_a_malformed_id() {
    let m = mock(vec![(200, r#"[{"tenant_id":"not-an-id"}]"#)]).await;
    let err = StorconClient::new(&m.base, 0)
        .list_tenant_ids()
        .await
        .unwrap_err();
    assert!(matches!(err, StorconError::Other(_)), "{err:?}");
}

fn at() -> chrono::DateTime<chrono::Utc> {
    "2026-09-30T12:34:56Z".parse().unwrap()
}

#[tokio::test]
async fn lsn_by_timestamp_parses_each_kind() {
    let m = mock(vec![
        (200, r#"{"lsn":"0/16B3748","kind":"present","valid_until":"2026-09-30T12:40:00.000Z"}"#),
        (200, r#"{"lsn":"0/16B3749","kind":"future"}"#),
        (200, r#"{"lsn":"0/16B374A","kind":"past"}"#),
        (200, r#"{"lsn":"0/0","kind":"nodata"}"#),
    ])
    .await;
    let client = StorconClient::new(&m.base, 0);
    let (t, tl) = (id(1), id(2));
    let mut got = Vec::new();
    for _ in 0..4 {
        got.push(client.lsn_by_timestamp(&t, &tl, at()).await.unwrap());
    }
    assert_eq!(
        got,
        vec![
            LsnAtTime { lsn: lsn("0/16B3748"), kind: LsnKind::Present },
            LsnAtTime { lsn: lsn("0/16B3749"), kind: LsnKind::Future },
            LsnAtTime { lsn: lsn("0/16B374A"), kind: LsnKind::Past },
            LsnAtTime { lsn: lsn("0/0"), kind: LsnKind::Nodata },
        ]
    );
    let seen = m.seen();
    assert_eq!(seen[0].method, "GET");
    assert_eq!(
        seen[0].path,
        format!("/v1/tenant/{}/timeline/{}/get_lsn_by_timestamp", t.as_str(), tl.as_str())
    );
    assert!(seen[0].query.contains("with_lease=true"), "{}", seen[0].query);
    assert!(
        seen[0].query.contains("timestamp=2026-09-30T12%3A34%3A56Z"),
        "{}",
        seen[0].query
    );
}

#[tokio::test]
async fn timestamp_of_lsn_none_on_404() {
    let m = mock(vec![
        (200, r#""2026-09-30T12:34:56.123456Z""#),
        (404, r#"{"msg":"not found"}"#),
        (412, r#"{"msg":"Timestamp for lsn 0/1 not found"}"#),
        (400, r#"{"msg":"bad lsn"}"#),
    ])
    .await;
    let client = StorconClient::new(&m.base, 0);
    let (t, tl) = (id(1), id(2));
    let some = client.timestamp_of_lsn(&t, &tl, lsn("0/16B3748")).await.unwrap();
    assert_eq!(some, Some("2026-09-30T12:34:56.123456Z".parse().unwrap()));
    assert_eq!(client.timestamp_of_lsn(&t, &tl, lsn("0/1")).await.unwrap(), None);
    assert_eq!(client.timestamp_of_lsn(&t, &tl, lsn("0/1")).await.unwrap(), None);
    assert!(matches!(
        client.timestamp_of_lsn(&t, &tl, lsn("0/1")).await,
        Err(StorconError::Rejected { .. })
    ));
    let seen = m.seen();
    assert_eq!(
        seen[0].path,
        format!("/v1/tenant/{}/timeline/{}/get_timestamp_of_lsn", t.as_str(), tl.as_str())
    );
    assert_eq!(seen[0].query, "lsn=0%2F16B3748");
}

#[tokio::test]
async fn detach_ancestor_sends_multi_level_no_reparent() {
    let m = mock(vec![(200, r#"{"reparented_timelines":[]}"#), (202, "{}")]).await;
    let client = StorconClient::new(&m.base, 0);
    let (t, tl) = (id(1), id(2));
    client.detach_ancestor(&t, &tl).await.unwrap();
    client.detach_ancestor(&t, &tl).await.unwrap();
    let seen = m.seen();
    assert_eq!(seen[0].method, "PUT");
    assert_eq!(
        seen[0].path,
        format!("/v1/tenant/{}/timeline/{}/detach_ancestor", t.as_str(), tl.as_str())
    );
    assert_eq!(seen[0].query, "detach_behavior=multi_level_and_no_reparent");
}

#[tokio::test]
async fn patch_tenant_config_sends_seconds() {
    let m = mock(vec![(200, "{}")]).await;
    let t = id(1);
    StorconClient::new(&m.base, 0)
        .patch_tenant_config(&t, Duration::from_secs(604_800))
        .await
        .unwrap();
    let seen = m.seen();
    assert_eq!(seen[0].method, "PATCH");
    assert_eq!(seen[0].path, "/v1/tenant/config");
    assert_eq!(
        seen[0].body,
        json!({ "tenant_id": t.as_str(), "pitr_interval": "604800s" })
    );
}

#[tokio::test]
async fn tenant_pitr_interval_parses_humantime() {
    let m = mock(vec![
        (200, r#"{"tenant_specific_overrides":{},"effective_config":{"pitr_interval":"7days"}}"#),
        (200, r#"{"effective_config":{"pitr_interval":"604800s"}}"#),
        (200, r#"{"effective_config":{"pitr_interval":"1h 30m"}}"#),
        (200, r#"{"effective_config":{"pitr_interval":"garbage"}}"#),
    ])
    .await;
    let client = StorconClient::new(&m.base, 0);
    let t = id(1);
    assert_eq!(client.tenant_pitr_interval(&t).await.unwrap(), Some(Duration::from_secs(604_800)));
    assert_eq!(client.tenant_pitr_interval(&t).await.unwrap(), Some(Duration::from_secs(604_800)));
    assert_eq!(client.tenant_pitr_interval(&t).await.unwrap(), Some(Duration::from_secs(5_400)));
    assert!(matches!(
        client.tenant_pitr_interval(&t).await,
        Err(StorconError::Other(_))
    ));
    let seen = m.seen();
    assert_eq!(seen[0].method, "GET");
    assert_eq!(seen[0].path, format!("/v1/tenant/{}/config", t.as_str()));
}

#[tokio::test]
async fn tenant_pitr_interval_none_when_unset() {
    let m = mock(vec![
        (200, r#"{"effective_config":{}}"#),
        (200, r#"{"effective_config":{"pitr_interval":null}}"#),
        (200, r#"{}"#),
    ])
    .await;
    let client = StorconClient::new(&m.base, 0);
    for _ in 0..3 {
        assert_eq!(client.tenant_pitr_interval(&id(1)).await.unwrap(), None);
    }
}

#[tokio::test]
async fn list_timelines_returns_ids() {
    let m = mock(vec![(
        200,
        r#"[{"tenant_id":"x-0000","timeline_id":"00000000000000000000000000000002","last_record_lsn":"0/1"},
            {"tenant_id":"x-0000","timeline_id":"00000000000000000000000000000003","last_record_lsn":"0/2"}]"#,
    )])
    .await;
    let t = id(1);
    let got = StorconClient::new(&m.base, 0).list_timelines(&t).await.unwrap();
    assert_eq!(got, vec![id(2), id(3)]);
    assert_eq!(m.seen()[0].path, format!("/v1/tenant/{}/timeline", t.as_str()));
}

#[tokio::test]
async fn new_calls_map_a_500_with_its_body() {
    let m = mock(vec![(500, r#"{"msg":"pageserver on fire"}"#)]).await;
    let err = StorconClient::new(&m.base, 0)
        .lsn_by_timestamp(&id(1), &id(2), at())
        .await
        .unwrap_err();
    assert!(matches!(err, StorconError::Other(_)));
    let text = err.to_string();
    assert!(text.contains("500") && text.contains("pageserver on fire"), "{text}");
}

/// Answers every request after `delay` with a timeline description.
async fn slow(delay: Duration) -> String {
    let app = Router::new().fallback(move || async move {
        tokio::time::sleep(delay).await;
        (
            [("content-type", "application/json")],
            r#"{"ancestor_lsn": null, "last_record_lsn": "0/16B3748"}"#,
        )
    });
    serve(app).await
}

#[tokio::test]
async fn timeline_create_and_detach_have_their_own_longer_timeouts() {
    assert_eq!(
        chelabase_neon::storcon::CREATE_TIMELINE_TIMEOUT,
        Duration::from_secs(120)
    );
    assert_eq!(
        chelabase_neon::storcon::DETACH_TIMEOUT,
        Duration::from_secs(600)
    );
    let base = slow(Duration::from_millis(400)).await;
    let client = StorconClient::new(&base, 0)
        .with_timeout(Duration::from_millis(100))
        .with_timeline_timeouts(Duration::from_secs(5), Duration::from_secs(5));
    let (t, tl) = (id(1), id(2));

    client
        .create_timeline(&t, &tl, 17, Some((&id(3), None)))
        .await
        .expect("create outlasts the request timeout");
    client
        .detach_ancestor(&t, &tl)
        .await
        .expect("detach outlasts the request timeout");
    let err = client.get_timeline(&t, &tl).await.unwrap_err();
    assert!(matches!(err, StorconError::Other(_)), "{err:?}");
}

#[tokio::test]
async fn a_detach_that_outlasts_its_timeout_is_other() {
    let base = slow(Duration::from_millis(400)).await;
    let client = StorconClient::new(&base, 0)
        .with_timeline_timeouts(Duration::from_secs(5), Duration::from_millis(100));
    let started = Instant::now();
    let err = client.detach_ancestor(&id(1), &id(2)).await.unwrap_err();
    assert!(matches!(err, StorconError::Other(_)), "{err:?}");
    assert!(started.elapsed() < Duration::from_millis(390));
    let err = StorconClient::new(&base, 0)
        .with_timeline_timeouts(Duration::from_millis(100), Duration::from_secs(5))
        .create_timeline(&id(1), &id(2), 17, None)
        .await
        .unwrap_err();
    assert!(matches!(err, StorconError::Other(_)), "{err:?}");
}

// ---- Transient refusals: unschedulable 409s and the Attaching 503 (carried-gaps T1) ----

const CREATE_409: &str =
    r#"{"msg":"Conflict: Failed to schedule shard(s): No pageserver found matching constraint"}"#;
const DELETE_409: &str =
    r#"{"msg":"Conflict: Scheduling error: No pageserver found matching constraint"}"#;
const NO_PAGESERVERS_409: &str =
    r#"{"msg":"Conflict: Failed to schedule shard(s): No pageservers found"}"#;
const ATTACHING_503: &str =
    r#"{"msg":"Timed out waiting 5s for tenant active state. Latest state: Some(Attaching)"}"#;

/// A client that retries unschedulable refusals quickly (1 ms base, 5 ms cap) within `budget`.
fn quick(client: StorconClient, budget: Duration) -> StorconClient {
    client.with_schedule_retry(Duration::from_millis(1), Duration::from_millis(5), budget)
}

/// Answers every request with `(status, body)` after `delay`; returns the base URL and a
/// request counter.
async fn repeating(
    status: u16,
    body: &'static str,
    delay: Duration,
) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = count.clone();
    let app = Router::new().fallback(move || {
        let counter = counter.clone();
        async move {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tokio::time::sleep(delay).await;
            (
                AxumStatus::from_u16(status).unwrap(),
                [("content-type", "application/json")],
                body,
            )
        }
    });
    (serve(app).await, count)
}

fn requests(count: &Arc<std::sync::atomic::AtomicUsize>) -> usize {
    count.load(std::sync::atomic::Ordering::SeqCst)
}

fn rejected(status: u16, message: &str) -> StorconError {
    StorconError::Rejected {
        status: StatusCode::from_u16(status).unwrap(),
        message: message.to_string(),
    }
}

#[test]
fn is_unschedulable_matches_only_the_two_scheduling_conflicts() {
    for message in [
        "Conflict: Failed to schedule shard(s): No pageserver found matching constraint",
        "Conflict: Scheduling error: No pageserver found matching constraint",
        "Conflict: Failed to schedule shard(s): No pageservers found",
        "Conflict: Scheduling error: No pageservers found",
    ] {
        assert!(rejected(409, message).is_unschedulable(), "{message}");
    }
    for message in [
        "tenant exists",
        "Conflict: has children",
        "Conflict: Tenant policy Detached",
        "No pageserver found matching constraint",
        "Conflict: No pageserver found matching constraint but more",
        "",
    ] {
        assert!(!rejected(409, message).is_unschedulable(), "{message}");
    }
    // Only a 409 counts, and a transport or 5xx error never does.
    let same_text = "Conflict: Failed to schedule shard(s): No pageserver found matching constraint";
    assert!(!rejected(400, same_text).is_unschedulable());
    assert!(!rejected(503, same_text).is_unschedulable());
    assert!(!StorconError::Other(anyhow::anyhow!(same_text)).is_unschedulable());
}

#[test]
fn is_tenant_attaching_rejects_everything_that_is_not_that_503() {
    assert!(!rejected(409, "Timed out waiting 5s for tenant active state. Latest state: Some(Attaching)")
        .is_tenant_attaching());
    assert!(!rejected(404, "Attaching").is_tenant_attaching());
    assert!(!StorconError::Other(anyhow::anyhow!("connection refused")).is_tenant_attaching());
    assert!(!StorconError::Other(anyhow::anyhow!("Shutting down")).is_tenant_attaching());
    assert!(!rejected(409, "Conflict: No pageserver found matching constraint")
        .is_tenant_attaching());
}

#[tokio::test]
async fn is_tenant_attaching_matches_the_real_503_and_nothing_else() {
    for (body, expected) in [
        (ATTACHING_503, true),
        // Wrapped by storcon's proxying of the pageserver's answer.
        (
            r#"{"msg":"Error from pageserver 1: Timed out waiting 5s for tenant active state. Latest state: Some(Attaching)"}"#,
            true,
        ),
        (
            r#"{"msg":"Timed out waiting 5s for tenant active state. Latest state: Some(Stopping)"}"#,
            false,
        ),
        (r#"{"msg":"Shutting down"}"#, false),
        (r#"{"msg":"Attaching"}"#, false),
    ] {
        let m = mock(vec![(503, body)]).await;
        let err = quick(StorconClient::new(&m.base, 0), Duration::ZERO)
            .create_timeline(&id(1), &id(2), 17, None)
            .await
            .unwrap_err();
        assert_eq!(err.is_tenant_attaching(), expected, "{body}: {err:?}");
        assert!(!err.is_unschedulable(), "{err:?}");
    }
}

// ---- create_tenant ----

/// Default settings (base 250 ms): a few unschedulable answers, then success.
#[tokio::test]
async fn create_tenant_retries_an_unschedulable_409_with_default_settings() {
    let m = mock(vec![(409, CREATE_409), (409, CREATE_409), (201, "{}")]).await;
    StorconClient::new(&m.base, 1)
        .create_tenant(&id(1))
        .await
        .unwrap();
    let seen = m.seen();
    assert_eq!(seen.len(), 3);
    assert!(seen.iter().all(|s| s.method == "POST" && s.path == "/v1/tenant"));
}

#[tokio::test]
async fn create_tenant_rides_out_k_unschedulable_answers() {
    for k in [1usize, 5, 12] {
        let mut replies = vec![(409, CREATE_409); k];
        replies.push((201, "{}"));
        let m = mock(replies).await;
        quick(StorconClient::new(&m.base, 1), Duration::from_secs(10))
            .create_tenant(&id(1))
            .await
            .unwrap();
        assert_eq!(m.seen().len(), k + 1, "k={k}");
    }
}

/// Create is idempotent, so a retry (a lost success included) resends the identical body.
#[tokio::test]
async fn create_tenant_retry_resends_the_same_body() {
    let m = mock(vec![(409, CREATE_409), (201, "{}")]).await;
    let tenant = id(7);
    quick(StorconClient::new(&m.base, 2), Duration::from_secs(10))
        .create_tenant(&tenant)
        .await
        .unwrap();
    let seen = m.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].body, seen[1].body);
    assert_eq!(
        seen[1].body,
        json!({ "new_tenant_id": tenant.as_str(), "placement_policy": { "Attached": 2 } })
    );
}

#[tokio::test]
async fn create_tenant_retries_no_pageservers_found_too() {
    let m = mock(vec![(409, NO_PAGESERVERS_409), (409, CREATE_409), (200, "{}")]).await;
    quick(StorconClient::new(&m.base, 1), Duration::from_secs(10))
        .create_tenant(&id(1))
        .await
        .unwrap();
    assert_eq!(m.seen().len(), 3);
}

/// Every other refusal is final at once, with the same error as before.
#[tokio::test]
async fn create_tenant_never_retries_other_409s_or_statuses() {
    for (status, body, message) in [
        (409, r#"{"msg":"tenant exists"}"#, "tenant exists"),
        (409, r#"{"msg":"Conflict: has children"}"#, "Conflict: has children"),
        (
            409,
            r#"{"msg":"Conflict: Tenant policy Detached"}"#,
            "Conflict: Tenant policy Detached",
        ),
        (400, r#"{"msg":"bad request"}"#, "bad request"),
        (404, r#"{"msg":"gone"}"#, "gone"),
    ] {
        let m = mock(vec![(status, body), (201, "{}")]).await;
        let err = quick(StorconClient::new(&m.base, 1), Duration::from_secs(10))
            .create_tenant(&id(1))
            .await
            .unwrap_err();
        assert!(
            matches!(&err, StorconError::Rejected { message: got, .. } if got == message),
            "{err:?}"
        );
        assert_eq!(m.seen().len(), 1, "{body}");
    }
    // A 5xx (not the Attaching one) on tenant create is not retried either.
    let m = mock(vec![(500, r#"{"msg":"boom"}"#), (201, "{}")]).await;
    let err = quick(StorconClient::new(&m.base, 1), Duration::from_secs(10))
        .create_tenant(&id(1))
        .await
        .unwrap_err();
    assert!(matches!(err, StorconError::Other(_)), "{err:?}");
    assert_eq!(m.seen().len(), 1);
}

/// A permanently unschedulable stack: gives up after the budget, keeping the original text.
#[tokio::test]
async fn create_tenant_gives_up_after_the_budget_with_the_original_message() {
    let (base, count) = repeating(409, CREATE_409, Duration::ZERO).await;
    let started = Instant::now();
    let err = quick(StorconClient::new(&base, 1), Duration::from_millis(300))
        .create_tenant(&id(1))
        .await
        .unwrap_err();
    let elapsed = started.elapsed();
    let text = err.to_string();
    assert!(
        text.contains("Failed to schedule shard(s): No pageserver found matching constraint"),
        "{text}"
    );
    assert!(text.contains("after") && text.contains("attempts"), "{text}");
    assert!(elapsed >= Duration::from_millis(300), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    assert!(requests(&count) > 2, "{}", requests(&count));
}

/// A zero budget still asks once and does not retry.
#[tokio::test]
async fn create_tenant_with_a_zero_budget_asks_once() {
    let (base, count) = repeating(409, CREATE_409, Duration::ZERO).await;
    let err = quick(StorconClient::new(&base, 1), Duration::ZERO)
        .create_tenant(&id(1))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("No pageserver found"), "{err}");
    assert_eq!(requests(&count), 1);
}

/// Dropping the future during a backoff sleep leaves nothing running: no further request.
#[tokio::test]
async fn create_tenant_cancelled_during_a_sleep_sends_nothing_more() {
    let (base, count) = repeating(409, CREATE_409, Duration::ZERO).await;
    let client = StorconClient::new(&base, 1).with_schedule_retry(
        Duration::from_secs(2),
        Duration::from_secs(2),
        Duration::from_secs(30),
    );
    let timed_out = tokio::time::timeout(Duration::from_millis(300), client.create_tenant(&id(1)))
        .await
        .is_err();
    assert!(timed_out, "create_tenant returned instead of backing off");
    assert_eq!(requests(&count), 1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(requests(&count), 1);
}

// ---- delete ----

/// Default behaviour: an unschedulable 409 is like a 202 (poll again), then 404 or 200.
#[tokio::test]
async fn delete_tenant_polls_through_unschedulable_409s_to_404() {
    let m = mock(vec![
        (409, DELETE_409),
        (409, DELETE_409),
        (409, DELETE_409),
        (404, r#"{"msg":"not found"}"#),
    ])
    .await;
    let tenant = id(1);
    fast_deletes(StorconClient::new(&m.base, 0), 10)
        .delete_tenant(&tenant)
        .await
        .unwrap();
    let seen = m.seen();
    assert_eq!(seen.len(), 4);
    let path = format!("/v1/tenant/{tenant}");
    assert!(seen.iter().all(|s| s.method == "DELETE" && s.path == path));
}

#[tokio::test]
async fn delete_tenant_polls_through_unschedulable_409s_to_200() {
    let m = mock(vec![(409, DELETE_409), (409, NO_PAGESERVERS_409), (200, "")]).await;
    fast_deletes(StorconClient::new(&m.base, 0), 10)
        .delete_tenant(&id(1))
        .await
        .unwrap();
    assert_eq!(m.seen().len(), 3);
}

/// 202 and unschedulable 409 answers draw on the same attempts/deadline.
#[tokio::test]
async fn delete_alternating_202_and_409_share_one_budget() {
    let m = mock(vec![
        (202, ""),
        (409, DELETE_409),
        (202, ""),
        (409, DELETE_409),
        (202, ""),
    ])
    .await;
    let err = fast_deletes(StorconClient::new(&m.base, 0), 4)
        .delete_tenant(&id(1))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");
    assert_eq!(m.seen().len(), 4);

    let m = mock(vec![
        (202, ""),
        (409, DELETE_409),
        (202, ""),
        (409, DELETE_409),
        (200, ""),
    ])
    .await;
    fast_deletes(StorconClient::new(&m.base, 0), 5)
        .delete_tenant(&id(1))
        .await
        .unwrap();
    assert_eq!(m.seen().len(), 5);
}

/// A permanently unschedulable delete times out like a permanent 202, after the attempts.
#[tokio::test]
async fn delete_with_permanent_unschedulable_409_times_out_after_the_attempts() {
    let (base, count) = repeating(409, DELETE_409, Duration::ZERO).await;
    let err = fast_deletes(StorconClient::new(&base, 0), 5)
        .delete_tenant(&id(1))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");
    assert_eq!(requests(&count), 5);
}

/// The deadline bounds slow 409 answers (shape of `delete_polling_has_an_overall_deadline`).
#[tokio::test]
async fn delete_409_polling_has_an_overall_deadline() {
    let (base, _count) = repeating(409, DELETE_409, Duration::from_millis(100)).await;
    let started = Instant::now();
    // Budget: 1000 × 1 ms + 300 ms ≈ 1.3 s. Without a deadline: 1000 × ~100 ms ≈ 100 s.
    let err = StorconClient::new(&base, 0)
        .with_timeout(Duration::from_millis(300))
        .with_delete_poll(Duration::from_millis(1), 1000)
        .delete_tenant(&id(1))
        .await
        .unwrap_err();
    assert!(matches!(err, StorconError::Other(_)), "{err:?}");
    assert!(err.to_string().contains("timed out"), "{err}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

/// Other conflicts stay final for tenant and timeline deletes, even mid-poll.
#[tokio::test]
async fn delete_other_409s_stay_final_even_after_unschedulable_ones() {
    let m = mock(vec![
        (409, DELETE_409),
        (409, r#"{"msg":"Conflict: Tenant policy Detached"}"#),
        (200, ""),
    ])
    .await;
    let err = fast_deletes(StorconClient::new(&m.base, 0), 10)
        .delete_tenant(&id(1))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, StorconError::Rejected { message, .. }
            if message == "Conflict: Tenant policy Detached"),
        "{err:?}"
    );
    assert_eq!(m.seen().len(), 2);

    let m = mock(vec![(409, r#"{"msg":"tenant exists"}"#), (200, "")]).await;
    let err = fast_deletes(StorconClient::new(&m.base, 0), 10)
        .delete_tenant(&id(1))
        .await
        .unwrap_err();
    assert!(matches!(err, StorconError::Rejected { .. }), "{err:?}");
    assert_eq!(m.seen().len(), 1);
}

// ---- create_timeline and the Attaching 503 ----

/// Default settings: the Attaching 503 is retried like an unschedulable conflict.
#[tokio::test]
async fn create_timeline_retries_the_attaching_503_with_default_settings() {
    let m = mock(vec![
        (503, ATTACHING_503),
        (
            201,
            r#"{"timeline_id":"x","ancestor_lsn":null,"last_record_lsn":"0/14EE0B8"}"#,
        ),
    ])
    .await;
    let (tenant, timeline) = (id(1), id(2));
    let info = StorconClient::new(&m.base, 0)
        .create_timeline(&tenant, &timeline, 17, None)
        .await
        .unwrap();
    assert_eq!(info.last_record_lsn, lsn("0/14EE0B8"));
    let seen = m.seen();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0].body, seen[1].body);
    assert_eq!(seen[0].path, format!("/v1/tenant/{tenant}/timeline"));
}

#[tokio::test]
async fn create_timeline_rides_out_k_attaching_503s() {
    for k in [1usize, 6] {
        let mut replies = vec![(503, ATTACHING_503); k];
        replies.push((
            201,
            r#"{"ancestor_lsn":"0/14EE0B8","last_record_lsn":"0/14EE0B8"}"#,
        ));
        let m = mock(replies).await;
        let main = id(2);
        quick(StorconClient::new(&m.base, 0), Duration::from_secs(10))
            .create_timeline(&id(1), &id(3), 17, Some((&main, None)))
            .await
            .unwrap();
        assert_eq!(m.seen().len(), k + 1, "k={k}");
    }
}

/// Any other 503, any 5xx and every 4xx (the scheduling 409 included: timeline calls don't
/// schedule) is final at once.
#[tokio::test]
async fn create_timeline_does_not_retry_other_failures() {
    for (status, body) in [
        (503, r#"{"msg":"Shutting down"}"#),
        (503, r#"{"msg":"Timed out waiting 5s for tenant active state. Latest state: Some(Stopping)"}"#),
        (503, r#"{"msg":"unavailable"}"#),
        (500, r#"{"msg":"boom"}"#),
        (409, CREATE_409),
        (409, r#"{"msg":"timeline exists"}"#),
        (404, r#"{"msg":"gone"}"#),
    ] {
        let m = mock(vec![
            (status, body),
            (
                201,
                r#"{"ancestor_lsn":null,"last_record_lsn":"0/14EE0B8"}"#,
            ),
        ])
        .await;
        let err = quick(StorconClient::new(&m.base, 0), Duration::from_secs(10))
            .create_timeline(&id(1), &id(2), 17, None)
            .await
            .unwrap_err();
        assert_eq!(m.seen().len(), 1, "{status} {body}: {err}");
    }
}

#[tokio::test]
async fn create_timeline_gives_up_on_a_permanent_attaching_503_with_the_original_message() {
    let (base, count) = repeating(503, ATTACHING_503, Duration::ZERO).await;
    let started = Instant::now();
    let err = quick(StorconClient::new(&base, 0), Duration::from_millis(300))
        .create_timeline(&id(1), &id(2), 17, None)
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("Timed out waiting 5s for tenant active state. Latest state: Some(Attaching)"),
        "{text}"
    );
    assert!(text.contains("after") && text.contains("attempts"), "{text}");
    assert!(started.elapsed() >= Duration::from_millis(300));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(requests(&count) > 2);
}

/// Cancelling during a backoff sleep sends nothing more.
#[tokio::test]
async fn create_timeline_cancelled_during_a_sleep_sends_nothing_more() {
    let (base, count) = repeating(503, ATTACHING_503, Duration::ZERO).await;
    let client = StorconClient::new(&base, 0).with_schedule_retry(
        Duration::from_secs(2),
        Duration::from_secs(2),
        Duration::from_secs(30),
    );
    let timed_out = tokio::time::timeout(
        Duration::from_millis(300),
        client.create_timeline(&id(1), &id(2), 17, None),
    )
    .await
    .is_err();
    assert!(timed_out, "create_timeline returned instead of backing off");
    assert_eq!(requests(&count), 1);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(requests(&count), 1);
}

// ---- F-storcon: request timeouts, the budget cap, kept error classes (carried-gaps I2/H1/M2/H2) ----

const TIMELINE_OK: &str = r#"{"ancestor_lsn": null, "last_record_lsn": "0/16B3748"}"#;

/// The first request is answered with `first` after `first_delay`; every later one with `rest`
/// after `rest_delay`. Returns the base URL and a request counter (counted on arrival).
async fn phased(
    first: (u16, &'static str),
    first_delay: Duration,
    rest: (u16, &'static str),
    rest_delay: Duration,
) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = count.clone();
    let app = Router::new().fallback(move || {
        let counter = counter.clone();
        async move {
            let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let ((status, body), delay) = if n == 0 {
                (first, first_delay)
            } else {
                (rest, rest_delay)
            };
            tokio::time::sleep(delay).await;
            (
                AxumStatus::from_u16(status).unwrap(),
                [("content-type", "application/json")],
                body,
            )
        }
    });
    (serve(app).await, count)
}

/// I2/P6: a tenant create whose first answer outlasts the request timeout is sent again
/// (create_tenant is idempotent, a lost success included) and succeeds, within the budget.
#[tokio::test]
async fn create_tenant_retries_a_request_timeout() {
    let (base, count) = phased(
        (201, "{}"),
        Duration::from_millis(80),
        (201, "{}"),
        Duration::ZERO,
    )
    .await;
    let started = Instant::now();
    quick(
        StorconClient::new(&base, 1).with_timeout(Duration::from_millis(50)),
        Duration::from_secs(2),
    )
    .create_tenant(&id(1))
    .await
    .expect("a request timeout is retried for create_tenant");
    assert_eq!(requests(&count), 2);
    // One timed-out attempt plus one backoff, far below the budget.
    assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
}

/// I2/P6: a tenant create that times out every time stops (it doesn't hang on the 5 s answer)
/// and reports the request timeout, not success, never before the budget is spent. Real
/// sockets on a real clock, so only what no scheduling delay can change is asserted here: how
/// many attempts fit in the budget and by how much it is overrun depend on the machine's load
/// (a full run once starved the first attempt past the whole budget). The loop's timing
/// (attempts > 1, each with the request timeout, overrun ≤ one attempt) is pinned under a
/// paused clock, without I/O, by `storcon::tests::retries_of_a_slow_failure_stop_at_the_budget`.
#[tokio::test]
async fn create_tenant_timeouts_stop_at_the_budget() {
    let (base, _count) = repeating(201, "{}", Duration::from_secs(5)).await;
    let budget = Duration::from_millis(400);
    let started = Instant::now();
    let err = quick(
        StorconClient::new(&base, 1).with_timeout(Duration::from_millis(50)),
        budget,
    )
    .create_tenant(&id(1))
    .await
    .unwrap_err();
    let elapsed = started.elapsed();
    assert!(matches!(err, StorconError::Other(_)), "{err:?}");
    assert!(err.is_request_timeout(), "{err:?}");
    // Never given up before the budget: the loop reads the same monotonic clock, started
    // after `started`.
    assert!(elapsed >= budget, "{elapsed:?}");
    // The 5 s answer is a success: waiting for it would have returned Ok above.
}

/// I2/P6: only create_tenant retries a request timeout. A timeline create that times out is
/// final (it is not idempotent for a lost success); regression guard.
#[tokio::test]
async fn create_timeline_does_not_retry_a_request_timeout() {
    let (base, count) = phased(
        (201, TIMELINE_OK),
        Duration::from_millis(80),
        (201, TIMELINE_OK),
        Duration::ZERO,
    )
    .await;
    let err = quick(
        StorconClient::new(&base, 0)
            .with_timeline_timeouts(Duration::from_millis(50), Duration::from_secs(5)),
        Duration::from_secs(2),
    )
    .create_timeline(&id(1), &id(2), 17, None)
    .await
    .unwrap_err();
    assert!(matches!(err, StorconError::Other(_)), "{err:?}");
    assert_eq!(requests(&count), 1);
}

/// I2/P6: a delete that times out is not retried as a timeout either; regression guard.
#[tokio::test]
async fn delete_does_not_retry_a_request_timeout() {
    let (base, count) = phased(
        (200, "{}"),
        Duration::from_millis(80),
        (200, "{}"),
        Duration::ZERO,
    )
    .await;
    let err = fast_deletes(
        quick(
            StorconClient::new(&base, 0).with_timeout(Duration::from_millis(50)),
            Duration::from_secs(2),
        ),
        5,
    )
    .delete_tenant(&id(1))
    .await
    .unwrap_err();
    assert!(matches!(err, StorconError::Other(_)), "{err:?}");
    assert_eq!(requests(&count), 1);
}

/// H1: the last attempt's timeout is capped at what is left of the budget. The first answer
/// is an unschedulable 409, the second hangs; the request timeout (2 s) is far above the
/// budget (100 ms), so only the cap ends it in time.
#[tokio::test]
async fn create_tenant_last_attempt_never_overruns_the_budget() {
    let (base, count) = phased(
        (409, CREATE_409),
        Duration::ZERO,
        (201, "{}"),
        Duration::from_secs(10),
    )
    .await;
    let started = Instant::now();
    let err = quick(
        StorconClient::new(&base, 1).with_timeout(Duration::from_secs(2)),
        Duration::from_millis(100),
    )
    .create_tenant(&id(1))
    .await
    .unwrap_err();
    let elapsed = started.elapsed();
    assert!(requests(&count) >= 2, "{}", requests(&count));
    assert!(elapsed < Duration::from_millis(100 + 400), "{elapsed:?} ({err})");
}

/// H1 for the timeline create: its own 2 s timeout must not outlast a 100 ms budget.
#[tokio::test]
async fn create_timeline_last_attempt_never_overruns_the_budget() {
    let (base, count) = phased(
        (503, ATTACHING_503),
        Duration::ZERO,
        (201, TIMELINE_OK),
        Duration::from_secs(10),
    )
    .await;
    let started = Instant::now();
    let err = quick(
        StorconClient::new(&base, 0)
            .with_timeline_timeouts(Duration::from_secs(2), Duration::from_secs(5)),
        Duration::from_millis(100),
    )
    .create_timeline(&id(1), &id(2), 17, None)
    .await
    .unwrap_err();
    let elapsed = started.elapsed();
    assert!(requests(&count) >= 2, "{}", requests(&count));
    assert!(elapsed < Duration::from_millis(100 + 400), "{elapsed:?} ({err})");
}

/// The timeline create has its own budget: one slow request may outlast the create budget
/// (the tenant create's) as long as it fits the timeline budget.
#[tokio::test]
async fn create_timeline_uses_its_own_budget_not_the_tenant_one() {
    let base = slow(Duration::from_millis(600)).await;
    let client = quick(
        StorconClient::new(&base, 0)
            .with_timeline_timeouts(Duration::from_secs(5), Duration::from_secs(5)),
        Duration::from_millis(100),
    )
    .with_timeline_retry_budget(Duration::from_secs(3));
    client
        .create_timeline(&id(1), &id(2), 17, None)
        .await
        .expect("a 600 ms request fits the 3 s timeline budget");
}

/// ...and the timeline budget still bounds a request that hangs.
#[tokio::test]
async fn the_timeline_budget_still_bounds_a_hanging_request() {
    let base = slow(Duration::from_secs(10)).await;
    let client = quick(
        StorconClient::new(&base, 0)
            .with_timeline_timeouts(Duration::from_secs(5), Duration::from_secs(5)),
        Duration::from_millis(100),
    )
    .with_timeline_retry_budget(Duration::from_millis(500));
    let started = Instant::now();
    client
        .create_timeline(&id(1), &id(2), 17, None)
        .await
        .unwrap_err();
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_millis(500 + 400), "{elapsed:?}");
}

/// H1 for a create whose very first request hangs: the budget caps it too.
#[tokio::test]
async fn create_tenant_first_attempt_is_capped_by_the_budget_too() {
    let (base, count) = repeating(201, "{}", Duration::from_secs(10)).await;
    let started = Instant::now();
    let err = quick(
        StorconClient::new(&base, 1).with_timeout(Duration::from_secs(2)),
        Duration::from_millis(100),
    )
    .create_tenant(&id(1))
    .await
    .unwrap_err();
    let elapsed = started.elapsed();
    assert!(requests(&count) >= 1);
    assert!(elapsed < Duration::from_millis(100 + 400), "{elapsed:?} ({err})");
}

/// M2: after exhaustion the error still answers `is_unschedulable` (the note must not break
/// the `ends_with` check), like `is_tenant_attaching`.
#[tokio::test]
async fn an_exhausted_create_tenant_is_still_unschedulable() {
    let (base, _count) = repeating(409, CREATE_409, Duration::ZERO).await;
    let err = quick(StorconClient::new(&base, 1), Duration::from_millis(100))
        .create_tenant(&id(1))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("after"), "{err}");
    assert!(err.is_unschedulable(), "{err}");
    assert!(matches!(err, StorconError::Rejected { .. }), "{err:?}");
    let (base, _count) = repeating(409, NO_PAGESERVERS_409, Duration::ZERO).await;
    let err = quick(StorconClient::new(&base, 1), Duration::from_millis(100))
        .create_tenant(&id(1))
        .await
        .unwrap_err();
    assert!(err.is_unschedulable(), "{err}");
}

/// H2: a delete whose unschedulable 409 never clears times out but keeps the last 409's text.
#[tokio::test]
async fn an_exhausted_delete_keeps_the_last_409_text() {
    let (base, _count) = repeating(409, DELETE_409, Duration::ZERO).await;
    let err = fast_deletes(StorconClient::new(&base, 0), 3)
        .delete_tenant(&id(1))
        .await
        .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("timed out"), "{text}");
    assert!(
        text.contains("Scheduling error: No pageserver found matching constraint"),
        "{text}"
    );
    // The same for a timeline delete.
    let err = fast_deletes(StorconClient::new(&base, 0), 3)
        .delete_timeline(&id(1), &id(2))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("No pageserver found matching constraint"), "{err}");
}

/// H2 boundary: a delete that only ever got 202s has no 409 to report.
#[tokio::test]
async fn an_exhausted_delete_of_202s_has_no_409_text() {
    let (base, _count) = repeating(202, "{}", Duration::ZERO).await;
    let err = fast_deletes(StorconClient::new(&base, 0), 3)
        .delete_tenant(&id(1))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("timed out"), "{err}");
    assert!(!err.to_string().contains("Conflict"), "{err}");
}
