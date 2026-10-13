//! Live checks of the SQL editor's run engine against a scratch database on the dev metadata
//! Postgres (the workbench stack).

use std::time::{Duration, Instant};

use chelabase_types::BranchId;
use chelabase_types::role_name::member_hand_over_sql;
use futures_util::StreamExt;
use orchestrator::sql_editor::events::{Outcome, RunEvent};
use orchestrator::sql_editor::registry::{Canceller, Cancelled, RunGuard, RunRegistry};
use orchestrator::sql_editor::run::{self, RunSpec, RunTarget};
use orchestrator::sql_editor::{APPLICATION_NAME, MAX_RESPONSE_BYTES, MAX_ROWS_PER_STATEMENT};
use tokio_postgres::NoTls;
use uuid::Uuid;

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

/// A scratch role and database it owns; dropped by [`Scratch::drop`].
struct Scratch {
    admin: tokio_postgres::Client,
    target: RunTarget,
    registry: RunRegistry,
    /// The branch every run of this scratch database is registered on.
    branch: BranchId,
}

impl Scratch {
    async fn new() -> Self {
        let mut buf = [0u8; 4];
        getrandom::fill(&mut buf).unwrap();
        let suffix = format!("{:08x}", u32::from_le_bytes(buf));
        let database = format!("t3_run_{suffix}");
        let owner = format!("t3_owner_{suffix}");
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
        let config: tokio_postgres::Config = admin_url().parse().unwrap();
        let host = match &config.get_hosts()[0] {
            tokio_postgres::config::Host::Tcp(h) => h.clone(),
            #[allow(unreachable_patterns)]
            _ => "metadata-db".into(),
        };
        // The host is a service name inside the stack, so resolve it.
        let addr = std::net::ToSocketAddrs::to_socket_addrs(&format!(
            "{host}:{}",
            config.get_ports()[0]
        ))
        .unwrap()
        .next()
        .unwrap();
        Scratch {
            admin,
            target: RunTarget {
                addr,
                user: owner,
                password,
                database,
            },
            registry: RunRegistry::default(),
            branch: BranchId::new(),
        }
    }

    fn guard(&self) -> RunGuard {
        self.registry
            .reserve(
                "tester",
                Uuid::new_v4(),
                None,
                self.branch,
                &self.target.database,
            )
            .unwrap()
    }

    async fn run(&self, sql: &str, role: Option<&str>, timeout: Duration) -> Vec<RunEvent> {
        let connected = run::connect(&self.target).await.unwrap();
        let spec = RunSpec {
            sql: sql.into(),
            role: role.map(Into::into),
            after: None,
            timeout,
        };
        run::run(connected, spec, self.guard()).collect().await
    }

