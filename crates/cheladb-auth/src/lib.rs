//! Authentication for Cheladb.
//!
//! - [`JwtVerifier`]: Zitadel JWTs verified locally via JWKS (no introspection).
//!   Soft-bind uses claim `urn:zitadel:iam:org:id` → metadata `tenants.zitadel_org_id`.
//! - [`internal`]: gateway-minted internal identity tokens for service-to-service trust.

use jsonwebtoken::{
    decode, decode_header, Algorithm, DecodingKey, Validation,
};
use parking_lot::RwLock;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tracing::{debug, warn};

pub mod internal;

/// Zitadel org id claim (when asserted via org-scoped flows).
pub const CLAIM_ORG_ID: &str = "urn:zitadel:iam:org:id";
/// User's owning organization (request scope `urn:zitadel:iam:user:resourceowner`).
pub const CLAIM_RESOURCE_OWNER_ID: &str = "urn:zitadel:iam:user:resourceowner:id";

/// Issuer / JWKS settings for local JWT verification.
#[derive(Debug, Clone)]
pub struct ZitadelAuthConfig {
    /// OIDC issuer URL, e.g. `http://localhost:8080`.
    pub issuer: String,
    /// JWKS URL (usually `{issuer}/oauth/v2/keys`).
    pub jwks_url: String,
    /// Expected audience values (API app client_id). Empty = skip aud when token has none.
    pub audiences: Vec<String>,
}

impl ZitadelAuthConfig {
    pub fn from_env() -> Result<Self, AuthError> {
        let issuer = std::env::var("CHELADB_ZITADEL_ISSUER")
            .unwrap_or_else(|_| "http://localhost:8080".to_string());
        let jwks_url = std::env::var("CHELADB_ZITADEL_JWKS_URL")
            .unwrap_or_else(|_| format!("{issuer}/oauth/v2/keys"));
        let audiences = std::env::var("CHELADB_ZITADEL_AUDIENCES")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();

        Ok(Self {
            issuer,
            jwks_url,
            audiences,
        })
    }
}

/// Claims extracted after successful verification.
#[derive(Debug, Clone)]
pub struct VerifiedClaims {
    pub sub: String,
    pub org_id: String,
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("auth config error: {0}")]
    Config(String),
    #[error("missing or invalid authorization bearer token")]
    Unauthorized,
    #[error("token verification failed: {0}")]
    InvalidToken(String),
    #[error("jwks fetch failed: {0}")]
    Jwks(String),
}

/// Alias kept for existing call sites that only load config.
pub type AuthConfigError = AuthError;

struct JwksCache {
    keys: HashMap<String, DecodingKey>,
    fetched_at: Instant,
}

/// In-process JWKS JWT verifier with TTL cache.
#[derive(Clone)]
pub struct JwtVerifier {
    config: ZitadelAuthConfig,
    http: reqwest::Client,
    cache: Arc<RwLock<Option<JwksCache>>>,
    cache_ttl: Duration,
}

