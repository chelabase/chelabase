//! Data API key resolution: key string -> branch, role and kind, via the
//! orchestrator's internal listener.
//!
//! - Results (including "unknown") are cached for 30 s, keyed by the key's
//!   SHA-256. Concurrent misses for one key share one lookup (`try_get_with`).
//! - Only lookups that reach the internal listener are charged to the per-IP
//!   limiter (20/s token bucket), so valid, cached keys are never limited.
//! - Revocation reaches a gateway instance only when its cache entry expires;
//!   instances don't share caches.

use chelabase_types::data_key::{self, DataKeyKind};
use moka::future::Cache;
use serde::Deserialize;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::warn;
use uuid::Uuid;

pub const KEY_CACHE_TTL: Duration = Duration::from_secs(30);
pub const MISSES_PER_SECOND: u32 = 20;
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// The gateway's own view of the internal listener's answer.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ResolvedKey {
    pub key_id: Uuid,
    pub branch_id: Uuid,
    /// The project the key's branch belongs to (absent in answers from an older orchestrator).
    #[serde(default)]
    pub project_id: Uuid,
    pub kind: DataKeyKind,
    pub role: String,
    /// The key's allow-list; absent or `null` means every database the branch serves.
    #[serde(default)]
    pub databases: Option<Vec<String>>,
}

/// Why a key was refused. Maps to 401 | 401 | 429 | 503.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyError {
    Missing,
    Invalid,
    RateLimited,
    Unavailable,
}

pub struct DataKeyResolver {
    client: reqwest::Client,
    base: String,
    bearer: String,
    ttl_cache: Cache<[u8; 32], Option<ResolvedKey>>,
    limiter: moka::sync::Cache<IpAddr, Arc<Mutex<(f64, Instant)>>>,
    misses_per_sec: u32,
}

impl DataKeyResolver {
    pub fn new(internal_url: &str, bearer: &str) -> Self {
        Self::with_config(internal_url, bearer, KEY_CACHE_TTL, MISSES_PER_SECOND)
    }

    /// Tests inject a short TTL (moka uses real time) and, when needed, a rate.
    pub fn with_config(
        internal_url: &str,
        bearer: &str,
        ttl: Duration,
        misses_per_sec: u32,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(LOOKUP_TIMEOUT)
            .build()
            .expect("build the key lookup HTTP client");
        Self {
            client,
            base: internal_url.trim_end_matches('/').to_string(),
            bearer: bearer.to_string(),
            ttl_cache: Cache::builder()
                .max_capacity(100_000)
                .time_to_live(ttl)
                .build(),
            // Idle buckets are forgotten: a full bucket is the default anyway.
            limiter: moka::sync::Cache::builder()
                .max_capacity(100_000)
                .time_to_idle(Duration::from_secs(60))
                .build(),
            misses_per_sec: misses_per_sec.max(1),
        }
    }

    pub fn with_ttl(internal_url: &str, bearer: &str, ttl: Duration) -> Self {
        Self::with_config(internal_url, bearer, ttl, MISSES_PER_SECOND)
    }

    pub async fn resolve(
        &self,
        key: &str,
        client_ip: Option<IpAddr>,
    ) -> Result<ResolvedKey, KeyError> {
        if data_key::parse_kind(key).is_none() {
            return Err(KeyError::Invalid);
        }
        let hash = data_key::hash(key);
        match self
            .ttl_cache
            .try_get_with(hash, self.lookup(hash, client_ip))
            .await
        {
            Ok(Some(resolved)) => Ok(resolved),
            Ok(None) => Err(KeyError::Invalid),
            Err(err) => Err(*err),
        }
    }

