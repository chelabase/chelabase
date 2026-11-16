//! s3-endpoint m2: a branch's S3 access keys through the Management API. Who may list, create
//! and revoke them (the data keys' rules: Developer for a role the member may use, Admin for
//! any), the role identity a key is bound to, the database it reaches, the endpoint being off,
//! and the storage event a revoke publishes.
#![allow(dead_code, unused_imports)]
// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

include!("common/fakes.rs");

const ENDPOINT: &str = "https://s3.example.test";

/// A world with the S3 endpoint at `endpoint`: on the `dev` branch, `dan` is a Developer
/// granted `authenticated` and `reporting`, `ada` an Admin, `vic` a Viewer granted `reporting`;
/// `other` is a role nobody is granted and `power` an owner-level one. The branch has the main
/// database `web1`, a database `extra` made in SQL and the system `postgres`. The worker finds
/// every role as OID 16390 on timeline `tl1`.
async fn world(endpoint: Option<&str>) -> MemberWorld {
    use_s3_endpoint(endpoint);
    let w = member_world(Mode::SelfHosted).await;
    use_s3_endpoint(None);
    let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
    for role in ["authenticated", "reporting", "other"] {
        w.metadata.add_group_role(&ep, role, &[]);
    }
    w.metadata
        .add_role(&ep, "power", RoleOrigin::Sql, true, false);
    w.metadata.add_granted(
        w.tenant,
        "dan",
        w.project,
        PlatformRole::Developer,
        &["authenticated", "reporting"],
    );
    w.metadata
        .add_granted(w.tenant, "ada", w.project, PlatformRole::Admin, &[]);
    w.metadata.add_granted(
        w.tenant,
        "vic",
        w.project,
        PlatformRole::Viewer,
        &["reporting"],
    );
    w.metadata
        .add_branch_db(w.dev, "web1", "postgres", DatabaseOrigin::Main);
    w.metadata
        .add_branch_db(w.dev, "extra", "postgres", DatabaseOrigin::Sql);
    w.metadata
        .add_branch_db(w.dev, "postgres", "postgres", DatabaseOrigin::System);
    *w.jobs.role_check.lock().unwrap() = Some(Ok(CheckDataRoleReply::ok_with_oid(16390, "tl1")));
    w
}

fn s3_keys(w: &MemberWorld, branch: BranchId) -> String {
    format!(
        "/v1/databases/{}/branches/{}/storage/s3-keys",
        w.db,
        branch.as_uuid()
    )
}

