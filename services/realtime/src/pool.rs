//! Check connections (spec §4, P8, W20): `authenticator` pools per (branch, Postgres database)
//! from `GET /internal/data/branches/{id}/connection` (at `CHELABASE_INTERNAL_URL`, realtime's
//! bearer, the answer cached [`LOOKUP_TTL`]), TLS verified as the compute's `server_name`.
//!
//! Connections are capped per compute (branch) per instance across its databases at
//! [`PER_COMPUTE`] and close after [`IDLE_TIMEOUT`] idle: 3 instances × 4 + rest's pools + 20
//! walsenders stays under `max_connections = 100`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use chelabase_query::server_tls::ServerTls;
use serde::Deserialize;
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_postgres::{Client, TransactionStatus};
use uuid::Uuid;

use crate::check::{CheckError, CheckTarget};

pub const PER_COMPUTE: usize = 4;
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
pub const LOOKUP_TTL: Duration = Duration::from_secs(60);
/// How long a check waits for a connection of its compute's budget.
pub const ACQUIRE_WAIT: Duration = Duration::from_secs(2);
/// Connecting (TCP, TLS, authentication).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The connection route's answer.
pub const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct BudgetInner {
    used: Mutex<HashMap<Uuid, usize>>,
    freed: Notify,
}

/// The per-compute connection budget of one instance.
pub struct ComputeBudget {
    per_compute: usize,
    inner: Arc<BudgetInner>,
}

/// One connection's share of its compute's budget; dropping it gives it back.
pub struct BudgetPermit {
    branch_id: Uuid,
    inner: Arc<BudgetInner>,
}

impl std::fmt::Debug for BudgetPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BudgetPermit")
            .field("branch_id", &self.branch_id)
            .finish()
    }
}

impl Drop for BudgetPermit {
    fn drop(&mut self) {
        let mut used = self.inner.used.lock().unwrap();
        if let Some(n) = used.get_mut(&self.branch_id) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                used.remove(&self.branch_id);
            }
        }
        drop(used);
        self.inner.freed.notify_waiters();
    }
}

impl ComputeBudget {
    pub fn new(per_compute: usize) -> Self {
        Self {
            per_compute,
            inner: Arc::default(),
        }
    }
    /// A share of `branch_id`'s budget, `None` when all are in use (by any of its databases).
    pub fn try_take(&self, branch_id: Uuid) -> Option<BudgetPermit> {
        let mut used = self.inner.used.lock().unwrap();
        let n = used.entry(branch_id).or_insert(0);
        if *n >= self.per_compute {
            return None;
        }
        *n += 1;
        Some(BudgetPermit {
            branch_id,
            inner: self.inner.clone(),
        })
    }
    /// [`ComputeBudget::try_take`], waiting up to `wait` for a share to be given back.
    pub async fn take(&self, branch_id: Uuid, wait: Duration) -> Option<BudgetPermit> {
        let deadline = Instant::now() + wait;
        loop {
            let freed = self.inner.freed.notified();
            if let Some(p) = self.try_take(branch_id) {
                return Some(p);
            }
            if tokio::time::timeout_at(deadline, freed).await.is_err() {
                return self.try_take(branch_id);
            }
        }
    }
    pub fn in_use(&self, branch_id: Uuid) -> usize {
        self.inner
            .used
            .lock()
            .unwrap()
            .get(&branch_id)
            .copied()
            .unwrap_or(0)
    }
}

/// `GET /internal/data/branches/{id}/connection`'s answer (the fields realtime uses).
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct ConnInfo {
    pub host: String,
    pub port: u16,
    /// The compute certificate's name: `host` is then its IP, dialled, and TLS is verified as
    /// this name (U15).
    #[serde(default)]
    pub server_name: Option<String>,
    /// The compute's address on the `control` network (C4, SP11): where the replication session
    /// dials, since `chelabase_realtime` logs in only there. `null` or absent when the
    /// orchestrator found none (the session is then unavailable); the check pools use `host`.
    #[serde(default)]
    pub replication_host: Option<String>,
    pub user: String,
    pub password: String,
}

impl std::fmt::Debug for ConnInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnInfo")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("server_name", &self.server_name)
            .field("replication_host", &self.replication_host)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .finish()
    }
}

/// The orchestrator's connection route, cached per branch.
pub struct ConnLookup {
    http: reqwest::Client,
    base: String,
    token: String,
    cache: Mutex<HashMap<Uuid, (Instant, Arc<ConnInfo>)>>,
}

