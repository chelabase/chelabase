//! T10 fix (user ruling 2026-10-23: preview, then confirm). With a named successor S, a
//! role's `SECURITY DEFINER` routines and views without `security_invoker`, granted to someone
//! other than S, would run with S's rights after the move. The preview lists them with a
//! token; a delete to S must carry the current token, checked before anything changes and
//! again inside each database's transaction. `chelabase_orphaned` never needs one.

use chelabase_nats::{ExposureKind, SUBJECT_ROLE_DELETE_PREVIEW, exposure_token};

use super::*;
use crate::fakes::{FakeWorker, Journal, entries, position, ready_database, worker};

const Q: &str = QUARANTINE_ROLE;
/// [`exposure_sql`]'s marker, the fake's needle for its answers.
const MARK: &str = "/* chelabase: exposure read */";

/// `app` created (plain) on a ready main branch; the journal and SQL log emptied.
async fn with_app(w: &FakeWorker, journal: &Journal) -> (BranchId, String, String) {
    let id = ready_database(w).await;
    let main = w.metadata.main(id);
    let ep = main.endpoint_id.unwrap();
    let database = w.metadata.state().endpoints[&ep].databases[0].name.clone();
    assert_eq!(
        w.create_role(main.id, &RoleSpec::login("app", false)).await,
        RoleOutcome::Ok
    );
    journal.lock().unwrap().clear();
    w.computes.sql.lock().unwrap().clear();
    (main.id, ep, database)
}

fn sql(w: &FakeWorker) -> Vec<String> {
    w.computes
        .sql
        .lock()
        .unwrap()
        .iter()
        .map(|(_, _, s)| s.clone())
        .collect()
}

fn sql_on(w: &FakeWorker) -> Vec<(String, String)> {
    w.computes
        .sql
        .lock()
        .unwrap()
        .iter()
        .map(|(_, db, s)| (db.clone(), s.clone()))
        .collect()
}

fn has_row(w: &FakeWorker, ep: &str, name: &str) -> bool {
    w.metadata.state().endpoints[ep].logins.contains_key(name)
}

fn at(s: &str, needle: &str) -> usize {
    s.find(needle)
        .unwrap_or_else(|| panic!("{needle:?} missing from {s}"))
}

/// One line of [`exposure_sql`]'s output.
fn line(kind: &str, identity: &str, grantee: &str) -> String {
    format!(
        "exposure|{kind}|{}|{}\n",
        hex::encode(identity),
        hex::encode(grantee)
    )
}

fn exposure(database: &str, kind: ExposureKind, identity: &str, grantees: &[&str]) -> Exposure {
    Exposure {
        database: database.into(),
        kind,
        identity: identity.into(),
        grantees: grantees.iter().map(|g| g.to_string()).collect(),
    }
}

fn databases(names: &[&str]) -> String {
    names
        .iter()
        .map(|n| format!("{}\n", hex::encode(n)))
        .collect()
}

/// `app` exists in SQL; databases `app` and `other`; `heir` a valid successor; `app`
/// exposes a definer granted to anon (twice: two privileges) and a non-invoker view granted to
/// PUBLIC in `app`, and a procedure granted to `web` in `other`. Answers the exposures, sorted.
fn exposed(w: &FakeWorker) -> Vec<Exposure> {
    w.computes.answer(&exists_sql("app"), "1\n");
    w.computes
        .answer("encode(convert_to(datname", &databases(&["app", "other"]));
    w.computes.answer(&successor_check_sql("heir"), "t\n");
    let in_app = [
        line("view", "public.v", "PUBLIC"),
        line("function", "public.planted()", "anon"),
        line("function", "public.planted()", "anon"),
    ]
    .concat();
    w.computes.answer_in("app", MARK, &in_app);
    w.computes.answer_in(
        "other",
        MARK,
        &line("procedure", "public.p(integer)", "web"),
    );
    w.computes
        .answer_in("app", "REASSIGN OWNED BY", "count|2\n");
    w.computes
        .answer_in("other", "REASSIGN OWNED BY", "count|1\n");
    vec![
        exposure("app", ExposureKind::Function, "public.planted()", &["anon"]),
        exposure("app", ExposureKind::View, "public.v", &["PUBLIC"]),
        exposure(
            "other",
            ExposureKind::Procedure,
            "public.p(integer)",
            &["web"],
        ),
    ]
}

