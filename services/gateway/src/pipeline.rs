//! The fixed request pipeline:
//! request id → host → (a site passes through) → route → CORS → auth → internal identity.
//!
//! Pure over request parts; `proxy.rs` adapts it to Pingora. Nothing here is
//! configurable per route except the typed `Policy` from `chelabase-routes`.

use crate::data_keys::{DataKeyResolver, KeyError, ResolvedKey};
use crate::registry::{SharedRegistry, Site, Target};
use crate::user_tokens::{UserTokenError, UserTokens};
use chelabase_auth::internal::{AuthMethod, DataIdentity, Identity, InternalSigner, IDENTITY_HEADER};
use chelabase_auth::member_token::{MemberTokenError, MemberTokenVerifier};
use chelabase_auth::AuthError;
use chelabase_routes::{match_endpoint, Auth, Cors, Endpoint, Match, Scope, Service};
use chelabase_types::data_key::DataKeyKind;
use http::HeaderMap;
use serde_json::{json, Value};
use std::net::IpAddr;
use std::sync::Arc;
use tracing::{debug, error};
use uuid::Uuid;

pub const REQUEST_ID_HEADER: &str = "x-request-id";
/// The gateway's own liveness check; answered before host or route checks.
pub const GATEWAY_HEALTH_PATH: &str = "/_gateway/health";
/// Readiness (#22): 200 once the gateway can check member tokens (its member verify key is
/// configured), else 503. `GATEWAY_HEALTH_PATH` stays liveness.
pub const GATEWAY_READY_PATH: &str = "/_gateway/ready";
const MAX_REQUEST_ID_LEN: usize = 128;
/// Client headers never forwarded upstream: upstreams only ever see the
/// gateway-minted identity.
/// The end-user token header; checked by the gateway, never forwarded.
pub const USER_TOKEN_HEADER: &str = "chelabase-user";
/// An app server's own secret key, sent beside its public key on the app auth routes: with
/// [`CLIENT_IP_HEADER`] it vouches for that address, without a usable one it marks the call
/// server-vouched (auth skips its per-IP limits). Checked by the gateway, never forwarded.
pub const SERVER_KEY_HEADER: &str = "chelabase-server-key";
/// The app user's address as the app's server saw it. Believed only on the app auth routes
/// (`Auth::PublicDataKey`) and only with the branch's secret key in [`SERVER_KEY_HEADER`];
/// never forwarded.
pub const CLIENT_IP_HEADER: &str = "chelabase-client-ip";
/// For the functions service only, set by the gateway after it verified them: the caller's
/// key when it is a public key, and the verified user token. Every other upstream gets
/// neither, and a client's copies are removed everywhere.
pub const FORWARDED_KEY_HEADER: &str = "chelabase-forwarded-key";
pub const FORWARDED_USER_HEADER: &str = "chelabase-forwarded-user";
const STRIPPED_REQUEST_HEADERS: &[&str] = &[
    IDENTITY_HEADER,
    "authorization",
    USER_TOKEN_HEADER,
    SERVER_KEY_HEADER,
    CLIENT_IP_HEADER,
    FORWARDED_KEY_HEADER,
    FORWARDED_USER_HEADER,
];
const CORS_ALLOW_HEADERS: &str = "authorization, content-type, x-request-id, x-chelabase-studio";
/// Any-origin (data key, app user) routes: the request headers `@chelabase/client` sends to the
/// data API and storage (`x-upsert` and `range` are storage's).
const CORS_ANY_ALLOW_HEADERS: &str = "authorization, content-type, chelabase-user, x-upsert, range";
/// Response headers the client reads that aren't CORS-safelisted: an object's `etag`, and a
/// ranged download's `content-range`.
const CORS_ANY_EXPOSE_HEADERS: &str = "etag, content-range";
const CORS_MAX_AGE_SECS: &str = "600";

