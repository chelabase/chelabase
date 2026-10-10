//! The data API engine against real computes.
//!
//! Each test provisions its own database with the real worker (as the worker's own live
//! tests do: `handle_provision` straight on the metadata database and the Neon stack, so
//! no management token is needed), serves rest's router in-process on `127.0.0.1:0` and
//! talks to it with identity tokens it mints itself (a fresh key pair; the gateway is out
//! of the picture). Connection info comes from the orchestrator's internal listener, so
//! `just dev` must be up. Owner DDL runs through `admin_sql` after `SET ROLE postgres`,
//! which is what the owner's own connection would do. The database is removed at the end,
//! pass or fail.
//!
//! ```text
//! set -a; . .chelabase/zitadel/demo.env; eval "$(scripts/neon-images.sh)"; CHELABASE_COMPUTE_IMAGE="$COMPUTE_NODE_IMAGE"; set +a
//! CHELABASE_COMPUTE_IMAGE=$COMPUTE_NODE_IMAGE \
//!   cargo test -p rest --test rest_live -- --ignored --test-threads=1 --nocapture
//! ```

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chelabase_auth::internal::{
    AuthMethod, DataIdentity, IDENTITY_HEADER, Identity, InternalSigner, InternalVerifier,
    generate_keypair_pem,
};
use chelabase_metadata::{
    BranchRecord, BranchStatus, CreateBranchOutcome, DEMO_PROJECT_ID, DEMO_TENANT_ID,
    DataApiState, DatabaseStatus, EnableStart, MetadataClient, MetadataConfig, NewBranch,
    NewDataKey,
};
use chelabase_nats::{BranchJob, DataApiDatabaseRequest, PointInTime};
use chelabase_neon::{ComputeBackend, ComputeLocator, NeonId};
use chelabase_secrets::{Purpose, SecretKey, open_credential};
use chelabase_types::data_key::DataKeyKind;
use chelabase_types::{BranchId, DatabaseId};
use rest::app::{AppState, internal_router, router};
use rest::conn::InternalSource;
use rest::exec::Engine;
use serde_json::{Value, json};
use uuid::Uuid;
use worker::config::{RealWorker, WorkerConfig, build};
use worker::data_api::RoleCheck;

const OWNER: &str = "SET ROLE postgres;\n";

struct Live {
    worker: RealWorker,
    metadata: MetadataClient,
    db: DatabaseId,
    database_name: String,
    rest: SocketAddr,
    internal: SocketAddr,
    internal_token: String,
    signer: InternalSigner,
    http: reqwest::Client,
    key: SecretKey,
    /// The served rest's state (its engine's counters).
    state: Arc<AppState<InternalSource>>,
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_owned())
}

async fn setup() -> (RealWorker, MetadataClient) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rest=info,worker=warn".into()),
        )
        .with_test_writer()
        .try_init();
    let mut config =
        WorkerConfig::from_env().expect("worker config (is CHELABASE_COMPUTE_IMAGE set?)");
    if config.compute_jwt_dir.is_relative() {
        config.compute_jwt_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(&config.compute_jwt_dir);
    }
    let metadata = MetadataClient::connect(&MetadataConfig {
        database_url: env_or(
            "CHELABASE_METADATA_DATABASE_URL",
            "postgres://chelabase:chelabase@127.0.0.1:5433/chelabase_metadata",
        ),
    })
    .await
    .expect("metadata database");
    let key =
        SecretKey::from_env().expect("CHELABASE_SECRETS_KEY (source .chelabase/zitadel/demo.env)");
    let worker = build(&config, metadata.clone(), key).expect("worker");
    (worker, metadata)
}

async fn serve(app: axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// Best-effort removal of everything the database left: computes, tenant, rows.
async fn cleanup(worker: &RealWorker, metadata: &MetadataClient, id: DatabaseId) {
    for branch in metadata.list_branches(id).await.unwrap_or_default() {
        let Some(ep) = branch.endpoint_id else {
            continue;
        };
        let _ = worker.computes.delete(&ep).await;
        if let Ok(Some(endpoint)) = metadata.neon_endpoint(&ep).await
            && let Ok(tenant) = endpoint.tenant_id.parse::<NeonId>()
        {
            let _ = worker.storage.storcon.delete_tenant(&tenant).await;
        }
        let _ = metadata.delete_endpoint(&ep).await;
    }
    for table in ["branches", "databases"] {
        let column = if table == "branches" {
            "database_id"
        } else {
            "id"
        };
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE {column} = $1"
        )))
        .bind(id.as_uuid())
        .execute(metadata.pool())
        .await;
    }
}

/// Provisions a database, serves rest, runs `body`, and removes the database.
async fn live<F, Fut>(name: &str, body: F)
where
    F: FnOnce(Arc<Live>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    live_capped(name, rest::exec::DEFAULT_MAX_RESPONSE_BYTES, body).await;
}

/// [`live`] with rest's responses capped at `max_response_bytes`.
async fn live_capped<F, Fut>(name: &str, max_response_bytes: usize, body: F)
where
    F: FnOnce(Arc<Live>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (worker, metadata) = setup().await;
    let db = DatabaseId::new();
    let db_name = format!("rest-{name}-{}", &db.to_string()[..6]);
    metadata
        .insert_database_pending(db, DEMO_TENANT_ID.into(), DEMO_PROJECT_ID.into(), &db_name)
        .await
        .expect("insert pending database");
    let started = Instant::now();
    let provisioned = tokio::time::timeout(
        Duration::from_secs(120),
        worker.handle_provision(db, &db_name),
    )
    .await;
    println!("provision took {:?}", started.elapsed());
    let ready = matches!(provisioned, Ok(Ok(())));
    let record = metadata.get_database(db).await.unwrap().unwrap();
    if !ready || record.status != DatabaseStatus::Ready {
        cleanup(&worker, &metadata, db).await;
        panic!("provision failed: {provisioned:?} {:?}", record.error);
    }

    let (private, public) = generate_keypair_pem();
    let internal_token = env_or("CHELABASE_REST_INTERNAL_TOKEN", "chelabase-dev-rest-internal");
    let state = Arc::new(AppState {
        engine: Engine::new(
            InternalSource::new(
                &env_or("CHELABASE_INTERNAL_URL", "http://127.0.0.1:8083"),
                &internal_token,
            ),
            200,
        )
        .with_max_response_bytes(max_response_bytes),
        verifier: InternalVerifier::from_pem(public.as_bytes()).unwrap(),
        internal_token: internal_token.clone(),
    });
    let ctx = Arc::new(Live {
        worker,
        metadata: metadata.clone(),
        db,
        database_name: record.database_name.clone().expect("database name"),
        rest: serve(router(state.clone())).await,
        internal: serve(internal_router(state.clone())).await,
        state,
        internal_token,
        signer: InternalSigner::from_pem(private.as_bytes()).unwrap(),
        http: reqwest::Client::new(),
        key: SecretKey::from_env().expect("CHELABASE_SECRETS_KEY"),
    });
    let outcome = tokio::spawn(body(ctx.clone())).await;
    cleanup(&ctx.worker, &ctx.metadata, db).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

/// A response: status and body (raw text, and parsed).
struct Answer {
    status: u16,
    text: String,
    json: Value,
}

impl std::fmt::Debug for Answer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.status, self.text)
    }
}

impl Answer {
    #[track_caller]
    fn ok(&self) -> &Value {
        assert_eq!(self.status, 200, "{self:?}");
        &self.json
    }

    #[track_caller]
    fn error(&self, status: u16, code: &str) -> &Value {
        assert_eq!(self.status, status, "{self:?}");
        assert_eq!(self.json["error"]["code"], code, "{self:?}");
        &self.json["error"]
    }
}

impl Live {
    fn main_id(&self) -> BranchId {
        BranchId::from(self.db)
    }

    async fn branch(&self, id: BranchId) -> BranchRecord {
        self.metadata
            .get_branch(id)
            .await
            .unwrap()
            .expect("branch row")
    }

    /// SQL as cloud_admin on `branch`, in its main database (psql's text output).
    async fn sql_on(&self, branch: BranchId, sql: &str) -> String {
        self.sql_in(branch, &self.database_name, sql).await
    }

    /// SQL as cloud_admin on `branch`, in `database` (psql's text output).
    async fn sql_in(&self, branch: BranchId, database: &str, sql: &str) -> String {
        let b = self.branch(branch).await;
        let ep = b.endpoint_id.as_deref().expect("branch endpoint");
        self.worker
            .computes
            .admin_sql(ep, database, sql)
            .await
            .unwrap_or_else(|e| panic!("sql on {} in {database}: {e:#}\n{sql}", b.name))
    }

    /// SQL as the database owner on main, in `database`.
    async fn owner_in(&self, database: &str, sql: &str) -> String {
        self.sql_in(self.main_id(), database, &format!("{OWNER}{sql}"))
            .await
    }

