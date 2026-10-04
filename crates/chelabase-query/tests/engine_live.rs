//! `with_transaction` and the pipelined `run` against the dev metadata Postgres. Run through
//! the live queue:
//! just live cargo test -p chelabase-query --test engine_live -- --ignored
//!
//! `with_transaction` works in its own scratch table; the pipeline tests each in a scratch
//! database with a minimal `chelabase` schema (the version table and the guard functions),
//! dropped afterwards. An sqlx session outside the transaction checks what was committed.

use std::sync::atomic::{AtomicUsize, Ordering};

use chelabase_query::engine::RunSettings;
use chelabase_query::{QueryCache, QueryClient, QueryFailure, RunAs, run, with_transaction};
use http::StatusCode;
use sqlx::{Connection, PgConnection};
use tokio_postgres::types::Type;
use uuid::Uuid;

/// rest's reset, as a stand-in for the caller's.
const RESET_SQL: &str = "SET ROLE NONE; RESET ALL";

/// The metadata database the live recipes use (`CHELABASE_METADATA_DATABASE_URL`, as the
/// justfile exports it), else the shared dev one.
fn url() -> String {
    std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://chelabase:chelabase@127.0.0.1:5433/chelabase_metadata".into()
    })
}

async fn committed(admin: &mut PgConnection, table: &str) -> Vec<i32> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT id FROM {table} ORDER BY id"
    )))
    .fetch_all(admin)
    .await
    .unwrap()
}

/// The client's backend state as another session sees it (`idle` = no transaction open).
async fn state(admin: &mut PgConnection, pid: i32) -> String {
    sqlx::query_scalar("SELECT state FROM pg_catalog.pg_stat_activity WHERE pid = $1")
        .bind(pid)
        .fetch_one(admin)
        .await
        .unwrap()
}

/// A failure inside the transaction (one `f` returns, or a statement's own) rolls back
/// everything `f` wrote and leaves the connection idle and usable; a success commits; and a
/// transaction a dropped request left open is rolled back, never committed by the next one.
#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn with_transaction_rolls_back_on_error() {
    let mut admin = PgConnection::connect(&url())
        .await
        .expect("metadata Postgres (just deps-up)");
    let (client, connection) = tokio_postgres::connect(&url(), tokio_postgres::NoTls)
        .await
        .expect("metadata Postgres (just deps-up)");
    tokio::spawn(connection);
    let pid: i32 = client
        .query_one("SELECT pg_catalog.pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let run = Uuid::new_v4().simple().to_string();
    let table = format!("cq_engine_{}", &run[..12]);
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE TABLE {table} (id int PRIMARY KEY)"
    )))
    .execute(&mut admin)
    .await
    .unwrap();
    let insert = |id: i32| format!("INSERT INTO {table} VALUES ({id})");

    // `f` fails after a write.
    let out: Result<(), QueryFailure> = with_transaction(&client, RESET_SQL, "rid-1", async |c| {
        c.batch_execute(&insert(1)).await.unwrap();
        Err(QueryFailure::invalid_query("boom"))
    })
    .await;
    assert_eq!(out.unwrap_err().message, "boom");
    assert_eq!(committed(&mut admin, &table).await, Vec::<i32>::new());

    // A statement fails after a write: its error, mapped, and nothing kept.
    let out: Result<(), QueryFailure> = with_transaction(&client, RESET_SQL, "rid-2", async |c| {
        c.batch_execute(&insert(2)).await.unwrap();
        c.batch_execute(&insert(2))
            .await
            .map_err(|e| QueryFailure::from_pg(&e, "rid-2"))
    })
    .await;
    let e = out.unwrap_err();
    assert_eq!((e.status, e.code), (StatusCode::CONFLICT, "conflict"));
    assert_eq!(e.sqlstate.as_deref(), Some("23505"));
    assert_eq!(committed(&mut admin, &table).await, Vec::<i32>::new());

    // The connection is out of the aborted transaction: the next one commits.
    let out = with_transaction(&client, RESET_SQL, "rid-3", async |c| {
        c.batch_execute(&insert(3))
            .await
            .map_err(|e| QueryFailure::from_pg(&e, "rid-3"))?;
        Ok(7)
    })
    .await;
    assert_eq!(out.unwrap(), 7);
    assert_eq!(committed(&mut admin, &table).await, vec![3]);
    assert_eq!(
        state(&mut admin, pid).await,
        "idle",
        "no transaction left open"
    );

    // A request dropped mid-transaction leaves it open with a write in it; the next
    // request's prefix rolls it back instead of committing it with its own work.
    client
        .batch_execute(&format!("BEGIN; {}", insert(4)))
        .await
        .unwrap();
    assert_eq!(state(&mut admin, pid).await, "idle in transaction");
    let out = with_transaction(&client, RESET_SQL, "rid-4", async |c| {
        c.batch_execute(&insert(5))
            .await
            .map_err(|e| QueryFailure::from_pg(&e, "rid-4"))
    })
    .await;
    out.unwrap();
    assert_eq!(
        committed(&mut admin, &table).await,
        vec![3, 5],
        "the stale row 4 was rolled back, not committed"
    );
    assert_eq!(
        state(&mut admin, pid).await,
        "idle",
        "no transaction left open"
    );

    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP TABLE {table}")))
        .execute(&mut admin)
        .await
        .unwrap();
}

