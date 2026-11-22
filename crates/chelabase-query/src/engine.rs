//! [`run`] pipelines a request whose database's schema version, schema and role view it
//! already holds: one round trip from the reset to `COMMIT`, the version asserted and the
//! response capped in SQL ([`crate::pipeline`]). Anything else (a cold start, a schema that
//! changed, a database without the guard functions) goes the sequential path, which reads
//! and records the version.
//!
//! The sequential path: one request, in a transaction [`run`] (or [`schema_for`]) opens and
//! commits ([`with_transaction`]):
//!
//! 1. one round trip sets the role and the claims ([`RunAs::Role`]; the owner runs as
//!    itself), the statement timeout and the transaction timeout (the same value: it
//!    bounds the whole request) and reads `chelabase.schema_version` ([`PREAMBLE_SQL`],
//!    [`OWNER_PREAMBLE_SQL`]);
//! 2. a schema version not seen before is introspected in the same transaction, and the
//!    role's view of it computed once;
//! 3. the tree is planned through the shape cache, keyed by
//!    `(branch, database, schema version, role, default_schema, shape hash)`;
//! 4. each plan runs with its values bound (`max_rows` applied at binding), its result
//!    counted against the response cap ([`ResponseBudget`]);
//! 5. before `COMMIT`, in the same round trip: `SET CONSTRAINTS ALL IMMEDIATE` (deferred
//!    triggers fire now, not at `COMMIT`), then [`SESSION_CHECK_SQL`], which fails unless
//!    the transaction still runs as the role the request asked for and the session holds
//!    no SQL-level prepared statement. A failed check commits nothing: the request answers
//!    500 and the connection is discarded ([`with_checked_transaction`]). The pipelined
//!    path sends the same two steps ([`crate::pipeline`]).
//!
//! Every statement declares its parameters' types (`text`, `jsonb`, `int8`, `text[]`): the
//! server never infers one. Every plan returns one `text` row: the operation's JSON, sent on
//! unparsed. A batch joins its results into one array.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use http::StatusCode;
use tokio_postgres::{Client, TransactionStatus};
use tokio_postgres::types::{ToSql, Type};
use uuid::Uuid;

use crate::error::{DbFields, QueryFailure, SCHEMA_MISSING, classify, schema_missing_in};
use crate::pipeline::{
    self, Attempt, Call, Ctx, Param, QueryClient, Step, StepKind, join_in_order, sql_budget,
    take_trailing_reset,
};
use crate::query::Op;
use crate::schema::{self, IntrospectError, RoleView, Schema};
use crate::shape::shape_hash;
use crate::sql::{BindValue, ParamType, Plan, PlanError, plan, resolve_bind};

/// Schema versions kept by the count-bounded caches (versions and guard marks: one entry
/// per served database in the steady state).
pub const SCHEMA_CACHE: u64 = 1_000;
/// The bytes the schema cache holds across every branch and database (each schema weighed
/// by [`Schema::approx_bytes`] and its key).
pub const SCHEMA_CACHE_BYTES: u64 = 256 * 1024 * 1024;
/// The bytes of plans kept per branch.
pub const SHAPE_BYTES_PER_BRANCH: u64 = 8 * 1024 * 1024;
/// The bytes of plans kept across every branch.
pub const SHAPE_BYTES_TOTAL: u64 = 256 * 1024 * 1024;
/// A plan set (or a statement) over this many bytes is used for its request and never
/// cached: not as a plan, not as a named statement on the server.
pub const MAX_CACHED_PLAN_BYTES: usize = 64 * 1024;
/// The largest query response, unless the caller says otherwise.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// `$1` role, `$2` claims, `$3` statement timeout in ms, also the transaction timeout
/// (Postgres 17: set inside a transaction, it starts counting then and ends with it), all
/// `text`; `$4` (`int8`) the role's OID a secret key is bound to, or `NULL`. `set_config`
/// returns its value; the last column is the schema version.
///
/// The role is set only while the role named `$1` has OID `$4`: a dropped role, or another
/// role created under the name since the key was made, sets `''` instead, which no role can
/// be named (Postgres refuses zero-length names), so the preamble fails as for a missing role
/// and nothing runs as anyone. The check and the `SET ROLE` are one expression: no window.
pub const PREAMBLE_SQL: &str = "SELECT pg_catalog.set_config('role', CASE WHEN $4::pg_catalog.int8 IS NULL OR \
     (SELECT r.oid::pg_catalog.int8 FROM pg_catalog.pg_roles r WHERE r.rolname OPERATOR(pg_catalog.=) $1) OPERATOR(pg_catalog.=) $4::pg_catalog.int8 \
     THEN $1 ELSE '' END, true), \
     pg_catalog.set_config('request.jwt.claims', $2, true), pg_catalog.set_config('statement_timeout', $3, true), \
     pg_catalog.set_config('transaction_timeout', $3, true), \
     (SELECT version::pg_catalog.text FROM chelabase.schema_version)";

/// The owner's preamble: no role and no claims (the connection's own user runs the
/// request), `$1` the statement and transaction timeout in ms (`text`). It returns the
/// user's name (the role its view and plans are cached under) and the schema version.
pub const OWNER_PREAMBLE_SQL: &str = "SELECT pg_catalog.set_config('statement_timeout', $1, true), \
     pg_catalog.set_config('transaction_timeout', $1, true), current_user::pg_catalog.text, \
     (SELECT version::pg_catalog.text FROM chelabase.schema_version)";

/// [`PREAMBLE_SQL`] in the pipeline: `$5` (`uuid`) the version the request was planned
/// against, asserted (`CB001` when the database's differs) rather than read.
pub const GUARDED_PREAMBLE_SQL: &str = "SELECT pg_catalog.set_config('role', CASE WHEN $4::pg_catalog.int8 IS NULL OR \
     (SELECT r.oid::pg_catalog.int8 FROM pg_catalog.pg_roles r WHERE r.rolname OPERATOR(pg_catalog.=) $1) OPERATOR(pg_catalog.=) $4::pg_catalog.int8 \
     THEN $1 ELSE '' END, true), \
     pg_catalog.set_config('request.jwt.claims', $2, true), pg_catalog.set_config('statement_timeout', $3, true), \
     pg_catalog.set_config('transaction_timeout', $3, true), \
     chelabase.assert_schema_version($5)";

/// [`OWNER_PREAMBLE_SQL`] in the pipeline: `$2` (`uuid`) the version asserted.
pub const OWNER_GUARDED_PREAMBLE_SQL: &str = "SELECT pg_catalog.set_config('statement_timeout', $1, true), \
     pg_catalog.set_config('transaction_timeout', $1, true), current_user::pg_catalog.text, \
     chelabase.assert_schema_version($2)";

/// Fails when the session holds a prepared statement made in SQL (`PREPARE`), and leaves the
/// role unset otherwise. It reads `pg_prepared_statement()`, the function behind the
/// `pg_prepared_statements` view (cheaper to plan, and this text is planned on every use).
/// The function builds a row per statement the session holds, so its cost grows with the
/// connection's statement cache: a request sends it as seldom as it can
/// ([`opening_reset_sql`]). Such a statement shares its namespace with the protocol-level
/// statements a pooled connection caches, and no reset drops it without dropping those too,
/// so a session that holds one is closed rather than cleaned. It raises the way the
/// preamble does: the empty name is no role, so `set_config` refuses it.
pub const NO_SQL_PREPARED_SQL: &str = "SELECT pg_catalog.set_config('role', CASE WHEN EXISTS \
     (SELECT 1 FROM pg_catalog.pg_prepared_statement() p WHERE p.from_sql) THEN '' ELSE 'none' END, false)";

/// What returns a pooled `authenticator` connection to the state a new session has: no
/// role, the connection's default settings, no session-level advisory lock, no temporary
/// table, no open cursor, no `LISTEN` registration, no sequence state, and (or the reset
/// fails, [`NO_SQL_PREPARED_SQL`]) no SQL-level prepared statement. Protocol-level prepared
/// statements survive it. Every statement of it is allowed inside a transaction block.
/// Every transaction sends it first ([`begin_sql`]) and again after its end; a reset that
/// fails discards the connection. rest, storage and realtime share this text.
pub const POOLED_RESET_SQL: &str = "SET ROLE NONE; RESET ALL; \
     SELECT pg_catalog.pg_advisory_unlock_all(); DISCARD TEMP; CLOSE ALL; UNLISTEN *; \
     DISCARD SEQUENCES; \
     SELECT pg_catalog.set_config('role', CASE WHEN EXISTS \
     (SELECT 1 FROM pg_catalog.pg_prepared_statement() p WHERE p.from_sql) THEN '' ELSE 'none' END, false)";

/// `reset_sql` without its prepared-statement check ([`NO_SQL_PREPARED_SQL`], when it ends
/// with it): what opens a transaction on a connection whose last transaction ended with the
/// full reset ([`QueryClient::opening_reset`]). Nothing ran on the session since that reset
/// passed, so the check has nothing new to find, and the request's own two (before its
/// `COMMIT`, and in the reset after it) still run.
pub fn opening_reset_sql(reset_sql: &str) -> &str {
    match reset_sql.strip_suffix(NO_SQL_PREPARED_SQL) {
        Some(rest) => rest.trim_end().trim_end_matches(';'),
        None => reset_sql,
    }
}

/// Sent before [`SESSION_CHECK_SQL`]: deferred constraints and constraint triggers queued so
/// far run now, inside the checked part of the transaction. Work they queue in turn while
/// deferring constraints again still runs at `COMMIT`; the check's second column is what
/// covers that (the connection is closed).
pub const CONSTRAINTS_IMMEDIATE_SQL: &str = "SET CONSTRAINTS ALL IMMEDIATE";

