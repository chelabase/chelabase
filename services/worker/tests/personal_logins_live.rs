//! Personal Postgres logins against real computes (members hub step 1, T6; spec Part 6
//! "Convergence", "Memberships", "Revocation is immediate"; D2; MH-9, MH-10, MH-16, MH-17,
//! MH-23): the member-role reconciler converges each (member, Chelabase database) login on
//! every branch from its metadata record, and the proxy answers its record's verifier.
//!
//! Same setup as `member_roles_live` (the workbench stack, the fork image, `psql` in the
//! runner, the proxy's CA):
//!
//! ```text
//! just workbench test -p worker --test personal_logins_live -- --run-ignored only
//! ```
//!
//! These run in the end-of-step live pass only (plan P22): the in-process worker would race
//! an older stack worker's resync and sweep on the same databases. Each test makes its own
//! tenant and project, provisions a database there and deletes it afterwards, pass or fail.

use std::future::Future;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use chelabase_metadata::members::{Admission, GrantRow, MemberRecord};
use chelabase_metadata::{
    BranchStatus, CreateBranchOutcome, DatabaseStatus, MetadataClient, NewBranch,
};
use chelabase_nats::{BranchJob, MemberRolesReply, PointInTime};
use chelabase_neon::{ComputeBackend, ComputeLocator};
use chelabase_types::access::PlatformRole;
use chelabase_types::role_name::is_marked_sql;
use chelabase_types::{BranchId, DatabaseId, TenantId};
use chrono::Utc;
use uuid::Uuid;
use worker::config::RealWorker;

mod common;
use common::{cleanup, proxy_ca_path, proxy_hostaddr, setup};

/// Group roles the owner makes in SQL (`NOLOGIN`).
const POOL: [&str; 3] = ["billing", "sales", "hr"];

/// One provisioned database in its own tenant and project, with a tenant admin and two
/// members (active, no grants yet).
struct Fx {
    worker: RealWorker,
    metadata: MetadataClient,
    project: Uuid,
    id: DatabaseId,
    main: BranchId,
    ep: String,
    database: String,
    admin: MemberRecord,
    members: [MemberRecord; 2],
}

async fn live<F, Fut>(body: F)
where
    F: FnOnce(Arc<Fx>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (worker, metadata, _config) = setup().await;
    let tenant = TenantId::new();
    sqlx::query("INSERT INTO tenants (id, slug) VALUES ($1, $2)")
        .bind(tenant.as_uuid())
        .bind(format!("live-personal-{}", tenant.as_uuid().simple()))
        .execute(metadata.pool())
        .await
        .expect("insert the tenant");
    let project = metadata
        .create_project(tenant, "personal logins live")
        .await
        .expect("create the project");
    let id = DatabaseId::new();
    let name = format!("live-{}", &id.to_string()[..8]);
    metadata
        .insert_database_pending(id, tenant, project.id, &name)
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
        panic!("provision failed: {provisioned:?}");
    };
    let main = metadata
        .get_branch(BranchId::from(id))
        .await
        .unwrap()
        .expect("main branch");
    let member = |n: &str| {
        let metadata = metadata.clone();
        let n = n.to_string();
        async move {
            metadata
                .ensure_member_first_comer_for_tests(
                    tenant,
                    &format!("z-{n}-{}", Uuid::new_v4()),
                    &format!("{n}@personal.live"),
                    &n,
                    Admission::Active,
                )
                .await
                .expect("a member")
        }
    };
    let admin = member("admin").await;
    assert!(admin.tenant_admin, "the first member claims tenant admin");
    let members = [member("m1").await, member("m2").await];
    let fx = Arc::new(Fx {
        ep: main.endpoint_id.clone().expect("endpoint"),
        database: record.database_name.clone().expect("database name"),
        main: main.id,
        worker,
        metadata,
        project: project.id.as_uuid(),
        id,
        admin,
        members,
    });
    for role in POOL {
        fx.as_owner(&format!("CREATE ROLE {role} NOLOGIN;")).await;
    }

    let outcome = tokio::spawn(body(fx.clone())).await;
    cleanup(&fx.worker, &fx.metadata, fx.id).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

