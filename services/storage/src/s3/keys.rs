//! S3 access keys for the S3 endpoint (s3-endpoint m5): an access key id resolves to its
//! secret and scope ([`S3Key`]) from a cache, or from the orchestrator's internal listener
//! ([`Control::s3_key`]).
//!
//! - **Lifetime.** An answer (a live key, or "unknown") is trusted for [`LIVENESS_TTL`]. NATS
//!   drops entries at once ([`crate::events::apply`]): a key event drops that key, a branch
//!   event the branch's keys, an access event everything. A drop that lands while a lookup is
//!   in flight wins: that answer is returned but not cached ([`Fenced`]).
//! - **Bounds.** Live keys and unknown ids are cached apart, at most [`MAX_LIVE`] and
//!   [`MAX_UNKNOWN`]: a flood of random ids evicts unknown ids, never live keys.
//! - **Miss limiter.** A lookup that reaches the orchestrator is charged to its client's token
//!   bucket ([`MISSES_PER_SECOND`]); past it the answer is [`KeyError::Limited`] and the
//!   orchestrator isn't asked. Cached answers and malformed ids are never charged. The client
//!   is the gateway's `X-Forwarded-For` ([`client_ip`]): an IPv4 address, or an IPv6 address's
//!   /64. The buckets are a bounded map ([`MAX_CLIENTS`], forgotten after [`CLIENT_IDLE`]).
//! - **The secret** is an [`S3Secret`](crate::control::S3Secret): wiped when dropped, never
//!   printed or serialised.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use chelabase_types::s3_key::is_access_key_id;
use uuid::Uuid;

use crate::cache::Fenced;
use crate::control::{Control, LIVENESS_TTL, S3Key};

/// The most live keys cached.
pub const MAX_LIVE: u64 = 100_000;
/// The most unknown ids cached (apart from the live keys).
pub const MAX_UNKNOWN: u64 = 10_000;
/// Orchestrator lookups a client may cause per second (the bucket's size and refill rate).
pub const MISSES_PER_SECOND: u32 = 20;
/// The most clients the miss limiter tracks.
pub const MAX_CLIENTS: u64 = 100_000;
/// How long an idle client's bucket is kept (a full bucket is the default anyway).
pub const CLIENT_IDLE: Duration = Duration::from_secs(60);

/// Why there's no key. The S3 listener maps these to `InvalidAccessKeyId`, `SlowDown` and
/// `ServiceUnavailable`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyError {
    /// No live key has this id (or the id isn't one).
    Unknown,
    /// The client is past its miss limit: the orchestrator wasn't asked.
    Limited,
    /// The orchestrator couldn't answer, or answered something unreadable (logged).
    Unavailable,
}

/// The S3 key store (see the module docs).
pub struct S3Keys {
    control: Arc<dyn Control>,
    live: Fenced<String, S3Key>,
    unknown: Fenced<String, ()>,
    limiter: MissLimiter,
}

impl S3Keys {
    pub fn new(control: Arc<dyn Control>) -> Self {
        Self::with_config(control, LIVENESS_TTL, Arc::new(Instant::now))
    }

    fn with_config(
        control: Arc<dyn Control>,
        ttl: Duration,
        clock: Arc<dyn Fn() -> Instant + Send + Sync>,
    ) -> Self {
        S3Keys {
            control,
            live: Fenced::new(MAX_LIVE, ttl),
            unknown: Fenced::new(MAX_UNKNOWN, ttl),
            limiter: MissLimiter {
                buckets: moka::sync::Cache::builder()
                    .max_capacity(MAX_CLIENTS)
                    .time_to_idle(CLIENT_IDLE)
                    .build(),
                rate: f64::from(MISSES_PER_SECOND),
                clock,
            },
        }
    }

