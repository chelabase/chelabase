//! Waiting with backoff up to one deadline, for readiness only.
//!
//! A check answers [`Check::Ready`], [`Check::Wait`] (a dependency isn't up yet: try again) or
//! an error (a real failure: stop now). Only the caller decides what counts as "not up yet",
//! and only readiness does: a refused connection, a 503, a readiness probe's 404, an unmigrated
//! schema, an Actions target host that doesn't resolve yet.

use anyhow::{Result, bail};
use std::future::Future;
use std::time::Duration;
use tokio::time::Instant;

/// One attempt's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Check<T> {
    Ready(T),
    /// Not ready yet, and why (named in the deadline's error).
    Wait(String),
}

/// The backoff between attempts: `first`, doubling up to `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub first: Duration,
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            first: Duration::from_millis(250),
            max: Duration::from_secs(5),
        }
    }
}

/// The run's one deadline: every wait of the run shares it.
#[derive(Debug, Clone, Copy)]
pub struct Waiter {
    deadline: Instant,
    timeout: Duration,
    backoff: Backoff,
}

impl Waiter {
    pub fn new(timeout: Duration, backoff: Backoff) -> Self {
        Self {
            deadline: Instant::now() + timeout,
            timeout,
            backoff,
        }
    }

    /// The same deadline, or `limit` from now when that comes first.
    pub fn at_most(&self, limit: Duration) -> Self {
        let mut w = *self;
        w.deadline = w.deadline.min(Instant::now() + limit);
        w
    }

    /// Runs `attempt` until it is ready, it fails, or the deadline passes (an error naming
    /// `what` and the last reason it waited).
    pub async fn until<T, F, Fut>(&self, what: &str, mut attempt: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<Check<T>>>,
    {
        let mut delay = self.backoff.first;
        let mut waited = false;
        loop {
            let reason = match attempt().await? {
                Check::Ready(value) => return Ok(value),
                Check::Wait(reason) => reason,
            };
            if !waited {
                tracing::info!("waiting for {what}: {reason}");
                waited = true;
            }
            let now = Instant::now();
            if now >= self.deadline {
                bail!(
                    "timed out after {}s waiting for {what}: {reason}",
                    self.timeout.as_secs()
                );
            }
            tokio::time::sleep(delay.min(self.deadline - now)).await;
            delay = (delay * 2).min(self.backoff.max);
        }
    }
}

/// `--timeout`: seconds, or a number with `s`, `m` or `h`.
pub fn parse_timeout(text: &str) -> Result<Duration> {
    let text = text.trim();
    let (digits, unit) = match text.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => text.split_at(i),
        None => (text, "s"),
    };
    let n: u64 = match digits.parse() {
        Ok(n) if n > 0 => n,
        _ => bail!("--timeout {text:?}: expected a positive number of seconds, or 30s, 10m, 1h"),
    };
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        _ => bail!("--timeout {text:?}: the unit is s, m or h"),
    };
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn quick(timeout_ms: u64) -> Waiter {
        Waiter::new(
            Duration::from_millis(timeout_ms),
            Backoff {
                first: Duration::from_millis(1),
                max: Duration::from_millis(4),
            },
        )
    }

    #[tokio::test]
    async fn waits_until_ready() {
        let n = AtomicU32::new(0);
        let v = quick(5_000)
            .until("x", || async {
                Ok(if n.fetch_add(1, Ordering::SeqCst) < 3 {
                    Check::Wait("not yet".into())
                } else {
                    Check::Ready(7)
                })
            })
            .await
            .unwrap();
        assert_eq!((v, n.load(Ordering::SeqCst)), (7, 4));
    }

    #[tokio::test]
    async fn an_error_stops_at_once() {
        let n = AtomicU32::new(0);
        let err = quick(5_000)
            .until::<(), _, _>("x", || async {
                n.fetch_add(1, Ordering::SeqCst);
                bail!("refused")
            })
            .await
            .unwrap_err();
        assert_eq!(n.load(Ordering::SeqCst), 1);
        assert_eq!(err.to_string(), "refused");
    }

    #[tokio::test]
    async fn the_deadline_names_what_and_why() {
        let err = quick(20)
            .until::<(), _, _>("the orchestrator", || async {
                Ok(Check::Wait("connection refused".into()))
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("waiting for the orchestrator: connection refused"),
            "{err}"
        );
    }

    #[test]
    fn timeouts_parse() {
        assert_eq!(parse_timeout("600").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_timeout("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_timeout("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_timeout("1h").unwrap(), Duration::from_secs(3600));
        for bad in ["", "0", "-1", "10d", "m", "1.5m"] {
            assert!(parse_timeout(bad).is_err(), "{bad}");
        }
    }
}
