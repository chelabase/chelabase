//! Live checks of the SQL editor's run engine against a scratch database on the dev metadata
//! Postgres (`just deps-up`).

use std::time::{Duration, Instant};

use cheladb_types::BranchId;
use futures_util::StreamExt;
use orchestrator::sql_editor::events::{Outcome, RunEvent};
use orchestrator::sql_editor::registry::{RunGuard, RunRegistry};
use orchestrator::sql_editor::run::{self, RunSpec, RunTarget};
use orchestrator::sql_editor::{APPLICATION_NAME, MAX_RESPONSE_BYTES, MAX_ROWS_PER_STATEMENT};
use tokio_postgres::NoTls;
use uuid::Uuid;

fn admin_url() -> String {
    std::env::var("CHELADB_METADATA_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://cheladb:cheladb@127.0.0.1:5433/cheladb_metadata".into())
}

async fn admin_connect() -> tokio_postgres::Client {
    let (client, conn) = tokio_postgres::connect(&admin_url(), NoTls)
        .await
        .expect("the metadata Postgres (just deps-up)");
    tokio::spawn(conn);
    client
}

/// A scratch role and database it owns; dropped by [`Scratch::drop`].
struct Scratch {
    admin: tokio_postgres::Client,
    target: RunTarget,
    registry: RunRegistry,
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
            _ => "127.0.0.1".into(),
        };
        let addr = format!("{host}:{}", config.get_ports()[0]).parse().unwrap();
        Scratch {
            admin,
            target: RunTarget {
                addr,
                user: owner,
                password,
                database,
            },
            registry: RunRegistry::default(),
        }
    }

    fn guard(&self) -> RunGuard {
        self.registry
            .reserve(
                "tester",
                Uuid::new_v4(),
                BranchId::new(),
                &self.target.database,
            )
            .unwrap()
    }

    async fn run(&self, sql: &str, role: Option<&str>, timeout: Duration) -> Vec<RunEvent> {
        let connected = run::connect(&self.target).await.unwrap();
        let spec = RunSpec {
            sql: sql.into(),
            role: role.map(Into::into),
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
    async fn assert_slot_freed_within(&self, user: &str, id: Uuid, within: Duration) {
        let until = Instant::now() + within;
        // `cancel` returns false once the run's guard is gone.
        while self.registry.cancel(user, id) {
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
#[ignore = "needs the metadata Postgres (just deps-up)"]
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
#[ignore = "needs the metadata Postgres (just deps-up)"]
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
#[ignore = "needs the metadata Postgres (just deps-up)"]
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
#[ignore = "needs the metadata Postgres (just deps-up)"]
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
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn lone_vacuum_runs() {
    let s = Scratch::new().await;
    let events = s.run_ok("VACUUM").await;
    assert_eq!(completes(&events), vec![(0, "VACUUM".into(), 0, false)]);
    assert_eq!(outcome(&events), Outcome::Ok);
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
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
#[ignore = "needs the metadata Postgres (just deps-up)"]
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
#[ignore = "needs the metadata Postgres (just deps-up)"]
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
#[ignore = "needs the metadata Postgres (just deps-up)"]
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
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn token_cancels_the_backend() {
    let s = Scratch::new().await;
    let connected = run::connect(&s.target).await.unwrap();
    let id = Uuid::new_v4();
    let guard = s
        .registry
        .reserve("tester", id, BranchId::new(), &s.target.database)
        .unwrap();
    let spec = RunSpec {
        sql: "SELECT pg_sleep(30)".into(),
        role: None,
        timeout: Duration::from_secs(60),
    };
    let stream = run::run(connected, spec, guard);
    let started = Instant::now();
    let registry = &s.registry;
    let (events, _) = tokio::join!(stream.collect::<Vec<_>>(), async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(registry.cancel("tester", id));
    });
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(outcome(&events), Outcome::Cancelled);
    s.assert_no_backend_within(Duration::from_secs(2)).await;
    // The guard dropped with the run: the id is free again.
    assert!(!s.registry.cancel("tester", id));
    s.drop().await;
}

#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn client_drop_cancels_the_backend() {
    let s = Scratch::new().await;
    let connected = run::connect(&s.target).await.unwrap();
    let spec = RunSpec {
        sql: "SELECT pg_sleep(30)".into(),
        role: None,
        timeout: Duration::from_secs(60),
    };
    let id = Uuid::new_v4();
    let guard = s
        .registry
        .reserve("tester", id, BranchId::new(), &s.target.database)
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
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn unread_stream_frees_its_slot_after_the_deadline() {
    let s = Scratch::new().await;
    let connected = run::connect(&s.target).await.unwrap();
    // 300 statements of 3 events each: far more than the event buffer holds.
    let spec = RunSpec {
        sql: "SELECT 1;".repeat(300),
        role: None,
        timeout: Duration::from_millis(500),
    };
    let id = Uuid::new_v4();
    let guard = s
        .registry
        .reserve("tester", id, BranchId::new(), &s.target.database)
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
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn external_cancel_before_the_deadline_is_an_error() {
    let s = Scratch::new().await;
    let connected = run::connect(&s.target).await.unwrap();
    let spec = RunSpec {
        sql: "SELECT pg_sleep(30)".into(),
        role: None,
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
#[ignore = "needs the metadata Postgres (just deps-up)"]
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
#[ignore = "needs the metadata Postgres (just deps-up)"]
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
#[ignore = "needs the metadata Postgres (just deps-up)"]
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
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn notice_flood_with_a_slow_reader_ends_at_the_cap() {
    let s = Scratch::new().await;
    // About 25 MB of notices while the reader dawdles: the notice queue stays bounded
    // (backpressure on the socket) and the run still ends at the cap, not at the deadline.
    let sql = "DO $$ BEGIN FOR i IN 1..200000 LOOP RAISE NOTICE '%', repeat('n', 100); END LOOP; END $$";
    let connected = run::connect(&s.target).await.unwrap();
    let spec = RunSpec {
        sql: sql.into(),
        role: None,
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

#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
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