impl JwtVerifier {
    pub fn new(config: ZitadelAuthConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
            cache: Arc::new(RwLock::new(None)),
            cache_ttl: Duration::from_secs(600),
        }
    }

    pub fn from_env() -> Result<Self, AuthError> {
        Ok(Self::new(ZitadelAuthConfig::from_env()?))
    }

    pub fn config(&self) -> &ZitadelAuthConfig {
        &self.config
    }

    /// Verify a raw Bearer token (without the `Bearer ` prefix).
    pub async fn verify(&self, token: &str) -> Result<VerifiedClaims, AuthError> {
        let token = token.trim();
        if token.is_empty() {
            return Err(AuthError::Unauthorized);
        }

        let header = decode_header(token)
            .map_err(|e| AuthError::InvalidToken(format!("header: {e}")))?;
        let kid = header
            .kid
            .ok_or_else(|| AuthError::InvalidToken("missing kid".into()))?;

        let key = self.decoding_key_for_kid(&kid).await?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(std::slice::from_ref(&self.config.issuer));
        validation.validate_aud = false;
        validation.leeway = 60;

        let data = decode::<Value>(token, &key, &validation)
            .map_err(|e| AuthError::InvalidToken(e.to_string()))?;

        build_claims(&data.claims, &self.config)
    }

    async fn decoding_key_for_kid(&self, kid: &str) -> Result<DecodingKey, AuthError> {
        if let Some(key) = self.cached_key(kid) {
            return Ok(key);
        }
        self.refresh_jwks().await?;
        self.cached_key(kid).ok_or_else(|| {
            AuthError::InvalidToken(format!("no jwk for kid {kid}"))
        })
    }

    fn cached_key(&self, kid: &str) -> Option<DecodingKey> {
        let guard = self.cache.read();
        let cache = guard.as_ref()?;
        if cache.fetched_at.elapsed() > self.cache_ttl {
            return None;
        }
        cache.keys.get(kid).cloned()
    }

    async fn refresh_jwks(&self) -> Result<(), AuthError> {
        let body: JwksResponse = self
            .http
            .get(&self.config.jwks_url)
            .send()
            .await
            .map_err(|e| AuthError::Jwks(e.to_string()))?
            .error_for_status()
            .map_err(|e| AuthError::Jwks(e.to_string()))?
            .json()
            .await
            .map_err(|e| AuthError::Jwks(e.to_string()))?;

        let mut keys = HashMap::new();
        for jwk in body.keys {
            let Some(kid) = jwk.kid.clone() else {
                continue;
            };
            let (Some(n), Some(e)) = (jwk.n.as_deref(), jwk.e.as_deref()) else {
                warn!(%kid, "jwks entry missing n/e");
                continue;
            };
            match DecodingKey::from_rsa_components(n, e) {
                Ok(key) => {
                    keys.insert(kid, key);
                }
                Err(err) => warn!(%kid, %err, "failed to parse jwk"),
            }
        }

        if keys.is_empty() {
            return Err(AuthError::Jwks("jwks contained no usable RSA keys".into()));
        }

        *self.cache.write() = Some(JwksCache {
            keys,
            fetched_at: Instant::now(),
        });
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
struct JwksResponse {
    keys: Vec<Jwk>,
}

#[derive(Debug, Deserialize)]
struct Jwk {
    kid: Option<String>,
    n: Option<String>,
    e: Option<String>,
}

/// Extract and validate `sub`/org/audience from already-signature-verified claims.
fn build_claims(claims: &Value, config: &ZitadelAuthConfig) -> Result<VerifiedClaims, AuthError> {
    check_audience(claims, config)?;

    let sub = claims
        .get("sub")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AuthError::InvalidToken("missing sub".into()))?
        .to_string();

    let org_id = claims
        .get(CLAIM_ORG_ID)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            claims
                .get(CLAIM_RESOURCE_OWNER_ID)
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        })
        .ok_or_else(|| {
            AuthError::InvalidToken(format!(
                "missing org claim ({CLAIM_ORG_ID} or {CLAIM_RESOURCE_OWNER_ID})"
            ))
        })?
        .to_string();

    Ok(VerifiedClaims { sub, org_id })
}

