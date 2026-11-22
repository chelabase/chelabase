//! Live checks of read-only runs (`read_only: true`) against a scratch database on the dev
//! metadata Postgres (the workbench stack). One scratch database per test.
//!
//! [`Scratch::run_ro`] follows the route: the script is parsed first
//! ([`read_only::check`]) and a refused script never connects. [`Scratch::run_slices`] is the
//! seam that hands the run engine slices the parse would never produce, so Postgres's own
//! layer is checked without the parse in front of it.

use std::time::{Duration, Instant};

use chelabase_types::BranchId;
use futures_util::StreamExt;
use orchestrator::sql_editor::events::{Outcome, RunEvent};
use orchestrator::sql_editor::read_only::{self, Refused};
use orchestrator::sql_editor::registry::{Canceller, Cancelled, RunGuard, RunRegistry};
use orchestrator::sql_editor::run::{self, Finished, RunSpec, RunTarget, RunTls};
use orchestrator::sql_editor::{APPLICATION_NAME, MAX_RESPONSE_BYTES, MAX_ROWS_PER_STATEMENT};
use tokio_postgres::NoTls;
use uuid::Uuid;

/// SQLSTATE `read_only_sql_transaction`.
const READ_ONLY: &str = "25006";
/// SQLSTATE `insufficient_privilege`.
const DENIED: &str = "42501";

fn admin_url() -> String {
    std::env::var("CHELABASE_METADATA_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".into())
}

async fn admin_connect() -> tokio_postgres::Client {
    let (client, conn) = tokio_postgres::connect(&admin_url(), NoTls)
        .await
        .expect("the metadata Postgres (is the workbench stack up?)");
    tokio::spawn(conn);
    client
}

/// The cluster's superuser, connected to `database`.
async fn admin_in(database: &str) -> tokio_postgres::Client {
    let mut config: tokio_postgres::Config = admin_url().parse().unwrap();
    config.dbname(database);
    let (client, conn) = config.connect(NoTls).await.expect("the metadata Postgres");
    tokio::spawn(conn);
    client
}

/// A scratch owner and database, with a table `t`, a sequence `s` and a function `w()` that
/// inserts into `t`; dropped by [`Scratch::drop`].
struct Scratch {
    admin: tokio_postgres::Client,
    /// The superuser in the scratch database.
    db: tokio_postgres::Client,
    target: RunTarget,
    registry: RunRegistry,
    branch: BranchId,
}

impl Scratch {
    async fn new() -> Self {
        let mut buf = [0u8; 4];
        getrandom::fill(&mut buf).unwrap();
        let suffix = format!("{:08x}", u32::from_le_bytes(buf));
        let database = format!("m5_ro_{suffix}");
        let owner = format!("m5_owner_{suffix}");
        let password = format!("pw_{suffix}");
        let admin = admin_connect().await;
        admin
            .batch_execute(&format!("CREATE ROLE {owner} LOGIN PASSWORD '{password}'"))
            .await
            .unwrap();
        admin
            .batch_execute(&format!("CREATE DATABASE {database} OWNER {owner}"))
            .await
            .unwrap();
        let db = admin_in(&database).await;
        db.batch_execute(&format!(
            "SET ROLE {owner}; \
             CREATE TABLE t (id int); \
             CREATE SEQUENCE s; \
             CREATE FUNCTION w() RETURNS int LANGUAGE sql AS 'INSERT INTO t VALUES (1) RETURNING id'; \
             RESET ROLE"
        ))
        .await
        .unwrap();
        let config: tokio_postgres::Config = admin_url().parse().unwrap();
        let host = match &config.get_hosts()[0] {
            tokio_postgres::config::Host::Tcp(h) => h.clone(),
            #[allow(unreachable_patterns)]
            _ => "metadata-db".into(),
        };
        let addr = std::net::ToSocketAddrs::to_socket_addrs(&format!(
            "{host}:{}",
            config.get_ports()[0]
        ))
        .unwrap()
        .next()
        .unwrap();
        Scratch {
            admin,
            db,
            target: RunTarget {
                addr,
                user: owner,
                password,
                database,
                tls: RunTls::Plain,
            },
            registry: RunRegistry::default(),
            branch: BranchId::new(),
        }
    }

    fn guard(&self) -> RunGuard {
        self.guard_for(Uuid::new_v4())
    }

