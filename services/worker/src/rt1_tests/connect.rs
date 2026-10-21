//! P15/W21/S22: every database of a branch grants `CONNECT` to `chelabase_realtime` (a
//! `replication=database` login needs it, and an owner may `REVOKE CONNECT … FROM PUBLIC`): at
//! provisioning, at data API enable, and in the member-roles reconcile (the DDL hook's reaction
//! to a `CREATE DATABASE`), in the step that guards a new database, so a born-closed database
//! has it before it opens.

use chelabase_metadata::DataApiState;
use chelabase_nats::{DataApiDatabaseRequest, MemberRolesReply};
use chelabase_types::BranchId;

use crate::fakes::{FakeDbRow, FakeWorker, ready_database, worker};
use crate::member_roles::BRANCH_PROTECTED_CHECK_MARK;
use crate::realtime::{REALTIME_CONNECT_MARK, REALTIME_ROLE, realtime_connect_sql};

fn h(s: &str) -> String {
    hex::encode(s.as_bytes())
}

/// The grant every connect script carries for `database`.
fn grant(database: &str) -> String {
    format!(
        "GRANT CONNECT ON DATABASE \"{}\" TO {REALTIME_ROLE}",
        database.replace('"', "\"\"")
    )
}

/// `(index, script)` of every connect script `ep` ran, in order.
fn connect_scripts(w: &FakeWorker, ep: &str) -> Vec<(usize, String)> {
    w.computes
        .sql
        .lock()
        .unwrap()
        .iter()
        .enumerate()
        .filter(|(_, (e, _, s))| e == ep && s.starts_with(REALTIME_CONNECT_MARK))
        .map(|(i, (_, _, s))| (i, s.clone()))
        .collect()
}

/// Index of the first script `ep` ran containing `needle`.
fn first(w: &FakeWorker, ep: &str, needle: &str) -> Option<usize> {
    w.computes
        .sql
        .lock()
        .unwrap()
        .iter()
        .position(|(e, _, s)| e == ep && s.contains(needle))
}

/// A worker with the realtime password and a ready database `app`: its main branch and endpoint.
async fn ready() -> (FakeWorker, BranchId, String) {
    let (mut w, _journal) = worker();
    w.realtime_password = Some("rt-secret-1".into());
    let db = ready_database(&w).await;
    let main = w.metadata.main(db);
    (w, main.id, main.endpoint_id.unwrap())
}

#[test]
fn the_connect_script_grants_each_database_with_quoted_names() {
    let sql = realtime_connect_sql(&["app", "we\"ird", "Mixed Case"]);
    assert!(
        sql.starts_with(&format!("{REALTIME_CONNECT_MARK}\n")),
        "{sql}"
    );
    for db in ["app", "we\"ird", "Mixed Case"] {
        assert!(sql.contains(&grant(db)), "{db}: {sql}");
    }
    assert!(
        REALTIME_CONNECT_MARK.starts_with("-- chelabase:"),
        "a comment marker"
    );
}

