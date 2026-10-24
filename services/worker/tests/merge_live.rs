//! Schema merges (step 6g) against the real Neon stack: compute, rehearse, apply and finish
//! through `handle_merge_job` (the merge consumer's dispatch), after the status moves the
//! orchestrator makes before it publishes each job.
//!
//! Same setup as `branch_live` and `roles_live` (the workbench stack, so the proxy,
//! gateway and orchestrator run this branch, `CHELABASE_COMPUTE_IMAGE` set to the fork image),
//! plus `psql` and `curl` in the runner and the proxy's CA (`CHELABASE_PROXY_CA_FILE`, default `.chelabase/neon/proxy-ca.crt` in the checkout):
//!
//! ```text
//! just workbench test -p worker --test merge_live -- --run-ignored only
//! ```
//!
//! Each test provisions its own database and deletes it afterwards, pass or fail (`KEEP=1`
//! leaves it). Schema objects are made in the owner's session (`postgres`, as a user would);
//! roles on a branch are made as `cloud_admin` (no DDL forwarding, so the branch's metadata
//! doesn't learn them, as for a role made in SQL before the roles API knew it).

use std::future::Future;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chelabase_metadata::{
    BaseSource, BranchDeleteOutcome, BranchKind, BranchRecord, BranchStatus, CreateBranchOutcome,
    DEMO_PROJECT_ID, DEMO_TENANT_ID, DatabaseStatus, MergeCreate, MergeDirection, MergeDraftPatch,
    MergeRecord, MergeStatus, MergeStatusPatch, MergeWrite, MetadataClient, NewBranch, NewMerge,
    SettingsPatch,
};
use chelabase_nats::{BranchJob, MergeAfter, MergeJob, PointInTime};
use chelabase_neon::{ComputeBackend, Lsn, NeonId, SqlOpts};
use chelabase_schema::SchemaModel;
use chelabase_secrets::{Purpose, open_credential};
use chelabase_types::{BranchId, DatabaseId};
use serde_json::{Value, json};
use uuid::Uuid;
use worker::branch_job::Attempt;
use worker::config::RealWorker;
use worker::roles::{RoleOutcome, RoleSpec};

mod common;
use common::{cleanup, pageserver_container, proxy_ca_path, proxy_hostaddr, setup};

/// One provisioned database: main, its database name and the owner's password.
struct Fx {
    worker: RealWorker,
    metadata: MetadataClient,
    id: DatabaseId,
    main: BranchId,
    database: String,
    owner_password: String,
    storcon_url: String,
    /// An admin NeonJWT for the storage controller (it runs without `--dev`), signed with the
    /// stack's compute-JWT key pair as `branch_live`'s `admin_storcon` does.
    storcon_token: String,
}

