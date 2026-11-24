//! `with_transaction` and the pipelined `run` against the dev metadata Postgres. Run through
//! the live queue:
//! just workbench test -p chelabase-query --test engine_live -- --run-ignored only
//!
//! `with_transaction` works in its own scratch table; the pipeline tests each in a scratch
//! database with a minimal `chelabase` schema (the version table and the guard functions),
//! dropped afterwards. An sqlx session outside the transaction checks what was committed.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use chelabase_query::engine::{NO_SQL_PREPARED_SQL, POOLED_RESET_SQL, RunSettings};
use chelabase_query::error::ErrorDetail;
use chelabase_query::{QueryCache, QueryClient, QueryFailure, RunAs, run, with_transaction};
use http::StatusCode;
use sqlx::{Connection, PgConnection};
use tokio_postgres::types::Type;
use uuid::Uuid;

/// rest's reset (the pooled `authenticator` connections').
const RESET_SQL: &str = POOLED_RESET_SQL;

/// The metadata database the live recipes use (`CHELABASE_METADATA_DATABASE_URL`, as the
/// justfile exports it), else the shared dev one.
fn url() -> String {
    std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".into()
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
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn with_transaction_rolls_back_on_error() {
    let mut admin = PgConnection::connect(&url())
        .await
        .expect("metadata Postgres (is the workbench stack up?)");
    let (client, connection) = tokio_postgres::connect(&url(), tokio_postgres::NoTls)
        .await
        .expect("metadata Postgres (is the workbench stack up?)");
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

/// A client that counts its prepares (only the pipeline prepares through it) and records
/// whether it was marked broken (a pool would close it).
struct Counted {
    client: tokio_postgres::Client,
    prepares: AtomicUsize,
    broken: AtomicBool,
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
        self.broken.store(true, Ordering::Relaxed);
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
    /// The run's suffix, for the names of cluster-wide objects (roles).
    suffix: String,
    /// Roles the test made, dropped with the database.
    roles: Vec<String>,
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
            .expect("metadata Postgres (is the workbench stack up?)");
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
                broken: AtomicBool::new(false),
            },
            cache: QueryCache::default(),
            branch: Uuid::new_v4(),
            suffix: run[..12].to_owned(),
            roles: Vec::new(),
        }
    }

    /// A new role `cq_<name>_<suffix>` (no login) that may use what the tests touch.
    async fn role(&mut self, name: &str) -> String {
        let role = format!("cq_{name}_{}", self.suffix);
        self.sql(&format!(
            "CREATE ROLE {role} NOLOGIN;
             GRANT USAGE ON SCHEMA chelabase TO {role};
             GRANT SELECT ON chelabase.schema_version TO {role};
             GRANT SELECT, INSERT ON ALL TABLES IN SCHEMA public TO {role};
             ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT, INSERT ON TABLES TO {role}"
        ))
        .await;
        self.roles.push(role.clone());
        role
    }

    /// Whether the connection was marked broken (a pool would have closed it).
    fn broken(&self) -> bool {
        self.client.broken.load(Ordering::Relaxed)
    }

    /// One value read on the request's own connection, outside any request.
    async fn on_connection<T: for<'a> tokio_postgres::types::FromSql<'a>>(&self, sql: &str) -> T {
        self.client.client.query_one(sql, &[]).await.unwrap().get(0)
    }

    /// Runs `tree` as the owner with `budget`: the result, and whether it was pipelined
    /// (anything prepared).
    async fn run(
        &self,
        tree: serde_json::Value,
        budget: usize,
    ) -> (Result<String, QueryFailure>, bool) {
        self.run_as(RunAs::Owner { user: &self.user }, tree, budget)
            .await
    }

    /// [`Self::run`] as `run_as`.
    async fn run_as(
        &self,
        run_as: RunAs<'_>,
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
            run_as,
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

    /// Drops the scratch database; the connection must not have been marked broken (no
    /// reset or session check failed).
    async fn finish(self) {
        assert!(!self.broken(), "the connection was marked broken");
        self.finish_any().await;
    }

    /// Drops the scratch database and the test's roles, whatever the connection's state.
    async fn finish_any(mut self) {
        drop(self.client);
        self.admin.close().await.unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE {} WITH (FORCE)",
            self.name
        )))
        .execute(&mut self.meta)
        .await
        .unwrap();
        for role in &self.roles {
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP ROLE IF EXISTS {role}")))
                .execute(&mut self.meta)
                .await
                .unwrap();
        }
    }
}

/// With the version cached, a write runs in one pipeline and commits.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
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

