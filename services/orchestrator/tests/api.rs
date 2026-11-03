// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

// The fakes and the test app, shared with `access_walk.rs`.
include!("common/fakes.rs");

#[tokio::test]
async fn health_is_public() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = reqwest::get(format!("{}/health", app.base)).await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn openapi_json_route_serves_the_document() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = reqwest::get(format!("{}/v1/openapi.json", app.base)).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "application/json");
    assert_eq!(resp.headers()["cache-control"], "no-cache");
    let served: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(served, serde_json::to_value(orchestrator::openapi()).unwrap());
    let committed = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../openapi/management.json")).unwrap();
    assert_eq!(served, serde_json::from_str::<serde_json::Value>(&committed).unwrap());
}

#[tokio::test]
async fn docs_page_is_gone() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    for path in ["/v1/docs", "/v1/docs/scalar.js", "/v1/docs/nope.js"] {
        let resp = reqwest::get(format!("{}{path}", app.base)).await.unwrap();
        assert_eq!(resp.status(), 404, "{path}");
    }
}

#[tokio::test]
async fn missing_identity_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = post_create(&app, None, create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 401);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(code_of(&body), "unauthorized");
    assert!(message_of(&body).contains("gateway"));
}

#[tokio::test]
async fn bearer_token_without_identity_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/databases", app.base))
        .bearer_auth("some-unsigned-jwt")
        .json(&create_body(TenantId::new()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn garbage_identity_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = post_create(&app, Some("not-a-jwt"), create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn expired_identity_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let token = app
        .signer
        .sign_at(Service::Orchestrator.name(), &identity_of(DEFAULT_SUB), now - 3600)
        .unwrap();
    let resp = post_create(&app, Some(&token), create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn identity_minted_for_another_service_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let token = app.signer.sign("some-other-service", &identity_of(DEFAULT_SUB)).unwrap();
    let resp = post_create(&app, Some(&token), create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn identity_from_untrusted_key_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let (other_private, _) = generate_keypair_pem();
    let forger = InternalSigner::from_pem(other_private.as_bytes()).unwrap();
    let token = forger
        .sign(Service::Orchestrator.name(), &identity_of(DEFAULT_SUB))
        .unwrap();
    let resp = post_create(&app, Some(&token), create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 401);
}

// #126: an identity that names no tenant (no member token) is 401: nothing finds a tenant
// any other way.
#[tokio::test]
async fn an_identity_without_a_tenant_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let token = app.token_for_org("no-such-tenant");
    let resp = post_create(&app, Some(&token), create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 401);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(code_of(&body), "unauthorized");
}

#[tokio::test]
async fn valid_request_is_202_accepted_and_publishes_job() {
    let metadata = FakeMetadata::default();
    let bound_tenant = TenantId::new();
    metadata.bind_org("org-1", bound_tenant);
    let jobs = FakeJobs::default();
    let project_id = metadata.create_project(bound_tenant, "Default").await.unwrap().id;
    let app = spawn_app(metadata, jobs.clone()).await;
    let token = app.token_for_org("org-1");

    let resp = post_create(
        &app,
        Some(&token),
        serde_json::json!({
            "project_id": project_id,
            "name": "demo-db",
        }),
    )
    .await;

    assert_eq!(resp.status(), 202);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "pending");
    assert!(body["job_id"].is_string());
    assert!(body["database_id"].is_string());

    let published = jobs.published.lock().unwrap();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].payload.tenant_id, bound_tenant);
    assert_eq!(published[0].payload.project_id, project_id);
    assert_eq!(published[0].payload.name, "demo-db");
}

// A name that parses but is invalid (blank, reserved, too long) is a 422 on every operation
// that takes one; reserved or over-long database names would otherwise hand the owner a
// system database or fail provisioning with a confusing timeout.
#[tokio::test]
async fn invalid_names_are_422_everywhere() {
    let metadata = FakeMetadata::default();
    let bound_tenant = TenantId::new();
    metadata.bind_org("org-1", bound_tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app(metadata, jobs.clone()).await;
    let token = app.token_for_org("org-1");
    for name in ["   ".to_string(), "postgres".to_string(), "template1".to_string(), "x".repeat(64)] {
        let resp = post_create(
            &app,
            Some(&token),
            serde_json::json!({ "project_id": ProjectId::new(), "name": name }),
        )
        .await;
        let err = error_of(resp, 422).await;
        assert_eq!(code_of(&err), "invalid_request", "database {name:?}");
    }
    assert!(jobs.published.lock().unwrap().is_empty());
    let t = branching().await;
    for name in ["", "Dev", "a b", "main"] {
        let resp = t.send(Method::POST, &t.branches(), Some(json!({ "name": name }))).await;
        let err = error_of(resp, 422).await;
        assert_eq!(code_of(&err), "invalid_request", "branch {name:?}");
    }
}

#[tokio::test]
async fn malformed_json_is_400() {
    let metadata = FakeMetadata::default();
    metadata.bind_org("org-1", TenantId::new());
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/databases", app.base))
        .header(IDENTITY_HEADER, app.token_for_org("org-1"))
        .header("content-type", "application/json")
        .body("{nope")
        .send()
        .await
        .unwrap();
    let err = error_of(resp, 400).await;
    assert_eq!(code_of(&err), "invalid_request");
}

#[tokio::test]
async fn password_resets_are_204() {
    let jobs = FakeJobs::default();
    let mut id = None;
    let app = lifecycle_app(jobs.clone(), |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 204);
    assert!(resp.bytes().await.unwrap().is_empty());
    let t = branching().await;
    let dev = t.ready("dev");
    let resp = t.send(Method::POST, &format!("{}/reset-password", t.branch(dev)), None).await;
    assert_eq!(resp.status(), 204);
    assert!(resp.bytes().await.unwrap().is_empty());
}

#[tokio::test]
async fn duplicate_database_name_is_409() {
    let metadata = FakeMetadata::default();
    let bound_tenant = TenantId::new();
    metadata.bind_org("org-1", bound_tenant);
    metadata.fail_next_insert();
    let project = metadata.create_project(bound_tenant, "Default").await.unwrap().id;
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let token = app.token_for_org("org-1");
    let body = json!({ "project_id": project, "name": "demo" });
    let resp = post_create(&app, Some(&token), body).await;
    assert_eq!(resp.status(), 409);
}

#[tokio::test]
async fn get_database_own_tenant_is_200() {
    let metadata = FakeMetadata::default();
    let bound_tenant = TenantId::new();
    metadata.bind_org("org-1", bound_tenant);
    let record = sample_database(bound_tenant);
    let database_id = record.database_id;
    metadata.insert_database(record);
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let token = app.token_for_org("org-1");

    let resp = reqwest::Client::new()
        .get(format!("{}/v1/databases/{}", app.base, database_id.as_uuid()))
        .header(IDENTITY_HEADER, token)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["tenant_id"], serde_json::json!(bound_tenant));
}

#[tokio::test]
async fn other_tenants_database_is_404_like_a_missing_one() {
    let metadata = FakeMetadata::default();
    metadata.bind_org("org-1", TenantId::new());
    let record = sample_database(TenantId::new());
    let database_id = record.database_id;
    metadata.insert_database(record);
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let token = app.token_for_org("org-1");

    let resp = reqwest::Client::new()
        .get(format!("{}/v1/databases/{}", app.base, database_id.as_uuid()))
        .header(IDENTITY_HEADER, token)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(code_of(&body), "not_found");
    // Byte for byte what a database that doesn't exist answers.
    let missing = reqwest::Client::new()
        .get(format!("{}/v1/databases/{}", app.base, uuid::Uuid::new_v4()))
        .header(IDENTITY_HEADER, app.token_for_org("org-1"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
    let missing_body: serde_json::Value = missing.json().await.unwrap();
    assert_eq!(body, missing_body);
}

#[tokio::test]
async fn get_database_unknown_id_is_404() {
    let metadata = FakeMetadata::default();
    metadata.bind_org("org-1", TenantId::new());
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let token = app.token_for_org("org-1");

    let resp = reqwest::Client::new()
        .get(format!("{}/v1/databases/{}", app.base, uuid::Uuid::new_v4()))
        .header(IDENTITY_HEADER, token)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
}

fn overview_with_database(tenant_id: TenantId) -> TenantOverview {
    let project_id = ProjectId::new();
    let mut db = sample_database(tenant_id);
    db.project_id = project_id;
    db.name = "web1".into();
    TenantOverview {
        id: tenant_id,
        slug: "demo".into(),
        display_name: None,
        projects: vec![ProjectOverview {
            id: project_id,
            name: "Default".into(),
            slug: "default".into(),
            status: ProjectStatus::Active,
            databases: vec![db.record],
        }],
    }
}

async fn get_me(app: &TestApp, identity_header: Option<&str>) -> reqwest::Response {
    let mut req = reqwest::Client::new().get(format!("{}/v1/me", app.base));
    if let Some(token) = identity_header {
        req = req.header(IDENTITY_HEADER, token);
    }
    req.send().await.unwrap()
}

#[tokio::test]
async fn me_returns_tenant_projects_and_databases() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    metadata.set_overview(overview_with_database(tenant));
    let app = spawn_app(metadata, FakeJobs::default()).await;

    let resp = get_me(&app, Some(&app.token_for_org("org-1"))).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["user"]["sub"], "user-1");
    assert_eq!(body["tenant"]["id"], serde_json::json!(tenant));
    assert_eq!(body["tenant"]["slug"], "demo");
    let project = &body["tenant"]["projects"][0];
    assert_eq!(project["slug"], "default");
    let db = &project["databases"][0];
    assert_eq!(db["name"], "web1");
    assert_eq!(db["status"], "ready");
    assert_eq!(db["host"], "ep-0123456789ab.localtest.me");
    assert_eq!(db["port"], 4432);
    assert!(db["database_id"].is_string());
}

#[tokio::test]
async fn me_without_a_tenant_returns_null_tenant() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = get_me(&app, Some(&app.token_for_org("no-tenant"))).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["user"], json!({ "sub": DEFAULT_SUB }));
    assert!(body["tenant"].is_null());
}

// Review Focus 2.
#[tokio::test]
async fn me_with_a_missing_tenant_row_returns_null_tenant() {
    let metadata = FakeMetadata::default();
    metadata.bind_org("org-1", TenantId::new());
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let resp = get_me(&app, Some(&app.token_for_org("org-1"))).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["tenant"].is_null());
}

#[tokio::test]
async fn me_without_identity_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    assert_eq!(get_me(&app, None).await.status(), 401);
}

#[tokio::test]
async fn me_never_exposes_database_secrets() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    metadata.set_overview(overview_with_database(tenant));
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let text = get_me(&app, Some(&app.token_for_org("org-1")))
        .await
        .text()
        .await
        .unwrap();
    for secret in [
        "secret-owner-sealed",
        "owner_password_enc",
        "endpoint_id",
    ] {
        assert!(!text.contains(secret), "/v1/me leaked {secret}: {text}");
    }
}

#[tokio::test]
async fn get_deleted_database_is_404() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let mut record = sample_database(tenant);
    record.status = DatabaseStatus::Deleted;
    let id = record.database_id;
    metadata.insert_database(record);
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/databases/{}", app.base, id.as_uuid()))
        .header(IDENTITY_HEADER, app.token_for_org("org-1"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

// GET /v1/databases/{id} never returns credentials or container ids.
#[tokio::test]
async fn get_database_never_exposes_secrets() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let db = overview_with_database(tenant).projects[0].databases[0].clone();
    let id = db.database_id;
    metadata.insert_database(db);
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let text = reqwest::Client::new()
        .get(format!("{}/v1/databases/{}", app.base, id.as_uuid()))
        .header(IDENTITY_HEADER, app.token_for_org("org-1"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for secret in ["secret-owner-sealed", "password", "endpoint_id"] {
        assert!(!text.contains(secret), "GET leaked {secret}: {text}");
    }
}

// --- Lifecycle: reveal, reset, delete ----------------------------------------

/// A ready database with sealed credentials (`s3cret`) under `app.key`.
fn ready_database(tenant: TenantId, key: &SecretKey) -> Db {
    let mut db = sample_database(tenant);
    db.name = "web1".into();
    db.database_name = Some("web1".into());
    db.owner_role = Some("postgres".into());
    db.main.owner_password_enc =
        Some(seal_credential(key, &db.main.id.to_string(), Purpose::Owner, "s3cret"));
    db
}

async fn call(app: &TestApp, method: reqwest::Method, path: &str, org: &str) -> reqwest::Response {
    reqwest::Client::new()
        .request(method, format!("{}{path}", app.base))
        .header(IDENTITY_HEADER, app.token_for_org(org))
        .send()
        .await
        .unwrap()
}

/// App with org-1 bound to a tenant; `setup` gets the tenant and key to insert rows.
async fn lifecycle_app(jobs: FakeJobs, setup: impl FnOnce(&FakeMetadata, TenantId, &SecretKey)) -> TestApp {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let app = spawn_app(metadata.clone(), jobs).await;
    setup(&metadata, tenant, &app.key);
    app
}

#[tokio::test]
async fn reveal_ready_database_returns_decrypted_owner_credentials() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}/connection", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["user"], "postgres");
    assert_eq!(body["password"], "s3cret");
    assert_eq!(body["database"], "web1");
    assert_eq!(body["host"], "ep-0123456789ab.localtest.me");
    assert_eq!(body["port"], 4432);
    assert_eq!(
        body["uri"],
        "postgresql://postgres:s3cret@ep-0123456789ab.localtest.me:4432/web1?sslmode=verify-full"
    );
    assert_eq!(body["http_url"], "https://ep-0123456789ab.localtest.me:4444/sql");
}

async fn reveal_in_mode(mode: Mode) -> serde_json::Value {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let app = spawn_app_in_mode(metadata.clone(), FakeJobs::default(), "http://127.0.0.1:9", mode).await;
    let db = ready_database(tenant, &app.key);
    let id = db.database_id;
    metadata.insert_database(db);
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}/connection", id.as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 200);
    resp.json().await.unwrap()
}

// C5: self-hosted mode names the dev CA's file; the URI is unchanged.
#[tokio::test]
async fn reveal_carries_the_ca_file_in_self_hosted_mode() {
    let body = reveal_in_mode(Mode::SelfHosted).await;
    assert_eq!(body["ca_file"], ".chelabase/neon/proxy-ca.crt");
    assert!(body["uri"].as_str().unwrap().ends_with("?sslmode=verify-full"));
}

#[tokio::test]
async fn reveal_has_no_ca_file_in_platform_mode() {
    let body = reveal_in_mode(Mode::Platform).await;
    assert!(body["ca_file"].is_null(), "{body}");
    assert!(body["uri"].as_str().unwrap().ends_with("?sslmode=verify-full"));
}

// Review fix: a replaced secrets key gets an explicit message, not a bare 500.
#[tokio::test]
async fn reveal_with_a_different_secrets_key_explains_the_key_mismatch() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, _k| {
        let db = ready_database(t, &SecretKey::generate());
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}/connection", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 500);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(code_of(&body), "credentials_unavailable");
}

#[tokio::test]
async fn reveal_without_an_endpoint_is_409() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.main.endpoint_id = None;
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}/connection", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(code_of(&body), "credentials_unavailable");
}

// /v1/me and GET show the proxy host and port when an endpoint is linked, nothing otherwise.
#[tokio::test]
async fn summaries_show_the_proxy_host() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let overview = overview_with_database(tenant);
    let mut bare = sample_database(tenant);
    bare.main.endpoint_id = None;
    bare.project_id = overview.projects[0].id;
    let (linked_id, bare_id) = (overview.projects[0].databases[0].database_id, bare.database_id);
    metadata.insert_database(overview.projects[0].databases[0].clone());
    metadata.insert_database(bare.clone());
    let mut overview = overview;
    overview.projects[0].databases.push(bare.record);
    metadata.set_overview(overview);
    let app = spawn_app(metadata, FakeJobs::default()).await;

    let me: serde_json::Value = get_me(&app, Some(&app.token_for_org("org-1"))).await.json().await.unwrap();
    let dbs = &me["tenant"]["projects"][0]["databases"];
    assert_eq!(dbs[0]["host"], "ep-0123456789ab.localtest.me");
    assert_eq!(dbs[0]["port"], 4432);
    assert!(dbs[1]["host"].is_null() && dbs[1]["port"].is_null());

    for (id, host, port) in [(linked_id, serde_json::json!("ep-0123456789ab.localtest.me"), serde_json::json!(4432)), (bare_id, serde_json::Value::Null, serde_json::Value::Null)] {
        let body: serde_json::Value = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}", id.as_uuid()), "org-1").await.json().await.unwrap();
        assert_eq!(body["host"], host);
        assert_eq!(body["port"], port);
    }
}

// A linked endpoint isn't reachable until the database is ready: no host or port before.
#[tokio::test]
async fn summaries_hide_the_proxy_host_until_ready() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let mut overview = overview_with_database(tenant);
    let db = &mut overview.projects[0].databases[0];
    db.status = DatabaseStatus::Provisioning;
    assert!(main_for(db).endpoint_id.is_some());
    let id = db.database_id;
    metadata.insert_database(db.clone());
    metadata.set_overview(overview);
    let app = spawn_app(metadata, FakeJobs::default()).await;

    let me: serde_json::Value = get_me(&app, Some(&app.token_for_org("org-1"))).await.json().await.unwrap();
    let summary = &me["tenant"]["projects"][0]["databases"][0];
    assert_eq!(summary["status"], "provisioning");
    assert!(summary["host"].is_null() && summary["port"].is_null(), "{summary}");

    let body: serde_json::Value = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}", id.as_uuid()), "org-1").await.json().await.unwrap();
    assert_eq!(body["status"], "provisioning");
    assert!(body["host"].is_null() && body["port"].is_null(), "{body}");
}

// Main's branch row holds the database's endpoint and sealed password now: reveal reads both
// from it, and the password opens with main's id (the database's) as AAD.
#[tokio::test]
async fn reveal_uses_main_branch_row() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.main.endpoint_id = Some("ep-aaaaaaaaaaaa".into());
        db.main.owner_password_enc = Some(seal_credential(
            k,
            &BranchId::from(db.database_id).to_string(),
            Purpose::Owner,
            "from-main",
        ));
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let id = id.unwrap().as_uuid();
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{id}/connection"), "org-1").await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["password"], "from-main");
    assert_eq!(body["host"], "ep-aaaaaaaaaaaa.localtest.me");
    assert_eq!(
        body["uri"],
        "postgresql://postgres:from-main@ep-aaaaaaaaaaaa.localtest.me:4432/web1?sslmode=verify-full"
    );
    // GET shows main's host too.
    let body: serde_json::Value = call(&app, reqwest::Method::GET, &format!("/v1/databases/{id}"), "org-1").await.json().await.unwrap();
    assert_eq!(body["host"], "ep-aaaaaaaaaaaa.localtest.me");
}

// No main branch row at all: nothing to reveal or reset.
#[tokio::test]
async fn reveal_and_reset_without_a_main_branch_are_409() {
    let jobs = FakeJobs::default();
    let mut id = None;
    let app = lifecycle_app(jobs.clone(), |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
        m.branches.lock().unwrap().clear();
    })
    .await;
    let id = id.unwrap().as_uuid();
    for (method, path) in [
        (reqwest::Method::GET, format!("/v1/databases/{id}/connection")),
        (reqwest::Method::POST, format!("/v1/databases/{id}/reset-password")),
    ] {
        let resp = call(&app, method, &path, "org-1").await;
        assert_eq!(resp.status(), 409, "{path}");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(code_of(&body), "credentials_unavailable");
    }
    assert!(jobs.resets.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reveal_not_ready_is_409() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.status = DatabaseStatus::Provisioning;
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}/connection", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 409);
}

// Review Focus 2.
#[tokio::test]
async fn reveal_without_a_sealed_owner_password_says_set_in_sql() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.main.owner_password_enc = None;
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}/connection", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["password"].is_null(), "{body}");
    assert_eq!(body["password_state"], "set_in_sql");
    assert_eq!(body["uri"], "postgresql://postgres@ep-0123456789ab.localtest.me:4432/web1?sslmode=verify-full");
}

#[tokio::test]
async fn reveal_other_tenant_is_404() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, _t, k| {
        let db = ready_database(TenantId::new(), k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}/connection", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn reveal_deleted_is_404() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.status = DatabaseStatus::Deleted;
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}/connection", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn reset_password_ok_is_204() {
    let jobs = FakeJobs::default();
    let mut id = None;
    let app = lifecycle_app(jobs.clone(), |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 204);
    assert_eq!(*jobs.resets.lock().unwrap(), vec![(id.unwrap(), BranchId::from(id.unwrap()))]);
}

#[tokio::test]
async fn reset_password_not_ready_is_409_without_asking_the_worker() {
    let jobs = FakeJobs::default();
    let mut id = None;
    let app = lifecycle_app(jobs.clone(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.status = DatabaseStatus::Pending;
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 409);
    assert!(jobs.resets.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reset_password_worker_says_not_ready_is_409() {
    let jobs = FakeJobs::default();
    jobs.reset_answers(FakeReset::NotReady);
    let mut id = None;
    let app = lifecycle_app(jobs, |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 409);
}

// Review Focus 5.
#[tokio::test]
async fn reset_password_worker_timeout_is_504() {
    let jobs = FakeJobs::default();
    jobs.reset_answers(FakeReset::Timeout);
    let mut id = None;
    let app = lifecycle_app(jobs, |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 504);
}

#[tokio::test]
async fn reset_password_worker_deadline_is_504() {
    let jobs = FakeJobs::default();
    jobs.reset_answers(FakeReset::WorkerTimedOut);
    let mut id = None;
    let app = lifecycle_app(jobs, |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 504);
}

#[tokio::test]
async fn reset_password_with_no_worker_is_503() {
    let jobs = FakeJobs::default();
    jobs.reset_answers(FakeReset::Unavailable);
    let mut id = None;
    let app = lifecycle_app(jobs, |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 503);
}

// Databases with no endpoint (never provisioned on Neon) can't be reset.
#[tokio::test]
async fn reset_without_an_endpoint_is_409() {
    let jobs = FakeJobs::default();
    let mut id = None;
    let app = lifecycle_app(jobs.clone(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.main.endpoint_id = None;
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(code_of(&body), "credentials_unavailable");
    assert!(jobs.resets.lock().unwrap().is_empty(), "worker must not be asked");
}

#[tokio::test]
async fn reset_not_applied_is_503_with_the_message() {
    let jobs = FakeJobs::default();
    jobs.reset_answers(FakeReset::NotApplied);
    let mut id = None;
    let app = lifecycle_app(jobs, |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 503);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(code_of(&body), "not_applied");
    assert_eq!(message_of(&body), RESET_NOT_APPLIED);
}

#[tokio::test]
async fn delete_ready_database_is_202_and_publishes_job() {
    let jobs = FakeJobs::default();
    let mut id = None;
    let app = lifecycle_app(jobs.clone(), |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::DELETE, &format!("/v1/databases/{}", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 202);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "deleting");
    assert_eq!(*jobs.deleted.lock().unwrap(), vec![id.unwrap()]);
}

#[tokio::test]
async fn delete_twice_is_409() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let path = format!("/v1/databases/{}", id.unwrap().as_uuid());
    assert_eq!(call(&app, reqwest::Method::DELETE, &path, "org-1").await.status(), 202);
    assert_eq!(call(&app, reqwest::Method::DELETE, &path, "org-1").await.status(), 409);
}

#[tokio::test]
async fn delete_deleted_is_404() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.status = DatabaseStatus::Deleted;
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::DELETE, &format!("/v1/databases/{}", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn delete_other_tenant_is_404() {
    let jobs = FakeJobs::default();
    let mut id = None;
    let app = lifecycle_app(jobs.clone(), |m, _t, k| {
        let db = ready_database(TenantId::new(), k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::DELETE, &format!("/v1/databases/{}", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 404);
    assert!(jobs.deleted.lock().unwrap().is_empty());
}

/// Every endpoint `chelabase-routes` assigns to the orchestrator must be routed
/// by the orchestrator's own router. A 401 still proves the route exists; only
/// 404 (unknown path) and 405 (wrong method) mean drift.
#[tokio::test]
async fn every_declared_orchestrator_endpoint_is_routed() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let client = reqwest::Client::new();
    let declared: Vec<_> = ENDPOINTS
        .iter()
        .filter(|ep| ep.service == Service::Orchestrator)
        .collect();
    assert!(!declared.is_empty());

    for ep in declared {
        let method = reqwest::Method::from_bytes(ep.method.as_str().as_bytes()).unwrap();
        let url = format!("{}{}", app.base, fill_params(ep.path));
        let status = client.request(method, &url).send().await.unwrap().status();
        assert!(
            status != 404 && status != 405,
            "{} {} is declared in chelabase-routes but the orchestrator answered {status}",
            ep.method.as_str(),
            ep.path
        );
    }
}

fn fill_params(template: &str) -> String {
    template
        .split('/')
        .map(|segment| {
            if segment.starts_with('{') {
                uuid::Uuid::nil().to_string()
            } else {
                segment.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

// --- Branches ------------------------------------------------------------------


struct Branching {
    app: TestApp,
    metadata: FakeMetadata,
    jobs: FakeJobs,
    db: DatabaseId,
}

impl Branching {
    fn main(&self) -> BranchId {
        BranchId::from(self.db)
    }

    fn add(&self, name: &str, parent: BranchId, kind: BranchKind, status: BranchStatus) -> BranchId {
        self.metadata.add_branch(self.db, name, parent, kind, status)
    }

    fn ready(&self, name: &str) -> BranchId {
        self.add(name, self.main(), BranchKind::Normal, BranchStatus::Ready)
    }

    fn backup(&self, of: BranchId) -> BranchId {
        self.add("dev-before-restore-20260101T000000Z", of, BranchKind::Backup, BranchStatus::Ready)
    }

    fn branches(&self) -> String {
        format!("/v1/databases/{}/branches", self.db.as_uuid())
    }

    fn branch(&self, id: BranchId) -> String {
        format!("{}/{}", self.branches(), id.as_uuid())
    }

    async fn send(&self, method: Method, path: &str, body: Option<Value>) -> reqwest::Response {
        let mut req = reqwest::Client::new()
            .request(method, format!("{}{path}", self.app.base))
            .header(IDENTITY_HEADER, self.app.token_for_org("org-1"));
        if let Some(body) = body {
            req = req.json(&body);
        }
        req.send().await.unwrap()
    }

    fn published(&self) -> Vec<BranchJob> {
        self.jobs.branch_jobs.lock().unwrap().iter().map(|j| j.payload.clone()).collect()
    }
}

/// A ready database `web1` for org-1 (max 5 branches, 7 day window) with sealed password `s3cret`.
async fn branching() -> Branching {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app(metadata.clone(), jobs.clone()).await;
    let db = ready_database(tenant, &app.key);
    let id = db.database_id;
    metadata.insert_database(db);
    Branching { app, metadata, jobs, db: id }
}

/// The error body of a response with the expected status.
async fn error_of(resp: reqwest::Response, status: u16) -> Value {
    assert_eq!(resp.status(), status);
    resp.json().await.unwrap()
}

fn code_of(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_else(|| panic!("no error code in {body}"))
}

fn message_of(body: &Value) -> &str {
    body["error"]["message"].as_str().unwrap_or_else(|| panic!("no error message in {body}"))
}

// ---- list

#[tokio::test]
async fn list_branches_returns_the_tree_with_hosts_only_for_ready_normal_branches() {
    let t = branching().await;
    let dev = t.ready("dev");
    let pending = t.add("wip", dev, BranchKind::Normal, BranchStatus::Pending);
    let backup = t.backup(dev);
    let resp = t.send(Method::GET, &t.branches(), None).await;
    assert_eq!(resp.status(), 200);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(list.len(), 4);
    let by = |id: BranchId| list.iter().find(|b| b["id"] == json!(id)).unwrap_or_else(|| panic!("{id} missing"));
    let main = by(t.main());
    assert_eq!(main["name"], "main");
    assert!(main["parent_branch_id"].is_null());
    assert_eq!(main["kind"], "normal");
    assert_eq!(main["status"], "ready");
    assert_eq!(main["host"], "ep-0123456789ab.localtest.me");
    assert_eq!(main["port"], 4432);
    let dev_json = by(dev);
    assert_eq!(dev_json["parent_branch_id"], json!(t.main()));
    assert!(dev_json["host"].as_str().unwrap().ends_with(".localtest.me"));
    assert_eq!(dev_json["port"], 4432);
    for id in [pending, backup] {
        assert!(by(id)["host"].is_null() && by(id)["port"].is_null(), "{}", by(id));
    }
    assert_eq!(by(backup)["kind"], "backup");
    assert_eq!(by(pending)["status"], "pending");
    for key in ["id", "name", "parent_branch_id", "kind", "status", "host", "port", "branched_at_time", "branched_at_lsn", "created_at", "error"] {
        assert!(main.get(key).is_some(), "missing {key}");
    }
    let text = serde_json::to_string(&list).unwrap();
    for secret in ["secret-owner-sealed", "owner_password", "endpoint_id", "timeline"] {
        assert!(!text.contains(secret), "leaked {secret}");
    }
}

#[tokio::test]
async fn list_branches_carries_where_each_history_begins() {
    let t = branching().await;
    let dev = t.ready("dev");
    let backup = t.backup(dev);
    let start: chrono::DateTime<chrono::Utc> = "2026-09-20T09:00:00Z".parse().unwrap();
    for id in [dev, backup] {
        t.metadata.branches.lock().unwrap().get_mut(&id).unwrap().history_start = Some(start);
    }
    let resp = t.send(Method::GET, &t.branches(), None).await;
    assert_eq!(resp.status(), 200);
    let list: Vec<Value> = resp.json().await.unwrap();
    let by = |id: BranchId| list.iter().find(|b| b["id"] == json!(id)).unwrap();
    assert_eq!(by(dev)["history_start"], "2026-09-20T09:00:00Z");
    assert_eq!(by(backup)["history_start"], "2026-09-20T09:00:00Z");
    // Unknown (a row from before it was recorded): null, never missing.
    let main = by(t.main());
    assert!(main.get("history_start").is_some_and(Value::is_null), "{main}");
    assert!(!serde_json::to_string(&list).unwrap().contains("pending_timeline_at"));
}

#[tokio::test]
async fn list_branches_hides_deleted_and_refuses_strangers() {
    let t = branching().await;
    t.add("gone", t.main(), BranchKind::Normal, BranchStatus::Deleted);
    let list: Vec<Value> = t.send(Method::GET, &t.branches(), None).await.json().await.unwrap();
    assert_eq!(list.len(), 1);
    let unknown = format!("/v1/databases/{}/branches", uuid::Uuid::new_v4());
    assert_eq!(t.send(Method::GET, &unknown, None).await.status(), 404);

    let mut other_id = None;
    let other = lifecycle_app(FakeJobs::default(), |m, _t, k| {
        let db = ready_database(TenantId::new(), k);
        other_id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let path = format!("/v1/databases/{}/branches", other_id.unwrap().as_uuid());
    assert_eq!(call(&other, Method::GET, &path, "org-1").await.status(), 404);
}


// ---- create

#[tokio::test]
async fn create_branch_is_202_with_the_pending_branch_and_a_job() {
    let t = branching().await;
    let resp = t.send(Method::POST, &t.branches(), Some(json!({ "name": "dev" }))).await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["name"], "dev");
    assert_eq!(body["status"], "pending");
    assert_eq!(body["kind"], "normal");
    assert_eq!(body["parent_branch_id"], json!(t.main()));
    assert!(body["host"].is_null());
    let id = BranchId(body["id"].as_str().unwrap().parse().unwrap());
    assert_eq!(t.metadata.branch(id).status, BranchStatus::Pending);
    assert_eq!(t.published(), vec![BranchJob::Create { branch_id: id, at: None }]);
}

#[tokio::test]
async fn create_branch_takes_a_parent_and_a_point() {
    let t = branching().await;
    let dev = t.ready("dev");
    let resp = t
        .send(Method::POST, &t.branches(), Some(json!({ "name": "feat", "parent_branch_id": dev, "at": { "lsn": "0/16B3748" } })))
        .await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    let id = BranchId(body["id"].as_str().unwrap().parse().unwrap());
    assert_eq!(body["parent_branch_id"], json!(dev));
    assert_eq!(body["branched_at_lsn"], "0/16B3748");
    assert_eq!(t.published(), vec![BranchJob::Create { branch_id: id, at: Some(PointInTime::Lsn("0/16B3748".into())) }]);

    let resp = t
        .send(Method::POST, &t.branches(), Some(json!({ "name": "old", "at": { "timestamp": "2026-09-30T12:00:00Z" } })))
        .await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    let at: chrono::DateTime<chrono::Utc> = "2026-09-30T12:00:00Z".parse().unwrap();
    assert_eq!(t.metadata.branch(BranchId(body["id"].as_str().unwrap().parse().unwrap())).branched_at_time, Some(at));
    assert!(matches!(t.published().last(), Some(BranchJob::Create { at: Some(PointInTime::Timestamp(x)), .. }) if *x == at));
}

#[tokio::test]
async fn create_branch_from_a_restore_backup_is_202() {
    let t = branching().await;
    let dev = t.ready("dev");
    let backup = t.backup(dev);
    let resp = t
        .send(Method::POST, &t.branches(), Some(json!({ "name": "from-backup", "parent_branch_id": backup })))
        .await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    let id = BranchId(body["id"].as_str().unwrap().parse().unwrap());
    assert_eq!(body["parent_branch_id"], json!(backup));
    assert_eq!(body["kind"], "normal");
    assert_eq!(t.metadata.branch(id).parent_branch_id, Some(backup));
    assert_eq!(t.published(), vec![BranchJob::Create { branch_id: id, at: None }]);
}

#[tokio::test]
async fn create_branch_with_a_taken_name_is_409() {
    let t = branching().await;
    t.ready("dev");
    let err = error_of(t.send(Method::POST, &t.branches(), Some(json!({ "name": "dev" }))).await, 409).await;
    assert_eq!(code_of(&err), "name_taken");
    let err = error_of(t.send(Method::POST, &t.branches(), Some(json!({ "name": "main" }))).await, 422).await;
    assert_eq!(code_of(&err), "invalid_request");
    assert!(t.published().is_empty());
}

#[tokio::test]
async fn create_branch_at_the_cap_is_409_naming_the_cap_and_the_patch() {
    let t = branching().await;
    t.metadata.databases.lock().unwrap().get_mut(&t.db).unwrap().max_branches = 1;
    t.ready("dev");
    let err = error_of(t.send(Method::POST, &t.branches(), Some(json!({ "name": "b" }))).await, 409).await;
    assert_eq!(code_of(&err), "branch_cap_reached");
    assert!(message_of(&err).contains("max_branches"), "{err}");
    assert!(t.published().is_empty());
}

#[tokio::test]
async fn create_branch_needs_a_ready_parent_that_exists() {
    let t = branching().await;
    let pending = t.add("p", t.main(), BranchKind::Normal, BranchStatus::Pending);
    let err = error_of(t.send(Method::POST, &t.branches(), Some(json!({ "name": "x", "parent_branch_id": pending }))).await, 409).await;
    assert_eq!(code_of(&err), "branch_not_ready");
    let unknown = BranchId::new();
    assert_eq!(t.send(Method::POST, &t.branches(), Some(json!({ "name": "x", "parent_branch_id": unknown }))).await.status(), 404);
    let deleted = t.add("d", t.main(), BranchKind::Normal, BranchStatus::Deleted);
    assert_eq!(t.send(Method::POST, &t.branches(), Some(json!({ "name": "x", "parent_branch_id": deleted }))).await.status(), 404);
    assert!(t.published().is_empty());
}

#[tokio::test]
async fn create_branch_on_a_database_that_is_not_ready_is_409() {
    let t = branching().await;
    t.metadata.branches.lock().unwrap().get_mut(&t.main()).unwrap().status = BranchStatus::Provisioning;
    let err = error_of(t.send(Method::POST, &t.branches(), Some(json!({ "name": "x" }))).await, 409).await;
    assert_eq!(code_of(&err), "branch_not_ready");
}

#[tokio::test]
async fn create_branch_rejects_bad_input_with_422() {
    let t = branching().await;
    let long = "a".repeat(64);
    for name in ["", "Dev", "-x", "_x", "a b", "a.b", long.as_str(), "main"] {
        let resp = t.send(Method::POST, &t.branches(), Some(json!({ "name": name }))).await;
        assert_eq!(resp.status(), 422, "name {name:?}");
    }
    for at in [
        json!({ "lsn": "zzz" }),
        json!({ "lsn": "0/" }),
        json!({ "lsn": "1234" }),
        json!({ "lsn": "" }),
        json!({ "timestamp": "yesterday" }),
        json!({ "timestamp": "2026-09-30" }),
        json!({}),
        json!({ "lsn": "0/1", "timestamp": "2026-09-30T12:00:00Z" }),
    ] {
        let resp = t.send(Method::POST, &t.branches(), Some(json!({ "name": "ok", "at": at }))).await;
        assert_eq!(resp.status(), 422, "at {at}");
    }
    // Longest allowed name and the character set.
    for name in ["a".repeat(63), "0-a_b".to_string()] {
        assert_eq!(t.send(Method::POST, &t.branches(), Some(json!({ "name": name }))).await.status(), 202, "{name}");
    }
    assert_eq!(t.published().len(), 2);
}

#[tokio::test]
async fn create_branch_publish_failure_is_503_and_fails_the_branch() {
    let t = branching().await;
    *t.jobs.fail_branch_publish.lock().unwrap() = true;
    assert_eq!(t.send(Method::POST, &t.branches(), Some(json!({ "name": "dev" }))).await.status(), 503);
    let branches = t.metadata.branches.lock().unwrap();
    let dev = branches.values().find(|b| b.name == "dev").unwrap();
    assert_eq!(dev.status, BranchStatus::Failed);
    assert!(dev.error.as_deref().unwrap().contains("publish"));
}

#[tokio::test]
async fn create_branch_on_another_tenants_database_is_404_like_an_unknown_one() {
    let jobs = FakeJobs::default();
    let mut id = None;
    let app = lifecycle_app(jobs.clone(), |m, _t, k| {
        let db = ready_database(TenantId::new(), k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let path = format!("/v1/databases/{}/branches", id.unwrap().as_uuid());
    let resp = reqwest::Client::new()
        .post(format!("{}{path}", app.base))
        .header(IDENTITY_HEADER, app.token_for_org("org-1"))
        .json(&json!({ "name": "dev" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/databases/{}/branches", app.base, uuid::Uuid::new_v4()))
        .header(IDENTITY_HEADER, app.token_for_org("org-1"))
        .json(&json!({ "name": "dev" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert!(jobs.branch_jobs.lock().unwrap().is_empty());
}

// ---- connection and reset-password

#[tokio::test]
async fn branch_connection_reveals_the_branchs_own_password() {
    let t = branching().await;
    let dev = t.ready("dev");
    let sealed = seal_credential(&t.app.key, &dev.to_string(), Purpose::Owner, "dev-pw");
    t.metadata.branches.lock().unwrap().get_mut(&dev).unwrap().owner_password_enc = Some(sealed);
    let ep = t.metadata.branch(dev).endpoint_id.unwrap();
    let resp = t.send(Method::GET, &format!("{}/connection", t.branch(dev)), None).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["password"], "dev-pw");
    assert_eq!(body["host"], format!("{ep}.localtest.me"));
    assert_eq!(body["port"], 4432);
    assert_eq!(body["database"], "web1");
    assert_eq!(body["user"], "postgres");
    assert_eq!(body["uri"], format!("postgresql://postgres:dev-pw@{ep}.localtest.me:4432/web1?sslmode=verify-full"));
    assert_eq!(body["http_url"], format!("https://{ep}.localtest.me:4444/sql"));
}

#[tokio::test]
async fn branch_connection_and_reset_refuse_backups_unready_and_unknown_branches() {
    let t = branching().await;
    let dev = t.ready("dev");
    let backup = t.backup(dev);
    let pending = t.add("p", t.main(), BranchKind::Normal, BranchStatus::Pending);
    let resetting = t.add("r", t.main(), BranchKind::Normal, BranchStatus::Resetting);
    let gone = t.add("g", t.main(), BranchKind::Normal, BranchStatus::Deleted);
    for (method, suffix) in [(Method::GET, "connection"), (Method::POST, "reset-password")] {
        let err = error_of(t.send(method.clone(), &format!("{}/{suffix}", t.branch(backup)), None).await, 409).await;
        assert_eq!(code_of(&err), "backup_branch");
        for id in [pending, resetting] {
            let err = error_of(t.send(method.clone(), &format!("{}/{suffix}", t.branch(id)), None).await, 409).await;
            assert_eq!(code_of(&err), "branch_not_ready");
        }
        for id in [BranchId::new(), gone] {
            assert_eq!(t.send(method.clone(), &format!("{}/{suffix}", t.branch(id)), None).await.status(), 404);
        }
    }
    assert!(t.jobs.resets.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_branch_of_another_database_is_404_under_this_one() {
    let t = branching().await;
    let other = DatabaseId::new();
    let foreign = t.metadata.add_branch(other, "x", BranchId::from(other), BranchKind::Normal, BranchStatus::Ready);
    for (method, suffix) in [
        (Method::GET, "/connection"),
        (Method::POST, "/reset-password"),
        (Method::POST, "/reset-to-parent"),
        (Method::POST, "/restore"),
        (Method::DELETE, ""),
    ] {
        let resp = t.send(method, &format!("{}{suffix}", t.branch(foreign)), Some(json!({ "at": { "lsn": "0/1" } }))).await;
        assert_eq!(resp.status(), 404, "{suffix}");
    }
}

#[tokio::test]
async fn branch_reset_password_asks_the_worker_for_that_branch() {
    let t = branching().await;
    let dev = t.ready("dev");
    let resp = t.send(Method::POST, &format!("{}/reset-password", t.branch(dev)), None).await;
    assert_eq!(resp.status(), 204);
    assert_eq!(*t.jobs.resets.lock().unwrap(), vec![(t.db, dev)]);
    t.jobs.reset_answers(FakeReset::NotApplied);
    let resp = t.send(Method::POST, &format!("{}/reset-password", t.branch(dev)), None).await;
    let body = error_of(resp, 503).await;
    assert_eq!(code_of(&body), "not_applied");
    assert_eq!(message_of(&body), RESET_NOT_APPLIED);
    t.jobs.reset_answers(FakeReset::Timeout);
    assert_eq!(t.send(Method::POST, &format!("{}/reset-password", t.branch(dev)), None).await.status(), 504);
    t.jobs.reset_answers(FakeReset::Unavailable);
    assert_eq!(t.send(Method::POST, &format!("{}/reset-password", t.branch(dev)), None).await.status(), 503);
}

#[tokio::test]
async fn old_routes_are_aliases_for_main() {
    let t = branching().await;
    let main = t.main();
    let old: Value = t.send(Method::GET, &format!("/v1/databases/{}/connection", t.db.as_uuid()), None).await.json().await.unwrap();
    let new: Value = t.send(Method::GET, &format!("{}/connection", t.branch(main)), None).await.json().await.unwrap();
    assert_eq!(old, new);
    assert_eq!(old["password"], "s3cret");
    assert_eq!(t.send(Method::POST, &format!("/v1/databases/{}/reset-password", t.db.as_uuid()), None).await.status(), 204);
    assert_eq!(t.send(Method::POST, &format!("{}/reset-password", t.branch(main)), None).await.status(), 204);
    assert_eq!(*t.jobs.resets.lock().unwrap(), vec![(t.db, main), (t.db, main)]);
}

// ---- reset to parent

#[tokio::test]
async fn reset_to_parent_is_202_and_publishes_the_job() {
    let t = branching().await;
    let dev = t.ready("dev");
    let resp = t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "resetting");
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Resetting);
    assert_eq!(t.published(), vec![BranchJob::ResetToParent { branch_id: dev }]);
    // Twice: no longer ready.
    let err = error_of(t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await, 409).await;
    assert_eq!(code_of(&err), "branch_not_ready");
    assert_eq!(t.published().len(), 1);
}

#[tokio::test]
async fn reset_and_restore_claims_clear_the_last_error() {
    let t = branching().await;
    let dev = t.ready("dev");
    let set_error = |id| t.metadata.branches.lock().unwrap().get_mut(&id).unwrap().error = Some("an old failure".into());
    set_error(dev);
    assert_eq!(t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await.status(), 202);
    assert_eq!(t.metadata.branch(dev).error, None, "each attempt starts clean");

    let qa = t.ready("qa");
    set_error(qa);
    let resp = t
        .send(Method::POST, &format!("{}/restore", t.branch(qa)), Some(json!({ "at": { "lsn": "0/16B3748" } })))
        .await;
    assert_eq!(resp.status(), 202);
    assert_eq!(t.metadata.branch(qa).error, None);
}

#[tokio::test]
async fn reset_to_parent_refusals() {
    let t = branching().await;
    let dev = t.ready("dev");
    let child = t.add("child", dev, BranchKind::Normal, BranchStatus::Ready);
    let other_child = t.add("child2", dev, BranchKind::Normal, BranchStatus::Pending);
    let backup = t.backup(child);
    let path = |id| format!("{}/reset-to-parent", t.branch(id));

    let err = error_of(t.send(Method::POST, &path(t.main()), None).await, 409).await;
    assert_eq!(code_of(&err), "main_branch");
    let err = error_of(t.send(Method::POST, &path(backup), None).await, 409).await;
    assert_eq!(code_of(&err), "backup_branch");
    let err = error_of(t.send(Method::POST, &path(other_child), None).await, 409).await;
    assert_eq!(code_of(&err), "branch_not_ready");
    let err = error_of(t.send(Method::POST, &path(dev), None).await, 409).await;
    assert_eq!(code_of(&err), "has_children");
    assert!(message_of(&err).contains("child") && message_of(&err).contains("child2"), "{err}");
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Ready, "a refused reset leaves the branch ready");
    assert_eq!(t.send(Method::POST, &path(BranchId::new()), None).await.status(), 404);
    assert!(t.published().is_empty());
}

#[tokio::test]
async fn backups_do_not_block_reset_to_parent() {
    let t = branching().await;
    let dev = t.ready("dev");
    t.backup(dev);
    assert_eq!(t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await.status(), 202);
}

#[tokio::test]
async fn reset_to_parent_publish_failure_is_503_and_the_branch_stays_ready() {
    let t = branching().await;
    let dev = t.ready("dev");
    *t.jobs.fail_branch_publish.lock().unwrap() = true;
    assert_eq!(t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await.status(), 503);
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Ready);
}

// ---- restore

#[tokio::test]
async fn restore_returns_the_backup_name() {
    let t = branching().await;
    let dev = t.ready("dev");
    let resp = t
        .send(Method::POST, &format!("{}/restore", t.branch(dev)), Some(json!({ "at": { "lsn": "0/16B3748" } })))
        .await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    let name = body["backup_name"].as_str().unwrap();
    let (prefix, stamp) = name.split_once("-before-restore-").expect(name);
    assert_eq!(prefix, "dev");
    assert!(stamp.len() == 16 && stamp.ends_with('Z') && stamp.as_bytes()[8] == b'T', "{stamp}");
    assert!(stamp[..8].bytes().all(|b| b.is_ascii_digit()) && stamp[9..15].bytes().all(|b| b.is_ascii_digit()), "{stamp}");
    let backup_id = BranchId(body["backup_branch_id"].as_str().unwrap().parse().unwrap());
    assert_eq!(body["status"], "resetting");
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Resetting);
    assert_eq!(
        t.published(),
        vec![BranchJob::Restore {
            branch_id: dev,
            backup_id,
            backup_name: name.to_string(),
            at: PointInTime::Lsn("0/16B3748".into()),
        }]
    );
}

#[tokio::test]
async fn restore_truncates_a_long_name_to_fit_63_characters() {
    let t = branching().await;
    let long = format!("a{}", "b".repeat(62));
    assert_eq!(long.len(), 63);
    let dev = t.ready(&long);
    let resp = t
        .send(Method::POST, &format!("{}/restore", t.branch(dev)), Some(json!({ "at": { "timestamp": "2026-09-30T12:00:00Z" } })))
        .await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    let name = body["backup_name"].as_str().unwrap();
    assert_eq!(name.len(), 63, "{name}");
    assert!(name.starts_with("abbb") && name.contains("-before-restore-2"), "{name}");
}

#[tokio::test]
async fn restore_works_on_main() {
    let t = branching().await;
    let resp = t
        .send(Method::POST, &format!("{}/restore", t.branch(t.main())), Some(json!({ "at": { "lsn": "0/1" } })))
        .await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert!(body["backup_name"].as_str().unwrap().starts_with("main-before-restore-"));
}

// T14 M8: a branch's history begins at its branch point (and, by time, at its
// `history_start`): a restore to before it would bring back the parent's older state, so it's
// refused at once, before anything changes. At the start itself it's accepted.
#[tokio::test]
async fn restore_before_the_branch_history_begins_is_refused() {
    let t = branching().await;
    let dev = t.ready("dev");
    let start: chrono::DateTime<chrono::Utc> = "2026-09-30T12:00:00Z".parse().unwrap();
    {
        let mut branches = t.metadata.branches.lock().unwrap();
        let b = branches.get_mut(&dev).unwrap();
        b.branched_at_lsn = Some("0/2000000".into());
        b.history_start = Some(start);
    }
    let path = format!("{}/restore", t.branch(dev));
    for (at, message) in [
        (
            json!({ "lsn": "0/1FFFFFF" }),
            "the LSN 0/1FFFFFF is before dev's history begins (its branch point, 0/2000000)",
        ),
        (
            json!({ "timestamp": "2026-09-30T11:59:59Z" }),
            "the time 2026-09-30T11:59:59Z is before dev's history begins (2026-09-30T12:00:00Z)",
        ),
    ] {
        let err = error_of(t.send(Method::POST, &path, Some(json!({ "at": at }))).await, 422).await;
        assert_eq!(code_of(&err), "invalid_request", "{at}");
        assert_eq!(message_of(&err), message, "{at}");
    }
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Ready);
    assert!(t.published().is_empty());
    let ok = t.send(Method::POST, &path, Some(json!({ "at": { "lsn": "0/2000000" } }))).await;
    assert_eq!(ok.status(), 202);
}

#[tokio::test]
async fn restore_refusals() {
    let t = branching().await;
    let dev = t.ready("dev");
    let child = t.add("child", dev, BranchKind::Normal, BranchStatus::Ready);
    let backup = t.backup(child);
    let pending = t.add("p", t.main(), BranchKind::Normal, BranchStatus::Provisioning);
    let path = |id| format!("{}/restore", t.branch(id));
    let at = || Some(json!({ "at": { "lsn": "0/1" } }));

    let err = error_of(t.send(Method::POST, &path(backup), at()).await, 409).await;
    assert_eq!(code_of(&err), "backup_branch");
    let err = error_of(t.send(Method::POST, &path(pending), at()).await, 409).await;
    assert_eq!(code_of(&err), "branch_not_ready");
    let err = error_of(t.send(Method::POST, &path(dev), at()).await, 409).await;
    assert_eq!(code_of(&err), "has_children");
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Ready);
    assert_eq!(t.send(Method::POST, &path(BranchId::new()), at()).await.status(), 404);
    // Bad bodies before anything changes: an empty one isn't JSON (400), the rest are 422.
    assert_eq!(t.send(Method::POST, &path(child), None).await.status(), 400);
    for body in [
        Some(json!({})),
        Some(json!({ "at": {} })),
        Some(json!({ "at": { "lsn": "nope" } })),
        Some(json!({ "at": { "timestamp": "nope" } })),
    ] {
        assert_eq!(t.send(Method::POST, &path(child), body.clone()).await.status(), 422, "{body:?}");
    }
    assert_eq!(t.metadata.branch(child).status, BranchStatus::Ready);
    assert!(t.published().is_empty());
}

// ---- delete

#[tokio::test]
async fn delete_branch_is_202_and_takes_its_backups_along() {
    let t = branching().await;
    let dev = t.ready("dev");
    let backup = t.backup(dev);
    let resp = t.send(Method::DELETE, &t.branch(dev), None).await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "deleting");
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Deleting);
    assert_eq!(t.metadata.branch(backup).status, BranchStatus::Deleting);
    assert_eq!(t.published(), vec![BranchJob::Delete { branch_id: dev }]);
    // Twice.
    let err = error_of(t.send(Method::DELETE, &t.branch(dev), None).await, 409).await;
    assert_eq!(code_of(&err), "already_deleting");
}

#[tokio::test]
async fn delete_branch_refusals() {
    let t = branching().await;
    let dev = t.ready("dev");
    t.add("kid", dev, BranchKind::Normal, BranchStatus::Provisioning);
    t.add("kid2", dev, BranchKind::Normal, BranchStatus::Failed);
    let err = error_of(t.send(Method::DELETE, &t.branch(t.main()), None).await, 409).await;
    assert_eq!(code_of(&err), "main_branch");
    let err = error_of(t.send(Method::DELETE, &t.branch(dev), None).await, 409).await;
    assert_eq!(code_of(&err), "has_children");
    assert!(message_of(&err).contains("kid") && message_of(&err).contains("kid2"), "{err}");
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Ready, "a refused delete leaves the branch ready");
    let gone = t.add("g", t.main(), BranchKind::Normal, BranchStatus::Deleted);
    assert_eq!(t.send(Method::DELETE, &t.branch(gone), None).await.status(), 404);
    assert_eq!(t.send(Method::DELETE, &t.branch(BranchId::new()), None).await.status(), 404);
    assert!(t.published().is_empty());
}

// Fix round 1, I1: a backup can be a parent. Deleting its branch would take the backup
// along, and Neon can't delete a timeline that others branch from.
#[tokio::test]
async fn delete_branch_refuses_while_a_backup_has_children() {
    let t = branching().await;
    let dev = t.ready("dev");
    let backup = t.backup(dev);
    let backup_name = t.metadata.branch(backup).name;
    t.add("x", backup, BranchKind::Normal, BranchStatus::Ready);
    let err = error_of(t.send(Method::DELETE, &t.branch(dev), None).await, 409).await;
    assert_eq!(code_of(&err), "has_children");
    assert_eq!(
        message_of(&err),
        format!("cannot delete dev: its backup {backup_name} has child branches (x); delete them first")
    );
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Ready, "released");
    assert_eq!(t.metadata.branch(backup).status, BranchStatus::Ready);
    assert!(t.published().is_empty());
}

#[tokio::test]
async fn delete_of_a_backup_or_a_branch_with_only_backups_is_allowed() {
    let t = branching().await;
    let dev = t.ready("dev");
    let backup = t.backup(dev);
    assert_eq!(t.send(Method::DELETE, &t.branch(backup), None).await.status(), 202);
    let other = t.ready("other");
    t.backup(other);
    assert_eq!(t.send(Method::DELETE, &t.branch(other), None).await.status(), 202);
}

#[tokio::test]
async fn delete_of_a_failed_branch_is_allowed() {
    let t = branching().await;
    let bad = t.add("bad", t.main(), BranchKind::Normal, BranchStatus::Failed);
    assert_eq!(t.send(Method::DELETE, &t.branch(bad), None).await.status(), 202);
}

// ---- settings

fn database_path(t: &Branching) -> String {
    format!("/v1/databases/{}", t.db.as_uuid())
}

#[tokio::test]
async fn patch_updates_the_settings_and_returns_them() {
    let t = branching().await;
    let resp = t
        .send(Method::PATCH, &database_path(&t), Some(json!({ "history_window": 86400, "max_branches": 10 })))
        .await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body,
        json!({
            "history_window": 86400, "max_branches": 10,
            "max_rows": 1000, "statement_timeout_ms": 8000, "max_connections": 5,
        })
    );
    let db = t.metadata.database(t.db);
    assert_eq!(db.history_window, std::time::Duration::from_secs(86400));
    assert_eq!(db.max_branches, 10);
    // GET shows the settings too.
    let get: Value = t.send(Method::GET, &database_path(&t), None).await.json().await.unwrap();
    assert_eq!(get["history_window"], 86400);
    assert_eq!(get["max_branches"], 10);
}

#[tokio::test]
async fn patch_publishes_settings_only_when_the_window_changed() {
    let t = branching().await;
    let path = database_path(&t);
    let published = || t.jobs.settings_jobs.lock().unwrap().clone();
    // Only the cap.
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "max_branches": 3 }))).await.status(), 200);
    assert!(published().is_empty());
    // The same window as now (7 days).
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "history_window": 7 * 24 * 3600 }))).await.status(), 200);
    assert!(published().is_empty());
    // Nothing at all.
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({}))).await.status(), 200);
    assert!(published().is_empty());
    // A different window.
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "history_window": 3600 }))).await.status(), 200);
    assert_eq!(published(), vec![t.db]);
}

#[tokio::test]
async fn patch_validates_the_ranges() {
    let t = branching().await;
    let path = database_path(&t);
    for body in [
        json!({ "history_window": 3599 }),
        json!({ "history_window": 30 * 24 * 3600 + 1 }),
        json!({ "history_window": 0 }),
        json!({ "history_window": -5 }),
        json!({ "history_window": "7d" }),
        json!({ "max_branches": -1 }),
        json!({ "max_branches": 21 }),
        json!({ "max_branches": "many" }),
        json!({ "surprise": 1 }),
    ] {
        let resp = t.send(Method::PATCH, &path, Some(body.clone())).await;
        assert_eq!(resp.status(), 422, "{body}");
    }
    assert_eq!(t.metadata.database(t.db).max_branches, 5);
    assert!(t.jobs.settings_jobs.lock().unwrap().is_empty());
    for body in [
        json!({ "history_window": 3600, "max_branches": 0 }),
        json!({ "history_window": 30 * 24 * 3600, "max_branches": 20 }),
    ] {
        assert_eq!(t.send(Method::PATCH, &path, Some(body.clone())).await.status(), 200, "{body}");
    }
}

#[tokio::test]
async fn patch_refusals() {
    let t = branching().await;
    let unknown = format!("/v1/databases/{}", uuid::Uuid::new_v4());
    assert_eq!(t.send(Method::PATCH, &unknown, Some(json!({ "max_branches": 1 }))).await.status(), 404);
    t.metadata.databases.lock().unwrap().get_mut(&t.db).unwrap().status = DatabaseStatus::Deleting;
    let err = error_of(t.send(Method::PATCH, &database_path(&t), Some(json!({ "max_branches": 1 }))).await, 409).await;
    assert_eq!(code_of(&err), "database_not_ready");
    assert!(t.jobs.settings_jobs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn patch_on_another_tenants_database_is_404() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, _t, k| {
        let db = ready_database(TenantId::new(), k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = reqwest::Client::new()
        .patch(format!("{}/v1/databases/{}", app.base, id.unwrap().as_uuid()))
        .header(IDENTITY_HEADER, app.token_for_org("org-1"))
        .json(&json!({ "max_branches": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

// ---- branch_count

#[tokio::test]
async fn me_and_get_database_show_branch_count_with_one_lookup_for_me() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let overview = overview_with_database(tenant);
    let id = overview.projects[0].databases[0].database_id;
    metadata.insert_database(overview.projects[0].databases[0].clone());
    metadata.set_overview(overview);
    let dev = metadata.add_branch(id, "dev", BranchId::from(id), BranchKind::Normal, BranchStatus::Ready);
    metadata.add_branch(id, "backup", dev, BranchKind::Backup, BranchStatus::Ready);
    metadata.add_branch(id, "gone", BranchId::from(id), BranchKind::Normal, BranchStatus::Deleted);
    let app = spawn_app(metadata.clone(), FakeJobs::default()).await;

    let me: Value = get_me(&app, Some(&app.token_for_org("org-1"))).await.json().await.unwrap();
    let db = &me["tenant"]["projects"][0]["databases"][0];
    assert_eq!(db["branch_count"], 2, "main and dev; backups and deleted branches don't count");
    assert_eq!(db["host"], "ep-0123456789ab.localtest.me");
    assert_eq!(metadata.main_count_queries.load(Ordering::SeqCst), 1);

    let get: Value = call(&app, Method::GET, &format!("/v1/databases/{}", id.as_uuid()), "org-1").await.json().await.unwrap();
    assert_eq!(get["branch_count"], 2);
    assert_eq!(get["host"], "ep-0123456789ab.localtest.me");
}

/// Merges name the Postgres database (`MergeSummary.database`), which differs from the
/// display name whenever sanitizing changes it: both views carry the Postgres name.
#[tokio::test]
async fn me_and_get_database_show_the_postgres_database_name() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let mut overview = overview_with_database(tenant);
    overview.projects[0].databases[0].name = "web-1 prod".into();
    overview.projects[0].databases[0].database_name = None;
    let id = overview.projects[0].databases[0].database_id;
    metadata.insert_database(overview.projects[0].databases[0].clone());
    metadata.set_overview(overview);
    let app = spawn_app(metadata.clone(), FakeJobs::default()).await;

    let me: Value = get_me(&app, Some(&app.token_for_org("org-1"))).await.json().await.unwrap();
    let db = &me["tenant"]["projects"][0]["databases"][0];
    assert_eq!(db["name"], "web-1 prod");
    assert_eq!(db["postgres_database"], "web_1_prod");

    let get: Value = call(&app, Method::GET, &format!("/v1/databases/{}", id.as_uuid()), "org-1").await.json().await.unwrap();
    assert_eq!(get["postgres_database"], "web_1_prod");
}

// ---- data API keys

impl Branching {
    fn keys(&self, branch: BranchId) -> String {
        format!("{}/keys", self.branch(branch))
    }

    /// The branch's public key, sealed for display like the worker does.
    fn seed_public_key(&self, branch: BranchId) -> (uuid::Uuid, String) {
        let generated = data_key::generate(DataKeyKind::Public);
        let id = uuid::Uuid::new_v4();
        let new = NewDataKey {
            id,
            branch_id: branch,
            kind: DataKeyKind::Public,
            role: "anon".into(),
            name: "public".into(),
            prefix: generated.prefix.clone(),
            sha256: generated.sha256,
            sealed: Some(seal_credential(&self.app.key, &id.to_string(), Purpose::DataKey, &generated.key)),
            databases: None,
            role_oid: None,
            role_timeline: None,
            role_bypass_rls: false,
        };
        insert_key(&mut self.metadata.keys.lock().unwrap(), &new).unwrap();
        (id, generated.key)
    }

    fn seed_secret_key(&self, branch: BranchId, name: &str, role: &str) -> (uuid::Uuid, String) {
        let generated = data_key::generate(DataKeyKind::Secret);
        let id = uuid::Uuid::new_v4();
        let new = NewDataKey {
            id,
            branch_id: branch,
            kind: DataKeyKind::Secret,
            role: role.into(),
            name: name.into(),
            prefix: generated.prefix.clone(),
            sha256: generated.sha256,
            sealed: None,
            databases: None,
            role_oid: None,
            role_timeline: None,
            role_bypass_rls: false,
        };
        insert_key(&mut self.metadata.keys.lock().unwrap(), &new).unwrap();
        (id, generated.key)
    }

    async fn list_keys(&self, branch: BranchId) -> Vec<Value> {
        let resp = self.send(Method::GET, &self.keys(branch), None).await;
        assert_eq!(resp.status(), 200);
        resp.json().await.unwrap()
    }
}

#[tokio::test]
async fn list_shows_the_public_key_in_full_and_never_secret_keys() {
    let t = branching().await;
    let (public_id, public) = t.seed_public_key(t.main());
    let (secret_id, secret) = t.seed_secret_key(t.main(), "ci", "reporting");
    let list = t.list_keys(t.main()).await;
    assert_eq!(list.len(), 2);
    let by = |id: uuid::Uuid| list.iter().find(|k| k["id"] == json!(id)).unwrap();
    let p = by(public_id);
    assert_eq!(p["kind"], "public");
    assert_eq!(p["role"], "anon");
    assert_eq!(p["name"], "public");
    assert_eq!(p["key"], public);
    assert_eq!(p["prefix"], public[..12]);
    assert!(p["created_at"].is_string());
    assert!(p["last_used_at"].is_null());
    let s = by(secret_id);
    assert_eq!(s["kind"], "secret");
    assert_eq!(s["role"], "reporting");
    assert_eq!(s["name"], "ci");
    assert_eq!(s["prefix"], secret[..12]);
    assert!(s.get("key").is_none(), "{s}");
    let text = serde_json::to_string(&list).unwrap();
    for hidden in [secret.as_str(), "sealed", "sha256", "revoked"] {
        assert!(!text.contains(hidden), "leaked {hidden}");
    }
    // Another branch's keys are its own.
    let dev = t.ready("dev");
    assert!(t.list_keys(dev).await.is_empty());
}

#[tokio::test]
async fn create_secret_key_returns_it_once() {
    let t = branching().await;
    t.seed_public_key(t.main());
    let resp = t
        .send(Method::POST, &t.keys(t.main()), Some(json!({ "name": "ci", "role": "reporting" })))
        .await;
    assert_eq!(resp.status(), 201);
    let created: Value = resp.json().await.unwrap();
    let key = created["key"].as_str().unwrap().to_string();
    assert!(key.starts_with("chk_sec_") && key.len() == 51, "{key}");
    assert_eq!(created["kind"], "secret");
    assert_eq!(created["role"], "reporting");
    assert_eq!(created["name"], "ci");
    assert_eq!(created["prefix"], key[..12]);
    let id: uuid::Uuid = created["id"].as_str().unwrap().parse().unwrap();
    // The worker was asked first, then only the hash was stored.
    assert_eq!(*t.jobs.role_checks.lock().unwrap(), vec![(t.main(), "reporting".to_string())]);
    {
        let keys = t.metadata.keys.lock().unwrap();
        let (stored, hash) = keys.iter().find(|(k, _)| k.id == id).unwrap();
        assert_eq!(*hash, data_key::hash(&key));
        assert_eq!(stored.sealed, None);
        assert_eq!(stored.branch_id, t.main());
    }
    // A second list has no `key` for it.
    let list = t.list_keys(t.main()).await;
    let listed = list.iter().find(|k| k["id"] == json!(id)).expect("listed");
    assert!(listed.get("key").is_none(), "{listed}");
    assert!(!serde_json::to_string(&list).unwrap().contains(&key));

    // The role defaults to `authenticated`.
    let created: Value = t
        .send(Method::POST, &t.keys(t.main()), Some(json!({ "name": "web" })))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(created["role"], "authenticated");
}

#[tokio::test]
async fn create_validates_the_body() {
    let t = branching().await;
    for body in [
        json!({}),
        json!({ "name": "" }),
        json!({ "name": "x".repeat(64) }),
        json!({ "name": "ok", "role": "" }),
        json!({ "name": "ok", "surprise": 1 }),
        json!({ "name": 5 }),
    ] {
        let resp = t.send(Method::POST, &t.keys(t.main()), Some(body.clone())).await;
        assert_eq!(resp.status(), 422, "{body}");
    }
    assert_eq!(
        t.send(Method::POST, &t.keys(t.main()), Some(json!({ "name": "x".repeat(63) }))).await.status(),
        201
    );
    // Nothing was asked of the worker for the refused ones.
    assert_eq!(t.jobs.role_checks.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn secret_keys_store_the_role_oid_the_check_found() {
    let t = branching().await;
    t.seed_public_key(t.main());
    *t.jobs.role_check.lock().unwrap() = Some(Ok(CheckDataRoleReply::ok_with_oid(16390, "tl1")));
    let created: Value = t
        .send(Method::POST, &t.keys(t.main()), Some(json!({ "name": "ci", "role": "reporting" })))
        .await
        .json()
        .await
        .unwrap();
    let id: uuid::Uuid = created["id"].as_str().unwrap().parse().unwrap();
    // E26: bound to the OID and the timeline the check read it on.
    let identity = |id: uuid::Uuid| {
        let keys = t.metadata.keys.lock().unwrap();
        let k = &keys.iter().find(|(k, _)| k.id == id).unwrap().0;
        (k.role_oid, k.role_timeline.clone())
    };
    assert_eq!(identity(id), (Some(16390), Some("tl1".to_string())));
    let rotated: Value = t
        .send(Method::POST, &format!("{}/{id}/rotate", t.keys(t.main())), None)
        .await
        .json()
        .await
        .unwrap();
    let new_id: uuid::Uuid = rotated["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(identity(new_id), (Some(16390), Some("tl1".to_string())), "the rotated key keeps the identity");
    // After a switch the same OID still under the key's name: kept, bound to the new timeline.
    *t.jobs.role_check.lock().unwrap() = Some(Ok(CheckDataRoleReply::ok_with_oid(16390, "tl2")));
    let rotated: Value = t
        .send(Method::POST, &format!("{}/{new_id}/rotate", t.keys(t.main())), None)
        .await
        .json()
        .await
        .unwrap();
    let newer: uuid::Uuid = rotated["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(identity(newer), (Some(16390), Some("tl2".to_string())));
}

// E33: a secret key may use a role with BYPASSRLS; the check says so, the key stores it, and
// the list marks the key so Studio can show "bypasses RLS". A rotation takes the new check's
// answer; the public key never bypasses.
#[tokio::test]
async fn secret_keys_say_whether_their_role_bypasses_rls() {
    let t = branching().await;
    let (public, _) = t.seed_public_key(t.main());
    *t.jobs.role_check.lock().unwrap() =
        Some(Ok(CheckDataRoleReply::ok_with_oid(16390, "tl1").bypassing_rls(true)));
    let created: Value = t
        .send(Method::POST, &t.keys(t.main()), Some(json!({ "name": "svc", "role": "service" })))
        .await
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    let flag = |list: &[Value], id: &str| list.iter().find(|k| k["id"] == json!(id)).unwrap()["bypasses_rls"].clone();
    let list = t.list_keys(t.main()).await;
    assert_eq!(flag(&list, &id), true);
    assert_eq!(flag(&list, &public.to_string()), false);
    // The role lost BYPASSRLS since: the rotation's check says so.
    *t.jobs.role_check.lock().unwrap() =
        Some(Ok(CheckDataRoleReply::ok_with_oid(16390, "tl1").bypassing_rls(false)));
    let rotated: Value = t
        .send(Method::POST, &format!("{}/{id}/rotate", t.keys(t.main())), None)
        .await
        .json()
        .await
        .unwrap();
    let list = t.list_keys(t.main()).await;
    assert_eq!(flag(&list, rotated["id"].as_str().unwrap()), false);
    // A worker that predates E33 doesn't say: not marked.
    *t.jobs.role_check.lock().unwrap() = Some(Ok(CheckDataRoleReply::ok_with_oid(16391, "tl1")));
    let old: Value = t
        .send(Method::POST, &t.keys(t.main()), Some(json!({ "name": "old", "role": "other" })))
        .await
        .json()
        .await
        .unwrap();
    let list = t.list_keys(t.main()).await;
    assert_eq!(flag(&list, old["id"].as_str().unwrap()), false);
}

// The role was dropped and created again under the same name (a new OID): the old key is
// dead, and rotating it must not hand the new role a key.
#[tokio::test]
async fn rotate_refuses_a_key_whose_role_was_recreated() {
    let t = branching().await;
    t.seed_public_key(t.main());
    *t.jobs.role_check.lock().unwrap() = Some(Ok(CheckDataRoleReply::ok_with_oid(16390, "tl1")));
    let created: Value = t
        .send(Method::POST, &t.keys(t.main()), Some(json!({ "name": "ci", "role": "reporting" })))
        .await
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    *t.jobs.role_check.lock().unwrap() = Some(Ok(CheckDataRoleReply::ok_with_oid(16391, "tl1")));
    let resp = t.send(Method::POST, &format!("{}/{id}/rotate", t.keys(t.main())), None).await;
    let body = error_of(resp, 422).await;
    assert_eq!(code_of(&body), "role_refused");
    assert_eq!(
        message_of(&body),
        "the role reporting was dropped and created again since this key was made; create a new key for it"
    );
    let list = t.list_keys(t.main()).await;
    assert!(list.iter().any(|k| k["id"] == json!(id)), "the old key is still listed");
    assert_eq!(list.len(), 2, "nothing inserted");
}

#[tokio::test]
async fn secret_key_writes_while_the_endpoint_is_busy_are_503_with_retry_after() {
    let t = branching().await;
    t.seed_public_key(t.main());
    let (old_id, _) = t.seed_secret_key(t.main(), "ci", "reporting");
    *t.metadata.endpoint_busy.lock().unwrap() = true;
    let create = t
        .send(Method::POST, &t.keys(t.main()), Some(json!({ "name": "x", "role": "reporting" })))
        .await;
    assert_eq!(create.status(), 503);
    assert_eq!(create.headers()["retry-after"], "1");
    assert_eq!(code_of(&create.json::<Value>().await.unwrap()), "unavailable");
    let rotate = t.send(Method::POST, &format!("{}/{old_id}/rotate", t.keys(t.main())), None).await;
    assert_eq!(rotate.status(), 503);
    assert_eq!(rotate.headers()["retry-after"], "1");
    let list = t.list_keys(t.main()).await;
    assert_eq!(list.len(), 2, "nothing changed");
    assert!(list.iter().any(|k| k["id"] == json!(old_id)));
}

// C1: a NOLOGIN role without a password has no `neon_roles` row; the worker's check on the
// compute is what decides it exists, for create and for rotate.
#[tokio::test]
async fn secret_key_create_and_rotate_for_a_rowless_nologin_role() {
    let t = branching().await;
    t.seed_public_key(t.main());
    let created = t
        .send(Method::POST, &t.keys(t.main()), Some(json!({ "name": "ci", "role": "reader" })))
        .await;
    assert_eq!(created.status(), 201);
    let created: Value = created.json().await.unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    let rotated = t.send(Method::POST, &format!("{}/{id}/rotate", t.keys(t.main())), None).await;
    assert_eq!(rotated.status(), 201);
    let rotated: Value = rotated.json().await.unwrap();
    assert_eq!(rotated["role"], "reader");
    assert_eq!(
        *t.jobs.role_checks.lock().unwrap(),
        vec![(t.main(), "reader".to_string()), (t.main(), "reader".to_string())],
        "rotation re-checks the role on the compute"
    );
}

#[tokio::test]
async fn rotate_maps_check_role_refusal() {
    let t = branching().await;
    t.seed_public_key(t.main());
    let (old_id, _) = t.seed_secret_key(t.main(), "ci", "reporting");
    let path = format!("{}/{old_id}/rotate", t.keys(t.main()));
    #[allow(clippy::type_complexity)]
    let cases: [(Result<CheckDataRoleReply, ResetError>, u16, &str); 4] = [
        (Ok(CheckDataRoleReply::refused("role reporting doesn't exist")), 422, "role_refused"),
        (Ok(CheckDataRoleReply::not_ready()), 409, "branch_not_ready"),
        (Err(ResetError::Timeout), 504, "timeout"),
        (Err(ResetError::Transport("no worker listening".into())), 503, "worker_unavailable"),
    ];
    for (answer, status, code) in cases {
        *t.jobs.role_check.lock().unwrap() = Some(answer);
        let body = error_of(t.send(Method::POST, &path, None).await, status).await;
        assert_eq!(code_of(&body), code);
    }
    let list = t.list_keys(t.main()).await;
    assert!(list.iter().any(|k| k["id"] == json!(old_id)), "the old key is still live");
    assert_eq!(list.len(), 2, "nothing inserted");
}

// The public key's role is `anon`, which no DDL can drop: its rotation asks no worker.
#[tokio::test]
async fn rotate_public_key_skips_the_role_check() {
    let t = branching().await;
    let (public_id, _) = t.seed_public_key(t.main());
    let resp = t.send(Method::POST, &format!("{}/{public_id}/rotate", t.keys(t.main())), None).await;
    assert_eq!(resp.status(), 201);
    assert!(t.jobs.role_checks.lock().unwrap().is_empty());
}

// A rotation whose key was renamed after it was read: the worker refuses the old name, the
// key is read again and its new name checked.
#[tokio::test]
async fn rotate_follows_a_rename_seen_by_the_check() {
    let t = branching().await;
    t.seed_public_key(t.main());
    let (old_id, _) = t.seed_secret_key(t.main(), "ci", "reporting");
    *t.jobs.role_check.lock().unwrap() = Some(Ok(CheckDataRoleReply::refused("gone")));
    *t.jobs.rename_on_check.lock().unwrap() =
        Some((t.metadata.keys.clone(), old_id, "reporting2".into()));
    let resp = t.send(Method::POST, &format!("{}/{old_id}/rotate", t.keys(t.main())), None).await;
    // The fake refuses every check; what matters is that the new name was asked.
    let body = error_of(resp, 422).await;
    assert_eq!(code_of(&body), "role_refused");
    assert_eq!(
        *t.jobs.role_checks.lock().unwrap(),
        vec![(t.main(), "reporting".to_string()), (t.main(), "reporting2".to_string())]
    );
}

#[tokio::test]
async fn create_maps_check_role_refusal_to_422() {
    let t = branching().await;
    let body = || Some(json!({ "name": "ci", "role": "boss" }));
    #[allow(clippy::type_complexity)]
    let cases: [(Result<CheckDataRoleReply, ResetError>, u16, &str, Option<&str>); 6] = [
        (Ok(CheckDataRoleReply::refused("the role boss is a superuser")), 422, "role_refused", Some("the role boss is a superuser")),
        (Ok(CheckDataRoleReply::not_ready()), 409, "branch_not_ready", None),
        (Ok(CheckDataRoleReply::failed("grant failed: secret detail")), 500, "internal", None),
        (Err(ResetError::Timeout), 504, "timeout", None),
        (Err(ResetError::Transport("no worker listening".into())), 503, "worker_unavailable", None),
        // A reply that says no without saying why is our failure.
        (Ok(CheckDataRoleReply { ok: false, error: None, kind: None, role_oid: None, role_timeline: None, role_bypass_rls: None }), 500, "internal", None),
    ];
    for (answer, status, code, message) in cases {
        *t.jobs.role_check.lock().unwrap() = Some(answer);
        let resp = t.send(Method::POST, &t.keys(t.main()), body()).await;
        assert_eq!(resp.status(), status);
        let err: Value = resp.json().await.unwrap();
        let text = message_of(&err);
        assert_eq!(code_of(&err), code);
        if let Some(message) = message {
            assert_eq!(text, message);
        }
        assert!(!text.contains("secret detail"), "{text}");
    }
    assert!(t.metadata.keys.lock().unwrap().is_empty(), "a refused role stores no key");

    // A branch that isn't ready is a 409 without asking the worker.
    *t.jobs.role_check.lock().unwrap() = None;
    let before = t.jobs.role_checks.lock().unwrap().len();
    let resetting = t.add("busy", t.main(), BranchKind::Normal, BranchStatus::Resetting);
    let resp = t.send(Method::POST, &t.keys(resetting), body()).await;
    assert_eq!(resp.status(), 409);
    let backup = t.backup(t.main());
    let err = error_of(t.send(Method::POST, &t.keys(backup), body()).await, 409).await;
    assert_eq!(code_of(&err), "backup_branch");
    assert_eq!(t.jobs.role_checks.lock().unwrap().len(), before);
}

#[tokio::test]
async fn revoke_public_is_409() {
    let t = branching().await;
    let (public_id, _) = t.seed_public_key(t.main());
    let path = format!("{}/{public_id}", t.keys(t.main()));
    let resp = t.send(Method::DELETE, &path, None).await;
    assert_eq!(resp.status(), 409);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(code_of(&body), "public_key_not_revocable");
    assert_eq!(message_of(&body), "rotate the public key instead");
    assert_eq!(t.list_keys(t.main()).await.len(), 1);
}

#[tokio::test]
async fn revoke_secret_is_204_then_404() {
    let t = branching().await;
    let (id, _) = t.seed_secret_key(t.main(), "ci", "reporting");
    let (other, _) = t.seed_secret_key(t.main(), "keep", "reporting");
    let path = format!("{}/{id}", t.keys(t.main()));
    assert_eq!(t.send(Method::DELETE, &path, None).await.status(), 204);
    let list = t.list_keys(t.main()).await;
    assert_eq!(list.iter().map(|k| k["id"].clone()).collect::<Vec<_>>(), vec![json!(other)]);
    // Already revoked, unknown, not a uuid, and another branch's key.
    assert_eq!(t.send(Method::DELETE, &path, None).await.status(), 404);
    let unknown = format!("{}/{}", t.keys(t.main()), uuid::Uuid::new_v4());
    assert_eq!(t.send(Method::DELETE, &unknown, None).await.status(), 404);
    let dev = t.ready("dev");
    let wrong_branch = format!("{}/{other}", t.keys(dev));
    assert_eq!(t.send(Method::DELETE, &wrong_branch, None).await.status(), 404);
    assert_eq!(t.list_keys(t.main()).await.len(), 1);
}

#[tokio::test]
async fn rotate_unknown_is_404() {
    let t = branching().await;
    let (id, _) = t.seed_secret_key(t.main(), "ci", "reporting");
    let unknown = format!("{}/{}/rotate", t.keys(t.main()), uuid::Uuid::new_v4());
    assert_eq!(t.send(Method::POST, &unknown, None).await.status(), 404);
    // Revoked keys can't be rotated.
    assert_eq!(t.send(Method::DELETE, &format!("{}/{id}", t.keys(t.main())), None).await.status(), 204);
    let revoked = format!("{}/{id}/rotate", t.keys(t.main()));
    assert_eq!(t.send(Method::POST, &revoked, None).await.status(), 404);
    assert!(t.list_keys(t.main()).await.is_empty());
}

#[tokio::test]
async fn rotate_public_gives_a_new_public_key_in_full() {
    let t = branching().await;
    let (old_id, old) = t.seed_public_key(t.main());
    let resp = t.send(Method::POST, &format!("{}/{old_id}/rotate", t.keys(t.main())), None).await;
    assert_eq!(resp.status(), 201);
    let created: Value = resp.json().await.unwrap();
    let key = created["key"].as_str().unwrap();
    assert!(key.starts_with("chk_pub_") && key.len() == 51 && key != old, "{key}");
    assert_eq!(created["kind"], "public");
    assert_eq!(created["role"], "anon");
    assert_eq!(created["name"], "public");
    assert_ne!(created["id"], json!(old_id));
    // Only the new one is live, and it is shown in full (sealed under its own id).
    let list = t.list_keys(t.main()).await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], created["id"]);
    assert_eq!(list[0]["key"], key);
    let new_id: uuid::Uuid = created["id"].as_str().unwrap().parse().unwrap();
    let sealed = t.metadata.keys.lock().unwrap().iter().find(|(k, _)| k.id == new_id).unwrap().0.sealed.clone().unwrap();
    assert_eq!(open_credential(&t.app.key, &new_id.to_string(), Purpose::DataKey, &sealed).unwrap(), key);
    // No role check for a public key.
    assert!(t.jobs.role_checks.lock().unwrap().is_empty());
}

#[tokio::test]
async fn rotate_secret_keeps_name_and_role() {
    let t = branching().await;
    t.seed_public_key(t.main());
    let (old_id, old) = t.seed_secret_key(t.main(), "ci", "reporting");
    let created: Value = t
        .send(Method::POST, &format!("{}/{old_id}/rotate", t.keys(t.main())), None)
        .await
        .json()
        .await
        .unwrap();
    let key = created["key"].as_str().unwrap();
    assert!(key.starts_with("chk_sec_") && key != old);
    assert_eq!((created["kind"].as_str(), created["role"].as_str(), created["name"].as_str()), (Some("secret"), Some("reporting"), Some("ci")));
    let list = t.list_keys(t.main()).await;
    assert_eq!(list.len(), 2, "the public key and the new secret key");
    assert!(list.iter().all(|k| k["id"] != json!(old_id)));
    assert!(list.iter().find(|k| k["kind"] == "secret").unwrap().get("key").is_none());
}

#[tokio::test]
async fn another_tenant_gets_404() {
    let t = branching().await;
    let mut other_db = None;
    let other = lifecycle_app(FakeJobs::default(), |m, _t, k| {
        let db = ready_database(TenantId::new(), k);
        other_db = Some((db.database_id, BranchId::from(db.database_id)));
        m.insert_database(db);
    })
    .await;
    let (db, branch) = other_db.unwrap();
    let base = format!("/v1/databases/{}/branches/{}", db.as_uuid(), branch.as_uuid());
    let key = uuid::Uuid::new_v4();
    for (method, path, body) in [
        (Method::GET, format!("{base}/keys"), None),
        (Method::POST, format!("{base}/keys"), Some(json!({ "name": "x" }))),
        (Method::POST, format!("{base}/keys/{key}/rotate"), None),
        (Method::DELETE, format!("{base}/keys/{key}"), None),
        (Method::GET, format!("{base}/data-api/schema"), None),
    ] {
        let mut req = reqwest::Client::new()
            .request(method.clone(), format!("{}{path}", other.base))
            .header(IDENTITY_HEADER, other.token_for_org("org-1"));
        if let Some(body) = body {
            req = req.json(&body);
        }
        assert_eq!(req.send().await.unwrap().status(), 404, "{method} {path}");
    }
    drop(t);
}

/// Another tenant's database: every per-database data API route and the console answer 404
/// as for a missing one, and nothing runs.
#[tokio::test]
async fn data_api_database_routes_of_another_tenant_are_404() {
    let jobs = FakeJobs::default();
    let mut other_db = None;
    let other = lifecycle_app(jobs.clone(), |m, _t, k| {
        let db = ready_database(TenantId::new(), k);
        let branch = BranchId::from(db.database_id);
        other_db = Some((db.database_id, branch));
        m.insert_database(db);
        m.add_branch_db(branch, "web1", "postgres", DatabaseOrigin::Main);
        m.add_branch_db(branch, "analytics", "etl", DatabaseOrigin::Sql);
    })
    .await;
    let (db, branch) = other_db.unwrap();
    let base = format!("/v1/databases/{}/branches/{}", db.as_uuid(), branch.as_uuid());
    let key = uuid::Uuid::new_v4();
    for (method, path, body) in [
        (Method::POST, format!("{base}/pg-databases/analytics/data-api"), None),
        (Method::DELETE, format!("{base}/pg-databases/analytics/data-api?remove=true"), None),
        (Method::PATCH, format!("{base}/pg-databases/analytics"), Some(json!({ "default_schema": "app" }))),
        (Method::PATCH, format!("{base}/keys/{key}"), Some(json!({ "databases": null }))),
        (Method::POST, format!("{base}/console/query"), Some(json!({ "role": null, "tree": { "v": 1, "op": "select", "from": "t" } }))),
        (Method::POST, format!("{base}/console/query"), Some(json!({ "role": "editor", "tree": { "v": 1, "op": "select", "from": "t" } }))),
        (Method::GET, format!("{base}/console/schema"), None),
        (Method::GET, format!("{base}/console/schema?role=editor"), None),
    ] {
        let mut req = reqwest::Client::new()
            .request(method.clone(), format!("{}{path}", other.base))
            .header(IDENTITY_HEADER, other.token_for_org("org-1"));
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req.send().await.unwrap();
        assert_eq!(code_of(&error_of(resp, 404).await), "not_found", "{method} {path}");
    }
    assert!(jobs.data_api_calls.lock().unwrap().is_empty());
    assert!(jobs.role_checks.lock().unwrap().is_empty());
}

#[tokio::test]
async fn keys_of_an_unknown_branch_are_404() {
    let t = branching().await;
    let unknown = format!("{}/keys", t.branch(BranchId::new()));
    assert_eq!(t.send(Method::GET, &unknown, None).await.status(), 404);
    assert_eq!(t.send(Method::POST, &unknown, Some(json!({ "name": "x" }))).await.status(), 404);
}

#[tokio::test]
async fn settings_patch_bounds() {
    let t = branching().await;
    let path = database_path(&t);
    for (field, ok, bad) in [
        ("max_rows", [1, 100000], [0, 100001]),
        ("statement_timeout_ms", [100, 120000], [99, 120001]),
        ("max_connections", [1, 50], [0, 51]),
    ] {
        for value in ok {
            let resp = t.send(Method::PATCH, &path, Some(json!({ field: value }))).await;
            assert_eq!(resp.status(), 200, "{field}={value}");
            let body: Value = resp.json().await.unwrap();
            assert_eq!(body[field], value);
        }
        for value in bad {
            assert_eq!(t.send(Method::PATCH, &path, Some(json!({ field: value }))).await.status(), 422, "{field}={value}");
        }
        assert_eq!(t.send(Method::PATCH, &path, Some(json!({ field: "many" }))).await.status(), 422);
    }
    // `default_schema` moved to the per-database settings: this route no longer knows it.
    let moved = t.send(Method::PATCH, &path, Some(json!({ "default_schema": "app" }))).await;
    assert!(moved.status().is_client_error(), "{}", moved.status());
    // Refused values change nothing, and the stored values come back on the next PATCH.
    let db = t.metadata.database(t.db);
    assert_eq!((db.max_rows, db.statement_timeout_ms, db.max_connections), (100000, 120000, 50));
    let body: Value = t.send(Method::PATCH, &path, Some(json!({}))).await.json().await.unwrap();
    assert_eq!(body["max_rows"], 100000);
}

// ---- the data API's schema, proxied from rest

type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

async fn mock_rest(seen: Seen) -> String {
    use axum::extract::Path;
    use axum::http::HeaderMap;
    use axum::routing::get;
    let app = Router::new().route(
        "/internal/schema/{branch}",
        get(move |Path(branch): Path<String>, headers: HeaderMap| {
            let seen = seen.clone();
            async move {
                let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_string);
                seen.lock().unwrap().push((branch, auth));
                axum::Json(json!({ "version": "v1", "schemas": { "public": { "tables": {}, "functions": {}, "enums": {} } } }))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn data_api_schema_is_proxied_from_rest() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let rest = mock_rest(seen.clone()).await;
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app_with_rest(metadata.clone(), jobs.clone(), &rest).await;
    let db = ready_database(tenant, &app.key);
    let id = db.database_id;
    metadata.insert_database(db);
    let t = Branching { app, metadata, jobs, db: id };

    let path = format!("{}/data-api/schema", t.branch(t.main()));
    let resp = t.send(Method::GET, &path, None).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["version"], "v1");
    assert!(body["schemas"]["public"].is_object());
    assert_eq!(
        *seen.lock().unwrap(),
        vec![(t.main().as_uuid().to_string(), Some(format!("Bearer {REST_TOKEN}")))]
    );
}

/// A rest that records each `?database=` it is asked for (`None` when absent) and answers
/// 404 `unknown_database` for `gone`.
async fn mock_rest_schema_databases() -> (String, Arc<Mutex<Vec<Option<String>>>>) {
    use axum::extract::Query;
    use axum::routing::get;
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let app = Router::new().route(
        "/internal/schema/{branch}",
        get(move |Query(q): Query<std::collections::HashMap<String, String>>| {
            let log = log.clone();
            async move {
                let database = q.get("database").cloned();
                log.lock().unwrap().push(database.clone());
                if database.as_deref() == Some("gone") {
                    return (
                        axum::http::StatusCode::NOT_FOUND,
                        axum::Json(json!({ "error": { "code": "unknown_database", "message": "no such database" } })),
                    );
                }
                (axum::http::StatusCode::OK, axum::Json(json!({ "version": "v1", "schemas": {}, "database": database })))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (rest, seen)
}

async fn branching_with_schema_rest() -> (Branching, Arc<Mutex<Vec<Option<String>>>>) {
    let (rest, seen) = mock_rest_schema_databases().await;
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app_with_rest(metadata.clone(), jobs.clone(), &rest).await;
    let db = ready_database(tenant, &app.key);
    let id = db.database_id;
    metadata.insert_database(db);
    (Branching { app, metadata, jobs, db: id }, seen)
}

#[tokio::test]
async fn data_api_schema_forwards_the_database() {
    let (t, seen) = branching_with_schema_rest().await;
    let main = t.seeded();
    t.metadata.add_branch_db(main, "my db&x", "etl", DatabaseOrigin::Sql);
    t.metadata.set_data_api(main, "my db&x", chelabase_metadata::DataApiState::On);
    let path = format!("{}/data-api/schema", t.branch(main));
    // No parameter: main, as today.
    assert_eq!(t.send(Method::GET, &path, None).await.status(), 200);
    let resp = t.send(Method::GET, &format!("{path}?database=web1"), None).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["database"], "web1");
    // The name is percent-encoded on the way to rest and decoded there.
    assert_eq!(t.send(Method::GET, &format!("{path}?database=my%20db%26x"), None).await.status(), 200);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![None, Some("web1".to_string()), Some("my db&x".to_string())]
    );
}

#[tokio::test]
async fn data_api_schema_unknown_or_disabled_database() {
    let (t, seen) = branching_with_schema_rest().await;
    let main = t.seeded();
    let path = format!("{}/data-api/schema", t.branch(main));
    let err = error_of(t.send(Method::GET, &format!("{path}?database=nope"), None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    let err = error_of(t.send(Method::GET, &format!("{path}?database=analytics"), None).await, 409).await;
    assert_eq!(code_of(&err), "data_api_disabled");
    // Both are answered from metadata, before rest is asked.
    assert!(seen.lock().unwrap().is_empty());
    // Rest saying `unknown_database` for a database metadata lists as on (it vanished
    // meanwhile): answered by what metadata says, here still not found.
    t.metadata.add_branch_db(main, "gone", "etl", DatabaseOrigin::Sql);
    t.metadata.set_data_api(main, "gone", chelabase_metadata::DataApiState::On);
    let err = error_of(t.send(Method::GET, &format!("{path}?database=gone"), None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn data_api_schema_is_503_when_rest_is_down() {
    let t = branching().await; // rest points at a closed port
    let path = format!("{}/data-api/schema", t.branch(t.main()));
    let resp = t.send(Method::GET, &path, None).await;
    assert_eq!(resp.status(), 503);
    let text = resp.text().await.unwrap();
    assert!(!text.contains(&std::net::Ipv4Addr::LOCALHOST.to_string()), "{text}");
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(code_of(&body), "unavailable");
}

/// Rest answering 503 (the branch's database is unreachable, the schema is being
/// reinstalled) is passed on with its body and `Retry-After`, not turned into a 502.
#[tokio::test]
async fn data_api_schema_passes_rests_503_through() {
    use axum::routing::get;
    let app = Router::new().route(
        "/internal/schema/{branch}",
        get(|| async {
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                [(axum::http::header::RETRY_AFTER, "2")],
                axum::Json(json!({ "error": { "code": "unavailable", "message": "try again" } })),
            )
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app_with_rest(metadata.clone(), jobs.clone(), &rest).await;
    let db = ready_database(tenant, &app.key);
    let id = db.database_id;
    metadata.insert_database(db);
    let t = Branching { app, metadata, jobs, db: id };

    let resp = t.send(Method::GET, &format!("{}/data-api/schema", t.branch(t.main())), None).await;
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.headers()["retry-after"], "2");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "unavailable");
    assert_eq!(body["error"]["message"], "try again");
}

// ---- error shape rulings

#[tokio::test]
async fn parent_lookup_error_is_500() {
    let t = branching().await;
    // Only the parent lookup reads a branch before the create.
    *t.metadata.fail_get_branch.lock().unwrap() = true;
    let err = error_of(t.send(Method::POST, &t.branches(), Some(json!({ "name": "x" }))).await, 500).await;
    assert_eq!(code_of(&err), "internal");
    assert!(t.published().is_empty());
}

#[tokio::test]
async fn publish_failure_is_503_worker_unavailable() {
    let t = branching().await;
    *t.jobs.fail_branch_publish.lock().unwrap() = true;
    let err = error_of(t.send(Method::POST, &t.branches(), Some(json!({ "name": "dev" }))).await, 503).await;
    assert_eq!(code_of(&err), "worker_unavailable");
    let dev = t.ready("qa");
    let err = error_of(t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await, 503).await;
    assert_eq!(code_of(&err), "worker_unavailable");
    let err = error_of(
        t.send(Method::POST, &format!("{}/restore", t.branch(dev)), Some(json!({ "at": { "lsn": "0/1" } }))).await,
        503,
    )
    .await;
    assert_eq!(code_of(&err), "worker_unavailable");

    // Provisioning a database too.
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    *jobs.fail_provision_publish.lock().unwrap() = true;
    let project = metadata.create_project(tenant, "Default").await.unwrap().id;
    let app = spawn_app(metadata, jobs).await;
    let token = app.token_for_org("org-1");
    let resp = post_create(&app, Some(&token), json!({ "project_id": project, "name": "demo" })).await;
    let err = error_of(resp, 503).await;
    assert_eq!(code_of(&err), "worker_unavailable");
}

#[tokio::test]
async fn backup_branch_key_create_is_backup_branch() {
    let t = branching().await;
    let backup = t.backup(t.main());
    let err = error_of(t.send(Method::POST, &t.keys(backup), Some(json!({ "name": "ci" }))).await, 409).await;
    assert_eq!(code_of(&err), "backup_branch");
    assert!(t.jobs.role_checks.lock().unwrap().is_empty());
}

// Another tenant's project looks exactly like a missing one: same status, same body.
#[tokio::test]
async fn create_database_in_another_tenants_project_is_the_missing_project_answer() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let (foreign, missing) = (ProjectId::new(), ProjectId::new());
    metadata.foreign_projects.lock().unwrap().push(foreign);
    metadata.missing_projects.lock().unwrap().push(missing);
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let token = app.token_for_org("org-1");
    let body = |project: ProjectId| json!({ "project_id": project, "name": "demo" });

    let resp = post_create(&app, Some(&token), body(foreign)).await;
    let foreign_err = error_of(resp, 404).await;
    let resp = post_create(&app, Some(&token), body(missing)).await;
    let missing_err = error_of(resp, 404).await;
    assert_eq!(code_of(&foreign_err), "not_found");
    assert_eq!(code_of(&missing_err), "not_found");
    // Only the id differs; the message must not name a tenant or say "belong".
    assert_eq!(
        message_of(&foreign_err),
        message_of(&missing_err).replace(&missing.to_string(), &foreign.to_string())
    );
    assert!(!message_of(&foreign_err).contains("belong"), "{foreign_err}");
}

#[tokio::test]
async fn bad_json_body_is_json_invalid_request() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let url = format!("{}/v1/databases", app.base);
    for (content_type, body, status) in [
        ("application/json", "{nope", 400),
        ("text/plain", "{}", 415),
        ("application/json", r#"{"tenant_id": 5}"#, 422),
    ] {
        let resp = reqwest::Client::new()
            .post(&url)
            .header(IDENTITY_HEADER, app.token_for_org("org-1"))
            .header("content-type", content_type)
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.headers()["content-type"], "application/json", "{body}");
        let err = error_of(resp, status).await;
        assert_eq!(code_of(&err), "invalid_request", "{body}");
        assert!(!message_of(&err).is_empty());
    }
}

#[tokio::test]
async fn non_uuid_path_is_json_invalid_request() {
    let t = branching().await;
    for path in [
        "/v1/databases/not-a-uuid".to_string(),
        "/v1/databases/not-a-uuid/branches".to_string(),
        format!("/v1/databases/{}/branches/not-a-uuid/connection", t.db.as_uuid()),
        format!("/v1/databases/{}/branches/not-a-uuid/keys", t.db.as_uuid()),
    ] {
        let resp = t.send(Method::GET, &path, None).await;
        assert_eq!(resp.status(), 400, "{path}");
        assert_eq!(resp.headers()["content-type"], "application/json", "{path}");
        let err = error_of(resp, 400).await;
        assert_eq!(code_of(&err), "invalid_request", "{path}");
    }
}

/// Starts a mock rest answering the schema route with `status` and `body`.
async fn rest_answering(status: u16, body: Value) -> Branching {
    use axum::routing::get;
    let app = Router::new().route(
        "/internal/schema/{branch}",
        get(move || {
            let body = body.clone();
            async move { (axum::http::StatusCode::from_u16(status).unwrap(), axum::Json(body)) }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app_with_rest(metadata.clone(), jobs.clone(), &rest).await;
    let db = ready_database(tenant, &app.key);
    let id = db.database_id;
    metadata.insert_database(db);
    Branching { app, metadata, jobs, db: id }
}

#[tokio::test]
async fn schema_409_is_branch_not_ready() {
    let t = rest_answering(409, json!({ "status": "resetting" })).await;
    let resp = t.send(Method::GET, &format!("{}/data-api/schema", t.branch(t.main())), None).await;
    let err = error_of(resp, 409).await;
    assert_eq!(code_of(&err), "branch_not_ready");
    assert_eq!(message_of(&err), "branch is not ready");
    assert_eq!(err["error"].as_object().unwrap().len(), 2, "{err}");
}

#[tokio::test]
async fn schema_404_is_not_found_and_unexpected_answers_are_unavailable() {
    let t = rest_answering(404, json!({ "error": { "code": "unknown_branch", "message": "no such branch" } })).await;
    let resp = t.send(Method::GET, &format!("{}/data-api/schema", t.branch(t.main())), None).await;
    let err = error_of(resp, 404).await;
    assert_eq!(code_of(&err), "not_found");
    let t = rest_answering(500, json!({ "oops": true })).await;
    let resp = t.send(Method::GET, &format!("{}/data-api/schema", t.branch(t.main())), None).await;
    let err = error_of(resp, 502).await;
    assert_eq!(code_of(&err), "unavailable");
}

// ---- login roles

impl Branching {
    fn roles(&self, branch: BranchId) -> String {
        format!("{}/roles", self.branch(branch))
    }

    /// The branch's endpoint, with its owner row so a list has something to start from.
    fn with_owner_row(&self, branch: BranchId) -> String {
        let ep = self.metadata.branch(branch).endpoint_id.unwrap();
        self.metadata.add_role(&ep, "postgres", RoleOrigin::Owner, true, false);
        ep
    }
}

fn role_row(name: &str, origin: RoleOrigin, level: RoleLevel, password: PasswordState) -> RoleRow {
    RoleRow {
        name: name.into(),
        origin,
        level,
        login: true,
        password,
        created_at: chrono::Utc::now(),
        pending: false,
        attributes: RoleAttributes::default(),
    }
}

fn role_reply(code: &str) -> RoleReply {
    RoleReply { ok: false, code: Some(code.into()), error: Some("worker said so".into()), keys: vec![], reassigned: None, preview: None }
}

// c3 (SP13): a personal login is listed with the new `personal` origin, between the others.
#[tokio::test]
async fn list_roles_hides_personal_logins() {
    // Members hub T8 (MH-11), a deliberate change: this test used to assert that a personal
    // login is listed with origin `personal`. Every generic role route now treats one as absent.
    let t = branching().await;
    let dev = t.ready("dev");
    let ep = t.with_owner_row(dev);
    t.metadata.add_role(&ep, "app", RoleOrigin::Api, false, false);
    t.metadata.add_role_row(&ep, role_row("alice", RoleOrigin::Personal, RoleLevel::Plain, PasswordState::SetInSql));
    t.metadata.personal_login_names.lock().unwrap().insert((t.db.as_uuid(), "alice".to_string()));
    let resp = t.send(Method::GET, &t.roles(dev), None).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let origin_of = |name: &str| {
        body.as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name)
            .map(|r| r["origin"].clone())
    };
    assert_eq!(origin_of("alice"), None, "a personal login is not listed: {body}");
    assert_eq!(origin_of("app"), Some(json!("api")));
    assert_eq!(origin_of("postgres"), Some(json!("owner")));
}

#[tokio::test]
async fn list_roles_shapes_and_blocked_keys() {
    let t = branching().await;
    let dev = t.ready("dev");
    let ep = t.with_owner_row(dev);
    t.metadata.add_role(&ep, "app", RoleOrigin::Api, false, true);
    t.metadata.add_role_row(&ep, role_row("etl", RoleOrigin::Sql, RoleLevel::OwnerLevel, PasswordState::SetInSql));
    t.metadata.add_role_row(&ep, role_row("old", RoleOrigin::Sql, RoleLevel::Plain, PasswordState::Md5Unusable));
    // Live secret keys naming `etl` (owner-level) and `app` (plain): only the first is blocked.
    {
    let mut keys = t.metadata.keys.lock().unwrap();
    for (role, name) in [("etl", "ci"), ("app", "web")] {
        let id = uuid::Uuid::new_v4();
        insert_key(
            &mut keys,
            &NewDataKey {
                id,
                branch_id: dev,
                kind: DataKeyKind::Secret,
                role: role.into(),
                name: name.into(),
                prefix: "ck_x".into(),
                sealed: None,
                sha256: [id.as_bytes()[0]; 32],
                databases: None,
                role_oid: None,
                role_timeline: None,
                role_bypass_rls: false,
            },
        )
        .unwrap();
    }
    }
    let resp = t.send(Method::GET, &t.roles(dev), None).await;
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("cache-control").is_none());
    let body: Value = resp.json().await.unwrap();
    let rows = body.as_array().unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0]["name"], "postgres");
    assert_eq!(rows[0]["origin"], "owner");
    assert_eq!(rows[0]["level"], "owner_level");
    assert_eq!(rows[0]["blocked_keys"], json!([]));
    assert_eq!(rows[1]["name"], "app");
    assert_eq!(rows[1]["origin"], "api");
    assert_eq!(rows[1]["level"], "plain");
    assert_eq!(rows[1]["login"], true);
    assert_eq!(rows[1]["password"], "generated");
    assert_eq!(rows[1]["pending"], true);
    assert_eq!(rows[1]["blocked_keys"], json!([]));
    assert_eq!(rows[2]["origin"], "sql");
    assert_eq!(rows[2]["level"], "owner_level");
    assert_eq!(rows[2]["password"], "set_in_sql");
    assert_eq!(rows[2]["blocked_keys"].as_array().unwrap().len(), 1);
    assert_eq!(rows[2]["blocked_keys"][0]["name"], "ci");
    assert!(rows[2]["blocked_keys"][0]["id"].is_string());
    assert_eq!(rows[3]["password"], "md5_unusable");
    assert!(rows[1]["created_at"].is_string());
    for row in rows {
        assert!(row.get("kind").is_none() && row.get("privileged").is_none(), "{row}");
    }
}

#[tokio::test]
async fn list_roles_reports_unreadable() {
    let t = branching().await;
    let dev = t.ready("dev");
    let ep = t.with_owner_row(dev);
    t.metadata.add_role_row(&ep, role_row("lost", RoleOrigin::Api, RoleLevel::Plain, PasswordState::Unreadable));
    let body: Value = t.send(Method::GET, &t.roles(dev), None).await.json().await.unwrap();
    let row = body.as_array().unwrap().iter().find(|r| r["name"] == "lost").unwrap();
    assert_eq!(row["password"], "unreadable");
}

// C10: a key says whether its role is owner-level (the data API refuses such a key), so Studio
// needn't list the roles.
#[tokio::test]
async fn data_keys_mark_blocked_roles() {
    let t = branching().await;
    let dev = t.ready("dev");
    let ep = t.with_owner_row(dev);
    t.metadata.add_role_row(&ep, role_row("etl", RoleOrigin::Sql, RoleLevel::OwnerLevel, PasswordState::SetInSql));
    t.metadata.add_role_row(&ep, role_row("app", RoleOrigin::Api, RoleLevel::Plain, PasswordState::Generated));
    let (blocked, _) = t.seed_secret_key(dev, "ci", "etl");
    let (plain, _) = t.seed_secret_key(dev, "web", "app");
    let (gone, _) = t.seed_secret_key(dev, "ghost", "dropped");
    let (public, _) = t.seed_public_key(dev);
    let list = t.list_keys(dev).await;
    let flag = |id: uuid::Uuid| list.iter().find(|k| k["id"] == json!(id)).unwrap()["role_blocked"].clone();
    assert_eq!(flag(blocked), true);
    assert_eq!(flag(plain), false);
    assert_eq!(flag(gone), false);
    assert_eq!(flag(public), false);
}

// A failing role lookup doesn't fail the key list: the flag is a hint, so every key answers
// `role_blocked: false`.
#[tokio::test]
async fn data_keys_list_survives_a_failing_role_lookup() {
    let t = branching().await;
    let dev = t.ready("dev");
    let ep = t.with_owner_row(dev);
    t.metadata.add_role_row(&ep, role_row("etl", RoleOrigin::Sql, RoleLevel::OwnerLevel, PasswordState::SetInSql));
    let (key, _) = t.seed_secret_key(dev, "ci", "etl");
    *t.metadata.fail_list_roles.lock().unwrap() = true;
    let list = t.list_keys(dev).await;
    let entry = list.iter().find(|k| k["id"] == json!(key)).unwrap();
    assert_eq!(entry["role_blocked"], false);
}

#[tokio::test]
async fn create_role_takes_owner_level_and_refuses_privileged() {
    let t = branching().await;
    let dev = t.ready("dev");
    let ep = t.with_owner_row(dev);
    // The worker writes the row; the fake answers ok.
    t.metadata.add_role(&ep, "app", RoleOrigin::Api, true, true);
    let resp = t.send(Method::POST, &t.roles(dev), Some(json!({ "name": "app", "owner_level": true }))).await;
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.unwrap();
    // Exactly the role's fields: `password` is the state (`generated`), never a secret.
    let mut fields: Vec<&str> = body.as_object().unwrap().keys().map(String::as_str).collect();
    fields.sort();
    assert_eq!(
        fields,
        [
            "blocked_keys",
            "bypass_rls",
            "connection_limit",
            "created_at",
            "guard_exempt",
            "inherit",
            "level",
            "login",
            "member_admin",
            "member_no_set",
            "member_of",
            "name",
            "origin",
            "password",
            "pending",
            "valid_until",
        ],
        "{body}"
    );
    assert_eq!(body["password"], "generated");
    assert_eq!(body["name"], "app");
    assert_eq!(body["origin"], "api");
    assert_eq!(body["level"], "owner_level");
    assert_eq!(body["pending"], true);
    assert!(body["created_at"].is_string());
    assert_eq!(*t.jobs.role_calls.lock().unwrap(), vec!["create:app:true".to_string()]);
    // `privileged` was revision 1's field: now unknown.
    let err = error_of(t.send(Method::POST, &t.roles(dev), Some(json!({ "name": "x", "privileged": true }))).await, 422).await;
    assert_eq!(code_of(&err), "invalid_request");
    assert_eq!(t.jobs.role_calls.lock().unwrap().len(), 1);
}

// U1: the new fields reach the worker as sent; omitted ones keep a plain LOGIN role's
// defaults; the answer carries the attributes and never a password.
#[tokio::test]
async fn create_role_accepts_new_fields() {
    let t = branching().await;
    let dev = t.ready("dev");
    let ep = t.with_owner_row(dev);
    let attributes = RoleAttributes {
        inherit: false,
        connection_limit: 3,
        valid_until: chrono::DateTime::from_timestamp(1_900_000_000, 0),
        bypass_rls: true,
        member_of: vec!["readers".into()],
        member_no_set: vec![],
        member_admin: vec![],
    };
    t.metadata.add_role_row(
        &ep,
        RoleRow {
            login: false,
            attributes: attributes.clone(),
            ..role_row("grp", RoleOrigin::Api, RoleLevel::Plain, PasswordState::None)
        },
    );
    let resp = t
        .send(
            Method::POST,
            &t.roles(dev),
            Some(json!({
                "name": "grp",
                "login": false,
                "inherit": false,
                "connection_limit": 3,
                "valid_until": "2030-03-17T17:46:40Z",
                "bypass_rls": true,
                "member_of": ["readers"],
            })),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["login"], false);
    assert_eq!(body["password"], "none");
    assert_eq!(body["inherit"], false);
    assert_eq!(body["connection_limit"], 3);
    assert_eq!(body["valid_until"], "2030-03-17T17:46:40Z");
    assert_eq!(body["bypass_rls"], true);
    assert_eq!(body["member_of"], json!(["readers"]));
    let sent = t.jobs.role_creates.lock().unwrap().clone();
    assert_eq!(
        sent,
        vec![RoleCreateRequest {
            login: false,
            inherit: false,
            connection_limit: 3,
            valid_until: attributes.valid_until,
            bypass_rls: true,
            member_of: vec!["readers".into()],
            ..RoleCreateRequest::login(dev, "grp", false)
        }]
    );

    // Omitted fields: today's defaults.
    t.metadata.add_role(&ep, "app", RoleOrigin::Api, false, false);
    let resp = t.send(Method::POST, &t.roles(dev), Some(json!({ "name": "app" }))).await;
    assert_eq!(resp.status(), 201);
    assert_eq!(
        t.jobs.role_creates.lock().unwrap().last().unwrap(),
        &RoleCreateRequest::login(dev, "app", false)
    );
}

// U2: only the allowed attributes can be expressed; anything else is refused before the
// worker hears of it, as are memberships in Chelabase's own roles.
#[tokio::test]
async fn create_role_refuses_unknown_attribute() {
    let t = branching().await;
    let dev = t.ready("dev");
    for field in ["superuser", "createrole", "createdb", "replication", "password", "admin_of"] {
        let err = error_of(
            t.send(Method::POST, &t.roles(dev), Some(json!({ "name": "app", field: true }))).await,
            422,
        )
        .await;
        assert_eq!(code_of(&err), "invalid_request", "{field}");
    }
    for target in ["authenticated", "anon", "authenticator", "neon_superuser", "chelabase_admin", "pg_monitor", "postgres", "app"] {
        let err = error_of(
            t.send(Method::POST, &t.roles(dev), Some(json!({ "name": "app", "member_of": [target] }))).await,
            422,
        )
        .await;
        assert_eq!(code_of(&err), "role_membership_not_allowed", "{target}");
    }
    let err = error_of(
        t.send(Method::POST, &t.roles(dev), Some(json!({ "name": "app", "connection_limit": -2 }))).await,
        422,
    )
    .await;
    assert_eq!(code_of(&err), "role_attribute_not_allowed");
    assert!(t.jobs.role_calls.lock().unwrap().is_empty());
    // The worker's refusal of a target (missing, or not the owner's to grant) maps too.
    *t.jobs.role_reply.lock().unwrap() = Some(Ok(role_reply("role_membership_not_allowed")));
    let err = error_of(
        t.send(Method::POST, &t.roles(dev), Some(json!({ "name": "app", "member_of": ["ghost"] }))).await,
        422,
    )
    .await;
    assert_eq!(code_of(&err), "role_membership_not_allowed");
}

// U1: PATCH changes what's given (null clears the expiry), answers 202 with the role, and
// asks the worker over the update RPC.
#[tokio::test]
async fn patch_role_is_202_and_publishes() {
    let t = branching().await;
    let dev = t.ready("dev");
    let ep = t.with_owner_row(dev);
    t.metadata.add_role(&ep, "app", RoleOrigin::Api, false, false);
    let path = format!("{}/app", t.roles(dev));
    let resp = t
        .send(
            Method::PATCH,
            &path,
            Some(json!({ "login": false, "bypass_rls": true, "valid_until": null, "member_of": [] })),
        )
        .await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["name"], "app");
    assert!(body.get("password").is_some_and(|p| p.is_string()));
    assert_eq!(*t.jobs.role_calls.lock().unwrap(), vec!["update:app".to_string()]);
    assert_eq!(
        t.jobs.role_updates.lock().unwrap().clone(),
        vec![RoleUpdateRequest {
            login: Some(false),
            bypass_rls: Some(true),
            valid_until: Some(None),
            member_of: Some(vec![]),
            ..RoleUpdateRequest::new(dev, "app")
        }]
    );
    // Unknown fields, the owner, reserved memberships: refused without the worker.
    let err = error_of(t.send(Method::PATCH, &path, Some(json!({ "superuser": true }))).await, 422).await;
    assert_eq!(code_of(&err), "invalid_request");
    let owner = format!("{}/postgres", t.roles(dev));
    let err = error_of(t.send(Method::PATCH, &owner, Some(json!({ "inherit": false }))).await, 422).await;
    assert_eq!(code_of(&err), "reserved_role_name");
    // Any `chelabase_` role is Chelabase's own, listed reserved or not.
    for name in ["chelabase_custom", "Chelabase_x", "cb_m_x", "CB_M_X"] {
        let target = format!("{}/{name}", t.roles(dev));
        let err = error_of(t.send(Method::PATCH, &target, Some(json!({ "inherit": false }))).await, 422).await;
        assert_eq!(code_of(&err), "reserved_role_name", "{name}");
    }
    let err = error_of(t.send(Method::PATCH, &path, Some(json!({ "member_of": ["anon"] }))).await, 422).await;
    assert_eq!(code_of(&err), "role_membership_not_allowed");
    assert_eq!(t.jobs.role_calls.lock().unwrap().len(), 1);
    // The worker's answers map like the other role routes'.
    *t.jobs.role_reply.lock().unwrap() = Some(Ok(role_reply("not_found")));
    error_of(t.send(Method::PATCH, &path, Some(json!({ "inherit": false }))).await, 404).await;
    *t.jobs.role_reply.lock().unwrap() = Some(Ok(role_reply("not_applied")));
    let err = error_of(t.send(Method::PATCH, &path, Some(json!({ "inherit": false }))).await, 503).await;
    assert_eq!(code_of(&err), "not_applied");
    // T14 I2: turning LOGIN on at the branch's cap is refused like a create at the cap.
    *t.jobs.role_reply.lock().unwrap() = Some(Ok(role_reply("role_cap_reached")));
    let err = error_of(t.send(Method::PATCH, &path, Some(json!({ "login": true }))).await, 409).await;
    assert_eq!(code_of(&err), "role_cap_reached");
}

// P13: the list shows the attributes read-only, and never a password.
#[tokio::test]
async fn role_list_shows_attributes_without_password() {
    let t = branching().await;
    let dev = t.ready("dev");
    let ep = t.with_owner_row(dev);
    t.metadata.add_role_row(
        &ep,
        RoleRow {
            login: false,
            attributes: RoleAttributes {
                inherit: false,
                connection_limit: 5,
                valid_until: None,
                bypass_rls: true,
                member_of: vec!["a".into(), "b".into()],
                member_no_set: vec!["b".into()],
                member_admin: vec!["a".into()],
            },
            ..role_row("grp", RoleOrigin::Api, RoleLevel::Plain, PasswordState::None)
        },
    );
    let body: Value = t.send(Method::GET, &t.roles(dev), None).await.json().await.unwrap();
    let row = body.as_array().unwrap().iter().find(|r| r["name"] == "grp").unwrap().clone();
    assert_eq!(row["login"], false);
    assert_eq!(row["inherit"], false);
    assert_eq!(row["connection_limit"], 5);
    assert_eq!(row["valid_until"], Value::Null);
    assert_eq!(row["bypass_rls"], true);
    assert_eq!(row["member_of"], json!(["a", "b"]));
    // I2: the options of the memberships.
    assert_eq!(row["member_no_set"], json!(["b"]));
    assert_eq!(row["member_admin"], json!(["a"]));
    assert_eq!(row["password"], "none");
    let owner = body.as_array().unwrap().iter().find(|r| r["name"] == "postgres").unwrap();
    assert_eq!(owner["inherit"], true);
    assert_eq!(owner["connection_limit"], -1);
    let text = body.to_string();
    assert!(!text.contains("SCRAM") && !text.contains("v1:"), "{text}");
}

#[tokio::test]
async fn create_role_defaults_to_not_privileged() {
    let t = branching().await;
    let dev = t.ready("dev");
    let ep = t.with_owner_row(dev);
    t.metadata.add_role(&ep, "app", RoleOrigin::Api, false, false);
    let resp = t.send(Method::POST, &t.roles(dev), Some(json!({ "name": "app" }))).await;
    assert_eq!(resp.status(), 201);
    assert_eq!(*t.jobs.role_calls.lock().unwrap(), vec!["create:app:false".to_string()]);
}

#[tokio::test]
async fn create_role_bad_name_is_422_without_rpc() {
    let t = branching().await;
    let dev = t.ready("dev");
    for (name, code) in [
        ("Bad-Name", "invalid_role_name"),
        ("", "invalid_role_name"),
        ("postgres", "reserved_role_name"),
        ("pg_x", "reserved_role_name"),
        ("chelabase_x", "reserved_role_name"),
        ("Chelabase_X", "reserved_role_name"),
        ("cb_m_x", "reserved_role_name"),
        ("CB_M_X", "reserved_role_name"),
    ] {
        let err = error_of(t.send(Method::POST, &t.roles(dev), Some(json!({ "name": name }))).await, 422).await;
        assert_eq!(code_of(&err), code, "{name}");
    }
    assert!(t.jobs.role_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn create_role_unknown_field_is_422() {
    let t = branching().await;
    let dev = t.ready("dev");
    let err = error_of(t.send(Method::POST, &t.roles(dev), Some(json!({ "name": "app", "password": "x" }))).await, 422).await;
    assert_eq!(code_of(&err), "invalid_request");
    let resp = reqwest::Client::new()
        .post(format!("{}{}", t.app.base, t.roles(dev)))
        .header(IDENTITY_HEADER, t.app.token_for_org("org-1"))
        .header("content-type", "application/json")
        .body("{nope")
        .send()
        .await
        .unwrap();
    assert_eq!(code_of(&error_of(resp, 400).await), "invalid_request");
    assert!(t.jobs.role_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn role_routes_refuse_backups_unready_and_unknown_branches() {
    let t = branching().await;
    let dev = t.ready("dev");
    let backup = t.backup(dev);
    let pending = t.add("p", t.main(), BranchKind::Normal, BranchStatus::Pending);
    for (method, suffix, body) in [
        (Method::GET, "", None),
        (Method::POST, "", Some(json!({ "name": "app" }))),
        (Method::POST, "/app/reset-password", None),
        (Method::DELETE, "/app", None),
    ] {
        let path = |id| format!("{}{suffix}", t.roles(id));
        let err = error_of(t.send(method.clone(), &path(backup), body.clone()).await, 409).await;
        assert_eq!(code_of(&err), "backup_branch", "{suffix}");
        let err = error_of(t.send(method.clone(), &path(pending), body.clone()).await, 409).await;
        assert_eq!(code_of(&err), "branch_not_ready", "{suffix}");
        assert_eq!(t.send(method.clone(), &path(BranchId::new()), body.clone()).await.status(), 404, "{suffix}");
    }
    assert!(t.jobs.role_calls.lock().unwrap().is_empty());
    assert!(t.jobs.resets.lock().unwrap().is_empty());
}

#[tokio::test]
async fn role_reply_codes_map_to_statuses() {
    let t = branching().await;
    let dev = t.ready("dev");
    let ep = t.with_owner_row(dev);
    t.metadata.add_role(&ep, "app", RoleOrigin::Api, false, false);
    let key = uuid::Uuid::new_v4();
    let in_use = RoleReply {
        keys: vec![chelabase_nats::KeyRef { id: key, name: "ci".into() }],
        ..role_reply("role_in_use")
    };
    let table: Vec<(RoleReply, u16, &str)> = vec![
        (role_reply("invalid_role_name"), 422, "invalid_role_name"),
        (role_reply("reserved_role_name"), 422, "reserved_role_name"),
        (role_reply("role_exists"), 409, "role_exists"),
        (role_reply("role_cap_reached"), 409, "role_cap_reached"),
        (role_reply("branch_not_ready"), 409, "branch_not_ready"),
        (in_use, 409, "role_in_use"),
        (role_reply("not_found"), 404, "not_found"),
        (role_reply("role_privilege_mismatch"), 500, "role_privilege_mismatch"),
        (role_reply("failed"), 500, "internal"),
        (role_reply("something_new"), 500, "internal"),
        (role_reply("not_applied"), 503, "not_applied"),
    ];
    let requests = [
        (Method::POST, t.roles(dev), Some(json!({ "name": "app" }))),
        (Method::POST, format!("{}/app/reset-password", t.roles(dev)), None),
        (Method::DELETE, format!("{}/app", t.roles(dev)), None),
    ];
    for (reply, status, code) in table {
        *t.jobs.role_reply.lock().unwrap() = Some(Ok(reply.clone()));
        for (method, path, body) in &requests {
            let err = error_of(t.send(method.clone(), path, body.clone()).await, status).await;
            assert_eq!(code_of(&err), code, "{method} {path} {reply:?}");
            let text = err.to_string();
            assert!(!text.contains("worker said so"), "the worker's message leaked: {text}");
            if reply.code.as_deref() == Some("role_in_use") {
                assert_eq!(err["error"]["keys"], json!([{ "id": key, "name": "ci" }]));
            } else {
                assert!(err["error"].get("keys").is_none());
            }
            if code == "not_applied" {
                let expected = match *method {
                    Method::POST if path.ends_with("/reset-password") => RESET_NOT_APPLIED,
                    Method::POST => "the role was stored but not yet applied; it will be retried",
                    _ => "the role was deleted but the change is not yet applied; it will be retried",
                };
                assert_eq!(message_of(&err), expected, "{method} {path}");
            }
        }
    }
    *t.jobs.role_reply.lock().unwrap() = Some(Err(ResetError::Timeout));
    let err = error_of(t.send(Method::DELETE, &format!("{}/app", t.roles(dev)), None).await, 504).await;
    assert_eq!(code_of(&err), "timeout");
    *t.jobs.role_reply.lock().unwrap() = Some(Err(ResetError::Transport("no worker listening".into())));
    let err = error_of(t.send(Method::DELETE, &format!("{}/app", t.roles(dev)), None).await, 503).await;
    assert_eq!(code_of(&err), "worker_unavailable");
}

#[tokio::test]
async fn reset_owner_alias_uses_owner_reset_and_is_204() {
    let t = branching().await;
    let dev = t.ready("dev");
    let resp = t.send(Method::POST, &format!("{}/postgres/reset-password", t.roles(dev)), None).await;
    assert_eq!(resp.status(), 204);
    assert_eq!(*t.jobs.resets.lock().unwrap(), vec![(t.db, dev)]);
    assert!(t.jobs.role_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reset_login_role_is_204() {
    let t = branching().await;
    let dev = t.ready("dev");
    let resp = t.send(Method::POST, &format!("{}/app/reset-password", t.roles(dev)), None).await;
    assert_eq!(resp.status(), 204);
    assert!(resp.text().await.unwrap().is_empty());
    assert_eq!(*t.jobs.role_calls.lock().unwrap(), vec!["reset:app".to_string()]);
    assert!(t.jobs.resets.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reset_and_delete_accept_sql_names() {
    let t = branching().await;
    let dev = t.ready("dev");
    // SQL can make `"My Role"`: the percent-encoded name reaches the worker decoded.
    let resp = t.send(Method::POST, &format!("{}/My%20Role/reset-password", t.roles(dev)), None).await;
    assert_eq!(resp.status(), 204);
    let resp = t.send(Method::DELETE, &format!("{}/Bad-Name", t.roles(dev)), None).await;
    assert_eq!(resp.status(), 200, "T10: a delete answers what it reassigned");
    assert_eq!(
        *t.jobs.role_calls.lock().unwrap(),
        vec!["reset:My Role".to_string(), "delete:Bad-Name".to_string()]
    );
    // A NUL byte, and a name over 63 bytes, never reach the worker.
    for path in [format!("{}/a%00b/reset-password", t.roles(dev)), format!("{}/{}/reset-password", t.roles(dev), "x".repeat(64))] {
        let err = error_of(t.send(Method::POST, &path, None).await, 422).await;
        assert_eq!(code_of(&err), "invalid_role_name", "{path}");
    }
    let err = error_of(t.send(Method::DELETE, &format!("{}/a%00b", t.roles(dev)), None).await, 422).await;
    assert_eq!(code_of(&err), "invalid_role_name");
    assert_eq!(t.jobs.role_calls.lock().unwrap().len(), 2);
}

// What Studio sends for a role named `a/b%61?#`: one segment, decoded once (`%2561` stays
// `%61`, not `a`).
#[tokio::test]
async fn role_path_is_decoded_once() {
    let t = branching().await;
    let dev = t.ready("dev");
    let segment = "a%2Fb%2561%3F%23";
    let resp = t.send(Method::POST, &format!("{}/{segment}/reset-password", t.roles(dev)), None).await;
    assert_eq!(resp.status(), 204);
    let resp = t.send(Method::DELETE, &format!("{}/{segment}", t.roles(dev)), None).await;
    assert_eq!(resp.status(), 200, "T10: a delete answers what it reassigned");
    assert_eq!(
        *t.jobs.role_calls.lock().unwrap(),
        vec!["reset:a/b%61?#".to_string(), "delete:a/b%61?#".to_string()]
    );
}

// T10: a delete answers 200 with what it reassigned (was 204).
#[tokio::test]
async fn delete_role_200() {
    let t = branching().await;
    let dev = t.ready("dev");
    let resp = t.send(Method::DELETE, &format!("{}/app", t.roles(dev)), None).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(*t.jobs.role_calls.lock().unwrap(), vec!["delete:app".to_string()]);
}

// ---- role sessions (E29a)

#[tokio::test]
async fn role_sessions_lists_the_live_sessions() {
    let t = branching().await;
    let dev = t.ready("dev");
    *t.jobs.sessions_reply.lock().unwrap() = Some(Ok(RoleSessionsReply::running(vec![
        RoleSession {
            client_addr: Some("10.0.0.7".into()),
            application_name: "api".into(),
            backend_start: "2026-10-08T10:00:00Z".parse().unwrap(),
        },
        RoleSession { client_addr: None, application_name: String::new(), backend_start: "2026-10-08T11:00:00Z".parse().unwrap() },
    ])));
    let resp = t.send(Method::GET, &format!("{}/old_app/sessions", t.roles(dev)), None).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body,
        json!({
            "compute_running": true,
            "count": 2,
            "sessions": [
                { "client_addr": "10.0.0.7", "application_name": "api", "backend_start": "2026-10-08T10:00:00Z" },
                { "client_addr": null, "application_name": "", "backend_start": "2026-10-08T11:00:00Z" },
            ],
        })
    );
    assert_eq!(*t.jobs.session_reads.lock().unwrap(), vec![(dev, "old_app".to_string())]);
}

#[tokio::test]
async fn role_sessions_without_a_running_compute_is_not_an_error() {
    let t = branching().await;
    let dev = t.ready("dev");
    *t.jobs.sessions_reply.lock().unwrap() = Some(Ok(RoleSessionsReply::not_running()));
    let resp = t.send(Method::GET, &format!("{}/app/sessions", t.roles(dev)), None).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body, json!({ "compute_running": false, "count": 0, "sessions": [] }));
}

#[tokio::test]
async fn role_sessions_errors() {
    let t = branching().await;
    let dev = t.ready("dev");
    let path = format!("{}/app/sessions", t.roles(dev));
    let cases: [(Result<RoleSessionsReply, ResetError>, u16, &str); 4] = [
        (Ok(RoleSessionsReply::not_ready()), 409, "branch_not_ready"),
        (Ok(RoleSessionsReply::failed("boom")), 500, "internal"),
        (Err(ResetError::Timeout), 504, "timeout"),
        (Err(ResetError::Transport("no worker listening".into())), 503, "worker_unavailable"),
    ];
    for (answer, status, code) in cases {
        *t.jobs.sessions_reply.lock().unwrap() = Some(answer);
        let err = error_of(t.send(Method::GET, &path, None).await, status).await;
        assert_eq!(code_of(&err), code);
        assert!(!message_of(&err).contains("boom"), "worker details stay in the logs");
    }
    // Refused before the worker is asked: a name Postgres can't hold, a backup, an unready branch.
    let reads = t.jobs.session_reads.lock().unwrap().len();
    let long = "x".repeat(64);
    let err = error_of(t.send(Method::GET, &format!("{}/{long}/sessions", t.roles(dev)), None).await, 422).await;
    assert_eq!(code_of(&err), "invalid_role_name");
    let backup = t.backup(dev);
    let err = error_of(t.send(Method::GET, &format!("{}/app/sessions", t.roles(backup)), None).await, 409).await;
    assert_eq!(code_of(&err), "backup_branch");
    let wip = t.add("wip", dev, BranchKind::Normal, BranchStatus::Pending);
    let err = error_of(t.send(Method::GET, &format!("{}/app/sessions", t.roles(wip)), None).await, 409).await;
    assert_eq!(code_of(&err), "branch_not_ready");
    let err = error_of(
        t.send(Method::GET, &format!("{}/app/sessions", t.roles(BranchId::new())), None).await,
        404,
    )
    .await;
    assert_eq!(code_of(&err), "not_found");
    assert_eq!(t.jobs.session_reads.lock().unwrap().len(), reads);
}

#[tokio::test]
async fn role_sessions_of_another_tenant_is_404() {
    let t = branching().await;
    let dev = t.ready("dev");
    t.metadata.bind_org("org-2", TenantId::new());
    let resp = reqwest::Client::new()
        .get(format!("{}{}/app/sessions", t.app.base, t.roles(dev)))
        .header(IDENTITY_HEADER, t.app.token_for_org("org-2"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert!(t.jobs.session_reads.lock().unwrap().is_empty());
}

#[tokio::test]
async fn delete_owner_is_422() {
    let t = branching().await;
    let dev = t.ready("dev");
    let err = error_of(t.send(Method::DELETE, &format!("{}/postgres", t.roles(dev)), None).await, 422).await;
    assert_eq!(code_of(&err), "reserved_role_name");
    assert!(t.jobs.role_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn connection_by_role_and_database() {
    let t = branching().await;
    let dev = t.ready("dev");
    let sealed = seal_credential(&t.app.key, &dev.to_string(), Purpose::Owner, "dev-pw");
    t.metadata.branches.lock().unwrap().get_mut(&dev).unwrap().owner_password_enc = Some(sealed);
    let ep = t.metadata.branch(dev).endpoint_id.unwrap();
    t.metadata.role_passwords.lock().unwrap().insert(
        (ep.clone(), "app".into()),
        chelabase_secrets::seal_role_password(&t.app.key, &dev.to_string(), "app", "app-pw"),
    );
    t.metadata.add_role(&ep, "postgres", RoleOrigin::Owner, true, false);
    t.metadata.add_role(&ep, "app", RoleOrigin::Api, false, false);
    t.metadata.add_role_row(&ep, role_row("etl", RoleOrigin::Sql, RoleLevel::Plain, PasswordState::SetInSql));
    t.metadata.add_role_row(&ep, role_row("old", RoleOrigin::Sql, RoleLevel::Plain, PasswordState::Md5Unusable));
    t.metadata.add_role_row(&ep, role_row("My Role@x", RoleOrigin::Sql, RoleLevel::Plain, PasswordState::SetInSql));
    t.metadata.add_branch_db(dev, "web1", "postgres", DatabaseOrigin::Main);
    t.metadata.add_branch_db(dev, "analytics", "etl", DatabaseOrigin::Sql);
    t.metadata.add_branch_db(dev, "postgres", "cloud_admin", DatabaseOrigin::System);
    let conn = format!("{}/connection", t.branch(dev));

    // Absent, or the owner by name: the owner's credentials.
    for path in [conn.clone(), format!("{conn}?role=postgres"), format!("{conn}?other=1")] {
        let resp = t.send(Method::GET, &path, None).await;
        assert_eq!(resp.status(), 200, "{path}");
        assert_eq!(resp.headers()["cache-control"], "no-store");
        let body: Value = resp.json().await.unwrap();
        assert_eq!((body["user"].as_str(), body["password"].as_str()), (Some("postgres"), Some("dev-pw")), "{path}");
        assert_eq!(body["password_state"], "generated");
    }

    // A role made through the API: its own user and password.
    let resp = t.send(Method::GET, &format!("{conn}?role=app"), None).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["user"], "app");
    assert_eq!(body["password"], "app-pw");
    assert_eq!(body["password_state"], "generated");
    assert_eq!(body["database"], "web1");
    assert_eq!(body["host"], format!("{ep}.localtest.me"));
    assert_eq!(body["uri"], format!("postgresql://app:app-pw@{ep}.localtest.me:4432/web1?sslmode=verify-full"));

    // A role set in SQL: no password, a uri without one.
    let resp = t.send(Method::GET, &format!("{conn}?role=etl&database=analytics"), None).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let body: Value = resp.json().await.unwrap();
    assert!(body["password"].is_null(), "{body}");
    assert_eq!(body["password_state"], "set_in_sql");
    assert_eq!(body["database"], "analytics");
    assert_eq!(body["uri"], format!("postgresql://etl@{ep}.localtest.me:4432/analytics?sslmode=verify-full"));

    // An MD5 role.
    let body: Value = t.send(Method::GET, &format!("{conn}?role=old"), None).await.json().await.unwrap();
    assert!(body["password"].is_null());
    assert_eq!(body["password_state"], "md5_unusable");

    // The system database is pickable; the user and database are percent-encoded in the uri.
    let body: Value = t.send(Method::GET, &format!("{conn}?role=My%20Role%40x&database=postgres"), None).await.json().await.unwrap();
    assert_eq!(body["user"], "My Role@x");
    assert_eq!(body["database"], "postgres");
    assert_eq!(body["uri"], format!("postgresql://My%20Role%40x@{ep}.localtest.me:4432/postgres?sslmode=verify-full"));

    // Unknown database, unknown role, NUL: 404, 404, 422.
    let err = error_of(t.send(Method::GET, &format!("{conn}?database=ghost"), None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    let err = error_of(t.send(Method::GET, &format!("{conn}?role=ghost"), None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    let err = error_of(t.send(Method::GET, &format!("{conn}?role=a%00b"), None).await, 422).await;
    assert_eq!(code_of(&err), "invalid_role_name");
    // Any Postgres name is a lookup now, so a platform role is just not there.
    let err = error_of(t.send(Method::GET, &format!("{conn}?role=anon"), None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");

    // A backup is refused whatever the role.
    let backup = t.backup(dev);
    let err = error_of(t.send(Method::GET, &format!("{}/connection?role=app", t.branch(backup)), None).await, 409).await;
    assert_eq!(code_of(&err), "backup_branch");
}

#[tokio::test]
async fn owner_set_in_sql_reveals_no_password() {
    let t = branching().await;
    let dev = t.ready("dev"); // no sealed owner password
    t.metadata.add_branch_db(dev, "web1", "postgres", DatabaseOrigin::Main);
    let ep = t.metadata.branch(dev).endpoint_id.unwrap();
    let resp = t.send(Method::GET, &format!("{}/connection", t.branch(dev)), None).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["user"], "postgres");
    assert!(body["password"].is_null(), "{body}");
    assert_eq!(body["password_state"], "set_in_sql");
    assert_eq!(body["uri"], format!("postgresql://postgres@{ep}.localtest.me:4432/web1?sslmode=verify-full"));
}

#[tokio::test]
async fn list_branch_databases() {
    let t = branching().await;
    let dev = t.ready("dev");
    t.metadata.add_branch_db(dev, "postgres", "cloud_admin", DatabaseOrigin::System);
    t.metadata.add_branch_db(dev, "zeta", "etl", DatabaseOrigin::Sql);
    t.metadata.add_branch_db(dev, "web1", "postgres", DatabaseOrigin::Main);
    t.metadata.add_branch_db(dev, "alpha", "etl", DatabaseOrigin::Sql);
    let resp = t.send(Method::GET, &format!("{}/databases", t.branch(dev)), None).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body,
        json!([
            { "name": "web1", "owner": "postgres", "origin": "main", "main": true, "data_api": "off", "data_api_error": null, "default_schema": "public" },
            { "name": "alpha", "owner": "etl", "origin": "sql", "main": false, "data_api": "off", "data_api_error": null, "default_schema": "public" },
            { "name": "postgres", "owner": "cloud_admin", "origin": "system", "main": false, "data_api": "off", "data_api_error": null, "default_schema": "public" },
            { "name": "zeta", "owner": "etl", "origin": "sql", "main": false, "data_api": "off", "data_api_error": null, "default_schema": "public" },
        ])
    );
    let backup = t.backup(dev);
    let err = error_of(t.send(Method::GET, &format!("{}/databases", t.branch(backup)), None).await, 409).await;
    assert_eq!(code_of(&err), "backup_branch");
    assert_eq!(t.send(Method::GET, &format!("{}/databases", t.branch(BranchId::new())), None).await.status(), 404);
}

#[tokio::test]
async fn main_database_missing_is_reported() {
    let t = branching().await;
    let dev = t.ready("dev");
    // `dev` has a database row but no `main` one: the branch says so; the reveal still names
    // the recorded main database (R24).
    t.metadata.add_branch_db(dev, "other", "etl", DatabaseOrigin::Sql);
    let list: Vec<Value> = t.send(Method::GET, &t.branches(), None).await.json().await.unwrap();
    let by = |id: BranchId| list.iter().find(|b| b["id"] == id.as_uuid().to_string()).unwrap().clone();
    assert_eq!(by(dev)["main_database_missing"], true);
    let body: Value = t.send(Method::GET, &format!("{}/connection", t.branch(dev)), None).await.json().await.unwrap();
    assert_eq!(body["database"], "web1");
    // With the main row present, it isn't missing.
    t.metadata.add_branch_db(dev, "web1", "postgres", DatabaseOrigin::Main);
    let list: Vec<Value> = t.send(Method::GET, &t.branches(), None).await.json().await.unwrap();
    let dev_row = list.iter().find(|b| b["id"] == dev.as_uuid().to_string()).unwrap();
    assert_eq!(dev_row["main_database_missing"], false);
}

// C9: the list asks for the main databases of all its ready branches in one query.
#[tokio::test]
async fn list_branches_reads_databases_once() {
    let t = branching().await;
    let a = t.ready("a");
    let b = t.ready("b");
    t.ready("c");
    t.metadata.add_branch_db(a, "web1", "postgres", DatabaseOrigin::Main);
    let list: Vec<Value> = t.send(Method::GET, &t.branches(), None).await.json().await.unwrap();
    let missing = |id: BranchId| list.iter().find(|x| x["id"] == id.as_uuid().to_string()).unwrap()["main_database_missing"].clone();
    assert_eq!(missing(a), false);
    assert_eq!(missing(b), true);
    assert_eq!(t.metadata.main_db_batch_queries.load(Ordering::SeqCst), 1);
    assert_eq!(t.metadata.branch_db_queries.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn update_branch_protected() {
    let t = branching().await;
    let dev = t.ready("dev");
    // Protecting runs the review scan: the fake has no compute (P4), so 503 and no change.
    let err = error_of(t.send(Method::PATCH, &t.branch(dev), Some(json!({ "protected": true }))).await, 503).await;
    assert_eq!(code_of(&err), "unavailable");
    assert!(!t.metadata.branch(dev).protected);
    // Unprotecting needs no compute.
    t.metadata.branches.lock().unwrap().get_mut(&dev).unwrap().protected = true;
    let body: Value = t.send(Method::PATCH, &t.branch(dev), Some(json!({ "protected": false }))).await.json().await.unwrap();
    assert_eq!(body["id"], dev.as_uuid().to_string());
    assert_eq!(body["protected"], false);
    assert_eq!(body["guard_logins"], true);
    assert!(body["review_acknowledged_hash"].is_null());
    // A backup: 409; unknown: 404; a deleting branch: 409 branch_not_ready (before any scan).
    let backup = t.backup(dev);
    let err = error_of(t.send(Method::PATCH, &t.branch(backup), Some(json!({ "protected": true }))).await, 409).await;
    assert_eq!(code_of(&err), "backup_branch");
    assert_eq!(t.send(Method::PATCH, &t.branch(BranchId::new()), Some(json!({ "protected": true }))).await.status(), 404);
    let deleting = t.add("gone", t.main(), BranchKind::Normal, BranchStatus::Deleting);
    let err = error_of(t.send(Method::PATCH, &t.branch(deleting), Some(json!({ "protected": true }))).await, 409).await;
    assert_eq!(code_of(&err), "branch_not_ready");
    // Unknown or missing field, a hash without protect: 422 (P5).
    for body in [
        json!({ "protected": true, "name": "x" }),
        json!({}),
        json!({ "protected": "yes" }),
        json!({ "review_hash": "abc" }),
        json!({ "protected": false, "review_hash": "abc" }),
        json!({ "guard_logins": "on" }),
    ] {
        let err = error_of(t.send(Method::PATCH, &t.branch(dev), Some(body.clone())).await, 422).await;
        assert_eq!(code_of(&err), "invalid_request", "{body}");
    }
    // Malformed JSON: 400.
    let resp = reqwest::Client::new()
        .patch(format!("{}{}", t.app.base, t.branch(dev)))
        .header(IDENTITY_HEADER, t.app.token_for_org("org-1"))
        .header("content-type", "application/json")
        .body("{\"protected\": tru")
        .send()
        .await
        .unwrap();
    let err = error_of(resp, 400).await;
    assert_eq!(code_of(&err), "invalid_request");
    assert!(!t.metadata.branch(dev).protected);
    // Needs the management credential.
    let resp = reqwest::Client::new()
        .patch(format!("{}{}", t.app.base, t.branch(dev)))
        .json(&json!({ "protected": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn protected_branch_refuses_delete_and_reset_but_not_restore() {
    let t = branching().await;
    let dev = t.ready("dev");
    t.metadata.branches.lock().unwrap().get_mut(&dev).unwrap().protected = true;
    let expect = |err: &Value| {
        assert_eq!(code_of(err), "branch_protected");
        assert_eq!(message_of(err), "this branch is protected; unprotect it first");
    };
    expect(&error_of(t.send(Method::DELETE, &t.branch(dev), None).await, 409).await);
    expect(&error_of(t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await, 409).await);
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Ready);
    assert!(t.published().is_empty());
    // A non-ready (failed) protected branch refuses a delete as well.
    let failed = t.add("bad", t.main(), BranchKind::Normal, BranchStatus::Failed);
    t.metadata.branches.lock().unwrap().get_mut(&failed).unwrap().protected = true;
    expect(&error_of(t.send(Method::DELETE, &t.branch(failed), None).await, 409).await);
    assert_eq!(t.metadata.branch(failed).status, BranchStatus::Failed);
    // Restore rewinds a protected branch (its previous state is kept as a backup).
    let resp = t
        .send(Method::POST, &format!("{}/restore", t.branch(dev)), Some(json!({ "at": { "timestamp": "2026-01-01T00:00:00Z" } })))
        .await;
    assert_eq!(resp.status(), 202);
    // Unprotected, the delete goes through.
    let other = t.ready("other");
    assert_eq!(t.send(Method::DELETE, &t.branch(other), None).await.status(), 202);
}

// T6: a protect that lands after the API's check is still refused by the delete itself; a
// claimed (ready) branch is released.
#[tokio::test]
async fn delete_refused_when_protected_after_the_check() {
    let t = branching().await;
    let dev = t.ready("dev");
    let failed = t.add("bad", t.main(), BranchKind::Normal, BranchStatus::Failed);
    for (branch, status) in [(dev, BranchStatus::Ready), (failed, BranchStatus::Failed)] {
        *t.metadata.protect_on_delete.lock().unwrap() = Some(branch);
        let err = error_of(t.send(Method::DELETE, &t.branch(branch), None).await, 409).await;
        assert_eq!(code_of(&err), "branch_protected", "{status:?}");
        assert_eq!(t.metadata.branch(branch).status, status);
    }
    assert!(t.published().is_empty());
}

#[tokio::test]
async fn database_delete_refused_with_a_protected_branch() {
    let t = branching().await;
    let dev = t.ready("dev");
    t.metadata.branches.lock().unwrap().get_mut(&dev).unwrap().protected = true;
    let path = format!("/v1/databases/{}", t.db.as_uuid());
    let err = error_of(t.send(Method::DELETE, &path, None).await, 409).await;
    assert_eq!(code_of(&err), "branch_protected");
    assert_eq!(message_of(&err), "a branch of this database is protected; unprotect it first");
    assert!(t.jobs.deleted.lock().unwrap().is_empty());
    assert_eq!(t.metadata.database(t.db).status, DatabaseStatus::Ready);
    t.metadata.branches.lock().unwrap().get_mut(&dev).unwrap().protected = false;
    assert_eq!(t.send(Method::DELETE, &path, None).await.status(), 202);
}

#[tokio::test]
async fn branch_responses_carry_protected() {
    let t = branching().await;
    let dev = t.ready("dev");
    t.metadata.branches.lock().unwrap().get_mut(&dev).unwrap().protected = true;
    let list: Vec<Value> = t.send(Method::GET, &t.branches(), None).await.json().await.unwrap();
    let flag = |id: BranchId| list.iter().find(|b| b["id"] == id.as_uuid().to_string()).unwrap()["protected"].clone();
    assert_eq!(flag(dev), true);
    assert_eq!(flag(t.main()), false);
    let resp = t.send(Method::POST, &t.branches(), Some(json!({ "name": "feat" }))).await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["protected"], false);
    // A branch that isn't ready yet has no databases to miss.
    assert_eq!(body["main_database_missing"], false);
}

#[tokio::test]
async fn connection_role_sealed_for_another_branch_is_500_credentials_unavailable() {
    let t = branching().await;
    let dev = t.ready("dev");
    let ep = t.metadata.branch(dev).endpoint_id.unwrap();
    t.metadata.add_role(&ep, "app", RoleOrigin::Api, false, false);
    t.metadata.role_passwords.lock().unwrap().insert(
        (ep, "app".into()),
        chelabase_secrets::seal_role_password(&t.app.key, &BranchId::new().to_string(), "app", "x"),
    );
    let err = error_of(t.send(Method::GET, &format!("{}/connection?role=app", t.branch(dev)), None).await, 500).await;
    assert_eq!(code_of(&err), "credentials_unavailable");
}

#[tokio::test]
async fn role_routes_need_management_auth() {
    let t = branching().await;
    let dev = t.ready("dev");
    for (method, suffix) in [
        (Method::GET, ""),
        (Method::POST, ""),
        (Method::POST, "/app/reset-password"),
        (Method::DELETE, "/app"),
    ] {
        let resp = reqwest::Client::new()
            .request(method.clone(), format!("{}{}{suffix}", t.app.base, t.roles(dev)))
            .json(&json!({ "name": "app" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "{method} {suffix}");
    }
    let resp = reqwest::Client::new()
        .get(format!("{}{}/connection?role=app", t.app.base, t.branch(dev)))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    assert!(t.jobs.role_calls.lock().unwrap().is_empty());
}


// ---- the data API per Postgres database (step 6c)

type Invalidations = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// A rest stand-in that records each invalidation: (branch, authorization).
async fn mock_rest_invalidate() -> (String, Invalidations) {
    use axum::extract::Path;
    use axum::http::HeaderMap;
    use axum::routing::post;
    let seen: Invalidations = Arc::default();
    let log = seen.clone();
    let app = Router::new().route(
        "/internal/branches/{branch}/invalidate",
        post(move |Path(branch): Path<String>, headers: HeaderMap| {
            let log = log.clone();
            async move {
                let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_string);
                log.lock().unwrap().push((branch, auth));
                axum::http::StatusCode::NO_CONTENT
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

/// Like `branching()`, with a rest that records invalidations and the given database cap.
async fn branching_with_rest(cap: u32) -> (Branching, Invalidations) {
    let (rest, seen) = mock_rest_invalidate().await;
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app_full(metadata.clone(), jobs.clone(), &rest, Mode::SelfHosted, cap).await;
    let db = ready_database(tenant, &app.key);
    let id = db.database_id;
    metadata.insert_database(db);
    (Branching { app, metadata, jobs, db: id }, seen)
}

impl Branching {
    fn pg(&self, branch: BranchId, name: &str) -> String {
        format!("{}/pg-databases/{name}", self.branch(branch))
    }

    /// The main branch with `web1` (main, on) and `analytics`, `scratch` (sql, off), and the
    /// system `postgres` database.
    fn seeded(&self) -> BranchId {
        let main = self.main();
        self.metadata.add_branch_db(main, "web1", "postgres", DatabaseOrigin::Main);
        self.metadata.set_data_api(main, "web1", chelabase_metadata::DataApiState::On);
        self.metadata.add_branch_db(main, "analytics", "etl", DatabaseOrigin::Sql);
        self.metadata.add_branch_db(main, "scratch", "etl", DatabaseOrigin::Sql);
        self.metadata.add_branch_db(main, "postgres", "cloud_admin", DatabaseOrigin::System);
        main
    }

    fn data_api_calls(&self) -> Vec<String> {
        self.jobs.data_api_calls.lock().unwrap().clone()
    }
}

impl FakeMetadata {
    fn set_data_api(&self, branch: BranchId, name: &str, state: chelabase_metadata::DataApiState) {
        let mut all = self.branch_dbs.lock().unwrap();
        let row = all.get_mut(&branch).unwrap().iter_mut().find(|d| d.name == name).unwrap();
        row.data_api = state;
        // As the store does: any state but off means an enable began.
        row.data_api_bootstrapped |= state != chelabase_metadata::DataApiState::Off;
    }

    fn pg_database(&self, branch: BranchId, name: &str) -> BranchDatabase {
        self.branch_dbs.lock().unwrap().get(&branch).unwrap().iter().find(|d| d.name == name).cloned().unwrap()
    }

    fn key_databases(&self, id: uuid::Uuid) -> Option<Vec<String>> {
        self.keys.lock().unwrap().iter().find(|(k, _)| k.id == id).unwrap().0.databases.clone()
    }
}

async fn wait_for_invalidations(seen: &Invalidations, n: usize) {
    for _ in 0..100 {
        if seen.lock().unwrap().len() >= n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn enable_answers_202_and_publishes() {
    let t = branching().await;
    let main = t.seeded();
    let resp = t.send(Method::POST, &format!("{}/data-api", t.pg(main, "analytics")), None).await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["name"], "analytics");
    assert_eq!(body["data_api"], "enabling");
    assert!(body["data_api_error"].is_null());
    assert_eq!(t.data_api_calls(), vec!["enable:analytics"]);
    assert_eq!(t.metadata.pg_database(main, "analytics").data_api, chelabase_metadata::DataApiState::Enabling);

    // Pressing it again while it is enabling starts the job again (a retry).
    assert_eq!(t.send(Method::POST, &format!("{}/data-api", t.pg(main, "analytics")), None).await.status(), 202);
    assert_eq!(t.data_api_calls().len(), 2);

    // Already served: 200, nothing published.
    let resp = t.send(Method::POST, &format!("{}/data-api", t.pg(main, "web1")), None).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<Value>().await.unwrap()["data_api"], "on");
    assert_eq!(t.data_api_calls().len(), 2);

    // A lost publish is healed by reconcile (the row stays `enabling`): still 202.
    *t.jobs.fail_enable_publish.lock().unwrap() = true;
    assert_eq!(t.send(Method::POST, &format!("{}/data-api", t.pg(main, "scratch")), None).await.status(), 202);
    assert_eq!(t.metadata.pg_database(main, "scratch").data_api, chelabase_metadata::DataApiState::Enabling);
}

#[tokio::test]
async fn enable_refusals() {
    let (t, seen) = branching_with_rest(2).await;
    let main = t.seeded();
    let enable = |name: &str| format!("{}/data-api", t.pg(main, name));
    // System: 422.
    let err = error_of(t.send(Method::POST, &enable("postgres"), None).await, 422).await;
    assert_eq!(code_of(&err), "invalid_request");
    // Unknown database and unknown branch: 404.
    let err = error_of(t.send(Method::POST, &enable("nope"), None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    assert_eq!(t.send(Method::POST, &format!("{}/data-api", t.pg(BranchId::new(), "x")), None).await.status(), 404);
    // Cap 2 with web1 on: one more fits, the next doesn't.
    assert_eq!(t.send(Method::POST, &enable("analytics"), None).await.status(), 202);
    let err = error_of(t.send(Method::POST, &enable("scratch"), None).await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    assert_eq!(message_of(&err), "this branch already serves 2 databases (the cap is 2)");
    assert_eq!(t.data_api_calls(), vec!["enable:analytics"]);
    assert_eq!(t.metadata.pg_database(main, "scratch").data_api, chelabase_metadata::DataApiState::Off);
    // A branch that isn't ready, and a backup.
    let pending = t.add("wip", main, BranchKind::Normal, BranchStatus::Pending);
    let err = error_of(t.send(Method::POST, &format!("{}/data-api", t.pg(pending, "x")), None).await, 409).await;
    assert_eq!(code_of(&err), "branch_not_ready");
    let backup = t.backup(main);
    let err = error_of(t.send(Method::POST, &format!("{}/data-api", t.pg(backup, "x")), None).await, 409).await;
    assert_eq!(code_of(&err), "backup_branch");
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn disable_main_is_refused() {
    let (t, seen) = branching_with_rest(10).await;
    let main = t.seeded();
    let err = error_of(t.send(Method::DELETE, &format!("{}/data-api", t.pg(main, "web1")), None).await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    assert!(t.data_api_calls().is_empty());
    assert_eq!(t.metadata.pg_database(main, "web1").data_api, chelabase_metadata::DataApiState::On);
    assert!(seen.lock().unwrap().is_empty());
    // Unknown: 404; the system database was never served: 422.
    assert_eq!(t.send(Method::DELETE, &format!("{}/data-api", t.pg(main, "nope")), None).await.status(), 404);
    assert_eq!(t.send(Method::DELETE, &format!("{}/data-api", t.pg(main, "postgres")), None).await.status(), 422);
    assert!(t.data_api_calls().is_empty());
}

#[tokio::test]
async fn disable_runs_the_job_then_invalidates_rest() {
    let (t, seen) = branching_with_rest(10).await;
    let main = t.seeded();
    t.metadata.set_data_api(main, "analytics", chelabase_metadata::DataApiState::On);
    let path = format!("{}/data-api", t.pg(main, "analytics"));
    assert_eq!(t.send(Method::DELETE, &path, None).await.status(), 204);
    assert_eq!(t.send(Method::DELETE, &format!("{path}?remove=true"), None).await.status(), 204);
    assert_eq!(t.data_api_calls(), vec!["disable:analytics:false", "disable:analytics:true"]);
    // The invalidation is awaited before the answer: it is already there.
    let expected = (main.as_uuid().to_string(), Some(format!("Bearer {REST_TOKEN}")));
    assert_eq!(*seen.lock().unwrap(), vec![expected.clone(), expected]);
    // A bad query is a 400, not a silent false.
    assert_eq!(t.send(Method::DELETE, &format!("{path}?remove=maybe"), None).await.status(), 400);

    // The worker's refusals map to statuses and do not invalidate.
    for (reply, status, code) in [
        (Ok(CheckDataRoleReply::not_ready()), 409, "branch_not_ready"),
        (Ok(CheckDataRoleReply::refused("database is in use")), 409, "conflict"),
        (Ok(CheckDataRoleReply::failed("boom")), 502, "unavailable"),
        (Err(ResetError::Timeout), 504, "timeout"),
        (Err(ResetError::Transport("no worker listening".into())), 503, "worker_unavailable"),
    ] {
        *t.jobs.data_api_reply.lock().unwrap() = Some(reply);
        let err = error_of(t.send(Method::DELETE, &path, None).await, status).await;
        assert_eq!(code_of(&err), code);
        assert!(!err.to_string().contains("boom"), "{err}");
    }
    assert_eq!(seen.lock().unwrap().len(), 2);
    // The refused text is the worker's.
    *t.jobs.data_api_reply.lock().unwrap() = Some(Ok(CheckDataRoleReply::refused("database is in use")));
    let err = error_of(t.send(Method::DELETE, &path, None).await, 409).await;
    assert_eq!(message_of(&err), "database is in use");
}

/// `remove` on a database no enable ever began on is refused: whatever `chelabase` schema it
/// holds isn't the data API's. Disabling it (already off) without `remove` still answers 204.
#[tokio::test]
async fn remove_on_a_database_never_enabled_is_refused() {
    let (t, seen) = branching_with_rest(10).await;
    let main = t.seeded();
    let path = format!("{}/data-api", t.pg(main, "scratch"));
    let err = error_of(t.send(Method::DELETE, &format!("{path}?remove=true"), None).await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    assert_eq!(
        message_of(&err),
        "the data API was never enabled on the database \"scratch\"; there is nothing to remove"
    );
    assert!(t.data_api_calls().is_empty());
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(t.send(Method::DELETE, &path, None).await.status(), 204);
    // Off after an enable began: the machinery may be there, so it can be removed.
    t.metadata.set_data_api(main, "scratch", chelabase_metadata::DataApiState::On);
    t.metadata.set_data_api(main, "scratch", chelabase_metadata::DataApiState::Off);
    assert_eq!(t.send(Method::DELETE, &format!("{path}?remove=true"), None).await.status(), 204);
    assert_eq!(t.data_api_calls(), vec!["disable:scratch:false", "disable:scratch:true"]);
}

#[tokio::test]
async fn disable_survives_rest_being_down() {
    let t = branching().await; // rest points at a closed port
    let main = t.seeded();
    t.metadata.set_data_api(main, "analytics", chelabase_metadata::DataApiState::On);
    let path = format!("{}/data-api", t.pg(main, "analytics"));
    assert_eq!(t.send(Method::DELETE, &path, None).await.status(), 204);
}

#[tokio::test]
async fn default_schema_patch_validates_and_invalidates() {
    let (t, seen) = branching_with_rest(10).await;
    let main = t.seeded();
    let path = t.pg(main, "web1");
    for bad in [json!({ "default_schema": "" }), json!({ "default_schema": "Public" }), json!({ "default_schema": "1app" }),
        json!({ "default_schema": "a-b" }), json!({ "default_schema": "a".repeat(64) }), json!({ "default_schema": 5 }),
        json!({}), json!({ "default_schema": "app", "extra": 1 })] {
        assert_eq!(t.send(Method::PATCH, &path, Some(bad.clone())).await.status(), 422, "{bad}");
    }
    assert_eq!(t.metadata.pg_database(main, "web1").default_schema, "public");
    assert!(t.data_api_calls().is_empty());

    // A served database: stored, bootstrap re-run, rest told.
    let resp = t.send(Method::PATCH, &path, Some(json!({ "default_schema": "app" }))).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!((body["name"].as_str(), body["default_schema"].as_str()), (Some("web1"), Some("app")));
    assert_eq!(t.metadata.pg_database(main, "web1").default_schema, "app");
    assert_eq!(t.data_api_calls(), vec!["rebootstrap:web1"]);
    wait_for_invalidations(&seen, 1).await;
    assert_eq!(seen.lock().unwrap().len(), 1);

    // The same value again changes nothing: no job, no invalidation.
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "default_schema": "app" }))).await.status(), 200);
    assert_eq!(t.data_api_calls().len(), 1);
    assert_eq!(seen.lock().unwrap().len(), 1);

    // A database that isn't served just stores it.
    let resp = t.send(Method::PATCH, &t.pg(main, "analytics"), Some(json!({ "default_schema": "reports" }))).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(t.metadata.pg_database(main, "analytics").default_schema, "reports");
    assert_eq!(t.data_api_calls().len(), 1);
    assert_eq!(seen.lock().unwrap().len(), 1);

    // Unknown database 404; the system database 422 (as enabling it); a failed bootstrap is
    // reported.
    assert_eq!(t.send(Method::PATCH, &t.pg(main, "nope"), Some(json!({ "default_schema": "app" }))).await.status(), 404);
    let err = error_of(t.send(Method::PATCH, &t.pg(main, "postgres"), Some(json!({ "default_schema": "app" }))).await, 422).await;
    assert_eq!(message_of(&err), "the system database \"postgres\" is never served");
    assert_eq!(t.metadata.pg_database(main, "postgres").default_schema, "public");
    *t.jobs.data_api_reply.lock().unwrap() = Some(Ok(CheckDataRoleReply::failed("boom")));
    let err = error_of(t.send(Method::PATCH, &path, Some(json!({ "default_schema": "other" }))).await, 502).await;
    assert_eq!(code_of(&err), "unavailable");
    // The old schema is put back: what is stored is what the bootstrap covers. rest may
    // have read the new one meanwhile, so it's told again.
    assert_eq!(t.metadata.pg_database(main, "web1").default_schema, "app");
    wait_for_invalidations(&seen, 2).await;
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn patch_default_schema_passes_grant_existing() {
    let t = branching().await;
    let main = t.seeded();
    let path = t.pg(main, "web1");
    // Off by default.
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "default_schema": "app" }))).await.status(), 200);
    assert_eq!(t.data_api_calls(), vec!["rebootstrap:web1"]);
    // Asked: the flag reaches the worker, with a changed schema...
    let resp = t.send(Method::PATCH, &path, Some(json!({ "default_schema": "app2", "grant_existing": true }))).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(t.metadata.pg_database(main, "web1").default_schema, "app2");
    // ...and with an unchanged one (so a user can grant later).
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "default_schema": "app2", "grant_existing": true }))).await.status(), 200);
    assert_eq!(t.data_api_calls(), vec!["rebootstrap:web1", "rebootstrap:web1:grant_existing", "rebootstrap:web1:grant_existing"]);
    // Explicitly false and unchanged: nothing runs.
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "default_schema": "app2", "grant_existing": false }))).await.status(), 200);
    assert_eq!(t.data_api_calls().len(), 3);
    // A failed grant on an unchanged schema leaves the stored schema alone.
    *t.jobs.data_api_reply.lock().unwrap() = Some(Ok(CheckDataRoleReply::failed("boom")));
    error_of(t.send(Method::PATCH, &path, Some(json!({ "default_schema": "app2", "grant_existing": true }))).await, 502).await;
    assert_eq!(t.metadata.pg_database(main, "web1").default_schema, "app2");
    // Not a bool: rejected.
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "default_schema": "app", "grant_existing": "yes" }))).await.status(), 422);
}

#[tokio::test]
async fn grant_existing_on_a_disabled_database_is_409() {
    let t = branching().await;
    let main = t.seeded();
    let path = t.pg(main, "analytics"); // off
    let err = error_of(t.send(Method::PATCH, &path, Some(json!({ "default_schema": "public", "grant_existing": true }))).await, 409).await;
    assert_eq!(code_of(&err), "data_api_disabled");
    // A changed schema on a database that isn't served is only stored; without the flag too.
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "default_schema": "public" }))).await.status(), 200);
    assert!(t.data_api_calls().is_empty());
}

#[tokio::test]
async fn update_database_no_longer_takes_default_schema() {
    let t = branching().await;
    let before = t.metadata.database(t.db);
    let resp = t.send(Method::PATCH, &database_path(&t), Some(json!({ "default_schema": "app" }))).await;
    assert_eq!(resp.status(), 422);
    let after = t.metadata.database(t.db);
    assert_eq!((before.max_rows, before.max_connections), (after.max_rows, after.max_connections));
    let body: Value = t.send(Method::PATCH, &database_path(&t), Some(json!({}))).await.json().await.unwrap();
    assert!(body.get("default_schema").is_none(), "{body}");
}

#[tokio::test]
async fn key_allow_list_validation() {
    let t = branching().await;
    let main = t.seeded();
    let (public_id, _) = t.seed_public_key(main);
    let (secret_id, _) = t.seed_secret_key(main, "ci", "reporting");
    let too_many: Vec<String> = (0..21).map(|i| format!("db{i}")).collect();
    let long = "a".repeat(64);
    let invalid: Vec<(&str, Value)> = vec![
        ("empty list", json!([])),
        ("empty name", json!([""])),
        ("64 bytes", json!([long])),
        ("21 names", json!(too_many)),
        ("duplicate", json!(["web1", "web1"])),
        ("unknown", json!(["web1", "nope"])),
        ("system", json!(["postgres"])),
        ("not a name", json!([1])),
        ("not a list", json!("web1")),
    ];
    for (what, list) in &invalid {
        let resp = t.send(Method::POST, &t.keys(main), Some(json!({ "name": "k", "databases": list }))).await;
        assert_eq!(resp.status(), 422, "create: {what}");
        assert_eq!(code_of(&resp.json::<Value>().await.unwrap()), "invalid_request");
        for id in [secret_id, public_id] {
            let resp = t.send(Method::PATCH, &format!("{}/{id}", t.keys(main)), Some(json!({ "databases": list }))).await;
            assert_eq!(resp.status(), 422, "patch: {what}");
        }
    }
    assert_eq!(t.metadata.keys.lock().unwrap().len(), 2, "nothing was created");
    assert_eq!(t.metadata.key_databases(secret_id), None);
    // PATCH must say what it means: a missing `databases` is not "every database".
    let resp = t.send(Method::PATCH, &format!("{}/{secret_id}", t.keys(main)), Some(json!({}))).await;
    assert_eq!(resp.status(), 422);

    // A disabled database (and the main one) is allowed; the list comes back on create and list.
    let resp = t
        .send(Method::POST, &t.keys(main), Some(json!({ "name": "k", "databases": ["web1", "analytics"] })))
        .await;
    assert_eq!(resp.status(), 201);
    let created: Value = resp.json().await.unwrap();
    assert_eq!(created["databases"], json!(["web1", "analytics"]));
    let id: uuid::Uuid = created["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(t.metadata.key_databases(id), Some(vec!["web1".into(), "analytics".into()]));
    let listed = t.list_keys(main).await;
    let by = |id: uuid::Uuid| listed.iter().find(|k| k["id"] == json!(id)).unwrap();
    assert_eq!(by(id)["databases"], json!(["web1", "analytics"]));
    assert!(by(secret_id)["databases"].is_null());
    // Omitted and null both mean every database.
    for body in [json!({ "name": "a" }), json!({ "name": "b", "databases": null })] {
        let created: Value = t.send(Method::POST, &t.keys(main), Some(body)).await.json().await.unwrap();
        assert!(created["databases"].is_null(), "{created}");
    }
    // A branch's list is checked against that branch's own databases.
    let dev = t.ready("dev");
    let resp = t.send(Method::POST, &t.keys(dev), Some(json!({ "name": "k", "databases": ["web1"] }))).await;
    assert_eq!(resp.status(), 422);
    // Rotation keeps the list.
    let resp = t.send(Method::POST, &format!("{}/{id}/rotate", t.keys(main)), None).await;
    assert_eq!(resp.status(), 201);
    let rotated: Value = resp.json().await.unwrap();
    assert_eq!(rotated["databases"], json!(["web1", "analytics"]));
}

#[tokio::test]
async fn patch_key_databases_invalidates() {
    let (t, seen) = branching_with_rest(10).await;
    let main = t.seeded();
    let (public_id, _) = t.seed_public_key(main);
    let (id, _) = t.seed_secret_key(main, "ci", "reporting");
    let path = format!("{}/{id}", t.keys(main));
    let main_text = main.as_uuid().to_string();

    // Narrowing from "every database": stored, the key returned, rest told.
    let resp = t.send(Method::PATCH, &path, Some(json!({ "databases": ["web1"] }))).await;
    assert_eq!(resp.status(), 200);
    let key: Value = resp.json().await.unwrap();
    assert_eq!((key["id"].as_str(), key["databases"].clone()), (Some(id.to_string().as_str()), json!(["web1"])));
    assert_eq!(key["kind"], "secret");
    assert!(key.get("key").is_none(), "{key}");
    assert_eq!(t.metadata.key_databases(id), Some(vec!["web1".into()]));
    wait_for_invalidations(&seen, 1).await;
    assert_eq!(seen.lock().unwrap()[0], (main_text.clone(), Some(format!("Bearer {REST_TOKEN}"))));

    // Changing the list invalidates; so does widening back to null (the old tokens are stale).
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "databases": ["analytics", "web1"] }))).await.status(), 200);
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "databases": null }))).await.status(), 200);
    assert_eq!(t.metadata.key_databases(id), None);
    assert_eq!(seen.lock().unwrap().len(), 3);
    // The same list again changes nothing and tells no one.
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "databases": null }))).await.status(), 200);
    assert_eq!(seen.lock().unwrap().len(), 3);

    // It works on the public key too (and shows the key in full, as the list does).
    let resp = t.send(Method::PATCH, &format!("{}/{public_id}", t.keys(main)), Some(json!({ "databases": ["web1"] }))).await;
    assert_eq!(resp.status(), 200);
    assert!(resp.json::<Value>().await.unwrap()["key"].is_string());
    assert_eq!(seen.lock().unwrap().len(), 4);

    // Unknown and revoked keys, and another branch's key: 404.
    let missing = format!("{}/{}", t.keys(main), uuid::Uuid::new_v4());
    assert_eq!(t.send(Method::PATCH, &missing, Some(json!({ "databases": null }))).await.status(), 404);
    assert_eq!(t.send(Method::DELETE, &path, None).await.status(), 204);
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "databases": null }))).await.status(), 404);
    let dev = t.ready("dev");
    let (other, _) = t.seed_secret_key(main, "other", "reporting");
    assert_eq!(t.send(Method::PATCH, &format!("{}/{other}", t.keys(dev)), Some(json!({ "databases": null }))).await.status(), 404);
    assert_eq!(seen.lock().unwrap().len(), 4);
}

#[test]
fn the_database_cap_is_read_from_the_environment() {
    use orchestrator::data_api_database_cap;
    assert_eq!(data_api_database_cap(None), Ok(10));
    assert_eq!(data_api_database_cap(Some("")), Ok(10));
    assert_eq!(data_api_database_cap(Some("  ")), Ok(10));
    assert_eq!(data_api_database_cap(Some("1")), Ok(1));
    assert_eq!(data_api_database_cap(Some(" 100 ")), Ok(100));
    for bad in ["0", "101", "-1", "ten", "1.5"] {
        let err = data_api_database_cap(Some(bad)).unwrap_err();
        assert!(err.contains("CHELABASE_BRANCH_DATA_API_DATABASE_CAP"), "{bad}: {err}");
    }
}

// ---- Studio's console (step 6c, task 6)

/// One request the rest stand-in received: its path, every header as `name: value` lines,
/// and its body.
#[derive(Debug, Clone)]
struct RestCall {
    path: String,
    query: String,
    headers: String,
    body: String,
}

type RestCalls = Arc<Mutex<Vec<RestCall>>>;

type RestAnswer = Arc<dyn Fn(&Value) -> (u16, Vec<(&'static str, &'static str)>, Value) + Send + Sync>;

/// A rest stand-in that records every request (on any path) and answers the console routes
/// (`/query`, and T5's full-cell `/cell`, P36) with `answer(body)` and the schema route with
/// `answer(null)`; other paths answer 404.
async fn mock_rest_console(answer: RestAnswer) -> (String, RestCalls) {
    use axum::extract::Request;
    let calls: RestCalls = Arc::default();
    let log = calls.clone();
    let app = Router::new().fallback(move |req: Request| {
        let log = log.clone();
        let answer = answer.clone();
        async move {
            let path = req.uri().path().to_string();
            let query = req.uri().query().unwrap_or_default().to_string();
            let headers = req
                .headers()
                .iter()
                .map(|(k, v)| format!("{k}: {}", v.to_str().unwrap_or("")))
                .collect::<Vec<_>>()
                .join("\n");
            let bytes = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
            let body = String::from_utf8_lossy(&bytes).into_owned();
            log.lock().unwrap().push(RestCall { path: path.clone(), query, headers, body: body.clone() });
            let schema = path.starts_with("/internal/schema/");
            let console = path.starts_with("/internal/console/") && (path.ends_with("/query") || path.ends_with("/cell"));
            if !schema && !console {
                return axum::response::IntoResponse::into_response(axum::http::StatusCode::NOT_FOUND);
            }
            let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let (status, headers, json) = answer(&request);
            let mut resp = axum::response::IntoResponse::into_response((
                axum::http::StatusCode::from_u16(status).unwrap(),
                axum::Json(json),
            ));
            for (k, v) in headers {
                resp.headers_mut().insert(k, axum::http::HeaderValue::from_static(v));
            }
            resp
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, calls)
}

/// `branching()` with a rest stand-in for the console and the seeded databases: `web1`
/// (main, on), `analytics` and `scratch` (off), and the system `postgres`.
async fn console_app(answer: RestAnswer) -> (Branching, RestCalls) {
    let (rest, calls) = mock_rest_console(answer).await;
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app_with_rest(metadata.clone(), jobs.clone(), &rest).await;
    let db = ready_database(tenant, &app.key);
    let id = db.database_id;
    metadata.insert_database(db);
    let t = Branching { app, metadata, jobs, db: id };
    t.seeded();
    (t, calls)
}

fn rest_ok(body: Value) -> RestAnswer {
    Arc::new(move |_| (200, vec![], body.clone()))
}

impl Branching {
    async fn console(&self, branch: BranchId, body: Value) -> reqwest::Response {
        self.send(Method::POST, &format!("{}/console/query", self.branch(branch)), Some(body)).await
    }

    /// `GET …/console/schema` with `query` (`""` or `?…`).
    async fn console_schema(&self, branch: BranchId, query: &str) -> reqwest::Response {
        self.send(Method::GET, &format!("{}/console/schema{query}", self.branch(branch)), None).await
    }

    fn role_checks(&self) -> Vec<(BranchId, String)> {
        self.jobs.role_checks.lock().unwrap().clone()
    }

    fn compute_lookups(&self) -> usize {
        self.app.locator.asked.lock().unwrap().len()
    }
}

fn select_notes() -> Value {
    json!({ "v": 1, "op": "select", "from": "notes" })
}

#[tokio::test]
async fn other_roles_check_once_then_forward_to_rest() {
    let rows = json!([{ "id": 1, "body": "hi" }]);
    let (t, calls) = console_app(rest_ok(rows.clone())).await;
    let main = t.main();
    for _ in 0..2 {
        let resp = t.console(main, json!({ "role": "editor", "database": null, "tree": select_notes() })).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["content-type"], "application/json");
        assert_eq!(resp.json::<Value>().await.unwrap(), rows);
    }
    // The worker checked the role once (it grants it to authenticator); the second request
    // went straight to rest.
    assert_eq!(t.role_checks(), vec![(main, "editor".to_string())]);
    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    for call in &calls {
        assert_eq!(call.path, format!("/internal/console/{}/query", main.as_uuid()));
        assert!(call.headers.contains(&format!("authorization: Bearer {REST_TOKEN}")), "{}", call.headers);
        let body: Value = serde_json::from_str(&call.body).unwrap();
        // No database named: main's, by name.
        assert_eq!(body, json!({ "role": "editor", "database": "web1", "tree": select_notes() }));
    }
    // Another role is checked on its own; the owner is never forwarded.
    t.metadata.set_data_api(main, "analytics", chelabase_metadata::DataApiState::On);
    let resp = t.console(main, json!({ "role": "viewer", "database": "analytics", "tree": select_notes() })).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(t.role_checks().len(), 2);
}

#[tokio::test]
async fn rest_answers_pass_through_with_their_status() {
    let answer: RestAnswer = Arc::new(|request: &Value| match request["tree"]["op"].as_str() {
        Some("delete") => (
            409,
            vec![],
            json!({ "error": { "code": "conflict", "message": "duplicate key", "sqlstate": "23505" } }),
        ),
        _ => (
            503,
            vec![("retry-after", "2")],
            json!({ "error": { "code": "unavailable", "message": "try again" } }),
        ),
    });
    let (t, _) = console_app(answer).await;
    let main = t.main();
    let delete = json!({ "v": 1, "op": "delete", "from": "notes", "where": { "col": "id", "eq": 1 } });
    let resp = t.console(main, json!({ "role": "editor", "tree": delete })).await;
    let body = error_of(resp, 409).await;
    assert_eq!(body, json!({ "error": { "code": "conflict", "message": "duplicate key", "sqlstate": "23505" } }));
    let resp = t.console(main, json!({ "role": "editor", "tree": select_notes() })).await;
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.headers()["retry-after"], "2");
}

#[tokio::test]
async fn owner_level_role_is_refused() {
    let (t, calls) = console_app(rest_ok(json!([]))).await;
    let main = t.main();
    let refusal = "the role `boss` can bypass row-level security; pick a plain role";
    *t.jobs.role_check.lock().unwrap() = Some(Ok(CheckDataRoleReply::refused(refusal)));
    for _ in 0..2 {
        let resp = t.console(main, json!({ "role": "boss", "tree": select_notes() })).await;
        let body = error_of(resp, 403).await;
        assert_eq!(code_of(&body), "role_refused");
        assert_eq!(message_of(&body), refusal);
    }
    // A refusal isn't remembered as a pass, and rest never heard of it.
    assert_eq!(t.role_checks().len(), 2);
    assert!(calls.lock().unwrap().is_empty());
    // A branch the worker can't reach yet.
    *t.jobs.role_check.lock().unwrap() = Some(Ok(CheckDataRoleReply::not_ready()));
    let resp = t.console(main, json!({ "role": "boss", "tree": select_notes() })).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "branch_not_ready");
    // rest's own refusal (an owner-level role that slipped through) passes through.
    *t.jobs.role_check.lock().unwrap() = None;
    let (t2, _) = console_app(Arc::new(|_| {
        (403, vec![], json!({ "error": { "code": "role_not_allowed", "message": "no" } }))
    }))
    .await;
    let resp = t2.console(t2.main(), json!({ "role": "boss", "tree": select_notes() })).await;
    assert_eq!(code_of(&error_of(resp, 403).await), "role_not_allowed");
}

#[tokio::test]
async fn a_disabled_database_is_409() {
    let (t, calls) = console_app(rest_ok(json!([]))).await;
    let main = t.main();
    for state in [
        chelabase_metadata::DataApiState::Off,
        chelabase_metadata::DataApiState::Enabling,
        chelabase_metadata::DataApiState::Error,
    ] {
        t.metadata.set_data_api(main, "analytics", state);
        for role in [Value::Null, json!("editor")] {
            let resp = t.console(main, json!({ "role": role, "database": "analytics", "tree": select_notes() })).await;
            let body = error_of(resp, 409).await;
            assert_eq!(code_of(&body), "data_api_disabled", "{state:?} {role}");
        }
        // Named by the tree instead of the request: the same.
        let tree = json!({ "v": 1, "op": "select", "from": "notes", "database": "analytics" });
        let resp = t.console(main, json!({ "role": null, "tree": tree })).await;
        assert_eq!(code_of(&error_of(resp, 409).await), "data_api_disabled");
    }
    assert!(t.role_checks().is_empty());
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(t.compute_lookups(), 0);
}

#[tokio::test]
async fn rest_unknown_database_maps_to_the_rows_state() {
    // rest says the database isn't served: the orchestrator reads the row again.
    let metadata_slot: Arc<Mutex<Option<(FakeMetadata, BranchId)>>> = Arc::default();
    let slot = metadata_slot.clone();
    let answer: RestAnswer = Arc::new(move |request: &Value| {
        // A disable landed between the orchestrator's check and rest's.
        if request["database"] == "analytics"
            && let Some((metadata, branch)) = slot.lock().unwrap().clone()
        {
            metadata.set_data_api(branch, "analytics", chelabase_metadata::DataApiState::Off);
        }
        (404, vec![], json!({ "error": { "code": "unknown_database", "message": "the database doesn't exist or isn't served by this key" } }))
    });
    let (t, _) = console_app(answer).await;
    let main = t.main();
    *metadata_slot.lock().unwrap() = Some((t.metadata.clone(), main));
    t.metadata.set_data_api(main, "analytics", chelabase_metadata::DataApiState::On);
    let resp = t.console(main, json!({ "role": "editor", "database": "analytics", "tree": select_notes() })).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "data_api_disabled");
    // Still on in metadata: a plain 404.
    let resp = t.console(main, json!({ "role": "editor", "tree": select_notes() })).await;
    assert_eq!(code_of(&error_of(resp, 404).await), "not_found");
}

#[tokio::test]
async fn an_unknown_database_branch_or_tenant_is_404() {
    let (t, calls) = console_app(rest_ok(json!([]))).await;
    let main = t.main();
    for role in [Value::Null, json!("editor")] {
        let resp = t.console(main, json!({ "role": role, "database": "nope", "tree": select_notes() })).await;
        assert_eq!(code_of(&error_of(resp, 404).await), "not_found", "{role}");
    }
    // The system database is never served: it answers as missing.
    let resp = t.console(main, json!({ "role": null, "database": "postgres", "tree": select_notes() })).await;
    assert_eq!(resp.status().as_u16() / 100, 4);
    let resp = t.console(BranchId::new(), json!({ "role": null, "tree": select_notes() })).await;
    assert_eq!(code_of(&error_of(resp, 404).await), "not_found");
    // Another tenant asking for this database's branch.
    t.metadata.bind_org("org-2", TenantId::new());
    let resp = reqwest::Client::new()
        .post(format!("{}{}/console/query", t.app.base, t.branch(main)))
        .header(IDENTITY_HEADER, t.app.token_for_org("org-2"))
        .json(&json!({ "role": "editor", "tree": select_notes() }))
        .send()
        .await
        .unwrap();
    assert_eq!(code_of(&error_of(resp, 404).await), "not_found");
    assert!(calls.lock().unwrap().is_empty());
    assert!(t.role_checks().is_empty());
}

#[tokio::test]
async fn bad_console_requests_are_refused_before_anything_runs() {
    let (t, calls) = console_app(rest_ok(json!([]))).await;
    let main = t.main();
    // The request and its tree name different databases.
    let tree = json!({ "v": 1, "op": "select", "from": "notes", "database": "analytics" });
    for role in [Value::Null, json!("editor")] {
        let resp = t.console(main, json!({ "role": role, "database": "web1", "tree": tree })).await;
        assert_eq!(code_of(&error_of(resp, 400).await), "invalid_query", "{role}");
    }
    // A tree the data API's grammar refuses.
    for role in [Value::Null, json!("editor")] {
        let resp = t.console(main, json!({ "role": role, "tree": { "v": 2, "op": "select", "from": "notes" } })).await;
        assert_eq!(code_of(&error_of(resp, 400).await), "invalid_query", "{role}");
    }
    // Not the console's shape.
    let resp = t.console(main, json!({ "role": 7, "tree": select_notes() })).await;
    assert_eq!(code_of(&error_of(resp, 422).await), "invalid_request");
    let resp = t.console(main, json!({ "role": "" , "tree": select_notes() })).await;
    assert_eq!(code_of(&error_of(resp, 422).await), "invalid_request");
    assert!(calls.lock().unwrap().is_empty());
    assert!(t.role_checks().is_empty());
    assert_eq!(t.compute_lookups(), 0);
}

/// Naming the owner's own role would skip the owner path for a forward rest can't serve:
/// the console says how to ask for the owner instead.
#[tokio::test]
async fn the_owners_name_as_a_role_is_422() {
    let (t, calls) = console_app(rest_ok(json!([]))).await;
    let main = t.main();
    let resp = t.console(main, json!({ "role": "postgres", "tree": select_notes() })).await;
    let body = error_of(resp, 422).await;
    assert_eq!(code_of(&body), "invalid_request");
    assert_eq!(message_of(&body), "use role: null to run as the owner");
    let resp = t.console_schema(main, "?role=postgres").await;
    let body = error_of(resp, 422).await;
    assert_eq!(message_of(&body), "omit role to view the schema as the owner");
    assert!(calls.lock().unwrap().is_empty());
    assert!(t.role_checks().is_empty());
}

/// rest's 401 means the orchestrator's own bearer was refused, never that the user is signed
/// out: Studio signs the user out on a 401, so every route that passes rest's answer on turns
/// it into a 502 `unavailable`. rest's 403 is the user's request refused and stays as it is.
#[tokio::test]
async fn a_rest_401_never_reaches_the_browser_as_a_401() {
    let refused: RestAnswer = Arc::new(|_| {
        (401, vec![], json!({ "error": { "code": "unauthorized", "message": "a valid bearer is required" } }))
    });
    let (t, calls) = cell_app(refused).await;
    let main = t.main();
    t.metadata.set_data_api(main, "analytics", chelabase_metadata::DataApiState::On);
    let mut seen = Vec::new();
    let console = t
        .console(main, json!({ "role": "editor", "database": null, "tree": select_notes() }))
        .await;
    seen.push(("console query", console));
    seen.push(("role schema", t.console_schema(main, "?role=editor").await));
    let cell = t
        .cell_as("mixed", main, json!({ "role": "billing_admin", "key": { "id": 1 }, "column": "id" }))
        .await;
    seen.push(("cell", cell));
    for (route, resp) in seen {
        let status = resp.status().as_u16();
        let body: Value = resp.json().await.unwrap();
        assert_eq!(status, 502, "{route}: {body}");
        assert_eq!(code_of(&body), "unavailable", "{route}");
        assert!(!message_of(&body).contains("bearer"), "{route}: {body}");
    }
    assert_eq!(calls.lock().unwrap().len(), 3, "each route asked rest");
}

#[tokio::test]
async fn console_schema_as_a_role_checks_then_forwards_role_and_database() {
    let schema = json!({ "version": "v1", "default_schema": "reporting", "schemas": {} });
    let (t, calls) = console_app(rest_ok(schema.clone())).await;
    let main = t.main();
    t.metadata.set_data_api(main, "analytics", chelabase_metadata::DataApiState::On);
    let resp = t.console_schema(main, "?role=editor&database=analytics").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "application/json");
    assert_eq!(resp.json::<Value>().await.unwrap(), schema);
    // No database: main's, by name. The role was checked once.
    assert_eq!(t.console_schema(main, "?role=editor").await.status(), 200);
    assert_eq!(t.role_checks(), vec![(main, "editor".to_string())]);
    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    for (call, database) in calls.iter().zip(["analytics", "web1"]) {
        assert_eq!(call.path, format!("/internal/schema/{}", main.as_uuid()));
        assert!(call.headers.contains(&format!("authorization: Bearer {REST_TOKEN}")), "{}", call.headers);
        let mut pairs: Vec<&str> = call.query.split('&').collect();
        pairs.sort();
        assert_eq!(pairs, [format!("database={database}").as_str(), "role=editor"]);
    }
}

#[tokio::test]
async fn console_schema_refusals() {
    let answer: RestAnswer = Arc::new(|_| {
        (404, vec![], json!({ "error": { "code": "unknown_database", "message": "the database doesn't exist or isn't served by this key" } }))
    });
    let (t, calls) = console_app(answer).await;
    let main = t.main();
    // Unknown, disabled (whoever asks), the system database.
    for query in ["?database=nope", "?database=nope&role=editor", "?database=postgres"] {
        assert_eq!(code_of(&error_of(t.console_schema(main, query).await, 404).await), "not_found", "{query}");
    }
    for query in ["?database=analytics", "?database=analytics&role=editor"] {
        let body = error_of(t.console_schema(main, query).await, 409).await;
        assert_eq!(code_of(&body), "data_api_disabled", "{query}");
    }
    // An empty role.
    assert_eq!(code_of(&error_of(t.console_schema(main, "?role=").await, 422).await), "invalid_request");
    // The owner without a sealed password; the worker refusing a role.
    t.metadata.branches.lock().unwrap().get_mut(&main).unwrap().owner_password_enc = None;
    assert_eq!(code_of(&error_of(t.console_schema(main, "").await, 409).await), "owner_password_unknown");
    *t.jobs.role_check.lock().unwrap() = Some(Ok(CheckDataRoleReply::refused("no")));
    assert_eq!(code_of(&error_of(t.console_schema(main, "?role=boss").await, 403).await), "role_refused");
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(t.compute_lookups(), 0);
    // rest no longer serving it after the check: the row decides (still on: 404).
    *t.jobs.role_check.lock().unwrap() = None;
    assert_eq!(code_of(&error_of(t.console_schema(main, "?role=editor").await, 404).await), "not_found");
    // Backups and branches that aren't ready.
    let backup = t.backup(main);
    assert_eq!(code_of(&error_of(t.console_schema(backup, "?role=editor").await, 409).await), "backup_branch");
    let pending = t.add("dev", main, BranchKind::Normal, BranchStatus::Pending);
    assert_eq!(code_of(&error_of(t.console_schema(pending, "").await, 409).await), "branch_not_ready");
}

#[tokio::test]
async fn owner_password_unknown_is_409_before_any_connection() {
    let (t, calls) = console_app(rest_ok(json!([]))).await;
    let main = t.main();
    for sealed in [None, Some(String::new())] {
        t.metadata.branches.lock().unwrap().get_mut(&main).unwrap().owner_password_enc = sealed;
        let resp = t.console(main, json!({ "role": null, "tree": select_notes() })).await;
        let body = error_of(resp, 409).await;
        assert_eq!(code_of(&body), "owner_password_unknown");
        assert_eq!(message_of(&body), "reset the owner's password to view as owner");
    }
    assert_eq!(t.compute_lookups(), 0);
    assert!(calls.lock().unwrap().is_empty());
}

/// RT1 T4: the console takes the console-only forms (`text_of`, `"text": true`) on both
/// paths: as a role they go to rest as written, as the owner they get past the parser to the
/// compute lookup (503 with none running, not a 400). A form that is wrong is still a 400
/// before anything runs.
#[tokio::test]
async fn console_takes_the_console_only_forms_on_both_paths() {
    let (t, calls) = console_app(rest_ok(json!([]))).await;
    let main = t.main();
    let trees = [
        json!({ "v": 1, "op": "select", "from": "notes",
            "select": [{ "text_of": "id", "as": "id_text" }, "body"],
            "where": { "col": "id", "eq": "9007199254740993", "text": true } }),
        json!({ "v": 1, "op": "count", "from": "notes",
            "where": { "col": "id", "in": ["1", "2"], "text": true } }),
        json!({ "v": 1, "op": "batch", "ops": [select_notes(),
            { "op": "select", "from": "notes", "select": [{ "text_of": "id", "as": "k" }] }] }),
    ];
    for tree in &trees {
        let resp = t.console(main, json!({ "role": "editor", "tree": tree })).await;
        assert_eq!(resp.status(), 200, "{tree}");
    }
    let forwarded: Vec<Value> = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.path.ends_with("/query"))
        .map(|c| serde_json::from_str::<Value>(&c.body).unwrap()["tree"].clone())
        .collect();
    assert_eq!(forwarded, trees.to_vec(), "rest gets the trees as written");

    let before = t.compute_lookups();
    for tree in &trees {
        let resp = t.console(main, json!({ "role": null, "tree": tree })).await;
        let status = resp.status();
        let body: Value = resp.json().await.unwrap();
        assert_eq!(status, 503, "{tree}: {body}");
        assert_eq!(code_of(&body), "unavailable", "{tree}");
    }
    assert_eq!(t.compute_lookups(), before + trees.len(), "the owner path went on to the compute");

    // Wrong forms never get that far, on either path.
    let sent = calls.lock().unwrap().len();
    for bad in [
        json!({ "v": 1, "op": "select", "from": "notes", "select": [{ "text_of": "id" }] }),
        json!({ "v": 1, "op": "select", "from": "notes", "where": { "col": "id", "neq": "1", "text": true } }),
        json!({ "v": 1, "op": "select", "from": "notes", "where": { "col": "id", "eq": 1, "text": true } }),
    ] {
        for role in [json!("editor"), Value::Null] {
            let resp = t.console(main, json!({ "role": role, "tree": bad })).await;
            assert_eq!(code_of(&error_of(resp, 400).await), "invalid_query", "{role}: {bad}");
        }
    }
    assert_eq!(calls.lock().unwrap().len(), sent);
    assert_eq!(t.compute_lookups(), before + trees.len());
}

#[tokio::test]
async fn owner_console_without_a_running_compute_is_503() {
    let (t, calls) = console_app(rest_ok(json!([]))).await;
    let main = t.main();
    let resp = t.console(main, json!({ "role": null, "tree": select_notes() })).await;
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.headers()["retry-after"], "2");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(code_of(&body), "unavailable");
    assert_eq!(*t.app.locator.asked.lock().unwrap(), vec!["ep-0123456789ab".to_string()]);
    assert!(calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn backups_and_unready_branches_are_refused() {
    let (t, calls) = console_app(rest_ok(json!([]))).await;
    let backup = t.backup(t.main());
    let resp = t.console(backup, json!({ "role": "editor", "tree": select_notes() })).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "backup_branch");
    let pending = t.add("dev", t.main(), BranchKind::Normal, BranchStatus::Pending);
    let resp = t.console(pending, json!({ "role": null, "tree": select_notes() })).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "branch_not_ready");
    assert!(calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_owner_password_never_goes_to_rest() {
    let (t, calls) = console_app(rest_ok(json!([]))).await;
    let main = t.main();
    let sealed = t.metadata.branch(main).owner_password_enc.unwrap();
    t.metadata.set_data_api(main, "analytics", chelabase_metadata::DataApiState::On);
    // The owner path, with and without a compute: never forwarded.
    for database in [Value::Null, json!("analytics")] {
        let resp = t.console(main, json!({ "role": null, "database": database, "tree": select_notes() })).await;
        assert_eq!(resp.status(), 503);
    }
    assert!(calls.lock().unwrap().is_empty(), "the owner path went to rest");
    // Every other role, read and write: rest sees the role, the database and the tree only.
    let insert = json!({ "v": 1, "op": "insert", "into": "notes", "values": [{ "body": "x" }] });
    for (role, tree) in [("editor", select_notes()), ("editor", insert), ("viewer", select_notes())] {
        let resp = t.console(main, json!({ "role": role, "tree": tree })).await;
        assert_eq!(resp.status(), 200);
    }
    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 3);
    for call in calls {
        for secret in ["s3cret", sealed.as_str()] {
            assert!(!call.body.contains(secret), "body: {}", call.body);
            assert!(!call.headers.contains(secret), "headers: {}", call.headers);
            assert!(!call.path.contains(secret), "path: {}", call.path);
        }
        let body: Value = serde_json::from_str(&call.body).unwrap();
        let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["database", "role", "tree"]);
    }
}

/// Collects what the orchestrator logs while a test runs (the test's runtime is
/// single-threaded, so the server's tasks log through this thread's subscriber).
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    fn capture(&self) -> tracing::subscriber::DefaultGuard {
        let logs = self.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || logs.clone())
            .finish();
        tracing::subscriber::set_default(subscriber)
    }
}

#[tokio::test]
async fn console_writes_are_audited_without_values() {
    let logs = Logs::default();
    let _guard = logs.capture();
    let (t, _) = console_app(rest_ok(json!([]))).await;
    let main = t.main();
    const SECRET: &str = "card-4111-1111-1111-1111";
    let insert = json!({ "v": 1, "op": "insert", "into": "notes", "values": [{ "body": SECRET }] });
    let update = json!({ "v": 1, "op": "update", "table": "notes", "set": { "body": SECRET }, "where": { "col": "id", "eq": 1 } });
    let batch = json!({ "v": 1, "op": "batch", "ops": [select_notes(), { "op": "delete", "from": "notes", "where": { "col": "body", "eq": SECRET } }] });
    let call = json!({ "v": 1, "op": "call", "fn": "touch", "args": { "note": SECRET } });
    for tree in [select_notes(), insert, update, batch, call] {
        let resp = t.console(main, json!({ "role": "editor", "tree": tree })).await;
        assert_eq!(resp.status(), 200);
    }
    // A read isn't a write.
    let resp = t.console(main, json!({ "role": "editor", "tree": { "v": 1, "op": "batch", "ops": [select_notes()] } })).await;
    assert_eq!(resp.status(), 200);
    let text = logs.text();
    let audit: Vec<&str> = text.lines().filter(|l| l.contains("console write")).collect();
    assert_eq!(audit.len(), 4, "{text}");
    for (line, op) in audit.iter().zip(["insert", "update", "batch", "call"]) {
        assert!(line.contains("user=user-1"), "{line}");
        assert!(line.contains(&format!("branch={}", main.as_uuid())), "{line}");
        assert!(line.contains("database=web1"), "{line}");
        assert!(line.contains("role=editor"), "{line}");
        assert!(line.contains(&format!("op={op}")), "{line}");
    }
    assert!(!text.contains(SECRET), "a value reached the log:\n{text}");
}

// ---- the owner path against a real Postgres: the dev metadata Postgres stands in for a
// compute (a scratch database and a scratch owner role, dropped at the end).

struct ScratchCompute {
    admin: sqlx::PgPool,
    database: String,
    owner: String,
    addr: std::net::SocketAddr,
}

impl ScratchCompute {
    async fn new() -> Self {
        use sqlx::Executor;
        let url = std::env::var("CHELABASE_METADATA_DATABASE_URL")
            .unwrap_or_else(|_| "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".into());
        let admin = sqlx::PgPool::connect(&url).await.expect("the metadata Postgres (is the workbench stack up?)");
        let suffix = format!("{:08x}", rand_u32());
        let database = format!("t6_console_{suffix}");
        let owner = format!("t6_owner_{suffix}");
        admin.execute(sqlx::AssertSqlSafe(format!("CREATE ROLE {owner} LOGIN"))).await.unwrap();
        admin
            .execute(sqlx::AssertSqlSafe(format!("CREATE DATABASE {database} OWNER {owner}")))
            .await
            .unwrap();
        let opts: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
        // The host is a service name on the stack network (`metadata-db`): resolved, not parsed.
        let addr = tokio::net::lookup_host((opts.get_host(), opts.get_port()))
            .await
            .unwrap()
            .next()
            .expect("the metadata Postgres host resolves");
        let scratch = sqlx::PgPool::connect_with(opts.database(&database)).await.unwrap();
        scratch
            .execute(sqlx::AssertSqlSafe(format!(
                "CREATE SCHEMA chelabase; \
                 CREATE TABLE chelabase.schema_version (version bigint NOT NULL); \
                 INSERT INTO chelabase.schema_version VALUES (1); \
                 GRANT USAGE ON SCHEMA chelabase TO {owner}; \
                 GRANT SELECT ON chelabase.schema_version TO {owner}; \
                 CREATE TABLE public.notes (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text NOT NULL); \
                 ALTER TABLE public.notes OWNER TO {owner};"
            )))
            .await
            .unwrap();
        scratch.close().await;
        ScratchCompute { admin, database, owner, addr }
    }

    async fn set_password(&self, password: &str) {
        use sqlx::Executor;
        self.admin
            .execute(sqlx::AssertSqlSafe(format!("ALTER ROLE {} PASSWORD '{password}'", self.owner)))
            .await
            .unwrap();
    }

    /// Runs `sql` in the scratch database, as the cluster's admin.
    async fn exec_in(&self, sql: &str) {
        use sqlx::Executor;
        let opts: sqlx::postgres::PgConnectOptions = (*self.admin.connect_options()).clone();
        let pool = sqlx::PgPool::connect_with(opts.database(&self.database)).await.unwrap();
        pool.execute(sqlx::AssertSqlSafe(sql.to_owned())).await.unwrap();
        pool.close().await;
    }

    /// Ends every session of the owner: a pool still using the old password must reconnect.
    async fn kick_owner(&self) {
        sqlx::query("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename = $1")
            .bind(&self.owner)
            .execute(&self.admin)
            .await
            .unwrap();
    }

    async fn drop(self) {
        use sqlx::Executor;
        self.admin
            .execute(sqlx::AssertSqlSafe(format!("DROP DATABASE {} WITH (FORCE)", self.database)))
            .await
            .unwrap();
        self.admin
            .execute(sqlx::AssertSqlSafe(format!("DROP ROLE {}", self.owner)))
            .await
            .unwrap();
    }
}

fn rand_u32() -> u32 {
    (SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos() ^ std::process::id().rotate_left(16))
        .wrapping_mul(2_654_435_761)
}

/// A console app whose main database is the scratch database, owned by the scratch owner,
/// with the compute at the metadata Postgres's address.
async fn owner_console_app(scratch: &ScratchCompute) -> Branching {
    let (t, _) = console_app(rest_ok(json!([]))).await;
    let main = t.main();
    {
        let mut databases = t.metadata.databases.lock().unwrap();
        let record = databases.get_mut(&t.db).unwrap();
        record.owner_role = Some(scratch.owner.clone());
        record.database_name = Some(scratch.database.clone());
    }
    let mut all = t.metadata.branch_dbs.lock().unwrap();
    let rows = all.get_mut(&main).unwrap();
    let web1 = rows.iter_mut().find(|d| d.name == "web1").unwrap();
    web1.name = scratch.database.clone();
    web1.owner = scratch.owner.clone();
    drop(all);
    *t.app.locator.addr.lock().unwrap() = Some(scratch.addr);
    t
}

impl Branching {
    fn seal_owner_password(&self, password: Option<&str>) {
        let main = self.main();
        let sealed = password.map(|p| seal_credential(&self.app.key, &main.to_string(), Purpose::Owner, p));
        self.metadata.branches.lock().unwrap().get_mut(&main).unwrap().owner_password_enc = sealed;
    }
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn owner_console_runs_through_the_owner_pool() {
    let logs = Logs::default();
    let _guard = logs.capture();
    let scratch = ScratchCompute::new().await;
    scratch.set_password("first-pass").await;
    let t = owner_console_app(&scratch).await;
    t.seal_owner_password(Some("first-pass"));
    let main = t.main();

    let insert = json!({ "v": 1, "op": "insert", "into": "notes", "values": [{ "body": "private words" }] });
    let resp = t.console(main, json!({ "role": null, "tree": insert })).await;
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(status, 200, "{text}");
    let resp = t.console(main, json!({ "role": null, "tree": select_notes() })).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "application/json");
    let rows: Value = resp.json().await.unwrap();
    assert_eq!(rows, json!([{ "id": 1, "body": "private words" }]));
    // A Postgres error comes back in the data API's shape.
    let bad = json!({ "v": 1, "op": "insert", "into": "notes", "values": [{ "id": 5, "body": "x" }] });
    let resp = t.console(main, json!({ "role": null, "tree": bad })).await;
    let body: Value = resp.json().await.unwrap();
    assert!(body["error"]["code"].is_string(), "{body}");
    // The owner pool holds at most 2 connections, under its own application name.
    let (n,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM pg_stat_activity WHERE usename = $1 AND application_name = 'chelabase-console'",
    )
    .bind(&scratch.owner)
    .fetch_one(&scratch.admin)
    .await
    .unwrap();
    assert!((1..=2).contains(&n), "{n} owner connections");
    // Audited as the owner, without the value.
    let text = logs.text();
    let audit: Vec<&str> = text.lines().filter(|l| l.contains("console write")).collect();
    assert_eq!(audit.len(), 2, "{text}");
    assert!(audit[0].contains(&format!("role={}", scratch.owner)), "{}", audit[0]);
    assert!(audit[0].contains("op=insert"), "{}", audit[0]);
    assert!(!text.contains("private words"), "{text}");
    assert!(!text.contains("first-pass"), "{text}");
    // rest was never asked.
    assert!(t.role_checks().is_empty());
    scratch.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn owner_password_unknown_then_works_after_reset() {
    let scratch = ScratchCompute::new().await;
    let t = owner_console_app(&scratch).await;
    let main = t.main();
    // Set in SQL: Chelabase doesn't know it.
    t.seal_owner_password(None);
    let resp = t.console(main, json!({ "role": null, "tree": select_notes() })).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "owner_password_unknown");
    // A reset: the worker sets a new password and stores it sealed. Nothing restarts.
    scratch.set_password("after-reset").await;
    t.seal_owner_password(Some("after-reset"));
    let resp = t.console(main, json!({ "role": null, "tree": select_notes() })).await;
    let status = resp.status();
    assert_eq!(status, 200, "{}", resp.text().await.unwrap());
    // Another reset while the first pool still holds connections: the new sealed password
    // makes a new pool (the old one couldn't reconnect with its password).
    scratch.set_password("second-reset").await;
    t.seal_owner_password(Some("second-reset"));
    scratch.kick_owner().await;
    let resp = t.console(main, json!({ "role": null, "tree": select_notes() })).await;
    let status = resp.status();
    assert_eq!(status, 200, "{}", resp.text().await.unwrap());
    scratch.drop().await;
}

/// The console's schema as the owner: everything the owner reaches, primary keys included,
/// a schema no other role may use among it.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn owner_console_schema_shows_what_only_the_owner_reaches() {
    let scratch = ScratchCompute::new().await;
    scratch.set_password("schema-pass").await;
    let owner = scratch.owner.clone();
    scratch
        .exec_in(&format!(
            "CREATE SCHEMA private AUTHORIZATION {owner}; \
             CREATE TABLE private.secret (id int PRIMARY KEY, note text); \
             ALTER TABLE private.secret OWNER TO {owner};"
        ))
        .await;
    let t = owner_console_app(&scratch).await;
    t.seal_owner_password(Some("schema-pass"));
    let main = t.main();
    let resp = t.console_schema(main, "").await;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["default_schema"], "public");
    let secret = &body["schemas"]["private"]["tables"]["secret"];
    assert_eq!(secret["primary_key"], json!(["id"]), "{body}");
    assert!(secret["privileges"].get(&owner).is_some(), "{body}");
    assert_eq!(body["schemas"]["public"]["tables"]["notes"]["primary_key"], json!(["id"]));
    // No other role may use `private` (PUBLIC has no USAGE on it): the role path wouldn't show it.
    let opts: sqlx::postgres::PgConnectOptions = (*scratch.admin.connect_options()).clone();
    let pool = sqlx::PgPool::connect_with(opts.database(&scratch.database)).await.unwrap();
    let (public_usage,): (bool,) = sqlx::query_as("SELECT has_schema_privilege('public', 'private', 'USAGE')")
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert!(!public_usage);
    // rest was never asked; no role was checked.
    assert!(t.role_checks().is_empty());
    scratch.drop().await;
}

// ---- Projects ---------------------------------------------------------------------

async fn send(app: &TestApp, method: Method, path: &str, org: &str, body: Option<Value>) -> reqwest::Response {
    let mut req = reqwest::Client::new()
        .request(method, format!("{}{path}", app.base))
        .header(IDENTITY_HEADER, app.token_for_org(org));
    if let Some(body) = body {
        req = req.json(&body);
    }
    req.send().await.unwrap()
}

/// org-1's tenant with the app running.
async fn projects_app(jobs: FakeJobs) -> (TestApp, FakeMetadata, TenantId) {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let app = spawn_app(metadata.clone(), jobs).await;
    (app, metadata, tenant)
}

fn project_database(tenant: TenantId, project: ProjectId, status: DatabaseStatus) -> Db {
    let mut db = sample_database(tenant);
    db.project_id = project;
    db.status = status;
    db.main.status = BranchStatus::from(status);
    db
}

#[tokio::test]
async fn list_projects_only_mine() {
    let (app, metadata, tenant) = projects_app(FakeJobs::default()).await;
    let mine = metadata.create_project(tenant, "Mine").await.unwrap();
    metadata.create_project(TenantId::new(), "Theirs").await.unwrap();
    let body: Value = send(&app, Method::GET, "/v1/projects", "org-1", None).await.json().await.unwrap();
    let list = body.as_array().unwrap();
    assert_eq!(list.len(), 1, "{body}");
    assert_eq!(list[0]["id"], json!(mine.id));
    assert_eq!(list[0]["name"], "Mine");
    assert_eq!(list[0]["status"], "active");
    assert!(list[0]["app_users"].is_null());
}

#[tokio::test]
async fn create_project_201_with_slug() {
    let (app, _, _) = projects_app(FakeJobs::default()).await;
    let resp = send(&app, Method::POST, "/v1/projects", "org-1", Some(json!({ "name": "  My App " }))).await;
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["name"], "My App");
    assert_eq!(body["slug"], "my-app");
    assert_eq!(body["status"], "active");
    assert!(body["error"].is_null());
    assert!(body["id"].is_string() && body["created_at"].is_string() && body["updated_at"].is_string());
    let again: Value = send(&app, Method::POST, "/v1/projects", "org-1", Some(json!({ "name": "My App" })))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(again["slug"], "my-app-2");
}

#[tokio::test]
async fn create_project_name_validation_422() {
    let (app, _, _) = projects_app(FakeJobs::default()).await;
    for name in ["", "   ", &"x".repeat(65)] {
        let resp = send(&app, Method::POST, "/v1/projects", "org-1", Some(json!({ "name": name }))).await;
        let body = error_of(resp, 422).await;
        assert_eq!(code_of(&body), "invalid_request");
    }
    let resp = send(&app, Method::POST, "/v1/projects", "org-1", Some(json!({}))).await;
    assert_eq!(resp.status(), 422);
}

#[tokio::test]
async fn get_other_tenant_project_404() {
    let (app, metadata, tenant) = projects_app(FakeJobs::default()).await;
    let mine = metadata.create_project(tenant, "Mine").await.unwrap();
    let theirs = metadata.create_project(TenantId::new(), "Theirs").await.unwrap();
    let resp = send(&app, Method::GET, &format!("/v1/projects/{}", mine.id), "org-1", None).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["slug"], "mine");
    for id in [theirs.id, ProjectId::new()] {
        let resp = send(&app, Method::GET, &format!("/v1/projects/{id}"), "org-1", None).await;
        assert_eq!(code_of(&error_of(resp, 404).await), "not_found");
    }
}

#[tokio::test]
async fn rename_project_changes_name_not_slug() {
    let (app, metadata, tenant) = projects_app(FakeJobs::default()).await;
    let p = metadata.create_project(tenant, "Old name").await.unwrap();
    let path = format!("/v1/projects/{}", p.id);
    let resp = send(&app, Method::PATCH, &path, "org-1", Some(json!({ "name": " New name " }))).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["name"], "New name");
    assert_eq!(body["slug"], "old-name");
    let resp = send(&app, Method::PATCH, &path, "org-1", Some(json!({ "name": "" }))).await;
    assert_eq!(resp.status(), 422);
    let resp = send(&app, Method::PATCH, &format!("/v1/projects/{}", ProjectId::new()), "org-1", Some(json!({ "name": "x" }))).await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn rename_deleting_project_409() {
    let (app, metadata, tenant) = projects_app(FakeJobs::default()).await;
    let p = metadata.create_project(tenant, "Going").await.unwrap();
    metadata.begin_project_delete(tenant, p.id).await.unwrap();
    let resp = send(&app, Method::PATCH, &format!("/v1/projects/{}", p.id), "org-1", Some(json!({ "name": "Back" }))).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "project_deleting");
}

#[tokio::test]
async fn delete_project_needs_the_slug() {
    let jobs = FakeJobs::default();
    let (app, metadata, tenant) = projects_app(jobs.clone()).await;
    let p = metadata.create_project(tenant, "Shop").await.unwrap();
    let path = format!("/v1/projects/{}", p.id);
    for confirm in ["Shop", "shop ", "other", ""] {
        let resp = send(&app, Method::DELETE, &path, "org-1", Some(json!({ "confirm": confirm }))).await;
        let body = error_of(resp, 422).await;
        assert_eq!(message_of(&body), "type the project's slug to confirm");
    }
    assert_eq!(send(&app, Method::DELETE, &path, "org-1", Some(json!({}))).await.status(), 422);
    assert!(jobs.project_deletes.lock().unwrap().is_empty());
    assert_eq!(metadata.get_project(tenant, p.id).await.unwrap().unwrap().status, ProjectStatus::Active);
    // An unknown project is 404 before the slug is looked at.
    let resp = send(&app, Method::DELETE, &format!("/v1/projects/{}", ProjectId::new()), "org-1", Some(json!({ "confirm": "x" }))).await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn delete_project_publishes_database_and_project_jobs() {
    let jobs = FakeJobs::default();
    let (app, metadata, tenant) = projects_app(jobs.clone()).await;
    let p = metadata.create_project(tenant, "Shop").await.unwrap();
    let ready = project_database(tenant, p.id, DatabaseStatus::Ready);
    let failed = project_database(tenant, p.id, DatabaseStatus::Failed);
    let other = project_database(tenant, ProjectId::new(), DatabaseStatus::Ready);
    let expected: HashSet<DatabaseId> = [ready.database_id, failed.database_id].into();
    for db in [ready, failed, other] {
        metadata.insert_database(db);
    }
    let resp = send(&app, Method::DELETE, &format!("/v1/projects/{}", p.id), "org-1", Some(json!({ "confirm": "shop" }))).await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "deleting");
    assert_eq!(body["id"], json!(p.id));
    let deleted: HashSet<DatabaseId> = jobs.deleted.lock().unwrap().iter().copied().collect();
    assert_eq!(deleted, expected);
    assert_eq!(*jobs.project_deletes.lock().unwrap(), vec![p.id]);
    // The project stays visible, as deleting.
    let body: Value = send(&app, Method::GET, &format!("/v1/projects/{}", p.id), "org-1", None).await.json().await.unwrap();
    assert_eq!(body["status"], "deleting");
}

#[tokio::test]
async fn delete_project_refusals_map_to_codes() {
    let jobs = FakeJobs::default();
    let (app, metadata, tenant) = projects_app(jobs.clone()).await;
    let delete = |p: ProjectRecord| {
        let app = &app;
        async move {
            let resp = send(app, Method::DELETE, &format!("/v1/projects/{}", p.id), "org-1", Some(json!({ "confirm": p.slug }))).await;
            let status = resp.status();
            let body: Value = resp.json().await.unwrap();
            (status.as_u16(), body)
        }
    };

    let pending = metadata.create_project(tenant, "Pending").await.unwrap();
    metadata.insert_database(project_database(tenant, pending.id, DatabaseStatus::Provisioning));
    let (status, body) = delete(pending).await;
    assert_eq!((status, code_of(&body)), (409, "database_not_ready"), "{body}");

    let protected = metadata.create_project(tenant, "Protected").await.unwrap();
    let mut db = project_database(tenant, protected.id, DatabaseStatus::Ready);
    db.main.protected = true;
    metadata.insert_database(db);
    let (status, body) = delete(protected).await;
    assert_eq!((status, code_of(&body)), (409, "branch_protected"), "{body}");

    let busy = metadata.create_project(tenant, "Busy").await.unwrap();
    let db = project_database(tenant, busy.id, DatabaseStatus::Ready);
    metadata
        .begin_enable_app_users(&NewProjectAuth {
            project_id: busy.id,
            database_id: db.database_id,
            pg_database: "web1",
            schema_name: "public",
            table_name: "users",
            email_verification: false,
            on_delete: OnDelete::Delete,
        })
        .await
        .unwrap();
    metadata.insert_database(db);
    let (status, body) = delete(busy).await;
    assert_eq!((status, code_of(&body)), (409, "conflict"), "{body}");

    let going = metadata.create_project(tenant, "Going").await.unwrap();
    metadata.begin_project_delete(tenant, going.id).await.unwrap();
    let (status, body) = delete(going).await;
    assert_eq!((status, code_of(&body)), (409, "project_deleting"), "{body}");

    let theirs = metadata.create_project(TenantId::new(), "Theirs").await.unwrap();
    let (status, _) = delete(theirs).await;
    assert_eq!(status, 404);

    assert!(jobs.deleted.lock().unwrap().is_empty());
    assert!(jobs.project_deletes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn create_database_in_deleting_project_409() {
    let (app, metadata, tenant) = projects_app(FakeJobs::default()).await;
    let p = metadata.create_project(tenant, "Going").await.unwrap();
    metadata.begin_project_delete(tenant, p.id).await.unwrap();
    let body = json!({ "project_id": p.id, "name": "demo" });
    let resp = post_create(&app, Some(&app.token_for_org("org-1")), body).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "project_deleting");
}

#[tokio::test]
async fn me_lists_project_name_and_status() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let mut overview = overview_with_database(tenant);
    overview.projects.push(ProjectOverview {
        id: ProjectId::new(),
        name: "Going".into(),
        slug: "going".into(),
        status: ProjectStatus::Deleting,
        databases: vec![],
    });
    metadata.set_overview(overview);
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let body: Value = get_me(&app, Some(&app.token_for_org("org-1"))).await.json().await.unwrap();
    let projects = &body["tenant"]["projects"];
    assert_eq!((projects[0]["name"].as_str(), projects[0]["status"].as_str()), (Some("Default"), Some("active")));
    assert_eq!((projects[1]["name"].as_str(), projects[1]["status"].as_str()), (Some("Going"), Some("deleting")));
}

use chelabase_identity::Pool;
use chelabase_identity::mock::FakeIdentity;
use chelabase_identity::rules::MEMBER_REMOVED_BAN_REASON;

// ---- saved queries

impl Branching {
    fn queries(&self) -> String {
        format!("/v1/databases/{}/sql-queries", self.db.as_uuid())
    }

    /// A request as another user of org-1, a tenant admin too (so as much access as the
    /// default caller).
    async fn send_as(&self, sub: &str, method: Method, path: &str, body: Option<Value>) -> reqwest::Response {
        let tenant = self.metadata.database(self.db).tenant_id;
        if self.metadata.member(tenant, sub).is_none() {
            self.metadata.add_member(tenant, sub, MemberStatus::Active, true);
        }
        let token = self.app.sign(&identity_of(sub), Some(tenant));
        let mut req = reqwest::Client::new()
            .request(method, format!("{}{path}", self.app.base))
            .header(IDENTITY_HEADER, token);
        if let Some(body) = body {
            req = req.json(&body);
        }
        req.send().await.unwrap()
    }

    async fn save(&self, name: &str, sql: &str) -> Value {
        let resp = self
            .send(Method::POST, &self.queries(), Some(serde_json::json!({"name": name, "sql": sql})))
            .await;
        assert_eq!(resp.status(), 201);
        resp.json().await.unwrap()
    }
}

#[tokio::test]
async fn saved_queries_list_create_get_update_delete() {
    let t = branching().await;
    let b = t.save("Beta", "select 2").await;
    let a = t
        .send(
            Method::POST,
            &t.queries(),
            Some(serde_json::json!({"name": "alpha", "sql": "select 1", "pg_database": "app", "role": "anon"})),
        )
        .await;
    assert_eq!(a.status(), 201);
    let a: Value = a.json().await.unwrap();
    assert_eq!(a["version"], 1);
    assert_eq!(a["pg_database"], "app");

    let list: Vec<Value> = t.send(Method::GET, &t.queries(), None).await.json().await.unwrap();
    assert_eq!(list.iter().map(|q| q["name"].as_str().unwrap()).collect::<Vec<_>>(), ["alpha", "Beta"]);
    assert!(list[0].get("sql").is_none(), "the list omits the text");

    let one = format!("{}/{}", t.queries(), a["id"].as_str().unwrap());
    let got: Value = t.send(Method::GET, &one, None).await.json().await.unwrap();
    assert_eq!(got["sql"], "select 1");

    // null clears a hint, an absent field stays.
    let resp = t
        .send(Method::PATCH, &one, Some(serde_json::json!({"version": 1, "sql": "select 3", "role": null})))
        .await;
    assert_eq!(resp.status(), 200);
    let upd: Value = resp.json().await.unwrap();
    assert_eq!((upd["version"].as_i64(), upd["sql"].as_str()), (Some(2), Some("select 3")));
    assert!(upd["role"].is_null());
    assert_eq!(upd["pg_database"], "app");
    assert_eq!(upd["name"], "alpha");

    assert_eq!(t.send(Method::DELETE, &one, None).await.status(), 204);
    assert_eq!(t.send(Method::GET, &one, None).await.status(), 404);
    assert_eq!(t.send(Method::DELETE, &one, None).await.status(), 404);
    let list: Vec<Value> = t.send(Method::GET, &t.queries(), None).await.json().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], b["id"]);
}

// R24: saved queries are shared within the database's project: every Developer lists and reads
// them; only the author or an Admin changes them.
#[tokio::test]
async fn another_users_saved_query_is_visible_but_not_editable() {
    let t = branching().await;
    let q = t.save("mine", "select 1").await;
    t.grant_member("user-2", PlatformRole::Developer, &[]);
    let one = format!("{}/{}", t.queries(), q["id"].as_str().unwrap());
    let got: Value = body_of(t.send_as("user-2", Method::GET, &one, None).await).await;
    assert_eq!(got["sql"], "select 1");
    let list: Vec<Value> = t.send_as("user-2", Method::GET, &t.queries(), None).await.json().await.unwrap();
    assert_eq!(list.len(), 1);
    for (method, body) in [(Method::PATCH, Some(serde_json::json!({"version": 1, "name": "x"}))), (Method::DELETE, None)] {
        let err = error_of(t.send_as("user-2", method, &one, body).await, 403).await;
        assert_eq!((code_of(&err), err["error"]["required"].as_str()), ("forbidden", Some("admin")));
    }
    // Same name for the other user is fine (names are unique per user).
    let resp = t
        .send_as("user-2", Method::POST, &t.queries(), Some(serde_json::json!({"name": "MINE", "sql": "select 9"})))
        .await;
    assert_eq!(resp.status(), 201);
    // A missing query, and one of another database, are 404.
    let missing = format!("{}/{}", t.queries(), uuid::Uuid::new_v4());
    for method in [Method::GET, Method::DELETE] {
        assert_eq!(t.send_as("user-2", method, &missing, None).await.status(), 404);
    }
    let other = format!("/v1/databases/{}/sql-queries/{}", uuid::Uuid::new_v4(), q["id"].as_str().unwrap());
    assert_eq!(t.send(Method::GET, &other, None).await.status(), 404);
}

#[tokio::test]
async fn saved_query_conflict_carries_the_current_row() {
    let t = branching().await;
    let q = t.save("q", "select 1").await;
    let one = format!("{}/{}", t.queries(), q["id"].as_str().unwrap());
    let resp = t.send(Method::PATCH, &one, Some(serde_json::json!({"version": 1, "sql": "select 2"}))).await;
    assert_eq!(resp.status(), 200);
    let body = error_of(
        t.send(Method::PATCH, &one, Some(serde_json::json!({"version": 1, "sql": "select 3"}))).await,
        409,
    )
    .await;
    assert_eq!(code_of(&body), "saved_query_conflict");
    assert_eq!(body["error"]["current"]["sql"], "select 2");
    assert_eq!(body["error"]["current"]["version"], 2);
}

#[tokio::test]
async fn saved_query_name_taken_is_409() {
    let t = branching().await;
    t.save("Report", "select 1").await;
    let other = t.save("other", "select 2").await;
    let body = error_of(
        t.send(Method::POST, &t.queries(), Some(serde_json::json!({"name": " report ", "sql": "select 3"}))).await,
        409,
    )
    .await;
    assert_eq!(code_of(&body), "saved_query_name_taken");
    let one = format!("{}/{}", t.queries(), other["id"].as_str().unwrap());
    let body = error_of(
        t.send(Method::PATCH, &one, Some(serde_json::json!({"version": 1, "name": "REPORT"}))).await,
        409,
    )
    .await;
    assert_eq!(code_of(&body), "saved_query_name_taken");
}

#[tokio::test]
async fn saved_query_cap_is_409() {
    let t = branching().await;
    let now = chrono::Utc::now();
    for i in 0..orchestrator::sql_editor::SAVED_QUERY_CAP {
        let q = SavedQuery {
            id: uuid::Uuid::new_v4(),
            name: format!("q{i}"),
            sql: "select 1".into(),
            pg_database: None,
            role: None,
            version: 1,
            created_at: now,
            updated_at: now,
            author_member_id: None,
        };
        t.metadata.saved_queries.lock().unwrap().push((t.db.as_uuid(), "user-1".into(), q));
    }
    let body = error_of(
        t.send(Method::POST, &t.queries(), Some(serde_json::json!({"name": "one more", "sql": "select 1"}))).await,
        409,
    )
    .await;
    assert_eq!(code_of(&body), "saved_query_cap");
}

#[tokio::test]
async fn saved_query_validation_is_422() {
    let t = branching().await;
    let q = t.save("q", "select 1").await;
    let one = format!("{}/{}", t.queries(), q["id"].as_str().unwrap());
    let long_name = "n".repeat(121);
    let big_sql = "s".repeat(1_048_577);
    let long_hint = "h".repeat(64);
    for body in [
        serde_json::json!({"name": "", "sql": "select 1"}),
        serde_json::json!({"name": "   ", "sql": "select 1"}),
        serde_json::json!({"name": long_name, "sql": "select 1"}),
        serde_json::json!({"name": "a", "sql": ""}),
        serde_json::json!({"name": "a", "sql": big_sql}),
        serde_json::json!({"name": "a", "sql": "select 1", "role": long_hint}),
        serde_json::json!({"name": "a", "sql": "select 1", "pg_database": long_hint}),
        serde_json::json!({"name": "a", "sql": "select 1", "extra": 1}),
        serde_json::json!({"sql": "select 1"}),
    ] {
        let resp = t.send(Method::POST, &t.queries(), Some(body.clone())).await;
        let err = error_of(resp, 422).await;
        assert_eq!(code_of(&err), "invalid_request", "{body}");
    }
    for body in [
        serde_json::json!({"version": 1, "name": ""}),
        serde_json::json!({"version": 1, "sql": ""}),
        serde_json::json!({"version": 1, "role": long_hint}),
        serde_json::json!({"name": "x"}),
        serde_json::json!({"version": 1, "bogus": true}),
    ] {
        let resp = t.send(Method::PATCH, &one, Some(body.clone())).await;
        let err = error_of(resp, 422).await;
        assert_eq!(code_of(&err), "invalid_request", "{body}");
    }
}

#[tokio::test]
async fn empty_saved_query_hints_are_null() {
    let t = branching().await;
    let resp = t
        .send(
            Method::POST,
            &t.queries(),
            Some(serde_json::json!({"name": "q", "sql": "select 1", "pg_database": "", "role": ""})),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let q: Value = resp.json().await.unwrap();
    assert!(q["pg_database"].is_null() && q["role"].is_null(), "{q}");
    let one = format!("{}/{}", t.queries(), q["id"].as_str().unwrap());
    let resp = t
        .send(Method::PATCH, &one, Some(serde_json::json!({"version": 1, "pg_database": "app", "role": "anon"})))
        .await;
    assert_eq!(resp.status(), 200);
    let resp = t
        .send(Method::PATCH, &one, Some(serde_json::json!({"version": 2, "pg_database": "", "role": ""})))
        .await;
    assert_eq!(resp.status(), 200);
    let q: Value = resp.json().await.unwrap();
    assert!(q["pg_database"].is_null() && q["role"].is_null(), "{q}");
}

#[tokio::test]
async fn saved_query_with_a_nul_byte_is_422() {
    let t = branching().await;
    let resp = t
        .send(Method::POST, &t.queries(), Some(serde_json::json!({"name": "q", "sql": "select 1\u{0}"})))
        .await;
    assert_eq!(code_of(&error_of(resp, 422).await), "invalid_request");
    let q = t.save("q", "select 1").await;
    let one = format!("{}/{}", t.queries(), q["id"].as_str().unwrap());
    let resp = t.send(Method::PATCH, &one, Some(serde_json::json!({"version": 1, "sql": "\u{0}"}))).await;
    assert_eq!(code_of(&error_of(resp, 422).await), "invalid_request");
}

#[tokio::test]
async fn saved_queries_of_a_deleted_database_are_404() {
    let t = branching().await;
    let q = t.save("q", "select 1").await;
    t.metadata
        .databases
        .lock()
        .unwrap()
        .get_mut(&t.db)
        .unwrap()
        .status = DatabaseStatus::Deleted;
    let one = format!("{}/{}", t.queries(), q["id"].as_str().unwrap());
    assert_eq!(t.send(Method::GET, &t.queries(), None).await.status(), 404);
    assert_eq!(t.send(Method::GET, &one, None).await.status(), 404);
    assert_eq!(t.send(Method::DELETE, &one, None).await.status(), 404);
}

// ---- SQL editor: run, cancel, schema ---------------------------------------------------

/// A ready database with the console's seeded Postgres databases (`web1` main and served,
/// `analytics` and `scratch` made in SQL, the system `postgres`) and a plain API role
/// `editor` on main.
async fn sql_app() -> Branching {
    let t = branching().await;
    let main = t.seeded();
    let ep = t.metadata.branch(main).endpoint_id.unwrap();
    t.metadata.add_role(&ep, "postgres", RoleOrigin::Owner, true, false);
    t.metadata.add_role(&ep, "editor", RoleOrigin::Api, false, false);
    t
}

impl Branching {
    async fn run_sql(&self, branch: BranchId, body: Value) -> reqwest::Response {
        self.send(Method::POST, &format!("{}/sql", self.branch(branch)), Some(body)).await
    }

    async fn sql_schema(&self, branch: BranchId, query: &str) -> reqwest::Response {
        self.send(Method::GET, &format!("{}/sql/schema{query}", self.branch(branch)), None).await
    }

    async fn cancel_run(&self, branch: BranchId, run_id: uuid::Uuid) -> reqwest::Response {
        self.send(Method::POST, &format!("{}/sql/runs/{run_id}/cancel", self.branch(branch)), None).await
    }

    fn set_protected(&self, branch: BranchId) {
        self.metadata.branches.lock().unwrap().get_mut(&branch).unwrap().protected = true;
    }
}

fn run_body(sql: &str) -> Value {
    json!({ "run_id": uuid::Uuid::new_v4(), "sql": sql })
}

/// `body` with `key` set to `value`.
fn with(mut body: Value, key: &str, value: Value) -> Value {
    body[key] = value;
    body
}

#[tokio::test]
async fn run_validation_is_422() {
    let t = sql_app().await;
    let main = t.main();
    let big = "x".repeat(1_048_577);
    for body in [
        run_body(""),
        run_body(&big),
        with(run_body("select 1"), "timeout_ms", json!(99)),
        with(run_body("select 1"), "timeout_ms", json!(900_001)),
        with(run_body("select 1"), "bogus", json!(true)),
        with(run_body("select 1"), "role", json!("")),
        json!({ "sql": "select 1" }),
        with(run_body("select 1"), "run_id", json!("not-a-uuid")),
    ] {
        let resp = t.run_sql(main, body.clone()).await;
        let err = error_of(resp, 422).await;
        assert_eq!(code_of(&err), "invalid_request", "{}", &body.to_string()[..body.to_string().len().min(200)]);
    }
    // The bounds themselves are accepted (and get as far as the compute lookup).
    for timeout in [100, 900_000] {
        let resp = t.run_sql(main, with(run_body("select 1"), "timeout_ms", json!(timeout))).await;
        assert_eq!(code_of(&error_of(resp, 503).await), "unavailable");
    }
    assert_eq!(t.compute_lookups(), 2);
}

#[tokio::test]
async fn run_with_a_nul_byte_is_422() {
    let t = sql_app().await;
    let main = t.main();
    for confirm in [false, true] {
        let body = with(run_body("select 1;\u{0} select 2"), "confirm", json!(confirm));
        let err = error_of(t.run_sql(main, body).await, 422).await;
        assert_eq!(code_of(&err), "invalid_request");
        assert!(message_of(&err).contains("NUL"), "{err}");
    }
    assert_eq!(t.compute_lookups(), 0);
}

#[tokio::test]
async fn run_unknown_database_is_404() {
    let t = sql_app().await;
    let main = t.main();
    for body in [
        with(run_body("select 1"), "database", json!("nope")),
        with(run_body("select 1"), "database", json!("postgres")),
    ] {
        let resp = t.run_sql(main, body.clone()).await;
        assert_eq!(code_of(&error_of(resp, 404).await), "not_found", "{body}");
    }
    assert_eq!(t.compute_lookups(), 0);
    // A database made in SQL (not served by the data API) and a known role are accepted.
    let ok = with(with(run_body("select 1"), "database", json!("analytics")), "role", json!("editor"));
    assert_eq!(code_of(&error_of(t.run_sql(main, ok).await, 503).await), "unavailable");
}

#[tokio::test]
async fn run_reserved_role_is_422() {
    let t = sql_app().await;
    let main = t.main();
    let ep = t.metadata.branch(main).endpoint_id.unwrap();
    // Even when listed (a resync might show one), a reserved name is refused.
    t.metadata.add_role(&ep, "cloud_admin", RoleOrigin::Sql, true, false);
    for role in ["cloud_admin", "neon_superuser", "authenticator", "chelabase_admin", "pg_read_all_data", "PG_monitor"] {
        let resp = t.run_sql(main, with(run_body("select 1"), "role", json!(role))).await;
        assert_eq!(code_of(&error_of(resp, 422).await), "invalid_request", "{role}");
    }
    assert_eq!(t.compute_lookups(), 0);
}

#[tokio::test]
async fn risky_statement_needs_confirm() {
    let t = sql_app().await;
    let main = t.main();
    let resp = t.run_sql(main, run_body("SELECT 1; DROP TABLE notes")).await;
    let err = error_of(resp, 409).await;
    assert_eq!(code_of(&err), "sql_confirm_required");
    let statements = err["error"]["statements"].as_array().unwrap();
    assert_eq!(statements.len(), 1, "{err}");
    assert_eq!(statements[0]["index"], 1);
    assert_eq!(statements[0]["kind"], "DropStmt");
    assert_eq!(statements[0]["reason"], "risky");
    assert_eq!(statements[0]["text_preview"], "DROP TABLE notes");
    // Nothing was connected.
    assert_eq!(t.compute_lookups(), 0);
    // `confirm: false` is the same as absent.
    let resp = t.run_sql(main, with(run_body("TRUNCATE notes"), "confirm", json!(false))).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "sql_confirm_required");
    assert_eq!(t.compute_lookups(), 0);
}

#[tokio::test]
async fn protected_branch_holds_inserts() {
    let t = sql_app().await;
    let main = t.main();
    t.set_protected(main);
    let resp = t.run_sql(main, run_body("INSERT INTO notes (body) VALUES ('x')")).await;
    let err = error_of(resp, 409).await;
    assert_eq!(code_of(&err), "sql_confirm_required");
    assert_eq!(err["error"]["statements"][0]["kind"], "InsertStmt");
    assert_eq!(err["error"]["statements"][0]["reason"], "protected");
    assert_eq!(t.compute_lookups(), 0);
    // A read passes the guard.
    let resp = t.run_sql(main, run_body("SELECT * FROM notes")).await;
    assert_eq!(code_of(&error_of(resp, 503).await), "unavailable");
}

#[tokio::test]
async fn confirm_skips_the_guard() {
    let t = sql_app().await;
    let main = t.main();
    t.set_protected(main);
    let resp = t.run_sql(main, with(run_body("DROP TABLE notes"), "confirm", json!(true))).await;
    // Past the guard: the compute isn't running in the fake.
    assert_eq!(code_of(&error_of(resp, 503).await), "unavailable");
    assert_eq!(t.compute_lookups(), 1);
}

// ---- Act-as through member roles (members and roles, T9) ----

impl Branching {
    /// A member role row for `member` on `branch`, as the reconciler records it, sealed with
    /// the orchestrator's key (password `pw`).
    fn member_role_row(&self, member: uuid::Uuid, branch: BranchId, applied: &[&str]) -> MemberBranchRole {
        let role_name = chelabase_types::role_name::member_role_name(member);
        let password_enc =
            chelabase_secrets::seal_member_role_password(&self.app.key, branch.as_uuid(), &role_name, "pw").unwrap();
        MemberBranchRole {
            member_id: member,
            branch_id: branch.as_uuid(),
            role_name,
            password_enc,
            applied_roles: applied.iter().map(|r| r.to_string()).collect(),
        }
    }

    fn put_member_role(&self, row: MemberBranchRole) {
        let mut rows = self.metadata.member_roles.lock().unwrap();
        rows.retain(|r| (r.member_id, r.branch_id) != (row.member_id, row.branch_id));
        rows.push(row);
    }

    /// A reconcile writes `rows` (as the worker would) and answers with them.
    fn on_reconcile(&self, rows: Vec<MemberBranchRole>) {
        *self.jobs.member_rows_on_reconcile.lock().unwrap() = Some((self.metadata.member_roles.clone(), rows));
    }

    fn reconciles(&self) -> Vec<MemberRolesReconcileRequest> {
        self.jobs.member_reconciles.lock().unwrap().clone()
    }

    /// The member row of `sub` (made by its first request).
    fn member_of_sub(&self, sub: &str) -> uuid::Uuid {
        self.metadata.members.lock().unwrap().iter().find(|m| m.identity_user_id == sub).expect("a member row").id
    }

    async fn run_as(&self, sub: &str, branch: BranchId, role: Option<&str>) -> reqwest::Response {
        let body = with(run_body("select 1"), "role", json!(role));
        self.send_as(sub, Method::POST, &format!("{}/sql", self.branch(branch)), Some(body)).await
    }

    async fn schema_as(&self, sub: &str, branch: BranchId, query: &str) -> reqwest::Response {
        self.send_as(sub, Method::GET, &format!("{}/sql/schema{query}", self.branch(branch)), None).await
    }
}

/// `sql_app` with group roles `billing`, `billing_admin` (which can become `billing`) and
/// `sales` on main, and a Developer `dev` granted `billing_admin` and `sales`.
async fn act_as_app() -> (Branching, uuid::Uuid) {
    let t = sql_app().await;
    let ep = t.metadata.branch(t.main()).endpoint_id.unwrap();
    t.metadata.add_group_role(&ep, "billing", &[]);
    t.metadata.add_group_role(&ep, "billing_admin", &["billing"]);
    t.metadata.add_group_role(&ep, "sales", &[]);
    let dev = t.grant_member("dev", PlatformRole::Developer, &["billing_admin", "sales"]);
    (t, dev)
}

// A named role runs on the member role's own connection: no owner password is needed, and a
// fresh row (the granted roles, a role reached through one's chain) asks for no reconcile.
#[tokio::test]
async fn sql_run_as_role_uses_member_session() {
    let (t, dev) = act_as_app().await;
    let main = t.main();
    t.put_member_role(t.member_role_row(dev, main, &["billing_admin", "sales"]));
    // Without an owner password the owner's connection can't open: only the member's does.
    t.seal_owner_password(None);
    for role in ["billing_admin", "billing", "sales"] {
        let resp = t.run_as("dev", main, Some(role)).await;
        assert_eq!(code_of(&error_of(resp, 503).await), "unavailable", "{role}");
    }
    // The schema read takes the same session for `?role=`.
    let resp = t.schema_as("dev", main, "?role=billing").await;
    assert_eq!(code_of(&error_of(resp, 503).await), "unavailable");
    assert!(t.reconciles().is_empty(), "{:?}", t.reconciles());
    assert_eq!(t.compute_lookups(), 4);
    // The owner (no role) is still Admin-only, on the owner's connection.
    let err = error_of(t.run_as("dev", main, None).await, 403).await;
    assert_eq!(err["error"]["required"], "owner");
    let err = error_of(t.schema_as("dev", main, "").await, 403).await;
    assert_eq!(err["error"]["required"], "owner");
    let resp = t.run_as(DEFAULT_SUB, main, None).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "owner_password_unknown");
    // `anon` works like any other role, through membership: no grant to the owner. R18: for a
    // grant naming it (an Admin's every role doesn't hold it).
    let anon_dev = t.grant_member("anon_dev", PlatformRole::Developer, &["anon", "billing"]);
    t.put_member_role(t.member_role_row(anon_dev, main, &["anon", "billing"]));
    let resp = t.run_as("anon_dev", main, Some("anon")).await;
    assert_eq!(code_of(&error_of(resp, 503).await), "unavailable");
    assert!(t.reconciles().is_empty(), "{:?}", t.reconciles());
}

// A missing row, a granted role the row lacks, a role the row holds but the grant no longer
// lists (revoked), and a password that won't open: each asks the worker first, for this member.
#[tokio::test]
async fn sql_run_reconciles_when_stale() {
    let (t, dev) = act_as_app().await;
    let main = t.main();
    t.seal_owner_password(None);
    let fresh = t.member_role_row(dev, main, &["billing_admin", "sales"]);
    let mut unopenable = fresh.clone();
    unopenable.password_enc = t.member_role_row(dev, BranchId::new(), &[]).password_enc;
    let stale: Vec<(&str, Option<MemberBranchRole>)> = vec![
        ("missing", None),
        ("lacks the role", Some(t.member_role_row(dev, main, &["sales"]))),
        ("holds a revoked role", Some(t.member_role_row(dev, main, &["billing_admin", "sales", "billing"]))),
        ("password won't open", Some(unopenable)),
    ];
    t.on_reconcile(vec![fresh.clone()]);
    for (i, (case, row)) in stale.into_iter().enumerate() {
        t.metadata.member_roles.lock().unwrap().clear();
        if let Some(row) = row {
            t.put_member_role(row);
        }
        let resp = t.run_as("dev", main, Some("billing")).await;
        assert_eq!(code_of(&error_of(resp, 503).await), "unavailable", "{case}");
        let asked = t.reconciles();
        assert_eq!(asked.len(), i + 1, "{case}");
        assert_eq!(
            asked[i],
            MemberRolesReconcileRequest { branch_id: main.as_uuid(), member_id: Some(dev), from_ddl: false },
            "{case}"
        );
        // The reconcile's row is the one used: the next run is fresh.
        let resp = t.run_as("dev", main, Some("billing")).await;
        assert_eq!(resp.status(), 503, "{case}");
        assert_eq!(t.reconciles().len(), i + 1, "{case}");
    }
    // The schema read checks the same way.
    t.metadata.member_roles.lock().unwrap().clear();
    let resp = t.schema_as("dev", main, "?role=sales").await;
    assert_eq!(resp.status(), 503);
    assert_eq!(t.reconciles().len(), 5);
}

#[tokio::test]
async fn sql_run_reconcile_failure_is_503() {
    let (t, dev) = act_as_app().await;
    let main = t.main();
    for answer in [
        Ok(MemberRolesReply::Failed { message: "psql failed".into() }),
        Err(ResetError::Timeout),
        Err(ResetError::Transport("no worker listening".into())),
        // Done, but nothing recorded for this member.
        Ok(MemberRolesReply::Done { roles: vec![MemberRoleApplied {
            member_id: dev,
            role_name: "cb_m_x".into(),
            applied_roles: vec!["sales".into()],
        }] }),
    ] {
        *t.jobs.member_reconcile_reply.lock().unwrap() = Some(answer.clone());
        for resp in [t.run_as("dev", main, Some("sales")).await, t.schema_as("dev", main, "?role=sales").await] {
            assert_eq!(resp.status(), 503, "{answer:?}");
            assert_eq!(resp.headers()["retry-after"], "2", "{answer:?}");
            let err: Value = resp.json().await.unwrap();
            assert_eq!(code_of(&err), "member_role_reconcile_failed", "{answer:?}");
        }
    }
    assert_eq!(t.compute_lookups(), 0);
}

// A role the member role can't become even after a reconcile (missing on the branch, or left
// out by the reconciler) is 403 `forbidden{required: "role:<name>"}`.
#[tokio::test]
async fn act_as_missing_role_is_forbidden() {
    let (t, dev) = act_as_app().await;
    let main = t.main();
    // A tenant admin may name any role; `ghost` doesn't exist on the branch.
    t.run_as(DEFAULT_SUB, main, None).await; // makes the member row (an owner run: one lookup)
    let admin = t.member_of_sub(DEFAULT_SUB);
    let lookups = t.compute_lookups();
    t.on_reconcile(vec![t.member_role_row(admin, main, &["billing", "sales"])]);
    for (sub, role) in [(DEFAULT_SUB, "ghost"), (DEFAULT_SUB, "Ghost2")] {
        let err = error_of(t.run_as(sub, main, Some(role)).await, 403).await;
        assert_eq!(code_of(&err), "forbidden", "{role}");
        assert_eq!(err["error"]["required"], format!("role:{role}"), "{role}");
        let err = error_of(t.schema_as(sub, main, &format!("?role={role}")).await, 403).await;
        assert_eq!(err["error"]["required"], format!("role:{role}"), "{role}");
    }
    // M10: a Developer granted `ghost` itself (a role the grant names but the branch lacks): the
    // grant makes it listed, the branch doesn't have it, so the member role never does.
    t.grant_member("gus", PlatformRole::Developer, &["ghost"]);
    let gus = t.member_of_sub("gus");
    t.on_reconcile(vec![t.member_role_row(gus, main, &["billing"])]);
    let err = error_of(t.run_as("gus", main, Some("ghost")).await, 403).await;
    assert_eq!((code_of(&err), err["error"]["required"].as_str()), ("forbidden", Some("role:ghost")));
    let err = error_of(t.schema_as("gus", main, "?role=ghost").await, 403).await;
    assert_eq!(err["error"]["required"], "role:ghost");
    // The reconciler left `sales` out of the Developer's role (its chain became unsafe).
    t.on_reconcile(vec![t.member_role_row(dev, main, &["billing_admin"])]);
    let err = error_of(t.run_as("dev", main, Some("sales")).await, 403).await;
    assert_eq!(err["error"]["required"], "role:sales");
    // The worker answered `Done` without this member: no member role at all.
    *t.jobs.member_reconcile_reply.lock().unwrap() = Some(Ok(MemberRolesReply::Done { roles: vec![] }));
    t.metadata.member_roles.lock().unwrap().clear();
    let err = error_of(t.run_as("dev", main, Some("billing")).await, 403).await;
    assert_eq!(err["error"]["required"], "role:billing");
    // None of these reached the compute.
    assert_eq!(t.compute_lookups(), lookups);
}

// R16 and R22: a LOGIN or owner-level role is never in a member role. An Admin acts as it on the
// owner's connection (`SET ROLE`), as before; a Developer can't name it at all.
#[tokio::test]
async fn admin_acts_as_login_role_through_the_owner() {
    let (t, _dev) = act_as_app().await;
    let main = t.main();
    t.seal_owner_password(None);
    // `editor` is a LOGIN role: the owner's connection, which has no password here.
    let resp = t.run_as(DEFAULT_SUB, main, Some("editor")).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "owner_password_unknown");
    let resp = t.schema_as(DEFAULT_SUB, main, "?role=editor").await;
    assert_eq!(code_of(&error_of(resp, 409).await), "owner_password_unknown");
    assert!(t.reconciles().is_empty());
    t.grant_member("dev2", PlatformRole::Developer, &["editor"]);
    let err = error_of(t.run_as("dev2", main, Some("editor")).await, 403).await;
    assert_eq!(err["error"]["required"], "admin");
}

// R18 (user, 2026-10-14): Run as / Act as never offer the data API's roles unless a grant
// names them: an Admin (every role, R17) is refused anon and authenticated; a member whose grant
// names `anon` may run as it. Previewing what the data API sees is TE-B2's read-only View as.
#[tokio::test]
async fn admins_run_as_the_data_api_roles_only_when_granted_by_name() {
    let (t, _dev) = act_as_app().await;
    let main = t.main();
    for role in ["anon", "authenticated"] {
        let err = error_of(t.run_as(DEFAULT_SUB, main, Some(role)).await, 403).await;
        assert_eq!(err["error"]["required"], format!("role:{role}"), "{role}");
    }
    let dev3 = t.grant_member("dev3", PlatformRole::Developer, &["anon"]);
    // The member's login already holds anon on this branch (as the reconciler leaves it).
    t.put_member_role(t.member_role_row(dev3, main, &["anon"]));
    let resp = t.run_as("dev3", main, Some("anon")).await;
    assert_ne!(resp.status(), 403, "a grant naming anon may run as it");
}

// Members hub T2 (D1, MH-1): act-as checks the route's minimum against the requested role's
// own level, read through `grant_sources`. `mixed` is a Developer through their own grant
// (`sales`); `billing` comes only from a Viewer team grant, so it stays a Viewer-level role.
#[tokio::test]
async fn act_as_uses_the_roles_level_not_the_members() {
    let (t, _dev) = act_as_app().await;
    let main = t.main();
    let mixed = t.grant_member("mixed", PlatformRole::Developer, &["sales"]);
    let project = t.metadata.database(t.db).project_id;
    t.metadata.team_grant(
        mixed,
        project,
        Grant {
            platform_role: PlatformRole::Viewer,
            roles: chelabase_types::access::RoleSet::Only([PgRole::Named("billing".into())].into_iter().collect()),
        },
    );
    // The member's login holds both (the reconciler grants every effective role).
    t.put_member_role(t.member_role_row(mixed, main, &["billing", "sales"]));
    // Developer routes (SQL run, SQL schema) as `billing`: refused on billing's own level.
    let err = error_of(t.run_as("mixed", main, Some("billing")).await, 403).await;
    assert_eq!((code_of(&err), err["error"]["required"].as_str()), ("forbidden", Some("role:billing")));
    let err = error_of(t.schema_as("mixed", main, "?role=billing").await, 403).await;
    assert_eq!(err["error"]["required"], "role:billing");
    // As `sales` (held at Developer) the access check passes (what follows needs a compute
    // the fakes don't have; only the access refusal matters here).
    let not_refused = |what: &'static str| {
        move |resp: reqwest::Response| async move {
            if resp.status() == 403 {
                let err: Value = resp.json().await.unwrap();
                assert_ne!(code_of(&err), "forbidden", "{what}: {err}");
            }
        }
    };
    not_refused("run as sales")(t.run_as("mixed", main, Some("sales")).await).await;
    // A Viewer route (the console's schema) as `billing` passes the access check.
    let resp = t.send_as("mixed", Method::GET, &format!("{}/console/schema?role=billing", t.branch(main)), None).await;
    not_refused("console schema as billing")(resp).await;
    // None of the refusals asked for a reconcile.
    assert!(t.reconciles().is_empty(), "{:?}", t.reconciles());
}

// Members hub T2 (P14): a suspended caller's refusal is 403 `member_suspended`.
#[test]
fn a_suspended_refusal_is_403_member_suspended() {
    let e = orchestrator::access::deny_to_api(chelabase_types::access::Deny::Suspended);
    assert_eq!((e.status, e.code), (403, chelabase_types::api_error::ErrorCode::MemberSuspended));
    assert_eq!(e.body()["error"]["code"], "member_suspended");
}

// I2 (T9 review): an effective Admin acts as any role of the branch. When the member role can't
// hold it (a chain through a reserved role, R23; or one the reconciler left out, e.g. an ADMIN
// chain 6a can't see), the owner's connection and `SET ROLE` serve it instead.
#[tokio::test]
async fn admin_falls_back_to_the_owner_when_the_member_role_cant_hold_the_role() {
    let (t, _dev) = act_as_app().await;
    let main = t.main();
    let ep = t.metadata.branch(main).endpoint_id.unwrap();
    t.metadata.add_group_role(&ep, "analyst", &["pg_read_all_data"]);
    t.seal_owner_password(None);
    // Seen in the graph: straight to the owner, no reconcile.
    let resp = t.run_as(DEFAULT_SUB, main, Some("analyst")).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "owner_password_unknown");
    let resp = t.schema_as(DEFAULT_SUB, main, "?role=analyst").await;
    assert_eq!(code_of(&error_of(resp, 409).await), "owner_password_unknown");
    assert!(t.reconciles().is_empty());
    // Left out by the reconciler: after it says so, the owner.
    let admin = t.member_of_sub(DEFAULT_SUB);
    t.on_reconcile(vec![t.member_role_row(admin, main, &["billing"])]);
    let resp = t.run_as(DEFAULT_SUB, main, Some("sales")).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "owner_password_unknown");
    assert_eq!(t.reconciles().len(), 1);
    // A project Admin (not a tenant admin) the same.
    let adm = t.grant_member("adm", PlatformRole::Admin, &["analyst"]);
    let resp = t.run_as("adm", main, Some("analyst")).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "owner_password_unknown");
    assert!(!t.reconciles().iter().any(|r| r.member_id == Some(adm)));
}

// R31: an act-as run records the role it acts as on the member's row before it starts (a
// reconcile hands leftovers to it); a schema read and an owner run record nothing.
#[tokio::test]
async fn act_as_run_records_the_acted_role() {
    let (t, dev) = act_as_app().await;
    let main = t.main();
    t.put_member_role(t.member_role_row(dev, main, &["billing_admin", "sales"]));
    let recorded = || t.metadata.acted_roles.lock().unwrap().get(&(dev, main.as_uuid())).cloned();
    let resp = t.schema_as("dev", main, "?role=sales").await;
    assert_eq!(resp.status(), 503);
    assert_eq!(recorded(), None);
    for role in ["sales", "billing"] {
        let resp = t.run_as("dev", main, Some(role)).await;
        assert_eq!(code_of(&error_of(resp, 503).await), "unavailable", "{role}");
        assert_eq!(recorded().as_deref(), Some(role));
    }
    let resp = t.run_as(DEFAULT_SUB, main, None).await;
    assert_eq!(resp.status(), 503);
    assert_eq!(t.metadata.acted_roles.lock().unwrap().len(), 1);
}

// M5 (T9 review), single rule: below Admin, a role whose chain the graph already refuses (here
// through a reserved role) is 403 `admin` from the access check, without asking the worker.
#[tokio::test]
async fn developer_poisoned_chain_is_403_without_reconcile() {
    let (t, _dev) = act_as_app().await;
    let main = t.main();
    let ep = t.metadata.branch(main).endpoint_id.unwrap();
    t.metadata.add_group_role(&ep, "analyst", &["pg_read_all_data"]);
    t.grant_member("dev3", PlatformRole::Developer, &["analyst"]);
    for resp in [t.run_as("dev3", main, Some("analyst")).await, t.schema_as("dev3", main, "?role=analyst").await] {
        let err = error_of(resp, 403).await;
        assert_eq!((code_of(&err), err["error"]["required"].as_str()), ("forbidden", Some("admin")));
    }
    assert!(t.reconciles().is_empty());
}

// M1 (T9 review): a row holding a role that has since become unsafe (LOGIN, or a chain through
// a reserved or owner-level role) is stale, even when the requested role is reachable.
#[tokio::test]
async fn a_row_holding_a_role_that_became_unsafe_is_stale() {
    let (t, dev) = act_as_app().await;
    let main = t.main();
    let ep = t.metadata.branch(main).endpoint_id.unwrap();
    t.seal_owner_password(None);
    let fresh = t.member_role_row(dev, main, &["billing_admin", "sales"]);
    t.on_reconcile(vec![t.member_role_row(dev, main, &["billing_admin"])]);
    for (i, change) in ["login", "reserved chain"].into_iter().enumerate() {
        {
            let mut roles = t.metadata.roles.lock().unwrap();
            let sales = roles.get_mut(&ep).unwrap().iter_mut().find(|r| r.name == "sales").unwrap();
            match change {
                "login" => sales.login = true,
                _ => {
                    sales.login = false;
                    sales.attributes.member_of = vec!["pg_monitor".into()];
                }
            }
        }
        t.put_member_role(fresh.clone());
        let resp = t.run_as("dev", main, Some("billing")).await;
        assert_eq!(code_of(&error_of(resp, 503).await), "unavailable", "{change}");
        assert_eq!(t.reconciles().len(), i + 1, "{change}");
    }
}

#[tokio::test]
async fn fifth_run_is_429() {
    let t = sql_app().await;
    let main = t.main();
    let runs = t.app.state.sql_editor.runs();
    let held: Vec<_> = (0..4)
        .map(|_| runs.reserve("user-1", uuid::Uuid::new_v4(), None, BranchId::new(), "web1").unwrap())
        .collect();
    let resp = t.run_sql(main, run_body("select 1")).await;
    assert_eq!(resp.status(), 429);
    assert_eq!(resp.headers()["retry-after"], "2");
    assert_eq!(code_of(&resp.json().await.unwrap()), "sql_run_limit");
    assert_eq!(t.compute_lookups(), 0);
    drop(held);
    let resp = t.run_sql(main, run_body("select 1")).await;
    assert_eq!(resp.status(), 503);
}

#[tokio::test]
async fn duplicate_run_id_is_409() {
    let t = sql_app().await;
    let main = t.main();
    let id = uuid::Uuid::new_v4();
    let _held = t.app.state.sql_editor.runs().reserve("user-1", id, None, main, "web1").unwrap();
    let resp = t.run_sql(main, json!({ "run_id": id, "sql": "select 1" })).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "run_id_in_use");
    // Another user may use the same id.
    let resp = t
        .send_as("user-2", Method::POST, &format!("{}/sql", t.branch(main)), Some(json!({ "run_id": id, "sql": "select 1" })))
        .await;
    assert_eq!(resp.status(), 503);
}

#[tokio::test]
async fn cancel_unknown_run_is_404() {
    let t = sql_app().await;
    let main = t.main();
    let resp = t.cancel_run(main, uuid::Uuid::new_v4()).await;
    let err = error_of(resp, 404).await;
    assert_eq!(code_of(&err), "not_found");
    assert_eq!(message_of(&err), "run not found");
    // A run in flight is cancelled: 202 `{}` and its token fires.
    let id = uuid::Uuid::new_v4();
    let held = t.app.state.sql_editor.runs().reserve("user-1", id, None, main, "web1").unwrap();
    // Another Developer's cancel doesn't reach it.
    t.grant_member("user-2", PlatformRole::Developer, &[]);
    let resp = t
        .send_as("user-2", Method::POST, &format!("{}/sql/runs/{id}/cancel", t.branch(main)), None)
        .await;
    assert_eq!(resp.status(), 403);
    assert!(!held.token().is_cancelled());
    let resp = t.cancel_run(main, id).await;
    assert_eq!(resp.status(), 202);
    assert_eq!(resp.json::<Value>().await.unwrap(), json!({}));
    assert!(held.token().is_cancelled());
    // The branch must still be the caller's.
    let backup = t.backup(main);
    assert_eq!(code_of(&error_of(t.cancel_run(backup, id).await, 409).await), "backup_branch");
    let other = uuid::Uuid::new_v4();
    let resp = t.send(Method::POST, &format!("{}/{other}/sql/runs/{id}/cancel", t.branches()), None).await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn owner_password_unknown_is_409() {
    let t = sql_app().await;
    let main = t.main();
    t.seal_owner_password(None);
    let id = uuid::Uuid::new_v4();
    let resp = t.run_sql(main, json!({ "run_id": id, "sql": "select 1" })).await;
    let err = error_of(resp, 409).await;
    assert_eq!(code_of(&err), "owner_password_unknown");
    assert_eq!(t.compute_lookups(), 0);
    // The slot was given back.
    assert!(t.app.state.sql_editor.runs().reserve("user-1", id, None, main, "web1").is_ok());
}

#[tokio::test]
async fn run_refuses_backups_unready_branches_and_strangers() {
    let t = sql_app().await;
    let main = t.main();
    let backup = t.backup(main);
    assert_eq!(code_of(&error_of(t.run_sql(backup, run_body("select 1")).await, 409).await), "backup_branch");
    let pending = t.add("dev", main, BranchKind::Normal, BranchStatus::Pending);
    assert_eq!(code_of(&error_of(t.run_sql(pending, run_body("select 1")).await, 409).await), "branch_not_ready");
    // The body is read before the branch checks: the access check needs its role and database.
    assert_eq!(code_of(&error_of(t.run_sql(pending, json!({})).await, 422).await), "invalid_request");
    let resp = t
        .send(Method::POST, &format!("{}/{}/sql", t.branches(), uuid::Uuid::new_v4()), Some(run_body("select 1")))
        .await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn sql_confirm_required_is_audited_without_sql() {
    use sha2::Digest;
    let logs = Logs::default();
    let _guard = logs.capture();
    let t = sql_app().await;
    let main = t.main();
    let sql = "DROP TABLE secret_table_4111";
    let id = uuid::Uuid::new_v4();
    let resp = t.run_sql(main, json!({ "run_id": id, "sql": sql })).await;
    assert_eq!(resp.status(), 409);
    let text = logs.text();
    let audit: Vec<&str> = text.lines().filter(|l| l.contains("sql run")).collect();
    assert_eq!(audit.len(), 1, "{text}");
    let line = audit[0];
    let sha = hex::encode(sha2::Sha256::digest(sql.as_bytes()));
    for field in [
        "user=user-1".to_string(),
        format!("database_id={}", t.db.as_uuid()),
        format!("branch_id={}", main.as_uuid()),
        "pg_database=web1".to_string(),
        "role=owner".to_string(),
        format!("run_id={id}"),
        "outcome=confirm_required".to_string(),
        "held=DropStmt".to_string(),
        format!("sql_sha256={sha}"),
        "protected=false".to_string(),
        "confirmed=false".to_string(),
    ] {
        assert!(line.contains(&field), "{field} missing from {line}");
    }
    assert!(!text.contains("secret_table_4111"), "SQL reached the log:\n{text}");
}

#[tokio::test]
async fn sql_schema_refusals() {
    let t = sql_app().await;
    let main = t.main();
    for query in ["?database=nope", "?database=postgres"] {
        assert_eq!(code_of(&error_of(t.sql_schema(main, query).await, 404).await), "not_found", "{query}");
    }
    assert_eq!(t.compute_lookups(), 0);
    // No compute running.
    assert_eq!(code_of(&error_of(t.sql_schema(main, "").await, 503).await), "unavailable");
    assert_eq!(code_of(&error_of(t.sql_schema(main, "?database=analytics").await, 503).await), "unavailable");
    t.seal_owner_password(None);
    assert_eq!(code_of(&error_of(t.sql_schema(main, "").await, 409).await), "owner_password_unknown");
    let backup = t.backup(main);
    assert_eq!(code_of(&error_of(t.sql_schema(backup, "").await, 409).await), "backup_branch");
}

#[tokio::test]
async fn sql_schema_counts_against_the_run_limits() {
    let t = sql_app().await;
    let main = t.main();
    let runs = t.app.state.sql_editor.runs();
    let held: Vec<_> = (0..4)
        .map(|_| runs.reserve("user-1", uuid::Uuid::new_v4(), None, BranchId::new(), "web1").unwrap())
        .collect();
    let resp = t.sql_schema(main, "").await;
    assert_eq!(resp.status(), 429);
    assert_eq!(resp.headers()["retry-after"], "2");
    assert_eq!(code_of(&resp.json().await.unwrap()), "sql_run_limit");
    assert_eq!(t.compute_lookups(), 0);
    drop(held);
    // A schema read's slot is given back when it ends.
    assert_eq!(code_of(&error_of(t.sql_schema(main, "").await, 503).await), "unavailable");
    let held: Vec<_> = (0..4)
        .map(|_| runs.reserve("user-1", uuid::Uuid::new_v4(), None, main, "web1").unwrap())
        .collect();
    drop(held);
}

// ---- the SQL editor against a real Postgres (scratch database, scratch owner)

/// Reads an NDJSON run stream to its end: the events in order.
async fn read_events(mut resp: reqwest::Response) -> Vec<Value> {
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.unwrap() {
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Sessions of the SQL editor (the owner's or a member role's) in the scratch database.
async fn editor_sessions(scratch: &ScratchCompute) -> i64 {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM pg_stat_activity WHERE datname = $1 AND application_name = 'chelabase-sql-editor'",
    )
    .bind(&scratch.database)
    .fetch_one(&scratch.admin)
    .await
    .unwrap();
    n
}

/// Waits (up to 10 s) until the scratch compute has no SQL editor session.
async fn wait_no_editor_sessions(scratch: &ScratchCompute) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while editor_sessions(scratch).await > 0 {
        assert!(std::time::Instant::now() < deadline, "an editor session outlived its run");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn run_streams_events_over_http() {
    let logs = Logs::default();
    let _guard = logs.capture();
    let scratch = ScratchCompute::new().await;
    scratch.set_password("run-pass").await;
    let t = owner_console_app(&scratch).await;
    t.seal_owner_password(Some("run-pass"));
    let main = t.main();
    let id = uuid::Uuid::new_v4();
    let sql = "INSERT INTO notes (body) VALUES ('streamed words'); SELECT id, body FROM notes";
    let resp = t.run_sql(main, json!({ "run_id": id, "sql": sql })).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "application/x-ndjson");
    assert_eq!(resp.headers()["cache-control"], "no-store");
    assert_eq!(resp.headers()["x-accel-buffering"], "no");
    let events = read_events(resp).await;
    let kinds: Vec<&str> = events.iter().map(|e| e["t"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["complete", "columns", "rows", "complete", "done"], "{events:?}");
    assert_eq!(events[0]["tag"], "INSERT 0 1");
    assert_eq!(events[2]["rows"], json!([["1", "streamed words"]]));
    assert_eq!(events[3]["tag"], "SELECT 1");
    assert_eq!(events[4]["outcome"], "ok");
    // The connection is closed and the slot given back.
    wait_no_editor_sessions(&scratch).await;
    assert!(t.app.state.sql_editor.runs().reserve("user-1", id, None, main, &scratch.database).is_ok());
    // One audit line, with the tags, without the SQL or its values.
    let text = logs.text();
    let audit: Vec<&str> = text.lines().filter(|l| l.contains("sql run")).collect();
    assert_eq!(audit.len(), 1, "{text}");
    for field in ["outcome=ok", "statements=2", "tags=INSERT 0 1,SELECT 1", "role=owner", "confirmed=false"] {
        assert!(audit[0].contains(field), "{field} missing from {}", audit[0]);
    }
    assert!(!audit[0].contains("held="), "{}", audit[0]);
    assert!(!text.contains("streamed words"), "{text}");
    assert!(!text.contains("run-pass"), "{text}");
    scratch.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn closing_the_stream_cancels_the_run() {
    let logs = Logs::default();
    let _guard = logs.capture();
    let scratch = ScratchCompute::new().await;
    scratch.set_password("cancel-pass").await;
    let t = owner_console_app(&scratch).await;
    t.seal_owner_password(Some("cancel-pass"));
    let main = t.main();

    // Cancelled through the route: the stream ends with `cancelled`.
    let id = uuid::Uuid::new_v4();
    let resp = t.run_sql(main, json!({ "run_id": id, "sql": "SELECT pg_sleep(30)" })).await;
    assert_eq!(resp.status(), 200);
    let started = std::time::Instant::now();
    assert_eq!(t.cancel_run(main, id).await.status(), 202);
    let events = read_events(resp).await;
    assert_eq!(events.last().unwrap()["t"], "done", "{events:?}");
    assert_eq!(events.last().unwrap()["outcome"], "cancelled", "{events:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));

    // Closed by the client: the server's session ends and the slot is given back.
    let id = uuid::Uuid::new_v4();
    let resp = t.run_sql(main, json!({ "run_id": id, "sql": "SELECT pg_sleep(30)" })).await;
    assert_eq!(resp.status(), 200);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while editor_sessions(&scratch).await == 0 {
        assert!(std::time::Instant::now() < deadline, "the run never started");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    drop(resp);
    wait_no_editor_sessions(&scratch).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while t.cancel_run(main, id).await.status() != 404 {
        assert!(std::time::Instant::now() < deadline, "the run is still registered");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let text = logs.text();
    let audit: Vec<&str> = text.lines().filter(|l| l.contains("sql run")).collect();
    assert_eq!(audit.len(), 2, "{text}");
    assert!(audit.iter().all(|l| l.contains("outcome=cancelled")), "{text}");
    scratch.drop().await;
}

// I4: the run route registers the starter, so a Developer cancels their own run over HTTP and
// another Developer can't.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn developer_cancels_their_own_run_over_http() {
    let scratch = ScratchCompute::new().await;
    scratch.set_password("starter-pass").await;
    let group = format!("{}_grp", scratch.owner);
    scratch.exec_in(&format!("CREATE ROLE {group} NOLOGIN; GRANT {group} TO {}", scratch.owner)).await;
    let t = owner_console_app(&scratch).await;
    t.seal_owner_password(Some("starter-pass"));
    let main = t.main();
    let ep = t.metadata.branch(main).endpoint_id.unwrap();
    t.metadata.add_group_role(&ep, &group, &[]);
    let a = t.grant_member("dev-a", PlatformRole::Developer, &[&group]);
    t.grant_member("dev-b", PlatformRole::Developer, &[&group]);
    // dev-a's member role, as the reconciler would have made it (T9: runs log in as it).
    let row = t.member_role_row(a, main, &[&group]);
    scratch
        .exec_in(&format!(
            "CREATE ROLE {0} LOGIN NOINHERIT PASSWORD 'pw'; GRANT {group} TO {0} WITH INHERIT FALSE, SET TRUE",
            row.role_name
        ))
        .await;
    let member_role = row.role_name.clone();
    t.put_member_role(row);

    let id = uuid::Uuid::new_v4();
    let path = format!("{}/sql", t.branch(main));
    let resp = t
        .send_as("dev-a", Method::POST, &path, Some(json!({ "run_id": id, "role": group, "sql": "SELECT pg_sleep(30)" })))
        .await;
    assert_eq!(resp.status(), 200);
    let cancel = format!("{}/sql/runs/{id}/cancel", t.branch(main));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while editor_sessions(&scratch).await == 0 {
        assert!(std::time::Instant::now() < deadline, "the run never started");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let err = error_of(t.send_as("dev-b", Method::POST, &cancel, None).await, 403).await;
    assert_eq!(err["error"]["required"], "admin");
    assert_eq!(t.send_as("dev-a", Method::POST, &cancel, None).await.status(), 202);
    let events = read_events(resp).await;
    assert_eq!(events.last().unwrap()["outcome"], "cancelled", "{events:?}");
    // R5: after `done`, the run's hand-over is asked for, with the run's backend.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while t.jobs.member_hand_overs.lock().unwrap().is_empty() {
        assert!(std::time::Instant::now() < deadline, "no hand-over asked for");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let asked = t.jobs.member_hand_overs.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(
        (asked[0].branch_id, asked[0].member_id, asked[0].database.as_str(), asked[0].acted_role.as_str()),
        (main.as_uuid(), a, scratch.database.as_str(), group.as_str())
    );
    assert!(asked[0].backend_pid.is_some_and(|pid| pid > 0), "{asked:?}");
    wait_no_editor_sessions(&scratch).await;
    let admin = scratch.admin.clone();
    scratch.drop().await;
    use sqlx::Executor;
    admin.execute(sqlx::AssertSqlSafe(format!("DROP ROLE {member_role}; DROP ROLE {group}"))).await.unwrap();
}

// M6 (T9 review): a member-role login the server refuses (28P01: the password changed under
// us) forces one reconcile, then the login is tried again; still refused is 503.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn member_login_refused_forces_one_reconcile() {
    let scratch = ScratchCompute::new().await;
    scratch.set_password("owner-pass").await;
    let group = format!("{}_grp", scratch.owner);
    let t = owner_console_app(&scratch).await;
    t.seal_owner_password(Some("owner-pass"));
    let main = t.main();
    let ep = t.metadata.branch(main).endpoint_id.unwrap();
    t.metadata.add_group_role(&ep, &group, &[]);
    let a = t.grant_member("dev-a", PlatformRole::Developer, &[&group]);
    let row = t.member_role_row(a, main, &[&group]); // sealed "pw"
    let member_role = row.role_name.clone();
    scratch
        .exec_in(&format!(
            "CREATE ROLE {group} NOLOGIN; \
             CREATE ROLE {member_role} LOGIN NOINHERIT PASSWORD 'changed-in-sql'; \
             GRANT {group} TO {member_role} WITH INHERIT FALSE, SET TRUE"
        ))
        .await;
    t.put_member_role(row.clone());
    // The worker's reconcile puts the right password back (here: a row sealing the catalog's).
    let mut fixed = row.clone();
    fixed.password_enc =
        chelabase_secrets::seal_member_role_password(&t.app.key, main.as_uuid(), &member_role, "changed-in-sql").unwrap();
    t.on_reconcile(vec![fixed]);
    let resp = t.run_as("dev-a", main, Some(&group)).await;
    assert_eq!(resp.status(), 200);
    let events = read_events(resp).await;
    assert_eq!(events.last().unwrap()["outcome"], "ok", "{events:?}");
    assert_eq!(t.reconciles().len(), 1);
    // A reconcile that doesn't fix it: one forced reconcile, then 503.
    t.put_member_role(row.clone());
    t.on_reconcile(vec![row]);
    let resp = t.run_as("dev-a", main, Some(&group)).await;
    assert_eq!(code_of(&error_of(resp, 503).await), "member_role_reconcile_failed");
    assert_eq!(t.reconciles().len(), 2);
    wait_no_editor_sessions(&scratch).await;
    let admin = scratch.admin.clone();
    scratch.drop().await;
    use sqlx::Executor;
    admin.execute(sqlx::AssertSqlSafe(format!("DROP ROLE {member_role}; DROP ROLE {group}"))).await.unwrap();
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn schema_route_returns_pgls_json() {
    let scratch = ScratchCompute::new().await;
    scratch.set_password("schema-pass").await;
    let t = owner_console_app(&scratch).await;
    t.seal_owner_password(Some("schema-pass"));
    let main = t.main();
    let resp = t.sql_schema(main, "").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "application/json");
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let body: Value = resp.json().await.unwrap();
    let tables = body["tables"].as_array().unwrap_or_else(|| panic!("no tables in {body}"));
    assert!(tables.iter().any(|t| t["name"] == "notes" && t["schema"] == "public"), "{body}");
    let columns = body["columns"].as_array().unwrap();
    assert!(columns.iter().any(|c| c["table_name"] == "notes" && c["name"] == "body"));
    wait_no_editor_sessions(&scratch).await;
    scratch.drop().await;
}

// --- Table API: reads (TE-A, T5) ----------------------------------------------------------

impl Branching {
    async fn table_get(&self, branch: BranchId, rest: &str) -> reqwest::Response {
        self.send(Method::GET, &format!("{}/{rest}", self.branch(branch)), None).await
    }
}

// T5 review, Minor: the access check comes before the query string, so a caller with no
// access learns nothing from a malformed one (404, not 422); a Viewer gets the 422.
#[tokio::test]
async fn table_reads_check_access_before_the_query_string() {
    let w = member_world(Mode::SelfHosted).await;
    let elsewhere = w.metadata.create_project(w.tenant, "Elsewhere").await.unwrap().id;
    w.metadata.add_granted(w.tenant, "outsider", elsewhere, PlatformRole::Viewer, &["app"]);
    w.metadata.add_granted(w.tenant, "vic", w.project, PlatformRole::Viewer, &["app"]);
    let path = format!(
        "/v1/databases/{}/branches/{}/objects?database=a&database=b",
        w.db.as_uuid(),
        w.dev.as_uuid()
    );
    let err = error_of(w.app.send_as("outsider", "org-1", Method::GET, &path, None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    assert_eq!(w.app.send_as("vic", "org-1", Method::GET, &path, None).await.status(), 422);
}

#[tokio::test]
async fn table_reads_check_the_database_before_any_compute() {
    let t = sql_app().await;
    let main = t.main();
    for rest in ["objects?database=nope", "types?database=postgres", "tables/public/notes?database=nope"] {
        assert_eq!(code_of(&error_of(t.table_get(main, rest).await, 404).await), "not_found", "{rest}");
    }
    assert_eq!(t.compute_lookups(), 0);
    for rest in ["objects", "types", "tables/public/notes", "tables/we%22ird/na%20me?database=analytics"] {
        assert_eq!(code_of(&error_of(t.table_get(main, rest).await, 503).await), "unavailable", "{rest}");
    }
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn table_reads_over_http() {
    let scratch = ScratchCompute::new().await;
    scratch.set_password("read-pass").await;
    scratch.exec_in(&format!(
        "CREATE VIEW public.notes_v AS SELECT id FROM public.notes; ALTER VIEW public.notes_v OWNER TO {o}; \
         CREATE TYPE public.mood AS ENUM ('sad', 'ok'); ALTER TYPE public.mood OWNER TO {o};",
        o = scratch.owner
    ))
    .await;
    let t = owner_console_app(&scratch).await;
    t.seal_owner_password(Some("read-pass"));
    let main = t.main();
    let resp = t.table_get(main, "objects").await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let objects = body["objects"].as_array().unwrap();
    let notes = objects.iter().find(|o| o["name"] == "notes").unwrap_or_else(|| panic!("{body}"));
    assert_eq!((notes["kind"].as_str(), notes["editable"].as_bool()), (Some("table"), Some(true)));
    let view = objects.iter().find(|o| o["name"] == "notes_v").unwrap();
    assert_eq!((view["kind"].as_str(), view["editable"].as_bool()), (Some("view"), Some(false)));
    assert!(objects.iter().all(|o| o["schema"] != "chelabase"), "{body}");
    let resp = t.table_get(main, "tables/public/notes").await;
    assert_eq!(resp.status(), 200);
    let table: Value = resp.json().await.unwrap();
    let columns: Vec<&str> = table["columns"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(columns, ["id", "body"]);
    assert!(table["constraints"].as_array().unwrap().iter().any(|c| c["kind"] == "primary_key"), "{table}");
    assert_eq!(table["kind"], "table", "{table}");
    // gc: a view answers too (read-only, columns only); a missing relation is still 404.
    let resp = t.table_get(main, "tables/public/notes_v").await;
    assert_eq!(resp.status(), 200);
    let view: Value = resp.json().await.unwrap();
    assert_eq!((view["kind"].as_str(), view["columns"][0]["name"].as_str()), (Some("view"), Some("id")), "{view}");
    for rest in ["tables/public/nope"] {
        assert_eq!(code_of(&error_of(t.table_get(main, rest).await, 404).await), "not_found", "{rest}");
    }
    let resp = t.table_get(main, "types").await;
    assert_eq!(resp.status(), 200);
    let types: Value = resp.json().await.unwrap();
    let types = types["types"].as_array().unwrap();
    assert!(types.iter().any(|t| t["schema"] == "pg_catalog" && t["name"] == "int8" && t["kind"] == "base"));
    assert!(types.iter().any(|t| t["name"] == "mood" && t["labels"] == json!(["sad", "ok"])));
    wait_no_editor_sessions(&scratch).await;
    scratch.drop().await;
}

// --- gc (user ruling 2026-10-21): `GET …/tables/{schema}/{table}` answers, read-only, for views,
// materialized views, foreign tables and partitioned tables ------------------------------------

/// `(name, data_type, not_null)` of a relation answer's columns, in the order given.
fn column_shapes(body: &Value) -> Vec<(String, String, bool)> {
    body["columns"]
        .as_array()
        .unwrap_or_else(|| panic!("no columns in {body}"))
        .iter()
        .map(|c| {
            (
                c["name"].as_str().unwrap().to_string(),
                c["data_type"].as_str().unwrap().to_string(),
                c["not_null"].as_bool().unwrap(),
            )
        })
        .collect()
}

fn shapes(want: &[(&str, &str, bool)]) -> Vec<(String, String, bool)> {
    want.iter().map(|(n, t, nn)| (n.to_string(), t.to_string(), *nn)).collect()
}

/// A read-only kind's answer: the generated `Table` shape with every list but `columns` empty
/// (required fields, never absent or null).
fn assert_columns_only(body: &Value, what: &str) {
    for list in ["constraints", "indexes", "policies", "grants", "options"] {
        assert_eq!(body[list], json!([]), "{what}: {list} in {body}");
    }
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn table_get_serves_views_and_other_relations_read_only() {
    let (scratch, t) = changes_app("gc-kinds").await;
    let main = t.main();
    // file_fdw ships with the stack's Postgres (contrib); a foreign table needs no file to exist
    // until it is scanned.
    let fdw = scratch.yes("SELECT EXISTS (SELECT FROM pg_available_extensions WHERE name = 'file_fdw')").await;
    scratch
        .exec_in(&format!(
            "CREATE TABLE public.orders (id uuid PRIMARY KEY, placed_at timestamptz NOT NULL, \
               amount numeric(12,2) NOT NULL, note text); \
             CREATE VIEW public.orders_v AS SELECT placed_at, id, amount, note FROM public.orders; \
             GRANT SELECT ON public.orders_v TO PUBLIC; \
             CREATE MATERIALIZED VIEW public.orders_mv AS SELECT id, amount FROM public.orders WITH NO DATA; \
             CREATE UNIQUE INDEX orders_mv_id ON public.orders_mv (id); \
             CREATE TABLE public.events (id bigint NOT NULL, gone int, at timestamptz) PARTITION BY RANGE (id); \
             ALTER TABLE public.events DROP COLUMN gone; \
             CREATE TABLE public.events_1 PARTITION OF public.events FOR VALUES FROM (0) TO (10); \
             CREATE SEQUENCE public.counter; \
             CREATE TYPE public.pair AS (a int, b int); \
             ALTER TABLE public.orders OWNER TO {o}; ALTER VIEW public.orders_v OWNER TO {o}; \
             ALTER MATERIALIZED VIEW public.orders_mv OWNER TO {o}; ALTER TABLE public.events OWNER TO {o}; \
             ALTER TABLE public.events_1 OWNER TO {o}; ALTER SEQUENCE public.counter OWNER TO {o};",
            o = scratch.owner
        ))
        .await;
    if fdw {
        scratch
            .exec_in(&format!(
                "CREATE EXTENSION file_fdw; CREATE SERVER gc_files FOREIGN DATA WRAPPER file_fdw; \
                 CREATE FOREIGN TABLE public.imports (id integer NOT NULL, label varchar(40), seen timestamptz) \
                   SERVER gc_files OPTIONS (filename '/gc/secret-path.csv', format 'csv'); \
                 ALTER FOREIGN TABLE public.imports OWNER TO {o};",
                o = scratch.owner
            ))
            .await;
    } else {
        eprintln!("skipping the foreign table: file_fdw isn't available on this Postgres");
    }

    // The view: format_type's text, attnum order, every column nullable (a view's attnotnull).
    let resp = t.table_get(main, "tables/public/orders_v").await;
    assert_eq!(resp.status(), 200, "a view answers");
    let text = resp.text().await.unwrap();
    let view: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(view["kind"], "view", "{view}");
    assert_eq!((view["schema"].as_str(), view["name"].as_str()), (Some("public"), Some("orders_v")));
    assert_eq!(
        column_shapes(&view),
        shapes(&[
            ("placed_at", "timestamp with time zone", false),
            ("id", "uuid", false),
            ("amount", "numeric(12,2)", false),
            ("note", "text", false),
        ])
    );
    assert_columns_only(&view, "view");
    // Nothing of the definition: the base table's name appears nowhere.
    assert!(!text.contains("orders\"") && !text.contains("FROM") && !text.contains("public.orders "), "{text}");

    let resp = t.table_get(main, "tables/public/orders_mv").await;
    assert_eq!(resp.status(), 200, "a materialized view answers");
    let mv: Value = resp.json().await.unwrap();
    assert_eq!(mv["kind"], "materialized_view", "{mv}");
    assert_eq!(column_shapes(&mv), shapes(&[("id", "uuid", false), ("amount", "numeric(12,2)", false)]));
    assert_columns_only(&mv, "materialized view");

    // The partitioned table: its own NOT NULLs, the dropped column left out.
    let resp = t.table_get(main, "tables/public/events").await;
    assert_eq!(resp.status(), 200, "a partitioned table answers");
    let parted: Value = resp.json().await.unwrap();
    assert_eq!(parted["kind"], "partitioned_table", "{parted}");
    assert_eq!(column_shapes(&parted), shapes(&[("id", "bigint", true), ("at", "timestamp with time zone", false)]));
    assert_columns_only(&parted, "partitioned table");

    if fdw {
        let resp = t.table_get(main, "tables/public/imports").await;
        assert_eq!(resp.status(), 200, "a foreign table answers");
        let text = resp.text().await.unwrap();
        let ft: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(ft["kind"], "foreign_table", "{ft}");
        assert_eq!(
            column_shapes(&ft),
            shapes(&[
                ("id", "integer", true),
                ("label", "character varying(40)", false),
                ("seen", "timestamp with time zone", false),
            ])
        );
        assert_columns_only(&ft, "foreign table");
        for leak in ["secret-path", "gc_files", "filename", "csv"] {
            assert!(!text.contains(leak), "{leak} in {text}");
        }
    }

    // An ordinary table is unchanged, with its kind.
    let resp = t.table_get(main, "tables/public/orders").await;
    assert_eq!(resp.status(), 200);
    let orders: Value = resp.json().await.unwrap();
    assert_eq!(orders["kind"], "table", "{orders}");
    assert!(orders["constraints"].as_array().unwrap().iter().any(|c| c["kind"] == "primary_key"), "{orders}");
    assert_eq!(
        column_shapes(&orders),
        shapes(&[
            ("id", "uuid", true),
            ("placed_at", "timestamp with time zone", true),
            ("amount", "numeric(12,2)", true),
            ("note", "text", false),
        ])
    );

    // Any other kind, a partition, Postgres's own catalogs and Chelabase's schema stay 404.
    for rest in [
        "tables/public/counter",
        "tables/public/orders_pkey",
        "tables/public/orders_mv_id",
        "tables/public/pair",
        "tables/public/events_1",
        "tables/pg_catalog/pg_roles",
        "tables/pg_catalog/pg_class",
        "tables/information_schema/tables",
        "tables/chelabase/schema_version",
        "tables/public/nope",
    ] {
        assert_eq!(code_of(&error_of(t.table_get(main, rest).await, 404).await), "not_found", "{rest}");
    }
    wait_no_editor_sessions(&scratch).await;
    scratch.drop().await;
}

// gc: a view is checked as a table is. A member whose login may connect reads it (its own
// columns, even when it selects from a table the owner and the member can't read: that's the
// view's pg_attribute, as Postgres's own catalog shows it); without CONNECT, or without access
// to the project, the refusal is the table's own.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn table_get_checks_a_view_as_it_checks_a_table() {
    let (scratch, t) = changes_app("gc-access").await;
    let main = t.main();
    let ep = t.metadata.branch(main).endpoint_id.unwrap();
    let group = format!("{}_grp", scratch.owner);
    t.metadata.add_group_role(&ep, &group, &[]);
    let vic = t.grant_member("vic", PlatformRole::Viewer, &[&group]);
    let login = chelabase_types::role_name::member_role_name(vic);
    let record = t.metadata.database(t.db);
    let elsewhere = t.metadata.create_project(record.tenant_id, "Elsewhere").await.unwrap().id;
    t.metadata.add_granted(record.tenant_id, "outsider", elsewhere, PlatformRole::Viewer, &[]);
    scratch
        .exec_in(&format!(
            "CREATE ROLE {group} NOLOGIN; CREATE ROLE {login} LOGIN NOINHERIT; \
             GRANT {group} TO {login} WITH INHERIT FALSE, SET TRUE; \
             CREATE SCHEMA gc_vault; REVOKE ALL ON SCHEMA gc_vault FROM PUBLIC; \
             CREATE TABLE gc_vault.ledger (k uuid, total numeric(12,2), at timestamptz); \
             REVOKE ALL ON gc_vault.ledger FROM PUBLIC; \
             CREATE VIEW public.ledger_v AS SELECT k AS ref, total AS sum_total, at AS booked FROM gc_vault.ledger;"
        ))
        .await;
    let get = |sub: &'static str, rest: &'static str| {
        let path = format!("{}/{rest}", t.branch(main));
        let t = &t;
        async move { t.send_as(sub, Method::GET, &path, None).await }
    };
    for sub in ["vic", "user-1"] {
        let resp = get(sub, "tables/public/ledger_v").await;
        assert_eq!(resp.status(), 200, "{sub}: a view over a table no one but the admin reads");
        let text = resp.text().await.unwrap();
        let view: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(view["kind"], "view", "{sub}: {view}");
        assert_eq!(
            column_shapes(&view),
            shapes(&[
                ("ref", "uuid", false),
                ("sum_total", "numeric(12,2)", false),
                ("booked", "timestamp with time zone", false),
            ]),
            "{sub}"
        );
        for leak in ["gc_vault", "ledger\"", "ledger.", "SELECT"] {
            assert!(!text.contains(leak), "{sub}: {leak} in {text}");
        }
    }
    // Without CONNECT for the member's login: the table's 403, word for word.
    scratch
        .exec_in(&format!(
            "REVOKE CONNECT ON DATABASE {db} FROM PUBLIC; GRANT CONNECT ON DATABASE {db} TO {group};",
            db = scratch.database
        ))
        .await;
    let table = error_of(get("vic", "tables/public/notes").await, 403).await;
    assert_eq!(code_of(&table), "permission_denied");
    let view = error_of(get("vic", "tables/public/ledger_v").await, 403).await;
    assert_eq!((code_of(&view), message_of(&view)), (code_of(&table), message_of(&table)));
    // No access to the project: the table's 404.
    let table = error_of(get("outsider", "tables/public/notes").await, 404).await;
    let view = error_of(get("outsider", "tables/public/ledger_v").await, 404).await;
    assert_eq!(code_of(&table), "not_found");
    assert_eq!((code_of(&view), message_of(&view)), (code_of(&table), message_of(&table)));
    wait_no_editor_sessions(&scratch).await;
    let admin = scratch.admin.clone();
    scratch.drop().await;
    use sqlx::Executor;
    admin.execute(sqlx::AssertSqlSafe(format!("DROP ROLE {login}"))).await.unwrap();
    admin.execute(sqlx::AssertSqlSafe(format!("DROP ROLE {group}"))).await.unwrap();
}

// gc (coordinator ruling): every relation `…/objects` lists answers `…/tables/{schema}/{name}`.
// A table that inherits from another is listed as a table, not editable, and answers with its
// full table read (inherited columns and CHECK constraints included) as kind `table`.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn table_get_serves_every_listed_object_and_inheriting_tables() {
    let (scratch, t) = changes_app("gc-every").await;
    let main = t.main();
    scratch
        .exec_in(&format!(
            "CREATE TABLE public.animals (id bigint PRIMARY KEY, name text NOT NULL CHECK (name <> '')); \
             CREATE TABLE public.dogs (good boolean NOT NULL DEFAULT true) INHERITS (public.animals); \
             CREATE INDEX dogs_name ON public.dogs (name); \
             CREATE TABLE public.\"Odd Name\" (x int); \
             CREATE VIEW public.\"we'ird v\" AS SELECT x FROM public.\"Odd Name\"; \
             CREATE TABLE public.parted (k int NOT NULL) PARTITION BY LIST (k); \
             CREATE TABLE public.parted_1 PARTITION OF public.parted FOR VALUES IN (1); \
             ALTER TABLE public.animals OWNER TO {o}; ALTER TABLE public.dogs OWNER TO {o}; \
             ALTER TABLE public.\"Odd Name\" OWNER TO {o}; ALTER VIEW public.\"we'ird v\" OWNER TO {o}; \
             ALTER TABLE public.parted OWNER TO {o}; ALTER TABLE public.parted_1 OWNER TO {o};",
            o = scratch.owner
        ))
        .await;

    let resp = t.table_get(main, "tables/public/dogs").await;
    assert_eq!(resp.status(), 200, "an inheriting table answers");
    let dogs: Value = resp.json().await.unwrap();
    assert_eq!(dogs["kind"], "table", "{dogs}");
    assert_eq!(
        column_shapes(&dogs),
        shapes(&[("id", "bigint", true), ("name", "text", true), ("good", "boolean", true)])
    );
    assert_eq!(dogs["columns"][2]["default"], "true", "{dogs}");
    // The inherited CHECK applies to the child; its own index is there.
    let constraints = dogs["constraints"].as_array().unwrap();
    assert!(constraints.iter().any(|c| c["kind"] == "check"), "{dogs}");
    assert!(dogs["indexes"].as_array().unwrap().iter().any(|i| i["name"] == "dogs_name"), "{dogs}");
    // The parent is unchanged.
    let resp = t.table_get(main, "tables/public/animals").await;
    assert_eq!(resp.status(), 200);
    let animals: Value = resp.json().await.unwrap();
    assert_eq!(animals["kind"], "table", "{animals}");
    assert!(animals["constraints"].as_array().unwrap().iter().any(|c| c["kind"] == "primary_key"), "{animals}");

    // Every object the list shows answers 200 with the list's kind, and nothing else is listed.
    let resp = t.table_get(main, "objects").await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let objects = body["objects"].as_array().unwrap();
    let names: Vec<&str> = objects.iter().map(|o| o["name"].as_str().unwrap()).collect();
    for want in ["animals", "dogs", "Odd Name", "we'ird v", "parted"] {
        assert!(names.contains(&want), "{want} in {names:?}");
    }
    assert!(!names.contains(&"parted_1"), "{names:?}");
    for o in objects {
        let (schema, name) = (o["schema"].as_str().unwrap(), o["name"].as_str().unwrap());
        let rest = format!("tables/{}/{}", urlencoding::encode(schema), urlencoding::encode(name));
        let resp = t.table_get(main, &rest).await;
        assert_eq!(resp.status(), 200, "{schema}.{name}: {o}");
        let got: Value = resp.json().await.unwrap();
        assert_eq!(got["kind"], o["kind"], "{schema}.{name}: {got}");
        assert_eq!((got["schema"].as_str(), got["name"].as_str()), (Some(schema), Some(name)), "{got}");
        assert_eq!(got["oid"], o["oid"], "{schema}.{name}");
    }
    // The partition stays out.
    assert_eq!(code_of(&error_of(t.table_get(main, "tables/public/parted_1").await, 404).await), "not_found");
    wait_no_editor_sessions(&scratch).await;
    scratch.drop().await;
}

// gc: the OpenAPI document gives `getTable`'s answer a required `kind` (the object list's
// `ObjectKind`) beside the `Table` fields.
#[test]
fn get_table_documents_the_relation_kind() {
    let doc = serde_json::to_value(orchestrator::openapi()).unwrap();
    let op = &doc["paths"]["/v1/databases/{id}/branches/{branch_id}/tables/{schema}/{table}"]["get"];
    let schema = &op["responses"]["200"]["content"]["application/json"]["schema"];
    assert!(!schema.is_null(), "{op}");
    // Every property and required name reachable through `$ref` and `allOf`.
    fn walk<'a>(doc: &'a Value, s: &'a Value, props: &mut Vec<(String, &'a Value)>, required: &mut Vec<String>) {
        if let Some(r) = s["$ref"].as_str() {
            let name = r.rsplit('/').next().unwrap();
            return walk(doc, &doc["components"]["schemas"][name], props, required);
        }
        for part in s["allOf"].as_array().into_iter().flatten() {
            walk(doc, part, props, required);
        }
        for (k, v) in s["properties"].as_object().into_iter().flatten() {
            props.push((k.clone(), v));
        }
        required.extend(s["required"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_string)));
    }
    let (mut props, mut required) = (Vec::new(), Vec::new());
    walk(&doc, schema, &mut props, &mut required);
    let kind = props.iter().find(|(k, _)| k == "kind").map(|(_, v)| *v).unwrap_or_else(|| panic!("no kind in {schema}"));
    assert!(kind.to_string().contains("#/components/schemas/ObjectKind"), "{kind}");
    for name in ["kind", "columns", "constraints", "indexes", "policies", "grants", "options"] {
        assert!(required.iter().any(|r| r == name), "{name} isn't required: {required:?}");
    }
    let summary = op["summary"].as_str().unwrap_or_default();
    assert!(!summary.contains("ordinary table"), "the summary still says only tables: {summary}");
}

// --- Schema merges (step 6g, T10) ------------------------------------------------------------

impl Branching {
    fn merges(&self) -> String {
        format!("/v1/databases/{}/merges", self.db.as_uuid())
    }

    fn merge_path(&self, id: uuid::Uuid) -> String {
        format!("{}/{id}", self.merges())
    }

    /// A ready `dev` under main, with an `app` database on both.
    fn mergeable(&self) -> BranchId {
        let dev = self.ready("dev");
        self.metadata.add_branch_db(self.main(), "web1", "postgres", DatabaseOrigin::Main);
        self.metadata.add_branch_db(self.main(), "app", "postgres", DatabaseOrigin::Sql);
        self.metadata.add_branch_db(dev, "web1", "postgres", DatabaseOrigin::Main);
        self.metadata.add_branch_db(dev, "app", "postgres", DatabaseOrigin::Sql);
        dev
    }

    /// Stores a merge row in `status` (source `dev`, target main) and returns it, changed by `f`.
    fn seed_merge(&self, dev: BranchId, status: MergeStatus, f: impl FnOnce(&mut MergeRecord)) -> MergeRecord {
        let mut row = merge_row(self.db, dev, self.main(), status);
        f(&mut row);
        self.metadata.merges.lock().unwrap().push(row.clone());
        row
    }

    fn merge_state(&self, id: uuid::Uuid) -> MergeRecord {
        self.metadata.merges.lock().unwrap().iter().find(|r| r.id == id).cloned().expect("merge row")
    }

    fn set_merge(&self, id: uuid::Uuid, f: impl FnOnce(&mut MergeRecord)) {
        let mut rows = self.metadata.merges.lock().unwrap();
        f(rows.iter_mut().find(|r| r.id == id).expect("merge row"));
    }

    fn merge_jobs(&self) -> Vec<MergeJob> {
        self.jobs.merge_jobs.lock().unwrap().clone()
    }

    async fn create_merge(&self, branch: BranchId, body: Value) -> reqwest::Response {
        self.send(Method::POST, &format!("{}/merges", self.branch(branch)), Some(body)).await
    }
}

fn into_parent() -> Value {
    json!({ "direction": "into_parent", "database": "app" })
}

#[tokio::test]
async fn create_merge_is_202_and_publishes_compute() {
    let t = branching().await;
    let dev = t.mergeable();
    let resp = t.create_merge(dev, into_parent()).await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "computing");
    assert_eq!(body["direction"], "into_parent");
    assert_eq!(body["source_branch_id"], json!(dev));
    assert_eq!(body["target_branch_id"], json!(t.main()));
    assert_eq!(body["database"], "app");
    assert_eq!(body["version"], 1);
    let id: uuid::Uuid = body["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(t.merge_jobs(), vec![MergeJob::Compute { merge_id: id }]);
    assert_eq!(t.merge_state(id).created_by, "user-1");

    // From the parent: the parent is the source and the branch receives. The database
    // defaults to the main one.
    let second = t.create_merge(dev, json!({ "direction": "from_parent" })).await;
    assert_eq!(second.status(), 202);
    let second: Value = second.json().await.unwrap();
    assert_eq!(second["source_branch_id"], json!(t.main()));
    assert_eq!(second["target_branch_id"], json!(dev));
    assert_eq!(second["database"], "web1");
}

#[tokio::test]
async fn create_merge_refuses_main_into_parent() {
    let t = branching().await;
    let dev = t.mergeable();
    for direction in ["into_parent", "from_parent"] {
        let err = error_of(t.create_merge(t.main(), json!({ "direction": direction })).await, 422).await;
        assert_eq!(code_of(&err), "invalid_request");
        assert!(message_of(&err).contains("main"), "{err}");
    }
    // A backup is no merge party; a body of the wrong shape is a 422.
    let backup = t.backup(dev);
    let err = error_of(t.create_merge(backup, into_parent()).await, 409).await;
    assert_eq!(code_of(&err), "backup_branch");
    let err = error_of(t.create_merge(dev, json!({ "direction": "sideways" })).await, 422).await;
    assert_eq!(code_of(&err), "invalid_request");
    let err = error_of(t.create_merge(dev, json!({ "direction": "into_parent", "extra": 1 })).await, 422).await;
    assert_eq!(code_of(&err), "invalid_request");
    let err = error_of(t.create_merge(dev, json!({ "direction": "into_parent", "database": "nope" })).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    assert!(t.merge_jobs().is_empty());
}

#[tokio::test]
async fn create_merge_refuses_unready_branch() {
    let t = branching().await;
    let wip = t.add("wip", t.main(), BranchKind::Normal, BranchStatus::Pending);
    let err = error_of(t.create_merge(wip, into_parent()).await, 409).await;
    assert_eq!(code_of(&err), "branch_not_ready");
    // A ready branch whose parent isn't ready.
    let parent = t.add("parent", t.main(), BranchKind::Normal, BranchStatus::Resetting);
    let kid = t.add("kid", parent, BranchKind::Normal, BranchStatus::Ready);
    let err = error_of(t.create_merge(kid, into_parent()).await, 409).await;
    assert_eq!(code_of(&err), "branch_not_ready");
    assert!(t.merge_jobs().is_empty());
    assert!(t.metadata.merges.lock().unwrap().is_empty());
}

#[tokio::test]
async fn second_open_merge_is_409_with_id() {
    let t = branching().await;
    let dev = t.mergeable();
    let first: Value = t.create_merge(dev, into_parent()).await.json().await.unwrap();
    let err = error_of(t.create_merge(dev, into_parent()).await, 409).await;
    assert_eq!(code_of(&err), "merge_open_exists");
    assert_eq!(err["error"]["merge_id"], first["id"]);
    assert_eq!(t.merge_jobs().len(), 1, "no second job");
    // Another Postgres database of the pair is its own merge.
    let other = t.create_merge(dev, json!({ "direction": "into_parent", "database": "web1" })).await;
    assert_eq!(other.status(), 202);
}

#[tokio::test]
async fn create_merge_lost_race_is_a_retryable_409() {
    let t = branching().await;
    let dev = t.mergeable();
    *t.metadata.create_merge_conflicts.lock().unwrap() = true;
    let resp = t.create_merge(dev, into_parent()).await;
    assert_eq!(resp.status(), 409);
    assert_eq!(resp.headers()["retry-after"], "1");
    let err: Value = resp.json().await.unwrap();
    assert_eq!(code_of(&err), "merge_busy");
    assert!(t.merge_jobs().is_empty());
}

#[tokio::test]
async fn create_merge_publish_failure_is_503_and_fails_the_merge() {
    let t = branching().await;
    let dev = t.mergeable();
    *t.jobs.fail_merge_publish.lock().unwrap() = true;
    let err = error_of(t.create_merge(dev, into_parent()).await, 503).await;
    assert_eq!(code_of(&err), "worker_unavailable");
    let rows = t.metadata.merges.lock().unwrap().clone();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, MergeStatus::Failed);
    assert!(rows[0].error.as_deref().unwrap_or_default().contains("publish failed"));
    // The pair is free again.
    *t.jobs.fail_merge_publish.lock().unwrap() = false;
    assert_eq!(t.create_merge(dev, into_parent()).await.status(), 202);
}

#[tokio::test]
async fn get_and_list_merges() {
    let t = branching().await;
    let dev = t.mergeable();
    let other = t.ready("other");
    let a = t.seed_merge(dev, MergeStatus::Draft, |r| {
        r.analysis = Some(json!({"items": [], "conflicts": [], "reported": []}));
        r.plan = Some(json!({"steps": []}));
    });
    let b = t.seed_merge(other, MergeStatus::Done, |_| {});
    t.metadata.merge_events.lock().unwrap().push({
        let mut e = MergeEvent::new(a.id, "status");
        e.detail = Some(json!({"to": "draft"}));
        e
    });

    let got: Value = t.send(Method::GET, &t.merge_path(a.id), None).await.json().await.unwrap();
    assert_eq!(got["merge"]["id"], json!(a.id));
    assert_eq!(got["merge"]["version"], 3);
    assert_eq!(got["merge"]["plan"], json!({"steps": []}));
    assert_eq!(got["merge"]["analysis"]["conflicts"], json!([]));
    assert_eq!(got["merge"]["resolutions"], json!([]));
    assert_eq!(got["events"].as_array().unwrap().len(), 1);
    assert_eq!(got["events"][0]["kind"], "status");

    let all: Vec<Value> = t.send(Method::GET, &t.merges(), None).await.json().await.unwrap();
    assert_eq!(all.len(), 2);
    assert!(all[0].get("analysis").is_none(), "the list is a summary");
    let ids = |list: &[Value]| list.iter().map(|m| m["id"].clone()).collect::<Vec<_>>();
    let for_dev: Vec<Value> =
        t.send(Method::GET, &format!("{}?branch={}", t.merges(), dev.as_uuid()), None).await.json().await.unwrap();
    assert_eq!(ids(&for_dev), vec![json!(a.id)]);
    let for_other: Vec<Value> =
        t.send(Method::GET, &format!("{}?branch={}", t.merges(), other.as_uuid()), None).await.json().await.unwrap();
    assert_eq!(ids(&for_other), vec![json!(b.id)]);
    let bad = t.send(Method::GET, &format!("{}?branch=nope", t.merges()), None).await;
    assert_eq!(bad.status(), 400);

    // Unknown merges, and another database's, are 404 `not_found` (P16).
    let err = error_of(t.send(Method::GET, &t.merge_path(uuid::Uuid::new_v4()), None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    t.set_merge(a.id, |r| r.database_id = uuid::Uuid::new_v4());
    let err = error_of(t.send(Method::GET, &t.merge_path(a.id), None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
}

#[tokio::test]
async fn update_merge_with_stale_version_is_409_with_current() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::Draft, |_| {});
    let path = t.merge_path(m.id);
    let resolutions = json!([{ "conflict": "c1", "choice": "keep_target" }, { "conflict": "c2", "choice": { "own_sql": "ALTER TABLE t ADD c int" } }]);

    let resp = t.send(Method::PATCH, &path, Some(json!({ "version": 2, "resolutions": resolutions }))).await;
    assert_eq!(resp.status(), 409);
    let err: Value = resp.json().await.unwrap();
    assert_eq!(code_of(&err), "merge_version_conflict");
    assert_eq!(err["error"]["current"]["version"], 3);
    assert_eq!(err["error"]["current"]["id"], json!(m.id));
    assert!(t.merge_jobs().is_empty());
    assert_eq!(t.merge_state(m.id).resolutions, json!([]));

    let saved = t
        .send(
            Method::PATCH,
            &path,
            Some(json!({
                "version": 3,
                "resolutions": resolutions,
                "steps": [{ "id": "u1", "after": null, "sql": "UPDATE t SET c = 0" }],
                "acknowledgements": ["column:1:2"],
                "options": { "lock_timeout_ms": 2000, "after": "delete" },
            })),
        )
        .await;
    assert_eq!(saved.status(), 200);
    let saved: Value = saved.json().await.unwrap();
    assert_eq!(saved["version"], 4);
    assert_eq!(saved["status"], "computing", "a save recomputes the analysis and the plan");
    assert_eq!(saved["resolutions"], resolutions);
    assert_eq!(saved["options"], json!({"lock_timeout_ms": 2000, "after": "delete"}));
    assert_eq!(t.merge_jobs(), vec![MergeJob::Compute { merge_id: m.id }]);
    let row = t.merge_state(m.id);
    assert_eq!(row.user_steps, json!([{ "id": "u1", "after": null, "sql": "UPDATE t SET c = 0" }]));
    assert_eq!(row.acknowledgements, json!(["column:1:2"]));

    // While the worker computes, the draft can't be edited.
    let err = error_of(t.send(Method::PATCH, &path, Some(json!({ "version": 4 }))).await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    assert_eq!(t.merge_jobs().len(), 1);
}

#[tokio::test]
async fn update_merge_body_is_validated() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::Draft, |_| {});
    let path = t.merge_path(m.id);
    let bad = [
        json!({ "resolutions": [] }),
        json!({ "version": 3, "surprise": true }),
        json!({ "version": 3, "resolutions": [{ "conflict": "c1", "choice": "take_it" }] }),
        json!({ "version": 3, "resolutions": [{ "conflict": "c1", "choice": { "own_sql": "  " } }] }),
        json!({ "version": 3, "resolutions": [{ "conflict": "c1", "choice": "keep_target", "x": 1 }] }),
        json!({ "version": 3, "steps": [{ "id": "u1", "after": null, "sql": "" }] }),
        json!({ "version": 3, "options": { "after": "explode" } }),
        json!({ "version": 3, "options": { "lock_timeout_ms": 0 } }),
        json!({ "version": 3, "options": { "rebaseline": true } }),
    ];
    for body in bad {
        let err = error_of(t.send(Method::PATCH, &path, Some(body.clone())).await, 422).await;
        assert_eq!(code_of(&err), "invalid_request", "{body}");
    }
    let err = error_of(t.send(Method::PATCH, &t.merge_path(uuid::Uuid::new_v4()), Some(json!({ "version": 1 }))).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    assert_eq!(t.merge_state(m.id).version, 3);
    assert!(t.merge_jobs().is_empty());
}

#[tokio::test]
async fn update_merge_publishes_compute_once_for_concurrent_saves() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::Rehearsed, |r| r.rehearsal = Some(json!({"ok": true})));
    let path = t.merge_path(m.id);
    let body = json!({ "version": 3, "steps": [] });
    let (a, b) = tokio::join!(
        t.send(Method::PATCH, &path, Some(body.clone())),
        t.send(Method::PATCH, &path, Some(body.clone())),
    );
    let mut statuses = vec![a.status().as_u16(), b.status().as_u16()];
    statuses.sort();
    assert_eq!(statuses, vec![200, 409]);
    assert_eq!(t.merge_jobs(), vec![MergeJob::Compute { merge_id: m.id }]);
    let row = t.merge_state(m.id);
    assert_eq!((row.status, row.rehearsal), (MergeStatus::Computing, None));
}

#[tokio::test]
async fn update_merge_publish_failure_fails_the_merge() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::Draft, |_| {});
    *t.jobs.fail_merge_publish.lock().unwrap() = true;
    let err = error_of(t.send(Method::PATCH, &t.merge_path(m.id), Some(json!({ "version": 3, "steps": [] }))).await, 503).await;
    assert_eq!(code_of(&err), "worker_unavailable");
    assert_eq!(t.merge_state(m.id).status, MergeStatus::Failed);
}

#[tokio::test]
async fn rehearse_needs_a_draft_with_a_plan_and_no_open_conflicts() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::Draft, |_| {});
    let path = format!("{}/rehearse", t.merge_path(m.id));
    // No plan yet.
    let err = error_of(t.send(Method::POST, &path, None).await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    // An unresolved conflict.
    t.set_merge(m.id, |r| {
        r.analysis = Some(json!({"conflicts": [{"id": "c1"}]}));
        r.plan = Some(json!({"steps": []}));
    });
    let err = error_of(t.send(Method::POST, &path, None).await, 409).await;
    assert_eq!(code_of(&err), "merge_conflicts_open");
    t.set_merge(m.id, |r| r.resolutions = json!([{ "conflict": "c1", "choice": "keep_target" }]));
    let resp = t.send(Method::POST, &path, None).await;
    assert_eq!(resp.status(), 202);
    assert_eq!(resp.json::<Value>().await.unwrap()["status"], "rehearsing");
    assert_eq!(t.merge_jobs(), vec![MergeJob::Rehearse { merge_id: m.id }]);
    // Not a draft any more.
    let err = error_of(t.send(Method::POST, &path, None).await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    // A lost publish puts the draft back.
    t.set_merge(m.id, |r| r.status = MergeStatus::Draft);
    *t.jobs.fail_merge_publish.lock().unwrap() = true;
    let err = error_of(t.send(Method::POST, &path, None).await, 503).await;
    assert_eq!(code_of(&err), "worker_unavailable");
    assert_eq!(t.merge_state(m.id).status, MergeStatus::Draft);
}

#[tokio::test]
async fn recompute_moves_to_computing_and_stores_rebaseline() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::Draft, |_| {});
    let path = format!("{}/recompute", t.merge_path(m.id));
    // An empty body is `{}`.
    let resp = t.send(Method::POST, &path, None).await;
    assert_eq!(resp.status(), 202);
    assert_eq!(resp.json::<Value>().await.unwrap()["status"], "computing");
    assert_eq!(t.merge_state(m.id).options.get("rebaseline"), None);

    t.set_merge(m.id, |r| {
        r.status = MergeStatus::Stale;
        r.error = Some("old".into());
    });
    let resp = t.send(Method::POST, &path, Some(json!({ "rebaseline": true }))).await;
    assert_eq!(resp.status(), 202);
    let row = t.merge_state(m.id);
    assert_eq!((row.status, row.error), (MergeStatus::Computing, None));
    assert_eq!(row.options["rebaseline"], true);
    assert_eq!(row.options["lock_timeout_ms"], 5000, "other options stay");
    assert_eq!(t.merge_jobs(), vec![MergeJob::Compute { merge_id: m.id }; 2]);

    // Not from a computing, rehearsing or rehearsed merge, nor from a failed online phase.
    for status in [MergeStatus::Computing, MergeStatus::Rehearsing, MergeStatus::Rehearsed, MergeStatus::Done] {
        t.set_merge(m.id, |r| r.status = status);
        let err = error_of(t.send(Method::POST, &path, None).await, 409).await;
        assert_eq!(code_of(&err), "conflict", "{status:?}");
    }
    t.set_merge(m.id, |r| {
        r.status = MergeStatus::Failed;
        r.failed_phase = Some("online".into());
    });
    let err = error_of(t.send(Method::POST, &path, None).await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    assert!(message_of(&err).contains("retry"), "{err}");
    t.set_merge(m.id, |r| r.failed_phase = Some("phase1".into()));
    assert_eq!(t.send(Method::POST, &path, Some(json!({}))).await.status(), 202);
    assert_eq!(t.merge_state(m.id).failed_phase, None);
    let err = error_of(t.send(Method::POST, &path, Some(json!({ "rebaseline": "yes" }))).await, 422).await;
    assert_eq!(code_of(&err), "invalid_request");
}

fn plan_with(step: Value) -> Value {
    json!({ "steps": [step] })
}

#[tokio::test]
async fn apply_refusal_order() {
    let t = branching().await;
    let dev = t.mergeable();
    let other = t.ready("other");
    let m = t.seed_merge(dev, MergeStatus::Draft, |r| {
        r.analysis = Some(json!({"conflicts": [{"id": "c1"}]}));
        r.plan = Some(plan_with(json!({"id": "s1", "items": ["column:1:2"], "data_loss": true, "security": false})));
    });
    let path = format!("{}/apply", t.merge_path(m.id));
    let refused = |code: &'static str| async move { code };
    let _ = refused;
    let go = |body: Option<Value>| t.send(Method::POST, &path, body);

    // 1. Not rehearsed.
    let err = error_of(go(Some(json!({ "confirm": true }))).await, 409).await;
    assert_eq!(code_of(&err), "merge_not_rehearsed");
    t.set_merge(m.id, |r| r.status = MergeStatus::Stale);
    let err = error_of(go(None).await, 409).await;
    assert_eq!(code_of(&err), "merge_stale");
    // The apply request doesn't read the target: the message says what stale means, no more.
    assert_eq!(message_of(&err), "the merge is stale: a schema changed since it was computed; recompute the merge");
    t.set_merge(m.id, |r| r.status = MergeStatus::Rehearsed);
    // 2. An open conflict.
    let err = error_of(go(None).await, 409).await;
    assert_eq!(code_of(&err), "merge_conflicts_open");
    t.set_merge(m.id, |r| r.resolutions = json!([{ "conflict": "c1", "choice": "take_source" }]));
    // 3. Data loss, then security, unacknowledged.
    let err = error_of(go(None).await, 409).await;
    assert_eq!(code_of(&err), "merge_drops_unacknowledged");
    t.set_merge(m.id, |r| r.acknowledgements = json!(["column:1:2"]));
    t.set_merge(m.id, |r| {
        r.plan = Some(json!({"steps": [
            {"id": "s1", "items": ["column:1:2"], "data_loss": true, "security": false},
            {"id": "s2", "items": ["role:name:admin"], "data_loss": false, "security": true},
            {"id": "s3", "items": [], "data_loss": false, "security": true},
        ]}));
    });
    let err = error_of(go(None).await, 409).await;
    assert_eq!(code_of(&err), "merge_drops_unacknowledged");
    assert_eq!(
        message_of(&err),
        "acknowledge these data-loss, security or breaking changes first: role:name:admin, s3",
        "{err}"
    );
    // A step with no items is acknowledged by its own id.
    t.set_merge(m.id, |r| r.acknowledgements = json!(["column:1:2", "role:name:admin", "s3"]));
    // 4. A protected target needs `confirm`.
    t.metadata.branches.lock().unwrap().get_mut(&t.main()).unwrap().protected = true;
    let err = error_of(go(None).await, 409).await;
    assert_eq!(code_of(&err), "merge_confirm_required");
    let err = error_of(go(Some(json!({ "confirm": false }))).await, 409).await;
    assert_eq!(code_of(&err), "merge_confirm_required");
    // 5. Another merge applying on the target.
    let busy = t.seed_merge(other, MergeStatus::Applying, |_| {});
    let err = error_of(go(Some(json!({ "confirm": true }))).await, 409).await;
    assert_eq!(code_of(&err), "merge_busy");
    assert_eq!(err["error"]["merge_id"], json!(busy.id));
    assert!(t.merge_jobs().is_empty());
    assert_eq!(t.merge_state(m.id).status, MergeStatus::Rehearsed);

    t.set_merge(busy.id, |r| r.status = MergeStatus::Done);
    let resp = go(Some(json!({ "confirm": true }))).await;
    assert_eq!(resp.status(), 202);
    assert_eq!(resp.json::<Value>().await.unwrap()["status"], "applying");
    assert_eq!(t.merge_state(m.id).status, MergeStatus::Applying);
    assert_eq!(t.merge_jobs(), vec![MergeJob::Apply { merge_id: m.id }]);
    // Twice: no longer rehearsed.
    let err = error_of(go(Some(json!({ "confirm": true }))).await, 409).await;
    assert_eq!(code_of(&err), "merge_not_rehearsed");
}

#[tokio::test]
async fn apply_on_protected_needs_confirm() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::Rehearsed, |r| {
        r.analysis = Some(json!({"conflicts": []}));
        r.plan = Some(json!({"steps": []}));
    });
    let path = format!("{}/apply", t.merge_path(m.id));
    t.metadata.branches.lock().unwrap().get_mut(&t.main()).unwrap().protected = true;
    let err = error_of(t.send(Method::POST, &path, Some(json!({}))).await, 409).await;
    assert_eq!(code_of(&err), "merge_confirm_required");
    let err = error_of(t.send(Method::POST, &path, Some(json!({ "unknown": 1 }))).await, 422).await;
    assert_eq!(code_of(&err), "invalid_request");
    assert_eq!(t.send(Method::POST, &path, Some(json!({ "confirm": true }))).await.status(), 202);

    // An unprotected target needs no confirm, and the body may be absent.
    let m2 = t.seed_merge(dev, MergeStatus::Rehearsed, |r| {
        r.plan = Some(json!({"steps": []}));
        r.analysis = Some(json!({"conflicts": []}));
        r.target_branch_id = t.main().as_uuid();
    });
    t.set_merge(m.id, |r| r.status = MergeStatus::Done);
    t.metadata.branches.lock().unwrap().get_mut(&t.main()).unwrap().protected = false;
    assert_eq!(t.send(Method::POST, &format!("{}/apply", t.merge_path(m2.id)), None).await.status(), 202);
}

#[tokio::test]
async fn apply_publish_failure_puts_the_merge_back_to_rehearsed() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::Rehearsed, |r| {
        r.analysis = Some(json!({"conflicts": []}));
        r.plan = Some(json!({"steps": []}));
    });
    *t.jobs.fail_merge_publish.lock().unwrap() = true;
    let err = error_of(t.send(Method::POST, &format!("{}/apply", t.merge_path(m.id)), None).await, 503).await;
    assert_eq!(code_of(&err), "worker_unavailable");
    assert_eq!(t.merge_state(m.id).status, MergeStatus::Rehearsed);
}

// E20: a failure before the online phase (`roles`, `enum_add`, `tx`) is retried from the
// start: `failed` → `applying` and an Apply job (the worker's pre-checks run again). Never to
// `online`.
#[tokio::test]
async fn retry_before_the_online_phase_applies_from_the_start() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::Failed, |_| {});
    let path = format!("{}/retry", t.merge_path(m.id));
    for phase in ["roles", "enum_add", "tx"] {
        t.set_merge(m.id, |r| {
            r.status = MergeStatus::Failed;
            r.failed_phase = Some(phase.into());
            r.error = Some("step x: boom".into());
        });
        t.jobs.merge_jobs.lock().unwrap().clear();
        let resp = t.send(Method::POST, &path, None).await;
        assert_eq!(resp.status(), 202, "{phase}");
        assert_eq!(resp.json::<Value>().await.unwrap()["status"], "applying", "{phase}");
        let row = t.merge_state(m.id);
        assert_eq!((row.status, row.failed_phase, row.error), (MergeStatus::Applying, None, None), "{phase}");
        assert_eq!(t.merge_jobs(), vec![MergeJob::Apply { merge_id: m.id }], "{phase}");
    }

    // A publish failure puts it back, phase and all.
    t.set_merge(m.id, |r| {
        r.status = MergeStatus::Failed;
        r.failed_phase = Some("tx".into());
    });
    *t.jobs.fail_merge_publish.lock().unwrap() = true;
    let err = error_of(t.send(Method::POST, &path, None).await, 503).await;
    assert_eq!(code_of(&err), "worker_unavailable");
    let row = t.merge_state(m.id);
    assert_eq!((row.status, row.failed_phase.as_deref()), (MergeStatus::Failed, Some("tx")));
}

#[tokio::test]
async fn retry_only_after_online_failure() {
    let t = branching().await;
    let dev = t.mergeable();
    // A failure with no phase (a compute's) or an unknown one isn't retried.
    let m = t.seed_merge(dev, MergeStatus::Failed, |r| r.failed_phase = None);
    let path = format!("{}/retry", t.merge_path(m.id));
    let err = error_of(t.send(Method::POST, &path, None).await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    t.set_merge(m.id, |r| r.failed_phase = Some("phase1".into()));
    let err = error_of(t.send(Method::POST, &path, None).await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    t.set_merge(m.id, |r| r.status = MergeStatus::Draft);
    let err = error_of(t.send(Method::POST, &path, None).await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    assert!(t.merge_jobs().is_empty());

    t.set_merge(m.id, |r| {
        r.status = MergeStatus::Failed;
        r.failed_phase = Some("online".into());
        r.error = Some("lock timeout".into());
    });
    let resp = t.send(Method::POST, &path, None).await;
    assert_eq!(resp.status(), 202);
    assert_eq!(resp.json::<Value>().await.unwrap()["status"], "online");
    let row = t.merge_state(m.id);
    assert_eq!((row.status, row.failed_phase, row.error), (MergeStatus::Online, None, None));
    assert_eq!(t.merge_jobs(), vec![MergeJob::Resume { merge_id: m.id }]);

    // Another merge applying on the target: `merge_busy`.
    let other = t.ready("other");
    t.set_merge(m.id, |r| {
        r.status = MergeStatus::Failed;
        r.failed_phase = Some("online".into());
    });
    let busy = t.seed_merge(other, MergeStatus::Applying, |_| {});
    let err = error_of(t.send(Method::POST, &path, None).await, 409).await;
    assert_eq!(code_of(&err), "merge_busy");
    assert_eq!(err["error"]["merge_id"], json!(busy.id));
}

/// What the worker leaves on a merge applied with drift: the choice skipped.
fn drift_result() -> Value {
    json!({
        "created_roles": [],
        "drift": [{ "key": { "kind": "table", "schema": "public", "table": null, "name": "items" },
                    "kind": "missing", "expected": null, "actual": null, "produced": false }],
        "after": { "choice": "keep", "skipped": "the target doesn't read back as merged" },
    })
}

// E21: with drift, the after-merge choice is run by hand, once.
#[tokio::test]
async fn finish_runs_the_after_merge_choice_by_hand_once() {
    use chelabase_nats::MergeAfter;
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::Done, |r| r.result = Some(drift_result()));
    let path = format!("{}/finish", t.merge_path(m.id));
    let reset = Some(json!({ "after": "reset" }));

    // Only after drift.
    let err = error_of(t.send(Method::POST, &path, reset.clone()).await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    t.set_merge(m.id, |r| r.status = MergeStatus::AppliedWithDrift);
    // The body is checked.
    for bad in [json!({ "after": "nuke" }), json!({ "after": "keep", "x": 1 }), json!({})] {
        let err = error_of(t.send(Method::POST, &path, Some(bad.clone())).await, 422).await;
        assert_eq!(code_of(&err), "invalid_request", "{bad}");
    }
    assert!(t.merge_jobs().is_empty());

    let resp = t.send(Method::POST, &path, reset.clone()).await;
    assert_eq!(resp.status(), 202);
    assert_eq!(resp.json::<Value>().await.unwrap()["status"], "applied_with_drift");
    let row = t.merge_state(m.id);
    assert_eq!(row.status, MergeStatus::AppliedWithDrift);
    let result = row.result.unwrap();
    assert_eq!(result["after"], json!({ "choice": "reset", "requested": true }));
    assert_eq!(result["drift"], drift_result()["drift"], "the rest of the result stays");
    assert_eq!(
        t.merge_jobs(),
        vec![MergeJob::Finish { merge_id: m.id, after: MergeAfter::Reset }]
    );

    // Once: requested (or done), a second call is refused.
    let err = error_of(t.send(Method::POST, &path, reset.clone()).await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    t.set_merge(m.id, |r| {
        r.result.as_mut().unwrap()["after"] = json!({ "choice": "reset", "outcome": "done" });
    });
    let err = error_of(t.send(Method::POST, &path, reset).await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    assert_eq!(t.merge_jobs().len(), 1);
}

#[tokio::test]
async fn finish_from_the_parent_only_keeps_and_a_publish_failure_is_undone() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::AppliedWithDrift, |r| {
        r.result = Some(drift_result());
        r.direction = chelabase_metadata::MergeDirection::FromParent;
    });
    let path = format!("{}/finish", t.merge_path(m.id));
    // An update from the parent only keeps (D11, T11 R9).
    let err = error_of(t.send(Method::POST, &path, Some(json!({ "after": "delete" }))).await, 409).await;
    assert_eq!(code_of(&err), "conflict");

    *t.jobs.fail_merge_publish.lock().unwrap() = true;
    let err = error_of(t.send(Method::POST, &path, Some(json!({ "after": "keep" }))).await, 503).await;
    assert_eq!(code_of(&err), "worker_unavailable");
    assert_eq!(t.merge_state(m.id).result, Some(drift_result()), "the request is undone");
}

#[tokio::test]
async fn cancel_stops_what_nothing_is_applying() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::Draft, |_| {});
    let path = format!("{}/cancel", t.merge_path(m.id));
    let resp = t.send(Method::POST, &path, None).await;
    assert_eq!(resp.status(), 202);
    assert_eq!(resp.json::<Value>().await.unwrap()["status"], "cancelled");
    assert!(t.merge_jobs().is_empty(), "a draft has nothing to stop");

    // The worker holds a rehearsing merge: it is told to tear down.
    t.set_merge(m.id, |r| r.status = MergeStatus::Rehearsing);
    assert_eq!(t.send(Method::POST, &path, None).await.status(), 202);
    assert_eq!(t.merge_state(m.id).status, MergeStatus::Cancelled);
    assert_eq!(t.merge_jobs(), vec![MergeJob::Cancel { merge_id: m.id }]);

    for status in [MergeStatus::Applying, MergeStatus::Online, MergeStatus::Finishing, MergeStatus::Done, MergeStatus::Failed, MergeStatus::Cancelled] {
        t.set_merge(m.id, |r| r.status = status);
        let err = error_of(t.send(Method::POST, &path, None).await, 409).await;
        assert_eq!(code_of(&err), "conflict", "{status:?}");
        assert_eq!(t.merge_state(m.id).status, status);
    }
}

#[tokio::test]
async fn export_is_text_plain_attachment() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::Draft, |_| {});
    let path = format!("{}/plan.sql", t.merge_path(m.id));
    // No plan yet.
    let err = error_of(t.send(Method::GET, &path, None).await, 409).await;
    assert_eq!(code_of(&err), "conflict");

    let plan = json!({"steps": [{
        "id": "s1", "kind": "generated", "phase": "tx", "sql": "CREATE TABLE t (id int)",
        "items": [], "data_loss": false, "rewrite": false, "security": false, "locks": [],
        "role_op": null, "online": null
    }]});
    t.set_merge(m.id, |r| r.plan = Some(plan.clone()));
    let resp = t.send(Method::GET, &path, None).await;
    assert_eq!(resp.status(), 200);
    let headers = resp.headers().clone();
    assert_eq!(headers["content-type"], "text/plain; charset=utf-8");
    assert_eq!(headers["content-disposition"], format!("attachment; filename=\"merge-{}.sql\"", m.id));
    assert_eq!(headers["cache-control"], "no-store");
    let opts = chelabase_schema::plan::PlanOptions {
        lock_timeout_ms: chelabase_schema::plan::lock_timeout_ms(&m.options),
        merge_id: m.id,
    };
    let expected =
        chelabase_schema::export::export_sql(&serde_json::from_value(plan).unwrap(), &opts);
    assert_eq!(resp.text().await.unwrap(), expected);
}

#[tokio::test]
async fn branch_ops_refused_while_merge_busy() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::Rehearsing, |_| {});
    for branch in [dev, t.main()] {
        let reset = t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(branch)), None).await;
        let restore = t
            .send(Method::POST, &format!("{}/restore", t.branch(branch)), Some(json!({ "at": { "lsn": "0/16B3748" } })))
            .await;
        let delete = t.send(Method::DELETE, &t.branch(branch), None).await;
        for resp in [reset, restore, delete] {
            let err = error_of(resp, 409).await;
            // Main has no parent (and a child): its own refusal may come first, but never a job.
            if branch == dev {
                assert_eq!(code_of(&err), "branch_merge_busy", "{err}");
                assert_eq!(err["error"]["merge_id"], json!(m.id));
            }
        }
    }
    assert!(t.published().is_empty(), "no branch job was published");
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Ready);
    assert_eq!(t.metadata.branch(t.main()).status, BranchStatus::Ready);
    assert_eq!(t.merge_state(m.id).status, MergeStatus::Rehearsing, "a running merge isn't cancelled");

    // Applying holds the target (main) too: restore of main is refused for the merge.
    t.set_merge(m.id, |r| r.status = MergeStatus::Applying);
    let restore = t
        .send(Method::POST, &format!("{}/restore", t.branch(t.main())), Some(json!({ "at": { "lsn": "0/16B3748" } })))
        .await;
    let err = error_of(restore, 409).await;
    assert_eq!(code_of(&err), "branch_merge_busy");
    // Once nothing runs, the branch ops work again.
    t.set_merge(m.id, |r| r.status = MergeStatus::Done);
    assert_eq!(t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await.status(), 202);
}

#[tokio::test]
async fn delete_branch_cancels_drafts() {
    let t = branching().await;
    let dev = t.mergeable();
    let draft = t.seed_merge(dev, MergeStatus::Draft, |_| {});
    let computing = t.seed_merge(dev, MergeStatus::Computing, |r| r.database_name = "web1".into());
    let done = t.seed_merge(dev, MergeStatus::Done, |_| {});
    let resp = t.send(Method::DELETE, &t.branch(dev), None).await;
    assert_eq!(resp.status(), 202);
    assert_eq!(t.merge_state(draft.id).status, MergeStatus::Cancelled);
    assert_eq!(t.merge_state(computing.id).status, MergeStatus::Cancelled);
    assert_eq!(t.merge_state(done.id).status, MergeStatus::Done);

    // A refused delete cancels nothing.
    let other = t.ready("other");
    t.add("kid", other, BranchKind::Normal, BranchStatus::Ready);
    let kept = t.seed_merge(other, MergeStatus::Draft, |_| {});
    assert_eq!(t.send(Method::DELETE, &t.branch(other), None).await.status(), 409);
    assert_eq!(t.merge_state(kept.id).status, MergeStatus::Draft);
}

#[tokio::test]
async fn rehearsal_branches_are_not_listed() {
    let t = branching().await;
    let dev = t.ready("dev");
    let rehearsal = t.add("_rehearsal-0a1b2c3d", dev, BranchKind::Rehearsal, BranchStatus::Ready);
    let list: Vec<Value> = t.send(Method::GET, &t.branches(), None).await.json().await.unwrap();
    assert!(list.iter().all(|b| b["id"] != json!(rehearsal)), "{list:?}");
    assert_eq!(list.len(), 2);
    // Nor reachable by id.
    for (method, path) in [
        (Method::DELETE, t.branch(rehearsal)),
        (Method::PATCH, t.branch(rehearsal)),
        (Method::POST, format!("{}/reset-to-parent", t.branch(rehearsal))),
        (Method::GET, format!("{}/connection", t.branch(rehearsal))),
    ] {
        let err = error_of(t.send(method.clone(), &path, Some(json!({ "protected": true }))).await, 404).await;
        assert_eq!(code_of(&err), "not_found", "{method} {path}");
    }
    assert_eq!(t.metadata.branch(rehearsal).status, BranchStatus::Ready);
}

#[tokio::test]
async fn rehearsal_branch_refused_by_console_and_sql_editor() {
    let t = branching().await;
    let dev = t.ready("dev");
    let rehearsal = t.add("_rehearsal-0a1b2c3d", dev, BranchKind::Rehearsal, BranchStatus::Ready);
    let base = t.branch(rehearsal);
    let calls = [
        (Method::POST, format!("{base}/console/query"), Some(json!({ "role": null, "tree": { "v": 1, "op": "select", "from": "t" } }))),
        (Method::GET, format!("{base}/console/schema"), None),
        (Method::POST, format!("{base}/sql"), Some(json!({ "run_id": uuid::Uuid::new_v4(), "sql": "select 1" }))),
        (Method::GET, format!("{base}/sql/schema"), None),
        (Method::GET, format!("{base}/roles"), None),
        (Method::POST, format!("{base}/roles"), Some(json!({ "name": "app" }))),
        (Method::GET, format!("{base}/keys"), None),
        (Method::GET, format!("{base}/databases"), None),
        (Method::POST, format!("{base}/merges"), Some(into_parent())),
    ];
    for (method, path, body) in calls {
        let resp = t.send(method.clone(), &path, body).await;
        assert!(matches!(resp.status().as_u16(), 404), "{method} {path}: {}", resp.status());
    }
}

// --- T14 F3: the merge state machine ---------------------------------------------------------

/// A merge in `state` (source `dev`, target main) that every action could take: an analysis
/// without conflicts, a plan with one unflagged step, and a drift result whose choice was skipped.
fn seed_in_state(t: &Branching, dev: BranchId, state: chelabase_metadata::MergeState) -> MergeRecord {
    // One merge at a time: the earlier ones are out of the way.
    for r in t.metadata.merges.lock().unwrap().iter_mut() {
        r.status = MergeStatus::Cancelled;
        r.failed_phase = None;
    }
    t.seed_merge(dev, state.status, |r| {
        r.failed_phase = state.failed_phase.map(str::to_string);
        r.analysis = Some(json!({"conflicts": []}));
        r.plan = Some(plan_with(json!({
            "id": "s1", "kind": "generated", "phase": "tx", "sql": "CREATE TABLE t (id int)",
            "items": [], "data_loss": false, "rewrite": false, "security": false, "locks": [],
            "role_op": null, "online": null
        })));
        r.result = Some(drift_result());
    })
}

/// The request of `action` on merge `id`.
async fn act(t: &Branching, id: uuid::Uuid, action: chelabase_metadata::MergeAction) -> reqwest::Response {
    use chelabase_metadata::MergeAction::*;
    let path = t.merge_path(id);
    match action {
        Edit => t.send(Method::PATCH, &path, Some(json!({ "version": 3, "steps": [] }))).await,
        EditKeepsPlan => t.send(Method::PATCH, &path, Some(json!({ "version": 3, "acknowledgements": ["a"] }))).await,
        Rehearse => t.send(Method::POST, &format!("{path}/rehearse"), None).await,
        Apply => t.send(Method::POST, &format!("{path}/apply"), Some(json!({ "confirm": true }))).await,
        Recompute => t.send(Method::POST, &format!("{path}/recompute"), None).await,
        Retry => t.send(Method::POST, &format!("{path}/retry"), None).await,
        Cancel => t.send(Method::POST, &format!("{path}/cancel"), None).await,
        Abandon => t.send(Method::POST, &format!("{path}/abandon"), None).await,
        Finish => t.send(Method::POST, &format!("{path}/finish"), Some(json!({ "after": "keep" }))).await,
    }
}

/// The job an allowed `action` publishes from `state` (T14 F3 fix round, M9): the worker's work
/// for the new status; none for a move nothing runs.
fn job_of(state: chelabase_metadata::MergeState, action: chelabase_metadata::MergeAction, id: uuid::Uuid) -> Vec<MergeJob> {
    use chelabase_metadata::MergeAction::*;
    match action {
        Edit | Recompute => vec![MergeJob::Compute { merge_id: id }],
        EditKeepsPlan | Abandon => vec![],
        Rehearse => vec![MergeJob::Rehearse { merge_id: id }],
        Apply => vec![MergeJob::Apply { merge_id: id }],
        // E20: after an online failure the online phase resumes; before it the apply runs again.
        Retry if state.failed_phase == Some("online") => vec![MergeJob::Resume { merge_id: id }],
        Retry => vec![MergeJob::Apply { merge_id: id }],
        // A worker holds a computing or rehearsing merge: it's told to stop.
        Cancel if matches!(state.status, MergeStatus::Computing | MergeStatus::Rehearsing) => {
            vec![MergeJob::Cancel { merge_id: id }]
        }
        Cancel => vec![],
        Finish => vec![MergeJob::Finish { merge_id: id, after: chelabase_nats::MergeAfter::Keep }],
    }
}

// The invariant (T14 F3), exhaustively: every action in every state moves the merge exactly as
// `MergeState::after` says, clears its failed phase, publishes the new status's job (M9), logs an
// apply attempt's start for a move to `applying` (I2) and lists the leftovers of an abandon (E27);
// it's refused (409, nothing written, no job) otherwise.
#[tokio::test]
async fn every_action_in_every_state_follows_the_table() {
    use chelabase_metadata::{MergeAction::*, MergeState};
    let t = branching().await;
    let dev = t.mergeable();
    for state in MergeState::all() {
        for action in [Edit, EditKeepsPlan, Rehearse, Apply, Recompute, Retry, Cancel, Abandon, Finish] {
            let m = seed_in_state(&t, dev, state);
            t.jobs.merge_jobs.lock().unwrap().clear();
            t.metadata.merge_events.lock().unwrap().clear();
            let resp = act(&t, m.id, action).await;
            let code = resp.status().as_u16();
            let row = t.merge_state(m.id);
            let at = format!("{state:?} {action:?}");
            match state.after(action) {
                Some(to) => {
                    assert!((200..300).contains(&code), "{at}: {code} {}", resp.text().await.unwrap());
                    assert_eq!(row.status, to, "{at}");
                    assert_eq!(row.failed_phase, None, "{at}");
                    assert_eq!(t.merge_jobs(), job_of(state, action, m.id), "{at}");
                    let starts: Vec<Value> = t
                        .metadata
                        .merge_events
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|e| e.merge_id == m.id && e.kind == "status")
                        .filter_map(|e| e.detail.clone())
                        .filter(|d| d["to"] == "applying")
                        .collect();
                    assert_eq!(starts.len(), usize::from(to == MergeStatus::Applying), "{at}");
                    let leftovers = row.result.as_ref().map(|r| r["leftovers"].is_array());
                    assert_eq!(leftovers, Some(action == Abandon), "{at}");
                }
                None => {
                    assert_eq!(code, 409, "{at}");
                    assert_eq!((row.status, row.failed_phase.as_deref()), (state.status, state.failed_phase), "{at}");
                    assert!(t.merge_jobs().is_empty(), "{at}");
                }
            }
        }
    }
}

// T14 F3 fix round (M4): apply and rehearse take the version the user saw: another one is a 409
// `merge_version_conflict` (the merge as it is now in `error.current`), and nothing moves. A save
// of the after-merge choice keeps a rehearsed merge rehearsed (E34), so without it an apply could
// run a choice the user never saw.
#[tokio::test]
async fn apply_and_rehearse_take_the_version_the_user_saw() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::Rehearsed, |r| {
        r.analysis = Some(json!({"conflicts": []}));
        r.plan = Some(json!({"steps": []}));
    });
    let apply = format!("{}/apply", t.merge_path(m.id));
    let err = error_of(t.send(Method::POST, &apply, Some(json!({ "version": m.version - 1 }))).await, 409).await;
    assert_eq!(code_of(&err), "merge_version_conflict");
    assert_eq!(err["error"]["current"]["version"], json!(m.version));
    assert_eq!(t.merge_state(m.id).status, MergeStatus::Rehearsed);
    assert!(t.merge_jobs().is_empty());
    assert_eq!(t.send(Method::POST, &apply, Some(json!({ "version": m.version }))).await.status(), 202);

    let d = seed_in_state(&t, dev, chelabase_metadata::MergeState::of(MergeStatus::Draft, None));
    let rehearse = format!("{}/rehearse", t.merge_path(d.id));
    let err = error_of(t.send(Method::POST, &rehearse, Some(json!({ "version": d.version + 1 }))).await, 409).await;
    assert_eq!(code_of(&err), "merge_version_conflict");
    assert_eq!(t.merge_state(d.id).status, MergeStatus::Draft);
    assert_eq!(t.send(Method::POST, &rehearse, Some(json!({ "version": d.version }))).await.status(), 202);
}

// T14 F3 fix round (M7): `merge_busy` names a merge failed half applied too, not only one running.
#[tokio::test]
async fn merge_busy_names_a_half_applied_merge() {
    let t = branching().await;
    let dev = t.mergeable();
    let half = seed_in_state(&t, dev, chelabase_metadata::MergeState::of(MergeStatus::Failed, Some("online")));
    let other = t.ready("other");
    t.metadata.add_branch_db(other, "app", "postgres", DatabaseOrigin::Sql);
    let err = error_of(t.create_merge(other, into_parent()).await, 409).await;
    assert_eq!(code_of(&err), "merge_busy");
    assert_eq!(err["error"]["merge_id"], json!(half.id));
    assert!(message_of(&err).contains("failed half applied"), "{err}");
    assert!(message_of(&err).contains("retry or abandon"), "{err}");
}

// T14 F3 fix round (I1, the hold invariant on every path): a delete of a branch that isn't ready
// (no claim) is refused under the database's lock too, when a merge holds it after the pre-check.
#[tokio::test]
async fn a_delete_without_a_claim_is_refused_under_the_lock() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = seed_in_state(&t, dev, chelabase_metadata::MergeState::of(MergeStatus::Failed, Some("online")));
    t.metadata.branches.lock().unwrap().get_mut(&dev).unwrap().status = BranchStatus::Failed;
    *t.metadata.busy_precheck_misses.lock().unwrap() = true;
    let err = error_of(t.send(Method::DELETE, &t.branch(dev), None).await, 409).await;
    assert_eq!(code_of(&err), "branch_merge_busy", "{err}");
    assert_eq!(err["error"]["merge_id"], json!(m.id));
    assert!(t.published().is_empty());
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Failed);
}

// The invariant's other half (T14 F3, E27): reset, restore and delete of the source and of the
// target are refused (`branch_merge_busy`) exactly in the states that hold the branches.
#[tokio::test]
async fn branch_ops_in_every_state_follow_the_table() {
    use chelabase_metadata::MergeState;
    let t = branching().await;
    for (i, state) in MergeState::all().into_iter().enumerate() {
        for op in ["reset", "restore", "delete"] {
            // A target with the source as its child: an op on the target is otherwise refused
            // for its child, one on the source goes through.
            let target = t.ready(&format!("p{i}{op}"));
            let source = t.add(&format!("c{i}{op}"), target, BranchKind::Normal, BranchStatus::Ready);
            let m = t.seed_merge(source, state.status, |r| {
                r.target_branch_id = target.as_uuid();
                r.failed_phase = state.failed_phase.map(str::to_string);
            });
            for (branch, free) in [(source, None), (target, Some("has_children"))] {
                let resp = match op {
                    "reset" => t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(branch)), None).await,
                    "restore" => {
                        t.send(Method::POST, &format!("{}/restore", t.branch(branch)), Some(json!({ "at": { "lsn": "0/16B3748" } })))
                            .await
                    }
                    _ => t.send(Method::DELETE, &t.branch(branch), None).await,
                };
                let code = resp.status().as_u16();
                let body: Value = resp.json().await.unwrap_or(Value::Null);
                if state.holds_branches() {
                    assert_eq!(code, 409, "{state:?} {op} {body}");
                    assert_eq!(code_of(&body), "branch_merge_busy", "{state:?} {op}");
                    assert_eq!(body["error"]["merge_id"], json!(m.id));
                } else {
                    match free {
                        None => assert_eq!(code, 202, "{state:?} {op} {body}"),
                        Some(refusal) => assert_eq!(code_of(&body), refusal, "{state:?} {op}"),
                    }
                }
            }
            t.set_merge(m.id, |r| r.status = MergeStatus::Cancelled);
        }
    }
}

// T14 M12: a merge that takes the branch between the API's pre-check and its claim is caught by
// the claim (checked under the branch's lock): refused, nothing published, the branch ready.
#[tokio::test]
async fn a_merge_that_takes_the_branch_after_the_precheck_is_refused_by_the_claim() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::Applying, |_| {});
    *t.metadata.busy_precheck_misses.lock().unwrap() = true;
    for resp in [
        t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await,
        t.send(Method::POST, &format!("{}/restore", t.branch(dev)), Some(json!({ "at": { "lsn": "0/16B3748" } }))).await,
        t.send(Method::DELETE, &t.branch(dev), None).await,
    ] {
        let err = error_of(resp, 409).await;
        assert_eq!(code_of(&err), "branch_merge_busy", "{err}");
        assert_eq!(err["error"]["merge_id"], json!(m.id));
    }
    assert!(t.published().is_empty());
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Ready);
}

// I4: a rehearsal branch of the target still there (a cancel's teardown pending) holds it.
#[tokio::test]
async fn a_leftover_rehearsal_holds_its_branch() {
    let t = branching().await;
    let dev = t.mergeable();
    let rehearsal = t.add("_rehearsal-0a1b2c3d", dev, BranchKind::Rehearsal, BranchStatus::Ready);
    let err = error_of(t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await, 409).await;
    assert_eq!(code_of(&err), "branch_merge_busy");
    assert_eq!(err["error"]["rehearsal_branch_id"], json!(rehearsal));
    t.metadata.branches.lock().unwrap().get_mut(&rehearsal).unwrap().status = BranchStatus::Deleted;
    assert_eq!(t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await.status(), 202);
}

// E27: Abandon gives up a merge that failed half applied: `cancelled`, with what phase 1 left on
// the target and phase 2 didn't finish listed in `result.leftovers`.
#[tokio::test]
async fn abandon_cancels_a_half_applied_merge_and_lists_leftovers() {
    let t = branching().await;
    let dev = t.mergeable();
    let step = |id: &str, phase: &str, sql: &str, online: Value| {
        json!({ "id": id, "kind": "generated", "phase": phase, "sql": sql, "items": [],
                "data_loss": false, "rewrite": false, "security": false, "locks": [],
                "role_op": null, "online": online })
    };
    let nn = "chelabase_merge_nn_2_0123456789abcdef0123456789abcdef";
    let plan = json!({ "steps": [
        step("fk", "tx", "ALTER TABLE public.o ADD CONSTRAINT o_fk FOREIGN KEY (c) REFERENCES public.c (id) NOT VALID", Value::Null),
        step("nn", "tx", &format!("ALTER TABLE public.t ADD CONSTRAINT {nn} CHECK (a IS NOT NULL) NOT VALID"), Value::Null),
        step("validate-fk", "online", "ALTER TABLE public.o VALIDATE CONSTRAINT o_fk",
             json!({ "constraint_validated": { "schema": "public", "table": "o", "name": "o_fk" } })),
        step("validate-nn", "online", &format!("ALTER TABLE public.t VALIDATE CONSTRAINT {nn}"),
             json!({ "constraint_validated": { "schema": "public", "table": "t", "name": nn } })),
        step("set-nn", "online", "ALTER TABLE public.t ALTER COLUMN a SET NOT NULL",
             json!({ "not_null_set": { "schema": "public", "table": "t", "column": "a" } })),
        step("drop-nn", "online", &format!("ALTER TABLE public.t DROP CONSTRAINT IF EXISTS {nn}"), Value::Null),
    ]});
    let m = t.seed_merge(dev, MergeStatus::Failed, |r| {
        r.failed_phase = Some("online".into());
        r.plan = Some(plan);
        r.result = None;
    });
    // The nn check was validated and the column set; the fk's validation and the drop weren't.
    for (step, phase) in [("validate-nn", "online"), ("set-nn", "online")] {
        let mut e = MergeEvent::new(m.id, "step");
        e.step_id = Some(step.into());
        e.phase = Some(phase.into());
        e.outcome = Some("ok".into());
        t.metadata.merge_events.lock().unwrap().push(e);
    }
    let resp = t.send(Method::POST, &format!("{}/abandon", t.merge_path(m.id)), None).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "cancelled");
    let leftovers = json!([
        { "kind": "not_valid_constraint", "schema": "public", "table": "o", "name": "o_fk" },
        { "kind": "not_null_check", "schema": "public", "table": "t", "name": nn },
    ]);
    assert_eq!(body["result"]["leftovers"], leftovers);
    let row = t.merge_state(m.id);
    assert_eq!(row.status, MergeStatus::Cancelled);
    assert_eq!(row.result.unwrap()["leftovers"], leftovers);
    assert!(t.merge_jobs().is_empty(), "nothing for the worker");
    // The target is free again.
    assert_eq!(t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await.status(), 202);
}

// T14 M11: every merge route answers 404 for a merge of another database.
#[tokio::test]
async fn every_merge_route_is_404_for_another_database() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::Draft, |r| r.database_id = uuid::Uuid::new_v4());
    let path = t.merge_path(m.id);
    for (method, p, body) in [
        (Method::GET, path.clone(), None),
        (Method::PATCH, path.clone(), Some(json!({ "version": 3 }))),
        (Method::POST, format!("{path}/rehearse"), None),
        (Method::POST, format!("{path}/apply"), None),
        (Method::POST, format!("{path}/recompute"), None),
        (Method::POST, format!("{path}/retry"), None),
        (Method::POST, format!("{path}/cancel"), None),
        (Method::POST, format!("{path}/abandon"), None),
        (Method::POST, format!("{path}/finish"), Some(json!({ "after": "keep" }))),
        (Method::GET, format!("{path}/plan.sql"), None),
    ] {
        let err = error_of(t.send(method.clone(), &p, body).await, 404).await;
        assert_eq!(code_of(&err), "not_found", "{method} {p}");
    }
    assert_eq!(t.merge_state(m.id).status, MergeStatus::Draft);
}

// T14 M11: `getMerge` and the edit's answers serve the plan's steps only (not the expected model,
// the own-SQL keys, the digest or the fingerprint, ~100 KB a poll).
#[tokio::test]
async fn get_merge_serves_only_the_plan_steps() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::Draft, |r| {
        r.plan = Some(json!({ "steps": [], "target_digest": "d", "expected": {"big": true}, "own_sql": [], "precheck_fingerprint": "f", "follows_rename": [] }));
    });
    let got: Value = t.send(Method::GET, &t.merge_path(m.id), None).await.json().await.unwrap();
    assert_eq!(got["merge"]["plan"], json!({ "steps": [] }));
    let err = error_of(t.send(Method::PATCH, &t.merge_path(m.id), Some(json!({ "version": 1 }))).await, 409).await;
    assert_eq!(err["error"]["current"]["plan"], json!({ "steps": [] }));
}

// F1 minor 3 (orchestrator side): a step id the plan reserves (`:`, `.`, control characters) is
// refused at the save.
#[tokio::test]
async fn step_ids_with_reserved_characters_are_refused() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::Draft, |_| {});
    // `phase1`: phase 1's own entry in the merge's log (T14 F3 fix round, M8).
    for id in ["create-index:public.t.ix", "a.1", "line\nbreak", "tab\t", "phase1"] {
        let body = json!({ "version": 3, "steps": [{ "id": id, "after": null, "sql": "SELECT 1" }] });
        let err = error_of(t.send(Method::PATCH, &t.merge_path(m.id), Some(body)).await, 422).await;
        assert_eq!(code_of(&err), "invalid_request", "{id:?}");
    }
    assert!(t.merge_jobs().is_empty());
}

// E29a: a rename step (breaking) needs an acknowledgement like a drop.
#[tokio::test]
async fn a_breaking_step_needs_an_acknowledgement() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::Rehearsed, |r| {
        r.analysis = Some(json!({"conflicts": []}));
        r.plan = Some(plan_with(json!({
            "id": "role:rename:app", "kind": "generated", "phase": "roles", "sql": "ALTER ROLE app RENAME TO web",
            "items": ["role:name:web"], "data_loss": false, "rewrite": false, "security": false, "locks": [],
            "role_op": { "rename": { "oid": 16500, "from": "app", "to": "web" } }, "online": null,
            "breaking": { "role_rename": { "role": "app", "new_name": "web" } }
        })));
    });
    let path = format!("{}/apply", t.merge_path(m.id));
    let err = error_of(t.send(Method::POST, &path, None).await, 409).await;
    assert_eq!(code_of(&err), "merge_drops_unacknowledged");
    assert!(message_of(&err).contains("role:name:web"), "{err}");
    t.set_merge(m.id, |r| r.acknowledgements = json!(["role:name:web"]));
    assert_eq!(t.send(Method::POST, &path, None).await.status(), 202);
}

// T10 minor (T14 Studio/API minor 14): rehearse and apply refuse a branch that isn't ready.
#[tokio::test]
async fn rehearse_and_apply_refuse_a_branch_that_isnt_ready() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::Draft, |r| {
        r.analysis = Some(json!({"conflicts": []}));
        r.plan = Some(json!({"steps": []}));
    });
    t.metadata.branches.lock().unwrap().get_mut(&dev).unwrap().status = BranchStatus::Resetting;
    let err = error_of(t.send(Method::POST, &format!("{}/rehearse", t.merge_path(m.id)), None).await, 409).await;
    assert_eq!(code_of(&err), "branch_not_ready");
    t.set_merge(m.id, |r| r.status = MergeStatus::Rehearsed);
    let err = error_of(t.send(Method::POST, &format!("{}/apply", t.merge_path(m.id)), None).await, 409).await;
    assert_eq!(code_of(&err), "branch_not_ready");
    assert!(t.merge_jobs().is_empty());
    assert_eq!(t.merge_state(m.id).status, MergeStatus::Rehearsed);
}

// T14 M11: a rehearsal runs the draft the user saw: a save landing between the read and the move
// (a newer version) refuses it.
#[tokio::test]
async fn rehearse_is_guarded_by_the_version() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::Draft, |r| {
        r.analysis = Some(json!({"conflicts": []}));
        r.plan = Some(json!({"steps": []}));
    });
    *t.metadata.bump_merge_version_on_write.lock().unwrap() = true;
    let err = error_of(t.send(Method::POST, &format!("{}/rehearse", t.merge_path(m.id)), None).await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    assert_eq!(t.merge_state(m.id).status, MergeStatus::Draft);
    assert!(t.merge_jobs().is_empty());
}

// T14 M8: of two concurrent requests for the after-merge choice only one is recorded and
// published.
#[tokio::test]
async fn concurrent_finish_requests_publish_once() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::AppliedWithDrift, |r| r.result = Some(drift_result()));
    let path = format!("{}/finish", t.merge_path(m.id));
    *t.metadata.bump_merge_version_on_write.lock().unwrap() = false;
    *t.metadata.result_changes_before_write.lock().unwrap() = Some(json!({ "after": { "choice": "delete", "requested": true } }));
    let err = error_of(t.send(Method::POST, &path, Some(json!({ "after": "keep" }))).await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    assert!(t.merge_jobs().is_empty(), "the other request won");
}

// E34: acknowledgements and the after-merge choice keep a rehearsed merge rehearsed (no
// recompute); any other edit returns it to draft through a recompute.
#[tokio::test]
async fn acknowledgements_keep_a_rehearsed_merge_rehearsed() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::Rehearsed, |r| r.rehearsal = Some(json!({"ok": true})));
    let path = t.merge_path(m.id);
    let resp = t.send(Method::PATCH, &path, Some(json!({ "version": 3, "acknowledgements": ["x"], "options": { "after": "reset" } }))).await;
    assert_eq!(resp.status(), 200);
    let row = t.merge_state(m.id);
    assert_eq!((row.status, row.rehearsal.clone(), row.version), (MergeStatus::Rehearsed, Some(json!({"ok": true})), 4));
    assert_eq!(row.options["after"], "reset");
    assert!(t.merge_jobs().is_empty(), "nothing to recompute");
    // The lock timeout is part of the plan's script: a recompute.
    let resp = t.send(Method::PATCH, &path, Some(json!({ "version": 4, "options": { "lock_timeout_ms": 2000 } }))).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(t.merge_state(m.id).status, MergeStatus::Computing);
    assert_eq!(t.merge_jobs(), vec![MergeJob::Compute { merge_id: m.id }]);
}

// I2: applying and retrying log the attempt's start (`status` → `applying`) with the move, so the
// worker scopes "started" to the attempt.
#[tokio::test]
async fn apply_and_retry_log_the_attempt_start() {
    let t = branching().await;
    let dev = t.mergeable();
    let m = t.seed_merge(dev, MergeStatus::Rehearsed, |r| {
        r.analysis = Some(json!({"conflicts": []}));
        r.plan = Some(json!({"steps": []}));
    });
    assert_eq!(t.send(Method::POST, &format!("{}/apply", t.merge_path(m.id)), None).await.status(), 202);
    t.set_merge(m.id, |r| {
        r.status = MergeStatus::Failed;
        r.failed_phase = Some("tx".into());
    });
    assert_eq!(t.send(Method::POST, &format!("{}/retry", t.merge_path(m.id)), None).await.status(), 202);
    let logged: Vec<Value> = t
        .metadata
        .merge_events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.merge_id == m.id && e.kind == "status")
        .filter_map(|e| e.detail.clone())
        .collect();
    assert_eq!(logged, vec![json!({"to": "applying"}), json!({"to": "applying"})]);
}

// --- Members: who may call what (members and roles, T5) --------------------------------------

#[tokio::test]
async fn removed_member_is_refused_with_a_valid_token() {
    let w = member_world(Mode::SelfHosted).await;
    w.metadata.add_granted(w.tenant, "bob", w.project, PlatformRole::Admin, &[]);
    let path = format!("/v1/databases/{}", w.db);
    assert_eq!(w.app.send_as("bob", "org-1", Method::GET, &path, None).await.status(), 200);

    w.metadata.set_member(w.tenant, "bob", |m| m.status = MemberStatus::Removed);
    let err = error_of(w.app.send_as("bob", "org-1", Method::GET, &path, None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    // The request neither revives the member nor makes a new one.
    assert_eq!(w.metadata.member(w.tenant, "bob").unwrap().status, MemberStatus::Removed);
    assert_eq!(w.metadata.members.lock().unwrap().iter().filter(|m| m.identity_user_id == "bob").count(), 1);

    // A removed tenant admin is refused too, on every project.
    w.metadata.set_member(w.tenant, DEFAULT_SUB, |m| m.status = MemberStatus::Removed);
    let err = error_of(w.app.send_as(DEFAULT_SUB, "org-1", Method::GET, &path, None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    let err = error_of(w.app.send_as(DEFAULT_SUB, "org-1", Method::POST, "/v1/projects", Some(json!({"name": "x"}))).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
}

// I1: an invited member whose invite expired (or never existed) stays invited and gets
// `no_access_yet`, whatever token they hold.
#[tokio::test]
async fn expired_invite_sign_in_is_no_access_yet() {
    let w = member_world(Mode::SelfHosted).await;
    let id = w.metadata.add_member(w.tenant, "dave", MemberStatus::Invited, false);
    w.metadata.add_open_invite(id);
    w.metadata.grant(id, w.project, PlatformRole::Viewer, &[]);
    w.metadata.expire_invites(id);
    let path = format!("/v1/databases/{}", w.db);
    let err = error_of(w.app.send_as("dave", "org-1", Method::GET, &path, None).await, 403).await;
    assert_eq!(code_of(&err), "no_access_yet");
    assert_eq!(w.metadata.member(w.tenant, "dave").unwrap().status, MemberStatus::Invited);
}

// R49: an invited member whose invite expired can't read the member list, so `/v1/me` names
// the tenant admins to ask (active ones only); nobody else gets the list there.
#[tokio::test]
async fn me_names_the_tenant_admins_for_an_expired_invite() {
    let w = member_world(Mode::SelfHosted).await;
    let id = w.metadata.add_member(w.tenant, "dave", MemberStatus::Invited, false);
    w.metadata.add_open_invite(id);
    w.metadata.expire_invites(id);
    w.metadata.add_member(w.tenant, "gone_admin", MemberStatus::Removed, true);
    let me: Value = w.app.send_as("dave", "org-1", Method::GET, "/v1/me", None).await.json().await.unwrap();
    assert_eq!(me["member"]["status"], "invited");
    let admins = me["tenant_admins"].as_array().expect("tenant_admins").clone();
    let default_admin = w.metadata.member(w.tenant, DEFAULT_SUB).unwrap();
    assert_eq!(admins, vec![json!({ "name": default_admin.display_name, "email": default_admin.email })]);
    // An active member reads the member list instead: no list here.
    w.metadata.add_granted(w.tenant, "erin", w.project, PlatformRole::Viewer, &[]);
    let me: Value = w.app.send_as("erin", "org-1", Method::GET, "/v1/me", None).await.json().await.unwrap();
    assert_eq!(me["tenant_admins"], json!([]));
}

// I3: `/v1/me` lists, per project, the databases the member's grant is narrowed on, with the
// effective role and roles there; a database without an override is not listed, a tenant admin
// has none.
#[tokio::test]
async fn me_lists_narrowed_databases() {
    let w = member_world(Mode::SelfHosted).await;
    let m = w.metadata.add_granted(w.tenant, "nat", w.project, PlatformRole::Admin, &["app", "reports"]);
    w.metadata.narrow(m, w.db, OverrideRow { platform_role: Some(PlatformRole::Developer), pg_roles: Some(vec!["app".into()]) });
    let me: Value = w.app.send_as("nat", "org-1", Method::GET, "/v1/me", None).await.json().await.unwrap();
    assert_eq!(
        me["projects"],
        json!([{
            "project_id": w.project,
            "platform_role": "admin",
            // R17: an Admin grant is every role, whatever a row names.
            "roles": "all",
            "role_levels": {},
            "databases": [{ "database_id": w.db, "platform_role": "developer", "roles": ["app"], "role_levels": { "app": "developer" } }],
        }]),
        "{me}"
    );
    // A role-only override keeps the grant's role.
    w.metadata.narrow(m, w.db, OverrideRow { platform_role: None, pg_roles: Some(vec!["reports".into()]) });
    let me: Value = w.app.send_as("nat", "org-1", Method::GET, "/v1/me", None).await.json().await.unwrap();
    assert_eq!(me["projects"][0]["databases"], json!([{ "database_id": w.db, "platform_role": "admin", "roles": ["owner", "reports"], "role_levels": { "reports": "admin" } }]));
    // No override: an empty list. A tenant admin: always empty.
    w.metadata.add_granted(w.tenant, "pia", w.project, PlatformRole::Viewer, &[]);
    let me: Value = w.app.send_as("pia", "org-1", Method::GET, "/v1/me", None).await.json().await.unwrap();
    assert_eq!(me["projects"][0]["databases"], json!([]));
    let me: Value = w.app.send_as(DEFAULT_SUB, "org-1", Method::GET, "/v1/me", None).await.json().await.unwrap();
    assert!(me["projects"].as_array().unwrap().iter().all(|p| p["databases"] == json!([]) && p["roles"] == "all"), "{me}");
}

// R39: a person with no row is refused (the "ask your admin for an invite" answer) and nothing
// is written: rows are made only by sign-up and accept-invite (#126).
#[tokio::test]
async fn closed_signup_creates_no_member() {
    let w = member_world(Mode::SelfHosted).await;
    let path = format!("/v1/databases/{}", w.db);
    let before = (w.metadata.members.lock().unwrap().len(), w.metadata.events.lock().unwrap().len());
    let err = error_of(w.app.send_as("hal", "org-1", Method::GET, &path, None).await, 403).await;
    assert_eq!(code_of(&err), "no_access_yet");
    let err = error_of(w.app.send_as("hal", "org-1", Method::GET, "/v1/members", None).await, 403).await;
    assert_eq!(code_of(&err), "no_access_yet");
    assert!(w.metadata.member(w.tenant, "hal").is_none(), "no member row");
    assert_eq!((w.metadata.members.lock().unwrap().len(), w.metadata.events.lock().unwrap().len()), before);
    // #119: `/v1/me` tells a member token with no row nothing of the tenant.
    let err = error_of(w.app.send_as("hal", "org-1", Method::GET, "/v1/me", None).await, 403).await;
    assert_eq!(code_of(&err), "no_access_yet");
}

// R40: a pending member is 403 `pending_approval` on every route except /v1/me.
#[tokio::test]
async fn pending_member_is_403_pending_approval() {
    let t = mw(Mode::SelfHosted).await;
    let pat = t.w.metadata.add_member(t.w.tenant, "pat", MemberStatus::Pending, false);
    let project = t.w.project;
    let db = t.w.db;
    for (method, path, body) in [
        (Method::GET, "/v1/members".to_string(), None),
        (Method::GET, "/v1/projects".to_string(), None),
        (Method::GET, format!("/v1/databases/{db}"), None),
        (Method::GET, format!("/v1/projects/{project}"), None),
        (Method::GET, "/v1/invites/delivery".to_string(), None),
        (Method::PATCH, "/v1/tenant".to_string(), Some(json!({ "display_name": "x" }))),
        (Method::POST, "/v1/invites".to_string(), Some(json!({ "email": "a@x.com" }))),
        (Method::POST, format!("/v1/members/{pat}/approve"), Some(json!({ "grants": [] }))),
        (Method::POST, format!("/v1/members/{pat}/decline"), None),
    ] {
        let err = error_of(t.send("pat", method.clone(), &path, body).await, 403).await;
        assert_eq!(code_of(&err), "pending_approval", "{method} {path}");
    }
    let me = body_of(t.send("pat", Method::GET, "/v1/me", None).await).await;
    assert_eq!(me["member"], json!({ "id": pat, "status": "pending", "tenant_admin": false }));
    assert_eq!(me["projects"], json!([]));
    assert_eq!(t.w.metadata.member(t.w.tenant, "pat").unwrap().status, MemberStatus::Pending);
    assert!(t.actions().is_empty());
}

// P14 (members hub T3): a suspended member is 403 `member_suspended` on every route except
// GET /v1/me, which says `suspended` and lists no project; signing in keeps them suspended
// (S6), grants and all.
#[tokio::test]
async fn a_suspended_caller_gets_member_suspended_and_me_says_so() {
    let t = mw(Mode::SelfHosted).await;
    let sam = t.w.metadata.add_member(t.w.tenant, "sam", MemberStatus::Suspended, false);
    t.w.metadata.grant(sam, t.w.project, PlatformRole::Admin, &[]);
    let project = t.w.project;
    let db = t.w.db;
    for (method, path, body) in [
        (Method::GET, "/v1/members".to_string(), None),
        (Method::GET, "/v1/projects".to_string(), None),
        (Method::GET, format!("/v1/databases/{db}"), None),
        (Method::GET, format!("/v1/projects/{project}"), None),
        (Method::PATCH, "/v1/tenant".to_string(), Some(json!({ "display_name": "x" }))),
        (Method::POST, "/v1/invites".to_string(), Some(json!({ "email": "a@x.com" }))),
        (Method::GET, "/v1/access-events".to_string(), None),
    ] {
        let err = error_of(t.send("sam", method.clone(), &path, body).await, 403).await;
        assert_eq!(code_of(&err), "member_suspended", "{method} {path}");
    }
    let me = body_of(t.send("sam", Method::GET, "/v1/me", None).await).await;
    assert_eq!(me["member"], json!({ "id": sam, "status": "suspended", "tenant_admin": false }));
    assert_eq!(me["projects"], json!([]));
    assert_eq!(t.w.metadata.member(t.w.tenant, "sam").unwrap().status, MemberStatus::Suspended);
    assert!(t.actions().is_empty());
}

// R40: approval stores the first grants and activates the member in one step, then reconciles.
#[tokio::test]
async fn approve_grants_and_activates() {
    let t = mw(Mode::SelfHosted).await;
    let pat = t.w.metadata.add_member(t.w.tenant, "pat", MemberStatus::Pending, false);
    let body = invite_body(&t.w, "ignored@x.com", "developer", &["app"]);
    let approve = |sub: &'static str, id: uuid::Uuid, body: Value| {
        let t = &t;
        async move { t.send(sub, Method::POST, &format!("/v1/members/{id}/approve"), Some(body)).await }
    };
    // Only a tenant admin approves.
    t.w.metadata.add_granted(t.w.tenant, "padmin", t.w.project, PlatformRole::Admin, &[]);
    let err = error_of(approve("padmin", pat, json!({ "grants": body["grants"] })).await, 403).await;
    assert_eq!((code_of(&err), err["error"]["required"].as_str()), ("forbidden", Some("tenant admin")));
    assert_eq!(t.w.metadata.member(t.w.tenant, "pat").unwrap().status, MemberStatus::Pending);

    let resp = approve(DEFAULT_SUB, pat, json!({ "grants": body["grants"] })).await;
    assert_eq!(resp.status(), 200);
    let member = body_of(resp).await;
    assert_eq!((member["status"].as_str(), member["has_access"].as_bool()), (Some("active"), Some(true)));
    assert_eq!(t.grant_of(pat), Some(GrantRow { platform_role: PlatformRole::Developer, pg_roles: vec!["app".into()] }));
    assert_eq!(t.w.metadata.member(t.w.tenant, "pat").unwrap().status, MemberStatus::Active);
    assert_eq!(t.actions(), vec!["member_approved"]);
    assert_eq!(t.enqueued(), t.branches(), "the project's branches reconcile");
    // The member can use what was granted.
    let path = format!("/v1/databases/{}", t.w.db);
    assert_eq!(t.send("pat", Method::GET, &path, None).await.status(), 200);
    // Approving again, or a member who is not pending, is a conflict; a stranger's id is 404.
    let err = error_of(approve(DEFAULT_SUB, pat, json!({ "grants": [] })).await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    let err = error_of(approve(DEFAULT_SUB, uuid::Uuid::new_v4(), json!({ "grants": [] })).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    // No grants is fine: active, no access yet.
    let bob = t.w.metadata.add_member(t.w.tenant, "bob", MemberStatus::Pending, false);
    let member = body_of(approve(DEFAULT_SUB, bob, json!({})).await).await;
    assert_eq!((member["status"].as_str(), member["has_access"].as_bool()), (Some("active"), Some(false)));
    assert_eq!(t.actions(), vec!["member_approved", "member_approved"]);
}

// I-A: a pending member has no grants or overrides until it is approved, so every write of
// them is refused, and the refusal changes nothing.
#[tokio::test]
async fn grant_and_override_writes_on_a_pending_member_are_refused() {
    let t = mw(Mode::SelfHosted).await;
    let pat = t.w.metadata.add_member(t.w.tenant, "pat", MemberStatus::Pending, false);
    // A leftover grant, so the override routes get as far as the store.
    t.w.metadata.grant(pat, t.w.project, PlatformRole::Developer, &[]);
    let before = t.grant_of(pat);
    let grant = format!("/v1/projects/{}/grants/{pat}", t.w.project);
    let calls = [
        (Method::PUT, grant.clone(), Some(json!({ "platform_role": "viewer", "pg_roles": [] }))),
        (Method::DELETE, grant.clone(), None),
        (Method::PUT, format!("{grant}/overrides"), Some(json!({ "database_id": t.w.db, "platform_role": "viewer" }))),
        (Method::DELETE, format!("{grant}/overrides?database_id={}", t.w.db), None),
    ];
    for (method, path, body) in calls {
        let resp = t.send(DEFAULT_SUB, method.clone(), &path, body).await;
        assert_eq!(resp.status(), 409, "{method} {path}");
        let err: Value = resp.json().await.unwrap();
        assert_eq!(code_of(&err), "conflict", "{method} {path}");
        assert!(err["error"]["message"].as_str().unwrap().contains("approve it first"), "{err}");
    }
    assert_eq!(t.grant_of(pat), before, "nothing changed");
    assert!(t.actions().is_empty());
    // An invite for its email is refused too: approve it instead (its own code, naming the member).
    let err = error_of(
        t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "pat@example.test", "viewer", &[]))).await,
        409,
    )
    .await;
    assert_eq!(code_of(&err), "signup_pending");
    assert_eq!(err["error"]["member_id"], json!(pat.to_string()), "{err}");
    assert_eq!(t.grant_of(pat), before);
}

// I-A: approval checks the grants against the member's existing overrides, as any grant is
// checked: an Admin grant holding a role that is only safe for Admins is refused when an
// override narrows the member below Admin on a database.
#[tokio::test]
async fn approve_checks_grants_against_overrides() {
    let t = mw(Mode::SelfHosted).await;
    let ep = t.main_endpoint();
    t.w.metadata.add_group_role(&ep, "billing", &[]);
    t.w.metadata.add_role(&ep, "svc", RoleOrigin::Sql, false, false); // a LOGIN role
    t.w.metadata.add_group_role(&ep, "via_login", &["billing", "svc"]);
    let pat = t.w.metadata.add_member(t.w.tenant, "pat", MemberStatus::Pending, false);
    t.w.metadata.overrides.lock().unwrap().insert(
        (pat, t.w.db.as_uuid()),
        // R17: an Admin grant keeps the roles an override names.
        chelabase_metadata::members::OverrideRow { platform_role: Some(PlatformRole::Developer), pg_roles: Some(vec!["via_login".into()]) },
    );
    let path = format!("/v1/members/{pat}/approve");
    let body = json!({ "grants": [{ "project_id": t.w.project, "platform_role": "admin", "pg_roles": ["via_login"] }] });
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, &path, Some(body.clone())).await, 422).await;
    assert_eq!(err["error"]["role"], "via_login");
    assert_eq!(t.w.metadata.member(t.w.tenant, "pat").unwrap().status, MemberStatus::Pending);
    assert_eq!(t.grant_of(pat), None);
    // Without the override the same grant is fine (Admin may hold it).
    t.w.metadata.overrides.lock().unwrap().clear();
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &path, Some(body)).await.status(), 200);
}

// M4: a member who went away between the check and the write is "member not found".
#[tokio::test]
async fn approve_of_a_member_who_went_away_is_member_not_found() {
    let t = mw(Mode::SelfHosted).await;
    let pat = t.w.metadata.add_member(t.w.tenant, "pat", MemberStatus::Pending, false);
    *t.w.metadata.vanish_on_approve.lock().unwrap() = Some(pat);
    let err = error_of(
        t.send(DEFAULT_SUB, Method::POST, &format!("/v1/members/{pat}/approve"), Some(json!({ "grants": [] }))).await,
        404,
    )
    .await;
    assert_eq!((code_of(&err), err["error"]["message"].as_str()), ("not_found", Some("member not found")));
    assert!(t.actions().is_empty());
}

// R40: the grants are checked as an invite's are; a refused approval changes nothing.
#[tokio::test]
async fn approve_checks_grants() {
    let t = mw(Mode::SelfHosted).await;
    let pat = t.w.metadata.add_member(t.w.tenant, "pat", MemberStatus::Pending, false);
    let path = format!("/v1/members/{pat}/approve");
    let grant = |role: &str, roles: &[&str]| {
        json!({ "grants": [{ "project_id": t.w.project, "platform_role": role, "pg_roles": roles }] })
    };
    for roles in [&["owner"][..], &["cb_m_0123"], &["chelabase_x"], &["pg_read_all_data"], &["postgres"]] {
        let err = error_of(t.send(DEFAULT_SUB, Method::POST, &path, Some(grant("developer", roles))).await, 422).await;
        assert_eq!(code_of(&err), "invalid_request", "{roles:?}");
    }
    // A project twice, and a project of nobody's.
    let twice = json!({ "grants": [
        { "project_id": t.w.project, "platform_role": "viewer", "pg_roles": [] },
        { "project_id": t.w.project, "platform_role": "developer", "pg_roles": [] },
    ] });
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, &path, Some(twice)).await, 422).await;
    assert_eq!(code_of(&err), "invalid_request");
    let nowhere = json!({ "grants": [{ "project_id": uuid::Uuid::new_v4(), "platform_role": "viewer", "pg_roles": [] }] });
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &path, Some(nowhere)).await.status(), 404);
    assert_eq!(t.w.metadata.member(t.w.tenant, "pat").unwrap().status, MemberStatus::Pending);
    assert_eq!(t.grant_of(pat), None);
    assert!(t.actions().is_empty());
    assert!(t.enqueued().is_empty());
    // A valid one still works afterwards.
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &path, Some(grant("viewer", &[]))).await.status(), 200);
}

// R40: declining removes the pending member and bans its account, as removal does.
#[tokio::test]
async fn decline_removes_and_bans() {
    let t = mw(Mode::SelfHosted).await;
    let zid = t.identity.with_invited_user(&Pool::Members, "pat@example.test");
    let pat = t.w.metadata.add_member(t.w.tenant, &zid, MemberStatus::Pending, false);
    t.identity.with_session(&Pool::Members, &zid);
    let path = format!("/v1/members/{pat}/decline");
    t.w.metadata.add_granted(t.w.tenant, "padmin", t.w.project, PlatformRole::Admin, &[]);
    let err = error_of(t.send("padmin", Method::POST, &path, None).await, 403).await;
    assert_eq!(err["error"]["required"], "tenant admin");
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &path, None).await.status(), 204);
    assert_eq!(t.w.metadata.member(t.w.tenant, &zid).unwrap().status, MemberStatus::Removed);
    assert!(t.identity.user(&Pool::Members, &zid).unwrap().banned, "the account is banned");
    assert_eq!(t.identity.session_count(&Pool::Members), 0, "its sessions are ended");
    assert_eq!(t.actions(), vec!["member_declined"]);
    // A repeat retries the identity steps and writes nothing more.
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &path, None).await.status(), 204);
    assert_eq!(t.actions(), vec!["member_declined"]);
    // An active member isn't declined; a stranger's id is 404.
    let active = t.w.metadata.add_granted(t.w.tenant, "act", t.w.project, PlatformRole::Viewer, &[]);
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, &format!("/v1/members/{active}/decline"), None).await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, &format!("/v1/members/{}/decline", uuid::Uuid::new_v4()), None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
}

#[tokio::test]
async fn platform_mode_creates_no_members() {
    let w = member_world(Mode::Platform).await;
    let before = w.metadata.members.lock().unwrap().len();
    let path = format!("/v1/databases/{}", w.db);
    let err = error_of(w.app.send_as("dave", "org-1", Method::GET, &path, None).await, 403).await;
    assert_eq!(code_of(&err), "not_available_in_platform_mode");
    let err = error_of(w.app.send_as("dave", "org-1", Method::GET, "/v1/me", None).await, 403).await;
    assert_eq!(code_of(&err), "not_available_in_platform_mode");
    assert_eq!(w.metadata.members.lock().unwrap().len(), before);
    assert!(w.metadata.member(w.tenant, "dave").is_none());

    // An existing member is served; a tenant admin isn't an instance admin without the
    // operator tenant.
    assert_eq!(w.app.send_as(DEFAULT_SUB, "org-1", Method::GET, &path, None).await.status(), 200);
    let me: Value = w.app.send_as(DEFAULT_SUB, "org-1", Method::GET, "/v1/me", None).await.json().await.unwrap();
    assert_eq!(me["instance_admin"], json!(false));
}

#[tokio::test]
async fn me_lists_capabilities() {
    let w = member_world(Mode::SelfHosted).await;
    let other = w.metadata.create_project(w.tenant, "Other").await.unwrap().id;
    let overview = |id: ProjectId, name: &str| ProjectOverview {
        id,
        name: name.into(),
        slug: name.to_lowercase(),
        status: ProjectStatus::Active,
        databases: vec![],
    };
    w.metadata.set_overview(TenantOverview {
        id: w.tenant,
        slug: "demo".into(),
        display_name: None,
        projects: vec![overview(w.project, "Billing"), overview(other, "Other")],
    });
    w.metadata.add_granted(w.tenant, "erin", w.project, PlatformRole::Developer, &["sales", "billing"]);

    let me: Value = w.app.send_as("erin", "org-1", Method::GET, "/v1/me", None).await.json().await.unwrap();
    assert_eq!(me["sub"], "erin");
    assert_eq!(me["email"], "erin@example.test");
    assert_eq!(me["name"], "erin");
    assert_eq!(me["tenant_id"], json!(w.tenant));
    let erin = w.metadata.member(w.tenant, "erin").unwrap().id;
    assert_eq!(me["member"], json!({ "id": erin, "status": "active", "tenant_admin": false }));
    assert_eq!(me["instance_admin"], json!(false));
    assert_eq!(
        me["projects"],
        json!([{ "project_id": w.project, "platform_role": "developer", "roles": ["billing", "sales"], "role_levels": { "billing": "developer", "sales": "developer" }, "databases": [] }])
    );
    // The tenant overview shows only the projects the member is granted.
    let names: Vec<&str> = me["tenant"]["projects"].as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Billing"]);

    // R17: an Admin grant is every role (the database owner included), whatever a row names.
    w.metadata.add_granted(w.tenant, "fay", other, PlatformRole::Admin, &["billing"]);
    let me: Value = w.app.send_as("fay", "org-1", Method::GET, "/v1/me", None).await.json().await.unwrap();
    assert_eq!(me["projects"][0]["roles"], json!("all"));

    // The tenant admin: every project, every role, and (self-hosted) the instance.
    let me: Value = w.app.send_as(DEFAULT_SUB, "org-1", Method::GET, "/v1/me", None).await.json().await.unwrap();
    assert_eq!((&me["member"]["status"], &me["member"]["tenant_admin"]), (&json!("active"), &json!(true)));
    assert!(me["member"]["id"].as_str().is_some_and(|id| id.len() == 36), "{me}");
    assert_eq!(me["instance_admin"], json!(true));
    let mut expected = vec![
        json!({ "project_id": w.project, "platform_role": "admin", "roles": "all", "role_levels": {}, "databases": [] }),
        json!({ "project_id": other, "platform_role": "admin", "roles": "all", "role_levels": {}, "databases": [] }),
    ];
    let mut got = me["projects"].as_array().unwrap().clone();
    let key = |v: &Value| v["project_id"].as_str().unwrap().to_string();
    expected.sort_by_key(key);
    got.sort_by_key(key);
    assert_eq!(got, expected);
    assert_eq!(me["tenant"]["projects"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn tenant_admin_is_admin_on_every_project() {
    let w = member_world(Mode::SelfHosted).await;
    // The tenant admin holds no grant rows, yet passes Admin-only routes on the project.
    let path = format!("/v1/databases/{}/connection", w.db);
    assert_eq!(w.app.send_as(DEFAULT_SUB, "org-1", Method::GET, &path, None).await.status(), 200);
    let renamed = w
        .app
        .send_as(DEFAULT_SUB, "org-1", Method::PATCH, &format!("/v1/projects/{}", w.project), Some(json!({"name": "B2"})))
        .await;
    assert_eq!(renamed.status(), 200);

    // A tenant admin's own override rows don't narrow them.
    let admin = w.metadata.member(w.tenant, DEFAULT_SUB).unwrap().id;
    w.metadata.narrow(admin, w.db, OverrideRow { platform_role: Some(PlatformRole::Viewer), pg_roles: None });
    assert_eq!(w.app.send_as(DEFAULT_SUB, "org-1", Method::GET, &path, None).await.status(), 200);

    // A Developer grant is too low for the same route.
    w.metadata.add_granted(w.tenant, "gus", w.project, PlatformRole::Developer, &[]);
    let err = error_of(w.app.send_as("gus", "org-1", Method::GET, &path, None).await, 403).await;
    assert_eq!(code_of(&err), "forbidden");
    assert_eq!(err["error"]["required"], "admin");
}

#[tokio::test]
async fn overrides_narrow_the_project_grant() {
    let w = member_world(Mode::SelfHosted).await;
    let ivy = w.metadata.add_granted(w.tenant, "ivy", w.project, PlatformRole::Admin, &["app"]);
    let connection = format!("/v1/databases/{}/connection", w.db);
    assert_eq!(w.app.send_as("ivy", "org-1", Method::GET, &connection, None).await.status(), 200);

    // Down to Viewer on this database: reads pass, the owner's credentials don't.
    w.metadata.narrow(ivy, w.db, OverrideRow { platform_role: Some(PlatformRole::Viewer), pg_roles: None });
    let err = error_of(w.app.send_as("ivy", "org-1", Method::GET, &connection, None).await, 403).await;
    assert_eq!(err["error"]["required"], "admin");
    let get = format!("/v1/databases/{}", w.db);
    assert_eq!(w.app.send_as("ivy", "org-1", Method::GET, &get, None).await.status(), 200);

    // A roles-only override keeps Admin but takes `app` away, in every Postgres database of
    // the Chelabase database (no Postgres-database level, R17).
    let console = format!("/v1/databases/{}/branches/{}/console/query", w.db, w.dev.as_uuid());
    let body = |database: &str| json!({ "role": "app", "database": database, "tree": { "v": 1, "op": "select", "from": "notes" } });
    w.metadata.narrow(ivy, w.db, OverrideRow { platform_role: None, pg_roles: Some(vec!["other".into()]) });
    assert_eq!(w.app.send_as("ivy", "org-1", Method::GET, &connection, None).await.status(), 200);
    for database in ["web1", "web2"] {
        let err = error_of(w.app.send_as("ivy", "org-1", Method::POST, &console, Some(body(database))).await, 403).await;
        assert_eq!(err["error"]["required"], "role:app", "{database}");
    }
}

// --- T5 review fixes (R16-R19) ---------------------------------------------------------------

/// Whether a response body is an access refusal (`forbidden`, `no_access_yet` or
/// `not_available_in_platform_mode`).
fn access_refusal(body: &Value) -> bool {
    matches!(
        body["error"]["code"].as_str(),
        Some("forbidden" | "no_access_yet" | "not_available_in_platform_mode")
    )
}

async fn body_of(resp: reqwest::Response) -> Value {
    resp.json().await.unwrap_or(Value::Null)
}

fn protect(metadata: &FakeMetadata, branch: BranchId) {
    metadata.branches.lock().unwrap().get_mut(&branch).unwrap().protected = true;
}

// R16: a role that is owner-level, has BYPASSRLS, or reaches the owner or an owner-level role
// through its memberships is Admin-only to act as, even when the grant lists it.
#[tokio::test]
async fn developer_cannot_act_as_owner_level_role() {
    let w = member_world(Mode::SelfHosted).await;
    let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
    w.metadata.add_group_role(&ep, "app", &[]);
    w.metadata.add_role(&ep, "power", RoleOrigin::Sql, true, false);
    let mut bypass = role_row("bypass", RoleOrigin::Sql, RoleLevel::Plain, PasswordState::Generated);
    bypass.attributes.bypass_rls = true;
    let mut reacher = role_row("reacher", RoleOrigin::Sql, RoleLevel::Plain, PasswordState::Generated);
    reacher.attributes.member_of = vec!["middle".into()];
    let mut middle = role_row("middle", RoleOrigin::Sql, RoleLevel::Plain, PasswordState::Generated);
    middle.attributes.member_of = vec!["power".into()];
    let mut ownerish = role_row("ownerish", RoleOrigin::Sql, RoleLevel::Plain, PasswordState::Generated);
    ownerish.attributes.member_of = vec!["postgres".into()];
    let mut looped = role_row("looped", RoleOrigin::Sql, RoleLevel::Plain, PasswordState::Generated);
    looped.attributes.member_of = vec!["looped2".into()];
    let mut looped2 = role_row("looped2", RoleOrigin::Sql, RoleLevel::Plain, PasswordState::Generated);
    looped2.attributes.member_of = vec!["looped".into()];
    looped.login = false;
    looped2.login = false;
    for row in [bypass, reacher, middle, ownerish, looped, looped2] {
        w.metadata.add_role_row(&ep, row);
    }
    let all = ["app", "power", "bypass", "reacher", "middle", "ownerish", "looped"];
    w.metadata.add_granted(w.tenant, "dan", w.project, PlatformRole::Developer, &all);
    w.metadata.add_granted(w.tenant, "ada", w.project, PlatformRole::Admin, &all);
    let console = format!("/v1/databases/{}/branches/{}/console/query", w.db, w.dev.as_uuid());
    let sql = format!("/v1/databases/{}/branches/{}/sql", w.db, w.dev.as_uuid());
    let query = |role: &str| json!({ "role": role, "tree": { "v": 1, "op": "select", "from": "notes" } });
    let run = |role: &str| json!({ "run_id": uuid::Uuid::new_v4(), "role": role, "sql": "select 1" });
    for role in ["power", "bypass", "reacher", "middle", "ownerish"] {
        let err = error_of(w.app.send_as("dan", "org-1", Method::POST, &console, Some(query(role))).await, 403).await;
        assert_eq!(err["error"]["required"], "admin", "{role}");
        let err = error_of(w.app.send_as("dan", "org-1", Method::POST, &sql, Some(run(role))).await, 403).await;
        assert_eq!(err["error"]["required"], "admin", "{role}");
        let conn = format!("/v1/databases/{}/branches/{}/connection?role={role}", w.db, w.dev.as_uuid());
        let err = error_of(w.app.send_as("dan", "org-1", Method::GET, &conn, None).await, 403).await;
        assert_eq!(err["error"]["required"], "admin", "{role}");
        // An Admin may act as it.
        let ok = body_of(w.app.send_as("ada", "org-1", Method::POST, &console, Some(query(role))).await).await;
        assert!(!access_refusal(&ok), "{role}: {ok}");
    }
    // A plain role, and a membership loop that reaches nothing owner-level, are fine.
    for role in ["app", "looped"] {
        let ok = body_of(w.app.send_as("dan", "org-1", Method::POST, &console, Some(query(role))).await).await;
        assert!(!access_refusal(&ok), "{role}: {ok}");
    }
}

// I1: a route whose body names a Chelabase database reads that database's override.
#[tokio::test]
async fn put_app_users_reads_the_database_override() {
    let w = member_world(Mode::SelfHosted).await;
    let pat = w.metadata.add_granted(w.tenant, "pat", w.project, PlatformRole::Admin, &[]);
    let path = format!("/v1/projects/{}/app-users", w.project);
    let body = json!({ "database_id": w.db });
    let before = body_of(w.app.send_as("pat", "org-1", Method::PUT, &path, Some(body.clone())).await).await;
    assert!(!access_refusal(&before), "{before}");
    w.metadata.narrow(pat, w.db, OverrideRow { platform_role: Some(PlatformRole::Viewer), pg_roles: None });
    let err = error_of(w.app.send_as("pat", "org-1", Method::PUT, &path, Some(body)).await, 403).await;
    assert_eq!(err["error"]["required"], "admin");
}

// I3: `create_merge` goes by the branch merged into.
#[tokio::test]
async fn create_merge_into_protected_parent_needs_admin() {
    let w = member_world(Mode::SelfHosted).await;
    w.metadata.add_granted(w.tenant, "dev1", w.project, PlatformRole::Developer, &[]);
    let main = BranchId::from(w.db);
    protect(&w.metadata, main);
    let path = format!("/v1/databases/{}/branches/{}/merges", w.db, w.dev.as_uuid());
    let body = |direction: &str| json!({ "direction": direction, "database": "app" });
    let err = error_of(w.app.send_as("dev1", "org-1", Method::POST, &path, Some(body("into_parent"))).await, 403).await;
    assert_eq!(err["error"]["required"], "admin");
    let ok = body_of(w.app.send_as("dev1", "org-1", Method::POST, &path, Some(body("from_parent"))).await).await;
    assert!(!access_refusal(&ok), "{ok}");
    // From the parent into a protected dev: the other way round.
    w.metadata.branches.lock().unwrap().get_mut(&main).unwrap().protected = false;
    protect(&w.metadata, w.dev);
    let ok = body_of(w.app.send_as("dev1", "org-1", Method::POST, &path, Some(body("into_parent"))).await).await;
    assert!(!access_refusal(&ok), "{ok}");
    let err = error_of(w.app.send_as("dev1", "org-1", Method::POST, &path, Some(body("from_parent"))).await, 403).await;
    assert_eq!(err["error"]["required"], "admin");
}

// I3: merge actions go by the merge's target branch, not its source.
#[tokio::test]
async fn merge_actions_follow_the_target_branch() {
    let w = member_world(Mode::SelfHosted).await;
    w.metadata.add_granted(w.tenant, "dev1", w.project, PlatformRole::Developer, &[]);
    let main = BranchId::from(w.db);
    protect(&w.metadata, main);
    let into_main = w.merge;
    let from_main = merge_row(w.db, main, w.dev, MergeStatus::Draft);
    let from_main_id = from_main.id;
    w.metadata.merges.lock().unwrap().push(from_main);
    for action in ["rehearse", "apply", "cancel", "recompute", "abandon"] {
        let into = format!("/v1/databases/{}/merges/{into_main}/{action}", w.db);
        let err = error_of(w.app.send_as("dev1", "org-1", Method::POST, &into, Some(json!({}))).await, 403).await;
        assert_eq!(err["error"]["required"], "admin", "{action}");
        let from = format!("/v1/databases/{}/merges/{from_main_id}/{action}", w.db);
        let ok = body_of(w.app.send_as("dev1", "org-1", Method::POST, &from, Some(json!({}))).await).await;
        assert!(!access_refusal(&ok), "{action}: {ok}");
    }
}

// M3: an admission answer doesn't depend on whether the target exists.
#[tokio::test]
async fn admission_answers_dont_reveal_existence() {
    let w = member_world(Mode::SelfHosted).await;
    let paths = [
        format!("/v1/databases/{}", w.db),
        format!("/v1/databases/{}", uuid::Uuid::new_v4()),
        format!("/v1/projects/{}", w.project),
        format!("/v1/projects/{}", uuid::Uuid::new_v4()),
    ];
    for path in &paths {
        let err = error_of(w.app.send_as("nobody", "org-1", Method::GET, path, None).await, 403).await;
        assert_eq!(code_of(&err), "no_access_yet", "{path}");
    }
    let p = member_world(Mode::Platform).await;
    let paths = [format!("/v1/databases/{}", p.db), format!("/v1/databases/{}", uuid::Uuid::new_v4())];
    for path in &paths {
        let err = error_of(p.app.send_as("dave", "org-1", Method::GET, path, None).await, 403).await;
        assert_eq!(code_of(&err), "not_available_in_platform_mode", "{path}");
    }
}

// M4: one read of the member row per request, in both modes, with or without claims.
#[tokio::test]
async fn the_member_row_is_read_once_per_request() {
    for mode in [Mode::SelfHosted, Mode::Platform] {
        let w = member_world(mode).await;
        let path = format!("/v1/databases/{}", w.db);
        let bare = Identity { email: None, name: None, ..identity_of(DEFAULT_SUB) };
        let full = identity_of(DEFAULT_SUB);
        for (label, identity) in [("claims", &full), ("no claims", &bare)] {
            // The first request may refresh the claims; count the steady state.
            w.app.send_identity(identity, "org-1", Method::GET, &path).await;
            let before = w.metadata.member_reads.load(Ordering::SeqCst);
            assert_eq!(w.app.send_identity(identity, "org-1", Method::GET, &path).await.status(), 200);
            assert_eq!(w.metadata.member_reads.load(Ordering::SeqCst) - before, 1, "{mode:?} {label}");
        }
    }
}

// M6: an unknown name is null, like an unknown email.
#[tokio::test]
async fn me_name_is_null_when_unknown() {
    let w = member_world(Mode::SelfHosted).await;
    w.metadata.set_member(w.tenant, DEFAULT_SUB, |m| {
        m.display_name = String::new();
        m.email = None;
    });
    let bare = Identity { email: None, name: None, ..identity_of(DEFAULT_SUB) };
    let me = body_of(w.app.send_identity(&bare, "org-1", Method::GET, "/v1/me").await).await;
    assert!(me["name"].is_null(), "{me}");
    assert!(me["email"].is_null(), "{me}");
}

// --- Route-specific refinements (members and roles, T6) --------------------------------------

fn select_tree() -> Value {
    json!({ "v": 1, "op": "select", "from": "notes" })
}

fn write_tree() -> Value {
    json!({ "v": 1, "op": "insert", "into": "notes", "values": [{ "body": "x" }] })
}

fn console_path(w: &MemberWorld) -> String {
    format!("/v1/databases/{}/branches/{}/console/query", w.db, w.dev.as_uuid())
}

// Console: a Viewer reads (select), and nothing that writes reaches rest.
#[tokio::test]
async fn viewer_console_select_ok_write_forbidden() {
    let w = member_world(Mode::SelfHosted).await;
    let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
    w.metadata.add_group_role(&ep, "app", &[]);
    w.metadata.add_granted(w.tenant, "vic", w.project, PlatformRole::Viewer, &["app"]);
    w.metadata.add_granted(w.tenant, "dev1", w.project, PlatformRole::Developer, &["app"]);
    let path = console_path(&w);
    let body = |tree: Value| json!({ "role": "app", "tree": tree });

    let read = body_of(w.app.send_as("vic", "org-1", Method::POST, &path, Some(body(select_tree()))).await).await;
    assert!(!access_refusal(&read), "{read}");
    for tree in [write_tree(), json!({ "v": 1, "op": "delete", "from": "notes", "where": { "col": "id", "eq": 1 } })] {
        let err = error_of(w.app.send_as("vic", "org-1", Method::POST, &path, Some(body(tree))).await, 403).await;
        assert_eq!((code_of(&err), err["error"]["required"].as_str()), ("forbidden", Some("developer")));
    }
    // A Developer's write is not refused.
    let ok = body_of(w.app.send_as("dev1", "org-1", Method::POST, &path, Some(body(write_tree()))).await).await;
    assert!(!access_refusal(&ok), "{ok}");
}

#[tokio::test]
async fn viewer_batch_with_write_forbidden() {
    let w = member_world(Mode::SelfHosted).await;
    let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
    w.metadata.add_group_role(&ep, "app", &[]);
    w.metadata.add_granted(w.tenant, "vic", w.project, PlatformRole::Viewer, &["app"]);
    let path = console_path(&w);
    let batch = |ops: Vec<Value>| json!({ "role": "app", "tree": { "v": 1, "op": "batch", "ops": ops } });

    let reads = batch(vec![select_tree(), select_tree()]);
    let ok = body_of(w.app.send_as("vic", "org-1", Method::POST, &path, Some(reads)).await).await;
    assert!(!access_refusal(&ok), "{ok}");
    // One write anywhere in the batch refuses all of it.
    let mixed = batch(vec![select_tree(), json!({ "op": "delete", "from": "notes", "where": { "col": "id", "eq": 1 } })]);
    let err = error_of(w.app.send_as("vic", "org-1", Method::POST, &path, Some(mixed)).await, 403).await;
    assert_eq!(err["error"]["required"], "developer");
}

impl Branching {
    /// An active member with `role` on the database's project.
    fn grant_member(&self, sub: &str, role: PlatformRole, pg_roles: &[&str]) -> uuid::Uuid {
        let record = self.metadata.database(self.db);
        self.metadata.add_granted(record.tenant_id, sub, record.project_id, role, pg_roles)
    }
}

// SQL editor cancel: the run's starter or an Admin; another Developer is refused.
#[tokio::test]
async fn cancel_by_other_developer_forbidden() {
    let t = sql_app().await;
    let main = t.main();
    let a = t.grant_member("dev-a", PlatformRole::Developer, &[]);
    t.grant_member("dev-b", PlatformRole::Developer, &[]);
    t.grant_member("adm", PlatformRole::Admin, &[]);
    let id = uuid::Uuid::new_v4();
    let path = format!("{}/sql/runs/{id}/cancel", t.branch(main));
    let runs = t.app.state.sql_editor.runs();
    let held = runs.reserve("dev-a", id, Some(a), main, "web1").unwrap();

    let err = error_of(t.send_as("dev-b", Method::POST, &path, None).await, 403).await;
    assert_eq!((code_of(&err), err["error"]["required"].as_str()), ("forbidden", Some("admin")));
    assert!(!held.token().is_cancelled());
    assert_eq!(t.send_as("dev-a", Method::POST, &path, None).await.status(), 202);
    assert!(held.token().is_cancelled());
    drop(held);

    // A project Admin and a tenant admin may cancel anyone's.
    for sub in ["adm", DEFAULT_SUB] {
        let held = runs.reserve("dev-a", id, Some(a), main, "web1").unwrap();
        assert_eq!(t.send_as(sub, Method::POST, &path, None).await.status(), 202, "{sub}");
        assert!(held.token().is_cancelled(), "{sub}");
    }
    // No such run is still a 404.
    let err = error_of(t.send_as("dev-b", Method::POST, &path, None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
}

// Saved queries: the author or an Admin edits and deletes; a query saved before members has
// no author, so only an Admin does.
#[tokio::test]
async fn saved_query_edit_by_author_or_admin() {
    let t = branching().await;
    t.grant_member("dev-a", PlatformRole::Developer, &[]);
    t.grant_member("dev-b", PlatformRole::Developer, &[]);
    t.grant_member("adm", PlatformRole::Admin, &[]);
    let created = t.send_as("dev-a", Method::POST, &t.queries(), Some(json!({ "name": "mine", "sql": "select 1" }))).await;
    assert_eq!(created.status(), 201);
    let q: Value = created.json().await.unwrap();
    let one = format!("{}/{}", t.queries(), q["id"].as_str().unwrap());

    // Another Developer sees it, but it isn't theirs.
    assert_eq!(t.send_as("dev-b", Method::GET, &one, None).await.status(), 200);
    for (method, body) in [(Method::PATCH, Some(json!({ "version": 1, "name": "x" }))), (Method::DELETE, None)] {
        let err = error_of(t.send_as("dev-b", method, &one, body).await, 403).await;
        assert_eq!((code_of(&err), err["error"]["required"].as_str()), ("forbidden", Some("admin")));
    }
    // The author, then an Admin (not the author) on the same query.
    let resp = t.send_as("dev-a", Method::PATCH, &one, Some(json!({ "version": 1, "sql": "select 2" }))).await;
    assert_eq!(resp.status(), 200);
    let resp = t.send_as("adm", Method::PATCH, &one, Some(json!({ "version": 2, "name": "renamed" }))).await;
    assert_eq!(resp.status(), 200);
    let upd: Value = resp.json().await.unwrap();
    assert_eq!((upd["name"].as_str(), upd["version"].as_i64()), (Some("renamed"), Some(3)));
    assert_eq!(t.send_as("dev-b", Method::DELETE, &one, None).await.status(), 403);
    assert_eq!(t.send_as("adm", Method::DELETE, &one, None).await.status(), 204);

    // The author deletes their own.
    let q: Value = t.send_as("dev-a", Method::POST, &t.queries(), Some(json!({ "name": "again", "sql": "select 1" }))).await.json().await.unwrap();
    let again = format!("{}/{}", t.queries(), q["id"].as_str().unwrap());
    assert_eq!(t.send_as("dev-a", Method::DELETE, &again, None).await.status(), 204);

    // Saved before members: no author.
    let now = chrono::Utc::now();
    let old = SavedQuery {
        id: uuid::Uuid::new_v4(),
        name: "old".into(),
        sql: "select 0".into(),
        pg_database: None,
        role: None,
        version: 1,
        created_at: now,
        updated_at: now,
        author_member_id: None,
    };
    t.metadata.saved_queries.lock().unwrap().push((t.db.as_uuid(), "dev-a".into(), old.clone()));
    let legacy = format!("{}/{}", t.queries(), old.id);
    assert_eq!(t.send_as("dev-a", Method::PATCH, &legacy, Some(json!({ "version": 1, "name": "x" }))).await.status(), 403);
    assert_eq!(t.send_as("dev-a", Method::DELETE, &legacy, None).await.status(), 403);
    assert_eq!(t.send_as("adm", Method::PATCH, &legacy, Some(json!({ "version": 1, "name": "x" }))).await.status(), 200);
    assert_eq!(t.send_as("adm", Method::DELETE, &legacy, None).await.status(), 204);
}

fn seed_key(w: &MemberWorld, kind: DataKeyKind, role: &str, name: &str) -> uuid::Uuid {
    let generated = data_key::generate(kind);
    let id = uuid::Uuid::new_v4();
    let new = NewDataKey {
        id,
        branch_id: w.dev,
        kind,
        role: role.into(),
        name: name.into(),
        prefix: generated.prefix.clone(),
        sha256: generated.sha256,
        sealed: None,
        databases: None,
        role_oid: None,
        role_timeline: None,
        role_bypass_rls: false,
    };
    insert_key(&mut w.metadata.keys.lock().unwrap(), &new).unwrap();
    id
}

// Data keys: a Developer manages keys bound to roles they may use; an Admin any key; a key
// bound to an owner-level role is Admin-only (R16).
#[tokio::test]
async fn key_for_disallowed_role_forbidden() {
    let w = member_world(Mode::SelfHosted).await;
    let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
    w.metadata.add_group_role(&ep, "reporting", &[]);
    w.metadata.add_group_role(&ep, "other", &[]);
    w.metadata.add_role(&ep, "power", RoleOrigin::Sql, true, false);
    w.metadata.add_granted(w.tenant, "dan", w.project, PlatformRole::Developer, &["reporting", "power"]);
    w.metadata.add_granted(w.tenant, "ada", w.project, PlatformRole::Admin, &[]);
    let keys = format!("/v1/databases/{}/branches/{}/keys", w.db, w.dev.as_uuid());
    let as_dan = |method: Method, path: String, body: Option<Value>| {
        let app = &w.app;
        async move { app.send_as("dan", "org-1", method, &path, body).await }
    };
    let required = |err: &Value| err["error"]["required"].as_str().map(str::to_string);

    // Create.
    assert_eq!(as_dan(Method::POST, keys.clone(), Some(json!({ "name": "ok", "role": "reporting" }))).await.status(), 201);
    let err = error_of(as_dan(Method::POST, keys.clone(), Some(json!({ "name": "no", "role": "other" }))).await, 403).await;
    assert_eq!((code_of(&err), required(&err)), ("forbidden", Some("role:other".into())));
    // Listed but owner-level: Admin only.
    let err = error_of(as_dan(Method::POST, keys.clone(), Some(json!({ "name": "no", "role": "power" }))).await, 403).await;
    assert_eq!(required(&err), Some("admin".into()));
    assert_eq!(w.app.send_as("ada", "org-1", Method::POST, &keys, Some(json!({ "name": "any", "role": "other" }))).await.status(), 201);
    assert_eq!(w.app.send_as("ada", "org-1", Method::POST, &keys, Some(json!({ "name": "pow", "role": "power" }))).await.status(), 201);

    // Rotate, patch and delete, by the key's role.
    let mine = seed_key(&w, DataKeyKind::Secret, "reporting", "mine");
    let theirs = seed_key(&w, DataKeyKind::Secret, "other", "theirs");
    let powerful = seed_key(&w, DataKeyKind::Secret, "power", "powerful");
    let public = seed_key(&w, DataKeyKind::Public, "anon", "public");
    let patch = Some(json!({ "databases": null }));
    for (id, want) in [(theirs, "role:other"), (powerful, "admin"), (public, "role:anon")] {
        let one = format!("{keys}/{id}");
        let rotate = format!("{one}/rotate");
        for (method, path, body) in [(Method::POST, rotate, None), (Method::PATCH, one.clone(), patch.clone()), (Method::DELETE, one, None)] {
            let err = error_of(as_dan(method.clone(), path.clone(), body).await, 403).await;
            assert_eq!(required(&err), Some(want.into()), "{method} {path}");
        }
    }
    let one = format!("{keys}/{mine}");
    assert_eq!(as_dan(Method::PATCH, one.clone(), patch.clone()).await.status(), 200);
    assert_eq!(as_dan(Method::POST, format!("{one}/rotate"), None).await.status(), 201);
    // An Admin manages any key.
    for id in [theirs, powerful, public] {
        let one = format!("{keys}/{id}");
        assert_eq!(w.app.send_as("ada", "org-1", Method::PATCH, &one, patch.clone()).await.status(), 200, "{id}");
    }
    assert_eq!(w.app.send_as("ada", "org-1", Method::DELETE, &format!("{keys}/{theirs}"), None).await.status(), 204);
}

// Merges: a plan with a security step is applied by an Admin, even into an unprotected target.
#[tokio::test]
async fn merge_with_security_steps_needs_admin() {
    let w = member_world(Mode::SelfHosted).await;
    w.metadata.add_granted(w.tenant, "dev1", w.project, PlatformRole::Developer, &[]);
    w.metadata.add_granted(w.tenant, "ada", w.project, PlatformRole::Admin, &[]);
    let set = |plan: Value| {
        let mut rows = w.metadata.merges.lock().unwrap();
        let m = rows.iter_mut().find(|r| r.id == w.merge).unwrap();
        m.status = MergeStatus::Rehearsed;
        m.analysis = Some(json!({"conflicts": []}));
        m.plan = Some(plan);
        m.acknowledgements = json!(["grant"]);
    };
    let apply = format!("/v1/databases/{}/merges/{}/apply", w.db, w.merge);

    set(json!({ "steps": [{ "id": "grant", "security": true }] }));
    let err = error_of(w.app.send_as("dev1", "org-1", Method::POST, &apply, Some(json!({}))).await, 403).await;
    assert_eq!((code_of(&err), err["error"]["required"].as_str()), ("forbidden", Some("admin")));
    assert_eq!(w.app.send_as("ada", "org-1", Method::POST, &apply, Some(json!({}))).await.status(), 202);

    // Without a security step a Developer applies it.
    w.metadata.merges.lock().unwrap().iter_mut().find(|r| r.id == w.merge).unwrap().status = MergeStatus::Draft;
    set(json!({ "steps": [{ "id": "t1", "data_loss": false, "security": false }] }));
    assert_eq!(w.app.send_as("dev1", "org-1", Method::POST, &apply, Some(json!({}))).await.status(), 202);
}

// Projects: the list shows the projects the caller has a grant on; a tenant admin sees all.
#[tokio::test]
async fn project_list_filtered_by_grant() {
    let w = member_world(Mode::SelfHosted).await;
    let other = w.metadata.create_project(w.tenant, "Other").await.unwrap().id;
    w.metadata.add_granted(w.tenant, "erin", w.project, PlatformRole::Viewer, &[]);
    w.metadata.add_granted(w.tenant, "fay", other, PlatformRole::Developer, &[]);
    w.metadata.add_member(w.tenant, "new", MemberStatus::Active, false);
    let ids = |sub: &'static str| {
        let app = &w.app;
        async move {
            let list: Value = body_of(app.send_as(sub, "org-1", Method::GET, "/v1/projects", None).await).await;
            let mut ids: Vec<String> = list.as_array().map(|a| a.iter().map(|p| p["id"].as_str().unwrap().to_string()).collect()).unwrap_or_default();
            ids.sort();
            ids
        }
    };
    assert_eq!(ids("erin").await, [w.project.to_string()]);
    assert_eq!(ids("fay").await, [other.to_string()]);
    let mut both = vec![w.project.to_string(), other.to_string()];
    both.sort();
    assert_eq!(ids(DEFAULT_SUB).await, both);
    // A member with no grant anywhere lists nothing.
    assert_eq!(ids("new").await, Vec::<String>::new());
}

// Databases: created under a project the caller administers (the body has no tenant_id).
#[tokio::test]
async fn create_database_needs_project_admin() {
    let w = member_world(Mode::SelfHosted).await;
    let other = w.metadata.create_project(w.tenant, "Other").await.unwrap().id;
    w.metadata.add_granted(w.tenant, "dev1", w.project, PlatformRole::Developer, &[]);
    w.metadata.add_granted(w.tenant, "ada", w.project, PlatformRole::Admin, &[]);
    let body = |project: ProjectId| json!({ "project_id": project, "name": "fresh" });

    let err = error_of(w.app.send_as("dev1", "org-1", Method::POST, "/v1/databases", Some(body(w.project))).await, 403).await;
    assert_eq!((code_of(&err), err["error"]["required"].as_str()), ("forbidden", Some("admin")));
    // A project the caller has no grant on is a missing one.
    let err = error_of(w.app.send_as("ada", "org-1", Method::POST, "/v1/databases", Some(body(other))).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    let resp = w.app.send_as("ada", "org-1", Method::POST, "/v1/databases", Some(body(w.project))).await;
    assert_eq!(resp.status(), 202);
    // A tenant admin passes anywhere.
    let resp = w.app.send_as(DEFAULT_SUB, "org-1", Method::POST, "/v1/databases", Some(body(other))).await;
    assert_eq!(resp.status(), 202);
}

// R20: the app-user routes act on the project's auth database, so its override applies.
#[tokio::test]
async fn app_user_routes_apply_the_auth_database_override() {
    let w = member_world(Mode::SelfHosted).await;
    let now = chrono::Utc::now();
    let enabled = |database_id: DatabaseId| ProjectAuth {
        project_id: w.project,
        status: AppUsersStatus::Enabled,
        error: None,
        database_id,
        pg_database: "web1".into(),
        schema_name: "public".into(),
        table_name: "users".into(),
        email_verification: false,
        on_delete: OnDelete::Delete,
        created_at: now,
        updated_at: now,
    };
    w.metadata.project_auths.lock().unwrap().insert(w.project, enabled(w.db));
    let pat = w.metadata.add_granted(w.tenant, "pat", w.project, PlatformRole::Admin, &[]);
    w.metadata.add_granted(w.tenant, "quin", w.project, PlatformRole::Admin, &[]);
    let base = format!("/v1/projects/{}", w.project);
    let create = json!({ "email": "a@example.test", "password": "correct-horse-9" });

    // Without an override: not refused (the identity client is missing, which is another answer).
    let ok = body_of(w.app.send_as("pat", "org-1", Method::DELETE, &format!("{base}/app-users"), None).await).await;
    assert!(!access_refusal(&ok), "{ok}");
    w.metadata.narrow(pat, w.db, OverrideRow { platform_role: Some(PlatformRole::Viewer), pg_roles: None });
    assert_eq!(w.app.send_as("pat", "org-1", Method::GET, &format!("{base}/app-users"), None).await.status(), 200);
    let err = error_of(w.app.send_as("pat", "org-1", Method::DELETE, &format!("{base}/app-users"), None).await, 403).await;
    assert_eq!(err["error"]["required"], "admin");
    let err = error_of(w.app.send_as("pat", "org-1", Method::POST, &format!("{base}/users"), Some(create.clone())).await, 403).await;
    assert_eq!(err["error"]["required"], "developer");
    // The same Admin grant without the override still passes.
    let ok = body_of(w.app.send_as("quin", "org-1", Method::POST, &format!("{base}/users"), Some(create.clone())).await).await;
    assert!(!access_refusal(&ok), "{ok}");

    // Fail closed: an auth database the check can't resolve is a refusal, not a pass.
    w.metadata.project_auths.lock().unwrap().insert(w.project, enabled(DatabaseId::new()));
    let err = error_of(w.app.send_as("quin", "org-1", Method::POST, &format!("{base}/users"), Some(create)).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
}

// --- T6 review fixes (R21-R25) ---------------------------------------------------------------

// R22: a LOGIN role is Admin-only to act as, to bind a key to, and to read a connection string
// for; a NOLOGIN group role is what a grant should name.
#[tokio::test]
async fn login_roles_are_admin_only() {
    let w = member_world(Mode::SelfHosted).await;
    let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
    w.metadata.add_group_role(&ep, "grp", &[]);
    w.metadata.add_role(&ep, "svc", RoleOrigin::Sql, false, false);
    w.metadata.add_granted(w.tenant, "dan", w.project, PlatformRole::Developer, &["grp", "svc"]);
    w.metadata.add_granted(w.tenant, "ada", w.project, PlatformRole::Admin, &["grp", "svc"]);
    let console = console_path(&w);
    let sql = format!("/v1/databases/{}/branches/{}/sql", w.db, w.dev.as_uuid());
    let keys = format!("/v1/databases/{}/branches/{}/keys", w.db, w.dev.as_uuid());
    let query = |role: &str| json!({ "role": role, "tree": select_tree() });
    let run = |role: &str| json!({ "run_id": uuid::Uuid::new_v4(), "role": role, "sql": "select 1" });

    let err = error_of(w.app.send_as("dan", "org-1", Method::POST, &console, Some(query("svc"))).await, 403).await;
    assert_eq!(err["error"]["required"], "admin");
    let err = error_of(w.app.send_as("dan", "org-1", Method::POST, &sql, Some(run("svc"))).await, 403).await;
    assert_eq!(err["error"]["required"], "admin");
    let err = error_of(w.app.send_as("dan", "org-1", Method::POST, &keys, Some(json!({ "name": "k", "role": "svc" }))).await, 403).await;
    assert_eq!(err["error"]["required"], "admin");
    let ok = body_of(w.app.send_as("dan", "org-1", Method::POST, &console, Some(query("grp"))).await).await;
    assert!(!access_refusal(&ok), "{ok}");
    assert_eq!(w.app.send_as("dan", "org-1", Method::POST, &keys, Some(json!({ "name": "k", "role": "grp" }))).await.status(), 201);
    // An Admin may use a login role.
    let ok = body_of(w.app.send_as("ada", "org-1", Method::POST, &console, Some(query("svc"))).await).await;
    assert!(!access_refusal(&ok), "{ok}");
    assert_eq!(w.app.send_as("ada", "org-1", Method::POST, &keys, Some(json!({ "name": "k2", "role": "svc" }))).await.status(), 201);

    // Connection strings: Admin for every role.
    for role in ["grp", "svc"] {
        let conn = format!("/v1/databases/{}/branches/{}/connection?role={role}", w.db, w.dev.as_uuid());
        let err = error_of(w.app.send_as("dan", "org-1", Method::GET, &conn, None).await, 403).await;
        assert_eq!(err["error"]["required"], "admin", "{role}");
        let ok = body_of(w.app.send_as("ada", "org-1", Method::GET, &conn, None).await).await;
        assert!(!access_refusal(&ok), "{role}: {ok}");
    }
}

// R21: a role reachable through a granted role's membership chain is allowed; a chain that
// reaches an owner-level, LOGIN or reserved role makes the granted role contribute nothing.
fn chain_world_roles(w: &MemberWorld) {
    let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
    w.metadata.add_group_role(&ep, "billing", &[]);
    w.metadata.add_group_role(&ep, "billing_admin", &["billing"]);
    w.metadata.add_role(&ep, "svc", RoleOrigin::Sql, false, false);
    w.metadata.add_role(&ep, "power", RoleOrigin::Sql, true, false);
    w.metadata.add_group_role(&ep, "via_login", &["billing", "svc"]);
    w.metadata.add_group_role(&ep, "via_reserved", &["billing", "pg_monitor"]);
    w.metadata.add_group_role(&ep, "via_owner_level", &["billing", "power"]);
    w.metadata.add_group_role(&ep, "via_owner", &["billing", "postgres"]);
    for (sub, granted) in [
        ("dan", "billing_admin"),
        ("e1", "via_login"),
        ("e2", "via_reserved"),
        ("e3", "via_owner_level"),
        ("e4", "via_owner"),
    ] {
        w.metadata.add_granted(w.tenant, sub, w.project, PlatformRole::Developer, &[granted]);
    }
}

// M3: an act-as reads the branch's roles once: `authorize` carries the graph it loaded to the
// handler, which doesn't read it again.
#[tokio::test]
async fn act_as_reads_the_branch_roles_once() {
    let w = member_world(Mode::SelfHosted).await;
    chain_world_roles(&w);
    w.metadata.add_granted(w.tenant, "direct", w.project, PlatformRole::Developer, &["billing"]);
    w.metadata.add_branch_db(w.dev, "web", "postgres", chelabase_metadata::DatabaseOrigin::Main);
    let sql = format!("/v1/databases/{}/branches/{}/sql", w.db, w.dev.as_uuid());
    // `dan` is granted `billing_admin` and acts as `billing` (through the chain); `direct` is
    // granted `billing` itself.
    for (sub, role) in [("dan", "billing"), ("direct", "billing")] {
        let before = w.metadata.role_reads.load(Ordering::SeqCst);
        let run = json!({ "run_id": uuid::Uuid::new_v4(), "role": role, "sql": "select 1" });
        let answer = body_of(w.app.send_as(sub, "org-1", Method::POST, &sql, Some(run)).await).await;        assert_eq!(w.metadata.role_reads.load(Ordering::SeqCst) - before, 1, "{sub} acting as {role}: {answer}");
    }
}

#[tokio::test]
async fn act_as_chain_reachable_role_allowed() {
    let w = member_world(Mode::SelfHosted).await;
    chain_world_roles(&w);
    let console = console_path(&w);
    let query = json!({ "role": "billing", "tree": select_tree() });
    let ok = body_of(w.app.send_as("dan", "org-1", Method::POST, &console, Some(query.clone())).await).await;
    assert!(!access_refusal(&ok), "{ok}");
    // The sql editor takes the same chain, through dan's member role (granted `billing_admin`).
    let dan = w.metadata.members.lock().unwrap().iter().find(|m| m.identity_user_id == "dan").unwrap().id;
    let role_name = chelabase_types::role_name::member_role_name(dan);
    let password_enc =
        chelabase_secrets::seal_member_role_password(&w.app.key, w.dev.as_uuid(), &role_name, "pw").unwrap();
    w.metadata.member_roles.lock().unwrap().push(MemberBranchRole {
        member_id: dan,
        branch_id: w.dev.as_uuid(),
        role_name,
        password_enc,
        applied_roles: vec!["billing_admin".into()],
    });
    let sql = format!("/v1/databases/{}/branches/{}/sql", w.db, w.dev.as_uuid());
    let run = json!({ "run_id": uuid::Uuid::new_v4(), "role": "billing", "sql": "select 1" });
    let ok = body_of(w.app.send_as("dan", "org-1", Method::POST, &sql, Some(run)).await).await;
    assert!(!access_refusal(&ok), "{ok}");
    // A chain through a LOGIN, reserved, owner-level or owner role contributes nothing.
    for sub in ["e1", "e2", "e3", "e4"] {
        let err = error_of(w.app.send_as(sub, "org-1", Method::POST, &console, Some(query.clone())).await, 403).await;
        assert_eq!(err["error"]["required"], "role:billing", "{sub}");
    }
    // Not reachable at all.
    let other = json!({ "role": "svc", "tree": select_tree() });
    let err = error_of(w.app.send_as("dan", "org-1", Method::POST, &console, Some(other)).await, 403).await;
    assert!(matches!(err["error"]["required"].as_str(), Some("role:svc" | "admin")), "{err}");
}

/// `deep` → `mid` → `svc` (LOGIN), and `deep_res` → `pg_monitor` (reserved), granted to the
/// Developer `dee`.
fn deep_chain_roles(w: &MemberWorld) {
    let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
    w.metadata.add_role(&ep, "svc", RoleOrigin::Sql, false, false);
    w.metadata.add_group_role(&ep, "mid", &["svc"]);
    w.metadata.add_group_role(&ep, "deep", &["mid"]);
    w.metadata.add_group_role(&ep, "deep_res", &["pg_monitor"]);
    w.metadata.add_granted(w.tenant, "dee", w.project, PlatformRole::Developer, &["deep", "deep_res"]);
}

// Single rule (T7 merge, R33): acting as a role whose chain reaches a LOGIN role (however deep)
// or a reserved role is Admin-only, decided by `RoleGraph::refusal` in `authorize`.
#[tokio::test]
async fn act_as_a_role_reaching_a_deeper_login_or_reserved_role_needs_admin() {
    let w = member_world(Mode::SelfHosted).await;
    deep_chain_roles(&w);
    let console = console_path(&w);
    let sql = format!("/v1/databases/{}/branches/{}/sql", w.db, w.dev.as_uuid());
    for role in ["deep", "deep_res"] {
        let query = json!({ "role": role, "tree": select_tree() });
        let err = error_of(w.app.send_as("dee", "org-1", Method::POST, &console, Some(query)).await, 403).await;
        assert_eq!((code_of(&err), err["error"]["required"].as_str()), ("forbidden", Some("admin")), "{role}");
        let run = json!({ "run_id": uuid::Uuid::new_v4(), "role": role, "sql": "select 1" });
        let err = error_of(w.app.send_as("dee", "org-1", Method::POST, &sql, Some(run)).await, 403).await;
        assert_eq!(err["error"]["required"], "admin", "{role}");
    }
    assert!(w.jobs.member_reconciles.lock().unwrap().is_empty());
}

// R33 (user): a data key below Admin can't be bound to a role whose chain reaches a LOGIN,
// owner-level or reserved role.
#[tokio::test]
async fn key_bound_to_a_role_reaching_a_login_or_reserved_role_needs_admin() {
    let w = member_world(Mode::SelfHosted).await;
    deep_chain_roles(&w);
    let keys = format!("/v1/databases/{}/branches/{}/keys", w.db, w.dev.as_uuid());
    for role in ["deep", "deep_res"] {
        let create = json!({ "name": format!("k-{role}"), "role": role });
        let err = error_of(w.app.send_as("dee", "org-1", Method::POST, &keys, Some(create)).await, 403).await;
        assert_eq!((code_of(&err), err["error"]["required"].as_str()), ("forbidden", Some("admin")), "{role}");
    }
}

#[tokio::test]
async fn key_bound_to_chain_reachable_role_allowed() {
    let w = member_world(Mode::SelfHosted).await;
    chain_world_roles(&w);
    let keys = format!("/v1/databases/{}/branches/{}/keys", w.db, w.dev.as_uuid());
    let create = json!({ "name": "bill", "role": "billing" });
    assert_eq!(w.app.send_as("dan", "org-1", Method::POST, &keys, Some(create.clone())).await.status(), 201);
    let key = seed_key(&w, DataKeyKind::Secret, "billing", "seeded");
    let one = format!("{keys}/{key}");
    assert_eq!(w.app.send_as("dan", "org-1", Method::PATCH, &one, Some(json!({ "databases": null }))).await.status(), 200);
    assert_eq!(w.app.send_as("dan", "org-1", Method::POST, &format!("{one}/rotate"), None).await.status(), 201);
    for sub in ["e1", "e2", "e3", "e4"] {
        let err = error_of(w.app.send_as(sub, "org-1", Method::POST, &keys, Some(create.clone())).await, 403).await;
        assert_eq!(err["error"]["required"], "role:billing", "{sub}");
    }
}

// M7: a Viewer's `update` and `call` are writes too, alone or inside a batch.
#[tokio::test]
async fn viewer_console_refuses_update_and_call() {
    let w = member_world(Mode::SelfHosted).await;
    let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
    w.metadata.add_group_role(&ep, "app", &[]);
    w.metadata.add_granted(w.tenant, "vic", w.project, PlatformRole::Viewer, &["app"]);
    let path = console_path(&w);
    let update = json!({ "v": 1, "op": "update", "table": "notes", "set": { "body": "y" }, "where": { "col": "id", "eq": 1 } });
    let call = json!({ "v": 1, "op": "call", "fn": "touch", "args": {} });
    let batch_call = json!({ "v": 1, "op": "batch", "ops": [select_tree(), { "op": "call", "fn": "touch" }] });
    for tree in [update, call, batch_call] {
        let err = error_of(w.app.send_as("vic", "org-1", Method::POST, &path, Some(json!({ "role": "app", "tree": tree }))).await, 403).await;
        assert_eq!(err["error"]["required"], "developer");
    }
}

fn set_plan(w: &MemberWorld, status: MergeStatus, plan: Value) {
    let mut rows = w.metadata.merges.lock().unwrap();
    let m = rows.iter_mut().find(|r| r.id == w.merge).unwrap();
    m.status = status;
    m.analysis = Some(json!({"conflicts": []}));
    m.plan = Some(plan);
    m.rehearsal = Some(json!({"ok": true}));
    if status == MergeStatus::Failed {
        m.failed_phase = Some("tx".into());
    }
}

// R25 and M3: a retry that re-runs security steps, and a change to their acknowledgements, are
// Admin's; a step the orchestrator can't read counts as a security step.
#[tokio::test]
async fn merge_security_retry_and_acknowledgements_need_admin() {
    let w = member_world(Mode::SelfHosted).await;
    w.metadata.add_granted(w.tenant, "dev1", w.project, PlatformRole::Developer, &[]);
    w.metadata.add_granted(w.tenant, "ada", w.project, PlatformRole::Admin, &[]);
    let merge = format!("/v1/databases/{}/merges/{}", w.db, w.merge);
    let secure = json!({ "steps": [{ "id": "grant", "security": true, "items": ["role:x"] }] });

    // Retry.
    set_plan(&w, MergeStatus::Failed, secure.clone());
    let err = error_of(w.app.send_as("dev1", "org-1", Method::POST, &format!("{merge}/retry"), Some(json!({}))).await, 403).await;
    assert_eq!(err["error"]["required"], "admin");
    assert_eq!(w.app.send_as("ada", "org-1", Method::POST, &format!("{merge}/retry"), Some(json!({}))).await.status(), 202);
    set_plan(&w, MergeStatus::Failed, json!({ "steps": [{ "id": "t1", "security": false }] }));
    assert_eq!(w.app.send_as("dev1", "org-1", Method::POST, &format!("{merge}/retry"), Some(json!({}))).await.status(), 202);

    // Acknowledgements.
    set_plan(&w, MergeStatus::Rehearsed, secure);
    let version = w.metadata.merges.lock().unwrap().iter().find(|r| r.id == w.merge).unwrap().version;
    let err = error_of(w.app.send_as("dev1", "org-1", Method::PATCH, &merge, Some(json!({ "version": version, "acknowledgements": ["role:x"] }))).await, 403).await;
    assert_eq!(err["error"]["required"], "admin");
    // Other items stay a Developer's.
    let resp = w.app.send_as("dev1", "org-1", Method::PATCH, &merge, Some(json!({ "version": version, "acknowledgements": ["other"] }))).await;
    assert_eq!(resp.status(), 200);
    let resp = w.app.send_as("ada", "org-1", Method::PATCH, &merge, Some(json!({ "version": version + 1, "acknowledgements": ["other", "role:x"] }))).await;
    assert_eq!(resp.status(), 200);

    // M3: a step that doesn't parse and carries no flag counts as a security step.
    set_plan(&w, MergeStatus::Rehearsed, json!({ "steps": [{ "wat": 1 }] }));
    let err = error_of(w.app.send_as("dev1", "org-1", Method::POST, &format!("{merge}/apply"), Some(json!({}))).await, 403).await;
    assert_eq!(err["error"]["required"], "admin");
}

// I2: the database that holds a project's app users can't be deleted from under them.
#[tokio::test]
async fn delete_database_refuses_the_app_auth_database() {
    let w = member_world(Mode::SelfHosted).await;
    let now = chrono::Utc::now();
    w.metadata.project_auths.lock().unwrap().insert(
        w.project,
        ProjectAuth {
            project_id: w.project,
            status: AppUsersStatus::Enabled,
            error: None,
                database_id: w.db,
            pg_database: "web1".into(),
            schema_name: "public".into(),
            table_name: "users".into(),
            email_verification: false,
            on_delete: OnDelete::Delete,
            created_at: now,
            updated_at: now,
        },
    );
    let path = format!("/v1/databases/{}", w.db);
    let err = error_of(w.app.send_as(DEFAULT_SUB, "org-1", Method::DELETE, &path, None).await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    assert!(message_of(&err).contains("app users"), "{err}");
    // Once app users are gone the delete goes ahead.
    w.metadata.project_auths.lock().unwrap().clear();
    assert_eq!(w.app.send_as(DEFAULT_SUB, "org-1", Method::DELETE, &path, None).await.status(), 202);
}

// I5: the tenant of a new database comes from the caller, whatever the body says.
#[tokio::test]
async fn create_database_takes_the_tenant_from_the_caller() {
    let w = member_world(Mode::SelfHosted).await;
    let foreign = TenantId::new();
    let body = json!({ "project_id": w.project, "name": "fresh", "tenant_id": foreign });
    let resp = w.app.send_as(DEFAULT_SUB, "org-1", Method::POST, "/v1/databases", Some(body)).await;
    assert_eq!(resp.status(), 202);
    let published = w.jobs.published.lock().unwrap();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].payload.tenant_id, w.tenant);
    assert_eq!(published[0].payload.project_id, w.project);
}

// ---- T7: the member management API --------------------------------------------------------

/// The member world (org-1's tenant; project Billing with database web1, owner `postgres`,
/// main plus `dev`), with the identity fake behind the orchestrator (its members pool exists)
/// and `ops` as the operator tenant, whose active member `operator` is.
struct Mw {
    w: MemberWorld,
    identity: FakeIdentity,
}

async fn mw(mode: Mode) -> Mw {
    mw_with(mode, true).await
}

/// `mw` whose orchestrator has no identity client.
async fn mw_without_identity() -> Mw {
    mw_with(Mode::SelfHosted, false).await
}

async fn mw_with(mode: Mode, with_identity: bool) -> Mw {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let ops = TenantId::new();
    metadata.bind_org("ops", ops);
    metadata.add_member(ops, "operator", MemberStatus::Active, false);
    let jobs = FakeJobs::default();
    let identity = FakeIdentity::start().await;
    identity.with_pool(&Pool::Members);
    let app = spawn_app_core_with(
        metadata.clone(),
        jobs.clone(),
        "http://127.0.0.1:9",
        mode,
        10,
        Arc::new(chelabase_query::app_users::PgUsersDb::new()),
        None,
        Some(ops),
        None,
        with_identity.then(|| identity.client()),
    )
    .await;
    let project = metadata.create_project(tenant, "Billing").await.unwrap().id;
    let mut db = sample_database(tenant);
    db.project_id = project;
    db.name = "web1".into();
    db.database_name = Some("web1".into());
    db.owner_role = Some("postgres".into());
    let (db_id, main) = (db.database_id, db.main.id);
    metadata.insert_database(db);
    let dev = metadata.add_branch(db_id, "dev", main, BranchKind::Normal, BranchStatus::Ready);
    let w = MemberWorld { app, metadata, tenant, project, db: db_id, dev, merge: uuid::Uuid::nil(), jobs };
    Mw { w, identity }
}

impl Mw {
    async fn send(&self, sub: &str, method: Method, path: &str, body: Option<Value>) -> reqwest::Response {
        self.w.app.send_as(sub, "org-1", method, path, body).await
    }
    /// Main and dev, as `project_branches` lists them.
    fn branches(&self) -> Vec<BranchId> {
        let mut out = vec![BranchId::from(self.w.db), self.w.dev];
        out.sort_by_key(|b| b.as_uuid());
        out
    }
    fn enqueued(&self) -> Vec<BranchId> {
        let mut out = self.w.jobs.member_role_enqueues.lock().unwrap().clone();
        out.sort_by_key(|b| b.as_uuid());
        out.dedup();
        out
    }
    fn clear_enqueued(&self) {
        self.w.jobs.member_role_enqueues.lock().unwrap().clear();
    }
    fn grant_of(&self, member: uuid::Uuid) -> Option<GrantRow> {
        self.w.metadata.grants.lock().unwrap().get(&(member, self.w.project.as_uuid())).cloned()
    }
    fn actions(&self) -> Vec<String> {
        self.w.metadata.events.lock().unwrap().iter().map(|e| e.action.clone()).collect()
    }
    fn main_endpoint(&self) -> String {
        self.w.metadata.branch(BranchId::from(self.w.db)).endpoint_id.unwrap()
    }
}

fn invite_body(w: &MemberWorld, email: &str, role: &str, roles: &[&str]) -> Value {
    json!({ "email": email, "grants": [{ "project_id": w.project, "platform_role": role, "pg_roles": roles }] })
}

#[tokio::test]
async fn invite_creates_identity_user_and_invited_member() {
    let t = mw(Mode::SelfHosted).await;
    let resp = t
        .send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "  New@Example.COM ", "developer", &["app"])))
        .await;
    assert_eq!(resp.status(), 201);
    let body = body_of(resp).await;
    assert_eq!(body["outcome"], "invited");
    let member = &body["member"];
    assert_eq!((member["email"].as_str(), member["status"].as_str()), (Some("new@example.com"), Some("invited")));
    assert_eq!(member["tenant_admin"], false);
    let invite_id = member["invite"]["id"].as_str().unwrap().to_string();
    let expires: chrono::DateTime<chrono::Utc> = member["invite"]["expires_at"].as_str().unwrap().parse().unwrap();
    let hours = (expires - chrono::Utc::now()).num_minutes();
    assert!((71 * 60..=72 * 60).contains(&hours), "the invite lives 72 h: {hours} min");

    // The identity service: a passwordless user in the members pool, and only the invite code,
    // mailed with a link to the accept page (the code itself isn't in it).
    let sub = sub_of(&t, "new@example.com");
    assert!(!t.identity.user(&Pool::Members, &sub).unwrap().has_password);
    let sent = t.identity.codes_sent();
    assert_eq!(sent.len(), 1);
    assert_eq!((sent[0].user_id.as_str(), sent[0].kind), (sub.as_str(), chelabase_identity::CodeKind::Invite));
    let link = sent[0].link.clone().unwrap();
    assert!(link.starts_with(&format!("http://studio.test/accept-invite?user={sub}&t=")), "{link}");
    assert!(!link.contains(&sent[0].code), "{link}");
    // Metadata: an invited member with its (inert) grant, an invite, one event.
    let id: uuid::Uuid = member["id"].as_str().unwrap().parse().unwrap();
    let stored = t.w.metadata.members.lock().unwrap().iter().find(|m| m.id == id).cloned().unwrap();
    assert_eq!((stored.identity_user_id.as_str(), stored.status), (sub.as_str(), MemberStatus::Invited));
    assert_eq!(t.grant_of(id), Some(GrantRow { platform_role: PlatformRole::Developer, pg_roles: vec!["app".into()] }));
    assert_eq!(t.actions(), vec!["invite_created"]);
    // Everyone in the tenant sees the invited member and its open invite.
    t.w.metadata.add_granted(t.w.tenant, "viewer", t.w.project, PlatformRole::Viewer, &[]);
    let list = body_of(t.send("viewer", Method::GET, "/v1/members", None).await).await;
    let listed = list.as_array().unwrap().iter().find(|m| m["id"] == member["id"]).cloned().unwrap();
    assert_eq!(listed["invite"]["id"], invite_id.as_str());
}

// Review focus 3, R8 and R18: a member is matched by identity user id first, then by a non-NULL
// email (trimmed, any case); no second identity user is made.
#[tokio::test]
async fn invite_matches_existing_member_case_insensitively() {
    let t = mw(Mode::SelfHosted).await;
    let bob = t.w.metadata.add_member(t.w.tenant, "bob", MemberStatus::Active, false);
    t.w.metadata.set_member(t.w.tenant, "bob", |m| m.email = Some("bob@x.com".into()));
    let resp = t
        .send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, " Bob@X.com", "viewer", &[])))
        .await;
    assert_eq!(resp.status(), 200);
    let body = body_of(resp).await;
    assert_eq!(body["outcome"], "existing_member");
    assert_eq!(body["member"]["id"], bob.to_string());
    assert!(!t.identity.calls().iter().any(|c| c == "POST /internal/v1/pools/members/users"), "{:?}", t.identity.calls());
    assert_eq!(t.grant_of(bob).map(|g| g.platform_role), Some(PlatformRole::Viewer));
    assert_eq!(t.enqueued(), t.branches());

    // Id first: the identity service says carol@x.com is user Z; the member with Z has no stored
    // email (it went stale) while another member still holds it.
    let z = t.identity.with_user(&Pool::Members, "carol@x.com", "Secret-pass1");
    let carol = t.w.metadata.add_member(t.w.tenant, &z, MemberStatus::Active, false);
    t.w.metadata.set_member(t.w.tenant, &z, |m| m.email = None);
    let stale = t.w.metadata.add_member(t.w.tenant, "stale", MemberStatus::Active, false);
    t.w.metadata.set_member(t.w.tenant, "stale", |m| m.email = Some("carol@x.com".into()));
    let body = body_of(
        t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "CAROL@x.com", "developer", &[])))
            .await,
    )
    .await;
    assert_eq!(body["outcome"], "existing_member");
    assert_eq!(body["member"]["id"], carol.to_string());
    assert_eq!(t.grant_of(stale), None);
}

#[tokio::test]
async fn project_admin_invites_into_own_project_only() {
    let t = mw(Mode::SelfHosted).await;
    let other = t.w.metadata.create_project(t.w.tenant, "Other").await.unwrap().id;
    t.w.metadata.add_granted(t.w.tenant, "padmin", t.w.project, PlatformRole::Admin, &[]);
    let body = json!({ "email": "eve@x.com", "platform_role": "viewer", "pg_roles": [] });
    let own = format!("/v1/projects/{}/invites", t.w.project);
    let resp = t.send("padmin", Method::POST, &own, Some(body.clone())).await;
    assert_eq!(resp.status(), 201);
    let eve: uuid::Uuid = body_of(resp).await["member"]["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(t.grant_of(eve).map(|g| g.platform_role), Some(PlatformRole::Viewer));
    // Another project: no grant there, so it doesn't exist for them.
    let theirs = format!("/v1/projects/{other}/invites");
    let err = error_of(t.send("padmin", Method::POST, &theirs, Some(body.clone())).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    // Tenant invites are a tenant admin's.
    let err = error_of(
        t.send("padmin", Method::POST, "/v1/invites", Some(invite_body(&t.w, "fay@x.com", "viewer", &[]))).await,
        403,
    )
    .await;
    assert_eq!((code_of(&err), err["error"]["required"].as_str()), ("forbidden", Some("tenant admin")));
    // A Developer can't invite into the project.
    t.w.metadata.add_granted(t.w.tenant, "dev", t.w.project, PlatformRole::Developer, &[]);
    let err = error_of(t.send("dev", Method::POST, &own, Some(body)).await, 403).await;
    assert_eq!(err["error"]["required"], "admin");
}

#[tokio::test]
async fn override_cannot_widen() {
    let t = mw(Mode::SelfHosted).await;
    let m = t.w.metadata.add_granted(t.w.tenant, "m", t.w.project, PlatformRole::Developer, &["app", "web"]);
    let path = format!("/v1/projects/{}/grants/{m}/overrides", t.w.project);
    let put = |body: Value| t.send(DEFAULT_SUB, Method::PUT, &path, Some(body));
    for widening in [
        json!({ "database_id": t.w.db, "platform_role": "admin" }),
        json!({ "database_id": t.w.db, "pg_roles": ["app", "other"] }),
        json!({ "database_id": t.w.db }),
    ] {
        let err = error_of(put(widening.clone()).await, 422).await;
        assert_eq!(code_of(&err), "invalid_request", "{widening}");
    }
    assert!(t.w.metadata.overrides.lock().unwrap().is_empty());
    let resp = put(json!({ "database_id": t.w.db, "platform_role": "viewer", "pg_roles": ["web"] })).await;
    assert_eq!(resp.status(), 200);
    let body = body_of(resp).await;
    assert_eq!((body["platform_role"].as_str(), body["pg_roles"][0].as_str()), (Some("viewer"), Some("web")));
    assert_eq!(
        t.w.metadata.overrides.lock().unwrap().get(&(m, t.w.db.as_uuid())),
        Some(&OverrideRow { platform_role: Some(PlatformRole::Viewer), pg_roles: Some(vec!["web".into()]) })
    );
    // A member with no grant on the project has nothing to narrow.
    let bare = t.w.metadata.add_member(t.w.tenant, "bare", MemberStatus::Active, false);
    let bare_path = format!("/v1/projects/{}/grants/{bare}/overrides", t.w.project);
    let err = error_of(
        t.send(DEFAULT_SUB, Method::PUT, &bare_path, Some(json!({ "database_id": t.w.db, "platform_role": "viewer" }))).await,
        422,
    )
    .await;
    assert_eq!(code_of(&err), "invalid_request");
    // Another project's database isn't this project's.
    let other = t.w.metadata.create_project(t.w.tenant, "Other").await.unwrap().id;
    let mut odb = sample_database(t.w.tenant);
    odb.project_id = other;
    let odb_id = odb.database_id;
    t.w.metadata.insert_database(odb);
    let err = error_of(put(json!({ "database_id": odb_id, "platform_role": "viewer" })).await, 404).await;
    assert_eq!(message_of(&err), "database not found");
    // Deleting it: once, then 404.
    let del = format!("{path}?database_id={}", t.w.db);
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &del, None).await.status(), 204);
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &del, None).await.status(), 404);
    let err = error_of(t.send(DEFAULT_SUB, Method::DELETE, &path, None).await, 400).await;
    assert_eq!(code_of(&err), "invalid_request");
}

// R17 (user, 2026-10-14): a project Admin's grant is every role on the project (`"all"`),
// whatever roles the request names; Developer and Viewer grants keep their named roles.
#[tokio::test]
async fn an_admin_grant_holds_every_role() {
    let t = mw(Mode::SelfHosted).await;
    let m = t.w.metadata.add_member(t.w.tenant, "m", MemberStatus::Active, false);
    let grant = format!("/v1/projects/{}/grants/{m}", t.w.project);
    for roles in [json!([]), json!(["a", "b"])] {
        let resp = t.send(DEFAULT_SUB, Method::PUT, &grant, Some(json!({ "platform_role": "admin", "pg_roles": roles }))).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(body_of(resp).await["pg_roles"], json!("all"));
    }
    let list = body_of(t.send(DEFAULT_SUB, Method::GET, &format!("/v1/projects/{}/grants", t.w.project), None).await).await;
    let row = list.as_array().unwrap().iter().find(|g| g["member_id"] == json!(m.to_string())).unwrap().clone();
    assert_eq!(row["pg_roles"], json!("all"));
    let resp = t.send(DEFAULT_SUB, Method::PUT, &grant, Some(json!({ "platform_role": "developer", "pg_roles": ["a"] }))).await;
    assert_eq!(body_of(resp).await["pg_roles"], json!(["a"]));
}

#[tokio::test]
async fn grant_change_enqueues_reconcile_for_every_branch() {
    let t = mw(Mode::SelfHosted).await;
    let m = t.w.metadata.add_member(t.w.tenant, "m", MemberStatus::Active, false);
    let grant = format!("/v1/projects/{}/grants/{m}", t.w.project);
    let resp = t
        .send(DEFAULT_SUB, Method::PUT, &grant, Some(json!({ "platform_role": "developer", "pg_roles": ["b", "a", "b"] })))
        .await;
    assert_eq!(resp.status(), 200);
    let body = body_of(resp).await;
    assert_eq!((body["member_id"].as_str(), body["pg_roles"].clone()), (Some(m.to_string().as_str()), json!(["a", "b"])));
    assert_eq!(t.enqueued(), t.branches());
    let list = body_of(t.send(DEFAULT_SUB, Method::GET, &format!("/v1/projects/{}/grants", t.w.project), None).await).await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["platform_role"], "developer");

    let overrides = format!("{grant}/overrides");
    for (method, path, body) in [
        (Method::PUT, overrides.clone(), Some(json!({ "database_id": t.w.db, "platform_role": "viewer" }))),
        (Method::DELETE, format!("{overrides}?database_id={}", t.w.db), None),
        (Method::DELETE, grant.clone(), None),
    ] {
        t.clear_enqueued();
        let resp = t.send(DEFAULT_SUB, method.clone(), &path, body).await;
        assert!(resp.status().is_success(), "{method} {path}: {}", resp.status());
        assert_eq!(t.enqueued(), t.branches(), "{method} {path}");
    }
    assert_eq!(t.actions(), vec!["grant_put", "override_put", "override_deleted", "grant_deleted"]);
    // Nothing changed, nothing enqueued: a missing grant is 404.
    t.clear_enqueued();
    let err = error_of(t.send(DEFAULT_SUB, Method::DELETE, &grant, None).await, 404).await;
    assert_eq!(message_of(&err), "grant not found");
    assert!(t.enqueued().is_empty());
}

#[tokio::test]
async fn remove_member_bans_and_enqueues() {
    let t = mw(Mode::SelfHosted).await;
    let z = t.identity.with_user(&Pool::Members, "gil@x.com", "Secret-pass1");
    let gil = t.w.metadata.add_granted(t.w.tenant, &z, t.w.project, PlatformRole::Developer, &["app"]);
    t.identity.with_session(&Pool::Members, &z);
    let other = t.identity.with_user(&Pool::Members, "hal@x.com", "Secret-pass1");
    t.identity.with_session(&Pool::Members, &other);
    assert_eq!(t.send(&z, Method::GET, "/v1/projects", None).await.status(), 200);

    let path = format!("/v1/members/{gil}");
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &path, None).await.status(), 204);
    assert!(t.identity.user(&Pool::Members, &z).unwrap().banned);
    assert_eq!(t.identity.session_count(&Pool::Members), 1, "only gil's session ended");
    assert_eq!(t.enqueued(), t.branches());
    assert_eq!(t.w.metadata.member(t.w.tenant, &z).unwrap().status, MemberStatus::Removed);
    assert_eq!(t.grant_of(gil), None);
    // Their still-valid token is refused at once.
    let err = error_of(t.send(&z, Method::GET, &format!("/v1/projects/{}", t.w.project), None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    // Removing again is a retry: already banned, which is fine.
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &path, None).await.status(), 204);
    // Another tenant's member, or none, is 404.
    let foreign = TenantId::new();
    let stranger = t.w.metadata.add_member(foreign, "s", MemberStatus::Active, false);
    let err = error_of(t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/members/{stranger}"), None).await, 404).await;
    assert_eq!(message_of(&err), "member not found");
}

#[tokio::test]
async fn last_admin_demotion_conflicts() {
    let t = mw(Mode::SelfHosted).await;
    let me = t.w.metadata.member(t.w.tenant, DEFAULT_SUB).unwrap().id;
    let path = format!("/v1/members/{me}");
    let err = error_of(t.send(DEFAULT_SUB, Method::PATCH, &path, Some(json!({ "tenant_admin": false }))).await, 409).await;
    assert_eq!(code_of(&err), "last_tenant_admin");
    let err = error_of(t.send(DEFAULT_SUB, Method::DELETE, &path, None).await, 409).await;
    assert_eq!(code_of(&err), "last_tenant_admin");
    // With a second admin, the first may step down.
    let other = t.w.metadata.add_member(t.w.tenant, "other", MemberStatus::Active, false);
    let resp = t
        .send(DEFAULT_SUB, Method::PATCH, &format!("/v1/members/{other}"), Some(json!({ "tenant_admin": true })))
        .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(body_of(resp).await["tenant_admin"], true);
    t.clear_enqueued();
    let resp = t.send(DEFAULT_SUB, Method::PATCH, &path, Some(json!({ "tenant_admin": false }))).await;
    assert_eq!(resp.status(), 200);
    // A tenant admin's roles cover every project: every branch reconciles.
    assert_eq!(t.enqueued(), t.branches());
}

// R9: only an active member may become a tenant admin.
#[tokio::test]
async fn promote_invited_member_conflicts() {
    let t = mw(Mode::SelfHosted).await;
    let invited = t.w.metadata.add_member(t.w.tenant, "inv", MemberStatus::Invited, false);
    let err = error_of(
        t.send(DEFAULT_SUB, Method::PATCH, &format!("/v1/members/{invited}"), Some(json!({ "tenant_admin": true }))).await,
        409,
    )
    .await;
    assert_eq!(code_of(&err), "conflict");
    assert!(!t.w.metadata.member(t.w.tenant, "inv").unwrap().tenant_admin);
}

#[tokio::test]
async fn open_signup_platform_refused() {
    // Platform mode: an operator may read the settings, never switch open sign-up on.
    let t = mw(Mode::Platform).await;
    let op = |method: Method, body: Option<Value>| {
        t.w.app.send_as("operator", "ops", method, "/v1/instance/settings", body)
    };
    let err = error_of(op(Method::PATCH, Some(json!({ "open_signup": true }))).await, 403).await;
    assert_eq!(code_of(&err), "not_available_in_platform_mode");
    let settings = body_of(op(Method::GET, None).await).await;
    assert_eq!(settings, json!({ "open_signup": false, "open_signup_switchable": false }));

    // Self-hosted: the tenant admin switches it; anyone else is refused.
    let t = mw(Mode::SelfHosted).await;
    let resp = t.send(DEFAULT_SUB, Method::PATCH, "/v1/instance/settings", Some(json!({ "open_signup": true }))).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(body_of(resp).await, json!({ "open_signup": true, "open_signup_switchable": true }));
    assert_eq!(t.actions(), vec!["open_signup_set"]);
    let got = body_of(t.send(DEFAULT_SUB, Method::GET, "/v1/instance/settings", None).await).await;
    assert_eq!(got["open_signup"], true);
    t.w.metadata.add_granted(t.w.tenant, "padmin", t.w.project, PlatformRole::Admin, &[]);
    let err = error_of(t.send("padmin", Method::GET, "/v1/instance/settings", None).await, 403).await;
    assert_eq!(err["error"]["required"], "instance admin");
}

// ---- T7 review fixes --------------------------------------------------------------------------

/// The role pool of the grant-invariant tests, on every database's branches:
/// `plain` and `group2` are NOLOGIN groups; `login` is a LOGIN role; `boss` reaches the owner
/// (`postgres`) on the first database only, and is a plain group on the second. `lead` holds
/// ADMIN on `plain` and `sneaky` is a member of the owner without the SET option on the first
/// database only (I2: neither is reach, both make the chain unsafe).
const POOL: [&str; 6] = ["plain", "group2", "boss", "login", "lead", "sneaky"];

/// Whether `role` may be held below Admin on database `first` (true) or the second (false).
fn pool_bad(first: bool, role: &str) -> bool {
    role == "login" || role == "lead" || (first && (role == "boss" || role == "sneaky"))
}

impl Mw {
    /// A second database of the project (its own endpoint), and the pool's roles on both mains.
    fn second_database(&self) -> DatabaseId {
        let mut db = sample_database(self.w.tenant);
        db.project_id = self.w.project;
        db.name = "web2".into();
        db.owner_role = Some("postgres".into());
        db.main.endpoint_id = Some("ep-second000000".into());
        let id = db.database_id;
        self.w.metadata.insert_database(db);
        for (ep, first) in [(self.main_endpoint(), true), ("ep-second000000".to_string(), false)] {
            let md = &self.w.metadata;
            md.add_group_role(&ep, "plain", &[]);
            md.add_group_role(&ep, "group2", &[]);
            md.add_group_role(&ep, "boss", if first { &["postgres"] } else { &[] });
            md.add_role(&ep, "login", RoleOrigin::Sql, false, false);
            md.add_group_role_with(&ep, "lead", &["plain"], &[], &["plain"]);
            if first {
                md.add_group_role_with(&ep, "sneaky", &["postgres"], &["postgres"], &[]);
            } else {
                md.add_group_role(&ep, "sneaky", &[]);
            }
        }
        id
    }
}

// I4: a grant, an invite of an existing member and an override are written only while the
// member's access is what the check read. Each of these has the other request's write land
// between the check and the write; the check runs again on the new state and refuses.
#[tokio::test]
async fn grant_put_checks_again_when_an_override_lands_in_between() {
    let t = mw(Mode::SelfHosted).await;
    t.second_database();
    let m = t.w.metadata.add_member(t.w.tenant, "m", MemberStatus::Active, false);
    t.w.metadata.grant(m, t.w.project, PlatformRole::Admin, &["plain"]);
    let db = t.w.db;
    // Admin may hold `boss`; but another request narrows the member to Developer with `boss`
    // on web1 (R17: an Admin grant keeps the roles an override names).
    t.w.metadata.race_once(move |md| {
        md.narrow(m, db, OverrideRow { platform_role: Some(PlatformRole::Developer), pg_roles: Some(vec!["boss".into()]) });
    });
    let grant = format!("/v1/projects/{}/grants/{m}", t.w.project);
    let err = error_of(
        t.send(DEFAULT_SUB, Method::PUT, &grant, Some(json!({ "platform_role": "admin", "pg_roles": ["plain", "boss"] }))).await,
        422,
    )
    .await;
    assert_eq!((err["error"]["role"].as_str(), err["error"]["database_id"].as_str()), (Some("boss"), Some(db.to_string().as_str())), "{err}");
    assert_eq!(t.grant_of(m).unwrap().pg_roles, vec!["plain".to_string()], "nothing written");
}

#[tokio::test]
async fn invite_of_an_existing_member_checks_again_when_an_override_lands_in_between() {
    let t = mw(Mode::SelfHosted).await;
    t.second_database();
    let m = t.w.metadata.add_member(t.w.tenant, "m", MemberStatus::Active, false);
    t.w.metadata.set_member(t.w.tenant, "m", |r| r.email = Some("m@x.com".into()));
    t.w.metadata.grant(m, t.w.project, PlatformRole::Admin, &["plain"]);
    let db = t.w.db;
    // R17: an Admin grant keeps the roles an override names.
    t.w.metadata.race_once(move |md| {
        md.narrow(m, db, OverrideRow { platform_role: Some(PlatformRole::Developer), pg_roles: Some(vec!["boss".into()]) });
    });
    let err = error_of(
        t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "m@x.com", "admin", &["boss"]))).await,
        422,
    )
    .await;
    assert_eq!(err["error"]["database_id"], t.w.db.to_string(), "{err}");
    assert_eq!(t.grant_of(m).unwrap().pg_roles, vec!["plain".to_string()], "nothing written");
}

#[tokio::test]
async fn override_put_checks_again_when_the_grant_changes_in_between() {
    let t = mw(Mode::SelfHosted).await;
    t.second_database();
    let m = t.w.metadata.add_member(t.w.tenant, "m", MemberStatus::Active, false);
    t.w.metadata.grant(m, t.w.project, PlatformRole::Admin, &["plain"]);
    let project = t.w.project;
    // Narrowing the Admin (every role, R17) to `boss` is fine; but another request makes the
    // member a Developer holding `boss` (a stand-in for any grant change), which breaks it once
    // the override lands.
    t.w.metadata.race_once(move |md| md.grant(m, project, PlatformRole::Developer, &["plain", "boss"]));
    let o = format!("/v1/projects/{}/grants/{m}/overrides", t.w.project);
    let err = error_of(
        t.send(DEFAULT_SUB, Method::PUT, &o, Some(json!({ "database_id": t.w.db, "pg_roles": ["boss"] }))).await,
        422,
    )
    .await;
    assert_eq!((err["error"]["role"].as_str(), err["error"]["database_id"].as_str()), (Some("boss"), Some(t.w.db.to_string().as_str())), "{err}");
    assert!(t.w.metadata.overrides.lock().unwrap().is_empty(), "nothing written");
}

#[tokio::test]
async fn override_delete_checks_again_when_the_grant_changes_in_between() {
    let t = mw(Mode::SelfHosted).await;
    t.second_database();
    let m = t.w.metadata.add_member(t.w.tenant, "m", MemberStatus::Active, false);
    t.w.metadata.grant(m, t.w.project, PlatformRole::Developer, &["plain"]);
    t.w.metadata.narrow(m, t.w.db, OverrideRow { platform_role: None, pg_roles: Some(vec!["plain".into()]) });
    let project = t.w.project;
    // Another request gives the Developer `boss`: fine while the override keeps only `plain`,
    // but not once the delete takes the override away.
    t.w.metadata.race_once(move |md| md.grant(m, project, PlatformRole::Developer, &["plain", "boss"]));
    let o = format!("/v1/projects/{}/grants/{m}/overrides?database_id={}", t.w.project, t.w.db);
    let err = error_of(t.send(DEFAULT_SUB, Method::DELETE, &o, None).await, 422).await;
    assert_eq!(err["error"]["database_id"], t.w.db.to_string(), "{err}");
    assert_eq!(t.w.metadata.overrides.lock().unwrap().len(), 1, "the override stays");
}

// I4: a second change during the retry is a 409, with nothing written.
#[tokio::test]
async fn a_checked_write_that_keeps_changing_is_a_conflict() {
    let t = mw(Mode::SelfHosted).await;
    let m = t.w.metadata.add_member(t.w.tenant, "m", MemberStatus::Active, false);
    t.w.metadata.grant(m, t.w.project, PlatformRole::Viewer, &[]);
    let project = t.w.project;
    for roles in [&["a"][..], &["b"][..]] {
        let roles: Vec<&'static str> = roles.to_vec();
        t.w.metadata.race_once(move |md| md.grant(m, project, PlatformRole::Viewer, &roles));
    }
    let grant = format!("/v1/projects/{}/grants/{m}", t.w.project);
    let err = error_of(
        t.send(DEFAULT_SUB, Method::PUT, &grant, Some(json!({ "platform_role": "developer", "pg_roles": [] }))).await,
        409,
    )
    .await;
    assert_eq!(code_of(&err), "conflict");
    assert_eq!(t.grant_of(m).unwrap().platform_role, PlatformRole::Viewer);
}

// M16: a refusal names the branch whose roles broke it. A role can be a plain group on main and
// reach a LOGIN role on a branch; the picker reads each database's main, so the 422 is what
// tells it which branch (and database and chain) to show.
#[tokio::test]
async fn grant_refusal_names_the_non_main_branch_it_was_found_on() {
    let t = mw(Mode::SelfHosted).await;
    let main_ep = t.main_endpoint();
    let dev_ep = t.w.metadata.branch(t.w.dev).endpoint_id.unwrap();
    for ep in [&main_ep, &dev_ep] {
        t.w.metadata.add_group_role(ep, "tidy", &[]);
    }
    // `reports` is a plain group on main, but on dev it is a member of a LOGIN role.
    t.w.metadata.add_group_role(&main_ep, "reports", &[]);
    t.w.metadata.add_role(&dev_ep, "svc", RoleOrigin::Sql, false, false);
    t.w.metadata.add_group_role(&dev_ep, "reports", &["svc"]);
    let m = t.w.metadata.add_member(t.w.tenant, "m", MemberStatus::Active, false);
    let grant = format!("/v1/projects/{}/grants/{m}", t.w.project);
    let put = |roles: &[&str]| {
        t.send(DEFAULT_SUB, Method::PUT, &grant, Some(json!({ "platform_role": "developer", "pg_roles": roles })))
    };
    let err = error_of(put(&["tidy", "reports"]).await, 422).await;
    let dev_name = t.w.metadata.branch(t.w.dev).name;
    assert_eq!(
        (err["error"]["role"].as_str(), err["error"]["branch"].as_str(), err["error"]["branch_main"].as_bool()),
        (Some("reports"), Some(dev_name.as_str()), Some(false)),
        "{err}"
    );
    assert_eq!(err["error"]["chain"], json!(["reports", "svc"]));
    assert_eq!(err["error"]["branch_id"], t.w.dev.as_uuid().to_string());
    assert_eq!(err["error"]["database_id"], t.w.db.to_string());
    // A refusal on main names main.
    t.w.metadata.add_role(&main_ep, "svc2", RoleOrigin::Sql, false, false);
    t.w.metadata.add_group_role(&main_ep, "mainbad", &["svc2"]);
    let err = error_of(put(&["mainbad"]).await, 422).await;
    assert_eq!((err["error"]["branch_main"].as_bool(), err["error"]["branch"].as_str()), (Some(true), Some("main")), "{err}");
}

// I2 (R30 as changed): a role that holds ADMIN on another role, or whose chain does, is refused
// below Admin at grant time, naming the chain; a membership without the SET option is no reach
// but still counts in the chain.
#[tokio::test]
async fn grant_of_a_chain_holding_admin_or_inheriting_the_owner_is_refused() {
    let t = mw(Mode::SelfHosted).await;
    t.second_database();
    let m = t.w.metadata.add_member(t.w.tenant, "m", MemberStatus::Active, false);
    let grant = format!("/v1/projects/{}/grants/{m}", t.w.project);
    let put = |role: &str| {
        t.send(DEFAULT_SUB, Method::PUT, &grant, Some(json!({ "platform_role": "developer", "pg_roles": [role] })))
    };
    let err = error_of(put("lead").await, 422).await;
    assert_eq!(err["error"]["chain"], json!(["lead"]), "{err}");
    assert!(message_of(&err).contains("ADMIN"), "{err}");
    // Why, for clients to put in their own words; a one-role chain reaches nothing else.
    assert_eq!(err["error"]["why"], "admin", "{err}");
    assert!(!message_of(&err).contains("through"), "{err}");
    let err = error_of(put("sneaky").await, 422).await;
    assert_eq!(err["error"]["chain"], json!(["sneaky", "postgres"]), "{err}");
    assert_eq!(err["error"]["why"], "owner_level", "{err}");
    assert_eq!(put("plain").await.status(), 200);
    // Admin may hold them.
    let resp = t.send(DEFAULT_SUB, Method::PUT, &grant, Some(json!({ "platform_role": "admin", "pg_roles": ["lead", "sneaky"] }))).await;
    assert_eq!(resp.status(), 200);
}

// I1: a grant put re-checks the member's overrides: the effective set on each database must
// still pass, named by the database.
#[tokio::test]
async fn grant_put_rechecks_existing_overrides() {
    let t = mw(Mode::SelfHosted).await;
    let db2 = t.second_database();
    let m = t.w.metadata.add_member(t.w.tenant, "m", MemberStatus::Active, false);
    let grant = format!("/v1/projects/{}/grants/{m}", t.w.project);
    let put = |role: &str, roles: &[&str]| {
        t.send(DEFAULT_SUB, Method::PUT, &grant, Some(json!({ "platform_role": role, "pg_roles": roles })))
    };
    assert_eq!(put("developer", &["plain"]).await.status(), 200);
    let o = format!("{grant}/overrides");
    // An override at Developer naming `boss` (the API can't write it today; written directly,
    // as if `boss` changed since).
    t.w.metadata.narrow(m, t.w.db, OverrideRow { platform_role: Some(PlatformRole::Developer), pg_roles: Some(vec!["boss".into()]) });
    let events = t.actions().len();
    // Admin holds every role (R17), but on web1 the override leaves Developer with `boss`.
    let err = error_of(put("admin", &[]).await, 422).await;
    assert_eq!(err["error"]["database_id"], t.w.db.to_string(), "{err}");
    assert_eq!(err["error"]["role"], "boss");
    assert!(message_of(&err).contains("web1"), "{err}");
    assert_eq!(t.grant_of(m).unwrap().pg_roles, vec!["plain".to_string()], "nothing written");
    assert_eq!(t.actions().len(), events);
    // The same through an invite of the member.
    t.w.metadata.set_member(t.w.tenant, "m", |r| r.email = Some("m@x.com".into()));
    let err = error_of(
        t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "m@x.com", "admin", &["boss"]))).await,
        422,
    )
    .await;
    assert_eq!(err["error"]["database_id"], t.w.db.to_string());
    assert_eq!(err["error"]["project_id"], t.w.project.to_string(), "{err}");
    // A narrowing that removes `boss` makes it fine: the effective set is intersected.
    let resp = t
        .send(DEFAULT_SUB, Method::PUT, &o, Some(json!({ "database_id": t.w.db, "platform_role": "developer", "pg_roles": ["plain"] })))
        .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(put("admin", &["plain", "boss", "login"]).await.status(), 200);
    // Dropping web1's narrowing leaves the Admin grant there: fine.
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &format!("{o}?database_id={}", t.w.db), None).await.status(), 204);
    // On web2 an override down to Viewer keeping `login` (R17: an Admin grant keeps the roles
    // an override names): refused, named web2.
    let err = error_of(
        t.send(DEFAULT_SUB, Method::PUT, &o, Some(json!({ "database_id": db2, "platform_role": "viewer", "pg_roles": ["login"] }))).await,
        422,
    )
    .await;
    assert_eq!(err["error"]["database_id"], db2.to_string());
    assert_eq!(err["error"]["role"], "login");
}

// I2 (R29): a removed member comes back only as `invited`, with a notice (they have an
// account); the ban stays until they accept, and the event names the inviter.
#[tokio::test]
async fn removed_member_is_reinvited_with_a_notice() {
    let t = mw(Mode::SelfHosted).await;
    let z = t.identity.with_user(&Pool::Members, "rae@x.com", "Secret-pass1");
    let rae = t.w.metadata.add_member(t.w.tenant, &z, MemberStatus::Active, false);
    t.w.metadata.set_member(t.w.tenant, &z, |m| m.email = Some("rae@x.com".into()));
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/members/{rae}"), None).await.status(), 204);
    assert!(t.identity.user(&Pool::Members, &z).unwrap().banned);
    t.w.metadata.add_granted(t.w.tenant, "padmin", t.w.project, PlatformRole::Admin, &[]);
    let padmin = t.w.metadata.member(t.w.tenant, "padmin").unwrap().id;
    let body = json!({ "email": "rae@x.com", "platform_role": "viewer", "pg_roles": [] });
    let resp = t.send("padmin", Method::POST, &format!("/v1/projects/{}/invites", t.w.project), Some(body)).await;
    assert_eq!(resp.status(), 201);
    let body = body_of(resp).await;
    assert_eq!((body["outcome"].as_str(), body["member"]["status"].as_str()), (Some("invited"), Some("invited")));
    assert_eq!(body["member"]["id"], rae.to_string(), "the same member row");
    // I1: the invite never lifts the ban; accept-invite does (m11).
    assert!(t.identity.user(&Pool::Members, &z).unwrap().banned, "still banned until they accept");
    // They have an account: a notice to sign in and accept, never a code.
    assert!(t.identity.codes_sent().is_empty());
    let notices = t.identity.notices_sent();
    assert_eq!(notices.len(), 1);
    assert!(notices[0].link.starts_with("http://studio.test/accept-invite?t="), "{}", notices[0].link);
    let event = t.w.metadata.events.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        (event.action.as_str(), event.actor_member_id, event.project_id),
        ("invite_created", Some(padmin), Some(t.w.project.as_uuid()))
    );
    // Revoking it never deletes an account with a password.
    let invite = body["member"]["invite"]["id"].as_str().unwrap().to_string();
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/invites/{invite}"), None).await.status(), 204);
    assert!(t.identity.user(&Pool::Members, &z).unwrap().banned);
}

// M4: when the code can't be sent the invite is revoked at once, and the log says why.
#[tokio::test]
async fn failed_invite_delivery_is_revoked_with_its_reason() {
    let t = mw(Mode::SelfHosted).await;
    t.identity.fail(CODES_ROUTE, 503, "internal", 1);
    let resp = t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "fail@x.com", "viewer", &[]))).await;
    assert_eq!(resp.status(), 503);
    let events = t.w.metadata.events.lock().unwrap().clone();
    let revoked = events.iter().find(|e| e.action == "invite_revoked").expect("a compensating revoke");
    assert_eq!(revoked.after.as_ref().unwrap()["reason"], "delivery_failed");
}

// I2 (R29): an account nobody established (no password, or an unverified email) is invited by
// mail with an invite code, which only its mailbox can read.
#[tokio::test]
async fn unestablished_account_gets_an_invite_code() {
    let t = mw(Mode::SelfHosted).await;
    let nopw = t.identity.with_invited_user(&Pool::Members, "nopw@x.com");
    let pre = t.identity.with_unverified_user(&Pool::Members, "pre@x.com", "Secret-pass1");
    for (email, z) in [("nopw@x.com", &nopw), ("pre@x.com", &pre)] {
        let resp = t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, email, "viewer", &[]))).await;
        assert_eq!(resp.status(), 201, "{email}");
        let body = body_of(resp).await;
        assert_eq!(body["outcome"], "invited", "{email}");
        assert_eq!(t.w.metadata.member(t.w.tenant, z).unwrap().status, MemberStatus::Invited);
        assert!(t.identity.codes_sent().iter().any(|c| &c.user_id == z), "{email}: an invite code is mailed");
    }
    assert!(t.identity.notices_sent().is_empty());
    assert!(!t.identity.calls().iter().any(|c| c == "POST /internal/v1/pools/members/users"), "no second user");
}

// I3: an existing member's invite writes all its grants or none.
#[tokio::test]
async fn existing_member_invite_is_all_or_nothing() {
    let t = mw(Mode::SelfHosted).await;
    t.second_database();
    let other = t.w.metadata.create_project(t.w.tenant, "Other").await.unwrap().id;
    let m = t.w.metadata.add_granted(t.w.tenant, "m", t.w.project, PlatformRole::Admin, &["plain"]);
    t.w.metadata.set_member(t.w.tenant, "m", |r| r.email = Some("m@x.com".into()));
    // Written directly (the API can't today), as if `boss` changed since.
    t.w.metadata.narrow(m, t.w.db, OverrideRow { platform_role: Some(PlatformRole::Developer), pg_roles: Some(vec!["boss".into()]) });
    t.clear_enqueued();
    // The second grant is refused (web1's override would leave Developer with `boss`; R17: an
    // Admin grant keeps the roles an override names).
    let body = json!({ "email": "m@x.com", "grants": [
        { "project_id": other, "platform_role": "viewer", "pg_roles": [] },
        { "project_id": t.w.project, "platform_role": "admin", "pg_roles": ["boss"] },
    ] });
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(body)).await, 422).await;
    assert_eq!(err["error"]["database_id"], t.w.db.to_string());
    assert!(t.w.metadata.grants.lock().unwrap().get(&(m, other.as_uuid())).is_none(), "nothing written");
    assert!(t.enqueued().is_empty());
    // Both fine: both written, one event each, both projects reconciled.
    let body = json!({ "email": "m@x.com", "grants": [
        { "project_id": other, "platform_role": "viewer", "pg_roles": [] },
        { "project_id": t.w.project, "platform_role": "developer", "pg_roles": ["plain", "group2"] },
    ] });
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(body)).await.status(), 200);
    assert!(t.w.metadata.grants.lock().unwrap().get(&(m, other.as_uuid())).is_some());
    assert_eq!(t.grant_of(m).unwrap().pg_roles, vec!["group2".to_string(), "plain".to_string()]);
    assert_eq!(t.actions().iter().filter(|a| *a == "grant_put").count(), 2);
}

// M6: a write that changes nothing enqueues no reconcile.
#[tokio::test]
async fn no_op_writes_enqueue_nothing() {
    let t = mw(Mode::SelfHosted).await;
    let m = t.w.metadata.add_member(t.w.tenant, "m", MemberStatus::Active, false);
    t.w.metadata.set_member(t.w.tenant, "m", |r| r.email = Some("m@x.com".into()));
    let grant = format!("/v1/projects/{}/grants/{m}", t.w.project);
    let body = json!({ "platform_role": "developer", "pg_roles": ["b", "a"] });
    t.send(DEFAULT_SUB, Method::PUT, &grant, Some(body.clone())).await;
    let o = json!({ "database_id": t.w.db, "platform_role": "viewer" });
    t.send(DEFAULT_SUB, Method::PUT, &format!("{grant}/overrides"), Some(o.clone())).await;
    t.clear_enqueued();
    let events = t.actions().len();
    for (method, path, body) in [
        (Method::PUT, grant.clone(), Some(json!({ "platform_role": "developer", "pg_roles": ["a", "b", "a"] }))),
        (Method::PUT, format!("{grant}/overrides"), Some(o)),
        (Method::PATCH, format!("/v1/members/{m}"), Some(json!({ "tenant_admin": false }))),
        (Method::POST, "/v1/invites".to_string(), Some(invite_body(&t.w, "m@x.com", "developer", &["a", "b"]))),
    ] {
        let resp = t.send(DEFAULT_SUB, method.clone(), &path, body).await;
        assert!(resp.status().is_success(), "{method} {path}: {}", resp.status());
        assert!(t.enqueued().is_empty(), "{method} {path}");
    }
    assert_eq!(t.actions().len(), events, "no event either");
}

// M5: removal reconciles the projects a member holds member roles in, grants or not.
#[tokio::test]
async fn remove_member_reconciles_branches_it_has_roles_on() {
    let t = mw(Mode::SelfHosted).await;
    let m = t.w.metadata.add_member(t.w.tenant, "m", MemberStatus::Active, false);
    t.w.metadata.member_branch_roles.lock().unwrap().push((m, t.w.dev));
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/members/{m}"), None).await.status(), 204);
    assert_eq!(t.enqueued(), t.branches());
}

// M3: a project's log lists the invites and removals whose grants name it.
#[tokio::test]
async fn project_access_events_list_invites_and_removals() {
    let t = mw(Mode::SelfHosted).await;
    let other = t.w.metadata.create_project(t.w.tenant, "Other").await.unwrap().id;
    let body = json!({ "email": "two@x.com", "grants": [
        { "project_id": other, "platform_role": "viewer", "pg_roles": [] },
        { "project_id": t.w.project, "platform_role": "viewer", "pg_roles": [] },
    ] });
    let resp = t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(body)).await;
    assert_eq!(resp.status(), 201);
    let member = body_of(resp).await["member"]["id"].as_str().unwrap().to_string();
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/members/{member}"), None).await.status(), 204);
    let page = body_of(t.send(DEFAULT_SUB, Method::GET, &format!("/v1/projects/{}/access-events", t.w.project), None).await).await;
    let actions: Vec<&str> = page["events"].as_array().unwrap().iter().map(|e| e["action"].as_str().unwrap()).collect();
    assert_eq!(actions, vec!["member_removed", "invite_created"]);
}

// The grant invariant (T7 review, second finding in grant/override validation): after any
// sequence of grant puts and deletes, override puts and deletes and invites, every member's
// effective grant on every database passes the role check for its effective platform role.
// A write that would break it is refused with 422 naming the database, and writes nothing.
mod grant_invariant {
    use super::*;
    use proptest::prelude::*;

    #[derive(Clone, Debug)]
    enum Op {
        PutGrant { member: usize, role: PlatformRole, roles: Vec<&'static str> },
        DeleteGrant { member: usize },
        PutOverride { member: usize, first: bool, role: Option<PlatformRole>, roles: Option<Vec<&'static str>> },
        DeleteOverride { member: usize, first: bool },
        Invite { member: usize, role: PlatformRole, roles: Vec<&'static str> },
    }

    fn role() -> impl Strategy<Value = PlatformRole> {
        prop_oneof![Just(PlatformRole::Viewer), Just(PlatformRole::Developer), Just(PlatformRole::Admin)]
    }

    fn roles() -> impl Strategy<Value = Vec<&'static str>> {
        proptest::sample::subsequence(POOL.to_vec(), 0..=POOL.len())
    }

    fn op() -> impl Strategy<Value = Op> {
        let member = 0..2usize;
        prop_oneof![
            (member.clone(), role(), roles()).prop_map(|(member, role, roles)| Op::PutGrant { member, role, roles }),
            member.clone().prop_map(|member| Op::DeleteGrant { member }),
            (member.clone(), any::<bool>(), proptest::option::of(role()), proptest::option::of(roles()))
                .prop_map(|(member, first, role, roles)| Op::PutOverride { member, first, role, roles }),
            (member.clone(), any::<bool>()).prop_map(|(member, first)| Op::DeleteOverride { member, first }),
            (member, role(), roles()).prop_map(|(member, role, roles)| Op::Invite { member, role, roles }),
        ]
    }

    /// Every (member, database) whose effective grant holds, below Admin, a role the pool says
    /// is refused there.
    fn violations(t: &Mw, members: &[uuid::Uuid], db2: DatabaseId) -> Vec<String> {
        let mut out = vec![];
        for m in members {
            let Some(g) = t.grant_of(*m) else { continue };
            for (db, first) in [(t.w.db, true), (db2, false)] {
                let o = t.w.metadata.overrides.lock().unwrap().get(&(*m, db.as_uuid())).cloned();
                let role = o.as_ref().and_then(|o| o.platform_role).map_or(g.platform_role, |r| r.min(g.platform_role));
                let held: Vec<&String> = match o.as_ref().and_then(|o| o.pg_roles.as_ref()) {
                    Some(narrow) => g.pg_roles.iter().filter(|r| narrow.contains(r)).collect(),
                    None => g.pg_roles.iter().collect(),
                };
                if role < PlatformRole::Admin {
                    for r in held.into_iter().filter(|r| pool_bad(first, r)) {
                        out.push(format!("member {m} on {}: {role:?} holds {r}", if first { "web1" } else { "web2" }));
                    }
                }
            }
        }
        out
    }

    type Snapshot = (Vec<(uuid::Uuid, uuid::Uuid, GrantRow)>, Vec<(uuid::Uuid, uuid::Uuid, OverrideRow)>, usize);

    fn snapshot(t: &Mw) -> Snapshot {
        let mut grants: Vec<_> = t.w.metadata.grants.lock().unwrap().iter().map(|((m, p), g)| (*m, *p, g.clone())).collect();
        grants.sort_by_key(|(m, p, _)| (*m, *p));
        let mut overrides: Vec<_> =
            t.w.metadata.overrides.lock().unwrap().iter().map(|((m, d), o)| (*m, *d, o.clone())).collect();
        overrides.sort_by_key(|(m, d, _)| (*m, *d));
        (grants, overrides, t.w.metadata.events.lock().unwrap().len())
    }

    async fn run(ops: Vec<Op>) -> Result<(), TestCaseError> {
        let t = mw(Mode::SelfHosted).await;
        let db2 = t.second_database();
        let members: Vec<uuid::Uuid> = (0..2)
            .map(|i| {
                let sub = format!("m{i}");
                let id = t.w.metadata.add_member(t.w.tenant, &sub, MemberStatus::Active, false);
                t.w.metadata.set_member(t.w.tenant, &sub, |r| r.email = Some(format!("m{i}@x.com")));
                id
            })
            .collect();
        for (step, op) in ops.into_iter().enumerate() {
            let before = snapshot(&t);
            let (method, path, body) = match &op {
                Op::PutGrant { member, role, roles } => (
                    Method::PUT,
                    format!("/v1/projects/{}/grants/{}", t.w.project, members[*member]),
                    Some(json!({ "platform_role": role, "pg_roles": roles })),
                ),
                Op::DeleteGrant { member } => (Method::DELETE, format!("/v1/projects/{}/grants/{}", t.w.project, members[*member]), None),
                Op::PutOverride { member, first, role, roles } => (
                    Method::PUT,
                    format!("/v1/projects/{}/grants/{}/overrides", t.w.project, members[*member]),
                    Some(json!({ "database_id": if *first { t.w.db } else { db2 }, "platform_role": role, "pg_roles": roles })),
                ),
                Op::DeleteOverride { member, first } => (
                    Method::DELETE,
                    format!(
                        "/v1/projects/{}/grants/{}/overrides?database_id={}",
                        t.w.project,
                        members[*member],
                        if *first { t.w.db } else { db2 }
                    ),
                    None,
                ),
                Op::Invite { member, role, roles } => {
                    (Method::POST, "/v1/invites".to_string(), Some(invite_body(&t.w, &format!("m{member}@x.com"), role.as_str(), roles)))
                }
            };
            let resp = t.send(DEFAULT_SUB, method, &path, body).await;
            let status = resp.status().as_u16();
            let answer: Value = resp.json().await.unwrap_or(Value::Null);
            prop_assert!(status < 500, "step {step} {op:?}: {status} {answer}");
            if status >= 400 {
                prop_assert_eq!(&snapshot(&t), &before, "step {} {:?}: a refusal wrote", step, op);
                if !answer["error"]["chain"].is_null() {
                    prop_assert!(!answer["error"]["database_id"].is_null(), "step {step} {op:?}: {answer}");
                }
            }
            let broken = violations(&t, &members, db2);
            prop_assert!(broken.is_empty(), "step {step} {op:?} -> {status}: {broken:?}");
        }
        Ok(())
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]
        #[test]
        fn effective_grants_always_pass_the_role_check(ops in proptest::collection::vec(op(), 1..12)) {
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
            rt.block_on(run(ops))?;
        }
    }
}

#[tokio::test]
async fn resend_invite_sends_a_new_code() {
    let t = mw(Mode::SelfHosted).await;
    let body = body_of(
        t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "jay@x.com", "viewer", &[]))).await,
    )
    .await;
    let invite = body["member"]["invite"]["id"].as_str().unwrap().to_string();
    let first = t.identity.codes_sent()[0].code.clone();
    let resp = t.send(DEFAULT_SUB, Method::POST, &format!("/v1/invites/{invite}/resend"), None).await;
    assert_eq!(resp.status(), 200);
    let resent = body_of(resp).await;
    assert_eq!(resent["id"], invite.as_str());
    let sent = t.identity.codes_sent();
    assert_eq!(sent.len(), 2);
    assert_ne!(sent[1].code, first);
    assert_eq!(t.actions(), vec!["invite_created", "invite_resent"]);
    // A revoked invite can't be resent.
    t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/invites/{invite}"), None).await;
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, &format!("/v1/invites/{invite}/resend"), None).await, 409).await;
    assert_eq!(code_of(&err), "invite_closed");
}

// R7: grants, overrides and invites refuse reserved role names.
#[tokio::test]
async fn grant_with_reserved_role_name_is_refused() {
    let t = mw(Mode::SelfHosted).await;
    t.w.metadata.databases.lock().unwrap().get_mut(&t.w.db).unwrap().owner_role = Some("web_owner".into());
    let m = t.w.metadata.add_granted(t.w.tenant, "m", t.w.project, PlatformRole::Admin, &["app"]);
    let grant = format!("/v1/projects/{}/grants/{m}", t.w.project);
    for name in ["owner", "OWNER", "web_owner", "postgres", "cb_m_0123", "CB_M_x", "chelabase_admin", "Chelabase_x", "pg_read_all_data", "cloud_admin", "neon_superuser", "authenticator", ""] {
        for role in ["admin", "developer"] {
            let err = error_of(
                t.send(DEFAULT_SUB, Method::PUT, &grant, Some(json!({ "platform_role": role, "pg_roles": [name] }))).await,
                422,
            )
            .await;
            assert_eq!(code_of(&err), "invalid_request", "{role} {name:?}");
        }
        let err = error_of(
            t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "kai@x.com", "viewer", &[name]))).await,
            422,
        )
        .await;
        assert_eq!(code_of(&err), "invalid_request", "invite {name:?}");
        // The refusal names the invite's project, and the database whose owner the name is.
        assert_eq!(err["error"]["project_id"], t.w.project.to_string(), "invite {name:?}: {err}");
        if name == "web_owner" {
            assert_eq!(err["error"]["database_id"], t.w.db.to_string(), "{err}");
        }
        let err = error_of(
            t.send(DEFAULT_SUB, Method::PUT, &format!("{grant}/overrides"), Some(json!({ "database_id": t.w.db, "pg_roles": [name] })))
                .await,
            422,
        )
        .await;
        assert_eq!(code_of(&err), "invalid_request", "override {name:?}");
    }
    assert!(t.identity.user_by_email(&Pool::Members, "kai@x.com").is_none(), "a refused invite makes no identity user");
    assert_eq!(t.grant_of(m).unwrap().pg_roles, vec!["app".to_string()]);
    // The data API's own roles stay grantable (R10).
    for name in ["anon", "authenticated"] {
        let resp = t.send(DEFAULT_SUB, Method::PUT, &grant, Some(json!({ "platform_role": "developer", "pg_roles": [name] }))).await;
        assert_eq!(resp.status(), 200, "{name}");
    }
}

// R16 and R21–R23: below Admin, a granted role and its whole chain must be plain NOLOGIN groups.
#[tokio::test]
async fn grant_whose_chain_is_not_plain_groups_is_refused_below_admin() {
    let t = mw(Mode::SelfHosted).await;
    let ep = t.main_endpoint();
    let md = &t.w.metadata;
    md.add_group_role(&ep, "reader", &[]);
    md.add_group_role(&ep, "billing_admin", &["billing"]);
    md.add_group_role(&ep, "billing", &["postgres"]);
    md.add_group_role(&ep, "creator", &[]);
    md.roles.lock().unwrap().get_mut(&ep).unwrap().last_mut().unwrap().level = RoleLevel::OwnerLevel;
    md.add_group_role(&ep, "rls_free", &[]);
    md.roles.lock().unwrap().get_mut(&ep).unwrap().last_mut().unwrap().attributes.bypass_rls = true;
    md.add_role(&ep, "app_login", RoleOrigin::Sql, false, false);
    md.add_group_role(&ep, "via_login", &["app_login"]);
    md.add_group_role(&ep, "via_reserved", &["authenticator"]);
    // On dev only: the check covers every branch of the project.
    let dev_ep = md.branch(t.w.dev).endpoint_id.unwrap();
    md.add_group_role(&dev_ep, "dev_only", &["neon_superuser"]);
    let m = md.add_member(t.w.tenant, "m", MemberStatus::Active, false);
    let grant = format!("/v1/projects/{}/grants/{m}", t.w.project);
    let put = |role: &str, name: &str| {
        t.send(DEFAULT_SUB, Method::PUT, &grant, Some(json!({ "platform_role": role, "pg_roles": [name] })))
    };
    for (name, chain) in [
        ("billing_admin", vec!["billing_admin", "billing", "postgres"]),
        ("creator", vec!["creator"]),
        ("rls_free", vec!["rls_free"]),
        ("app_login", vec!["app_login"]),
        ("via_login", vec!["via_login", "app_login"]),
        ("via_reserved", vec!["via_reserved", "authenticator"]),
        ("dev_only", vec!["dev_only", "neon_superuser"]),
    ] {
        for role in ["developer", "viewer"] {
            let err = error_of(put(role, name).await, 422).await;
            assert_eq!(code_of(&err), "invalid_request", "{role} {name}");
            assert_eq!(err["error"]["role"], name, "{err}");
            assert_eq!(err["error"]["chain"], json!(chain), "{err}");
            assert!(message_of(&err).contains(name), "{err}");
        }
        // Admin may hold it (R16: owner-level and LOGIN roles are Admin-only).
        assert_eq!(put("admin", name).await.status(), 200, "admin {name}");
        // An override may not keep it below Admin (R17: an Admin grant keeps what it names).
        let err = error_of(
            t.send(DEFAULT_SUB, Method::PUT, &format!("{grant}/overrides"), Some(json!({ "database_id": t.w.db, "platform_role": "developer", "pg_roles": [name] })))
                .await,
            422,
        )
        .await;
        assert_eq!(err["error"]["chain"], json!(chain), "override {name}");
        // The same goes for invites.
        let err = error_of(
            t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "lee@x.com", "developer", &[name]))).await,
            422,
        )
        .await;
        assert_eq!(err["error"]["role"], name);
    }
    // Plain groups, and roles no branch has (yet), are fine.
    assert_eq!(put("developer", "reader").await.status(), 200);
    assert_eq!(put("viewer", "not_there_yet").await.status(), 200);
}

#[tokio::test]
async fn access_events_page_newest_first() {
    let t = mw(Mode::SelfHosted).await;
    let m = t.w.metadata.add_member(t.w.tenant, "m", MemberStatus::Active, false);
    let grant = format!("/v1/projects/{}/grants/{m}", t.w.project);
    for role in ["viewer", "developer", "admin"] {
        t.send(DEFAULT_SUB, Method::PUT, &grant, Some(json!({ "platform_role": role, "pg_roles": [] }))).await;
    }
    t.send(DEFAULT_SUB, Method::PATCH, "/v1/instance/settings", Some(json!({ "open_signup": true }))).await;
    let page = body_of(t.send(DEFAULT_SUB, Method::GET, "/v1/access-events?limit=2", None).await).await;
    let events = page["events"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!((events[0]["action"].as_str(), events[1]["action"].as_str()), (Some("open_signup_set"), Some("grant_put")));
    let next = page["next_before"].as_i64().unwrap();
    let rest = body_of(t.send(DEFAULT_SUB, Method::GET, &format!("/v1/access-events?before={next}&limit=10"), None).await).await;
    assert_eq!(rest["events"].as_array().unwrap().len(), 2);
    assert!(rest["next_before"].is_null());
    // The project's log has only its own events.
    let project = body_of(t.send(DEFAULT_SUB, Method::GET, &format!("/v1/projects/{}/access-events", t.w.project), None).await).await;
    assert_eq!(project["events"].as_array().unwrap().len(), 3);
    assert_eq!(project["events"][0]["target_member_id"], m.to_string());
    for bad in ["limit=0", "limit=201", "limit=x", "before=y"] {
        let err = error_of(t.send(DEFAULT_SUB, Method::GET, &format!("/v1/access-events?{bad}"), None).await, 400).await;
        assert_eq!(code_of(&err), "invalid_request", "{bad}");
    }
    // Only a tenant admin reads the tenant's log.
    t.w.metadata.add_granted(t.w.tenant, "padmin", t.w.project, PlatformRole::Admin, &[]);
    let err = error_of(t.send("padmin", Method::GET, "/v1/access-events", None).await, 403).await;
    assert_eq!(err["error"]["required"], "tenant admin");
    assert_eq!(t.send("padmin", Method::GET, &format!("/v1/projects/{}/access-events", t.w.project), None).await.status(), 200);
}

// ---- T12x: API additions for the Members UI ------------------------------------------------

/// The invite link's `t` parameter: the token of the template identity was given.
fn token_of(template: &str) -> String {
    template.split("&t=").nth(1).expect("the template carries t=").to_string()
}

/// SHA-256 of a token, as stored.
fn hash_of(token: &str) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(token.as_bytes()).into()
}

fn stored_hash(t: &Mw, invite: &str) -> [u8; 32] {
    let id: uuid::Uuid = invite.parse().unwrap();
    t.w.metadata.invites.lock().unwrap().iter().find(|i| i.id == id).unwrap().token_hash
}

#[tokio::test]
async fn list_members_reports_has_access_and_created_at() {
    let t = mw(Mode::SelfHosted).await;
    let md = &t.w.metadata;
    let bare = md.add_member(t.w.tenant, "bare", MemberStatus::Active, false);
    let granted = md.add_granted(t.w.tenant, "granted", t.w.project, PlatformRole::Viewer, &[]);
    let boss = md.add_member(t.w.tenant, "boss", MemberStatus::Active, true);
    // An override alone is not access.
    md.narrow(bare, t.w.db, OverrideRow { platform_role: Some(PlatformRole::Viewer), pg_roles: None });
    // An invited member holds its (inert) grants: that counts.
    let invited = body_of(
        t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "inv@x.com", "viewer", &[]))).await,
    )
    .await;
    let bare_invite = body_of(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(json!({ "email": "none@x.com", "grants": [] }))).await).await;
    assert_eq!(invited["member"]["has_access"], true);
    assert!(invited["member"]["created_at"].as_str().unwrap().parse::<chrono::DateTime<chrono::Utc>>().is_ok());
    assert_eq!(bare_invite["member"]["has_access"], false);

    let list = body_of(t.send(DEFAULT_SUB, Method::GET, "/v1/members", None).await).await;
    let find = |id: String| list.as_array().unwrap().iter().find(|m| m["id"] == id.as_str()).cloned().unwrap();
    for (id, want) in [
        (bare, false),
        (granted, true),
        (boss, true),
        (uuid::Uuid::parse_str(invited["member"]["id"].as_str().unwrap()).unwrap(), true),
        (uuid::Uuid::parse_str(bare_invite["member"]["id"].as_str().unwrap()).unwrap(), false),
    ] {
        let m = find(id.to_string());
        assert_eq!(m["has_access"], want, "{m}");
        assert!(m["created_at"].as_str().unwrap().parse::<chrono::DateTime<chrono::Utc>>().is_ok(), "{m}");
    }
    // `created_at` is the row's own: the same in the write's answer and the list, and later
    // invites are later.
    let at = |m: &Value| m["created_at"].as_str().unwrap().parse::<chrono::DateTime<chrono::Utc>>().unwrap();
    let listed_invited = find(invited["member"]["id"].as_str().unwrap().to_string());
    let listed_bare_invite = find(bare_invite["member"]["id"].as_str().unwrap().to_string());
    assert_eq!(at(&listed_invited), at(&invited["member"]));
    assert_eq!(at(&listed_bare_invite), at(&bare_invite["member"]));
    assert!(at(&invited["member"]) < at(&bare_invite["member"]));
    assert!(at(&find(bare.to_string())) < at(&listed_invited), "seeded members came first");
    // A grant on a soft-deleted project is no access.
    let ghost = md.create_project(t.w.tenant, "Ghost").await.unwrap().id;
    let holder = md.add_granted(t.w.tenant, "holder", ghost, PlatformRole::Viewer, &[]);
    let has = |list: &Value, id: uuid::Uuid| list.as_array().unwrap().iter().find(|m| m["id"] == id.to_string().as_str()).unwrap()["has_access"].clone();
    let list = body_of(t.send(DEFAULT_SUB, Method::GET, "/v1/members", None).await).await;
    assert_eq!(has(&list, holder), true);
    md.projects.lock().unwrap().iter_mut().find(|p| p.id == ghost).unwrap().status = chelabase_metadata::ProjectStatus::Deleted;
    let list = body_of(t.send(DEFAULT_SUB, Method::GET, "/v1/members", None).await).await;
    assert_eq!(has(&list, holder), false);
    // The single-member answers carry them too.
    let updated = body_of(t.send(DEFAULT_SUB, Method::PATCH, &format!("/v1/members/{bare}"), Some(json!({ "tenant_admin": true }))).await).await;
    assert_eq!(updated["has_access"], true);
    assert!(updated["created_at"].is_string());
}

#[tokio::test]
async fn access_events_name_removed_members_and_projects() {
    let t = mw(Mode::SelfHosted).await;
    let md = &t.w.metadata;
    let m = md.add_member(t.w.tenant, "m", MemberStatus::Active, false);
    let grant = format!("/v1/projects/{}/grants/{m}", t.w.project);
    t.send(DEFAULT_SUB, Method::PUT, &grant, Some(json!({ "platform_role": "viewer", "pg_roles": [] }))).await;
    let admin_email = md.member(t.w.tenant, DEFAULT_SUB).unwrap().email;
    let events = |path: String| {
        let t = &t;
        async move { body_of(t.send(DEFAULT_SUB, Method::GET, &path, None).await).await }
    };
    let page = events("/v1/access-events".into()).await;
    let put = &page["events"][0];
    assert_eq!(put["action"], "grant_put");
    assert_eq!(put["actor_email"].as_str(), admin_email.as_deref());
    assert_eq!(put["target_email"], "m@example.test");
    assert_eq!(put["project_name"], "Billing");
    let project_page = events(format!("/v1/projects/{}/access-events", t.w.project)).await;
    assert_eq!(project_page["events"][0]["project_name"], "Billing");
    assert_eq!(project_page["events"][0]["target_email"], "m@example.test");

    // The target is removed (the row is kept, only marked) and the project is deleted (its row
    // stays with its last name): both still resolve.
    md.set_member(t.w.tenant, "m", |m| m.status = MemberStatus::Removed);
    md.projects.lock().unwrap().iter_mut().find(|p| p.id == t.w.project).unwrap().status = chelabase_metadata::ProjectStatus::Deleted;
    let put = &events("/v1/access-events".into()).await["events"][0];
    assert_eq!((put["target_email"].as_str(), put["project_name"].as_str()), (Some("m@example.test"), Some("Billing")));
    // A project row that is gone, and an event with no actor or project, resolve to null.
    md.projects.lock().unwrap().retain(|p| p.id != t.w.project);
    md.log(Change::new(t.w.tenant, None, "open_signup_set", None));
    let page = events("/v1/access-events".into()).await;
    let system = &page["events"][0];
    assert_eq!((system["actor_email"].is_null(), system["target_email"].is_null(), system["project_name"].is_null()), (true, true, true));
    let put = &page["events"][1];
    assert!(put["project_name"].is_null() && put["project_id"].is_string(), "{put}");
    assert_eq!(put["target_email"], "m@example.test");
}

// R32: the invite mail's link carries a Chelabase token (and never the code); only the token's
// SHA-256 is stored.
#[tokio::test]
async fn invite_mail_link_carries_the_token() {
    let t = mw(Mode::SelfHosted).await;
    let body = body_of(
        t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "tok@x.com", "viewer", &[]))).await,
    )
    .await;
    let sub = sub_of(&t, "tok@x.com");
    let link = t.identity.codes_sent()[0].link.clone().unwrap();
    assert!(link.starts_with(&format!("http://studio.test/accept-invite?user={sub}&t=")), "{link}");
    let token = token_of(&link);
    assert_eq!(token.len(), 43, "32 bytes, base64url without padding: {token}");
    assert!(token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'), "{token}");
    assert_eq!(stored_hash(&t, body["member"]["invite"]["id"].as_str().unwrap()), hash_of(&token));
    // Another invite, another token.
    t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "tok2@x.com", "viewer", &[]))).await;
    let other = token_of(t.identity.codes_sent()[1].link.as_deref().unwrap());
    assert_ne!(other, token);
}

#[tokio::test]
async fn resend_rotates_the_token() {
    let t = mw(Mode::SelfHosted).await;
    let body = body_of(
        t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "rot@x.com", "viewer", &[]))).await,
    )
    .await;
    let invite = body["member"]["invite"]["id"].as_str().unwrap().to_string();
    let first = token_of(t.identity.codes_sent()[0].link.as_deref().unwrap());
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &format!("/v1/invites/{invite}/resend"), None).await.status(), 200);
    let second = token_of(t.identity.codes_sent()[1].link.as_deref().unwrap());
    assert_ne!(first, second);
    // Only the new link's token is stored.
    let stored = stored_hash(&t, &invite);
    assert_eq!(stored, hash_of(&second));
    assert_ne!(stored, hash_of(&first));
}

// --- R35: invites by link, and the delivery probe --------------------------------------------

/// `invite_body` with `delivery: "link"`.
fn link_invite_body(w: &MemberWorld, email: &str) -> Value {
    let mut body = invite_body(w, email, "developer", &[]);
    body["delivery"] = json!("link");
    body
}

// With `link`, the identity service hands back the code instead of mailing it; the response
// carries the whole `/accept-invite` link once, and nothing keeps it.
#[tokio::test]
async fn link_invite_returns_the_url_once_and_sends_no_mail() {
    let t = mw(Mode::SelfHosted).await;
    let resp = t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(link_invite_body(&t.w, " Link@X.com "))).await;
    assert_eq!(resp.status(), 201);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let body = body_of(resp).await;
    assert_eq!(body["outcome"], "invited");
    let sub = sub_of(&t, "link@x.com");
    let url = body["invite_url"].as_str().unwrap().to_string();
    let prefix = format!("http://studio.test/accept-invite?user={sub}&code=");
    assert!(url.starts_with(&prefix), "{url}");
    let code = url[prefix.len()..prefix.len() + 6].to_string();
    let token = token_of(&url);
    assert_eq!(token.len(), 43, "{token}");
    // Nothing was mailed.
    assert!(t.identity.codes_sent().is_empty() && t.identity.notices_sent().is_empty());
    // Only the token's hash is stored.
    let invite = body["member"]["invite"]["id"].as_str().unwrap();
    assert_eq!(stored_hash(&t, invite), hash_of(&token));
    assert_eq!(t.actions(), vec!["invite_created"]);
    // Shown once: the member list has neither the link nor the code.
    let list = body_of(t.send(DEFAULT_SUB, Method::GET, "/v1/members", None).await).await.to_string();
    assert!(!list.contains(&code) && !list.contains("invite_url") && !list.contains(&token), "{list}");
    // The default is email, and an email invite has no link.
    let by_mail = body_of(
        t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "mail@x.com", "viewer", &[]))).await,
    )
    .await;
    assert!(by_mail.get("invite_url").is_none(), "{by_mail}");
    assert_eq!(t.identity.codes_sent().len(), 1);
    // A person who needs no code (already a member) gets no link.
    let again = t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(link_invite_body(&t.w, "link@x.com"))).await;
    assert_eq!(again.status(), 200);
    let again = body_of(again).await;
    assert_eq!(again["outcome"], "existing_member");
    assert!(again.get("invite_url").is_none(), "{again}");
    // The project invite takes `delivery` too.
    let mut project = json!({ "email": "proj@x.com", "platform_role": "viewer" });
    project["delivery"] = json!("link");
    let resp = t.send(DEFAULT_SUB, Method::POST, &format!("/v1/projects/{}/invites", t.w.project), Some(project)).await;
    assert_eq!(resp.status(), 201);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    assert!(body_of(resp).await["invite_url"].as_str().unwrap().starts_with("http://studio.test/accept-invite?user="));
    // A delivery that is neither is refused before anything is made.
    let mut bad = link_invite_body(&t.w, "bad@x.com");
    bad["delivery"] = json!("pigeon");
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(bad)).await, 422).await;
    assert_eq!(code_of(&err), "invalid_request");
    assert!(t.identity.user_by_email(&Pool::Members, "bad@x.com").is_none());
}

#[tokio::test]
async fn link_resend_rotates_code_and_token() {
    let t = mw(Mode::SelfHosted).await;
    let body = body_of(
        t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(link_invite_body(&t.w, "relink@x.com"))).await,
    )
    .await;
    let invite = body["member"]["invite"]["id"].as_str().unwrap().to_string();
    let sub = sub_of(&t, "relink@x.com");
    let first_url = body["invite_url"].as_str().unwrap().to_string();
    let path = format!("/v1/invites/{invite}/resend");
    let resp = t.send(DEFAULT_SUB, Method::POST, &path, Some(json!({ "delivery": "link" }))).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let resent = body_of(resp).await;
    let second_url = resent["invite_url"].as_str().unwrap().to_string();
    assert_ne!(first_url, second_url);
    let prefix = format!("http://studio.test/accept-invite?user={sub}&code=");
    assert!(second_url.starts_with(&prefix), "{second_url}");
    assert_ne!(first_url[prefix.len()..prefix.len() + 6], second_url[prefix.len()..prefix.len() + 6], "a new code");
    // Only the new link's token is stored; no mail went out.
    assert_eq!(stored_hash(&t, &invite), hash_of(&token_of(&second_url)));
    assert!(t.identity.codes_sent().is_empty());
    assert_eq!(resent["id"], invite.as_str());
    assert_eq!(t.actions(), vec!["invite_created", "invite_resent"]);
    // Without a body the resend is by email, as before: a mail and no link.
    let resp = t.send(DEFAULT_SUB, Method::POST, &path, None).await;
    assert_eq!(resp.status(), 200);
    assert!(body_of(resp).await.get("invite_url").is_none());
    assert_eq!(t.identity.codes_sent().len(), 1);
    // An unknown delivery is refused.
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, &path, Some(json!({ "delivery": "pigeon" }))).await, 422).await;
    assert_eq!(code_of(&err), "invalid_request");
    // M3: a `null` body is the default too: by email.
    let resp = t.send(DEFAULT_SUB, Method::POST, &path, Some(Value::Null)).await;
    assert_eq!(resp.status(), 200);
    assert!(body_of(resp).await.get("invite_url").is_none());
    assert_eq!(t.identity.codes_sent().len(), 2);
}

// I1: a link carries a code that sets the password, so it is for a user the invite makes or one
// with no password and an unverified email; anyone else is 409 "send by email", before anything
// changes (`invite_return_refused_for_existing_account` has each kind of account).
#[tokio::test]
async fn link_invite_is_for_users_without_a_password() {
    let t = mw(Mode::SelfHosted).await;
    t.identity.set_email_configured(false);
    // A removed member with a password: a link is refused, before anything changes.
    let zid = t.identity.with_user(&Pool::Members, "back@x.com", "Correct-horse-9");
    t.w.metadata.add_member(t.w.tenant, &zid, MemberStatus::Removed, false);
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(link_invite_body(&t.w, "back@x.com"))).await, 409).await;
    assert_eq!(code_of(&err), "link_not_allowed");
    // Whether email delivery works now, so Studio can offer it.
    assert_eq!(err["error"]["email_available"], json!(false), "{err}");
    assert!(!t.identity.calls().iter().any(|c| c.contains("/codes") || c == "POST /internal/v1/pools/members/users"));
    assert_eq!(t.w.metadata.member(t.w.tenant, &zid).unwrap().status, MemberStatus::Removed);
    assert!(t.actions().is_empty());
    t.identity.set_email_configured(true);
    let by_mail = t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "back@x.com", "viewer", &[]))).await;
    assert_eq!(by_mail.status(), 201);
    assert!(body_of(by_mail).await.get("invite_url").is_none());
    // An account that never set a password: a link is fine.
    let fresh = t.identity.with_invited_user(&Pool::Members, "fresh@x.com");
    let resp = t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(link_invite_body(&t.w, "fresh@x.com"))).await;
    assert_eq!(resp.status(), 201);
    assert!(body_of(resp).await["invite_url"].as_str().unwrap().contains(&format!("user={fresh}&code=")));
}

// I1 for resend: the invitee set a password since (outside the invite), so only email resends.
#[tokio::test]
async fn link_resend_refused_after_the_invitee_has_a_password() {
    let t = mw(Mode::SelfHosted).await;
    t.identity.set_email_configured(false);
    let body = body_of(
        t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(link_invite_body(&t.w, "late@x.com"))).await,
    )
    .await;
    let invite = body["member"]["invite"]["id"].as_str().unwrap().to_string();
    let sub = sub_of(&t, "late@x.com");
    let path = format!("/v1/invites/{invite}/resend");
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &path, Some(json!({ "delivery": "link" }))).await.status(), 200);
    // A password since the invite was made.
    t.identity.client().set_password(&Pool::Members, &sub, "Correct-horse-9").await.unwrap();
    let codes = t.identity.calls().iter().filter(|c| c.contains("/codes")).count();
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, &path, Some(json!({ "delivery": "link" }))).await, 409).await;
    assert_eq!(code_of(&err), "link_not_allowed");
    assert_eq!(err["error"]["email_available"], json!(false));
    assert_eq!(t.identity.calls().iter().filter(|c| c.contains("/codes")).count(), codes, "no new code was made");
    // By email: an invite code still, as the email isn't verified (pre-registered).
    t.identity.set_email_configured(true);
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &path, None).await.status(), 200);
    assert_eq!(t.identity.codes_sent().len(), 1);
    // With email configured, the refusal says so.
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, &path, Some(json!({ "delivery": "link" }))).await, 409).await;
    assert_eq!((code_of(&err), &err["error"]["email_available"]), ("link_not_allowed", &json!(true)), "{err}");
}

// Each invite 409 has its own code, so Studio never reads message text.
#[tokio::test]
async fn invite_conflicts_have_their_own_codes() {
    let t = mw(Mode::SelfHosted).await;
    // A sign-up waiting for approval.
    let pending = t.identity.with_unverified_user(&Pool::Members, "wait@x.com", "Secret-pass1");
    t.w.metadata.add_member(t.w.tenant, &pending, MemberStatus::Pending, false);
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "wait@x.com", "viewer", &[]))).await, 409).await;
    assert_eq!(code_of(&err), "signup_pending", "{err}");
    // A revoked invite can't be resent.
    let body = body_of(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "gone@x.com", "viewer", &[]))).await).await;
    let invite = body["member"]["invite"]["id"].as_str().unwrap().to_string();
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/invites/{invite}"), None).await.status(), 204);
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, &format!("/v1/invites/{invite}/resend"), None).await, 409).await;
    assert_eq!(code_of(&err), "invite_closed", "{err}");
}

#[tokio::test]
async fn delivery_reports_smtp_state() {
    let t = mw(Mode::SelfHosted).await;
    let ask = |sub: &'static str| {
        let t = &t;
        async move { body_of(t.send(sub, Method::GET, "/v1/invites/delivery", None).await).await }
    };
    // Any active member may ask (the answer decides the invite dialog).
    t.w.metadata.add_granted(t.w.tenant, "viewer", t.w.project, PlatformRole::Viewer, &[]);
    assert_eq!(ask("viewer").await, json!({ "email_available": true, "reason": null }));
    assert!(t.identity.calls().contains(&"GET /internal/v1/email/status".to_string()));
    // Platform mode makes no members: a stranger is refused like on any tenant route.
    let platform = mw(Mode::Platform).await;
    let err = error_of(platform.send("stranger", Method::GET, "/v1/invites/delivery", None).await, 403).await;
    assert_eq!(code_of(&err), "not_available_in_platform_mode");
    // Without an identity client the answer is "no", not an error.
    let none = mw_without_identity().await;
    assert_eq!(
        body_of(none.send(DEFAULT_SUB, Method::GET, "/v1/invites/delivery", None).await).await,
        json!({ "email_available": false, "reason": "identity_unavailable" })
    );
}

// --- R36: tenant display names ---------------------------------------------------------------

#[tokio::test]
async fn tenant_admin_renames_tenant() {
    let t = mw(Mode::SelfHosted).await;
    t.w.metadata.set_overview(TenantOverview {
        id: t.w.tenant,
        slug: "demo".into(),
        display_name: None,
        projects: vec![],
    });
    let me = |sub: &'static str| {
        let t = &t;
        async move { body_of(t.send(sub, Method::GET, "/v1/me", None).await).await }
    };
    assert_eq!(me(DEFAULT_SUB).await["tenant"]["display_name"], Value::Null);
    let patch = |body: Value| {
        let t = &t;
        async move { t.send(DEFAULT_SUB, Method::PATCH, "/v1/tenant", Some(body)).await }
    };
    // Trimmed; the slug stays.
    let resp = patch(json!({ "display_name": "  Acme Corp  " })).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(body_of(resp).await, json!({ "display_name": "Acme Corp" }));
    let tenant = me(DEFAULT_SUB).await["tenant"].clone();
    assert_eq!((tenant["slug"].as_str(), tenant["display_name"].as_str()), (Some("demo"), Some("Acme Corp")));
    assert_eq!(tenant["id"], json!(t.w.tenant));
    // One event in the same step, with the old and the new name.
    assert_eq!(t.actions(), vec!["tenant_renamed"]);
    {
        let events = t.w.metadata.events.lock().unwrap();
        assert_eq!(events[0].before, Some(json!({ "display_name": null })));
        assert_eq!(events[0].after, Some(json!({ "display_name": "Acme Corp" })));
    }
    // The same name writes nothing.
    assert_eq!(patch(json!({ "display_name": "Acme Corp" })).await.status(), 200);
    assert_eq!(t.actions().len(), 1);
    // 80 characters (not bytes) are fine.
    let long = "é".repeat(80);
    assert_eq!(body_of(patch(json!({ "display_name": long })).await).await["display_name"], long.as_str());
    // Empty or blank clears it: readers fall back to the slug.
    assert_eq!(body_of(patch(json!({ "display_name": "   " })).await).await, json!({ "display_name": null }));
    assert_eq!(me(DEFAULT_SUB).await["tenant"]["display_name"], Value::Null);
    assert_eq!(body_of(patch(json!({ "display_name": null })).await).await, json!({ "display_name": null }));
    // Too long, control characters, a missing or mistyped field: 422, and nothing changes.
    let events = t.actions().len();
    for bad in [
        json!({ "display_name": "x".repeat(81) }),
        json!({ "display_name": "a\nb" }),
        json!({}),
        json!({ "display_name": 7 }),
    ] {
        let err = error_of(patch(bad.clone()).await, 422).await;
        assert_eq!(code_of(&err), "invalid_request", "{bad}");
    }
    assert_eq!(t.actions().len(), events);
}

#[tokio::test]
async fn rename_needs_tenant_admin() {
    let t = mw(Mode::SelfHosted).await;
    t.w.metadata.add_granted(t.w.tenant, "padmin", t.w.project, PlatformRole::Admin, &[]);
    let body = || Some(json!({ "display_name": "Mine" }));
    let err = error_of(t.send("padmin", Method::PATCH, "/v1/tenant", body()).await, 403).await;
    assert_eq!((code_of(&err), err["error"]["required"].as_str()), ("forbidden", Some("tenant admin")));
    let err = error_of(t.send("stranger", Method::PATCH, "/v1/tenant", body()).await, 403).await;
    assert_eq!(code_of(&err), "no_access_yet", "a stranger has no row while sign-up is closed");
    assert!(t.actions().is_empty());
}

// --- Protection reconciles member roles (TE-A, T7) ---------------------------------------

#[tokio::test]
async fn guard_changes_reconcile_member_roles() {
    let t = branching().await;
    let main = t.main();
    t.metadata.branches.lock().unwrap().get_mut(&main).unwrap().protected = true;
    assert_eq!(t.send(Method::PATCH, &t.branch(main), Some(json!({ "protected": false }))).await.status(), 200);
    assert_eq!(t.send(Method::PATCH, &t.branch(main), Some(json!({ "guard_logins": false }))).await.status(), 200);
    assert_eq!(t.send(Method::PUT, &format!("{}/guard-exemptions/deploy", t.branch(main)), None).await.status(), 200);
    assert_eq!(t.send(Method::DELETE, &format!("{}/guard-exemptions/deploy", t.branch(main)), None).await.status(), 200);
    assert_eq!(*t.jobs.member_role_enqueues.lock().unwrap(), vec![main, main, main, main]);
}

// PB T1 review, finding 2: the fake's create copies the parent's switch, as the real one does.
#[tokio::test]
async fn a_new_branch_copies_its_parents_guard_switch() {
    let t = branching().await;
    let main = t.main();
    t.metadata.branches.lock().unwrap().get_mut(&main).unwrap().guard_logins = false;
    let resp = t.send(Method::POST, &t.branches(), Some(json!({ "name": "kid" }))).await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    let kid = BranchId(body["id"].as_str().unwrap().parse().unwrap());
    assert!(!t.metadata.branch(kid).guard_logins);
    assert_eq!(body["guard_logins"], false);
}

// --- Table API: schema changes (TE-A, T6) -------------------------------------------------

impl Branching {
    async fn changes(&self, branch: BranchId, body: Value) -> reqwest::Response {
        self.send(Method::POST, &format!("{}/schema-changes", self.branch(branch)), Some(body)).await
    }
}

fn comment_op(table: &str) -> Value {
    json!({ "set_table_comment": { "table": { "schema": "public", "name": table }, "comment": "c" } })
}

fn create_memos() -> Value {
    json!({ "create_table": {
        "table": { "schema": "public", "name": "memos" },
        "columns": [
            { "name": "id", "data_type": { "schema": "pg_catalog", "name": "int8" }, "identity": "always", "not_null": true },
            { "name": "body", "data_type": { "schema": "pg_catalog", "name": "text" }, "default": "''", "not_null": true }
        ],
        "primary_key": ["id"],
        "rls": { "enabled": true }
    }})
}

#[tokio::test]
async fn schema_changes_validate_before_any_compute() {
    let t = sql_app().await;
    let main = t.main();
    let mut concurrent = json!({ "create_index": { "table": { "schema": "public", "name": "notes" }, "keys": [{ "column": { "name": "body" } }], "concurrently": true } });
    for body in [
        json!({ "operations": [] }),
        json!({ "operations": [comment_op("notes")], "lock_timeout_ms": 0 }),
        json!({ "operations": [comment_op("notes")], "lock_timeout_ms": 60_001 }),
        json!({ "operations": [comment_op("notes")], "cascade": { "dependents_hash": "abc" } }),
        json!({ "operations": [concurrent.clone(), comment_op("notes")] }),
        json!({ "operations": [{ "rename_table": { "table": { "schema": "public", "name": "notes" }, "to": "" } }] }),
        json!({ "operations": [comment_op("notes")], "surprise": 1 }),
    ] {
        let resp = t.changes(main, body.clone()).await;
        let status = resp.status().as_u16();
        let err = error_of(resp, status).await;
        assert!(status == 400 || status == 422, "{status} {body}");
        assert_eq!(code_of(&err), "invalid_request", "{body}");
    }
    let err = error_of(t.changes(main, json!({ "operations": [concurrent.clone(), comment_op("notes")] })).await, 400).await;
    assert_eq!(err["error"]["op_index"], 0, "{err}");
    assert_eq!(t.compute_lookups(), 0);
    // Valid requests get past validation (the fake has no compute).
    concurrent["create_index"]["name"] = json!("notes_body_idx");
    for body in [json!({ "operations": [concurrent] }), json!({ "dry_run": true, "operations": [create_memos()] })] {
        assert_eq!(code_of(&error_of(t.changes(main, body).await, 503).await), "unavailable");
    }
}

#[tokio::test]
async fn schema_changes_on_a_protected_branch_need_admin() {
    let (t, dev) = act_as_app().await;
    let main = t.main();
    t.set_protected(main);
    let body = json!({ "role": "sales", "operations": [comment_op("notes")] });
    let err = error_of(t.send_as("dev", Method::POST, &format!("{}/schema-changes", t.branch(main)), Some(body.clone())).await, 403).await;
    assert_eq!((code_of(&err), err["error"]["required"].as_str()), ("forbidden", Some("admin")));
    // Unprotected: a Developer acting as a role in their set passes the check (with the
    // member role a reconcile would have made).
    t.put_member_role(t.member_role_row(dev, main, &["billing_admin", "sales"]));
    t.metadata.branches.lock().unwrap().get_mut(&main).unwrap().protected = false;
    let resp = t.send_as("dev", Method::POST, &format!("{}/schema-changes", t.branch(main)), Some(body)).await;
    assert_ne!(resp.status(), 403);
    // A reserved role is refused as the SQL editor refuses it.
    let reserved = json!({ "role": "cb_m_x", "operations": [comment_op("notes")] });
    assert_eq!(t.changes(main, reserved).await.status(), 422);
}

impl ScratchCompute {
    /// One boolean from `sql`, run in the scratch database as the cluster's admin.
    async fn yes(&self, sql: &str) -> bool {
        let opts: sqlx::postgres::PgConnectOptions = (*self.admin.connect_options()).clone();
        let pool = sqlx::PgPool::connect_with(opts.database(&self.database)).await.unwrap();
        let v: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned())).fetch_one(&pool).await.unwrap();
        pool.close().await;
        v
    }
}

async fn changes_app(password: &str) -> (ScratchCompute, Branching) {
    let scratch = ScratchCompute::new().await;
    scratch.set_password(password).await;
    let t = owner_console_app(&scratch).await;
    t.seal_owner_password(Some(password));
    (scratch, t)
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_dry_run_leaves_no_trace_and_the_real_run_runs_the_same_sql() {
    let (scratch, t) = changes_app("chg-pass").await;
    let main = t.main();
    let resp = t.changes(main, json!({ "dry_run": true, "operations": [create_memos()] })).await;
    assert_eq!(resp.status(), 200);
    let dry: Value = resp.json().await.unwrap();
    assert_eq!((dry["dry_run"].as_bool(), dry["committed"].as_bool(), dry["would_succeed"].as_bool()), (Some(true), Some(false), Some(true)), "{dry}");
    assert!(dry["statements"].as_array().unwrap().iter().all(|s| s["executed"] == true), "{dry}");
    assert!(!scratch.yes("SELECT to_regclass('public.memos') IS NOT NULL").await, "the dry run rolled back");
    let resp = t.changes(main, json!({ "operations": [create_memos()] })).await;
    assert_eq!(resp.status(), 200);
    let real: Value = resp.json().await.unwrap();
    assert_eq!(real["committed"], true);
    let sql = |v: &Value| v["statements"].as_array().unwrap().iter().map(|s| s["sql"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(sql(&dry), sql(&real));
    assert!(scratch.yes("SELECT relrowsecurity FROM pg_class WHERE oid = 'public.memos'::regclass").await);
    // A second create fails as Postgres says, typed.
    let err = error_of(t.changes(main, json!({ "operations": [create_memos()] })).await, 400).await;
    assert_eq!((code_of(&err), err["error"]["sqlstate"].as_str(), err["error"]["op_index"].as_u64()), ("schema_change_failed", Some("42P07"), Some(0)));
    wait_no_editor_sessions(&scratch).await;
    scratch.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn cascade_needs_the_hash_the_dry_run_returned() {
    let (scratch, t) = changes_app("cas-pass").await;
    let main = t.main();
    scratch.exec_in(&format!("CREATE VIEW public.notes_v AS SELECT id, body FROM public.notes; ALTER VIEW public.notes_v OWNER TO {};", scratch.owner)).await;
    let drop = json!({ "drop_table": { "table": { "schema": "public", "name": "notes" } } });
    // Without cascade (user, 2026-10-13): the dry run and the real run both answer 409
    // has_dependents with what a cascade would drop and its hash, and drop nothing.
    let mut hashes = Vec::new();
    for dry_run in [true, false] {
        let err = error_of(t.changes(main, json!({ "dry_run": dry_run, "operations": [drop.clone()] })).await, 409).await;
        assert_eq!(code_of(&err), "has_dependents", "{err}");
        let objects = err["error"]["dependents"]["objects"].as_array().unwrap_or_else(|| panic!("{err}"));
        assert!(objects.iter().any(|o| o["name"] == "notes_v" && o["kind"] == "view" && o["description"] == "view notes_v"), "{err}");
        hashes.push(err["error"]["dependents"]["hash"].as_str().unwrap().to_string());
        assert!(scratch.yes("SELECT to_regclass('public.notes') IS NOT NULL").await, "nothing dropped");
    }
    assert_eq!(hashes[0], hashes[1], "the same answer from both runs");
    let hash = hashes.remove(0);
    // A dry run with the hash: 200, executed and rolled back.
    let dry: Value = t.changes(main, json!({ "dry_run": true, "operations": [drop.clone()], "cascade": { "dependents_hash": hash } })).await.json().await.unwrap();
    assert_eq!((dry["would_succeed"].as_bool(), dry["dependents"]["hash"].as_str()), (Some(true), Some(hash.as_str())), "{dry}");
    assert!(scratch.yes("SELECT to_regclass('public.notes_v') IS NOT NULL").await, "the dry run rolled back");
    let wrong = "0".repeat(64);
    let err = error_of(t.changes(main, json!({ "operations": [drop.clone()], "cascade": { "dependents_hash": wrong } })).await, 409).await;
    assert_eq!(code_of(&err), "dependents_changed");
    assert!(scratch.yes("SELECT to_regclass('public.notes') IS NOT NULL").await, "nothing dropped");
    let resp = t.changes(main, json!({ "operations": [drop], "cascade": { "dependents_hash": hash } })).await;
    assert_eq!(resp.status(), 200);
    assert!(scratch.yes("SELECT to_regclass('public.notes') IS NULL AND to_regclass('public.notes_v') IS NULL").await);
    wait_no_editor_sessions(&scratch).await;
    scratch.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_failed_concurrent_index_leaves_no_invalid_index() {
    let (scratch, t) = changes_app("idx-pass").await;
    let main = t.main();
    scratch.exec_in("INSERT INTO public.notes (body) VALUES ('same'), ('same')").await;
    let op = json!({ "create_index": { "table": { "schema": "public", "name": "notes" }, "name": "notes_body_uq", "unique": true, "keys": [{ "column": { "name": "body" } }], "concurrently": true } });
    let dry: Value = t.changes(main, json!({ "dry_run": true, "operations": [op.clone()] })).await.json().await.unwrap();
    assert_eq!((dry["statements"][0]["executed"].as_bool(), dry["statements"][0]["skipped"].as_str(), dry["would_succeed"].clone()), (Some(false), Some("concurrently"), Value::Null), "{dry}");
    let err = error_of(t.changes(main, json!({ "operations": [op] })).await, 409).await;
    assert_eq!((code_of(&err), err["error"]["sqlstate"].as_str()), ("index_build_failed", Some("23505")));
    assert!(scratch.yes("SELECT NOT EXISTS (SELECT FROM pg_class WHERE relname = 'notes_body_uq')").await);
    wait_no_editor_sessions(&scratch).await;
    scratch.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_held_lock_times_out_typed() {
    let (scratch, t) = changes_app("lock-pass").await;
    let main = t.main();
    let opts: sqlx::postgres::PgConnectOptions = (*scratch.admin.connect_options()).clone();
    let holder = sqlx::PgPool::connect_with(opts.database(&scratch.database)).await.unwrap();
    let mut tx = holder.begin().await.unwrap();
    sqlx::query("LOCK TABLE public.notes IN ACCESS SHARE MODE").execute(&mut *tx).await.unwrap();
    let add = json!({ "add_column": { "table": { "schema": "public", "name": "notes" }, "column": { "name": "x", "data_type": { "schema": "pg_catalog", "name": "int4" } } } });
    let err = error_of(t.changes(main, json!({ "operations": [add], "lock_timeout_ms": 200 })).await, 409).await;
    assert_eq!(code_of(&err), "lock_timeout");
    tx.rollback().await.unwrap();
    holder.close().await;
    wait_no_editor_sessions(&scratch).await;
    scratch.drop().await;
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_role_without_rights_is_permission_denied() {
    let (scratch, t) = changes_app("perm-pass").await;
    let main = t.main();
    let ep = t.metadata.branch(main).endpoint_id.unwrap();
    let group = format!("{}_grp", scratch.owner);
    t.metadata.add_group_role(&ep, &group, &[]);
    let a = t.grant_member("dev-a", PlatformRole::Developer, &[&group]);
    let row = t.member_role_row(a, main, &[&group]);
    let member_role = row.role_name.clone();
    scratch.exec_in(&format!("CREATE ROLE {group} NOLOGIN; CREATE ROLE {member_role} LOGIN NOINHERIT PASSWORD 'pw'; GRANT {group} TO {member_role} WITH INHERIT FALSE, SET TRUE; REVOKE CREATE ON SCHEMA public FROM PUBLIC;")).await;
    t.put_member_role(row);
    let body = json!({ "role": group, "operations": [create_memos()] });
    let err = error_of(t.send_as("dev-a", Method::POST, &format!("{}/schema-changes", t.branch(main)), Some(body)).await, 403).await;
    assert_eq!((code_of(&err), err["error"]["op_index"].as_u64()), ("permission_denied", Some(0)));
    wait_no_editor_sessions(&scratch).await;
    let admin = scratch.admin.clone();
    scratch.drop().await;
    use sqlx::Executor;
    admin.execute(sqlx::AssertSqlSafe(format!("DROP ROLE {member_role}; DROP ROLE {group}"))).await.unwrap();
}

// --- T6 review, fix round ------------------------------------------------------------------

/// The `has_dependents` hash of nothing: a cascade whose drops reach only each other.
fn no_dependents_hash() -> String {
    chelabase_schema::ddl::dependents_hash(&[])
}

// Finding 1: a failed concurrent build cleans up only an index it created itself; an older
// INVALID index of the same name (a leftover, or another session's build) stays.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_failed_concurrent_build_leaves_other_invalid_indexes_alone() {
    let (scratch, t) = changes_app("left-pass").await;
    let main = t.main();
    // A leftover INVALID index `notes_left` (as a failed concurrent build leaves one).
    scratch.exec_in("CREATE INDEX notes_left ON public.notes (body)").await;
    scratch.exec_in("UPDATE pg_index SET indisvalid = false WHERE indexrelid = 'public.notes_left'::regclass").await;
    for keys in [json!([{ "column": { "name": "nope" } }]), json!([{ "column": { "name": "body" } }])] {
        let op = json!({ "create_index": { "table": { "schema": "public", "name": "notes" }, "name": "notes_left", "keys": keys, "concurrently": true } });
        let resp = t.changes(main, json!({ "operations": [op] })).await;
        assert!(resp.status().is_client_error(), "{}", resp.status());
        assert!(scratch.yes("SELECT EXISTS (SELECT FROM pg_class WHERE relname = 'notes_left')").await, "the leftover stays: {keys}");
    }
    wait_no_editor_sessions(&scratch).await;
    scratch.drop().await;
}

// Finding 2: drops that reach only each other aren't a cascade beyond the request.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn dropping_a_parent_and_its_child_together_works_in_both_orders() {
    let (scratch, t) = changes_app("pair-pass").await;
    let main = t.main();
    let tables = |o: &str| {
        format!(
            "CREATE TABLE public.parent (id int PRIMARY KEY); CREATE TABLE public.child (pid int REFERENCES public.parent); \
             ALTER TABLE public.parent OWNER TO {o}; ALTER TABLE public.child OWNER TO {o};"
        )
    };
    let drop = |n: &str| json!({ "drop_table": { "table": { "schema": "public", "name": n } } });
    // Child first: no cascade needed.
    scratch.exec_in(&tables(&scratch.owner)).await;
    let resp = t.changes(main, json!({ "operations": [drop("child"), drop("parent")] })).await;
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
    assert!(scratch.yes("SELECT to_regclass('public.parent') IS NULL AND to_regclass('public.child') IS NULL").await);
    // Parent first: the cascade reaches only the child, which the request drops too.
    scratch.exec_in(&tables(&scratch.owner)).await;
    let ops = json!([drop("parent"), drop("child")]);
    let resp = t.changes(main, json!({ "operations": ops, "cascade": { "dependents_hash": no_dependents_hash() } })).await;
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
    assert!(scratch.yes("SELECT to_regclass('public.parent') IS NULL AND to_regclass('public.child') IS NULL").await);
    // An enum and the one column of its type.
    scratch.exec_in(&format!("CREATE TYPE public.mood AS ENUM ('ok'); ALTER TYPE public.mood OWNER TO {o}; ALTER TABLE public.notes ADD COLUMN m public.mood;", o = scratch.owner)).await;
    let ops = json!([
        { "drop_enum": { "schema": "public", "name": "mood" } },
        { "drop_column": { "table": { "schema": "public", "name": "notes" }, "column": "m" } },
    ]);
    let resp = t.changes(main, json!({ "operations": ops, "cascade": { "dependents_hash": no_dependents_hash() } })).await;
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
    wait_no_editor_sessions(&scratch).await;
    scratch.drop().await;
}

// Minor: the context read before the transaction honours `lock_timeout_ms` too.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn the_context_read_times_out_on_a_held_lock() {
    let (scratch, t) = changes_app("ctx-pass").await;
    let main = t.main();
    let opts: sqlx::postgres::PgConnectOptions = (*scratch.admin.connect_options()).clone();
    let holder = sqlx::PgPool::connect_with(opts.database(&scratch.database)).await.unwrap();
    let mut tx = holder.begin().await.unwrap();
    sqlx::query("LOCK TABLE public.notes IN ACCESS EXCLUSIVE MODE").execute(&mut *tx).await.unwrap();
    let retype = json!({ "alter_column": { "table": { "schema": "public", "name": "notes" }, "column": "body",
        "change": { "data_type": { "schema": "pg_catalog", "name": "varchar" } } } });
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        t.changes(main, json!({ "dry_run": true, "operations": [retype], "lock_timeout_ms": 200 })),
    )
    .await
    .expect("answered, not hung");
    assert_eq!(code_of(&error_of(resp, 409).await), "lock_timeout");
    tx.rollback().await.unwrap();
    holder.close().await;
    wait_no_editor_sessions(&scratch).await;
    scratch.drop().await;
}

// Minor: a table the request names that doesn't exist is 404 whatever the path (concurrent or
// not, dry run or not).
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_missing_table_is_not_found_on_every_path() {
    let (scratch, t) = changes_app("ghost-pass").await;
    let main = t.main();
    let ghost = json!({ "schema": "public", "name": "ghost" });
    let add = json!({ "add_column": { "table": ghost, "column": { "name": "x", "data_type": { "schema": "pg_catalog", "name": "int4" } } } });
    let index = json!({ "create_index": { "table": ghost, "name": "ghost_x", "keys": [{ "column": { "name": "x" } }], "concurrently": true } });
    for (op, dry_run) in [(add.clone(), true), (add, false), (index.clone(), true), (index, false)] {
        let err = error_of(t.changes(main, json!({ "dry_run": dry_run, "operations": [op.clone()] })).await, 404).await;
        assert_eq!((code_of(&err), err["error"]["op_index"].as_u64()), ("not_found", Some(0)), "{op} dry_run={dry_run}");
    }
    wait_no_editor_sessions(&scratch).await;
    scratch.drop().await;
}

// T6 fix round (the implementer's): an enum dropped with the table that has a column of its
// type, or with that column. The enum's cascade reaches only the request's own targets.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn dropping_an_enum_with_its_columns_table_or_column() {
    let (scratch, t) = changes_app("enum-pass").await;
    let main = t.main();
    let make = |table: bool| {
        let o = &scratch.owner;
        let column = if table {
            format!("CREATE TABLE public.moods (m public.mood); ALTER TABLE public.moods OWNER TO {o};")
        } else {
            "ALTER TABLE public.notes ADD COLUMN m public.mood;".to_string()
        };
        format!("CREATE TYPE public.mood AS ENUM ('ok'); ALTER TYPE public.mood OWNER TO {o}; {column}")
    };
    let drop_enum = json!({ "drop_enum": { "schema": "public", "name": "mood" } });
    let none = no_dependents_hash();
    // [drop_enum, drop_table]: without cascade, the enum's drop reaches the table's column (a
    // part of the request's other target): a cascade of nothing beyond the request.
    scratch.exec_in(&make(true)).await;
    let ops = json!([drop_enum.clone(), { "drop_table": { "table": { "schema": "public", "name": "moods" } } }]);
    for dry_run in [true, false] {
        let err = error_of(t.changes(main, json!({ "dry_run": dry_run, "operations": ops.clone() })).await, 409).await;
        assert_eq!((code_of(&err), err["error"]["dependents"]["hash"].as_str()), ("has_dependents", Some(none.as_str())), "{err}");
    }
    let dry: Value = t.changes(main, json!({ "dry_run": true, "operations": ops.clone(), "cascade": { "dependents_hash": none } })).await.json().await.unwrap();
    assert_eq!(dry["would_succeed"], true, "{dry}");
    let resp = t.changes(main, json!({ "operations": ops, "cascade": { "dependents_hash": none } })).await;
    assert_eq!(resp.status(), 200, "{:?}", resp.text().await);
    assert!(scratch.yes("SELECT to_regclass('public.moods') IS NULL AND to_regtype('public.mood') IS NULL").await);
    // [drop_enum, drop_column]: the enum's cascade drops the column; its own drop is skipped.
    scratch.exec_in(&make(false)).await;
    let ops = json!([drop_enum, { "drop_column": { "table": { "schema": "public", "name": "notes" }, "column": "m" } }]);
    let resp = t.changes(main, json!({ "operations": ops, "cascade": { "dependents_hash": none } })).await;
    assert_eq!(resp.status(), 200);
    let real: Value = resp.json().await.unwrap();
    assert_eq!((real["statements"][1]["executed"].as_bool(), real["statements"][1]["skipped"].as_str()), (Some(false), Some("already_dropped")), "{real}");
    assert!(scratch.yes("SELECT NOT EXISTS (SELECT FROM pg_attribute WHERE attrelid = 'public.notes'::regclass AND attname = 'm' AND NOT attisdropped)").await);
    wait_no_editor_sessions(&scratch).await;
    scratch.drop().await;
}

// --- Members hub T5: the privileges read and privilege ops -------------------------------
//
// The live tests run on scratch databases of the stack's metadata Postgres (PG 17, P22), with
// the fakes' metadata: the owner run is the scratch owner `t6_owner_<hex>`, a plain LOGIN role
// (not a superuser), so roles owning objects reach it only through the memberships each test
// grants. Cluster-wide roles of a run are `<owner>_<name>` and are dropped with it; `anon`,
// `authenticated` and `neon_superuser` are made once if missing and kept (as `catalog_live`
// and `app_users_live` do), only memberships of the run's own roles being added to them.

impl Branching {
    /// `GET …/pg-databases/{pg_database}/privileges{query}` as the default caller (a tenant admin).
    async fn privileges(&self, branch: BranchId, pg_database: &str, query: &str) -> reqwest::Response {
        self.send(Method::GET, &self.privileges_path(branch, pg_database, query), None).await
    }

    async fn privileges_as(&self, sub: &str, branch: BranchId, pg_database: &str, query: &str) -> reqwest::Response {
        self.send_as(sub, Method::GET, &self.privileges_path(branch, pg_database, query), None).await
    }

    fn privileges_path(&self, branch: BranchId, pg_database: &str, query: &str) -> String {
        format!("{}/pg-databases/{pg_database}/privileges{query}", self.branch(branch))
    }

    /// The `privileges_changed` events, oldest first.
    fn privilege_events(&self) -> Vec<chelabase_metadata::members::AccessEvent> {
        self.metadata.events.lock().unwrap().iter().filter(|e| e.action == "privileges_changed").cloned().collect()
    }

    /// Registers `names` as roles of the main branch in the fakes (for routes that look a role
    /// up in the metadata).
    fn known_roles(&self, names: &[&String]) {
        let ep = self.metadata.branch(self.main()).endpoint_id.unwrap();
        for n in names {
            self.metadata.add_group_role(&ep, n, &[]);
        }
    }
}

impl ScratchCompute {
    /// `<owner>_<name>`: a cluster-wide role name of this run.
    fn named(&self, name: &str) -> String {
        format!("{}_{name}", self.owner)
    }

    /// NOLOGIN roles of this run (cluster-wide), by their full names.
    async fn make_roles<const N: usize>(&self, names: [&str; N]) -> [String; N] {
        let full = names.map(|n| self.named(n));
        for r in &full {
            self.exec_cluster(&format!("CREATE ROLE {r} NOLOGIN")).await;
        }
        full
    }

    /// Runs `sql` as the cluster's admin on the metadata database (roles and memberships).
    async fn exec_cluster(&self, sql: &str) {
        use sqlx::Executor;
        self.admin.execute(sqlx::AssertSqlSafe(sql.to_owned())).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    /// One text value of `sql` (NULL as ""), in the scratch database as the cluster's admin.
    async fn text(&self, sql: &str) -> String {
        let opts: sqlx::postgres::PgConnectOptions = (*self.admin.connect_options()).clone();
        let pool = sqlx::PgPool::connect_with(opts.database(&self.database)).await.unwrap();
        let v: Option<String> =
            sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned())).fetch_one(&pool).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
        pool.close().await;
        v.unwrap_or_default()
    }

    /// `grantor:PRIVILEGE` entries of `grantee` on relation `relation` (`schema.name`), sorted.
    async fn acl(&self, relation: &str, grantee: &str) -> String {
        self.text(&format!(
            "SELECT string_agg(e, ',' ORDER BY e) FROM (SELECT a.grantor::regrole::text || ':' || a.privilege_type AS e \
             FROM pg_class c, aclexplode(c.relacl) a WHERE c.oid = '{relation}'::regclass AND a.grantee = '{grantee}'::regrole) x"
        ))
        .await
    }

    /// The same for schema `schema`'s ACL (only `grantee`'s own entries: PUBLIC's default
    /// `USAGE` on `public` (PG15+) never shows up here, unlike `has_schema_privilege`).
    async fn schema_acl(&self, schema: &str, grantee: &str) -> String {
        self.text(&format!(
            "SELECT string_agg(e, ',' ORDER BY e) FROM (SELECT a.grantor::regrole::text || ':' || a.privilege_type AS e \
             FROM pg_namespace n, aclexplode(n.nspacl) a WHERE n.nspname = '{schema}' AND a.grantee = '{grantee}'::regrole) x"
        ))
        .await
    }

    /// The same for one column's ACL.
    async fn column_acl(&self, relation: &str, column: &str, grantee: &str) -> String {
        self.text(&format!(
            "SELECT string_agg(e, ',' ORDER BY e) FROM (SELECT a.grantor::regrole::text || ':' || a.privilege_type AS e \
             FROM pg_attribute att, aclexplode(att.attacl) a WHERE att.attrelid = '{relation}'::regclass \
             AND att.attname = '{column}' AND a.grantee = '{grantee}'::regrole) x"
        ))
        .await
    }

    /// `grantor:PRIVILEGE` of `grantee` in `for_role`'s default privileges for tables in `public`.
    async fn default_acl(&self, for_role: &str, grantee: &str) -> String {
        self.text(&format!(
            "SELECT string_agg(e, ',' ORDER BY e) FROM (SELECT a.grantor::regrole::text || ':' || a.privilege_type AS e \
             FROM pg_default_acl d, aclexplode(d.defaclacl) a WHERE d.defaclrole = '{for_role}'::regrole \
             AND d.defaclnamespace = 'public'::regnamespace AND d.defaclobjtype = 'r' AND a.grantee = '{grantee}'::regrole) x"
        ))
        .await
    }

    /// Every `pg_auth_members` row of `member` (role/grantor/admin/inherit/set), sorted (P26: an
    /// owner run leaves them exactly as they were).
    async fn memberships(&self, member: &str) -> String {
        self.text(&format!(
            "SELECT string_agg(e, ',' ORDER BY e) FROM (SELECT m.roleid::regrole::text || '/' || m.grantor::regrole::text \
             || '/' || m.admin_option || '/' || m.inherit_option || '/' || m.set_option AS e \
             FROM pg_auth_members m WHERE m.member = '{member}'::regrole) x"
        ))
        .await
    }

    /// Drops the database, then `roles` (best effort, in order), then the owner.
    async fn drop_with(self, roles: &[String]) {
        use sqlx::Executor;
        self.admin
            .execute(sqlx::AssertSqlSafe(format!("DROP DATABASE {} WITH (FORCE)", self.database)))
            .await
            .unwrap();
        for r in roles {
            let _ = self.admin.execute(sqlx::AssertSqlSafe(format!("DROP ROLE IF EXISTS {r}"))).await;
        }
        self.admin.execute(sqlx::AssertSqlSafe(format!("DROP ROLE {}", self.owner))).await.unwrap();
    }
}

/// Makes the cluster-wide `role` (NOLOGIN) unless it exists; it is kept.
fn shared_role_sql(role: &str) -> String {
    format!("DO $$ BEGIN CREATE ROLE {role} NOLOGIN; EXCEPTION WHEN duplicate_object OR unique_violation THEN NULL; END $$")
}

/// The body of a response with `status`, its text in the failure message.
async fn answer(resp: reqwest::Response, status: u16) -> Value {
    let got = resp.status().as_u16();
    let text = resp.text().await.unwrap();
    assert_eq!(got, status, "{text}");
    serde_json::from_str(&text).unwrap_or(Value::Null)
}

fn public_table(name: &str) -> Value {
    json!({ "schema": "public", "name": name })
}

fn grant_op(table: &str, privileges: &[&str], grantee: &str) -> Value {
    json!({ "grant": { "table": public_table(table), "grant": { "privileges": privileges, "grantee": { "role": grantee } } } })
}

fn revoke_op(table: &str, privileges: &[&str], grantee: &str) -> Value {
    json!({ "revoke": { "table": public_table(table), "grant": { "privileges": privileges, "grantee": { "role": grantee } } } })
}

fn schema_usage_op(grantee: &str) -> Value {
    json!({ "grant_schema": { "schema": "public", "grantee": { "role": grantee }, "privileges": ["usage"] } })
}

fn all_rows_op(table: &str, role: &str, enabled: bool) -> Value {
    json!({ "set_all_rows": { "table": public_table(table), "role": role, "enabled": enabled } })
}

fn default_select_op(for_role: &str, grant: bool, grantee: &str) -> Value {
    json!({ "alter_default_privileges": { "for_role": for_role, "schema": "public", "object": "tables",
        "grant": grant, "grantee": { "role": grantee }, "privileges": ["select"] } })
}

fn relation_target(name: &str) -> Value {
    json!({ "relation": { "schema": "public", "name": name } })
}

/// A listed object of a read page, by name.
fn listed<'a>(page: &'a Value, name: &str) -> &'a Value {
    page["objects"]
        .as_array()
        .unwrap_or_else(|| panic!("no objects in {page}"))
        .iter()
        .find(|o| o["name"] == name)
        .unwrap_or_else(|| panic!("{name} isn't listed in {page}"))
}

/// `(grantor, PRIVILEGE)` of a `grants` list.
fn grant_pairs(grants: &Value) -> std::collections::BTreeSet<(String, String)> {
    grants
        .as_array()
        .unwrap_or_else(|| panic!("no grants in {grants}"))
        .iter()
        .map(|g| (g["grantor"]["name"].as_str().unwrap().to_string(), g["privilege"].as_str().unwrap().to_string()))
        .collect()
}

fn pair_set(v: &[(&str, &str)]) -> std::collections::BTreeSet<(String, String)> {
    v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
}

/// A query value with `/`, `%`, `&`, `+` and space escaped.
fn query_escaped(s: &str) -> String {
    s.replace('%', "%25").replace('/', "%2F").replace('&', "%26").replace('+', "%2B").replace(' ', "%20")
}

/// A Developer `dev-a` acting as `group` (a role this run made) through a member role, as the
/// reconciler would have made it (password `pw`, `INHERIT FALSE, SET TRUE`). Returns the member
/// id and the member role's name (to drop with the run).
async fn developer_acting_as(t: &Branching, scratch: &ScratchCompute, group: &str) -> (uuid::Uuid, String) {
    let main = t.main();
    t.known_roles(&[&group.to_string()]);
    let a = t.grant_member("dev-a", PlatformRole::Developer, &[group]);
    let row = t.member_role_row(a, main, &[group]);
    let member_role = row.role_name.clone();
    scratch
        .exec_cluster(&format!("CREATE ROLE {member_role} LOGIN NOINHERIT PASSWORD 'pw'; GRANT {group} TO {member_role} WITH INHERIT FALSE, SET TRUE"))
        .await;
    t.put_member_role(row);
    (a, member_role)
}

// The read checks what needs no compute first: `role` is required, `limit` is 1 to 500, and a
// malformed `after` in object mode is 400/422, not a SQL error (T4-6).
#[tokio::test]
async fn privileges_read_validates_before_any_compute() {
    let t = sql_app().await;
    let main = t.main();
    for query in [
        "",
        "?role=",
        "?role=editor&limit=0",
        "?role=editor&limit=501",
        "?role=editor&limit=x",
        "?role=editor&schema=public&after=garbage",
        "?role=editor&schema=public&after=0%2Fx%2Fnotes",
        "?role=editor&schema=public&after=x%2F1%2Fnotes",
    ] {
        let resp = t.privileges(main, "web1", query).await;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap();
        assert!(status == 400 || status == 422, "{query}: {status} {text}");
        let body: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(code_of(&body), "invalid_request", "{query}");
    }
    assert_eq!(t.compute_lookups(), 0);
    // The bounds and a well-formed `after` get as far as the compute (none in the fake).
    for query in ["?role=editor&limit=500", "?role=editor&limit=1", "?role=editor&schema=public&after=0%2F16384%2Fnotes", "?role=editor&after=public"] {
        let resp = t.privileges(main, "web1", query).await;
        assert_eq!(code_of(&answer(resp, 503).await), "unavailable", "{query}");
    }
}

// MH-31: the Postgres database resolves only through `branch_databases`, never a template or
// the system `postgres`.
#[tokio::test]
async fn privileges_read_refuses_a_pg_database_outside_branch_databases() {
    let t = sql_app().await;
    let main = t.main();
    let resp = t.privileges(main, "web1", "?role=editor").await;
    assert_eq!(code_of(&answer(resp, 503).await), "unavailable", "web1 is the branch's database");
    for name in ["postgres", "template1", "template0", "nope"] {
        let resp = t.privileges(main, name, "?role=editor").await;
        assert_eq!(code_of(&answer(resp, 404).await), "not_found", "{name}");
    }
    assert_eq!(t.compute_lookups(), 1, "only web1 reached the compute");
}

// R6 / MH-30 / MH-32: the read is Viewer level; owner-run privilege edits are Admin only; a
// Developer acting as a role passes the route's access (Postgres then decides).
#[tokio::test]
async fn privilege_powers_follow_the_routes() {
    let (t, dev) = act_as_app().await;
    let main = t.main();
    t.grant_member("vic", PlatformRole::Viewer, &[]);
    let resp = t.privileges_as("vic", main, "web1", "?role=billing").await;
    assert_eq!(code_of(&answer(resp, 503).await), "unavailable", "a Viewer reads");
    let path = format!("{}/schema-changes", t.branch(main));
    let op = json!({ "operations": [schema_usage_op("billing")] });
    let err = answer(t.send_as("vic", Method::POST, &path, Some(op.clone())).await, 403).await;
    assert_eq!(code_of(&err), "forbidden", "{err}");
    let err = answer(t.send_as("dev", Method::POST, &path, Some(op)).await, 403).await;
    assert_eq!((code_of(&err), err["error"]["required"].as_str()), ("forbidden", Some("admin")), "an owner run is Admin only");
    t.put_member_role(t.member_role_row(dev, main, &["billing_admin", "sales"]));
    let as_role = json!({ "role": "sales", "operations": [schema_usage_op("billing")] });
    let resp = t.send_as("dev", Method::POST, &path, Some(as_role)).await;
    assert_eq!(resp.status(), 503, "a Developer acting as a role gets past access");
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn privileges_read_lists_grants_with_grantors_and_hashes() {
    let (scratch, t) = changes_app("prd-pass").await;
    let main = t.main();
    let o = scratch.owner.clone();
    let [reader, mid, x1, x5] = scratch.make_roles(["reader", "mid", "x1", "x5"]).await;
    t.known_roles(&[&reader, &mid]);
    scratch.exec_cluster(&format!("GRANT {x1} TO {o} WITH INHERIT FALSE, SET TRUE")).await;
    scratch
        .exec_in(&format!(
            "GRANT USAGE ON SCHEMA public TO {reader};
             GRANT SELECT ON public.notes TO {mid} WITH GRANT OPTION;
             GRANT INSERT ON public.notes TO {reader};
             SET ROLE {mid}; GRANT SELECT ON public.notes TO {reader}; RESET ROLE;
             CREATE TABLE public.t_x1 (id int); ALTER TABLE public.t_x1 OWNER TO {x1};
             CREATE TABLE public.t_x5 (id int); ALTER TABLE public.t_x5 OWNER TO {x5};"
        ))
        .await;
    let db = scratch.database.clone();
    let page = answer(t.privileges(main, &db, &format!("?role={reader}&schema=public")).await, 200).await;
    assert_eq!(page["role"], reader.as_str());
    assert!(page["server_version_num"].as_i64().unwrap() >= 170000, "{page}");
    assert!(page["offered"]["table"].as_array().unwrap().contains(&json!("maintain")), "{page}");
    let notes = listed(&page, "notes");
    assert_eq!((notes["kind"].as_str(), notes["owner"].as_str(), notes["owner_settable"].as_bool()), (Some("table"), Some(o.as_str()), Some(true)));
    assert_eq!(grant_pairs(&notes["grants"]), pair_set(&[(o.as_str(), "INSERT"), (mid.as_str(), "SELECT")]), "every grantor (MH-24)");
    let hash = notes["acl_hash"].as_str().unwrap();
    assert!(hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()), "{hash}");
    // D6 / P27: a role the owner can SET to is settable; one it can't reach isn't.
    assert_eq!(listed(&page, "t_x1")["owner_settable"], true);
    assert_eq!(listed(&page, "t_x5")["owner_settable"], false);
    assert!(page["next_after"].is_null(), "{page}");
    // Schema mode: the schema list and default ACLs, no objects.
    let top = answer(t.privileges(main, &db, &format!("?role={reader}")).await, 200).await;
    let public = top["schemas"].as_array().unwrap().iter().find(|s| s["name"] == "public").unwrap_or_else(|| panic!("{top}"));
    assert!(grant_pairs(&public["grants"]).iter().any(|(_, p)| p == "USAGE"), "{public}");
    assert_eq!(top["objects"], json!([]));
    // PUBLIC is read as `public` (exact, lower case, T4-1).
    let pubpage = answer(t.privileges(main, &db, "?role=public&schema=public").await, 200).await;
    assert_eq!(pubpage["role"], "public");
    // A role that isn't on the branch.
    let err = answer(t.privileges(main, &db, &format!("?role={}", scratch.named("nobody"))).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[reader, mid, x1, x5]).await;
}

// MH-32: the object mode is paged (`limit`, `next_after`) and every object comes exactly once.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn privileges_read_pages_per_schema() {
    let (scratch, t) = changes_app("pg-pass").await;
    let main = t.main();
    let o = scratch.owner.clone();
    let [reader] = scratch.make_roles(["reader"]).await;
    t.known_roles(&[&reader]);
    let mut sql = format!("CREATE SCHEMA app AUTHORIZATION {o};");
    for i in 0..5 {
        sql.push_str(&format!("CREATE TABLE app.p{i} (id int); ALTER TABLE app.p{i} OWNER TO {o};"));
    }
    scratch.exec_in(&sql).await;
    let db = scratch.database.clone();
    let mut names = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..10 {
        let query = match &after {
            None => format!("?role={reader}&schema=app&limit=2"),
            Some(a) => format!("?role={reader}&schema=app&limit=2&after={}", query_escaped(a)),
        };
        let page = answer(t.privileges(main, &db, &query).await, 200).await;
        let objects = page["objects"].as_array().unwrap();
        assert!(objects.len() <= 2, "{page}");
        names.extend(objects.iter().map(|o| o["name"].as_str().unwrap().to_string()));
        after = page["next_after"].as_str().map(str::to_string);
        if after.is_none() {
            break;
        }
    }
    assert!(after.is_none(), "paging ends");
    assert_eq!(names, ["p0", "p1", "p2", "p3", "p4"]);
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[reader]).await;
}

// D6 with T1's P26: each statement on an object another role owns runs as that owner. Every
// way the owner reaches it: SET already (case 1), an owner-granted row without SET (case 2),
// ADMIN only (case 3), ADMIN through an INHERIT membership (T1 item 6's extra). The grantor
// is the object's owner, the owner's memberships are exactly as before, and the statement
// after a run-as one runs as the owner again (RESET ROLE). `public` (owned by
// `pg_database_owner`) works too.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn owner_run_grants_on_a_table_owned_by_a_settable_role() {
    let (scratch, t) = changes_app("run-pass").await;
    let main = t.main();
    let o = scratch.owner.clone();
    let [reader, x1, x2, x3, x4, mid] = scratch.make_roles(["reader", "x1", "x2", "x3", "x4", "mid"]).await;
    scratch
        .exec_cluster(&format!(
            "GRANT {x1} TO {o} WITH INHERIT FALSE, SET TRUE;
             GRANT {x2} TO {o} WITH ADMIN TRUE, INHERIT FALSE, SET FALSE;
             GRANT {x2} TO {o} WITH ADMIN FALSE, INHERIT TRUE, SET FALSE GRANTED BY {o};
             GRANT {x3} TO {o} WITH ADMIN TRUE, INHERIT FALSE, SET FALSE;
             GRANT {mid} TO {o} WITH ADMIN FALSE, INHERIT TRUE, SET FALSE;
             GRANT {x4} TO {mid} WITH ADMIN TRUE, INHERIT FALSE, SET FALSE;"
        ))
        .await;
    let mut sql = String::new();
    for (table, owner) in [("t1", &x1), ("t2", &x2), ("t3", &x3), ("t4", &x4)] {
        sql.push_str(&format!("CREATE TABLE public.{table} (id int); ALTER TABLE public.{table} OWNER TO {owner};"));
    }
    scratch.exec_in(&sql).await;
    let before = scratch.memberships(&o).await;
    let ops = json!([
        grant_op("t1", &["select"], &reader),
        grant_op("t2", &["select"], &reader),
        grant_op("t3", &["select"], &reader),
        grant_op("t4", &["select"], &reader),
        grant_op("notes", &["select"], &reader),
        schema_usage_op(&reader),
    ]);
    let real = answer(t.changes(main, json!({ "operations": ops })).await, 200).await;
    assert_eq!(real["committed"], true, "{real}");
    for (table, owner) in [("t1", &x1), ("t2", &x2), ("t3", &x3), ("t4", &x4)] {
        assert_eq!(scratch.acl(&format!("public.{table}"), &reader).await, format!("{owner}:SELECT"), "{table}: granted as its owner");
    }
    assert_eq!(scratch.acl("public.notes", &reader).await, format!("{o}:SELECT"), "RESET ROLE: the owner's own table as the owner");
    let public_acl = scratch.schema_acl("public", &reader).await;
    assert!(public_acl.split(',').any(|e| e.ends_with(":USAGE")), "the reader's own USAGE entry, not PUBLIC's default: {public_acl:?}");
    assert_eq!(scratch.memberships(&o).await, before, "P26: the owner's memberships are unchanged");
    // A statement failing after a run-as one rolls back the whole request, transient grant
    // included.
    let ops = json!([
        revoke_op("t3", &["select"], &reader),
        { "grant": { "table": public_table("notes"), "grant": { "columns": ["nope"], "privileges": ["select"], "grantee": { "role": reader } } } },
    ]);
    let err = answer(t.changes(main, json!({ "operations": ops })).await, 400).await;
    assert_eq!((code_of(&err), err["error"]["sqlstate"].as_str(), err["error"]["op_index"].as_u64()), ("schema_change_failed", Some("42703"), Some(1)), "{err}");
    assert_eq!(scratch.acl("public.t3", &reader).await, format!("{x3}:SELECT"), "rolled back");
    assert_eq!(scratch.memberships(&o).await, before);
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[x2.clone(), x1, x3, x4, mid, reader]).await;
}

// D6: an object whose owner the run-as sequence can't reach is 422 `invalid_request` with
// `extra.owner`, in a dry run too, and nothing runs. That includes an owner the database
// owner holds ADMIN on only through a non-INHERIT membership (Postgres finds no grantor there:
// `no possible grantors`, never a 500).
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn owner_run_refuses_an_object_whose_owner_is_unreachable() {
    let (scratch, t) = changes_app("unr-pass").await;
    let main = t.main();
    let o = scratch.owner.clone();
    let [reader, x5, adm, x6] = scratch.make_roles(["reader", "x5", "adm", "x6"]).await;
    scratch
        .exec_cluster(&format!(
            "GRANT {adm} TO {o} WITH ADMIN TRUE, INHERIT FALSE, SET FALSE;
             GRANT {x6} TO {adm} WITH ADMIN TRUE, INHERIT TRUE, SET TRUE;"
        ))
        .await;
    scratch
        .exec_in(&format!(
            "CREATE TABLE public.t5 (id int); ALTER TABLE public.t5 OWNER TO {x5};
             CREATE TABLE public.t6 (id int); ALTER TABLE public.t6 OWNER TO {x6};"
        ))
        .await;
    let before = scratch.memberships(&o).await;
    for (table, owner) in [("t5", &x5), ("t6", &x6)] {
        for dry_run in [true, false] {
            let body = json!({ "dry_run": dry_run, "operations": [grant_op(table, &["select"], &reader)] });
            let err = answer(t.changes(main, body).await, 422).await;
            assert_eq!((code_of(&err), err["error"]["owner"].as_str()), ("invalid_request", Some(owner.as_str())), "{table} dry_run={dry_run}: {err}");
        }
        assert_eq!(scratch.acl(&format!("public.{table}"), &reader).await, "", "{table}");
    }
    assert_eq!(scratch.memberships(&o).await, before);
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[x6, adm, x5, reader]).await;
}

// P30 (user): on an object `cloud_admin` made, the owner's grants are recorded with grantor
// `neon_superuser` (which holds every privilege with grant option there, and which the owner
// inherits). The editor revokes them as the owner, without run-as, although the object's
// owner isn't settable, and the verify-after sees them gone. (Guard: the base already does
// this; T5's owner check must not refuse it.)
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_neon_superuser_grant_is_revoked_as_the_owner() {
    let (scratch, t) = changes_app("nsu-pass").await;
    let main = t.main();
    let o = scratch.owner.clone();
    let [reader, x5] = scratch.make_roles(["reader", "x5"]).await;
    scratch.exec_cluster(&shared_role_sql("neon_superuser")).await;
    scratch.exec_cluster(&format!("GRANT neon_superuser TO {o} WITH INHERIT TRUE, SET FALSE")).await;
    scratch
        .exec_in(&format!(
            "CREATE TABLE public.ca (id int); ALTER TABLE public.ca OWNER TO {x5};
             GRANT ALL ON public.ca TO neon_superuser WITH GRANT OPTION;
             SET ROLE {o}; GRANT SELECT ON public.ca TO {reader}; RESET ROLE;"
        ))
        .await;
    assert_eq!(scratch.acl("public.ca", &reader).await, "neon_superuser:SELECT", "the setup");
    let real = answer(t.changes(main, json!({ "operations": [revoke_op("ca", &["select"], &reader)] })).await, 200).await;
    assert_eq!(real["committed"], true);
    assert_eq!(scratch.acl("public.ca", &reader).await, "");
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[x5, reader]).await;
}

// MH-24: a revoke another grantor's entry survives (the table's own and a column's) is 409
// `privileges_not_applied`, naming object, privilege, grantee, grantor and column; the whole
// request rolls back (the owner's own INSERT stays); a dry run answers the same.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_revoke_another_grantor_still_holds_is_privileges_not_applied() {
    let (scratch, t) = changes_app("pna-pass").await;
    let main = t.main();
    let o = scratch.owner.clone();
    let [reader, mid] = scratch.make_roles(["reader", "mid"]).await;
    scratch
        .exec_in(&format!(
            "GRANT SELECT ON public.notes TO {mid} WITH GRANT OPTION;
             GRANT INSERT ON public.notes TO {reader};
             SET ROLE {mid}; GRANT SELECT ON public.notes TO {reader}; GRANT SELECT (body) ON public.notes TO {reader}; RESET ROLE;"
        ))
        .await;
    for dry_run in [true, false] {
        let body = json!({ "dry_run": dry_run, "operations": [revoke_op("notes", &["select", "insert"], &reader)] });
        let err = answer(t.changes(main, body).await, 409).await;
        assert_eq!(code_of(&err), "privileges_not_applied", "{err}");
        let mismatches = err["error"]["mismatches"].as_array().unwrap_or_else(|| panic!("{err}"));
        for column in [Value::Null, json!("body")] {
            let want = json!({ "object": relation_target("notes"), "privilege": "SELECT", "grantee": reader,
                "still_granted_by": mid, "column": column, "wanted": false });
            assert!(mismatches.contains(&want), "{want} in {err}");
        }
        assert!(mismatches.iter().all(|m| m["privilege"] == "SELECT"), "INSERT was revoked: {err}");
    }
    assert_eq!(scratch.acl("public.notes", &reader).await, format!("{o}:INSERT,{mid}:SELECT"), "rolled back");
    assert_eq!(scratch.column_acl("public.notes", "body", &reader).await, format!("{mid}:SELECT"));
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[reader, mid]).await;
}

// P29: Postgres's `no privileges could be revoked` / `were granted` WARNINGs (01006, 01007)
// fail the request per statement, before any re-read: here a role holding SELECT without a
// grant option revokes from a role that holds nothing (the re-read alone would pass), then
// grants (nothing lands).
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_no_privileges_warning_is_privileges_not_applied() {
    let (scratch, t) = changes_app("warn-pass").await;
    let main = t.main();
    let [grp, g] = scratch.make_roles(["grp", "g"]).await;
    let (_, member_role) = developer_acting_as(&t, &scratch, &grp).await;
    scratch.exec_in(&format!("GRANT SELECT ON public.notes TO {grp}")).await;
    let path = format!("{}/schema-changes", t.branch(main));
    for op in [revoke_op("notes", &["select"], &g), grant_op("notes", &["select"], &g)] {
        let body = json!({ "role": grp, "operations": [op.clone()] });
        let err = answer(t.send_as("dev-a", Method::POST, &path, Some(body)).await, 409).await;
        assert_eq!(code_of(&err), "privileges_not_applied", "{op}: {err}");
    }
    assert_eq!(scratch.acl("public.notes", &g).await, "");
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[member_role, grp, g]).await;
}

// MH-33 / P10: `expect_acl` carries the read's hash; a change since is 409
// `privileges_changed_since_read` naming the object (a dry run too), and nothing changes; with
// the fresh hash it applies; an `expect_acl` entry for an object the request doesn't touch is
// ignored.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_stale_acl_hash_is_privileges_changed_since_read() {
    let (scratch, t) = changes_app("stale-pass").await;
    let main = t.main();
    let o = scratch.owner.clone();
    let [reader] = scratch.make_roles(["reader"]).await;
    t.known_roles(&[&reader]);
    let db = scratch.database.clone();
    let q = format!("?role={reader}&schema=public");
    let page = answer(t.privileges(main, &db, &q).await, 200).await;
    let old = listed(&page, "notes")["acl_hash"].as_str().unwrap().to_string();
    scratch.exec_in(&format!("GRANT INSERT ON public.notes TO {reader}")).await;
    for dry_run in [true, false] {
        let body = json!({ "dry_run": dry_run, "operations": [grant_op("notes", &["select"], &reader)],
            "expect_acl": [{ "object": relation_target("notes"), "acl_hash": old }] });
        let err = answer(t.changes(main, body).await, 409).await;
        assert_eq!(code_of(&err), "privileges_changed_since_read", "{err}");
        assert_eq!(err["error"]["objects"], json!([relation_target("notes")]), "{err}");
    }
    assert_eq!(scratch.acl("public.notes", &reader).await, format!("{o}:INSERT"), "nothing changed");
    let page = answer(t.privileges(main, &db, &q).await, 200).await;
    let fresh = listed(&page, "notes")["acl_hash"].as_str().unwrap().to_string();
    assert_ne!(fresh, old);
    let body = json!({ "operations": [grant_op("notes", &["select"], &reader)],
        "expect_acl": [
            { "object": relation_target("notes"), "acl_hash": fresh },
            { "object": { "schema": { "schema": "public" } }, "acl_hash": "0".repeat(64) },
        ] });
    let real = answer(t.changes(main, body).await, 200).await;
    assert_eq!(real["committed"], true);
    assert_eq!(scratch.acl("public.notes", &reader).await, format!("{o}:INSERT,{o}:SELECT"));
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[reader]).await;
}

// S3: the hash covers the whole ACL, so another grantee's grant since the read is caught too.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_stale_hash_from_another_grantees_change_is_detected() {
    let (scratch, t) = changes_app("other-pass").await;
    let main = t.main();
    let [reader, other] = scratch.make_roles(["reader", "other"]).await;
    t.known_roles(&[&reader]);
    let db = scratch.database.clone();
    let page = answer(t.privileges(main, &db, &format!("?role={reader}&schema=public")).await, 200).await;
    let old = listed(&page, "notes")["acl_hash"].as_str().unwrap().to_string();
    scratch.exec_in(&format!("GRANT SELECT ON public.notes TO {other}")).await;
    let body = json!({ "operations": [grant_op("notes", &["select"], &reader)],
        "expect_acl": [{ "object": relation_target("notes"), "acl_hash": old }] });
    let err = answer(t.changes(main, body).await, 409).await;
    assert_eq!(code_of(&err), "privileges_changed_since_read", "{err}");
    assert_eq!(scratch.acl("public.notes", &reader).await, "");
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[reader, other]).await;
}

// MH-27 / P11: a function is named by the read's OID and identity; only the server's own
// `regprocedure` text runs. A changed identity (a rename, or client text that isn't the
// read's) is 409 `privileges_changed_since_read`; a dropped function is 404 with `op_index`.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_changed_function_identity_is_privileges_changed_since_read() {
    let (scratch, t) = changes_app("fn-pass").await;
    let main = t.main();
    let o = scratch.owner.clone();
    let [reader] = scratch.make_roles(["reader"]).await;
    t.known_roles(&[&reader]);
    scratch
        .exec_in(&format!(
            "CREATE FUNCTION public.f(integer) RETURNS integer LANGUAGE sql AS 'SELECT 1';
             ALTER FUNCTION public.f(integer) OWNER TO {o};
             REVOKE EXECUTE ON FUNCTION public.f(integer) FROM PUBLIC;"
        ))
        .await;
    let db = scratch.database.clone();
    let page = answer(t.privileges(main, &db, &format!("?role={reader}&schema=public")).await, 200).await;
    let f = listed(&page, "f");
    assert_eq!(f["kind"], "function");
    let (oid, identity) = (f["oid"].as_u64().unwrap(), f["identity"].as_str().unwrap().to_string());
    let grant = |identity: &str| json!({ "operations": [{ "grant_function": { "function": { "oid": oid, "identity": identity }, "grantee": { "role": reader } } }] });
    let real = answer(t.changes(main, grant(&identity)).await, 200).await;
    assert_eq!(real["committed"], true);
    assert!(scratch.yes(&format!("SELECT has_function_privilege('{reader}', {oid}::oid, 'EXECUTE')")).await);
    // Client text that isn't the function's identity never reaches SQL.
    let hostile = format!("{identity} TO PUBLIC; GRANT ALL ON ALL TABLES IN SCHEMA public TO {reader}; --");
    let err = answer(t.changes(main, grant(&hostile)).await, 409).await;
    assert_eq!(code_of(&err), "privileges_changed_since_read", "{err}");
    assert_eq!(scratch.acl("public.notes", &reader).await, "");
    scratch.exec_in("ALTER FUNCTION public.f(integer) RENAME TO g").await;
    let err = answer(t.changes(main, grant(&identity)).await, 409).await;
    assert_eq!(code_of(&err), "privileges_changed_since_read", "{err}");
    scratch.exec_in("DROP FUNCTION public.g(integer)").await;
    let err = answer(t.changes(main, grant(&identity)).await, 404).await;
    assert_eq!((code_of(&err), err["error"]["op_index"].as_u64()), ("not_found", Some(0)), "{err}");
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[reader]).await;
}

// MH-26: "all rows" is refused (422 `exposes_data_api`, the chain from the data API side) for a
// role `anon` is or inherits, and for one a live secret key's role inherits; in a dry run too;
// no policy is made. A role nothing exposes gets its policy.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn all_rows_for_a_role_anon_inherits_is_exposes_data_api() {
    let (scratch, t) = changes_app("expo-pass").await;
    let main = t.main();
    let [rr, kr, k, safe] = scratch.make_roles(["rr", "kr", "k", "safe"]).await;
    scratch.exec_cluster(&shared_role_sql("anon")).await;
    scratch.exec_cluster(&format!("GRANT {rr} TO anon; GRANT {kr} TO {k}")).await;
    t.seed_secret_key(main, "ci", &k);
    scratch.exec_in("ALTER TABLE public.notes ENABLE ROW LEVEL SECURITY").await;
    for (role, chain) in [(rr.as_str(), json!(["anon", rr])), (kr.as_str(), json!([k, kr])), ("anon", json!(["anon"]))] {
        for dry_run in [true, false] {
            let body = json!({ "dry_run": dry_run, "operations": [all_rows_op("notes", role, true)] });
            let err = answer(t.changes(main, body).await, 422).await;
            assert_eq!(code_of(&err), "exposes_data_api", "{role}: {err}");
            assert_eq!(err["error"]["chain"], chain, "{role}: {err}");
        }
    }
    assert!(scratch.yes("SELECT NOT EXISTS (SELECT FROM pg_policy WHERE polrelid = 'public.notes'::regclass)").await);
    let real = answer(t.changes(main, json!({ "operations": [all_rows_op("notes", &safe, true)] })).await, 200).await;
    assert_eq!(real["committed"], true);
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[rr, kr, k, safe]).await;
}

// MH-26: the hub's policy is found by shape and marker, never by name: turning it on beside a
// user's policy named `<role>_all_rows` (another shape) and a same-shape policy without the
// marker makes a policy of its own, under another name; turning it off drops only that one.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn all_rows_toggle_off_drops_only_the_marked_policy() {
    let (scratch, t) = changes_app("tog-pass").await;
    let main = t.main();
    let [rs] = scratch.make_roles(["rs"]).await;
    t.known_roles(&[&rs]);
    scratch
        .exec_in(&format!(
            "ALTER TABLE public.notes ENABLE ROW LEVEL SECURITY;
             CREATE POLICY {rs}_all_rows ON public.notes FOR SELECT TO {rs} USING (id > 0);
             CREATE POLICY mine ON public.notes AS PERMISSIVE FOR ALL TO {rs} USING (true) WITH CHECK (true);"
        ))
        .await;
    let marked = "SELECT polname::text FROM pg_policy WHERE polrelid = 'public.notes'::regclass \
                  AND obj_description(oid, 'pg_policy') = 'chelabase:all_rows'";
    let real = answer(t.changes(main, json!({ "operations": [all_rows_op("notes", &rs, true)] })).await, 200).await;
    assert_eq!(real["committed"], true);
    let name = scratch.text(marked).await;
    assert!(!name.is_empty() && name != format!("{rs}_all_rows") && name != "mine", "{name}");
    let page = answer(t.privileges(main, &scratch.database, &format!("?role={rs}&schema=public")).await, 200).await;
    assert_eq!(listed(&page, "notes")["all_rows"]["policy"], name.as_str());
    let real = answer(t.changes(main, json!({ "operations": [all_rows_op("notes", &rs, false)] })).await, 200).await;
    assert_eq!(real["committed"], true);
    assert!(
        scratch.yes(&format!("SELECT count(*) = 0 FROM ({marked}) m")).await,
        "the hub's policy is gone"
    );
    let left = scratch
        .text("SELECT string_agg(polname::text, ',' ORDER BY polname) FROM pg_policy WHERE polrelid = 'public.notes'::regclass")
        .await;
    assert_eq!(left, format!("mine,{rs}_all_rows"), "the user's policies stay");
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[rs]).await;
}

// MH-18 / MH-28 / S5: a personal login is refused as a grantee (400 `invalid_request` with
// `op_index`), and so is the database owner in an owner run (the allow-list's owner is the
// session role); nothing runs.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_personal_login_is_refused_as_grantee() {
    let (scratch, t) = changes_app("pl-pass").await;
    let main = t.main();
    let o = scratch.owner.clone();
    let [pl, reader] = scratch.make_roles(["pl", "reader"]).await;
    t.metadata.personal_login_names.lock().unwrap().insert((t.db.as_uuid(), pl.clone()));
    for (ops, op_index) in [
        (json!([grant_op("notes", &["select"], &pl)]), 0),
        (json!([schema_usage_op(&reader), schema_usage_op(&pl)]), 1),
        (json!([grant_op("notes", &["select"], &o)]), 0),
    ] {
        let err = answer(t.changes(main, json!({ "operations": ops.clone() })).await, 400).await;
        assert_eq!((code_of(&err), err["error"]["op_index"].as_u64()), ("invalid_request", Some(op_index)), "{ops}: {err}");
    }
    assert_eq!(scratch.acl("public.notes", &pl).await, "");
    assert!(
        scratch.yes(&format!("SELECT position('{reader}=' in coalesce(nspacl::text, '')) = 0 AND position('{pl}=' in coalesce(nspacl::text, '')) = 0 FROM pg_namespace WHERE nspname = 'public'")).await,
        "nothing ran"
    );
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[pl, reader]).await;
}

// P28 / S4: `ALTER DEFAULT PRIVILEGES FOR ROLE X` runs as X (SET is enough; through the
// transient grant when the owner holds only ADMIN), alone in its request (its `DefaultAcl`
// target is in `owners`); the owner's memberships are unchanged; revoking it works the same.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn default_privileges_for_another_role_apply_as_that_role() {
    let (scratch, t) = changes_app("adp-pass").await;
    let main = t.main();
    let o = scratch.owner.clone();
    let [reader, x1, x3] = scratch.make_roles(["reader", "x1", "x3"]).await;
    scratch
        .exec_cluster(&format!(
            "GRANT {x1} TO {o} WITH INHERIT FALSE, SET TRUE;
             GRANT {x3} TO {o} WITH ADMIN TRUE, INHERIT FALSE, SET FALSE;"
        ))
        .await;
    let before = scratch.memberships(&o).await;
    for x in [&x1, &x3] {
        let real = answer(t.changes(main, json!({ "operations": [default_select_op(x, true, &reader)] })).await, 200).await;
        assert_eq!(real["committed"], true);
        assert_eq!(scratch.default_acl(x, &reader).await, format!("{x}:SELECT"), "{x}");
    }
    assert_eq!(scratch.memberships(&o).await, before);
    let real = answer(t.changes(main, json!({ "operations": [default_select_op(&x1, false, &reader)] })).await, 200).await;
    assert_eq!(real["committed"], true);
    assert_eq!(scratch.default_acl(&x1, &reader).await, "");
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[reader, x1, x3]).await;
}

// MH-31 / R5: the dry run runs the same statements (run-as included) and changes nothing (the
// read's hashes are the same after it); the real run's SQL is the dry run's.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn the_dry_run_returns_the_same_sql_and_changes_nothing() {
    let (scratch, t) = changes_app("dry-pass").await;
    let main = t.main();
    let o = scratch.owner.clone();
    let [reader, x1] = scratch.make_roles(["reader", "x1"]).await;
    t.known_roles(&[&reader]);
    scratch.exec_cluster(&format!("GRANT {x1} TO {o} WITH INHERIT FALSE, SET TRUE")).await;
    scratch.exec_in(&format!("CREATE TABLE public.t1 (id int); ALTER TABLE public.t1 OWNER TO {x1};")).await;
    let db = scratch.database.clone();
    let hashes = |page: &Value| {
        ["notes", "t1"].map(|n| listed(page, n)["acl_hash"].as_str().unwrap().to_string())
    };
    let q = format!("?role={reader}&schema=public");
    let before = hashes(&answer(t.privileges(main, &db, &q).await, 200).await);
    let ops = json!([grant_op("t1", &["select"], &reader), grant_op("notes", &["select"], &reader), schema_usage_op(&reader)]);
    let dry = answer(t.changes(main, json!({ "dry_run": true, "operations": ops.clone() })).await, 200).await;
    assert_eq!((dry["dry_run"].as_bool(), dry["committed"].as_bool(), dry["would_succeed"].as_bool()), (Some(true), Some(false), Some(true)), "{dry}");
    assert!(dry["statements"].as_array().unwrap().iter().all(|s| s["executed"] == true), "{dry}");
    assert_eq!(hashes(&answer(t.privileges(main, &db, &q).await, 200).await), before, "the dry run changed nothing");
    assert_eq!(scratch.acl("public.t1", &reader).await, "");
    let real = answer(t.changes(main, json!({ "operations": ops })).await, 200).await;
    assert_eq!(real["committed"], true);
    let sql = |v: &Value| v["statements"].as_array().unwrap().iter().map(|s| s["sql"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(sql(&dry), sql(&real));
    assert_eq!(scratch.acl("public.t1", &reader).await, format!("{x1}:SELECT"));
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[reader, x1]).await;
}

// P31 (user): the dry run's answer flags every grant (default privileges included) to `anon`
// or `authenticated`, or to a role either of them inherits, with the chain from the data API
// side: `data_api_exposure: [{ op_index, grantee, chain }]`, in op order. Other grants and
// revokes aren't flagged.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn the_dry_run_flags_grants_that_expose_to_the_data_api() {
    let (scratch, t) = changes_app("flag-pass").await;
    let main = t.main();
    let o = scratch.owner.clone();
    let [rr, pr] = scratch.make_roles(["rr", "pr"]).await;
    scratch.exec_cluster(&shared_role_sql("anon")).await;
    scratch.exec_cluster(&shared_role_sql("authenticated")).await;
    scratch.exec_cluster(&format!("GRANT {rr} TO authenticated")).await;
    let ops = json!([
        grant_op("notes", &["select"], "anon"),
        grant_op("notes", &["select"], &rr),
        grant_op("notes", &["select"], &pr),
        default_select_op(&o, true, "authenticated"),
        revoke_op("notes", &["select"], "anon"),
    ]);
    let dry = answer(t.changes(main, json!({ "dry_run": true, "operations": ops })).await, 200).await;
    assert_eq!(
        dry["data_api_exposure"],
        json!([
            { "op_index": 0, "grantee": "anon", "chain": ["anon"] },
            { "op_index": 1, "grantee": rr, "chain": ["authenticated", rr] },
            { "op_index": 3, "grantee": "authenticated", "chain": ["authenticated"] },
        ]),
        "{dry}"
    );
    let plain = answer(t.changes(main, json!({ "dry_run": true, "operations": [grant_op("notes", &["select"], &pr)] })).await, 200).await;
    assert_eq!(plain["data_api_exposure"], json!([]), "{plain}");
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[rr, pr]).await;
}

// MH-31: MAINTAIN is offered and granted on Postgres 17 (the render's `server_version_num` is
// the compute's, not 0).
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn maintain_is_granted_on_postgres_17() {
    let (scratch, t) = changes_app("mnt-pass").await;
    let main = t.main();
    let o = scratch.owner.clone();
    let [reader] = scratch.make_roles(["reader"]).await;
    let real = answer(t.changes(main, json!({ "operations": [grant_op("notes", &["maintain"], &reader)] })).await, 200).await;
    assert_eq!(real["committed"], true);
    assert_eq!(scratch.acl("public.notes", &reader).await, format!("{o}:MAINTAIN"));
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[reader]).await;
}

// MH-30 / P13: every committed privilege apply writes one `privileges_changed` (database,
// branch, project; `after: {pg_database, acted_as, ops: [{op, object}]}`), never SQL text, in
// events or logs; a dry run, a refused request and a request without privilege ops write none.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn every_privilege_apply_writes_privileges_changed_without_sql() {
    let logs = Logs::default();
    let _guard = logs.capture();
    let (scratch, t) = changes_app("evt-pass").await;
    let main = t.main();
    let o = scratch.owner.clone();
    let [reader] = scratch.make_roles(["reader"]).await;
    let ops = json!([schema_usage_op(&reader), grant_op("notes", &["select"], &reader)]);
    answer(t.changes(main, json!({ "dry_run": true, "operations": ops.clone() })).await, 200).await;
    assert!(t.privilege_events().is_empty(), "a dry run writes none");
    answer(t.changes(main, json!({ "operations": ops })).await, 200).await;
    let events = t.privilege_events();
    assert_eq!(events.len(), 1, "{events:?}");
    let e = &events[0];
    let record = t.metadata.database(t.db);
    assert_eq!(e.database_id, Some(t.db.as_uuid()));
    assert_eq!(e.branch_id, Some(main.as_uuid()));
    assert_eq!(e.project_id, Some(record.project_id.as_uuid()));
    assert!(e.actor_member_id.is_some(), "{e:?}");
    let after = e.after.clone().unwrap();
    assert_eq!(after["pg_database"], scratch.database.as_str(), "{after}");
    assert_eq!(
        after["ops"],
        json!([
            { "op": "grant_schema", "object": { "schema": { "schema": "public" } } },
            { "op": "grant", "object": relation_target("notes") },
        ]),
        "{after}"
    );
    let text = after.to_string();
    for sql in ["GRANT", "REVOKE", "USAGE ON", " ON SCHEMA", " TO "] {
        assert!(!text.contains(sql), "SQL in the event: {text}");
    }
    // A refused request (the owner as grantee) and one without privilege ops write none.
    answer(t.changes(main, json!({ "operations": [grant_op("notes", &["select"], &o)] })).await, 400).await;
    answer(t.changes(main, json!({ "operations": [comment_op("notes")] })).await, 200).await;
    assert_eq!(t.privilege_events().len(), 1);
    let logged = logs.text();
    assert!(!logged.contains("GRANT SELECT") && !logged.contains("GRANT USAGE"), "SQL in the logs:\n{logged}");
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[reader]).await;
}

// MH-30: a Developer's privilege op acting as a role does what Postgres lets that role do and
// is logged too (actor, `acted_as`); the same Developer's owner run is refused before anything.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_developer_run_as_a_role_is_logged_too() {
    let (scratch, t) = changes_app("devlog-pass").await;
    let main = t.main();
    let [grp, g] = scratch.make_roles(["grp", "g"]).await;
    let (a, member_role) = developer_acting_as(&t, &scratch, &grp).await;
    scratch.exec_in(&format!("CREATE TABLE public.gt (id int); ALTER TABLE public.gt OWNER TO {grp};")).await;
    let path = format!("{}/schema-changes", t.branch(main));
    let body = json!({ "role": grp, "operations": [grant_op("gt", &["select"], &g)] });
    let real = answer(t.send_as("dev-a", Method::POST, &path, Some(body)).await, 200).await;
    assert_eq!(real["committed"], true);
    let events = t.privilege_events();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].actor_member_id, Some(a));
    let after = events[0].after.clone().unwrap();
    assert_eq!((after["acted_as"].as_str(), after["pg_database"].as_str()), (Some(grp.as_str()), Some(scratch.database.as_str())), "{after}");
    assert_eq!(scratch.acl("public.gt", &g).await, format!("{grp}:SELECT"));
    let owner_run = json!({ "operations": [grant_op("gt", &["insert"], &g)] });
    let err = answer(t.send_as("dev-a", Method::POST, &path, Some(owner_run)).await, 403).await;
    assert_eq!(err["error"]["required"], "admin");
    assert_eq!(t.privilege_events().len(), 1);
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[member_role, grp, g]).await;
}

// --- Table API: rows (TE-A, T8) -----------------------------------------------------------

impl Branching {
    async fn table_post(&self, branch: BranchId, rest: &str, body: Value) -> reqwest::Response {
        self.send(Method::POST, &format!("{}/tables/{rest}", self.branch(branch)), Some(body)).await
    }
}

#[tokio::test]
async fn row_routes_validate_before_any_compute() {
    let t = sql_app().await;
    let main = t.main();
    assert_eq!(t.table_post(main, "public/notes/cell", json!({ "key": { "id": 1 } })).await.status(), 422);
    assert_eq!(t.table_post(main, "public/notes/rows/default", json!({ "database": "nope" })).await.status(), 404);
    assert_eq!(t.compute_lookups(), 0);
    assert_eq!(code_of(&error_of(t.table_post(main, "public/notes/rows/default", json!({})).await, 503).await), "unavailable");
}

// --- T5 (Q1, P40): a full-cell read as a role held below Developer goes through rest --------

/// A branching app whose rest is `rest`, with the console's seeded databases (`web1` main and
/// on, `analytics` and `scratch` off), the owner `postgres`, group roles `billing`,
/// `billing_admin` (which can become `billing`), `sales` and `boss` on main, and `mixed`: a
/// Developer through their own grant (`sales`) and a Viewer through a team's grant
/// (`billing_admin`, `boss`). Their member role holds `sales` only (T2-2).
async fn cell_app_at(rest: &str) -> (Branching, uuid::Uuid) {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app_with_rest(metadata.clone(), jobs.clone(), rest).await;
    let db = ready_database(tenant, &app.key);
    let id = db.database_id;
    metadata.insert_database(db);
    let t = Branching { app, metadata, jobs, db: id };
    let main = t.seeded();
    let ep = t.metadata.branch(main).endpoint_id.unwrap();
    t.metadata.add_role(&ep, "postgres", RoleOrigin::Owner, true, false);
    t.metadata.add_role(&ep, "editor", RoleOrigin::Api, false, false);
    t.metadata.add_group_role(&ep, "billing", &[]);
    t.metadata.add_group_role(&ep, "billing_admin", &["billing"]);
    t.metadata.add_group_role(&ep, "sales", &[]);
    t.metadata.add_group_role(&ep, "boss", &[]);
    let mixed = t.grant_member("mixed", PlatformRole::Developer, &["sales"]);
    let project = t.metadata.database(t.db).project_id;
    t.metadata.team_grant(
        mixed,
        project,
        Grant {
            platform_role: PlatformRole::Viewer,
            roles: chelabase_types::access::RoleSet::Only(
                [PgRole::Named("billing_admin".into()), PgRole::Named("boss".into())].into_iter().collect(),
            ),
        },
    );
    t.put_member_role(t.member_role_row(mixed, main, &["sales"]));
    (t, mixed)
}

/// [`cell_app_at`] with the rest stand-in answering `answer`.
async fn cell_app(answer: RestAnswer) -> (Branching, RestCalls) {
    let (rest, calls) = mock_rest_console(answer).await;
    let (t, _) = cell_app_at(&rest).await;
    (t, calls)
}

impl Branching {
    /// `POST …/tables/public/notes/cell` as `sub`.
    async fn cell_as(&self, sub: &str, branch: BranchId, body: Value) -> reqwest::Response {
        self.send_as(sub, Method::POST, &format!("{}/tables/public/notes/cell", self.branch(branch)), Some(body)).await
    }
}

fn cell_of_id() -> Value {
    json!({ "value": "1", "bytes": 4, "data_type": "integer" })
}

/// rest's 403 for a column the role can't select (the management body, P40).
fn column_refused() -> Value {
    json!({ "error": {
        "code": "permission_denied", "message": "permission denied for table notes", "op_index": null,
        "sqlstate": "42501", "detail": null, "hint": null, "position": null
    } })
}

// A Viewer-level team role reads a full cell through rest's console path: the stand-in gets
// `/internal/console/{branch}/cell` with the bearer and `{role, database, schema, table, key,
// column}` (main's database by name, as the console sends it), the worker's role check runs
// once, and no member-role session is opened (no compute lookup). A column the role lacks is
// rest's 403 as it came. A role reached only through the Viewer-level chain goes to rest as
// itself. The access refusals stay where they were, before rest.
#[tokio::test]
async fn get_cell_below_developer_goes_through_rest() {
    let answer: RestAnswer = Arc::new(|request: &Value| match request["column"].as_str() {
        Some("id") => (200, vec![], cell_of_id()),
        _ => (403, vec![], column_refused()),
    });
    let (t, calls) = cell_app(answer).await;
    let main = t.main();
    let resp = t.cell_as("mixed", main, json!({ "role": "billing_admin", "key": { "id": 1 }, "column": "id" })).await;
    assert_eq!(resp.status(), 200, "the read went to rest");
    assert_eq!(resp.json::<Value>().await.unwrap(), cell_of_id());
    {
        let calls = calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0].path, format!("/internal/console/{}/cell", main.as_uuid()));
        assert!(calls[0].headers.contains(&format!("authorization: Bearer {REST_TOKEN}")), "{}", calls[0].headers);
        let body: Value = serde_json::from_str(&calls[0].body).unwrap();
        assert_eq!(
            body,
            json!({ "role": "billing_admin", "database": "web1", "schema": "public", "table": "notes",
                    "key": { "id": 1 }, "column": "id" })
        );
    }
    assert_eq!(t.compute_lookups(), 0, "no member-role session");
    assert_eq!(t.role_checks(), vec![(main, "billing_admin".to_string())]);

    // A column the role lacks: rest's refusal, unchanged.
    let resp = t.cell_as("mixed", main, json!({ "role": "billing_admin", "key": { "id": 1 }, "column": "body" })).await;
    assert_eq!(error_of(resp, 403).await, column_refused());

    // Reached only through `billing_admin`'s chain (Viewer level): rest, as `billing`, with the
    // database named.
    let resp = t
        .cell_as("mixed", main, json!({ "role": "billing", "database": "web1", "key": { "id": 1 }, "column": "id" }))
        .await;
    assert_eq!(resp.status(), 200);
    let last: Value = serde_json::from_str(&calls.lock().unwrap().last().unwrap().body).unwrap();
    assert_eq!((last["role"].as_str(), last["database"].as_str()), (Some("billing"), Some("web1")), "{last}");
    assert_eq!(calls.lock().unwrap().len(), 3);
    assert_eq!(t.compute_lookups(), 0);

    // Refused before rest, as today: a reserved name (422), a role not in the set and the
    // owner's name (403 `forbidden`), and an owner-level role the worker refuses (403
    // `role_refused`, not remembered).
    let refused = |role: &'static str| json!({ "role": role, "key": { "id": 1 }, "column": "id" });
    let err = error_of(t.cell_as("mixed", main, refused("authenticator")).await, 422).await;
    assert_eq!(code_of(&err), "invalid_request");
    for role in ["editor", "postgres"] {
        let err = error_of(t.cell_as("mixed", main, refused(role)).await, 403).await;
        assert_eq!(code_of(&err), "forbidden", "{role}");
    }
    *t.jobs.role_check.lock().unwrap() = Some(Ok(CheckDataRoleReply::refused("the role `boss` can create databases")));
    for _ in 0..2 {
        let err = error_of(t.cell_as("mixed", main, refused("boss")).await, 403).await;
        assert_eq!(code_of(&err), "role_refused");
    }
    assert_eq!(calls.lock().unwrap().len(), 3, "no refusal reached rest");
    assert_eq!(t.compute_lookups(), 0);
}

// A role held at Developer (`sales`, the member's own grant) keeps today's member-role session:
// the compute is looked up (503 in the fakes), rest is never asked and no role check runs, with
// the data API on or off.
#[tokio::test]
async fn get_cell_at_developer_keeps_the_member_role_path() {
    let (t, calls) = cell_app(rest_ok(cell_of_id())).await;
    let main = t.main();
    let body = json!({ "role": "sales", "key": { "id": 1 }, "column": "id" });
    let resp = t.cell_as("mixed", main, body.clone()).await;
    assert_eq!(code_of(&error_of(resp, 503).await), "unavailable");
    assert_eq!(t.compute_lookups(), 1);
    t.metadata.set_data_api(main, "web1", chelabase_metadata::DataApiState::Off);
    let resp = t.cell_as("mixed", main, body).await;
    assert_eq!(code_of(&error_of(resp, 503).await), "unavailable");
    assert_eq!(t.compute_lookups(), 2);
    assert!(calls.lock().unwrap().is_empty(), "{:?}", calls.lock().unwrap());
    assert!(t.role_checks().is_empty());
}

// `role: None` (the owner) never goes to rest: the tenant admin reads on the owner's session; a
// Viewer is refused as today. The tenant admin naming a role holds it at Admin: member path too.
#[tokio::test]
async fn get_cell_as_the_owner_never_calls_rest() {
    let (t, calls) = cell_app(rest_ok(cell_of_id())).await;
    let main = t.main();
    let path = format!("{}/tables/public/notes/cell", t.branch(main));
    let resp = t.send(Method::POST, &path, Some(json!({ "key": { "id": 1 }, "column": "id" }))).await;
    assert_eq!(code_of(&error_of(resp, 503).await), "unavailable");
    assert_eq!(t.compute_lookups(), 1);
    let resp = t.send(Method::POST, &path, Some(json!({ "role": "billing", "key": { "id": 1 }, "column": "id" }))).await;
    assert_ne!(resp.status(), 200, "the fakes have no compute");
    t.grant_member("vic", PlatformRole::Viewer, &["billing_admin"]);
    let err = error_of(t.cell_as("vic", main, json!({ "key": { "id": 1 }, "column": "id" })).await, 403).await;
    assert_eq!(code_of(&err), "forbidden");
    assert!(calls.lock().unwrap().is_empty(), "{:?}", calls.lock().unwrap());
    assert!(t.role_checks().is_empty());
}

// The rest path needs the data API on the picked database: off is 409 `data_api_disabled`
// before the role check and before rest; an unknown database is 404. A database turned off
// between the check and rest's answer (rest says `unknown_database`) is re-read: 409 too.
#[tokio::test]
async fn get_cell_through_rest_needs_the_data_api() {
    let (t, calls) = cell_app(rest_ok(cell_of_id())).await;
    let main = t.main();
    t.metadata.set_data_api(main, "web1", chelabase_metadata::DataApiState::Off);
    for database in [Value::Null, json!("web1"), json!("analytics")] {
        let body = json!({ "role": "billing_admin", "database": database, "key": { "id": 1 }, "column": "id" });
        let err = error_of(t.cell_as("mixed", main, body).await, 409).await;
        assert_eq!(code_of(&err), "data_api_disabled", "{database}");
    }
    let body = json!({ "role": "billing_admin", "database": "nope", "key": { "id": 1 }, "column": "id" });
    assert_eq!(code_of(&error_of(t.cell_as("mixed", main, body).await, 404).await), "not_found");
    assert!(calls.lock().unwrap().is_empty(), "{:?}", calls.lock().unwrap());
    assert!(t.role_checks().is_empty(), "{:?}", t.role_checks());
    assert_eq!(t.compute_lookups(), 0);

    // Turned off while the request was on its way: rest's `unknown_database`, re-read.
    type Slot = Arc<Mutex<Option<(FakeMetadata, BranchId)>>>;
    let slot: Slot = Arc::default();
    let seen = slot.clone();
    let answer: RestAnswer = Arc::new(move |_| {
        if let Some((metadata, branch)) = seen.lock().unwrap().as_ref() {
            metadata.set_data_api(*branch, "web1", chelabase_metadata::DataApiState::Off);
        }
        (404, vec![], json!({ "error": { "code": "unknown_database", "message": "the database doesn't exist or isn't served by this key" } }))
    });
    let (t, calls) = cell_app(answer).await;
    let main = t.main();
    *slot.lock().unwrap() = Some((t.metadata.clone(), main));
    let resp = t.cell_as("mixed", main, json!({ "role": "billing_admin", "key": { "id": 1 }, "column": "id" })).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "data_api_disabled");
    assert_eq!(calls.lock().unwrap().len(), 1);
}

// rest's answers come back as they are (status, body, `Retry-After`); rest unreachable is 503
// `unavailable` with `Retry-After`.
#[tokio::test]
async fn get_cell_through_rest_passes_its_answer_on() {
    let answer: RestAnswer = Arc::new(|request: &Value| match request["column"].as_str() {
        Some("body") => (403, vec![], column_refused()),
        _ => (503, vec![("retry-after", "2")], json!({ "error": { "code": "unavailable", "message": "try again" } })),
    });
    let (t, _) = cell_app(answer).await;
    let main = t.main();
    let resp = t.cell_as("mixed", main, json!({ "role": "billing_admin", "key": { "id": 1 }, "column": "body" })).await;
    assert_eq!(error_of(resp, 403).await, column_refused());
    let resp = t.cell_as("mixed", main, json!({ "role": "billing_admin", "key": { "id": 1 }, "column": "id" })).await;
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.headers()["retry-after"], "2");
    assert_eq!(resp.json::<Value>().await.unwrap(), json!({ "error": { "code": "unavailable", "message": "try again" } }));
    assert_eq!(t.compute_lookups(), 0);

    // Nothing listens where rest should be.
    let dead = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };
    let (t, _) = cell_app_at(&dead).await;
    let main = t.main();
    let resp = t.cell_as("mixed", main, json!({ "role": "billing_admin", "key": { "id": 1 }, "column": "id" })).await;
    assert_eq!(resp.status(), 503);
    assert!(resp.headers().contains_key("retry-after"));
    assert_eq!(code_of(&resp.json::<Value>().await.unwrap()), "unavailable");
    assert_eq!(t.compute_lookups(), 0);
}

// #26: rest's client never follows a redirect, so a 3xx from rest is no answer: it is 503
// `unavailable`, never passed on (with no `location`, it would read as an empty success).
#[tokio::test]
async fn a_redirect_from_rest_is_unavailable_not_passed_on() {
    for status in [301u16, 302, 303, 307, 308] {
        let answer: RestAnswer =
            Arc::new(move |_: &Value| (status, vec![("location", "http://elsewhere.invalid/")], json!({})));
        let (t, calls) = cell_app(answer).await;
        let main = t.main();
        let resp = t.cell_as("mixed", main, json!({ "role": "billing_admin", "key": { "id": 1 }, "column": "id" })).await;
        assert_eq!(resp.status(), 503, "a {status} from rest was passed on");
        assert_eq!(code_of(&resp.json::<Value>().await.unwrap()), "unavailable", "{status}");
        assert_eq!(calls.lock().unwrap().len(), 1, "{status}: rest asked once, nothing followed");
    }
}

#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn default_rows_and_full_cells_over_http() {
    let (scratch, t) = changes_app("rows-pass").await;
    let main = t.main();
    scratch.exec_in(&format!(
        "CREATE TABLE public.d (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, n int DEFAULT 7, at timestamptz DEFAULT now()); ALTER TABLE public.d OWNER TO {o}; \
         CREATE TABLE public.strict (id int PRIMARY KEY, must text NOT NULL); ALTER TABLE public.strict OWNER TO {o}; \
         CREATE TABLE public.nokey (v text); ALTER TABLE public.nokey OWNER TO {o}; \
         INSERT INTO public.notes (body) VALUES (repeat('x', 100000));",
        o = scratch.owner
    ))
    .await;
    let resp = t.table_post(main, "public/d/rows/default", json!({})).await;
    assert_eq!(resp.status(), 200);
    let row: Value = resp.json().await.unwrap();
    assert_eq!((row["row"]["id"].as_i64(), row["row"]["n"].as_i64()), (Some(1), Some(7)), "{row}");
    assert!(row["row"]["at"].is_string());
    let err = error_of(t.table_post(main, "public/strict/rows/default", json!({})).await, 400).await;
    assert_eq!(code_of(&err), "constraint_violation");
    let resp = t.table_post(main, "public/notes/cell", json!({ "key": { "id": 1 }, "column": "body" })).await;
    assert_eq!(resp.status(), 200);
    let cell: Value = resp.json().await.unwrap();
    assert_eq!(cell["value"].as_str().map(str::len), Some(100_000));
    assert_eq!(cell["data_type"], "text");
    assert!(cell["bytes"].as_i64().unwrap() > 0);
    let resp = t.table_post(main, "public/notes/cell", json!({ "key": { "id": "1" }, "column": "id" })).await;
    assert_eq!(resp.json::<Value>().await.unwrap()["value"], "1");
    for (rest, body, status, code) in [
        ("public/notes/cell", json!({ "key": { "id": 99 }, "column": "body" }), 404, "not_found"),
        ("public/notes/cell", json!({ "key": { "body": "x" }, "column": "body" }), 400, "invalid_request"),
        ("public/notes/cell", json!({ "key": { "id": 1 }, "column": "nope" }), 400, "unknown_column"),
        ("public/nokey/cell", json!({ "key": { "v": "x" }, "column": "v" }), 400, "invalid_request"),
        ("public/ghost/cell", json!({ "key": { "id": 1 }, "column": "v" }), 404, "not_found"),
        ("public/notes/cell", json!({ "key": { "id": [1] }, "column": "body" }), 400, "invalid_request"),
    ] {
        assert_eq!(code_of(&error_of(t.table_post(main, rest, body.clone()).await, status).await), code, "{rest} {body}");
    }
    wait_no_editor_sessions(&scratch).await;
    scratch.drop().await;
}

// --- T8 review, fix round ------------------------------------------------------------------

// Important: the 64 MiB cap is on the value's text, so a compressible value stored small but
// over the cap as text answers 413 without being fetched.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_compressible_cell_over_the_cap_is_too_large() {
    let (scratch, t) = changes_app("cap-pass").await;
    let main = t.main();
    scratch.exec_in("INSERT INTO public.notes (body) VALUES (repeat('x', 70 * 1024 * 1024))").await;
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        t.table_post(main, "public/notes/cell", json!({ "key": { "id": 1 }, "column": "body" })),
    )
    .await
    .expect("answered");
    assert_eq!(code_of(&error_of(resp, 413).await), "too_large");
    wait_no_editor_sessions(&scratch).await;
    scratch.drop().await;
}

// Minor 4: the all-defaults insert, like the cell fetch, works on ordinary tables only.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn default_rows_only_go_into_ordinary_tables() {
    let (scratch, t) = changes_app("kind-pass").await;
    let main = t.main();
    scratch
        .exec_in(&format!(
            "CREATE VIEW public.notes_v AS SELECT * FROM public.notes; ALTER VIEW public.notes_v OWNER TO {o}; \
             CREATE TABLE public.p (id int DEFAULT 1) PARTITION BY RANGE (id); CREATE TABLE public.p1 PARTITION OF public.p FOR VALUES FROM (0) TO (10); \
             ALTER TABLE public.p OWNER TO {o}; ALTER TABLE public.p1 OWNER TO {o};",
            o = scratch.owner
        ))
        .await;
    for rest in ["public/notes_v/rows/default", "public/p/rows/default"] {
        assert_eq!(code_of(&error_of(t.table_post(main, rest, json!({})).await, 404).await), "not_found", "{rest}");
    }
    assert!(scratch.yes("SELECT NOT EXISTS (SELECT FROM public.notes) AND NOT EXISTS (SELECT FROM public.p)").await, "nothing inserted");
    wait_no_editor_sessions(&scratch).await;
    scratch.drop().await;
}

// --- End-of-step review, fix round ---------------------------------------------------------

// Finding 4: a key is compared as given, never truncated or rounded to the column's typmod.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn cell_keys_are_not_truncated_by_the_column_type() {
    let (scratch, t) = changes_app("key-pass").await;
    let main = t.main();
    scratch
        .exec_in(&format!(
            "CREATE TABLE public.codes (code varchar(5) PRIMARY KEY, v text); ALTER TABLE public.codes OWNER TO {o}; \
             INSERT INTO public.codes VALUES ('abcde', 'five'); \
             CREATE TABLE public.prices (p numeric(10,2) PRIMARY KEY, v text); ALTER TABLE public.prices OWNER TO {o}; \
             INSERT INTO public.prices VALUES (1.25, 'one quarter');",
            o = scratch.owner
        ))
        .await;
    for (rest, key) in [("public/codes/cell", json!({ "code": "abcdefg" })), ("public/prices/cell", json!({ "p": 1.249 }))] {
        let body = json!({ "key": key, "column": "v" });
        assert_eq!(code_of(&error_of(t.table_post(main, rest, body.clone()).await, 404).await), "not_found", "{rest} {body}");
    }
    let resp = t.table_post(main, "public/codes/cell", json!({ "key": { "code": "abcde" }, "column": "v" })).await;
    assert_eq!(resp.json::<Value>().await.unwrap()["value"], "five");
    wait_no_editor_sessions(&scratch).await;
    scratch.drop().await;
}

// Finding 5: the inserted row comes back as Postgres wrote it: numeric digits kept exactly.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn default_rows_keep_numeric_digits() {
    let (scratch, t) = changes_app("num-pass").await;
    let main = t.main();
    scratch
        .exec_in(&format!(
            "CREATE TABLE public.nums (id int GENERATED ALWAYS AS IDENTITY PRIMARY KEY, a numeric DEFAULT 1.10, \
             b numeric DEFAULT 12345678901234567890123.000001); ALTER TABLE public.nums OWNER TO {};",
            scratch.owner
        ))
        .await;
    let resp = t.table_post(main, "public/nums/rows/default", json!({})).await;
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(text.contains("1.10"), "{text}");
    assert!(text.contains("12345678901234567890123.000001"), "{text}");
    wait_no_editor_sessions(&scratch).await;
    scratch.drop().await;
}

// --- T10 (user rulings R10, R12): the table API's reads require CONNECT --------------------

// R12: a member's reads are checked as Postgres checks their login: the member's own login
// (`cb_m_*`, NOINHERIT, its roles granted WITH INHERIT FALSE) must hold CONNECT on the
// database, through PUBLIC or a grant to it. A group's CONNECT doesn't reach it, as the SQL
// editor's login would find. The tenant admin reads as the owner.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn table_reads_need_connect_for_the_members_login() {
    let (scratch, t) = changes_app("conn-pass").await;
    let main = t.main();
    let ep = t.metadata.branch(main).endpoint_id.unwrap();
    let group = format!("{}_grp", scratch.owner);
    t.metadata.add_group_role(&ep, &group, &[]);
    let vic = t.grant_member("vic", PlatformRole::Viewer, &[&group]);
    let login = chelabase_types::role_name::member_role_name(vic);
    scratch
        .exec_in(&format!(
            "CREATE ROLE {group} NOLOGIN; CREATE ROLE {login} LOGIN NOINHERIT; \
             GRANT {group} TO {login} WITH INHERIT FALSE, SET TRUE; \
             REVOKE CONNECT ON DATABASE {db} FROM PUBLIC; GRANT CONNECT ON DATABASE {db} TO {group};",
            db = scratch.database
        ))
        .await;
    let get = |sub: &'static str, rest: &'static str| {
        let path = format!("{}/{rest}", t.branch(main));
        let t = &t;
        async move { t.send_as(sub, Method::GET, &path, None).await }
    };
    for rest in ["objects", "types", "tables/public/notes"] {
        let err = error_of(get("vic", rest).await, 403).await;
        assert_eq!(code_of(&err), "permission_denied", "{rest}");
        assert_eq!(get("user-1", rest).await.status(), 200, "{rest}");
    }
    scratch.exec_in(&format!("GRANT CONNECT ON DATABASE {db} TO {login};", db = scratch.database)).await;
    for rest in ["objects", "types", "tables/public/notes"] {
        assert_eq!(get("vic", rest).await.status(), 200, "{rest}");
    }
    wait_no_editor_sessions(&scratch).await;
    let admin = scratch.admin.clone();
    scratch.drop().await;
    use sqlx::Executor;
    admin.execute(sqlx::AssertSqlSafe(format!("DROP ROLE {login}"))).await.unwrap();
    admin.execute(sqlx::AssertSqlSafe(format!("DROP ROLE {group}"))).await.unwrap();
}

// R12: a member with no login on the branch yet (not reconciled, or no roles to act as) can't
// connect, so their reads are refused even while PUBLIC holds CONNECT.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn table_reads_refuse_a_member_with_no_login_on_the_branch() {
    let (scratch, t) = changes_app("conn-login").await;
    let main = t.main();
    let ep = t.metadata.branch(main).endpoint_id.unwrap();
    let group = format!("{}_grp", scratch.owner);
    t.metadata.add_group_role(&ep, &group, &[]);
    let vic = t.grant_member("vic", PlatformRole::Viewer, &[&group]);
    let login = chelabase_types::role_name::member_role_name(vic);
    scratch.exec_in(&format!("CREATE ROLE {group} NOLOGIN;")).await;
    for rest in ["objects", "types", "tables/public/notes"] {
        let path = format!("{}/{rest}", t.branch(main));
        let err = error_of(t.send_as("vic", Method::GET, &path, None).await, 403).await;
        assert_eq!(code_of(&err), "permission_denied", "{rest}");
    }
    scratch
        .exec_in(&format!("CREATE ROLE {login} LOGIN NOINHERIT; GRANT {group} TO {login} WITH INHERIT FALSE, SET TRUE;"))
        .await;
    for rest in ["objects", "types", "tables/public/notes"] {
        let path = format!("{}/{rest}", t.branch(main));
        assert_eq!(t.send_as("vic", Method::GET, &path, None).await.status(), 200, "{rest}");
    }
    wait_no_editor_sessions(&scratch).await;
    let admin = scratch.admin.clone();
    scratch.drop().await;
    use sqlx::Executor;
    admin.execute(sqlx::AssertSqlSafe(format!("DROP ROLE {login}"))).await.unwrap();
    admin.execute(sqlx::AssertSqlSafe(format!("DROP ROLE {group}"))).await.unwrap();
}

// TE-B1 browser pass C1 (ruling R16): a member whose login isn't on the branch yet (the SQL
// editor's lazy reconcile never ran there) gets it reconciled by the read itself, once, before
// the CONNECT check; a login that exists but lacks CONNECT asks for no reconcile.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn table_reads_reconcile_a_missing_member_login_first() {
    let (scratch, t) = changes_app("conn-reconcile").await;
    let main = t.main();
    let ep = t.metadata.branch(main).endpoint_id.unwrap();
    let group = format!("{}_grp", scratch.owner);
    t.metadata.add_group_role(&ep, &group, &[]);
    let dev = t.grant_member("dev", PlatformRole::Developer, &[&group]);
    let login = chelabase_types::role_name::member_role_name(dev);
    scratch.exec_in(&format!("CREATE ROLE {group} NOLOGIN;")).await;
    let path = format!("{}/objects", t.branch(main));
    // No login yet: the read asks the worker to reconcile this member on this branch.
    let _ = t.send_as("dev", Method::GET, &path, None).await;
    let asked: Vec<_> = t.jobs.member_reconciles.lock().unwrap().clone();
    assert_eq!(asked.len(), 1, "one reconcile for the missing login: {asked:?}");
    assert_eq!((asked[0].branch_id, asked[0].member_id), (main.as_uuid(), Some(dev)));
    // Once the login exists (as the worker would leave it), the read passes with no new reconcile.
    scratch
        .exec_in(&format!("CREATE ROLE {login} LOGIN NOINHERIT; GRANT {group} TO {login} WITH INHERIT FALSE, SET TRUE;"))
        .await;
    assert_eq!(t.send_as("dev", Method::GET, &path, None).await.status(), 200);
    assert_eq!(t.jobs.member_reconciles.lock().unwrap().len(), 1, "an existing login needs no reconcile");
    // A login without CONNECT is refused as before, with no reconcile.
    scratch
        .exec_in(&format!("REVOKE CONNECT ON DATABASE {db} FROM PUBLIC;", db = scratch.database))
        .await;
    let err = error_of(t.send_as("dev", Method::GET, &path, None).await, 403).await;
    assert_eq!(code_of(&err), "permission_denied");
    assert_eq!(t.jobs.member_reconciles.lock().unwrap().len(), 1);
    wait_no_editor_sessions(&scratch).await;
    let admin = scratch.admin.clone();
    scratch.drop().await;
    use sqlx::Executor;
    admin.execute(sqlx::AssertSqlSafe(format!("DROP ROLE {login}"))).await.unwrap();
    admin.execute(sqlx::AssertSqlSafe(format!("DROP ROLE {group}"))).await.unwrap();
}

// --- Protected branches: switch, exemptions, review (PB, T4) -------------------------------

#[tokio::test]
async fn the_switch_and_exemptions_round_trip() {
    let t = sql_app().await;
    let main = t.main();
    let body: Value = t.send(Method::PATCH, &t.branch(main), Some(json!({ "guard_logins": false }))).await.json().await.unwrap();
    assert_eq!(body["guard_logins"], false);
    assert!(!t.metadata.branch(main).guard_logins);
    let list = |resp: reqwest::Response| async move { resp.json::<Value>().await.unwrap()["exemptions"].clone() };
    let exemptions = format!("{}/guard-exemptions", t.branch(main));
    assert_eq!(list(t.send(Method::GET, &exemptions, None).await).await, json!([]));
    assert_eq!(list(t.send(Method::PUT, &format!("{exemptions}/editor"), None).await).await, json!(["editor"]));
    assert_eq!(list(t.send(Method::PUT, &format!("{exemptions}/editor"), None).await).await, json!(["editor"]), "idempotent");
    assert_eq!(list(t.send(Method::PUT, &format!("{exemptions}/Ci%20Bot"), None).await).await, json!(["Ci Bot", "editor"]));
    // The role list says which login roles are exempt.
    let roles: Value = t.send(Method::GET, &format!("{}/roles", t.branch(main)), None).await.json().await.unwrap();
    let exempt = |name: &str| roles.as_array().unwrap().iter().find(|r| r["name"] == name).unwrap()["guard_exempt"].clone();
    assert_eq!((exempt("editor"), exempt("postgres")), (json!(true), json!(false)));
    assert_eq!(list(t.send(Method::DELETE, &format!("{exemptions}/editor"), None).await).await, json!(["Ci Bot"]));
    assert_eq!(list(t.send(Method::DELETE, &format!("{exemptions}/editor"), None).await).await, json!(["Ci Bot"]), "idempotent");
    // A name of 64 bytes: 422 (P10).
    let err = error_of(t.send(Method::PUT, &format!("{exemptions}/{}", "a".repeat(64)), None).await, 422).await;
    assert_eq!(code_of(&err), "invalid_request");
    // Unknown branch: 404.
    assert_eq!(t.send(Method::GET, &format!("{}/guard-exemptions", t.branch(BranchId::new())), None).await.status(), 404);
}

#[tokio::test]
async fn guard_writes_need_admin_and_reads_need_developer() {
    let (t, _dev) = act_as_app().await;
    let main = t.main();
    let exemptions = format!("{}/guard-exemptions", t.branch(main));
    for (method, path, body) in [
        (Method::PATCH, t.branch(main), Some(json!({ "guard_logins": false }))),
        (Method::PUT, format!("{exemptions}/deploy"), None),
        (Method::DELETE, format!("{exemptions}/deploy"), None),
        (Method::POST, format!("{}/review/acknowledge", t.branch(main)), Some(json!({ "hash": "h" }))),
        (Method::POST, format!("{}/review/fix", t.branch(main)), Some(json!({ "key": "k", "action": "drop", "hash": "h" }))),
    ] {
        let err = error_of(t.send_as("dev", method.clone(), &path, body).await, 403).await;
        assert_eq!((code_of(&err), err["error"]["required"].as_str()), ("forbidden", Some("admin")), "{method} {path}");
    }
    assert_eq!(t.send_as("dev", Method::GET, &exemptions, None).await.status(), 200);
    assert!(t.metadata.branch(main).guard_logins, "unchanged");
    // The review read is allowed for a Developer (it needs a compute: the fake has none).
    let err = error_of(t.send_as("dev", Method::GET, &format!("{}/review", t.branch(main)), None).await, 503).await;
    assert_eq!(code_of(&err), "unavailable");
}

#[tokio::test]
async fn review_requests_validate_before_any_compute() {
    let t = sql_app().await;
    let main = t.main();
    for (path, body) in [
        ("review/acknowledge", json!({})),
        ("review/acknowledge", json!({ "hash": "h", "x": 1 })),
        ("review/fix", json!({ "key": "k", "action": "burn", "hash": "h" })),
        ("review/fix", json!({ "key": "k", "action": "drop" })),
    ] {
        let err = error_of(t.send(Method::POST, &format!("{}/{path}", t.branch(main)), Some(body.clone())).await, 422).await;
        assert_eq!(code_of(&err), "invalid_request", "{path} {body}");
    }
    assert_eq!(t.compute_lookups(), 0);
}

// --- The review goes through the worker (PB T4 fix round, R8/R10) ---------------------------

const REVIEW_DOC: &str = r#"[
  {"kind":"function","schema":"public","name":"planted","identity":"","function":"public.planted()","owner":"dev","reasons":[{"kind":"security_definer","via":null}],"opaque_body":true},
  {"kind":"function","schema":"public","name":"later","identity":"","function":"public.later()","owner":"dev","reasons":[{"kind":"security_definer","via":null}],"opaque_body":true}
]"#;

/// The two planted functions of [`REVIEW_DOC`] in database `web1` (`later` first: keys sort).
fn review_items() -> Vec<chelabase_types::protection::ReviewItem> {
    chelabase_schema::review::parse_review("web1", REVIEW_DOC).unwrap()
}

impl Branching {
    fn review_answers(&self, reply: chelabase_nats::ReviewReply) {
        *self.jobs.review_reply.lock().unwrap() = Some(Ok(reply));
    }

    fn review_requests(&self) -> Vec<chelabase_nats::ReviewRequest> {
        self.jobs.review_requests.lock().unwrap().clone()
    }
}

#[tokio::test]
async fn the_review_comes_from_the_worker() {
    let t = branching().await;
    let main = t.main();
    let items = review_items();
    t.review_answers(chelabase_nats::ReviewReply::Scanned { items: items.clone() });
    let body: Value = t.send(Method::GET, &format!("{}/review", t.branch(main)), None).await.json().await.unwrap();
    assert_eq!(body["hash"], chelabase_schema::review::review_hash(&items).as_str());
    assert_eq!(body["items"][0]["key"], items[0].key.as_str());
    assert_eq!((body["items"][0]["new"].clone(), body["items"][1]["new"].clone()), (json!(false), json!(false)), "nothing is new before a first acknowledgement");
    assert!(body["acknowledged_hash"].is_null());
    assert_eq!(t.review_requests(), vec![chelabase_nats::ReviewRequest { branch_id: main.as_uuid(), fix: None }]);
}

#[tokio::test]
async fn protect_needs_the_current_review_hash() {
    let t = branching().await;
    let main = t.main();
    let items = review_items();
    let hash = chelabase_schema::review::review_hash(&items);
    t.review_answers(chelabase_nats::ReviewReply::Scanned { items: items.clone() });
    for body in [json!({ "protected": true }), json!({ "protected": true, "review_hash": "0".repeat(64) })] {
        let err = error_of(t.send(Method::PATCH, &t.branch(main), Some(body.clone())).await, 409).await;
        assert_eq!(code_of(&err), "review_changed", "{body}");
        assert_eq!(err["error"]["review"]["hash"], hash.as_str());
        assert!(!t.metadata.branch(main).protected);
    }
    assert!(t.jobs.member_role_enqueues.lock().unwrap().is_empty());
    let body: Value = t.send(Method::PATCH, &t.branch(main), Some(json!({ "protected": true, "review_hash": hash }))).await.json().await.unwrap();
    assert_eq!((body["protected"].as_bool(), body["review_acknowledged_hash"].as_str()), (Some(true), Some(hash.as_str())));
    let record = t.metadata.branch(main);
    assert_eq!(record.review_ack_keys, items.iter().map(|i| i.key.clone()).collect::<Vec<_>>());
    assert_eq!(*t.jobs.member_role_enqueues.lock().unwrap(), vec![main]);
    // An empty review needs no hash.
    let dev = t.ready("dev");
    t.review_answers(chelabase_nats::ReviewReply::Scanned { items: vec![] });
    assert_eq!(t.send(Method::PATCH, &t.branch(dev), Some(json!({ "protected": true }))).await.status(), 200);
}

// T4 review, Minor 3: the scan comes before any write, so a failed scan writes nothing.
#[tokio::test]
async fn a_failed_scan_writes_nothing() {
    let t = branching().await;
    let main = t.main();
    t.review_answers(chelabase_nats::ReviewReply::Failed { message: "compute unreachable".into() });
    let err = error_of(t.send(Method::PATCH, &t.branch(main), Some(json!({ "guard_logins": false, "protected": true }))).await, 503).await;
    assert_eq!(code_of(&err), "unavailable");
    let record = t.metadata.branch(main);
    assert!(record.guard_logins && !record.protected);
    assert!(t.jobs.member_role_enqueues.lock().unwrap().is_empty());
}

#[tokio::test]
async fn new_and_acknowledge_follow_the_acknowledged_keys() {
    let t = branching().await;
    let main = t.main();
    let items = review_items();
    let planted = items.iter().find(|i| i.name == "planted").unwrap().key.clone();
    {
        let mut branches = t.metadata.branches.lock().unwrap();
        let record = branches.get_mut(&main).unwrap();
        record.review_ack_hash = Some("h0".into());
        record.review_ack_keys = vec![planted.clone()];
    }
    t.review_answers(chelabase_nats::ReviewReply::Scanned { items: items.clone() });
    let path = format!("{}/review", t.branch(main));
    let body: Value = t.send(Method::GET, &path, None).await.json().await.unwrap();
    let new_of = |name: &str| body["items"].as_array().unwrap().iter().find(|i| i["name"] == name).unwrap()["new"].clone();
    assert_eq!((new_of("planted"), new_of("later")), (json!(false), json!(true)));
    assert_eq!(body["acknowledged_hash"], "h0");
    let hash = chelabase_schema::review::review_hash(&items);
    let err = error_of(t.send(Method::POST, &format!("{path}/acknowledge"), Some(json!({ "hash": "h0" }))).await, 409).await;
    assert_eq!(code_of(&err), "review_changed");
    assert_eq!(t.send(Method::POST, &format!("{path}/acknowledge"), Some(json!({ "hash": hash }))).await.status(), 200);
    let record = t.metadata.branch(main);
    assert_eq!(record.review_ack_hash.as_deref(), Some(hash.as_str()));
    assert_eq!(record.review_ack_keys.len(), 2);
}

#[tokio::test]
async fn fixes_go_to_the_worker_with_the_hash() {
    let t = branching().await;
    let main = t.main();
    let items = review_items();
    let hash = chelabase_schema::review::review_hash(&items);
    let path = format!("{}/review/fix", t.branch(main));
    let key = items[0].key.clone();
    let order = json!({ "key": key, "action": "reassign", "hash": hash });
    t.review_answers(chelabase_nats::ReviewReply::Fixed { sql: "ALTER ROUTINE public.later() OWNER TO postgres;".into(), items: vec![] });
    let body: Value = t.send(Method::POST, &path, Some(order.clone())).await.json().await.unwrap();
    assert_eq!(body["sql"], "ALTER ROUTINE public.later() OWNER TO postgres;");
    assert_eq!(body["review"]["items"], json!([]));
    assert_eq!(body["review"]["hash"], chelabase_schema::review::review_hash(&[]).as_str());
    assert_eq!(
        t.review_requests(),
        vec![chelabase_nats::ReviewRequest {
            branch_id: main.as_uuid(),
            fix: Some(chelabase_nats::ReviewFixOrder { key: key.clone(), action: chelabase_types::protection::FixAction::Reassign, hash: hash.clone() }),
        }]
    );
    t.review_answers(chelabase_nats::ReviewReply::Changed { items: items.clone() });
    let err = error_of(t.send(Method::POST, &path, Some(order.clone())).await, 409).await;
    assert_eq!(code_of(&err), "review_changed");
    assert_eq!(err["error"]["review"]["hash"], hash.as_str());
    t.review_answers(chelabase_nats::ReviewReply::NotFound { items: items.clone() });
    assert_eq!(code_of(&error_of(t.send(Method::POST, &path, Some(order.clone())).await, 404).await), "not_found");
    for (sqlstate, code) in [("2BP01", "has_dependents"), ("55P03", "lock_timeout")] {
        t.review_answers(chelabase_nats::ReviewReply::FixFailed { sqlstate: sqlstate.into(), message: "refused".into(), items: items.clone() });
        assert_eq!(code_of(&error_of(t.send(Method::POST, &path, Some(order.clone())).await, 409).await), code, "{sqlstate}");
    }
    t.review_answers(chelabase_nats::ReviewReply::Failed { message: "compute unreachable".into() });
    assert_eq!(code_of(&error_of(t.send(Method::POST, &path, Some(order)).await, 503).await), "unavailable");
}

// ---- Members hub T9: the roster's `access`, the log's member filter and branch, role events

/// The roster as `sub` sees it: the raw body and the member objects by id.
async fn roster_as(t: &Mw, sub: &str) -> (String, std::collections::HashMap<uuid::Uuid, Value>) {
    let resp = t.send(sub, Method::GET, "/v1/members", None).await;
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    let list: Vec<Value> = serde_json::from_str(&text).unwrap();
    let by_id = list.into_iter().map(|m| (m["id"].as_str().unwrap().parse().unwrap(), m)).collect();
    (text, by_id)
}

/// `access` of a roster member: `(level, [(project, platform_role)])`, projects sorted.
fn access_of(m: &Value) -> (String, Vec<(String, String)>) {
    let access = &m["access"];
    assert!(access.is_object(), "access must be an object, never null or missing: {m}");
    let mut keys: Vec<&str> = access.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort();
    assert_eq!(keys, ["level", "projects"], "{m}");
    let mut projects: Vec<(String, String)> = access["projects"]
        .as_array()
        .unwrap_or_else(|| panic!("projects must be an array: {m}"))
        .iter()
        .map(|p| {
            let mut pk: Vec<&str> = p.as_object().unwrap().keys().map(String::as_str).collect();
            pk.sort();
            assert_eq!(pk, ["platform_role", "project_id", "via"], "{p}");
            assert_eq!(p["via"], "own", "step 1 is always own: {p}");
            (p["project_id"].as_str().unwrap().to_string(), p["platform_role"].as_str().unwrap().to_string())
        })
        .collect();
    projects.sort();
    (access["level"].as_str().unwrap_or_else(|| panic!("level: {m}")).to_string(), projects)
}

/// `access_of` sorts the projects (ids are random), so expectations are sorted the same way.
fn sorted_pairs(mut v: Vec<(String, String)>) -> Vec<(String, String)> {
    v.sort();
    v
}

fn pair(project: impl std::fmt::Display, role: &str) -> (String, String) {
    (project.to_string(), role.to_string())
}

// R1: `level` and `projects` cover the projects the caller can see, so a non-admin never
// learns of grants on projects they can't see; a Developer never sees `pg_roles` or overrides
// (MH-35); the grants endpoint stays Admin-only (MH-34).
#[tokio::test]
async fn roster_level_is_over_the_callers_visible_projects() {
    let t = mw(Mode::SelfHosted).await;
    let md = &t.w.metadata;
    let p1 = t.w.project;
    let p2 = md.create_project(t.w.tenant, "Second").await.unwrap().id;
    let p3 = md.create_project(t.w.tenant, "Hidden").await.unwrap().id;
    let dev = md.add_granted(t.w.tenant, "dev", p1, PlatformRole::Developer, &[]);
    let ann = md.add_granted(t.w.tenant, "ann", p1, PlatformRole::Viewer, &["secretrole"]);
    md.grant(ann, p3, PlatformRole::Admin, &["secretrole"]);
    md.narrow(ann, t.w.db, OverrideRow { platform_role: None, pg_roles: Some(vec!["secretoverride".into()]) });
    let cy = md.add_granted(t.w.tenant, "cy", p1, PlatformRole::Developer, &[]);
    md.grant(cy, p2, PlatformRole::Viewer, &[]);
    // Admin only on a project the caller can't see, and Viewer on a visible one.
    let bo = md.add_granted(t.w.tenant, "bo", p2, PlatformRole::Admin, &[]);
    md.grant(bo, p1, PlatformRole::Viewer, &[]);

    let (text, roster) = roster_as(&t, "dev").await;
    // First failure of the task: `access` is missing.
    assert_eq!(access_of(&roster[&ann]), ("viewer".into(), vec![pair(p1, "viewer")]), "ann's admin on P3 is hidden");
    assert_eq!(access_of(&roster[&cy]), ("developer".into(), vec![pair(p1, "developer")]));
    assert_eq!(access_of(&roster[&bo]), ("viewer".into(), vec![pair(p1, "viewer")]), "bo's admin on P2 is hidden");
    assert_eq!(access_of(&roster[&dev]), ("developer".into(), vec![pair(p1, "developer")]));
    for hidden in [p2.to_string(), p3.to_string(), "secretrole".into(), "secretoverride".to_string()] {
        assert!(!text.contains(&hidden), "a Developer's roster leaked {hidden}: {text}");
    }
    // The grants endpoint is not widened for the roster (MH-34).
    let err = error_of(t.send("dev", Method::GET, &format!("/v1/projects/{p1}/grants"), None).await, 403).await;
    assert_eq!(code_of(&err), "forbidden");

    // A Viewer sees the same projects: its own grant is the visible set.
    let vic = md.add_granted(t.w.tenant, "vic", p2, PlatformRole::Viewer, &[]);
    let (text, roster) = roster_as(&t, "vic").await;
    assert_eq!(access_of(&roster[&bo]), ("admin".into(), vec![pair(p2, "admin")]), "bo is Admin on vic's one project");
    assert_eq!(access_of(&roster[&cy]), ("viewer".into(), vec![pair(p2, "viewer")]));
    assert_eq!(access_of(&roster[&vic]).0, "viewer");
    assert!(!text.contains(&p3.to_string()) && !text.contains(&p1.to_string()), "{text}");

    // The tenant admin sees everything.
    let (_, roster) = roster_as(&t, DEFAULT_SUB).await;
    assert_eq!(
        access_of(&roster[&ann]),
        ("admin".into(), sorted_pairs(vec![pair(p1, "viewer"), pair(p3, "admin")])),
        "level is the highest over all projects"
    );
    assert_eq!(access_of(&roster[&cy]), ("developer".into(), sorted_pairs(vec![pair(p1, "developer"), pair(p2, "viewer")])));
}

// MH-35: `other` is access only on projects the caller can't see; `none` is no grant anywhere
// (an override alone is no grant).
#[tokio::test]
async fn a_member_only_on_hidden_projects_is_other_and_one_without_grants_is_none() {
    let t = mw(Mode::SelfHosted).await;
    let md = &t.w.metadata;
    let p1 = t.w.project;
    let p2 = md.create_project(t.w.tenant, "Second").await.unwrap().id;
    let dev = md.add_granted(t.w.tenant, "dev", p1, PlatformRole::Viewer, &[]);
    let hid = md.add_granted(t.w.tenant, "hid", p2, PlatformRole::Admin, &["secretrole"]);
    let nobody = md.add_member(t.w.tenant, "nobody", MemberStatus::Active, false);
    let only_override = md.add_member(t.w.tenant, "ovr", MemberStatus::Active, false);
    md.narrow(only_override, t.w.db, OverrideRow { platform_role: Some(PlatformRole::Viewer), pg_roles: None });

    let (text, roster) = roster_as(&t, "dev").await;
    assert_eq!(access_of(&roster[&hid]), ("other".into(), vec![]), "{text}");
    assert_eq!(access_of(&roster[&nobody]), ("none".into(), vec![]));
    assert_eq!(access_of(&roster[&only_override]), ("none".into(), vec![]));
    assert_eq!(roster[&hid]["has_access"], true, "has_access is kept as it is");
    assert_eq!(roster[&nobody]["has_access"], false);
    assert!(!text.contains(&p2.to_string()) && !text.contains("secretrole"), "{text}");

    // A tenant admin sees the same member's real level.
    let (_, roster) = roster_as(&t, DEFAULT_SUB).await;
    assert_eq!(access_of(&roster[&hid]), ("admin".into(), vec![pair(p2, "admin")]));
    assert_eq!(access_of(&roster[&nobody]), ("none".into(), vec![]));
    assert_eq!(access_of(&roster[&dev]).0, "viewer");
}

// A tenant admin member's level is `tenant_admin` whatever their grants, and they hold every
// project (as Admin) in what the caller can see.
#[tokio::test]
async fn a_tenant_admin_sees_every_project() {
    let t = mw(Mode::SelfHosted).await;
    let md = &t.w.metadata;
    let p1 = t.w.project;
    let p2 = md.create_project(t.w.tenant, "Second").await.unwrap().id;
    let lead = md.add_member(t.w.tenant, "lead", MemberStatus::Active, true);
    let lead2 = md.add_member(t.w.tenant, "lead2", MemberStatus::Active, true);
    md.grant(lead2, p1, PlatformRole::Viewer, &[]);
    let dev = md.add_granted(t.w.tenant, "dev", p1, PlatformRole::Developer, &[]);

    let (_, roster) = roster_as(&t, DEFAULT_SUB).await;
    let everything = vec![pair(p1, "admin"), pair(p2, "admin")];
    let mut everything = everything;
    everything.sort();
    assert_eq!(access_of(&roster[&lead]), ("tenant_admin".into(), everything.clone()));
    assert_eq!(access_of(&roster[&lead2]), ("tenant_admin".into(), everything), "a viewer grant doesn't lower it");

    // The Developer sees the tenant admins' level, over the one project it can see.
    let (text, roster) = roster_as(&t, "dev").await;
    assert_eq!(access_of(&roster[&lead]), ("tenant_admin".into(), vec![pair(p1, "admin")]));
    assert_eq!(access_of(&roster[&lead2]), ("tenant_admin".into(), vec![pair(p1, "admin")]));
    assert_eq!(access_of(&roster[&dev]).0, "developer");
    assert!(!text.contains(&p2.to_string()), "{text}");
}

// MemberStatusView::Suspended is listed, and `access` is present whatever the status (Studio
// groups by status first, S17); removed members are not listed.
#[tokio::test]
async fn roster_lists_suspended_members() {
    let t = mw(Mode::SelfHosted).await;
    let md = &t.w.metadata;
    let p1 = t.w.project;
    let sus = md.add_member(t.w.tenant, "sus", MemberStatus::Suspended, false);
    md.grant(sus, p1, PlatformRole::Developer, &[]);
    let inv = md.add_member(t.w.tenant, "inv", MemberStatus::Invited, false);
    md.grant(inv, p1, PlatformRole::Viewer, &[]);
    let pend = md.add_member(t.w.tenant, "pend", MemberStatus::Pending, false);
    let gone = md.add_member(t.w.tenant, "gone", MemberStatus::Removed, false);

    for caller in [DEFAULT_SUB, "sus-viewer"] {
        if caller != DEFAULT_SUB {
            md.add_granted(t.w.tenant, caller, p1, PlatformRole::Viewer, &[]);
        }
        let (text, roster) = roster_as(&t, caller).await;
        assert_eq!(roster[&sus]["status"], "suspended", "{text}");
        assert_eq!(roster[&sus]["has_access"], true);
        assert_eq!(access_of(&roster[&sus]), ("developer".into(), vec![pair(p1, "developer")]));
        assert_eq!(roster[&inv]["status"], "invited");
        assert_eq!(access_of(&roster[&inv]), ("viewer".into(), vec![pair(p1, "viewer")]));
        assert_eq!(access_of(&roster[&pend]), ("none".into(), vec![]));
        assert!(!roster.contains_key(&gone), "a removed member is not listed");
        for m in roster.values() {
            access_of(m);
        }
    }
}

/// Seeds one event (the real writes log one per change).
#[allow(clippy::too_many_arguments)]
fn seed_event(
    t: &Mw,
    actor: Option<uuid::Uuid>,
    action: &str,
    target: Option<uuid::Uuid>,
    project: Option<ProjectId>,
    branch: Option<BranchId>,
) -> i64 {
    let mut events = t.w.metadata.events.lock().unwrap();
    let id = events.len() as i64 + 1;
    events.push(chelabase_metadata::members::AccessEvent {
        id,
        tenant_id: t.w.tenant,
        actor_member_id: actor,
        action: action.into(),
        target_member_id: target,
        project_id: project.as_ref().map(ProjectId::as_uuid),
        database_id: None,
        before: None,
        after: None,
        at: chrono::Utc::now(),
        actor_email: None,
        target_email: None,
        project_name: None,
        branch_id: branch.as_ref().map(BranchId::as_uuid),
    });
    id
}

fn event_ids(page: &Value) -> Vec<i64> {
    page["events"].as_array().unwrap_or_else(|| panic!("no events in {page}")).iter().map(|e| e["id"].as_i64().unwrap()).collect()
}

// Part 3: `?member_id=` matches the target or the actor, on both log routes; MH-36: the tenant
// log stays tenant-admin, a project's log is for its Admins.
#[tokio::test]
async fn events_filter_by_member_as_target_or_actor() {
    let t = mw(Mode::SelfHosted).await;
    let md = &t.w.metadata;
    let p1 = t.w.project;
    let p2 = md.create_project(t.w.tenant, "Second").await.unwrap().id;
    let admin = md.member(t.w.tenant, DEFAULT_SUB).unwrap().id;
    let a = md.add_member(t.w.tenant, "a", MemberStatus::Active, false);
    let b = md.add_member(t.w.tenant, "b", MemberStatus::Active, false);
    let e1 = seed_event(&t, Some(admin), "grant_put", Some(a), Some(p1), None);
    let e2 = seed_event(&t, Some(a), "invite_created", Some(b), Some(p1), None);
    let e3 = seed_event(&t, Some(admin), "grant_put", Some(b), Some(p2), None);
    let e4 = seed_event(&t, None, "open_signup_set", None, None, None);
    let e5 = seed_event(&t, Some(a), "grant_deleted", Some(a), Some(p2), None);

    let get = |q: String| {
        let t = &t;
        async move { body_of(t.send(DEFAULT_SUB, Method::GET, &q, None).await).await }
    };
    // First failure: the parameter is ignored and every event comes back.
    assert_eq!(event_ids(&get(format!("/v1/access-events?member_id={a}")).await), vec![e5, e2, e1]);
    assert_eq!(event_ids(&get(format!("/v1/access-events?member_id={b}")).await), vec![e3, e2]);
    assert_eq!(event_ids(&get(format!("/v1/access-events?member_id={admin}")).await), vec![e3, e1]);
    assert_eq!(event_ids(&get("/v1/access-events".into()).await), vec![e5, e4, e3, e2, e1]);
    // Someone with no events (or another tenant's id) is an empty page, not an error.
    let page = get(format!("/v1/access-events?member_id={}", uuid::Uuid::new_v4())).await;
    assert_eq!((event_ids(&page), page["next_before"].clone()), (vec![], Value::Null));
    // It composes with the paging.
    let first = get(format!("/v1/access-events?member_id={a}&limit=2")).await;
    assert_eq!(event_ids(&first), vec![e5, e2]);
    assert_eq!(first["next_before"], e2);
    let next = get(format!("/v1/access-events?member_id={a}&limit=2&before={e2}")).await;
    assert_eq!(event_ids(&next), vec![e1]);
    // Not a UUID: 400, not ignored.
    let err = error_of(t.send(DEFAULT_SUB, Method::GET, "/v1/access-events?member_id=nope", None).await, 400).await;
    assert_eq!(code_of(&err), "invalid_request");

    // The project's log filters the same way, within the project.
    assert_eq!(event_ids(&get(format!("/v1/projects/{p1}/access-events?member_id={a}")).await), vec![e2, e1]);
    assert_eq!(event_ids(&get(format!("/v1/projects/{p2}/access-events?member_id={a}")).await), vec![e5]);
    assert_eq!(event_ids(&get(format!("/v1/projects/{p2}/access-events?member_id={b}")).await), vec![e3]);

    // MH-36: a project Admin reads its project's log (and filters it), not the tenant's; a
    // Developer reads neither.
    md.add_granted(t.w.tenant, "padmin", p1, PlatformRole::Admin, &[]);
    md.add_granted(t.w.tenant, "pdev", p1, PlatformRole::Developer, &[]);
    let path = format!("/v1/projects/{p1}/access-events?member_id={a}");
    assert_eq!(event_ids(&body_of(t.send("padmin", Method::GET, &path, None).await).await), vec![e2, e1]);
    let err = error_of(t.send("padmin", Method::GET, &format!("/v1/access-events?member_id={a}"), None).await, 403).await;
    assert_eq!(code_of(&err), "forbidden");
    error_of(t.send("pdev", Method::GET, &path, None).await, 403).await;
    // An Admin of another project holds no grant here: the project's existence isn't revealed.
    md.add_granted(t.w.tenant, "oadmin", p2, PlatformRole::Admin, &[]);
    let err = error_of(t.send("oadmin", Method::GET, &path, None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
}

// MH-37: each event carries `branch_id` (null when the change isn't per branch).
#[tokio::test]
async fn events_carry_branch_id() {
    let t = mw(Mode::SelfHosted).await;
    let admin = t.w.metadata.member(t.w.tenant, DEFAULT_SUB).unwrap().id;
    let with = seed_event(&t, Some(admin), "role_created", None, Some(t.w.project), Some(t.w.dev));
    let without = seed_event(&t, Some(admin), "grant_put", Some(admin), Some(t.w.project), None);
    for path in ["/v1/access-events".to_string(), format!("/v1/projects/{}/access-events", t.w.project)] {
        let page = body_of(t.send(DEFAULT_SUB, Method::GET, &path, None).await).await;
        let events = page["events"].as_array().unwrap();
        let find = |id: i64| events.iter().find(|e| e["id"] == id).unwrap_or_else(|| panic!("{id} in {page}"));
        // First failure: the key is missing.
        assert_eq!(find(with).get("branch_id"), Some(&json!(t.w.dev)), "{path}");
        assert_eq!(find(without).get("branch_id"), Some(&Value::Null), "{path}: present and null");
    }
}

/// The role events the log holds, as `(action, event)`.
fn role_events(t: &Mw) -> Vec<(String, chelabase_metadata::members::AccessEvent)> {
    t.w.metadata
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.action.starts_with("role_"))
        .map(|e| (e.action.clone(), e.clone()))
        .collect()
}

// P25/MH-37: the role routes write `role_created`, `role_updated` and `role_deleted` after the
// worker answers Done, with the project, database and branch; a refused or failed call writes
// none, and no event carries a password.
#[tokio::test]
async fn role_create_update_delete_write_events_with_the_branch() {
    let t = mw(Mode::SelfHosted).await;
    let admin = t.w.metadata.member(t.w.tenant, DEFAULT_SUB).unwrap().id;
    let dev = t.w.dev;
    let roles = format!("/v1/databases/{}/branches/{}/roles", t.w.db, dev);
    let ep = t.w.metadata.branch(dev).endpoint_id.unwrap();
    t.w.metadata.add_role(&ep, "postgres", RoleOrigin::Owner, true, false);
    t.w.metadata.add_role(&ep, "app", RoleOrigin::Api, false, false);

    // Refused or failed first: nothing is logged.
    for reply in ["role_exists", "role_cap_reached", "branch_not_ready"] {
        *t.w.jobs.role_reply.lock().unwrap() = Some(Ok(role_reply(reply)));
        error_of(t.send(DEFAULT_SUB, Method::POST, &roles, Some(json!({ "name": "app" }))).await, 409).await;
    }
    *t.w.jobs.role_reply.lock().unwrap() = Some(Err(ResetError::Transport("no worker".into())));
    error_of(t.send(DEFAULT_SUB, Method::DELETE, &format!("{roles}/app"), None).await, 503).await;
    *t.w.jobs.role_reply.lock().unwrap() = None;
    error_of(t.send(DEFAULT_SUB, Method::POST, &roles, Some(json!({ "name": "Bad Name!" }))).await, 422).await;
    error_of(t.send(DEFAULT_SUB, Method::PATCH, &format!("{roles}/postgres"), Some(json!({ "inherit": false }))).await, 422).await;
    assert!(role_events(&t).is_empty(), "a refusal or failure wrote an event: {:?}", role_events(&t));

    // First failure of the task: no `role_created` event.
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &roles, Some(json!({ "name": "app" }))).await.status(), 201);
    assert_eq!(t.send(DEFAULT_SUB, Method::PATCH, &format!("{roles}/app"), Some(json!({ "inherit": false }))).await.status(), 202);
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &format!("{roles}/app"), None).await.status(), 200, "T10: a delete answers what it reassigned");
    // A password reset isn't a role change event.
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &format!("{roles}/app/reset-password"), None).await.status(), 204);

    let events = role_events(&t);
    let actions: Vec<&str> = events.iter().map(|(a, _)| a.as_str()).collect();
    assert_eq!(actions, ["role_created", "role_updated", "role_deleted"]);
    for (action, e) in &events {
        assert_eq!(e.branch_id, Some(dev.as_uuid()), "{action}");
        assert_eq!(e.database_id, Some(t.w.db.as_uuid()), "{action}");
        assert_eq!(e.project_id, Some(t.w.project.as_uuid()), "{action}");
        assert_eq!(e.actor_member_id, Some(admin), "{action}");
        assert_eq!(e.target_member_id, None, "{action}");
        let detail = e.after.as_ref().or(e.before.as_ref()).unwrap_or_else(|| panic!("{action}: no before/after"));
        assert_eq!(detail["role"], "app", "{action}");
        let text = format!("{:?}{:?}", e.before, e.after).to_lowercase();
        assert!(!text.contains("password") && !text.contains("s3cret"), "{action}: {text}");
    }
    let created = events[0].1.after.as_ref().unwrap();
    assert!(created.get("login").is_some() && created.get("attributes").is_some(), "{created}");
    // The log shows them, with the branch, on the project's Activity.
    let page = body_of(t.send(DEFAULT_SUB, Method::GET, &format!("/v1/projects/{}/access-events", t.w.project), None).await).await;
    let shown: Vec<(&str, &Value)> =
        page["events"].as_array().unwrap().iter().map(|e| (e["action"].as_str().unwrap(), &e["branch_id"])).collect();
    assert_eq!(shown, [("role_deleted", &json!(dev)), ("role_updated", &json!(dev)), ("role_created", &json!(dev))]);
}

// ---- Members hub T8: personal logins, revocation's side effects, suspension (spec Part 6) ----
//
// Through the fakes: the routes' powers row by row (Part 7), the password shown once, names
// (D3), every revocation path clearing the proxy cache (P4) and reconciling (P16 b),
// suspension with identity's ban (D5, S7), and what personal logins are hidden from (MH-11,
// MH-12, MH-21). The fake's personal-login records (`FakeLogin`) stand for T3's metadata.

use chelabase_metadata::personal_logins::{LoginEndpoint, LoginStatus, RevokeReason};

/// The T8 world: `mw`'s (project Billing, database web1 with main `ep-0123456789ab` and `dev`),
/// each member behind a fake identity user (their sub) so suspension can lock them: `alice` and
/// `bob` Developers with `app`, `vera` a Viewer, `pam` an Admin of Billing, and `oscar` an
/// Admin of project Elsewhere only. Each is (member id, sub).
struct Pl {
    t: Mw,
    alice: (uuid::Uuid, String),
    bob: (uuid::Uuid, String),
    vera: (uuid::Uuid, String),
    pam: (uuid::Uuid, String),
    oscar: (uuid::Uuid, String),
    elsewhere: ProjectId,
}

async fn pl_world() -> Pl {
    let t = mw(Mode::SelfHosted).await;
    let elsewhere = t.w.metadata.create_project(t.w.tenant, "Elsewhere").await.unwrap().id;
    let person = |name: &str, project: ProjectId, role: PlatformRole| {
        let sub = t.identity.with_user(&Pool::Members, &format!("{name}@x.com"), "Secret-pass1");
        (t.w.metadata.add_granted(t.w.tenant, &sub, project, role, &["app"]), sub)
    };
    let alice = person("alice", t.w.project, PlatformRole::Developer);
    let bob = person("bob", t.w.project, PlatformRole::Developer);
    let vera = person("vera", t.w.project, PlatformRole::Viewer);
    let pam = person("pam", t.w.project, PlatformRole::Admin);
    let oscar = person("oscar", elsewhere, PlatformRole::Admin);
    Pl { t, alice, bob, vera, pam, oscar, elsewhere }
}

/// Another ready database of `project` named `name`, its main on `endpoint`.
fn add_database(w: &MemberWorld, project: ProjectId, name: &str, endpoint: &str) -> DatabaseId {
    let mut db = sample_database(w.tenant);
    db.project_id = project;
    db.name = name.into();
    db.database_name = Some(name.into());
    db.owner_role = Some("postgres".into());
    db.main.endpoint_id = Some(endpoint.into());
    let id = db.database_id;
    w.metadata.insert_database(db);
    id
}

/// Marks a fake record revoked, as the metadata's revocation does (MH-9: no verifier left).
fn revoked(reason: RevokeReason) -> impl FnOnce(&mut FakeLogin) {
    move |l| {
        l.login.status = LoginStatus::Revoked;
        l.login.revoked_at = Some(chrono::Utc::now());
        l.login.revoked_reason = Some(reason);
        l.verifier = None;
    }
}

impl Pl {
    fn db(&self) -> DatabaseId {
        self.t.w.db
    }
    fn main(&self) -> BranchId {
        BranchId::from(self.t.w.db)
    }
    fn main_ep(&self) -> String {
        self.t.main_endpoint()
    }
    fn dev_ep(&self) -> String {
        self.t.w.metadata.branch(self.t.w.dev).endpoint_id.unwrap()
    }
    /// The (endpoint, role) of each row of a login named `name` on main and `dev`.
    fn rows(&self, name: &str) -> Vec<(String, String)> {
        vec![(self.main_ep(), name.to_string()), (self.dev_ep(), name.to_string())]
    }
    fn cache(&self) -> &FakeCache {
        &self.t.w.app.cache
    }
    fn me(&self) -> uuid::Uuid {
        self.t.w.metadata.member(self.t.w.tenant, DEFAULT_SUB).unwrap().id
    }
    fn status_of(&self, member: uuid::Uuid) -> MemberStatus {
        self.t.w.metadata.members.lock().unwrap().iter().find(|m| m.id == member).unwrap().status
    }
    /// One more member of the tenant behind a fake identity user, granted `role` on `project`.
    fn add_person(&self, name: &str, project: ProjectId, role: PlatformRole) -> (uuid::Uuid, String) {
        let sub = self.t.identity.with_user(&Pool::Members, &format!("{name}@x.com"), "Secret-pass1");
        (self.t.w.metadata.add_granted(self.t.w.tenant, &sub, project, role, &["app"]), sub)
    }
    /// A live login of `who` on web1 named `name`, with its rows on main and `dev`.
    fn seed(&self, who: &(uuid::Uuid, String), name: &str) -> FakeLogin {
        self.t.w.metadata.seed_personal_login(who.0, self.db(), name)
    }
    fn login_path(&self) -> String {
        format!("/v1/databases/{}/personal-login", self.db())
    }
    fn member_logins_path(&self, member: uuid::Uuid) -> String {
        format!("/v1/members/{member}/personal-logins")
    }
    fn database_logins_path(&self) -> String {
        format!("/v1/databases/{}/personal-logins", self.db())
    }
    fn delete_path(&self, member: uuid::Uuid) -> String {
        format!("/v1/databases/{}/personal-logins/{member}", self.db())
    }
    async fn send(&self, sub: &str, method: Method, path: &str, body: Option<Value>) -> reqwest::Response {
        self.t.send(sub, method, path, body).await
    }
    async fn put_login(&self, sub: &str, name: &str) -> reqwest::Response {
        self.send(sub, Method::PUT, &self.login_path(), Some(json!({ "name": name }))).await
    }
    async fn reset_login(&self, sub: &str) -> reqwest::Response {
        self.send(sub, Method::POST, &format!("{}/reset-password", self.login_path()), None).await
    }
    /// `POST /v1/members/{member}/{action}`: `suspend`, `reinstate` or `revoke-postgres-access`.
    async fn member_action(&self, sub: &str, member: uuid::Uuid, action: &str) -> reqwest::Response {
        self.send(sub, Method::POST, &format!("/v1/members/{member}/{action}"), None).await
    }
}

/// `valid_until` of a login view is about 90 days from now (D4).
fn assert_ninety_days(login: &Value) {
    let until: chrono::DateTime<chrono::Utc> = login["valid_until"]
        .as_str()
        .unwrap_or_else(|| panic!("no valid_until: {login}"))
        .parse()
        .unwrap();
    let hours = (until - chrono::Utc::now()).num_hours();
    assert!((89 * 24..=90 * 24).contains(&hours), "valid_until {until} is not 90 days out");
}

// Part 7: create and reset are the caller's own, for Developer and above (tenant admins
// included); a Viewer (R9), a member narrowed below Developer on the database, and a member
// with no grant on the project get none.
#[tokio::test]
async fn personal_login_create_is_own_and_developer_plus() {
    let p = pl_world().await;
    let resp = p.put_login(&p.alice.1, "alice_login").await;
    assert_eq!(resp.status(), 201, "a Developer creates their own login");
    let body = body_of(resp).await;
    let login = &body["login"];
    assert_eq!(login["member_id"], json!(p.alice.0), "{body}");
    assert_eq!(login["database_id"], json!(p.db()), "{body}");
    assert_eq!(login["project_id"], json!(p.t.w.project), "{body}");
    assert_eq!(login["database_name"], "web1", "{body}");
    assert_eq!(login["role_name"], "alice_login", "{body}");
    assert_eq!(login["status"], "active", "{body}");
    assert_eq!(login["revoked_at"], Value::Null, "{body}");
    assert_eq!(login["revoked_reason"], Value::Null, "{body}");
    assert_ninety_days(login);
    for (sub, id, name) in [(p.pam.1.as_str(), p.pam.0, "pam_login"), (DEFAULT_SUB, p.me(), "admin_login")] {
        let resp = p.put_login(sub, name).await;
        assert_eq!(resp.status(), 201, "{name}");
        assert_eq!(body_of(resp).await["login"]["member_id"], json!(id), "{name}: always the caller's own");
    }

    let err = error_of(p.put_login(&p.vera.1, "vera_login").await, 403).await;
    assert_eq!((code_of(&err), &err["error"]["required"]), ("forbidden", &json!("developer")));
    let err = error_of(p.reset_login(&p.vera.1).await, 403).await;
    assert_eq!((code_of(&err), &err["error"]["required"]), ("forbidden", &json!("developer")));
    p.t.w.metadata.narrow(p.bob.0, p.db(), OverrideRow { platform_role: Some(PlatformRole::Viewer), pg_roles: None });
    let err = error_of(p.put_login(&p.bob.1, "bob_login").await, 403).await;
    assert_eq!((code_of(&err), &err["error"]["required"]), ("forbidden", &json!("developer")), "overrides count");
    let err = error_of(p.put_login(&p.oscar.1, "oscar_login").await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    for who in [&p.vera, &p.bob, &p.oscar] {
        assert!(p.t.w.metadata.personal_login(who.0, p.db()).is_none(), "a refusal makes nothing");
    }

    // Reset acts on the caller's own login, and only theirs.
    let resp = p.reset_login(&p.alice.1).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(body_of(resp).await["login"]["member_id"], json!(p.alice.0));
}

// MH-15: the password is server-generated (128 bits or more), answered once with `no-store`
// and a connection string for main's default database (P23), and never again: not listed, not
// revealable, not in an access event, a worker request, a cache message or a log line. Only a
// SCRAM verifier is stored.
#[tokio::test]
async fn create_answers_the_password_once_with_no_store() {
    let logs = Logs::default();
    let _guard = {
        let logs = logs.clone();
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_ansi(false)
                .with_env_filter("orchestrator=trace,chelabase_metadata=trace,chelabase_neon=trace")
                .with_writer(move || logs.clone())
                .finish(),
        )
    };
    let p = pl_world().await;
    let resp = p.put_login(&p.alice.1, "alice_login").await;
    assert_eq!(resp.status(), 201);
    assert_eq!(resp.headers().get("cache-control").and_then(|v| v.to_str().ok()), Some("no-store"));
    let body = body_of(resp).await;
    let password = body["password"].as_str().unwrap_or_else(|| panic!("no password: {body}")).to_string();
    assert!(password.len() >= 22 && password.chars().all(|c| c.is_ascii_alphanumeric()), "{password}");
    let uri = body["connection_string"].as_str().unwrap_or_else(|| panic!("no connection_string: {body}"));
    assert!(uri.starts_with(&format!("postgresql://alice_login:{password}@{}.", p.main_ep())), "{uri}");
    assert!(uri.contains("/web1"), "{uri}");
    assert!(body["ready_on_main"].is_boolean(), "{body}");

    let stored = p
        .t
        .w
        .metadata
        .personal_login(p.alice.0, p.db())
        .expect("the record")
        .verifier
        .expect("a verifier");
    assert!(stored.starts_with("SCRAM-SHA-256$") && !stored.contains(&password), "{stored}");

    let listed = p.send(&p.alice.1, Method::GET, &p.member_logins_path(p.alice.0), None).await;
    assert_eq!(listed.status(), 200);
    let listed = listed.text().await.unwrap();
    assert!(listed.contains("alice_login") && !listed.contains(&password) && !listed.contains("SCRAM"), "{listed}");
    let reveal = format!("/v1/databases/{}/branches/{}/connection?role=alice_login", p.db(), p.main());
    assert_eq!(p.send(DEFAULT_SUB, Method::GET, &reveal, None).await.status(), 404, "never revealable");

    let events = format!("{:?}", p.t.w.metadata.events.lock().unwrap());
    assert!(!events.contains(&password) && !events.contains(&stored), "{events}");
    let requests = format!(
        "{:?}{:?}",
        p.t.w.jobs.member_reconciles.lock().unwrap(),
        p.t.w.jobs.member_role_enqueues.lock().unwrap()
    );
    assert!(!requests.contains(&password), "{requests}");
    assert!(!format!("{:?}", p.cache().sent()).contains(&password));
    let text = logs.text();
    assert!(!text.contains(&password) && !text.contains(&stored), "{text}");

    let other = body_of(p.put_login(&p.pam.1, "pam_login").await).await;
    assert!(other["password"].is_string() && other["password"] != json!(password), "{other}");
}

// MH-15: a live login is 409 `personal_login_exists`, whatever the name (renaming is delete
// and create). P6: a revoked record is replaced, its name free for its own member (S10).
#[tokio::test]
async fn create_twice_is_personal_login_exists() {
    let p = pl_world().await;
    assert_eq!(p.put_login(&p.alice.1, "alice_login").await.status(), 201);
    for name in ["alice_login", "alice_other"] {
        let err = error_of(p.put_login(&p.alice.1, name).await, 409).await;
        assert_eq!(code_of(&err), "personal_login_exists", "{name}");
    }
    p.t.w.metadata.edit_personal_login(p.alice.0, p.db(), revoked(RevokeReason::AccessLost));
    let resp = p.put_login(&p.alice.1, "alice_login").await;
    assert_eq!(resp.status(), 201, "a revoked record is replaced");
    assert_eq!(body_of(resp).await["login"]["status"], "active");
}

// T3-M4: a create that replaces an expired (or revoked) record tells the proxy cache about the
// old record's rows, which the replace removes.
#[tokio::test]
async fn create_replacing_an_expired_login_clears_the_cache_for_its_old_rows() {
    let p = pl_world().await;
    p.seed(&p.alice, "alice_old");
    p.t.w.metadata.edit_personal_login(p.alice.0, p.db(), |l| {
        l.login.valid_until = chrono::Utc::now() - chrono::Duration::minutes(1);
        l.login.status = LoginStatus::Expired;
    });
    assert_eq!(p.put_login(&p.alice.1, "alice_new").await.status(), 201);
    assert!(
        p.cache().wait_for(&p.rows("alice_old")).await,
        "the replaced record's rows are cleared from the proxy cache: {:?}",
        p.cache().sent()
    );
}

// D3: the name is free but unique across every branch of the database, against every role
// there and every login of the database (any status): 409 `role_exists`. The shape and the
// reserved names are refused (422).
#[tokio::test]
async fn a_name_taken_on_another_branch_is_role_exists() {
    let p = pl_world().await;
    p.t.w.metadata.add_role(&p.dev_ep(), "billing", RoleOrigin::Sql, false, false);
    let err = error_of(p.put_login(&p.alice.1, "billing").await, 409).await;
    assert_eq!(code_of(&err), "role_exists", "a role on another branch holds the name");
    p.seed(&p.bob, "shared");
    p.t.w.metadata.edit_personal_login(p.bob.0, p.db(), revoked(RevokeReason::AccessLost));
    let err = error_of(p.put_login(&p.alice.1, "shared").await, 409).await;
    assert_eq!(code_of(&err), "role_exists", "another member's revoked login still holds its name");

    let long = "a".repeat(64);
    for name in ["Alice", "1abc", "a-b", "", long.as_str()] {
        let err = error_of(p.put_login(&p.alice.1, name).await, 422).await;
        assert_eq!(code_of(&err), "invalid_role_name", "{name:?}");
    }
    for name in ["postgres", "owner", "anon", "authenticator", "pg_monitor", "cb_m_x", "chelabase_x"] {
        let err = error_of(p.put_login(&p.alice.1, name).await, 422).await;
        assert_eq!(code_of(&err), "reserved_role_name", "{name}");
    }
    let err = error_of(p.send(&p.alice.1, Method::PUT, &p.login_path(), Some(json!({}))).await, 422).await;
    assert_eq!(code_of(&err), "invalid_request", "the name is required");
    assert!(p.t.w.metadata.personal_login(p.alice.0, p.db()).is_none(), "no refusal wrote anything");
    assert_eq!(p.put_login(&p.alice.1, "alice_login").await.status(), 201);
}

// P7, S9: create asks main for one synchronous member-filtered reconcile and enqueues the
// other branches; `ready_on_main` is true only when the reply is `Done` and the login's row is
// on main's endpoint.
#[tokio::test]
async fn create_asks_main_for_a_synchronous_reconcile_and_enqueues_the_others() {
    let p = pl_world().await;
    let (main, main_ep) = (p.main(), p.main_ep());
    let store = p.t.w.metadata.personal_logins.clone();
    *p.t.w.jobs.member_reconcile_hook.lock().unwrap() = Some(Arc::new(move |r: &MemberRolesReconcileRequest| {
        if r.branch_id != main.as_uuid() {
            return;
        }
        let Some(member) = r.member_id else { return };
        for l in store.lock().unwrap().iter_mut().filter(|l| l.login.member_id == member) {
            l.endpoints.push(LoginEndpoint {
                branch_id: main.as_uuid(),
                endpoint_id: main_ep.clone(),
                role_name: l.login.role_name.clone(),
            });
        }
    }));
    let resp = p.put_login(&p.alice.1, "alice_login").await;
    assert_eq!(resp.status(), 201);
    let body = body_of(resp).await;
    assert_eq!(body["ready_on_main"], true, "{body}");
    assert_eq!(
        p.t.w.jobs.member_reconciles.lock().unwrap().clone(),
        vec![MemberRolesReconcileRequest { branch_id: main.as_uuid(), member_id: Some(p.alice.0), from_ddl: false }],
        "one synchronous reconcile, of main, for that member"
    );
    let enqueued = p.t.w.jobs.member_role_enqueues.lock().unwrap().clone();
    assert!(enqueued.contains(&p.t.w.dev), "the other branches are enqueued: {enqueued:?}");

    // `Done` without the login's row on main isn't ready.
    *p.t.w.jobs.member_reconcile_hook.lock().unwrap() = None;
    let resp = p.put_login(&p.pam.1, "pam_login").await;
    assert_eq!(resp.status(), 201);
    assert_eq!(body_of(resp).await["ready_on_main"], false);
}

// S9: any reconcile outcome but `Done` is `ready_on_main: false`, never an error: the login is
// made and its password answered (201, not 503).
#[tokio::test]
async fn create_reports_ready_on_main_false_when_the_reconcile_times_out() {
    let p = pl_world().await;
    let outcomes = [
        (&p.alice, "alice_login", Err(ResetError::Timeout)),
        (&p.pam, "pam_login", Err(ResetError::Transport("no worker listening".into()))),
        (&p.bob, "bob_login", Ok(MemberRolesReply::Failed { message: "compute not ready".into() })),
    ];
    for (who, name, reply) in outcomes {
        *p.t.w.jobs.member_reconcile_reply.lock().unwrap() = Some(reply);
        let resp = p.put_login(&who.1, name).await;
        assert_eq!(resp.status(), 201, "{name}: the login is made all the same");
        let body = body_of(resp).await;
        assert_eq!(body["ready_on_main"], false, "{body}");
        assert!(body["password"].as_str().is_some_and(|s| !s.is_empty()), "{body}");
    }
}

// P8, D4: a reset answers a new password once (no-store), renews `valid_until` to 90 days,
// stores a new verifier and tells the proxy cache about every row of the login. Without a live
// login (none, or a revoked one) it is 404.
#[tokio::test]
async fn reset_renews_valid_until_and_clears_the_cache() {
    let p = pl_world().await;
    p.seed(&p.alice, "alice_login");
    p.t.w.metadata.edit_personal_login(p.alice.0, p.db(), |l| {
        l.login.valid_until = chrono::Utc::now() + chrono::Duration::days(3)
    });
    let before = p.t.w.metadata.personal_login(p.alice.0, p.db()).unwrap().verifier.unwrap();
    let resp = p.reset_login(&p.alice.1).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("cache-control").and_then(|v| v.to_str().ok()), Some("no-store"));
    let body = body_of(resp).await;
    let password = body["password"].as_str().unwrap_or_else(|| panic!("no password: {body}")).to_string();
    assert_ninety_days(&body["login"]);
    let uri = body["connection_string"].as_str().unwrap_or_else(|| panic!("no connection_string: {body}"));
    assert!(uri.starts_with(&format!("postgresql://alice_login:{password}@{}.", p.main_ep())), "{uri}");
    let after = p.t.w.metadata.personal_login(p.alice.0, p.db()).unwrap().verifier.unwrap();
    assert!(after != before && after.starts_with("SCRAM-SHA-256$") && !after.contains(&password), "{after}");
    assert!(
        p.cache().wait_for(&p.rows("alice_login")).await,
        "every row of the login is cleared from the proxy cache: {:?}",
        p.cache().sent()
    );
    assert!(!format!("{:?}", p.t.w.metadata.events.lock().unwrap()).contains(&password));

    let err = error_of(p.reset_login(&p.pam.1).await, 404).await;
    assert_eq!(code_of(&err), "not_found", "no login");
    p.seed(&p.bob, "bob_login");
    p.t.w.metadata.edit_personal_login(p.bob.0, p.db(), revoked(RevokeReason::RevokedNow));
    let err = error_of(p.reset_login(&p.bob.1).await, 404).await;
    assert_eq!(code_of(&err), "not_found", "a revoked login isn't reset");
}

// Part 7: a login is deleted by its member, a tenant admin, or an Admin of the database's
// project; anyone else gets 404. The deleted rows are cleared from the proxy cache.
#[tokio::test]
async fn personal_login_delete_by_self_tenant_admin_or_project_admin() {
    let p = pl_world().await;
    p.seed(&p.alice, "alice_login");
    p.seed(&p.bob, "bob_login");
    assert_eq!(p.send(&p.alice.1, Method::DELETE, &p.delete_path(p.alice.0), None).await.status(), 204, "own");
    assert!(p.t.w.metadata.personal_login(p.alice.0, p.db()).is_none());
    assert!(
        p.cache().wait_for(&p.rows("alice_login")).await,
        "the deleted rows are cleared from the proxy cache: {:?}",
        p.cache().sent()
    );

    for (who, sub) in [("a Developer", &p.alice.1), ("a Viewer", &p.vera.1), ("another project's Admin", &p.oscar.1)] {
        let err = error_of(p.send(sub, Method::DELETE, &p.delete_path(p.bob.0), None).await, 404).await;
        assert_eq!(code_of(&err), "not_found", "{who}");
    }
    assert!(p.t.w.metadata.personal_login(p.bob.0, p.db()).is_some(), "the refusals deleted nothing");
    assert_eq!(p.send(&p.pam.1, Method::DELETE, &p.delete_path(p.bob.0), None).await.status(), 204, "the project's Admin");
    assert!(p.t.w.metadata.personal_login(p.bob.0, p.db()).is_none());
    p.seed(&p.bob, "bob_again");
    assert_eq!(p.send(DEFAULT_SUB, Method::DELETE, &p.delete_path(p.bob.0), None).await.status(), 204, "a tenant admin");
    let err = error_of(p.send(DEFAULT_SUB, Method::DELETE, &p.delete_path(p.bob.0), None).await, 404).await;
    assert_eq!(code_of(&err), "not_found", "nothing left to delete");
}

// MH-22: self sees all of their own, a tenant admin all, a project Admin those on their
// projects, everyone else an empty list (200, never 403). No verifier or password is listed.
#[tokio::test]
async fn personal_login_list_is_filtered_by_caller() {
    let p = pl_world().await;
    let web9 = add_database(&p.t.w, p.elsewhere, "web9", "ep-0000000000e9");
    p.t.w.metadata.grant(p.alice.0, p.elsewhere, PlatformRole::Developer, &[]);
    p.seed(&p.alice, "alice_login");
    p.t.w.metadata.seed_personal_login(p.alice.0, web9, "alice_e9");
    let path = p.member_logins_path(p.alice.0);
    let names = |body: &Value| -> Vec<String> {
        let mut out: Vec<String> = body["logins"]
            .as_array()
            .unwrap_or_else(|| panic!("no logins: {body}"))
            .iter()
            .map(|l| l["role_name"].as_str().unwrap().to_string())
            .collect();
        out.sort();
        out
    };
    let cases: [(&str, &str, &[&str]); 6] = [
        ("self", p.alice.1.as_str(), &["alice_e9", "alice_login"]),
        ("a tenant admin", DEFAULT_SUB, &["alice_e9", "alice_login"]),
        ("Billing's Admin", p.pam.1.as_str(), &["alice_login"]),
        ("Elsewhere's Admin", p.oscar.1.as_str(), &["alice_e9"]),
        ("a Viewer", p.vera.1.as_str(), &[]),
        ("another Developer", p.bob.1.as_str(), &[]),
    ];
    for (who, sub, want) in cases {
        let resp = p.send(sub, Method::GET, &path, None).await;
        assert_eq!(resp.status(), 200, "{who}");
        let text = resp.text().await.unwrap();
        assert!(!text.contains("SCRAM"), "{who}: {text}");
        let body: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(names(&body), want.to_vec(), "{who}: {body}");
    }
    let own = body_of(p.send(&p.alice.1, Method::GET, &path, None).await).await;
    let fields: std::collections::BTreeSet<&str> =
        own["logins"][0].as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        fields,
        std::collections::BTreeSet::from([
            "created_at", "database_id", "database_name", "member_id", "project_id", "revoked_at",
            "revoked_reason", "role_name", "status", "valid_until",
        ]),
        "{own}"
    );
    p.t.w.metadata.edit_personal_login(p.alice.0, p.db(), revoked(RevokeReason::AccessLost));
    let own = body_of(p.send(&p.alice.1, Method::GET, &path, None).await).await;
    let revoked = own["logins"].as_array().unwrap().iter().find(|l| l["role_name"] == "alice_login").unwrap().clone();
    assert_eq!((revoked["status"].clone(), revoked["revoked_reason"].clone()), (json!("revoked"), json!("access_lost")));
    assert!(revoked["revoked_at"].is_string(), "{revoked}");
}

// P24, S8: the database's list is for its project's Admins and tenant admins, with every
// record whatever its status.
#[tokio::test]
async fn database_personal_logins_are_project_admin_only() {
    let p = pl_world().await;
    p.seed(&p.alice, "alice_login");
    p.seed(&p.bob, "bob_login");
    p.t.w.metadata.edit_personal_login(p.bob.0, p.db(), revoked(RevokeReason::AccessLost));
    let path = p.database_logins_path();
    for sub in [p.pam.1.as_str(), DEFAULT_SUB] {
        let resp = p.send(sub, Method::GET, &path, None).await;
        assert_eq!(resp.status(), 200, "{sub}");
        let body = body_of(resp).await;
        let mut got: Vec<(String, String, Value)> = body["logins"]
            .as_array()
            .unwrap_or_else(|| panic!("no logins: {body}"))
            .iter()
            .map(|l| (l["role_name"].as_str().unwrap().to_string(), l["status"].as_str().unwrap().to_string(), l["member_id"].clone()))
            .collect();
        got.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            got,
            vec![
                ("alice_login".to_string(), "active".to_string(), json!(p.alice.0)),
                ("bob_login".to_string(), "revoked".to_string(), json!(p.bob.0)),
            ],
            "{sub}"
        );
    }
    for sub in [&p.alice.1, &p.vera.1] {
        let err = error_of(p.send(sub, Method::GET, &path, None).await, 403).await;
        assert_eq!((code_of(&err), &err["error"]["required"]), ("forbidden", &json!("admin")), "{sub}");
    }
    let err = error_of(p.send(&p.oscar.1, Method::GET, &path, None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
}

// Part 7: suspend, reinstate and revoke-now are tenant admins' only, even on oneself.
#[tokio::test]
async fn suspend_reinstate_and_revoke_are_tenant_admin_only() {
    let p = pl_world().await;
    for (who, sub) in [("a Developer", &p.alice.1), ("a project Admin", &p.pam.1), ("a Viewer", &p.vera.1)] {
        for action in ["suspend", "reinstate", "revoke-postgres-access"] {
            let err = error_of(p.member_action(sub, p.bob.0, action).await, 403).await;
            assert_eq!((code_of(&err), &err["error"]["required"]), ("forbidden", &json!("tenant admin")), "{action} by {who}");
        }
    }
    let err = error_of(p.member_action(&p.alice.1, p.alice.0, "suspend").await, 403).await;
    assert_eq!(code_of(&err), "forbidden", "not even oneself");
    assert_eq!(p.status_of(p.bob.0), MemberStatus::Active);

    let resp = p.member_action(DEFAULT_SUB, p.bob.0, "suspend").await;
    assert_eq!(resp.status(), 200);
    let body = body_of(resp).await;
    assert_eq!((body["id"].clone(), body["status"].clone()), (json!(p.bob.0), json!("suspended")), "{body}");
    let resp = p.member_action(DEFAULT_SUB, p.bob.0, "reinstate").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(body_of(resp).await["status"], "active");
    let resp = p.member_action(DEFAULT_SUB, p.bob.0, "revoke-postgres-access").await;
    assert_eq!(resp.status(), 200);
    let body = body_of(resp).await;
    assert!(body["revoked"].is_u64() && body["rotated_branches"].is_u64(), "{body}");
}

// P4, MH-9: every write that changes a member's access tells the proxy cache about every row
// of the member's logins (grant put and delete, override put and delete, tenant admin cleared,
// remove, suspend, reinstate, revoke-now). Valkey being down doesn't fail the write.
#[tokio::test]
async fn every_access_write_clears_the_proxy_cache_for_the_members_logins() {
    let p = pl_world().await;
    let project = p.t.w.project;
    let grant = |m: uuid::Uuid| format!("/v1/projects/{project}/grants/{m}");
    let told = |name: String, what: &'static str| {
        let p = &p;
        async move {
            assert!(
                p.cache().wait_for(&p.rows(&name)).await,
                "{what}: the proxy cache is told about every row of the member's login: {:?}",
                p.cache().sent()
            );
            p.cache().clear();
        }
    };

    let m = p.add_person("m_put", project, PlatformRole::Developer);
    p.seed(&m, "m_put");
    let resp = p.send(DEFAULT_SUB, Method::PUT, &grant(m.0), Some(json!({ "platform_role": "viewer", "pg_roles": [] }))).await;
    assert_eq!(resp.status(), 200);
    told("m_put".into(), "grant put").await;

    let m = p.add_person("m_del", project, PlatformRole::Developer);
    p.seed(&m, "m_del");
    assert!(p.send(DEFAULT_SUB, Method::DELETE, &grant(m.0), None).await.status().is_success());
    told("m_del".into(), "grant delete").await;

    let m = p.add_person("m_oput", project, PlatformRole::Developer);
    p.seed(&m, "m_oput");
    let body = json!({ "database_id": p.db(), "platform_role": "viewer" });
    assert_eq!(p.send(DEFAULT_SUB, Method::PUT, &format!("{}/overrides", grant(m.0)), Some(body)).await.status(), 200);
    told("m_oput".into(), "override put").await;

    let m = p.add_person("m_odel", project, PlatformRole::Developer);
    p.t.w.metadata.narrow(m.0, p.db(), OverrideRow { platform_role: Some(PlatformRole::Developer), pg_roles: None });
    p.seed(&m, "m_odel");
    let path = format!("{}/overrides?database_id={}", grant(m.0), p.db());
    assert!(p.send(DEFAULT_SUB, Method::DELETE, &path, None).await.status().is_success());
    told("m_odel".into(), "override delete").await;

    let sub = p.t.identity.with_user(&Pool::Members, "m_admin@x.com", "Secret-pass1");
    let m = (p.t.w.metadata.add_member(p.t.w.tenant, &sub, MemberStatus::Active, true), sub);
    p.seed(&m, "m_admin");
    let resp = p.send(DEFAULT_SUB, Method::PATCH, &format!("/v1/members/{}", m.0), Some(json!({ "tenant_admin": false }))).await;
    assert_eq!(resp.status(), 200);
    told("m_admin".into(), "tenant admin cleared").await;

    let m = p.add_person("m_rm", project, PlatformRole::Developer);
    p.seed(&m, "m_rm");
    assert_eq!(p.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/members/{}", m.0), None).await.status(), 204);
    told("m_rm".into(), "remove").await;

    let m = p.add_person("m_susp", project, PlatformRole::Developer);
    p.seed(&m, "m_susp");
    assert_eq!(p.member_action(DEFAULT_SUB, m.0, "suspend").await.status(), 200);
    told("m_susp".into(), "suspend").await;
    assert_eq!(p.member_action(DEFAULT_SUB, m.0, "reinstate").await.status(), 200);
    told("m_susp".into(), "reinstate").await;

    let m = p.add_person("m_now", project, PlatformRole::Developer);
    p.seed(&m, "m_now");
    assert_eq!(p.member_action(DEFAULT_SUB, m.0, "revoke-postgres-access").await.status(), 200);
    told("m_now".into(), "revoke-now").await;

    *p.cache().down.lock().unwrap() = true;
    let m = p.add_person("m_down", project, PlatformRole::Developer);
    p.seed(&m, "m_down");
    assert_eq!(
        p.member_action(DEFAULT_SUB, m.0, "revoke-postgres-access").await.status(),
        200,
        "Valkey down doesn't fail the write (the verifier is already gone)"
    );
}

// P15: a tenant admin can't suspend themself (409 `conflict`; the last-active-admin check
// that follows in the metadata is reachable through the API only by a self-suspension, which
// this refuses first); an invited or pending member can't be suspended (409 `conflict`);
// another tenant's member, or none, is 404.
#[tokio::test]
async fn suspending_yourself_or_the_last_tenant_admin_is_refused() {
    let p = pl_world().await;
    let me = p.me();
    let err = error_of(p.member_action(DEFAULT_SUB, me, "suspend").await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    assert_eq!(p.status_of(me), MemberStatus::Active);
    let invited = p.t.w.metadata.add_member(p.t.w.tenant, "inv", MemberStatus::Invited, false);
    let pending = p.t.w.metadata.add_member(p.t.w.tenant, "pend", MemberStatus::Pending, false);
    for (what, m) in [("invited", invited), ("pending", pending)] {
        let err = error_of(p.member_action(DEFAULT_SUB, m, "suspend").await, 409).await;
        assert_eq!(code_of(&err), "conflict", "{what}");
        let err = error_of(p.member_action(DEFAULT_SUB, m, "reinstate").await, 409).await;
        assert_eq!(code_of(&err), "conflict", "{what} can't be reinstated");
    }
    let stranger = p.t.w.metadata.add_member(TenantId::new(), "s", MemberStatus::Active, false);
    for m in [stranger, uuid::Uuid::new_v4()] {
        for action in ["suspend", "reinstate", "revoke-postgres-access"] {
            let err = error_of(p.member_action(DEFAULT_SUB, m, action).await, 404).await;
            assert_eq!(code_of(&err), "not_found", "{action}");
        }
    }
}

// MH-11: every generic role route treats a personal login as absent: not listed; reset,
// delete, update, sessions and the connection reveal are 404, and the worker is never asked.
#[tokio::test]
async fn generic_role_routes_hide_personal_logins() {
    let p = pl_world().await;
    p.seed(&p.alice, "alice_login");
    p.t.w.metadata.add_role(&p.dev_ep(), "app", RoleOrigin::Api, false, false);
    let dev = p.t.w.dev;
    let roles = format!("/v1/databases/{}/branches/{dev}/roles", p.db());
    let listed = body_of(p.send(DEFAULT_SUB, Method::GET, &roles, None).await).await;
    let names: Vec<&str> = listed.as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"app") && !names.contains(&"alice_login"), "{listed}");
    let calls = [
        (Method::POST, format!("{roles}/alice_login/reset-password"), None),
        (Method::DELETE, format!("{roles}/alice_login"), None),
        (Method::PATCH, format!("{roles}/alice_login"), Some(json!({ "inherit": false }))),
        (Method::GET, format!("{roles}/alice_login/sessions"), None),
        (Method::GET, format!("/v1/databases/{}/branches/{dev}/connection?role=alice_login", p.db()), None),
    ];
    for (method, path, body) in calls {
        let what = format!("{method} {path}");
        let err = error_of(p.send(DEFAULT_SUB, method, &path, body).await, 404).await;
        assert_eq!(code_of(&err), "not_found", "{what}");
    }
    assert!(p.t.w.jobs.role_calls.lock().unwrap().is_empty(), "the worker was never asked");
    assert!(p.t.w.jobs.session_reads.lock().unwrap().is_empty());
}

// MH-21: act-as of a personal login is refused for everyone, tenant admins and the login's own
// member included, with `forbidden` `role:<name>`.
#[tokio::test]
async fn act_as_a_personal_login_is_forbidden_even_for_a_tenant_admin() {
    let p = pl_world().await;
    p.seed(&p.alice, "alice_login");
    p.t.w.metadata.add_branch_db(p.main(), "web1", "postgres", DatabaseOrigin::Main);
    let base = format!("/v1/databases/{}/branches/{}", p.db(), p.main());
    let calls = vec![
        (Method::GET, format!("{base}/console/schema?role=alice_login"), None),
        (
            Method::POST,
            format!("{base}/console/query"),
            Some(json!({ "role": "alice_login", "tree": { "v": 1, "op": "select", "from": "notes" } })),
        ),
        (
            Method::POST,
            format!("{base}/sql"),
            Some(json!({ "run_id": uuid::Uuid::new_v4(), "role": "alice_login", "sql": "select 1" })),
        ),
        (
            Method::POST,
            format!("{base}/schema-changes"),
            Some(json!({ "role": "alice_login", "dry_run": true, "operations": [{ "set_table_comment": { "table": { "schema": "public", "name": "notes" }, "comment": null } }] })),
        ),
    ];
    for (who, sub) in [("a tenant admin", DEFAULT_SUB), ("a project Admin", p.pam.1.as_str()), ("the login's member", p.alice.1.as_str())] {
        for (method, path, body) in calls.clone() {
            let what = format!("{method} {path} as {who}");
            let resp = p.send(sub, method, &path, body).await;
            let status = resp.status().as_u16();
            let err = body_of(resp).await;
            assert_eq!(
                (status, &err["error"]["code"], &err["error"]["required"]),
                (403, &json!("forbidden"), &json!("role:alice_login")),
                "{what}: {err}"
            );
        }
    }
}

// MH-21, S12: no grant or override may name a personal login: a project grant is refused for a
// login of any database of the project (Admin grants included, whose roles aren't otherwise
// checked), an override for a login of its database, also one whose rows aren't on any branch
// yet (S11).
#[tokio::test]
async fn grants_and_overrides_refuse_a_personal_login_name() {
    let p = pl_world().await;
    p.seed(&p.alice, "alice_login");
    let web2 = add_database(&p.t.w, p.t.w.project, "web2", "ep-0000000000b2");
    p.t.w.metadata.seed_personal_login(p.bob.0, web2, "bob_web2");
    // A login whose rows the worker hasn't made yet: a record only.
    p.seed(&p.pam, "fresh_login");
    p.t.w.metadata.edit_personal_login(p.pam.0, p.db(), |l| l.endpoints.clear());
    for rows in p.t.w.metadata.roles.lock().unwrap().values_mut() {
        rows.retain(|r| r.name != "fresh_login");
    }
    let project = p.t.w.project;
    let grant = format!("/v1/projects/{project}/grants/{}", p.vera.0);
    for (role, name) in [("admin", "alice_login"), ("admin", "bob_web2"), ("developer", "fresh_login")] {
        let body = json!({ "platform_role": role, "pg_roles": [name] });
        let err = error_of(p.send(DEFAULT_SUB, Method::PUT, &grant, Some(body)).await, 422).await;
        assert_eq!((code_of(&err), &err["error"]["role"]), ("invalid_request", &json!(name)), "a {role} grant naming {name}");
    }
    assert_eq!(
        p.t.w.metadata.grants.lock().unwrap().get(&(p.vera.0, project.as_uuid())).map(|g| g.platform_role),
        Some(PlatformRole::Viewer),
        "the grant is unchanged"
    );
    let overrides = format!("/v1/projects/{project}/grants/{}/overrides", p.pam.0);
    for (db, name) in [(p.db(), "alice_login"), (web2, "bob_web2"), (p.db(), "fresh_login")] {
        let body = json!({ "database_id": db, "platform_role": "admin", "pg_roles": [name] });
        let err = error_of(p.send(DEFAULT_SUB, Method::PUT, &overrides, Some(body)).await, 422).await;
        assert_eq!((code_of(&err), &err["error"]["role"]), ("invalid_request", &json!(name)), "an override naming {name}");
    }
    assert!(p.t.w.metadata.overrides.lock().unwrap().is_empty(), "no override written");
}

// MH-12: a key can't be bound to a personal login, even by a tenant admin; the worker isn't
// asked and nothing is stored.
#[tokio::test]
async fn a_key_cannot_be_bound_to_a_personal_login() {
    let p = pl_world().await;
    p.seed(&p.alice, "alice_login");
    let keys = format!("/v1/databases/{}/branches/{}/keys", p.db(), p.main());
    let err = error_of(p.send(DEFAULT_SUB, Method::POST, &keys, Some(json!({ "name": "k", "role": "alice_login" }))).await, 403).await;
    assert_eq!((code_of(&err), &err["error"]["required"]), ("forbidden", &json!("role:alice_login")));
    assert!(p.t.w.metadata.keys.lock().unwrap().is_empty(), "no key stored");
    assert!(p.t.w.jobs.role_checks.lock().unwrap().is_empty(), "the worker isn't asked about it");
}

// P16 (b), S2: revoke-now revokes every live login (`revoked_now`), marks the member's
// `cb_m_*` roles for rotation, reconciles every branch of every project the member reaches
// (which rotates them and ends their sessions) and tells the proxy cache about every row.
// Studio access is untouched.
#[tokio::test]
async fn revoke_now_revokes_logins_and_reconciles_every_reached_project() {
    let p = pl_world().await;
    let web9 = add_database(&p.t.w, p.elsewhere, "web9", "ep-0000000000e9");
    p.t.w.metadata.grant(p.alice.0, p.elsewhere, PlatformRole::Developer, &[]);
    p.seed(&p.alice, "alice_login");
    p.t.w.metadata.seed_personal_login(p.alice.0, web9, "alice_e9");
    p.t.w.metadata.member_roles.lock().unwrap().push(MemberBranchRole {
        member_id: p.alice.0,
        branch_id: p.t.w.dev.as_uuid(),
        role_name: chelabase_types::role_name::member_role_name(p.alice.0),
        password_enc: "v1:sealed".into(),
        applied_roles: vec!["app".into()],
    });
    p.t.clear_enqueued();

    let resp = p.member_action(DEFAULT_SUB, p.alice.0, "revoke-postgres-access").await;
    assert_eq!(resp.status(), 200);
    let body = body_of(resp).await;
    assert_eq!(body["revoked"], 2, "{body}");
    assert!(body["rotated_branches"].is_u64(), "{body}");
    for db in [p.db(), web9] {
        let l = p.t.w.metadata.personal_login(p.alice.0, db).unwrap();
        assert_eq!((l.login.status, l.login.revoked_reason), (LoginStatus::Revoked, Some(RevokeReason::RevokedNow)));
        assert!(l.verifier.is_none(), "no verifier is left");
    }
    let rows = p.t.w.metadata.member_roles.lock().unwrap().clone();
    assert!(
        rows.iter().filter(|r| r.member_id == p.alice.0).all(|r| r.password_enc.is_empty()),
        "the member's roles are marked for rotation: {rows:?}"
    );
    let enqueued = p.t.enqueued();
    for b in [p.main(), p.t.w.dev, BranchId::from(web9)] {
        assert!(enqueued.contains(&b), "{b} reconciles: {enqueued:?}");
    }
    let mut want = p.rows("alice_login");
    want.push(("ep-0000000000e9".into(), "alice_e9".into()));
    assert!(p.cache().wait_for(&want).await, "every row is cleared from the proxy cache: {:?}", p.cache().sent());
    assert_eq!(p.send(&p.alice.1, Method::GET, "/v1/projects", None).await.status(), 200, "Studio access is untouched");
}

// S11, D3: the role routes can't make a role under a personal login's name, on a branch where
// the login is (main) or isn't yet (a new branch).
#[tokio::test]
async fn create_role_refuses_a_personal_login_name() {
    let p = pl_world().await;
    p.seed(&p.alice, "alice_login");
    let feature = p.t.w.metadata.add_branch(p.db(), "feature", p.main(), BranchKind::Normal, BranchStatus::Ready);
    for branch in [p.main(), feature] {
        let path = format!("/v1/databases/{}/branches/{branch}/roles", p.db());
        let err = error_of(p.send(DEFAULT_SUB, Method::POST, &path, Some(json!({ "name": "alice_login" }))).await, 409).await;
        assert_eq!(code_of(&err), "role_exists", "{branch}");
    }
    assert!(p.t.w.jobs.role_creates.lock().unwrap().is_empty(), "the worker is never asked");
}

// --- Members hub final round, area B: privilege ops and keys ------------------------------

/// Every privilege op that names a schema, on `schema`, granting to (or revoking from) `role`.
fn schema_named_ops(schema: &str, role: &str) -> Vec<Value> {
    let t = json!({ "schema": schema, "name": "t" });
    vec![
        json!({ "grant": { "table": t, "grant": { "privileges": ["select"], "grantee": { "role": role } } } }),
        json!({ "revoke": { "table": t, "grant": { "privileges": ["select"], "grantee": "public" } } }),
        json!({ "grant_schema": { "schema": schema, "grantee": { "role": role }, "privileges": ["usage"] } }),
        json!({ "revoke_schema": { "schema": schema, "grantee": { "role": role }, "privileges": ["create"] } }),
        json!({ "grant_sequence": { "sequence": t, "grantee": { "role": role }, "privileges": ["usage"] } }),
        json!({ "revoke_sequence": { "sequence": t, "grantee": { "role": role }, "privileges": ["usage"], "grant_option_only": true } }),
        json!({ "alter_default_privileges": { "for_role": "postgres", "schema": schema, "object": "tables",
            "grant": true, "grantee": { "role": role }, "privileges": ["select"] } }),
        json!({ "set_all_rows": { "table": t, "role": role, "enabled": true } }),
    ]
}

// T4-5: privilege ops on the schemas the hub never shows (Postgres's own, `information_schema`,
// Chelabase's) are refused explicitly, 400 `invalid_request` with the op's `op_index`, before
// any compute; an ordinary schema gets as far as the compute (none in the fake).
#[tokio::test]
async fn privilege_ops_on_hidden_schemas_are_refused_before_any_compute() {
    let t = sql_app().await;
    let main = t.main();
    for schema in ["pg_catalog", "pg_toast", "pg_temp_3", "information_schema", "chelabase", "chelabase_merge", "chelabase_guard"] {
        for op in schema_named_ops(schema, "readers") {
            let body = json!({ "operations": [schema_usage_op("readers"), op.clone()] });
            let err = answer(t.changes(main, body).await, 400).await;
            assert_eq!((code_of(&err), err["error"]["op_index"].as_u64()), ("invalid_request", Some(1)), "{schema}: {op}: {err}");
        }
    }
    assert_eq!(t.compute_lookups(), 0, "refused before any compute");
    for op in schema_named_ops("app", "readers") {
        let resp = t.changes(main, json!({ "operations": [op.clone()] })).await;
        assert_eq!(code_of(&answer(resp, 503).await), "unavailable", "{op}");
    }
}

// T5-M4: an owner run is recognised by the owner's real name too (`Requested::from_name`), so a
// caller below Admin naming it is told `required: "admin"`, as for `owner` in any case.
#[tokio::test]
async fn an_owner_run_named_by_the_owners_real_name_needs_admin() {
    let (t, _dev) = act_as_app().await;
    let main = t.main();
    let path = format!("{}/schema-changes", t.branch(main));
    for role in ["postgres", "owner", "OWNER"] {
        let body = json!({ "role": role, "operations": [schema_usage_op("billing")] });
        let err = answer(t.send_as("dev", Method::POST, &path, Some(body)).await, 403).await;
        assert_eq!((code_of(&err), err["error"]["required"].as_str()), ("forbidden", Some("admin")), "{role}: {err}");
    }
}

/// Waits (bounded) until a backend of `user` in the scratch database waits on a lock.
async fn wait_for_lock_wait(scratch: &ScratchCompute, user: &str) {
    let sql = format!(
        "SELECT EXISTS (SELECT FROM pg_stat_activity WHERE datname = '{}' AND usename = '{user}' AND wait_event_type = 'Lock')",
        scratch.database
    );
    for _ in 0..300 {
        if scratch.yes(&sql).await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("no backend of {user} ever waited on a lock");
}

/// A pool on the scratch database as the cluster's admin (an open transaction's session).
async fn scratch_pool(scratch: &ScratchCompute) -> sqlx::PgPool {
    let opts: sqlx::postgres::PgConnectOptions = (*scratch.admin.connect_options()).clone();
    sqlx::PgPool::connect_with(opts.database(&scratch.database)).await.unwrap()
}

// T4-5: an extension's schema (a schema that is a member of an extension), the objects in it,
// and functions in a hidden schema (named by OID: `pg_catalog.now()`) are refused the same way,
// 400 `invalid_request` with `op_index`, in a dry run too; nothing runs.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn privilege_ops_on_extension_schemas_and_catalog_functions_are_refused() {
    let (scratch, t) = changes_app("hid-pass").await;
    let main = t.main();
    let o = scratch.owner.clone();
    let [reader] = scratch.make_roles(["reader"]).await;
    t.known_roles(&[&reader]);
    scratch
        .exec_in(&format!(
            "CREATE SCHEMA ext_s AUTHORIZATION {o};
             CREATE TABLE ext_s.t (id int); ALTER TABLE ext_s.t OWNER TO {o};
             CREATE FUNCTION ext_s.g() RETURNS int LANGUAGE sql AS 'SELECT 1'; ALTER FUNCTION ext_s.g() OWNER TO {o};
             ALTER EXTENSION plpgsql ADD SCHEMA ext_s;"
        ))
        .await;
    let now_oid: u64 = scratch.text("SELECT 'pg_catalog.now()'::regprocedure::oid::text").await.parse().unwrap();
    let g_oid: u64 = scratch.text("SELECT 'ext_s.g()'::regprocedure::oid::text").await.parse().unwrap();
    let mut ops = schema_named_ops("ext_s", &reader);
    for (oid, identity) in [(now_oid, "pg_catalog.now()"), (g_oid, "ext_s.g()")] {
        let function = json!({ "oid": oid, "identity": identity });
        ops.push(json!({ "grant_function": { "function": function, "grantee": { "role": reader } } }));
        ops.push(json!({ "revoke_function": { "function": function, "grantee": "public" } }));
    }
    for op in ops {
        for dry_run in [true, false] {
            let body = json!({ "dry_run": dry_run, "operations": [schema_usage_op(&reader), op.clone()] });
            let err = answer(t.changes(main, body).await, 400).await;
            assert_eq!((code_of(&err), err["error"]["op_index"].as_u64()), ("invalid_request", Some(1)), "{op} dry_run={dry_run}: {err}");
        }
    }
    for schema in ["public", "ext_s"] {
        assert_eq!(scratch.schema_acl(schema, &reader).await, "", "nothing ran: no ACL entry of the reader on {schema}");
    }
    assert!(scratch.yes("SELECT has_function_privilege('public', 'pg_catalog.now()', 'EXECUTE')").await);
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[reader]).await;
}

// T5-I1: a function's `regprocedure` text is produced under `search_path = ''`, so it names
// the function the read named whatever the role it then runs as resolves. Here the owner's
// `$user` schema holds `f()`, and the function's owner `x1` (whom the grant runs as) has an
// `f()` of its own in its `$user` schema: the grant lands on the read's function only.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_function_grant_run_as_its_owner_names_the_functions_schema() {
    let (scratch, t) = changes_app("fsp-pass").await;
    let main = t.main();
    let o = scratch.owner.clone();
    let [reader, x1] = scratch.make_roles(["reader", "x1"]).await;
    t.known_roles(&[&reader]);
    scratch.exec_cluster(&format!("GRANT {x1} TO {o} WITH INHERIT FALSE, SET TRUE")).await;
    scratch
        .exec_in(&format!(
            "CREATE SCHEMA {o} AUTHORIZATION {o}; GRANT USAGE ON SCHEMA {o} TO {x1};
             CREATE SCHEMA {x1} AUTHORIZATION {x1};
             CREATE FUNCTION {o}.f() RETURNS int LANGUAGE sql AS 'SELECT 1'; ALTER FUNCTION {o}.f() OWNER TO {x1};
             CREATE FUNCTION {x1}.f() RETURNS int LANGUAGE sql AS 'SELECT 2'; ALTER FUNCTION {x1}.f() OWNER TO {x1};
             REVOKE EXECUTE ON FUNCTION {o}.f() FROM PUBLIC; REVOKE EXECUTE ON FUNCTION {x1}.f() FROM PUBLIC;"
        ))
        .await;
    let db = scratch.database.clone();
    let page = answer(t.privileges(main, &db, &format!("?role={reader}&schema={o}")).await, 200).await;
    let f = listed(&page, "f");
    let (oid, identity) = (f["oid"].as_u64().unwrap(), f["identity"].as_str().unwrap().to_string());
    let ops = json!([{ "grant_function": { "function": { "oid": oid, "identity": identity }, "grantee": { "role": reader } } }]);
    let real = answer(t.changes(main, json!({ "operations": ops })).await, 200).await;
    assert_eq!(real["committed"], true, "{real}");
    assert!(scratch.yes(&format!("SELECT has_function_privilege('{reader}', '{o}.f()', 'EXECUTE')")).await, "the read's function");
    assert!(!scratch.yes(&format!("SELECT has_function_privilege('{reader}', '{x1}.f()', 'EXECUTE')")).await, "not x1's own f()");
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[x1, reader]).await;
}

// T5-I2: verify runs whenever every statement ran or was skipped as already dropped: in a
// request that drops an enum (cascading to a column) and the column (skipped), a revoke another
// grantor's entry survives is still 409 `privileges_not_applied`, and the whole request rolls
// back.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_mixed_request_with_a_skipped_drop_still_verifies() {
    let (scratch, t) = changes_app("mixv-pass").await;
    let main = t.main();
    let o = scratch.owner.clone();
    let [reader, mid] = scratch.make_roles(["reader", "mid"]).await;
    scratch
        .exec_in(&format!(
            "CREATE TYPE public.mood AS ENUM ('ok'); ALTER TYPE public.mood OWNER TO {o};
             ALTER TABLE public.notes ADD COLUMN m public.mood;
             GRANT SELECT ON public.notes TO {mid} WITH GRANT OPTION;
             SET ROLE {mid}; GRANT SELECT ON public.notes TO {reader}; RESET ROLE;"
        ))
        .await;
    let ops = json!([
        { "drop_enum": { "schema": "public", "name": "mood" } },
        { "drop_column": { "table": public_table("notes"), "column": "m" } },
        revoke_op("notes", &["select"], &reader),
    ]);
    let none = no_dependents_hash();
    let err = answer(t.changes(main, json!({ "operations": ops, "cascade": { "dependents_hash": none } })).await, 409).await;
    assert_eq!(code_of(&err), "privileges_not_applied", "{err}");
    let want = json!({ "object": relation_target("notes"), "privilege": "SELECT", "grantee": reader,
        "still_granted_by": mid, "column": null, "wanted": false });
    assert!(err["error"]["mismatches"].as_array().is_some_and(|m| m.contains(&want)), "{want} in {err}");
    assert!(scratch.yes("SELECT to_regtype('public.mood') IS NOT NULL").await, "rolled back");
    assert_eq!(scratch.acl("public.notes", &reader).await, format!("{mid}:SELECT"));
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[reader, mid]).await;
}

// T5-M1: a statement's `not applied` warning (01006/01007) is still 409
// `privileges_not_applied`, and the answer names what differs (the re-read runs before
// failing): a grant that didn't land (`wanted: true`) and a revoke another grantor's entry
// survives (`still_granted_by`).
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_warned_statement_names_its_mismatches() {
    let (scratch, t) = changes_app("warn2-pass").await;
    let main = t.main();
    let o = scratch.owner.clone();
    let [grp, g, h] = scratch.make_roles(["grp", "g", "h"]).await;
    let (_, member_role) = developer_acting_as(&t, &scratch, &grp).await;
    scratch.exec_in(&format!("GRANT SELECT ON public.notes TO {grp}; GRANT SELECT ON public.notes TO {h}")).await;
    let path = format!("{}/schema-changes", t.branch(main));
    let cases = [
        (grant_op("notes", &["select"], &g), json!({ "object": relation_target("notes"), "privilege": "SELECT", "grantee": g,
            "still_granted_by": null, "column": null, "wanted": true })),
        (revoke_op("notes", &["select"], &h), json!({ "object": relation_target("notes"), "privilege": "SELECT", "grantee": h,
            "still_granted_by": o, "column": null, "wanted": false })),
    ];
    for (op, want) in cases {
        let body = json!({ "role": grp, "operations": [op.clone()] });
        let err = answer(t.send_as("dev-a", Method::POST, &path, Some(body)).await, 409).await;
        assert_eq!(code_of(&err), "privileges_not_applied", "{op}: {err}");
        assert_eq!(err["error"]["op_index"], 0, "{op}: {err}");
        assert!(err["error"]["mismatches"].as_array().is_some_and(|m| m.contains(&want)), "{op}: {want} in {err}");
    }
    assert_eq!(scratch.acl("public.notes", &g).await, "");
    assert_eq!(scratch.acl("public.notes", &h).await, format!("{o}:SELECT"));
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[member_role, grp, g, h]).await;
}

// T5-M2: the `expect_acl` check (and the owners' read) runs after the request's locks: here the
// request waits for its drop's `LOCK TABLE` while another session changes the ACL it read and
// commits; the request then answers 409 `privileges_changed_since_read` and changes nothing.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn the_acl_check_runs_after_the_requests_locks() {
    let (scratch, t) = changes_app("lockh-pass").await;
    let main = t.main();
    let o = scratch.owner.clone();
    let [reader, other] = scratch.make_roles(["reader", "other"]).await;
    t.known_roles(&[&reader]);
    scratch.exec_in(&format!("CREATE TABLE public.t2 (id int, c int); ALTER TABLE public.t2 OWNER TO {o};")).await;
    let db = scratch.database.clone();
    let page = answer(t.privileges(main, &db, &format!("?role={reader}&schema=public")).await, 200).await;
    let hash = listed(&page, "notes")["acl_hash"].as_str().unwrap().to_string();
    let blocker = scratch_pool(&scratch).await;
    let mut tx = blocker.begin().await.unwrap();
    sqlx::query("SELECT count(*) FROM public.t2").execute(&mut *tx).await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("GRANT INSERT ON public.notes TO {other}"))).execute(&mut *tx).await.unwrap();
    let body = json!({
        "lock_timeout_ms": 30000,
        "operations": [{ "drop_column": { "table": public_table("t2"), "column": "c" } }, grant_op("notes", &["select"], &reader)],
        "expect_acl": [{ "object": relation_target("notes"), "acl_hash": hash }],
    });
    let (resp, ()) = tokio::join!(t.changes(main, body), async {
        wait_for_lock_wait(&scratch, &o).await;
        tx.commit().await.unwrap();
    });
    let err = answer(resp, 409).await;
    assert_eq!(code_of(&err), "privileges_changed_since_read", "{err}");
    assert_eq!(err["error"]["objects"], json!([relation_target("notes")]), "{err}");
    assert!(scratch.yes("SELECT EXISTS (SELECT FROM pg_attribute WHERE attrelid = 'public.t2'::regclass AND attname = 'c' AND NOT attisdropped)").await);
    assert_eq!(scratch.acl("public.notes", &reader).await, "");
    blocker.close().await;
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[reader, other]).await;
}

// T5-M3: after a statement run as an object's owner X, only the database owner's own
// membership rows of X are compared: a membership of X granted to another role meanwhile (here
// while the statement waits on another session's uncommitted default-privileges row) is no
// spurious 500; the request commits.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn an_unrelated_membership_of_the_owner_role_is_no_failure() {
    let (scratch, t) = changes_app("memb-pass").await;
    let main = t.main();
    let o = scratch.owner.clone();
    let [reader, x1, other] = scratch.make_roles(["reader", "x1", "other"]).await;
    scratch.exec_cluster(&format!("GRANT {x1} TO {o} WITH INHERIT FALSE, SET TRUE")).await;
    let before = scratch.memberships(&o).await;
    let blocker = scratch_pool(&scratch).await;
    let mut tx = blocker.begin().await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "ALTER DEFAULT PRIVILEGES FOR ROLE {x1} IN SCHEMA public GRANT SELECT ON TABLES TO {other}"
    )))
    .execute(&mut *tx)
    .await
    .unwrap();
    let body = json!({ "lock_timeout_ms": 30000, "operations": [default_select_op(&x1, true, &reader)] });
    let (resp, ()) = tokio::join!(t.changes(main, body), async {
        wait_for_lock_wait(&scratch, &o).await;
        scratch.exec_cluster(&format!("GRANT {x1} TO {other}")).await;
        tx.rollback().await.unwrap();
    });
    let real = answer(resp, 200).await;
    assert_eq!(real["committed"], true, "{real}");
    assert_eq!(scratch.default_acl(&x1, &reader).await, format!("{x1}:SELECT"));
    assert_eq!(scratch.memberships(&o).await, before, "P26: the owner's own rows are unchanged");
    blocker.close().await;
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[other, x1, reader]).await;
}

// T5-M5: an aggregate (or window function) named by OID is refused cleanly, 400
// `invalid_request` with `op_index` (the editor offers functions and procedures only), never a
// 404 that says it doesn't exist; a dry run's answer carries `dry_run: true`.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn an_aggregate_is_refused_not_missing() {
    let (scratch, t) = changes_app("agg-pass").await;
    let main = t.main();
    let o = scratch.owner.clone();
    let [reader] = scratch.make_roles(["reader"]).await;
    t.known_roles(&[&reader]);
    scratch
        .exec_in(&format!(
            "CREATE AGGREGATE public.agg(integer) (SFUNC = int4pl, STYPE = integer); ALTER AGGREGATE public.agg(integer) OWNER TO {o};"
        ))
        .await;
    let oid: u64 = scratch.text("SELECT 'public.agg(integer)'::regprocedure::oid::text").await.parse().unwrap();
    let function = json!({ "oid": oid, "identity": "public.agg(integer)" });
    for op in [
        json!({ "grant_function": { "function": function, "grantee": { "role": reader } } }),
        json!({ "revoke_function": { "function": function, "grantee": "public" } }),
    ] {
        for dry_run in [true, false] {
            let err = answer(t.changes(main, json!({ "dry_run": dry_run, "operations": [op.clone()] })).await, 400).await;
            assert_eq!((code_of(&err), err["error"]["op_index"].as_u64()), ("invalid_request", Some(0)), "{op}: {err}");
            assert_eq!(err["error"]["dry_run"].as_bool(), dry_run.then_some(true), "{op}: {err}");
        }
    }
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[reader]).await;
}

// EOS-H1: a secret key's role may not be, or inherit, a role with the hub's "all rows" policy
// on the branch (every row would reach the data API through the key): creating the key is 422
// `exposes_data_api` with the chain from the key's role, and no key is stored. A role that
// reaches no such role gets its key.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_key_for_a_role_reaching_an_all_rows_role_is_refused() {
    let (scratch, t) = changes_app("keyx-pass").await;
    let main = t.main();
    let [r, k, s] = scratch.make_roles(["r", "k", "s"]).await;
    t.known_roles(&[&r, &k, &s]);
    scratch.exec_in("ALTER TABLE public.notes ENABLE ROW LEVEL SECURITY").await;
    let on = answer(t.changes(main, json!({ "operations": [all_rows_op("notes", &r, true)] })).await, 200).await;
    assert_eq!(on["committed"], true, "{on}");
    scratch.exec_cluster(&format!("GRANT {r} TO {k}")).await;
    // final B2 (fail closed, via the worker): the check is the worker's now; the fake worker
    // answers what the worker's own check reads on this compute (worker unit and live tests),
    // and the request must name the key's role alone.
    for (role, chain) in [(&k, json!([k, r])), (&r, json!([r]))] {
        let reply = chelabase_nats::AllRowsCheckReply::Exposed { chain: serde_json::from_value(chain.clone()).unwrap() };
        *t.jobs.all_rows_reply.lock().unwrap() = Some(Ok(reply));
        let err = answer(t.send(Method::POST, &t.keys(main), Some(json!({ "name": "ci", "role": role }))).await, 422).await;
        assert_eq!(code_of(&err), "exposes_data_api", "{role}: {err}");
        assert_eq!(err["error"]["chain"], chain, "{role}: {err}");
        let asked = t.jobs.all_rows_checks.lock().unwrap().last().cloned().expect("the worker is asked");
        assert_eq!((asked.branch_id, asked.starting, asked.change), (main, vec![role.to_string()], None));
    }
    *t.jobs.all_rows_reply.lock().unwrap() = None;
    assert!(t.list_keys(main).await.iter().all(|key| key["name"] != "ci"), "no key stored");
    let resp = t.send(Method::POST, &t.keys(main), Some(json!({ "name": "safe", "role": s }))).await;
    assert_eq!(resp.status(), 201, "{:?}", resp.text().await);
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[k, s, r]).await;
}

// EOS-H1: a role change that would make a live secret key's role reach a role with the hub's
// "all rows" policy (the key's role itself, or a role it inherits, joining it) is 422
// `exposes_data_api` with the chain from the key's role, and the worker is never asked; a
// change that exposes nothing goes through.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn a_role_change_exposing_a_keys_role_to_all_rows_is_refused() {
    let (scratch, t) = changes_app("keyr-pass").await;
    let main = t.main();
    let [r, k, m] = scratch.make_roles(["r", "k", "m"]).await;
    t.known_roles(&[&r, &k, &m]);
    let ep = t.metadata.branch(main).endpoint_id.unwrap();
    for role in [&k, &m] {
        t.metadata.add_role(&ep, role, RoleOrigin::Sql, false, false);
    }
    scratch.exec_in("ALTER TABLE public.notes ENABLE ROW LEVEL SECURITY").await;
    let on = answer(t.changes(main, json!({ "operations": [all_rows_op("notes", &r, true)] })).await, 200).await;
    assert_eq!(on["committed"], true, "{on}");
    scratch.exec_cluster(&format!("GRANT {m} TO {k}")).await;
    t.seed_secret_key(main, "ci", &k);
    // final B2 (fail closed, via the worker): the fake worker answers what the worker's own
    // check reads; the request carries the data API roles (the key's role among them) and the
    // change.
    for (role, chain) in [(&m, json!([k, m, r])), (&k, json!([k, r]))] {
        let reply = chelabase_nats::AllRowsCheckReply::Exposed { chain: serde_json::from_value(chain.clone()).unwrap() };
        *t.jobs.all_rows_reply.lock().unwrap() = Some(Ok(reply));
        let path = format!("{}/{role}", t.roles(main));
        let err = answer(t.send(Method::PATCH, &path, Some(json!({ "member_of": [r] }))).await, 422).await;
        assert_eq!(code_of(&err), "exposes_data_api", "{role}: {err}");
        assert_eq!(err["error"]["chain"], chain, "{role}: {err}");
        let asked = t.jobs.all_rows_checks.lock().unwrap().last().cloned().expect("the worker is asked");
        assert!(asked.starting.contains(&k), "{asked:?}");
        assert_eq!(
            asked.change,
            Some(chelabase_nats::MembershipChange { role: role.to_string(), member_of: vec![r.clone()] })
        );
    }
    *t.jobs.all_rows_reply.lock().unwrap() = None;
    assert!(t.jobs.role_calls.lock().unwrap().iter().all(|c| !c.starts_with("update:")), "the worker is never asked");
    let path = format!("{}/{m}", t.roles(main));
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "member_of": [] }))).await.status(), 202);
    wait_no_editor_sessions(&scratch).await;
    scratch.drop_with(&[k, m, r]).await;
}

// ---- final B2: EOS-H1 fails closed, via the worker (user ruling 2026-10-23)
//
// Key creation and a role's `member_of` change ask the worker for the all-rows check
// (`chelabase.rpc.roles.all_rows_check`, run as `cloud_admin`): an exposure is 422
// `exposes_data_api` with the worker's chain; a check that can't complete (the worker's
// `failed`, a timeout, no worker) is 503 and nothing is created or sent; never allowed.

fn all_rows_exposed(chain: &[&str]) -> chelabase_nats::AllRowsCheckReply {
    chelabase_nats::AllRowsCheckReply::Exposed { chain: chain.iter().map(|r| r.to_string()).collect() }
}

/// The ways an all-rows check can't complete, and the 503 code each answers.
fn all_rows_failures() -> [(Result<chelabase_nats::AllRowsCheckReply, ResetError>, &'static str); 3] {
    [
        (Ok(chelabase_nats::AllRowsCheckReply::Failed { message: "compute unreachable: secret detail".into() }), "unavailable"),
        (Err(ResetError::Timeout), "unavailable"),
        (Err(ResetError::Transport("no worker listening".into())), "worker_unavailable"),
    ]
}

#[tokio::test]
async fn key_create_asks_the_worker_for_the_all_rows_check() {
    let t = branching().await;
    t.seed_public_key(t.main());
    let resp = t.send(Method::POST, &t.keys(t.main()), Some(json!({ "name": "ci", "role": "reporting" }))).await;
    assert_eq!(
        *t.jobs.all_rows_checks.lock().unwrap(),
        vec![chelabase_nats::AllRowsCheckRequest { branch_id: t.main(), starting: vec!["reporting".into()], change: None }],
        "a key create asks for its role alone, with no change"
    );
    assert_eq!(resp.status(), 201, "a clear check creates the key");
    assert!(t.metadata.keys.lock().unwrap().iter().any(|(k, _)| k.name == "ci"));
}

#[tokio::test]
async fn key_create_the_worker_finds_exposing_is_422_with_its_chain() {
    let t = branching().await;
    *t.jobs.all_rows_reply.lock().unwrap() = Some(Ok(all_rows_exposed(&["reporting", "grp", "everything"])));
    let err = error_of(
        t.send(Method::POST, &t.keys(t.main()), Some(json!({ "name": "ci", "role": "reporting" }))).await,
        422,
    )
    .await;
    assert_eq!(code_of(&err), "exposes_data_api", "{err}");
    assert_eq!(err["error"]["chain"], json!(["reporting", "grp", "everything"]), "{err}");
    assert!(t.metadata.keys.lock().unwrap().is_empty(), "no key stored");
}

#[tokio::test]
async fn key_create_fails_closed_when_the_all_rows_check_cannot_complete() {
    let t = branching().await;
    for (answer, code) in all_rows_failures() {
        *t.jobs.all_rows_reply.lock().unwrap() = Some(answer.clone());
        let resp = t.send(Method::POST, &t.keys(t.main()), Some(json!({ "name": "ci", "role": "reporting" }))).await;
        assert_eq!(resp.status(), 503, "{answer:?}: never allowed on a failed check");
        let err: Value = resp.json().await.unwrap();
        assert_eq!(code_of(&err), code, "{answer:?}: {err}");
        assert!(!message_of(&err).contains("secret detail"), "{err}");
        assert!(t.metadata.keys.lock().unwrap().is_empty(), "{answer:?}: no key stored");
    }
}

/// `app` on a ready `dev` branch, with a secret key on role `k`, a revoked one on role
/// `gone`, the public key (`anon`) and, on main, a key on role `elsewhere`.
fn membership_change_setup(t: &Branching) -> BranchId {
    let dev = t.ready("dev");
    let ep = t.with_owner_row(dev);
    t.metadata.add_role(&ep, "app", RoleOrigin::Api, false, false);
    t.seed_public_key(dev);
    t.seed_secret_key(dev, "ci", "k");
    let (revoked, _) = t.seed_secret_key(dev, "old", "gone");
    for (key, _) in t.metadata.keys.lock().unwrap().iter_mut() {
        if key.id == revoked {
            key.revoked_at = Some(chrono::Utc::now());
        }
    }
    t.seed_secret_key(t.main(), "other", "elsewhere");
    dev
}

#[tokio::test]
async fn a_membership_change_asks_the_worker_with_the_data_api_roles() {
    let t = branching().await;
    let dev = membership_change_setup(&t);
    let path = format!("{}/app", t.roles(dev));
    let resp = t.send(Method::PATCH, &path, Some(json!({ "member_of": ["readers"] }))).await;
    assert_eq!(
        *t.jobs.all_rows_checks.lock().unwrap(),
        vec![chelabase_nats::AllRowsCheckRequest {
            branch_id: dev,
            starting: ["anon", "authenticated", "authenticator", "k"].map(String::from).to_vec(),
            change: Some(chelabase_nats::MembershipChange { role: "app".into(), member_of: vec!["readers".into()] }),
        }],
        "the data API roles and this branch's live keys' roles, sorted, deduplicated; the change as sent"
    );
    assert_eq!(resp.status(), 202, "a clear check goes on to the worker's update");
    assert_eq!(*t.jobs.role_calls.lock().unwrap(), vec!["update:app".to_string()]);
    // A change without `member_of` isn't checked; a membership refused here isn't either.
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "inherit": false }))).await.status(), 202);
    let err = error_of(t.send(Method::PATCH, &path, Some(json!({ "member_of": ["anon"] }))).await, 422).await;
    assert_eq!(code_of(&err), "role_membership_not_allowed");
    assert_eq!(t.jobs.all_rows_checks.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_membership_change_the_worker_finds_exposing_is_422_and_never_applied() {
    let t = branching().await;
    let dev = membership_change_setup(&t);
    *t.jobs.all_rows_reply.lock().unwrap() = Some(Ok(all_rows_exposed(&["k", "app", "everything"])));
    let path = format!("{}/app", t.roles(dev));
    let err = error_of(t.send(Method::PATCH, &path, Some(json!({ "member_of": ["everything"] }))).await, 422).await;
    assert_eq!(code_of(&err), "exposes_data_api", "{err}");
    assert_eq!(err["error"]["chain"], json!(["k", "app", "everything"]), "{err}");
    assert!(t.jobs.role_calls.lock().unwrap().is_empty(), "the update is never sent");
}

#[tokio::test]
async fn a_membership_change_fails_closed_when_the_all_rows_check_cannot_complete() {
    let t = branching().await;
    let dev = membership_change_setup(&t);
    let path = format!("{}/app", t.roles(dev));
    for (answer, code) in all_rows_failures() {
        *t.jobs.all_rows_reply.lock().unwrap() = Some(answer.clone());
        let resp = t.send(Method::PATCH, &path, Some(json!({ "member_of": ["everything"] }))).await;
        assert_eq!(resp.status(), 503, "{answer:?}: never allowed on a failed check");
        let err: Value = resp.json().await.unwrap();
        assert_eq!(code_of(&err), code, "{answer:?}: {err}");
        assert!(!message_of(&err).contains("secret detail"), "{err}");
    }
    assert!(t.jobs.role_calls.lock().unwrap().is_empty(), "the update is never sent");
}

// ---- Members hub step 2 (T23, part B): invites, roster and reads around teams ----------------

mod t23_teams {
    use super::*;

    /// A tenant invite body with these teams and no grants.
    fn invite_with_teams(email: &str, teams: Value) -> Value {
        json!({ "email": email, "grants": [], "team_ids": teams })
    }

    fn members_of(t: &Mw, team: uuid::Uuid) -> Vec<uuid::Uuid> {
        t.w.metadata.stored_team(team).map(|x| x.members.into_iter().collect()).unwrap_or_default()
    }

    fn count(t: &Mw, action: &str) -> usize {
        t.actions().iter().filter(|a| *a == action).count()
    }

    /// The member's row of the roster as `sub` sees it.
    async fn roster_row(t: &Mw, sub: &str, member: uuid::Uuid) -> Value {
        let list = body_of(t.send(sub, Method::GET, "/v1/members", None).await).await;
        list.as_array()
            .unwrap_or_else(|| panic!("the roster: {list}"))
            .iter()
            .find(|m| m["id"] == json!(member))
            .cloned()
            .unwrap_or_else(|| panic!("{member} isn't listed: {list}"))
    }

    // MH-8, P31: the project invite refuses `team_ids` in any form, `[]` and `null` included; the
    // same invite without the field goes through.
    #[tokio::test]
    async fn project_invite_refuses_team_ids() {
        let t = mw(Mode::SelfHosted).await;
        let team = t.w.metadata.seed_team(t.w.tenant, "Billing team");
        let path = format!("/v1/projects/{}/invites", t.w.project);
        for teams in [json!([]), json!([team]), Value::Null] {
            let body = json!({ "email": "eve@x.com", "platform_role": "viewer", "pg_roles": [], "team_ids": teams });
            let err = error_of(t.send(DEFAULT_SUB, Method::POST, &path, Some(body)).await, 422).await;
            assert_eq!(code_of(&err), "invalid_request", "{teams}: {err}");
            assert_eq!(message_of(&err), "teams can be attached only by a tenant admin's invite", "{teams}: {err}");
        }
        assert!(t.identity.user_by_email(&Pool::Members, "eve@x.com").is_none(), "no identity user was made");
        assert!(t.actions().is_empty(), "nothing written: {:?}", t.actions());
        assert!(members_of(&t, team).is_empty());
        let body = json!({ "email": "eve@x.com", "platform_role": "viewer", "pg_roles": [] });
        assert_eq!(t.send(DEFAULT_SUB, Method::POST, &path, Some(body)).await.status(), 201);
    }

    // P12: `team_ids` on the tenant invite adds the memberships in the invite's transaction on
    // every outcome, one `team_member_added` each; the reconcile covers the teams' projects and
    // the answer's access counts them (T9-I1: built from what was written).
    #[tokio::test]
    async fn tenant_invite_with_team_ids_adds_memberships_on_every_outcome() {
        let t = mw(Mode::SelfHosted).await;
        let team = t.w.metadata.seed_team(t.w.tenant, "Billing team");
        t.w.metadata.seed_team_grant(team, t.w.project, PlatformRole::Developer, &[]);
        let z = t.identity.with_user(&Pool::Members, "dana@x.com", "Secret-pass1");
        let bob = t.w.metadata.add_member(t.w.tenant, "bob", MemberStatus::Active, false);
        t.w.metadata.set_member(t.w.tenant, "bob", |m| m.email = Some("bob@x.com".into()));
        for (email, outcome, status) in [("new@x.com", "invited", 201), ("dana@x.com", "invited", 201), ("bob@x.com", "existing_member", 200)] {
            t.clear_enqueued();
            let added = count(&t, "team_member_added");
            let resp = t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_with_teams(email, json!([team])))).await;
            assert_eq!(resp.status(), status, "{email}");
            let body = body_of(resp).await;
            assert_eq!(body["outcome"], outcome, "{body}");
            let id: uuid::Uuid = body["member"]["id"].as_str().unwrap().parse().unwrap();
            assert!(members_of(&t, team).contains(&id), "{outcome}: the membership is written");
            assert_eq!(count(&t, "team_member_added"), added + 1, "{outcome}: one event per team");
            assert_eq!(t.enqueued(), t.branches(), "{outcome}: the team's project reconciles");
            assert_eq!(body["member"]["has_access"], true, "{outcome}: {body}");
            assert_eq!(
                body["member"]["access"]["projects"],
                json!([{ "project_id": t.w.project, "platform_role": "developer", "via": "team" }]),
                "{outcome}: {body}"
            );
        }
        assert_eq!(t.w.metadata.member(t.w.tenant, &z).map(|m| members_of(&t, team).contains(&m.id)), Some(true));
        assert!(members_of(&t, team).contains(&bob));
    }

    // P7, P12: an invite's grants and teams are checked together on the union, per project: an
    // existing member narrowed on web1 to {Viewer, [login]} can't join an Admin team (R17 keeps
    // the override's `login`); a new person can't get a team's `boss` at Developer. Refused with
    // the team and project named; nothing written.
    #[tokio::test]
    async fn invite_team_ids_are_checked_on_the_union() {
        let t = mw(Mode::SelfHosted).await;
        t.second_database();
        let admins = t.w.metadata.seed_team(t.w.tenant, "Admins");
        t.w.metadata.seed_team_grant(admins, t.w.project, PlatformRole::Admin, &[]);
        let bosses = t.w.metadata.seed_team(t.w.tenant, "Bosses");
        t.w.metadata.seed_team_grant(bosses, t.w.project, PlatformRole::Developer, &["boss"]);
        let m = t.w.metadata.add_member(t.w.tenant, "m", MemberStatus::Active, false);
        t.w.metadata.set_member(t.w.tenant, "m", |r| r.email = Some("m@x.com".into()));
        t.w.metadata.grant(m, t.w.project, PlatformRole::Developer, &["plain"]);
        t.w.metadata.narrow(m, t.w.db, OverrideRow { platform_role: Some(PlatformRole::Viewer), pg_roles: Some(vec!["login".into()]) });
        let err = error_of(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_with_teams("m@x.com", json!([admins])))).await, 422).await;
        assert_eq!(code_of(&err), "invalid_request", "{err}");
        assert_eq!(
            (err["error"]["role"].as_str(), err["error"]["database_id"].clone(), err["error"]["team_id"].clone(), err["error"]["project_id"].clone()),
            (Some("login"), json!(t.w.db), json!(admins), json!(t.w.project)),
            "{err}"
        );
        assert!(members_of(&t, admins).is_empty(), "nothing written");
        // A new person: grants and teams together. `plain` alone is fine; the team's `boss` isn't.
        let body = json!({ "email": "new@x.com", "grants": [{ "project_id": t.w.project, "platform_role": "developer", "pg_roles": ["plain"] }], "team_ids": [bosses] });
        let err = error_of(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(body)).await, 422).await;
        assert_eq!((err["error"]["role"].as_str(), err["error"]["team_id"].clone()), (Some("boss"), json!(bosses)), "{err}");
        assert!(members_of(&t, bosses).is_empty());
        assert!(
            !t.w.metadata.members.lock().unwrap().iter().any(|r| r.email.as_deref() == Some("new@x.com")),
            "no member row"
        );
        assert!(t.actions().is_empty(), "{:?}", t.actions());
    }

    // Final fix round, defect 3: an invite's or approval's grants and teams are checked on the
    // union they write, not step by step. An own Viewer `{login}` (a LOGIN role) with a team
    // granting Admin is Admin from a source (R17): valid, so the invite and the approval go
    // through. A union that is invalid in the end is still refused: naming the team when its
    // grant is why, and no team when the own grant's role is.
    #[tokio::test]
    async fn invite_teams_are_checked_on_the_written_union_not_step_by_step() {
        let t = mw(Mode::SelfHosted).await;
        t.second_database();
        let admins = t.w.metadata.seed_team(t.w.tenant, "Admins");
        t.w.metadata.seed_team_grant(admins, t.w.project, PlatformRole::Admin, &[]);
        let viewer_login = json!([{ "project_id": t.w.project, "platform_role": "viewer", "pg_roles": ["login"] }]);
        let body = json!({ "email": "new@x.com", "grants": viewer_login, "team_ids": [admins] });
        let resp = t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(body)).await;
        let status = resp.status();
        let answer: Value = resp.json().await.unwrap_or(Value::Null);
        assert_eq!(status, 201, "the invite: {answer}");
        let pat = t.w.metadata.add_member(t.w.tenant, "pat", MemberStatus::Pending, false);
        let body = json!({ "grants": viewer_login, "team_ids": [admins] });
        let resp = t.send(DEFAULT_SUB, Method::POST, &format!("/v1/members/{pat}/approve"), Some(body)).await;
        let status = resp.status();
        let answer: Value = resp.json().await.unwrap_or(Value::Null);
        assert_eq!(status, 200, "the approval: {answer}");
        assert!(members_of(&t, admins).contains(&pat));

        // Invalid in the end because of the team: `boss` at Developer reaches the owner on web1.
        let bosses = t.w.metadata.seed_team(t.w.tenant, "Bosses");
        t.w.metadata.seed_team_grant(bosses, t.w.project, PlatformRole::Developer, &["boss"]);
        let body = json!({ "email": "b@x.com", "grants": [{ "project_id": t.w.project, "platform_role": "viewer", "pg_roles": [] }], "team_ids": [bosses] });
        let err = error_of(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(body)).await, 422).await;
        assert_eq!(
            (err["error"]["role"].as_str(), err["error"]["team_id"].clone(), err["error"]["project_id"].clone()),
            (Some("boss"), json!(bosses), json!(t.w.project)),
            "{err}"
        );
        // Invalid in the end because of the own grant: `login` at Developer (a team's level).
        let devs = t.w.metadata.seed_team(t.w.tenant, "Devs");
        t.w.metadata.seed_team_grant(devs, t.w.project, PlatformRole::Developer, &["plain"]);
        let body = json!({ "email": "c@x.com", "grants": viewer_login, "team_ids": [devs] });
        let err = error_of(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(body)).await, 422).await;
        assert_eq!((err["error"]["role"].as_str(), err["error"]["project_id"].clone()), (Some("login"), json!(t.w.project)), "{err}");
        assert!(err["error"]["team_id"].is_null(), "the own grant is why: {err}");
        assert!(members_of(&t, bosses).is_empty() && members_of(&t, devs).is_empty(), "nothing written");
    }

    // P12: approval takes `team_ids` as the invite does: memberships in the approval's
    // transaction, one event each, the teams' projects reconciled, the answer's access counting
    // them; and the same union check.
    #[tokio::test]
    async fn approve_with_team_ids() {
        let t = mw(Mode::SelfHosted).await;
        t.second_database();
        let team = t.w.metadata.seed_team(t.w.tenant, "Billing team");
        t.w.metadata.seed_team_grant(team, t.w.project, PlatformRole::Developer, &["plain"]);
        let pat = t.w.metadata.add_member(t.w.tenant, "pat", MemberStatus::Pending, false);
        let path = format!("/v1/members/{pat}/approve");
        let resp = t.send(DEFAULT_SUB, Method::POST, &path, Some(json!({ "grants": [], "team_ids": [team] }))).await;
        assert_eq!(resp.status(), 200);
        let member = body_of(resp).await;
        assert_eq!((member["status"].as_str(), member["has_access"].as_bool()), (Some("active"), Some(true)), "{member}");
        assert_eq!(member["access"]["projects"], json!([{ "project_id": t.w.project, "platform_role": "developer", "via": "team" }]), "{member}");
        assert!(members_of(&t, team).contains(&pat));
        assert_eq!(t.actions(), vec!["member_approved", "team_member_added"]);
        assert_eq!(t.enqueued(), {
            let mut b = t.branches();
            b.push(BranchId::from(DatabaseId(t.w.metadata.databases.lock().unwrap().values().find(|d| d.name == "web2").unwrap().database_id.as_uuid())));
            b.sort_by_key(|b| b.as_uuid());
            b
        });
        // The union check: a team's `boss` at Developer is refused (web1), nothing written.
        let bosses = t.w.metadata.seed_team(t.w.tenant, "Bosses");
        t.w.metadata.seed_team_grant(bosses, t.w.project, PlatformRole::Developer, &["boss"]);
        let sam = t.w.metadata.add_member(t.w.tenant, "sam", MemberStatus::Pending, false);
        let err = error_of(
            t.send(DEFAULT_SUB, Method::POST, &format!("/v1/members/{sam}/approve"), Some(json!({ "team_ids": [bosses] }))).await,
            422,
        )
        .await;
        assert_eq!((err["error"]["role"].as_str(), err["error"]["team_id"].clone()), (Some("boss"), json!(bosses)), "{err}");
        assert_eq!(t.w.metadata.member(t.w.tenant, "sam").unwrap().status, MemberStatus::Pending);
        assert!(members_of(&t, bosses).is_empty());
    }

    // P12: at most 50 ids, deduplicated, each a team of the caller's tenant, else 422 naming the
    // id; nothing written. Duplicates count once.
    #[tokio::test]
    async fn unknown_foreign_or_too_many_team_ids_are_422() {
        let t = mw(Mode::SelfHosted).await;
        let team = t.w.metadata.seed_team(t.w.tenant, "Ours");
        let foreign = t.w.metadata.seed_team(TenantId::new(), "Theirs");
        let unknown = uuid::Uuid::new_v4();
        let pat = t.w.metadata.add_member(t.w.tenant, "pat", MemberStatus::Pending, false);
        let approve = format!("/v1/members/{pat}/approve");
        let many: Vec<uuid::Uuid> = (0..51).map(|_| uuid::Uuid::new_v4()).collect();
        for bad in [unknown, foreign] {
            for (path, body) in [
                ("/v1/invites".to_string(), invite_with_teams("new@x.com", json!([team, bad]))),
                (approve.clone(), json!({ "team_ids": [bad] })),
            ] {
                let err = error_of(t.send(DEFAULT_SUB, Method::POST, &path, Some(body)).await, 422).await;
                assert_eq!(code_of(&err), "invalid_request", "{path}: {err}");
                assert_eq!(err["error"]["team_id"], json!(bad), "{path}: the id is named: {err}");
            }
        }
        for (path, body) in [("/v1/invites".to_string(), invite_with_teams("new@x.com", json!(many))), (approve.clone(), json!({ "team_ids": many }))] {
            let err = error_of(t.send(DEFAULT_SUB, Method::POST, &path, Some(body)).await, 422).await;
            assert_eq!(code_of(&err), "invalid_request", "{path}: {err}");
        }
        assert!(t.actions().is_empty(), "{:?}", t.actions());
        assert!(t.identity.user_by_email(&Pool::Members, "new@x.com").is_none(), "refused before the identity service");
        // 50 is the limit after deduplication: one id 51 times is one team.
        let same: Vec<uuid::Uuid> = vec![team; 51];
        let resp = t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_with_teams("new@x.com", json!(same)))).await;
        assert_eq!(resp.status(), 201);
        assert_eq!(count(&t, "team_member_added"), 1);
    }

    // P25: a team grant changed between the invite's check and its write is `Changed`: the
    // invite is checked again on the new team and written; a second change is a 409 with
    // nothing written.
    #[tokio::test]
    async fn an_invite_racing_a_team_grant_change_is_retried() {
        let t = mw(Mode::SelfHosted).await;
        let team = t.w.metadata.seed_team(t.w.tenant, "Racy");
        t.w.metadata.seed_team_grant(team, t.w.project, PlatformRole::Viewer, &[]);
        let project = t.w.project;
        t.w.metadata.race_teams_once(move |md| md.seed_team_grant(team, project, PlatformRole::Developer, &[]));
        let resp = t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_with_teams("new@x.com", json!([team])))).await;
        assert_eq!(resp.status(), 201);
        let body = body_of(resp).await;
        let id: uuid::Uuid = body["member"]["id"].as_str().unwrap().parse().unwrap();
        assert!(members_of(&t, team).contains(&id), "written on the retry");
        assert_eq!(body["member"]["access"]["projects"][0]["platform_role"], "developer", "built from what was written: {body}");
        // An existing member, and the team changes twice.
        let bob = t.w.metadata.add_member(t.w.tenant, "bob", MemberStatus::Active, false);
        t.w.metadata.set_member(t.w.tenant, "bob", |m| m.email = Some("bob@x.com".into()));
        for role in [PlatformRole::Viewer, PlatformRole::Developer] {
            t.w.metadata.race_teams_once(move |md| md.seed_team_grant(team, project, role, &[]));
        }
        let err = error_of(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_with_teams("bob@x.com", json!([team])))).await, 409).await;
        assert_eq!(code_of(&err), "conflict", "{err}");
        assert!(!members_of(&t, team).contains(&bob), "nothing written");
    }

    // P13: the roster's `via` names where each project's level comes from (`own`, `team`, `both`)
    // and the level is the best source's; a tenant admin's projects stay `own`.
    #[tokio::test]
    async fn roster_via_reports_own_team_and_both() {
        let t = mw(Mode::SelfHosted).await;
        let p = t.w.project;
        let own = t.w.metadata.add_granted(t.w.tenant, "own", p, PlatformRole::Developer, &[]);
        let team_only = t.w.metadata.add_member(t.w.tenant, "teamonly", MemberStatus::Active, false);
        let both = t.w.metadata.add_granted(t.w.tenant, "both", p, PlatformRole::Viewer, &[]);
        let team = t.w.metadata.seed_team(t.w.tenant, "Devs");
        t.w.metadata.seed_team_grant(team, p, PlatformRole::Developer, &[]);
        let viewers = t.w.metadata.seed_team(t.w.tenant, "Viewers");
        t.w.metadata.seed_team_grant(viewers, p, PlatformRole::Viewer, &[]);
        t.w.metadata.seed_team_member(viewers, team_only);
        t.w.metadata.seed_team_member(team, both);
        for (member, role, via, level) in [
            (own, "developer", "own", "developer"),
            (team_only, "viewer", "team", "viewer"),
            (both, "developer", "both", "developer"),
        ] {
            let row = roster_row(&t, DEFAULT_SUB, member).await;
            assert_eq!(row["access"]["projects"], json!([{ "project_id": p, "platform_role": role, "via": via }]), "{row}");
            assert_eq!(row["access"]["level"], level, "{row}");
            assert_eq!(row["has_access"], true, "{row}");
        }
        let admin = t.w.metadata.member(t.w.tenant, DEFAULT_SUB).unwrap().id;
        let row = roster_row(&t, DEFAULT_SUB, admin).await;
        assert!(row["access"]["projects"].as_array().unwrap().iter().all(|p| p["via"] == "own"), "{row}");
    }

    // R1 on the roster's team-derived levels: a caller who sees only project A never learns of a
    // team grant on B (MH-35's `other` still applies).
    #[tokio::test]
    async fn roster_level_counts_team_grants_on_visible_projects_only() {
        let t = mw(Mode::SelfHosted).await;
        let a = t.w.project;
        let b = t.w.metadata.create_project(t.w.tenant, "Other").await.unwrap().id;
        t.w.metadata.add_granted(t.w.tenant, "seer", a, PlatformRole::Viewer, &[]);
        let x = t.w.metadata.add_granted(t.w.tenant, "x", a, PlatformRole::Viewer, &[]);
        let y = t.w.metadata.add_member(t.w.tenant, "y", MemberStatus::Active, false);
        let team = t.w.metadata.seed_team(t.w.tenant, "B admins");
        t.w.metadata.seed_team_grant(team, b, PlatformRole::Admin, &[]);
        t.w.metadata.seed_team_member(team, x);
        t.w.metadata.seed_team_member(team, y);
        let row = roster_row(&t, "seer", x).await;
        assert_eq!(row["access"]["level"], "viewer", "{row}");
        assert_eq!(row["access"]["projects"], json!([{ "project_id": a, "platform_role": "viewer", "via": "own" }]), "{row}");
        let row = roster_row(&t, "seer", y).await;
        assert_eq!((row["access"]["level"].as_str(), row["has_access"].as_bool()), (Some("other"), Some(true)), "{row}");
        assert_eq!(row["access"]["projects"], json!([]), "{row}");
        let row = roster_row(&t, DEFAULT_SUB, x).await;
        assert_eq!(row["access"]["level"], "admin", "{row}");
    }

    // P14, R1, MH-34 on `GET /v1/members/{id}/teams`: every team of the member, each with its
    // grants on the caller's visible projects only, `pg_roles` null below the project's Admins.
    #[tokio::test]
    async fn member_teams_are_filtered_by_visibility_and_pg_roles_rule() {
        let t = mw(Mode::SelfHosted).await;
        let a = t.w.project;
        let b = t.w.metadata.create_project(t.w.tenant, "Other").await.unwrap().id;
        let dev = t.w.metadata.add_granted(t.w.tenant, "dev", a, PlatformRole::Developer, &[]);
        let m = t.w.metadata.add_member(t.w.tenant, "m", MemberStatus::Active, false);
        let alpha = t.w.metadata.seed_team(t.w.tenant, "alpha");
        t.w.metadata.seed_team_grant(alpha, a, PlatformRole::Developer, &["reports"]);
        let beta = t.w.metadata.seed_team(t.w.tenant, "Beta");
        t.w.metadata.seed_team_grant(beta, b, PlatformRole::Admin, &[]);
        for team in [alpha, beta] {
            t.w.metadata.seed_team_member(team, m);
        }
        let path = format!("/v1/members/{m}/teams");
        let list = body_of(t.send("dev", Method::GET, &path, None).await).await;
        assert_eq!(list.as_array().map(Vec::len), Some(2), "{list}");
        assert_eq!((list[0]["name"].as_str(), list[1]["name"].as_str()), (Some("alpha"), Some("Beta")), "by lower(name): {list}");
        assert_eq!(list[0]["projects"], json!([{ "project_id": a, "platform_role": "developer", "pg_roles": null }]), "{list}");
        assert_eq!(list[1]["projects"], json!([]), "a project dev can't see is never named: {list}");
        assert_eq!(list[0]["member_ids"], json!([m]), "{list}");
        let list = body_of(t.send(DEFAULT_SUB, Method::GET, &path, None).await).await;
        assert_eq!(list[0]["projects"][0]["pg_roles"], json!(["reports"]), "{list}");
        assert_eq!(list[1]["projects"], json!([{ "project_id": b, "platform_role": "admin", "pg_roles": [] }]), "{list}");
        // A member with no teams: an empty list.
        let list = body_of(t.send("dev", Method::GET, &format!("/v1/members/{dev}/teams"), None).await).await;
        assert_eq!(list, json!([]));
    }

    // P15, P30: the project's team grants are listed for its Admins (by lower(name)); the grants
    // list stays the own grants' array.
    #[tokio::test]
    async fn project_team_grants_are_listed_for_project_admins() {
        let t = mw(Mode::SelfHosted).await;
        let p = t.w.project;
        let padmin = t.w.metadata.add_granted(t.w.tenant, "padmin", p, PlatformRole::Admin, &[]);
        t.w.metadata.add_granted(t.w.tenant, "dev", p, PlatformRole::Developer, &[]);
        let m = t.w.metadata.add_member(t.w.tenant, "m", MemberStatus::Active, false);
        let zeta = t.w.metadata.seed_team(t.w.tenant, "Zeta");
        t.w.metadata.seed_team_grant(zeta, p, PlatformRole::Viewer, &["reports"]);
        t.w.metadata.seed_team_member(zeta, m);
        let alpha = t.w.metadata.seed_team(t.w.tenant, "alpha");
        t.w.metadata.seed_team_grant(alpha, p, PlatformRole::Developer, &[]);
        let elsewhere = t.w.metadata.seed_team(t.w.tenant, "Elsewhere");
        t.w.metadata.seed_team_member(elsewhere, m);
        let path = format!("/v1/projects/{p}/team-grants");
        let resp = t.send("padmin", Method::GET, &path, None).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(
            body_of(resp).await,
            json!([
                { "team_id": alpha, "name": "alpha", "platform_role": "developer", "pg_roles": [], "member_ids": [] },
                { "team_id": zeta, "name": "Zeta", "platform_role": "viewer", "pg_roles": ["reports"], "member_ids": [m] },
            ])
        );
        let err = error_of(t.send("dev", Method::GET, &path, None).await, 403).await;
        assert_eq!(err["error"]["required"], "admin", "{err}");
        // The grants list is unchanged: the own grants only.
        let grants = body_of(t.send("padmin", Method::GET, &format!("/v1/projects/{p}/grants"), None).await).await;
        let ids: Vec<&str> = grants.as_array().unwrap().iter().map(|g| g["member_id"].as_str().unwrap()).collect();
        assert!(!ids.contains(&m.to_string().as_str()), "a team member isn't a grant: {grants}");
        assert!(ids.contains(&padmin.to_string().as_str()), "{grants}");
    }

    // P16: reinstating a member reconciles its teams' projects too.
    #[tokio::test]
    async fn reinstate_reconciles_team_projects() {
        let t = mw(Mode::SelfHosted).await;
        let s = t.w.metadata.add_member(t.w.tenant, "s", MemberStatus::Active, false);
        let team = t.w.metadata.seed_team(t.w.tenant, "Team");
        t.w.metadata.seed_team_grant(team, t.w.project, PlatformRole::Developer, &[]);
        t.w.metadata.seed_team_member(team, s);
        t.w.metadata.set_member(t.w.tenant, "s", |m| m.status = MemberStatus::Suspended);
        t.clear_enqueued();
        let resp = t.send(DEFAULT_SUB, Method::POST, &format!("/v1/members/{s}/reinstate"), None).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(t.enqueued(), t.branches(), "the team's project reconciles");
    }

    // P10, MH-5: removing a member drops its memberships (`member_removed` lists the teams) and
    // reconciles its teams' projects.
    #[tokio::test]
    async fn remove_member_reconciles_team_projects() {
        let t = mw(Mode::SelfHosted).await;
        let zid = t.identity.with_user(&Pool::Members, "r@x.com", "Secret-pass1");
        let r = t.w.metadata.add_member(t.w.tenant, &zid, MemberStatus::Active, false);
        let team = api_team(&t.w.app, "Team", t.w.project, "developer", &[], &[r]).await;
        t.clear_enqueued();
        let resp = t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/members/{r}"), None).await;
        assert_eq!(resp.status(), 204);
        assert_eq!(t.enqueued(), t.branches(), "the team's project reconciles");
        assert!(members_of(&t, team).is_empty(), "the membership is gone");
        let removed = t.w.metadata.events.lock().unwrap().iter().rev().find(|e| e.action == "member_removed").cloned().unwrap();
        assert_eq!(removed.before.unwrap()["teams"], json!([{ "id": team, "name": "Team" }]));
    }

    // MH-7: a member's Activity (`member_id`) shows the team events that reached them, even when
    // they are neither the actor nor the target.
    #[tokio::test]
    async fn member_activity_finds_team_events() {
        let t = mw(Mode::SelfHosted).await;
        let m = t.w.metadata.add_member(t.w.tenant, "m", MemberStatus::Active, false);
        let other = t.w.metadata.add_member(t.w.tenant, "other", MemberStatus::Active, false);
        let team = api_team(&t.w.app, "Team", t.w.project, "viewer", &[], &[m]).await;
        let resp = t
            .send(DEFAULT_SUB, Method::PUT, &format!("/v1/teams/{team}/grants/{}", t.w.project), Some(json!({ "platform_role": "developer", "pg_roles": [] })))
            .await;
        assert_eq!(resp.status(), 200);
        let page = body_of(t.send(DEFAULT_SUB, Method::GET, &format!("/v1/access-events?member_id={m}"), None).await).await;
        let actions: Vec<&str> = page["events"].as_array().unwrap().iter().map(|e| e["action"].as_str().unwrap()).collect();
        assert_eq!(actions, vec!["team_grant_put", "team_member_added"], "{page}");
        let page = body_of(t.send(DEFAULT_SUB, Method::GET, &format!("/v1/access-events?member_id={other}"), None).await).await;
        assert_eq!(page["events"], json!([]), "{page}");
    }

    // --- MH-2: one route per caller of the grant sources, with a team-only member -------------

    /// A team-only member `tom` (no own grant) in two teams: `Editors` gives Developer
    /// `{billing_editor}`, `Readers` Viewer `{billing_reader}`; both roles are NOLOGIN groups
    /// on `dev`.
    async fn team_only() -> (MemberWorld, uuid::Uuid) {
        let w = member_world(Mode::SelfHosted).await;
        let ep = w.metadata.branch(w.dev).endpoint_id.unwrap();
        w.metadata.add_group_role(&ep, "billing_editor", &[]);
        w.metadata.add_group_role(&ep, "billing_reader", &[]);
        let tom = w.metadata.add_member(w.tenant, "tom", MemberStatus::Active, false);
        api_team(&w.app, "Editors", w.project, "developer", &["billing_editor"], &[tom]).await;
        api_team(&w.app, "Readers", w.project, "viewer", &["billing_reader"], &[tom]).await;
        (w, tom)
    }

    /// An access refusal: `forbidden`, `no_access_yet`, `pending_approval`, `member_suspended`,
    /// or the project route's "not found" (R1).
    fn refused(status: u16, body: &Value) -> bool {
        matches!(body["error"]["code"].as_str(), Some("forbidden" | "no_access_yet" | "pending_approval" | "member_suspended"))
            || (status == 404 && body["error"]["message"] == "not found")
    }

    async fn as_tom(w: &MemberWorld, method: Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let resp = w.app.send_as("tom", "org-1", method, path, body).await;
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    fn branch_path(w: &MemberWorld) -> String {
        format!("/v1/databases/{}/branches/{}", w.db, w.dev.as_uuid())
    }

    #[tokio::test]
    async fn team_only_member_is_admitted() {
        let (w, _) = team_only().await;
        let (status, body) = as_tom(&w, Method::GET, &format!("/v1/databases/{}", w.db), None).await;
        assert_eq!(status, 200, "{body}");
    }

    #[tokio::test]
    async fn team_only_member_lists_the_project() {
        let (w, _) = team_only().await;
        let (status, list) = as_tom(&w, Method::GET, "/v1/projects", None).await;
        assert_eq!(status, 200, "{list}");
        let ids: Vec<&str> = list.as_array().unwrap().iter().map(|p| p["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec![w.project.to_string().as_str()], "{list}");
    }

    #[tokio::test]
    async fn team_only_member_me_lists_the_project() {
        let (w, _) = team_only().await;
        let (status, me) = as_tom(&w, Method::GET, "/v1/me", None).await;
        assert_eq!(status, 200, "{me}");
        assert_eq!(me["projects"].as_array().map(Vec::len), Some(1), "{me}");
        assert_eq!(me["projects"][0]["project_id"], json!(w.project), "{me}");
        assert_eq!(me["projects"][0]["platform_role"], "developer", "{me}");
        assert_eq!(me["projects"][0]["roles"], json!(["billing_editor", "billing_reader"]), "{me}");
    }

    #[tokio::test]
    async fn team_only_member_acts_as_a_team_role_in_the_table_api() {
        let (w, _) = team_only().await;
        let table = format!("{}/tables/public/notes", branch_path(&w));
        let (status, body) = as_tom(&w, Method::GET, &table, None).await;
        assert!(!refused(status, &body), "read: {status} {body}");
        let row = format!("{table}/rows/default");
        let (status, body) = as_tom(&w, Method::POST, &row, Some(json!({ "role": "billing_editor" }))).await;
        assert!(!refused(status, &body), "act as a Developer team role: {status} {body}");
        // A Viewer team role opens no Developer route (D1).
        let (status, err) = as_tom(&w, Method::POST, &row, Some(json!({ "role": "billing_reader" }))).await;
        assert_eq!((status, err["error"]["required"].as_str()), (403, Some("role:billing_reader")), "{err}");
    }

    #[tokio::test]
    async fn team_only_member_console_as_a_team_role() {
        let (w, _) = team_only().await;
        let console = format!("{}/console/query", branch_path(&w));
        for role in ["billing_reader", "billing_editor"] {
            let body = json!({ "role": role, "tree": { "v": 1, "op": "select", "from": "notes" } });
            let (status, answer) = as_tom(&w, Method::POST, &console, Some(body)).await;
            assert!(!refused(status, &answer), "{role}: {status} {answer}");
        }
    }

    #[tokio::test]
    async fn team_only_member_runs_sql_as_a_developer_team_role() {
        let (w, _) = team_only().await;
        let sql = format!("{}/sql", branch_path(&w));
        let run = |role: &str| json!({ "run_id": uuid::Uuid::new_v4(), "role": role, "sql": "select 1" });
        let (status, body) = as_tom(&w, Method::POST, &sql, Some(run("billing_editor"))).await;
        assert!(!refused(status, &body), "{status} {body}");
        let (status, err) = as_tom(&w, Method::POST, &sql, Some(run("billing_reader"))).await;
        assert_eq!((status, err["error"]["required"].as_str()), (403, Some("role:billing_reader")), "{err}");
    }

    #[tokio::test]
    async fn team_only_member_merges() {
        let (w, _) = team_only().await;
        let merges = format!("{}/merges", branch_path(&w));
        let (status, body) = as_tom(&w, Method::POST, &merges, Some(json!({ "direction": "into_parent", "database": "app" }))).await;
        assert!(!refused(status, &body), "{status} {body}");
    }

    #[tokio::test]
    async fn team_only_member_key_role_needs_developer() {
        let (w, _) = team_only().await;
        let keys = format!("{}/keys", branch_path(&w));
        let (status, body) = as_tom(&w, Method::POST, &keys, Some(json!({ "name": "editor-key", "role": "billing_editor" }))).await;
        assert_eq!(status, 201, "{body}");
        let (status, err) = as_tom(&w, Method::POST, &keys, Some(json!({ "name": "reader-key", "role": "billing_reader" }))).await;
        assert_eq!((status, err["error"]["required"].as_str()), (403, Some("role:billing_reader")), "{err}");
    }

    // Admin through a team reveals and resets the owner password (Project(Admin) routes).
    #[tokio::test]
    async fn team_admin_reveals_the_owner_password() {
        let w = member_world(Mode::SelfHosted).await;
        let ann = w.metadata.add_member(w.tenant, "ann", MemberStatus::Active, false);
        api_team(&w.app, "Admins", w.project, "admin", &[], &[ann]).await;
        let resp = w.app.send_as("ann", "org-1", Method::GET, &format!("/v1/databases/{}/connection", w.db), None).await;
        assert_eq!(resp.status(), 200);
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["password"], "s3cret", "{body}");
        let resp = w.app.send_as("ann", "org-1", Method::POST, &format!("/v1/databases/{}/reset-password", w.db), None).await;
        let status = resp.status().as_u16();
        let body: Value = resp.json().await.unwrap_or(Value::Null);
        assert!(!refused(status, &body), "{status} {body}");
    }

    // The roster counts a team-only member's access (`has_access`, `via: team`).
    #[tokio::test]
    async fn team_only_member_has_access_in_the_roster() {
        let (w, tom) = team_only().await;
        let list: Value = w.app.send_as(DEFAULT_SUB, "org-1", Method::GET, "/v1/members", None).await.json().await.unwrap();
        let row = list.as_array().unwrap().iter().find(|m| m["id"] == json!(tom)).cloned().unwrap();
        assert_eq!(row["has_access"], true, "{row}");
        assert_eq!(row["access"]["projects"], json!([{ "project_id": w.project, "platform_role": "developer", "via": "team" }]), "{row}");
    }

    // A project Admin through a team manages the project's grants, and grant validation runs.
    #[tokio::test]
    async fn team_only_project_admin_writes_checked_grants() {
        let w = member_world(Mode::SelfHosted).await;
        let ann = w.metadata.add_member(w.tenant, "ann", MemberStatus::Active, false);
        api_team(&w.app, "Admins", w.project, "admin", &[], &[ann]).await;
        let other = w.metadata.add_member(w.tenant, "other", MemberStatus::Active, false);
        let grant = format!("/v1/projects/{}/grants/{other}", w.project);
        let resp = w.app.send_as("ann", "org-1", Method::PUT, &grant, Some(json!({ "platform_role": "viewer", "pg_roles": [] }))).await;
        assert_eq!(resp.status(), 200);
        let resp = w.app.send_as("ann", "org-1", Method::PUT, &grant, Some(json!({ "platform_role": "viewer", "pg_roles": ["owner"] }))).await;
        assert_eq!(resp.status(), 422);
    }
}

// --- Epic review (#77) -------------------------------------------------------------------------

/// Revoking an invite of a user who is a member of another tenant leaves the account alone.
#[tokio::test]
async fn revoking_an_invite_leaves_a_user_who_is_a_member_elsewhere() {
    let t = mw(Mode::SelfHosted).await;
    let other_tenant = TenantId::new();
    let zr = t.identity.with_user(&Pool::Members, "ray@x.com", "Secret-pass1");
    let ray = t.w.metadata.add_member(t.w.tenant, &zr, MemberStatus::Active, false);
    t.w.metadata.set_member(t.w.tenant, &zr, |m| m.email = Some("ray@x.com".into()));
    t.w.metadata.add_member(other_tenant, &zr, MemberStatus::Active, false);
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/members/{ray}"), None).await.status(), 204);
    let body = body_of(
        t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "ray@x.com", "viewer", &[]))).await,
    )
    .await;
    let invite = body["member"]["invite"]["id"].as_str().unwrap().to_string();
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/invites/{invite}"), None).await.status(), 204);
    let user = t.identity.user(&Pool::Members, &zr).expect("revoke: still a member elsewhere");
    assert!(!user.banned, "{user:?}");
}

// --- #119: member tokens (the gateway's identity names a tenant, `tid`) ------------------------

/// The gateway's identity for a member token of person `sub` naming `tenant`.
fn member_identity_token(app: &TestApp, sub: &str, tenant: uuid::Uuid) -> String {
    app.signer
        .sign_for_tenant(Service::Orchestrator.name(), &identity_of(sub), tenant)
        .unwrap()
}

#[allow(clippy::disallowed_methods, clippy::disallowed_types)] // test code: a plain client against the test app
async fn send_member(app: &TestApp, sub: &str, tenant: uuid::Uuid, method: Method, path: &str) -> reqwest::Response {
    reqwest::Client::new()
        .request(method, format!("{}{path}", app.base))
        .header(IDENTITY_HEADER, member_identity_token(app, sub, tenant))
        .send()
        .await
        .unwrap()
}

fn set_status(metadata: &FakeMetadata, tenant: TenantId, sub: &str, status: MemberStatus) {
    let mut members = metadata.members.lock().unwrap();
    let m = members.iter_mut().find(|m| m.tenant_id == tenant && m.identity_user_id == sub).unwrap();
    m.status = status;
}

// AC 1 and AC 4: an active member reaches the route as that member of `tid`; once suspended in
// metadata, the same (still valid) token is refused with 403, and once removed with today's
// refusal for a removed member.
#[tokio::test]
async fn member_token_removed_member_403() {
    let w = member_world(Mode::SelfHosted).await;
    w.metadata.add_granted(w.tenant, "rita", w.project, PlatformRole::Viewer, &[]);
    let path = format!("/v1/databases/{}", w.db);
    let tid = w.tenant.as_uuid();
    assert_eq!(send_member(&w.app, "rita", tid, Method::GET, &path).await.status(), 200);
    let me: Value = send_member(&w.app, "rita", tid, Method::GET, "/v1/me")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(me["member"]["status"], "active", "{me}");
    assert_eq!(me["tenant_id"], json!(tid), "{me}");

    set_status(&w.metadata, w.tenant, "rita", MemberStatus::Suspended);
    let err = error_of(send_member(&w.app, "rita", tid, Method::GET, &path).await, 403).await;
    assert_eq!(code_of(&err), "member_suspended");

    // A removed member is refused as today (`access::decide`: a removed member is told
    // nothing exists, 404 on every tenant route).
    set_status(&w.metadata, w.tenant, "rita", MemberStatus::Removed);
    let err = error_of(send_member(&w.app, "rita", tid, Method::GET, &path).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    let err = error_of(send_member(&w.app, "rita", tid, Method::GET, "/v1/members").await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    assert_eq!(w.metadata.member(w.tenant, "rita").unwrap().status, MemberStatus::Removed, "never reactivated");
}

// AC 5: `tid` names a tenant the subject isn't a member of: 403 and no member row, even with
// open sign-up on and a recent admin claim (neither makes a member row from a token).
#[tokio::test]
async fn member_token_wrong_tenant_403_no_jit() {
    let w = member_world(Mode::SelfHosted).await;
    w.metadata.open_signups.lock().unwrap().insert(w.tenant, true);
    w.metadata.set_admin_claim(w.tenant, Some(chrono::Utc::now() - chrono::Duration::minutes(5)));
    // A member of another tenant names this one.
    let other = TenantId::new();
    w.metadata.bind_org("org-2", other);
    w.metadata.add_member(other, "mallory", MemberStatus::Active, true);
    w.metadata.set_overview(TenantOverview {
        id: w.tenant,
        slug: "victim-corp".into(),
        display_name: Some("Victim Corp".into()),
        projects: vec![],
    });
    let before = (w.metadata.members.lock().unwrap().len(), w.metadata.events.lock().unwrap().len());
    let path = format!("/v1/databases/{}", w.db);
    for sub in ["mallory", "newcomer"] {
        let err = error_of(send_member(&w.app, sub, w.tenant.as_uuid(), Method::GET, &path).await, 403).await;
        assert_eq!(code_of(&err), "no_access_yet", "{sub}");
        let err = error_of(send_member(&w.app, sub, w.tenant.as_uuid(), Method::GET, "/v1/members").await, 403).await;
        assert_eq!(code_of(&err), "no_access_yet", "{sub}");
        // `/v1/me` too: `tid` alone never reveals the tenant.
        let err = error_of(send_member(&w.app, sub, w.tenant.as_uuid(), Method::GET, "/v1/me").await, 403).await;
        assert_eq!(code_of(&err), "no_access_yet", "{sub}");
        assert!(!err.to_string().contains("victim-corp"), "{sub}: {err}");
        assert!(w.metadata.member(w.tenant, sub).is_none(), "{sub}: no member row");
    }
    assert_eq!((w.metadata.members.lock().unwrap().len(), w.metadata.events.lock().unwrap().len()), before);
    // mallory's own tenant still works for her.
    let me: Value = send_member(&w.app, "mallory", other.as_uuid(), Method::GET, "/v1/me").await.json().await.unwrap();
    assert_eq!(me["member"]["status"], "active", "{me}");
}

// A tenant that no longer exists: 401, and nothing is written.
#[tokio::test]
async fn member_token_unknown_tenant_401() {
    let w = member_world(Mode::SelfHosted).await;
    let before = w.metadata.members.lock().unwrap().len();
    let gone = uuid::Uuid::new_v4();
    for path in ["/v1/me".to_string(), format!("/v1/databases/{}", w.db), "/v1/members".into()] {
        let err = error_of(send_member(&w.app, DEFAULT_SUB, gone, Method::GET, &path).await, 401).await;
        assert_eq!(code_of(&err), "unauthorized", "{path}");
    }
    assert_eq!(w.metadata.members.lock().unwrap().len(), before);
}

// `CHELABASE_OPERATOR_TENANT_ID`: in platform mode a member token naming the operator tenant
// passes the instance check; another tenant doesn't.
#[tokio::test]
async fn operator_tenant_is_instance_admin_for_member_tokens() {
    let metadata = FakeMetadata::default();
    let ops = TenantId::new();
    metadata.bind_org("ops-org", ops);
    metadata.add_member(ops, "op", MemberStatus::Active, false);
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let app = spawn_app_operator_tenant(metadata.clone(), Mode::Platform, Some(ops)).await;
    let resp = send_member(&app, "op", ops.as_uuid(), Method::GET, INSTANCE_PROBE).await;
    assert_eq!(resp.status(), 200);
    let err = error_of(send_member(&app, DEFAULT_SUB, tenant.as_uuid(), Method::GET, INSTANCE_PROBE).await, 403).await;
    assert_eq!(err["error"]["required"], "instance admin");
}

// #119 on the realtime routes: a member token is verified here with the member key; the member
// is read (never made) in the tenant `tid` names; `/internal/realtime/member` takes the
// gateway identity's `tenant_id`.
#[tokio::test]
#[allow(clippy::disallowed_methods, clippy::disallowed_types)] // test code: a plain client against the test app
async fn realtime_routes_take_member_tokens() {
    use chelabase_auth::member_token::{MemberClaims, MemberTokenSigner, MemberTokenVerifier, MEMBER_AUDIENCE};
    const ISSUER: &str = "http://auth.test";
    let (private, public) = chelabase_auth::internal::generate_keypair_pem();
    let signer = MemberTokenSigner::from_pem(private.as_bytes(), public.as_bytes(), ISSUER).unwrap();
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let project = metadata.create_project(tenant, "Billing").await.unwrap().id;
    metadata.add_granted(tenant, "rita", project, PlatformRole::Viewer, &[]);
    metadata.add_granted(tenant, "sus", project, PlatformRole::Viewer, &[]);
    metadata.set_member(tenant, "sus", |m| m.status = MemberStatus::Suspended);
    let other = TenantId::new();
    metadata.bind_org("org-2", other);
    let app = spawn_app_member_tokens(
        metadata.clone(),
        Mode::SelfHosted,
        MemberTokenVerifier::from_pem(public.as_bytes(), ISSUER).unwrap(),
    )
    .await;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let token = |sub: &str, tid: uuid::Uuid| {
        signer
            .sign(&MemberClaims {
                iss: ISSUER.into(),
                aud: MEMBER_AUDIENCE.into(),
                sub: sub.into(),
                tid,
                sid: "s".into(),
                email: format!("{sub}@example.test"),
                name: sub.into(),
                iat: now,
                exp: now + 300,
            })
            .unwrap()
    };
    let post = |path: &'static str, body: Value| {
        let base = app.base.clone();
        async move {
            let resp = reqwest::Client::new()
                .post(format!("{base}{path}"))
                .bearer_auth(REALTIME_TOKEN)
                .json(&body)
                .send()
                .await
                .unwrap();
            let status = resp.status().as_u16();
            (status, resp.json::<Value>().await.unwrap_or(Value::Null))
        }
    };
    const PROJECTS: &str = "/internal/realtime/projects";
    const MEMBER: &str = "/internal/realtime/member";

    let (status, body) = post(PROJECTS, json!({ "token": token("rita", tenant.as_uuid()) })).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["tenant_id"], json!(tenant.as_uuid()), "{body}");
    // Another tenant (rita isn't a member there), an unknown tenant, a suspended member, and a
    // token from another key are refused; nothing is created.
    let members_before = metadata.members.lock().unwrap().len();
    let (o_private, o_public) = chelabase_auth::internal::generate_keypair_pem();
    let foreign = MemberTokenSigner::from_pem(o_private.as_bytes(), o_public.as_bytes(), ISSUER).unwrap();
    let forged = foreign
        .sign(&MemberClaims {
            iss: ISSUER.into(),
            aud: MEMBER_AUDIENCE.into(),
            sub: "rita".into(),
            tid: tenant.as_uuid(),
            sid: "s".into(),
            email: "r@x".into(),
            name: "r".into(),
            iat: now,
            exp: now + 300,
        })
        .unwrap();
    for (what, t) in [
        ("other tenant", token("rita", other.as_uuid())),
        ("unknown tenant", token("rita", uuid::Uuid::new_v4())),
        ("suspended", token("sus", tenant.as_uuid())),
        ("foreign key", forged),
    ] {
        let (status, body) = post(PROJECTS, json!({ "token": t })).await;
        assert_ne!(status, 200, "{what}: {body}");
    }
    assert_eq!(metadata.members.lock().unwrap().len(), members_before);

    let ask = |body: Value| post(MEMBER, body);
    let (status, body) = ask(json!({ "sub": "rita", "tenant_id": tenant.as_uuid() })).await;
    assert_eq!((status, body["allowed"].clone()), (200, json!(true)), "{body}");
    for (what, b) in [
        ("suspended", json!({ "sub": "sus", "tenant_id": tenant.as_uuid() })),
        ("other tenant", json!({ "sub": "rita", "tenant_id": other.as_uuid() })),
        ("unknown tenant", json!({ "sub": "rita", "tenant_id": uuid::Uuid::new_v4() })),
        ("nil tenant", json!({ "sub": "rita", "tenant_id": uuid::Uuid::nil() })),
    ] {
        let (status, body) = ask(b).await;
        assert_eq!((status, body["allowed"].clone()), (200, json!(false)), "{what}: {body}");
    }
    // #126: the tenant is required, and nothing else is taken (an org is an unknown field).
    for b in [json!({ "sub": "rita" }), json!({ "sub": "rita", "tenant_id": tenant.as_uuid(), "org_id": "org-1" })] {
        assert_eq!(ask(b).await.0, 400);
    }
}

// `tid` is never trusted alone: naming the operator tenant makes no one an operator without an
// active membership there, on the instance check and on `/v1/me` alike.
#[tokio::test]
async fn operator_tenant_needs_an_active_membership() {
    let metadata = FakeMetadata::default();
    let ops = TenantId::new();
    metadata.bind_org("ops-org", ops);
    metadata.add_member(ops, "op", MemberStatus::Active, false);
    metadata.add_member(ops, "sus", MemberStatus::Suspended, false);
    metadata.add_member(ops, "off", MemberStatus::Active, false);
    metadata.set_member(ops, "off", |m| m.offboarded = true);
    let app = spawn_app_operator_tenant(metadata.clone(), Mode::Platform, Some(ops)).await;
    let before = metadata.members.lock().unwrap().len();
    assert_eq!(send_member(&app, "op", ops.as_uuid(), Method::GET, INSTANCE_PROBE).await.status(), 200);
    for sub in ["stranger", "sus", "off"] {
        let resp = send_member(&app, sub, ops.as_uuid(), Method::GET, INSTANCE_PROBE).await;
        assert_eq!(resp.status(), 403, "{sub}");
    }
    // A member not in good standing is still told where they stand; a stranger is told nothing.
    for sub in ["sus", "off"] {
        let me: Value = send_member(&app, sub, ops.as_uuid(), Method::GET, "/v1/me").await.json().await.unwrap();
        assert_eq!(me["instance_admin"], json!(false), "{sub}: {me}");
    }
    let err = error_of(send_member(&app, "stranger", ops.as_uuid(), Method::GET, "/v1/me").await, 403).await;
    assert_eq!(code_of(&err), "not_available_in_platform_mode");
    assert_eq!(metadata.members.lock().unwrap().len(), before, "nothing is admitted");
}

// The realtime routes read the same rule: their principal for a member token naming the operator
// tenant is an operator only with an active membership there, and is never made one by `tid`.
#[tokio::test]
async fn realtime_principal_operator_needs_an_active_membership() {
    use chelabase_auth::member_token::{MemberClaims, MEMBER_AUDIENCE};
    let metadata = FakeMetadata::default();
    let ops = TenantId::new();
    metadata.bind_org("ops-org", ops);
    metadata.add_member(ops, "op", MemberStatus::Active, false);
    metadata.add_member(ops, "sus", MemberStatus::Suspended, false);
    metadata.add_member(ops, "off", MemberStatus::Active, false);
    metadata.set_member(ops, "off", |m| m.offboarded = true);
    let other = TenantId::new();
    metadata.bind_org("org-1", other);
    metadata.add_member(other, "op", MemberStatus::Active, false);
    let app = spawn_app_operator_tenant(metadata.clone(), Mode::Platform, Some(ops)).await;
    let before = metadata.members.lock().unwrap().len();
    let principal = |sub: &'static str, tenant: TenantId| {
        let app = &app;
        async move {
            let claims = MemberClaims {
                iss: "http://auth.test".into(),
                aud: MEMBER_AUDIENCE.into(),
                sub: sub.into(),
                tid: tenant.as_uuid(),
                sid: "s".into(),
                email: String::new(),
                name: String::new(),
                iat: 0,
                exp: 0,
            };
            orchestrator::read_only_member_principal(&app.state, &claims).await.unwrap().unwrap()
        }
    };
    assert!(principal("op", ops).await.in_operator_org, "an active member of the operator tenant");
    assert!(!principal("op", other).await.in_operator_org, "the same person in another tenant");
    for sub in ["stranger", "sus", "off"] {
        assert!(!principal(sub, ops).await.in_operator_org, "{sub}");
    }
    assert!(principal("stranger", ops).await.member.is_none(), "read, never made");
    assert_eq!(metadata.members.lock().unwrap().len(), before);
}

// --- Members through the identity service (#122) -------------------------------------------------

const CODES_ROUTE: &str = "POST /internal/v1/pools/members/codes";

/// The members-pool user with this email.
fn sub_of(t: &Mw, email: &str) -> String {
    t.identity.user_by_email(&Pool::Members, email).expect("an identity user")
}

/// The body of a 201.
async fn created(resp: reqwest::Response) -> Value {
    let status = resp.status();
    let body = body_of(resp).await;
    assert_eq!(status, 201, "{body}");
    body
}

/// A second tenant holding the same person, for the cases about one global account.
fn second_tenant(t: &Mw, sub: &str, status: MemberStatus) -> (TenantId, uuid::Uuid) {
    let other = TenantId::new();
    let member = t.w.metadata.add_member(other, sub, status, false);
    (other, member)
}

/// An active Developer of Billing behind a verified identity user with a password.
fn identity_member(t: &Mw, email: &str) -> (uuid::Uuid, String) {
    let sub = t.identity.with_user(&Pool::Members, email, "Secret-pass1");
    let member = t.w.metadata.add_granted(t.w.tenant, &sub, t.w.project, PlatformRole::Developer, &["app"]);
    (member, sub)
}

fn invite_link_body(t: &Mw, email: &str) -> Value {
    let mut body = invite_body(&t.w, email, "developer", &["app"]);
    body["delivery"] = json!("link");
    body
}

#[tokio::test]
async fn invite_return_delivery_creates_passwordless_user() {
    let t = mw(Mode::SelfHosted).await;
    let resp = t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_link_body(&t, "new@x.com"))).await;
    assert_eq!(resp.headers().get("cache-control").unwrap(), "no-store");
    let body = created(resp).await;
    assert_eq!(body["outcome"], "invited");
    let sub = t.identity.user_by_email(&Pool::Members, "new@x.com").expect("an identity user");
    let user = t.identity.user(&Pool::Members, &sub).unwrap();
    assert!(!user.has_password && !user.verified, "{user:?}");
    let url = body["invite_url"].as_str().unwrap();
    assert!(url.starts_with(&format!("http://studio.test/accept-invite?user={sub}&code=")), "{url}");
    assert!(url.contains("&t="), "{url}");
    assert!(t.identity.codes_sent().is_empty() && t.identity.notices_sent().is_empty(), "nothing is mailed");
    assert_eq!(t.w.metadata.member(t.w.tenant, &sub).unwrap().status, MemberStatus::Invited);
}

#[tokio::test]
async fn invite_undo_keeps_preexisting_user() {
    let t = mw(Mode::SelfHosted).await;
    // An account that exists already, and a code that can't be made.
    let sub = t.identity.with_invited_user(&Pool::Members, "old@x.com");
    t.identity.fail(CODES_ROUTE, 503, "internal", 1);
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_link_body(&t, "old@x.com"))).await, 503).await;
    assert_eq!(code_of(&err), "identity_unavailable");
    assert!(t.identity.user(&Pool::Members, &sub).is_some(), "not this call's user: it stays");
    assert_eq!(t.w.metadata.member(t.w.tenant, &sub).unwrap().status, MemberStatus::Removed, "the undelivered invite is undone");
    // The same failure on a user this call made takes that user away again.
    t.identity.fail(CODES_ROUTE, 503, "internal", 1);
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_link_body(&t, "fresh@x.com"))).await, 503).await;
    assert_eq!(code_of(&err), "identity_unavailable");
    assert_eq!(t.identity.user_by_email(&Pool::Members, "fresh@x.com"), None);
}

// A user this call made that another tenant's invite reached before the undo isn't deleted.
#[tokio::test]
async fn invite_undo_keeps_a_user_shared_since() {
    let t = mw(Mode::SelfHosted).await;
    let sub = t.identity.with_invited_user(&Pool::Members, "both@x.com");
    second_tenant(&t, &sub, MemberStatus::Invited);
    t.identity.fail(CODES_ROUTE, 503, "internal", 1);
    // By email: a link for someone another tenant holds is refused before anything is sent.
    let body = invite_body(&t.w, "both@x.com", "developer", &["app"]);
    error_of(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(body)).await, 503).await;
    assert!(t.identity.user(&Pool::Members, &sub).is_some());
}

#[tokio::test]
async fn suspend_revokes_inline() {
    let p = pl_world().await;
    let sub = p.t.identity.with_user(&Pool::Members, "sue@x.com", "Secret-pass1");
    let member = (p.t.w.metadata.add_granted(p.t.w.tenant, &sub, p.t.w.project, PlatformRole::Developer, &["app"]), sub.clone());
    let login = p.seed(&member, "sue");
    p.t.identity.with_session(&Pool::Members, &sub);
    let resp = p.member_action(DEFAULT_SUB, member.0, "suspend").await;
    assert_eq!(resp.status(), 200);
    // The personal login, in the same request.
    let stored = p.t.w.metadata.personal_logins.lock().unwrap().iter().find(|l| l.login.role_name == login.login.role_name).cloned().unwrap();
    assert!(stored.verifier.is_none(), "the login's verifier is gone");
    // The realtime event names them.
    let events = p.t.w.app.rt_events.access();
    assert!(events.iter().any(|e| e.tenant_id == p.t.w.tenant.as_uuid() && e.member_sub.as_deref() == Some(sub.as_str())), "{events:?}");
    // Their identity sessions, and every route but /v1/me.
    assert_eq!(p.t.identity.session_count(&Pool::Members), 0);
    let err = error_of(p.send(&sub, Method::GET, "/v1/members", None).await, 403).await;
    assert_eq!(code_of(&err), "member_suspended");
}

#[tokio::test]
async fn suspend_keeps_sessions_while_another_membership_lives() {
    let t = mw(Mode::SelfHosted).await;
    let (member, sub) = identity_member(&t, "two@x.com");
    second_tenant(&t, &sub, MemberStatus::Active);
    t.identity.with_session(&Pool::Members, &sub);
    let path = format!("/v1/members/{member}/suspend");
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &path, None).await.status(), 200);
    assert_eq!(t.identity.session_count(&Pool::Members), 1, "the other tenant's session stays; /token refuses this tenant");
    assert!(!t.identity.user(&Pool::Members, &sub).unwrap().banned);
}

#[tokio::test]
async fn remove_one_of_two_tenants_keeps_account() {
    let t = mw(Mode::SelfHosted).await;
    let (member, sub) = identity_member(&t, "two@x.com");
    second_tenant(&t, &sub, MemberStatus::Active);
    t.identity.with_session(&Pool::Members, &sub);
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/members/{member}"), None).await.status(), 204);
    let user = t.identity.user(&Pool::Members, &sub).unwrap();
    assert!(!user.banned, "{user:?}");
    assert_eq!(t.identity.session_count(&Pool::Members), 1, "they can still mint tokens for the other tenant");
    let stored = t.w.metadata.member(t.w.tenant, &sub).unwrap();
    assert_eq!(stored.status, MemberStatus::Removed);
    assert!(stored.offboarded, "the offboarded mark is written");
}

#[tokio::test]
async fn remove_last_tenant_bans() {
    let t = mw(Mode::SelfHosted).await;
    let (member, sub) = identity_member(&t, "two@x.com");
    let (_, other) = second_tenant(&t, &sub, MemberStatus::Active);
    t.identity.with_session(&Pool::Members, &sub);
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/members/{member}"), None).await.status(), 204);
    t.w.metadata.members.lock().unwrap().iter_mut().find(|m| m.id == other).unwrap().status = MemberStatus::Removed;
    // The retry of the first removal sees the last membership gone.
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/members/{member}"), None).await.status(), 204);
    let user = t.identity.user(&Pool::Members, &sub).unwrap();
    assert!(user.banned, "{user:?}");
    assert_eq!(user.ban_reason.as_deref(), Some(MEMBER_REMOVED_BAN_REASON));
    assert_eq!(t.identity.session_count(&Pool::Members), 0);
}

// A ban somebody else placed keeps its reason, so nothing of ours lifts it later.
#[tokio::test]
async fn remove_keeps_a_foreign_ban_reason() {
    let t = mw(Mode::SelfHosted).await;
    let (member, sub) = identity_member(&t, "abuse@x.com");
    t.identity.client().ban(&Pool::Members, &sub, Some("abuse")).await.unwrap();
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/members/{member}"), None).await.status(), 204);
    assert_eq!(t.identity.user(&Pool::Members, &sub).unwrap().ban_reason.as_deref(), Some("abuse"));
}

// Each identity step of a removal failing: 503, the member is out, every other step ran, and
// the retry finishes.
#[tokio::test]
async fn remove_partial_failure_is_finished_by_a_retry() {
    for step in ["/sessions", "/ban", "GET /internal/v1/pools/members/users/"] {
        let t = mw(Mode::SelfHosted).await;
        let (member, sub) = identity_member(&t, "gone@x.com");
        t.identity.with_session(&Pool::Members, &sub);
        t.identity.fail(step, 503, "internal", 1);
        let path = format!("/v1/members/{member}");
        let err = error_of(t.send(DEFAULT_SUB, Method::DELETE, &path, None).await, 503).await;
        assert_eq!(code_of(&err), "identity_unavailable", "{step}");
        let stored = t.w.metadata.member(t.w.tenant, &sub).unwrap();
        assert_eq!(stored.status, MemberStatus::Removed, "{step}");
        assert!(t.w.app.rt_events.access().iter().any(|e| e.member_sub.as_deref() == Some(sub.as_str())), "{step}");
        assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &path, None).await.status(), 204, "{step}");
        assert!(t.identity.user(&Pool::Members, &sub).unwrap().banned, "{step}");
        assert_eq!(t.identity.session_count(&Pool::Members), 0, "{step}");
    }
}

// A failed login write is a retryable 503 with the member already out, and the retry does what
// it skipped.
#[tokio::test]
async fn remove_login_step_failure_is_finished_by_a_retry() {
    let t = mw(Mode::SelfHosted).await;
    let (member, sub) = identity_member(&t, "gone@x.com");
    t.identity.with_session(&Pool::Members, &sub);
    *t.w.metadata.fail_offboard.lock().unwrap() = true;
    let path = format!("/v1/members/{member}");
    let err = error_of(t.send(DEFAULT_SUB, Method::DELETE, &path, None).await, 503).await;
    assert_eq!(code_of(&err), "unavailable", "{err}");
    assert_eq!(t.w.metadata.member(t.w.tenant, &sub).unwrap().status, MemberStatus::Removed);
    *t.w.metadata.fail_offboard.lock().unwrap() = false;
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &path, None).await.status(), 204);
    assert!(t.w.metadata.member(t.w.tenant, &sub).unwrap().offboarded);
    assert!(t.identity.user(&Pool::Members, &sub).unwrap().banned);
    assert_eq!(t.identity.session_count(&Pool::Members), 0);
}

// Decline ends the same way as a removal, and a retry finishes it.
#[tokio::test]
async fn decline_bans_the_last_membership_and_retries() {
    let t = mw(Mode::SelfHosted).await;
    let sub = t.identity.with_unverified_user(&Pool::Members, "pia@x.com", "Secret-pass1");
    let pia = t.w.metadata.add_member(t.w.tenant, &sub, MemberStatus::Pending, false);
    t.identity.with_session(&Pool::Members, &sub);
    t.identity.fail("/ban", 503, "internal", 1);
    let path = format!("/v1/members/{pia}/decline");
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, &path, None).await, 503).await;
    assert_eq!(code_of(&err), "identity_unavailable");
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &path, None).await.status(), 204);
    assert!(t.identity.user(&Pool::Members, &sub).unwrap().banned);
    assert!(t.w.metadata.member(t.w.tenant, &sub).unwrap().offboarded);
    assert_eq!(t.identity.session_count(&Pool::Members), 0);
    let subs = t.w.app.rt_events.access().into_iter().filter(|e| e.member_sub.as_deref() == Some(sub.as_str())).count();
    assert_eq!(subs, 2, "each call emits, so a lost first event is repaired by the retry");
}

#[tokio::test]
async fn suspend_identity_down_503_member_refused() {
    let t = mw(Mode::SelfHosted).await;
    let (member, sub) = identity_member(&t, "sue@x.com");
    t.identity.with_session(&Pool::Members, &sub);
    t.identity.set_down(true);
    let path = format!("/v1/members/{member}/suspend");
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, &path, None).await, 503).await;
    assert_eq!(code_of(&err), "identity_unavailable");
    // Already locked out by the orchestrator.
    let err = error_of(t.send(&sub, Method::GET, "/v1/members", None).await, 403).await;
    assert_eq!(code_of(&err), "member_suspended");
    t.identity.set_down(false);
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &path, None).await.status(), 200);
    assert_eq!(t.identity.session_count(&Pool::Members), 0, "the retry finished it");
}

// No identity client at all (a token file not readable at start-up): the Chelabase side is
// still done before the 503.
#[tokio::test]
async fn no_identity_client_answers_503_after_the_metadata_and_login_steps() {
    let t = mw_without_identity().await;
    let bob = t.w.metadata.add_granted(t.w.tenant, "bob", t.w.project, PlatformRole::Developer, &["app"]);
    let ann = t.w.metadata.add_granted(t.w.tenant, "ann", t.w.project, PlatformRole::Developer, &["app"]);
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, &format!("/v1/members/{bob}/suspend"), None).await, 503).await;
    assert_eq!(code_of(&err), "identity_unavailable");
    assert_eq!(t.w.metadata.member(t.w.tenant, "bob").unwrap().status, MemberStatus::Suspended);
    assert!(t.w.app.rt_events.access().iter().any(|e| e.member_sub.as_deref() == Some("bob")));

    let err = error_of(t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/members/{ann}"), None).await, 503).await;
    assert_eq!(code_of(&err), "identity_unavailable");
    let stored = t.w.metadata.member(t.w.tenant, "ann").unwrap();
    assert_eq!(stored.status, MemberStatus::Removed);
    assert!(stored.offboarded, "the login step ran");
}

#[tokio::test]
async fn reinstate_restores() {
    let t = mw(Mode::SelfHosted).await;
    let (member, sub) = identity_member(&t, "sue@x.com");
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &format!("/v1/members/{member}/suspend"), None).await.status(), 200);
    // The mark, and a ban that is ours.
    t.w.metadata.members.lock().unwrap().iter_mut().find(|m| m.id == member).unwrap().offboarded = true;
    t.identity.client().ban(&Pool::Members, &sub, Some(MEMBER_REMOVED_BAN_REASON)).await.unwrap();
    let before = t.w.app.rt_events.access().len();
    let path = format!("/v1/members/{member}/reinstate");
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &path, None).await.status(), 200);
    assert!(!t.identity.user(&Pool::Members, &sub).unwrap().banned);
    let stored = t.w.metadata.member(t.w.tenant, &sub).unwrap();
    assert_eq!(stored.status, MemberStatus::Active);
    assert!(!stored.offboarded, "the mark is cleared");
    assert_eq!(t.send(&sub, Method::GET, "/v1/members", None).await.status(), 200);
    // Realtime re-checks once the member is really clear: the last event comes after the clear.
    let events = t.w.app.rt_events.access();
    assert_eq!(events.len(), before + 2, "{events:?}");
}

#[tokio::test]
async fn reinstate_partial_failure_is_finished_by_a_retry() {
    let t = mw(Mode::SelfHosted).await;
    let (member, sub) = identity_member(&t, "sue@x.com");
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &format!("/v1/members/{member}/suspend"), None).await.status(), 200);
    t.w.metadata.members.lock().unwrap().iter_mut().find(|m| m.id == member).unwrap().offboarded = true;
    t.identity.client().ban(&Pool::Members, &sub, Some(MEMBER_REMOVED_BAN_REASON)).await.unwrap();
    t.identity.fail("/unban", 503, "internal", 1);
    let path = format!("/v1/members/{member}/reinstate");
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, &path, None).await, 503).await;
    assert_eq!(code_of(&err), "identity_unavailable");
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &path, None).await.status(), 200);
    assert!(!t.identity.user(&Pool::Members, &sub).unwrap().banned);
    assert!(!t.w.metadata.member(t.w.tenant, &sub).unwrap().offboarded);
}

#[tokio::test]
async fn delivery_reports_email_not_configured() {
    let t = mw(Mode::SelfHosted).await;
    t.identity.set_email_configured(false);
    let body = body_of(t.send(DEFAULT_SUB, Method::GET, "/v1/invites/delivery", None).await).await;
    assert_eq!(body, json!({ "email_available": false, "reason": "email_not_configured" }));
    t.identity.set_email_configured(true);
    let body = body_of(t.send(DEFAULT_SUB, Method::GET, "/v1/invites/delivery", None).await).await;
    assert_eq!(body, json!({ "email_available": true, "reason": null }));
    t.identity.set_down(true);
    let body = body_of(t.send(DEFAULT_SUB, Method::GET, "/v1/invites/delivery", None).await).await;
    assert_eq!(body, json!({ "email_available": false, "reason": "identity_unavailable" }));
}

// The password and the verified flag each make an account "existing": a code handed to an admin
// would let them take it over.
#[tokio::test]
async fn invite_return_refused_for_existing_account() {
    let t = mw(Mode::SelfHosted).await;
    let verified = t.identity.with_user(&Pool::Members, "victim@x.io", "Secret-pass1");
    let unverified = t.identity.with_unverified_user(&Pool::Members, "pre@x.io", "Secret-pass1");
    let confirmed = t.identity.with_invited_user(&Pool::Members, "conf@x.io");
    t.identity.client().mark_email_verified(&Pool::Members, &confirmed).await.unwrap();
    for (email, sub) in [("victim@x.io", &verified), ("pre@x.io", &unverified), ("conf@x.io", &confirmed)] {
        let err = error_of(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_link_body(&t, email))).await, 409).await;
        assert_eq!(code_of(&err), "link_not_allowed", "{email}");
        assert_eq!(err["error"]["email_available"], true, "{email}");
        assert!(t.w.metadata.member(t.w.tenant, sub).is_none(), "{email}: no member row");
    }
    assert!(!t.identity.calls().iter().any(|c| c.contains(CODES_ROUTE)), "no code is created");
}

// By email an established account gets the notice: no code, and a link with only the ticket.
#[tokio::test]
async fn invite_email_to_an_established_account_sends_a_notice() {
    let t = mw(Mode::SelfHosted).await;
    let sub = t.identity.with_user(&Pool::Members, "ann@x.io", "Secret-pass1");
    let body = created(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "ann@x.io", "developer", &["app"]))).await).await;
    assert_eq!(body["outcome"], "invited");
    assert_eq!(body["member"]["status"], "invited", "nobody joins without accepting");
    let notices = t.identity.notices_sent();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].user_id, sub);
    assert!(notices[0].link.starts_with("http://studio.test/accept-invite?t="), "{}", notices[0].link);
    assert!(!notices[0].link.contains("code="), "{}", notices[0].link);
    assert!(t.identity.codes_sent().is_empty(), "never a password-setting code");
}

// A person with no proven mailbox or no password gets the code by email, which only the mailbox
// can read (m11's pre-registered case).
#[tokio::test]
async fn invite_email_to_a_preregistered_account_mails_a_code() {
    let t = mw(Mode::SelfHosted).await;
    t.identity.with_unverified_user(&Pool::Members, "pre@x.io", "Secret-pass1");
    created(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "pre@x.io", "developer", &["app"]))).await).await;
    assert_eq!(t.identity.codes_sent().len(), 1);
    assert!(t.identity.notices_sent().is_empty());
}

// A password set between the read and the code: the service refuses, and the person gets the
// notice instead of a failed invite.
#[tokio::test]
async fn invite_refused_at_the_service_falls_back_to_the_notice() {
    let t = mw(Mode::SelfHosted).await;
    t.identity.with_invited_user(&Pool::Members, "race@x.io");
    t.identity.fail(CODES_ROUTE, 409, "invite_refused", 1);
    created(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "race@x.io", "developer", &["app"]))).await).await;
    assert_eq!(t.identity.notices_sent().len(), 1);
    // With a link it is the same refusal as the read's, and the invite is undone.
    t.identity.with_invited_user(&Pool::Members, "race2@x.io");
    t.identity.fail(CODES_ROUTE, 409, "invite_refused", 1);
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_link_body(&t, "race2@x.io"))).await, 409).await;
    assert_eq!(code_of(&err), "link_not_allowed");
}

#[tokio::test]
async fn resend_never_gives_an_existing_account_a_code() {
    let t = mw(Mode::SelfHosted).await;
    let sub = t.identity.with_user(&Pool::Members, "ann@x.io", "Secret-pass1");
    let body = created(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "ann@x.io", "developer", &["app"]))).await).await;
    let invite = body["member"]["invite"]["id"].as_str().unwrap().to_string();
    let path = format!("/v1/invites/{invite}/resend");
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, &path, Some(json!({ "delivery": "link" }))).await, 409).await;
    assert_eq!(code_of(&err), "link_not_allowed");
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &path, None).await.status(), 200);
    assert_eq!(t.identity.notices_sent().iter().filter(|n| n.user_id == sub).count(), 2);
    assert!(t.identity.codes_sent().is_empty());
}

// Resend's own limit: five sends an hour per person, the first invite included. The 429 mails
// nothing and moves nothing.
#[tokio::test]
async fn invite_code_issuance_is_limited_per_user() {
    let t = mw(Mode::SelfHosted).await;
    let body = created(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "many@x.io", "developer", &["app"]))).await).await;
    let invite = body["member"]["invite"]["id"].as_str().unwrap().to_string();
    let path = format!("/v1/invites/{invite}/resend");
    for n in 2..=5 {
        assert_eq!(t.send(DEFAULT_SUB, Method::POST, &path, None).await.status(), 200, "send {n}");
    }
    let sent = t.identity.codes_sent().len();
    assert_eq!(sent, 5);
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, &path, None).await, 429).await;
    assert_eq!(code_of(&err), "rate_limited");
    assert_eq!(t.identity.codes_sent().len(), sent);
    // Link delivery counts the same, and so does another person's budget not.
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, &path, Some(json!({ "delivery": "link" }))).await, 429).await;
    assert_eq!(code_of(&err), "rate_limited");
    created(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "other@x.io", "developer", &["app"]))).await).await;
}

// A second tenant's invites spend the same budget: the code guesses are per user.
#[tokio::test]
async fn invite_limit_counts_a_first_invite_to_an_existing_user() {
    let t = mw(Mode::SelfHosted).await;
    let sub = t.identity.with_invited_user(&Pool::Members, "cap@x.io");
    let body = created(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "cap@x.io", "developer", &["app"]))).await).await;
    let invite = body["member"]["invite"]["id"].as_str().unwrap().to_string();
    for _ in 2..=5 {
        assert_eq!(t.send(DEFAULT_SUB, Method::POST, &format!("/v1/invites/{invite}/resend"), None).await.status(), 200);
    }
    // Held by another tenant, so the revoke keeps the user; invited again it is the sixth send.
    second_tenant(&t, &sub, MemberStatus::Invited);
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/invites/{invite}"), None).await.status(), 204);
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "cap@x.io", "developer", &["app"]))).await, 429).await;
    assert_eq!(code_of(&err), "rate_limited");
    assert_eq!(t.w.metadata.member(t.w.tenant, &sub).unwrap().status, MemberStatus::Removed, "nothing was written");
}

// The search is a substring match: the right person is found past a page of near misses, and a
// near miss alone is no match.
#[tokio::test]
async fn invite_matches_the_exact_email_over_pages() {
    let t = mw(Mode::SelfHosted).await;
    for n in 0..205 {
        t.identity.with_invited_user(&Pool::Members, &format!("x{n}ann@x.io"));
    }
    let sub = t.identity.with_invited_user(&Pool::Members, "ann@x.io");
    created(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "ann@x.io", "developer", &["app"]))).await).await;
    assert_eq!(t.w.metadata.member(t.w.tenant, &sub).unwrap().status, MemberStatus::Invited, "the existing user");
    let before = t.identity.calls().iter().filter(|c| c.starts_with("POST /internal/v1/pools/members/users")).count();
    assert_eq!(before, 0, "no second user");
    // A substring of a stored address is a new person.
    created(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "n@x.io", "developer", &["app"]))).await).await;
    assert!(t.identity.user_by_email(&Pool::Members, "n@x.io").is_some());
}

// Two invites of one new email at once: the loser of the create reads the user again.
#[tokio::test]
async fn invite_create_conflict_is_retryable() {
    let t = mw(Mode::SelfHosted).await;
    t.identity.fail("POST /internal/v1/pools/members/users", 409, "conflict", 1);
    let err = t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "race@x.io", "developer", &["app"]))).await;
    // The user was not really made by the forced fault, so the re-read finds nobody: a retryable 503.
    let err = error_of(err, 503).await;
    assert_eq!(code_of(&err), "identity_unavailable");
}

// #122 N4: the create lost to a concurrent invite that made the same user: the search runs once
// more and this invite goes to that user.
#[tokio::test]
async fn invite_create_lost_to_a_concurrent_invite_uses_that_user() {
    let t = mw(Mode::SelfHosted).await;
    t.identity.fail_after("POST /internal/v1/pools/members/users", 409, "conflict", 1);
    let body = created(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "race@x.io", "developer", &["app"]))).await).await;
    assert_eq!(body["outcome"], "invited");
    let sub = sub_of(&t, "race@x.io");
    assert_eq!(t.w.metadata.member(t.w.tenant, &sub).unwrap().status, MemberStatus::Invited);
    assert!(t.identity.user(&Pool::Members, &sub).is_some(), "not this call's user: never deleted");
}

// #122 B1: a failed offboard write doesn't skip the identity half of a removal: the sessions end
// and the account is banned in the same request, the answer is a retryable 503, and a retry
// finishes the write.
#[tokio::test]
async fn adv_remove_offboard_failure_still_ends_identity_access() {
    let t = mw(Mode::SelfHosted).await;
    let (member, sub) = identity_member(&t, "adv5@x.com");
    t.identity.with_session(&Pool::Members, &sub);
    *t.w.metadata.fail_offboard.lock().unwrap() = true;
    let _ = t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/members/{member}"), None).await;
    assert_eq!(t.identity.session_count(&Pool::Members), 0, "sessions left alive after the 500");
    assert!(t.identity.user(&Pool::Members, &sub).unwrap().banned, "not banned after the 500");
}

// #122 B1, decline: the same.
#[tokio::test]
async fn decline_offboard_failure_still_ends_identity_access() {
    let t = mw(Mode::SelfHosted).await;
    let sub = t.identity.with_unverified_user(&Pool::Members, "pia@x.com", "Secret-pass1");
    let pia = t.w.metadata.add_member(t.w.tenant, &sub, MemberStatus::Pending, false);
    t.identity.with_session(&Pool::Members, &sub);
    *t.w.metadata.fail_offboard.lock().unwrap() = true;
    let path = format!("/v1/members/{pia}/decline");
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, &path, None).await, 503).await;
    assert_eq!(code_of(&err), "unavailable", "{err}");
    assert_eq!(t.identity.session_count(&Pool::Members), 0);
    assert!(t.identity.user(&Pool::Members, &sub).unwrap().banned);
    *t.w.metadata.fail_offboard.lock().unwrap() = false;
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &path, None).await.status(), 204);
    assert!(t.w.metadata.member(t.w.tenant, &sub).unwrap().offboarded);
}

// #122 S1 (decided): removal from the last *active* tenant always ends the sessions; the account
// is banned only when no other non-removed row exists, so an open invite, a pending sign-up or a
// suspension elsewhere keeps it usable for that acceptance, approval or reinstatement.
#[tokio::test]
async fn adv_remove_ends_sessions_while_only_an_inactive_row_remains() {
    for other in [MemberStatus::Invited, MemberStatus::Pending, MemberStatus::Suspended] {
        let t = mw(Mode::SelfHosted).await;
        let (member, sub) = identity_member(&t, "adv1@x.com");
        second_tenant(&t, &sub, other);
        t.identity.with_session(&Pool::Members, &sub);
        assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/members/{member}"), None).await.status(), 204, "{other:?}");
        assert_eq!(t.identity.session_count(&Pool::Members), 0, "{other:?}: no other active tenant");
        let user = t.identity.user(&Pool::Members, &sub).unwrap();
        assert!(!user.banned, "{other:?}: another row lives: {user:?}");
    }
}

// #122 S1, suspend: the sessions end when no other tenant holds them as an active member.
#[tokio::test]
async fn suspend_ends_sessions_when_the_only_other_row_is_an_open_invite() {
    let t = mw(Mode::SelfHosted).await;
    let (member, sub) = identity_member(&t, "two@x.com");
    second_tenant(&t, &sub, MemberStatus::Invited);
    t.identity.with_session(&Pool::Members, &sub);
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &format!("/v1/members/{member}/suspend"), None).await.status(), 200);
    assert_eq!(t.identity.session_count(&Pool::Members), 0);
    assert!(!t.identity.user(&Pool::Members, &sub).unwrap().banned);
}

// #122 re-audit B1: an account banned for a reason that isn't ours can't be reinstated. The
// answer is 409 and nothing is written: the member stays suspended and refused, and the ban stays.
#[tokio::test]
async fn reinstate_under_a_foreign_ban_is_refused() {
    let t = mw(Mode::SelfHosted).await;
    let (member, sub) = identity_member(&t, "sue2@x.com");
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &format!("/v1/members/{member}/suspend"), None).await.status(), 200);
    assert!(!t.w.metadata.member(t.w.tenant, &sub).unwrap().offboarded, "suspend sets no mark");
    t.identity.client().ban(&Pool::Members, &sub, Some("abuse")).await.unwrap();
    let before = t.w.app.rt_events.access().len();
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, &format!("/v1/members/{member}/reinstate"), None).await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    assert_eq!(t.w.metadata.member(t.w.tenant, &sub).unwrap().status, MemberStatus::Suspended);
    assert_eq!(t.w.app.rt_events.access().len(), before, "nothing changed");
    let err = error_of(t.send(&sub, Method::GET, "/v1/members", None).await, 403).await;
    assert_eq!(code_of(&err), "member_suspended");
    let user = t.identity.user(&Pool::Members, &sub).unwrap();
    assert!(user.banned, "{user:?}");
    assert_eq!(user.ban_reason.as_deref(), Some("abuse"));
}

// #122 re-audit S1: a link hands the admin a password-setting code, so it is refused for a person
// another tenant still holds (an open invite there included). By email the code reaches only
// the mailbox owner, so that stays allowed; a resend by link is refused the same way.
#[tokio::test]
async fn link_invite_refused_for_another_tenants_invitee() {
    let t = mw(Mode::SelfHosted).await;
    let sub = t.identity.with_invited_user(&Pool::Members, "theirs@x.com");
    second_tenant(&t, &sub, MemberStatus::Invited);
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_link_body(&t, "theirs@x.com"))).await, 409).await;
    assert_eq!(code_of(&err), "link_not_allowed");
    assert!(t.w.metadata.member(t.w.tenant, &sub).is_none(), "no member row");
    assert!(!t.identity.calls().iter().any(|c| c.contains(CODES_ROUTE)), "no code is created");

    let body = created(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "theirs@x.com", "developer", &["app"]))).await).await;
    assert_eq!(t.identity.codes_sent().len(), 1, "email delivery stays allowed");
    let invite = body["member"]["invite"]["id"].as_str().unwrap().to_string();
    let path = format!("/v1/invites/{invite}/resend");
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, &path, Some(json!({ "delivery": "link" }))).await, 409).await;
    assert_eq!(code_of(&err), "link_not_allowed");
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &path, None).await.status(), 200, "by email it is resent");
}

// A link for someone whose only other row is removed is still a new person's link.
#[tokio::test]
async fn link_invite_allowed_when_the_other_tenant_removed_them() {
    let t = mw(Mode::SelfHosted).await;
    let sub = t.identity.with_invited_user(&Pool::Members, "gone@x.com");
    second_tenant(&t, &sub, MemberStatus::Removed);
    let body = created(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_link_body(&t, "gone@x.com"))).await).await;
    assert!(body["invite_url"].is_string(), "{body}");
}

// The m10 follow-up: finding the invitee is one exact identity lookup, never a substring search
// paged to the end. 650 users contain `a@x.com`; none is it.
#[tokio::test]
async fn invite_lookup_is_one_identity_call() {
    let t = mw(Mode::SelfHosted).await;
    for i in 0..650 {
        t.identity.with_invited_user(&Pool::Members, &format!("u{i}a@x.com"));
    }
    let list = "GET /internal/v1/pools/members/users";
    let before = t.identity.calls().len();
    created(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "a@x.com", "developer", &["app"]))).await).await;
    let lists = t.identity.calls()[before..].iter().filter(|c| c.as_str() == list).count();
    assert_eq!(lists, 1, "user-list reads for one invite");
}

// #122 re-audit N1: other memberships unreadable after the revoke is the retryable 503, and the
// same revoke again finishes it.
#[tokio::test]
async fn revoke_invite_other_memberships_failure_is_retryable() {
    let t = mw(Mode::SelfHosted).await;
    let body = created(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "new@x.io", "developer", &["app"]))).await).await;
    let invite = body["member"]["invite"]["id"].as_str().unwrap().to_string();
    let path = format!("/v1/invites/{invite}");
    *t.w.metadata.fail_other_memberships.lock().unwrap() = true;
    let err = error_of(t.send(DEFAULT_SUB, Method::DELETE, &path, None).await, 503).await;
    assert_eq!(code_of(&err), "unavailable");
    *t.w.metadata.fail_other_memberships.lock().unwrap() = false;
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &path, None).await.status(), 204);
    assert_eq!(t.identity.user_by_email(&Pool::Members, "new@x.io"), None);
}

// #122 re-audit N2: with no identity client, ending access for someone active in another tenant
// has nothing to do there, so it is no 503.
#[tokio::test]
async fn no_identity_client_is_no_503_while_active_elsewhere() {
    let t = mw_without_identity().await;
    let bob = t.w.metadata.add_granted(t.w.tenant, "bob", t.w.project, PlatformRole::Developer, &["app"]);
    let ann = t.w.metadata.add_granted(t.w.tenant, "ann", t.w.project, PlatformRole::Developer, &["app"]);
    second_tenant(&t, "bob", MemberStatus::Active);
    second_tenant(&t, "ann", MemberStatus::Active);
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &format!("/v1/members/{bob}/suspend"), None).await.status(), 200);
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/members/{ann}"), None).await.status(), 204);
}

// #122 S3: an invite that fails before its member row is written deletes the user it made only
// when no other tenant's row references it by then.
#[tokio::test]
async fn invite_failing_before_its_row_keeps_a_user_another_tenant_reached() {
    for shared in [true, false] {
        let t = mw(Mode::SelfHosted).await;
        let team = t.w.metadata.seed_team(t.w.tenant, "Racy");
        t.w.metadata.seed_team_grant(team, t.w.project, PlatformRole::Viewer, &[]);
        let project = t.w.project;
        // The team changes before each write, so the store gives up after its retries.
        t.w.metadata.race_teams_once(move |md| md.seed_team_grant(team, project, PlatformRole::Developer, &[]));
        let identity = t.identity.clone();
        t.w.metadata.race_teams_once(move |md| {
            md.seed_team_grant(team, project, PlatformRole::Viewer, &[]);
            if shared {
                // Another tenant's invite reaches the new user meanwhile.
                let sub = identity.user_by_email(&Pool::Members, "new@x.com").expect("made by this call");
                md.add_member(TenantId::new(), &sub, MemberStatus::Invited, false);
            }
        });
        let body = json!({ "email": "new@x.com", "grants": [], "team_ids": [team] });
        error_of(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(body)).await, 409).await;
        let left = t.identity.user_by_email(&Pool::Members, "new@x.com");
        assert_eq!(left.is_some(), shared, "shared: {shared}");
    }
}

// A removed member keeps the mark in metadata; a new invite clears it, or they could never use
// the tenant after accepting.
#[tokio::test]
async fn reinvite_clears_the_offboarded_mark() {
    let t = mw(Mode::SelfHosted).await;
    let (member, sub) = identity_member(&t, "back@x.com");
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/members/{member}"), None).await.status(), 204);
    assert!(t.w.metadata.member(t.w.tenant, &sub).unwrap().offboarded);
    created(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "back@x.com", "developer", &["app"]))).await).await;
    let stored = t.w.metadata.member(t.w.tenant, &sub).unwrap();
    assert_eq!(stored.status, MemberStatus::Invited);
    assert!(!stored.offboarded);
    assert!(t.identity.user(&Pool::Members, &sub).unwrap().banned, "the ban stays until they accept (m11)");
}

// #119 B3: a tenant admin on a member token has no org, and the members pool doesn't need one.
#[tokio::test]
async fn invite_works_on_a_member_token() {
    let t = mw(Mode::SelfHosted).await;
    #[allow(clippy::disallowed_methods, clippy::disallowed_types)] // test code: a plain client against the test app
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/invites", t.w.app.base))
        .header(IDENTITY_HEADER, member_identity_token(&t.w.app, DEFAULT_SUB, t.w.tenant.as_uuid()))
        .json(&invite_body(&t.w, "new@x.com", "developer", &["app"]))
        .send()
        .await
        .unwrap();
    created(resp).await;
    assert!(t.identity.user_by_email(&Pool::Members, "new@x.com").is_some());
}

// A revoked invite: a never-activated user goes; one with a password stays as it is, and one
// shared with another tenant too.
#[tokio::test]
async fn revoke_invite_deletes_only_a_passwordless_unshared_user() {
    let t = mw(Mode::SelfHosted).await;
    let revoke = async |email: &str| {
        let body = created(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, email, "developer", &["app"]))).await).await;
        let invite = body["member"]["invite"]["id"].as_str().unwrap().to_string();
        t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/invites/{invite}"), None).await
    };
    assert_eq!(revoke("new@x.io").await.status(), 204);
    assert_eq!(t.identity.user_by_email(&Pool::Members, "new@x.io"), None);

    let sub = t.identity.with_user(&Pool::Members, "ann@x.io", "Secret-pass1");
    assert_eq!(revoke("ann@x.io").await.status(), 204);
    let user = t.identity.user(&Pool::Members, &sub).expect("an established account stays");
    assert!(!user.banned, "{user:?}");

    let shared = t.identity.with_invited_user(&Pool::Members, "both@x.io");
    second_tenant(&t, &shared, MemberStatus::Invited);
    assert_eq!(revoke("both@x.io").await.status(), 204);
    assert!(t.identity.user(&Pool::Members, &shared).is_some());
}

// The identity step of a revoke fails: 503, retried by revoking again, and the row stays revoked.
#[tokio::test]
async fn revoke_invite_retries_the_identity_step() {
    let t = mw(Mode::SelfHosted).await;
    let body = created(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "new@x.io", "developer", &["app"]))).await).await;
    let invite = body["member"]["invite"]["id"].as_str().unwrap().to_string();
    t.identity.fail("DELETE /internal/v1/pools/members/users/", 503, "internal", 1);
    let path = format!("/v1/invites/{invite}");
    let err = error_of(t.send(DEFAULT_SUB, Method::DELETE, &path, None).await, 503).await;
    assert_eq!(code_of(&err), "identity_unavailable");
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &path, None).await.status(), 204);
    assert_eq!(t.identity.user_by_email(&Pool::Members, "new@x.io"), None);
}
