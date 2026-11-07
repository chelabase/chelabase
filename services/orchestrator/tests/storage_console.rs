//! m9: Studio's storage routes on a branch. The capability matrix (member level × operation),
//! a non-member, a role the member may not assume, and what reaches the storage service's
//! console listener (a fake here, recording each request).
#![allow(dead_code, unused_imports)]
// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

include!("common/fakes.rs");

use axum::body::Body;
use axum::extract::Request;
use axum::response::IntoResponse;
use futures_util::StreamExt;

/// What the fake storage saw of one request.
#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path_and_query: String,
    headers: axum::http::HeaderMap,
    body_len: usize,
}

#[derive(Clone, Default)]
struct FakeStorage {
    seen: Arc<Mutex<Vec<Seen>>>,
    /// Answer every request with this status (200 when unset).
    status: Arc<Mutex<Option<u16>>>,
    /// The big download's first chunk is sent, then it waits for this before the rest.
    go_on: Arc<tokio::sync::Notify>,
}

const MIB: usize = 1024 * 1024;

impl FakeStorage {
    async fn serve() -> (Self, String) {
        let fake = FakeStorage::default();
        let state = fake.clone();
        let app = Router::new().fallback(move |req: Request| {
            let state = state.clone();
            async move { state.answer(req).await }
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (fake, url)
    }

    async fn answer(&self, req: Request) -> axum::response::Response {
        let (parts, body) = req.into_parts();
        let path_and_query = parts
            .uri
            .path_and_query()
            .map(|p| p.to_string())
            .unwrap_or_default();
        // Counted as it streams in, never held whole.
        let mut stream = body.into_data_stream();
        let mut body_len = 0;
        while let Some(chunk) = stream.next().await {
            body_len += chunk.unwrap().len();
        }
        self.seen.lock().unwrap().push(Seen {
            method: parts.method.to_string(),
            path_and_query: path_and_query.clone(),
            headers: parts.headers.clone(),
            body_len,
        });
        if let Some(status) = *self.status.lock().unwrap() {
            return (
                axum::http::StatusCode::from_u16(status).unwrap(),
                axum::Json(json!({ "error": { "code": "x", "message": "x" } })),
            )
                .into_response();
        }
        if parts.uri.path() == "/storage/v1/object/b/big.bin" {
            // 100 MiB, 1 MiB at a time; the rest only once the client has the first chunk.
            let go_on = self.go_on.clone();
            let stream = futures_util::stream::unfold(0usize, move |i| {
                let go_on = go_on.clone();
                async move {
                    if i == 100 {
                        return None;
                    }
                    if i == 1 {
                        go_on.notified().await;
                    }
                    Some((
                        Ok::<_, std::io::Error>(axum::body::Bytes::from(vec![7u8; MIB])),
                        i + 1,
                    ))
                }
            });
            return (
                [
                    ("content-type", "application/octet-stream"),
                    ("x-secret", "no"),
                ],
                Body::from_stream(stream),
            )
                .into_response();
        }
        (
            [
                ("content-type", "application/json"),
                ("etag", "\"e1\""),
                ("set-cookie", "a=b"),
            ],
            axum::Json(json!({ "ok": true, "len": body_len })),
        )
            .into_response()
    }

    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    fn last(&self) -> Seen {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("storage was called")
    }
}

/// A world with a fake storage: `vic` holds `reader` at Viewer, `dee` holds `writer` at
/// Developer, `out` is a Developer of another project; the main database `web1` is served.
async fn world() -> (MemberWorld, FakeStorage) {
    let (storage, url) = FakeStorage::serve().await;
    use_storage(&url);
    let w = member_world(Mode::SelfHosted).await;
    let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
    w.metadata.add_group_role(&ep, "reader", &[]);
    w.metadata.add_group_role(&ep, "writer", &[]);
    w.metadata.add_group_role(&ep, "other", &[]);
    w.metadata.add_granted(
        w.tenant,
        "vic",
        w.project,
        PlatformRole::Viewer,
        &["reader"],
    );
    w.metadata.add_granted(
        w.tenant,
        "dee",
        w.project,
        PlatformRole::Developer,
        &["writer"],
    );
    let elsewhere = w
        .metadata
        .create_project(w.tenant, "Elsewhere")
        .await
        .unwrap()
        .id;
    w.metadata.add_granted(
        w.tenant,
        "out",
        elsewhere,
        PlatformRole::Developer,
        &["writer"],
    );
    w.metadata
        .add_branch_db(w.dev, "web1", "postgres", DatabaseOrigin::Main);
    for d in w
        .metadata
        .branch_dbs
        .lock()
        .unwrap()
        .get_mut(&w.dev)
        .unwrap()
    {
        d.data_api = chelabase_metadata::DataApiState::On;
    }
    (w, storage)
}

fn base(w: &MemberWorld) -> String {
    format!(
        "/v1/databases/{}/branches/{}/storage",
        w.db,
        w.dev.as_uuid()
    )
}

/// Every route: method, tail, whether it only reads.
const ROUTES: &[(&str, &str, bool)] = &[
    ("GET", "/bucket", true),
    ("POST", "/object/list/b", true),
    ("POST", "/object/resolve", true),
    ("GET", "/object/b/dir/a.txt", true),
    ("POST", "/bucket", false),
    ("PATCH", "/bucket/b", false),
    ("DELETE", "/bucket/b", false),
    ("POST", "/object/b/dir/a.txt", false),
    ("PUT", "/object/b/dir/a.txt", false),
    ("DELETE", "/object/b/dir/a.txt", false),
    ("DELETE", "/object/b", false),
    ("POST", "/object/copy", false),
    ("POST", "/object/move", false),
    ("POST", "/object/sign/b/a.txt", false),
    ("POST", "/upload/b/big.bin", false),
    (
        "PUT",
        "/upload/session/6f1c0b6e-9d2a-4a57-8a77-1f3c2a9b4e10/1",
        false,
    ),
    (
        "GET",
        "/upload/session/6f1c0b6e-9d2a-4a57-8a77-1f3c2a9b4e10",
        false,
    ),
    (
        "POST",
        "/upload/session/6f1c0b6e-9d2a-4a57-8a77-1f3c2a9b4e10/complete",
        false,
    ),
    (
        "DELETE",
        "/upload/session/6f1c0b6e-9d2a-4a57-8a77-1f3c2a9b4e10",
        false,
    ),
];

async fn call(w: &MemberWorld, sub: &str, method: &str, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .request(method.parse().unwrap(), format!("{}{path}", w.app.base))
        .header(IDENTITY_HEADER, w.app.token_for(sub, "org-1"))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap()
}

/// AC 1, 6 (and the matrix): a Viewer lists and downloads; every write, a signed URL and the
/// resumable upload's steps are 403 `required: developer`... as `role:<name>` for the role
/// held at Viewer, before storage is called. A Developer reaches storage on every route.
#[tokio::test]
async fn the_capability_matrix() {
    let (w, storage) = world().await;
    for &(method, tail, read) in ROUTES {
        let path = format!("{}{tail}?role=reader", base(&w));
        let before = storage.calls();
        let resp = call(&w, "vic", method, &path).await;
        if read {
            assert_eq!(resp.status(), 200, "viewer {method} {tail}");
            assert_eq!(storage.calls(), before + 1, "viewer {method} {tail}");
        } else {
            let status = resp.status();
            let body: Value = resp.json().await.unwrap();
            assert_eq!(status, 403, "viewer {method} {tail}: {body}");
            assert_eq!(body["error"]["code"], "forbidden", "{body}");
            assert_eq!(
                storage.calls(),
                before,
                "viewer {method} {tail} reached storage"
            );
        }
        let path = format!("{}{tail}?role=writer", base(&w));
        let before = storage.calls();
        let resp = call(&w, "dee", method, &path).await;
        assert_eq!(resp.status(), 200, "developer {method} {tail}");
        assert_eq!(storage.calls(), before + 1);
        let seen = storage.last();
        assert_eq!(seen.method, method);
    }
}

/// AC 2: a member with no grant on the project is 404 on every route (as a missing database),
/// before storage is called; so is a branch of another database (path ids vs. membership).
#[tokio::test]
async fn a_non_member_is_404_everywhere() {
    let (w, storage) = world().await;
    for &(method, tail, _) in ROUTES {
        let path = format!("{}{tail}?role=writer", base(&w));
        let resp = call(&w, "out", method, &path).await;
        assert_eq!(resp.status(), 404, "{method} {tail}");
    }
    // `dee`'s database with a branch of another database.
    let mut other = sample_database(w.tenant);
    other.project_id = w.project;
    let (other_id, other_main) = (other.database_id, other.main.id);
    w.metadata.insert_database(other);
    let foreign = format!(
        "/v1/databases/{}/branches/{}/storage/bucket?role=writer",
        w.db,
        other_main.as_uuid()
    );
    assert_eq!(call(&w, "dee", "GET", &foreign).await.status(), 404);
    let _ = other_id;
    assert_eq!(storage.calls(), 0);
}

/// AC 4: a role the member may not assume (not in their set, the owner, a reserved role, none
/// at all) is refused before storage is called; so is one the worker refuses.
#[tokio::test]
async fn a_role_the_member_may_not_assume_is_refused_first() {
    let (w, storage) = world().await;
    for (role, status) in [
        ("other", 403),
        ("authenticated", 403),
        ("postgres", 403),
        ("reader", 403),
    ] {
        let path = format!("{}/bucket?role={role}", base(&w));
        let resp = call(&w, "dee", "GET", &path).await;
        assert_eq!(resp.status(), status, "{role}");
    }
    for query in [
        "?role=",
        "?role=writer&claims=%5B1%5D",
        "?role=writer&claims=nope",
    ] {
        let path = format!("{}/bucket{query}", base(&w));
        assert_eq!(
            call(&w, "dee", "GET", &path).await.status(),
            422,
            "{query:?}"
        );
    }
    // No role (owner mode) needs Admin: a Developer is refused before storage.
    let path = format!("{}/bucket", base(&w));
    assert_eq!(call(&w, "dee", "GET", &path).await.status(), 403);
    *w.jobs.role_check.lock().unwrap() =
        Some(Ok(CheckDataRoleReply::refused("the role is owner-level")));
    let path = format!("{}/bucket?role=writer", base(&w));
    let resp = call(&w, "dee", "GET", &path).await;
    assert_eq!(resp.status(), 403);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["error"]["code"],
        "role_refused"
    );
    assert_eq!(storage.calls(), 0);
}

/// What storage receives: its bearer, the caller (branch, served database, role, claims, the
/// member as the key), the path under `/storage/v1`, only `upsert`, `download` and the
/// database of the query, and only the listed headers (never the client's identity or a
/// console header it made up). Only the listed headers come back.
#[tokio::test]
async fn storage_gets_the_caller_and_nothing_smuggled() {
    let (w, storage) = world().await;
    let dee = w
        .metadata
        .members
        .lock()
        .unwrap()
        .iter()
        .find(|m| m.identity_user_id == "dee")
        .map(|m| m.id);
    let claims = urlencoding::encode(r#"{"sub":"u-1","role":"authenticated"}"#).into_owned();
    let path = format!(
        "{}/object/b/dir/a%20b.txt?role=writer&claims={claims}&upsert=true&download=x.txt&database=web1&evil=1",
        base(&w)
    );
    let resp = reqwest::Client::new()
        .post(format!("{}{path}", w.app.base))
        .header(IDENTITY_HEADER, w.app.token_for("dee", "org-1"))
        .header("content-type", "text/plain")
        .header("x-upsert", "true")
        .header("x-chelabase-console", r#"{"role":"postgres"}"#)
        .header("cookie", "s=1")
        .body("hello")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["etag"], "\"e1\"");
    assert!(resp.headers().get("set-cookie").is_none());
    let seen = storage.last();
    assert_eq!(
        seen.path_and_query,
        "/storage/v1/object/b/dir/a%20b.txt?database=web1&upsert=true&download=x.txt"
    );
    assert_eq!(
        seen.headers["authorization"],
        format!("Bearer {STORAGE_TOKEN}")
    );
    assert_eq!(seen.headers["content-type"], "text/plain");
    assert_eq!(seen.headers["x-upsert"], "true");
    assert_eq!(seen.body_len, 5);
    for absent in [IDENTITY_HEADER, "cookie"] {
        assert!(
            seen.headers.get(absent).is_none(),
            "{absent} reached storage"
        );
    }
    let consoles: Vec<_> = seen.headers.get_all("x-chelabase-console").iter().collect();
    assert_eq!(consoles.len(), 1, "one caller, ours");
    let caller: Value = serde_json::from_slice(consoles[0].as_bytes()).unwrap();
    assert_eq!(caller["branch_id"], json!(w.dev.as_uuid()));
    assert_eq!(caller["database"], "web1");
    assert_eq!(caller["role"], "writer");
    assert_eq!(
        caller["claims"],
        json!({ "sub": "u-1", "role": "authenticated" })
    );
    assert_eq!(caller["member_id"], json!(dee.unwrap()));
}

/// AC 7: an 8 MiB multipart part goes through the console whole (no 2 MB cap here).
#[tokio::test]
async fn an_eight_mib_part_goes_through() {
    let (w, storage) = world().await;
    let path = format!(
        "{}/upload/session/6f1c0b6e-9d2a-4a57-8a77-1f3c2a9b4e10/1?role=writer",
        base(&w)
    );
    let resp = reqwest::Client::new()
        .put(format!("{}{path}", w.app.base))
        .header(IDENTITY_HEADER, w.app.token_for("dee", "org-1"))
        .header("content-type", "application/octet-stream")
        .body(vec![1u8; 8 * MIB])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(storage.last().body_len, 8 * MIB);
}

/// AC 8: a 100 MiB download streams: the first chunk reaches the client before storage has
/// sent the second (a buffered answer would never arrive), then all of it.
#[tokio::test]
async fn a_large_download_streams() {
    let (w, storage) = world().await;
    let path = format!("{}/object/b/big.bin?role=reader", base(&w));
    let resp = call(&w, "vic", "GET", &path).await;
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("x-secret").is_none());
    let mut stream = resp.bytes_stream();
    let first = tokio::time::timeout(std::time::Duration::from_secs(10), stream.next())
        .await
        .expect("the first chunk came before the whole body")
        .unwrap()
        .unwrap();
    let mut total = first.len();
    storage.go_on.notify_one();
    while let Some(chunk) = stream.next().await {
        total += chunk.unwrap().len();
    }
    assert_eq!(total, 100 * MIB);
}

/// Storage refusing the orchestrator's bearer is a 502 (never a 401: Studio would sign the
/// user out); its other answers pass through as they are.
#[tokio::test]
async fn storage_refusing_our_bearer_is_502() {
    let (w, storage) = world().await;
    let path = format!("{}/bucket?role=writer", base(&w));
    *storage.status.lock().unwrap() = Some(401);
    assert_eq!(call(&w, "dee", "GET", &path).await.status(), 502);
    *storage.status.lock().unwrap() = Some(403);
    assert_eq!(call(&w, "dee", "GET", &path).await.status(), 403);
}

/// NATS revocation (moved from m6): revoking, rotating or narrowing a secret key tells storage
/// to drop that key's cached liveness; changing a database's connection settings tells it to
/// drop each branch's connection info and pools. Nothing is published for a no-op.
#[tokio::test]
async fn key_and_settings_changes_tell_storage() {
    use chelabase_nats::StorageEvent;
    let (w, _storage) = world().await;
    w.metadata
        .add_member(w.tenant, "boss", MemberStatus::Active, true);
    let keys = format!("/v1/databases/{}/branches/{}/keys", w.db, w.dev.as_uuid());
    let made: Value = w
        .app
        .send_as(
            "boss",
            "org-1",
            Method::POST,
            &keys,
            Some(json!({ "name": "k1", "role": "writer" })),
        )
        .await
        .json()
        .await
        .unwrap();
    let id: uuid::Uuid = made["id"].as_str().expect("a key id").parse().unwrap();
    let events = || w.jobs.storage_events.lock().unwrap().clone();
    assert!(events().is_empty());
    let key = format!("{keys}/{id}");
    let resp = w
        .app
        .send_as(
            "boss",
            "org-1",
            Method::PATCH,
            &key,
            Some(json!({ "databases": ["web1"] })),
        )
        .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(events(), vec![StorageEvent::Key { key_id: id }]);
    let resp = w
        .app
        .send_as(
            "boss",
            "org-1",
            Method::POST,
            &format!("{key}/rotate"),
            None,
        )
        .await;
    assert_eq!(resp.status(), 201);
    let new_id: uuid::Uuid = resp.json::<Value>().await.unwrap()["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(events().last(), Some(&StorageEvent::Key { key_id: id }));
    let before = events().len();
    let resp = w
        .app
        .send_as(
            "boss",
            "org-1",
            Method::DELETE,
            &format!("{keys}/{new_id}"),
            None,
        )
        .await;
    assert_eq!(resp.status(), 204);
    assert_eq!(events()[before..], [StorageEvent::Key { key_id: new_id }]);

    // Settings: storage's budget (or the data API's) changed: one event per branch.
    let before = events().len();
    let db = format!("/v1/databases/{}", w.db);
    let resp = w
        .app
        .send_as(
            "boss",
            "org-1",
            Method::PATCH,
            &db,
            Some(json!({ "storage_max_connections": 7 })),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let mut branches: Vec<_> = events()[before..].to_vec();
    branches.sort_by_key(|e| format!("{e:?}"));
    let mut want = vec![
        StorageEvent::Branch {
            branch_id: w.dev.as_uuid(),
        },
        StorageEvent::Branch {
            branch_id: w.db.as_uuid(),
        },
    ];
    want.sort_by_key(|e| format!("{e:?}"));
    assert_eq!(branches, want);
    // The same value again, or another setting: nothing.
    let before = events().len();
    let resp = w
        .app
        .send_as(
            "boss",
            "org-1",
            Method::PATCH,
            &db,
            Some(json!({ "storage_max_connections": 7, "max_rows": 500 })),
        )
        .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(events().len(), before);
}

/// Owner mode (the human's option 3): no `?role=`, `?role=owner` or the owner's own name runs
/// as storage's own login, on every route. It needs Admin on the database (a tenant admin, or
/// an Admin grant); below it, 403 before storage is called. Storage is told owner mode and no
/// role or claims (objects written so have no owner); no worker role check is made.
#[tokio::test]
async fn owner_mode_needs_admin_and_names_no_role() {
    let (w, storage) = world().await;
    w.metadata
        .add_member(w.tenant, "boss", MemberStatus::Active, true);
    w.metadata
        .add_granted(w.tenant, "ada", w.project, PlatformRole::Admin, &[]);
    for &(method, tail, _) in ROUTES {
        for owner in ["", "?role=owner", "?role=postgres", "?claims=%7B%7D"] {
            let path = format!("{}{tail}{owner}", base(&w));
            for sub in ["vic", "dee"] {
                let before = storage.calls();
                let resp = call(&w, sub, method, &path).await;
                assert_eq!(resp.status(), 403, "{sub} {method} {tail}{owner}");
                assert_eq!(
                    storage.calls(),
                    before,
                    "{sub} {method} {tail}{owner} reached storage"
                );
            }
        }
        for sub in ["boss", "ada"] {
            let path = format!("{}{tail}", base(&w));
            let checks = w.jobs.role_checks.lock().unwrap().len();
            let resp = call(&w, sub, method, &path).await;
            assert_eq!(resp.status(), 200, "{sub} {method} {tail}");
            assert_eq!(
                w.jobs.role_checks.lock().unwrap().len(),
                checks,
                "no role check for the owner"
            );
            let seen = storage.last();
            let caller: Value =
                serde_json::from_slice(seen.headers["x-chelabase-console"].as_bytes()).unwrap();
            assert_eq!(caller["owner"], true, "{caller}");
            assert!(
                caller.get("role").is_none() && caller.get("claims").is_none(),
                "{caller}"
            );
            assert_eq!(caller["database"], "web1");
        }
    }
    // Claims mean nothing to the owner: refused rather than dropped silently.
    let path = format!("{}/bucket?claims=%7B%22sub%22%3A%22u%22%7D", base(&w));
    assert_eq!(call(&w, "boss", "GET", &path).await.status(), 422);
}

/// Sends `method path` as written (no client normalises it) and returns the status.
async fn raw(w: &MemberWorld, sub: &str, method: &str, path: &str) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let addr = w.app.base.trim_start_matches("http://").to_string();
    let mut s = tokio::net::TcpStream::connect(&addr).await.unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nhost: x\r\n{IDENTITY_HEADER}: {}\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{{}}",
        w.app.token_for(sub, "org-1")
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let head = String::from_utf8_lossy(&out);
    head.split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// B1 (the m9 audit): a `.` or `..` segment, raw or percent-encoded, must not turn the route
/// the orchestrator authorized into another one at storage.
#[tokio::test]
async fn adv_dot_segments_never_change_the_route() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (w, storage) = world().await;
    let addr = w.app.base.trim_start_matches("http://").to_string();
    let session = "6f1c0b6e-9d2a-4a57-8a77-1f3c2a9b4e10";
    let mut leaks = Vec::new();
    for (method, tail) in [
        ("GET", format!("/object/b/../../upload/session/{session}")),
        (
            "GET",
            format!("/object/b/%2e%2e/%2e%2e/upload/session/{session}"),
        ),
        (
            "GET",
            format!("/object/b/.%2E/.%2e/upload/session/{session}"),
        ),
        ("POST", "/object/list/..".to_string()),
    ] {
        let path = format!("{}{tail}?role=reader", base(&w));
        let before = storage.calls();
        let mut s = tokio::net::TcpStream::connect(&addr).await.unwrap();
        let req = format!(
            "{method} {path} HTTP/1.1\r\nhost: x\r\n{IDENTITY_HEADER}: {}\r\ncontent-type: application/json\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{{}}",
            w.app.token_for("vic", "org-1")
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        if storage.calls() > before {
            let seen = storage.last();
            if !seen.path_and_query.starts_with("/storage/v1/object/")
                || seen.path_and_query.contains("upload/session")
            {
                leaks.push(format!(
                    "{method} {tail} -> storage {} {}",
                    seen.method, seen.path_and_query
                ));
            }
        }
    }
    assert!(
        leaks.is_empty(),
        "a Viewer route reached another storage route: {leaks:#?}"
    );
}

/// The invariant behind B1, exhaustively over a small alphabet: for every console path made of
/// route words, `.`, `..` and their percent-encodings (any case), either the orchestrator
/// refuses it without calling storage, or storage receives exactly the route the orchestrator
/// matched and authorized (`/storage/v1` + the tail as sent).
#[tokio::test]
async fn dot_segments_are_refused_or_reach_storage_unchanged() {
    const WORDS: &[&str] = &[
        "object", "b", "upload", "bucket", ".", "..", "%2e", "%2E%2e", ".%2E", "%2e.",
    ];
    let (w, storage) = world().await;
    let mut tails = vec![String::new()];
    let mut all = Vec::new();
    for _ in 0..3 {
        tails = tails
            .iter()
            .flat_map(|t| WORDS.iter().map(move |word| format!("{t}/{word}")))
            .collect();
        all.extend(tails.iter().cloned());
    }
    let mut wrong = Vec::new();
    for tail in &all {
        for method in ["GET", "POST"] {
            let path = format!("{}{tail}?role=writer", base(&w));
            let before = storage.calls();
            let status = raw(&w, "dee", method, &path).await;
            if storage.calls() == before {
                continue;
            }
            let seen = storage.last();
            let got = seen.path_and_query.split('?').next().unwrap();
            if got != format!("/storage/v1{tail}") {
                wrong.push(format!("{method} {tail} ({status}) -> {got}"));
            }
        }
    }
    assert!(wrong.is_empty(), "storage got another route: {wrong:#?}");
}

/// #100 (the resolve route's audit, B1): a member who is a Developer through their own grant
/// (`writer`) and holds `reader` only at Viewer through a team grant. Console `sign` as
/// `reader` is 403 before storage; resolve as `reader` (a Viewer route) reaches storage naming
/// `reader` and the member, which storage binds into every URL it mints, so the URL's liveness
/// is judged on `reader`'s level (`internal::a_console_url_needs_its_role_at_developer`).
#[tokio::test]
async fn resolve_as_a_viewer_level_role_of_a_developer_names_that_role() {
    use chelabase_types::access::{Grant, PgRole, RoleSet};
    let (w, storage) = world().await;
    let mix = w.metadata.add_granted(w.tenant, "mix", w.project, PlatformRole::Developer, &["writer"]);
    w.metadata.team_grant(
        mix,
        w.project,
        Grant {
            platform_role: PlatformRole::Viewer,
            roles: RoleSet::Only([PgRole::Named("reader".into())].into()),
        },
    );
    let before = storage.calls();
    let resp = call(&w, "mix", "POST", &format!("{}/object/sign/b/a.txt?role=reader", base(&w))).await;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["error"]["required"], "role:reader", "{body}");
    assert_eq!(storage.calls(), before, "sign as a Viewer-level role reached storage");
    let resp = call(&w, "mix", "POST", &format!("{}/object/resolve?role=reader", base(&w))).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(storage.calls(), before + 1);
    let seen = storage.last();
    assert_eq!(seen.path_and_query, "/storage/v1/object/resolve?database=web1");
    let caller: Value = serde_json::from_slice(seen.headers["x-chelabase-console"].as_bytes()).unwrap();
    assert_eq!(caller["role"], "reader", "{caller}");
    assert_eq!(caller["member_id"], json!(mix), "{caller}");
    assert!(caller.get("owner").is_none_or(|o| o == false), "{caller}");
}
