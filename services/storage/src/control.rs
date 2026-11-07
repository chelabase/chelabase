//! What signed URLs need from the control plane, through the orchestrator's internal routes
//! behind storage's bearer: the URL key, whether the key that minted a URL is still live, and
//! the one-time upload tokens (consumed there, so on every replica and across restarts).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chelabase_auth::internal::DataIdentity;
use chelabase_types::data_key::DataKeyKind;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::cache::Fenced;
use crate::error::Failure;
use crate::token::UrlKey;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// How long a key's liveness is trusted. A revoked key's URLs fail within it.
pub const LIVENESS_TTL: Duration = Duration::from_secs(5);

/// A live data key, as the orchestrator answers it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LiveKey {
    pub key_id: Uuid,
    pub branch_id: Uuid,
    pub kind: DataKeyKind,
    pub role: String,
    pub databases: Option<Vec<String>>,
    pub role_oid: Option<i64>,
}

impl LiveKey {
    /// The identity an upload token's request runs as: the key as it is now, with the
    /// minting request's claims.
    pub fn identity(self, claims: Value) -> DataIdentity {
        DataIdentity {
            branch_id: self.branch_id,
            key_id: self.key_id,
            kind: self.kind,
            role: self.role,
            claims,
            databases: self.databases,
            project_id: None,
            role_oid: self.role_oid,
            server_vouched: false,
        }
    }
}

/// Who minted a signed URL, as its (signed) claims say: what its liveness is judged on (#100).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Minter {
    /// No role in the claims: a data key's URL. A console URL without one is never live.
    Key,
    /// A console URL minted in owner mode: live while its member is still an Admin.
    Owner,
    /// A console URL minted as this role: live while its member still holds that role at
    /// Developer.
    Role(String),
}

impl Minter {
    /// The minter a download token's claims name: the owner-mode marker, a role, or neither.
    pub fn of(role: Option<&str>) -> Self {
        match role {
            None => Minter::Key,
            Some(crate::internal::STORAGE_ROLE) => Minter::Owner,
            Some(role) => Minter::Role(role.to_owned()),
        }
    }

    /// The liveness route's query beyond `?branch=`.
    fn query(&self) -> String {
        match self {
            Minter::Key => String::new(),
            Minter::Owner => "&owner=true".into(),
            Minter::Role(role) => format!("&role={}", crate::names::encode_path(role)),
        }
    }
}

/// The control plane couldn't answer (logged by the caller): 503.
#[derive(Debug)]
pub struct Unreachable(pub String);

pub trait Control: Send + Sync + 'static {
    fn url_key(&self) -> BoxFuture<'_, Result<UrlKey, Unreachable>>;
    /// The key while it is live, for a URL signed on `branch`: a data key, or a member who
    /// minted the URL from the console while they still hold its role at Developer on the
    /// branch's database (an Admin for a URL minted in owner mode), as `minter` says.
    fn live_key(
        &self,
        key_id: Uuid,
        branch: Uuid,
        minter: &Minter,
    ) -> BoxFuture<'_, Result<Option<LiveKey>, Unreachable>>;
    /// The token's expiry as recorded (an app user's is cut to its session's), or `None` when
    /// the id is taken, the key isn't live, or the app session the claims name isn't.
    fn record_upload<'a>(
        &'a self,
        token_id: Uuid,
        key_id: Uuid,
        claims: &'a Value,
        expires_at: u64,
    ) -> BoxFuture<'a, Result<Option<u64>, Unreachable>>;
    /// The key as it is now and the minting claims, once; `None` after (or a dead key, or an
    /// app session no longer live).
    fn consume_upload(
        &self,
        token_id: Uuid,
    ) -> BoxFuture<'_, Result<Option<(LiveKey, Value)>, Unreachable>>;
}

/// The control plane, with the URL key fetched once and each key's liveness cached
/// [`LIVENESS_TTL`].
pub struct Urls {
    control: Arc<dyn Control>,
    key: tokio::sync::OnceCell<UrlKey>,
    live: Fenced<(Uuid, Uuid, Minter), Option<LiveKey>>,
}

