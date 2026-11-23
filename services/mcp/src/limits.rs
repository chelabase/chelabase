//! What bounds a tool call: its wall-clock time, the size of what it returns, how many a member
//! may make a minute, and how many run at once per member and overall.
//!
//! A refused call is answered at once (`rate_limited` with `retry_after_ms`); nothing waits for
//! a slot. Limits key on the member id from the verified credential.
//!
//! Failed bearer checks are not limited here. Behind the gateway there is no cheap per-client
//! key, a refusal costs one local Ed25519 verify (no lookup, no body read), and a bucket shared
//! by every client would let a flood of bad tokens lock out members with good ones.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;

/// The default per-run wall-clock limit (`CHELABASE_MCP_TIMEOUT_MS`).
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;
/// The largest `CHELABASE_MCP_TIMEOUT_MS`: the runtime takes no longer `timeout_ms` (m3, #347).
pub const MAX_TIMEOUT_MS: u64 = 60_000;
/// The default result cap in bytes of JSON text (`CHELABASE_MCP_RESULT_MAX_BYTES`).
pub const DEFAULT_RESULT_MAX_BYTES: usize = 24_000;
/// The smallest and largest `CHELABASE_MCP_RESULT_MAX_BYTES`.
pub const RESULT_MAX_BYTES_RANGE: (usize, usize) = (1_024, 4 << 20);
/// The most of a run's `console` output a tool result carries.
pub const LOGS_MAX_BYTES: usize = 4 * 1024;
/// The hint a call refused for want of a free slot carries: runs end within the timeout, and
/// most far sooner.
pub const BUSY_RETRY_AFTER_MS: u64 = 1_000;

/// The limits, from the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// `CHELABASE_MCP_TIMEOUT_MS`, 1 000 to 60 000 (default 30 000).
    pub timeout: Duration,
    /// `CHELABASE_MCP_RESULT_MAX_BYTES` (default 24 000).
    pub result_max_bytes: usize,
    /// `CHELABASE_MCP_CALLS_PER_MINUTE`: tool calls a member may make a minute (default 60).
    pub calls_per_minute: u32,
    /// `CHELABASE_MCP_MEMBER_CONCURRENCY`: runs a member may have in flight (default 2).
    pub member_concurrency: usize,
    /// `CHELABASE_MCP_CONCURRENCY`: runs in flight overall (default 8).
    pub total_concurrency: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
            result_max_bytes: DEFAULT_RESULT_MAX_BYTES,
            calls_per_minute: 60,
            member_concurrency: 2,
            total_concurrency: 8,
        }
    }
}

impl Limits {
    /// Blank values count as unset.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        fn number<T: std::str::FromStr + PartialOrd + std::fmt::Display + Copy>(
            get: &dyn Fn(&str) -> Option<String>,
            name: &str,
            default: T,
            (min, max): (T, T),
        ) -> anyhow::Result<T> {
            let Some(raw) = get(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
            else {
                return Ok(default);
            };
            let value: T = raw
                .parse()
                .ok()
                .with_context(|| format!("{name} must be a whole number"))?;
            anyhow::ensure!(
                value >= min && value <= max,
                "{name} must be from {min} to {max}, not {value}"
            );
            Ok(value)
        }
        let defaults = Self::default();
        let timeout_ms = number(
            &get,
            "CHELABASE_MCP_TIMEOUT_MS",
            DEFAULT_TIMEOUT_MS,
            (1_000, MAX_TIMEOUT_MS),
        )?;
        Ok(Self {
            timeout: Duration::from_millis(timeout_ms),
            result_max_bytes: number(
                &get,
                "CHELABASE_MCP_RESULT_MAX_BYTES",
                defaults.result_max_bytes,
                RESULT_MAX_BYTES_RANGE,
            )?,
            calls_per_minute: number(
                &get,
                "CHELABASE_MCP_CALLS_PER_MINUTE",
                defaults.calls_per_minute,
                (1, 10_000),
            )?,
            member_concurrency: number(
                &get,
                "CHELABASE_MCP_MEMBER_CONCURRENCY",
                defaults.member_concurrency,
                (1, 1_000),
            )?,
            total_concurrency: number(
                &get,
                "CHELABASE_MCP_CONCURRENCY",
                defaults.total_concurrency,
                (1, 10_000),
            )?,
        })
    }
}

