//! R1: the `chelabase_realtime` role. The worker creates it as `cloud_admin` (`admin_sql`, so
//! the DDL receiver never sees it) with `LOGIN REPLICATION NOBYPASSRLS NOINHERIT CONNECTION
//! LIMIT 12`, its password as a SCRAM verifier of the secret (never the plain text), and
//! `logical_decoding_work_mem = '8MB'` as a role setting. The script is idempotent: provisioning
//! runs it, every compute (re)start runs it (a reset or restore brings back an older catalog),
//! and reconcile runs it on the branches it visits (the backfill of branches made before RT1,
//! and a rotated secret).

use chelabase_neon::scram;
use chelabase_types::BranchId;

use crate::fakes::{FakeWorker, ready_database, worker};
use crate::realtime::{REALTIME_ROLE, REALTIME_VERIFIER_MARK, realtime_role_sql};

const VERIFIER: &str = "SCRAM-SHA-256$4096:c2FsdHNhbHQ=$c3RvcmVka2V5:c2VydmVya2V5";

#[test]
fn the_role_sql_is_exact() {
    let expected = format!(
        "SET search_path = pg_catalog, pg_temp; DO $$
BEGIN
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'chelabase_realtime') THEN
    CREATE ROLE chelabase_realtime;
  END IF;
END
$$;
ALTER ROLE chelabase_realtime WITH LOGIN REPLICATION NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS NOINHERIT CONNECTION LIMIT 12 PASSWORD '{VERIFIER}';
ALTER ROLE chelabase_realtime SET logical_decoding_work_mem = '8MB';
"
    );
    assert_eq!(realtime_role_sql(Some(VERIFIER)), expected);
    assert_eq!(REALTIME_ROLE, "chelabase_realtime");
    // RT1 final fix round: without a verifier (the stored one already verifies the secret) the
    // script is the same, less the password.
    assert_eq!(
        realtime_role_sql(None),
        expected.replace(&format!(" PASSWORD '{VERIFIER}'"), "")
    );
}

#[test]
fn the_role_sql_is_the_same_every_time_and_quotes_its_verifier() {
    // Idempotent by construction: the same input renders the same script (create if missing,
    // then the attributes, password and setting set again).
    assert_eq!(
        realtime_role_sql(Some(VERIFIER)),
        realtime_role_sql(Some(VERIFIER))
    );
    let sql = realtime_role_sql(Some("SCRAM-SHA-256$x'y"));
    assert!(sql.contains("PASSWORD 'SCRAM-SHA-256$x''y';"), "{sql}");
    assert!(!sql.contains("x'y'"), "{sql}");
}

/// Every script `ep` ran that (re)makes the role, in order.
fn role_scripts(w: &FakeWorker, ep: &str) -> Vec<String> {
    w.computes
        .sql
        .lock()
        .unwrap()
        .iter()
        .filter(|(e, _, s)| e == ep && s.contains("CREATE ROLE chelabase_realtime"))
        .map(|(_, _, s)| s.clone())
        .collect()
}

/// The verifier a role script sets.
fn verifier_in(script: &str) -> String {
    let start = script.find("PASSWORD '").expect("a password") + "PASSWORD '".len();
    let len = script[start..].find('\'').expect("a closing quote");
    script[start..start + len].to_string()
}

fn main_ep(w: &FakeWorker, db: chelabase_types::DatabaseId) -> String {
    w.metadata
        .main(db)
        .endpoint_id
        .expect("main has an endpoint")
}

#[tokio::test]
async fn provisioning_creates_the_role_with_the_secrets_verifier() {
    let (mut w, _journal) = worker();
    w.realtime_password = Some("rt-secret-1".into());
    let db = ready_database(&w).await;
    let ep = main_ep(&w, db);

    let scripts = role_scripts(&w, &ep);
    assert_eq!(scripts.len(), 1, "made once at provisioning: {scripts:?}");
    let verifier = verifier_in(&scripts[0]);
    assert!(scram::verifies(&verifier, "rt-secret-1"), "{verifier}");
    assert_eq!(scripts[0], realtime_role_sql(Some(&verifier)));
    // As cloud_admin (admin_sql's default user), and the plain text never reaches the compute.
    let sql = w.computes.sql.lock().unwrap().clone();
    let opts = w.computes.sql_opts.lock().unwrap().clone();
    let at = sql
        .iter()
        .position(|(e, _, s)| e == &ep && s.contains("CREATE ROLE chelabase_realtime"))
        .unwrap();
    assert_eq!(opts[at].user, None, "run as cloud_admin");
    assert!(
        !opts[at].verbose_errors,
        "terse errors: the verifier never shows in a LINE excerpt"
    );
    assert!(
        sql.iter().all(|(_, _, s)| !s.contains("rt-secret-1")),
        "the plain-text password reached the compute"
    );
}

