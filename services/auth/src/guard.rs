//! Rate limits, client IP, email normalization and response padding.

use http::HeaderMap;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
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
///
/// Invariant: within any one window of a key, the count is exactly the reservations granted and
/// not refunded, so it never exceeds `limit`, whatever the interleaving of `allow`, `refund` and
/// window rollover. A refused reservation counts nothing. A granted one stays counted until it
/// is refunded: one that is never decided (its request dropped mid-call) counts. (`over` and `hit` are the peek-and-count pair; `hit`
/// counts without a limit.)
///
/// It also keeps sliding logs ([`allow_logged`](Self::allow_logged)): per key, the instant of each
/// reservation granted in the last `window` and not refunded, so a key is refused exactly while
/// `limit` of them are younger than `window`, whenever the first one was made.
#[derive(Default)]
pub struct RateLimiter {
    windows: Mutex<HashMap<String, (Instant, u32)>>,
    logs: Mutex<HashMap<String, Vec<(Instant, u64)>>>,
    next_entry: AtomicU64,
    /// How far tests have moved this limiter's clock forward.
    skew: Mutex<Duration>,
}

impl RateLimiter {
    fn now(&self) -> Instant {
        Instant::now() + *self.skew.lock().expect("limiter skew lock")
    }

    /// Moves this limiter's clock forward (tests: a window passing without waiting for it).
    pub fn advance(&self, by: Duration) {
        *self.skew.lock().expect("limiter skew lock") += by;
    }

    /// Counts this attempt in `key`'s sliding log; `None` (and nothing counted) while `limit`
    /// attempts younger than `window` stand in it. On success returns the entry's id, which
    /// [`refund_logged`](Self::refund_logged) takes to remove it.
    pub fn allow_logged(&self, key: &str, limit: u32, window: Duration) -> Option<u64> {
        self.allow_logged_at(key, limit, window, self.now())
    }

    fn allow_logged_at(&self, key: &str, limit: u32, window: Duration, now: Instant) -> Option<u64> {
        let mut logs = self.logs.lock().expect("limiter lock");
        if logs.len() > 100_000 {
            logs.retain(|_, log| log.iter().any(|(at, _)| now.duration_since(*at) < window));
        }
        let log = logs.entry(key.to_string()).or_default();
        log.retain(|(at, _)| now.duration_since(*at) < window);
        if log.len() >= limit as usize {
            return None;
        }
        let id = self.next_entry.fetch_add(1, Ordering::Relaxed);
        log.push((now, id));
        Some(id)
    }

    /// Removes the entry [`allow_logged`](Self::allow_logged) counted as `id`; nothing when it
    /// has already aged out.
    pub fn refund_logged(&self, key: &str, id: u64) {
        let mut logs = self.logs.lock().expect("limiter lock");
        if let Some(log) = logs.get_mut(key) {
            log.retain(|(_, entry)| *entry != id);
        }
    }

    /// Counts this attempt; false (and nothing counted) once `key` has used `limit` attempts in
    /// the current window.
    pub fn allow(&self, key: &str, limit: u32, window: Duration) -> bool {
        self.allow_slot(key, limit, window).is_some()
    }

    /// Like [`allow`](Self::allow), and on success returns the start of the window the attempt
    /// was counted in, which [`refund`](Self::refund) needs to give it back.
    pub fn allow_slot(&self, key: &str, limit: u32, window: Duration) -> Option<Instant> {
        self.allow_at(key, limit, window, self.now())
    }

    fn allow_at(&self, key: &str, limit: u32, window: Duration, now: Instant) -> Option<Instant> {
        let mut windows = self.windows.lock().expect("limiter lock");
        if windows.len() > 100_000 {
            windows.retain(|_, (start, _)| now.duration_since(*start) < Duration::from_secs(3600));
        }
        let entry = windows.entry(key.to_string()).or_insert((now, 0));
        if now.duration_since(entry.0) >= window {
            *entry = (now, 0);
        }
        if entry.1 >= limit {
            // Turned away: a refused reservation holds nothing, so it must not count (a refund
            // would never come for it).
            return None;
        }
        entry.1 += 1;
        Some(entry.0)
    }

    /// Whether `key` has already used `limit` attempts in its current window; counts nothing.
    pub fn over(&self, key: &str, limit: u32, window: Duration) -> bool {
        let now = self.now();
        let windows = self.windows.lock().expect("limiter lock");
        windows
            .get(key)
            .is_some_and(|(start, n)| now.duration_since(*start) < window && *n >= limit)
    }