/// The last statement before `COMMIT`: `$1` (`text`) the role the request runs as
/// ([`RunAs::role`]; `none` stands for the connection's own login). It fails unless
/// `current_user` is that role and the session holds no SQL-level prepared statement, which
/// aborts the transaction, so the `COMMIT` behind it rolls back. When it passes it sets the
/// role to what it already is. It raises the way the preamble does (the empty name is no
/// role), with no function of the database's own.
///
/// Its second column ([`wrote_deferrable`]) says whether the transaction wrote a row of a
/// table that has a deferrable constraint trigger (a deferrable key or foreign key, or a
/// `CREATE CONSTRAINT TRIGGER … DEFERRABLE`). Only such a write can queue deferred work, and
/// deferred work can queue more of itself for `COMMIT`, after this check, where nothing
/// can look at it. Postgres has no statement that tells whether deferred work is pending,
/// so the signal is the write itself: the transaction's own row counts
/// (`pg_stat_get_xact_tuples_*`), which the code a request runs can't lower. They are only
/// kept while `track_counts` is on (a setting only a superuser changes), so anything else
/// counts as written. A request with the mark commits as usual, and its connection is
/// closed instead of pooled.
pub const SESSION_CHECK_SQL: &str = "SELECT pg_catalog.set_config('role', CASE WHEN \
     current_user::pg_catalog.text OPERATOR(pg_catalog.=) \
     (CASE WHEN $1 OPERATOR(pg_catalog.=) 'none' THEN session_user::pg_catalog.text ELSE $1 END) \
     AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_prepared_statement() p WHERE p.from_sql) \
     THEN pg_catalog.current_setting('role') ELSE '' END, true), \
     (pg_catalog.current_setting('track_counts') OPERATOR(pg_catalog.<>) 'on' OR EXISTS \
     (SELECT 1 FROM pg_catalog.pg_trigger t WHERE t.tgdeferrable AND \
     (pg_catalog.pg_stat_get_xact_tuples_inserted(t.tgrelid) OPERATOR(pg_catalog.+) \
     pg_catalog.pg_stat_get_xact_tuples_updated(t.tgrelid) OPERATOR(pg_catalog.+) \
     pg_catalog.pg_stat_get_xact_tuples_deleted(t.tgrelid)) OPERATOR(pg_catalog.>) 0))";

/// Whether the session check's row says the transaction wrote a table with a deferrable
/// constraint trigger ([`SESSION_CHECK_SQL`]'s second column). A row that can't be read
/// counts as yes.
pub(crate) fn wrote_deferrable(row: &tokio_postgres::Row) -> bool {
    row.try_get::<_, bool>(1).unwrap_or(true)
}

/// A request that may have left deferred work for its `COMMIT` ([`wrote_deferrable`]): what
/// it did is committed, and its connection is closed rather than pooled
/// ([`QueryClient::mark_broken`]).
pub(crate) fn discard_after_deferrable_write<C: QueryClient + ?Sized>(client: &C, request_id: &str) {
    tracing::debug!(
        request_id,
        "the request wrote a table with a deferrable constraint trigger; its connection isn't pooled again"
    );
    client.mark_broken();
}

/// How long a database found without the guard functions runs sequentially before the
/// pipeline is tried again (reconcile reinstalls them).
pub const NO_GUARD_TTL: Duration = Duration::from_secs(60);

/// Who a request runs as.
#[derive(Debug, Clone, Copy)]
pub enum RunAs<'a> {
    /// `SET ROLE role` (the connection's user must be a member) with these JWT claims;
    /// `oid`: the role's identity a secret key is bound to (the preamble refuses another
    /// role under the name), `None` to check the name only.
    Role {
        role: &'a str,
        claims: &'a serde_json::Value,
        oid: Option<i64>,
    },
    /// The connection's own user, the database's owner (P5): `user` is the connection's
    /// login user, the role its view and plans are cached under. The preamble checks the
    /// connection runs as it (a 500 otherwise).
    Owner { user: &'a str },
}

impl<'a> RunAs<'a> {
    /// The role the request's view and plans are cached under.
    pub fn role(&self) -> &'a str {
        match *self {
            RunAs::Role { role, .. } => role,
            RunAs::Owner { user } => user,
        }
    }
}

/// The limits a request runs under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSettings {
    pub max_rows: i64,
    pub statement_timeout_ms: i64,
    /// What an unqualified name means.
    pub default_schema: String,
    /// What returns the connection to a clean session before the request (the caller's
    /// `RESET_SQL` or `OWNER_RESET_SQL`); every transaction sends it
    /// first ([`begin_sql`]).
    pub reset_sql: &'static str,
}

/// One database at one schema version: what the schema cache is keyed by.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SchemaKey {
    pub branch: Uuid,
    pub database: String,
    pub version: String,
}

/// `(schema key, role, default_schema, shape hash)`.
pub type ShapeKey = (SchemaKey, String, String, u64);

/// What's left of a response's size cap. Each operation's result is counted as it arrives
/// (Postgres builds it as one value, so it can't be counted sooner); the first past the cap
/// aborts the request, and its transaction rolls back.
pub struct ResponseBudget {
    cap: usize,
    used: usize,
}

impl ResponseBudget {
    pub fn new(cap: usize) -> Self {
        ResponseBudget { cap, used: 0 }
    }

    /// Counts `n` more bytes: a 413 `too_large` once the total passes the cap.
    pub fn take(&mut self, n: usize) -> Result<(), QueryFailure> {
        self.used = self.used.saturating_add(n);
        if self.used <= self.cap {
            return Ok(());
        }
        Err(too_large(self.cap))
    }
}

/// The 413 `too_large` for a response over `cap` bytes.
pub(crate) fn too_large(cap: usize) -> QueryFailure {
    const MIB: usize = 1024 * 1024;
    let cap = if cap >= MIB && cap.is_multiple_of(MIB) {
        format!("{} MiB", cap / MIB)
    } else {
        format!("{cap} bytes")
    };
    QueryFailure::new(
        StatusCode::PAYLOAD_TOO_LARGE,
        "too_large",
        format!("the response is over {cap}; ask for fewer rows (limit) or fewer columns"),
    )
}

/// One branch's shapes. `held` is the bytes its cache holds, kept exactly by the cache's
/// eviction listener (so it trails an eviction until moka's maintenance runs, never an
/// insert).
struct BranchShapes {
    plans: moka::sync::Cache<ShapeKey, Arc<Vec<Plan>>>,
    held: Arc<AtomicU64>,
}

impl BranchShapes {
    /// Its weight in the global bound: the bytes it holds (at least 1; a weight is never 0).
    fn weight(&self) -> u32 {
        clamp_weight(self.held.load(Ordering::Relaxed).max(1) as usize)
    }
}

/// A moka weight: `bytes`, saturating at `u32::MAX` rather than wrapping.
fn clamp_weight(bytes: usize) -> u32 {
    u32::try_from(bytes).unwrap_or(u32::MAX)
}

/// The bytes a cached plan set weighs: its plans and the key it is stored under.
fn shape_bytes(key: &ShapeKey, plans: &[Plan]) -> usize {
    let (schema_key, role, default_schema, _) = key;
    std::mem::size_of::<ShapeKey>()
        + schema_key.database.len()
        + schema_key.version.len()
        + role.len()
        + default_schema.len()
        + std::mem::size_of::<Vec<Plan>>()
        + plans.iter().map(Plan::approx_bytes).sum::<usize>()
}

/// Plans by shape, bounded in bytes.
///
/// Invariants (tested in `engine::tests`):
/// - a branch holds at most `per_branch` bytes of plans (every database of the branch
///   together; LRU), each plan set weighed by [`shape_bytes`];
/// - the branches together hold at most about `total` bytes: the global cache weighs each
///   branch by the bytes it holds, refreshed on every insert, and evicts whole branches
///   past `total`;
/// - a plan set over [`MAX_CACHED_PLAN_BYTES`] is planned, returned and never inserted, so
///   no single request shape can take more than that from the bound, and the bound holds
///   whatever shapes arrive.
pub struct ShapeCache {
    branches: moka::sync::Cache<Uuid, Arc<BranchShapes>>,
    per_branch: u64,
}

impl Default for ShapeCache {
    fn default() -> Self {
        ShapeCache::with_bounds(SHAPE_BYTES_PER_BRANCH, SHAPE_BYTES_TOTAL)
    }
}

impl ShapeCache {
    /// A cache holding at most `per_branch` bytes of plans per branch and about `total` in
    /// all.
    pub fn with_bounds(per_branch: u64, total: u64) -> Self {
        ShapeCache {
            branches: moka::sync::Cache::builder()
                .max_capacity(total)
                .weigher(|_, b: &Arc<BranchShapes>| b.weight())
                .build(),
            per_branch,
        }
    }

    fn new_branch(&self) -> Arc<BranchShapes> {
        let held = Arc::new(AtomicU64::new(0));
        let released = held.clone();
        let plans = moka::sync::Cache::builder()
            .max_capacity(self.per_branch)
            .eviction_policy(moka::policy::EvictionPolicy::lru())
            .weigher(|k: &ShapeKey, v: &Arc<Vec<Plan>>| clamp_weight(shape_bytes(k, v)))
            .eviction_listener(move |k: Arc<ShapeKey>, v: Arc<Vec<Plan>>, _| {
                released.fetch_sub(shape_bytes(&k, &v) as u64, Ordering::Relaxed);
            })
            .build();
        Arc::new(BranchShapes { plans, held })
    }

