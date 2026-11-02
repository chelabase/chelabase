//! The storage sweep (object-storage m8, issue #99) against real computes, the real storage
//! API and SeaweedFS: objects made through the gateway, deleted on main but kept on a child,
//! moved to another bucket, deleted everywhere; one in-process sweep with no grace and no
//! history window deletes only the blob nothing references, and an open multipart upload.
//!
//! `shared-state`: provisions a database and a branch on the workbench stack, deleted
//! afterwards pass or fail, with every blob and mark it made.
//!
//! ```text
//! just workbench test --stack epic-object-storage -p worker --test storage_sweep_live -- --run-ignored only
//! ```

// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chelabase_metadata::{
    BranchStatus, CreateBranchOutcome, DEMO_TENANT_ID, DatabaseStatus, MetadataClient, NewBranch,
};
use chelabase_nats::BranchJob;
use chelabase_neon::ComputeBackend;
use chelabase_secrets::{Purpose, open_credential};
use chelabase_storage::ObjectStore;
use chelabase_types::{BranchId, DatabaseId, ProjectId};
use serde_json::{Value, json};
use uuid::Uuid;
use worker::config::RealWorker;
use worker::storage_sweep::{SweepBranch, SweepDatabase, SweepSource, sweep_database};

mod common;
use common::{cleanup, proxy_ca_path, proxy_hostaddr, setup, workspace_root};

struct Fx {
    worker: RealWorker,
    metadata: MetadataClient,
    project: ProjectId,
    id: DatabaseId,
    main: BranchId,
    ep: String,
    database: String,
    owner_password: String,
}

impl Fx {
    async fn provision() -> Fx {
        let (worker, metadata, _config) = setup().await;
        let id = DatabaseId::new();
        let name = format!("sw-{}", &id.to_string()[..8]);
        let project = metadata
            .create_project(DEMO_TENANT_ID.into(), &name)
            .await
            .expect("create a project")
            .id;
        metadata
            .insert_database_pending(id, DEMO_TENANT_ID.into(), project, &name)
            .await
            .expect("insert pending database");
        let provisioned =
            tokio::time::timeout(Duration::from_secs(90), worker.handle_provision(id, &name)).await;
        let ready = match provisioned {
            Ok(Ok(())) => metadata.get_database(id).await.ok().flatten(),
            _ => None,
        };
        let Some(record) = ready.filter(|r| r.status == DatabaseStatus::Ready) else {
            cleanup(&worker, &metadata, id).await;
            let _ = metadata.finish_project_delete(project).await;
            panic!("provision failed: {provisioned:?}");
        };
        let main = metadata
            .get_branch(BranchId::from(id))
            .await
            .unwrap()
            .expect("main branch");
        let owner_password = open_credential(
            &worker.key,
            &main.id.to_string(),
            Purpose::Owner,
            main.owner_password_enc.as_deref().expect("owner password"),
        )
        .expect("open the owner password");
        Fx {
            ep: main.endpoint_id.clone().expect("endpoint"),
            database: record.database_name.clone().expect("database name"),
            main: main.id,
            worker,
            metadata,
            project,
            id,
            owner_password,
        }
    }

    async fn teardown(&self) {
        let _ = self.metadata.finish_project_delete(self.project).await;
        cleanup(&self.worker, &self.metadata, self.id).await;
    }

    async fn admin(&self, sql: &str) -> String {
        self.worker
            .computes
            .admin_sql(&self.ep, &self.database, sql)
            .await
            .unwrap_or_else(|e| panic!("admin_sql {sql:?}: {e:#}"))
    }

