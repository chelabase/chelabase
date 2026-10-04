//! The pipelined run (shape C): one round trip for a whole request once its statements are
//! prepared on the connection.
//!
//! 1. Every value is encoded first: a value that doesn't fit its type fails the request
//!    before anything is sent.
//! 2. Every statement (the guarded preamble, each plan's `capped_sql`) is prepared with
//!    declared types, through the connection's statement cache ([`QueryClient`]); all of
//!    them finish before any step of the pipeline exists.
//! 3. One pipeline, polled in push order ([`join_in_order`]): `[ROLLBACK;] BEGIN; <reset>` (the `ROLLBACK`
//!    only when the connection may be in a transaction), the preamble (role, claims and timeouts, and `chelabase.assert_schema_version`), each plan
//!    (its result through `chelabase.cap_response`), `COMMIT`, then `<reset>` again so the
//!    next request prepares on a clean session (its failure marks the connection broken,
//!    [`QueryClient::mark_broken`], and never changes the response). A failed step aborts
//!    the transaction: the later steps fail with `25P02` and the `COMMIT` rolls back.
//! 4. The first real error is the request's ([`step_failed`]): a stale version or a database
//!    without the guard functions sends the request down the sequential path once
//!    ([`settle`]), the cap's error is the 413, and anything else maps as the sequential path
//!    maps it.
//! 5. Rust counts the results again against the budget ([`response`]).

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::Poll;

use bytes::BytesMut;
use futures_util::future::{MaybeDone, maybe_done};
use tokio_postgres::types::{IsNull, ToSql, Type, to_sql_checked};
use tokio_postgres::{Client, Row, Statement, TransactionStatus};
use uuid::Uuid;

use crate::engine::{
    QueryCache, ResponseBudget, RunAs, begin_sql, in_database, join_results, preamble_db_failure,
    too_large,
};
use crate::error::{DbFields, QueryFailure};
use crate::query::Op;

/// `chelabase.assert_schema_version`'s SQLSTATE: the database's schema version isn't the one
/// the request was planned against.
pub const STALE_VERSION: &str = "CB001";
/// `chelabase.cap_response`'s SQLSTATE: the response passed its budget.
pub const TOO_LARGE: &str = "CB002";

/// A connection a request runs on: a plain [`Client`] (statements prepared each time) or a
/// pooled one ([`Lease`], statements cached per connection).
pub trait QueryClient: Sync {
    /// The connection itself.
    fn client(&self) -> &Client;

    /// `sql` prepared with these parameter types (from the connection's cache when it has
    /// one).
    fn prepare_cached(
        &self,
        sql: &str,
        types: &[Type],
    ) -> impl Future<Output = Result<Statement, tokio_postgres::Error>> + Send;

    /// The session couldn't be reset after a request (logged at warn): its state (a
    /// `search_path`, a role) may leak into the next request's prepared statements, so the
    /// connection must not serve another request.
    fn mark_broken(&self);

    /// The connection's transaction status (see [`Client::transaction_status`]): only
    /// [`TransactionStatus::Idle`] means no transaction is open; anything else (`Unknown`
    /// while a request is in flight, included) may be inside one, so a `ROLLBACK` goes first.
    fn transaction_status(&self) -> TransactionStatus {
        self.client().transaction_status()
    }
}

/// A bare client: nothing cached, and this crate doesn't pool it, so marking it broken does
/// nothing (whoever reuses one, tests, owns that call).
impl QueryClient for Client {
    fn client(&self) -> &Client {
        self
    }

    fn prepare_cached(
        &self,
        sql: &str,
        types: &[Type],
    ) -> impl Future<Output = Result<Statement, tokio_postgres::Error>> + Send {
        self.prepare_typed(sql, types)
    }

    fn mark_broken(&self) {}
}

/// The most prepared statements a pooled connection keeps ([`Lease`]): at this many, its
/// cache is cleared before the next prepare (each dropped statement is closed on the server).
/// deadpool-postgres's per-connection cache has no bound of its own, and every distinct
/// request shape is a statement, so without one anyone holding a publishable key could grow
/// a connection's memory (and the server's) without end. 500 is what sqlx's LRU kept.
pub const STATEMENT_CACHE_CAP: usize = 500;

/// A pooled connection lent to one call (statements prepared through the connection's
/// cache). Dropped, it goes back to the pool only when the call finished ([`Lease::done`])
/// and the session was reset; otherwise (a cancelled or timed-out call that skipped the
/// trailing reset and may have left a transaction open, a panic, an error the caller
/// returned early with, a session that couldn't be reset) it's taken out of the pool and
/// closed.
///
/// ```ignore
/// let mut lease = Lease::new(pool.get().await?);
/// let out = run(&lease, /* ... */).await;
/// lease.done();
/// out
/// ```
pub struct Lease {
    object: Option<deadpool_postgres::Object>,
    broken: AtomicBool,
    done: bool,
}

/// Whether a lease returns its connection to the pool.
fn keeps(done: bool, broken: bool) -> bool {
    done && !broken
}

impl Lease {
    pub fn new(object: deadpool_postgres::Object) -> Self {
        Lease {
            object: Some(object),
            broken: AtomicBool::new(false),
            done: false,
        }
    }

    /// The call on the connection finished (not cancelled, not panicking): it may go back
    /// to the pool, unless its session couldn't be reset.
    pub fn done(&mut self) {
        self.done = true;
    }

    /// Whether the session couldn't be reset: the connection won't go back to the pool.
    pub fn is_broken(&self) -> bool {
        self.broken.load(Ordering::Relaxed)
    }

