//! One request, one transaction:
//!
//! 1. `BEGIN`;
//! 2. one round trip sets the role, the claims, the statement timeout and the transaction
//!    timeout (the same value: it bounds the whole request) and reads
//!    `cheladb.schema_version` ([`PREAMBLE_SQL`]);
//! 3. a schema version not seen before is introspected in the same transaction, and the
//!    role's view of it computed once;
//! 4. the tree is planned through the shape cache, keyed by
//!    `(branch, schema version, role, default_schema, shape hash)`;
//! 5. each plan runs with its values bound (`max_rows` applied at binding), its result
//!    counted against the response cap ([`ResponseBudget`]);
//! 6. `COMMIT`.
//!
//! Every plan returns one `text` row: the operation's JSON, sent on unparsed. A batch joins
//! its results into one array.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use cheladb_auth::internal::DataIdentity;
use cheladb_types::data_key::DataKeyKind;
use serde_json::Value;
use sqlx::{Acquire, PgConnection};
use uuid::Uuid;

use crate::conn::{AcquireError, ConnError, ConnInfo, ConnSource, Pools};
use crate::error::{ApiError, SCHEMA_MISSING};
use crate::query::{Op, parse};
use crate::schema::{self, IntrospectError, RoleView, Schema};
use crate::shape::shape_hash;
use crate::sql::{BindValue, Plan, plan, resolve_bind};

/// Schema versions kept (one per branch in the steady state).
pub const SCHEMA_CACHE: u64 = 1_000;
/// Shapes kept per branch.
pub const SHAPES_PER_BRANCH: u32 = 1_000;
/// Shapes kept across every branch.
pub const SHAPES_TOTAL: u64 = 50_000;
/// The largest query response, unless `CHELADB_REST_MAX_RESPONSE_BYTES` says otherwise.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

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
    pub fn take(&mut self, n: usize) -> Result<(), ApiError> {
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
        Err(ApiError::new(
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            format!("the response is over {cap}; ask for fewer rows (limit) or fewer columns"),
        ))
    }
}

/// `$1` role, `$2` claims, `$3` statement timeout in ms, also the transaction timeout
/// (Postgres 17: set inside a transaction, it starts counting then and ends with it).
/// `set_config` returns its value; the last column is the schema version.
pub const PREAMBLE_SQL: &str = "SELECT set_config('role', $1, true), \
     set_config('request.jwt.claims', $2, true), set_config('statement_timeout', $3, true), \
     set_config('transaction_timeout', $3, true), \
     (SELECT version::text FROM cheladb.schema_version)";

/// Whether role `$1` is owner-level: any attribute that bypasses or administers, or
/// membership in `neon_superuser`. The one wide definition.
pub const OWNER_LEVEL_SQL: &str = "SELECT rolsuper OR rolbypassrls OR rolcreaterole OR rolcreatedb OR rolreplication \
    OR pg_has_role(oid, 'neon_superuser', 'MEMBER') FROM pg_catalog.pg_roles WHERE rolname = $1";

/// How long an owner-level answer is kept.
const OWNER_LEVEL_TTL: std::time::Duration = std::time::Duration::from_secs(30);

fn owner_level_cache() -> moka::future::Cache<(Uuid, String), bool> {
    moka::future::Cache::builder()
        .max_capacity(SCHEMA_CACHE * 10)
        .time_to_live(OWNER_LEVEL_TTL)
        .build()
}

/// For a secret key only: refuses a role that is owner-level (cached per branch and role).
/// `fetch` asks Postgres on a miss.
async fn ensure_not_owner_level<F, Fut>(
    cache: &moka::future::Cache<(Uuid, String), bool>,
    data: &DataIdentity,
    fetch: F,
) -> Result<(), ApiError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<bool, ApiError>>,
{
    if data.kind != DataKeyKind::Secret {
        return Ok(());
    }
    let key = (data.branch_id, data.role.clone());
    let owner = match cache.get(&key).await {
        Some(v) => v,
        None => {
            let v = fetch().await?;
            cache.insert(key, v).await;
            v
        }
    };
    if owner {
        Err(ApiError::role_not_allowed(&data.role))
    } else {
        Ok(())
    }
}