    async fn run_ok(&self, sql: &str) -> Vec<RunEvent> {
        self.run(sql, None, Duration::from_secs(30)).await
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

    /// Fires the token of run `id`; false when no such run is in flight.
    fn cancel(&self, id: Uuid) -> bool {
        self.registry.cancel(id, self.branch, Canceller::Admin) == Cancelled::Done
    }

    /// Waits up to `within` for the editor's backends to go away.
    async fn assert_no_backend_within(&self, within: Duration) {
        let until = Instant::now() + within;
        loop {
            let left = self.editor_backends().await;
            if left == 0 {
                return;
            }
            assert!(
                Instant::now() < until,
                "{left} editor backend(s) still running"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Waits up to `within` for the run's registry slot to be freed.
    async fn assert_slot_freed_within(&self, _user: &str, id: Uuid, within: Duration) {
        let until = Instant::now() + within;
        // `cancel` returns false once the run's guard is gone.
        while self.cancel(id) {
            assert!(Instant::now() < until, "the run still holds its slot");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn drop(self) {
        self.admin
            .batch_execute(&format!(
                "DROP DATABASE {} WITH (FORCE)",
                self.target.database
            ))
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

fn completes(events: &[RunEvent]) -> Vec<(usize, String, u64, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            RunEvent::Complete {
                statement,
                tag,
                rows,
                truncated,
            } => Some((*statement, tag.clone(), *rows, *truncated)),
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

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn multi_statement_results_and_tags() {
    let s = Scratch::new().await;
    let events = s
        .run_ok(
            "SELECT 1 AS a, 'x'::text AS b, NULL::point AS p; \
             SELECT g FROM generate_series(1, 3) g; ; \
             CREATE TABLE t (id int)",
        )
        .await;
    let columns: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            RunEvent::Columns { statement, columns } => Some((
                *statement,
                columns
                    .iter()
                    .map(|c| (c.name.clone(), c.type_name.clone()))
                    .collect::<Vec<_>>(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        columns,
        vec![
            (
                0,
                vec![
                    ("a".into(), "int4".into()),
                    ("b".into(), "text".into()),
                    ("p".into(), String::new())
                ]
            ),
            (1, vec![("g".into(), "int4".into())]),
        ]
    );
    assert_eq!(
        rows_of(&events, 0),
        vec![vec![Some("1".into()), Some("x".into()), None]]
    );
    assert_eq!(rows_of(&events, 1).len(), 3);
    assert_eq!(
        completes(&events),
        vec![
            (0, "SELECT 1".into(), 1, false),
            (1, "SELECT 3".into(), 3, false),
            (2, "CREATE TABLE".into(), 0, false),
        ]
    );
    assert_eq!(outcome(&events), Outcome::Ok);
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn notice_arrives_before_done() {
    let s = Scratch::new().await;
    let events = s
        .run_ok("DO $$ BEGIN RAISE NOTICE 'hi'; PERFORM pg_sleep(1); END $$; SELECT 1")
        .await;
    let notice = events
        .iter()
        .position(|e| matches!(e, RunEvent::Notice { message, .. } if message == "hi"))
        .expect("a notice");
    let first_complete = events
        .iter()
        .position(|e| matches!(e, RunEvent::Complete { .. }))
        .unwrap();
    assert!(notice < first_complete, "{events:?}");
    assert!(matches!(
        &events[notice],
        RunEvent::Notice { severity, code, .. } if severity == "NOTICE" && code == "00000"
    ));
    assert_eq!(outcome(&events), Outcome::Ok);
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn error_position_and_statement_index() {
    let s = Scratch::new().await;
    let events = s.run_ok("SELECT 1; SELECT nope").await;
    let error = events
        .iter()
        .find_map(|e| match e {
            RunEvent::Error {
                statement,
                code,
                position,
                ..
            } => Some((*statement, code.clone(), *position)),
            _ => None,
        })
        .expect("an error event");
    // 1-based character offset of `nope` in the sent text.
    assert_eq!(error, (1, "42703".into(), Some(18)));
    assert_eq!(outcome(&events), Outcome::Error);
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn explicit_transaction_is_honoured() {
    let s = Scratch::new().await;
    let events = s
        .run_ok("BEGIN; CREATE TABLE kept (id int); COMMIT; BEGIN; CREATE TABLE gone (id int); ROLLBACK")
        .await;
    assert_eq!(outcome(&events), Outcome::Ok);
    let events = s
        .run_ok("SELECT to_regclass('kept') IS NOT NULL, to_regclass('gone') IS NULL")
        .await;
    assert_eq!(
        rows_of(&events, 0),
        vec![vec![Some("t".into()), Some("t".into())]]
    );
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn lone_vacuum_runs() {
    let s = Scratch::new().await;
    let events = s.run_ok("VACUUM").await;
    assert_eq!(completes(&events), vec![(0, "VACUUM".into(), 0, false)]);
    assert_eq!(outcome(&events), Outcome::Ok);
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn set_role_failure_is_an_error_event() {
    let s = Scratch::new().await;
    let events = s
        .run("SELECT 1", Some("no_such_role"), Duration::from_secs(10))
        .await;
    assert!(
        matches!(
            &events[..],
            [RunEvent::Error { statement: 0, code, .. }, RunEvent::Done { outcome: Outcome::Error, .. }]
                if code == "22023"
        ),
        "{events:?}"
    );
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn set_role_runs_as_that_role() {
    let s = Scratch::new().await;
    s.admin
        .batch_execute(&format!(
            "CREATE ROLE t3_reader_{0} NOLOGIN; GRANT t3_reader_{0} TO {0}",
            s.target.user
        ))
        .await
        .unwrap();
    let role = format!("t3_reader_{}", s.target.user);
    let events = s
        .run("SELECT current_user", Some(&role), Duration::from_secs(10))
        .await;
    assert_eq!(rows_of(&events, 0), vec![vec![Some(role.clone())]]);
    s.admin
        .batch_execute(&format!("DROP ROLE {role}"))
        .await
        .unwrap();
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn row_cap_truncates_but_completes() {
    let s = Scratch::new().await;
    let events = s.run_ok("SELECT generate_series(1, 5000)").await;
    assert_eq!(rows_of(&events, 0).len(), MAX_ROWS_PER_STATEMENT);
    assert!(events.iter().all(|e| match e {
        RunEvent::Rows { rows, .. } => rows.len() <= 200,
        _ => true,
    }));
    assert_eq!(
        completes(&events),
        vec![(0, "SELECT 5000".into(), 5000, true)]
    );
    assert_eq!(outcome(&events), Outcome::Ok);
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn deadline_ends_with_timeout() {
    let s = Scratch::new().await;
    let started = Instant::now();
    let events = s
        .run("SELECT pg_sleep(5)", None, Duration::from_millis(500))
        .await;
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(outcome(&events), Outcome::Timeout);
    assert!(
        !events.iter().any(|e| matches!(e, RunEvent::Error { .. })),
        "{events:?}"
    );
    s.assert_no_backend_within(Duration::from_secs(2)).await;
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn token_cancels_the_backend() {
    let s = Scratch::new().await;
    let connected = run::connect(&s.target).await.unwrap();
    let id = Uuid::new_v4();
    let guard = s
        .registry
        .reserve("tester", id, None, s.branch, &s.target.database)
        .unwrap();
    let spec = RunSpec {
        sql: "SELECT pg_sleep(30)".into(),
        role: None,
        after: None,
        timeout: Duration::from_secs(60),
    };
    let stream = run::run(connected, spec, guard);
    let started = Instant::now();
    let scratch = &s;
    let (events, _) = tokio::join!(stream.collect::<Vec<_>>(), async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(scratch.cancel(id));
    });
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(outcome(&events), Outcome::Cancelled);
    s.assert_no_backend_within(Duration::from_secs(2)).await;
    // The guard dropped with the run: the id is free again.
    assert!(!s.cancel(id));
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn client_drop_cancels_the_backend() {
    let s = Scratch::new().await;
    let connected = run::connect(&s.target).await.unwrap();
    let spec = RunSpec {
        sql: "SELECT pg_sleep(30)".into(),
        role: None,
        after: None,
        timeout: Duration::from_secs(60),
    };
    let id = Uuid::new_v4();
    let guard = s
        .registry
        .reserve("tester", id, None, s.branch, &s.target.database)
        .unwrap();
    let stream = run::run(connected, spec, guard);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(s.editor_backends().await, 1);
    drop(stream);
    s.assert_no_backend_within(Duration::from_secs(2)).await;
    s.assert_slot_freed_within("tester", id, Duration::from_secs(2))
        .await;
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn unread_stream_frees_its_slot_after_the_deadline() {
    let s = Scratch::new().await;
    let connected = run::connect(&s.target).await.unwrap();
    // 300 statements of 3 events each: far more than the event buffer holds.
    let spec = RunSpec {
        sql: "SELECT 1;".repeat(300),
        role: None,
        after: None,
        timeout: Duration::from_millis(500),
    };
    let id = Uuid::new_v4();
    let guard = s
        .registry
        .reserve("tester", id, None, s.branch, &s.target.database)
        .unwrap();
    // Held but never polled: a client that stays connected and stops reading.
    let stream = run::run(connected, spec, guard);
    tokio::time::sleep(Duration::from_millis(500)).await;
    s.assert_slot_freed_within("tester", id, Duration::from_secs(3))
        .await;
    s.assert_no_backend_within(Duration::from_secs(2)).await;
    drop(stream);
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn external_cancel_before_the_deadline_is_an_error() {
    let s = Scratch::new().await;
    let connected = run::connect(&s.target).await.unwrap();
    let spec = RunSpec {
        sql: "SELECT pg_sleep(30)".into(),
        role: None,
        after: None,
        timeout: Duration::from_secs(60),
    };
    let stream = run::run(connected, spec, s.guard());
    let admin = &s.admin;
    let database = s.target.database.clone();
    let (events, _) = tokio::join!(stream.collect::<Vec<_>>(), async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        admin
            .execute(
                "SELECT pg_cancel_backend(pid) FROM pg_stat_activity \
                 WHERE datname = $1 AND application_name = $2",
                &[&database, &APPLICATION_NAME],
            )
            .await
            .unwrap();
    });
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RunEvent::Error { code, .. } if code == "57014")),
        "{events:?}"
    );
    assert_eq!(outcome(&events), Outcome::Error);
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn response_cap_ends_with_truncated_response() {
    let s = Scratch::new().await;
    // Two statements of 1,000 rows of 10 kB: about 20 MB, twice the cap, in 2 MB `rows` events.
    let wide = "SELECT repeat('x', 10000) FROM generate_series(1, 1000)";
    let events = s.run_ok(&format!("{wide}; {wide}")).await;
    assert_eq!(outcome(&events), Outcome::TruncatedResponse);
    let sent: usize = events[..events.len() - 1]
        .iter()
        .map(|e| e.to_line().len())
        .sum();
    assert!(sent <= MAX_RESPONSE_BYTES, "{sent}");
    assert!(sent > MAX_RESPONSE_BYTES / 2, "{sent}");
    s.assert_no_backend_within(Duration::from_secs(2)).await;
    s.drop().await;
}

/// The bytes of the events before `done`.
fn sent_bytes(events: &[RunEvent]) -> usize {
    events[..events.len() - 1]
        .iter()
        .map(|e| e.to_line().len())
        .sum()
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn wide_rows_flush_before_the_cap() {
    let s = Scratch::new().await;
    // Ten 3 MB rows: the rows that fit go out before the run stops at the cap, rather than all
    // ten piling up in the orchestrator for one oversized `rows` event.
    let events = s
        .run_ok("SELECT repeat('x', 3000000) FROM generate_series(1, 10)")
        .await;
    assert_eq!(outcome(&events), Outcome::TruncatedResponse);
    let rows = rows_of(&events, 0).len();
    assert!((1..=3).contains(&rows), "{rows} rows sent");
    assert!(sent_bytes(&events) <= MAX_RESPONSE_BYTES);
    s.assert_no_backend_within(Duration::from_secs(2)).await;
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_row_past_the_cap_stops_the_run() {
    let s = Scratch::new().await;
    // 50 rows of 20 MB: the first row alone passes the cap; nothing past it is read.
    let started = Instant::now();
    let events = s
        .run_ok("SELECT repeat('x', 20000000) FROM generate_series(1, 50)")
        .await;
    assert_eq!(outcome(&events), Outcome::TruncatedResponse);
    assert!(rows_of(&events, 0).is_empty());
    assert!(sent_bytes(&events) <= MAX_RESPONSE_BYTES);
    assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
    s.assert_no_backend_within(Duration::from_secs(2)).await;
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn notice_flood_with_a_slow_reader_ends_at_the_cap() {
    let s = Scratch::new().await;
    // About 25 MB of notices while the reader dawdles: the notice queue stays bounded
    // (backpressure on the socket) and the run still ends at the cap, not at the deadline.
    let sql = "DO $$ BEGIN FOR i IN 1..200000 LOOP RAISE NOTICE '%', repeat('n', 100); END LOOP; END $$";
    let connected = run::connect(&s.target).await.unwrap();
    let spec = RunSpec {
        sql: sql.into(),
        role: None,
        after: None,
        timeout: Duration::from_secs(60),
    };
    let started = Instant::now();
    let mut stream = std::pin::pin!(run::run(connected, spec, s.guard()));
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        if events.len() < 200 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        events.push(event);
    }
    assert_eq!(outcome(&events), Outcome::TruncatedResponse);
    assert!(
        events[..events.len() - 1]
            .iter()
            .all(|e| matches!(e, RunEvent::Notice { .. }))
    );
    assert!(sent_bytes(&events) <= MAX_RESPONSE_BYTES);
    assert!(started.elapsed() < Duration::from_secs(30), "{:?}", started.elapsed());
    s.assert_no_backend_within(Duration::from_secs(2)).await;
    s.drop().await;
}

/// A member role as the reconciler makes it (`LOGIN NOINHERIT`, a group granted `WITH INHERIT
/// FALSE, SET TRUE`), for the scratch database; dropped by [`MemberRole::drop`] after the
/// scratch database is gone.
struct MemberRole {
    target: RunTarget,
    group: String,
}

impl MemberRole {
    async fn new(s: &Scratch) -> Self {
        let user = format!("cb_m_{}", s.target.user);
        let group = format!("t9_group_{}", s.target.user);
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
            },
            group,
        }
    }

    /// A run on the member role's connection, acting as the group (the editor's act-as run),
    /// with the hand-over the worker runs after it; returns once both are over.
    async fn run(&self, s: &Scratch, sql: &str) -> Vec<RunEvent> {
        let (stream, handed) = self.start(s, sql, Duration::from_secs(30), s.guard()).await;
        let events = stream.collect().await;
        handed.await.expect("the hand-over ran").expect("the hand-over succeeded");
        events
    }

    /// Starts an act-as run: its events, and the outcome of its hand-over (the worker's script,
    /// [`member_hand_over_sql`], run here as the superuser in the scratch database).
    async fn start(
        &self,
        s: &Scratch,
        sql: &str,
        timeout: Duration,
        guard: RunGuard,
    ) -> (
        std::pin::Pin<Box<dyn futures_util::Stream<Item = RunEvent> + Send>>,
        tokio::sync::oneshot::Receiver<Result<(), String>>,
    ) {
        self.start_as(s, sql, timeout, guard, &self.group.clone()).await
    }

    /// [`Self::start`], with `SET ROLE` to `set_role` (the hand-over still targets the group).
    async fn start_as(
        &self,
        _s: &Scratch,
        sql: &str,
        timeout: Duration,
        guard: RunGuard,
        set_role: &str,
    ) -> (
        std::pin::Pin<Box<dyn futures_util::Stream<Item = RunEvent> + Send>>,
        tokio::sync::oneshot::Receiver<Result<(), String>>,
    ) {
        let connected = run::connect(&self.target).await.unwrap();
        let backend = (connected.backend_pid(), connected.backend_start().to_owned());
        let hand_over = member_hand_over_sql(
            &self.target.user,
            Some(&self.group),
            Some((backend.0, &backend.1)),
        );
        let database = self.target.database.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let after: run::AfterRun = Box::pin(async move {
            let result = admin_in(&database)
                .await
                .batch_execute(&hand_over)
                .await
                .map_err(|e| format!("{e:?}"));
            let _ = tx.send(result);
        });
        let spec = RunSpec {
            sql: sql.into(),
            role: Some(set_role.to_owned()),
            timeout,
            after: Some(after),
        };
        (Box::pin(run::run(connected, spec, guard)), rx)
    }

    async fn drop(self) {
        admin_connect()
            .await
            .batch_execute(&format!("DROP ROLE {}; DROP ROLE {}", self.target.user, self.group))
            .await
            .unwrap();
    }
}

fn errors(events: &[RunEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            RunEvent::Error { code, .. } => Some(code.clone()),
            _ => None,
        })
        .collect()
}

// The hole T9 closes: a script that resets its role lands on the member role, never on the
// owner, and can't become the owner from there.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn script_cannot_reset_to_owner() {
    let s = Scratch::new().await;
    let m = MemberRole::new(&s).await;
    let events = m.run(&s, "SELECT current_user; RESET ROLE; SELECT current_user, session_user").await;
    assert_eq!(rows_of(&events, 0), vec![vec![Some(m.group.clone())]]);
    assert_eq!(
        rows_of(&events, 2),
        vec![vec![Some(m.target.user.clone()), Some(m.target.user.clone())]]
    );
    assert_ne!(m.target.user, s.target.user);
    for sql in [
        format!("SET ROLE {}", s.target.user),
        format!("RESET ROLE; SET ROLE {}", s.target.user),
        format!("SET SESSION AUTHORIZATION {}", s.target.user),
        format!("SET ROLE NONE; SET ROLE {}", s.target.user),
    ] {
        let events = m.run(&s, &sql).await;
        assert_eq!(errors(&events), vec!["42501".to_string()], "{sql}: {events:?}");
        assert_eq!(outcome(&events), Outcome::Error, "{sql}");
    }
    s.drop().await;
    m.drop().await;
}

// R5: what a script creates as the member role itself (after `RESET ROLE`, wherever PUBLIC may
// create) is handed to the acted role once the run is over, after a failing run too.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn objects_created_after_reset_role_end_up_owned_by_the_acted_role() {
    let s = Scratch::new().await;
    let m = MemberRole::new(&s).await;
    let setup = s.run_ok("CREATE SCHEMA open; GRANT USAGE, CREATE ON SCHEMA open TO PUBLIC").await;
    assert_eq!(outcome(&setup), Outcome::Ok, "{setup:?}");
    let events = m
        .run(
            &s,
            "RESET ROLE; \
             CREATE TABLE open.t (id int GENERATED ALWAYS AS IDENTITY, body text); \
             CREATE SEQUENCE open.s; \
             CREATE VIEW open.v AS SELECT 1 AS x; \
             CREATE MATERIALIZED VIEW open.mv AS SELECT 1 AS x; \
             CREATE FUNCTION open.f() RETURNS int LANGUAGE sql AS 'SELECT 1'; \
             CREATE PROCEDURE open.p() LANGUAGE sql AS 'SELECT 1'; \
             CREATE TYPE open.mood AS ENUM ('ok'); \
             CREATE TYPE open.pair AS (a int, b int); \
             CREATE DOMAIN open.pos AS int CHECK (VALUE > 0); \
             CREATE TEMP TABLE scratch_tmp (id int)",
        )
        .await;
    assert_eq!(outcome(&events), Outcome::Ok, "{events:?}");
    // A failing run hands over too (what it committed), and so does a run left inside a
    // transaction (what it committed before).
    let events = m
        .run(&s, "BEGIN; RESET ROLE; CREATE TABLE open.t2 (id int); COMMIT; SELECT nope")
        .await;
    assert_eq!(outcome(&events), Outcome::Error);
    let events = m
        .run(&s, "BEGIN; RESET ROLE; CREATE TABLE open.t3 (id int); COMMIT; BEGIN; SELECT 1")
        .await;
    assert_eq!(outcome(&events), Outcome::Ok);

    let owners = s
        .run_ok(
            "SELECT c.relname || ':' || pg_get_userbyid(c.relowner) FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE n.nspname = 'open' AND c.relkind IN ('r', 'S', 'v', 'm') \
             UNION ALL SELECT p.proname || ':' || pg_get_userbyid(p.proowner) FROM pg_proc p \
                 WHERE p.pronamespace = 'open'::regnamespace \
             UNION ALL SELECT t.typname || ':' || pg_get_userbyid(t.typowner) FROM pg_type t \
                 WHERE t.typnamespace = 'open'::regnamespace AND t.typname IN ('mood', 'pair', 'pos') \
             ORDER BY 1",
        )
        .await;
    // Compared as sets: the database's collation orders `t:…` and `t2:…` its own way.
    let mut found: Vec<String> = rows_of(&owners, 0).into_iter().map(|r| r[0].clone().unwrap()).collect();
    found.sort();
    let mut want: Vec<String> = [
        "f", "mood", "mv", "p", "pair", "pos", "s", "t", "t2", "t3", "t_id_seq", "v",
    ]
    .iter()
    .map(|name| format!("{name}:{}", m.group))
    .collect();
    want.sort();
    assert_eq!(found, want);
    // The member role owns nothing left in the database.
    let left = s
        .run_ok(&format!(
            "SELECT count(*) FROM pg_shdepend d JOIN pg_database db ON db.oid = d.dbid \
             WHERE db.datname = current_database() AND d.deptype = 'o' \
               AND d.refobjid = (SELECT oid FROM pg_roles WHERE rolname = '{}')",
            m.target.user
        ))
        .await;
    assert_eq!(rows_of(&left, 0), vec![vec![Some("0".into())]]);
    s.drop().await;
    m.drop().await;
}

// The schema read on the member role's session, as the acted role, sees what that role may use:
// a schema it has USAGE on, not one it hasn't.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn schema_read_follows_role_usage() {
    let s = Scratch::new().await;
    let m = MemberRole::new(&s).await;
    let setup = s
        .run_ok(&format!(
            "CREATE SCHEMA hidden; CREATE TABLE hidden.secret (id int); \
             CREATE SCHEMA visible; CREATE TABLE visible.open_t (id int); \
             GRANT USAGE ON SCHEMA visible TO {0}; GRANT SELECT ON visible.open_t TO {0}",
            m.group
        ))
        .await;
    assert_eq!(outcome(&setup), Outcome::Ok, "{setup:?}");
    let connected = run::connect(&m.target).await.unwrap();
    let json = orchestrator::sql_editor::schema::introspect(connected.client(), Some(&m.group))
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_str(&json).unwrap();
    let tables: Vec<String> = body["tables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| format!("{}.{}", t["schema"].as_str().unwrap(), t["name"].as_str().unwrap()))
        .collect();
    assert!(tables.contains(&"visible.open_t".to_string()), "{tables:?}");
    assert!(!tables.iter().any(|t| t.starts_with("hidden.")), "{tables:?}");
    let schemas: Vec<&str> = body["schemas"].as_array().unwrap().iter().filter_map(|s| s["name"].as_str()).collect();
    assert!(schemas.contains(&"visible") && !schemas.contains(&"hidden"), "{schemas:?}");
    drop(connected);
    s.drop().await;
    m.drop().await;
}

/// How an act-as run ends, for [`member_role_holds_nothing_after_any_run`].
#[derive(Debug, Clone, Copy)]
enum Ending {
    Ok,
    Error,
    Cancel,
    Timeout,
    Disconnect,
    /// Past the response cap (`truncated_response`).
    Truncated,
    /// `SET ROLE` to the acted role fails: the script never runs.
    FailedSetRole,
}

/// A script that, as the member role itself (after `RESET ROLE`), makes one object of every
/// class a non-superuser can own (`n` keeps names apart between runs), grants on them, sets
/// default privileges, names itself in policies and grants (as itself and as the group), sets
/// its own role defaults and shadows `pg_class` with a temp view. Committed before `tail`.
fn escape_script(n: usize, group: &str, database: &str, tail: &str) -> String {
    format!(
        "BEGIN;
RESET ROLE;
SET search_path = pg_temp, pg_catalog;
CREATE TEMP VIEW pg_class AS SELECT 1 AS oid;
CREATE SCHEMA s{n};
GRANT USAGE, CREATE ON SCHEMA s{n} TO {group};
CREATE TABLE s{n}.t (id int GENERATED ALWAYS AS IDENTITY, body text);
CREATE SEQUENCE s{n}.seq;
CREATE VIEW s{n}.v AS SELECT 1 AS x;
CREATE MATERIALIZED VIEW s{n}.mv AS SELECT 1 AS x;
CREATE FUNCTION s{n}.f(int) RETURNS int LANGUAGE sql AS 'SELECT $1';
CREATE PROCEDURE s{n}.p() LANGUAGE sql AS 'SELECT 1';
CREATE AGGREGATE s{n}.agg(int) (SFUNC = pg_catalog.int4pl, STYPE = int);
CREATE TYPE s{n}.mood AS ENUM ('ok');
CREATE TYPE s{n}.pair AS (a int, b int);
CREATE TYPE s{n}.span AS RANGE (SUBTYPE = int);
CREATE DOMAIN s{n}.pos AS int CHECK (VALUE > 0);
CREATE OPERATOR s{n}.=== (LEFTARG = int, RIGHTARG = int, FUNCTION = pg_catalog.int4eq);
CREATE COLLATION s{n}.coll FROM pg_catalog.\"C\";
CREATE CONVERSION s{n}.conv FOR 'UTF8' TO 'LATIN1' FROM pg_catalog.utf8_to_iso8859_1;
CREATE TEXT SEARCH CONFIGURATION s{n}.cfg (COPY = pg_catalog.simple);
CREATE TEXT SEARCH DICTIONARY s{n}.dict (TEMPLATE = pg_catalog.simple);
CREATE STATISTICS s{n}.st ON id, body FROM s{n}.t;
CREATE PUBLICATION pub{n} FOR TABLE s{n}.t;
CREATE EXTENSION IF NOT EXISTS pgcrypto SCHEMA s{n};
DO $$ BEGIN EXECUTE pg_catalog.format('GRANT SELECT ON LARGE OBJECT %s TO %I', pg_catalog.lo_create(0), '{group}'); END $$;
GRANT SELECT ON s{n}.t TO {group} WITH GRANT OPTION;
GRANT EXECUTE ON FUNCTION s{n}.f(int) TO SESSION_USER;
CREATE POLICY own ON s{n}.t TO SESSION_USER USING (true);
ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO PUBLIC;
ALTER DEFAULT PRIVILEGES IN SCHEMA s{n} GRANT SELECT ON TABLES TO SESSION_USER;
SET ROLE {group};
CREATE TABLE s{n}.g (id int);
GRANT SELECT ON s{n}.g TO SESSION_USER;
GRANT UPDATE (id) ON s{n}.g TO SESSION_USER;
CREATE POLICY gp ON s{n}.g TO SESSION_USER USING (true);
ALTER DEFAULT PRIVILEGES IN SCHEMA s{n} GRANT INSERT ON TABLES TO SESSION_USER;
RESET ROLE;
ALTER ROLE CURRENT_USER SET search_path = pg_temp;
ALTER ROLE CURRENT_USER SET default_transaction_read_only = on;
ALTER ROLE CURRENT_USER IN DATABASE {database} SET work_mem = '1MB';
COMMIT;
{tail}"
    )
}

/// Everything that still names `member` in the current database (and on shared objects): its
/// owned objects of any class (`pg_shdepend` and each catalog's owner column), any ACL or
/// default ACL entry it grants or holds, any policy naming it, its role settings. Empty is the
/// invariant.
const MEMBER_LEFTOVERS_SQL: &str = "\
WITH m AS (SELECT oid FROM pg_catalog.pg_roles WHERE rolname = $1),
     here AS (SELECT oid FROM pg_catalog.pg_database WHERE datname = pg_catalog.current_database())
SELECT 'shdepend ' || d.classid::regclass::text || ' ' || d.objid || ' ' || d.deptype::text
  FROM pg_catalog.pg_shdepend d, m
 WHERE d.refclassid = 'pg_catalog.pg_authid'::regclass AND d.refobjid = m.oid
   AND (d.dbid = 0 OR d.dbid = (SELECT oid FROM here))
UNION ALL SELECT 'class ' || relname FROM pg_catalog.pg_class, m WHERE relowner = m.oid
UNION ALL SELECT 'proc ' || proname FROM pg_catalog.pg_proc, m WHERE proowner = m.oid
UNION ALL SELECT 'type ' || typname FROM pg_catalog.pg_type, m WHERE typowner = m.oid
UNION ALL SELECT 'namespace ' || nspname FROM pg_catalog.pg_namespace, m WHERE nspowner = m.oid
UNION ALL SELECT 'operator ' || oprname FROM pg_catalog.pg_operator, m WHERE oprowner = m.oid
UNION ALL SELECT 'opclass ' || opcname FROM pg_catalog.pg_opclass, m WHERE opcowner = m.oid
UNION ALL SELECT 'opfamily ' || opfname FROM pg_catalog.pg_opfamily, m WHERE opfowner = m.oid
UNION ALL SELECT 'collation ' || collname FROM pg_catalog.pg_collation, m WHERE collowner = m.oid
UNION ALL SELECT 'conversion ' || conname FROM pg_catalog.pg_conversion, m WHERE conowner = m.oid
UNION ALL SELECT 'ts config ' || cfgname FROM pg_catalog.pg_ts_config, m WHERE cfgowner = m.oid
UNION ALL SELECT 'ts dict ' || dictname FROM pg_catalog.pg_ts_dict, m WHERE dictowner = m.oid
UNION ALL SELECT 'statistics ' || stxname FROM pg_catalog.pg_statistic_ext, m WHERE stxowner = m.oid
UNION ALL SELECT 'large object ' || l.oid FROM pg_catalog.pg_largeobject_metadata l, m WHERE l.lomowner = m.oid
UNION ALL SELECT 'extension ' || extname FROM pg_catalog.pg_extension, m WHERE extowner = m.oid
UNION ALL SELECT 'publication ' || pubname FROM pg_catalog.pg_publication, m WHERE pubowner = m.oid
UNION ALL SELECT 'server ' || srvname FROM pg_catalog.pg_foreign_server, m WHERE srvowner = m.oid
UNION ALL SELECT 'fdw ' || fdwname FROM pg_catalog.pg_foreign_data_wrapper, m WHERE fdwowner = m.oid
UNION ALL SELECT 'event trigger ' || evtname FROM pg_catalog.pg_event_trigger, m WHERE evtowner = m.oid
UNION ALL SELECT 'language ' || lanname FROM pg_catalog.pg_language, m WHERE lanowner = m.oid
UNION ALL SELECT 'database ' || datname FROM pg_catalog.pg_database, m WHERE datdba = m.oid
UNION ALL SELECT 'default acl owner ' || defaclobjtype::text FROM pg_catalog.pg_default_acl, m WHERE defaclrole = m.oid
UNION ALL SELECT 'acl class ' || c.relname FROM pg_catalog.pg_class c, pg_catalog.aclexplode(c.relacl) a, m
  WHERE a.grantee = m.oid OR a.grantor = m.oid
UNION ALL SELECT 'acl column ' || t.attname FROM pg_catalog.pg_attribute t, pg_catalog.aclexplode(t.attacl) a, m
  WHERE a.grantee = m.oid OR a.grantor = m.oid
UNION ALL SELECT 'acl proc ' || p.proname FROM pg_catalog.pg_proc p, pg_catalog.aclexplode(p.proacl) a, m
  WHERE a.grantee = m.oid OR a.grantor = m.oid
UNION ALL SELECT 'acl type ' || t.typname FROM pg_catalog.pg_type t, pg_catalog.aclexplode(t.typacl) a, m
  WHERE a.grantee = m.oid OR a.grantor = m.oid
UNION ALL SELECT 'acl namespace ' || n.nspname FROM pg_catalog.pg_namespace n, pg_catalog.aclexplode(n.nspacl) a, m
  WHERE a.grantee = m.oid OR a.grantor = m.oid
UNION ALL SELECT 'acl large object ' || l.oid FROM pg_catalog.pg_largeobject_metadata l, pg_catalog.aclexplode(l.lomacl) a, m
  WHERE a.grantee = m.oid OR a.grantor = m.oid
UNION ALL SELECT 'acl database ' || d.datname FROM pg_catalog.pg_database d, pg_catalog.aclexplode(d.datacl) a, m
  WHERE a.grantee = m.oid OR a.grantor = m.oid
UNION ALL SELECT 'acl language ' || g.lanname FROM pg_catalog.pg_language g, pg_catalog.aclexplode(g.lanacl) a, m
  WHERE a.grantee = m.oid OR a.grantor = m.oid
UNION ALL SELECT 'default acl ' || d.defaclobjtype::text FROM pg_catalog.pg_default_acl d, pg_catalog.aclexplode(d.defaclacl) a, m
  WHERE a.grantee = m.oid OR a.grantor = m.oid
UNION ALL SELECT 'policy ' || p.polname FROM pg_catalog.pg_policy p, m WHERE m.oid = ANY (p.polroles)
UNION ALL SELECT 'setting ' || pg_catalog.array_to_string(s.setconfig, ',') FROM pg_catalog.pg_db_role_setting s, m
  WHERE s.setrole = m.oid";

/// The roles `member` can `SET ROLE` to, itself included, sorted.
const MEMBER_SET_ROLES_SQL: &str = "\
SELECT r.rolname::text FROM pg_catalog.pg_roles r
 WHERE pg_catalog.pg_has_role($1::text, r.oid, 'SET') ORDER BY 1";

/// Waits (60 s at most) until [`escape_script`] number `n` committed: its schema is visible.
async fn until_committed(admin: &tokio_postgres::Client, n: usize) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        let seen = admin
            .query_opt("SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname = $1", &[&format!("s{n}")])
            .await
            .unwrap();
        if seen.is_some() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("script {n} didn't commit within 60 s");
}

/// The member-role invariant (`chelabase_types::role_name::member_hand_over_sql`): after any
/// act-as run ends — success, error, cancel, timeout or disconnect — the member role owns no
/// object of any class, appears in no ACL, default ACL or policy, and can `SET ROLE` only into
/// its reconciled safe set. Driven by [`escape_script`], which tries every way a script has
/// to leave something behind as the member role.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn member_role_holds_nothing_after_any_run() {
    let s = Scratch::new().await;
    let m = MemberRole::new(&s).await;
    // PUBLIC may create schemas, publications and trusted extensions here: every class the
    // member role can make on its own is open to it.
    let setup = s
        .run_ok(&format!("GRANT CREATE ON DATABASE {} TO PUBLIC", s.target.database))
        .await;
    assert_eq!(outcome(&setup), Outcome::Ok, "{setup:?}");
    let admin = admin_in(&s.target.database).await;
    // A second run of the same member, alive through every ending below: what it committed is
    // handed over too, and its own session is never ended by another run's hand-over.
    let other = Uuid::new_v4();
    let guard = s.registry.reserve("tester", other, None, s.branch, &s.target.database).unwrap();
    let (other_stream, other_handed) = m
        .start(
            &s,
            "BEGIN; RESET ROLE; CREATE SCHEMA concur; CREATE TABLE concur.t (id int); COMMIT; \
             SELECT pg_catalog.pg_sleep(120)",
            Duration::from_secs(180),
            guard,
        )
        .await;
    let other_events = tokio::spawn(other_stream.collect::<Vec<_>>());
    tokio::time::sleep(Duration::from_secs(1)).await;
    let endings = [
        Ending::Ok,
        Ending::Error,
        Ending::Cancel,
        Ending::Timeout,
        Ending::Disconnect,
        Ending::Truncated,
        Ending::FailedSetRole,
    ];
    for (n, ending) in endings.into_iter().enumerate() {
        let tail = match ending {
            Ending::Ok | Ending::FailedSetRole => "SELECT 1",
            Ending::Error => "SELECT nope",
            Ending::Truncated => "SELECT pg_catalog.repeat('x', 20000000) FROM pg_catalog.generate_series(1, 3)",
            _ => "SELECT pg_catalog.pg_sleep(30)",
        };
        let script = escape_script(n, &m.group, &s.target.database, tail);
        let timeout = match ending {
            Ending::Timeout => Duration::from_secs(4),
            _ => Duration::from_secs(60),
        };
        let id = Uuid::new_v4();
        let guard = s
            .registry
            .reserve("tester", id, None, s.branch, &s.target.database)
            .unwrap();
        let (mut stream, handed) = match ending {
            Ending::FailedSetRole => m.start_as(&s, &script, timeout, guard, "no_such_role_t9").await,
            _ => m.start(&s, &script, timeout, guard).await,
        };
        let events = match ending {
            // Both end the run once its body committed (a fixed 2 s delay raced a slow body:
            // the cancel landed before the COMMIT). Seen from the catalog: Postgres buffers a
            // script's results until a notice or the end, so the stream can't tell.
            Ending::Cancel => {
                until_committed(&admin, n).await;
                assert!(s.cancel(id), "the run ended before its cancel");
                stream.collect().await
            }
            Ending::Disconnect => {
                until_committed(&admin, n).await;
                let mut events = Vec::new();
                while let Ok(Some(event)) =
                    tokio::time::timeout(Duration::from_millis(200), stream.next()).await
                {
                    events.push(event);
                }
                drop(stream);
                events
            }
            _ => stream.collect().await,
        };
        let want = match ending {
            Ending::Ok => Some(Outcome::Ok),
            Ending::Error => Some(Outcome::Error),
            Ending::Cancel => Some(Outcome::Cancelled),
            Ending::Timeout => Some(Outcome::Timeout),
            Ending::Disconnect => None,
            Ending::Truncated => Some(Outcome::TruncatedResponse),
            Ending::FailedSetRole => Some(Outcome::Error),
        };
        if let Some(want) = want {
            assert_eq!(outcome(&events), want, "{ending:?}: {events:?}");
        }
        let ran = !matches!(ending, Ending::FailedSetRole);
        // Whatever ended the run, the script's body committed first. A cancel, a timeout or a
        // disconnect stops the stream while the tail runs, before the buffered COMMIT arrives:
        // their commit is seen in the catalog.
        if matches!(ending, Ending::Cancel | Ending::Timeout | Ending::Disconnect) {
            until_committed(&admin, n).await;
        } else {
            assert_eq!(
                events.iter().any(|e| matches!(e, RunEvent::Complete { tag, .. } if tag == "COMMIT")),
                ran,
                "{ending:?}: {events:?}"
            );
        }
        let handed = tokio::time::timeout(Duration::from_secs(30), handed)
            .await
            .unwrap_or_else(|_| panic!("{ending:?}: no hand-over"));
        handed.expect("the hand-over ran").unwrap_or_else(|e| panic!("{ending:?}: {e}"));

        if ran {
            // The script did make its objects (now the group's).
            let made: i64 = admin
                .query_one(
                    "SELECT count(*) FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
                     WHERE n.nspname = $1 AND pg_catalog.pg_get_userbyid(c.relowner) = $2",
                    &[&format!("s{n}"), &m.group],
                )
                .await
                .unwrap()
                .get(0);
            assert!(made >= 6, "{ending:?}: only {made} relations made");
        }
        assert_member_holds_nothing(&admin, &m, &format!("{ending:?}")).await;
        // The run's session is gone (its temp view with it); the other run's is still there.
        assert_eq!(s.editor_backends().await, 1, "{ending:?}");
    }
    // The other run's committed objects went to the group with the first hand-over.
    let concur: String = admin
        .query_one(
            "SELECT pg_catalog.pg_get_userbyid(c.relowner) FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'concur' AND c.relname = 't'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(concur, m.group);
    assert!(s.cancel(other));
    let other_events = other_events.await.unwrap();
    assert_eq!(outcome(&other_events), Outcome::Cancelled, "{other_events:?}");
    tokio::time::timeout(Duration::from_secs(30), other_handed)
        .await
        .expect("the other run's hand-over")
        .expect("the hand-over ran")
        .expect("the hand-over succeeded");
    assert_member_holds_nothing(&admin, &m, "the other run").await;
    assert_eq!(s.editor_backends().await, 0);
    drop(admin);
    s.drop().await;
    m.drop().await;
}

/// The member-role invariant, checked as the superuser: nothing names the member role, and it
/// can `SET ROLE` only to itself and its group.
async fn assert_member_holds_nothing(admin: &tokio_postgres::Client, m: &MemberRole, what: &str) {
    let left: Vec<String> = admin
        .query(MEMBER_LEFTOVERS_SQL, &[&m.target.user])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert!(left.is_empty(), "{what}: the member role still holds {left:?}");
    let set: Vec<String> = admin
        .query(MEMBER_SET_ROLES_SQL, &[&m.target.user])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    let mut want_set = vec![m.target.user.clone(), m.group.clone()];
    want_set.sort();
    assert_eq!(set, want_set, "{what}");
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn nothing_leaks_between_runs() {
    let s = Scratch::new().await;
    let events = s
        .run_ok("CREATE TEMP TABLE scratch_tmp (id int); SET search_path = nowhere")
        .await;
    assert_eq!(outcome(&events), Outcome::Ok);
    let events = s
        .run_ok("SELECT current_setting('search_path'), to_regclass('pg_temp.scratch_tmp') IS NULL")
        .await;
    assert_eq!(
        rows_of(&events, 0),
        vec![vec![Some("\"$user\", public".into()), Some("t".into())]]
    );
    s.drop().await;
}
