//! Who is calling an app flow: the gateway's identity token (`aud = "auth"`) for a public
//! data key, and the app user when the request carried a valid `Chelabase-User` token.

use super::AppUsers;
use crate::app::AppState;
use crate::guard::client_ip;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use chelabase_auth::internal::{AuthMethod, IDENTITY_HEADER};
use chelabase_types::api_error::{ApiError, ErrorCode};
use chelabase_types::data_key::DataKeyKind;
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

/// The identity token's audience.
pub const AUDIENCE: &str = "auth";

/// The app user's claims, as the gateway put them in the identity (`data.claims`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AppUserClaims {
    pub sub: Uuid,
    pub sid: Uuid,
    pub email: String,
    pub email_verified: bool,
    #[serde(default)]
    pub project: Option<Uuid>,
}

#[derive(Debug, Clone)]
pub struct AppCaller {
    pub branch_id: Uuid,
    pub project_id: Uuid,
    pub key_id: Uuid,
    pub user: Option<AppUserClaims>,
    pub request_id: String,
    pub ip: String,
}

pub(crate) fn unavailable() -> ApiError {
    ApiError::new(
        503,
        ErrorCode::Unavailable,
        "app users are not configured on this server",
    )
}

fn unauthorized(message: &str) -> ApiError {
    ApiError::new(401, ErrorCode::Unauthorized, message)
}

impl AppCaller {
    /// Reads and checks the identity header.
    pub fn from_headers(app: &AppUsers, headers: &http::HeaderMap) -> Result<Self, ApiError> {
        let token = headers
            .get(IDENTITY_HEADER)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| unauthorized("missing identity"))?;
        let claims = app
            .verifier
            .verify(token, AUDIENCE)
            .map_err(|_| unauthorized("invalid identity"))?;
        let data = match (claims.auth, claims.data) {
            (AuthMethod::DataKey, Some(data)) => data,
            _ => return Err(unauthorized("this endpoint takes a public key")),
        };
        // The gateway refuses secret keys here already; this is defence in depth.
        if data.kind != DataKeyKind::Public {
            return Err(unauthorized("this endpoint takes a public key"));
        }
        let project_id = data
            .project_id
            .ok_or_else(|| unauthorized("this key has no project"))?;
        // A user token's claims carry its session (`sid`); the key's own role says nothing
        // (a public key may itself have the role `authenticated`).
        let user = if data.claims.get("sid").is_some_and(|v| !v.is_null()) {
            let user: AppUserClaims = serde_json::from_value(data.claims)
                .map_err(|_| unauthorized("invalid identity"))?;
            if user.project.is_some_and(|p| p != project_id) {
                return Err(unauthorized("invalid identity"));
            }
            Some(user)
        } else {
            None
        };
        Ok(Self {
            branch_id: data.branch_id,
            project_id,
            key_id: data.key_id,
            user,
            request_id: claims.rid,
            ip: client_ip(headers),
        })
    }
}

impl FromRequestParts<Arc<AppState>> for AppCaller {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, ApiError> {
        let Some(app) = state.app.as_ref() else {
            return Err(unavailable());
        };
        Self::from_headers(app, &parts.headers)
    }
}
