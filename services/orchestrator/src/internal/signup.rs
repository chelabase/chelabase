//! The sign-up gate (`GET /internal/signup/allowed?org=`, bearer `CHELABASE_APP_AUTH_INTERNAL_TOKEN`
//! like `/internal/app/*`): the `auth` service asks before it creates a platform user.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use chelabase_neon::ComputeLocator;
use chelabase_types::api_error::ApiError;
use chelabase_types::mode_policy::SignUpRule;
use serde::Deserialize;
use serde_json::json;
use tracing::error;

use super::{InternalState, NeonMetadataPort};

#[derive(Deserialize)]
pub(super) struct Org {
    org: String,
}

/// The `first_member` window: it counts members, not Zitadel users, so until the first sign-up's
/// first request creates its member row, two concurrent sign-ups are both allowed. Both
/// accounts are created; only one wins the admin claim, atomically in `ensure_member`, and the
/// other's first request makes it a `pending` request while the claim is recent (M7,
/// `FIRST_MEMBER_WINDOW`), so the admin can approve it. A tenant
/// with only a pending invite (`invited` counts as a member) is closed, as intended.
///
/// `{"allowed": bool, "reason": "open" | "first_member" | "closed" | "platform"}`. A Zitadel
/// org no tenant owns is `closed`.
pub(super) async fn allowed<M, L>(
    State(state): State<Arc<InternalState<M, L>>>,
    Query(q): Query<Org>,
) -> Response
where
    M: NeonMetadataPort,
    L: ComputeLocator,
{
    if state.policy.sign_up() == SignUpRule::Refused {
        return Json(json!({"allowed": false, "reason": "platform"})).into_response();
    }
    let (allowed, reason) = match state.metadata.signup_facts(&q.org).await {
        Ok(Some(f)) if f.open => (true, "open"),
        Ok(Some(f)) if !f.has_members => (true, "first_member"),
        Ok(_) => (false, "closed"),
        Err(e) => {
            error!("sign-up gate: {e}");
            return ApiError::internal().into_response();
        }
    };
    Json(json!({"allowed": allowed, "reason": reason})).into_response()
}
