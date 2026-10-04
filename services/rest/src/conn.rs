//! Where each branch's database is, and a pool of connections to it.
//!
//! Connection info comes from the orchestrator's internal listener and is cached 60 s (or
//! until [`Pools::invalidate`]). Each served database of a branch gets its own sqlx pool
//! (dropped after 5 minutes unused); every pool of a branch waits on one set of branch
//! slots (`max_connections`), and one global semaphore caps connections across every
//! branch. Open connections, idle ones included, stay within `max_connections` per branch
//! too: before a pool may open one, idle connections of the branch's other pools are closed,
//! least recently used first ([`idle_to_close`]). A connection that fails to open (the compute moved after a reset or a restore,
//! or its password changed) drops the cached info, fetches it again and retries once;
//! nothing is retried once a statement was sent.

use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use sqlx::pool::PoolConnection;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions, PgSslMode};
use sqlx::{Executor, Postgres};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

/// How long connection info is trusted.
pub const INFO_TTL: Duration = Duration::from_secs(60);
/// A request naming a database the cached info doesn't serve fetches the info again when it
/// is at least this old (a database enabled since, without an invalidate reaching rest).
pub const INFO_REFRESH_AFTER: Duration = Duration::from_secs(2);
/// A branch's pool is dropped after this long unused.
pub const POOL_IDLE: Duration = Duration::from_secs(5 * 60);
/// Idle connections inside a pool are closed after this long.
pub const CONNECTION_IDLE: Duration = Duration::from_secs(60);
/// The longest a request waits for a connection (or a global permit).
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest a request waits for a connection in all (slots, first try and retry).
pub const ACQUIRE_DEADLINE: Duration = Duration::from_secs(6);
/// The first try for a connection, before the branch's info is fetched again.
pub const FIRST_ATTEMPT: Duration = Duration::from_secs(1);
/// Prepared statements kept per connection (the SQL text is the key).
pub const STATEMENT_CACHE: usize = 500;
/// Most databases with a live pool.
const MAX_POOLS: u64 = 10_000;
/// Run on every connection handed back to its pool: a function the request called may have
/// changed the session (`SET ROLE`, `set_config(…, false)`), and the next request must start
/// as `authenticator` with the connection's defaults. Prepared statements survive it.
pub const RESET_SQL: &str = "SET ROLE NONE; RESET ALL";

/// The branch's data API settings (project-wide), as the internal listener sends them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Settings {
    pub max_rows: i64,
    pub statement_timeout_ms: i64,
    pub max_connections: u32,
}

/// A database the branch's data API serves.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ServedDatabase {
    pub name: String,
    /// What an unqualified name means in this database.
    pub default_schema: String,
}

/// `GET /internal/data/branches/{id}/connection`.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(from = "WireConnInfo")]
pub struct ConnInfo {
    pub host: String,
    pub port: u16,
    /// The main database's name.
    pub database: String,
    pub user: String,
    pub password: String,
    pub settings: Settings,
    /// The served databases, main first.
    pub databases: Vec<ServedDatabase>,
}

impl ConnInfo {
    /// The served database named `name`.
    pub fn served(&self, name: &str) -> Option<&ServedDatabase> {
        self.databases.iter().find(|d| d.name == name)
    }

    /// Whether a pool built for `self` still fits `other` (the database is the pool's key).
    fn same_server(&self, other: &ConnInfo) -> bool {
        (
            &self.host,
            self.port,
            &self.user,
            &self.password,
            self.settings.max_connections,
        ) == (
            &other.host,
            other.port,
            &other.user,
            &other.password,
            other.settings.max_connections,
        )
    }
}

/// The answer as sent. Before served databases existed it had no `databases` and a
/// `settings.default_schema` for main; both shapes are read.
#[derive(Deserialize)]
struct WireConnInfo {
    host: String,
    port: u16,
    database: String,
    user: String,
    password: String,
    settings: WireSettings,
    #[serde(default)]
    databases: Option<Vec<ServedDatabase>>,
}

#[derive(Deserialize)]
struct WireSettings {
    max_rows: i64,
    statement_timeout_ms: i64,
    max_connections: u32,
    #[serde(default)]
    default_schema: Option<String>,
}

impl From<WireConnInfo> for ConnInfo {
    fn from(w: WireConnInfo) -> Self {
        let databases = w.databases.unwrap_or_else(|| {
            vec![ServedDatabase {
                name: w.database.clone(),
                default_schema: w
                    .settings
                    .default_schema
                    .unwrap_or_else(|| "public".to_owned()),
            }]
        });
        ConnInfo {
            host: w.host,
            port: w.port,
            database: w.database,
            user: w.user,
            password: w.password,
            settings: Settings {
                max_rows: w.settings.max_rows,
                statement_timeout_ms: w.settings.statement_timeout_ms,
                max_connections: w.settings.max_connections,
            },
            databases,
        }
    }
}

