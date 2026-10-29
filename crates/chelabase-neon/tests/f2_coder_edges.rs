//! Fork round 4, final fix round (F2): the coder's edge cases beyond the RED set: the error-body
//! cap on bodies that aren't text or are empty, and verifier spellings we refuse.

// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods)]

use std::net::SocketAddr;

use axum::Router;
use axum::http::StatusCode;
use chelabase_neon::ComputeSigner;
use chelabase_neon::jwt::generate_keypair;
use chelabase_neon::reconfigure::{ERROR_BODY_LIMIT, status_report};
use chelabase_neon::scram::is_verifier;

async fn failing(body: Vec<u8>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new().fallback(move || {
        let body = body.clone();
        async move { (StatusCode::BAD_GATEWAY, body) }
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

async fn status_error(addr: SocketAddr) -> String {
    let (pem, x) = generate_keypair();
    let signer = ComputeSigner::new(&pem, &x).unwrap();
    format!(
        "{:#}",
        status_report(&chelabase_http::client_builder().build().unwrap(), addr, &signer, "ep-a")
            .await
            .unwrap_err()
    )
}

// A body that isn't UTF-8 is still capped: at most the limit, whatever the replacement
// characters take.
#[tokio::test]
async fn a_binary_error_body_is_capped_too() {
    let addr = failing(vec![0xFF; 4096]).await;
    let text = status_error(addr).await;
    assert!(text.contains("502"), "{text}");
    assert!(text.len() < ERROR_BODY_LIMIT + 200, "{} bytes", text.len());
}

// An empty body leaves the status alone.
#[tokio::test]
async fn an_empty_error_body_keeps_the_status() {
    let addr = failing(Vec::new()).await;
    let text = status_error(addr).await;
    assert!(text.contains("502"), "{text}");
}

// F2 round 2: control characters (newlines, escapes, NUL) in the body are escaped, so the body
// can't forge log lines or drive a terminal; the cap still holds on the escaped text.
#[tokio::test]
async fn control_characters_in_the_error_body_are_escaped() {
    let body = "first\nforged: line\r\x1b[31mred\x00\tend".as_bytes().to_vec();
    let text = status_error(failing(body).await).await;
    assert!(
        !text.chars().any(|c| c.is_control()),
        "a control character reached the error: {text:?}"
    );
    assert!(text.contains("first\\nforged: line\\r"), "{text:?}");
    assert!(text.contains("\\u{1b}[31mred"), "{text:?}");
    let many = status_error(failing(vec![b'\n'; 4096]).await).await;
    assert!(many.len() < ERROR_BODY_LIMIT + 200, "{} bytes", many.len());
    assert!(!many.contains('\n'));
}

#[test]
fn odd_verifier_spellings_are_refused() {
    let pencil = "SCRAM-SHA-256$4096:W22ZaJ0SNY7soEsUEjb6gQ==$WG5d8oPm3OtcPnkdi4Uo7BkeZkBFzpcXkuLmtbsT4qY=:wfPLwcE6nTWhTAmQ7tl2KeoiWGPlZqQxSrmfPwDl2dU=";
    assert!(is_verifier(pencil));
    for bad in [
        // A sign (stricter than Postgres' strtol, on purpose) and an int overflow.
        pencil.replace("$4096:", "$+4096:"),
        pencil.replace("$4096:", "$99999999999:"),
        // URL-safe base64 isn't Postgres' alphabet.
        pencil.replace("W22ZaJ0SNY7soEsUEjb6gQ==", "W22ZaJ0SNY7soEsUEjb6g-=="),
        // Unpadded keys.
        pencil.replace("qY=:", "qY:"),
        // An extra `$` segment between the parts.
        pencil.replace("$WG5", "$$WG5"),
    ] {
        assert!(!is_verifier(&bad), "{bad}");
    }
    assert!(is_verifier(&pencil.replace("$4096:", "$04096:")), "digits only, value positive");
}
