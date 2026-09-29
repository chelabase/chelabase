//! Rate limits, client IP, email normalization and response padding.

use http::HeaderMap;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Fixed-window counters in memory (one auth process).
#[derive(Default)]
pub struct RateLimiter {
    windows: Mutex<HashMap<String, (Instant, u32)>>,
}

impl RateLimiter {
    /// Counts this attempt; false once `key` has used `limit` attempts in the current window.
    pub fn allow(&self, key: &str, limit: u32, window: Duration) -> bool {
        let now = Instant::now();
        let mut windows = self.windows.lock().expect("limiter lock");
        if windows.len() > 100_000 {
            windows.retain(|_, (start, _)| now.duration_since(*start) < Duration::from_secs(3600));
        }
        let entry = windows.entry(key.to_string()).or_insert((now, 0));
        if now.duration_since(entry.0) >= window {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 <= limit
    }

    /// Whether `key` has already used `limit` attempts in its current window; counts nothing.
    pub fn over(&self, key: &str, limit: u32, window: Duration) -> bool {
        let windows = self.windows.lock().expect("limiter lock");
        windows
            .get(key)
            .is_some_and(|(start, n)| start.elapsed() < window && *n >= limit)
    }

    /// Counts one attempt against `key` without checking a limit (e.g. after a failure).
    pub fn hit(&self, key: &str, window: Duration) {
        self.allow(key, u32::MAX, window);
    }
}

/// The rightmost `X-Forwarded-For` entry: the one the gateway appended from the TCP peer.
/// Entries to its left come from the client and can be forged.
pub fn client_ip(headers: &HeaderMap) -> String {
    headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .rfind(|s| !s.is_empty())
        .unwrap_or("unknown")
        .to_string()
}

pub fn normalize_email(email: &str) -> String {
    email.trim().to_lowercase()
}

/// A short, stable tag for logs, so emails never appear in them.
pub fn email_tag(email: &str) -> String {
    hex::encode(&Sha256::digest(email.as_bytes())[..6])
}

/// Sleep until `min` has passed since `start`.
pub async fn pad(start: Instant, min: Duration) {
    tokio::time::sleep_until((start + min).into()).await;
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // Review Focus 2.
    #[test]
    fn client_ip_is_the_rightmost_forwarded_entry() {
        let mut headers = http::HeaderMap::new();
        headers.insert("x-forwarded-for", "1.2.3.4, 10.0.0.9".parse().unwrap());
        assert_eq!(client_ip(&headers), "10.0.0.9");
        assert_eq!(client_ip(&http::HeaderMap::new()), "unknown");
    }

    #[test]
    fn emails_are_trimmed_and_lowercased() {
        assert_eq!(normalize_email("  Ada@Example.COM "), "ada@example.com");
        assert_eq!(email_tag("ada@example.com").len(), 12);
        assert_ne!(email_tag("ada@example.com"), email_tag("bob@example.com"));
    }

    #[test]
    fn the_limiter_allows_up_to_the_limit_per_window() {
        let limiter = RateLimiter::default();
        for _ in 0..5 {
            assert!(limiter.allow("sign-in:email:a", 5, Duration::from_secs(60)));
        }
        assert!(!limiter.allow("sign-in:email:a", 5, Duration::from_secs(60)));
        assert!(limiter.allow("sign-in:email:b", 5, Duration::from_secs(60)));
        assert!(limiter.allow("short", 1, Duration::from_millis(1)));
        std::thread::sleep(Duration::from_millis(5));
        assert!(
            limiter.allow("short", 1, Duration::from_millis(1)),
            "a new window starts over"
        );
    }

    #[test]
    fn a_peek_does_not_count_and_a_failure_does() {
        let limiter = RateLimiter::default();
        let minute = Duration::from_secs(60);
        for _ in 0..10 {
            assert!(!limiter.over("k", 2, minute));
        }
        limiter.hit("k", minute);
        limiter.hit("k", minute);
        assert!(limiter.over("k", 2, minute));
        assert!(!limiter.over("other", 2, minute));
    }

    #[tokio::test]
    async fn pad_waits_for_the_minimum() {
        let start = Instant::now();
        pad(start, Duration::from_millis(80)).await;
        assert!(start.elapsed() >= Duration::from_millis(80));
    }
}
