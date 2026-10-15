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
        .bearer_auth("some-zitadel-jwt")
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
        .sign_at(Service::Orchestrator.name(), &identity("org-1"), now - 3600)
        .unwrap();
    let resp = post_create(&app, Some(&token), create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn identity_minted_for_another_service_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let token = app.signer.sign("some-other-service", &identity("org-1")).unwrap();
    let resp = post_create(&app, Some(&token), create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn identity_from_untrusted_key_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let (other_private, _) = generate_keypair_pem();
    let forger = InternalSigner::from_pem(other_private.as_bytes()).unwrap();
    let token = forger
        .sign(Service::Orchestrator.name(), &identity("org-1"))
        .unwrap();
    let resp = post_create(&app, Some(&token), create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn unbound_org_is_tenant_not_bound() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let token = app.token_for_org("org-not-bound");
    let resp = post_create(&app, Some(&token), create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 403);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(code_of(&body), "tenant_not_bound");
    assert!(message_of(&body).contains("zitadel-bootstrap"));
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
async fn me_for_bound_org_returns_tenant_projects_and_databases() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    metadata.set_overview(overview_with_database(tenant));
    let app = spawn_app(metadata, FakeJobs::default()).await;

    let resp = get_me(&app, Some(&app.token_for_org("org-1"))).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["user"]["sub"], "user-1");
    assert_eq!(body["user"]["org_id"], "org-1");
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
async fn me_for_unbound_org_returns_null_tenant() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = get_me(&app, Some(&app.token_for_org("org-unbound"))).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["user"]["org_id"], "org-unbound");
    assert!(body["tenant"].is_null());
}

// Review Focus 2.
#[tokio::test]
async fn me_for_bound_org_with_missing_tenant_row_returns_null_tenant() {
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
    RoleReply { ok: false, code: Some(code.into()), error: Some("worker said so".into()), keys: vec![] }
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
    assert_eq!(resp.status(), 204);
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
    assert_eq!(resp.status(), 204);
    assert_eq!(
        *t.jobs.role_calls.lock().unwrap(),
        vec!["reset:a/b%61?#".to_string(), "delete:a/b%61?#".to_string()]
    );
}

#[tokio::test]
async fn delete_role_204() {
    let t = branching().await;
    let dev = t.ready("dev");
    let resp = t.send(Method::DELETE, &format!("{}/app", t.roles(dev)), None).await;
    assert_eq!(resp.status(), 204);
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

/// A rest stand-in that records every request (on any path) and answers the console route
/// with `answer(body)` and the schema route with `answer(null)`; other paths answer 404.
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
            if !schema && !(path.starts_with("/internal/console/") && path.ends_with("/query")) {
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

// ---- App users: settings and user admin ---------------------------------------------

use chelabase_query::app_users::{NewUserRow, PendingInsert, SyncTarget, UserRowError, UsersDb};
use chelabase_zitadel::client::Zitadel;
use chelabase_zitadel::mock::{ADMIN_TOKEN, MockZitadel, User as MockUser};

/// What `FakeUsersDb` does when asked to insert or to commit.
#[derive(Clone, Copy, PartialEq, Debug)]
enum UdbFault {
    Constraint,
    EmailTaken,
    CommitFails,
}

/// A users-table writer that journals what it was asked, and (with a probe) how many users
/// Zitadel had at each step.
#[derive(Clone, Default)]
struct FakeUsersDb {
    journal: Arc<Mutex<Vec<String>>>,
    targets: Arc<Mutex<Vec<SyncTarget>>>,
    rows: Arc<Mutex<Vec<NewUserRow>>>,
    fault: Arc<Mutex<Option<UdbFault>>>,
    probe: Arc<Mutex<Option<Arc<Mutex<chelabase_zitadel::mock::Inner>>>>>,
}

impl FakeUsersDb {
    fn zitadel_users(&self) -> usize {
        self.probe.lock().unwrap().as_ref().map_or(0, |i| i.lock().unwrap().users.len())
    }
    fn journal(&self) -> Vec<String> {
        self.journal.lock().unwrap().clone()
    }
}

struct FakePending {
    db: FakeUsersDb,
}

#[async_trait::async_trait]
impl PendingInsert for FakePending {
    async fn commit(self: Box<Self>) -> Result<(), UserRowError> {
        let users = self.db.zitadel_users();
        self.db.journal.lock().unwrap().push(format!("commit:users={users}"));
        if *self.db.fault.lock().unwrap() == Some(UdbFault::CommitFails) {
            return Err(UserRowError::Unavailable("commit failed".into()));
        }
        Ok(())
    }
    async fn rollback(self: Box<Self>) {
        self.db.journal.lock().unwrap().push("rollback".into());
    }
}

#[async_trait::async_trait]
impl UsersDb for FakeUsersDb {
    async fn begin_insert(&self, target: &SyncTarget, row: &NewUserRow) -> Result<Box<dyn PendingInsert>, UserRowError> {
        let users = self.zitadel_users();
        self.journal.lock().unwrap().push(format!("begin:{}:users={users}", row.email));
        self.targets.lock().unwrap().push(target.clone());
        self.rows.lock().unwrap().push(row.clone());
        match *self.fault.lock().unwrap() {
            Some(UdbFault::Constraint) => Err(UserRowError::Constraint {
                message: "null value in column \"nick\"".into(),
                constraint: Some("users_nick_check".into()),
            }),
            Some(UdbFault::EmailTaken) => Err(UserRowError::EmailTaken),
            _ => Ok(Box::new(FakePending { db: self.clone() })),
        }
    }
}

const AU_ORG: &str = "o1";
const AU_PASSWORD: &str = "correct-horse-9";

/// A project with a ready database (main branch, Postgres database `web1` served by the data
/// API), org-1's tenant, and optionally Zitadel's mock behind the orchestrator.
struct Au {
    app: TestApp,
    metadata: FakeMetadata,
    jobs: FakeJobs,
    tenant: TenantId,
    project: ProjectRecord,
    db: DatabaseId,
    main: BranchId,
    mock: Option<MockZitadel>,
    udb: FakeUsersDb,
}

async fn au_env(with_zitadel: bool) -> Au {
    let metadata = FakeMetadata::default();
    let jobs = FakeJobs::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let project = metadata.create_project(tenant, "Web").await.unwrap();
    let db = project_database(tenant, project.id, DatabaseStatus::Ready);
    let (db_id, main) = (db.database_id, db.main.id);
    metadata.insert_database(db);
    metadata.add_branch_db(main, "web1", "owner", DatabaseOrigin::Main);
    metadata.set_data_api(main, "web1", chelabase_metadata::DataApiState::On);
    let udb = FakeUsersDb::default();
    let (mock, zitadel) = if with_zitadel {
        let mock = MockZitadel::start().await;
        *udb.probe.lock().unwrap() = Some(mock.inner.clone());
        let z = Zitadel::new(&mock.base, ADMIN_TOKEN);
        (Some(mock), Some(z))
    } else {
        (None, None)
    };
    let app = spawn_app_parts(
        metadata.clone(),
        jobs.clone(),
        "http://127.0.0.1:9",
        Mode::SelfHosted,
        10,
        zitadel,
        Arc::new(udb.clone()),
    )
    .await;
    Au { app, metadata, jobs, tenant, project, db: db_id, main, mock, udb }
}

impl Au {
    fn path(&self, rest: &str) -> String {
        format!("/v1/projects/{}{rest}", self.project.id)
    }

    async fn call(&self, method: Method, rest: &str, body: Option<Value>) -> (u16, Value) {
        let resp = send(&self.app, method, &self.path(rest), "org-1", body).await;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap();
        (status, if text.is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap() })
    }

    fn set_auth(&self, status: AppUsersStatus) {
        let now = chrono::Utc::now();
        self.metadata.project_auths.lock().unwrap().insert(
            self.project.id,
            ProjectAuth {
                project_id: self.project.id,
                status,
                error: None,
                zitadel_org_id: Some(AU_ORG.into()),
                database_id: self.db,
                pg_database: "web1".into(),
                schema_name: "public".into(),
                table_name: "users".into(),
                email_verification: false,
                on_delete: OnDelete::Delete,
                created_at: now,
                updated_at: now,
            },
        );
    }

    fn auth(&self) -> ProjectAuth {
        self.metadata.project_auths.lock().unwrap().get(&self.project.id).cloned().expect("project_auth row")
    }

    /// The sync role's sealed password on the auth database's main branch.
    fn seal_sync_role(&self) {
        let sealed = chelabase_secrets::seal_role_password(&self.app.key, &self.main.to_string(), "chelabase_auth_sync", "sync-pw");
        self.metadata.branch_syncs.lock().unwrap().insert(
            self.main,
            BranchAuthSync { branch_id: self.main, pg_database: "web1".into(), password_sealed: sealed },
        );
        *self.app.locator.addr.lock().unwrap() = Some("127.0.0.1:55432".parse().unwrap());
    }

    fn mock(&self) -> &MockZitadel {
        self.mock.as_ref().expect("zitadel mock")
    }

    /// A Zitadel user with a UUID id, in org `org`.
    fn add_user(&self, org: &str, email: &str) -> uuid::Uuid {
        let id = uuid::Uuid::new_v4();
        self.mock().inner.lock().unwrap().users.push(MockUser {
            id: id.to_string(),
            email: email.into(),
            password: AU_PASSWORD.into(),
            verified: true,
            locked: false,
            org_id: org.into(),
            username: email.into(),
            display_name: String::new(),
            inactive: false,
            created_at: "2026-10-03T10:00:00Z".into(),
        });
        id
    }

    fn add_session(&self, user: uuid::Uuid, zitadel_session: &str) {
        let now = chrono::Utc::now();
        self.metadata.app_sessions.lock().unwrap().push(AppSession {
            id: uuid::Uuid::new_v4(),
            project_id: self.project.id,
            user_id: user,
            zitadel_session_id: zitadel_session.into(),
            created_at: now,
            last_used_at: now,
            expires_at: now + chrono::Duration::days(30),
            revoked_at: None,
        });
    }

    fn live_sessions(&self, user: uuid::Uuid) -> usize {
        self.metadata
            .app_sessions
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.user_id == user && s.revoked_at.is_none())
            .count()
    }

    fn syncs(&self) -> Vec<AppUsersSyncJob> {
        self.jobs.app_users_sync_jobs.lock().unwrap().clone()
    }

    fn jobs_published(&self) -> Vec<AppUsersJob> {
        self.jobs.app_users_jobs.lock().unwrap().clone()
    }
}

fn put_body(au: &Au) -> Value {
    json!({ "database_id": au.db })
}

#[tokio::test]
async fn get_app_users_404_until_enabled_then_the_settings() {
    let au = au_env(false).await;
    let (status, body) = au.call(Method::GET, "/app-users", None).await;
    assert_eq!((status, code_of(&body)), (404, "app_users_disabled"), "{body}");
    au.set_auth(AppUsersStatus::Enabled);
    let (status, body) = au.call(Method::GET, "/app-users", None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], "enabled");
    assert_eq!(body["database_id"], au.db.to_string());
    assert_eq!((body["pg_database"].as_str(), body["schema"].as_str(), body["table"].as_str()), (Some("web1"), Some("public"), Some("users")));
    assert_eq!((body["email_verification"].as_bool(), body["on_delete"].as_str()), (Some(false), Some("delete")));
    assert_eq!(body["zitadel_org_id"], AU_ORG);
    assert!(body["error"].is_null());
}

