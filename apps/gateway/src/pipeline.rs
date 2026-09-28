//! The fixed request pipeline:
//! request id → scope → route → CORS → auth → internal identity.
//!
//! Pure over request parts; `proxy.rs` adapts it to Pingora. Nothing here is
//! configurable per route except the typed `Policy` from `cheladb-routes`.

use crate::registry::SharedRegistry;
use cheladb_auth::internal::{AuthMethod, Identity, InternalSigner, IDENTITY_HEADER};
use cheladb_auth::{AuthError, JwtVerifier};
use cheladb_routes::{match_endpoint, Auth, Cors, Endpoint, Match, Scope, Service};
use http::HeaderMap;
use serde_json::{json, Value};
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
}

impl Pipeline {
    pub fn new(
        registry: Arc<SharedRegistry>,
        studio_origins: Vec<String>,
        verifier: JwtVerifier,
        signer: InternalSigner,
    ) -> Self {
        Self {
            registry,
            studio_origins,
            verifier,
            signer,
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
            return respond(404, Vec::new(), json!({ "error": "unknown host" }));
        };

        if let Some(preflight) = self.preflight(scope, req) {
            return Outcome::Respond(preflight);
        }

        let endpoint = match match_endpoint(scope, req.method, req.path) {
            Match::Found(endpoint) => endpoint,
            Match::NotFound => {
                return respond(404, Vec::new(), json!({ "error": "no route" }));
            }
            Match::MethodNotAllowed(allowed) => {
                let allow = allowed
                    .iter()
                    .map(|m| m.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return respond(
                    405,
                    vec![("allow", allow)],
                    json!({ "error": "method not allowed" }),
                );
            }
        };

        let cors = self.cors_headers(endpoint, req.headers);

        let identity = match endpoint.policy.auth {
            Auth::Public => None,
            Auth::Platform => {
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
        if endpoint.policy.cors == Cors::None {
            return Vec::new();
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

    /// Verify the platform JWT and mint an internal identity for `service`.
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
                json_response(503, json!({ "error": "auth unavailable" }))
            }
        })?;
        let identity = Identity {
            sub: claims.sub,
            org_id: claims.org_id,
            auth: AuthMethod::PlatformJwt,
            request_id: request_id.to_string(),
        };
        self.signer.sign(service.name(), &identity).map_err(|err| {
            error!("mint internal identity: {err}");
            json_response(500, json!({ "error": "internal error" }))
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

fn unauthorized(err: &AuthError) -> GatewayResponse {
    json_response(401, json!({ "error": err.to_string() }))
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

    async fn harness(platform_hosts: &[&str]) -> Harness {
        let jwks = MockJwks::start().await;
        let verifier = JwtVerifier::new(jwks.config(vec![]));
        let (pipeline, verify) = pipeline_with(platform_hosts, verifier);
        Harness {
            pipeline,
            jwks,
            verify,
        }
    }

    fn pipeline_with(platform_hosts: &[&str], verifier: JwtVerifier) -> (Pipeline, InternalVerifier) {
        let (private_pem, public_pem) = generate_keypair_pem();
        let pipeline = Pipeline::new(
            Arc::new(SharedRegistry::new(Registry::new(platform_hosts))),
            vec![STUDIO.to_string()],
            verifier,
            InternalSigner::from_pem(private_pem.as_bytes()).unwrap(),
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

    #[tokio::test]
    async fn unknown_host_is_404_when_platform_hosts_are_configured() {
        let h = harness(&["api.example.com"]).await;
        let handled = run(&h.pipeline, "GET", "/health", Some("other.example.com"), &[]).await;
        let resp = expect_response(handled.outcome);
        assert_eq!(resp.status, 404);
        assert_eq!(resp.body, Some(json!({ "error": "unknown host" })));
    }

    #[tokio::test]
    async fn undeclared_path_is_404() {
        let h = harness(&[]).await;
        let resp = expect_response(run(&h.pipeline, "GET", "/v1/other", None, &[]).await.outcome);
        assert_eq!(resp.status, 404);
        assert_eq!(resp.body, Some(json!({ "error": "no route" })));
    }

    #[tokio::test]
    async fn wrong_method_is_405_with_allow_header() {
        let h = harness(&[]).await;
        let resp = expect_response(run(&h.pipeline, "DELETE", "/v1/databases", None, &[]).await.outcome);
        assert_eq!(resp.status, 405);
        assert_eq!(header(&resp.headers, "allow"), Some("POST"));
    }

    #[tokio::test]
    async fn public_endpoint_is_proxied_without_identity() {
        let h = harness(&[]).await;
        let plan = expect_plan(run(&h.pipeline, "GET", "/health", None, &[]).await.outcome);
        assert_eq!(plan.endpoint.service, Service::Orchestrator);
        assert_eq!(plan.identity, None);
    }

    #[tokio::test]
    async fn platform_endpoint_without_bearer_is_401() {
        let h = harness(&[]).await;
        let resp = expect_response(run(&h.pipeline, "POST", "/v1/databases", None, &[]).await.outcome);
        assert_eq!(resp.status, 401);
    }

    #[tokio::test]
    async fn platform_endpoint_with_invalid_bearer_is_401() {
        let h = harness(&[]).await;
        let resp = expect_response(
            run(&h.pipeline, "POST", "/v1/databases", None, &[("authorization", "Bearer not-a-jwt")])
                .await
                .outcome,
        );
        assert_eq!(resp.status, 401);
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
        assert_eq!(claims.auth, AuthMethod::PlatformJwt);
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
        assert_eq!(resp.body, Some(json!({ "error": "auth unavailable" })));
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
    async fn upstream_headers_set_the_minted_identity_on_platform_routes() {
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
    async fn ui_auth_allows_credentials_for_studio_only() {
        let h = harness(&[]).await;
        let plan = expect_plan(run(&h.pipeline, "POST", "/auth/v1/sign-in", None, &[("origin", STUDIO)]).await.outcome);
        assert_eq!(plan.endpoint.service, Service::UiAuth);
        assert_eq!(plan.identity, None);
        assert_eq!(header(&plan.response_headers, "access-control-allow-origin"), Some(STUDIO));
        assert_eq!(header(&plan.response_headers, "access-control-allow-credentials"), Some("true"));

        let other = expect_plan(run(&h.pipeline, "POST", "/auth/v1/sign-in", None, &[("origin", "http://evil.example")]).await.outcome);
        assert_eq!(header(&other.response_headers, "access-control-allow-origin"), None);
        assert_eq!(header(&other.response_headers, "access-control-allow-credentials"), None);
    }

    #[tokio::test]
    async fn ui_auth_preflight_allows_credentials() {
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
        );
        let plan = expect_plan(run(&pipeline, "POST", "/auth/v1/sign-in", None, &[("origin", STUDIO)]).await.outcome);
        assert_eq!(header(&plan.response_headers, "access-control-allow-origin"), None);
        assert_eq!(header(&plan.response_headers, "access-control-allow-credentials"), None);
    }
}
