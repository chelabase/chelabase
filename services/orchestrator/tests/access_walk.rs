//! Route walk (members and roles, invariant test 2): every orchestrator endpoint is called as a
//! non-member, an ungranted member, a member one level too low (on an unprotected and on a
//! protected branch), a member asking for a role outside their set, and, as the positive
//! control, a member at exactly the minimum, in both modes, through the fake-backed API. Each
//! refusal must be the answer the endpoint's `Access` implies, and the control must pass the
//! access check. A handler that skips `authorize`, or checks the wrong level, fails here.
#![allow(dead_code, unused_imports)]

include!("common/fakes.rs");

use chelabase_routes::{Access, Endpoint};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Caller {
    /// Never seen before. The walk's tenant takes no sign-ups (R39), so they have no member row
    /// in either mode.
    NonMember,
    /// Signed up and waiting for approval (R40).
    Pending,
    /// An active member with a grant on another project only.
    Ungranted,
    /// An active member one level below what the endpoint needs (unprotected branches).
    TooLow,
    /// One level below what a `Branch` endpoint needs on a protected branch.
    TooLowProtected,
    /// An `ActAs` endpoint at its minimum, asking for a role outside the caller's set.
    OutsideSet,
    /// The positive control: exactly the minimum, with a role in the set for `ActAs`.
    AtMinimum,
    /// Members hub T8 (P14): suspended by a tenant admin, with a Developer grant it keeps.
    Suspended,
}

/// An access answer: status, code and `required`.
type Answer = (u16, &'static str, Option<String>);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Expect {
    Refuse(Answer),
    /// Passes the access check (any other answer is fine).
    Pass,
    /// This caller kind says nothing about the endpoint.
    Skip,
}

/// Who calls, as which role (`None`: no role, so the owner for `ActAs`), and what they get.
struct Call {
    sub: &'static str,
    role: Option<&'static str>,
    expect: Expect,
}

/// The orchestrator's endpoints.
fn orchestrator_endpoints() -> Vec<&'static Endpoint> {
    ENDPOINTS
        .iter()
        .filter(|e| e.service == Service::Orchestrator)
        .collect()
}

/// The role one below `role`, if any.
fn below(role: PlatformRole) -> Option<PlatformRole> {
    match role {
        PlatformRole::Viewer => None,
        PlatformRole::Developer => Some(PlatformRole::Viewer),
        PlatformRole::Admin => Some(PlatformRole::Developer),
    }
}

fn call(sub: &'static str, role: Option<&'static str>, expect: Expect) -> Call {
    Call { sub, role, expect }
}

fn forbidden(required: &str) -> Expect {
    Expect::Refuse((403, "forbidden", Some(required.to_string())))
}

