//! The four ways to run one request, each over its own pool to the branch's compute, as
//! `authenticated` with fixed claims:
//!
//! - **A:** today's sqlx path (rest's pool options; `BEGIN`, preamble, plans, `COMMIT`);
//! - **B:** sqlx without the acquire ping and the release reset; one simple query resets the
//!   session, begins and sets role, claims and timeouts;
//! - **C:** tokio-postgres: every statement prepared first, then one pipeline from the reset
//!   to `COMMIT`, the version asserted and the response capped in SQL;
//! - **D:** the same pipeline without `COMMIT`; the version is checked in Rust, then `COMMIT`;
//! - **R:** the shipped path: `rest::exec::Engine::query` (everything rest does for a request
//!   but the HTTP layer), built on the internal source. Not in the default run.

use std::borrow::Cow;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;
use std::time::{Duration, Instant};

use anyhow::Context;
use chelabase_query::Op;
use chelabase_query::engine::{DEFAULT_MAX_RESPONSE_BYTES, PREAMBLE_SQL, ResponseBudget};
use chelabase_query::sql::{Bind, BindValue, Plan, resolve_bind};
use futures_util::future::{MaybeDone, maybe_done};
use chelabase_auth::internal::DataIdentity;
use chelabase_types::data_key::DataKeyKind;
use rest::conn::{ACQUIRE_TIMEOUT, CONNECTION_IDLE, ConnInfo, InternalSource, RESET_SQL};
use rest::exec::Engine;
use sqlx::Executor;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions, PgSslMode};
use tokio_postgres::Row;
use tokio_postgres::types::{ToSql, Type};

use crate::literal::quote_literal;
use crate::workload::{BenchOp, ROLE};

/// Prepared statements sqlx keeps per connection: rest's value while it ran on sqlx.
const STATEMENT_CACHE: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
pub enum VariantKind {
    A,
    B,
    C,
    D,
    R,
}

impl VariantKind {
    /// The `application_name` of the variant's sessions (R's is rest's own).
    pub fn application_name(self) -> &'static str {
        match self {
            VariantKind::A => "bench-a",
            VariantKind::B => "bench-b",
            VariantKind::C => "bench-c",
            VariantKind::D => "bench-d",
            VariantKind::R => "chelabase-rest",
        }
    }
}

impl std::str::FromStr for VariantKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_uppercase().as_str() {
            "A" => Ok(VariantKind::A),
            "B" => Ok(VariantKind::B),
            "C" => Ok(VariantKind::C),
            "D" => Ok(VariantKind::D),
            "R" => Ok(VariantKind::R),
            other => Err(format!("unknown variant {other:?} (A, B, C, D or R)")),
        }
    }
}

/// Every Phase 1 variant, in order (R is only run when asked for).
pub const ALL_VARIANTS: [VariantKind; 4] = [
    VariantKind::A,
    VariantKind::B,
    VariantKind::C,
    VariantKind::D,
];

pub trait Exec: Send + Sync {
    fn kind(&self) -> VariantKind;
    fn exec(
        &self,
        op: &BenchOp,
    ) -> impl std::future::Future<Output = anyhow::Result<String>> + Send;
}

/// The claims every variant runs with.
pub fn fixed_claims() -> serde_json::Value {
    serde_json::json!({"sub": "00000000-0000-0000-0000-000000000001", "role": "authenticated"})
}

/// The SQLSTATE the stale-version check raises (`bench.assert_schema_version`, and D's check).
pub const STALE_VERSION: &str = "CB001";
/// The SQLSTATE `bench.cap_response` raises past the budget.
pub const TOO_LARGE: &str = "CB002";

/// D found a schema version other than the expected one (and rolled back).
#[derive(Debug)]
pub struct StaleVersion {
    pub expected: uuid::Uuid,
    pub found: Option<String>,
}

impl std::fmt::Display for StaleVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{STALE_VERSION}: stale schema version (expected {}, found {:?})",
            self.expected, self.found
        )
    }
}

impl std::error::Error for StaleVersion {}

/// The SQLSTATE behind a variant's error, if any: Postgres's (through sqlx or
/// tokio-postgres), or [`STALE_VERSION`] for D's own check.
pub fn sqlstate(e: &anyhow::Error) -> Option<String> {
    e.chain().find_map(|cause| {
        if let Some(e) = cause.downcast_ref::<tokio_postgres::Error>() {
            return e.code().map(|c| c.code().to_owned());
        }
        if let Some(sqlx::Error::Database(db)) = cause.downcast_ref::<sqlx::Error>() {
            return db.code().map(Cow::into_owned);
        }
        cause
            .downcast_ref::<StaleVersion>()
            .map(|_| STALE_VERSION.to_owned())
    })
}

/// What every variant runs with.
#[derive(Debug, Clone)]
struct RunCtx {
    role: String,
    claims: String,
    timeout: String,
    max_rows: i64,
}

impl RunCtx {
    fn new(claims: &serde_json::Value, max_rows: i64, timeout_ms: i64) -> Self {
        RunCtx {
            role: ROLE.to_owned(),
            claims: claims.to_string(),
            timeout: timeout_ms.to_string(),
            max_rows,
        }
    }
}

/// Joins results as `chelabase_query::run` does: `[r1,r2,…]` for a batch, else the one.
fn join_results(op: &Op, mut results: Vec<String>) -> String {
    match op {
        Op::Batch(_) => format!("[{}]", results.join(",")),
        _ => results.pop().unwrap_or_else(|| "null".into()),
    }
}

/// Counts each result against the response cap, as the engine does.
fn check_budget(results: &[String]) -> anyhow::Result<()> {
    let mut budget = ResponseBudget::new(DEFAULT_MAX_RESPONSE_BYTES);
    for r in results {
        budget
            .take(r.len() + 1)
            .map_err(|f| anyhow::anyhow!("{}: {}", f.code, f.message))?;
    }
    Ok(())
}

fn plan_misfit(bind: &Bind) -> anyhow::Error {
    anyhow::anyhow!("a plan doesn't fit the tree: {bind:?}")
}

