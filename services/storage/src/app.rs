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

use crate::control::Urls;
use crate::engine::Engine;
use crate::error::Failure;
use crate::{buckets, multipart, objects, public, signed};

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
    /// Signed URLs' key, key liveness and upload tokens (m6).
    pub urls: Urls,
    /// The cap on upload buffers in this process (m7).
    pub inflight: crate::limits::InFlight,
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
        .routes(routes!(signed::sign, signed::signed_download))
        .routes(routes!(signed::sign_upload, signed::signed_upload))
        .routes(routes!(public::public_download))
        .routes(routes!(multipart::create))
        .routes(routes!(multipart::upload_part))
        .routes(routes!(multipart::list_parts, multipart::abort))
        .routes(routes!(multipart::complete))
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
            urls: Urls::new(Arc::new(crate::control::Orchestrator::new("http://127.0.0.1:9", "t"))),
            inflight: crate::limits::InFlight::new(1 << 30),
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
        let (base, token, _) = serve_with(pools, managed, Arc::new(FakeControl::default())).await;
        (base, token)
    }

    /// The control plane stand-in: a fixed URL key, the live keys, the upload tokens.
    #[derive(Default)]
    struct FakeControl {
        live: std::sync::Mutex<Vec<crate::control::LiveKey>>,
        /// token id -> (key id, claims, spent)
        uploads: std::sync::Mutex<std::collections::HashMap<Uuid, (Uuid, serde_json::Value, bool)>>,
    }

    const URL_KEY: crate::token::UrlKey = crate::token::UrlKey { kid: 1, key: [3; 32] };

    impl crate::control::Control for FakeControl {
        fn url_key(&self) -> crate::control::BoxFuture<'_, Result<crate::token::UrlKey, crate::control::Unreachable>> {
            Box::pin(async { Ok(URL_KEY) })
        }
        fn live_key(&self, key_id: Uuid) -> crate::control::BoxFuture<'_, Result<Option<crate::control::LiveKey>, crate::control::Unreachable>> {
            let hit = self.live.lock().unwrap().iter().find(|k| k.key_id == key_id).cloned();
            Box::pin(async move { Ok(hit) })
        }
        fn record_upload<'a>(&'a self, token_id: Uuid, key_id: Uuid, claims: &'a serde_json::Value, exp: u64) -> crate::control::BoxFuture<'a, Result<Option<u64>, crate::control::Unreachable>> {
            let ok = self.uploads.lock().unwrap().insert(token_id, (key_id, claims.clone(), false)).is_none();
            Box::pin(async move { Ok(ok.then_some(exp)) })
        }
        fn consume_upload(&self, token_id: Uuid) -> crate::control::BoxFuture<'_, Result<Option<(crate::control::LiveKey, serde_json::Value)>, crate::control::Unreachable>> {
            let mut uploads = self.uploads.lock().unwrap();
            let out = match uploads.get_mut(&token_id) {
                Some((key, claims, spent)) if !*spent => {
                    *spent = true;
                    self.live.lock().unwrap().iter().find(|k| k.key_id == *key).cloned().map(|k| (k, claims.clone()))
                }
                _ => None,
            };
            Box::pin(async move { Ok(out) })
        }
    }

    /// [`serve`] with `control`; also the key's identity.
    async fn serve_with<S: chelabase_pgsession::ConnSource, M: chelabase_pgsession::ConnSource>(
        pools: S,
        managed: M,
        control: Arc<FakeControl>,
    ) -> (String, String, DataIdentity) {
        let (private, public) = generate_keypair_pem();
        let signer = InternalSigner::from_pem(private.as_bytes()).unwrap();
        let state = Arc::new(AppState {
            engine: Engine::new(Pools::new(pools, 4), managed),
            verifier: InternalVerifier::from_pem(public.as_bytes()).unwrap(),
            store: Arc::new(chelabase_storage::MemoryStore::default()),
            urls: Urls::new(control),
            inflight: crate::limits::InFlight::new(1 << 30),
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router(state)).await });
        let mut key = data();
        key.kind = DataKeyKind::Secret;
        key.role = "authenticated".into();
        let token = signer.sign(AUDIENCE, &identity_of(Some(key.clone()))).unwrap();
        (base, token, key)
    }

    // The shared no-redirect client, like every other client in the workspace.
    async fn call(
        base: &str,
        token: &str,
        method: &str,
        path: &str,
        body: Vec<u8>,
    ) -> (u16, Option<String>, serde_json::Value) {
        let res = chelabase_http::client_builder()
            .build()
            .unwrap()
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

    fn live_of(data: &DataIdentity) -> crate::control::LiveKey {
        crate::control::LiveKey {
            key_id: data.key_id,
            branch_id: data.branch_id,
            kind: data.kind,
            role: data.role.clone(),
            databases: None,
            role_oid: None,
        }
    }

    fn download_claims(data: &DataIdentity, exp: u64) -> crate::token::Claims {
        crate::token::Claims {
            purpose: crate::token::Purpose::Download,
            database_id: Uuid::new_v4(),
            branch_id: data.branch_id,
            database: "app".into(),
            datoid: Some(16384),
            key_id: data.key_id,
            bucket: "b".into(),
            name: "a.txt".into(),
            version_id: Some(Uuid::new_v4()),
            size: Some(1),
            etag: Some("e".into()),
            content_type: Some("text/html".into()),
            role: None,
            role_oid: None,
            session_id: None,
            upsert: false,
            token_id: Uuid::new_v4(),
            exp,
        }
    }

    /// AC 2, 6, 7: a signed GET with a missing, changed, expired or other object's token, or
    /// one whose key was revoked or moved to another branch, is refused before storage asks
    /// the branch anything (an unreachable orchestrator here would make that a 503) or reads
    /// the store. A good token gets as far as the branch.
    #[tokio::test]
    async fn a_bad_signed_url_is_refused_before_any_read() {
        let control = Arc::new(FakeControl::default());
        let (base, _, data) = serve_with(Unreachable, Unreachable, control.clone()).await;
        control.live.lock().unwrap().push(live_of(&data));
        let now = crate::signed::now();
        let good = crate::token::sign(&URL_KEY, &download_claims(&data, now + 60));
        let expired = crate::token::sign(&URL_KEY, &download_claims(&data, now - 1));
        let other_key = crate::token::sign(&crate::token::UrlKey { kid: 1, key: [4; 32] }, &download_claims(&data, now + 60));
        let mut moved = download_claims(&data, now + 60);
        moved.branch_id = Uuid::new_v4();
        let other_branch = crate::token::sign(&URL_KEY, &moved);
        let mut gone = download_claims(&data, now + 60);
        gone.key_id = Uuid::new_v4();
        let revoked = crate::token::sign(&URL_KEY, &gone);
        let tampered = format!("{}x", &good[..good.len() - 1]);
        let get = |path: String| {
            let base = base.clone();
            async move {
                let (status, _, json) = call(&base, "", "GET", &path, Vec::new()).await;
                (status, json["error"]["code"].as_str().unwrap_or_default().to_owned())
            }
        };
        let at = |t: &str| format!("/storage/v1/object/sign/b/a.txt?token={t}");
        for (path, want) in [
            ("/storage/v1/object/sign/b/a.txt".to_owned(), (401, "invalid_token")),
            (at(&tampered), (401, "invalid_token")),
            (at(&other_key), (401, "invalid_token")),
            (at(&expired), (401, "token_expired")),
            (format!("/storage/v1/object/sign/b/other.txt?token={good}"), (403, "token_not_valid")),
            (format!("/storage/v1/object/sign/c/a.txt?token={good}"), (403, "token_not_valid")),
            (at(&other_branch), (403, "token_not_valid")),
            (at(&revoked), (403, "token_not_valid")),
        ] {
            assert_eq!(get(path.clone()).await, (want.0, want.1.to_owned()), "{path}");
        }
        // The good one passes every check and reaches the branch (unreachable here).
        assert_eq!(get(at(&good)).await.0, 503);
    }

    /// AC 8, 11: an upload token is spent once; the second use is refused before the body.
    /// An upload token never opens a download, nor a download token an upload.
    #[tokio::test]
    async fn an_upload_token_is_used_once() {
        let control = Arc::new(FakeControl::default());
        let (base, key, data) = serve_with(Unreachable, Unreachable, control.clone()).await;
        control.live.lock().unwrap().push(live_of(&data));
        let now = crate::signed::now();
        let token_id = Uuid::new_v4();
        control.uploads.lock().unwrap().insert(token_id, (data.key_id, serde_json::json!({}), false));
        let claims = crate::token::Claims {
            purpose: crate::token::Purpose::Upload,
            datoid: None,
            version_id: None,
            size: None,
            etag: None,
            content_type: None,
            role: Some(data.role.clone()),
            token_id,
            ..download_claims(&data, now + 60)
        };
        let token = crate::token::sign(&URL_KEY, &claims);
        let path = format!("/storage/v1/object/upload/sign/b/a.txt?token={token}");
        // The first use is spent and goes on to upload (the branch is unreachable here).
        let (first, _, _) = call(&base, "", "PUT", &path, b"x".to_vec()).await;
        assert_eq!(first, 503);
        let (second, _, json) = call(&base, "", "PUT", &path, b"x".to_vec()).await;
        assert_eq!((second, json["error"]["code"].as_str()), (403, Some("token_not_valid")), "{json}");
        // Not a download token.
        let (status, _, _) = call(&base, "", "GET", &format!("/storage/v1/object/sign/b/a.txt?token={token}"), Vec::new()).await;
        assert_eq!(status, 403);
        let download = crate::token::sign(&URL_KEY, &download_claims(&data, now + 60));
        let (status, _, _) = call(&base, "", "PUT", &format!("/storage/v1/object/upload/sign/b/a.txt?token={download}"), b"x".to_vec()).await;
        assert_eq!(status, 403);
        // AC 10: minting with a TTL over 7 days is 400 before anything is read.
        for route in ["sign", "upload/sign"] {
            let body = serde_json::to_vec(&serde_json::json!({ "expires_in": 604_801 })).unwrap();
            let (status, _, json) = call(&base, &key, "POST", &format!("/storage/v1/object/{route}/b/a.txt"), body).await;
            assert_eq!(status, 400, "{route}: {json}");
        }
    }

    /// m6 re-audit S1: an upload refused for its `Content-Type` (400) is refused before the
    /// token is spent, so the URL still takes the corrected upload.
    #[tokio::test]
    #[allow(clippy::disallowed_methods, clippy::disallowed_types)]
    async fn a_refused_upload_keeps_its_token() {
        let control = Arc::new(FakeControl::default());
        let (base, _, data) = serve_with(Unreachable, Unreachable, control.clone()).await;
        control.live.lock().unwrap().push(live_of(&data));
        let token_id = Uuid::new_v4();
        control.uploads.lock().unwrap().insert(token_id, (data.key_id, serde_json::json!({}), false));
        let claims = crate::token::Claims {
            purpose: crate::token::Purpose::Upload,
            datoid: None,
            version_id: None,
            size: None,
            etag: None,
            content_type: None,
            role: Some(data.role.clone()),
            token_id,
            ..download_claims(&data, crate::signed::now() + 60)
        };
        let token = crate::token::sign(&URL_KEY, &claims);
        let url = format!("{base}/storage/v1/object/upload/sign/b/a.txt?token={token}");
        let put = |content_type: &'static str| {
            let url = url.clone();
            async move {
                let res = reqwest::Client::new()
                    .put(url)
                    .header("content-type", content_type)
                    .body(b"x".to_vec())
                    .send()
                    .await
                    .unwrap();
                let status = res.status().as_u16();
                let json: serde_json::Value = res.json().await.unwrap_or_default();
                (status, json["error"]["code"].as_str().unwrap_or_default().to_owned())
            }
        };
        for bad in ["text/plain; charset=\"utf-8\"", "text/plain,text/html"] {
            assert_eq!(put(bad).await, (400, "invalid_request".to_owned()), "{bad}");
            assert!(!control.uploads.lock().unwrap()[&token_id].2, "{bad} spent the token");
        }
        // The corrected retry spends it and goes on to upload (the branch is unreachable here).
        assert_eq!(put("text/plain; charset=utf-8").await.0, 503);
        assert!(control.uploads.lock().unwrap()[&token_id].2);
    }

    /// AC 1, 4, 6 over `MemoryStore` (m6 audit B2, B5): a download token serves its version
    /// while it is the name's current one; overwritten, deleted, re-created under the same
    /// name, deleted on its branch while another branch still holds the blob, or its database
    /// re-created: 404, though the blob is still in the store. (Which version is current is
    /// storage's own read on the branch: `SIGNED_ROW`.)
    #[tokio::test]
    async fn a_signed_download_serves_only_its_current_version() {
        use chelabase_storage::{ObjectStore, PutOptions};
        let store = Arc::new(chelabase_storage::MemoryStore::default());
        let (_, public) = generate_keypair_pem();
        let state = AppState {
            engine: Engine::new(Pools::new(RefusedSource, 1), RefusedSource),
            verifier: InternalVerifier::from_pem(public.as_bytes()).unwrap(),
            store: store.clone(),
            urls: Urls::new(Arc::new(FakeControl::default())),
            inflight: crate::limits::InFlight::new(1 << 20),
        };
        let claims = download_claims(&data(), crate::signed::now() + 60);
        let (datoid, version) = (claims.datoid.unwrap(), claims.version_id.unwrap());
        let database_id = claims.database_id;
        let key = crate::names::version_key(&crate::names::object_prefix(database_id, datoid), version);
        store
            .put(&key, bytes::Bytes::from_static(b"SECRET-v1"), PutOptions { content_type: None, if_none_match: false })
            .await
            .unwrap();
        let found = crate::objects::Found {
            datoid,
            version,
            size: 9,
            etag: "e".into(),
            content_type: "text/plain".into(),
            owner: None,
        };
        // AC 1: current, it serves the bytes, sandboxed.
        crate::signed::still_current(&claims, Some(datoid), Some(version)).unwrap();
        let out = crate::objects::serve(&state, &HeaderMap::new(), database_id, &found, "a.txt", None, "r").await.unwrap();
        assert_eq!(out.status(), 200);
        assert_eq!(out.headers()["content-security-policy"], "sandbox");
        let body = axum::body::to_bytes(out.into_body(), 1 << 20).await.unwrap();
        assert_eq!(&body[..], b"SECRET-v1");
        // AC 4, 6b: the blob stays (another branch may name it), but the URL doesn't open it.
        for (oid, current) in [
            (Some(datoid), Some(Uuid::new_v4())), // overwritten, or deleted and re-created
            (Some(datoid), None),                 // deleted on its branch
            (Some(datoid + 1), Some(version)),    // the database re-created
            (None, Some(version)),
        ] {
            let err = crate::signed::still_current(&claims, oid, current).unwrap_err();
            assert_eq!((err.0.status.as_u16(), err.0.code), (404, "object_not_found"), "{oid:?} {current:?}");
        }
        assert!(store.head(&key).await.is_ok());
        // A token without a version (not a download's) never matches.
        let upload = crate::token::Claims { version_id: None, ..claims.clone() };
        assert!(crate::signed::still_current(&upload, Some(datoid), None).is_err());
    }

    /// The orchestrator says the branch doesn't exist.
    struct NoBranch;

    impl chelabase_pgsession::ConnSource for NoBranch {
        async fn fetch(
            &self,
            _branch: Uuid,
        ) -> Result<chelabase_pgsession::ConnInfo, chelabase_pgsession::ConnError> {
            Err(chelabase_pgsession::ConnError::UnknownBranch)
        }
    }

    /// AC 5: a public path on an unknown branch, or one that isn't a branch id, is the
    /// object's 404 (never the data API's 401); a branch storage can't reach is 503.
    #[tokio::test]
    async fn a_public_path_off_any_branch_is_404() {
        let (base, _) = serve(NoBranch, NoBranch).await;
        for path in [
            format!("/storage/v1/object/public/{}/b/a.txt", Uuid::new_v4()),
            "/storage/v1/object/public/not-a-branch/b/a.txt".to_owned(),
        ] {
            let (status, _, json) = call(&base, "", "GET", &path, Vec::new()).await;
            assert_eq!((status, json["error"]["code"].as_str()), (404, Some("object_not_found")), "{path}");
        }
        let (base, _) = serve(Unreachable, Unreachable).await;
        let path = format!("/storage/v1/object/public/{}/b/a.txt", Uuid::new_v4());
        assert_eq!(call(&base, "", "GET", &path, Vec::new()).await.0, 503);
    }
}