/// Provisions a database, runs `body` on it, then deletes it, pass or fail (`KEEP=1` keeps it).
async fn live<F, Fut>(body: F)
where
    F: FnOnce(Arc<Fx>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (worker, metadata, config) = setup().await;
    let id = DatabaseId::new();
    let name = format!("live-{}", &id.to_string()[..8]);
    metadata
        .insert_database_pending(id, DEMO_TENANT_ID.into(), DEMO_PROJECT_ID.into(), &name)
        .await
        .expect("insert pending database");
    let started = Instant::now();
    let provisioned =
        tokio::time::timeout(Duration::from_secs(120), worker.handle_provision(id, &name)).await;
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
        database: record.database_name.clone().expect("database name"),
        main: main.id,
        worker,
        metadata,
        id,
        owner_password,
        storcon_url: config.storcon_url.clone(),
        storcon_token: chelabase_neon::jwt::storage_token_from_dir(
            &config.compute_jwt_dir,
            &chelabase_neon::jwt::StorageScope::Admin,
        )
        .expect("the compute-JWT key pair"),
    });

    // A panic inside the task is caught here, so the cleanup below always runs.
    let outcome = tokio::spawn(body(fx.clone())).await;
    if std::env::var("KEEP").as_deref() == Ok("1") {
        println!("KEEP=1: database {} ({}) left in place", name, fx.id);
    } else {
        cleanup(&fx.worker, &fx.metadata, fx.id).await;
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

/// The last non-empty line of psql's unaligned, tuples-only output.
fn last_line(out: &str) -> String {
    out.lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

impl Fx {
    async fn branch(&self, id: BranchId) -> BranchRecord {
        self.metadata
            .get_branch(id)
            .await
            .unwrap()
            .expect("branch row")
    }

    async fn ep(&self, id: BranchId) -> String {
        self.branch(id).await.endpoint_id.expect("branch endpoint")
    }

    /// SQL as `cloud_admin` on the branch, in the database.
    async fn try_admin(&self, b: BranchId, sql: &str) -> Result<String, String> {
        let ep = self.ep(b).await;
        self.worker
            .computes
            .admin_sql(&ep, &self.database, sql)
            .await
            .map_err(|e| format!("{e:#}"))
    }

    async fn admin(&self, b: BranchId, sql: &str) -> String {
        self.try_admin(b, sql)
            .await
            .unwrap_or_else(|e| panic!("as cloud_admin: {sql}: {e}"))
    }

    /// SQL in the owner's own session (`psql -h 127.0.0.1 -U postgres`, as the merge runs it).
    async fn try_owner(&self, b: BranchId, sql: &str) -> Result<String, String> {
        let ep = self.ep(b).await;
        let opts = SqlOpts {
            timeout: Duration::from_secs(60),
            user: Some("postgres".into()),
            ..SqlOpts::admin_default()
        };
        self.worker
            .computes
            .admin_sql_with(&ep, &self.database, sql, &opts)
            .await
            .map_err(|e| format!("{e:#}"))
    }

    async fn owner(&self, b: BranchId, sql: &str) -> String {
        self.try_owner(b, sql)
            .await
            .unwrap_or_else(|e| panic!("as postgres: {sql}: {e}"))
    }

    /// A one-value query's answer (as `cloud_admin`).
    async fn value(&self, b: BranchId, sql: &str) -> String {
        last_line(&self.admin(b, sql).await)
    }

    /// Makes a child of `parent` (the worker's create job) and waits for it.
    async fn child(&self, name: &str, parent: BranchId) -> BranchId {
        let child = BranchId::new();
        let outcome = self
            .metadata
            .create_branch(&NewBranch {
                id: child,
                database_id: self.id,
                name,
                parent_branch_id: parent,
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
        let record = self.branch(child).await;
        assert_eq!(record.status, BranchStatus::Ready, "{:?}", record.error);
        child
    }

    // --- The orchestrator's moves, then the job --------------------------------------------

    async fn merge(&self, id: Uuid) -> MergeRecord {
        self.metadata.merge(id).await.unwrap().expect("merge row")
    }

    /// A job's first delivery, as the API publishes it (an `Apply` delivered again is
    /// recovery, so only a first delivery runs a fresh apply); other jobs as their last
    /// delivery, so a failure is recorded rather than left for a redelivery.
    async fn job(&self, job: MergeJob) {
        let started = Instant::now();
        let name = format!("{job:?}");
        let attempt = match job {
            MergeJob::Apply { .. } => Attempt(1),
            _ => Attempt::LAST,
        };
        self.worker
            .handle_merge_job(job, attempt)
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        println!("{name} took {:?}", started.elapsed());
    }

    /// `createMerge`: the row (computing), then the compute job.
    async fn create_merge(
        &self,
        source: BranchId,
        target: BranchId,
        direction: MergeDirection,
        options: Option<Value>,
    ) -> MergeRecord {
        let id = Uuid::new_v4();
        let created = self
            .metadata
            .create_merge(&NewMerge {
                id,
                database_id: self.id,
                source_branch_id: source,
                target_branch_id: target,
                direction,
                database_name: self.database.clone(),
                options,
                created_by: "merge_live".into(),
            })
            .await
            .unwrap();
        assert!(matches!(created, MergeCreate::Created(_)), "{created:?}");
        self.job(MergeJob::Compute { merge_id: id }).await;
        self.merge(id).await
    }

    async fn merge_into_main(&self, dev: BranchId) -> MergeRecord {
        self.create_merge(dev, self.main, MergeDirection::IntoParent, None)
            .await
    }

    /// `updateMerge`: saves `patch` at the current version and recomputes.
    async fn save(&self, id: Uuid, mut patch: MergeDraftPatch) -> MergeRecord {
        patch.start_compute = true;
        let version = self.merge(id).await.version;
        let saved = self
            .metadata
            .update_merge_draft(id, version, &patch)
            .await
            .unwrap();
        assert!(matches!(saved, MergeWrite::Saved(_)), "{saved:?}");
        self.job(MergeJob::Compute { merge_id: id }).await;
        self.merge(id).await
    }

    async fn resolve(&self, id: Uuid, resolutions: Value) -> MergeRecord {
        self.save(
            id,
            MergeDraftPatch {
                resolutions: Some(resolutions),
                ..Default::default()
            },
        )
        .await
    }

    async fn steps(&self, id: Uuid, steps: Value) -> MergeRecord {
        self.save(
            id,
            MergeDraftPatch {
                user_steps: Some(steps),
                ..Default::default()
            },
        )
        .await
    }

    /// `rehearseMerge`: draft → rehearsing, then the job.
    async fn rehearse(&self, id: Uuid) -> MergeRecord {
        assert!(
            self.metadata
                .set_merge_status(
                    id,
                    &[MergeStatus::Draft],
                    MergeStatus::Rehearsing,
                    MergeStatusPatch::default()
                )
                .await
                .unwrap(),
            "the merge isn't a draft: {:?}",
            self.merge(id).await.status
        );
        self.job(MergeJob::Rehearse { merge_id: id }).await;
        self.merge(id).await
    }

    /// `applyMerge`'s move: rehearsed → applying (error cleared).
    async fn start_apply(&self, id: Uuid) {
        assert!(
            self.metadata
                .set_merge_status(
                    id,
                    &[MergeStatus::Rehearsed],
                    MergeStatus::Applying,
                    // As `applyMerge` does: the attempt's start logged (I2), both branches
                    // ready under their locks (M12).
                    MergeStatusPatch {
                        error: Some(None),
                        guard_branches_ready: true,
                        log_status: true,
                        ..Default::default()
                    }
                )
                .await
                .unwrap(),
            "the merge isn't rehearsed: {:?}",
            self.merge(id).await.status
        );
    }

    /// `applyMerge`: the move, then the job.
    async fn apply(&self, id: Uuid) -> MergeRecord {
        self.start_apply(id).await;
        self.job(MergeJob::Apply { merge_id: id }).await;
        self.merge(id).await
    }

    /// `retryMerge` (E20): online resumes; roles, enum_add and tx apply from the start.
    async fn retry(&self, id: Uuid) -> MergeRecord {
        let m = self.merge(id).await;
        assert_eq!(m.status, MergeStatus::Failed);
        let (to, job) = match m.failed_phase.as_deref() {
            Some("online") => (MergeStatus::Online, MergeJob::Resume { merge_id: id }),
            Some("roles" | "enum_add" | "tx") => {
                (MergeStatus::Applying, MergeJob::Apply { merge_id: id })
            }
            other => panic!("no retry from failed phase {other:?}"),
        };
        assert!(
            self.metadata
                .set_merge_status(
                    id,
                    &[MergeStatus::Failed],
                    to,
                    MergeStatusPatch {
                        error: Some(None),
                        failed_phase: Some(None),
                        guard_failed_phase: Some(m.failed_phase.clone()),
                        log_status: to == MergeStatus::Applying,
                        ..Default::default()
                    }
                )
                .await
                .unwrap()
        );
        self.job(job).await;
        self.merge(id).await
    }

    /// `finishMerge` (E21): the request in `result.after`, then the job.
    async fn finish(&self, id: Uuid, after: MergeAfter) -> MergeRecord {
        let m = self.merge(id).await;
        assert_eq!(m.status, MergeStatus::AppliedWithDrift);
        let mut result = m.result.clone().unwrap_or_else(|| json!({}));
        assert!(
            result["after"]["skipped"].is_string(),
            "the after-merge choice wasn't skipped: {result}"
        );
        result["after"] = json!({ "choice": after.as_str(), "requested": true });
        assert!(
            self.metadata
                .set_merge_status(
                    id,
                    &[MergeStatus::AppliedWithDrift],
                    MergeStatus::AppliedWithDrift,
                    MergeStatusPatch {
                        result: Some(result),
                        ..Default::default()
                    }
                )
                .await
                .unwrap()
        );
        self.job(MergeJob::Finish {
            merge_id: id,
            after,
        })
        .await;
        self.merge(id).await
    }

    /// Waits until `check` holds on the merge (polling every 200 ms), or panics after `max`.
    async fn wait_merge(
        &self,
        id: Uuid,
        max: Duration,
        what: &str,
        check: impl Fn(&MergeRecord) -> bool,
    ) -> MergeRecord {
        let started = Instant::now();
        loop {
            let m = self.merge(id).await;
            if check(&m) {
                return m;
            }
            assert!(
                started.elapsed() < max,
                "waited {max:?} for {what}; the merge is {:?}: {:?}",
                m.status,
                m.error
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

// --- Reading a merge -------------------------------------------------------------------------

/// A draft with a plan and nothing open, or panic with what the compute said.
fn assert_planned(m: &MergeRecord) {
    assert_eq!(m.status, MergeStatus::Draft, "{:?}", m.error);
    assert_eq!(m.error, None, "compute error");
    let resolved: Vec<&str> = m
        .resolutions
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["conflict"].as_str())
        .collect();
    let open: Vec<Value> = conflicts(m)
        .into_iter()
        .filter(|c| !resolved.contains(&c["id"].as_str().unwrap_or("")))
        .collect();
    assert!(
        open.is_empty(),
        "open conflicts: {}",
        serde_json::to_string_pretty(&open).unwrap()
    );
    assert!(m.plan.is_some(), "no plan");
}

fn conflicts(m: &MergeRecord) -> Vec<Value> {
    m.analysis
        .as_ref()
        .and_then(|a| a["conflicts"].as_array().cloned())
        .unwrap_or_default()
}

fn items(m: &MergeRecord) -> Vec<Value> {
    m.analysis
        .as_ref()
        .and_then(|a| a["items"].as_array().cloned())
        .unwrap_or_default()
}

fn plan_steps(m: &MergeRecord) -> Vec<Value> {
    m.plan
        .as_ref()
        .and_then(|p| p["steps"].as_array().cloned())
        .unwrap_or_default()
}

/// The plan's SQL, one step per line (`<phase> <id>: <sql>`), for messages and searches.
fn plan_text(m: &MergeRecord) -> String {
    plan_steps(m)
        .iter()
        .map(|s| {
            format!(
                "{} {}: {}",
                s["phase"].as_str().unwrap_or("?"),
                s["id"].as_str().unwrap_or("?"),
                s["sql"].as_str().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn rehearsal_drift(m: &MergeRecord) -> Vec<Value> {
    m.rehearsal
        .as_ref()
        .and_then(|r| r["drift"].as_array().cloned())
        .unwrap_or_default()
}

fn result_drift(m: &MergeRecord) -> Vec<Value> {
    m.result
        .as_ref()
        .and_then(|r| r["drift"].as_array().cloned())
        .unwrap_or_default()
}

/// Rehearsed with no drift (other than what own SQL produced).
fn assert_rehearsed_clean(m: &MergeRecord) {
    assert_eq!(
        m.status,
        MergeStatus::Rehearsed,
        "{:?}\nrehearsal: {}",
        m.error,
        serde_json::to_string_pretty(&m.rehearsal).unwrap()
    );
    let drift: Vec<Value> = rehearsal_drift(m)
        .into_iter()
        .filter(|d| d["produced"] != json!(true))
        .collect();
    assert!(
        drift.is_empty(),
        "rehearsal drift: {}",
        serde_json::to_string_pretty(&drift).unwrap()
    );
}

/// Done with no drift.
fn assert_done(m: &MergeRecord) {
    assert_eq!(
        m.status,
        MergeStatus::Done,
        "{:?} (failed phase {:?})\nresult: {}",
        m.error,
        m.failed_phase,
        serde_json::to_string_pretty(&m.result).unwrap()
    );
    let drift: Vec<Value> = result_drift(m)
        .into_iter()
        .filter(|d| d["produced"] != json!(true))
        .collect();
    assert!(
        drift.is_empty(),
        "drift after apply: {}",
        serde_json::to_string_pretty(&drift).unwrap()
    );
}

/// The merge's log as `kind step phase outcome` / `status <to>` lines.
async fn log_lines(fx: &Fx, id: Uuid) -> Vec<String> {
    fx.metadata
        .merge_events(id)
        .await
        .unwrap()
        .into_iter()
        .map(|e| match e.kind.as_str() {
            "status" => format!(
                "status {}",
                e.detail
                    .as_ref()
                    .and_then(|d| d["to"].as_str())
                    .unwrap_or("?")
            ),
            _ => format!(
                "{} {} {} {}",
                e.kind,
                e.phase.unwrap_or_default(),
                e.step_id.unwrap_or_default(),
                e.outcome.unwrap_or_default()
            ),
        })
        .collect()
}

// --- Through the proxy and the gateway -----------------------------------------------------

fn proxy_host(ep: &str) -> String {
    let domain = std::env::var("CHELABASE_PROXY_DOMAIN")
        .ok()
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| "localtest.me".into());
    format!("{ep}.{domain}")
}

fn proxy_pg_port() -> u16 {
    std::env::var("CHELABASE_PROXY_PG_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(4432)
}

/// `SELECT current_user` through the proxy's Postgres port as `user`: the answer or psql's error.
async fn proxy_login(
    ep: &str,
    database: &str,
    user: &str,
    password: &str,
) -> Result<String, String> {
    let ca = proxy_ca_path();
    assert!(
        ca.exists(),
        "{} is missing (run `just workbench up`)",
        ca.display()
    );
    // The proxy is dialled at CHELABASE_PROXY_HOSTADDR when set; the TLS host name stays <ep>.<domain>.
    let hostaddr = proxy_hostaddr();
    let out = tokio::process::Command::new("psql")
        .arg(format!(
            "host={} hostaddr={hostaddr} port={} dbname={database} sslmode=verify-full sslrootcert={}",
            proxy_host(ep),
            proxy_pg_port(),
            ca.display()
        ))
        .env("PGUSER", user)
        .env("PGPASSWORD", password)
        .env("PGCONNECT_TIMEOUT", "10")
        .args(["-X", "-qtA", "-c", "SELECT current_user"])
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

/// A data API select of `t` on the gateway with `key`: the status (0 when curl fails). The key
/// goes through stdin (`-H @-`), never argv.
async fn data_select_status(key: &str) -> u16 {
    use tokio::io::AsyncWriteExt;
    let mut child = tokio::process::Command::new("curl")
        .args([
            "-sS",
            "--max-time",
            "20",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
        ])
        .args([
            "-H",
            "@-",
            "-H",
            "content-type: application/json",
            "-X",
            "POST",
        ])
        .args(["-d", r#"{"v":1,"op":"select","from":"t"}"#])
        .arg(format!("{}/data/v1/query", gateway()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn curl");
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(format!("Authorization: Bearer {key}\n").as_bytes())
        .await
        .unwrap();
    drop(stdin);
    let out = child.wait_with_output().await.unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

/// The branch's live public data key, unsealed.
async fn public_key(fx: &Fx, branch: BranchId) -> Option<String> {
    let keys = fx.metadata.list_data_keys(branch).await.unwrap();
    let key = keys
        .into_iter()
        .find(|k| k.kind == chelabase_types::data_key::DataKeyKind::Public)?;
    let sealed = key.sealed?;
    Some(
        open_credential(
            &fx.worker.key,
            &key.id.to_string(),
            Purpose::DataKey,
            &sealed,
        )
        .expect("open the public key"),
    )
}

// =============================================================================================
// Scenarios
// =============================================================================================

/// The base schema every scenario starts from, made on main before `dev` is branched.
const BASE: &str = "CREATE TYPE mood AS ENUM ('ok', 'sad');
CREATE TABLE t (id bigint PRIMARY KEY, m mood, note text);
INSERT INTO t VALUES (1, 'ok', 'a'), (2, 'sad', 'b');";

/// main with [`BASE`], and `dev` branched from it.
async fn base_and_dev(fx: &Fx) -> BranchId {
    fx.owner(fx.main, BASE).await;
    fx.child("dev", fx.main).await
}

/// A branch's change into main, rehearsed and applied with keep, through every phase: an enum
/// value used by a new default (phase 1a), a new column and table (phase 1), a new index
/// (phase 2). E15: phases 1a, 1 and 2 run in the owner's session, so the new objects are the
/// owner's. Review Focus 5 and M11: main serves `public` through the data API (provision
/// bootstraps it), so the new table gets `anon`/`authenticated` grants from the default
/// privileges; neither the rehearsal nor the apply reports them as drift, and the rehearsal
/// (whose branch build runs the data API bootstrap again) isn't stale.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn merge_into_parent_end_to_end() {
    live(|fx| async move {
        let dev = base_and_dev(&fx).await;
        fx.owner(
            dev,
            "ALTER TYPE mood ADD VALUE 'meh';
             ALTER TABLE t ADD COLUMN score int NOT NULL DEFAULT 0;
             CREATE INDEX t_note_idx ON t (note);
             CREATE TABLE items (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                                 t_id bigint REFERENCES t (id), feel mood DEFAULT 'meh');",
        )
        .await;

        let m = fx.merge_into_main(dev).await;
        assert_planned(&m);
        assert_eq!(m.options["lock_timeout_ms"], json!(5000), "R2's default");
        let phases: Vec<String> = plan_steps(&m)
            .iter()
            .map(|s| s["phase"].as_str().unwrap_or("").to_string())
            .collect();
        for phase in ["enum_add", "tx", "online"] {
            assert!(
                phases.iter().any(|p| p == phase),
                "no {phase} step:\n{}",
                plan_text(&m)
            );
        }

        let m = fx.rehearse(m.id).await;
        assert_rehearsed_clean(&m);
        // The rehearsal is gone (its row is kept, deleted).
        let rehearsals: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, status FROM branches WHERE database_id = $1 AND kind = 'rehearsal'",
        )
        .bind(fx.id.as_uuid())
        .fetch_all(fx.metadata.pool())
        .await
        .unwrap();
        assert_eq!(
            rehearsals,
            [(
                format!("_rehearsal-{}", &m.id.simple().to_string()[..8]),
                "deleted".to_string()
            )]
        );

        let m = fx.apply(m.id).await;
        assert_done(&m);
        assert_eq!(m.result.as_ref().unwrap()["after"], json!({"choice": "keep", "base": "recorded"}));

        // main has the change, made by the owner.
        assert_eq!(
            fx.value(fx.main, "SELECT enum_range(NULL::mood)::text;").await,
            "{ok,sad,meh}"
        );
        assert_eq!(
            fx.value(
                fx.main,
                "SELECT string_agg(column_name || ':' || is_nullable || ':' || column_default, ',' ORDER BY ordinal_position) \
                 FROM information_schema.columns WHERE table_name = 't' AND column_name = 'score';"
            )
            .await,
            "score:NO:0"
        );
        assert_eq!(
            fx.value(
                fx.main,
                "SELECT i.indisvalid FROM pg_index i WHERE i.indexrelid = 'public.t_note_idx'::regclass;"
            )
            .await,
            "t"
        );
        assert_eq!(
            fx.value(fx.main, "SELECT tableowner FROM pg_tables WHERE tablename = 'items';").await,
            "postgres",
            "E15: phase 1 ran as the owner"
        );
        assert_eq!(
            fx.value(
                fx.main,
                "SELECT pg_get_userbyid(c.relowner) FROM pg_class c WHERE c.oid = 'public.t_note_idx'::regclass;"
            )
            .await,
            "postgres",
            "E15: phase 2 ran as the owner"
        );
        assert_eq!(
            fx.value(fx.main, "SELECT has_table_privilege('anon', 'public.items', 'SELECT');").await,
            "t",
            "the data API's default privileges gave the new table to anon"
        );
        // The recovery marker (P1) is there, and only the owner's SELECT and INSERT.
        assert_eq!(
            fx.value(
                fx.main,
                &format!("SELECT count(*) FROM chelabase_merge.applied WHERE merge_id = '{}';", m.id)
            )
            .await,
            "1"
        );
        // The log Studio reads.
        let log = log_lines(&fx, m.id).await;
        println!("log: {log:#?}");
        for line in [
            "step tx phase1 ok",
            "status online",
            "status finishing",
            "status done",
        ] {
            assert!(log.iter().any(|l| l == line), "{line:?} not in {log:?}");
        }
        // Keep: dev's base is now the merge's.
        let base = fx
            .metadata
            .schema_base(dev, &fx.database)
            .await
            .unwrap()
            .expect("dev's base");
        assert_eq!(base.source, BaseSource::Merge);
    })
    .await;
}

/// main's change into its child: the `update` base on the child.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn update_from_parent_end_to_end() {
    live(|fx| async move {
        let dev = base_and_dev(&fx).await;
        fx.owner(
            fx.main,
            "CREATE TABLE fresh (id int PRIMARY KEY, label text);
             ALTER TABLE t ADD COLUMN extra text;",
        )
        .await;
        let m = fx
            .create_merge(fx.main, dev, MergeDirection::FromParent, None)
            .await;
        assert_planned(&m);
        let m = fx.rehearse(m.id).await;
        assert_rehearsed_clean(&m);
        let m = fx.apply(m.id).await;
        assert_done(&m);
        assert_eq!(
            fx.value(dev, "SELECT count(*) FROM information_schema.columns WHERE (table_name, column_name) IN (('fresh','label'),('t','extra'));").await,
            "2"
        );
        let base = fx
            .metadata
            .schema_base(dev, &fx.database)
            .await
            .unwrap()
            .expect("dev's base");
        assert_eq!(base.source, BaseSource::Update);
        println!("result: {}", m.result.unwrap());
    })
    .await;
}

/// A rename on the branch and a new column on main merge by OID: main ends with the renamed
/// table and column and keeps its own new column.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn rename_on_branch_with_new_column_on_main() {
    live(|fx| async move {
        fx.owner(fx.main, "CREATE TABLE orders (id int PRIMARY KEY, total int);")
            .await;
        let dev = fx.child("dev", fx.main).await;
        fx.owner(
            dev,
            "ALTER TABLE orders RENAME TO purchases; ALTER TABLE purchases RENAME COLUMN total TO amount;",
        )
        .await;
        fx.owner(fx.main, "ALTER TABLE orders ADD COLUMN note text;")
            .await;
        let m = fx.merge_into_main(dev).await;
        assert_planned(&m);
        let text = plan_text(&m);
        assert!(text.contains("RENAME"), "{text}");
        assert!(!text.contains("DROP TABLE"), "{text}");
        let m = fx.rehearse(m.id).await;
        assert_rehearsed_clean(&m);
        let m = fx.apply(m.id).await;
        assert_done(&m);
        assert_eq!(
            fx.value(
                fx.main,
                "SELECT string_agg(column_name, ',' ORDER BY ordinal_position) FROM information_schema.columns WHERE table_name = 'purchases';"
            )
            .await,
            "id,amount,note"
        );
        assert_eq!(fx.value(fx.main, "SELECT to_regclass('public.orders') IS NULL;").await, "t");
    })
    .await;
}

/// Every resolution choice: keep target, take source, own SQL, and the target's columns first.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn each_resolution_choice() {
    live(|fx| async move {
        fx.owner(
            fx.main,
            "CREATE TABLE c (id int PRIMARY KEY, a int DEFAULT 1, b int DEFAULT 1, d int DEFAULT 1);",
        )
        .await;
        let dev = fx.child("dev", fx.main).await;
        fx.owner(
            dev,
            "ALTER TABLE c ALTER COLUMN a SET DEFAULT 2, ALTER COLUMN b SET DEFAULT 2,
                           ALTER COLUMN d SET DEFAULT 2, ADD COLUMN from_dev int;",
        )
        .await;
        fx.owner(
            fx.main,
            "ALTER TABLE c ALTER COLUMN a SET DEFAULT 3, ALTER COLUMN b SET DEFAULT 3,
                           ALTER COLUMN d SET DEFAULT 3, ADD COLUMN from_main int;",
        )
        .await;
        let m = fx.merge_into_main(dev).await;
        assert_eq!(m.status, MergeStatus::Draft, "{:?}", m.error);
        assert!(m.plan.is_none(), "no plan while conflicts are open");
        let open = conflicts(&m);
        println!("conflicts: {}", serde_json::to_string_pretty(&open).unwrap());
        let mut resolutions = Vec::new();
        for c in &open {
            let id = c["id"].as_str().unwrap();
            let text = c.to_string();
            let choice = if c["kind"] == "column_order" {
                json!("target_columns_first")
            } else if text.contains("\"name\":\"a\"") {
                json!("keep_target")
            } else if text.contains("\"name\":\"b\"") {
                json!("take_source")
            } else if text.contains("\"name\":\"d\"") {
                json!({ "own_sql": "ALTER TABLE c ALTER COLUMN d SET DEFAULT 4" })
            } else {
                panic!("unexpected conflict {c}");
            };
            resolutions.push(json!({ "conflict": id, "choice": choice }));
        }
        assert_eq!(resolutions.len(), 4, "a, b, d and the column order");
        let m = fx.resolve(m.id, Value::Array(resolutions)).await;
        assert_planned(&m);
        assert_eq!(conflicts(&m).len(), 4, "the analysis still lists them");
        let m = fx.rehearse(m.id).await;
        assert_rehearsed_clean(&m);
        let m = fx.apply(m.id).await;
        assert_done(&m);
        assert_eq!(
            fx.value(
                fx.main,
                "SELECT string_agg(column_name || '=' || coalesce(column_default, '-'), ',' ORDER BY ordinal_position) \
                 FROM information_schema.columns WHERE table_name = 'c';"
            )
            .await,
            "id=-,a=3,b=2,d=4,from_main=-,from_dev=-"
        );
    })
    .await;
}

/// A NOT NULL that main's rows break fails the rehearsal at its step with Postgres's error;
/// a backfill step before it makes it pass. E15: a step that tries to become `cloud_admin` fails
/// with permission denied in the owner's session, and `RESET ROLE` is refused before planning.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn not_null_fails_in_rehearsal_then_passes_with_backfill_step() {
    live(|fx| async move {
        fx.owner(
            fx.main,
            "CREATE TABLE n (id int PRIMARY KEY, v int); INSERT INTO n VALUES (1, 5);",
        )
        .await;
        let dev = fx.child("dev", fx.main).await;
        fx.owner(dev, "ALTER TABLE n ALTER COLUMN v SET NOT NULL;")
            .await;
        fx.owner(fx.main, "INSERT INTO n VALUES (2, NULL);").await;

        let m = fx.merge_into_main(dev).await;
        assert_planned(&m);
        let m = fx.rehearse(m.id).await;
        assert_eq!(m.status, MergeStatus::Draft, "{:?}", m.rehearsal);
        let error = m.error.clone().unwrap_or_default();
        println!("rehearsal error: {error}");
        assert!(error.starts_with("the rehearsal failed at step "), "{error}");
        assert!(error.contains("violated"), "{error}");
        let r = m.rehearsal.clone().unwrap();
        assert!(r["failed_step"].is_string(), "{r}");
        assert!(r["error"].as_str().unwrap_or("").contains("violated"), "{r}");

        // E15: the owner's session can't become cloud_admin.
        let m = fx
            .steps(
                m.id,
                json!([{ "id": "escalate", "after": null,
                         "sql": "SELECT set_config('role', 'cloud_admin', false)" }]),
            )
            .await;
        assert_planned(&m);
        let m = fx.rehearse(m.id).await;
        assert_eq!(m.status, MergeStatus::Draft);
        let error = m.error.clone().unwrap_or_default();
        println!("escalation: {error}");
        assert!(
            error.contains("permission denied to set role \"cloud_admin\""),
            "{error}"
        );
        assert!(error.contains("escalate"), "the failing step is named: {error}");
        let m = fx
            .steps(
                m.id,
                json!([{ "id": "reset", "after": null, "sql": "RESET ROLE" }]),
            )
            .await;
        assert_eq!(m.status, MergeStatus::Draft);
        assert!(m.plan.is_none());
        let error = m.error.clone().unwrap_or_default();
        println!("RESET ROLE: {error}");
        assert!(error.contains("step reset: "), "{error}");

        // The backfill.
        let m = fx
            .steps(
                m.id,
                json!([{ "id": "backfill", "after": null,
                         "sql": "UPDATE n SET v = 0 WHERE v IS NULL" }]),
            )
            .await;
        assert_planned(&m);
        let m = fx.rehearse(m.id).await;
        assert_rehearsed_clean(&m);
        let m = fx.apply(m.id).await;
        assert_done(&m);
        assert_eq!(
            fx.value(fx.main, "SELECT string_agg(id || '=' || v, ',' ORDER BY id) FROM n;").await,
            "1=5,2=0"
        );
        assert_eq!(
            fx.value(
                fx.main,
                "SELECT attnotnull FROM pg_attribute WHERE attrelid = 'public.n'::regclass AND attname = 'v';"
            )
            .await,
            "t"
        );
        assert_eq!(
            fx.value(
                fx.main,
                "SELECT count(*) FROM pg_constraint WHERE conname LIKE 'chelabase_merge_nn_%';"
            )
            .await,
            "0",
            "the temporary check is gone"
        );
    })
    .await;
}

/// U1: a NOLOGIN group, a login role in it and a BYPASSRLS role go through the control plane.
/// T8a: the rehearsal makes them NOLOGIN and shows no drift for it. P9/M8: the BYPASSRLS role
/// needs its acknowledgement when the apply starts. U3: `result.created_roles`.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn nologin_group_role_and_login_role_created_through_control_plane() {
    live(|fx| async move {
        let dev = base_and_dev(&fx).await;
        // As a user makes them in SQL: in the owner's session, which also makes the owner a
        // member (an ADMIN grant and the self-grant), as the merge's roles path does. E22: those
        // are never planned, and the expected model simulates them on the target.
        fx.owner(
            dev,
            "CREATE ROLE app_readers NOLOGIN;
             CREATE ROLE app_user LOGIN IN ROLE app_readers;
             CREATE ROLE app_auditor NOLOGIN BYPASSRLS;",
        )
        .await;
        fx.owner(dev, "GRANT SELECT ON t TO app_readers;").await;

        let m = fx.merge_into_main(dev).await;
        assert_planned(&m);
        println!("plan:\n{}", plan_text(&m));
        let security: Vec<Value> = plan_steps(&m)
            .into_iter()
            .filter(|s| s["security"] == json!(true))
            .collect();
        assert_eq!(security.len(), 1, "the BYPASSRLS role:\n{}", plan_text(&m));

        let m = fx.rehearse(m.id).await;
        assert_rehearsed_clean(&m);

        // Without the acknowledgement the worker refuses at the start (M8).
        let m = fx.apply(m.id).await;
        assert_eq!(m.status, MergeStatus::Rehearsed, "{:?}", m.error);
        let error = m.error.clone().unwrap_or_default();
        assert!(error.starts_with("merge_drops_unacknowledged"), "{error}");
        assert_eq!(
            fx.value(fx.main, "SELECT count(*) FROM pg_roles WHERE rolname LIKE 'app\\_%';").await,
            "0",
            "nothing ran"
        );

        let acks: Vec<Value> = security
            .iter()
            .flat_map(|s| {
                let items = s["items"].as_array().cloned().unwrap_or_default();
                if items.is_empty() { vec![s["id"].clone()] } else { items }
            })
            .collect();
        let m = fx
            .save(
                m.id,
                MergeDraftPatch {
                    acknowledgements: Some(Value::Array(acks)),
                    ..Default::default()
                },
            )
            .await;
        assert_planned(&m);
        let m = fx.rehearse(m.id).await;
        assert_rehearsed_clean(&m);
        let m = fx.apply(m.id).await;
        assert_done(&m);

        assert_eq!(
            fx.value(
                fx.main,
                "SELECT string_agg(rolname || ':' || rolcanlogin || ':' || rolbypassrls, ',' ORDER BY rolname) \
                 FROM pg_roles WHERE rolname LIKE 'app\\_%';"
            )
            .await,
            "app_auditor:false:true,app_readers:false:false,app_user:true:false"
        );
        assert_eq!(
            fx.value(fx.main, "SELECT pg_has_role('app_user', 'app_readers', 'MEMBER');").await,
            "t"
        );
        assert_eq!(
            fx.value(fx.main, "SELECT has_table_privilege('app_readers', 'public.t', 'SELECT');").await,
            "t"
        );
        let created = m.result.as_ref().unwrap()["created_roles"].clone();
        let mut names: Vec<String> = created
            .as_array()
            .unwrap()
            .iter()
            .map(|r| {
                assert_eq!(r["branch_id"], json!(fx.main.to_string()), "{r}");
                r["name"].as_str().unwrap().to_string()
            })
            .collect();
        names.sort();
        assert_eq!(names, ["app_auditor", "app_readers", "app_user"]);
        // The login role's password can be revealed from the Connection panel (U3).
        let ep = fx.ep(fx.main).await;
        assert!(
            fx.metadata.role_password(&ep, "app_user").await.unwrap().is_some(),
            "app_user has a revealable password"
        );
    })
    .await;
}

/// After-merge choices: reset (the source goes back to its parent), delete refused by a child
/// (recorded, the merge still done), and delete.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn keep_reset_delete_after_merge() {
    live(|fx| async move {
        fx.owner(fx.main, BASE).await;

        // Reset.
        let a = fx.child("a", fx.main).await;
        fx.owner(a, "CREATE TABLE from_a (id int);").await;
        let before = fx.branch(a).await.timeline_id;
        let m = fx
            .create_merge(
                a,
                fx.main,
                MergeDirection::IntoParent,
                Some(json!({ "lock_timeout_ms": 5000, "after": "reset" })),
            )
            .await;
        assert_planned(&m);
        let m = fx.rehearse(m.id).await;
        assert_rehearsed_clean(&m);
        let m = fx.apply(m.id).await;
        assert_done(&m);
        assert_eq!(
            m.result.as_ref().unwrap()["after"],
            json!({"choice": "reset", "outcome": "done"})
        );
        let a_now = fx.branch(a).await;
        assert_eq!(a_now.status, BranchStatus::Ready);
        assert_ne!(a_now.timeline_id, before, "reset to its parent");

        // Delete refused by a child.
        let b = fx.child("b", fx.main).await;
        let _b_child = fx.child("b-child", b).await;
        fx.owner(b, "CREATE TABLE from_b (id int);").await;
        let m = fx
            .create_merge(
                b,
                fx.main,
                MergeDirection::IntoParent,
                Some(json!({ "lock_timeout_ms": 5000, "after": "delete" })),
            )
            .await;
        assert_planned(&m);
        let m = fx.rehearse(m.id).await;
        assert_rehearsed_clean(&m);
        let m = fx.apply(m.id).await;
        assert_done(&m);
        let after = m.result.as_ref().unwrap()["after"].clone();
        println!("delete with a child: {after}");
        assert_eq!(after["choice"], "delete");
        assert!(
            after["refused"].is_string() || after["refused"].is_object(),
            "{after}"
        );
        assert_eq!(fx.branch(b).await.status, BranchStatus::Ready);

        // Delete.
        let c = fx.child("c", fx.main).await;
        fx.owner(c, "CREATE TABLE from_c (id int);").await;
        let m = fx
            .create_merge(
                c,
                fx.main,
                MergeDirection::IntoParent,
                Some(json!({ "lock_timeout_ms": 5000, "after": "delete" })),
            )
            .await;
        assert_planned(&m);
        let m = fx.rehearse(m.id).await;
        assert_rehearsed_clean(&m);
        let m = fx.apply(m.id).await;
        assert_done(&m);
        assert_eq!(
            m.result.as_ref().unwrap()["after"],
            json!({"choice": "delete", "outcome": "done"})
        );
        assert_eq!(fx.branch(c).await.status, BranchStatus::Deleted);
        assert_eq!(
            fx.value(
                fx.main,
                "SELECT count(*) FROM pg_tables WHERE tablename IN ('from_a', 'from_b', 'from_c');"
            )
            .await,
            "3"
        );
    })
    .await;
}

/// T14 F3 fix round (I1, E27): the after-merge reset and delete of the source are refused while
/// another merge holds it (here an update of the source from main, failed half applied): the
/// refusal names that merge, the source keeps its timeline, and the merge is still done.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn the_after_merge_choice_waits_for_a_merge_holding_the_source() {
    live(|fx| async move {
        fx.owner(fx.main, BASE).await;
        let a = fx.child("a", fx.main).await;
        let update = fx.create_merge(fx.main, a, MergeDirection::FromParent, None).await;
        let half_applied = MergeStatusPatch {
            failed_phase: Some(Some("online".into())),
            error: Some(Some("step o1: connection reset".into())),
            ..Default::default()
        };
        assert!(fx
            .metadata
            .set_merge_status(update.id, &MergeStatus::ALL, MergeStatus::Failed, half_applied)
            .await
            .unwrap());
        let before = fx.branch(a).await.timeline_id;
        for (i, choice) in ["reset", "delete"].into_iter().enumerate() {
            fx.owner(a, &format!("CREATE TABLE from_a{i} (id int);")).await;
            let m = fx
                .create_merge(
                    a,
                    fx.main,
                    MergeDirection::IntoParent,
                    Some(json!({ "lock_timeout_ms": 5000, "after": choice })),
                )
                .await;
            assert_planned(&m);
            let m = fx.rehearse(m.id).await;
            assert_rehearsed_clean(&m);
            let m = fx.apply(m.id).await;
            assert_done(&m);
            let after = m.result.as_ref().unwrap()["after"].clone();
            println!("{choice} while held: {after}");
            assert_eq!(after["choice"], choice);
            let refused = after["refused"].as_str().unwrap_or_default();
            assert!(refused.contains(&update.id.to_string()), "{after}");
            let a_now = fx.branch(a).await;
            assert_eq!((a_now.status, a_now.timeline_id.clone()), (BranchStatus::Ready, before.clone()));
        }
        assert!(fx
            .metadata
            .set_merge_status(update.id, &[MergeStatus::Failed], MergeStatus::Cancelled, MergeStatusPatch::default())
            .await
            .unwrap());
    })
    .await;
}

/// T14 F3 fix round (I2, E27): a merge whose phase 1 committed but was recorded failed in `tx`
/// (its answer and the marker read both lost), then recomputed, is never planned again: the
/// compute finds the marker and leaves it failed in `online` with its plan; a retry then runs
/// its online steps to the end.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn a_recompute_after_a_committed_phase1_stays_half_applied() {
    live(|fx| async move {
        let dev = base_and_dev(&fx).await;
        fx.owner(dev, "CREATE TABLE from_dev (id int); CREATE INDEX t_note_idx ON t (note);").await;
        let m = fx.merge_into_main(dev).await;
        assert_planned(&m);
        let m = fx.rehearse(m.id).await;
        assert_rehearsed_clean(&m);
        // Phase 1 commits; the worker goes away before the move on.
        fx.start_apply(m.id).await;
        let apply = tokio::spawn({
            let fx = fx.clone();
            async move {
                fx.worker
                    .handle_merge_job(MergeJob::Apply { merge_id: m.id }, Attempt(1))
                    .await
            }
        });
        let online = fx
            .wait_merge(m.id, Duration::from_secs(120), "online", |m| {
                m.status == MergeStatus::Online || !m.status.is_working()
            })
            .await;
        apply.abort();
        let _ = apply.await;
        assert_eq!(online.status, MergeStatus::Online, "{:?}", online.error);
        // As if phase 1's answer and the marker read were lost: failed in `tx`, then the user's
        // Recompute (`recomputeMerge` moves it to `computing`).
        let unsettled = MergeStatusPatch {
            failed_phase: Some(Some("tx".into())),
            error: Some(Some("step phase1: connection reset; Retry checks it first".into())),
            ..Default::default()
        };
        assert!(fx.metadata.set_merge_status(m.id, &[MergeStatus::Online], MergeStatus::Failed, unsettled).await.unwrap());
        let recompute = MergeStatusPatch {
            failed_phase: Some(None),
            error: Some(None),
            ..Default::default()
        };
        assert!(fx.metadata.set_merge_status(m.id, &[MergeStatus::Failed], MergeStatus::Computing, recompute).await.unwrap());
        fx.job(MergeJob::Compute { merge_id: m.id }).await;
        let after = fx.merge(m.id).await;
        println!("recomputed: {:?} {:?}", after.status, after.error);
        assert_eq!((after.status, after.failed_phase.as_deref()), (MergeStatus::Failed, Some("online")), "{:?}", after.error);
        assert_eq!(after.plan, online.plan, "not planned again");
        // Retry: the online steps run on top of the committed transaction.
        let retry = MergeStatusPatch {
            failed_phase: Some(None),
            error: Some(None),
            guard_failed_phase: Some(Some("online".into())),
            ..Default::default()
        };
        assert!(fx.metadata.set_merge_status(m.id, &[MergeStatus::Failed], MergeStatus::Online, retry).await.unwrap());
        fx.job(MergeJob::Resume { merge_id: m.id }).await;
        assert_done(&fx.merge(m.id).await);
    })
    .await;
}

/// After a merge with keep, the next merge carries only what changed since, and a column
/// renamed since is renamed on main (the identity map recorded with the keep base).
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn second_merge_carries_only_newer_changes() {
    live(|fx| async move {
        let dev = base_and_dev(&fx).await;
        fx.owner(
            dev,
            "CREATE TABLE first (id int PRIMARY KEY, old_name text);",
        )
        .await;
        let m = fx.merge_into_main(dev).await;
        assert_planned(&m);
        let m = fx.rehearse(m.id).await;
        assert_rehearsed_clean(&m);
        let m = fx.apply(m.id).await;
        assert_done(&m);

        fx.owner(
            dev,
            "ALTER TABLE first RENAME COLUMN old_name TO new_name; CREATE TABLE second (id int);",
        )
        .await;
        fx.owner(fx.main, "INSERT INTO first VALUES (1, 'kept');")
            .await;
        let m = fx.merge_into_main(dev).await;
        assert_planned(&m);
        let changed: Vec<String> = items(&m)
            .iter()
            .filter(|i| i["outcome"] == "apply")
            .map(|i| i["key"].to_string())
            .collect();
        println!("applied items: {changed:#?}");
        assert!(
            !changed
                .iter()
                .any(|k| k.contains("\"kind\":\"table\"") && k.contains("\"first\"")),
            "the first merge's table came back: {changed:?}"
        );
        let text = plan_text(&m);
        assert!(text.contains("RENAME COLUMN"), "{text}");
        assert!(!text.contains("DROP COLUMN"), "{text}");
        assert!(!text.contains("CREATE TABLE public.first"), "{text}");
        let m = fx.rehearse(m.id).await;
        assert_rehearsed_clean(&m);
        let m = fx.apply(m.id).await;
        assert_done(&m);
        assert_eq!(
            fx.value(fx.main, "SELECT new_name FROM first WHERE id = 1;")
                .await,
            "kept",
            "renamed in place, data kept"
        );
    })
    .await;
}

/// Review Focus 2: DDL on main after the compute makes the rehearsal stale; DDL between the
/// rehearsal and the apply makes the apply stale (the pre-check, before phase 0), and nothing
/// changes on main.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn concurrent_ddl_on_target_makes_the_merge_stale() {
    live(|fx| async move {
        let dev = base_and_dev(&fx).await;
        fx.owner(dev, "CREATE TABLE from_dev (id int);").await;

        // Between compute and rehearsal.
        let m = fx.merge_into_main(dev).await;
        assert_planned(&m);
        fx.owner(fx.main, "ALTER TABLE t ADD COLUMN moved1 int;")
            .await;
        let m = fx.rehearse(m.id).await;
        assert_eq!(m.status, MergeStatus::Stale, "{:?}", m.error);
        println!("stale rehearsal: {:?}", m.error);

        // Between rehearsal and apply.
        let m = fx.merge_into_main(dev).await;
        assert_planned(&m);
        let m = fx.rehearse(m.id).await;
        assert_rehearsed_clean(&m);
        fx.owner(fx.main, "ALTER TABLE t ADD COLUMN moved2 int;")
            .await;
        let m = fx.apply(m.id).await;
        assert_eq!(m.status, MergeStatus::Stale, "{:?}", m.error);
        let error = m.error.clone().unwrap_or_default();
        println!("stale apply: {error}");
        assert!(error.starts_with("merge_stale"), "{error}");
        assert!(error.contains("nothing was applied"), "{error}");
        assert_eq!(
            fx.value(fx.main, "SELECT to_regclass('public.from_dev') IS NULL;")
                .await,
            "t",
            "nothing changed on main"
        );
    })
    .await;
}

/// Review Focus 5: a new table in the schema the data API serves gets the default privileges'
/// grants on main, and nobody reports them: not the rehearsal, not the apply.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn new_table_in_served_schema_has_no_drift() {
    live(|fx| async move {
        let dev = base_and_dev(&fx).await;
        assert_eq!(
            fx.value(
                fx.main,
                "SELECT count(*) > 0 FROM pg_default_acl WHERE defaclrole = 'postgres'::regrole \
                 AND defaclnamespace = 'public'::regnamespace;"
            )
            .await,
            "t",
            "main serves public through the data API"
        );
        fx.owner(
            dev,
            "CREATE TABLE served (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text);
             CREATE SEQUENCE served_seq;",
        )
        .await;
        let m = fx.merge_into_main(dev).await;
        assert_planned(&m);
        let m = fx.rehearse(m.id).await;
        assert_rehearsed_clean(&m);
        let m = fx.apply(m.id).await;
        assert_done(&m);
        // The data API's default privileges: anon reads, authenticated reads and writes.
        for (role, privilege) in [
            ("anon", "SELECT"),
            ("authenticated", "SELECT"),
            ("authenticated", "INSERT"),
        ] {
            assert_eq!(
                fx.value(
                    fx.main,
                    &format!(
                        "SELECT has_table_privilege('{role}', 'public.served', '{privilege}');"
                    )
                )
                .await,
                "t",
                "{role} {privilege} on served"
            );
        }
    })
    .await;
}

/// P4/P14: while a rehearsal runs, its branch is `_rehearsal-<8 hex>`, its endpoint is
/// unroutable, so the proxy refuses a login the target takes, and (M11) the data API
/// gateway doesn't serve it.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn rehearsal_endpoint_is_not_routable() {
    live(|fx| async move {
        let dev = base_and_dev(&fx).await;
        fx.owner(dev, "CREATE TABLE from_dev (id int);").await;
        let m = fx.merge_into_main(dev).await;
        assert_planned(&m);
        // A slow step keeps the rehearsal up while it's probed.
        let m = fx
            .steps(
                m.id,
                json!([{ "id": "slow", "after": null, "sql": "SELECT pg_sleep(40)" }]),
            )
            .await;
        assert_planned(&m);
        let main_ep = fx.ep(fx.main).await;
        assert_eq!(
            proxy_login(&main_ep, &fx.database, "postgres", &fx.owner_password).await,
            Ok("postgres".to_string()),
            "the owner logs in to main through the proxy"
        );
        let main_key = public_key(&fx, fx.main).await.expect("main's public key");
        let mut status = 0;
        for _ in 0..30 {
            status = data_select_status(&main_key).await;
            if status == 200 {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        assert_eq!(status, 200, "main's public key reads t through the gateway");

        assert!(
            fx.metadata
                .set_merge_status(
                    m.id,
                    &[MergeStatus::Draft],
                    MergeStatus::Rehearsing,
                    MergeStatusPatch::default()
                )
                .await
                .unwrap()
        );
        let job = tokio::spawn({
            let fx = fx.clone();
            async move {
                fx.worker
                    .handle_merge_job(MergeJob::Rehearse { merge_id: m.id }, Attempt::LAST)
                    .await
            }
        });
        let name = format!("_rehearsal-{}", &m.id.simple().to_string()[..8]);
        let started = Instant::now();
        let rehearsal = loop {
            let found = fx
                .metadata
                .list_branches(fx.id)
                .await
                .unwrap()
                .into_iter()
                .find(|b| {
                    b.name == name && b.status == BranchStatus::Ready && b.endpoint_id.is_some()
                });
            if let Some(b) = found {
                break b;
            }
            assert!(
                started.elapsed() < Duration::from_secs(120),
                "no ready rehearsal {name}"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        };
        assert_eq!(rehearsal.kind, BranchKind::Rehearsal);
        assert_eq!(rehearsal.parent_branch_id, Some(fx.main));
        let ep = rehearsal.endpoint_id.clone().unwrap();
        let endpoint = fx
            .metadata
            .neon_endpoint(&ep)
            .await
            .unwrap()
            .expect("endpoint row");
        assert!(
            !endpoint.routable,
            "P4: the rehearsal's endpoint is unroutable"
        );

        // The proxy refuses the owner's login there (its verifier is the target's).
        let refused = proxy_login(&ep, &fx.database, "postgres", &fx.owner_password).await;
        println!("proxy on the rehearsal: {refused:?}");
        assert!(
            refused.is_err(),
            "the proxy routed to the rehearsal: {refused:?}"
        );

        // M11: the gateway doesn't serve it.
        match public_key(&fx, rehearsal.id).await {
            None => println!("the rehearsal has no public data key"),
            Some(key) => {
                let status = data_select_status(&key).await;
                println!("the rehearsal's public key on the gateway: {status}");
                assert_ne!(status, 200, "the gateway served the rehearsal");
            }
        }

        let outcome = job.await.unwrap();
        assert_eq!(outcome, Ok(()));
        let m = fx.merge(m.id).await;
        assert_rehearsed_clean(&m);
        assert_eq!(fx.branch(rehearsal.id).await.status, BranchStatus::Deleted);
        assert!(
            fx.metadata.neon_endpoint(&ep).await.unwrap().is_none(),
            "the endpoint row is gone"
        );
    })
    .await;
}

/// T4: a `chelabase_merge` schema the tenant made is never trusted: the marker setup refuses,
/// and the rehearsal fails with nothing applied.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn a_tenant_made_marker_schema_is_refused() {
    live(|fx| async move {
        let dev = base_and_dev(&fx).await;
        fx.owner(
            fx.main,
            "CREATE SCHEMA chelabase_merge;
             CREATE TABLE chelabase_merge.applied (merge_id uuid PRIMARY KEY, applied_at timestamptz NOT NULL DEFAULT now());",
        )
        .await;
        fx.owner(dev, "CREATE TABLE from_dev (id int);").await;
        let m = fx.merge_into_main(dev).await;
        assert_planned(&m);
        let m = fx.rehearse(m.id).await;
        assert_eq!(m.status, MergeStatus::Draft, "{:?}", m.rehearsal);
        let error = m.error.clone().unwrap_or_default();
        println!("marker setup: {error}");
        assert!(error.contains("chelabase_merge_marker_untrusted"), "{error}");
    })
    .await;
}

/// T7: a base whose stored model can't be used is read live through a static compute pinned at
/// the branch point: it boots from an endpoint row with no roles or databases, `cloud_admin`
/// reaches it, the catalog read (REPEATABLE READ READ ONLY, with its DO block) runs on the
/// hot standby, and the model equals the one recorded at branch create.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn a_base_read_live_equals_the_create_time_base() {
    live(|fx| async move {
        let dev = base_and_dev(&fx).await;
        let recorded = fx
            .metadata
            .schema_base(dev, &fx.database)
            .await
            .unwrap()
            .expect("the create-time base");
        assert_eq!(recorded.source, BaseSource::Create);
        let recorded_model: SchemaModel =
            serde_json::from_value(recorded.model.clone().expect("a model")).unwrap();
        // Main moves on, so a live read at main's head would differ.
        fx.owner(fx.main, "CREATE TABLE later_on_main (id int);").await;
        fx.owner(dev, "CREATE TABLE from_dev (id int);").await;
        // Written by another model version: untrusted.
        sqlx::query("UPDATE branch_schema_bases SET model_version = 0 WHERE id = $1")
            .bind(recorded.id)
            .execute(fx.metadata.pool())
            .await
            .unwrap();

        // Watch for the static compute while the compute job runs.
        let job = tokio::spawn({
            let fx = fx.clone();
            async move {
                let started = Instant::now();
                let m = fx.merge_into_main(dev).await;
                println!("compute with a live base read took {:?}", started.elapsed());
                m
            }
        });
        let tenant = fx
            .metadata
            .get_database(fx.id)
            .await
            .unwrap()
            .unwrap()
            .neon_tenant_id
            .unwrap();
        let mut standby = None;
        while !job.is_finished() {
            let rows: Vec<(String, Option<String>)> = sqlx::query_as(
                "SELECT id, static_lsn FROM neon_endpoints WHERE tenant_id = $1 AND static_lsn IS NOT NULL",
            )
            .bind(&tenant)
            .fetch_all(fx.metadata.pool())
            .await
            .unwrap();
            for (ep, lsn) in rows {
                let opts = SqlOpts {
                    timeout: Duration::from_secs(5),
                    ..SqlOpts::admin_default()
                };
                if let Ok(out) = fx
                    .worker
                    .computes
                    .admin_sql_with(&ep, &fx.database, "SELECT pg_is_in_recovery() || '|' || current_user;", &opts)
                    .await
                {
                    standby = Some((ep, lsn, last_line(&out)));
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let m = job.await.unwrap();
        assert_planned(&m);
        println!("the static compute seen: {standby:?}");
        if let Some((_, lsn, answer)) = &standby {
            assert_eq!(answer, "true|cloud_admin", "a hot standby that cloud_admin reaches");
            assert_eq!(lsn.as_deref(), recorded.lsn.as_deref(), "pinned at the base's LSN");
        }
        let filled = fx
            .metadata
            .schema_base(dev, &fx.database)
            .await
            .unwrap()
            .expect("the base");
        assert_eq!(filled.id, recorded.id, "filled in place");
        assert_eq!(filled.model_version, recorded.model_version.map(|_| 1));
        let live_model: SchemaModel =
            serde_json::from_value(filled.model.clone().expect("a model")).unwrap();
        assert_eq!(live_model, recorded_model, "the live read equals the create-time base");
        // Nothing of the static compute is left.
        let left: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM neon_endpoints WHERE tenant_id = $1 AND static_lsn IS NOT NULL",
        )
        .bind(&tenant)
        .fetch_one(fx.metadata.pool())
        .await
        .unwrap();
        assert_eq!(left, 0, "the static compute's endpoint row is gone");
        // The analysis is against the branch point: main's later table is the target's own.
        let later: Vec<Value> = items(&m)
            .into_iter()
            .filter(|i| i["key"].to_string().contains("later_on_main"))
            .collect();
        assert!(
            later.iter().all(|i| i["outcome"] == "target_only"),
            "{later:?}"
        );
    })
    .await;
}

/// T7b: restoring the branch to before a keep-merge restores the older base, so the next merge
/// doesn't read the merged table (now on main only) as the branch dropping it.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn restore_before_a_keep_merge_uses_the_older_base() {
    live(|fx| async move {
        let dev = base_and_dev(&fx).await;
        let noted = fx.value(dev, "SELECT pg_current_wal_insert_lsn();").await;
        println!("restore point: {noted}");
        fx.owner(dev, "CREATE TABLE merged_once (id int);").await;
        let m = fx.merge_into_main(dev).await;
        assert_planned(&m);
        let m = fx.rehearse(m.id).await;
        assert_rehearsed_clean(&m);
        let m = fx.apply(m.id).await;
        assert_done(&m);
        assert_eq!(
            fx.metadata
                .schema_base(dev, &fx.database)
                .await
                .unwrap()
                .unwrap()
                .source,
            BaseSource::Merge
        );

        // Restore dev to before its change (and the merge).
        assert!(
            fx.metadata
                .set_branch_status(dev, &[BranchStatus::Ready], BranchStatus::Resetting)
                .await
                .unwrap()
        );
        fx.worker
            .handle_branch_job(BranchJob::Restore {
                branch_id: dev,
                backup_id: BranchId::new(),
                backup_name: "dev-before-restore-20261006T120000Z".into(),
                at: PointInTime::Lsn(noted.clone()),
            })
            .await
            .expect("restore job");
        let restored = fx.branch(dev).await;
        assert_eq!(restored.status, BranchStatus::Ready, "{:?}", restored.error);
        // A child branch's restore: one detach copies main's level up to dev's branch point
        // and dev's up to the restore point (Neon fork round 2).
        assert_eq!(restored.error, None, "the restore of dev failed");
        println!(
            "restored: timeline {:?} branched at {:?} (parent {:?}); dev's LSN now {}",
            restored.timeline_id,
            restored.branched_at_lsn,
            restored.parent_branch_id,
            fx.value(dev, "SELECT pg_current_wal_insert_lsn();").await
        );
        assert_eq!(
            fx.value(dev, "SELECT to_regclass('public.merged_once') IS NULL;")
                .await,
            "t",
            "dev is back before its change"
        );
        let base = fx
            .metadata
            .schema_base(dev, &fx.database)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(base.source, BaseSource::Create, "the older base, copied");

        fx.owner(dev, "CREATE TABLE after_restore (id int);").await;
        let m = fx.merge_into_main(dev).await;
        assert_planned(&m);
        let text = plan_text(&m);
        println!("plan after the restore:\n{text}");
        assert!(
            !text.contains("DROP TABLE"),
            "main's merged table is undone:\n{text}"
        );
        let merged: Vec<Value> = items(&m)
            .into_iter()
            .filter(|i| i["key"].to_string().contains("merged_once"))
            .collect();
        assert!(
            merged.iter().all(|i| i["outcome"] == "target_only"),
            "{merged:?}"
        );
    })
    .await;
}

/// E19: a phase 2 `SET NOT NULL` behind a long transaction on main waits at most the merge's
/// `lock_timeout`, the merge fails in `online`, and Retry finishes it once the transaction ends.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn phase2_lock_timeout_fails_online_then_retry_finishes() {
    live(|fx| async move {
        fx.owner(
            fx.main,
            "CREATE TABLE n (id int PRIMARY KEY, v int); INSERT INTO n VALUES (1, 1), (2, 2);",
        )
        .await;
        let dev = fx.child("dev", fx.main).await;
        fx.owner(dev, "ALTER TABLE n ALTER COLUMN v SET NOT NULL;").await;
        let m = fx
            .create_merge(
                dev,
                fx.main,
                MergeDirection::IntoParent,
                Some(json!({ "lock_timeout_ms": 1500, "after": "keep" })),
            )
            .await;
        assert_planned(&m);
        // Phase 1 holds its lock on n a few seconds, so the long transaction can queue behind it
        // and start right after phase 1 commits, before phase 2.
        let check = plan_steps(&m)
            .into_iter()
            .find(|s| s["phase"] == "tx" && s["sql"].as_str().unwrap_or("").contains("NOT VALID"))
            .unwrap_or_else(|| panic!("no phase 1 check:\n{}", plan_text(&m)));
        let m = fx
            .steps(
                m.id,
                json!([{ "id": "hold", "after": check["id"], "sql": "SELECT pg_sleep(4)" }]),
            )
            .await;
        assert_planned(&m);
        let m = fx.rehearse(m.id).await;
        assert_rehearsed_clean(&m);

        fx.start_apply(m.id).await;
        let apply = tokio::spawn({
            let fx = fx.clone();
            async move {
                fx.worker
                    .handle_merge_job(MergeJob::Apply { merge_id: m.id }, Attempt(1))
                    .await
            }
        });
        // Phase 1 has its ACCESS EXCLUSIVE lock on n.
        let app = format!("chelabase-merge-{}", m.id);
        let started = Instant::now();
        loop {
            let held = fx
                .value(
                    fx.main,
                    &format!(
                        "SELECT count(*) FROM pg_locks l JOIN pg_stat_activity a USING (pid) \
                         WHERE a.application_name = '{app}' AND l.mode = 'AccessExclusiveLock' \
                         AND l.granted AND l.relation = 'public.n'::regclass;"
                    ),
                )
                .await;
            if held == "1" {
                break;
            }
            assert!(started.elapsed() < Duration::from_secs(60), "phase 1 never locked n");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let holder = tokio::spawn({
            let fx = fx.clone();
            async move {
                let ep = fx.ep(fx.main).await;
                let opts = SqlOpts {
                    timeout: Duration::from_secs(90),
                    app_name: Some("t13-holder".into()),
                    ..SqlOpts::admin_default()
                };
                fx.worker
                    .computes
                    .admin_sql_with(
                        &ep,
                        &fx.database,
                        "BEGIN; LOCK TABLE n IN ACCESS SHARE MODE; SELECT pg_sleep(20); COMMIT;",
                        &opts,
                    )
                    .await
                    .map_err(|e| format!("{e:#}"))
            }
        });
        let outcome = apply.await.unwrap();
        assert_eq!(outcome, Ok(()));
        let m = fx.merge(m.id).await;
        println!("after the lock timeout: {:?} {:?} {:?}", m.status, m.failed_phase, m.error);
        assert_eq!(m.status, MergeStatus::Failed, "{:?}", m.error);
        assert_eq!(m.failed_phase.as_deref(), Some("online"));
        let error = m.error.clone().unwrap_or_default();
        assert!(error.contains("lock timeout"), "{error}");
        assert!(!holder.is_finished(), "the long transaction still runs");
        holder.await.unwrap().expect("the long transaction");

        let m = fx.retry(m.id).await;
        assert_done(&m);
        assert_eq!(
            fx.value(
                fx.main,
                "SELECT attnotnull FROM pg_attribute WHERE attrelid = 'public.n'::regclass AND attname = 'v';"
            )
            .await,
            "t"
        );
        let log = log_lines(&fx, m.id).await;
        println!("log: {log:#?}");
        assert!(log.iter().any(|l| l.starts_with("step online") && l.ends_with("failed")), "{log:?}");
    })
    .await;
}

/// E20: a phase 1a lock timeout fails the merge in `enum_add`; Retry applies from the start, and
/// the enum value the first attempt made doesn't make it stale (`precheck_fingerprint`).
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn retry_after_an_enum_add_lock_timeout_runs_from_the_start() {
    live(|fx| async move {
        fx.owner(
            fx.main,
            "CREATE TYPE e1 AS ENUM ('a'); CREATE TYPE e2 AS ENUM ('a');
             CREATE TABLE u (id int PRIMARY KEY, x e1, y e2);",
        )
        .await;
        let dev = fx.child("dev", fx.main).await;
        // A role too (E20's roles half, after E22): the first attempt's phase 0 creates it, which
        // also gives the owner its automatic memberships; neither makes the retry stale.
        fx.owner(
            dev,
            "ALTER TYPE e1 ADD VALUE 'b'; ALTER TYPE e2 ADD VALUE 'b'; CREATE ROLE app_retry NOLOGIN;",
        )
        .await;
        fx.owner(
            dev,
            "ALTER TABLE u ALTER COLUMN x SET DEFAULT 'b', ALTER COLUMN y SET DEFAULT 'b';",
        )
        .await;
        let m = fx
            .create_merge(
                dev,
                fx.main,
                MergeDirection::IntoParent,
                Some(json!({ "lock_timeout_ms": 1500, "after": "keep" })),
            )
            .await;
        assert_planned(&m);
        println!("plan:\n{}", plan_text(&m));
        let adds: Vec<Value> = plan_steps(&m)
            .into_iter()
            .filter(|s| s["phase"] == "enum_add")
            .collect();
        assert_eq!(adds.len(), 2, "{}", plan_text(&m));
        let last = adds[1]["sql"].as_str().unwrap().to_string();
        let (locked, free) = if last.contains("e2") { ("e2", "e1") } else { ("e1", "e2") };
        let m = fx.rehearse(m.id).await;
        assert_rehearsed_clean(&m);

        // A transaction holds the second enum's lock.
        let holder = tokio::spawn({
            let fx = fx.clone();
            async move {
                let ep = fx.ep(fx.main).await;
                let opts = SqlOpts {
                    timeout: Duration::from_secs(90),
                    app_name: Some("t13-holder".into()),
                    ..SqlOpts::admin_default()
                };
                fx.worker
                    .computes
                    .admin_sql_with(
                        &ep,
                        &fx.database,
                        // `replica` keeps the data API's event trigger quiet: its schema_version row
                // update would otherwise hold back every DDL of the merge, not only this enum's.
                &format!(
                    "BEGIN; SET LOCAL session_replication_role = replica; \
                     ALTER TYPE {locked} ADD VALUE 'held'; SELECT pg_sleep(15); ROLLBACK;"
                ),
                        &opts,
                    )
                    .await
                    .map_err(|e| format!("{e:#}"))
            }
        });
        let started = Instant::now();
        while fx
            .value(
                fx.main,
                "SELECT count(*) FROM pg_stat_activity WHERE application_name = 't13-holder' AND query LIKE '%pg_sleep%';",
            )
            .await
            != "1"
        {
            assert!(started.elapsed() < Duration::from_secs(30), "the holder never started");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let m = fx.apply(m.id).await;
        println!("after the 1a lock timeout: {:?} {:?} {:?}", m.status, m.failed_phase, m.error);
        assert_eq!(m.status, MergeStatus::Failed, "{:?}", m.error);
        assert_eq!(m.failed_phase.as_deref(), Some("enum_add"));
        assert!(m.error.clone().unwrap_or_default().contains("lock timeout"), "{:?}", m.error);
        // The first attempt made the role and the first value.
        assert_eq!(
            fx.value(fx.main, "SELECT count(*) FROM pg_roles WHERE rolname = 'app_retry';").await,
            "1"
        );
        assert_eq!(
            fx.value(fx.main, &format!("SELECT enum_range(NULL::{free})::text;")).await,
            "{a,b}"
        );
        holder.await.unwrap().expect("the holder");

        let m = fx.retry(m.id).await;
        assert_done(&m);
        assert_eq!(
            fx.value(fx.main, "SELECT pg_has_role('postgres', 'app_retry', 'MEMBER');").await,
            "t"
        );
        for e in ["e1", "e2"] {
            assert_eq!(
                fx.value(fx.main, &format!("SELECT enum_range(NULL::{e})::text;")).await,
                "{a,b}"
            );
        }
        assert_eq!(
            fx.value(
                fx.main,
                "SELECT string_agg(column_name || '=' || column_default, ',' ORDER BY column_name) \
                 FROM information_schema.columns WHERE table_name = 'u' AND column_default IS NOT NULL;"
            )
            .await,
            "x='b'::e1,y='b'::e2"
        );
    })
    .await;
}

/// E27 (T14 F3): an online step that fails on the target's data (a NULL a row got after the
/// rehearsal) leaves the merge `failed` in `online`, half applied: it holds the target (the
/// claim of a reset is refused), and abandoning it lists the temporary NOT NULL check phase 1
/// left, which is on the target.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn a_failed_online_merge_holds_its_target_and_abandon_lists_leftovers() {
    use chelabase_metadata::{BranchBusy, ClaimOutcome, MergeEvent};
    live(|fx| async move {
        fx.owner(fx.main, "CREATE TABLE n (id int PRIMARY KEY, v int); INSERT INTO n VALUES (1, 1);")
            .await;
        let dev = fx.child("dev", fx.main).await;
        fx.owner(dev, "ALTER TABLE n ALTER COLUMN v SET NOT NULL;").await;
        let m = fx.merge_into_main(dev).await;
        assert_planned(&m);
        let m = fx.rehearse(m.id).await;
        assert_rehearsed_clean(&m);
        // Data, not schema: the pre-check passes, the validation fails.
        fx.owner(fx.main, "INSERT INTO n VALUES (2, NULL);").await;
        let m = fx.apply(m.id).await;
        assert_eq!((m.status, m.failed_phase.as_deref()), (MergeStatus::Failed, Some("online")), "{:?}", m.error);

        let held = fx.metadata.claim_branch_for_switch(fx.main, true).await.unwrap();
        assert_eq!(held, ClaimOutcome::MergeBusy(BranchBusy::Merge(m.id)));
        assert_eq!(fx.branch(fx.main).await.status, BranchStatus::Ready);

        // Abandon, as `abandonMerge` does.
        let plan: chelabase_schema::plan::Plan =
            serde_json::from_value(json!({ "steps": m.plan.as_ref().unwrap()["steps"] })).unwrap();
        let events: Vec<MergeEvent> = fx.metadata.merge_events(m.id).await.unwrap();
        let done: std::collections::HashSet<String> = events
            .iter()
            .filter(|e| e.kind == "step" && e.phase.as_deref() == Some("online") && e.outcome.as_deref() == Some("ok"))
            .filter_map(|e| e.step_id.clone())
            .collect();
        let left = chelabase_schema::plan::leftovers(&plan, &done);
        println!("leftovers: {left:?}");
        assert_eq!(left.len(), 1, "{left:?}");
        let chelabase_schema::plan::Leftover::NotNullCheck { name, .. } = &left[0] else {
            panic!("{left:?}")
        };
        assert_eq!(
            fx.value(fx.main, &format!("SELECT count(*) FROM pg_constraint WHERE conname = '{name}' AND NOT convalidated;"))
                .await,
            "1",
            "the listed check is on the target, not validated"
        );
        let abandon = MergeStatusPatch {
            result: Some(json!({ "leftovers": left })),
            guard_failed_phase: Some(Some("online".into())),
            log_status: true,
            ..Default::default()
        };
        assert!(fx.metadata.set_merge_status(m.id, &[MergeStatus::Failed], MergeStatus::Cancelled, abandon).await.unwrap());
        let free = fx.metadata.claim_branch_for_switch(fx.main, true).await.unwrap();
        assert_eq!(free, ClaimOutcome::Claimed);
        fx.metadata
            .set_branch_status(fx.main, &[BranchStatus::Resetting], BranchStatus::Ready)
            .await
            .unwrap();
    })
    .await;
}

/// E21: a merge that reads back with drift ends `applied_with_drift` and skips the after-merge
/// choice; `finishMerge {after: keep}` then runs it.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn applied_with_drift_skips_the_choice_then_finish_keeps() {
    live(|fx| async move {
        let dev = base_and_dev(&fx).await;
        fx.owner(dev, "CREATE TABLE from_dev (id int);").await;
        let m = fx.merge_into_main(dev).await;
        assert_planned(&m);
        // A step that makes something the merged model doesn't have.
        let m = fx
            .steps(
                m.id,
                json!([{ "id": "extra", "after": null, "sql": "CREATE TABLE made_by_a_step (id int)" }]),
            )
            .await;
        assert_planned(&m);
        let m = fx.rehearse(m.id).await;
        assert_eq!(m.status, MergeStatus::Rehearsed, "{:?}", m.error);
        assert!(!rehearsal_drift(&m).is_empty(), "the rehearsal shows the drift");
        let m = fx.apply(m.id).await;
        assert_eq!(m.status, MergeStatus::AppliedWithDrift, "{:?}", m.error);
        let result = m.result.clone().unwrap();
        println!("result with drift: {result}");
        assert!(!result_drift(&m).is_empty());
        assert!(result["after"]["skipped"].is_string(), "{result}");
        let before = fx.metadata.schema_base(dev, &fx.database).await.unwrap().unwrap();
        assert_eq!(before.source, BaseSource::Create);

        let m = fx.finish(m.id, MergeAfter::Keep).await;
        assert_eq!(m.status, MergeStatus::AppliedWithDrift);
        let after = m.result.as_ref().unwrap()["after"].clone();
        println!("finish keep: {after}");
        assert_eq!(after, json!({"choice": "keep", "base": "recorded"}));
        let base = fx.metadata.schema_base(dev, &fx.database).await.unwrap().unwrap();
        assert_eq!(base.source, BaseSource::Merge);
    })
    .await;
}

/// Recovery from the marker (T8b), with the worker "crashing" (its job dropped):
/// - before phase 1 commits (a slow phase 1 step): the running statement is cancelled, there's
///   no marker and a step was logged, so the merge goes back to `rehearsed`, "interrupted,
///   nothing applied", with nothing of phase 1 on main;
/// - after phase 1 commits but before the status moved on: the marker is there, so the resume
///   carries on with phase 2 and finishes.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn recovery_reads_the_marker() {
    live(|fx| async move {
        let dev = base_and_dev(&fx).await;
        fx.owner(
            dev,
            "ALTER TYPE mood ADD VALUE 'meh';
             CREATE TABLE from_dev (id int, feel mood DEFAULT 'meh');
             CREATE INDEX t_note_idx ON t (note);",
        )
        .await;
        let m = fx.merge_into_main(dev).await;
        assert_planned(&m);
        let m = fx
            .steps(
                m.id,
                json!([{ "id": "slow", "after": null, "sql": "SELECT pg_sleep(8)" }]),
            )
            .await;
        assert_planned(&m);
        let m = fx.rehearse(m.id).await;
        assert_rehearsed_clean(&m);
        let app = format!("chelabase-merge-{}", m.id);

        // 1. Crash inside phase 1.
        fx.start_apply(m.id).await;
        let apply = tokio::spawn({
            let fx = fx.clone();
            async move {
                fx.worker
                    .handle_merge_job(MergeJob::Apply { merge_id: m.id }, Attempt(1))
                    .await
            }
        });
        let started = Instant::now();
        while fx
            .value(
                fx.main,
                &format!("SELECT count(*) FROM pg_stat_activity WHERE application_name = '{app}' AND query LIKE '%pg_sleep%';"),
            )
            .await
            != "1"
        {
            assert!(started.elapsed() < Duration::from_secs(60), "phase 1 never started");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        apply.abort();
        let _ = apply.await;
        assert_eq!(fx.merge(m.id).await.status, MergeStatus::Applying);
        fx.job(MergeJob::Resume { merge_id: m.id }).await;
        let m = fx.merge(m.id).await;
        println!("after a crash in phase 1: {:?} {:?}", m.status, m.error);
        assert_eq!(m.status, MergeStatus::Rehearsed, "{:?}", m.error);
        assert!(
            m.error.as_deref().unwrap_or_default().starts_with("interrupted before the plan's transaction committed"),
            "{:?}",
            m.error
        );
        assert_eq!(
            fx.value(fx.main, "SELECT to_regclass('public.from_dev') IS NULL;").await,
            "t",
            "phase 1 rolled back"
        );
        assert_eq!(
            fx.value(
                fx.main,
                &format!("SELECT count(*) FROM pg_stat_activity WHERE application_name = '{app}';")
            )
            .await,
            "0",
            "the crashed run's statement was cancelled"
        );

        // 2. Crash after phase 1 committed, before the status moved on.
        // A transaction with an xid, open on main: `CREATE INDEX CONCURRENTLY` (the online
        // step) waits for it, so the merge sits in `online` until it ends and the poll below
        // can't miss that status.
        let holder = tokio::spawn({
            let fx = fx.clone();
            async move {
                let ep = fx.ep(fx.main).await;
                let opts = SqlOpts {
                    timeout: Duration::from_secs(90),
                    app_name: Some("t15-holder".into()),
                    ..SqlOpts::admin_default()
                };
                fx.worker
                    .computes
                    .admin_sql_with(
                        &ep,
                        &fx.database,
                        "BEGIN; SELECT txid_current(); SELECT pg_sleep(25); COMMIT;",
                        &opts,
                    )
                    .await
                    .map_err(|e| format!("{e:#}"))
            }
        });
        let started = Instant::now();
        while fx
            .value(
                fx.main,
                "SELECT count(*) FROM pg_stat_activity WHERE application_name = 't15-holder' AND query LIKE '%pg_sleep%';",
            )
            .await
            != "1"
        {
            assert!(started.elapsed() < Duration::from_secs(60), "the holder never started");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        fx.start_apply(m.id).await;
        let apply = tokio::spawn({
            let fx = fx.clone();
            async move {
                fx.worker
                    .handle_merge_job(MergeJob::Apply { merge_id: m.id }, Attempt(1))
                    .await
            }
        });
        let m2 = fx
            .wait_merge(m.id, Duration::from_secs(120), "online", |m| {
                m.status == MergeStatus::Online || !m.status.is_working()
            })
            .await;
        assert!(!holder.is_finished(), "the holder ended before the merge reached online");
        apply.abort();
        let _ = apply.await;
        holder.await.unwrap().expect("the holder");
        assert_eq!(m2.status, MergeStatus::Online, "{:?}", m2.error);
        // As if the worker died between phase 1's commit and the move to online.
        assert!(
            fx.metadata
                .set_merge_status(m.id, &[MergeStatus::Online], MergeStatus::Applying, MergeStatusPatch::default())
                .await
                .unwrap()
        );
        fx.job(MergeJob::Resume { merge_id: m.id }).await;
        let m = fx.merge(m.id).await;
        assert_done(&m);
        assert_eq!(
            fx.value(
                fx.main,
                "SELECT i.indisvalid FROM pg_index i WHERE i.indexrelid = 'public.t_note_idx'::regclass;"
            )
            .await,
            "t"
        );
        assert_eq!(
            fx.value(fx.main, "SELECT count(*) FROM from_dev;").await,
            "0"
        );
    })
    .await;
}

/// E22: the memberships Postgres gives the owner in a role made in the owner's session, the
/// three ways a role is made: in the owner's own session (a user's SQL), as `cloud_admin` after
/// `SET ROLE postgres` (a rehearsal's phase 0), and through the control plane's `create_role`
/// (an apply's phase 0). `chelabase_schema::owner_grants` encodes them.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn owner_memberships_of_created_roles() {
    live(|fx| async move {
        fx.owner(fx.main, "CREATE ROLE p_sql NOLOGIN;").await;
        fx.admin(fx.main, "SET ROLE postgres; CREATE ROLE p_set_role NOLOGIN; RESET ROLE;")
            .await;
        let spec = RoleSpec {
            login: false,
            ..RoleSpec::login("p_api", false)
        };
        assert_eq!(fx.worker.create_role(fx.main, &spec).await, RoleOutcome::Ok);
        let rows = fx
            .value(
                fx.main,
                "SELECT string_agg(r.rolname || ' grantor=' || g.rolname || ' admin=' || m.admin_option \
                 || ' inherit=' || m.inherit_option || ' set=' || m.set_option, '; ' \
                 ORDER BY r.rolname, g.rolname) \
                 FROM pg_auth_members m JOIN pg_roles r ON r.oid = m.roleid \
                 JOIN pg_roles g ON g.oid = m.grantor \
                 WHERE m.member = 'postgres'::regrole AND r.rolname LIKE 'p\\_%';",
            )
            .await;
        println!("owner memberships: {rows}");
        let owner = chelabase_schema::NamedRole {
            oid: chelabase_schema::Oid(0),
            name: "postgres".into(),
        };
        let want: Vec<String> = ["p_api", "p_set_role", "p_sql"]
            .iter()
            .flat_map(|name| {
                let role = chelabase_schema::NamedRole {
                    oid: chelabase_schema::Oid(0),
                    name: name.to_string(),
                };
                chelabase_schema::owner_grants(&role, &owner)
            })
            .map(|m| {
                format!(
                    "{} grantor={} admin={} inherit={} set={}",
                    m.role.name, m.grantor.name, m.admin, m.inherit, m.set
                )
            })
            .collect();
        assert_eq!(rows, want.join("; "), "chelabase_schema::owner_grants");
        assert_eq!(
            fx.value(fx.main, "SELECT rolname FROM pg_roles WHERE oid = 10;").await,
            chelabase_schema::BOOTSTRAP_SUPERUSER.1,
            "the bootstrap superuser"
        );
    })
    .await;
}

// =============================================================================================
// E17 after Neon fork round 2's patch 4: live base reads behind the GC cutoff
// =============================================================================================

/// How long GC may take to pass a point once [`Fx::fast_gc`] ran.
const GC_WAIT: Duration = Duration::from_secs(300);

/// The WAL each round of [`Fx::wait_gc_past`] writes on each branch (`a` has one int column).
const CHURN: &str = "INSERT INTO a SELECT generate_series(1, 20000);";

impl Fx {
    async fn tenant(&self) -> NeonId {
        self.metadata
            .get_database(self.id)
            .await
            .unwrap()
            .unwrap()
            .neon_tenant_id
            .expect("the database's tenant")
            .parse()
            .unwrap()
    }

    /// SQL as `cloud_admin`, retried for up to 60 s (a compute reconnecting after a
    /// pageserver restart).
    async fn admin_retrying(&self, b: BranchId, sql: &str) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match self.try_admin(b, sql).await {
                Ok(_) => return,
                Err(e) if Instant::now() < deadline => {
                    println!("retrying: {e}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Err(e) => panic!("as cloud_admin: {sql}: {e}"),
            }
        }
    }

    /// Lets GC pass fresh points within seconds: a 1 s history window (the worker sets the
    /// tenant's `pitr_interval` from it, and reconcile keeps it), `gc_horizon` 1 MiB,
    /// `gc_period` 1 s, `lsn_lease_length` 10 s and `checkpoint_timeout` 1 s (GC can't pass what
    /// isn't flushed to disk yet). Then the pageserver restarts: GC waits one
    /// lease length after a tenant attaches, fixed at attach time (10 minutes by default).
    async fn fast_gc(&self) {
        assert!(
            self.metadata
                .update_database_settings(
                    self.id,
                    &SettingsPatch {
                        history_window: Some(Duration::from_secs(1)),
                        ..Default::default()
                    },
                )
                .await
                .unwrap()
        );
        self.worker
            .handle_database_settings(self.id)
            .await
            .expect("settings job");
        let tenant = self.tenant().await;
        let resp = reqwest::Client::new()
            .patch(format!(
                "{}/v1/tenant/config",
                self.storcon_url.trim_end_matches('/')
            ))
            .bearer_auth(&self.storcon_token)
            .json(&json!({
                "tenant_id": tenant.as_str(),
                "gc_horizon": 1_048_576,
                "gc_period": "1s",
                "lsn_lease_length": "10s",
                // GC never passes a timeline's `disk_consistent_lsn`, and an idle timeline's
                // in-memory layer isn't flushed for `checkpoint_timeout` (10 min by default).
                "checkpoint_timeout": "1s",
            }))
            .send()
            .await
            .expect("patch the tenant config");
        let status = resp.status();
        assert!(
            status.is_success(),
            "patch the tenant config: {status}: {}",
            resp.text().await.unwrap_or_default()
        );

        // The stack's pageserver: `<CHELABASE_COMPOSE_PROJECT>-pageserver-1`.
        let restarted = tokio::process::Command::new("docker")
            .args(["restart", &pageserver_container()])
            .stdout(Stdio::null())
            .status()
            .await
            .expect("run docker");
        assert!(restarted.success(), "docker restart {}", pageserver_container());
        let main: NeonId = self
            .branch(self.main)
            .await
            .timeline_id
            .expect("main's timeline")
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(120);
        while let Err(e) = self
            .worker
            .storage
            .storcon
            .get_timeline(&tenant, &main)
            .await
        {
            assert!(
                Instant::now() < deadline,
                "the pageserver didn't come back: {e:?}"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Writes WAL on `writers` until GC has passed `point` on every one of `timelines` (the
    /// scenario's precondition), or panics after [`GC_WAIT`].
    async fn wait_gc_past(&self, point: Lsn, timelines: &[&str], writers: &[BranchId]) {
        let tenant = self.tenant().await;
        let started = Instant::now();
        loop {
            for b in writers {
                self.admin_retrying(*b, CHURN).await;
            }
            let mut behind = Vec::new();
            for t in timelines {
                let id: NeonId = t.parse().unwrap();
                let cutoff = match self.worker.storage.storcon.get_timeline(&tenant, &id).await {
                    Ok(info) => info.applied_gc_cutoff_lsn,
                    Err(e) => {
                        println!("reading timeline {t}: {e:?}");
                        None
                    }
                };
                if !cutoff.is_some_and(|c| c > point) {
                    behind.push((t.to_string(), cutoff));
                }
            }
            if behind.is_empty() {
                println!("GC passed {point} after {:?}", started.elapsed());
                return;
            }
            assert!(
                started.elapsed() < GC_WAIT,
                "GC didn't pass {point} within {GC_WAIT:?}: {behind:?}"
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// Marks the branch's current base as another model version's, so the next compute
    /// reads it live.
    async fn age_base(&self, b: BranchId) {
        let base = self
            .metadata
            .schema_base(b, &self.database)
            .await
            .unwrap()
            .expect("a base");
        sqlx::query("UPDATE branch_schema_bases SET model_version = 0 WHERE id = $1")
            .bind(base.id)
            .execute(self.metadata.pool())
            .await
            .unwrap();
    }

    /// The public tables of the branch's current base, as read (not the aged one).
    async fn base_tables(&self, b: BranchId) -> Vec<String> {
        let base = self
            .metadata
            .schema_base(b, &self.database)
            .await
            .unwrap()
            .expect("a base");
        assert_ne!(base.model_version, Some(0), "the base was read again");
        let model: SchemaModel =
            serde_json::from_value(base.model.clone().expect("a model")).unwrap();
        let mut tables: Vec<String> = model
            .tables
            .iter()
            .filter(|t| t.schema == "public")
            .map(|t| t.name.clone())
            .collect();
        tables.sort();
        tables
    }

    /// Waits for `job` (a merge compute) while recording the timelines of the static computes
    /// it starts; then checks that none of their endpoint rows is left.
    async fn static_timelines_while(
        &self,
        job: tokio::task::JoinHandle<MergeRecord>,
    ) -> (MergeRecord, Vec<String>) {
        let tenant = self.tenant().await.as_str().to_string();
        let query = "SELECT timeline_id FROM neon_endpoints \
                     WHERE tenant_id = $1 AND static_lsn IS NOT NULL";
        let mut seen: Vec<String> = Vec::new();
        while !job.is_finished() {
            let rows: Vec<String> = sqlx::query_scalar(query)
                .bind(&tenant)
                .fetch_all(self.metadata.pool())
                .await
                .unwrap();
            for t in rows {
                if !seen.contains(&t) {
                    seen.push(t);
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let m = job.await.unwrap();
        let left: Vec<String> = sqlx::query_scalar(query)
            .bind(&tenant)
            .fetch_all(self.metadata.pool())
            .await
            .unwrap();
        assert!(
            left.is_empty(),
            "static compute endpoint rows left: {left:?}"
        );
        (m, seen)
    }

    /// The merge of `source` into its parent `target`, computed while watching static
    /// computes.
    async fn compute_watching(
        self: &Arc<Self>,
        source: BranchId,
        target: BranchId,
    ) -> (MergeRecord, Vec<String>) {
        let job = tokio::spawn({
            let fx = self.clone();
            async move {
                fx.create_merge(source, target, MergeDirection::IntoParent, None)
                    .await
            }
        });
        self.static_timelines_while(job).await
    }

    /// The base's (timeline, LSN) location.
    async fn base_location(&self, b: BranchId) -> (BaseSource, String, Lsn) {
        let base = self
            .metadata
            .schema_base(b, &self.database)
            .await
            .unwrap()
            .expect("a base");
        (
            base.source,
            base.timeline_id.expect("the base's timeline"),
            base.lsn.expect("the base's LSN").parse().unwrap(),
        )
    }
}

/// A draft left without analysis because its base is gone.
fn assert_base_unavailable(m: &MergeRecord) {
    assert_eq!(m.status, MergeStatus::Draft, "{:?}", m.error);
    let error = m.error.clone().unwrap_or_default();
    assert!(error.starts_with("merge_base_unavailable"), "{error}");
    assert!(m.analysis.is_none(), "{:?}", m.analysis);
}

/// main: `a`; `dev` from main; `m` on main and `d` on dev after dev's branch point.
async fn a_dev_m_d(fx: &Fx) -> BranchId {
    fx.owner(fx.main, "CREATE TABLE a (v int);").await;
    let dev = fx.child("dev", fx.main).await;
    fx.owner(fx.main, "CREATE TABLE m (v int);").await;
    fx.owner(dev, "CREATE TABLE d (v int);").await;
    dev
}

/// E17 narrowed: once GC passed dev's branch point on both dev's and main's timelines, a base
/// read live (the stored one is another model version's) is read on main's timeline at that
/// point, which main retains while dev exists; the merge analyses against it.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn a_branch_point_behind_gc_is_read_on_the_parent() {
    live(|fx| async move {
        let dev = a_dev_m_d(&fx).await;
        let (source, dev_tl, point) = fx.base_location(dev).await;
        assert_eq!(source, BaseSource::Create);
        assert_eq!(
            fx.branch(dev).await.timeline_id.as_deref(),
            Some(dev_tl.as_str())
        );
        let main_tl = fx.branch(fx.main).await.timeline_id.unwrap();
        fx.fast_gc().await;
        fx.wait_gc_past(point, &[&dev_tl, &main_tl], &[fx.main, dev])
            .await;
        fx.age_base(dev).await;

        let (m, statics) = fx.compute_watching(dev, fx.main).await;

        assert_eq!(m.error, None);
        assert_planned(&m);
        assert_eq!(statics, vec![main_tl], "read on main's timeline");
        assert_eq!(fx.base_tables(dev).await, vec!["a".to_string()]);
    })
    .await;
}

/// A keep merge's base is recorded on dev's timeline at the merge, which no timeline
/// retains: once GC passed it, the merge stays a draft with `merge_base_unavailable`, and no
/// static compute is started.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn a_keep_merge_base_behind_gc_is_unavailable() {
    live(|fx| async move {
        let dev = a_dev_m_d(&fx).await;
        let m = fx.merge_into_main(dev).await;
        assert_planned(&m);
        let m = fx.rehearse(m.id).await;
        assert_rehearsed_clean(&m);
        let m = fx.apply(m.id).await;
        assert_done(&m);
        let (source, on, point) = fx.base_location(dev).await;
        assert_eq!(source, BaseSource::Merge);
        assert_eq!(
            fx.branch(dev).await.timeline_id.as_deref(),
            Some(on.as_str())
        );
        fx.fast_gc().await;
        fx.wait_gc_past(point, &[&on], &[fx.main, dev]).await;
        fx.owner(dev, "CREATE TABLE d2 (v int);").await;
        fx.age_base(dev).await;

        let (m, statics) = fx.compute_watching(dev, fx.main).await;

        assert_base_unavailable(&m);
        assert!(statics.is_empty(), "no static compute started: {statics:?}");
    })
    .await;
}

/// After a restore, dev's copied create base points at its old timeline (the backup's), still
/// a child of main's: it is read on main's timeline at dev's branch point. Once the backup is
/// deleted, nothing keeps that location, and the merge gives `merge_base_unavailable`.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn a_restored_branch_reads_its_base_until_the_backup_is_deleted() {
    live(|fx| async move {
        let dev = a_dev_m_d(&fx).await;
        let (_, old_tl, point) = fx.base_location(dev).await;
        let noted = fx.value(dev, "SELECT pg_current_wal_insert_lsn();").await;
        // WAL after the restore point, which the backup keeps.
        fx.admin(dev, CHURN).await;

        assert!(
            fx.metadata
                .set_branch_status(dev, &[BranchStatus::Ready], BranchStatus::Resetting)
                .await
                .unwrap()
        );
        let backup = BranchId::new();
        fx.worker
            .handle_branch_job(BranchJob::Restore {
                branch_id: dev,
                backup_id: backup,
                backup_name: "dev-before-restore-20261008T120000Z".into(),
                at: PointInTime::Lsn(noted),
            })
            .await
            .expect("restore job");
        let restored = fx.branch(dev).await;
        assert_eq!(restored.status, BranchStatus::Ready, "{:?}", restored.error);
        assert_eq!(restored.error, None);
        assert_ne!(restored.timeline_id.as_deref(), Some(old_tl.as_str()));
        assert_eq!(
            fx.branch(backup).await.timeline_id.as_deref(),
            Some(old_tl.as_str())
        );
        let copied = fx.base_location(dev).await;
        assert_eq!(
            copied,
            (BaseSource::Create, old_tl.clone(), point),
            "the copied base"
        );

        let main_tl = fx.branch(fx.main).await.timeline_id.unwrap();
        fx.fast_gc().await;
        fx.wait_gc_past(point, &[&old_tl, &main_tl], &[fx.main, dev])
            .await;
        fx.age_base(dev).await;
        let (m, statics) = fx.compute_watching(dev, fx.main).await;
        assert_eq!(m.error, None);
        assert_planned(&m);
        assert_eq!(statics, vec![main_tl], "read on main's timeline");
        assert_eq!(fx.base_tables(dev).await, vec!["a".to_string()]);

        // Delete the backup (as the API does for a branch: claimed, then the job).
        assert!(
            fx.metadata
                .set_branch_status(backup, &[BranchStatus::Ready], BranchStatus::Resetting)
                .await
                .unwrap()
        );
        assert_eq!(
            fx.metadata.begin_branch_delete(backup).await.unwrap(),
            BranchDeleteOutcome::Started
        );
        fx.worker
            .handle_branch_job(BranchJob::Delete { branch_id: backup })
            .await
            .expect("delete job");
        assert_eq!(fx.branch(backup).await.status, BranchStatus::Deleted);

        // Recompute with the base aged again: its timeline is gone.
        fx.age_base(dev).await;
        let job = tokio::spawn({
            let fx = fx.clone();
            async move { fx.save(m.id, MergeDraftPatch::default()).await }
        });
        let (m, statics) = fx.static_timelines_while(job).await;
        assert_base_unavailable(&m);
        assert!(statics.is_empty(), "no static compute started: {statics:?}");
    })
    .await;
}

/// A grandchild (main → dev → feat): once GC passed feat's branch point on feat's and dev's
/// timelines, feat's base is read on dev's timeline at that point.
#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn a_grandchild_branch_point_behind_gc_is_read_on_its_parent() {
    live(|fx| async move {
        fx.owner(fx.main, "CREATE TABLE a (v int);").await;
        let dev = fx.child("dev", fx.main).await;
        fx.owner(dev, "CREATE TABLE d1 (v int);").await;
        let feat = fx.child("feat", dev).await;
        fx.owner(dev, "CREATE TABLE d2 (v int);").await;
        fx.owner(feat, "CREATE TABLE f (v int);").await;
        let (source, feat_tl, point) = fx.base_location(feat).await;
        assert_eq!(source, BaseSource::Create);
        let dev_tl = fx.branch(dev).await.timeline_id.unwrap();
        fx.fast_gc().await;
        fx.wait_gc_past(point, &[&feat_tl, &dev_tl], &[fx.main, dev, feat])
            .await;
        fx.age_base(feat).await;

        let (m, statics) = fx.compute_watching(feat, dev).await;

        assert_eq!(m.error, None);
        assert_planned(&m);
        assert_eq!(statics, vec![dev_tl], "read on dev's timeline");
        assert_eq!(
            fx.base_tables(feat).await,
            vec!["a".to_string(), "d1".to_string()]
        );
    })
    .await;
}
