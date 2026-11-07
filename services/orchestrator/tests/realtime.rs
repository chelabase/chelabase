//! RT1 T6: the orchestrator's side of realtime, through the fake-backed public listener.
//!
//! - `/internal/realtime/{authorize,projects,visible}`: realtime's bearer only (R10, S15);
//!   the member's raw OIDC token verified here (a mock JWKS), the member read without ever
//!   being created (R10); the console's gates (R3) and S13's reason mapping; `tenant_id` and
//!   `member` present iff allowed.
//! - The events the orchestrator publishes after its metadata writes (P10, W18, S28): one per
//!   write path, none when the write fails, and a failed publish never fails the write.
//! - Live (`#[ignore]`): authorize for a member and `/visible` on the owner pool against a real
//!   Postgres (the dev metadata Postgres stands in for a compute, as `api.rs` does):
//!   just workbench test -p orchestrator --test realtime -- --run-ignored only

// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]
#![allow(dead_code, unused_imports)]

include!("common/fakes.rs");

use chelabase_auth::test_support::MemberKeys;
use chelabase_nats::rt::{AccessEvent, ProjectEvent};
use chelabase_identity::Pool;
use chelabase_identity::mock::FakeIdentity;

const AUTHORIZE: &str = "/internal/realtime/authorize";
const PROJECTS: &str = "/internal/realtime/projects";
const VISIBLE: &str = "/internal/realtime/visible";

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// A world on org-1's tenant: project Billing with database web1 (owner `postgres`; main
/// serves `web1` with the data API on and `analytics` with it off), a ready `dev` branch, a
/// backup of it and a `creating` branch. The default caller is the tenant admin; `dev1` is a
/// Developer holding `app`, `viewer1` a Viewer with no role.
struct Rt {
    app: TestApp,
    metadata: FakeMetadata,
    jobs: FakeJobs,
    jwks: MemberKeys,
    tenant: TenantId,
    project: ProjectId,
    db: DatabaseId,
    dev: BranchId,
    backup: BranchId,
    creating: BranchId,
}

async fn rt() -> Rt {
    rt_with(MemberKeys::new()).await
}

async fn rt_with(jwks: MemberKeys) -> Rt {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app_member_tokens_with(
        metadata.clone(),
        jobs.clone(),
        "http://127.0.0.1:9",
        Mode::SelfHosted,
        jwks.verifier(),
    )
    .await;
    let project = metadata.create_project(tenant, "Billing").await.unwrap().id;
    let mut db = sample_database(tenant);
    db.project_id = project;
    db.name = "web1".into();
    db.database_name = Some("web1".into());
    db.owner_role = Some("postgres".into());
    db.main.owner_password_enc = Some(seal_credential(
        &app.key,
        &db.main.id.to_string(),
        Purpose::Owner,
        "s3cret",
    ));
    let (db_id, main) = (db.database_id, db.main.id);
    metadata.insert_database(db);
    for (branch, name, origin) in [
        (main, "web1", DatabaseOrigin::Main),
        (main, "analytics", DatabaseOrigin::Sql),
    ] {
        metadata.add_branch_db(branch, name, "postgres", origin);
    }
    let dev = metadata.add_branch(db_id, "dev", main, BranchKind::Normal, BranchStatus::Ready);
    metadata.add_branch_db(dev, "web1", "postgres", DatabaseOrigin::Main);
    let backup = metadata.add_branch(
        db_id,
        "dev-before-restore-20260101T000000Z",
        dev,
        BranchKind::Backup,
        BranchStatus::Ready,
    );
    metadata.add_branch_db(backup, "web1", "postgres", DatabaseOrigin::Main);
    let creating = metadata.add_branch(
        db_id,
        "creating",
        main,
        BranchKind::Normal,
        BranchStatus::Provisioning,
    );
    metadata.add_branch_db(creating, "web1", "postgres", DatabaseOrigin::Main);
    // The data API is on for every `web1`, off for `analytics`.
    for rows in metadata.branch_dbs.lock().unwrap().values_mut() {
        for row in rows.iter_mut().filter(|r| r.name == "web1") {
            row.data_api = chelabase_metadata::DataApiState::On;
        }
    }
    metadata.add_granted(tenant, "dev1", project, PlatformRole::Developer, &["app"]);
    metadata.add_granted(tenant, "viewer1", project, PlatformRole::Viewer, &[]);
    Rt {
        app,
        metadata,
        jobs,
        jwks,
        tenant,
        project,
        db: db_id,
        dev,
        backup,
        creating,
    }
}

impl Rt {
    fn main(&self) -> BranchId {
        BranchId::from(self.db)
    }

    /// A member token, as auth signs it (with email and name).
    fn token(&self, sub: &str) -> String {
        self.token_in(sub, "org-1", 300)
    }

    /// A member token naming the tenant labelled `org` (an unknown label: a tenant that
    /// doesn't exist).
    fn token_in(&self, sub: &str, org: &str, ttl: i64) -> String {
        let tenant = self.metadata.tenants_by_org.lock().unwrap().get(org).copied().unwrap_or_else(TenantId::new);
        signed_member_token(&self.jwks, sub, tenant, ttl)
    }

    fn target(&self, branch: BranchId, pg_database: &str, role: Option<&str>) -> Value {
        json!({ "database_id": self.db, "branch_id": branch, "pg_database": pg_database, "role": role })
    }

    async fn post(&self, path: &str, bearer: Option<&str>, body: Value) -> (u16, Value) {
        self.post_raw(path, bearer, body.to_string()).await
    }