#[tokio::test]
async fn app_users_routes_refuse_a_deleting_project_and_another_tenants() {
    let au = au_env(false).await;
    let theirs = au.metadata.create_project(TenantId::new(), "Theirs").await.unwrap();
    let resp = send(&au.app, Method::GET, &format!("/v1/projects/{}/app-users", theirs.id), "org-1", None).await;
    assert_eq!(resp.status(), 404);
    au.metadata.begin_project_delete(au.tenant, au.project.id).await.unwrap();
    for (method, rest) in [(Method::GET, "/app-users"), (Method::DELETE, "/app-users"), (Method::GET, "/users")] {
        let (status, body) = au.call(method, rest, None).await;
        assert_eq!((status, code_of(&body)), (409, "project_deleting"), "{rest}: {body}");
    }
    let (status, body) = au.call(Method::PUT, "/app-users", Some(put_body(&au))).await;
    assert_eq!((status, code_of(&body)), (409, "project_deleting"), "{body}");
}

#[tokio::test]
async fn enable_app_users_202_publishes() {
    let au = au_env(true).await;
    let (status, body) = au.call(Method::PUT, "/app-users", Some(put_body(&au))).await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["status"], "enabling");
    assert_eq!((body["pg_database"].as_str(), body["schema"].as_str(), body["table"].as_str()), (Some("web1"), Some("public"), Some("users")));
    assert_eq!((body["email_verification"].as_bool(), body["on_delete"].as_str()), (Some(false), Some("delete")));
    assert_eq!(au.jobs_published(), vec![AppUsersJob::Enable { project_id: au.project.id }]);
    assert_eq!(*au.jobs.app_users_checks.lock().unwrap(), vec!["web1.public.users".to_string()]);
    assert_eq!(au.auth().status, AppUsersStatus::Enabling);
}

#[tokio::test]
async fn put_while_enabling_republishes() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabling);
    let (status, body) = au.call(Method::PUT, "/app-users", Some(put_body(&au))).await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(au.jobs_published(), vec![AppUsersJob::Enable { project_id: au.project.id }]);
    assert!(au.jobs.app_users_checks.lock().unwrap().is_empty(), "an existing location isn't checked again");
}

#[tokio::test]
async fn delete_while_disabling_republishes() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Disabling);
    let (status, body) = au.call(Method::DELETE, "/app-users", None).await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(au.jobs_published(), vec![AppUsersJob::Disable { project_id: au.project.id }]);
}

#[tokio::test]
async fn reserved_schema_is_422() {
    let au = au_env(true).await;
    for schema in ["chelabase", "information_schema", "pg_x", "pg_catalog"] {
        let mut body = put_body(&au);
        body["schema"] = json!(schema);
        let (status, answer) = au.call(Method::PUT, "/app-users", Some(body)).await;
        assert_eq!(status, 422, "{schema}: {answer}");
    }
    assert!(au.jobs_published().is_empty());
    assert!(au.metadata.project_auths.lock().unwrap().is_empty());
}

#[tokio::test]
async fn app_users_enable_refusals() {
    let au = au_env(true).await;
    let put = |extra: Value| {
        let mut body = put_body(&au);
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        body
    };
    // Identifiers and on_delete.
    for bad in [json!({"schema": "Bad-Name"}), json!({"table": "1x"}), json!({"table": ""}), json!({"on_delete": "keep"}), json!({"pg_database": ""})] {
        let (status, body) = au.call(Method::PUT, "/app-users", Some(put(bad.clone()))).await;
        assert_eq!(status, 422, "{bad}: {body}");
    }
    // Another project's database (same tenant) is unknown to this one.
    let other = au.metadata.create_project(au.tenant, "Other").await.unwrap();
    let foreign = project_database(au.tenant, other.id, DatabaseStatus::Ready);
    let foreign_id = foreign.database_id;
    au.metadata.insert_database(foreign);
    let (status, body) = au.call(Method::PUT, "/app-users", Some(json!({"database_id": foreign_id}))).await;
    assert_eq!((status, code_of(&body)), (404, "not_found"), "{body}");
    // Another tenant's.
    let theirs = project_database(TenantId::new(), ProjectId::new(), DatabaseStatus::Ready);
    let theirs_id = theirs.database_id;
    au.metadata.insert_database(theirs);
    let (status, _) = au.call(Method::PUT, "/app-users", Some(json!({"database_id": theirs_id}))).await;
    assert_eq!(status, 404);
    // Not ready.
    let pending = project_database(au.tenant, au.project.id, DatabaseStatus::Provisioning);
    let pending_id = pending.database_id;
    au.metadata.insert_database(pending);
    let (status, body) = au.call(Method::PUT, "/app-users", Some(json!({"database_id": pending_id}))).await;
    assert_eq!((status, code_of(&body)), (409, "database_not_ready"), "{body}");
    // The data API is off on a child branch.
    let child = au.metadata.add_branch(au.db, "dev", au.main, BranchKind::Normal, BranchStatus::Ready);
    au.metadata.add_branch_db(child, "web1", "owner", DatabaseOrigin::Main);
    let (status, body) = au.call(Method::PUT, "/app-users", Some(put_body(&au))).await;
    assert_eq!((status, code_of(&body)), (409, "data_api_not_enabled"), "{body}");
    au.metadata.set_data_api(child, "web1", chelabase_metadata::DataApiState::On);
    // The table exists.
    *au.jobs.app_users_check_reply.lock().unwrap() =
        Some(AppUsersCheckReply { ok: false, code: Some("app_users_table_exists".into()), error: Some("exists".into()) });
    let (status, body) = au.call(Method::PUT, "/app-users", Some(put_body(&au))).await;
    assert_eq!((status, code_of(&body)), (409, "app_users_table_exists"), "{body}");
    // Any other worker failure is a 503.
    *au.jobs.app_users_check_reply.lock().unwrap() =
        Some(AppUsersCheckReply { ok: false, code: Some("boom".into()), error: Some("no".into()) });
    let (status, _) = au.call(Method::PUT, "/app-users", Some(put_body(&au))).await;
    assert_eq!(status, 503);
    assert!(au.jobs_published().is_empty());
    assert!(au.metadata.project_auths.lock().unwrap().is_empty());
}

#[tokio::test]
async fn put_again_updates_settings_200() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let mut body = put_body(&au);
    body["email_verification"] = json!(true);
    body["on_delete"] = json!("mark");
    let (status, answer) = au.call(Method::PUT, "/app-users", Some(body)).await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!((answer["email_verification"].as_bool(), answer["on_delete"].as_str()), (Some(true), Some("mark")));
    assert_eq!(answer["status"], "enabled");
    assert_eq!(au.auth().on_delete, OnDelete::Mark);
    assert!(au.jobs_published().is_empty());
}

#[tokio::test]
async fn put_with_other_location_409_location_fixed() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    for extra in [json!({"schema": "app"}), json!({"table": "people"}), json!({"pg_database": "web2"})] {
        let mut body = put_body(&au);
        body[extra.as_object().unwrap().keys().next().unwrap()] = extra.as_object().unwrap().values().next().unwrap().clone();
        let (status, answer) = au.call(Method::PUT, "/app-users", Some(body)).await;
        assert_eq!((status, code_of(&answer)), (409, "app_users_location_fixed"), "{extra}: {answer}");
    }
    let other = project_database(au.tenant, au.project.id, DatabaseStatus::Ready);
    let other_id = other.database_id;
    au.metadata.insert_database(other);
    let (status, answer) = au.call(Method::PUT, "/app-users", Some(json!({"database_id": other_id}))).await;
    assert_eq!((status, code_of(&answer)), (409, "app_users_location_fixed"), "{answer}");
    assert_eq!(au.auth().database_id, au.db);
}

#[tokio::test]
async fn retry_after_failed_202() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Failed);
    au.metadata.project_auths.lock().unwrap().get_mut(&au.project.id).unwrap().error = Some("boom".into());
    let (status, body) = au.call(Method::PUT, "/app-users", Some(put_body(&au))).await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["status"], "enabling");
    assert!(body["error"].is_null());
    assert_eq!(au.jobs_published(), vec![AppUsersJob::Enable { project_id: au.project.id }]);
}

#[tokio::test]
async fn disable_refused_while_users_exist() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.add_user(AU_ORG, "a@x.test");
    let (status, body) = au.call(Method::DELETE, "/app-users", None).await;
    assert_eq!((status, code_of(&body)), (409, "app_users_has_users"), "{body}");
    assert!(au.jobs_published().is_empty());
    assert_eq!(au.auth().status, AppUsersStatus::Enabled);
}

#[tokio::test]
async fn disable_202_publishes() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.add_user("o2", "elsewhere@x.test");
    let (status, body) = au.call(Method::DELETE, "/app-users", None).await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["status"], "disabling");
    assert_eq!(au.jobs_published(), vec![AppUsersJob::Disable { project_id: au.project.id }]);
    assert_eq!(au.auth().status, AppUsersStatus::Disabling);
}

#[tokio::test]
async fn create_user_inserts_then_creates_then_commits() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.seal_sync_role();
    let (status, body) = au.call(Method::POST, "/users", Some(json!({"email": "new@x.test", "password": AU_PASSWORD}))).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!((body["email"].as_str(), body["email_verified"].as_bool(), body["state"].as_str()), (Some("new@x.test"), Some(true), Some("active")));
    let id: uuid::Uuid = body["id"].as_str().unwrap().parse().unwrap();
    // The row went in first (no Zitadel user yet) and committed after Zitadel had the user.
    assert_eq!(au.udb.journal(), vec!["begin:new@x.test:users=0".to_string(), "commit:users=1".to_string()]);
    let user = au.mock().user_by_id(&id.to_string()).expect("zitadel user");
    assert_eq!((user.email.as_str(), user.org_id.as_str(), user.verified), ("new@x.test", AU_ORG, true));
    let target = au.udb.targets.lock().unwrap()[0].clone();
    let loopback = std::net::Ipv4Addr::LOCALHOST.to_string();
    assert_eq!(
        (target.branch_id, target.host.as_str(), target.port, target.database.as_str(), target.user.as_str(), target.password.as_str()),
        (au.main.as_uuid(), loopback.as_str(), 55432, "web1", "chelabase_auth_sync", "sync-pw")
    );
    assert_eq!((target.schema.as_str(), target.table.as_str()), ("public", "users"));
    let row = au.udb.rows.lock().unwrap()[0].clone();
    assert_eq!((row.id, row.email_verified), (id, true));
    assert!(row.data.is_empty());
    let syncs = au.syncs();
    assert_eq!(syncs.len(), 1);
    assert_eq!((syncs[0].user_id, syncs[0].origin_branch_id, syncs[0].data.is_none()), (id, Some(au.main), true));
    assert_eq!(syncs[0].project_id, au.project.id);
}

// ---- T10 I3: the app-users sync reaches the compute's control address over TLS ----

