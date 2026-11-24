//! Data API and storage requests, limited per key and client (#322).
//!
//! The gateway sees every data API (`rest`) and storage request and knows its key, so one
//! token bucket per client of a key is kept here, not in each service:
//!
//! - **Who a bucket belongs to.** A request with a verified user token draws on its key's
//!   bucket for that user (`(key, user)`); a request without one on its key's bucket for the
//!   client's address (`(key, address)`, IPv6 by /64, as every per-address limit here). Each
//!   request draws on exactly one bucket. There is no bucket for the key as a whole: the public
//!   key ships in every browser, so a shared bucket would let anyone who reads an app's page
//!   use up the app's whole allowance (every user of the app limited), and one sized for a busy
//!   app would limit nothing.
//! - **Secret keys aren't limited.** They are server-side: one app server sends every user's
//!   requests on one key from one address, so any per-key limit would cap the app itself, and a
//!   leaked secret key is answered by revoking it, not by a rate.
//! - **What counts.** Only requests the gateway is about to proxy: a request refused before
//!   its key is resolved (missing or unknown key, the cache-miss limit), for a key of the
//!   wrong kind or for a bad user token never takes a token, and neither does the 429 itself.
//! - **Budgets.** Data API and storage requests have one budget each
//!   (`CHELABASE_GATEWAY_{DATA,STORAGE}_RATE_{PER_SECOND,BURST}`), so a page loading a hundred
//!   images doesn't hold back its queries; `CHELABASE_GATEWAY_STORAGE_RATE_SHARED=true` makes
//!   storage requests draw on the data API's budget instead.
//! - Each budget's buckets are a [`TokenBuckets`] map of at most [`CAPACITY`] clients.

use crate::buckets::TokenBuckets;
use crate::data_keys::{limiter_key, ResolvedKey};
use chelabase_routes::Service;
use chelabase_types::data_key::DataKeyKind;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Requests a client may send in a burst, by default.
pub const DEFAULT_BURST: u32 = 300;
/// Requests a second a client may keep sending, by default.
pub const DEFAULT_PER_SECOND: u32 = 100;
/// The most clients tracked at once, per budget.
pub const CAPACITY: usize = 100_000;

/// One budget: `burst` requests at once, refilled at `per_second`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub per_second: u32,
    pub burst: u32,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            per_second: DEFAULT_PER_SECOND,
            burst: DEFAULT_BURST,
        }
    }
}

/// The limits' settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RequestLimitsConfig {
    pub data: Budget,
    pub storage: Budget,
    /// Storage requests draw on the data API's budget (`storage` is unused).
    pub shared: bool,
}

/// Whose bucket a request draws on, within its key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Client {
    User(Uuid),
    Address(IpAddr),
}

type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

pub struct RequestLimits {
    data: Arc<TokenBuckets<(Uuid, Client)>>,
    storage: Arc<TokenBuckets<(Uuid, Client)>>,
}

impl Default for RequestLimits {
    fn default() -> Self {
        Self::new(&RequestLimitsConfig::default())
    }
}

impl RequestLimits {
    pub fn new(config: &RequestLimitsConfig) -> Self {
        Self::build(config, CAPACITY, Arc::new(Instant::now))
    }

    /// Tests use a small capacity and their own clock.
    pub fn build(config: &RequestLimitsConfig, capacity: usize, clock: Clock) -> Self {
        let budget = |b: &Budget| {
            let clock = clock.clone();
            Arc::new(
                TokenBuckets::new(
                    b.burst,
                    Duration::from_secs(1) / b.per_second.max(1),
                    capacity,
                )
                .with_clock(move || clock()),
            )
        };
        let data = budget(&config.data);
        let storage = if config.shared {
            data.clone()
        } else {
            budget(&config.storage)
        };
        Self { data, storage }
    }

    /// A request for `service` on `key`, by `user` (a verified user token's subject) from
    /// `client_ip`, is about to be proxied: whether it may go (it takes a token), and when not,
    /// how long until it may.
    pub fn check(
        &self,
        service: Service,
        key: &ResolvedKey,
        user: Option<Uuid>,
        client_ip: Option<IpAddr>,
    ) -> Result<(), Duration> {
        if key.kind != DataKeyKind::Public {
            return Ok(());
        }
        let buckets = match service {
            Service::Rest => &self.data,
            Service::Storage => &self.storage,
            _ => return Ok(()),
        };
        let client = match user {
            Some(user) => Client::User(user),
            None => Client::Address(limiter_key(client_ip)),
        };
        buckets.try_take((key.key_id, client))
    }