/// A request whose plans are over `MAX_CACHED_PLAN_BYTES` answers correctly every time and
/// leaves nothing in the plan cache (its statements are prepared for the request alone, see
/// the pipeline's unit test on the lease), while ordinary requests stay cached and pipelined.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn oversized_plan_runs_uncached() {
    use chelabase_query::engine::MAX_CACHED_PLAN_BYTES;
    let s = Scratch::new().await;
    s.warm().await;
    s.insert(1, 1 << 20).await.0.unwrap();
    s.cache.run_pending_tasks().await;
    let (_, ordinary_sets) = s.cache.shapes.held();
    assert!(ordinary_sets >= 1, "ordinary shapes are cached");

    let aliases: Vec<serde_json::Value> = (0..1600)
        .map(|i| serde_json::json!({"col": "id", "as": format!("{i:0>60}")}))
        .collect();
    let big = serde_json::json!({"v": 1, "op": "select", "from": "items", "select": aliases});
    for _ in 0..2 {
        let (out, _) = s.run(big.clone(), 1 << 24).await;
        let out = out.unwrap();
        assert!(out.len() > MAX_CACHED_PLAN_BYTES, "{}", out.len());
        assert!(out.starts_with("[{"), "the row came back");
    }
    s.cache.run_pending_tasks().await;
    assert_eq!(
        s.cache.shapes.held().1,
        ordinary_sets,
        "the oversized plan was not cached"
    );
    let pipelined = s.cache.pipelined_runs.load(Ordering::Relaxed);
    let (out, _) = s
        .run(serde_json::json!({"v": 1, "op": "select", "from": "items"}), 1 << 20)
        .await;
    out.unwrap();
    assert!(
        s.cache.pipelined_runs.load(Ordering::Relaxed) > pipelined,
        "ordinary requests still run pipelined"
    );
    s.finish().await;
}

/// A schema version changed under the cache: the pipeline's assert fails (`CB001`), nothing
/// of it commits, and the sequential path runs the request once and records the version.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
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
#[ignore = "needs the workbench stack's metadata Postgres"]
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
#[ignore = "needs the workbench stack's metadata Postgres"]
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
#[ignore = "needs the workbench stack's metadata Postgres"]
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
#[ignore = "needs the workbench stack's metadata Postgres"]
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

/// RT1 T4 (R18, W5): the console's text forms compare and read values by their text, so a
/// numeric `1.50`, a bigint past 2^53, a timestamptz and a text with quotes and a backslash
/// round-trip exactly (a JSON number or a re-parsed timestamp would not). Run it with:
/// just workbench test -p chelabase-query --test engine_live -- --run-ignored only text_forms_compare_by_text
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn text_forms_compare_by_text() {
    use chelabase_query::query::{ParseMode, parse, parse_with};
    use serde_json::{Value, json};

    let note = r#"O'Brien "x" \ end"#;
    let mut s = Scratch::new().await;
    s.sql(
        "CREATE TABLE public.vals (id bigint PRIMARY KEY, num numeric(10,2), big bigint, \
         ts timestamptz, note text);
         INSERT INTO public.vals VALUES
           (1, 1.50, 9007199254740993, '2026-10-16 12:00:00.5+00', 'O''Brien \"x\" \\ end'),
           (2, 2.50, 9007199254740992, '2026-10-17 00:00:00+00', 'obrien'),
           (3, NULL, NULL, NULL, NULL);",
    )
    .await;

    async fn console(s: &Scratch, tree: Value) -> Value {
        let op = parse_with(tree.to_string().as_bytes(), ParseMode::Console)
            .unwrap_or_else(|e| panic!("{tree} should parse in console mode: {e}"))
            .op;
        let out = run(
            &s.client,
            &s.cache,
            (s.branch, "main"),
            RunAs::Owner { user: &s.user },
            &settings(),
            1 << 20,
            &op,
            "rid-text",
        )
        .await
        .unwrap_or_else(|e| panic!("{tree}: {} {}", e.code, e.message));
        serde_json::from_str(&out).unwrap()
    }
    async fn ids(s: &Scratch, filter: Value) -> Vec<i64> {
        let rows = console(
            s,
            json!({"v": 1, "op": "select", "from": "vals", "select": ["id"],
                   "where": filter, "orderBy": [{"col": "id"}]}),
        )
        .await;
        rows.as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_i64().unwrap())
            .collect()
    }
    let text = |col: &str, values: Value| json!({"col": col, "in": values, "text": true});
    let eq = |col: &str, v: &str| json!({"col": col, "eq": v, "text": true});

    // Reading: every value comes back as its exact text, NULL as null.
    let rows = console(
        &s,
        json!({"v": 1, "op": "select", "from": "vals", "orderBy": [{"col": "id"}],
               "select": [{"text_of": "id", "as": "id_t"}, {"text_of": "num", "as": "num_t"},
                          {"text_of": "big", "as": "big_t"}, {"text_of": "ts", "as": "ts_t"},
                          {"text_of": "note", "as": "note_t"}]}),
    )
    .await;
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["id_t"], "1");
    assert_eq!(rows[0]["num_t"], "1.50");
    assert_eq!(rows[0]["big_t"], "9007199254740993");
    assert_eq!(rows[0]["note_t"], note);
    assert_eq!(rows[1]["num_t"], "2.50");
    assert_eq!(rows[1]["big_t"], "9007199254740992");
    for col in ["num_t", "big_t", "ts_t", "note_t"] {
        assert_eq!(rows[2][col], Value::Null, "{col}");
    }
    assert!(rows[0]["ts_t"].as_str().unwrap().starts_with("2026-10-16 "), "{}", rows[0]["ts_t"]);
    let ts = rows[0]["ts_t"].as_str().unwrap().to_owned();

    // Comparing: by text, not by value.
    assert_eq!(ids(&s, eq("num", "1.50")).await, vec![1]);
    assert_eq!(ids(&s, eq("num", "1.5")).await, Vec::<i64>::new(), "1.5 is not the text 1.50");
    assert_eq!(ids(&s, eq("num", "1.500")).await, Vec::<i64>::new());
    assert_eq!(ids(&s, text("num", json!(["1.50", "2.50"]))).await, vec![1, 2]);
    assert_eq!(ids(&s, eq("big", "9007199254740993")).await, vec![1]);
    assert_eq!(ids(&s, eq("big", "9007199254740992")).await, vec![2]);
    assert_eq!(ids(&s, eq("ts", &ts)).await, vec![1]);
    assert_eq!(
        ids(&s, eq("ts", "2026-10-16T12:00:00.5Z")).await,
        Vec::<i64>::new(),
        "the same instant in another spelling is another text"
    );
    assert_eq!(ids(&s, eq("note", note)).await, vec![1]);
    assert_eq!(ids(&s, text("note", json!(["obrien", note]))).await, vec![1, 2]);
    // COLLATE "C": exact, case-sensitive, byte-wise.
    assert_eq!(ids(&s, eq("note", "OBRIEN")).await, Vec::<i64>::new());
    // NULL rows never equal a text.
    assert_eq!(ids(&s, eq("num", "")).await, Vec::<i64>::new());
    // Mixed with ordinary conditions, and negated.
    assert_eq!(
        ids(&s, json!({"and": [eq("num", "1.50"), {"col": "big", "gt": 5}]})).await,
        vec![1]
    );
    assert_eq!(
        ids(&s, json!({"not": eq("note", note)})).await,
        vec![2],
        "NOT of a comparison with NULL is not true"
    );
    // The same trees are refused by the data API's parser.
    let refused = json!({"v": 1, "op": "select", "from": "vals", "where": eq("num", "1.50")});
    assert!(parse(refused.to_string().as_bytes()).is_err());
    s.finish().await;
}

