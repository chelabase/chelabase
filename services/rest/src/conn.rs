//! Where each branch's database is, and a pool of connections to it.
//!
//! Connection info comes from the orchestrator's internal listener and is cached 60 s (or
//! until [`Pools::invalidate`]). Each served database of a branch gets its own
//! deadpool-postgres pool (dropped after 5 minutes unused); every pool of a branch waits on
//! one set of branch slots (`max_connections`), and one global semaphore caps connections
//! across every branch. Open connections, idle ones included, stay within `max_connections`
//! per branch too: before a pool may open one, idle connections of the branch's other pools
//! are closed, least recently used pool first and oldest connection first
//! ([`idle_to_close`], [`oldest_first`]). A reaper closes connections idle past
//! [`CONNECTION_IDLE`]. A connection that fails to open (the compute moved after a reset or
//! a restore, or its password changed) drops the cached info, fetches it again and retries
//! once; nothing is retried once a statement was sent.
//!
//! Nothing runs when a connection goes back to its pool: every request starts with
//! `BEGIN; RESET_SQL` (with a `ROLLBACK` first when the connection may be inside a
//! transaction: [`chelabase_query::engine::begin_sql`]) and ends with
//! `RESET_SQL` after its `COMMIT`; a request that doesn't finish closes its connection
//! ([`chelabase_query::Lease`]).

use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use deadpool_postgres::{
    Manager, ManagerConfig, Object, Pool, PoolError, RecyclingMethod, Runtime, TimeoutType,
};
use serde::Deserialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use chelabase_query::server_tls::ServerTls;
use tokio_postgres_rustls_improved::MakeRustlsConnect;

/// How long connection info is trusted.
pub const INFO_TTL: Duration = Duration::from_secs(60);
/// A request naming a database the cached info doesn't serve fetches the info again when it
/// is at least this old (a database enabled since, without an invalidate reaching rest).
pub const INFO_REFRESH_AFTER: Duration = Duration::from_secs(2);
/// A branch's pool is dropped after this long unused.
pub const POOL_IDLE: Duration = Duration::from_secs(5 * 60);
/// Idle connections inside a pool are closed after this long (deadpool counts it from when
/// the connection was last handed out).
pub const CONNECTION_IDLE: Duration = Duration::from_secs(60);
/// How often the reaper looks for connections idle past [`CONNECTION_IDLE`].
pub const REAP_EVERY: Duration = Duration::from_secs(10);
/// The longest opening one connection may take.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest a request waits for a connection (or a global permit).
pub const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest a request waits for a connection in all (slots, first try and retry).
pub const ACQUIRE_DEADLINE: Duration = Duration::from_secs(6);
/// The first try for a connection, before the branch's info is fetched again.
pub const FIRST_ATTEMPT: Duration = Duration::from_secs(1);
/// Most databases with a live pool.
const MAX_POOLS: u64 = 10_000;
/// Sent at the front of every request (`[ROLLBACK;] BEGIN; RESET_SQL`) and after its end: a
/// function an earlier request called may have changed the session (`SET ROLE`, `set_config(…, false)`), and
/// each request must start as `authenticator` with the connection's defaults. Prepared
/// statements survive it.
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
    /// The name the compute's server certificate carries
    /// (`<endpoint_id>.compute.chelabase.internal`): `host` is then its IP, dialled, and the
    /// connection is TLS verified against the platform CA as this name (U15).
    pub server_name: Option<String>,
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
            &self.server_name,
            &self.user,
            &self.password,
            self.settings.max_connections,
        ) == (
            &other.host,
            other.port,
            &other.server_name,
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
    #[serde(default)]
    server_name: Option<String>,
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
            server_name: w.server_name,
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
            .field("server_name", &self.server_name)
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
    Db(DbAcquireError),
}

/// The pool's own failure to hand out a connection.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct DbAcquireError(pub PoolError);