    fn wrapper(&self) -> &deadpool_postgres::ClientWrapper {
        self.object
            .as_ref()
            .expect("a lease holds its connection until dropped")
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(object) = self.object.take()
            && !keeps(self.done, self.is_broken())
        {
            drop(deadpool_postgres::Object::take(object));
        }
    }
}

impl QueryClient for Lease {
    fn client(&self) -> &Client {
        self.wrapper()
    }

    fn prepare_cached(
        &self,
        sql: &str,
        types: &[Type],
    ) -> impl Future<Output = Result<Statement, tokio_postgres::Error>> + Send {
        let wrapper = self.wrapper();
        // Bounded ([`STATEMENT_CACHE_CAP`]): statements still in use stay valid until
        // they're dropped; the rest are closed now.
        if wrapper.statement_cache.size() >= STATEMENT_CACHE_CAP {
            wrapper.statement_cache.clear();
        }
        wrapper.prepare_typed_cached(sql, types)
    }

    fn mark_broken(&self) {
        self.broken.store(true, Ordering::Relaxed);
    }
}

/// One step of the pipeline.
pub(crate) type Step<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Polls every future in vector order until all are done. tokio-postgres writes a request
/// when its future is first polled, so the first pass sends them in this order. (Written by
/// hand rather than `join_all`, whose larger inputs go through `FuturesOrdered`.)
pub(crate) async fn join_in_order<T>(steps: Vec<Step<'_, T>>) -> Vec<T> {
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

/// What a failed step's error tells (tokio-postgres's errors have no public constructor, so
/// tests use a fake).
pub(crate) trait StepError {
    /// Postgres's error, when the server sent one.
    fn db(&self) -> Option<&dyn DbFields>;
    /// The failure the sequential path answers for it ([`QueryFailure::from_pg`]).
    fn failure(&self, request_id: &str) -> QueryFailure;
    /// The error for the log.
    fn describe(&self) -> String {
        match self.db() {
            Some(db) => format!("{}: {}", db.code(), db.message()),
            None => "not a database error".into(),
        }
    }
}

impl StepError for tokio_postgres::Error {
    fn db(&self) -> Option<&dyn DbFields> {
        self.as_db_error().map(|db| db as &dyn DbFields)
    }

    fn failure(&self, request_id: &str) -> QueryFailure {
        QueryFailure::from_pg(self, request_id)
    }

    fn describe(&self) -> String {
        format!("{self}: {self:?}")
    }
}

/// Takes the trailing reset's result (the last step, after `COMMIT`) off `results`. It never
/// decides the response (`COMMIT` did); its failure is logged and marks the connection
/// broken.
pub(crate) fn take_trailing_reset<T, E: StepError, C: QueryClient + ?Sized>(
    results: &mut Vec<Result<T, E>>,
    client: &C,
    request_id: &str,
) {
    if let Some(Err(e)) = results.pop() {
        tracing::warn!(
            request_id,
            "the session reset after the request failed; the connection is discarded: {}",
            e.describe()
        );
        client.mark_broken();
    }
}

/// Whether each prepared statement takes exactly the values encoded for it (`expected`:
/// each statement's parameter count, in order); a 500 otherwise, before anything is sent.
pub(crate) fn params_fit(
    expected: impl Iterator<Item = usize>,
    encoded: &[Vec<Encoded>],
    request_id: &str,
) -> Result<(), QueryFailure> {
    let expected: Vec<usize> = expected.collect();
    let given: Vec<usize> = encoded.iter().map(Vec::len).collect();
    if expected == given {
        Ok(())
    } else {
        Err(QueryFailure::internal(
            request_id,
            format!("prepared statements take {expected:?} parameters, given {given:?}"),
        ))
    }
}

/// The steps' outputs in order, or the first real error and its step's index: the `25P02`s
/// ("the transaction is aborted") only follow an earlier error, so they never win over one.
pub(crate) fn first_failure<T, E: StepError>(
    results: Vec<Result<T, E>>,
) -> Result<Vec<T>, (usize, E)> {
    let mut aborted = None;
    let mut out = Vec::with_capacity(results.len());
    for (i, r) in results.into_iter().enumerate() {
        match r {
            Ok(v) => out.push(v),
            Err(e) if e.db().is_some_and(|db| db.code() == "25P02") => {
                aborted.get_or_insert((i, e));
            }
            Err(e) => return Err((i, e)),
        }
    }
    match aborted {
        Some(first) => Err(first),
        None => Ok(out),
    }
}

/// How a pipelined attempt ended.
#[derive(Debug)]
pub(crate) enum Attempt {
    /// The request's answer.
    Done(Result<String, QueryFailure>),
    /// The pipeline can't run (no version known, a schema or view not cached, a statement
    /// that didn't prepare): the sequential path answers.
    Sequential,
    /// The schema changed since the version cached: drop it, then the sequential path.
    Stale,
    /// The database lacks the guard functions: mark it, then the sequential path.
    NoGuard,
}

/// What a step's failure is mapped with.
pub(crate) struct Ctx<'a> {
    pub run_as: RunAs<'a>,
    /// The database's name, for the missing-schema message.
    pub database: &'a str,
    /// The response's byte budget.
    pub budget: usize,
    pub request_id: &'a str,
    /// Counts the requests sent as one pipeline ([`QueryCache::pipelined_runs`]).
    pub runs: &'a AtomicU64,
}

/// A step of the pipeline, by what it sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepKind {
    /// `[ROLLBACK;] BEGIN; <reset>`.
    Begin,
    Preamble,
    Plan,
    Commit,
}

