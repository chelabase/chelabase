//! The personal-login marker in the resync (F2, part 2; C3's latent finding at
//! `resync.rs:886-890`): a role the catalog has is recorded as `Personal` only when its row is
//! a personal login's, its name is one a personal login may take, and the catalog shows the
//! marker (SP22's predicate, read by the catalog read's `m` lines). An unmarked role is
//! recorded as `Sql` (an owner-made `alice` on a point-in-time child is never adopted as
//! alice's personal login with the owner's verifier). Second finding in the personal-login
//! area, so the rule is tested exhaustively.

use chelabase_metadata::{ResyncWrite, RoleOrigin};
use chelabase_types::role_name::PERSONAL_LOGIN_MARKER;

use super::personal::{NAMES, personal, row};
use super::{PENCIL, PREFIX_ONLY};
use crate::OWNER_ROLE;
use crate::fakes::{FakeLogin, worker};
use crate::resync::{Observed, ObservedRole, RepairGroup, parse_observed, plan_resync, resync_sql};

/// An `m` line as psql prints the catalog read's marker rows.
fn m_line(name: &str) -> String {
    format!("m|{}||||||||||\n", hex::encode(name))
}

fn d_line(name: &str, owner: &str) -> String {
    format!("d|{}||{}|f|||||||\n", hex::encode(name), hex::encode(owner))
}

fn present(name: &str, marked: bool) -> ObservedRole {
    ObservedRole {
        password: Some(PENCIL.into()),
        marked,
        ..ObservedRole::plain(name)
    }
}

#[test]
fn a_plain_observed_role_is_unmarked() {
    assert!(!ObservedRole::plain("alice").marked);
}

// SP22: the catalog read asks for the marker with exactly the one predicate K3, K6 and C3 share.
#[test]
fn the_catalog_read_has_marker_rows_with_the_pinned_predicate() {
    let sql = resync_sql("postgres");
    let m = sql.find("SELECT 'm'").expect(&sql);
    let marker_part = &sql[m..];
    for needle in [
        "pg_db_role_setting",
        "setdatabase = 0",
        &format!("'{PERSONAL_LOGIN_MARKER}='"),
    ] {
        assert!(marker_part.contains(needle), "{needle}: {sql}");
    }
    assert!(
        sql.contains("UNION ALL\nSELECT 'm'"),
        "a third branch of the one read: {sql}"
    );
    assert!(sql.ends_with("ORDER BY 1, 2;"), "{sql}");
}

#[test]
fn m_lines_mark_their_roles_whatever_the_order() {
    let alice = crate::resync::role_line(&ObservedRole::plain("alice"));
    let bob = crate::resync::role_line(&ObservedRole::plain("bob"));
    for out in [
        format!(
            "{}{}{alice}{bob}",
            d_line("app", "postgres"),
            m_line("alice")
        ),
        format!("{alice}{bob}{}", m_line("alice")),
        format!("{}{alice}{}{bob}", m_line("alice"), m_line("alice")),
        // A marked role the read leaves out (e.g. a reserved name) names no `r` line.
        format!("{}{alice}{bob}{}", m_line("alice"), m_line("ghost")),
    ] {
        let o = parse_observed(&out).unwrap_or_else(|e| panic!("{e:#}: {out}"));
        let marked: Vec<(&str, bool)> = o
            .roles
            .iter()
            .map(|r| (r.name.as_str(), r.marked))
            .collect();
        assert_eq!(marked, [("alice", true), ("bob", false)], "{out}");
    }
    let o = parse_observed(&format!("{alice}{bob}")).unwrap();
    assert!(
        o.roles.iter().all(|r| !r.marked),
        "no `m` line, nothing marked"
    );
}