// ---------------------------------------------------------------------------------------
// A and B: sqlx

fn sqlx_connect_options(info: &ConnInfo, application_name: &str) -> PgConnectOptions {
    PgConnectOptions::new()
        .host(&info.host)
        .port(info.port)
        .username(&info.user)
        .password(&info.password)
        .database(&info.database)
        .ssl_mode(PgSslMode::Prefer)
        .application_name(application_name)
        .statement_cache_capacity(STATEMENT_CACHE)
}

/// A's pool options: rest's (`rest::conn`), with `pool_size` connections.
pub fn pool_a_options(pool_size: u32) -> PgPoolOptions {
    PgPoolOptions::new()
        .max_connections(pool_size.max(1))
        .min_connections(0)
        .idle_timeout(CONNECTION_IDLE)
        .acquire_timeout(ACQUIRE_TIMEOUT)
        .after_release(|conn, _| {
            Box::pin(async move {
                // An error closes the connection instead.
                conn.execute(sqlx::raw_sql(RESET_SQL)).await?;
                Ok(true)
            })
        })
}

/// B's pool options: no ping on acquire, no reset on release.
pub fn pool_b_options(pool_size: u32) -> PgPoolOptions {
    PgPoolOptions::new()
        .max_connections(pool_size.max(1))
        .min_connections(0)
        .idle_timeout(CONNECTION_IDLE)
        .acquire_timeout(ACQUIRE_TIMEOUT)
        .test_before_acquire(false)
}

/// B's begin: reset the session, begin, then role, claims and both timeouts, as one simple
/// query with every value a quoted literal.
pub fn b_begin_sql(role: &str, claims: &str, timeout_ms: i64) -> anyhow::Result<String> {
    let r = quote_literal(role)?;
    let c = quote_literal(claims)?;
    let t = quote_literal(&timeout_ms.to_string())?;
    Ok(format!(
        "SET ROLE NONE; RESET ALL; BEGIN; SELECT set_config('role', {r}, true), \
         set_config('request.jwt.claims', {c}, true), \
         set_config('statement_timeout', {t}, true), \
         set_config('transaction_timeout', {t}, true)"
    ))
}

/// Runs one plan as the engine's `run_plan` does: its one `text` row, `null` for NULL.
async fn sqlx_run_plan(
    conn: &mut sqlx::PgConnection,
    op: &Op,
    p: &Plan,
    max_rows: i64,
) -> anyhow::Result<String> {
    let mut q = sqlx::query_scalar::<_, Option<String>>(sqlx::AssertSqlSafe(p.sql.as_str()));
    for bind in &p.binds {
        q = match resolve_bind(op, bind, max_rows) {
            Some(BindValue::Text(v)) => q.bind(v.map(|v| v.into_owned())),
            Some(BindValue::TextArray(v)) => {
                q.bind(v.into_iter().map(str::to_owned).collect::<Vec<_>>())
            }
            Some(BindValue::Json(v)) => q.bind(sqlx::types::Json(v)),
            Some(BindValue::Int(v)) => q.bind(v),
            None => return Err(plan_misfit(bind)),
        };
    }
    Ok(q.fetch_one(conn).await?.unwrap_or_else(|| "null".into()))
}

async fn sqlx_run_plans(
    conn: &mut sqlx::PgConnection,
    op: &BenchOp,
    max_rows: i64,
) -> anyhow::Result<String> {
    let mut results = Vec::with_capacity(op.plans.len());
    for p in op.plans.iter() {
        results.push(sqlx_run_plan(&mut *conn, &op.op, p, max_rows).await?);
    }
    check_budget(&results)?;
    Ok(join_results(&op.op, results))
}

/// Today's path.
pub struct VariantA {
    pub pool: PgPool,
    ctx: RunCtx,
}

impl VariantA {
    pub fn connect(
        info: &ConnInfo,
        pool_size: u32,
        claims: &serde_json::Value,
        max_rows: i64,
        timeout_ms: i64,
    ) -> VariantA {
        VariantA {
            pool: pool_a_options(pool_size)
                .connect_lazy_with(sqlx_connect_options(info, "bench-a")),
            ctx: RunCtx::new(claims, max_rows, timeout_ms),
        }
    }

    async fn run(&self, op: &BenchOp) -> anyhow::Result<String> {
        let ctx = &self.ctx;
        let mut tx = self.pool.begin().await?;
        let (_, _, _, _, version) =
            sqlx::query_as::<_, (String, String, String, String, Option<String>)>(PREAMBLE_SQL)
                .bind(&ctx.role)
                .bind(&ctx.claims)
                .bind(&ctx.timeout)
                // No role OID: the name-only check, as for the public key.
                .bind(None::<i64>)
                .fetch_one(&mut *tx)
                .await?;
        anyhow::ensure!(version.is_some(), "chelabase.schema_version is empty");
        let out = sqlx_run_plans(&mut tx, op, ctx.max_rows).await?;
        tx.commit().await?;
        Ok(out)
    }
}

impl Exec for VariantA {
    fn kind(&self) -> VariantKind {
        VariantKind::A
    }

    fn exec(&self, op: &BenchOp) -> impl Future<Output = anyhow::Result<String>> + Send {
        self.run(op)
    }
}

/// Every cheap sqlx tweak.
pub struct VariantB {
    pub pool: PgPool,
    /// Shared, so each request's `begin_with` costs a reference count, not a copy.
    begin_sql: std::sync::Arc<str>,
    max_rows: i64,
}

impl VariantB {
    pub fn connect(
        info: &ConnInfo,
        pool_size: u32,
        claims: &serde_json::Value,
        max_rows: i64,
        timeout_ms: i64,
    ) -> anyhow::Result<VariantB> {
        Ok(VariantB {
            pool: pool_b_options(pool_size)
                .connect_lazy_with(sqlx_connect_options(info, "bench-b")),
            begin_sql: b_begin_sql(ROLE, &claims.to_string(), timeout_ms)?.into(),
            max_rows,
        })
    }