    /// How many clients have an entry in `service`'s budget (tests).
    pub fn tracked(&self, service: Service) -> usize {
        match service {
            Service::Storage => self.storage.tracked(),
            _ => self.data.tracked(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buckets::retry_after_secs;
    use std::sync::Mutex;

    /// A clock the test moves by hand.
    #[derive(Clone)]
    struct TestClock(Arc<Mutex<Instant>>);

    impl TestClock {
        fn new() -> Self {
            Self(Arc::new(Mutex::new(Instant::now())))
        }
        fn advance(&self, by: Duration) {
            *self.0.lock().unwrap() += by;
        }
    }

    fn limits(config: RequestLimitsConfig, capacity: usize) -> (RequestLimits, TestClock) {
        let clock = TestClock::new();
        let c = clock.clone();
        let limits =
            RequestLimits::build(&config, capacity, Arc::new(move || *c.0.lock().unwrap()));
        (limits, clock)
    }

    fn small(per_second: u32, burst: u32) -> RequestLimitsConfig {
        let budget = Budget { per_second, burst };
        RequestLimitsConfig {
            data: budget,
            storage: budget,
            shared: false,
        }
    }

    fn key(n: u128, kind: DataKeyKind) -> ResolvedKey {
        ResolvedKey {
            key_id: Uuid::from_u128(n),
            branch_id: Uuid::from_u128(0xb),
            project_id: Uuid::from_u128(0xc),
            kind,
            role: "anon".into(),
            databases: None,
            role_oid: None,
        }
    }

    fn public(n: u128) -> ResolvedKey {
        key(n, DataKeyKind::Public)
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

    /// Sends `n` requests that must all pass.
    fn pass(
        l: &RequestLimits,
        n: u32,
        service: Service,
        k: &ResolvedKey,
        user: Option<Uuid>,
        from: Option<IpAddr>,
    ) {
        for i in 0..n {
            assert_eq!(
                l.check(service, k, user, from),
                Ok(()),
                "request {i} is within the burst"
            );
        }
    }

    const A: &str = "203.0.113.9";

    #[test]
    fn requests_past_the_burst_get_a_wait_that_rounds_up_to_whole_seconds() {
        let (l, clock) = limits(small(2, 5), 100);
        let k = public(1);
        pass(&l, 5, Service::Rest, &k, None, ip(A));
        // Empty: one token comes back every 500 ms.
        let wait = l
            .check(Service::Rest, &k, None, ip(A))
            .expect_err("over the limit");
        assert_eq!(wait, Duration::from_millis(500));
        assert_eq!(retry_after_secs(wait), 1);
        // A refused request takes nothing: the wait only shrinks.
        clock.advance(Duration::from_millis(300));
        assert_eq!(
            l.check(Service::Rest, &k, None, ip(A)),
            Err(Duration::from_millis(200))
        );
        clock.advance(Duration::from_millis(200));
        assert_eq!(l.check(Service::Rest, &k, None, ip(A)), Ok(()));
        assert!(l.check(Service::Rest, &k, None, ip(A)).is_err());
        // A slower rate waits longer, and Retry-After rounds up.
        let (slow, _clock) = limits(small(1, 1), 100);
        slow.check(Service::Rest, &k, None, ip(A)).unwrap();
        let wait = slow.check(Service::Rest, &k, None, ip(A)).unwrap_err();
        assert_eq!(wait, Duration::from_secs(1));
        assert_eq!(retry_after_secs(wait), 1);
        assert_eq!(retry_after_secs(Duration::from_millis(1500)), 2);
    }

    #[test]
    fn the_bucket_refills_at_the_rate_up_to_the_burst() {
        let (l, clock) = limits(small(10, 20), 100);
        let k = public(1);
        pass(&l, 20, Service::Rest, &k, None, ip(A));
        assert!(l.check(Service::Rest, &k, None, ip(A)).is_err());
        // Half a second refills five.
        clock.advance(Duration::from_millis(500));
        pass(&l, 5, Service::Rest, &k, None, ip(A));
        assert!(l.check(Service::Rest, &k, None, ip(A)).is_err());
        // A quiet minute refills to the burst, never past it, and forgets the client.
        clock.advance(Duration::from_secs(60));
        pass(&l, 20, Service::Rest, &k, None, ip(A));
        assert!(l.check(Service::Rest, &k, None, ip(A)).is_err());
        clock.advance(Duration::from_secs(60));
        assert_eq!(l.check(Service::Rest, &k, None, ip(A)), Ok(()));
    }

    #[test]
    fn limits_are_per_key() {
        let (l, _clock) = limits(small(1, 3), 100);
        pass(&l, 3, Service::Rest, &public(1), None, ip(A));
        assert!(l.check(Service::Rest, &public(1), None, ip(A)).is_err());
        // Another key from the same client has its own bucket.
        pass(&l, 3, Service::Rest, &public(2), None, ip(A));
    }

    #[test]
    fn anonymous_requests_are_limited_per_client_address() {
        let (l, _clock) = limits(small(1, 3), 100);
        let k = public(1);
        pass(&l, 3, Service::Rest, &k, None, ip(A));
        assert!(l.check(Service::Rest, &k, None, ip(A)).is_err());
        // The same key from another address is another client.
        pass(&l, 3, Service::Rest, &k, None, ip("203.0.113.10"));
        // A dual-stack socket's mapped address is its IPv4 one.
        assert!(l
            .check(Service::Rest, &k, None, ip("::ffff:203.0.113.9"))
            .is_err());
        // One IPv6 /64 is one client; the next /64 is another.
        pass(&l, 3, Service::Rest, &k, None, ip("2001:db8:aa:bb::1"));
        assert!(l
            .check(Service::Rest, &k, None, ip("2001:db8:aa:bb:ffff::2"))
            .is_err());
        pass(&l, 3, Service::Rest, &k, None, ip("2001:db8:aa:bc::1"));
    }

    #[test]
    fn signed_in_users_have_their_own_buckets() {
        let (l, _clock) = limits(small(1, 3), 100);
        let k = public(1);
        let (alice, bob) = (Some(Uuid::from_u128(0xa)), Some(Uuid::from_u128(0xb0b)));
        pass(&l, 3, Service::Rest, &k, alice, ip(A));
        assert!(l.check(Service::Rest, &k, alice, ip(A)).is_err());
        // Another user of the same key from the same address (an app server, a shared network).
        pass(&l, 3, Service::Rest, &k, bob, ip(A));
        // The anonymous bucket of that address is a third one, and using it up limits no user.
        pass(&l, 3, Service::Rest, &k, None, ip(A));
        assert!(l.check(Service::Rest, &k, None, ip(A)).is_err());
        // A user is one client wherever they are.
        assert!(l
            .check(Service::Rest, &k, alice, ip("198.51.100.1"))
            .is_err());
        // The same user on another key is another bucket.
        pass(&l, 3, Service::Rest, &public(2), alice, ip(A));
    }

    #[test]
    fn secret_keys_and_other_services_are_not_limited() {
        let (l, _clock) = limits(small(1, 1), 100);
        let secret = key(1, DataKeyKind::Secret);
        for service in [Service::Rest, Service::Storage] {
            pass(&l, 50, service, &secret, None, ip(A));
        }
        for service in [Service::Functions, Service::Auth, Service::Realtime] {
            pass(&l, 50, service, &public(2), None, ip(A));
        }
        assert_eq!(l.tracked(Service::Rest) + l.tracked(Service::Storage), 0);
    }

    #[test]
    fn storage_and_data_budgets_are_separate_by_default() {
        let config = RequestLimitsConfig {
            data: Budget {
                per_second: 1,
                burst: 2,
            },
            storage: Budget {
                per_second: 1,
                burst: 4,
            },
            shared: false,
        };
        let (l, _clock) = limits(config, 100);
        let k = public(1);
        pass(&l, 2, Service::Rest, &k, None, ip(A));
        assert!(l.check(Service::Rest, &k, None, ip(A)).is_err());
        // Storage has its own budget, with its own burst.
        pass(&l, 4, Service::Storage, &k, None, ip(A));
        assert!(l.check(Service::Storage, &k, None, ip(A)).is_err());
    }

    #[test]
    fn a_shared_budget_counts_storage_against_the_data_api() {
        let config = RequestLimitsConfig {
            data: Budget {
                per_second: 1,
                burst: 4,
            },
            storage: Budget {
                per_second: 100,
                burst: 100,
            },
            shared: true,
        };
        let (l, _clock) = limits(config, 100);
        let k = public(1);
        pass(&l, 2, Service::Rest, &k, None, ip(A));
        pass(&l, 2, Service::Storage, &k, None, ip(A));
        assert!(
            l.check(Service::Storage, &k, None, ip(A)).is_err(),
            "storage's own budget is unused"
        );
        assert!(l.check(Service::Rest, &k, None, ip(A)).is_err());
    }

    #[test]
    fn a_full_map_still_limits_new_clients_and_stays_bounded() {
        let (l, _clock) = limits(small(1, 3), 64);
        let k = public(1);
        let limited = ip(A);
        pass(&l, 3, Service::Rest, &k, None, limited);
        // Ten times the bound of new clients, one request each, then each new one limited.
        for n in 0..640 {
            assert_eq!(l.check(Service::Rest, &k, None, nth(n)), Ok(()));
            assert!(l.tracked(Service::Rest) <= 64);
        }
        assert!(
            l.check(Service::Rest, &k, None, limited).is_err(),
            "the flood didn't free a limited client"
        );
        for n in 1000..1200 {
            pass(&l, 3, Service::Rest, &k, None, nth(n));
            assert!(
                l.check(Service::Rest, &k, None, nth(n)).is_err(),
                "client {n} is limited though the map was full"
            );
            assert!(l.tracked(Service::Rest) <= 64);
        }
    }
}