impl Fx {
    /// SQL as cloud_admin on `ep` in `database` (forwarding off).
    async fn sql_on(&self, ep: &str, database: &str, sql: &str) -> String {
        self.worker
            .computes
            .admin_sql(ep, database, sql)
            .await
            .unwrap_or_else(|e| panic!("admin_sql {sql:?}: {e:#}"))
    }

    async fn sql(&self, ep: &str, sql: &str) -> String {
        self.sql_on(ep, "postgres", sql).await
    }

    /// SQL as the branch owner on main (forwarding off).
    async fn as_owner(&self, sql: &str) -> String {
        self.sql(&self.ep, &format!("SET ROLE postgres;\n{sql}"))
            .await
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

    async fn grant(&self, member: Uuid, role: PlatformRole, roles: &[&str]) {
        self.metadata
            .put_project_grant(
                self.admin.id,
                member,
                self.project,
                &GrantRow {
                    platform_role: role,
                    pg_roles: roles.iter().map(|r| r.to_string()).collect(),
                },
            )
            .await
            .expect("put the grant");
    }

    async fn reconcile(&self, branch: BranchId) {
        match self.worker.reconcile_member_roles(branch, None).await {
            MemberRolesReply::Done { .. } => {}
            MemberRolesReply::Failed { message } => panic!("reconcile failed: {message}"),
        }
    }

    /// `member`'s login `name` on this database with `password`, valid for 90 days, as the
    /// API's create writes it (the record first, with its verifier).
    async fn create_login(&self, member: Uuid, name: &str, password: &str) {
        let verifier = chelabase_neon::scram::verifier(password);
        self.metadata
            .create_personal_login(
                member,
                self.id.as_uuid(),
                name,
                &verifier,
                Utc::now() + chrono::Duration::days(90),
            )
            .await
            .unwrap_or_else(|e| panic!("create the login {name}: {e:?}"));
    }

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

    async fn role_exists(&self, ep: &str, name: &str) -> bool {
        self.sql(
            ep,
            &format!("SELECT count(*) FROM pg_roles WHERE rolname = '{name}';"),
        )
        .await
        .trim()
            == "1"
    }

    async fn is_marked(&self, ep: &str, name: &str) -> bool {
        self.sql(ep, &is_marked_sql(name)).await.trim() == "t"
    }

    /// The verifier the proxy would authenticate `name` with on `ep`
    /// ([`MetadataClient::routable_role_secret`]): `None` only for an unknown or unroutable
    /// endpoint; `Some(None)` for a known endpoint without the role's row, or a row with no
    /// live verifier (revoked, expired).
    async fn proxy_secret(&self, ep: &str, name: &str) -> Option<Option<String>> {
        self.metadata
            .routable_role_secret(ep, name)
            .await
            .expect("read the role secret")
    }

    /// psql straight to `ep`'s compute, in the main database.
    async fn direct(
        &self,
        ep: &str,
        user: &str,
        password: &str,
        script: &str,
    ) -> Result<String, String> {
        let control = std::env::var("CHELABASE_COMPUTE_CONTROL_NETWORK")
            .expect("CHELABASE_COMPUTE_CONTROL_NETWORK");
        let stack = std::env::var("CHELABASE_STACK").unwrap_or_else(|_| "main".into());
        let addr = chelabase_neon::DockerLocator::connect(&control, &stack)
            .unwrap()
            .postgres_addr(ep)
            .await
            .expect("the compute's control address")
            .expect("on the control network");
        let tls = std::env::var("CHELABASE_COMPUTE_TLS_DIR").expect("CHELABASE_COMPUTE_TLS_DIR");
        let conn = format!(
            "host={} hostaddr={} port={} dbname={} sslmode=verify-full channel_binding=require \
             sslrootcert={tls}/current/ca.crt sslcert={tls}/current/client.crt sslkey={tls}/current/client.key",
            chelabase_neon::compute_tls::server_name(ep),
            addr.ip(),
            addr.port(),
            self.database,
        );
        psql(&conn, user, password, script).await
    }

    /// psql through the proxy to `ep`, in the main database.
    async fn proxied(
        &self,
        ep: &str,
        user: &str,
        password: &str,
        script: &str,
    ) -> Result<String, String> {
        psql(&proxy_conn(ep, &self.database), user, password, script).await
    }
}

/// `script` on psql's stdin (stop at the first error, errors with their SQLSTATE): the output,
/// or psql's error.
async fn psql(conn: &str, user: &str, password: &str, script: &str) -> Result<String, String> {
    let mut child = tokio::process::Command::new("psql")
        .arg(conn)
        .args([
            "-X",
            "-qtA",
            "-v",
            "ON_ERROR_STOP=1",
            "-v",
            "VERBOSITY=verbose",
            "-f",
            "-",
        ])
        .env("PGUSER", user)
        .env("PGPASSWORD", password)
        .env("PGCONNECT_TIMEOUT", "10")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn psql");
    let mut stdin = child.stdin.take().unwrap();
    tokio::io::AsyncWriteExt::write_all(&mut stdin, script.as_bytes())
        .await
        .unwrap();
    drop(stdin);
    let out = child.wait_with_output().await.unwrap();
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

fn proxy_conn(ep: &str, database: &str) -> String {
    let domain = std::env::var("CHELABASE_PROXY_DOMAIN")
        .ok()
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| "localtest.me".into());
    let port = std::env::var("CHELABASE_PROXY_PG_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
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
        "host={ep}.{domain} hostaddr={hostaddr} port={port} dbname={database} sslmode=verify-full sslrootcert={}",
        ca.display()
    )
}

/// `SET ROLE` refused because the role isn't reachable.
fn assert_set_role_refused(outcome: Result<String, String>, what: &str) {
    let err = outcome.expect_err(what);
    assert!(
        err.contains("42501") && err.contains("permission denied to set role"),
        "{what}: {err}"
    );
}

/// A password refused (through the proxy or straight to the compute).
fn assert_password_refused(outcome: Result<String, String>, what: &str) {
    let err = outcome.expect_err(what);
    assert!(
        err.contains("password authentication failed"),
        "{what}: {err}"
    );
}

/// Spec Part 6 "Memberships" (R10, D1, MH-23): through the proxy, the login becomes exactly
/// the roles its member holds at Developer or above, with `SET ROLE` only (no INHERIT), never
/// the owner or another role; it is marked and expires.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the members hub images (end-of-step pass)"]
async fn a_personal_login_reaches_only_its_safe_set_through_set_role() {
    live(|fx| async move {
        let m = fx.members[0].id;
        fx.grant(m, PlatformRole::Developer, &["billing", "sales"]).await;
        fx.create_login(m, "pl_alice", "pl-alice-pass").await;
        fx.reconcile(fx.main).await;

        assert!(fx.is_marked(&fx.ep, "pl_alice").await, "marked in its create transaction");
        let expires = fx
            .sql(&fx.ep, "SELECT rolvaliduntil IS NOT NULL AND rolvaliduntil > now() FROM pg_roles WHERE rolname = 'pl_alice';")
            .await;
        assert_eq!(expires.trim(), "t", "VALID UNTIL is the record's");
        assert!(matches!(fx.proxy_secret(&fx.ep, "pl_alice").await, Some(Some(_))), "its row on main");

        let me = |script: &'static str| {
            let fx = fx.clone();
            async move { fx.proxied(&fx.ep, "pl_alice", "pl-alice-pass", script).await }
        };
        assert_eq!(me("SELECT current_user;").await.unwrap(), "pl_alice");
        assert_eq!(me("SET ROLE billing;\nSELECT current_user;").await.unwrap(), "billing");
        assert_eq!(me("SET ROLE sales;\nSELECT current_user;").await.unwrap(), "sales");
        assert_set_role_refused(me("SET ROLE hr;").await, "hr isn't granted");
        assert_set_role_refused(me("SET ROLE postgres;").await, "never the owner");
        // INHERIT FALSE: without `SET ROLE` it holds nothing of billing's.
        assert_eq!(
            me("SELECT pg_has_role('pl_alice', 'billing', 'USAGE')::text;").await.unwrap(),
            "false"
        );
        let wrong = fx.proxied(&fx.ep, "pl_alice", "not-the-password", "SELECT 1;").await;
        assert_password_refused(wrong, "a wrong password");
    })
    .await;
}

