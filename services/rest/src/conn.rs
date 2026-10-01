//! Where each branch's database is, and a pool of connections to it.
//!
//! Connection info comes from the orchestrator's internal listener and is cached 60 s. Each
//! branch gets its own sqlx pool (dropped after 5 minutes unused), and one global semaphore
//! caps connections across every pool. A connection that fails to open (the compute moved
//! after a reset or a restore, or its password changed) drops the cached info, fetches it
//! again and retries once; nothing is retried once a statement was sent.

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
/// Most branches with a live pool.
const MAX_POOLS: u64 = 10_000;
/// Run on every connection handed back to its pool: a function the request called may have
/// changed the session (`SET ROLE`, `set_config(…, false)`), and the next request must start
/// as `authenticator` with the connection's defaults. Prepared statements survive it.
pub const RESET_SQL: &str = "SET ROLE NONE; RESET ALL";

/// The database's data API settings, as the internal listener sends them.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Settings {
    pub max_rows: i64,
    pub statement_timeout_ms: i64,
    pub max_connections: u32,
    pub default_schema: String,
}

/// `GET /internal/data/branches/{id}/connection`.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct ConnInfo {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: String,
    pub settings: Settings,
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

struct BranchPool {
    info: Arc<ConnInfo>,
    pool: PgPool,
    /// One per pool connection.
    slots: Arc<Semaphore>,
}

pub struct Pools<S> {
    source: S,
    infos: moka::future::Cache<Uuid, Arc<ConnInfo>>,
    pools: moka::future::Cache<Uuid, Arc<BranchPool>>,
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
            permits: Arc::new(Semaphore::new(max_connections)),
        }
    }

    /// The branch's connection info (cached; concurrent misses share one fetch).
    pub async fn info(&self, branch: Uuid) -> Result<Arc<ConnInfo>, ConnError> {
        self.infos
            .try_get_with(branch, async {
                self.source.fetch(branch).await.map(Arc::new)
            })
            .await
            .map_err(|e| (*e).clone())
    }

    /// The pool for `info`, rebuilt when the branch's info changed.
    async fn pool(&self, branch: Uuid, info: &Arc<ConnInfo>) -> Arc<BranchPool> {
        if let Some(p) = self.pools.get(&branch).await
            && *p.info == **info
        {
            return p;
        }
        let options = PgConnectOptions::new()
            .host(&info.host)
            .port(info.port)
            .username(&info.user)
            .password(&info.password)
            .database(&info.database)
            .ssl_mode(PgSslMode::Prefer)
            .application_name("cheladb-rest")
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
        let p = Arc::new(BranchPool {
            info: info.clone(),
            pool,
            slots: Arc::new(Semaphore::new(info.settings.max_connections.max(1) as usize)),
        });
        self.pools.insert(branch, p.clone()).await;
        p
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

    /// A connection to the branch's database, as `authenticator`, holding a branch slot
    /// and a global slot until it's dropped.
    pub async fn acquire(&self, branch: Uuid) -> Result<Conn, AcquireError> {
        let deadline = tokio::time::Instant::now() + ACQUIRE_DEADLINE;
        tokio::time::timeout_at(deadline, self.acquire_by(branch, deadline))
            .await
            .unwrap_or(Err(AcquireError::TimedOut))
    }

    async fn acquire_by(
        &self,
        branch: Uuid,
        deadline: tokio::time::Instant,
    ) -> Result<Conn, AcquireError> {
        let mut retried = false;
        loop {
            let info = self.info(branch).await?;
            let pool = self.pool(branch, &info).await;
            let permits = self.permits(&pool.slots).await?;
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

fn pool_at_size(p: &BranchPool) -> bool {
    p.pool.size() >= p.pool.options().get_max_connections()
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

    /// Nothing listens on port 1: every connect is refused.
    struct RefusedSource;

    impl ConnSource for RefusedSource {
        async fn fetch(&self, _branch: Uuid) -> Result<ConnInfo, ConnError> {
            Ok(ConnInfo {
                host: "127.0.0.1".into(),
                port: 1,
                database: "d".into(),
                user: "authenticator".into(),
                password: "pw".into(),
                settings: Settings {
                    max_rows: 1000,
                    statement_timeout_ms: 8000,
                    max_connections: 2,
                    default_schema: "public".into(),
                },
            })
        }
    }

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
        assert!(pools.acquire(Uuid::new_v4()).await.is_err());
        let took = started.elapsed();
        assert!(
            took <= ACQUIRE_DEADLINE + Duration::from_millis(50),
            "took {took:?}"
        );
    }

    #[test]
    fn a_busy_pool_timing_out_is_not_a_moved_compute() {
        assert!(!is_connect_error(&sqlx::Error::PoolTimedOut, true));
        assert!(is_connect_error(&sqlx::Error::PoolTimedOut, false));
        let io = || sqlx::Error::Io(std::io::Error::from(std::io::ErrorKind::ConnectionRefused));
        assert!(is_connect_error(&io(), true));
        assert!(is_connect_error(&io(), false));
        assert!(!is_connect_error(&sqlx::Error::RowNotFound, false));
    }
}