    async fn run(&self, op: &BenchOp) -> anyhow::Result<String> {
        let mut tx = self
            .pool
            .begin_with(sqlx::AssertSqlSafe(self.begin_sql.clone()))
            .await?;
        let out = sqlx_run_plans(&mut tx, op, self.max_rows).await?;
        tx.commit().await?;
        Ok(out)
    }
}

impl Exec for VariantB {
    fn kind(&self) -> VariantKind {
        VariantKind::B
    }

    fn exec(&self, op: &BenchOp) -> impl Future<Output = anyhow::Result<String>> + Send {
        self.run(op)
    }
}

// ---------------------------------------------------------------------------------------
// C and D: tokio-postgres pipelines

/// Resets the session and begins: the pipeline's first statement.
pub const PIPELINE_BEGIN_SQL: &str = "SET ROLE NONE; RESET ALL; BEGIN";

/// C's preamble: role, claims, both timeouts, then the version asserted in SQL (`$4`).
pub const C_PREAMBLE_SQL: &str = "SELECT set_config('role', $1, true), \
     set_config('request.jwt.claims', $2, true), set_config('statement_timeout', $3, true), \
     set_config('transaction_timeout', $3, true), bench.assert_schema_version($4)";

/// A plan wrapped in `bench.cap_response`, the budget the next parameter.
///
/// A read plan becomes a subquery. A write plan (`WITH r AS (<dml>) SELECT <tail>`, the
/// planner's only data-modifying shape) can't: Postgres allows a data-modifying `WITH` only
/// at the top level. Its CTE stays on top and the tail becomes a second CTE:
/// `WITH r AS (<dml>), q(r) AS (SELECT <tail>) SELECT bench.cap_response(q.r, $N) FROM q`.
/// Any other `WITH` shape is an error naming the plan.
pub fn cap_wrap(plan: &Plan) -> anyhow::Result<String> {
    const CTE: &str = "WITH r AS (";
    let n = plan.binds.len() + 1;
    let sql = plan.sql.as_str();
    let is_with = sql
        .trim_start()
        .get(..4)
        .is_some_and(|w| w.eq_ignore_ascii_case("WITH"));
    if !is_with {
        return Ok(format!(
            "SELECT bench.cap_response(q.r, ${n}) FROM ({sql}) AS q(r)"
        ));
    }
    let misfit = || anyhow::anyhow!("can't wrap the plan in cap_response: {sql}");
    if !sql.starts_with(CTE) {
        return Err(misfit());
    }
    let close = matching_paren(sql, CTE.len() - 1).ok_or_else(misfit)?;
    let dml = &sql[CTE.len()..close];
    let tail = sql[close + 1..]
        .strip_prefix(" SELECT ")
        .ok_or_else(misfit)?;
    Ok(format!(
        "WITH r AS ({dml}), q(r) AS (SELECT {tail}) SELECT bench.cap_response(q.r, ${n}) FROM q"
    ))
}

/// The index of the `)` closing the `(` at byte `open`, skipping single-quoted literals
/// (`''` escapes) and double-quoted identifiers (`""` escapes). `None` if it isn't closed.
pub fn matching_paren(sql: &str, open: usize) -> Option<usize> {
    let bytes = sql.as_bytes();
    debug_assert_eq!(bytes.get(open), Some(&b'('));
    let mut depth = 0usize;
    let mut quote: Option<u8> = None;
    let mut i = open;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) if b == q => {
                if bytes.get(i + 1) == Some(&q) {
                    i += 1; // a doubled quote: still inside
                } else {
                    quote = None;
                }
            }
            Some(_) => {}
            None => match b {
                b'\'' | b'"' => quote = Some(b),
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            },
        }
        i += 1;
    }
    None
}

/// A plan as C or D sends it: its SQL (cap-wrapped for C) and its parameter types.
#[derive(Debug)]
pub struct Prepared {
    pub sql: String,
    pub types: Vec<Type>,
}

/// C's form of a plan: wrapped, the budget an extra `int8`.
pub fn prepared_c(plan: &Plan) -> anyhow::Result<Prepared> {
    let mut types = bind_types(plan);
    types.push(Type::INT8);
    Ok(Prepared {
        sql: cap_wrap(plan)?,
        types,
    })
}

/// D's form of a plan: as planned.
pub fn prepared_d(plan: &Plan) -> anyhow::Result<Prepared> {
    Ok(Prepared {
        sql: plan.sql.clone(),
        types: bind_types(plan),
    })
}

type PlanSet = Arc<Vec<Plan>>;
/// A plan set (kept alive) and its prepared forms.
type PreparedEntry = (PlanSet, Arc<Vec<Prepared>>);

/// Each plan set's prepared forms, computed on its first request. Keyed by the `Arc`'s
/// address (the workload hands out one `Arc` per op kind); the entry keeps the `Arc` alive,
/// so an address can't be reused by another plan set while it's cached.
struct PreparedCache {
    make: fn(&Plan) -> anyhow::Result<Prepared>,
    sets: std::sync::RwLock<HashMap<usize, PreparedEntry>>,
}

impl PreparedCache {
    fn new(make: fn(&Plan) -> anyhow::Result<Prepared>) -> Self {
        PreparedCache {
            make,
            sets: Default::default(),
        }
    }

    fn get(&self, plans: &PlanSet) -> anyhow::Result<Arc<Vec<Prepared>>> {
        let key = Arc::as_ptr(plans) as usize;
        if let Some((_, p)) = self.sets.read().expect("prepared cache").get(&key) {
            return Ok(p.clone());
        }
        let prepared = Arc::new(
            plans
                .iter()
                .map(self.make)
                .collect::<anyhow::Result<Vec<_>>>()?,
        );
        self.sets
            .write()
            .expect("prepared cache")
            .insert(key, (plans.clone(), prepared.clone()));
        Ok(prepared)
    }
}