/// The kind of step `index` in a pipeline of `len` steps.
fn step_kind(index: usize, len: usize) -> StepKind {
    match index {
        0 => StepKind::Begin,
        1 => StepKind::Preamble,
        i if i + 1 == len => StepKind::Commit,
        _ => StepKind::Plan,
    }
}

/// Whether Postgres says a guard function doesn't exist (the database predates them).
fn guard_missing(db: &dyn DbFields) -> bool {
    db.code() == "42883"
        && (db.message().contains("chelabase.assert_schema_version")
            || db.message().contains("chelabase.cap_response"))
}

/// The request's outcome when `step` failed with `e` (the first real error).
pub(crate) fn step_failed<E: StepError>(step: StepKind, e: &E, ctx: &Ctx<'_>) -> Attempt {
    if let Some(db) = e.db() {
        if guard_missing(db) {
            return Attempt::NoGuard;
        }
        match (step, db.code()) {
            (StepKind::Preamble, STALE_VERSION) => return Attempt::Stale,
            (StepKind::Plan, TOO_LARGE) => return Attempt::Done(Err(too_large(ctx.budget))),
            (StepKind::Preamble, _) => {
                return Attempt::Done(Err(in_database(
                    preamble_db_failure(db, ctx.run_as, ctx.request_id),
                    ctx.database,
                )));
            }
            _ => {}
        }
    }
    Attempt::Done(Err(e.failure(ctx.request_id)))
}

/// A statement didn't prepare: a database without the guard functions is marked; anything
/// else (a schema that changed under a cached plan, a lost connection) the sequential path
/// meets again and answers as it always does.
pub(crate) fn prepare_failed<E: StepError>(e: &E) -> Attempt {
    match e.db() {
        Some(db) if guard_missing(db) => Attempt::NoGuard,
        _ => Attempt::Sequential,
    }
}

/// The budget `chelabase.cap_response` checks for a request of `plans` results: the server
/// counts each result's bytes, Rust each result and a separator, so the server's count
/// refuses exactly where Rust's would.
pub(crate) fn sql_budget(budget: usize, plans: usize) -> i64 {
    i64::try_from(budget.saturating_sub(plans)).unwrap_or(i64::MAX)
}

/// The results, counted again (each and a separator, as the sequential path counts them)
/// and joined: past the budget it's the 413. Only a forged server-side counter gets here
/// with too much, after the request committed.
///
/// A NULL result (a `void` function) is `null` in the response but counts as
/// `chelabase.cap_response` counts it, no bytes (plus the separator), so a response the
/// server's cap let commit is never refused here for it.
pub(crate) fn response(
    op: &Op,
    results: Vec<Option<String>>,
    budget: usize,
) -> Result<String, QueryFailure> {
    let mut count = ResponseBudget::new(budget);
    for r in &results {
        count.take(r.as_ref().map_or(0, String::len) + 1)?;
    }
    let results = results
        .into_iter()
        .map(|r| r.unwrap_or_else(|| "null".into()))
        .collect();
    Ok(join_results(op, results))
}

/// Runs the sequential path for `attempt`'s outcome when it needs it, once.
pub(crate) async fn settle(
    cache: &QueryCache,
    key: (Uuid, &str),
    attempt: Attempt,
    sequential: impl AsyncFnOnce() -> Result<String, QueryFailure>,
) -> Result<String, QueryFailure> {
    match attempt {
        Attempt::Done(out) => out,
        Attempt::Sequential => sequential().await,
        Attempt::Stale => {
            cache.versions.invalidate(&(key.0, key.1.to_owned()));
            sequential().await
        }
        Attempt::NoGuard => {
            tracing::info!(
                branch = %key.0,
                database = key.1,
                "the guard functions are missing; the data API runs this database sequentially for a while"
            );
            cache.no_guard.insert((key.0, key.1.to_owned()), ());
            sequential().await
        }
    }
}

/// One parameter value boxed for the driver, owned or borrowed.
pub(crate) type Param<'a> = Box<dyn ToSql + Sync + Send + 'a>;

/// One statement of the pipeline: its SQL, its declared types and its values.
pub(crate) struct Call<'a> {
    pub sql: &'a str,
    pub types: Vec<Type>,
    pub values: Vec<Param<'a>>,
}

/// A value already encoded for its declared type (`None`: NULL): it can't fail inside the
/// pipeline.
#[derive(Debug)]
pub(crate) struct Encoded(Option<Vec<u8>>);

