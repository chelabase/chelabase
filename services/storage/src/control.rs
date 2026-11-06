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
    fn live_key(&self, key_id: Uuid) -> BoxFuture<'_, Result<Option<LiveKey>, Unreachable>>;
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
    live: moka::future::Cache<Uuid, Option<LiveKey>>,
}

impl Urls {
    pub fn new(control: Arc<dyn Control>) -> Self {
        Urls {
            control,
            key: tokio::sync::OnceCell::new(),
            live: moka::future::Cache::builder()
                .max_capacity(100_000)
                .time_to_live(LIVENESS_TTL)
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

    /// The key while it is live (cached, a miss asks the control plane).
    pub async fn live_key(&self, key_id: Uuid, rid: &str) -> Result<Option<LiveKey>, Failure> {
        if let Some(hit) = self.live.get(&key_id).await {
            return Ok(hit);
        }
        let live = self
            .control
            .live_key(key_id)
            .await
            .map_err(|e| unreachable(e, rid))?;
        self.live.insert(key_id, live.clone()).await;
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

    fn live_key(&self, key_id: Uuid) -> BoxFuture<'_, Result<Option<LiveKey>, Unreachable>> {
        Box::pin(async move {
            let path = format!("/internal/storage/keys/{key_id}");
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
