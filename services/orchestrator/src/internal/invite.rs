//! The invite description (`POST /internal/invites/describe {user, token_hash}`, bearer
//! `CHELABASE_APP_AUTH_INTERNAL_TOKEN`, as for `/internal/app/*`): the `auth` service asks for it
//! on behalf of the accept page, before any code is spent. The hash travels in the body so no
//! request log or trace span carries it.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::response::{IntoResponse, Response};
use chelabase_neon::ComputeLocator;
use chelabase_types::api_error::ApiError;
use serde::Deserialize;
use serde_json::json;
use tracing::error;

use super::{InternalState, NeonMetadataPort, constant_time_eq};

#[derive(Deserialize)]
pub(super) struct Describe {
    /// The invitee's Zitadel user id.
    user: String,
    /// Hex of the SHA-256 of the link's token.
    token_hash: String,
}

fn not_found() -> Response {
    ApiError::not_found("invite not found").into_response()
}

/// `{inviter_name, inviter_email, tenant_name}` (the tenant's display name, else its slug; the
/// inviter's name, else its email's local part, else "An admin") for an open invite of this user whose stored
/// token hash equals `token_hash` (compared in constant time). Anything else is the same 404:
/// no invite, a closed one, one made before tokens existed, a wrong token or a malformed hash.
pub(super) async fn describe<M, L>(
    State(state): State<Arc<InternalState<M, L>>>,
    body: Result<Json<Describe>, JsonRejection>,
) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    let Ok(Json(q)) = body else {
        return not_found();
    };
    let Some(presented) = hex::decode(&q.token_hash).ok().filter(|h| h.len() == 32) else {
        return not_found();
    };
    let found = match state.metadata.open_invite_for_user(&q.user).await {
        Ok(found) => found,
        Err(e) => {
            error!("describe invite: {e}");
            return ApiError::internal().into_response();
        }
    };
    // Always one fixed-cost compare: with no stored hash (no invite, or one made before tokens
    // existed) the presented hash is compared with a random dummy, which never matches.
    let stored = match found.as_ref().and_then(|d| d.token_hash.clone()) {
        Some(hash) => hash,
        None => {
            let mut dummy = [0u8; 32];
            getrandom::fill(&mut dummy).expect("OS random number generator");
            dummy.to_vec()
        }
    };
    let matches = constant_time_eq(&stored, &presented);
    match found {
        Some(d) if d.token_hash.is_some() && matches => {
            let name = Some(d.inviter_name.trim())
                .filter(|n| !n.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    d.inviter_email
                        .as_deref()
                        .and_then(|e| e.split('@').next())
                        .filter(|n| !n.is_empty())
                        .map(str::to_string)
                })
                // No name and no email (a machine user): only tenant and project Admins invite.
                .unwrap_or_else(|| "An admin".to_string());
            Json(json!({
                "inviter_name": name,
                "inviter_email": d.inviter_email,
                "tenant_name": d.tenant_display_name.as_deref().unwrap_or(&d.tenant_slug),
            }))
            .into_response()
        }
        _ => not_found(),
    }
}
