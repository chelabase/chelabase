//! RT1 T7 coder's edge tests, beyond the RED set: name and id spellings at the join, the 64 KB
//! boundary, JWKS down at the first frame and at a renewal, the S16 reprobe on
//! re-authorization, a degraded channel recovering, `changed` batches re-checking every
//! selected member, access events that don't match their subject, project channel edges.

mod common;

use std::time::Duration;

use common::*;
use realtime::channel::{JoinTarget, MAX_NAME_BYTES, parse_join};
use realtime::protocol::{
    Change, ChangeOp, ClientFrame, MAX_FRAME_BYTES, StatusReason, close, parse_client_frame,
};
use realtime::source::fake::FakeChangeSource;
use realtime::source::{ChannelEvent, JoinInfo, JoinRefusal};
use serde_json::{Value, json};
use uuid::Uuid;

// --- Join params --------------------------------------------------------------------------------

#[test]
fn names_postgres_would_truncate_or_cant_hold_are_refused() {
    let long = "x".repeat(MAX_NAME_BYTES + 1);
    let fit = "x".repeat(MAX_NAME_BYTES);
    for field in ["pg_database", "schema", "table", "role"] {
        let mut p = table_params("items", None);
        p[field] = json!(long);
        assert!(
            parse_join("table", Some(&p)).is_err(),
            "{field} over 63 bytes"
        );
        let mut p = table_params("items", None);
        p[field] = json!(fit);
        assert!(parse_join("table", Some(&p)).is_ok(), "{field} of 63 bytes");
        let mut p = table_params("items", None);
        p[field] = json!("a\u{0}b");
        assert!(parse_join("table", Some(&p)).is_err(), "{field} with a NUL");
    }
    // 63 bytes, not characters: 32 two-byte characters are 64 bytes.
    let mut p = table_params("items", None);
    p["table"] = json!("é".repeat(32));
    assert!(parse_join("table", Some(&p)).is_err());
}

#[test]
fn uuids_have_one_spelling() {
    let simple = DB.simple().to_string();
    let braced = format!("{{{DB}}}");
    let urn = DB.urn().to_string();
    for spelling in [&simple, &braced, &urn] {
        let mut p = table_params("items", None);
        p["database_id"] = json!(spelling);
        assert!(parse_join("table", Some(&p)).is_err(), "{spelling}");
        assert!(
            parse_join(&format!("project:{spelling}"), None).is_err(),
            "{spelling}"
        );
    }
}

#[test]
fn the_owner_must_be_said_and_null_params_are_no_params() {
    let mut p = table_params("items", None);
    p.as_object_mut().unwrap().remove("role");
    assert!(
        parse_join("table", Some(&p)).is_err(),
        "a missing role is not the owner"
    );
    assert_eq!(
        parse_join(&project_id(PROJECT), Some(&Value::Null)),
        Ok(JoinTarget::Project(PROJECT))
    );
    assert_eq!(
        parse_client_frame(
            &json!({"type": "join", "ref": "1", "channel": project_id(PROJECT), "params": null})
                .to_string()
        ),
        Ok(ClientFrame::Join {
            r#ref: "1".into(),
            channel: project_id(PROJECT),
            params: None
        })
    );
}

// --- Frames -------------------------------------------------------------------------------------

#[tokio::test]
async fn a_frame_of_exactly_64_kb_is_taken() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    assert_eq!(ws.join_table("1", "items", None).await["type"], "joined");
    let channel = table_id("items");
    let empty = json!({"type": "view", "channel": channel, "keys": [""]}).to_string();
    let key = "k".repeat(MAX_FRAME_BYTES - empty.len());
    let text = json!({"type": "view", "channel": channel, "keys": [key]}).to_string();
    assert_eq!(text.len(), MAX_FRAME_BYTES);
    ws.send_text(text).await;
    let src = h.source.clone();
    let want = key.clone();
    assert!(
        eventually(Duration::from_secs(2), move || src
            .last_view("items")
            .is_some_and(|v| v == [want.clone()]))
        .await
    );
}

#[tokio::test]
async fn a_binary_frame_over_64_kb_closes_1009() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    ws.send_binary(vec![0u8; MAX_FRAME_BYTES + 1]).await;
    assert_eq!(ws.closed().await, close::TOO_BIG);
}

#[tokio::test]
async fn the_ticket_is_found_among_other_query_parameters() {
    let h = Harness::start().await;
    let ticket = h.ticket("ana").await;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let url = format!(
        "ws://{}/v1/realtime/socket?v=2&ticket={ticket}&x",
        h.rt().addr()
    );
    let mut req = url.into_client_request().unwrap();
    req.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        realtime::protocol::SUBPROTOCOL.parse().unwrap(),
    );
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.expect("opens");
    ws.close(None).await.unwrap();
}

// --- Valkey down ----------------------------------------------------------------------------------

