//! Rate limits, client IP, email normalization and response padding.

use http::HeaderMap;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Accept tickets already used, by nonce, each kept until the ticket would have expired.
///
/// In memory, so it assumes one auth process (like [`RateLimiter`]); a second instance would
/// need a shared store, or a replay could land on the other one.
#[derive(Default)]
pub struct UsedTickets {
    used: Mutex<HashMap<String, u64>>,
}

impl UsedTickets {
    const CAP: usize = 100_000;

    /// Marks the ticket used. False when it already was, or when the set is full of live
    /// entries (refusing is the safe side).
    pub fn claim(&self, nonce: &str, exp: u64, now: u64) -> bool {
        let mut used = self.used.lock().expect("tickets lock");
        if used.len() >= Self::CAP / 2 {
            used.retain(|_, e| *e >= now);
        }
        if used.len() >= Self::CAP || used.get(nonce).is_some_and(|e| *e >= now) {
            return false;
        }
        used.insert(nonce.to_string(), exp);
        true
    }
}

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

    /// Gives back one attempt counted by [`allow`](Self::allow) or [`hit`](Self::hit): never
    /// below zero, and only while `key`'s window is the current one (nothing for an expired or
    /// missing window).
    pub fn refund(&self, key: &str, window: Duration) {
        let mut windows = self.windows.lock().expect("limiter lock");
        if let Some((start, n)) = windows.get_mut(key)
            && start.elapsed() < window
        {
            *n = n.saturating_sub(1);
        }
    }
}

/// The client's address for rate limits: the rightmost `X-Forwarded-For` entry (the one the
/// gateway appended from its TCP peer) when `peer` is a trusted proxy, else `peer` itself. A
/// compute on the compute network reaches auth directly, so its forwarded header never counts
/// (RT1 T1 ruling (e)). IPv4-mapped IPv6 peers are compared and written as IPv4.
pub fn client_ip_from(headers: &HeaderMap, peer: IpAddr, trusted: &[IpAddr]) -> String {
    let peer = peer.to_canonical();
    if trusted.iter().any(|t| t.to_canonical() == peer) {
        let forwarded = client_ip(headers);
        if forwarded != "unknown" {
            return forwarded;
        }
    }
    peer.to_string()
}

/// `CHELABASE_AUTH_TRUSTED_PROXY`: the proxies whose `X-Forwarded-For` auth believes, as a
/// comma-separated list of IP addresses or host names (the stack: `gateway`). Names are resolved
/// at most every 10 s, so a recreated gateway's new address is picked up; a failed lookup trusts
/// nobody until the next one (fail closed: its callers are then rate-limited as the gateway).
#[derive(Clone, Default)]
pub struct TrustedProxies {
    inner: std::sync::Arc<TrustedInner>,
}

#[derive(Default)]
struct TrustedInner {
    addrs: Vec<IpAddr>,
    names: Vec<String>,
    resolved: Mutex<Option<(Instant, Vec<IpAddr>)>>,
}

const RESOLVE_EVERY: Duration = Duration::from_secs(10);

impl TrustedProxies {
    /// Parses the setting; blank is nobody. `true`, `1`, `yes`, `on` (which once meant "trust
    /// every peer") are refused: name the proxy instead.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let mut addrs = Vec::new();
        let mut names = Vec::new();
        for entry in raw.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            if matches!(
                entry.to_ascii_lowercase().as_str(),
                "true" | "1" | "yes" | "on" | "false" | "0" | "no" | "off"
            ) {
                return Err(format!(
                    "CHELABASE_AUTH_TRUSTED_PROXY={raw:?}: name the proxy (its host name or IP \
                     address, e.g. gateway), not a switch: only its X-Forwarded-For is believed"
                ));
            }
            match entry.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>() {
                Ok(ip) => addrs.push(ip),
                Err(_) if is_host_name(entry) => names.push(entry.to_ascii_lowercase()),
                Err(_) => {
                    return Err(format!(
                        "CHELABASE_AUTH_TRUSTED_PROXY: {entry:?} is neither an IP address nor a host name"
                    ));
                }
            }
        }
        Ok(Self {
            inner: std::sync::Arc::new(TrustedInner {
                addrs,
                names,
                resolved: Mutex::new(None),
            }),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.inner.addrs.is_empty() && self.inner.names.is_empty()
    }

    /// The trusted addresses now: the literal ones and the names' current addresses.
    pub async fn current(&self) -> Vec<IpAddr> {
        let mut out = self.inner.addrs.clone();
        if self.inner.names.is_empty() {
            return out;
        }
        if let Some((at, ips)) = self.inner.resolved.lock().expect("trusted lock").as_ref()
            && at.elapsed() < RESOLVE_EVERY
        {
            out.extend_from_slice(ips);
            return out;
        }
        let mut ips = Vec::new();
        for name in &self.inner.names {
            match tokio::net::lookup_host((name.as_str(), 0)).await {
                Ok(found) => ips.extend(found.map(|a| a.ip().to_canonical())),
                Err(e) => tracing::warn!("CHELABASE_AUTH_TRUSTED_PROXY: resolving {name}: {e}"),
            }
        }
        *self.inner.resolved.lock().expect("trusted lock") = Some((Instant::now(), ips.clone()));
        out.extend(ips);
        out
    }
}

fn is_host_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

/// The rightmost `X-Forwarded-For` entry: the one the gateway appended from the TCP peer.
/// Entries to its left come from the client and can be forged. Only meaningful on a request
/// [`client_ip_from`] has already rewritten (the router's forwarded-header layer does).
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

pub use chelabase_zitadel::rules::normalize_email;

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

    #[test]
    fn a_refund_gives_back_one_attempt_within_the_window() {
        let limiter = RateLimiter::default();
        let minute = Duration::from_secs(60);
        assert!(limiter.allow("k", 2, minute));
        assert!(limiter.allow("k", 2, minute));
        assert!(!limiter.allow("k", 2, minute));
        // Three attempts counted; one refund leaves two, so the next is the third again.
        limiter.refund("k", minute);
        assert!(!limiter.allow("k", 2, minute));
        limiter.refund("k", minute);
        limiter.refund("k", minute);
        limiter.refund("k", minute);
        // Never below zero: a full budget afterwards, not more.
        assert!(limiter.allow("k", 2, minute));
        assert!(limiter.allow("k", 2, minute));
        assert!(!limiter.allow("k", 2, minute));
        // A missing key does nothing.
        limiter.refund("missing", minute);
        assert!(limiter.allow("missing", 1, minute));
        assert!(!limiter.allow("missing", 1, minute));
    }

    #[test]
    fn a_refund_does_nothing_for_an_expired_window() {
        let limiter = RateLimiter::default();
        let short = Duration::from_millis(1);
        assert!(limiter.allow("k", 2, short));
        std::thread::sleep(Duration::from_millis(5));
        limiter.refund("k", short);
        // The expired window still holds its one attempt: checked under a long window, the
        // second is allowed and the third is not.
        let minute = Duration::from_secs(60);
        assert!(limiter.allow("k", 2, minute));
        assert!(!limiter.allow("k", 2, minute));
    }

    #[tokio::test]
    async fn pad_waits_for_the_minimum() {
        let start = Instant::now();
        pad(start, Duration::from_millis(80)).await;
        assert!(start.elapsed() >= Duration::from_millis(80));
    }
}
