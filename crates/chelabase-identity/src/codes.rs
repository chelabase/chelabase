//! m2's routes: email codes for verify, reset and invite, and the SMTP status. Kept together
//! (see `services/identity/src/routes/codes.ts` and `email/codes.ts`).
//!
//! - A code is six digits, bound to (pool, user, kind). Kinds never stand in for each other.
//! - TTL: 15 minutes for verify and reset, 7 days for invite. A new code replaces the old one
//!   and resets the attempts. A code allows 5 wrong guesses; after that every guess is refused.
//! - Any wrong, expired, used, replaced or locked code, or an unknown user, is
//!   [`IdentityError::CodeInvalid`](crate::IdentityError::CodeInvalid).

use crate::client::Identity;
use crate::error::IdentityError;
use crate::ops;
use crate::types::{Pool, User};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CodeKind {
    /// Confirms the user's email.
    Verify,
    /// Lets the user set a new password.
    Reset,
    /// Lets an invitee set a first password. The only kind the orchestrator may create.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    /// The service mails the code. `EmailNotConfigured` without SMTP, `EmailSendFailed` when
    /// the SMTP server refuses.
    Email,
    /// The code comes back in the response and nothing is sent (link delivery).
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

/// What to issue, and how the email reads.
#[derive(Debug, Clone)]
pub struct CodeRequest<'a> {
    pub user_id: &'a str,
    pub kind: CodeKind,
    pub delivery: Delivery,
    /// Shown in the email; single line, at most 128 characters. The service defaults to `Chelabase`.
    pub project_name: Option<&'a str>,
    /// An absolute http(s) URL where the code is entered, shown in the email. Never carries the code.
    pub link: Option<&'a str>,
}

impl<'a> CodeRequest<'a> {
    pub fn new(user_id: &'a str, kind: CodeKind, delivery: Delivery) -> Self {
        Self {
            user_id,
            kind,
            delivery,
            project_name: None,
            link: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IssuedCode {
    pub kind: CodeKind,
    pub delivery: Delivery,
    pub expires_at: DateTime<Utc>,
    /// Present only for [`Delivery::Return`].
    #[serde(default)]
    pub code: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct EmailStatus {
    pub configured: bool,
    /// Whether the SMTP server answered; only meaningful when configured.
    pub reachable: bool,
}

#[derive(Deserialize)]
struct UserBody {
    user: User,
}

impl Identity {
    /// A new code for the user, replacing a pending one of the same kind. The orchestrator may
    /// create only `Invite` codes (`Forbidden` otherwise).
    pub async fn create_code(
        &self,
        pool: &Pool,
        request: &CodeRequest<'_>,
    ) -> Result<IssuedCode, IdentityError> {
        let mut body = json!({
            "user_id": request.user_id,
            "kind": request.kind.as_str(),
            "delivery": request.delivery.as_str(),
        });
        if let Some(name) = request.project_name {
            body["project_name"] = Value::String(name.to_string());
        }
        if let Some(link) = request.link {
            body["link"] = Value::String(link.to_string());
        }
        self.call(&ops::CREATE_CODE, Some(pool), None, &[], Some(body))
            .await
    }

    /// Uses the code once and returns the updated user. `Verify` marks the email verified and
    /// takes no `new_password`. `Reset` and `Invite` require it: they set it, mark the email
    /// verified and revoke the user's sessions. A password out of range is
    /// `Invalid { code: "weak_password" }` and costs no attempt.
    pub async fn verify_code(
        &self,
        pool: &Pool,
        user_id: &str,
        kind: CodeKind,
        code: &str,
        new_password: Option<&str>,
    ) -> Result<User, IdentityError> {
        let mut body = json!({ "user_id": user_id, "kind": kind.as_str(), "code": code });
        if let Some(password) = new_password {
            body["new_password"] = Value::String(password.to_string());
        }
        let out: UserBody = self
            .call(&ops::VERIFY_CODE, Some(pool), None, &[], Some(body))
            .await?;
        Ok(out.user)
    }

    /// Whether the service can send email (replaces Zitadel's `email_provider_active`).
    pub async fn email_status(&self) -> Result<EmailStatus, IdentityError> {
        self.call(&ops::EMAIL_STATUS, None, None, &[], None).await
    }
}
