//! T9 I2 (the RT3-11 retry), I4 (open user templates), m1 (unprotected closed databases join
//! the unguard loop, unguard before open), m2 (`ResetMarker` needs the guard), m3 (the marker
//! is a Postgres bool), m5 (the wiring).
//!
//! **The invariant** (RT3-5, U2, D5), tested as an exhaustive table below: on a protected
//! branch, every database a non-superuser can connect to carries the current guard; a database
//! is opened (and a marker dropped from an open one) only after its guard was installed in this
//! run; a closed non-template database gets the guard but is opened only when it carries the
//! born-closed marker; closed templates are left alone. On an unprotected branch nothing is
//! guarded or closed by us: every connectable-or-owner-closed database loses a guard it
//! carries, a born-closed database is opened (its guard removed first), a stale marker is
//! dropped, and no guard is ever installed.

use std::collections::BTreeSet;
use std::time::Duration;

use chelabase_nats::MemberRolesReply;

use super::{fresh, h, set_protected, sql_log};
use crate::fakes::FakeWorker;
use crate::member_roles::{
    BRANCH_PROTECTED_CHECK_MARK, MemberCatalog, member_catalog_sql, parse_member_catalog,
};
use crate::resync::PendingResyncs;
use crate::schema_guard::{
    GUARD_PRESENT_MARK, GuardSettings, UNGUARD_MARK, schema_guard_sql, schema_unguard_sql,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    /// `d`: open, not a template.
    Open,
    /// `c f f f`: closed by its owner.
    OwnerClosed,
    /// `c f f t`: closed and marked `neon.born_closed` (a database made on a protected branch).
    BornClosed,
    /// `d` and `c t f t`: open with a stale marker.
    OpenMarked,
    /// `t`: an open user template (`datistemplate AND datallowconn`, not `template1`).
    OpenTemplate,
    /// `c f t f`: a closed template (`template0`, or a user's).
    ClosedTemplate,
    /// `c f t t`: a closed template that carries the marker.
    MarkedClosedTemplate,
}

const KINDS: [Kind; 7] = [
    Kind::Open,
    Kind::OwnerClosed,
    Kind::BornClosed,
    Kind::OpenMarked,
    Kind::OpenTemplate,
    Kind::ClosedTemplate,
    Kind::MarkedClosedTemplate,
];

fn db(kind: Kind) -> &'static str {
    match kind {
        Kind::Open => "k_open",
        Kind::OwnerClosed => "k_owner_closed",
        Kind::BornClosed => "k_born_closed",
        Kind::OpenMarked => "k_open_marked",
        Kind::OpenTemplate => "k_open_template",
        Kind::ClosedTemplate => "k_closed_template",
        Kind::MarkedClosedTemplate => "k_marked_closed_template",
    }
}

/// The catalog answer with one database per kind in `kinds` (and `template1`, which takes
/// connections, unless `without_template1`).
fn catalog(kinds: &[Kind], with_template1: bool) -> String {
    let mut out = format!("r {} f t f t\n", h("postgres"));
    for &kind in kinds {
        let n = h(db(kind));
        match kind {
            Kind::Open => out.push_str(&format!("d {n} {}\n", h("postgres"))),
            Kind::OwnerClosed => out.push_str(&format!("c {n} f f f\n")),
            Kind::BornClosed => out.push_str(&format!("c {n} f f t\n")),
            Kind::OpenMarked => {
                out.push_str(&format!("d {n} {}\nc {n} t f t\n", h("postgres")));
            }
            Kind::OpenTemplate => out.push_str(&format!("t {n}\n")),
            Kind::ClosedTemplate => out.push_str(&format!("c {n} f t f\n")),
            Kind::MarkedClosedTemplate => out.push_str(&format!("c {n} f t t\n")),
        }
    }
    if with_template1 {
        out.push_str(&format!("g {}\n", h("template1")));
    }
    out
}

fn guard_settings() -> GuardSettings {
    // `ready_database`'s branch: guard_logins on, no exemptions.
    GuardSettings {
        guard_logins: true,
        exempt: BTreeSet::new(),
    }
}

