//! Members hub step 1, final fix round, area A: the orchestrator's RED tests over the fakes:
//! D7 (the offboarded mark at the routes), T2-1 (the console's write check per role level),
//! T2-4 (`/me` and the project list read every grant source), T8-3, T8-5, T8-6, T9-I1,
//! T9-m1–m3, and T9-H1 with EOS-5 (the project log shows only the project's own grants,
//! overrides and branches).
#![allow(dead_code, unused_imports)]

include!("common/fakes.rs");

use chelabase_metadata::personal_logins::{LoginEndpoint, RevokeReason};
use chelabase_types::access::RoleSet;
use chelabase_identity::Pool;
use chelabase_identity::mock::FakeIdentity;

async fn body_of(resp: reqwest::Response) -> Value {
    resp.json().await.unwrap()
}

async fn error_of(resp: reqwest::Response, status: u16) -> Value {
    assert_eq!(resp.status(), status);
    resp.json().await.unwrap()
}

fn code_of(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}

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

/// An access event as the store would have logged it, appended to the fake's log.
fn log_event(
    w: &MemberWorld,
    action: &str,
    project: Option<uuid::Uuid>,
    before: Option<Value>,
    after: Option<Value>,
) -> i64 {
    let mut events = w.metadata.events.lock().unwrap();
    let id = events.len() as i64 + 1;
    events.push(chelabase_metadata::members::AccessEvent {
        id,
        tenant_id: w.tenant,
        actor_member_id: None,
        action: action.into(),
        target_member_id: None,
        project_id: project,
        database_id: None,
        before,
        after,
        at: chrono::Utc::now(),
        actor_email: None,
        target_email: None,
        project_name: None,
        branch_id: None,
    });
    id
}

// ---- D7 ------------------------------------------------------------------------------------

// D7: a member whose identity user was offboarded (the mark) is refused every route but
// `GET /v1/me` with 403 `member_offboarded`, whatever its grants: tenant routes, project and
// database routes, and a new personal login. `/me` answers, with no project to use.
#[tokio::test]
async fn an_offboarded_member_is_refused_everywhere_but_me() {
    let w = member_world(Mode::SelfHosted).await;
    w.metadata.add_granted(
        w.tenant,
        "olly",
        w.project,
        PlatformRole::Developer,
        &["app"],
    );
    w.metadata
        .set_member(w.tenant, "olly", |m| m.offboarded = true);
    let routes = [
        (Method::GET, "/v1/projects".to_string(), None),
        (Method::GET, "/v1/members".to_string(), None),
        (Method::GET, format!("/v1/projects/{}", w.project), None),
        (Method::GET, format!("/v1/databases/{}", w.db), None),
        (
            Method::PUT,
            format!("/v1/databases/{}/personal-login", w.db),
            Some(json!({ "name": "olly_login" })),
        ),
    ];
    for (method, path, body) in routes {
        let resp = w
            .app
            .send_as("olly", "org-1", method.clone(), &path, body)
            .await;
        let status = resp.status().as_u16();
        let err: Value = resp.json().await.unwrap_or(Value::Null);
        assert_eq!(
            (status, code_of(&err)),
            (403, "member_offboarded"),
            "{method} {path}: {err}"
        );
    }
    let me = w
        .app
        .send_as("olly", "org-1", Method::GET, "/v1/me", None)
        .await;
    assert_eq!(me.status(), 200);
    let me = body_of(me).await;
    assert_eq!(me["projects"], json!([]), "{me}");
}

// ---- T2-1 ----------------------------------------------------------------------------------

