//! Check connections (spec §4, P8, W20): pools per (branch, Postgres database, login), the
//! login `authenticator` or, for a check as an elevated role, the elevated login
//! ([`chelabase_query::roles`] holds the rule; [`crate::check`] applies it), both within the
//! compute's one budget,
//! from `GET /internal/data/branches/{id}/connection` (at `CHELABASE_INTERNAL_URL`, realtime's
//! bearer, the answer cached [`LOOKUP_TTL`]), TLS verified as the compute's `server_name`.
//!
//! Connections are capped per compute (branch) per instance across its databases at
//! [`PER_COMPUTE`] and close after [`IDLE_TIMEOUT`] idle: 3 instances × 4 + rest's pools + 20
//! walsenders stays under `max_connections = 100`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use chelabase_query::roles::Login;
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
/// A cached answer without the elevated login's credentials is asked for again when it is at
/// least this old (the worker may have stored them since).
pub const REFETCH_AFTER: Duration = Duration::from_secs(2);
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
    /// The elevated login's credentials; absent until the branch's bootstrap stored them.
    #[serde(default)]
    pub elevated: Option<Credentials>,
}

/// A login's name and password.
#[derive(Clone, PartialEq, Eq, Deserialize)]
pub struct Credentials {
    pub user: String,
    pub password: String,
}

impl ConnInfo {
    /// What `login` connects with (`None`: the branch has no elevated login stored).
    pub fn credentials(&self, login: Login) -> Option<(&str, &str)> {
        match login {
            Login::Authenticator => Some((&self.user, &self.password)),
            Login::Elevated => self
                .elevated
                .as_ref()
                .map(|c| (c.user.as_str(), c.password.as_str())),
        }
    }
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
            .field("elevated", &self.elevated.as_ref().map(|c| c.user.as_str()))
            .finish()
    }
}

