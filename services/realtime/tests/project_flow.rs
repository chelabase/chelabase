//! RT1 T7: `project:` channels end to end (spec §5.3, R17, R21, W34): the join against
//! `/internal/realtime/projects`, events narrowed per database, the list refresh for an unknown
//! database, and the per-second dedup.

mod common;

use std::time::Duration;

use chelabase_nats::rt::ProjectEvent;
use common::*;
use serde_json::{Value, json};
use uuid::Uuid;

fn subject(project: Uuid) -> String {
    format!("chelabase.rt.project.{project}")
}

fn event(e: &ProjectEvent) -> Value {
    serde_json::to_value(e).unwrap()
}

#[tokio::test]
async fn a_visible_project_joins() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    let joined = a.join_project("1", PROJECT).await;
    assert_eq!(
        joined,
        json!({"type": "joined", "ref": "1", "channel": project_id(PROJECT), "info": {}})
    );
    let calls = h.orch.projects_calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        sub_of(calls[0]["token"].as_str().unwrap()).as_deref(),
        Some("ana")
    );
    assert!(
        h.orch.authorize_calls().is_empty(),
        "projects, not authorize"
    );
}

#[tokio::test]
async fn an_invisible_project_or_a_failing_list_is_refused() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    let other = Uuid::from_u128(99);
    assert_eq!(a.join_project("1", other).await["code"], "forbidden");
    h.orch
        .on_projects(|_| (403, json!({"error": {"code": "forbidden"}})));
    assert_eq!(
        a.join_project("2", PROJECT).await["code"],
        "forbidden",
        "not an active member"
    );
    h.orch
        .on_projects(|_| (503, json!({"error": {"code": "unavailable"}})));
    assert_eq!(a.join_project("3", PROJECT).await["code"], "unavailable");
}

#[tokio::test]
async fn events_for_visible_databases_arrive_flattened() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_project("1", PROJECT).await["type"], "joined");
    let ev = ProjectEvent::BranchCreated {
        database_id: DB,
        branch_id: BR,
    };
    h.publish(&subject(PROJECT), &event(&ev)).await;
    assert_eq!(
        a.frame_of("project").await,
        json!({"type": "project", "channel": project_id(PROJECT), "event": "branch.created", "database_id": DB, "branch_id": BR})
    );
}

#[tokio::test]
async fn other_projects_events_dont_arrive() {
    let h = Harness::start().await;
    let other = Uuid::from_u128(99);
    h.orch
        .on_projects(move |_| projects_of(&[(PROJECT, &[DB]), (other, &[Uuid::from_u128(5)])]));
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_project("1", PROJECT).await["type"], "joined");
    h.publish(
        &subject(other),
        &event(&ProjectEvent::DatabaseCreated {
            database_id: Uuid::from_u128(5),
        }),
    )
    .await;
    let frames = a.frames_within(Duration::from_millis(400)).await;
    assert!(
        frames.iter().all(|f| f["type"] != "project"),
        "not joined to that project: {frames:?}"
    );
}

#[tokio::test]
async fn an_unknown_database_refreshes_the_list_first() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_project("1", PROJECT).await["type"], "joined");
    let fresh = Uuid::from_u128(0xd2);
    // Still not visible after the refresh: dropped.
    h.publish(
        &subject(PROJECT),
        &event(&ProjectEvent::DatabaseCreated { database_id: fresh }),
    )
    .await;
    let frames = a.frames_within(Duration::from_millis(500)).await;
    assert!(
        frames.iter().all(|f| f["type"] != "project"),
        "never delivered by default: {frames:?}"
    );
    assert_eq!(
        h.orch.projects_calls().len(),
        2,
        "the join, then one refresh"
    );
    // Now the person may see it (granted meanwhile): the refresh delivers it.
    h.orch
        .on_projects(move |_| projects_of(&[(PROJECT, &[DB, fresh])]));
    let ev = ProjectEvent::BranchStatus {
        database_id: fresh,
        branch_id: BR,
        status: "ready".into(),
    };
    h.publish(&subject(PROJECT), &event(&ev)).await;
    let got = a.frame_of("project").await;
    assert_eq!(
        (got["event"].as_str(), got["database_id"].clone()),
        (Some("branch.status"), json!(fresh))
    );
}

#[tokio::test]
async fn a_storm_of_identical_events_is_sent_once() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_project("1", PROJECT).await["type"], "joined");
    let made = ProjectEvent::PgDatabaseCreated {
        database_id: DB,
        branch_id: BR,
        name: "tmp".into(),
    };
    let dropped = ProjectEvent::PgDatabaseDropped {
        database_id: DB,
        branch_id: BR,
        name: "tmp".into(),
    };
    for _ in 0..5 {
        h.publish(&subject(PROJECT), &event(&made)).await;
        h.publish(&subject(PROJECT), &event(&dropped)).await;
    }
    let frames: Vec<Value> = a
        .frames_within(Duration::from_millis(600))
        .await
        .into_iter()
        .filter(|f| f["type"] == "project")
        .collect();
    let kinds: Vec<&str> = frames
        .iter()
        .map(|f| f["event"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        vec!["pg_database.created", "pg_database.dropped"],
        "each distinct event once, in arrival order"
    );
}

#[tokio::test]
async fn a_malformed_event_is_ignored() {
    let h = Harness::start().await;
    let mut a = h.authed("ana").await;
    assert_eq!(a.join_project("1", PROJECT).await["type"], "joined");
    h.publish(
        &subject(PROJECT),
        &json!({"event": "branch.renamed", "database_id": DB}),
    )
    .await;
    h.publish(&subject(PROJECT), &json!("garbage")).await;
    let ev = ProjectEvent::DatabaseDeleted { database_id: DB };
    h.publish(&subject(PROJECT), &event(&ev)).await;
    assert_eq!(
        a.frame_of("project").await["event"],
        "database.deleted",
        "the socket carries on"
    );
}
