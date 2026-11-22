//! The HTTP edge: `POST /mcp` (stateless Streamable HTTP, JSON answers), the protected-resource
//! metadata (RFC 9728) at `/.well-known/oauth-protected-resource[/mcp]`, and `/health`.
//!
//! Every `/mcp` request passes one gate before the SDK reads its body: an `Origin` not on the
//! allow-list is 403, a missing or invalid member token is 401 with the `resource_metadata`
//! challenge, any method but POST is 405, an unsupported `MCP-Protocol-Version` is 400. The
//! gate then drops `Authorization` and `Mcp-Session-Id` and attaches the verified [`Caller`].

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use chelabase_auth::member_token::MemberTokenVerifier;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::json;

use crate::auth::{Authenticator, Caller};
use crate::limits::{Admission, Limits};
use crate::protocol::{SUPPORTED_REVISIONS, Server};
use crate::sandbox::Sandbox;

/// The largest `/mcp` request body (256 KiB); larger is 413.
pub const MAX_BODY: usize = 256 * 1024;
pub const ENDPOINT_PATH: &str = "/mcp";
pub const METADATA_PATH: &str = "/.well-known/oauth-protected-resource";
pub const METADATA_MCP_PATH: &str = "/.well-known/oauth-protected-resource/mcp";

/// What the router needs from the configuration.
pub struct Settings {
    /// The public API origin (`<scheme>://<api host>`): the resource and the challenge are
    /// built from it, never from a request's `Host`.
    pub api_base: String,
    /// Browser origins allowed to call `/mcp` (`CHELABASE_MCP_ALLOWED_ORIGINS`, empty by
    /// default: a request with any `Origin` is refused).
    pub allowed_origins: Vec<String>,
    /// Log each call's code in its audit line (`CHELABASE_MCP_LOG_CODE=1`).
    pub log_code: bool,
    /// What bounds each tool call.
    pub limits: Limits,
}

struct Gate {
    auth: Authenticator,
    challenge: HeaderValue,
    allowed_origins: Vec<String>,
}

impl Gate {
    fn origin_allowed(&self, origin: &HeaderValue) -> bool {
        origin.to_str().is_ok_and(|o| {
            self.allowed_origins
                .iter()
                .any(|a| a.eq_ignore_ascii_case(o))
        })
    }
}

/// The router for `settings`, verifying member tokens with `verifier` and running code in
/// `sandbox`.
pub fn router(
    settings: Settings,
    verifier: MemberTokenVerifier,
    sandbox: Arc<dyn Sandbox>,
) -> Router {
    let api_base = settings.api_base.trim_end_matches('/').to_owned();
    let challenge = format!(r#"Bearer resource_metadata="{api_base}{METADATA_MCP_PATH}""#);
    let gate = Arc::new(Gate {
        auth: Authenticator::new(verifier),
        challenge: HeaderValue::from_str(&challenge)
            .expect("the API origin is a valid header value"),
        allowed_origins: settings
            .allowed_origins
            .iter()
            .map(|o| o.trim().trim_end_matches('/').to_owned())
            .filter(|o| !o.is_empty())
            .collect(),
    });

    let mut config = StreamableHttpServerConfig::default()
        .with_json_response(true)
        // The gateway is the edge and names the API host; the gate checks `Origin` itself.
        .disable_allowed_hosts()
        .disable_allowed_origins();
    config.legacy_session_mode = false;
    config.sse_keep_alive = None;
    config.max_request_body_bytes = MAX_BODY;
    let server = Server::new(
        sandbox,
        Admission::new(&settings.limits),
        settings.limits.result_max_bytes,
        settings.log_code,
    );
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    );

    let metadata = json!({
        "resource": format!("{api_base}{ENDPOINT_PATH}"),
        "authorization_servers": [api_base],
        "bearer_methods_supported": ["header"],
    });
    let metadata = move || {
        let doc = metadata.clone();
        async move { axum::Json(doc) }
    };

    let mcp = Router::new()
        .route_service(ENDPOINT_PATH, service)
        // `route_layer`: the gate runs for `/mcp` only, so an unknown path is a 404.
        .route_layer(middleware::from_fn_with_state(gate, gate_request));
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route(METADATA_PATH, get(metadata.clone()))
        .route(METADATA_MCP_PATH, get(metadata))
        .merge(mcp)
}

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        axum::Json(json!({"error": {"code": code, "message": message}})),
    )
        .into_response()
}