/// A connection, holding its branch and global slots until it's dropped. Lease the client
/// (`chelabase_query::Lease::new(conn.client)`) for a request.
pub struct Conn {
    pub client: Object,
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
    /// What its connections open with (the manager doesn't show it).
    #[cfg(test)]
    config: tokio_postgres::Config,
    pool: Pool,
    /// When a request last took a connection from it.
    last_used: std::sync::Mutex<tokio::time::Instant>,
    /// Connections being opened ([`Opening`]): deadpool counts one in its size only once
    /// it's open.
    opening: AtomicUsize,
}

impl DbPool {
    #[cfg(test)]
    fn new(info: Arc<ConnInfo>, config: tokio_postgres::Config, max_size: usize) -> Self {
        Self::with_connector(info, config, max_size, None)
    }

    /// A pool whose connections use `connector` (TLS verified against the platform CA), or
    /// none (plain: the fakes).
    fn with_connector(
        info: Arc<ConnInfo>,
        config: tokio_postgres::Config,
        max_size: usize,
        connector: Option<MakeRustlsConnect>,
    ) -> Self {
        DbPool {
            info,
            #[cfg(test)]
            config: config.clone(),
            pool: build_pool_with(config, max_size, connector),
            last_used: std::sync::Mutex::new(tokio::time::Instant::now()),
            opening: AtomicUsize::new(0),
        }
    }

    fn load(&self) -> PoolLoad {
        // `opening` first: a connection that finishes opening between the two reads is then
        // counted twice at worst, never missed.
        let opening = self.opening.load(Ordering::SeqCst);
        let status = self.pool.status();
        let size = status.size + opening;
        PoolLoad {
            size: u32::try_from(size).unwrap_or(u32::MAX),
            idle: status.available,
            last_used: *self.last_used.lock().expect("last_used"),
        }
    }
}

/// A connection being opened in a pool, counted in [`DbPool::opening`] until it's dropped
/// (opened, failed or cancelled).
struct Opening<'a>(&'a AtomicUsize);

impl<'a> Opening<'a> {
    fn new(count: &'a AtomicUsize) -> Self {
        count.fetch_add(1, Ordering::SeqCst);
        Opening(count)
    }
}

impl Drop for Opening<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// How a pool for `database` of `info` connects, plain (`host` dialled as given).
fn pg_config(info: &ConnInfo, database: &str) -> tokio_postgres::Config {
    let mut config = base_config(info, database);
    config.host(&info.host);
    config
}

/// Everything but where: the caller sets the host (plain) or the TLS target.
fn base_config(info: &ConnInfo, database: &str) -> tokio_postgres::Config {
    let mut config = tokio_postgres::Config::new();
    config
        .port(info.port)
        .user(&info.user)
        .password(&info.password)
        .dbname(database)
        .application_name("chelabase-rest")
        .connect_timeout(CONNECT_TIMEOUT);
    config
}

/// How a pool for `database` of `info` connects (U15): a named target (`server_name`) over TLS
/// verified against `server_tls` (its IP dialled, its name verified); an unnamed one plain,
/// only without a CA (the fakes). A target that can't be verified is refused, never dialled.
fn connect_config(
    info: &ConnInfo,
    database: &str,
    server_tls: Option<&ServerTls>,
) -> Result<(tokio_postgres::Config, Option<MakeRustlsConnect>), ConnError> {
    match (&info.server_name, server_tls) {
        (Some(name), Some(tls)) => {
            let mut config = base_config(info, database);
            tls.configure(&mut config, &info.host, name)
                .map_err(|e| ConnError::Unreachable(format!("compute TLS: {e}")))?;
            Ok((config, Some(tls.connector())))
        }
        (Some(_), None) => Err(ConnError::Unreachable(
            "no CA to verify the compute's TLS against".into(),
        )),
        (None, Some(_)) => Err(ConnError::Unreachable(
            "no certificate name to verify the compute's TLS against".into(),
        )),
        (None, None) => Ok((pg_config(info, database), None)),
    }
}

/// A plain pool ([`build_pool_with`] without a connector).
#[cfg(test)]
fn build_pool(config: tokio_postgres::Config, max_size: usize) -> Pool {
    build_pool_with(config, max_size, None)
}