/// What a run left on the compute, read from its script log.
#[derive(Debug, Default)]
struct Seen {
    /// Databases the current guard was installed in.
    installed: BTreeSet<String>,
    /// Databases the guard was removed from.
    unguarded: BTreeSet<String>,
    /// Databases `ALTER DATABASE … ALLOW_CONNECTIONS true` ran for.
    opened: BTreeSet<String>,
    /// Databases whose marker alone was reset.
    marker_reset: BTreeSet<String>,
    /// Log positions: install and open (open comes later), unguard and open.
    install_at: std::collections::BTreeMap<String, usize>,
    unguard_at: std::collections::BTreeMap<String, usize>,
    open_at: std::collections::BTreeMap<String, usize>,
    /// Scripts that ran in a database or named one of `silent`.
    touched_silent: Vec<String>,
    any_install: bool,
    catalog_reads: usize,
    flag_checks: usize,
}

fn read_log(w: &FakeWorker, silent: &[&str]) -> Seen {
    let install = schema_guard_sql(&guard_settings());
    let unguard = schema_unguard_sql();
    let mut seen = Seen::default();
    for (i, (database, text)) in sql_log(w).into_iter().enumerate() {
        if text.contains("chelabase: member roles") {
            seen.catalog_reads += 1;
            continue;
        }
        if text.contains(BRANCH_PROTECTED_CHECK_MARK) {
            seen.flag_checks += 1;
            continue;
        }
        for name in silent {
            if database == *name || text.contains(&format!("\"{name}\"")) {
                seen.touched_silent.push(format!("{database}: {text}"));
            }
        }
        if text == install {
            seen.installed.insert(database.clone());
            seen.install_at.entry(database.clone()).or_insert(i);
            seen.any_install = true;
        } else if text == unguard {
            seen.unguarded.insert(database.clone());
            seen.unguard_at.entry(database.clone()).or_insert(i);
        } else if text.contains("CREATE EVENT TRIGGER") {
            seen.any_install = true;
        }
        for kind in KINDS {
            let quoted = format!("\"{}\"", db(kind));
            if text.contains(&format!("ALTER DATABASE {quoted} ALLOW_CONNECTIONS true")) {
                seen.opened.insert(db(kind).to_string());
                seen.open_at.entry(db(kind).to_string()).or_insert(i);
            } else if text.contains(&format!("ALTER DATABASE {quoted} RESET neon.born_closed")) {
                seen.marker_reset.insert(db(kind).to_string());
            }
        }
    }
    seen
}

fn set_of(dbs: impl IntoIterator<Item = &'static str>) -> BTreeSet<String> {
    dbs.into_iter().map(str::to_string).collect()
}

/// One run of the reconcile over `kinds` on a branch (protected or not), with `failing`
/// databases failing their guard install (protected) or their guard removal (unprotected).
async fn run(
    kinds: &[Kind],
    protected: bool,
    failing: &BTreeSet<&'static str>,
) -> (MemberRolesReply, Seen) {
    let (w, _journal, branch, ep) = fresh().await;
    set_protected(&w, branch, protected);
    w.computes
        .answer_at(&ep, "chelabase: member roles", &catalog(kinds, true));
    if protected {
        // The compute already carries the flag: nothing to push (the flag tests cover the rest).
        w.computes.answer_at(&ep, BRANCH_PROTECTED_CHECK_MARK, "on");
        for database in failing {
            w.computes.fail_in(database, "CREATE EVENT TRIGGER");
        }
    } else {
        // A guard from when the branch was protected sits in every database.
        w.computes.answer_at(&ep, GUARD_PRESENT_MARK, "t");
        for database in failing {
            w.computes.fail_in(database, UNGUARD_MARK);
        }
    }
    let reply = w.reconcile_member_roles(branch, None).await;
    let silent: Vec<&str> = [Kind::ClosedTemplate, Kind::MarkedClosedTemplate]
        .iter()
        .filter(|k| kinds.contains(k))
        .map(|k| db(*k))
        .collect();
    (reply, read_log(&w, &silent))
}

/// Every subset of the databases a run may fail in (the six kinds' databases and template1).
fn failure_sets() -> Vec<BTreeSet<&'static str>> {
    let pool: Vec<&'static str> = KINDS.iter().map(|k| db(*k)).chain(["template1"]).collect();
    (0u32..1 << pool.len())
        .map(|mask| {
            pool.iter()
                .enumerate()
                .filter(|(i, _)| mask & (1 << i) != 0)
                .map(|(_, d)| *d)
                .collect()
        })
        .collect()
}