/// Why a request failed before or while it ran.
#[derive(Debug)]
pub enum Failure {
    /// No connection info for the branch: the caller decides what that means.
    Conn(ConnError),
    Api(ApiError),
}

impl From<ApiError> for Failure {
    fn from(e: ApiError) -> Self {
        Failure::Api(e)
    }
}

type ShapeKey = (String, String, String, u64);

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

/// Plans by shape: at most [`SHAPES_PER_BRANCH`] per branch (LRU-like), and about
/// [`SHAPES_TOTAL`] in all. The global bound weighs each branch by the shapes it has held
/// (capped at the per-branch bound), refreshed every 50 inserts.
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
        branch: Uuid,
        key: ShapeKey,
        make: impl FnOnce() -> Result<Vec<Plan>, crate::sql::PlanError>,
    ) -> Result<Arc<Vec<Plan>>, crate::sql::PlanError> {
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
}

pub struct Engine<S> {
    pools: Pools<S>,
    max_response_bytes: usize,
    schemas: moka::future::Cache<(Uuid, String), Arc<Schema>>,
    shapes: ShapeCache,
    owner_level: moka::future::Cache<(Uuid, String), bool>,
}

fn introspect_error(e: IntrospectError, request_id: &str) -> ApiError {
    match e {
        IntrospectError::Db(e) => ApiError::from_sqlx(e, request_id),
        IntrospectError::Decode(e) => ApiError::internal(request_id, e),
    }
}

impl<S: ConnSource> Engine<S> {
    /// `max_connections` caps connections across every branch. Responses are capped at
    /// [`DEFAULT_MAX_RESPONSE_BYTES`] ([`Self::with_max_response_bytes`] changes it).
    pub fn new(source: S, max_connections: usize) -> Self {
        Engine {
            pools: Pools::new(source, max_connections),
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            schemas: moka::future::Cache::new(SCHEMA_CACHE),
            shapes: ShapeCache::default(),
            owner_level: owner_level_cache(),
        }
    }

    /// Caps a query's response at `bytes`.
    pub fn with_max_response_bytes(mut self, bytes: usize) -> Self {
        self.max_response_bytes = bytes;
        self
    }

    async fn acquire(&self, branch: Uuid, request_id: &str) -> Result<crate::conn::Conn, Failure> {
        self.pools
            .acquire(branch)
            .await
            .map_err(|e| acquire_failure(e, branch, request_id))
    }

    /// Sets the role (`none` = stay `authenticator`), the claims and the timeout, and reads
    /// the schema version.
    async fn preamble(
        conn: &mut PgConnection,
        info: &ConnInfo,
        role: &str,
        claims: &str,
        request_id: &str,
    ) -> Result<String, ApiError> {
        let row: Result<(String, String, String, String, Option<String>), _> =
            sqlx::query_as(PREAMBLE_SQL)
                .bind(role)
                .bind(claims)
                .bind(info.settings.statement_timeout_ms.to_string())
                .fetch_one(conn)
                .await;
        match row {
            Ok((_, _, _, _, Some(version))) => Ok(version),
            Ok((_, _, _, _, None)) => Err(ApiError::unavailable(SCHEMA_MISSING)),
            Err(sqlx::Error::Database(db)) => Err(preamble_failure(db.as_ref(), role, request_id)),
            Err(e) => Err(ApiError::from_sqlx(e, request_id)),
        }
    }

    /// The owner-level check, in the request's transaction; a missing role counts as plain
    /// (the preamble reports it).
    async fn check_owner_level(
        &self,
        conn: &mut PgConnection,
        data: &DataIdentity,
        request_id: &str,
    ) -> Result<(), ApiError> {
        ensure_not_owner_level(&self.owner_level, data, || async {
            let row: Option<(Option<bool>,)> = sqlx::query_as(OWNER_LEVEL_SQL)
                .bind(&data.role)
                .fetch_optional(conn)
                .await
                .map_err(|e| ApiError::from_sqlx(e, request_id))?;
            Ok(row.and_then(|r| r.0).unwrap_or(false))
        })
        .await
    }