#[test]
fn a_malformed_m_line_is_refused_without_echoing_it() {
    for bad in [
        "m|zz||||||||||",
        "m|||||||||||",
        "m|61|t|||||||||",
        "m|61||||||||||x",
        "m|61||62||||||||",
        "m|61",
        "m|61|||||||||||",
    ] {
        let err = parse_observed(bad).expect_err(bad).to_string();
        assert!(
            !err.contains(bad),
            "{bad}: the error echoes the line: {err}"
        );
    }
}

/// What a row of the invariant is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    None,
    Personal,
    PersonalPrefixOnly,
    PersonalNoVerifier,
    Sql,
    Api,
}

/// What the catalog shows of the role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    Absent,
    Unmarked,
    Marked,
}

fn upsert_origin(write: &ResyncWrite, name: &str) -> Option<RoleOrigin> {
    write
        .upserts
        .iter()
        .find(|u| u.name == name)
        .map(|u| u.origin)
}

fn marker_scripts<'a>(repairs: &'a [RepairGroup], name: &str) -> Vec<&'a RepairGroup> {
    repairs
        .iter()
        .filter(|g| {
            g.role == name
                && g.statements
                    .iter()
                    .any(|s| s.contains(PERSONAL_LOGIN_MARKER))
        })
        .collect()
}

// Invariant, exhaustive over {no row, personal (whole verifier, prefix-only, none), sql, api}
// × {absent, unmarked, marked} × every name kind, with an unrelated present role beside it:
// - the role is recorded as `Personal` exactly when its row is a personal login's, its name is
//   allowed, and the catalog shows it marked, or lost it and the row can make it again (a
//   whole verifier);
// - a marker script runs exactly for a lost personal login that can be made again;
// - a personal row that exists unmarked (or under a refused name) is recorded as `Sql`, with
//   no repair: never adopted, never re-marked, never touched;
// - a personal row is never removed;
// - the unrelated role is recorded as `Sql`, whatever is marked.
#[test]
fn personal_origin_needs_a_personal_row_an_allowed_name_and_the_marker_exhaustively() {
    let rows = [
        Row::None,
        Row::Personal,
        Row::PersonalPrefixOnly,
        Row::PersonalNoVerifier,
        Row::Sql,
        Row::Api,
    ];
    for (name, refused) in NAMES {
        for r in rows {
            // Round 2: the role also exists without a SCRAM password (the branch that skips a
            // reserved name with no password).
            for (seen, scram) in [
                (Seen::Absent, true),
                (Seen::Unmarked, true),
                (Seen::Marked, true),
                (Seen::Unmarked, false),
                (Seen::Marked, false),
            ] {
                let case = format!("{name:?} row {r:?} {seen:?} scram {scram}");
                let catalog = |marked: bool| ObservedRole {
                    password: scram.then(|| PENCIL.to_string()),
                    ..present(name, marked)
                };
                let copies: Vec<_> = match r {
                    Row::None => vec![],
                    Row::Personal => vec![personal(name, Some(PENCIL))],
                    Row::PersonalPrefixOnly => vec![personal(name, Some(PREFIX_ONLY))],
                    Row::PersonalNoVerifier => vec![personal(name, None)],
                    Row::Sql => vec![row(name, RoleOrigin::Sql, Some(PENCIL))],
                    Row::Api => vec![row(name, RoleOrigin::Api, Some(PENCIL))],
                };
                let mut roles = vec![present("bystander", true)];
                match seen {
                    Seen::Absent => {}
                    Seen::Unmarked => roles.push(catalog(false)),
                    Seen::Marked => roles.push(catalog(true)),
                }
                let observed = Observed {
                    roles,
                    databases: vec![],
                };

                let (repairs, write) = plan_resync(OWNER_ROLE, &copies, &observed);

                let personal_row = matches!(
                    r,
                    Row::Personal | Row::PersonalPrefixOnly | Row::PersonalNoVerifier
                );
                let can_make = r == Row::Personal;
                let adopted = personal_row
                    && !refused
                    && (seen == Seen::Marked || (seen == Seen::Absent && can_make));
                let origin = upsert_origin(&write, name);
                assert_eq!(
                    origin == Some(RoleOrigin::Personal),
                    adopted,
                    "{case}: recorded as {origin:?}: {write:?}"
                );
                let remade = personal_row && !refused && seen == Seen::Absent && can_make;
                assert_eq!(
                    marker_scripts(&repairs, name).len(),
                    usize::from(remade),
                    "{case}: {repairs:?}"
                );
                if personal_row && seen != Seen::Absent {
                    if !adopted {
                        assert_eq!(origin, Some(RoleOrigin::Sql), "{case}: {write:?}");
                    }
                    assert!(
                        repairs.iter().all(|g| g.role != *name),
                        "{case}: an existing personal login gets no repair: {repairs:?}"
                    );
                }
                if personal_row {
                    assert!(
                        !write.removals.contains(&name.to_string()),
                        "{case}: {write:?}"
                    );
                }
                assert_eq!(
                    upsert_origin(&write, "bystander"),
                    Some(RoleOrigin::Sql),
                    "{case}: a marked role with no personal row is not adopted"
                );
            }
        }
    }
}