// T2-1: the console's write check reads the requested role's own level (D1), not the member's
// top level: a Developer acting as a role a Viewer-level team gives reads, and is refused a
// write (403 `forbidden`, `developer`) before it reaches rest.
#[tokio::test]
async fn a_console_write_needs_the_role_held_at_developer() {
    let w = member_world(Mode::SelfHosted).await;
    let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
    w.metadata.add_group_role(&ep, "billing", &[]);
    w.metadata.add_group_role(&ep, "sales", &[]);
    let mix = w.metadata.add_granted(
        w.tenant,
        "mix",
        w.project,
        PlatformRole::Developer,
        &["sales"],
    );
    w.metadata.team_grant(
        mix,
        w.project,
        Grant {
            platform_role: PlatformRole::Viewer,
            roles: RoleSet::Only([PgRole::parse("billing")].into_iter().collect()),
        },
    );
    let path = format!(
        "/v1/databases/{}/branches/{}/console/query",
        w.db,
        w.dev.as_uuid()
    );
    let insert = json!({ "v": 1, "op": "insert", "into": "notes", "values": [{ "body": "x" }] });
    let resp = w
        .app
        .send_as(
            "mix",
            "org-1",
            Method::POST,
            &path,
            Some(json!({ "role": "billing", "tree": insert })),
        )
        .await;
    // Not refused, the write goes on past the check (here to the branch's database list, which
    // the world leaves empty: a 404 after the check).
    let status = resp.status().as_u16();
    let err: Value = resp.json().await.unwrap_or(Value::Null);
    assert_eq!(
        (status, code_of(&err), err["error"]["required"].as_str()),
        (403, "forbidden", Some("developer")),
        "{err}"
    );
}

// ---- T2-4 ----------------------------------------------------------------------------------

// T2-4: `/me` and the project list read every grant source (`GrantSources`), not only the own
// grants: a member reached only through a (fake) team grant sees the project, at the team's
// level.
#[tokio::test]
async fn a_team_only_member_sees_its_projects() {
    let w = member_world(Mode::SelfHosted).await;
    let tam = w
        .metadata
        .add_member(w.tenant, "tam", MemberStatus::Active, false);
    w.metadata.team_grant(
        tam,
        w.project,
        Grant {
            platform_role: PlatformRole::Developer,
            roles: RoleSet::Only(Default::default()),
        },
    );
    let me = body_of(
        w.app
            .send_as("tam", "org-1", Method::GET, "/v1/me", None)
            .await,
    )
    .await;
    let projects = me["projects"].as_array().cloned().unwrap_or_default();
    assert!(
        projects
            .iter()
            .any(|p| p["project_id"] == json!(w.project) && p["platform_role"] == "developer"),
        "/me misses the team's project: {me}"
    );
    let list = body_of(
        w.app
            .send_as("tam", "org-1", Method::GET, "/v1/projects", None)
            .await,
    )
    .await;
    assert!(
        list.as_array()
            .is_some_and(|l| l.iter().any(|p| p["id"] == json!(w.project))),
        "the project list misses the team's project: {list}"
    );
}

// ---- T8-3, T8-5, T8-6 ------------------------------------------------------------------------

// T8-3: a suspension's side effects don't hang on the member as the handler read it before the
// write: a retry (or a suspension that raced another) still tells the proxies about the
// member's rows and reconciles their projects (both idempotent); only the event is once.
#[tokio::test]
async fn a_suspension_retry_still_tells_the_proxies_and_reconciles() {
    let (w, mock) = zworld().await;
    let sub = mock.with_user(&Pool::Members, "bob@x.com", "Secret-pass1");
    let bob = w
        .metadata
        .add_granted(w.tenant, &sub, w.project, PlatformRole::Developer, &["app"]);
    let login = w.metadata.seed_personal_login(bob, w.db, "bob_login");
    let rows: Vec<(String, String)> = login
        .endpoints
        .iter()
        .map(|r| (r.endpoint_id.clone(), r.role_name.clone()))
        .collect();
    let path = format!("/v1/members/{bob}/suspend");
    assert_eq!(
        w.app
            .send_as(DEFAULT_SUB, "org-1", Method::POST, &path, None)
            .await
            .status(),
        200
    );
    // Both rounds of the first suspension's publish (now and 1 s later) are in before the
    // retry, so what the cache hears next is the retry's.
    let twice = |sent: &[(String, String)]| {
        rows.iter()
            .all(|r| sent.iter().filter(|s| *s == r).count() >= 2)
    };
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let notified = w.app.cache.published.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if twice(&w.app.cache.sent()) {
            break;
        }
        tokio::time::timeout_at(deadline, notified)
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "the first suspension's two rounds: {:?}",
                    w.app.cache.sent()
                )
            });
    }
    w.jobs.member_role_enqueues.lock().unwrap().clear();
    w.app.cache.clear();
    let events = w.metadata.events.lock().unwrap().len();
    assert_eq!(
        w.app
            .send_as(DEFAULT_SUB, "org-1", Method::POST, &path, None)
            .await
            .status(),
        200
    );
    let enqueued = w.jobs.member_role_enqueues.lock().unwrap().clone();
    for b in [BranchId::from(w.db), w.dev] {
        assert!(
            enqueued.contains(&b),
            "the retry reconciled nothing: {b}: {enqueued:?}"
        );
    }
    assert!(
        w.app.cache.wait_for(&rows).await,
        "the retry told no proxy: {:?}",
        w.app.cache.sent()
    );
    assert_eq!(
        w.metadata.events.lock().unwrap().len(),
        events,
        "no second event"
    );
}