type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

struct State {
    per_member: HashMap<String, usize>,
    total: usize,
}

struct Inner {
    per_minute: f64,
    member_concurrency: usize,
    total_concurrency: usize,
    /// member -> (tokens, last refill). Idle buckets expire once they would be full again.
    buckets: moka::sync::Cache<String, Arc<Mutex<(f64, Instant)>>>,
    state: Mutex<State>,
    clock: Clock,
}

/// Admits tool calls: a token bucket per member and the concurrency slots. Cheap to clone.
#[derive(Clone)]
pub struct Admission(Arc<Inner>);

/// A held concurrency slot, released when dropped: whether the run ended with a value, an
/// error, a timeout, or its future was dropped (a client disconnect).
pub struct Slot {
    inner: Arc<Inner>,
    member: String,
}

impl Admission {
    pub fn new(limits: &Limits) -> Self {
        let per_minute = f64::from(limits.calls_per_minute);
        Self(Arc::new(Inner {
            per_minute,
            member_concurrency: limits.member_concurrency,
            total_concurrency: limits.total_concurrency,
            buckets: moka::sync::Cache::builder()
                .max_capacity(100_000)
                // An idle bucket is full again within a minute; dropping it then loses nothing.
                .time_to_idle(Duration::from_secs(60))
                .build(),
            state: Mutex::new(State {
                per_member: HashMap::new(),
                total: 0,
            }),
            clock: Arc::new(Instant::now),
        }))
    }

    /// Replaces the bucket clock (tests only).
    pub fn with_clock(self, clock: impl Fn() -> Instant + Send + Sync + 'static) -> Self {
        let mut inner = Arc::try_unwrap(self.0)
            .unwrap_or_else(|_| panic!("with_clock before the admission is shared"));
        inner.clock = Arc::new(clock);
        Self(Arc::new(inner))
    }

    /// A slot for one run by `member`, or the milliseconds to wait before trying again. A call
    /// refused for want of a slot costs no token.
    pub fn admit(&self, member: &str) -> Result<Slot, u64> {
        let inner = &self.0;
        let mut state = inner.state.lock().unwrap_or_else(|p| p.into_inner());
        let held = state.per_member.get(member).copied().unwrap_or(0);
        if held >= inner.member_concurrency || state.total >= inner.total_concurrency {
            return Err(BUSY_RETRY_AFTER_MS);
        }
        inner.take_token(member)?;
        *state.per_member.entry(member.to_owned()).or_insert(0) += 1;
        state.total += 1;
        Ok(Slot {
            inner: inner.clone(),
            member: member.to_owned(),
        })
    }

    /// Runs in flight overall.
    pub fn in_flight(&self) -> usize {
        self.0.state.lock().unwrap_or_else(|p| p.into_inner()).total
    }
}