/// RT1 gf (item 3): what the console puts on the wire for a `numeric`. The row JSON keeps Postgres's own
/// literal (scale and every digit), and NaN and the infinities are quoted strings, exactly as Postgres prints
/// them; so Studio's `2.5` for `2.50` is lost in the browser, not here. The `text_of` read answers the same
/// texts. Run it with:
/// just workbench test -p chelabase-query --test engine_live -- --run-ignored only numeric_wire_keeps_postgres_text
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn numeric_wire_keeps_postgres_text() {
    use serde_json::{Value, json};

    let mut s = Scratch::new().await;
    s.sql(
        "CREATE TABLE public.money (id bigint PRIMARY KEY, price numeric(12,2), wide numeric);
         INSERT INTO public.money VALUES
           (1, 2.50, 12345678901234567890.12),
           (2, 5.00, 0.10),
           (3, 'NaN', 'Infinity'),
           (4, NULL, '-Infinity');",
    )
    .await;
    let (out, _) = s
        .run(
            json!({"v": 1, "op": "select", "from": "money", "orderBy": [{"col": "id"}]}),
            1 << 20,
        )
        .await;
    let body = out.unwrap();
    for literal in [
        r#""price":2.50"#,
        r#""price":5.00"#,
        r#""wide":12345678901234567890.12"#,
        r#""wide":0.10"#,
        r#""price":"NaN""#,
        r#""wide":"Infinity""#,
        r#""price":null"#,
        r#""wide":"-Infinity""#,
    ] {
        assert!(body.contains(literal), "{literal} is Postgres's own literal in {body}");
    }

    let op = chelabase_query::parse_with(
        json!({"v": 1, "op": "select", "from": "money", "orderBy": [{"col": "id"}],
               "select": [{"text_of": "price", "as": "p"}, {"text_of": "wide", "as": "w"}]})
        .to_string()
        .as_bytes(),
        chelabase_query::ParseMode::Console,
    )
    .unwrap()
    .op;
    let out = run(
        &s.client,
        &s.cache,
        (s.branch, "main"),
        RunAs::Owner { user: &s.user },
        &settings(),
        1 << 20,
        &op,
        "rid-numeric",
    )
    .await
    .unwrap();
    let rows: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        rows,
        json!([
            {"p": "2.50", "w": "12345678901234567890.12"},
            {"p": "5.00", "w": "0.10"},
            {"p": "NaN", "w": "Infinity"},
            {"p": null, "w": "-Infinity"},
        ])
    );
    s.finish().await;
}