impl fmt::Debug for ConnInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnInfo")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("settings", &self.settings)
            .field("databases", &self.databases)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnError {
    /// The branch doesn't exist or is being deleted (404).
    #[error("the branch doesn't exist")]
    UnknownBranch,
    /// The branch isn't ready, or its compute has no address yet (409); the branch status.
    #[error("the branch is {0}")]
    NotReady(String),
    /// The internal listener can't be asked.
    #[error("connection info unavailable: {0}")]
    Unreachable(String),
}

/// Fetches a branch's connection info.
pub trait ConnSource: Send + Sync + 'static {
    fn fetch(&self, branch: Uuid) -> impl Future<Output = Result<ConnInfo, ConnError>> + Send;
}

/// The orchestrator's internal listener.
pub struct InternalSource {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl InternalSource {
    pub fn new(base_url: &str, token: &str) -> Self {
        InternalSource {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("an HTTP client"),
            base: base_url.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
        }
    }
}

impl ConnSource for InternalSource {
    async fn fetch(&self, branch: Uuid) -> Result<ConnInfo, ConnError> {
        let url = format!("{}/internal/data/branches/{branch}/connection", self.base);
        let response = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| ConnError::Unreachable(e.to_string()))?;
        match response.status().as_u16() {
            200 => response
                .json()
                .await
                .map_err(|e| ConnError::Unreachable(format!("reading the answer: {e}"))),
            404 => Err(ConnError::UnknownBranch),
            409 => {
                #[derive(Deserialize)]
                struct Status {
                    status: String,
                }
                let status = response
                    .json::<Status>()
                    .await
                    .map_or_else(|_| "not ready".to_owned(), |s| s.status);
                Err(ConnError::NotReady(status))
            }
            status => Err(ConnError::Unreachable(format!(
                "the internal listener answered {status}"
            ))),
        }
    }
}

