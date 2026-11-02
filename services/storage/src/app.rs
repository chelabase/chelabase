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

/// The largest JSON request body: delete-many's 1000 names of up to 1024 bytes even with every
/// character `\u` escaped (12 bytes per UTF-8 byte at worst, about 12.5 MB); the names' count
/// and bytes are checked after parsing. Over it: 413. Object uploads stream, unlimited here.
pub const MAX_JSON_BODY: usize = 16 * 1024 * 1024;

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
        .layer(DefaultBodyLimit::max(MAX_JSON_BODY))
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
    /// The orchestrator can't say where the branch is.
    struct Unreachable;

    impl chelabase_pgsession::ConnSource for Unreachable {
        async fn fetch(
            &self,
            _branch: Uuid,
        ) -> Result<chelabase_pgsession::ConnInfo, chelabase_pgsession::ConnError> {
            Err(chelabase_pgsession::ConnError::Unreachable("down".into()))
        }
    }

    /// The router on a local port with `pools` (as authenticator) and `managed`; its base URL
    /// and a valid identity token for a secret key on `authenticated`.
    async fn serve<S: chelabase_pgsession::ConnSource, M: chelabase_pgsession::ConnSource>(
        pools: S,
        managed: M,
    ) -> (String, String) {
        let (private, public) = generate_keypair_pem();
        let signer = InternalSigner::from_pem(private.as_bytes()).unwrap();
        let state = Arc::new(AppState {
            engine: Engine::new(Pools::new(pools, 4), managed),
            verifier: InternalVerifier::from_pem(public.as_bytes()).unwrap(),
            store: Arc::new(chelabase_storage::MemoryStore::default()),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router(state)).await });
        let mut key = data();
        key.kind = DataKeyKind::Secret;
        key.role = "authenticated".into();
        (
            base,
            signer.sign(AUDIENCE, &identity_of(Some(key))).unwrap(),
        )
    }

    // A plain client against a local test server (clippy.toml allows it in tests only).
    #[allow(clippy::disallowed_methods, clippy::disallowed_types)]
    async fn call(
        base: &str,
        token: &str,
        method: &str,
        path: &str,
        body: Vec<u8>,
    ) -> (u16, Option<String>, serde_json::Value) {
        let res = reqwest::Client::new()
            .request(method.parse().unwrap(), format!("{base}{path}"))
            .header(IDENTITY_HEADER, token)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        let retry = res
            .headers()
            .get("retry-after")
            .map(|v| v.to_str().unwrap().to_owned());
        let status = res.status().as_u16();
        (status, retry, res.json().await.unwrap_or_default())
    }

    /// B2: the orchestrator unreachable is 503 with `Retry-After` on an object route.
    #[tokio::test]
    async fn an_unreachable_orchestrator_is_a_503_with_retry_after() {
        let (base, token) = serve(Unreachable, Unreachable).await;
        for (method, path) in [
            ("POST", "/storage/v1/object/b/a.txt"),
            ("GET", "/storage/v1/object/b/a.txt"),
        ] {
            let (status, retry, json) = call(&base, &token, method, path, b"x".to_vec()).await;
            assert_eq!(
                (status, retry.as_deref()),
                (503, Some("2")),
                "{method} {path}: {json}"
            );
            assert_eq!(json["error"]["code"], "unavailable");
        }
    }

    /// B2: storage's own login refused (the compute refuses every connection here) is 503
    /// `storage_unavailable`, before the body is read.
    #[tokio::test]
    async fn a_refused_storage_login_is_storage_unavailable() {
        let (base, token) = serve(RefusedSource, RefusedSource).await;
        let (status, retry, json) = call(
            &base,
            &token,
            "POST",
            "/storage/v1/object/b/a.txt",
            b"x".to_vec(),
        )
        .await;
        assert_eq!((status, retry.as_deref()), (503, Some("2")), "{json}");
        assert_eq!(json["error"]["code"], "storage_unavailable");
        assert!(
            json["error"].get("detail").is_none() && json["error"].get("sqlstate").is_none(),
            "{json}"
        );
    }

    /// N8: delete-many takes its advertised 1000 names of 1024 bytes (it fails here only at
    /// the unreachable orchestrator, after the body was read).
    #[tokio::test]
    async fn delete_many_takes_a_thousand_long_names() {
        let (base, token) = serve(Unreachable, Unreachable).await;
        let names: Vec<String> = (0..1000)
            .map(|i| format!("{i:04}{}", "n".repeat(1020)))
            .collect();
        let body = serde_json::to_vec(&serde_json::json!({ "names": names })).unwrap();
        let (status, _, json) = call(&base, &token, "DELETE", "/storage/v1/object/b", body).await;
        assert_eq!(status, 503, "{json}");
    }

    /// SF1 (the re-audit's test): 1000 names of 1024 bytes as Python's `json.dumps` sends
    /// them, every character `\u` escaped (about 3 MB) are taken.
    #[tokio::test]
    async fn delete_many_takes_escaped_names() {
        let (base, token) = serve(Unreachable, Unreachable).await;
        let one = "\\ud83d\\ude00".repeat(256);
        let body = format!(
            "{{\"names\": [{}]}}",
            (0..1000)
                .map(|_| format!("\"{one}\""))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["names"][0].as_str().unwrap().len(), 1024);
        let (status, _, json) = call(
            &base,
            &token,
            "DELETE",
            "/storage/v1/object/b",
            body.into_bytes(),
        )
        .await;
        assert_eq!(
            status, 503,
            "the advertised 1000 x 1024-byte names were refused: {json}"
        );
    }

    /// SF1: a JSON body over the cap is 413 `too_large`, not 400; 1001 names is 400.
    #[tokio::test]
    async fn an_oversized_json_body_is_413() {
        let (base, token) = serve(Unreachable, Unreachable).await;
        let body = format!("{{\"names\": [\"{}\"]}}", "n".repeat(MAX_JSON_BODY));
        let (status, _, json) = call(
            &base,
            &token,
            "DELETE",
            "/storage/v1/object/b",
            body.into_bytes(),
        )
        .await;
        assert_eq!(
            (status, json["error"]["code"].clone()),
            (413, serde_json::json!("too_large")),
            "{json}"
        );
        let names: Vec<String> = (0..1001).map(|i| i.to_string()).collect();
        let body = serde_json::to_vec(&serde_json::json!({ "names": names })).unwrap();
        assert_eq!(
            call(&base, &token, "DELETE", "/storage/v1/object/b", body)
                .await
                .0,
            400
        );
    }
}
