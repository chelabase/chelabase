//! External services (edge functions m9): `ANY /services/v1/{name}[/{*rest}]` with a data key,
//! proxied to the service's base URL with the rest and the query ([`target`]).
//!
//! - The function's `auth` applies as for a function ([`crate::caller`]). Not a service, or
//!   no deployment on the branch: 404 `service_not_found`. Disabled: 503 `service_disabled`;
//!   [`health`] says unhealthy: 503 `service_unhealthy`.
//! - The request goes through the services' own [`Outbound`] ([`outbound_from_lookup`]:
//!   `CHELABASE_SERVICES_ALLOW_HOSTS` and `_ALLOW_HTTP`, never the webhooks' settings, and the
//!   stack's hard deny), so it reaches public addresses only, or allowlisted hosts outside the
//!   stack; a redirect comes back to the client as it is; there is no total timeout, only
//!   [`HEADERS_TIMEOUT`] for the response's headers and [`IDLE_TIMEOUT`] between reads.
//! - Hop-by-hop headers (and those `Connection` names), `host`, `authorization`, every
//!   `chelabase-*` and `x-chelabase-*` and `content-digest` from the client are dropped; the
//!   service gets `chelabase-branch`, `chelabase-role`, `chelabase-user` (the app user's
//!   subject, else empty), a fresh `chelabase-request-id` and the [`signature`].

pub mod health;
pub mod signature;
pub mod target;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Response, StatusCode, header};
use bytes::{Bytes, BytesMut};
use chelabase_http::outbound::{Outbound, PublicClient, SERVICES_ENV, refused_by_guard};
use futures_util::StreamExt;
use uuid::Uuid;

use crate::app::{AppState, Incoming, error, strip};
use crate::caller::{self, Caller, Form, Refusal};
use health::{Health, Status};
use signature::{BODY, BRANCH, DIGEST, MAX_SIGNED_BODY, REQUEST_ID, ROLE, SIGNATURE, Signed, USER};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const HEADERS_TIMEOUT: Duration = Duration::from_secs(30);
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// `CHELABASE_INTERNAL_SUBNETS`: the stack's own networks (comma-separated CIDRs), denied
/// beside the networks this container is attached to.
pub const INTERNAL_SUBNETS: &str = "CHELABASE_INTERNAL_SUBNETS";

/// The proxy's client and the services' health.
pub struct Services {
    pub client: PublicClient,
    pub health: Arc<Health>,
}

/// Where services may be: `CHELABASE_SERVICES_*`, with the stack's hard deny over
/// [`INTERNAL_SUBNETS`] and the attached networks.
pub fn outbound_from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Outbound> {
    let mut internal =
        chelabase_http::outbound::parse_nets(&get(INTERNAL_SUBNETS).unwrap_or_default())
            .map_err(|e| anyhow::anyhow!("{INTERNAL_SUBNETS}: {e}"))?;
    internal.extend(chelabase_http::outbound::attached_nets());
    Ok(Outbound::from_lookup(SERVICES_ENV, get)
        .map_err(anyhow::Error::msg)?
        .deny_stack(internal))
}

impl Services {
    pub fn new(outbound: &Outbound) -> reqwest::Result<Self> {
        Ok(Self {
            client: outbound.streaming_client(CONNECT_TIMEOUT, IDLE_TIMEOUT)?,
            health: Arc::new(Health::default()),
        })
    }
}

fn not_found() -> Response<Body> {
    error(
        StatusCode::NOT_FOUND,
        "service_not_found",
        "no such service",
    )
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The client's headers without ours, the hop's, and those `Connection` names.
fn forwarded_headers(incoming: &HeaderMap) -> HeaderMap {
    let mut out = incoming.clone();
    let named: Vec<String> = incoming
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(',').map(|n| n.trim().to_ascii_lowercase()))
        .collect();
    for name in named {
        out.remove(name.as_str());
    }
    strip(&mut out, true);
    out.remove(DIGEST);
    out
}

/// The body: buffered when it ends within [`MAX_SIGNED_BODY`], else streamed as it comes.
enum Payload {
    Signed(Bytes),
    Streamed(reqwest::Body),
}

