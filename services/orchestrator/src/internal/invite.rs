//! The invite description (`POST /internal/invites/describe {user, token_hash}`, bearer
//! `CHELABASE_APP_AUTH_INTERNAL_TOKEN`, as for `/internal/app/*`): the `auth` service asks for it
//! on behalf of the accept page, before any code is spent. The hash travels in the body so no
//! request log or trace span carries it.
//!
//! The open check (`POST /internal/invites/open {user}`, the same bearer): whether this identity
//! user has an open member invite, and its stored expiry, for auth's accept-invite gate (#61).
//!
//! The accept (`POST /internal/invites/accept {user, token_hash, email, name}`, the same
//! bearer): once auth has the invitee's proof (the invite code, or the account's own password),
//! the member row is written here, because a member token never makes one (#124). The open
//! invite whose stored hash equals `token_hash` activates its `invited` row in its tenant
//! (the invite closes, [`MetadataClient::admit_member`]) and the offboarded mark is cleared.
//!
//! [`MetadataClient::admit_member`]: chelabase_metadata::MetadataClient::admit_member

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::response::{IntoResponse, Response};
use chelabase_metadata::members::{Admission, InviteDescription, MemberStatus};
use chelabase_neon::ComputeLocator;
use chrono::SecondsFormat;
use chelabase_types::TenantId;
use chelabase_types::api_error::{ApiError, ErrorCode};
use serde::Deserialize;
use serde_json::json;
use tracing::error;

use super::{InternalState, NeonMetadataPort, constant_time_eq};

#[derive(Deserialize)]
pub(super) struct Describe {
    /// The invitee's identity user id.
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
    match found {
        Some(d) if token_matches(Some(&d), &presented) => {
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

/// Whether the invite's stored token hash is `presented`. Always one fixed-cost compare: with no
/// stored hash (no invite, or one made before tokens existed) the presented hash is compared with
/// a random dummy, which never matches.
fn token_matches(found: Option<&InviteDescription>, presented: &[u8]) -> bool {
    let stored = match found.and_then(|d| d.token_hash.clone()) {
        Some(hash) => hash,
        None => {
            let mut dummy = [0u8; 32];
            getrandom::fill(&mut dummy).expect("OS random number generator");
            dummy.to_vec()
        }
    };
    constant_time_eq(&stored, presented) && found.is_some_and(|d| d.token_hash.is_some())
}

#[derive(Deserialize)]
pub(super) struct Open {
    /// The invitee's identity user id.
    user: String,
}

/// `{open, expires_at}`: whether this identity user's latest member invite that isn't revoked or
/// accepted (its member still invited) is open, i.e. not expired by the database clock (as
/// `describe` decides), and its stored expiry (RFC 3339,
/// `null` with no such invite). Nothing else about the invite or the member is returned. A body
/// that isn't `{user: string}` is the same as a user with no invite.
pub(super) async fn open<M, L>(
    State(state): State<Arc<InternalState<M, L>>>,
    body: Result<Json<Open>, JsonRejection>,
) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    let found = match body {
        Ok(Json(q)) => match state.metadata.invite_expiry_for_user(&q.user).await {
            Ok(found) => found,
            Err(e) => {
                error!("open invite: {e}");
                return ApiError::internal().into_response();
            }
        },
        Err(_) => None,
    };
    Json(json!({
        "open": found.is_some_and(|(_, open)| open),
        "expires_at": found.map(|(at, _)| at.to_rfc3339_opts(SecondsFormat::Micros, true)),
    }))
    .into_response()
}

/// Missing fields are empty, so they are refused like wrong ones.
#[derive(Deserialize)]
pub(super) struct Accept {
    /// The invitee's identity user id.
    #[serde(default)]
    user: String,
    /// Hex of the SHA-256 of the link's token.
    #[serde(default)]
    token_hash: String,
    /// The account's email and name, as identity has them: the row takes them.
    #[serde(default)]
    email: String,
    #[serde(default)]
    name: String,
}

fn accepted(tenant: TenantId) -> Response {
    Json(json!({ "tenant_id": tenant.as_uuid() })).into_response()
}

/// `{tenant_id}` once the user's row in the invite's tenant is active: their open invite whose
/// stored hash is `token_hash` activated it now, or (a retry after a later step of auth's failed)
/// their accepted invite with that hash names a tenant they are active in. 404 `not_found` for
/// anything else: no open or accepted invite has that hash (or the person isn't active in an
/// accepted one's tenant), a malformed body. 403
/// `member_suspended` for a suspended row, 410 `invite_expired` when the invite expired before
/// the write. Each success clears the offboarded mark (a removed member invited again; m9 clears
/// it on the invite too), so the access check doesn't answer `member_offboarded`.
///
/// The invite matched is the user's latest open one, as `describe` shows it; the row activated
/// is in that invite's tenant, which `admit_member` activates only while the row has an open
/// invite.
pub(super) async fn accept<M, L>(
    State(state): State<Arc<InternalState<M, L>>>,
    body: Result<Json<Accept>, JsonRejection>,
) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    let Ok(Json(b)) = body else {
        return not_found();
    };
    let Some(presented) = hex::decode(&b.token_hash).ok().filter(|h| h.len() == 32) else {
        return not_found();
    };
    if b.user.is_empty() {
        return not_found();
    }
    let failed = |what: &str, e: &dyn std::fmt::Display| {
        error!("accept invite: {what}: {e}");
        ApiError::internal().into_response()
    };
    let found = match state.metadata.open_invite_for_user(&b.user).await {
        Ok(found) => found,
        Err(e) => return failed("the open invite", &e),
    };
    let tenant = match found.as_ref() {
        Some(invite) if token_matches(found.as_ref(), &presented) => invite.tenant_id,
        _ => {
            // A retry: the invite this hash names was accepted already. Every candidate is
            // compared, in constant time, so the answer doesn't tell where the hash differs.
            let earlier = match state.metadata.accepted_invites_for_user(&b.user).await {
                Ok(rows) => rows,
                Err(e) => return failed("the accepted invites", &e),
            };
            let tenant = earlier.iter().fold(None, |hit, (tenant, hash)| {
                let same = hash.as_deref().is_some_and(|h| constant_time_eq(h, &presented));
                hit.or(same.then_some(*tenant))
            });
            let Some(tenant) = tenant else {
                return not_found();
            };
            return match state.metadata.member_tenants(&b.user).await {
            Ok(rows) if rows.contains(&(tenant, MemberStatus::Active)) => match state.metadata.clear_offboarded(tenant, &b.user).await {
                Ok(_) => accepted(tenant),
                Err(e) => failed("clearing the offboarded mark", &e),
            },
                Ok(_) => not_found(),
                Err(e) => failed("the memberships", &e),
            };
        }
    };
    match state
        .metadata
        .admit_sign_up(tenant, &b.user, &b.email, &b.name, Admission::Closed)
        .await
    {
        Ok(Some(MemberStatus::Active)) => {}
        Ok(Some(MemberStatus::Suspended)) => {
            return ApiError::new(403, ErrorCode::MemberSuspended, "the member is suspended").into_response();
        }
        Ok(Some(MemberStatus::Invited)) => {
            return ApiError::new(410, ErrorCode::InviteExpired, "the invite has expired").into_response();
        }
        Ok(_) => return not_found(),
        Err(e) => return failed("activating the member", &e),
    }
    match state.metadata.clear_offboarded(tenant, &b.user).await {
        Ok(_) => accepted(tenant),
        Err(e) => failed("clearing the offboarded mark", &e),
    }
}
