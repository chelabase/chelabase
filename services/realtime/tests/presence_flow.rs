//! RT1 T7: presence end to end (spec §5.1, §5.2, R15, R20): members across sockets and
//! instances, the setter's visibility check, viewers' blanked states, the re-check when a
//! `rows` batch touches a selected key, the rate and size limits, a closed socket and a crashed
//! instance, and no presence on a degraded channel.

mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use common::*;
use realtime::protocol::{Change, ChangeOp};
use realtime::source::ChannelEvent;
use serde_json::{Value, json};

fn state(row: Option<&str>, editing: bool) -> Value {
    json!({"row": row, "column": "name", "editing": editing})
}

fn subs(members: &Value) -> BTreeSet<String> {
    members
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["sub"].as_str().unwrap().to_string())
        .collect()
}

/// The next `presence_diff` of `ws`.
async fn diff(ws: &mut Ws) -> Value {
    ws.frame_of("presence_diff").await
}

/// Whether any member in `frames`' diffs (joins) is `sub` with a non-null row.
fn saw_row_of(frames: &[Value], sub: &str) -> bool {
    frames
        .iter()
        .filter(|f| f["type"] == "presence_diff" || f["type"] == "presence_state")
        .any(|f| {
            ["joins", "members"].iter().any(|k| {
                f[*k].as_array().is_some_and(|ms| {
                    ms.iter()
                        .any(|m| m["sub"] == sub && !m["state"]["row"].is_null())
                })
            })
        })
}

#[tokio::test]
async fn members_see_each_other_join() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    assert_eq!(
        subs(&a.frame_of("presence_state").await["members"]),
        ["ana".to_string()].into()
    );
    let mut b = h.authed("bob").await;
    assert_eq!(b.join_table("1", "items", None).await["type"], "joined");
    assert_eq!(
        subs(&b.frame_of("presence_state").await["members"]),
        ["ana".to_string(), "bob".to_string()].into()
    );
    let d = diff(&mut a).await;
    assert_eq!(subs(&d["joins"]), ["bob".to_string()].into());
    assert_eq!(d["leaves"], json!([]));
}

#[tokio::test]
async fn two_tabs_of_one_person_are_two_members() {
    let h = Harness::start().await;
    let mut a1 = h.authed("ana").await;
    let mut a2 = h.authed("ana").await;
    assert_eq!(a1.join_table("1", "items", None).await["type"], "joined");
    assert_eq!(a2.join_table("1", "items", None).await["type"], "joined");
    let members = a2.frame_of("presence_state").await["members"].clone();
    let ids: BTreeSet<&str> = members
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 2, "per socket and channel: {members}");
}

#[tokio::test]
async fn a_state_update_is_a_leave_and_a_join_of_the_same_id() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    let mut b = h.authed("bob").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    assert_eq!(b.join_table("1", "items", None).await["type"], "joined");
    a.send(json!({"type": "presence", "channel": table_id("items"), "state": state(Some("[\"1\"]"), true)})).await;
    let d = diff(&mut b).await;
    let (joins, leaves) = (
        d["joins"].as_array().unwrap(),
        d["leaves"].as_array().unwrap(),
    );
    assert_eq!((joins.len(), leaves.len()), (1, 1), "{d}");
    assert_eq!(joins[0]["id"], leaves[0]["id"]);
    assert_eq!(joins[0]["sub"], "ana");
    assert_eq!(joins[0]["state"], state(Some("[\"1\"]"), true));
    assert_eq!(
        leaves[0]["state"],
        json!({"row": null, "column": null, "editing": false})
    );
}

#[tokio::test]
async fn a_row_the_setter_cant_see_is_refused() {
    // R15: checked as the setter first.
    let h = Harness::start().await;
    h.source.set_visible(None, "[\"secret\"]", false);
    let mut a = h.authed("ana").await;
    let mut b = h.authed("bob").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    assert_eq!(b.join_table("1", "items", None).await["type"], "joined");
    a.send(json!({"type": "presence", "channel": table_id("items"), "state": state(Some("[\"secret\"]"), false)})).await;
    let e = a.frame_of("error").await;
    assert_eq!(
        (e["code"].as_str(), e["channel"].as_str()),
        (Some("invalid"), Some(table_id("items").as_str())),
        "{e}"
    );
    assert!(
        !saw_row_of(&b.frames_within(Duration::from_millis(400)).await, "ana"),
        "nothing reached bob"
    );
}

