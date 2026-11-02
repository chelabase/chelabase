//! m2's routes (#115): email codes for verify, reset and invite, and the SMTP status. Kept
//! together because they are built from m2's design sketch and will be reconciled with its
//! `openapi.json`. Assumed, in one place:
//!
//! - `POST /pools/{pool}/codes` takes `{user_id, kind, delivery}` and answers 201 with
//!   `{code?, expires_at?}` (`code` only for `delivery: "return"`).
//! - `POST /pools/{pool}/codes/verify` takes `{user_id, kind, code, new_password?}` and answers
//!   200 (body ignored). Refusals are `code_invalid` and `code_locked` (any 4xx status; the
//!   client reads the `error` string), and 409 `email_not_configured` on create.
//! - `GET /email/status` answers `{configured, reachable?}`.

use crate::client::Identity;
use crate::error::IdentityError;
use crate::ops;
use crate::types::Pool;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodeKind {
    /// Confirms the user's email.
    Verify,
    /// Lets the user set a new password. Valid 15 minutes.
    Reset,
    /// Lets an invitee set a first password. Valid 7 days.
    Invite,
}

impl CodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CodeKind::Verify => "verify",
            CodeKind::Reset => "reset",
            CodeKind::Invite => "invite",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// The service mails the code. `EmailNotConfigured` without SMTP.
    Email,
    /// The code comes back in the response and nothing is sent.
    Return,
}

impl Delivery {
    fn as_str(self) -> &'static str {
        match self {
            Delivery::Email => "email",
            Delivery::Return => "return",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IssuedCode {
    /// Present only for [`Delivery::Return`].
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct EmailStatus {
    pub configured: bool,
    /// Whether the SMTP server answered; only meaningful when configured.
    #[serde(default)]
    pub reachable: bool,
}

impl Identity {
    /// A new code for the user, replacing a pending one of the same kind.
    pub async fn create_code(
        &self,
        pool: &Pool,
        user_id: &str,
        kind: CodeKind,
        delivery: Delivery,
    ) -> Result<IssuedCode, IdentityError> {
        let body =
            json!({ "user_id": user_id, "kind": kind.as_str(), "delivery": delivery.as_str() });
        self.call(&ops::CREATE_CODE, Some(pool), None, &[], Some(body))
            .await
    }

    /// Uses the code once. `verify` marks the email verified; `reset` and `invite` set
    /// `new_password` and revoke the user's sessions.
    pub async fn verify_code(
        &self,
        pool: &Pool,
        user_id: &str,
        kind: CodeKind,
        code: &str,
        new_password: Option<&str>,
    ) -> Result<(), IdentityError> {
        let mut body = json!({ "user_id": user_id, "kind": kind.as_str(), "code": code });
        if let Some(password) = new_password {
            body["new_password"] = Value::String(password.to_string());
        }
        self.call_empty(&ops::VERIFY_CODE, Some(pool), None, Some(body))
            .await
    }

    /// Whether the service can send email (replaces Zitadel's `email_provider_active`).
    pub async fn email_status(&self) -> Result<EmailStatus, IdentityError> {
        self.call(&ops::EMAIL_STATUS, None, None, &[], None).await
    }
}