/// The machinery the pipeline needs, as the worker installs it (the version table and the
/// two guard functions), and the tables the tests write to.
const SCRATCH_SQL: &str = "CREATE SCHEMA chelabase;
CREATE TABLE chelabase.schema_version (version uuid NOT NULL);
INSERT INTO chelabase.schema_version VALUES (gen_random_uuid());
CREATE FUNCTION chelabase.assert_schema_version(expected uuid) RETURNS uuid
  LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path = '' AS
  $f$ DECLARE v uuid; BEGIN
    SELECT version INTO v FROM chelabase.schema_version;
    IF v IS DISTINCT FROM expected THEN
      RAISE EXCEPTION USING ERRCODE = 'CB001', MESSAGE = 'chelabase: stale schema version';
    END IF;
    RETURN v; END $f$;
CREATE FUNCTION chelabase.cap_response(result text, budget bigint) RETURNS text
  LANGUAGE plpgsql VOLATILE SET search_path = '' AS
  $f$ DECLARE n bigint := coalesce(pg_catalog.octet_length(result), 0); total bigint; BEGIN
    IF n OPERATOR(pg_catalog.>) budget THEN
      RAISE EXCEPTION USING ERRCODE = 'CB002', MESSAGE = 'chelabase: response too large';
    END IF;
    total := coalesce(nullif(pg_catalog.current_setting('chelabase.response_bytes', true), '')::bigint, 0) OPERATOR(pg_catalog.+) n;
    IF total OPERATOR(pg_catalog.>) budget THEN
      RAISE EXCEPTION USING ERRCODE = 'CB002', MESSAGE = 'chelabase: response too large';
    END IF;
    PERFORM pg_catalog.set_config('chelabase.response_bytes', total::text, true);
    RETURN result; END $f$;
CREATE TABLE public.items (id bigint, body text);";

/// A client that counts its prepares: only the pipeline prepares through it.
struct Counted {
    client: tokio_postgres::Client,
    prepares: AtomicUsize,
}

impl QueryClient for Counted {
    fn client(&self) -> &tokio_postgres::Client {
        &self.client
    }

    async fn prepare_cached(
        &self,
        sql: &str,
        types: &[Type],
    ) -> Result<tokio_postgres::Statement, tokio_postgres::Error> {
        self.prepares.fetch_add(1, Ordering::Relaxed);
        self.client.prepare_typed(sql, types).await
    }

    fn mark_broken(&self) {
        panic!("the session reset failed");
    }
}

/// A scratch database with [`SCRATCH_SQL`] installed, run as its owner (the metadata user).
struct Scratch {
    meta: PgConnection,
    name: String,
    /// The superuser's session in the scratch database, outside the request's.
    admin: PgConnection,
    client: Counted,
    pid: i32,
    user: String,
    cache: QueryCache,
    branch: Uuid,
}

/// `url()` pointing at database `name`.
fn url_for(name: &str) -> String {
    let base = url();
    let (head, tail) = base.rsplit_once('/').expect("a database in the URL");
    let query = tail
        .split_once('?')
        .map(|(_, q)| format!("?{q}"))
        .unwrap_or_default();
    format!("{head}/{name}{query}")
}

fn settings() -> RunSettings {
    RunSettings {
        max_rows: 1000,
        statement_timeout_ms: 5000,
        default_schema: "public".into(),
        reset_sql: RESET_SQL,
    }
}

