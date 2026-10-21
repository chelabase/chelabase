//! Fork round 3 on a real compute: the keys plugin, the realtime restriction, migrations before
//! `running`, the compute's TLS files and networks, certificate renewal. Needs the round-3 images.
//!
//! ```text
//! just workbench test -p worker --test compute_fork_live -- --run-ignored only
//! ```

use std::future::Future;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use chelabase_metadata::{DEMO_PROJECT_ID, DEMO_TENANT_ID, DatabaseStatus, MetadataClient};
use chelabase_neon::compute::{ComputeLocator, container_name};
use chelabase_neon::reconfigure::ComputeCtlStatus;
use chelabase_neon::ComputeBackend;
use chelabase_types::{BranchId, DatabaseId};
use tokio::io::AsyncWriteExt;
use worker::config::RealWorker;
use worker::ports::SpecPusher;

mod common;
use common::{cleanup, setup};

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

struct Db {
    worker: RealWorker,
    metadata: MetadataClient,
    id: DatabaseId,
    ep: String,
    database: String,
    stack: String,
    control_network: String,
}

impl Db {
    async fn sql(&self, database: &str, sql: &str) -> String {
        self.worker
            .computes
            .admin_sql(&self.ep, database, sql)
            .await
            .unwrap_or_else(|e| panic!("admin_sql {sql:?}: {e:#}"))
    }

    fn container(&self) -> String {
        // In a stack's runner the compose project (`CHELABASE_COMPOSE_PROJECT`, `chelabase-<stack>`)
        // names the containers; elsewhere the worker's own stack name does.
        match std::env::var("CHELABASE_COMPOSE_PROJECT") {
            Ok(project) => format!("{project}-compute-{}", self.ep),
            Err(_) => container_name(&self.stack, &self.ep),
        }
    }

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
        stack: config.stack.clone(),
        control_network: config.compute_control_network.clone(),
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
        let normal = db.conninfo("chelabase_realtime", "rt-live-pw", false).await;
        let repl = db.conninfo("chelabase_realtime", "rt-live-pw", true).await;
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
        let repl = db.conninfo("chelabase_realtime", "rt-live-pw", true).await;
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
