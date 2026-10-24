//! Members hub T10: a role delete names the role its objects go to (`?successor=`), never the
//! owner. `DELETE …/roles/{role}?successor=<role>|chelabase_orphaned`; omitted means
//! `chelabase_orphaned` (the quarantine role). A successor that is the role itself, the owner,
//! reserved, owner-level or a personal login is refused here with 422 `invalid_successor` before
//! the worker is asked; the worker's own refusal (a role the owner can't SET to, or one SQL
//! shows owner-level or marked) maps to the same. A done delete answers 200 with
//! `{successor, reassigned: {count, definers}}`, and its `role_deleted` event carries
//! `after.successor`.
#![allow(dead_code, unused_imports)]

include!("common/fakes.rs");

const QUARANTINE: &str = "chelabase_orphaned";

async fn error_of(resp: reqwest::Response, status: u16) -> Value {
    assert_eq!(resp.status(), status);
    resp.json().await.unwrap()
}

fn code_of(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap()
}

fn refused(code: &str) -> RoleReply {
    RoleReply { ok: false, code: Some(code.into()), error: Some("worker said so".into()), keys: vec![], reassigned: None, preview: None }
}

fn done(count: u64, definers: &[&str]) -> RoleReply {
    RoleReply {
        ok: true,
        code: None,
        error: None,
        keys: vec![],
        reassigned: Some(chelabase_nats::Reassigned {
            count,
            definers: definers.iter().map(|d| d.to_string()).collect(),
            confirmed: vec![],
        }),
        preview: None,
    }
}

/// A member world with the owner's row, an API group role `app` and a few roles to hand to.
async fn world() -> (MemberWorld, String) {
    let w = member_world(Mode::SelfHosted).await;
    let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
    w.metadata.add_role(&ep, "postgres", RoleOrigin::Owner, true, false);
    w.metadata.add_role(&ep, "app", RoleOrigin::Api, false, false);
    w.metadata.add_role(&ep, "heir", RoleOrigin::Sql, false, false);
    w.metadata.add_role(&ep, "My Heir", RoleOrigin::Sql, false, false);
    w.metadata.add_role(&ep, "boss", RoleOrigin::Api, true, false);
    w.metadata.add_role(&ep, "alice_login", RoleOrigin::Personal, false, false);
    let roles = format!("/v1/databases/{}/branches/{}/roles", w.db, w.dev);
    (w, roles)
}

async fn delete(w: &MemberWorld, path: &str) -> reqwest::Response {
    w.app.send_as(DEFAULT_SUB, "org-1", Method::DELETE, path, None).await
}

fn successors(w: &MemberWorld) -> Vec<String> {
    w.jobs.role_deletes.lock().unwrap().iter().map(|r| r.successor.clone()).collect()
}

fn role_events(w: &MemberWorld) -> Vec<chelabase_metadata::members::AccessEvent> {
    w.metadata.events.lock().unwrap().iter().filter(|e| e.action.starts_with("role_")).cloned().collect()
}

// Omitted: the quarantine role, never the owner.
#[tokio::test]
async fn an_omitted_successor_is_the_quarantine_role() {
    let (w, roles) = world().await;
    let resp = delete(&w, &format!("{roles}/app")).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(successors(&w), [QUARANTINE]);
    // Named explicitly, it's accepted too (the one reserved name a successor may be).
    let resp = delete(&w, &format!("{roles}/app?successor={QUARANTINE}")).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(successors(&w), [QUARANTINE, QUARANTINE]);
}

// A named successor reaches the worker as decoded once.
#[tokio::test]
async fn a_named_successor_reaches_the_worker() {
    let (w, roles) = world().await;
    assert_eq!(delete(&w, &format!("{roles}/app?successor=heir")).await.status(), 200);
    assert_eq!(delete(&w, &format!("{roles}/app?successor=My%20Heir")).await.status(), 200);
    assert_eq!(successors(&w), ["heir", "My Heir"]);
    let names: Vec<String> = w.jobs.role_deletes.lock().unwrap().iter().map(|r| r.name.clone()).collect();
    assert_eq!(names, ["app", "app"]);
}

