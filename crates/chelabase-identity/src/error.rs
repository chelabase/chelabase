//! The service's refusals, typed. The codes come from `services/identity/src/http.ts` and
//! `routes.ts` (m1); `invalid_code`, `email_not_configured` and
//! `email_send_failed` are m2's.

use serde::Deserialize;

/// What a 404 said is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    Pool,
    User,
    Session,
    /// The route itself: the service is older than this client.
    Route,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentityError {
    #[error("not found: {0:?}")]
    NotFound(Missing),
    /// The email (or another unique value) is taken.
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("user is banned")]
    Banned,
    /// Wrong email or password. Never says which.
    #[error("bad credentials")]
    BadCredentials,
    /// The session was revoked while it was being made.
    #[error("session revoked")]
    SessionRevoked,
    /// The code is wrong, expired, used, replaced or locked out (5 wrong guesses), or the user
    /// is unknown. The service answers all of them alike (`invalid_code`), so neither can this.
    #[error("code invalid")]
    CodeInvalid,
    /// `delivery: "email"` was asked for and no SMTP is configured.
    #[error("email is not configured")]
    EmailNotConfigured,
    /// The service refused the input (400, 413): `code` is `invalid_request`, `invalid_pool`,
    /// `invalid_email`, `weak_password`, `bad_request` or `body_too_large`.
    #[error("invalid ({code}): {message}")]
    Invalid { code: String, message: String },
    /// The caller's token is missing or unknown.
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    /// The caller's token may not call this route.
    #[error("forbidden: {0}")]
    Forbidden(String),
    /// A 4xx the client has no name for (a proxy's page, a code from a newer service).
    #[error("rejected ({status} {code}): {message}")]
    Rejected {
        status: u16,
        code: String,
        message: String,
    },
    /// No answer, a timeout, a 5xx, a redirect or an unreadable answer.
    #[error("identity unavailable: {0}")]
    Unavailable(String),
    /// The SMTP server refused the message (502 `email_send_failed`). The service answered, so
    /// this is not `Unavailable`; the caller decides whether to try again.
    #[error("email send failed: {0}")]
    EmailSendFailed(String),
    /// The client could not be set up (token file, URL).
    #[error("identity client config: {0}")]
    Config(String),
}

impl IdentityError {
    /// Only a failure to get an answer is worth retrying; a 4xx is the service's decision.
    pub fn is_retryable(&self) -> bool {
        matches!(self, IdentityError::Unavailable(_))
    }
}

#[derive(Deserialize)]
struct Body {
    error: String,
    #[serde(default)]
    message: String,
}

/// The error a response with this status and body stands for, or `None` for a 2xx.
/// A 3xx is `Unavailable`: redirects are never followed, so it is a failed call.
pub fn classify_status(status: u16, body: &str) -> Option<IdentityError> {
    if (200..300).contains(&status) {
        return None;
    }
    if status == 502
        && let Ok(Body { error, message }) = serde_json::from_str::<Body>(body)
        && error == "email_send_failed"
    {
        return Some(IdentityError::EmailSendFailed(message));
    }
    if !(400..500).contains(&status) {
        return Some(IdentityError::Unavailable(status.to_string()));
    }
    let Ok(Body {
        error: code,
        message,
    }) = serde_json::from_str::<Body>(body)
    else {
        return Some(IdentityError::Rejected {
            status,
            code: String::new(),
            message: body.to_string(),
        });
    };
    use IdentityError as E;
    Some(match code.as_str() {
        "pool_not_found" => E::NotFound(Missing::Pool),
        "user_not_found" => E::NotFound(Missing::User),
        "session_not_found" => E::NotFound(Missing::Session),
        "not_found" => E::NotFound(Missing::Route),
        "conflict" => E::Conflict(message),
        "email_not_configured" => E::EmailNotConfigured,
        "banned" => E::Banned,
        "bad_credentials" => E::BadCredentials,
        "session_revoked" => E::SessionRevoked,
        "invalid_code" => E::CodeInvalid,
        "unauthorized" => E::Unauthorized(message),
        "forbidden" => E::Forbidden(message),
        "invalid_request" | "invalid_pool" | "invalid_email" | "weak_password" | "bad_request"
        | "body_too_large" => E::Invalid { code, message },
        _ => E::Rejected {
            status,
            code,
            message,
        },
    })
}