fn check_audience(claims: &Value, config: &ZitadelAuthConfig) -> Result<(), AuthError> {
    if config.audiences.is_empty() {
        return Ok(());
    }

    let Some(aud_value) = claims.get("aud") else {
        // Local Zitadel PATs often omit aud; soft-bind is the gate.
        debug!("token has no aud claim; skipping audience check");
        return Ok(());
    };

    let token_auds: Vec<String> = match aud_value {
        Value::String(s) if !s.is_empty() => vec![s.clone()],
        Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };

    if token_auds.is_empty() {
        debug!("token aud empty; skipping audience check");
        return Ok(());
    }

    let ok = token_auds
        .iter()
        .any(|a| config.audiences.iter().any(|e| e == a));
    if !ok {
        return Err(AuthError::InvalidToken(
            "token audience does not match CHELADB_ZITADEL_AUDIENCES".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(audiences: Vec<String>) -> ZitadelAuthConfig {
        ZitadelAuthConfig {
            issuer: "http://issuer.test".into(),
            jwks_url: "http://issuer.test/keys".into(),
            audiences,
        }
    }

    #[test]
    fn build_claims_uses_org_id_claim_when_present() {
        let claims = json!({
            "sub": "user-1",
            CLAIM_ORG_ID: "org-1",
            CLAIM_RESOURCE_OWNER_ID: "org-2",
        });
        let result = build_claims(&claims, &config(vec![])).unwrap();
        assert_eq!(result.sub, "user-1");
        assert_eq!(result.org_id, "org-1");
    }

    #[test]
    fn build_claims_falls_back_to_resource_owner_claim() {
        let claims = json!({
            "sub": "user-1",
            CLAIM_RESOURCE_OWNER_ID: "org-2",
        });
        let result = build_claims(&claims, &config(vec![])).unwrap();
        assert_eq!(result.org_id, "org-2");
    }

    #[test]
    fn build_claims_errors_when_no_org_claim_present() {
        let claims = json!({ "sub": "user-1" });
        let err = build_claims(&claims, &config(vec![])).unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn build_claims_errors_when_sub_missing() {
        let claims = json!({ CLAIM_ORG_ID: "org-1" });
        let err = build_claims(&claims, &config(vec![])).unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn build_claims_errors_when_sub_empty() {
        let claims = json!({ "sub": "", CLAIM_ORG_ID: "org-1" });
        let err = build_claims(&claims, &config(vec![])).unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn build_claims_skips_audience_check_when_config_has_none() {
        let claims = json!({ "sub": "user-1", CLAIM_ORG_ID: "org-1", "aud": "someone-else" });
        assert!(build_claims(&claims, &config(vec![])).is_ok());
    }

    #[test]
    fn build_claims_skips_audience_check_when_token_has_no_aud() {
        let claims = json!({ "sub": "user-1", CLAIM_ORG_ID: "org-1" });
        assert!(build_claims(&claims, &config(vec!["expected-aud".into()])).is_ok());
    }

    #[test]
    fn build_claims_accepts_matching_audience() {
        let claims = json!({ "sub": "user-1", CLAIM_ORG_ID: "org-1", "aud": "expected-aud" });
        assert!(build_claims(&claims, &config(vec!["expected-aud".into()])).is_ok());
    }

    #[test]
    fn build_claims_accepts_matching_audience_in_array() {
        let claims = json!({
            "sub": "user-1",
            CLAIM_ORG_ID: "org-1",
            "aud": ["other-aud", "expected-aud"],
        });
        assert!(build_claims(&claims, &config(vec!["expected-aud".into()])).is_ok());
    }

    #[test]
    fn build_claims_rejects_mismatched_audience() {
        let claims = json!({ "sub": "user-1", CLAIM_ORG_ID: "org-1", "aud": "wrong-aud" });
        let err = build_claims(&claims, &config(vec!["expected-aud".into()])).unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }
}

#[cfg(feature = "test-support")]
pub mod test_support {
    //! In-process JWKS mock for tests. No network calls beyond localhost.

    use crate::{ZitadelAuthConfig, CLAIM_ORG_ID};
    use axum::{routing::get, Json, Router};
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use jsonwebtoken::{encode, EncodingKey, Header};
    use rsa::pkcs1::EncodeRsaPrivateKey;
    use rsa::traits::PublicKeyParts;
    use rsa::RsaPrivateKey;
    use serde_json::{json, Value};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    /// A locally-hosted JWKS endpoint backed by a freshly generated RSA keypair,
    /// plus a signer for minting test JWTs against that same key.
    pub struct MockJwks {
        issuer: String,
        jwks_url: String,
        kid: String,
        encoding_key: EncodingKey,
        _shutdown: oneshot::Sender<()>,
    }

    impl MockJwks {
        /// Start the mock JWKS server on an ephemeral localhost port.
        pub async fn start() -> Self {
            let private_key = RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).expect("rsa keygen");
            let public_key = private_key.to_public_key();
            let n = URL_SAFE_NO_PAD.encode(public_key.n().to_bytes_be());
            let e = URL_SAFE_NO_PAD.encode(public_key.e().to_bytes_be());
            let kid = "test-key-1".to_string();

            let pem = private_key
                .to_pkcs1_pem(rsa::pkcs8::LineEnding::LF)
                .expect("encode rsa pem");
            let encoding_key =
                EncodingKey::from_rsa_pem(pem.as_bytes()).expect("build jsonwebtoken encoding key");

            let jwks_body = json!({
                "keys": [{
                    "kty": "RSA",
                    "use": "sig",
                    "alg": "RS256",
                    "kid": kid,
                    "n": n,
                    "e": e,
                }]
            });

            let app = Router::new().route(
                "/oauth/v2/keys",
                get(move || {
                    let body = jwks_body.clone();
                    async move { Json(body) }
                }),
            );

            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind mock jwks listener");
            let addr = listener.local_addr().expect("local addr");

            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            tokio::spawn(async move {
                let _ = axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = shutdown_rx.await;
                    })
                    .await;
            });

            let issuer = format!("http://{addr}");
            let jwks_url = format!("http://{addr}/oauth/v2/keys");

            Self {
                issuer,
                jwks_url,
                kid,
                encoding_key,
                _shutdown: shutdown_tx,
            }
        }

        pub fn issuer(&self) -> &str {
            &self.issuer
        }

        pub fn jwks_url(&self) -> &str {
            &self.jwks_url
        }

        pub fn kid(&self) -> &str {
            &self.kid
        }

        /// Build a `ZitadelAuthConfig` pointing at this mock's issuer/JWKS URL.
        pub fn config(&self, audiences: Vec<String>) -> ZitadelAuthConfig {
            ZitadelAuthConfig {
                issuer: self.issuer.clone(),
                jwks_url: self.jwks_url.clone(),
                audiences,
            }
        }

        /// Sign arbitrary claims with this mock's key, using the correct `kid` header.
        /// Callers control every claim, including `iss`/`exp`, for negative-path tests.
        pub fn sign(&self, claims: Value) -> String {
            let mut header = Header::new(jsonwebtoken::Algorithm::RS256);
            header.kid = Some(self.kid.clone());
            encode(&header, &claims, &self.encoding_key).expect("sign test jwt")
        }

        /// Sign a well-formed, currently-valid token for the given subject/org.
        pub fn token_for_org(&self, sub: &str, org_id: &str, ttl_secs: i64) -> String {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time")
                .as_secs() as i64;
            let claims = json!({
                "sub": sub,
                "iss": self.issuer,
                "iat": now,
                "exp": now + ttl_secs,
                CLAIM_ORG_ID: org_id,
            });
            self.sign(claims)
        }
    }

    /// A JWKS endpoint that's reachable but always answers with a server error.
    ///
    /// Prefer this over pointing at a closed/dropped port when a test needs a
    /// deterministic JWKS-fetch failure: an actually-listening server fails on
    /// every request without racing another test's ephemeral port bind for a
    /// just-freed port number.
    pub struct BrokenJwks {
        jwks_url: String,
        _shutdown: oneshot::Sender<()>,
    }

    impl BrokenJwks {
        pub async fn start() -> Self {
            use axum::http::StatusCode;

            let app = Router::new()
                .route("/oauth/v2/keys", get(|| async { StatusCode::INTERNAL_SERVER_ERROR }));

            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind broken jwks listener");
            let addr = listener.local_addr().expect("local addr");

            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            tokio::spawn(async move {
                let _ = axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = shutdown_rx.await;
                    })
                    .await;
            });

            Self {
                jwks_url: format!("http://{addr}/oauth/v2/keys"),
                _shutdown: shutdown_tx,
            }
        }

        pub fn jwks_url(&self) -> &str {
            &self.jwks_url
        }
    }
}