#[tokio::test]
async fn protected_branch_table() {
    // Connectable (or openable) databases the guard goes into, and template1.
    let guarded_kinds = [
        Kind::Open,
        Kind::OwnerClosed,
        Kind::BornClosed,
        Kind::OpenMarked,
        Kind::OpenTemplate,
    ];
    let attempted: BTreeSet<&'static str> = guarded_kinds
        .iter()
        .map(|k| db(*k))
        .chain(["template1"])
        .collect();
    for failing in failure_sets() {
        let (reply, seen) = run(&KINDS, true, &failing).await;
        let ctx = format!("failing {failing:?}");
        let ok: BTreeSet<&'static str> = attempted.difference(&failing).copied().collect();
        // Every connectable database carries the guard, except where its install failed.
        assert_eq!(
            seen.installed,
            set_of(ok.iter().copied()),
            "{ctx}: guards installed"
        );
        // A database opens only when it is born closed and its guard was installed in this run.
        let expected_open = if ok.contains(db(Kind::BornClosed)) {
            set_of([db(Kind::BornClosed)])
        } else {
            BTreeSet::new()
        };
        assert_eq!(seen.opened, expected_open, "{ctx}: databases opened");
        // The owner's close is respected: never opened (covered above); closed templates and
        // the marked template are untouched.
        assert!(
            seen.touched_silent.is_empty(),
            "{ctx}: closed templates were touched: {:?}",
            seen.touched_silent
        );
        // m2: a stale marker is dropped from an open database only once it is guarded.
        let expected_reset = if ok.contains(db(Kind::OpenMarked)) {
            set_of([db(Kind::OpenMarked)])
        } else {
            BTreeSet::new()
        };
        assert_eq!(seen.marker_reset, expected_reset, "{ctx}: markers reset");
        // Guard before open, for every opened database.
        for opened in &seen.opened {
            assert!(
                seen.install_at[opened] < seen.open_at[opened],
                "{ctx}: {opened} was opened before its guard: {seen:?}"
            );
        }
        assert!(
            seen.unguarded.is_empty(),
            "{ctx}: a protected branch removes no guard"
        );
        // The run reports a failed install.
        let failed = matches!(reply, MemberRolesReply::Failed { .. });
        assert_eq!(
            failed,
            !failing.is_empty() && failing.iter().any(|f| attempted.contains(f)),
            "{ctx}: reply {reply:?}"
        );
    }
}

#[tokio::test]
async fn unprotected_branch_table() {
    // Where a leftover guard is looked for (and removed): every database but the closed
    // templates; template1 follows when another database's guard went.
    let swept = [
        Kind::Open,
        Kind::OwnerClosed,
        Kind::BornClosed,
        Kind::OpenMarked,
        Kind::OpenTemplate,
    ];
    let attempted: BTreeSet<&'static str> =
        swept.iter().map(|k| db(*k)).chain(["template1"]).collect();
    for failing in failure_sets() {
        let (reply, seen) = run(&KINDS, false, &failing).await;
        let ctx = format!("failing {failing:?}");
        let ok: BTreeSet<&'static str> = attempted.difference(&failing).copied().collect();
        assert_eq!(
            seen.unguarded,
            set_of(ok.iter().copied()),
            "{ctx}: guards removed"
        );
        assert!(
            !seen.any_install,
            "{ctx}: an unprotected branch installs no guard"
        );
        assert!(
            seen.touched_silent.is_empty(),
            "{ctx}: closed templates were touched: {:?}",
            seen.touched_silent
        );
        // A born-closed database is opened (its guard, if any, removed first); the owner's
        // close stays; the marker of an open database goes.
        assert!(
            !seen.opened.contains(db(Kind::OwnerClosed)),
            "{ctx}: the owner's close is respected"
        );
        if !failing.contains(db(Kind::BornClosed)) {
            assert!(
                seen.opened.contains(db(Kind::BornClosed)),
                "{ctx}: born closed is opened: {seen:?}"
            );
        }
        assert_eq!(
            seen.opened
                .difference(&set_of([db(Kind::BornClosed)]))
                .count(),
            0,
            "{ctx}: only the born-closed one opens"
        );
        assert!(
            seen.marker_reset.contains(db(Kind::OpenMarked)),
            "{ctx}: a stale marker is dropped"
        );
        if let (Some(unguard), Some(open)) = (
            seen.unguard_at.get(db(Kind::BornClosed)),
            seen.open_at.get(db(Kind::BornClosed)),
        ) {
            assert!(unguard < open, "{ctx}: unguard runs before open");
        }
        let failed = matches!(reply, MemberRolesReply::Failed { .. });
        assert_eq!(
            failed,
            failing.iter().any(|f| attempted.contains(f)),
            "{ctx}: reply {reply:?}"
        );
    }
}

