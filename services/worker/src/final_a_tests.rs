//! Members hub step 1, final fix round, area A: the worker's RED tests (T6-M1–M4, T6-M6,
//! EOS-1, EOS-4, EOS-H2 and D7's sweep), over the fakes.

use std::collections::BTreeSet;

use chelabase_metadata::RoleOrigin;
use chelabase_metadata::members::{GrantRow, MemberBranchRole, MemberRecord, MemberStatus};
use chelabase_metadata::personal_logins::PersonalLoginRecord;
use chelabase_secrets::{open_member_role_password, seal_member_role_password};
use chelabase_types::access::PlatformRole;
use chelabase_types::{BranchId, DatabaseId, TenantId};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::OWNER_ROLE;
use crate::fakes::{FakeLogin, FakeWorker, ready_database, worker};
use crate::member_roles::{
    ActualMemberRole, DesiredMemberRole, RoleKind, member_catalog_sql, member_role_name,
    parse_member_catalog, plan_member_roles, render_step,
};
use crate::resync::ObservedRole;

const NEEDLE: &str = "chelabase: member roles";

fn h(s: &str) -> String {
    hex::encode(s.as_bytes())
}

fn far() -> DateTime<Utc> {
    chrono::TimeZone::with_ymd_and_hms(&Utc, 2099, 1, 2, 3, 4, 5).unwrap()
}

fn scram(password: &str) -> String {
    chelabase_neon::scram::verifier(password)
}

/// Catalog lines: plain groups (`!` owner-level), and the `app` database.
fn groups(roles: &[&str]) -> String {
    let mut out = String::new();
    for r in roles {
        let (name, level) = r.strip_prefix('!').map_or((*r, "f"), |n| (n, "t"));
        out.push_str(&format!("r {} f {level} f f\n", h(name)));
    }
    out.push_str(&format!("d {} {}\n", h("app"), h(OWNER_ROLE)));
    out
}

/// A marked role's lines, its memberships ours and shaped.
fn marked(name: &str, verifier: &str, until: DateTime<Utc>, memberships: &[&str]) -> String {
    let mut out = format!(
        "r {n} t f f t\nk {n}\np {n} {}\nu {n} {}\n",
        h(verifier),
        until.timestamp_micros(),
        n = h(name)
    );
    for m in memberships {
        out.push_str(&format!("m {} {} - t\n", h(name), h(m)));
    }
    out
}

fn member(tenant: TenantId, identity_user: &str) -> MemberRecord {
    MemberRecord {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        identity_user_id: identity_user.to_string(),
        email: None,
        display_name: "m".into(),
        tenant_admin: false,
        status: MemberStatus::Active,
        accepted_at: None,
        offboarded: false, suspended_at: None, suspended_by: None,
    }
}

/// An active member of `db`'s tenant, with a Developer grant naming `roles` when `granted`.
fn add_member(w: &FakeWorker, db: DatabaseId, roles: &[&str], granted: bool) -> Uuid {
    let record = w.metadata.database(db);
    let m = member(record.tenant_id, &format!("z-{}", Uuid::new_v4()));
    let id = m.id;
    let mut state = w.metadata.state();
    state.members.push(m);
    if granted {
        state.grants.insert(
            (id, record.project_id.as_uuid()),
            GrantRow {
                platform_role: PlatformRole::Developer,
                pg_roles: roles.iter().map(|r| r.to_string()).collect(),
            },
        );
    }
    id
}

fn record(w: &FakeWorker, db: DatabaseId, member: Uuid, name: &str, verifier: &str) {
    w.metadata
        .state()
        .personal_logins
        .push(PersonalLoginRecord {
            member_id: member,
            database_id: db.as_uuid(),
            role_name: name.into(),
            scram_verifier: Some(verifier.to_string()),
            valid_until: far(),
            revoked: false,
            qualifies: true,
        });
}

/// Every script but the catalog read and the read-only checks, quotes removed.
fn scripts(w: &FakeWorker) -> Vec<String> {
    w.computes
        .sql
        .lock()
        .unwrap()
        .iter()
        .map(|(_, _, s)| s.replace('"', ""))
        .filter(|s| !s.contains(NEEDLE) && !s.starts_with(crate::schema_guard::GUARD_PRESENT_MARK))
        .collect()
}

/// A ready database's main branch and its endpoint.
async fn main_of(w: &FakeWorker) -> (DatabaseId, BranchId, String) {
    let id = ready_database(w).await;
    let main = w.metadata.main(id);
    w.computes.sql.lock().unwrap().clear();
    (id, main.id, main.endpoint_id.unwrap())
}