/// Why no connection could be had.
#[derive(Debug, thiserror::Error)]
pub enum AcquireError {
    #[error(transparent)]
    Conn(#[from] ConnError),
    /// Every global permit is taken.
    #[error("too many connections in use")]
    Busy,
    /// No connection within [`ACQUIRE_DEADLINE`].
    #[error("timed out waiting for a connection")]
    TimedOut,
    /// The database stopped being served (the info fetched again for a retry says so).
    #[error("the database isn't served")]
    NotServed,
    #[error(transparent)]
    Db(sqlx::Error),
}

/// A connection, holding its branch and global slots until it's dropped.
pub struct Conn {
    pub conn: PoolConnection<Postgres>,
    pub info: Arc<ConnInfo>,
    _permits: Permits,
}

/// A branch slot and a global slot, held together for the life of a connection.
pub struct Permits {
    _branch: OwnedSemaphorePermit,
    _global: OwnedSemaphorePermit,
}

/// One served database's pool.
struct DbPool {
    info: Arc<ConnInfo>,
    pool: PgPool,
    /// When a request last took a connection from it.
    last_used: std::sync::Mutex<tokio::time::Instant>,
}

impl DbPool {
    fn load(&self) -> PoolLoad {
        PoolLoad {
            size: self.pool.size(),
            idle: self.pool.num_idle(),
            last_used: *self.last_used.lock().expect("last_used"),
        }
    }
}

/// A branch's slots, shared by all its databases' pools: `max_connections` of them, and the
/// pools themselves (for the bound on open connections).
struct BranchSlots {
    /// The semaphore's current size; it's resized in place, so permits held across a change
    /// still count.
    size: std::sync::Mutex<usize>,
    slots: Arc<Semaphore>,
    pools: std::sync::Mutex<Vec<std::sync::Weak<DbPool>>>,
}

impl BranchSlots {
    fn new(size: usize) -> Self {
        BranchSlots {
            size: std::sync::Mutex::new(size),
            slots: Arc::new(Semaphore::new(size)),
            pools: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Grows by adding permits, shrinks by forgetting free ones; permits still held past the
    /// new size are forgotten as they come back.
    fn resize(&self, size: usize) {
        let mut current = self.size.lock().expect("slots size");
        if size > *current {
            self.slots.add_permits(size - *current);
        } else if size < *current {
            let excess = *current - size;
            let owed = excess - self.slots.forget_permits(excess);
            if owed > 0 {
                let slots = self.slots.clone();
                tokio::spawn(async move {
                    if let Ok(permits) = slots.acquire_many_owned(owed as u32).await {
                        permits.forget();
                    }
                });
            }
        }
        *current = size;
    }

    /// Remembers `pool` as one of the branch's (once), dropping pools that are gone.
    fn register(&self, pool: &Arc<DbPool>) {
        let mut pools = self.pools.lock().expect("branch pools");
        pools.retain(|p| p.strong_count() > 0);
        if !pools.iter().any(|p| std::ptr::eq(p.as_ptr(), Arc::as_ptr(pool))) {
            pools.push(Arc::downgrade(pool));
        }
    }

    /// The branch's live pools other than `target`.
    fn others(&self, target: &Arc<DbPool>) -> Vec<Arc<DbPool>> {
        self.pools
            .lock()
            .expect("branch pools")
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .filter(|p| !Arc::ptr_eq(p, target) && !p.pool.is_closed())
            .collect()
    }
}

/// A pool's connections as the bound sees them.
#[derive(Debug, Clone, Copy)]
struct PoolLoad {
    /// Open connections, idle and in use.
    size: u32,
    idle: usize,
    last_used: tokio::time::Instant,
}

/// Before `target` may open a connection: how many idle connections to close in each of
/// `others` (by index), least recently used pool first, so the branch's open connections
/// stay within `max` once it opens one. Nothing when `target` has an idle connection to
/// reuse, or there's room. Held connections can't pass `max` (the branch slots), so when the
/// total is at `max` and `target` has none idle, the others hold enough idle ones.
fn idle_to_close(target: &PoolLoad, others: &[PoolLoad], max: u32) -> Vec<(usize, usize)> {
    if target.idle > 0 {
        return Vec::new();
    }
    let total: u64 = u64::from(target.size) + others.iter().map(|p| u64::from(p.size)).sum::<u64>();
    let mut excess = (total + 1).saturating_sub(u64::from(max)) as usize;
    let mut order: Vec<usize> = (0..others.len()).collect();
    order.sort_by_key(|&i| others[i].last_used);
    let mut plan = Vec::new();
    for i in order {
        if excess == 0 {
            break;
        }
        let n = others[i].idle.min(excess);
        if n > 0 {
            plan.push((i, n));
            excess -= n;
        }
    }
    plan
}

pub struct Pools<S> {
    source: S,
    /// Each branch's info and when it was fetched.
    infos: moka::future::Cache<Uuid, (Arc<ConnInfo>, tokio::time::Instant)>,
    pools: moka::future::Cache<(Uuid, String), Arc<DbPool>>,
    slots: moka::future::Cache<Uuid, Arc<BranchSlots>>,
    permits: Arc<Semaphore>,
}

/// Failing to open a connection, before any statement: worth a fresh look at where the
/// compute is. `at_size`: the pool already holds all its connections.
fn is_connect_error(e: &sqlx::Error, at_size: bool) -> bool {
    match e {
        sqlx::Error::Io(_) | sqlx::Error::Tls(_) => true,
        // sqlx retries a refused connection until it times out; a pool whose connections
        // are all open timed out waiting for one to come back.
        sqlx::Error::PoolTimedOut => !at_size,
        // An invalidate closed the pool under the request: the next try builds a new one.
        sqlx::Error::PoolClosed => true,
        sqlx::Error::Database(db) => db
            .code()
            .is_some_and(|c| c.starts_with("28") || c.starts_with("08") || c.starts_with("57P")),
        _ => false,
    }
}

impl<S: ConnSource> Pools<S> {
    pub fn new(source: S, max_connections: usize) -> Self {
        Pools {
            source,
            infos: moka::future::Cache::builder()
                .max_capacity(MAX_POOLS)
                .time_to_live(INFO_TTL)
                .build(),
            pools: moka::future::Cache::builder()
                .max_capacity(MAX_POOLS)
                .time_to_idle(POOL_IDLE)
                .build(),
            slots: moka::future::Cache::builder()
                .max_capacity(MAX_POOLS)
                .time_to_idle(POOL_IDLE)
                .build(),
            permits: Arc::new(Semaphore::new(max_connections)),
        }
    }

    /// The branch's connection info (cached; concurrent misses share one fetch).
    pub async fn info(&self, branch: Uuid) -> Result<Arc<ConnInfo>, ConnError> {
        self.infos
            .try_get_with(branch, async {
                self.source
                    .fetch(branch)
                    .await
                    .map(|info| (Arc::new(info), tokio::time::Instant::now()))
            })
            .await
            .map(|(info, _)| info)
            .map_err(|e| (*e).clone())
    }

    /// The branch's info fetched again when the cached one is [`INFO_REFRESH_AFTER`] old or
    /// older; `None` when it's newer (or there's none: the next [`Self::info`] fetches).
    pub async fn refresh_stale(&self, branch: Uuid) -> Result<Option<Arc<ConnInfo>>, ConnError> {
        match self.infos.get(&branch).await {
            Some((_, fetched)) if fetched.elapsed() >= INFO_REFRESH_AFTER => {
                self.infos.invalidate(&branch).await;
                self.info(branch).await.map(Some)
            }
            _ => Ok(None),
        }
    }

    /// Forgets what's held for `branch` at once (a database disabled, a setting changed): its
    /// info and its pools (closed once their connections come back). Its slots stay, so
    /// connections still held count against the branch; the next info resizes them.
    pub async fn invalidate(&self, branch: Uuid) {
        self.infos.invalidate(&branch).await;
        let keys: Vec<_> = self
            .pools
            .iter()
            .filter(|(k, _)| k.0 == branch)
            .map(|(k, _)| k)
            .collect();
        for key in keys {
            if let Some(p) = self.pools.remove(&*key).await {
                tokio::spawn(async move { p.pool.close().await });
            }
        }
    }

    /// The pool of `database` for `info`, rebuilt when the branch's address, credentials or
    /// size changed.
    async fn pool(&self, branch: Uuid, database: &str, info: &Arc<ConnInfo>) -> Arc<DbPool> {
        let key = (branch, database.to_owned());
        if let Some(p) = self.pools.get(&key).await
            && p.info.same_server(info)
            && !p.pool.is_closed()
        {
            return p;
        }
        let options = PgConnectOptions::new()
            .host(&info.host)
            .port(info.port)
            .username(&info.user)
            .password(&info.password)
            .database(database)
            .ssl_mode(PgSslMode::Prefer)
            .application_name("chelabase-rest")
            .statement_cache_capacity(STATEMENT_CACHE);
        let pool = PgPoolOptions::new()
            .max_connections(info.settings.max_connections.max(1))
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
            .connect_lazy_with(options);
        let p = Arc::new(DbPool {
            info: info.clone(),
            pool,
            last_used: std::sync::Mutex::new(tokio::time::Instant::now()),
        });
        self.pools.insert(key, p.clone()).await;
        p
    }

    /// The branch's slots and pools, resized in place when `max_connections` changed.
    async fn branch(&self, branch: Uuid, info: &ConnInfo) -> Arc<BranchSlots> {
        let size = info.settings.max_connections.max(1) as usize;
        let slots = self
            .slots
            .get_with(branch, async { Arc::new(BranchSlots::new(size)) })
            .await;
        slots.resize(size);
        slots
    }

    /// The branch's semaphore (see [`Self::branch`]).
    #[cfg(test)]
    async fn slots(&self, branch: Uuid, info: &ConnInfo) -> Arc<Semaphore> {
        self.branch(branch, info).await.slots.clone()
    }

    /// Closes idle connections of the branch's other pools until `target` may open one
    /// within `max_connections` ([`idle_to_close`]).
    async fn make_room(&self, slots: &BranchSlots, target: &Arc<DbPool>, max: u32) {
        let others = slots.others(target);
        let loads: Vec<PoolLoad> = others.iter().map(|p| p.load()).collect();
        for (i, n) in idle_to_close(&target.load(), &loads, max) {
            for _ in 0..n {
                // Only an idle connection is taken; none left means another request did it.
                let Some(conn) = others[i].pool.try_acquire() else {
                    break;
                };
                let _ = conn.close().await;
            }
        }
    }

    /// A branch slot, then a global one: a branch's queue waits on its own slots (as many
    /// as its pool has connections), so it never holds more global slots than that.
    async fn permits(&self, branch: &Arc<Semaphore>) -> Result<Permits, AcquireError> {
        let wait = |sem: Arc<Semaphore>| async move {
            tokio::time::timeout(ACQUIRE_TIMEOUT, sem.acquire_owned())
                .await
                .map_err(|_| AcquireError::Busy)?
                .map_err(|_| AcquireError::Busy)
        };
        let branch = wait(branch.clone()).await?;
        let global = wait(self.permits.clone()).await?;
        Ok(Permits {
            _branch: branch,
            _global: global,
        })
    }

    /// A connection to the branch's `database`, as `authenticator`, holding a branch slot
    /// and a global slot until it's dropped. The caller checked the database is served.
    pub async fn acquire(&self, branch: Uuid, database: &str) -> Result<Conn, AcquireError> {
        let deadline = tokio::time::Instant::now() + ACQUIRE_DEADLINE;
        tokio::time::timeout_at(deadline, self.acquire_by(branch, database, deadline))
            .await
            .unwrap_or(Err(AcquireError::TimedOut))
    }

    async fn acquire_by(
        &self,
        branch: Uuid,
        database: &str,
        deadline: tokio::time::Instant,
    ) -> Result<Conn, AcquireError> {
        let mut retried = false;
        loop {
            let info = self.info(branch).await?;
            // The info fetched again for a retry may no longer serve it.
            if info.served(database).is_none() {
                return Err(AcquireError::NotServed);
            }
            let slots = self.branch(branch, &info).await;
            let pool = self.pool(branch, database, &info).await;
            slots.register(&pool);
            let permits = self.permits(&slots.slots).await?;
            *pool.last_used.lock().expect("last_used") = tokio::time::Instant::now();
            self.make_room(&slots, &pool, info.settings.max_connections.max(1))
                .await;
            // sqlx keeps retrying a refused connection until its acquire timeout; a
            // compute that moved refuses, so the first attempt gets less time before the
            // info is fetched again.
            let attempt = if retried {
                pool.pool.acquire().await
            } else {
                let first = FIRST_ATTEMPT.min(deadline - tokio::time::Instant::now());
                tokio::time::timeout(first, pool.pool.acquire())
                    .await
                    .unwrap_or(Err(sqlx::Error::PoolTimedOut))
            };
            match attempt {
                Ok(conn) => {
                    return Ok(Conn {
                        conn,
                        info,
                        _permits: permits,
                    });
                }
                Err(e) if !retried && is_connect_error(&e, pool_at_size(&pool)) => {
                    // Look again: a new address or password rebuilds the pool.
                    drop(permits);
                    self.infos.invalidate(&branch).await;
                    retried = true;
                }
                Err(e) => return Err(AcquireError::Db(e)),
            }
        }
    }
}

fn pool_at_size(p: &DbPool) -> bool {
    p.pool.size() >= p.pool.options().get_max_connections()
}

#[cfg(test)]
impl<S: ConnSource> Pools<S> {
    /// Fills every cache for `branch`: its info, a pool per served database, its slots.
    pub(crate) async fn warm(&self, branch: Uuid) {
        let info = self.info(branch).await.unwrap();
        let slots = self.branch(branch, &info).await;
        for d in &info.databases {
            slots.register(&self.pool(branch, &d.name, &info).await);
        }
    }

    /// What's held for `branch`: (info cached, pools, slots).
    pub(crate) async fn cached(&self, branch: Uuid) -> (bool, usize, bool) {
        (
            self.infos.contains_key(&branch),
            self.pools.iter().filter(|(k, _)| k.0 == branch).count(),
            self.slots.contains_key(&branch),
        )
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A branch serving `main` (`public`) and `analytics` (`reporting`), 2 connections.
    pub fn two_databases() -> ConnInfo {
        ConnInfo {
            host: "127.0.0.1".into(),
            port: 1,
            database: "main".into(),
            user: "authenticator".into(),
            password: "pw".into(),
            settings: Settings {
                max_rows: 1000,
                statement_timeout_ms: 8000,
                max_connections: 2,
            },
            databases: vec![
                ServedDatabase {
                    name: "main".into(),
                    default_schema: "public".into(),
                },
                ServedDatabase {
                    name: "analytics".into(),
                    default_schema: "reporting".into(),
                },
            ],
        }
    }

    /// [`two_databases`]: nothing listens on port 1, so every connect is refused.
    pub struct RefusedSource;

    impl ConnSource for RefusedSource {
        async fn fetch(&self, _branch: Uuid) -> Result<ConnInfo, ConnError> {
            Ok(two_databases())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoSource;

    impl ConnSource for NoSource {
        async fn fetch(&self, _branch: Uuid) -> Result<ConnInfo, ConnError> {
            Err(ConnError::NotReady("test".into()))
        }
    }

    /// One branch with slow queries and a queue of waiters can't take every global slot:
    /// its queue waits on its own slots first.
    #[tokio::test(start_paused = true)]
    async fn a_saturated_branch_leaves_global_slots_for_others() {
        let pools = Arc::new(Pools::new(NoSource, 3));
        let busy = Arc::new(Semaphore::new(2));
        let mut held = Vec::new();
        for _ in 0..2 {
            held.push(pools.permits(&busy).await.unwrap());
        }
        // Three more waiters on the busy branch.
        let mut waiters = Vec::new();
        for _ in 0..3 {
            let (pools, busy) = (pools.clone(), busy.clone());
            waiters.push(tokio::spawn(
                async move { pools.permits(&busy).await.is_ok() },
            ));
        }
        tokio::task::yield_now().await;
        let other = Arc::new(Semaphore::new(2));
        assert!(
            pools.permits(&other).await.is_ok(),
            "another branch got a slot"
        );
        drop(held);
        for w in waiters {
            w.await.unwrap();
        }
    }

    use super::test_support::RefusedSource;

    /// Waiting for slots, the first try and the retry share one deadline: a request gets
    /// its answer within [`ACQUIRE_DEADLINE`], however the wait is spread.
    #[tokio::test(start_paused = true)]
    async fn acquiring_answers_within_one_deadline() {
        let pools = Pools::new(RefusedSource, 1);
        // Every global slot is taken until just before the slot wait would time out.
        let held = pools.permits.clone().acquire_owned().await.unwrap();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(4_900)).await;
            drop(held);
        });
        let started = tokio::time::Instant::now();
        assert!(pools.acquire(Uuid::new_v4(), "main").await.is_err());
        let took = started.elapsed();
        assert!(
            took <= ACQUIRE_DEADLINE + Duration::from_millis(50),
            "took {took:?}"
        );
    }

    /// The orchestrator's current shape: the served databases, main first.
    #[test]
    fn conn_info_reads_the_databases_list() {
        let info: ConnInfo = serde_json::from_value(serde_json::json!({
            "host": "10.0.0.1", "port": 5432, "database": "main",
            "user": "authenticator", "password": "pw", "status": "ready",
            "settings": { "max_rows": 1000, "statement_timeout_ms": 8000, "max_connections": 4 },
            "databases": [
                { "name": "main", "default_schema": "public" },
                { "name": "analytics", "default_schema": "reporting" },
            ],
        }))
        .unwrap();
        assert_eq!(
            info.databases,
            vec![
                ServedDatabase {
                    name: "main".into(),
                    default_schema: "public".into()
                },
                ServedDatabase {
                    name: "analytics".into(),
                    default_schema: "reporting".into()
                },
            ]
        );
        assert_eq!(info.settings.max_connections, 4);
        assert_eq!(
            info.served("analytics").map(|d| d.default_schema.as_str()),
            Some("reporting")
        );
        assert!(info.served("ghost").is_none());
    }

    /// The shape before served databases existed: main alone, with the settings'
    /// `default_schema` (or `public` without one).
    #[test]
    fn conn_info_without_databases_serves_main_with_the_old_default_schema() {
        let old = |settings: serde_json::Value| -> ConnInfo {
            serde_json::from_value(serde_json::json!({
                "host": "10.0.0.1", "port": 5432, "database": "appdb",
                "user": "authenticator", "password": "pw", "status": "ready",
                "settings": settings,
            }))
            .unwrap()
        };
        let info = old(serde_json::json!({
            "max_rows": 1000, "statement_timeout_ms": 8000, "max_connections": 4,
            "default_schema": "app",
        }));
        assert_eq!(
            info.databases,
            vec![ServedDatabase {
                name: "appdb".into(),
                default_schema: "app".into()
            }]
        );
        let info = old(serde_json::json!({
            "max_rows": 1000, "statement_timeout_ms": 8000, "max_connections": 4,
        }));
        assert_eq!(
            info.databases,
            vec![ServedDatabase {
                name: "appdb".into(),
                default_schema: "public".into()
            }]
        );
    }

    /// Each database of a branch has its own pool; all of them wait on one set of branch
    /// slots, sized by `max_connections`.
    #[tokio::test]
    async fn pools_are_per_database_and_share_the_branch_slots() {
        let pools = Pools::new(RefusedSource, 10);
        let branch = Uuid::new_v4();
        let info = pools.info(branch).await.unwrap();
        let main = pools.pool(branch, "main", &info).await;
        let analytics = pools.pool(branch, "analytics", &info).await;
        assert!(!Arc::ptr_eq(&main, &analytics));
        assert_eq!(main.pool.connect_options().get_database(), Some("main"));
        assert_eq!(
            analytics.pool.connect_options().get_database(),
            Some("analytics")
        );
        assert!(Arc::ptr_eq(&main, &pools.pool(branch, "main", &info).await));

        let slots = pools.slots(branch, &info).await;
        assert!(Arc::ptr_eq(&slots, &pools.slots(branch, &info).await));
        assert_eq!(slots.available_permits(), 2);
        // Slots taken for one database are gone for the other.
        let held = pools.permits(&slots).await.unwrap();
        assert_eq!(pools.slots(branch, &info).await.available_permits(), 1);
        drop(held);
        // Another branch has its own.
        let other = pools.slots(Uuid::new_v4(), &info).await;
        assert!(!Arc::ptr_eq(&slots, &other));
        // A new `max_connections` resizes the branch's slots.
        let mut bigger = (*info).clone();
        bigger.settings.max_connections = 5;
        let resized = pools.slots(branch, &Arc::new(bigger)).await;
        assert_eq!(resized.available_permits(), 5);
    }

    fn load(size: u32, idle: usize, age_secs: u64) -> PoolLoad {
        PoolLoad {
            size,
            idle,
            last_used: tokio::time::Instant::now() - Duration::from_secs(age_secs),
        }
    }

    /// The branch's open connections never pass `max_connections` across its pools: before
    /// a pool opens a connection, idle ones of the branch's other pools are closed, least
    /// recently used first, until there's room.
    #[tokio::test(start_paused = true)]
    async fn idle_connections_of_other_pools_make_room() {
        tokio::time::advance(Duration::from_secs(100)).await;
        // Room left: nothing closes.
        assert!(idle_to_close(&load(1, 0, 0), &[load(1, 1, 5)], 3).is_empty());
        // The target reuses one of its own idle connections: nothing opens, nothing closes.
        assert!(idle_to_close(&load(1, 1, 0), &[load(2, 2, 5)], 3).is_empty());
        // Full: the least recently used pool gives up its idle connections first.
        let others = [load(1, 1, 5), load(2, 2, 50), load(1, 0, 90)];
        assert_eq!(idle_to_close(&load(0, 0, 0), &others, 4), vec![(1, 1)]);
        assert_eq!(idle_to_close(&load(1, 0, 0), &others, 4), vec![(1, 2)]);
        assert_eq!(idle_to_close(&load(2, 0, 0), &others, 4), vec![(1, 2), (0, 1)]);
        // For every mix, the branch's total after one connection opens stays within the cap
        // whenever enough idle connections exist (held ones can't pass it: the branch slots).
        for max in 1..=4u32 {
            for target in 0..=max {
                for a in 0..=max {
                    for b in 0..=max {
                        for (ia, ib) in [(0, 0), (a as usize, 0), (0, b as usize), (a as usize, b as usize)] {
                            let others = [load(a, ia, 10), load(b, ib, 20)];
                            let plan = idle_to_close(&load(target, 0, 0), &others, max);
                            let closed: usize = plan.iter().map(|(_, n)| n).sum();
                            for (i, n) in &plan {
                                assert!(*n <= others[*i].idle, "never more than idle");
                            }
                            let total = (target + a + b) as usize;
                            let idle = ia + ib;
                            if total < max as usize {
                                assert_eq!(closed, 0);
                            } else {
                                assert_eq!(closed, (total + 1 - max as usize).min(idle));
                                if total + 1 - max as usize <= idle {
                                    assert!(total - closed < max as usize);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// [`idle_to_close`] end to end: two databases of one branch with `max_connections` 2.
    /// Two connections left idle in one, then one taken in the other: the branch never has
    /// more than 2 open.
    #[tokio::test]
    #[ignore = "needs a Postgres: CHELABASE_METADATA_DATABASE_URL or the dev metadata Postgres (just deps-up)"]
    async fn open_connections_stay_within_the_branch_cap_live() {
        use sqlx::Executor;
        let url = std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://chelabase:chelabase@127.0.0.1:5433/chelabase_metadata".into()
        });
        let parsed = reqwest::Url::parse(&url).unwrap();
        let admin = PgPool::connect(&url).await.unwrap();
        let suffix = Uuid::new_v4().simple().to_string()[..8].to_owned();
        let (a, b) = (format!("rest_bound_a_{suffix}"), format!("rest_bound_b_{suffix}"));
        for db in [&a, &b] {
            admin
                .execute(sqlx::AssertSqlSafe(format!("CREATE DATABASE {db}")))
                .await
                .unwrap();
        }
        let info = ConnInfo {
            host: parsed.host_str().unwrap().into(),
            port: parsed.port().unwrap_or(5432),
            database: a.clone(),
            user: parsed.username().into(),
            password: parsed.password().unwrap_or_default().into(),
            settings: Settings {
                max_rows: 10,
                statement_timeout_ms: 1000,
                max_connections: 2,
            },
            databases: [&a, &b]
                .iter()
                .map(|n| ServedDatabase {
                    name: (*n).clone(),
                    default_schema: "public".into(),
                })
                .collect(),
        };
        struct Fixed(ConnInfo);
        impl ConnSource for Fixed {
            async fn fetch(&self, _branch: Uuid) -> Result<ConnInfo, ConnError> {
                Ok(self.0.clone())
            }
        }
        let pools = Pools::new(Fixed(info), 10);
        let branch = Uuid::new_v4();
        let open = || async {
            let (n,): (i64,) = sqlx::query_as(
                "SELECT count(*) FROM pg_stat_activity WHERE datname = ANY($1) AND application_name = 'chelabase-rest'",
            )
            .bind(vec![a.clone(), b.clone()])
            .fetch_one(&admin)
            .await
            .unwrap();
            n
        };
        let settle = |db: String, idle: usize| {
            let pools = &pools;
            async move {
                for _ in 0..200 {
                    if let Some(p) = pools.pools.get(&(branch, db.clone())).await
                        && p.pool.num_idle() == idle
                    {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                panic!("{db} never had {idle} idle");
            }
        };
        let (c1, c2) = (
            pools.acquire(branch, &a).await.unwrap(),
            pools.acquire(branch, &a).await.unwrap(),
        );
        drop((c1, c2));
        settle(a.clone(), 2).await;
        assert_eq!(open().await, 2);
        let c3 = pools.acquire(branch, &b).await.unwrap();
        let mut seen = 0;
        for _ in 0..100 {
            seen = open().await;
            if seen <= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(seen <= 2, "{seen} connections open for a branch capped at 2");
        let a_pool = pools.pools.get(&(branch, a.clone())).await.unwrap();
        assert_eq!(a_pool.pool.size() + 1, 2, "one of a's idle connections was closed");
        drop(c3);
        pools.invalidate(branch).await;
        for db in [&a, &b] {
            admin
                .execute(sqlx::AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
                .await
                .unwrap();
        }
    }

    /// A new `max_connections` resizes the branch's one semaphore: permits held across the
    /// change still count, so the branch never holds more than the new size.
    #[tokio::test]
    async fn branch_slots_resize_in_place() {
        let pools = Pools::new(RefusedSource, 10);
        let branch = Uuid::new_v4();
        let info = pools.info(branch).await.unwrap();
        let slots = pools.slots(branch, &info).await;
        let held: Vec<_> = (0..2).map(|_| slots.clone().try_acquire_owned().unwrap()).collect();
        let with = |n: u32| {
            let mut i = (*info).clone();
            i.settings.max_connections = n;
            Arc::new(i)
        };
        // Shrinking to 1 while both are held: none is free until the branch is under 1.
        let shrunk = pools.slots(branch, &with(1)).await;
        assert!(Arc::ptr_eq(&slots, &shrunk), "the same semaphore");
        assert_eq!(shrunk.available_permits(), 0);
        drop(held);
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(shrunk.available_permits(), 1);
        // Growing while one is held.
        let one = shrunk.clone().try_acquire_owned().unwrap();
        let grown = pools.slots(branch, &with(3)).await;
        assert!(Arc::ptr_eq(&slots, &grown));
        assert_eq!(grown.available_permits(), 2);
        drop(one);
        assert_eq!(grown.available_permits(), 3);
    }

    /// A connection whose database stopped being served while it waited for a retry isn't
    /// opened: the re-fetched info is checked again.
    #[tokio::test(start_paused = true)]
    async fn a_retry_checks_the_database_is_still_served() {
        struct Dropping(std::sync::atomic::AtomicUsize);
        impl ConnSource for Arc<Dropping> {
            async fn fetch(&self, _branch: Uuid) -> Result<ConnInfo, ConnError> {
                let mut info = super::test_support::two_databases();
                if self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 {
                    info.databases.truncate(1);
                }
                Ok(info)
            }
        }
        let pools = Pools::new(Arc::new(Dropping(0.into())), 10);
        let e = pools.acquire(Uuid::new_v4(), "analytics").await.err().unwrap();
        assert!(matches!(e, AcquireError::NotServed), "{e:?}");
    }

    #[test]
    fn a_busy_pool_timing_out_is_not_a_moved_compute() {
        assert!(!is_connect_error(&sqlx::Error::PoolTimedOut, true));
        assert!(is_connect_error(&sqlx::Error::PoolTimedOut, false));
        let io = || sqlx::Error::Io(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
        assert!(is_connect_error(&io(), true));
        assert!(is_connect_error(&io(), false));
        assert!(!is_connect_error(&sqlx::Error::RowNotFound, false));
        // A pool closed under the request (an invalidate): a new one is built.
        assert!(is_connect_error(&sqlx::Error::PoolClosed, true));
    }
}
