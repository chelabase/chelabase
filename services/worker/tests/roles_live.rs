//! Login roles against real computes (roadmap step 6a, revision 2): roles and databases managed
//! from SQL through Neon's DDL forwarding to the orchestrator's receiver, the worker's resync,
//! API roles, the protected flag, and logins through Neon's proxy (Postgres wire on
//! `CHELABASE_PROXY_PG_PORT`, default 4432, and SQL over HTTP on `CHELABASE_PROXY_HTTP_PORT`, default 4444).
//!
//! Same setup as `neon_live` (`just deps-up`, `just dev` from this checkout, so the orchestrator
//! serves `neon.console_url` and the proxy runs with `max_roles=64`, `CHELABASE_COMPUTE_IMAGE` set
//! to the fork image), plus `psql` and `curl` on the host, the built `chelabase` CLI (for API
//! tokens) and the proxy's CA in `.chelabase/neon/proxy-ca.crt`:
//!
//! ```text
//! set -a; . .chelabase/zitadel/demo.env; eval "$(scripts/neon-images.sh)"; CHELABASE_COMPUTE_IMAGE="$COMPUTE_NODE_IMAGE"; set +a
//! CHELABASE_COMPUTE_IMAGE=$COMPUTE_NODE_IMAGE \
//!   cargo test -p worker --test roles_live -- --ignored --test-threads=1 --nocapture
//! ```
//!
//! Each test provisions its own database and deletes it afterwards, pass or fail. A few tests
//! restart `just dev`'s orchestrator with extra settings (or stop it) and put it back as it was
//! afterwards, pass or fail; they need `.chelabase/dev/pids` from `just dev`.

use std::future::Future;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chelabase_metadata::{
    BranchStatus, CreateBranchOutcome, DEMO_PROJECT_ID, DEMO_TENANT_ID, DatabaseStatus,
    MetadataClient, NewBranch, RoleLevel, RoleOrigin,
};
use chelabase_nats::{BranchJob, PointInTime};
use chelabase_neon::ComputeBackend;
use chelabase_secrets::{Purpose, open_credential, open_role_password};
use chelabase_types::{BranchId, DatabaseId};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use worker::config::RealWorker;
use worker::roles::{RoleOutcome, RolePatch, RoleSpec};

mod common;
use common::{cleanup, setup, workspace_root};

/// One provisioned database: its main branch, endpoint and the owner's password.
struct Fx {
    worker: RealWorker,
    metadata: MetadataClient,
    id: DatabaseId,
    main: BranchId,
    ep: String,
    database: String,
    owner_password: String,
}

/// Provisions a database, runs `body` on it, then deletes it, pass or fail.
async fn live<F, Fut>(body: F)
where
    F: FnOnce(Arc<Fx>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (worker, metadata, _config) = setup().await;
    let id = DatabaseId::new();
    let name = format!("live-{}", &id.to_string()[..8]);
    metadata
        .insert_database_pending(id, DEMO_TENANT_ID.into(), DEMO_PROJECT_ID.into(), &name)
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
        id,
        owner_password,
    });

    // A panic inside the task is caught here, so the cleanup below always runs.
    let outcome = tokio::spawn(body(fx.clone())).await;
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

    /// The first non-empty line of a one-value query.
    async fn value(&self, ep: &str, sql: &str) -> String {
        self.sql_in(ep, "postgres", sql)
            .await
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .to_string()
    }

    /// How many roles named `role` exist on `ep`.
    async fn role_count(&self, ep: &str, role: &str) -> String {
        self.value(
            ep,
            &format!(
                "SELECT count(*) FROM pg_roles WHERE rolname = {};",
                literal(role)
            ),
        )
        .await
    }

    async fn create(&self, branch: BranchId, name: &str, owner_level: bool) {
        let started = Instant::now();
        let outcome = self.worker.create_role(branch, &RoleSpec::login(name, owner_level)).await;
        println!("create {name} took {:?}", started.elapsed());
        assert_eq!(outcome, RoleOutcome::Ok, "create_role {name}");
    }

    /// A role's revealable password (sealed in metadata).
    async fn password(&self, branch: BranchId, role: &str) -> String {
        let ep = self.endpoint_of(branch).await;
        let sealed = self
            .metadata
            .role_password(&ep, role)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("no revealable password for {role} on {ep}"));
        open_role_password(&self.worker.key, &branch.to_string(), role, &sealed)
            .expect("open the role's password")
    }

    async fn endpoint_of(&self, branch: BranchId) -> String {
        self.metadata
            .get_branch(branch)
            .await
            .unwrap()
            .expect("branch row")
            .endpoint_id
            .expect("branch endpoint")
    }

    /// The `(name, origin)` of each copy on `ep` but the owner's, sorted.
    async fn copies(&self, ep: &str) -> Vec<(String, RoleOrigin)> {
        let (copies, _) = self.metadata.role_copies(ep).await.unwrap();
        let mut rows: Vec<(String, RoleOrigin)> = copies
            .into_iter()
            .filter(|c| c.origin != RoleOrigin::Owner)
            .map(|c| (c.name, c.origin))
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    }

    /// `rolname|super|createrole|createdb|bypassrls|replication|login` of `role` on `ep`.
    async fn attributes(&self, ep: &str, role: &str) -> String {
        self.value(
            ep,
            &format!(
                "SELECT rolname || '|' || rolsuper || '|' || rolcreaterole || '|' || \
                 rolcreatedb || '|' || rolbypassrls || '|' || rolreplication || '|' || \
                 rolcanlogin FROM pg_roles WHERE rolname = '{role}';"
            ),
        )
        .await
    }

    /// `direct|any`: direct `pg_auth_members` rows with `neon_superuser`, and membership at
    /// any depth.
    async fn membership(&self, ep: &str, role: &str) -> String {
        self.value(
            ep,
            &format!(
                "SELECT (SELECT count(*) FROM pg_auth_members m JOIN pg_roles r ON r.oid = m.member \
                 WHERE r.rolname = '{role}' AND m.roleid = 'neon_superuser'::regrole) || '|' || \
                 pg_has_role('{role}', 'neon_superuser', 'MEMBER');"
            ),
        )
        .await
    }

    /// Asserts `role` exists on `ep` with no attributes but LOGIN and no `neon_superuser`.
    async fn assert_plain(&self, ep: &str, role: &str) {
        assert_eq!(
            self.attributes(ep, role).await,
            format!("{role}|false|false|false|false|false|true"),
            "{role}'s attributes"
        );
        assert_eq!(
            self.membership(ep, role).await,
            "0|false",
            "{role}'s neon_superuser membership (direct|any)"
        );
    }

    /// SQL as the branch owner through the proxy, in the main database.
    async fn owner(&self, sql: &str) -> Result<String, String> {
        psql(
            &self.ep,
            &self.database,
            "postgres",
            &self.owner_password,
            sql,
        )
        .await
    }

    /// Like [`Self::owner`], but the call must succeed.
    async fn owner_ok(&self, sql: &str) -> String {
        self.owner(sql)
            .await
            .unwrap_or_else(|e| panic!("as postgres: {sql}: {e}"))
    }

    /// Like [`Self::owner`], but the call must fail; the error.
    async fn owner_err(&self, sql: &str) -> String {
        match self.owner(sql).await {
            Ok(out) => panic!("as postgres, {sql} succeeded: {out}"),
            Err(e) => e,
        }
    }

    fn branch_path(&self, branch: BranchId) -> String {
        format!("/v1/databases/{}/branches/{branch}", self.id)
    }

    /// Makes a child of main (the worker's create job) and waits for it; its endpoint.
    async fn child(&self, name: &str) -> (BranchId, String) {
        self.child_at(name, None).await
    }

    /// [`Self::child`] at `at` (`None`: now).
    async fn child_at(
        &self,
        name: &str,
        at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> (BranchId, String) {
        let child = BranchId::new();
        let outcome = self
            .metadata
            .create_branch(&NewBranch {
                id: child,
                database_id: self.id,
                name,
                parent_branch_id: self.main,
                branched_at_time: at,
                branched_at_lsn: None,
            })
            .await
            .unwrap();
        assert_eq!(outcome, CreateBranchOutcome::Created);
        let started = Instant::now();
        self.worker
            .handle_branch_job(BranchJob::Create {
                branch_id: child,
                at: at.map(PointInTime::Timestamp),
            })
            .await
            .expect("create job");
        println!("create branch {name} took {:?}", started.elapsed());
        let record = self.metadata.get_branch(child).await.unwrap().unwrap();
        assert_eq!(record.status, BranchStatus::Ready, "{:?}", record.error);
        (child, record.endpoint_id.expect("child endpoint"))
    }

    /// A secret data key on `role` through the API; `(id, key)`.
    async fn secret_key(&self, name: &str, role: &str) -> (String, String) {
        let (status, body) = api(
            "POST",
            &format!("{}/keys", self.branch_path(self.main)),
            Some(json!({ "name": name, "role": role })),
        )
        .await;
        assert_eq!(status, 201, "create key {name} on {role}: {body}");
        (
            body["id"].as_str().unwrap().to_string(),
            body["key"].as_str().unwrap().to_string(),
        )
    }

    /// `listRoles`' row for `role` on main.
    async fn listed_role(&self, role: &str) -> Value {
        let (status, body) = api(
            "GET",
            &format!("{}/roles", self.branch_path(self.main)),
            None,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        body.as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == role)
            .cloned()
            .unwrap_or(Value::Null)
    }

    /// `GET …/databases` of main: `(name, origin)` sorted.
    async fn listed_databases(&self) -> Vec<(String, String)> {
        let (status, body) = api(
            "GET",
            &format!("{}/databases", self.branch_path(self.main)),
            None,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let mut rows: Vec<(String, String)> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|d| {
                (
                    d["name"].as_str().unwrap().to_string(),
                    d["origin"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        rows.sort();
        rows
    }
}

/// `'…'` with `'` doubled.
fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `"…"` with `"` doubled.
fn ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

fn proxy_host(ep: &str) -> String {
    let domain = std::env::var("CHELABASE_PROXY_DOMAIN")
        .ok()
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| "localtest.me".into());
    format!("{ep}.{domain}")
}

fn proxy_ca() -> String {
    let ca = workspace_root().join(".chelabase/neon/proxy-ca.crt");
    assert!(
        ca.exists(),
        "{} is missing (run just deps-up)",
        ca.display()
    );
    ca.display().to_string()
}

/// psql through the proxy (Postgres port) as `user` (passed in `PGUSER`, so any name works).
fn psql_base(ep: &str, database: &str, user: &str, password: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("psql");
    cmd.arg(format!(
        "host={} hostaddr=127.0.0.1 port={} dbname={database} sslmode=verify-full sslrootcert={}",
        proxy_host(ep),
        proxy_pg_port(),
        proxy_ca()
    ))
    .env("PGUSER", user)
    .env("PGPASSWORD", password)
    .env("PGCONNECT_TIMEOUT", "10")
    .kill_on_drop(true);
    cmd
}

fn psql_command(
    ep: &str,
    database: &str,
    user: &str,
    password: &str,
    sql: &str,
) -> tokio::process::Command {
    let mut cmd = psql_base(ep, database, user, password);
    cmd.args(["-X", "-qtA", "-v", "ON_ERROR_STOP=1", "-c", sql])
        .stdin(Stdio::null());
    cmd
}

/// `sql` (one `-c`, so one implicit transaction) through the proxy as `user`: the output, or
/// psql's error.
async fn psql(
    ep: &str,
    database: &str,
    user: &str,
    password: &str,
    sql: &str,
) -> Result<String, String> {
    let out = psql_command(ep, database, user, password, sql)
        .output()
        .await
        .expect("run psql");
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// A script on psql's stdin (autocommit, statement by statement, stop at the first error):
/// the output, or psql's error.
async fn psql_script(
    ep: &str,
    database: &str,
    user: &str,
    password: &str,
    script: &str,
) -> Result<String, String> {
    let mut child = psql_base(ep, database, user, password)
        .args(["-X", "-qtA", "-v", "ON_ERROR_STOP=1", "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn psql");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(script.as_bytes()).await.unwrap();
    drop(stdin);
    let out = child.wait_with_output().await.unwrap();
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// `sql` through the proxy's SQL over HTTP as `user`: `(status, body)`.
async fn http_sql(
    ep: &str,
    database: &str,
    user: &str,
    password: &str,
    sql: &str,
) -> (u16, String) {
    let host = proxy_host(ep);
    let http_port = proxy_http_port();
    let body = json!({ "query": sql, "params": [] }).to_string();
    let out = tokio::process::Command::new("curl")
        .args(["-sS", "--max-time", "30", "--cacert", &proxy_ca()])
        .args(["--resolve", &format!("{host}:{http_port}:127.0.0.1")])
        .args([
            "-w",
            "\n%{http_code}",
            "-X",
            "POST",
            &format!("https://{host}:{http_port}/sql"),
        ])
        .arg("-H")
        .arg(format!(
            "Neon-Connection-String: postgresql://{user}:{password}@{host}/{database}"
        ))
        .args(["-H", "content-type: application/json", "-d", &body])
        .stdin(Stdio::null())
        .output()
        .await
        .expect("run curl");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, status) = text.rsplit_once('\n').unwrap_or(("", &text));
    (status.trim().parse().unwrap_or(0), body.to_string())
}

/// Asserts the login works on the Postgres port (and on /sql for plain names).
async fn assert_logs_in(ep: &str, database: &str, user: &str, password: &str) {
    let out = psql(ep, database, user, password, "SELECT current_user")
        .await
        .unwrap_or_else(|e| panic!("{user} on the proxy: {e}"));
    assert_eq!(out, user);
    if user.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        let (status, body) =
            http_sql(ep, database, user, password, "SELECT current_user AS u").await;
        assert_eq!(status, 200, "{user} on /sql: {body}");
        assert!(body.contains(&format!("\"u\":\"{user}\"")), "{body}");
    }
}

/// Asserts the proxy refuses the password on the Postgres port (and on /sql for plain names).
async fn assert_refused(ep: &str, database: &str, user: &str, password: &str) {
    match psql(ep, database, user, password, "SELECT 1").await {
        Ok(_) => panic!("{user} logged in on the proxy with a password that should be refused"),
        Err(e) => assert!(
            e.contains(&format!("password authentication failed for user '{user}'")),
            "unexpected refusal of {user} on the proxy: {e}"
        ),
    }
    if user.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        let (status, body) = http_sql(ep, database, user, password, "SELECT 1").await;
        assert_eq!(status, 400, "{user} on /sql: {body}");
        assert!(
            body.contains("password authentication failed"),
            "{user} on /sql: {body}"
        );
    }
}

fn gateway() -> String {
    std::env::var("GATEWAY_URL").unwrap_or_else(|_| "http://127.0.0.1:8000".into())
}

fn env_port(var: &str, default: u16) -> u16 {
    std::env::var(var)
        .ok()
        .map(|v| v.parse().unwrap_or_else(|_| panic!("{var} is not a port: {v}")))
        .unwrap_or(default)
}

/// The port of a `host:port` listen address in `var`, or `default`.
fn addr_port(var: &str, default: u16) -> u16 {
    match std::env::var(var) {
        Ok(addr) => addr
            .rsplit(':')
            .next()
            .and_then(|p| p.parse().ok())
            .unwrap_or_else(|| panic!("{var} is not host:port: {addr}")),
        Err(_) => default,
    }
}

fn proxy_pg_port() -> u16 {
    env_port("CHELABASE_PROXY_PG_PORT", 4432)
}

fn proxy_http_port() -> u16 {
    env_port("CHELABASE_PROXY_HTTP_PORT", 4444)
}

fn internal_port() -> u16 {
    addr_port("CHELABASE_INTERNAL_ADDR", 8083)
}

fn orchestrator_port() -> u16 {
    addr_port("CHELABASE_ORCHESTRATOR_ADDR", 8081)
}

/// The built `chelabase` CLI, next to this test's own binary's directory.
fn chelabase_cli() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap();
    let cli = exe.parent().unwrap().parent().unwrap().join("chelabase");
    assert!(
        cli.exists(),
        "{} is missing (just dev builds it)",
        cli.display()
    );
    cli
}

/// A management API call through the gateway as the demo tenant's user: `(status, body)`.
async fn api(method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let token = tokio::process::Command::new(chelabase_cli())
        .args(["dev", "token"])
        .current_dir(workspace_root())
        .output()
        .await
        .expect("chelabase dev token");
    assert!(token.status.success(), "chelabase dev token failed");
    let token = String::from_utf8_lossy(&token.stdout).trim().to_string();
    let mut cmd = tokio::process::Command::new("curl");
    cmd.args([
        "-sS",
        "--max-time",
        "60",
        "-w",
        "\n%{http_code}",
        "-X",
        method,
    ])
    .arg(format!("{}{path}", gateway()))
    .arg("-H")
    .arg(format!("Authorization: Bearer {token}"));
    if let Some(body) = body {
        cmd.args([
            "-H",
            "content-type: application/json",
            "-d",
            &body.to_string(),
        ]);
    }
    let out = cmd.stdin(Stdio::null()).output().await.expect("run curl");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, status) = text.rsplit_once('\n').unwrap_or(("", &text));
    (
        status.trim().parse().unwrap_or(0),
        serde_json::from_str(body).unwrap_or(Value::Null),
    )
}

/// A data API select on `table` with `key`: `(status, body)`.
async fn data_select(key: &str, table: &str) -> (u16, Value) {
    let out = tokio::process::Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "30",
            "-w",
            "\n%{http_code}",
            "-X",
            "POST",
        ])
        .arg(format!("{}/data/v1/query", gateway()))
        .args(["-H", "content-type: application/json", "-H"])
        .arg(format!("Authorization: Bearer {key}"))
        .arg("-d")
        .arg(json!({ "v": 1, "op": "select", "from": table }).to_string())
        .stdin(Stdio::null())
        .output()
        .await
        .expect("run curl");
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, status) = text.rsplit_once('\n').unwrap_or(("", &text));
    (
        status.trim().parse().unwrap_or(0),
        serde_json::from_str(body).unwrap_or(Value::Null),
    )
}

