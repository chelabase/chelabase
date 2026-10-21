//! RT1 T7: table channels end to end with the fake orchestrator and the fake change source:
//! the join flow (authorize, then watch), refusal codes, a second join of a joined id, limits,
//! `rows` numbering, `status`/`resync`, `view` (R19) and leaving (spec §2, §4, P11, P13, S13,
//! S16, S17).

mod common;

use std::time::Duration;

use common::*;
use realtime::protocol::{Change, ChangeOp, StatusReason};
use realtime::source::fake::FakeChangeSource;
use realtime::source::{ChannelEvent, JoinInfo, JoinRefusal};
use serde_json::{Value, json};

#[tokio::test]
async fn an_owner_join_is_authorized_then_watched() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    let joined = ws.join_table("1", "items", None).await;
    assert_eq!(
        joined,
        json!({"type": "joined", "ref": "1", "channel": table_id("items"), "info": {"pk": ["id"], "live": true}})
    );
    let calls = h.orch.authorize_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]["target"],
        json!({"database_id": DB, "branch_id": BR, "pg_database": "app", "role": null})
    );
    assert_eq!(
        sub_of(calls[0]["token"].as_str().unwrap()).as_deref(),
        Some("ana"),
        "the socket's own token"
    );
    assert_eq!(h.source.watchers("items"), 1);
}

#[tokio::test]
async fn presence_state_follows_joined_with_the_joiner() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    assert_eq!(
        ws.join_table("1", "items", Some("billing")).await["type"],
        "joined"
    );
    let state = ws.frame_of("presence_state").await;
    assert_eq!(state["channel"], table_id("items"));
    let members = state["members"].as_array().unwrap();
    assert_eq!(members.len(), 1, "{state}");
    let me = &members[0];
    assert_eq!(me["sub"], "ana");
    assert_eq!(me["name"], "Name ana", "from the orchestrator's member");
    assert_eq!(me["email"], "ana@example.com");
    assert_eq!(me["role"], "billing", "the act-as role");
    assert_eq!(
        me["color_seed"],
        json!(realtime::protocol::color_seed("ana"))
    );
    assert_eq!(
        me["state"],
        json!({"row": null, "column": null, "editing": false})
    );
    assert!(me["id"].as_str().is_some_and(|s| !s.is_empty()));
}

#[tokio::test]
async fn a_role_join_sends_the_role() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    assert_eq!(
        ws.join_table("1", "items", Some("billing")).await["type"],
        "joined"
    );
    assert_eq!(h.orch.authorize_calls()[0]["target"]["role"], "billing");
}

fn error_code(v: &Value) -> (&str, &str) {
    (
        v["type"].as_str().unwrap_or(""),
        v["code"].as_str().unwrap_or(""),
    )
}

#[tokio::test]
async fn orchestrator_refusals_map_to_error_codes_and_nothing_is_watched() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    for (answer, code) in [
        (refused("forbidden"), "forbidden"),
        (refused("not_found"), "not_found"),
        (refused("unavailable"), "unavailable"),
        ((503, json!({})), "unavailable"),
        ((401, json!({})), "unavailable"),
    ] {
        let a = answer.clone();
        h.orch.on_authorize(move |_| a.clone());
        let r = format!("r-{code}-{}", answer.0);
        let reply = ws.join_table(&r, "items", None).await;
        assert_eq!(error_code(&reply), ("error", code), "{answer:?} → {reply}");
        assert_eq!(reply["ref"], r.as_str());
        assert_eq!(h.source.watchers("items"), 0, "never watched");
    }
}

#[tokio::test]
async fn source_refusals_map_to_error_codes() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    for (i, (refusal, code)) in [
        (JoinRefusal::Forbidden, "forbidden"),
        (JoinRefusal::NotFound, "not_found"),
        (JoinRefusal::Unavailable, "unavailable"),
        (JoinRefusal::Invalid("not a table".into()), "invalid"),
    ]
    .into_iter()
    .enumerate()
    {
        let table = format!("t{i}");
        h.source.set_join(&table, Err(refusal));
        let reply = ws.join_table(&format!("{i}"), &table, None).await;
        assert_eq!(error_code(&reply), ("error", code), "{reply}");
        if code == "invalid" {
            assert!(
                reply["message"].as_str().unwrap().contains("not a table"),
                "{reply}"
            );
        }
    }
}

