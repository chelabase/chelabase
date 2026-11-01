//! Zitadel v4's Session, User, Organization and OIDC v2 REST APIs.

use crate::refusals::{self, Refusal};
use chrono::{DateTime, Utc};
use reqwest::Method;
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ZError {
    /// Zitadel understood and refused (a 4xx carrying its own error body). `code` is the gRPC
    /// code and `id` the first error detail's id (`QUERY-Dfbg2`): both stable, so match on
    /// them (see [`crate::refusals`]). `id` is `None` only for Zitadel's request validation
    /// ([`ZError::is_invalid_request`]). `message` is the error text (localized, then each
    /// detail's id and message), logged and never shown to users.
    #[error("zitadel rejected the request ({status}): {message}")]
    Rejected {
        status: u16,
        code: Option<u32>,
        id: Option<String>,
        message: String,
    },
    /// No answer, a timeout, a 5xx, an unreadable answer, or a 4xx whose body isn't Zitadel's
    /// own error (a proxy's page, an empty body): it never reached Zitadel's logic.
    /// Zitadel's request validation (a 400 with code 3 and no detail) is `Rejected` with no id.
    #[error("zitadel unavailable: {0}")]
    Unavailable(String),
}

impl ZError {
    /// The gRPC code of Zitadel's refusal.
    pub fn zitadel_code(&self) -> Option<u32> {
        match self {
            ZError::Rejected { code, .. } => *code,
            ZError::Unavailable(_) => None,
        }
    }

    /// The id of the refusal's first error detail (`COMMAND-bd4ir1mblj`).
    pub fn detail_id(&self) -> Option<&str> {
        match self {
            ZError::Rejected { id, .. } => id.as_deref(),
            ZError::Unavailable(_) => None,
        }
    }

    /// Whether this is exactly that captured refusal: its status, code and detail id.
    pub fn is(&self, refusal: Refusal) -> bool {
        matches!(self, ZError::Rejected { status, code, id, .. }
            if *status == refusal.status
                && *code == Some(refusal.code)
                && id.as_deref() == Some(refusal.id))
    }

    /// Whether this is Zitadel's request validation refusing a field (a password over 200
    /// characters): a 400 with gRPC code 3 and no detail id (see [`crate::refusals`]).
    pub fn is_invalid_request(&self) -> bool {
        matches!(
            self,
            ZError::Rejected {
                status: 400,
                code: Some(refusals::INVALID_ARGUMENT),
                id: None,
                ..
            }
        )
    }

    /// Whether this is one of those captured refusals.
    pub fn is_any(&self, refusals: &[Refusal]) -> bool {
        refusals.iter().any(|r| self.is(*r))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: String,
    pub token: String,
}

pub struct NewUser<'a> {
    pub org_id: &'a str,
    pub email: &'a str,
    pub given_name: &'a str,
    pub family_name: &'a str,
    /// `None` makes a user that cannot sign in with a password.
    pub password: Option<&'a str>,
    /// Caller-chosen Zitadel user id (sent as `userId`).
    pub user_id: Option<&'a str>,
    pub username: Option<&'a str>,
    /// With `!verified`, ask Zitadel to email the verification code.
    pub send_code: bool,
    /// Create the email already verified (the default; see `CHELABASE_REQUIRE_EMAIL_VERIFICATION`).
    pub verified: bool,
    pub url_template: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserState {
    Active,
    Inactive,
    Locked,
    /// `USER_STATE_INITIAL`: made by the v1 management API with neither a password nor a
    /// verified email, and never signed in (see [`crate::refusals`]).
    Initial,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZUser {
    pub id: String,
    pub org_id: String,
    pub email: String,
    pub email_verified: bool,
    /// `human.profile.displayName`; empty when unset.
    pub display_name: String,
    pub state: UserState,
    pub created_at: Option<DateTime<Utc>>,
    /// Whether the user ever set a password (`human.passwordChanged`): an invitee who never
    /// accepted has none.
    pub has_password: bool,
}

/// What an org's email lookup found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmailMatch {
    None,
    One(ZUser),
    /// More than one user has the email (made outside Chelabase's checks): it is taken, and no
    /// single user signs in with it.
    Many,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPage {
    pub users: Vec<ZUser>,
    pub next_offset: Option<u64>,
}

/// Sessions per `sessions/search` page.
const SESSION_PAGE: u32 = 100;

pub struct Zitadel {
    http: chelabase_http::Client,
    base: String,
    /// The `Host` every call carries, when `base` is Zitadel's in-network address (#24).
    host: Option<String>,
    token: String,
}

fn str_at(value: &Value, pointer: &str) -> Result<String, ZError> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ZError::Unavailable(format!("answer without {pointer}")))
}