// Red-team "privilege ops" (MH-28) for a successor: never the role itself, the owner, a
// reserved name (any case; `anon`/`authenticated` included: they serve the data API), a member
// role, a personal login (MH-18), an owner-level role, or a name Postgres can't hold. Refused
// before the worker is asked, and nothing is logged.
#[tokio::test]
async fn refused_successors_never_reach_the_worker() {
    let (w, roles) = world().await;
    let long = "x".repeat(64);
    for successor in [
        "app",
        "postgres",
        "owner",
        "cloud_admin",
        "neon_superuser",
        "authenticator",
        "anon",
        "authenticated",
        "public",
        "none",
        "pg_read_all_data",
        "PG_MONITOR",
        "chelabase_admin",
        "chelabase_schema_locked",
        "Chelabase_Orphaned2",
        "CHELABASE_ORPHANED",
        "cb_m_0123",
        "CB_M_x",
        "alice_login",
        "boss",
        "",
        "a%00b",
        long.as_str(),
    ] {
        let path = format!("{roles}/app?successor={successor}");
        let err = error_of(delete(&w, &path).await, 422).await;
        assert_eq!(code_of(&err), "invalid_successor", "{successor:?}");
    }
    assert!(w.jobs.role_calls.lock().unwrap().is_empty(), "{:?}", w.jobs.role_calls.lock().unwrap());
    assert!(role_events(&w).is_empty());
}

// The same rule for a role with a quoted name: itself is refused, by its decoded name.
#[tokio::test]
async fn a_quoted_role_cant_succeed_itself() {
    let (w, roles) = world().await;
    let err = error_of(delete(&w, &format!("{roles}/My%20Heir?successor=My%20Heir")).await, 422).await;
    assert_eq!(code_of(&err), "invalid_successor");
    assert!(w.jobs.role_calls.lock().unwrap().is_empty());
}

// The worker decides what only SQL knows (SET reach, owner-level by SQL, the marker): its
// refusal is 422 `invalid_successor`, with no worker text and no event.
#[tokio::test]
async fn the_workers_refusal_is_invalid_successor() {
    let (w, roles) = world().await;
    *w.jobs.role_reply.lock().unwrap() = Some(Ok(refused("invalid_successor")));
    let err = error_of(delete(&w, &format!("{roles}/app?successor=heir")).await, 422).await;
    assert_eq!(code_of(&err), "invalid_successor");
    assert!(!err.to_string().contains("worker said so"), "{err}");
    assert!(role_events(&w).is_empty());
}

// The answer lists what moved, the SECURITY DEFINER routines among it for review.
#[tokio::test]
async fn the_answer_lists_what_was_reassigned() {
    let (w, roles) = world().await;
    *w.jobs.role_reply.lock().unwrap() = Some(Ok(done(4, &["public.planted()", "\"Odd\".\"f|g\"(integer)"])));
    let resp = delete(&w, &format!("{roles}/app?successor=heir")).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["successor"], "heir", "{body}");
    assert_eq!(
        body["reassigned"],
        json!({ "count": 4, "definers": ["public.planted()", "\"Odd\".\"f|g\"(integer)"] }),
        "{body}"
    );
}

// P25/R-T9a: `role_deleted` carries the successor, applied or not.
#[tokio::test]
async fn the_event_names_the_successor() {
    let (w, roles) = world().await;
    *w.jobs.role_reply.lock().unwrap() = Some(Ok(refused("not_applied")));
    let err = error_of(delete(&w, &format!("{roles}/app?successor=heir")).await, 503).await;
    assert_eq!(code_of(&err), "not_applied");
    *w.jobs.role_reply.lock().unwrap() = Some(Ok(done(1, &[])));
    assert_eq!(delete(&w, &format!("{roles}/app")).await.status(), 200);

    let events = role_events(&w);
    let after: Vec<(Value, Value, Value)> = events
        .iter()
        .map(|e| {
            let a = e.after.as_ref().unwrap();
            (a["role"].clone(), a["successor"].clone(), a["applied"].clone())
        })
        .collect();
    assert_eq!(
        after,
        [
            (json!("app"), json!("heir"), json!(false)),
            (json!("app"), json!(QUARANTINE), json!(true)),
        ]
    );
    assert!(events.iter().all(|e| e.action == "role_deleted"));
}

// Final round T10-4: a worker older than T10 ignores `successor`, hands the objects to the
// owner and answers ok without `reassigned`. That reply is an error (500 `internal`), never a
// 200 that claims the objects went to the successor (no silent owner fallback).
#[tokio::test]
async fn an_ok_reply_without_reassigned_is_an_error() {
    let (w, roles) = world().await;
    let old_worker = RoleReply { ok: true, code: None, error: None, keys: vec![], reassigned: None, preview: None };
    for path in [format!("{roles}/app?successor=heir"), format!("{roles}/app")] {
        *w.jobs.role_reply.lock().unwrap() = Some(Ok(old_worker.clone()));
        let resp = delete(&w, &path).await;
        let status = resp.status().as_u16();
        let body: Value = resp.json().await.unwrap();
        assert_eq!(status, 500, "{path}: {body}");
        assert_eq!(code_of(&body), "internal", "{path}: {body}");
        assert!(body.get("reassigned").is_none() && body.get("successor").is_none(), "{path}: {body}");
    }
}