#[tokio::test]
async fn without_a_password_no_realtime_sql_runs() {
    // Guard: a worker without CHELABASE_REALTIME_PG_PASSWORD_FILE makes no role and grants
    // nothing (the fakes' default).
    let (w, _journal) = worker();
    ready_database(&w).await;
    let sql = w.computes.sql.lock().unwrap().clone();
    assert!(
        sql.iter().all(|(_, _, s)| !s.contains(REALTIME_ROLE)),
        "{sql:?}"
    );
}

#[tokio::test]
async fn a_new_branch_gets_the_role_with_the_current_password() {
    let (mut w, _journal) = worker();
    w.realtime_password = Some("rt-secret-1".into());
    let db = ready_database(&w).await;
    let dev = crate::fakes::ready_branch(&w, db, "dev", BranchId::from(db)).await;
    let ep = w.metadata.branch(dev).endpoint_id.unwrap();
    let scripts = role_scripts(&w, &ep);
    assert_eq!(scripts.len(), 1, "{scripts:?}");
    assert!(scram::verifies(&verifier_in(&scripts[0]), "rt-secret-1"));
}

#[tokio::test]
async fn a_restarted_compute_gets_the_role_again() {
    // A reset or restore brings back an older catalog: the restart re-applies the role.
    let (mut w, _journal) = worker();
    w.realtime_password = Some("rt-secret-1".into());
    let db = ready_database(&w).await;
    let ep = main_ep(&w, db);
    w.computes.sql.lock().unwrap().clear();
    w.computes.sql_opts.lock().unwrap().clear();

    w.recreate_compute(BranchId::from(db), &ep).await.unwrap();

    let scripts = role_scripts(&w, &ep);
    assert_eq!(scripts.len(), 1, "{scripts:?}");
    assert!(scram::verifies(&verifier_in(&scripts[0]), "rt-secret-1"));
}

#[tokio::test]
async fn reconcile_backfills_the_role_on_a_branch_made_before_it() {
    let (mut w, _journal) = worker();
    // Made before RT1: no role.
    let db = ready_database(&w).await;
    let ep = main_ep(&w, db);
    assert!(role_scripts(&w, &ep).is_empty());
    w.computes
        .answer("to_regclass('chelabase.schema_version')", "t\n");
    w.realtime_password = Some("rt-secret-1".into());

    w.reconcile_once().await.unwrap();

    let scripts = role_scripts(&w, &ep);
    assert_eq!(scripts.len(), 1, "backfilled once: {scripts:?}");
    assert!(scram::verifies(&verifier_in(&scripts[0]), "rt-secret-1"));
}

#[tokio::test]
async fn reconcile_applies_a_rotated_password() {
    let (mut w, _journal) = worker();
    w.realtime_password = Some("rt-secret-1".into());
    let db = ready_database(&w).await;
    let ep = main_ep(&w, db);
    w.computes
        .answer("to_regclass('chelabase.schema_version')", "t\n");
    w.realtime_password = Some("rt-secret-2".into());

    w.reconcile_once().await.unwrap();

    let scripts = role_scripts(&w, &ep);
    assert!(
        scripts.len() >= 2,
        "provisioning, then reconcile: {scripts:?}"
    );
    let last = verifier_in(scripts.last().unwrap());
    assert!(
        scram::verifies(&last, "rt-secret-2"),
        "the new password: {last}"
    );
    assert!(!scram::verifies(&last, "rt-secret-1"));
}

// --- RT1 final fix round (batch B, T5 hardening): the password is set only when the stored
// verifier doesn't verify the secret. Before the role script the worker reads the stored
// verifier (a script in `postgres` starting with `REALTIME_VERIFIER_MARK`, as cloud_admin; its
// output, trimmed, is the role's `rolpassword`, empty without a role or a password).