fn preview_of(successor: &str, exposures: Vec<Exposure>) -> DeletePreview {
    let confirm =
        (!exposures.is_empty() && successor != Q).then(|| exposure_token(successor, &exposures));
    DeletePreview {
        successor: successor.into(),
        exposures,
        confirm,
    }
}

fn changed_nothing(w: &FakeWorker, ep: &str) {
    let ran = sql(w);
    assert!(
        !ran.iter()
            .any(|s| s.contains("NOLOGIN") || s.contains("REASSIGN") || s.contains("DROP ROLE")),
        "{ran:?}"
    );
    assert!(has_row(w, ep, "app"));
}

// --- the SQL -------------------------------------------------------------------------------

#[test]
fn the_guard_text_is_pinned() {
    assert_eq!(EXPOSURES_CHANGED, "chelabase: exposures changed");
}

// The read: definer routines and non-invoker views (materialized ones included) the role
// owns, their ACLs (the default when NULL) exploded, the successor and the role itself not
// counted; names quoted as literals; none of the needles the delete's other steps are told
// apart by, and nothing that changes anything.
#[test]
fn the_exposure_read_covers_definers_and_non_invoker_views() {
    let s = exposure_sql("app", "heir");
    for needle in [
        MARK,
        "SET search_path = pg_catalog, pg_temp",
        "prosecdef",
        "prokind",
        "security_invoker",
        "relkind",
        "aclexplode",
        "acldefault",
        "reloptions",
        "'app'",
        "'heir'",
        "exposure|",
    ] {
        assert!(s.contains(needle), "{needle} missing: {s}");
    }
    for absent in [
        "REASSIGN OWNED BY",
        "FROM pg_database",
        "BEGIN;",
        "ALTER ",
        "REVOKE ",
        "GRANT ",
        "DROP ",
        EXPOSURES_CHANGED,
    ] {
        assert!(absent.is_empty() || !s.contains(absent), "{absent} in {s}");
    }
    assert!(!s.contains(&exists_sql("app")), "{s}");
    let hostile = exposure_sql("a'b", "c'd");
    assert!(
        hostile.contains("'a''b'") && hostile.contains("'c''d'"),
        "{hostile}"
    );
    assert!(
        !hostile.contains("'a'b'") && !hostile.contains("'c'd'"),
        "{hostile}"
    );
}

// The output: one line per (object, grantee), grouped, grantees sorted (byte order: PUBLIC
// first) and deduplicated, the list sorted; hostile identities survive the hex.
#[test]
fn exposure_output_parses() {
    assert_eq!(parse_exposures("app", "").unwrap(), vec![]);
    let odd = "\"x\ny\".\"v|w\"";
    let out = [
        line("view", odd, "web"),
        line("function", "public.f(integer)", "authenticated"),
        line("materialized_view", "public.m", "anon"),
        line("view", odd, "PUBLIC"),
        line("function", "public.f(integer)", "anon"),
        line("procedure", "public.p()", "anon"),
        line("function", "public.f(integer)", "anon"),
        "\n".to_string(),
    ]
    .concat();
    assert_eq!(
        parse_exposures("db", &out).unwrap(),
        vec![
            exposure(
                "db",
                ExposureKind::Function,
                "public.f(integer)",
                &["anon", "authenticated"]
            ),
            exposure("db", ExposureKind::Procedure, "public.p()", &["anon"]),
            exposure("db", ExposureKind::View, odd, &["PUBLIC", "web"]),
            exposure("db", ExposureKind::MaterializedView, "public.m", &["anon"]),
        ]
    );
    let good_id = hex::encode("public.f()");
    let good_to = hex::encode("anon");
    for bad in [
        format!("exposure|table|{good_id}|{good_to}\n"),
        format!("exposure|Function|{good_id}|{good_to}\n"),
        format!("exposure|function|zz|{good_to}\n"),
        format!("exposure|function|{good_id}|ff\n"),
        format!("exposure|function|{good_id}\n"),
        format!("exposure|function|{good_id}|{good_to}|x\n"),
        "count|1\n".to_string(),
        "hello\n".to_string(),
    ] {
        assert!(parse_exposures("db", &bad).is_err(), "{bad:?}");
    }
}

