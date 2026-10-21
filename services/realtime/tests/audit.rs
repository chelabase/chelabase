//! RT1 T7 audit's tests: a client that floods frames is still closed at its token's `exp` and
//! still re-authorized on an access event; a client that stops reading is dropped without
//! stalling the table's other viewers; a degraded channel gets keyless changes only (spec §4);
//! two databases created in quick succession both reach a project channel (R21).

mod common;

use std::time::{Duration, Instant};

use common::*;
use futures_util::{SinkExt, StreamExt};
use realtime::protocol::{Change, ChangeOp, SUBPROTOCOL, close};
use realtime::source::ChannelEvent;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use uuid::Uuid;

type Raw = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A raw client socket (the harness's `Ws` can't read and write at once).
async fn raw_open(h: &Harness, sub: &str) -> Raw {
    let ticket = h.ticket(sub).await;
    let mut req = Harness::socket_url(h.rt(), &ticket)
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(SUBPROTOCOL),
    );
    tokio_tungstenite::connect_async(req)
        .await
        .expect("opens")
        .0
}

async fn raw_send(ws: &mut Raw, v: Value) {
    ws.send(Message::Text(v.to_string().into()))
        .await
        .expect("send");
}

/// The next text frame answering `ref`.
async fn raw_reply(ws: &mut Raw, r: &str) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                let v: Value = serde_json::from_str(t.as_str()).unwrap();
                if v["ref"] == r {
                    return v;
                }
            }
            Ok(Some(Ok(_))) => {}
            other => panic!("wanted the answer to ref {r}, got {other:?}"),
        }
    }
}

/// An authenticated raw socket for `sub` (token valid `exp_in` s) joined to `table`.
async fn raw_joined(h: &Harness, sub: &str, exp_in: i64, table: &str) -> Raw {
    let mut ws = raw_open(h, sub).await;
    raw_send(
        &mut ws,
        json!({"type": "auth", "token": token(sub, exp_in)}),
    )
    .await;
    raw_send(
        &mut ws,
        json!({"type": "join", "ref": "1", "channel": "table", "params": table_params(table, None)}),
    )
    .await;
    assert_eq!(raw_reply(&mut ws, "1").await["type"], "joined");
    ws
}

/// A `view` frame of 300 keys for `table`, as text.
fn heavy_view(table: &str) -> String {
    let keys: Vec<String> = (0..300).map(|i| format!("[\"{i:08}\"]")).collect();
    json!({"type": "view", "channel": table_id(table), "keys": keys}).to_string()
}

/// Writes `text` as fast as the socket takes it until `stop` passes or the socket fails.
async fn flood(mut sink: impl SinkExt<Message> + Unpin, text: String, stop: Instant) {
    while Instant::now() < stop {
        if sink.send(Message::Text(text.clone().into())).await.is_err() {
            return;
        }
    }
}

