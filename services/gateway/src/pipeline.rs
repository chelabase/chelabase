//! The fixed request pipeline:
//! request id → scope → route → CORS → auth → internal identity.
//!
//! Pure over request parts; `proxy.rs` adapts it to Pingora. Nothing here is
//! configurable per route except the typed `Policy` from `cheladb-routes`.

use crate::data_keys::{DataKeyResolver, KeyError};
use crate::registry::SharedRegistry;
use crate::user_tokens::{UserTokenError, UserTokens};
use cheladb_auth::internal::{AuthMethod, DataIdentity, Identity, InternalSigner, IDENTITY_HEADER};
use cheladb_auth::{AuthError, JwtVerifier};
use cheladb_routes::{match_endpoint, Auth, Cors, Endpoint, Match, Scope, Service};
use cheladb_types::data_key::DataKeyKind;
use http::HeaderMap;
use serde_json::{json, Value};
use std::net::IpAddr;
use std::sync::Arc;
use tracing::error;
use uuid::Uuid;

pub const REQUEST_ID_HEADER: &str = "x-request-id";
/// The gateway's own liveness check; answered before host or route checks.
pub const GATEWAY_HEALTH_PATH: &str = "/_gateway/health";
const MAX_REQUEST_ID_LEN: usize = 128;
/// Client headers never forwarded upstream: upstreams only ever see the
/// gateway-minted identity.
/// The end-user token header; checked by the gateway, never forwarded.
pub const USER_TOKEN_HEADER: &str = "cheladb-user";
const STRIPPED_REQUEST_HEADERS: &[&str] = &[IDENTITY_HEADER, "authorization", USER_TOKEN_HEADER];
const CORS_ALLOW_HEADERS: &str = "authorization, content-type, x-request-id";
const CORS_MAX_AGE_SECS: &str = "600";

/// The parts of an incoming request the pipeline looks at.
pub struct RequestParts<'a> {
    pub method: &'a str,
    /// Path only, without the query string.
    pub path: &'a str,
    pub host: Option<&'a str>,
    pub headers: &'a HeaderMap,
    /// The downstream socket address (never `X-Forwarded-For`).
    pub client_ip: Option<IpAddr>,
}

/// A response the gateway writes itself, without calling an upstream.
#[derive(Debug, Clone, PartialEq)]
pub struct GatewayResponse {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Option<Value>,
}

