//! Members hub T2, the coder's edge cases beyond `api.rs`: per-role levels (D1, MH-1) where
//! the act-as chain rule (R21) and the data key role check meet a role held below the route's
//! minimum. Team grants come from the fake (step 1's metadata never fills them).
#![allow(dead_code, unused_imports)]

include!("common/fakes.rs");

use chelabase_types::access::RoleSet;

async fn error_of(resp: reqwest::Response, status: u16) -> Value {
    assert_eq!(resp.status(), status);
    resp.json().await.unwrap()
}

/// Whether a response body is an access refusal.
fn access_refusal(body: &Value) -> bool {
    matches!(body["error"]["code"].as_str(), Some("forbidden" | "no_access_yet" | "not_available_in_platform_mode"))
}

fn viewer_team(names: &[&str]) -> Grant {
    Grant { platform_role: PlatformRole::Viewer, roles: RoleSet::Only(names.iter().map(|n| PgRole::parse(n)).collect()) }
}

/// `billing_admin` can become `billing`; `sales` stands alone. `mix` is a Developer through
/// their own grant (`sales`) and holds `billing_admin` only from a Viewer team; `lead` is the
/// other way round (own Developer `billing_admin`, team Viewer `sales`).
fn roles(w: &MemberWorld) -> (uuid::Uuid, uuid::Uuid) {
    let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
    w.metadata.add_group_role(&ep, "billing", &[]);
    w.metadata.add_group_role(&ep, "billing_admin", &["billing"]);
    w.metadata.add_group_role(&ep, "sales", &[]);
    let mix = w.metadata.add_granted(w.tenant, "mix", w.project, PlatformRole::Developer, &["sales"]);
    w.metadata.team_grant(mix, w.project, viewer_team(&["billing_admin"]));
    let lead = w.metadata.add_granted(w.tenant, "lead", w.project, PlatformRole::Developer, &["billing_admin"]);
    w.metadata.team_grant(lead, w.project, viewer_team(&["sales"]));
    (mix, lead)
}

fn sql_path(w: &MemberWorld) -> String {
    format!("/v1/databases/{}/branches/{}/sql", w.db, w.dev.as_uuid())
}

fn run(role: &str) -> Value {
    json!({ "run_id": uuid::Uuid::new_v4(), "role": role, "sql": "select 1" })
}

// R21 with D1: a role reached through a granted role's chain is held at that granted role's
// level. A Viewer-level role's chain opens no Developer route; a Developer-level one's does.
#[tokio::test]
async fn a_chain_counts_at_the_granting_roles_level() {
    let w = member_world(Mode::SelfHosted).await;
    roles(&w);
    let sql = sql_path(&w);
    let as_ = |sub: &'static str, role: &'static str| {
        let (app, sql) = (&w.app, sql.clone());
        async move { app.send_as(sub, "org-1", Method::POST, &sql, Some(run(role))).await }
    };
    // `mix`: billing_admin (Viewer) and billing (its chain) are refused on a Developer route.
    for role in ["billing_admin", "billing"] {
        let err = error_of(as_("mix", role).await, 403).await;
        assert_eq!(err["error"]["required"].as_str(), Some(format!("role:{role}").as_str()), "{err}");
    }
    let ok: Value = as_("mix", "sales").await.json().await.unwrap_or(Value::Null);
    assert!(!access_refusal(&ok), "{ok}");
    // `lead`: billing through the Developer-level billing_admin passes; sales (Viewer) doesn't.
    let ok: Value = as_("lead", "billing").await.json().await.unwrap_or(Value::Null);
    assert!(!access_refusal(&ok), "{ok}");
    let err = error_of(as_("lead", "sales").await, 403).await;
    assert_eq!(err["error"]["required"], "role:sales");
    // A Viewer route (the console's select) accepts the Viewer-level roles and their chains.
    let console = format!("/v1/databases/{}/branches/{}/console/query", w.db, w.dev.as_uuid());
    for role in ["billing_admin", "billing"] {
        let body = json!({ "role": role, "tree": { "v": 1, "op": "select", "from": "notes" } });
        let resp = w.app.send_as("mix", "org-1", Method::POST, &console, Some(body)).await;
        let answer: Value = resp.json().await.unwrap_or(Value::Null);
        assert!(!access_refusal(&answer), "{role}: {answer}");
    }
}