/// [`au_env`] (with Zitadel) whose orchestrator has `compute_tls`; also the main branch's
/// endpoint id.
async fn au_env_tls(compute_tls: Arc<orchestrator::compute_tls::ComputeTls>) -> (Au, String) {
    let metadata = FakeMetadata::default();
    let jobs = FakeJobs::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let project = metadata.create_project(tenant, "Web").await.unwrap();
    let db = project_database(tenant, project.id, DatabaseStatus::Ready);
    let endpoint = db.main.endpoint_id.clone().expect("the main branch has an endpoint");
    let (db_id, main) = (db.database_id, db.main.id);
    metadata.insert_database(db);
    metadata.add_branch_db(main, "web1", "owner", DatabaseOrigin::Main);
    metadata.set_data_api(main, "web1", chelabase_metadata::DataApiState::On);
    let udb = FakeUsersDb::default();
    let mock = MockZitadel::start().await;
    *udb.probe.lock().unwrap() = Some(mock.inner.clone());
    let zitadel = Zitadel::new(&mock.base, ADMIN_TOKEN);
    let app = spawn_app_with_compute_tls(
        metadata.clone(),
        jobs.clone(),
        "http://127.0.0.1:9",
        Mode::SelfHosted,
        10,
        Some(zitadel),
        Arc::new(udb.clone()),
        None,
        Some(compute_tls),
    )
    .await;
    let au = Au { app, metadata, jobs, tenant, project, db: db_id, main, mock: Some(mock), udb };
    (au, endpoint)
}

/// A compute client pair written into a fresh directory (kept alive with the guard).
fn written_client_pair() -> (tempfile::TempDir, Arc<orchestrator::compute_tls::ComputeTls>) {
    use chelabase_neon::compute_tls::{ComputeCa, ORCHESTRATOR_CN, write_client_pair};
    let dir = tempfile::tempdir().unwrap();
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let pair = ca.issue_client(ORCHESTRATOR_CN, now).unwrap();
    write_client_pair(dir.path(), &pair, ca.cert_pem(), now).unwrap();
    let tls = Arc::new(orchestrator::compute_tls::ComputeTls::new(dir.path().to_path_buf()));
    (dir, tls)
}

// The sync's target carries the same compute TLS as every other compute connection: the
// compute's certificate name and the loaded client pair (taken from one snapshot).
#[tokio::test]
async fn app_user_sync_reaches_the_compute_over_tls() {
    let (_dir, compute_tls) = written_client_pair();
    let fingerprint = compute_tls.get().expect("the pair is loaded").fingerprint();
    let (au, endpoint) = au_env_tls(compute_tls).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.seal_sync_role();
    let (status, body) = au.call(Method::POST, "/users", Some(json!({"email": "new@x.test", "password": AU_PASSWORD}))).await;
    assert_eq!(status, 201, "{body}");
    let target = au.udb.targets.lock().unwrap()[0].clone();
    let tls = target.tls.as_ref().expect("the sync uses TLS to the compute");
    assert_eq!(tls.server_name, chelabase_neon::compute_tls::server_name(&endpoint));
    assert_eq!(tls.fingerprint, fingerprint);
    // The address stays the compute's (control) address.
    assert_eq!((target.host.as_str(), target.port), (std::net::Ipv4Addr::LOCALHOST.to_string().as_str(), 55432));
}

// R-T10a: no client pair loaded yet is 503 `compute_tls_failed`, never a plaintext
// connection; the log names the branch and the endpoint (RT3-7, T10 m2).
#[tokio::test]
async fn app_user_sync_without_a_client_pair_is_compute_tls_failed() {
    let logs = Logs::default();
    let _guard = logs.capture();
    let empty = tempfile::tempdir().unwrap();
    let compute_tls = Arc::new(orchestrator::compute_tls::ComputeTls::new(empty.path().to_path_buf()));
    let (au, endpoint) = au_env_tls(compute_tls).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.seal_sync_role();
    let (status, body) = au.call(Method::POST, "/users", Some(json!({"email": "new@x.test", "password": AU_PASSWORD}))).await;
    assert_eq!((status, code_of(&body)), (503, "compute_tls_failed"), "{body}");
    assert!(au.udb.journal().is_empty(), "no connection was tried: {:?}", au.udb.journal());
    assert!(au.mock().user("new@x.test").is_none());
    let text = logs.text();
    let line = text
        .lines()
        .find(|l| l.contains("no compute client certificate loaded"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(line.contains(&format!("branch_id={}", au.main)), "{line}");
    assert!(line.contains(&format!("endpoint_id={endpoint}")), "{line}");
}

#[tokio::test]
async fn create_user_email_verified_false_is_passed_on() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.seal_sync_role();
    let (status, body) =
        au.call(Method::POST, "/users", Some(json!({"email": "u@x.test", "password": AU_PASSWORD, "email_verified": false}))).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["email_verified"], false);
    assert!(!au.udb.rows.lock().unwrap()[0].email_verified);
    assert!(!au.mock().user("u@x.test").unwrap().verified);
}

#[tokio::test]
async fn create_user_commit_failure_deletes_zitadel_user() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.seal_sync_role();
    *au.udb.fault.lock().unwrap() = Some(UdbFault::CommitFails);
    let (status, body) = au.call(Method::POST, "/users", Some(json!({"email": "n@x.test", "password": AU_PASSWORD}))).await;
    assert_eq!((status, code_of(&body)), (503, "unavailable"), "{body}");
    assert!(au.mock().user("n@x.test").is_none());
    assert!(au.mock().calls().iter().any(|c| c.starts_with("DELETE /v2/users/")), "{:?}", au.mock().calls());
    assert!(au.syncs().is_empty());
}

#[tokio::test]
async fn create_user_constraint_422_never_calls_zitadel() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.seal_sync_role();
    *au.udb.fault.lock().unwrap() = Some(UdbFault::Constraint);
    let (status, body) = au.call(Method::POST, "/users", Some(json!({"email": "n@x.test", "password": AU_PASSWORD}))).await;
    assert_eq!((status, code_of(&body)), (422, "constraint_violation"), "{body}");
    assert_eq!(body["error"]["constraint"], "users_nick_check");
    assert!(au.mock().calls().is_empty(), "{:?}", au.mock().calls());
    assert!(au.syncs().is_empty());
}

#[tokio::test]
async fn create_user_email_taken_and_weak_password() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.seal_sync_role();
    *au.udb.fault.lock().unwrap() = Some(UdbFault::EmailTaken);
    let (status, body) = au.call(Method::POST, "/users", Some(json!({"email": "n@x.test", "password": AU_PASSWORD}))).await;
    assert_eq!((status, code_of(&body)), (409, "email_taken"), "{body}");
    *au.udb.fault.lock().unwrap() = None;
    // The user exists in the org but the table doesn't know it (N2): the org is asked before
    // Zitadel could make a duplicate, and the row is rolled back.
    au.add_user(AU_ORG, "dup@x.test");
    let (status, body) = au.call(Method::POST, "/users", Some(json!({"email": "DUP@x.test", "password": AU_PASSWORD}))).await;
    assert_eq!((status, code_of(&body)), (409, "email_taken"), "{body}");
    assert_eq!(au.udb.journal().last().map(String::as_str), Some("rollback"));
    assert!(!au.mock().calls().contains(&"POST /v2/users/new".to_string()));
    // Zitadel's password policy.
    let (status, body) = au.call(Method::POST, "/users", Some(json!({"email": "w@x.test", "password": "short"}))).await;
    assert_eq!((status, code_of(&body)), (422, "weak_password"), "{body}");
    assert_eq!(au.udb.journal().last().map(String::as_str), Some("rollback"));
    assert!(au.syncs().is_empty());
}

#[tokio::test]
async fn list_users_pages_and_searches() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    for i in 0..55 {
        au.add_user(AU_ORG, &format!("user{i:02}@x.test"));
    }
    au.add_user("o2", "stranger@x.test");
    let (status, page) = au.call(Method::GET, "/users", None).await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["users"].as_array().unwrap().len(), 50);
    let token = page["next_page_token"].as_str().expect("next token").to_string();
    assert_eq!(page["users"][0]["state"], "active");
    let (status, page2) = au.call(Method::GET, &format!("/users?page_token={token}"), None).await;
    assert_eq!(status, 200, "{page2}");
    assert_eq!(page2["users"].as_array().unwrap().len(), 5);
    assert!(page2["next_page_token"].is_null());
    let (_, found) = au.call(Method::GET, "/users?search=USER07", None).await;
    assert_eq!(found["users"].as_array().unwrap().len(), 1);
    assert_eq!(found["users"][0]["email"], "user07@x.test");
    let (status, bad) = au.call(Method::GET, "/users?page_token=not-a-token", None).await;
    assert_eq!(status, 422, "{bad}");
}

#[tokio::test]
async fn get_user_and_sessions() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user(AU_ORG, "a@x.test");
    au.add_session(id, "s1");
    let (status, body) = au.call(Method::GET, &format!("/users/{id}"), None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!((body["id"].as_str(), body["email"].as_str()), (Some(id.to_string().as_str()), Some("a@x.test")));
    let (status, sessions) = au.call(Method::GET, &format!("/users/{id}/sessions"), None).await;
    assert_eq!(status, 200, "{sessions}");
    assert_eq!(sessions.as_array().unwrap().len(), 1);
    for key in ["id", "created_at", "last_used_at", "expires_at"] {
        assert!(sessions[0][key].is_string(), "{key}");
    }
    assert!(sessions[0].get("zitadel_session_id").is_none());
    let (status, _) = au.call(Method::GET, &format!("/users/{}", uuid::Uuid::new_v4()), None).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn update_password_revokes_sessions() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user(AU_ORG, "a@x.test");
    au.add_session(id, "s1");
    au.add_session(id, "s2");
    let (status, body) = au.call(Method::PATCH, &format!("/users/{id}"), Some(json!({"password": "brand-new-pw-1"}))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(au.mock().user_by_id(&id.to_string()).unwrap().password, "brand-new-pw-1");
    assert_eq!(au.live_sessions(id), 0);
    let calls = au.mock().calls();
    assert!(calls.contains(&"DELETE /v2/sessions/s1".to_string()) && calls.contains(&"DELETE /v2/sessions/s2".to_string()), "{calls:?}");
    assert_eq!(au.syncs().len(), 1);
    assert_eq!(au.syncs()[0].user_id, id);
}

#[tokio::test]
async fn update_email_and_flag() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user(AU_ORG, "a@x.test");
    let (status, body) = au.call(Method::PATCH, &format!("/users/{id}"), Some(json!({"email": "b@x.test"}))).await;
    assert_eq!(status, 200, "{body}");
    let user = au.mock().user_by_id(&id.to_string()).unwrap();
    assert_eq!((user.email.as_str(), user.verified), ("b@x.test", true));
    // A new address can be set unverified (a code is sent).
    let (status, body) = au.call(Method::PATCH, &format!("/users/{id}"), Some(json!({"email": "c@x.test", "email_verified": false}))).await;
    assert_eq!(status, 200, "{body}");
    let user = au.mock().user_by_id(&id.to_string()).unwrap();
    assert_eq!((user.email.as_str(), user.verified), ("c@x.test", false));
    assert_eq!(body["email_verified"], false);
    assert_eq!(au.syncs().len(), 2);
}

