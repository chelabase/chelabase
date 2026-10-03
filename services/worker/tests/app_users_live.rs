//! App users against real computes (roadmap step 6b): the users bootstrap, its guard and protect
//! triggers, the claim helpers under the data API, the data API's remove and rebootstrap paths
//! with the app-users objects, and the worker's enable, sync, branch and disable jobs with the
//! real Zitadel.
//!
//! Same setup as `roles_live` (`just dev` from this checkout, so the gateway, rest and the
//! orchestrator serve the data API, plus `psql` on the host and the proxy's CA), with
//! `demo.env` sourced for the Zitadel admin PAT and the app JWT key pair:
//!
//! ```text
//! just live bash -c 'set -a; . .cheladb/zitadel/demo.env; eval "$(scripts/neon-images.sh)"; set +a;
//!   CHELADB_COMPUTE_IMAGE=$COMPUTE_NODE_IMAGE \
//!   cargo test -p worker --test app_users_live -- --ignored --test-threads=1 --nocapture'
//! ```
//!
//! Each test provisions its own database in its own project and deletes both afterwards, pass
//! or fail (with the project's Zitadel org and users, when the test made them).

use std::future::Future;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cheladb_auth::app_token::{APP_ISSUER, AppClaims, AppTokenSigner, app_audience};
use cheladb_metadata::{
    AppUsersStatus, BeginEnable, BranchStatus, CreateBranchOutcome, DEMO_TENANT_ID, DatabaseStatus,
    MetadataClient, NewAppSession, NewBranch, NewProjectAuth, OnDelete,
};
use cheladb_nats::{AppUsersJob, AppUsersSyncJob, BranchJob};
use cheladb_neon::{ComputeBackend, ComputeLocator, DockerLocator};
use cheladb_secrets::{Purpose, open_credential, open_role_password};
use cheladb_types::{BranchId, DatabaseId, ProjectId};
use cheladb_zitadel::client::NewUser;
use serde_json::{Value, json};
use uuid::Uuid;
use worker::app_users_sql::{USERS_SYNC_ROLE, gone_call_sql, upsert_call_sql, users_bootstrap_sql};
use worker::config::RealWorker;

mod common;
use common::{cleanup, setup, workspace_root};

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
    let (worker, metadata, _config) = setup().await;
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
    if let Some(zitadel) = &fx.worker.zitadel {
        // Only the org this project made (its id is the project's): never another org.
        let _ = zitadel.delete_org(&fx.project.to_string()).await;
    }
    let _ = fx.metadata.finish_project_delete(fx.project).await;
    cleanup(&fx.worker, &fx.metadata, fx.id).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

impl Fx {
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
                    &cheladb_neon::scram::verifier(SYNC_PASSWORD),
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

    /// SQL as `cheladb_auth_sync` logged in with its password, straight to the compute's
    /// Postgres (as `auth` connects).
    async fn sync_login(&self, ep: &str, sql: &str) -> Result<String, String> {
        let locator = DockerLocator::connect("cheladb_default").expect("the Docker client");
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
                zitadel_session_id: "app-users-live",
                refresh_hash: rand_hash(),
                expires_at: now + chrono::Duration::hours(1),
            })
            .await
            .expect("insert the app session");
        let signer = AppTokenSigner::from_pem_files(
            &env_path("CHELADB_APP_JWT_PRIVATE_KEY"),
            &env_path("CHELADB_APP_JWT_PUBLIC_KEY"),
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
    let domain = std::env::var("CHELADB_PROXY_DOMAIN")
        .ok()
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| "localtest.me".into());
    format!("{ep}.{domain}")
}

