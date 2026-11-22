//! The Chelabase MCP server: `search` and `execute` over the Chelabase API for a signed-in
//! member, behind the gateway's `/mcp` route.

pub mod app;
pub mod auth;
pub mod config;
pub mod policy;
pub mod protocol;
pub mod sandbox;
pub mod sealing;
pub mod secret_fields;
pub mod spec;
pub mod tools;

/// The log filter when `RUST_LOG` is unset. The protocol library's own lines stay at `warn`.
pub const DEFAULT_LOG_FILTER: &str = "info,rmcp=warn";