// The guard: inside the transaction, before the reassignment, for a named successor only;
// what it expects is part of the script (kind, identity and grantees), embedded safely; the
// plain script is the guard with nothing expected.
#[test]
fn the_guard_is_inside_the_transaction_for_a_named_successor() {
    assert!(!EXPOSURES_CHANGED.is_empty(), "the guard's text is pinned");
    let e = exposure("app", ExposureKind::Function, "public.planted()", &["anon"]);
    let s = role_delete_script_with("app", "heir", std::slice::from_ref(&e));
    let guard = at(&s, EXPOSURES_CHANGED);
    assert!(
        at(&s, "BEGIN;") < guard && guard < at(&s, "REASSIGN OWNED BY"),
        "{s}"
    );
    assert!(
        !s.contains(MARK),
        "the fake tells the read and the script apart: {s}"
    );
    assert!(s.trim_end().ends_with("COMMIT;"), "{s}");

    assert_eq!(
        role_delete_script("app", "heir"),
        role_delete_script_with("app", "heir", &[])
    );
    assert!(
        role_delete_script("app", "heir").contains(EXPOSURES_CHANGED),
        "none expected is still checked"
    );
    assert!(
        !role_delete_script_with("app", Q, std::slice::from_ref(&e)).contains(EXPOSURES_CHANGED)
    );
    assert_eq!(
        role_delete_script_with("app", Q, std::slice::from_ref(&e)),
        role_delete_script("app", Q)
    );

    let more = Exposure {
        grantees: vec!["anon".into(), "web".into()],
        ..e.clone()
    };
    let other = Exposure {
        identity: "public.other()".into(),
        ..e.clone()
    };
    let view = Exposure {
        kind: ExposureKind::View,
        ..e.clone()
    };
    let scripts = [
        role_delete_script_with("app", "heir", &[]),
        s.clone(),
        role_delete_script_with("app", "heir", &[more]),
        role_delete_script_with("app", "heir", &[other]),
        role_delete_script_with("app", "heir", &[view]),
    ];
    for (i, a) in scripts.iter().enumerate() {
        for b in &scripts[i + 1..] {
            assert_ne!(a, b, "the expected exposures shape the guard");
        }
    }
    let hostile = Exposure {
        identity: "x'); DROP TABLE t; --".into(),
        grantees: vec!["a'b".into()],
        ..e
    };
    let h = role_delete_script_with("app", "heir", &[hostile]);
    assert!(!h.contains("x'); DROP TABLE t; --"), "{h}");
}

#[test]
fn preview_outcomes_as_replies() {
    let p = preview_of(
        "heir",
        vec![exposure("app", ExposureKind::View, "public.v", &["PUBLIC"])],
    );
    let reply = role_reply(RoleOutcome::Preview(p.clone()));
    assert!(reply.ok && reply.code.is_none(), "{reply:?}");
    assert_eq!(reply.preview, Some(p.clone()));
    let reply = role_reply(RoleOutcome::Unconfirmed(p.clone()));
    assert!(!reply.ok, "{reply:?}");
    assert_eq!(reply.code.as_deref(), Some("exposure_unconfirmed"));
    assert!(
        reply.error.as_deref().is_some_and(|e| !e.is_empty()),
        "{reply:?}"
    );
    assert_eq!(reply.preview, Some(p));
}

// --- the preview ---------------------------------------------------------------------------

// Every connectable database is read; the list is sorted and grouped, the token the
// canonical hash; nothing changes: no lock, no NOLOGIN, no reassignment, the row kept.
#[tokio::test]
async fn the_preview_reads_every_database_and_changes_nothing() {
    let (w, journal) = worker();
    let (branch, ep, _db) = with_app(&w, &journal).await;
    let list = exposed(&w);

    let outcome = w.preview_role_delete(branch, "app", "heir").await;

    assert_eq!(outcome, RoleOutcome::Preview(preview_of("heir", list)));
    let ran = sql_on(&w);
    assert!(
        ran.contains(&("app".to_string(), exposure_sql("app", "heir"))),
        "{ran:?}"
    );
    assert!(
        ran.contains(&("other".to_string(), exposure_sql("app", "heir"))),
        "{ran:?}"
    );
    assert!(sql(&w).contains(&successor_check_sql("heir")), "{ran:?}");
    changed_nothing(&w, &ep);
    assert!(
        position(&journal, "metadata.lock_endpoint").is_none(),
        "{:?}",
        entries(&journal)
    );
}