// T8-5: when main's synchronous reconcile doesn't answer `Done` (it failed, or the worker
// didn't answer in time), main is enqueued like the other branches, so the login converges
// there without waiting for the sweep; `ready_on_main` is false.
#[tokio::test]
async fn a_main_reconcile_that_isnt_done_is_enqueued() {
    for reply in [
        Ok(MemberRolesReply::Failed {
            message: "the compute is starting".into(),
        }),
        Err(ResetError::Timeout),
    ] {
        let (w, mock) = zworld().await;
        let sub = mock.with_user(&Pool::Members, "ann@x.com", "Secret-pass1");
        w.metadata
            .add_granted(w.tenant, &sub, w.project, PlatformRole::Developer, &["app"]);
        *w.jobs.member_reconcile_reply.lock().unwrap() = Some(reply.clone());
        let resp = w
            .app
            .send_as(
                &sub,
                "org-1",
                Method::PUT,
                &format!("/v1/databases/{}/personal-login", w.db),
                Some(json!({ "name": "ann_login" })),
            )
            .await;
        assert_eq!(resp.status(), 201, "{reply:?}");
        assert_eq!(body_of(resp).await["ready_on_main"], false);
        let enqueued = w.jobs.member_role_enqueues.lock().unwrap().clone();
        assert!(
            enqueued.contains(&BranchId::from(w.db)),
            "{reply:?}: main isn't enqueued: {enqueued:?}"
        );
    }
}

// T8-6: revoke-now's `revoked` is what its write revoked, a login made between the handler's
// reads and the write included (the count the metadata returns, not a read beforehand).
#[tokio::test]
async fn revoke_now_counts_what_its_write_revoked() {
    let (w, mock) = zworld().await;
    let sub = mock.with_user(&Pool::Members, "cal@x.com", "Secret-pass1");
    let cal = w
        .metadata
        .add_granted(w.tenant, &sub, w.project, PlatformRole::Developer, &["app"]);
    w.metadata.seed_personal_login(cal, w.db, "cal_login");
    let mut web2 = sample_database(w.tenant);
    web2.project_id = w.project;
    web2.name = "web2".into();
    let web2_id = web2.database_id;
    w.metadata.insert_database(web2);
    w.metadata.race_once(move |m| {
        m.seed_personal_login(cal, web2_id, "cal_late");
    });
    let body = body_of(
        w.app
            .send_as(
                DEFAULT_SUB,
                "org-1",
                Method::POST,
                &format!("/v1/members/{cal}/revoke-postgres-access"),
                None,
            )
            .await,
    )
    .await;
    assert_eq!(body["revoked"], json!(2), "{body}");
}

// ---- T9-I1 ---------------------------------------------------------------------------------

fn grants_body(w: &MemberWorld) -> Value {
    json!([{ "project_id": w.project, "platform_role": "developer", "pg_roles": [] }])
}

