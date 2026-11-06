//! Members hub T8, the coder's edge cases beyond `api.rs`: the one "not found" answer of a
//! login delete, overrides in the member list's filter (MH-22), reset converging like create,
//! a main without an endpoint refused before anything is written, a custom owner's name,
//! the role drop asked for on delete, reinstate's side effects, a suspension retry that redoes
//! only the identity step (S7), revoke-now's counts, and another tenant's member.
#![allow(dead_code, unused_imports)]

include!("common/fakes.rs");

use chelabase_metadata::personal_logins::{LoginEndpoint, RevokeReason};
use chelabase_identity::Pool;
use chelabase_identity::mock::FakeIdentity;

/// org-1's tenant with project Billing (database web1, owner `postgres`, main and `dev`) and
/// the identity fake behind the orchestrator; `alice` and `bob` Developers, `pam` the Admin of
/// Billing, each `(member id, sub)`.
struct E {
    w: MemberWorld,
    mock: FakeIdentity,
    alice: (uuid::Uuid, String),
    bob: (uuid::Uuid, String),
    pam: (uuid::Uuid, String),
}

async fn world() -> E {
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
    let person = |name: &str, role: PlatformRole| {
        let sub = mock.with_user(&Pool::Members, &format!("{name}@x.com"), "Secret-pass1");
        (
            w.metadata
                .add_granted(w.tenant, &sub, w.project, role, &["app"]),
            sub,
        )
    };
    let alice = person("alice", PlatformRole::Developer);
    let bob = person("bob", PlatformRole::Developer);
    let pam = person("pam", PlatformRole::Admin);
    E {
        w,
        mock,
        alice,
        bob,
        pam,
    }
}

impl E {
    async fn send(
        &self,
        sub: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> reqwest::Response {
        self.w.app.send_as(sub, "org-1", method, path, body).await
    }
    fn main(&self) -> BranchId {
        BranchId::from(self.w.db)
    }
    fn login_path(&self) -> String {
        format!("/v1/databases/{}/personal-login", self.w.db)
    }
    fn delete_path(&self, member: uuid::Uuid) -> String {
        format!("/v1/databases/{}/personal-logins/{member}", self.w.db)
    }
    fn seed(&self, who: &(uuid::Uuid, String), name: &str) -> FakeLogin {
        self.w.metadata.seed_personal_login(who.0, self.w.db, name)
    }
}

async fn body_of(resp: reqwest::Response) -> Value {
    resp.json().await.unwrap()
}

async fn error_of(resp: reqwest::Response, status: u16) -> Value {
    assert_eq!(resp.status(), status);
    resp.json().await.unwrap()
}

// A delete the caller may not make, of a login that isn't there, or of an unknown member: one
// answer, 404 `not_found` with the same message, so nothing is told apart (and it isn't a
// route's generic "not found", which the access walk reads as an access refusal).
#[tokio::test]
async fn a_refused_delete_and_a_missing_login_answer_alike() {
    let e = world().await;
    e.seed(&e.bob, "bob_login");
    let cases = [
        (e.alice.1.as_str(), e.delete_path(e.bob.0)),
        (DEFAULT_SUB, e.delete_path(e.alice.0)),
        (DEFAULT_SUB, e.delete_path(uuid::Uuid::new_v4())),
        (e.alice.1.as_str(), e.delete_path(e.alice.0)),
    ];
    for (sub, path) in cases {
        let err = error_of(e.send(sub, Method::DELETE, &path, None).await, 404).await;
        assert_eq!(err["error"]["code"], "not_found", "{path}");
        assert_eq!(
            err["error"]["message"], "personal login not found",
            "{path}"
        );
    }
    assert!(e.w.metadata.personal_login(e.bob.0, e.w.db).is_some());
}

// MH-22 with overrides: a project Admin narrowed below Admin on the database sees none of its
// logins in a member's list, and can't delete them; another database of the project, where
// they stay Admin, still shows.
#[tokio::test]
async fn a_project_admin_narrowed_on_a_database_has_no_power_over_its_logins() {
    let e = world().await;
    e.seed(&e.alice, "alice_login");
    let mut web2 = sample_database(e.w.tenant);
    web2.project_id = e.w.project;
    web2.name = "web2".into();
    web2.database_name = Some("web2".into());
    let web2_id = web2.database_id;
    e.w.metadata.insert_database(web2);
    e.w.metadata
        .seed_personal_login(e.alice.0, web2_id, "alice_web2");
    e.w.metadata.narrow(
        e.pam.0,
        e.w.db,
        OverrideRow {
            platform_role: Some(PlatformRole::Developer),
            pg_roles: None,
        },
    );
    let listed = body_of(
        e.send(
            &e.pam.1,
            Method::GET,
            &format!("/v1/members/{}/personal-logins", e.alice.0),
            None,
        )
        .await,
    )
    .await;
    let names: Vec<&str> = listed["logins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["role_name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["alice_web2"], "{listed}");
    let err = error_of(
        e.send(&e.pam.1, Method::DELETE, &e.delete_path(e.alice.0), None)
            .await,
        404,
    )
    .await;
    assert_eq!(err["error"]["code"], "not_found");
    assert!(e.w.metadata.personal_login(e.alice.0, e.w.db).is_some());
}

// P8 with S9: a reset converges like a create, one synchronous member-filtered reconcile of
// main and the other branches enqueued; `ready_on_main` follows the row on main.
#[tokio::test]
async fn reset_converges_main_synchronously_and_enqueues_the_others() {
    let e = world().await;
    e.seed(&e.alice, "alice_login");
    let resp = e
        .send(
            &e.alice.1,
            Method::POST,
            &format!("{}/reset-password", e.login_path()),
            None,
        )
        .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        body_of(resp).await["ready_on_main"],
        true,
        "the seeded login has its row on main"
    );
    assert_eq!(
        e.w.jobs.member_reconciles.lock().unwrap().clone(),
        vec![MemberRolesReconcileRequest {
            branch_id: e.main().as_uuid(),
            member_id: Some(e.alice.0),
            from_ddl: false
        }]
    );
    assert!(
        e.w.jobs
            .member_role_enqueues
            .lock()
            .unwrap()
            .contains(&e.w.dev)
    );
}