    async fn schema(
        &self,
        conn: &mut PgConnection,
        branch: Uuid,
        version: String,
        request_id: &str,
    ) -> Result<Arc<Schema>, ApiError> {
        self.schemas
            .try_get_with((branch, version.clone()), async {
                schema::introspect(conn, version).await.map(Arc::new)
            })
            .await
            .map_err(|e| match Arc::try_unwrap(e) {
                Ok(e) => introspect_error(e, request_id),
                Err(e) => ApiError::internal(request_id, e),
            })
    }

    async fn view(
        conn: &mut PgConnection,
        schema: &Schema,
        role: &str,
        request_id: &str,
    ) -> Result<Arc<RoleView>, ApiError> {
        schema::role_view(conn, schema, role)
            .await
            .map_err(|e| introspect_error(e, request_id))
    }

    /// `POST /data/v1/query`: the JSON result, unparsed.
    pub async fn query(
        &self,
        data: &DataIdentity,
        request_id: &str,
        body: &[u8],
    ) -> Result<String, Failure> {
        let op = parse(body).map_err(ApiError::from)?;
        let branch = data.branch_id;
        let mut conn = self.acquire(branch, request_id).await?;
        let info = conn.info.clone();
        let mut tx = conn
            .conn
            .begin()
            .await
            .map_err(|e| ApiError::from_sqlx(e, request_id))?;
        let claims = data.claims.to_string();
        self.check_owner_level(&mut tx, data, request_id).await?;
        let version = Self::preamble(&mut tx, &info, &data.role, &claims, request_id).await?;
        let schema = self
            .schema(&mut tx, branch, version.clone(), request_id)
            .await?;
        let view = Self::view(&mut tx, &schema, &data.role, request_id).await?;
        let default_schema = &info.settings.default_schema;
        let key = (
            version,
            data.role.clone(),
            default_schema.clone(),
            shape_hash(&op),
        );
        let plans = self
            .shapes
            .get_or_plan(branch, key, || plan(&op, view.as_ref(), default_schema))
            .map_err(ApiError::from)?;
        let mut results = Vec::with_capacity(plans.len());
        // Each result, and a separator or bracket next to it.
        let mut budget = ResponseBudget::new(self.max_response_bytes);
        for p in plans.iter() {
            let result = run(&mut tx, &op, p, info.settings.max_rows, request_id).await?;
            budget.take(result.len() + 1)?;
            results.push(result);
        }
        tx.commit()
            .await
            .map_err(|e| ApiError::from_sqlx(e, request_id))?;
        Ok(match op {
            Op::Batch(_) => format!("[{}]", results.join(",")),
            _ => results.pop().unwrap_or_else(|| "null".into()),
        })
    }

    /// `GET /data/v1/schema`: what the key's role can reach, without privileges.
    pub async fn role_schema(
        &self,
        data: &DataIdentity,
        request_id: &str,
    ) -> Result<Value, Failure> {
        let (schema, views, info_default) = self
            .schemas_for(
                data.branch_id,
                &data.role,
                &[&data.role],
                Some(data),
                request_id,
            )
            .await?;
        let roles: Vec<(&str, &schema::Visibility)> =
            vec![(data.role.as_str(), &views[0].visibility)];
        Ok(schema.to_json(&info_default, &roles, false))
    }

    /// The internal `GET /internal/schema/{branch}`: everything `anon` or `authenticated`
    /// can reach, with what each holds.
    pub async fn branch_schema(&self, branch: Uuid, request_id: &str) -> Result<Value, Failure> {
        let (schema, views, info_default) = self
            .schemas_for(
                branch,
                "none",
                &["anon", "authenticated"],
                None,
                request_id,
            )
            .await?;
        let roles: Vec<(&str, &schema::Visibility)> = vec![
            ("anon", &views[0].visibility),
            ("authenticated", &views[1].visibility),
        ];
        Ok(schema.to_json(&info_default, &roles, true))
    }

