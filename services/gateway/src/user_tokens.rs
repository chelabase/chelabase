//! End-user (app-user) tokens: verify the `Chelabase-User` JWT offline, then check the session
//! is still live through the orchestrator's internal listener.
//!
//! - The signature, issuer, audience (the key's project) and expiry are checked locally.
//! - Liveness is cached for 30 s per session id, so a revoked session stays usable on a
//!   gateway instance for at most that long. Transport failures are never cached.
//! - A session counts as dead when the answer says so (`live: false` / `revoked: true`), when
//!   its `expires_at` is in the past, when it is unknown (404) or belongs to another project.

use chelabase_auth::app_token::{AppClaims, AppTokenError, AppTokenVerifier};
use moka::future::Cache;
use serde::Deserialize;
use std::time::Duration;
use tracing::warn;
use uuid::Uuid;

pub const LIVENESS_TTL: Duration = Duration::from_secs(30);
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a user token was refused. Maps to 401 | 401 | 401 | 503.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserTokenError {
    Invalid,
    Expired,
    Revoked,
    Unavailable,
}

/// The internal listener's answer for one session, as far as the gateway needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Liveness {
    live: bool,
    project_id: Option<Uuid>,
}

#[derive(Deserialize)]
struct SessionAnswer {
    #[serde(default)]
    live: Option<bool>,
    #[serde(default)]
    revoked: Option<bool>,
    #[serde(default)]
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    project_id: Option<Uuid>,
}

impl SessionAnswer {
    fn into_liveness(self) -> Liveness {
        let flagged_live = match (self.live, self.revoked) {
            (_, Some(true)) | (Some(false), _) => false,
            (Some(true), _) | (None, Some(false)) => true,
            // Neither field present: don't guess, treat it as dead.
            (None, None) => false,
        };
        let expired = self.expires_at.is_some_and(|at| at <= chrono::Utc::now());
        Liveness {
            live: flagged_live && !expired,
            project_id: self.project_id,
        }
    }
}

pub struct UserTokens {
    verifier: Option<AppTokenVerifier>,
    client: reqwest::Client,
    base: String,
    bearer: String,
    live: Cache<Uuid, Liveness>,
}

impl UserTokens {
    pub fn new(verifier: Option<AppTokenVerifier>, internal_url: &str, bearer: &str) -> Self {
        Self::with_ttl(verifier, internal_url, bearer, LIVENESS_TTL)
    }

    pub fn with_ttl(
        verifier: Option<AppTokenVerifier>,
        internal_url: &str,
        bearer: &str,
        ttl: Duration,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(LOOKUP_TIMEOUT)
            .build()
            .expect("build the session liveness HTTP client");
        Self {
            verifier,
            client,
            base: internal_url.trim_end_matches('/').to_string(),
            bearer: bearer.to_string(),
            live: Cache::builder()
                .max_capacity(100_000)
                .time_to_live(ttl)
                .build(),
        }
    }

    /// With no verification key configured, every user token is invalid.
    pub async fn check(&self, token: &str, project: Uuid) -> Result<AppClaims, UserTokenError> {
        let verifier = self.verifier.as_ref().ok_or(UserTokenError::Invalid)?;
        let claims = verifier.verify(token, project).map_err(|err| match err {
            AppTokenError::Expired => UserTokenError::Expired,
            AppTokenError::Invalid => UserTokenError::Invalid,
        })?;
        if claims.project != project {
            return Err(UserTokenError::Invalid);
        }
        let liveness = self
            .live
            .try_get_with(claims.sid, self.lookup(claims.sid))
            .await
            .map_err(|err| *err)?;
        if !liveness.live || liveness.project_id.is_some_and(|p| p != project) {
            return Err(UserTokenError::Revoked);
        }
        Ok(claims)
    }