/// The parts of an incoming request the pipeline looks at.
pub struct RequestParts<'a> {
    pub method: &'a str,
    /// Path only, without the query string.
    pub path: &'a str,
    pub host: Option<&'a str>,
    pub headers: &'a HeaderMap,
    /// The client's address: the downstream socket address, or, when that peer is a trusted
    /// proxy, the client address its `X-Forwarded-For` or `Forwarded` names
    /// ([`crate::client_addr::client_address`]). Per-IP limits key on it.
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
    /// The address `X-Forwarded-For` names upstream: the request's client address, or on an
    /// app auth route the one the app's server vouched for with its secret key.
    pub client_ip: Option<IpAddr>,
    /// [`FORWARDED_KEY_HEADER`] and [`FORWARDED_USER_HEADER`], on a functions call only.
    pub forwarded: Vec<(&'static str, String)>,
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
        set.extend(self.forwarded.iter().cloned());
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
    /// Pass the request through to the site's upstream unchanged (no routes, CORS or auth).
    Site(Site),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Handled {
    pub request_id: String,
    pub outcome: Outcome,
}

pub struct Pipeline {
    registry: Arc<SharedRegistry>,
    /// The one origin Studio CORS allows (`<scheme>://<studio host>`).
    studio_origin: String,
    /// Member tokens signed by auth (#119); `None` refuses every bearer.
    members: Option<MemberTokenVerifier>,
    signer: InternalSigner,
    keys: Arc<DataKeyResolver>,
    users: Arc<UserTokens>,
    /// The last readiness `/_gateway/ready` answered ([`READY_UNKNOWN`] before the first), so
    /// its reason is logged once per change, not per probe.
    ready_seen: std::sync::atomic::AtomicU8,
}

const READY_UNKNOWN: u8 = 0;
const READY_YES: u8 = 1;
const READY_NO: u8 = 2;

impl Pipeline {
    pub fn new(
        registry: Arc<SharedRegistry>,
        studio_origin: String,
        signer: InternalSigner,
        keys: Arc<DataKeyResolver>,
        users: Arc<UserTokens>,
    ) -> Self {
        Self {
            registry,
            studio_origin,
            members: None,
            signer,
            keys,
            users,
            ready_seen: std::sync::atomic::AtomicU8::new(READY_UNKNOWN),
        }
    }

    /// Verify member tokens (`CHELABASE_MEMBER_JWT_PUBLIC_KEY`, #119).
    pub fn with_member_tokens(mut self, members: Option<MemberTokenVerifier>) -> Self {
        self.members = members;
        self
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
        if req.method == "GET" && req.path == GATEWAY_READY_PATH {
            // Answered on any host: the body never says why (a variable, a path, a mode). The
            // reason is logged once each time readiness changes.
            let ready = self.members.is_some();
            let was = self.ready_seen.swap(
                if ready { READY_YES } else { READY_NO },
                std::sync::atomic::Ordering::Relaxed,
            );
            return match ready {
                true => {
                    if was != READY_YES {
                        tracing::info!("the gateway is ready: it verifies member tokens");
                    }
                    respond(200, Vec::new(), json!({ "status": "ok" }))
                }
                false => {
                    if was != READY_NO {
                        tracing::warn!(
                            "the gateway isn't ready: CHELABASE_MEMBER_JWT_PUBLIC_KEY is not set"
                        );
                    }
                    respond(503, Vec::new(), json!({ "status": "waiting" }))
                }
            };
        }

        if req.host.is_none() {
            return empty_response(400);
        }
        match self.registry.load().target_for(req.host) {
            None => return empty_response(404),
            // Studio reads its runtime config from its own origin (it doesn't know the API's
            // yet): that one public GET is routed as on the API host.
            Some(Target::Site(Site::Studio))
                if req.method == "GET" && req.path == chelabase_routes::auth::STUDIO_CONFIG.path => {}
            Some(Target::Site(site)) => return Outcome::Site(site),
            Some(Target::Api) => {}
        }
        let scope = Scope::Api;

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
                    "no such data API endpoint: use POST /data/v1/query, GET /data/v1/schema or GET /data/v1/types",
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

        let mut client_ip = req.client_ip;
        let mut forwarded = Vec::new();
        let identity = match endpoint.policy.auth {
            Auth::Public => None,
            Auth::DataKey | Auth::SecretDataKey | Auth::PublicDataKey => {
                let secret_only = endpoint.policy.auth == Auth::SecretDataKey;
                let public_only = endpoint.policy.auth == Auth::PublicDataKey;
                match self
                    .authenticate_key(endpoint.service, req, secret_only, public_only, request_id)
                    .await
                {
                    Ok((token, vouch, forward)) => {
                        if let Vouch::ClientIp(ip) = vouch {
                            client_ip = Some(ip);
                        }
                        if endpoint.service == Service::Functions {
                            forwarded = forward;
                        }
                        Some(token)
                    }
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
            client_ip,
            forwarded,
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
            // Exactly the matched endpoint's method: the route table, not a hand list.
            return Some(GatewayResponse {
                status: 204,
                headers: vec![
                    ("access-control-allow-origin", "*".to_string()),
                    ("access-control-allow-methods", endpoint.method.as_str().to_string()),
                    ("access-control-allow-headers", CORS_ANY_ALLOW_HEADERS.to_string()),
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
        if self.origin_allowed(origin) {
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
            Cors::Any => {
                return vec![
                    ("access-control-allow-origin", "*".to_string()),
                    ("access-control-expose-headers", CORS_ANY_EXPOSE_HEADERS.to_string()),
                ]
            }
            Cors::Studio | Cors::StudioCredentials => {}
        }
        let credentials = endpoint.policy.cors == Cors::StudioCredentials;
        let mut cors = vec![("vary", "Origin".to_string())];
        if let Some(origin) = header_str(headers, "origin").filter(|o| self.origin_allowed(o)) {
            cors.push(("access-control-allow-origin", origin.to_string()));
            if credentials {
                cors.push(("access-control-allow-credentials", "true".to_string()));
            }
        }
        cors
    }

    /// Studio CORS allows exactly Studio's origin (P8).
    fn origin_allowed(&self, origin: &str) -> bool {
        origin == self.studio_origin
    }

    /// Resolve the data API key from `Authorization: Bearer` and mint an
    /// internal identity for `service`. Errors use the data API error body. On a public-only
    /// route (the app auth routes) it also returns what the app's server vouched for, if
    /// anything ([`Self::server_vouch`]); a server-vouched call is marked in the identity.
    async fn authenticate_key(
        &self,
        service: Service,
        req: &RequestParts<'_>,
        secret_only: bool,
        public_only: bool,
        request_id: &str,
    ) -> Result<(String, Vouch, Vec<(&'static str, String)>), GatewayResponse> {
        let key = bearer_token(req.headers).ok_or(KeyError::Missing).map_err(key_error)?;
        let resolved = self.keys.resolve(key, req.client_ip).await.map_err(key_error)?;
        if let Some(refused) = key_kind_refusal(secret_only, public_only, resolved.kind) {
            return Err(refused);
        }
        // The key's role OID binds the request to the role's identity; a user token runs as
        // `authenticated`, which isn't the key's role, so it carries none.
        let (role, claims, role_oid) = match header_str(req.headers, USER_TOKEN_HEADER) {
            None if req.headers.contains_key(USER_TOKEN_HEADER) => {
                return Err(user_token_error(UserTokenError::Invalid));
            }
            None => (resolved.role.clone(), json!({}), resolved.role_oid),
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
                    None,
                )
            }
        };
        let vouch = if public_only {
            self.server_vouch(req, &resolved).await
        } else {
            Vouch::None
        };
        // What a functions call may hand its function (the caller's own public key and verified
        // token, so the function can call the data API as the caller); the caller keeps the
        // list for `Service::Functions` only. A secret key is never in it.
        let mut forward = Vec::new();
        if resolved.kind == DataKeyKind::Public {
            forward.push((FORWARDED_KEY_HEADER, key.to_string()));
            if let Some(token) = header_str(req.headers, USER_TOKEN_HEADER) {
                forward.push((FORWARDED_USER_HEADER, token.to_string()));
            }
        }
        let identity = Identity {
            sub: resolved.key_id.to_string(),
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
                role_oid,
                server_vouched: vouch == Vouch::Server,
            }),
            email: None,
            name: None,
        };
        let token = self.signer.sign(service.name(), &identity).map_err(|err| {
            error!("mint internal identity: {err}");
            internal_key_error(request_id)
        })?;
        Ok((token, vouch, forward))
    }

    /// What the app's server vouches for when [`SERVER_KEY_HEADER`] holds the secret key of the
    /// public key's branch: the address in [`CLIENT_IP_HEADER`] when that is one IP literal,
    /// else the call itself ([`Vouch::Server`]; the malformed or absent address is not used).
    /// Anything less is [`Vouch::None`] (the request keeps the address the gateway settled on).
    /// Never an error: a misconfigured app server degrades to its own address. The server key
    /// is looked up whenever it is sent, and its lookup is charged to the real client
    /// address's miss limit. Logs never hold either header's value.
    async fn server_vouch(&self, req: &RequestParts<'_>, public: &ResolvedKey) -> Vouch {
        if !req.headers.contains_key(SERVER_KEY_HEADER) {
            if req.headers.contains_key(CLIENT_IP_HEADER) {
                debug!("ignoring chelabase-client-ip: no chelabase-server-key");
            }
            return Vouch::None;
        }
        let Some(server_key) = single_header(req.headers, SERVER_KEY_HEADER) else {
            debug!("ignoring chelabase-server-key: not one header line");
            return Vouch::None;
        };
        match self.keys.resolve(server_key, req.client_ip).await {
            Ok(server) if server_key_vouches(public, &server) => match asserted_client_ip(req.headers) {
                Some(ip) => Vouch::ClientIp(ip),
                None => {
                    if req.headers.contains_key(CLIENT_IP_HEADER) {
                        debug!("ignoring chelabase-client-ip: not one IP address; the call is server-vouched");
                    }
                    Vouch::Server
                }
            },
            Ok(_) => {
                debug!("ignoring chelabase-server-key: not this branch's secret key");
                Vouch::None
            }
            Err(err) => {
                debug!(?err, "ignoring chelabase-server-key: it didn't resolve");
                Vouch::None
            }
        }
    }

    /// Verify the management token and mint an internal identity for `service`.
    ///
    /// #119: every bearer is a member token and only the member verifier decides it: any
    /// refusal there is 401 (no verify key configured: every bearer is refused). A member names
    /// its tenant (`tid`); the orchestrator still requires an active membership there.
    async fn authenticate(
        &self,
        service: Service,
        headers: &HeaderMap,
        request_id: &str,
    ) -> Result<String, GatewayResponse> {
        let token = bearer_token(headers).ok_or_else(|| unauthorized(&AuthError::Unauthorized))?;
        let Some(members) = &self.members else {
            return Err(unauthorized(&AuthError::InvalidToken("member tokens aren't accepted here".into())));
        };
        let claims = members.verify(token).map_err(|err| match err {
            MemberTokenError::Expired => unauthorized(&AuthError::Expired),
            MemberTokenError::Invalid => unauthorized(&AuthError::InvalidToken("not a valid member token".into())),
        })?;
        let identity = Identity {
            sub: claims.sub,
            auth: AuthMethod::ManagementJwt,
            request_id: request_id.to_string(),
            data: None,
            email: Some(claims.email).filter(|e| !e.is_empty()),
            name: Some(claims.name).filter(|n| !n.is_empty()),
        };
        self.signer.sign_for_tenant(service.name(), &identity, claims.tid, &claims.sid).map_err(|err| {
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

/// A bodiless answer (no `Host`, an unknown host): nothing that names the gateway or its hosts.
fn empty_response(status: u16) -> Outcome {
    Outcome::Respond(GatewayResponse {
        status,
        headers: Vec::new(),
        body: None,
    })
}

fn json_response(status: u16, body: Value) -> GatewayResponse {
    GatewayResponse {
        status,
        headers: Vec::new(),
        body: Some(body),
    }
}

/// The shared error body, `{"error":{"code","message"}}`, written to match `chelabase_types`'
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

/// What an app's server vouched for with the branch's secret key ([`Pipeline::server_vouch`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Vouch {
    /// Nothing: no valid server key, or not an app auth route.
    None,
    /// The user's address: auth rate-limits it per IP (#49).
    ClientIp(IpAddr),
    /// The call, without a usable address: auth skips its per-IP limits (#55).
    Server,
}

/// [`CLIENT_IP_HEADER`] when it is exactly one header line holding one IPv4 or IPv6 literal:
/// no port, brackets, zone, list or whitespace. An IPv4-mapped address comes back as IPv4.
fn asserted_client_ip(headers: &HeaderMap) -> Option<IpAddr> {
    let ip: IpAddr = single_header(headers, CLIENT_IP_HEADER)?.parse().ok()?;
    Some(ip.to_canonical())
}

/// Whether the server key `server` vouches for a request made with the public key `public`:
/// a secret key of the same project and branch. (The resolver never answers a key revoked
/// before it was cached; one revoked later keeps vouching until its 30 s cache entry expires,
/// as it keeps working on the data API.)
fn server_key_vouches(public: &ResolvedKey, server: &ResolvedKey) -> bool {
    public.kind == DataKeyKind::Public
        && server.kind == DataKeyKind::Secret
        && server.project_id == public.project_id
        && server.branch_id == public.branch_id
}

/// `name`'s value when the request has exactly one line of it, in visible ASCII.
fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    value.to_str().ok()
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
    use crate::registry::{Registry, Site};
    use chelabase_auth::internal::{generate_keypair_pem, InternalVerifier, IDENTITY_HEADER};
    use chelabase_auth::test_support::MemberKeys;
    use crate::user_tokens::test_support as ut;
    use chelabase_auth::app_token::AppTokenSigner;
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

    const STUDIO: &str = "http://studio.test";
    const API: &str = "api.test";

    struct Harness {
        pipeline: Pipeline,
        keys: MemberKeys,
        verify: InternalVerifier,
    }

    /// The tenant every harness token names.
    const TENANT: Uuid = Uuid::from_u128(0x7e7e);

    async fn harness(api_hosts: &[&str]) -> Harness {
        let keys = MemberKeys::new();
        let (pipeline, verify) = pipeline_with(api_hosts, Some(&keys));
        Harness {
            pipeline,
            keys,
            verify,
        }
    }

    fn pipeline_with(api_hosts: &[&str], keys: Option<&MemberKeys>) -> (Pipeline, InternalVerifier) {
        pipeline_with_registry(Registry::new(api_hosts), keys)
    }

    fn pipeline_with_registry(registry: Registry, keys: Option<&MemberKeys>) -> (Pipeline, InternalVerifier) {
        let (private_pem, public_pem) = generate_keypair_pem();
        let pipeline = Pipeline::new(
            Arc::new(SharedRegistry::new(registry)),
            STUDIO.to_string(),
            InternalSigner::from_pem(private_pem.as_bytes()).unwrap(),
            Arc::new(DataKeyResolver::new("http://127.0.0.1:1", "unused")),
            Arc::new(UserTokens::new(None, "http://127.0.0.1:1", "unused")),
        )
        .with_member_tokens(keys.map(MemberKeys::verifier));
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

    /// `host: None` means the API host; [`run_without_host`] sends none.
    async fn run(pipeline: &Pipeline, method: &str, path: &str, host: Option<&str>, pairs: &[(&str, &str)]) -> Handled {
        let headers = headers(pairs);
        pipeline
            .handle(&RequestParts {
                method,
                path,
                host: host.or(Some(API)),
                headers: &headers,
                client_ip: None,
            })
            .await
    }

    async fn run_without_host(pipeline: &Pipeline, method: &str, path: &str) -> Handled {
        let headers = headers(&[]);
        pipeline
            .handle(&RequestParts { method, path, host: None, headers: &headers, client_ip: None })
            .await
    }

    fn expect_response(outcome: Outcome) -> GatewayResponse {
        match outcome {
            Outcome::Respond(resp) => resp,
            Outcome::Proxy(plan) => panic!("expected a gateway response, got proxy plan {plan:?}"),
            Outcome::Site(site) => panic!("expected a gateway response, got site {site:?}"),
        }
    }

    fn expect_plan(outcome: Outcome) -> ProxyPlan {
        match outcome {
            Outcome::Proxy(plan) => plan,
            Outcome::Respond(resp) => panic!("expected a proxy plan, got response {resp:?}"),
            Outcome::Site(site) => panic!("expected a proxy plan, got site {site:?}"),
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

    // #22, #126: `/_gateway/ready` is 503 until the gateway can verify member tokens (and says
    // only that, on any host); without the key every bearer is 401, never proxied.
    #[tokio::test]
    async fn ready_once_member_tokens_can_be_verified() {
        let keys = MemberKeys::new();
        let (without, _) = pipeline_with(&[API], None);
        for host in [None, Some("anyone.example")] {
            let resp = expect_response(run(&without, "GET", GATEWAY_READY_PATH, host, &[]).await.outcome);
            assert_eq!((resp.status, resp.body), (503, Some(json!({ "status": "waiting" }))));
        }
        let bearer = format!("Bearer {}", keys.token("user-1", TENANT, 300));
        let resp = expect_response(
            run(&without, "POST", "/v1/databases", None, &[("authorization", &bearer)]).await.outcome,
        );
        assert_eq!(resp.status, 401);
        let (with, _) = pipeline_with(&[API], Some(&keys));
        let resp = expect_response(run(&with, "GET", GATEWAY_READY_PATH, None, &[]).await.outcome);
        assert_eq!(resp.status, 200);
        let plan = expect_plan(run(&with, "POST", "/v1/databases", None, &[("authorization", &bearer)]).await.outcome);
        assert_eq!(plan.endpoint.service, Service::Orchestrator);
    }

    // #126: every bearer goes to the member verifier: an RS256 token (the old provider's
    // shape), another key's member token or garbage is 401, never a fall-through.
    #[tokio::test]
    async fn only_member_tokens_are_accepted() {
        let h = harness(&[API]).await;
        let other = MemberKeys::new();
        let rs256 = "eyJhbGciOiJSUzI1NiIsImtpZCI6ImsifQ.eyJzdWIiOiJ1c2VyLTEifQ.c2ln";
        for token in [rs256.to_string(), other.token("user-1", TENANT, 300), "not-a-jwt".to_string()] {
            let bearer = format!("Bearer {token}");
            let resp = expect_response(
                run(&h.pipeline, "POST", "/v1/databases", None, &[("authorization", &bearer)]).await.outcome,
            );
            assert_eq!(resp.status, 401, "{token}");
            assert_eq!(error_code(&resp), Some("unauthorized"));
        }
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
    async fn an_unknown_host_is_404_with_no_body() {
        let h = harness(&[API]).await;
        let resp = expect_response(run(&h.pipeline, "GET", "/health", Some("other.example.com"), &[]).await.outcome);
        assert_eq!(resp.status, 404);
        assert_eq!(resp.body, None);
    }

    #[tokio::test]
    async fn a_request_without_a_host_is_400() {
        let h = harness(&[API]).await;
        let resp = expect_response(run_without_host(&h.pipeline, "GET", "/health").await.outcome);
        assert_eq!(resp.status, 400);
    }

    async fn site_harness() -> Harness {
        let keys = MemberKeys::new();
        let registry = Registry::new([API])
            .with_site("studio.test", Site::Studio)
            .with_site("docs.test:8000", Site::Docs);
        let (pipeline, verify) = pipeline_with_registry(registry, Some(&keys));
        Harness { pipeline, keys, verify }
    }

    // Selfhost-install m5: Studio reads its runtime config from its own origin before it knows
    // the API's, so `GET /auth/v1/studio-config` on Studio's host goes to auth like on the API
    // host. Only that one public GET: everything else there is Studio's.
    #[tokio::test]
    async fn studio_config_is_served_on_studio_host() {
        let h = site_harness().await;
        for host in ["studio.test", "studio.test:8000"] {
            let plan = expect_plan(run(&h.pipeline, "GET", "/auth/v1/studio-config", Some(host), &[]).await.outcome);
            assert_eq!(plan.endpoint.service, Service::Auth, "{host}");
            assert_eq!(plan.endpoint.path, "/auth/v1/studio-config");
            assert_eq!(plan.identity, None);
            for (method, path) in [
                ("POST", "/auth/v1/studio-config"),
                ("GET", "/auth/v1/instance"),
                ("POST", "/auth/v1/sign-in"),
                ("GET", "/auth/v1/studio-config/x"),
            ] {
                let handled = run(&h.pipeline, method, path, Some(host), &[]).await;
                assert!(matches!(handled.outcome, Outcome::Site(Site::Studio)), "{host} {method} {path}");
            }
        }
        // Not on another site's host.
        let handled = run(&h.pipeline, "GET", "/auth/v1/studio-config", Some("docs.test:8000"), &[]).await;
        assert!(matches!(handled.outcome, Outcome::Site(Site::Docs)));
    }

    #[tokio::test]
    async fn site_hosts_pass_through_before_routes_auth_and_cors() {
        let h = site_harness().await;
        for (host, site) in [("studio.test", Site::Studio), ("studio.test:8000", Site::Studio), ("docs.test:8000", Site::Docs)] {
            for method in ["GET", "POST", "OPTIONS"] {
                let handled = run(
                    &h.pipeline,
                    method,
                    "/v1/no-such-route",
                    Some(host),
                    &[("authorization", "Bearer not-a-jwt"), ("origin", STUDIO), ("access-control-request-method", "GET")],
                )
                .await;
                assert!(matches!(handled.outcome, Outcome::Site(s) if s == site), "{host} {method}");
            }
        }
        let resp = expect_response(run(&h.pipeline, "GET", "/", Some("docs.test:9000"), &[]).await.outcome);
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn undeclared_path_is_404() {
        let h = harness(&[API]).await;
        let resp = expect_response(run(&h.pipeline, "GET", "/v1/other", None, &[]).await.outcome);
        assert_eq!(resp.status, 404);
        assert_eq!(resp.body, Some(shape("not_found", "no route")));
    }

    #[tokio::test]
    async fn wrong_method_is_405_with_allow_header() {
        let h = harness(&[API]).await;
        let resp = expect_response(run(&h.pipeline, "DELETE", "/v1/databases", None, &[]).await.outcome);
        assert_eq!(resp.status, 405);
        assert_eq!(header(&resp.headers, "allow"), Some("POST"));
        assert_eq!(resp.body, Some(shape("method_not_allowed", "method not allowed")));
    }

    /// Under /data/ the gateway's own 404 and 405 use the data API's error body.
    #[tokio::test]
    async fn data_routes_answer_404_and_405_with_the_data_api_body() {
        let h = harness(&[API]).await;
        let resp = expect_response(run(&h.pipeline, "POST", "/data/v1/nope", None, &[]).await.outcome);
        assert_eq!(resp.status, 404);
        let body = resp.body.unwrap();
        assert_eq!(body["error"]["code"], "invalid_query");
        // The hint names every data API route a client can call.
        let message = body["error"]["message"].as_str().unwrap();
        for route in ["POST /data/v1/query", "GET /data/v1/schema", "GET /data/v1/types"] {
            assert!(message.contains(route), "{route}: {body}");
        }
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
        let h = harness(&[API]).await;
        let plan = expect_plan(run(&h.pipeline, "GET", "/health", None, &[]).await.outcome);
        assert_eq!(plan.endpoint.service, Service::Orchestrator);
        assert_eq!(plan.identity, None);
    }

    #[tokio::test]
    async fn management_endpoint_without_bearer_is_401() {
        let h = harness(&[API]).await;
        let resp = expect_response(run(&h.pipeline, "POST", "/v1/databases", None, &[]).await.outcome);
        assert_eq!(resp.status, 401);
        assert_eq!(error_code(&resp), Some("unauthorized"));
    }

    #[tokio::test]
    async fn management_endpoint_with_invalid_bearer_is_401() {
        let h = harness(&[API]).await;
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
        let h = harness(&[API]).await;
        let token = h.keys.token("user-1", TENANT, 300);
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
        assert_eq!(claims.tenant_id, Some(TENANT));
        assert_eq!(claims.rid, "rid-42");
        assert_eq!(claims.auth, AuthMethod::ManagementJwt);
    }

    #[tokio::test]
    async fn identity_carries_the_tokens_email_and_name() {
        let h = harness(&[API]).await;
        let mut claims = h.keys.claims("user-1", TENANT, 300);
        claims.email = "bob@x.com".into();
        claims.name = "Bob".into();
        let token = h.keys.sign(&claims);
        let bearer = format!("Bearer {token}");
        let plan = expect_plan(
            run(&h.pipeline, "GET", "/v1/me", None, &[("authorization", &bearer)])
                .await
                .outcome,
        );
        let claims = h.verify.verify(plan.identity.as_deref().unwrap(), "orchestrator").unwrap();
        assert_eq!(claims.email.as_deref(), Some("bob@x.com"));
        assert_eq!(claims.name.as_deref(), Some("Bob"));

        // With empty ones, the identity carries none.
        let mut claims = h.keys.claims("user-1", TENANT, 300);
        claims.email = String::new();
        claims.name = String::new();
        let bare = format!("Bearer {}", h.keys.sign(&claims));
        let plan = expect_plan(
            run(&h.pipeline, "GET", "/v1/me", None, &[("authorization", &bare)])
                .await
                .outcome,
        );
        let claims = h.verify.verify(plan.identity.as_deref().unwrap(), "orchestrator").unwrap();
        assert_eq!((claims.email, claims.name), (None, None));
    }

    #[tokio::test]
    async fn lowercase_bearer_scheme_is_accepted() {
        let h = harness(&[API]).await;
        let token = h.keys.token("user-1", TENANT, 300);
        let bearer = format!("bearer {token}");
        let plan = expect_plan(
            run(&h.pipeline, "POST", "/v1/databases", None, &[("authorization", &bearer)])
                .await
                .outcome,
        );
        assert!(plan.identity.is_some());
    }

    #[tokio::test]
    async fn preflight_for_studio_endpoint_is_answered_with_cors_headers() {
        let h = harness(&[API]).await;
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
        // #150: Studio's allow-list is its own, untouched by the any-origin class.
        assert_eq!(
            header(&resp.headers, "access-control-allow-headers"),
            Some("authorization, content-type, x-request-id, x-chelabase-studio")
        );
        assert_eq!(header(&resp.headers, "access-control-expose-headers"), None);
    }

    #[tokio::test]
    async fn preflight_from_disallowed_origin_gets_no_cors_headers() {
        let h = harness(&[API]).await;
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
        let h = harness(&[API]).await;
        let resp = expect_response(run(&h.pipeline, "OPTIONS", "/v1/databases", None, &[]).await.outcome);
        assert_eq!(resp.status, 405);
    }

    #[tokio::test]
    async fn preflight_for_non_cors_endpoint_is_405() {
        let h = harness(&[API]).await;
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
        let h = harness(&[API]).await;
        let bearer = format!("Bearer {}", h.keys.token("user-1", TENANT, 300));
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
        let h = harness(&[API]).await;
        let bearer = format!("Bearer {}", h.keys.token("user-1", TENANT, 300));
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
        let h = harness(&[API]).await;
        let bearer = format!("Bearer {}", h.keys.token("user-1", TENANT, 300));
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
        let h = harness(&[API]).await;
        let plan = expect_plan(run(&h.pipeline, "GET", "/health", None, &[("origin", STUDIO)]).await.outcome);
        assert!(plan.response_headers.is_empty());
    }

    #[tokio::test]
    async fn upstream_headers_strip_credentials_even_on_public_routes() {
        let h = harness(&[API]).await;
        let plan = expect_plan(run(&h.pipeline, "GET", "/health", None, &[]).await.outcome);
        let edits = plan.upstream_headers("rid-1");
        assert!(edits.remove.contains(&IDENTITY_HEADER));
        assert!(edits.remove.contains(&"authorization"));
        assert_eq!(edits.set, vec![(REQUEST_ID_HEADER, "rid-1".to_string())]);
    }

    #[tokio::test]
    async fn upstream_headers_set_the_minted_identity_on_management_routes() {
        let h = harness(&[API]).await;
        let bearer = format!("Bearer {}", h.keys.token("user-1", TENANT, 300));
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
        let h = harness(&[API]).await;
        let resp = expect_response(
            run(&h.pipeline, "POST", "/v1/databases", None, &[("origin", STUDIO)]).await.outcome,
        );
        assert_eq!(resp.status, 401);
        assert_eq!(header(&resp.headers, "access-control-allow-origin"), Some(STUDIO));
    }

    #[tokio::test]
    async fn auth_allows_credentials_for_studio_only() {
        let h = harness(&[API]).await;
        let plan = expect_plan(run(&h.pipeline, "POST", "/auth/v1/sign-in", None, &[("origin", STUDIO)]).await.outcome);
        assert_eq!(plan.endpoint.service, Service::Auth);
        assert_eq!(plan.identity, None);
        assert_eq!(header(&plan.response_headers, "access-control-allow-origin"), Some(STUDIO));
        assert_eq!(header(&plan.response_headers, "access-control-allow-credentials"), Some("true"));
        assert_eq!(header(&plan.response_headers, "access-control-expose-headers"), None);

        let other = expect_plan(run(&h.pipeline, "POST", "/auth/v1/sign-in", None, &[("origin", "http://evil.example")]).await.outcome);
        assert_eq!(header(&other.response_headers, "access-control-allow-origin"), None);
        assert_eq!(header(&other.response_headers, "access-control-allow-credentials"), None);
    }

    #[tokio::test]
    async fn auth_preflight_allows_credentials() {
        let h = harness(&[API]).await;
        let resp = expect_response(run(&h.pipeline, "OPTIONS", "/auth/v1/sign-in", None,
            &[("origin", STUDIO), ("access-control-request-method", "POST")]).await.outcome);
        assert_eq!(resp.status, 204);
        assert_eq!(header(&resp.headers, "access-control-allow-credentials"), Some("true"));
        assert_eq!(header(&resp.headers, "access-control-allow-methods"), Some("POST"));
        assert_eq!(
            header(&resp.headers, "access-control-allow-headers"),
            Some("authorization, content-type, x-request-id, x-chelabase-studio")
        );
        assert_eq!(header(&resp.headers, "access-control-expose-headers"), None);
    }

    /// auth-replacement m8: Studio's `/token` call carries `X-Chelabase-Studio`, which auth
    /// requires as its CSRF guard, so the preflight must allow it.
    #[tokio::test]
    async fn token_preflight_allows_the_studio_header() {
        let h = harness(&[API]).await;
        let resp = expect_response(run(&h.pipeline, "OPTIONS", "/auth/v1/token", None,
            &[("origin", STUDIO), ("access-control-request-method", "POST"),
              ("access-control-request-headers", "x-chelabase-studio")]).await.outcome);
        assert_eq!(resp.status, 204);
        let allowed = header(&resp.headers, "access-control-allow-headers").unwrap_or_default();
        assert!(allowed.split(", ").any(|h| h == "x-chelabase-studio"), "{allowed}");
    }

    // ---- data API keys (Task 4) ----

    use crate::data_keys::test_support::{self as dk, Answer};
    use chelabase_types::data_key::DataKeyKind;

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
        let (private_pem, public_pem) = generate_keypair_pem();
        let pipeline = Pipeline::new(
            Arc::new(SharedRegistry::new(Registry::new([API]))),
            STUDIO.to_string(),
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
        assert_eq!(claims.tenant_id, None);
        assert_eq!(claims.rid, rid);
        let data = claims.data.expect("data identity");
        assert_eq!(data.key_id, resolved.key_id);
        assert_eq!(data.branch_id, resolved.branch_id);
        assert_eq!(data.kind, DataKeyKind::Public);
        assert_eq!(data.role, "anon");
        assert_eq!(data.claims, json!({}));
    }

    /// A secret key's role OID rides in the minted identity, so rest runs the key only as the
    /// role with that identity; a key without one (public, or not backfilled) mints none.
    #[tokio::test]
    async fn the_role_oid_is_carried_in_the_minted_token() {
        let h = data_harness().await;
        let bound = dk::new_key(DataKeyKind::Secret);
        let mut resolved = dk::resolved(DataKeyKind::Secret);
        resolved.role_oid = Some(16390);
        h.mock.set(&bound, Answer::Known(resolved));
        let legacy = dk::new_key(DataKeyKind::Secret);
        h.mock.set(&legacy, Answer::Known(dk::resolved(DataKeyKind::Secret)));
        for (key, oid) in [(&bound, Some(16390)), (&legacy, None)] {
            let plan = expect_plan(data_call(&h, "POST", "/data/v1/query", Some(key)).await.outcome);
            let claims = h.verify.verify(plan.identity.as_deref().unwrap(), "rest").unwrap();
            assert_eq!(claims.data.unwrap().role_oid, oid);
        }
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
                    host: Some(API),
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

    /// #150: an any-origin preflight allows the method of the endpoint it matched, and only
    /// that one; a method no endpoint serves on the path gets no CORS grant.
    #[tokio::test]
    async fn cors_any_preflight_allows_the_matched_method() {
        let h = data_harness().await;
        for (path, method) in [
            ("/storage/v1/object/upload/sign/files/a%20b.png", "PUT"),
            ("/storage/v1/object/upload/sign/files/a.png", "POST"),
            ("/storage/v1/upload/session/s1/1", "PUT"),
            ("/storage/v1/upload/session/s1", "DELETE"),
            ("/storage/v1/object/files/a.txt", "PUT"),
            ("/storage/v1/object/files/a.txt", "DELETE"),
            ("/storage/v1/object/files/a.txt", "HEAD"),
            ("/storage/v1/bucket/files", "PATCH"),
            ("/storage/v1/object/public/main/files/a.txt", "GET"),
            ("/data/v1/schema", "GET"),
            ("/data/v1/query", "POST"),
        ] {
            let resp = expect_response(
                run(
                    &h.pipeline,
                    "OPTIONS",
                    path,
                    None,
                    &[("origin", "https://app.example"), ("access-control-request-method", method)],
                )
                .await
                .outcome,
            );
            assert_eq!(resp.status, 204, "{method} {path}");
            assert_eq!(header(&resp.headers, "access-control-allow-origin"), Some("*"), "{method} {path}");
            assert_eq!(header(&resp.headers, "access-control-allow-methods"), Some(method), "{method} {path}");
            assert_eq!(header(&resp.headers, "access-control-allow-credentials"), None);
        }
        // No grant for a method no endpoint serves on the path.
        for (path, method) in [
            ("/data/v1/schema", "PUT"),
            ("/storage/v1/bucket", "PUT"),
            ("/storage/v1/bucket", "DELETE"),
            ("/storage/v1/upload/session/s1", "PUT"),
        ] {
            let resp = expect_response(
                run(
                    &h.pipeline,
                    "OPTIONS",
                    path,
                    None,
                    &[("origin", "https://app.example"), ("access-control-request-method", method)],
                )
                .await
                .outcome,
            );
            assert_ne!(resp.status, 204, "{method} {path}");
            assert_eq!(header(&resp.headers, "access-control-allow-origin"), None, "{method} {path}");
            assert_eq!(header(&resp.headers, "access-control-allow-methods"), None, "{method} {path}");
        }
        assert_eq!(h.mock.lookups(), 0, "preflight does no auth");
    }

    /// #150: the headers the data API and storage clients send are allowed; the client reads
    /// `etag` (and a ranged download's `content-range`), so responses expose them.
    #[tokio::test]
    async fn cors_any_allows_the_clients_headers_and_exposes_what_they_read() {
        let h = data_harness().await;
        for (path, method) in [("/data/v1/query", "POST"), ("/storage/v1/object/files/a.txt", "PUT")] {
            let resp = expect_response(
                run(
                    &h.pipeline,
                    "OPTIONS",
                    path,
                    None,
                    &[
                        ("origin", "https://app.example"),
                        ("access-control-request-method", method),
                        ("access-control-request-headers", "authorization, content-type, chelabase-user, x-upsert, range"),
                    ],
                )
                .await
                .outcome,
            );
            assert_eq!(
                header(&resp.headers, "access-control-allow-headers"),
                Some("authorization, content-type, chelabase-user, x-upsert, range"),
                "{path}"
            );
        }
        let key = dk::new_key(DataKeyKind::Public);
        h.mock.set(&key, Answer::Known(dk::resolved(DataKeyKind::Public)));
        let bearer = format!("Bearer {key}");
        let plan = expect_plan(
            run(
                &h.pipeline,
                "HEAD",
                "/storage/v1/object/files/a.txt",
                None,
                &[("authorization", &bearer), ("origin", "https://app.example")],
            )
            .await
            .outcome,
        );
        assert_eq!(header(&plan.response_headers, "access-control-allow-origin"), Some("*"));
        assert_eq!(header(&plan.response_headers, "access-control-expose-headers"), Some("etag, content-range"));
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
            assert_eq!(header(&resp.headers, "access-control-allow-methods"), Some(method));
            assert_eq!(
                header(&resp.headers, "access-control-allow-headers"),
                Some("authorization, content-type, chelabase-user, x-upsert, range")
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
            &[("authorization", &bearer), ("chelabase-user", token)],
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
    async fn chelabase_user_header_is_stripped_upstream() {
        let h = data_harness().await;
        let case = user_case(&h, DataKeyKind::Public);
        h.sessions.set(case.sid, ut::Session::Live(case.project));
        let token = user_token(&h, case.project, case.sid, 300);
        let plan = expect_plan(user_call(&h, &case.key, &token).await.outcome);
        let edits = plan.upstream_headers("rid");
        assert!(edits.remove.contains(&"chelabase-user"));
        assert!(!edits.set.iter().any(|(n, _)| *n == "chelabase-user"));
    }

    #[tokio::test]
    async fn preflight_allows_chelabase_user() {
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
        let allowed = header(&resp.headers, "access-control-allow-headers").unwrap_or_default();
        assert!(allowed.split(", ").any(|h| h == "chelabase-user"), "{allowed}");
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
            .unwrap()
            .0;
        let data = h.verify.verify(&minted, "auth").unwrap().data.unwrap();
        assert_eq!((data.project_id, data.role.as_str()), (Some(case.project), "anon"));

        let with_user = headers(&[("authorization", &bearer), ("chelabase-user", &token)]);
        let minted = h
            .pipeline
            .authenticate_key(Service::Auth, &parts(&with_user), false, true, "rid")
            .await
            .unwrap()
            .0;
        let data = h.verify.verify(&minted, "auth").unwrap().data.unwrap();
        assert_eq!((data.project_id, data.role.as_str()), (Some(case.project), "authenticated"));

        // A secret key is refused on a public-only route before any user token is looked at.
        let secret = user_case(&h, DataKeyKind::Secret);
        let bearer = format!("Bearer {}", secret.key);
        let hs = headers(&[("authorization", &bearer), ("chelabase-user", &token)]);
        let refused = h
            .pipeline
            .authenticate_key(Service::Auth, &parts(&hs), false, true, "rid")
            .await
            .unwrap_err();
        assert_eq!(refused.status, 401);
    }

    // ---- an app server vouching for its user's address (#49) ----

    const PEER: &str = "198.51.100.7";

    fn peer() -> Option<IpAddr> {
        Some(PEER.parse().unwrap())
    }

    /// A public key and the secret key of the same branch, both known to the mock.
    struct AppKeys {
        public: String,
        resolved: ResolvedKey,
        secret: String,
    }

    fn app_keys(h: &DataHarness) -> AppKeys {
        let public = dk::new_key(DataKeyKind::Public);
        let resolved = dk::resolved(DataKeyKind::Public);
        h.mock.set(&public, Answer::Known(resolved.clone()));
        let secret = dk::new_key(DataKeyKind::Secret);
        h.mock.set(&secret, Answer::Known(secret_of(&resolved)));
        AppKeys { public, resolved, secret }
    }

    /// A secret key of `public`'s branch.
    fn secret_of(public: &ResolvedKey) -> ResolvedKey {
        ResolvedKey {
            key_id: TestUuid::new_v4(),
            kind: DataKeyKind::Secret,
            role: "service".to_string(),
            ..public.clone()
        }
    }

    /// A request from [`PEER`] with `public` as its bearer key plus `extra` headers.
    async fn app_call(h: &DataHarness, path: &str, public: &str, extra: &[(&str, &str)]) -> Outcome {
        let bearer = format!("Bearer {public}");
        let mut pairs = vec![("authorization", bearer.as_str())];
        pairs.extend_from_slice(extra);
        let headers = headers(&pairs);
        h.pipeline
            .handle(&RequestParts { method: "POST", path, host: Some(API), headers: &headers, client_ip: peer() })
            .await
            .outcome
    }

    const SIGN_IN: &str = "/auth/v1/app/sign-in";

    #[test]
    fn the_client_ip_header_is_one_ip_literal() {
        let ip = |raw: &str| asserted_client_ip(&headers(&[(CLIENT_IP_HEADER, raw)]));
        assert_eq!(ip("203.0.113.9"), Some("203.0.113.9".parse().unwrap()));
        assert_eq!(ip("2001:db8::1"), Some("2001:db8::1".parse().unwrap()));
        assert_eq!(ip("::ffff:203.0.113.9"), Some("203.0.113.9".parse().unwrap()), "mapped is IPv4");
        for bad in [
            "203.0.113.9:443",
            "203.0.113.9, 10.0.0.1",
            "203.0.113.9,10.0.0.1",
            "[2001:db8::1]",
            "[2001:db8::1]:443",
            "fe80::1%eth0",
            "unknown",
            "",
            " 203.0.113.9",
            "203.0.113.9 ",
            "203.0.113.09",
            "203.0.113",
            "localhost",
        ] {
            assert_eq!(ip(bad), None, "{bad:?}");
        }
        let two = headers(&[(CLIENT_IP_HEADER, "203.0.113.9"), (CLIENT_IP_HEADER, "203.0.113.9")]);
        assert_eq!(asserted_client_ip(&two), None, "two header lines");
        assert_eq!(asserted_client_ip(&HeaderMap::new()), None);
        let mut opaque = HeaderMap::new();
        opaque.insert(CLIENT_IP_HEADER, HeaderValue::from_bytes(b"203.0.113.9\xff").unwrap());
        assert_eq!(asserted_client_ip(&opaque), None, "not visible ASCII");
    }

    #[test]
    fn only_the_same_branchs_secret_key_vouches() {
        let public = dk::resolved(DataKeyKind::Public);
        assert!(server_key_vouches(&public, &secret_of(&public)));
        let other_public = ResolvedKey { key_id: TestUuid::new_v4(), ..public.clone() };
        assert!(!server_key_vouches(&public, &other_public), "a public key");
        let other_project = dk::resolved(DataKeyKind::Secret);
        assert!(!server_key_vouches(&public, &other_project), "another project's secret key");
        let other_branch = ResolvedKey { branch_id: TestUuid::new_v4(), ..secret_of(&public) };
        assert!(!server_key_vouches(&public, &other_branch), "another branch's secret key");
        let same_branch_other_project = ResolvedKey { project_id: TestUuid::new_v4(), ..secret_of(&public) };
        assert!(!server_key_vouches(&public, &same_branch_other_project));
        // Only a public bearer key can be vouched for.
        let secret = secret_of(&public);
        assert!(!server_key_vouches(&secret, &secret_of(&public)));
    }

    #[tokio::test]
    async fn a_vouched_client_ip_is_forwarded_on_app_auth_routes() {
        let h = data_harness().await;
        let k = app_keys(&h);
        for (sent, want) in [
            ("203.0.113.9", "203.0.113.9"),
            ("2001:db8::1", "2001:db8::1"),
            ("::ffff:203.0.113.9", "203.0.113.9"),
        ] {
            let plan = expect_plan(
                app_call(&h, SIGN_IN, &k.public, &[(SERVER_KEY_HEADER, &k.secret), (CLIENT_IP_HEADER, sent)]).await,
            );
            assert_eq!(plan.client_ip, Some(want.parse().unwrap()), "{sent}");
        }
        // Every app route that takes the public key, not just sign-in.
        let plan = expect_plan(
            app_call(&h, "/auth/v1/app/refresh", &k.public, &[(SERVER_KEY_HEADER, &k.secret), (CLIENT_IP_HEADER, "203.0.113.9")])
                .await,
        );
        assert_eq!(plan.client_ip, Some("203.0.113.9".parse().unwrap()));
    }

    #[tokio::test]
    async fn the_request_is_still_the_public_keys() {
        let h = data_harness().await;
        let k = app_keys(&h);
        let plan = expect_plan(
            app_call(&h, SIGN_IN, &k.public, &[(SERVER_KEY_HEADER, &k.secret), (CLIENT_IP_HEADER, "203.0.113.9")]).await,
        );
        assert_eq!(plan.client_ip, Some("203.0.113.9".parse().unwrap()));
        let claims = h.verify.verify(plan.identity.as_deref().unwrap(), "auth").unwrap();
        assert_eq!(claims.sub, k.resolved.key_id.to_string());
        let data = claims.data.unwrap();
        assert_eq!(data.key_id, k.resolved.key_id);
        assert_eq!(data.kind, DataKeyKind::Public);
        assert_eq!(data.role, "anon");
        assert_eq!(data.role_oid, None);
    }

    #[tokio::test]
    async fn an_unvouched_client_ip_is_ignored() {
        let h = data_harness().await;
        let k = app_keys(&h);
        let other_project = dk::new_key(DataKeyKind::Secret);
        h.mock.set(&other_project, Answer::Known(dk::resolved(DataKeyKind::Secret)));
        let other_branch = dk::new_key(DataKeyKind::Secret);
        h.mock.set(
            &other_branch,
            Answer::Known(ResolvedKey { branch_id: TestUuid::new_v4(), ..secret_of(&k.resolved) }),
        );
        // The listener answers a revoked key as it does an unknown one: 404.
        let revoked = dk::new_key(DataKeyKind::Secret);
        h.mock.set(&revoked, Answer::NotFound);
        let unknown = dk::new_key(DataKeyKind::Secret);
        let failing = dk::new_key(DataKeyKind::Secret);
        h.mock.set(&failing, Answer::ServerError);
        let cases: Vec<(&str, Vec<(&str, &str)>)> = vec![
            ("no server key", vec![(CLIENT_IP_HEADER, "203.0.113.9")]),
            ("the public key as server key", vec![(SERVER_KEY_HEADER, &k.public), (CLIENT_IP_HEADER, "203.0.113.9")]),
            ("another project's secret key", vec![(SERVER_KEY_HEADER, &other_project), (CLIENT_IP_HEADER, "203.0.113.9")]),
            ("another branch's secret key", vec![(SERVER_KEY_HEADER, &other_branch), (CLIENT_IP_HEADER, "203.0.113.9")]),
            ("a revoked secret key", vec![(SERVER_KEY_HEADER, &revoked), (CLIENT_IP_HEADER, "203.0.113.9")]),
            ("an unknown key", vec![(SERVER_KEY_HEADER, &unknown), (CLIENT_IP_HEADER, "203.0.113.9")]),
            ("a lookup failure", vec![(SERVER_KEY_HEADER, &failing), (CLIENT_IP_HEADER, "203.0.113.9")]),
            ("not a key", vec![(SERVER_KEY_HEADER, "garbage"), (CLIENT_IP_HEADER, "203.0.113.9")]),
            ("an empty server key", vec![(SERVER_KEY_HEADER, ""), (CLIENT_IP_HEADER, "203.0.113.9")]),
            (
                "two server keys",
                vec![(SERVER_KEY_HEADER, &k.secret), (SERVER_KEY_HEADER, &k.secret), (CLIENT_IP_HEADER, "203.0.113.9")],
            ),
        ];
        for (case, extra) in cases {
            // No error to the caller: the request goes through as from the peer, and isn't
            // server-vouched (#55) with or without the client address.
            for with_ip in [true, false] {
                let extra: Vec<_> = extra.iter().copied().filter(|(n, _)| with_ip || *n != CLIENT_IP_HEADER).collect();
                let plan = expect_plan(app_call(&h, SIGN_IN, &k.public, &extra).await);
                assert_eq!(plan.client_ip, peer(), "{case} {with_ip}");
                let data = h.verify.verify(plan.identity.as_deref().unwrap(), "auth").unwrap().data.unwrap();
                assert_eq!(data.key_id, k.resolved.key_id, "{case} {with_ip}");
                assert!(!data.server_vouched, "{case} {with_ip}");
            }
        }
    }

    // ---- an app server vouching for a call without a client address (#55) ----

    /// Whether the plan's minted identity is server-vouched.
    fn server_vouched(h: &DataHarness, plan: &ProxyPlan) -> bool {
        h.verify.verify(plan.identity.as_deref().unwrap(), "auth").unwrap().data.unwrap().server_vouched
    }

    #[tokio::test]
    async fn the_branchs_secret_key_without_a_client_ip_vouches_for_the_call() {
        let h = data_harness().await;
        let k = app_keys(&h);
        for path in [SIGN_IN, "/auth/v1/app/refresh", "/auth/v1/app/sign-up"] {
            let plan = expect_plan(app_call(&h, path, &k.public, &[(SERVER_KEY_HEADER, &k.secret)]).await);
            assert_eq!(plan.client_ip, peer(), "{path}: the address is unchanged");
            assert!(server_vouched(&h, &plan), "{path}");
            // Still the public key's identity.
            let data = h.verify.verify(plan.identity.as_deref().unwrap(), "auth").unwrap().data.unwrap();
            assert_eq!((data.key_id, data.kind), (k.resolved.key_id, DataKeyKind::Public), "{path}");
        }
    }

    #[tokio::test]
    async fn a_malformed_client_ip_with_the_branchs_secret_key_is_server_vouched() {
        let h = data_harness().await;
        let k = app_keys(&h);
        let cases: Vec<(&str, Vec<(&str, &str)>)> = vec![
            ("a port", vec![(CLIENT_IP_HEADER, "203.0.113.9:443")]),
            ("a list", vec![(CLIENT_IP_HEADER, "203.0.113.9, 10.0.0.1")]),
            ("brackets", vec![(CLIENT_IP_HEADER, "[2001:db8::1]")]),
            ("unknown", vec![(CLIENT_IP_HEADER, "unknown")]),
            ("an empty value", vec![(CLIENT_IP_HEADER, "")]),
            ("two header lines", vec![(CLIENT_IP_HEADER, "203.0.113.9"), (CLIENT_IP_HEADER, "203.0.113.9")]),
        ];
        for (case, mut extra) in cases {
            extra.push((SERVER_KEY_HEADER, &k.secret));
            let plan = expect_plan(app_call(&h, SIGN_IN, &k.public, &extra).await);
            assert_eq!(plan.client_ip, peer(), "{case}: the malformed value is not used");
            assert!(server_vouched(&h, &plan), "{case}");
        }
    }

    #[tokio::test]
    async fn a_vouched_client_ip_is_not_server_vouched() {
        let h = data_harness().await;
        let k = app_keys(&h);
        let plan = expect_plan(
            app_call(&h, SIGN_IN, &k.public, &[(SERVER_KEY_HEADER, &k.secret), (CLIENT_IP_HEADER, "203.0.113.9")]).await,
        );
        assert_eq!(plan.client_ip, Some("203.0.113.9".parse().unwrap()));
        assert!(!server_vouched(&h, &plan), "per-IP limits apply to the forwarded address");
    }

    #[tokio::test]
    async fn a_browser_call_is_not_server_vouched() {
        let h = data_harness().await;
        let k = app_keys(&h);
        for extra in [vec![], vec![(CLIENT_IP_HEADER, "203.0.113.9")]] {
            let plan = expect_plan(app_call(&h, SIGN_IN, &k.public, &extra).await);
            assert_eq!(plan.client_ip, peer());
            assert!(!server_vouched(&h, &plan));
        }
    }

    #[tokio::test]
    async fn the_claim_is_never_set_on_data_routes() {
        let h = data_harness().await;
        let k = app_keys(&h);
        expect_plan(app_call(&h, "/data/v1/query", &k.public, &[]).await);
        let before = h.mock.lookups();
        for extra in [vec![(SERVER_KEY_HEADER, k.secret.as_str())], vec![]] {
            let plan = expect_plan(app_call(&h, "/data/v1/query", &k.public, &extra).await);
            let data = h.verify.verify(plan.identity.as_deref().unwrap(), "rest").unwrap().data.unwrap();
            assert!(!data.server_vouched);
        }
        assert_eq!(h.mock.lookups(), before, "the server key is never looked up on a data route");
    }

    /// The server key is looked up whenever it is sent on an app route, with or without a
    /// client address; each miss is charged to the real peer.
    #[tokio::test]
    async fn an_unknown_server_key_without_a_client_ip_is_charged_to_the_real_peer() {
        let h = data_harness().await;
        let k = app_keys(&h);
        for _ in 0..40 {
            let unknown = dk::new_key(DataKeyKind::Secret);
            let plan = expect_plan(app_call(&h, SIGN_IN, &k.public, &[(SERVER_KEY_HEADER, &unknown)]).await);
            assert_eq!(plan.client_ip, peer());
            assert!(!server_vouched(&h, &plan));
        }
        assert!(h.mock.lookups() <= 25, "the misses were limited: {}", h.mock.lookups());
        let key = dk::new_key(DataKeyKind::Public);
        let headers = headers(&[("authorization", &format!("Bearer {key}"))]);
        let parts =
            RequestParts { method: "POST", path: "/data/v1/query", host: Some(API), headers: &headers, client_ip: peer() };
        assert_eq!(expect_response(h.pipeline.handle(&parts).await.outcome).status, 429);
    }

    #[tokio::test]
    async fn a_valid_pair_is_ignored_on_data_routes() {
        let h = data_harness().await;
        let k = app_keys(&h);
        let pair = [(SERVER_KEY_HEADER, k.secret.as_str()), (CLIENT_IP_HEADER, "203.0.113.9")];
        // The public key's own lookup first; the server key is never looked up.
        expect_plan(app_call(&h, "/data/v1/query", &k.public, &[]).await);
        let before = h.mock.lookups();
        let plan = expect_plan(app_call(&h, "/data/v1/query", &k.public, &pair).await);
        assert_eq!(plan.client_ip, peer());
        assert_eq!(h.mock.lookups(), before);
    }

    #[tokio::test]
    async fn an_unknown_server_key_is_charged_to_the_real_peer() {
        let h = data_harness().await;
        let k = app_keys(&h);
        let asserted = "203.0.113.9";
        // Each fresh unknown server key is one lookup charged to someone's miss bucket.
        for _ in 0..40 {
            let unknown = dk::new_key(DataKeyKind::Secret);
            let plan = expect_plan(
                app_call(&h, SIGN_IN, &k.public, &[(SERVER_KEY_HEADER, &unknown), (CLIENT_IP_HEADER, asserted)]).await,
            );
            assert_eq!(plan.client_ip, peer());
        }
        assert!(h.mock.lookups() <= 25, "the misses were limited: {}", h.mock.lookups());
        // The peer's bucket is empty; the asserted address's is untouched.
        let miss = async |ip: &str| {
            let key = dk::new_key(DataKeyKind::Public);
            let headers = headers(&[("authorization", &format!("Bearer {key}"))]);
            let parts = RequestParts {
                method: "POST",
                path: "/data/v1/query",
                host: Some(API),
                headers: &headers,
                client_ip: Some(ip.parse().unwrap()),
            };
            expect_response(h.pipeline.handle(&parts).await.outcome).status
        };
        assert_eq!(miss(PEER).await, 429);
        assert_eq!(miss(asserted).await, 401);
    }

    #[tokio::test]
    async fn neither_header_reaches_the_upstream() {
        let h = data_harness().await;
        let k = app_keys(&h);
        let pair = [(SERVER_KEY_HEADER, k.secret.as_str()), (CLIENT_IP_HEADER, "203.0.113.9")];
        for path in [SIGN_IN, "/data/v1/query"] {
            let plan = expect_plan(app_call(&h, path, &k.public, &pair).await);
            let edits = plan.upstream_headers("rid");
            for name in [SERVER_KEY_HEADER, CLIENT_IP_HEADER] {
                assert!(edits.remove.contains(&name), "{path}: {name}");
                assert!(!edits.set.iter().any(|(n, _)| *n == name), "{path}: {name}");
            }
        }
        // Public routes too.
        let plan = expect_plan(run(&h.pipeline, "GET", "/health", None, &pair).await.outcome);
        assert!(plan.upstream_headers("rid").remove.contains(&SERVER_KEY_HEADER));
    }

    #[tokio::test]
    async fn preflights_allow_neither_header() {
        let h = data_harness().await;
        for path in [SIGN_IN, "/data/v1/query"] {
            let resp = expect_response(
                run(
                    &h.pipeline,
                    "OPTIONS",
                    path,
                    None,
                    &[
                        ("origin", "https://app.example"),
                        ("access-control-request-method", "POST"),
                        ("access-control-request-headers", "chelabase-server-key, chelabase-client-ip"),
                    ],
                )
                .await
                .outcome,
            );
            let allowed = header(&resp.headers, "access-control-allow-headers").unwrap();
            assert!(!allowed.contains(SERVER_KEY_HEADER) && !allowed.contains(CLIENT_IP_HEADER), "{path}: {allowed}");
        }
    }

    #[derive(Clone, Default)]
    struct Logs(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Logs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn no_log_line_holds_either_headers_value() {
        let logs = Logs::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let h = data_harness().await;
        let k = app_keys(&h);
        let other = dk::new_key(DataKeyKind::Secret);
        h.mock.set(&other, Answer::Known(dk::resolved(DataKeyKind::Secret)));
        let unknown = dk::new_key(DataKeyKind::Secret);
        let failing = dk::new_key(DataKeyKind::Secret);
        h.mock.set(&failing, Answer::ServerError);
        let ips = ["203.0.113.9", "2001:db8::1", "203.0.113.9:443", "unknown-ip-value"];
        for server in [&k.secret, &k.public, &other, &unknown, &failing] {
            for ip in ips {
                expect_plan(app_call(&h, SIGN_IN, &k.public, &[(SERVER_KEY_HEADER, server), (CLIENT_IP_HEADER, ip)]).await);
            }
        }
        expect_plan(app_call(&h, SIGN_IN, &k.public, &[(CLIENT_IP_HEADER, "203.0.113.9")]).await);
        expect_plan(app_call(&h, SIGN_IN, &k.public, &[(SERVER_KEY_HEADER, &k.secret)]).await);

        let text = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
        assert!(text.contains("chelabase-client-ip"), "the decisions are logged:\n{text}");
        for value in [k.secret.as_str(), k.public.as_str(), &other, &unknown, &failing, "203.0.113", "2001:db8", "unknown-ip-value"] {
            assert!(!text.contains(value), "{value} logged:\n{text}");
        }
    }

    // ---- edge functions m4: the functions upstream ----

    fn set_value<'a>(edits: &'a UpstreamHeaders, name: &str) -> Option<&'a str> {
        edits.set.iter().find(|(n, _)| *n == name).map(|(_, v)| v.as_str())
    }

    /// The gateway hands the functions service the caller's key only when it is a public key,
    /// after resolving it; a secret key never leaves the gateway. The identity is minted for
    /// `functions` and names the key's branch.
    #[tokio::test]
    async fn functions_calls_forward_a_public_key_only() {
        let h = data_harness().await;
        for kind in [DataKeyKind::Public, DataKeyKind::Secret] {
            let key = dk::new_key(kind);
            let resolved = dk::resolved(kind);
            h.mock.set(&key, Answer::Known(resolved.clone()));
            for path in ["/functions/v1/hello", "/functions/v1/hello/a/b"] {
                let plan = expect_plan(data_call(&h, "POST", path, Some(&key)).await.outcome);
                assert_eq!(plan.endpoint.service, Service::Functions, "{path}");
                let claims = h.verify.verify(plan.identity.as_deref().unwrap(), "functions").unwrap();
                assert_eq!(claims.data.unwrap().branch_id, resolved.branch_id);
                let edits = plan.upstream_headers("rid");
                let forwarded = set_value(&edits, FORWARDED_KEY_HEADER);
                match kind {
                    DataKeyKind::Public => assert_eq!(forwarded, Some(key.as_str())),
                    DataKeyKind::Secret => assert_eq!(forwarded, None),
                }
                assert_eq!(set_value(&edits, FORWARDED_USER_HEADER), None);
                assert!(edits.remove.contains(&"authorization"));
            }
        }
    }

    #[tokio::test]
    async fn functions_calls_forward_the_verified_user_token() {
        let h = data_harness().await;
        let case = user_case(&h, DataKeyKind::Public);
        h.sessions.set(case.sid, ut::Session::Live(case.project));
        let token = user_token(&h, case.project, case.sid, 300);
        let bearer = format!("Bearer {}", case.key);
        let plan = expect_plan(
            run(&h.pipeline, "GET", "/functions/v1/hello", None, &[("authorization", &bearer), ("chelabase-user", &token)])
                .await
                .outcome,
        );
        let edits = plan.upstream_headers("rid");
        assert_eq!(set_value(&edits, FORWARDED_KEY_HEADER), Some(case.key.as_str()));
        assert_eq!(set_value(&edits, FORWARDED_USER_HEADER), Some(token.as_str()));
        assert!(edits.remove.contains(&USER_TOKEN_HEADER));
    }

    #[tokio::test]
    async fn functions_key_form_without_a_key_is_401() {
        let h = data_harness().await;
        let resp = expect_response(data_call(&h, "GET", "/functions/v1/hello", None).await.outcome);
        assert_eq!(resp.status, 401);
    }

    /// The keyless `/public/` form: no identity, nothing forwarded, whatever the client sent.
    #[tokio::test]
    async fn the_public_functions_form_mints_and_forwards_nothing() {
        let h = data_harness().await;
        let key = dk::new_key(DataKeyKind::Public);
        h.mock.set(&key, Answer::Known(dk::resolved(DataKeyKind::Public)));
        let bearer = format!("Bearer {key}");
        let path = format!("/functions/v1/public/{}/hello/x", TestUuid::new_v4());
        let plan = expect_plan(
            run(
                &h.pipeline,
                "GET",
                &path,
                None,
                &[("authorization", &bearer), (FORWARDED_KEY_HEADER, "forged"), (FORWARDED_USER_HEADER, "forged")],
            )
            .await
            .outcome,
        );
        assert_eq!(plan.endpoint.service, Service::Functions);
        assert_eq!(plan.identity, None);
        let edits = plan.upstream_headers("rid");
        assert_eq!(set_value(&edits, FORWARDED_KEY_HEADER), None);
        assert_eq!(set_value(&edits, FORWARDED_USER_HEADER), None);
        assert!(edits.remove.contains(&FORWARDED_KEY_HEADER));
        assert!(edits.remove.contains(&FORWARDED_USER_HEADER));
    }

    /// No other upstream ever gets either forwarded header: the client's copies are removed
    /// and the gateway sets them for the functions service only.
    #[tokio::test]
    async fn neither_forwarded_header_reaches_rest_or_realtime() {
        let h = data_harness().await;
        let case = user_case(&h, DataKeyKind::Public);
        h.sessions.set(case.sid, ut::Session::Live(case.project));
        let token = user_token(&h, case.project, case.sid, 300);
        let bearer = format!("Bearer {}", case.key);
        let forged = [
            ("authorization", bearer.as_str()),
            ("chelabase-user", token.as_str()),
            (FORWARDED_KEY_HEADER, "forged"),
            (FORWARDED_USER_HEADER, "forged"),
        ];
        let mut services = Vec::new();
        for (method, path) in [("POST", "/data/v1/query"), ("GET", "/v1/realtime/socket")] {
            let handled = run(&h.pipeline, method, path, None, &forged).await;
            let Outcome::Proxy(plan) = handled.outcome else { continue };
            services.push(plan.endpoint.service);
            let edits = plan.upstream_headers("rid");
            assert!(edits.remove.contains(&FORWARDED_KEY_HEADER), "{path}");
            assert!(edits.remove.contains(&FORWARDED_USER_HEADER), "{path}");
            assert_eq!(set_value(&edits, FORWARDED_KEY_HEADER), None, "{path}");
            assert_eq!(set_value(&edits, FORWARDED_USER_HEADER), None, "{path}");
        }
        assert_eq!(services, vec![Service::Rest, Service::Realtime]);
    }

    #[tokio::test]
    async fn the_functions_internal_routes_are_not_routable() {
        let h = data_harness().await;
        let sha = "a".repeat(64);
        for (method, path) in [
            ("GET", format!("/internal/bundles/{}/{sha}", TestUuid::new_v4())),
            ("PUT", format!("/internal/bundles/{}/{sha}", TestUuid::new_v4())),
            ("POST", format!("/internal/invoke/{}/hello", TestUuid::new_v4())),
        ] {
            let resp = expect_response(run(&h.pipeline, method, &path, None, &[]).await.outcome);
            assert_eq!(resp.status, 404, "{method} {path}");
        }
    }
}
