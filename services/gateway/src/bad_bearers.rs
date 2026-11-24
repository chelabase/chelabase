//! Refused `/mcp` bearers, limited per client address (#395).
//!
//! The gateway passes the `/mcp` bearer through unchecked; the mcp service refuses a bad one but
//! never sees the client's address. So the gateway counts every 401 a `Service::Mcp` endpoint
//! answers (its own, for a missing bearer, and the upstream's) against the client's address and
//! answers 429 with `Retry-After` once an address is over the limit.
//!
//! - A token bucket per address ([`limiter_key`]: IPv6 by /64, unknown addresses share one):
//!   [`BURST`] refusals at once, then one more every [`REFILL_EVERY`]. Only a refusal takes a
//!   token; the check before a request takes none, so successful requests never count and a
//!   long-held `POST /mcp` holds no limiter state.
//! - Refusals that were already in flight when the bucket ran dry still count, down to
//!   `-BURST`: a flood of concurrent bad bearers (all past the check before the first answer)
//!   buys a longer cool-down, at most `(BURST + 1) * REFILL_EVERY` (62 s).
//! - Only addresses with a refusal not yet refilled have an entry (a full bucket is no entry),
//!   so a client that never sends a bad bearer has nothing to lose or evict.
//! - At most [`CAPACITY`] entries. When a new address needs one and the map is full, refilled
//!   entries are dropped first; then, down to seven eighths of the capacity, those with the
//!   most tokens left go (the least-limited addresses, which forgetting frees the least). So a
//!   full map costs one pass per `capacity / 8` new addresses, however many had refilled. A new address is always
//!   admitted, so a full map never stops limiting new clients, and a limited address is only
//!   forgotten once nearly every other entry is as limited as it is ([`TokenBuckets`]).

use crate::buckets::TokenBuckets;
pub use crate::buckets::retry_after_secs;
use crate::data_keys::limiter_key;
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// Refusals an address may have at once.
pub const BURST: u32 = 30;
/// One more refusal is allowed this often (30 a minute; an idle address is forgotten after
/// a minute).
pub const REFILL_EVERY: Duration = Duration::from_secs(2);
/// The most addresses tracked at once.
pub const CAPACITY: usize = 100_000;

pub struct BadBearerLimiter {
    buckets: TokenBuckets<IpAddr>,
}

impl Default for BadBearerLimiter {
    fn default() -> Self {
        Self::with_limits(BURST, REFILL_EVERY, CAPACITY)
    }
}

impl BadBearerLimiter {
    /// Tests use small limits.
    pub fn with_limits(burst: u32, refill_every: Duration, capacity: usize) -> Self {
        Self {
            buckets: TokenBuckets::new(burst, refill_every.max(Duration::from_millis(1)), capacity),
        }
    }

    /// Replaces the clock (tests).
    pub fn with_clock(mut self, clock: impl Fn() -> Instant + Send + Sync + 'static) -> Self {
        self.buckets = self.buckets.with_clock(clock);
        self
    }

    /// Whether a request from `ip` may go on; when not, how long until it may. Takes nothing.
    pub fn check(&self, ip: Option<IpAddr>) -> Result<(), Duration> {
        self.buckets.wait(&limiter_key(ip))
    }

    /// `ip` was answered 401 on an mcp endpoint: it takes one token.
    pub fn refused(&self, ip: Option<IpAddr>) {
        self.buckets.charge(limiter_key(ip));
    }