fn proxy_conn(ep: &str, database: &str) -> String {
    let port: u16 = std::env::var("CHELADB_PROXY_PG_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(4432);
    let ca = workspace_root().join(".cheladb/neon/proxy-ca.crt");
    assert!(
        ca.exists(),
        "{} is missing (run just deps-up)",
        ca.display()
    );
    format!(
        "host={} hostaddr=127.0.0.1 port={port} dbname={database} sslmode=verify-full sslrootcert={}",
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
    std::env::var("GATEWAY_URL").unwrap_or_else(|_| "http://127.0.0.1:8000".into())
}

/// The built `cheladb` CLI, next to this test's own binary's directory.
fn cheladb_cli() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap();
    let cli = exe.parent().unwrap().parent().unwrap().join("cheladb");
    assert!(
        cli.exists(),
        "{} is missing (just dev builds it)",
        cli.display()
    );
    cli
}

/// A management API call through the gateway as the demo tenant's user: `(status, body)`.
async fn api(method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let token = tokio::process::Command::new(cheladb_cli())
        .args(["dev", "token"])
        .current_dir(workspace_root())
        .output()
        .await
        .expect("cheladb dev token");
    assert!(token.status.success(), "cheladb dev token failed");
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
        req = req.header("cheladb-user", token);
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

/// Whether the managed-object error names the users table as Cheladb's.
fn managed_refusal(err: &str) -> bool {
    err.contains("42501") && err.contains("managed by Cheladb app users")
}

// --- The bootstrap, the guard and the protect triggers -------------------------------------

/// What the users bootstrap makes, as one comparable text: the table's columns, indexes,
/// triggers, policies and grants, the event triggers, the `cheladb` functions and the config
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
    FROM pg_proc p WHERE pronamespace = 'cheladb'::regnamespace
  UNION ALL SELECT 'cfg ' || schema_name || '.' || table_name || ' ' || (table_oid = 'public.users'::regclass)
    FROM cheladb.app_users_table
  UNION ALL SELECT 'role ' || rolname || ' ' || rolcanlogin || rolinherit || rolbypassrls || rolsuper
      || ' ' || rolconnlimit
    FROM pg_roles WHERE rolname = 'cheladb_auth_sync'
) s;";

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
async fn users_bootstrap_is_idempotent() {
    live(|fx| async move {
        fx.bootstrap().await.expect("first bootstrap");
        let first = fx.sql(SNAPSHOT_SQL).await;
        assert!(first.contains("trg cheladb_users_guard A"), "{first}");
        assert!(first.contains("evt cheladb_users_protect_ddl"), "{first}");
        fx.bootstrap().await.expect("second bootstrap");
        let second = fx.sql(SNAPSHOT_SQL).await;
        assert_eq!(first, second, "the second run changed something");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
async fn guard_refuses_the_owner_and_allows_sync() {
    live(|fx| async move {
        fx.bootstrap().await.expect("bootstrap");
        fx.owner_ok("ALTER TABLE users ADD COLUMN nickname text").await;
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        fx.upsert(a, "a@example.com", None).await;

        // The owner (postgres, through the proxy).
        let refused_rows = "rows of public.users are created and deleted by Cheladb app users";
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
        ] {
            let err = fx
                .owner_err(&format!("UPDATE users SET {col} = {value} WHERE id = '{a}'"))
                .await;
            let text = format!("column {col} is managed by Cheladb app users");
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
                    .unwrap_or_else(|e| panic!("as cheladb_auth_sync: {sql}: {e}"))
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

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
            "DROP TRIGGER cheladb_users_guard ON users",
            "DROP TRIGGER cheladb_users_guard_truncate ON users",
            "ALTER TRIGGER cheladb_users_guard ON users RENAME TO guard",
            "ALTER TABLE users DISABLE TRIGGER cheladb_users_guard",
            "ALTER TABLE users DISABLE TRIGGER ALL",
            "ALTER TABLE users SET SCHEMA other",
        ] {
            let err = fx.owner_err(sql).await;
            assert!(managed_refusal(&err), "{sql}: {err}");
        }
        // The public schema itself: refused (as Cheladb's, or for want of ownership).
        let err = fx.owner_err("DROP SCHEMA public CASCADE").await;
        println!("DROP SCHEMA public CASCADE: {err}");
        assert!(err.contains("ERROR"), "{err}");
        assert_eq!(fx.value(SNAPSHOT_TRIGGERS).await, "2|true|5");
    })
    .await;
}

/// `<enabled guard triggers>|<table in place (true/false)>|<managed columns>`.
const SNAPSHOT_TRIGGERS: &str = "SELECT (SELECT count(*) FROM pg_trigger WHERE tgrelid = to_regclass('public.users') \
     AND tgname LIKE 'cheladb_users_guard%' AND tgenabled IN ('O', 'A')) || '|' || \
     (to_regclass('public.users') IS NOT NULL) || '|' || \
     (SELECT count(*) FROM pg_attribute WHERE attrelid = to_regclass('public.users') AND NOT attisdropped \
      AND attname IN ('id', 'email', 'email_verified', 'created_at', 'deleted_at'));";

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
async fn alter_index_rename_on_the_table_is_refused() {
    live(|fx| async move {
        fx.bootstrap().await.expect("bootstrap");
        // ALTER INDEX accepts a table name too.
        let err = fx.owner_err("ALTER INDEX users RENAME TO people").await;
        assert!(managed_refusal(&err), "{err}");
        // The table's own indexes are the developer's.
        fx.owner_ok("ALTER INDEX users_pkey RENAME TO users_id_key")
            .await;
        assert_eq!(fx.value(SNAPSHOT_TRIGGERS).await, "2|true|5");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
        assert_eq!(fx.value(SNAPSHOT_TRIGGERS).await, "2|true|5");
        fx.owner_ok(&unrelated(3)).await;
        let err = fx.owner_err("DROP TABLE users").await;
        assert!(managed_refusal(&err), "{err}");
    })
    .await;
}

// --- Sync-role writes: sign-up data and marked deletes -------------------------------------

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
                &upsert_call_sql(dup, "SAME@example.com", true, chrono::Utc::now(), None),
            )
            .await
            .expect_err("a second live row with the same email");
        assert!(
            format!("{err:#}").contains("cheladb_users_email_key"),
            "{err:#}"
        );
    })
    .await;
}