/// The Studio drawer's "Email verified" toggle (browser pass: 503). Zitadel v4.19.1 refuses
/// `POST /v2/users/{id}/email` with the unchanged address (`Errors.User.Email.NotChanged`), so
/// the flag alone goes through `PATCH /v2/users/{id}`.
#[tokio::test]
async fn update_email_verified_alone_marks_the_current_address() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user(AU_ORG, "a@x.test");
    au.mock().inner.lock().unwrap().users.iter_mut().for_each(|u| u.verified = false);
    let (status, body) = au.call(Method::PATCH, &format!("/users/{id}"), Some(json!({"email_verified": true}))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!((body["email"].as_str(), body["email_verified"].as_bool()), (Some("a@x.test"), Some(true)));
    assert!(au.mock().user_by_id(&id.to_string()).unwrap().verified);
    let calls = au.mock().calls();
    assert!(calls.contains(&format!("PATCH /v2/users/{id}")), "{calls:?}");
    assert!(!calls.contains(&format!("POST /v2/users/{id}/email")), "{calls:?}");
    assert_eq!(au.syncs().len(), 1);
    // Again, and with the same address spelled out: nothing to change, still 200.
    for patch in [json!({"email_verified": true}), json!({"email": "a@x.test", "email_verified": true})] {
        let (status, body) = au.call(Method::PATCH, &format!("/users/{id}"), Some(patch.clone())).await;
        assert_eq!(status, 200, "{patch}: {body}");
        assert_eq!(body["email_verified"], true);
    }
    // Zitadel can't take the verification back without a new address: 422, nothing changes.
    let (status, body) = au.call(Method::PATCH, &format!("/users/{id}"), Some(json!({"email_verified": false}))).await;
    assert_eq!((status, code_of(&body)), (422, "invalid_request"), "{body}");
    assert!(au.mock().user_by_id(&id.to_string()).unwrap().verified);
    // An unverified user asked to stay unverified: nothing to do, 200.
    let other = au.add_user(AU_ORG, "b@x.test");
    au.mock().inner.lock().unwrap().users.iter_mut().filter(|u| u.id == other.to_string()).for_each(|u| u.verified = false);
    let (status, body) = au.call(Method::PATCH, &format!("/users/{other}"), Some(json!({"email_verified": false}))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["email_verified"], false);
}

/// N2: an address two users of the org already share (made outside Chelabase's checks) is taken.
#[tokio::test]
async fn update_to_an_ambiguous_email_is_taken() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.seal_sync_role();
    let a = au.add_user(AU_ORG, "a@x.test");
    au.add_user(AU_ORG, "c@x.test");
    au.add_user(AU_ORG, "C@x.test");
    let (status, body) = au.call(Method::PATCH, &format!("/users/{a}"), Some(json!({"email": "c@x.test"}))).await;
    assert_eq!((status, code_of(&body)), (409, "email_taken"), "{body}");
    let (status, body) = au.call(Method::POST, "/users", Some(json!({"email": "c@x.test", "password": AU_PASSWORD}))).await;
    assert_eq!((status, code_of(&body)), (409, "email_taken"), "{body}");
}

/// Zitadel's precondition refusals (already in that state) are 409s, never 503.
#[tokio::test]
async fn state_changes_already_done_are_409() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user(AU_ORG, "a@x.test");
    let (status, body) = au.call(Method::POST, &format!("/users/{id}/reactivate"), None).await;
    assert_eq!((status, code_of(&body)), (409, "conflict"), "{body}");
    let (status, body) = au.call(Method::POST, &format!("/users/{id}/deactivate"), None).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = au.call(Method::POST, &format!("/users/{id}/deactivate"), None).await;
    assert_eq!((status, code_of(&body)), (409, "conflict"), "{body}");
}

#[tokio::test]
async fn update_email_taken_409() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let a = au.add_user(AU_ORG, "a@x.test");
    au.add_user(AU_ORG, "b@x.test");
    let (status, body) = au.call(Method::PATCH, &format!("/users/{a}"), Some(json!({"email": "B@x.test"}))).await;
    assert_eq!((status, code_of(&body)), (409, "email_taken"), "{body}");
    assert_eq!(au.mock().user_by_id(&a.to_string()).unwrap().email, "a@x.test");
    assert!(au.syncs().is_empty());
    // The user's own address isn't "taken".
    let (status, _) = au.call(Method::PATCH, &format!("/users/{a}"), Some(json!({"email": "a@x.test"}))).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn deactivate_revokes_and_syncs() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user(AU_ORG, "a@x.test");
    au.add_session(id, "s1");
    let (status, body) = au.call(Method::POST, &format!("/users/{id}/deactivate"), None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["state"], "inactive");
    assert!(au.mock().user_by_id(&id.to_string()).unwrap().inactive);
    assert_eq!(au.live_sessions(id), 0);
    assert!(au.mock().calls().contains(&"DELETE /v2/sessions/s1".to_string()));
    assert_eq!(au.syncs().len(), 1);
    let (status, body) = au.call(Method::POST, &format!("/users/{id}/reactivate"), None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["state"], "active");
    assert!(!au.mock().user_by_id(&id.to_string()).unwrap().inactive);
    assert_eq!(au.syncs().len(), 2);
}

#[tokio::test]
async fn revoke_sessions_is_204() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user(AU_ORG, "a@x.test");
    au.add_session(id, "s1");
    let (status, _) = au.call(Method::DELETE, &format!("/users/{id}/sessions"), None).await;
    assert_eq!(status, 204);
    assert_eq!(au.live_sessions(id), 0);
    assert!(au.mock().calls().contains(&"DELETE /v2/sessions/s1".to_string()));
}

#[tokio::test]
async fn delete_user_syncs() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user(AU_ORG, "a@x.test");
    au.add_session(id, "s1");
    let (status, _) = au.call(Method::DELETE, &format!("/users/{id}"), None).await;
    assert_eq!(status, 204);
    assert!(au.mock().user_by_id(&id.to_string()).is_none());
    assert_eq!(au.live_sessions(id), 0);
    assert_eq!(au.syncs().len(), 1);
    assert_eq!((au.syncs()[0].user_id, au.syncs()[0].data.is_none()), (id, true));
}

#[tokio::test]
async fn other_org_user_404() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user("o2", "stranger@x.test");
    for (method, rest, body) in [
        (Method::GET, format!("/users/{id}"), None),
        (Method::PATCH, format!("/users/{id}"), Some(json!({"password": "brand-new-pw-1"}))),
        (Method::POST, format!("/users/{id}/deactivate"), None),
        (Method::POST, format!("/users/{id}/reactivate"), None),
        (Method::GET, format!("/users/{id}/sessions"), None),
        (Method::DELETE, format!("/users/{id}/sessions"), None),
        (Method::DELETE, format!("/users/{id}"), None),
    ] {
        let (status, answer) = au.call(method.clone(), &rest, body).await;
        assert_eq!(status, 404, "{method} {rest}: {answer}");
    }
    let user = au.mock().user_by_id(&id.to_string()).unwrap();
    assert_eq!((user.password.as_str(), user.inactive), (AU_PASSWORD, false));
    assert!(au.syncs().is_empty());
}

#[tokio::test]
async fn user_routes_need_enabled_app_users() {
    let au = au_env(true).await;
    let (status, body) = au.call(Method::GET, "/users", None).await;
    assert_eq!((status, code_of(&body)), (409, "app_users_disabled"), "{body}");
    au.set_auth(AppUsersStatus::Enabling);
    let (status, body) = au.call(Method::POST, "/users", Some(json!({"email": "a@x.test", "password": AU_PASSWORD}))).await;
    assert_eq!((status, code_of(&body)), (409, "app_users_disabled"), "{body}");
}

#[tokio::test]
async fn routes_503_without_zitadel() {
    let au = au_env(false).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = uuid::Uuid::new_v4();
    for (method, rest, body) in [
        (Method::GET, "/users".to_string(), None),
        (Method::POST, "/users".to_string(), Some(json!({"email": "a@x.test", "password": AU_PASSWORD}))),
        (Method::GET, format!("/users/{id}"), None),
        (Method::PATCH, format!("/users/{id}"), Some(json!({"email_verified": true}))),
        (Method::DELETE, format!("/users/{id}"), None),
        (Method::DELETE, "/app-users".to_string(), None),
    ] {
        let (status, answer) = au.call(method.clone(), &rest, body).await;
        assert_eq!((status, code_of(&answer)), (503, "unavailable"), "{method} {rest}: {answer}");
    }
    // Reading the settings needs no Zitadel.
    let (status, _) = au.call(Method::GET, "/app-users", None).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn data_api_disable_refused_on_auth_database() {
    let au = au_env(false).await;
    au.metadata.add_branch_db(au.main, "web2", "owner", DatabaseOrigin::Sql);
    au.metadata.set_data_api(au.main, "web2", chelabase_metadata::DataApiState::On);
    au.set_auth(AppUsersStatus::Enabled);
    au.metadata.project_auths.lock().unwrap().get_mut(&au.project.id).unwrap().pg_database = "web2".into();
    let base = format!("/v1/databases/{}/branches/{}/pg-databases/web2/data-api", au.db, au.main);
    for query in ["", "?remove=true"] {
        let resp = send(&au.app, Method::DELETE, &format!("{base}{query}"), "org-1", None).await;
        let body = error_of(resp, 409).await;
        assert_eq!(code_of(&body), "conflict", "{body}");
        assert_eq!(message_of(&body), "app users use this database: disable app users first");
    }
    assert!(au.jobs.data_api_calls.lock().unwrap().is_empty());
    // Another Postgres database of the same Chelabase database is free to go.
    au.metadata.add_branch_db(au.main, "web3", "owner", DatabaseOrigin::Sql);
    let resp = send(&au.app, Method::DELETE, &format!("/v1/databases/{}/branches/{}/pg-databases/web3/data-api", au.db, au.main), "org-1", None).await;
    assert_eq!(resp.status(), 204);
}

#[tokio::test]
async fn list_projects_batches_the_auth_read() {
    let (app, metadata, tenant) = projects_app(FakeJobs::default()).await;
    let mut ids = vec![];
    for name in ["A", "B", "C"] {
        ids.push(metadata.create_project(tenant, name).await.unwrap().id);
    }
    metadata.project_auths.lock().unwrap().insert(
        ids[1],
        ProjectAuth {
            project_id: ids[1],
            status: AppUsersStatus::Enabled,
            error: None,
            zitadel_org_id: None,
            database_id: DatabaseId::new(),
            pg_database: "web1".into(),
            schema_name: "public".into(),
            table_name: "users".into(),
            email_verification: false,
            on_delete: OnDelete::Delete,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        },
    );
    let body: Value = send(&app, Method::GET, "/v1/projects", "org-1", None).await.json().await.unwrap();
    let states: Vec<_> = body.as_array().unwrap().iter().map(|p| p["app_users"].as_str()).collect();
    assert_eq!(states, vec![None, Some("enabled"), None]);
    assert_eq!(metadata.project_auth_reads.load(Ordering::SeqCst), 0);
    assert_eq!(metadata.project_auth_batch_reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn disable_retry_while_disabling_skips_the_users_check() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Disabling);
    // The org still lists a user (or is gone): the retry must not look.
    au.add_user(AU_ORG, "a@x.test");
    let (status, body) = au.call(Method::DELETE, "/app-users", None).await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(au.jobs_published(), vec![AppUsersJob::Disable { project_id: au.project.id }]);
    assert!(au.mock().calls().is_empty(), "{:?}", au.mock().calls());
}

#[tokio::test]
async fn put_while_disabling_409() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Disabling);
    let (status, body) = au.call(Method::PUT, "/app-users", Some(put_body(&au))).await;
    assert_eq!((status, code_of(&body)), (409, "conflict"), "{body}");
    assert!(au.jobs_published().is_empty());
    assert_eq!(au.auth().status, AppUsersStatus::Disabling);
}