// T9-I1: once an invite, an addition or an approval committed (and a link invite's code was
// made), nothing it reads afterwards can turn the answer into a 500: `access` comes from the
// grants it wrote and what it read before. The roster's read failing here loses no link.
#[tokio::test]
async fn an_access_write_never_answers_500_after_its_commit() {
    let (w, mock) = zworld().await;
    *w.metadata.fail_member_project_roles.lock().unwrap() = true;

    let link = json!({ "email": "link@x.com", "grants": grants_body(&w), "delivery": "link" });
    let resp = w
        .app
        .send_as(
            DEFAULT_SUB,
            "org-1",
            Method::POST,
            "/v1/invites",
            Some(link),
        )
        .await;
    assert_eq!(
        resp.status(),
        201,
        "a link invite answered without its link"
    );
    let body = body_of(resp).await;
    assert!(
        body["invite_url"]
            .as_str()
            .is_some_and(|u| u.contains("/accept-invite?")),
        "{body}"
    );
    assert_eq!(
        body["member"]["access"]["projects"][0]["project_id"],
        json!(w.project),
        "{body}"
    );

    mock.with_user(&Pool::Members, "old@x.com", "Secret-pass1");
    let add = json!({ "email": "old@x.com", "grants": grants_body(&w) });
    let resp = w
        .app
        .send_as(DEFAULT_SUB, "org-1", Method::POST, "/v1/invites", Some(add))
        .await;
    assert_eq!(
        resp.status(),
        201,
        "an existing account's invite answered 500 after its commit"
    );
    assert_eq!(body_of(resp).await["outcome"], "invited");

    let pending = w
        .metadata
        .add_member(w.tenant, "pend", MemberStatus::Pending, false);
    let resp = w
        .app
        .send_as(
            DEFAULT_SUB,
            "org-1",
            Method::POST,
            &format!("/v1/members/{pending}/approve"),
            Some(json!({ "grants": grants_body(&w) })),
        )
        .await;
    assert_eq!(
        resp.status(),
        200,
        "an approval answered 500 after its commit"
    );
}

// ---- T9-m1, T9-m2, T9-m3 -------------------------------------------------------------------

fn role_events(w: &MemberWorld) -> Vec<chelabase_metadata::members::AccessEvent> {
    w.metadata
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.action.starts_with("role_"))
        .cloned()
        .collect()
}

// T9-m1: a role call the worker didn't answer in time may have been made: it is logged, as not
// applied, like `not_applied`.
#[tokio::test]
async fn a_role_call_that_timed_out_is_logged_as_not_applied() {
    let w = member_world(Mode::SelfHosted).await;
    let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
    w.metadata
        .add_role(&ep, "postgres", RoleOrigin::Owner, true, false);
    *w.jobs.role_reply.lock().unwrap() = Some(Err(ResetError::Timeout));
    let roles = format!("/v1/databases/{}/branches/{}/roles", w.db, w.dev);
    let resp = w
        .app
        .send_as(
            DEFAULT_SUB,
            "org-1",
            Method::POST,
            &roles,
            Some(json!({ "name": "fresh" })),
        )
        .await;
    assert_eq!(resp.status(), 504);
    let events = role_events(&w);
    assert_eq!(events.len(), 1, "a timed-out create wrote no event");
    assert_eq!(events[0].after.as_ref().unwrap()["applied"], false);
}

// T9-m2: an update event names only the fields the request changed: no `login` key when the
// request doesn't change it (it is never `null`).
#[tokio::test]
async fn an_update_event_has_no_login_it_didnt_change() {
    let w = member_world(Mode::SelfHosted).await;
    let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
    w.metadata
        .add_role(&ep, "postgres", RoleOrigin::Owner, true, false);
    w.metadata
        .add_role(&ep, "app", RoleOrigin::Api, false, false);
    let path = format!("/v1/databases/{}/branches/{}/roles/app", w.db, w.dev);
    let resp = w
        .app
        .send_as(
            DEFAULT_SUB,
            "org-1",
            Method::PATCH,
            &path,
            Some(json!({ "connection_limit": 5 })),
        )
        .await;
    assert_eq!(resp.status(), 202);
    let after = role_events(&w)[0].after.clone().unwrap();
    assert!(after.get("login").is_none(), "{after}");
}

