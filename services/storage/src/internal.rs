//! The console's internal listener (m9): Studio's storage routes as the orchestrator proxies
//! them after its member and Act-as checks, behind a static bearer
//! (`CHELABASE_STORAGE_INTERNAL_TOKEN`, the one storage presents to the orchestrator: both ways,
//! as rest's).
//!
//! Each request names who it runs as in [`CONSOLE_HEADER`], a JSON object
//! `{"branch_id", "database", "role", "claims", "member_id", "request_id"}`: the role and claims
//! of a key whose database list is that one database, and the member as the key id (their
//! multipart sessions are their own; a signed URL they mint lives while they still have
//! Developer access to the branch's database). The orchestrator checked the member may act as
//! that role on that branch; storage still refuses an owner-level role, as for a secret key.
//!
//! The header is read only here, after the bearer: the public listener never looks at it. The
//! caller reaches the handlers through a task-local ([`console`]), so nothing a client sends
//! can set it there.
//!
//! Only the routes Studio's console uses are served, at their public paths: no keyless route
//! (signed and public downloads, signed uploads) and no upload URL. A console download URL lives
//! at most [`MAX_CONSOLE_TTL`].

use std::sync::Arc;

use axum::Router;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, patch, post, put};
use chelabase_auth::internal::DataIdentity;
use chelabase_pgsession::ConnSource;
use chelabase_routes::storage as ep;
use chelabase_types::data_key::DataKeyKind;
use serde::Deserialize;
use serde_json::{Map, Value};
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::app::{AppState, MAX_JSON_BODY};
use crate::error::Failure;
use crate::{buckets, multipart, objects, signed};

/// Who a console request runs as (JSON, see the module docs).
pub const CONSOLE_HEADER: &str = "x-chelabase-console";

/// The longest a download URL minted from the console lives: an hour.
pub const MAX_CONSOLE_TTL: i64 = 3600;

/// A console request's caller: the identity its statements run as, and the orchestrator's
/// request id.
#[derive(Debug, Clone)]
pub struct ConsoleCaller {
    pub data: DataIdentity,
    pub request_id: String,
}

tokio::task_local! {
    static CONSOLE: ConsoleCaller;
}

/// The console caller of the request being handled, on the internal listener only.
pub fn console() -> Option<ConsoleCaller> {
    CONSOLE.try_with(Clone::clone).ok()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    branch_id: Uuid,
    database: String,
    role: String,
    #[serde(default)]
    claims: Map<String, Value>,
    member_id: Uuid,
    request_id: String,
}

/// The caller [`CONSOLE_HEADER`] names: a role and a database are required.
pub fn caller_of(headers: &HeaderMap) -> Result<ConsoleCaller, Failure> {
    let text = headers
        .get(CONSOLE_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| Failure::invalid_request("the console caller is missing"))?;
    let wire: Wire = serde_json::from_str(text)
        .map_err(|e| Failure::invalid_request(format!("the console caller: {e}")))?;
    if wire.role.is_empty() || wire.database.is_empty() {
        return Err(Failure::invalid_request(
            "the console caller names no role or no database",
        ));
    }
    Ok(ConsoleCaller {
        data: DataIdentity {
            branch_id: wire.branch_id,
            key_id: wire.member_id,
            kind: DataKeyKind::Secret,
            role: wire.role,
            claims: Value::Object(wire.claims),
            databases: Some(vec![wire.database]),
            project_id: None,
            role_oid: None,
            server_vouched: false,
        },
        request_id: wire.request_id,
    })
}

/// The bearer the internal listener takes.
#[derive(Clone)]
struct Bearer(Arc<str>);

/// 401 without the bearer (compared in constant time), 400 without a readable caller; else the
/// request runs with its caller set.
async fn console_layer(State(bearer): State<Bearer>, request: Request, next: Next) -> Response {
    let given = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    if !bool::from(given.as_bytes().ct_eq(bearer.0.as_bytes())) {
        return Failure::unauthorized("a valid bearer is required").into_response();
    }
    let caller = match caller_of(request.headers()) {
        Ok(caller) => caller,
        Err(f) => return f.into_response(),
    };
    CONSOLE.scope(caller, next.run(request)).await
}