    /// `CREATE DATABASE` on main in the owner's own session (so the compute forwards it, as
    /// any SQL client's would), then waits for the forwarded row in `branch_databases`.
    async fn create_database(&self, name: &str) {
        use sqlx::Executor;
        let mut owner = self.owner_session().await;
        owner
            .execute(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .await
            .unwrap_or_else(|e| panic!("CREATE DATABASE {name}: {e}"));
        self.wait_row(self.main_id(), name, true).await;
    }

    /// Waits (30 s at most) until `branch` has (`present`) or lacks a `branch_databases` row
    /// named `name`.
    async fn wait_row(&self, branch: BranchId, name: &str, present: bool) {
        let started = Instant::now();
        loop {
            let rows = self.metadata.branch_databases(branch).await.unwrap();
            if rows.iter().any(|d| d.name == name) == present {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the row {name} is still {} after 30 s: {rows:?}",
                if present { "missing" } else { "there" }
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// The data API state of `name` on `branch`.
    async fn data_api_state(&self, branch: BranchId, name: &str) -> DataApiState {
        self.metadata
            .branch_databases(branch)
            .await
            .unwrap()
            .into_iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("no row {name}"))
            .data_api
    }

    /// What the enable route and the worker's job do: `enabling`, the bootstrap, `on`. Then
    /// rest is told (the orchestrator would call the dev stack's rest, not this test's).
    async fn enable(&self, branch: BranchId, name: &str) {
        let start = self
            .metadata
            .begin_enable_data_api(branch, name, 10)
            .await
            .unwrap();
        assert_eq!(start, EnableStart::Started);
        self.worker
            .enable_data_api(&DataApiDatabaseRequest {
                branch_id: branch,
                name: name.into(),
                remove: false,
                grant_existing: false,
            })
            .await;
        let row = self
            .metadata
            .branch_databases(branch)
            .await
            .unwrap()
            .into_iter()
            .find(|d| d.name == name)
            .unwrap();
        assert_eq!(row.data_api, DataApiState::On, "{:?}", row.data_api_error);
        // No invalidate: rest finds a newly enabled database by fetching the branch's info
        // again once the cached one is old enough.
        tokio::time::sleep(rest::conn::INFO_REFRESH_AFTER + Duration::from_millis(100)).await;
    }

    /// The disable request (the machinery stays).
    async fn disable(&self, branch: BranchId, name: &str) {
        let reply = self
            .worker
            .disable_data_api(&DataApiDatabaseRequest {
                branch_id: branch,
                name: name.into(),
                remove: false,
                grant_existing: false,
            })
            .await;
        assert!(reply.ok, "{reply:?}");
    }

    /// rest's internal invalidate route.
    async fn invalidate(&self, branch: BranchId) {
        let resp = self
            .http
            .post(format!(
                "http://{}/internal/branches/{}/invalidate",
                self.internal,
                branch.as_uuid()
            ))
            .bearer_auth(&self.internal_token)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 204);
    }

    /// A direct connection to main's Postgres, as `user` (the published loopback port).
    async fn connect_as(
        &self,
        user: &str,
        password: &str,
    ) -> Result<sqlx::PgConnection, sqlx::Error> {
        use sqlx::Connection;
        let main = self.branch(self.main_id()).await;
        let ep = main.endpoint_id.as_deref().expect("endpoint");
        let addr = self
            .worker
            .computes
            .postgres_addr(ep)
            .await
            .unwrap()
            .expect("a published Postgres port");
        let options = sqlx::postgres::PgConnectOptions::new()
            .host(&addr.ip().to_string())
            .port(addr.port())
            .username(user)
            .password(password)
            .database(&self.database_name);
        sqlx::PgConnection::connect_with(&options).await
    }

    /// The owner's own session (session_user = the owner, not cloud_admin).
    async fn owner_session(&self) -> sqlx::PgConnection {
        let main = self.branch(self.main_id()).await;
        let password = open_credential(
            &self.key,
            &main.id.to_string(),
            Purpose::Owner,
            main.owner_password_enc.as_deref().expect("owner password"),
        )
        .unwrap();
        self.connect_as("postgres", &password)
            .await
            .expect("the owner connects")
    }

    /// SQL as the database owner on main.
    async fn owner(&self, sql: &str) -> String {
        self.sql_on(self.main_id(), &format!("{OWNER}{sql}")).await
    }

    fn token(&self, branch: BranchId, kind: DataKeyKind, role: &str) -> String {
        self.token_for(branch, kind, role, None)
    }

    /// A secret key's request on main bound to the role OID `oid`.
    async fn query_bound(&self, role: &str, oid: i64, body: Value) -> Answer {
        let key_id = Uuid::new_v4();
        let identity = Identity {
            sub: key_id.to_string(),
            org_id: String::new(),
            auth: AuthMethod::DataKey,
            request_id: format!("live-{key_id}"),
            data: Some(DataIdentity {
                branch_id: self.main_id().as_uuid(),
                key_id,
                kind: DataKeyKind::Secret,
                role: role.into(),
                claims: json!({}),
                databases: None,
                project_id: None,
                role_oid: Some(oid),
            }),
            email: None,
            name: None,
        };
        let resp = self
            .http
            .post(format!("http://{}/data/v1/query", self.rest))
            .header(IDENTITY_HEADER, self.signer.sign("rest", &identity).unwrap())
            .json(&body)
            .send()
            .await
            .unwrap();
        Self::answer(resp).await
    }

    /// A token whose key may reach only `databases` (`None`: every served database).
    fn token_for(
        &self,
        branch: BranchId,
        kind: DataKeyKind,
        role: &str,
        databases: Option<Vec<String>>,
    ) -> String {
        let key_id = Uuid::new_v4();
        let identity = Identity {
            sub: key_id.to_string(),
            org_id: String::new(),
            auth: AuthMethod::DataKey,
            request_id: format!("live-{key_id}"),
            data: Some(DataIdentity {
                branch_id: branch.as_uuid(),
                key_id,
                kind,
                role: role.into(),
                claims: json!({}),
                databases,
                project_id: None,
                role_oid: None,
            }),
            email: None,
            name: None,
        };
        self.signer.sign("rest", &identity).unwrap()
    }

    async fn answer(resp: reqwest::Response) -> Answer {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap();
        let json = serde_json::from_str(&text).unwrap_or(Value::Null);
        Answer { status, text, json }
    }

    async fn query_on(
        &self,
        branch: BranchId,
        kind: DataKeyKind,
        role: &str,
        body: Value,
    ) -> Answer {
        let resp = self
            .http
            .post(format!("http://{}/data/v1/query", self.rest))
            .header(IDENTITY_HEADER, self.token(branch, kind, role))
            .json(&body)
            .send()
            .await
            .unwrap();
        Self::answer(resp).await
    }

    /// As a key on `branch` with the allow-list `databases`.
    async fn query_listed(
        &self,
        branch: BranchId,
        kind: DataKeyKind,
        role: &str,
        databases: Option<Vec<String>>,
        body: Value,
    ) -> Answer {
        let resp = self
            .http
            .post(format!("http://{}/data/v1/query", self.rest))
            .header(IDENTITY_HEADER, self.token_for(branch, kind, role, databases))
            .json(&body)
            .send()
            .await
            .unwrap();
        Self::answer(resp).await
    }

    /// `GET /data/v1/schema` as `role` on main, for `database` (`None`: main).
    async fn schema_of(&self, role: &str, database: Option<&str>) -> Answer {
        let mut url = format!("http://{}/data/v1/schema", self.rest);
        if let Some(database) = database {
            url.push_str(&format!("?database={database}"));
        }
        let resp = self
            .http
            .get(url)
            .header(
                IDENTITY_HEADER,
                self.token(self.main_id(), DataKeyKind::Secret, role),
            )
            .send()
            .await
            .unwrap();
        Self::answer(resp).await
    }

    /// As the public key (`anon`) on main.
    async fn anon(&self, body: Value) -> Answer {
        self.query_on(self.main_id(), DataKeyKind::Public, "anon", body)
            .await
    }

    /// As a secret key with `authenticated` on main.
    async fn auth(&self, body: Value) -> Answer {
        self.query_on(self.main_id(), DataKeyKind::Secret, "authenticated", body)
            .await
    }

    /// As a secret key with `authenticated` on main, with the body sent as written.
    async fn auth_raw(&self, body: &str) -> Answer {
        let resp = self
            .http
            .post(format!("http://{}/data/v1/query", self.rest))
            .header(
                IDENTITY_HEADER,
                self.token(self.main_id(), DataKeyKind::Secret, "authenticated"),
            )
            .header("content-type", "application/json")
            .body(body.to_owned())
            .send()
            .await
            .unwrap();
        Self::answer(resp).await
    }

    async fn role_schema(&self, role: &str) -> Answer {
        let resp = self
            .http
            .get(format!("http://{}/data/v1/schema", self.rest))
            .header(
                IDENTITY_HEADER,
                self.token(self.main_id(), DataKeyKind::Secret, role),
            )
            .send()
            .await
            .unwrap();
        Self::answer(resp).await
    }

    async fn internal_schema(&self) -> Answer {
        let resp = self
            .http
            .get(format!(
                "http://{}/internal/schema/{}",
                self.internal,
                self.main_id().as_uuid()
            ))
            .bearer_auth(&self.internal_token)
            .send()
            .await
            .unwrap();
        Self::answer(resp).await
    }

    /// Ready → resetting, as the API does before a reset or restore job.
    async fn claim(&self, id: BranchId) {
        let moved = self
            .metadata
            .set_branch_status(id, &[BranchStatus::Ready], BranchStatus::Resetting)
            .await
            .unwrap();
        assert!(moved, "branch was not ready");
    }
}

fn keys(row: &Value) -> Vec<&str> {
    let mut k: Vec<&str> = row
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    k.sort_unstable();
    k
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn select_insert_update_delete_call_and_batch_round_trip() {
    live("crud", |l| async move {
        l.owner(
            "CREATE TABLE items (id bigint GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, \
               name text NOT NULL, qty int NOT NULL DEFAULT 0);
             CREATE FUNCTION add(a int, b int DEFAULT 1) RETURNS int LANGUAGE sql AS 'SELECT a + b';
             CREATE FUNCTION noop() RETURNS void LANGUAGE plpgsql AS 'BEGIN END';
             CREATE FUNCTION total_of(ids int[]) RETURNS bigint LANGUAGE sql \
               AS 'SELECT coalesce(sum(x), 0) FROM unnest(ids) x';
             CREATE FUNCTION names() RETURNS SETOF text LANGUAGE sql AS 'SELECT name FROM items ORDER BY id';",
        )
        .await;

        let inserted = l
            .auth(json!({"v":1,"op":"insert","into":"items","values":[{"name":"a"},{"name":"b"}],"returning":["id","name"]}))
            .await;
        assert_eq!(inserted.ok(), &json!([{"id":1,"name":"a"},{"id":2,"name":"b"}]));
        let one = l
            .auth(json!({"v":1,"op":"select","from":"items","select":["name",{"col":"qty","as":"n"}],"where":{"col":"name","eq":"a"}}))
            .await;
        assert_eq!(one.ok(), &json!([{"name":"a","n":0}]));
        let updated = l
            .auth(json!({"v":1,"op":"update","table":"items","set":{"qty":5},"where":{"col":"name","eq":"a"},"returning":["qty"]}))
            .await;
        assert_eq!(updated.ok(), &json!([{"qty":5}]));
        let deleted = l
            .auth(json!({"v":1,"op":"delete","from":"public.items","where":{"col":"name","eq":"b"},"returning":"*"}))
            .await;
        assert_eq!(deleted.ok(), &json!([{"id":2,"name":"b","qty":0}]));
        // Writes without `returning` answer [].
        let quiet = l
            .auth(json!({"v":1,"op":"update","table":"items","set":{"qty":6},"where":{"col":"id","eq":1}}))
            .await;
        assert_eq!(quiet.ok(), &json!([]));

        assert_eq!(l.auth(json!({"v":1,"op":"call","fn":"add","args":{"a":2}})).await.ok(), &json!(3));
        assert_eq!(l.auth(json!({"v":1,"op":"call","fn":"add","args":{"a":2,"b":5}})).await.ok(), &json!(7));
        // A void function answers JSON null.
        let void = l.auth(json!({"v":1,"op":"call","fn":"noop"})).await;
        assert_eq!(void.text, "null");
        // A STABLE void function still runs: its RAISE is the answer.
        l.owner(
            "CREATE FUNCTION check_it(n int) RETURNS void LANGUAGE plpgsql STABLE \
               AS 'BEGIN IF n < 0 THEN RAISE EXCEPTION ''n must not be negative''; END IF; END';",
        )
        .await;
        let raised = l
            .auth(json!({"v":1,"op":"call","fn":"check_it","args":{"n":-1}}))
            .await;
        assert_eq!(raised.error(400, "raised")["message"], "n must not be negative");
        let fine = l.auth(json!({"v":1,"op":"call","fn":"check_it","args":{"n":1}})).await;
        assert_eq!(fine.text, "null");
        // An array argument takes a JSON array.
        assert_eq!(
            l.auth(json!({"v":1,"op":"call","fn":"total_of","args":{"ids":[1,2,3]}})).await.ok(),
            &json!(6)
        );
        assert_eq!(l.auth(json!({"v":1,"op":"call","fn":"names"})).await.ok(), &json!(["a"]));

        let batch = l
            .auth(json!({"v":1,"op":"batch","ops":[
                {"op":"insert","into":"items","values":[{"name":"c"}]},
                {"op":"select","from":"items","select":["name"],"orderBy":[{"col":"id"}]},
                {"op":"call","fn":"noop"}]}))
            .await;
        assert_eq!(batch.ok(), &json!([[], [{"name":"a"},{"name":"c"}], null]));

        // `limit` clamps to max_rows (1000 by default) and `in: []` matches nothing.
        let none = l
            .auth(json!({"v":1,"op":"select","from":"items","where":{"col":"id","in":[]},"limit":5000}))
            .await;
        assert_eq!(none.ok(), &json!([]));
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn batch_failure_rolls_back_everything() {
    live("batch", |l| async move {
        l.owner("CREATE TABLE u (id int PRIMARY KEY);").await;
        let mut ops: Vec<Value> = (1..50)
            .map(|i| json!({"op":"insert","into":"u","values":[{"id":i}]}))
            .collect();
        ops.push(json!({"op":"insert","into":"u","values":[{"id":1}]}));
        assert_eq!(ops.len(), 50);
        let failed = l.auth(json!({"v":1,"op":"batch","ops":ops})).await;
        let error = failed.error(409, "conflict");
        assert_eq!(error["sqlstate"], "23505");
        assert_eq!(
            l.auth(json!({"v":1,"op":"select","from":"u"})).await.ok(),
            &json!([])
        );
        // 51 operations are refused before anything runs.
        let too_many: Vec<Value> = (0..51)
            .map(|i| json!({"op":"insert","into":"u","values":[{"id":i}]}))
            .collect();
        l.auth(json!({"v":1,"op":"batch","ops":too_many}))
            .await
            .error(400, "invalid_query");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn casts_for_enum_domain_uuid_timestamptz_numeric_arrays_jsonb() {
    live("casts", |l| async move {
        l.owner(
            "CREATE TYPE mood AS ENUM ('sad', 'happy');
             CREATE DOMAIN email AS text CHECK (VALUE LIKE '%@%');
             CREATE TABLE c (id uuid PRIMARY KEY, m mood, e email, at timestamptz, n numeric, \
               p numeric(5,2), tags text[], meta jsonb, nums int[]);",
        )
        .await;
        let id = Uuid::new_v4().to_string();
        let digits = "123456789012345678901234567890";
        let row = json!({"id":id,"m":"happy","e":"a@b.c","at":"2026-10-01T12:00:00Z","n":digits,
            "p":"1.25","tags":["x","y"],"meta":{"k":[1,2]},"nums":[1,2]});
        l.auth(json!({"v":1,"op":"insert","into":"c","values":[row]})).await.ok();

        for (col, filter) in [
            ("id", json!({"col":"id","eq":id})),
            ("m", json!({"col":"m","eq":"happy"})),
            ("m in", json!({"col":"m","in":["happy","sad"]})),
            ("e", json!({"col":"e","eq":"a@b.c"})),
            ("e like", json!({"col":"e","like":"%@b.c"})),
            ("at", json!({"col":"at","eq":"2026-10-01 14:00:00+02"})),
            ("at gte", json!({"col":"at","gte":"2026-10-01T00:00:00Z"})),
            ("n", json!({"col":"n","eq":digits})),
            ("p", json!({"col":"p","eq":1.25})),
            ("tags", json!({"col":"tags","eq":["x","y"]})),
            ("meta", json!({"col":"meta","eq":{"k":[1,2]}})),
            ("nums", json!({"col":"nums","eq":[1,2]})),
        ] {
            let found = l
                .auth(json!({"v":1,"op":"select","from":"c","select":["id"],"where":filter}))
                .await;
            assert_eq!(found.ok(), &json!([{"id":id}]), "{col}");
        }
        // 30 digits come back exactly: the bytes Postgres wrote, unparsed.
        let all = l.auth(json!({"v":1,"op":"select","from":"c","select":["n","at","tags","meta"]})).await;
        assert!(all.text.contains(&format!("\"n\":{digits}")), "{all:?}");
        assert_eq!(all.json[0]["tags"], json!(["x","y"]));
        assert_eq!(all.json[0]["meta"], json!({"k":[1,2]}));

        // Bad values are the client's fault.
        l.auth(json!({"v":1,"op":"select","from":"c","where":{"col":"m","eq":"angry"}}))
            .await
            .error(400, "invalid_value");
        l.auth(json!({"v":1,"op":"insert","into":"c","values":[{"id":Uuid::new_v4().to_string(),"e":"nope"}]}))
            .await
            .error(400, "invalid_value");
        l.auth(json!({"v":1,"op":"select","from":"c","where":{"col":"id","eq":"not-a-uuid"}}))
            .await
            .error(400, "invalid_value");
        l.auth(json!({"v":1,"op":"select","from":"c","where":{"col":"n","like":"1%"}}))
            .await
            .error(400, "invalid_query");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn anon_reads_and_cannot_write() {
    live("anon", |l| async move {
        l.owner("CREATE TABLE t (v text);").await;
        l.auth(json!({"v":1,"op":"insert","into":"t","values":[{"v":"a"}]}))
            .await
            .ok();
        assert_eq!(
            l.anon(json!({"v":1,"op":"select","from":"t"})).await.ok(),
            &json!([{"v":"a"}])
        );
        let denied = l
            .anon(json!({"v":1,"op":"insert","into":"t","values":[{"v":"b"}]}))
            .await;
        assert_eq!(denied.error(403, "permission_denied")["sqlstate"], "42501");
        l.anon(json!({"v":1,"op":"delete","from":"t","all":true}))
            .await
            .error(403, "permission_denied");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn secret_key_custom_role() {
    live("role", |l| async move {
        l.owner(
            "CREATE ROLE reporter NOLOGIN;
             GRANT USAGE ON SCHEMA public TO reporter;
             CREATE TABLE r (v text);
             CREATE TABLE other (v text);
             INSERT INTO r VALUES ('seen');
             GRANT SELECT ON r TO reporter;",
        )
        .await;
        assert!(matches!(
            l.worker.check_data_role(l.main_id(), "reporter").await,
            RoleCheck::Ok(_, false)
        ));
        let reporter = |body| l.query_on(l.main_id(), DataKeyKind::Secret, "reporter", body);
        assert_eq!(
            reporter(json!({"v":1,"op":"select","from":"r"})).await.ok(),
            &json!([{"v":"seen"}])
        );
        reporter(json!({"v":1,"op":"select","from":"other"}))
            .await
            .error(404, "unknown_table");
        reporter(json!({"v":1,"op":"insert","into":"r","values":[{"v":"x"}]}))
            .await
            .error(403, "permission_denied");
        // The role's own view of the schema.
        let schema = l.role_schema("reporter").await;
        let tables = &schema.ok()["schemas"]["public"]["tables"];
        assert_eq!(keys(tables), ["r"]);
    })
    .await;
}

/// A secret key is bound to its role's OID: the role dropped and created again under the same
/// name (in SQL as cloud_admin, which the DDL receiver never sees: a late commit it couldn't
/// refuse) gets no request of the old key, on the sequential path or the pipeline, and the
/// key bound to the new OID works.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn secret_key_is_bound_to_its_roles_oid() {
    live("roleoid", |l| async move {
        l.owner(
            "CREATE TABLE r (v text);
             INSERT INTO r VALUES ('seen');
             CREATE ROLE bound_reader NOLOGIN;
             GRANT SELECT ON r TO bound_reader;",
        )
        .await;
        let RoleCheck::Ok(old, _) = l.worker.check_data_role(l.main_id(), "bound_reader").await else {
            panic!("the role check passes");
        };
        let select = json!({"v":1,"op":"select","from":"r"});
        for _ in 0..2 {
            assert_eq!(l.query_bound("bound_reader", old, select.clone()).await.ok(), &json!([{"v":"seen"}]));
        }
        l.sql_on(
            l.main_id(),
            "REVOKE ALL ON r FROM bound_reader; DROP ROLE bound_reader; CREATE ROLE bound_reader NOLOGIN; \
             GRANT SELECT ON r TO bound_reader;",
        )
        .await;
        let RoleCheck::Ok(new, _) = l.worker.check_data_role(l.main_id(), "bound_reader").await else {
            panic!("the recreated role passes the check");
        };
        assert_ne!(old, new, "a recreated role has another OID");
        for _ in 0..2 {
            l.query_bound("bound_reader", old, select.clone())
                .await
                .error(503, "unavailable");
        }
        assert_eq!(l.query_bound("bound_reader", new, select.clone()).await.ok(), &json!([{"v":"seen"}]));
    })
    .await;
}

/// E33: a secret key whose role is owner-level — CREATEDB, CREATEROLE, REPLICATION or
/// `neon_superuser` at any depth — is refused before rest switches to it. A plain role reads,
/// and so does a role with BYPASSRLS (it reads past row-level security; the worker's check
/// says so, for Studio's mark).
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn secret_key_of_an_owner_level_role_is_refused() {
    live("ownerlvl", |l| async move {
        l.owner(
            "CREATE TABLE r (v text);
             INSERT INTO r VALUES ('seen');
             ALTER TABLE r ENABLE ROW LEVEL SECURITY;
             CREATE POLICY nobody ON r USING (false);
             CREATE ROLE plain_reader NOLOGIN;
             CREATE ROLE by_bypassrls NOLOGIN BYPASSRLS;
             CREATE ROLE by_createdb NOLOGIN CREATEDB;
             CREATE ROLE by_createrole NOLOGIN CREATEROLE;
             CREATE ROLE by_group NOLOGIN;
             CREATE ROLE by_depth NOLOGIN;
             GRANT by_group TO by_depth;
             GRANT USAGE ON SCHEMA public TO plain_reader, by_bypassrls, by_depth;
             GRANT SELECT ON r TO plain_reader, by_bypassrls, by_depth;",
        )
        .await;
        // The owner can't grant neon_superuser; cloud_admin can.
        l.sql_on(l.main_id(), "GRANT neon_superuser TO by_group;")
            .await;
        assert!(matches!(
            l.worker.check_data_role(l.main_id(), "plain_reader").await,
            RoleCheck::Ok(_, false)
        ));
        assert!(matches!(
            l.worker.check_data_role(l.main_id(), "by_bypassrls").await,
            RoleCheck::Ok(_, true)
        ));
        let select = json!({"v":1,"op":"select","from":"r"});
        // The policy hides every row from a plain role; BYPASSRLS reads past it.
        assert_eq!(
            l.query_on(l.main_id(), DataKeyKind::Secret, "plain_reader", select.clone())
                .await
                .ok(),
            &json!([])
        );
        assert_eq!(
            l.query_on(l.main_id(), DataKeyKind::Secret, "by_bypassrls", select.clone())
                .await
                .ok(),
            &json!([{"v":"seen"}])
        );
        for role in ["by_createdb", "by_createrole", "by_depth"] {
            let answer = l
                .query_on(l.main_id(), DataKeyKind::Secret, role, select.clone())
                .await;
            let error = answer.error(403, "role_not_allowed");
            assert_eq!(
                error["message"],
                format!(
                    "the role `{role}` of this key is owner-level or has system privileges \
                     (superuser, CREATEROLE, CREATEDB, REPLICATION, or a member of neon_superuser \
                     or a system role); use a role without them (BYPASSRLS is allowed)"
                ),
                "{role}"
            );
        }
    })
    .await;
}

/// The internal schema route with `?role=` (the console's schema as a role): what that role
/// reaches, with its privileges; a missing role is 404 `unknown_role`, an owner-level one 403.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn internal_schema_for_a_console_role() {
    live("schemarole", |l| async move {
        l.owner(
            "CREATE SCHEMA hidden;
             CREATE TABLE hidden.t (id int PRIMARY KEY, v text);
             CREATE ROLE console_editor NOLOGIN;
             CREATE ROLE console_boss NOLOGIN CREATEDB;
             GRANT USAGE ON SCHEMA hidden TO console_editor;
             GRANT SELECT ON hidden.t TO console_editor;",
        )
        .await;
        let get = |query: String| {
            let l = &l;
            async move {
                let resp = l
                    .http
                    .get(format!(
                        "http://{}/internal/schema/{}{query}",
                        l.internal,
                        l.main_id().as_uuid()
                    ))
                    .bearer_auth(&l.internal_token)
                    .send()
                    .await
                    .unwrap();
                Live::answer(resp).await
            }
        };
        let answer = get(format!("?role=console_editor&database={}", l.database_name)).await;
        let doc = answer.ok();
        let t = &doc["schemas"]["hidden"]["tables"]["t"];
        assert_eq!(t["primary_key"], json!(["id"]), "{doc}");
        assert!(t["privileges"].get("console_editor").is_some(), "{doc}");
        assert!(t["privileges"].get("anon").is_none(), "{doc}");
        // anon and authenticated can't reach it.
        let default = l.internal_schema().await;
        assert!(default.ok()["schemas"].get("hidden").is_none());
        get("?role=console_ghost".into()).await.error(404, "unknown_role");
        get("?role=console_boss".into()).await.error(403, "role_not_allowed");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn rls_policy_is_enforced() {
    live("rls", |l| async move {
        l.owner(
            "CREATE TABLE p (v text, visible boolean);
             INSERT INTO p VALUES ('shown', true), ('hidden', false);
             ALTER TABLE p ENABLE ROW LEVEL SECURITY;
             CREATE POLICY see ON p FOR SELECT TO anon USING (visible);",
        )
        .await;
        assert_eq!(
            l.anon(json!({"v":1,"op":"select","from":"p","select":["v"]}))
                .await
                .ok(),
            &json!([{"v":"shown"}])
        );
        // RLS on and no policy for authenticated: no rows.
        assert_eq!(
            l.auth(json!({"v":1,"op":"select","from":"p"})).await.ok(),
            &json!([])
        );
        let internal = l.internal_schema().await;
        assert_eq!(
            internal.ok()["schemas"]["public"]["tables"]["p"]["rls"],
            true
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn unfiltered_update_is_refused() {
    live("unfiltered", |l| async move {
        l.owner("CREATE TABLE t (v text); INSERT INTO t VALUES ('a'), ('b');").await;
        l.auth(json!({"v":1,"op":"update","table":"t","set":{"v":"z"}}))
            .await
            .error(400, "invalid_query");
        l.auth(json!({"v":1,"op":"delete","from":"t"}))
            .await
            .error(400, "invalid_query");
        let all = l
            .auth(json!({"v":1,"op":"update","table":"t","set":{"v":"z"},"all":true,"returning":["v"]}))
            .await;
        assert_eq!(all.ok(), &json!([{"v":"z"},{"v":"z"}]));
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn new_column_is_visible_on_the_next_request() {
    live("ddl", |l| async move {
        l.owner("CREATE TABLE t (a int); INSERT INTO t VALUES (1);")
            .await;
        let before = l.auth(json!({"v":1,"op":"select","from":"t"})).await;
        assert_eq!(keys(&before.ok()[0]), ["a"]);
        l.owner("ALTER TABLE t ADD COLUMN b text DEFAULT 'x';")
            .await;
        let after = l.auth(json!({"v":1,"op":"select","from":"t"})).await;
        assert_eq!(after.ok(), &json!([{"a":1,"b":"x"}]));
        let by_b = l
            .auth(
                json!({"v":1,"op":"select","from":"t","select":["a"],"where":{"col":"b","eq":"x"}}),
            )
            .await;
        assert_eq!(by_b.ok(), &json!([{"a":1}]));
        // A new table too.
        l.owner("CREATE TABLE t2 (c int);").await;
        assert_eq!(
            l.anon(json!({"v":1,"op":"select","from":"t2"})).await.ok(),
            &json!([])
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn restore_rewinds_the_schema_and_the_old_shape_is_served() {
    live("restore", |l| async move {
        l.owner("CREATE TABLE h (a int); INSERT INTO h VALUES (1);")
            .await;
        let select_all = json!({"v":1,"op":"select","from":"h"});
        assert_eq!(l.auth(select_all.clone()).await.ok(), &json!([{"a":1}]));
        tokio::time::sleep(Duration::from_secs(2)).await;
        let noted = chrono::Utc::now();
        tokio::time::sleep(Duration::from_secs(2)).await;
        l.owner("ALTER TABLE h ADD COLUMN b int DEFAULT 2;").await;
        assert_eq!(
            l.auth(select_all.clone()).await.ok(),
            &json!([{"a":1,"b":2}])
        );
        let select_b = json!({"v":1,"op":"select","from":"h","select":["b"]});
        assert_eq!(l.auth(select_b.clone()).await.ok(), &json!([{"b":2}]));

        l.claim(l.main_id()).await;
        let started = Instant::now();
        l.worker
            .handle_branch_job(BranchJob::Restore {
                branch_id: l.main_id(),
                backup_id: BranchId::new(),
                backup_name: format!("backup-{}", &Uuid::new_v4().to_string()[..8]),
                at: PointInTime::Timestamp(noted),
            })
            .await
            .expect("restore job");
        println!("restore took {:?}", started.elapsed());
        let main = l.branch(l.main_id()).await;
        assert_eq!(main.status, BranchStatus::Ready, "{:?}", main.error);

        // The schema that's there now (a new compute, too): no `b`.
        assert_eq!(l.auth(select_all).await.ok(), &json!([{"a":1}]));
        l.auth(select_b).await.error(400, "unknown_column");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn dropped_chelabase_is_unavailable_then_healed_by_bootstrap() {
    live("heal", |l| async move {
        l.owner("CREATE TABLE t (v text);").await;
        let select = json!({"v":1,"op":"select","from":"t"});
        l.anon(select.clone()).await.ok();

        l.sql_on(l.main_id(), "DROP SCHEMA chelabase CASCADE;").await;
        let down = l.anon(select.clone()).await;
        let error = down.error(503, "unavailable");
        assert_eq!(
            error["message"],
            chelabase_query::error::schema_missing_in(&l.database_name)
        );
        let main = l.branch(l.main_id()).await;
        l.worker
            .bootstrap_data_api(&main, &l.database_name)
            .await
            .expect("bootstrap");
        l.anon(select.clone()).await.ok();

        // Revoked grants answer the same way, until the bootstrap runs again.
        l.sql_on(
            l.main_id(),
            "REVOKE USAGE ON SCHEMA chelabase FROM PUBLIC, authenticator;",
        )
        .await;
        l.anon(select.clone()).await.error(503, "unavailable");
        l.worker
            .bootstrap_data_api(&main, &l.database_name)
            .await
            .expect("bootstrap");
        l.anon(select).await.ok();
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn reset_moves_the_compute_and_the_engine_reconnects() {
    live("reset", |l| async move {
        l.owner("CREATE TABLE t (v text); INSERT INTO t VALUES ('A');")
            .await;
        let dev_id = BranchId::new();
        let outcome = l
            .metadata
            .create_branch(&NewBranch {
                id: dev_id,
                database_id: l.db,
                name: "dev",
                parent_branch_id: l.main_id(),
                branched_at_time: None,
                branched_at_lsn: None,
            })
            .await
            .unwrap();
        assert_eq!(outcome, CreateBranchOutcome::Created);
        l.worker
            .handle_branch_job(BranchJob::Create {
                branch_id: dev_id,
                at: None,
            })
            .await
            .expect("create job");
        let dev = l.branch(dev_id).await;
        assert_eq!(dev.status, BranchStatus::Ready, "{:?}", dev.error);
        let select = json!({"v":1,"op":"select","from":"t","orderBy":[{"col":"v"}]});
        let on_dev = |body: Value| l.query_on(dev_id, DataKeyKind::Secret, "authenticated", body);
        assert_eq!(on_dev(select.clone()).await.ok(), &json!([{"v":"A"}]));

        l.owner("INSERT INTO t VALUES ('B');").await;
        let old_endpoint = dev.endpoint_id.clone();
        l.claim(dev_id).await;
        let started = Instant::now();
        l.worker
            .handle_branch_job(BranchJob::ResetToParent { branch_id: dev_id })
            .await
            .expect("reset job");
        println!("reset took {:?}", started.elapsed());
        let dev = l.branch(dev_id).await;
        assert_eq!(dev.status, BranchStatus::Ready, "{:?}", dev.error);
        println!("endpoint {old_endpoint:?} → {:?}", dev.endpoint_id);

        let started = Instant::now();
        assert_eq!(on_dev(select).await.ok(), &json!([{"v":"A"},{"v":"B"}]));
        println!("first request after the reset took {:?}", started.elapsed());
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn unreachable_table_looks_missing() {
    live("hidden", |l| async move {
        l.owner(
            "CREATE TABLE hidden (v text);
             REVOKE ALL ON hidden FROM anon, authenticated;
             CREATE SCHEMA priv;
             CREATE TABLE priv.x (v text);
             GRANT SELECT ON priv.x TO anon;
             CREATE FUNCTION secret_fn() RETURNS int LANGUAGE sql AS 'SELECT 1';
             REVOKE EXECUTE ON FUNCTION secret_fn() FROM authenticated;",
        )
        .await;
        let hidden = l.anon(json!({"v":1,"op":"select","from":"hidden"})).await;
        let missing = l.anon(json!({"v":1,"op":"select","from":"nope"})).await;
        assert_eq!(
            hidden.error(404, "unknown_table")["message"],
            "unknown table public.hidden"
        );
        assert_eq!(
            missing.error(404, "unknown_table")["message"],
            "unknown table public.nope"
        );
        assert_eq!(
            keys(
                hidden.json["error"]
                    .as_object()
                    .map(|_| &hidden.json["error"])
                    .unwrap()
            ),
            keys(&missing.json["error"])
        );
        // A grant on a table in a schema without USAGE doesn't reach it either.
        l.anon(json!({"v":1,"op":"select","from":"priv.x"}))
            .await
            .error(404, "unknown_table");
        l.auth(json!({"v":1,"op":"call","fn":"secret_fn"}))
            .await
            .error(404, "unknown_function");
        // Nor is any of it in the role's schema.
        let schema = l.role_schema("anon").await;
        let schemas = &schema.ok()["schemas"];
        assert!(schemas.get("priv").is_none(), "{schema:?}");
        assert!(schemas["public"]["tables"].get("hidden").is_none());
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn hostile_names_live() {
    live("names", |l| async move {
        l.owner(r#"CREATE TABLE "Order" (id int PRIMARY KEY, "select" text, "we""ird; x" text);"#)
            .await;
        let inserted = l
            .auth(json!({"v":1,"op":"insert","into":"Order",
                "values":[{"id":1,"select":"s","we\"ird; x":"w"}],"returning":"*"}))
            .await;
        assert_eq!(inserted.ok(), &json!([{"id":1,"select":"s","we\"ird; x":"w"}]));
        let found = l
            .auth(json!({"v":1,"op":"select","from":"public.Order","select":[{"col":"select","as":"from"}],
                "where":{"and":[{"col":"select","eq":"s"},{"col":"we\"ird; x","like":"w%"}]}}))
            .await;
        assert_eq!(found.ok(), &json!([{"from":"s"}]));
        l.auth(json!({"v":1,"op":"select","from":"order"}))
            .await
            .error(404, "unknown_table");
        l.auth(json!({"v":1,"op":"select","from":"Order","select":["Select"]}))
            .await
            .error(400, "unknown_column");
        // A value that looks like SQL is only ever a value.
        let none = l
            .auth(json!({"v":1,"op":"select","from":"Order","where":{"col":"select","eq":"s'; DROP TABLE \"Order\"; --"}}))
            .await;
        assert_eq!(none.ok(), &json!([]));
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn extension_functions_work_in_defaults_and_are_not_callable() {
    live("ext", |l| async move {
        l.owner(
            r#"CREATE EXTENSION "uuid-ossp";
               CREATE TABLE e (id uuid PRIMARY KEY DEFAULT uuid_generate_v4(), v text);"#,
        )
        .await;
        let inserted = l
            .auth(json!({"v":1,"op":"insert","into":"e","values":[{"v":"a"}],"returning":["id"]}))
            .await;
        let id = inserted.ok()[0]["id"].as_str().expect("a uuid").to_owned();
        assert!(Uuid::parse_str(&id).is_ok(), "{id}");
        l.auth(json!({"v":1,"op":"call","fn":"uuid_generate_v4"}))
            .await
            .error(404, "unknown_function");
        let schema = l.role_schema("authenticated").await;
        let functions = &schema.ok()["schemas"]["public"]["functions"];
        assert!(functions.get("uuid_generate_v4").is_none(), "{functions}");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn schema_endpoints_live() {
    live("schema", |l| async move {
        l.owner(
            "CREATE TYPE mood AS ENUM ('sad', 'happy');
             CREATE TABLE s (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, m mood NOT NULL, \
               tags text[], note text);
             CREATE FUNCTION bump(n int DEFAULT 1) RETURNS SETOF int LANGUAGE sql AS 'SELECT n';",
        )
        .await;
        let schema = l.role_schema("authenticated").await;
        let public = &schema.ok()["schemas"]["public"];
        assert!(schema.json["version"].as_str().is_some_and(|v| Uuid::parse_str(v).is_ok()));
        let s = &public["tables"]["s"];
        assert_eq!(s["privileges"], Value::Null);
        assert_eq!(
            s["columns"]["id"],
            json!({"type":"int8","array":false,"nullable":false,"has_default":true,"generated":true})
        );
        assert_eq!(
            s["columns"]["m"],
            json!({"type":"mood","array":false,"nullable":false,"has_default":false,"generated":false,"enum":"public.mood"})
        );
        assert_eq!(s["columns"]["tags"]["array"], true);
        assert_eq!(public["enums"]["mood"], json!(["sad","happy"]));
        assert_eq!(
            public["functions"]["bump"],
            json!([{"args":[{"name":"n","type":"int4","array":false,"has_default":true}],
                    "returns":{"type":"int4","array":false,"set":true}}])
        );
        // Writing the identity column is refused up front.
        l.auth(json!({"v":1,"op":"insert","into":"s","values":[{"id":1,"m":"sad"}]}))
            .await
            .error(400, "invalid_query");

        let internal = l.internal_schema().await;
        assert_eq!(
            internal.ok()["schemas"]["public"]["tables"]["s"]["privileges"],
            json!({"anon":["select"],"authenticated":["select","insert","update","delete"]})
        );
        // anon can't execute functions by default, authenticated can: listed once.
        assert_eq!(internal.json["schemas"]["public"]["functions"]["bump"].as_array().map(Vec::len), Some(1));
    })
    .await;
}

/// A `chelabase` schema the owner made before the first bootstrap (with a trigger that
/// would make them superuser), an `=` operator planted for the first bootstrap's grants,
/// and a membership in `chelabase_admin`: the bootstrap renames the schema aside, revokes
/// the membership and never runs the owner's code as superuser.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn untrusted_chelabase_is_quarantined_and_runs_nothing_as_superuser() {
    live("quarantine", |l| async move {
        let superuser = || async {
            l.sql_on(l.main_id(), "SELECT rolsuper FROM pg_roles WHERE rolname = 'postgres';")
                .await
                .trim()
                .to_owned()
        };
        assert_eq!(superuser().await, "f", "the owner must start as a non-superuser");
        let quarantined = || async {
            l.sql_on(
                l.main_id(),
                "SELECT count(*) FROM pg_namespace WHERE nspname LIKE 'chelabase\\_quarantine\\_%';",
            )
            .await
            .trim()
            .to_owned()
        };
        // As if the database had never been bootstrapped, and the owner made
        // chelabase_admin first, with a login.
        l.sql_on(
            l.main_id(),
            "DROP SCHEMA chelabase CASCADE; DROP OWNED BY chelabase_admin; DROP ROLE chelabase_admin;",
        )
        .await;
        l.owner(
            "CREATE ROLE chelabase_admin LOGIN PASSWORD 'pwn';
             CREATE SCHEMA chelabase;
             CREATE TABLE chelabase.schema_version (one boolean PRIMARY KEY DEFAULT true CHECK (one), version uuid NOT NULL);
             CREATE FUNCTION public.escalate() RETURNS trigger LANGUAGE plpgsql
               AS 'BEGIN ALTER ROLE postgres SUPERUSER; RETURN NEW; END';
             CREATE TRIGGER escalate BEFORE INSERT OR UPDATE ON chelabase.schema_version
               FOR EACH ROW EXECUTE FUNCTION public.escalate();
             CREATE FUNCTION public.hijack(oid, regnamespace) RETURNS boolean LANGUAGE plpgsql
               AS 'BEGIN ALTER ROLE postgres SUPERUSER; RETURN $1 OPERATOR(pg_catalog.=) $2::oid; END';
             CREATE OPERATOR public.= (LEFTARG = oid, RIGHTARG = regnamespace, FUNCTION = public.hijack);
             CREATE TABLE t (v text);",
        )
        .await;
        let main = l.branch(l.main_id()).await;
        l.worker
            .bootstrap_data_api(&main, &l.database_name)
            .await
            .expect("bootstrap");
        assert_eq!(superuser().await, "f", "the owner's code ran as superuser");
        assert_eq!(quarantined().await, "1");
        // The owner can't log in as chelabase_admin to plant a trigger in the new schema.
        let admin = l
            .sql_on(
                l.main_id(),
                "SELECT rolcanlogin::text || ',' || (rolpassword IS NULL)::text FROM pg_authid WHERE rolname = 'chelabase_admin';",
            )
            .await;
        assert_eq!(admin.trim(), "false,true");
        if let Ok(mut conn) = l.connect_as("chelabase_admin", "pwn").await {
            sqlx::raw_sql(
                "CREATE FUNCTION chelabase.escalate() RETURNS trigger LANGUAGE plpgsql
                   AS 'BEGIN ALTER ROLE postgres SUPERUSER; RETURN NEW; END';
                 CREATE TRIGGER escalate BEFORE UPDATE ON chelabase.schema_version
                   FOR EACH ROW EXECUTE FUNCTION chelabase.escalate();",
            )
            .execute(&mut conn)
            .await
            .unwrap();
            l.owner("CREATE TABLE planted (v text);").await;
            panic!("logged in as chelabase_admin; superuser now: {}", superuser().await);
        }
        let owner = l
            .sql_on(l.main_id(), "SELECT nspowner::regrole FROM pg_namespace WHERE nspname = 'chelabase';")
            .await;
        assert_eq!(owner.trim(), "chelabase_admin");
        // The fresh schema works (and its first bootstrap granted `t`).
        let select = json!({"v":1,"op":"select","from":"t"});
        assert_eq!(l.anon(select.clone()).await.ok(), &json!([]));
        // DDL by the owner bumps the version through the new trigger, as before.
        l.owner("CREATE TABLE t2 (v text);").await;
        assert_eq!(superuser().await, "f");

        // A membership in chelabase_admin is revoked, and what it could have touched is set
        // aside again.
        l.sql_on(l.main_id(), "GRANT chelabase_admin TO postgres;").await;
        l.worker
            .bootstrap_data_api(&main, &l.database_name)
            .await
            .expect("bootstrap");
        let member = l
            .sql_on(l.main_id(), "SELECT pg_has_role('postgres', 'chelabase_admin', 'MEMBER');")
            .await;
        assert_eq!(member.trim(), "f");
        assert_eq!(quarantined().await, "2");
        l.anon(select.clone()).await.ok();

        // A healthy schema is left alone.
        l.worker
            .bootstrap_data_api(&main, &l.database_name)
            .await
            .expect("bootstrap");
        assert_eq!(quarantined().await, "2");
        assert_eq!(superuser().await, "f");
    })
    .await;
}

/// Chelabase's extension trigger must never run the owner's own event triggers with a
/// superuser current_user: its GRANTs run as whoever ran CREATE EXTENSION.
///
/// The owner's trigger acts only on GRANT (what Chelabase's trigger issues; the
/// extension scripts here have none). A trusted extension's script itself runs as the
/// bootstrap superuser and fires the owner's event triggers for its own commands on Neon
/// (session user not a superuser, so neon_fmgr_hook lets them run): that is Postgres and
/// Neon behaviour, reported separately, not this trigger.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn owner_event_triggers_never_run_as_superuser() {
    live("evtrig", |l| async move {
        let mut owner = l.owner_session().await;
        let created = sqlx::raw_sql(
            "CREATE FUNCTION public.sneak() RETURNS event_trigger LANGUAGE plpgsql AS
               'BEGIN IF TG_TAG = ''GRANT'' AND current_user <> session_user THEN ALTER ROLE postgres SUPERUSER; END IF; END';
             CREATE EVENT TRIGGER sneak ON ddl_command_end EXECUTE FUNCTION public.sneak();",
        )
        .execute(&mut owner)
        .await;
        created.expect("Neon lets the owner create an event trigger");
        sqlx::raw_sql("CREATE EXTENSION pgcrypto; CREATE EXTENSION \"uuid-ossp\";")
            .execute(&mut owner)
            .await
            .expect("the owner's CREATE EXTENSION");
        let owners = l
            .sql_on(
                l.main_id(),
                "SELECT string_agg(DISTINCT p.proowner::regrole::text, ',') FROM pg_proc p
                   JOIN pg_depend d ON d.objid = p.oid AND d.deptype = 'e'
                   JOIN pg_extension e ON e.oid = d.refobjid WHERE e.extname IN ('pgcrypto', 'uuid-ossp');",
            )
            .await;
        println!("extension functions are owned by: {}", owners.trim());
        let superuser = l
            .sql_on(l.main_id(), "SELECT rolsuper FROM pg_roles WHERE rolname = 'postgres';")
            .await;
        assert_eq!(
            superuser.trim(),
            "f",
            "Chelabase's GRANT ran the owner's event trigger as superuser"
        );
        // The extension's functions work for the API's roles.
        l.owner("CREATE TABLE e (id uuid PRIMARY KEY DEFAULT gen_random_uuid(), h text DEFAULT encode(digest('x', 'sha256'), 'hex'));")
            .await;
        l.auth(json!({"v":1,"op":"insert","into":"e","values":[{"id":Uuid::new_v4().to_string()}],"returning":["h"]}))
            .await
            .ok();
    })
    .await;
}

/// A superuser's CREATE EXTENSION (compute_ctl, Neon's privileged path) must keep the
/// extension's own revokes: Chelabase's trigger grants only the owner role's functions.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn a_superusers_extension_keeps_its_revokes() {
    live("extrevoke", |l| async move {
        let can = l
            .sql_on(
                l.main_id(),
                "CREATE EXTENSION pg_buffercache;
                 SELECT has_function_privilege('anon', 'pg_buffercache_pages()', 'EXECUTE');",
            )
            .await;
        assert_eq!(
            can.trim(),
            "f",
            "PUBLIC got EXECUTE back on pg_buffercache_pages()"
        );
    })
    .await;
}

/// A bare 30-digit number in the request keeps its digits: into a `numeric` column, a
/// `jsonb` column and a filter.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn big_request_numbers_keep_their_digits() {
    live("bignum", |l| async move {
        const BIG: &str = "123456789012345678901234567890";
        const FRACTION: &str = "1234567890.12345678901234567890";
        l.owner("CREATE TABLE big (id int PRIMARY KEY, n numeric, f numeric, j jsonb);")
            .await;
        let inserted = l
            .auth_raw(&format!(
                r#"{{"v":1,"op":"insert","into":"big","values":[{{"id":1,"n":{BIG},"f":{FRACTION},"j":{{"x":{BIG}}}}}]}}"#
            ))
            .await;
        assert_eq!(inserted.ok(), &json!([]));
        let read = l
            .auth_raw(&format!(
                r#"{{"v":1,"op":"select","from":"big","select":["n","f","j"],"where":{{"col":"n","eq":{BIG}}}}}"#
            ))
            .await;
        assert_eq!(read.status, 200, "{read:?}");
        assert_eq!(
            read.text,
            format!(r#"[{{"n":{BIG},"f":{FRACTION},"j":{{"x": {BIG}}}}}]"#),
            "{read:?}"
        );
    })
    .await;
}

/// Each relation's `kind`, and `security_invoker` for views however the option was spelt.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn kinds_and_security_invoker_live() {
    live("kinds", |l| async move {
        l.owner(
            "CREATE TABLE base (id int PRIMARY KEY);
             CREATE TABLE parted (id int) PARTITION BY RANGE (id);
             CREATE MATERIALIZED VIEW mv AS SELECT id FROM base;
             CREATE VIEW plain AS SELECT id FROM base;
             CREATE VIEW inv_bare WITH (security_invoker) AS SELECT id FROM base;
             CREATE VIEW inv_on WITH (security_invoker = on) AS SELECT id FROM base;
             CREATE VIEW inv_yes WITH (security_invoker = 'YES') AS SELECT id FROM base;
             CREATE VIEW inv_one WITH (security_invoker = 1) AS SELECT id FROM base;
             CREATE VIEW inv_off WITH (security_invoker = false) AS SELECT id FROM base;
             CREATE VIEW barrier WITH (security_barrier) AS SELECT id FROM base;
             GRANT SELECT ON ALL TABLES IN SCHEMA public TO anon;",
        )
        .await;
        let internal = l.internal_schema().await;
        let tables = &internal.ok()["schemas"]["public"]["tables"];
        for (name, kind, invoker) in [
            ("base", "table", false),
            ("parted", "table", false),
            ("mv", "materialized_view", false),
            ("plain", "view", false),
            ("inv_bare", "view", true),
            ("inv_on", "view", true),
            ("inv_yes", "view", true),
            ("inv_one", "view", true),
            ("inv_off", "view", false),
            ("barrier", "view", false),
        ] {
            assert_eq!(tables[name]["kind"], kind, "{name}: {}", tables[name]);
            assert_eq!(
                tables[name]["security_invoker"], invoker,
                "{name}: {}",
                tables[name]
            );
        }
    })
    .await;
}

/// The transaction timeout (= `statement_timeout_ms`) bounds a whole batch, even when no
/// single statement is over it.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn a_batch_past_the_transaction_timeout_answers_timeout() {
    live("txtimeout", |l| async move {
        l.metadata
            .update_database_settings(
                l.db,
                &chelabase_metadata::SettingsPatch {
                    statement_timeout_ms: Some(1000),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        l.owner("CREATE FUNCTION nap(s float8) RETURNS void LANGUAGE sql AS 'SELECT pg_sleep(s)';")
            .await;
        let one = l
            .auth(json!({"v":1,"op":"call","fn":"nap","args":{"s":0.6}}))
            .await;
        assert_eq!(one.text, "null", "{one:?}");
        let started = Instant::now();
        let nap = json!({"op":"call","fn":"nap","args":{"s":0.6}});
        let batch = l
            .auth(json!({"v":1,"op":"batch","ops":[nap, nap, nap]}))
            .await;
        batch.error(504, "timeout");
        assert!(
            started.elapsed() < Duration::from_millis(1700),
            "{:?}",
            started.elapsed()
        );
        // The next request gets a working connection.
        let again = l
            .auth(json!({"v":1,"op":"call","fn":"nap","args":{"s":0}}))
            .await;
        assert_eq!(again.text, "null", "{again:?}");
    })
    .await;
}

/// A function that changes the session (its role, a setting) leaves nothing behind for the
/// next request on the same connection.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn a_session_level_set_role_does_not_leak_to_the_next_request() {
    live("reset", |l| async move {
        l.metadata
            .update_database_settings(
                l.db,
                &chelabase_metadata::SettingsPatch {
                    max_connections: Some(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        l.owner(
            "CREATE TABLE t (id int PRIMARY KEY);
             INSERT INTO t VALUES (1);
             CREATE FUNCTION become_anon() RETURNS void LANGUAGE plpgsql AS \
               'BEGIN PERFORM set_config(''role'', ''anon'', false); \
                PERFORM set_config(''default_transaction_read_only'', ''on'', false); END';",
        )
        .await;
        let called = l.auth(json!({"v":1,"op":"call","fn":"become_anon"})).await;
        assert_eq!(called.text, "null", "{called:?}");
        let read = l.auth(json!({"v":1,"op":"select","from":"t"})).await;
        assert_eq!(read.ok(), &json!([{"id":1}]));
        let written = l
            .auth(json!({"v":1,"op":"insert","into":"t","values":[{"id":2}]}))
            .await;
        assert_eq!(written.ok(), &json!([]));
    })
    .await;
}

/// Reconcile's health check notices each thing the bootstrap repairs, and the bootstrap
/// repairs it.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn the_health_check_sees_what_the_bootstrap_repairs() {
    live("health", |l| async move {
        let main = l.branch(l.main_id()).await;
        let ep = main.endpoint_id.clone().expect("endpoint");
        assert!(l.worker.data_api_healthy(l.main_id(), &ep).await.unwrap());
        for breakage in [
            "ALTER EVENT TRIGGER chelabase_schema_version_ddl DISABLE;",
            "ALTER EVENT TRIGGER chelabase_extension_functions DISABLE;",
            "ALTER ROLE authenticator NOLOGIN;",
            "REVOKE USAGE ON SCHEMA chelabase FROM PUBLIC, authenticator;",
            "REVOKE SELECT ON chelabase.schema_version FROM authenticator;",
            // The default schema's USAGE (the data API parses as authenticator).
            "REVOKE USAGE ON SCHEMA public FROM PUBLIC, authenticator;",
            "DROP EVENT TRIGGER chelabase_schema_version_drop;",
            "DROP FUNCTION chelabase.assert_schema_version(uuid);",
            "DROP FUNCTION chelabase.cap_response(text, bigint);",
        ] {
            l.sql_on(l.main_id(), breakage).await;
            assert!(!l.worker.data_api_healthy(l.main_id(), &ep).await.unwrap(), "{breakage}");
            l.worker
                .bootstrap_data_api(&main, &l.database_name)
                .await
                .expect("bootstrap");
            assert!(
                l.worker.data_api_healthy(l.main_id(), &ep).await.unwrap(),
                "after {breakage}"
            );
        }
    })
    .await;
}

/// A schema change between two pipelined requests: the next one's asserted version is stale,
/// so it's planned again and runs sequentially, and its write commits once (the stale
/// pipeline's transaction aborted at the preamble, before the plan ran).
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn stale_version_replans_and_commits_once() {
    live("stale", |l| async move {
        l.owner("CREATE TABLE t (v text);").await;
        let insert = |v: &str| json!({"v":1,"op":"insert","into":"t","values":[{"v":v}],"returning":["v"]});
        // The first request reads the version (sequential), the second is pipelined.
        assert_eq!(l.auth(insert("a")).await.ok(), &json!([{"v":"a"}]));
        assert_eq!(l.auth(insert("b")).await.ok(), &json!([{"v":"b"}]));
        // The version changes under the cached one.
        l.owner("CREATE TABLE other (x int);").await;
        assert_eq!(l.auth(insert("c")).await.ok(), &json!([{"v":"c"}]));
        let all = json!({"v":1,"op":"select","from":"t","orderBy":[{"col":"v"}]});
        assert_eq!(
            l.auth(all.clone()).await.ok(),
            &json!([{"v":"a"},{"v":"b"},{"v":"c"}])
        );
        // The new version is the cached one now: the next requests pipeline again.
        assert_eq!(l.auth(insert("d")).await.ok(), &json!([{"v":"d"}]));
        assert_eq!(
            l.auth(json!({"v":1,"op":"select","from":"other"})).await.ok(),
            &json!([])
        );
        assert_eq!(l.auth(all).await.ok().as_array().map(Vec::len), Some(4));
    })
    .await;
}

/// A pipelined batch whose third write fails: every step after it is aborted, `COMMIT`
/// rolls back, and none of the batch's rows stay.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn mid_batch_failure_leaves_no_rows() {
    live("midbatch", |l| async move {
        l.owner(
            "CREATE TABLE u (id int PRIMARY KEY);
             CREATE FUNCTION refuse() RETURNS void LANGUAGE plpgsql AS 'BEGIN RAISE EXCEPTION ''no''; END';",
        )
        .await;
        let select = json!({"v":1,"op":"select","from":"u","orderBy":[{"col":"id"}]});
        // Warm: the next requests are pipelined.
        assert_eq!(l.auth(select.clone()).await.ok(), &json!([]));
        let insert = |id: i32| json!({"op":"insert","into":"u","values":[{"id":id}]});
        let failed = l
            .auth(json!({"v":1,"op":"batch","ops":[insert(1), insert(2), insert(1), insert(3)]}))
            .await;
        assert_eq!(failed.error(409, "conflict")["sqlstate"], "23505");
        assert_eq!(l.auth(select.clone()).await.ok(), &json!([]));
        // A function raising half-way: the same.
        let raised = l
            .auth(json!({"v":1,"op":"batch","ops":[
                insert(1), {"op":"call","fn":"refuse"}, insert(2)]}))
            .await;
        assert_eq!(raised.error(400, "raised")["message"], "no");
        assert_eq!(l.auth(select.clone()).await.ok(), &json!([]));
        // And the connection serves the next batch.
        let ok = l
            .auth(json!({"v":1,"op":"batch","ops":[insert(1), insert(2)]}))
            .await;
        assert_eq!(ok.ok(), &json!([[], []]));
        assert_eq!(l.auth(select).await.ok(), &json!([{"id":1},{"id":2}]));
    })
    .await;
}

/// A write whose `returning` is over the response cap is the 413, and the row isn't kept:
/// the cap is checked in SQL, before `COMMIT`.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn oversized_write_response_leaves_no_row() {
    live_capped("oversized", 200, |l| async move {
        l.owner("CREATE TABLE w (v text);").await;
        let select = json!({"v":1,"op":"select","from":"w","select":["v"]});
        // Warm (sequential), then pipelined.
        assert_eq!(l.auth(select.clone()).await.ok(), &json!([]));
        assert_eq!(l.auth(select.clone()).await.ok(), &json!([]));
        let long = "x".repeat(300);
        let too_big = l
            .auth(json!({"v":1,"op":"insert","into":"w","values":[{"v":long}],"returning":["v"]}))
            .await;
        assert_eq!(
            too_big.error(413, "too_large")["message"],
            "the response is over 200 bytes; ask for fewer rows (limit) or fewer columns"
        );
        assert_eq!(l.auth(select.clone()).await.ok(), &json!([]));
        // Without `returning` the response is small: the row is kept.
        let quiet = l
            .auth(json!({"v":1,"op":"insert","into":"w","values":[{"v":"short"}]}))
            .await;
        assert_eq!(quiet.ok(), &json!([]));
        assert_eq!(l.auth(select).await.ok(), &json!([{"v":"short"}]));
    })
    .await;
}

/// An RPC can set the server-side response counter (`chelabase.response_bytes`) to anything,
/// but it can't make the response pass the budget: a single result over it is refused in
/// SQL whatever the counter says, and Rust counts the results again.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn forged_response_counter_cannot_return_more_than_the_budget() {
    live_capped("forged", 200, |l| async move {
        l.owner(
            "CREATE TABLE big (v text); INSERT INTO big VALUES (repeat('x', 300));
             CREATE TABLE half (v text); INSERT INTO half VALUES (repeat('y', 120));
             CREATE FUNCTION forge() RETURNS void LANGUAGE plpgsql AS \
               'BEGIN PERFORM set_config(''chelabase.response_bytes'', ''-1000000000'', true); END';",
        )
        .await;
        let half = json!({"op":"select","from":"half"});
        // Warm (sequential), then pipelined.
        for _ in 0..2 {
            assert_eq!(
                l.auth(json!({"v":1,"op":"select","from":"half","limit":0})).await.ok(),
                &json!([])
            );
        }
        let message = "the response is over 200 bytes; ask for fewer rows (limit) or fewer columns";
        // Unforged, two results that are together over the budget: the SQL counter refuses.
        let over = l
            .auth(json!({"v":1,"op":"batch","ops":[half, half]}))
            .await;
        assert_eq!(over.error(413, "too_large")["message"], message);
        // Forged, one result over the budget on its own: still refused in SQL.
        let one = l
            .auth(json!({"v":1,"op":"batch","ops":[
                {"op":"call","fn":"forge"}, {"op":"select","from":"big"}]}))
            .await;
        assert_eq!(one.error(413, "too_large")["message"], message);
        // Forged, each result under the budget but together over it: Rust's count refuses.
        let together = l
            .auth(json!({"v":1,"op":"batch","ops":[{"op":"call","fn":"forge"}, half, half]}))
            .await;
        assert_eq!(together.error(413, "too_large")["message"], message);
        // Each alone fits.
        let fits = l.auth(json!({"v":1,"op":"select","from":"half"})).await;
        assert_eq!(fits.ok().as_array().map(Vec::len), Some(1));
    })
    .await;
}

/// An RPC that changes the session (`set_config(…, false)`, a `SET ROLE`) leaves nothing for
/// the next request on the same connection: the pipeline resets the session after `COMMIT`,
/// before the next request prepares its statements on it.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn session_state_set_by_an_rpc_does_not_reach_the_next_request() {
    live("session", |l| async move {
        l.metadata
            .update_database_settings(
                l.db,
                &chelabase_metadata::SettingsPatch {
                    max_connections: Some(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        l.owner(
            "CREATE TABLE t (id int PRIMARY KEY);
             INSERT INTO t VALUES (1);
             CREATE SCHEMA elsewhere;
             GRANT USAGE ON SCHEMA elsewhere TO anon, authenticated;
             CREATE FUNCTION wander() RETURNS void LANGUAGE plpgsql AS \
               'BEGIN PERFORM set_config(''search_path'', ''elsewhere'', false); \
                PERFORM set_config(''default_transaction_read_only'', ''on'', false); \
                PERFORM set_config(''app.marker'', ''left over'', false); \
                EXECUTE ''SET ROLE anon''; END';
             CREATE FUNCTION whoami() RETURNS text LANGUAGE sql STABLE AS \
               'SELECT current_user::text || '' | '' || current_setting(''search_path'') \
                  || '' | '' || current_setting(''transaction_read_only'') \
                  || '' | '' || coalesce(current_setting(''app.marker'', true), '''')';",
        )
        .await;
        let whoami = json!({"v":1,"op":"call","fn":"whoami"});
        // Sequential first, then pipelined.
        let before = l.auth(whoami.clone()).await.ok().clone();
        assert_eq!(l.auth(whoami.clone()).await.ok(), &before);
        assert!(
            before.as_str().is_some_and(|s| s.starts_with("authenticated | ")
                && s.contains(" | off | ")),
            "{before}"
        );
        let wandered = l.auth(json!({"v":1,"op":"call","fn":"wander"})).await;
        assert_eq!(wandered.text, "null", "{wandered:?}");
        assert_eq!(l.auth(whoami.clone()).await.ok(), &before);
        let read = l.auth(json!({"v":1,"op":"select","from":"t"})).await;
        assert_eq!(read.ok(), &json!([{"id":1}]));
        let written = l
            .auth(json!({"v":1,"op":"insert","into":"t","values":[{"id":2}]}))
            .await;
        assert_eq!(written.ok(), &json!([]));
        // The same in a batch. Inside the request the RPC's `SET ROLE anon` holds for the
        // operations after it (as Postgres runs them; anon may read `t`, not run `whoami`).
        let batch = l
            .auth(json!({"v":1,"op":"batch","ops":[{"op":"call","fn":"wander"}, {"op":"select","from":"t","orderBy":[{"col":"id"}]}]}))
            .await;
        assert_eq!(batch.ok(), &json!([null, [{"id":1},{"id":2}]]));
        assert_eq!(l.auth(whoami).await.ok(), &before);
    })
    .await;
}

/// A database whose guard functions are gone (a database not bootstrapped since they were
/// added): the pipeline finds them missing and every request runs sequentially, with the same
/// answers, until the bootstrap puts them back.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn a_database_without_the_guard_functions_runs_sequentially() {
    live("noguard", |l| async move {
        l.owner("CREATE TABLE t (id int PRIMARY KEY, v text);").await;
        let select = json!({"v":1,"op":"select","from":"t","orderBy":[{"col":"id"}]});
        // Warm (sequential), then pipelined: the guard functions' statements are prepared.
        assert_eq!(l.auth(select.clone()).await.ok(), &json!([]));
        assert_eq!(l.auth(select.clone()).await.ok(), &json!([]));
        l.sql_on(
            l.main_id(),
            "DROP FUNCTION chelabase.assert_schema_version(uuid); \
             DROP FUNCTION chelabase.cap_response(text, bigint);",
        )
        .await;
        let main = l.branch(l.main_id()).await;
        let ep = main.endpoint_id.clone().expect("endpoint");
        assert!(!l.worker.data_api_healthy(l.main_id(), &ep).await.unwrap());

        let inserted = l
            .auth(json!({"v":1,"op":"insert","into":"t","values":[{"id":1,"v":"a"}],"returning":["id","v"]}))
            .await;
        assert_eq!(inserted.ok(), &json!([{"id":1,"v":"a"}]));
        let insert = |id: i32| json!({"op":"insert","into":"t","values":[{"id":id}]});
        let batch = l
            .auth(json!({"v":1,"op":"batch","ops":[insert(2), {"op":"select","from":"t","select":["id"],"orderBy":[{"col":"id"}]}]}))
            .await;
        assert_eq!(batch.ok(), &json!([[], [{"id":1},{"id":2}]]));
        let failed = l
            .auth(json!({"v":1,"op":"batch","ops":[insert(3), insert(1)]}))
            .await;
        assert_eq!(failed.error(409, "conflict")["sqlstate"], "23505");
        assert_eq!(
            l.auth(select.clone()).await.ok(),
            &json!([{"id":1,"v":"a"},{"id":2,"v":null}])
        );
        l.anon(json!({"v":1,"op":"insert","into":"t","values":[{"id":9}]}))
            .await
            .error(403, "permission_denied");

        l.worker
            .bootstrap_data_api(&main, &l.database_name)
            .await
            .expect("bootstrap");
        assert!(l.worker.data_api_healthy(l.main_id(), &ep).await.unwrap());
        assert_eq!(
            l.auth(select).await.ok().as_array().map(Vec::len),
            Some(2)
        );
    })
    .await;
}

/// Selects `t`, in `database` when given (else the main one).
fn select_t(database: Option<&str>) -> Value {
    let mut body = json!({"v":1,"op":"select","from":"t"});
    if let Some(database) = database {
        body["database"] = json!(database);
    }
    body
}

/// A stored secret key of main with the allow-list `databases`; its hash (any 32 bytes do).
async fn stored_key(l: &Live, name: &str, databases: Vec<String>) -> [u8; 32] {
    let mut sha256 = [0u8; 32];
    sha256[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    sha256[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    l.metadata
        .insert_data_key(&NewDataKey {
            id: Uuid::new_v4(),
            branch_id: l.main_id(),
            kind: DataKeyKind::Secret,
            role: "authenticated".into(),
            name: name.into(),
            prefix: "chk_sec_live".into(),
            sha256,
            sealed: None,
            databases: Some(databases),
            role_oid: None,
            role_timeline: None,
            role_bypass_rls: false,
        })
        .await
        .expect("insert the key");
    sha256
}

/// Ends every session in `database` on main (rest's pools included), so the owner may rename
/// or drop it.
async fn end_sessions(l: &Live, database: &str) {
    l.invalidate(l.main_id()).await;
    l.sql_on(
        l.main_id(),
        &format!(
            "SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity \
             WHERE datname = '{database}' AND pid <> pg_backend_pid();"
        ),
    )
    .await;
}

/// The owner's own session runs `sql` (forwarded by the compute, as any client's DDL is).
async fn owner_ddl(l: &Live, sql: &str) {
    use sqlx::Executor;
    let mut owner = l.owner_session().await;
    owner
        .execute(sqlx::AssertSqlSafe(sql.to_owned()))
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

/// A second database, enabled with its own default schema: each database answers from its own
/// `t`, its own schema cache, and the schema endpoint lists both.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn two_databases_serve_separately_with_their_own_schema() {
    live("twodb", |l| async move {
        l.owner("CREATE TABLE t (v text); INSERT INTO t VALUES ('main');")
            .await;
        l.create_database("analytics").await;
        assert_eq!(
            l.data_api_state(l.main_id(), "analytics").await,
            DataApiState::Off
        );
        l.owner_in(
            "analytics",
            "CREATE SCHEMA stats; CREATE TABLE stats.t (v text, n int); \
             INSERT INTO stats.t VALUES ('analytics', 1);",
        )
        .await;
        let in_analytics = select_t(Some("analytics"));
        let error = l.anon(in_analytics.clone()).await;
        assert_eq!(
            error.error(404, "unknown_database")["message"],
            rest::error::UNKNOWN_DATABASE
        );

        assert!(
            l.metadata
                .set_database_default_schema(l.main_id(), "analytics", "stats")
                .await
                .unwrap()
        );
        let started = Instant::now();
        l.enable(l.main_id(), "analytics").await;
        println!("enable took {:?}", started.elapsed());

        // Each database's own `t`, in its own default schema; the first grants covered `stats`.
        assert_eq!(l.anon(select_t(None)).await.ok(), &json!([{"v":"main"}]));
        assert_eq!(
            l.anon(in_analytics.clone()).await.ok(),
            &json!([{"v":"analytics","n":1}])
        );
        let main_name = l.database_name.clone();
        assert_eq!(
            l.anon(select_t(Some(&main_name))).await.ok(),
            &json!([{"v":"main"}])
        );
        assert_eq!(
            l.auth(json!({"v":1,"database":"analytics","op":"select","from":"stats.t","select":["n"]}))
                .await
                .ok(),
            &json!([{"n":1}])
        );

        // DDL in one database moves only its own schema version.
        l.owner_in("analytics", "ALTER TABLE stats.t ADD COLUMN w int DEFAULT 7;")
            .await;
        assert_eq!(
            l.anon(in_analytics.clone()).await.ok(),
            &json!([{"v":"analytics","n":1,"w":7}])
        );
        assert_eq!(l.anon(select_t(None)).await.ok(), &json!([{"v":"main"}]));

        // Objects made after the enable get the default privileges in `stats`; a batch runs
        // in the database the top level names.
        l.owner_in(
            "analytics",
            "CREATE TABLE stats.later (x int); INSERT INTO stats.later VALUES (3);",
        )
        .await;
        assert_eq!(
            l.anon(json!({"v":1,"database":"analytics","op":"select","from":"later"}))
                .await
                .ok(),
            &json!([{"x":3}])
        );
        let batch = l
            .auth(json!({"v":1,"database":"analytics","op":"batch","ops":[
                {"op":"insert","into":"later","values":[{"x":4}],"returning":["x"]},
                {"op":"select","from":"later","orderBy":[{"col":"x"}]}
            ]}))
            .await;
        assert_eq!(batch.ok(), &json!([[{"x":4}], [{"x":3},{"x":4}]]));
        l.auth(json!({"v":1,"database":"analytics","op":"batch","ops":[
                {"op":"select","from":"later","database":"analytics"}
            ]}))
            .await
            .error(400, "invalid_query");

        let schema = l.schema_of("authenticated", Some("analytics")).await;
        assert!(
            schema.ok()["schemas"]["stats"]["tables"]["t"].is_object(),
            "{schema:?}"
        );
        assert_eq!(schema.json["databases"], json!([main_name, "analytics"]));
        let schema = l.schema_of("authenticated", None).await;
        assert!(schema.ok()["schemas"]["stats"].is_null(), "{schema:?}");
    })
    .await;
}

/// A database whose default schema isn't `public`: the pipeline prepares its statements as
/// `authenticator` (before the role switch), which the bootstrap grants `USAGE` on the
/// schema, so its requests run pipelined instead of failing to prepare and falling back every
/// time. A branch bootstrapped without that grant fails the health check, and the bootstrap
/// puts it back.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn a_non_public_default_schema_runs_pipelined() {
    live("nonpublic", |l| async move {
        l.create_database("analytics").await;
        l.owner_in(
            "analytics",
            "CREATE SCHEMA stats; CREATE TABLE stats.t (v text); \
             INSERT INTO stats.t VALUES ('analytics');",
        )
        .await;
        assert!(
            l.metadata
                .set_database_default_schema(l.main_id(), "analytics", "stats")
                .await
                .unwrap()
        );
        l.enable(l.main_id(), "analytics").await;
        let ep = l.branch(l.main_id()).await.endpoint_id.expect("endpoint");
        assert!(l.worker.data_api_healthy_in(&ep, "analytics", "stats").await.unwrap());

        let read = select_t(Some("analytics"));
        let write = json!({"v":1,"database":"analytics","op":"insert","into":"t",
            "values":[{"v":"more"}],"returning":["v"]});
        // Warm each role (sequential: the version, the schema and the role's view), then
        // every request is one pipeline.
        assert_eq!(l.anon(read.clone()).await.ok(), &json!([{"v":"analytics"}]));
        assert_eq!(l.auth(read.clone()).await.ok(), &json!([{"v":"analytics"}]));
        let before = l.state.engine.pipelined_runs();
        for _ in 0..3 {
            assert_eq!(l.anon(read.clone()).await.ok(), &json!([{"v":"analytics"}]));
        }
        assert_eq!(l.auth(write).await.ok(), &json!([{"v":"more"}]));
        assert_eq!(l.state.engine.pipelined_runs() - before, 4);

        // A database bootstrapped before authenticator got the grant: the check notices,
        // and the bootstrap repairs it.
        l.sql_in(
            l.main_id(),
            "analytics",
            "REVOKE USAGE ON SCHEMA stats FROM authenticator;",
        )
        .await;
        assert!(!l.worker.data_api_healthy_in(&ep, "analytics", "stats").await.unwrap());
        let main = l.branch(l.main_id()).await;
        l.worker
            .bootstrap_data_api(&main, &l.database_name)
            .await
            .expect("bootstrap");
        assert!(l.worker.data_api_healthy_in(&ep, "analytics", "stats").await.unwrap());
    })
    .await;
}

/// A key's allow-list: only the listed databases answer, everything else is the one 404 text.
/// A stored list follows a rename made in SQL, and dropping the last listed database revokes
/// the key (Review Focus 2).
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn allow_list_limits_a_key_to_one_database() {
    live("allow", |l| async move {
        l.owner("CREATE TABLE t (v text); INSERT INTO t VALUES ('main');")
            .await;
        l.create_database("analytics").await;
        l.owner_in(
            "analytics",
            "CREATE TABLE t (v text); INSERT INTO t VALUES ('analytics');",
        )
        .await;
        l.enable(l.main_id(), "analytics").await;
        let main_name = l.database_name.clone();
        let main = l.main_id();

        for (kind, role) in [
            (DataKeyKind::Public, "anon"),
            (DataKeyKind::Secret, "authenticated"),
        ] {
            let only = Some(vec!["analytics".to_string()]);
            assert_eq!(
                l.query_listed(main, kind, role, only.clone(), select_t(Some("analytics")))
                    .await
                    .ok(),
                &json!([{"v":"analytics"}])
            );
            for database in [None, Some(main_name.as_str()), Some("nope")] {
                let answer = l
                    .query_listed(main, kind, role, only.clone(), select_t(database))
                    .await;
                assert_eq!(
                    answer.error(404, "unknown_database")["message"],
                    rest::error::UNKNOWN_DATABASE,
                    "{database:?}"
                );
            }
            let main_only = Some(vec![main_name.clone()]);
            assert_eq!(
                l.query_listed(main, kind, role, main_only.clone(), select_t(None))
                    .await
                    .ok(),
                &json!([{"v":"main"}])
            );
            l.query_listed(main, kind, role, main_only, select_t(Some("analytics")))
                .await
                .error(404, "unknown_database");
            // No list: every served database.
            assert_eq!(
                l.query_listed(main, kind, role, None, select_t(Some("analytics")))
                    .await
                    .ok(),
                &json!([{"v":"analytics"}])
            );
        }

        // The schema endpoint lists only what the key reaches, and refuses the rest.
        let schema = |database: Option<&str>| {
            let mut url = format!("http://{}/data/v1/schema", l.rest);
            if let Some(database) = database {
                url.push_str(&format!("?database={database}"));
            }
            let token = l.token_for(
                main,
                DataKeyKind::Secret,
                "authenticated",
                Some(vec!["analytics".into()]),
            );
            let l = l.clone();
            async move {
                let resp = l
                    .http
                    .get(url)
                    .header(IDENTITY_HEADER, token)
                    .send()
                    .await
                    .unwrap();
                Live::answer(resp).await
            }
        };
        let listed = schema(Some("analytics")).await;
        assert_eq!(listed.ok()["databases"], json!(["analytics"]));
        schema(None).await.error(404, "unknown_database");

        // Stored lists follow SQL: a rename is followed, a drop removes the name, and a key
        // left with no database is revoked.
        let only = stored_key(&l, "only-analytics", vec!["analytics".into()]).await;
        let both = stored_key(&l, "both", vec![main_name.clone(), "analytics".into()]).await;
        end_sessions(&l, "analytics").await;
        owner_ddl(&l, "ALTER DATABASE analytics RENAME TO analytics_two").await;
        l.wait_row(main, "analytics_two", true).await;
        let lookup = |sha: [u8; 32]| {
            let l = l.clone();
            async move { l.metadata.lookup_data_key(&sha).await.unwrap() }
        };
        assert_eq!(
            lookup(only).await.expect("live").databases,
            Some(vec!["analytics_two".to_string()])
        );
        assert_eq!(
            lookup(both).await.expect("live").databases,
            Some(vec![main_name.clone(), "analytics_two".to_string()])
        );
        assert_eq!(
            l.data_api_state(main, "analytics_two").await,
            DataApiState::On,
            "a rename keeps the flag"
        );
        l.invalidate(main).await;
        assert_eq!(
            l.query_listed(
                main,
                DataKeyKind::Secret,
                "authenticated",
                Some(vec!["analytics_two".into()]),
                select_t(Some("analytics_two"))
            )
            .await
            .ok(),
            &json!([{"v":"analytics"}])
        );

        end_sessions(&l, "analytics_two").await;
        owner_ddl(&l, "DROP DATABASE analytics_two").await;
        l.wait_row(main, "analytics_two", false).await;
        assert!(
            lookup(only).await.is_none(),
            "the key's last database is gone: revoked"
        );
        assert_eq!(
            lookup(both).await.expect("live").databases,
            Some(vec![main_name.clone()])
        );
    })
    .await;
}

/// Disabling a served database while rest holds a warm pool and cached shapes for it: 404 at
/// once after the invalidate, and within the 60 s info cache without it (Review Focus 4).
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn disable_answers_unknown_database_at_once() {
    live("disable", |l| async move {
        l.create_database("analytics").await;
        l.owner_in(
            "analytics",
            "CREATE TABLE t (v text); INSERT INTO t VALUES ('analytics');",
        )
        .await;
        let main = l.main_id();
        l.enable(main, "analytics").await;
        let query = select_t(Some("analytics"));
        for _ in 0..3 {
            assert_eq!(l.anon(query.clone()).await.ok(), &json!([{"v":"analytics"}]));
        }

        l.disable(main, "analytics").await;
        assert_eq!(l.data_api_state(main, "analytics").await, DataApiState::Off);
        l.invalidate(main).await;
        l.anon(query.clone()).await.error(404, "unknown_database");
        // rest's console route answers the same.
        let resp = l
            .http
            .post(format!(
                "http://{}/internal/console/{}/query",
                l.internal,
                main.as_uuid()
            ))
            .bearer_auth(&l.internal_token)
            .json(&json!({"role":"authenticated","database":"analytics","tree":select_t(None)}))
            .send()
            .await
            .unwrap();
        Live::answer(resp).await.error(404, "unknown_database");

        // The machinery stays: re-enabling is the bootstrap again, and serves at once.
        let ep = l.branch(main).await.endpoint_id.expect("endpoint");
        assert!(l.worker.data_api_healthy_in(&ep, "analytics", "public").await.unwrap());
        l.enable(main, "analytics").await;
        assert_eq!(l.anon(query.clone()).await.ok(), &json!([{"v":"analytics"}]));

        // Without the invalidate call (it failed, say): the connection info's 60 s lifetime.
        l.disable(main, "analytics").await;
        let started = Instant::now();
        loop {
            let answer = l.anon(query.clone()).await;
            if answer.status == 404 {
                answer.error(404, "unknown_database");
                break;
            }
            answer.ok();
            assert!(
                started.elapsed() < Duration::from_secs(70),
                "still served {:?} after the disable",
                started.elapsed()
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        println!("served for {:?} after the disable, without an invalidate", started.elapsed());
    })
    .await;
}

/// A child of a branch that serves a second database serves it too, with the parent's
/// default schema, and its own bootstrap ran there (Review Focus 5).
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn child_branch_serves_the_parent_second_database() {
    live("childdb", |l| async move {
        l.create_database("analytics").await;
        l.owner_in(
            "analytics",
            "CREATE SCHEMA stats; CREATE TABLE stats.t (v text); \
             INSERT INTO stats.t VALUES ('parent');",
        )
        .await;
        assert!(
            l.metadata
                .set_database_default_schema(l.main_id(), "analytics", "stats")
                .await
                .unwrap()
        );
        l.enable(l.main_id(), "analytics").await;

        let dev_id = BranchId::new();
        let outcome = l
            .metadata
            .create_branch(&NewBranch {
                id: dev_id,
                database_id: l.db,
                name: "dev",
                parent_branch_id: l.main_id(),
                branched_at_time: None,
                branched_at_lsn: None,
            })
            .await
            .unwrap();
        assert_eq!(outcome, CreateBranchOutcome::Created);
        let started = Instant::now();
        l.worker
            .handle_branch_job(BranchJob::Create {
                branch_id: dev_id,
                at: None,
            })
            .await
            .expect("create job");
        println!("branch create took {:?}", started.elapsed());
        let dev = l.branch(dev_id).await;
        assert_eq!(dev.status, BranchStatus::Ready, "{:?}", dev.error);

        let row = l
            .metadata
            .branch_databases(dev_id)
            .await
            .unwrap()
            .into_iter()
            .find(|d| d.name == "analytics")
            .expect("the child lists analytics");
        assert_eq!(row.data_api, DataApiState::On);
        assert_eq!(row.default_schema, "stats");
        let ep = dev.endpoint_id.clone().expect("endpoint");
        assert!(l.worker.data_api_healthy_in(&ep, "analytics", "stats").await.unwrap());

        let on_dev = |body: Value| l.query_on(dev_id, DataKeyKind::Secret, "authenticated", body);
        assert_eq!(
            on_dev(select_t(Some("analytics"))).await.ok(),
            &json!([{"v":"parent"}])
        );
        on_dev(json!({"v":1,"database":"analytics","op":"insert","into":"t","values":[{"v":"child"}]}))
            .await
            .ok();
        assert_eq!(
            on_dev(json!({"v":1,"database":"analytics","op":"select","from":"t","orderBy":[{"col":"v"}]}))
                .await
                .ok(),
            &json!([{"v":"child"},{"v":"parent"}])
        );
        assert_eq!(
            l.anon(select_t(Some("analytics"))).await.ok(),
            &json!([{"v":"parent"}]),
            "the parent's data is its own"
        );
    })
    .await;
}