async fn gate_request(State(gate): State<Arc<Gate>>, mut request: Request, next: Next) -> Response {
    if let Some(origin) = request.headers().get(header::ORIGIN)
        && !gate.origin_allowed(origin)
    {
        return error(
            StatusCode::FORBIDDEN,
            "forbidden",
            "this origin may not call the MCP endpoint",
        );
    }
    let caller: Caller = match gate.auth.authenticate(request.headers()) {
        Ok(caller) => caller,
        Err(_) => {
            let mut response = error(
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "a member token with at least 60 seconds left is required",
            );
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, gate.challenge.clone());
            return response;
        }
    };
    if request.method() != Method::POST {
        let mut response = error(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "the MCP endpoint takes POST only",
        );
        response
            .headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("POST"));
        return response;
    }
    if let Some(version) = request.headers().get("mcp-protocol-version")
        && !SUPPORTED_REVISIONS
            .iter()
            .any(|s| version.as_bytes() == s.as_bytes())
    {
        let body = json!({
            "jsonrpc": "2.0",
            "error": {
                "code": -32022,
                "message": "Unsupported protocol version",
                "data": {"requested": version.to_str().unwrap_or(""), "supported": SUPPORTED_REVISIONS},
            },
        });
        return (StatusCode::BAD_REQUEST, axum::Json(body)).into_response();
    }
    let headers = request.headers_mut();
    headers.remove(header::AUTHORIZATION);
    // Stateless: no session is issued, and one sent is ignored.
    headers.remove("mcp-session-id");
    request.extensions_mut().insert(caller);
    next.run(request).await
}

/// One tool call's audit record.
pub struct Audit<'a> {
    pub request_id: &'a str,
    pub caller: &'a Caller,
    pub tool: &'a str,
    pub code_sha256: String,
    /// Logged only when `CHELABASE_MCP_LOG_CODE=1`.
    pub code: &'a str,
    pub destructive: bool,
    pub duration_ms: u64,
    pub outcome: &'a str,
    /// Bytes of the tool result's text (after any cut).
    pub result_bytes: usize,
    /// Whether the result was cut to the cap.
    pub truncated: bool,
    /// The run's id (empty when nothing ran).
    pub exec_id: &'a str,
}

/// Writes `record`'s one `info` line (target `mcp::audit`).
pub fn audit(record: &Audit<'_>, log_code: bool) {
    let code = log_code.then_some(record.code);
    tracing::info!(
        target: "mcp::audit",
        request_id = record.request_id,
        member = record.caller.member(),
        tenant = %record.caller.tenant(),
        credential = record.caller.kind().as_str(),
        read_only = record.caller.read_only(),
        tool = record.tool,
        code_sha256 = %record.code_sha256,
        code_bytes = record.code.len(),
        destructive = record.destructive,
        duration_ms = record.duration_ms,
        outcome = record.outcome,
        result_bytes = record.result_bytes,
        truncated = record.truncated,
        exec_id = record.exec_id,
        code,
        "tool call"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn captured(log_code: bool, record: &Audit<'_>) -> String {
        let buf = Buf::default();
        let writer = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, || audit(record, log_code));
        String::from_utf8(buf.0.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn audit_line_has_no_code_by_default() {
        let caller = Caller::for_tests("member-1", uuid::Uuid::nil(), false);
        let record = Audit {
            request_id: "req-1",
            caller: &caller,
            tool: "search",
            code_sha256: "00".into(),
            code: "async () => 'secret-code-marker'",
            destructive: false,
            duration_ms: 3,
            outcome: "ok",
            result_bytes: 2,
            truncated: false,
            exec_id: "ab12",
        };
        let line = captured(false, &record);
        assert_eq!(line.lines().count(), 1, "{line}");
        assert!(!line.contains("secret-code-marker"), "{line}");
        assert!(line.contains("code_bytes=32"), "{line}");
        assert!(line.contains("truncated=false"), "{line}");
        assert!(line.contains("exec_id=\"ab12\""), "{line}");
        let line = captured(true, &record);
        assert!(line.contains("secret-code-marker"), "{line}");
    }
}
