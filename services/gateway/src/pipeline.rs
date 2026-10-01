//! The fixed request pipeline:
//! request id → scope → route → CORS → auth → internal identity.
//!
//! Pure over request parts; `proxy.rs` adapts it to Pingora. Nothing here is
//! configurable per route except the typed `Policy` from `cheladb-routes`.

use crate::data_keys::{DataKeyResolver, KeyError};
use crate::registry::SharedRegistry;
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
const STRIPPED_REQUEST_HEADERS: &[&str] = &[IDENTITY_HEADER, "authorization"];
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
}

impl Pipeline {
    pub fn new(
        registry: Arc<SharedRegistry>,
        studio_origins: Vec<String>,
        verifier: JwtVerifier,
        signer: InternalSigner,
        keys: Arc<DataKeyResolver>,
    ) -> Self {
        Self {
            registry,
            studio_origins,
            verifier,
            signer,
            keys,
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
            Auth::DataKey | Auth::SecretDataKey => {
                let secret_only = endpoint.policy.auth == Auth::SecretDataKey;
                match self
                    .authenticate_key(endpoint.service, req, secret_only, request_id)
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
                    ("access-control-allow-headers", "authorization, content-type".to_string()),
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
        request_id: &str,
    ) -> Result<String, GatewayResponse> {
        let key = bearer_token(req.headers).ok_or(KeyError::Missing).map_err(key_error)?;
        let resolved = self.keys.resolve(key, req.client_ip).await.map_err(key_error)?;
        if secret_only && resolved.kind != DataKeyKind::Secret {
            return Err(key_error(KeyError::Invalid));
        }
        let identity = Identity {
            sub: resolved.key_id.to_string(),
            org_id: String::new(),
            auth: AuthMethod::DataKey,
            request_id: request_id.to_string(),
            data: Some(DataIdentity {
                branch_id: resolved.branch_id,
                key_id: resolved.key_id,
                kind: resolved.kind,
                role: resolved.role,
                claims: json!({}),
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

fn key_error(err: KeyError) -> GatewayResponse {
    match err {
        KeyError::Missing => data_error(401, "unauthorized", "missing API key: send Authorization: Bearer <key>"),
        KeyError::Invalid => data_error(401, "unauthorized", "invalid, revoked or unauthorized API key"),
        KeyError::RateLimited => data_error(429, "rate_limited", "too many unknown-key lookups; slow down"),
        KeyError::Unavailable => data_error(503, "unavailable", "key lookup is unavailable; retry shortly"),
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
    use cheladb_auth::ZitadelAuthConfig;
    use http::{HeaderName, HeaderValue};

    const STUDIO: &str = "http://localhost:3000";

    struct Harness {
        pipeline: Pipeline,
        jwks: MockJwks,
        verify: InternalVerifier,
    }

    async fn harness(management_hosts: &[&str]) -> Harness {
        let jwks = MockJwks::start().await;
        let verifier = JwtVerifier::new(jwks.config(vec![]));
        let (pipeline, verify) = pipeline_with(management_hosts, verifier);
        Harness {
            pipeline,
            jwks,
            verify,
        }
    }

    fn pipeline_with(management_hosts: &[&str], verifier: JwtVerifier) -> (Pipeline, InternalVerifier) {
        let (private_pem, public_pem) = generate_keypair_pem();
        let pipeline = Pipeline::new(
            Arc::new(SharedRegistry::new(Registry::new(management_hosts))),
            vec![STUDIO.to_string()],
            verifier,
            InternalSigner::from_pem(private_pem.as_bytes()).unwrap(),
            Arc::new(DataKeyResolver::new("http://127.0.0.1:1", "unused")),
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
    async fn unknown_host_is_404_when_management_hosts_are_configured() {
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
    }

    async fn data_harness() -> DataHarness {
        let (url, mock) = dk::start().await;
        let jwks = MockJwks::start().await;
        let (private_pem, public_pem) = generate_keypair_pem();
        let pipeline = Pipeline::new(
            Arc::new(SharedRegistry::new(Registry::new(Vec::<String>::new()))),
            vec![STUDIO.to_string()],
            JwtVerifier::new(jwks.config(vec![])),
            InternalSigner::from_pem(private_pem.as_bytes()).unwrap(),
            Arc::new(DataKeyResolver::new(&url, dk::BEARER)),
        );
        DataHarness {
            pipeline,
            verify: InternalVerifier::from_pem(public_pem.as_bytes()).unwrap(),
            mock,
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
                Some("authorization, content-type")
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
}