/// Instructions for proxying a request upstream.
#[derive(Debug, Clone, PartialEq)]
pub struct ProxyPlan {
    pub endpoint: &'static Endpoint,
    /// Internal identity token for the upstream, when the endpoint requires auth.
    pub identity: Option<String>,
    /// Headers added to the response returned to the client (CORS).
    pub response_headers: Vec<(&'static str, String)>,
}

/// Header edits for the upstream request: remove first, then set.
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamHeaders {
    pub remove: &'static [&'static str],
    pub set: Vec<(&'static str, String)>,
}

impl ProxyPlan {
    /// The security-relevant upstream header edits for this plan.
    pub fn upstream_headers(&self, request_id: &str) -> UpstreamHeaders {
        let mut set = vec![(REQUEST_ID_HEADER, request_id.to_string())];
        if let Some(token) = &self.identity {
            set.push((IDENTITY_HEADER, token.clone()));
        }
        UpstreamHeaders {
            remove: STRIPPED_REQUEST_HEADERS,
            set,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Respond(GatewayResponse),
    Proxy(ProxyPlan),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Handled {
    pub request_id: String,
    pub outcome: Outcome,
}

pub struct Pipeline {
    registry: Arc<SharedRegistry>,
    studio_origins: Vec<String>,
    verifier: JwtVerifier,
    signer: InternalSigner,
    keys: Arc<DataKeyResolver>,
    users: Arc<UserTokens>,
}

impl Pipeline {
    pub fn new(
        registry: Arc<SharedRegistry>,
        studio_origins: Vec<String>,
        verifier: JwtVerifier,
        signer: InternalSigner,
        keys: Arc<DataKeyResolver>,
        users: Arc<UserTokens>,
    ) -> Self {
        Self {
            registry,
            studio_origins,
            verifier,
            signer,
            keys,
            users,
        }
    }

    pub async fn handle(&self, req: &RequestParts<'_>) -> Handled {
        let request_id = request_id(req.headers);
        let outcome = self.route(req, &request_id).await;
        Handled {
            request_id,
            outcome,
        }
    }

    async fn route(&self, req: &RequestParts<'_>, request_id: &str) -> Outcome {
        if req.method == "GET" && req.path == GATEWAY_HEALTH_PATH {
            return respond(200, Vec::new(), json!({ "status": "ok" }));
        }

        let Some(scope) = self.registry.load().scope_for(req.host) else {
            return respond(404, Vec::new(), error_json("not_found", "unknown host"));
        };

        if let Some(preflight) = self.preflight(scope, req) {
            return Outcome::Respond(preflight);
        }

        let data_route = req.path == "/data" || req.path.starts_with("/data/");
        let endpoint = match match_endpoint(scope, req.method, req.path) {
            Match::Found(endpoint) => endpoint,
            Match::NotFound if data_route => {
                return Outcome::Respond(data_error(
                    404,
                    "invalid_query",
                    "no such data API endpoint: use POST /data/v1/query or GET /data/v1/schema",
                ));
            }
            Match::NotFound => {
                return respond(404, Vec::new(), error_json("not_found", "no route"));
            }
            Match::MethodNotAllowed(allowed) => {
                let allow = allowed
                    .iter()
                    .map(|m| m.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                if data_route {
                    let mut resp = data_error(
                        405,
                        "invalid_query",
                        &format!("this data API endpoint takes {allow}"),
                    );
                    resp.headers.push(("allow", allow));
                    return Outcome::Respond(resp);
                }
                return respond(
                    405,
                    vec![("allow", allow)],
                    error_json("method_not_allowed", "method not allowed"),
                );
            }
        };

        let cors = self.cors_headers(endpoint, req.headers);

        let identity = match endpoint.policy.auth {
            Auth::Public => None,
            Auth::DataKey | Auth::SecretDataKey | Auth::PublicDataKey => {
                let secret_only = endpoint.policy.auth == Auth::SecretDataKey;
                let public_only = endpoint.policy.auth == Auth::PublicDataKey;
                match self
                    .authenticate_key(endpoint.service, req, secret_only, public_only, request_id)
                    .await
                {
                    Ok(token) => Some(token),
                    Err(mut resp) => {
                        resp.headers.extend(cors);
                        return Outcome::Respond(resp);
                    }
                }
            }
            Auth::Management => {
                match self
                    .authenticate(endpoint.service, req.headers, request_id)
                    .await
                {
                    Ok(token) => Some(token),
                    Err(mut resp) => {
                        resp.headers.extend(cors);
                        return Outcome::Respond(resp);
                    }
                }
            }
        };

        Outcome::Proxy(ProxyPlan {
            endpoint,
            identity,
            response_headers: cors,
        })
    }

    /// Answer a CORS preflight for a Studio endpoint without auth or proxying.
    fn preflight(&self, scope: Scope, req: &RequestParts<'_>) -> Option<GatewayResponse> {
        if req.method != "OPTIONS" {
            return None;
        }
        let origin = header_str(req.headers, "origin")?;
        let requested = header_str(req.headers, "access-control-request-method")?;
        let Match::Found(endpoint) = match_endpoint(scope, requested, req.path) else {
            return None;
        };
        if endpoint.policy.cors == Cors::Any {
            return Some(GatewayResponse {
                status: 204,
                headers: vec![
                    ("access-control-allow-origin", "*".to_string()),
                    ("access-control-allow-methods", "GET, POST".to_string()),
                    ("access-control-allow-headers", "authorization, content-type, cheladb-user".to_string()),
                    ("access-control-max-age", CORS_MAX_AGE_SECS.to_string()),
                ],
                body: None,
            });
        }
        if !matches!(endpoint.policy.cors, Cors::Studio | Cors::StudioCredentials) {
            return None;
        }
        let credentials = endpoint.policy.cors == Cors::StudioCredentials;
        let mut headers = vec![("vary", "Origin".to_string())];
        if self.origin_allowed(origin) && !(credentials && self.wildcard()) {
            headers.extend([
                ("access-control-allow-origin", origin.to_string()),
                ("access-control-allow-methods", requested.to_string()),
                ("access-control-allow-headers", CORS_ALLOW_HEADERS.to_string()),
                ("access-control-max-age", CORS_MAX_AGE_SECS.to_string()),
            ]);
            if credentials {
                headers.push(("access-control-allow-credentials", "true".to_string()));
            }
        }
        Some(GatewayResponse {
            status: 204,
            headers,
            body: None,
        })
    }

    /// CORS response headers for a Studio endpoint. `Vary: Origin` is always
    /// present so caches never reuse a response across origins.
    fn cors_headers(&self, endpoint: &Endpoint, headers: &HeaderMap) -> Vec<(&'static str, String)> {
        match endpoint.policy.cors {
            Cors::None => return Vec::new(),
            // Any origin, never credentials; `*` needs no `Vary`.
            Cors::Any => return vec![("access-control-allow-origin", "*".to_string())],
            Cors::Studio | Cors::StudioCredentials => {}
        }
        let credentials = endpoint.policy.cors == Cors::StudioCredentials;
        let mut cors = vec![("vary", "Origin".to_string())];
        if let Some(origin) = header_str(headers, "origin")
            .filter(|o| self.origin_allowed(o) && !(credentials && self.wildcard()))
        {
            cors.push(("access-control-allow-origin", origin.to_string()));
            if credentials {
                cors.push(("access-control-allow-credentials", "true".to_string()));
            }
        }
        cors
    }

    /// Credentialed CORS needs an explicit origin list; a "*" entry disables it.
    fn wildcard(&self) -> bool {
        self.studio_origins.iter().any(|o| o == "*")
    }

    fn origin_allowed(&self, origin: &str) -> bool {
        self.studio_origins
            .iter()
            .any(|allowed| allowed == "*" || allowed == origin)
    }

    /// Resolve the data API key from `Authorization: Bearer` and mint an
    /// internal identity for `service`. Errors use the data API error body.
    async fn authenticate_key(
        &self,
        service: Service,
        req: &RequestParts<'_>,
        secret_only: bool,
        public_only: bool,
        request_id: &str,
    ) -> Result<String, GatewayResponse> {
        let key = bearer_token(req.headers).ok_or(KeyError::Missing).map_err(key_error)?;
        let resolved = self.keys.resolve(key, req.client_ip).await.map_err(key_error)?;
        if let Some(refused) = key_kind_refusal(secret_only, public_only, resolved.kind) {
            return Err(refused);
        }
        let (role, claims) = match header_str(req.headers, USER_TOKEN_HEADER) {
            None if req.headers.contains_key(USER_TOKEN_HEADER) => {
                return Err(user_token_error(UserTokenError::Invalid));
            }
            None => (resolved.role, json!({})),
            Some(_) if resolved.kind == DataKeyKind::Secret => {
                return Err(data_error(
                    400,
                    "user_token_with_secret_key",
                    "a user token can't be sent with a secret key",
                ));
            }
            // Never fall back to the key's own role on a bad token.
            Some(token) => {
                let claims = self
                    .users
                    .check(token, resolved.project_id)
                    .await
                    .map_err(user_token_error)?;
                (
                    "authenticated".to_string(),
                    json!({
                        "sub": claims.sub, "sid": claims.sid, "project": claims.project,
                        "email": claims.email, "email_verified": claims.email_verified,
                        "role": "authenticated", "exp": claims.exp,
                    }),
                )
            }
        };
        let identity = Identity {
            sub: resolved.key_id.to_string(),
            org_id: String::new(),
            auth: AuthMethod::DataKey,
            request_id: request_id.to_string(),
            data: Some(DataIdentity {
                branch_id: resolved.branch_id,
                key_id: resolved.key_id,
                kind: resolved.kind,
                role,
                claims,
                databases: resolved.databases,
                project_id: Some(resolved.project_id),
            }),
        };
        self.signer.sign(service.name(), &identity).map_err(|err| {
            error!("mint internal identity: {err}");
            internal_key_error(request_id)
        })
    }

    /// Verify the management token and mint an internal identity for `service`.
    async fn authenticate(
        &self,
        service: Service,
        headers: &HeaderMap,
        request_id: &str,
    ) -> Result<String, GatewayResponse> {
        let token = bearer_token(headers).ok_or_else(|| unauthorized(&AuthError::Unauthorized))?;
        let claims = self.verifier.verify(token).await.map_err(|err| match err {
            AuthError::Unauthorized | AuthError::InvalidToken(_) => unauthorized(&err),
            AuthError::Jwks(_) | AuthError::Config(_) => {
                error!("auth infrastructure error: {err}");
                json_response(503, error_json("unavailable", "auth unavailable"))
            }
        })?;
        let identity = Identity {
            sub: claims.sub,
            org_id: claims.org_id,
            auth: AuthMethod::ManagementJwt,
            request_id: request_id.to_string(),
            data: None,
        };
        self.signer.sign(service.name(), &identity).map_err(|err| {
            error!("mint internal identity: {err}");
            json_response(500, error_json("internal", "internal error"))
        })
    }
}

/// Reuse a well-formed incoming `X-Request-Id`, else generate a UUIDv4.
pub fn request_id(headers: &HeaderMap) -> String {
    header_str(headers, REQUEST_ID_HEADER)
        .filter(|id| {
            !id.is_empty()
                && id.len() <= MAX_REQUEST_ID_LEN
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
        .map(str::to_string)
        .unwrap_or_else(|| Uuid::new_v4().to_string())
}

fn respond(status: u16, headers: Vec<(&'static str, String)>, body: Value) -> Outcome {
    Outcome::Respond(GatewayResponse {
        status,
        headers,
        body: Some(body),
    })
}

fn json_response(status: u16, body: Value) -> GatewayResponse {
    GatewayResponse {
        status,
        headers: Vec::new(),
        body: Some(body),
    }
}

/// The shared error body, `{"error":{"code","message"}}`, written to match `cheladb_types`'
/// `ApiError::body` (the gateway doesn't depend on that type's axum feature).
pub(crate) fn error_json(code: &str, message: &str) -> Value {
    json!({ "error": { "code": code, "message": message } })
}

/// The data API error body: `{"error":{"code","message"}}`.
fn data_error(status: u16, code: &str, message: &str) -> GatewayResponse {
    let mut resp = json_response(status, error_json(code, message));
    if status == 503 {
        resp.headers.push(("retry-after", "1".to_string()));
    }
    resp
}

fn internal_key_error(request_id: &str) -> GatewayResponse {
    data_error(500, "internal", &format!("internal error (request id {request_id})"))
}

/// The answer for a resolved key of the wrong kind for the endpoint, if any.
fn key_kind_refusal(
    secret_only: bool,
    public_only: bool,
    kind: DataKeyKind,
) -> Option<GatewayResponse> {
    if secret_only && kind != DataKeyKind::Secret {
        return Some(key_error(KeyError::Invalid));
    }
    if public_only && kind == DataKeyKind::Secret {
        return Some(data_error(401, "unauthorized", "this endpoint takes a public key"));
    }
    None
}

fn key_error(err: KeyError) -> GatewayResponse {
    match err {
        KeyError::Missing => data_error(401, "unauthorized", "missing API key: send Authorization: Bearer <key>"),
        KeyError::Invalid => data_error(401, "unauthorized", "invalid, revoked or unauthorized API key"),
        KeyError::RateLimited => data_error(429, "rate_limited", "too many unknown-key lookups; slow down"),
        KeyError::Unavailable => data_error(503, "unavailable", "key lookup is unavailable; retry shortly"),
    }
}

fn user_token_error(err: UserTokenError) -> GatewayResponse {
    match err {
        UserTokenError::Invalid => data_error(401, "user_token_invalid", "the user token is invalid"),
        UserTokenError::Expired => data_error(401, "user_token_expired", "the user token has expired"),
        UserTokenError::Revoked => data_error(401, "user_session_revoked", "the user session was revoked"),
        UserTokenError::Unavailable => {
            data_error(503, "unavailable", "session check is unavailable; retry shortly")
        }
    }
}

fn unauthorized(err: &AuthError) -> GatewayResponse {
    json_response(401, error_json("unauthorized", &err.to_string()))
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}

/// `Authorization: Bearer <token>`, scheme case-insensitive.
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = header_str(headers, "authorization")?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Registry;
    use cheladb_auth::internal::{generate_keypair_pem, InternalVerifier, IDENTITY_HEADER};
    use cheladb_auth::test_support::{BrokenJwks, MockJwks};
    use crate::user_tokens::test_support as ut;
    use cheladb_auth::app_token::AppTokenSigner;
    use cheladb_auth::ZitadelAuthConfig;
    use http::{HeaderName, HeaderValue};

    #[test]
    fn public_only_endpoint_refuses_secret_key() {
        let refused = key_kind_refusal(false, true, DataKeyKind::Secret).expect("refused");
        assert_eq!(refused.status, 401);
        let body = refused.body.unwrap();
        assert_eq!(body["error"]["code"], "unauthorized");
        assert_eq!(body["error"]["message"], "this endpoint takes a public key");
        assert!(key_kind_refusal(false, true, DataKeyKind::Public).is_none());
        // The other policies are unchanged.
        assert!(key_kind_refusal(true, false, DataKeyKind::Public).is_some());
        assert!(key_kind_refusal(true, false, DataKeyKind::Secret).is_none());
        assert!(key_kind_refusal(false, false, DataKeyKind::Secret).is_none());
        assert!(key_kind_refusal(false, false, DataKeyKind::Public).is_none());
    }

    const STUDIO: &str = "http://localhost:3000";

    struct Harness {
        pipeline: Pipeline,
        jwks: MockJwks,
        verify: InternalVerifier,
    }

    async fn harness(api_hosts: &[&str]) -> Harness {
        let jwks = MockJwks::start().await;
        let verifier = JwtVerifier::new(jwks.config(vec![]));
        let (pipeline, verify) = pipeline_with(api_hosts, verifier);
        Harness {
            pipeline,
            jwks,
            verify,
        }
    }

    fn pipeline_with(api_hosts: &[&str], verifier: JwtVerifier) -> (Pipeline, InternalVerifier) {
        let (private_pem, public_pem) = generate_keypair_pem();
        let pipeline = Pipeline::new(
            Arc::new(SharedRegistry::new(Registry::new(api_hosts))),
            vec![STUDIO.to_string()],
            verifier,
            InternalSigner::from_pem(private_pem.as_bytes()).unwrap(),
            Arc::new(DataKeyResolver::new("http://127.0.0.1:1", "unused")),
            Arc::new(UserTokens::new(None, "http://127.0.0.1:1", "unused")),
        );
        (pipeline, InternalVerifier::from_pem(public_pem.as_bytes()).unwrap())
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        map
    }

    async fn run(pipeline: &Pipeline, method: &str, path: &str, host: Option<&str>, pairs: &[(&str, &str)]) -> Handled {
        let headers = headers(pairs);
        pipeline
            .handle(&RequestParts {
                method,
                path,
                host,
                headers: &headers,
                client_ip: None,
            })
            .await
    }

    fn expect_response(outcome: Outcome) -> GatewayResponse {
        match outcome {
            Outcome::Respond(resp) => resp,
            Outcome::Proxy(plan) => panic!("expected a gateway response, got proxy plan {plan:?}"),
        }
    }

    fn expect_plan(outcome: Outcome) -> ProxyPlan {
        match outcome {
            Outcome::Proxy(plan) => plan,
            Outcome::Respond(resp) => panic!("expected a proxy plan, got response {resp:?}"),
        }
    }

    fn header<'a>(headers: &'a [(&'static str, String)], name: &str) -> Option<&'a str> {
        headers.iter().find(|(n, _)| *n == name).map(|(_, v)| v.as_str())
    }

    #[test]
    fn request_id_reuses_a_well_formed_incoming_id() {
        assert_eq!(request_id(&headers(&[("x-request-id", "abc-123_x.y")])), "abc-123_x.y");
    }

    #[test]
    fn request_id_replaces_malformed_or_oversized_ids() {
        for bad in ["has space", "semi;colon", &"a".repeat(129)] {
            let id = request_id(&headers(&[("x-request-id", bad)]));
            assert_ne!(id, bad);
            assert!(Uuid::parse_str(&id).is_ok(), "{id}");
        }
    }

    #[test]
    fn request_id_is_generated_when_missing() {
        assert!(Uuid::parse_str(&request_id(&HeaderMap::new())).is_ok());
    }

    #[tokio::test]
    async fn gateway_health_is_answered_before_host_checks() {
        let h = harness(&["api.example.com"]).await;
        let handled = run(&h.pipeline, "GET", GATEWAY_HEALTH_PATH, Some("other.example.com"), &[]).await;
        let resp = expect_response(handled.outcome);
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, Some(json!({ "status": "ok" })));
    }

    fn shape(code: &str, message: &str) -> Value {
        json!({ "error": { "code": code, "message": message } })
    }

    #[tokio::test]
    async fn unknown_host_is_404_when_api_hosts_are_configured() {
        let h = harness(&["api.example.com"]).await;
        let handled = run(&h.pipeline, "GET", "/health", Some("other.example.com"), &[]).await;
        let resp = expect_response(handled.outcome);
        assert_eq!(resp.status, 404);
        assert_eq!(resp.body, Some(shape("not_found", "unknown host")));
    }

    #[tokio::test]
    async fn undeclared_path_is_404() {
        let h = harness(&[]).await;
        let resp = expect_response(run(&h.pipeline, "GET", "/v1/other", None, &[]).await.outcome);
        assert_eq!(resp.status, 404);
        assert_eq!(resp.body, Some(shape("not_found", "no route")));
    }

    #[tokio::test]
    async fn wrong_method_is_405_with_allow_header() {
        let h = harness(&[]).await;
        let resp = expect_response(run(&h.pipeline, "DELETE", "/v1/databases", None, &[]).await.outcome);
        assert_eq!(resp.status, 405);
        assert_eq!(header(&resp.headers, "allow"), Some("POST"));
        assert_eq!(resp.body, Some(shape("method_not_allowed", "method not allowed")));
    }

    /// Under /data/ the gateway's own 404 and 405 use the data API's error body.
    #[tokio::test]
    async fn data_routes_answer_404_and_405_with_the_data_api_body() {
        let h = harness(&[]).await;
        let resp = expect_response(run(&h.pipeline, "POST", "/data/v1/nope", None, &[]).await.outcome);
        assert_eq!(resp.status, 404);
        let body = resp.body.unwrap();
        assert_eq!(body["error"]["code"], "invalid_query");
        assert!(body["error"]["message"].as_str().unwrap().contains("/data/v1/query"), "{body}");
        let resp = expect_response(run(&h.pipeline, "GET", "/data/v1/query", None, &[]).await.outcome);
        assert_eq!(resp.status, 405);
        assert_eq!(header(&resp.headers, "allow"), Some("POST"));
        let body = resp.body.unwrap();
        assert_eq!(body["error"]["code"], "invalid_query");
        assert!(body["error"]["message"].as_str().unwrap().contains("POST"), "{body}");
        // Elsewhere the management API's body stays.
        let resp = expect_response(run(&h.pipeline, "GET", "/database/x", None, &[]).await.outcome);
        assert_eq!(resp.body, Some(shape("not_found", "no route")));
    }

    #[tokio::test]
    async fn public_endpoint_is_proxied_without_identity() {
        let h = harness(&[]).await;
        let plan = expect_plan(run(&h.pipeline, "GET", "/health", None, &[]).await.outcome);
        assert_eq!(plan.endpoint.service, Service::Orchestrator);
        assert_eq!(plan.identity, None);
    }

    #[tokio::test]
    async fn management_endpoint_without_bearer_is_401() {
        let h = harness(&[]).await;
        let resp = expect_response(run(&h.pipeline, "POST", "/v1/databases", None, &[]).await.outcome);
        assert_eq!(resp.status, 401);
        assert_eq!(error_code(&resp), Some("unauthorized"));
    }

    #[tokio::test]
    async fn management_endpoint_with_invalid_bearer_is_401() {
        let h = harness(&[]).await;
        let resp = expect_response(
            run(&h.pipeline, "POST", "/v1/databases", None, &[("authorization", "Bearer not-a-jwt")])
                .await
                .outcome,
        );
        assert_eq!(resp.status, 401);
        assert_eq!(error_code(&resp), Some("unauthorized"));
    }

    #[tokio::test]
    async fn valid_bearer_mints_identity_for_the_target_service() {
        let h = harness(&[]).await;
        let token = h.jwks.token_for_org("user-1", "org-1", 300);
        let bearer = format!("Bearer {token}");
        let handled = run(
            &h.pipeline,
            "POST",
            "/v1/databases",
            None,
            &[("authorization", &bearer), ("x-request-id", "rid-42")],
        )
        .await;
        assert_eq!(handled.request_id, "rid-42");

        let plan = expect_plan(handled.outcome);
        let claims = h
            .verify
            .verify(plan.identity.as_deref().unwrap(), "orchestrator")
            .unwrap();
        assert_eq!(claims.sub, "user-1");
        assert_eq!(claims.org_id, "org-1");
        assert_eq!(claims.rid, "rid-42");
        assert_eq!(claims.auth, AuthMethod::ManagementJwt);
    }

    #[tokio::test]
    async fn lowercase_bearer_scheme_is_accepted() {
        let h = harness(&[]).await;
        let token = h.jwks.token_for_org("user-1", "org-1", 300);
        let bearer = format!("bearer {token}");
        let plan = expect_plan(
            run(&h.pipeline, "POST", "/v1/databases", None, &[("authorization", &bearer)])
                .await
                .outcome,
        );
        assert!(plan.identity.is_some());
    }

    #[tokio::test]
    async fn jwks_unavailable_is_503() {
        let broken = BrokenJwks::start().await;
        let verifier = JwtVerifier::new(ZitadelAuthConfig {
            issuer: "http://irrelevant-issuer".into(),
            jwks_url: broken.jwks_url().to_string(),
            audiences: vec![],
        });
        let (pipeline, _) = pipeline_with(&[], verifier);
        // Any well-formed JWT with a kid triggers a JWKS fetch, which fails.
        let signer = MockJwks::start().await;
        let bearer = format!("Bearer {}", signer.token_for_org("user-1", "org-1", 300));

        let resp = expect_response(
            run(&pipeline, "POST", "/v1/databases", None, &[("authorization", &bearer)])
                .await
                .outcome,
        );
        assert_eq!(resp.status, 503);
        assert_eq!(resp.body, Some(shape("unavailable", "auth unavailable")));
    }

    #[tokio::test]
    async fn preflight_for_studio_endpoint_is_answered_with_cors_headers() {
        let h = harness(&[]).await;
        let resp = expect_response(
            run(
                &h.pipeline,
                "OPTIONS",
                "/v1/databases",
                None,
                &[("origin", STUDIO), ("access-control-request-method", "POST")],
            )
            .await
            .outcome,
        );
        assert_eq!(resp.status, 204);
        assert_eq!(resp.body, None);
        assert_eq!(header(&resp.headers, "access-control-allow-origin"), Some(STUDIO));
        assert_eq!(header(&resp.headers, "access-control-allow-methods"), Some("POST"));
        assert_eq!(header(&resp.headers, "vary"), Some("Origin"));
    }

    #[tokio::test]
    async fn preflight_from_disallowed_origin_gets_no_cors_headers() {
        let h = harness(&[]).await;
        let resp = expect_response(
            run(
                &h.pipeline,
                "OPTIONS",
                "/v1/databases",
                None,
                &[("origin", "http://evil.example"), ("access-control-request-method", "POST")],
            )
            .await
            .outcome,
        );
        assert_eq!(resp.status, 204);
        assert_eq!(header(&resp.headers, "access-control-allow-origin"), None);
        assert_eq!(header(&resp.headers, "vary"), Some("Origin"));
    }

    #[tokio::test]
    async fn options_without_preflight_headers_is_405() {
        let h = harness(&[]).await;
        let resp = expect_response(run(&h.pipeline, "OPTIONS", "/v1/databases", None, &[]).await.outcome);
        assert_eq!(resp.status, 405);
    }

    #[tokio::test]
    async fn preflight_for_non_cors_endpoint_is_405() {
        let h = harness(&[]).await;
        let resp = expect_response(
            run(
                &h.pipeline,
                "OPTIONS",
                "/health",
                None,
                &[("origin", STUDIO), ("access-control-request-method", "GET")],
            )
            .await
            .outcome,
        );
        assert_eq!(resp.status, 405);
    }

    #[tokio::test]
    async fn proxied_studio_request_from_allowed_origin_gets_cors_headers() {
        let h = harness(&[]).await;
        let bearer = format!("Bearer {}", h.jwks.token_for_org("user-1", "org-1", 300));
        let plan = expect_plan(
            run(
                &h.pipeline,
                "POST",
                "/v1/databases",
                None,
                &[("authorization", &bearer), ("origin", STUDIO)],
            )
            .await
            .outcome,
        );
        assert_eq!(header(&plan.response_headers, "access-control-allow-origin"), Some(STUDIO));
        assert_eq!(header(&plan.response_headers, "vary"), Some("Origin"));
    }

    #[tokio::test]
    async fn proxied_request_from_disallowed_origin_gets_no_cors_headers() {
        let h = harness(&[]).await;
        let bearer = format!("Bearer {}", h.jwks.token_for_org("user-1", "org-1", 300));
        let plan = expect_plan(
            run(
                &h.pipeline,
                "POST",
                "/v1/databases",
                None,
                &[("authorization", &bearer), ("origin", "http://evil.example")],
            )
            .await
            .outcome,
        );
        assert_eq!(header(&plan.response_headers, "access-control-allow-origin"), None);
        assert_eq!(header(&plan.response_headers, "vary"), Some("Origin"));
    }

    // A cache must not reuse an origin-less response for a CORS request.
    #[tokio::test]
    async fn studio_endpoint_varies_on_origin_even_without_origin() {
        let h = harness(&[]).await;
        let bearer = format!("Bearer {}", h.jwks.token_for_org("user-1", "org-1", 300));
        let plan = expect_plan(
            run(&h.pipeline, "POST", "/v1/databases", None, &[("authorization", &bearer)])
                .await
                .outcome,
        );
        assert_eq!(header(&plan.response_headers, "access-control-allow-origin"), None);
        assert_eq!(header(&plan.response_headers, "vary"), Some("Origin"));
    }

    #[tokio::test]
    async fn non_cors_endpoint_never_gets_cors_headers() {
        let h = harness(&[]).await;
        let plan = expect_plan(run(&h.pipeline, "GET", "/health", None, &[("origin", STUDIO)]).await.outcome);
        assert!(plan.response_headers.is_empty());
    }

    #[tokio::test]
    async fn upstream_headers_strip_credentials_even_on_public_routes() {
        let h = harness(&[]).await;
        let plan = expect_plan(run(&h.pipeline, "GET", "/health", None, &[]).await.outcome);
        let edits = plan.upstream_headers("rid-1");
        assert!(edits.remove.contains(&IDENTITY_HEADER));
        assert!(edits.remove.contains(&"authorization"));
        assert_eq!(edits.set, vec![(REQUEST_ID_HEADER, "rid-1".to_string())]);
    }

    #[tokio::test]
    async fn upstream_headers_set_the_minted_identity_on_management_routes() {
        let h = harness(&[]).await;
        let bearer = format!("Bearer {}", h.jwks.token_for_org("user-1", "org-1", 300));
        let plan = expect_plan(
            run(&h.pipeline, "POST", "/v1/databases", None, &[("authorization", &bearer)])
                .await
                .outcome,
        );
        let edits = plan.upstream_headers("rid-2");
        assert!(edits.remove.contains(&IDENTITY_HEADER));
        let identity = edits
            .set
            .iter()
            .find(|(name, _)| *name == IDENTITY_HEADER)
            .map(|(_, value)| value.as_str());
        assert_eq!(identity, plan.identity.as_deref());
        assert!(edits.set.contains(&(REQUEST_ID_HEADER, "rid-2".to_string())));
    }

    // Review Focus 1: browsers hide non-CORS error responses from Studio.
    #[tokio::test]
    async fn unauthorized_response_on_studio_endpoint_carries_cors_headers() {
        let h = harness(&[]).await;
        let resp = expect_response(
            run(&h.pipeline, "POST", "/v1/databases", None, &[("origin", STUDIO)]).await.outcome,
        );
        assert_eq!(resp.status, 401);
        assert_eq!(header(&resp.headers, "access-control-allow-origin"), Some(STUDIO));
    }

    #[tokio::test]
    async fn auth_allows_credentials_for_studio_only() {
        let h = harness(&[]).await;
        let plan = expect_plan(run(&h.pipeline, "POST", "/auth/v1/sign-in", None, &[("origin", STUDIO)]).await.outcome);
        assert_eq!(plan.endpoint.service, Service::Auth);
        assert_eq!(plan.identity, None);
        assert_eq!(header(&plan.response_headers, "access-control-allow-origin"), Some(STUDIO));
        assert_eq!(header(&plan.response_headers, "access-control-allow-credentials"), Some("true"));

        let other = expect_plan(run(&h.pipeline, "POST", "/auth/v1/sign-in", None, &[("origin", "http://evil.example")]).await.outcome);
        assert_eq!(header(&other.response_headers, "access-control-allow-origin"), None);
        assert_eq!(header(&other.response_headers, "access-control-allow-credentials"), None);
    }

    #[tokio::test]
    async fn auth_preflight_allows_credentials() {
        let h = harness(&[]).await;
        let resp = expect_response(run(&h.pipeline, "OPTIONS", "/auth/v1/sign-in", None,
            &[("origin", STUDIO), ("access-control-request-method", "POST")]).await.outcome);
        assert_eq!(resp.status, 204);
        assert_eq!(header(&resp.headers, "access-control-allow-credentials"), Some("true"));
    }

    #[tokio::test]
    async fn a_wildcard_origin_setting_never_allows_credentials() {
        let jwks = MockJwks::start().await;
        let (private_pem, _) = generate_keypair_pem();
        let pipeline = Pipeline::new(
            Arc::new(SharedRegistry::new(Registry::new(Vec::<String>::new()))),
            vec!["*".to_string()],
            JwtVerifier::new(jwks.config(vec![])),
            InternalSigner::from_pem(private_pem.as_bytes()).unwrap(),
            Arc::new(DataKeyResolver::new("http://127.0.0.1:1", "unused")),
            Arc::new(UserTokens::new(None, "http://127.0.0.1:1", "unused")),
        );
        let plan = expect_plan(run(&pipeline, "POST", "/auth/v1/sign-in", None, &[("origin", STUDIO)]).await.outcome);
        assert_eq!(header(&plan.response_headers, "access-control-allow-origin"), None);
        assert_eq!(header(&plan.response_headers, "access-control-allow-credentials"), None);
    }

    // ---- data API keys (Task 4) ----

    use crate::data_keys::test_support::{self as dk, Answer};
    use cheladb_types::data_key::DataKeyKind;

    struct DataHarness {
        pipeline: Pipeline,
        verify: InternalVerifier,
        mock: dk::Mock,
        sessions: ut::Mock,
        app: AppTokenSigner,
    }

    async fn data_harness() -> DataHarness {
        let (url, mock) = dk::start().await;
        let (sessions_url, sessions) = ut::start().await;
        let (app, app_verifier) = ut::keys();
        let jwks = MockJwks::start().await;
        let (private_pem, public_pem) = generate_keypair_pem();
        let pipeline = Pipeline::new(
            Arc::new(SharedRegistry::new(Registry::new(Vec::<String>::new()))),
            vec![STUDIO.to_string()],
            JwtVerifier::new(jwks.config(vec![])),
            InternalSigner::from_pem(private_pem.as_bytes()).unwrap(),
            Arc::new(DataKeyResolver::new(&url, dk::BEARER)),
            Arc::new(UserTokens::new(Some(app_verifier), &sessions_url, ut::BEARER)),
        );
        DataHarness {
            pipeline,
            verify: InternalVerifier::from_pem(public_pem.as_bytes()).unwrap(),
            mock,
            sessions,
            app,
        }
    }

    fn error_code(resp: &GatewayResponse) -> Option<&str> {
        resp.body.as_ref()?["error"]["code"].as_str()
    }

    async fn data_call(h: &DataHarness, method: &str, path: &str, key: Option<&str>) -> Handled {
        let bearer = key.map(|k| format!("Bearer {k}"));
        let mut pairs = Vec::new();
        if let Some(b) = &bearer {
            pairs.push(("authorization", b.as_str()));
        }
        run(&h.pipeline, method, path, None, &pairs).await
    }

    #[tokio::test]
    async fn known_key_mints_rest_identity_with_data() {
        let h = data_harness().await;
        let key = dk::new_key(DataKeyKind::Public);
        let resolved = dk::resolved(DataKeyKind::Public);
        h.mock.set(&key, Answer::Known(resolved.clone()));

        let handled = data_call(&h, "POST", "/data/v1/query", Some(&key)).await;
        let rid = handled.request_id.clone();
        let plan = expect_plan(handled.outcome);
        assert_eq!(plan.endpoint.service, Service::Rest);
        let claims = h.verify.verify(plan.identity.as_deref().unwrap(), "rest").unwrap();
        assert_eq!(claims.auth, AuthMethod::DataKey);
        assert_eq!(claims.sub, resolved.key_id.to_string());
        assert_eq!(claims.org_id, "");
        assert_eq!(claims.rid, rid);
        let data = claims.data.expect("data identity");
        assert_eq!(data.key_id, resolved.key_id);
        assert_eq!(data.branch_id, resolved.branch_id);
        assert_eq!(data.kind, DataKeyKind::Public);
        assert_eq!(data.role, "anon");
        assert_eq!(data.claims, json!({}));
    }

    /// A key's allow-list rides in the minted identity; a key without one mints none.
    #[tokio::test]
    async fn the_allow_list_is_carried_in_the_minted_token() {
        let h = data_harness().await;
        let listed = dk::new_key(DataKeyKind::Secret);
        let mut resolved = dk::resolved(DataKeyKind::Secret);
        resolved.databases = Some(vec!["main".into(), "analytics".into()]);
        h.mock.set(&listed, Answer::Known(resolved));
        let unlisted = dk::new_key(DataKeyKind::Secret);
        h.mock.set(&unlisted, Answer::Known(dk::resolved(DataKeyKind::Secret)));

        for (key, want) in [
            (&listed, Some(vec!["main".to_string(), "analytics".to_string()])),
            (&unlisted, None),
        ] {
            let plan = expect_plan(data_call(&h, "POST", "/data/v1/query", Some(key)).await.outcome);
            let claims = h.verify.verify(plan.identity.as_deref().unwrap(), "rest").unwrap();
            assert_eq!(claims.data.unwrap().databases, want);
        }
    }

    #[tokio::test]
    async fn secret_key_is_accepted_on_the_schema_endpoint() {
        let h = data_harness().await;
        let key = dk::new_key(DataKeyKind::Secret);
        h.mock.set(&key, Answer::Known(dk::resolved(DataKeyKind::Secret)));
        let plan = expect_plan(data_call(&h, "GET", "/data/v1/schema", Some(&key)).await.outcome);
        let claims = h.verify.verify(plan.identity.as_deref().unwrap(), "rest").unwrap();
        assert_eq!(claims.data.unwrap().kind, DataKeyKind::Secret);
    }

    #[tokio::test]
    async fn public_key_on_schema_endpoint_is_401() {
        let h = data_harness().await;
        let key = dk::new_key(DataKeyKind::Public);
        h.mock.set(&key, Answer::Known(dk::resolved(DataKeyKind::Public)));
        let resp = expect_response(data_call(&h, "GET", "/data/v1/schema", Some(&key)).await.outcome);
        assert_eq!(resp.status, 401);
        assert_eq!(error_code(&resp), Some("unauthorized"));
        assert!(resp.body.as_ref().unwrap()["error"]["message"].is_string());
    }

    #[tokio::test]
    async fn missing_or_malformed_key_is_401_unauthorized_without_lookup() {
        let h = data_harness().await;
        for key in [None, Some("not-a-key")] {
            let resp = expect_response(data_call(&h, "POST", "/data/v1/query", key).await.outcome);
            assert_eq!(resp.status, 401);
            assert_eq!(error_code(&resp), Some("unauthorized"));
        }
        // A non-Bearer scheme is a missing key too.
        let resp = expect_response(
            run(&h.pipeline, "POST", "/data/v1/query", None, &[("authorization", "Basic abc")])
                .await
                .outcome,
        );
        assert_eq!(resp.status, 401);
        assert_eq!(h.mock.lookups(), 0);
    }

    #[tokio::test]
    async fn unknown_key_is_401_and_unavailable_is_503_with_retry_after() {
        let h = data_harness().await;
        let unknown = dk::new_key(DataKeyKind::Public);
        let resp = expect_response(data_call(&h, "POST", "/data/v1/query", Some(&unknown)).await.outcome);
        assert_eq!((resp.status, error_code(&resp)), (401, Some("unauthorized")));

        let broken = dk::new_key(DataKeyKind::Public);
        h.mock.set(&broken, Answer::ServerError);
        let resp = expect_response(data_call(&h, "POST", "/data/v1/query", Some(&broken)).await.outcome);
        assert_eq!((resp.status, error_code(&resp)), (503, Some("unavailable")));
        assert!(header(&resp.headers, "retry-after").is_some());
    }

    #[tokio::test]
    async fn miss_flood_is_429_rate_limited() {
        let h = data_harness().await;
        let mut statuses = Vec::new();
        for _ in 0..40 {
            let key = dk::new_key(DataKeyKind::Public);
            let headers = headers(&[("authorization", &format!("Bearer {key}"))]);
            let handled = h
                .pipeline
                .handle(&RequestParts {
                    method: "POST",
                    path: "/data/v1/query",
                    host: None,
                    headers: &headers,
                    client_ip: Some("203.0.113.9".parse().unwrap()),
                })
                .await;
            let resp = expect_response(handled.outcome);
            statuses.push((resp.status, error_code(&resp).map(str::to_string)));
        }
        assert!(statuses.contains(&(429, Some("rate_limited".to_string()))));
        assert!(h.mock.lookups() <= 25, "{}", h.mock.lookups());
    }

    #[tokio::test]
    async fn cors_any_preflight_and_response_headers() {
        let h = data_harness().await;
        for (path, method) in [("/data/v1/query", "POST"), ("/data/v1/schema", "GET")] {
            let resp = expect_response(
                run(
                    &h.pipeline,
                    "OPTIONS",
                    path,
                    None,
                    &[("origin", "https://anything.example"), ("access-control-request-method", method)],
                )
                .await
                .outcome,
            );
            assert_eq!(resp.status, 204, "{path}");
            assert_eq!(header(&resp.headers, "access-control-allow-origin"), Some("*"));
            assert_eq!(header(&resp.headers, "access-control-allow-methods"), Some("GET, POST"));
            assert_eq!(
                header(&resp.headers, "access-control-allow-headers"),
                Some("authorization, content-type, cheladb-user")
            );
            assert_eq!(header(&resp.headers, "access-control-max-age"), Some("600"));
            assert_eq!(header(&resp.headers, "access-control-allow-credentials"), None);
        }
        assert_eq!(h.mock.lookups(), 0, "preflight does no auth");

        // Proxied responses and gateway errors both carry the wildcard origin.
        let key = dk::new_key(DataKeyKind::Public);
        h.mock.set(&key, Answer::Known(dk::resolved(DataKeyKind::Public)));
        let bearer = format!("Bearer {key}");
        let plan = expect_plan(
            run(
                &h.pipeline,
                "POST",
                "/data/v1/query",
                None,
                &[("authorization", &bearer), ("origin", "https://anything.example")],
            )
            .await
            .outcome,
        );
        assert_eq!(header(&plan.response_headers, "access-control-allow-origin"), Some("*"));
        assert_eq!(header(&plan.response_headers, "access-control-allow-credentials"), None);

        let denied = expect_response(
            run(&h.pipeline, "POST", "/data/v1/query", None, &[("origin", "https://anything.example")])
                .await
                .outcome,
        );
        assert_eq!(denied.status, 401);
        assert_eq!(header(&denied.headers, "access-control-allow-origin"), Some("*"));
        assert_eq!(header(&denied.headers, "access-control-allow-credentials"), None);
    }

    #[tokio::test]
    async fn client_authorization_header_is_not_forwarded() {
        let h = data_harness().await;
        let key = dk::new_key(DataKeyKind::Public);
        h.mock.set(&key, Answer::Known(dk::resolved(DataKeyKind::Public)));
        let plan = expect_plan(data_call(&h, "POST", "/data/v1/query", Some(&key)).await.outcome);
        let edits = plan.upstream_headers("rid");
        assert!(edits.remove.contains(&"authorization"));
        assert!(edits.set.iter().any(|(n, _)| *n == IDENTITY_HEADER));
    }

    #[test]
    fn internal_error_body_carries_the_request_id() {
        let resp = internal_key_error("rid-77");
        assert_eq!((resp.status, error_code(&resp)), (500, Some("internal")));
        let message = resp.body.as_ref().unwrap()["error"]["message"].as_str().unwrap();
        assert!(message.contains("rid-77"), "{message}");
    }

    #[tokio::test]
    async fn client_identity_header_is_not_forwarded_on_data_routes() {
        let h = data_harness().await;
        let key = dk::new_key(DataKeyKind::Public);
        h.mock.set(&key, Answer::Known(dk::resolved(DataKeyKind::Public)));
        let bearer = format!("Bearer {key}");
        let plan = expect_plan(
            run(
                &h.pipeline,
                "POST",
                "/data/v1/query",
                None,
                &[("authorization", &bearer), (IDENTITY_HEADER, "forged-token")],
            )
            .await
            .outcome,
        );
        let edits = plan.upstream_headers("rid");
        assert!(edits.remove.contains(&IDENTITY_HEADER));
        let sent: Vec<_> = edits.set.iter().filter(|(n, _)| *n == IDENTITY_HEADER).collect();
        assert_eq!(sent.len(), 1);
        assert_ne!(sent[0].1, "forged-token");
        assert_eq!(Some(sent[0].1.as_str()), plan.identity.as_deref());
    }

    // ---- app-user tokens (step 6b, Task 10) ----

    use uuid::Uuid as TestUuid;

    /// A public key plus a signed token for its project and a session the mock reports as `session`.
    struct UserCase {
        key: String,
        project: TestUuid,
        sid: TestUuid,
    }

    fn user_case(h: &DataHarness, kind: DataKeyKind) -> UserCase {
        let key = dk::new_key(kind);
        let resolved = dk::resolved(kind);
        let project = resolved.project_id;
        h.mock.set(&key, Answer::Known(resolved));
        UserCase { key, project, sid: TestUuid::new_v4() }
    }

    fn user_token(h: &DataHarness, project: TestUuid, sid: TestUuid, exp_in: i64) -> String {
        h.app.sign(&ut::claims(project, sid, exp_in)).unwrap()
    }

    async fn user_call(h: &DataHarness, key: &str, token: &str) -> Handled {
        let bearer = format!("Bearer {key}");
        run(
            &h.pipeline,
            "POST",
            "/data/v1/query",
            None,
            &[("authorization", &bearer), ("cheladb-user", token)],
        )
        .await
    }

    #[tokio::test]
    async fn identity_carries_project_id() {
        let h = data_harness().await;
        let case = user_case(&h, DataKeyKind::Public);
        let plan = expect_plan(data_call(&h, "POST", "/data/v1/query", Some(&case.key)).await.outcome);
        let claims = h.verify.verify(plan.identity.as_deref().unwrap(), "rest").unwrap();
        let data = claims.data.unwrap();
        assert_eq!(data.project_id, Some(case.project));
        assert_eq!(data.role, "anon");
        assert_eq!(h.sessions.lookups(), 0, "no user header, no liveness call");
    }

    #[tokio::test]
    async fn valid_user_token_mints_authenticated_with_claims() {
        let h = data_harness().await;
        let case = user_case(&h, DataKeyKind::Public);
        h.sessions.set(case.sid, ut::Session::Live(case.project));
        let token = user_token(&h, case.project, case.sid, 300);
        let plan = expect_plan(user_call(&h, &case.key, &token).await.outcome);
        let claims = h.verify.verify(plan.identity.as_deref().unwrap(), "rest").unwrap();
        let data = claims.data.unwrap();
        assert_eq!(data.role, "authenticated");
        assert_eq!(data.kind, DataKeyKind::Public);
        assert_eq!(data.project_id, Some(case.project));
        assert_eq!(data.claims["sid"], case.sid.to_string());
        assert_eq!(data.claims["project"], case.project.to_string());
        assert_eq!(data.claims["email"], "ada@example.com");
        assert_eq!(data.claims["email_verified"], true);
        assert_eq!(data.claims["role"], "authenticated");
        assert!(data.claims["sub"].is_string() && data.claims["exp"].is_u64());
        assert_eq!(h.sessions.bad_bearer.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    /// Review Focus 2: a bad user token is never downgraded to the anon role.
    #[tokio::test]
    async fn user_token_never_falls_back_to_anon() {
        let h = data_harness().await;
        let case = user_case(&h, DataKeyKind::Public);
        let revoked_sid = TestUuid::new_v4();
        h.sessions.set(revoked_sid, ut::Session::Revoked(case.project));
        let expired_session = TestUuid::new_v4();
        h.sessions.set(expired_session, ut::Session::Expired(case.project));
        let cases = [
            ("garbage", "user_token_invalid"),
            ("", "user_token_invalid"),
        ]
        .map(|(t, c)| (t.to_string(), c))
        .into_iter()
        .chain([
            (user_token(&h, case.project, case.sid, -300), "user_token_expired"),
            (user_token(&h, case.project, revoked_sid, 300), "user_session_revoked"),
            (user_token(&h, case.project, expired_session, 300), "user_session_revoked"),
            (user_token(&h, case.project, TestUuid::new_v4(), 300), "user_session_revoked"),
        ]);
        for (token, code) in cases {
            let handled = user_call(&h, &case.key, &token).await;
            let resp = expect_response(handled.outcome);
            assert_eq!((resp.status, error_code(&resp)), (401, Some(code)), "token {token:?}");
        }
    }

    #[tokio::test]
    async fn secret_key_with_user_token_400() {
        let h = data_harness().await;
        let case = user_case(&h, DataKeyKind::Secret);
        h.sessions.set(case.sid, ut::Session::Live(case.project));
        let token = user_token(&h, case.project, case.sid, 300);
        let resp = expect_response(user_call(&h, &case.key, &token).await.outcome);
        assert_eq!((resp.status, error_code(&resp)), (400, Some("user_token_with_secret_key")));
        assert_eq!(h.sessions.lookups(), 0);
    }

    #[tokio::test]
    async fn token_for_another_project_is_invalid() {
        let h = data_harness().await;
        let case = user_case(&h, DataKeyKind::Public);
        let other = TestUuid::new_v4();
        h.sessions.set(case.sid, ut::Session::Live(other));
        let token = user_token(&h, other, case.sid, 300);
        let resp = expect_response(user_call(&h, &case.key, &token).await.outcome);
        assert_eq!((resp.status, error_code(&resp)), (401, Some("user_token_invalid")));
        assert_eq!(h.sessions.lookups(), 0);
    }

    #[tokio::test]
    async fn session_of_another_project_is_revoked() {
        let h = data_harness().await;
        let case = user_case(&h, DataKeyKind::Public);
        h.sessions.set(case.sid, ut::Session::Live(TestUuid::new_v4()));
        let token = user_token(&h, case.project, case.sid, 300);
        let resp = expect_response(user_call(&h, &case.key, &token).await.outcome);
        assert_eq!((resp.status, error_code(&resp)), (401, Some("user_session_revoked")));
    }

    #[tokio::test]
    async fn session_liveness_is_cached_30s() {
        let h = data_harness().await;
        let case = user_case(&h, DataKeyKind::Public);
        h.sessions.set(case.sid, ut::Session::Live(case.project));
        let token = user_token(&h, case.project, case.sid, 300);
        for _ in 0..2 {
            expect_plan(user_call(&h, &case.key, &token).await.outcome);
        }
        assert_eq!(h.sessions.lookups(), 1);
    }

    #[tokio::test]
    async fn revoked_session_401() {
        let h = data_harness().await;
        let case = user_case(&h, DataKeyKind::Public);
        h.sessions.set(case.sid, ut::Session::Revoked(case.project));
        let token = user_token(&h, case.project, case.sid, 300);
        let resp = expect_response(user_call(&h, &case.key, &token).await.outcome);
        assert_eq!((resp.status, error_code(&resp)), (401, Some("user_session_revoked")));
    }

    #[tokio::test]
    async fn liveness_unavailable_503() {
        let h = data_harness().await;
        let case = user_case(&h, DataKeyKind::Public);
        h.sessions.set(case.sid, ut::Session::Error);
        let token = user_token(&h, case.project, case.sid, 300);
        let resp = expect_response(user_call(&h, &case.key, &token).await.outcome);
        assert_eq!((resp.status, error_code(&resp)), (503, Some("unavailable")));
        assert!(header(&resp.headers, "retry-after").is_some());
        // Not cached: recovers once the listener does.
        h.sessions.set(case.sid, ut::Session::Live(case.project));
        expect_plan(user_call(&h, &case.key, &token).await.outcome);
    }

    #[tokio::test]
    async fn cheladb_user_header_is_stripped_upstream() {
        let h = data_harness().await;
        let case = user_case(&h, DataKeyKind::Public);
        h.sessions.set(case.sid, ut::Session::Live(case.project));
        let token = user_token(&h, case.project, case.sid, 300);
        let plan = expect_plan(user_call(&h, &case.key, &token).await.outcome);
        let edits = plan.upstream_headers("rid");
        assert!(edits.remove.contains(&"cheladb-user"));
        assert!(!edits.set.iter().any(|(n, _)| *n == "cheladb-user"));
    }

    #[tokio::test]
    async fn preflight_allows_cheladb_user() {
        let h = data_harness().await;
        let resp = expect_response(
            run(
                &h.pipeline,
                "OPTIONS",
                "/data/v1/query",
                None,
                &[("origin", "https://app.example"), ("access-control-request-method", "POST")],
            )
            .await
            .outcome,
        );
        assert_eq!(
            header(&resp.headers, "access-control-allow-headers"),
            Some("authorization, content-type, cheladb-user")
        );
    }

    /// Routes that take only a public key (`Auth::PublicDataKey`, the app auth routes) mint the
    /// same identity: the project, and `authenticated` once a user token rides along.
    #[tokio::test]
    async fn auth_app_routes_carry_the_identity() {
        let h = data_harness().await;
        let case = user_case(&h, DataKeyKind::Public);
        h.sessions.set(case.sid, ut::Session::Live(case.project));
        let token = user_token(&h, case.project, case.sid, 300);
        let bearer = format!("Bearer {}", case.key);

        let anon = headers(&[("authorization", &bearer)]);
        let parts = |headers| RequestParts { method: "POST", path: "/", host: None, headers, client_ip: None };
        let minted = h
            .pipeline
            .authenticate_key(Service::Auth, &parts(&anon), false, true, "rid")
            .await
            .unwrap();
        let data = h.verify.verify(&minted, "auth").unwrap().data.unwrap();
        assert_eq!((data.project_id, data.role.as_str()), (Some(case.project), "anon"));

        let with_user = headers(&[("authorization", &bearer), ("cheladb-user", &token)]);
        let minted = h
            .pipeline
            .authenticate_key(Service::Auth, &parts(&with_user), false, true, "rid")
            .await
            .unwrap();
        let data = h.verify.verify(&minted, "auth").unwrap().data.unwrap();
        assert_eq!((data.project_id, data.role.as_str()), (Some(case.project), "authenticated"));

        // A secret key is refused on a public-only route before any user token is looked at.
        let secret = user_case(&h, DataKeyKind::Secret);
        let bearer = format!("Bearer {}", secret.key);
        let hs = headers(&[("authorization", &bearer), ("cheladb-user", &token)]);
        let refused = h
            .pipeline
            .authenticate_key(Service::Auth, &parts(&hs), false, true, "rid")
            .await
            .unwrap_err();
        assert_eq!(refused.status, 401);
    }
}