/// The parameter types a plan declares: what sqlx sends for the same binds (`text`, `jsonb`,
/// `int8`, `text[]`), so the server reads the values as rest's path does.
pub fn bind_types(plan: &Plan) -> Vec<Type> {
    plan.binds
        .iter()
        .map(|b| match b {
            Bind::Text(_) => Type::TEXT,
            Bind::TextArray(_) => Type::TEXT_ARRAY,
            Bind::Json(_) => Type::JSONB,
            Bind::Limit { .. } | Bind::Offset { .. } => Type::INT8,
        })
        .collect()
}

type Param<'a> = Box<dyn ToSql + Sync + Send + 'a>;

/// A plan's values, in parameter order.
fn plan_params<'a>(op: &'a Op, plan: &Plan, max_rows: i64) -> anyhow::Result<Vec<Param<'a>>> {
    plan.binds
        .iter()
        .map(|bind| {
            Ok(match resolve_bind(op, bind, max_rows) {
                Some(BindValue::Text(v)) => Box::new(v) as Param<'a>,
                Some(BindValue::TextArray(v)) => Box::new(v),
                Some(BindValue::Json(v)) => Box::new(v),
                Some(BindValue::Int(v)) => Box::new(v),
                None => return Err(plan_misfit(bind)),
            })
        })
        .collect()
}

fn param_refs<'p>(params: &'p [Param<'_>]) -> Vec<&'p (dyn ToSql + Sync)> {
    params
        .iter()
        .map(|p| p.as_ref() as &(dyn ToSql + Sync))
        .collect()
}

type Step<'a> =
    Pin<Box<dyn Future<Output = Result<Option<Row>, tokio_postgres::Error>> + Send + 'a>>;

/// Polls every future in vector order until all are done. tokio-postgres writes a request
/// when its future is first polled, so the first pass sends them in this order. (Written by
/// hand rather than `join_all`, whose larger inputs go through `FuturesOrdered`.)
async fn join_in_order<'a>(
    steps: Vec<Step<'a>>,
) -> Vec<Result<Option<Row>, tokio_postgres::Error>> {
    let mut steps: Vec<_> = steps.into_iter().map(maybe_done).collect();
    std::future::poll_fn(|cx| {
        let mut done = true;
        for step in steps.iter_mut() {
            if Pin::new(step).poll(cx).is_pending() {
                done = false;
            }
        }
        if done { Poll::Ready(()) } else { Poll::Pending }
    })
    .await;
    steps
        .into_iter()
        .map(|s| match s {
            MaybeDone::Done(r) => r,
            _ => unreachable!("every step finished"),
        })
        .collect()
}

/// The first error in pipeline order (a later `25P02`, "the transaction is aborted", follows
/// from it).
fn first_error(
    results: Vec<Result<Option<Row>, tokio_postgres::Error>>,
) -> Result<Vec<Option<Row>>, tokio_postgres::Error> {
    results.into_iter().collect()
}

fn text_result(row: Option<Row>) -> anyhow::Result<String> {
    let row = row.context("a plan returned no row")?;
    anyhow::ensure!(row.len() == 1, "a plan returned {} columns", row.len());
    Ok(row
        .try_get::<_, Option<String>>(0)?
        .unwrap_or_else(|| "null".into()))
}

fn tokio_config(info: &ConnInfo, application_name: &str) -> tokio_postgres::Config {
    let mut config = tokio_postgres::Config::new();
    config
        .host(&info.host)
        .port(info.port)
        .user(&info.user)
        .password(&info.password)
        .dbname(&info.database)
        .application_name(application_name);
    config
}

fn deadpool(
    info: &ConnInfo,
    application_name: &str,
    pool_size: u32,
) -> anyhow::Result<deadpool_postgres::Pool> {
    let manager = deadpool_postgres::Manager::from_config(
        tokio_config(info, application_name),
        tokio_postgres::NoTls,
        deadpool_postgres::ManagerConfig {
            recycling_method: deadpool_postgres::RecyclingMethod::Fast,
        },
    );
    Ok(deadpool_postgres::Pool::builder(manager)
        .max_size(pool_size.max(1) as usize)
        .runtime(deadpool_postgres::Runtime::Tokio1)
        .wait_timeout(Some(ACQUIRE_TIMEOUT))
        .build()?)
}

/// A pipeline variant (C or D).
pub struct Pipelined {
    pub pool: deadpool_postgres::Pool,
    ctx: RunCtx,
    expected_version: uuid::Uuid,
    prepared: PreparedCache,
}

impl Pipelined {
    #[allow(clippy::too_many_arguments)]
    fn connect(
        info: &ConnInfo,
        application_name: &str,
        pool_size: u32,
        claims: &serde_json::Value,
        expected_version: uuid::Uuid,
        max_rows: i64,
        timeout_ms: i64,
        make: fn(&Plan) -> anyhow::Result<Prepared>,
    ) -> anyhow::Result<Pipelined> {
        Ok(Pipelined {
            pool: deadpool(info, application_name, pool_size)?,
            ctx: RunCtx::new(claims, max_rows, timeout_ms),
            expected_version,
            prepared: PreparedCache::new(make),
        })
    }
}

/// The full pipeline: one round trip once its statements are prepared.
pub struct VariantC(pub Pipelined);

impl VariantC {
    pub fn connect(
        info: &ConnInfo,
        pool_size: u32,
        claims: &serde_json::Value,
        expected_version: uuid::Uuid,
        max_rows: i64,
        timeout_ms: i64,
    ) -> anyhow::Result<VariantC> {
        Pipelined::connect(
            info,
            "bench-c",
            pool_size,
            claims,
            expected_version,
            max_rows,
            timeout_ms,
            prepared_c,
        )
        .map(VariantC)
    }

