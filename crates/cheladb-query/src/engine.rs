//! One request inside a transaction the caller began:
//!
//! 1. one round trip sets the role and the claims ([`RunAs::Role`]; the owner runs as
//!    itself), the statement timeout and the transaction timeout (the same value: it
//!    bounds the whole request) and reads `cheladb.schema_version` ([`PREAMBLE_SQL`],
//!    [`OWNER_PREAMBLE_SQL`]);
//! 2. a schema version not seen before is introspected in the same transaction, and the
//!    role's view of it computed once;
//! 3. the tree is planned through the shape cache, keyed by
//!    `(branch, database, schema version, role, default_schema, shape hash)`;
//! 4. each plan runs with its values bound (`max_rows` applied at binding), its result
//!    counted against the response cap ([`ResponseBudget`]).
//!
//! The caller commits. Every plan returns one `text` row: the operation's JSON, sent on
//! unparsed. A batch joins its results into one array.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use http::StatusCode;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::{QueryFailure, SCHEMA_MISSING, classify, schema_missing_in};
use crate::query::Op;
use crate::schema::{self, IntrospectError, RoleView, Schema};
use crate::shape::shape_hash;
use crate::sql::{BindValue, Plan, PlanError, plan, resolve_bind};

/// Schema versions kept (one per served database in the steady state).
pub const SCHEMA_CACHE: u64 = 1_000;
/// Shapes kept per branch.
pub const SHAPES_PER_BRANCH: u32 = 1_000;
/// Shapes kept across every branch.
pub const SHAPES_TOTAL: u64 = 50_000;
/// The largest query response, unless the caller says otherwise.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

/// `$1` role, `$2` claims, `$3` statement timeout in ms, also the transaction timeout
/// (Postgres 17: set inside a transaction, it starts counting then and ends with it).
/// `set_config` returns its value; the last column is the schema version.
pub const PREAMBLE_SQL: &str = "SELECT set_config('role', $1, true), \
     set_config('request.jwt.claims', $2, true), set_config('statement_timeout', $3, true), \
     set_config('transaction_timeout', $3, true), \
     (SELECT version::text FROM cheladb.schema_version)";

/// The owner's preamble: no role and no claims (the connection's own user runs the
/// request), `$1` the statement and transaction timeout in ms. It returns the user's name
/// (the role its view and plans are cached under) and the schema version.
pub const OWNER_PREAMBLE_SQL: &str = "SELECT set_config('statement_timeout', $1, true), \
     set_config('transaction_timeout', $1, true), current_user::text, \
     (SELECT version::text FROM cheladb.schema_version)";

/// Who a request runs as.
#[derive(Debug, Clone, Copy)]
pub enum RunAs<'a> {
    /// `SET ROLE role` (the connection's user must be a member) with these JWT claims.
    Role {
        role: &'a str,
        claims: &'a serde_json::Value,
    },
    /// The connection's own user, the database's owner (P5).
    Owner,
}

/// The limits a request runs under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSettings {
    pub max_rows: i64,
    pub statement_timeout_ms: i64,
    /// What an unqualified name means.
    pub default_schema: String,
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
        const MIB: usize = 1024 * 1024;
        let cap = if self.cap >= MIB && self.cap.is_multiple_of(MIB) {
            format!("{} MiB", self.cap / MIB)
        } else {
            format!("{} bytes", self.cap)
        };
        Err(QueryFailure::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            format!("the response is over {cap}; ask for fewer rows (limit) or fewer columns"),
        ))
    }
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

/// The schema cache ([`SchemaKey`] to [`Schema`], each with its roles' views) and the shape
/// cache. One per process; every database of every branch shares it.
pub struct QueryCache {
    schemas: moka::future::Cache<SchemaKey, Arc<Schema>>,
    pub shapes: ShapeCache,
}

impl Default for QueryCache {
    fn default() -> Self {
        QueryCache {
            schemas: moka::future::Cache::builder()
                .max_capacity(SCHEMA_CACHE)
                .support_invalidation_closures()
                .build(),
            shapes: ShapeCache::default(),
        }
    }
}