/// D2, S1: a new branch carries no personal row from its parent; its login comes from its own
/// reconcile, with the record's verifier, marked.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the members hub images (end-of-step pass)"]
async fn a_new_branch_gets_the_login_through_the_reconcile() {
    live(|fx| async move {
        let m = fx.members[0].id;
        fx.grant(m, PlatformRole::Developer, &["billing"]).await;
        fx.create_login(m, "pl_bob", "pl-bob-pass").await;
        fx.reconcile(fx.main).await;

        let child = fx.child("pl-dev").await;
        let child_ep = fx.endpoint_of(child).await;
        fx.reconcile(child).await;

        assert!(fx.is_marked(&child_ep, "pl_bob").await);
        assert!(
            matches!(fx.proxy_secret(&child_ep, "pl_bob").await, Some(Some(_))),
            "its row on the child"
        );
        assert_eq!(
            fx.proxied(
                &child_ep,
                "pl_bob",
                "pl-bob-pass",
                "SET ROLE billing;\nSELECT current_user;"
            )
            .await
            .unwrap(),
            "billing"
        );
    })
    .await;
}

/// MH-10 (a restore before the deletion): the restored catalog holds the deleted login,
/// marked; the proxy refuses it at once (no row, no record), and the reconcile drops it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the members hub images (end-of-step pass)"]
async fn a_restore_before_deletion_brings_no_login_back() {
    live(|fx| async move {
        let m = fx.members[0].id;
        fx.grant(m, PlatformRole::Developer, &["billing"]).await;
        fx.create_login(m, "pl_carol", "pl-carol-pass").await;
        fx.reconcile(fx.main).await;
        assert!(fx.role_exists(&fx.ep, "pl_carol").await);
        tokio::time::sleep(Duration::from_secs(2)).await;
        let noted = Utc::now();
        tokio::time::sleep(Duration::from_secs(2)).await;
        fx.metadata
            .delete_personal_login(m, m, fx.id.as_uuid())
            .await
            .expect("delete the login");
        fx.reconcile(fx.main).await;
        assert!(
            !fx.role_exists(&fx.ep, "pl_carol").await,
            "dropped with its record"
        );

        let moved = fx
            .metadata
            .set_branch_status(fx.main, &[BranchStatus::Ready], BranchStatus::Resetting)
            .await
            .unwrap();
        assert!(moved, "main was not ready");
        fx.worker
            .handle_branch_job(BranchJob::Restore {
                branch_id: fx.main,
                backup_id: BranchId::new(),
                backup_name: "personal-before-restore-20261023T120000Z".into(),
                at: PointInTime::Timestamp(noted),
            })
            .await
            .expect("restore job");
        let main = fx.metadata.get_branch(fx.main).await.unwrap().unwrap();
        assert_eq!(main.status, BranchStatus::Ready, "{:?}", main.error);
        // Whether or not a reconcile ran since (the stack's own worker may), the proxy has no
        // verifier for it.
        assert!(
            !matches!(fx.proxy_secret(&fx.ep, "pl_carol").await, Some(Some(_))),
            "no record, no verifier"
        );
        assert_password_refused(
            fx.proxied(&fx.ep, "pl_carol", "pl-carol-pass", "SELECT 1;")
                .await,
            "the restored login",
        );
        fx.reconcile(fx.main).await;
        assert!(
            !fx.role_exists(&fx.ep, "pl_carol").await,
            "the restored orphan is dropped"
        );
    })
    .await;
}