#[tokio::test]
async fn delete_user_succeeds_and_syncs_when_the_revoke_fails() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user(AU_ORG, "a@x.test");
    *au.metadata.fail_revoke.lock().unwrap() = true;
    let (status, _) = au.call(Method::DELETE, &format!("/users/{id}"), None).await;
    assert_eq!(status, 204);
    assert!(au.mock().user_by_id(&id.to_string()).is_none());
    assert_eq!(au.syncs().len(), 1);
}

#[tokio::test]
async fn update_with_a_weak_password_changes_nothing() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user(AU_ORG, "a@x.test");
    let (status, body) =
        au.call(Method::PATCH, &format!("/users/{id}"), Some(json!({"email": "new@x.test", "password": "short"}))).await;
    assert_eq!((status, code_of(&body)), (422, "weak_password"), "{body}");
    let user = au.mock().user_by_id(&id.to_string()).unwrap();
    assert_eq!((user.email.as_str(), user.password.as_str()), ("a@x.test", AU_PASSWORD));
    assert!(au.syncs().is_empty());
}

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
        let mut id = identity("org-1");
        id.sub = sub.into();
        let token = self.app.signer.sign(Service::Orchestrator.name(), &id).unwrap();
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
        self.metadata.members.lock().unwrap().iter().find(|m| m.zitadel_user_id == sub).expect("a member row").id
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
    for rest in ["tables/public/notes_v", "tables/public/nope"] {
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
    assert_eq!(w.metadata.members.lock().unwrap().iter().filter(|m| m.zitadel_user_id == "bob").count(), 1);

    // A removed tenant admin is refused too, on every project.
    w.metadata.set_member(w.tenant, DEFAULT_SUB, |m| m.status = MemberStatus::Removed);
    let err = error_of(w.app.send_as(DEFAULT_SUB, "org-1", Method::GET, &path, None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    let err = error_of(w.app.send_as(DEFAULT_SUB, "org-1", Method::POST, "/v1/projects", Some(json!({"name": "x"}))).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
}

#[tokio::test]
async fn invited_member_activates_on_first_request() {
    let w = member_world(Mode::SelfHosted).await;
    let id = w.metadata.add_member(w.tenant, "carol", MemberStatus::Invited, false);
    w.metadata.add_open_invite(id);
    w.metadata.grant(id, w.project, PlatformRole::Viewer, &[]);
    let path = format!("/v1/databases/{}", w.db);
    assert_eq!(w.app.send_as("carol", "org-1", Method::GET, &path, None).await.status(), 200);
    let m = w.metadata.member(w.tenant, "carol").unwrap();
    assert_eq!(m.status, MemberStatus::Active);
    assert!(m.accepted_at.is_some());
    assert_eq!(m.email.as_deref(), Some("carol@example.test"), "the token's claims are stored");
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
            "databases": [{ "database_id": w.db, "platform_role": "developer", "roles": ["app"] }],
        }]),
        "{me}"
    );
    // A role-only override keeps the grant's role.
    w.metadata.narrow(m, w.db, OverrideRow { platform_role: None, pg_roles: Some(vec!["reports".into()]) });
    let me: Value = w.app.send_as("nat", "org-1", Method::GET, "/v1/me", None).await.json().await.unwrap();
    assert_eq!(me["projects"][0]["databases"], json!([{ "database_id": w.db, "platform_role": "admin", "roles": ["owner", "reports"] }]));
    // No override: an empty list. A tenant admin: always empty.
    w.metadata.add_granted(w.tenant, "pia", w.project, PlatformRole::Viewer, &[]);
    let me: Value = w.app.send_as("pia", "org-1", Method::GET, "/v1/me", None).await.json().await.unwrap();
    assert_eq!(me["projects"][0]["databases"], json!([]));
    let me: Value = w.app.send_as(DEFAULT_SUB, "org-1", Method::GET, "/v1/me", None).await.json().await.unwrap();
    assert!(me["projects"].as_array().unwrap().iter().all(|p| p["databases"] == json!([]) && p["roles"] == "all"), "{me}");
}

// R39: sign-up is off unless the tenant switched it on: a person in the org with no invite and
// no row is refused (the "ask your admin for an invite" answer) and nothing is written.
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
    // /v1/me still answers, with no membership.
    let me: Value = w.app.send_as("hal", "org-1", Method::GET, "/v1/me", None).await.json().await.unwrap();
    assert!(me["member"].is_null(), "{me}");
    assert_eq!(me["projects"], json!([]));
}

// M7 (R41): on a fresh install two people may sign up before either has a member row (the gate
// counts members). The one whose first request comes second is not stranded: while the tenant's
// admin claim is recent they are a pending request (shown in Requests). Anyone later is refused.
#[tokio::test]
async fn first_member_race_loser_is_a_pending_request() {
    let w = member_world(Mode::SelfHosted).await;
    w.metadata.set_admin_claim(w.tenant, Some(chrono::Utc::now() - chrono::Duration::minutes(5)));
    let me: Value = w.app.send_as("late", "org-1", Method::GET, "/v1/me", None).await.json().await.unwrap();
    let late = w.metadata.member(w.tenant, "late").expect("a member row");
    assert_eq!((late.status, late.tenant_admin), (MemberStatus::Pending, false));
    assert_eq!(me["member"]["status"], "pending", "{me}");
    let events: Vec<String> = w.metadata.events.lock().unwrap().iter().map(|e| e.action.clone()).collect();
    assert_eq!(events, vec!["member_joined".to_string()]);
    // The admin sees the request.
    let list: Value = w.app.send_as(DEFAULT_SUB, "org-1", Method::GET, "/v1/members", None).await.json().await.unwrap();
    assert!(list.as_array().unwrap().iter().any(|m| m["status"] == "pending"), "{list}");

    // Once the claim is old, a person with no row is refused again, and nothing is written.
    w.metadata.set_admin_claim(w.tenant, Some(chrono::Utc::now() - chrono::Duration::hours(3)));
    let before = w.metadata.members.lock().unwrap().len();
    let me: Value = w.app.send_as("hal", "org-1", Method::GET, "/v1/me", None).await.json().await.unwrap();
    assert!(me["member"].is_null(), "{me}");
    assert_eq!(w.metadata.members.lock().unwrap().len(), before);
    // And a tenant whose admin was not claimed through sign-up never admits one.
    w.metadata.set_admin_claim(w.tenant, None);
    let me: Value = w.app.send_as("ivy", "org-1", Method::GET, "/v1/me", None).await.json().await.unwrap();
    assert!(me["member"].is_null(), "{me}");
}

// R39: with open sign-up on, the same person becomes a pending member (one `member_joined`
// event) who can do nothing yet.
#[tokio::test]
async fn open_signup_creates_pending_member() {
    let w = member_world(Mode::SelfHosted).await;
    w.metadata.open_signups.lock().unwrap().insert(w.tenant, true);
    let me: Value = w.app.send_as("hal", "org-1", Method::GET, "/v1/me", None).await.json().await.unwrap();
    let hal = w.metadata.member(w.tenant, "hal").unwrap().id;
    assert_eq!(me["member"], json!({ "id": hal, "status": "pending", "tenant_admin": false }));
    assert_eq!(me["projects"], json!([]));
    let m = w.metadata.member(w.tenant, "hal").expect("a member row");
    assert_eq!((m.status, m.tenant_admin), (MemberStatus::Pending, false), "the tenant has an admin already");
    let events: Vec<String> = w.metadata.events.lock().unwrap().iter().map(|e| e.action.clone()).collect();
    assert_eq!(events, vec!["member_joined"]);
    // Asking again changes nothing, and a pending member stays out.
    let path = format!("/v1/databases/{}", w.db);
    let err = error_of(w.app.send_as("hal", "org-1", Method::GET, &path, None).await, 403).await;
    assert_eq!(code_of(&err), "pending_approval");
    assert_eq!(w.metadata.members.lock().unwrap().iter().filter(|m| m.zitadel_user_id == "hal").count(), 1);
    assert_eq!(w.metadata.events.lock().unwrap().len(), 1, "no second event");
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

// R40: declining removes the pending member and deactivates its Zitadel user, as removal does.
#[tokio::test]
async fn decline_removes_and_deactivates() {
    let t = mw(Mode::SelfHosted).await;
    let zid = t.mock.add_user_in("org-1", "pat@example.test", "", false);
    let pat = t.w.metadata.add_member(t.w.tenant, &zid, MemberStatus::Pending, false);
    let path = format!("/v1/members/{pat}/decline");
    t.w.metadata.add_granted(t.w.tenant, "padmin", t.w.project, PlatformRole::Admin, &[]);
    let err = error_of(t.send("padmin", Method::POST, &path, None).await, 403).await;
    assert_eq!(err["error"]["required"], "tenant admin");
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &path, None).await.status(), 204);
    assert_eq!(t.w.metadata.member(t.w.tenant, &zid).unwrap().status, MemberStatus::Removed);
    assert!(t.mock.user_by_id(&zid).unwrap().inactive, "the Zitadel user is deactivated");
    assert!(t.mock.calls().contains(&"POST /v2/sessions/search".to_string()), "its sessions are ended");
    assert_eq!(t.actions(), vec!["member_declined"]);
    // A repeat retries the Zitadel steps and writes nothing more.
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &path, None).await.status(), 204);
    assert_eq!(t.actions(), vec!["member_declined"]);
    // An active member isn't declined; a stranger's id is 404.
    let active = t.w.metadata.add_granted(t.w.tenant, "act", t.w.project, PlatformRole::Viewer, &[]);
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, &format!("/v1/members/{active}/decline"), None).await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, &format!("/v1/members/{}/decline", uuid::Uuid::new_v4()), None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
}

// R39, R40: a pending member never becomes the first admin; whoever finds the tenant without
// one claims it.
#[tokio::test]
async fn pending_never_becomes_first_admin() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    metadata.members.lock().unwrap().retain(|m| m.zitadel_user_id != DEFAULT_SUB); // no admin yet
    metadata.open_signups.lock().unwrap().insert(tenant, true);
    let pat = metadata.add_member(tenant, "pat", MemberStatus::Pending, false);
    let app = spawn_app_full(metadata.clone(), FakeJobs::default(), "http://127.0.0.1:9", Mode::SelfHosted, 10).await;
    let me: Value = app.send_as("pat", "org-1", Method::GET, "/v1/me", None).await.json().await.unwrap();
    assert_eq!(me["member"], json!({ "id": pat, "status": "pending", "tenant_admin": false }));
    let m = metadata.member(tenant, "pat").unwrap();
    assert_eq!((m.id, m.status, m.tenant_admin), (pat, MemberStatus::Pending, false));
    let me: Value = app.send_as("first", "org-1", Method::GET, "/v1/me", None).await.json().await.unwrap();
    assert_eq!((&me["member"]["status"], &me["member"]["tenant_admin"]), (&json!("active"), &json!(true)));
    assert!(me["member"]["id"].as_str().is_some_and(|id| id.len() == 36), "{me}");
    assert_eq!(metadata.member(tenant, "pat").unwrap().status, MemberStatus::Pending);
    // Even with the claim made by someone else, pat stays pending on every later request.
    app.send_as("pat", "org-1", Method::GET, "/v1/me", None).await;
    assert!(!metadata.member(tenant, "pat").unwrap().tenant_admin);
}