#[tokio::test]
async fn the_sources_info_is_the_joined_info() {
    let h = Harness::start().await;
    h.source.set_join(
        "events",
        Ok(JoinInfo {
            pk: None,
            live: false,
            reason: Some(StatusReason::NoSlot),
            ..FakeChangeSource::default_info()
        }),
    );
    let mut ws = h.authed("ana").await;
    let joined = ws.join_table("1", "events", None).await;
    assert_eq!(
        joined["info"],
        json!({"pk": null, "live": false, "reason": "no_slot"})
    );
}

#[tokio::test]
async fn a_second_join_of_a_joined_id_is_invalid() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    assert_eq!(ws.join_table("1", "items", None).await["type"], "joined");
    // S17: the id has no role, so an act-as switch must leave first.
    let again = ws.join_table("2", "items", Some("billing")).await;
    assert_eq!(error_code(&again), ("error", "invalid"));
    assert!(
        again["message"]
            .as_str()
            .unwrap()
            .contains("already joined"),
        "{again}"
    );
    assert_eq!(h.source.watchers("items"), 1);
}

#[tokio::test]
async fn an_act_as_switch_leaves_and_rejoins() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    assert_eq!(ws.join_table("1", "items", None).await["type"], "joined");
    ws.send(json!({"type": "leave", "ref": "2", "channel": table_id("items")}))
        .await;
    assert_eq!(
        ws.join_table("3", "items", Some("billing")).await["type"],
        "joined"
    );
    assert_eq!(h.source.watchers("items"), 1);
    assert_eq!(h.orch.authorize_calls().len(), 2, "a fresh authorization");
}

#[tokio::test]
async fn channels_per_socket_are_limited() {
    let h = Harness::start_with(|c| c.limits.channels_per_socket = 2).await;
    let mut ws = h.authed("ana").await;
    assert_eq!(ws.join_table("1", "a", None).await["type"], "joined");
    assert_eq!(ws.join_project("2", PROJECT).await["type"], "joined");
    let third = ws.join_table("3", "c", None).await;
    assert_eq!(error_code(&third), ("error", "limit"), "{third}");
    ws.send(json!({"type": "leave", "ref": "4", "channel": table_id("a")}))
        .await;
    assert_eq!(
        ws.join_table("5", "c", None).await["type"],
        "joined",
        "a left channel frees its place"
    );
}

#[tokio::test]
async fn joins_are_limited_to_five_a_second_per_socket() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    for i in 0..6 {
        ws.send(json!({"type": "join", "ref": format!("{i}"), "channel": "table", "params": table_params(&format!("t{i}"), None)}))
            .await;
    }
    let mut codes = vec![];
    for i in 0..6 {
        let reply = ws.reply(&format!("{i}")).await;
        codes.push(reply["code"].as_str().unwrap_or("joined").to_string());
    }
    assert_eq!(
        codes.iter().filter(|c| *c == "joined").count(),
        5,
        "{codes:?}"
    );
    assert_eq!(
        codes.iter().filter(|c| *c == "limit").count(),
        1,
        "{codes:?}"
    );
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(
        ws.join_table("9", "t9", None).await["type"],
        "joined",
        "a second later"
    );
}

fn insert(key: &str) -> Change {
    Change {
        op: ChangeOp::Insert,
        keys: Some(vec![key.into()]),
    }
}

#[tokio::test]
async fn rows_are_numbered_per_channel_from_one() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    assert_eq!(ws.join_table("1", "a", None).await["type"], "joined");
    assert_eq!(ws.join_table("2", "b", None).await["type"], "joined");
    h.source
        .emit("a", ChannelEvent::Rows(vec![insert("[\"1\"]")]))
        .await;
    let first = ws.frame_of("rows").await;
    assert_eq!(
        first,
        json!({"type": "rows", "channel": table_id("a"), "seq": 1, "changes": [{"op": "insert", "keys": ["[\"1\"]"]}]})
    );
    h.source
        .emit(
            "a",
            ChannelEvent::Rows(vec![Change {
                op: ChangeOp::Changed,
                keys: None,
            }]),
        )
        .await;
    let second = ws.frame_of("rows").await;
    assert_eq!(
        (second["seq"].clone(), second["changes"].clone()),
        (json!(2), json!([{"op": "changed"}]))
    );
    h.source
        .emit("b", ChannelEvent::Rows(vec![insert("[\"9\"]")]))
        .await;
    let other = ws.frame_of("rows").await;
    assert_eq!(
        (other["channel"].as_str(), other["seq"].as_u64()),
        (Some(table_id("b").as_str()), Some(1))
    );
}