#[tokio::test]
async fn a_failed_visibility_check_counts_as_not_visible() {
    let h = Harness::start().await;
    h.source.fail_visible(true);
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    a.send(json!({"type": "presence", "channel": table_id("items"), "state": state(Some("[\"1\"]"), false)})).await;
    assert_eq!(a.frame_of("error").await["code"], "invalid");
}

#[tokio::test]
async fn a_cleared_selection_needs_no_check() {
    let h = Harness::start().await;
    h.source.fail_visible(true);
    let mut a = h.authed("ana").await;
    let mut b = h.authed("bob").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    assert_eq!(b.join_table("1", "items", None).await["type"], "joined");
    a.send(json!({"type": "presence", "channel": table_id("items"), "state": {"row": null, "column": null, "editing": false}})).await;
    let frames = a.frames_within(Duration::from_millis(300)).await;
    assert!(frames.iter().all(|f| f["type"] != "error"), "{frames:?}");
}

#[tokio::test]
async fn a_viewer_who_cant_see_the_row_gets_the_member_blanked() {
    let h = Harness::start().await;
    h.source.set_visible(Some("billing"), "[\"3\"]", false);
    let mut a = h.authed("ana").await; // the owner: sees the row
    let mut b = h.authed("bob").await; // acting as billing: doesn't
    let mut c = h.authed("cy").await; // the owner too
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    assert_eq!(
        b.join_table("1", "items", Some("billing")).await["type"],
        "joined"
    );
    assert_eq!(c.join_table("1", "items", None).await["type"], "joined");
    a.send(json!({"type": "presence", "channel": table_id("items"), "state": state(Some("[\"3\"]"), true)})).await;
    let mut seen_by_c = false;
    for f in c.frames_within(Duration::from_millis(600)).await {
        if f["type"] == "presence_diff"
            && f["joins"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["sub"] == "ana" && m["state"]["row"] == "[\"3\"]")
        {
            seen_by_c = true;
        }
    }
    assert!(seen_by_c, "a viewer who sees the row gets the state");
    assert!(
        !saw_row_of(&b.frames_within(Duration::from_millis(300)).await, "ana"),
        "bob never gets ana's row"
    );
}

#[tokio::test]
async fn a_rows_batch_touching_a_selected_key_rechecks_its_viewers() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    let mut b = h.authed("bob").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    assert_eq!(
        b.join_table("1", "items", Some("billing")).await["type"],
        "joined"
    );
    a.send(json!({"type": "presence", "channel": table_id("items"), "state": state(Some("[\"4\"]"), false)})).await;
    assert!(
        saw_row_of(&b.frames_within(Duration::from_millis(500)).await, "ana"),
        "visible at first"
    );
    // An update hides the row from billing (an RLS policy on the new values).
    h.source.set_visible(Some("billing"), "[\"4\"]", false);
    h.source
        .emit(
            "items",
            ChannelEvent::Rows(vec![Change {
                op: ChangeOp::Update,
                keys: Some(vec!["[\"4\"]".into()]),
            }]),
        )
        .await;
    let mut blanked = false;
    for f in b.frames_within(Duration::from_millis(1500)).await {
        if f["type"] == "presence_diff"
            && f["joins"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["sub"] == "ana" && m["state"]["row"].is_null())
        {
            blanked = true;
        }
    }
    assert!(blanked, "bob gets ana with a blank state");
}

#[tokio::test]
async fn presence_state_over_1_kb_is_limit_and_a_bad_shape_is_invalid() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    a.send(json!({"type": "presence", "channel": table_id("items"), "state": state(Some(&"x".repeat(2000)), false)})).await;
    assert_eq!(a.frame_of("error").await["code"], "limit");
    a.send(json!({"type": "presence", "channel": table_id("items"), "state": {"row": 5}}))
        .await;
    assert_eq!(a.frame_of("error").await["code"], "invalid");
    a.send(json!({"type": "presence", "channel": table_id("nope"), "state": state(None, false)}))
        .await;
    assert_eq!(
        a.frame_of("error").await["code"],
        "invalid",
        "an unjoined channel"
    );
}

#[tokio::test]
async fn presence_over_20_a_second_keeps_only_the_latest() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    let mut b = h.authed("bob").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    assert_eq!(b.join_table("1", "items", None).await["type"], "joined");
    for i in 0..30 {
        a.send(json!({"type": "presence", "channel": table_id("items"), "state": state(Some(&format!("[\"{i}\"]")), false)})).await;
    }
    let frames = b.frames_within(Duration::from_millis(2000)).await;
    let updates: Vec<&Value> = frames
        .iter()
        .filter(|f| f["type"] == "presence_diff")
        .flat_map(|f| f["joins"].as_array().unwrap().iter())
        .filter(|m| m["sub"] == "ana")
        .collect();
    assert!(updates.len() <= 21, "{} updates reached bob", updates.len());
    assert_eq!(
        updates.last().unwrap()["state"]["row"],
        "[\"29\"]",
        "the latest state wins"
    );
    let own = a.frames_within(Duration::from_millis(100)).await;
    let errors: Vec<&Value> = own.iter().filter(|f| f["type"] == "error").collect();
    assert!(
        errors.is_empty(),
        "presence over the rate is dropped silently: {errors:?}"
    );
}

