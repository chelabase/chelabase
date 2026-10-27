//! Members hub step 2, demotion (user, 2026-10-27): demoting a tenant admin to a normal member
//! runs the union check (`check_effective`) on everything the member keeps (own grants,
//! overrides and team grants, on every project and database), under the tenant lock with the
//! checked-write re-read (P6, P25). A break is 422 naming the role, with `team_id`, `project_id`
//! and `database_id` as other union refusals; nothing is written.
#![allow(dead_code, unused_imports)]

include!("common/fakes.rs");

/// The member world with `plain` (a NOLOGIN group), `boss` (reaches the owner) and `login`
/// (a LOGIN role) on web1's main.
async fn world() -> MemberWorld {
    let w = member_world(Mode::SelfHosted).await;
    let ep = main_endpoint(&w);
    w.metadata.add_group_role(&ep, "plain", &[]);
    w.metadata.add_group_role(&ep, "boss", &["postgres"]);
    w.metadata.add_role(&ep, "login", RoleOrigin::Sql, false, false);
    w
}

/// Web1's main endpoint.
fn main_endpoint(w: &MemberWorld) -> String {
    w.metadata.branch(BranchId::from(w.db)).endpoint_id.unwrap()
}

async fn admin(w: &MemberWorld, method: Method, path: &str, body: Option<Value>) -> (u16, Value) {
    let resp = w.app.send_as(DEFAULT_SUB, "org-1", method, path, body).await;
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn demote(w: &MemberWorld, member: uuid::Uuid) -> (u16, Value) {
    admin(w, Method::PATCH, &format!("/v1/members/{member}"), Some(json!({ "tenant_admin": false }))).await
}

fn is_tenant_admin(w: &MemberWorld, member: uuid::Uuid) -> bool {
    w.metadata.members.lock().unwrap().iter().find(|m| m.id == member).unwrap().tenant_admin
}

fn admin_unset_events(w: &MemberWorld, member: uuid::Uuid) -> usize {
    w.metadata
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.action == "admin_unset" && e.target_member_id == Some(member))
        .count()
}

/// Asserts the refusal left everything as it was: still a tenant admin, no `admin_unset`
/// event, no reconcile enqueued.
fn nothing_written(w: &MemberWorld, member: uuid::Uuid) {
    assert!(is_tenant_admin(w, member), "still a tenant admin");
    assert_eq!(admin_unset_events(w, member), 0, "no admin_unset event");
    assert!(w.jobs.member_role_enqueues.lock().unwrap().is_empty(), "no reconcile");
}

/// A tenant admin `ta` (the default caller stays the other active tenant admin).
fn tenant_admin(w: &MemberWorld) -> uuid::Uuid {
    w.metadata.add_member(w.tenant, "ta", MemberStatus::Active, true)
}

// A tenant admin in a team granting Viewer {login} (written while the team-write check skipped
// tenant admins, fix (e)): demoting them would leave a LOGIN role below Admin. 422 naming
// `login`, the team, the project and the database; nothing is written.
#[tokio::test]
async fn demoting_a_tenant_admin_whose_team_grants_a_login_role_is_refused() {
    let w = world().await;
    let ta = tenant_admin(&w);
    let team = w.metadata.seed_team(w.tenant, "Logins");
    w.metadata.seed_team_grant(team, w.project, PlatformRole::Viewer, &["login"]);
    w.metadata.seed_team_member(team, ta);
    let (status, err) = demote(&w, ta).await;
    assert_eq!((status, err["error"]["role"].as_str()), (422, Some("login")), "{err}");
    assert_eq!(err["error"]["code"], "invalid_request", "{err}");
    assert!(err["error"]["message"].as_str().unwrap_or_default().contains("login"), "{err}");
    assert_eq!(err["error"]["team_id"], json!(team), "{err}");
    assert_eq!(err["error"]["project_id"], json!(w.project), "{err}");
    assert_eq!(err["error"]["database_id"], json!(w.db), "{err}");
    nothing_written(&w, ta);
}

// An own Developer grant naming `boss` (owner-level through its chain): 422 naming `boss`, the
// project and the database, and no team (the own grant is why).
#[tokio::test]
async fn demoting_a_tenant_admin_whose_own_grant_names_an_owner_level_role_is_refused() {
    let w = world().await;
    let ta = tenant_admin(&w);
    w.metadata.grant(ta, w.project, PlatformRole::Developer, &["boss"]);
    let (status, err) = demote(&w, ta).await;
    assert_eq!((status, err["error"]["role"].as_str()), (422, Some("boss")), "{err}");
    assert_eq!(err["error"]["project_id"], json!(w.project), "{err}");
    assert_eq!(err["error"]["database_id"], json!(w.db), "{err}");
    assert!(err["error"].get("team_id").is_none_or(Value::is_null), "the own grant is why: {err}");
    assert_eq!(err["error"]["chain"], json!(["boss", "postgres"]), "{err}");
    nothing_written(&w, ta);
}

// An own Admin grant (fine at Admin) narrowed on web1 to Developer {boss}: the database's
// effective access breaks. 422 naming `boss` and web1.
#[tokio::test]
async fn demoting_a_tenant_admin_whose_override_leaves_an_owner_level_role_is_refused() {
    let w = world().await;
    let ta = tenant_admin(&w);
    w.metadata.grant(ta, w.project, PlatformRole::Admin, &[]);
    w.metadata.narrow(ta, w.db, OverrideRow { platform_role: Some(PlatformRole::Developer), pg_roles: Some(vec!["boss".into()]) });
    let (status, err) = demote(&w, ta).await;
    assert_eq!((status, err["error"]["role"].as_str()), (422, Some("boss")), "{err}");
    assert_eq!(err["error"]["project_id"], json!(w.project), "{err}");
    assert_eq!(err["error"]["database_id"], json!(w.db), "{err}");
    nothing_written(&w, ta);
}