/// A worker that provisioned `app` with `rt-secret-1`, reconcile ready to visit it, and nothing
/// recorded since.
async fn provisioned() -> (FakeWorker, String) {
    let (mut w, _journal) = worker();
    w.realtime_password = Some("rt-secret-1".into());
    let db = ready_database(&w).await;
    let ep = main_ep(&w, db);
    w.computes
        .answer("to_regclass('chelabase.schema_version')", "t\n");
    w.computes.sql.lock().unwrap().clear();
    w.computes.sql_opts.lock().unwrap().clear();
    (w, ep)
}

#[tokio::test]
async fn an_unchanged_password_is_not_set_again() {
    let (w, ep) = provisioned().await;
    w.computes.answer(
        REALTIME_VERIFIER_MARK,
        &format!("{}\n", scram::verifier("rt-secret-1")),
    );

    w.reconcile_once().await.unwrap();

    let scripts = role_scripts(&w, &ep);
    assert_eq!(scripts.len(), 1, "{scripts:?}");
    assert_eq!(
        scripts[0],
        realtime_role_sql(None),
        "attributes and setting, no password"
    );
    // The read came first, in `postgres`, as cloud_admin.
    let sql = w.computes.sql.lock().unwrap().clone();
    let opts = w.computes.sql_opts.lock().unwrap().clone();
    let read = sql
        .iter()
        .position(|(e, _, s)| e == &ep && s.starts_with(REALTIME_VERIFIER_MARK))
        .expect("the stored verifier was read");
    let role = sql
        .iter()
        .position(|(e, _, s)| e == &ep && s.contains("CREATE ROLE chelabase_realtime"))
        .unwrap();
    assert!(read < role, "{sql:?}");
    assert_eq!(sql[read].1, "postgres");
    assert_eq!(opts[read].user, None, "as cloud_admin");
    assert!(
        sql[read].2.contains("pg_catalog.pg_authid") && sql[read].2.contains(REALTIME_ROLE),
        "{}",
        sql[read].2
    );
}

#[tokio::test]
async fn a_stored_verifier_of_another_password_is_replaced() {
    let (w, ep) = provisioned().await;
    w.computes.answer(
        REALTIME_VERIFIER_MARK,
        &format!("{}\n", scram::verifier("an-old-secret")),
    );

    w.reconcile_once().await.unwrap();

    let scripts = role_scripts(&w, &ep);
    assert_eq!(scripts.len(), 1, "{scripts:?}");
    assert!(scram::verifies(&verifier_in(&scripts[0]), "rt-secret-1"));
}

#[tokio::test]
async fn no_role_or_an_unreadable_verifier_sets_the_password() {
    // No row (empty output), a plain-text or MD5 password, garbage: all set the verifier.
    for stored in ["", "\n", "md5abcdef", "rt-secret-1\n", "SCRAM-SHA-256$garbage"] {
        let (w, ep) = provisioned().await;
        w.computes.answer(REALTIME_VERIFIER_MARK, stored);
        w.reconcile_once().await.unwrap();
        let scripts = role_scripts(&w, &ep);
        assert_eq!(scripts.len(), 1, "{stored:?}: {scripts:?}");
        assert!(
            scram::verifies(&verifier_in(&scripts[0]), "rt-secret-1"),
            "{stored:?}"
        );
    }
    // The read fails: the password is set anyway (fail toward a working role).
    let (w, ep) = provisioned().await;
    w.computes.fail_when(REALTIME_VERIFIER_MARK);
    w.reconcile_once().await.unwrap();
    let scripts = role_scripts(&w, &ep);
    assert_eq!(scripts.len(), 1, "{scripts:?}");
    assert!(scram::verifies(&verifier_in(&scripts[0]), "rt-secret-1"));
}

#[tokio::test]
async fn the_verifier_read_never_carries_the_secret() {
    let (w, _ep) = provisioned().await;
    w.computes.answer(
        REALTIME_VERIFIER_MARK,
        &format!("{}\n", scram::verifier("rt-secret-1")),
    );
    w.reconcile_once().await.unwrap();
    let sql = w.computes.sql.lock().unwrap().clone();
    assert!(
        sql.iter().all(|(_, _, s)| !s.contains("rt-secret-1")),
        "{sql:?}"
    );
    assert!(REALTIME_VERIFIER_MARK.starts_with("-- chelabase:"));
}