// A password answered without a main to use it on would be lost: a main with no endpoint is
// 409 `credentials_unavailable` before anything is written (create and reset).
#[tokio::test]
async fn a_main_without_an_endpoint_is_refused_before_any_write() {
    let e = world().await;
    e.w.metadata
        .branches
        .lock()
        .unwrap()
        .get_mut(&e.main())
        .unwrap()
        .endpoint_id = None;
    let err = error_of(
        e.send(
            &e.alice.1,
            Method::PUT,
            &e.login_path(),
            Some(json!({ "name": "alice_login" })),
        )
        .await,
        409,
    )
    .await;
    assert_eq!(err["error"]["code"], "credentials_unavailable");
    assert!(
        e.w.metadata.personal_login(e.alice.0, e.w.db).is_none(),
        "nothing written"
    );
    e.seed(&e.bob, "bob_login");
    let before =
        e.w.metadata
            .personal_login(e.bob.0, e.w.db)
            .unwrap()
            .verifier;
    let err = error_of(
        e.send(
            &e.bob.1,
            Method::POST,
            &format!("{}/reset-password", e.login_path()),
            None,
        )
        .await,
        409,
    )
    .await;
    assert_eq!(err["error"]["code"], "credentials_unavailable");
    assert_eq!(
        e.w.metadata
            .personal_login(e.bob.0, e.w.db)
            .unwrap()
            .verifier,
        before,
        "the old password still works"
    );
}

// D3: the database owner's name is refused even when it isn't a reserved word (a custom owner).
#[tokio::test]
async fn a_custom_owner_name_is_not_a_login_name() {
    let e = world().await;
    e.w.metadata
        .databases
        .lock()
        .unwrap()
        .get_mut(&e.w.db)
        .unwrap()
        .owner_role = Some("app_owner".into());
    let err = error_of(
        e.send(
            &e.alice.1,
            Method::PUT,
            &e.login_path(),
            Some(json!({ "name": "app_owner" })),
        )
        .await,
        422,
    )
    .await;
    assert_eq!(err["error"]["code"], "reserved_role_name");
    assert!(e.w.metadata.personal_login(e.alice.0, e.w.db).is_none());
}

// A deleted login's branches are asked to reconcile, which drops its role there.
#[tokio::test]
async fn a_deleted_login_asks_its_branches_to_drop_the_role() {
    let e = world().await;
    e.seed(&e.alice, "alice_login");
    e.w.jobs.member_role_enqueues.lock().unwrap().clear();
    assert_eq!(
        e.send(&e.alice.1, Method::DELETE, &e.delete_path(e.alice.0), None)
            .await
            .status(),
        204
    );
    let enqueued = e.w.jobs.member_role_enqueues.lock().unwrap().clone();
    for b in [e.main(), e.w.dev] {
        assert!(enqueued.contains(&b), "{b}: {enqueued:?}");
    }
    let rows =
        e.w.metadata
            .roles
            .lock()
            .unwrap()
            .values()
            .flatten()
            .filter(|r| r.name == "alice_login")
            .count();
    assert_eq!(rows, 0, "the proxy rows went with the record");
}

