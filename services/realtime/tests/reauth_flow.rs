//! RT1 T7: re-authorization end to end (spec §4, R15, W19, P13, S13): access events (within
//! 1 s, per member or tenant, coalesced), the periodic cycle, three unavailable answers,
//! `token_expired`, renewals, and project channels.

mod common;

use std::time::{Duration, Instant};

use common::*;
use realtime::protocol::{Change, ChangeOp};
use realtime::source::ChannelEvent;
use serde_json::json;
use uuid::Uuid;

fn access_subject(tenant: Uuid) -> String {
    format!("chelabase.rt.access.{tenant}")
}

/// Ana is refused from now on; everyone else stays allowed.
fn refuse_ana(h: &Harness, reason: &'static str) {
    h.orch.on_authorize(
        move |body| match body["token"].as_str().and_then(sub_of).as_deref() {
            Some("ana") => refused(reason),
            Some(sub) => allowed(sub),
            None => refused("forbidden"),
        },
    );
}

#[tokio::test]
async fn an_access_event_revokes_within_a_second() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    let mut b = h.authed("bob").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    assert_eq!(b.join_table("1", "items", None).await["type"], "joined");
    refuse_ana(&h, "forbidden");
    let started = Instant::now();
    h.publish(
        &access_subject(TENANT),
        &json!({"tenant_id": TENANT, "member_sub": "ana"}),
    )
    .await;
    let e = a.frame_of("error").await;
    assert!(
        started.elapsed() <= Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(
        (e["code"].as_str(), e["channel"].as_str()),
        (Some("forbidden"), Some(table_id("items").as_str())),
        "{e}"
    );
    let src = h.source.clone();
    assert!(
        eventually(Duration::from_secs(1), move || src.watchers("items") == 1).await,
        "ana's watch is dropped"
    );
    let d = b.frame_of("presence_diff").await;
    assert!(
        d["leaves"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["sub"] == "ana"),
        "ana left the members: {d}"
    );
}

#[tokio::test]
async fn not_found_leaves_with_not_found() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    refuse_ana(&h, "not_found");
    h.publish(
        &access_subject(TENANT),
        &json!({"tenant_id": TENANT, "member_sub": "ana"}),
    )
    .await;
    assert_eq!(a.frame_of("error").await["code"], "not_found");
}

#[tokio::test]
async fn a_whole_tenant_event_reauthorizes_every_member() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    let mut b = h.authed("bob").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    assert_eq!(b.join_table("1", "items", None).await["type"], "joined");
    let (ana0, bob0) = (
        h.orch.authorize_calls_for("ana"),
        h.orch.authorize_calls_for("bob"),
    );
    h.publish(
        &access_subject(TENANT),
        &json!({"tenant_id": TENANT, "member_sub": null}),
    )
    .await;
    let orch = h.orch.clone();
    assert!(
        eventually(Duration::from_secs(1), move || orch
            .authorize_calls_for("ana")
            > ana0
            && orch.authorize_calls_for("bob") > bob0)
        .await
    );
}

#[tokio::test]
async fn events_for_other_members_or_tenants_change_nothing() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    let before = h.orch.authorize_calls_for("ana");
    h.publish(
        &access_subject(TENANT),
        &json!({"tenant_id": TENANT, "member_sub": "bob"}),
    )
    .await;
    let other = Uuid::from_u128(77);
    h.publish(
        &access_subject(other),
        &json!({"tenant_id": other, "member_sub": null}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(h.orch.authorize_calls_for("ana"), before);
}

#[tokio::test]
async fn a_burst_of_access_events_is_coalesced_per_member() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    let before = h.orch.authorize_calls_for("ana");
    for _ in 0..10 {
        h.publish(
            &access_subject(TENANT),
            &json!({"tenant_id": TENANT, "member_sub": "ana"}),
        )
        .await;
    }
    tokio::time::sleep(Duration::from_millis(1600)).await;
    let runs = h.orch.authorize_calls_for("ana") - before;
    assert!(
        (1..=2).contains(&runs),
        "{runs} re-authorizations for a burst of 10"
    );
}

#[tokio::test]
async fn every_channel_is_reauthorized_each_cycle() {
    let h = Harness::start_with(|c| c.timings.reauth_interval = Duration::from_millis(300)).await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    refuse_ana(&h, "forbidden");
    let e = a.frame_of("error").await;
    assert_eq!(e["code"], "forbidden", "{e}");
}

#[tokio::test]
async fn three_unavailable_answers_leave_with_unavailable() {
    let h = Harness::start_with(|c| c.timings.reauth_interval = Duration::from_millis(150)).await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    refuse_ana(&h, "unavailable");
    let status = a.frame_of("status").await;
    assert_eq!(
        (status["live"].clone(), status["reason"].clone()),
        (json!(false), json!("unavailable")),
        "degraded first"
    );
    let e = a.frame_of("error").await;
    assert_eq!(
        (e["code"].as_str(), e["channel"].as_str()),
        (Some("unavailable"), Some(table_id("items").as_str()))
    );
    let src = h.source.clone();
    assert!(eventually(Duration::from_secs(1), move || src.watchers("items") == 0).await);
}

#[tokio::test]
async fn token_expired_keeps_the_channel() {
    // W19: the renewal is awaited; the socket's own exp decides.
    let h = Harness::start_with(|c| c.timings.reauth_interval = Duration::from_millis(150)).await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    refuse_ana(&h, "token_expired");
    let frames = a.frames_within(Duration::from_millis(900)).await;
    assert!(frames.iter().all(|f| f["type"] != "error"), "{frames:?}");
    h.source
        .emit(
            "items",
            ChannelEvent::Rows(vec![Change {
                op: ChangeOp::Changed,
                keys: None,
            }]),
        )
        .await;
    assert_eq!(
        a.frame_of("rows").await["channel"],
        table_id("items"),
        "still joined"
    );
    assert_eq!(h.source.watchers("items"), 1);
}

#[tokio::test]
async fn a_renewal_reauthorizes_every_joined_channel_with_the_new_token() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_table("1", "a", None).await["type"], "joined");
    assert_eq!(
        a.join_table("2", "b", Some("billing")).await["type"],
        "joined"
    );
    let before = h.orch.authorize_calls().len();
    let renewed = token("ana", 7200);
    a.send(json!({"type": "auth", "token": renewed.clone()}))
        .await;
    let orch = h.orch.clone();
    assert!(
        eventually(Duration::from_secs(2), move || orch.authorize_calls().len()
            >= before + 2)
        .await
    );
    let calls = h.orch.authorize_calls();
    assert!(
        calls[before..]
            .iter()
            .all(|c| c["token"] == renewed.as_str()),
        "the renewed token is used"
    );
}

#[tokio::test]
async fn a_project_channel_is_left_when_the_project_is_no_longer_visible() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_project("1", PROJECT).await["type"], "joined");
    h.orch.on_projects(|_| projects_of(&[]));
    h.publish(
        &access_subject(TENANT),
        &json!({"tenant_id": TENANT, "member_sub": "ana"}),
    )
    .await;
    let e = a.frame_of("error").await;
    assert_eq!(
        (e["code"].as_str(), e["channel"].as_str()),
        (Some("forbidden"), Some(project_id(PROJECT).as_str())),
        "{e}"
    );
}