impl Scratch {
    async fn new() -> Scratch {
        let mut meta = PgConnection::connect(&url())
            .await
            .expect("metadata Postgres (just deps-up)");
        let run = Uuid::new_v4().simple().to_string();
        let name = format!("cq_pipe_{}", &run[..12]);
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&mut meta)
            .await
            .unwrap();
        let mut admin = PgConnection::connect(&url_for(&name)).await.unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(SCRATCH_SQL))
            .execute(&mut admin)
            .await
            .unwrap();
        let (client, connection) = tokio_postgres::connect(&url_for(&name), tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(connection);
        let row = client
            .query_one(
                "SELECT pg_catalog.pg_backend_pid(), current_user::text",
                &[],
            )
            .await
            .unwrap();
        Scratch {
            meta,
            name,
            admin,
            pid: row.get(0),
            user: row.get(1),
            client: Counted {
                client,
                prepares: AtomicUsize::new(0),
            },
            cache: QueryCache::default(),
            branch: Uuid::new_v4(),
        }
    }

    /// Runs `tree` as the owner with `budget`: the result, and whether it was pipelined
    /// (anything prepared).
    async fn run(
        &self,
        tree: serde_json::Value,
        budget: usize,
    ) -> (Result<String, QueryFailure>, bool) {
        let op = chelabase_query::parse(tree.to_string().as_bytes())
            .unwrap()
            .op;
        let before = self.client.prepares.load(Ordering::Relaxed);
        let out = run(
            &self.client,
            &self.cache,
            (self.branch, "main"),
            RunAs::Owner { user: &self.user },
            &settings(),
            budget,
            &op,
            "rid-live",
        )
        .await;
        (out, self.client.prepares.load(Ordering::Relaxed) > before)
    }

    async fn insert(&self, id: i64, budget: usize) -> (Result<String, QueryFailure>, bool) {
        self.run(
            serde_json::json!({
                "v": 1, "op": "insert", "into": "items",
                "values": [{"id": id, "body": "x".repeat(40)}], "returning": ["id", "body"],
            }),
            budget,
        )
        .await
    }

    /// Runs a read, so the version, the schema and the owner's view are cached.
    async fn warm(&self) {
        let (out, _) = self
            .run(
                serde_json::json!({"v": 1, "op": "select", "from": "items"}),
                1 << 20,
            )
            .await;
        out.unwrap();
        assert!(
            self.cache
                .versions
                .contains_key(&(self.branch, "main".to_owned())),
            "the sequential path records the version"
        );
    }

    /// The ids committed, as another session sees them.
    async fn ids(&mut self) -> Vec<i64> {
        sqlx::query_scalar("SELECT id FROM public.items ORDER BY id")
            .fetch_all(&mut self.admin)
            .await
            .unwrap()
    }

    async fn sql(&mut self, sql: &str) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_owned()))
            .execute(&mut self.admin)
            .await
            .unwrap();
    }

    async fn finish(mut self) {
        drop(self.client);
        self.admin.close().await.unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE {} WITH (FORCE)",
            self.name
        )))
        .execute(&mut self.meta)
        .await
        .unwrap();
    }
}

/// With the version cached, a write runs in one pipeline and commits.
#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn pipeline_commits_a_write() {
    let mut s = Scratch::new().await;
    s.warm().await;
    let (out, pipelined) = s.insert(1, 1 << 20).await;
    let out = out.unwrap();
    assert!(pipelined, "the second request is pipelined");
    assert!(out.contains("\"id\""), "{out}");
    assert_eq!(s.ids().await, vec![1]);
    assert_eq!(state(&mut s.admin, s.pid).await, "idle");
    // Batches too: each result joined, in order.
    let (out, pipelined) = s
        .run(
            serde_json::json!({"v": 1, "op": "batch", "ops": [
                {"op": "select", "from": "items"}, {"op": "select", "from": "items"},
            ]}),
            1 << 20,
        )
        .await;
    assert!(pipelined);
    assert!(out.unwrap().starts_with('['));
    s.finish().await;
}