#[tokio::test]
async fn an_empty_preview_has_no_token() {
    let (w, journal) = worker();
    let (branch, ep, _db) = with_app(&w, &journal).await;
    w.computes.answer(&exists_sql("app"), "1\n");
    w.computes
        .answer("encode(convert_to(datname", &databases(&["app"]));
    w.computes.answer(&successor_check_sql("heir"), "t\n");

    assert_eq!(
        w.preview_role_delete(branch, "app", "heir").await,
        RoleOutcome::Preview(DeletePreview {
            successor: "heir".into(),
            exposures: vec![],
            confirm: None
        })
    );
    changed_nothing(&w, &ep);
}

// The quarantine role's preview lists what it found, with no token (it never needs one), and
// checks no successor in SQL.
#[tokio::test]
async fn the_quarantine_preview_lists_without_a_token() {
    let (w, journal) = worker();
    let (branch, ep, _db) = with_app(&w, &journal).await;
    exposed(&w);

    let outcome = w.preview_role_delete(branch, "app", Q).await;

    let RoleOutcome::Preview(p) = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!((p.successor.as_str(), p.confirm.as_deref()), (Q, None));
    assert_eq!(p.exposures.len(), 3, "{p:?}");
    assert!(
        !sql(&w).iter().any(|s| s.contains("successor check")),
        "{:?}",
        sql(&w)
    );
    changed_nothing(&w, &ep);
}

// The delete's refusals, with nothing read: the successor rules, the owner, a missing role or
// branch; and a successor SQL refuses is refused before any exposure is read.
#[tokio::test]
async fn the_preview_refuses_as_the_delete_does() {
    let (w, journal) = worker();
    let (branch, ep, _db) = with_app(&w, &journal).await;
    for successor in [
        "app",
        OWNER_ROLE,
        "anon",
        "CHELABASE_ORPHANED",
        "cb_m_x",
        "",
    ] {
        let outcome = w.preview_role_delete(branch, "app", successor).await;
        assert!(
            matches!(outcome, RoleOutcome::Refused("invalid_successor", _)),
            "{successor:?}: {outcome:?}"
        );
    }
    assert!(matches!(
        w.preview_role_delete(branch, OWNER_ROLE, "heir").await,
        RoleOutcome::Refused("reserved_role_name", _)
    ));
    assert!(sql(&w).is_empty(), "{:?}", sql(&w));
    assert_eq!(
        w.preview_role_delete(branch, "ghost", "heir").await,
        RoleOutcome::NotFound
    );
    assert_eq!(
        w.preview_role_delete(BranchId::new(), "app", "heir").await,
        RoleOutcome::NotReady
    );

    w.computes.answer(&exists_sql("app"), "1\n");
    w.computes
        .answer("encode(convert_to(datname", &databases(&["app"]));
    w.computes.answer(&successor_check_sql("heir"), "f\n");
    let outcome = w.preview_role_delete(branch, "app", "heir").await;
    assert!(
        matches!(outcome, RoleOutcome::Refused("invalid_successor", _)),
        "{outcome:?}"
    );
    assert!(!sql(&w).iter().any(|s| s.contains(MARK)), "{:?}", sql(&w));
    changed_nothing(&w, &ep);
}

// A role already gone from SQL exposes nothing.
#[tokio::test]
async fn the_preview_of_a_role_gone_from_sql_is_empty() {
    let (w, journal) = worker();
    let (branch, _ep, _db) = with_app(&w, &journal).await;
    w.computes.answer(&successor_check_sql("heir"), "t\n");
    assert_eq!(
        w.preview_role_delete(branch, "app", "heir").await,
        RoleOutcome::Preview(DeletePreview {
            successor: "heir".into(),
            exposures: vec![],
            confirm: None
        })
    );
    assert!(!sql(&w).iter().any(|s| s.contains(MARK)), "{:?}", sql(&w));
}

// --- the delete with a token ---------------------------------------------------------------

// No token while something is exposed: refused with the current preview, before anything
// changes (no NOLOGIN, no sessions ended, no reassignment, the row kept). `delete_role` is
// the same as no token.
#[tokio::test]
async fn a_delete_without_the_token_is_unconfirmed_and_touches_nothing() {
    let (w, journal) = worker();
    let (branch, ep, _db) = with_app(&w, &journal).await;
    let list = exposed(&w);

    let outcome = w.delete_role_with(branch, "app", "heir", None).await;

    assert_eq!(
        outcome,
        RoleOutcome::Unconfirmed(preview_of("heir", list.clone()))
    );
    changed_nothing(&w, &ep);
    assert!(position(&journal, "metadata.delete_role_copy").is_none());
    assert_eq!(
        w.delete_role(branch, "app", "heir").await,
        RoleOutcome::Unconfirmed(preview_of("heir", list))
    );
    changed_nothing(&w, &ep);
}