// --- session integrity of a pooled connection (m3) -----------------------------------------

/// A row trigger on `public.items` that runs `body` for the row with id 1 only, so the
/// requests after it are ordinary ones.
fn on_first_item(body: &str) -> String {
    format!(
        "CREATE FUNCTION public.on_first_item() RETURNS trigger LANGUAGE plpgsql AS
           $f$ BEGIN IF NEW.id = 1 THEN {body} END IF; RETURN NEW; END $f$;
         CREATE TRIGGER on_first_item BEFORE INSERT ON public.items
           FOR EACH ROW EXECUTE FUNCTION public.on_first_item();"
    )
}

/// The 500 a failed session check answers: nothing of Postgres's text.
fn assert_internal(out: Result<String, QueryFailure>) {
    let e = out.expect_err("the request must not succeed");
    assert_eq!(
        (e.status, e.code),
        (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        "{e:?}"
    );
    assert_eq!(e.message, "internal error (request id rid-live)");
    assert_eq!(e.sqlstate, None);
}

/// AC 1: a temporary table and a session-level advisory lock a request's function left
/// are gone before the next request: the table doesn't exist, and another session can take
/// the lock. The connection stays in use.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn pooled_reset_drops_temp_tables_and_advisory_locks() {
    let mut s = Scratch::new().await;
    s.sql(&on_first_item(
        "CREATE TEMP TABLE cq_left (id int); PERFORM pg_catalog.pg_advisory_lock(424242);",
    ))
    .await;
    s.warm().await;
    let (out, pipelined) = s.insert(1, 1 << 20).await;
    out.unwrap();
    assert!(pipelined);
    let (out, _) = s.insert(2, 1 << 20).await;
    out.unwrap();
    assert_eq!(s.ids().await, vec![1, 2]);
    assert!(
        s.on_connection::<bool>("SELECT pg_catalog.to_regclass('pg_temp.cq_left') IS NULL")
            .await,
        "the temporary table is gone"
    );
    let free: bool = sqlx::query_scalar("SELECT pg_catalog.pg_try_advisory_lock(424242)")
        .fetch_one(&mut s.admin)
        .await
        .unwrap();
    assert!(free, "another session can take the lock");
    s.finish().await;
}

/// AC 9: a `WITH HOLD` cursor, a `LISTEN` registration and a sequence's `currval` a
/// request left aren't visible to the next request on the connection.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn pooled_reset_closes_held_cursors_listens_and_sequences() {
    let mut s = Scratch::new().await;
    s.sql("CREATE SEQUENCE public.cq_seq").await;
    s.sql(&on_first_item(
        "EXECUTE 'DECLARE cq_cur CURSOR WITH HOLD FOR SELECT 1'; LISTEN cq_chan; \
         PERFORM pg_catalog.nextval('public.cq_seq');",
    ))
    .await;
    s.warm().await;
    let (out, _) = s.insert(1, 1 << 20).await;
    out.unwrap();
    let (out, _) = s.insert(2, 1 << 20).await;
    out.unwrap();
    assert_eq!(s.ids().await, vec![1, 2]);
    let cursors: i64 = s
        // By name: the view also lists the portal of the statement that reads it.
        .on_connection(
            "SELECT pg_catalog.count(*) FROM pg_catalog.pg_cursors WHERE name = 'cq_cur'",
        )
        .await;
    assert_eq!(cursors, 0, "no cursor is left open");
    let listens: i64 = s
        .on_connection("SELECT pg_catalog.count(*) FROM pg_catalog.pg_listening_channels()")
        .await;
    assert_eq!(listens, 0, "nothing is listened to");
    let currval = s
        .client
        .client
        .query_one("SELECT pg_catalog.currval('public.cq_seq')", &[])
        .await
        .unwrap_err();
    assert_eq!(
        currval.code().map(|c| c.code()),
        Some("55000"),
        "the sequence has no value in this session: {currval}"
    );
    s.finish().await;
}