/// A schema version changed under the cache: the pipeline's assert fails (`CB001`), nothing
/// of it commits, and the sequential path runs the request once and records the version.
#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn stale_version_replans_once_and_commits_once() {
    let mut s = Scratch::new().await;
    s.warm().await;
    s.sql("UPDATE chelabase.schema_version SET version = gen_random_uuid()")
        .await;
    let current: Uuid = sqlx::query_scalar("SELECT version FROM chelabase.schema_version")
        .fetch_one(&mut s.admin)
        .await
        .unwrap();
    let (out, pipelined) = s.insert(2, 1 << 20).await;
    out.unwrap();
    assert!(pipelined, "the pipeline was tried first");
    assert_eq!(
        s.ids().await,
        vec![2],
        "committed once (no PK: a second commit would show)"
    );
    assert_eq!(
        s.cache
            .versions
            .get(&(s.branch, "main".to_owned()))
            .as_deref(),
        Some(current.to_string().as_str())
    );
    // The next request pipelines against the new version.
    let (out, pipelined) = s.insert(3, 1 << 20).await;
    out.unwrap();
    assert!(pipelined);
    assert_eq!(s.ids().await, vec![2, 3]);
    s.finish().await;
}

/// A response past the budget fails in SQL (`CB002`): the 413, and the write rolled back.
#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn cap_error_leaves_no_row() {
    let mut s = Scratch::new().await;
    s.warm().await;
    let (out, pipelined) = s.insert(4, 20).await;
    assert!(pipelined);
    let e = out.unwrap_err();
    assert_eq!(
        (e.status, e.code),
        (StatusCode::PAYLOAD_TOO_LARGE, "too_large")
    );
    assert_eq!(
        e.message,
        "the response is over 20 bytes; ask for fewer rows (limit) or fewer columns"
    );
    assert_eq!(s.ids().await, Vec::<i64>::new());
    assert_eq!(state(&mut s.admin, s.pid).await, "idle");
    s.finish().await;
}

/// A database without the guard functions (one dropped here) is served sequentially and
/// marked, so the next request doesn't try the pipeline.
#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn missing_guard_functions_run_sequentially() {
    let mut s = Scratch::new().await;
    s.warm().await;
    s.sql("DROP FUNCTION chelabase.cap_response(text, bigint)")
        .await;
    let (out, pipelined) = s.insert(5, 1 << 20).await;
    out.unwrap();
    assert!(pipelined, "the pipeline was tried first");
    assert_eq!(s.ids().await, vec![5]);
    assert!(
        s.cache
            .no_guard
            .contains_key(&(s.branch, "main".to_owned()))
    );
    let (out, pipelined) = s.insert(6, 1 << 20).await;
    out.unwrap();
    assert!(!pipelined, "the marked database runs sequentially");
    assert_eq!(s.ids().await, vec![5, 6]);
    s.finish().await;
}

/// A transaction a dropped request left open (with a write in it) is rolled back by the
/// pipeline's first step, never committed with the next request's work.
#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn a_stale_open_transaction_is_not_committed() {
    let mut s = Scratch::new().await;
    s.warm().await;
    s.client
        .client
        .batch_execute("BEGIN; INSERT INTO public.items VALUES (99, 'stale')")
        .await
        .unwrap();
    assert_eq!(state(&mut s.admin, s.pid).await, "idle in transaction");
    let (out, pipelined) = s.insert(7, 1 << 20).await;
    out.unwrap();
    assert!(pipelined);
    assert_eq!(s.ids().await, vec![7], "the stale row 99 was rolled back");
    assert_eq!(state(&mut s.admin, s.pid).await, "idle");
    s.finish().await;
}

/// A request dropped while its response is still in flight (so the driver's status is
/// `Unknown`, not `Idle`) still gets the `ROLLBACK`: the transaction it opened and wrote in
/// is never committed with the next request's work.
#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn a_dropped_request_leaves_a_transaction_that_is_not_committed() {
    let mut s = Scratch::new().await;
    s.warm().await;
    // Sent, then dropped before its response arrives.
    let dropped = tokio::time::timeout(
        std::time::Duration::from_millis(20),
        s.client.client.batch_execute(
            "BEGIN; INSERT INTO public.items VALUES (98, 'stale'); SELECT pg_catalog.pg_sleep(0.3)",
        ),
    )
    .await;
    assert!(dropped.is_err(), "the request was still running");
    assert_ne!(
        s.client.client.transaction_status(),
        tokio_postgres::TransactionStatus::Idle,
        "the status isn't idle, so the next request sends ROLLBACK"
    );
    let (out, _) = s.insert(8, 1 << 20).await;
    out.unwrap();
    assert_eq!(s.ids().await, vec![8], "the stale row 98 was rolled back");
    assert_eq!(state(&mut s.admin, s.pid).await, "idle");
    s.finish().await;
}
