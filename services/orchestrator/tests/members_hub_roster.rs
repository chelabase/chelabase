//! Members hub T9, the coder's edge cases beyond `api.rs`: a role route whose worker reply is
//! `not_applied` logs its event with `after.applied: false` (R-T9a), the update event names only
//! what the request changes, the single-member responses carry `access` like the roster, and
//! the project log refuses a malformed `member_id`.
#![allow(dead_code, unused_imports)]

include!("common/fakes.rs");

use chelabase_identity::Pool;
use chelabase_identity::mock::FakeIdentity;

/// org-1's tenant with project Billing (database web1, owner `postgres`, main and `dev`) and
/// the identity fake behind the orchestrator (the member routes need it).
async fn zworld() -> (MemberWorld, FakeIdentity) {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    let mock = FakeIdentity::start().await;
    mock.with_pool(&Pool::Members);
    let app = spawn_app_identity(metadata.clone(), jobs.clone(), Some(mock.client()), Arc::new(chelabase_query::app_users::PgUsersDb::new()), None)
    .await;
    let project = metadata.create_project(tenant, "Billing").await.unwrap().id;
    let mut db = sample_database(tenant);
    db.project_id = project;
    db.name = "web1".into();
    db.database_name = Some("web1".into());
    db.owner_role = Some("postgres".into());
    let (db_id, main) = (db.database_id, db.main.id);
    metadata.insert_database(db);
    let dev = metadata.add_branch(db_id, "dev", main, BranchKind::Normal, BranchStatus::Ready);
    let w = MemberWorld {
        app,
        metadata,
        tenant,
        project,
        db: db_id,
        dev,
        merge: uuid::Uuid::nil(),
        jobs,
    };
    (w, mock)
}

async fn error_of(resp: reqwest::Response, status: u16) -> Value {
    assert_eq!(resp.status(), status);
    resp.json().await.unwrap()
}

fn code_of(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap()
}

fn role_reply(code: &str) -> RoleReply {
    RoleReply { ok: false, code: Some(code.into()), error: Some("worker said so".into()), keys: vec![], reassigned: None, preview: None }
}

fn role_events(w: &MemberWorld) -> Vec<chelabase_metadata::members::AccessEvent> {
    w.metadata.events.lock().unwrap().iter().filter(|e| e.action.starts_with("role_")).cloned().collect()
}

fn set_reply(w: &MemberWorld, reply: Option<Result<RoleReply, ResetError>>) {
    *w.jobs.role_reply.lock().unwrap() = reply;
}

// R-T9a: stored but not yet applied is logged too, as not applied; `Done` after it logs applied.
#[tokio::test]
async fn a_not_applied_role_call_logs_its_event_as_not_applied() {
    let w = member_world(Mode::SelfHosted).await;
    let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
    w.metadata.add_role(&ep, "postgres", RoleOrigin::Owner, true, false);
    w.metadata.add_role(&ep, "app", RoleOrigin::Api, false, false);
    let roles = format!("/v1/databases/{}/branches/{}/roles", w.db, w.dev);
    let send = |method: Method, path: String, body: Option<Value>| {
        let app = &w.app;
        async move { app.send_as(DEFAULT_SUB, "org-1", method, &path, body).await }
    };

    set_reply(&w, Some(Ok(role_reply("not_applied"))));
    for resp in [
        send(Method::POST, roles.clone(), Some(json!({ "name": "fresh" }))).await,
        send(Method::PATCH, format!("{roles}/app"), Some(json!({ "connection_limit": 5 }))).await,
        send(Method::DELETE, format!("{roles}/app"), None).await,
    ] {
        assert_eq!(code_of(&error_of(resp, 503).await), "not_applied");
    }
    let events = role_events(&w);
    let actions: Vec<&str> = events.iter().map(|e| e.action.as_str()).collect();
    assert_eq!(actions, ["role_created", "role_updated", "role_deleted"]);
    for e in &events {
        assert_eq!(e.after.as_ref().unwrap()["applied"], false, "{}", e.action);
        assert_eq!(e.branch_id, Some(w.dev.as_uuid()));
        assert_eq!(e.database_id, Some(w.db.as_uuid()));
        assert_eq!(e.project_id, Some(w.project.as_uuid()));
    }

    // The retry that the worker finishes: Done logs `applied: true`.
    set_reply(&w, None);
    assert_eq!(send(Method::DELETE, format!("{roles}/app"), None).await.status(), 200, "T10: a delete answers 200");
    let last = role_events(&w).pop().unwrap();
    assert_eq!((last.action.as_str(), &last.after.as_ref().unwrap()["applied"]), ("role_deleted", &json!(true)));
    assert_eq!(role_events(&w).len(), 4);
}