// T9-m3: a delete event says what was deleted: the role's `login` and `attributes` as they
// were (read before the delete), besides the successor.
#[tokio::test]
async fn a_delete_event_names_what_was_deleted() {
    let w = member_world(Mode::SelfHosted).await;
    let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
    w.metadata
        .add_role(&ep, "postgres", RoleOrigin::Owner, true, false);
    w.metadata
        .add_role(&ep, "app", RoleOrigin::Api, false, false);
    let path = format!("/v1/databases/{}/branches/{}/roles/app", w.db, w.dev);
    assert_eq!(
        w.app
            .send_as(DEFAULT_SUB, "org-1", Method::DELETE, &path, None)
            .await
            .status(),
        200
    );
    let after = role_events(&w)[0].after.clone().unwrap();
    assert_eq!(after["role"], "app");
    assert_eq!(after["login"], true, "{after}");
    assert!(after["attributes"].is_object(), "{after}");
}

// ---- T9-H1, EOS-5 --------------------------------------------------------------------------

// T9-H1, EOS-5: an event in a project's log shows only that project's part of it: its grants
// in `before`/`after.grants`, the overrides on its databases, and the branches of its
// databases in `rotated_branches`. The tenant's own log keeps every part.
#[tokio::test]
async fn the_project_log_shows_only_the_projects_part_of_an_event() {
    let w = member_world(Mode::SelfHosted).await;
    let sales = w
        .metadata
        .create_project(w.tenant, "Sales")
        .await
        .unwrap()
        .id;
    let mut db2 = sample_database(w.tenant);
    db2.project_id = sales;
    db2.name = "crm".into();
    let (db2_id, main2) = (db2.database_id, db2.main.id);
    w.metadata.insert_database(db2);
    let main1 = BranchId::from(w.db);
    let grant = |p: ProjectId| json!({ "project_id": p, "platform_role": "developer", "pg_roles": ["app"] });
    let o =
        |d: DatabaseId| json!({ "database_id": d, "platform_role": "viewer", "pg_roles": null });
    let added = log_event(
        &w,
        "member_added",
        None,
        None,
        Some(
            json!({ "status": "active", "email": "x@x.com", "grants": [grant(w.project), grant(sales)] }),
        ),
    );
    let removed = log_event(
        &w,
        "member_removed",
        None,
        Some(json!({ "status": "active", "tenant_admin": false,
                     "grants": [grant(w.project), grant(sales)], "overrides": [o(w.db), o(db2_id)] })),
        Some(json!({ "status": "removed", "tenant_admin": false })),
    );
    let rotated = log_event(
        &w,
        "personal_logins_revoked",
        Some(w.project.as_uuid()),
        None,
        Some(json!({ "count": 0, "reason": "revoked_now", "logins": [],
                     "rotated_branches": [main1, w.dev, main2] })),
    );
    let page = body_of(
        w.app
            .send_as(
                DEFAULT_SUB,
                "org-1",
                Method::GET,
                &format!("/v1/projects/{}/access-events", w.project),
                None,
            )
            .await,
    )
    .await;
    let event = |id: i64| {
        page["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["id"] == json!(id))
            .cloned()
            .unwrap_or_else(|| panic!("event {id} missing: {page}"))
    };
    assert_eq!(
        event(added)["after"]["grants"],
        json!([grant(w.project)]),
        "another project's grant shows"
    );
    assert_eq!(
        event(removed)["before"]["grants"],
        json!([grant(w.project)])
    );
    assert_eq!(
        event(removed)["before"]["overrides"],
        json!([o(w.db)]),
        "another project's override shows"
    );
    assert_eq!(
        event(rotated)["after"]["rotated_branches"],
        json!([main1, w.dev]),
        "another project's branch shows"
    );

    let tenant = body_of(
        w.app
            .send_as(DEFAULT_SUB, "org-1", Method::GET, "/v1/access-events", None)
            .await,
    )
    .await;
    let whole = tenant["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["id"] == json!(rotated))
        .cloned()
        .unwrap();
    assert_eq!(
        whole["after"]["rotated_branches"],
        json!([main1, w.dev, main2]),
        "the tenant's log is whole"
    );
}