// ---- EOS-1 ---------------------------------------------------------------------------------

// EOS-1: a revoke-now (or a suspension) that marks the member's row for rotation (`''`) after
// the reconcile read the rows and before it writes them is never overwritten: the row write is
// a compare-and-set on the password the run read, so the next reconcile still rotates.
#[tokio::test]
async fn a_rotation_mark_landing_during_a_reconcile_survives_it() {
    let (w, _journal) = worker();
    let (db, branch, ep) = main_of(&w).await;
    let alice = add_member(&w, db, &["billing"], true);
    let role = member_role_name(alice);
    let sealed = seal_member_role_password(&w.key, branch.as_uuid(), &role, "the row's").unwrap();
    w.metadata.state().member_roles.push(MemberBranchRole {
        member_id: alice,
        branch_id: branch.as_uuid(),
        role_name: role.clone(),
        password_enc: sealed,
        applied_roles: vec!["billing".into()],
    });
    // The catalog holds another password, so the run rotates and writes the row.
    w.computes.answer_at(
        &ep,
        NEEDLE,
        &format!(
            "{}r {r} t f f t\nm {r} {b} - t\np {r} {v}\n",
            groups(&["!postgres", "billing"]),
            r = h(&role),
            b = h("billing"),
            v = h(&scram("a password set by hand")),
        ),
    );
    let (member_id, branch_id) = (alice, branch.as_uuid());
    w.metadata.state().before_upsert_member_branch_role = Some(Box::new(move |state| {
        for r in state
            .member_roles
            .iter_mut()
            .filter(|r| (r.member_id, r.branch_id) == (member_id, branch_id))
        {
            r.password_enc = String::new();
        }
    }));
    w.reconcile_member_roles(branch, None).await;
    let row = w
        .metadata
        .state()
        .member_roles
        .iter()
        .find(|r| r.member_id == alice)
        .cloned()
        .expect("the row stays");
    assert_eq!(
        row.password_enc,
        "",
        "the reconcile overwrote the rotation mark (it opens: {:?})",
        open_member_role_password(&w.key, branch.as_uuid(), &role, &row.password_enc).is_ok()
    );
}

// ---- T6-M1, T6-M3 -------------------------------------------------------------------------

// T6-M1: a record whose member qualifies (as the record says) but has no effective source
// here (no grant on the project: nothing to hold) is skipped: no role is made for it.
#[tokio::test]
async fn a_qualifying_record_without_a_source_gets_no_login() {
    let (w, _journal) = worker();
    let (db, branch, ep) = main_of(&w).await;
    let alice = add_member(&w, db, &[], false);
    record(&w, db, alice, "alice", &scram("pw"));
    w.computes
        .answer_at(&ep, NEEDLE, &groups(&["!postgres", "billing"]));
    w.reconcile_member_roles(branch, None).await;
    let made: Vec<String> = scripts(&w)
        .into_iter()
        .filter(|s| s.contains("CREATE ROLE alice"))
        .collect();
    assert!(
        made.is_empty(),
        "a login with no source was made: {made:#?}"
    );
}

// T6-M3: a wanted login whose name has a legacy row of another origin on the endpoint (`sql`,
// from before the login) ends with its personal row (the record's verifier): the row is taken
// over by the login, never left routing the old password forever.
#[tokio::test]
async fn a_legacy_row_under_a_logins_name_becomes_its_personal_row() {
    let (w, _journal) = worker();
    let (db, branch, ep) = main_of(&w).await;
    let alice = add_member(&w, db, &["billing"], true);
    let verifier = scram("pw");
    record(&w, db, alice, "alice", &verifier);
    {
        let mut state = w.metadata.state();
        let endpoint = state.endpoints.get_mut(&ep).unwrap();
        endpoint
            .logins
            .insert("alice".into(), FakeLogin::new(RoleOrigin::Sql, None, false));
        endpoint.set_verifier("alice", Some(&scram("an old sql password")));
    }
    w.computes.answer_at(
        &ep,
        NEEDLE,
        &format!(
            "{}{}",
            groups(&["!postgres", "billing"]),
            marked("alice", &verifier, far(), &["billing"])
        ),
    );
    w.reconcile_member_roles(branch, None).await;
    let state = w.metadata.state();
    let endpoint = &state.endpoints[&ep];
    assert_eq!(
        endpoint.logins.get("alice").map(|l| l.origin),
        Some(RoleOrigin::Personal),
        "the legacy row still answers for the login"
    );
    assert_eq!(endpoint.verifier("alice"), Some(verifier));
}