#[tokio::test]
async fn open_sockets_stay_up_while_valkey_is_down() {
    // Spec §8.2: no new tickets, but the count refresh failing closes nothing.
    let h = Harness::start_with(|c| c.timings.socket_refresh = Duration::from_millis(100)).await;
    let mut ws = h.authed("ana").await;
    assert_eq!(ws.join_project("1", PROJECT).await["type"], "joined");
    h.store.fail(true);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(ws.join_table("2", "items", None).await["type"], "joined");
    assert_eq!(
        h.post_ticket(h.rt(), Some(&h.identity("ana", ORG))).await.0,
        503
    );
}

// --- JWKS down ----------------------------------------------------------------------------------

#[tokio::test]
async fn a_first_token_that_cant_be_checked_closes_try_again_later() {
    let h = Harness::start().await;
    h.verifier.set_unavailable(true);
    let mut ws = Harness::open(h.rt(), &h.ticket("ana").await).await;
    ws.send(json!({"type": "auth", "token": token("ana", 3600)}))
        .await;
    assert_eq!(ws.closed().await, 1013, "not 4001: the token may be fine");
}

#[tokio::test]
async fn a_renewal_that_cant_be_checked_keeps_the_current_token() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    // The first auth frame has been taken once a join is answered.
    assert_eq!(ws.join_project("1", PROJECT).await["type"], "joined");
    h.verifier.set_unavailable(true);
    ws.send(json!({"type": "auth", "token": token("ana", 7200)}))
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    h.verifier.set_unavailable(false);
    assert_eq!(
        ws.join_table("2", "items", None).await["type"],
        "joined",
        "still open"
    );
}

// --- Re-authorization ---------------------------------------------------------------------------

#[tokio::test]
async fn an_allowed_reauthorization_probes_the_table_again() {
    // S16: the membership still allows, but the table now refuses (a REVOKE in SQL).
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    assert_eq!(ws.join_table("1", "items", None).await["type"], "joined");
    h.source.set_join("items", Err(JoinRefusal::Forbidden));
    let before = h.source.reprobes();
    ws.send(json!({"type": "auth", "token": token("ana", 7200)}))
        .await;
    let e = ws.frame_of("error").await;
    assert_eq!(
        (e["code"].as_str(), e["channel"].as_str()),
        (Some("forbidden"), Some(table_id("items").as_str())),
        "{e}"
    );
    assert!(h.source.reprobes() > before);
    let src = h.source.clone();
    assert!(eventually(Duration::from_secs(1), move || src.watchers("items") == 0).await);
}

#[tokio::test]
async fn a_reprobe_that_changes_the_tables_state_sends_its_status() {
    let h = Harness::start().await;
    let mut ws = h.authed("ana").await;
    assert_eq!(ws.join_table("1", "items", None).await["type"], "joined");
    h.source.set_join(
        "items",
        Ok(JoinInfo {
            live: false,
            reason: Some(StatusReason::NoSlot),
            ..FakeChangeSource::default_info()
        }),
    );
    ws.send(json!({"type": "auth", "token": token("ana", 7200)}))
        .await;
    assert_eq!(
        ws.frame_of("status").await,
        json!({"type": "status", "channel": table_id("items"), "live": false, "reason": "no_slot"})
    );
}

#[tokio::test]
async fn a_degraded_channel_recovers_with_its_status_and_presence() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    let mut b = h.authed("bob").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    assert_eq!(b.join_table("1", "items", None).await["type"], "joined");
    a.frame_of("presence_state").await;
    let access = format!("chelabase.rt.access.{TENANT}");
    h.orch.on_authorize(
        |body| match body["token"].as_str().and_then(sub_of).as_deref() {
            Some("ana") => refused("unavailable"),
            Some(sub) => allowed(sub),
            None => refused("forbidden"),
        },
    );
    h.publish(&access, &json!({"tenant_id": TENANT, "member_sub": "ana"}))
        .await;
    assert_eq!(a.frame_of("status").await["reason"], "unavailable");
    // R20: while degraded, ana's own presence doesn't go out.
    a.send(json!({"type": "presence", "channel": table_id("items"), "state": {"row": "[\"7\"]", "column": null, "editing": false}}))
        .await;
    let frames = b.frames_within(Duration::from_millis(400)).await;
    assert!(
        frames.iter().all(|f| f["type"] != "presence_diff"),
        "{frames:?}"
    );
    // Allowed again: the next run (the trailing run of the 1 s window) ends the degradation.
    h.orch.on_authorize(
        |body| match body["token"].as_str().and_then(sub_of).as_deref() {
            Some(sub) => allowed(sub),
            None => refused("forbidden"),
        },
    );
    h.publish(&access, &json!({"tenant_id": TENANT, "member_sub": "ana"}))
        .await;
    let status = a.frame_of("status").await;
    assert_eq!(
        status,
        json!({"type": "status", "channel": table_id("items"), "live": true})
    );
    let state = a.frame_of("presence_state").await;
    assert_eq!(state["members"].as_array().unwrap().len(), 2, "{state}");
}

