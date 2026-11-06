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

/// The control plane couldn't answer (logged by the caller): 503.
#[derive(Debug)]
pub struct Unreachable(pub String);

pub trait Control: Send + Sync + 'static {
    fn url_key(&self) -> BoxFuture<'_, Result<UrlKey, Unreachable>>;
    /// The key while it is live, for a URL signed on `branch`: a data key, or a member who
    /// minted the URL from the console while they still have Developer access to the branch's
    /// database (an Admin for a URL minted in owner mode, `owner`).
    fn live_key(
        &self,
        key_id: Uuid,
        branch: Uuid,
        owner: bool,
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
    live: moka::future::Cache<(Uuid, Uuid, bool), Option<LiveKey>>,
}

impl Urls {
    pub fn new(control: Arc<dyn Control>) -> Self {
        Urls {
            control,
            key: tokio::sync::OnceCell::new(),
            live: moka::future::Cache::builder()
                .max_capacity(100_000)
                .time_to_live(LIVENESS_TTL)
                .support_invalidation_closures()
                .build(),
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
        if let Err(e) = self.live.invalidate_entries_if(move |k, _| k.0 == key_id) {
            tracing::warn!(%key_id, "dropping a key's liveness: {e}");
            self.live.invalidate_all();
        }
    }

    /// Forgets every key's cached liveness (m9: a member's access changed).
    pub fn forget_all(&self) {
        self.live.invalidate_all();
    }

    /// The key while it is live for a URL signed on `branch` (cached, a miss asks the control
    /// plane).
    pub async fn live_key(
        &self,
        key_id: Uuid,
        branch: Uuid,
        owner: bool,
        rid: &str,
    ) -> Result<Option<LiveKey>, Failure> {
        if let Some(hit) = self.live.get(&(key_id, branch, owner)).await {
            return Ok(hit);
        }
        let live = self
            .control
            .live_key(key_id, branch, owner)
            .await
            .map_err(|e| unreachable(e, rid))?;
        self.live.insert((key_id, branch, owner), live.clone()).await;
        Ok(live)
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
        owner: bool,
    ) -> BoxFuture<'_, Result<Option<LiveKey>, Unreachable>> {
        Box::pin(async move {
            let path = format!("/internal/storage/keys/{key_id}?branch={branch}&owner={owner}");
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
        fn live_key(&self, key_id: Uuid, branch: Uuid, _owner: bool) -> BoxFuture<'_, Result<Option<LiveKey>, Unreachable>> {
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
            urls.live_key(k, b, false, "r").await.unwrap();
        }
        assert_eq!(lookups(), 3);
        urls.live_key(k1, b1, false, "r").await.unwrap();
        assert_eq!(lookups(), 3, "cached");
        urls.forget_key(k1);
        for (k, b) in [(k1, b1), (k1, b2), (k2, b1)] {
            urls.live_key(k, b, false, "r").await.unwrap();
        }
        assert_eq!(lookups(), 5, "k1 asked again on both branches, k2 still cached");
        urls.forget_all();
        urls.live_key(k2, b1, false, "r").await.unwrap();
        assert_eq!(lookups(), 6);
    }
}