impl ConnLookup {
    /// `base_url` = `CHELABASE_INTERNAL_URL`, `token` = realtime's internal token.
    pub fn new(base_url: &str, token: &str) -> Self {
        Self {
            http: chelabase_http::client_builder()
                .timeout(LOOKUP_TIMEOUT)
                .build()
                .expect("realtime's HTTP client builds"),
            base: base_url.trim_end_matches('/').to_string(),
            token: token.to_string(),
            cache: Mutex::new(HashMap::new()),
        }
    }
    /// Asks the route (S25: `Err(Unavailable)` unless it answers 200), and caches the answer.
    pub async fn fetch(&self, branch_id: Uuid) -> Result<Arc<ConnInfo>, CheckError> {
        let url = format!(
            "{}/internal/data/branches/{branch_id}/connection",
            self.base
        );
        let resp = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| CheckError::Unavailable(format!("the connection route: {e}")))?;
        let status = resp.status().as_u16();
        if status != 200 {
            self.invalidate(branch_id);
            return Err(CheckError::Unavailable(format!(
                "the connection route answered {status}"
            )));
        }
        let info: ConnInfo = resp
            .json()
            .await
            .map_err(|e| CheckError::Unavailable(format!("the connection route's answer: {e}")))?;
        let info = Arc::new(info);
        self.cache
            .lock()
            .unwrap()
            .insert(branch_id, (Instant::now(), info.clone()));
        Ok(info)
    }
    /// The cached answer when it is younger than [`LOOKUP_TTL`], else [`ConnLookup::fetch`].
    pub async fn get(&self, branch_id: Uuid) -> Result<Arc<ConnInfo>, CheckError> {
        let cached = self
            .cache
            .lock()
            .unwrap()
            .get(&branch_id)
            .filter(|(at, _)| at.elapsed() < LOOKUP_TTL)
            .map(|(_, info)| info.clone());
        match cached {
            Some(info) => Ok(info),
            None => self.fetch(branch_id).await,
        }
    }
    pub fn invalidate(&self, branch_id: Uuid) {
        self.cache.lock().unwrap().remove(&branch_id);
    }
}

/// How a session to `database` of `info` connects (U15): a named target over TLS verified
/// against the platform CA (the IP dialled, the name verified); an unnamed one plain only when
/// there is no CA (tests). A target that can't be verified is never dialled.
pub fn connect_config(
    info: &ConnInfo,
    database: &str,
    tls: Option<&ServerTls>,
) -> Result<tokio_postgres::Config, CheckError> {
    let mut config = tokio_postgres::Config::new();
    config
        .port(info.port)
        .user(&info.user)
        .password(&info.password)
        .dbname(database)
        .application_name("chelabase-realtime")
        .connect_timeout(CONNECT_TIMEOUT);
    match (&info.server_name, tls) {
        (Some(name), Some(tls)) => {
            tls.configure(&mut config, &info.host, name)
                .map_err(|e| CheckError::Unavailable(format!("compute TLS: {e}")))?;
        }
        (Some(_), None) => {
            return Err(CheckError::Unavailable(
                "no CA to verify the compute's TLS against (CHELABASE_COMPUTE_CA_FILE)".into(),
            ));
        }
        (None, Some(_)) => {
            return Err(CheckError::Unavailable(
                "no certificate name to verify the compute's TLS against".into(),
            ));
        }
        (None, None) => {
            config.host(&info.host);
        }
    }
    Ok(config)
}

/// Connects with `config` (TLS through `tls` when given), the connection driven by a task.
pub async fn connect(
    config: &tokio_postgres::Config,
    tls: Option<&ServerTls>,
) -> Result<Client, tokio_postgres::Error> {
    match tls {
        Some(tls) => {
            let (client, connection) = config.connect(tls.connector()).await?;
            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    tracing::debug!(error = %e, "a check connection ended");
                }
            });
            Ok(client)
        }
        None => {
            let (client, connection) = config.connect(tokio_postgres::NoTls).await?;
            tokio::spawn(async move {
                if let Err(e) = connection.await {
                    tracing::debug!(error = %e, "a check connection ended");
                }
            });
            Ok(client)
        }
    }
}

type PoolKey = (Uuid, String);

struct IdleConn {
    client: Client,
    permit: BudgetPermit,
    info: Arc<ConnInfo>,
    since: Instant,
}

/// The `authenticator` connections of every compute this instance checks on.
pub struct CheckPools {
    lookup: Arc<ConnLookup>,
    tls: Option<ServerTls>,
    budget: ComputeBudget,
    idle: Mutex<HashMap<PoolKey, Vec<IdleConn>>>,
}