/// The orchestrator's connection route, cached per branch.
pub struct ConnLookup {
    http: chelabase_http::Client,
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
    /// The cached answer when it is younger than `age`, else [`ConnLookup::fetch`]: a second
    /// look for something the cached answer lacks, at most once per `age` per branch.
    pub async fn get_newer_than(&self, branch_id: Uuid, age: Duration) -> Result<Arc<ConnInfo>, CheckError> {
        let cached = self
            .cache
            .lock()
            .unwrap()
            .get(&branch_id)
            .filter(|(at, _)| at.elapsed() < age)
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
    connect_config_as(info, (&info.user, &info.password), database, tls)
}

/// [`connect_config`] with a login's `credentials` ([`ConnInfo::credentials`]).
pub fn connect_config_as(
    info: &ConnInfo,
    (user, password): (&str, &str),
    database: &str,
    tls: Option<&ServerTls>,
) -> Result<tokio_postgres::Config, CheckError> {
    let mut config = tokio_postgres::Config::new();
    config
        .port(info.port)
        .user(user)
        .password(password)
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

type PoolKey = (Uuid, String, Login);

struct IdleConn {
    client: Client,
    permit: BudgetPermit,
    info: Arc<ConnInfo>,
    since: Instant,
}

/// The check connections (of either login) of every compute this instance checks on.
pub struct CheckPools {
    lookup: Arc<ConnLookup>,
    tls: Option<ServerTls>,
    budget: ComputeBudget,
    idle: Mutex<HashMap<PoolKey, Vec<IdleConn>>>,
}

/// A connection out of the pool; dropping it gives it back (when its session is clean and
/// open) or closes it. A connection whose reset or session check failed is marked broken
/// ([`chelabase_query::QueryClient::mark_broken`]) and closed when dropped, though its
/// session may look idle: it never serves another check.
pub struct PooledClient {
    client: Option<Client>,
    permit: Option<BudgetPermit>,
    broken: std::sync::atomic::AtomicBool,
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

impl chelabase_query::QueryClient for PooledClient {
    fn client(&self) -> &Client {
        self
    }

    fn prepare_cached(
        &self,
        sql: &str,
        types: &[tokio_postgres::types::Type],
    ) -> impl Future<Output = Result<tokio_postgres::Statement, tokio_postgres::Error>> + Send {
        Client::prepare_typed(self, sql, types)
    }

    fn mark_broken(&self) {
        self.broken.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Drop for PooledClient {
    fn drop(&mut self) {
        let (Some(client), Some(permit)) = (self.client.take(), self.permit.take()) else {
            return;
        };
        if *self.broken.get_mut()
            || client.is_closed()
            || client.transaction_status() != TransactionStatus::Idle
        {
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

    /// Closes one idle connection of `branch_id`'s other pools, whichever their database or
    /// login (its budget share goes back).
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
        self.get_as(target, Login::Authenticator).await
    }

    /// A connection to `target`'s database as `login`, within the compute's one budget. For
    /// the elevated login on a branch whose cached answer has no credentials for it (cached
    /// before the worker stored them), the route is asked again once; still none is
    /// `Unavailable`, and nothing is opened as `authenticator` instead.
    pub async fn get_as(
        self: &Arc<Self>,
        target: &CheckTarget,
        login: Login,
    ) -> Result<PooledClient, CheckError> {
        let mut info = self.lookup.get(target.branch_id).await?;
        if info.credentials(login).is_none() {
            info = self.lookup.get_newer_than(target.branch_id, REFETCH_AFTER).await?;
        }
        let Some(credentials) = info.credentials(login) else {
            return Err(CheckError::Unavailable(
                "the branch has no elevated login to check as".into(),
            ));
        };
        let key: PoolKey = (target.branch_id, target.pg_database.clone(), login);
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
                broken: Default::default(),
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
        let config = connect_config_as(&info, credentials, &target.pg_database, self.tls.as_ref())?;
        let client = connect(&config, self.tls.as_ref()).await.map_err(|e| {
            self.lookup.invalidate(target.branch_id);
            CheckError::Unavailable(format!("connecting to the compute: {e}"))
        })?;
        Ok(PooledClient {
            client: Some(client),
            permit: Some(permit),
            broken: Default::default(),
            info,
            key,
            pools: Arc::downgrade(self),
        })
    }
}

#[cfg(test)]
mod login_tests {
    use super::*;

    fn info(elevated: bool) -> ConnInfo {
        let mut v = serde_json::json!({
            "host": "127.0.0.1", "port": 5432, "user": "authenticator", "password": "pw-secret",
        });
        if elevated {
            v["elevated"] = serde_json::json!({
                "user": chelabase_query::roles::ELEVATED_LOGIN, "password": "el-secret",
            });
        }
        serde_json::from_value(v).unwrap()
    }

    /// Each login's pool connects with that login's credentials; a branch without the
    /// elevated login has none for it; neither password is printed.
    #[test]
    fn each_login_connects_with_its_own_credentials() {
        let with = info(true);
        assert_eq!(info(false).credentials(Login::Elevated), None);
        assert_eq!(with.credentials(Login::Authenticator), Some(("authenticator", "pw-secret")));
        let credentials = with.credentials(Login::Elevated).unwrap();
        assert_eq!(credentials, (chelabase_query::roles::ELEVATED_LOGIN, "el-secret"));
        let config = connect_config_as(&with, credentials, "main", None).unwrap();
        assert_eq!(config.get_user(), Some(chelabase_query::roles::ELEVATED_LOGIN));
        assert_eq!(config.get_password(), Some(&b"el-secret"[..]));
        let config = connect_config(&with, "main", None).unwrap();
        assert_eq!(config.get_user(), Some("authenticator"));
        let printed = format!("{with:?}");
        assert!(!printed.contains("el-secret") && !printed.contains("pw-secret"), "{printed}");
    }

    /// Both logins' pools of a compute draw on its one budget, and a pool's key carries the
    /// login, so a connection of one login is never handed out for the other.
    #[test]
    fn both_logins_share_the_compute_budget() {
        let budget = ComputeBudget::new(PER_COMPUTE);
        let branch = Uuid::new_v4();
        let held: Vec<_> = (0..PER_COMPUTE).map(|_| budget.try_take(branch).unwrap()).collect();
        assert!(budget.try_take(branch).is_none());
        drop(held);
        let (a, e): (PoolKey, PoolKey) = (
            (branch, "main".into(), Login::Authenticator),
            (branch, "main".into(), Login::Elevated),
        );
        assert_ne!(a, e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::RESET_SQL;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;

    /// A Postgres stand-in on 127.0.0.1 that answers simple queries as a transaction would:
    /// `BEGIN…` leaves a transaction open, `COMMIT`/`ROLLBACK` closes it, and anything else
    /// (the trailing reset) succeeds, or fails with the session left idle and open when
    /// `fail_reset`. It counts the connections it accepted and reports each one that ended.
    struct FakePg {
        port: u16,
        accepted: Arc<AtomicUsize>,
        ended: mpsc::UnboundedReceiver<()>,
    }

    fn ready(status: u8) -> [u8; 6] {
        [b'Z', 0, 0, 0, 5, status]
    }

    fn error_response() -> Vec<u8> {
        let mut body = Vec::new();
        for (field, value) in [(b'S', "ERROR"), (b'V', "ERROR"), (b'C', "XX000"), (b'M', "no reset")] {
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

    async fn fake_pg(fail_reset: bool) -> FakePg {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = Arc::new(AtomicUsize::new(0));
        let (ended_tx, ended) = mpsc::unbounded_channel();
        let count = accepted.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                count.fetch_add(1, Ordering::SeqCst);
                let ended_tx = ended_tx.clone();
                tokio::spawn(async move {
                    let len = socket.read_i32().await.unwrap();
                    let mut startup = vec![0; usize::try_from(len).unwrap() - 4];
                    socket.read_exact(&mut startup).await.unwrap();
                    // AuthenticationOk, then ReadyForQuery (idle).
                    socket.write_all(&[b'R', 0, 0, 0, 8, 0, 0, 0, 0]).await.unwrap();
                    socket.write_all(&ready(b'I')).await.unwrap();
                    while let Ok(kind) = socket.read_u8().await {
                        let Ok(len) = socket.read_i32().await else { break };
                        let mut body = vec![0; usize::try_from(len).unwrap() - 4];
                        if socket.read_exact(&mut body).await.is_err() || kind != b'Q' {
                            break;
                        }
                        let sql = String::from_utf8_lossy(&body);
                        let reply = if sql.starts_with("BEGIN") {
                            ready(b'T').to_vec()
                        } else if sql.starts_with("COMMIT") || sql.starts_with("ROLLBACK") {
                            ready(b'I').to_vec()
                        } else if fail_reset {
                            [error_response(), ready(b'I').to_vec()].concat()
                        } else {
                            ready(b'I').to_vec()
                        };
                        if socket.write_all(&reply).await.is_err() {
                            break;
                        }
                    }
                    let _ = ended_tx.send(());
                });
            }
        });
        FakePg {
            port,
            accepted,
            ended,
        }
    }

    /// Pools whose lookup already holds `fake`'s address for one branch (no route asked).
    fn pools_for(fake: &FakePg) -> (Arc<CheckPools>, CheckTarget) {
        let lookup = Arc::new(ConnLookup::new("http://127.0.0.1:1", "token"));
        let info: ConnInfo = serde_json::from_value(serde_json::json!({
            "host": "127.0.0.1", "port": fake.port, "user": "authenticator", "password": "pw",
        }))
        .unwrap();
        let target = CheckTarget {
            database_id: Uuid::new_v4(),
            branch_id: Uuid::new_v4(),
            pg_database: "main".into(),
        };
        lookup
            .cache
            .lock()
            .unwrap()
            .insert(target.branch_id, (Instant::now(), Arc::new(info)));
        (CheckPools::new(lookup, None), target)
    }

    fn idle_count(pools: &CheckPools) -> usize {
        pools.idle.lock().unwrap().values().map(Vec::len).sum()
    }

    async fn check(pools: &Arc<CheckPools>, target: &CheckTarget) {
        let client = pools.get(target).await.unwrap();
        let out = chelabase_query::engine::with_transaction(
            &client,
            RESET_SQL,
            "test",
            async |_| Ok(()),
        )
        .await;
        assert!(out.is_ok(), "the trailing reset never changes the result: {out:?}");
    }

    /// A check whose trailing reset failed (the session idle and open, so it looks clean):
    /// its connection is closed when dropped, its budget share goes back, and the next
    /// checkout opens a fresh one.
    #[tokio::test]
    async fn a_failed_reset_discards_the_connection() {
        let mut fake = fake_pg(true).await;
        let (pools, target) = pools_for(&fake);
        check(&pools, &target).await;
        assert_eq!(idle_count(&pools), 0, "not returned to the pool");
        assert!(
            pools.budget.inner.used.lock().unwrap().get(&target.branch_id).is_none(),
            "its budget share went back"
        );
        tokio::time::timeout(Duration::from_secs(5), fake.ended.recv())
            .await
            .expect("the connection is closed")
            .unwrap();
        let _next = pools.get(&target).await.unwrap();
        assert_eq!(fake.accepted.load(Ordering::SeqCst), 2, "a fresh connection");
    }

    /// A clean check gives its connection back, and the next checkout reuses it.
    #[tokio::test]
    async fn a_clean_check_returns_the_connection() {
        let fake = fake_pg(false).await;
        let (pools, target) = pools_for(&fake);
        check(&pools, &target).await;
        assert_eq!(idle_count(&pools), 1, "returned to the pool");
        let _next = pools.get(&target).await.unwrap();
        assert_eq!(fake.accepted.load(Ordering::SeqCst), 1, "reused");
        assert_eq!(idle_count(&pools), 0);
    }
}