// ---- T6-M2 ---------------------------------------------------------------------------------

// T6-M2: a marked role under a name no personal login may take (a reserved role, the owner)
// is left alone by the reconciler, so the resync never counts it as drift: no reconcile is
// asked for it, on every resync.
#[tokio::test(start_paused = true)]
async fn a_marked_reserved_or_owner_role_is_not_drift() {
    for name in ["authenticator", OWNER_ROLE] {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let db = w.metadata.state().db_rows[&main.id]
            .iter()
            .find(|r| r.main)
            .unwrap()
            .name
            .clone();
        let mut out = format!("d|{}||{}|f|||||||\n", h(&db), h(OWNER_ROLE));
        out.push_str(&crate::resync::role_line(&ObservedRole {
            password: Some(scram("pw")),
            ..ObservedRole::plain(name)
        }));
        out.push_str(&format!("m|{}||||||||||\n", h(name)));
        w.computes.answer("FROM pg_authid", &out);
        let before = w.member_role_requests.sent().len();
        w.resync(main.id).await.unwrap();
        assert_eq!(
            w.member_role_requests.sent().len(),
            before,
            "{name}: a marked role the reconciler leaves alone asked for a reconcile"
        );
    }
}

// ---- T6-M4 ---------------------------------------------------------------------------------

// T6-M4: the reconciler makes personal logins; the one-off `Worker::create_personal_login`
// (tests only) is not part of the worker's public API.
#[test]
fn create_personal_login_is_not_public() {
    let source = include_str!("roles.rs");
    let at = source
        .find("async fn create_personal_login(")
        .expect("the function is found");
    let line_start = source[..at].rfind('\n').map_or(0, |i| i + 1);
    let declaration = &source[line_start..at];
    let before: Vec<&str> = source[..line_start].lines().rev().take(3).collect();
    assert!(
        !declaration.trim_start().starts_with("pub ")
            || before.iter().any(|l| l.contains("#[cfg(")),
        "still `pub` outside tests: {declaration:?}"
    );
}

// ---- T6-M6 ---------------------------------------------------------------------------------

// T6-M6: a marked login's per-role settings are read like a member role's (`s` lines; the
// marker itself, a `setdatabase = 0` entry, is not one), and the plan resets a per-database
// setting in that database, never with a role-wide `RESET ALL` (which would drop the marker).
#[test]
fn a_marked_logins_settings_are_read_and_reset_in_their_database() {
    let sql = member_catalog_sql();
    let s_part = sql
        .split("UNION ALL")
        .find(|part| part.contains("SELECT 's '"))
        .expect("the settings read");
    assert!(
        s_part.contains(chelabase_types::role_name::PERSONAL_LOGIN_MARKER),
        "the settings read covers only member roles: {s_part}"
    );

    let verifier = scram("pw");
    let out = format!(
        "{}{}s {} {}\n",
        groups(&["billing"]),
        marked("alice", &verifier, far(), &[]),
        h("alice"),
        h("app")
    );
    let catalog = parse_member_catalog(&out).unwrap();
    assert_eq!(
        catalog.personal["alice"].settings,
        vec![Some("app".to_string())],
        "a marked login's setting in `app` isn't read"
    );

    let wanted = DesiredMemberRole {
        member_id: Uuid::nil(),
        role_name: "alice".into(),
        roles: BTreeSet::new(),
        verifier: verifier.clone(),
        rotate: false,
        locked: false,
        kind: RoleKind::Personal { valid_until: far() },
    };
    let have = ActualMemberRole {
        role_name: "alice".into(),
        attributes_ok: true,
        settings: vec![Some("app".into())],
        verifier: Some(verifier),
        memberships: Vec::new(),
        valid_until: Some(far()),
    };
    let rendered: String = plan_member_roles(&[wanted], &[have])
        .iter()
        .map(render_step)
        .collect::<Vec<_>>()
        .join("\n")
        .replace('"', "");
    assert!(
        rendered.contains("ALTER ROLE alice IN DATABASE app RESET ALL"),
        "the setting stays: {rendered}"
    );
    assert!(
        !rendered.contains("ALTER ROLE alice RESET ALL"),
        "{rendered}"
    );
}