#[tokio::test]
async fn platform_mode_creates_no_members() {
    let w = member_world(Mode::Platform).await;
    let before = w.metadata.members.lock().unwrap().len();
    let path = format!("/v1/databases/{}", w.db);
    let err = error_of(w.app.send_as("dave", "org-1", Method::GET, &path, None).await, 403).await;
    assert_eq!(code_of(&err), "not_available_in_platform_mode");
    let me: Value = w.app.send_as("dave", "org-1", Method::GET, "/v1/me", None).await.json().await.unwrap();
    assert!(me["member"].is_null(), "{me}");
    assert_eq!(me["projects"], json!([]));
    assert_eq!(w.metadata.members.lock().unwrap().len(), before);
    assert!(w.metadata.member(w.tenant, "dave").is_none());

    // An existing member is served; a tenant admin isn't an instance admin without the
    // operator org.
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
        json!([{ "project_id": w.project, "platform_role": "developer", "roles": ["billing", "sales"], "databases": [] }])
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
        json!({ "project_id": w.project, "platform_role": "admin", "roles": "all", "databases": [] }),
        json!({ "project_id": other, "platform_role": "admin", "roles": "all", "databases": [] }),
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

// I2 (R18): a token without email or name claims gets them from Zitadel, once.
#[tokio::test]
async fn missing_claims_are_read_from_zitadel() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    metadata.open_signups.lock().unwrap().insert(tenant, true);
    let mock = MockZitadel::start().await;
    let zed = mock.add_user("Zed@Example.Test", "correct-horse-9", true);
    mock.set_display_name(&zed, "Zed Zulu");
    let app = spawn_app_parts(
        metadata.clone(),
        FakeJobs::default(),
        "http://127.0.0.1:9",
        Mode::SelfHosted,
        10,
        Some(Zitadel::new(&mock.base, ADMIN_TOKEN)),
        Arc::new(chelabase_query::app_users::PgUsersDb::new()),
    )
    .await;
    let bare = |sub: &str| Identity { email: None, name: None, ..identity_of(sub, "org-1") };
    let me = body_of(app.send_identity(&bare(&zed), Method::GET, "/v1/me").await).await;
    assert_eq!(me["email"], "zed@example.test", "{me}");
    assert_eq!(me["name"], "Zed Zulu");
    let m = metadata.member(tenant, &zed).unwrap();
    assert_eq!((m.email.as_deref(), m.display_name.as_str()), (Some("zed@example.test"), "Zed Zulu"));
    // Known now: the next request doesn't ask Zitadel again.
    app.send_identity(&bare(&zed), Method::GET, "/v1/me").await;
    let lookups = mock.inner.lock().unwrap().calls.iter().filter(|c| c.contains(&format!("/v2/users/{zed}"))).count();
    assert_eq!(lookups, 1);
    // A Zitadel failure isn't fatal: the member is made without them (pending, as zed is).
    let me = body_of(app.send_identity(&bare("ghost"), Method::GET, "/v1/me").await).await;
    assert!(me["email"].is_null() && me["name"].is_null(), "{me}");
    assert_eq!(me["member"]["status"], "pending");
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
        let bare = Identity { email: None, name: None, ..identity_of(DEFAULT_SUB, "org-1") };
        let full = identity_of(DEFAULT_SUB, "org-1");
        for (label, identity) in [("claims", &full), ("no claims", &bare)] {
            // The first request may refresh the claims; count the steady state.
            w.app.send_identity(identity, Method::GET, &path).await;
            let before = w.metadata.member_reads.load(Ordering::SeqCst);
            assert_eq!(w.app.send_identity(identity, Method::GET, &path).await.status(), 200);
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
    let bare = Identity { email: None, name: None, ..identity_of(DEFAULT_SUB, "org-1") };
    let me = body_of(w.app.send_identity(&bare, Method::GET, "/v1/me").await).await;
    assert!(me["name"].is_null(), "{me}");
    assert!(me["email"].is_null(), "{me}");
}

// M7 (R19): `Instance` routes are checked before the tenant binding, so a platform operator
// whose org is no tenant reaches them; every other route still answers `tenant_not_bound`.
#[tokio::test]
async fn instance_routes_skip_tenant_binding_for_operators() {
    let spawn = |mode: Mode, metadata: FakeMetadata| async move {
        spawn_app_custom(
            metadata,
            FakeJobs::default(),
            "http://127.0.0.1:9",
            mode,
            10,
            None,
            Arc::new(chelabase_query::app_users::PgUsersDb::new()),
            Some("ops"),
        )
        .await
    };
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    metadata.add_member(tenant, "plain", MemberStatus::Active, false);
    let app = spawn(Mode::Platform, metadata.clone()).await;
    // The operator org isn't a tenant: the probe passes, any tenant route doesn't.
    let resp = app.send_as("op", "ops", Method::GET, INSTANCE_PROBE, None).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "op");
    let err = error_of(app.send_as("op", "ops", Method::GET, "/v1/projects", None).await, 403).await;
    assert_eq!(code_of(&err), "tenant_not_bound");
    // In platform mode a tenant admin isn't an instance admin, and another org is unbound.
    let err = error_of(app.send_as(DEFAULT_SUB, "org-1", Method::GET, INSTANCE_PROBE, None).await, 403).await;
    assert_eq!((code_of(&err), err["error"]["required"].as_str()), ("forbidden", Some("instance admin")));
    let err = error_of(app.send_as("x", "elsewhere", Method::GET, INSTANCE_PROBE, None).await, 403).await;
    assert_eq!(code_of(&err), "tenant_not_bound");

    // Self-hosted: the tenant admin passes, another member doesn't, the operator org is nothing.
    let app = spawn(Mode::SelfHosted, metadata).await;
    assert_eq!(app.send_as(DEFAULT_SUB, "org-1", Method::GET, INSTANCE_PROBE, None).await.status(), 200);
    let err = error_of(app.send_as("plain", "org-1", Method::GET, INSTANCE_PROBE, None).await, 403).await;
    assert_eq!(err["error"]["required"], "instance admin");
    let err = error_of(app.send_as("op", "ops", Method::GET, INSTANCE_PROBE, None).await, 403).await;
    assert_eq!(code_of(&err), "tenant_not_bound");
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
        zitadel_org_id: Some("zorg".into()),
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

    // Without an override: not refused (the Zitadel client is missing, which is another answer).
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
    let dan = w.metadata.members.lock().unwrap().iter().find(|m| m.zitadel_user_id == "dan").unwrap().id;
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
            zitadel_org_id: Some("zorg".into()),
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
/// main plus `dev`), with Zitadel's mock behind the orchestrator (admin PAT; org-1 exists in
/// it) and, in platform mode, `ops` as the operator org.
struct Mw {
    w: MemberWorld,
    mock: MockZitadel,
}

async fn mw(mode: Mode) -> Mw {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    let mock = MockZitadel::start().await;
    mock.add_org("org-1");
    let app = spawn_app_custom(
        metadata.clone(),
        jobs.clone(),
        "http://127.0.0.1:9",
        mode,
        10,
        Some(Zitadel::new(&mock.base, ADMIN_TOKEN)),
        Arc::new(chelabase_query::app_users::PgUsersDb::new()),
        Some("ops"),
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
    Mw { w, mock }
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
async fn invite_creates_zitadel_user_and_invited_member() {
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

    // Zitadel: a passwordless user in the tenant's org, and only the invite mail.
    let user = t.mock.user("new@example.com").expect("a Zitadel user");
    assert_eq!((user.org_id.as_str(), user.password.as_str()), ("org-1", ""));
    assert_eq!(t.mock.emails(), vec![(user.id.clone(), "invite".to_string())]);
    assert!(
        t.mock
            .invite_template(&user.id)
            .is_some_and(|u| u.starts_with("http://studio.test/accept-invite?user={{.UserID}}&code={{.Code}}&t=")),
        "{:?}",
        t.mock.invite_template(&user.id)
    );
    // Metadata: an invited member with its (inert) grant, an invite, one event.
    let id: uuid::Uuid = member["id"].as_str().unwrap().parse().unwrap();
    let stored = t.w.metadata.members.lock().unwrap().iter().find(|m| m.id == id).cloned().unwrap();
    assert_eq!((stored.zitadel_user_id.as_str(), stored.status), (user.id.as_str(), MemberStatus::Invited));
    assert_eq!(t.grant_of(id), Some(GrantRow { platform_role: PlatformRole::Developer, pg_roles: vec!["app".into()] }));
    assert_eq!(t.actions(), vec!["invite_created"]);
    // Everyone in the tenant sees the invited member and its open invite.
    t.w.metadata.add_granted(t.w.tenant, "viewer", t.w.project, PlatformRole::Viewer, &[]);
    let list = body_of(t.send("viewer", Method::GET, "/v1/members", None).await).await;
    let listed = list.as_array().unwrap().iter().find(|m| m["id"] == member["id"]).cloned().unwrap();
    assert_eq!(listed["invite"]["id"], invite_id.as_str());
}

// Review focus 3, R8 and R18: a member is matched by Zitadel user id first, then by a non-NULL
// email (trimmed, any case); no second Zitadel user is made.
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
    assert!(!t.mock.calls().iter().any(|c| c == "POST /v2/users/new"), "{:?}", t.mock.calls());
    assert_eq!(t.grant_of(bob).map(|g| g.platform_role), Some(PlatformRole::Viewer));
    assert_eq!(t.enqueued(), t.branches());

    // Id first: Zitadel says carol@x.com is user Z; the member with Z has no stored email (it
    // went stale) while another member still holds it.
    let z = t.mock.add_user_in("org-1", "carol@x.com", "Secret-pass1", true);
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
async fn invite_existing_org_user_gets_member_row() {
    let t = mw(Mode::SelfHosted).await;
    let z = t.mock.add_user_in("org-1", "dana@x.com", "Secret-pass1", true);
    t.mock.set_display_name(&z, "Dana D");
    let resp = t
        .send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "dana@x.com", "developer", &["app"])))
        .await;
    assert_eq!(resp.status(), 201);
    let body = body_of(resp).await;
    assert_eq!(body["outcome"], "added");
    let m = &body["member"];
    assert_eq!((m["status"].as_str(), m["display_name"].as_str()), (Some("active"), Some("Dana D")));
    assert!(m["invite"].is_null());
    let stored = t.w.metadata.member(t.w.tenant, &z).expect("a member row for the Zitadel user");
    assert_eq!(stored.status, MemberStatus::Active);
    assert_eq!(t.grant_of(stored.id).map(|g| g.pg_roles), Some(vec!["app".to_string()]));
    // No new Zitadel user and no mail.
    assert!(t.mock.emails().is_empty());
    assert_eq!(t.mock.users_with_email("dana@x.com"), 1);
    assert_eq!(t.actions(), vec!["member_added"]);
    assert_eq!(t.enqueued(), t.branches());
    // Inviting them again finds the member.
    let again = body_of(
        t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "dana@x.com", "viewer", &[])))
            .await,
    )
    .await;
    assert_eq!(again["outcome"], "existing_member");
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
async fn remove_member_deactivates_and_enqueues() {
    let t = mw(Mode::SelfHosted).await;
    let z = t.mock.add_user_in("org-1", "gil@x.com", "Secret-pass1", true);
    let gil = t.w.metadata.add_granted(t.w.tenant, &z, t.w.project, PlatformRole::Developer, &["app"]);
    let z_client = Zitadel::new(&t.mock.base, ADMIN_TOKEN);
    z_client.create_session(&z, "Secret-pass1").await.unwrap();
    let other = t.mock.add_user_in("org-1", "hal@x.com", "Secret-pass1", true);
    z_client.create_session(&other, "Secret-pass1").await.unwrap();
    assert_eq!(t.send(&z, Method::GET, "/v1/projects", None).await.status(), 200);

    let path = format!("/v1/members/{gil}");
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &path, None).await.status(), 204);
    assert!(t.mock.user_by_id(&z).unwrap().inactive);
    assert_eq!(t.mock.session_count(), 1, "only gil's session ended");
    assert_eq!(t.enqueued(), t.branches());
    assert_eq!(t.w.metadata.member(t.w.tenant, &z).unwrap().status, MemberStatus::Removed);
    assert_eq!(t.grant_of(gil), None);
    // Their still-valid token is refused at once.
    let err = error_of(t.send(&z, Method::GET, &format!("/v1/projects/{}", t.w.project), None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    // Removing again is a retry: Zitadel says already inactive, which is fine.
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

#[tokio::test]
async fn revoke_invite_deletes_zitadel_user() {
    let t = mw(Mode::SelfHosted).await;
    let body = body_of(
        t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "ivy@x.com", "viewer", &[]))).await,
    )
    .await;
    let invite = body["member"]["invite"]["id"].as_str().unwrap().to_string();
    let member: uuid::Uuid = body["member"]["id"].as_str().unwrap().parse().unwrap();
    let user = t.mock.user("ivy@x.com").unwrap();
    let path = format!("/v1/invites/{invite}");
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &path, None).await.status(), 204);
    assert!(t.mock.user_by_id(&user.id).is_none(), "the never-activated user is deleted");
    assert!(t.mock.calls().contains(&format!("DELETE /v2/users/{}", user.id)));
    assert_eq!(t.w.metadata.member(t.w.tenant, &user.id).unwrap().status, MemberStatus::Removed);
    assert_eq!(t.grant_of(member), None);
    // I4: revoking again (the member is removed) retries the Zitadel delete; it is gone: fine.
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &path, None).await.status(), 204);
    let err = error_of(t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/invites/{}", uuid::Uuid::new_v4()), None).await, 404).await;
    assert_eq!(message_of(&err), "invite not found");
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

// I2 (R29): a removed member comes back only as `invited`, with a fresh code (a password-reset
// one: they have a password); the Zitadel user stays deactivated, and the event names the inviter.
#[tokio::test]
async fn removed_member_is_reinvited_with_a_code() {
    let t = mw(Mode::SelfHosted).await;
    let z = t.mock.add_user_in("org-1", "rae@x.com", "Secret-pass1", true);
    let rae = t.w.metadata.add_member(t.w.tenant, &z, MemberStatus::Active, false);
    t.w.metadata.set_member(t.w.tenant, &z, |m| m.email = Some("rae@x.com".into()));
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/members/{rae}"), None).await.status(), 204);
    assert!(t.mock.user_by_id(&z).unwrap().inactive);
    t.w.metadata.add_granted(t.w.tenant, "padmin", t.w.project, PlatformRole::Admin, &[]);
    let padmin = t.w.metadata.member(t.w.tenant, "padmin").unwrap().id;
    let body = json!({ "email": "rae@x.com", "platform_role": "viewer", "pg_roles": [] });
    let resp = t.send("padmin", Method::POST, &format!("/v1/projects/{}/invites", t.w.project), Some(body)).await;
    assert_eq!(resp.status(), 201);
    let body = body_of(resp).await;
    assert_eq!((body["outcome"].as_str(), body["member"]["status"].as_str()), (Some("invited"), Some("invited")));
    assert_eq!(body["member"]["id"], rae.to_string(), "the same member row");
    // I1: the invite never reactivates the user; accept-invite does, once the code is verified.
    assert!(t.mock.user_by_id(&z).unwrap().inactive, "still deactivated until the code is accepted");
    assert!(!t.mock.calls().iter().any(|c| c.contains("reactivate")), "{:?}", t.mock.calls());
    // Zitadel makes no invite code for a user who has a password ("already initialized"): the
    // code is a password-reset one, mailed with the accept-invite link marked `reset=1`.
    assert!(t.mock.invite_code(&z).is_none());
    assert!(t.mock.emails().contains(&(z.clone(), "reset".to_string())));
    let template = t.mock.reset_template(&z).expect("a reset mail with a link");
    assert!(
        template.starts_with("http://studio.test/accept-invite?user={{.UserID}}&code={{.Code}}&t=")
            && template.ends_with("&reset=1"),
        "{template}"
    );
    let event = t.w.metadata.events.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        (event.action.as_str(), event.actor_member_id, event.project_id),
        ("invite_created", Some(padmin), Some(t.w.project.as_uuid()))
    );
    // Revoking it never deletes a user who once had a password: it is deactivated again.
    let invite = body["member"]["invite"]["id"].as_str().unwrap().to_string();
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/invites/{invite}"), None).await.status(), 204);
    assert!(t.mock.user_by_id(&z).unwrap().inactive);
}