/// Reads until `want` holds for a text frame or the socket closes: the frame, or the close code.
async fn read_until(
    stream: &mut (impl StreamExt<Item = Result<Message, tungstenite::Error>> + Unpin),
    within: Duration,
    want: impl Fn(&Value) -> bool,
) -> Result<Value, Option<u16>> {
    let deadline = Instant::now() + within;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, stream.next()).await {
            Err(_) => return Err(None),
            Ok(Some(Ok(Message::Text(t)))) => {
                let v: Value = serde_json::from_str(t.as_str()).unwrap();
                if want(&v) {
                    return Ok(v);
                }
            }
            Ok(Some(Ok(Message::Close(f)))) => return Err(Some(f.map_or(1005, |f| f.code.into()))),
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(_)) | None) => return Err(Some(1006)),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flooding_client_is_still_closed_at_its_tokens_exp() {
    let h = Harness::start().await;
    let ws = raw_joined(&h, "ana", 2, "items").await;
    let (sink, mut stream) = ws.split();
    let started = Instant::now();
    let flooder = tokio::spawn(flood(
        sink,
        heavy_view("items"),
        started + Duration::from_secs(8),
    ));
    let end = read_until(&mut stream, Duration::from_secs(6), |_| false).await;
    flooder.abort();
    assert_eq!(
        end,
        Err(Some(close::TOKEN_EXPIRED)),
        "closed 4001 at exp (≤ 2 s) although the client never stops sending"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_flooding_client_is_still_reauthorized_on_an_access_event() {
    let h = Harness::start().await;
    let ws = raw_joined(&h, "ana", 3600, "items").await;
    let (sink, mut stream) = ws.split();
    let flooder = tokio::spawn(flood(
        sink,
        heavy_view("items"),
        Instant::now() + Duration::from_secs(8),
    ));
    tokio::time::sleep(Duration::from_millis(300)).await;
    h.orch.on_authorize(|_| refused("forbidden"));
    h.publish(
        &format!("chelabase.rt.access.{TENANT}"),
        &json!({"tenant_id": TENANT, "member_sub": "ana"}),
    )
    .await;
    let revoked = read_until(&mut stream, Duration::from_secs(3), |v| {
        v["type"] == "error" && v["code"] == "forbidden"
    })
    .await;
    flooder.abort();
    assert!(
        revoked.is_ok(),
        "the access event revokes within a second while the client floods: {revoked:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_stops_reading_is_dropped_and_the_others_keep_up() {
    let h = Harness::start_with(|c| c.timings.write_timeout = Duration::from_millis(300)).await;
    // `slow` joins, then never reads again; `fast` reads everything.
    let _slow = raw_joined(&h, "slow", 3600, "items").await;
    let mut fast = h.authed("fast").await;
    assert_eq!(fast.join_table("1", "items", None).await["type"], "joined");
    // ~24 MB of rows: more than loopback's socket buffers hold for `slow`.
    let keys: Vec<String> = (0..4000).map(|i| format!("[\"{i:058}\"]")).collect();
    let frames = 100;
    for _ in 0..frames {
        let ev = ChannelEvent::Rows(vec![Change {
            op: ChangeOp::Insert,
            keys: Some(keys.clone()),
        }]);
        let sent = tokio::time::timeout(Duration::from_secs(5), h.source.emit("items", ev)).await;
        assert!(sent.is_ok(), "a slow viewer stalls the change source");
    }
    let mut last = 0;
    while last < frames {
        let f = fast.frame_of("rows").await;
        last = f["seq"].as_u64().unwrap();
    }
    let store = h.store.clone();
    assert!(
        eventually(Duration::from_secs(4), move || {
            !store.keys().iter().any(|k| k == "rt:cnt:sock:slow")
        })
        .await,
        "the stalled socket is dropped and gives its count back"
    );
}

#[tokio::test]
async fn a_degraded_channel_gets_keyless_changes_only() {
    // Spec §4: orchestrator unreachable during re-authorization → only keyless `changed`.
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    h.orch.on_authorize(|_| refused("unavailable"));
    h.publish(
        &format!("chelabase.rt.access.{TENANT}"),
        &json!({"tenant_id": TENANT, "member_sub": "ana"}),
    )
    .await;
    assert_eq!(a.frame_of("status").await["reason"], "unavailable");
    h.source
        .emit(
            "items",
            ChannelEvent::Rows(vec![
                Change {
                    op: ChangeOp::Insert,
                    keys: Some(vec!["[\"7\"]".into()]),
                },
                Change {
                    op: ChangeOp::Delete,
                    keys: Some(vec!["[\"8\"]".into()]),
                },
            ]),
        )
        .await;
    let rows = a.frame_of("rows").await;
    assert_eq!(
        rows["changes"],
        json!([{"op": "changed"}]),
        "no key reaches a viewer whose access can't be checked: {rows}"
    );
}

#[tokio::test]
async fn two_new_databases_in_quick_succession_both_reach_the_project_channel() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_project("1", PROJECT).await["type"], "joined");
    let (d2, d3) = (Uuid::from_u128(0xd2), Uuid::from_u128(0xd3));
    // The first refresh (for d2) is answered before d3 exists; d3 needs a second one.
    let calls = std::sync::atomic::AtomicUsize::new(0);
    h.orch.on_projects(
        move |_| match calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
            0 => projects_of(&[(PROJECT, &[DB, d2])]),
            _ => projects_of(&[(PROJECT, &[DB, d2, d3])]),
        },
    );
    let subject = format!("chelabase.rt.project.{PROJECT}");
    for d in [d2, d3] {
        h.publish(
            &subject,
            &json!({"event": "database.created", "database_id": d}),
        )
        .await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut seen = vec![];
    for _ in 0..2 {
        seen.push(a.frame_of("project").await["database_id"].clone());
    }
    assert_eq!(seen, vec![json!(d2), json!(d3)]);
}
