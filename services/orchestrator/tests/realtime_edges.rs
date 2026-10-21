//! RT1 T6, the coder's edge cases beyond `realtime.rs`: revocation paths a re-authorization
//! must see (a deleted branch, a grant taken away, a member removed), request-shape refusals,
//! the key cap's boundary and `/projects`' refusals.
#![allow(dead_code, unused_imports)]

include!("common/fakes.rs");

use chelabase_auth::test_support::MockJwks;

const AUTHORIZE: &str = "/internal/realtime/authorize";
const PROJECTS: &str = "/internal/realtime/projects";
const VISIBLE: &str = "/internal/realtime/visible";

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// org-1's tenant: project P with database `web1` (data API on, owner `postgres`, its owner
/// password sealed), a ready `dev` branch; `dev1` a Developer holding `app`.
struct World {
    app: TestApp,
    metadata: FakeMetadata,
    jwks: MockJwks,
    jobs: FakeJobs,
    tenant: TenantId,
    project: ProjectId,
    db: DatabaseId,
    dev: BranchId,
}

async fn world() -> World {
    let jwks = MockJwks::start().await;
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app_oidc(
        metadata.clone(),
        jobs.clone(),
        "http://127.0.0.1:9",
        Mode::SelfHosted,
        jwks.config(vec![]),
    )
    .await;
    let project = metadata.create_project(tenant, "P").await.unwrap().id;
    let mut db = sample_database(tenant);
    db.project_id = project;
    db.name = "web1".into();
    db.database_name = Some("web1".into());
    db.owner_role = Some("postgres".into());
    db.main.owner_password_enc = Some(seal_credential(
        &app.key,
        &db.main.id.to_string(),
        Purpose::Owner,
        "pw",
    ));
    let (db_id, main) = (db.database_id, db.main.id);
    metadata.insert_database(db);
    metadata.add_branch_db(main, "web1", "postgres", DatabaseOrigin::Main);
    let dev = metadata.add_branch(db_id, "dev", main, BranchKind::Normal, BranchStatus::Ready);
    metadata.add_branch_db(dev, "web1", "postgres", DatabaseOrigin::Main);
    for rows in metadata.branch_dbs.lock().unwrap().values_mut() {
        for row in rows.iter_mut() {
            row.data_api = chelabase_metadata::DataApiState::On;
        }
    }
    metadata.add_granted(tenant, "dev1", project, PlatformRole::Developer, &["app"]);
    World {
        app,
        metadata,
        jwks,
        jobs,
        tenant,
        project,
        db: db_id,
        dev,
    }
}

impl World {
    fn main(&self) -> BranchId {
        BranchId::from(self.db)
    }

    fn token(&self, sub: &str) -> String {
        self.jwks.sign(json!({
            "sub": sub,
            "iss": self.jwks.issuer(),
            "exp": now() + 300,
            chelabase_auth::CLAIM_ORG_ID: "org-1",
        }))
    }