/// Polls the data API with `key` until it answers `status` (at most `within`).
async fn wait_for_data_status(key: &str, table: &str, status: u16, within: Duration) -> Value {
    let deadline = Instant::now() + within;
    loop {
        let (got, body) = data_select(key, table).await;
        if got == status {
            return body;
        }
        assert!(
            Instant::now() < deadline,
            "the data API still answers {got} {body} after {within:?} (wanted {status})"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

// --- `just dev`'s orchestrator, restarted with other settings -------------------------------

/// The dev directory `just dev` writes its pids and logs to.
fn dev_dir() -> std::path::PathBuf {
    workspace_root().join(".chelabase/dev")
}

fn pids_file() -> std::path::PathBuf {
    dev_dir().join("pids")
}

async fn port_open(port: u16) -> bool {
    tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_ok()
}

/// Stops `just dev`'s orchestrator (its whole process group) and waits for its ports to close.
async fn stop_orchestrator() {
    let path = pids_file();
    let pids =
        std::fs::read_to_string(&path).expect("read the dev pids file (is just dev running?)");
    let mut found = false;
    let mut kept = Vec::new();
    for pid in pids.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let cmd = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let cmd = String::from_utf8_lossy(&cmd).replace('\0', " ");
        if cmd.contains("orchestrator") {
            found = true;
            let _ = std::process::Command::new("kill")
                .args(["--", &format!("-{pid}")])
                .status();
        } else {
            kept.push(pid);
        }
    }
    assert!(found, "no orchestrator in {}", path.display());
    std::fs::write(&path, kept.join("\n") + "\n").unwrap();
    for _ in 0..100 {
        if !port_open(internal_port()).await && !port_open(orchestrator_port()).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("the orchestrator still listens 20 s after the kill");
}

/// The variables `just dev` sources for its apps beyond `demo.env` (which the test's own
/// environment already has): `.chelabase/zitadel/actions.env` under `root`, when present.
/// `KEY=VALUE` lines; blank lines and `#` comments are skipped.
fn dev_env_files(root: &std::path::Path) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(root.join(".chelabase/zitadel/actions.env")) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.to_string()))
        .collect()
}