    async fn schemas_for(
        &self,
        branch: Uuid,
        role: &str,
        roles: &[&str],
        guard: Option<&DataIdentity>,
        request_id: &str,
    ) -> Result<(Arc<Schema>, Vec<Arc<RoleView>>, String), Failure> {
        let mut conn = self.acquire(branch, request_id).await?;
        let info = conn.info.clone();
        let mut tx = conn
            .conn
            .begin()
            .await
            .map_err(|e| ApiError::from_sqlx(e, request_id))?;
        if let Some(data) = guard {
            self.check_owner_level(&mut tx, data, request_id).await?;
        }
        let version = Self::preamble(&mut tx, &info, role, "{}", request_id).await?;
        let schema = self.schema(&mut tx, branch, version, request_id).await?;
        let mut views = Vec::new();
        for r in roles {
            views.push(Self::view(&mut tx, &schema, r, request_id).await?);
        }
        tx.commit()
            .await
            .map_err(|e| ApiError::from_sqlx(e, request_id))?;
        Ok((schema, views, info.settings.default_schema.clone()))
    }
}

/// Why no connection could be had, as the client sees it (the cause is logged).
fn acquire_failure(e: AcquireError, branch: Uuid, request_id: &str) -> Failure {
    match e {
        AcquireError::Conn(e) => Failure::Conn(e),
        AcquireError::TimedOut => {
            ApiError::unavailable("no connection to the branch's database in time; try again")
                .into()
        }
        AcquireError::Busy => {
            ApiError::unavailable("too many connections in use; try again").into()
        }
        AcquireError::Db(e) => {
            tracing::warn!(request_id, branch_id = %branch, "connecting: {e}");
            ApiError::unavailable(crate::error::UNREACHABLE).into()
        }
    }
}

/// The preamble failed in Postgres: the machinery schema is missing, or the key's role
/// can't be used. Postgres's text goes to the log only.
fn preamble_failure(db: &dyn sqlx::error::DatabaseError, role: &str, request_id: &str) -> ApiError {
    let code = db.code().unwrap_or_default();
    let message = db.message();
    if matches!(code.as_ref(), "42P01" | "3F000")
        || message.contains("cheladb")
        || message.contains("schema_version")
    {
        return ApiError::unavailable(SCHEMA_MISSING);
    }
    if crate::error::classify(&code).is_some_and(|(status, _)| status == 503) {
        return ApiError::from_database(db, request_id);
    }
    // The key's role is gone or not granted to authenticator (a restore can rewind the
    // grant until the bootstrap runs again).
    tracing::warn!(request_id, role, "the preamble failed: {code}: {message}");
    ApiError::unavailable(format!(
        "the key's role {role} can't be used right now; try again"
    ))
}

