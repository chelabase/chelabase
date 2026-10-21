//! Valkey, through realtime's own ACL user (S6: `~rt:* -@all +set +getdel +incr +decr +expire
//! +zadd +zrem +zremrangebyscore +zcard +ping`), and an in-memory store with the same semantics
//! for tests. Valkey down is a [`StoreError`]: no new tickets (503), open sockets unaffected
//! (spec §8.2).

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use redis::aio::MultiplexedConnection;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("valkey: {0}")]
pub struct StoreError(pub String);

/// The ticket commands: `SET key value EX ttl` and `GETDEL key`.
#[async_trait]
pub trait TicketStore: Send + Sync + 'static {
    async fn put(&self, key: &str, value: Vec<u8>, ttl: Duration) -> Result<(), StoreError>;
    /// The value, removed in the same command (a replay finds nothing).
    async fn take(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError>;
}

/// The counter commands P16 needs (S6), one method per Valkey command.
#[async_trait]
pub trait CounterStore: Send + Sync + 'static {
    /// `INCR key` with `EXPIRE key ttl NX`, atomically; the new count.
    async fn incr(&self, key: &str, ttl: Duration) -> Result<u64, StoreError>;
    /// `ZADD key score member` (adds or moves the member).
    async fn zadd(&self, key: &str, member: &str, score: u64) -> Result<(), StoreError>;
    async fn zrem(&self, key: &str, member: &str) -> Result<(), StoreError>;
    /// `ZREMRANGEBYSCORE key -inf max` (scores ≤ `max` go).
    async fn zremrangebyscore(&self, key: &str, max: u64) -> Result<(), StoreError>;
    async fn zcard(&self, key: &str) -> Result<u64, StoreError>;
    async fn expire(&self, key: &str, ttl: Duration) -> Result<(), StoreError>;
}

/// Valkey connection settings: `CHELABASE_VALKEY_URL` (no credentials in it),
/// `CHELABASE_VALKEY_USER`, `CHELABASE_VALKEY_PASSWORD[_FILE]` and `CHELABASE_VALKEY_TLS`
/// (`off` by default in the stack; `on` = `rediss://`, platform mode; P5, S7).
#[derive(Clone)]
pub struct ValkeyConfig {
    pub url: String,
    pub user: Option<String>,
    pub password: Option<String>,
    pub tls: bool,
}

impl std::fmt::Debug for ValkeyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValkeyConfig")
            .field("url", &self.url)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("tls", &self.tls)
            .finish()
    }
}

impl ValkeyConfig {
    /// The URL the client dials: the scheme from `tls` (`redis` or `rediss`), the user and the
    /// password percent-encoded, then the host, port and path of `url`.
    pub fn connection_url(&self) -> String {
        let rest = self
            .url
            .strip_prefix("rediss://")
            .or_else(|| self.url.strip_prefix("redis://"))
            .unwrap_or(&self.url);
        let scheme = if self.tls { "rediss" } else { "redis" };
        let creds = match (&self.user, &self.password) {
            (None, None) => String::new(),
            (user, password) => format!(
                "{}{}@",
                urlencoding::encode(user.as_deref().unwrap_or("")),
                password
                    .as_deref()
                    .map(|p| format!(":{}", urlencoding::encode(p)))
                    .unwrap_or_default()
            ),
        };
        format!("{scheme}://{creds}{rest}")
    }
}

/// How long connecting, or one command, may take before Valkey counts as down.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

/// Valkey's `EX`/`EXPIRE` take whole seconds; a sub-second TTL rounds up (never 0).
fn ttl_secs(ttl: Duration) -> u64 {
    (ttl.as_secs() + u64::from(ttl.subsec_nanos() > 0)).max(1)
}

/// A Valkey error as its code (`NOPERM`, `WRONGPASS`) or category: an error's detail can quote
/// the command's arguments (a sealed ticket), so it never goes further.
fn describe(e: &redis::RedisError) -> String {
    e.code().unwrap_or_else(|| e.category()).to_string()
}

/// Both traits on one multiplexed Valkey connection, dialled again after a failure.
pub struct ValkeyStore {
    client: redis::Client,
    conn: tokio::sync::Mutex<Option<MultiplexedConnection>>,
}