    /// Counts one attempt against `key` without checking a limit (e.g. after a failure).
    pub fn hit(&self, key: &str, window: Duration) {
        self.allow(key, u32::MAX, window);
    }

    /// Gives back one attempt that [`allow_slot`](Self::allow_slot) counted in the window that
    /// began at `started`. It applies only to that window: when `key` is in a later window (or
    /// has none) nothing happens, and the count never goes below zero.
    pub fn refund(&self, key: &str, started: Instant) {
        let mut windows = self.windows.lock().expect("limiter lock");
        if let Some((start, n)) = windows.get_mut(key)
            && *start == started
        {
            *n = n.saturating_sub(1);
        }
    }
}

/// One attempt reserved against a [`RateLimiter`] key (an address's, an invitee's, an email's).
/// It fails closed: the attempt stays counted unless [`refund`](Self::refund) gives it back, so a
/// slot that is dropped undecided (a new early return, or the request future dropped because the
/// client hung up while identity checked a code or password) spends budget. Only outcomes that
/// must not count refund.
pub struct Slot<'a> {
    limiter: &'a RateLimiter,
    key: String,
    /// Start of the window the slot was reserved in; the refund applies only to that window.
    started: Instant,
}

impl<'a> Slot<'a> {
    /// Counts one attempt against `key`; `None` (and nothing counted) once its `limit` for the
    /// `window` is used.
    pub fn reserve(limiter: &'a RateLimiter, key: String, limit: u32, window: Duration) -> Option<Self> {
        let started = limiter.allow_slot(&key, limit, window)?;
        Some(Slot { limiter, key, started })
    }

    /// Gives the attempt back: the outcome isn't a failure of the caller's.
    pub fn refund(self) {
        self.limiter.refund(&self.key, self.started);
    }
}

/// Refused code checks one email may take per kind in [`CODE_CHECK_WINDOW`], on the checks
/// that name an email without a session (reset confirm, app email verification). Equal to
/// identity's `FAILURE_BUDGET`, over a sliding window as long as its `FAILURE_WINDOW_MS`:
/// every guess identity counts against an account went through one of these reservations, and
/// identity's window starts no earlier than the first of them, so auth refuses an email before
/// identity's stored budget for it can be spent. A spent budget is then never the first 429 an
/// email gets, and the answers for a known and an unknown email stay alike.
pub const CODE_CHECKS_PER_EMAIL: u32 = 10;
/// The sliding window of [`CODE_CHECKS_PER_EMAIL`]: identity's `FAILURE_WINDOW_MS` (24 h).
pub const CODE_CHECK_WINDOW: Duration = Duration::from_secs(24 * 3600);

/// One code check reserved against an email's [`CODE_CHECKS_PER_EMAIL`]
/// (`<scope>code-check:<kind>:<tag>`). Like [`Slot`] it fails closed: it stays counted unless
/// [`refund`](Self::refund) gives it back, which only a right code does.
pub struct CodeCheckSlot<'a> {
    limiter: &'a RateLimiter,
    key: String,
    id: u64,
}

impl<'a> CodeCheckSlot<'a> {
    /// `scope` prefixes the key (`""` for members, `app:<project>:` for app users), `kind` is
    /// the code kind and `tag` the email's [`email_tag`]. `None`, with nothing counted, once the
    /// email has [`CODE_CHECKS_PER_EMAIL`] unrefunded checks of this kind in the window.
    pub fn reserve(limiter: &'a RateLimiter, scope: &str, kind: &str, tag: &str) -> Option<Self> {
        let key = format!("{scope}code-check:{kind}:{tag}");
        let id = limiter.allow_logged(&key, CODE_CHECKS_PER_EMAIL, CODE_CHECK_WINDOW)?;
        Some(CodeCheckSlot { limiter, key, id })
    }

    /// Gives the check back: the code was right.
    pub fn refund(self) {
        self.limiter.refund_logged(&self.key, self.id);
    }
}

/// Failed sign-ins one client may spend on one email a minute.
pub const SIGN_IN_PER_CLIENT: u32 = 5;
/// Failed sign-ins one email may take a minute from every client together: well above
/// [`SIGN_IN_PER_CLIENT`], so failures from one client don't lock the owner out from another,
/// and still a bound on guesses spread over many clients.
pub const SIGN_IN_PER_EMAIL: u32 = 30;

