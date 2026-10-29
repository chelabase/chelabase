//! Fork round 4, final fix round (F2, part 4): compute_ctl's error body is capped at 512 bytes
//! in our errors (cut on a character boundary, never more), a symlink loop under a client
//! pair's `current` reads as absent, and `capabilities: null` is no capability. The pinned
//! names (`ERROR_BODY_LIMIT`, `scram::is_verifier`) are in `f2_scram.rs`.

// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::net::SocketAddr;
use std::time::SystemTime;

use axum::Router;
use axum::http::StatusCode;
use chelabase_neon::ComputeSigner;
use chelabase_neon::compute_tls::{ComputeCa, ORCHESTRATOR_CN, REALTIME_CN, renew_client_pair};
use chelabase_neon::jwt::generate_keypair;
use chelabase_neon::reconfigure::{
    ComputeCtlReport, ComputeCtlStatus, push, push_storage_token, status_report,
};
use serde_json::{Value, json};

fn signer() -> ComputeSigner {
    let (pem, x) = generate_keypair();
    ComputeSigner::new(&pem, &x).unwrap()
}

async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// A compute_ctl that answers every route with 500 and `body`.
async fn failing_compute_ctl(body: String) -> SocketAddr {
    serve(Router::new().fallback(move || {
        let body = body.clone();
        async move { (StatusCode::INTERNAL_SERVER_ERROR, body) }
    }))
    .await
}

/// The text of each of our three compute_ctl calls' errors against `addr`.
async fn errors(addr: SocketAddr) -> Vec<(&'static str, String)> {
    let http = chelabase_http::client_builder().build().unwrap();
    let s = signer();
    let config = json!({"spec": {}, "compute_ctl_config": {}});
    vec![
        (
            "push",
            format!(
                "{:#}",
                push(&http, addr, &s, "ep-a", &config).await.unwrap_err()
            ),
        ),
        (
            "status_report",
            format!(
                "{:#}",
                status_report(&http, addr, &s, "ep-a").await.unwrap_err()
            ),
        ),
        (
            "push_storage_token",
            format!(
                "{:#}",
                push_storage_token(&http, addr, &s, "ep-a", "t")
                    .await
                    .unwrap_err()
            ),
        ),
    ]
}

// A compute_ctl (or whatever answers at its address) can send any amount of text: our error
// carries its first 512 bytes, never the rest.
#[tokio::test]
async fn compute_ctls_error_body_is_capped_at_512_bytes() {
    let body = format!("{}TAIL-MARKER", "A".repeat(4096));
    let addr = failing_compute_ctl(body).await;
    for (call, text) in errors(addr).await {
        assert!(text.contains("500"), "{call}: the status stays: {text}");
        assert!(
            text.contains(&"A".repeat(512)),
            "{call}: the first 512 bytes stay: {text}"
        );
        assert!(
            !text.contains(&"A".repeat(513)),
            "{call}: at most 512 bytes of the body"
        );
        assert!(!text.contains("TAIL-MARKER"), "{call}: the rest is dropped");
        assert!(text.len() < 1024, "{call}: {} bytes", text.len());
    }
}

// The cut never splits a character: 256 two-byte characters are exactly 512 bytes.
#[tokio::test]
async fn the_cap_keeps_whole_characters() {
    let addr = failing_compute_ctl("é".repeat(400)).await;
    for (call, text) in errors(addr).await {
        assert!(text.contains(&"é".repeat(256)), "{call}: {text}");
        assert!(!text.contains(&"é".repeat(257)), "{call}: {text}");
    }
    // 511 bytes, then a two-byte character across the limit: it is dropped whole.
    let addr = failing_compute_ctl(format!("{}é{}", "B".repeat(511), "Q".repeat(100))).await;
    for (call, text) in errors(addr).await {
        assert!(text.contains(&"B".repeat(511)), "{call}: {text}");
        assert!(!text.contains('é') && !text.contains('Q'), "{call}: {text}");
    }
}

// A body within the limit is kept whole.
#[tokio::test]
async fn a_short_error_body_is_kept_whole() {
    let body = format!("{}!", "D".repeat(511));
    let addr = failing_compute_ctl(body.clone()).await;
    for (call, text) in errors(addr).await {
        assert!(text.contains(&body), "{call}: {text}");
    }
}

/// A compute_ctl whose `/status` answers 200 with `body`.
async fn status_compute(body: Value) -> SocketAddr {
    serve(Router::new().route(
        "/status",
        axum::routing::get(move || {
            let body = body.clone();
            async move { axum::Json(body) }
        }),
    ))
    .await
}

// `"capabilities": null` (a compute_ctl that serialises an empty list as null) is no
// capability, not an unreadable status.
#[tokio::test]
async fn null_capabilities_are_no_capability() {
    for status in ["running", "configuration_pending"] {
        let addr = status_compute(json!({
            "status": status, "start_time": "2026-10-21T00:00:00Z", "error": null,
            "capabilities": null,
        }))
        .await;
        let report = status_report(
            &chelabase_http::client_builder().build().unwrap(),
            addr,
            &signer(),
            "ep-a",
        )
        .await
        .unwrap_or_else(|e| panic!("{status}: {e:#}"));
        let want = match status {
            "running" => ComputeCtlStatus::Running,
            other => ComputeCtlStatus::Other(other.into()),
        };
        assert_eq!(
            report,
            ComputeCtlReport {
                status: want,
                storage_token_refresh: false
            }
        );
    }
}

// A symlink loop where `current` should be (`current -> current`, or two links pointing at each
// other) is a missing pair: a new one is written, never an error that stops the sweep.
#[test]
fn a_symlink_loop_under_current_is_a_missing_pair() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    for (cn, two_links) in [
        (REALTIME_CN, false),
        (REALTIME_CN, true),
        (ORCHESTRATOR_CN, false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        if two_links {
            std::os::unix::fs::symlink("other", dir.path().join("current")).unwrap();
            std::os::unix::fs::symlink("current", dir.path().join("other")).unwrap();
        } else {
            std::os::unix::fs::symlink("current", dir.path().join("current")).unwrap();
        }
        let wrote = renew_client_pair(dir.path(), &ca, cn, now)
            .unwrap_or_else(|e| panic!("{cn} (two links {two_links}): {e:#}"));
        assert!(wrote, "{cn} (two links {two_links}): a pair is written");
        for file in ["client.crt", "client.key", "ca.crt"] {
            assert!(
                dir.path().join("current").join(file).is_file(),
                "{cn} (two links {two_links}): {file}"
            );
        }
        assert!(
            !renew_client_pair(dir.path(), &ca, cn, now).unwrap(),
            "{cn} (two links {two_links}): the new pair is current"
        );
    }
}