impl ToSql for Encoded {
    fn to_sql(
        &self,
        _: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        Ok(match &self.0 {
            Some(bytes) => {
                out.extend_from_slice(bytes);
                IsNull::No
            }
            None => IsNull::Yes,
        })
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    to_sql_checked!();
}

/// `call`'s values encoded for its types: a 500 (a bug here, as the driver's encode error is
/// on the sequential path) when one doesn't fit or the counts differ.
pub(crate) fn encode(call: &Call<'_>, request_id: &str) -> Result<Vec<Encoded>, QueryFailure> {
    if call.values.len() != call.types.len() {
        return Err(QueryFailure::internal(
            request_id,
            format!(
                "{} values for {} parameters: {}",
                call.values.len(),
                call.types.len(),
                call.sql
            ),
        ));
    }
    let mut buf = BytesMut::new();
    call.values
        .iter()
        .zip(&call.types)
        .map(|(value, ty)| {
            buf.clear();
            match value.to_sql_checked(ty, &mut buf) {
                Ok(IsNull::Yes) => Ok(Encoded(None)),
                Ok(IsNull::No) => Ok(Encoded(Some(buf.to_vec()))),
                Err(e) => Err(QueryFailure::internal(
                    request_id,
                    format!("a value doesn't encode as {ty}: {e}"),
                )),
            }
        })
        .collect()
}

/// One pipelined attempt: encode, prepare, then `[ROLLBACK;] BEGIN; <reset_sql>`, `preamble`,
/// `plans`, `COMMIT` in one pipeline.
pub(crate) async fn attempt<C: QueryClient + ?Sized>(
    client: &C,
    reset_sql: &str,
    preamble: Call<'_>,
    plans: Vec<Call<'_>>,
    op: &Op,
    ctx: &Ctx<'_>,
) -> Attempt {
    let calls: Vec<&Call<'_>> = std::iter::once(&preamble).chain(&plans).collect();

    // 1. Values, encoded: nothing is sent before they all are.
    let mut encoded = Vec::with_capacity(calls.len());
    for call in &calls {
        match encode(call, ctx.request_id) {
            Ok(values) => encoded.push(values),
            Err(f) => return Attempt::Done(Err(f)),
        }
    }

    // 2. Every statement prepared (no I/O when the connection has it cached), before any
    // step of the pipeline exists: a prepare still in flight can't follow the COMMIT. Each
    // prepare runs to its end even when another fails: one dropped after its Parse went out
    // would leave a named statement on the server that nothing closes.
    let prepared = futures_util::future::join_all(
        calls
            .iter()
            .map(|call| client.prepare_cached(call.sql, &call.types)),
    )
    .await;
    let mut statements = Vec::with_capacity(prepared.len());
    for p in prepared {
        match p {
            Ok(statement) => statements.push(statement),
            Err(e) => {
                tracing::debug!(
                    request_id = ctx.request_id,
                    sqlstate = sqlstate(&e),
                    "the pipeline falls back to the sequential path: a statement didn't prepare: {}",
                    e.describe()
                );
                return prepare_failed(&e);
            }
        }
    }
    if let Err(f) = params_fit(
        statements.iter().map(|s| s.params().len()),
        &encoded,
        ctx.request_id,
    ) {
        return Attempt::Done(Err(f));
    }

    // 3. The pipeline, created in order and polled in order. After `COMMIT`, the reset
    // again: the session is clean when the next request prepares its statements (they
    // bind functions and operators by the session's `search_path` at Parse, and the pool
    // caches them).
    let refs: Vec<Vec<&(dyn ToSql + Sync)>> = encoded
        .iter()
        .map(|values| values.iter().map(|v| v as &(dyn ToSql + Sync)).collect())
        .collect();
    let c = client.client();
    let begin = begin_sql(reset_sql, client.transaction_status());
    let begin = &begin;
    type Out = Result<Option<Row>, tokio_postgres::Error>;
    let mut steps: Vec<Step<'_, Out>> = Vec::with_capacity(statements.len() + 3);
    steps.push(Box::pin(async move {
        c.batch_execute(begin).await.map(|()| None)
    }));
    for (statement, values) in statements.iter().zip(&refs) {
        steps.push(Box::pin(async move {
            c.query_one(statement, values).await.map(Some)
        }));
    }
    steps.push(Box::pin(async move {
        c.batch_execute("COMMIT").await.map(|()| None)
    }));
    steps.push(Box::pin(async move {
        c.batch_execute(reset_sql).await.map(|()| None)
    }));
    let mut results = join_in_order(steps).await;
    ctx.runs.fetch_add(1, Ordering::Relaxed);
    tracing::debug!(request_id = ctx.request_id, "the request ran pipelined");
    take_trailing_reset(&mut results, client, ctx.request_id);
    let len = results.len();
    let rows = match first_failure(results) {
        Ok(rows) => rows,
        Err((index, e)) => {
            let outcome = step_failed(step_kind(index, len), &e, ctx);
            if matches!(outcome, Attempt::Stale | Attempt::NoGuard) {
                tracing::debug!(
                    request_id = ctx.request_id,
                    sqlstate = sqlstate(&e),
                    "the pipeline falls back to the sequential path: {outcome:?}"
                );
            }
            return outcome;
        }
    };
    Attempt::Done(finish(rows, op, ctx))
}

/// The SQLSTATE of a step's error, for the log (`none` when the server sent none).
fn sqlstate<E: StepError>(e: &E) -> &str {
    e.db().map_or("none", |db| db.code())
}

/// The committed pipeline's answer: the owner checked, each plan's text (`null` for a
/// NULL), counted again and joined.
fn finish(rows: Vec<Option<Row>>, op: &Op, ctx: &Ctx<'_>) -> Result<String, QueryFailure> {
    let mut rows = rows.into_iter().skip(1);
    let preamble = rows.next().flatten();
    if let (RunAs::Owner { user }, Some(row)) = (ctx.run_as, &preamble) {
        let current: String = row
            .try_get(2)
            .map_err(|e| QueryFailure::from_pg(&e, ctx.request_id))?;
        if current != user {
            return Err(QueryFailure::internal(
                ctx.request_id,
                format!("the owner's connection runs as {current}, not {user}"),
            ));
        }
    }
    let mut results = Vec::new();
    for row in rows {
        // The COMMIT's `None` ends the plans.
        let Some(row) = row else { break };
        let out = row
            .try_get::<_, Option<String>>(0)
            .map_err(|e| QueryFailure::from_pg(&e, ctx.request_id))?;
        results.push(out);
    }
    response(op, results, ctx.budget)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use http::StatusCode;
    use tokio_postgres::types::Type;
    use uuid::Uuid;

    use super::*;
    use crate::engine::{QueryCache, ResponseBudget, RunAs};
    use crate::error::tests::{FakeDb, closed_client};
    use crate::error::{DbFields, QueryFailure, schema_missing_in};

    /// A Postgres error as a pipeline step returns it.
    struct FakeErr(FakeDb);

    impl StepError for FakeErr {
        fn db(&self) -> Option<&dyn DbFields> {
            Some(&self.0)
        }
        fn failure(&self, request_id: &str) -> QueryFailure {
            QueryFailure::from_db(&self.0, request_id)
        }
    }

    fn err(code: &'static str, message: &'static str) -> Result<(), FakeErr> {
        Err(FakeErr(FakeDb::new(code, message)))
    }

    const ABORTED: &str =
        "current transaction is aborted, commands ignored until end of transaction block";

    fn ctx(run_as: RunAs<'_>, budget: usize) -> Ctx<'_> {
        static RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        Ctx {
            run_as,
            database: "main",
            budget,
            request_id: "rid-1",
            runs: &RUNS,
        }
    }

    fn role_ctx(budget: usize) -> Ctx<'static> {
        static CLAIMS: std::sync::LazyLock<serde_json::Value> =
            std::sync::LazyLock::new(|| serde_json::json!({"role": "anon"}));
        ctx(
            RunAs::Role {
                role: "anon",
                claims: &CLAIMS,
            },
            budget,
        )
    }

    fn done_err(a: Attempt) -> QueryFailure {
        match a {
            Attempt::Done(Err(f)) => f,
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    /// The pipeline's futures are polled, the first time, in the order they were pushed.
    #[tokio::test]
    async fn join_in_order_polls_in_push_order() {
        let order = std::sync::Mutex::new(Vec::new());
        let steps: Vec<Step<'_, usize>> = (0..40_usize)
            .map(|i| {
                let order = &order;
                Box::pin(async move {
                    order.lock().unwrap().push(i);
                    tokio::task::yield_now().await;
                    i
                }) as Step<'_, usize>
            })
            .collect();
        let results = join_in_order(steps).await;
        assert_eq!(results, (0..40).collect::<Vec<_>>());
        assert_eq!(*order.lock().unwrap(), (0..40).collect::<Vec<_>>());
    }

    /// The first real error is the request's; the `25P02`s after it (every later step of the
    /// aborted transaction) are ignored, and so is the `COMMIT` that became a rollback.
    #[test]
    fn first_error_wins_and_aborted_steps_are_ignored() {
        let results = vec![
            Ok(()),
            Ok(()),
            err(
                "23505",
                "duplicate key value violates unique constraint \"t_pkey\"",
            ),
            err("25P02", ABORTED),
            err("25P02", ABORTED),
            Ok(()),
        ];
        let (index, e) = first_failure(results).unwrap_err();
        assert_eq!(index, 2);
        let f = done_err(step_failed(StepKind::Plan, &e, &role_ctx(100)));
        assert_eq!((f.status, f.code), (StatusCode::CONFLICT, "conflict"));
        assert_eq!(f.sqlstate.as_deref(), Some("23505"));

        // Only aborted steps (no cause seen): the first of them.
        let (index, _) =
            first_failure(vec![Ok(()), err("25P02", ABORTED), err("25P02", ABORTED)]).unwrap_err();
        assert_eq!(index, 1);
        // No error: every step's output, in order.
        assert_eq!(
            first_failure::<_, FakeErr>(vec![Ok(1), Ok(2), Ok(3)]).ok(),
            Some(vec![1, 2, 3])
        );

        // The preamble's failures map as the sequential path maps them.
        let e = FakeErr(FakeDb::new(
            "42501",
            "permission denied to set role \"anon\"",
        ));
        let f = done_err(step_failed(StepKind::Preamble, &e, &role_ctx(100)));
        assert_eq!(f.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            f.message,
            "the key's role anon can't be used right now; try again"
        );
        let e = FakeErr(FakeDb::new(
            "42P01",
            "relation \"chelabase.schema_version\" does not exist",
        ));
        let owner = ctx(RunAs::Owner { user: "owner" }, 100);
        let f = done_err(step_failed(StepKind::Preamble, &e, &owner));
        assert_eq!(f.message, schema_missing_in("main"));
        let f = done_err(step_failed(StepKind::Preamble, &e, &role_ctx(100)));
        assert_eq!(f.message, schema_missing_in("main"));
    }

    /// A stale version (`CB001`) drops the cached version and runs the sequential path once,
    /// whose answer (a failure included) is the request's.
    #[tokio::test]
    async fn stale_version_falls_back_to_sequential_once() {
        let e = FakeErr(FakeDb::new("CB001", "chelabase: stale schema version"));
        let attempt = step_failed(StepKind::Preamble, &e, &role_ctx(100));
        assert!(matches!(attempt, Attempt::Stale), "{attempt:?}");

        let cache = QueryCache::default();
        let branch = Uuid::new_v4();
        cache.versions.insert((branch, "main".into()), "v1".into());
        cache.versions.insert((branch, "shop".into()), "v1".into());
        let runs = AtomicUsize::new(0);
        let out = settle(&cache, (branch, "main"), attempt, async || {
            runs.fetch_add(1, Ordering::Relaxed);
            Err(QueryFailure::invalid_query("from the sequential path"))
        })
        .await;
        assert_eq!(out.unwrap_err().message, "from the sequential path");
        assert_eq!(runs.load(Ordering::Relaxed), 1);
        assert_eq!(cache.versions.get(&(branch, "main".into())), None);
        assert_eq!(
            cache.versions.get(&(branch, "shop".into())).as_deref(),
            Some("v1"),
            "only that database's version is dropped"
        );
        assert!(!cache.no_guard.contains_key(&(branch, "main".into())));

        // A finished attempt never runs it; `Sequential` runs it once.
        let out = settle(
            &cache,
            (branch, "main"),
            Attempt::Done(Ok("1".into())),
            async || {
                runs.fetch_add(1, Ordering::Relaxed);
                Ok("2".into())
            },
        )
        .await;
        assert_eq!(out.unwrap(), "1");
        let out = settle(&cache, (branch, "main"), Attempt::Sequential, async || {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok("2".into())
        })
        .await;
        assert_eq!(out.unwrap(), "2");
        assert_eq!(runs.load(Ordering::Relaxed), 2);
    }

    /// A database without the guard functions (`42883` naming one, when preparing or
    /// running) is marked and served by the sequential path; any other missing function is
    /// the request's own error, mapped as the sequential path maps it.
    #[tokio::test]
    async fn missing_guard_marks_the_database_and_runs_sequentially() {
        let assert_missing = FakeErr(FakeDb::new(
            "42883",
            "function chelabase.assert_schema_version(uuid) does not exist",
        ));
        let cap_missing = FakeErr(FakeDb::new(
            "42883",
            "function chelabase.cap_response(text, bigint) does not exist",
        ));
        for e in [&assert_missing, &cap_missing] {
            assert!(matches!(prepare_failed(e), Attempt::NoGuard));
            assert!(matches!(
                step_failed(StepKind::Plan, e, &role_ctx(100)),
                Attempt::NoGuard
            ));
            assert!(matches!(
                step_failed(StepKind::Preamble, e, &role_ctx(100)),
                Attempt::NoGuard
            ));
        }
        // Any other prepare failure: the sequential path answers (and maps it).
        let other = FakeErr(FakeDb::new("42P01", "relation \"notes\" does not exist"));
        assert!(matches!(prepare_failed(&other), Attempt::Sequential));
        // A user's function missing while running: a 500, as sequentially.
        let rpc = FakeErr(FakeDb::new(
            "42883",
            "function public.f(integer) does not exist",
        ));
        let f = done_err(step_failed(StepKind::Plan, &rpc, &role_ctx(100)));
        assert_eq!(f.status, StatusCode::INTERNAL_SERVER_ERROR);

        let cache = QueryCache::default();
        let branch = Uuid::new_v4();
        cache.versions.insert((branch, "main".into()), "v1".into());
        let runs = AtomicUsize::new(0);
        let out = settle(&cache, (branch, "main"), Attempt::NoGuard, async || {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok("[]".into())
        })
        .await;
        assert_eq!(out.unwrap(), "[]");
        assert_eq!(runs.load(Ordering::Relaxed), 1);
        assert!(cache.no_guard.contains_key(&(branch, "main".into())));
        assert!(!cache.no_guard.contains_key(&(branch, "shop".into())));
        cache.invalidate_branch(branch);
        assert!(!cache.no_guard.contains_key(&(branch, "main".into())));
        assert_eq!(cache.versions.get(&(branch, "main".into())), None);
    }

    /// The cap in SQL (`CB002`) is the sequential path's 413, word for word.
    #[test]
    fn cap_error_is_413() {
        let e = FakeErr(FakeDb::new("CB002", "chelabase: response too large"));
        for budget in [10, 16 * 1024 * 1024] {
            let f = done_err(step_failed(StepKind::Plan, &e, &role_ctx(budget)));
            assert_eq!(
                f,
                ResponseBudget::new(budget).take(budget + 1).unwrap_err(),
                "{budget}"
            );
            assert_eq!(
                (f.status, f.code),
                (StatusCode::PAYLOAD_TOO_LARGE, "too_large")
            );
        }
    }

    /// Rust re-counts what it read (each result and a separator, as the sequential path
    /// does): past the budget it's the same 413, whatever the SQL counter said.
    #[test]
    fn rust_backstop_refuses_past_the_budget() {
        let select = crate::parse(br#"{"v":1,"op":"select","from":"t"}"#)
            .unwrap()
            .op;
        let some = |s: &str| Some(s.to_owned());
        assert_eq!(response(&select, vec![some("12345")], 6).unwrap(), "12345");
        let f = response(&select, vec![some("12345")], 5).unwrap_err();
        assert_eq!(f, ResponseBudget::new(5).take(6).unwrap_err());

        let batch = batch_op();
        assert_eq!(
            response(&batch, vec![some("1"), some("2")], 4).unwrap(),
            "[1,2]"
        );
        assert_eq!(
            response(&batch, vec![some("1"), some("2")], 3)
                .unwrap_err()
                .status,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        // The SQL budget makes the server's count refuse exactly where this one does.
        assert_eq!(sql_budget(100, 3), 97);
        assert_eq!(sql_budget(2, 3), 0);
    }

    fn batch_op() -> crate::Op {
        crate::parse(
            br#"{"v":1,"op":"batch","ops":[{"op":"select","from":"t"},{"op":"select","from":"t"}]}"#,
        )
        .unwrap()
        .op
    }

    /// A NULL result (a `void` function) is `null` in the response but counts as the server
    /// counts it (no bytes, plus the separator): a request the cap let commit is never
    /// refused afterwards for it.
    #[test]
    fn a_null_result_counts_as_the_server_counts_it() {
        let batch = batch_op();
        // The server counted 1 byte for 2 plans: sql_budget(3, 2) = 1 lets it through.
        assert_eq!(sql_budget(3, 2), 1);
        assert_eq!(
            response(&batch, vec![Some("1".into()), None], 3).unwrap(),
            "[1,null]"
        );
        assert_eq!(
            response(&batch, vec![Some("1".into()), None], 2)
                .unwrap_err()
                .status,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        let call = crate::parse(br#"{"v":1,"op":"select","from":"t"}"#)
            .unwrap()
            .op;
        assert_eq!(response(&call, vec![None], 1).unwrap(), "null");
    }

    /// The trailing reset (after `COMMIT`) never decides the response: its failure is
    /// logged and marks the connection broken, and the steps before it decide as before.
    #[tokio::test]
    async fn trailing_reset_failure_keeps_the_response_and_marks_the_connection() {
        let counting = Counting::new().await;
        let mut results = vec![Ok(()), Ok(()), Ok(()), Ok(()), err("08006", "lost")];
        take_trailing_reset(&mut results, &counting, "rid-1");
        assert!(counting.broken.load(Ordering::Relaxed));
        assert_eq!(results.len(), 4);
        assert!(first_failure(results).is_ok());

        // A plan's failure stays the request's.
        let counting = Counting::new().await;
        let mut results = vec![
            Ok(()),
            Ok(()),
            err("23505", "duplicate"),
            Ok(()),
            err("08006", "lost"),
        ];
        take_trailing_reset(&mut results, &counting, "rid-1");
        assert!(counting.broken.load(Ordering::Relaxed));
        let (index, e) = first_failure(results).unwrap_err();
        assert_eq!((index, e.0.code), (2, "23505"));

        // A clean reset marks nothing.
        let counting = Counting::new().await;
        let mut results: Vec<Result<(), FakeErr>> = vec![Ok(()), Ok(()), Ok(()), Ok(()), Ok(())];
        take_trailing_reset(&mut results, &counting, "rid-1");
        assert!(!counting.broken.load(Ordering::Relaxed));
        assert_eq!(results.len(), 4);
    }

    /// Each prepared statement must take exactly the values encoded for it, or nothing is
    /// sent (a 500).
    #[test]
    fn statements_must_take_every_value() {
        let values = |n: usize| (0..n).map(|_| Encoded(None)).collect::<Vec<_>>();
        assert!(params_fit([2, 3].into_iter(), &[values(2), values(3)], "rid-1").is_ok());
        let f = params_fit([2, 4].into_iter(), &[values(2), values(3)], "rid-1").unwrap_err();
        assert_eq!(f.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(params_fit([2].into_iter(), &[values(2), values(3)], "rid-1").is_err());
    }

    /// A client that counts what it's asked to do; its connection is closed, so anything it
    /// sent would fail.
    struct Counting {
        client: tokio_postgres::Client,
        prepares: AtomicUsize,
        uses: AtomicUsize,
        broken: std::sync::atomic::AtomicBool,
    }

    impl Counting {
        async fn new() -> Counting {
            Counting {
                client: closed_client().await,
                prepares: AtomicUsize::new(0),
                uses: AtomicUsize::new(0),
                broken: Default::default(),
            }
        }
    }

    impl QueryClient for Counting {
        fn mark_broken(&self) {
            self.broken.store(true, Ordering::Relaxed);
        }
        fn client(&self) -> &tokio_postgres::Client {
            self.uses.fetch_add(1, Ordering::Relaxed);
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
    }

    /// A connection goes back to the pool only when the call on it finished and its
    /// session was reset.
    #[test]
    fn a_lease_keeps_only_finished_clean_connections() {
        assert!(keeps(true, false));
        assert!(!keeps(true, true));
        assert!(!keeps(false, false));
        assert!(!keeps(false, true));
    }

    /// A pool of connections to a fake server that accepts every connection and keeps it
    /// open (nothing is ever queried).
    async fn fake_pool() -> deadpool_postgres::Pool {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let len = socket.read_i32().await.unwrap();
                    let mut startup = vec![0; usize::try_from(len).unwrap() - 4];
                    socket.read_exact(&mut startup).await.unwrap();
                    // AuthenticationOk, then ReadyForQuery (idle).
                    socket
                        .write_all(&[b'R', 0, 0, 0, 8, 0, 0, 0, 0, b'Z', 0, 0, 0, 5, b'I'])
                        .await
                        .unwrap();
                    let mut buf = [0; 1024];
                    while socket.read(&mut buf).await.is_ok_and(|n| n > 0) {}
                });
            }
        });
        let mut config = tokio_postgres::Config::new();
        config.host("127.0.0.1").port(port).user("x").dbname("x");
        let manager = deadpool_postgres::Manager::from_config(
            config,
            tokio_postgres::NoTls,
            deadpool_postgres::ManagerConfig {
                recycling_method: deadpool_postgres::RecyclingMethod::Fast,
            },
        );
        deadpool_postgres::Pool::builder(manager)
            .max_size(4)
            .runtime(deadpool_postgres::Runtime::Tokio1)
            .build()
            .unwrap()
    }

    /// Finished and clean: back to the pool. Broken, never finished (an error before
    /// `done`, a panic) or cancelled mid-call: taken out of the pool (closed).
    #[tokio::test]
    async fn a_lease_returns_or_discards_the_connection() {
        let pool = fake_pool().await;

        let mut lease = Lease::new(pool.get().await.unwrap());
        assert!(!lease.client().is_closed());
        lease.done();
        drop(lease);
        assert_eq!(pool.status().size, 1, "kept");

        let mut lease = Lease::new(pool.get().await.unwrap());
        lease.mark_broken();
        assert!(lease.is_broken());
        lease.done();
        drop(lease);
        assert_eq!(pool.status().size, 0, "broken: taken");

        drop(Lease::new(pool.get().await.unwrap()));
        assert_eq!(pool.status().size, 0, "never finished: taken");

        let object = pool.get().await.unwrap();
        let cancelled = tokio::time::timeout(std::time::Duration::from_millis(10), async {
            let mut lease = Lease::new(object);
            std::future::pending::<()>().await;
            lease.done();
        })
        .await;
        assert!(cancelled.is_err());
        assert_eq!(pool.status().size, 0, "cancelled: taken");
    }

    /// A client whose prepares of `"slow"` finish (counted) only after a few polls; every
    /// prepare fails (its connection is closed).
    struct SlowPrepares {
        client: tokio_postgres::Client,
        finished: AtomicUsize,
    }

    impl QueryClient for SlowPrepares {
        fn mark_broken(&self) {}
        fn client(&self) -> &tokio_postgres::Client {
            &self.client
        }
        async fn prepare_cached(
            &self,
            sql: &str,
            types: &[Type],
        ) -> Result<tokio_postgres::Statement, tokio_postgres::Error> {
            if sql == "slow" {
                for _ in 0..5 {
                    tokio::task::yield_now().await;
                }
                self.finished.fetch_add(1, Ordering::Relaxed);
            }
            self.client.prepare_typed(sql, types).await
        }
    }

    /// A prepare that fails doesn't cancel the others: every prepare runs to its end (a
    /// prepare dropped half-way would leave its named statement on the server, never
    /// closed) before the request falls back to the sequential path; nothing is counted as
    /// pipelined.
    #[tokio::test]
    async fn every_prepare_finishes_before_the_fallback() {
        let client = SlowPrepares {
            client: closed_client().await,
            finished: AtomicUsize::new(0),
        };
        let select = crate::parse(br#"{"v":1,"op":"select","from":"t"}"#)
            .unwrap()
            .op;
        let call = |sql: &'static str| Call {
            sql,
            types: vec![],
            values: vec![],
        };
        let runs = std::sync::atomic::AtomicU64::new(0);
        let mut ctx = role_ctx(100);
        ctx.runs = &runs;
        let out = attempt(
            &client,
            "RESET ALL",
            call("fast"),
            vec![call("slow"), call("slow")],
            &select,
            &ctx,
        )
        .await;
        assert!(matches!(out, Attempt::Sequential), "{out:?}");
        assert_eq!(client.finished.load(Ordering::Relaxed), 2);
        assert_eq!(runs.load(Ordering::Relaxed), 0);
    }

    /// A connection's statement cache stays bounded: at [`STATEMENT_CACHE_CAP`] statements
    /// it's cleared before the next prepare.
    #[tokio::test]
    async fn the_statement_cache_is_cleared_at_the_cap() {
        let pool = crate::fake_pg::pool(1).await;
        let lease = Lease::new(pool.get().await.unwrap());
        let size = || lease.wrapper().statement_cache.size();
        for i in 0..STATEMENT_CACHE_CAP {
            lease
                .prepare_cached(&format!("SELECT {i}"), &[Type::INT8])
                .await
                .unwrap();
        }
        assert_eq!(size(), STATEMENT_CACHE_CAP);
        let past = lease
            .prepare_cached("SELECT 'one more'", &[])
            .await
            .unwrap();
        assert_eq!(size(), 1, "cleared, then the new one cached");
        assert_eq!(past.params(), &[] as &[Type]);
        // The connection still serves.
        lease.client().batch_execute("SELECT 1").await.unwrap();
    }

    /// A value that doesn't encode as its declared type fails the request before anything
    /// is prepared or sent: no `COMMIT` can follow a step that never went out.
    #[tokio::test]
    async fn encode_error_sends_nothing() {
        let counting = Counting::new().await;
        let select = crate::parse(br#"{"v":1,"op":"select","from":"t"}"#)
            .unwrap()
            .op;
        let preamble = Call {
            sql: "SELECT $1",
            types: vec![Type::TEXT],
            values: vec![Box::new("5000".to_owned())],
        };
        let plan = Call {
            sql: "SELECT $1",
            types: vec![Type::TEXT],
            values: vec![Box::new(5_i64)],
        };
        let attempt = attempt(
            &counting,
            "RESET ALL",
            preamble,
            vec![plan],
            &select,
            &role_ctx(100),
        )
        .await;
        let f = done_err(attempt);
        assert_eq!(f.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(counting.prepares.load(Ordering::Relaxed), 0);
        assert_eq!(counting.uses.load(Ordering::Relaxed), 0);

        // The values and types must also line up one for one.
        let short = Call {
            sql: "SELECT $1, $2",
            types: vec![Type::TEXT, Type::TEXT],
            values: vec![Box::new("a".to_owned())],
        };
        assert_eq!(
            encode(&short, "rid-1").unwrap_err().status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
        let ok = Call {
            sql: "SELECT $1, $2",
            types: vec![Type::TEXT, Type::INT8],
            values: vec![Box::new(None::<String>), Box::new(7_i64)],
        };
        assert_eq!(encode(&ok, "rid-1").unwrap().len(), 2);
    }
}