/// A connection out of the pool; dropping it gives it back (when its session is clean and
/// open) or closes it.
pub struct PooledClient {
    client: Option<Client>,
    permit: Option<BudgetPermit>,
    info: Arc<ConnInfo>,
    key: PoolKey,
    pools: Weak<CheckPools>,
}

impl std::ops::Deref for PooledClient {
    type Target = Client;
    fn deref(&self) -> &Client {
        self.client.as_ref().expect("held until dropped")
    }
}

impl Drop for PooledClient {
    fn drop(&mut self) {
        let (Some(client), Some(permit)) = (self.client.take(), self.permit.take()) else {
            return;
        };
        if client.is_closed() || client.transaction_status() != TransactionStatus::Idle {
            return;
        }
        if let Some(pools) = self.pools.upgrade() {
            pools
                .idle
                .lock()
                .unwrap()
                .entry(self.key.clone())
                .or_default()
                .push(IdleConn {
                    client,
                    permit,
                    info: self.info.clone(),
                    since: Instant::now(),
                });
        }
    }
}

impl CheckPools {
    pub fn new(lookup: Arc<ConnLookup>, tls: Option<ServerTls>) -> Arc<Self> {
        let pools = Arc::new(Self {
            lookup,
            tls,
            budget: ComputeBudget::new(PER_COMPUTE),
            idle: Mutex::new(HashMap::new()),
        });
        let weak = Arc::downgrade(&pools);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(IDLE_TIMEOUT / 3);
            loop {
                tick.tick().await;
                let Some(pools) = weak.upgrade() else {
                    return;
                };
                pools.reap();
            }
        });
        pools
    }

    pub fn lookup(&self) -> &Arc<ConnLookup> {
        &self.lookup
    }

    pub fn tls(&self) -> Option<&ServerTls> {
        self.tls.as_ref()
    }

    /// Closes connections idle for [`IDLE_TIMEOUT`] (and closed ones).
    fn reap(&self) {
        let mut idle = self.idle.lock().unwrap();
        for conns in idle.values_mut() {
            conns.retain(|c| c.since.elapsed() < IDLE_TIMEOUT && !c.client.is_closed());
        }
        idle.retain(|_, conns| !conns.is_empty());
    }

    /// Closes one idle connection of `branch_id`'s other databases (its budget share goes back).
    fn evict_other(&self, branch_id: Uuid, keep: &PoolKey) -> bool {
        let mut idle = self.idle.lock().unwrap();
        for (key, conns) in idle.iter_mut() {
            if key.0 == branch_id && key != keep && conns.pop().is_some() {
                return true;
            }
        }
        false
    }

    /// A connection to `target`'s database as `authenticator`.
    pub async fn get(self: &Arc<Self>, target: &CheckTarget) -> Result<PooledClient, CheckError> {
        let info = self.lookup.get(target.branch_id).await?;
        let key: PoolKey = (target.branch_id, target.pg_database.clone());
        let reused = {
            let mut idle = self.idle.lock().unwrap();
            let mut found = None;
            if let Some(conns) = idle.get_mut(&key) {
                while let Some(c) = conns.pop() {
                    if !c.client.is_closed() && c.since.elapsed() < IDLE_TIMEOUT && *c.info == *info
                    {
                        found = Some(c);
                        break;
                    }
                }
            }
            found
        };
        if let Some(c) = reused {
            return Ok(PooledClient {
                client: Some(c.client),
                permit: Some(c.permit),
                info: c.info,
                key,
                pools: Arc::downgrade(self),
            });
        }
        let permit = match self.budget.try_take(target.branch_id) {
            Some(p) => p,
            None => {
                let freed = self.evict_other(target.branch_id, &key);
                let wait = if freed { Duration::ZERO } else { ACQUIRE_WAIT };
                match self.budget.take(target.branch_id, wait).await {
                    Some(p) => p,
                    None => {
                        return Err(CheckError::Unavailable(
                            "every check connection of the compute is in use".into(),
                        ));
                    }
                }
            }
        };
        let config = connect_config(&info, &target.pg_database, self.tls.as_ref())?;
        let client = connect(&config, self.tls.as_ref()).await.map_err(|e| {
            self.lookup.invalidate(target.branch_id);
            CheckError::Unavailable(format!("connecting to the compute: {e}"))
        })?;
        Ok(PooledClient {
            client: Some(client),
            permit: Some(permit),
            info,
            key,
            pools: Arc::downgrade(self),
        })
    }
}