// A role event names what the request changes and no more; `valid_until: null` is `cleared`.
#[tokio::test]
async fn a_role_update_event_names_only_the_changed_fields() {
    let w = member_world(Mode::SelfHosted).await;
    let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
    w.metadata.add_role(&ep, "postgres", RoleOrigin::Owner, true, false);
    w.metadata.add_role(&ep, "app", RoleOrigin::Api, false, false);
    let path = format!("/v1/databases/{}/branches/{}/roles/app", w.db, w.dev);
    let patch = |body: Value| {
        let (app, path) = (&w.app, path.clone());
        async move { app.send_as(DEFAULT_SUB, "org-1", Method::PATCH, &path, Some(body)).await }
    };
    assert_eq!(patch(json!({ "inherit": false })).await.status(), 202);
    assert_eq!(patch(json!({ "valid_until": null, "member_of": [] })).await.status(), 202);
    let events = role_events(&w);
    let first = events[0].after.as_ref().unwrap();
    assert_eq!(first["role"], "app");
    assert_eq!(first["login"], Value::Null);
    assert_eq!(first["attributes"], json!({ "inherit": false }));
    let second = events[1].after.as_ref().unwrap();
    assert_eq!(second["attributes"], json!({ "valid_until": "cleared", "member_of": [] }));
}

// Every `Member` the API answers carries `access`, not only the roster's.
#[tokio::test]
async fn a_single_member_response_carries_access() {
    let w = member_world(Mode::SelfHosted).await;
    let ann = w.metadata.add_granted(w.tenant, "ann", w.project, PlatformRole::Developer, &[]);
    let resp = w
        .app
        .send_as(DEFAULT_SUB, "org-1", Method::PATCH, &format!("/v1/members/{ann}"), Some(json!({ "tenant_admin": true })))
        .await;
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["access"]["level"], "tenant_admin", "{body}");
    assert_eq!(
        body["access"]["projects"],
        json!([{ "project_id": w.project, "platform_role": "admin", "via": "own" }]),
        "{body}"
    );
    let resp = w
        .app
        .send_as(DEFAULT_SUB, "org-1", Method::PATCH, &format!("/v1/members/{ann}"), Some(json!({ "tenant_admin": false })))
        .await;
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["access"]["level"], "developer", "{body}");
}

// `member_id` is checked on the project route too, and a tenant-less caller's filter never
// widens what the route allows.
#[tokio::test]
async fn the_project_log_refuses_a_malformed_member_id() {
    let w = member_world(Mode::SelfHosted).await;
    let path = format!("/v1/projects/{}/access-events?member_id=not-a-uuid", w.project);
    let err = error_of(w.app.send_as(DEFAULT_SUB, "org-1", Method::GET, &path, None).await, 400).await;
    assert_eq!(code_of(&err), "invalid_request");
}

// D10 (fix K): every `Member` carries `suspended_at` (date-time or null) and `suspended_by`
// (the acting member's id or null), always present.
fn assert_suspension_null(m: &Value) {
    let o = m.as_object().unwrap_or_else(|| panic!("not an object: {m}"));
    for key in ["suspended_at", "suspended_by"] {
        assert_eq!(o.get(key), Some(&Value::Null), "{key} must be present and null: {m}");
    }
}