/// Who acts as `caller` on `e`, and what they must get. Written from the design's answers
/// table, not from `decide`.
fn plan(e: &Endpoint, caller: Caller, mode: Mode) -> Call {
    use Expect::{Pass, Refuse, Skip};
    let access = e.policy.access;
    match (caller, access) {
        // P14: a suspended member is refused everywhere, whatever the route needs, except
        // `/v1/me` (the one signed-in route), which tells them they're suspended.
        (Caller::Suspended, Access::Public | Access::DataKey) => call("suspended", None, Skip),
        (Caller::Suspended, Access::SignedIn) => call("suspended", None, Pass),
        (Caller::Suspended, _) => call("suspended", Some("app"), Refuse((403, "member_suspended", None))),
        (Caller::AtMinimum, Access::Public | Access::DataKey) => call("stranger", None, Pass),
        // #119, #126: a member token's `/v1/me` needs a row in its tenant (a stranger is 403).
        (Caller::AtMinimum, Access::SignedIn) => call("outsider", None, Pass),
        (_, Access::Public | Access::SignedIn | Access::DataKey) => call("stranger", None, Skip),

        // A pending member is refused everywhere, whatever the route needs.
        (Caller::Pending, _) => call("pending", None, Refuse((403, "pending_approval", None))),

        (Caller::NonMember, _) if mode == Mode::Platform => call(
            "stranger",
            None,
            Refuse((403, "not_available_in_platform_mode", None)),
        ),
        // R39: sign-up is closed in the walk's tenant, so a stranger has no member row at all.
        (Caller::NonMember, _) => call("stranger", None, Refuse((403, "no_access_yet", None))),

        (Caller::Ungranted, Access::TenantMember) => call("outsider", None, Pass),
        (Caller::Ungranted, Access::TenantAdmin) => {
            call("outsider", None, forbidden("tenant admin"))
        }
        (Caller::Ungranted, Access::Instance) => {
            call("outsider", None, forbidden("instance admin"))
        }
        (Caller::Ungranted, _) => call("outsider", None, Refuse((404, "not_found", None))),

        (Caller::TooLow, Access::TenantMember) => call("viewer", None, Skip),
        (Caller::TooLow, Access::TenantAdmin) => call("admin", None, forbidden("tenant admin")),
        (Caller::TooLow, Access::Instance) => call("admin", None, forbidden("instance admin")),
        (Caller::TooLow, Access::Project(min)) => match below(min) {
            Some(lower) => call(sub_of(lower), None, forbidden(min.as_str())),
            None => call("viewer", None, Skip),
        },
        (Caller::TooLow, Access::Branch { unprotected, .. }) => match below(unprotected) {
            Some(lower) => call(sub_of(lower), None, forbidden(unprotected.as_str())),
            None => call("viewer", None, Skip),
        },
        // No role asked for is the owner: a Viewer route needs Admin for it.
        (Caller::TooLow, Access::ActAs(min)) => match below(min) {
            Some(lower) => call(sub_of(lower), Some("app"), forbidden(min.as_str())),
            None => call("viewer", None, forbidden("owner")),
        },
        (Caller::TooLow, Access::ActAsBranch { unprotected, .. }) => match below(unprotected) {
            Some(lower) => call(sub_of(lower), Some("app"), forbidden(unprotected.as_str())),
            None => call("viewer", None, forbidden("owner")),
        },

        (Caller::TooLowProtected, Access::Branch { protected, .. }) => match below(protected) {
            Some(lower) => call(sub_of(lower), None, forbidden(protected.as_str())),
            None => call("viewer", None, Skip),
        },
        (Caller::TooLowProtected, Access::ActAsBranch { protected, .. }) => match below(protected) {
            Some(lower) => call(sub_of(lower), Some("app"), forbidden(protected.as_str())),
            None => call("viewer", None, Skip),
        },
        (Caller::TooLowProtected, _) => call("viewer", None, Skip),

        (Caller::OutsideSet, Access::ActAs(min)) => {
            call(sub_of(min), Some("other"), forbidden("role:other"))
        }
        (Caller::OutsideSet, Access::ActAsBranch { unprotected, .. }) => {
            call(sub_of(unprotected), Some("other"), forbidden("role:other"))
        }
        (Caller::OutsideSet, _) => call("viewer", None, Skip),

        (Caller::AtMinimum, Access::TenantMember) => call("viewer", None, Pass),
        (Caller::AtMinimum, Access::TenantAdmin) => call(DEFAULT_SUB, None, Pass),
        (Caller::AtMinimum, Access::Instance) if mode == Mode::SelfHosted => {
            call(DEFAULT_SUB, None, Pass)
        }
        (Caller::AtMinimum, Access::Instance) => call(DEFAULT_SUB, None, Skip),
        (Caller::AtMinimum, Access::Project(min)) => call(sub_of(min), None, Pass),
        (Caller::AtMinimum, Access::Branch { unprotected, .. }) => {
            call(sub_of(unprotected), None, Pass)
        }
        (Caller::AtMinimum, Access::ActAs(min)) => call(sub_of(min), Some("app"), Pass),
        (Caller::AtMinimum, Access::ActAsBranch { unprotected, .. }) => {
            call(sub_of(unprotected), Some("app"), Pass)
        }
    }
}

fn sub_of(role: PlatformRole) -> &'static str {
    match role {
        PlatformRole::Viewer => "viewer",
        PlatformRole::Developer => "developer",
        PlatformRole::Admin => "admin",
    }
}

