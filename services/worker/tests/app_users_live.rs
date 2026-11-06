//! App users against real computes (roadmap step 6b): the users bootstrap, its guard and protect
//! triggers, the claim helpers under the data API, the data API's remove and rebootstrap paths
//! with the app-users objects, and the worker's enable, sync, reconcile, branch and disable
//! jobs with the real identity service.
//!
//! Same setup as `roles_live` (the workbench stack, so the gateway, rest and the
//! orchestrator serve the data API, plus `psql` in the runner and the proxy's CA), with
//! `demo.env` sourced for the app JWT key pair. The worker calls the identity service with its
//! own token (`CHELABASE_IDENTITY_WORKER_TOKEN_FILE`); the test makes and bans users with
//! auth's (`CHELABASE_IDENTITY_TOKEN_FILE`), as the admin API and sign-up would:
//!
//! ```text
//! just workbench test -p worker --test app_users_live -- --run-ignored only
//! ```
//!
//! Each test provisions its own database in its own project and deletes both afterwards, pass
//! or fail (with the project's identity pool and its users, when the test made them).

// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::future::Future;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chelabase_auth::app_token::{APP_ISSUER, AppClaims, AppTokenSigner, app_audience};
use chelabase_identity::{Identity, IdentityError, Missing, NewUser, Pool};
use chelabase_metadata::{
    AppUsersStatus, BeginEnable, BranchStatus, CreateBranchOutcome, DEMO_TENANT_ID, DatabaseStatus,
    MetadataClient, NewAppSession, NewBranch, NewProjectAuth, OnDelete,
};
use chelabase_nats::{AppUsersJob, AppUsersSyncJob, BranchJob};
use chelabase_neon::{ComputeBackend, ComputeLocator, DockerLocator};
use chelabase_secrets::{Purpose, open_credential, open_role_password};
use chelabase_types::{BranchId, DatabaseId, ProjectId};
use serde_json::{Value, json};
use uuid::Uuid;
use worker::app_users_sql::{
    USERS_OUTDATED_SQL, USERS_SYNC_ROLE, gone_call_sql, upsert_call_sql, users_bootstrap_sql,
};
use worker::config::RealWorker;

mod common;
use common::{cleanup, proxy_ca_path, proxy_hostaddr, setup, workspace_root};

const OWNER: &str = "postgres";
/// The sync role's password the tests' own bootstraps set.
const SYNC_PASSWORD: &str = "live-sync-Passw0rd-7d1f";

/// One provisioned database in its own project: its main branch, endpoint and the owner's
/// password.
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