    /// One trip to the internal listener; runs only on a cache miss, and only
    /// once for concurrent misses of the same key.
    async fn lookup(
        &self,
        hash: [u8; 32],
        client_ip: Option<IpAddr>,
    ) -> Result<Option<ResolvedKey>, KeyError> {
        if !self.allow_miss(client_ip) {
            return Err(KeyError::RateLimited);
        }
        let url = format!("{}/internal/data/keys/{}", self.base, hex::encode(hash));
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.bearer)
            .send()
            .await
            .map_err(|err| {
                warn!("data key lookup failed: {err}");
                KeyError::Unavailable
            })?;
        match response.status().as_u16() {
            200 => response
                .json::<ResolvedKey>()
                .await
                .map(Some)
                .map_err(|err| {
                    warn!("data key lookup returned an unreadable body: {err}");
                    KeyError::Unavailable
                }),
            404 => Ok(None),
            status => {
                warn!(status, "data key lookup refused or failed");
                Err(KeyError::Unavailable)
            }
        }
    }

    /// Token bucket per client address (IPv6 by /64, unknown addresses share one).
    fn allow_miss(&self, client_ip: Option<IpAddr>) -> bool {
        let rate = f64::from(self.misses_per_sec);
        let now = Instant::now();
        let bucket = self
            .limiter
            .get_with(limiter_key(client_ip), || Arc::new(Mutex::new((rate, now))));
        let mut state = bucket
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (tokens, last) = *state;
        let tokens = (tokens + now.duration_since(last).as_secs_f64() * rate).min(rate);
        if tokens >= 1.0 {
            *state = (tokens - 1.0, now);
            true
        } else {
            *state = (tokens, now);
            false
        }
    }
}