    pub fn get_or_plan(
        &self,
        key: ShapeKey,
        make: impl FnOnce() -> Result<Vec<Plan>, PlanError>,
    ) -> Result<Arc<Vec<Plan>>, PlanError> {
        let branch = key.0.branch;
        if let Some(plans) = self
            .branches
            .get(&branch)
            .and_then(|shapes| shapes.plans.get(&key))
        {
            return Ok(plans);
        }
        let plans = Arc::new(make()?);
        let bytes = shape_bytes(&key, &plans);
        if bytes > MAX_CACHED_PLAN_BYTES {
            return Ok(plans);
        }
        let shapes = self.branches.get_with(branch, || self.new_branch());
        shapes.held.fetch_add(bytes as u64, Ordering::Relaxed);
        shapes.plans.insert(key, plans.clone());
        // Re-weigh the branch in the global bound.
        self.branches.insert(branch, shapes);
        Ok(plans)
    }

    /// Drops every plan of `branch`.
    pub fn invalidate_branch(&self, branch: Uuid) {
        self.branches.invalidate(&branch);
    }

    /// Runs moka's pending maintenance (evictions); the bounds hold exactly only after it.
    pub fn run_pending_tasks(&self) {
        self.branches.run_pending_tasks();
        for (_, shapes) in self.branches.iter() {
            shapes.plans.run_pending_tasks();
        }
    }

    /// The bytes of plans the cache holds, and how many plan sets (after
    /// [`ShapeCache::run_pending_tasks`] for exact numbers).
    pub fn held(&self) -> (u64, u64) {
        self.branches.iter().fold((0, 0), |(bytes, sets), (_, b)| {
            (bytes + b.plans.weighted_size(), sets + b.plans.entry_count())
        })
    }

    /// The bytes of plans held for `branch`, if it holds any.
    pub fn branch_bytes(&self, branch: Uuid) -> Option<u64> {
        self.branches.get(&branch).map(|b| b.plans.weighted_size())
    }
}

/// A cached schema's moka weight: its approximate size and its key's.
fn schema_weight(key: &SchemaKey, schema: &Schema) -> u32 {
    clamp_weight(
        schema
            .approx_bytes()
            .saturating_add(std::mem::size_of::<SchemaKey>())
            .saturating_add(key.database.len() + key.version.len()),
    )
}

/// `(branch, database)`.
pub type DatabaseKey = (Uuid, String);

/// The schema cache ([`SchemaKey`] to [`Schema`], each with its roles' views), the shape
/// cache, the last schema version seen per database and the databases found without the
/// guard functions. One per process; every database of every branch shares it.
pub struct QueryCache {
    schemas: moka::future::Cache<SchemaKey, Arc<Schema>>,
    pub shapes: ShapeCache,
    /// The version the sequential path last read in each database: what the pipeline plans
    /// against and asserts.
    pub versions: moka::sync::Cache<DatabaseKey, String>,
    /// Databases missing the guard functions: served sequentially for [`NO_GUARD_TTL`].
    pub no_guard: moka::sync::Cache<DatabaseKey, ()>,
    /// How many requests went out as one pipeline (whatever their outcome, a fallback to the
    /// sequential path after it included): tells the two paths apart in tests and metrics.
    pub pipelined_runs: AtomicU64,
}

impl Default for QueryCache {
    fn default() -> Self {
        QueryCache {
            schemas: moka::future::Cache::builder()
                .max_capacity(SCHEMA_CACHE_BYTES)
                .weigher(|k: &SchemaKey, s: &Arc<Schema>| schema_weight(k, s))
                .support_invalidation_closures()
                .build(),
            shapes: ShapeCache::default(),
            versions: moka::sync::Cache::builder()
                .max_capacity(SCHEMA_CACHE)
                .support_invalidation_closures()
                .build(),
            no_guard: moka::sync::Cache::builder()
                .max_capacity(SCHEMA_CACHE)
                .time_to_live(NO_GUARD_TTL)
                .support_invalidation_closures()
                .build(),
            pipelined_runs: AtomicU64::new(0),
        }
    }
}

impl QueryCache {
    /// The schema at `key`, introspected through `client` (inside the request's
    /// transaction) on a miss.
    pub async fn schema(
        &self,
        client: &Client,
        key: SchemaKey,
        request_id: &str,
    ) -> Result<Arc<Schema>, QueryFailure> {
        let version = key.version.clone();
        self.cached_schema(key, request_id, async {
            schema::introspect(client, version).await
        })
        .await
    }

    /// The schema at `key`, or `make`'s on a miss (an error isn't cached).
    pub async fn cached_schema(
        &self,
        key: SchemaKey,
        request_id: &str,
        make: impl Future<Output = Result<Schema, IntrospectError>>,
    ) -> Result<Arc<Schema>, QueryFailure> {
        self.schemas
            .try_get_with(key, async { make.await.map(Arc::new) })
            .await
            .map_err(|e| match Arc::try_unwrap(e) {
                Ok(e) => introspect_failure(e, request_id),
                Err(e) => QueryFailure::internal(request_id, e),
            })
    }

    /// Drops the cached schemas of `(branch, database)` at any version but `keep`. A request
    /// still planning against an older one holds its own `Arc`, so it is unaffected.
    pub fn forget_other_versions(&self, branch: Uuid, database: &str, keep: &str) {
        let (database, keep) = (database.to_owned(), keep.to_owned());
        let _ = self.schemas.invalidate_entries_if(move |k, _| {
            k.branch == branch && k.database == database && k.version != keep
        });
    }

    /// The bytes and the count of the schemas held (after [`QueryCache::run_pending_tasks`]
    /// for exact numbers).
    pub fn schemas_held(&self) -> (u64, u64) {
        (self.schemas.weighted_size(), self.schemas.entry_count())
    }

    /// Runs moka's pending maintenance on the schema and shape caches.
    pub async fn run_pending_tasks(&self) {
        self.schemas.run_pending_tasks().await;
        self.shapes.run_pending_tasks();
    }

    /// The schema at `key` when the cache holds it.
    pub async fn cached(&self, key: &SchemaKey) -> Option<Arc<Schema>> {
        self.schemas.get(key).await
    }

    /// Drops every schema, plan, version and guard mark of `branch` (a database disabled, a
    /// setting changed).
    pub fn invalidate_branch(&self, branch: Uuid) {
        let _ = self
            .schemas
            .invalidate_entries_if(move |k, _| k.branch == branch);
        self.shapes.invalidate_branch(branch);
        let _ = self
            .versions
            .invalidate_entries_if(move |k, _| k.0 == branch);
        let _ = self
            .no_guard
            .invalidate_entries_if(move |k, _| k.0 == branch);
    }
}

pub fn introspect_failure(e: IntrospectError, request_id: &str) -> QueryFailure {
    match e {
        IntrospectError::Db(e) => QueryFailure::from_pg(&e, request_id),
        IntrospectError::Decode(e) => QueryFailure::internal(request_id, e),
    }
}

/// What every transaction starts with, in one round trip: `ROLLBACK` (ends whatever a
/// dropped request left open) unless the connection is [`TransactionStatus::Idle`] (outside
/// a transaction Postgres logs a warning for it; any other status, `Unknown` included, may
/// be inside one), `BEGIN`, then `reset_sql` (a clean session) inside the transaction: a
/// reset that fails aborts it, so no statement queued behind it can run, and commit,
/// outside a transaction. Read `status` ([`QueryClient::transaction_status`]) right before
/// sending, when nothing is in flight.
pub fn begin_sql(reset_sql: &str, status: TransactionStatus) -> String {
    if status == TransactionStatus::Idle {
        format!("BEGIN; {reset_sql}")
    } else {
        format!("ROLLBACK; BEGIN; {reset_sql}")
    }
}

/// Runs `f` in a transaction on `client`: [`begin_sql`] (`[ROLLBACK;] BEGIN; reset_sql`), `f`,
/// then `COMMIT` when it succeeds, or `ROLLBACK` (best effort: a connection that failed
/// can't take it, and the pool drops it) when it fails, returning `f`'s failure. `reset_sql`
/// runs again after the `COMMIT` or `ROLLBACK`, in the same round trip, so the next request
/// prepares its statements on a clean session (a rolled-back transaction undoes the first
/// reset). When the session can't be reset (either time), the connection is marked broken
/// ([`QueryClient::mark_broken`]); the trailing reset's failure never changes the result.
///
/// A panic in `f` or a dropped future (a client gone, a timeout) rolls nothing back here:
/// a pooled connection ([`crate::Lease`]) is then taken out of the pool and closed, which
/// ends the transaction. For any other client, the `ROLLBACK` that opens the next
/// transaction on it (sent whenever its status isn't idle) is the guard, so stale work is
/// never committed with a later request's.
///
/// A failed `COMMIT` (a deferred key or constraint trigger) goes through
/// [`QueryFailure::from_pg`]; [`with_transaction_mapped`] picks another mapping for it.
pub async fn with_transaction<C: QueryClient + ?Sized, T>(
    client: &C,
    reset_sql: &str,
    request_id: &str,
    f: impl AsyncFnOnce(&Client) -> Result<T, QueryFailure>,
) -> Result<T, QueryFailure> {
    let on_commit = |e: &tokio_postgres::Error| QueryFailure::from_pg(e, request_id);
    with_transaction_mapped(client, reset_sql, request_id, on_commit, f).await
}

