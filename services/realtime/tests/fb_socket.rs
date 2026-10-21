//! RT1 final fix round (batch B, T7 items): the socket loop never waits on the orchestrator
//! inline (a renewal is read while a join's authorize is pending), `view` commands are
//! latest-wins, a fresh re-authorization answer refreshes the per-cycle cache, shutdown keeps
//! one deadline inside compose's 10 s grace, and an instance re-publishes its members after a
//! partition healed.

mod common;

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::*;
use futures_util::SinkExt;
use realtime::protocol::SUBPROTOCOL;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

/// Reads `ws` until `want` holds for a frame; panics on a close or at `deadline`.
async fn frame_until(
    ws: &mut Ws,
    deadline: Instant,
    what: &str,
    want: impl Fn(&Value) -> bool,
) -> Value {
    loop {
        match ws
            .read(deadline.saturating_duration_since(Instant::now()))
            .await
        {
            Read::Frame(v) if want(&v) => return v,
            Read::Frame(_) => continue,
            Read::Closed(code) => panic!("{what}: the socket closed {code}"),
            Read::Timeout => panic!("{what}: nothing by the deadline"),
        }
    }
}

/// Whether a frame matching `want` arrives before `deadline` (other frames skipped).
async fn arrives(ws: &mut Ws, deadline: Instant, want: impl Fn(&Value) -> bool) -> bool {
    loop {
        match ws
            .read(deadline.saturating_duration_since(Instant::now()))
            .await
        {
            Read::Frame(v) if want(&v) => return true,
            Read::Frame(_) => continue,
            Read::Closed(_) | Read::Timeout => return false,
        }
    }
}

fn is_forbidden(v: &Value) -> bool {
    v["type"] == "error" && v["code"] == "forbidden"
}

// --- defect: the socket loop awaited `/authorize` inline -------------------------------------

/// A join waits up to 20 s for the orchestrator (S14). Meanwhile the socket keeps reading: a
/// renewal sent while the join is pending is verified before the first token's `exp`, so the
/// socket stays open (with the inline call it was read only after the join returned, when the
/// first token had expired: 4001).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_renewal_is_read_while_a_join_waits_for_the_orchestrator() {
    let h = Harness::start().await;
    let ticket = h.ticket("ana").await;
    let mut ws = Harness::open(h.rt(), &ticket).await;
    ws.send(json!({"type": "auth", "token": token("ana", 3)}))
        .await;
    h.orch.delay_authorize(Duration::from_secs(6));
    let started = Instant::now();
    ws.send(json!({"type": "join", "ref": "1", "channel": "table", "params": table_params("items", None)}))
        .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    ws.send(json!({"type": "auth", "token": token("ana", 3600)}))
        .await;
    let joined = frame_until(
        &mut ws,
        started + Duration::from_secs(12),
        "the renewal must keep the socket open past the first token's exp",
        |v| v["ref"] == "1",
    )
    .await;
    assert_eq!(joined["type"], "joined", "{joined}");
    assert!(
        started.elapsed() >= Duration::from_secs(5),
        "the join waited for the slow orchestrator"
    );
    // Still open afterwards.
    assert!(
        !matches!(ws.read(Duration::from_millis(500)).await, Read::Closed(_)),
        "the renewed socket stays open"
    );
}

/// The same for a periodic re-authorization: while it waits on the orchestrator, frames are
/// still read (a `leave` is answered at once with the channel gone, not after the call).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn frames_are_read_while_a_reauthorization_waits() {
    let h = Harness::start_with(|c| c.timings.reauth_interval = Duration::from_secs(1)).await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    assert_eq!(a.join_table("2", "other", None).await["type"], "joined");
    h.orch.delay_authorize(Duration::from_secs(8));
    // The first periodic re-authorization starts (≈1 s) and waits 8 s.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let asked = Instant::now();
    a.send(json!({"type": "leave", "ref": "3", "channel": table_id("missing")}))
        .await;
    let answer = frame_until(
        &mut a,
        asked + Duration::from_secs(3),
        "the leave's answer",
        |v| v["ref"] == "3",
    )
    .await;
    assert_eq!(answer["code"], "invalid", "{answer}");
    assert!(
        asked.elapsed() < Duration::from_secs(2),
        "answered while the re-authorization waits: {:?}",
        asked.elapsed()
    );
}

// --- hardening: latest-wins `view` -----------------------------------------------------------

/// `view` frames replace each other: while one is being checked, only the newest waits (the
/// command channel isn't an unbounded backlog of stale views).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn view_frames_are_latest_wins() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    h.source.set_view_delay(Some(Duration::from_millis(300)));
    for i in 0..30 {
        a.send(
            json!({"type": "view", "channel": table_id("items"), "keys": [format!("[\"{i}\"]")]}),
        )
        .await;
    }
    let src = h.source.clone();
    assert!(
        eventually(Duration::from_millis(1500), move || {
            src.last_view("items") == Some(vec!["[\"29\"]".to_string()])
        })
        .await,
        "the newest view lands within a few checks: last {:?} after {} calls",
        h.source.last_view("items"),
        h.source.view_calls("items")
    );
    assert!(
        h.source.view_calls("items") <= 4,
        "stale views are skipped, not checked one by one: {} calls",
        h.source.view_calls("items")
    );
}

