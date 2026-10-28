//! Authentication for Chelabase.
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
use chelabase_types::file_value::{FileValue, Sensitivity};
use thiserror::Error;
use tracing::{debug, warn};

pub mod app_token;
pub mod internal;

/// Zitadel org id claim (when asserted via org-scoped flows).
pub const CLAIM_ORG_ID: &str = "urn:zitadel:iam:org:id";
/// User's owning organization (request scope `urn:zitadel:iam:user:resourceowner`).
pub const CLAIM_RESOURCE_OWNER_ID: &str = "urn:zitadel:iam:user:resourceowner:id";

/// The audiences a token must name one of (#22): never empty, never skipped.
#[derive(Debug, Clone)]
pub enum Audiences {
    /// A fixed list: `CHELABASE_ZITADEL_AUDIENCES`, or the plain ids. An empty one (only code
    /// can build it) accepts no token.
    List(Vec<String>),
    /// The bootstrap's ids, read when first needed (`CHELABASE_ZITADEL_PROJECT_ID[_FILE]`,
    /// `CHELABASE_ZITADEL_API_CLIENT_ID[_FILE]`): until every one is there, every token is
    /// refused as [`AuthError::NotReady`].
    Ids(Vec<FileValue>),
}

impl From<Vec<String>> for Audiences {
    fn from(list: Vec<String>) -> Self {
        Self::List(list)
    }
}

impl Audiences {
    /// The audiences now. `NotReady` while an id file isn't written (or is refused); `Config`
    /// for an empty list.
    pub fn current(&self) -> Result<Vec<String>, AuthError> {
        let list: Vec<String> = match self {
            Self::List(list) => list.clone(),
            Self::Ids(ids) => ids
                .iter()
                .map(|id| id.get().map(str::to_string))
                .collect::<Result<_, _>>()
                .map_err(|e| AuthError::NotReady(e.to_string()))?,
        };
        if list.iter().all(|a| a.trim().is_empty()) {
            return Err(AuthError::Config(
                "no token audience is configured (CHELABASE_ZITADEL_PROJECT_ID_FILE and \
                 CHELABASE_ZITADEL_API_CLIENT_ID_FILE, or CHELABASE_ZITADEL_AUDIENCES)"
                    .into(),
            ));
        }
        Ok(list)
    }

    /// Whether [`Self::current`] has the audiences (a service's readiness).
    pub fn is_ready(&self) -> bool {
        self.current().is_ok()
    }
}

/// Issuer / JWKS settings for local JWT verification.
#[derive(Debug, Clone)]
pub struct ZitadelAuthConfig {
    /// OIDC issuer URL, e.g. `http://auth.chelabase.localhost:8000`.
    pub issuer: String,
    /// JWKS URL (usually `{issuer}/oauth/v2/keys`). Only where the keys are fetched: a token's
    /// `iss` is always checked against [`Self::issuer`].
    pub jwks_url: String,
    /// The `Host` the key fetch carries, when `jwks_url` is Zitadel's in-network address
    /// (`CHELABASE_ZITADEL_INTERNAL_URL`, #24): the public auth host. `None`: the URL's own.
    pub jwks_host: Option<String>,
    /// The audiences a token must name one of (the project id and the API app's client id).
    pub audiences: Audiences,
}

impl ZitadelAuthConfig {
    pub fn from_env() -> Result<Self, AuthError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// The issuer comes from `CHELABASE_AUTH_HOST` (and the public scheme), and nothing else.
    /// The JWKS URL: `CHELABASE_ZITADEL_JWKS_URL` (fetched as it is), else
    /// `{CHELABASE_ZITADEL_INTERNAL_URL}/oauth/v2/keys` with the auth host as `Host` (#24: behind
    /// a TLS proxy the public URL isn't reachable inside the network), else
    /// `{issuer}/oauth/v2/keys`. A malformed internal URL refuses the config.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, AuthError> {
        let issuer = chelabase_types::entry::issuer_from_lookup(&get)
            .map_err(|e| AuthError::Config(e.to_string()))?
            .ok_or_else(|| AuthError::Config("CHELABASE_AUTH_HOST is not set".into()))?;
        let zitadel = chelabase_types::entry::zitadel_at(&get, &issuer)
            .map_err(|e| AuthError::Config(e.to_string()))?;
        let (jwks_url, jwks_host) = match get("CHELABASE_ZITADEL_JWKS_URL")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
        {
            Some(url) => (url, None),
            None => (format!("{}/oauth/v2/keys", zitadel.base), zitadel.host),
        };
        let audiences = audiences_from_lookup(&get)?;
        Ok(Self {
            issuer,
            jwks_url,
            jwks_host,
            audiences,
        })
    }
}