/// [`with_transaction`], with `on_commit` mapping the failure of a `COMMIT` that follows an
/// `f` that succeeded: what deferred work (a deferred key, a deferred constraint trigger,
/// whatever those queue) raised once every statement had passed. It sees the Postgres error
/// itself, so a caller that must never show the text of what fired at COMMIT (storage's
/// object delete) can tell it from `f`'s own failures, which pass through untouched.
pub async fn with_transaction_mapped<C: QueryClient + ?Sized, T>(
    client: &C,
    reset_sql: &str,
    request_id: &str,
    on_commit: impl FnOnce(&tokio_postgres::Error) -> QueryFailure,
    f: impl AsyncFnOnce(&Client) -> Result<T, QueryFailure>,
) -> Result<T, QueryFailure> {
    transact(client, reset_sql, None, request_id, on_commit, f).await
}

/// [`with_transaction_mapped`] for a transaction that runs as `role` (`none`: the
/// connection's own login): when `f` succeeds, [`CONSTRAINTS_IMMEDIATE_SQL`] and
/// [`SESSION_CHECK_SQL`] go out before the `COMMIT`, in its round trip
/// ([`closing_steps`]). Deferred work that fails is `on_commit`'s, as when it failed at
/// `COMMIT`. A failed check (the transaction no longer runs as `role`, or the session holds
/// a SQL-level prepared statement) commits nothing: a 500, the details in the log only, and
/// the connection marked broken. A transaction that wrote a table with a deferrable
/// constraint trigger commits, and its connection is marked broken too
/// ([`SESSION_CHECK_SQL`]): deferred work may have run inside its `COMMIT`, after the check.
pub async fn with_checked_transaction<C: QueryClient + ?Sized, T>(
    client: &C,
    reset_sql: &str,
    role: &str,
    request_id: &str,
    on_commit: impl FnOnce(&tokio_postgres::Error) -> QueryFailure,
    f: impl AsyncFnOnce(&Client) -> Result<T, QueryFailure>,
) -> Result<T, QueryFailure> {
    transact(client, reset_sql, Some(role), request_id, on_commit, f).await
}

/// What ends a transaction, in the order sent (one round trip): with the session check
/// (`checked`) the constraints go immediate first, then the check, then the `COMMIT`, then
/// the reset; without it (a rollback, or a transaction no role was set in) the `COMMIT` or
/// `ROLLBACK` and the reset.
pub(crate) fn closing_steps(checked: bool) -> &'static [StepKind] {
    if checked {
        &[
            StepKind::Constraints,
            StepKind::Check,
            StepKind::Commit,
            StepKind::Reset,
        ]
    } else {
        &[StepKind::Commit, StepKind::Reset]
    }
}

/// The 500 of a failed [`SESSION_CHECK_SQL`]: Postgres's text goes to the log only. Logged
/// at warn, not error: a function of the database's own can cause it on every call.
pub(crate) fn session_check_failure(
    role: &str,
    error: impl std::fmt::Display,
    request_id: &str,
) -> QueryFailure {
    tracing::warn!(
        request_id,
        role,
        "the session check before COMMIT failed; nothing was committed and the connection is discarded: {error}"
    );
    QueryFailure::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        format!("internal error (request id {request_id})"),
    )
}

async fn transact<C: QueryClient + ?Sized, T>(
    client: &C,
    reset_sql: &str,
    check: Option<&str>,
    request_id: &str,
    on_commit: impl FnOnce(&tokio_postgres::Error) -> QueryFailure,
    f: impl AsyncFnOnce(&Client) -> Result<T, QueryFailure>,
) -> Result<T, QueryFailure> {
    let c = client.client();
    let status = client.transaction_status();
    let begin = begin_sql(client.opening_reset(reset_sql, status), status);
    if let Err(e) = c.batch_execute(&begin).await {
        tracing::warn!(
            request_id,
            "the transaction couldn't begin on a clean session; the connection is discarded: {e}"
        );
        client.mark_broken();
        return Err(pipeline::begin_failure(&e, request_id));
    }
    let out = f(c).await;
    let end = if out.is_ok() { "COMMIT" } else { "ROLLBACK" };
    // Only work that would commit is checked: a rollback keeps nothing. Nor does a
    // transaction a failed statement already aborted, when `f` answered for that statement
    // itself (its `Ok` carries the answer): the `COMMIT` rolls it back, and `f`'s answer
    // stands as it always did. The status is the connection's own (from the server's
    // `ReadyForQuery`), read while nothing is in flight; an error's SQLSTATE is never
    // taken for it (the request's own work can raise any).
    let mut status = client.transaction_status();
    if check.is_some() && out.is_ok() && status == TransactionStatus::Unknown {
        // In practice only after a statement of `f`'s failed: the driver can hand a failed
        // statement's error back just before it has read the status that follows it, while a
        // statement that succeeded resolves with its status already read. So a request whose
        // statements all succeeded never sends this. An empty query settles it (one more round trip, on this
        // path only), so the choice below never rests on a status not yet known. If this
        // fails, the status stays as it is and the steps before `COMMIT` are sent.
        let _ = c.batch_execute("").await;
        status = client.transaction_status();
    }
    let aborted = status == TransactionStatus::Failed;
    let check = check.filter(|_| out.is_ok() && !aborted);
    let kinds = closing_steps(check.is_some());
    // Set by the check's row ([`wrote_deferrable`]).
    let deferrable = std::sync::atomic::AtomicBool::new(false);
    let deferrable = &deferrable;
    type Out = Result<(), tokio_postgres::Error>;
    let steps: Vec<Step<'_, Out>> = kinds
        .iter()
        .map(|kind| -> Step<'_, Out> {
            match kind {
                StepKind::Constraints => Box::pin(c.batch_execute(CONSTRAINTS_IMMEDIATE_SQL)),
                StepKind::Check => Box::pin(async move {
                    let role = check.unwrap_or_default();
                    c.query_typed_one(SESSION_CHECK_SQL, &[(&role, Type::TEXT)])
                        .await
                        .map(|row| deferrable.store(wrote_deferrable(&row), Ordering::Relaxed))
                }),
                StepKind::Reset => Box::pin(c.batch_execute(reset_sql)),
                _ => Box::pin(c.batch_execute(end)),
            }
        })
        .collect();
    let mut results = join_in_order(steps).await;
    take_trailing_reset(&mut results, client, request_id);
    let ended = results.pop().expect("the COMMIT or ROLLBACK step");
    let out = match out {
        Ok(out) => out,
        Err(failure) => {
            if let Err(e) = ended {
                tracing::debug!(request_id, "the rollback failed: {e}");
            }
            return Err(failure);
        }
    };
    if let Some(role) = check {
        let checked = results.pop().expect("the session check step");
        let constraints = results.pop().expect("the SET CONSTRAINTS step");
        // Deferred work that failed: the transaction is aborted, and the COMMIT behind it
        // rolled back.
        if let Err(e) = constraints {
            return Err(on_commit(&e));
        }
        if let Err(e) = checked {
            client.mark_broken();
            return Err(session_check_failure(role, &e, request_id));
        }
        // Whatever the COMMIT answered: deferred work may have run in it.
        if deferrable.load(Ordering::Relaxed) {
            discard_after_deferrable_write(client, request_id);
        }
    }
    ended.map_err(|e| on_commit(&e))?;
    Ok(out)
}

/// The sequential path's transaction opening, for callers that run their own statements
/// (the console, the realtime check): [`with_transaction`] and the [`preamble`] for `run_as`
/// (`statement_timeout_ms` from `settings`), then `f` with the connection and the schema
/// version the preamble read; the `COMMIT` (or `ROLLBACK`) and the trailing reset are
/// [`with_transaction`]'s. A preamble that fails (a dropped role, a database without
/// `chelabase.schema_version`: 503, naming `database` as the sequential path's does) never
/// runs `f` and never commits.
pub async fn console_session<C: QueryClient + ?Sized, T>(
    client: &C,
    database: &str,
    run_as: RunAs<'_>,
    settings: &RunSettings,
    request_id: &str,
    f: impl AsyncFnOnce(&Client, &str) -> Result<T, QueryFailure>,
) -> Result<T, QueryFailure> {
    let on_commit = |e: &tokio_postgres::Error| QueryFailure::from_pg(e, request_id);
    console_session_mapped(client, database, run_as, settings, request_id, on_commit, f).await
}

/// [`console_session`], with a failed `COMMIT` mapped by `on_commit`
/// ([`with_transaction_mapped`]).
pub async fn console_session_mapped<C: QueryClient + ?Sized, T>(
    client: &C,
    database: &str,
    run_as: RunAs<'_>,
    settings: &RunSettings,
    request_id: &str,
    on_commit: impl FnOnce(&tokio_postgres::Error) -> QueryFailure,
    f: impl AsyncFnOnce(&Client, &str) -> Result<T, QueryFailure>,
) -> Result<T, QueryFailure> {
    let reset = settings.reset_sql;
    with_checked_transaction(client, reset, run_as.role(), request_id, on_commit, async |c| {
        let pre = preamble(c, run_as, settings.statement_timeout_ms, request_id)
            .await
            .map_err(|f| in_database(f, database))?;
        f(c, &pre.version).await
    })
    .await
}

/// What the preamble read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preamble {
    pub version: String,
    /// The role the request runs as: [`RunAs::Role`]'s, or the owner's user name.
    pub role: String,
}

