//! The orchestrator's internal app-auth routes (`/internal/app/*`, bearer
//! `CHELABASE_APP_AUTH_INTERNAL_TOKEN`): branch settings, app sessions and the sync job.

use chelabase_query::app_users::SyncTarget;
use reqwest::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use uuid::Uuid;

/// A branch's app-user settings, and where its sign-ups go.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppBranchInfo {
    pub project_id: Uuid,
    /// `enabling`, `enabled`, `disabling` or `failed`.
    pub status: String,
    pub zitadel_org_id: Option<String>,
    pub email_verification: bool,
    pub on_delete: String,
    pub auth_database_id: Uuid,
    pub pg_database: String,
    pub schema: String,
    pub table: String,
    /// The branch itself when it belongs to the auth database, else the auth database's main
    /// branch (R5).
    pub target_branch_id: Uuid,
    /// `None` while the target has no running compute or no sync role.
    pub sync: Option<SyncTarget>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateSession {
    pub project_id: Uuid,
    pub user_id: Uuid,
    pub zitadel_session_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionCreated {
    pub session_id: Uuid,
    pub refresh_token: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefreshRequest {
    pub project_id: Uuid,
    pub refresh_token: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Refreshed {
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub refresh_token: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectBody {
    pub project_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RevokeUserSessions {
    pub project_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_session_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_before: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncRequest {
    pub project_id: Uuid,
    pub user_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_branch_id: Option<Uuid>,
}

/// An open invite as the accept page shows it (`GET /internal/invites/describe`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InviteDescription {
    pub inviter_name: String,
    pub inviter_email: Option<String>,
    pub tenant_name: String,
}

/// The sign-up gate's answer (`GET /internal/signup/allowed`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SignUpAnswer {
    pub allowed: bool,
    /// `open`, `first_member`, `closed` or `platform`.
    pub reason: String,
}

#[derive(Debug, thiserror::Error)]
pub enum InternalError {
    /// The orchestrator answered with a 4xx; `code` is the envelope's error code.
    #[error("orchestrator refused ({status}): {code}")]
    Refused { status: u16, code: String },
    /// No answer, a 5xx or an unreadable answer.
    #[error("orchestrator unavailable: {0}")]
    Unavailable(String),
}

pub struct InternalClient {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl InternalClient {
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

    async fn call(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(StatusCode, String), InternalError> {
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
            .map_err(|e| InternalError::Unavailable(e.without_url().to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| InternalError::Unavailable(e.without_url().to_string()))?;
        if status.is_server_error() {
            return Err(InternalError::Unavailable(status.to_string()));
        }
        if status.is_client_error() {
            let code = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| {
                    v.pointer("/error/code")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_default();
            return Err(InternalError::Refused {
                status: status.as_u16(),
                code,
            });
        }
        Ok((status, text))
    }

    async fn json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<T, InternalError> {
        let (_, text) = self.call(method, path, body).await?;
        serde_json::from_str(&text)
            .map_err(|e| InternalError::Unavailable(format!("bad answer: {e}")))
    }

    /// Whether the tenant of this Zitadel org lets a visitor create an account.
    pub async fn signup_allowed(&self, org_id: &str) -> Result<SignUpAnswer, InternalError> {
        self.json(
            Method::GET,
            &format!("/internal/signup/allowed?org={}", percent_encode(org_id)),
            None,
        )
        .await
    }

    /// The open invite of this Zitadel user whose token hashes to `token_hash` (hex SHA-256);
    /// `None` for anything else (the orchestrator answers every mismatch the same 404).
    pub async fn describe_invite(
        &self,
        user: &str,
        token_hash: &str,
    ) -> Result<Option<InviteDescription>, InternalError> {
        // A POST body, not a query: the hash stays out of request logs and trace spans.
        match self
            .json(
                Method::POST,
                "/internal/invites/describe",
                Some(serde_json::json!({ "user": user, "token_hash": token_hash })),
            )
            .await
        {
            Ok(found) => Ok(Some(found)),
            Err(InternalError::Refused { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `None` when the branch is unknown or its project has no app users.
    pub async fn branch_info(
        &self,
        branch_id: Uuid,
    ) -> Result<Option<AppBranchInfo>, InternalError> {
        match self
            .json(
                Method::GET,
                &format!("/internal/app/branches/{branch_id}"),
                None,
            )
            .await
        {
            Ok(info) => Ok(Some(info)),
            Err(InternalError::Refused { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn create_session(
        &self,
        body: &CreateSession,
    ) -> Result<SessionCreated, InternalError> {
        self.json(Method::POST, "/internal/app/sessions", Some(v(body)))
            .await
    }

    /// A refused token is `Refused { status: 401, code }`, with `refresh_token_invalid` or
    /// `refresh_token_reused`.
    pub async fn refresh(&self, body: &RefreshRequest) -> Result<Refreshed, InternalError> {
        self.json(
            Method::POST,
            "/internal/app/sessions/refresh",
            Some(v(body)),
        )
        .await
    }

    pub async fn revoke(&self, session_id: Uuid, project_id: Uuid) -> Result<(), InternalError> {
        self.call(
            Method::POST,
            &format!("/internal/app/sessions/{session_id}/revoke"),
            Some(v(&ProjectBody { project_id })),
        )
        .await
        .map(drop)
    }

    pub async fn revoke_by_token(&self, body: &RefreshRequest) -> Result<(), InternalError> {
        self.call(
            Method::POST,
            "/internal/app/sessions/revoke-by-token",
            Some(v(body)),
        )
        .await
        .map(drop)
    }

    pub async fn revoke_user_sessions(
        &self,
        user_id: Uuid,
        body: &RevokeUserSessions,
    ) -> Result<(), InternalError> {
        self.call(
            Method::POST,
            &format!("/internal/app/users/{user_id}/revoke-sessions"),
            Some(v(body)),
        )
        .await
        .map(drop)
    }

    pub async fn sync(&self, body: &SyncRequest) -> Result<(), InternalError> {
        self.call(Method::POST, "/internal/app/sync", Some(v(body)))
            .await
            .map(drop)
    }
}

/// Everything but RFC 3986 unreserved characters, as `%XX`.
fn percent_encode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn v<T: Serialize>(body: &T) -> Value {
    serde_json::to_value(body).expect("request body serialises")
}

#[cfg(test)]
mod tests {
    use super::percent_encode;

    #[test]
    fn org_ids_are_percent_encoded() {
        assert_eq!(percent_encode("2999"), "2999");
        assert_eq!(percent_encode("a b&c=d/é"), "a%20b%26c%3Dd%2F%C3%A9");
    }
}
