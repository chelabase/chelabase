//! RT1 final fix round, batch A (T3 hardening): the gateway's log filter caps Pingora's targets
//! (`pingora_*`) at `warn` whatever `RUST_LOG` says. Pingora's debug and trace logs print whole
//! requests (a realtime ticket in the query, `Authorization`), and W16's `request_summary` only
//! covers its error logs.
//!
//! Pinned: `gateway::logging::env_filter(rust_log: Option<&str>) -> tracing_subscriber::EnvFilter`
//! (`main` passes `RUST_LOG`; `None` is today's default `gateway=info,chelabase_auth=info,pingora=warn`).

use std::io::Write;
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// What gets logged with the gateway's filter for `RUST_LOG = rust_log`.
fn logged(rust_log: Option<&str>) -> String {
    let out = Captured::default();
    let writer = out.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(gateway::logging::env_filter(rust_log))
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        tracing::trace!(target: "pingora_proxy::proxy_h1", "LEAK-trace-proxy GET /v1/realtime/socket?ticket=SECRET");
        tracing::debug!(target: "pingora_core::protocols::http::v1::server", "LEAK-debug-core Authorization: Bearer SECRET");
        tracing::info!(target: "pingora_proxy", "LEAK-info-proxy");
        tracing::warn!(target: "pingora_core::server", "KEPT-warn-core");
        tracing::error!(target: "pingora_proxy", "KEPT-error-proxy");
        tracing::debug!(target: "gateway::proxy", "GATEWAY-debug");
        tracing::info!(target: "gateway::proxy", "GATEWAY-info");
    });
    String::from_utf8(out.0.lock().unwrap().clone()).unwrap()
}

#[test]
fn pingora_logs_are_capped_at_warn_whatever_rust_log_says() {
    for rust_log in [
        None,
        Some("debug"),
        Some("trace"),
        Some("pingora=trace"),
        Some("pingora_proxy=trace,pingora_core=debug"),
        Some("info,pingora_core::protocols=trace"),
        Some("gateway=debug,pingora_proxy::proxy_h1=trace"),
    ] {
        let text = logged(rust_log);
        assert!(
            !text.contains("LEAK-"),
            "RUST_LOG={rust_log:?} lets Pingora log below warn:\n{text}"
        );
        assert!(!text.contains("SECRET"), "RUST_LOG={rust_log:?}:\n{text}");
        assert!(
            text.contains("KEPT-warn-core"),
            "RUST_LOG={rust_log:?} drops Pingora's warnings:\n{text}"
        );
        assert!(
            text.contains("KEPT-error-proxy"),
            "RUST_LOG={rust_log:?}:\n{text}"
        );
    }
}

#[test]
fn the_gateways_own_level_still_follows_rust_log() {
    // Guard: only Pingora is capped.
    let default = logged(None);
    assert!(
        default.contains("GATEWAY-info") && !default.contains("GATEWAY-debug"),
        "{default}"
    );
    let debug = logged(Some("debug"));
    assert!(debug.contains("GATEWAY-debug"), "{debug}");
    let quiet = logged(Some("gateway=warn"));
    assert!(!quiet.contains("GATEWAY-info"), "{quiet}");
}