// A stale token (another list, another successor's, garbage) is refused the same way.
#[tokio::test]
async fn a_wrong_token_is_unconfirmed() {
    let (w, journal) = worker();
    let (branch, ep, _db) = with_app(&w, &journal).await;
    let list = exposed(&w);
    let fewer = exposure_token("heir", &list[..2]);
    let for_q = exposure_token(Q, &list);
    for token in ["0".repeat(64), fewer, for_q, String::new()] {
        let outcome = w
            .delete_role_with(branch, "app", "heir", Some(&token))
            .await;
        assert_eq!(
            outcome,
            RoleOutcome::Unconfirmed(preview_of("heir", list.clone())),
            "{token:?}"
        );
    }
    changed_nothing(&w, &ep);
}

// The current token: each database runs the script guarding its own exposures, after they were
// read and before anything else; the delete is done and lists what it kept.
#[tokio::test]
async fn the_right_token_deletes_and_keeps_the_exposures() {
    let (w, journal) = worker();
    let (branch, ep, db) = with_app(&w, &journal).await;
    let list = exposed(&w);
    let token = exposure_token("heir", &list);

    let outcome = w
        .delete_role_with(branch, "app", "heir", Some(&token))
        .await;

    assert_eq!(
        outcome,
        RoleOutcome::Deleted(Reassigned {
            count: 3,
            definers: vec![],
            confirmed: list.clone()
        })
    );
    let ran = sql_on(&w);
    let pos = |d: &str, s: &str| {
        ran.iter()
            .position(|(rd, rs)| rd == d && rs == s)
            .unwrap_or_else(|| panic!("{s:?} in {d} missing from {ran:?}"))
    };
    let script_app = role_delete_script_with("app", "heir", &list[..2]);
    let script_other = role_delete_script_with("app", "heir", &list[2..]);
    let read_app = pos("app", &exposure_sql("app", "heir"));
    let read_other = pos("other", &exposure_sql("app", "heir"));
    let nologin = ran
        .iter()
        .position(|(_, s)| s.contains("NOLOGIN"))
        .expect("NOLOGIN");
    assert!(
        read_app < nologin && read_other < nologin,
        "read before anything changes: {ran:?}"
    );
    assert!(
        nologin < pos("app", &script_app) && nologin < pos("other", &script_other),
        "{ran:?}"
    );
    assert_eq!(
        ran.last(),
        Some(&(db, "DROP ROLE IF EXISTS \"app\";".to_string()))
    );
    assert!(!has_row(&w, &ep, "app"));
}

// A token while nothing is exposed (any token: it confirms nothing) doesn't block the delete.
#[tokio::test]
async fn a_token_with_nothing_exposed_is_ignored() {
    let (w, journal) = worker();
    let (branch, ep, _db) = with_app(&w, &journal).await;
    w.computes.answer(&exists_sql("app"), "1\n");
    w.computes
        .answer("encode(convert_to(datname", &databases(&["app"]));
    w.computes.answer(&successor_check_sql("heir"), "t\n");
    w.computes
        .answer_in("app", "REASSIGN OWNED BY", "count|1\n");

    let outcome = w
        .delete_role_with(branch, "app", "heir", Some(&"0".repeat(64)))
        .await;

    assert_eq!(
        outcome,
        RoleOutcome::Deleted(Reassigned {
            count: 1,
            definers: vec![],
            confirmed: vec![]
        })
    );
    assert!(
        sql(&w).contains(&role_delete_script_with("app", "heir", &[])),
        "{:?}",
        sql(&w)
    );
    assert!(!has_row(&w, &ep, "app"));
}