/// `CHELABASE_ZITADEL_AUDIENCES` (a comma-separated list; the workbench's demo.env), else the
/// project id and the API client id, each `NAME` or `NAME_FILE` (the bootstrap's `project-id`
/// and `api-client-id`, which may appear after the service starts).
///
/// None of them configured is not a startup error, because the workbench starts the gateway
/// and the orchestrator before its bootstrap has written demo.env. It is an empty list, which
/// accepts no token: every Zitadel bearer is refused (503) and the service isn't ready, with a
/// warning at startup. An empty list never means "accept any".
fn audiences_from_lookup(get: &impl Fn(&str) -> Option<String>) -> Result<Audiences, AuthError> {
    const LIST: &str = "CHELABASE_ZITADEL_AUDIENCES";
    const IDS: [&str; 2] = ["CHELABASE_ZITADEL_PROJECT_ID", "CHELABASE_ZITADEL_API_CLIENT_ID"];
    let non_blank = |name: &str| get(name).filter(|v| !v.trim().is_empty());
    let mut ids = Vec::new();
    for name in IDS {
        if let Some(id) = FileValue::from_lookup(get, name, Sensitivity::Public).map_err(AuthError::Config)? {
            ids.push(id);
        }
    }
    if let Some(raw) = get(LIST) {
        let list: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        if !list.is_empty() {
            if let Some(file) = IDS.iter().map(|n| format!("{n}_FILE")).find(|f| non_blank(f).is_some()) {
                return Err(AuthError::Config(format!("set {LIST} or {file}, not both")));
            }
            return Ok(Audiences::List(list));
        }
    }
    if ids.is_empty() {
        warn!(
            "no token audience is configured (CHELABASE_ZITADEL_PROJECT_ID_FILE and \
             CHELABASE_ZITADEL_API_CLIENT_ID_FILE, the bootstrap's project-id and api-client-id, \
             or {LIST}): every Zitadel token is refused"
        );
        return Ok(Audiences::List(Vec::new()));
    }
    Ok(Audiences::Ids(ids))
}

/// Claims extracted after successful verification.
#[derive(Debug, Clone)]
pub struct VerifiedClaims {
    pub sub: String,
    pub org_id: String,
    /// The `email` claim, when the token carries a non-empty one.
    pub email: Option<String>,
    /// The `name` claim, when the token carries a non-empty one.
    pub name: Option<String>,
    /// The `exp` claim (unix seconds).
    pub exp: u64,
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("auth config error: {0}")]
    Config(String),
    #[error("missing or invalid authorization bearer token")]
    Unauthorized,
    #[error("token verification failed: {0}")]
    InvalidToken(String),
    /// The token's `exp` has passed (beyond the leeway).
    #[error("token expired")]
    Expired,
    #[error("jwks fetch failed: {0}")]
    Jwks(String),
    /// The audiences aren't there yet (the bootstrap writes them after the service starts), or
    /// their file is refused: no token is checked, so none passes (503).
    #[error("the token audiences aren't ready: {0}")]
    NotReady(String),
}

/// Alias kept for existing call sites that only load config.
pub type AuthConfigError = AuthError;

struct JwksCache {
    keys: HashMap<String, DecodingKey>,
    fetched_at: Instant,
}