/// Starts the orchestrator as `just dev` does (same environment as this test, plus
/// [`dev_env_files`], plus `env`),
/// records it in the dev directory's `pids` in place of the old one, and waits for it to answer.
async fn start_orchestrator(env: &[(&str, &str)]) {
    use std::os::unix::process::CommandExt;
    let root = workspace_root();
    let log = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(dev_dir().join("orchestrator.log"))
        .unwrap();
    // Detached like `just dev`'s apps: it outlives this test, and `just dev-down` stops it.
    #[allow(clippy::zombie_processes)]
    let child = std::process::Command::new("cargo")
        .args(["run", "-q", "-p", "orchestrator"])
        .current_dir(&root)
        .envs(dev_env_files(&root))
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .process_group(0)
        .spawn()
        .expect("start the orchestrator");
    let pid = child.id().to_string();
    // First, as `just dev` writes it; `just dev-down` stops the group.
    let path = pids_file();
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    std::fs::write(&path, format!("{pid}\n{old}")).unwrap();
    for _ in 0..600 {
        if port_open(internal_port()).await && port_open(orchestrator_port()).await {
            println!("orchestrator up with {env:?}");
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("the orchestrator didn't come up within 120 s");
}

/// Runs `body` with the orchestrator restarted with `env` (`None`: stopped), then restarts it
/// plain, pass or fail.
async fn with_orchestrator<F, Fut>(env: Option<&'static [(&'static str, &'static str)]>, body: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    stop_orchestrator().await;
    if let Some(env) = env {
        start_orchestrator(env).await;
    }
    let outcome = tokio::spawn(body()).await;
    if env.is_some() {
        stop_orchestrator().await;
    }
    start_orchestrator(&[]).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

/// The restarted orchestrator gets what `just dev` sources besides the test's own environment
/// (`actions.env`: without its signing key the Zitadel webhook answers 503 until `just dev`
/// runs again).
#[test]
fn restarted_orchestrator_gets_the_dev_env_files() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join(".chelabase/zitadel")).unwrap();
    std::fs::write(
        root.path().join(".chelabase/zitadel/actions.env"),
        "# comment\n\nCHELABASE_ZITADEL_ACTIONS_SIGNING_KEY=k=1\n",
    )
    .unwrap();
    assert_eq!(
        dev_env_files(root.path()),
        vec![(
            "CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY".to_string(),
            "k=1".to_string()
        )]
    );
    let empty = tempfile::tempdir().unwrap();
    assert!(dev_env_files(empty.path()).is_empty());
}

// --- SQL-managed roles ----------------------------------------------------------------------

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn forwarding_is_off_in_admin_sql() {
    live(|fx| async move {
        assert_eq!(fx.value(&fx.ep, "SHOW neon.forward_ddl;").await, "off");
        // And on for everyone else, pointed at the receiver.
        assert_eq!(fx.owner_ok("SHOW neon.forward_ddl").await, "on");
        let url = fx.owner_ok("SHOW neon.console_url").await;
        assert!(
            url.ends_with(&format!("/compute/api/v2/computes/{}/ddl", fx.ep)),
            "{url}"
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn migration_script_runs_through_the_proxy() {
    live(|fx| async move {
        let (ep, db) = (fx.ep.clone(), fx.database.clone());
        let started = Instant::now();
        fx.owner_ok("CREATE ROLE app LOGIN PASSWORD 'app-pass-one-111'")
            .await;
        println!("CREATE ROLE took {:?}", started.elapsed());
        assert_logs_in(&ep, &db, "app", "app-pass-one-111").await;
        assert_eq!(fx.copies(&ep).await, [("app".to_string(), RoleOrigin::Sql)]);

        let started = Instant::now();
        fx.owner_ok("ALTER ROLE app PASSWORD 'app-pass-two-222'")
            .await;
        println!("ALTER ROLE … PASSWORD took {:?}", started.elapsed());
        assert_refused(&ep, &db, "app", "app-pass-one-111").await;
        assert_logs_in(&ep, &db, "app", "app-pass-two-222").await;

        // A rename clears an MD5 hash only; a SCRAM verifier keeps working under the new name.
        fx.owner_ok("ALTER ROLE app RENAME TO app2").await;
        assert_logs_in(&ep, &db, "app2", "app-pass-two-222").await;
        assert_eq!(
            fx.copies(&ep).await,
            [("app2".to_string(), RoleOrigin::Sql)]
        );

        fx.owner_ok("CREATE TABLE t (n int); INSERT INTO t VALUES (7); GRANT SELECT ON t TO app2")
            .await;
        assert_eq!(
            psql(&ep, &db, "app2", "app-pass-two-222", "SELECT n FROM t").await,
            Ok("7".into())
        );
        let started = Instant::now();
        fx.owner_ok("CREATE DATABASE extra").await;
        println!("CREATE DATABASE took {:?}", started.elapsed());
        assert_eq!(
            psql(
                &ep,
                "extra",
                "app2",
                "app-pass-two-222",
                "SELECT current_database()"
            )
            .await,
            Ok("extra".into())
        );
        assert!(
            fx.listed_databases()
                .await
                .contains(&("extra".into(), "sql".into()))
        );

        fx.owner_ok("REVOKE ALL ON t FROM app2").await;
        fx.owner_ok("DROP ROLE app2").await;
        assert_refused(&ep, &db, "app2", "app-pass-two-222").await;
        assert_eq!(fx.copies(&ep).await, []);
        assert_eq!(fx.role_count(&ep, "app2").await, "0");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn quoted_names_work() {
    live(|fx| async move {
        let name = "My \"Role\"";
        fx.owner_ok(&format!(
            "CREATE ROLE {} LOGIN PASSWORD 'quoted-pass-333'",
            ident(name)
        ))
        .await;
        assert_logs_in(&fx.ep, &fx.database, name, "quoted-pass-333").await;
        assert_eq!(
            fx.copies(&fx.ep).await,
            [(name.to_string(), RoleOrigin::Sql)]
        );

        // Reset through the worker: the old password is refused, the revealed one works.
        assert_eq!(fx.worker.reset_role(fx.main, name).await, RoleOutcome::Ok);
        let fresh = fx.password(fx.main, name).await;
        assert_logs_in(&fx.ep, &fx.database, name, &fresh).await;
        assert_refused(&fx.ep, &fx.database, name, "quoted-pass-333").await;

        // And through the API: the path parameter percent-encoded.
        let (status, body) = api(
            "GET",
            &format!(
                "{}/connection?role=My%20%22Role%22",
                fx.branch_path(fx.main)
            ),
            None,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["user"], name);
        assert!(
            body["uri"]
                .as_str()
                .unwrap()
                .starts_with("postgresql://My%20%22Role%22:"),
            "{}",
            body["uri"]
        );

        assert_eq!(fx.worker.delete_role(fx.main, name).await, RoleOutcome::Ok);
        assert_eq!(fx.role_count(&fx.ep, name).await, "0");
        assert_eq!(fx.copies(&fx.ep).await, []);
        assert_refused(&fx.ep, &fx.database, name, &fresh).await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn savepoint_and_multi_statement_transactions() {
    live(|fx| async move {
        psql_script(
            &fx.ep,
            &fx.database,
            "postgres",
            &fx.owner_password,
            "BEGIN;
             SAVEPOINT s;
             CREATE ROLE sp_role LOGIN PASSWORD 'savepoint-pass-1';
             ROLLBACK TO SAVEPOINT s;
             CREATE ROLE kept LOGIN PASSWORD 'kept-pass-12345';
             COMMIT;
             BEGIN;
             CREATE ROLE t_a LOGIN PASSWORD 'multi-pass-12345';
             ALTER ROLE t_a RENAME TO t_b;
             DROP ROLE t_b;
             COMMIT;
             DROP ROLE IF EXISTS ghost;",
        )
        .await
        .expect("the script");
        for gone in ["sp_role", "t_a", "t_b"] {
            assert_eq!(fx.role_count(&fx.ep, gone).await, "0", "{gone}");
        }
        assert_eq!(
            fx.copies(&fx.ep).await,
            [("kept".to_string(), RoleOrigin::Sql)]
        );
        assert_logs_in(&fx.ep, &fx.database, "kept", "kept-pass-12345").await;
        assert_refused(&fx.ep, &fx.database, "sp_role", "savepoint-pass-1").await;

        // A transaction that rolls back as a whole leaves no copy either.
        psql_script(
            &fx.ep,
            &fx.database,
            "postgres",
            &fx.owner_password,
            "BEGIN; CREATE ROLE rolled LOGIN PASSWORD 'rolled-pass-123'; ROLLBACK;",
        )
        .await
        .expect("the rollback script");
        assert_eq!(
            fx.copies(&fx.ep).await,
            [("kept".to_string(), RoleOrigin::Sql)]
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn restore_before_the_migration_and_rerun() {
    live(|fx| async move {
        const MIGRATION: &str = "CREATE ROLE mig LOGIN PASSWORD 'migration-pass-1'";
        tokio::time::sleep(Duration::from_secs(2)).await;
        let noted = chrono::Utc::now();
        tokio::time::sleep(Duration::from_secs(2)).await;
        fx.owner_ok(MIGRATION).await;
        assert_logs_in(&fx.ep, &fx.database, "mig", "migration-pass-1").await;
        // Past the receiver's 10 s window (R7), in which the resync leaves a fresh copy alone.
        tokio::time::sleep(Duration::from_secs(11)).await;
        // An API role made after the restore point.
        fx.create(fx.main, "apir", false).await;
        let apir = fx.password(fx.main, "apir").await;

        let moved = fx
            .metadata
            .set_branch_status(fx.main, &[BranchStatus::Ready], BranchStatus::Resetting)
            .await
            .unwrap();
        assert!(moved, "main was not ready");
        let started = Instant::now();
        fx.worker
            .handle_branch_job(BranchJob::Restore {
                branch_id: fx.main,
                backup_id: BranchId::new(),
                backup_name: "main-before-restore-20261002T120000Z".into(),
                at: PointInTime::Timestamp(noted),
            })
            .await
            .expect("restore job");
        println!("restore took {:?}", started.elapsed());
        let main = fx.metadata.get_branch(fx.main).await.unwrap().unwrap();
        assert_eq!(main.status, BranchStatus::Ready, "{:?}", main.error);

        // The SQL role is gone, catalog and copy; the API role came back.
        assert_eq!(fx.role_count(&fx.ep, "mig").await, "0");
        assert_eq!(
            fx.copies(&fx.ep).await,
            [("apir".to_string(), RoleOrigin::Api)]
        );
        assert_refused(&fx.ep, &fx.database, "mig", "migration-pass-1").await;
        assert_logs_in(&fx.ep, &fx.database, "apir", &apir).await;

        // The migration runs again (no "already exists").
        fx.owner_ok(MIGRATION).await;
        assert_logs_in(&fx.ep, &fx.database, "mig", "migration-pass-1").await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn child_keeps_passwords_owner_differs() {
    live(|fx| async move {
        fx.owner_ok("CREATE ROLE app LOGIN PASSWORD 'app-pass-child-1'")
            .await;
        fx.create(fx.main, "apir", false).await;
        let apir = fx.password(fx.main, "apir").await;
        let (child, child_ep) = fx.child("dev").await;

        assert_eq!(
            fx.copies(&child_ep).await,
            [
                ("apir".to_string(), RoleOrigin::Api),
                ("app".to_string(), RoleOrigin::Sql)
            ]
        );
        assert_logs_in(&child_ep, &fx.database, "app", "app-pass-child-1").await;
        assert_logs_in(&child_ep, &fx.database, "apir", &apir).await;
        // The child's owner has its own password.
        assert_refused(&child_ep, &fx.database, "postgres", &fx.owner_password).await;
        let record = fx.metadata.get_branch(child).await.unwrap().unwrap();
        let child_owner = open_credential(
            &fx.worker.key,
            &child.to_string(),
            Purpose::Owner,
            record
                .owner_password_enc
                .as_deref()
                .expect("child owner password"),
        )
        .unwrap();
        assert_ne!(child_owner, fx.owner_password);
        assert_logs_in(&child_ep, &fx.database, "postgres", &child_owner).await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn protected_child_gets_fresh_passwords() {
    live(|fx| async move {
        fx.owner_ok("CREATE ROLE app LOGIN PASSWORD 'app-pass-protected'")
            .await;
        fx.create(fx.main, "apir", false).await;
        let apir = fx.password(fx.main, "apir").await;
        let (status, body) = api(
            "PATCH",
            &fx.branch_path(fx.main),
            Some(json!({ "protected": true })),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let (child, child_ep) = fx.child("dev").await;

        for (role, parent_pw) in [("app", "app-pass-protected"), ("apir", apir.as_str())] {
            assert_refused(&child_ep, &fx.database, role, parent_pw).await;
            let fresh = fx.password(child, role).await;
            assert_ne!(fresh, parent_pw, "{role}");
            assert_logs_in(&child_ep, &fx.database, role, &fresh).await;
        }
        // The parent keeps its own.
        assert_logs_in(&fx.ep, &fx.database, "app", "app-pass-protected").await;
        assert_logs_in(&fx.ep, &fx.database, "apir", &apir).await;

        // A protected child can't be deleted until it's unprotected.
        let path = format!("/v1/databases/{}/branches/{child}", fx.id);
        let (status, _) = api("PATCH", &path, Some(json!({ "protected": true }))).await;
        assert_eq!(status, 200);
        let (status, body) = api("DELETE", &path, None).await;
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["error"]["code"], "branch_protected", "{body}");
        let (status, _) = api("PATCH", &path, Some(json!({ "protected": false }))).await;
        assert_eq!(status, 200);
    })
    .await;
}

// I3: a child of a protected parent at a point before a role was dropped on the parent has
// that role in its catalog, though the parent's copies don't: it gets a fresh password too, so
// the old one is refused on the child. A role without LOGIN that has a password gets one too.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn protected_child_at_a_past_point_refuses_a_dropped_roles_password() {
    live(|fx| async move {
        fx.owner_ok("CREATE ROLE gone LOGIN PASSWORD 'gone-pass-12345'")
            .await;
        fx.owner_ok("CREATE ROLE grp PASSWORD 'grp-pass-12345'").await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let noted = chrono::Utc::now();
        tokio::time::sleep(Duration::from_secs(2)).await;
        fx.owner_ok("DROP ROLE gone").await;
        fx.owner_ok("DROP ROLE grp").await;
        assert_eq!(fx.copies(&fx.ep).await, []);
        let (status, body) = api(
            "PATCH",
            &fx.branch_path(fx.main),
            Some(json!({ "protected": true })),
        )
        .await;
        assert_eq!(status, 200, "{body}");

        let (child, child_ep) = fx.child_at("past", Some(noted)).await;

        assert_eq!(fx.role_count(&child_ep, "gone").await, "1");
        assert_refused(&child_ep, &fx.database, "gone", "gone-pass-12345").await;
        let fresh = fx.password(child, "gone").await;
        assert_ne!(fresh, "gone-pass-12345");
        assert_logs_in(&child_ep, &fx.database, "gone", &fresh).await;
        assert_eq!(
            fx.copies(&child_ep).await,
            [
                ("gone".to_string(), RoleOrigin::Sql),
                ("grp".to_string(), RoleOrigin::Sql)
            ]
        );
        let grp = fx.password(child, "grp").await;
        assert_ne!(grp, "grp-pass-12345");
        // Unprotected again, so the cleanup can delete it.
        let (status, _) = api(
            "PATCH",
            &fx.branch_path(fx.main),
            Some(json!({ "protected": false })),
        )
        .await;
        assert_eq!(status, 200);
    })
    .await;
}

// M7, revised in round 2 (user, 2026-10-08): an empty password means no password, as in plain
// Postgres (`PASSWORD ''` is taken as PASSWORD NULL, with a notice). The role is made without a
// password, so it has no copy and can't log in through the proxy; `ALTER ROLE … PASSWORD ''`
// clears a password and removes the copy. Setting a password again restores the login.
// (Refusing '' explicitly in the fork is parked until the next fork work.)
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn empty_password_means_no_password() {
    live(|fx| async move {
        fx.owner_ok("CREATE ROLE blank LOGIN PASSWORD ''").await;
        assert_eq!(fx.role_count(&fx.ep, "blank").await, "1");
        assert_eq!(fx.copies(&fx.ep).await, []);

        fx.owner_ok("CREATE ROLE later LOGIN PASSWORD 'later-pass-12345'")
            .await;
        assert_logs_in(&fx.ep, &fx.database, "later", "later-pass-12345").await;
        fx.owner_ok("ALTER ROLE later PASSWORD ''").await;
        assert_eq!(fx.copies(&fx.ep).await, []);
        assert!(
            psql(&fx.ep, &fx.database, "later", "later-pass-12345", "SELECT 1")
                .await
                .is_err(),
            "a cleared password must not log in"
        );

        fx.owner_ok("ALTER ROLE later PASSWORD 'later-pass-67890'")
            .await;
        assert_eq!(fx.copies(&fx.ep).await, [("later".to_string(), RoleOrigin::Sql)]);
        assert_logs_in(&fx.ep, &fx.database, "later", "later-pass-67890").await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn md5_refused() {
    live(|fx| async move {
        // `md5` || md5(password || user), as old pg_dumpall output has it.
        let out = std::process::Command::new("bash")
            .args(["-c", "printf %s 'md5-pass-12345m5' | md5sum | cut -c1-32"])
            .output()
            .unwrap();
        let hash = format!("md5{}", String::from_utf8_lossy(&out.stdout).trim());
        let err = fx
            .owner_err(&format!("CREATE ROLE m5 LOGIN PASSWORD '{hash}'"))
            .await;
        assert!(
            err.contains("MD5 passwords can't be used through Chelabase; set a plain password"),
            "{err}"
        );
        assert_eq!(fx.role_count(&fx.ep, "m5").await, "0");
        assert_eq!(fx.copies(&fx.ep).await, []);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn pg_dumpall_style_scram_accepted() {
    live(|fx| async move {
        let verifier = chelabase_neon::scram::verifier("dumped-pass-12345");
        // As pg_dumpall writes it: CREATE ROLE, then ALTER ROLE … PASSWORD '<SCRAM verifier>'.
        // (Without its NOSUPERUSER: Postgres lets only a superuser name that attribute.)
        psql_script(
            &fx.ep,
            &fx.database,
            "postgres",
            &fx.owner_password,
            &format!(
                "CREATE ROLE dumped;\n\
                 ALTER ROLE dumped WITH INHERIT NOCREATEROLE NOCREATEDB LOGIN \
                 NOREPLICATION NOBYPASSRLS PASSWORD '{verifier}';\n"
            ),
        )
        .await
        .expect("the pg_dumpall lines");
        assert_logs_in(&fx.ep, &fx.database, "dumped", "dumped-pass-12345").await;
        let row = fx.listed_role("dumped").await;
        assert_eq!(row["origin"], "sql", "{row}");
        assert_eq!(row["password"], "set_in_sql", "{row}");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn weak_password_with_strength_on() {
    live(|fx| async move {
        with_orchestrator(
            Some(&[("CHELABASE_PASSWORD_STRENGTH", "on")]),
            move || async move {
                let err = fx
                    .owner_err("CREATE ROLE weak LOGIN PASSWORD 'abcdefgh123'")
                    .await;
                assert!(
                    err.contains(
                        "password too weak: use at least 12 characters and mix letters, digits \
                     and symbols (about 60 bits)"
                    ),
                    "{err}"
                );
                assert!(
                    !err.contains("abcdefgh123"),
                    "the refusal echoes the password: {err}"
                );
                assert_eq!(fx.role_count(&fx.ep, "weak").await, "0");
                // A strong one, and a pre-hashed one (self-hosted, Q6), pass.
                fx.owner_ok("CREATE ROLE strong LOGIN PASSWORD 'Tr0ub4dor&3-Horse'")
                    .await;
                assert_logs_in(&fx.ep, &fx.database, "strong", "Tr0ub4dor&3-Horse").await;
                let verifier = chelabase_neon::scram::verifier("short");
                fx.owner_ok(&format!("CREATE ROLE hashed LOGIN PASSWORD '{verifier}'"))
                    .await;
                assert_logs_in(&fx.ep, &fx.database, "hashed", "short").await;
            },
        )
        .await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn key_role_drop_refused() {
    live(|fx| async move {
        fx.owner_ok("CREATE ROLE kr LOGIN PASSWORD 'key-role-pass-1'")
            .await;
        let (key_id, _) = fx.secret_key("k1", "kr").await;
        let err = fx.owner_err("DROP ROLE kr").await;
        assert!(
            err.contains("role \"kr\" backs data API key(s) k1; revoke them first"),
            "{err}"
        );
        assert_eq!(fx.role_count(&fx.ep, "kr").await, "1");
        assert_logs_in(&fx.ep, &fx.database, "kr", "key-role-pass-1").await;
        // Once the key is revoked, the drop goes through.
        let (status, body) = api(
            "DELETE",
            &format!("{}/keys/{key_id}", fx.branch_path(fx.main)),
            None,
        )
        .await;
        assert_eq!(status, 204, "{body}");
        fx.owner_ok("DROP ROLE kr").await;
        assert_eq!(fx.copies(&fx.ep).await, []);
    })
    .await;
}

// C1: the usual role behind a secret key is NOLOGIN without a password, so it has no
// `neon_roles` row. Create and rotate check it on the compute; the key works; a drop is
// refused while it lives; and a just-dropped name is refused a key.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn nologin_role_backs_a_secret_key() {
    live(|fx| async move {
        fx.owner_ok(
            "CREATE TABLE notes (id int); INSERT INTO notes VALUES (1); \
             CREATE ROLE api_reader NOLOGIN; GRANT SELECT ON notes TO api_reader",
        )
        .await;
        assert_eq!(fx.copies(&fx.ep).await, [], "no row for a role without a password");
        let (key_id, key) = fx.secret_key("reader-key", "api_reader").await;
        let body = wait_for_data_status(&key, "notes", 200, Duration::from_secs(30)).await;
        assert!(body.to_string().contains('1'), "{body}");

        let (status, body) = api(
            "POST",
            &format!("{}/keys/{key_id}/rotate", fx.branch_path(fx.main)),
            None,
        )
        .await;
        assert_eq!(status, 201, "rotate: {body}");
        assert_eq!(body["role"], "api_reader");
        let rotated = body["key"].as_str().unwrap().to_string();
        wait_for_data_status(&rotated, "notes", 200, Duration::from_secs(30)).await;
        wait_for_data_status(&key, "notes", 401, Duration::from_secs(40)).await;

        // REVOKE first, in the same transaction: Postgres refuses to drop a role that holds a
        // grant before anything is forwarded. The refusal rolls the REVOKE back too.
        let err = fx
            .owner_err("BEGIN; REVOKE ALL ON notes FROM api_reader; DROP ROLE api_reader; COMMIT;")
            .await;
        assert!(
            err.contains("role \"api_reader\" backs data API key(s) reader-key; revoke them first"),
            "{err}"
        );

        // The key is bound to the role's OID.
        let oid: i64 = sqlx::query_scalar("SELECT role_oid FROM data_api_keys WHERE id = $1::uuid")
            .bind(body["id"].as_str().unwrap())
            .fetch_one(fx.metadata.pool())
            .await
            .unwrap();
        assert_eq!(
            fx.value(&fx.ep, "SELECT oid FROM pg_roles WHERE rolname = 'api_reader';").await,
            oid.to_string()
        );
        // A role that's gone is refused a key.
        fx.owner_ok("CREATE ROLE gone_reader NOLOGIN; DROP ROLE gone_reader").await;
        let (status, body) = api(
            "POST",
            &format!("{}/keys", fx.branch_path(fx.main)),
            Some(json!({ "name": "late", "role": "gone_reader" })),
        )
        .await;
        assert_eq!(status, 422, "{body}");
        assert_eq!(body["error"]["code"], "role_refused", "{body}");
    })
    .await;
}

// A key's role dropped and created again under the same name where the DDL receiver can't
// refuse it (as cloud_admin, forwarding off: the late commit no check sees). The key is
// bound to the old OID: the data API refuses it at once, and the resync revokes it. A key
// made for the new role works.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn recreated_role_kills_its_key_and_the_resync_revokes_it() {
    live(|fx| async move {
        fx.owner_ok(
            "CREATE TABLE notes (id int); INSERT INTO notes VALUES (1); \
             CREATE ROLE twin NOLOGIN; GRANT SELECT ON notes TO twin",
        )
        .await;
        let (old_id, old_key) = fx.secret_key("old", "twin").await;
        wait_for_data_status(&old_key, "notes", 200, Duration::from_secs(30)).await;

        fx.sql(
            "DROP OWNED BY twin; DROP ROLE twin; CREATE ROLE twin NOLOGIN; \
             GRANT SELECT ON notes TO twin; GRANT twin TO authenticator;",
        )
        .await;
        // At once: the gateway's key cache still has the key, the preamble refuses the OID.
        let (status, body) = data_select(&old_key, "notes").await;
        assert_eq!(status, 503, "{body}");

        fx.worker.resync(fx.main).await.expect("resync");
        let revoked: bool =
            sqlx::query_scalar("SELECT revoked_at IS NOT NULL FROM data_api_keys WHERE id = $1::uuid")
                .bind(&old_id)
                .fetch_one(fx.metadata.pool())
                .await
                .unwrap();
        assert!(revoked, "the resync revoked the dead key");
        wait_for_data_status(&old_key, "notes", 401, Duration::from_secs(40)).await;

        let (_, new_key) = fx.secret_key("new", "twin").await;
        wait_for_data_status(&new_key, "notes", 200, Duration::from_secs(30)).await;
    })
    .await;
}

// Re-review: Neon sends `DROP ROLE kr; ALTER ROLE other RENAME TO kr` as the rename alone; a
// live key on `kr` still refuses it, with the DROP's text, and nothing changes.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn drop_then_rename_onto_a_key_role_refused() {
    live(|fx| async move {
        fx.owner_ok("CREATE ROLE kr LOGIN PASSWORD 'key-role-pass-1'")
            .await;
        fx.owner_ok("CREATE ROLE other LOGIN PASSWORD 'other-pass-1'")
            .await;
        fx.secret_key("k1", "kr").await;
        let err = fx
            .owner_err("BEGIN; DROP ROLE kr; ALTER ROLE other RENAME TO kr; COMMIT;")
            .await;
        assert!(
            err.contains("role \"kr\" backs data API key(s) k1; revoke them first"),
            "{err}"
        );
        assert_eq!(fx.role_count(&fx.ep, "kr").await, "1");
        assert_eq!(fx.role_count(&fx.ep, "other").await, "1");
        assert_logs_in(&fx.ep, &fx.database, "kr", "key-role-pass-1").await;
        assert_logs_in(&fx.ep, &fx.database, "other", "other-pass-1").await;
        assert_eq!(
            fx.copies(&fx.ep).await,
            [
                ("kr".to_string(), RoleOrigin::Sql),
                ("other".to_string(), RoleOrigin::Sql)
            ]
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn control_plane_down_fails_the_ddl() {
    live(|fx| async move {
        // The owner's session is opened first: the proxy needs the control plane to log in.
        let mut session = psql_base(&fx.ep, &fx.database, "postgres", &fx.owner_password)
            .args(["-X", "-qtA", "-v", "ON_ERROR_STOP=0", "-f", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn psql");
        let mut stdin = session.stdin.take().unwrap();
        let mut stdout = BufReader::new(session.stdout.take().unwrap()).lines();
        stdin
            .write_all(b"\\set VERBOSITY verbose\nSELECT 'ready';\n")
            .await
            .unwrap();
        stdin.flush().await.unwrap();
        let first = tokio::time::timeout(Duration::from_secs(20), stdout.next_line())
            .await
            .expect("psql answered")
            .unwrap();
        assert_eq!(first.as_deref(), Some("ready"));

        let fx2 = fx.clone();
        with_orchestrator(None, move || async move {
            let started = Instant::now();
            stdin
                .write_all(
                    b"CREATE ROLE down_role LOGIN PASSWORD 'down-pass-12345';\n\
                      CREATE TABLE still_works (n int);\n\
                      SELECT 'done';\n",
                )
                .await
                .unwrap();
            drop(stdin);
            let out = tokio::time::timeout(Duration::from_secs(60), session.wait_with_output())
                .await
                .expect("psql finished")
                .unwrap();
            println!("the refused DDL took {:?}", started.elapsed());
            let stderr = String::from_utf8_lossy(&out.stderr);
            // The fork's F2: a clean message and SQLSTATE 55000, no internal address.
            assert!(
                stderr.contains(
                    "ERROR:  55000: role and database changes are unavailable right now; try again"
                ),
                "unexpected error: {stderr}"
            );
            for leak in ["curl", "host.docker.internal", "control plane", "HTTP"] {
                assert!(!stderr.contains(leak), "the error shows {leak:?}: {stderr}");
            }
            assert_eq!(fx2.role_count(&fx2.ep, "down_role").await, "0");
            // Non-role DDL is unaffected.
            assert_eq!(
                fx2.sql("SELECT count(*) FROM pg_tables WHERE tablename = 'still_works';")
                    .await
                    .trim(),
                "1"
            );
        })
        .await;
        assert_eq!(fx.copies(&fx.ep).await, []);
        // With the control plane back, the same DDL works.
        fx.owner_ok("CREATE ROLE down_role LOGIN PASSWORD 'down-pass-12345'")
            .await;
        assert_logs_in(&fx.ep, &fx.database, "down_role", "down-pass-12345").await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn forwarding_off_then_resync_picks_up() {
    live(|fx| async move {
        fx.sql("CREATE ROLE quiet LOGIN PASSWORD 'quiet-pass-12345';")
            .await;
        assert_eq!(fx.copies(&fx.ep).await, [], "admin_sql forwarded");
        assert_refused(&fx.ep, &fx.database, "quiet", "quiet-pass-12345").await;
        let started = Instant::now();
        fx.worker.resync(fx.main).await.expect("resync");
        println!("resync took {:?}", started.elapsed());
        assert_eq!(
            fx.copies(&fx.ep).await,
            [("quiet".to_string(), RoleOrigin::Sql)]
        );
        assert_logs_in(&fx.ep, &fx.database, "quiet", "quiet-pass-12345").await;
    })
    .await;
}

// T12b: LOGIN and NOLOGIN reach the control plane in the statement's own transaction.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn alter_role_nologin_blocks_the_proxy_login_at_once() {
    live(|fx| async move {
        fx.owner_ok("CREATE ROLE nl_role LOGIN PASSWORD 'nl-pass-12345'")
            .await;
        assert_logs_in(&fx.ep, &fx.database, "nl_role", "nl-pass-12345").await;
        assert_eq!(fx.listed_role("nl_role").await["login"], json!(true));

        fx.owner_ok("ALTER ROLE nl_role NOLOGIN").await;
        // Right away, no waiting for a resync.
        if let Ok(out) = psql(&fx.ep, &fx.database, "nl_role", "nl-pass-12345", "SELECT 1").await {
            panic!("nl_role logged in after NOLOGIN: {out}");
        }
        assert_eq!(fx.listed_role("nl_role").await["login"], json!(false));

        fx.owner_ok("ALTER ROLE nl_role LOGIN").await;
        assert_logs_in(&fx.ep, &fx.database, "nl_role", "nl-pass-12345").await;
        assert_eq!(fx.listed_role("nl_role").await["login"], json!(true));
    })
    .await;
}

// T13d (re-review I1): `ALTER ROLE … VALID UNTIL` alone is an attribute change; it must not
// remove an API role's copy (login, revealable password). The owner can't set its own VALID
// UNTIL (Postgres requires CREATEROLE and ADMIN on the role), so the owner case is covered by
// `ddl_valid_until_alone_is_an_attribute_change` in the orchestrator's internal tests.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn alter_role_valid_until_keeps_the_copy() {
    live(|fx| async move {
        fx.create(fx.main, "api_vu", false).await;
        let password = fx.password(fx.main, "api_vu").await;
        assert_logs_in(&fx.ep, &fx.database, "api_vu", &password).await;

        fx.owner_ok("ALTER ROLE api_vu VALID UNTIL 'infinity'").await;
        assert_logs_in(&fx.ep, &fx.database, "api_vu", &password).await;
        assert_eq!(fx.password(fx.main, "api_vu").await, password, "still revealable");
        assert_eq!(fx.listed_role("api_vu").await["login"], json!(true));
        assert_eq!(
            fx.copies(&fx.ep).await,
            [("api_vu".to_string(), RoleOrigin::Api)]
        );
    })
    .await;
}

// T13d (D20): a role dropped and created again in one transaction is refused while a secret
// data key uses it (the key keeps working), and goes through when none does. Needs a fork
// build that sends `recreated`.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn drop_and_create_in_one_transaction_is_refused_while_a_key_uses_the_role() {
    live(|fx| async move {
        let setup = "CREATE TABLE notes (id int PRIMARY KEY, body text);\n\
                     INSERT INTO notes VALUES (1, 'n');\n\
                     CREATE ROLE app LOGIN PASSWORD 'app-pass-12345';\n\
                     GRANT SELECT ON notes TO app;\n";
        psql_script(&fx.ep, &fx.database, "postgres", &fx.owner_password, setup)
            .await
            .expect("setup");
        let (_, key) = fx.secret_key("k1", "app").await;
        let body = wait_for_data_status(&key, "notes", 200, Duration::from_secs(30)).await;
        assert_eq!(body, json!([{ "id": 1, "body": "n" }]));

        // The REVOKE comes first: Postgres refuses to drop a role that still holds a grant,
        // before the fork forwards anything. The refusal rolls the transaction back, so the
        // grant (and the key) survive.
        let err = fx
            .owner_err(
                "BEGIN; REVOKE ALL ON notes FROM app; DROP ROLE app; \
                 CREATE USER app PASSWORD 'recreated-pass-1'; COMMIT;",
            )
            .await;
        assert!(
            err.contains("role \"app\" backs data API key(s) k1; revoke them first"),
            "{err}"
        );
        assert_eq!(fx.role_count(&fx.ep, "app").await, "1");
        assert_logs_in(&fx.ep, &fx.database, "app", "app-pass-12345").await;
        assert_eq!(data_select(&key, "notes").await.0, 200, "the key still works");

        // No live key on `other`: the same statement goes through.
        fx.owner_ok("CREATE ROLE other LOGIN PASSWORD 'other-pass-1234'")
            .await;
        fx.owner_ok("BEGIN; DROP ROLE other; CREATE USER other PASSWORD 'recreated-pass-1'; COMMIT;")
            .await;
        assert_logs_in(&fx.ep, &fx.database, "other", "recreated-pass-1").await;
    })
    .await;
}

// T12b: a GRANT (nothing the receiver can mirror) is read back by a resync a few seconds
// later, not at reconcile's next pass (every 30 s).
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn grant_shows_the_derived_flag_within_seconds() {
    live(|fx| async move {
        fx.create(fx.main, "api_admin", true).await;
        // Create, then grant within milliseconds: the copy's `forwarded_at` is fresh when the
        // touched resync runs, and its level must still update.
        let granted = Instant::now();
        psql_script(
            &fx.ep,
            &fx.database,
            "postgres",
            &fx.owner_password,
            "CREATE ROLE by_grant LOGIN PASSWORD 'by-grant-pass-123';\n\
             GRANT api_admin TO by_grant;\n",
        )
        .await
        .expect("create and grant");
        let deadline = Instant::now() + Duration::from_secs(6);
        let row = loop {
            let row = fx.listed_role("by_grant").await;
            if row["level"] == "owner_level" || Instant::now() >= deadline {
                break row;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        };
        println!(
            "by_grant: listed as owner-level after {:?}",
            granted.elapsed()
        );
        assert_eq!(row["level"], "owner_level", "{row}");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn owner_level_by_sql_blocks_the_key() {
    live(|fx| async move {
        fx.create(fx.main, "api_admin", true).await;
        let roles = ["by_bypassrls", "by_createdb", "by_membership"];
        let mut setup = String::from("CREATE TABLE notes (id int PRIMARY KEY, body text);\n");
        setup.push_str("INSERT INTO notes VALUES (1, 'n');\n");
        for r in roles {
            setup.push_str(&format!(
                "CREATE ROLE {r} LOGIN PASSWORD '{r}-pass-123';\nGRANT SELECT ON notes TO {r};\n"
            ));
        }
        psql_script(&fx.ep, &fx.database, "postgres", &fx.owner_password, &setup)
            .await
            .expect("setup");
        let mut keys = Vec::new();
        for r in roles {
            let (id, key) = fx.secret_key(&format!("key-{r}"), r).await;
            // A first read: rest caches the role as plain.
            let body = wait_for_data_status(&key, "notes", 200, Duration::from_secs(30)).await;
            assert_eq!(body, json!([{ "id": 1, "body": "n" }]));
            keys.push((r, id, key));
        }
        let changed = Instant::now();
        psql_script(
            &fx.ep,
            &fx.database,
            "postgres",
            &fx.owner_password,
            "ALTER ROLE by_bypassrls BYPASSRLS;\n\
             ALTER ROLE by_createdb CREATEDB;\n\
             GRANT api_admin TO by_membership;\n",
        )
        .await
        .expect("make them owner-level");
        // E33: BYPASSRLS alone is allowed; the other two are owner-level now.
        let (bypass, owner_level) = keys.split_at(1);
        for (r, _, key) in owner_level {
            let body = wait_for_data_status(key, "notes", 403, Duration::from_secs(35)).await;
            assert_eq!(body["error"]["code"], "role_not_allowed", "{r}: {body}");
            println!("{r}: 403 after {:?}", changed.elapsed());
        }
        let (_, bypass_id, bypass_key) = &bypass[0];
        let body = wait_for_data_status(bypass_key, "notes", 200, Duration::from_secs(5)).await;
        assert_eq!(body, json!([{ "id": 1, "body": "n" }]));
        // The key list marks the BYPASSRLS key once the resync has read it.
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let (status, listed) = api("GET", &format!("{}/keys", fx.branch_path(fx.main)), None).await;
            assert_eq!(status, 200, "{listed}");
            let key = listed.as_array().unwrap().iter().find(|k| k["id"] == json!(bypass_id)).cloned().unwrap();
            if key["bypasses_rls"] == true || Instant::now() >= deadline {
                assert_eq!(key["bypasses_rls"], true, "{key}");
                assert_eq!(key["role_blocked"], false, "{key}");
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        // listRoles' level comes from the resync (these ALTERs forward nothing to record):
        // reconcile's next pass, every 30 s.
        assert_eq!(fx.listed_role("by_bypassrls").await["level"], "plain");
        for (r, id, _) in owner_level {
            let deadline = Instant::now() + Duration::from_secs(60);
            let row = loop {
                let row = fx.listed_role(r).await;
                if row["level"] == "owner_level" || Instant::now() >= deadline {
                    break row;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            };
            println!("{r}: listed as owner-level after {:?}", changed.elapsed());
            assert_eq!(row["level"], "owner_level", "{r}: {row}");
            assert_eq!(
                row["blocked_keys"],
                json!([{ "id": id, "name": format!("key-{r}") }]),
                "{r}: {row}"
            );
        }
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn database_cap_refusal_leaves_no_database() {
    live(|fx| async move {
        with_orchestrator(
            Some(&[("CHELABASE_BRANCH_DATABASE_CAP", "2")]),
            move || async move {
                // main + one: the system `postgres` database isn't counted.
                fx.owner_ok("CREATE DATABASE second").await;
                let err = fx.owner_err("CREATE DATABASE third").await;
                assert!(
                    err.contains(
                        "this branch has reached its limit of 2 databases \
                     (CHELABASE_BRANCH_DATABASE_CAP)"
                    ),
                    "{err}"
                );
                assert_eq!(
                    fx.value(
                        &fx.ep,
                        "SELECT count(*) FROM pg_database WHERE datname = 'third';"
                    )
                    .await,
                    "0"
                );
                let names: Vec<String> = fx
                    .listed_databases()
                    .await
                    .into_iter()
                    .map(|(n, _)| n)
                    .collect();
                assert!(!names.contains(&"third".to_string()), "{names:?}");
                assert!(names.contains(&"second".to_string()), "{names:?}");
            },
        )
        .await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn owner_can_use_roles_it_creates() {
    live(|fx| async move {
        // Q9: `createrole_self_grant = 'set, inherit'`. (`ALTER … OWNER TO` also needs the new
        // owner to have CREATE on the schema: Postgres 15+'s rule, nothing of ours.)
        let out = psql_script(
            &fx.ep,
            &fx.database,
            "postgres",
            &fx.owner_password,
            "CREATE TABLE t (n int);
             CREATE ROLE app LOGIN PASSWORD 'app-pass-q9-1234';
             GRANT CREATE ON SCHEMA public TO app;
             ALTER TABLE t OWNER TO app;
             SET ROLE app;
             SELECT current_user;
             RESET ROLE;
             SELECT tableowner FROM pg_tables WHERE tablename = 't';",
        )
        .await
        .expect("the owner uses its new role");
        assert_eq!(out, "app\napp");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn main_database_drop_is_refused() {
    live(|fx| async move {
        let main = fx.database.clone();
        // A key to watch the data API with.
        fx.owner_ok(
            "CREATE TABLE notes (id int); CREATE ROLE reader LOGIN PASSWORD 'reader-pass-123'; \
             GRANT SELECT ON notes TO reader",
        )
        .await;
        let (_, key) = fx.secret_key("watch", "reader").await;
        wait_for_data_status(&key, "notes", 200, Duration::from_secs(30)).await;

        // The fork refuses each form before it runs (neon.protected_databases), even FORCE,
        // which would otherwise end the data API's pooled session.
        let refused =
            format!("ERROR:  42501: the branch's main database \"{main}\" can't be dropped");
        for drop in [
            format!("DROP DATABASE {}", ident(&main)),
            format!("DROP DATABASE IF EXISTS {}", ident(&main)),
            format!("DROP DATABASE {} WITH (FORCE)", ident(&main)),
        ] {
            let started = Instant::now();
            let err = psql_script(
                &fx.ep,
                "postgres",
                "postgres",
                &fx.owner_password,
                &format!("\\set VERBOSITY verbose\n{drop};\n"),
            )
            .await
            .expect_err(&format!("{drop} succeeded"));
            println!("{drop} refused in {:?}", started.elapsed());
            assert!(err.contains(&refused), "{drop}: {err}");
        }
        assert_eq!(
            fx.value(
                &fx.ep,
                &format!(
                    "SELECT count(*) FROM pg_database WHERE datname = {} AND datconnlimit <> -2;",
                    literal(&main)
                ),
            )
            .await,
            "1",
            "main is still a valid database"
        );
        assert!(
            fx.listed_databases()
                .await
                .contains(&(main.clone(), "main".into()))
        );
        let (status, branches) =
            api("GET", &format!("/v1/databases/{}/branches", fx.id), None).await;
        assert_eq!(status, 200);
        assert_eq!(branches[0]["main_database_missing"], false, "{branches}");
        assert_eq!(data_select(&key, "notes").await.0, 200);

        // A rename stays refused by the receiver. Postgres refuses a rename of a database in
        // use first, so the data API's pooled session is ended (as cloud_admin) just before.
        fx.sql_in(
            &fx.ep,
            "postgres",
            &format!(
                "SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity \
                 WHERE datname = {};",
                literal(&main)
            ),
        )
        .await;
        let err = psql(
            &fx.ep,
            "postgres",
            "postgres",
            &fx.owner_password,
            &format!("ALTER DATABASE {} RENAME TO renamed_main", ident(&main)),
        )
        .await
        .expect_err("main was renamed");
        assert!(
            err.contains(&format!(
                "ERROR:  the branch's main database \"{main}\" can't be renamed"
            )),
            "{err}"
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn system_database_listed_and_connectable() {
    live(|fx| async move {
        // Right after the provision: no resync has to happen first.
        let listed = fx.listed_databases().await;
        assert!(
            listed.contains(&("postgres".into(), "system".into())),
            "{listed:?}"
        );
        let (status, body) = api(
            "GET",
            &format!("{}/connection?database=postgres", fx.branch_path(fx.main)),
            None,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["database"], "postgres");
        let out = psql(
            &fx.ep,
            "postgres",
            "postgres",
            body["password"].as_str().unwrap(),
            "SELECT current_database()",
        )
        .await;
        assert_eq!(out, Ok("postgres".into()));
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn owner_password_set_in_sql() {
    live(|fx| async move {
        fx.owner_ok("ALTER ROLE postgres PASSWORD 'owner-set-in-sql-1'")
            .await;
        assert_logs_in(&fx.ep, &fx.database, "postgres", "owner-set-in-sql-1").await;
        assert_refused(&fx.ep, &fx.database, "postgres", &fx.owner_password).await;
        let (status, body) = api(
            "GET",
            &format!("{}/connection", fx.branch_path(fx.main)),
            None,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["password_state"], "set_in_sql", "{body}");
        assert_eq!(body["password"], Value::Null, "{body}");

        // The receiver's refusal reaches the client as its body exactly (fork F2: no
        // "Received HTTP code …" prefix).
        let err = psql(
            &fx.ep,
            &fx.database,
            "postgres",
            "owner-set-in-sql-1",
            "ALTER ROLE postgres PASSWORD NULL",
        )
        .await
        .expect_err("the owner's password was removed");
        assert!(
            err.contains("ERROR:  the branch owner's password can't be removed; reset it instead"),
            "{err}"
        );
        assert!(!err.contains("Received HTTP code"), "{err}");
        assert_logs_in(&fx.ep, &fx.database, "postgres", "owner-set-in-sql-1").await;
    })
    .await;
}

// --- API roles ------------------------------------------------------------------------------

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn plain_role_logs_in_and_is_plain() {
    live(|fx| async move {
        fx.create(fx.main, "app", false).await;
        let pw = fx.password(fx.main, "app").await;
        assert_logs_in(&fx.ep, &fx.database, "app", &pw).await;
        fx.assert_plain(&fx.ep, "app").await;
        assert_eq!(
            fx.copies(&fx.ep).await,
            [("app".to_string(), RoleOrigin::Api)]
        );
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn owner_level_role_is_a_neon_superuser_member() {
    live(|fx| async move {
        fx.create(fx.main, "admin", true).await;
        let pw = fx.password(fx.main, "admin").await;
        assert_logs_in(&fx.ep, &fx.database, "admin", &pw).await;
        assert_eq!(fx.membership(&fx.ep, "admin").await, "1|true");
        // The owner's attributes (N1, "match the owner").
        assert_eq!(
            fx.attributes(&fx.ep, "admin").await,
            "admin|false|true|true|true|true|true"
        );
        let (copies, _) = fx.metadata.role_copies(&fx.ep).await.unwrap();
        let admin = copies.iter().find(|c| c.name == "admin").unwrap();
        assert_eq!(admin.level, RoleLevel::OwnerLevel);
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn reset_applies_at_once() {
    live(|fx| async move {
        fx.create(fx.main, "app", false).await;
        let old = fx.password(fx.main, "app").await;
        // Cached by the proxy: only the reset's /password_updated can make it refuse.
        assert_logs_in(&fx.ep, &fx.database, "app", &old).await;
        let started = Instant::now();
        assert_eq!(fx.worker.reset_role(fx.main, "app").await, RoleOutcome::Ok);
        println!("reset took {:?}", started.elapsed());
        let new = fx.password(fx.main, "app").await;
        assert_ne!(old, new);
        assert_logs_in(&fx.ep, &fx.database, "app", &new).await;
        assert_refused(&fx.ep, &fx.database, "app", &old).await;
        fx.assert_plain(&fx.ep, "app").await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn reset_of_an_sql_role_makes_it_revealable() {
    live(|fx| async move {
        fx.owner_ok("CREATE ROLE app LOGIN PASSWORD 'app-pass-sql-reset'")
            .await;
        assert_eq!(fx.listed_role("app").await["password"], "set_in_sql");
        assert_eq!(fx.worker.reset_role(fx.main, "app").await, RoleOutcome::Ok);
        let fresh = fx.password(fx.main, "app").await;
        assert_logs_in(&fx.ep, &fx.database, "app", &fresh).await;
        assert_refused(&fx.ep, &fx.database, "app", "app-pass-sql-reset").await;
        let row = fx.listed_role("app").await;
        assert_eq!(row["password"], "generated", "{row}");
        assert_eq!(row["origin"], "sql", "{row}");
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn delete_refuses_login_and_hands_objects_to_the_owner() {
    live(|fx| async move {
        fx.create(fx.main, "app", false).await;
        let pw = fx.password(fx.main, "app").await;

        // A database made in SQL, and CREATE on public in both, so app makes its own tables.
        fx.owner_ok("CREATE DATABASE extra_db").await;
        for db in [fx.database.as_str(), "extra_db"] {
            fx.sql_in(&fx.ep, db, "GRANT CREATE ON SCHEMA public TO app;")
                .await;
            psql(&fx.ep, db, "app", &pw, "CREATE TABLE app_t (n int);")
                .await
                .unwrap_or_else(|e| panic!("app creates a table in {db}: {e}"));
        }

        // An open session of app's.
        let mut session = psql_command(&fx.ep, &fx.database, "app", &pw, "SELECT pg_sleep(300)")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn psql");
        let mut seen = false;
        for _ in 0..50 {
            let n = fx
                .value(
                    &fx.ep,
                    "SELECT count(*) FROM pg_stat_activity WHERE usename = 'app';",
                )
                .await;
            if n != "0" {
                seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert!(seen, "app's session never showed in pg_stat_activity");

        let started = Instant::now();
        assert_eq!(fx.worker.delete_role(fx.main, "app").await, RoleOutcome::Ok);
        println!("delete took {:?}", started.elapsed());

        let ended = tokio::time::timeout(Duration::from_secs(10), session.wait())
            .await
            .expect("app's session still open 10 s after the delete")
            .unwrap();
        assert!(!ended.success(), "the session's query finished normally");
        assert_refused(&fx.ep, &fx.database, "app", &pw).await;
        assert_eq!(fx.role_count(&fx.ep, "app").await, "0");
        for db in [fx.database.as_str(), "extra_db"] {
            let owner = fx
                .sql_in(
                    &fx.ep,
                    db,
                    "SELECT tableowner FROM pg_tables WHERE tablename = 'app_t';",
                )
                .await;
            assert_eq!(owner.trim(), "postgres", "app_t's owner in {db}");
        }
        assert_eq!(fx.copies(&fx.ep).await, []);
        assert_logs_in(&fx.ep, &fx.database, "postgres", &fx.owner_password).await;
    })
    .await;
}

// I1: a database name that isn't a plain identifier (here one with a hyphen) doesn't block
// the delete: psql reaches it, and the role's table in it moves to the owner.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn delete_reaches_a_hyphenated_database() {
    live(|fx| async move {
        fx.create(fx.main, "app", false).await;
        fx.owner_ok("CREATE DATABASE \"my-app\"").await;
        fx.sql_in(
            &fx.ep,
            "my-app",
            "GRANT CREATE ON SCHEMA public TO app; SET ROLE app; CREATE TABLE app_t (n int);",
        )
        .await;

        assert_eq!(fx.worker.delete_role(fx.main, "app").await, RoleOutcome::Ok);

        let owner = fx
            .sql_in(
                &fx.ep,
                "my-app",
                "SELECT tableowner FROM pg_tables WHERE tablename = 'app_t';",
            )
            .await;
        assert_eq!(owner.trim(), "postgres", "app_t's owner in my-app");
        assert_eq!(fx.role_count(&fx.ep, "app").await, "0");
    })
    .await;
}

// M4: an owner-level role that granted a role to another login role (so Postgres 16+ records it
// as that membership's grantor) still deletes. (The owner makes the group and gives `admin`
// ADMIN on it: revision 2's owner-level role is a `neon_superuser` member without CREATEROLE.)
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn delete_owner_level_role_that_granted_memberships() {
    live(|fx| async move {
        fx.create(fx.main, "admin", true).await;
        fx.create(fx.main, "app", false).await;
        let admin_pw = fx.password(fx.main, "admin").await;
        let app_pw = fx.password(fx.main, "app").await;
        // An owner-level role has the owner's attributes (N1): it makes roles and databases
        // and bypasses RLS.
        psql_script(
            &fx.ep,
            &fx.database,
            "admin",
            &admin_pw,
            "CREATE ROLE app_group NOLOGIN;\nGRANT app_group TO app;\n\
             CREATE DATABASE admin_made;\n",
        )
        .await
        .unwrap_or_else(|e| panic!("admin makes a role, grants it and makes a database: {e}"));
        assert!(
            fx.listed_databases()
                .await
                .contains(&("admin_made".to_string(), "sql".into())),
            "admin's database is listed"
        );
        fx.owner_ok(
            "CREATE TABLE secrets (n int); INSERT INTO secrets VALUES (1), (2); \
             ALTER TABLE secrets ENABLE ROW LEVEL SECURITY; GRANT SELECT ON secrets TO app",
        )
        .await;
        let count = |user: &'static str, pw: String| {
            let fx = fx.clone();
            async move {
                psql(
                    &fx.ep,
                    &fx.database,
                    user,
                    &pw,
                    "SELECT count(*) FROM secrets",
                )
                .await
                .unwrap_or_else(|e| panic!("{user} reads secrets: {e}"))
            }
        };
        assert_eq!(
            count("admin", admin_pw.clone()).await,
            "2",
            "admin bypasses RLS"
        );
        assert_eq!(count("app", app_pw.clone()).await, "0", "app doesn't");
        let grantor = |member: &'static str| {
            format!(
                "SELECT coalesce(string_agg(g.rolname, ','), '') FROM pg_auth_members m \
                 JOIN pg_roles r ON r.oid = m.member JOIN pg_roles g ON g.oid = m.grantor \
                 WHERE r.rolname = '{member}' AND m.roleid = 'app_group'::regrole;"
            )
        };
        assert_eq!(fx.value(&fx.ep, &grantor("app")).await, "admin");

        let started = Instant::now();
        let outcome = fx.worker.delete_role(fx.main, "admin").await;
        println!("delete took {:?}", started.elapsed());
        assert_eq!(outcome, RoleOutcome::Ok);

        assert_eq!(fx.role_count(&fx.ep, "admin").await, "0");
        // The group itself stays (roles have no owner to reassign); admin's database goes to
        // the owner.
        assert_eq!(fx.role_count(&fx.ep, "app_group").await, "1");
        assert_eq!(
            fx.value(
                &fx.ep,
                "SELECT pg_get_userbyid(datdba) FROM pg_database WHERE datname = 'admin_made';"
            )
            .await,
            "postgres"
        );
        // The memberships the role granted went with it.
        assert_eq!(
            fx.value(&fx.ep, &grantor("app")).await,
            "",
            "app's membership"
        );
        assert_logs_in(&fx.ep, &fx.database, "app", &app_pw).await;
        fx.assert_plain(&fx.ep, "app").await;
    })
    .await;
}

#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn restore_before_the_role_brings_it_back() {
    live(|fx| async move {
        fx.sql("CREATE TABLE t (v text); INSERT INTO t VALUES ('A');")
            .await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let noted = chrono::Utc::now();
        tokio::time::sleep(Duration::from_secs(2)).await;
        fx.create(fx.main, "app", false).await;
        let pw = fx.password(fx.main, "app").await;
        assert_logs_in(&fx.ep, &fx.database, "app", &pw).await;

        let moved = fx
            .metadata
            .set_branch_status(fx.main, &[BranchStatus::Ready], BranchStatus::Resetting)
            .await
            .unwrap();
        assert!(moved, "main was not ready");
        let started = Instant::now();
        fx.worker
            .handle_branch_job(BranchJob::Restore {
                branch_id: fx.main,
                backup_id: BranchId::new(),
                backup_name: "app-before-restore-20261001T120000Z".into(),
                at: PointInTime::Timestamp(noted),
            })
            .await
            .expect("restore job");
        println!("restore took {:?}", started.elapsed());
        let main = fx.metadata.get_branch(fx.main).await.unwrap().unwrap();
        assert_eq!(main.status, BranchStatus::Ready, "{:?}", main.error);
        // The restore really rewound: a row written after the noted time would be gone; the
        // role was made after it and is back, plain, with the same password.
        assert_eq!(
            fx.sql("SELECT string_agg(v, ',') FROM t;").await.trim(),
            "A"
        );
        fx.assert_plain(&fx.ep, "app").await;
        assert_logs_in(&fx.ep, &fx.database, "app", &pw).await;
    })
    .await;
}

// U1: a NOLOGIN group role with attributes, made by admin_sql (never in the spec), and a
// LOGIN member of it; an update drops the membership and LOGIN; the resync keeps both
// NOLOGIN roles and restores the group's attributes after drift in SQL.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn nologin_group_role_and_membership_on_a_real_compute() {
    live(|fx| async move {
        const ATTRS: &str = "SELECT rolcanlogin::text || ',' || rolinherit::text || ',' || \
            rolconnlimit || ',' || rolbypassrls::text || ',' || (rolpassword IS NULL)::text || \
            ',' || coalesce(extract(epoch FROM rolvaliduntil)::bigint::text, '-') \
            FROM pg_authid WHERE rolname = 'readers';";
        const MEMBERS: &str = "SELECT count(*) FROM pg_auth_members \
            WHERE roleid = 'readers'::regrole AND member = 'app'::regrole;";
        let group = RoleSpec {
            login: false,
            inherit: false,
            connection_limit: 3,
            valid_until: chrono::DateTime::from_timestamp(1_900_000_000, 0),
            bypass_rls: true,
            ..RoleSpec::login("readers", false)
        };
        assert_eq!(fx.worker.create_role(fx.main, &group).await, RoleOutcome::Ok);
        assert_eq!(fx.value(&fx.ep, ATTRS).await, "false,false,3,true,true,1900000000");
        assert_eq!(fx.metadata.role_password(&fx.ep, "readers").await.unwrap(), None);

        let member = RoleSpec {
            member_of: vec!["readers".into()],
            ..RoleSpec::login("app", false)
        };
        assert_eq!(fx.worker.create_role(fx.main, &member).await, RoleOutcome::Ok);
        assert_eq!(fx.value(&fx.ep, MEMBERS).await, "1");
        let pw = fx.password(fx.main, "app").await;
        assert_logs_in(&fx.ep, &fx.database, "app", &pw).await;

        // Chelabase's own roles are never a membership target.
        let reserved = RoleSpec {
            member_of: vec!["authenticated".into()],
            ..RoleSpec::login("sneaky", false)
        };
        assert!(matches!(
            fx.worker.create_role(fx.main, &reserved).await,
            RoleOutcome::Refused("role_membership_not_allowed", _)
        ));
        assert_eq!(fx.role_count(&fx.ep, "sneaky").await, "0");

        // The update: no membership, no LOGIN, no password.
        let patch = RolePatch {
            member_of: Some(vec![]),
            login: Some(false),
            ..RolePatch::default()
        };
        assert_eq!(fx.worker.update_role(fx.main, "app", &patch).await, RoleOutcome::Ok);
        assert_eq!(fx.value(&fx.ep, MEMBERS).await, "0");
        assert_eq!(
            fx.value(&fx.ep, "SELECT rolcanlogin::text || ',' || (rolpassword IS NULL)::text FROM pg_authid WHERE rolname = 'app';").await,
            "false,true"
        );
        assert_refused(&fx.ep, &fx.database, "app", &pw).await;

        // E32: changes in SQL stick, for an API-made role too: the resync records them (and
        // keeps both roles) instead of setting them back.
        fx.sql("ALTER ROLE readers LOGIN INHERIT CONNECTION LIMIT -1 NOBYPASSRLS;").await;
        fx.worker.resync(fx.main).await.unwrap();
        assert_eq!(fx.value(&fx.ep, ATTRS).await, "true,true,-1,false,true,1900000000");
        assert_eq!(fx.role_count(&fx.ep, "app").await, "1");
        let copies = fx.copies(&fx.ep).await;
        assert!(copies.contains(&("readers".to_string(), RoleOrigin::Api)), "{copies:?}");
        assert!(copies.contains(&("app".to_string(), RoleOrigin::Api)), "{copies:?}");
        let (rows, _) = fx.metadata.role_copies(&fx.ep).await.unwrap();
        let readers = rows.iter().find(|r| r.name == "readers").unwrap();
        assert!(readers.login, "recorded");
        assert_eq!(
            (readers.attributes.inherit, readers.attributes.connection_limit, readers.attributes.bypass_rls),
            (true, -1, false),
            "recorded"
        );
        assert!(!readers.attributes_pending);
        // A later PATCH starts from there: only what it names changes.
        let limit = RolePatch { connection_limit: Some(2), ..RolePatch::default() };
        assert_eq!(fx.worker.update_role(fx.main, "readers", &limit).await, RoleOutcome::Ok);
        assert_eq!(fx.value(&fx.ep, ATTRS).await, "true,true,2,false,true,1900000000");

        // Fix round 1: a role made in SQL by cloud_admin (the owner has no ADMIN on it) is
        // someone else's: the PATCH doesn't find it and changes nothing.
        fx.sql("CREATE ROLE theirs NOLOGIN;").await;
        let bypass = RolePatch {
            bypass_rls: Some(true),
            ..RolePatch::default()
        };
        assert_eq!(fx.worker.update_role(fx.main, "theirs", &bypass).await, RoleOutcome::NotFound);
        assert_eq!(
            fx.value(&fx.ep, "SELECT rolbypassrls::text FROM pg_authid WHERE rolname = 'theirs';").await,
            "false"
        );
        // One the owner made is the owner's to change.
        fx.owner_ok("CREATE ROLE mine NOLOGIN;").await;
        assert_eq!(fx.worker.update_role(fx.main, "mine", &bypass).await, RoleOutcome::Ok);

        // A group the owner administers but that reaches a reserved role, two levels down, is
        // never a membership target.
        fx.owner_ok("CREATE ROLE mid NOLOGIN; CREATE ROLE outer_g NOLOGIN; GRANT mid TO outer_g;")
            .await;
        fx.sql("GRANT authenticated TO mid;").await;
        let join = RolePatch {
            member_of: Some(vec!["outer_g".into()]),
            ..RolePatch::default()
        };
        assert!(matches!(
            fx.worker.update_role(fx.main, "app", &join).await,
            RoleOutcome::Refused("role_membership_not_allowed", _)
        ));
        assert_eq!(
            fx.value(&fx.ep, "SELECT count(*) FROM pg_auth_members WHERE member = 'app'::regrole;").await,
            "0"
        );
        // Once the reach is gone, the same group is fine.
        fx.sql("REVOKE authenticated FROM mid;").await;
        assert_eq!(fx.worker.update_role(fx.main, "app", &join).await, RoleOutcome::Ok);
    })
    .await;
}

impl Fx {
    /// Creates the NOLOGIN role `name` on `ep` (as cloud_admin, forwarding off) with the first
    /// OID at or past `target` the compute hands out: placeholder roles take the OIDs before
    /// it and are dropped again. The role's OID.
    async fn role_at_oid(&self, ep: &str, name: &str, target: i64) -> i64 {
        self.sql_in(
            ep,
            "postgres",
            &format!(
                "DO $do$
                 DECLARE i int := 0; o oid;
                 BEGIN
                   LOOP
                     i := i + 1;
                     EXECUTE format('CREATE ROLE %I NOLOGIN', 'oid_filler_' || i);
                     SELECT oid INTO o FROM pg_roles WHERE rolname = 'oid_filler_' || i;
                     EXIT WHEN o >= {target} OR i >= 20000;
                   END LOOP;
                   EXECUTE format('ALTER ROLE %I RENAME TO %I', 'oid_filler_' || i, {name});
                   FOR j IN 1 .. i - 1 LOOP
                     EXECUTE format('DROP ROLE %I', 'oid_filler_' || j);
                   END LOOP;
                 END
                 $do$;",
                name = literal(name),
            ),
        )
        .await;
        self.value(ep, &format!("SELECT oid FROM pg_roles WHERE rolname = {};", literal(name)))
            .await
            .parse()
            .unwrap_or_else(|e| panic!("{name}'s OID on {ep}: {e}"))
    }

    /// A secret key on `role` of `branch` through the API; its id.
    async fn secret_key_on(&self, branch: BranchId, name: &str, role: &str) -> String {
        let (status, body) = api(
            "POST",
            &format!("{}/keys", self.branch_path(branch)),
            Some(json!({ "name": name, "role": role })),
        )
        .await;
        assert_eq!(status, 201, "create key {name} on {role}: {body}");
        body["id"].as_str().unwrap().to_string()
    }

    /// `(role, role_oid, role_timeline, revoked)` of the key `id`.
    async fn key_row(&self, id: &str) -> (String, Option<i64>, Option<String>, bool) {
        sqlx::query_as(
            "SELECT role, role_oid, role_timeline, revoked_at IS NOT NULL
             FROM data_api_keys WHERE id = $1::uuid",
        )
        .bind(id)
        .fetch_one(self.metadata.pool())
        .await
        .unwrap()
    }
}

// E26: a secret key is bound to its role's OID on a timeline. The source `dev` makes `app`
// (OID A) with a key; the parent reaches A with a different role `x`. After `dev` is reset onto
// the parent, `x` holds A on dev: the key is revoked, never moved to `x`. A key on `shared`
// (made before the branch, the same OID and name on both timelines) is kept and stamped with
// the new timeline.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn reset_does_not_move_a_key_to_a_reused_oid() {
    live(|fx| async move {
        fx.sql("CREATE ROLE shared NOLOGIN;").await;
        let (dev, dev_ep) = fx.child("dev").await;
        // `app` on dev at an OID main hasn't reached yet, then `x` on main at that OID.
        let main_next = fx.role_at_oid(&fx.ep, "probe", 0).await;
        let app_oid = fx.role_at_oid(&dev_ep, "app", main_next + 1).await;
        let x_oid = fx.role_at_oid(&fx.ep, "x", app_oid).await;
        assert_eq!(x_oid, app_oid, "main's x must take dev's app's OID for this test");

        let app_key = fx.secret_key_on(dev, "app-key", "app").await;
        let shared_key = fx.secret_key_on(dev, "shared-key", "shared").await;
        let before = fx.metadata.get_branch(dev).await.unwrap().unwrap().timeline_id;
        let shared_oid: i64 = fx
            .value(&dev_ep, "SELECT oid FROM pg_roles WHERE rolname = 'shared';")
            .await
            .parse()
            .unwrap();
        assert_eq!(
            fx.key_row(&app_key).await,
            ("app".to_string(), Some(app_oid), before.clone(), false),
            "bound to app's OID on dev's timeline"
        );

        let moved = fx
            .metadata
            .set_branch_status(dev, &[BranchStatus::Ready], BranchStatus::Resetting)
            .await
            .unwrap();
        assert!(moved, "dev was not ready");
        fx.worker
            .handle_branch_job(BranchJob::ResetToParent { branch_id: dev })
            .await
            .expect("reset job");
        let after = fx.metadata.get_branch(dev).await.unwrap().unwrap();
        assert_eq!(after.status, BranchStatus::Ready, "{:?}", after.error);
        assert_ne!(after.timeline_id, before, "a new timeline");
        let dev_ep = after.endpoint_id.clone().expect("dev's endpoint");
        assert_eq!(
            fx.value(&dev_ep, "SELECT oid FROM pg_roles WHERE rolname = 'x';").await,
            app_oid.to_string(),
            "x holds app's old OID on dev now"
        );
        assert_eq!(fx.role_count(&dev_ep, "app").await, "0");

        assert_eq!(
            fx.key_row(&app_key).await,
            ("app".to_string(), Some(app_oid), before.clone(), true),
            "revoked, never moved to x"
        );
        assert_eq!(
            fx.key_row(&shared_key).await,
            ("shared".to_string(), Some(shared_oid), after.timeline_id.clone(), false),
            "kept under its name, stamped with the new timeline"
        );
    })
    .await;
}

// T14 E29: a rename through the control plane (a merge's phase 0 calls it): the role in SQL
// with forwarding off, its row resealed under the new name (the password logs in, the old name
// doesn't), and its key following with its OID (still served).
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn rename_role_through_the_control_plane() {
    live(|fx| async move {
        fx.create(fx.main, "app", false).await;
        fx.owner_ok("CREATE TABLE notes (id int); INSERT INTO notes VALUES (1); GRANT SELECT ON notes TO app")
            .await;
        let password = fx.password(fx.main, "app").await;
        let (key_id, key) = fx.secret_key("app-key", "app").await;
        wait_for_data_status(&key, "notes", 200, Duration::from_secs(30)).await;
        let oid = fx.value(&fx.ep, "SELECT oid FROM pg_roles WHERE rolname = 'app';").await;

        assert_eq!(fx.worker.rename_role(fx.main, "app", "app_v2").await, RoleOutcome::Ok);

        assert_eq!(fx.role_count(&fx.ep, "app").await, "0");
        assert_eq!(fx.value(&fx.ep, "SELECT oid FROM pg_roles WHERE rolname = 'app_v2';").await, oid);
        assert_eq!(fx.password(fx.main, "app_v2").await, password, "resealed under the new name");
        assert_logs_in(&fx.ep, &fx.database, "app_v2", &password).await;
        assert_refused(&fx.ep, &fx.database, "app", &password).await;
        let copies = fx.copies(&fx.ep).await;
        assert!(copies.contains(&("app_v2".to_string(), RoleOrigin::Api)), "{copies:?}");
        let role: String = sqlx::query_scalar("SELECT role FROM data_api_keys WHERE id = $1::uuid")
            .bind(&key_id)
            .fetch_one(fx.metadata.pool())
            .await
            .unwrap();
        assert_eq!(role, "app_v2");
        wait_for_data_status(&key, "notes", 200, Duration::from_secs(35)).await;
        // A taken or reserved name is refused, and nothing moves.
        fx.owner_ok("CREATE ROLE taken NOLOGIN").await;
        assert!(matches!(
            fx.worker.rename_role(fx.main, "app_v2", "taken").await,
            RoleOutcome::Refused("role_exists", _)
        ));
        assert!(matches!(
            fx.worker.rename_role(fx.main, "app_v2", "anon").await,
            RoleOutcome::Refused("reserved_role_name", _)
        ));
        assert_eq!(fx.role_count(&fx.ep, "app_v2").await, "1");
    })
    .await;
}

// T14 E32, the upgrade: an API-made role given a group and attributes in SQL (as a migration
// would) keeps them through the resync, which records them; nothing is revoked.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn sql_changes_to_an_api_role_stick_through_the_resync() {
    live(|fx| async move {
        fx.create(fx.main, "app", false).await;
        fx.owner_ok("CREATE ROLE readers NOLOGIN; GRANT readers TO app; ALTER ROLE app NOINHERIT CONNECTION LIMIT 4")
            .await;
        fx.worker.resync(fx.main).await.unwrap();
        assert_eq!(
            fx.value(&fx.ep, "SELECT count(*) FROM pg_auth_members WHERE member = 'app'::regrole AND roleid = 'readers'::regrole;").await,
            "1",
            "the membership stays"
        );
        assert_eq!(
            fx.value(&fx.ep, "SELECT rolinherit::text || ',' || rolconnlimit FROM pg_roles WHERE rolname = 'app';").await,
            "false,4"
        );
        let (rows, _) = fx.metadata.role_copies(&fx.ep).await.unwrap();
        let app = rows.iter().find(|r| r.name == "app").unwrap();
        assert_eq!(app.origin, RoleOrigin::Api);
        assert_eq!(
            (app.attributes.inherit, app.attributes.connection_limit, app.attributes.member_of.clone()),
            (false, 4, vec!["readers".to_string()]),
            "recorded"
        );
    })
    .await;
}

// T14 I2: turning LOGIN on counts toward the login-role cap, in the same transaction as the
// write (the worker's cap is 50 in the dev stack: a branch near it is built with SQL roles).
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn turning_login_on_respects_the_cap() {
    live(|fx| async move {
        let cap = fx.worker.roles.role_cap;
        let group = RoleSpec { login: false, ..RoleSpec::login("grp", false) };
        assert_eq!(fx.worker.create_role(fx.main, &group).await, RoleOutcome::Ok);
        // Fill the cap with SQL login roles (recorded by the resync).
        let mut script = String::new();
        for i in 0..cap {
            script.push_str(&format!("CREATE ROLE filler_{i} LOGIN PASSWORD 'filler-{i}-pass-123';\n"));
        }
        psql_script(&fx.ep, &fx.database, "postgres", &fx.owner_password, &script)
            .await
            .expect("fill the cap");
        fx.worker.resync(fx.main).await.unwrap();
        let on = RolePatch { login: Some(true), ..RolePatch::default() };
        assert!(matches!(
            fx.worker.update_role(fx.main, "grp", &on).await,
            RoleOutcome::Refused("role_cap_reached", _)
        ));
        assert_eq!(
            fx.value(&fx.ep, "SELECT rolcanlogin::text FROM pg_roles WHERE rolname = 'grp';").await,
            "false"
        );
    })
    .await;
}

/// E29a: a role's live sessions, read by the worker over the admin path and served by
/// `listRoleSessions`: the count, the client address, `application_name` and start time.
#[tokio::test]
#[ignore = "needs the Neon stack, just dev and CHELABASE_COMPUTE_IMAGE"]
async fn role_sessions_lists_the_live_connections() {
    live(|fx| async move {
        fx.create(fx.main, "sess_app", false).await;
        let password = fx.password(fx.main, "sess_app").await;
        let ask = chelabase_nats::RoleRequest { branch_id: fx.main, name: "sess_app".into() };

        let none = fx.worker.role_sessions_reply(&ask).await;
        assert!(none.ok && none.compute_running, "{none:?}");
        assert!(none.sessions.is_empty(), "{none:?}");

        // One session held open as the role, with an application name.
        let mut held = psql_command(&fx.ep, &fx.database, "sess_app", &password, "SELECT pg_sleep(60)");
        held.env("PGAPPNAME", "sessions-live");
        let mut child = held.spawn().expect("spawn psql");
        let started = Instant::now();
        let reply = loop {
            let reply = fx.worker.role_sessions_reply(&ask).await;
            if !reply.sessions.is_empty() || started.elapsed() > Duration::from_secs(20) {
                break reply;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        assert!(reply.ok && reply.compute_running, "{reply:?}");
        assert_eq!(reply.sessions.len(), 1, "{reply:?}");
        assert_eq!(reply.sessions[0].application_name, "sessions-live");
        assert!(reply.sessions[0].client_addr.is_some(), "the proxy connects over TCP: {reply:?}");

        let (status, body) = api(
            "GET",
            &format!("{}/roles/sess_app/sessions", fx.branch_path(fx.main)),
            None,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["compute_running"], true, "{body}");
        assert_eq!(body["count"], 1, "{body}");
        assert_eq!(body["sessions"][0]["application_name"], "sessions-live", "{body}");

        // Another role's sessions aren't counted.
        let (status, body) = api(
            "GET",
            &format!("{}/roles/nobody_here/sessions", fx.branch_path(fx.main)),
            None,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["count"], 0, "{body}");

        child.kill().await.ok();
    })
    .await;
}
