//! Zitadel v4's Session, User, Organization and OIDC v2 REST APIs.

use chrono::{DateTime, Utc};
use reqwest::Method;
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum ZError {
    /// Zitadel understood and refused (4xx). `message` is Zitadel's error id, e.g.
    /// `Errors.User.Code.Invalid`; never shown to users.
    #[error("zitadel rejected the request ({status}): {message}")]
    Rejected { status: u16, message: String },
    /// No answer, a timeout, a 5xx or an unreadable answer.
    #[error("zitadel unavailable: {0}")]
    Unavailable(String),
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
    /// Create the email already verified (the default; see `CHELADB_REQUIRE_EMAIL_VERIFICATION`).
    pub verified: bool,
    pub url_template: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserState {
    Active,
    Inactive,
    Locked,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZUser {
    pub id: String,
    pub org_id: String,
    pub email: String,
    pub email_verified: bool,
    pub state: UserState,
    pub created_at: Option<DateTime<Utc>>,
}

/// What an org's email lookup found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmailMatch {
    None,
    One(ZUser),
    /// More than one user has the email (made outside Cheladb's checks): it is taken, and no
    /// single user signs in with it.
    Many,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPage {
    pub users: Vec<ZUser>,
    pub next_offset: Option<u64>,
}

pub struct Zitadel {
    http: reqwest::Client,
    base: String,
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
        state: match v.get("state").and_then(Value::as_str) {
            Some("USER_STATE_ACTIVE") => UserState::Active,
            Some("USER_STATE_INACTIVE") => UserState::Inactive,
            Some("USER_STATE_LOCKED") => UserState::Locked,
            _ => UserState::Other,
        },
        created_at: details
            .and_then(|d| d.get("creationDate"))
            .and_then(Value::as_str)
            .and_then(|t| t.parse().ok()),
    })
}

/// proto3 JSON writes uint64 as a string.
fn as_u64(v: &Value) -> Option<u64> {
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

impl Zitadel {
    pub fn new(base: &str, token: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .expect("reqwest client"),
            base: base.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value, ZError> {
        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base))
            .bearer_auth(&self.token);
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
        // A proxy may answer a 4xx with HTML; the status alone is still a rejection.
        let parsed = if text.trim().is_empty() {
            Some(Value::Null)
        } else {
            serde_json::from_str::<Value>(&text).ok()
        };
        if status.is_client_error() {
            let message = parsed
                .as_ref()
                .and_then(|v| v.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            return Err(ZError::Rejected {
                status: status.as_u16(),
                message,
            });
        }
        let value = parsed.ok_or_else(|| ZError::Unavailable(format!("{status}: not JSON")))?;
        Ok(value)
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

    pub async fn get_user(&self, user_id: &str) -> Result<Option<ZUser>, ZError> {
        match self
            .call(Method::GET, &format!("/v2/users/{user_id}"), None)
            .await
        {
            Ok(answer) => Ok(answer.get("user").and_then(parse_user)),
            Err(ZError::Rejected { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
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

    /// 404 is fine: the user is already gone.
    async fn call_or_gone(&self, method: Method, path: &str) -> Result<(), ZError> {
        match self.call(method, path, None).await {
            Ok(_) | Err(ZError::Rejected { status: 404, .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub async fn delete_user(&self, user_id: &str) -> Result<(), ZError> {
        self.call_or_gone(Method::DELETE, &format!("/v2/users/{user_id}"))
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

    pub async fn reactivate_user(&self, user_id: &str) -> Result<(), ZError> {
        self.call(
            Method::POST,
            &format!("/v2/users/{user_id}/reactivate"),
            Some(json!({})),
        )
        .await?;
        Ok(())
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

    /// Ends a session without its token (admin token).
    pub async fn delete_session_admin(&self, session_id: &str) -> Result<(), ZError> {
        self.call_or_gone(Method::DELETE, &format!("/v2/sessions/{session_id}"))
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

    pub async fn delete_org(&self, org_id: &str) -> Result<(), ZError> {
        self.call_or_gone(Method::DELETE, &format!("/v2/organizations/{org_id}"))
            .await
    }
}