/// Runs the preamble for `run_as` (see the module docs).
pub async fn preamble(
    client: &Client,
    run_as: RunAs<'_>,
    statement_timeout_ms: i64,
    request_id: &str,
) -> Result<Preamble, QueryFailure> {
    let timeout = statement_timeout_ms.to_string();
    let read = match run_as {
        RunAs::Role { role, claims, oid } => {
            let claims = claims.to_string();
            client
                .query_typed_one(
                    PREAMBLE_SQL,
                    &[
                        (&role, Type::TEXT),
                        (&claims, Type::TEXT),
                        (&timeout, Type::TEXT),
                        (&oid, Type::INT8),
                    ],
                )
                .await
                .and_then(|row| Ok((role.to_owned(), row.try_get::<_, Option<String>>(4)?)))
        }
        RunAs::Owner { .. } => client
            .query_typed_one(OWNER_PREAMBLE_SQL, &[(&timeout, Type::TEXT)])
            .await
            .and_then(|row| Ok((row.try_get::<_, String>(2)?, row.try_get(3)?))),
    };
    match read {
        Ok((role, _)) if role != run_as.role() => Err(QueryFailure::internal(
            request_id,
            format!(
                "the owner's connection runs as {role}, not {}",
                run_as.role()
            ),
        )),
        Ok((role, Some(version))) => Ok(Preamble { version, role }),
        Ok((_, None)) => Err(QueryFailure::unavailable(SCHEMA_MISSING)),
        Err(e) => Err(match e.as_db_error() {
            Some(db) => preamble_db_failure(db, run_as, request_id),
            None => QueryFailure::from_pg(&e, request_id),
        }),
    }
}

/// The preamble failed in Postgres: [`preamble_failure`] for a role; for the owner, the
/// missing machinery or the error as mapped.
pub(crate) fn preamble_db_failure(
    db: &dyn DbFields,
    run_as: RunAs<'_>,
    request_id: &str,
) -> QueryFailure {
    match run_as {
        RunAs::Role { role, .. } => preamble_failure(db, role, request_id),
        RunAs::Owner { .. } if schema_missing(db) => QueryFailure::unavailable(SCHEMA_MISSING),
        RunAs::Owner { .. } => QueryFailure::from_db(db, request_id),
    }
}

/// Whether a preamble error says the machinery schema (or its version table) is missing:
/// Postgres's "doesn't exist" for that table (42P01) or that schema (3F000), nothing else.
fn schema_missing(db: &dyn DbFields) -> bool {
    let message = db.message();
    match db.code() {
        "42P01" => message.contains("schema_version"),
        "3F000" => message.contains("\"chelabase\""),
        _ => false,
    }
}

/// `f` with the missing-machinery text naming `database`; any other failure as it is.
pub(crate) fn in_database(f: QueryFailure, database: &str) -> QueryFailure {
    if f.status == StatusCode::SERVICE_UNAVAILABLE && f.message == SCHEMA_MISSING {
        QueryFailure::unavailable(schema_missing_in(database))
    } else {
        f
    }
}

/// The preamble failed in Postgres: the machinery schema is missing, or the key's role
/// can't be used. Postgres's text goes to the log only.
pub fn preamble_failure(db: &dyn DbFields, role: &str, request_id: &str) -> QueryFailure {
    if schema_missing(db) {
        return QueryFailure::unavailable(SCHEMA_MISSING);
    }
    let code = db.code();
    if classify(code).is_some_and(|(status, _)| status == 503) {
        return QueryFailure::from_db(db, request_id);
    }
    // The key's role is gone or not granted to authenticator (a restore can rewind the
    // grant until the bootstrap runs again).
    tracing::warn!(
        request_id,
        role,
        "the preamble failed: {code}: {}",
        db.message()
    );
    role_unusable(role)
}

/// The 503 a request answers when its role can't be used on the login it runs on: the
/// preamble's role switch was refused, or the role may run on neither login right now.
pub fn role_unusable(role: &str) -> QueryFailure {
    QueryFailure::unavailable(format!(
        "the key's role {role} can't be used right now; try again"
    ))
}

/// The role's view of the schema, computed once per schema version.
pub async fn view(
    client: &Client,
    schema: &Schema,
    role: &str,
    request_id: &str,
) -> Result<Arc<RoleView>, QueryFailure> {
    schema::role_view(client, schema, role)
        .await
        .map_err(|e| introspect_failure(e, request_id))
}

/// Runs the preamble, then reads the schema of database `key = (branch, database)` at the
/// version it read (from the cache when it holds it), inside a transaction the caller
/// opened ([`with_transaction`]). The schema's key and the role the request runs as come
/// back with it. The version read becomes the database's in [`QueryCache::versions`].
pub async fn load_schema(
    client: &Client,
    cache: &QueryCache,
    key: (Uuid, &str),
    run_as: RunAs<'_>,
    statement_timeout_ms: i64,
    request_id: &str,
) -> Result<(Arc<Schema>, SchemaKey, String), QueryFailure> {
    let pre = preamble(client, run_as, statement_timeout_ms, request_id)
        .await
        .map_err(|f| in_database(f, key.1))?;
    let schema_key = SchemaKey {
        branch: key.0,
        database: key.1.to_owned(),
        version: pre.version,
    };
    let database = (key.0, key.1.to_owned());
    let previous = cache.versions.get(&database);
    cache
        .versions
        .insert(database, schema_key.version.clone());
    if previous.is_some_and(|v| v != schema_key.version) {
        // The database moved on: nothing will ask for its older schemas again.
        cache.forget_other_versions(key.0, key.1, &schema_key.version);
    }
    let schema = cache.schema(client, schema_key.clone(), request_id).await?;
    Ok((schema, schema_key, pre.role))
}

/// In its own transaction: runs the preamble, then reads the schema and `run_as`'s view of
/// it (from the cache when it holds them). `key` is `(branch, database)`.
pub async fn schema_for<C: QueryClient + ?Sized>(
    client: &C,
    cache: &QueryCache,
    key: (Uuid, &str),
    run_as: RunAs<'_>,
    settings: &RunSettings,
    request_id: &str,
) -> Result<(Arc<Schema>, Arc<RoleView>), QueryFailure> {
    let on_commit = |e: &tokio_postgres::Error| QueryFailure::from_pg(e, request_id);
    let reset = settings.reset_sql;
    with_checked_transaction(client, reset, run_as.role(), request_id, on_commit, async |client| {
        let (schema, _, role) = load_schema(
            client,
            cache,
            key,
            run_as,
            settings.statement_timeout_ms,
            request_id,
        )
        .await?;
        let view = view(client, &schema, &role, request_id).await?;
        Ok((schema, view))
    })
    .await
}

/// Runs `op` as `run_as` in database `key = (branch, database)`, in its own transaction:
/// the JSON result, unparsed. `budget` caps the response's bytes.
///
/// Pipelined in one round trip when the database's version, schema and view are cached
/// ([`crate::pipeline`]); otherwise, or when the version is stale or the guard functions
/// are missing, sequentially (once), which records the version for the next request.
#[allow(clippy::too_many_arguments)]
pub async fn run<C: QueryClient + ?Sized>(
    client: &C,
    cache: &QueryCache,
    key: (Uuid, &str),
    run_as: RunAs<'_>,
    settings: &RunSettings,
    budget: usize,
    op: &Op,
    request_id: &str,
) -> Result<String, QueryFailure> {
    let attempt = pipelined(client, cache, key, run_as, settings, budget, op, request_id).await;
    pipeline::settle(cache, key, attempt, async || {
        let on_commit = |e: &tokio_postgres::Error| QueryFailure::from_pg(e, request_id);
        let reset = settings.reset_sql;
        with_checked_transaction(client, reset, run_as.role(), request_id, on_commit, async |c| {
            run_sequential(c, cache, key, run_as, settings, budget, op, request_id).await
        })
        .await
    })
    .await
}

/// [`run`]'s pipelined attempt, or [`Attempt::Sequential`] when it can't run: no version
/// known for the database, its guard functions known missing, its schema or the role's
/// view not cached, or a plan that fails against the cached schema (the sequential path
/// plans against the current one and answers).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn pipelined<C: QueryClient + ?Sized>(
    client: &C,
    cache: &QueryCache,
    key: (Uuid, &str),
    run_as: RunAs<'_>,
    settings: &RunSettings,
    budget: usize,
    op: &Op,
    request_id: &str,
) -> Attempt {
    let database = (key.0, key.1.to_owned());
    if cache.no_guard.contains_key(&database) {
        return Attempt::Sequential;
    }
    let Some(version) = cache.versions.get(&database) else {
        return Attempt::Sequential;
    };
    let Ok(expected) = version.parse::<Uuid>() else {
        return Attempt::Sequential;
    };
    let schema_key = SchemaKey {
        branch: key.0,
        database: database.1,
        version,
    };
    let Some(schema) = cache.cached(&schema_key).await else {
        return Attempt::Sequential;
    };
    let role = run_as.role();
    let Some(view) = schema.cached_view(role) else {
        return Attempt::Sequential;
    };
    let default_schema = &settings.default_schema;
    let shape = (
        schema_key,
        role.to_owned(),
        default_schema.clone(),
        shape_hash(op),
    );
    let Ok(plans) = cache
        .shapes
        .get_or_plan(shape, || plan(op, view.as_ref(), default_schema))
    else {
        return Attempt::Sequential;
    };

    let cap = sql_budget(budget, plans.len());
    let mut calls = Vec::with_capacity(plans.len());
    for p in plans.iter() {
        let mut values = match plan_params(op, p, settings.max_rows, request_id) {
            Ok(values) => values,
            Err(f) => return Attempt::Done(Err(f)),
        };
        values.push(Box::new(cap));
        calls.push(Call {
            sql: &p.capped_sql,
            types: p.capped_param_types().into_iter().map(pg_type).collect(),
            values,
        });
    }
    let timeout = settings.statement_timeout_ms.to_string();
    let preamble = match run_as {
        RunAs::Role { role, claims, oid } => Call {
            sql: GUARDED_PREAMBLE_SQL,
            types: vec![Type::TEXT, Type::TEXT, Type::TEXT, Type::INT8, Type::UUID],
            values: vec![
                Box::new(role),
                Box::new(claims.to_string()),
                Box::new(timeout),
                Box::new(oid),
                Box::new(expected),
            ],
        },
        RunAs::Owner { .. } => Call {
            sql: OWNER_GUARDED_PREAMBLE_SQL,
            types: vec![Type::TEXT, Type::UUID],
            values: vec![Box::new(timeout), Box::new(expected)],
        },
    };
    let ctx = Ctx {
        run_as,
        database: key.1,
        budget,
        request_id,
        runs: &cache.pipelined_runs,
    };
    pipeline::attempt(client, settings.reset_sql, preamble, calls, op, &ctx).await
}