/// The world plus the walk's members; with `protected`, `dev` and main are protected.
async fn walk_world(mode: Mode, protected: bool) -> MemberWorld {
    let w = member_world(mode).await;
    let elsewhere = w
        .metadata
        .create_project(w.tenant, "Elsewhere")
        .await
        .unwrap()
        .id;
    w.metadata.add_granted(
        w.tenant,
        "outsider",
        elsewhere,
        PlatformRole::Viewer,
        &["app"],
    );
    w.metadata.add_member(w.tenant, "pending", MemberStatus::Pending, false);
    // T8 (P14): suspended, keeping a Developer grant it would otherwise pass with.
    w.metadata.add_granted(w.tenant, "suspended", w.project, PlatformRole::Developer, &["app"]);
    w.metadata.set_member(w.tenant, "suspended", |m| m.status = MemberStatus::Suspended);
    // A reconcile gives each member a member role on `dev` that can become `app` (T9).
    let mut rows = Vec::new();
    for role in [
        PlatformRole::Viewer,
        PlatformRole::Developer,
        PlatformRole::Admin,
    ] {
        let member = w
            .metadata
            .add_granted(w.tenant, sub_of(role), w.project, role, &["app"]);
        let role_name = chelabase_types::role_name::member_role_name(member);
        let password_enc = chelabase_secrets::seal_member_role_password(
            &w.app.key,
            w.dev.as_uuid(),
            &role_name,
            "pw",
        )
        .unwrap();
        rows.push(MemberBranchRole {
            member_id: member,
            branch_id: w.dev.as_uuid(),
            role_name,
            password_enc,
            applied_roles: vec!["app".into()],
        });
    }
    *w.jobs.member_rows_on_reconcile.lock().unwrap() = Some((w.metadata.member_roles.clone(), rows));
    if protected {
        let mut branches = w.metadata.branches.lock().unwrap();
        for id in [w.dev, BranchId::from(w.db)] {
            branches.get_mut(&id).unwrap().protected = true;
        }
    }
    w
}

/// The endpoint's path with the world's ids (project routes take the project, the rest the
/// database; branch routes `dev`, which `create_merge` merges into main), and `?role=` for a
/// GET act-as route.
fn path_of(e: &Endpoint, w: &MemberWorld, role: Option<&str>) -> String {
    let id = if e.path.starts_with("/v1/projects/") {
        w.project.as_uuid()
    } else {
        w.db.as_uuid()
    };
    // Members hub step 2 (P35): a team of the walk's tenant, fresh for each call (a walked
    // delete never takes another route's team away).
    let path = if e.path.contains("{team_id}") {
        let team = w.metadata.seed_team(w.tenant, &format!("walk-{}", uuid::Uuid::new_v4()));
        e.path.replace("{team_id}", &team.to_string())
    } else {
        e.path.to_string()
    };
    let path = path
        .replace("{project_id}", &w.project.as_uuid().to_string())
        .replace("{id}", &id.to_string())
        .replace("{branch_id}", &w.dev.as_uuid().to_string())
        .replace("{merge_id}", &w.merge.to_string())
        .replace("{role}", "app")
        // m9: Studio's storage routes.
        .replace("{*name}", "dir/a.txt")
        .replace("{bucket}", "b")
        .replace("{session}", &uuid::Uuid::new_v4().to_string())
        .replace("{part}", "1")
        .replace("{name}", "web1")
        .replace("{key_id}", &uuid::Uuid::new_v4().to_string())
        .replace("{query_id}", &uuid::Uuid::new_v4().to_string())
        .replace("{run_id}", &uuid::Uuid::new_v4().to_string())
        .replace("{user_id}", &uuid::Uuid::new_v4().to_string())
        .replace("{member_id}", &uuid::Uuid::new_v4().to_string())
        .replace("{invite_id}", &uuid::Uuid::new_v4().to_string())
        .replace("{schema}", "public")
        .replace("{table}", "notes");
    match (role, e.method, e.policy.access) {
        // m9: Studio's storage routes take the role in the query whatever their method.
        (Some(role), _, Access::ActAs(_)) if e.name.starts_with(chelabase_routes::orchestrator::STORAGE_PREFIX) => {
            format!("{path}?role={role}")
        }
        (Some(role), chelabase_routes::Method::Get, Access::ActAs(_)) => {
            format!("{path}?role={role}")
        }
        // The types download refines by key role as createKey does: the walk's members hold
        // `app`, not the default `authenticated`.
        _ if e.name == chelabase_routes::orchestrator::DATA_API_TYPES.name => format!("{path}?role=app"),
        _ => path,
    }
}