/// How long one JWKS fetch may take (RT1 S14).
pub const JWKS_FETCH_TIMEOUT: Duration = Duration::from_secs(3);

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
            // A hung JWKS endpoint fails the verification well inside its callers' budgets
            // (RT1 S14: realtime gives an authorize 20 s).
            http: reqwest::Client::builder()
                .timeout(JWKS_FETCH_TIMEOUT)
                .build()
                .unwrap_or_default(),
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
        // Before anything else: without audiences no token is checked, so none passes.
        let audiences = self.config.audiences.current()?;

        let header = decode_header(token)
            .map_err(|e| AuthError::InvalidToken(format!("header: {e}")))?;
        let kid = header
            .kid
            .ok_or_else(|| AuthError::InvalidToken("missing kid".into()))?;

        let key = self.decoding_key_for_kid(&kid).await?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(std::slice::from_ref(&self.config.issuer));
        // `set_issuer` only checks an `iss` that is there: a token must carry one (#24).
        validation.set_required_spec_claims(&["exp", "iss"]);
        validation.validate_aud = false;
        validation.leeway = 60;

        let data = decode::<Value>(token, &key, &validation).map_err(|e| match e.kind() {
            jsonwebtoken::errors::ErrorKind::ExpiredSignature => AuthError::Expired,
            _ => AuthError::InvalidToken(e.to_string()),
        })?;

        // m13 audit B1: jsonwebtoken's issuer check also passes a JSON list naming the issuer
        // among others. `iss` is one string, and it must be the issuer exactly.
        if data.claims.get("iss").and_then(Value::as_str) != Some(self.config.issuer.as_str()) {
            return Err(AuthError::InvalidToken("InvalidIssuer: iss must be the issuer, as one string".into()));
        }
        check_audience(&data.claims, &audiences)?;
        build_claims(&data.claims)
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
        let mut request = self.http.get(&self.config.jwks_url);
        if let Some(host) = &self.config.jwks_host {
            request = request.header(reqwest::header::HOST, host);
        }
        let body: JwksResponse = request
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
fn build_claims(claims: &Value) -> Result<VerifiedClaims, AuthError> {
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

    let text = |key: &str| {
        claims
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    // `verify`'s validation requires `exp` (and checks it); a fractional one is truncated to
    // whole seconds.
    let exp = claims
        .get("exp")
        .and_then(|v| v.as_u64().or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64)))
        .unwrap_or(0);
    Ok(VerifiedClaims { sub, org_id, email: text("email"), name: text("name"), exp })
}