    /// How many addresses have an entry.
    pub fn tracked(&self) -> usize {
        self.buckets.tracked()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    /// A clock the test moves by hand.
    #[derive(Clone)]
    struct Clock(Arc<StdMutex<Instant>>);

    impl Clock {
        fn new() -> Self {
            Self(Arc::new(StdMutex::new(Instant::now())))
        }
        fn advance(&self, by: Duration) {
            *self.0.lock().unwrap() += by;
        }
        fn now(&self) -> Instant {
            *self.0.lock().unwrap()
        }
    }

    fn limiter(burst: u32, capacity: usize) -> (BadBearerLimiter, Clock) {
        let clock = Clock::new();
        let c = clock.clone();
        let limiter = BadBearerLimiter::with_limits(burst, Duration::from_secs(2), capacity)
            .with_clock(move || c.now());
        (limiter, clock)
    }

    fn ip(raw: &str) -> Option<IpAddr> {
        Some(raw.parse().unwrap())
    }

    /// The `n`th address of 198.18.0.0/15 (benchmarking space).
    fn nth(n: u32) -> Option<IpAddr> {
        Some(IpAddr::V4(std::net::Ipv4Addr::from(
            (198u32 << 24 | 18 << 16) + n,
        )))
    }

    #[test]
    fn repeated_refusals_limit_the_address_with_a_retry_after() {
        let (l, clock) = limiter(5, 100);
        let a = ip("203.0.113.9");
        for i in 0..5 {
            assert_eq!(l.check(a), Ok(()), "refusal {i} is still within the burst");
            l.refused(a);
        }
        let wait = l.check(a).expect_err("over the limit");
        assert_eq!(wait, Duration::from_secs(2));
        assert_eq!(retry_after_secs(wait), 2);
        // A token comes back after the wait, and a further refusal takes it again.
        clock.advance(Duration::from_secs(1));
        assert_eq!(retry_after_secs(l.check(a).unwrap_err()), 1);
        clock.advance(Duration::from_secs(1));
        assert_eq!(l.check(a), Ok(()));
        l.refused(a);
        assert!(l.check(a).is_err());
        // A quiet minute forgets it.
        clock.advance(Duration::from_secs(60));
        assert_eq!(l.check(a), Ok(()));
        assert_eq!(l.tracked(), 0);
    }

    #[test]
    fn another_address_is_not_affected() {
        let (l, _clock) = limiter(3, 100);
        let a = ip("203.0.113.9");
        for _ in 0..3 {
            l.refused(a);
        }
        assert!(l.check(a).is_err());
        assert_eq!(l.check(ip("203.0.113.10")), Ok(()));
        assert_eq!(l.check(ip("2001:db8:1::1")), Ok(()));
        assert_eq!(l.check(None), Ok(()), "an unknown address is its own key");
        // One IPv6 /64 is one client; the next /64 is another.
        let v6 = ip("2001:db8:aa:bb::1");
        for _ in 0..3 {
            l.refused(v6);
        }
        assert!(l.check(ip("2001:db8:aa:bb:ffff::2")).is_err());
        assert_eq!(l.check(ip("2001:db8:aa:bc::1")), Ok(()));
        // A dual-stack socket's mapped address is its IPv4 one.
        assert!(l.check(ip("::ffff:203.0.113.9")).is_err());
    }

    #[test]
    fn checks_alone_never_count() {
        let (l, _clock) = limiter(3, 100);
        let a = ip("203.0.113.9");
        for _ in 0..1000 {
            assert_eq!(l.check(a), Ok(()));
        }
        assert_eq!(
            l.tracked(),
            0,
            "a client that is never refused has no entry"
        );
        l.refused(a);
        for _ in 0..1000 {
            assert_eq!(l.check(a), Ok(()));
        }
        l.refused(a);
        assert_eq!(
            l.check(a),
            Ok(()),
            "two refusals of three, however many checks between"
        );
        l.refused(a);
        assert!(l.check(a).is_err());
    }

    #[test]
    fn refusals_in_flight_lengthen_the_cool_down_up_to_a_bound() {
        let (l, clock) = limiter(3, 100);
        let a = ip("203.0.113.9");
        // A hundred bad bearers passed the check at once; each answer is a refusal.
        for _ in 0..100 {
            l.refused(a);
        }
        // Debt stops at -burst: from -3 to 1 token is 4 refills of 2 s.
        assert_eq!(l.check(a), Err(Duration::from_secs(8)));
        clock.advance(Duration::from_secs(8));
        assert_eq!(l.check(a), Ok(()));
    }

    #[test]
    fn a_full_limiter_still_limits_new_clients() {
        let (l, _clock) = limiter(3, 64);
        for n in 0..64 {
            l.refused(nth(n));
        }
        assert_eq!(l.tracked(), 64);
        // Every new address past the bound is still counted and limited.
        for n in 64..200 {
            let a = nth(n);
            for _ in 0..3 {
                assert_eq!(l.check(a), Ok(()));
                l.refused(a);
            }
            assert!(
                l.check(a).is_err(),
                "address {n} is limited though the map was full"
            );
            assert!(l.tracked() <= 64);
        }
    }

    #[test]
    fn a_flood_of_new_addresses_neither_frees_a_limited_one_nor_limits_a_clean_one() {
        let (l, _clock) = limiter(3, 64);
        let attacker = ip("203.0.113.9");
        for _ in 0..3 {
            l.refused(attacker);
        }
        // A client refused once: still within its allowance.
        let member = ip("203.0.113.20");
        l.refused(member);
        // Ten times the bound of fresh addresses, one refusal each.
        for n in 0..640 {
            l.refused(nth(n));
            assert!(l.tracked() <= 64);
        }
        assert!(
            l.check(attacker).is_err(),
            "the flood didn't free the limited address"
        );
        assert_eq!(l.check(member), Ok(()), "nor limit one that wasn't");
        assert_eq!(
            l.check(ip("203.0.113.30")),
            Ok(()),
            "nor one that was never refused"
        );
    }

    #[test]
    fn refilled_entries_are_dropped_before_any_other() {
        let (l, clock) = limiter(3, 8);
        for n in 0..7 {
            l.refused(nth(n));
        }
        clock.advance(Duration::from_secs(60));
        let attacker = ip("203.0.113.9");
        for _ in 0..3 {
            l.refused(attacker);
        }
        // The map is full; the next address frees the refilled ones and nothing else.
        l.refused(nth(100));
        assert_eq!(l.tracked(), 2);
        assert!(l.check(attacker).is_err());
    }

    /// A full map scans all its entries at most once per `capacity / 8` new addresses, even
    /// when only a few entries have refilled each time (refills spread out in time, as steady
    /// traffic from many addresses gives).
    #[test]
    fn a_full_map_scans_at_most_once_per_eighth_of_new_addresses_under_staggered_refills() {
        const CAPACITY: u32 = 1000;
        const NEW: u32 = 5000;
        let (l, clock) = limiter(3, CAPACITY as usize);
        // One refusal each, spread over one refill: an entry refills every step from now on.
        let step = Duration::from_secs(2) / CAPACITY;
        for n in 0..CAPACITY {
            l.refused(nth(n));
            clock.advance(step);
        }
        assert_eq!(l.tracked(), CAPACITY as usize);
        // A new address that doesn't grow the map is one that made room, a full scan.
        let mut scans = 0;
        for n in CAPACITY..CAPACITY + NEW {
            let before = l.tracked();
            l.refused(nth(n));
            let after = l.tracked();
            assert!(after <= CAPACITY as usize);
            if after <= before {
                scans += 1;
            }
            clock.advance(step);
        }
        let bound = NEW / (CAPACITY / 8) + 1;
        assert!(
            scans <= bound,
            "{scans} of {NEW} new addresses ran a full scan (amortised bound {bound})"
        );
    }

    #[test]
    fn retry_after_rounds_up_to_whole_seconds() {
        assert_eq!(retry_after_secs(Duration::ZERO), 1);
        assert_eq!(retry_after_secs(Duration::from_millis(1)), 1);
        assert_eq!(retry_after_secs(Duration::from_millis(1001)), 2);
        assert_eq!(retry_after_secs(Duration::from_secs(8)), 8);
    }
}