/// Runs one plan with `op`'s values: its one `text` row, or `null` for a NULL (a call to a
/// `void` function).
async fn run(
    conn: &mut PgConnection,
    op: &Op,
    p: &Plan,
    max_rows: i64,
    request_id: &str,
) -> Result<String, ApiError> {
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
                return Err(ApiError::internal(
                    request_id,
                    format!("a cached plan doesn't fit the tree: {bind:?}"),
                ));
            }
        };
    }
    let out = q
        .fetch_one(conn)
        .await
        .map_err(|e| ApiError::from_sqlx(e, request_id))?;
    Ok(out.unwrap_or_else(|| "null".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(role: &str, hash: u64) -> ShapeKey {
        ("v1".into(), role.into(), "public".into(), hash)
    }

    /// Failing to connect, or a role Postgres refuses, answers a fixed message.
    #[test]
    fn connection_and_role_failures_are_fixed_messages() {
        const SECRET: &str = "10.1.2.3:5432: permission denied to set role \"x\"";
        let io = sqlx::Error::Io(std::io::Error::other(SECRET));
        let Failure::Api(e) = acquire_failure(AcquireError::Db(io), Uuid::nil(), "rid-1") else {
            panic!("an API error");
        };
        assert_eq!(e.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert!(!e.message.contains("10.1.2.3"), "{}", e.message);
        let db = crate::error::tests::FakeDb {
            code: "42501",
            message: SECRET,
        };
        let e = preamble_failure(&db, "x", "rid-1");
        assert_eq!(e.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            !e.message.contains("10.1.2.3") && e.message.contains("x"),
            "{}",
            e.message
        );
        let missing = crate::error::tests::FakeDb {
            code: "42P01",
            message: "relation \"cheladb.schema_version\" does not exist",
        };
        assert_eq!(
            preamble_failure(&missing, "x", "rid-1").message,
            SCHEMA_MISSING
        );
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

    #[test]
    fn the_response_budget_refuses_past_the_cap() {
        assert_eq!(DEFAULT_MAX_RESPONSE_BYTES, 16 * 1024 * 1024);
        let mut budget = ResponseBudget::new(10);
        budget.take(4).unwrap();
        budget.take(6).unwrap();
        let e = budget.take(1).unwrap_err();
        assert_eq!(e.status, axum::http::StatusCode::PAYLOAD_TOO_LARGE);
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
            cache
                .get_or_plan(branch, key(role, hash), || {
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
        let err = cache.get_or_plan(branch, key("anon", 3), || {
            Err(crate::sql::PlanError::UnknownTable("t".into()))
        });
        assert!(err.is_err());
        assert!(
            cache
                .get_or_plan(branch, key("anon", 3), || Ok(vec![]))
                .is_ok()
        );
    }
    #[test]
    fn owner_level_sql_is_pinned() {
        for needle in [
            "rolsuper",
            "rolbypassrls",
            "rolcreaterole",
            "rolcreatedb",
            "rolreplication",
            "pg_has_role(oid, 'neon_superuser', 'MEMBER')",
            "FROM pg_catalog.pg_roles WHERE rolname = $1",
        ] {
            assert!(OWNER_LEVEL_SQL.contains(needle), "{needle}: {OWNER_LEVEL_SQL}");
        }
    }

    fn identity(kind: DataKeyKind, branch: Uuid, role: &str) -> DataIdentity {
        DataIdentity {
            branch_id: branch,
            key_id: Uuid::nil(),
            kind,
            role: role.into(),
            claims: serde_json::json!({}),
        }
    }

    fn owner_cache() -> moka::future::Cache<(Uuid, String), bool> {
        owner_level_cache()
    }

    #[tokio::test]
    async fn owner_level_check_is_cached_per_branch_and_role() {
        let cache = owner_cache();
        let branch = Uuid::new_v4();
        let queries = std::cell::Cell::new(0);
        let check = |branch: Uuid, role: &'static str, answer: bool| {
            let data = identity(DataKeyKind::Secret, branch, role);
            let cache = &cache;
            let queries = &queries;
            async move {
                ensure_not_owner_level(cache, &data, || async {
                    queries.set(queries.get() + 1);
                    Ok(answer)
                })
                .await
            }
        };
        assert!(check(branch, "svc", false).await.is_ok());
        assert!(check(branch, "svc", false).await.is_ok());
        assert_eq!(queries.get(), 1);
        // Another role asks again; an owner-level answer is cached and refused.
        let e = check(branch, "boss", true).await.unwrap_err();
        assert_eq!((e.status, e.code), (axum::http::StatusCode::FORBIDDEN, "role_not_allowed"));
        assert_eq!(queries.get(), 2);
        assert!(check(branch, "boss", true).await.is_err());
        assert_eq!(queries.get(), 2);
        // Another branch asks again.
        assert!(check(Uuid::new_v4(), "svc", false).await.is_ok());
        assert_eq!(queries.get(), 3);
    }

    #[tokio::test]
    async fn publishable_keys_skip_the_check() {
        let cache = owner_cache();
        let data = identity(DataKeyKind::Public, Uuid::new_v4(), "anon");
        let r = ensure_not_owner_level(&cache, &data, || async {
            panic!("a publishable key must not be checked");
            #[allow(unreachable_code)]
            Ok(true)
        })
        .await;
        assert!(r.is_ok());
    }

}
