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
    pub identity_session_id: String,
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

/// The session a refresh token would refresh, and its user ([`InternalClient::peek`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Peeked {
    pub session_id: Uuid,
    pub user_id: Uuid,
    /// The token is the previous one, inside its grace: reuse once the grace ends.
    pub grace: bool,
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

/// What the orchestrator stores about a Zitadel user's latest member invite
/// (`POST /internal/invites/open`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct OpenInvite {
    /// The invite is open: not revoked, accepted or expired.
    pub open: bool,
    /// Its stored expiry; `None` when the user has no such invite.
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// The sign-up gate's answer (`POST /internal/signup/check`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SignUpAnswer {
    pub allowed: bool,
    /// `open`, `first_member`, `closed`, `setup_code_invalid` or `platform`.
    pub reason: String,
    /// A fresh install waits for its setup code.
    #[serde(default)]
    pub setup_required: bool,
    /// With `first_member`: what binds the fresh-install claim to the account this sign-up
    /// creates (`claimed`).
    #[serde(default)]
    pub claim_nonce: Option<ClaimNonce>,
}

/// A one-time claim nonce. `Debug` shows nothing.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct ClaimNonce(String);

impl ClaimNonce {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ClaimNonce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClaimNonce(..)")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InternalError {
    /// The orchestrator answered with a 4xx; `code` is the envelope's error code.
    #[error("orchestrator refused ({status}): {code}")]
    Refused { status: u16, code: String },
    /// No answer, a 5xx, a 3xx (never followed) or an unreadable answer.
    #[error("orchestrator unavailable: {0}")]
    Unavailable(String),
}

pub struct InternalClient {
    http: chelabase_http::Client,
    base: String,
    token: String,
}

impl InternalClient {
    pub fn new(base: &str, token: &str) -> Self {
        Self {
            // Never a redirect (#26): the internal bearer goes to the orchestrator only.
            http: chelabase_http::client_builder()
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
        // #26: a 3xx (never followed) is no answer: its empty body would read as success, and a
        // revoke or sync that met one did nothing.
        if !status.is_success() {
            return Err(InternalError::Unavailable(format!(
                "{status}: the orchestrator answered with no result (redirects are not followed)"
            )));
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

    /// Whether the tenant of this Zitadel org lets a visitor create an account, with the setup
    /// code they gave (a body, never a URL).
    pub async fn signup_check(
        &self,
        org_id: &str,
        setup_code: Option<&str>,
    ) -> Result<SignUpAnswer, InternalError> {
        let mut body = serde_json::json!({ "org": org_id });
        if let Some(code) = setup_code {
            body["setup_code"] = Value::from(code);
        }
        self.json(Method::POST, "/internal/signup/check", Some(body)).await
    }

    /// Binds the fresh-install claim to the account just created: `true` when bound, `false`
    /// when the tenant was already claimed (the account stays, as a newcomer). Any error means
    /// no claim: the caller deletes the account.
    pub async fn signup_claimed(
        &self,
        org_id: &str,
        user_id: &str,
        nonce: &ClaimNonce,
    ) -> Result<bool, InternalError> {
        #[derive(Deserialize)]
        struct Claimed {
            bound: bool,
        }
        let answer: Claimed = self
            .json(
                Method::POST,
                "/internal/signup/claimed",
                Some(serde_json::json!({ "org": org_id, "user_id": user_id, "nonce": nonce.0 })),
            )
            .await?;
        Ok(answer.bound)
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

    /// Whether this Zitadel user has an open member invite, and its stored expiry. Any refusal
    /// (a wrong token, a 404) is an error, never "no invite".
    pub async fn open_invite(&self, user: &str) -> Result<OpenInvite, InternalError> {
        self.json(
            Method::POST,
            "/internal/invites/open",
            Some(serde_json::json!({ "user": user })),
        )
        .await
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

    /// What [`InternalClient::refresh`] would do with this token, rotating and revoking
    /// nothing: the session and its user, or the same `Refused { status: 401, code }` verdict.
    pub async fn peek(&self, body: &RefreshRequest) -> Result<Peeked, InternalError> {
        self.json(Method::POST, "/internal/app/sessions/peek", Some(v(body)))
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

fn v<T: Serialize>(body: &T) -> Value {
    serde_json::to_value(body).expect("request body serialises")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// An "orchestrator" that answers every request with `status` and a `location` elsewhere.
    async fn answering(status: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = [0u8; 8192];
                let _ = sock.read(&mut buf).await;
                let reply = format!(
                    "HTTP/1.1 {status}\r\nlocation: http://elsewhere.invalid/\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                );
                let _ = sock.write_all(reply.as_bytes()).await;
            }
        });
        format!("http://{addr}")
    }

    /// #26: a 3xx is never followed, so it is no answer at all: a revoke or a sync that met one
    /// did nothing, and must not report success.
    #[tokio::test]
    async fn a_redirect_is_not_a_successful_revoke_or_sync() {
        for status in [
            "301 Moved Permanently",
            "302 Found",
            "303 See Other",
            "307 Temporary Redirect",
            "308 Permanent Redirect",
            "304 Not Modified",
        ] {
            let c = InternalClient::new(&answering(status).await, "internal-token");
            let (project_id, user_id) = (Uuid::new_v4(), Uuid::new_v4());
            let refresh = RefreshRequest {
                project_id,
                refresh_token: "rt".into(),
            };
            let all = RevokeUserSessions {
                project_id,
                keep_session_id: None,
                created_before: None,
            };
            let sync = SyncRequest {
                project_id,
                user_id,
                data: None,
                origin_branch_id: None,
            };
            let results = [
                ("revoke", c.revoke(Uuid::new_v4(), project_id).await),
                ("revoke_by_token", c.revoke_by_token(&refresh).await),
                ("revoke_user_sessions", c.revoke_user_sessions(user_id, &all).await),
                ("sync", c.sync(&sync).await),
            ];
            for (what, r) in results {
                assert!(
                    matches!(r, Err(InternalError::Unavailable(_))),
                    "a {status} from the orchestrator read as a successful {what}: {r:?}"
                );
            }
        }
    }
}