    /// The live key `access_key_id` names, for a request from `client_ip`.
    pub async fn get(
        &self,
        access_key_id: &str,
        client_ip: Option<IpAddr>,
    ) -> Result<S3Key, KeyError> {
        // Never a lookup, a charge or a cache entry for what can't be an id.
        if !is_access_key_id(access_key_id) {
            return Err(KeyError::Unknown);
        }
        let id = access_key_id.to_owned();
        // Two fenced caches, one inside the other: the unknown ids', whose lookup is the live
        // keys' cache, whose lookup asks the orchestrator. An unknown id is the only `Ok` the
        // outer one sees (so only it is cached there); a live key or a failure passes through
        // it as an `Err`. A drop during a lookup bumps the fence of the cache that would keep
        // its answer, so that answer is returned but not cached.
        let outcome = self
            .unknown
            .get_or_lookup(id.clone(), async {
                match self
                    .live
                    .get_or_lookup(id.clone(), self.fetch(&id, client_ip))
                    .await
                {
                    Ok(key) => Err(Ok(key)),
                    Err(KeyError::Unknown) => Ok(()),
                    Err(e) => Err(Err(e)),
                }
            })
            .await;
        match outcome {
            Ok(()) => Err(KeyError::Unknown),
            Err(found) => found,
        }
    }

    /// Both caches missed: one trip to the orchestrator, charged to the client.
    async fn fetch(
        &self,
        access_key_id: &str,
        client_ip: Option<IpAddr>,
    ) -> Result<S3Key, KeyError> {
        if !self.limiter.allow(client_ip) {
            return Err(KeyError::Limited);
        }
        match self.control.s3_key(access_key_id).await {
            Ok(Some(key)) => Ok(key),
            Ok(None) => Err(KeyError::Unknown),
            Err(e) => {
                // The access key id is public (it travels in every request); the error never
                // quotes the answer ([`crate::control::parse_s3_key`]).
                tracing::warn!(access_key_id, "the orchestrator's S3 key route: {}", e.0);
                Err(KeyError::Unavailable)
            }
        }
    }

    /// Forgets the key `key_id` (NATS: it was revoked or changed).
    pub fn forget_key(&self, key_id: Uuid) {
        self.live.forget_if(move |_, key| key.key_id == key_id);
    }

    /// Forgets every key on `branch` (NATS: the branch changed).
    pub fn forget_branch(&self, branch: Uuid) {
        self.live.forget_if(move |_, key| key.branch_id == branch);
    }

    /// Forgets everything (NATS: a member's access changed).
    pub fn forget_all(&self) {
        self.live.forget_all();
        self.unknown.forget_all();
    }
}

/// The client a request through the gateway comes from: the last `X-Forwarded-For` entry,
/// which the gateway sets (replacing whatever the client sent). `None` when absent or not an
/// address: such requests share one bucket.
pub fn client_ip(headers: &HeaderMap) -> Option<IpAddr> {
    let last = headers.get_all("x-forwarded-for").iter().next_back()?;
    last.to_str().ok()?.rsplit(',').next()?.trim().parse().ok()
}

/// A token bucket per client (see the module docs).
struct MissLimiter {
    buckets: moka::sync::Cache<IpAddr, Arc<Mutex<(f64, Instant)>>>,
    rate: f64,
    clock: Arc<dyn Fn() -> Instant + Send + Sync>,
}

impl MissLimiter {
    /// Takes a token from the client's bucket; false when it is empty.
    fn allow(&self, client_ip: Option<IpAddr>) -> bool {
        let now = (self.clock)();
        let bucket = self.buckets.get_with(client_key(client_ip), || {
            Arc::new(Mutex::new((self.rate, now)))
        });
        let mut state = bucket
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (tokens, last) = *state;
        let tokens =
            (tokens + now.saturating_duration_since(last).as_secs_f64() * self.rate).min(self.rate);
        if tokens >= 1.0 {
            *state = (tokens - 1.0, now);
            true
        } else {
            *state = (tokens, now);
            false
        }
    }
}