async fn send(
    w: &MemberWorld,
    sub: &str,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> reqwest::Response {
    w.app.send_as(sub, "org-1", method, path, body).await
}

async fn create(w: &MemberWorld, sub: &str, body: Value) -> reqwest::Response {
    send(w, sub, Method::POST, &s3_keys(w, w.dev), Some(body)).await
}

async fn status_and_body(resp: reqwest::Response) -> (u16, Value) {
    let status = resp.status().as_u16();
    let body = resp.json().await.unwrap_or(Value::Null);
    (status, body)
}

async fn list(w: &MemberWorld, sub: &str) -> Value {
    let resp = send(w, sub, Method::GET, &s3_keys(w, w.dev), None).await;
    let (status, body) = status_and_body(resp).await;
    assert_eq!(status, 200, "{body}");
    body
}

fn stored(w: &MemberWorld) -> Vec<FakeS3Key> {
    w.metadata.s3_keys.lock().unwrap().clone()
}

fn events(w: &MemberWorld) -> Vec<chelabase_nats::StorageEvent> {
    w.jobs.storage_events.lock().unwrap().clone()
}

/// AC 1: a Developer who may act as `authenticated` makes a key with only a name. 201 with an
/// access key id, a 40-character secret, the main database and the default role, never cached;
/// a following GET lists it without its secret.
#[tokio::test]
async fn developer_creates_and_lists() {
    let w = world(Some(ENDPOINT)).await;
    let resp = create(&w, "dan", json!({ "name": "ci" })).await;
    assert_eq!(
        resp.headers()
            .get("cache-control")
            .map(|v| v.to_str().unwrap()),
        Some("no-store")
    );
    let (status, created) = status_and_body(resp).await;
    assert_eq!(status, 201, "{created}");
    let access_key_id = created["access_key_id"].as_str().unwrap();
    assert!(
        chelabase_types::s3_key::is_access_key_id(access_key_id),
        "{access_key_id}"
    );
    let secret = created["secret_access_key"].as_str().unwrap();
    assert_eq!(secret.len(), 40);
    assert!(secret.bytes().all(|b| b.is_ascii_alphanumeric()));
    assert_eq!(created["database"], "web1");
    assert_eq!(created["role"], "authenticated");
    assert_eq!(created["name"], "ci");
    assert!(created["created_at"].is_string());

    // Stored bound to the role's identity, with that secret.
    let keys = stored(&w);
    assert_eq!(keys.len(), 1);
    let key = &keys[0];
    assert_eq!(key.record.id.to_string(), created["id"].as_str().unwrap());
    assert_eq!(key.record.branch_id, w.dev);
    assert_eq!(
        (key.record.role_oid, key.record.role_timeline.as_str()),
        (16390, "tl1")
    );
    assert_eq!(key.secret.as_deref(), Some(secret));
    assert_eq!(
        w.jobs.role_checks.lock().unwrap().as_slice(),
        &[(w.dev, "authenticated".to_string())]
    );

    let listed = list(&w, "dan").await;
    assert_eq!(listed["enabled"], true);
    assert_eq!(listed["endpoint"], ENDPOINT);
    assert_eq!(listed["region"], "us-east-1");
    let keys = listed["keys"].as_array().unwrap();
    assert_eq!(keys.len(), 1);
    let k = keys[0].as_object().unwrap();
    let mut fields: Vec<&str> = k.keys().map(String::as_str).collect();
    fields.sort_unstable();
    assert_eq!(
        fields,
        [
            "access_key_id",
            "created_at",
            "database",
            "id",
            "last_used_at",
            "name",
            "role"
        ],
        "{listed}"
    );
    assert_eq!(k["access_key_id"], access_key_id);
    assert_eq!(k["last_used_at"], Value::Null);
    assert!(
        !listed.to_string().contains(secret),
        "the list never carries the secret"
    );

    // A named database and role the Developer holds.
    let (status, body) = status_and_body(
        create(
            &w,
            "dan",
            json!({ "name": "rep", "database": "extra", "role": "reporting" }),
        )
        .await,
    )
    .await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(
        (body["database"].as_str(), body["role"].as_str()),
        (Some("extra"), Some("reporting"))
    );
    assert_eq!(
        list(&w, "dan").await["keys"][0]["name"],
        "rep",
        "newest first"
    );
}

/// The body's shape: a name of 1 to 63 characters, no unknown fields, no empty role or database.
#[tokio::test]
async fn create_validates_the_body() {
    let w = world(Some(ENDPOINT)).await;
    for body in [
        json!({}),
        json!({ "name": "" }),
        json!({ "name": "x".repeat(64) }),
        json!({ "name": "ok", "role": "" }),
        json!({ "name": "ok", "database": "" }),
        json!({ "name": "ok", "surprise": 1 }),
        json!({ "name": 5 }),
    ] {
        let (status, err) = status_and_body(create(&w, "dan", body.clone()).await).await;
        assert_eq!(status, 422, "{body}: {err}");
        assert_eq!(err["error"]["code"], "invalid_request", "{body}");
    }
    assert!(stored(&w).is_empty());
    assert!(
        w.jobs.role_checks.lock().unwrap().is_empty(),
        "nothing was asked of the worker"
    );
}

/// AC 2: a Developer can't bind a key to a role they may not act as (403 `role:<name>`), nor to
/// a personal login; an Admin may bind any role but a personal login.
#[tokio::test]
async fn role_refinement() {
    let w = world(Some(ENDPOINT)).await;
    let (status, err) =
        status_and_body(create(&w, "dan", json!({ "name": "no", "role": "other" })).await).await;
    assert_eq!(
        (status, err["error"]["required"].as_str()),
        (403, Some("role:other")),
        "{err}"
    );
    // An owner-level role is Admin-only for a Developer, even granted.
    let (status, err) =
        status_and_body(create(&w, "dan", json!({ "name": "no", "role": "postgres" })).await).await;
    assert_eq!(
        (status, err["error"]["required"].as_str()),
        (403, Some("admin")),
        "{err}"
    );
    assert!(stored(&w).is_empty());
    assert!(
        w.jobs.role_checks.lock().unwrap().is_empty(),
        "refused before the worker"
    );

    let (status, body) =
        status_and_body(create(&w, "ada", json!({ "name": "any", "role": "other" })).await).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["role"], "other");

    // A personal login: nobody's.
    w.metadata
        .personal_login_names
        .lock()
        .unwrap()
        .insert((w.db.as_uuid(), "alice".to_string()));
    for sub in ["dan", "ada"] {
        let (status, err) =
            status_and_body(create(&w, sub, json!({ "name": "pl", "role": "alice" })).await).await;
        assert_eq!(
            (status, err["error"]["required"].as_str()),
            (403, Some("role:alice")),
            "{sub}: {err}"
        );
    }
    assert_eq!(stored(&w).len(), 1);
}