impl Inner {
    /// One token from `member`'s bucket (capacity and refill both `per_minute` a minute), or
    /// the milliseconds until the next one.
    fn take_token(&self, member: &str) -> Result<(), u64> {
        let now = (self.clock)();
        let capacity = self.per_minute;
        let per_ms = self.per_minute / 60_000.0;
        let bucket = self
            .buckets
            .get_with_by_ref(member, || Arc::new(Mutex::new((capacity, now))));
        let mut bucket = bucket.lock().unwrap_or_else(|p| p.into_inner());
        let (tokens, last) = *bucket;
        let elapsed_ms = now.saturating_duration_since(last).as_secs_f64() * 1_000.0;
        let tokens = (tokens + elapsed_ms * per_ms).min(capacity);
        if tokens >= 1.0 {
            *bucket = (tokens - 1.0, now);
            Ok(())
        } else {
            *bucket = (tokens, now);
            Err((((1.0 - tokens) / per_ms).ceil() as u64).max(1))
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
        state.total = state.total.saturating_sub(1);
        if let Some(held) = state.per_member.get_mut(&self.member) {
            *held -= 1;
            if *held == 0 {
                state.per_member.remove(&self.member);
            }
        }
    }
}

/// `text` cut to at most `max` bytes at a character boundary, followed by the truncation
/// marker, and whether it was cut. Text within `max` comes back unchanged.
pub fn truncate(mut text: String, max: usize) -> (String, bool) {
    let total = text.len();
    if total <= max {
        return (text, false);
    }
    let kept = floor_boundary(&text, max);
    text.truncate(kept);
    text.push_str(&marker(kept, total));
    (text, true)
}

/// The marker after a cut result: both sizes, and what to do about it.
pub fn marker(kept: usize, total: usize) -> String {
    format!(
        "\n--- TRUNCATED: {kept} of {total} bytes; return less (select fields, filter, page) ---"
    )
}

/// The largest index at most `max` that is a character boundary of `text`.
fn floor_boundary(text: &str, max: usize) -> usize {
    if max >= text.len() {
        return text.len();
    }
    (0..=max)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0)
}