/// A body the route accepts far enough to reach the access check (`None` for no body), with
/// `role` for an act-as body.
fn body_of(e: &Endpoint, w: &MemberWorld, role: Option<&str>) -> Option<Value> {
    use chelabase_routes::orchestrator as r;
    let name = e.name;
    if name == r::CREATE_DATABASE.name {
        return Some(json!({ "project_id": w.project, "name": "walked" }));
    }
    if name == r::CREATE_PROJECT.name || name == r::UPDATE_PROJECT.name {
        return Some(json!({ "name": "walked" }));
    }
    if name == r::DELETE_PROJECT.name {
        return Some(json!({ "confirm": "billing" }));
    }
    // Bodies a typed extractor (or a body read before the check) must accept.
    let typed = [
        (r::UPDATE_BRANCH.name, json!({ "protected": false })),
        (r::ACKNOWLEDGE_BRANCH_REVIEW.name, json!({ "hash": "h" })),
        (
            r::FIX_BRANCH_REVIEW_ITEM.name,
            json!({ "key": "k", "action": "drop", "hash": "h" }),
        ),
        (
            r::CONSOLE_QUERY.name,
            json!({ "role": role, "tree": { "v": 1, "op": "select", "from": "notes" } }),
        ),
        (
            r::CREATE_MERGE.name,
            json!({ "direction": "into_parent", "database": "app" }),
        ),
        (
            r::CREATE_SAVED_QUERY.name,
            json!({ "name": "q", "sql": "select 1" }),
        ),
        (r::UPDATE_SAVED_QUERY.name, json!({ "version": 1 })),
        (
            r::RUN_SQL.name,
            json!({ "run_id": uuid::Uuid::new_v4(), "role": role, "sql": "select 1" }),
        ),
        (r::APPLY_SCHEMA_CHANGES.name, json!({ "role": role, "dry_run": true, "operations": [{ "set_table_comment": { "table": { "schema": "public", "name": "notes" }, "comment": null } }] })),
        (r::INSERT_DEFAULT_ROW.name, json!({ "role": role })),
        (r::GET_CELL.name, json!({ "role": role, "key": { "id": 1 }, "column": "body" })),
        (r::CREATE_ROLE.name, json!({ "name": "walked" })),
        (r::PUT_APP_USERS.name, json!({ "database_id": w.db })),
        (
            r::CREATE_APP_USER.name,
            json!({ "email": "a@example.test", "password": "correct-horse-9" }),
        ),
        // T7: the member API.
        (r::UPDATE_MEMBER.name, json!({ "tenant_admin": false })),
        (
            r::CREATE_INVITE.name,
            json!({ "email": "walked@example.test", "grants": [] }),
        ),
        (
            r::CREATE_PROJECT_INVITE.name,
            json!({ "email": "walked@example.test", "platform_role": "viewer", "pg_roles": [] }),
        ),
        (
            r::PUT_PROJECT_GRANT.name,
            json!({ "platform_role": "viewer", "pg_roles": [] }),
        ),
        (
            r::PUT_GRANT_OVERRIDE.name,
            json!({ "database_id": w.db, "platform_role": "viewer" }),
        ),
        (r::UPDATE_INSTANCE_SETTINGS.name, json!({ "open_signup": false })),
        (r::UPDATE_TENANT.name, json!({ "display_name": "Walked" })),
        (r::APPROVE_MEMBER.name, json!({ "grants": [] })),
        // Members hub T8: the personal login's name.
        ("orchestrator.put_personal_login", json!({ "name": "walked" })),
        // Members hub step 2 (P35): the team writes.
        ("orchestrator.create_team", json!({ "name": format!("walked-{}", uuid::Uuid::new_v4()) })),
        ("orchestrator.update_team", json!({ "description": "walked" })),
        ("orchestrator.put_team_grant", json!({ "platform_role": "viewer", "pg_roles": [] })),
    ];
    if let Some((_, body)) = typed.into_iter().find(|(n, _)| *n == name) {
        return Some(body);
    }
    match e.method {
        chelabase_routes::Method::Get | chelabase_routes::Method::Delete => None,
        _ => Some(json!({})),
    }
}

fn method_of(e: &Endpoint) -> Method {
    Method::from_bytes(e.method.as_str().as_bytes()).unwrap()
}