impl Urls {
    pub fn new(control: Arc<dyn Control>) -> Self {
        Urls {
            control,
            key: tokio::sync::OnceCell::new(),
            live: Fenced::new(100_000, LIVENESS_TTL),
        }
    }

    pub fn control(&self) -> &dyn Control {
        self.control.as_ref()
    }

    pub async fn key(&self, rid: &str) -> Result<&UrlKey, Failure> {
        self.key
            .get_or_try_init(|| self.control.url_key())
            .await
            .map_err(|e| unreachable(e, rid))
    }

    /// Forgets `key_id`'s cached liveness on every branch (m9: it was revoked, rotated or its
    /// databases changed): its URLs are checked again on their next use.
    pub fn forget_key(&self, key_id: Uuid) {
        self.live.forget_if(move |k, _| k.0 == key_id);
    }

    /// Forgets every key's cached liveness (m9: a member's access changed).
    pub fn forget_all(&self) {
        self.live.forget_all();
    }

    /// The key while it is live for a URL signed on `branch` (cached, a miss asks the control
    /// plane; a forget during the lookup leaves its answer uncached: [`Fenced`]).
    pub async fn live_key(
        &self,
        key_id: Uuid,
        branch: Uuid,
        minter: &Minter,
        rid: &str,
    ) -> Result<Option<LiveKey>, Failure> {
        self.live
            .get_or_lookup((key_id, branch, minter.clone()), async {
                self.control
                    .live_key(key_id, branch, minter)
                    .await
                    .map_err(|e| unreachable(e, rid))
            })
            .await
    }
}

pub fn unreachable(e: Unreachable, rid: &str) -> Failure {
    tracing::warn!(
        request_id = rid,
        "the orchestrator's storage routes: {}",
        e.0
    );
    Failure::unavailable("storage can't reach the control plane right now; try again")
}

/// The orchestrator's internal listener.
pub struct Orchestrator {
    http: chelabase_http::Client,
    base: String,
    token: String,
}

impl Orchestrator {
    pub fn new(base_url: &str, token: &str) -> Self {
        Orchestrator {
            // Never a redirect (#26): the bearer goes to the orchestrator only.
            http: chelabase_http::client_builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("an HTTP client"),
            base: base_url.trim_end_matches('/').to_owned(),
            token: token.to_owned(),
        }
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<(u16, Value), Unreachable> {
        let url = format!("{}{path}", self.base);
        let req = match method {
            "GET" => self.http.get(url),
            _ => self.http.post(url),
        };
        let req = match body {
            Some(b) => req.json(&b),
            None => req,
        };
        let res = req
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| Unreachable(e.to_string()))?;
        let status = res.status().as_u16();
        let json = res.json().await.unwrap_or(Value::Null);
        Ok((status, json))
    }
}

fn parse<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, Unreachable> {
    serde_json::from_value(v).map_err(|e| Unreachable(format!("reading the answer: {e}")))
}

fn status(path: &str, code: u16) -> Unreachable {
    Unreachable(format!("{path}: HTTP {code}"))
}

