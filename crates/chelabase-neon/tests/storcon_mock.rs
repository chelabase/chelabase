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
            r#"{"ancestor_lsn":"0/14EE0B8","last_record_lsn":"0/14EE0B8"}"#,
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
            ancestor_lsn: None,
            last_record_lsn: lsn("0/14EE0B8"),
        }
    );
    let branch = client
        .create_timeline(&tenant, &dev, 17, Some((&main, Some(lsn("0/14EE0B8")))))
        .await
        .unwrap();
    assert_eq!(branch.ancestor_lsn, Some(lsn("0/14EE0B8")));
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
