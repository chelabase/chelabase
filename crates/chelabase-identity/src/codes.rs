//! m2's routes: email codes for verify, reset and invite, and the SMTP status. Kept together
//! (see `services/identity/src/routes/codes.ts` and `email/codes.ts`).
//!
//! - A code is six digits, bound to (pool, user, kind). Kinds never stand in for each other.
//! - An `EmailChange` code is mailed to the requested new address and bound to the current
//!   address and that new one: it works only for that new address, and only while the current
//!   address is unchanged. Confirming it swaps the email in, verified, and notifies the old one.
//! - Every real change of a user's email (`set_email`, `set_email_notifying`, or a confirmed
//!   `EmailChange`) revokes all of that user's outstanding codes, so a code never comes back
//!   when the address changes back to the one it was sent to.
//! - TTL: 15 minutes for verify, reset and email change, 72 hours for invite. A new code replaces the old one
//!   and resets the attempts. A code allows 5 wrong guesses; after that every guess is refused.
//! - A live verify, reset or email change code issued less than 60 seconds ago is not replaced:
//!   [`IdentityError::CodeRecentlySent`](crate::IdentityError::CodeRecentlySent), nothing is
//!   sent and the earlier code keeps working. Invites are always replaced.
//! - Each (user, kind) has a stored failure budget: 10 wrong guesses in 24 hours, across codes.
//!   A new code doesn't refill it; a right code clears it. Once spent, every guess of that kind
//!   is [`IdentityError::CodeBudgetSpent`](crate::IdentityError::CodeBudgetSpent), unchecked,
//!   until the window ends.
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
#[serde(rename_all = "snake_case")]
pub enum CodeKind {
    /// Confirms the user's email.
    Verify,
    /// Lets the user set a new password.
    Reset,
    /// Lets an invitee set a first password. The only kind the orchestrator may create.
    Invite,
    /// Proves a new address before it replaces the user's email. Auth only; issued with
    /// [`Identity::create_email_change_code`], used with [`Identity::confirm_email_change`].
    EmailChange,
}

impl CodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CodeKind::Verify => "verify",
            CodeKind::Reset => "reset",
            CodeKind::Invite => "invite",
            CodeKind::EmailChange => "email_change",
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
    /// create only `Invite` codes (`Forbidden` otherwise). `CodeRecentlySent` while a live
    /// code of that kind (not `Invite`) issued less than a minute ago stands.
    pub async fn create_code(
        &self,
        pool: &Pool,
        request: &CodeRequest<'_>,
    ) -> Result<IssuedCode, IdentityError> {
        self.post_code(pool, Self::code_body(request)).await
    }

    fn code_body(request: &CodeRequest<'_>) -> Value {
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
        body
    }

    async fn post_code(&self, pool: &Pool, body: Value) -> Result<IssuedCode, IdentityError> {
        self.call(&ops::CREATE_CODE, Some(pool), None, &[], Some(body))
            .await
    }

    /// Mails an `EmailChange` code to `new_email` (never returned, never sent to the current
    /// address). Nothing about the user changes yet. `Conflict` when another user holds
    /// `new_email` or it is already the user's; `EmailNotConfigured` without SMTP; `Forbidden`
    /// for any caller but auth; `CodeRecentlySent` as for [`Identity::create_code`].
    pub async fn create_email_change_code(
        &self,
        pool: &Pool,
        user_id: &str,
        new_email: &str,
        project_name: Option<&str>,
    ) -> Result<IssuedCode, IdentityError> {
        let mut request = CodeRequest::new(user_id, CodeKind::EmailChange, Delivery::Email);
        request.project_name = project_name;
        let mut body = Self::code_body(&request);
        body["new_email"] = Value::String(new_email.to_string());
        self.post_code(pool, body).await
    }

    /// Uses an `EmailChange` code issued for `new_email`: the user's email becomes `new_email`,
    /// verified, and the previous address gets a notice (when SMTP is configured). Another
    /// address, a wrong code or a current address changed since the code was issued is
    /// `CodeInvalid`; `new_email` taken by another user since then is `Conflict` (the code is
    /// spent); a spent failure budget is `CodeBudgetSpent`.
    pub async fn confirm_email_change(
        &self,
        pool: &Pool,
        user_id: &str,
        new_email: &str,
        code: &str,
    ) -> Result<User, IdentityError> {
        let body = json!({
            "user_id": user_id,
            "kind": CodeKind::EmailChange.as_str(),
            "code": code,
            "new_email": new_email,
        });
        self.post_verify(pool, body).await
    }

    /// Uses the code once and returns the updated user. `Verify` marks the email verified and
    /// takes no `new_password`. `Reset` and `Invite` require it: they set it, mark the email
    /// verified and revoke the user's sessions. A password out of range is
    /// `Invalid { code: "weak_password" }` and costs no attempt. A spent failure budget for
    /// the kind is `CodeBudgetSpent`: the code is not checked.
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
        self.post_verify(pool, body).await
    }

    async fn post_verify(&self, pool: &Pool, body: Value) -> Result<User, IdentityError> {
        let out: UserBody = self
            .call(&ops::VERIFY_CODE, Some(pool), None, &[], Some(body))
            .await?;
        Ok(out.user)
    }

    /// Tells an existing account it was invited: a mail with `link` (the invite's ticket) and no
    /// code, so it can set nothing. Members pool only; a `link` holding a `code` is refused.
    pub async fn send_invite_notice(
        &self,
        pool: &Pool,
        user_id: &str,
        project_name: Option<&str>,
        link: &str,
    ) -> Result<(), IdentityError> {
        let mut body = json!({ "link": link });
        if let Some(name) = project_name {
            body["project_name"] = Value::String(name.to_string());
        }
        self.call_empty(&ops::SEND_INVITE_NOTICE, Some(pool), Some(user_id), Some(body))
            .await
    }

    /// Whether the service can send email.
    pub async fn email_status(&self) -> Result<EmailStatus, IdentityError> {
        self.call(&ops::EMAIL_STATUS, None, None, &[], None).await
    }
}