    async fn post(&self, path: &str, body: Value) -> (u16, Value) {
        let resp = reqwest::Client::new()
            .post(format!("{}{path}", self.app.base))
            .bearer_auth(REALTIME_TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    async fn reason(&self, sub: &str, branch: BranchId, role: Option<&str>) -> Value {
        let target = json!({ "database_id": self.db, "branch_id": branch, "pg_database": "web1", "role": role });
        let (status, body) = self
            .post(
                AUTHORIZE,
                json!({ "token": self.token(sub), "target": target }),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        if body["allowed"] == json!(true) {
            json!("allowed")
        } else {
            body["reason"].clone()
        }
    }
}

// Revocation paths (red-team "Tickets & auth"): what a re-authorization sees after each.
#[tokio::test]
async fn revocations_show_on_the_next_authorize() {
    let w = world().await;
    assert_eq!(w.reason("dev1", w.dev, Some("app")).await, json!("allowed"));
    assert_eq!(w.reason(DEFAULT_SUB, w.dev, None).await, json!("allowed"));

    // The branch deleted: not_found (the channel is left).
    w.metadata
        .branches
        .lock()
        .unwrap()
        .get_mut(&w.dev)
        .unwrap()
        .status = BranchStatus::Deleted;
    assert_eq!(w.reason(DEFAULT_SUB, w.dev, None).await, json!("not_found"));
    // Being deleted: not ready, so degraded rather than left.
    w.metadata
        .branches
        .lock()
        .unwrap()
        .get_mut(&w.dev)
        .unwrap()
        .status = BranchStatus::Deleting;
    assert_eq!(
        w.reason(DEFAULT_SUB, w.dev, None).await,
        json!("unavailable")
    );

    // The grant replaced by one without the role: forbidden.
    let dev1 = w.metadata.member(w.tenant, "dev1").unwrap().id;
    w.metadata
        .grant(dev1, w.project, PlatformRole::Developer, &["other"]);
    assert_eq!(
        w.reason("dev1", w.main(), Some("app")).await,
        json!("forbidden")
    );

    // The member removed: forbidden, whatever the target.
    w.metadata
        .members
        .lock()
        .unwrap()
        .iter_mut()
        .filter(|m| m.id == dev1)
        .for_each(|m| m.status = MemberStatus::Removed);
    assert_eq!(
        w.reason("dev1", w.main(), Some("other")).await,
        json!("forbidden")
    );
    assert_eq!(
        w.reason("dev1", BranchId::new(), None).await,
        json!("forbidden"),
        "never not_found for a non-member"
    );

    // The database deleted: not_found.
    w.metadata
        .databases
        .lock()
        .unwrap()
        .get_mut(&w.db)
        .unwrap()
        .status = DatabaseStatus::Deleted;
    assert_eq!(
        w.reason(DEFAULT_SUB, w.main(), None).await,
        json!("not_found")
    );
}

// A branch id of another database is an unknown target.
#[tokio::test]
async fn another_databases_branch_is_not_found() {
    let w = world().await;
    let other = sample_database(w.tenant);
    let other_main = other.main.id;
    w.metadata.insert_database(other);
    assert_eq!(
        w.reason(DEFAULT_SUB, other_main, None).await,
        json!("not_found")
    );
}

// The body's shape: unknown fields anywhere, a missing token or a non-uuid id are 400.
#[tokio::test]
async fn authorize_bodies_of_another_shape_are_400() {
    let w = world().await;
    let token = w.token(DEFAULT_SUB);
    let target =
        json!({ "database_id": w.db, "branch_id": w.main(), "pg_database": "web1", "role": null });
    let mut extra_target = target.clone();
    extra_target["schema"] = json!("public");
    for body in [
        json!({ "token": token, "target": extra_target }),
        json!({ "token": token, "target": target, "sub": "user-1" }),
        json!({ "target": target }),
        json!({ "token": token, "target": { "database_id": "nope", "branch_id": w.main(), "pg_database": "web1" } }),
        json!({ "token": 7, "target": target }),
    ] {
        let (status, _) = w.post(AUTHORIZE, body.clone()).await;
        assert_eq!(status, 400, "{body}");
    }
    // `role` may be left out: the owner.
    let (status, body) = w
        .post(AUTHORIZE, json!({ "token": token, "target": { "database_id": w.db, "branch_id": w.main(), "pg_database": "web1" } }))
        .await;
    assert_eq!(
        (status, body["allowed"].clone()),
        (200, json!(true)),
        "{body}"
    );
}

fn visible_body(w: &World, keys: Option<Value>) -> Value {
    json!({
        "token": w.token(DEFAULT_SUB),
        "database_id": w.db,
        "branch_id": w.main(),
        "pg_database": "web1",
        "rel_oid": 16_384,
        "xid": null,
        "keys": keys,
    })
}

// The key cap's boundary: 1000 keys pass validation (here the compute isn't running: 503),
// rows of mixed or zero width don't.
#[tokio::test]
async fn visible_key_cap_and_row_widths() {
    let w = world().await;
    let thousand: Vec<Value> = (0..1000).map(|i| json!([i.to_string(), "x"])).collect();
    let (status, _) = w
        .post(VISIBLE, visible_body(&w, Some(json!(thousand))))
        .await;
    assert_eq!(status, 503, "1000 keys reach the compute");
    let (status, _) = w.post(VISIBLE, visible_body(&w, Some(json!([])))).await;
    assert_eq!(status, 503, "no keys is a valid (empty) check");
    for keys in [
        json!([["1", "a"], ["2"]]),
        json!([[]]),
        json!([[null]]),
        json!("1"),
    ] {
        let (status, _) = w.post(VISIBLE, visible_body(&w, Some(keys.clone()))).await;
        assert_eq!(status, 400, "{keys}");
    }
    // A negative or oversized OID isn't one.
    for oid in [json!(-1), json!(4_294_967_296_u64), json!("16384")] {
        let mut body = visible_body(&w, None);
        body["rel_oid"] = oid.clone();
        let (status, _) = w.post(VISIBLE, body).await;
        assert_eq!(status, 400, "rel_oid {oid}");
    }
}

// RT1 final fix round (batch B, T6 hardening): a full check, 1000 keys of long texts (a
// composite text key near the btree limit), is past axum's default 2 MB body limit; it still
// reaches the compute (not running here: 503), never 413.
#[tokio::test]
async fn visible_takes_a_full_check_of_long_keys() {
    let w = world().await;
    let long = |i: usize| format!("{i:04}{}", "k".repeat(1_396));
    let keys: Vec<Value> = (0..1000).map(|i| json!([long(i), long(i + 1)])).collect();
    let body = visible_body(&w, Some(json!(keys)));
    assert!(
        serde_json::to_vec(&body).unwrap().len() > 2_800_000,
        "past axum's default"
    );
    let (status, answer) = w.post(VISIBLE, body.clone()).await;
    assert_eq!(status, 503, "the check reaches the compute: {answer}");
    let mut convert = body;
    convert["convert"] = json!(true);
    assert_eq!(w.post(VISIBLE, convert).await.0, 503, "convert mode too");
}

// The owner pool is the console's too: concurrent `/visible` calls on one database hold at
// most `REALTIME_OWNER_CONNECTIONS` of its connections, and a compute that never answers costs
// each call at most its budget (under realtime's 5 s, S14), then 503.
#[tokio::test]
async fn visible_calls_share_the_owner_pool_and_keep_their_budget() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let w = world().await;
    // A "compute" that accepts connections and never answers.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    *w.app.locator.addr.lock().unwrap() = Some(listener.local_addr().unwrap());
    let accepted = Arc::new(AtomicUsize::new(0));
    let counter = accepted.clone();
    let server = tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            counter.fetch_add(1, Ordering::SeqCst);
            held.push(socket);
        }
    });
    let started = std::time::Instant::now();
    let calls: Vec<_> = (0..3)
        .map(|_| w.post(VISIBLE, visible_body(&w, Some(json!([["1", "a"]])))))
        .collect();
    let calls = futures_util::future::join_all(calls);
    tokio::pin!(calls);
    // While the calls wait (well inside their budget), realtime holds only its share. (When
    // the budget ends, a waiting call may take the freed share for a moment: not counted.)
    let mid = tokio::time::sleep(std::time::Duration::from_secs(3));
    tokio::select! {
        _ = &mut calls => panic!("the calls ended before the compute could answer"),
        () = mid => {}
    }
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        orchestrator::REALTIME_OWNER_CONNECTIONS as usize,
        "realtime held more owner connections than its share"
    );
    for (status, body) in calls.await {
        assert_eq!(status, 503, "{body}");
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "realtime gives /visible 5 s: {elapsed:?}"
    );
    assert!(
        elapsed >= orchestrator::internal::realtime::VISIBLE_BUDGET,
        "the calls waited for the compute: {elapsed:?}"
    );
    server.abort();
}