    fn guard_for(&self, id: Uuid) -> RunGuard {
        self.registry
            .reserve("tester", id, None, self.branch, &self.target.database)
            .unwrap()
    }

    /// A read-only run as the route does it: parsed first, never connected when refused.
    async fn run_ro(&self, sql: &str) -> Result<Vec<RunEvent>, Refused> {
        self.run_ro_on(&self.target, sql, None, Duration::from_secs(30)).await
    }

    async fn run_ro_on(
        &self,
        target: &RunTarget,
        sql: &str,
        role: Option<&str>,
        timeout: Duration,
    ) -> Result<Vec<RunEvent>, Refused> {
        let statements = read_only::check(sql)?;
        Ok(self.run_slices_on(target, statements, role, timeout, self.guard()).await)
    }

    /// The seam: these slices run as they are, without the parse.
    async fn run_slices(&self, statements: &[&str]) -> Vec<RunEvent> {
        let statements = statements.iter().map(|s| (*s).to_owned()).collect();
        self.run_slices_on(&self.target, statements, None, Duration::from_secs(30), self.guard())
            .await
    }

    async fn run_slices_on(
        &self,
        target: &RunTarget,
        statements: Vec<String>,
        role: Option<&str>,
        timeout: Duration,
        guard: RunGuard,
    ) -> Vec<RunEvent> {
        self.start(target, statements, role, timeout, guard).await.collect().await
    }

    async fn start(
        &self,
        target: &RunTarget,
        statements: Vec<String>,
        role: Option<&str>,
        timeout: Duration,
        guard: RunGuard,
    ) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = RunEvent> + Send>> {
        let connected = run::connect(target).await.unwrap();
        let spec = RunSpec {
            sql: statements.join(";"),
            role: role.map(Into::into),
            timeout,
            after: None,
        };
        Box::pin(run::run_read_only_reporting(
            connected,
            spec,
            statements,
            guard,
            Finished::default(),
        ))
    }

    /// The editor's backends still connected to the scratch database.
    async fn editor_backends(&self) -> i64 {
        self.admin
            .query_one(
                "SELECT count(*) FROM pg_stat_activity WHERE datname = $1 AND application_name = $2",
                &[&self.target.database, &APPLICATION_NAME],
            )
            .await
            .unwrap()
            .get(0)
    }

    async fn assert_no_backend_within(&self, within: Duration) {
        let until = Instant::now() + within;
        loop {
            let left = self.editor_backends().await;
            if left == 0 {
                return;
            }
            assert!(Instant::now() < until, "{left} editor backend(s) still running");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn cancel(&self, id: Uuid) -> bool {
        self.registry.cancel(id, self.branch, Canceller::Admin) == Cancelled::Done
    }

    /// Rows in `t`, read by the superuser.
    async fn rows_in_t(&self) -> i64 {
        self.db.query_one("SELECT count(*) FROM t", &[]).await.unwrap().get(0)
    }

    async fn drop(self) {
        drop(self.db);
        self.admin
            .batch_execute(&format!("DROP DATABASE {} WITH (FORCE)", self.target.database))
            .await
            .unwrap();
        self.admin
            .batch_execute(&format!("DROP ROLE {}", self.target.user))
            .await
            .unwrap();
    }
}

fn outcome(events: &[RunEvent]) -> Outcome {
    match events.last() {
        Some(RunEvent::Done { outcome, .. }) => *outcome,
        other => panic!("the last event isn't done: {other:?}"),
    }
}

fn completes(events: &[RunEvent]) -> Vec<(usize, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            RunEvent::Complete { statement, tag, .. } => Some((*statement, tag.clone())),
            _ => None,
        })
        .collect()
}