/// MH-9, MH-17: removing the member revokes the login at once on every branch (the proxy has
/// no verifier before any compute work, as for a suspended compute); the reconcile then drops
/// it everywhere and ends its open sessions.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the members hub images (end-of-step pass)"]
async fn a_removed_member_loses_the_login_on_every_branch() {
    live(|fx| async move {
        let m = fx.members[0].id;
        fx.grant(m, PlatformRole::Developer, &["billing"]).await;
        fx.create_login(m, "pl_dave", "pl-dave-pass").await;
        fx.reconcile(fx.main).await;
        let child = fx.child("pl-dev").await;
        let child_ep = fx.endpoint_of(child).await;
        fx.reconcile(child).await;
        for ep in [&fx.ep, &child_ep] {
            assert!(fx.role_exists(ep, "pl_dave").await, "{ep}");
        }
        let session = {
            let fx = fx.clone();
            tokio::spawn(async move {
                fx.direct(&fx.ep, "pl_dave", "pl-dave-pass", "SELECT pg_sleep(60);")
                    .await
            })
        };
        tokio::time::sleep(Duration::from_secs(2)).await;

        fx.metadata
            .remove_member(fx.admin.id, m)
            .await
            .expect("remove the member");

        for ep in [&fx.ep, &child_ep] {
            assert!(
                !matches!(fx.proxy_secret(ep, "pl_dave").await, Some(Some(_))),
                "{ep}: revoked in the removal's transaction"
            );
        }
        fx.reconcile(fx.main).await;
        fx.reconcile(child).await;
        for ep in [&fx.ep, &child_ep] {
            assert!(!fx.role_exists(ep, "pl_dave").await, "{ep}: dropped");
            assert_eq!(
                fx.proxy_secret(ep, "pl_dave").await,
                Some(None),
                "{ep}: its row is gone (a known endpoint without the role)"
            );
        }
        let ended = tokio::time::timeout(Duration::from_secs(10), session)
            .await
            .expect("the session ended at once")
            .unwrap();
        let err = ended.expect_err("the session was terminated");
        assert!(
            err.contains("57P01") || err.contains("terminating connection"),
            "{err}"
        );
    })
    .await;
}