impl ValkeyStore {
    pub async fn connect(config: &ValkeyConfig) -> Result<Self, StoreError> {
        use redis::IntoConnectionInfo;
        if config.tls {
            // redis' rustls takes the process-wide provider; aws-lc-rs is the workspace's.
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        }
        let anonymous = ValkeyConfig {
            user: None,
            password: None,
            ..config.clone()
        };
        let info = anonymous
            .connection_url()
            .into_connection_info()
            .map_err(|e| StoreError(format!("CHELABASE_VALKEY_URL: {e}")))?;
        // The credentials go in as given, never through URL encoding.
        let mut settings = info.redis_settings().clone();
        if let Some(user) = &config.user {
            settings = settings.set_username(user);
        }
        if let Some(password) = &config.password {
            settings = settings.set_password(password);
        }
        let client = redis::Client::open(info.set_redis_settings(settings))
            .map_err(|e| StoreError(format!("CHELABASE_VALKEY_URL: {e}")))?;
        let store = Self {
            client,
            conn: tokio::sync::Mutex::new(None),
        };
        // Fail at startup on a wrong password or an unreachable server.
        let mut conn = store.connection().await?;
        store.run::<String>(&mut conn, redis::cmd("PING")).await?;
        Ok(store)
    }

    async fn connection(&self) -> Result<MultiplexedConnection, StoreError> {
        let mut guard = self.conn.lock().await;
        if let Some(conn) = guard.as_ref() {
            return Ok(conn.clone());
        }
        let conn = tokio::time::timeout(
            CONNECT_TIMEOUT,
            self.client.get_multiplexed_async_connection(),
        )
        .await
        .map_err(|_| StoreError("connect timed out".into()))?
        .map_err(|e| StoreError(format!("connect: {}", describe(&e))))?;
        *guard = Some(conn.clone());
        Ok(conn)
    }

    async fn run<T: redis::FromRedisValue + Send>(
        &self,
        conn: &mut MultiplexedConnection,
        cmd: redis::Cmd,
    ) -> Result<T, StoreError> {
        self.settle(cmd.query_async::<T>(conn)).await
    }

    /// A query's answer within [`COMMAND_TIMEOUT`]; a broken connection is dropped so the next
    /// command dials again.
    async fn settle<T>(
        &self,
        query: impl std::future::Future<Output = redis::RedisResult<T>>,
    ) -> Result<T, StoreError> {
        match tokio::time::timeout(COMMAND_TIMEOUT, query).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => {
                if e.is_io_error() || e.is_connection_dropped() || e.is_timeout() {
                    *self.conn.lock().await = None;
                }
                // The kind only: an error's detail can quote the command's arguments.
                Err(StoreError(describe(&e)))
            }
            Err(_) => {
                *self.conn.lock().await = None;
                Err(StoreError("command timed out".into()))
            }
        }
    }

    async fn command<T: redis::FromRedisValue + Send>(
        &self,
        cmd: redis::Cmd,
    ) -> Result<T, StoreError> {
        let mut conn = self.connection().await?;
        self.run(&mut conn, cmd).await
    }
}

#[async_trait]
impl TicketStore for ValkeyStore {
    async fn put(&self, key: &str, value: Vec<u8>, ttl: Duration) -> Result<(), StoreError> {
        let mut cmd = redis::cmd("SET");
        cmd.arg(key).arg(value).arg("EX").arg(ttl_secs(ttl));
        self.command::<()>(cmd).await
    }
    async fn take(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let mut cmd = redis::cmd("GETDEL");
        cmd.arg(key);
        self.command::<Option<Vec<u8>>>(cmd).await
    }
}