// The owner's view only: a tenant admin passes; a Developer, a stranger and a non-ready branch
// are refused with their statuses.
#[tokio::test]
async fn visible_refusals_carry_their_status() {
    let w = world().await;
    let mut body = visible_body(&w, None);
    body["token"] = json!(w.token("stranger"));
    assert_eq!(w.post(VISIBLE, body).await.0, 403);
    w.metadata
        .branches
        .lock()
        .unwrap()
        .get_mut(&w.main())
        .unwrap()
        .status = BranchStatus::Resetting;
    assert_eq!(
        w.post(VISIBLE, visible_body(&w, None)).await.0,
        503,
        "not ready: retried"
    );
}

// `/projects` refuses anyone who isn't an active member, and an expired token is 401.
#[tokio::test]
async fn projects_refusals() {
    let w = world().await;
    w.metadata
        .add_member(w.tenant, "waiting", MemberStatus::Pending, false);
    for sub in ["waiting", "stranger"] {
        let (status, _) = w.post(PROJECTS, json!({ "token": w.token(sub) })).await;
        assert_eq!(status, 403, "{sub}");
    }
    let expired = w.jwks.sign(json!({
        "sub": DEFAULT_SUB, "iss": w.jwks.issuer(), "exp": now() - 120,
        chelabase_auth::CLAIM_ORG_ID: "org-1",
    }));
    assert_eq!(w.post(PROJECTS, json!({ "token": expired })).await.0, 401);
    let unbound = w.jwks.sign(json!({
        "sub": DEFAULT_SUB, "iss": w.jwks.issuer(), "exp": now() + 300,
        chelabase_auth::CLAIM_ORG_ID: "org-elsewhere",
    }));
    assert_eq!(w.post(PROJECTS, json!({ "token": unbound })).await.0, 403);
    // A project with no live database is listed with none.
    let (status, body) = w
        .post(PROJECTS, json!({ "token": w.token(DEFAULT_SUB) }))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["projects"].as_array().unwrap().len(), 1, "{body}");
}