/// The token must name one of `expected` in `aud` (a string or an array). No `aud`, an empty
/// one, or an empty `expected` is refused: the check is never skipped (#22).
fn check_audience(claims: &Value, expected: &[String]) -> Result<(), AuthError> {
    let token_auds: Vec<&str> = match claims.get("aud") {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(arr)) => arr.iter().filter_map(|v| v.as_str()).collect(),
        _ => Vec::new(),
    };
    let token_auds: Vec<&str> = token_auds.into_iter().filter(|a| !a.is_empty()).collect();
    if token_auds.is_empty() {
        debug!("token has no aud claim; refused");
        return Err(AuthError::InvalidToken("the token names no audience".into()));
    }
    let ok = token_auds
        .iter()
        .any(|a| expected.iter().any(|e| !e.is_empty() && e == a));
    if !ok {
        return Err(AuthError::InvalidToken(
            "token audience does not match the configured audiences".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn aud(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn build_claims_uses_org_id_claim_when_present() {
        let claims = json!({
            "sub": "user-1",
            CLAIM_ORG_ID: "org-1",
            CLAIM_RESOURCE_OWNER_ID: "org-2",
        });
        let result = build_claims(&claims).unwrap();
        assert_eq!(result.sub, "user-1");
        assert_eq!(result.org_id, "org-1");
    }

    #[test]
    fn build_claims_falls_back_to_resource_owner_claim() {
        let claims = json!({
            "sub": "user-1",
            CLAIM_RESOURCE_OWNER_ID: "org-2",
        });
        let result = build_claims(&claims).unwrap();
        assert_eq!(result.org_id, "org-2");
    }

    #[test]
    fn build_claims_reads_email_and_name_when_present() {
        let claims = json!({
            "sub": "user-1",
            CLAIM_ORG_ID: "org-1",
            "email": "Bob@X.com",
            "name": "Bob Builder",
        });
        let result = build_claims(&claims).unwrap();
        assert_eq!(result.email.as_deref(), Some("Bob@X.com"));
        assert_eq!(result.name.as_deref(), Some("Bob Builder"));

        let bare = build_claims(&json!({ "sub": "u", CLAIM_ORG_ID: "o", "email": "" })).unwrap();
        assert_eq!(bare.email, None, "an empty claim is absent");
        assert_eq!(bare.name, None);
    }

    #[test]
    fn build_claims_errors_when_no_org_claim_present() {
        let claims = json!({ "sub": "user-1" });
        let err = build_claims(&claims).unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn build_claims_errors_when_sub_missing() {
        let claims = json!({ CLAIM_ORG_ID: "org-1" });
        let err = build_claims(&claims).unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn build_claims_errors_when_sub_empty() {
        let claims = json!({ "sub": "", CLAIM_ORG_ID: "org-1" });
        let err = build_claims(&claims).unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    // #22: an empty audience list, or a token without an audience, is refused (it used to be
    // skipped).
    #[test]
    fn the_audience_check_is_never_skipped() {
        let claims = json!({ "sub": "user-1", CLAIM_ORG_ID: "org-1", "aud": "someone-else" });
        assert!(check_audience(&claims, &[]).is_err());
        assert!(check_audience(&claims, &aud(&[""])).is_err());
        for no_aud in [json!({}), json!({ "aud": "" }), json!({ "aud": [] }), json!({ "aud": 7 })] {
            assert!(check_audience(&no_aud, &aud(&["expected-aud"])).is_err(), "{no_aud}");
        }
    }

    #[test]
    fn matching_audiences_pass_and_others_are_refused() {
        let one = json!({ "aud": "expected-aud" });
        assert!(check_audience(&one, &aud(&["expected-aud"])).is_ok());
        let many = json!({ "aud": ["other-aud", "expected-aud"] });
        assert!(check_audience(&many, &aud(&["expected-aud"])).is_ok());
        let wrong = json!({ "aud": "wrong-aud" });
        let err = check_audience(&wrong, &aud(&["expected-aud"])).unwrap_err();
        assert!(matches!(err, AuthError::InvalidToken(_)));
    }

    #[test]
    fn an_empty_list_is_not_ready_to_check_anything() {
        assert!(Audiences::List(vec![]).current().is_err());
        assert!(!Audiences::List(vec![" ".into()]).is_ready());
        assert_eq!(Audiences::List(aud(&["p1"])).current().unwrap(), aud(&["p1"]));
    }
}

#[cfg(feature = "test-support")]
pub mod test_support {
    //! In-process JWKS mock for tests. No network calls beyond localhost.

    use crate::{ZitadelAuthConfig, CLAIM_ORG_ID};

    /// The audience every mock token names (unless a test sets its own `aud`), and the one
    /// [`MockJwks::config`] expects when given an empty list.
    pub const MOCK_AUDIENCE: &str = "chelabase-test-api";
    use axum::{routing::get, Json, Router};
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use jsonwebtoken::{encode, EncodingKey, Header};
    use aws_lc_rs::encoding::AsDer;
    use aws_lc_rs::rsa::{KeyPair, KeySize, PublicKeyComponents};
    use aws_lc_rs::signature::KeyPair as _;
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
            let key_pair = KeyPair::generate(KeySize::Rsa2048).expect("rsa keygen");
            let components: PublicKeyComponents<Vec<u8>> = key_pair.public_key().into();
            let n = URL_SAFE_NO_PAD.encode(&components.n);
            let e = URL_SAFE_NO_PAD.encode(&components.e);
            let kid = "test-key-1".to_string();

            let pkcs8 = AsDer::<aws_lc_rs::encoding::Pkcs8V1Der>::as_der(&key_pair)
                .expect("encode rsa pkcs8");
            let body = base64::engine::general_purpose::STANDARD.encode(pkcs8.as_ref());
            let wrapped = body
                .as_bytes()
                .chunks(64)
                .map(|c| std::str::from_utf8(c).expect("base64 is ascii"))
                .collect::<Vec<_>>()
                .join("\n");
            let pem = format!("-----BEGIN PRIVATE KEY-----\n{wrapped}\n-----END PRIVATE KEY-----\n");
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

        /// Build a `ZitadelAuthConfig` pointing at this mock's issuer/JWKS URL. An empty
        /// `audiences` means [`MOCK_AUDIENCE`] (the one its tokens name): no config accepts
        /// any audience.
        pub fn config(&self, audiences: Vec<String>) -> ZitadelAuthConfig {
            let audiences = if audiences.is_empty() { vec![MOCK_AUDIENCE.to_string()] } else { audiences };
            ZitadelAuthConfig {
                issuer: self.issuer.clone(),
                jwks_url: self.jwks_url.clone(),
                jwks_host: None,
                audiences: audiences.into(),
            }
        }

        /// Sign claims with this mock's key, using the correct `kid` header. Claims without an
        /// `aud` get [`MOCK_AUDIENCE`]; callers control every other claim, including
        /// `iss`/`exp`, for negative-path tests ([`Self::sign_exact`] adds nothing).
        pub fn sign(&self, mut claims: Value) -> String {
            if let Some(map) = claims.as_object_mut() {
                map.entry("aud").or_insert_with(|| json!(MOCK_AUDIENCE));
            }
            self.sign_exact(claims)
        }

        /// Sign exactly `claims`.
        pub fn sign_exact(&self, claims: Value) -> String {
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