/// A pool of up to `max_size` connections opened with `config`: TLS through `connector`
/// (plain without one: the fakes only), `Fast` recycling (only a closed connection is dropped), a wait
/// for a free connection capped at [`ACQUIRE_TIMEOUT`]. Each connection's statement cache
/// is bounded by the lease that prepares through it
/// ([`chelabase_query::pipeline::STATEMENT_CACHE_CAP`]).
fn build_pool_with(
    config: tokio_postgres::Config,
    max_size: usize,
    connector: Option<MakeRustlsConnect>,
) -> Pool {
    let manager_config = ManagerConfig {
        recycling_method: RecyclingMethod::Fast,
    };
    let manager = match connector {
        Some(connector) => Manager::from_config(config, connector, manager_config),
        None => Manager::from_config(config, tokio_postgres::NoTls, manager_config),
    };
    Pool::builder(manager)
        .max_size(max_size)
        .runtime(Runtime::Tokio1)
        .wait_timeout(Some(ACQUIRE_TIMEOUT))
        .build()
        .expect("a pool with a runtime")
}

/// Which `n` of a pool's idle connections to close: the longest unused first. `idle` is each
/// connection's key and how long since it was last handed out.
fn oldest_first<K: Copy>(idle: &[(K, Duration)], n: usize) -> Vec<K> {
    let mut order: Vec<&(K, Duration)> = idle.iter().collect();
    order.sort_by_key(|(_, unused)| std::cmp::Reverse(*unused));
    order.into_iter().take(n).map(|(k, _)| *k).collect()
}

/// Closes `n` of `pool`'s idle connections, the longest unused first ([`oldest_first`]).
/// Connections in use are never touched; fewer close when fewer are idle.
fn close_oldest_idle(pool: &Pool, n: usize) {
    if n == 0 {
        return;
    }
    let mut idle = Vec::new();
    pool.retain(|_, m| {
        idle.push((m.created, m.last_used()));
        true
    });
    let mut close = oldest_first(&idle, n);
    // Connections are told apart by when they were created; one taken in between stays.
    let _closed = pool.retain(|_, m| match close.iter().position(|c| *c == m.created) {
        Some(i) => {
            close.swap_remove(i);
            false
        }
        None => true,
    });
}

/// Closes every idle connection of `pool` unused for `idle` or longer.
fn reap(pool: &Pool, idle: Duration) {
    // The removed connections close as they drop.
    let _closed = pool.retain(|_, m| m.last_used() < idle);
}

type PoolCache = moka::future::Cache<(Uuid, String), Arc<DbPool>>;

/// Every `every`, [`reap`]s each cached pool with `idle`; ends once the cache is gone.
fn spawn_reaper(
    pools: Weak<PoolCache>,
    every: Duration,
    idle: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            let Some(pools) = pools.upgrade() else {
                return;
            };
            for (_, p) in pools.iter() {
                reap(&p.pool, idle);
            }
        }
    })
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
    /// Open connections, idle and in use, plus those being opened ([`Opening`]).
    size: u32,
    /// deadpool's `available`: open connections minus the requests holding or waiting for
    /// one, so it under-reports idle connections while requests wait.
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
    /// Shared with the reaper, which holds it weakly.
    pools: Arc<PoolCache>,
    slots: moka::future::Cache<Uuid, Arc<BranchSlots>>,
    permits: Arc<Semaphore>,
    /// The platform CA computes are verified against (U15); `None` only for the fakes.
    server_tls: Option<ServerTls>,
}

/// Failing to open a connection, before any statement: worth a fresh look at where the
/// compute is. `at_size`: the pool already holds all its connections.
fn is_connect_error(e: &PoolError, at_size: bool) -> bool {
    match e {
        // A pool whose connections are all open timed out waiting for one to come back.
        PoolError::Timeout(TimeoutType::Wait) => !at_size,
        PoolError::Timeout(TimeoutType::Create) => true,
        // Refused, closed, or any failure before Postgres answered; of Postgres's answers,
        // refused credentials, a connection exception or a server shutting down or starting.
        PoolError::Backend(e) => match e.as_db_error() {
            _ if e.is_closed() => true,
            None => true,
            Some(db) => {
                let code = db.code().code();
                code.starts_with("28") || code.starts_with("08") || code.starts_with("57P")
            }
        },
        // An invalidate closed the pool under the request: the next try builds a new one.
        PoolError::Closed => true,
        _ => false,
    }
}