#[tokio::test]
async fn an_unprotected_closed_database_is_looked_at_for_a_leftover_guard() {
    // m1, alone: an owner-closed database that still holds a guard from when the branch was
    // protected loses it, and stays closed.
    let (reply, seen) = run(&[Kind::OwnerClosed], false, &BTreeSet::new()).await;
    assert!(matches!(reply, MemberRolesReply::Done { .. }), "{reply:?}");
    assert_eq!(seen.unguarded, set_of([db(Kind::OwnerClosed), "template1"]));
    assert!(seen.opened.is_empty() && seen.marker_reset.is_empty());
}

#[tokio::test]
async fn a_failed_install_keeps_a_born_closed_database_closed() {
    let failing: BTreeSet<&'static str> = [db(Kind::BornClosed)].into();
    let (reply, seen) = run(&[Kind::BornClosed], true, &failing).await;
    assert!(
        matches!(reply, MemberRolesReply::Failed { .. }),
        "{reply:?}"
    );
    assert!(seen.opened.is_empty(), "{seen:?}");
    // The next run (the install works now) opens it, after the guard.
    let (reply, seen) = run(&[Kind::BornClosed], true, &BTreeSet::new()).await;
    assert!(matches!(reply, MemberRolesReply::Done { .. }), "{reply:?}");
    assert!(seen.install_at[db(Kind::BornClosed)] < seen.open_at[db(Kind::BornClosed)]);
}

// --- The catalog (I4, m3) --------------------------------------------------------------------

#[test]
fn the_catalog_lists_open_user_templates() {
    let out = format!("t {}\nt {}\n", h("tpl_b"), h("tpl_a"));
    let catalog: MemberCatalog = parse_member_catalog(&out).unwrap();
    assert_eq!(
        catalog.open_templates,
        ["tpl_a", "tpl_b"],
        "sorted, by name"
    );
    assert!(
        catalog.databases.is_empty(),
        "member-role work keeps its list"
    );
    assert!(catalog.guard_only_databases.is_empty());
    assert!(
        parse_member_catalog(&format!("t {} extra", h("x"))).is_err(),
        "an extra field"
    );
    assert!(parse_member_catalog("t zz").is_err(), "not hex");
    assert!(parse_member_catalog("t").is_err(), "a missing name");
    let sql = member_catalog_sql();
    assert!(sql.contains("SELECT 't ' || "), "{sql}");
    // Open templates other than template1; template1 stays the `g` line.
    assert!(sql.contains("d.datistemplate AND d.datallowconn"), "{sql}");
    assert!(
        sql.contains("d.datname <> 'template1'") || sql.contains("d.datname != 'template1'"),
        "{sql}"
    );
}

#[test]
fn the_born_closed_marker_is_matched_as_a_postgres_bool() {
    // The C side reads the marker with `parse_bool`: on, true, yes, 1 (and prefixes),
    // case-insensitive. The catalog must not look for the single spelling `on`.
    let sql = member_catalog_sql();
    assert!(
        !sql.contains("neon.born_closed=on"),
        "an exact-spelling match misses 'true', 'ON', '1': {sql}"
    );
    assert!(sql.contains("neon.born_closed"), "{sql}");
    let lower = sql.to_lowercase();
    for spelling in ["'on'", "'true'", "'yes'", "'1'"] {
        assert!(lower.contains(spelling), "{spelling} accepted: {sql}");
    }
    assert!(lower.contains("lower("), "case-insensitive: {sql}");
}

// --- The RT3-11 retry (I2) -------------------------------------------------------------------

fn catalog_reads(w: &FakeWorker) -> usize {
    w.computes
        .sql
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, _, s)| s.contains("chelabase: member roles"))
        .count()
}