fn rows_of(events: &[RunEvent], index: usize) -> Vec<Vec<Option<String>>> {
    events
        .iter()
        .filter_map(|e| match e {
            RunEvent::Rows { statement, rows } if *statement == index => Some(rows.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

/// The run's errors: (statement, SQLSTATE).
fn errors(events: &[RunEvent]) -> Vec<(usize, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            RunEvent::Error { statement, code, .. } => Some((*statement, code.clone())),
            _ => None,
        })
        .collect()
}

/// Whether any event names `statement`.
fn touches(events: &[RunEvent], index: usize) -> bool {
    events.iter().any(|e| match e {
        RunEvent::Columns { statement, .. }
        | RunEvent::Rows { statement, .. }
        | RunEvent::Complete { statement, .. }
        | RunEvent::Error { statement, .. } => *statement == index,
        _ => false,
    })
}

/// The run failed at `statement` with one of `codes`, and nothing ran after it.
#[track_caller]
fn failed_at(events: &[RunEvent], statement: usize, codes: &[&str], what: &str) {
    let errs = errors(events);
    assert_eq!(errs.len(), 1, "{what}: {events:?}");
    assert_eq!(errs[0].0, statement, "{what}: {events:?}");
    assert!(codes.contains(&errs[0].1.as_str()), "{what}: {events:?}");
    assert_eq!(outcome(events), Outcome::Error, "{what}");
    assert!(!touches(events, statement + 1), "{what}: {events:?}");
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_multi_statement_script_returns_every_result() {
    let s = Scratch::new().await;
    let events = s
        .run_ro(
            "SELECT 1 AS a, 'x'::text AS b; \
             SELECT g FROM generate_series(1, 3) g; ; \
             SHOW server_version; \
             EXPLAIN SELECT * FROM t; \
             WITH c AS (SELECT 2 AS n) SELECT n FROM c",
        )
        .await
        .unwrap();
    assert_eq!(outcome(&events), Outcome::Ok, "{events:?}");
    let tags: Vec<(usize, String)> = completes(&events);
    assert_eq!(
        tags,
        vec![
            (0, "SELECT 1".into()),
            (1, "SELECT 3".into()),
            (2, "SHOW".into()),
            (3, "EXPLAIN".into()),
            (4, "SELECT 1".into()),
        ]
    );
    assert_eq!(rows_of(&events, 0), vec![vec![Some("1".into()), Some("x".into())]]);
    assert_eq!(rows_of(&events, 1).len(), 3);
    assert_eq!(rows_of(&events, 2).len(), 1);
    assert_eq!(rows_of(&events, 4), vec![vec![Some("2".into())]]);
    // The event shape a normal run uses: columns before rows before complete, per statement.
    let columns = events.iter().filter(|e| matches!(e, RunEvent::Columns { .. })).count();
    assert_eq!(columns, 5);
    s.assert_no_backend_within(Duration::from_secs(2)).await;
    s.drop().await;
}

// The parse refuses these kinds before Postgres sees them (the unit tests and the route's
// 422); through the seam, Postgres's read-only transaction refuses each one itself.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn writes_are_refused() {
    let s = Scratch::new().await;
    let owner = &s.target.user;
    let writes = [
        "INSERT INTO t VALUES (1)".to_owned(),
        "UPDATE t SET id = 2".to_owned(),
        "DELETE FROM t".to_owned(),
        "TRUNCATE t".to_owned(),
        "CREATE TABLE u (a int)".to_owned(),
        "CREATE TEMP TABLE tmp (a int)".to_owned(),
        "CREATE TEMPORARY VIEW v AS SELECT 1".to_owned(),
        "ALTER TABLE t ADD COLUMN b int".to_owned(),
        "DROP TABLE t".to_owned(),
        "CREATE INDEX ON t (id)".to_owned(),
        "COMMENT ON TABLE t IS 'x'".to_owned(),
        format!("GRANT SELECT ON t TO {owner}"),
        format!("REVOKE SELECT ON t FROM {owner}"),
        // Through the allowed path: Postgres refuses the write inside a SELECT.
        "WITH x AS (INSERT INTO t VALUES (1) RETURNING id) SELECT * FROM x".to_owned(),
        "SELECT 1 INTO u".to_owned(),
        "EXPLAIN ANALYZE INSERT INTO t VALUES (1)".to_owned(),
    ];
    s.db.batch_execute("INSERT INTO t VALUES (7)").await.unwrap();
    for sql in &writes {
        let events = s.run_slices(&["SELECT 1", sql, "SELECT 2"]).await;
        failed_at(&events, 1, &[READ_ONLY], sql);
        assert_eq!(rows_of(&events, 0), vec![vec![Some("1".into())]], "{sql}");
    }
    // The allowed-path forms go through the parse too.
    for sql in &writes[13..] {
        let events = s.run_ro(sql).await.unwrap_or_else(|r| panic!("{sql}: {r:?}"));
        failed_at(&events, 0, &[READ_ONLY], sql);
    }
    // Nothing changed.
    let after: Vec<(i64, i32)> = s
        .db
        .query("SELECT count(*), max(id) FROM t", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    assert_eq!(after, vec![(1, 7)]);
    let objects: i64 = s
        .db
        .query_one(
            "SELECT count(*) FROM pg_class WHERE relname IN ('u', 'tmp', 'v') \
             OR (relname = 't' AND relnatts <> 1) \
             OR (relkind = 'i' AND relnamespace = 'public'::regnamespace)",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(objects, 0);
    let comment: Option<String> =
        s.db.query_one("SELECT obj_description('t'::regclass)", &[]).await.unwrap().get(0);
    assert_eq!(comment, None);
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_select_calling_a_writing_function_is_refused() {
    let s = Scratch::new().await;
    for sql in ["SELECT w()", "SELECT * FROM w()", "SELECT 1 WHERE w() = 1", "VALUES (w())"] {
        let events = s.run_ro(sql).await.unwrap();
        failed_at(&events, 0, &[READ_ONLY], sql);
    }
    assert_eq!(s.rows_in_t().await, 0);
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn nextval_is_refused() {
    let s = Scratch::new().await;
    for sql in ["SELECT nextval('s')", "SELECT setval('s', 100)", "SELECT setval('s', 5, false)"] {
        let events = s.run_ro(sql).await.unwrap();
        failed_at(&events, 0, &[READ_ONLY], sql);
    }
    let (last, called): (i64, bool) = {
        let row = s.db.query_one("SELECT last_value, is_called FROM s", &[]).await.unwrap();
        (row.get(0), row.get(1))
    };
    assert_eq!((last, called), (1, false));
    s.drop().await;
}

/// Statements that leave or change the transaction or the session, or aren't reads.
fn leaving_statements(owner: &str) -> Vec<String> {
    vec![
        "COMMIT".into(),
        "ROLLBACK".into(),
        "BEGIN".into(),
        "SAVEPOINT a".into(),
        "RELEASE SAVEPOINT a".into(),
        "PREPARE TRANSACTION 'm5'".into(),
        "SET TRANSACTION READ WRITE".into(),
        "SET default_transaction_read_only = off".into(),
        format!("SET ROLE {owner}"),
        format!("SET SESSION AUTHORIZATION {owner}"),
        "SET LOCAL statement_timeout = 0".into(),
        "RESET ALL".into(),
        "DISCARD ALL".into(),
        "COPY t TO STDOUT".into(),
        "LOCK TABLE t".into(),
        "ANALYZE t".into(),
        "VACUUM t".into(),
        "REINDEX TABLE t".into(),
        "CHECKPOINT".into(),
        "NOTIFY c".into(),
        "PREPARE p AS SELECT 1".into(),
        "DO $$ BEGIN INSERT INTO t VALUES (1); END $$".into(),
        "CALL p()".into(),
    ]
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_script_that_leaves_the_transaction_is_refused_whole() {
    let s = Scratch::new().await;
    // The first statement would leave a trace a later run could see if it ran: a session
    // advisory lock outlives a rolled-back transaction until the backend ends.
    let first = "SELECT pg_advisory_lock(4242)";
    for bad in leaving_statements(&s.target.user) {
        for (sql, index) in [
            (format!("{bad}; {first}; SELECT 2"), 0),
            (format!("{first}; {bad}; SELECT 2"), 1),
            (format!("{first}; SELECT 2; {bad}"), 2),
        ] {
            let refused = s.run_ro(&sql).await.expect_err(&sql);
            assert_eq!(refused.statement, index, "{sql}: {refused:?}");
            assert_ne!(refused.kind, read_only::UNPARSED, "{sql}");
        }
    }
    // Nothing connected, so nothing ran: no editor backend, the lock is free, `t` is empty.
    assert_eq!(s.editor_backends().await, 0);
    let free: bool = s
        .db
        .query_one("SELECT pg_try_advisory_lock(4242)", &[])
        .await
        .unwrap()
        .get(0);
    assert!(free);
    assert_eq!(s.rows_in_t().await, 0);
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_boundary_mismatch_is_refused_not_run() {
    let s = Scratch::new().await;
    // `;` and a write hidden where a careless splitter would cut: each stays one statement,
    // and runs as the read it is.
    for (sql, want) in [
        ("SELECT $$; COMMIT; INSERT INTO t VALUES (1); $$ AS v", "; COMMIT; INSERT INTO t VALUES (1); "),
        ("SELECT $x$ $$; COMMIT; $$ $x$ AS v", " $$; COMMIT; $$ "),
        ("SELECT 1 /* ; COMMIT; INSERT INTO t VALUES (1) */ AS v", "1"),
        ("SELECT 1 -- ; COMMIT; INSERT INTO t VALUES (1)\n AS v", "1"),
        ("SELECT '; COMMIT; INSERT INTO t VALUES (1)' AS v", "; COMMIT; INSERT INTO t VALUES (1)"),
        ("SELECT E'\\'; COMMIT; INSERT INTO t VALUES (1); --' AS v", "'; COMMIT; INSERT INTO t VALUES (1); --"),
        ("SELECT 1 AS \"; COMMIT; INSERT INTO t VALUES (1)\"", "1"),
    ] {
        let events = s.run_ro(sql).await.unwrap_or_else(|r| panic!("{sql}: {r:?}"));
        assert_eq!(outcome(&events), Outcome::Ok, "{sql}: {events:?}");
        assert_eq!(completes(&events), vec![(0, "SELECT 1".into())], "{sql}");
        assert_eq!(rows_of(&events, 0), vec![vec![Some(want.into())]], "{sql}");
    }
    // A comment or string the parse can't close is a refusal, never a guess.
    for sql in ["SELECT 1 /* ; COMMIT", "SELECT $$; COMMIT", "SELECT '; COMMIT"] {
        assert_eq!(s.run_ro(sql).await.expect_err(sql).kind, read_only::UNPARSED, "{sql}");
    }
    // Through the seam: a slice the server sees as two commands is refused by the prepare,
    // and neither command runs.
    for slice in [
        "SELECT 1; COMMIT",
        "SELECT 1; INSERT INTO t VALUES (1)",
        "COMMIT; INSERT INTO t VALUES (1)",
        "SELECT $$x$$; SET default_transaction_read_only = off",
    ] {
        let events = s.run_slices(&[slice, "INSERT INTO t VALUES (2)"]).await;
        failed_at(&events, 0, &["42601"], slice);
        assert!(rows_of(&events, 0).is_empty(), "{slice}: {events:?}");
    }
    // A lone transaction statement the parse would have refused, through the seam: it is one
    // command, so the prepare takes it, but the run stops at its tag before anything else
    // runs outside the transaction.
    for slice in ["COMMIT", "ROLLBACK", "END", "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE"] {
        let events = s.run_slices(&["SELECT 1", slice, "INSERT INTO t VALUES (3)"]).await;
        failed_at(&events, 1, &[READ_ONLY], slice);
    }
    assert_eq!(s.rows_in_t().await, 0);
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_function_cannot_commit_or_go_read_write() {
    let s = Scratch::new().await;
    s.db.batch_execute(&format!(
        "SET ROLE {0}; \
         CREATE FUNCTION do_commit() RETURNS int LANGUAGE plpgsql AS \
           $$ BEGIN INSERT INTO t VALUES (1); COMMIT; RETURN 1; END $$; \
         CREATE FUNCTION go_rw() RETURNS int LANGUAGE plpgsql AS \
           $$ BEGIN EXECUTE 'SET TRANSACTION READ WRITE'; INSERT INTO t VALUES (1); RETURN 1; END $$; \
         CREATE FUNCTION go_rw_local() RETURNS int LANGUAGE plpgsql AS \
           $$ BEGIN SET LOCAL transaction_read_only = off; INSERT INTO t VALUES (1); RETURN 1; END $$; \
         CREATE PROCEDURE p_commit() LANGUAGE plpgsql AS \
           $$ BEGIN INSERT INTO t VALUES (1); COMMIT; END $$; \
         RESET ROLE",
        s.target.user
    ))
    .await
    .unwrap();
    for sql in [
        "SELECT do_commit()",
        "SELECT go_rw()",
        "SELECT go_rw_local()",
        "SELECT set_config('transaction_read_only', 'off', true)",
        "SELECT set_config('transaction_read_only', 'off', false)",
        "SELECT set_config('transaction_read_only', 'off', false), w()",
    ] {
        let events = s.run_ro(sql).await.unwrap_or_else(|r| panic!("{sql}: {r:?}"));
        assert_eq!(errors(&events).len(), 1, "{sql}: {events:?}");
        assert_eq!(outcome(&events), Outcome::Error, "{sql}");
    }
    // The session default changes, the running transaction doesn't: the next statement is
    // still refused.
    let events = s
        .run_ro("SELECT set_config('default_transaction_read_only', 'off', false); SELECT w()")
        .await
        .unwrap();
    failed_at(&events, 1, &[READ_ONLY], "default_transaction_read_only");
    // A procedure that commits can't be called (refused before anything runs).
    assert_eq!(s.run_ro("CALL p_commit()").await.expect_err("call").kind, "CallStmt");
    assert_eq!(s.rows_in_t().await, 0);
    s.drop().await;
}

/// A member's login role (the act-as connection): NOINHERIT, may `SET ROLE` to its group.
struct MemberRole {
    target: RunTarget,
    group: String,
}

impl MemberRole {
    async fn new(s: &Scratch) -> Self {
        let user = format!("cb_m_{}", s.target.user);
        let group = format!("m5_group_{}", s.target.user);
        let password = format!("pw_{user}");
        s.admin
            .batch_execute(&format!(
                "CREATE ROLE {user} LOGIN NOINHERIT NOCREATEDB NOCREATEROLE NOBYPASSRLS NOREPLICATION \
                     PASSWORD '{password}'; \
                 CREATE ROLE {group} NOLOGIN; \
                 GRANT {group} TO {user} WITH INHERIT FALSE, SET TRUE, ADMIN FALSE"
            ))
            .await
            .unwrap();
        MemberRole {
            target: RunTarget {
                addr: s.target.addr,
                user,
                password,
                database: s.target.database.clone(),
                tls: RunTls::Plain,
            },
            group,
        }
    }

    async fn drop(self) {
        admin_connect()
            .await
            .batch_execute(&format!("DROP ROLE {}; DROP ROLE {}", self.target.user, self.group))
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn set_config_stays_within_the_members_reach() {
    let s = Scratch::new().await;
    let owner = s.target.user.clone();
    let m = MemberRole::new(&s).await;
    // The group may read and write `t`; a secret table only the owner can read.
    s.db.batch_execute(&format!(
        "SET ROLE {owner}; \
         CREATE TABLE secret (v text); INSERT INTO secret VALUES ('owner only'); \
         GRANT SELECT, INSERT ON t TO {group}; GRANT EXECUTE ON FUNCTION w() TO {group}; \
         RESET ROLE",
        group = m.group
    ))
    .await
    .unwrap();
    let timeout = Duration::from_secs(30);

    // The member's own login, acting as the group.
    let on_member = |sql: String| {
        let (s, m) = (&s, &m);
        async move { s.run_ro_on(&m.target, &sql, Some(&m.group), timeout).await.unwrap() }
    };
    let events = on_member(format!("SELECT set_config('role', '{owner}', false)")).await;
    failed_at(&events, 0, &[DENIED], "role to the owner");
    let events = on_member(format!("SELECT set_config('session_authorization', '{owner}', false)")).await;
    failed_at(&events, 0, &[DENIED], "session_authorization");
    // Back to the member role itself: within reach, still no secret and no write.
    let events = on_member(
        "SELECT set_config('role', 'none', false), current_user; SELECT * FROM secret".into(),
    )
    .await;
    assert_eq!(rows_of(&events, 0), vec![vec![Some("none".into()), Some(m.target.user.clone())]]);
    failed_at(&events, 1, &[DENIED], "secret as the member role");
    // As the group (which may insert), writes are still refused.
    let events = on_member("SELECT current_user; SELECT w()".into()).await;
    assert_eq!(rows_of(&events, 0), vec![vec![Some(m.group.clone())]]);
    failed_at(&events, 1, &[READ_ONLY], "insert as the group");

    // An Admin acting as the group on the owner's connection: `set_config` can return to the
    // owner (whom an Admin may act as anyway), never beyond, and nothing writes.
    s.admin.batch_execute(&format!("GRANT {} TO {owner}", m.group)).await.unwrap();
    let on_owner = |sql: String| {
        let (s, m) = (&s, &m);
        async move { s.run_ro_on(&s.target, &sql, Some(&m.group), timeout).await.unwrap() }
    };
    let events = on_owner(
        "SELECT current_user; SELECT set_config('role', 'none', false), current_user; SELECT w()".into(),
    )
    .await;
    assert_eq!(rows_of(&events, 0), vec![vec![Some(m.group.clone())]]);
    assert_eq!(rows_of(&events, 1), vec![vec![Some("none".into()), Some(owner.clone())]]);
    failed_at(&events, 2, &[READ_ONLY], "insert as the owner");
    let superuser: String = s.admin.query_one("SELECT current_user", &[]).await.unwrap().get(0);
    let events = on_owner(format!("SELECT set_config('session_authorization', '{superuser}', false)")).await;
    failed_at(&events, 0, &[DENIED], "session_authorization to the superuser");
    let events = on_owner(format!("SELECT set_config('role', '{superuser}', false)")).await;
    failed_at(&events, 0, &[DENIED], "role to the superuser");

    assert_eq!(s.rows_in_t().await, 0);
    s.admin.batch_execute(&format!("REVOKE {} FROM {owner}", m.group)).await.unwrap();
    s.db.batch_execute(&format!("DROP OWNED BY {}", m.group)).await.unwrap();
    s.drop().await;
    m.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn functions_with_lasting_effects_are_refused_or_denied() {
    let s = Scratch::new().await;
    let admin_pid: i32 = s.db.query_one("SELECT pg_backend_pid()", &[]).await.unwrap().get(0);
    for sql in [
        format!("SELECT pg_terminate_backend({admin_pid})"),
        format!("SELECT pg_cancel_backend({admin_pid})"),
        "SELECT pg_stat_reset()".into(),
        "SELECT pg_stat_reset_shared('bgwriter')".into(),
        "SELECT pg_create_logical_replication_slot('m5_slot', 'pgoutput')".into(),
        "SELECT pg_create_physical_replication_slot('m5_slot')".into(),
        "SELECT pg_reload_conf()".into(),
        "SELECT pg_switch_wal()".into(),
        "SELECT pg_rotate_logfile()".into(),
        "SELECT lo_create(0)".into(),
        "SELECT pg_read_file('postgresql.conf')".into(),
    ] {
        let events = s.run_ro(&sql).await.unwrap_or_else(|r| panic!("{sql}: {r:?}"));
        failed_at(&events, 0, &[READ_ONLY, DENIED, "55000"], &sql);
    }
    // The superuser's backend is alive, no slot exists, no large object was made.
    let alive: i32 = s.db.query_one("SELECT 1", &[]).await.unwrap().get(0);
    assert_eq!(alive, 1);
    let slots: i64 = s
        .admin
        .query_one("SELECT count(*) FROM pg_replication_slots WHERE slot_name = 'm5_slot'", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(slots, 0);
    let objects: i64 =
        s.db.query_one("SELECT count(*) FROM pg_largeobject_metadata", &[]).await.unwrap().get(0);
    assert_eq!(objects, 0);
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_silent_statement_still_gets_heartbeats() {
    let s = Scratch::new().await;
    let events = s
        .run_ro_on(&s.target, "SELECT pg_sleep(20); SELECT 1", None, Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(outcome(&events), Outcome::Ok, "{events:?}");
    assert!(events.iter().any(|e| matches!(e, RunEvent::Heartbeat)), "{events:?}");
    // A result past the row cap is streamed and capped, not collected.
    let n = MAX_ROWS_PER_STATEMENT * 5;
    let events = s.run_ro(&format!("SELECT g FROM generate_series(1, {n}) g")).await.unwrap();
    assert_eq!(rows_of(&events, 0).len(), MAX_ROWS_PER_STATEMENT);
    assert!(events.iter().any(|e| matches!(
        e,
        RunEvent::Complete { statement: 0, truncated: true, rows, .. } if *rows == n as u64
    )));
    assert!(events.iter().filter(|e| matches!(e, RunEvent::Rows { .. })).count() > 1);
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn an_error_stops_the_script() {
    let s = Scratch::new().await;
    let events = s.run_ro("SELECT 1; SELECT 1/0; SELECT pg_advisory_lock(99)").await.unwrap();
    assert_eq!(rows_of(&events, 0), vec![vec![Some("1".into())]]);
    // 0-based, as in a normal run: the second statement is `statement: 1`.
    failed_at(&events, 1, &["22012"], "division by zero");
    assert!(!touches(&events, 2));
    s.assert_no_backend_within(Duration::from_secs(2)).await;
    let free: bool = s.db.query_one("SELECT pg_try_advisory_lock(99)", &[]).await.unwrap().get(0);
    assert!(free, "the third statement ran");
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_cancelled_read_only_run_rolls_back() {
    let s = Scratch::new().await;
    // Cancelled.
    let id = Uuid::new_v4();
    let stream = s
        .start(&s.target, vec!["SELECT pg_sleep(30)".into()], None, Duration::from_secs(60), s.guard_for(id))
        .await;
    let scratch = &s;
    let (events, _) = tokio::join!(stream.collect::<Vec<_>>(), async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(scratch.cancel(id));
    });
    assert_eq!(outcome(&events), Outcome::Cancelled);
    s.assert_no_backend_within(Duration::from_secs(2)).await;
    // Timed out.
    let events = s
        .run_ro_on(&s.target, "SELECT 1; SELECT pg_sleep(10)", None, Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(outcome(&events), Outcome::Timeout, "{events:?}");
    s.assert_no_backend_within(Duration::from_secs(2)).await;
    // The response cap.
    let wide = "SELECT repeat('x', 10000) FROM generate_series(1, 1000)";
    let events = s.run_ro(&format!("{wide}; {wide}")).await.unwrap();
    assert_eq!(outcome(&events), Outcome::TruncatedResponse);
    let sent: usize = events[..events.len() - 1].iter().map(|e| e.to_line().len()).sum();
    assert!(sent <= MAX_RESPONSE_BYTES, "{sent}");
    s.assert_no_backend_within(Duration::from_secs(2)).await;
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn nothing_reaches_the_next_run() {
    let s = Scratch::new().await;
    // Session state a script can make without a top-level SET: a session setting through
    // `set_config`, a session advisory lock (outlives the rollback until the backend ends).
    let dirty = "SELECT set_config('search_path', 'nowhere', false), \
                 set_config('default_transaction_read_only', 'off', false), \
                 pg_advisory_lock(7)";
    let check = "SELECT current_setting('search_path'), \
                 current_setting('default_transaction_read_only'), \
                 current_setting('transaction_read_only')";
    let wide = "SELECT repeat('x', 10000) FROM generate_series(1, 1000)";
    let endings: Vec<(String, Duration, Outcome)> = vec![
        (dirty.to_owned(), Duration::from_secs(30), Outcome::Ok),
        (format!("{dirty}; SELECT 1/0"), Duration::from_secs(30), Outcome::Error),
        (format!("{dirty}; SELECT pg_sleep(10)"), Duration::from_secs(1), Outcome::Timeout),
        (format!("{dirty}; {wide}; {wide}"), Duration::from_secs(30), Outcome::TruncatedResponse),
    ];
    for (sql, timeout, want) in endings {
        let events = s.run_ro_on(&s.target, &sql, None, timeout).await.unwrap();
        assert_eq!(outcome(&events), want, "{sql}");
        s.assert_no_backend_within(Duration::from_secs(2)).await;
        let events = s.run_ro(check).await.unwrap();
        assert_eq!(
            rows_of(&events, 0),
            vec![vec![Some("\"$user\", public".into()), Some("off".into()), Some("on".into())]],
            "after {want:?}"
        );
        let free: bool = s.db.query_one("SELECT pg_try_advisory_lock(7)", &[]).await.unwrap().get(0);
        assert!(free, "after {want:?}");
        s.db.batch_execute("SELECT pg_advisory_unlock(7)").await.unwrap();
    }
    // Cancelled.
    let id = Uuid::new_v4();
    let stream = s
        .start(
            &s.target,
            vec![dirty.into(), "SELECT pg_sleep(30)".into()],
            None,
            Duration::from_secs(60),
            s.guard_for(id),
        )
        .await;
    let scratch = &s;
    let (events, _) = tokio::join!(stream.collect::<Vec<_>>(), async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(scratch.cancel(id));
    });
    assert_eq!(outcome(&events), Outcome::Cancelled);
    s.assert_no_backend_within(Duration::from_secs(2)).await;
    let free: bool = s.db.query_one("SELECT pg_try_advisory_lock(7)", &[]).await.unwrap().get(0);
    assert!(free, "after the cancel");
    s.drop().await;
}