/// AC 3: an owner-level role is refused with 422, even for an Admin, and nothing is stored:
/// whether the metadata knows the role as owner-level or only the worker's check finds it.
#[tokio::test]
async fn owner_level_role_refused() {
    let w = world(Some(ENDPOINT)).await;
    let (status, err) =
        status_and_body(create(&w, "ada", json!({ "name": "pow", "role": "power" })).await).await;
    assert_eq!(
        (status, err["error"]["code"].as_str()),
        (422, Some("role_refused")),
        "{err}"
    );
    *w.jobs.role_check.lock().unwrap() = Some(Ok(CheckDataRoleReply::refused(
        "owner-level roles can't back a secret key",
    )));
    let (status, err) =
        status_and_body(create(&w, "ada", json!({ "name": "pow", "role": "other" })).await).await;
    assert_eq!(
        (status, err["error"]["code"].as_str()),
        (422, Some("role_refused")),
        "{err}"
    );
    assert!(stored(&w).is_empty());
}

/// AC 4: a database the branch doesn't have, or the system database, is 422 and nothing is
/// stored; so is a branch whose main database was dropped (no default).
#[tokio::test]
async fn unserved_database_refused() {
    let w = world(Some(ENDPOINT)).await;
    for database in ["nope", "postgres", "WEB1"] {
        let (status, err) =
            status_and_body(create(&w, "dan", json!({ "name": "k", "database": database })).await)
                .await;
        assert_eq!(
            (status, err["error"]["code"].as_str()),
            (422, Some("invalid_request")),
            "{database}: {err}"
        );
    }
    w.metadata
        .branch_dbs
        .lock()
        .unwrap()
        .get_mut(&w.dev)
        .unwrap()
        .retain(|d| d.origin != DatabaseOrigin::Main);
    let (status, err) = status_and_body(create(&w, "dan", json!({ "name": "k" })).await).await;
    assert_eq!(
        (status, err["error"]["code"].as_str()),
        (422, Some("invalid_request")),
        "{err}"
    );
    assert!(stored(&w).is_empty());
}

/// The role identity AC: without a role OID and timeline there is no key. A missing role is
/// 422, an unreachable compute or worker 503 (504 on the worker's timeout), and an answer
/// without the identity (a worker that predates it) 503: never a key bound by name only.
#[tokio::test]
async fn no_role_identity_no_key() {
    let w = world(Some(ENDPOINT)).await;
    let cases: Vec<(Result<CheckDataRoleReply, ResetError>, u16)> = vec![
        (
            Ok(CheckDataRoleReply::refused(
                "role \"authenticated\" does not exist",
            )),
            422,
        ),
        (
            Ok(CheckDataRoleReply::failed(
                "connecting to the compute: refused",
            )),
            503,
        ),
        (
            Err(ResetError::Transport("no worker listening".into())),
            503,
        ),
        (Err(ResetError::Timeout), 504),
        (Ok(CheckDataRoleReply::ok()), 503),
        (Ok(CheckDataRoleReply::not_ready()), 409),
    ];
    for (answer, want) in cases {
        *w.jobs.role_check.lock().unwrap() = Some(answer.clone());
        let (status, err) = status_and_body(create(&w, "dan", json!({ "name": "k" })).await).await;
        assert_eq!(status, want, "{answer:?}: {err}");
    }
    assert!(stored(&w).is_empty());
}

