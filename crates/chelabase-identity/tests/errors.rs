//! How the client reads an answer: AC 2 (5xx and connection errors are retryable, a 4xx never).

use chelabase_identity::classify_status as classify;
use chelabase_identity::{Identity, IdentityError, Missing, Pool};

fn body(code: &str) -> String {
    format!(r#"{{"error":"{code}","message":"m"}}"#)
}

#[test]
fn classify_status() {
    use IdentityError as E;
    let m = || "m".to_string();
    let table: Vec<(u16, String, E)> = vec![
        (404, body("pool_not_found"), E::NotFound(Missing::Pool)),
        (404, body("user_not_found"), E::NotFound(Missing::User)),
        (
            404,
            body("session_not_found"),
            E::NotFound(Missing::Session),
        ),
        (404, body("not_found"), E::NotFound(Missing::Route)),
        (409, body("conflict"), E::Conflict(m())),
        (409, body("invite_refused"), E::InviteRefused(m())),
        (409, body("email_not_configured"), E::EmailNotConfigured),
        (403, body("banned"), E::Banned),
        (403, body("forbidden"), E::Forbidden(m())),
        (401, body("bad_credentials"), E::BadCredentials),
        (401, body("session_revoked"), E::SessionRevoked),
        (401, body("unauthorized"), E::Unauthorized(m())),
        (400, body("invalid_code"), E::CodeInvalid),
        (
            400,
            body("invalid_request"),
            E::Invalid {
                code: "invalid_request".into(),
                message: m(),
            },
        ),
        (
            400,
            body("invalid_pool"),
            E::Invalid {
                code: "invalid_pool".into(),
                message: m(),
            },
        ),
        (
            400,
            body("invalid_email"),
            E::Invalid {
                code: "invalid_email".into(),
                message: m(),
            },
        ),
        (
            400,
            body("weak_password"),
            E::Invalid {
                code: "weak_password".into(),
                message: m(),
            },
        ),
        (
            400,
            body("bad_request"),
            E::Invalid {
                code: "bad_request".into(),
                message: m(),
            },
        ),
        (
            413,
            body("body_too_large"),
            E::Invalid {
                code: "body_too_large".into(),
                message: m(),
            },
        ),
        // A 4xx that is not the service's own answer (a proxy page, an unknown code).
        (
            404,
            "<html>nope</html>".into(),
            E::Rejected {
                status: 404,
                code: String::new(),
                message: "<html>nope</html>".into(),
            },
        ),
        (
            400,
            body("brand_new"),
            E::Rejected {
                status: 400,
                code: "brand_new".into(),
                message: m(),
            },
        ),
        (
            429,
            String::new(),
            E::Rejected {
                status: 429,
                code: String::new(),
                message: String::new(),
            },
        ),
        // The service failed, or never answered: retry.
        (502, body("email_send_failed"), E::EmailSendFailed(m())),
        (500, body("internal"), E::Unavailable("500".into())),
        (
            502,
            "<html>bad gateway</html>".into(),
            E::Unavailable("502".into()),
        ),
        (503, String::new(), E::Unavailable("503".into())),
        // A redirect is never followed, so it is a failed call.
        (302, String::new(), E::Unavailable("302".into())),
    ];
    for (status, text, want) in table {
        let got = classify(status, &text).unwrap_or_else(|| panic!("{status} {text}: no error"));
        // Unavailable carries a detail line; compare the variant for it.
        match (&got, &want) {
            (E::Unavailable(_), E::Unavailable(_)) => {}
            _ => assert_eq!(got, want, "{status} {text}"),
        }
        assert_eq!(
            got.is_retryable(),
            matches!(want, E::Unavailable(_)),
            "{status} {text}"
        );
        if (400..500).contains(&status) {
            assert!(!got.is_retryable(), "a {status} is never retryable");
        }
    }
    for ok in [200, 201, 204] {
        assert_eq!(classify(ok, "{}"), None);
    }
}

#[tokio::test]
async fn connection_error_is_unavailable() {
    // Nothing listens on port 1.
    let id = Identity::new("http://127.0.0.1:1", "t");
    let err = id.get_user(&Pool::Members, "u1").await.unwrap_err();
    assert!(matches!(err, IdentityError::Unavailable(_)), "{err:?}");
    assert!(err.is_retryable());
}

#[tokio::test]
async fn server_failure_is_unavailable() {
    let fake = chelabase_identity::mock::FakeIdentity::start().await;
    fake.set_down(true);
    let err = fake
        .client()
        .get_user(&Pool::Members, "u1")
        .await
        .unwrap_err();
    assert!(matches!(err, IdentityError::Unavailable(_)), "{err:?}");
    assert!(err.is_retryable());
}

#[tokio::test]
async fn missing_or_empty_token_is_unauthorized() {
    let fake = chelabase_identity::mock::FakeIdentity::start().await;
    let err = Identity::new(&fake.base, "")
        .get_user(&Pool::Members, "u1")
        .await
        .unwrap_err();
    assert!(matches!(err, IdentityError::Unauthorized(_)), "{err:?}");
}

#[test]
fn token_file_is_read_and_trimmed() {
    let dir = std::env::temp_dir().join(format!("identity-token-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("t");
    std::fs::write(&file, "secret\n").unwrap();
    assert!(Identity::from_token_file("http://x", &file).is_ok());
    std::fs::write(&file, "\n").unwrap();
    assert!(matches!(
        Identity::from_token_file("http://x", &file),
        Err(IdentityError::Config(_))
    ));
    assert!(matches!(
        Identity::from_token_file("http://x", &dir.join("none")),
        Err(IdentityError::Config(_))
    ));
    std::fs::remove_dir_all(dir).unwrap();
}
