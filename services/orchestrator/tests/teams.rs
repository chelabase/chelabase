//! Members hub step 2 (T23, part A): the team API through the fake-backed orchestrator.
//!
//! - Who reads and who writes (Part 7's Teams row), and what a read shows (R1, P11, MH-34).
//! - Every team write, and every own-grant and override write, is checked on the union of the
//!   member's sources narrowed by their overrides, for every affected member and database
//!   (MH-4, P7); a refusal names the member, the team and the database, and writes nothing.
//! - Every team write that changed something fires the grant writes' side effects (MH-5):
//!   realtime per affected member, the proxy cache for their logins, a reconcile of the
//!   projects' branches; an unchanged one fires nothing.
//! - The answers P17 pins, the events P9 pins, and the I4 retry.
//! - The invariant, as a property test over random own-grant, override and team writes.
#![allow(dead_code, unused_imports)]

include!("common/fakes.rs");

use chelabase_types::access::RoleSet;

/// The role pool on both databases' mains (as `api.rs`'s grant-invariant tests): `plain` and
/// `group2` are NOLOGIN groups; `login` is a LOGIN role; `boss` reaches the owner (`postgres`)
/// on the first database only and is a plain group on the second; `lead` holds ADMIN on
/// `plain`; `sneaky` is a member of the owner without SET on the first database only.
const POOL: [&str; 6] = ["plain", "group2", "boss", "login", "lead", "sneaky"];

/// Whether `role` may not be held below Admin on the first database (`true`) or the second.
fn pool_bad(first: bool, role: &str) -> bool {
    role == "login" || role == "lead" || (first && (role == "boss" || role == "sneaky"))
}

/// The member world (org-1's tenant; project Billing with databases web1 and web2, the pool's
/// roles on both mains; web1 also has `dev`).
struct Tw {
    w: MemberWorld,
    db2: DatabaseId,
}

async fn tw() -> Tw {
    let w = member_world(Mode::SelfHosted).await;
    let mut db = sample_database(w.tenant);
    db.project_id = w.project;
    db.name = "web2".into();
    db.owner_role = Some("postgres".into());
    db.main.endpoint_id = Some("ep-second000000".into());
    let db2 = db.database_id;
    w.metadata.insert_database(db);
    let main_ep = w.metadata.branch(BranchId::from(w.db)).endpoint_id.unwrap();
    for (ep, first) in [(main_ep, true), ("ep-second000000".to_string(), false)] {
        let md = &w.metadata;
        md.add_group_role(&ep, "plain", &[]);
        md.add_group_role(&ep, "group2", &[]);
        md.add_group_role(&ep, "boss", if first { &["postgres"] } else { &[] });
        md.add_role(&ep, "login", RoleOrigin::Sql, false, false);
        md.add_group_role_with(&ep, "lead", &["plain"], &[], &["plain"]);
        if first {
            md.add_group_role_with(&ep, "sneaky", &["postgres"], &["postgres"], &[]);
        } else {
            md.add_group_role(&ep, "sneaky", &[]);
        }
    }
    Tw { w, db2 }
}

impl Tw {
    async fn send(
        &self,
        sub: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let resp = self.w.app.send_as(sub, "org-1", method, path, body).await;
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    /// As the default caller (the tenant admin).
    async fn admin(&self, method: Method, path: &str, body: Option<Value>) -> (u16, Value) {
        self.send(DEFAULT_SUB, method, path, body).await
    }

    fn actions(&self) -> Vec<String> {
        self.w
            .metadata
            .events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e.action.clone())
            .collect()
    }

    fn grant_of(&self, member: uuid::Uuid) -> Option<GrantRow> {
        self.w
            .metadata
            .grants
            .lock()
            .unwrap()
            .get(&(member, self.w.project.as_uuid()))
            .cloned()
    }

    fn team_grant_of(&self, team: uuid::Uuid) -> Option<GrantRow> {
        self.w
            .metadata
            .stored_team(team)
            .and_then(|t| t.grants.get(&self.w.project.as_uuid()).cloned())
    }

    fn team_members_of(&self, team: uuid::Uuid) -> Vec<uuid::Uuid> {
        self.w
            .metadata
            .stored_team(team)
            .map(|t| t.members.into_iter().collect())
            .unwrap_or_default()
    }

    /// An active member `sub` (email `<sub>@x.com`) with no grant.
    fn member(&self, sub: &str) -> uuid::Uuid {
        let id = self
            .w
            .metadata
            .add_member(self.w.tenant, sub, MemberStatus::Active, false);
        self.w.metadata.set_member(self.w.tenant, sub, |r| {
            r.email = Some(format!("{sub}@x.com"))
        });
        id
    }

    fn enqueued(&self) -> Vec<BranchId> {
        let mut out = self.w.jobs.member_role_enqueues.lock().unwrap().clone();
        out.sort_by_key(|b| b.as_uuid());
        out.dedup();
        out
    }

    fn clear_effects(&self) {
        self.w.jobs.member_role_enqueues.lock().unwrap().clear();
        self.w.app.rt_events.clear();
        self.w.app.cache.clear();
    }

    /// Waits until no spawned publish has landed for 300 ms (10 s at most).
    async fn quiesce(&self) {
        let seen = || {
            (
                self.w.jobs.member_role_enqueues.lock().unwrap().len(),
                self.w.app.rt_events.all().len(),
                self.w.app.cache.sent().len(),
            )
        };
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut last = seen();
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let now = seen();
            if now == last {
                return;
            }
            last = now;
        }
        panic!("spawned publishes never stopped: {last:?}");
    }
}

fn team_path(team: uuid::Uuid) -> String {
    format!("/v1/teams/{team}")
}

fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}