/// AC 2, both paths and both identities: a request that ends as another role than the one
/// it asked for commits nothing, answers 500 with no rows, and its connection is marked
/// broken. `pipelined`: whether the request under test runs as one pipeline.
async fn role_left_changed_rolls_back(pipelined: bool) {
    for as_owner in [true, false] {
        let mut s = Scratch::new().await;
        let other = s.role("other").await;
        let app = s.role("app").await;
        s.sql(&on_first_item(&format!("SET ROLE {other};"))).await;
        let claims = serde_json::json!({});
        let user = s.user.clone();
        let run_as = if as_owner {
            RunAs::Owner { user: &user }
        } else {
            RunAs::Role {
                role: &app,
                claims: &claims,
                oid: None,
            }
        };
        let insert = |id: i64| {
            serde_json::json!({
                "v": 1, "op": "insert", "into": "items",
                "values": [{"id": id, "body": "x"}], "returning": ["id"],
            })
        };
        if pipelined {
            // Caches the version, the schema and the role's view.
            let (out, _) = s.run_as(run_as, insert(7), 1 << 20).await;
            out.unwrap();
        }
        let (out, was_pipelined) = s.run_as(run_as, insert(1), 1 << 20).await;
        assert_eq!(was_pipelined, pipelined, "owner: {as_owner}");
        assert_internal(out);
        assert!(s.broken(), "owner {as_owner}: the connection is discarded");
        let kept: Vec<i64> = if pipelined { vec![7] } else { vec![] };
        assert_eq!(s.ids().await, kept, "owner {as_owner}: nothing was committed");
        assert_eq!(state(&mut s.admin, s.pid).await, "idle");
        assert_eq!(
            s.on_connection::<String>("SELECT current_user::pg_catalog.text")
                .await,
            user,
            "the reset put the session back to its login"
        );
        s.finish_any().await;
    }
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn role_left_changed_rolls_back_pipelined() {
    role_left_changed_rolls_back(true).await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn role_left_changed_rolls_back_sequential() {
    role_left_changed_rolls_back(false).await;
}

/// AC 3: a request during which a SQL-level prepared statement is created commits nothing
/// and answers 500, pipelined and sequential; the connection is marked broken, and no later
/// request runs on it (the opening reset refuses the session).
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn sql_prepared_statement_rolls_back() {
    for pipelined in [true, false] {
        let mut s = Scratch::new().await;
        s.sql(&on_first_item("EXECUTE 'PREPARE cq_left AS SELECT 1';"))
            .await;
        if pipelined {
            s.warm().await;
        }
        let (out, was_pipelined) = s.insert(1, 1 << 20).await;
        assert_eq!(was_pipelined, pipelined);
        assert_internal(out);
        assert!(s.broken(), "pipelined {pipelined}");
        assert_eq!(s.ids().await, Vec::<i64>::new(), "pipelined {pipelined}");
        assert_eq!(state(&mut s.admin, s.pid).await, "idle");

        // The statement outlives the rollback, so the session can't be cleaned: an ordinary
        // request on it is refused too, and nothing of it commits.
        let (out, _) = s.insert(2, 1 << 20).await;
        let e = out.unwrap_err();
        assert_eq!(
            (e.status, e.code),
            (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
            "pipelined {pipelined}: {e:?}"
        );
        assert_eq!(s.ids().await, Vec::<i64>::new());
        s.finish_any().await;
    }
}

/// AC 8: a deferred constraint trigger that changes the role for the transaction and writes
/// runs before the session check (the constraints go immediate first), so nothing commits
/// and the answer is 500, on both paths. A deferred trigger that only writes still commits.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn deferred_trigger_role_change_rolls_back() {
    for pipelined in [false, true] {
        let mut s = Scratch::new().await;
        let other = s.role("other").await;
        s.sql(&format!(
            "CREATE TABLE public.audit (id bigint);
             GRANT INSERT ON public.audit TO {other};
             CREATE FUNCTION public.at_commit() RETURNS trigger LANGUAGE plpgsql AS
               $f$ BEGIN
                 IF NEW.id = 1 THEN SET LOCAL ROLE {other}; END IF;
                 INSERT INTO public.audit VALUES (NEW.id);
                 RETURN NULL; END $f$;
             CREATE CONSTRAINT TRIGGER at_commit AFTER INSERT ON public.items
               DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.at_commit();"
        ))
        .await;
        if pipelined {
            s.warm().await;
        }
        let (out, was_pipelined) = s.insert(1, 1 << 20).await;
        assert_eq!(was_pipelined, pipelined);
        assert_internal(out);
        assert!(s.broken(), "pipelined {pipelined}: the connection is discarded");
        assert_eq!(s.ids().await, Vec::<i64>::new(), "pipelined {pipelined}");
        let audited: Vec<i64> = sqlx::query_scalar("SELECT id FROM public.audit ORDER BY id")
            .fetch_all(&mut s.admin)
            .await
            .unwrap();
        assert_eq!(audited, Vec::<i64>::new(), "pipelined {pipelined}");

        // Deferred work that leaves the role alone commits as before.
        let (out, _) = s.insert(2, 1 << 20).await;
        out.unwrap();
        assert_eq!(s.ids().await, vec![2]);
        let audited: Vec<i64> = sqlx::query_scalar("SELECT id FROM public.audit ORDER BY id")
            .fetch_all(&mut s.admin)
            .await
            .unwrap();
        assert_eq!(audited, vec![2]);
        s.finish_any().await;
    }
}

/// AC 7: the owner console's reset (orchestrator's `OWNER_RESET_SQL`: the pooled reset's
/// clean-up, then the prepared-statement check) fails on a session that holds a SQL-level prepared
/// statement, which discards the connection; a session without one resets cleanly.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn owner_reset_refuses_sql_prepared() {
    // The orchestrator's `OWNER_RESET_SQL` is this constant (its unit test pins it).
    let owner_reset = POOLED_RESET_SQL;
    assert!(owner_reset.ends_with(NO_SQL_PREPARED_SQL));
    let s = Scratch::new().await;
    let pg = |e: tokio_postgres::Error| QueryFailure::from_pg(&e, "rid-owner");

    let out = with_transaction(&s.client, owner_reset, "rid-owner", async |c| {
        c.batch_execute("SELECT 1").await.map_err(pg)
    })
    .await;
    out.unwrap();
    assert!(!s.broken(), "a clean session resets");

    // The statement is made by the transaction's own work; the reset after its COMMIT
    // refuses the session.
    let out = with_transaction(&s.client, owner_reset, "rid-owner", async |c| {
        c.batch_execute("PREPARE cq_left AS SELECT 1")
            .await
            .map_err(pg)
    })
    .await;
    out.unwrap();
    assert!(s.broken(), "the reset failed: the connection is discarded");
    // What the reset raises, so its mapping stays explicit.
    let e = s.client.client.batch_execute(owner_reset).await.unwrap_err();
    assert_eq!(e.code().map(|c| c.code()), Some("22023"), "{e}");

    // And the next transaction can't begin on it.
    let out: Result<(), QueryFailure> =
        with_transaction(&s.client, owner_reset, "rid-owner", async |_| Ok(())).await;
    let e = out.unwrap_err();
    assert_eq!(
        (e.status, e.code),
        (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
        "{e:?}"
    );
    s.finish_any().await;
}

/// A caller that answers for a failed statement itself (its closure returns `Ok` with the
/// answer, as the console's cell read does for a refused column) keeps that answer, every
/// time: the transaction was already aborted, so the steps before `COMMIT` don't run,
/// nothing commits, and the connection stays in use. On a multi-threaded runtime, as the
/// services run, and many rounds: the answer must not depend on when the driver learns the
/// transaction's status.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn an_answer_for_a_failed_statement_stands() {
    let mut s = Scratch::new().await;
    let mut lost = Vec::new();
    for round in 0..1000 {
        let out = chelabase_query::console_session(
            &s.client,
            "main",
            RunAs::Owner { user: &s.user },
            &settings(),
            "rid-live",
            async |c, _version| {
                c.batch_execute("INSERT INTO public.items VALUES (1, 'kept?')")
                    .await
                    .map_err(|e| QueryFailure::from_pg(&e, "rid-live"))?;
                let refused = c
                    .batch_execute("SELECT 1 FROM public.no_such_table")
                    .await
                    .unwrap_err();
                Ok(refused.code().map(|c| c.code().to_owned()))
            },
        )
        .await;
        match out {
            Ok(code) => assert_eq!(code.as_deref(), Some("42P01")),
            Err(e) => lost.push((round, e.status.as_u16(), e.code)),
        }
    }
    assert!(
        lost.is_empty(),
        "the caller's answer was replaced in {} of 1000 rounds: {:?}",
        lost.len(),
        &lost[..lost.len().min(5)]
    );
    assert_eq!(s.ids().await, Vec::<i64>::new(), "the aborted transactions kept nothing");
    assert_eq!(state(&mut s.admin, s.pid).await, "idle");
    s.finish().await;
}

/// The session check is no statement of the session's: a request whose own work drops a
/// statement the connection holds and prepares another under its name can't put anything in
/// the check's place. Ending as another role, it commits nothing and answers 500, pipelined,
/// as the owner and as a role.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn session_check_cannot_be_replaced_by_the_request() {
    for as_owner in [true, false] {
        let mut s = Scratch::new().await;
        let other = s.role("other").await;
        let app = s.role("app").await;
        // Looks for the check among the session's statements by its text; with one found,
        // another statement takes its name.
        s.sql(&on_first_item(&format!(
            "SELECT p.name INTO n FROM pg_catalog.pg_prepared_statements p
              WHERE NOT p.from_sql
                AND p.statement LIKE '%p.from_sql) THEN pg_catalog.current_setting(%';
             IF n IS NOT NULL THEN
               EXECUTE pg_catalog.format('DEALLOCATE %I', n);
               EXECUTE pg_catalog.format('PREPARE %I(text) AS SELECT $1', n);
             END IF;
             SET ROLE {other};"
        ))
        .replace("$f$ BEGIN", "$f$ DECLARE n text; BEGIN"))
        .await;
        let claims = serde_json::json!({});
        let user = s.user.clone();
        let run_as = if as_owner {
            RunAs::Owner { user: &user }
        } else {
            RunAs::Role {
                role: &app,
                claims: &claims,
                oid: None,
            }
        };
        let insert = |id: i64| {
            serde_json::json!({
                "v": 1, "op": "insert", "into": "items",
                "values": [{"id": id, "body": "x"}], "returning": ["id"],
            })
        };
        // Twice: the first caches the version and the view, the second runs pipelined and
        // leaves its statements on the connection.
        for id in [7, 8] {
            let (out, _) = s.run_as(run_as, insert(id), 1 << 20).await;
            out.unwrap();
        }
        let (out, was_pipelined) = s.run_as(run_as, insert(1), 1 << 20).await;
        assert!(was_pipelined, "owner {as_owner}");
        assert_eq!(
            s.ids().await,
            vec![7, 8],
            "owner {as_owner}: nothing was committed (answered {out:?})"
        );
        assert_internal(out);
        assert!(s.broken(), "owner {as_owner}: the connection is discarded");
        s.finish_any().await;
    }
}