#[tokio::test]
async fn status_and_resync_reach_the_socket() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    assert_eq!(ws.join_table("1", "items", None).await["type"], "joined");
    h.source
        .emit(
            "items",
            ChannelEvent::Status {
                live: false,
                reason: Some(StatusReason::ComputeStopping),
            },
        )
        .await;
    assert_eq!(
        ws.frame_of("status").await,
        json!({"type": "status", "channel": table_id("items"), "live": false, "reason": "compute_stopping"})
    );
    h.source
        .emit(
            "items",
            ChannelEvent::Resync {
                reason: "slot".into(),
            },
        )
        .await;
    assert_eq!(
        ws.frame_of("resync").await,
        json!({"type": "resync", "channel": table_id("items"), "reason": "slot"})
    );
}

#[tokio::test]
async fn view_frames_reach_the_source() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    assert_eq!(ws.join_table("1", "items", None).await["type"], "joined");
    ws.send(json!({"type": "view", "channel": table_id("items"), "keys": ["[\"1\"]", "[\"2\"]"]}))
        .await;
    let src = h.source.clone();
    assert!(
        eventually(Duration::from_secs(2), move || src.last_view("items")
            == Some(vec!["[\"1\"]".into(), "[\"2\"]".into()]))
        .await
    );
}

#[tokio::test]
async fn a_view_over_1000_keys_is_limit_and_an_unjoined_view_is_invalid() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    assert_eq!(ws.join_table("1", "items", None).await["type"], "joined");
    let keys: Vec<String> = (0..1001).map(|i| format!("[\"{i}\"]")).collect();
    ws.send(json!({"type": "view", "channel": table_id("items"), "keys": keys}))
        .await;
    assert_eq!(ws.frame_of("error").await["code"], "limit");
    ws.send(json!({"type": "view", "channel": table_id("other"), "keys": []}))
        .await;
    let e = ws.frame_of("error").await;
    assert_eq!(
        (e["code"].as_str(), e["channel"].as_str()),
        (Some("invalid"), Some(table_id("other").as_str())),
        "{e}"
    );
}

#[tokio::test]
async fn leaving_unwatches_and_stops_rows() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    assert_eq!(ws.join_table("1", "items", None).await["type"], "joined");
    ws.send(json!({"type": "leave", "ref": "2", "channel": table_id("items")}))
        .await;
    let src = h.source.clone();
    assert!(
        eventually(Duration::from_secs(2), move || src.watchers("items") == 0).await,
        "the handle is dropped"
    );
    h.source
        .emit("items", ChannelEvent::Rows(vec![insert("[\"1\"]")]))
        .await;
    let frames = ws.frames_within(Duration::from_millis(300)).await;
    assert!(frames.iter().all(|f| f["type"] != "rows"), "{frames:?}");
    ws.send(json!({"type": "leave", "ref": "3", "channel": table_id("items")}))
        .await;
    let e = ws.frame_of("error").await;
    assert_eq!(
        (e["code"].as_str(), e["ref"].as_str()),
        (Some("invalid"), Some("3")),
        "leaving an unjoined channel: {e}"
    );
}

#[tokio::test]
async fn a_closed_socket_unwatches_everything() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    assert_eq!(ws.join_table("1", "a", None).await["type"], "joined");
    assert_eq!(ws.join_table("2", "b", None).await["type"], "joined");
    ws.close().await;
    let src = h.source.clone();
    assert!(
        eventually(Duration::from_secs(2), move || src.watchers("a")
            + src.watchers("b")
            == 0)
        .await
    );
}

#[tokio::test]
async fn two_sockets_have_their_own_sequences() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    let mut b = h.authed("bob").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    h.source
        .emit("items", ChannelEvent::Rows(vec![insert("[\"1\"]")]))
        .await;
    assert_eq!(a.frame_of("rows").await["seq"], 1);
    assert_eq!(b.join_table("1", "items", None).await["type"], "joined");
    h.source
        .emit("items", ChannelEvent::Rows(vec![insert("[\"2\"]")]))
        .await;
    assert_eq!(a.frame_of("rows").await["seq"], 2);
    assert_eq!(b.frame_of("rows").await["seq"], 1, "per socket");
}