/// The plans' results as the response: a batch's joined into an array, else the one result.
pub(crate) fn join_results(op: &Op, mut results: Vec<String>) -> String {
    match op {
        Op::Batch(_) => format!("[{}]", results.join(",")),
        _ => results.pop().unwrap_or_else(|| "null".into()),
    }
}

/// [`run`]'s work one statement at a time, inside a transaction the caller opened: the
/// preamble, the schema and view, the plans, then each plan, its result counted against
/// `budget`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_sequential(
    client: &Client,
    cache: &QueryCache,
    key: (Uuid, &str),
    run_as: RunAs<'_>,
    settings: &RunSettings,
    budget: usize,
    op: &Op,
    request_id: &str,
) -> Result<String, QueryFailure> {
    let (schema, schema_key, role) = load_schema(
        client,
        cache,
        key,
        run_as,
        settings.statement_timeout_ms,
        request_id,
    )
    .await?;
    let view = view(client, &schema, &role, request_id).await?;
    let default_schema = &settings.default_schema;
    let shape = (schema_key, role, default_schema.clone(), shape_hash(op));
    let plans = cache
        .shapes
        .get_or_plan(shape, || plan(op, view.as_ref(), default_schema))?;
    let mut results = Vec::with_capacity(plans.len());
    // Each result, and a separator or bracket next to it.
    let mut budget = ResponseBudget::new(budget);
    for p in plans.iter() {
        let result = run_plan(client, op, p, settings.max_rows, request_id).await?;
        budget.take(result.len() + 1)?;
        results.push(result);
    }
    Ok(join_results(op, results))
}

/// `p`'s values from `op`, in parameter order (`max_rows` caps the limits).
pub(crate) fn plan_params<'a>(
    op: &'a Op,
    p: &Plan,
    max_rows: i64,
    request_id: &str,
) -> Result<Vec<Param<'a>>, QueryFailure> {
    p.binds
        .iter()
        .map(|bind| {
            Ok(match resolve_bind(op, bind, max_rows) {
                Some(BindValue::Text(v)) => Box::new(v) as Param<'a>,
                Some(BindValue::TextArray(v)) => Box::new(v),
                Some(BindValue::Json(v)) => Box::new(v),
                Some(BindValue::Int(v)) => Box::new(v),
                None => {
                    return Err(QueryFailure::internal(
                        request_id,
                        format!("a cached plan doesn't fit the tree: {bind:?}"),
                    ));
                }
            })
        })
        .collect()
}

/// The Postgres type a plan parameter is declared with ([`Plan::param_types`]).
pub(crate) fn pg_type(t: ParamType) -> Type {
    match t {
        ParamType::Text => Type::TEXT,
        ParamType::Jsonb => Type::JSONB,
        ParamType::Int8 => Type::INT8,
        ParamType::TextArray => Type::TEXT_ARRAY,
    }
}

