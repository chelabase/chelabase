//! RT1 final fix round (batch B, T7), the coder's edges: access events coalesced per member on
//! the instance (coordinator ruling 4: one `/authorize` per target per burst, however many
//! sockets the member has), and joins answered off the socket loop (a pending join counts as
//! joined, a leave cancels it, replies keep the order of the joins).

mod common;

use std::time::{Duration, Instant};

use common::*;
use serde_json::json;
use uuid::Uuid;

fn access_subject(tenant: Uuid) -> String {
    format!("chelabase.rt.access.{tenant}")
}

/// Three sockets of one member on one table: an access event re-authorizes each of them (a
/// revocation reaches every socket) with one `/authorize` call between them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_access_event_costs_one_call_per_target_for_all_the_members_sockets() {
    let h = Harness::start().await;
    let mut sockets = Vec::new();
    for _ in 0..3 {
        let mut ws = h.authed("ana").await;
        assert_eq!(ws.join_table("1", "items", None).await["type"], "joined");
        sockets.push(ws);
    }
    let before = h.orch.authorize_calls_for("ana");
    h.orch.on_authorize(|_| refused("forbidden"));
    h.publish(
        &access_subject(TENANT),
        &json!({"tenant_id": TENANT, "member_sub": "ana"}),
    )
    .await;
    for ws in &mut sockets {
        let e = ws.frame_of("error").await;
        assert_eq!(e["code"], "forbidden", "every socket is revoked: {e}");
    }
    assert_eq!(
        h.orch.authorize_calls_for("ana") - before,
        1,
        "one call for the member's three sockets"
    );
}

/// A burst of events for one member with three sockets: one burst now and at most one trailing
/// one, each a single call (not one per socket per event).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_burst_for_a_member_with_three_sockets_is_one_or_two_calls() {
    let h = Harness::start().await;
    let mut sockets = Vec::new();
    for _ in 0..3 {
        let mut ws = h.authed("ana").await;
        assert_eq!(ws.join_table("1", "items", None).await["type"], "joined");
        sockets.push(ws);
    }
    let before = h.orch.authorize_calls_for("ana");
    for _ in 0..10 {
        h.publish(
            &access_subject(TENANT),
            &json!({"tenant_id": TENANT, "member_sub": "ana"}),
        )
        .await;
    }
    tokio::time::sleep(Duration::from_millis(1600)).await;
    let calls = h.orch.authorize_calls_for("ana") - before;
    assert!((1..=2).contains(&calls), "{calls} calls for a burst of 10");
}

/// A whole-tenant event: each member's sockets share their member's call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tenant_event_costs_one_call_per_member() {
    let h = Harness::start().await;
    let mut sockets = Vec::new();
    for sub in ["ana", "ana", "bob", "bob"] {
        let mut ws = h.authed(sub).await;
        assert_eq!(ws.join_table("1", "items", None).await["type"], "joined");
        sockets.push(ws);
    }
    let (ana, bob) = (
        h.orch.authorize_calls_for("ana"),
        h.orch.authorize_calls_for("bob"),
    );
    h.publish(&access_subject(TENANT), &json!({"tenant_id": TENANT}))
        .await;
    let orch = h.orch.clone();
    assert!(
        eventually(Duration::from_secs(2), move || {
            orch.authorize_calls_for("ana") > ana && orch.authorize_calls_for("bob") > bob
        })
        .await
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(h.orch.authorize_calls_for("ana") - ana, 1);
    assert_eq!(h.orch.authorize_calls_for("bob") - bob, 1);
}

/// A join waiting on the orchestrator is joined as far as a second join goes, and a leave
/// cancels it: no `joined` for it, and the channel can be joined again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pending_join_is_already_joined_and_a_leave_cancels_it() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    h.orch.delay_authorize(Duration::from_millis(800));
    a.send(json!({"type": "join", "ref": "1", "channel": "table", "params": table_params("items", None)}))
        .await;
    a.send(json!({"type": "join", "ref": "2", "channel": "table", "params": table_params("items", None)}))
        .await;
    a.send(json!({"type": "leave", "ref": "3", "channel": table_id("items")}))
        .await;
    let second = a.reply("2").await;
    assert_eq!(second["code"], "invalid", "{second}");
    // Nothing more for ref 1 (cancelled) and no error for the leave.
    let frames = a.frames_within(Duration::from_millis(1500)).await;
    assert!(
        frames.iter().all(|f| f["ref"] != "1" && f["ref"] != "3"),
        "{frames:?}"
    );
    h.orch.delay_authorize(Duration::ZERO);
    assert_eq!(a.join_table("4", "items", None).await["type"], "joined");
}

/// A slow join's answer comes before the answers of the joins after it, even refusals made at
/// once; the socket is read meanwhile (a renewal frame gets in).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn joins_are_answered_in_order() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    h.orch.delay_authorize(Duration::from_millis(600));
    let started = Instant::now();
    a.send(json!({"type": "join", "ref": "1", "channel": "table", "params": table_params("items", None)}))
        .await;
    a.send(json!({"type": "join", "ref": "2", "channel": "table", "params": table_params("items", None)}))
        .await;
    a.send(json!({"type": "auth", "token": token("ana", 7200)}))
        .await;
    let first = a.frame().await;
    assert_eq!(first["ref"], "1", "the slow join first: {first}");
    assert_eq!(first["type"], "joined");
    assert!(started.elapsed() >= Duration::from_millis(500));
    let second = a.reply("2").await;
    assert_eq!(second["code"], "invalid", "{second}");
}