// M4: when the code can't be sent the invite is revoked at once, and the log says why.
#[tokio::test]
async fn failed_invite_delivery_is_revoked_with_its_reason() {
    let t = mw(Mode::SelfHosted).await;
    t.mock.fail("/invite_code", 503, "Errors.Internal", 1);
    let resp = t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "fail@x.com", "viewer", &[]))).await;
    assert_eq!(resp.status(), 503);
    let events = t.w.metadata.events.lock().unwrap().clone();
    let revoked = events.iter().find(|e| e.action == "invite_revoked").expect("a compensating revoke");
    assert_eq!(revoked.after.as_ref().unwrap()["reason"], "delivery_failed");
}

// I2 (R29): a user of the org who never set a password, or isn't Active, is invited by mail: an
// invite code for one with no way to sign in, a password-reset code for one with a password
// (Zitadel makes no invite code for them, Initial state included).
#[tokio::test]
async fn org_user_without_password_gets_an_invite_code() {
    let t = mw(Mode::SelfHosted).await;
    let nopw = t.mock.add_user_in("org-1", "nopw@x.com", "", false);
    let initial = t.mock.add_user_in("org-1", "init@x.com", "Secret-pass1", true);
    t.mock.set_initial(&initial);
    for (email, z, reset) in [("nopw@x.com", &nopw, false), ("init@x.com", &initial, true)] {
        let resp = t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, email, "viewer", &[]))).await;
        assert_eq!(resp.status(), 201, "{email}");
        let body = body_of(resp).await;
        assert_eq!(body["outcome"], "invited", "{email}");
        assert_eq!(t.w.metadata.member(t.w.tenant, z).unwrap().status, MemberStatus::Invited);
        assert_eq!(t.mock.invite_code(z).is_some(), !reset, "{email}");
        assert_eq!(t.mock.reset_template(z).is_some(), reset, "{email}");
    }
    assert_eq!(t.mock.users_with_email("nopw@x.com"), 1, "no second user");
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

// I4: the metadata revoke comes first; a Zitadel failure is retried by revoking again.
#[tokio::test]
async fn revoke_retries_the_zitadel_delete() {
    let t = mw(Mode::SelfHosted).await;
    let body = body_of(
        t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "zed@x.com", "viewer", &[]))).await,
    )
    .await;
    let invite = body["member"]["invite"]["id"].as_str().unwrap().to_string();
    let user = t.mock.user("zed@x.com").unwrap();
    let path = format!("/v1/invites/{invite}");
    t.mock.fail("DELETE /v2/users/", 503, "Errors.Internal", 1);
    let err = error_of(t.send(DEFAULT_SUB, Method::DELETE, &path, None).await, 503).await;
    assert_eq!(code_of(&err), "unavailable");
    assert_eq!(t.w.metadata.member(t.w.tenant, &user.id).unwrap().status, MemberStatus::Removed, "revoked first");
    assert!(t.mock.user_by_id(&user.id).is_some());
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &path, None).await.status(), 204);
    assert!(t.mock.user_by_id(&user.id).is_none());
    // An old invite of a member invited again (same row, a newer open invite) is closed: 409.
    // Yan is invited, then removed as a member (the invite is revoked, the user deactivated but
    // kept), then invited again: R29 brings the same row back with a new code.
    let body = body_of(
        t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "yan@x.com", "viewer", &[]))).await,
    )
    .await;
    let old = body["member"]["invite"]["id"].as_str().unwrap().to_string();
    let yan = body["member"]["id"].as_str().unwrap().to_string();
    assert_eq!(t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/members/{yan}"), None).await.status(), 204);
    let again = body_of(
        t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "yan@x.com", "viewer", &[]))).await,
    )
    .await;
    assert_eq!((again["outcome"].as_str(), again["member"]["id"].as_str()), (Some("invited"), Some(yan.as_str())));
    let err = error_of(t.send(DEFAULT_SUB, Method::DELETE, &format!("/v1/invites/{old}"), None).await, 409).await;
    assert_eq!(code_of(&err), "invite_closed");
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
    let user = t.mock.user("jay@x.com").unwrap();
    let first = t.mock.invite_code(&user.id).unwrap();
    let resp = t.send(DEFAULT_SUB, Method::POST, &format!("/v1/invites/{invite}/resend"), None).await;
    assert_eq!(resp.status(), 200);
    let resent = body_of(resp).await;
    assert_eq!(resent["id"], invite.as_str());
    assert_ne!(t.mock.invite_code(&user.id).unwrap(), first);
    assert_eq!(t.mock.emails().len(), 2);
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
    assert!(t.mock.user("kai@x.com").is_none(), "a refused invite makes no Zitadel user");
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

/// The invite link's `t` parameter: the token of the template Zitadel was given.
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

// R32: the invite link carries a Chelabase token besides Zitadel's code; the Zitadel mail gets
// it in the URL template and only its SHA-256 is stored.
#[tokio::test]
async fn invite_url_template_carries_the_token() {
    let t = mw(Mode::SelfHosted).await;
    let body = body_of(
        t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "tok@x.com", "viewer", &[]))).await,
    )
    .await;
    let user = t.mock.user("tok@x.com").unwrap();
    let template = t.mock.invite_template(&user.id).unwrap();
    assert!(
        template.starts_with("http://studio.test/accept-invite?user={{.UserID}}&code={{.Code}}&t="),
        "{template}"
    );
    let token = token_of(&template);
    assert_eq!(token.len(), 43, "32 bytes, base64url without padding: {token}");
    assert!(token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'), "{token}");
    assert_eq!(stored_hash(&t, body["member"]["invite"]["id"].as_str().unwrap()), hash_of(&token));
    // The Zitadel user's own tokens are never reused: another invite, another token.
    t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "tok2@x.com", "viewer", &[]))).await;
    let other = token_of(&t.mock.invite_template(&t.mock.user("tok2@x.com").unwrap().id).unwrap());
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
    let user = t.mock.user("rot@x.com").unwrap();
    let first = token_of(&t.mock.invite_template(&user.id).unwrap());
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &format!("/v1/invites/{invite}/resend"), None).await.status(), 200);
    let second = token_of(&t.mock.invite_template(&user.id).unwrap());
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