// --- Under the data API --------------------------------------------------------------------

/// The schema's app users marker is read the same whatever the request's role: a custom role
/// with no access to schema `cheladb` still reads its tables, before and after app users
/// are enabled (the first request of a schema version introspects as its role).
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
        // may call the claim helpers (EXECUTE and USAGE on `cheladb` for PUBLIC).
        fx.owner_ok(
            "ALTER TABLE items ENABLE ROW LEVEL SECURITY;
             CREATE POLICY items_open ON items USING (cheladb.user_id() IS NULL OR cheladb.claims() IS NOT NULL);",
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
async fn helpers_work_under_the_data_api() {
    live(|fx| async move {
        fx.bootstrap().await.expect("bootstrap");
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        fx.upsert(a, "a@example.com", None).await;
        fx.upsert(b, "b@example.com", None).await;
        fx.owner_ok(
            "CREATE TABLE notes (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                 owner uuid NOT NULL DEFAULT cheladb.user_id(), email text DEFAULT cheladb.user_email(),
                 body text NOT NULL)",
        )
        .await;
        let key = fx.public_key().await;
        let token = fx.user_token(a, "a@example.com").await;
        let (status, body) = data(&key, Some(&token), json!({"v": 1, "op": "select", "from": "users"})).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(ids(&body), [a.to_string()], "the built-in cheladb_users_select_own");
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
            "true|false|cheladb_users_select_own,cheladb_users_update_own"
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
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
async fn data_api_rebootstrap_keeps_app_users_objects() {
    live(|fx| async move {
        fx.bootstrap().await.expect("bootstrap");
        let before = fx.sql(SNAPSHOT_SQL).await;
        let reply = fx
            .worker
            .rebootstrap_data_api(&cheladb_nats::DataApiDatabaseRequest {
                branch_id: fx.main,
                name: fx.database.clone(),
                remove: false,
                grant_existing: false,
            })
            .await;
        assert!(reply.ok, "{reply:?}");
        assert_eq!(
            fx.value("SELECT count(*) FROM pg_namespace WHERE nspname LIKE 'cheladb_quarantine%'")
                .await,
            "0",
            "the data API bootstrap quarantined the cheladb schema"
        );
        assert_eq!(fx.sql(SNAPSHOT_SQL).await, before);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELADB_COMPUTE_IMAGE"]
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
        let helpers = "SELECT count(*) FROM pg_proc WHERE pronamespace = to_regnamespace('cheladb') \
                       AND proname IN ('claims', 'user_id', 'user_email')";
        enable().await;
        assert_eq!(first_line(&fx.sql_in(&fx.ep, "extra", helpers).await), "3");
        let (status, body) = api("DELETE", &format!("{path}?remove=true"), None).await;
        assert_eq!(status, 204, "remove: {body}");
        assert_eq!(
            first_line(&fx.sql_in(&fx.ep, "extra", "SELECT to_regnamespace('cheladb') IS NULL").await),
            "t",
            "the cheladb schema is gone"
        );

        // A policy still using a helper: the remove fails with Postgres' dependency message.
        enable().await;
        fx.sql_in(
            &fx.ep,
            "extra",
            "CREATE TABLE public.mine (owner uuid); ALTER TABLE public.mine ENABLE ROW LEVEL SECURITY;
             CREATE POLICY mine_own ON public.mine USING (owner = cheladb.user_id());",
        )
        .await;
        let (status, body) = api("DELETE", &format!("{path}?remove=true"), None).await;
        println!("remove with a dependent policy: {status} {body}");
        assert_eq!(status, 409, "{body}");
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("cannot drop function cheladb.user_id() because other objects depend on it"),
            "the refusal names the dependency: {body}"
        );
        assert_eq!(first_line(&fx.sql_in(&fx.ep, "extra", helpers).await), "3");
    })
    .await;
}

// --- The worker's jobs with the real Zitadel ------------------------------------------------

/// The org's live count in Zitadel (v2 search by id; 0 once deleted).
async fn org_count(org: &str) -> usize {
    let issuer = std::env::var("CHELADB_ZITADEL_ISSUER").expect("CHELADB_ZITADEL_ISSUER");
    let pat = std::fs::read_to_string(env_path("CHELADB_ZITADEL_ADMIN_PAT_FILE")).unwrap();
    let res: Value = reqwest::Client::new()
        .post(format!("{issuer}/v2/organizations/_search"))
        .bearer_auth(pat.trim())
        .json(&json!({"queries": [{"idQuery": {"id": org}}]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    res["result"].as_array().map_or(0, Vec::len)
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev, CHELADB_COMPUTE_IMAGE and demo.env (Zitadel admin)"]
async fn app_users_live() {
    live(|fx| async move {
        let zitadel = fx.worker.zitadel.as_ref().expect("Zitadel admin access (source demo.env)");
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
        let org = fx.project.to_string();
        assert_eq!(row.zitadel_org_id.as_deref(), Some(org.as_str()));
        assert_eq!(org_count(&org).await, 1);
        for ep in [&fx.ep, &dev_ep] {
            assert_eq!(
                first_line(&fx.sql_in(ep, &fx.database, SNAPSHOT_TRIGGERS).await),
                "2|true|5",
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

        // A user in the org: the sync fans out to both branches.
        let user = Uuid::new_v4();
        let uid = user.to_string();
        let email = format!("live-{}@example.com", &uid[..8]);
        zitadel
            .create_user(&NewUser {
                org_id: &org,
                email: &email,
                given_name: "live",
                family_name: "-",
                password: Some("Live-Passw0rd!"),
                user_id: Some(&uid),
                username: Some(&uid),
                send_code: false,
                verified: true,
                url_template: "",
            })
            .await
            .expect("create the Zitadel user");
        fx.worker
            .handle_app_users_sync(&AppUsersSyncJob {
                project_id: fx.project,
                user_id: user,
                data: None,
                origin_branch_id: None,
            })
            .await
            .expect("sync");
        let has_row = format!("SELECT count(*) FROM public.users WHERE id = '{user}'");
        for ep in [&fx.ep, &dev_ep] {
            assert_eq!(first_line(&fx.sql_in(ep, &fx.database, &has_row).await), "1", "{ep}");
        }

        // A new branch gets the row (its create hook) and a fresh sync password.
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

        // Disable: the table stays as an ordinary one (RLS on, no policies), the role and the
        // org go.
        zitadel.delete_user(&uid).await.expect("delete the user");
        assert!(fx.metadata.begin_disable_app_users(fx.project).await.unwrap());
        fx.worker
            .handle_app_users_job(&AppUsersJob::Disable { project_id: fx.project })
            .await
            .expect("disable");
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
                         (SELECT count(*) FROM pg_roles WHERE rolname = 'cheladb_auth_sync') || '|' || \
                         (SELECT count(*) FROM pg_event_trigger WHERE evtname LIKE 'cheladb_users%')"
                    )
                    .await
                ),
                "true|0|0|true|0|0",
                "{ep}"
            );
        }
        // An ordinary table now: the owner drops it.
        fx.owner_ok("DROP TABLE users").await;
        assert_eq!(org_count(&org).await, 0, "the org is gone");
    })
    .await;
}