#[tokio::test]
async fn the_roster_carries_the_suspension_of_a_suspended_member_and_null_otherwise() {
    let (w, mock) = zworld().await;
    let ann_sub = mock.with_user(&Pool::Members, "ann@x.com", "Secret-pass1");
    let bob_sub = mock.with_user(&Pool::Members, "bob@x.com", "Secret-pass1");
    let ann = w.metadata.add_granted(w.tenant, &ann_sub, w.project, PlatformRole::Developer, &[]);
    let bob = w.metadata.add_granted(w.tenant, &bob_sub, w.project, PlatformRole::Viewer, &[]);
    let send = |method: Method, path: String| {
        let app = &w.app;
        async move { app.send_as(DEFAULT_SUB, "org-1", method, &path, None).await }
    };
    let roster = || async {
        let resp = send(Method::GET, "/v1/members".into()).await;
        assert_eq!(resp.status(), 200);
        let list: Vec<Value> = resp.json().await.unwrap();
        list
    };
    let of = |list: &[Value], id: uuid::Uuid| list.iter().find(|m| m["id"] == json!(id)).cloned().unwrap();

    let list = roster().await;
    assert!(list.len() >= 3);
    for m in &list {
        assert_suspension_null(m);
    }
    let admin = w.metadata.member(w.tenant, DEFAULT_SUB).unwrap().id;

    let before = chrono::Utc::now() - chrono::Duration::seconds(5);
    assert_eq!(send(Method::POST, format!("/v1/members/{ann}/suspend")).await.status(), 200);
    let list = roster().await;
    let a = of(&list, ann);
    assert_eq!(a["status"], "suspended");
    let at: chrono::DateTime<chrono::Utc> = a["suspended_at"].as_str().unwrap_or_else(|| panic!("{a}")).parse().unwrap();
    assert!(at >= before && at <= chrono::Utc::now() + chrono::Duration::seconds(5), "{at}");
    assert_eq!(a["suspended_by"], json!(admin), "{a}");
    assert_suspension_null(&of(&list, bob));

    assert_eq!(send(Method::POST, format!("/v1/members/{ann}/reinstate")).await.status(), 200);
    assert_suspension_null(&of(&roster().await, ann));
}

#[tokio::test]
async fn every_single_member_response_carries_the_suspension_fields() {
    let (w, mock) = zworld().await;
    let ann_sub = mock.with_user(&Pool::Members, "ann@x.com", "Secret-pass1");
    let pat_sub = mock.with_user(&Pool::Members, "pat@x.com", "Secret-pass1");
    let ann = w.metadata.add_granted(w.tenant, &ann_sub, w.project, PlatformRole::Developer, &[]);
    let pat = w.metadata.add_member(w.tenant, &pat_sub, MemberStatus::Pending, false);
    let admin_send = |method: Method, path: String, body: Option<Value>| {
        let app = &w.app;
        async move { app.send_as(DEFAULT_SUB, "org-1", method, &path, body).await }
    };
    // update
    let resp = admin_send(Method::PATCH, format!("/v1/members/{ann}"), Some(json!({ "tenant_admin": true }))).await;
    assert_eq!(resp.status(), 200);
    assert_suspension_null(&resp.json::<Value>().await.unwrap());
    let resp = admin_send(Method::PATCH, format!("/v1/members/{ann}"), Some(json!({ "tenant_admin": false }))).await;
    assert_suspension_null(&resp.json::<Value>().await.unwrap());
    // approve
    let resp = admin_send(Method::POST, format!("/v1/members/{pat}/approve"), Some(json!({ "grants": [] }))).await;
    assert_eq!(resp.status(), 200);
    assert_suspension_null(&resp.json::<Value>().await.unwrap());
    // invite
    let invite = json!({ "email": "new@example.test", "grants": [{ "project_id": w.project, "platform_role": "viewer", "pg_roles": [] }] });
    let resp = admin_send(Method::POST, "/v1/invites".into(), Some(invite)).await;
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.unwrap();
    assert_suspension_null(&body["member"]);
    // suspend, then reinstate: both answer the member
    let resp = admin_send(Method::POST, format!("/v1/members/{ann}/suspend"), None).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert!(body["suspended_at"].is_string(), "{body}");
    let admin = w.metadata.member(w.tenant, DEFAULT_SUB).unwrap().id;
    assert_eq!(body["suspended_by"], json!(admin), "{body}");
    // a retried suspend keeps the first values
    let again: Value = admin_send(Method::POST, format!("/v1/members/{ann}/suspend"), None).await.json().await.unwrap();
    assert_eq!((&again["suspended_at"], &again["suspended_by"]), (&body["suspended_at"], &body["suspended_by"]));
    let resp = admin_send(Method::POST, format!("/v1/members/{ann}/reinstate"), None).await;
    assert_eq!(resp.status(), 200);
    assert_suspension_null(&resp.json::<Value>().await.unwrap());
}