// With `link`, Zitadel hands back the code instead of mailing it; the response carries the
// whole `/accept-invite` link once, and nothing keeps it.
#[tokio::test]
async fn link_invite_returns_the_url_once_and_sends_no_mail() {
    let t = mw(Mode::SelfHosted).await;
    let resp = t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(link_invite_body(&t.w, " Link@X.com "))).await;
    assert_eq!(resp.status(), 201);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let body = body_of(resp).await;
    assert_eq!(body["outcome"], "invited");
    let user = t.mock.user("link@x.com").expect("a Zitadel user");
    let code = t.mock.invite_code(&user.id).unwrap();
    let url = body["invite_url"].as_str().unwrap().to_string();
    let prefix = format!("http://studio.test/accept-invite?user={}&code={code}&t=", user.id);
    assert!(url.starts_with(&prefix), "{url}");
    let token = url[prefix.len()..].to_string();
    assert_eq!(token.len(), 43, "{token}");
    // No mail and no template: Zitadel sent nothing.
    assert!(t.mock.emails().is_empty());
    assert_eq!(t.mock.invite_template(&user.id), None);
    // Only the token's hash is stored, and the Zitadel code is the one in the link.
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
    assert_eq!(t.mock.emails().len(), 1);
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
    assert!(t.mock.user("bad@x.com").is_none());
}

#[tokio::test]
async fn link_resend_rotates_code_and_token() {
    let t = mw(Mode::SelfHosted).await;
    let body = body_of(
        t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(link_invite_body(&t.w, "relink@x.com"))).await,
    )
    .await;
    let invite = body["member"]["invite"]["id"].as_str().unwrap().to_string();
    let user = t.mock.user("relink@x.com").unwrap();
    let (first_url, first_code) = (body["invite_url"].as_str().unwrap().to_string(), t.mock.invite_code(&user.id).unwrap());
    let path = format!("/v1/invites/{invite}/resend");
    let resp = t.send(DEFAULT_SUB, Method::POST, &path, Some(json!({ "delivery": "link" }))).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let resent = body_of(resp).await;
    let second_code = t.mock.invite_code(&user.id).unwrap();
    let second_url = resent["invite_url"].as_str().unwrap().to_string();
    assert_ne!(first_code, second_code);
    assert_ne!(first_url, second_url);
    let prefix = format!("http://studio.test/accept-invite?user={}&code={second_code}&t=", user.id);
    assert!(second_url.starts_with(&prefix), "{second_url}");
    // Only the new link's token is stored; no mail went out.
    assert_eq!(stored_hash(&t, &invite), hash_of(&second_url[prefix.len()..]));
    assert!(t.mock.emails().is_empty());
    assert_eq!(resent["id"], invite.as_str());
    assert_eq!(t.actions(), vec!["invite_created", "invite_resent"]);
    // Without a body the resend is by email, as before: a mail and no link.
    let resp = t.send(DEFAULT_SUB, Method::POST, &path, None).await;
    assert_eq!(resp.status(), 200);
    assert!(body_of(resp).await.get("invite_url").is_none());
    assert_eq!(t.mock.emails().len(), 1);
    // An unknown delivery is refused.
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, &path, Some(json!({ "delivery": "pigeon" }))).await, 422).await;
    assert_eq!(code_of(&err), "invalid_request");
    // M3: a `null` body is the default too: by email.
    let mails = t.mock.emails().len();
    let resp = t.send(DEFAULT_SUB, Method::POST, &path, Some(Value::Null)).await;
    assert_eq!(resp.status(), 200);
    assert!(body_of(resp).await.get("invite_url").is_none());
    assert_eq!(t.mock.emails().len(), mails + 1);
}

// I1: a link carries only a code, so it is for a Zitadel user the invite makes or one that has
// never had a password; anyone else is 409 "send by email", before anything changes.
#[tokio::test]
async fn link_invite_is_for_users_without_a_password() {
    let t = mw(Mode::SelfHosted).await;
    // A removed member whose Zitadel user has a password: a link is refused, email works.
    let zid = t.mock.add_user_in("org-1", "back@x.com", "Correct-horse-9", true);
    t.w.metadata.add_member(t.w.tenant, &zid, MemberStatus::Removed, false);
    t.mock.set_inactive(&zid, true);
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(link_invite_body(&t.w, "back@x.com"))).await, 409).await;
    assert_eq!(code_of(&err), "link_not_allowed");
    // Whether email delivery works now, so Studio can offer it (no provider in this mock).
    assert_eq!(err["error"]["email_available"], json!(false), "{err}");
    let writes = |t: &Mw| {
        t.mock.calls().iter().filter(|c| c.contains("invite_code") || c.contains("reactivate") || c.contains("users/new")).count()
    };
    assert_eq!(writes(&t), 0, "nothing was changed in Zitadel");
    assert_eq!(t.w.metadata.member(t.w.tenant, &zid).unwrap().status, MemberStatus::Removed);
    assert!(t.actions().is_empty());
    let by_mail = t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "back@x.com", "viewer", &[]))).await;
    assert_eq!(by_mail.status(), 201);
    assert!(body_of(by_mail).await.get("invite_url").is_none());
    // Any other way to sign in counts as much as a password: a passkey, an OTP, an IdP link.
    for (i, method) in ["PASSKEY", "TOTP", "U2F", "OTP_EMAIL", "IDP"].into_iter().enumerate() {
        let email = format!("factor{i}@x.com");
        let zid = t.mock.add_user_in("org-1", &email, "", false);
        t.mock.add_auth_method(&zid, &format!("AUTHENTICATION_METHOD_TYPE_{method}"));
        let err = error_of(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(link_invite_body(&t.w, &email))).await, 409).await;
        assert_eq!(code_of(&err), "link_not_allowed", "{method}");
        assert!(t.w.metadata.member(t.w.tenant, &zid).is_none(), "{method}: nothing was made");
    }
    // An org user who never set a password: a link is fine.
    let fresh = t.mock.add_user_in("org-1", "fresh@x.com", "", false);
    let resp = t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(link_invite_body(&t.w, "fresh@x.com"))).await;
    assert_eq!(resp.status(), 201);
    assert!(body_of(resp).await["invite_url"].as_str().unwrap().contains(&format!("user={fresh}&code=")));
}

// I1 for resend: the invitee set a password since (outside the invite), so only email resends.
#[tokio::test]
async fn link_resend_refused_after_the_invitee_has_a_password() {
    let t = mw(Mode::SelfHosted).await;
    let body = body_of(
        t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(link_invite_body(&t.w, "late@x.com"))).await,
    )
    .await;
    let invite = body["member"]["invite"]["id"].as_str().unwrap().to_string();
    let user = t.mock.user("late@x.com").unwrap();
    let path = format!("/v1/invites/{invite}/resend");
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &path, Some(json!({ "delivery": "link" }))).await.status(), 200);
    let code = t.mock.invite_code(&user.id);
    // A password, or any other factor, since the invite was made.
    for after in ["password", "AUTHENTICATION_METHOD_TYPE_TOTP", "AUTHENTICATION_METHOD_TYPE_PASSKEY"] {
        if after == "password" {
            t.mock.set_password(&user.id, "Correct-horse-9");
        } else {
            t.mock.set_password(&user.id, "");
            t.mock.add_auth_method(&user.id, after);
        }
        let err = error_of(t.send(DEFAULT_SUB, Method::POST, &path, Some(json!({ "delivery": "link" }))).await, 409).await;
        assert_eq!(code_of(&err), "link_not_allowed", "{after}");
        assert_eq!(err["error"]["email_available"], json!(false), "{after}");
        assert_eq!(t.mock.invite_code(&user.id), code, "{after}: no new code was made");
    }
    assert_eq!(t.send(DEFAULT_SUB, Method::POST, &path, None).await.status(), 200);
    // By email: a password-reset code (Zitadel makes no invite code for them now).
    assert!(t.mock.reset_template(&user.id).is_some_and(|t| t.ends_with("&reset=1")));
    // With an active email provider, the refusal says so.
    t.mock.set_email_providers(&[("p1", true)]);
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, &path, Some(json!({ "delivery": "link" }))).await, 409).await;
    assert_eq!((code_of(&err), &err["error"]["email_available"]), ("link_not_allowed", &json!(true)), "{err}");
}

// Each invite 409 has its own code, so Studio never reads message text.
#[tokio::test]
async fn invite_conflicts_have_their_own_codes() {
    let t = mw(Mode::SelfHosted).await;
    // Two users of the organisation share the email.
    t.mock.allow_duplicate_emails();
    t.mock.add_user_in("org-1", "twin@x.com", "", false);
    t.mock.add_user_in("org-1", "twin@x.com", "", false);
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "twin@x.com", "viewer", &[]))).await, 409).await;
    assert_eq!(code_of(&err), "email_ambiguous", "{err}");
    // A user of another organisation already has it: Zitadel refuses to make a second one.
    let t = mw(Mode::SelfHosted).await;
    t.mock.add_org("org-2");
    t.mock.add_user_in("org-2", "elsewhere@x.com", "Correct-horse-9", true);
    let err = error_of(t.send(DEFAULT_SUB, Method::POST, "/v1/invites", Some(invite_body(&t.w, "elsewhere@x.com", "viewer", &[]))).await, 409).await;
    assert_eq!(code_of(&err), "email_in_use_elsewhere", "{err}");
    // A revoked invite can't be resent (revoking a replaced one: `revoke_retries_the_zitadel_delete`).
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
    // No provider at all, then only an inactive one.
    assert_eq!(ask(DEFAULT_SUB).await, json!({ "email_available": false, "reason": "no_email_provider" }));
    t.mock.set_email_providers(&[("p1", false)]);
    assert_eq!(ask(DEFAULT_SUB).await, json!({ "email_available": false, "reason": "no_email_provider" }));
    // An active one; any active member may ask (the answer decides the invite dialog).
    t.mock.set_email_providers(&[("p1", false), ("p2", true)]);
    t.w.metadata.add_granted(t.w.tenant, "viewer", t.w.project, PlatformRole::Viewer, &[]);
    assert_eq!(ask("viewer").await, json!({ "email_available": true, "reason": null }));
    // A Zitadel failure is a plain "no", with the reason.
    t.mock.set_down(true);
    assert_eq!(ask(DEFAULT_SUB).await, json!({ "email_available": false, "reason": "zitadel_unavailable" }));
    t.mock.set_down(false);
    // The server asked Zitadel's admin API, once per question.
    assert!(t.mock.calls().contains(&"POST /admin/v1/email/_search".to_string()));
    // Platform mode makes no members: a stranger is refused like on any tenant route.
    let platform = mw(Mode::Platform).await;
    let err = error_of(platform.send("stranger", Method::GET, "/v1/invites/delivery", None).await, 403).await;
    assert_eq!(code_of(&err), "not_available_in_platform_mode");
    // Without a Zitadel client the answer is "no", not an error.
    let w = member_world(Mode::SelfHosted).await;
    let resp = w.app.send_as(DEFAULT_SUB, "org-1", Method::GET, "/v1/invites/delivery", None).await;
    assert_eq!(
        body_of(resp).await,
        json!({ "email_available": false, "reason": "zitadel_not_configured" })
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