    async fn post_raw(&self, path: &str, bearer: Option<&str>, body: String) -> (u16, Value) {
        let mut req = reqwest::Client::new()
            .post(format!("{}{path}", self.app.base))
            .header("content-type", "application/json")
            .body(body);
        if let Some(bearer) = bearer {
            req = req.bearer_auth(bearer);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    /// `/authorize` as `sub`, with realtime's bearer; the answer must be a 200.
    async fn authorize(&self, sub: &str, target: Value) -> Value {
        self.authorize_token(&self.token(sub), target).await
    }

    async fn authorize_token(&self, token: &str, target: Value) -> Value {
        let (status, body) = self
            .post(
                AUTHORIZE,
                Some(REALTIME_TOKEN),
                json!({ "token": token, "target": target }),
            )
            .await;
        assert_eq!(status, 200, "{body}");
        body
    }

    async fn visible(&self, sub: &str, body: Value) -> (u16, Value) {
        let mut body = body;
        body["token"] = json!(self.token(sub));
        self.post(VISIBLE, Some(REALTIME_TOKEN), body).await
    }

    fn visible_body(
        &self,
        branch: BranchId,
        pg_database: &str,
        rel_oid: u32,
        xid: Option<&str>,
        keys: Option<Value>,
    ) -> Value {
        json!({
            "database_id": self.db,
            "branch_id": branch,
            "pg_database": pg_database,
            "rel_oid": rel_oid,
            "xid": xid,
            "keys": keys,
        })
    }

    fn role_checks(&self) -> Vec<(BranchId, String)> {
        self.jobs.role_checks.lock().unwrap().clone()
    }

    fn answer_role_check(&self, answer: Result<CheckDataRoleReply, ResetError>) {
        *self.jobs.role_check.lock().unwrap() = Some(answer);
    }
}

/// A refusal: `allowed: false` with exactly this reason, and neither `tenant_id` nor `member`.
#[track_caller]
fn assert_refused(body: &Value, reason: &str) {
    assert_eq!(body["allowed"], json!(false), "{body}");
    assert_eq!(body["reason"], json!(reason), "{body}");
    assert!(
        body.get("tenant_id").is_none(),
        "tenant_id only when allowed: {body}"
    );
    assert!(
        body.get("member").is_none(),
        "member only when allowed: {body}"
    );
}

/// Allowed: no reason, the tenant and the member as the token names them.
#[track_caller]
fn assert_allowed(body: &Value, tenant: TenantId, sub: &str) {
    assert_eq!(body["allowed"], json!(true), "{body}");
    assert!(body.get("reason").is_none_or(Value::is_null), "{body}");
    assert_eq!(body["tenant_id"], json!(tenant.as_uuid()), "{body}");
    assert_eq!(
        body["member"],
        json!({ "sub": sub, "name": format!("Name of {sub}"), "email": format!("{sub}@mail.test") }),
        "{body}"
    );
}

fn refused_reply(kind: &str, message: &str) -> CheckDataRoleReply {
    CheckDataRoleReply {
        ok: false,
        error: Some(message.into()),
        kind: Some(kind.into()),
        ..CheckDataRoleReply::ok()
    }
}

// --- Bearers (R10, S15) --------------------------------------------------------------------

// Each realtime route takes realtime's bearer only; rest's token, a wrong one or none is 401,
// before the body is looked at.
#[tokio::test]
async fn realtime_routes_need_realtime_s_bearer() {
    let t = rt().await;
    let token = t.token(DEFAULT_SUB);
    let bodies = [
        (
            AUTHORIZE,
            json!({ "token": token, "target": t.target(t.main(), "web1", None) }),
        ),
        (PROJECTS, json!({ "token": token })),
        (
            VISIBLE,
            json!({ "token": token, "database_id": t.db, "branch_id": t.main(), "pg_database": "web1", "rel_oid": 1, "xid": null, "keys": null }),
        ),
    ];
    for (path, body) in &bodies {
        for bearer in [None, Some(REST_TOKEN), Some("wrong"), Some("")] {
            let (status, _) = t.post(path, bearer, body.clone()).await;
            assert_eq!(status, 401, "POST {path} with {bearer:?}");
        }
        // With it, a body that isn't JSON (or isn't the route's shape) is 400.
        let (status, _) = t
            .post_raw(path, Some(REALTIME_TOKEN), "not json".into())
            .await;
        assert_eq!(status, 400, "POST {path} with a bad body");
        let (status, _) = t
            .post(path, Some(REALTIME_TOKEN), json!({ "nope": 1 }))
            .await;
        assert_eq!(status, 400, "POST {path} with the wrong shape");
    }
}

// --- /authorize: allowed --------------------------------------------------------------------

#[tokio::test]
async fn the_tenant_admin_may_watch_as_the_owner() {
    let t = rt().await;
    let body = t
        .authorize(DEFAULT_SUB, t.target(t.main(), "web1", None))
        .await;
    assert_allowed(&body, t.tenant, DEFAULT_SUB);
    // The owner's path never asks the worker's role check.
    assert!(t.role_checks().is_empty());
}

#[tokio::test]
async fn a_granted_role_is_allowed_after_the_workers_check() {
    let t = rt().await;
    let body = t
        .authorize("dev1", t.target(t.dev, "web1", Some("app")))
        .await;
    assert_allowed(&body, t.tenant, "dev1");
    assert_eq!(
        t.role_checks(),
        vec![(t.dev, "app".to_string())],
        "R3: the console's check_role"
    );
}

// --- /authorize: forbidden ------------------------------------------------------------------

// The access refusals: the owner needs Admin (R7), a role needs a grant (or the R21 chain).
#[tokio::test]
async fn access_refusals_are_forbidden() {
    let t = rt().await;
    for (sub, role) in [
        ("dev1", None),
        ("viewer1", None),
        ("dev1", Some("other")),
        ("viewer1", Some("app")),
    ] {
        let body = t.authorize(sub, t.target(t.main(), "web1", role)).await;
        assert_refused(&body, "forbidden");
    }
    assert!(
        t.role_checks().is_empty(),
        "refused before the worker is asked"
    );
}

// R3: the console's gates. A backup branch, the data API off, the owner's own name as a role
// and the worker refusing the role (owner-level, missing) leave the channel.
#[tokio::test]
async fn console_gate_refusals_are_forbidden() {
    let t = rt().await;
    let backup = t
        .authorize(DEFAULT_SUB, t.target(t.backup, "web1", None))
        .await;
    assert_refused(&backup, "forbidden");
    let off = t
        .authorize(DEFAULT_SUB, t.target(t.main(), "analytics", None))
        .await;
    assert_refused(&off, "forbidden");
    let owner_by_name = t
        .authorize(DEFAULT_SUB, t.target(t.main(), "web1", Some("postgres")))
        .await;
    assert_refused(&owner_by_name, "forbidden");
    t.answer_role_check(Ok(refused_reply("refused", "role \"app\" is owner-level")));
    let refused = t
        .authorize("dev1", t.target(t.main(), "web1", Some("app")))
        .await;
    assert_refused(&refused, "forbidden");
}

// A bad signature (or anything else that isn't expiry) is a refusal, not a retry.
#[tokio::test]
async fn a_token_that_does_not_verify_is_forbidden() {
    let t = rt().await;
    let token = t.token(DEFAULT_SUB);
    let (head, sig) = token.rsplit_once('.').unwrap();
    let flipped = if sig.starts_with('A') { "B" } else { "A" };
    let tampered = format!("{head}.{flipped}{}", &sig[1..]);
    for bad in [tampered.as_str(), "not-a-jwt"] {
        let body = t
            .authorize_token(bad, t.target(t.main(), "web1", None))
            .await;
        assert_refused(&body, "forbidden");
    }
    // Another key's token, even with a valid shape.
    let other = MemberKeys::new();
    let foreign = signed_member_token(&other, DEFAULT_SUB, t.tenant, 300);
    let body = t
        .authorize_token(&foreign, t.target(t.main(), "web1", None))
        .await;
    assert_refused(&body, "forbidden");
}

// R10: the member is read, never made: an unknown person, a removed, pending or invited member
// and a tenant that doesn't exist are refused, and nothing is written (no admission, no first-member
// admin claim, no activation of an invite).
#[tokio::test]
async fn members_are_resolved_read_only() {
    let t = rt().await;
    t.metadata
        .add_member(t.tenant, "gone", MemberStatus::Removed, false);
    t.metadata
        .add_member(t.tenant, "waiting", MemberStatus::Pending, false);
    let invited = t
        .metadata
        .add_member(t.tenant, "inv", MemberStatus::Invited, false);
    t.metadata.add_open_invite(invited);
    // Open sign-up would admit a newcomer as pending on a normal request.
    t.metadata
        .open_signups
        .lock()
        .unwrap()
        .insert(t.tenant, true);
    let members_before = t.metadata.members.lock().unwrap().clone();
    let orgs_before = t.metadata.tenants_by_org.lock().unwrap().clone();
    for sub in ["stranger", "gone", "waiting", "inv"] {
        let body = t.authorize(sub, t.target(t.main(), "web1", None)).await;
        assert_refused(&body, "forbidden");
    }
    let unbound = t
        .authorize_token(
            &t.token_in(DEFAULT_SUB, "org-unbound", 300),
            t.target(t.main(), "web1", None),
        )
        .await;
    assert_refused(&unbound, "forbidden");
    assert_eq!(
        *t.metadata.members.lock().unwrap(),
        members_before,
        "no member written"
    );
    assert_eq!(
        *t.metadata.tenants_by_org.lock().unwrap(),
        orgs_before,
        "no tenant named"
    );
    assert_eq!(
        t.metadata.member(t.tenant, "inv").unwrap().status,
        MemberStatus::Invited
    );
    assert!(t.role_checks().is_empty());
}

// R10: a fresh tenant with no member at all: the first person to call isn't made its admin.
#[tokio::test]
async fn no_first_member_claim() {
    let jwks = MemberKeys::new();
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    // Bound without `bind_org`, which would add the default admin.
    metadata
        .tenants_by_org
        .lock()
        .unwrap()
        .insert("org-1".into(), tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app_member_tokens_with(
        metadata.clone(),
        jobs.clone(),
        "http://127.0.0.1:9",
        Mode::SelfHosted,
        jwks.verifier(),
    )
    .await;
    let t = Rt {
        app,
        metadata: metadata.clone(),
        jobs,
        jwks,
        tenant,
        project: ProjectId::new(),
        db: DatabaseId::new(),
        dev: BranchId::new(),
        backup: BranchId::new(),
        creating: BranchId::new(),
    };
    let body = t.authorize("first", t.target(t.main(), "web1", None)).await;
    assert_refused(&body, "forbidden");
    let (status, _) = t
        .post(
            PROJECTS,
            Some(REALTIME_TOKEN),
            json!({ "token": t.token("first") }),
        )
        .await;
    assert_eq!(status, 403);
    assert!(
        metadata.members.lock().unwrap().is_empty(),
        "no member made"
    );
    assert!(
        metadata.admin_claims.lock().unwrap().is_empty(),
        "no admin claimed"
    );
}

// --- /authorize: not_found, unavailable, token_expired -------------------------------------

#[tokio::test]
async fn unknown_targets_are_not_found() {
    let t = rt().await;
    let other_tenant = sample_database(TenantId::new());
    let foreign = other_tenant.database_id;
    t.metadata.insert_database(other_tenant);
    let targets = [
        json!({ "database_id": DatabaseId::new(), "branch_id": t.main(), "pg_database": "web1", "role": null }),
        json!({ "database_id": foreign, "branch_id": BranchId::from(foreign), "pg_database": "web1", "role": null }),
        t.target(BranchId::new(), "web1", None),
        t.target(t.main(), "nope", None),
        // The system database is never served.
        t.target(t.main(), "postgres", None),
    ];
    for target in targets {
        let body = t.authorize(DEFAULT_SUB, target.clone()).await;
        assert_refused(&body, "not_found");
    }
}

// S13: a branch not ready (a reset or switch in progress), the worker saying not ready, timing
// out or unreachable, and a metadata error are all `unavailable`: the channel stays, degraded.
#[tokio::test]
async fn transient_failures_are_unavailable() {
    let t = rt().await;
    let creating = t
        .authorize(DEFAULT_SUB, t.target(t.creating, "web1", None))
        .await;
    assert_refused(&creating, "unavailable");
    *t.metadata.fail_get_branch.lock().unwrap() = true;
    let metadata = t
        .authorize(DEFAULT_SUB, t.target(t.main(), "web1", None))
        .await;
    assert_refused(&metadata, "unavailable");
    // Each worker answer on its own branch and role (a passed check is remembered).
    let cases: [(&str, Result<CheckDataRoleReply, ResetError>); 3] = [
        (
            "r_not_ready",
            Ok(refused_reply("not_ready", "branch is not ready")),
        ),
        ("r_timeout", Err(ResetError::Timeout)),
        (
            "r_transport",
            Err(ResetError::Transport("no worker listening".into())),
        ),
    ];
    for (role, answer) in cases {
        t.metadata.grant(
            t.metadata.member(t.tenant, "dev1").unwrap().id,
            t.project,
            PlatformRole::Developer,
            &["app", role],
        );
        t.answer_role_check(answer);
        let body = t
            .authorize("dev1", t.target(t.main(), "web1", Some(role)))
            .await;
        assert_refused(&body, "unavailable");
    }
}

// W19/S14: an expired token waits for the renewal; it never leaves the channel.
#[tokio::test]
async fn an_expired_token_is_token_expired() {
    let t = rt().await;
    // Past the verifier's 60 s leeway.
    let expired = t.token_in(DEFAULT_SUB, "org-1", -120);
    let body = t
        .authorize_token(&expired, t.target(t.main(), "web1", None))
        .await;
    assert_refused(&body, "token_expired");
}

// S13: an empty role is the caller's bug (400), not a refusal.
#[tokio::test]
async fn an_empty_role_is_400() {
    let t = rt().await;
    let (status, _) = t
        .post(AUTHORIZE, Some(REALTIME_TOKEN), json!({ "token": t.token(DEFAULT_SUB), "target": t.target(t.main(), "web1", Some("")) }))
        .await;
    assert_eq!(status, 400);
}

// --- /projects ------------------------------------------------------------------------------

/// The `{project_id, database_ids}` entries, sorted.
fn projects_of(body: &Value) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = body["projects"]
        .as_array()
        .unwrap_or_else(|| panic!("no projects: {body}"))
        .iter()
        .map(|p| {
            let mut dbs: Vec<String> = p["database_ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|d| d.as_str().unwrap().to_string())
                .collect();
            dbs.sort();
            (p["project_id"].as_str().unwrap().to_string(), dbs)
        })
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn projects_lists_what_the_member_may_see() {
    let t = rt().await;
    let other = t
        .metadata
        .create_project(t.tenant, "Other")
        .await
        .unwrap()
        .id;
    let mut odb = sample_database(t.tenant);
    odb.project_id = other;
    let odb_id = odb.database_id;
    t.metadata.insert_database(odb);
    let mut gone = sample_database(t.tenant);
    gone.project_id = other;
    gone.status = DatabaseStatus::Deleted;
    t.metadata.insert_database(gone);

    // The tenant admin: every project, with its live databases.
    let (status, body) = t
        .post(
            PROJECTS,
            Some(REALTIME_TOKEN),
            json!({ "token": t.token(DEFAULT_SUB) }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["tenant_id"], json!(t.tenant.as_uuid()));
    let mut want = vec![
        (
            t.project.as_uuid().to_string(),
            vec![t.db.as_uuid().to_string()],
        ),
        (
            other.as_uuid().to_string(),
            vec![odb_id.as_uuid().to_string()],
        ),
    ];
    want.sort();
    assert_eq!(projects_of(&body), want);

    // A Developer of Billing only.
    let (status, body) = t
        .post(
            PROJECTS,
            Some(REALTIME_TOKEN),
            json!({ "token": t.token("dev1") }),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        projects_of(&body),
        vec![(
            t.project.as_uuid().to_string(),
            vec![t.db.as_uuid().to_string()]
        )]
    );

    // Someone who isn't a member: refused, and still not one.
    let (status, _) = t
        .post(
            PROJECTS,
            Some(REALTIME_TOKEN),
            json!({ "token": t.token("stranger") }),
        )
        .await;
    assert_eq!(status, 403);
    assert!(t.metadata.member(t.tenant, "stranger").is_none());
}

// --- /visible (the parts that need no Postgres; the rest is live) ---------------------------

// R10: only the owner's probe runs here, re-authorized as the owner (Admin).
#[tokio::test]
async fn visible_needs_owner_access() {
    let t = rt().await;
    let body = t.visible_body(t.main(), "web1", 16_384, None, None);
    let (status, _) = t.visible("dev1", body.clone()).await;
    assert_eq!(status, 403, "a Developer can't view as the owner");
    let mut unknown = body.clone();
    unknown["database_id"] = json!(DatabaseId::new());
    let (status, _) = t.visible(DEFAULT_SUB, unknown).await;
    assert_eq!(status, 404);
    let (status, _) = t
        .visible(
            DEFAULT_SUB,
            t.visible_body(t.main(), "nope", 16_384, None, None),
        )
        .await;
    assert_eq!(status, 404, "an unserved Postgres database");
}

// The request names a relation only by OID: names, SQL or extra fields are refused; the xid is
// an xid8 in decimal; at most 1000 keys.
#[tokio::test]
async fn visible_validates_the_request() {
    let t = rt().await;
    for (field, value) in [
        ("schema", json!("public")),
        ("table", json!("notes")),
        ("sql", json!("select 1")),
    ] {
        let mut body = t.visible_body(t.main(), "web1", 16_384, None, None);
        body[field] = value;
        let (status, _) = t.visible(DEFAULT_SUB, body).await;
        assert_eq!(status, 400, "an extra {field:?}");
    }
    for xid in [
        "abc",
        "-1",
        "1.5",
        "",
        " 12",
        "0x10",
        "18446744073709551616",
    ] {
        let (status, _) = t
            .visible(
                DEFAULT_SUB,
                t.visible_body(t.main(), "web1", 16_384, Some(xid), None),
            )
            .await;
        assert_eq!(status, 400, "xid {xid:?}");
    }
    let too_many: Vec<Value> = (0..1001).map(|i| json!([i.to_string()])).collect();
    let (status, _) = t
        .visible(
            DEFAULT_SUB,
            t.visible_body(t.main(), "web1", 16_384, None, Some(json!(too_many))),
        )
        .await;
    assert_eq!(status, 400, "1001 keys");
    let (status, _) = t
        .visible(
            DEFAULT_SUB,
            t.visible_body(t.main(), "web1", 16_384, None, Some(json!([[1]]))),
        )
        .await;
    assert_eq!(status, 400, "keys are strings");
}

#[tokio::test]
async fn visible_refuses_an_expired_token() {
    let t = rt().await;
    let mut body = t.visible_body(t.main(), "web1", 16_384, None, None);
    body["token"] = json!(t.token_in(DEFAULT_SUB, "org-1", -120));
    let (status, _) = t.post(VISIBLE, Some(REALTIME_TOKEN), body).await;
    assert_eq!(status, 401);
}

// --- Events: platform (P10) -----------------------------------------------------------------

/// Waits up to 2 s for `done`.
async fn eventually(done: impl Fn() -> bool) -> bool {
    for _ in 0..200 {
        if done() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    done()
}

/// Nothing more arrives (spawned publishes have had their chance).
async fn settle() {
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
}

impl Rt {
    async fn send(
        &self,
        sub: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> reqwest::Response {
        self.app.send_as(sub, "org-1", method, path, body).await
    }

    fn events(&self) -> &FakeRtEvents {
        &self.app.rt_events
    }

    /// Waits for exactly `n` project events, then for none more.
    async fn project_events(&self, n: usize) -> Vec<(uuid::Uuid, ProjectEvent)> {
        assert!(
            eventually(|| self.events().project().len() >= n).await,
            "{:?}",
            self.events().all()
        );
        settle().await;
        let got = self.events().project();
        assert_eq!(got.len(), n, "{got:?}");
        got
    }

    async fn access_events(&self, n: usize) -> Vec<AccessEvent> {
        assert!(
            eventually(|| self.events().access().len() >= n).await,
            "{:?}",
            self.events().all()
        );
        settle().await;
        let got = self.events().access();
        assert_eq!(got.len(), n, "{got:?}");
        got
    }
}

#[tokio::test]
async fn creating_a_database_publishes_database_created() {
    let t = rt().await;
    let resp = t
        .send(
            DEFAULT_SUB,
            Method::POST,
            "/v1/databases",
            Some(json!({ "project_id": t.project, "name": "shop" })),
        )
        .await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    let database_id: uuid::Uuid = body["database_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(
        t.project_events(1).await,
        vec![(
            t.project.as_uuid(),
            ProjectEvent::DatabaseCreated { database_id }
        )]
    );
    // A write that fails publishes nothing.
    t.events().clear();
    t.metadata.fail_next_insert();
    let resp = t
        .send(
            DEFAULT_SUB,
            Method::POST,
            "/v1/databases",
            Some(json!({ "project_id": t.project, "name": "shop2" })),
        )
        .await;
    assert!(!resp.status().is_success());
    settle().await;
    assert!(t.events().all().is_empty(), "{:?}", t.events().all());
}

// A publish that fails never fails the write it reports.
#[tokio::test]
async fn a_failed_publish_does_not_fail_the_write() {
    let t = rt().await;
    *t.events().fail.lock().unwrap() = true;
    let resp = t
        .send(
            DEFAULT_SUB,
            Method::POST,
            "/v1/databases",
            Some(json!({ "project_id": t.project, "name": "shop" })),
        )
        .await;
    assert_eq!(resp.status(), 202);
    assert!(
        eventually(|| !t.events().project().is_empty()).await,
        "the publish was tried"
    );
}

#[tokio::test]
async fn deleting_a_database_publishes_database_deleted() {
    let t = rt().await;
    // The dev branches would hold the database back only when protected; none is.
    let resp = t
        .send(
            DEFAULT_SUB,
            Method::DELETE,
            &format!("/v1/databases/{}", t.db.as_uuid()),
            None,
        )
        .await;
    assert_eq!(resp.status(), 202);
    let got = t.project_events(1).await;
    assert_eq!(
        got,
        vec![(
            t.project.as_uuid(),
            ProjectEvent::DatabaseDeleted {
                database_id: t.db.as_uuid()
            }
        )]
    );
    t.events().clear();
    // Deleting it again is refused (409): nothing more.
    let resp = t
        .send(
            DEFAULT_SUB,
            Method::DELETE,
            &format!("/v1/databases/{}", t.db.as_uuid()),
            None,
        )
        .await;
    assert!(!resp.status().is_success());
    settle().await;
    assert!(t.events().all().is_empty(), "{:?}", t.events().all());
}

#[tokio::test]
async fn creating_and_deleting_a_branch_publish_branch_events() {
    let t = rt().await;
    let branches = format!("/v1/databases/{}/branches", t.db.as_uuid());
    let resp = t
        .send(
            DEFAULT_SUB,
            Method::POST,
            &branches,
            Some(json!({ "name": "feature" })),
        )
        .await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    let branch_id: uuid::Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let database_id = t.db.as_uuid();
    assert_eq!(
        t.project_events(1).await,
        vec![(
            t.project.as_uuid(),
            ProjectEvent::BranchCreated {
                database_id,
                branch_id
            }
        )]
    );
    t.events().clear();
    // The name is taken now: refused, nothing published.
    let resp = t
        .send(
            DEFAULT_SUB,
            Method::POST,
            &branches,
            Some(json!({ "name": "feature" })),
        )
        .await;
    assert_eq!(resp.status(), 409);
    settle().await;
    assert!(t.events().all().is_empty(), "{:?}", t.events().all());

    let feature = BranchId(branch_id);
    t.metadata
        .branches
        .lock()
        .unwrap()
        .get_mut(&feature)
        .unwrap()
        .status = BranchStatus::Ready;
    let resp = t
        .send(
            DEFAULT_SUB,
            Method::DELETE,
            &format!("{branches}/{branch_id}"),
            None,
        )
        .await;
    assert_eq!(resp.status(), 202);
    assert_eq!(
        t.project_events(1).await,
        vec![(
            t.project.as_uuid(),
            ProjectEvent::BranchDeleted {
                database_id,
                branch_id
            }
        )]
    );
}

// RT1 final fix round (batch B, T6 hardening): a create whose job can't be queued flips the
// new row to `failed`; that write is announced too (main's status for a database, whose
// main branch id is the database's), after the `created` event.
#[tokio::test]
async fn a_create_whose_job_cant_be_queued_publishes_its_failure() {
    let t = rt().await;
    *t.jobs.fail_provision_publish.lock().unwrap() = true;
    let resp = t
        .send(
            DEFAULT_SUB,
            Method::POST,
            "/v1/databases",
            Some(json!({ "project_id": t.project, "name": "shop" })),
        )
        .await;
    assert!(!resp.status().is_success(), "{}", resp.status());
    let got = t.project_events(2).await;
    let ProjectEvent::DatabaseCreated { database_id } = got[0].1.clone() else {
        panic!("created first: {got:?}");
    };
    assert_eq!(
        got[1],
        (
            t.project.as_uuid(),
            ProjectEvent::BranchStatus {
                database_id,
                branch_id: database_id,
                status: "failed".into(),
            }
        )
    );

    t.events().clear();
    *t.jobs.fail_branch_publish.lock().unwrap() = true;
    let resp = t
        .send(
            DEFAULT_SUB,
            Method::POST,
            &format!("/v1/databases/{}/branches", t.db.as_uuid()),
            Some(json!({ "name": "feature" })),
        )
        .await;
    assert!(!resp.status().is_success(), "{}", resp.status());
    let got = t.project_events(2).await;
    let ProjectEvent::BranchCreated { database_id, branch_id } = got[0].1.clone() else {
        panic!("created first: {got:?}");
    };
    assert_eq!(database_id, t.db.as_uuid());
    assert_eq!(
        got[1],
        (
            t.project.as_uuid(),
            ProjectEvent::BranchStatus {
                database_id,
                branch_id,
                status: "failed".into(),
            }
        )
    );
}

// W18: a reset is a branch status change the orchestrator writes (`resetting`).
#[tokio::test]
async fn a_reset_publishes_the_branch_status() {
    let t = rt().await;
    // `dev` has a backup child; a fresh ready branch has none.
    let fresh = t.metadata.add_branch(
        t.db,
        "fresh",
        t.main(),
        BranchKind::Normal,
        BranchStatus::Ready,
    );
    let path = format!(
        "/v1/databases/{}/branches/{}/reset-to-parent",
        t.db.as_uuid(),
        fresh.as_uuid()
    );
    let resp = t.send(DEFAULT_SUB, Method::POST, &path, None).await;
    assert_eq!(resp.status(), 202);
    assert_eq!(
        t.project_events(1).await,
        vec![(
            t.project.as_uuid(),
            ProjectEvent::BranchStatus {
                database_id: t.db.as_uuid(),
                branch_id: fresh.as_uuid(),
                status: "resetting".into()
            }
        )]
    );
}

// --- Events: access (P10, W18, S28) ---------------------------------------------------------

/// [`rt`] with the identity fake behind the orchestrator (the member API's removals and invites).
async fn rt_z() -> (Rt, FakeIdentity) {
    let jwks = MemberKeys::new();
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
    let t = Rt {
        app,
        metadata,
        jobs,
        jwks,
        tenant,
        project,
        db: db_id,
        dev,
        backup: BranchId::new(),
        creating: BranchId::new(),
    };
    (t, mock)
}

fn about(tenant: TenantId, sub: Option<&str>) -> AccessEvent {
    AccessEvent {
        tenant_id: tenant.as_uuid(),
        member_sub: sub.map(str::to_string),
    }
}

// Every grant and override write names the member whose access changed; a write that finds
// nothing (404) publishes nothing.
#[tokio::test]
async fn grant_and_override_writes_publish_an_access_event_for_the_member() {
    let (t, _z) = rt_z().await;
    let m = t
        .metadata
        .add_member(t.tenant, "m-sub", MemberStatus::Active, false);
    let grant = format!("/v1/projects/{}/grants/{m}", t.project);
    let overrides = format!("{grant}/overrides");
    let writes = [
        (
            Method::PUT,
            grant.clone(),
            Some(json!({ "platform_role": "developer", "pg_roles": ["a"] })),
        ),
        (
            Method::PUT,
            overrides.clone(),
            Some(json!({ "database_id": t.db, "platform_role": "viewer" })),
        ),
        (
            Method::DELETE,
            format!("{overrides}?database_id={}", t.db.as_uuid()),
            None,
        ),
        (Method::DELETE, grant.clone(), None),
    ];
    for (method, path, body) in writes {
        t.events().clear();
        let resp = t.send(DEFAULT_SUB, method.clone(), &path, body).await;
        assert!(
            resp.status().is_success(),
            "{method} {path}: {}",
            resp.status()
        );
        assert_eq!(
            t.access_events(1).await,
            vec![about(t.tenant, Some("m-sub"))],
            "{method} {path}"
        );
        assert!(t.events().project().is_empty(), "{method} {path}");
    }
    t.events().clear();
    assert_eq!(
        t.send(DEFAULT_SUB, Method::DELETE, &grant, None)
            .await
            .status(),
        404
    );
    let refused = t
        .send(
            DEFAULT_SUB,
            Method::PUT,
            &overrides,
            Some(json!({ "database_id": t.db, "platform_role": "admin" })),
        )
        .await;
    assert!(!refused.status().is_success());
    settle().await;
    assert!(t.events().all().is_empty(), "{:?}", t.events().all());
}

#[tokio::test]
async fn the_tenant_admin_flag_publishes_an_access_event() {
    let (t, _z) = rt_z().await;
    let other = t
        .metadata
        .add_member(t.tenant, "other-sub", MemberStatus::Active, false);
    let resp = t
        .send(
            DEFAULT_SUB,
            Method::PATCH,
            &format!("/v1/members/{other}"),
            Some(json!({ "tenant_admin": true })),
        )
        .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        t.access_events(1).await,
        vec![about(t.tenant, Some("other-sub"))]
    );
    // The last admin can't step down (409): nothing.
    t.events().clear();
    let me = t.metadata.member(t.tenant, DEFAULT_SUB).unwrap().id;
    t.send(
        DEFAULT_SUB,
        Method::PATCH,
        &format!("/v1/members/{other}"),
        Some(json!({ "tenant_admin": false })),
    )
    .await;
    t.events().clear();
    let resp = t
        .send(
            DEFAULT_SUB,
            Method::PATCH,
            &format!("/v1/members/{me}"),
            Some(json!({ "tenant_admin": false })),
        )
        .await;
    assert_eq!(resp.status(), 409);
    settle().await;
    assert!(t.events().access().is_empty(), "{:?}", t.events().all());
}

#[tokio::test]
async fn removing_approving_and_declining_members_publish_access_events() {
    let (t, z) = rt_z().await;
    let gil_sub = z.with_user(&Pool::Members, "gil@x.com", "Secret-pass1");
    let gil = t.metadata.add_granted(
        t.tenant,
        &gil_sub,
        t.project,
        PlatformRole::Developer,
        &["app"],
    );
    assert_eq!(
        t.send(
            DEFAULT_SUB,
            Method::DELETE,
            &format!("/v1/members/{gil}"),
            None
        )
        .await
        .status(),
        204
    );
    assert_eq!(
        t.access_events(1).await,
        vec![about(t.tenant, Some(gil_sub.as_str()))]
    );

    t.events().clear();
    let pat = t
        .metadata
        .add_member(t.tenant, "pat-sub", MemberStatus::Pending, false);
    let body = json!({ "grants": [{ "project_id": t.project, "platform_role": "viewer", "pg_roles": [] }] });
    assert_eq!(
        t.send(
            DEFAULT_SUB,
            Method::POST,
            &format!("/v1/members/{pat}/approve"),
            Some(body)
        )
        .await
        .status(),
        200
    );
    assert_eq!(
        t.access_events(1).await,
        vec![about(t.tenant, Some("pat-sub"))]
    );

    t.events().clear();
    let zid = z.with_invited_user(&Pool::Members, "kim@example.test");
    let kim = t
        .metadata
        .add_member(t.tenant, &zid, MemberStatus::Pending, false);
    assert_eq!(
        t.send(
            DEFAULT_SUB,
            Method::POST,
            &format!("/v1/members/{kim}/decline"),
            None
        )
        .await
        .status(),
        204
    );
    assert_eq!(
        t.access_events(1).await,
        vec![about(t.tenant, Some(zid.as_str()))]
    );

    // Another tenant's member: 404, nothing.
    t.events().clear();
    let stranger = t
        .metadata
        .add_member(TenantId::new(), "s", MemberStatus::Active, false);
    assert_eq!(
        t.send(
            DEFAULT_SUB,
            Method::DELETE,
            &format!("/v1/members/{stranger}"),
            None
        )
        .await
        .status(),
        404
    );
    settle().await;
    assert!(t.events().all().is_empty(), "{:?}", t.events().all());
}

// S28: an invite's writer publishes `member_sub: null` (the whole tenant re-checks).
#[tokio::test]
async fn invites_publish_whole_tenant_access_events() {
    let (t, _z) = rt_z().await;
    let body = json!({ "email": "ivy@x.com", "grants": [{ "project_id": t.project, "platform_role": "viewer", "pg_roles": [] }] });
    let resp = t
        .send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(body))
        .await;
    assert_eq!(resp.status(), 201);
    let created: Value = resp.json().await.unwrap();
    assert_eq!(t.access_events(1).await, vec![about(t.tenant, None)]);
    t.events().clear();
    let invite = created["member"]["invite"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        t.send(
            DEFAULT_SUB,
            Method::DELETE,
            &format!("/v1/invites/{invite}"),
            None
        )
        .await
        .status(),
        204
    );
    assert_eq!(t.access_events(1).await, vec![about(t.tenant, None)]);
    // An unknown invite: 404, nothing.
    t.events().clear();
    assert_eq!(
        t.send(
            DEFAULT_SUB,
            Method::DELETE,
            &format!("/v1/invites/{}", uuid::Uuid::new_v4()),
            None
        )
        .await
        .status(),
        404
    );
    settle().await;
    assert!(t.events().all().is_empty(), "{:?}", t.events().all());
}

// Deleting a project changes every member's access to it: a whole-tenant access event.
#[tokio::test]
async fn deleting_a_project_publishes_a_whole_tenant_access_event() {
    let (t, _z) = rt_z().await;
    let shop = t.metadata.create_project(t.tenant, "Shop").await.unwrap();
    let resp = t
        .send(
            DEFAULT_SUB,
            Method::DELETE,
            &format!("/v1/projects/{}", shop.id),
            Some(json!({ "confirm": shop.slug })),
        )
        .await;
    assert_eq!(resp.status(), 202);
    assert_eq!(t.access_events(1).await, vec![about(t.tenant, None)]);
    // A wrong confirmation is refused: nothing.
    t.events().clear();
    let resp = t
        .send(
            DEFAULT_SUB,
            Method::DELETE,
            &format!("/v1/projects/{}", t.project),
            Some(json!({ "confirm": "nope" })),
        )
        .await;
    assert_eq!(resp.status(), 422);
    settle().await;
    assert!(t.events().access().is_empty(), "{:?}", t.events().all());
}

// --- Live: a member's authorize, and /visible on the owner pool -----------------------------

/// The dev metadata Postgres standing in for main's compute: a scratch database owned by a
/// scratch owner (with the `chelabase.schema_version` the console's preamble reads).
struct ScratchCompute {
    admin: sqlx::PgPool,
    database: String,
    owner: String,
    addr: std::net::SocketAddr,
}

impl ScratchCompute {
    async fn new(password: &str) -> Self {
        use sqlx::Executor;
        let url = std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".into()
        });
        let admin = sqlx::PgPool::connect(&url)
            .await
            .expect("the metadata Postgres (is the workbench stack up?)");
        let suffix = &uuid::Uuid::new_v4().simple().to_string()[..10];
        let database = format!("t6_visible_{suffix}");
        let owner = format!("t6_vowner_{suffix}");
        admin
            .execute(sqlx::AssertSqlSafe(format!(
                "CREATE ROLE {owner} LOGIN PASSWORD '{password}'"
            )))
            .await
            .unwrap();
        admin
            .execute(sqlx::AssertSqlSafe(format!(
                "CREATE DATABASE {database} OWNER {owner}"
            )))
            .await
            .unwrap();
        let opts: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
        // The host is a service name on the stack network (`metadata-db`): resolved, not parsed.
        let addr = tokio::net::lookup_host((opts.get_host(), opts.get_port()))
            .await
            .unwrap()
            .next()
            .expect("the metadata Postgres host resolves");
        let scratch = ScratchCompute {
            admin,
            database,
            owner,
            addr,
        };
        let owner = &scratch.owner;
        scratch
            .exec_in(&format!(
                "CREATE SCHEMA chelabase;
                 CREATE TABLE chelabase.schema_version (version bigint NOT NULL);
                 INSERT INTO chelabase.schema_version VALUES (1);
                 GRANT USAGE ON SCHEMA chelabase TO {owner};
                 GRANT SELECT ON chelabase.schema_version TO {owner};
                 CREATE TABLE public.\"we\"\"ird\" (id bigint, tag text, body text, PRIMARY KEY (id, tag));
                 ALTER TABLE public.\"we\"\"ird\" OWNER TO {owner};
                 INSERT INTO public.\"we\"\"ird\" VALUES (9007199254740993, 'he said \"hi\"', 'x'), (1, 'a', 'y');
                 CREATE TABLE public.forced (id int PRIMARY KEY);
                 ALTER TABLE public.forced OWNER TO {owner};
                 ALTER TABLE public.forced ENABLE ROW LEVEL SECURITY;
                 ALTER TABLE public.forced FORCE ROW LEVEL SECURITY;
                 CREATE SCHEMA locked;
                 CREATE TABLE locked.t (id int PRIMARY KEY);
                 GRANT SELECT ON locked.t TO {owner};"
            ))
            .await;
        scratch
    }

    async fn exec_in(&self, sql: &str) {
        use sqlx::Executor;
        let opts: sqlx::postgres::PgConnectOptions = (*self.admin.connect_options()).clone();
        let pool = sqlx::PgPool::connect_with(opts.database(&self.database))
            .await
            .unwrap();
        pool.execute(sqlx::AssertSqlSafe(sql.to_owned()))
            .await
            .unwrap();
        pool.close().await;
    }

    async fn oid(&self, relation: &str) -> u32 {
        let opts: sqlx::postgres::PgConnectOptions = (*self.admin.connect_options()).clone();
        let pool = sqlx::PgPool::connect_with(opts.database(&self.database))
            .await
            .unwrap();
        let (oid,): (i64,) = sqlx::query_as("SELECT $1::text::regclass::oid::bigint")
            .bind(relation)
            .fetch_one(&pool)
            .await
            .unwrap();
        pool.close().await;
        oid as u32
    }

    async fn drop(self) {
        use sqlx::Executor;
        self.admin
            .execute(sqlx::AssertSqlSafe(format!(
                "DROP DATABASE {} WITH (FORCE)",
                self.database
            )))
            .await
            .unwrap();
        self.admin
            .execute(sqlx::AssertSqlSafe(format!("DROP ROLE {}", self.owner)))
            .await
            .unwrap();
    }
}

/// [`rt`] whose main database is the scratch database (owner and sealed password included) and
/// whose compute is the scratch Postgres.
async fn rt_live(scratch: &ScratchCompute, password: &str) -> Rt {
    let t = rt().await;
    let main = t.main();
    {
        let mut databases = t.metadata.databases.lock().unwrap();
        let record = databases.get_mut(&t.db).unwrap();
        record.owner_role = Some(scratch.owner.clone());
        record.database_name = Some(scratch.database.clone());
    }
    {
        let mut all = t.metadata.branch_dbs.lock().unwrap();
        let web1 = all
            .get_mut(&main)
            .unwrap()
            .iter_mut()
            .find(|d| d.name == "web1")
            .unwrap();
        web1.name = scratch.database.clone();
        web1.owner = scratch.owner.clone();
    }
    t.metadata
        .branches
        .lock()
        .unwrap()
        .get_mut(&main)
        .unwrap()
        .owner_password_enc = Some(seal_credential(
        &t.app.key,
        &main.to_string(),
        Purpose::Owner,
        password,
    ));
    *t.app.locator.addr.lock().unwrap() = Some(scratch.addr);
    t
}

/// A member's authorize, end to end through the verifier and the gates, and the owner's
/// probe and key conversion on the owner pool (R19, W2, W4, P7, S11, S12).
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn visible_probes_and_converts_keys_on_the_owner_pool() {
    let password = "visible-pass";
    let scratch = ScratchCompute::new(password).await;
    let t = rt_live(&scratch, password).await;
    let db = scratch.database.clone();

    // Authorize: the tenant admin as the owner, a Developer refused.
    let body = t
        .authorize(DEFAULT_SUB, t.target(t.main(), &db, None))
        .await;
    assert_allowed(&body, t.tenant, DEFAULT_SUB);
    let body = t.authorize("dev1", t.target(t.main(), &db, None)).await;
    assert_refused(&body, "forbidden");

    // Probe only.
    let weird = scratch.oid(r#"public."we""ird""#).await;
    let (status, body) = t
        .visible(
            DEFAULT_SUB,
            t.visible_body(t.main(), &db, weird, None, None),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!({ "probe": { "ok": true, "bypass": true }, "visible": true, "keys": [] })
    );

    // Key conversion: lossless texts, only the rows that exist, S11's key strings.
    let keys = json!([
        ["9007199254740993", "he said \"hi\""],
        ["1", "missing"],
        ["1", "a"]
    ]);
    let (status, body) = t
        .visible(
            DEFAULT_SUB,
            t.visible_body(t.main(), &db, weird, None, Some(keys)),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    let mut got: Vec<String> = body["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k.as_str().unwrap().to_string())
        .collect();
    got.sort();
    let mut want = vec![
        serde_json::to_string(&["9007199254740993", "he said \"hi\""]).unwrap(),
        serde_json::to_string(&["1", "a"]).unwrap(),
    ];
    want.sort();
    assert_eq!(got, want);
    assert_eq!(body["visible"], json!(true));

    // A key row of the wrong arity is the caller's bug.
    let (status, _) = t
        .visible(
            DEFAULT_SUB,
            t.visible_body(t.main(), &db, weird, None, Some(json!([["1"]]))),
        )
        .await;
    assert_eq!(status, 400);

    // FORCE ROW LEVEL SECURITY: the owner is an RLS viewer there.
    let forced = scratch.oid("public.forced").await;
    let (_, body) = t
        .visible(
            DEFAULT_SUB,
            t.visible_body(t.main(), &db, forced, None, None),
        )
        .await;
    assert_eq!(
        body["probe"],
        json!({ "ok": true, "bypass": false }),
        "{body}"
    );

    // W2: SELECT on a table in a schema the owner can't use: the probe fails, no keys.
    let locked = scratch.oid("locked.t").await;
    let (status, body) = t
        .visible(
            DEFAULT_SUB,
            t.visible_body(t.main(), &db, locked, None, Some(json!([["1"]]))),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["probe"]["ok"], json!(false), "{body}");
    assert_eq!(body["keys"], json!([]), "{body}");

    // An OID that is no relation: the probe fails.
    let (_, body) = t
        .visible(
            DEFAULT_SUB,
            t.visible_body(t.main(), &db, 4_000_000_000, None, None),
        )
        .await;
    assert_eq!(body["probe"]["ok"], json!(false), "{body}");

    // P7: an xid still in progress isn't visible (and gives no keys); committed, it is.
    let opts: sqlx::postgres::PgConnectOptions = (*scratch.admin.connect_options()).clone();
    let mut writer = <sqlx::PgConnection as sqlx::Connection>::connect_with(&opts.database(&db))
        .await
        .unwrap();
    sqlx::raw_sql(r#"BEGIN; INSERT INTO public."we""ird" VALUES (7, 'new', 'z')"#)
        .execute(&mut writer)
        .await
        .unwrap();
    let (xid,): (String,) = sqlx::query_as("SELECT pg_current_xact_id()::text")
        .fetch_one(&mut writer)
        .await
        .unwrap();
    let new_key = json!([["7", "new"]]);
    let (status, body) = t
        .visible(
            DEFAULT_SUB,
            t.visible_body(t.main(), &db, weird, Some(&xid), Some(new_key.clone())),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        (body["visible"].clone(), body["keys"].clone()),
        (json!(false), json!([])),
        "{body}"
    );
    sqlx::raw_sql("COMMIT").execute(&mut writer).await.unwrap();
    let (_, body) = t
        .visible(
            DEFAULT_SUB,
            t.visible_body(t.main(), &db, weird, Some(&xid), Some(new_key)),
        )
        .await;
    assert_eq!(body["visible"], json!(true), "{body}");
    assert_eq!(
        body["keys"],
        json!([serde_json::to_string(&["7", "new"]).unwrap()]),
        "{body}"
    );

    drop(writer);
    scratch.drop().await;
}

/// RT1 T8: the convert mode (owner deletes) prints keys of rows that don't exist in the owner
/// session's form, in order; a relation whose key type is outside P6 is the caller's bug (400),
/// and a text the type can't read fails (503: realtime sends keyless deletes).
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn visible_convert_mode_prints_keys_and_refuses_keys_outside_p6() {
    let password = "convert-pass";
    let scratch = ScratchCompute::new(password).await;
    let t = rt_live(&scratch, password).await;
    let db = scratch.database.clone();
    let owner = scratch.owner.clone();
    scratch
        .exec_in(&format!(
            "CREATE TABLE public.hosts (addr inet PRIMARY KEY);
             ALTER TABLE public.hosts OWNER TO {owner};
             CREATE TABLE public.amounts (amount numeric, id bigint, PRIMARY KEY (amount, id));
             ALTER TABLE public.amounts OWNER TO {owner};"
        ))
        .await;
    let convert = |rel: u32, keys: Value| {
        let mut body = t.visible_body(t.main(), &db, rel, Some("not an xid"), Some(keys));
        body["convert"] = json!(true);
        body
    };

    let hosts = scratch.oid("public.hosts").await;
    let (status, body) = t
        .visible(DEFAULT_SUB, convert(hosts, json!([["10.0.0.1"]])))
        .await;
    assert_eq!(status, 400, "inet is outside P6: {body}");

    let amounts = scratch.oid("public.amounts").await;
    let (status, body) = t
        .visible(
            DEFAULT_SUB,
            convert(amounts, json!([["1.50", "9007199254740993"], ["2", "1"]])),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!({ "keys": [
            serde_json::to_string(&["1.50", "9007199254740993"]).unwrap(),
            serde_json::to_string(&["2", "1"]).unwrap(),
        ] }),
        "no rows exist: printed, in order, the numeric's scale kept"
    );

    let (status, body) = t
        .visible(DEFAULT_SUB, convert(amounts, json!([["one", "1"]])))
        .await;
    assert_eq!(status, 503, "a text numeric can't read: {body}");

    scratch.drop().await;
}

// --- Members hub step 1, final fix round, area A ----------------------------------------------

const MEMBER: &str = "/internal/realtime/member";

// T2-4: realtime's project list reads every grant source: a member reached only through a
// (fake) team grant sees the project.
#[tokio::test]
async fn realtime_projects_read_every_grant_source() {
    let t = rt().await;
    let tam = t.metadata.add_member(t.tenant, "tam", MemberStatus::Active, false);
    t.metadata.team_grant(
        tam,
        t.project,
        Grant {
            platform_role: PlatformRole::Viewer,
            roles: chelabase_types::access::RoleSet::Only(Default::default()),
        },
    );
    let (status, body) = t
        .post(PROJECTS, Some(REALTIME_TOKEN), json!({ "token": t.token("tam") }))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        projects_of(&body),
        vec![(t.project.as_uuid().to_string(), vec![t.db.as_uuid().to_string()])]
    );
}

// Members hub step 2 (T23, MH-2): a member reached only through a team made with the team API
// sees the project in realtime's list; once removed from the team, it doesn't.
#[tokio::test]
async fn team_only_member_gets_realtime_projects() {
    let t = rt().await;
    let tam = t.metadata.add_member(t.tenant, "tam", MemberStatus::Active, false);
    let team = api_team(&t.app, "Readers", t.project, "viewer", &[], &[tam]).await;
    let (status, body) = t
        .post(PROJECTS, Some(REALTIME_TOKEN), json!({ "token": t.token("tam") }))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        projects_of(&body),
        vec![(t.project.as_uuid().to_string(), vec![t.db.as_uuid().to_string()])]
    );
    let resp = t
        .app
        .send_as(DEFAULT_SUB, "org-1", Method::DELETE, &format!("/v1/teams/{team}/members/{tam}"), None)
        .await;
    assert_eq!(resp.status(), 204);
    let (status, body) = t
        .post(PROJECTS, Some(REALTIME_TOKEN), json!({ "token": t.token("tam") }))
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(projects_of(&body).is_empty(), "{body}");
}

// D7, T8-1: realtime asks before it issues a Studio ticket. `/internal/realtime/member` (its
// bearer only; `{sub, tenant_id}` from the gateway's identity) answers `{allowed: true}` for an
// active member and `{allowed: false, reason: "forbidden"}` for a suspended or offboarded one,
// a non-member, or another tenant; the project list refuses an offboarded member like a
// suspended one.
#[tokio::test]
async fn realtime_refuses_suspended_and_offboarded_members() {
    let t = rt().await;
    t.metadata.add_granted(t.tenant, "sus", t.project, PlatformRole::Developer, &[]);
    t.metadata.set_member(t.tenant, "sus", |m| m.status = MemberStatus::Suspended);
    t.metadata.add_granted(t.tenant, "off", t.project, PlatformRole::Developer, &[]);
    t.metadata.set_member(t.tenant, "off", |m| m.offboarded = true);
    let ask = |sub: &str, tenant: TenantId| json!({ "sub": sub, "tenant_id": tenant });

    let (status, body) = t.post(MEMBER, Some(REALTIME_TOKEN), ask("dev1", t.tenant)).await;
    assert_eq!((status, body["allowed"].clone()), (200, json!(true)), "{body}");
    let other = TenantId::new();
    for (sub, tenant) in [("sus", t.tenant), ("off", t.tenant), ("stranger", t.tenant), ("dev1", other)] {
        let (status, body) = t.post(MEMBER, Some(REALTIME_TOKEN), ask(sub, tenant)).await;
        assert_eq!(status, 200, "{sub} {tenant}: {body}");
        assert_refused(&body, "forbidden");
    }
    let (status, _) = t.post(MEMBER, None, ask("dev1", t.tenant)).await;
    assert_eq!(status, 401, "realtime's bearer only");

    let (status, _) = t
        .post(PROJECTS, Some(REALTIME_TOKEN), json!({ "token": t.token("off") }))
        .await;
    assert_eq!(status, 403, "an offboarded member's projects");
}

// --- Object storage m17: the storage channel ----------------------------------------------------

/// AC 4: a `storage:` join is authorized by `/authorize` with the storage console's own rules
/// (m9's `authorize`: `ActAs(Viewer)`, owner mode Admin-only, then the branch and database
/// gates and the worker's role check). One row per (member, Act-as role): a Viewer holding the
/// role watches it; one without it, a Developer asking for another role, any non-Admin asking
/// for owner mode and a non-member are refused.
#[tokio::test]
async fn storage_realtime_join_matrix_matches_the_console_list_route() {
    let t = rt().await;
    t.metadata.add_granted(t.tenant, "viewer2", t.project, PlatformRole::Viewer, &["app"]);
    let rows: [(&str, Option<&str>, bool); 9] = [
        (DEFAULT_SUB, None, true),
        (DEFAULT_SUB, Some("app"), true),
        ("dev1", Some("app"), true),
        ("viewer2", Some("app"), true),
        ("dev1", None, false),
        ("dev1", Some("other"), false),
        ("viewer1", None, false),
        ("viewer1", Some("app"), false),
        ("stranger", Some("app"), false),
    ];
    for (sub, role, allowed) in rows {
        let body = t.authorize(sub, t.target(t.main(), "web1", role)).await;
        if allowed {
            assert_allowed(&body, t.tenant, sub);
        } else {
            assert_refused(&body, "forbidden");
        }
    }
    // The gates: a backup branch, a database the data API doesn't serve.
    assert_refused(&t.authorize(DEFAULT_SUB, t.target(t.backup, "web1", None)).await, "forbidden");
    assert_refused(&t.authorize(DEFAULT_SUB, t.target(t.main(), "analytics", None)).await, "forbidden");
}

/// `/visible`'s `bucket` goes with a check: never with the convert mode, and it is a UUID.
#[tokio::test]
async fn storage_realtime_visible_bucket_goes_with_a_check() {
    let t = rt().await;
    let mut body = t.visible_body(t.main(), "web1", 16_384, None, Some(json!([["x"]])));
    body["convert"] = json!(true);
    body["bucket"] = json!(uuid::Uuid::new_v4());
    assert_eq!(t.visible(DEFAULT_SUB, body).await.0, 400, "bucket with the convert mode");
    let mut body = t.visible_body(t.main(), "web1", 16_384, None, None);
    body["bucket"] = json!("images");
    assert_eq!(t.visible(DEFAULT_SUB, body).await.0, 400, "a bucket is a UUID");
}

/// AC 5 (owner mode): with `bucket`, the owner's check returns only the objects of that bucket
/// it sees, and only while the bucket exists; `bucket` on another relation is a 400.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn storage_realtime_visible_scopes_the_owners_check_to_a_bucket() {
    let password = "storage-visible-pass";
    let scratch = ScratchCompute::new(password).await;
    let t = rt_live(&scratch, password).await;
    let db = scratch.database.clone();
    let owner = scratch.owner.clone();
    let (a, b, gone) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    let (o1, o2, o3) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    scratch
        .exec_in(&format!(
            "CREATE SCHEMA storage AUTHORIZATION {owner};
             CREATE TABLE storage.buckets (id uuid PRIMARY KEY, name text NOT NULL);
             CREATE TABLE storage.objects (id uuid PRIMARY KEY, bucket_id uuid NOT NULL, name text NOT NULL);
             ALTER TABLE storage.buckets OWNER TO {owner};
             ALTER TABLE storage.objects OWNER TO {owner};
             INSERT INTO storage.buckets VALUES ('{a}', 'a'), ('{b}', 'b');
             INSERT INTO storage.objects VALUES ('{o1}', '{a}', 'x'), ('{o2}', '{b}', 'y'), ('{o3}', '{gone}', 'z');"
        ))
        .await;
    let objects = scratch.oid("storage.objects").await;
    let keys = json!([[o1.to_string()], [o2.to_string()], [o3.to_string()]]);
    let scoped = |bucket: uuid::Uuid, rel: u32, keys: Value| {
        let mut body = t.visible_body(t.main(), &db, rel, None, Some(keys));
        body["bucket"] = json!(bucket);
        body
    };
    let (status, body) = t.visible(DEFAULT_SUB, scoped(a, objects, keys.clone())).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["keys"], json!([serde_json::to_string(&[o1.to_string()]).unwrap()]), "{body}");
    let (_, body) = t.visible(DEFAULT_SUB, scoped(b, objects, keys.clone())).await;
    assert_eq!(body["keys"], json!([serde_json::to_string(&[o2.to_string()]).unwrap()]), "{body}");
    let (_, body) = t.visible(DEFAULT_SUB, scoped(gone, objects, keys.clone())).await;
    assert_eq!(body["keys"], json!([]), "a bucket that doesn't exist shows nothing: {body}");
    let weird = scratch.oid(r#"public."we""ird""#).await;
    let (status, body) = t
        .visible(DEFAULT_SUB, scoped(a, weird, json!([["1", "a"]])))
        .await;
    assert_eq!(status, 400, "a bucket on another relation: {body}");
    scratch.drop().await;
}