    async fn run(&self, op: &BenchOp) -> anyhow::Result<String> {
        let Pipelined {
            pool,
            ctx,
            expected_version,
            prepared,
        } = &self.0;
        let wrapped = prepared.get(&op.plans)?;
        let client = pool.get().await?;
        let budget = DEFAULT_MAX_RESPONSE_BYTES as i64;

        // 1. Prepare everything first (no I/O when cached).
        let (preamble, stmts) = futures_util::try_join!(
            client.prepare_typed_cached(
                C_PREAMBLE_SQL,
                &[Type::TEXT, Type::TEXT, Type::TEXT, Type::UUID]
            ),
            futures_util::future::try_join_all(
                wrapped
                    .iter()
                    .map(|p| client.prepare_typed_cached(&p.sql, &p.types))
            ),
        )?;

        // 2. The values.
        let mut params = Vec::with_capacity(op.plans.len());
        for p in op.plans.iter() {
            let mut values = plan_params(&op.op, p, ctx.max_rows)?;
            values.push(Box::new(budget));
            params.push(values);
        }
        let refs: Vec<Vec<&(dyn ToSql + Sync)>> = params.iter().map(|v| param_refs(v)).collect();
        let preamble_params: [&(dyn ToSql + Sync); 4] =
            [&ctx.role, &ctx.claims, &ctx.timeout, expected_version];

        // 3. The pipeline, created in order and polled in order.
        let c = &**client;
        let mut steps: Vec<Step<'_>> = Vec::with_capacity(op.plans.len() + 3);
        steps.push(Box::pin(async move {
            c.batch_execute(PIPELINE_BEGIN_SQL).await.map(|()| None)
        }));
        let preamble = &preamble;
        let preamble_params = &preamble_params;
        steps.push(Box::pin(async move {
            c.query_one(preamble, preamble_params).await.map(Some)
        }));
        for (stmt, values) in stmts.iter().zip(&refs) {
            steps.push(Box::pin(async move {
                c.query_one(stmt, values).await.map(Some)
            }));
        }
        steps.push(Box::pin(async move {
            c.batch_execute("COMMIT").await.map(|()| None)
        }));
        let rows = first_error(join_in_order(steps).await)?;

        let n = op.plans.len();
        let results = rows
            .into_iter()
            .skip(2)
            .take(n)
            .map(text_result)
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(join_results(&op.op, results))
    }
}

impl Exec for VariantC {
    fn kind(&self) -> VariantKind {
        VariantKind::C
    }

    fn exec(&self, op: &BenchOp) -> impl Future<Output = anyhow::Result<String>> + Send {
        self.run(op)
    }
}

/// The pipeline without `COMMIT`: the version checked in Rust, then `COMMIT` (2 round trips).
pub struct VariantD(pub Pipelined);

impl VariantD {
    pub fn connect(
        info: &ConnInfo,
        pool_size: u32,
        claims: &serde_json::Value,
        expected_version: uuid::Uuid,
        max_rows: i64,
        timeout_ms: i64,
    ) -> anyhow::Result<VariantD> {
        Pipelined::connect(
            info,
            "bench-d",
            pool_size,
            claims,
            expected_version,
            max_rows,
            timeout_ms,
            prepared_d,
        )
        .map(VariantD)
    }

    async fn run(&self, op: &BenchOp) -> anyhow::Result<String> {
        let Pipelined {
            pool,
            ctx,
            expected_version,
            prepared,
        } = &self.0;
        let typed = prepared.get(&op.plans)?;
        let client = pool.get().await?;

        let (preamble, stmts) = futures_util::try_join!(
            client.prepare_typed_cached(PREAMBLE_SQL, &[Type::TEXT, Type::TEXT, Type::TEXT, Type::INT8]),
            futures_util::future::try_join_all(
                typed
                    .iter()
                    .map(|p| client.prepare_typed_cached(&p.sql, &p.types))
            ),
        )?;

        let mut params = Vec::with_capacity(op.plans.len());
        for p in op.plans.iter() {
            params.push(plan_params(&op.op, p, ctx.max_rows)?);
        }
        let refs: Vec<Vec<&(dyn ToSql + Sync)>> = params.iter().map(|v| param_refs(v)).collect();
        // No role OID: the name-only check, as for the public key.
        let no_oid: Option<i64> = None;
        let preamble_params: [&(dyn ToSql + Sync); 4] = [&ctx.role, &ctx.claims, &ctx.timeout, &no_oid];

        let c = &**client;
        let mut steps: Vec<Step<'_>> = Vec::with_capacity(op.plans.len() + 2);
        steps.push(Box::pin(async move {
            c.batch_execute(PIPELINE_BEGIN_SQL).await.map(|()| None)
        }));
        let preamble = &preamble;
        let preamble_params = &preamble_params;
        steps.push(Box::pin(async move {
            c.query_one(preamble, preamble_params).await.map(Some)
        }));
        for (stmt, values) in stmts.iter().zip(&refs) {
            steps.push(Box::pin(async move {
                c.query_one(stmt, values).await.map(Some)
            }));
        }
        let joined = first_error(join_in_order(steps).await);

        // The transaction is open whatever happened: end it before the connection goes back.
        let checked = joined.map_err(anyhow::Error::from).and_then(|rows| {
            let mut rows = rows.into_iter().skip(1);
            let pre = rows
                .next()
                .flatten()
                .context("the preamble returned no row")?;
            let found: Option<String> = pre.try_get(4)?;
            if found.as_deref() != Some(expected_version.to_string().as_str()) {
                return Err(StaleVersion {
                    expected: *expected_version,
                    found,
                }
                .into());
            }
            let results = rows.map(text_result).collect::<anyhow::Result<Vec<_>>>()?;
            check_budget(&results)?;
            Ok(results)
        });
        match checked {
            Ok(results) => {
                c.batch_execute("COMMIT").await?;
                Ok(join_results(&op.op, results))
            }
            Err(e) => {
                // A failed rollback leaves a broken connection; the pool drops it on recycle
                // only if it's closed, so report both.
                if let Err(rb) = c.batch_execute("ROLLBACK").await {
                    return Err(e.context(format!("and the rollback failed: {rb}")));
                }
                Err(e)
            }
        }
    }
}

impl Exec for VariantD {
    fn kind(&self) -> VariantKind {
        VariantKind::D
    }

    fn exec(&self, op: &BenchOp) -> impl Future<Output = anyhow::Result<String>> + Send {
        self.run(op)
    }
}