// Exposures that change between the check and a database's transaction fail that transaction
// with the guard's text: the delete answers a fresh preview (re-read), the row is kept and
// the role isn't dropped.
#[tokio::test]
async fn exposures_changed_inside_the_transaction_answer_unconfirmed() {
    assert!(!EXPOSURES_CHANGED.is_empty(), "the guard's text is pinned");
    let (w, journal) = worker();
    let (branch, ep, _db) = with_app(&w, &journal).await;
    let list = exposed(&w);
    let token = exposure_token("heir", &list);
    *w.computes.fail_sql_containing.lock().unwrap() = Some((
        "REASSIGN OWNED BY".into(),
        format!("ERROR:  {EXPOSURES_CHANGED}\nCONTEXT:  PL/pgSQL function inline_code_block"),
    ));

    let outcome = w
        .delete_role_with(branch, "app", "heir", Some(&token))
        .await;

    assert_eq!(outcome, RoleOutcome::Unconfirmed(preview_of("heir", list)));
    assert!(has_row(&w, &ep, "app"));
    assert!(
        !sql(&w).iter().any(|s| s.starts_with("DROP ROLE")),
        "{:?}",
        sql(&w)
    );
    let reads = sql(&w).iter().filter(|s| s.contains(MARK)).count();
    assert!(
        reads >= 3,
        "the exposures are read again after the guard fired: {reads}"
    );
}

// Any other failure of the script stays a failure (a retry is safe), never `Unconfirmed`.
#[tokio::test]
async fn other_script_failures_stay_failures() {
    let (w, journal) = worker();
    let (branch, ep, _db) = with_app(&w, &journal).await;
    let list = exposed(&w);
    let token = exposure_token("heir", &list);
    *w.computes.fail_sql_containing.lock().unwrap() = Some((
        "REASSIGN OWNED BY".into(),
        "ERROR:  canceling statement due to lock timeout".into(),
    ));

    let outcome = w
        .delete_role_with(branch, "app", "heir", Some(&token))
        .await;

    assert!(matches!(outcome, RoleOutcome::Failed(_)), "{outcome:?}");
    assert!(has_row(&w, &ep, "app"));
}

// The quarantine role never needs a token: no exposure is read, the plain quarantine script
// runs, nothing is listed as kept.
#[tokio::test]
async fn the_quarantine_role_never_needs_a_token() {
    let (w, journal) = worker();
    let (branch, ep, _db) = with_app(&w, &journal).await;
    exposed(&w);

    let outcome = w.delete_role_with(branch, "app", Q, None).await;

    assert_eq!(
        outcome,
        RoleOutcome::Deleted(Reassigned {
            count: 3,
            definers: vec![],
            confirmed: vec![]
        })
    );
    assert!(!sql(&w).iter().any(|s| s.contains(MARK)), "{:?}", sql(&w));
    assert!(
        sql(&w).contains(&role_delete_script("app", Q)),
        "{:?}",
        sql(&w)
    );
    assert!(!has_row(&w, &ep, "app"));
}

// --- the RPCs ------------------------------------------------------------------------------

// The preview subject answers `preview`; the delete subject reads `confirm`: without it
// `exposure_unconfirmed` with the current preview, with it done and listing what it kept.
#[tokio::test]
async fn the_preview_rpc_and_the_confirm_field() {
    let (w, journal) = worker();
    let (branch, _ep, _db) = with_app(&w, &journal).await;
    let list = exposed(&w);
    let deadline = Duration::from_secs(5);
    let request = |confirm: Option<String>| {
        serde_json::to_vec(&RoleDeleteRequest {
            branch_id: branch,
            name: "app".into(),
            successor: "heir".into(),
            confirm,
        })
        .unwrap()
    };

    let reply = w
        .role_rpc_reply(SUBJECT_ROLE_DELETE_PREVIEW, &request(None), deadline)
        .await;
    assert!(reply.ok, "{reply:?}");
    let preview = reply.preview.expect("the preview");
    assert_eq!(preview, preview_of("heir", list.clone()));

    let reply = w
        .role_rpc_reply(SUBJECT_ROLE_DELETE, &request(None), deadline)
        .await;
    assert_eq!(
        reply.code.as_deref(),
        Some("exposure_unconfirmed"),
        "{reply:?}"
    );
    assert_eq!(reply.preview.as_ref(), Some(&preview));

    let reply = w
        .role_rpc_reply(
            SUBJECT_ROLE_DELETE,
            &request(preview.confirm.clone()),
            deadline,
        )
        .await;
    assert!(reply.ok, "{reply:?}");
    assert_eq!(reply.reassigned.map(|r| r.confirmed), Some(list));
}