/// MH-16: on a protected branch a Developer's login holds the DDL guard's marker and is refused
/// DDL; with `guard_logins` on (the default) an Admin's login is guarded too.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the members hub images (end-of-step pass)"]
async fn a_guarded_login_is_refused_ddl_on_a_protected_branch() {
    live(|fx| async move {
        let m = fx.members[0].id;
        fx.grant(m, PlatformRole::Developer, &["billing"]).await;
        fx.sql_on(
            &fx.ep,
            &fx.database,
            "GRANT CREATE ON SCHEMA public TO PUBLIC;",
        )
        .await;
        fx.metadata
            .set_branch_protected(fx.main, true)
            .await
            .unwrap();
        fx.create_login(m, "pl_erin", "pl-erin-pass").await;
        fx.create_login(fx.admin.id, "pl_root", "pl-root-pass")
            .await;
        fx.reconcile(fx.main).await;

        let locked = fx
            .sql(
                &fx.ep,
                "SELECT pg_has_role('pl_erin', 'chelabase_schema_locked', 'MEMBER')::text;",
            )
            .await;
        assert_eq!(locked.trim(), "true", "the Developer's login is locked");
        let refused = |out: &Result<String, String>| {
            out.as_ref()
                .is_err_and(|e| e.contains("42501") && e.contains("branch is protected"))
        };
        let out = fx
            .proxied(
                &fx.ep,
                "pl_erin",
                "pl-erin-pass",
                "CREATE TABLE public.pl_t1 (id int);",
            )
            .await;
        assert!(refused(&out), "{out:?}");
        let out = fx
            .proxied(
                &fx.ep,
                "pl_root",
                "pl-root-pass",
                "CREATE TABLE public.pl_t2 (id int);",
            )
            .await;
        assert!(
            refused(&out),
            "guard_logins guards an Admin's login too: {out:?}"
        );

        fx.metadata
            .set_branch_protected(fx.main, false)
            .await
            .unwrap();
        fx.reconcile(fx.main).await;
        fx.sql_on(
            &fx.ep,
            &fx.database,
            "REVOKE CREATE ON SCHEMA public FROM PUBLIC;",
        )
        .await;
    })
    .await;
}