/// The two attempts a password check for an email reserves: one against the email from this
/// client (`<scope>sign-in:email-client:<tag>:<bucket>`, [`SIGN_IN_PER_CLIENT`] a minute) and
/// one against the email from anywhere (`<scope>sign-in:email:<tag>`, [`SIGN_IN_PER_EMAIL`] a
/// minute). Both are held or neither is, and they are refunded together, so each follows its
/// caller's refund table exactly as one [`Slot`] would.
pub struct SignInSlots<'a> {
    client: Slot<'a>,
    email: Slot<'a>,
}

impl<'a> SignInSlots<'a> {
    /// `scope` prefixes both keys (`""` for members, `app:<project>:` for app users); `tag` is
    /// the email's [`email_tag`] and `bucket` the client's [`ip_bucket`]. `None`, with nothing
    /// counted, when either limit is used.
    pub fn reserve(limiter: &'a RateLimiter, scope: &str, tag: &str, bucket: &str) -> Option<Self> {
        let minute = Duration::from_secs(60);
        let client = Slot::reserve(
            limiter,
            format!("{scope}sign-in:email-client:{tag}:{bucket}"),
            SIGN_IN_PER_CLIENT,
            minute,
        )?;
        let Some(email) = Slot::reserve(limiter, format!("{scope}sign-in:email:{tag}"), SIGN_IN_PER_EMAIL, minute) else {
            client.refund();
            return None;
        };
        Some(SignInSlots { client, email })
    }