// A reset whose job can't be queued is released and announces nothing (P10: publish only what
// committed and stays).
#[tokio::test]
async fn a_reset_that_cant_be_queued_publishes_nothing() {
    let w = world().await;
    *w.jobs.fail_branch_publish.lock().unwrap() = true;
    let path = format!(
        "/v1/databases/{}/branches/{}/reset-to-parent",
        w.db.as_uuid(),
        w.dev.as_uuid()
    );
    let resp = w
        .app
        .send_as(DEFAULT_SUB, "org-1", Method::POST, &path, None)
        .await;
    assert!(!resp.status().is_success(), "{}", resp.status());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        w.app.rt_events.all().is_empty(),
        "{:?}",
        w.app.rt_events.all()
    );
}

// RT1 T8 (coordinator ruling): `/visible`'s convert mode for owner deletes. `convert: true`
// takes keys (required) and ignores `xid`; it is otherwise validated like a check, and past
// validation it goes to the compute (not running here: 503). Its behaviour against Postgres
// (the owner session's texts, rows that don't exist, the P6 allowlist's 400) is
// `realtime_live`'s and `chelabase-query`'s `realtime_live`.
#[tokio::test]
async fn visible_convert_mode_takes_keys_and_ignores_the_xid() {
    let w = world().await;
    let mut body = visible_body(&w, Some(json!([["2026-01-02 03:04:05+00", "x"]])));
    body["convert"] = json!(true);
    let (status, answer) = w.post(VISIBLE, body.clone()).await;
    assert_eq!(status, 503, "a valid convert request reaches the compute: {answer}");
    body["xid"] = json!("not an xid");
    assert_eq!(w.post(VISIBLE, body.clone()).await.0, 503, "the xid is ignored");
    for keys in [json!(null), json!([["1", "a"], ["2"]]), json!([[]])] {
        let mut b = body.clone();
        b["keys"] = keys.clone();
        assert_eq!(w.post(VISIBLE, b).await.0, 400, "convert keys {keys}");
    }
    let too_many: Vec<Value> = (0..1001).map(|i| json!([i.to_string(), "x"])).collect();
    body["keys"] = json!(too_many);
    assert_eq!(w.post(VISIBLE, body.clone()).await.0, 400, "1001 keys");
    for flag in [json!("yes"), json!(1)] {
        let mut b = visible_body(&w, Some(json!([["1", "a"]])));
        b["convert"] = flag.clone();
        assert_eq!(w.post(VISIBLE, b).await.0, 400, "convert {flag}");
    }
    let mut off = visible_body(&w, Some(json!([["1", "a"]])));
    off["convert"] = json!(false);
    assert_eq!(w.post(VISIBLE, off).await.0, 503, "convert: false is a check");
}

// The convert mode is the owner's only, like the probe: a Developer is refused.
#[tokio::test]
async fn visible_convert_mode_needs_owner_access() {
    let w = world().await;
    let mut body = visible_body(&w, Some(json!([["1", "a"]])));
    body["convert"] = json!(true);
    assert_eq!(w.post(VISIBLE, body.clone()).await.0, 503, "the admin passes");
    body["token"] = json!(w.token("dev1"));
    assert_eq!(w.post(VISIBLE, body).await.0, 403);
}