/// AC 8, the part no check before `COMMIT` can reach: deferred work that queues more
/// deferred work while the constraints are being made immediate runs at `COMMIT`. It
/// commits, and the connection is discarded, never pooled, on both paths. So is the
/// connection of any request that wrote a table with a deferrable constraint trigger, even
/// when the trigger does nothing else; a request that wrote no such table keeps its
/// connection.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn deferred_work_at_commit_closes_the_connection() {
    for pipelined in [false, true] {
        let mut s = Scratch::new().await;
        let other = s.role("other").await;
        s.sql(&format!(
            "CREATE TABLE public.hop (id bigint);
             CREATE TABLE public.audit (id bigint, who text DEFAULT current_user);
             CREATE TABLE public.plain (id bigint);
             GRANT INSERT ON public.audit, public.hop TO {other};
             CREATE FUNCTION public.first_fire() RETURNS trigger LANGUAGE plpgsql AS
               $f$ BEGIN
                 IF NEW.id = 1 THEN SET CONSTRAINTS ALL DEFERRED; END IF;
                 INSERT INTO public.hop VALUES (NEW.id);
                 RETURN NULL; END $f$;
             CREATE CONSTRAINT TRIGGER first_fire AFTER INSERT ON public.items
               DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.first_fire();
             CREATE FUNCTION public.at_commit() RETURNS trigger LANGUAGE plpgsql AS
               $f$ BEGIN
                 IF NEW.id = 1 THEN SET LOCAL ROLE {other}; END IF;
                 INSERT INTO public.audit (id) VALUES (NEW.id);
                 RETURN NULL; END $f$;
             CREATE CONSTRAINT TRIGGER at_commit AFTER INSERT ON public.hop
               DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.at_commit();"
        ))
        .await;
        let plain = |id: i64| {
            serde_json::json!({
                "v": 1, "op": "insert", "into": "plain", "values": [{"id": id}],
            })
        };
        // Caches the version and the view (pipelined), and shows the other side: a write
        // to a table without a deferrable trigger keeps the connection.
        let (out, _) = s.run(plain(1), 1 << 20).await;
        out.unwrap();
        if pipelined {
            let (out, was_pipelined) = s.run(plain(2), 1 << 20).await;
            out.unwrap();
            assert!(was_pipelined);
        } else {
            s.cache.versions.invalidate(&(s.branch, "main".to_owned()));
        }
        assert!(!s.broken(), "pipelined {pipelined}: an ordinary write keeps its connection");

        let (out, was_pipelined) = s.insert(1, 1 << 20).await;
        assert_eq!(was_pipelined, pipelined);
        out.unwrap();
        assert_eq!(s.ids().await, vec![1], "pipelined {pipelined}: the work commits");
        let audited: Vec<i64> = sqlx::query_scalar("SELECT id FROM public.audit ORDER BY id")
            .fetch_all(&mut s.admin)
            .await
            .unwrap();
        assert_eq!(audited, vec![1], "pipelined {pipelined}: so does what ran at COMMIT");
        assert!(
            s.broken(),
            "pipelined {pipelined}: the connection is discarded, never pooled"
        );
        assert_eq!(
            s.on_connection::<String>("SELECT current_user::pg_catalog.text")
                .await,
            s.user,
            "the reset put the session back to its login"
        );
        s.finish_any().await;

        // A deferrable trigger that does nothing but exist: the same.
        let mut s = Scratch::new().await;
        s.sql(
            "CREATE FUNCTION public.quiet() RETURNS trigger LANGUAGE plpgsql AS
               $f$ BEGIN RETURN NULL; END $f$;
             CREATE CONSTRAINT TRIGGER quiet AFTER INSERT ON public.items
               DEFERRABLE INITIALLY IMMEDIATE FOR EACH ROW EXECUTE FUNCTION public.quiet();",
        )
        .await;
        if pipelined {
            s.warm().await;
        }
        assert!(!s.broken());
        let (out, was_pipelined) = s.insert(3, 1 << 20).await;
        assert_eq!(was_pipelined, pipelined);
        out.unwrap();
        assert_eq!(s.ids().await, vec![3]);
        assert!(s.broken(), "pipelined {pipelined}: a deferrable trigger's table was written");
        s.finish_any().await;
    }
}