    /// Gives both attempts back.
    pub fn refund(self) {
        self.client.refund();
        self.email.refund();
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

/// The key a client address is limited under: an IPv6 address stands for its /64
/// (`2001:db8:1:2::/64`), the block one subscriber is usually given, so a client can't step
/// around a per-address limit by moving within it. IPv4 and IPv4-mapped IPv6 addresses are the
/// IPv4 address. A zone suffix (`%eth0`) and enclosing brackets are dropped before parsing, so
/// every way of writing one address gives one key. Anything unparsable is kept as given.
pub fn ip_bucket(raw: &str) -> String {
    let trimmed = raw.trim();
    let bare = trimmed.strip_prefix('[').and_then(|r| r.strip_suffix(']')).unwrap_or(trimmed);
    let bare = bare.split_once('%').map_or(bare, |(addr, _zone)| addr);
    match bare.parse::<IpAddr>().map(|ip| ip.to_canonical()) {
        Ok(IpAddr::V4(v4)) => v4.to_string(),
        Ok(IpAddr::V6(v6)) => {
            let s = v6.segments();
            let net = std::net::Ipv6Addr::new(s[0], s[1], s[2], s[3], 0, 0, 0, 0);
            format!("{net}/64")
        }
        Err(_) => raw.to_string(),
    }
}

/// [`client_ip`] as a rate-limit key ([`ip_bucket`]).
pub fn client_bucket(headers: &HeaderMap) -> String {
    ip_bucket(&client_ip(headers))
}

pub use chelabase_identity::rules::normalize_email;

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

    // m9b AC 4: one /64 is one key, however the address is written.
    #[test]
    fn ipv6_addresses_share_their_64() {
        let a = ip_bucket("2001:db8:1:2::1");
        assert_eq!(a, "2001:db8:1:2::/64");
        assert_eq!(ip_bucket("2001:0db8:0001:0002:ffff:ffff:ffff:fffe"), a);
        assert_eq!(ip_bucket("2001:DB8:1:2:0:0:0:9"), a);
        assert_eq!(ip_bucket("[2001:db8:1:2::abcd]"), a);
        assert_ne!(ip_bucket("2001:db8:1:3::1"), a, "another /64");
        assert_ne!(ip_bucket("2001:db8:2:2::1"), a);
    }

    #[test]
    fn ipv4_and_mapped_addresses_are_ipv4() {
        assert_eq!(ip_bucket("203.0.113.9"), "203.0.113.9");
        assert_eq!(ip_bucket("::ffff:203.0.113.9"), "203.0.113.9");
        assert_eq!(ip_bucket("::ffff:cb00:7109"), "203.0.113.9");
        assert_eq!(ip_bucket("[::ffff:203.0.113.9]"), "203.0.113.9");
        assert_ne!(ip_bucket("203.0.113.10"), ip_bucket("203.0.113.9"), "IPv4 is never grouped");
    }

    #[test]
    fn a_zone_suffix_is_ignored() {
        assert_eq!(ip_bucket("fe80::1%eth0"), "fe80::/64");
        assert_eq!(ip_bucket("fe80::2%7"), ip_bucket("fe80::1"));
        assert_eq!(ip_bucket("[fe80::1%25eth0]"), "fe80::/64");
    }

    #[test]
    fn an_unparsable_address_is_kept() {
        assert_eq!(ip_bucket("unknown"), "unknown");
        assert_eq!(ip_bucket("1.2.3.4:80"), "1.2.3.4:80");
        assert_eq!(ip_bucket(""), "");
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
        let slot = limiter.allow_slot("k", 2, minute).unwrap();
        assert!(limiter.allow("k", 2, minute));
        assert!(!limiter.allow("k", 2, minute));
        // The refused third counted nothing; one refund leaves one held, so one more fits.
        limiter.refund("k", slot);
        assert!(limiter.allow("k", 2, minute));
        assert!(!limiter.allow("k", 2, minute));
        limiter.refund("k", slot);
        limiter.refund("k", slot);
        limiter.refund("k", slot);
        // Never below zero: a full budget afterwards, not more.
        assert!(limiter.allow("k", 2, minute));
        assert!(limiter.allow("k", 2, minute));
        assert!(!limiter.allow("k", 2, minute));
        // A missing key does nothing.
        limiter.refund("missing", slot);
        assert!(limiter.allow("missing", 1, minute));
        assert!(!limiter.allow("missing", 1, minute));
    }

    #[test]
    fn a_refund_after_expiry_leaves_the_next_window_a_full_budget_and_no_more() {
        let limiter = RateLimiter::default();
        let short = Duration::from_millis(1);
        let slot = limiter.allow_slot("k", 2, short).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        limiter.refund("k", slot);
        let minute = Duration::from_secs(60);
        assert!(limiter.allow("k", 2, minute));
        assert!(limiter.allow("k", 2, minute));
        assert!(!limiter.allow("k", 2, minute));
    }

    #[test]
    fn a_refund_for_an_old_window_does_not_free_the_new_one() {
        let limiter = RateLimiter::default();
        let w = Duration::from_millis(50);
        // A slot reserved in the first window (still in flight)...
        let old = limiter.allow_slot("k", 1, w).unwrap();
        std::thread::sleep(Duration::from_millis(60));
        // ...the window rolls over and a failure spends the new window's only attempt...
        assert!(limiter.allow("k", 1, w));
        // ...then the first request succeeds and hands its slot back.
        limiter.refund("k", old);
        // The new window's failure must still count.
        assert!(!limiter.allow("k", 1, w), "refund of a stale slot freed the new window's failure");
    }

    // #74 AC 5: a slot reserved with a minute window spends that minute only.
    #[test]
    fn a_slot_with_a_minute_window_can_try_again_after_it() {
        let limiter = RateLimiter::default();
        let minute = Duration::from_secs(60);
        for _ in 0..5 {
            assert!(Slot::reserve(&limiter, "k".into(), 5, minute).is_some());
        }
        assert!(Slot::reserve(&limiter, "k".into(), 5, minute).is_none());
        // 61 s later (the window's start moved back instead of waiting).
        let back = Duration::from_secs(61);
        for (start, _) in limiter.windows.lock().unwrap().values_mut() {
            *start = start.checked_sub(back).expect("an instant 61 s back");
        }
        let slot = Slot::reserve(&limiter, "k".into(), 5, minute).expect("a new window");
        slot.refund();
        for _ in 0..5 {
            assert!(Slot::reserve(&limiter, "k".into(), 5, minute).is_some());
        }
        assert!(Slot::reserve(&limiter, "k".into(), 5, minute).is_none());
    }

    // m9b AC 5, 6: the pair holds both attempts or neither.
    #[test]
    fn sign_in_slots_hold_both_or_neither() {
        let limiter = RateLimiter::default();
        for i in 0..SIGN_IN_PER_EMAIL {
            let bucket = format!("c{}", i / SIGN_IN_PER_CLIENT);
            assert!(SignInSlots::reserve(&limiter, "", "t", &bucket).is_some(), "{i}");
        }
        // Client c0 is at its five, and the email at its thirty: a new client is refused, and
        // the refusal leaves its own key untouched.
        assert!(SignInSlots::reserve(&limiter, "", "t", "c0").is_none());
        assert!(SignInSlots::reserve(&limiter, "", "t", "new").is_none());
        assert!(!limiter.over("sign-in:email-client:t:new", 1, Duration::from_secs(60)));
        // Another email, and another scope, are apart.
        assert!(SignInSlots::reserve(&limiter, "", "u", "c0").is_some());
        assert!(SignInSlots::reserve(&limiter, "app:p:", "t", "c0").is_some());
        // A refund frees both.
        let limiter = RateLimiter::default();
        for _ in 0..20 {
            SignInSlots::reserve(&limiter, "", "t", "c").unwrap().refund();
        }
        for _ in 0..SIGN_IN_PER_CLIENT {
            assert!(SignInSlots::reserve(&limiter, "", "t", "c").is_some());
        }
        assert!(SignInSlots::reserve(&limiter, "", "t", "c").is_none());
    }

    use proptest::prelude::*;

    #[derive(Debug, Clone)]
    enum Op {
        Reserve,
        /// Refund the n-th (modulo) slot still held.
        Refund(usize),
        /// Drop the n-th (modulo) slot still held without deciding it: it stays counted.
        Abandon(usize),
        /// Move the clock forward by this many milliseconds.
        Advance(u64),
    }

    fn ops() -> impl Strategy<Value = Vec<Op>> {
        prop::collection::vec(
            prop_oneof![
                4 => Just(Op::Reserve),
                3 => any::<usize>().prop_map(Op::Refund),
                1 => any::<usize>().prop_map(Op::Abandon),
                2 => (0u64..250).prop_map(Op::Advance),
            ],
            0..200,
        )
    }

    proptest! {
        // The invariant on `RateLimiter`: the count for the current window is exactly the
        // reservations granted in it and not refunded (a refused one counts nothing), and so
        // never exceeds the limit.
        #[test]
        fn the_count_is_exactly_the_unrefunded_reservations_in_the_window(
            ops in ops(),
            limit in 1u32..6,
        ) {
            let limiter = RateLimiter::default();
            let window = Duration::from_millis(100);
            let mut now = Instant::now();
            let mut held: Vec<Instant> = Vec::new();
            let mut unrefunded: HashMap<Instant, u32> = HashMap::new();
            for op in ops {
                match op {
                    Op::Reserve => {
                        if let Some(started) = limiter.allow_at("k", limit, window, now) {
                            held.push(started);
                            *unrefunded.entry(started).or_default() += 1;
                        }
                    }
                    Op::Refund(n) if !held.is_empty() => {
                        let started = held.remove(n % held.len());
                        limiter.refund("k", started);
                        *unrefunded.get_mut(&started).unwrap() -= 1;
                    }
                    Op::Abandon(n) if !held.is_empty() => {
                        held.remove(n % held.len());
                    }
                    Op::Refund(_) | Op::Abandon(_) => {}
                    Op::Advance(ms) => now += Duration::from_millis(ms),
                }
                for (started, n) in &unrefunded {
                    prop_assert!(*n <= limit, "window {started:?} holds {n} > {limit}");
                }
                let current = limiter.windows.lock().unwrap().get("k").copied();
                if let Some((start, count)) = current {
                    let expected = unrefunded.get(&start).copied().unwrap_or(0);
                    prop_assert_eq!(count, expected, "window {:?}", start);
                }
            }
        }
    }

    // A1: the email's check bound is identity's budget over a window no shorter than identity's
    // `FAILURE_WINDOW_MS` (24 h, `services/identity/src/email/codes.ts`), and the window slides:
    // a check ages out 24 h after it was made, not when a window that began earlier ends.
    #[test]
    fn code_checks_slide_over_identitys_failure_window() {
        const IDENTITY_FAILURE_BUDGET: u32 = 10;
        const IDENTITY_FAILURE_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);
        assert!(CODE_CHECK_WINDOW >= IDENTITY_FAILURE_WINDOW);
        const { assert!(CODE_CHECKS_PER_EMAIL <= IDENTITY_FAILURE_BUDGET) };

        let limiter = RateLimiter::default();
        let (limit, window) = (CODE_CHECKS_PER_EMAIL, CODE_CHECK_WINDOW);
        let t0 = Instant::now();
        assert!(limiter.allow_logged_at("k", limit, window, t0).is_some());
        let late = t0 + window - Duration::from_secs(60);
        for _ in 1..limit {
            assert!(limiter.allow_logged_at("k", limit, window, late).is_some());
        }
        assert!(limiter.allow_logged_at("k", limit, window, t0 + window - Duration::from_secs(1)).is_none());
        // The first check has aged out, the other nine have not: exactly one is free.
        assert!(limiter.allow_logged_at("k", limit, window, t0 + window).is_some());
        assert!(limiter.allow_logged_at("k", limit, window, t0 + window).is_none());
        // Other keys are apart, and a refund frees exactly its own entry.
        let a = CodeCheckSlot::reserve(&limiter, "", "reset", "t").unwrap();
        assert!(CodeCheckSlot::reserve(&limiter, "app:p:", "reset", "t").is_some());
        assert!(CodeCheckSlot::reserve(&limiter, "", "verify", "t").is_some());
        for _ in 1..limit {
            CodeCheckSlot::reserve(&limiter, "", "reset", "t").unwrap();
        }
        assert!(CodeCheckSlot::reserve(&limiter, "", "reset", "t").is_none());
        a.refund();
        assert!(CodeCheckSlot::reserve(&limiter, "", "reset", "t").is_some());
        assert!(CodeCheckSlot::reserve(&limiter, "", "reset", "t").is_none());
    }