    async fn value(&self, sql: &str) -> String {
        self.admin(sql)
            .await
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty())
            .unwrap_or_default()
            .to_string()
    }

    /// SQL as the owner through the proxy.
    async fn owner(&self, sql: &str) {
        let port: u16 = std::env::var("CHELABASE_PROXY_PG_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(4432);
        let domain = std::env::var("CHELABASE_PROXY_DOMAIN")
            .ok()
            .filter(|d| !d.trim().is_empty())
            .unwrap_or_else(|| "localtest.me".into());
        let conn = format!(
            "host={}.{domain} hostaddr={} port={port} dbname={} sslmode=verify-full sslrootcert={}",
            self.ep,
            proxy_hostaddr(),
            self.database,
            proxy_ca_path().display()
        );
        let out = tokio::process::Command::new("psql")
            .arg(&conn)
            .env("PGUSER", "postgres")
            .env("PGPASSWORD", &self.owner_password)
            .env("PGCONNECT_TIMEOUT", "10")
            .args(["-X", "-qtA", "-v", "ON_ERROR_STOP=1", "-c", sql])
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .expect("run psql");
        assert!(
            out.status.success(),
            "{sql}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A child of main, made by the worker's create job.
    async fn child(&self, name: &str) -> BranchId {
        let child = BranchId::new();
        let outcome = self
            .metadata
            .create_branch(&NewBranch {
                id: child,
                database_id: self.id,
                name,
                parent_branch_id: self.main,
                branched_at_time: None,
                branched_at_lsn: None,
            })
            .await
            .unwrap();
        assert_eq!(outcome, CreateBranchOutcome::Created);
        self.worker
            .handle_branch_job(BranchJob::Create {
                branch_id: child,
                at: None,
            })
            .await
            .expect("create job");
        let record = self.metadata.get_branch(child).await.unwrap().unwrap();
        assert_eq!(record.status, BranchStatus::Ready, "{:?}", record.error);
        child
    }

    /// A secret key on `authenticated` for `branch`.
    async fn secret_key(&self, branch: BranchId, name: &str) -> String {
        let (status, body) = api(
            "POST",
            &format!("/v1/databases/{}/branches/{branch}/keys", self.id),
            Some(json!({ "name": name, "role": "authenticated" })),
        )
        .await;
        assert_eq!(status, 201, "create key {name}: {body}");
        body["key"].as_str().unwrap().to_string()
    }

    async fn prefix(&self) -> String {
        let oid = self
            .value("SELECT oid FROM pg_database WHERE datname = current_database()")
            .await;
        format!("{}/{oid}/", self.id)
    }

    /// Every blob, mark and open upload this database left in the shared store goes.
    async fn purge(&self, store: &chelabase_storage::S3Store) {
        for prefix in [format!("{}/", self.id), format!("_sweep/{}/", self.id)] {
            for o in store.list(&prefix).await.unwrap_or_default() {
                let _ = store.delete(&o.key).await;
            }
            for u in store.list_uploads(&prefix).await.unwrap_or_default() {
                let _ = store.multipart_abort(&u.key, &u.upload_id).await;
            }
        }
    }
}

fn gateway() -> String {
    std::env::var("GATEWAY_URL")
        .or_else(|_| std::env::var("CHELABASE_ENTRY_API_URL"))
        .unwrap_or_else(|_| "http://api.chelabase.localhost:8000".into())
}

/// A management API call as the demo user (`chelabase dev token`).
async fn api(method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let cli =
        std::env::var("CHELABASE_CLI").expect("CHELABASE_CLI (run through just workbench test)");
    let token = tokio::process::Command::new(cli)
        .args(["dev", "token"])
        .current_dir(
            std::env::var("CHELABASE_CHECKOUT").map_or_else(|_| workspace_root(), Into::into),
        )
        .output()
        .await
        .expect("chelabase dev token");
    assert!(token.status.success(), "chelabase dev token failed");
    let mut req = reqwest::Client::new()
        .request(method.parse().unwrap(), format!("{}{path}", gateway()))
        .bearer_auth(String::from_utf8_lossy(&token.stdout).trim())
        .timeout(Duration::from_secs(60));
    if let Some(body) = body {
        req = req.json(&body);
    }
    let res = req.send().await.expect("the management API answers");
    let status = res.status().as_u16();
    (
        status,
        serde_json::from_str(&res.text().await.unwrap_or_default()).unwrap_or(Value::Null),
    )
}

/// An object call with data key `key`: `(status, body)`.
async fn object(
    method: &str,
    path: &str,
    key: &str,
    body: Option<Vec<u8>>,
    json: bool,
) -> (u16, bytes::Bytes) {
    let mut req = reqwest::Client::new()
        .request(method.parse().unwrap(), format!("{}{path}", gateway()))
        .bearer_auth(key)
        .timeout(Duration::from_secs(60));
    if json {
        req = req.header("content-type", "application/json");
    }
    if let Some(body) = body {
        req = req.body(body);
    }
    let res = req.send().await.expect("the storage API answers");
    (res.status().as_u16(), res.bytes().await.unwrap_or_default())
}

/// The object store as the storage service sees it (the stack's `storage` identity).
async fn object_store() -> chelabase_storage::S3Store {
    let state = std::env::var("CHELABASE_STACK_DIR")
        .expect("CHELABASE_STACK_DIR (run through just workbench test)");
    let cfg = chelabase_storage::StorageConfig::from_lookup(|k| {
        Some(match k {
            "CHELABASE_STORAGE_BACKEND" => "seaweedfs".into(),
            "CHELABASE_STORAGE_S3_ENDPOINT" => "http://seaweedfs:8333".into(),
            "CHELABASE_STORAGE_S3_BUCKET" => "chelabase-storage".into(),
            "CHELABASE_STORAGE_S3_ACCESS_KEY_FILE" => format!("{state}/storage-s3/access-key"),
            "CHELABASE_STORAGE_S3_SECRET_KEY_FILE" => format!("{state}/storage-s3/secret-key"),
            _ => return None,
        })
    })
    .expect("the object store's settings");
    chelabase_storage::S3Store::new(&cfg).await
}

/// The worker's own reads, with no history window: a mark can be acted on at once.
struct NoWindow<'a>(&'a RealWorker);

impl SweepSource for NoWindow<'_> {
    async fn database(&self, db: DatabaseId) -> Result<Option<SweepDatabase>, String> {
        let found = self.0.database(db).await?;
        if let Some(d) = &found {
            assert!(
                d.retention >= Duration::from_secs(3600),
                "the real window was read: {d:?}"
            );
        }
        Ok(found.map(|d| SweepDatabase {
            retention: Duration::ZERO,
            ..d
        }))
    }

    async fn references(
        &self,
        db: DatabaseId,
        b: &SweepBranch,
        expire: bool,
    ) -> Result<(HashSet<Uuid>, u64), String> {
        self.0.references(db, b, expire).await
    }
}