// The owner's name: a personal row named as the owner is never adopted, marked or not.
#[test]
fn the_owner_is_never_adopted_as_a_personal_login() {
    for marked in [false, true] {
        let observed = Observed {
            roles: vec![present(OWNER_ROLE, marked)],
            databases: vec![],
        };
        let (repairs, write) =
            plan_resync(OWNER_ROLE, &[personal(OWNER_ROLE, Some(PENCIL))], &observed);
        assert!(repairs.is_empty(), "{repairs:?}");
        assert!(
            write
                .upserts
                .iter()
                .all(|u| u.origin != RoleOrigin::Personal),
            "{write:?}"
        );
    }
}

/// The whole path through the fakes: the catalog read shows `alice`, with an `m` line or
/// without; her row is a personal login's. The origin the resync records.
async fn origin_after_resync(marked: bool) -> RoleOrigin {
    let (w, journal) = worker();
    let id = crate::fakes::ready_database(&w).await;
    let main = w.metadata.main(id);
    let ep = main.endpoint_id.clone().unwrap();
    {
        let mut state = w.metadata.state();
        let endpoint = state.endpoints.get_mut(&ep).unwrap();
        endpoint.set_verifier("alice", Some(PENCIL));
        endpoint.logins.insert(
            "alice".into(),
            FakeLogin {
                origin: RoleOrigin::Personal,
                password_enc: None,
                pending: false,
                login: true,
                level: chelabase_metadata::RoleLevel::Plain,
                api_owner_level: false,
                attributes: Default::default(),
                attributes_pending: false,
                attributes_change_id: None,
            },
        );
    }
    let main_db = w.metadata.state().db_rows[&main.id]
        .iter()
        .find(|r| r.main)
        .unwrap()
        .name
        .clone();
    let mut out = crate::resync::role_line(&ObservedRole {
        password: Some(PENCIL.into()),
        ..ObservedRole::plain("alice")
    });
    if marked {
        out.push_str(&m_line("alice"));
    }
    out.push_str(&d_line(&main_db, OWNER_ROLE));
    w.computes.answer("FROM pg_authid", &out);
    journal.lock().unwrap().clear();

    w.resync(main.id).await.unwrap();

    let origin = w.metadata.state().endpoints[&ep].logins["alice"].origin;
    assert!(
        !w.computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .any(|(_, _, s)| s.contains("CREATE ROLE \"alice\"")),
        "an existing role is never made again"
    );
    origin
}

#[tokio::test(start_paused = true)]
async fn the_resync_records_an_unmarked_personal_row_as_sql() {
    assert_eq!(origin_after_resync(false).await, RoleOrigin::Sql);
}

#[tokio::test(start_paused = true)]
async fn the_resync_keeps_a_marked_personal_login_personal() {
    assert_eq!(origin_after_resync(true).await, RoleOrigin::Personal);
}
