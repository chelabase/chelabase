//! Members hub T10 (coder's edge cases): a role delete's `?successor=`, beyond the RED set.
#![allow(dead_code, unused_imports)]

include!("common/fakes.rs");

async fn world() -> (MemberWorld, String, String) {
    let w = member_world(Mode::SelfHosted).await;
    let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
    w.metadata.add_role(&ep, "postgres", RoleOrigin::Owner, true, false);
    w.metadata.add_role(&ep, "app", RoleOrigin::Api, false, false);
    w.metadata.add_role(&ep, "heir", RoleOrigin::Sql, false, false);
    let roles = format!("/v1/databases/{}/branches/{}/roles", w.db, w.dev);
    (w, roles, ep)
}

async fn delete(w: &MemberWorld, path: &str) -> reqwest::Response {
    w.app.send_as(DEFAULT_SUB, "org-1", Method::DELETE, path, None).await
}

fn role_events(w: &MemberWorld) -> usize {
    w.metadata.events.lock().unwrap().iter().filter(|e| e.action.starts_with("role_")).count()
}

// A successor named twice is a malformed query: refused before the worker, no event.
#[tokio::test]
async fn a_successor_named_twice_is_refused() {
    let (w, roles, _) = world().await;
    let resp = delete(&w, &format!("{roles}/app?successor=heir&successor=chelabase_orphaned")).await;
    assert_eq!(resp.status(), 422);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "invalid_request", "{body}");
    assert!(w.jobs.role_calls.lock().unwrap().is_empty());
    assert_eq!(role_events(&w), 0);
}

// A worker from before T10 answers no `reassigned`: the answer still has the shape, with
// nothing listed.
#[tokio::test]
async fn an_answer_without_reassigned_is_zero() {
    let (w, roles, _) = world().await;
    let resp = delete(&w, &format!("{roles}/app?successor=heir")).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, json!({ "successor": "heir", "reassigned": { "count": 0, "definers": [] } }));
}

// The worker unreachable: no event, 503, and the successor was still checked first.
#[tokio::test]
async fn a_transport_failure_logs_nothing() {
    let (w, roles, _) = world().await;
    *w.jobs.role_reply.lock().unwrap() = Some(Err(ResetError::Transport("down".into())));
    let resp = delete(&w, &format!("{roles}/app?successor=heir")).await;
    assert_eq!(resp.status(), 503);
    assert_eq!(role_events(&w), 0);
}

// A role recorded in SQL as owner-level (metadata level) is refused before the worker, a
// plain one with the same origin isn't.
#[tokio::test]
async fn an_owner_level_sql_role_is_refused_from_metadata() {
    let (w, roles, ep) = world().await;
    w.metadata.add_role(&ep, "sql_boss", RoleOrigin::Sql, true, false);
    let resp = delete(&w, &format!("{roles}/app?successor=sql_boss")).await;
    assert_eq!(resp.status(), 422);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "invalid_successor", "{body}");
    assert!(w.jobs.role_calls.lock().unwrap().is_empty());
    assert_eq!(delete(&w, &format!("{roles}/app?successor=heir")).await.status(), 200);
}
