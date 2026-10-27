//! Members hub step 2 (T23, coder's edges beyond the RED set): removals checked on the union,
//! overrides on a team-only member, a deleting project, unchanged renames; the T23 audit's two
//! races (a grant delete and a team-member add, each racing a team grant change).
#![allow(dead_code, unused_imports)]

include!("common/fakes.rs");

/// The member world with `plain` (a NOLOGIN group) and `boss` (reaches the owner) on web1's main.
async fn world() -> MemberWorld {
    let w = member_world(Mode::SelfHosted).await;
    let ep = w.metadata.branch(BranchId::from(w.db)).endpoint_id.unwrap();
    w.metadata.add_group_role(&ep, "plain", &[]);
    w.metadata.add_group_role(&ep, "boss", &["postgres"]);
    w
}

async fn admin(w: &MemberWorld, method: Method, path: &str, body: Option<Value>) -> (u16, Value) {
    let resp = w.app.send_as(DEFAULT_SUB, "org-1", method, path, body).await;
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

// MH-4 on removals: own Developer {boss} was safe while a team made the member Admin; taking the
// team's grant away, removing the member from the team or deleting the team would leave `boss`
// below Admin. Each is refused naming the member and the team; nothing is written.
#[tokio::test]
async fn taking_a_team_admin_source_away_is_checked_on_the_union() {
    let w = world().await;
    let m = w.metadata.add_member(w.tenant, "m", MemberStatus::Active, false);
    let team = api_team(&w.app, "Admins", w.project, "admin", &[], &[m]).await;
    // Written while Admin through the team: nothing below Admin to check then.
    let (status, body) = admin(
        &w,
        Method::PUT,
        &format!("/v1/projects/{}/grants/{m}", w.project),
        Some(json!({ "platform_role": "developer", "pg_roles": ["boss"] })),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let events = w.metadata.events.lock().unwrap().len();
    for (method, path) in [
        (Method::DELETE, format!("/v1/teams/{team}/grants/{}", w.project)),
        (Method::DELETE, format!("/v1/teams/{team}/members/{m}")),
        (Method::DELETE, format!("/v1/teams/{team}")),
    ] {
        let (status, err) = admin(&w, method.clone(), &path, None).await;
        assert_eq!((status, err["error"]["role"].as_str()), (422, Some("boss")), "{method} {path}: {err}");
        assert_eq!((err["error"]["member_id"].clone(), err["error"]["team_id"].clone()), (json!(m), json!(team)), "{err}");
    }
    assert_eq!(w.metadata.events.lock().unwrap().len(), events, "nothing written");
    assert!(w.metadata.stored_team(team).unwrap().members.contains(&m));
}

// An override narrows the union: a team-only member can be narrowed, and the narrowing can't
// widen what the teams give (role or names).
#[tokio::test]
async fn an_override_narrows_a_team_only_member() {
    let w = world().await;
    let m = w.metadata.add_member(w.tenant, "m", MemberStatus::Active, false);
    api_team(&w.app, "Devs", w.project, "developer", &["plain"], &[m]).await;
    let path = format!("/v1/projects/{}/grants/{m}/overrides", w.project);
    let (status, body) = admin(&w, Method::PUT, &path, Some(json!({ "database_id": w.db, "platform_role": "viewer" }))).await;
    assert_eq!(status, 200, "{body}");
    let (status, err) = admin(&w, Method::PUT, &path, Some(json!({ "database_id": w.db, "platform_role": "admin" }))).await;
    assert_eq!(status, 422, "{err}");
    let (status, err) = admin(&w, Method::PUT, &path, Some(json!({ "database_id": w.db, "pg_roles": ["other"] }))).await;
    assert_eq!((status, err["error"]["role"].as_str()), (422, Some("other")), "{err}");
}

// A team grant on a project being deleted is 409 `project_deleting`, as an invite's grant is.
#[tokio::test]
async fn a_team_grant_on_a_deleting_project_is_409() {
    let w = world().await;
    let team = api_team(&w.app, "Ops", w.project, "viewer", &[], &[]).await;
    let other = w.metadata.create_project(w.tenant, "Going").await.unwrap().id;
    w.metadata.projects.lock().unwrap().iter_mut().find(|p| p.id == other).unwrap().status = ProjectStatus::Deleting;
    let (status, err) = admin(
        &w,
        Method::PUT,
        &format!("/v1/teams/{team}/grants/{other}"),
        Some(json!({ "platform_role": "viewer", "pg_roles": [] })),
    )
    .await;
    assert_eq!((status, err["error"]["code"].as_str()), (409, Some("project_deleting")), "{err}");
}

// An unchanged rename writes nothing and answers the team; an empty PATCH is unchanged too.
#[tokio::test]
async fn an_unchanged_team_update_writes_nothing() {
    let w = world().await;
    let team = api_team(&w.app, "Ops", w.project, "viewer", &[], &[]).await;
    let events = w.metadata.events.lock().unwrap().len();
    for body in [json!({ "name": "  Ops " }), json!({})] {
        let (status, answer) = admin(&w, Method::PATCH, &format!("/v1/teams/{team}"), Some(body.clone())).await;
        assert_eq!((status, answer["name"].as_str()), (200, Some("Ops")), "{body}: {answer}");
        assert_eq!(answer["projects"][0]["platform_role"], "viewer", "{answer}");
    }
    assert_eq!(w.metadata.events.lock().unwrap().len(), events, "no event");
}

// I4, MH-4 (T23 audit): own Admin made a team's Developer `boss` safe to hold; a team grant
// written between an own-grant delete's check and its write is seen by the write (`Changed`),
// checked again, and refused. Nothing is deleted.
#[tokio::test]
async fn a_grant_delete_racing_a_team_grant_is_checked_again() {
    let w = world().await;
    let m = w.metadata.add_granted(w.tenant, "m", w.project, PlatformRole::Admin, &[]);
    let team = api_team(&w.app, "Viewers", w.project, "viewer", &[], &[m]).await;
    let project = w.project;
    w.metadata.race_once(move |md| md.seed_team_grant(team, project, PlatformRole::Developer, &["boss"]));
    let (status, err) = admin(&w, Method::DELETE, &format!("/v1/projects/{}/grants/{m}", w.project), None).await;
    assert_eq!((status, err["error"]["role"].as_str()), (422, Some("boss")), "{err}");
    assert_eq!(err["error"]["database_id"], json!(w.db), "{err}");
    assert_eq!(
        w.metadata.grants.lock().unwrap().get(&(m, w.project.as_uuid())).map(|g| g.platform_role),
        Some(PlatformRole::Admin),
        "the grant stays"
    );
}

// I4, MH-4 (T23 audit): a team's grant changed (same project, other roles) between a member
// add's check and its write is seen by the write (`Changed`), checked again on the new grant,
// and refused. The member isn't added.
#[tokio::test]
async fn adding_a_member_racing_a_team_grant_change_is_checked_again() {
    let w = world().await;
    let m = w.metadata.add_member(w.tenant, "m", MemberStatus::Active, false);
    let team = api_team(&w.app, "Viewers", w.project, "viewer", &[], &[]).await;
    let project = w.project;
    w.metadata.race_once(move |md| md.seed_team_grant(team, project, PlatformRole::Developer, &["boss"]));
    let (status, err) = admin(&w, Method::PUT, &format!("/v1/teams/{team}/members/{m}"), None).await;
    assert_eq!((status, err["error"]["role"].as_str()), (422, Some("boss")), "{err}");
    assert_eq!((err["error"]["member_id"].clone(), err["error"]["team_id"].clone()), (json!(m), json!(team)), "{err}");
    assert!(!w.metadata.stored_team(team).unwrap().members.contains(&m), "not added");
}

// --- Final fix round (backend) -------------------------------------------------------------------

/// Web1's main endpoint.
fn main_endpoint(w: &MemberWorld) -> String {
    w.metadata.branch(BranchId::from(w.db)).endpoint_id.unwrap()
}

// Defect 1: a team with no members has no union to check, but its grant must still be one some
// member could hold, as the own-grant check refuses it: below Admin, `login` (a LOGIN role),
// `boss` (reaches the owner on web1) and `lead` (holds ADMIN on `plain`) are each 422 naming the
// role and the team; nothing is written. Admin holds every role (R17), and a plain group is fine.
#[tokio::test]
async fn an_empty_teams_grant_is_checked_on_its_own() {
    let w = world().await;
    let ep = main_endpoint(&w);
    w.metadata.add_role(&ep, "login", RoleOrigin::Sql, false, false);
    w.metadata.add_group_role_with(&ep, "lead", &["plain"], &[], &["plain"]);
    let (status, body) = admin(&w, Method::POST, "/v1/teams", Some(json!({ "name": "Empty" }))).await;
    assert_eq!(status, 201, "{body}");
    let team: uuid::Uuid = body["id"].as_str().unwrap().parse().unwrap();
    let grant = format!("/v1/teams/{team}/grants/{}", w.project);
    let events = w.metadata.events.lock().unwrap().len();
    for (level, role) in [("viewer", "login"), ("developer", "boss"), ("viewer", "lead")] {
        let (status, err) = admin(&w, Method::PUT, &grant, Some(json!({ "platform_role": level, "pg_roles": [role] }))).await;
        assert_eq!((status, err["error"]["role"].as_str()), (422, Some(role)), "{level} {role}: {err}");
        assert_eq!(err["error"]["team_id"], json!(team), "{level} {role}: {err}");
        assert_eq!(err["error"]["database_id"], json!(w.db), "{level} {role}: {err}");
    }
    assert_eq!(w.metadata.stored_team(team).unwrap().grants.get(&w.project.as_uuid()), None, "nothing written");
    assert_eq!(w.metadata.events.lock().unwrap().len(), events, "no event");
    for (level, roles) in [("admin", json!(["login", "boss", "lead"])), ("developer", json!(["plain"]))] {
        let (status, body) = admin(&w, Method::PUT, &grant, Some(json!({ "platform_role": level, "pg_roles": roles }))).await;
        assert_eq!(status, 200, "{level}: {body}");
    }
}

// T23 audit (e), R17: a tenant admin holds every role, so a team write is never refused because
// of a tenant admin in the team. `n` (own Admin) keeps each team non-empty, so only the tenant
// admin's union could break: a grant put, a member add and a grant delete each go through.
#[tokio::test]
async fn a_tenant_admin_in_a_team_never_refuses_its_writes() {
    let w = world().await;
    let ta = w.metadata.add_member(w.tenant, "ta", MemberStatus::Active, true);
    let n = w.metadata.add_granted(w.tenant, "n", w.project, PlatformRole::Admin, &[]);
    let team = w.metadata.seed_team(w.tenant, "Mixed");
    w.metadata.seed_team_member(team, ta);
    w.metadata.seed_team_member(team, n);
    let (status, body) = admin(
        &w,
        Method::PUT,
        &format!("/v1/teams/{team}/grants/{}", w.project),
        Some(json!({ "platform_role": "developer", "pg_roles": ["boss"] })),
    )
    .await;
    assert_eq!(status, 200, "a grant put: {body}");
    // Adding the tenant admin to a team granting `boss` at Developer.
    let bosses = w.metadata.seed_team(w.tenant, "Bosses");
    w.metadata.seed_team_grant(bosses, w.project, PlatformRole::Developer, &["boss"]);
    w.metadata.seed_team_member(bosses, n);
    let (status, body) = admin(&w, Method::PUT, &format!("/v1/teams/{bosses}/members/{ta}"), None).await;
    assert_eq!(status, 200, "a member add: {body}");
    // Taking a team's Admin grant away from a tenant admin whose own grant names `boss`.
    w.metadata.grant(ta, w.project, PlatformRole::Developer, &["boss"]);
    let admins = w.metadata.seed_team(w.tenant, "Admins");
    w.metadata.seed_team_grant(admins, w.project, PlatformRole::Admin, &[]);
    w.metadata.seed_team_member(admins, ta);
    let (status, body) = admin(&w, Method::DELETE, &format!("/v1/teams/{admins}/grants/{}", w.project), None).await;
    assert_eq!(status, 204, "a grant delete: {body}");
}

// T23 audit (f), I4: a retried team write checks the role graphs as they are at the retry, not as
// the first try read them. Between the first check and its write, the member's own grant changes
// (`Changed`) and `group2` becomes a member of the owner on web1: the retry refuses `group2`.
#[tokio::test]
async fn a_retried_team_write_reads_the_role_graphs_again() {
    let w = world().await;
    let ep = main_endpoint(&w);
    w.metadata.add_group_role(&ep, "group2", &[]);
    let m = w.metadata.add_granted(w.tenant, "m", w.project, PlatformRole::Viewer, &[]);
    let team = api_team(&w.app, "Racy", w.project, "developer", &["plain"], &[m]).await;
    let project = w.project;
    w.metadata.race_once(move |md| {
        md.grant(m, project, PlatformRole::Viewer, &["plain"]);
        let mut roles = md.roles.lock().unwrap();
        let row = roles.get_mut(&ep).unwrap().iter_mut().find(|r| r.name == "group2").unwrap();
        row.attributes.member_of = vec!["postgres".into()];
    });
    let (status, err) = admin(
        &w,
        Method::PUT,
        &format!("/v1/teams/{team}/grants/{}", w.project),
        Some(json!({ "platform_role": "developer", "pg_roles": ["group2"] })),
    )
    .await;
    assert_eq!((status, err["error"]["role"].as_str()), (422, Some("group2")), "{err}");
    assert_eq!(err["error"]["database_id"], json!(w.db), "{err}");
    assert_eq!(
        w.metadata.stored_team(team).unwrap().grants.get(&w.project.as_uuid()).map(|g| g.pg_roles.clone()),
        Some(vec!["plain".to_string()]),
        "nothing written"
    );
}