// --- hardening: the per-cycle cache takes fresh answers --------------------------------------

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Two sockets of one member on one table. The first's periodic re-authorization caches
/// `allowed` for the cycle; access is revoked and the first socket's renewal asks afresh
/// (`forbidden`). The second socket's periodic re-authorization in the same cycle must use that
/// fresh answer, not the stale `allowed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fresh_answer_replaces_the_cycles_cached_one() {
    const CYCLE_MS: u64 = 4000;
    let h =
        Harness::start_with(|c| c.timings.reauth_interval = Duration::from_millis(CYCLE_MS)).await;
    // Start just after a cycle boundary, so both sockets' first periodic runs share a cycle.
    let aligned = Instant::now() + Duration::from_secs(5);
    while !(100..400).contains(&(unix_ms() % CYCLE_MS)) {
        assert!(Instant::now() < aligned, "no cycle boundary found");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let t0 = Instant::now();
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    tokio::time::sleep_until((t0 + Duration::from_secs(1)).into()).await;
    let mut b = h.authed("ana").await;
    assert_eq!(b.join_table("1", "items", None).await["type"], "joined");
    // a's periodic run (≈ t0 + 4 s) caches `allowed`.
    tokio::time::sleep_until((t0 + Duration::from_millis(4400)).into()).await;
    let before = h.orch.authorize_calls_for("ana");
    assert!(before >= 3, "two joins and a's periodic run: {before}");
    h.orch.on_authorize(|_| refused("forbidden"));
    a.send(json!({"type": "auth", "token": token("ana", 7200)}))
        .await;
    assert!(
        arrives(
            &mut a,
            Instant::now() + Duration::from_secs(1),
            is_forbidden
        )
        .await,
        "the renewal's fresh answer revokes a"
    );
    // b's periodic run (≈ t0 + 5 s, same cycle).
    assert!(
        arrives(&mut b, t0 + Duration::from_millis(6500), is_forbidden).await,
        "b's run in the same cycle used the stale cached `allowed`"
    );
}

// --- hardening: one shutdown deadline --------------------------------------------------------

/// A raw socket that authenticates, joins and then never reads or answers again.
async fn silent_socket(
    h: &Harness,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>> {
    let ticket = h.ticket("silent").await;
    let mut req = Harness::socket_url(h.rt(), &ticket)
        .into_client_request()
        .unwrap();
    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_static(SUBPROTOCOL),
    );
    let mut ws = tokio_tungstenite::connect_async(req)
        .await
        .expect("opens")
        .0;
    ws.send(Message::Text(
        json!({"type": "auth", "token": token("silent", 3600)})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    ws.send(Message::Text(
        json!({"type": "join", "ref": "1", "channel": "table", "params": table_params("items", None)})
            .to_string()
            .into(),
    ))
    .await
    .unwrap();
    ws
}

/// Every phase of a shutdown stuck at once (both listeners holding a half-sent request, a
/// socket that never answers its 1012, a change source that never finishes releasing): `wait`
/// still returns inside compose's 10 s stop grace, with a margin.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_ends_within_composes_grace_whatever_hangs() {
    let mut h = Harness::start().await;
    h.source.hang_shutdown(true);
    let _silent = silent_socket(&h).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut half_public = TcpStream::connect(h.rt().addr()).await.unwrap();
    half_public
        .write_all(b"GET /health HTTP/1.1\r\nHost: realtime\r\n")
        .await
        .unwrap();
    let mut half_internal = TcpStream::connect(h.rt().internal_addr()).await.unwrap();
    half_internal
        .write_all(b"GET /health HTTP/1.1\r\nHost: realtime\r\n")
        .await
        .unwrap();
    let rt = h.take_rt();
    let started = Instant::now();
    rt.shutdown();
    let waited = tokio::time::timeout(Duration::from_secs(30), rt.wait()).await;
    assert!(waited.is_ok(), "wait never returned");
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(9),
        "one deadline for every phase, inside the 10 s grace: took {took:?}"
    );
}

// --- hardening: members re-published after a partition --------------------------------------

/// Two instances lose the bus (a partition): each drops the other's members after three missed
/// heartbeats. When the bus is back, each instance re-publishes its own members, so the other
/// side sees them again without anyone changing their state.
#[tokio::test]
async fn members_come_back_after_a_partition() {
    let h =
        Harness::start_with(|c| c.timings.presence_heartbeat = Duration::from_millis(100)).await;
    let other = h.instance(|_| {}).await;
    let mut a = h.authed_on(&other, "ana").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    let mut b = h.authed_on(h.rt(), "bob").await;
    assert_eq!(b.join_table("1", "items", None).await["type"], "joined");
    let snapshot = b.frame_of("presence_state").await;
    assert_eq!(
        snapshot["members"].as_array().map(Vec::len),
        Some(2),
        "{snapshot}"
    );
    h.bus.set_down(true);
    let gone = b.frame_of("presence_diff").await;
    assert!(
        gone["leaves"]
            .as_array()
            .is_some_and(|l| l.iter().any(|m| m["sub"] == "ana")),
        "ana expires on bob's instance during the partition: {gone}"
    );
    h.bus.set_down(false);
    let back = arrives(&mut b, Instant::now() + Duration::from_secs(3), |v| {
        v["type"] == "presence_diff"
            && v["joins"]
                .as_array()
                .is_some_and(|j| j.iter().any(|m| m["sub"] == "ana"))
    })
    .await;
    assert!(back, "ana is back once the bus is");
    let _ = &mut a;
}
