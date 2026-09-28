//! Zitadel v4's Session, User and OIDC v2 REST APIs, with the login-client token.

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
    pub password: &'a str,
    /// Create the email already verified (the default; see `CHELADB_REQUIRE_EMAIL_VERIFICATION`).
    pub verified: bool,
    pub url_template: &'a str,
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
        let value: Value = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text)
                .map_err(|_| ZError::Unavailable(format!("{status}: not JSON")))?
        };
        if status.is_client_error() {
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            return Err(ZError::Rejected {
                status: status.as_u16(),
                message,
            });
        }
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
            [one] => one
                .get("userId")
                .and_then(Value::as_str)
                .map(str::to_string),
            _ => None,
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
        } else {
            json!({"email": user.email, "sendCode": {"urlTemplate": user.url_template}})
        };
        let body = json!({"organizationId": user.org_id, "human": {
            "profile": {"givenName": user.given_name, "familyName": user.family_name},
            "email": email,
            "password": {"password": user.password, "changeRequired": false}
        }});
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
}