fn parse_user(v: &Value) -> Option<ZUser> {
    let details = v.get("details");
    Some(ZUser {
        id: v.get("userId")?.as_str()?.to_string(),
        org_id: details
            .and_then(|d| d.get("resourceOwner"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        email: v
            .pointer("/human/email/email")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        email_verified: v
            .pointer("/human/email/isVerified")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        display_name: v
            .pointer("/human/profile/displayName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        state: match v.get("state").and_then(Value::as_str) {
            Some("USER_STATE_ACTIVE") => UserState::Active,
            Some("USER_STATE_INACTIVE") => UserState::Inactive,
            Some("USER_STATE_LOCKED") => UserState::Locked,
            Some("USER_STATE_INITIAL") => UserState::Initial,
            _ => UserState::Other,
        },
        created_at: details
            .and_then(|d| d.get("creationDate"))
            .and_then(Value::as_str)
            .and_then(|t| t.parse().ok()),
        has_password: v
            .pointer("/human/passwordChanged")
            .is_some_and(|p| !p.is_null()),
    })
}

/// proto3 JSON writes uint64 as a string.
fn as_u64(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// An error body's text: Zitadel's `message` (localized, with the error id appended) followed
/// by each `details[]` entry's `id` and `message`, so the detail's id (`COMMAND-Wgn4q`) survives
/// in logs whatever the top-level message's language. Zitadel v4 sends no error key; callers
/// match the id ([`ZError::is`]), never this text.
fn error_text(body: Option<&Value>) -> String {
    let Some(body) = body else {
        return String::new();
    };
    let mut text = body
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    for d in body
        .get("details")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for key in ["id", "message"] {
            if let Some(v) = d.get(key).and_then(Value::as_str) {
                text.push_str(" | ");
                text.push_str(v);
            }
        }
    }
    text
}

/// The `@type`s of the `details[]` entries in Zitadel's own error answers: an `ErrorDetail`,
/// or a `CredentialsCheckError` for a wrong password (`tests/zitadel_live.rs`).
const ZITADEL_DETAILS: [&str; 2] = [
    "type.googleapis.com/zitadel.v1.ErrorDetail",
    "type.googleapis.com/zitadel.v1.CredentialsCheckError",
];

/// Zitadel's own error, read from an error body: its gRPC `code` and the `id` of the first
/// `details[]` entry of Zitadel's detail types. Both `message`s are localized (v4.19.1 answers a
/// missing user with `{"code":5, "message":"User could not be found (QUERY-Dfbg2)", "details":
/// [{"@type":"type.googleapis.com/zitadel.v1.ErrorDetail", "id":"QUERY-Dfbg2", "message":
/// "User could not be found"}]}`), so no message is read.
fn zitadel_error(body: Option<&Value>) -> Option<(u32, String)> {
    let body = body?;
    let code = u32::try_from(body.get("code")?.as_u64()?).ok()?;
    let id = body
        .get("details")?
        .as_array()?
        .iter()
        .find(|d| {
            d.get("@type")
                .and_then(Value::as_str)
                .is_some_and(|t| ZITADEL_DETAILS.contains(&t))
        })?
        .get("id")?
        .as_str()
        .filter(|id| !id.is_empty())?;
    Some((code, id.to_string()))
}

/// Whether a 400's body is Zitadel's request validation refusing a field (captured live, see
/// [`crate::refusals`]): gRPC code 3 as a number, a string `message`, and a `details` that is
/// missing or an empty array. It has no id.
fn invalid_request(status: u16, body: Option<&Value>) -> bool {
    let Some(body) = body else {
        return false;
    };
    status == 400
        && body.get("code").and_then(Value::as_u64) == Some(u64::from(refusals::INVALID_ARGUMENT))
        && body.get("message").is_some_and(Value::is_string)
        && body
            .get("details")
            .is_none_or(|d| d.as_array().is_some_and(Vec::is_empty))
}

/// What a 4xx answer means: Zitadel's own refusal is `Rejected` with its code and detail id,
/// or with no id for its request validation (a 400, code 3, no detail). Any other body (HTML,
/// empty, JSON without Zitadel's detail, such as a wrong `Host`'s "instance not found") is
/// `Unavailable`: it never reached Zitadel's logic, or can't be told apart from a proxy's answer.
fn refused(status: u16, body: Option<&Value>) -> ZError {
    if let Some((code, id)) = zitadel_error(body) {
        return ZError::Rejected {
            status,
            code: Some(code),
            id: Some(id),
            message: error_text(body),
        };
    }
    if invalid_request(status, body) {
        return ZError::Rejected {
            status,
            code: Some(refusals::INVALID_ARGUMENT),
            id: None,
            message: error_text(body),
        };
    }
    ZError::Unavailable(format!(
        "a {status} that isn't Zitadel's own error: {}",
        error_text(body)
    ))
}

/// How the client reads a 4xx answer with this body (for tests against captured answers).
#[cfg(any(test, feature = "mock"))]
pub fn classify(status: u16, body: &str) -> ZError {
    let parsed = if body.trim().is_empty() {
        Some(Value::Null)
    } else {
        serde_json::from_str::<Value>(body).ok()
    };
    refused(status, parsed.as_ref())
}

impl Zitadel {
    pub fn new(base: &str, token: &str) -> Self {
        Self {
            // #26: never a redirect. Zitadel answers its REST calls directly; a 3xx (a
            // misconfigured or compromised upstream) fails the call instead of sending the
            // token's request, with the forced Host, somewhere else.
            http: chelabase_http::client_builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("reqwest client"),
            base: base.trim_end_matches('/').to_string(),
            host: None,
            token: token.to_string(),
        }
    }

    /// Every call carries `host` as its `Host` (Zitadel picks its instance by Host): set when
    /// `base` is Zitadel's in-network address rather than the public auth URL (#24, see
    /// `chelabase_types::entry::zitadel_at`). `None` keeps the base URL's own.
    pub fn with_host(mut self, host: Option<&str>) -> Self {
        self.host = host.map(str::to_string);
        self
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, ZError> {
        let (status, parsed) = self.send(method, path, body).await?;
        if status.is_client_error() {
            return Err(refused(status.as_u16(), parsed.as_ref()));
        }
        let value = parsed.ok_or_else(|| ZError::Unavailable(format!("{status}: not JSON")))?;
        Ok(value)
    }

    /// One call: a 2xx or 4xx status and its body as JSON (`None` when it isn't JSON, `Null`
    /// when it's empty). No answer, a 5xx and a 3xx are `Unavailable`.
    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(reqwest::StatusCode, Option<Value>), ZError> {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token);
        if let Some(host) = &self.host {
            request = request.header(reqwest::header::HOST, host);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| ZError::Unavailable(e.to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| ZError::Unavailable(e.to_string()))?;
        if status.is_server_error() {
            return Err(ZError::Unavailable(format!("{status}")));
        }
        // #26: a 3xx (never followed) is a failed call, not an answer: its body may be empty,
        // which would otherwise read as success.
        if !status.is_success() && !status.is_client_error() {
            return Err(ZError::Unavailable(format!("{status}: Zitadel answered with no result (redirects are not followed)")));
        }
        // A proxy may answer a 4xx with HTML: `refused` tells it from Zitadel's own error.
        let parsed = if text.trim().is_empty() {
            Some(Value::Null)
        } else {
            serde_json::from_str::<Value>(&text).ok()
        };
        Ok((status, parsed))
    }

    /// Whether the pending sign-in asked for sign-up (`prompt=create`).
    pub async fn auth_request(&self, id: &str) -> Result<bool, ZError> {
        let answer = self
            .call(Method::GET, &format!("/v2/oidc/auth_requests/{id}"), None)
            .await?;
        Ok(answer
            .pointer("/authRequest/prompt")
            .and_then(Value::as_array)
            .is_some_and(|p| p.iter().any(|v| v == "PROMPT_CREATE")))
    }

    pub async fn find_user_by_email(
        &self,
        org_id: &str,
        email: &str,
    ) -> Result<Option<String>, ZError> {
        Ok(match self.find_user_in_org(org_id, email).await? {
            EmailMatch::One(u) => Some(u.id),
            EmailMatch::None | EmailMatch::Many => None,
        })
    }

    pub async fn email_verified(&self, user_id: &str) -> Result<bool, ZError> {
        let answer = self
            .call(Method::GET, &format!("/v2/users/{user_id}"), None)
            .await?;
        Ok(answer
            .pointer("/user/human/email/isVerified")
            .and_then(Value::as_bool)
            .unwrap_or(false))
    }

    pub async fn create_session(&self, user_id: &str, password: &str) -> Result<Session, ZError> {
        let body =
            json!({"checks": {"user": {"userId": user_id}, "password": {"password": password}}});
        let answer = self.call(Method::POST, "/v2/sessions", Some(body)).await?;
        Ok(Session {
            id: str_at(&answer, "/sessionId")?,
            token: str_at(&answer, "/sessionToken")?,
        })
    }

    pub async fn delete_session(&self, session: &Session) -> Result<(), ZError> {
        self.call(
            Method::DELETE,
            &format!("/v2/sessions/{}", session.id),
            Some(json!({"sessionToken": session.token})),
        )
        .await?;
        Ok(())
    }

    /// Finalizes the pending sign-in; the URL to send the browser to.
    pub async fn finish(&self, auth_request: &str, session: &Session) -> Result<String, ZError> {
        let body = json!({"session": {"sessionId": session.id, "sessionToken": session.token}});
        let answer = self
            .call(
                Method::POST,
                &format!("/v2/oidc/auth_requests/{auth_request}"),
                Some(body),
            )
            .await?;
        str_at(&answer, "/callbackUrl")
    }

    pub async fn create_user(&self, user: &NewUser<'_>) -> Result<String, ZError> {
        let email = if user.verified {
            json!({"email": user.email, "isVerified": true})
        } else if user.send_code {
            json!({"email": user.email, "sendCode": {"urlTemplate": user.url_template}})
        } else {
            json!({"email": user.email})
        };
        let mut human = json!({
            "profile": {"givenName": user.given_name, "familyName": user.family_name},
            "email": email,
        });
        if let Some(password) = user.password {
            human["password"] = json!({"password": password, "changeRequired": false});
        }
        let mut body = json!({"organizationId": user.org_id, "human": human});
        if let Some(id) = user.user_id {
            body["userId"] = json!(id);
        }
        if let Some(name) = user.username {
            body["username"] = json!(name);
        }
        let answer = self.call(Method::POST, "/v2/users/new", Some(body)).await?;
        str_at(&answer, "/id")
    }

    pub async fn verify_email(&self, user_id: &str, code: &str) -> Result<(), ZError> {
        self.call(
            Method::POST,
            &format!("/v2/users/{user_id}/email/verify"),
            Some(json!({"verificationCode": code})),
        )
        .await?;
        Ok(())
    }

    /// Checks an invite code (`POST /v2/users/{userId}/invite_code/verify`, body
    /// `{"verificationCode"}`). Zitadel answers a wrong, an expired and a missing code alike
    /// ([`refusals::INVITE_CODE_INVALID`]), and a deleted or unknown user with a 400
    /// ([`refusals::INVITE_USER_NOT_FOUND`]), not a 404: both captured by `tests/zitadel_live.rs`.
    pub async fn verify_invite_code(&self, user_id: &str, code: &str) -> Result<(), ZError> {
        self.call(
            Method::POST,
            &format!("/v2/users/{user_id}/invite_code/verify"),
            Some(json!({"verificationCode": code})),
        )
        .await?;
        Ok(())
    }

    /// A passwordless human user in `org_id` (an invitee): the username is the email, the
    /// names are its local part, and the email is created unverified with `returnCode`, so
    /// Zitadel sends no verification mail (the invite is the only one). Returns the user id.
    pub async fn create_user_without_password(
        &self,
        org_id: &str,
        email: &str,
    ) -> Result<String, ZError> {
        let local = email.split('@').next().filter(|l| !l.is_empty()).unwrap_or(email);
        let body = json!({
            "organizationId": org_id,
            "human": {
                "profile": {"givenName": local, "familyName": local, "displayName": local},
                "email": {"email": email, "returnCode": {}},
            },
        });
        let answer = self.call(Method::POST, "/v2/users/new", Some(body)).await?;
        str_at(&answer, "/id")
    }

    /// Sends the user an invite code (Zitadel v4.19.1 `POST /v2/users/{userId}/invite_code`,
    /// body `{"sendCode": {"urlTemplate", "applicationName"}}`). A new code replaces the old one.
    /// The template may use `{{.UserID}}`, `{{.OrgID}}` and `{{.Code}}` (200 characters at most).
    pub async fn create_invite_code(&self, user_id: &str, url_template: &str) -> Result<(), ZError> {
        self.call(
            Method::POST,
            &format!("/v2/users/{user_id}/invite_code"),
            Some(json!({"sendCode": {"urlTemplate": url_template, "applicationName": "Chelabase"}})),
        )
        .await?;
        Ok(())
    }

    /// Makes a new invite code and returns it instead of mailing it (Zitadel v4.19.1
    /// `POST /v2/users/{userId}/invite_code`, body `{"returnCode": {}}`, answering `inviteCode`).
    /// A new code replaces the old one. The caller builds the link and must never log it.
    pub async fn return_invite_code(&self, user_id: &str) -> Result<String, ZError> {
        let answer = self
            .call(
                Method::POST,
                &format!("/v2/users/{user_id}/invite_code"),
                Some(json!({"returnCode": {}})),
            )
            .await?;
        str_at(&answer, "/inviteCode")
    }

    /// Whether the user has any way to sign in: a password, a passkey, an OTP or U2F factor or an
    /// IdP link (Zitadel v4.19.1 `GET /v2/users/{userId}/authentication_methods`). No
    /// `domainQuery`: without one every method counts, passkeys and U2F of any domain (or none,
    /// added through the v1 APIs) included; a `domainQuery` needs a non-empty `domain`, so
    /// `includeWithoutDomain` alone is a 400.
    pub async fn has_auth_methods(&self, user_id: &str) -> Result<bool, ZError> {
        let answer = self
            .call(Method::GET, &format!("/v2/users/{user_id}/authentication_methods"), None)
            .await?;
        Ok(answer
            .get("authMethodTypes")
            .and_then(Value::as_array)
            .is_some_and(|types| !types.is_empty()))
    }

    /// Whether an email provider is active, so Zitadel can mail invites (Zitadel v4.19.1
    /// `POST /admin/v1/email/_search`, ListEmailProviders; the older `/admin/v1/smtp/_search`
    /// is deprecated). A provider's `state` is `EMAIL_PROVIDER_ACTIVE` or
    /// `EMAIL_PROVIDER_INACTIVE`; SMTP and HTTP providers both count. Needs the admin token.
    pub async fn email_provider_active(&self) -> Result<bool, ZError> {
        let answer = self
            .call(Method::POST, "/admin/v1/email/_search", Some(json!({})))
            .await?;
        Ok(answer
            .get("result")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|p| p.get("state").and_then(Value::as_str) == Some("EMAIL_PROVIDER_ACTIVE")))
    }

    /// Ends every session of the user (`POST /v2/sessions/search` with a `userIdQuery`, then a
    /// delete of each; one already gone is fine). The search is paged, so it repeats until a
    /// search comes back empty, or finds only sessions it already ended (a session that
    /// outlives its delete isn't retried forever). Returns how many were ended.
    pub async fn delete_user_sessions(&self, user_id: &str) -> Result<usize, ZError> {
        let mut ended = std::collections::HashSet::new();
        loop {
            let body = json!({
                "query": {"limit": SESSION_PAGE},
                "queries": [{"userIdQuery": {"id": user_id}}],
            });
            let answer = self.call(Method::POST, "/v2/sessions/search", Some(body)).await?;
            let ids: Vec<String> = answer
                .get("sessions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|s| s.get("id").and_then(Value::as_str).map(str::to_string))
                .filter(|id| !ended.contains(id))
                .collect();
            if ids.is_empty() {
                return Ok(ended.len());
            }
            for id in ids {
                self.delete_session_admin(&id).await?;
                ended.insert(id);
            }
        }
    }

    pub async fn resend_code(&self, user_id: &str, url_template: &str) -> Result<(), ZError> {
        self.call(
            Method::POST,
            &format!("/v2/users/{user_id}/email/resend"),
            Some(json!({"sendCode": {"urlTemplate": url_template}})),
        )
        .await?;
        Ok(())
    }

    pub async fn send_password_reset(
        &self,
        user_id: &str,
        url_template: &str,
    ) -> Result<(), ZError> {
        let body = json!({"sendLink": {"notificationType": "NOTIFICATION_TYPE_Email", "urlTemplate": url_template}});
        self.call(
            Method::POST,
            &format!("/v2/users/{user_id}/password_reset"),
            Some(body),
        )
        .await?;
        Ok(())
    }

    pub async fn set_password(
        &self,
        user_id: &str,
        code: &str,
        password: &str,
    ) -> Result<(), ZError> {
        let body = json!({"newPassword": {"password": password, "changeRequired": false}, "verificationCode": code});
        self.call(
            Method::POST,
            &format!("/v2/users/{user_id}/password"),
            Some(body),
        )
        .await?;
        Ok(())
    }

    /// The user (`GET /v2/users/{userId}`). `None` only when Zitadel definitely has no such
    /// user: its own not-found for this endpoint ([`refusals::GET_USER_NOT_FOUND`], checked
    /// against the real Zitadel by `tests/zitadel_live.rs`). Any other refusal is `Rejected`
    /// (another object's not-found too), a 4xx that isn't Zitadel's own error (a misrouted
    /// proxy's page, an empty body, a wrong `Host`'s "instance not found") is `Unavailable`,
    /// and so is a success answer that can't be read as a user (an empty body, no
    /// `user.userId`, no `details.resourceOwner`, another user's `userId`), never "no such
    /// user" (#57, #60): callers end sessions and delete rows on `None`, so an API change or a
    /// misbehaving proxy must fail closed.
    pub async fn get_user(&self, user_id: &str) -> Result<Option<ZUser>, ZError> {
        let path = format!("/v2/users/{user_id}");
        let (status, parsed) = self.send(Method::GET, &path, None).await?;
        if status.is_client_error() {
            let err = refused(status.as_u16(), parsed.as_ref());
            return if err.is(refusals::GET_USER_NOT_FOUND) {
                Ok(None)
            } else {
                Err(err)
            };
        }
        match parsed
            .as_ref()
            .and_then(|a| a.get("user"))
            .and_then(parse_user)
        {
            // An answer for another user, or one whose org can't be read, is a fault too:
            // callers act on the user's org and state (#57 audit B1, S1).
            Some(user) if user.id == user_id && !user.org_id.is_empty() => Ok(Some(user)),
            Some(_) => Err(ZError::Unavailable(
                "the user answer is for another user or has no organization".into(),
            )),
            None => Err(ZError::Unavailable("the user answer can't be read".into())),
        }
    }

    /// Whether Zitadel definitely has no user with this id: `true` only on its not-found, as
    /// [`Zitadel::get_user`]'s `None`.
    pub async fn user_is_gone(&self, user_id: &str) -> Result<bool, ZError> {
        Ok(self.get_user(user_id).await?.is_none())
    }

    pub async fn find_user_in_org(
        &self,
        org_id: &str,
        email: &str,
    ) -> Result<EmailMatch, ZError> {
        let body = json!({"queries": [
            {"emailQuery": {"emailAddress": email, "method": "TEXT_QUERY_METHOD_EQUALS_IGNORE_CASE"}},
            {"organizationIdQuery": {"organizationId": org_id}}
        ]});
        let answer = self.call(Method::POST, "/v2/users", Some(body)).await?;
        let result = answer
            .get("result")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(match result.as_slice() {
            [] => EmailMatch::None,
            [one] => parse_user(one).map_or(EmailMatch::None, EmailMatch::One),
            _ => EmailMatch::Many,
        })
    }

    pub async fn list_users(
        &self,
        org_id: &str,
        email_contains: Option<&str>,
        offset: u64,
        limit: u32,
    ) -> Result<UserPage, ZError> {
        let mut queries = vec![json!({"organizationIdQuery": {"organizationId": org_id}})];
        if let Some(part) = email_contains {
            queries.push(json!({"emailQuery": {"emailAddress": part,
                "method": "TEXT_QUERY_METHOD_CONTAINS_IGNORE_CASE"}}));
        }
        let body = json!({"query": {"offset": offset, "limit": limit}, "queries": queries});
        let answer = self.call(Method::POST, "/v2/users", Some(body)).await?;
        let users: Vec<ZUser> = answer
            .get("result")
            .and_then(Value::as_array)
            .map(|r| r.iter().filter_map(parse_user).collect())
            .unwrap_or_default();
        let total = answer
            .pointer("/details/totalResult")
            .and_then(as_u64)
            .unwrap_or(0);
        let next = offset + users.len() as u64;
        Ok(UserPage {
            next_offset: (!users.is_empty() && next < total).then_some(next),
            users,
        })
    }

    pub async fn org_has_users(&self, org_id: &str) -> Result<bool, ZError> {
        Ok(!self.list_users(org_id, None, 0, 1).await?.users.is_empty())
    }

    /// A delete: done on success, or on one of `gone`, the not-found answers captured for this
    /// endpoint (so a repeat is idempotent). Any other answer is an error, another object's
    /// not-found (a wrong `Host`'s, an org's) included.
    async fn call_or_gone(&self, path: &str, gone: &[Refusal]) -> Result<(), ZError> {
        match self.call(Method::DELETE, path, None).await {
            Ok(_) => Ok(()),
            Err(e) if e.is_any(gone) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Deletes the user; one already gone ([`refusals::DELETE_USER_NOT_FOUND`]) is fine.
    pub async fn delete_user(&self, user_id: &str) -> Result<(), ZError> {
        self.call_or_gone(
            &format!("/v2/users/{user_id}"),
            &[refusals::DELETE_USER_NOT_FOUND],
        )
        .await
    }

    pub async fn deactivate_user(&self, user_id: &str) -> Result<(), ZError> {
        self.call(
            Method::POST,
            &format!("/v2/users/{user_id}/deactivate"),
            Some(json!({})),
        )
        .await?;
        Ok(())
    }

    /// Deactivates the user, settled: one already inactive, or gone, is done (see
    /// [`Self::call_settled`]). Deactivating a locked user succeeds; it becomes inactive. A user
    /// in the Initial state can't be deactivated ([`refusals::DEACTIVATE_INITIAL`]): an error.
    pub async fn deactivate_user_settled(&self, user_id: &str) -> Result<(), ZError> {
        self.call_settled(
            user_id,
            "deactivate",
            refusals::DEACTIVATE_USER_NOT_FOUND,
            |state| state == UserState::Inactive,
        )
        .await
    }

    /// Reactivates the user, settled: one already active, or gone, is done (see
    /// [`Self::call_settled`]). So is a user in the Initial state (never signed in): Zitadel
    /// refuses to reactivate it as "not inactive", and it was never deactivated.
    pub async fn reactivate_user_settled(&self, user_id: &str) -> Result<(), ZError> {
        self.call_settled(
            user_id,
            "reactivate",
            refusals::REACTIVATE_USER_NOT_FOUND,
            |state| matches!(state, UserState::Active | UserState::Initial),
        )
        .await
    }

    pub async fn reactivate_user(&self, user_id: &str) -> Result<(), ZError> {
        self.call(
            Method::POST,
            &format!("/v2/users/{user_id}/reactivate"),
            Some(json!({})),
        )
        .await?;
        Ok(())
    }

    /// Locks the user (`POST /v2/users/{id}/lock`): no sign-in until unlocked (a suspended
    /// member, D5). Settled (see [`Self::call_settled`]): one already locked, or gone, is done,
    /// so a retry completes. Zitadel refuses to lock an inactive user with the same answer as a
    /// locked one; that user isn't locked, so it's an error.
    pub async fn lock_user(&self, user_id: &str) -> Result<(), ZError> {
        self.call_settled(user_id, "lock", refusals::LOCK_USER_NOT_FOUND, |state| {
            state == UserState::Locked
        })
        .await
    }

    /// Unlocks the user (`POST /v2/users/{id}/unlock`), settled as [`Self::lock_user`]: one
    /// that is active, inactive or initial, or gone, is done. A state `get_user` can't read is
    /// an error.
    pub async fn unlock_user(&self, user_id: &str) -> Result<(), ZError> {
        self.call_settled(user_id, "unlock", refusals::UNLOCK_USER_NOT_FOUND, |state| {
            matches!(state, UserState::Active | UserState::Inactive | UserState::Initial)
        })
        .await
    }

    /// A state change (`POST /v2/users/{id}/{verb}`), settled: done on success; on `gone` (this
    /// endpoint's own not-found: nothing left to change); or on Zitadel's precondition refusal
    /// (gRPC FAILED_PRECONDITION) when [`Self::get_user`] then shows the user in the target
    /// state (or gone). Zitadel's refusals don't say which state the user is in (a locked and
    /// an inactive user get the same one), so the state is read. Anything else is an error:
    /// the refusal itself when the state doesn't match.
    async fn call_settled(
        &self,
        user_id: &str,
        verb: &str,
        gone: Refusal,
        target: impl Fn(UserState) -> bool,
    ) -> Result<(), ZError> {
        let path = format!("/v2/users/{user_id}/{verb}");
        let refusal = match self.call(Method::POST, &path, Some(json!({}))).await {
            Ok(_) => return Ok(()),
            Err(e) if e.is(gone) => return Ok(()),
            Err(e) if e.zitadel_code() == Some(refusals::FAILED_PRECONDITION) => e,
            Err(e) => return Err(e),
        };
        match self.get_user(user_id).await? {
            None => Ok(()),
            Some(user) if target(user.state) => Ok(()),
            Some(_) => Err(refusal),
        }
    }

    /// `verified` marks the new address verified; otherwise a code is sent, with
    /// `url_template` when given.
    pub async fn set_email(
        &self,
        user_id: &str,
        email: &str,
        verified: bool,
        url_template: Option<&str>,
    ) -> Result<(), ZError> {
        let body = if verified {
            json!({"email": email, "isVerified": true})
        } else {
            match url_template {
                Some(t) => json!({"email": email, "sendCode": {"urlTemplate": t}}),
                None => json!({"email": email, "sendCode": {}}),
            }
        };
        self.call(
            Method::POST,
            &format!("/v2/users/{user_id}/email"),
            Some(body),
        )
        .await?;
        Ok(())
    }

    /// Marks the user's current address verified. `set_email` can't: Zitadel v4 refuses it
    /// for an unchanged address (`Errors.User.Email.NotChanged`), while `UpdateUser` with the
    /// same address only adds the verified event (nothing when already verified).
    pub async fn mark_email_verified(&self, user_id: &str, email: &str) -> Result<(), ZError> {
        let body = json!({"human": {"email": {"email": email, "isVerified": true}}});
        self.call(Method::PATCH, &format!("/v2/users/{user_id}"), Some(body))
            .await?;
        Ok(())
    }

    /// Needs the admin token's permission; no code or current password.
    pub async fn set_password_admin(&self, user_id: &str, password: &str) -> Result<(), ZError> {
        let body = json!({"newPassword": {"password": password, "changeRequired": false}});
        self.call(
            Method::POST,
            &format!("/v2/users/{user_id}/password"),
            Some(body),
        )
        .await?;
        Ok(())
    }

    pub async fn change_password(
        &self,
        user_id: &str,
        current: &str,
        password: &str,
    ) -> Result<(), ZError> {
        let body = json!({"newPassword": {"password": password, "changeRequired": false},
            "currentPassword": current});
        self.call(
            Method::POST,
            &format!("/v2/users/{user_id}/password"),
            Some(body),
        )
        .await?;
        Ok(())
    }

    pub async fn send_password_reset_code(
        &self,
        user_id: &str,
        url_template: &str,
    ) -> Result<(), ZError> {
        self.send_password_reset(user_id, url_template).await
    }

    /// Ends a session without its token (admin token). Zitadel answers 200 for a session that
    /// was ended before or never existed, so there is no not-found to accept.
    pub async fn delete_session_admin(&self, session_id: &str) -> Result<(), ZError> {
        self.call_or_gone(&format!("/v2/sessions/{session_id}"), &[])
            .await
    }

    /// Creates the organization with this id; one that exists already is fine.
    pub async fn ensure_org(&self, org_id: &str, name: &str) -> Result<(), ZError> {
        let body = json!({"name": name, "organizationId": org_id});
        match self
            .call(Method::POST, "/v2/organizations", Some(body))
            .await
        {
            Ok(_) | Err(ZError::Rejected { status: 409, .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Deletes the organization. Zitadel answers 200 for one deleted before or never created,
    /// so there is no not-found to accept.
    pub async fn delete_org(&self, org_id: &str) -> Result<(), ZError> {
        self.call_or_gone(&format!("/v2/organizations/{org_id}"), &[])
            .await
    }
}