/// The DDL that asked for the run may not be visible yet (a `CREATE DATABASE` is forwarded
/// before it commits): an enqueued run that came from the DDL receiver and found no database
/// that is `born_closed AND NOT allow_connections` looks once more. A run that didn't come from
/// the DDL receiver never does.
#[tokio::test(start_paused = true)]
async fn the_ddl_retry_fires_only_for_ddl_runs_that_found_no_born_closed_database() {
    // The kinds a run can find, without the marked closed template.
    let kinds = [
        Kind::Open,
        Kind::OwnerClosed,
        Kind::BornClosed,
        Kind::OpenMarked,
        Kind::OpenTemplate,
        Kind::ClosedTemplate,
    ];
    for mask in 0u32..1 << kinds.len() {
        let present: Vec<Kind> = kinds
            .iter()
            .enumerate()
            .filter(|(i, _)| mask & (1 << i) != 0)
            .map(|(_, k)| *k)
            .collect();
        for protected in [false, true] {
            for from_ddl in [false, true] {
                let (w, _journal, branch, ep) = fresh().await;
                set_protected(&w, branch, protected);
                w.computes
                    .answer_at(&ep, "chelabase: member roles", &catalog(&present, false));
                w.computes.answer_at(
                    &ep,
                    BRANCH_PROTECTED_CHECK_MARK,
                    if protected { "on" } else { "off" },
                );
                let reply = w
                    .reconcile_member_roles_soon(
                        &PendingResyncs::default(),
                        branch,
                        Duration::from_secs(1),
                        from_ddl,
                    )
                    .await;
                assert!(
                    matches!(reply, MemberRolesReply::Done { .. }),
                    "{present:?}: {reply:?}"
                );
                let found = present.contains(&Kind::BornClosed);
                let expected = if from_ddl && protected && !found {
                    2
                } else {
                    1
                };
                assert_eq!(
                    catalog_reads(&w),
                    expected,
                    "{present:?} protected={protected} from_ddl={from_ddl}: catalog reads"
                );
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn the_ddl_retry_runs_once_not_forever() {
    let (w, _journal, branch, ep) = fresh().await;
    set_protected(&w, branch, true);
    w.computes.answer_at(&ep, BRANCH_PROTECTED_CHECK_MARK, "on");
    w.computes
        .answer_at(&ep, "chelabase: member roles", &catalog(&[], false));
    let reply = w
        .reconcile_member_roles_soon(
            &PendingResyncs::default(),
            branch,
            Duration::from_secs(1),
            true,
        )
        .await;
    assert!(matches!(reply, MemberRolesReply::Done { .. }));
    assert_eq!(catalog_reads(&w), 2);
}

/// Requests coalesce: a call that finds another waiting for the same branch returns at once
/// and the waiting run covers it. A DDL request that merges into a waiting non-DDL run still
/// gets its retry (the waiting run learns that the DDL receiver asked).
#[tokio::test(start_paused = true)]
async fn a_ddl_request_merged_into_a_waiting_run_keeps_its_retry() {
    let (w, _journal, branch, ep) = fresh().await;
    set_protected(&w, branch, true);
    w.computes.answer_at(&ep, BRANCH_PROTECTED_CHECK_MARK, "on");
    w.computes
        .answer_at(&ep, "chelabase: member roles", &catalog(&[], false));
    let pending = PendingResyncs::default();
    let delay = Duration::from_secs(10);
    let first = w.reconcile_member_roles_soon(&pending, branch, delay, false);
    let second = async {
        tokio::time::sleep(Duration::from_secs(1)).await;
        w.reconcile_member_roles_soon(&pending, branch, delay, true)
            .await
    };
    let (first, second) = tokio::join!(first, second);
    assert!(matches!(first, MemberRolesReply::Done { .. }));
    assert!(matches!(second, MemberRolesReply::Done { .. }));
    assert_eq!(catalog_reads(&w), 2, "the merged DDL request's retry ran");
}

/// Not the DDL receiver's request: the enqueue that every other publisher makes (a new branch,
/// the members' endpoints, hand-overs) runs once.
#[tokio::test(start_paused = true)]
async fn a_request_not_from_the_ddl_receiver_runs_once() {
    let (w, _journal, branch, ep) = fresh().await;
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &catalog(&[Kind::Open], false),
    );
    w.reconcile_member_roles_soon(
        &PendingResyncs::default(),
        branch,
        Duration::from_secs(1),
        false,
    )
    .await;
    assert_eq!(catalog_reads(&w), 1);
}

/// Hardening: the retry exists for `CREATE DATABASE` on a protected branch (the born-closed
/// rule); an unprotected branch's DDL run never looks twice.
#[tokio::test(start_paused = true)]
async fn an_unprotected_branch_never_retries_for_ddl() {
    let (w, _journal, branch, ep) = fresh().await;
    w.computes
        .answer_at(&ep, "chelabase: member roles", &catalog(&[], false));
    w.reconcile_member_roles_soon(
        &PendingResyncs::default(),
        branch,
        Duration::from_secs(1),
        true,
    )
    .await;
    assert_eq!(catalog_reads(&w), 1);
}