#[async_trait]
impl CounterStore for ValkeyStore {
    /// `INCR` and `EXPIRE … NX` in one `MULTI`/`EXEC`: both apply or neither does, so a
    /// connection that breaks halfway never leaves a count without a TTL (and `NX` gives a
    /// key that somehow has none its TTL on the next increment, without extending a window).
    async fn incr(&self, key: &str, ttl: Duration) -> Result<u64, StoreError> {
        let mut pipe = redis::pipe();
        pipe.atomic()
            .cmd("INCR")
            .arg(key)
            .cmd("EXPIRE")
            .arg(key)
            .arg(ttl_secs(ttl))
            .arg("NX")
            .ignore();
        let mut conn = self.connection().await?;
        let (n,): (u64,) = self.settle(pipe.query_async(&mut conn)).await?;
        Ok(n)
    }
    async fn zadd(&self, key: &str, member: &str, score: u64) -> Result<(), StoreError> {
        let mut cmd = redis::cmd("ZADD");
        cmd.arg(key).arg(score).arg(member);
        self.command::<()>(cmd).await
    }
    async fn zrem(&self, key: &str, member: &str) -> Result<(), StoreError> {
        let mut cmd = redis::cmd("ZREM");
        cmd.arg(key).arg(member);
        self.command::<()>(cmd).await
    }
    async fn zremrangebyscore(&self, key: &str, max: u64) -> Result<(), StoreError> {
        let mut cmd = redis::cmd("ZREMRANGEBYSCORE");
        cmd.arg(key).arg("-inf").arg(max);
        self.command::<()>(cmd).await
    }
    async fn zcard(&self, key: &str) -> Result<u64, StoreError> {
        let mut cmd = redis::cmd("ZCARD");
        cmd.arg(key);
        self.command(cmd).await
    }
    async fn expire(&self, key: &str, ttl: Duration) -> Result<(), StoreError> {
        let mut cmd = redis::cmd("EXPIRE");
        cmd.arg(key).arg(ttl_secs(ttl));
        self.command::<()>(cmd).await
    }
}

enum Val {
    Str(Vec<u8>),
    ZSet(BTreeMap<String, u64>),
}

struct Entry {
    val: Val,
    expires: Option<tokio::time::Instant>,
}

impl Entry {
    fn live(&self, now: tokio::time::Instant) -> bool {
        self.expires.is_none_or(|t| t > now)
    }
}

/// An in-memory Valkey for tests: key expiry on `tokio::time::Instant` (so paused test time
/// applies), [`MemoryStore::fail`] makes every command fail as if Valkey were down, and every
/// command yields to the scheduler once before it runs (so concurrent callers interleave as
/// they would across instances).
#[derive(Default)]
pub struct MemoryStore {
    data: Mutex<HashMap<String, Entry>>,
    down: AtomicBool,
}

impl MemoryStore {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self::default())
    }
    /// Every command fails while `down`.
    pub fn fail(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }
    /// A string key's value, unless expired (a test's look at what was stored).
    pub fn get_raw(&self, key: &str) -> Option<Vec<u8>> {
        let now = tokio::time::Instant::now();
        match self.data.lock().unwrap().get(key) {
            Some(e) if e.live(now) => match &e.val {
                Val::Str(v) => Some(v.clone()),
                Val::ZSet(_) => None,
            },
            _ => None,
        }
    }
    /// Sets a string key without a TTL (a test's tampering).
    pub fn set_raw(&self, key: &str, value: Vec<u8>) {
        self.data.lock().unwrap().insert(
            key.to_string(),
            Entry {
                val: Val::Str(value),
                expires: None,
            },
        );
    }
    /// The time a key has left, `None` without a TTL or when absent.
    pub fn ttl_of(&self, key: &str) -> Option<Duration> {
        let now = tokio::time::Instant::now();
        let data = self.data.lock().unwrap();
        let e = data.get(key).filter(|e| e.live(now))?;
        e.expires.map(|t| t.saturating_duration_since(now))
    }
    /// Every live key, sorted.
    pub fn keys(&self) -> Vec<String> {
        let now = tokio::time::Instant::now();
        let mut keys: Vec<String> = self
            .data
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, e)| e.live(now))
            .map(|(k, _)| k.clone())
            .collect();
        keys.sort();
        keys
    }

    /// One command: a yield, the down check, then `f` on the live entries.
    async fn cmd<T>(
        &self,
        f: impl FnOnce(&mut HashMap<String, Entry>, tokio::time::Instant) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        tokio::task::yield_now().await;
        if self.down.load(Ordering::SeqCst) {
            return Err(StoreError("connection refused".into()));
        }
        let now = tokio::time::Instant::now();
        let mut data = self.data.lock().unwrap();
        data.retain(|_, e| e.live(now));
        f(&mut data, now)
    }
}

fn wrong_type() -> StoreError {
    StoreError("WRONGTYPE".into())
}

