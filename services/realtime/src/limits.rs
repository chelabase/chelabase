//! Limits (spec §8.1, R15, P16, S6, W33, W35).
//!
//! - Per socket, in memory: joins 5/s ([`RateLimiter`]), channels 50, presence 20/s (latest
//!   wins, `presence::PresenceThrottle`), frames 64 KB.
//! - Per member, in Valkey across instances: sockets 20 ([`SocketCounter`], the sorted set
//!   `rt:cnt:sock:<sub>`, member = socket id, score = expiry in unix ms; refreshed every 30 s,
//!   entries past their score trimmed on every count, so a crashed instance's sockets heal in
//!   90 s), tickets 10/s ([`TicketRate`], `rt:cnt:tix:<sub>:<unix second>` with `INCR` and a
//!   short TTL).
//! - Concurrent admissions on several instances never admit more than the limit: the socket is
//!   added first, then counted, and taken back out when over.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::store::{CounterStore, StoreError};

/// A socket entry lives this long past its last refresh.
pub const SOCKET_ENTRY_TTL: Duration = Duration::from_secs(90);
/// How often a live socket refreshes its entry.
pub const SOCKET_REFRESH: Duration = Duration::from_secs(30);

/// How often an admission that lost a race with concurrent ones (everyone added, everyone
/// over) tries again, and the most it waits before each try.
const ADMIT_ATTEMPTS: u32 = 4;
const ADMIT_JITTER_MS: u64 = 25;

/// How long a ticket-rate counter outlives its second.
const TICKET_RATE_TTL: Duration = Duration::from_secs(2);

/// `rt:cnt:sock:<sub>`.
pub fn socket_key(sub: &str) -> String {
    format!("rt:cnt:sock:{sub}")
}

/// `rt:cnt:tix:<sub>:<unix second>`.
pub fn ticket_rate_key(sub: &str, unix_second: u64) -> String {
    format!("rt:cnt:tix:{sub}:{unix_second}")
}

/// A sliding one-second window: at most `per_sec` events in any second.
#[derive(Debug, Clone)]
pub struct RateLimiter {
    per_sec: u32,
    window: Duration,
    events: VecDeque<Instant>,
}

impl RateLimiter {
    pub fn new(per_sec: u32) -> Self {
        Self {
            per_sec,
            window: Duration::from_secs(1),
            events: VecDeque::new(),
        }
    }
    pub fn allow(&mut self, now: Instant) -> bool {
        while self
            .events
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= self.window)
        {
            self.events.pop_front();
        }
        if self.events.len() < self.per_sec as usize {
            self.events.push_back(now);
            true
        } else {
            false
        }
    }
}

fn jitter() -> Duration {
    let mut b = [0u8; 2];
    getrandom::fill(&mut b).expect("OS random number generator");
    Duration::from_millis(1 + u64::from(u16::from_le_bytes(b)) % ADMIT_JITTER_MS)
}

/// A member's sockets across instances.
#[derive(Clone)]
pub struct SocketCounter {
    store: Arc<dyn CounterStore>,
    limit: usize,
}

impl SocketCounter {
    pub fn new(store: Arc<dyn CounterStore>, limit: usize) -> Self {
        Self { store, limit }
    }
    /// Admits the socket (`now_ms`: unix milliseconds) unless the member already has `limit`
    /// live sockets.
    ///
    /// The socket goes in first, then the set is counted, and an admission that finds the set
    /// over the limit takes itself back out: of any group of concurrent admissions the one that
    /// added last always sees every other, so the limit is never exceeded. When everyone added
    /// at once and everyone is over, those that find room after taking themselves out try again
    /// after a random pause, so one of them gets in.
    pub async fn admit(&self, sub: &str, socket_id: &str, now_ms: u64) -> Result<bool, StoreError> {
        let key = socket_key(sub);
        let ttl_ms = SOCKET_ENTRY_TTL.as_millis() as u64;
        for attempt in 0..ADMIT_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(jitter() * attempt).await;
            }
            self.store.zremrangebyscore(&key, now_ms).await?;
            self.store.zadd(&key, socket_id, now_ms + ttl_ms).await?;
            self.store.expire(&key, SOCKET_ENTRY_TTL).await?;
            if self.store.zcard(&key).await? as usize <= self.limit {
                return Ok(true);
            }
            self.store.zrem(&key, socket_id).await?;
            if self.store.zcard(&key).await? as usize >= self.limit {
                // Truly full (not a race between admissions).
                return Ok(false);
            }
        }
        Ok(false)
    }
    /// Pushes the socket's expiry to `now_ms` + [`SOCKET_ENTRY_TTL`].
    pub async fn refresh(&self, sub: &str, socket_id: &str, now_ms: u64) -> Result<(), StoreError> {
        let key = socket_key(sub);
        self.store
            .zadd(
                &key,
                socket_id,
                now_ms + SOCKET_ENTRY_TTL.as_millis() as u64,
            )
            .await?;
        self.store.expire(&key, SOCKET_ENTRY_TTL).await
    }
    pub async fn release(&self, sub: &str, socket_id: &str) -> Result<(), StoreError> {
        self.store.zrem(&socket_key(sub), socket_id).await
    }
    /// The member's live sockets at `now_ms` (expired entries trimmed first).
    pub async fn count(&self, sub: &str, now_ms: u64) -> Result<u64, StoreError> {
        let key = socket_key(sub);
        self.store.zremrangebyscore(&key, now_ms).await?;
        self.store.zcard(&key).await
    }
}

/// W35: tickets per member per second, across instances.
#[derive(Clone)]
pub struct TicketRate {
    store: Arc<dyn CounterStore>,
    per_sec: u64,
}

impl TicketRate {
    pub fn new(store: Arc<dyn CounterStore>, per_sec: u64) -> Self {
        Self { store, per_sec }
    }
    pub async fn allow(&self, sub: &str, now_ms: u64) -> Result<bool, StoreError> {
        let n = self
            .store
            .incr(&ticket_rate_key(sub, now_ms / 1000), TICKET_RATE_TTL)
            .await?;
        Ok(n <= self.per_sec)
    }
}
