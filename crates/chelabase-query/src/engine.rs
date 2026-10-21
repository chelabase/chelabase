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
//!    counted against the response cap ([`ResponseBudget`]).
//!
//! Every statement declares its parameters' types (`text`, `jsonb`, `int8`, `text[]`): the
//! server never infers one. Every plan returns one `text` row: the operation's JSON, sent on
//! unparsed. A batch joins its results into one array.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use http::StatusCode;
use tokio_postgres::{Client, TransactionStatus};
use tokio_postgres::types::{ToSql, Type};
use uuid::Uuid;

use crate::error::{DbFields, QueryFailure, SCHEMA_MISSING, classify, schema_missing_in};
use crate::pipeline::{
    self, Attempt, Call, Ctx, Param, QueryClient, Step, join_in_order, sql_budget,
    take_trailing_reset,
};
use crate::query::Op;
use crate::schema::{self, IntrospectError, RoleView, Schema};
use crate::shape::shape_hash;
use crate::sql::{BindValue, ParamType, Plan, PlanError, plan, resolve_bind};

/// Schema versions kept (one per served database in the steady state).
pub const SCHEMA_CACHE: u64 = 1_000;
/// Shapes kept per branch.
pub const SHAPES_PER_BRANCH: u32 = 1_000;
/// Shapes kept across every branch.
pub const SHAPES_TOTAL: u64 = 50_000;
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

/// One branch's shapes, and how many were ever inserted (its weight in the global bound).
struct BranchShapes {
    plans: moka::sync::Cache<ShapeKey, Arc<Vec<Plan>>>,
    inserts: AtomicU32,
}

impl BranchShapes {
    fn weight(&self) -> u32 {
        self.inserts
            .load(Ordering::Relaxed)
            .clamp(1, SHAPES_PER_BRANCH)
    }
}

/// Plans by shape: at most [`SHAPES_PER_BRANCH`] per branch (LRU-like, every database of
/// the branch together), and about [`SHAPES_TOTAL`] in all. The global bound weighs each
/// branch by the shapes it has held (capped at the per-branch bound), refreshed every 50
/// inserts.
pub struct ShapeCache {
    branches: moka::sync::Cache<Uuid, Arc<BranchShapes>>,
}

impl Default for ShapeCache {
    fn default() -> Self {
        ShapeCache {
            branches: moka::sync::Cache::builder()
                .max_capacity(SHAPES_TOTAL)
                .weigher(|_, b: &Arc<BranchShapes>| b.weight())
                .build(),
        }
    }
}

impl ShapeCache {
    pub fn get_or_plan(
        &self,
        key: ShapeKey,
        make: impl FnOnce() -> Result<Vec<Plan>, PlanError>,
    ) -> Result<Arc<Vec<Plan>>, PlanError> {
        let branch = key.0.branch;
        let shapes = self.branches.get_with(branch, || {
            Arc::new(BranchShapes {
                plans: moka::sync::Cache::new(u64::from(SHAPES_PER_BRANCH)),
                inserts: AtomicU32::new(0),
            })
        });
        if let Some(plans) = shapes.plans.get(&key) {
            return Ok(plans);
        }
        let plans = Arc::new(make()?);
        shapes.plans.insert(key, plans.clone());
        let n = shapes.inserts.fetch_add(1, Ordering::Relaxed) + 1;
        if n.is_multiple_of(50) && n <= SHAPES_PER_BRANCH {
            // Re-weigh the branch.
            self.branches.insert(branch, shapes);
        }
        Ok(plans)
    }

    /// Drops every plan of `branch`.
    pub fn invalidate_branch(&self, branch: Uuid) {
        self.branches.invalidate(&branch);
    }
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
                .max_capacity(SCHEMA_CACHE)
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
pub async fn with_transaction<C: QueryClient + ?Sized, T>(
    client: &C,
    reset_sql: &str,
    request_id: &str,
    f: impl AsyncFnOnce(&Client) -> Result<T, QueryFailure>,
) -> Result<T, QueryFailure> {
    let c = client.client();
    let begin = begin_sql(reset_sql, client.transaction_status());
    if let Err(e) = c.batch_execute(&begin).await {
        tracing::warn!(
            request_id,
            "the transaction couldn't begin on a clean session; the connection is discarded: {e}"
        );
        client.mark_broken();
        return Err(QueryFailure::from_pg(&e, request_id));
    }
    let out = f(c).await;
    let end = if out.is_ok() { "COMMIT" } else { "ROLLBACK" };
    type Out = Result<(), tokio_postgres::Error>;
    let steps: Vec<Step<'_, Out>> = vec![
        Box::pin(c.batch_execute(end)),
        Box::pin(c.batch_execute(reset_sql)),
    ];
    let mut results = join_in_order(steps).await;
    take_trailing_reset(&mut results, client, request_id);
    let ended = results.pop().expect("the COMMIT or ROLLBACK step");
    match out {
        Ok(out) => {
            ended.map_err(|e| QueryFailure::from_pg(&e, request_id))?;
            Ok(out)
        }
        Err(failure) => {
            if let Err(e) = ended {
                tracing::debug!(request_id, "the rollback failed: {e}");
            }
            Err(failure)
        }
    }
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
    with_transaction(client, settings.reset_sql, request_id, async |c| {
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
    cache
        .versions
        .insert((key.0, key.1.to_owned()), schema_key.version.clone());
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
    with_transaction(client, settings.reset_sql, request_id, async |client| {
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
        with_transaction(client, settings.reset_sql, request_id, async |client| {
            run_sequential(client, cache, key, run_as, settings, budget, op, request_id).await
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