/// The keyless routes and upload URLs: not the console's.
async fn not_served() -> Response {
    Failure::new(
        axum::http::StatusCode::NOT_FOUND,
        "not_found",
        "not a console route",
    )
    .into_response()
}

/// The console's routes behind `token`.
pub fn internal_router<S: ConnSource, M: ConnSource>(
    state: Arc<AppState<S, M>>,
    token: &str,
) -> Router {
    Router::new()
        .route(
            ep::LIST_BUCKETS.path,
            get(buckets::list::<S, M>).post(buckets::create::<S, M>),
        )
        .route(
            ep::UPDATE_BUCKET.path,
            patch(buckets::update::<S, M>).delete(buckets::delete::<S, M>),
        )
        .route(
            ep::UPLOAD_OBJECT.path,
            post(objects::upload::<S, M>)
                .put(objects::put::<S, M>)
                .get(objects::download::<S, M>)
                .head(objects::head::<S, M>)
                .delete(objects::delete::<S, M>),
        )
        .route(
            ep::DELETE_OBJECTS.path,
            delete(objects::delete_many::<S, M>),
        )
        .route(ep::LIST_OBJECTS.path, post(objects::list::<S, M>))
        .route(ep::COPY_OBJECT.path, post(objects::copy::<S, M>))
        .route(ep::MOVE_OBJECT.path, post(objects::r#move::<S, M>))
        .route(
            ep::SIGN_OBJECT.path,
            post(signed::sign::<S, M>).get(not_served),
        )
        .route(ep::CREATE_UPLOAD.path, post(multipart::create::<S, M>))
        .route(ep::UPLOAD_PART.path, put(multipart::upload_part::<S, M>))
        .route(
            ep::LIST_PARTS.path,
            get(multipart::list_parts::<S, M>).delete(multipart::abort::<S, M>),
        )
        .route(ep::COMPLETE_UPLOAD.path, post(multipart::complete::<S, M>))
        // Matched before the object wildcard, so they never read as a bucket's objects.
        .route(ep::SIGN_UPLOAD.path, any(not_served))
        .route(ep::PUBLIC_DOWNLOAD.path, any(not_served))
        .layer(DefaultBodyLimit::max(MAX_JSON_BODY))
        .layer(from_fn_with_state(Bearer(token.into()), console_layer))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Orchestrator, Urls};
    use crate::engine::Engine;
    use chelabase_auth::internal::{IDENTITY_HEADER, InternalVerifier, generate_keypair_pem};
    use chelabase_pgsession::Pools;
    use serde_json::json;

    const TOKEN: &str = "the-storage-internal-token";

    /// The orchestrator can't say where the branch is: a request that got past every check
    /// before the branch answers 503.
    struct Unreachable;

    impl ConnSource for Unreachable {
        async fn fetch(
            &self,
            _branch: Uuid,
        ) -> Result<chelabase_pgsession::ConnInfo, chelabase_pgsession::ConnError> {
            Err(chelabase_pgsession::ConnError::Unreachable("down".into()))
        }
    }

    fn state() -> Arc<AppState<Unreachable, Unreachable>> {
        let (_, public) = generate_keypair_pem();
        Arc::new(AppState {
            engine: Engine::new(Pools::new(Unreachable, 4), Unreachable),
            verifier: InternalVerifier::from_pem(public.as_bytes()).unwrap(),
            store: Arc::new(chelabase_storage::MemoryStore::default()),
            urls: Urls::new(Arc::new(Orchestrator::new("http://127.0.0.1:9", "t"))),
            inflight: crate::limits::InFlight::new(1 << 30),
        })
    }

    /// The internal router and the public one on local ports.
    async fn serve() -> (String, String) {
        let state = state();
        let internal = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let public = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let bases = (
            format!("http://{}", internal.local_addr().unwrap()),
            format!("http://{}", public.local_addr().unwrap()),
        );
        let router = internal_router(state.clone(), TOKEN);
        tokio::spawn(async move { axum::serve(internal, router).await });
        let router = crate::app::router(state);
        tokio::spawn(async move { axum::serve(public, router).await });
        bases
    }

    fn caller() -> String {
        json!({
            "branch_id": Uuid::new_v4(), "database": "app", "role": "authenticated",
            "claims": { "sub": Uuid::new_v4() }, "member_id": Uuid::new_v4(), "request_id": "req-9",
        })
        .to_string()
    }

    async fn call(
        url: String,
        method: &str,
        headers: &[(&str, String)],
        body: Value,
    ) -> (u16, Value) {
        let mut req = chelabase_http::client_builder()
            .build()
            .unwrap()
            .request(method.parse().unwrap(), url)
            .header("content-type", "application/json")
            .body(body.to_string());
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        let res = req.send().await.unwrap();
        let status = res.status().as_u16();
        (status, res.json().await.unwrap_or_default())
    }

    /// AC 5: without the bearer (missing, wrong, another scheme) every console route is 401,
    /// before the caller is read; with it, a request without a readable caller is 400.
    #[tokio::test]
    async fn the_internal_listener_needs_its_bearer() {
        let (internal, _) = serve().await;
        let routes = [
            ("GET", "/storage/v1/bucket"),
            ("POST", "/storage/v1/object/list/b"),
            ("POST", "/storage/v1/object/b/a.txt"),
            ("GET", "/storage/v1/object/b/a.txt"),
            ("DELETE", "/storage/v1/object/b/a.txt"),
            ("POST", "/storage/v1/object/sign/b/a.txt"),
            ("PUT", "/storage/v1/upload/session/x/1"),
        ];
        for (method, path) in routes {
            for auth in [
                None,
                Some("Bearer wrong".to_owned()),
                Some(format!("Basic {TOKEN}")),
            ] {
                let mut headers = vec![(CONSOLE_HEADER, caller())];
                if let Some(auth) = &auth {
                    headers.push(("authorization", auth.clone()));
                }
                let (status, json) =
                    call(format!("{internal}{path}"), method, &headers, json!({})).await;
                assert_eq!(
                    (status, json["error"]["code"].as_str()),
                    (401, Some("unauthorized")),
                    "{method} {path} {auth:?}"
                );
            }
            let bearer = ("authorization", format!("Bearer {TOKEN}"));
            for bad in ["", "{}", "not json", &json!({ "branch_id": Uuid::new_v4(), "database": "app", "role": "", "member_id": Uuid::new_v4(), "request_id": "r" }).to_string()] {
                let mut headers = vec![bearer.clone()];
                if !bad.is_empty() {
                    headers.push((CONSOLE_HEADER, bad.to_owned()));
                }
                let (status, _) =
                    call(format!("{internal}{path}"), method, &headers, json!({})).await;
                assert_eq!(status, 400, "{method} {path} {bad:?}");
            }
        }
    }

    /// With the bearer and a caller, the request runs as that caller: it gets past every check
    /// to the branch (unreachable here: 503). The keyless routes and upload URLs aren't
    /// served here.
    #[tokio::test]
    async fn a_console_request_runs_as_its_caller() {
        let (internal, _) = serve().await;
        let headers = [
            ("authorization", format!("Bearer {TOKEN}")),
            (CONSOLE_HEADER, caller()),
        ];
        for (method, path) in [
            ("GET", "/storage/v1/bucket"),
            ("POST", "/storage/v1/object/list/b"),
            ("GET", "/storage/v1/object/b/a.txt"),
        ] {
            let (status, json) =
                call(format!("{internal}{path}"), method, &headers, json!({})).await;
            assert_eq!(status, 503, "{method} {path}: {json}");
        }
        for (method, path) in [
            ("GET", "/storage/v1/object/sign/b/a.txt?token=x"),
            ("POST", "/storage/v1/object/upload/sign/b/a.txt"),
            ("PUT", "/storage/v1/object/upload/sign/b/a.txt?token=x"),
            (
                "GET",
                format!("/storage/v1/object/public/{}/b/a.txt", Uuid::new_v4()).leak(),
            ),
        ] {
            let (status, _) = call(format!("{internal}{path}"), method, &headers, json!({})).await;
            assert!(status == 404 || status == 405, "{method} {path}: {status}");
        }
    }

    /// The public listener never reads the console header: alone it's a missing identity
    /// (401), even with the internal bearer beside it.
    #[tokio::test]
    async fn the_public_listener_ignores_the_console_header() {
        let (_, public) = serve().await;
        let headers = [
            ("authorization", format!("Bearer {TOKEN}")),
            (CONSOLE_HEADER, caller()),
        ];
        let (status, json) = call(
            format!("{public}/storage/v1/bucket"),
            "GET",
            &headers,
            json!({}),
        )
        .await;
        assert_eq!(
            (status, json["error"]["code"].as_str()),
            (401, Some("unauthorized"))
        );
        // An identity header on the internal listener is no caller either.
        let (internal, _) = serve().await;
        let headers = [
            ("authorization", format!("Bearer {TOKEN}")),
            (IDENTITY_HEADER, "forged".to_owned()),
        ];
        let (status, _) = call(
            format!("{internal}/storage/v1/bucket"),
            "GET",
            &headers,
            json!({}),
        )
        .await;
        assert_eq!(status, 400);
    }

    /// A console download URL lives at most an hour: longer is 400 before anything is read.
    #[tokio::test]
    async fn a_console_url_lives_at_most_an_hour() {
        let (internal, _) = serve().await;
        let headers = [
            ("authorization", format!("Bearer {TOKEN}")),
            (CONSOLE_HEADER, caller()),
        ];
        let url = format!("{internal}/storage/v1/object/sign/b/a.txt");
        let (status, json) = call(
            url.clone(),
            "POST",
            &headers,
            json!({ "expires_in": MAX_CONSOLE_TTL + 1 }),
        )
        .await;
        assert_eq!(
            (status, json["error"]["code"].as_str()),
            (400, Some("invalid_request")),
            "{json}"
        );
        // An hour gets past the check (to the unreachable branch).
        let (status, _) = call(
            url,
            "POST",
            &headers,
            json!({ "expires_in": MAX_CONSOLE_TTL }),
        )
        .await;
        assert_eq!(status, 503);
    }

    /// The caller's identity: a secret key's path (the owner-level refusal applies), the
    /// member as the key, one database on its list, the claims as sent.
    #[test]
    fn the_header_names_the_caller() {
        let member = Uuid::new_v4();
        let mut headers = HeaderMap::new();
        let text = json!({
            "branch_id": Uuid::nil(), "database": "app", "role": "authenticated",
            "claims": { "sub": "u1" }, "member_id": member, "request_id": "r1",
        })
        .to_string();
        headers.insert(CONSOLE_HEADER, text.parse().unwrap());
        let c = caller_of(&headers).unwrap();
        assert_eq!(c.data.kind, DataKeyKind::Secret);
        assert_eq!(c.data.key_id, member);
        assert_eq!(c.data.databases, Some(vec!["app".to_owned()]));
        assert_eq!(c.data.claims, json!({ "sub": "u1" }));
        assert_eq!(c.request_id, "r1");
        // Claims that aren't an object, or an unknown field, are refused.
        for bad in [
            json!({ "branch_id": Uuid::nil(), "database": "app", "role": "r", "claims": [1], "member_id": member, "request_id": "r" }),
            json!({ "branch_id": Uuid::nil(), "database": "app", "role": "r", "member_id": member, "request_id": "r", "databases": null }),
        ] {
            headers.insert(CONSOLE_HEADER, bad.to_string().parse().unwrap());
            assert!(caller_of(&headers).is_err(), "{bad}");
        }
    }
}