impl Control for Orchestrator {
    fn url_key(&self) -> BoxFuture<'_, Result<UrlKey, Unreachable>> {
        Box::pin(async move {
            #[derive(Deserialize)]
            struct Key {
                kid: u32,
                key: String,
            }
            let path = "/internal/storage/url-key";
            match self.call("GET", path, None).await? {
                (200, v) => {
                    let k: Key = parse(v)?;
                    let key = STANDARD
                        .decode(k.key)
                        .ok()
                        .and_then(|b| <[u8; 32]>::try_from(b).ok())
                        .ok_or_else(|| Unreachable("the URL key isn't 32 bytes".into()))?;
                    Ok(UrlKey { kid: k.kid, key })
                }
                (code, _) => Err(status(path, code)),
            }
        })
    }

    fn live_key(
        &self,
        key_id: Uuid,
        branch: Uuid,
        minter: &Minter,
    ) -> BoxFuture<'_, Result<Option<LiveKey>, Unreachable>> {
        let path = format!("/internal/storage/keys/{key_id}?branch={branch}{}", minter.query());
        Box::pin(async move {
            match self.call("GET", &path, None).await? {
                (200, v) => parse(v).map(Some),
                (404, _) => Ok(None),
                (code, _) => Err(status(&path, code)),
            }
        })
    }

    fn record_upload<'a>(
        &'a self,
        token_id: Uuid,
        key_id: Uuid,
        claims: &'a Value,
        expires_at: u64,
    ) -> BoxFuture<'a, Result<Option<u64>, Unreachable>> {
        Box::pin(async move {
            #[derive(Deserialize)]
            struct Recorded {
                expires_at: u64,
            }
            let expires = chrono::DateTime::<chrono::Utc>::from_timestamp(expires_at as i64, 0)
                .ok_or_else(|| Unreachable("an expiry out of range".into()))?;
            let path = "/internal/storage/upload-tokens";
            let body = json!({ "token_id": token_id, "key_id": key_id, "claims": claims, "expires_at": expires });
            match self.call("POST", path, Some(body)).await? {
                // Never later than asked, whatever the answer says.
                (200, v) => parse::<Recorded>(v).map(|r| Some(r.expires_at.min(expires_at))),
                (409, _) => Ok(None),
                (code, _) => Err(status(path, code)),
            }
        })
    }

    fn consume_upload(
        &self,
        token_id: Uuid,
    ) -> BoxFuture<'_, Result<Option<(LiveKey, Value)>, Unreachable>> {
        Box::pin(async move {
            #[derive(Deserialize)]
            struct Spent {
                key: LiveKey,
                claims: Value,
            }
            let path = format!("/internal/storage/upload-tokens/{token_id}/consume");
            match self.call("POST", &path, None).await? {
                (200, v) => parse::<Spent>(v).map(|s| Some((s.key, s.claims))),
                (404, _) => Ok(None),
                (code, _) => Err(status(&path, code)),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts the liveness lookups; every key is live on its branch.
    #[derive(Default)]
    struct Counting(AtomicUsize);

    impl Control for Counting {
        fn url_key(&self) -> BoxFuture<'_, Result<UrlKey, Unreachable>> {
            Box::pin(async { Err(Unreachable("unused".into())) })
        }
        fn live_key(&self, key_id: Uuid, branch: Uuid, _: &Minter) -> BoxFuture<'_, Result<Option<LiveKey>, Unreachable>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                Ok(Some(LiveKey {
                    key_id,
                    branch_id: branch,
                    kind: DataKeyKind::Secret,
                    role: "authenticated".into(),
                    databases: None,
                    role_oid: None,
                }))
            })
        }
        fn record_upload<'a>(&'a self, _: Uuid, _: Uuid, _: &'a Value, _: u64) -> BoxFuture<'a, Result<Option<u64>, Unreachable>> {
            Box::pin(async { Ok(None) })
        }
        fn consume_upload(&self, _: Uuid) -> BoxFuture<'_, Result<Option<(LiveKey, Value)>, Unreachable>> {
            Box::pin(async { Ok(None) })
        }
    }

    /// NATS (m9): a key event drops that key's cached liveness on every branch (the next URL
    /// asks again), an access event drops every key's; others stay cached.
    #[tokio::test]
    async fn events_drop_cached_liveness() {
        let control = Arc::new(Counting::default());
        let urls = Urls::new(control.clone());
        let (k1, k2, b1, b2) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let lookups = || control.0.load(Ordering::SeqCst);
        for (k, b) in [(k1, b1), (k1, b2), (k2, b1)] {
            urls.live_key(k, b, &Minter::Key, "r").await.unwrap();
        }
        assert_eq!(lookups(), 3);
        urls.live_key(k1, b1, &Minter::Key, "r").await.unwrap();
        assert_eq!(lookups(), 3, "cached");
        urls.forget_key(k1);
        for (k, b) in [(k1, b1), (k1, b2), (k2, b1)] {
            urls.live_key(k, b, &Minter::Key, "r").await.unwrap();
        }
        assert_eq!(lookups(), 5, "k1 asked again on both branches, k2 still cached");
        urls.forget_all();
        urls.live_key(k2, b1, &Minter::Key, "r").await.unwrap();
        assert_eq!(lookups(), 6);
    }

    /// Lets the test fire a forget while a liveness lookup is in flight: the first lookup says
    /// it started, then waits to be let go.
    #[derive(Default)]
    struct Gated {
        lookups: AtomicUsize,
        started: tokio::sync::Notify,
        go_on: tokio::sync::Notify,
    }

    impl Control for Gated {
        fn url_key(&self) -> BoxFuture<'_, Result<UrlKey, Unreachable>> {
            Box::pin(async { Err(Unreachable("unused".into())) })
        }
        fn live_key(&self, key_id: Uuid, branch: Uuid, _: &Minter) -> BoxFuture<'_, Result<Option<LiveKey>, Unreachable>> {
            let first = self.lookups.fetch_add(1, Ordering::SeqCst) == 0;
            Box::pin(async move {
                if first {
                    self.started.notify_one();
                    self.go_on.notified().await;
                }
                Ok(Some(LiveKey {
                    key_id,
                    branch_id: branch,
                    kind: DataKeyKind::Secret,
                    role: "authenticated".into(),
                    databases: None,
                    role_oid: None,
                }))
            })
        }
        fn record_upload<'a>(&'a self, _: Uuid, _: Uuid, _: &'a Value, _: u64) -> BoxFuture<'a, Result<Option<u64>, Unreachable>> {
            Box::pin(async { Ok(None) })
        }
        fn consume_upload(&self, _: Uuid) -> BoxFuture<'_, Result<Option<(LiveKey, Value)>, Unreachable>> {
            Box::pin(async { Ok(None) })
        }
    }

    /// S1 (m9's audit): a forget that lands while a lookup is in flight wins: the answer from
    /// before it isn't cached, so the next use asks the control plane again.
    #[tokio::test]
    async fn a_forget_during_a_lookup_is_not_lost() {
        for forget_all in [false, true] {
            let control = Arc::new(Gated::default());
            let urls = Arc::new(Urls::new(control.clone()));
            let (k, b) = (Uuid::new_v4(), Uuid::new_v4());
            let in_flight = tokio::spawn({
                let urls = urls.clone();
                async move { urls.live_key(k, b, &Minter::Key, "r").await.unwrap() }
            });
            control.started.notified().await;
            if forget_all {
                urls.forget_all();
            } else {
                urls.forget_key(k);
            }
            control.go_on.notify_one();
            in_flight.await.unwrap();
            urls.live_key(k, b, &Minter::Key, "r").await.unwrap();
            assert_eq!(control.lookups.load(Ordering::SeqCst), 2, "forget_all: {forget_all}");
            urls.live_key(k, b, &Minter::Key, "r").await.unwrap();
            assert_eq!(control.lookups.load(Ordering::SeqCst), 2, "cached again after");
        }
    }

    /// #100: a URL's claims name its minter, and the liveness route is asked about exactly
    /// that one: owner mode, the role (encoded, so it can't add a parameter), or neither.
    #[test]
    fn the_liveness_route_is_asked_about_the_urls_minter() {
        assert_eq!(Minter::of(None), Minter::Key);
        assert_eq!(Minter::of(Some(crate::internal::STORAGE_ROLE)), Minter::Owner);
        assert_eq!(Minter::of(Some("reader")), Minter::Role("reader".into()));
        assert_eq!(Minter::Key.query(), "");
        assert_eq!(Minter::Owner.query(), "&owner=true");
        assert_eq!(Minter::Role("a b&owner=true".into()).query(), "&role=a%20b%26owner%3Dtrue");
    }
}