/// The bucket a client is charged to: an IPv4 address (also when written IPv4-mapped), an
/// IPv6 address's /64, or one shared bucket for no address.
fn client_key(ip: Option<IpAddr>) -> IpAddr {
    match ip {
        None => IpAddr::from([0u8; 4]),
        Some(IpAddr::V4(v4)) => IpAddr::V4(v4),
        Some(IpAddr::V6(v6)) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => {
                let mut octets = v6.octets();
                octets[8..].fill(0);
                IpAddr::from(octets)
            }
        },
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::Value;

    use super::*;
    use crate::control::{BoxFuture, LiveKey, Minter, S3Secret, Unreachable};
    use crate::token::UrlKey;

    /// A live key on `branch` with a distinctive secret.
    pub fn key(access_key_id: &str, branch: Uuid) -> S3Key {
        S3Key {
            key_id: Uuid::new_v4(),
            access_key_id: access_key_id.to_owned(),
            secret: S3Secret::new(format!("SeCrEt{}", "x".repeat(34))),
            branch_id: branch,
            project_id: Uuid::new_v4(),
            database: "app".into(),
            role: "app_writer".into(),
            role_oid: 16_384,
            role_timeline: "tl-1".into(),
        }
    }

    /// The `n`th well-formed access key id.
    pub fn id(n: u64) -> String {
        const B32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
        let body: String = (0..16)
            .map(|i| B32[(n.checked_shr(i * 5).unwrap_or(0) & 31) as usize] as char)
            .collect();
        format!("CBAK{body}")
    }

    /// The control plane's S3 keys: the ones in `keys` are live; counts the lookups. With
    /// `gated`, the first lookup says it started and waits to be let go.
    #[derive(Default)]
    pub struct FakeS3 {
        pub keys: Mutex<HashMap<String, S3Key>>,
        pub lookups: AtomicUsize,
        pub gated: bool,
        pub started: tokio::sync::Notify,
        pub go_on: tokio::sync::Notify,
    }

    impl FakeS3 {
        pub fn with(keys: &[&S3Key]) -> Arc<Self> {
            let fake = FakeS3::default();
            for k in keys {
                fake.keys
                    .lock()
                    .unwrap()
                    .insert(k.access_key_id.clone(), (*k).clone());
            }
            Arc::new(fake)
        }

        pub fn lookups(&self) -> usize {
            self.lookups.load(Ordering::SeqCst)
        }
    }

    impl Control for FakeS3 {
        fn url_key(&self) -> BoxFuture<'_, Result<UrlKey, Unreachable>> {
            Box::pin(async { Err(Unreachable("unused".into())) })
        }
        fn live_key(
            &self,
            _: Uuid,
            _: Uuid,
            _: &Minter,
        ) -> BoxFuture<'_, Result<Option<LiveKey>, Unreachable>> {
            Box::pin(async { Ok(None) })
        }
        fn record_upload<'a>(
            &'a self,
            _: Uuid,
            _: Uuid,
            _: &'a Value,
            _: u64,
        ) -> BoxFuture<'a, Result<Option<u64>, Unreachable>> {
            Box::pin(async { Ok(None) })
        }
        fn consume_upload(
            &self,
            _: Uuid,
        ) -> BoxFuture<'_, Result<Option<(LiveKey, Value)>, Unreachable>> {
            Box::pin(async { Ok(None) })
        }
        fn s3_key<'a>(
            &'a self,
            access_key_id: &'a str,
        ) -> BoxFuture<'a, Result<Option<S3Key>, Unreachable>> {
            let first = self.lookups.fetch_add(1, Ordering::SeqCst) == 0;
            Box::pin(async move {
                if first && self.gated {
                    self.started.notify_one();
                    self.go_on.notified().await;
                }
                Ok(self.keys.lock().unwrap().get(access_key_id).cloned())
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::test_support::{FakeS3, id, key};
    use super::*;
    use crate::control::Orchestrator;

    const IP: Option<IpAddr> = Some(IpAddr::V4(std::net::Ipv4Addr::new(203, 0, 113, 7)));

    /// A store over `control` whose miss buckets' clock is frozen (a slow run never refills a
    /// bucket the test drained).
    fn frozen(control: Arc<dyn Control>, ttl: Duration) -> S3Keys {
        let now = Instant::now();
        S3Keys::with_config(control, ttl, Arc::new(move || now))
    }

    /// AC 1: a live key (and an unknown id) is asked for once within the lifetime, and again
    /// after it.
    #[tokio::test]
    async fn cached_within_ttl() {
        let k = key(&id(1), Uuid::new_v4());
        let fake = FakeS3::with(&[&k]);
        let keys = frozen(fake.clone(), Duration::from_millis(300));
        assert_eq!(keys.get(&id(1), IP).await.unwrap().key_id, k.key_id);
        assert_eq!(keys.get(&id(1), IP).await.unwrap().key_id, k.key_id);
        assert_eq!(fake.lookups(), 1, "cached");
        assert_eq!(keys.get(&id(2), IP).await.unwrap_err(), KeyError::Unknown);
        assert_eq!(keys.get(&id(2), IP).await.unwrap_err(), KeyError::Unknown);
        assert_eq!(fake.lookups(), 2, "unknown cached too");
        tokio::time::sleep(Duration::from_millis(400)).await;
        keys.get(&id(1), IP).await.unwrap();
        keys.get(&id(2), IP).await.unwrap_err();
        assert_eq!(fake.lookups(), 4, "asked again after the lifetime");
        // The store's own lifetime is the liveness one.
        assert_eq!(LIVENESS_TTL, Duration::from_secs(5));
    }

    /// AC 2: a key, branch or access drop makes the next `get` ask again; other keys stay
    /// cached.
    #[tokio::test]
    async fn drops_by_key_branch_and_all() {
        let (b1, b2) = (Uuid::new_v4(), Uuid::new_v4());
        let (k1, k2) = (key(&id(1), b1), key(&id(2), b2));
        let fake = FakeS3::with(&[&k1, &k2]);
        let keys = frozen(fake.clone(), LIVENESS_TTL);
        let both = || async {
            keys.get(&id(1), IP).await.unwrap();
            keys.get(&id(2), IP).await.unwrap();
        };
        both().await;
        assert_eq!(fake.lookups(), 2);
        keys.forget_key(k1.key_id);
        both().await;
        assert_eq!(fake.lookups(), 3, "k1 asked again, k2 cached");
        keys.forget_key(Uuid::new_v4());
        keys.forget_branch(Uuid::new_v4());
        both().await;
        assert_eq!(
            fake.lookups(),
            3,
            "other keys' and branches' drops leave both cached"
        );
        keys.forget_branch(b2);
        both().await;
        assert_eq!(fake.lookups(), 4, "k2 asked again, k1 cached");
        keys.forget_all();
        both().await;
        assert_eq!(fake.lookups(), 6, "both asked again");
    }

    /// AC 3: a drop that lands while that key's lookup is in flight: the caller gets the
    /// answer, but it isn't cached.
    #[tokio::test]
    async fn forget_during_lookup_is_not_cached() {
        let branch = Uuid::new_v4();
        for drop in ["key", "branch", "all"] {
            let k = key(&id(1), branch);
            let mut fake = FakeS3 {
                gated: true,
                ..Default::default()
            };
            fake.keys.get_mut().unwrap().insert(id(1), k.clone());
            let fake = Arc::new(fake);
            let keys = Arc::new(frozen(fake.clone(), LIVENESS_TTL));
            let in_flight = tokio::spawn({
                let keys = keys.clone();
                async move { keys.get(&id(1), IP).await }
            });
            fake.started.notified().await;
            match drop {
                "key" => keys.forget_key(k.key_id),
                "branch" => keys.forget_branch(branch),
                _ => keys.forget_all(),
            }
            fake.go_on.notify_one();
            assert_eq!(
                in_flight.await.unwrap().unwrap().key_id,
                k.key_id,
                "{drop}: the caller gets it"
            );
            keys.get(&id(1), IP).await.unwrap();
            assert_eq!(fake.lookups(), 2, "{drop}: not cached");
            keys.get(&id(1), IP).await.unwrap();
            assert_eq!(fake.lookups(), 2, "{drop}: cached again after");
        }
    }

    /// AC 4: 20 lookups a second per IPv4 address, then `Limited` without asking; an IPv6 /64
    /// is one client, another /64 another. Cached keys are never limited.
    #[tokio::test]
    async fn miss_limiter_ipv4_and_ipv6_64() {
        let live = key(&id(1000), Uuid::new_v4());
        let fake = FakeS3::with(&[&live]);
        let keys = frozen(fake.clone(), LIVENESS_TTL);
        keys.get(&id(1000), IP).await.unwrap();
        for n in 1..MISSES_PER_SECOND as u64 {
            assert_eq!(
                keys.get(&id(n), IP).await.unwrap_err(),
                KeyError::Unknown,
                "{n}"
            );
        }
        assert_eq!(fake.lookups(), 20);
        assert_eq!(keys.get(&id(21), IP).await.unwrap_err(), KeyError::Limited);
        assert_eq!(fake.lookups(), 20, "the 21st isn't asked");
        // Written IPv4-mapped, it is the same client.
        let mapped = Some(IpAddr::V6(
            std::net::Ipv4Addr::new(203, 0, 113, 7).to_ipv6_mapped(),
        ));
        assert_eq!(
            keys.get(&id(22), mapped).await.unwrap_err(),
            KeyError::Limited
        );
        // A cached key (and a cached unknown id) still answers.
        assert_eq!(keys.get(&id(1000), IP).await.unwrap().key_id, live.key_id);
        assert_eq!(keys.get(&id(1), IP).await.unwrap_err(), KeyError::Unknown);
        assert_eq!(fake.lookups(), 20);

        let a: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let same64: IpAddr = "2001:db8:1:2:ffff:eeee:dddd:cccc".parse().unwrap();
        let other64: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        for n in 0..10 {
            keys.get(&id(100 + n), Some(a)).await.unwrap_err();
            keys.get(&id(200 + n), Some(same64)).await.unwrap_err();
        }
        assert_eq!(fake.lookups(), 40);
        assert_eq!(
            keys.get(&id(300), Some(a)).await.unwrap_err(),
            KeyError::Limited
        );
        assert_eq!(
            keys.get(&id(301), Some(same64)).await.unwrap_err(),
            KeyError::Limited
        );
        assert_eq!(
            keys.get(&id(302), Some(other64)).await.unwrap_err(),
            KeyError::Unknown
        );
        assert_eq!(fake.lookups(), 41);

        // The client is the last `X-Forwarded-For` entry the gateway set.
        let headers = |values: &[&str]| {
            let mut h = HeaderMap::new();
            for v in values {
                h.append("x-forwarded-for", v.parse().unwrap());
            }
            h
        };
        assert_eq!(client_ip(&headers(&["203.0.113.7"])), IP);
        assert_eq!(client_ip(&headers(&["198.51.100.1, 203.0.113.7"])), IP);
        assert_eq!(client_ip(&headers(&["198.51.100.1", " 203.0.113.7 "])), IP);
        assert_eq!(client_ip(&headers(&["2001:db8:1:2::1"])), Some(a));
        assert_eq!(client_ip(&headers(&["203.0.113.7, not-an-ip"])), None);
        assert_eq!(client_ip(&HeaderMap::new()), None);
        assert_eq!(client_key(None), client_key(None));
        assert_ne!(client_key(None), client_key(IP));
    }

    /// AC 5: 200 000 clients keep the limiter within its bound; 50 000 unknown ids never push
    /// out a live key cached before them.
    #[tokio::test]
    async fn bounded_maps() {
        let live = key(&id(u64::MAX), Uuid::new_v4());
        let fake = FakeS3::with(&[&live]);
        let keys = frozen(fake.clone(), Duration::from_secs(600));
        for n in 0..200_000u32 {
            assert!(keys.limiter.allow(Some(IpAddr::V4(n.into()))));
        }
        keys.limiter.buckets.run_pending_tasks();
        let clients = keys.limiter.buckets.entry_count();
        assert!(clients <= MAX_CLIENTS, "{clients} clients tracked");

        keys.get(&id(u64::MAX), IP).await.unwrap();
        for n in 0..50_000u32 {
            let ip = Some(IpAddr::V4(((n + 1) << 8).into()));
            assert_eq!(
                keys.get(&id(n as u64), ip).await.unwrap_err(),
                KeyError::Unknown
            );
        }
        let asked = fake.lookups();
        assert_eq!(asked, 50_001);
        assert_eq!(
            keys.get(&id(u64::MAX), IP).await.unwrap().key_id,
            live.key_id
        );
        assert_eq!(fake.lookups(), asked, "the live key stayed cached");
    }

    /// Collects what a tracing subscriber writes.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Captured {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
    }

    /// AC 6: the secret never shows in `Debug`, `Display` or tracing fields, and it reaches
    /// SigV4 only through `expose`.
    #[tokio::test]
    async fn secret_never_debug_printed() {
        let k = key(&id(1), Uuid::new_v4());
        let secret = k.secret.expose().to_owned();
        assert!(secret.starts_with("SeCrEt"));
        let printed = [
            format!("{k:?}"),
            format!("{k:#?}"),
            format!("{:?}", k.secret),
            format!("{}", k.secret),
            format!("{:?}", Ok::<_, KeyError>(k.clone())),
        ];
        for p in &printed {
            assert!(!p.contains(&secret) && !p.contains("SeCrEt"), "{p}");
        }
        assert_eq!(format!("{:?}", k.secret), "S3Secret(..)");
        assert_eq!(format!("{}", k.secret), "S3Secret(..)");
        assert!(
            printed[0].contains(&id(1)),
            "the rest of the key still prints"
        );

        let out = Captured::default();
        let writer = out.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        tracing::info!(key = ?k, secret = %k.secret, debug = ?k.secret, "a key");
        // The store's own logs on a lookup, and on an unreadable answer.
        let keys = frozen(FakeS3::with(&[&k]), LIVENESS_TTL);
        keys.get(&id(1), IP).await.unwrap();
        let body = format!(
            r#"{{"key_id":"{}","access_key_id":"{}","secret":"{secret}","branch_id":"{}","project_id":"{}","database":"app","role":"r","role_oid":"{secret}"}}"#,
            Uuid::new_v4(),
            id(1),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let (base, _) = fake_orchestrator(body).await;
        let keys = frozen(Arc::new(Orchestrator::new(&base, "t")), LIVENESS_TTL);
        assert_eq!(
            keys.get(&id(1), IP).await.unwrap_err(),
            KeyError::Unavailable
        );
        let logged = out.text();
        assert!(
            logged.contains("a key") && logged.contains("S3Secret(..)"),
            "{logged}"
        );
        assert!(
            !logged.contains(&secret) && !logged.contains("SeCrEt"),
            "{logged}"
        );
    }

    /// The orchestrator's S3 key route answering `body` (200) for every id; its base URL and
    /// the count of calls carrying storage's bearer.
    async fn fake_orchestrator(body: String) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let app = axum::Router::new().route(
            "/internal/storage/s3-keys/{id}",
            axum::routing::get(move |headers: HeaderMap| {
                let body = body.clone();
                let counted = counted.clone();
                async move {
                    if headers.get("authorization").and_then(|v| v.to_str().ok())
                        == Some("Bearer t")
                    {
                        counted.fetch_add(1, Ordering::SeqCst);
                    }
                    ([("content-type", "application/json")], body)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        (base, calls)
    }

    /// AC 7: an answer without the role's identity (missing or null), or for another key,
    /// is the control plane unavailable, never a key, and nothing is cached. A whole answer
    /// is a key.
    #[tokio::test]
    async fn missing_role_identity_is_unavailable() {
        use std::sync::atomic::Ordering;
        let (key_id, branch, project) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let secret = "a".repeat(40);
        let body = |access: &str, identity: &str| {
            format!(
                r#"{{"key_id":"{key_id}","access_key_id":"{access}","secret":"{secret}","branch_id":"{branch}","project_id":"{project}","database":"app","role":"r"{identity}}}"#
            )
        };
        for (what, answer) in [
            ("no role_oid", body(&id(1), r#","role_timeline":"tl""#)),
            ("no role_timeline", body(&id(1), r#","role_oid":16384"#)),
            ("neither", body(&id(1), "")),
            (
                "null role_oid",
                body(&id(1), r#","role_oid":null,"role_timeline":"tl""#),
            ),
            (
                "null role_timeline",
                body(&id(1), r#","role_oid":16384,"role_timeline":null"#),
            ),
            (
                "another key",
                body(&id(2), r#","role_oid":16384,"role_timeline":"tl""#),
            ),
            ("not json", "nope".into()),
        ] {
            let (base, calls) = fake_orchestrator(answer).await;
            let keys = frozen(Arc::new(Orchestrator::new(&base, "t")), LIVENESS_TTL);
            assert_eq!(
                keys.get(&id(1), IP).await.unwrap_err(),
                KeyError::Unavailable,
                "{what}"
            );
            assert_eq!(
                keys.get(&id(1), IP).await.unwrap_err(),
                KeyError::Unavailable,
                "{what}"
            );
            assert_eq!(calls.load(Ordering::SeqCst), 2, "{what}: nothing cached");
        }
        let (base, calls) =
            fake_orchestrator(body(&id(1), r#","role_oid":16384,"role_timeline":"tl""#)).await;
        let keys = frozen(Arc::new(Orchestrator::new(&base, "t")), LIVENESS_TTL);
        let got = keys.get(&id(1), IP).await.unwrap();
        assert_eq!(
            (got.key_id, got.branch_id, got.project_id),
            (key_id, branch, project)
        );
        assert_eq!(
            (
                got.role_oid,
                got.role_timeline.as_str(),
                got.secret.expose()
            ),
            (16384, "tl", secret.as_str())
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "asked with storage's bearer"
        );
        // 404 is unknown; any other status is unavailable.
        let app = axum::Router::new()
            .route(
                "/internal/storage/s3-keys/CBAKAAAAAAAAAAAAAAAA",
                axum::routing::get(|| async { axum::http::StatusCode::NOT_FOUND }),
            )
            .fallback(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let keys = frozen(Arc::new(Orchestrator::new(&base, "t")), LIVENESS_TTL);
        assert_eq!(
            keys.get("CBAKAAAAAAAAAAAAAAAA", IP).await.unwrap_err(),
            KeyError::Unknown
        );
        assert_eq!(
            keys.get(&id(1), IP).await.unwrap_err(),
            KeyError::Unavailable
        );
    }

    /// AC 8: a malformed id is unknown without asking the orchestrator or charging the
    /// client's bucket.
    #[tokio::test]
    async fn malformed_id_short_circuits() {
        let fake = FakeS3::with(&[]);
        let keys = frozen(fake.clone(), LIVENESS_TTL);
        let lower = id(1).to_lowercase();
        let long = format!("{}A", id(1));
        let short = &id(1)[..19];
        let other_prefix = format!("AKIA{}", &id(1)[4..]);
        let digits = "CBAK0000000000000000";
        for bad in [
            "",
            "../x",
            "CBAK/../../keys/AAAA",
            lower.as_str(),
            long.as_str(),
            short,
            other_prefix.as_str(),
            digits,
        ] {
            for _ in 0..5 {
                assert_eq!(
                    keys.get(bad, IP).await.unwrap_err(),
                    KeyError::Unknown,
                    "{bad:?}"
                );
            }
        }
        assert_eq!(fake.lookups(), 0);
        // The client's whole bucket is still there.
        for n in 0..MISSES_PER_SECOND as u64 {
            assert_eq!(
                keys.get(&id(n), IP).await.unwrap_err(),
                KeyError::Unknown,
                "{n}"
            );
        }
        assert_eq!(fake.lookups(), MISSES_PER_SECOND as usize);
        // The orchestrator's client refuses one too, should anything call it directly.
        let orchestrator = Orchestrator::new("http://127.0.0.1:9", "t");
        assert!(matches!(orchestrator.s3_key("../x").await, Ok(None)));
    }
}