#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_sweep_live() {
    let fx = Arc::new(Fx::provision().await);
    let task = fx.clone();
    let outcome = tokio::spawn(async move { run(&task).await }).await;
    fx.purge(&object_store().await).await;
    fx.teardown().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

async fn run(fx: &Fx) {
    let store = object_store().await;
    for name in ["files", "other"] {
        fx.admin(&format!(
            "BEGIN; SET LOCAL ROLE chelabase_storage; INSERT INTO storage.buckets (name) VALUES ('{name}'); COMMIT;"
        ))
        .await;
    }
    fx.owner("SELECT storage.create_policy('b_select', 'buckets', 'SELECT', ARRAY['authenticated']::name[], 'true', NULL)").await;
    for (i, (command, using, check)) in [
        ("SELECT", "'true'", "NULL"),
        ("INSERT", "NULL", "'true'"),
        ("UPDATE", "'true'", "'true'"),
        ("DELETE", "'true'", "NULL"),
    ]
    .iter()
    .enumerate()
    {
        fx.owner(&format!(
            "SELECT storage.create_policy('o_{i}', 'objects', '{command}', ARRAY['authenticated']::name[], {using}, {check})"
        ))
        .await;
    }
    let key = fx.secret_key(fx.main, "sw-main").await;
    let mut versions = std::collections::HashMap::new();
    for name in ["kept-on-child", "moved", "gone", "stays"] {
        let (status, b) = object(
            "POST",
            &format!("/storage/v1/object/files/{name}"),
            &key,
            Some(name.as_bytes().to_vec()),
            false,
        )
        .await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&b));
        let body: Value = serde_json::from_slice(&b).unwrap();
        versions.insert(name, body["version_id"].as_str().unwrap().to_string());
    }
    // The child copies every row; then main loses two, the child one.
    let child = fx.child("sweep-child").await;
    let child_key = fx.secret_key(child, "sw-child").await;
    for (k, name) in [
        (&key, "kept-on-child"),
        (&key, "gone"),
        (&child_key, "gone"),
    ] {
        let (status, b) = object(
            "DELETE",
            &format!("/storage/v1/object/files/{name}"),
            k,
            None,
            false,
        )
        .await;
        assert!(
            status == 200 || status == 204,
            "delete {name}: {status} {}",
            String::from_utf8_lossy(&b)
        );
    }
    let body = json!({ "bucket": "files", "from": "moved", "to": "moved", "to_bucket": "other" })
        .to_string();
    let (status, b) = object(
        "POST",
        "/storage/v1/object/move",
        &key,
        Some(body.into_bytes()),
        true,
    )
    .await;
    assert_eq!(status, 204, "{}", String::from_utf8_lossy(&b));
    // A multipart upload nobody finished.
    let prefix = fx.prefix().await;
    let open = format!("{prefix}{}", Uuid::new_v4());
    store.multipart_create(&open, None).await.unwrap();

    let all: HashSet<String> = versions.values().cloned().collect();
    assert_eq!(blobs(&store, &prefix).await, all);

    let source = NoWindow(&fx.worker);
    // A dry run counts, and changes nothing.
    let dry = sweep_database(&source, &store, fx.id, Duration::ZERO, true)
        .await
        .unwrap();
    println!("dry run: {dry:?}");
    assert_eq!(dry.blocked, None, "{dry:?}");
    assert_eq!(
        (dry.referenced, dry.marked, dry.uploads_aborted),
        (3, 1, 0),
        "{dry:?}"
    );
    assert_eq!(blobs(&store, &prefix).await, all);
    assert_eq!(store.list_uploads(&prefix).await.unwrap().len(), 1);

    // The first pass marks the one blob nothing references and the open upload (SeaweedFS
    // doesn't say when an upload began); the second deletes the blob and aborts the upload.
    let first = sweep_database(&source, &store, fx.id, Duration::ZERO, false)
        .await
        .unwrap();
    println!("first pass: {first:?}");
    assert_eq!(
        (
            first.blocked.as_deref(),
            first.marked,
            first.deleted,
            first.uploads_aborted
        ),
        (None, 1, 0, 0),
        "{first:?}"
    );
    assert_eq!(store.list_uploads(&prefix).await.unwrap().len(), 1);
    assert_eq!(blobs(&store, &prefix).await, all);
    let second = sweep_database(&source, &store, fx.id, Duration::ZERO, false)
        .await
        .unwrap();
    println!("second pass: {second:?}");
    assert_eq!(
        (
            second.blocked.as_deref(),
            second.referenced,
            second.deleted,
            second.uploads_aborted
        ),
        (None, 3, 1, 1),
        "{second:?}"
    );
    assert!(store.list_uploads(&prefix).await.unwrap().is_empty());
    let mut left = all.clone();
    left.remove(&versions["gone"]);
    assert_eq!(blobs(&store, &prefix).await, left);

    // What survived still downloads: the moved object on main, the deleted one on the child.
    let (status, b) = object("GET", "/storage/v1/object/other/moved", &key, None, false).await;
    assert_eq!((status, &b[..]), (200, &b"moved"[..]));
    let (status, b) = object(
        "GET",
        "/storage/v1/object/files/kept-on-child",
        &child_key,
        None,
        false,
    )
    .await;
    assert_eq!((status, &b[..]), (200, &b"kept-on-child"[..]));
    let (status, b) = object("GET", "/storage/v1/object/files/stays", &key, None, false).await;
    assert_eq!((status, &b[..]), (200, &b"stays"[..]));

    // With the real history window, a fresh mark waits: deleted everywhere, "stays" is
    // marked, and the next pass leaves it waiting.
    for k in [&key, &child_key] {
        let (status, _) = object("DELETE", "/storage/v1/object/files/stays", k, None, false).await;
        assert!(status == 200 || status == 204, "{status}");
    }
    let real = sweep_database(&fx.worker, &store, fx.id, Duration::ZERO, false)
        .await
        .unwrap();
    println!("with the real window: {real:?}");
    assert_eq!(
        (real.blocked.as_deref(), real.marked, real.deleted),
        (None, 1, 0),
        "{real:?}"
    );
    let again = sweep_database(&fx.worker, &store, fx.id, Duration::ZERO, false)
        .await
        .unwrap();
    assert_eq!((again.waiting, again.deleted), (1, 0), "{again:?}");
    assert_eq!(blobs(&store, &prefix).await, left);
}

/// The blob keys under `prefix`, without it.
async fn blobs(store: &chelabase_storage::S3Store, prefix: &str) -> HashSet<String> {
    store
        .list(prefix)
        .await
        .unwrap()
        .into_iter()
        .map(|o| o.key[prefix.len()..].to_string())
        .collect()
}