// D1: a data key acts as its role with writes, so its role must be held at Developer (the key
// routes' minimum), directly or through the chain of a role held at Developer.
#[tokio::test]
async fn a_key_needs_its_role_held_at_developer() {
    let w = member_world(Mode::SelfHosted).await;
    roles(&w);
    let keys = format!("/v1/databases/{}/branches/{}/keys", w.db, w.dev.as_uuid());
    let create = |sub: &'static str, role: &'static str| {
        let (app, keys) = (&w.app, keys.clone());
        async move {
            let body = json!({ "name": format!("{sub}-{role}"), "role": role });
            app.send_as(sub, "org-1", Method::POST, &keys, Some(body)).await
        }
    };
    for role in ["billing_admin", "billing"] {
        let err = error_of(create("mix", role).await, 403).await;
        assert_eq!(err["error"]["required"].as_str(), Some(format!("role:{role}").as_str()), "{err}");
    }
    assert_eq!(create("mix", "sales").await.status(), 201);
    assert_eq!(create("lead", "billing").await.status(), 201);
    let err = error_of(create("lead", "sales").await, 403).await;
    assert_eq!(err["error"]["required"], "role:sales");
}

// MH-3 at the route: a team Admin gives every role; a data API role the own Viewer grant names
// stays at Viewer, so a Developer route as `anon` is refused while a Viewer one passes.
#[tokio::test]
async fn a_named_data_api_role_keeps_its_own_level_under_a_team_admin() {
    let w = member_world(Mode::SelfHosted).await;
    let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
    w.metadata.add_group_role(&ep, "sales", &[]);
    let m = w.metadata.add_granted(w.tenant, "ann", w.project, PlatformRole::Viewer, &["anon"]);
    w.metadata.team_grant(m, w.project, Grant { platform_role: PlatformRole::Admin, roles: RoleSet::all() });
    let sql = sql_path(&w);
    let err = error_of(w.app.send_as("ann", "org-1", Method::POST, &sql, Some(run("anon"))).await, 403).await;
    assert_eq!(err["error"]["required"], "role:anon");
    let ok: Value = w.app.send_as("ann", "org-1", Method::POST, &sql, Some(run("sales"))).await.json().await.unwrap_or(Value::Null);
    assert!(!access_refusal(&ok), "{ok}");
    // Never `authenticated`: no source names it.
    let err = error_of(w.app.send_as("ann", "org-1", Method::POST, &sql, Some(run("authenticated"))).await, 403).await;
    assert_eq!(err["error"]["required"], "role:authenticated");
}

async fn me_of(w: &MemberWorld, sub: &'static str) -> Value {
    w.app.send_as(sub, "org-1", Method::GET, "/v1/me", None).await.json().await.unwrap()
}

// D1 on `/v1/me`: each held role at its own level, so Studio can gate writes per acted-as role.
#[tokio::test]
async fn me_carries_each_roles_level() {
    let w = member_world(Mode::SelfHosted).await;
    roles(&w);
    // `mix`: own Developer `sales`, team Viewer `billing_admin`.
    let me = me_of(&w, "mix").await;
    assert_eq!(me["projects"][0]["role_levels"], json!({ "billing_admin": "viewer", "sales": "developer" }), "{me}");
    // `lead`: the other way round.
    let me = me_of(&w, "lead").await;
    assert_eq!(me["projects"][0]["role_levels"], json!({ "billing_admin": "developer", "sales": "viewer" }), "{me}");
}

// An override narrows the level on its database (the one the editor uses), not the project's.
#[tokio::test]
async fn me_role_levels_apply_the_database_override() {
    let w = member_world(Mode::SelfHosted).await;
    let (mix, _) = roles(&w);
    w.metadata.narrow(mix, w.db, OverrideRow { platform_role: Some(PlatformRole::Viewer), pg_roles: None });
    let me = me_of(&w, "mix").await;
    let project = &me["projects"][0];
    assert_eq!(project["role_levels"], json!({ "billing_admin": "viewer", "sales": "developer" }), "{me}");
    assert_eq!(project["databases"][0]["role_levels"], json!({ "billing_admin": "viewer", "sales": "viewer" }), "{me}");
}

// Admin: every role is Admin, so `roles` stays `"all"` and no role is enumerated; a tenant admin
// the same. A named data API role keeps its own level.
#[tokio::test]
async fn me_role_levels_for_admin_are_not_enumerated() {
    let w = member_world(Mode::SelfHosted).await;
    w.metadata.add_granted(w.tenant, "root", w.project, PlatformRole::Admin, &[]);
    let me = me_of(&w, "root").await;
    assert_eq!(me["projects"][0]["roles"], "all");
    assert_eq!(me["projects"][0]["role_levels"], json!({}), "{me}");
    let me = w.app.send_as(DEFAULT_SUB, "org-1", Method::GET, "/v1/me", None).await.json::<Value>().await.unwrap();
    assert!(me["projects"].as_array().unwrap().iter().all(|p| p["role_levels"] == json!({})), "{me}");
}