/// Runs one plan with `op`'s values: its one `text` row, or `null` for a NULL (a call to a
/// `void` function).
async fn run_plan(
    client: &Client,
    op: &Op,
    p: &Plan,
    max_rows: i64,
    request_id: &str,
) -> Result<String, QueryFailure> {
    let values = plan_params(op, p, max_rows, request_id)?;
    let params: Vec<(&(dyn ToSql + Sync), Type)> = values
        .iter()
        .map(|v| v.as_ref() as &(dyn ToSql + Sync))
        .zip(p.param_types().into_iter().map(pg_type))
        .collect();
    // The planner's text: names from the catalog, quoted; every value a parameter.
    let out = client
        .query_typed_one(&p.sql, &params)
        .await
        .and_then(|row| row.try_get::<_, Option<String>>(0))
        .map_err(|e| QueryFailure::from_pg(&e, request_id))?;
    Ok(out.unwrap_or_else(|| "null".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SCHEMA_MISSING;
    use crate::error::tests::FakeDb;
    use crate::schema::Raw;
    use crate::sql::{Catalog, ColumnInfo, FnLookupError, FunctionInfo, TableInfo};

    fn key(role: &str, hash: u64) -> ShapeKey {
        (
            schema_key(Uuid::nil(), "main", "v1"),
            role.into(),
            "public".into(),
            hash,
        )
    }

    fn schema_key(branch: Uuid, database: &str, version: &str) -> SchemaKey {
        SchemaKey {
            branch,
            database: database.into(),
            version: version.into(),
        }
    }

    /// A role Postgres refuses, or a missing machinery schema, answers a fixed message.
    #[test]
    fn connection_and_role_failures_are_fixed_messages() {
        const SECRET: &str = "10.1.2.3:5432: permission denied to set role \"x\"";
        let db = FakeDb::new("42501", SECRET);
        let e = preamble_failure(&db, "x", "rid-1");
        assert_eq!(e.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            !e.message.contains("10.1.2.3") && e.message.contains("x"),
            "{}",
            e.message
        );
        let missing = FakeDb::new(
            "42P01",
            "relation \"chelabase.schema_version\" does not exist",
        );
        assert_eq!(
            preamble_failure(&missing, "x", "rid-1").message,
            SCHEMA_MISSING
        );
    }

    /// Only Postgres saying the version table (or the `chelabase` schema) doesn't exist means
    /// the machinery is missing; any other error naming them (a refused privilege, a raise)
    /// doesn't.
    #[test]
    fn schema_missing_needs_the_sqlstate_and_the_name() {
        for (code, message, missing) in [
            (
                "42P01",
                "relation \"chelabase.schema_version\" does not exist",
                true,
            ),
            ("3F000", "schema \"chelabase\" does not exist", true),
            ("42P01", "relation \"notes\" does not exist", false),
            ("3F000", "schema \"app\" does not exist", false),
            ("42501", "permission denied for schema chelabase", false),
            ("42501", "permission denied for table schema_version", false),
            ("P0001", "chelabase schema_version check failed", false),
        ] {
            assert_eq!(
                schema_missing(&FakeDb::new(code, message)),
                missing,
                "{code} {message}"
            );
        }
    }

    /// The missing-machinery 503 names the database (spec Part 3); other failures pass as
    /// they are.
    #[test]
    fn the_missing_schema_text_names_the_database() {
        let e = in_database(QueryFailure::unavailable(SCHEMA_MISSING), "analytics");
        assert_eq!(e.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(e.message, schema_missing_in("analytics"));
        assert_eq!(
            e.message,
            "the data API's schema is missing in the database \"analytics\"; it will be reinstalled"
        );
        let other =
            QueryFailure::unavailable("the key's role x can't be used right now; try again");
        assert_eq!(in_database(other.clone(), "analytics"), other);
    }

    /// The preamble bounds the whole transaction (a batch of slow statements included) with
    /// the same setting as each statement.
    #[test]
    fn preamble_sets_the_transaction_timeout() {
        assert!(
            PREAMBLE_SQL.contains("set_config('statement_timeout', $3, true)")
                && PREAMBLE_SQL.contains("set_config('transaction_timeout', $3, true)"),
            "{PREAMBLE_SQL}"
        );
    }

    /// A secret key runs as its role only while the role of that name still has the key's
    /// OID (`$4`): a dropped role, or another one created under the name, makes the
    /// `SET ROLE` target `''`, which no role can be named, so the preamble fails as for a
    /// missing role. No OID (`NULL`: the public key, a user token, a key not backfilled yet)
    /// checks the name only. One expression, so there is no window between check and use.
    #[test]
    fn preamble_binds_the_role_to_its_oid() {
        let guard = "pg_catalog.set_config('role', CASE WHEN $4::pg_catalog.int8 IS NULL OR \
             (SELECT r.oid::pg_catalog.int8 FROM pg_catalog.pg_roles r \
             WHERE r.rolname OPERATOR(pg_catalog.=) $1) OPERATOR(pg_catalog.=) $4::pg_catalog.int8 \
             THEN $1 ELSE '' END, true)";
        assert!(PREAMBLE_SQL.starts_with(&format!("SELECT {guard}, ")), "{PREAMBLE_SQL}");
        assert!(GUARDED_PREAMBLE_SQL.starts_with(&format!("SELECT {guard}, ")), "{GUARDED_PREAMBLE_SQL}");
        assert!(!PREAMBLE_SQL.contains("set_config('role', $1"), "{PREAMBLE_SQL}");
    }

    /// The owner runs as itself: no `SET ROLE`, no claims, but the same timeouts and the
    /// same version read (P5).
    #[test]
    fn owner_preamble_sets_no_role() {
        assert_eq!(
            OWNER_PREAMBLE_SQL,
            "SELECT pg_catalog.set_config('statement_timeout', $1, true), \
             pg_catalog.set_config('transaction_timeout', $1, true), current_user::pg_catalog.text, \
             (SELECT version::pg_catalog.text FROM chelabase.schema_version)"
        );
        assert!(!OWNER_PREAMBLE_SQL.contains("'role'"));
        assert!(!OWNER_PREAMBLE_SQL.contains("request.jwt.claims"));
    }

    /// `public.items (id bigint, body jsonb)`.
    struct Items(TableInfo);

    impl Catalog for Items {
        fn table(&self, name: &crate::query::Name) -> Option<&TableInfo> {
            (name.schema.as_deref() == Some("public") && name.name == "items").then_some(&self.0)
        }
        fn function(
            &self,
            _: &crate::query::Name,
            _: &[&str],
        ) -> Result<&FunctionInfo, FnLookupError> {
            Err(FnLookupError::NotFound)
        }
    }

    fn planned(tree: serde_json::Value) -> (Op, Plan) {
        let col = |name: &str, ty: &str| ColumnInfo {
            name: name.into(),
            type_sql: ty.into(),
            type_qualified: String::new(),
            array: false,
            text_like: false,
            generated: false,
        };
        let catalog = Items(TableInfo {
            schema: "public".into(),
            name: "items".into(),
            columns: vec![col("id", "bigint"), col("body", "jsonb")],
        });
        let op = crate::parse(tree.to_string().as_bytes()).unwrap().op;
        let mut plans = plan(&op, &catalog, "public").unwrap();
        assert_eq!(plans.len(), 1);
        (op, plans.remove(0))
    }

    /// Every parameter is declared as sqlx sent it (`text` for a filter value the SQL casts,
    /// `int8` for a limit or offset, `jsonb` for inserted rows), so the server never infers
    /// one: an inferred `CAST($1 AS bigint)` would be an `int8` the text value can't bind to.
    #[test]
    fn prepared_types_match_sqlx() {
        let (op, select) = planned(serde_json::json!({
            "v": 1, "op": "select", "from": "items",
            "where": {"col": "id", "eq": 7}, "limit": 5,
        }));
        assert!(select.sql.contains("CAST($1 AS bigint)"), "{}", select.sql);
        let types = |p: &Plan| p.param_types().into_iter().map(pg_type).collect::<Vec<_>>();
        assert_eq!(types(&select), [Type::TEXT, Type::INT8, Type::INT8]);
        assert_eq!(plan_params(&op, &select, 1000, "rid").unwrap().len(), 3);

        let (op, insert) = planned(serde_json::json!({
            "v": 1, "op": "insert", "into": "items",
            "values": [{"id": 1, "body": {"a": 1}}], "returning": ["id"],
        }));
        assert_eq!(types(&insert), [Type::JSONB, Type::INT8]);
        assert_eq!(
            pg_type(crate::sql::ParamType::Jsonb),
            Type::JSONB,
            "each ParamType maps to its Postgres type"
        );
        assert_eq!(plan_params(&op, &insert, 1000, "rid").unwrap().len(), 2);
    }

    /// The pipeline's preambles are the sequential ones with the version asserted in place
    /// of read (the owner's still returns `current_user`).
    #[test]
    fn guarded_preambles_assert_the_version() {
        assert_eq!(
            GUARDED_PREAMBLE_SQL,
            PREAMBLE_SQL.replace(
                "(SELECT version::pg_catalog.text FROM chelabase.schema_version)",
                "chelabase.assert_schema_version($5)"
            )
        );
        assert_eq!(
            OWNER_GUARDED_PREAMBLE_SQL,
            OWNER_PREAMBLE_SQL.replace(
                "(SELECT version::pg_catalog.text FROM chelabase.schema_version)",
                "chelabase.assert_schema_version($2)"
            )
        );
        assert_eq!(RunAs::Owner { user: "owner" }.role(), "owner");
        let claims = serde_json::json!({});
        assert_eq!(
            RunAs::Role {
                role: "anon",
                claims: &claims,
                oid: None,
            }
            .role(),
            "anon"
        );
    }

    /// The pipeline runs only with a known version, its schema and the role's view cached,
    /// and the guard functions not known missing; otherwise the sequential path answers
    /// (and nothing is sent: the client here is closed).
    #[tokio::test]
    async fn the_pipeline_needs_a_known_version_and_the_guard() {
        let client = crate::error::tests::closed_client().await;
        let cache = QueryCache::default();
        let branch = Uuid::new_v4();
        let version = Uuid::new_v4().to_string();
        let claims = serde_json::json!({});
        let run_as = RunAs::Role {
            role: "anon",
            claims: &claims,
            oid: None,
        };
        let settings = RunSettings {
            max_rows: 1000,
            statement_timeout_ms: 5000,
            default_schema: "public".into(),
            reset_sql: "RESET ALL",
        };
        let op = crate::parse(br#"{"v":1,"op":"select","from":"t"}"#)
            .unwrap()
            .op;
        let attempt = async || {
            pipelined(
                &client,
                &cache,
                (branch, "main"),
                run_as,
                &settings,
                100,
                &op,
                "rid",
            )
            .await
        };
        // No version known.
        assert!(matches!(attempt().await, Attempt::Sequential));
        // A version, but not its schema.
        cache
            .versions
            .insert((branch, "main".into()), version.clone());
        assert!(matches!(attempt().await, Attempt::Sequential));
        // Its schema, but not the role's view.
        let schema = cache
            .cached_schema(schema_key(branch, "main", &version), "rid", async {
                Ok(Schema::build(version.clone(), Raw::default()))
            })
            .await
            .unwrap();
        assert!(matches!(attempt().await, Attempt::Sequential));
        // The view too: the plan fails against the cached schema (no table `t`), and the
        // sequential path, reading the current schema, answers.
        schema.insert_view("anon", Default::default());
        assert!(matches!(attempt().await, Attempt::Sequential));
        // A database marked as missing the guard functions.
        cache.no_guard.insert((branch, "main".into()), ());
        assert!(matches!(attempt().await, Attempt::Sequential));
    }

    /// The opening reset drops the prepared-statement check only for a pooled connection
    /// that is idle and not marked broken: its last transaction ended with the full reset,
    /// or it would have been closed. Any other client, and any other state, gets the full
    /// text.
    #[tokio::test]
    async fn opening_reset_skips_the_check_only_on_a_clean_pooled_connection() {
        let light = "SET ROLE NONE; RESET ALL; SELECT pg_catalog.pg_advisory_unlock_all(); \
                     DISCARD TEMP; CLOSE ALL; UNLISTEN *; DISCARD SEQUENCES";
        assert_eq!(opening_reset_sql(POOLED_RESET_SQL), light);
        assert_eq!(
            format!("{light}; {NO_SQL_PREPARED_SQL}"),
            POOLED_RESET_SQL,
            "only the check is left out"
        );
        // A reset without the check is sent whole.
        assert_eq!(opening_reset_sql("RESET ALL"), "RESET ALL");
        assert_eq!(opening_reset_sql(light), light);

        // A plain client isn't known clean: the full reset, whatever its status.
        let plain = crate::error::tests::closed_client().await;
        for status in [TransactionStatus::Idle, TransactionStatus::Unknown] {
            assert_eq!(plain.opening_reset(POOLED_RESET_SQL, status), POOLED_RESET_SQL);
        }

        let pool = crate::fake_pg::pool(1).await;
        let lease = crate::pipeline::Lease::new(pool.get().await.unwrap());
        assert_eq!(lease.opening_reset(POOLED_RESET_SQL, TransactionStatus::Idle), light);
        // Maybe inside a transaction a dropped request left: the full reset.
        for status in [
            TransactionStatus::InTransaction,
            TransactionStatus::Failed,
            TransactionStatus::Unknown,
        ] {
            assert_eq!(
                lease.opening_reset(POOLED_RESET_SQL, status),
                POOLED_RESET_SQL,
                "{status:?}"
            );
        }
        // A reset or a check failed earlier on this lease: the full reset.
        lease.mark_broken();
        assert_eq!(
            lease.opening_reset(POOLED_RESET_SQL, TransactionStatus::Idle),
            POOLED_RESET_SQL
        );
    }

    /// Every transaction starts by ending whatever a dropped request may have left open
    /// (unless the connection is idle: outside a transaction Postgres logs a warning), then
    /// begins, then resets the session inside the transaction: one round trip.
    #[test]
    fn begin_sql_rolls_back_only_when_not_idle() {
        // BEGIN before the reset: a failing reset aborts the transaction, so nothing queued
        // behind it can run (and commit) outside one.
        let reset = "SET ROLE NONE; RESET ALL";
        assert_eq!(
            begin_sql(reset, TransactionStatus::Idle),
            "BEGIN; SET ROLE NONE; RESET ALL"
        );
        for status in [
            TransactionStatus::InTransaction,
            TransactionStatus::Failed,
            TransactionStatus::Unknown,
        ] {
            assert_eq!(
                begin_sql(reset, status),
                "ROLLBACK; BEGIN; SET ROLE NONE; RESET ALL",
                "{status:?}"
            );
        }
    }

    /// rest serves requests from axum handlers: every entry point's future must be `Send`,
    /// on a plain client and on a pooled one.
    #[allow(dead_code, clippy::too_many_arguments)]
    fn entry_points_are_send(
        client: &Client,
        pooled: &crate::pipeline::Lease,
        cache: &QueryCache,
        run_as: RunAs<'_>,
        settings: &RunSettings,
        op: &Op,
    ) {
        fn send<T: Send>(_: T) {}
        send(run(
            pooled,
            cache,
            (Uuid::nil(), "main"),
            run_as,
            settings,
            1,
            op,
            "rid",
        ));
        send(schema_for(
            pooled,
            cache,
            (Uuid::nil(), "main"),
            run_as,
            settings,
            "rid",
        ));
        send(run(
            client,
            cache,
            (Uuid::nil(), "main"),
            run_as,
            settings,
            1,
            op,
            "rid",
        ));
        send(schema_for(
            client,
            cache,
            (Uuid::nil(), "main"),
            run_as,
            settings,
            "rid",
        ));
        send(load_schema(
            client,
            cache,
            (Uuid::nil(), "main"),
            run_as,
            1,
            "rid",
        ));
        send(with_transaction(client, "RESET ALL", "rid", async |c| {
            c.batch_execute("SELECT 1")
                .await
                .map_err(|e| QueryFailure::from_pg(&e, "rid"))
        }));
    }

    #[test]
    fn the_response_budget_refuses_past_the_cap() {
        assert_eq!(DEFAULT_MAX_RESPONSE_BYTES, 16 * 1024 * 1024);
        let mut budget = ResponseBudget::new(10);
        budget.take(4).unwrap();
        budget.take(6).unwrap();
        let e = budget.take(1).unwrap_err();
        assert_eq!(e.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(e.code, "too_large");
        assert!(e.message.contains("10 bytes"), "{}", e.message);
        let e = ResponseBudget::new(DEFAULT_MAX_RESPONSE_BYTES)
            .take(DEFAULT_MAX_RESPONSE_BYTES + 1)
            .unwrap_err();
        assert!(e.message.contains("16 MiB"), "{}", e.message);
    }

    #[test]
    fn shape_cache_plans_once_per_key_and_keeps_roles_apart() {
        let cache = ShapeCache::default();
        let branch = Uuid::new_v4();
        let made = std::cell::Cell::new(0);
        let get = |branch: Uuid, role: &str, hash: u64| {
            let mut k = key(role, hash);
            k.0.branch = branch;
            cache
                .get_or_plan(k, || {
                    made.set(made.get() + 1);
                    Ok(vec![])
                })
                .unwrap()
        };
        get(branch, "anon", 1);
        get(branch, "anon", 1);
        assert_eq!(made.get(), 1);
        get(branch, "authenticated", 1);
        get(branch, "anon", 2);
        assert_eq!(made.get(), 3);
        // Another branch never shares a plan.
        get(Uuid::new_v4(), "anon", 1);
        assert_eq!(made.get(), 4);
        // A planning error isn't cached.
        let mut k = key("anon", 3);
        k.0.branch = branch;
        let err = cache.get_or_plan(k.clone(), || {
            Err(crate::sql::PlanError::UnknownTable("t".into()))
        });
        assert!(err.is_err());
        assert!(cache.get_or_plan(k, || Ok(vec![])).is_ok());
    }

    fn plan_of(bytes: usize) -> Vec<Plan> {
        vec![Plan {
            sql: "x".repeat(bytes),
            binds: vec![],
            capped_sql: String::new(),
        }]
    }

    fn put(cache: &ShapeCache, branch: Uuid, hash: u64, bytes: usize) {
        let mut k = key("anon", hash);
        k.0.branch = branch;
        cache.get_or_plan(k, || Ok(plan_of(bytes))).unwrap();
    }

    /// A branch never holds more than its byte bound, and still serves the latest shapes.
    #[test]
    fn shape_cache_stays_under_branch_bytes() {
        let bound = 100_000;
        let cache = ShapeCache::with_bounds(bound, u64::MAX / 2);
        let branch = Uuid::new_v4();
        for hash in 0..200 {
            put(&cache, branch, hash, 5_000);
            if hash % 7 == 0 {
                cache.run_pending_tasks();
                assert!(cache.held().0 <= bound, "{}", cache.held().0);
            }
        }
        cache.run_pending_tasks();
        let (bytes, sets) = cache.held();
        assert!(bytes <= bound && sets > 5, "{bytes} bytes, {sets} sets");
        let made = std::cell::Cell::new(0);
        let mut k = key("anon", 199);
        k.0.branch = branch;
        cache
            .get_or_plan(k, || {
                made.set(made.get() + 1);
                Ok(plan_of(5_000))
            })
            .unwrap();
        assert_eq!(made.get(), 0, "the latest shape is still cached");
    }

    /// Branches together stay under the global bound: whole branches are evicted.
    #[test]
    fn shape_cache_global_bytes_evict_branches() {
        let total = 200_000;
        let cache = ShapeCache::with_bounds(100_000, total);
        let branches: Vec<Uuid> = (0..30).map(|_| Uuid::new_v4()).collect();
        for round in 0..10 {
            for b in &branches {
                put(&cache, *b, round, 5_000);
            }
        }
        cache.run_pending_tasks();
        let (bytes, _) = cache.held();
        assert!(bytes <= total, "{bytes} bytes held, bound {total}");
        assert!(bytes > 0);
        let alive = branches
            .iter()
            .filter(|b| cache.branch_bytes(**b).is_some())
            .count();
        assert!(alive < branches.len(), "some branches were evicted");
    }

    /// A plan set over [`MAX_CACHED_PLAN_BYTES`] is returned and never kept.
    #[test]
    fn oversized_plan_is_not_cached() {
        let cache = ShapeCache::default();
        let branch = Uuid::new_v4();
        let made = std::cell::Cell::new(0);
        for _ in 0..2 {
            let mut k = key("anon", 1);
            k.0.branch = branch;
            let plans = cache
                .get_or_plan(k, || {
                    made.set(made.get() + 1);
                    Ok(plan_of(MAX_CACHED_PLAN_BYTES + 1))
                })
                .unwrap();
            assert_eq!(plans.len(), 1);
        }
        assert_eq!(made.get(), 2, "planned again: nothing was kept");
        cache.run_pending_tasks();
        assert_eq!(cache.held(), (0, 0));
        assert_eq!(cache.branch_bytes(branch), None);
        // One under the limit, key included, is kept.
        put(&cache, branch, 2, 1_000);
        cache.run_pending_tasks();
        assert_eq!(cache.held().1, 1);
    }

    /// Recording a database's new schema version drops its old versions, and no other
    /// database's.
    #[tokio::test]
    async fn new_schema_version_evicts_the_old_one() {
        let cache = QueryCache::default();
        let branch = Uuid::new_v4();
        let load = |database: &'static str, version: &'static str| {
            let cache = &cache;
            async move {
                cache
                    .cached_schema(schema_key(branch, database, version), "rid", async {
                        Ok(Schema::build(version.into(), Raw::default()))
                    })
                    .await
                    .unwrap()
            }
        };
        load("main", "v1").await;
        load("shop", "v1").await;
        cache.versions.insert((branch, "main".into()), "v1".into());
        // What load_schema does on reading v2 where v1 was held.
        cache.forget_other_versions(branch, "main", "v2");
        load("main", "v2").await;
        cache.run_pending_tasks().await;
        assert!(cache.cached(&schema_key(branch, "main", "v1")).await.is_none());
        assert!(cache.cached(&schema_key(branch, "main", "v2")).await.is_some());
        assert!(cache.cached(&schema_key(branch, "shop", "v1")).await.is_some());
        assert_eq!(cache.schemas_held().1, 2);
        assert!(cache.schemas_held().0 > 0, "schemas are weighed");
    }

    /// Two databases of one branch at the same version never share a plan or a schema, and
    /// invalidating the branch drops both (and nothing of another branch).
    #[tokio::test]
    async fn shape_cache_keys_apart_databases() {
        let cache = QueryCache::default();
        let branch = Uuid::new_v4();
        let other = Uuid::new_v4();
        let made = std::cell::Cell::new(0);
        let plan_in = |branch: Uuid, database: &str| {
            cache
                .shapes
                .get_or_plan(
                    (
                        schema_key(branch, database, "v1"),
                        "anon".into(),
                        "public".into(),
                        1,
                    ),
                    || {
                        made.set(made.get() + 1);
                        Ok(vec![])
                    },
                )
                .unwrap()
        };
        plan_in(branch, "main");
        plan_in(branch, "main");
        assert_eq!(made.get(), 1);
        plan_in(branch, "shop");
        plan_in(branch, "shop");
        assert_eq!(made.get(), 2);
        plan_in(other, "main");
        assert_eq!(made.get(), 3);

        let built = std::sync::atomic::AtomicUsize::new(0);
        let schema_in = |branch: Uuid, database: &'static str| {
            let built = &built;
            let cache = &cache;
            async move {
                cache
                    .cached_schema(schema_key(branch, database, "v1"), "rid", async {
                        built.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        Ok(Schema::build("v1".into(), Raw::default()))
                    })
                    .await
                    .unwrap()
            }
        };
        schema_in(branch, "main").await;
        schema_in(branch, "main").await;
        schema_in(branch, "shop").await;
        schema_in(other, "main").await;
        assert_eq!(built.load(std::sync::atomic::Ordering::Relaxed), 3);

        cache.invalidate_branch(branch);
        plan_in(branch, "main");
        plan_in(branch, "shop");
        plan_in(other, "main");
        assert_eq!(
            made.get(),
            5,
            "the branch's plans are gone, the other's kept"
        );
        schema_in(branch, "main").await;
        schema_in(branch, "shop").await;
        schema_in(other, "main").await;
        assert_eq!(built.load(std::sync::atomic::Ordering::Relaxed), 5);
    }
}