fn limiter_key(ip: Option<IpAddr>) -> IpAddr {
    match ip {
        None => IpAddr::from([0u8; 4]),
        Some(IpAddr::V4(v4)) => IpAddr::V4(v4),
        Some(IpAddr::V6(v6)) => {
            // Dual-stack listeners show IPv4 clients as ::ffff:a.b.c.d.
            if let Some(v4) = v6.to_ipv4_mapped() {
                return IpAddr::V4(v4);
            }
            let mut octets = v6.octets();
            octets[8..].fill(0);
            IpAddr::from(octets)
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::get;
    use axum::{Json, Router};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub const BEARER: &str = "test-data-keys-token";

    #[derive(Clone)]
    pub enum Answer {
        Known(ResolvedKey),
        NotFound,
        ServerError,
    }

    #[derive(Clone, Default)]
    pub struct Mock {
        pub lookups: Arc<AtomicUsize>,
        pub answers: Arc<Mutex<HashMap<String, Answer>>>,
        pub delay: Arc<Mutex<Duration>>,
        pub bad_bearer: Arc<AtomicUsize>,
    }

    impl Mock {
        pub fn set(&self, key: &str, answer: Answer) {
            self.answers
                .lock()
                .unwrap()
                .insert(hex::encode(data_key::hash(key)), answer);
        }
        pub fn lookups(&self) -> usize {
            self.lookups.load(Ordering::SeqCst)
        }
    }

    pub fn resolved(kind: DataKeyKind) -> ResolvedKey {
        ResolvedKey {
            key_id: Uuid::new_v4(),
            branch_id: Uuid::new_v4(),
            project_id: Uuid::new_v4(),
            kind,
            role: if kind == DataKeyKind::Public {
                "anon"
            } else {
                "service"
            }
            .to_string(),
            databases: None,
        }
    }

    pub fn new_key(kind: DataKeyKind) -> String {
        data_key::generate(kind).key
    }

    /// An axum server standing in for the orchestrator's internal listener.
    pub async fn start() -> (String, Mock) {
        let mock = Mock::default();
        let app = Router::new()
            .route(
                "/internal/data/keys/{hash}",
                get(
                    |State(mock): State<Mock>, Path(hash): Path<String>, headers: HeaderMap| async move {
                        mock.lookups.fetch_add(1, Ordering::SeqCst);
                        let delay = *mock.delay.lock().unwrap();
                        if !delay.is_zero() {
                            tokio::time::sleep(delay).await;
                        }
                        let expected = format!("Bearer {BEARER}");
                        if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some(&expected) {
                            mock.bad_bearer.fetch_add(1, Ordering::SeqCst);
                            return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({}))).into_response_pair();
                        }
                        let answer = mock.answers.lock().unwrap().get(&hash).cloned();
                        match answer {
                            Some(Answer::Known(k)) => (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "key_id": k.key_id, "branch_id": k.branch_id, "project_id": k.project_id,
                                    "kind": k.kind.as_str(), "role": k.role,
                                    "databases": k.databases,
                                })),
                            )
                                .into_response_pair(),
                            Some(Answer::ServerError) => {
                                (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({}))).into_response_pair()
                            }
                            _ => (StatusCode::NOT_FOUND, Json(serde_json::json!({}))).into_response_pair(),
                        }
                    },
                ),
            )
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), mock)
    }

    trait IntoPair {
        fn into_response_pair(self) -> (StatusCode, Json<serde_json::Value>);
    }
    impl IntoPair for (StatusCode, Json<serde_json::Value>) {
        fn into_response_pair(self) -> (StatusCode, Json<serde_json::Value>) {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use std::net::Ipv4Addr;

    const IP: Option<IpAddr> = Some(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)));

    /// A lookup answer without `databases` (an orchestrator from before allow-lists) or
    /// with `null` is a key without a list.
    #[test]
    fn a_lookup_without_databases_has_no_list() {
        let base = serde_json::json!({
            "key_id": Uuid::nil(), "branch_id": Uuid::nil(), "kind": "secret", "role": "svc",
        });
        let k: ResolvedKey = serde_json::from_value(base.clone()).unwrap();
        assert_eq!(k.databases, None);
        let mut listed = base;
        listed["databases"] = serde_json::json!(["main"]);
        let k: ResolvedKey = serde_json::from_value(listed).unwrap();
        assert_eq!(k.databases, Some(vec!["main".to_string()]));
    }

    #[tokio::test]
    async fn malformed_key_is_401_without_lookup() {
        let (url, mock) = start().await;
        let r = DataKeyResolver::new(&url, BEARER);
        for bad in [
            "",
            "nope",
            "chk_pub_short",
            "chk_xxx_0000000000000000000000000000000000000000000",
        ] {
            assert_eq!(r.resolve(bad, IP).await, Err(KeyError::Invalid), "{bad}");
        }
        assert_eq!(mock.lookups(), 0);
    }

    #[tokio::test]
    async fn malformed_keys_are_not_charged_to_the_limiter() {
        let (url, mock) = start().await;
        let r = DataKeyResolver::with_config(&url, BEARER, KEY_CACHE_TTL, 2);
        for _ in 0..10 {
            assert_eq!(r.resolve("garbage", IP).await, Err(KeyError::Invalid));
        }
        let key = new_key(DataKeyKind::Public);
        mock.set(&key, Answer::Known(resolved(DataKeyKind::Public)));
        assert!(r.resolve(&key, IP).await.is_ok());
    }

    #[tokio::test]
    async fn known_key_resolves_with_the_bearer() {
        let (url, mock) = start().await;
        let key = new_key(DataKeyKind::Secret);
        let expected = resolved(DataKeyKind::Secret);
        mock.set(&key, Answer::Known(expected.clone()));
        let r = DataKeyResolver::new(&url, BEARER);
        assert_eq!(r.resolve(&key, IP).await, Ok(expected.clone()));
        assert_eq!(r.resolve(&key, IP).await, Ok(expected));
        assert_eq!(mock.lookups(), 1, "second call is a cache hit");
        assert_eq!(mock.bad_bearer.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn unknown_key_is_cached_for_30s() {
        let (url, mock) = start().await;
        let key = new_key(DataKeyKind::Public);
        let r = DataKeyResolver::new(&url, BEARER);
        for _ in 0..25 {
            assert_eq!(r.resolve(&key, IP).await, Err(KeyError::Invalid));
        }
        assert_eq!(mock.lookups(), 1);
    }

    #[tokio::test]
    async fn revoked_key_is_refused_after_ttl() {
        let (url, mock) = start().await;
        let key = new_key(DataKeyKind::Public);
        mock.set(&key, Answer::Known(resolved(DataKeyKind::Public)));
        let r = DataKeyResolver::with_ttl(&url, BEARER, Duration::from_millis(150));
        assert!(r.resolve(&key, IP).await.is_ok());
        mock.set(&key, Answer::NotFound); // revoked
        assert!(
            r.resolve(&key, IP).await.is_ok(),
            "still cached inside the TTL"
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(r.resolve(&key, IP).await, Err(KeyError::Invalid));
        assert_eq!(mock.lookups(), 2);
    }

    #[tokio::test]
    async fn miss_flood_from_one_ip_gets_429_and_bounded_lookups() {
        let (url, mock) = start().await;
        let r = Arc::new(DataKeyResolver::new(&url, BEARER));
        let tasks: Vec<_> = (0..80)
            .map(|_| {
                let r = r.clone();
                tokio::spawn(async move { r.resolve(&new_key(DataKeyKind::Public), IP).await })
            })
            .collect();
        let mut limited = 0;
        for t in tasks {
            match t.await.unwrap() {
                Err(KeyError::RateLimited) => limited += 1,
                Err(KeyError::Invalid) => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(mock.lookups() <= 25, "lookups {}", mock.lookups());
        assert!(limited >= 50, "limited {limited}");
        // Another address is unaffected.
        let other = Some(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 1)));
        assert_eq!(
            r.resolve(&new_key(DataKeyKind::Public), other).await,
            Err(KeyError::Invalid)
        );
    }

    #[tokio::test]
    async fn cached_keys_are_not_limited() {
        let (url, mock) = start().await;
        let key = new_key(DataKeyKind::Public);
        mock.set(&key, Answer::Known(resolved(DataKeyKind::Public)));
        let r = DataKeyResolver::new(&url, BEARER);
        for _ in 0..200 {
            assert!(r.resolve(&key, IP).await.is_ok());
        }
        assert_eq!(mock.lookups(), 1);
    }

    #[tokio::test]
    async fn concurrent_misses_coalesce() {
        let (url, mock) = start().await;
        *mock.delay.lock().unwrap() = Duration::from_millis(200);
        let key = new_key(DataKeyKind::Public);
        mock.set(&key, Answer::Known(resolved(DataKeyKind::Public)));
        let r = Arc::new(DataKeyResolver::new(&url, BEARER));
        let tasks: Vec<_> = (0..10)
            .map(|_| {
                let (r, key) = (r.clone(), key.clone());
                tokio::spawn(async move { r.resolve(&key, IP).await })
            })
            .collect();
        for t in tasks {
            assert!(t.await.unwrap().is_ok());
        }
        assert_eq!(mock.lookups(), 1);
    }

    #[tokio::test]
    async fn lookup_5xx_is_503_and_not_cached() {
        let (url, mock) = start().await;
        let key = new_key(DataKeyKind::Public);
        mock.set(&key, Answer::ServerError);
        let r = DataKeyResolver::new(&url, BEARER);
        assert_eq!(r.resolve(&key, IP).await, Err(KeyError::Unavailable));
        assert_eq!(r.resolve(&key, IP).await, Err(KeyError::Unavailable));
        assert_eq!(mock.lookups(), 2);
        mock.set(&key, Answer::Known(resolved(DataKeyKind::Public)));
        assert!(
            r.resolve(&key, IP).await.is_ok(),
            "recovers once the listener does"
        );
    }

    #[tokio::test]
    async fn unreachable_listener_is_unavailable() {
        let r = DataKeyResolver::new("http://127.0.0.1:1", BEARER);
        assert_eq!(
            r.resolve(&new_key(DataKeyKind::Public), IP).await,
            Err(KeyError::Unavailable)
        );
    }

    #[test]
    fn limiter_key_normalises_mapped_v4_groups_v6_by_64_and_handles_none() {
        let a: std::net::Ipv4Addr = "203.0.113.7".parse().unwrap();
        let b: std::net::Ipv4Addr = "198.51.100.1".parse().unwrap();
        let (ma, mb) = (a.to_ipv6_mapped(), b.to_ipv6_mapped());
        assert_ne!(limiter_key(Some(IpAddr::V6(ma))), limiter_key(Some(IpAddr::V6(mb))));
        assert_eq!(limiter_key(Some(IpAddr::V6(ma))), limiter_key(Some(IpAddr::V4(a))));
        assert_eq!(limiter_key(Some(IpAddr::V6(mb))), IpAddr::V4(b));

        let x: IpAddr = "2001:db8:1:2:aaaa:bbbb:cccc:dddd".parse().unwrap();
        let same64: IpAddr = "2001:db8:1:2::1".parse().unwrap();
        let other64: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(limiter_key(Some(x)), limiter_key(Some(same64)));
        assert_ne!(limiter_key(Some(x)), limiter_key(Some(other64)));

        assert_eq!(limiter_key(None), IpAddr::from([0u8; 4]));
    }
}