// Reinstate gives the member's roles back: every branch of their projects reconciles, and the
// proxies are told about their (still revoked) logins' rows.
#[tokio::test]
async fn reinstate_reconciles_the_members_projects() {
    let e = world().await;
    let login = e.seed(&e.bob, "bob_login");
    assert_eq!(
        e.send(
            DEFAULT_SUB,
            Method::POST,
            &format!("/v1/members/{}/suspend", e.bob.0),
            None
        )
        .await
        .status(),
        200
    );
    e.w.jobs.member_role_enqueues.lock().unwrap().clear();
    e.w.app.cache.clear();
    assert_eq!(
        e.send(
            DEFAULT_SUB,
            Method::POST,
            &format!("/v1/members/{}/reinstate", e.bob.0),
            None
        )
        .await
        .status(),
        200
    );
    let enqueued = e.w.jobs.member_role_enqueues.lock().unwrap().clone();
    for b in [e.main(), e.w.dev] {
        assert!(enqueued.contains(&b), "{b}: {enqueued:?}");
    }
    let rows: Vec<(String, String)> = login
        .endpoints
        .iter()
        .map(|r| (r.endpoint_id.clone(), r.role_name.clone()))
        .collect();
    assert!(
        e.w.app.cache.wait_for(&rows).await,
        "{:?}",
        e.w.app.cache.sent()
    );
    let l = e.w.metadata.personal_login(e.bob.0, e.w.db).unwrap();
    assert_eq!(
        l.login.revoked_reason,
        Some(RevokeReason::Suspended),
        "the login stays revoked"
    );
}

// S7: suspending a suspended member again redoes the identity step (ending the sessions) and
// writes no second event.
// (Final round A, T8-3: the side effects, idempotent, run again too; see `final_a.rs`.)
#[tokio::test]
async fn a_suspension_retry_redoes_only_the_identity_step() {
    let e = world().await;
    let path = format!("/v1/members/{}/suspend", e.bob.0);
    assert_eq!(
        e.send(DEFAULT_SUB, Method::POST, &path, None)
            .await
            .status(),
        200
    );
    let events = e.w.metadata.events.lock().unwrap().len();
    assert_eq!(
        e.send(DEFAULT_SUB, Method::POST, &path, None)
            .await
            .status(),
        200
    );
    assert_eq!(
        e.w.metadata.events.lock().unwrap().len(),
        events,
        "no second event"
    );
    let ends = e
        .mock
        .calls()
        .iter()
        .filter(|c| **c == format!("DELETE /internal/v1/pools/members/users/{}/sessions", e.bob.1))
        .count();
    assert_eq!(ends, 2);
}

// P16 (b): `revoked` counts the logins that were live (not one revoked earlier), and
// `rotated_branches` the branches holding the member's role.
#[tokio::test]
async fn revoke_now_counts_live_logins_and_rotated_branches() {
    let e = world().await;
    e.seed(&e.alice, "alice_login");
    let mut web2 = sample_database(e.w.tenant);
    web2.project_id = e.w.project;
    web2.name = "web2".into();
    let web2_id = web2.database_id;
    e.w.metadata.insert_database(web2);
    e.w.metadata
        .seed_personal_login(e.alice.0, web2_id, "alice_old");
    e.w.metadata.edit_personal_login(e.alice.0, web2_id, |l| {
        l.login.revoked_at = Some(chrono::Utc::now());
        l.login.revoked_reason = Some(RevokeReason::AccessLost);
        l.verifier = None;
    });
    for branch in [e.main(), e.w.dev] {
        e.w.metadata
            .member_roles
            .lock()
            .unwrap()
            .push(MemberBranchRole {
                member_id: e.alice.0,
                branch_id: branch.as_uuid(),
                role_name: chelabase_types::role_name::member_role_name(e.alice.0),
                password_enc: "v1:sealed".into(),
                applied_roles: vec!["app".into()],
            });
    }
    let body = body_of(
        e.send(
            DEFAULT_SUB,
            Method::POST,
            &format!("/v1/members/{}/revoke-postgres-access", e.alice.0),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(
        (body["revoked"].clone(), body["rotated_branches"].clone()),
        (json!(1), json!(2)),
        "{body}"
    );
    // Studio access and grants are untouched.
    assert!(
        e.w.metadata
            .grants
            .lock()
            .unwrap()
            .contains_key(&(e.alice.0, e.w.project.as_uuid()))
    );
}

// MH-22: another tenant's member answers like a missing one, and the list never answers 403.
#[tokio::test]
async fn another_tenants_member_has_no_logins_to_list() {
    let e = world().await;
    let stranger =
        e.w.metadata
            .add_member(TenantId::new(), "s", MemberStatus::Active, false);
    let err = error_of(
        e.send(
            &e.alice.1,
            Method::GET,
            &format!("/v1/members/{stranger}/personal-logins"),
            None,
        )
        .await,
        404,
    )
    .await;
    assert_eq!(err["error"]["code"], "not_found");
    assert_ne!(err["error"]["message"], "not found");
}