// ---------------------------------------------------------------------------------------
// R: the shipped path

/// Where R finds the branch: rest's internal source, as the service itself uses it.
#[derive(Debug, Clone)]
pub struct RTarget {
    pub internal_url: String,
    pub internal_token: String,
    pub branch: uuid::Uuid,
}

/// The identity of an R request: a key's `authenticated` role with the fixed claims.
pub fn r_identity(branch: uuid::Uuid) -> DataIdentity {
    DataIdentity {
        branch_id: branch,
        key_id: uuid::Uuid::nil(),
        kind: DataKeyKind::Public,
        role: ROLE.to_owned(),
        claims: fixed_claims(),
        databases: None,
        project_id: None,
        role_oid: None,
        server_vouched: false,
    }
}

/// `Engine::query` on the internal source: parse, database pick, pool, owner-level check
/// (cached), the pipelined `run`.
pub struct VariantR {
    engine: Engine<InternalSource>,
    identity: DataIdentity,
}

impl VariantR {
    pub fn connect(target: &RTarget, pool_size: u32) -> VariantR {
        VariantR {
            engine: Engine::new(
                InternalSource::new(&target.internal_url, &target.internal_token),
                pool_size.max(1) as usize,
            ),
            identity: r_identity(target.branch),
        }
    }

    async fn run(&self, op: &BenchOp) -> anyhow::Result<String> {
        self.engine
            .query(&self.identity, "bench-r", &op.body)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))
    }

    /// Drops the engine's pools and connections for the branch.
    pub async fn close(&self) {
        self.engine.invalidate(self.identity.branch_id).await;
    }
}

impl Exec for VariantR {
    fn kind(&self) -> VariantKind {
        VariantKind::R
    }

    fn exec(&self, op: &BenchOp) -> impl Future<Output = anyhow::Result<String>> + Send {
        self.run(op)
    }
}

// ---------------------------------------------------------------------------------------
// All of them

pub enum AnyVariant {
    A(VariantA),
    B(VariantB),
    C(VariantC),
    D(VariantD),
    R(Box<VariantR>),
}

impl Exec for AnyVariant {
    fn kind(&self) -> VariantKind {
        match self {
            AnyVariant::A(v) => v.kind(),
            AnyVariant::B(v) => v.kind(),
            AnyVariant::C(v) => v.kind(),
            AnyVariant::D(v) => v.kind(),
            AnyVariant::R(v) => v.kind(),
        }
    }

    async fn exec(&self, op: &BenchOp) -> anyhow::Result<String> {
        match self {
            AnyVariant::A(v) => v.exec(op).await,
            AnyVariant::B(v) => v.exec(op).await,
            AnyVariant::C(v) => v.exec(op).await,
            AnyVariant::D(v) => v.exec(op).await,
            AnyVariant::R(v) => v.exec(op).await,
        }
    }
}

impl AnyVariant {
    /// One variant of `kind`, with its own lazily connected pool of `pool_size` (R reaches
    /// the branch through `target`, the others through `info`).
    #[allow(clippy::too_many_arguments)]
    pub fn connect(
        kind: VariantKind,
        info: &ConnInfo,
        target: &RTarget,
        pool_size: u32,
        claims: &serde_json::Value,
        expected_version: uuid::Uuid,
        max_rows: i64,
        timeout_ms: i64,
    ) -> anyhow::Result<AnyVariant> {
        Ok(match kind {
            VariantKind::A => AnyVariant::A(VariantA::connect(
                info, pool_size, claims, max_rows, timeout_ms,
            )),
            VariantKind::B => AnyVariant::B(VariantB::connect(
                info, pool_size, claims, max_rows, timeout_ms,
            )?),
            VariantKind::C => AnyVariant::C(VariantC::connect(
                info,
                pool_size,
                claims,
                expected_version,
                max_rows,
                timeout_ms,
            )?),
            VariantKind::D => AnyVariant::D(VariantD::connect(
                info,
                pool_size,
                claims,
                expected_version,
                max_rows,
                timeout_ms,
            )?),
            VariantKind::R => AnyVariant::R(Box::new(VariantR::connect(target, pool_size))),
        })
    }

    /// Closes this variant's pool and its connections.
    pub async fn close(&self) {
        match self {
            AnyVariant::A(v) => v.pool.close().await,
            AnyVariant::B(v) => v.pool.close().await,
            AnyVariant::C(v) => v.0.pool.close(),
            AnyVariant::D(v) => v.0.pool.close(),
            AnyVariant::R(v) => v.close().await,
        }
    }
}

/// The selected variants, each with its own pool of `pool_size` connections.
pub struct Variants {
    /// In the order selected.
    pub all: Vec<AnyVariant>,
}

