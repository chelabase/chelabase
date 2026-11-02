//! The HTTP edge: `/storage/v1/*` for the gateway, which it trusts only through the gateway's
//! identity token minted for `storage` with a data key's identity, and `GET /health`.

use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::get;
use chelabase_auth::internal::{AuthMethod, DataIdentity, IDENTITY_HEADER, InternalVerifier};
use chelabase_pgsession::ConnSource;
use chelabase_storage::ObjectStore;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::engine::Engine;
use crate::error::Failure;
use crate::{buckets, objects};

/// The identity token's audience.
pub const AUDIENCE: &str = "storage";

/// The largest bucket request body (a JSON object).
pub const MAX_BUCKET_BODY: usize = 64 * 1024;

pub struct AppState<S, M> {
    pub engine: Engine<S, M>,
    pub verifier: InternalVerifier,
    /// Where objects go (m5's object routes).
    pub store: Arc<dyn ObjectStore>,
}

/// The public routes with their OpenAPI annotations ([`crate::openapi::openapi`]).
pub fn api_router<S: ConnSource, M: ConnSource>() -> OpenApiRouter<Arc<AppState<S, M>>> {
    OpenApiRouter::new()
        .routes(routes!(buckets::list, buckets::create))
        .routes(routes!(buckets::update, buckets::delete))
        .routes(routes!(
            objects::upload,
            objects::put,
            objects::download,
            objects::head,
            objects::delete
        ))
        .routes(routes!(objects::delete_many))
        .routes(routes!(objects::list))
        .routes(routes!(objects::copy))
        .routes(routes!(objects::r#move))
}

pub fn router<S: ConnSource, M: ConnSource>(state: Arc<AppState<S, M>>) -> Router {
    api_router()
        .split_for_parts()
        .0
        .layer(DefaultBodyLimit::max(MAX_BUCKET_BODY))
        .route("/health", get(health))
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    axum::Json(serde_json::json!({ "status": "ok", "service": "storage" }))
}

/// The data key behind the request, and the gateway's request id: only a token minted for
/// [`AUDIENCE`] with a data key's identity.
pub fn identity<S, M>(
    state: &AppState<S, M>,
    headers: &HeaderMap,
) -> Result<(DataIdentity, String), Failure> {
    let token = headers
        .get(IDENTITY_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| Failure::unauthorized("missing identity"))?;
    let claims = state
        .verifier
        .verify(token, AUDIENCE)
        .map_err(|_| Failure::unauthorized("invalid identity"))?;
    match (claims.auth, claims.data) {
        (AuthMethod::DataKey, Some(data)) => Ok((data, claims.rid)),
        _ => Err(Failure::unauthorized("a data API key is required")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chelabase_auth::internal::{Identity, InternalSigner, generate_keypair_pem};
    use chelabase_pgsession::Pools;
    use chelabase_pgsession::test_support::RefusedSource;
    use chelabase_types::data_key::DataKeyKind;
    use uuid::Uuid;

    fn identity_of(data: Option<DataIdentity>) -> Identity {
        Identity {
            sub: Uuid::new_v4().to_string(),
            org_id: String::new(),
            auth: if data.is_some() {
                AuthMethod::DataKey
            } else {
                AuthMethod::ManagementJwt
            },
            request_id: "req-1".into(),
            data,
            email: None,
            name: None,
        }
    }

    fn data() -> DataIdentity {
        DataIdentity {
            branch_id: Uuid::new_v4(),
            key_id: Uuid::new_v4(),
            kind: DataKeyKind::Public,
            role: "anon".into(),
            claims: serde_json::json!({}),
            databases: None,
            project_id: None,
            role_oid: None,
            server_vouched: false,
        }
    }

    /// Only the gateway's token for `storage`, carrying a data key, is trusted: one minted for
    /// rest, a management identity, a forged or missing one are 401.
    #[tokio::test]
    async fn only_the_gateways_storage_identity_is_trusted() {
        let (private, public) = generate_keypair_pem();
        let signer = InternalSigner::from_pem(private.as_bytes()).unwrap();
        let state = AppState {
            engine: Engine::new(Pools::new(RefusedSource, 1), RefusedSource),
            verifier: InternalVerifier::from_pem(public.as_bytes()).unwrap(),
            store: Arc::new(chelabase_storage::MemoryStore::default()),
        };
        let headers = |token: &str| {
            let mut h = HeaderMap::new();
            h.insert(IDENTITY_HEADER, token.parse().unwrap());
            h
        };
        let good = signer.sign(AUDIENCE, &identity_of(Some(data()))).unwrap();
        let (got, rid) = identity(&state, &headers(&good)).unwrap();
        assert_eq!((got.role.as_str(), rid.as_str()), ("anon", "req-1"));
        let (other, _) = generate_keypair_pem();
        for token in [
            signer.sign("rest", &identity_of(Some(data()))).unwrap(),
            signer.sign(AUDIENCE, &identity_of(None)).unwrap(),
            InternalSigner::from_pem(other.as_bytes())
                .unwrap()
                .sign(AUDIENCE, &identity_of(Some(data())))
                .unwrap(),
            "not-a-token".into(),
        ] {
            let err = identity(&state, &headers(&token)).unwrap_err();
            assert_eq!(err.0.status.as_u16(), 401, "{token}");
        }
        assert_eq!(
            identity(&state, &HeaderMap::new())
                .unwrap_err()
                .0
                .status
                .as_u16(),
            401
        );
    }
}
