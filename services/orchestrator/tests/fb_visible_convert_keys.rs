//! RT1 final fix round (coordinator ruling 1), the coder's edges: `/visible` takes
//! `convert_keys` beside a check, validated like `keys` (the cap, one width per list, strings
//! only), never with the convert mode; past validation the call reaches the compute (not
//! running here: 503). The same-session behaviour is the live suites'.
#![allow(dead_code, unused_imports)]

include!("common/fakes.rs");

use chelabase_auth::test_support::MockJwks;

const VISIBLE: &str = "/internal/realtime/visible";

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

struct World {
    app: TestApp,
    jwks: MockJwks,
    db: DatabaseId,
}

/// org-1's tenant: project P with database `web1` (data API on, owner `postgres`, its owner
/// password sealed).
async fn world() -> World {
    let jwks = MockJwks::start().await;
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let app = spawn_app_oidc(
        metadata.clone(),
        FakeJobs::default(),
        "http://127.0.0.1:9",
        Mode::SelfHosted,
        jwks.config(vec![]),
    )
    .await;
    let project = metadata.create_project(tenant, "P").await.unwrap().id;
    let mut db = sample_database(tenant);
    db.project_id = project;
    db.name = "web1".into();
    db.database_name = Some("web1".into());
    db.owner_role = Some("postgres".into());
    db.main.owner_password_enc = Some(seal_credential(
        &app.key,
        &db.main.id.to_string(),
        Purpose::Owner,
        "pw",
    ));
    let (db_id, main) = (db.database_id, db.main.id);
    metadata.insert_database(db);
    metadata.add_branch_db(main, "web1", "postgres", DatabaseOrigin::Main);
    for rows in metadata.branch_dbs.lock().unwrap().values_mut() {
        for row in rows.iter_mut() {
            row.data_api = chelabase_metadata::DataApiState::On;
        }
    }
    World {
        app,
        jwks,
        db: db_id,
    }
}

impl World {
    fn body(&self, keys: Option<Value>, convert_keys: Value) -> Value {
        let token = self.jwks.sign(json!({
            "sub": DEFAULT_SUB,
            "iss": self.jwks.issuer(),
            "exp": now() + 300,
            chelabase_auth::CLAIM_ORG_ID: "org-1",
        }));
        json!({
            "token": token,
            "database_id": self.db,
            "branch_id": BranchId::from(self.db),
            "pg_database": "web1",
            "rel_oid": 16_384,
            "xid": null,
            "keys": keys,
            "convert_keys": convert_keys,
        })
    }

    async fn post(&self, body: Value) -> u16 {
        reqwest::Client::new()
            .post(format!("{}{VISIBLE}", self.app.base))
            .bearer_auth(REALTIME_TOKEN)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }
}

#[tokio::test]
async fn convert_keys_beside_a_check_reach_the_compute() {
    let w = world().await;
    let thousand: Vec<Value> = (0..1000).map(|i| json!([i.to_string()])).collect();
    for (keys, convert) in [
        (Some(json!([["1"]])), json!([["1"], ["2"]])),
        (Some(json!([["1"]])), json!(thousand)),
        (None, json!([["1"]])),
        (Some(json!([["1"]])), json!([])),
        (Some(json!([["1"]])), Value::Null),
    ] {
        assert_eq!(
            w.post(w.body(keys, convert.clone())).await,
            503,
            "{convert}"
        );
    }
}

#[tokio::test]
async fn convert_keys_are_validated_like_keys() {
    let w = world().await;
    let over: Vec<Value> = (0..1001).map(|i| json!([i.to_string()])).collect();
    for convert in [
        json!(over),
        json!([["1", "a"], ["2"]]),
        json!([[]]),
        json!([[null]]),
        json!([[1]]),
        json!("1"),
    ] {
        assert_eq!(
            w.post(w.body(Some(json!([["1"]])), convert.clone())).await,
            400,
            "{convert}"
        );
    }
    // Never with the convert mode, which prints `keys` alone.
    let mut body = w.body(Some(json!([["1"]])), json!([["1"]]));
    body["convert"] = json!(true);
    assert_eq!(w.post(body).await, 400, "convert mode with convert_keys");
}