impl Variants {
    #[allow(clippy::too_many_arguments)]
    pub async fn connect(
        kinds: &[VariantKind],
        info: &ConnInfo,
        target: &RTarget,
        pool_size: u32,
        claims: &serde_json::Value,
        expected_version: uuid::Uuid,
        max_rows: i64,
        timeout_ms: i64,
    ) -> anyhow::Result<Variants> {
        let all = kinds
            .iter()
            .map(|&kind| {
                AnyVariant::connect(
                    kind,
                    info,
                    target,
                    pool_size,
                    claims,
                    expected_version,
                    max_rows,
                    timeout_ms,
                )
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(Variants { all })
    }

    /// C, for the sanity ping (every run selects it).
    pub fn c(&self) -> &VariantC {
        self.all
            .iter()
            .find_map(|v| match v {
                AnyVariant::C(c) => Some(c),
                _ => None,
            })
            .expect("Variants holds C")
    }

    /// One `SELECT 1` on C's pool (a simple query: one round trip), timed without the acquire.
    pub async fn ping(&self) -> anyhow::Result<Duration> {
        let client = self.c().0.pool.get().await?;
        let start = Instant::now();
        client.batch_execute("SELECT 1").await?;
        Ok(start.elapsed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_query::query::{Path, Slot};
    use chelabase_query::sql::{Bind, Plan};
    use rest::conn::{ACQUIRE_TIMEOUT, CONNECTION_IDLE};

    #[test]
    fn variant_a_uses_rest_pool_options() {
        let o = pool_a_options(32);
        assert_eq!(o.get_max_connections(), 32);
        assert_eq!(o.get_idle_timeout(), Some(CONNECTION_IDLE));
        assert_eq!(o.get_acquire_timeout(), ACQUIRE_TIMEOUT);
        assert!(o.get_test_before_acquire());
        assert!(!pool_b_options(32).get_test_before_acquire());
    }

    #[test]
    fn b_begin_statement_quotes_every_value() {
        let sql = b_begin_sql("o'role", r#"{"sub":"it's","x":"a\b"}"#, 5000).unwrap();
        assert_eq!(
            sql,
            "SET ROLE NONE; RESET ALL; BEGIN; SELECT set_config('role', 'o''role', true), \
             set_config('request.jwt.claims', '{\"sub\":\"it''s\",\"x\":\"a\\b\"}', true), \
             set_config('statement_timeout', '5000', true), \
             set_config('transaction_timeout', '5000', true)"
        );
        assert!(b_begin_sql("r", "a\0b", 1).is_err());
    }

    #[test]
    fn cap_wrap_uses_next_param_index() {
        let text = |i| {
            Bind::Text(Path {
                op: None,
                slot: Slot::Where(vec![i]),
            })
        };
        let plan = Plan {
            sql: "SELECT x($1, $2, $3)::text".into(),
            binds: vec![text(0), text(1), Bind::Limit { op: None }],
            capped_sql: String::new(),
        };
        assert_eq!(
            cap_wrap(&plan).unwrap(),
            "SELECT bench.cap_response(q.r, $4) FROM (SELECT x($1, $2, $3)::text) AS q(r)"
        );
        assert_eq!(bind_types(&plan), [Type::TEXT, Type::TEXT, Type::INT8]);
    }

    /// Every bind kind declares the type rest's engine declares for it (`text[]` included).
    #[test]
    fn bind_types_match_the_engine() {
        let at = || Path {
            op: None,
            slot: Slot::Where(vec![]),
        };
        let plan = Plan {
            sql: String::new(),
            binds: vec![
                Bind::Text(at()),
                Bind::TextArray(at()),
                Bind::Json(at()),
                Bind::Limit { op: None },
                Bind::Offset { op: None },
            ],
            capped_sql: String::new(),
        };
        assert_eq!(
            bind_types(&plan),
            [Type::TEXT, Type::TEXT_ARRAY, Type::JSONB, Type::INT8, Type::INT8]
        );
    }

    fn real_workload() -> crate::workload::Workload {
        crate::workload::Workload::from_catalog(
            &crate::workload::tests::BenchCatalog::new(),
            "public",
        )
        .unwrap()
    }

    /// A write plan's data-modifying CTE must stay top-level: the cap goes in a second CTE.
    fn assert_write_wrap(plan: &Plan) {
        let wrapped = cap_wrap(plan).unwrap();
        assert!(wrapped.starts_with("WITH r AS (INSERT"), "{wrapped}");
        assert!(wrapped.contains("q(r) AS (SELECT"), "{wrapped}");
        assert!(wrapped.ends_with("FROM q"), "{wrapped}");
        let cap = format!("bench.cap_response(q.r, ${})", plan.binds.len() + 1);
        assert!(wrapped.contains(&cap), "{wrapped}");
        assert!(!wrapped.contains("FROM (WITH"), "{wrapped}");
    }

    #[test]
    fn cap_wrap_keeps_insert_cte_top_level() {
        let w = real_workload();
        let mut rng = rand::rng();
        let insert = w.next(crate::workload::OpKind::Insert, &mut rng);
        assert_write_wrap(&insert.plans[0]);
        let batch = w.next(crate::workload::OpKind::Batch, &mut rng);
        assert_write_wrap(&batch.plans[2]);
        // The batch's reads keep the subquery wrap.
        let read = &batch.plans[0];
        let prefix = format!(
            "SELECT bench.cap_response(q.r, ${}) FROM (SELECT",
            read.binds.len() + 1
        );
        assert!(cap_wrap(read).unwrap().starts_with(&prefix));
    }

    /// Each plan set is wrapped once; later requests reuse it.
    #[test]
    fn prepared_cache_wraps_each_plan_set_once() {
        let w = real_workload();
        let mut rng = rand::rng();
        let cache = PreparedCache::new(prepared_c);
        let a = w.next(crate::workload::OpKind::Batch, &mut rng);
        let b = w.next(crate::workload::OpKind::Batch, &mut rng);
        let first = cache.get(&a.plans).unwrap();
        assert!(Arc::ptr_eq(&first, &cache.get(&b.plans).unwrap()));
        assert_eq!(first.len(), 3);
        assert_eq!(first[2].sql, cap_wrap(&a.plans[2]).unwrap());
        assert_eq!(first[2].types.last(), Some(&Type::INT8));
        let other = w.next(crate::workload::OpKind::ByKey, &mut rng);
        assert!(!Arc::ptr_eq(&first, &cache.get(&other.plans).unwrap()));
    }

    #[test]
    fn cap_wrap_splices_the_exact_shape() {
        let plan = |sql: &str| Plan {
            sql: sql.into(),
            capped_sql: String::new(),
            binds: vec![Bind::Limit { op: None }],
        };
        assert_eq!(
            cap_wrap(&plan(
                "WITH r AS (INSERT INTO \"t)\" (\"a\") SELECT ')' RETURNING \"a\") SELECT x FROM (SELECT * FROM r LIMIT $1) t"
            ))
            .unwrap(),
            "WITH r AS (INSERT INTO \"t)\" (\"a\") SELECT ')' RETURNING \"a\"), \
             q(r) AS (SELECT x FROM (SELECT * FROM r LIMIT $1) t) \
             SELECT bench.cap_response(q.r, $2) FROM q"
        );
        for bad in [
            "WITH r AS (INSERT INTO t SELECT 1), s AS (SELECT 1) SELECT 1",
            "WITH r AS (INSERT INTO t SELECT 1) UPDATE t SET a = 1",
            "WITH r AS (INSERT INTO t SELECT ')'",
            "WITH x AS (SELECT 1) SELECT 1",
        ] {
            let e = cap_wrap(&plan(bad)).expect_err(bad);
            assert!(format!("{e:#}").contains(bad), "{e:#}");
        }
    }

    #[test]
    fn matching_paren_skips_literals_and_identifiers() {
        let sql = r#"(a ")"" )" ')'')' (b)) tail"#;
        assert_eq!(matching_paren(sql, 0), Some(sql.find(" tail").unwrap() - 1));
        assert_eq!(matching_paren("(a ')'", 0), None);
        assert_eq!(matching_paren(r#"(a ")"#, 0), None);
    }

    /// The pipeline's futures are polled, the first time, in the order they were pushed.
    #[tokio::test]
    async fn join_in_order_polls_in_push_order() {
        let order = std::sync::Mutex::new(Vec::new());
        let steps: Vec<Step<'_>> = (0..40)
            .map(|i| {
                let order = &order;
                Box::pin(async move {
                    order.lock().unwrap().push(i);
                    tokio::task::yield_now().await;
                    Ok(None)
                }) as Step<'_>
            })
            .collect();
        let results = join_in_order(steps).await;
        assert_eq!(results.len(), 40);
        assert_eq!(*order.lock().unwrap(), (0..40).collect::<Vec<_>>());
    }

    #[test]
    fn variant_kinds_parse_from_their_letters() {
        assert_eq!("C".parse::<VariantKind>().unwrap(), VariantKind::C);
        assert_eq!("r".parse::<VariantKind>().unwrap(), VariantKind::R);
        assert!("X".parse::<VariantKind>().is_err());
        // The default run stays the four Phase 1 variants.
        assert!(!ALL_VARIANTS.contains(&VariantKind::R));
        assert_eq!(VariantKind::R.application_name(), "chelabase-rest");
        assert_eq!(VariantKind::C.application_name(), "bench-c");
    }

    #[test]
    fn r_runs_as_authenticated_with_the_fixed_claims() {
        let id = r_identity(uuid::Uuid::nil());
        assert_eq!(id.role, ROLE);
        assert_eq!(id.claims, fixed_claims());
        assert_eq!(id.branch_id, uuid::Uuid::nil());
        assert!(id.databases.is_none());
    }

    #[test]
    fn sqlstate_reads_d_stale_version() {
        let e = anyhow::Error::from(StaleVersion {
            expected: uuid::Uuid::nil(),
            found: Some("x".into()),
        });
        assert_eq!(sqlstate(&e).as_deref(), Some(STALE_VERSION));
        assert_eq!(sqlstate(&anyhow::anyhow!("other")), None);
    }

    #[test]
    fn batch_results_join_as_the_engine_does() {
        let batch = Op::Batch(vec![]);
        assert_eq!(
            join_results(&batch, vec!["1".into(), "[]".into()]),
            "[1,[]]"
        );
        let workload = crate::workload::Workload::from_catalog(
            &crate::workload::tests::BenchCatalog::new(),
            "public",
        )
        .unwrap();
        let mut rng = rand::rng();
        let one = workload.next(crate::workload::OpKind::ByKey, &mut rng);
        assert_eq!(join_results(&one.op, vec!["[{}]".into()]), "[{}]");
    }

    fn unreachable_info() -> ConnInfo {
        ConnInfo {
            host: "127.0.0.1".into(),
            port: 1,
            server_name: None,
            database: "main".into(),
            user: "authenticator".into(),
            password: "x".into(),
            settings: rest::conn::Settings {
                max_rows: 1000,
                statement_timeout_ms: 8000,
                max_connections: 4,
            },
            databases: vec![],
        }
    }

    fn unreachable_target() -> RTarget {
        RTarget {
            internal_url: "http://127.0.0.1:1".into(),
            internal_token: "x".into(),
            branch: uuid::Uuid::nil(),
        }
    }

    /// Throughput builds a fresh variant per cell, with its latency pool's options.
    #[tokio::test]
    async fn connect_one_builds_each_kind_with_its_pool_size() {
        for kind in [
            VariantKind::A,
            VariantKind::B,
            VariantKind::C,
            VariantKind::D,
            VariantKind::R,
        ] {
            let v = AnyVariant::connect(
                kind,
                &unreachable_info(),
                &unreachable_target(),
                7,
                &fixed_claims(),
                uuid::Uuid::nil(),
                1000,
                8000,
            )
            .unwrap();
            assert_eq!(v.kind(), kind);
            let size = match &v {
                AnyVariant::A(a) => a.pool.options().get_max_connections() as usize,
                AnyVariant::B(b) => b.pool.options().get_max_connections() as usize,
                AnyVariant::C(c) => c.0.pool.status().max_size,
                AnyVariant::D(d) => d.0.pool.status().max_size,
                AnyVariant::R(_) => 7,
            };
            assert_eq!(size, 7, "{kind:?}");
        }
    }

    /// Throughput runs the variants one after another; each closes its pool when done, so
    /// the next one's connections fit the compute's `max_connections`.
    #[tokio::test]
    async fn close_closes_each_variants_pool() {
        let v = Variants::connect(
            &ALL_VARIANTS,
            &unreachable_info(),
            &unreachable_target(),
            2,
            &fixed_claims(),
            uuid::Uuid::nil(),
            1000,
            8000,
        )
        .await
        .unwrap();
        for any in &v.all {
            any.close().await;
        }
        for any in &v.all {
            let closed = match any {
                AnyVariant::A(a) => a.pool.is_closed(),
                AnyVariant::B(b) => b.pool.is_closed(),
                AnyVariant::C(c) => c.0.pool.is_closed(),
                AnyVariant::D(d) => d.0.pool.is_closed(),
                AnyVariant::R(_) => true, // not in ALL_VARIANTS
            };
            assert!(closed, "{:?} still open", any.kind());
        }
    }
}
