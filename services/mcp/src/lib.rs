//! The Chelabase MCP server: `search` and `execute` over the Chelabase API for a signed-in
//! member, behind the gateway's `/mcp` route.

pub mod app;
pub mod auth;
pub mod config;
pub mod host;
pub mod internal;
pub mod limits;
pub mod policy;
pub mod protocol;
pub mod routes_match;
pub mod runtime;
pub mod sandbox;
pub mod sealing;
pub mod secret_fields;
pub mod spec;
pub mod tools;

/// The log filter when `RUST_LOG` is unset. The protocol library's own lines stay at `warn`.
pub const DEFAULT_LOG_FILTER: &str = "info,rmcp=warn";

/// The log filter for `env` (the value of `RUST_LOG`, if set): `rmcp=warn` is always last, so
/// the protocol library's debug lines (which can carry a request's body) never reach the logs.
/// An operator cannot raise `rmcp` above `warn` through `RUST_LOG`.
pub fn log_filter(env: Option<&str>) -> String {
    match env.map(str::trim).filter(|e| !e.is_empty()) {
        Some(env) => format!("{env},rmcp=warn"),
        None => DEFAULT_LOG_FILTER.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rmcp_stays_at_warn_whatever_rust_log_says() {
        assert_eq!(log_filter(None), DEFAULT_LOG_FILTER);
        assert_eq!(log_filter(Some("")), DEFAULT_LOG_FILTER);
        assert_eq!(log_filter(Some("debug")), "debug,rmcp=warn");
        assert_eq!(log_filter(Some("rmcp=trace")), "rmcp=trace,rmcp=warn");
        // The last directive for a target wins.
        let filter = tracing_subscriber::EnvFilter::new(log_filter(Some("rmcp=trace")));
        assert!(filter.to_string().ends_with("rmcp=warn"));
    }
}
