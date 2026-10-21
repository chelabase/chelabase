//! Fork rounds 3 and 4 on a real compute: the keys plugin, the realtime restriction, migrations
//! before `running`, the compute's TLS files and networks, certificate renewal (round 3); slot
//! aux files after a crash, compute_ctl's internal API, storage-token refresh (round 4). Needs
//! the round-4 images.
//!
//! ```text
//! just workbench test -p worker --test compute_fork_live -- --run-ignored only
//! ```

use std::future::Future;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use chelabase_metadata::{
    BranchStatus, DEMO_PROJECT_ID, DEMO_TENANT_ID, DatabaseStatus, MetadataClient, NewBranch,
};
use chelabase_nats::BranchJob;
use chelabase_neon::compute::{ComputeLocator, container_name};
use chelabase_neon::compute_tls::server_name;
use chelabase_neon::jwt::{STORAGE_TOKEN_TTL, StorageScope};
use chelabase_neon::{DockerLocator, NeonId};
use chelabase_neon::reconfigure::ComputeCtlStatus;
use chelabase_neon::ComputeBackend;
use chelabase_types::role_name::is_marked_sql;
use chelabase_types::{BranchId, DatabaseId};
use tokio::io::AsyncWriteExt;
use worker::config::RealWorker;
use worker::ports::SpecPusher;

mod common;
use common::{cleanup, setup, stack_name};

/// The realtime role with a password these tests know. Since RT1 T5 the worker makes the role
/// at provisioning; before, nothing did: both cases end with the role and this password.
const REALTIME_ROLE_WITH_TEST_PASSWORD: &str = "DO $$
BEGIN
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'chelabase_realtime') THEN
    CREATE ROLE chelabase_realtime LOGIN REPLICATION NOBYPASSRLS NOINHERIT;
  END IF;
END
$$;
ALTER ROLE chelabase_realtime PASSWORD 'rt-live-pw';";

/// Where compute_ctl keeps the compute's storage token (K3; Chelabase's pgdata is
/// `/var/db/postgres/compute`).
const STORAGE_TOKEN_FILE: &str = "/var/db/postgres/storage_auth.token";
/// The per-start secret of compute_ctl's internal API (K3).
const INTERNAL_TOKEN_FILE: &str = "/var/db/postgres/compute_ctl_internal.token";

struct Db {
    worker: RealWorker,
    metadata: MetadataClient,
    id: DatabaseId,
    ep: String,
    database: String,
    stack: String,
    control_network: String,
    /// `CHELABASE_REALTIME_TLS_DIR`: the realtime client pair (`current/{client.crt,client.key,
    /// ca.crt}`), written by the worker's certificate sweep.
    realtime_tls_dir: PathBuf,
}

impl Db {
    async fn sql(&self, database: &str, sql: &str) -> String {
        self.worker
            .computes
            .admin_sql(&self.ep, database, sql)
            .await
            .unwrap_or_else(|e| panic!("admin_sql {sql:?}: {e:#}"))
    }

    /// The compute's container. `self.stack` is `common::stack_name`'s: in a stack's runner the
    /// compose project (`CHELABASE_COMPOSE_PROJECT`) names it, elsewhere the worker's own stack.
    fn container(&self) -> String {
        container_name(&self.stack, &self.ep)
    }

    /// A login over the compute network with a password only: how the proxy reaches a compute.
    /// Since round 4 (W31) `chelabase_realtime` is refused here; it dials the control address
    /// with the realtime pair ([`Db::realtime_conninfo`], SP9).
    async fn conninfo(&self, user: &str, password: &str, replication: bool) -> String {
        let addr = self.worker.computes.postgres_addr(&self.ep).await.unwrap().expect("address");
        format!(
            "host={} port={} dbname={} user={user} password={password} sslmode=prefer{}",
            addr.ip(),
            addr.port(),
            self.database,
            if replication { " replication=database" } else { "" }
        )
    }