async fn payload(body: Body) -> Option<Payload> {
    let mut stream = body.into_data_stream();
    let mut buf = BytesMut::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if buf.len() + chunk.len() > MAX_SIGNED_BODY {
            let head = futures_util::stream::iter([Ok(buf.freeze()), Ok(chunk)]);
            let rest = stream.map(|c| c.map_err(std::io::Error::other));
            return Some(Payload::Streamed(reqwest::Body::wrap_stream(
                head.chain(rest),
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    Some(Payload::Signed(buf.freeze()))
}

/// One call of service `name` on `branch` by `caller` (the key form).
pub(crate) async fn call(
    s: &AppState,
    branch: Uuid,
    name: &str,
    caller: Caller,
    req: Incoming,
) -> Response<Body> {
    let Some(services) = s.services.as_ref() else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "services are unavailable",
        );
    };
    let resolution = match s.resolver.resolve(branch, name).await {
        Ok(Some(r)) if r.service.is_some() => r,
        Ok(_) => return not_found(),
        Err(e) => {
            tracing::warn!("{e}");
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "service lookup is unavailable; retry shortly",
            );
        }
    };
    match caller::check(resolution.auth, Form::Key, &caller) {
        Ok(()) => {}
        Err(Refusal::NotFound) => return not_found(),
        Err(Refusal::Unauthorized) => {
            return error(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "this service needs another credential",
            );
        }
    }
    let external = resolution.service.as_ref().expect("checked above");
    if !resolution.enabled {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "service_disabled",
            "the service is disabled",
        );
    }
    if services.health.status(external.deployment_id) == Status::Unhealthy {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "service_unhealthy",
            "the service is failing its health checks",
        );
    }
    let Some(secret) = external.secret.as_ref() else {
        tracing::warn!(%branch, service = name, "the service has no signing secret");
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "the service has no signing secret",
        );
    };
    let Some(upstream) = target::upstream(&external.url, &req.path, req.query.as_deref()) else {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_path",
            "the path leaves the service",
        );
    };
    let Some(body) = payload(req.body).await else {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "the request body couldn't be read",
        );
    };
    let (role, user) = match &caller {
        Caller::PublicKey { role } | Caller::SecretKey { role } => (role.as_str(), ""),
        Caller::AppUser { role, user_id, .. } => {
            (role.as_str(), user_id.as_deref().unwrap_or_default())
        }
        Caller::Anonymous | Caller::Internal => ("", ""),
    };
    let (request_id, branch_text, target) = (
        Uuid::new_v4().to_string(),
        branch.to_string(),
        upstream.target(),
    );
    let digest = match &body {
        Payload::Signed(bytes) => Some(signature::content_digest(bytes)),
        Payload::Streamed(_) => None,
    };
    let signed = Signed {
        request_id: &request_id,
        method: req.method.as_str(),
        target: &target,
        branch: &branch_text,
        role,
        user,
        digest: digest.as_deref(),
    };
    let Ok(sig) = signature::sign(secret, unix_now(), &signed) else {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "a signed value holds a line break",
        );
    };
    let mut headers = forwarded_headers(&req.headers);
    let ours = [
        (BRANCH, branch_text.as_str()),
        (ROLE, role),
        (USER, user),
        (REQUEST_ID, &request_id),
        (SIGNATURE, &sig),
    ];
    for (name, value) in ours
        .into_iter()
        .chain(digest.as_deref().map(|d| (DIGEST, d)))
    {
        let Ok(value) = HeaderValue::from_str(value) else {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "a signed value isn't a header value",
            );
        };
        headers.insert(HeaderName::from_static(name), value);
    }
    if digest.is_none() {
        headers.insert(BODY, HeaderValue::from_static("unsigned"));
    }
    let Ok(builder) = services.client.request(req.method, upstream.url.as_str()) else {
        return error(
            StatusCode::BAD_GATEWAY,
            "service_refused",
            "the service's address is refused",
        );
    };
    let builder = builder.headers(headers);
    let builder = match body {
        Payload::Signed(bytes) => builder.body(bytes),
        Payload::Streamed(stream) => builder.body(stream),
    };
    let resp = match PublicClient::send_within(builder, HEADERS_TIMEOUT).await {
        Some(Ok(resp)) => resp,
        Some(Err(e)) => {
            if let Some(refused) = refused_by_guard(&e) {
                tracing::warn!(%branch, service = name, "service refused: {}", refused.log_detail());
                return error(
                    StatusCode::BAD_GATEWAY,
                    "service_refused",
                    "the service's address is refused",
                );
            }
            tracing::warn!(%branch, service = name, "service unreachable: {e}");
            return error(
                StatusCode::BAD_GATEWAY,
                "service_unreachable",
                "the service didn't answer",
            );
        }
        None => {
            return error(
                StatusCode::GATEWAY_TIMEOUT,
                "service_timeout",
                "the service didn't answer in time",
            );
        }
    };
    let mut out = Response::builder().status(resp.status());
    if let Some(h) = out.headers_mut() {
        *h = resp.headers().clone();
        strip(h, false);
    }
    out.body(Body::from_stream(resp.bytes_stream()))
        .unwrap_or_else(|_| {
            error(
                StatusCode::BAD_GATEWAY,
                "service_unreachable",
                "the service's answer was invalid",
            )
        })
}