    // A1: concurrent reservations for one email never grant more than the bound.
    #[test]
    fn concurrent_code_checks_never_pass_the_bound() {
        let limiter = RateLimiter::default();
        let granted = std::sync::atomic::AtomicU32::new(0);
        std::thread::scope(|scope| {
            for _ in 0..32 {
                scope.spawn(|| {
                    for _ in 0..4 {
                        if let Some(slot) = CodeCheckSlot::reserve(&limiter, "", "reset", "t") {
                            granted.fetch_add(1, Ordering::Relaxed);
                            std::mem::forget(slot);
                        }
                    }
                });
            }
        });
        assert_eq!(granted.load(Ordering::Relaxed), CODE_CHECKS_PER_EMAIL);
    }

    proptest! {
        // The invariant on the sliding logs: a reservation is granted exactly when fewer than
        // `limit` granted, unrefunded reservations are younger than the window, and the log
        // holds exactly those.
        #[test]
        fn a_log_holds_exactly_the_unrefunded_reservations_in_the_window(
            ops in ops(),
            limit in 1u32..6,
        ) {
            let limiter = RateLimiter::default();
            let window = Duration::from_millis(100);
            let mut now = Instant::now();
            let mut held: Vec<u64> = Vec::new();
            let mut model: Vec<(Instant, u64)> = Vec::new();
            for op in ops {
                match op {
                    Op::Reserve => {
                        model.retain(|(at, _)| now.duration_since(*at) < window);
                        let got = limiter.allow_logged_at("k", limit, window, now);
                        prop_assert_eq!(got.is_some(), model.len() < limit as usize);
                        if let Some(id) = got {
                            held.push(id);
                            model.push((now, id));
                        }
                    }
                    Op::Refund(n) if !held.is_empty() => {
                        let id = held.remove(n % held.len());
                        limiter.refund_logged("k", id);
                        model.retain(|(_, e)| *e != id);
                    }
                    Op::Abandon(n) if !held.is_empty() => {
                        held.remove(n % held.len());
                    }
                    Op::Refund(_) | Op::Abandon(_) => {}
                    Op::Advance(ms) => now += Duration::from_millis(ms),
                }
                let live = |log: &[(Instant, u64)]| {
                    log.iter().filter(|(at, _)| now.duration_since(*at) < window).map(|(_, id)| *id).collect::<Vec<_>>()
                };
                let logs = limiter.logs.lock().unwrap();
                let actual = live(logs.get("k").map(Vec::as_slice).unwrap_or(&[]));
                prop_assert_eq!(actual, live(&model));
            }
        }
    }

    #[tokio::test]
    async fn pad_waits_for_the_minimum() {
        let start = Instant::now();
        pad(start, Duration::from_millis(80)).await;
        assert!(start.elapsed() >= Duration::from_millis(80));
    }
}