/// P8, D2: a reset changes the record; each branch's reconcile sets the new verifier, so the
/// old password is refused and the new one works on every branch.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the members hub images (end-of-step pass)"]
async fn a_reset_password_works_and_the_old_one_is_refused_on_every_branch() {
    live(|fx| async move {
        let m = fx.members[0].id;
        fx.grant(m, PlatformRole::Developer, &["billing"]).await;
        fx.create_login(m, "pl_fay", "pl-fay-old").await;
        fx.reconcile(fx.main).await;
        let child = fx.child("pl-dev").await;
        let child_ep = fx.endpoint_of(child).await;
        fx.reconcile(child).await;
        for ep in [&fx.ep, &child_ep] {
            assert_eq!(
                fx.direct(ep, "pl_fay", "pl-fay-old", "SELECT current_user;")
                    .await
                    .unwrap(),
                "pl_fay"
            );
        }

        let verifier = chelabase_neon::scram::verifier("pl-fay-new");
        fx.metadata
            .reset_personal_login(
                m,
                fx.id.as_uuid(),
                &verifier,
                Utc::now() + chrono::Duration::days(90),
            )
            .await
            .expect("reset");
        fx.reconcile(fx.main).await;
        fx.reconcile(child).await;

        for ep in [&fx.ep, &child_ep] {
            assert_password_refused(
                fx.direct(ep, "pl_fay", "pl-fay-old", "SELECT 1;").await,
                "the old password",
            );
            assert_eq!(
                fx.direct(ep, "pl_fay", "pl-fay-new", "SELECT current_user;")
                    .await
                    .unwrap(),
                "pl_fay"
            );
            assert_eq!(
                fx.proxied(ep, "pl_fay", "pl-fay-new", "SELECT current_user;")
                    .await
                    .unwrap(),
                "pl_fay",
                "{ep}: the proxy answers the record's verifier"
            );
        }
    })
    .await;
}