/// Branches a key can't be made on: a backup (409 `backup_branch`), one not ready (409), and
/// a branch being deleted.
#[tokio::test]
async fn unusable_branches_refused() {
    let w = world(Some(ENDPOINT)).await;
    for (kind, status, code) in [
        (BranchKind::Backup, BranchStatus::Ready, "backup_branch"),
        (
            BranchKind::Normal,
            BranchStatus::Provisioning,
            "branch_not_ready",
        ),
        (
            BranchKind::Normal,
            BranchStatus::Deleting,
            "branch_not_ready",
        ),
    ] {
        {
            let mut branches = w.metadata.branches.lock().unwrap();
            let b = branches.get_mut(&w.dev).unwrap();
            (b.kind, b.status) = (kind, status);
        }
        let (got, err) = status_and_body(create(&w, "ada", json!({ "name": "k" })).await).await;
        assert_eq!(
            (got, err["error"]["code"].as_str()),
            (409, Some(code)),
            "{kind:?} {status:?}: {err}"
        );
    }
    assert!(stored(&w).is_empty());
}

/// AC 5: with `CHELABASE_S3_HOST` unset the list says so and creating is 409 `s3_disabled`
/// (before the worker is asked); revoking an existing key still works.
#[tokio::test]
async fn disabled_endpoint() {
    let w = world(None).await;
    let listed = list(&w, "vic").await;
    assert_eq!(listed["enabled"], false);
    assert_eq!(listed["endpoint"], Value::Null);
    assert_eq!(listed["region"], "us-east-1");
    assert_eq!(listed["keys"], json!([]));
    let (status, err) = status_and_body(create(&w, "ada", json!({ "name": "k" })).await).await;
    assert_eq!(
        (status, err["error"]["code"].as_str()),
        (409, Some("s3_disabled")),
        "{err}"
    );
    assert!(w.jobs.role_checks.lock().unwrap().is_empty());

    // A key made while the endpoint was on.
    let id = seed(&w, w.dev, "authenticated");
    assert_eq!(list(&w, "vic").await["keys"].as_array().unwrap().len(), 1);
    let resp = send(
        &w,
        "dan",
        Method::DELETE,
        &format!("{}/{id}", s3_keys(&w, w.dev)),
        None,
    )
    .await;
    assert_eq!(resp.status(), 204);
    assert_eq!(
        events(&w),
        vec![chelabase_nats::StorageEvent::Key { key_id: id }]
    );
    assert_eq!(list(&w, "vic").await["keys"], json!([]));
}

/// Stores a live key on `branch` for `role` as m1's store would (`web1`, OID 16390 on `tl1`).
fn seed(w: &MemberWorld, branch: BranchId, role: &str) -> uuid::Uuid {
    let id = uuid::Uuid::new_v4();
    let pair = chelabase_types::s3_key::generate();
    w.metadata.s3_keys.lock().unwrap().push(FakeS3Key {
        record: S3KeyRecord {
            id,
            branch_id: branch,
            database: "web1".into(),
            role: role.into(),
            name: "seeded".into(),
            access_key_id: pair.access_key_id,
            role_oid: 16390,
            role_timeline: "tl1".into(),
            created_at: chrono::Utc::now(),
            last_used_at: None,
        },
        secret: Some(pair.secret),
    });
    id
}