/// Provisions a database in a new project of the demo tenant, runs `body` on it, then deletes
/// the project's app users rows, the database and the project, pass or fail.
async fn live<F, Fut>(body: F)
where
    F: FnOnce(Arc<Fx>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (mut worker, metadata, _config) = setup().await;
    // The runner's CHELABASE_IDENTITY_TOKEN_FILE is auth's; the worker calls with its own.
    worker.identity = Some(worker_identity());
    let id = DatabaseId::new();
    let name = format!("au-{}", &id.to_string()[..8]);
    let project = metadata
        .create_project(DEMO_TENANT_ID.into(), &name)
        .await
        .expect("create a project")
        .id;
    metadata
        .insert_database_pending(id, DEMO_TENANT_ID.into(), project, &name)
        .await
        .expect("insert pending database");
    let started = Instant::now();
    let provisioned =
        tokio::time::timeout(Duration::from_secs(90), worker.handle_provision(id, &name)).await;
    println!("provision took {:?}", started.elapsed());
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
    let fx = Arc::new(Fx {
        ep: main.endpoint_id.clone().expect("endpoint"),
        database: record.database_name.clone().expect("database name"),
        main: main.id,
        worker,
        metadata,
        project,
        id,
        owner_password,
    });

    // A panic inside the task is caught here, so the cleanup below always runs.
    let outcome = tokio::spawn(body(fx.clone())).await;
    if let Some(identity) = &fx.worker.identity {
        // Only this project's pool (named by its id): never another one.
        let _ = identity.drop_pool(&fx.pool()).await;
    }
    let _ = fx.metadata.finish_project_delete(fx.project).await;
    cleanup(&fx.worker, &fx.metadata, fx.id).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

/// The worker's identity client: `CHELABASE_IDENTITY_URL` with the worker's token.
fn worker_identity() -> Identity {
    let url = std::env::var("CHELABASE_IDENTITY_URL").expect("CHELABASE_IDENTITY_URL");
    let token = std::env::var("CHELABASE_IDENTITY_WORKER_TOKEN_FILE")
        .expect("CHELABASE_IDENTITY_WORKER_TOKEN_FILE");
    Identity::from_token_file(&url, std::path::Path::new(&token))
        .expect("the worker's identity token")
}

impl Fx {
    /// The project's app-user pool.
    fn pool(&self) -> Pool {
        Pool::App(self.project.as_uuid())
    }

    /// SQL as cloud_admin on `ep`'s compute, in `database` (forwarding off: `admin_sql`).
    async fn sql_in(&self, ep: &str, database: &str, sql: &str) -> String {
        self.worker
            .computes
            .admin_sql(ep, database, sql)
            .await
            .unwrap_or_else(|e| panic!("admin_sql {sql:?}: {e:#}"))
    }

    /// SQL as cloud_admin on main, in the main database.
    async fn sql(&self, sql: &str) -> String {
        self.sql_in(&self.ep, &self.database, sql).await
    }

    /// The first non-empty line of a one-value query on main.
    async fn value(&self, sql: &str) -> String {
        first_line(&self.sql(sql).await)
    }

    /// The users bootstrap for `public.users` on main, as the worker runs it.
    async fn bootstrap(&self) -> Result<String, String> {
        self.worker
            .computes
            .admin_sql(
                &self.ep,
                &self.database,
                &users_bootstrap_sql(
                    "public",
                    "users",
                    &chelabase_neon::scram::verifier(SYNC_PASSWORD),
                    OWNER,
                ),
            )
            .await
            .map_err(|e| format!("{e:#}"))
    }

    /// A row written as the sync role, the way the worker's sync does it.
    async fn upsert(&self, id: Uuid, email: &str, data: Option<Value>) -> String {
        first_line_matching(
            &self
                .sql(&upsert_call_sql(
                    id,
                    email,
                    true,
                    false,
                    chrono::Utc::now(),
                    data.as_ref(),
                ))
                .await,
            &["updated", "inserted", "inserted_managed"],
        )
    }

    /// SQL as the branch owner through the proxy, in the main database; errors carry their
    /// SQLSTATE (`VERBOSITY=verbose`).
    async fn owner(&self, sql: &str) -> Result<String, String> {
        psql(
            &proxy_conn(&self.ep, &self.database),
            OWNER,
            &self.owner_password,
            sql,
        )
        .await
    }

    async fn owner_ok(&self, sql: &str) -> String {
        self.owner(sql)
            .await
            .unwrap_or_else(|e| panic!("as postgres: {sql}: {e}"))
    }

    async fn owner_err(&self, sql: &str) -> String {
        match self.owner(sql).await {
            Ok(out) => panic!("as postgres, {sql} succeeded: {out}"),
            Err(e) => e,
        }
    }

    /// SQL as `chelabase_auth_sync` logged in with its password, straight to the compute's
    /// Postgres (as `auth` connects).
    async fn sync_login(&self, ep: &str, sql: &str) -> Result<String, String> {
        let locator = DockerLocator::connect(
            &std::env::var("CHELABASE_COMPUTE_NETWORK")
                .unwrap_or_else(|_| "chelabase_compute".into()),
            &std::env::var("CHELABASE_STACK").unwrap_or_else(|_| "main".into()),
        )
        .expect("the Docker client");
        let addr = locator
            .postgres_addr(ep)
            .await
            .expect("the compute's Postgres address")
            .expect("a published Postgres port");
        let conn = format!(
            "host={} port={} dbname={} sslmode=prefer",
            addr.ip(),
            addr.port(),
            self.database
        );
        psql(&conn, USERS_SYNC_ROLE, SYNC_PASSWORD, sql).await
    }

    fn branch_path(&self, branch: BranchId) -> String {
        format!("/v1/databases/{}/branches/{branch}", self.id)
    }

    /// Makes a child of main (the worker's create job) and waits for it; its id and endpoint.
    async fn child(&self, name: &str) -> (BranchId, String) {
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
        let started = Instant::now();
        self.worker
            .handle_branch_job(BranchJob::Create {
                branch_id: child,
                at: None,
            })
            .await
            .expect("create job");
        println!("create branch {name} took {:?}", started.elapsed());
        let record = self.metadata.get_branch(child).await.unwrap().unwrap();
        assert_eq!(record.status, BranchStatus::Ready, "{:?}", record.error);
        (child, record.endpoint_id.expect("child endpoint"))
    }

    /// A secret data key on `role` for main, through the API.
    async fn secret_key(&self, name: &str, role: &str) -> String {
        let (status, body) = api(
            "POST",
            &format!("{}/keys", self.branch_path(self.main)),
            Some(json!({ "name": name, "role": role })),
        )
        .await;
        assert_eq!(status, 201, "create key {name} on {role}: {body}");
        body["key"].as_str().unwrap().to_string()
    }

    /// Main's public data key, through the API.
    async fn public_key(&self) -> String {
        let (status, body) = api(
            "GET",
            &format!("{}/keys", self.branch_path(self.main)),
            None,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        body.as_array()
            .unwrap()
            .iter()
            .find(|k| k["kind"] == "public")
            .and_then(|k| k["key"].as_str())
            .expect("a public key")
            .to_string()
    }

    /// An access token for `user` in this project, with a live app session behind it.
    async fn user_token(&self, user: Uuid, email: &str) -> String {
        let sid = Uuid::new_v4();
        let now = chrono::Utc::now();
        self.metadata
            .insert_app_session(&NewAppSession {
                id: sid,
                project_id: self.project,
                user_id: user,
                identity_session_id: "app-users-live",
                refresh_hash: rand_hash(),
                expires_at: now + chrono::Duration::hours(1),
            })
            .await
            .expect("insert the app session");
        let signer = AppTokenSigner::from_pem_files(
            &env_path("CHELABASE_APP_JWT_PRIVATE_KEY"),
            &env_path("CHELABASE_APP_JWT_PUBLIC_KEY"),
        )
        .expect("the app JWT key pair");
        let iat = now.timestamp() as u64;
        signer
            .sign(&AppClaims {
                iss: APP_ISSUER.to_string(),
                aud: app_audience(self.project.as_uuid()),
                sub: user,
                sid,
                project: self.project.as_uuid(),
                email: email.to_string(),
                email_verified: true,
                role: "authenticated".to_string(),
                iat,
                exp: iat + 600,
            })
            .expect("sign the app token")
    }
}

fn first_line(out: &str) -> String {
    out.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// The first line of `out` that is one of `wanted` (psql echoes BEGIN/SET/COMMIT around it).
fn first_line_matching(out: &str, wanted: &[&str]) -> String {
    out.lines()
        .map(str::trim)
        .find(|l| wanted.contains(l))
        .unwrap_or_else(|| panic!("none of {wanted:?} in {out:?}"))
        .to_string()
}

fn rand_hash() -> [u8; 32] {
    let mut h = [0u8; 32];
    h[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    h[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    h
}

fn env_path(var: &str) -> std::path::PathBuf {
    let value = std::env::var(var).unwrap_or_else(|_| panic!("{var} is unset (source demo.env)"));
    let path = std::path::PathBuf::from(value);
    if path.is_relative() {
        workspace_root().join(path)
    } else {
        path
    }
}

fn proxy_host(ep: &str) -> String {
    let domain = std::env::var("CHELABASE_PROXY_DOMAIN")
        .ok()
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| "localtest.me".into());
    format!("{ep}.{domain}")
}

fn proxy_conn(ep: &str, database: &str) -> String {
    let port: u16 = std::env::var("CHELABASE_PROXY_PG_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(4432);
    let ca = proxy_ca_path();
    assert!(
        ca.exists(),
        "{} is missing (run `just workbench up`)",
        ca.display()
    );
    // The proxy is dialled at CHELABASE_PROXY_HOSTADDR when set; the TLS host name stays <ep>.<domain>.
    let hostaddr = proxy_hostaddr();
    format!(
        "host={} hostaddr={hostaddr} port={port} dbname={database} sslmode=verify-full sslrootcert={}",
        proxy_host(ep),
        ca.display()
    )
}

/// `sql` (one `-c`, one implicit transaction) as `user` on `conn`: the output, or psql's
/// error with its SQLSTATE.
async fn psql(conn: &str, user: &str, password: &str, sql: &str) -> Result<String, String> {
    let out = tokio::process::Command::new("psql")
        .arg(conn)
        .env("PGUSER", user)
        .env("PGPASSWORD", password)
        .env("PGCONNECT_TIMEOUT", "10")
        .args([
            "-X",
            "-qtA",
            "-v",
            "ON_ERROR_STOP=1",
            "-v",
            "VERBOSITY=verbose",
            "-c",
            sql,
        ])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .expect("run psql");
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn gateway() -> String {
    std::env::var("GATEWAY_URL").unwrap_or_else(|_| "http://api.chelabase.localhost:8000".into())
}

/// The `chelabase` CLI: `CHELABASE_CLI` (the runner sets it), else the build next to this test's
/// own binary's directory.
fn chelabase_cli() -> std::path::PathBuf {
    if let Ok(cli) = std::env::var("CHELABASE_CLI") {
        return cli.into();
    }
    let exe = std::env::current_exe().unwrap();
    let cli = exe.parent().unwrap().parent().unwrap().join("chelabase");
    assert!(
        cli.exists(),
        "{} is missing (just workbench up builds it)",
        cli.display()
    );
    cli
}

/// A management API call through the gateway as the demo tenant's user: `(status, body)`.
async fn api(method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let token = tokio::process::Command::new(chelabase_cli())
        .args(["dev", "token"])
        .current_dir(
            std::env::var("CHELABASE_CHECKOUT").map_or_else(|_| workspace_root(), Into::into),
        )
        .output()
        .await
        .expect("chelabase dev token");
    assert!(token.status.success(), "chelabase dev token failed");
    let token = String::from_utf8_lossy(&token.stdout).trim().to_string();
    let mut req = reqwest::Client::new()
        .request(method.parse().unwrap(), format!("{}{path}", gateway()))
        .bearer_auth(token)
        .timeout(Duration::from_secs(60));
    if let Some(body) = body {
        req = req.json(&body);
    }
    let res = req.send().await.expect("the management API answers");
    let status = res.status().as_u16();
    let text = res.text().await.unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// A data API query with a public `key`, and the user's token when given: `(status, body)`.
async fn data(key: &str, user: Option<&str>, query: Value) -> (u16, Value) {
    let mut req = reqwest::Client::new()
        .post(format!("{}/data/v1/query", gateway()))
        .bearer_auth(key)
        .timeout(Duration::from_secs(30))
        .json(&query);
    if let Some(token) = user {
        req = req.header("chelabase-user", token);
    }
    let res = req.send().await.expect("the data API answers");
    let status = res.status().as_u16();
    let text = res.text().await.unwrap_or_default();
    (status, serde_json::from_str(&text).unwrap_or(Value::Null))
}

/// The ids in a data API select's rows.
fn ids(body: &Value) -> Vec<String> {
    let rows = body.get("rows").unwrap_or(body);
    let mut ids: Vec<String> = rows
        .as_array()
        .unwrap_or_else(|| panic!("no rows in {body}"))
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

/// Whether the managed-object error names the users table as Chelabase's.
fn managed_refusal(err: &str) -> bool {
    err.contains("42501") && err.contains("managed by Chelabase app users")
}

// --- The bootstrap, the guard and the protect triggers -------------------------------------

/// What the users bootstrap makes, as one comparable text: the table's columns, indexes,
/// triggers, policies and grants, the event triggers, the `chelabase` functions and the config
/// row, and the sync role's attributes (not its password, whose SCRAM salt changes each time).
const SNAPSHOT_SQL: &str = "SELECT string_agg(x, E'\\n' ORDER BY x) FROM (
  SELECT 'col ' || attname || ' ' || format_type(atttypid, atttypmod) || ' ' || attnotnull AS x
    FROM pg_attribute WHERE attrelid = 'public.users'::regclass AND attnum > 0 AND NOT attisdropped
  UNION ALL SELECT 'idx ' || indexrelid::regclass::text || ' ' || pg_get_indexdef(indexrelid)
    FROM pg_index WHERE indrelid = 'public.users'::regclass
  UNION ALL SELECT 'trg ' || tgname || ' ' || tgenabled::text || ' ' || tgfoid::regprocedure::text
    FROM pg_trigger WHERE tgrelid = 'public.users'::regclass
  UNION ALL SELECT 'pol ' || polname || ' ' || pg_get_expr(polqual, polrelid)
    FROM pg_policy WHERE polrelid = 'public.users'::regclass
  UNION ALL SELECT 'acl ' || coalesce(relacl::text, '') || ' rls ' || relrowsecurity
    FROM pg_class WHERE oid = 'public.users'::regclass
  UNION ALL SELECT 'evt ' || evtname || ' ' || evtevent || ' ' || evtenabled::text FROM pg_event_trigger
  UNION ALL SELECT 'fn ' || p.oid::regprocedure::text || ' ' || md5(coalesce(prosrc, '')) || ' '
      || pg_get_userbyid(proowner) || ' ' || coalesce(proacl::text, '')
    FROM pg_proc p WHERE pronamespace = 'chelabase'::regnamespace
  UNION ALL SELECT 'cfg ' || schema_name || '.' || table_name || ' ' || (table_oid = 'public.users'::regclass)
    FROM chelabase.app_users_table
  UNION ALL SELECT 'role ' || rolname || ' ' || rolcanlogin || rolinherit || rolbypassrls || rolsuper
      || ' ' || rolconnlimit
    FROM pg_roles WHERE rolname = 'chelabase_auth_sync'
) s;";

#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn users_bootstrap_is_idempotent() {
    live(|fx| async move {
        fx.bootstrap().await.expect("first bootstrap");
        let first = fx.sql(SNAPSHOT_SQL).await;
        assert!(first.contains("trg chelabase_users_guard A"), "{first}");
        assert!(first.contains("evt chelabase_users_protect_ddl"), "{first}");
        fx.bootstrap().await.expect("second bootstrap");
        let second = fx.sql(SNAPSHOT_SQL).await;
        assert_eq!(first, second, "the second run changed something");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn guard_refuses_the_owner_and_allows_sync() {
    live(|fx| async move {
        fx.bootstrap().await.expect("bootstrap");
        fx.owner_ok("ALTER TABLE users ADD COLUMN nickname text").await;
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        fx.upsert(a, "a@example.com", None).await;

        // The owner (postgres, through the proxy).
        let refused_rows = "rows of public.users are created and deleted by Chelabase app users";
        let err = fx
            .owner_err(&format!("INSERT INTO users (id, email) VALUES ('{b}', 'b@example.com')"))
            .await;
        assert!(err.contains("42501") && err.contains(refused_rows), "insert: {err}");
        let err = fx.owner_err(&format!("DELETE FROM users WHERE id = '{a}'")).await;
        assert!(err.contains("42501") && err.contains(refused_rows), "delete: {err}");
        let err = fx.owner_err("TRUNCATE users").await;
        assert!(err.contains("42501") && err.contains(refused_rows), "truncate: {err}");
        for (col, value) in [
            ("id", format!("'{b}'")),
            ("email", "'x@example.com'".to_string()),
            ("email_verified", "false".to_string()),
            ("created_at", "now() - interval '1 day'".to_string()),
            ("deleted_at", "now()".to_string()),
            ("banned_at", "now()".to_string()),
        ] {
            let err = fx
                .owner_err(&format!("UPDATE users SET {col} = {value} WHERE id = '{a}'"))
                .await;
            let text = format!("column {col} is managed by Chelabase app users");
            assert!(err.contains("42501") && err.contains(&text), "update {col}: {err}");
        }
        fx.owner_ok(&format!("UPDATE users SET nickname = 'ann' WHERE id = '{a}'")).await;
        assert_eq!(
            fx.owner_ok(&format!("SELECT nickname || ' ' || email FROM users WHERE id = '{a}'"))
                .await,
            "ann a@example.com"
        );

        // The sync role, a real login with its password.
        let sync = |sql: String| {
            let fx = fx.clone();
            async move {
                let ep = fx.ep.clone();
                fx.sync_login(&ep, &sql)
                    .await
                    .unwrap_or_else(|e| panic!("as chelabase_auth_sync: {sql}: {e}"))
            }
        };
        sync(format!("INSERT INTO users (id, email) VALUES ('{b}', 'b@example.com')")).await;
        sync(format!(
            "UPDATE users SET email = 'b2@example.com', email_verified = true, deleted_at = now() WHERE id = '{b}'"
        ))
        .await;
        sync(format!("DELETE FROM users WHERE id = '{a}'")).await;
        assert_eq!(
            fx.value("SELECT string_agg(email, ',') FROM public.users").await,
            "b2@example.com"
        );
    })
    .await;
}

/// AC 3: `banned_at` follows the ban: set once, cleared on reactivation, readable by the user's
/// own session, and refused to the owner (`guard_refuses_the_owner_and_allows_sync`).
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn banned_at_follows_the_ban() {
    live(|fx| async move {
        fx.bootstrap().await.expect("bootstrap");
        let id = Uuid::new_v4();
        let email = "ban@example.com";
        let call = |banned: bool| upsert_call_sql(id, email, true, banned, chrono::Utc::now(), None);
        let banned_at = format!("SELECT coalesce(banned_at::text, 'null') FROM public.users WHERE id = '{id}'");

        fx.sql(&call(false)).await;
        assert_eq!(fx.value(&banned_at).await, "null");
        fx.sql(&call(true)).await;
        let first = fx.value(&banned_at).await;
        assert_ne!(first, "null");
        fx.sql(&call(true)).await;
        assert_eq!(fx.value(&banned_at).await, first, "the first ban's time is kept");

        // The user reads it on their own row, as a policy would.
        let own = format!(
            "BEGIN; SET LOCAL ROLE authenticated; \
             SELECT set_config('request.jwt.claims', '{{\"sub\":\"{id}\"}}', true); \
             SELECT 'seen ' || (banned_at IS NOT NULL) FROM public.users; COMMIT;"
        );
        let out = fx.sql(&own).await;
        assert!(out.lines().any(|l| l.trim() == "seen true"), "{out}");

        fx.sql(&call(false)).await;
        assert_eq!(fx.value(&banned_at).await, "null");

        // A row first written for a banned user is marked from the start.
        let late = Uuid::new_v4();
        fx.sql(&upsert_call_sql(late, "late@example.com", true, true, chrono::Utc::now(), None))
            .await;
        assert_eq!(
            fx.value(&format!("SELECT banned_at IS NOT NULL FROM public.users WHERE id = '{late}'"))
                .await,
            "t"
        );

        // Sign-up data can't set it.
        let data = json!({"banned_at": "2020-01-01T00:00:00Z"});
        let mine = Uuid::new_v4();
        fx.upsert(mine, "mine@example.com", Some(data)).await;
        assert_eq!(
            fx.value(&format!("SELECT banned_at IS NULL FROM public.users WHERE id = '{mine}'"))
                .await,
            "t"
        );
    })
    .await;
}

/// An existing branch, as a worker before `banned_at` left it: the check reports it, the
/// bootstrap adds the column and replaces the upsert. An app column of another type is refused
/// by name and rolls the whole script back.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn the_bootstrap_upgrades_an_existing_users_table() {
    live(|fx| async move {
        fx.bootstrap().await.expect("bootstrap");
        let id = Uuid::new_v4();
        fx.upsert(id, "old@example.com", None).await;
        let old_upsert = "chelabase.app_users_upsert(uuid, text, boolean, timestamptz, jsonb)";
        let new_upsert = "chelabase.app_users_upsert(uuid, text, boolean, boolean, timestamptz, jsonb)";
        let old_args = "p_id uuid, p_email text, p_verified boolean, p_created timestamptz, p_data jsonb";
        let downgrade = |column: &str| {
            format!(
                "BEGIN;
                 DROP EVENT TRIGGER chelabase_users_protect_drop;
                 DROP EVENT TRIGGER chelabase_users_protect_ddl;
                 ALTER TABLE public.users DROP COLUMN banned_at;
                 {column}
                 DROP FUNCTION {new_upsert};
                 CREATE FUNCTION chelabase.app_users_upsert({old_args}) RETURNS text LANGUAGE sql AS $$ SELECT 'old'::text $$;
                 COMMIT;"
            )
        };
        let says = |out: String, flag: &str| out.lines().any(|l| l.trim() == flag);
        assert!(says(fx.sql(USERS_OUTDATED_SQL).await, "f"));

        fx.sql(&downgrade("")).await;
        assert!(says(fx.sql(USERS_OUTDATED_SQL).await, "t"));
        fx.bootstrap().await.expect("the upgrade");
        assert!(says(fx.sql(USERS_OUTDATED_SQL).await, "f"));
        assert_eq!(
            fx.value(&format!("SELECT to_regprocedure('{old_upsert}') IS NULL")).await,
            "t",
            "one upsert left"
        );
        assert_eq!(fx.value(SNAPSHOT_TRIGGERS).await, "2|true|6");
        // The row survived and the new upsert marks it.
        fx.sql(&upsert_call_sql(id, "old@example.com", true, true, chrono::Utc::now(), None))
            .await;
        assert_eq!(
            fx.value(&format!("SELECT banned_at IS NOT NULL FROM public.users WHERE id = '{id}'"))
                .await,
            "t"
        );

        fx.sql(&downgrade("ALTER TABLE public.users ADD COLUMN banned_at text;")).await;
        let err = fx.bootstrap().await.expect_err("a colliding column");
        assert!(err.contains("column banned_at of"), "{err}");
        assert!(err.contains("must be timestamptz"), "{err}");
        assert_eq!(
            fx.value(&format!("SELECT to_regprocedure('{old_upsert}') IS NOT NULL")).await,
            "t",
            "rolled back"
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn protect_refuses_drop_rename_retype_disable() {
    live(|fx| async move {
        fx.bootstrap().await.expect("bootstrap");
        fx.owner_ok("CREATE SCHEMA other").await;
        for sql in [
            "DROP TABLE users",
            "ALTER TABLE users RENAME TO people",
            "ALTER TABLE users DROP COLUMN email",
            "ALTER TABLE users RENAME COLUMN email TO mail",
            "ALTER TABLE users ALTER COLUMN email TYPE varchar(320)",
            "DROP TRIGGER chelabase_users_guard ON users",
            "DROP TRIGGER chelabase_users_guard_truncate ON users",
            "ALTER TRIGGER chelabase_users_guard ON users RENAME TO guard",
            "ALTER TABLE users DISABLE TRIGGER chelabase_users_guard",
            "ALTER TABLE users DISABLE TRIGGER ALL",
            "ALTER TABLE users SET SCHEMA other",
        ] {
            let err = fx.owner_err(sql).await;
            assert!(managed_refusal(&err), "{sql}: {err}");
        }
        // The public schema itself: refused (as Chelabase's, or for want of ownership).
        let err = fx.owner_err("DROP SCHEMA public CASCADE").await;
        println!("DROP SCHEMA public CASCADE: {err}");
        assert!(err.contains("ERROR"), "{err}");
        assert_eq!(fx.value(SNAPSHOT_TRIGGERS).await, "2|true|6");
    })
    .await;
}

/// `<enabled guard triggers>|<table in place (true/false)>|<managed columns>`.
const SNAPSHOT_TRIGGERS: &str = "SELECT (SELECT count(*) FROM pg_trigger WHERE tgrelid = to_regclass('public.users') \
     AND tgname LIKE 'chelabase_users_guard%' AND tgenabled IN ('O', 'A')) || '|' || \
     (to_regclass('public.users') IS NOT NULL) || '|' || \
     (SELECT count(*) FROM pg_attribute WHERE attrelid = to_regclass('public.users') AND NOT attisdropped \
      AND attname IN ('id', 'email', 'email_verified', 'created_at', 'deleted_at', 'banned_at'));";

#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn alter_index_rename_on_the_table_is_refused() {
    live(|fx| async move {
        fx.bootstrap().await.expect("bootstrap");
        // ALTER INDEX accepts a table name too.
        let err = fx.owner_err("ALTER INDEX users RENAME TO people").await;
        assert!(managed_refusal(&err), "{err}");
        // The table's own indexes are the developer's.
        fx.owner_ok("ALTER INDEX users_pkey RENAME TO users_id_key")
            .await;
        assert_eq!(fx.value(SNAPSHOT_TRIGGERS).await, "2|true|6");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn unrelated_ddl_works_when_the_users_table_is_fine_and_when_broken() {
    live(|fx| async move {
        fx.bootstrap().await.expect("bootstrap");
        let unrelated = |n: u32| {
            format!(
                "CREATE TABLE IF NOT EXISTS other (id int);
                 ALTER TABLE other ADD COLUMN c{n} int;
                 CREATE OR REPLACE FUNCTION touch() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
                 CREATE TRIGGER t{n} BEFORE INSERT ON other FOR EACH ROW EXECUTE FUNCTION touch();"
            )
        };
        fx.owner_ok(&unrelated(1)).await;
        // Broken: the users table dropped by a superuser with event triggers off.
        fx.sql("SET event_triggers = off; DROP TABLE public.users;").await;
        assert_eq!(fx.value(SNAPSHOT_TRIGGERS).await, "0|false|0");
        fx.owner_ok(&unrelated(2)).await;
        // The bootstrap repairs it.
        fx.bootstrap().await.expect("the bootstrap after the drop");
        assert_eq!(fx.value(SNAPSHOT_TRIGGERS).await, "2|true|6");
        fx.owner_ok(&unrelated(3)).await;
        let err = fx.owner_err("DROP TABLE users").await;
        assert!(managed_refusal(&err), "{err}");
    })
    .await;
}

// --- Sync-role writes: sign-up data and marked deletes -------------------------------------

#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn sign_up_data_respects_column_insert_privilege() {
    live(|fx| async move {
        fx.bootstrap().await.expect("bootstrap");
        fx.owner_ok(
            "ALTER TABLE users ADD COLUMN display_name text, ADD COLUMN plan text NOT NULL DEFAULT 'free';
             GRANT INSERT (display_name) ON users TO authenticated;",
        )
        .await;
        let data = json!({ "display_name": "Ann", "plan": "enterprise" });
        let a = Uuid::new_v4();
        assert_eq!(fx.upsert(a, "a@example.com", Some(data.clone())).await, "inserted");
        assert_eq!(
            fx.value(&format!("SELECT display_name || '|' || plan FROM public.users WHERE id = '{a}'"))
                .await,
            "Ann|free",
            "plan isn't granted for INSERT to authenticated: it keeps its default"
        );
        // Revoked again: the column is out of sign-up data too.
        fx.owner_ok("REVOKE INSERT (display_name) ON users FROM authenticated").await;
        let b = Uuid::new_v4();
        assert_eq!(fx.upsert(b, "b@example.com", Some(data)).await, "inserted_managed");
        assert_eq!(
            fx.value(&format!(
                "SELECT coalesce(display_name, '<null>') || '|' || plan FROM public.users WHERE id = '{b}'"
            ))
            .await,
            "<null>|free"
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn mark_deleted_email_can_sign_up_again() {
    live(|fx| async move {
        fx.bootstrap().await.expect("bootstrap");
        let (old, new, dup) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        fx.upsert(old, "Same@example.com", None).await;
        let out = fx.sql(&gone_call_sql(old, true)).await;
        assert_eq!(
            first_line_matching(&out, &["changed", "unchanged"]),
            "changed"
        );
        assert_eq!(
            fx.upsert(new, "same@example.com", None).await,
            "inserted_managed"
        );
        assert_eq!(
            fx.value("SELECT count(*) || '|' || count(deleted_at) FROM public.users")
                .await,
            "2|1"
        );
        // A live row still holds the address.
        let err = fx
            .worker
            .computes
            .admin_sql(
                &fx.ep,
                &fx.database,
                &upsert_call_sql(dup, "SAME@example.com", true, false, chrono::Utc::now(), None),
            )
            .await
            .expect_err("a second live row with the same email");
        assert!(
            format!("{err:#}").contains("chelabase_users_email_key"),
            "{err:#}"
        );
    })
    .await;
}

// --- Under the data API --------------------------------------------------------------------

/// The schema's app users marker is read the same whatever the request's role: a custom role
/// with no access to schema `chelabase` still reads its tables, before and after app users
/// are enabled (the first request of a schema version introspects as its role).
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn a_custom_role_reads_through_the_data_api() {
    live(|fx| async move {
        fx.owner_ok(
            "CREATE TABLE items (id int PRIMARY KEY, label text);
             INSERT INTO items VALUES (1, 'first');
             CREATE ROLE reporter NOLOGIN;
             GRANT SELECT ON items TO reporter;",
        )
        .await;
        let key = fx.secret_key("reporter", "reporter").await;
        let select = json!({"v": 1, "op": "select", "from": "items"});
        let (status, body) = data(&key, None, select.clone()).await;
        assert_eq!(status, 200, "before app users: {body}");
        fx.bootstrap().await.expect("bootstrap");
        // A new schema version: introspected again, first by this role.
        let (status, body) = data(&key, None, select.clone()).await;
        assert_eq!(status, 200, "with app users: {body}");
        // T4: a policy written without `TO` applies to PUBLIC, the custom role included, and
        // may call the claim helpers (EXECUTE and USAGE on `chelabase` for PUBLIC).
        fx.owner_ok(
            "ALTER TABLE items ENABLE ROW LEVEL SECURITY;
             CREATE POLICY items_open ON items USING (chelabase.user_id() IS NULL OR chelabase.claims() IS NOT NULL);",
        )
        .await;
        let (status, body) = data(&key, None, select).await;
        assert_eq!(status, 200, "a PUBLIC policy calling the claim helpers: {body}");
        let rows = body.get("rows").unwrap_or(&body);
        assert_eq!(rows.as_array().map(Vec::len), Some(1), "{body}");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn helpers_work_under_the_data_api() {
    live(|fx| async move {
        fx.bootstrap().await.expect("bootstrap");
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        fx.upsert(a, "a@example.com", None).await;
        fx.upsert(b, "b@example.com", None).await;
        fx.owner_ok(
            "CREATE TABLE notes (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                 owner uuid NOT NULL DEFAULT chelabase.user_id(), email text DEFAULT chelabase.user_email(),
                 body text NOT NULL)",
        )
        .await;
        let key = fx.public_key().await;
        let token = fx.user_token(a, "a@example.com").await;
        let (status, body) = data(&key, Some(&token), json!({"v": 1, "op": "select", "from": "users"})).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(ids(&body), [a.to_string()], "the built-in chelabase_users_select_own");
        let (status, body) = data(
            &key,
            Some(&token),
            json!({"v": 1, "op": "insert", "into": "notes", "values": [{"body": "mine"}]}),
        )
        .await;
        assert!((200..300).contains(&status), "{status} {body}");
        assert_eq!(
            fx.value("SELECT owner || '|' || email FROM public.notes").await,
            format!("{a}|a@example.com"),
            "the claim helpers as column defaults"
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn new_users_table_is_rls_locked() {
    live(|fx| async move {
        fx.bootstrap().await.expect("bootstrap");
        fx.owner_ok("ALTER TABLE users ADD COLUMN nickname text").await;
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        fx.upsert(a, "a@example.com", None).await;
        fx.upsert(b, "b@example.com", None).await;
        assert_eq!(
            fx.value(
                "SELECT relrowsecurity || '|' || has_table_privilege('anon', 'public.users', 'SELECT') \
                 || '|' || (SELECT string_agg(polname, ',' ORDER BY polname) FROM pg_policy \
                 WHERE polrelid = 'public.users'::regclass) FROM pg_class WHERE oid = 'public.users'::regclass"
            )
            .await,
            "true|false|chelabase_users_select_own,chelabase_users_update_own"
        );
        let key = fx.public_key().await;
        let select = json!({"v": 1, "op": "select", "from": "users"});
        // The public key alone (anon): no grant, so the table isn't in its schema.
        let (status, body) = data(&key, None, select.clone()).await;
        assert!(matches!(status, 403 | 404), "anon: {status} {body}");
        let token = fx.user_token(a, "a@example.com").await;
        let (status, body) = data(&key, Some(&token), select).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(ids(&body), [a.to_string()]);
        // A updates A's app column; B's row isn't A's to update.
        for (id, want) in [(a, 200), (b, 200)] {
            let (status, body) = data(
                &key,
                Some(&token),
                json!({"v": 1, "op": "update", "table": "users", "set": {"nickname": "nick"},
                       "where": {"col": "id", "eq": id.to_string()}}),
            )
            .await;
            assert_eq!(status, want, "update {id}: {body}");
        }
        assert_eq!(
            fx.value("SELECT string_agg(coalesce(nickname, '-'), ',' ORDER BY email) FROM public.users")
                .await,
            "nick,-"
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn data_api_rebootstrap_keeps_app_users_objects() {
    live(|fx| async move {
        fx.bootstrap().await.expect("bootstrap");
        let before = fx.sql(SNAPSHOT_SQL).await;
        let reply = fx
            .worker
            .rebootstrap_data_api(&chelabase_nats::DataApiDatabaseRequest {
                branch_id: fx.main,
                name: fx.database.clone(),
                remove: false,
                grant_existing: false,
            })
            .await;
        assert!(reply.ok, "{reply:?}");
        assert_eq!(
            fx.value(
                "SELECT count(*) FROM pg_namespace WHERE nspname LIKE 'chelabase_quarantine%'"
            )
            .await,
            "0",
            "the data API bootstrap quarantined the chelabase schema"
        );
        assert_eq!(fx.sql(SNAPSHOT_SQL).await, before);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn remove_data_api_drops_the_helpers() {
    live(|fx| async move {
        // A second Postgres database, served by the data API (which installs the helpers).
        fx.owner_ok("CREATE DATABASE extra").await;
        let path = format!("{}/pg-databases/extra/data-api", fx.branch_path(fx.main));
        let enable = || async {
            let (status, body) = api("POST", &path, None).await;
            assert_eq!(status, 202, "enable: {body}");
            let deadline = Instant::now() + Duration::from_secs(60);
            loop {
                let (status, body) =
                    api("GET", &format!("{}/databases", fx.branch_path(fx.main)), None).await;
                assert_eq!(status, 200, "{body}");
                let row = body
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|d| d["name"] == "extra")
                    .cloned()
                    .unwrap_or(Value::Null);
                match row["data_api"].as_str() {
                    Some("on") => return,
                    Some("error") => panic!("enabling extra: {row}"),
                    _ => {}
                }
                assert!(Instant::now() < deadline, "extra still not on: {row}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        };
        let helpers = "SELECT count(*) FROM pg_proc WHERE pronamespace = to_regnamespace('chelabase') \
                       AND proname IN ('claims', 'user_id', 'user_email')";
        enable().await;
        assert_eq!(first_line(&fx.sql_in(&fx.ep, "extra", helpers).await), "3");
        let (status, body) = api("DELETE", &format!("{path}?remove=true"), None).await;
        assert_eq!(status, 204, "remove: {body}");
        assert_eq!(
            first_line(&fx.sql_in(&fx.ep, "extra", "SELECT to_regnamespace('chelabase') IS NULL").await),
            "t",
            "the chelabase schema is gone"
        );

        // A policy still using a helper: the remove fails with Postgres' dependency message.
        enable().await;
        fx.sql_in(
            &fx.ep,
            "extra",
            "CREATE TABLE public.mine (owner uuid); ALTER TABLE public.mine ENABLE ROW LEVEL SECURITY;
             CREATE POLICY mine_own ON public.mine USING (owner = chelabase.user_id());",
        )
        .await;
        let (status, body) = api("DELETE", &format!("{path}?remove=true"), None).await;
        println!("remove with a dependent policy: {status} {body}");
        assert_eq!(status, 409, "{body}");
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("cannot drop function chelabase.user_id() because other objects depend on it"),
            "the refusal names the dependency: {body}"
        );
        assert_eq!(first_line(&fx.sql_in(&fx.ep, "extra", helpers).await), "3");
    })
    .await;
}

// --- The worker's jobs with the real identity service ---------------------------------------

/// Whether the pool exists, asked as the worker.
async fn pool_exists(identity: &Identity, pool: &Pool) -> bool {
    match identity.list_users(pool, 1, 0).await {
        Ok(_) => true,
        Err(IdentityError::NotFound(Missing::Pool)) => false,
        Err(e) => panic!("listing {pool}: {e}"),
    }
}

/// AC 1, 3 and 6 live: enable creates the pool (again: a no-op), a ban revokes the user's app
/// sessions through the sync, the reconcile makes every branch's table match the pool, and
/// disable keeps a pool that still holds users (the row stays `disabling`), then drops it once
/// they are deleted.
#[tokio::test]
#[ignore = "needs the workbench stack, CHELABASE_COMPUTE_IMAGE and demo.env"]
async fn app_users_live() {
    live(|fx| async move {
        let identity = fx.worker.identity.clone().expect("the worker's identity client");
        // Users are made and banned as auth (the admin API and sign-up do it in production).
        let admin = Identity::from_env().expect("the identity service (CHELABASE_IDENTITY_URL)");
        let pool = fx.pool();
        let (dev, dev_ep) = fx.child("dev").await;
        let auth = NewProjectAuth {
            project_id: fx.project,
            database_id: fx.id,
            pg_database: &fx.database,
            schema_name: "public",
            table_name: "users",
            email_verification: false,
            on_delete: OnDelete::Delete,
        };
        assert_eq!(fx.metadata.begin_enable_app_users(&auth).await.unwrap(), BeginEnable::Started);
        fx.worker
            .handle_app_users_job(&AppUsersJob::Enable { project_id: fx.project })
            .await
            .expect("enable");
        let row = fx.metadata.project_auth(fx.project).await.unwrap().unwrap();
        assert_eq!(row.status, AppUsersStatus::Enabled, "{:?}", row.error);
        assert!(pool_exists(&identity, &pool).await, "the pool exists");
        for ep in [&fx.ep, &dev_ep] {
            assert_eq!(
                first_line(&fx.sql_in(ep, &fx.database, SNAPSHOT_TRIGGERS).await),
                "2|true|6",
                "{ep}"
            );
        }

        // I1: resync never adopts the sync role as a login role.
        fx.worker.resync(fx.main).await.expect("resync");
        let (copies, _) = fx.metadata.role_copies(&fx.ep).await.unwrap();
        assert!(
            !copies.iter().any(|c| c.name == USERS_SYNC_ROLE),
            "resync recorded {USERS_SYNC_ROLE}: {copies:?}"
        );

        // A user in the pool: the sync fans out to both branches.
        let email = format!("live-{}@example.com", &Uuid::new_v4().to_string()[..8]);
        let created = admin
            .create_user(
                &pool,
                &NewUser { email: &email, password: Some("Live-Passw0rd!"), name: None, email_verified: true },
            )
            .await
            .expect("create the user");
        let user: Uuid = created.id.parse().expect("identity user ids are uuids");
        let job = |user_id| AppUsersSyncJob {
            project_id: fx.project,
            user_id,
            data: None,
            origin_branch_id: None,
        };
        fx.worker.handle_app_users_sync(&job(user)).await.expect("sync");
        let has_row = format!("SELECT count(*) FROM public.users WHERE id = '{user}'");
        for ep in [&fx.ep, &dev_ep] {
            assert_eq!(first_line(&fx.sql_in(ep, &fx.database, &has_row).await), "1", "{ep}");
        }

        // AC 1: enabling again (the create the enable runs) is a no-op: the user stays.
        identity.create_pool(&pool).await.expect("create the pool again");
        identity.get_user(&pool, &created.id).await.expect("the user survives a second create");

        // AC 3: a ban, then the sync: the user's app sessions are revoked.
        let session = Uuid::new_v4();
        fx.metadata
            .insert_app_session(&NewAppSession {
                id: session,
                project_id: fx.project,
                user_id: user,
                identity_session_id: "app-users-live",
                refresh_hash: rand_hash(),
                expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
            })
            .await
            .expect("insert the app session");
        admin.ban(&pool, &created.id, None).await.expect("ban the user");
        fx.worker.handle_app_users_sync(&job(user)).await.expect("sync after the ban");
        assert!(
            fx.metadata.list_user_sessions(fx.project, user).await.unwrap().is_empty(),
            "the banned user's app sessions are revoked"
        );
        // AC 3: the row says so on every branch, and the unban below clears it (reconcile).
        let banned = format!("SELECT count(banned_at) FROM public.users WHERE id = '{user}'");
        for ep in [&fx.ep, &dev_ep] {
            assert_eq!(first_line(&fx.sql_in(ep, &fx.database, &banned).await), "1", "{ep}");
        }
        admin.unban(&pool, &created.id).await.expect("unban the user");

        // AC 6: a stale row, a missing row and a ghost row; the reconcile makes both branches
        // match the pool.
        let email2 = format!("live2-{}@example.com", &Uuid::new_v4().to_string()[..8]);
        let second: Uuid = admin
            .create_user(&pool, &NewUser { email: &email2, password: None, name: None, email_verified: false })
            .await
            .expect("create a second user")
            .id
            .parse()
            .unwrap();
        let ghost = Uuid::new_v4();
        for ep in [&fx.ep, &dev_ep] {
            fx.sql_in(ep, &fx.database, &upsert_call_sql(user, "stale@example.com", false, false, chrono::Utc::now(), None))
                .await;
            fx.sql_in(ep, &fx.database, &upsert_call_sql(ghost, "ghost@example.com", true, false, chrono::Utc::now(), None))
                .await;
        }
        fx.worker.reconcile_app_users(fx.project).await.expect("reconcile");
        // uuid order is byte order in Postgres and in `Uuid`'s `Ord`.
        let table = "SELECT string_agg(id || ' ' || email || ' ' || email_verified, ',' ORDER BY id) FROM public.users";
        let want = {
            let mut rows = [(user, format!("{user} {email} true")), (second, format!("{second} {email2} false"))];
            rows.sort();
            rows.map(|(_, r)| r).join(",")
        };
        for ep in [&fx.ep, &dev_ep] {
            assert_eq!(first_line(&fx.sql_in(ep, &fx.database, table).await), want, "{ep}");
            assert_eq!(first_line(&fx.sql_in(ep, &fx.database, &banned).await), "0", "{ep}");
        }

        // A new branch gets the rows (its create hook) and a fresh sync password.
        let (dev2, dev2_ep) = fx.child("dev2").await;
        assert_eq!(first_line(&fx.sql_in(&dev2_ep, &fx.database, &has_row).await), "1");
        let password = |branch: BranchId| {
            let fx = fx.clone();
            async move {
                let row = fx.metadata.branch_auth_sync(branch).await.unwrap().expect("sync row");
                open_role_password(&fx.worker.key, &branch.to_string(), USERS_SYNC_ROLE, &row.password_sealed)
                    .unwrap()
            }
        };
        let (pw_main, pw_dev, pw_dev2) = (password(fx.main).await, password(dev).await, password(dev2).await);
        assert_ne!(pw_main, pw_dev2);
        assert_ne!(pw_dev, pw_dev2);

        // Disable while the pool still holds both users (the orchestrator refuses that, so here
        // they stand for sign-ups that raced the disable): the pool is kept and the row stays
        // `disabling` with the reason.
        assert!(fx.metadata.begin_disable_app_users(fx.project).await.unwrap());
        let disable = AppUsersJob::Disable { project_id: fx.project };
        fx.worker.handle_app_users_job(&disable).await.expect("disable on hold");
        let held = fx.metadata.project_auth(fx.project).await.unwrap().expect("the row stays");
        assert_eq!(held.status, AppUsersStatus::Disabling);
        assert!(
            held.error.as_deref().is_some_and(|e| e.contains("still holds 2 user(s)")),
            "{:?}",
            held.error
        );
        assert!(pool_exists(&identity, &pool).await, "the pool is kept");

        // With the users deleted, the disable finishes: the table stays as an ordinary one
        // (RLS on, no policies), the role and the pool go.
        for id in [user, second] {
            admin.delete_user(&pool, &id.to_string()).await.expect("delete the user");
        }
        fx.worker.handle_app_users_job(&disable).await.expect("disable");
        assert!(fx.metadata.project_auth(fx.project).await.unwrap().is_none());
        for ep in [&fx.ep, &dev_ep, &dev2_ep] {
            assert_eq!(
                first_line(
                    &fx.sql_in(
                        ep,
                        &fx.database,
                        "SELECT (to_regclass('public.users') IS NOT NULL) || '|' || \
                         (SELECT count(*) FROM pg_trigger WHERE tgrelid = to_regclass('public.users') AND NOT tgisinternal) || '|' || \
                         (SELECT count(*) FROM pg_policy WHERE polrelid = to_regclass('public.users')) || '|' || \
                         (SELECT relrowsecurity FROM pg_class WHERE oid = to_regclass('public.users')) || '|' || \
                         (SELECT count(*) FROM pg_roles WHERE rolname = 'chelabase_auth_sync') || '|' || \
                         (SELECT count(*) FROM pg_event_trigger WHERE evtname LIKE 'chelabase_users%')"
                    )
                    .await
                ),
                "true|0|0|true|0|0",
                "{ep}"
            );
        }
        // An ordinary table now: the owner drops it.
        fx.owner_ok("DROP TABLE users").await;
        assert!(!pool_exists(&identity, &pool).await, "the pool is gone");
    })
    .await;
}