/// MH-18 as amended (T6-C1): the escalation the review found. On an unprotected branch the
/// owner side (here the owner itself; in the finding a Developer `SET ROLE app_owner`) gives a
/// personal login `CREATE` on a schema; the login plants a `SECURITY DEFINER` function and a
/// view over a table it can't read, both open to PUBLIC. The cleanup (kept login) and the drop
/// hand them to the powerless `chelabase_orphaned`, never the owner: nobody runs the function
/// with the owner's rights, and the view reads nothing the login couldn't.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack on the members hub images (end-of-step pass)"]
async fn a_planted_definer_function_or_view_is_quarantined_not_given_to_the_owner() {
    live(|fx| async move {
        let m = fx.members[0].id;
        fx.grant(m, PlatformRole::Developer, &["billing"]).await;
        fx.create_login(m, "pl_gus", "pl-gus-pass").await;
        fx.reconcile(fx.main).await;
        let owner = |sql: String| {
            let fx = fx.clone();
            async move {
                fx.sql_on(&fx.ep, &fx.database, &format!("SET ROLE postgres;\n{sql}"))
                    .await
            }
        };
        owner(
            "CREATE SCHEMA app;\n\
             CREATE TABLE app.secret (v text);\n\
             INSERT INTO app.secret VALUES ('owner-only');\n\
             GRANT USAGE ON SCHEMA app TO PUBLIC;\n\
             GRANT CREATE ON SCHEMA app TO pl_gus;"
                .into(),
        )
        .await;
        let gus = |script: &'static str| {
            let fx = fx.clone();
            async move { fx.proxied(&fx.ep, "pl_gus", "pl-gus-pass", script).await }
        };
        gus("CREATE FUNCTION app.whoami() RETURNS text LANGUAGE sql SECURITY DEFINER \
             AS $$ SELECT current_user::text $$;\n\
             GRANT EXECUTE ON FUNCTION app.whoami() TO PUBLIC;\n\
             CREATE VIEW app.leak AS SELECT v FROM app.secret;\n\
             GRANT SELECT ON app.leak TO PUBLIC;")
        .await
        .expect("the login plants its objects");

        // The kept login's cleanup (its leftovers in `pg_shdepend`).
        fx.reconcile(fx.main).await;
        let owners = |fx: Arc<Fx>| async move {
            fx.sql_on(
                &fx.ep,
                &fx.database,
                "SELECT (SELECT proowner::regrole::text FROM pg_proc WHERE oid = 'app.whoami()'::regprocedure) \
                 || ',' || (SELECT relowner::regrole::text FROM pg_class WHERE oid = 'app.leak'::regclass);",
            )
            .await
        };
        assert_eq!(
            owners(fx.clone()).await.trim(),
            "chelabase_orphaned,chelabase_orphaned",
            "quarantined, not the owner's"
        );
        let quarantine = fx
            .sql(
                &fx.ep,
                "SELECT rolcanlogin::text || ',' || rolsuper::text || ',' || rolcreaterole::text \
                 || ',' || rolcreatedb::text || ',' || rolbypassrls::text || ',' || rolreplication::text \
                 || ',' || (SELECT count(*) FROM pg_auth_members WHERE roleid = r.oid OR member = r.oid)::text \
                 FROM pg_roles r WHERE rolname = 'chelabase_orphaned';",
            )
            .await;
        assert_eq!(
            quarantine.trim(),
            "false,false,false,false,false,false,0",
            "NOLOGIN, no attributes, no members, member of nothing"
        );
        let checks = fx
            .sql_on(
                &fx.ep,
                &fx.database,
                "SELECT has_function_privilege('billing', 'app.whoami()', 'EXECUTE')::text \
                 || ',' || has_function_privilege('postgres', 'app.whoami()', 'EXECUTE')::text \
                 || ',' || has_schema_privilege('pl_gus', 'app', 'CREATE')::text;",
            )
            .await;
        assert_eq!(
            checks.trim(),
            "false,false,false",
            "EXECUTE revoked from PUBLIC; the login's own grant gone (DROP OWNED)"
        );

        // Another role calling it doesn't run as the owner: refused outright.
        let called = fx
            .worker
            .computes
            .admin_sql(
                &fx.ep,
                &fx.database,
                "SET ROLE billing;\nSELECT app.whoami();",
            )
            .await;
        match called {
            Ok(out) => assert!(
                !out.contains("postgres") && !out.contains("cloud_admin"),
                "never the owner's rights: {out}"
            ),
            Err(e) => assert!(
                format!("{e:#}").contains("permission denied for function whoami"),
                "{e:#}"
            ),
        }
        // The view reads with the quarantine role's rights: nothing.
        let read = fx
            .worker
            .computes
            .admin_sql(&fx.ep, &fx.database, "SET ROLE billing;\nSELECT v FROM app.leak;")
            .await;
        let err = read.expect_err("the view can't read the owner's table");
        assert!(
            format!("{err:#}").contains("permission denied for table secret"),
            "{err:#}"
        );

        // The drop path: a second planted function, then the login's deletion.
        owner("GRANT CREATE ON SCHEMA app TO pl_gus;".into()).await;
        gus("CREATE FUNCTION app.whoami2() RETURNS text LANGUAGE sql SECURITY DEFINER \
             AS $$ SELECT current_user::text $$;")
            .await
            .expect("the second function");
        fx.metadata
            .delete_personal_login(m, m, fx.id.as_uuid())
            .await
            .expect("delete the login");
        fx.reconcile(fx.main).await;
        assert!(!fx.role_exists(&fx.ep, "pl_gus").await, "dropped");
        let after = fx
            .sql_on(
                &fx.ep,
                &fx.database,
                "SELECT (SELECT proowner::regrole::text FROM pg_proc WHERE oid = 'app.whoami2()'::regprocedure) \
                 || ',' || has_function_privilege('billing', 'app.whoami2()', 'EXECUTE')::text;",
            )
            .await;
        assert_eq!(
            after.trim(),
            "chelabase_orphaned,false",
            "the drop quarantines too, with EXECUTE revoked from PUBLIC"
        );
    })
    .await;
}