/// AC 6: revoking a live key is 204, publishes `StorageEvent::Key` with its id and drops it
/// from the list; revoking it again is 404 and publishes nothing more.
#[tokio::test]
async fn revoke_publishes_and_hides() {
    let w = world(Some(ENDPOINT)).await;
    let (status, created) = status_and_body(create(&w, "dan", json!({ "name": "ci" })).await).await;
    assert_eq!(status, 201);
    let id: uuid::Uuid = created["id"].as_str().unwrap().parse().unwrap();
    let one = format!("{}/{id}", s3_keys(&w, w.dev));
    let resp = send(&w, "dan", Method::DELETE, &one, None).await;
    assert_eq!(resp.status(), 204);
    assert_eq!(
        events(&w),
        vec![chelabase_nats::StorageEvent::Key { key_id: id }]
    );
    assert_eq!(list(&w, "dan").await["keys"], json!([]));
    assert_eq!(stored(&w)[0].secret, None, "the secret is erased");
    let (status, err) = status_and_body(send(&w, "dan", Method::DELETE, &one, None).await).await;
    assert_eq!(
        (status, err["error"]["code"].as_str()),
        (404, Some("not_found")),
        "{err}"
    );
    let (status, _) = status_and_body(
        send(
            &w,
            "dan",
            Method::DELETE,
            &format!("{}/{}", s3_keys(&w, w.dev), uuid::Uuid::new_v4()),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(status, 404);
    assert_eq!(events(&w).len(), 1);
}

/// AC 6: a key of another branch, through this branch's (or another database's) path, is 404
/// and stays live; a Developer revoking a key of a role they may not use is 403, an Admin may.
#[tokio::test]
async fn revoke_other_branch_is_404() {
    let w = world(Some(ENDPOINT)).await;
    let main = w
        .metadata
        .branches
        .lock()
        .unwrap()
        .values()
        .find(|b| b.database_id == w.db && b.id != w.dev)
        .expect("the main branch")
        .id;
    let on_main = seed(&w, main, "authenticated");
    let resp = send(
        &w,
        "ada",
        Method::DELETE,
        &format!("{}/{on_main}", s3_keys(&w, w.dev)),
        None,
    )
    .await;
    assert_eq!(resp.status(), 404);

    // Another database of the tenant: its path never reaches this database's keys.
    let mut other = sample_database(w.tenant);
    other.project_id = w.project;
    let (other_db, other_main) = (other.database_id, other.main.id);
    w.metadata.insert_database(other);
    let on_dev = seed(&w, w.dev, "authenticated");
    for path in [
        format!(
            "/v1/databases/{other_db}/branches/{}/storage/s3-keys/{on_dev}",
            w.dev.as_uuid()
        ),
        format!(
            "/v1/databases/{other_db}/branches/{}/storage/s3-keys/{on_dev}",
            other_main.as_uuid()
        ),
    ] {
        let resp = send(&w, "ada", Method::DELETE, &path, None).await;
        assert_eq!(resp.status(), 404, "{path}");
    }
    assert!(events(&w).is_empty());
    assert!(
        stored(&w).iter().all(|k| k.secret.is_some()),
        "both keys stay live"
    );

    // The key's role refines the revoke as it refines the create.
    let theirs = seed(&w, w.dev, "other");
    let one = format!("{}/{theirs}", s3_keys(&w, w.dev));
    let (status, err) = status_and_body(send(&w, "dan", Method::DELETE, &one, None).await).await;
    assert_eq!(
        (status, err["error"]["required"].as_str()),
        (403, Some("role:other")),
        "{err}"
    );
    assert_eq!(
        send(&w, "ada", Method::DELETE, &one, None).await.status(),
        204
    );
    assert_eq!(
        events(&w),
        vec![chelabase_nats::StorageEvent::Key { key_id: theirs }]
    );
}

/// AC 7: a Viewer lists the keys; creating and revoking are 403, even for a role they hold.
#[tokio::test]
async fn viewer_reads_only() {
    let w = world(Some(ENDPOINT)).await;
    let id = seed(&w, w.dev, "reporting");
    let listed = list(&w, "vic").await;
    assert_eq!(listed["keys"][0]["id"], id.to_string());
    let (status, err) =
        status_and_body(create(&w, "vic", json!({ "name": "k", "role": "reporting" })).await).await;
    assert_eq!(
        (status, err["error"]["code"].as_str()),
        (403, Some("forbidden")),
        "{err}"
    );
    let resp = send(
        &w,
        "vic",
        Method::DELETE,
        &format!("{}/{id}", s3_keys(&w, w.dev)),
        None,
    )
    .await;
    assert_eq!(resp.status(), 403);
    assert_eq!(stored(&w).len(), 1);
    assert!(stored(&w)[0].secret.is_some());
    assert!(events(&w).is_empty());
    // A non-member sees nothing.
    let resp = send(&w, "stranger", Method::GET, &s3_keys(&w, w.dev), None).await;
    assert!(
        matches!(resp.status().as_u16(), 403 | 404),
        "{}",
        resp.status()
    );
}

/// A busy endpoint (a DDL transaction holds it) is 503 with `Retry-After`; nothing is stored.
#[tokio::test]
async fn busy_endpoint_is_503() {
    let w = world(Some(ENDPOINT)).await;
    *w.metadata.endpoint_busy.lock().unwrap() = true;
    let resp = create(&w, "dan", json!({ "name": "k" })).await;
    assert_eq!(resp.status(), 503);
    assert!(resp.headers().contains_key("retry-after"));
    assert!(stored(&w).is_empty());
}
