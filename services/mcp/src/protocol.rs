//! The MCP methods, on the official Rust SDK (`rmcp`): `initialize` (and 2026-07-28's
//! `server/discover`), `tools/list` and `tools/call`. The SDK's stateless Streamable HTTP
//! service carries them; [`crate::app`] puts the authentication and the request checks in
//! front of it and hands each request's verified [`Caller`] down in the request extensions.

use std::borrow::Cow;
use std::sync::Arc;

use axum::http::request::Parts;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, DiscoverResult, Implementation,
    InitializeRequestParams, InitializeResult, ListToolsResult, PaginatedRequestParams,
    ProtocolVersion, ServerCapabilities, ServerConfig,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler};

use crate::auth::Caller;
use crate::sandbox::Sandbox;
use crate::tools;

/// The revisions served: the current one and the one before it.
pub const SUPPORTED_REVISIONS: [&str; 2] = ["2026-07-28", "2025-11-25"];

/// The gateway's request id header.
const REQUEST_ID_HEADER: &str = "x-request-id";

#[derive(Clone)]
pub struct Server {
    sandbox: Arc<dyn Sandbox>,
    log_code: bool,
}

impl Server {
    pub fn new(sandbox: Arc<dyn Sandbox>, log_code: bool) -> Self {
        Self { sandbox, log_code }
    }
}

fn parts(context: &RequestContext<RoleServer>) -> Option<&Parts> {
    context.extensions.get::<Parts>()
}

/// The verified caller [`crate::app`] attached. Every method needs one; its absence means the
/// handler was reached without the gate, and nothing is served.
fn caller(context: &RequestContext<RoleServer>) -> Result<Caller, ErrorData> {
    parts(context)
        .and_then(|p| p.extensions.get::<Caller>())
        .cloned()
        .ok_or_else(|| ErrorData::internal_error("unauthenticated request", None))
}

/// The gateway's request id, or the JSON-RPC id when there is none.
fn request_id(context: &RequestContext<RoleServer>) -> String {
    parts(context)
        .and_then(|p| p.headers.get(REQUEST_ID_HEADER))
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(|| serde_json::to_string(&context.id).unwrap_or_default())
}

impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("chelabase", env!("CARGO_PKG_VERSION")))
            .with_instructions(tools::instructions(false))
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Owned(vec![
            ProtocolVersion::V_2026_07_28,
            ProtocolVersion::V_2025_11_25,
        ])
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        let caller = caller(&context)?;
        context.peer.set_peer_info(request.clone());
        let mut result = self.negotiate_initialize(&request)?;
        result.instructions = Some(tools::instructions(caller.read_only()));
        Ok(result)
    }

    async fn discover(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, ErrorData> {
        let caller = caller(&context)?;
        let info = self
            .get_info()
            .with_instructions(tools::instructions(caller.read_only()));
        Ok(DiscoverResult::from_server_info(
            self.supported_protocol_versions().into_owned(),
            info,
        ))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let caller = caller(&context)?;
        Ok(ListToolsResult::with_all_items(tools::definitions(
            caller.read_only(),
        )))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let caller = caller(&context)?;
        tools::call(
            self.sandbox.as_ref(),
            &caller,
            &request_id(&context),
            self.log_code,
            &request.name,
            request.arguments,
        )
        .await
        .map(Into::into)
    }
}