/// AC 8, when the work that ran at `COMMIT` fails: nothing commits, the request answers
/// with that error, and the connection is still discarded, on both paths. Unchecked work
/// ran on it, whatever the `COMMIT` answered.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn failed_work_at_commit_still_closes_the_connection() {
    for pipelined in [false, true] {
        let mut s = Scratch::new().await;
        s.sql(
            "CREATE TABLE public.hop (id bigint);
             CREATE FUNCTION public.first_fire() RETURNS trigger LANGUAGE plpgsql AS
               $f$ BEGIN
                 IF NEW.id = 1 THEN SET CONSTRAINTS ALL DEFERRED; END IF;
                 INSERT INTO public.hop VALUES (NEW.id);
                 RETURN NULL; END $f$;
             CREATE CONSTRAINT TRIGGER first_fire AFTER INSERT ON public.items
               DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.first_fire();
             CREATE FUNCTION public.at_commit() RETURNS trigger LANGUAGE plpgsql AS
               $f$ BEGIN
                 IF NEW.id = 1 THEN RAISE EXCEPTION 'ran at commit'; END IF;
                 RETURN NULL; END $f$;
             CREATE CONSTRAINT TRIGGER at_commit AFTER INSERT ON public.hop
               DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.at_commit();",
        )
        .await;
        if pipelined {
            s.warm().await;
        }
        let (out, was_pipelined) = s.insert(1, 1 << 20).await;
        assert_eq!(was_pipelined, pipelined);
        let e = out.expect_err("the COMMIT failed");
        assert_eq!(e.message, "ran at commit", "pipelined {pipelined}: {e:?}");
        assert_eq!(s.ids().await, Vec::<i64>::new(), "pipelined {pipelined}");
        assert!(
            s.broken(),
            "pipelined {pipelined}: work ran at COMMIT, so the connection is discarded"
        );
        assert_eq!(state(&mut s.admin, s.pid).await, "idle");
        s.finish_any().await;
    }
}