    async fn lookup(&self, sid: Uuid) -> Result<Liveness, UserTokenError> {
        let url = format!("{}/internal/app/sessions/{sid}", self.base);
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.bearer)
            .send()
            .await
            .map_err(|err| {
                warn!("session liveness lookup failed: {err}");
                UserTokenError::Unavailable
            })?;
        match response.status().as_u16() {
            200 => response
                .json::<SessionAnswer>()
                .await
                .map(SessionAnswer::into_liveness)
                .map_err(|err| {
                    warn!("session liveness lookup returned an unreadable body: {err}");
                    UserTokenError::Unavailable
                }),
            404 => Ok(Liveness {
                live: false,
                project_id: None,
            }),
            status => {
                warn!(status, "session liveness lookup refused or failed");
                Err(UserTokenError::Unavailable)
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::get;
    use axum::{Json, Router};
    use chelabase_auth::app_token::{APP_ISSUER, AppTokenSigner, app_audience};
    use chelabase_auth::internal::generate_keypair_pem;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    pub const BEARER: &str = "test-app-sessions-token";

    #[derive(Clone)]
    pub enum Session {
        Live(Uuid),
        Revoked(Uuid),
        Expired(Uuid),
        Error,
    }

    #[derive(Clone, Default)]
    pub struct Mock {
        pub lookups: Arc<AtomicUsize>,
        pub bad_bearer: Arc<AtomicUsize>,
        pub sessions: Arc<Mutex<HashMap<Uuid, Session>>>,
    }

    impl Mock {
        pub fn set(&self, sid: Uuid, session: Session) {
            self.sessions.lock().unwrap().insert(sid, session);
        }
        pub fn lookups(&self) -> usize {
            self.lookups.load(Ordering::SeqCst)
        }
    }

    pub async fn start() -> (String, Mock) {
        let mock = Mock::default();
        let app = Router::new()
            .route(
                "/internal/app/sessions/{id}",
                get(|State(mock): State<Mock>, Path(id): Path<Uuid>, headers: HeaderMap| async move {
                    mock.lookups.fetch_add(1, Ordering::SeqCst);
                    let expected = format!("Bearer {BEARER}");
                    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some(&expected) {
                        mock.bad_bearer.fetch_add(1, Ordering::SeqCst);
                        return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({})));
                    }
                    let session = mock.sessions.lock().unwrap().get(&id).cloned();
                    let future = chrono::Utc::now() + chrono::Duration::hours(1);
                    let past = chrono::Utc::now() - chrono::Duration::hours(1);
                    match session {
                        Some(Session::Live(p)) => (
                            StatusCode::OK,
                            Json(serde_json::json!({"live": true, "project_id": p, "user_id": Uuid::nil(), "expires_at": future})),
                        ),
                        Some(Session::Revoked(p)) => (
                            StatusCode::OK,
                            Json(serde_json::json!({"live": false, "revoked": true, "project_id": p, "user_id": Uuid::nil()})),
                        ),
                        // `live: true` but past its expiry: the gateway must still refuse it.
                        Some(Session::Expired(p)) => (
                            StatusCode::OK,
                            Json(serde_json::json!({"live": true, "project_id": p, "user_id": Uuid::nil(), "expires_at": past})),
                        ),
                        Some(Session::Error) => (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({}))),
                        None => (StatusCode::NOT_FOUND, Json(serde_json::json!({}))),
                    }
                }),
            )
            .with_state(mock.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), mock)
    }

    /// A signer and the matching verifier.
    pub fn keys() -> (AppTokenSigner, AppTokenVerifier) {
        let (private, public) = generate_keypair_pem();
        (
            AppTokenSigner::from_pem(private.as_bytes(), public.as_bytes()).unwrap(),
            AppTokenVerifier::from_pem(public.as_bytes()).unwrap(),
        )
    }

    pub fn claims(project: Uuid, sid: Uuid, exp_in_secs: i64) -> AppClaims {
        let now = chrono::Utc::now().timestamp();
        AppClaims {
            iss: APP_ISSUER.to_string(),
            aud: app_audience(project),
            sub: Uuid::new_v4(),
            sid,
            project,
            email: "ada@example.com".to_string(),
            email_verified: true,
            role: "authenticated".to_string(),
            iat: now as u64,
            exp: (now + exp_in_secs) as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[tokio::test]
    async fn without_a_verification_key_every_token_is_invalid() {
        let (signer, _) = keys();
        let (url, mock) = start().await;
        let project = Uuid::new_v4();
        let token = signer.sign(&claims(project, Uuid::new_v4(), 300)).unwrap();
        let users = UserTokens::new(None, &url, BEARER);
        assert_eq!(
            users.check(&token, project).await.unwrap_err(),
            UserTokenError::Invalid
        );
        assert_eq!(mock.lookups(), 0);
    }

    #[test]
    fn an_answer_with_neither_flag_is_dead() {
        let answer: SessionAnswer = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(!answer.into_liveness().live);
    }
}