/// `s` with every run of white space as one space.
fn squeezed(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// RT1 final fix round (batch B, T5 hardening): a sweep re-runs the script on every database
/// every time, so each grant runs only where the role doesn't already hold `CONNECT`
/// (`has_database_privilege`), inside one `DO` block whose dollar quote no database name can
/// close.
#[test]
fn each_grant_runs_only_where_the_role_lacks_connect() {
    let dbs = ["app", "it's", "we\"ird", "a$$b", "x$y$z"];
    let sql = realtime_connect_sql(&dbs);
    assert!(
        sql.starts_with(&format!("{REALTIME_CONNECT_MARK}\n")),
        "{sql}"
    );
    let flat = squeezed(&sql);
    // Coordinator ruling: the guard is realtime's own ACL entry on the database (PUBLIC's
    // CONNECT doesn't count), read with `aclexplode`.
    assert!(!sql.contains("has_database_privilege"), "{sql}");
    for db in dbs {
        let literal = format!("'{}'", db.replace('\'', "''"));
        let guarded_grant = format!("THEN {}; END IF;", grant(db));
        let end = flat
            .find(&guarded_grant)
            .unwrap_or_else(|| panic!("{db}: no guarded grant in {sql}"));
        let start = flat[..end]
            .rfind("IF NOT EXISTS (")
            .unwrap_or_else(|| panic!("{db}: no IF NOT EXISTS guard in {sql}"));
        let guard = &flat[start..end];
        for piece in [
            "pg_catalog.aclexplode(",
            "datacl",
            "datname",
            literal.as_str(),
            "grantee",
            &format!("'{REALTIME_ROLE}'"),
            "privilege_type",
            "'CONNECT'",
        ] {
            assert!(guard.contains(piece), "{db}: {piece} not in the guard {guard}");
        }
    }
    assert_eq!(
        sql.matches("GRANT CONNECT ON DATABASE").count(),
        dbs.len(),
        "one guarded grant per database: {sql}"
    );
    // One DO block; its dollar-quote tag appears exactly twice (open and close).
    let body = flat
        .split_once("DO ")
        .unwrap_or_else(|| panic!("a DO block: {sql}"))
        .1;
    let tag = body.split_whitespace().next().unwrap();
    assert!(tag.starts_with('$') && tag.ends_with('$'), "{tag}");
    assert_eq!(sql.matches(tag).count(), 2, "{tag} in {sql}");
}

#[tokio::test]
async fn provisioning_grants_connect_on_the_main_database_after_making_the_role() {
    let (w, _main, ep) = ready().await;
    let grants = connect_scripts(&w, &ep);
    let (at, script) = grants
        .iter()
        .find(|(_, s)| s.contains(&grant("app")))
        .cloned()
        .unwrap_or_else(|| panic!("no CONNECT on app: {grants:?}"));
    let role = first(&w, &ep, "CREATE ROLE chelabase_realtime").expect("the role was made");
    assert!(
        role < at,
        "the role exists before it's granted anything: {script}"
    );
}

#[tokio::test]
async fn enabling_the_data_api_grants_connect_on_that_database() {
    let (w, main, ep) = ready().await;
    w.metadata
        .state()
        .db_rows
        .entry(main)
        .or_default()
        .push(FakeDbRow::new(
            "analytics",
            DataApiState::Enabling,
            "public",
        ));
    w.computes.sql.lock().unwrap().clear();

    w.enable_data_api(&DataApiDatabaseRequest {
        branch_id: main,
        name: "analytics".into(),
        remove: false,
        grant_existing: false,
    })
    .await;

    let grants = connect_scripts(&w, &ep);
    assert!(
        grants.iter().any(|(_, s)| s.contains(&grant("analytics"))),
        "{grants:?}"
    );
}

/// A catalog answer: the owner, `databases` (open, owned by postgres), `closed` lines
/// `(name, allow_connections, template, born_closed)` and template1 as guard-only.
fn catalog(databases: &[&str], closed: &[(&str, bool, bool, bool)]) -> String {
    let flag = |b: bool| if b { "t" } else { "f" };
    let mut out = format!("r {} f t f t\n", h("postgres"));
    for db in databases {
        out.push_str(&format!("d {} {}\n", h(db), h("postgres")));
    }
    for (name, allow, template, born) in closed {
        out.push_str(&format!(
            "c {} {} {} {}\n",
            h(name),
            flag(*allow),
            flag(*template),
            flag(*born)
        ));
    }
    out.push_str(&format!("g {}\n", h("template1")));
    out
}

#[tokio::test]
async fn the_member_roles_reconcile_grants_connect_on_every_database() {
    let (w, main, ep) = ready().await;
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &catalog(&["app", "analytics", "postgres"], &[]),
    );
    w.computes.sql.lock().unwrap().clear();

    let reply = w.reconcile_member_roles(main, None).await;

    assert!(matches!(reply, MemberRolesReply::Done { .. }), "{reply:?}");
    let grants: Vec<String> = connect_scripts(&w, &ep)
        .into_iter()
        .map(|(_, s)| s)
        .collect();
    for db in ["app", "analytics"] {
        assert!(
            grants.iter().any(|s| s.contains(&grant(db))),
            "{db}: {grants:?}"
        );
    }
}

#[tokio::test]
async fn a_born_closed_database_gets_connect_before_it_opens() {
    let (w, main, ep) = ready().await;
    w.metadata
        .state()
        .branches
        .get_mut(&main)
        .unwrap()
        .protected = true;
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &catalog(&["app", "postgres"], &[("fresh", false, false, true)]),
    );
    w.computes.answer_at(&ep, BRANCH_PROTECTED_CHECK_MARK, "on");
    w.computes.sql.lock().unwrap().clear();

    let reply = w.reconcile_member_roles(main, None).await;

    assert!(matches!(reply, MemberRolesReply::Done { .. }), "{reply:?}");
    let granted = connect_scripts(&w, &ep)
        .into_iter()
        .find(|(_, s)| s.contains(&grant("fresh")))
        .map(|(i, _)| i)
        .unwrap_or_else(|| {
            panic!(
                "no CONNECT on the born-closed database: {:?}",
                w.computes.sql.lock().unwrap()
            )
        });
    let opened =
        first(&w, &ep, "ALTER DATABASE \"fresh\" ALLOW_CONNECTIONS true").expect("it was opened");
    assert!(granted < opened, "granted at {granted}, opened at {opened}");
}

#[tokio::test]
async fn without_a_password_the_member_reconcile_grants_nothing() {
    // Guard: no role, nothing to grant.
    let (w, _journal) = worker();
    let db = ready_database(&w).await;
    let main = w.metadata.main(db);
    let ep = main.endpoint_id.unwrap();
    w.computes.answer_at(
        &ep,
        "chelabase: member roles",
        &catalog(&["app", "postgres"], &[]),
    );
    w.reconcile_member_roles(main.id, None).await;
    assert!(connect_scripts(&w, &ep).is_empty());
}