impl QueryCache {
    /// The schema at `key`, introspected through `conn` (inside the request's transaction)
    /// on a miss.
    pub async fn schema(
        &self,
        conn: &mut PgConnection,
        key: SchemaKey,
        request_id: &str,
    ) -> Result<Arc<Schema>, QueryFailure> {
        let version = key.version.clone();
        self.cached_schema(key, request_id, async {
            schema::introspect(conn, version).await
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

    /// Drops every schema and plan of `branch` (a database disabled, a setting changed).
    pub fn invalidate_branch(&self, branch: Uuid) {
        let _ = self
            .schemas
            .invalidate_entries_if(move |k, _| k.branch == branch);
        self.shapes.invalidate_branch(branch);
    }
}

pub fn introspect_failure(e: IntrospectError, request_id: &str) -> QueryFailure {
    match e {
        IntrospectError::Db(e) => QueryFailure::from_sqlx(e, request_id),
        IntrospectError::Decode(e) => QueryFailure::internal(request_id, e),
    }
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
    conn: &mut PgConnection,
    run_as: RunAs<'_>,
    statement_timeout_ms: i64,
    request_id: &str,
) -> Result<Preamble, QueryFailure> {
    let timeout = statement_timeout_ms.to_string();
    let (row, role) = match run_as {
        RunAs::Role { role, claims } => {
            let row =
                sqlx::query_as::<_, (String, String, String, String, Option<String>)>(PREAMBLE_SQL)
                    .bind(role)
                    .bind(claims.to_string())
                    .bind(&timeout)
                    .fetch_one(conn)
                    .await
                    .map(|r| (role.to_owned(), r.4));
            (row, Some(role))
        }
        RunAs::Owner => {
            let row =
                sqlx::query_as::<_, (String, String, String, Option<String>)>(OWNER_PREAMBLE_SQL)
                    .bind(&timeout)
                    .fetch_one(conn)
                    .await
                    .map(|r| (r.2, r.3));
            (row, None)
        }
    };
    match row {
        Ok((role, Some(version))) => Ok(Preamble { version, role }),
        Ok((_, None)) => Err(QueryFailure::unavailable(SCHEMA_MISSING)),
        Err(sqlx::Error::Database(db)) => Err(match role {
            Some(role) => preamble_failure(db.as_ref(), role, request_id),
            None if schema_missing(db.as_ref()) => QueryFailure::unavailable(SCHEMA_MISSING),
            None => QueryFailure::from_database(db.as_ref(), request_id),
        }),
        Err(e) => Err(QueryFailure::from_sqlx(e, request_id)),
    }
}

/// Whether a preamble error says the machinery schema (or its version table) is missing:
/// Postgres's "doesn't exist" for that table (42P01) or that schema (3F000), nothing else.
fn schema_missing(db: &dyn sqlx::error::DatabaseError) -> bool {
    let code = db.code().unwrap_or_default();
    let message = db.message();
    match code.as_ref() {
        "42P01" => message.contains("schema_version"),
        "3F000" => message.contains("\"cheladb\""),
        _ => false,
    }
}

/// `f` with the missing-machinery text naming `database`; any other failure as it is.
fn in_database(f: QueryFailure, database: &str) -> QueryFailure {
    if f.status == StatusCode::SERVICE_UNAVAILABLE && f.message == SCHEMA_MISSING {
        QueryFailure::unavailable(schema_missing_in(database))
    } else {
        f
    }
}

/// The preamble failed in Postgres: the machinery schema is missing, or the key's role
/// can't be used. Postgres's text goes to the log only.
pub fn preamble_failure(
    db: &dyn sqlx::error::DatabaseError,
    role: &str,
    request_id: &str,
) -> QueryFailure {
    if schema_missing(db) {
        return QueryFailure::unavailable(SCHEMA_MISSING);
    }
    let code = db.code().unwrap_or_default();
    if classify(&code).is_some_and(|(status, _)| status == 503) {
        return QueryFailure::from_database(db, request_id);
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
    conn: &mut PgConnection,
    schema: &Schema,
    role: &str,
    request_id: &str,
) -> Result<Arc<RoleView>, QueryFailure> {
    schema::role_view(conn, schema, role)
        .await
        .map_err(|e| introspect_failure(e, request_id))
}

/// Runs the preamble, then reads the schema of database `key = (branch, database)` at the
/// version it read (from the cache when it holds it), inside the caller's transaction. The
/// schema's key and the role the request runs as come back with it.
pub async fn load_schema(
    conn: &mut PgConnection,
    cache: &QueryCache,
    key: (Uuid, &str),
    run_as: RunAs<'_>,
    statement_timeout_ms: i64,
    request_id: &str,
) -> Result<(Arc<Schema>, SchemaKey, String), QueryFailure> {
    let pre = preamble(conn, run_as, statement_timeout_ms, request_id)
        .await
        .map_err(|f| in_database(f, key.1))?;
    let schema_key = SchemaKey {
        branch: key.0,
        database: key.1.to_owned(),
        version: pre.version,
    };
    let schema = cache.schema(conn, schema_key.clone(), request_id).await?;
    Ok((schema, schema_key, pre.role))
}

/// Runs the preamble, then reads the schema and `run_as`'s view of it (from the cache when
/// it holds them), inside the caller's transaction. `key` is `(branch, database)`.
pub async fn schema_for(
    conn: &mut PgConnection,
    cache: &QueryCache,
    key: (Uuid, &str),
    run_as: RunAs<'_>,
    settings: &RunSettings,
    request_id: &str,
) -> Result<(Arc<Schema>, Arc<RoleView>), QueryFailure> {
    let (schema, _, role) = load_schema(
        conn,
        cache,
        key,
        run_as,
        settings.statement_timeout_ms,
        request_id,
    )
    .await?;
    let view = view(conn, &schema, &role, request_id).await?;
    Ok((schema, view))
}

/// Runs `op` as `run_as` in database `key = (branch, database)`, inside the transaction the
/// caller began (and commits): the JSON result, unparsed. `budget` caps the response's
/// bytes.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    conn: &mut PgConnection,
    cache: &QueryCache,
    key: (Uuid, &str),
    run_as: RunAs<'_>,
    settings: &RunSettings,
    budget: usize,
    op: &Op,
    request_id: &str,
) -> Result<String, QueryFailure> {
    let (schema, schema_key, role) = load_schema(
        conn,
        cache,
        key,
        run_as,
        settings.statement_timeout_ms,
        request_id,
    )
    .await?;
    let view = view(conn, &schema, &role, request_id).await?;
    let default_schema = &settings.default_schema;
    let shape = (schema_key, role, default_schema.clone(), shape_hash(op));
    let plans = cache
        .shapes
        .get_or_plan(shape, || plan(op, view.as_ref(), default_schema))?;
    let mut results = Vec::with_capacity(plans.len());
    // Each result, and a separator or bracket next to it.
    let mut budget = ResponseBudget::new(budget);
    for p in plans.iter() {
        let result = run_plan(conn, op, p, settings.max_rows, request_id).await?;
        budget.take(result.len() + 1)?;
        results.push(result);
    }
    Ok(match op {
        Op::Batch(_) => format!("[{}]", results.join(",")),
        _ => results.pop().unwrap_or_else(|| "null".into()),
    })
}

/// Runs one plan with `op`'s values: its one `text` row, or `null` for a NULL (a call to a
/// `void` function).
async fn run_plan(
    conn: &mut PgConnection,
    op: &Op,
    p: &Plan,
    max_rows: i64,
    request_id: &str,
) -> Result<String, QueryFailure> {
    let mut q = sqlx::query_scalar::<_, Option<String>>(
        // The planner's text: names from the catalog, quoted; every value a parameter.
        sqlx::AssertSqlSafe(p.sql.as_str()),
    );
    for bind in &p.binds {
        q = match resolve_bind(op, bind, max_rows) {
            Some(BindValue::Text(v)) => q.bind(v.map(|v| v.into_owned())),
            Some(BindValue::Json(v)) => q.bind(sqlx::types::Json(v)),
            Some(BindValue::Int(v)) => q.bind(v),
            None => {
                return Err(QueryFailure::internal(
                    request_id,
                    format!("a cached plan doesn't fit the tree: {bind:?}"),
                ));
            }
        };
    }
    let out = q
        .fetch_one(conn)
        .await
        .map_err(|e| QueryFailure::from_sqlx(e, request_id))?;
    Ok(out.unwrap_or_else(|| "null".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::SCHEMA_MISSING;
    use crate::error::tests::FakeDb;
    use crate::schema::Raw;

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
        let db = FakeDb {
            code: "42501",
            message: SECRET,
        };
        let e = preamble_failure(&db, "x", "rid-1");
        assert_eq!(e.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            !e.message.contains("10.1.2.3") && e.message.contains("x"),
            "{}",
            e.message
        );
        let missing = FakeDb {
            code: "42P01",
            message: "relation \"cheladb.schema_version\" does not exist",
        };
        assert_eq!(
            preamble_failure(&missing, "x", "rid-1").message,
            SCHEMA_MISSING
        );
    }

    /// Only Postgres saying the version table (or the `cheladb` schema) doesn't exist means
    /// the machinery is missing; any other error naming them (a refused privilege, a raise)
    /// doesn't.
    #[test]
    fn schema_missing_needs_the_sqlstate_and_the_name() {
        for (code, message, missing) in [
            ("42P01", "relation \"cheladb.schema_version\" does not exist", true),
            ("3F000", "schema \"cheladb\" does not exist", true),
            ("42P01", "relation \"notes\" does not exist", false),
            ("3F000", "schema \"app\" does not exist", false),
            ("42501", "permission denied for schema cheladb", false),
            ("42501", "permission denied for table schema_version", false),
            ("P0001", "cheladb schema_version check failed", false),
        ] {
            assert_eq!(schema_missing(&FakeDb { code, message }), missing, "{code} {message}");
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
        let other = QueryFailure::unavailable("the key's role x can't be used right now; try again");
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

    /// The owner runs as itself: no `SET ROLE`, no claims, but the same timeouts and the
    /// same version read (P5).
    #[test]
    fn owner_preamble_sets_no_role() {
        assert_eq!(
            OWNER_PREAMBLE_SQL,
            "SELECT set_config('statement_timeout', $1, true), \
             set_config('transaction_timeout', $1, true), current_user::text, \
             (SELECT version::text FROM cheladb.schema_version)"
        );
        assert!(!OWNER_PREAMBLE_SQL.contains("'role'"));
        assert!(!OWNER_PREAMBLE_SQL.contains("request.jwt.claims"));
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