// A chain holding ADMIN on another role is refused too (R30 via `check_effective`), here
// through a team at Developer.
#[tokio::test]
async fn demoting_a_tenant_admin_whose_team_grants_an_admin_chain_is_refused() {
    let w = world().await;
    let ep = main_endpoint(&w);
    w.metadata.add_group_role_with(&ep, "lead", &["plain"], &[], &["plain"]);
    let ta = tenant_admin(&w);
    let team = w.metadata.seed_team(w.tenant, "Leads");
    w.metadata.seed_team_grant(team, w.project, PlatformRole::Developer, &["lead"]);
    w.metadata.seed_team_member(team, ta);
    let (status, err) = demote(&w, ta).await;
    assert_eq!((status, err["error"]["role"].as_str()), (422, Some("lead")), "{err}");
    assert_eq!(err["error"]["team_id"], json!(team), "{err}");
    nothing_written(&w, ta);
}

// Guard (passes on the base): a clean demotion still goes through with its usual side effects:
// 200 with `tenant_admin: false`, one `admin_unset` event, every branch of the tenant
// reconciles, and realtime re-checks the member (`access_changed` with their sub). Own
// Developer {plain} plus a team's Viewer {plain} is clean; so is own Developer {boss} while a
// team makes the member Admin on the project (R17: the union is checked, not each source).
#[tokio::test]
async fn a_clean_demotion_succeeds_with_its_side_effects() {
    let w = world().await;
    let ta = tenant_admin(&w);
    w.metadata.grant(ta, w.project, PlatformRole::Developer, &["plain"]);
    let viewers = w.metadata.seed_team(w.tenant, "Viewers");
    w.metadata.seed_team_grant(viewers, w.project, PlatformRole::Viewer, &["plain"]);
    w.metadata.seed_team_member(viewers, ta);
    w.app.rt_events.clear();
    let (status, body) = demote(&w, ta).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["tenant_admin"], false, "{body}");
    assert!(!is_tenant_admin(&w, ta));
    assert_eq!(admin_unset_events(&w, ta), 1);
    let enqueued = w.jobs.member_role_enqueues.lock().unwrap().clone();
    for branch in [BranchId::from(w.db), w.dev] {
        assert!(enqueued.contains(&branch), "{branch} reconciles: {enqueued:?}");
    }
    let wanted = chelabase_nats::rt::AccessEvent { tenant_id: w.tenant.as_uuid(), member_sub: Some("ta".into()) };
    let mut seen = Vec::new();
    for _ in 0..50 {
        seen = w.app.rt_events.access();
        if seen.contains(&wanted) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(seen.contains(&wanted), "access_changed for the member: {seen:?}");

    // R17: a team's Admin makes the own grant's `boss` fine to hold.
    let tb = w.metadata.add_member(w.tenant, "tb", MemberStatus::Active, true);
    w.metadata.grant(tb, w.project, PlatformRole::Developer, &["boss"]);
    let admins = w.metadata.seed_team(w.tenant, "Admins");
    w.metadata.seed_team_grant(admins, w.project, PlatformRole::Admin, &[]);
    w.metadata.seed_team_member(admins, tb);
    let (status, body) = demote(&w, tb).await;
    assert_eq!(status, 200, "{body}");
    assert!(!is_tenant_admin(&w, tb));
}

// P6/P25 race: the tenant admin's team grants Viewer {} (clean) when the demotion is checked;
// a team write changes it to Viewer {login} before the demotion's write. The write sees the
// change (`Changed`), the demotion is checked again and refused naming `login` and the team.
#[tokio::test]
async fn a_demotion_racing_a_team_grant_change_is_checked_again() {
    let w = world().await;
    let ta = tenant_admin(&w);
    let team = w.metadata.seed_team(w.tenant, "Viewers");
    w.metadata.seed_team_grant(team, w.project, PlatformRole::Viewer, &[]);
    w.metadata.seed_team_member(team, ta);
    let project = w.project;
    w.metadata.race_once(move |md| md.seed_team_grant(team, project, PlatformRole::Viewer, &["login"]));
    let (status, err) = demote(&w, ta).await;
    assert_eq!((status, err["error"]["role"].as_str()), (422, Some("login")), "{err}");
    assert_eq!(err["error"]["team_id"], json!(team), "{err}");
    assert_eq!(err["error"]["database_id"], json!(w.db), "{err}");
    assert!(w.metadata.race_before_write.lock().unwrap().is_empty(), "the race ran");
    nothing_written(&w, ta);
}

// P6/P25 race: the tenant admin has no access of their own when the demotion is checked; a
// team write adds them to a team granting Viewer {login} before the demotion's write. The write
// sees it, the demotion is checked again and refused naming `login` and the team.
#[tokio::test]
async fn a_demotion_racing_a_team_member_add_is_checked_again() {
    let w = world().await;
    let ta = tenant_admin(&w);
    let team = w.metadata.seed_team(w.tenant, "Logins");
    w.metadata.seed_team_grant(team, w.project, PlatformRole::Viewer, &["login"]);
    w.metadata.race_once(move |md| md.seed_team_member(team, ta));
    let (status, err) = demote(&w, ta).await;
    assert_eq!((status, err["error"]["role"].as_str()), (422, Some("login")), "{err}");
    assert_eq!(err["error"]["team_id"], json!(team), "{err}");
    assert_eq!(err["error"]["project_id"], json!(w.project), "{err}");
    assert!(w.metadata.race_before_write.lock().unwrap().is_empty(), "the race ran");
    nothing_written(&w, ta);
}