/// Calls every orchestrator endpoint as `caller` and checks each answer; returns how many
/// endpoints were called.
async fn walk(caller: Caller, mode: Mode) -> usize {
    let w = walk_world(mode, caller == Caller::TooLowProtected).await;
    let mut failures = vec![];
    let mut called = 0;
    for e in orchestrator_endpoints() {
        let Call { sub, role, expect } = plan(e, caller, mode);
        if expect == Expect::Skip {
            continue;
        }
        let path = path_of(e, &w, role);
        let resp = w
            .app
            .send_as(sub, "org-1", method_of(e), &path, body_of(e, &w, role))
            .await;
        called += 1;
        let status = resp.status().as_u16();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        let code = body["error"]["code"].as_str().unwrap_or("");
        let required = body["error"]["required"].as_str().map(str::to_string);
        let what = format!("{} {} as {sub} ({role:?})", e.method.as_str(), e.path);
        match expect {
            Expect::Refuse((want_status, want_code, want_required)) => {
                if (status, code, &required) != (want_status, want_code, &want_required) {
                    failures.push(format!(
                        "{what}: got {status} {code} {required:?}, want {want_status} {want_code} {want_required:?}"
                    ));
                }
            }
            Expect::Pass => {
                let refused = matches!(
                    code,
                    "forbidden" | "no_access_yet" | "pending_approval" | "not_available_in_platform_mode"
                ) || status == 401
                    || (status == 404 && body["error"]["message"] == "not found");
                if refused {
                    failures.push(format!(
                        "{what}: unexpected access refusal {status} {code} {required:?}"
                    ));
                }
            }
            Expect::Skip => unreachable!(),
        }
    }
    assert!(
        failures.is_empty(),
        "{caller:?} in {mode:?}:\n{}",
        failures.join("\n")
    );
    called
}

#[tokio::test]
async fn every_route_refuses_non_members() {
    for mode in [Mode::SelfHosted, Mode::Platform] {
        walk(Caller::NonMember, mode).await;
    }
}

#[tokio::test]
async fn every_route_refuses_pending_members() {
    for mode in [Mode::SelfHosted, Mode::Platform] {
        walk(Caller::Pending, mode).await;
    }
}

// Members hub T8 (P14, T2-3): every route but `/v1/me` answers a suspended member 403
// `member_suspended`, the personal-login routes included.
#[tokio::test]
async fn every_route_refuses_suspended_members_but_me() {
    for mode in [Mode::SelfHosted, Mode::Platform] {
        walk(Caller::Suspended, mode).await;
    }
}

#[tokio::test]
async fn every_route_refuses_ungranted_members() {
    for mode in [Mode::SelfHosted, Mode::Platform] {
        walk(Caller::Ungranted, mode).await;
    }
}

#[tokio::test]
async fn every_route_refuses_one_level_too_low() {
    for mode in [Mode::SelfHosted, Mode::Platform] {
        walk(Caller::TooLow, mode).await;
    }
}

#[tokio::test]
async fn branch_routes_on_a_protected_branch_need_the_protected_minimum() {
    for mode in [Mode::SelfHosted, Mode::Platform] {
        assert!(walk(Caller::TooLowProtected, mode).await > 10);
    }
}

#[tokio::test]
async fn act_as_refuses_a_role_outside_the_set() {
    for mode in [Mode::SelfHosted, Mode::Platform] {
        // The four act-as routes (the branch connection is Admin-only now, R22).
        assert!(walk(Caller::OutsideSet, mode).await >= 4);
    }
}

#[tokio::test]
async fn a_caller_at_the_minimum_passes_every_route() {
    for mode in [Mode::SelfHosted, Mode::Platform] {
        walk(Caller::AtMinimum, mode).await;
    }
}

#[tokio::test]
async fn walk_covers_every_orchestrator_endpoint() {
    let total = orchestrator_endpoints().len();
    assert!(total > 50, "the orchestrator has {total} endpoints");
    // The positive control calls every endpoint (it skips none).
    assert_eq!(walk(Caller::AtMinimum, Mode::SelfHosted).await, total);
    // Every endpoint that needs a member is refused somewhere in the walk: none is listed as
    // open to everyone by mistake.
    let callers = [
        Caller::NonMember,
        Caller::Pending,
        Caller::Ungranted,
        Caller::TooLow,
        Caller::TooLowProtected,
        Caller::OutsideSet,
    ];
    for e in orchestrator_endpoints() {
        let refused = callers
            .into_iter()
            .flat_map(|c| [(c, Mode::SelfHosted), (c, Mode::Platform)])
            .any(|(c, mode)| matches!(plan(e, c, mode).expect, Expect::Refuse(_)));
        let open = matches!(e.policy.access, Access::Public | Access::SignedIn);
        assert!(refused || open, "{} is never refused", e.name);
    }
}