/// #320 against Postgres's own errors: a public key's anonymous caller (hidden) keeps the
/// code, message and SQLSTATE of each mapped failure and loses `detail` and `hint`, and no
/// row value is in what is left (a constraint violation's message names the constraint, table
/// or column; `detail` is where the values are). Shown keeps Postgres's detail.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn hidden_failures_carry_no_row_values() {
    let (client, connection) = tokio_postgres::connect(&url(), tokio_postgres::NoTls)
        .await
        .expect("metadata Postgres (is the workbench stack up?)");
    tokio::spawn(connection);
    let run = Uuid::new_v4().simple().to_string();
    let schema = format!("cq_redact_{}", &run[..12]);
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {schema};
             SET search_path = {schema};
             CREATE TABLE parents (id int PRIMARY KEY);
             CREATE TABLE members (
               email text UNIQUE,
               tag text CHECK (tag = lower(tag)),
               parent int REFERENCES parents (id),
               note text NOT NULL DEFAULT 'zq-row-default');
             INSERT INTO members (email) VALUES ('zq-dup@example.com');
             CREATE FUNCTION refuse() RETURNS void LANGUAGE plpgsql AS $$
               BEGIN RAISE EXCEPTION 'refused' USING DETAIL = 'zq-fn-detail', HINT = 'zq-fn-hint'; END $$;"
        ))
        .await
        .unwrap();
    let cases = [
        ("INSERT INTO members (email) VALUES ('zq-dup@example.com')", "23505"),
        ("INSERT INTO members (tag) VALUES ('ZQ-UPPER')", "23514"),
        ("INSERT INTO members (parent) VALUES (4242)", "23503"),
        ("INSERT INTO members (email, note) VALUES ('zq-null@example.com', NULL)", "23502"),
        ("SELECT refuse()", "P0001"),
    ];
    for (sql, sqlstate) in cases {
        let err = client.batch_execute(sql).await.unwrap_err();
        let full = QueryFailure::from_pg(&err, "rid-1");
        assert_eq!(full.sqlstate.as_deref(), Some(sqlstate), "{sql}: {full:?}");
        assert!(full.detail.is_some(), "{sql}: Postgres sent no detail: {full:?}");
        assert_eq!(full.clone().shown_to(ErrorDetail::Shown), full);
        let hidden = full.clone().shown_to(ErrorDetail::Hidden);
        assert_eq!((&hidden.detail, &hidden.hint), (&None, &None), "{sql}");
        assert_eq!(
            (hidden.status, hidden.code, &hidden.message, &hidden.sqlstate),
            (full.status, full.code, &full.message, &full.sqlstate)
        );
        let left = format!("{} {:?}", hidden.message, hidden.sqlstate);
        for leak in ["zq-dup", "ZQ-UPPER", "4242", "zq-row-default", "zq-null", "zq-fn-"] {
            assert!(!left.contains(leak), "{sql}: {leak} in {left}");
        }
    }
    client
        .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
        .await
        .unwrap();
}
