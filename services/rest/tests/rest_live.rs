//! The data API engine against real computes.
//!
//! Each test provisions its own database with the real worker (as the worker's own live
//! tests do: `handle_provision` straight on the metadata database and the Neon stack, so
//! no platform token is needed), serves rest's router in-process on `127.0.0.1:0` and
//! talks to it with identity tokens it mints itself (a fresh key pair; the gateway is out
//! of the picture). Connection info comes from the orchestrator's internal listener, so
//! `just dev` must be up. Owner DDL runs through `admin_sql` after `SET ROLE postgres`,
//! which is what the owner's own connection would do. The database is removed at the end,
//! pass or fail.
//!
//! ```text
//! set -a; . .cheladb/zitadel/demo.env; eval "$(scripts/neon-images.sh)"; set +a
//! CHELADB_COMPUTE_IMAGE=$COMPUTE_NODE_IMAGE \
//!   cargo test -p rest --test rest_live -- --ignored --test-threads=1 --nocapture
//! ```

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cheladb_auth::internal::{
    AuthMethod, DataIdentity, IDENTITY_HEADER, Identity, InternalSigner, InternalVerifier,
    generate_keypair_pem,
};
use cheladb_metadata::{
    BranchRecord, BranchStatus, CreateBranchOutcome, DEMO_PROJECT_ID, DEMO_TENANT_ID,
    DatabaseStatus, MetadataClient, MetadataConfig, NewBranch,
};
use cheladb_nats::{BranchJob, PointInTime};
use cheladb_neon::{ComputeBackend, ComputeLocator, NeonId};
use cheladb_secrets::{Purpose, SecretKey, open_credential};
use cheladb_types::data_key::DataKeyKind;
use cheladb_types::{BranchId, DatabaseId};
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
        WorkerConfig::from_env().expect("worker config (is CHELADB_COMPUTE_IMAGE set?)");
    if config.compute_jwt_dir.is_relative() {
        config.compute_jwt_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(&config.compute_jwt_dir);
    }
    let metadata = MetadataClient::connect(&MetadataConfig {
        database_url: env_or(
            "CHELADB_METADATA_DATABASE_URL",
            "postgres://cheladb:cheladb@127.0.0.1:5433/cheladb_metadata",
        ),
    })
    .await
    .expect("metadata database");
    let key =
        SecretKey::from_env().expect("CHELADB_SECRETS_KEY (source .cheladb/zitadel/demo.env)");
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
            let _ = worker.storage.delete_tenant(&tenant).await;
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
    let internal_token = env_or("CHELADB_REST_INTERNAL_TOKEN", "cheladb-dev-rest-internal");
    let state = Arc::new(AppState {
        engine: Engine::new(
            InternalSource::new(
                &env_or("CHELADB_INTERNAL_URL", "http://127.0.0.1:8083"),
                &internal_token,
            ),
            200,
        ),
        verifier: InternalVerifier::from_pem(public.as_bytes()).unwrap(),
        internal_token: internal_token.clone(),
    });
    let ctx = Arc::new(Live {
        worker,
        metadata: metadata.clone(),
        db,
        database_name: record.database_name.clone().expect("database name"),
        rest: serve(router(state.clone())).await,
        internal: serve(internal_router(state)).await,
        internal_token,
        signer: InternalSigner::from_pem(private.as_bytes()).unwrap(),
        http: reqwest::Client::new(),
        key: SecretKey::from_env().expect("CHELADB_SECRETS_KEY"),
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

    /// SQL as cloud_admin on `branch` (psql's text output).
    async fn sql_on(&self, branch: BranchId, sql: &str) -> String {
        let b = self.branch(branch).await;
        let ep = b.endpoint_id.as_deref().expect("branch endpoint");
        self.worker
            .computes
            .admin_sql(ep, &self.database_name, sql)
            .await
            .unwrap_or_else(|e| panic!("sql on {}: {e:#}\n{sql}", b.name))
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
            }),
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
        assert_eq!(
            l.worker.check_data_role(l.main_id(), "reporter").await,
            RoleCheck::Ok
        );
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

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
async fn dropped_cheladb_is_unavailable_then_healed_by_bootstrap() {
    live("heal", |l| async move {
        l.owner("CREATE TABLE t (v text);").await;
        let select = json!({"v":1,"op":"select","from":"t"});
        l.anon(select.clone()).await.ok();

        l.sql_on(l.main_id(), "DROP SCHEMA cheladb CASCADE;").await;
        let down = l.anon(select.clone()).await;
        let error = down.error(503, "unavailable");
        assert_eq!(error["message"], rest::error::SCHEMA_MISSING);
        let main = l.branch(l.main_id()).await;
        l.worker
            .bootstrap_data_api(&main, &l.database_name)
            .await
            .expect("bootstrap");
        l.anon(select.clone()).await.ok();

        // Revoked grants answer the same way, until the bootstrap runs again.
        l.sql_on(
            l.main_id(),
            "REVOKE USAGE ON SCHEMA cheladb FROM authenticator;",
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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

/// A `cheladb` schema the owner made before the first bootstrap (with a trigger that
/// would make them superuser), an `=` operator planted for the first bootstrap's grants,
/// and a membership in `cheladb_admin`: the bootstrap renames the schema aside, revokes
/// the membership and never runs the owner's code as superuser.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
async fn untrusted_cheladb_is_quarantined_and_runs_nothing_as_superuser() {
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
                "SELECT count(*) FROM pg_namespace WHERE nspname LIKE 'cheladb\\_quarantine\\_%';",
            )
            .await
            .trim()
            .to_owned()
        };
        // As if the database had never been bootstrapped, and the owner made
        // cheladb_admin first, with a login.
        l.sql_on(
            l.main_id(),
            "DROP SCHEMA cheladb CASCADE; DROP OWNED BY cheladb_admin; DROP ROLE cheladb_admin;",
        )
        .await;
        l.owner(
            "CREATE ROLE cheladb_admin LOGIN PASSWORD 'pwn';
             CREATE SCHEMA cheladb;
             CREATE TABLE cheladb.schema_version (one boolean PRIMARY KEY DEFAULT true CHECK (one), version uuid NOT NULL);
             CREATE FUNCTION public.escalate() RETURNS trigger LANGUAGE plpgsql
               AS 'BEGIN ALTER ROLE postgres SUPERUSER; RETURN NEW; END';
             CREATE TRIGGER escalate BEFORE INSERT OR UPDATE ON cheladb.schema_version
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
        // The owner can't log in as cheladb_admin to plant a trigger in the new schema.
        let admin = l
            .sql_on(
                l.main_id(),
                "SELECT rolcanlogin::text || ',' || (rolpassword IS NULL)::text FROM pg_authid WHERE rolname = 'cheladb_admin';",
            )
            .await;
        assert_eq!(admin.trim(), "false,true");
        if let Ok(mut conn) = l.connect_as("cheladb_admin", "pwn").await {
            sqlx::raw_sql(
                "CREATE FUNCTION cheladb.escalate() RETURNS trigger LANGUAGE plpgsql
                   AS 'BEGIN ALTER ROLE postgres SUPERUSER; RETURN NEW; END';
                 CREATE TRIGGER escalate BEFORE UPDATE ON cheladb.schema_version
                   FOR EACH ROW EXECUTE FUNCTION cheladb.escalate();",
            )
            .execute(&mut conn)
            .await
            .unwrap();
            l.owner("CREATE TABLE planted (v text);").await;
            panic!("logged in as cheladb_admin; superuser now: {}", superuser().await);
        }
        let owner = l
            .sql_on(l.main_id(), "SELECT nspowner::regrole FROM pg_namespace WHERE nspname = 'cheladb';")
            .await;
        assert_eq!(owner.trim(), "cheladb_admin");
        // The fresh schema works (and its first bootstrap granted `t`).
        let select = json!({"v":1,"op":"select","from":"t"});
        assert_eq!(l.anon(select.clone()).await.ok(), &json!([]));
        // DDL by the owner bumps the version through the new trigger, as before.
        l.owner("CREATE TABLE t2 (v text);").await;
        assert_eq!(superuser().await, "f");

        // A membership in cheladb_admin is revoked, and what it could have touched is set
        // aside again.
        l.sql_on(l.main_id(), "GRANT cheladb_admin TO postgres;").await;
        l.worker
            .bootstrap_data_api(&main, &l.database_name)
            .await
            .expect("bootstrap");
        let member = l
            .sql_on(l.main_id(), "SELECT pg_has_role('postgres', 'cheladb_admin', 'MEMBER');")
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

/// The platform's extension trigger must never run the owner's own event triggers with a
/// superuser current_user: its GRANTs run as whoever ran CREATE EXTENSION.
///
/// The owner's trigger acts only on GRANT (what the platform's trigger issues; the
/// extension scripts here have none). A trusted extension's script itself runs as the
/// bootstrap superuser and fires the owner's event triggers for its own commands on Neon
/// (session user not a superuser, so neon_fmgr_hook lets them run): that is Postgres and
/// Neon behaviour, reported separately, not this trigger.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
            "the platform's GRANT ran the owner's event trigger as superuser"
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
/// extension's own revokes: the platform's trigger grants only the owner role's functions.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
async fn a_batch_past_the_transaction_timeout_answers_timeout() {
    live("txtimeout", |l| async move {
        l.metadata
            .update_database_settings(
                l.db,
                &cheladb_metadata::SettingsPatch {
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
async fn a_session_level_set_role_does_not_leak_to_the_next_request() {
    live("reset", |l| async move {
        l.metadata
            .update_database_settings(
                l.db,
                &cheladb_metadata::SettingsPatch {
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
async fn the_health_check_sees_what_the_bootstrap_repairs() {
    live("health", |l| async move {
        let main = l.branch(l.main_id()).await;
        let ep = main.endpoint_id.clone().expect("endpoint");
        assert!(l.worker.data_api_healthy(&ep).await.unwrap());
        for breakage in [
            "ALTER EVENT TRIGGER cheladb_schema_version_ddl DISABLE;",
            "ALTER EVENT TRIGGER cheladb_extension_functions DISABLE;",
            "ALTER ROLE authenticator NOLOGIN;",
            "REVOKE USAGE ON SCHEMA cheladb FROM authenticator;",
            "REVOKE SELECT ON cheladb.schema_version FROM authenticator;",
            "DROP EVENT TRIGGER cheladb_schema_version_drop;",
        ] {
            l.sql_on(l.main_id(), breakage).await;
            assert!(!l.worker.data_api_healthy(&ep).await.unwrap(), "{breakage}");
            l.worker
                .bootstrap_data_api(&main, &l.database_name)
                .await
                .expect("bootstrap");
            assert!(
                l.worker.data_api_healthy(&ep).await.unwrap(),
                "after {breakage}"
            );
        }
    })
    .await;
}