impl<S: ConnSource> Pools<S> {
    /// Starts the reaper too, so it must be called inside a Tokio runtime.
    pub fn new(source: S, max_connections: usize) -> Self {
        let pools = Arc::new(
            moka::future::Cache::builder()
                .max_capacity(MAX_POOLS)
                .time_to_idle(POOL_IDLE)
                .build(),
        );
        spawn_reaper(Arc::downgrade(&pools), REAP_EVERY, CONNECTION_IDLE);
        Pools {
            source,
            infos: moka::future::Cache::builder()
                .max_capacity(MAX_POOLS)
                .time_to_live(INFO_TTL)
                .build(),
            pools,
            slots: moka::future::Cache::builder()
                .max_capacity(MAX_POOLS)
                .time_to_idle(POOL_IDLE)
                .build(),
            permits: Arc::new(Semaphore::new(max_connections)),
            server_tls: None,
        }
    }

    /// Like [`Self::new`], verifying every compute against `server_tls` (U15): the service's
    /// pools. A target without a certificate name is refused before any connection.
    pub fn with_server_tls(source: S, max_connections: usize, server_tls: ServerTls) -> Self {
        Pools {
            server_tls: Some(server_tls),
            ..Self::new(source, max_connections)
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
    /// info and its pools (closed at once; connections in use close as they come back). Its slots stay, so
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
                p.pool.close();
            }
        }
    }

    /// The pool of `database` for `info`, rebuilt when the branch's address, credentials or
    /// size changed.
    async fn pool(
        &self,
        branch: Uuid,
        database: &str,
        info: &Arc<ConnInfo>,
    ) -> Result<Arc<DbPool>, ConnError> {
        let key = (branch, database.to_owned());
        if let Some(p) = self.pools.get(&key).await
            && p.info.same_server(info)
            && !p.pool.is_closed()
        {
            return Ok(p);
        }
        let (config, connector) = connect_config(info, database, self.server_tls.as_ref())?;
        let p = Arc::new(DbPool::with_connector(
            info.clone(),
            config,
            info.settings.max_connections.max(1) as usize,
            connector,
        ));
        self.pools.insert(key, p.clone()).await;
        Ok(p)
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
    /// within `max_connections` ([`idle_to_close`]). `own_open`: the caller's [`Opening`] is
    /// already in `target`'s load (`idle_to_close` adds the one to open itself).
    fn make_room(&self, slots: &BranchSlots, target: &Arc<DbPool>, max: u32, own_open: bool) {
        let others = slots.others(target);
        let loads: Vec<PoolLoad> = others.iter().map(|p| p.load()).collect();
        let mut load = target.load();
        if own_open {
            load.size = load.size.saturating_sub(1);
        }
        for (i, n) in idle_to_close(&load, &loads, max) {
            // Only idle connections close; fewer left means another request took them.
            close_oldest_idle(&others[i].pool, n);
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
            let pool = self.pool(branch, database, &info).await?;
            slots.register(&pool);
            let permits = self.permits(&slots.slots).await?;
            *pool.last_used.lock().expect("last_used") = tokio::time::Instant::now();
            // No idle connection to reuse: one may be opened, so it counts against the
            // branch before room is made (another request's `make_room` sees it) until the
            // attempt ends, however it ends.
            let opening = (pool.pool.status().available == 0).then(|| Opening::new(&pool.opening));
            self.make_room(
                &slots,
                &pool,
                info.settings.max_connections.max(1),
                opening.is_some(),
            );
            // A compute that moved may not answer at all (a dropped SYN), so the first
            // attempt gets less time before the info is fetched again; running out of it
            // counts as a wait for the pool.
            let attempt = if retried {
                pool.pool.get().await
            } else {
                let first = FIRST_ATTEMPT.min(deadline - tokio::time::Instant::now());
                tokio::time::timeout(first, pool.pool.get())
                    .await
                    .unwrap_or(Err(PoolError::Timeout(TimeoutType::Wait)))
            };
            drop(opening);
            match attempt {
                Ok(client) => {
                    return Ok(Conn {
                        client,
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
                Err(e) => return Err(AcquireError::Db(DbAcquireError(e))),
            }
        }
    }
}

fn pool_at_size(p: &DbPool) -> bool {
    let status = p.pool.status();
    status.size >= status.max_size
}

#[cfg(test)]
impl<S: ConnSource> Pools<S> {
    /// Fills every cache for `branch`: its info, a pool per served database, its slots.
    pub(crate) async fn warm(&self, branch: Uuid) {
        let info = self.info(branch).await.unwrap();
        let slots = self.branch(branch, &info).await;
        for d in &info.databases {
            slots.register(&self.pool(branch, &d.name, &info).await.unwrap());
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
            server_name: None,
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

    /// How [`fake_pg`] answers a startup message.
    #[derive(Debug, Clone, Copy)]
    pub enum Answer {
        /// `AuthenticationOk`, `ReadyForQuery`, then it holds the socket until the client
        /// closes it.
        Ready,
        /// `AuthenticationOk`, `ReadyForQuery`, then it closes the socket.
        ReadyThenClose,
        /// A FATAL `ErrorResponse` with this SQLSTATE and message, then it closes.
        Error(&'static str, &'static str),
    }

    /// A Postgres stand-in on 127.0.0.1 answering every startup with one [`Answer`].
    pub struct FakePg {
        pub port: u16,
        /// Sockets it answered [`Answer::Ready`] on that the client hasn't closed yet.
        pub open: Arc<std::sync::atomic::AtomicUsize>,
    }

    fn error_response(code: &str, message: &str) -> Vec<u8> {
        let mut body = Vec::new();
        for (field, value) in [(b'S', "FATAL"), (b'V', "FATAL"), (b'C', code), (b'M', message)] {
            body.push(field);
            body.extend_from_slice(value.as_bytes());
            body.push(0);
        }
        body.push(0);
        let mut out = vec![b'E'];
        out.extend_from_slice(&i32::try_from(body.len() + 4).unwrap().to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    pub async fn fake_pg(answer: Answer) -> FakePg {
        use std::sync::atomic::Ordering;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let open = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = open.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let open = count.clone();
                tokio::spawn(async move {
                    let Ok(len) = socket.read_i32().await else {
                        return;
                    };
                    let mut startup = vec![0; usize::try_from(len).unwrap_or(4) - 4];
                    if socket.read_exact(&mut startup).await.is_err() {
                        return;
                    }
                    const READY: [u8; 15] = [b'R', 0, 0, 0, 8, 0, 0, 0, 0, b'Z', 0, 0, 0, 5, b'I'];
                    match answer {
                        Answer::Ready => {
                            open.fetch_add(1, Ordering::SeqCst);
                            if socket.write_all(&READY).await.is_ok() {
                                let mut buf = [0; 1024];
                                while matches!(socket.read(&mut buf).await, Ok(n) if n > 0) {}
                            }
                            open.fetch_sub(1, Ordering::SeqCst);
                        }
                        Answer::ReadyThenClose => {
                            let _ = socket.write_all(&READY).await;
                        }
                        Answer::Error(code, message) => {
                            let _ = socket.write_all(&error_response(code, message)).await;
                        }
                    }
                });
            }
        });
        FakePg { port, open }
    }

    pub fn fake_config(port: u16) -> tokio_postgres::Config {
        let mut config = tokio_postgres::Config::new();
        config.host("127.0.0.1").port(port).user("x").dbname("x");
        config
    }

    /// What connecting to a [`fake_pg`] answering `answer` fails with.
    pub async fn connect_error(answer: Answer) -> tokio_postgres::Error {
        let fake = fake_pg(answer).await;
        fake_config(fake.port)
            .connect(tokio_postgres::NoTls)
            .await
            .map(|_| ())
            .unwrap_err()
    }

    /// A refused connection (nothing listens on the port).
    pub async fn refused_error() -> tokio_postgres::Error {
        let port = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            listener.local_addr().unwrap().port()
        };
        fake_config(port)
            .connect(tokio_postgres::NoTls)
            .await
            .map(|_| ())
            .unwrap_err()
    }

    /// A statement on a connection the server closed.
    pub async fn closed_error() -> tokio_postgres::Error {
        let fake = fake_pg(Answer::ReadyThenClose).await;
        let (client, connection) = fake_config(fake.port)
            .connect(tokio_postgres::NoTls)
            .await
            .unwrap();
        // The connection ends when it reads the server's end of the stream.
        let _ = connection.await;
        client.batch_execute("SELECT 1").await.unwrap_err()
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
        let main = pools.pool(branch, "main", &info).await.unwrap();
        let analytics = pools.pool(branch, "analytics", &info).await.unwrap();
        assert!(!Arc::ptr_eq(&main, &analytics));
        assert_eq!(main.config.get_dbname(), Some("main"));
        assert_eq!(analytics.config.get_dbname(), Some("analytics"));
        assert!(Arc::ptr_eq(&main, &pools.pool(branch, "main", &info).await.unwrap()));

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
    #[ignore = "needs a Postgres: CHELABASE_METADATA_DATABASE_URL or the workbench stack's metadata Postgres"]
    async fn open_connections_stay_within_the_branch_cap_live() {
        use sqlx::Executor;
        use sqlx::postgres::PgPool;
        let url = std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
            "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".into()
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
            server_name: None,
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
                        && p.pool.status().available == idle
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
        assert_eq!(
            a_pool.pool.status().size + 1,
            2,
            "one of a's idle connections was closed"
        );
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

    #[tokio::test]
    async fn a_busy_pool_timing_out_is_not_a_moved_compute() {
        use super::test_support::{Answer, closed_error, connect_error, refused_error};
        let backend = |e| PoolError::Backend(e);
        // (error, retried when the pool is at its size, retried below it)
        let table: Vec<(&str, PoolError, bool, bool)> = vec![
            // A pool whose connections are all open timed out waiting for one to come back.
            ("wait", PoolError::Timeout(TimeoutType::Wait), false, true),
            ("create", PoolError::Timeout(TimeoutType::Create), true, true),
            ("recycle", PoolError::Timeout(TimeoutType::Recycle), false, false),
            ("refused", backend(refused_error().await), true, true),
            ("closed", backend(closed_error().await), true, true),
            (
                "password",
                backend(connect_error(Answer::Error("28P01", "password failed")).await),
                true,
                true,
            ),
            (
                "connection",
                backend(connect_error(Answer::Error("08006", "gone")).await),
                true,
                true,
            ),
            (
                "starting up",
                backend(connect_error(Answer::Error("57P03", "starting")).await),
                true,
                true,
            ),
            (
                "no database",
                backend(connect_error(Answer::Error("3D000", "no such database")).await),
                false,
                false,
            ),
            // A pool closed under the request (an invalidate): a new one is built.
            ("pool closed", PoolError::Closed, true, true),
            ("no runtime", PoolError::NoRuntimeSpecified, false, false),
        ];
        for (name, e, at_size, below) in &table {
            assert_eq!(is_connect_error(e, true), *at_size, "{name} at size: {e:?}");
            assert_eq!(is_connect_error(e, false), *below, "{name} below size: {e:?}");
        }
    }

    /// Within a pool, the connections unused longest close first, never more than idle.
    #[tokio::test]
    async fn make_room_closes_the_oldest_idle_first() {
        let s = Duration::from_secs;
        let idle = [(0, s(5)), (1, s(50)), (2, s(1)), (3, s(20))];
        assert_eq!(oldest_first(&idle, 1), vec![1]);
        assert_eq!(oldest_first(&idle, 2), vec![1, 3]);
        assert_eq!(oldest_first(&idle, 9), vec![1, 3, 0, 2]);
        assert!(oldest_first(&idle, 0).is_empty());

        // On a pool: three idle connections, handed out at different times.
        let fake = super::test_support::fake_pg(super::test_support::Answer::Ready).await;
        let pool = build_pool(super::test_support::fake_config(fake.port), 3);
        let mut held = Vec::new();
        for _ in 0..3 {
            held.push(pool.get().await.unwrap());
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let created: Vec<_> = held.iter().map(|c| Object::metrics(c).created).collect();
        // One in use is never closed.
        let in_use = held.pop().unwrap();
        drop(held);
        close_oldest_idle(&pool, 1);
        let mut left = Vec::new();
        pool.retain(|_, m| {
            left.push(m.created);
            true
        });
        assert_eq!(left, vec![created[1]], "the oldest went first");
        close_oldest_idle(&pool, 5);
        assert_eq!(pool.status().size, 1, "only the one in use is left");
        drop(in_use);
        for _ in 0..100 {
            if fake.open.load(std::sync::atomic::Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(fake.open.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// deadpool counts a connection in its size only once it's open: a connection another
    /// database of the branch is still opening counts too, so the cap holds while two
    /// requests open connections at once.
    #[tokio::test(start_paused = true)]
    async fn pending_opens_count_against_the_branch_cap() {
        tokio::time::advance(Duration::from_secs(100)).await;
        let info = Arc::new(super::test_support::two_databases());
        let analytics = DbPool::new(info.clone(), pg_config(&info, "analytics"), 2);
        assert_eq!(analytics.load().size, 0);
        let opening = Opening::new(&analytics.opening);
        assert_eq!(analytics.load().size, 1, "an open in flight counts");
        // `main` keeps one idle connection; the target opens one; the branch's cap is 2.
        let main = load(1, 1, 50);
        assert_eq!(
            idle_to_close(&load(0, 0, 0), &[main, analytics.load()], 2),
            vec![(0, 1)]
        );
        drop(opening);
        assert!(idle_to_close(&load(0, 0, 0), &[main, analytics.load()], 2).is_empty());
    }

    /// An [`Opening`] counts from when it's made until it's dropped, a cancelled request's
    /// included.
    #[tokio::test]
    async fn an_opening_is_counted_until_it_ends() {
        let count = Arc::new(AtomicUsize::new(0));
        {
            let _a = Opening::new(&count);
            assert_eq!(count.load(Ordering::SeqCst), 1);
            let _b = Opening::new(&count);
            assert_eq!(count.load(Ordering::SeqCst), 2);
        }
        assert_eq!(count.load(Ordering::SeqCst), 0);
        // Cancelled while it waits.
        let c = count.clone();
        let task = tokio::spawn(async move {
            let _opening = Opening::new(&c);
            std::future::pending::<()>().await;
        });
        for _ in 0..100 {
            if count.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
        task.abort();
        let _ = task.await;
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    /// The reaper closes connections idle past the limit, keeps the others, and ends once its
    /// pools are gone.
    #[tokio::test]
    async fn reaper_closes_connections_idle_past_the_limit() {
        use std::sync::atomic::Ordering;
        let fake = super::test_support::fake_pg(super::test_support::Answer::Ready).await;
        let mut info = super::test_support::two_databases();
        info.port = fake.port;
        let pools = Arc::new(moka::future::Cache::builder().build());
        let pool = Arc::new(DbPool::new(
            Arc::new(info.clone()),
            pg_config(&info, "main"),
            2,
        ));
        pools.insert((Uuid::nil(), "main".to_owned()), pool.clone()).await;
        let (a, b) = (pool.pool.get().await.unwrap(), pool.pool.get().await.unwrap());
        drop(a);
        let reaper = spawn_reaper(
            Arc::downgrade(&pools),
            Duration::from_millis(20),
            Duration::from_millis(400),
        );
        // Not idle long enough yet.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(pool.pool.status().size, 2);
        // Past the limit: the idle one closes, the one in use stays.
        let mut size = 2;
        for _ in 0..200 {
            size = pool.pool.status().size;
            if size == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(size, 1, "the idle connection was closed");
        for _ in 0..100 {
            if fake.open.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(fake.open.load(Ordering::SeqCst), 1, "its socket too");
        drop(b);
        drop(pools);
        tokio::time::timeout(Duration::from_secs(2), reaper)
            .await
            .expect("the reaper ends with its pools")
            .unwrap();
    }
}