/// Polls `done` (10 ms apart, 2 s at most): spawned publishes land after the answer.
async fn eventually(done: impl Fn() -> bool) -> bool {
    for _ in 0..200 {
        if done() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    done()
}

/// Gives spawned publishes their chance before asserting that none came.
async fn settle() {
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
}

// --- Who reads, who writes (Part 7's Teams row) -------------------------------------------------

// Everyone in the tenant reads teams and a member's teams; only tenant admins write them; the
// project's team grants are its Admins'. A suspended member is refused everywhere (step 1 P14).
#[tokio::test]
async fn team_routes_by_power() {
    let t = tw().await;
    let p = t.w.project;
    t.w.metadata
        .add_granted(t.w.tenant, "padmin", p, PlatformRole::Admin, &[]);
    let dev =
        t.w.metadata
            .add_granted(t.w.tenant, "dev", p, PlatformRole::Developer, &[]);
    t.w.metadata
        .add_granted(t.w.tenant, "viewer", p, PlatformRole::Viewer, &[]);
    t.w.metadata
        .add_granted(t.w.tenant, "susp", p, PlatformRole::Developer, &[]);
    t.w.metadata
        .set_member(t.w.tenant, "susp", |m| m.status = MemberStatus::Suspended);
    let team = t.w.metadata.seed_team(t.w.tenant, "Billing team");
    t.w.metadata
        .seed_team_grant(team, p, PlatformRole::Developer, &["plain"]);
    t.w.metadata.seed_team_member(team, dev);

    let reads = [
        "/v1/teams".to_string(),
        team_path(team),
        format!("/v1/members/{dev}/teams"),
    ];
    for sub in [DEFAULT_SUB, "padmin", "dev", "viewer"] {
        for path in &reads {
            let (status, body) = t.send(sub, Method::GET, path, None).await;
            assert_eq!(status, 200, "GET {path} as {sub}: {body}");
        }
    }
    let team_grants = format!("/v1/projects/{p}/team-grants");
    for sub in [DEFAULT_SUB, "padmin"] {
        let (status, body) = t.send(sub, Method::GET, &team_grants, None).await;
        assert_eq!(status, 200, "GET {team_grants} as {sub}: {body}");
    }
    for sub in ["dev", "viewer"] {
        let (status, body) = t.send(sub, Method::GET, &team_grants, None).await;
        assert_eq!(
            (status, code(&body), body["error"]["required"].as_str()),
            (403, "forbidden", Some("admin")),
            "{sub}: {body}"
        );
    }

    let writes: Vec<(Method, String, Option<Value>)> = vec![
        (
            Method::POST,
            "/v1/teams".into(),
            Some(json!({ "name": "Ops" })),
        ),
        (
            Method::PATCH,
            team_path(team),
            Some(json!({ "name": "Renamed" })),
        ),
        (
            Method::PUT,
            format!("{}/grants/{p}", team_path(team)),
            Some(json!({ "platform_role": "viewer", "pg_roles": [] })),
        ),
        (
            Method::DELETE,
            format!("{}/grants/{p}", team_path(team)),
            None,
        ),
        (
            Method::PUT,
            format!("{}/members/{dev}", team_path(team)),
            None,
        ),
        (
            Method::DELETE,
            format!("{}/members/{dev}", team_path(team)),
            None,
        ),
        (Method::DELETE, team_path(team), None),
    ];
    for sub in ["padmin", "dev", "viewer"] {
        for (method, path, body) in &writes {
            let (status, answer) = t.send(sub, method.clone(), path, body.clone()).await;
            assert_eq!(
                (status, code(&answer), answer["error"]["required"].as_str()),
                (403, "forbidden", Some("tenant admin")),
                "{method} {path} as {sub}: {answer}"
            );
        }
    }
    for (method, path, body) in reads
        .iter()
        .map(|p| (Method::GET, p.clone(), None))
        .chain([(Method::GET, team_grants.clone(), None)])
        .chain(writes.clone())
    {
        let (status, answer) = t.send("susp", method.clone(), &path, body).await;
        assert_eq!(
            (status, code(&answer)),
            (403, "member_suspended"),
            "{method} {path} as susp: {answer}"
        );
    }
    assert!(
        t.actions().is_empty(),
        "a refused write logged: {:?}",
        t.actions()
    );
    assert_eq!(
        t.team_grant_of(team).map(|g| g.platform_role),
        Some(PlatformRole::Developer),
        "nothing changed"
    );

    // The tenant admin's writes go through, in an order that keeps each one meaningful.
    let expected = [201, 200, 200, 204, 200, 204, 204];
    for ((method, path, body), want) in writes.into_iter().zip(expected) {
        let (status, answer) = t.admin(method.clone(), &path, body).await;
        assert_eq!(
            status, want,
            "{method} {path} as the tenant admin: {answer}"
        );
    }
}

// --- Reads (R1, P11, MH-34) ---------------------------------------------------------------------

// A team's projects are the caller's visible ones only (R1, any access source counts); its
// members are shown to everyone, as the roster is; `member_ids` is absent from the list.
#[tokio::test]
async fn team_reads_hide_projects_the_caller_cannot_see() {
    let t = tw().await;
    let a = t.w.project;
    let b =
        t.w.metadata
            .create_project(t.w.tenant, "Other")
            .await
            .unwrap()
            .id;
    let alice =
        t.w.metadata
            .add_granted(t.w.tenant, "alice", a, PlatformRole::Viewer, &[]);
    let bob =
        t.w.metadata
            .add_granted(t.w.tenant, "bob", b, PlatformRole::Viewer, &[]);
    let carol = t.member("carol");
    // `dave` is the third member; `alice` (own Viewer on A) is in no team, so B is no source of
    // hers (P11: any access source makes a project visible).
    let dave = t.member("dave");
    t.member("nobody");
    let team = t.w.metadata.seed_team(t.w.tenant, "Mixed");
    t.w.metadata
        .seed_team_grant(team, a, PlatformRole::Developer, &["plain"]);
    t.w.metadata
        .seed_team_grant(team, b, PlatformRole::Viewer, &[]);
    for m in [dave, bob, carol] {
        t.w.metadata.seed_team_member(team, m);
    }
    assert!(!t.team_members_of(team).contains(&alice));
    let projects_of = |team: &Value| -> Vec<String> {
        team["projects"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .map(|p| p["project_id"].as_str().unwrap_or("").to_string())
            .collect()
    };
    let mut both = vec![a.to_string(), b.to_string()];
    both.sort();
    let mut members = vec![dave.to_string(), bob.to_string(), carol.to_string()];
    members.sort();

    // The list: alice sees project A only, nobody sees none; the tenant admin and carol (who
    // reaches both through the team itself) see both.
    for (sub, want) in [
        ("alice", vec![a.to_string()]),
        ("nobody", vec![]),
        (DEFAULT_SUB, both.clone()),
        ("carol", both.clone()),
    ] {
        let (status, list) = t.send(sub, Method::GET, "/v1/teams", None).await;
        assert_eq!(status, 200, "{sub}: {list}");
        let listed = list
            .as_array()
            .unwrap_or_else(|| panic!("an array: {list}"));
        assert_eq!(listed.len(), 1, "{sub}: {list}");
        let mut got = projects_of(&listed[0]);
        got.sort();
        assert_eq!(got, want, "{sub}: {list}");
        assert_eq!(
            listed[0]["member_count"], 3,
            "{sub}: members are counted for everyone: {list}"
        );
        assert!(
            listed[0].get("member_ids").is_none(),
            "{sub}: the list carries no member ids: {list}"
        );
        assert_eq!(
            (listed[0]["id"].as_str(), listed[0]["name"].as_str()),
            (Some(team.to_string().as_str()), Some("Mixed"))
        );
    }
    // One team: the same filter, with its member ids for everyone.
    let (status, one) = t.send("alice", Method::GET, &team_path(team), None).await;
    assert_eq!(status, 200, "{one}");
    assert_eq!(projects_of(&one), vec![a.to_string()], "{one}");
    let mut ids: Vec<String> = one["member_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    ids.sort();
    assert_eq!(ids, members, "{one}");
    // A member's teams: the same filter (P14).
    let (status, theirs) = t
        .send(
            "alice",
            Method::GET,
            &format!("/v1/members/{bob}/teams"),
            None,
        )
        .await;
    assert_eq!(status, 200, "{theirs}");
    assert_eq!(projects_of(&theirs[0]), vec![a.to_string()], "{theirs}");
    // A deleted project's grant is no access and is never shown.
    t.w.metadata
        .projects
        .lock()
        .unwrap()
        .iter_mut()
        .find(|p| p.id == b)
        .unwrap()
        .status = ProjectStatus::Deleted;
    let (_, one) = t.admin(Method::GET, &team_path(team), None).await;
    assert_eq!(projects_of(&one), vec![a.to_string()], "{one}");
}

// MH-34: a team's Postgres roles on a project are shown to tenant admins and that project's
// Admins only; everyone else gets `pg_roles: null` (the platform role is still shown).
#[tokio::test]
async fn team_pg_roles_are_shown_only_to_admins_of_the_project() {
    let t = tw().await;
    let a = t.w.project;
    let b =
        t.w.metadata
            .create_project(t.w.tenant, "Other")
            .await
            .unwrap()
            .id;
    t.w.metadata
        .add_granted(t.w.tenant, "padmin", a, PlatformRole::Admin, &[]);
    let dev =
        t.w.metadata
            .add_granted(t.w.tenant, "dev", a, PlatformRole::Developer, &[]);
    // An Admin elsewhere, a Viewer here: not an Admin of A.
    let bmin =
        t.w.metadata
            .add_granted(t.w.tenant, "bmin", b, PlatformRole::Admin, &[]);
    t.w.metadata.grant(bmin, a, PlatformRole::Viewer, &[]);
    let team = t.w.metadata.seed_team(t.w.tenant, "Billing team");
    t.w.metadata
        .seed_team_grant(team, a, PlatformRole::Developer, &["plain", "group2"]);
    t.w.metadata.seed_team_member(team, dev);
    for (sub, want) in [
        (DEFAULT_SUB, json!(["group2", "plain"])),
        ("padmin", json!(["group2", "plain"])),
        ("dev", Value::Null),
        ("bmin", Value::Null),
    ] {
        for path in [team_path(team), format!("/v1/members/{dev}/teams")] {
            let (status, body) = t.send(sub, Method::GET, &path, None).await;
            assert_eq!(status, 200, "{sub} {path}: {body}");
            let team_view = if body.is_array() {
                body[0].clone()
            } else {
                body.clone()
            };
            let access = &team_view["projects"][0];
            assert_eq!(access["project_id"], json!(a), "{sub} {path}: {body}");
            assert_eq!(access["platform_role"], "developer", "{sub} {path}: {body}");
            assert_eq!(access["pg_roles"], want, "{sub} {path}: {body}");
        }
    }
    // The project's team grants (its Admins' route) name the roles and the members (P15).
    let (status, list) = t
        .send(
            "padmin",
            Method::GET,
            &format!("/v1/projects/{a}/team-grants"),
            None,
        )
        .await;
    assert_eq!(status, 200, "{list}");
    assert_eq!(
        list,
        json!([{ "team_id": team, "name": "Billing team", "platform_role": "developer", "pg_roles": ["group2", "plain"], "member_ids": [dev] }])
    );
}

// --- Writes checked on the union (MH-4, P7) -----------------------------------------------------

// MH-4a's scenario: own Developer {plain}, an override on web1 {Viewer, [login]} (`login` is a
// LOGIN role; the override keeps nothing of the own grant), then a team Admin grant: Admin from
// any source keeps the override's roles (R17), so the member would hold `login` below Admin on
// web1. Refused naming the member, the team and the database; nothing written.
#[tokio::test]
async fn a_team_grant_is_checked_on_the_union_for_every_member_and_database() {
    let t = tw().await;
    let m = t.w.metadata.add_granted(
        t.w.tenant,
        "m",
        t.w.project,
        PlatformRole::Developer,
        &["plain"],
    );
    t.w.metadata.narrow(
        m,
        t.w.db,
        OverrideRow {
            platform_role: Some(PlatformRole::Viewer),
            pg_roles: Some(vec!["login".into()]),
        },
    );
    let fine =
        t.w.metadata
            .add_granted(t.w.tenant, "fine", t.w.project, PlatformRole::Admin, &[]);
    // A Viewer with no override: what the team gives is what they hold.
    let d =
        t.w.metadata
            .add_granted(t.w.tenant, "d", t.w.project, PlatformRole::Viewer, &[]);
    let (status, body) = t
        .admin(Method::POST, "/v1/teams", Some(json!({ "name": "Ops" })))
        .await;
    assert_eq!(status, 201, "{body}");
    let team: uuid::Uuid = body["id"].as_str().unwrap().parse().unwrap();
    for member in [fine, m, d] {
        api_add_to_team(&t.w.app, team, member).await;
    }
    let events = t.actions().len();
    let grant = format!("{}/grants/{}", team_path(team), t.w.project);
    let (status, err) = t
        .admin(
            Method::PUT,
            &grant,
            Some(json!({ "platform_role": "admin", "pg_roles": [] })),
        )
        .await;
    assert_eq!((status, code(&err)), (422, "invalid_request"), "{err}");
    assert_eq!(err["error"]["role"], "login", "{err}");
    assert_eq!(err["error"]["database_id"], json!(t.w.db), "{err}");
    assert_eq!(err["error"]["member_id"], json!(m), "{err}");
    assert_eq!(err["error"]["team_id"], json!(team), "{err}");
    assert_eq!(t.team_grant_of(team), None, "nothing written");
    assert_eq!(t.actions().len(), events, "no event: {:?}", t.actions());
    // `boss` reaches the owner on web1 only: a Developer team grant naming it is refused on web1
    // for `d` (`m`'s override keeps only `login` there, and the Admin member is never checked),
    // whatever web2 allows.
    let (status, err) = t
        .admin(
            Method::PUT,
            &grant,
            Some(json!({ "platform_role": "developer", "pg_roles": ["boss"] })),
        )
        .await;
    assert_eq!(
        (status, err["error"]["role"].as_str()),
        (422, Some("boss")),
        "{err}"
    );
    assert_eq!(err["error"]["database_id"], json!(t.w.db), "{err}");
    assert_eq!(err["error"]["member_id"], json!(d), "{err}");
    // A safe grant goes through.
    let (status, body) = t
        .admin(
            Method::PUT,
            &grant,
            Some(json!({ "platform_role": "developer", "pg_roles": ["plain"] })),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        t.team_grant_of(team),
        Some(GrantRow {
            platform_role: PlatformRole::Developer,
            pg_roles: vec!["plain".into()]
        })
    );
}

// Adding a member to a team is checked on the member's union: the team's Admin grant would make
// the override's `login` theirs below Admin on web1.
#[tokio::test]
async fn adding_a_member_is_checked_on_the_union() {
    let t = tw().await;
    let team = api_team(&t.w.app, "Admins", t.w.project, "admin", &[], &[]).await;
    let m = t.w.metadata.add_granted(
        t.w.tenant,
        "m",
        t.w.project,
        PlatformRole::Developer,
        &["plain"],
    );
    t.w.metadata.narrow(
        m,
        t.w.db,
        OverrideRow {
            platform_role: Some(PlatformRole::Viewer),
            pg_roles: Some(vec!["login".into()]),
        },
    );
    let path = format!("{}/members/{m}", team_path(team));
    let (status, err) = t.admin(Method::PUT, &path, None).await;
    assert_eq!((status, code(&err)), (422, "invalid_request"), "{err}");
    assert_eq!(
        (
            err["error"]["role"].as_str(),
            err["error"]["database_id"].clone(),
            err["error"]["member_id"].clone(),
            err["error"]["team_id"].clone()
        ),
        (Some("login"), json!(t.w.db), json!(m), json!(team)),
        "{err}"
    );
    assert!(!t.team_members_of(team).contains(&m), "not added");
    // A member with nothing to break is added.
    let n = t.member("n");
    let (status, body) = t
        .admin(
            Method::PUT,
            &format!("{}/members/{n}", team_path(team)),
            None,
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(t.team_members_of(team).contains(&n));
}

// An override is checked on the union: own Admin with a team Developer {boss}. Narrowing web1
// to Developer drops the Admin source's "every role", but the team's `boss` stays at Developer,
// and it reaches the owner there. On web2 `boss` is a plain group: fine. And taking an override
// away is checked on the union too: the team's `boss` comes back on web1.
#[tokio::test]
async fn an_override_write_is_checked_on_the_union() {
    let t = tw().await;
    let m =
        t.w.metadata
            .add_granted(t.w.tenant, "m", t.w.project, PlatformRole::Admin, &[]);
    api_team_joined(
        &t.w.app,
        "Bosses",
        t.w.project,
        "developer",
        &["boss"],
        &[m],
    )
    .await;
    let o = format!("/v1/projects/{}/grants/{m}/overrides", t.w.project);
    let (status, err) = t
        .admin(
            Method::PUT,
            &o,
            Some(json!({ "database_id": t.w.db, "platform_role": "developer" })),
        )
        .await;
    assert_eq!(
        (status, err["error"]["role"].as_str()),
        (422, Some("boss")),
        "{err}"
    );
    assert_eq!(err["error"]["database_id"], json!(t.w.db), "{err}");
    assert!(
        t.w.metadata
            .overrides
            .lock()
            .unwrap()
            .get(&(m, t.w.db.as_uuid()))
            .is_none(),
        "nothing written"
    );
    let (status, body) = t
        .admin(
            Method::PUT,
            &o,
            Some(json!({ "database_id": t.db2, "platform_role": "developer" })),
        )
        .await;
    assert_eq!(status, 200, "{body}");

    // The delete: own Developer {plain}, web1 narrowed to [plain], a team Developer {boss}.
    let n = t.w.metadata.add_granted(
        t.w.tenant,
        "n",
        t.w.project,
        PlatformRole::Developer,
        &["plain"],
    );
    t.w.metadata.narrow(
        n,
        t.w.db,
        OverrideRow {
            platform_role: None,
            pg_roles: Some(vec!["plain".into()]),
        },
    );
    api_team_joined(
        &t.w.app,
        "More bosses",
        t.w.project,
        "developer",
        &["boss"],
        &[n],
    )
    .await;
    let del = format!(
        "/v1/projects/{}/grants/{n}/overrides?database_id={}",
        t.w.project, t.w.db
    );
    let (status, err) = t.admin(Method::DELETE, &del, None).await;
    assert_eq!(
        (status, err["error"]["role"].as_str()),
        (422, Some("boss")),
        "{err}"
    );
    assert_eq!(err["error"]["database_id"], json!(t.w.db), "{err}");
    assert!(
        t.w.metadata
            .overrides
            .lock()
            .unwrap()
            .contains_key(&(n, t.w.db.as_uuid())),
        "the override stays"
    );
}

// An own grant is checked on the union: own Admin with a team Developer {boss}. Lowering the own
// grant to Developer, or deleting it, leaves the team's `boss` at Developer on web1.
#[tokio::test]
async fn an_own_grant_write_is_checked_on_the_union() {
    let t = tw().await;
    let m =
        t.w.metadata
            .add_granted(t.w.tenant, "m", t.w.project, PlatformRole::Admin, &[]);
    api_team_joined(
        &t.w.app,
        "Bosses",
        t.w.project,
        "developer",
        &["boss"],
        &[m],
    )
    .await;
    let grant = format!("/v1/projects/{}/grants/{m}", t.w.project);
    let (status, err) = t
        .admin(
            Method::PUT,
            &grant,
            Some(json!({ "platform_role": "developer", "pg_roles": ["plain"] })),
        )
        .await;
    assert_eq!(
        (status, err["error"]["role"].as_str()),
        (422, Some("boss")),
        "{err}"
    );
    assert_eq!(err["error"]["database_id"], json!(t.w.db), "{err}");
    assert_eq!(
        t.grant_of(m).map(|g| g.platform_role),
        Some(PlatformRole::Admin),
        "nothing written"
    );
    let (status, err) = t.admin(Method::DELETE, &grant, None).await;
    assert_eq!(
        (status, err["error"]["role"].as_str()),
        (422, Some("boss")),
        "{err}"
    );
    assert_eq!(
        t.grant_of(m).map(|g| g.platform_role),
        Some(PlatformRole::Admin),
        "the grant stays"
    );
}

// R7, MH-21, S12: a team grant never names `owner` or the database's owner, a reserved name or
// a member's personal login, at any level.
#[tokio::test]
async fn team_grants_refuse_owner_reserved_and_personal_login_names() {
    let t = tw().await;
    let m =
        t.w.metadata
            .add_granted(t.w.tenant, "m", t.w.project, PlatformRole::Developer, &[]);
    t.w.metadata.seed_personal_login(m, t.w.db, "m_login");
    let (status, body) = t
        .admin(Method::POST, "/v1/teams", Some(json!({ "name": "Ops" })))
        .await;
    assert_eq!(status, 201, "{body}");
    let team: uuid::Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let grant = format!("{}/grants/{}", team_path(team), t.w.project);
    for role in ["developer", "admin"] {
        for name in [
            "owner",
            "OWNER",
            "postgres",
            "cb_m_0123",
            "chelabase_x",
            "pg_read_all_data",
            "m_login",
        ] {
            let (status, err) = t
                .admin(
                    Method::PUT,
                    &grant,
                    Some(json!({ "platform_role": role, "pg_roles": [name] })),
                )
                .await;
            assert_eq!(
                (status, code(&err)),
                (422, "invalid_request"),
                "{role} {name}: {err}"
            );
            assert_eq!(err["error"]["role"], name, "{role} {name}: {err}");
        }
    }
    assert_eq!(t.team_grant_of(team), None, "nothing written");
}

// --- Side effects (MH-5) ------------------------------------------------------------------------

// Every team write that changed something: realtime re-checks each affected member, the proxies
// forget their logins' rows, every branch of the affected projects reconciles. An unchanged write
// fires nothing. A write that ends a member's Developer level revokes their login (MH-9).
#[tokio::test]
async fn every_team_write_fires_realtime_cache_and_reconcile() {
    let w = member_world(Mode::SelfHosted).await;
    let ep = w.metadata.branch(BranchId::from(w.db)).endpoint_id.unwrap();
    w.metadata.add_group_role(&ep, "plain", &[]);
    w.metadata.add_group_role(&ep, "group2", &[]);
    let t = Tw { db2: w.db, w };
    let m = t.member("fx");
    let team = api_team(
        &t.w.app,
        "Effects",
        t.w.project,
        "developer",
        &["plain"],
        &[m],
    )
    .await;
    let login = t.w.metadata.seed_personal_login(m, t.w.db, "fx_login");
    let rows: Vec<(String, String)> = login
        .endpoints
        .iter()
        .map(|e| (e.endpoint_id.clone(), e.role_name.clone()))
        .collect();
    assert!(!rows.is_empty(), "the login has rows on the branches");
    let mut branches = vec![BranchId::from(t.w.db), t.w.dev];
    branches.sort_by_key(|b| b.as_uuid());
    let p = t.w.project;
    let tp = team_path(team);
    let steps: Vec<(&str, Method, String, Option<Value>, u16)> = vec![
        (
            "put grant",
            Method::PUT,
            format!("{tp}/grants/{p}"),
            Some(json!({ "platform_role": "developer", "pg_roles": ["plain", "group2"] })),
            200,
        ),
        (
            "remove member",
            Method::DELETE,
            format!("{tp}/members/{m}"),
            None,
            204,
        ),
        (
            "add member",
            Method::PUT,
            format!("{tp}/members/{m}"),
            None,
            200,
        ),
        (
            "delete grant",
            Method::DELETE,
            format!("{tp}/grants/{p}"),
            None,
            204,
        ),
        (
            "put grant again",
            Method::PUT,
            format!("{tp}/grants/{p}"),
            Some(json!({ "platform_role": "viewer", "pg_roles": [] })),
            200,
        ),
        ("delete team", Method::DELETE, tp.clone(), None, 204),
    ];
    for (what, method, path, body, want) in steps {
        t.clear_effects();
        let (status, answer) = t.admin(method, &path, body).await;
        assert_eq!(status, want, "{what}: {answer}");
        assert_eq!(
            t.enqueued(),
            branches,
            "{what}: every branch of the project reconciles"
        );
        let rt = t.w.app.rt_events.clone();
        assert!(
            eventually(|| rt
                .access()
                .iter()
                .any(|e| e.member_sub.as_deref() == Some("fx")
                    && e.tenant_id == t.w.tenant.as_uuid()))
            .await,
            "{what}: realtime re-checks the member: {:?}",
            rt.all()
        );
        assert!(
            t.w.app.cache.wait_for(&rows).await,
            "{what}: the proxies forget the login's rows: {:?}",
            t.w.app.cache.sent()
        );
    }
    // MH-9: a team write that took the member below Developer revoked the login.
    assert!(
        t.w.metadata
            .personal_login(m, t.w.db)
            .unwrap()
            .verifier
            .is_none(),
        "the login was revoked once the team gave only Viewer"
    );

    // Unchanged writes fire nothing. The steps above and the new team (which gives the member
    // their access back) publish from spawned tasks, so wait until they've all landed: one that's
    // late on a busy machine would be counted against an unchanged write.
    t.clear_effects();
    let team = api_team(&t.w.app, "Quiet", p, "developer", &["plain"], &[m]).await;
    let tp = team_path(team);
    t.quiesce().await;
    for (what, method, path, body) in [
        (
            "same grant",
            Method::PUT,
            format!("{tp}/grants/{p}"),
            Some(json!({ "platform_role": "developer", "pg_roles": ["plain"] })),
        ),
        (
            "member already in",
            Method::PUT,
            format!("{tp}/members/{m}"),
            None,
        ),
    ] {
        t.clear_effects();
        let events = t.actions().len();
        let (status, answer) = t.admin(method, &path, body).await;
        assert_eq!(status, 200, "{what}: {answer}");
        settle().await;
        assert!(t.enqueued().is_empty(), "{what}: nothing reconciles");
        assert!(
            t.w.app.rt_events.access().is_empty(),
            "{what}: no realtime event"
        );
        assert!(t.w.app.cache.sent().is_empty(), "{what}: no cache publish");
        assert_eq!(t.actions().len(), events, "{what}: no event");
    }
    // A grant deleted that was never there, a member removed who isn't in: 204, nothing fires.
    let other = t.member("other");
    for (what, path) in [
        (
            "absent grant",
            format!(
                "{tp}/grants/{}",
                t.w.metadata
                    .create_project(t.w.tenant, "Spare")
                    .await
                    .unwrap()
                    .id
            ),
        ),
        ("absent member", format!("{tp}/members/{other}")),
    ] {
        t.clear_effects();
        let (status, answer) = t.admin(Method::DELETE, &path, None).await;
        assert_eq!(status, 204, "{what}: {answer}");
        settle().await;
        assert!(
            t.enqueued().is_empty() && t.w.app.rt_events.access().is_empty(),
            "{what}: nothing fires"
        );
    }
}

// I4: a team write whose check read changes before the write is checked again; a second change
// is a 409 with nothing written.
#[tokio::test]
async fn a_changed_team_write_is_retried_then_conflict() {
    let t = tw().await;
    let m =
        t.w.metadata
            .add_granted(t.w.tenant, "m", t.w.project, PlatformRole::Viewer, &[]);
    let team = api_team(&t.w.app, "Racy", t.w.project, "developer", &["plain"], &[m]).await;
    let project = t.w.project;
    for roles in [&["plain"][..], &["group2"][..]] {
        let roles: Vec<&'static str> = roles.to_vec();
        t.w.metadata
            .race_once(move |md| md.grant(m, project, PlatformRole::Viewer, &roles));
    }
    let grant = format!("{}/grants/{}", team_path(team), t.w.project);
    let (status, err) = t
        .admin(
            Method::PUT,
            &grant,
            Some(json!({ "platform_role": "developer", "pg_roles": ["group2"] })),
        )
        .await;
    assert_eq!((status, code(&err)), (409, "conflict"), "{err}");
    assert_eq!(
        t.team_grant_of(team).unwrap().pg_roles,
        vec!["plain".to_string()],
        "nothing written"
    );
    // One change in between: checked again on the new state, then written.
    t.w.metadata
        .race_once(move |md| md.grant(m, project, PlatformRole::Viewer, &[]));
    let (status, body) = t
        .admin(
            Method::PUT,
            &grant,
            Some(json!({ "platform_role": "developer", "pg_roles": ["group2"] })),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        t.team_grant_of(team).unwrap().pg_roles,
        vec!["group2".to_string()]
    );
    // A member added in between changes the affected set: checked again, and refused when the
    // newcomer's union breaks.
    let n = t.w.metadata.add_granted(
        t.w.tenant,
        "n",
        t.w.project,
        PlatformRole::Developer,
        &["plain"],
    );
    t.w.metadata.narrow(
        n,
        t.w.db,
        OverrideRow {
            platform_role: Some(PlatformRole::Viewer),
            pg_roles: Some(vec!["login".into()]),
        },
    );
    t.w.metadata
        .race_once(move |md| md.seed_team_member(team, n));
    let (status, err) = t
        .admin(
            Method::PUT,
            &grant,
            Some(json!({ "platform_role": "admin", "pg_roles": [] })),
        )
        .await;
    assert_eq!(
        (status, err["error"]["member_id"].clone()),
        (422, json!(n)),
        "{err}"
    );
    assert_eq!(
        t.team_grant_of(team).unwrap().platform_role,
        PlatformRole::Developer,
        "nothing written"
    );
}

// --- Answers (P2, P17) --------------------------------------------------------------------------

// P2: a name is trimmed, 1–80 characters, unique per tenant ignoring case (409 `name_taken`); a
// description at most 500 characters. Out of bounds is 422, checked before the store.
#[tokio::test]
async fn team_name_taken_is_409_and_bounds_are_422() {
    let t = tw().await;
    let (status, body) = t
        .admin(
            Method::POST,
            "/v1/teams",
            Some(json!({ "name": "  Billing  " })),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["name"], "Billing", "trimmed: {body}");
    let first: uuid::Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let (status, err) = t
        .admin(
            Method::POST,
            "/v1/teams",
            Some(json!({ "name": "billing" })),
        )
        .await;
    assert_eq!((status, code(&err)), (409, "name_taken"), "{err}");
    let (status, body) = t
        .admin(Method::POST, "/v1/teams", Some(json!({ "name": "Ops" })))
        .await;
    assert_eq!(status, 201, "{body}");
    let ops: uuid::Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let (status, err) = t
        .admin(
            Method::PATCH,
            &team_path(ops),
            Some(json!({ "name": "BILLING" })),
        )
        .await;
    assert_eq!((status, code(&err)), (409, "name_taken"), "{err}");
    // Another tenant may have a team of the same name.
    let other = TenantId::new();
    t.w.metadata.seed_team(other, "Ops2");
    let (status, _) = t
        .admin(Method::POST, "/v1/teams", Some(json!({ "name": "Ops2" })))
        .await;
    assert_eq!(status, 201);
    // A team renamed to its own name in another case is fine.
    let (status, body) = t
        .admin(
            Method::PATCH,
            &team_path(first),
            Some(json!({ "name": "BILLING" })),
        )
        .await;
    assert_eq!(
        (status, body["name"].as_str()),
        (200, Some("BILLING")),
        "{body}"
    );

    let events = t.actions().len();
    let long = "x".repeat(81);
    let long_description = "d".repeat(501);
    for body in [
        json!({ "name": "" }),
        json!({ "name": "   " }),
        json!({ "name": long }),
        json!({ "name": "Fine", "description": long_description }),
    ] {
        let (status, err) = t.admin(Method::POST, "/v1/teams", Some(body.clone())).await;
        assert_eq!(
            (status, code(&err)),
            (422, "invalid_request"),
            "{body}: {err}"
        );
    }
    for body in [
        json!({ "name": "" }),
        json!({ "name": long }),
        json!({ "description": long_description }),
    ] {
        let (status, err) = t
            .admin(Method::PATCH, &team_path(ops), Some(body.clone()))
            .await;
        assert_eq!(
            (status, code(&err)),
            (422, "invalid_request"),
            "{body}: {err}"
        );
    }
    assert_eq!(t.actions().len(), events, "no refused write logged");
    // The bounds themselves are fine.
    let (status, body) = t
        .admin(
            Method::POST,
            "/v1/teams",
            Some(json!({ "name": "y".repeat(80), "description": "e".repeat(500) })),
        )
        .await;
    assert_eq!(status, 201, "{body}");
}

// P17: a sign-up waiting for approval can't join a team (409 `conflict`, "approve it first").
#[tokio::test]
async fn a_pending_member_is_409() {
    let t = tw().await;
    let team = api_team(&t.w.app, "Ops", t.w.project, "viewer", &[], &[]).await;
    let pat =
        t.w.metadata
            .add_member(t.w.tenant, "pat", MemberStatus::Pending, false);
    let (status, err) = t
        .admin(
            Method::PUT,
            &format!("{}/members/{pat}", team_path(team)),
            None,
        )
        .await;
    assert_eq!((status, code(&err)), (409, "conflict"), "{err}");
    assert!(
        err["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("approve"),
        "{err}"
    );
    assert!(t.team_members_of(team).is_empty());
}

// P17: an unknown team, project or member, or another tenant's, is 404, with the thing named.
#[tokio::test]
async fn unknown_team_project_or_member_is_404() {
    let t = tw().await;
    let team = api_team(&t.w.app, "Ops", t.w.project, "viewer", &[], &[]).await;
    let other = TenantId::new();
    let foreign_team = t.w.metadata.seed_team(other, "Theirs");
    let foreign_project =
        t.w.metadata
            .create_project(other, "Theirs")
            .await
            .unwrap()
            .id;
    let foreign_member =
        t.w.metadata
            .add_member(other, "stranger", MemberStatus::Active, false);
    let removed = t.member("gone");
    t.w.metadata
        .set_member(t.w.tenant, "gone", |m| m.status = MemberStatus::Removed);
    let m = t.member("m");
    let p = t.w.project;
    for unknown in [uuid::Uuid::new_v4(), foreign_team] {
        let tp = team_path(unknown);
        for (method, path, body) in [
            (Method::GET, tp.clone(), None),
            (Method::PATCH, tp.clone(), Some(json!({ "name": "x" }))),
            (Method::DELETE, tp.clone(), None),
            (
                Method::PUT,
                format!("{tp}/grants/{p}"),
                Some(json!({ "platform_role": "viewer", "pg_roles": [] })),
            ),
            (Method::DELETE, format!("{tp}/grants/{p}"), None),
            (Method::PUT, format!("{tp}/members/{m}"), None),
            (Method::DELETE, format!("{tp}/members/{m}"), None),
        ] {
            let (status, err) = t.admin(method.clone(), &path, body).await;
            assert_eq!(
                (status, code(&err), err["error"]["message"].as_str()),
                (404, "not_found", Some("team not found")),
                "{method} {path}: {err}"
            );
        }
    }
    let tp = team_path(team);
    for project in [ProjectId::new(), foreign_project] {
        let (status, err) = t
            .admin(
                Method::PUT,
                &format!("{tp}/grants/{project}"),
                Some(json!({ "platform_role": "viewer", "pg_roles": [] })),
            )
            .await;
        assert_eq!(
            (status, err["error"]["message"].as_str()),
            (404, Some("project not found")),
            "{err}"
        );
    }
    for member in [uuid::Uuid::new_v4(), foreign_member, removed] {
        let (status, err) = t
            .admin(Method::PUT, &format!("{tp}/members/{member}"), None)
            .await;
        assert_eq!(
            (status, err["error"]["message"].as_str()),
            (404, Some("member not found")),
            "{err}"
        );
    }
    let (status, err) = t
        .admin(
            Method::GET,
            &format!("/v1/members/{foreign_member}/teams"),
            None,
        )
        .await;
    assert_eq!(
        (status, err["error"]["message"].as_str()),
        (404, Some("member not found")),
        "{err}"
    );
}

// P17's answers: create 201 `Team`; update 200; put grant 200 with the team; member put 200
// (also when already in); the deletes 204 (also when there was nothing). An Admin team grant
// names no roles (R17). An empty team is a team.
#[tokio::test]
async fn team_write_answers_are_pinned() {
    let t = tw().await;
    let m = t.member("m");
    let p = t.w.project;
    let (status, team) = t
        .admin(Method::POST, "/v1/teams", Some(json!({ "name": "Ops" })))
        .await;
    assert_eq!(status, 201, "{team}");
    assert_eq!(
        (
            team["name"].as_str(),
            team["description"].as_str(),
            team["member_count"].as_u64(),
            team["member_ids"].clone(),
            team["projects"].clone()
        ),
        (Some("Ops"), Some(""), Some(0), json!([]), json!([])),
        "an empty team: {team}"
    );
    assert!(
        team["created_at"].is_string() && team["updated_at"].is_string(),
        "{team}"
    );
    let id: uuid::Uuid = team["id"].as_str().unwrap().parse().unwrap();
    let tp = team_path(id);
    let (status, team) = t
        .admin(
            Method::PATCH,
            &tp,
            Some(json!({ "description": "On call" })),
        )
        .await;
    assert_eq!(
        (status, team["name"].as_str(), team["description"].as_str()),
        (200, Some("Ops"), Some("On call")),
        "{team}"
    );
    let (status, team) = t
        .admin(
            Method::PUT,
            &format!("{tp}/grants/{p}"),
            Some(json!({ "platform_role": "developer", "pg_roles": ["plain", "group2", "plain"] })),
        )
        .await;
    assert_eq!(status, 200, "{team}");
    assert_eq!(
        team["projects"],
        json!([{ "project_id": p, "platform_role": "developer", "pg_roles": ["group2", "plain"] }]),
        "{team}"
    );
    let (status, team) = t
        .admin(
            Method::PUT,
            &format!("{tp}/grants/{p}"),
            Some(json!({ "platform_role": "admin", "pg_roles": ["plain"] })),
        )
        .await;
    assert_eq!(status, 200, "{team}");
    assert_eq!(
        team["projects"][0]["pg_roles"],
        json!([]),
        "R17: an Admin team grant names no roles: {team}"
    );
    for _ in 0..2 {
        let (status, team) = t
            .admin(Method::PUT, &format!("{tp}/members/{m}"), None)
            .await;
        assert_eq!(
            (
                status,
                team["member_ids"].clone(),
                team["member_count"].as_u64()
            ),
            (200, json!([m]), Some(1)),
            "{team}"
        );
    }
    assert_eq!(
        t.actions()
            .iter()
            .filter(|a| *a == "team_member_added")
            .count(),
        1,
        "one event for one change"
    );
    for path in [format!("{tp}/members/{m}"), format!("{tp}/grants/{p}")] {
        for _ in 0..2 {
            let (status, body) = t.admin(Method::DELETE, &path, None).await;
            assert_eq!(status, 204, "DELETE {path}: {body}");
        }
    }
    let (status, _) = t.admin(Method::DELETE, &tp, None).await;
    assert_eq!(status, 204);
    let (status, _) = t.admin(Method::GET, &tp, None).await;
    assert_eq!(status, 404);
}

// Deleting a team with grants and members takes their access through it away (it was their only
// source), with one event naming them.
#[tokio::test]
async fn deleting_a_team_with_grants_and_members_takes_its_access_away() {
    let t = tw().await;
    let m = t.member("m");
    let team = api_team(
        &t.w.app,
        "Only source",
        t.w.project,
        "developer",
        &["plain"],
        &[m],
    )
    .await;
    let db = format!("/v1/databases/{}", t.w.db);
    let (status, body) = t.send("m", Method::GET, &db, None).await;
    assert_eq!(status, 200, "the team gives access: {body}");
    let (status, _) = t.admin(Method::DELETE, &team_path(team), None).await;
    assert_eq!(status, 204);
    let (status, body) = t.send("m", Method::GET, &db, None).await;
    assert_ne!(status, 200, "no access left: {body}");
    let deleted: Vec<_> =
        t.w.metadata
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.action == "team_deleted")
            .cloned()
            .collect();
    assert_eq!(deleted.len(), 1);
    assert_eq!(
        deleted[0].before.as_ref().unwrap()["affected_member_ids"],
        json!([m.to_string()])
    );
}

// --- The log (P9, MH-7) -------------------------------------------------------------------------

// Team events carry the team, the grants they concern and the members they reach; the project's
// log shows the team's grant events there, without who else the team reaches (P9).
#[tokio::test]
async fn team_events_are_logged_and_project_logs_hide_who_they_reach() {
    let t = tw().await;
    let (m1, m2) = (t.member("m1"), t.member("m2"));
    let team = api_team(
        &t.w.app,
        "Logged",
        t.w.project,
        "developer",
        &["plain"],
        &[m1, m2],
    )
    .await;
    let events = t.w.metadata.events.lock().unwrap().clone();
    let actions: Vec<&str> = events.iter().map(|e| e.action.as_str()).collect();
    assert_eq!(
        actions,
        vec![
            "team_created",
            "team_grant_put",
            "team_member_added",
            "team_member_added"
        ]
    );
    let put = &events[1];
    assert_eq!(
        (put.project_id, put.target_member_id),
        (Some(t.w.project.as_uuid()), None)
    );
    let mut reached = vec![];
    let after = put.after.as_ref().unwrap();
    assert_eq!(after["team"], json!({ "id": team, "name": "Logged" }));
    assert_eq!(
        after["grants"],
        json!([{ "project_id": t.w.project, "platform_role": "developer", "pg_roles": ["plain"] }])
    );
    assert_eq!(
        after["affected_member_ids"],
        json!(reached),
        "no member yet when the grant was put"
    );
    assert_eq!(events[2].target_member_id, Some(m1));
    reached = vec![m1.to_string()];
    assert_eq!(
        events[2].after.as_ref().unwrap()["affected_member_ids"],
        json!(reached)
    );

    // A second grant put now reaches both, and the project's log shows it without them.
    let (status, _) = t
        .admin(
            Method::PUT,
            &format!("{}/grants/{}", team_path(team), t.w.project),
            Some(json!({ "platform_role": "viewer", "pg_roles": [] })),
        )
        .await;
    assert_eq!(status, 200);
    let (status, page) = t
        .admin(
            Method::GET,
            &format!("/v1/projects/{}/access-events", t.w.project),
            None,
        )
        .await;
    assert_eq!(status, 200, "{page}");
    let latest = &page["events"][0];
    assert_eq!(latest["action"], "team_grant_put", "{page}");
    assert_eq!(
        latest["after"]["affected_member_ids"],
        json!([]),
        "a project log hides who the team reaches: {page}"
    );
    assert_eq!(latest["before"]["affected_member_ids"], json!([]), "{page}");
    // The tenant's log keeps them.
    let (_, page) = t.admin(Method::GET, "/v1/access-events", None).await;
    let mut both = vec![m1.to_string(), m2.to_string()];
    both.sort();
    assert_eq!(
        page["events"][0]["after"]["affected_member_ids"],
        json!(both),
        "{page}"
    );
}

// R17 race, the team side of demotion: a team write's check skips a tenant admin (it read the
// flag before the lock); a demotion (clean when checked: the team grants Viewer {}) commits
// before the team write compares. The write sees the flag changed (`Changed`), is checked
// again with the member no longer an admin, and is refused naming `login`; nothing is written.
#[tokio::test]
async fn a_team_write_that_skipped_a_demoted_admin_is_checked_again() {
    let t = tw().await;
    let ta = t.w.metadata.add_member(t.w.tenant, "ta", MemberStatus::Active, true);
    let (status, body) = t.admin(Method::POST, "/v1/teams", Some(json!({ "name": "Viewers" }))).await;
    assert_eq!(status, 201, "{body}");
    let team: uuid::Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let grant_path = format!("/v1/teams/{team}/grants/{}", t.w.project);
    let (status, body) = t.admin(Method::PUT, &grant_path, Some(json!({ "platform_role": "viewer", "pg_roles": [] }))).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = t.admin(Method::PUT, &format!("/v1/teams/{team}/members/{ta}"), None).await;
    assert!(status < 300, "{status} {body}");

    t.w.metadata.race_once(move |md| {
        md.members.lock().unwrap().iter_mut().find(|m| m.id == ta).unwrap().tenant_admin = false;
    });
    let (status, err) =
        t.admin(Method::PUT, &grant_path, Some(json!({ "platform_role": "viewer", "pg_roles": ["login"] }))).await;
    assert_eq!((status, err["error"]["role"].as_str()), (422, Some("login")), "{err}");
    assert_eq!(err["error"]["team_id"], json!(team), "{err}");
    assert_eq!(err["error"]["member_id"], json!(ta), "{err}");
    assert!(t.w.metadata.race_before_write.lock().unwrap().is_empty(), "the race ran");
    let stored = t.w.metadata.teams.lock().unwrap().iter().find(|x| x.record.id == team).unwrap().grants.clone();
    assert_eq!(
        stored.get(&t.w.project.as_uuid()).map(|g| g.pg_roles.clone()),
        Some(Vec::<String>::new()),
        "the team grant is unchanged"
    );
}

// --- The invariant (MH-4) -----------------------------------------------------------------------

// After any sequence of own-grant, override and team writes, every member's effective access on
// every database (their own grant ∪ their teams' grants, narrowed by their override there)
// holds below Admin only roles the pool allows there. A refused write writes nothing.
mod team_invariant {
    use super::*;
    use proptest::prelude::*;

    #[derive(Clone, Debug)]
    enum Op {
        PutOwn {
            member: usize,
            role: PlatformRole,
            roles: Vec<&'static str>,
        },
        DeleteOwn {
            member: usize,
        },
        PutOverride {
            member: usize,
            first: bool,
            role: Option<PlatformRole>,
            roles: Option<Vec<&'static str>>,
        },
        DeleteOverride {
            member: usize,
            first: bool,
        },
        PutTeamGrant {
            team: usize,
            role: PlatformRole,
            roles: Vec<&'static str>,
        },
        DeleteTeamGrant {
            team: usize,
        },
        AddMember {
            team: usize,
            member: usize,
        },
        RemoveMember {
            team: usize,
            member: usize,
        },
        Promote {
            member: usize,
        },
        Demote {
            member: usize,
        },
    }

    /// One step: a write, and maybe a racing write whose check read the same state and which
    /// commits just before the step's write compares (through `race_once`).
    type Step = (Op, Option<Op>);

    /// Racers lean to demotions, the write a check that skipped tenant admins must see.
    fn step() -> impl Strategy<Value = Step> {
        let racer = prop_oneof![1 => (0..2usize).prop_map(|member| Op::Demote { member }), 1 => op()];
        (op(), proptest::option::weighted(0.6, racer))
    }

    /// The start: per member, whether they begin as a tenant admin and in the first team.
    fn start() -> impl Strategy<Value = [(bool, bool); 2]> {
        any::<[(bool, bool); 2]>()
    }

    fn role() -> impl Strategy<Value = PlatformRole> {
        prop_oneof![
            Just(PlatformRole::Viewer),
            Just(PlatformRole::Developer),
            Just(PlatformRole::Admin)
        ]
    }

    fn roles() -> impl Strategy<Value = Vec<&'static str>> {
        proptest::sample::subsequence(POOL.to_vec(), 0..=POOL.len())
    }

    fn op() -> impl Strategy<Value = Op> {
        let member = 0..2usize;
        let team = 0..2usize;
        prop_oneof![
            (member.clone(), role(), roles()).prop_map(|(member, role, roles)| Op::PutOwn {
                member,
                role,
                roles
            }),
            member.clone().prop_map(|member| Op::DeleteOwn { member }),
            (
                member.clone(),
                any::<bool>(),
                proptest::option::of(role()),
                proptest::option::of(roles())
            )
                .prop_map(|(member, first, role, roles)| Op::PutOverride {
                    member,
                    first,
                    role,
                    roles
                }),
            (member.clone(), any::<bool>())
                .prop_map(|(member, first)| Op::DeleteOverride { member, first }),
            (team.clone(), role(), roles()).prop_map(|(team, role, roles)| Op::PutTeamGrant {
                team,
                role,
                roles
            }),
            team.clone().prop_map(|team| Op::DeleteTeamGrant { team }),
            (team.clone(), member.clone())
                .prop_map(|(team, member)| Op::AddMember { team, member }),
            (team, member.clone()).prop_map(|(team, member)| Op::RemoveMember { team, member }),
            member.clone().prop_map(|member| Op::Promote { member }),
            member.prop_map(|member| Op::Demote { member }),
        ]
    }

    fn is_admin(t: &Tw, member: uuid::Uuid) -> bool {
        t.w.metadata.members.lock().unwrap().iter().any(|m| m.id == member && m.tenant_admin)
    }

    /// The fake's access state, saved so a racing write's result can be committed later.
    #[derive(Clone)]
    struct Saved {
        members: Vec<MemberRecord>,
        grants: HashMap<(uuid::Uuid, uuid::Uuid), GrantRow>,
        overrides: HashMap<OverrideKey, OverrideRow>,
        teams: Vec<FakeTeam>,
        events: Vec<chelabase_metadata::members::AccessEvent>,
    }

    fn save(md: &FakeMetadata) -> Saved {
        Saved {
            members: md.members.lock().unwrap().clone(),
            grants: md.grants.lock().unwrap().clone(),
            overrides: md.overrides.lock().unwrap().clone(),
            teams: md.teams.lock().unwrap().clone(),
            events: md.events.lock().unwrap().clone(),
        }
    }

    fn load(md: &FakeMetadata, s: &Saved) {
        *md.members.lock().unwrap() = s.members.clone();
        *md.grants.lock().unwrap() = s.grants.clone();
        *md.overrides.lock().unwrap() = s.overrides.clone();
        *md.teams.lock().unwrap() = s.teams.clone();
        *md.events.lock().unwrap() = s.events.clone();
    }

    /// Every (member, database, role) held below Admin that the pool refuses there.
    fn violations(t: &Tw, members: &[uuid::Uuid]) -> Vec<String> {
        let mut out = vec![];
        let project = t.w.project.as_uuid();
        // R17: a tenant admin is Admin on every project, whatever its sources say.
        for m in members.iter().filter(|m| !is_admin(t, **m)) {
            for (db, first) in [(t.w.db, true), (t.db2, false)] {
                let own =
                    t.w.metadata
                        .grants
                        .lock()
                        .unwrap()
                        .get(&(*m, project))
                        .map(GrantRow::to_grant);
                let teams: Vec<Grant> =
                    t.w.metadata
                        .stored_team_grants(*m, project)
                        .values()
                        .map(GrantRow::to_grant)
                        .collect();
                let narrowing =
                    t.w.metadata
                        .overrides
                        .lock()
                        .unwrap()
                        .get(&(*m, db.as_uuid()))
                        .map(OverrideRow::to_narrowing);
                let Some(e) = GrantSources::new(own, teams, narrowing).effective() else {
                    continue;
                };
                if e.platform_role == PlatformRole::Admin {
                    continue;
                }
                if let RoleSet::Only(set) = &e.roles {
                    for r in set {
                        if let PgRole::Named(name) = r
                            && pool_bad(first, name)
                        {
                            out.push(format!(
                                "member {m} on {}: {:?} holds {name}",
                                if first { "web1" } else { "web2" },
                                e.platform_role
                            ));
                        }
                    }
                }
            }
        }
        out
    }

    type Snapshot = (
        Vec<(uuid::Uuid, uuid::Uuid, GrantRow)>,
        Vec<(uuid::Uuid, uuid::Uuid, OverrideRow)>,
        Vec<(uuid::Uuid, Vec<(uuid::Uuid, GrantRow)>, Vec<uuid::Uuid>)>,
        usize,
        Vec<(uuid::Uuid, bool)>,
    );

    fn snapshot(t: &Tw) -> Snapshot {
        let mut grants: Vec<_> =
            t.w.metadata
                .grants
                .lock()
                .unwrap()
                .iter()
                .map(|((m, p), g)| (*m, *p, g.clone()))
                .collect();
        grants.sort_by_key(|(m, p, _)| (*m, *p));
        let mut overrides: Vec<_> =
            t.w.metadata
                .overrides
                .lock()
                .unwrap()
                .iter()
                .map(|((m, d), o)| (*m, *d, o.clone()))
                .collect();
        overrides.sort_by_key(|(m, d, _)| (*m, *d));
        let mut teams: Vec<_> =
            t.w.metadata
                .teams
                .lock()
                .unwrap()
                .iter()
                .map(|t| {
                    (
                        t.record.id,
                        t.grants.iter().map(|(p, g)| (*p, g.clone())).collect(),
                        t.members.iter().copied().collect(),
                    )
                })
                .collect();
        teams.sort_by_key(|(id, _, _)| *id);
        let mut admins: Vec<_> =
            t.w.metadata.members.lock().unwrap().iter().map(|m| (m.id, m.tenant_admin)).collect();
        admins.sort();
        (
            grants,
            overrides,
            teams,
            t.w.metadata.events.lock().unwrap().len(),
            admins,
        )
    }

    async fn run(start: [(bool, bool); 2], ops: Vec<Step>) -> Result<(), TestCaseError> {
        let t = tw().await;
        let members: Vec<uuid::Uuid> = (0..2).map(|i| t.member(&format!("m{i}"))).collect();
        let mut teams = vec![];
        for i in 0..2 {
            let (status, body) = t
                .admin(
                    Method::POST,
                    "/v1/teams",
                    Some(json!({ "name": format!("team{i}") })),
                )
                .await;
            prop_assert_eq!(status, 201, "setup: POST /v1/teams: {}", body);
            teams.push(body["id"].as_str().unwrap().parse::<uuid::Uuid>().unwrap());
        }
        let md = t.w.metadata.clone();
        for (i, (admin, in_first)) in start.iter().enumerate() {
            if *admin {
                md.members.lock().unwrap().iter_mut().find(|m| m.id == members[i]).unwrap().tenant_admin = true;
            }
            if *in_first {
                md.teams.lock().unwrap().iter_mut().find(|x| x.record.id == teams[0]).unwrap().members.insert(members[i]);
            }
        }
        for (step, (op, racer)) in ops.into_iter().enumerate() {
            // The racer's check reads the same state as the step's: it runs first, its result
            // is saved and undone, and `race_once` commits it just before the step's write
            // compares (a write that never reaches a checked compare leaves it pending: the
            // racer then runs after the step, through the API).
            let mut raced = None;
            if let Some(r) = &racer {
                let pre = save(&md);
                let (method, path, body) = request(&t, &members, &teams, r);
                let (status, answer) = t.admin(method, &path, body).await;
                prop_assert!(status < 500, "step {step} racer {r:?}: {status} {answer}");
                if status < 400 {
                    let after = save(&md);
                    raced = Some(snapshot(&t));
                    load(&md, &pre);
                    md.race_once(move |md| load(md, &after));
                }
            }
            let before = snapshot(&t);
            let (method, path, body) = request(&t, &members, &teams, &op);
            let (status, answer) = t.admin(method, &path, body).await;
            prop_assert!(status < 500, "step {step} {op:?} (racer {racer:?}): {status} {answer}");
            prop_assert!(
                status != 404 || !path.starts_with("/v1/teams/"),
                "step {step} {op:?}: the team routes answer: {answer}"
            );
            if status >= 400 {
                let now = snapshot(&t);
                prop_assert!(
                    now == before || raced.as_ref() == Some(&now),
                    "step {} {:?} (racer {:?}): a refusal wrote",
                    step,
                    op,
                    racer
                );
                if !answer["error"]["chain"].is_null() {
                    prop_assert!(
                        !answer["error"]["database_id"].is_null(),
                        "step {step} {op:?}: {answer}"
                    );
                }
            }
            let pending = md.race_before_write.lock().unwrap().pop_front();
            if pending.is_some()
                && let Some(r) = &racer
            {
                let (method, path, body) = request(&t, &members, &teams, r);
                let (status, answer) = t.admin(method, &path, body).await;
                prop_assert!(status < 500, "step {step} racer {r:?} after: {status} {answer}");
            }
            let broken = violations(&t, &members);
            prop_assert!(
                broken.is_empty(),
                "step {step} {op:?} (racer {racer:?}) -> {status}: {broken:?}"
            );
        }
        Ok(())
    }

    /// The API request for `op`.
    fn request(t: &Tw, members: &[uuid::Uuid], teams: &[uuid::Uuid], op: &Op) -> (Method, String, Option<Value>) {
        let p = t.w.project;
        let db = |first: bool| if first { t.w.db } else { t.db2 };
        match op {
                Op::PutOwn {
                    member,
                    role,
                    roles,
                } => (
                    Method::PUT,
                    format!("/v1/projects/{p}/grants/{}", members[*member]),
                    Some(json!({ "platform_role": role, "pg_roles": roles })),
                ),
                Op::DeleteOwn { member } => (
                    Method::DELETE,
                    format!("/v1/projects/{p}/grants/{}", members[*member]),
                    None,
                ),
                Op::PutOverride {
                    member,
                    first,
                    role,
                    roles,
                } => (
                    Method::PUT,
                    format!("/v1/projects/{p}/grants/{}/overrides", members[*member]),
                    Some(
                        json!({ "database_id": db(*first), "platform_role": role, "pg_roles": roles }),
                    ),
                ),
                Op::DeleteOverride { member, first } => (
                    Method::DELETE,
                    format!(
                        "/v1/projects/{p}/grants/{}/overrides?database_id={}",
                        members[*member],
                        db(*first)
                    ),
                    None,
                ),
                Op::PutTeamGrant { team, role, roles } => (
                    Method::PUT,
                    format!("/v1/teams/{}/grants/{p}", teams[*team]),
                    Some(json!({ "platform_role": role, "pg_roles": roles })),
                ),
                Op::DeleteTeamGrant { team } => (
                    Method::DELETE,
                    format!("/v1/teams/{}/grants/{p}", teams[*team]),
                    None,
                ),
                Op::AddMember { team, member } => (
                    Method::PUT,
                    format!("/v1/teams/{}/members/{}", teams[*team], members[*member]),
                    None,
                ),
                Op::RemoveMember { team, member } => (
                    Method::DELETE,
                    format!("/v1/teams/{}/members/{}", teams[*team], members[*member]),
                    None,
                ),
                Op::Promote { member } | Op::Demote { member } => (
                    Method::PATCH,
                    format!("/v1/members/{}", members[*member]),
                    Some(json!({ "tenant_admin": matches!(op, Op::Promote { .. }) })),
                ),
            }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]
        #[test]
        fn every_write_sequence_keeps_the_invariant(start in start(), ops in proptest::collection::vec(step(), 1..20)) {
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            rt.block_on(run(start, ops))?;
        }
    }
}