#[async_trait]
impl TicketStore for MemoryStore {
    async fn put(&self, key: &str, value: Vec<u8>, ttl: Duration) -> Result<(), StoreError> {
        self.cmd(|data, now| {
            data.insert(
                key.to_string(),
                Entry {
                    val: Val::Str(value),
                    expires: Some(now + ttl),
                },
            );
            Ok(())
        })
        .await
    }
    async fn take(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        self.cmd(|data, _| match data.remove(key) {
            None => Ok(None),
            Some(Entry {
                val: Val::Str(v), ..
            }) => Ok(Some(v)),
            Some(other) => {
                data.insert(key.to_string(), other);
                Err(wrong_type())
            }
        })
        .await
    }
}

#[async_trait]
impl CounterStore for MemoryStore {
    async fn incr(&self, key: &str, ttl: Duration) -> Result<u64, StoreError> {
        self.cmd(|data, now| {
            let entry = data.entry(key.to_string()).or_insert(Entry {
                val: Val::Str(b"0".to_vec()),
                expires: None,
            });
            let Val::Str(v) = &mut entry.val else {
                return Err(wrong_type());
            };
            let n: u64 = std::str::from_utf8(v)
                .ok()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| StoreError("not an integer".into()))?;
            let n = n + 1;
            *v = n.to_string().into_bytes();
            // `EXPIRE … NX`, as the Valkey store sends it.
            if entry.expires.is_none() {
                entry.expires = Some(now + ttl);
            }
            Ok(n)
        })
        .await
    }
    async fn zadd(&self, key: &str, member: &str, score: u64) -> Result<(), StoreError> {
        self.cmd(|data, _| {
            let entry = data.entry(key.to_string()).or_insert(Entry {
                val: Val::ZSet(BTreeMap::new()),
                expires: None,
            });
            let Val::ZSet(z) = &mut entry.val else {
                return Err(wrong_type());
            };
            z.insert(member.to_string(), score);
            Ok(())
        })
        .await
    }
    async fn zrem(&self, key: &str, member: &str) -> Result<(), StoreError> {
        self.cmd(|data, _| {
            if let Some(entry) = data.get_mut(key) {
                let Val::ZSet(z) = &mut entry.val else {
                    return Err(wrong_type());
                };
                z.remove(member);
                if z.is_empty() {
                    data.remove(key);
                }
            }
            Ok(())
        })
        .await
    }
    async fn zremrangebyscore(&self, key: &str, max: u64) -> Result<(), StoreError> {
        self.cmd(|data, _| {
            if let Some(entry) = data.get_mut(key) {
                let Val::ZSet(z) = &mut entry.val else {
                    return Err(wrong_type());
                };
                z.retain(|_, score| *score > max);
                if z.is_empty() {
                    data.remove(key);
                }
            }
            Ok(())
        })
        .await
    }
    async fn zcard(&self, key: &str) -> Result<u64, StoreError> {
        self.cmd(|data, _| match data.get(key) {
            None => Ok(0),
            Some(Entry {
                val: Val::ZSet(z), ..
            }) => Ok(z.len() as u64),
            Some(_) => Err(wrong_type()),
        })
        .await
    }
    async fn expire(&self, key: &str, ttl: Duration) -> Result<(), StoreError> {
        self.cmd(|data, now| {
            if let Some(entry) = data.get_mut(key) {
                entry.expires = Some(now + ttl);
            }
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttls_round_up_to_whole_seconds_and_never_to_zero() {
        assert_eq!(ttl_secs(Duration::from_secs(30)), 30);
        assert_eq!(ttl_secs(Duration::from_millis(1200)), 2);
        assert_eq!(ttl_secs(Duration::from_millis(1)), 1);
        assert_eq!(ttl_secs(Duration::ZERO), 1, "EX 0 is an error in Valkey");
    }

    #[test]
    fn a_tls_url_without_credentials_keeps_its_host_and_path() {
        let c = ValkeyConfig {
            url: "redis://valkey:6380/2".into(),
            user: None,
            password: None,
            tls: true,
        };
        assert_eq!(c.connection_url(), "rediss://valkey:6380/2");
        let already = ValkeyConfig {
            url: "rediss://valkey:6380".into(),
            tls: false,
            ..c
        };
        assert_eq!(
            already.connection_url(),
            "redis://valkey:6380",
            "tls decides"
        );
    }
}
