//! A bounded map of token buckets, shared by the gateway's own limiters (refused `/mcp` bearers,
//! #395; data API and storage requests, #322).
//!
//! - A bucket holds at most `burst` tokens and gains one every `refill_every`. Only a key whose
//!   bucket isn't full has an entry: a full bucket is the default, so a key that never spent a
//!   token has nothing to lose or evict.
//! - At most `capacity` entries. When a new key needs one and the map is full, refilled entries
//!   are dropped first; then, down to seven eighths of the capacity, those with the most tokens
//!   left go (the least-limited keys, which forgetting frees the least). So a full map costs
//!   one pass per `capacity / 8` new keys, however many had refilled. A new key is always
//!   admitted, so a full map never stops limiting new keys, and a limited key is only forgotten
//!   once nearly every other entry is as limited as it is.
//! - The clock is injectable (tests move it by hand). No cache crate: its eviction would decide
//!   which limited key is forgotten.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// One key's bucket: its tokens at `at` (below the burst, else it has no entry).
#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

pub struct TokenBuckets<K> {
    buckets: Mutex<HashMap<K, Bucket>>,
    burst: f64,
    refill_every: Duration,
    capacity: usize,
    /// `Instant::now`, or a test's own clock.
    clock: Arc<dyn Fn() -> Instant + Send + Sync>,
}

impl<K: Hash + Eq + Clone> TokenBuckets<K> {
    /// `burst` tokens at most (at least 1), one more every `refill_every` (at least 1 µs), at
    /// most `capacity` entries (at least 1).
    pub fn new(burst: u32, refill_every: Duration, capacity: usize) -> Self {
        Self {
            buckets: Mutex::new(HashMap::new()),
            burst: f64::from(burst.max(1)),
            refill_every: refill_every.max(Duration::from_micros(1)),
            capacity: capacity.max(1),
            clock: Arc::new(Instant::now),
        }
    }

    /// Replaces the clock (tests).
    pub fn with_clock(mut self, clock: impl Fn() -> Instant + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// Whether `key` has a token; when not, how long until it has one. Takes nothing.
    pub fn wait(&self, key: &K) -> Result<(), Duration> {
        let now = (self.clock)();
        let mut buckets = self.lock();
        let Some(bucket) = buckets.get(key) else {
            return Ok(());
        };
        let tokens = self.tokens(bucket, now);
        if tokens >= self.burst {
            buckets.remove(key);
            return Ok(());
        }
        if tokens >= 1.0 {
            return Ok(());
        }
        Err(self.until_one(tokens))
    }

    /// Takes one token from `key` whether or not it has one: a bucket already empty goes into
    /// debt, down to `-burst`.
    pub fn charge(&self, key: K) {
        let now = (self.clock)();
        let mut buckets = self.lock();
        let tokens = self.current_or_admit(&mut buckets, &key, now);
        let tokens = (tokens - 1.0).max(-self.burst);
        buckets.insert(key, Bucket { tokens, at: now });
    }

    /// Takes one token from `key` when it has one; when not, takes nothing and says how long
    /// until it has one.
    pub fn try_take(&self, key: K) -> Result<(), Duration> {
        let now = (self.clock)();
        let mut buckets = self.lock();
        let tokens = self.current_or_admit(&mut buckets, &key, now);
        if tokens < 1.0 {
            return Err(self.until_one(tokens));
        }
        buckets.insert(
            key,
            Bucket {
                tokens: tokens - 1.0,
                at: now,
            },
        );
        Ok(())
    }

    /// How many keys have an entry.
    pub fn tracked(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<K, Bucket>> {
        self.buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `key`'s tokens at `now`; a key without an entry has a full bucket, and room is made for
    /// its entry first when the map is full.
    fn current_or_admit(&self, buckets: &mut HashMap<K, Bucket>, key: &K, now: Instant) -> f64 {
        match buckets.get(key) {
            Some(bucket) => self.tokens(bucket, now),
            None => {
                if buckets.len() >= self.capacity {
                    self.make_room(buckets, now);
                }
                self.burst
            }
        }
    }

    /// How long a bucket at `tokens` (below 1) takes to reach one token.
    fn until_one(&self, tokens: f64) -> Duration {
        self.refill_every.mul_f64(1.0 - tokens)
    }

    /// `bucket`'s tokens at `now`, at most the burst.
    fn tokens(&self, bucket: &Bucket, now: Instant) -> f64 {
        let refilled = now.saturating_duration_since(bucket.at).as_secs_f64()
            / self.refill_every.as_secs_f64();
        (bucket.tokens + refilled).min(self.burst)
    }

    /// Makes room in a full map: drops refilled entries, then, while the map is still above
    /// seven eighths of its capacity, the entries with the most tokens left. Always ending at
    /// most there (however many entries had refilled) bounds the cost: a full pass happens at
    /// most once per `capacity / 8` new keys.
    fn make_room(&self, buckets: &mut HashMap<K, Bucket>, now: Instant) {
        buckets.retain(|_, bucket| self.tokens(bucket, now) < self.burst);
        let target = self.capacity - (self.capacity / 8).max(1);
        if buckets.len() <= target {
            return;
        }
        let evict = buckets.len() - target;
        let mut by_tokens: Vec<(f64, K)> = buckets
            .iter()
            .map(|(key, bucket)| (self.tokens(bucket, now), key.clone()))
            .collect();
        let first = by_tokens.len() - evict;
        by_tokens.select_nth_unstable_by(first, |a, b| a.0.total_cmp(&b.0));
        for (_, key) in &by_tokens[first..] {
            buckets.remove(key);
        }
    }
}

/// `Retry-After` for a wait: whole seconds, rounded up, at least 1.
pub fn retry_after_secs(wait: Duration) -> u64 {
    let secs = wait.as_secs() + u64::from(wait.subsec_nanos() > 0);
    secs.max(1)
}