#[tokio::test]
async fn an_access_event_whose_tenant_isnt_its_subjects_is_ignored() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    let before = h.orch.authorize_calls_for("ana");
    let other = Uuid::from_u128(77);
    // Published on another tenant's subject, naming ana's tenant in the body.
    h.publish(
        &format!("chelabase.rt.access.{other}"),
        &json!({"tenant_id": TENANT, "member_sub": "ana"}),
    )
    .await;
    h.publish(&format!("chelabase.rt.access.{TENANT}"), &json!("garbage"))
        .await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(h.orch.authorize_calls_for("ana"), before);
}

// --- Presence -------------------------------------------------------------------------------------

#[tokio::test]
async fn a_changed_batch_rechecks_every_selected_member() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    let mut b = h.authed("bob").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    assert_eq!(
        b.join_table("1", "items", Some("billing")).await["type"],
        "joined"
    );
    a.send(json!({"type": "presence", "channel": table_id("items"), "state": {"row": "[\"5\"]", "column": null, "editing": false}}))
        .await;
    let seen = b.frames_within(Duration::from_millis(700)).await;
    assert!(
        seen.iter().any(|f| f["type"] == "presence_diff"
            && f["joins"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["state"]["row"] == "[\"5\"]")),
        "{seen:?}"
    );
    h.source.set_visible(Some("billing"), "[\"5\"]", false);
    h.source
        .emit(
            "items",
            ChannelEvent::Rows(vec![Change {
                op: ChangeOp::Changed,
                keys: None,
            }]),
        )
        .await;
    let frames = b.frames_within(Duration::from_millis(700)).await;
    assert!(
        frames.iter().any(|f| f["type"] == "presence_diff"
            && f["joins"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["sub"] == "ana" && m["state"]["row"].is_null())),
        "{frames:?}"
    );
}

#[tokio::test]
async fn the_same_state_again_sends_nothing() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    let mut b = h.authed("bob").await;
    assert_eq!(a.join_table("1", "items", None).await["type"], "joined");
    assert_eq!(b.join_table("1", "items", None).await["type"], "joined");
    let state = json!({"row": "[\"1\"]", "column": "name", "editing": false});
    a.send(json!({"type": "presence", "channel": table_id("items"), "state": state}))
        .await;
    b.frame_of("presence_diff").await;
    b.frames_within(Duration::from_millis(300)).await;
    a.send(json!({"type": "presence", "channel": table_id("items"), "state": state}))
        .await;
    let frames = b.frames_within(Duration::from_millis(400)).await;
    assert!(
        frames.iter().all(|f| f["type"] != "presence_diff"),
        "{frames:?}"
    );
}

// --- Project channels -----------------------------------------------------------------------------

#[tokio::test]
async fn presence_and_view_on_a_project_channel_are_invalid() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_project("1", PROJECT).await["type"], "joined");
    let channel = project_id(PROJECT);
    a.send(json!({"type": "presence", "channel": channel, "state": {"row": null, "column": null, "editing": false}}))
        .await;
    assert_eq!(a.frame_of("error").await["code"], "invalid");
    a.send(json!({"type": "view", "channel": channel, "keys": []}))
        .await;
    assert_eq!(a.frame_of("error").await["code"], "invalid");
}

#[tokio::test]
async fn a_left_project_gets_no_more_events() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_project("1", PROJECT).await["type"], "joined");
    a.send(json!({"type": "leave", "ref": "2", "channel": project_id(PROJECT)}))
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.publish(
        &format!("chelabase.rt.project.{PROJECT}"),
        &json!({"event": "database.deleted", "database_id": DB}),
    )
    .await;
    let frames = a.frames_within(Duration::from_millis(400)).await;
    assert!(frames.iter().all(|f| f["type"] != "project"), "{frames:?}");
    assert_eq!(
        a.join_project("3", PROJECT).await["type"],
        "joined",
        "and can join again"
    );
}

#[tokio::test]
async fn a_failed_refresh_drops_the_event_and_the_channel_carries_on() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_project("1", PROJECT).await["type"], "joined");
    h.orch
        .on_projects(|_| (503, json!({"error": {"code": "unavailable"}})));
    let subject = format!("chelabase.rt.project.{PROJECT}");
    h.publish(
        &subject,
        &json!({"event": "database.created", "database_id": Uuid::from_u128(0xd3)}),
    )
    .await;
    let frames = a.frames_within(Duration::from_millis(400)).await;
    assert!(frames.iter().all(|f| f["type"] != "project"), "{frames:?}");
    h.publish(
        &subject,
        &json!({"event": "database.deleted", "database_id": DB}),
    )
    .await;
    assert_eq!(a.frame_of("project").await["event"], "database.deleted");
}