/// A run's `console` lines as one text of at most [`LOGS_MAX_BYTES`] (plus a short note when
/// cut), or `None` when there were none.
pub fn logs_text(lines: &[String]) -> Option<String> {
    if lines.is_empty() {
        return None;
    }
    let text = lines.join("\n");
    if text.len() <= LOGS_MAX_BYTES {
        return Some(text);
    }
    let end = floor_boundary(&text, LOGS_MAX_BYTES);
    Some(format!(
        "{}\n--- LOGS TRUNCATED: {end} of {} bytes ---",
        &text[..end],
        text.len()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// A clock that moves only when told to.
    fn frozen() -> (Arc<AtomicU64>, impl Fn() -> Instant + Send + Sync + 'static) {
        let base = Instant::now();
        let offset_ms = Arc::new(AtomicU64::new(0));
        let read = offset_ms.clone();
        (offset_ms, move || {
            base + Duration::from_millis(read.load(Ordering::SeqCst))
        })
    }

    #[test]
    fn the_sixty_first_call_is_refused() {
        let (advance, clock) = frozen();
        let admission = Admission::new(&Limits::default()).with_clock(clock);
        for i in 0..60 {
            assert!(admission.admit("m1").is_ok(), "call {i} refused");
        }
        let retry_after = admission.admit("m1").err().expect("the 61st is refused");
        assert!(
            (1..=1_000).contains(&retry_after),
            "retry_after_ms {retry_after}"
        );
        // Another member has its own bucket.
        assert!(admission.admit("m2").is_ok());
        advance.store(retry_after, Ordering::SeqCst);
        assert!(
            admission.admit("m1").is_ok(),
            "a token is back after the wait"
        );
        assert!(admission.admit("m1").is_err());
    }

    #[test]
    fn slots_are_per_member() {
        let admission = Admission::new(&Limits::default());
        let a1 = admission.admit("a").unwrap();
        let _a2 = admission.admit("a").unwrap();
        assert_eq!(admission.admit("a").err(), Some(BUSY_RETRY_AFTER_MS));
        // Another member still runs.
        let _b1 = admission.admit("b").unwrap();
        drop(a1);
        let _a3 = admission.admit("a").expect("a freed slot is free again");

        // Overall: 8 at once across members, then refused whoever asks.
        let mut held = vec![];
        for m in ["c", "c", "d", "d", "e"] {
            held.push(admission.admit(m).unwrap());
        }
        assert_eq!(admission.in_flight(), 8);
        assert_eq!(admission.admit("f").err(), Some(BUSY_RETRY_AFTER_MS));
        // A refusal for want of a slot costs no token.
        drop(held);
        for _ in 0..55 {
            drop(admission.admit("f").unwrap());
        }
    }

    #[tokio::test]
    async fn a_slot_is_released_on_error_and_on_drop() {
        let admission = Admission::new(&Limits::default());

        // A run that ends in an error.
        let run = |slot: Slot| async move {
            let _slot = slot;
            Err::<(), &str>("threw")
        };
        assert!(run(admission.admit("m").unwrap()).await.is_err());
        assert_eq!(admission.in_flight(), 0);

        // A run whose future is dropped mid-way (the client went away).
        let slot = admission.admit("m").unwrap();
        let task = tokio::spawn(async move {
            let _slot = slot;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        assert_eq!(admission.in_flight(), 1);
        task.abort();
        let _ = task.await;
        assert_eq!(admission.in_flight(), 0);
        let _a = admission.admit("m").unwrap();
        let _b = admission.admit("m").unwrap();
    }

    #[test]
    fn truncation_keeps_valid_utf8_and_states_sizes() {
        let short = "[1,2,3]".to_owned();
        assert_eq!(truncate(short.clone(), 100), (short, false));

        // "é" is two bytes: a cut at an odd byte would split one.
        let text = format!("\"{}\"", "é".repeat(60_000));
        let total = text.len();
        let (cut, truncated) = truncate(text, 24_001);
        assert!(truncated);
        let marker_at = cut.find("\n--- TRUNCATED: ").expect("a marker");
        let kept = &cut[..marker_at];
        assert!(
            kept.len() <= 24_001 && kept.len() >= 24_000,
            "{}",
            kept.len()
        );
        assert_eq!(&cut[marker_at..], marker(kept.len(), total));
        assert!(cut.ends_with("bytes; return less (select fields, filter, page) ---"));
        assert!(cut.contains(&format!("{} of {total} bytes", kept.len())));
    }

    #[test]
    fn logs_are_capped() {
        assert_eq!(logs_text(&[]), None);
        assert_eq!(
            logs_text(&["a".into(), "b".into()]).as_deref(),
            Some("a\nb")
        );
        let lines: Vec<String> = (0..200)
            .map(|i| format!("line {i} {}", "ü".repeat(20)))
            .collect();
        let text = logs_text(&lines).unwrap();
        let (kept, note) = text.split_once("\n--- LOGS TRUNCATED: ").unwrap();
        assert!(kept.len() <= LOGS_MAX_BYTES);
        assert!(note.starts_with(&format!("{} of ", kept.len())));
    }

    #[test]
    fn limits_read_the_environment() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        assert_eq!(Limits::from_lookup(env(&[])).unwrap(), Limits::default());
        let set = Limits::from_lookup(env(&[
            ("CHELABASE_MCP_TIMEOUT_MS", "5000"),
            ("CHELABASE_MCP_RESULT_MAX_BYTES", " 4096 "),
            ("CHELABASE_MCP_CALLS_PER_MINUTE", ""),
        ]))
        .unwrap();
        assert_eq!(set.timeout, Duration::from_secs(5));
        assert_eq!(set.result_max_bytes, 4096);
        assert_eq!(set.calls_per_minute, 60);
        for bad in [
            ("CHELABASE_MCP_TIMEOUT_MS", "60001"),
            ("CHELABASE_MCP_TIMEOUT_MS", "abc"),
            ("CHELABASE_MCP_RESULT_MAX_BYTES", "10"),
            ("CHELABASE_MCP_CONCURRENCY", "0"),
        ] {
            let pairs: &'static [(&str, &str)] = Box::leak(Box::new([bad]));
            assert!(Limits::from_lookup(env(pairs)).is_err(), "{bad:?}");
        }
    }
}