    /// `chelabase_realtime` as realtime dials it (SP9): the compute's address on the `control`
    /// network, TLS with the realtime client pair, the role's password.
    async fn realtime_conninfo(&self, replication: bool) -> String {
        let addr = DockerLocator::connect(&self.control_network, &self.stack)
            .expect("docker")
            .postgres_addr(&self.ep)
            .await
            .expect("locating the compute")
            .expect("the compute has an address on the control network");
        let tls = self.realtime_tls_dir.join("current");
        for file in ["client.crt", "client.key", "ca.crt"] {
            assert!(
                tls.join(file).exists(),
                "{} is missing: the worker's certificate sweep writes the realtime pair (is CHELABASE_REALTIME_TLS_DIR the stack's?)",
                tls.join(file).display()
            );
        }
        format!(
            "host={} hostaddr={} port={} dbname={} user=chelabase_realtime password=rt-live-pw \
             sslmode=verify-full sslrootcert={} sslcert={} sslkey={}{}",
            server_name(&self.ep),
            addr.ip(),
            addr.port(),
            self.database,
            tls.join("ca.crt").display(),
            tls.join("client.crt").display(),
            tls.join("client.key").display(),
            if replication { " replication=database" } else { "" }
        )
    }

    async fn psql(&self, conninfo: &str, script: &str) -> Result<String, String> {
        let mut child = tokio::process::Command::new("psql")
            .args([conninfo, "-X", "-qtA", "-v", "ON_ERROR_STOP=1", "-f", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("psql");
        child.stdin.take().unwrap().write_all(script.as_bytes()).await.unwrap();
        let out = child.wait_with_output().await.unwrap();
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).into_owned())
        }
    }

    async fn ready_again(&self) {
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if self.worker.computes.admin_sql(&self.ep, "postgres", "SELECT 1").await.is_ok() {
                return;
            }
            assert!(Instant::now() < deadline, "the compute didn't come back");
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }

    async fn docker(&self, args: &[&str]) -> String {
        let out = tokio::process::Command::new("docker").args(args).output().await.expect("docker");
        assert!(out.status.success(), "docker {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// The endpoint's tenant.
    async fn tenant(&self) -> NeonId {
        let endpoint = self.metadata.neon_endpoint(&self.ep).await.unwrap().expect("endpoint row");
        endpoint.tenant_id.parse().expect("tenant id")
    }

    /// `docker exec` in the compute: stdout.
    async fn exec(&self, args: &[&str]) -> String {
        let container = self.container();
        let mut all = vec!["exec", container.as_str()];
        all.extend_from_slice(args);
        self.docker(&all).await
    }

    /// [`Db::exec`] that gives up after `within`: the `docker` client is killed and the test
    /// fails, instead of hanging on a command that never ends (a server that never answers).
    async fn exec_within(&self, within: Duration, args: &[&str]) -> String {
        let container = self.container();
        let mut all = vec!["exec", container.as_str()];
        all.extend_from_slice(args);
        let child = tokio::process::Command::new("docker")
            .args(&all)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("docker");
        let out = tokio::time::timeout(within, child.wait_with_output())
            .await
            .unwrap_or_else(|_| panic!("docker {all:?} took over {within:?}"))
            .expect("docker");
        assert!(out.status.success(), "docker {all:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// The compute's current storage token, as compute_ctl keeps it in its file (K3).
    async fn storage_token_file(&self) -> String {
        self.exec(&["cat", STORAGE_TOKEN_FILE]).await.trim().to_string()
    }

    /// The paths a `basebackup` of the endpoint's timeline ships (what a compute start gets,
    /// `pg_replslot/<slot>/state` entries included), asked of the pageserver as `token`. The
    /// pageserver image has no `testing` APIs (`list_aux_files` answers 400), so the basebackup
    /// stands in: it is the one place a stale slot aux entry reaches a compute.
    async fn basebackup_entries(&self, token: &str) -> Result<Vec<String>, String> {
        let endpoint = self.metadata.neon_endpoint(&self.ep).await.unwrap().expect("endpoint row");
        let tenant: NeonId = endpoint.tenant_id.parse().expect("tenant id");
        let connstring = self
            .worker
            .storage
            .storcon
            .pageserver_connstring(&tenant)
            .await
            .expect("the tenant's pageserver");
        // `postgresql://no_user@host:port`
        let hostport = connstring.rsplit('@').next().unwrap();
        let (host, port) = hostport.rsplit_once(':').expect("host:port");
        let conninfo =
            format!("host={host} port={port} user=no_user dbname=postgres sslmode=disable password={token}");
        let command = format!("basebackup {} {}", endpoint.tenant_id, endpoint.timeline_id);
        let out = tokio::process::Command::new("psql")
            .args([conninfo.as_str(), "-X", "-q", "-c", command.as_str()])
            .output()
            .await
            .expect("psql");
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).into_owned());
        }
        let mut tar = tokio::process::Command::new("tar")
            .args(["-t", "-f", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("tar");
        let mut stdin = tar.stdin.take().unwrap();
        let bytes = out.stdout;
        let writer = tokio::spawn(async move {
            let _ = stdin.write_all(&bytes).await;
        });
        let listing = tar.wait_with_output().await.unwrap();
        let _ = writer.await;
        assert!(
            listing.status.success(),
            "tar -t of the basebackup: {}",
            String::from_utf8_lossy(&listing.stderr)
        );
        Ok(String::from_utf8_lossy(&listing.stdout).lines().map(str::to_string).collect())
    }

    /// One hand-made HTTP request to compute_ctl's internal port from inside the compute
    /// (bash's `/dev/tcp`: the image has no `curl`, SP12): the status code. Bounded: the
    /// `docker exec` gives up after 20 s, and the read inside the compute after 15 s.
    async fn internal_status(&self, bearer: Option<&str>) -> u16 {
        let auth = bearer.map(|b| format!("Authorization: Bearer {b}\\r\\n")).unwrap_or_default();
        let script = format!(
            "exec 3<>/dev/tcp/127.0.0.1/3081; \
             printf 'POST /extension_server/live_none.tar.zst HTTP/1.1\\r\\nHost: localhost\\r\\n{auth}Content-Length: 0\\r\\nConnection: close\\r\\n\\r\\n' >&3; \
             IFS= read -r -t 15 line <&3; printf '%s\\n' \"$line\""
        );
        let line = self.exec_within(Duration::from_secs(20), &["bash", "-c", script.as_str()]).await;
        line.split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or_else(|| panic!("no HTTP status line from :3081: {line:?}"))
    }

    async fn sql_result(&self, sql: &str) -> Result<String, String> {
        self.worker.computes.admin_sql(&self.ep, "postgres", sql).await.map_err(|e| format!("{e:#}"))
    }

    async fn ctl_status(&self) -> ComputeCtlStatus {
        let addr = self.worker.computes.control_addr(&self.ep).await.unwrap().expect("control address");
        self.worker.pusher.status(addr, &self.ep).await.expect("compute_ctl status")
    }
}

async fn with_db<F, Fut>(body: F)
where
    F: FnOnce(Arc<Db>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (worker, metadata, config) = setup().await;
    let id = DatabaseId::new();
    let name = format!("live-{}", &id.to_string()[..8]);
    metadata
        .insert_database_pending(id, DEMO_TENANT_ID.into(), DEMO_PROJECT_ID.into(), &name)
        .await
        .expect("insert pending database");
    let provisioned = tokio::time::timeout(Duration::from_secs(90), worker.handle_provision(id, &name)).await;
    let record = metadata.get_database(id).await.ok().flatten();
    let Some(record) = record.filter(|r| r.status == DatabaseStatus::Ready) else {
        cleanup(&worker, &metadata, id).await;
        panic!("provision failed: {provisioned:?}");
    };
    let branch = metadata.get_branch(BranchId::from(id)).await.unwrap().expect("main branch");
    let db = Arc::new(Db {
        ep: branch.endpoint_id.expect("endpoint"),
        database: record.database_name.expect("database name"),
        stack: stack_name(&config.stack),
        control_network: config.compute_control_network.clone(),
        realtime_tls_dir: config.realtime_tls_dir.clone(),
        worker,
        metadata,
        id,
    });
    let outcome = tokio::spawn(body(db.clone())).await;
    cleanup(&db.worker, &db.metadata, db.id).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the round-3 images"]
async fn chelabase_keys_ships_and_decodes() {
    with_db(|db| async move {
        db.sql(&db.database, "CREATE TABLE k (id int PRIMARY KEY, v int); CREATE TABLE kt (ts timestamptz PRIMARY KEY);").await;
        let out = db
            .sql(
                &db.database,
                "SELECT 'slot' FROM pg_create_logical_replication_slot('live_keys', 'chelabase_keys', true);\n\
                 SET TimeZone = 'Asia/Tokyo';\n\
                 INSERT INTO k VALUES (1, 0);\n\
                 UPDATE k SET id = 2 WHERE id = 1;\n\
                 TRUNCATE k;\n\
                 INSERT INTO kt VALUES ('2026-01-02 03:04:05+00');\n\
                 SELECT data FROM pg_logical_slot_get_changes('live_keys', NULL, NULL);",
            )
            .await;
        let lines: Vec<&str> = out.lines().filter(|l| l.starts_with('{')).collect();
        assert_eq!(lines.len(), 4, "{out}");
        assert!(lines[0].contains(r#""t":"k","op":"i","k":[["1"]]"#), "{}", lines[0]);
        assert!(lines[1].contains(r#""op":"i","k":[["2"]]"#) && lines[1].contains(r#""op":"d","k":[["1"]]"#), "{}", lines[1]);
        assert!(lines[2].contains(r#""t":"k","op":"c""#), "{}", lines[2]);
        assert!(lines[3].contains(r#""k":[["2026-01-02 03:04:05+00"]]"#), "{}", lines[3]);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the round-3 images"]
async fn realtime_role_is_restricted() {
    with_db(|db| async move {
        db.sql("postgres", REALTIME_ROLE_WITH_TEST_PASSWORD).await;
        let normal = db.realtime_conninfo(false).await;
        let repl = db.realtime_conninfo(true).await;
        // W31: the role is refused over the compute network, with or without a password.
        let proxy_side = db.conninfo("chelabase_realtime", "rt-live-pw", true).await;
        let err = db.psql(&proxy_side, "SELECT 1;").await.unwrap_err();
        assert!(err.contains("pg_hba.conf"), "{err}");
        let err = db.psql(&normal, "SELECT 1;").await.unwrap_err();
        assert!(err.contains("may only open logical replication connections"), "{err}");
        let err = db.psql(&repl, "SELECT 1;").await.unwrap_err();
        assert!(err.contains("can't run SQL in a replication connection"), "{err}");
        let err = db.psql(&repl, "DROP_REPLICATION_SLOT rt_none;").await.unwrap_err();
        assert!(err.contains(r#"permission denied for replication command "DROP_REPLICATION_SLOT""#), "{err}");
        let ok = db
            .psql(&repl, "CREATE_REPLICATION_SLOT rt_live TEMPORARY LOGICAL chelabase_keys (SNAPSHOT 'nothing');")
            .await
            .unwrap();
        assert!(ok.contains("rt_live"), "{ok}");
        let err = db.sql_result("SELECT pg_create_logical_replication_slot('rt_su', 'test_decoding', true);").await.unwrap_err();
        assert!(err.contains(r#"reserved for chelabase_realtime"#), "{err}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the round-3 images"]
async fn realtime_slot_goes_with_a_compute_restart() {
    with_db(|db| async move {
        db.sql("postgres", REALTIME_ROLE_WITH_TEST_PASSWORD).await;
        let repl = db.realtime_conninfo(true).await;
        let mut child = tokio::process::Command::new("psql")
            .args([repl.as_str(), "-X", "-qtA"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("psql");
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(b"CREATE_REPLICATION_SLOT rt_held TEMPORARY LOGICAL chelabase_keys (SNAPSHOT 'nothing');\n")
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let n = db.sql("postgres", "SELECT count(*) FROM pg_replication_slots WHERE slot_name = 'rt_held';").await;
            if n.trim() == "1" {
                break;
            }
            assert!(Instant::now() < deadline, "the slot never appeared");
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        db.docker(&["restart", &db.container()]).await;
        db.ready_again().await;
        let n = db.sql("postgres", "SELECT count(*) FROM pg_replication_slots WHERE slot_name = 'rt_held';").await;
        assert_eq!(n.trim(), "0", "the temporary slot is gone");
        drop(stdin);
        let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
        let _ = child.kill().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the round-3 images"]
async fn migrations_done_when_provisioned() {
    with_db(|db| async move {
        let id = db.sql("postgres", "SELECT id FROM neon_migration.migration_id;").await;
        assert_eq!(id.trim(), "12", "every compute_ctl migration ran before running");
        let bypass = db.sql("postgres", "SELECT rolbypassrls FROM pg_roles WHERE rolname = 'authenticator';").await;
        assert_eq!(bypass.trim(), "f");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the round-3 images"]
async fn compute_has_tls_and_both_networks() {
    with_db(|db| async move {
        assert_eq!(db.sql("postgres", "SHOW ssl;").await.trim(), "on");
        assert_eq!(db.sql("postgres", "SHOW ssl_ca_file;").await.trim(), "/var/db/chelabase/tls/ca.crt");
        let networks = db.docker(&["inspect", "-f", "{{json .NetworkSettings.Networks}}", &db.container()]).await;
        assert!(networks.contains(&format!("\"{}\"", db.control_network)), "{networks}");
        let modes = db.docker(&["exec", &db.container(), "stat", "-c", "%a %U %n", "/var/db/chelabase/tls/server.key", "/var/db/chelabase/tls/server.crt", "/var/db/chelabase/tls/ca.crt"]).await;
        for line in modes.lines() {
            assert!(line.starts_with("644 root "), "{modes}");
        }
        assert_eq!(db.ctl_status().await, ComputeCtlStatus::Running);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the round-3 images"]
async fn cert_renewal_keeps_the_compute_running() {
    with_db(|db| async move {
        let before = db.docker(&["exec", &db.container(), "cat", "/var/db/postgres/compute/server.crt"]).await;
        let (renewed, failed) = db.worker.renew_certs_once(SystemTime::now() + Duration::from_secs(70 * 86_400)).await;
        assert!(renewed >= 1 && failed == 0, "{renewed} renewed, {failed} failed");
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            assert_eq!(db.ctl_status().await, ComputeCtlStatus::Running, "the status never leaves running");
            let now = db.docker(&["exec", &db.container(), "cat", "/var/db/postgres/compute/server.crt"]).await;
            if now != before {
                break;
            }
            assert!(Instant::now() < deadline, "compute_ctl never picked up the new certificate");
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        assert_eq!(db.sql("postgres", "SELECT 1;").await.trim(), "1");
    })
    .await;
}


/// U16 (K1): a temporary `rt_` slot's `neon-file:` copy used to outlive a crash and ride in every
/// later basebackup. Hold one as the realtime role across a checkpoint, kill the compute, start
/// it again: the new compute has no such slot and the timeline ships no state for it. (Aux files
/// are not inherited by child branches, so the test uses the branch's own timeline.)
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the round-4 images"]
async fn a_crashed_compute_leaves_no_slot_aux_file() {
    with_db(|db| async move {
        db.sql("postgres", REALTIME_ROLE_WITH_TEST_PASSWORD).await;
        let repl = db.realtime_conninfo(true).await;
        let mut child = tokio::process::Command::new("psql")
            .args([repl.as_str(), "-X", "-qtA"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("psql");
        let mut stdin = child.stdin.take().unwrap();
        stdin
            .write_all(b"CREATE_REPLICATION_SLOT rt_aux TEMPORARY LOGICAL chelabase_keys (SNAPSHOT 'nothing');\n")
            .await
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let n = db.sql("postgres", "SELECT count(*) FROM pg_replication_slots WHERE slot_name = 'rt_aux';").await;
            if n.trim() == "1" {
                break;
            }
            assert!(Instant::now() < deadline, "the slot never appeared");
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        // A checkpoint saves every slot's state (and used to copy it to the pageserver).
        db.sql("postgres", "CHECKPOINT;").await;

        db.docker(&["kill", &db.container()]).await;
        drop(stdin);
        let _ = child.kill().await;
        db.docker(&["start", &db.container()]).await;
        db.ready_again().await;

        // Only this test's slot: realtime (or another test) may hold its own `rt_` slots here.
        let n = db.sql("postgres", "SELECT count(*) FROM pg_replication_slots WHERE slot_name = 'rt_aux';").await;
        assert_eq!(n.trim(), "0", "a temporary slot doesn't survive the crash");
        // The restarted primary cleans up what an older compute left; one more checkpoint makes
        // sure everything it logged has reached the pageserver.
        db.sql("postgres", "CHECKPOINT;").await;
        let token = db.worker.pusher.signer().storage_token(&StorageScope::Tenant(db.tenant().await)).unwrap();
        let entries = db.basebackup_entries(&token).await.expect("basebackup");
        assert!(
            !entries.iter().any(|e| e.contains("pg_replslot/rt_aux")),
            "the timeline still ships the dead slot's state: {entries:?}"
        );
    })
    .await;
}

/// U6 (K3, SP12): compute_ctl's internal API listens on loopback only and answers 401 without
/// the per-start secret; nothing outside the compute can connect to `:3081`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the round-4 images"]
async fn compute_ctl_internal_api_is_loopback_and_needs_the_token() {
    with_db(|db| async move {
        let sockets = db.exec(&["ss", "-ltnp"]).await;
        let on_3081: Vec<&str> = sockets
            .lines()
            .filter(|l| l.split_whitespace().nth(3).is_some_and(|local| local.ends_with(":3081")))
            .collect();
        assert!(!on_3081.is_empty(), "nothing listens on :3081: {sockets}");
        for line in &on_3081 {
            let local = line.split_whitespace().nth(3).unwrap();
            assert!(
                local == "127.0.0.1:3081" || local == "[::1]:3081",
                "the internal API listens beyond loopback ({local}): {sockets}"
            );
        }

        let secret = db.exec(&["cat", INTERNAL_TOKEN_FILE]).await.trim().to_string();
        assert_eq!(secret.len(), 64, "64 lowercase hex characters");
        assert_eq!(db.internal_status(None).await, 401, "no bearer");
        assert_eq!(db.internal_status(Some("wrong")).await, 401, "wrong bearer");
        let near_miss = format!("{secret}x");
        assert_eq!(db.internal_status(Some(near_miss.as_str())).await, 401, "near-miss bearer");
        assert_ne!(db.internal_status(Some(secret.as_str())).await, 401, "the secret opens the route");

        // From the runner: no connection on either network.
        let compute_ip = db.worker.computes.postgres_addr(&db.ep).await.unwrap().expect("address").ip();
        let control_ip = DockerLocator::connect(&db.control_network, &db.stack)
            .unwrap()
            .postgres_addr(&db.ep)
            .await
            .unwrap()
            .expect("control address")
            .ip();
        for ip in [compute_ip, control_ip] {
            let attempt = tokio::time::timeout(
                Duration::from_secs(5),
                tokio::net::TcpStream::connect((ip, 3081)),
            )
            .await;
            assert!(!matches!(attempt, Ok(Ok(_))), "{ip}:3081 accepted a connection");
        }
    })
    .await;
}

/// RT4-3, S9 (K3, K5): a compute's storage token expires and is replaced through
/// `POST /storage_token` by one refresh pass, with no restart. The runner mints a token that
/// expires in 30 s (SP8: `iat` shifted back by the TTL, no extra parameter), pushes it, waits
/// out the storage servers' 60 s leeway, and sees the pageserver refuse it; the pass puts a
/// good one in the file and the pageserver takes it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the round-4 images"]
async fn an_expired_storage_token_is_refreshed_without_a_restart() {
    with_db(|db| async move {
        let addr = db.worker.computes.control_addr(&db.ep).await.unwrap().expect("control address");
        let report = db.worker.pusher.report(addr, &db.ep).await.expect("compute_ctl status");
        assert!(report.storage_token_refresh, "the compute image doesn't advertise storage_token_refresh");
        let tenant = db.tenant().await;
        let container_started = db.docker(&["inspect", "-f", "{{.State.StartedAt}}", &db.container()]).await;
        let postmaster_started = db.sql("postgres", "SELECT pg_postmaster_start_time();").await;
        db.sql(&db.database, "CREATE TABLE expiry (id int PRIMARY KEY, v text); INSERT INTO expiry SELECT g, repeat('x', 100) FROM generate_series(1, 2000) g;").await;

        let real_now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        let short = db
            .worker
            .pusher
            .signer()
            .tenant_token(&tenant, real_now - STORAGE_TOKEN_TTL.as_secs() + 30, true)
            .unwrap();
        db.worker.pusher.push_storage_token(addr, &db.ep, &short).await.expect("POST /storage_token");
        assert_eq!(db.storage_token_file().await, short, "the file holds the pushed token");
        db.basebackup_entries(&short).await.expect("the short-lived token is good for now");

        // exp = real_now + 30; the storage servers allow 60 s of leeway.
        let wait_until = Instant::now() + Duration::from_secs(30 + 61 + 2);
        tokio::time::sleep_until(wait_until.into()).await;
        let refused = db.basebackup_entries(&short).await;
        assert!(refused.is_err(), "the pageserver still takes a token that expired over a minute ago");

        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        // The pass covers every compute of the stack, and a sibling test's compute may be
        // restarting meanwhile (its failure is not this test's): assert on this endpoint only.
        let pass = db.worker.refresh_storage_tokens_once(now).await;
        assert_eq!(db.worker.storage_token_failure_count(&db.ep), 0, "{pass:?}");
        let fresh = db.storage_token_file().await;
        assert_ne!(fresh, short, "the pass replaced this compute's token file: {pass:?}");
        db.basebackup_entries(&fresh).await.expect("the refreshed token is good");
        assert_eq!(db.sql(&db.database, "SELECT count(*) FROM expiry;").await.trim(), "2000");

        // No restart: the container and the postmaster are the same ones, and compute_ctl
        // never left running.
        assert_eq!(
            db.docker(&["inspect", "-f", "{{.State.StartedAt}}", &db.container()]).await,
            container_started
        );
        assert_eq!(db.sql("postgres", "SELECT pg_postmaster_start_time();").await, postmaster_started);
        assert_eq!(db.ctl_status().await, ComputeCtlStatus::Running);
    })
    .await;
}

/// What the API does for a branch create, then the create job; the child's endpoint.
async fn create_child(db: &Db, name: &str) -> (BranchId, String) {
    let id = BranchId::new();
    let outcome = db
        .metadata
        .create_branch(&NewBranch {
            id,
            database_id: db.id,
            name,
            parent_branch_id: BranchId::from(db.id),
            branched_at_time: None,
            branched_at_lsn: None,
        })
        .await
        .unwrap();
    assert_eq!(outcome, chelabase_metadata::CreateBranchOutcome::Created);
    db.worker
        .handle_branch_job(BranchJob::Create { branch_id: id, at: None })
        .await
        .expect("create job");
    let branch = db.metadata.get_branch(id).await.unwrap().unwrap();
    assert_eq!(branch.status, BranchStatus::Ready, "{:?}", branch.error);
    (id, branch.endpoint_id.expect("the child's endpoint"))
}

/// `sql` as the owner: a superuser session that has given its superuser powers up
/// (`SET ROLE postgres`), which is what the customer's own sessions are (a CREATEROLE
/// non-superuser); the answer or the error text.
async fn as_owner(db: &Db, ep: &str, sql: &str) -> Result<String, String> {
    db.worker
        .computes
        .admin_sql(ep, "postgres", &format!("SET ROLE postgres;\n{sql}"))
        .await
        .map_err(|e| format!("{e:#}"))
}

/// Whether `role` carries the marker on the compute of `ep` (SP22's predicate).
async fn is_marked(db: &Db, ep: &str, role: &str) -> bool {
    let out = db
        .worker
        .computes
        .admin_sql(ep, "postgres", &is_marked_sql(role))
        .await
        .unwrap_or_else(|e| panic!("is_marked_sql: {e:#}"));
    out.trim() == "t"
}

/// RT4-4 / D1: the worker makes a personal login and marks it in one transaction; the owner (a
/// CREATEROLE non-superuser) can't change, grant, rename or drop it, and neither can the login
/// itself; a child branch's compute guards it too (the marker travels with the catalog).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the round-4 images"]
async fn a_personal_login_is_guarded() {
    with_db(|db| async move {
        let password = "personal-live-pw";
        let verifier = chelabase_neon::scram::verifier(password);
        db.worker.create_personal_login(&db.ep, "Alice", &verifier).await.expect("create_personal_login");

        assert!(is_marked(&db, &db.ep, "Alice").await, "marked as it was made");
        assert!(!is_marked(&db, &db.ep, "alice").await, "`Alice` is not `alice`");
        // It is a plain login: no owner attributes, no neon_superuser, not made by the owner.
        let attributes = db
            .sql(
                "postgres",
                "SELECT rolcanlogin, rolsuper, rolcreaterole, rolcreatedb, rolreplication, rolbypassrls, \
                 pg_has_role('Alice', 'neon_superuser', 'MEMBER') FROM pg_roles WHERE rolname = 'Alice';",
            )
            .await;
        assert_eq!(attributes.trim(), "t|f|f|f|f|f|f", "{attributes}");

        // The owner's statements on it are refused; nothing is forwarded.
        for statement in [
            "ALTER ROLE \"Alice\" PASSWORD 'owner-set';",
            "ALTER ROLE \"Alice\" SET search_path = public;",
            "ALTER ROLE \"Alice\" CREATEDB;",
            "ALTER ROLE \"Alice\" RENAME TO \"Alice2\";",
            "GRANT \"Alice\" TO postgres;",
            "GRANT pg_read_all_data TO \"Alice\";",
            "DROP ROLE \"Alice\";",
            "ALTER ROLE \"Alice\" RESET chelabase.managed_role;",
            "ALTER ROLE \"Alice\" SET chelabase.managed_role = '';",
        ] {
            let err = as_owner(&db, &db.ep, statement).await.expect_err(statement);
            assert!(
                err.contains("managed by Chelabase") || err.contains("chelabase.managed_role"),
                "{statement}: {err}"
            );
        }
        assert!(is_marked(&db, &db.ep, "Alice").await, "still marked");

        // The login itself can't change its password or settings from SQL either.
        let conninfo = db.conninfo("Alice", password, false).await;
        let err = db.psql(&conninfo, "ALTER ROLE CURRENT_USER PASSWORD 'mine';").await.unwrap_err();
        assert!(err.contains("managed by Chelabase"), "{err}");
        let err = db.psql(&conninfo, "ALTER ROLE CURRENT_USER SET search_path = public;").await.unwrap_err();
        assert!(err.contains("managed by Chelabase"), "{err}");
        assert_eq!(db.psql(&conninfo, "SELECT current_user;").await.unwrap().trim(), "Alice");

        // `CREATE ROLE` on an existing name fails: the worker never adopts a customer's role.
        db.sql("postgres", "CREATE ROLE taken NOLOGIN;").await;
        assert!(db.worker.create_personal_login(&db.ep, "taken", &verifier).await.is_err());
        assert!(!is_marked(&db, &db.ep, "taken").await, "an existing role is never marked");

        // A child branch's compute guards it too.
        let (_child, child_ep) = create_child(&db, "dev").await;
        assert!(is_marked(&db, &child_ep, "Alice").await, "the marker came with the catalog");
        let err = as_owner(&db, &child_ep, "ALTER ROLE \"Alice\" PASSWORD 'owner-set';").await.unwrap_err();
        assert!(err.contains("managed by Chelabase"), "{err}");
    })
    .await;
}

/// RT4-4: a personal login the compute lost (dropped behind the platform's back as
/// `cloud_admin`) is made again by one resync, marked in the same script, so the owner's
/// `ALTER ROLE` on it is refused again.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the round-4 images"]
async fn a_recreated_personal_login_is_marked_again() {
    with_db(|db| async move {
        let password = "personal-live-pw";
        let verifier = chelabase_neon::scram::verifier(password);
        let branch = BranchId::from(db.id);
        db.worker.create_personal_login(&db.ep, "bob", &verifier).await.expect("create_personal_login");
        // The Members step records the row; until it does, the first resync records the role as
        // an SQL one and the test flips its origin, as the Members step will have made it.
        db.worker.resync(branch).await.expect("first resync");
        let flipped = sqlx::query("UPDATE neon_roles SET origin = 'personal' WHERE endpoint_id = $1 AND name = 'bob'")
            .bind(&db.ep)
            .execute(db.metadata.pool())
            .await
            .unwrap();
        assert_eq!(flipped.rows_affected(), 1, "the resync recorded the role");
        assert!(is_marked(&db, &db.ep, "bob").await);

        db.sql("postgres", "DROP OWNED BY bob; DROP ROLE bob;").await;
        assert_eq!(db.sql("postgres", "SELECT count(*) FROM pg_roles WHERE rolname = 'bob';").await.trim(), "0");

        db.worker.resync(branch).await.expect("the resync that recreates it");

        assert_eq!(db.sql("postgres", "SELECT count(*) FROM pg_roles WHERE rolname = 'bob';").await.trim(), "1");
        assert!(is_marked(&db, &db.ep, "bob").await, "marked again");
        let err = as_owner(&db, &db.ep, "ALTER ROLE bob PASSWORD 'owner-set';").await.unwrap_err();
        assert!(err.contains("managed by Chelabase"), "{err}");
        // Its password is the stored verifier: the login works, and is not a spec role.
        let conninfo = db.conninfo("bob", password, false).await;
        assert_eq!(db.psql(&conninfo, "SELECT current_user;").await.unwrap().trim(), "bob");
        let origin: String = sqlx::query_scalar("SELECT origin FROM neon_roles WHERE endpoint_id = $1 AND name = 'bob'")
            .bind(&db.ep)
            .fetch_one(db.metadata.pool())
            .await
            .unwrap();
        assert_eq!(origin, "personal", "the resync keeps the origin");
        let spec_roles = db.metadata.neon_roles(&db.ep).await.unwrap();
        assert!(spec_roles.iter().all(|r| r.name != "bob"), "never a compute-spec role: {spec_roles:?}");
    })
    .await;
}
