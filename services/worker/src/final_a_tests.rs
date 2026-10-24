//! Members hub step 1, final fix round, area A: the worker's RED tests (T6-M1–M4, T6-M6,
//! EOS-1, EOS-4, EOS-H2 and D7's sweep), over the fakes.

use std::collections::BTreeSet;

use chelabase_metadata::RoleOrigin;
use chelabase_metadata::members::{GrantRow, MemberBranchRole, MemberRecord, MemberStatus};
use chelabase_metadata::personal_logins::{LoginEndpoint, PersonalLoginRecord};
use chelabase_secrets::{open_member_role_password, seal_member_role_password};
use chelabase_types::access::PlatformRole;
use chelabase_types::{BranchId, DatabaseId, TenantId};
use chelabase_zitadel::client::Zitadel;
use chelabase_zitadel::mock::{ADMIN_TOKEN, MockZitadel};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::OWNER_ROLE;
use crate::fakes::{FakeLogin, FakeWorker, entries, ready_database, worker};
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

fn member(tenant: TenantId, zitadel: &str) -> MemberRecord {
    MemberRecord {
        id: Uuid::new_v4(),
        tenant_id: tenant,
        zitadel_user_id: zitadel.to_string(),
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

// ---- The identity sweep: EOS-4, EOS-H2, D7 -------------------------------------------------

/// A ready database with a Zitadel mock behind the worker, and the database's tenant.
async fn sweep_setup() -> (FakeWorker, MockZitadel, DatabaseId, BranchId, TenantId) {
    let (mut w, journal) = worker();
    let mock = MockZitadel::start().await;
    w.zitadel = Some(Zitadel::new(&mock.base, ADMIN_TOKEN));
    let db = ready_database(&w).await;
    let main = w.metadata.main(db).id;
    let tenant = w.metadata.database(db).tenant_id;
    journal.lock().unwrap().clear();
    (w, mock, db, main, tenant)
}

/// A member of `tenant` for an existing Zitadel user, holding one live login with a row on
/// main.
fn login_holder(
    w: &FakeWorker,
    db: DatabaseId,
    main: BranchId,
    tenant: TenantId,
    zitadel: &str,
) -> Uuid {
    let m = member(tenant, zitadel);
    let id = m.id;
    let role = format!("login_{}", &id.simple().to_string()[..8]);
    let ep = w.metadata.branch(main).endpoint_id.unwrap();
    let mut state = w.metadata.state();
    state.members.push(m);
    state.personal_logins.push(PersonalLoginRecord {
        member_id: id,
        database_id: db.as_uuid(),
        role_name: role.clone(),
        scram_verifier: Some(scram("pw")),
        valid_until: far(),
        revoked: false,
        qualifies: true,
    });
    state.login_endpoints.insert(
        id,
        vec![LoginEndpoint {
            branch_id: main.as_uuid(),
            endpoint_id: ep,
            role_name: role,
        }],
    );
    id
}

fn revoked(w: &FakeWorker, member: Uuid) -> bool {
    w.metadata
        .state()
        .personal_logins
        .iter()
        .filter(|l| l.member_id == member)
        .all(|l| l.revoked)
}

fn offboarded(w: &FakeWorker, member: Uuid) -> bool {
    w.metadata
        .state()
        .members
        .iter()
        .find(|m| m.id == member)
        .is_some_and(|m| m.offboarded)
}

/// Everything logged on this thread while the guard lives, as plain text.
#[derive(Clone, Default)]
struct Logs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Logs;
    fn make_writer(&'a self) -> Logs {
        self.clone()
    }
}

impl Logs {
    fn capture() -> (Logs, tracing::subscriber::DefaultGuard) {
        let logs = Logs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(logs.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        (logs, tracing::subscriber::set_default(subscriber))
    }

    fn lines_with(&self, needle: &str) -> usize {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .filter(|l| l.contains(needle))
            .count()
    }
}

// EOS-4: without a Zitadel client the sweep can't read anything; it says so once (a warning
// naming the identity sweep), not on every pass, and not never.
#[tokio::test]
async fn without_zitadel_the_sweep_says_so_once() {
    let (w, _journal) = worker();
    let (logs, _guard) = Logs::capture();
    for _ in 0..3 {
        assert_eq!(w.sweep_identities_once().await, (0, 0));
    }
    assert_eq!(
        logs.lines_with("identity sweep"),
        1,
        "{}",
        String::from_utf8_lossy(&logs.0.lock().unwrap())
    );
}

// EOS-H2: a pass in which no lookup found any user (every answer a 404, or a 404 beside
// failures) revokes nothing: Zitadel answering 404 for everyone is a misconfigured instance or
// token, not every member deleted at once. A 404 counts once the same pass found a user.
#[tokio::test]
async fn a_pass_where_every_user_is_missing_revokes_nothing() {
    let (w, mock, db, main, tenant) = sweep_setup().await;
    let a = mock.add_user("a@example.test", "pw", true);
    let b = mock.add_user("b@example.test", "pw", true);
    let ma = login_holder(&w, db, main, tenant, &a);
    let mb = login_holder(&w, db, main, tenant, &b);
    mock.remove_user(&a);
    mock.remove_user(&b);
    let (_, revoked_count) = w.sweep_identities_once().await;
    assert_eq!(revoked_count, 0, "a pass of nothing but 404s revoked");
    assert!(!revoked(&w, ma) && !revoked(&w, mb));
    assert!(
        !entries(&w.metadata.journal)
            .iter()
            .any(|e| e.starts_with("metadata.revoke_logins_for_zitadel_user")),
        "{:?}",
        entries(&w.metadata.journal)
    );

    // A 404 beside a failure: still no user found, still nothing revoked.
    let c = mock.add_user("c@example.test", "pw", true);
    let mc = login_holder(&w, db, main, tenant, &c);
    mock.fail(&format!("GET /v2/users/{c}"), 500, "Errors.Internal", 1);
    w.sweep_identities_once().await;
    assert!(!revoked(&w, ma) && !revoked(&w, mc));

    // Once the pass finds a user, the 404s are believed.
    let (_, revoked_count) = w.sweep_identities_once().await;
    assert_eq!(revoked_count, 2);
    assert!(revoked(&w, ma) && revoked(&w, mb) && !revoked(&w, mc));
}

// D7: the sweep marks the members it offboards (its write is the webhook's), and clears the
// mark of a marked member whose Zitadel user it reads as active again (reactivated or unlocked
// while no webhook arrived). A marked member still inactive, or unreadable, stays marked; the
// logins stay revoked.
#[tokio::test]
async fn the_sweep_marks_and_clears_the_offboarded_mark() {
    let (w, mock, db, main, tenant) = sweep_setup().await;
    let gone = mock.add_user("gone@example.test", "pw", true);
    let kept = mock.add_user("kept@example.test", "pw", true);
    let m_gone = login_holder(&w, db, main, tenant, &gone);
    login_holder(&w, db, main, tenant, &kept);
    mock.set_inactive(&gone, true);
    w.sweep_identities_once().await;
    assert!(
        offboarded(&w, m_gone),
        "the sweep's revocation marks the member"
    );

    // Back to active in Zitadel: the next pass clears the mark (no live login is left, so
    // only the mark brings the member into the pass).
    mock.set_inactive(&gone, false);
    w.sweep_identities_once().await;
    assert!(
        !offboarded(&w, m_gone),
        "the mark outlived the user's reactivation"
    );
    assert!(revoked(&w, m_gone), "the logins stay revoked");

    // Marked and still locked, or unreadable: the mark stays.
    let locked = mock.add_user("locked@example.test", "pw", true);
    let flaky = mock.add_user("flaky@example.test", "pw", true);
    let (m_locked, m_flaky) = {
        let mut state = w.metadata.state();
        let mut l = member(tenant, &locked);
        l.offboarded = true;
        let mut f = member(tenant, &flaky);
        f.offboarded = true;
        let ids = (l.id, f.id);
        state.members.push(l);
        state.members.push(f);
        ids
    };
    mock.lock(&locked);
    mock.fail(&format!("GET /v2/users/{flaky}"), 500, "Errors.Internal", 1);
    w.sweep_identities_once().await;
    assert!(offboarded(&w, m_locked));
    assert!(
        offboarded(&w, m_flaky),
        "an unreadable state cleared the mark"
    );
    w.sweep_identities_once().await;
    assert!(!offboarded(&w, m_flaky), "the next pass reads it active");
}