#[tokio::test]
async fn a_channel_has_at_most_its_maximum_of_members() {
    let h = Harness::start_with(|c| c.limits.members_per_channel = 2).await;
    let mut a = h.authed("ana").await;
    let mut b = h.authed("bob").await;
    let mut c = h.authed("cy").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    assert_eq!(b.join_table("1", "items", None).await["type"], "joined");
    let third = c.join_table("1", "items", None).await;
    assert_eq!(
        (third["type"].as_str(), third["code"].as_str()),
        (Some("error"), Some("limit")),
        "{third}"
    );
}

#[tokio::test]
async fn a_closed_socket_leaves_at_once() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    let mut b = h.authed("bob").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    assert_eq!(b.join_table("1", "items", None).await["type"], "joined");
    a.close().await;
    let d = diff(&mut b).await;
    assert_eq!(subs(&d["leaves"]), ["ana".to_string()].into());
}

#[tokio::test]
async fn presence_crosses_instances() {
    let h = Harness::start().await;
    let other = h.instance(|_| {}).await;
    let mut a = h.authed_on(h.rt(), "ana").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    let mut b = h.authed_on(&other, "bob").await;
    assert_eq!(b.join_table("1", "items", None).await["type"], "joined");
    // The instance that starts watching asks the others for their members.
    let snapshot = b.frame_of("presence_state").await;
    assert_eq!(
        subs(&snapshot["members"]),
        ["ana".to_string(), "bob".to_string()].into(),
        "{snapshot}"
    );
    assert_eq!(
        subs(&diff(&mut a).await["joins"]),
        ["bob".to_string()].into()
    );
    a.send(json!({"type": "presence", "channel": table_id("items"), "state": state(Some("[\"1\"]"), false)})).await;
    assert!(
        saw_row_of(&b.frames_within(Duration::from_millis(800)).await, "ana"),
        "an update crosses"
    );
    a.close().await;
    assert_eq!(
        subs(&diff(&mut b).await["leaves"]),
        ["ana".to_string()].into(),
        "a leave crosses"
    );
}

#[tokio::test]
async fn a_crashed_instances_members_leave_after_three_missed_heartbeats() {
    let h =
        Harness::start_with(|c| c.timings.presence_heartbeat = Duration::from_millis(100)).await;
    let other = h.instance(|_| {}).await;
    let mut a = h.authed_on(&other, "ana").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    let mut b = h.authed_on(h.rt(), "bob").await;
    assert_eq!(b.join_table("1", "items", None).await["type"], "joined");
    assert_eq!(
        subs(&b.frame_of("presence_state").await["members"]).len(),
        2
    );
    other.kill();
    let d = b.frame_of("presence_diff").await;
    assert_eq!(
        subs(&d["leaves"]),
        ["ana".to_string()].into(),
        "dropped after ~300 ms of silence"
    );
}

#[tokio::test]
async fn a_degraded_channel_gets_no_presence() {
    // R20: bob's re-authorization is unavailable; his channel degrades.
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    let mut b = h.authed("bob").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    assert_eq!(b.join_table("1", "items", None).await["type"], "joined");
    h.orch.on_authorize(
        |body| match body["token"].as_str().and_then(sub_of).as_deref() {
            Some("bob") => refused("unavailable"),
            Some(sub) => allowed(sub),
            None => refused("forbidden"),
        },
    );
    h.publish(
        &format!("chelabase.rt.access.{TENANT}"),
        &json!({"tenant_id": TENANT, "member_sub": "bob"}),
    )
    .await;
    let status = b.frame_of("status").await;
    assert_eq!(
        status,
        json!({"type": "status", "channel": table_id("items"), "live": false, "reason": "unavailable"})
    );
    a.send(json!({"type": "presence", "channel": table_id("items"), "state": state(Some("[\"1\"]"), false)})).await;
    let frames = b.frames_within(Duration::from_millis(600)).await;
    assert!(
        frames
            .iter()
            .all(|f| f["type"] != "presence_diff" && f["type"] != "presence_state"),
        "{frames:?}"
    );
}
