//! Member roles against real computes (members-and-roles design, part 3; invariant tests 6
//! and 7; rulings R16, R17): the reconciler keeps each branch's hidden `cb_m_*` roles equal to
//! the metadata, and Postgres holds a member role to the roles it may become.
//!
//! Same setup as `roles_live` (the workbench stack, the fork image in
//! `CHELABASE_COMPUTE_IMAGE`), plus `psql` in the runner and the proxy's CA in
//! `CHELABASE_PROXY_CA_FILE` (default `.chelabase/neon/proxy-ca.crt` in the checkout):
//!
//! ```text
//! just workbench test -p worker --test member_roles_live -- --run-ignored only
//! ```
//!
//! Each test makes its own tenant and project (so no other member of the dev tenant is in the
//! model), provisions a database there and deletes it afterwards, pass or fail. The tenant, its
//! project and its members stay (`access_events` is append-only).
//!
//! Invariants checked after every reconcile in `reconcile_converges`:
//! - **Reach (R21):** the roles a member role can `SET ROLE` into (`pg_has_role(…, 'SET')`, any
//!   chain) are exactly the closure of its included granted roles, and none is owner-level.
//! - **Staleness:** every `cb_m_` role's `rolpassword` is its row's password (the SCRAM
//!   verifier checked against the stored password), and a second reconcile changes nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chelabase_metadata::members::{Admission, GrantRow, MemberRecord, OverrideRow};
use chelabase_metadata::{
    BranchStatus, CreateBranchOutcome, DatabaseStatus, MetadataClient, NewBranch,
};
use chelabase_nats::{
    BranchJob, MemberRoleApplied, MemberRoleHandOverRequest, MemberRolesReply, PointInTime,
};
use chelabase_neon::{ComputeBackend, ComputeLocator};
use chelabase_secrets::{open_member_role_password, open_role_password};
use chelabase_types::access::PlatformRole;
use chelabase_types::{BranchId, DatabaseId, TenantId};
use proptest::prelude::*;
use proptest::test_runner::{Config, TestRunner};
use uuid::Uuid;
use worker::config::RealWorker;
use worker::member_roles::member_role_name;
use worker::roles::{RoleOutcome, RolePatch, RoleSpec};

mod common;
use common::{cleanup, proxy_ca_path, proxy_hostaddr, setup};

/// The roles the tests grant from (made in SQL by the owner, `NOLOGIN`).
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
        .bind(format!("live-members-{}", tenant.as_uuid().simple()))
        .execute(metadata.pool())
        .await
        .expect("insert the tenant");
    let project = metadata
        .create_project(tenant, "members live")
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
        teardown(&worker, &metadata, id).await;
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
                .ensure_member(
                    tenant,
                    &format!("z-{n}-{}", Uuid::new_v4()),
                    &format!("{n}@members.live"),
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
    teardown(&fx.worker, &fx.metadata, fx.id).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

async fn teardown(worker: &RealWorker, metadata: &MetadataClient, id: DatabaseId) {
    let _ = sqlx::query("DELETE FROM database_grant_overrides WHERE database_id = $1")
        .bind(id.as_uuid())
        .execute(metadata.pool())
        .await;
    cleanup(worker, metadata, id).await;
}

/// True for a role no member role may hold by name: written out here, apart from the worker's
/// own rule.
fn reserved(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    if lower == "anon" || lower == "authenticated" {
        return false;
    }
    lower.starts_with("pg_")
        || lower.starts_with("cb_m_")
        || lower.starts_with("chelabase_")
        || [
            "owner",
            "postgres",
            "authenticator",
            "cloud_admin",
            "neon_superuser",
            "public",
            "none",
        ]
        .contains(&lower.as_str())
}

/// A role no granted role's chain may hold, as SQL over `pg_roles r`: owner-level (R16, R23:
/// CREATEDB too), holding ADMIN on any role through any membership (N2), or LOGIN (R22).
const UNSAFE: &str = "(r.rolsuper OR r.rolreplication OR r.rolcreaterole OR r.rolbypassrls \
    OR r.rolcreatedb OR r.rolcanlogin \
    OR pg_has_role(r.oid, 'postgres', 'MEMBER') OR pg_has_role(r.oid, 'neon_superuser', 'MEMBER') \
    OR pg_has_role(r.oid, 'cloud_admin', 'MEMBER') \
    OR EXISTS (SELECT 1 FROM pg_auth_members x WHERE x.admin_option \
               AND pg_has_role(r.oid, x.member, 'MEMBER')))";

/// What the live catalog says, read apart from the reconciler: every role, the unsafe ones
/// and every role's `SET` or ADMIN memberships (member roles' included, R23).
#[derive(Debug, Default)]
struct Live {
    roles: BTreeSet<String>,
    unsafe_roles: BTreeSet<String>,
    edges: BTreeMap<String, BTreeSet<String>>,
}

impl Live {
    fn reach(&self, from: &str) -> BTreeSet<String> {
        let mut seen = BTreeSet::from([from.to_string()]);
        let mut queue = vec![from.to_string()];
        while let Some(r) = queue.pop() {
            for next in self.edges.get(&r).into_iter().flatten() {
                if seen.insert(next.clone()) {
                    queue.push(next.clone());
                }
            }
        }
        seen
    }

    /// R21–R23, N2: the granted roles of `candidate` that exist and whose chain (themselves
    /// included) holds no reserved or unsafe role.
    fn safe(&self, candidate: &BTreeSet<String>) -> BTreeSet<String> {
        candidate
            .iter()
            .filter(|r| self.roles.contains(*r))
            .filter(|r| {
                self.reach(r)
                    .iter()
                    .all(|n| !reserved(n) && !self.unsafe_roles.contains(n))
            })
            .cloned()
            .collect()
    }

    /// What `SET ROLE` reaches from `roles`, them included.
    fn closure(&self, roles: &BTreeSet<String>) -> BTreeSet<String> {
        roles.iter().flat_map(|r| self.reach(r)).collect()
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

    async fn sql(&self, sql: &str) -> String {
        self.sql_on(&self.ep, "postgres", sql).await
    }

    /// SQL as the branch owner on main (forwarding off).
    async fn as_owner(&self, sql: &str) -> String {
        self.sql(&format!("SET ROLE postgres;\n{sql}")).await
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

    async fn narrow(&self, member: Uuid, o: OverrideRow) {
        self.metadata
            .put_override(self.admin.id, member, self.id.as_uuid(), &o)
            .await
            .expect("put the override");
    }

    async fn reconcile(&self, branch: BranchId) -> Vec<MemberRoleApplied> {
        let started = Instant::now();
        let reply = self.worker.reconcile_member_roles(branch, None).await;
        println!("reconcile took {:?}", started.elapsed());
        match reply {
            MemberRolesReply::Done { roles } => roles,
            MemberRolesReply::Failed { message } => panic!("reconcile failed: {message}"),
        }
    }

    async fn live(&self, ep: &str) -> Live {
        let out = self
            .sql_on(
                ep,
                "postgres",
                &format!(
                    "SELECT 'r|' || r.rolname || '|' || {UNSAFE}::text FROM pg_roles r \
                     UNION ALL SELECT 'e|' || m.rolname || '|' || g.rolname FROM pg_auth_members a \
                     JOIN pg_roles m ON m.oid = a.member JOIN pg_roles g ON g.oid = a.roleid \
                     WHERE a.set_option OR a.admin_option;"
                ),
            )
            .await;
        let mut live = Live::default();
        for line in out.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let parts: Vec<&str> = line.split('|').collect();
            match parts.as_slice() {
                ["r", name, flagged] => {
                    live.roles.insert(name.to_string());
                    if *flagged == "true" {
                        live.unsafe_roles.insert(name.to_string());
                    }
                }
                ["e", m, g] => {
                    live.edges
                        .entry(m.to_string())
                        .or_default()
                        .insert(g.to_string());
                }
                _ => panic!("{line}"),
            }
        }
        live
    }

    /// Each `cb_m_` role on `ep` with the roles it holds directly; every membership must be
    /// the reconciler's own shape and every role a plain login role with no settings.
    async fn catalog(&self, ep: &str) -> BTreeMap<String, BTreeSet<String>> {
        let out = self
            .sql_on(
                ep,
                "postgres",
                "SELECT r.rolname || '|' || coalesce(g.rolname, '') || '|' || \
                 coalesce((a.set_option AND NOT a.inherit_option AND NOT a.admin_option \
                 AND a.grantor = 10)::text, 'none') || '|' || \
                 (r.rolcanlogin AND NOT r.rolinherit AND NOT r.rolsuper AND NOT r.rolcreatedb \
                 AND NOT r.rolcreaterole AND NOT r.rolbypassrls AND NOT r.rolreplication \
                 AND NOT EXISTS (SELECT 1 FROM pg_db_role_setting s WHERE s.setrole = r.oid))::text \
                 FROM pg_roles r LEFT JOIN pg_auth_members a ON a.member = r.oid \
                 LEFT JOIN pg_roles g ON g.oid = a.roleid WHERE r.rolname LIKE 'cb\\_m\\_%';",
            )
            .await;
        let mut catalog: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for line in out.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let parts: Vec<&str> = line.split('|').collect();
            assert_eq!(parts.len(), 4, "{line}");
            assert_eq!(
                parts[3], "true",
                "{} has the member-role attributes",
                parts[0]
            );
            let entry = catalog.entry(parts[0].to_string()).or_default();
            if !parts[1].is_empty() {
                assert_eq!(
                    parts[2], "true",
                    "{line}: SET only, no INHERIT or ADMIN, ours"
                );
                entry.insert(parts[1].to_string());
            }
        }
        catalog
    }

    /// Reach: every role each `cb_m_` role can `SET ROLE` into through any chain, and whether
    /// it is owner-level.
    async fn reach(&self, ep: &str) -> BTreeMap<String, BTreeSet<(String, bool)>> {
        let out = self
            .sql_on(
                ep,
                "postgres",
                &format!(
                    "SELECT m.rolname || '|' || r.rolname || '|' || {UNSAFE}::text \
                     FROM pg_roles m CROSS JOIN pg_roles r WHERE m.rolname LIKE 'cb\\_m\\_%' \
                     AND r.oid <> m.oid AND pg_has_role(m.oid, r.oid, 'SET');"
                ),
            )
            .await;
        let mut reach: BTreeMap<String, BTreeSet<(String, bool)>> = BTreeMap::new();
        for line in out.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let parts: Vec<&str> = line.split('|').collect();
            reach
                .entry(parts[0].to_string())
                .or_default()
                .insert((parts[1].to_string(), parts[2] == "true"));
        }
        reach
    }

    /// Staleness: each `cb_m_` role's `rolpassword` checked against its row's password.
    async fn assert_passwords_match_rows(&self, branch: BranchId, ep: &str) {
        let out = self
            .sql_on(
                ep,
                "postgres",
                "SELECT rolname || '|' || coalesce(rolpassword, '') FROM pg_authid \
                 WHERE rolname LIKE 'cb\\_m\\_%';",
            )
            .await;
        let rows = self.rows(branch).await;
        for line in out.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let (role, verifier) = line.split_once('|').unwrap();
            let sealed = rows
                .get(role)
                .unwrap_or_else(|| panic!("{role} has no row on {branch}"));
            let password =
                open_member_role_password(&self.worker.key, branch.as_uuid(), role, sealed)
                    .expect("the row's password opens for this branch");
            assert!(
                chelabase_neon::scram::verifies(verifier, &password),
                "{role}'s catalog password is its row's"
            );
        }
    }

    /// `(role, sealed password)` of each row on `branch`.
    async fn rows(&self, branch: BranchId) -> BTreeMap<String, String> {
        self.metadata
            .member_branch_roles(branch.as_uuid())
            .await
            .unwrap()
            .into_iter()
            .map(|r| (r.role_name, r.password_enc))
            .collect()
    }

    async fn password(&self, branch: BranchId, role: &str) -> String {
        let sealed = self
            .rows(branch)
            .await
            .remove(role)
            .expect("a row for the role");
        open_member_role_password(&self.worker.key, branch.as_uuid(), role, &sealed)
            .expect("open the member role's password")
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

    /// psql straight to `ep`'s compute (as the orchestrator connects a member role).
    async fn direct(
        &self,
        ep: &str,
        user: &str,
        password: &str,
        script: &str,
    ) -> Result<String, String> {
        let addr = self
            .worker
            .computes
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
        psql(&conn, user, password, script).await
    }

    /// As member `m`'s role on main, in the main database.
    async fn as_member(&self, m: Uuid, script: &str) -> Result<String, String> {
        let role = member_role_name(m);
        let password = self.password(self.main, &role).await;
        self.direct(&self.ep, &role, &password, script).await
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

/// A session ended by `pg_terminate_backend` (SQLSTATE 57P01, "terminating connection due to
/// administrator command").
fn assert_terminated(ended: Result<String, String>) {
    let err = ended.expect_err("the session was terminated");
    assert!(
        err.contains("57P01") && err.contains("terminating connection"),
        "{err}"
    );
}

/// `SET ROLE` refused for the reason Postgres gives when a role isn't reachable.
fn assert_set_role_refused(outcome: Result<String, String>, what: &str) {
    let err = outcome.expect_err(what);
    assert!(
        err.contains("42501") && err.contains("permission denied to set role"),
        "{what}: {err}"
    );
}

// --- The model (invariants 6, reach and staleness) ----------------------------------------

/// A random step of the convergence test.
#[derive(Debug, Clone)]
enum Op {
    Grant {
        member: usize,
        role: PlatformRole,
        roles: Vec<usize>,
    },
    DropGrant {
        member: usize,
    },
    Narrow {
        member: usize,
        role: Option<PlatformRole>,
        roles: Option<Vec<usize>>,
    },
    DropNarrowing {
        member: usize,
    },
    CreateRole(usize),
    DropRole(usize),
    /// The pool role becomes owner-level (a member of `neon_superuser`, BYPASSRLS), or stops.
    OwnerLevel(usize, bool),
    /// The owner makes `a` a member of `b` (SET), or revokes it.
    Chain(usize, usize, bool),
    /// The owner grants a pool role straight to a member role.
    OwnerGrantsToMember(usize, usize),
    /// The owner makes `a` hold ADMIN (no SET) on `b` (N2).
    AdminChain(usize, usize),
    /// The pool role gets LOGIN, or loses it (R22).
    Login(usize, bool),
    /// The member role's password drifts (set behind the reconciler's back).
    SelfPassword(usize),
    /// A child of main (at most one in a run); later reconciles cover it too.
    Branch,
}

fn platform_role() -> impl Strategy<Value = PlatformRole> {
    prop_oneof![
        Just(PlatformRole::Viewer),
        Just(PlatformRole::Developer),
        Just(PlatformRole::Admin)
    ]
}

fn pool_subset() -> impl Strategy<Value = Vec<usize>> {
    proptest::sample::subsequence(vec![0usize, 1, 2], 0..=3)
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (0..2usize, platform_role(), pool_subset())
            .prop_map(|(member, role, roles)| Op::Grant { member, role, roles }),
        1 => (0..2usize).prop_map(|member| Op::DropGrant { member }),
        2 => (0..2usize, proptest::option::of(platform_role()), proptest::option::of(pool_subset()))
            .prop_filter("an override narrows something", |(_, r, s)| r.is_some() || s.is_some())
            .prop_map(|(member, role, roles)| Op::Narrow { member, role, roles }),
        1 => (0..2usize).prop_map(|member| Op::DropNarrowing { member }),
        1 => (0..3usize).prop_map(Op::CreateRole),
        1 => (0..3usize).prop_map(Op::DropRole),
        2 => (0..3usize, any::<bool>()).prop_map(|(i, on)| Op::OwnerLevel(i, on)),
        2 => (0..3usize, 0..3usize, any::<bool>())
            .prop_filter("a role isn't its own member", |(a, b, _)| a != b)
            .prop_map(|(a, b, on)| Op::Chain(a, b, on)),
        1 => (0..2usize, 0..3usize).prop_map(|(m, i)| Op::OwnerGrantsToMember(m, i)),
        1 => (0..3usize, 0..3usize)
            .prop_filter("a role isn't its own member", |(a, b)| a != b)
            .prop_map(|(a, b)| Op::AdminChain(a, b)),
        1 => (0..3usize, any::<bool>()).prop_map(|(i, on)| Op::Login(i, on)),
        1 => (0..2usize).prop_map(Op::SelfPassword),
        1 => Just(Op::Branch),
    ]
}

/// What the test believes about the metadata, written apart from the reconciler. Role state
/// lives in the catalog and is read back from it ([`Live`]).
#[derive(Debug, Clone, Default)]
struct Model {
    grants: BTreeMap<usize, (PlatformRole, BTreeSet<String>)>,
    overrides: BTreeMap<usize, (Option<PlatformRole>, Option<BTreeSet<String>>)>,
    branches: Vec<BranchId>,
}

fn names(roles: &[usize]) -> BTreeSet<String> {
    roles.iter().map(|i| POOL[*i].to_string()).collect()
}

impl Model {
    /// The expected `cb_m_` roles of a branch whose catalog is `live`: role name → the roles it
    /// holds.
    fn expected(&self, fx: &Fx, live: &Live) -> BTreeMap<String, BTreeSet<String>> {
        let mut out = BTreeMap::new();
        let admin = live.safe(&live.roles);
        if !admin.is_empty() {
            out.insert(member_role_name(fx.admin.id), admin);
        }
        for (i, m) in fx.members.iter().enumerate() {
            let Some((_, roles)) = self.grants.get(&i) else {
                continue;
            };
            let mut roles = roles.clone();
            if let Some((_, Some(narrow))) = self.overrides.get(&i) {
                roles.retain(|r| narrow.contains(r));
            }
            let roles = live.safe(&roles);
            if !roles.is_empty() {
                out.insert(member_role_name(m.id), roles);
            }
        }
        out
    }
}

async fn role_exists(fx: &Fx, role: &str) -> bool {
    fx.sql(&format!(
        "SELECT count(*) FROM pg_roles WHERE rolname = '{role}';"
    ))
    .await
    .trim()
        == "1"
}

async fn apply(fx: &Fx, model: &mut Model, op: &Op) {
    match op {
        Op::Grant {
            member,
            role,
            roles,
        } => {
            let roles = names(roles);
            let list: Vec<&str> = roles.iter().map(String::as_str).collect();
            fx.grant(fx.members[*member].id, *role, &list).await;
            model.grants.insert(*member, (*role, roles));
        }
        Op::DropGrant { member } => {
            if model.grants.remove(member).is_some() {
                fx.metadata
                    .delete_project_grant(fx.admin.id, fx.members[*member].id, fx.project)
                    .await
                    .expect("delete the grant");
            }
        }
        Op::Narrow {
            member,
            role,
            roles,
        } => {
            let roles = roles.as_ref().map(|r| names(r));
            fx.narrow(
                fx.members[*member].id,
                OverrideRow {
                    platform_role: *role,
                    pg_roles: roles.clone().map(|r| r.into_iter().collect()),
                },
            )
            .await;
            model.overrides.insert(*member, (*role, roles));
        }
        Op::DropNarrowing { member } => {
            if model.overrides.remove(member).is_some() {
                fx.metadata
                    .delete_override(fx.admin.id, fx.members[*member].id, fx.id.as_uuid())
                    .await
                    .expect("delete the override");
            }
        }
        Op::CreateRole(i) => {
            if !role_exists(fx, POOL[*i]).await {
                fx.as_owner(&format!("CREATE ROLE {} NOLOGIN;", POOL[*i]))
                    .await;
            }
        }
        Op::DropRole(i) => {
            if role_exists(fx, POOL[*i]).await {
                fx.sql(&format!("DROP ROLE {};", POOL[*i])).await;
            }
        }
        Op::OwnerLevel(i, on) => {
            if role_exists(fx, POOL[*i]).await {
                let r = POOL[*i];
                if *on {
                    fx.sql(&format!(
                        "GRANT neon_superuser TO {r}; ALTER ROLE {r} BYPASSRLS;"
                    ))
                    .await;
                } else {
                    fx.sql(&format!(
                        "REVOKE neon_superuser FROM {r}; ALTER ROLE {r} NOBYPASSRLS;"
                    ))
                    .await;
                }
            }
        }
        Op::Chain(a, b, on) => {
            let (a, b) = (POOL[*a], POOL[*b]);
            if role_exists(fx, a).await && role_exists(fx, b).await {
                // A grant that would make a cycle is refused, and a revoke of a missing one only
                // warns: either way the catalog is what the model reads back.
                let sql = if *on {
                    format!("SET ROLE postgres; GRANT {b} TO {a};")
                } else {
                    format!("REVOKE {b} FROM {a} GRANTED BY postgres;")
                };
                let _ = fx.worker.computes.admin_sql(&fx.ep, "postgres", &sql).await;
            }
        }
        Op::OwnerGrantsToMember(m, i) => {
            let role = member_role_name(fx.members[*m].id);
            if role_exists(fx, &role).await && role_exists(fx, POOL[*i]).await {
                fx.as_owner(&format!("GRANT {} TO {role};", POOL[*i])).await;
            }
        }
        Op::AdminChain(a, b) => {
            let (a, b) = (POOL[*a], POOL[*b]);
            if role_exists(fx, a).await && role_exists(fx, b).await {
                // Refused when it would make a cycle; the model reads the catalog back.
                let sql = format!(
                    "SET ROLE postgres; GRANT {b} TO {a} WITH ADMIN OPTION, SET FALSE, INHERIT FALSE;"
                );
                let _ = fx.worker.computes.admin_sql(&fx.ep, "postgres", &sql).await;
            }
        }
        Op::Login(i, on) => {
            if role_exists(fx, POOL[*i]).await {
                let attr = if *on { "LOGIN" } else { "NOLOGIN" };
                fx.sql(&format!("ALTER ROLE {} {attr};", POOL[*i])).await;
            }
        }
        Op::SelfPassword(m) => {
            // N3: password drift, made as cloud_admin with forwarding off (a member's own
            // `ALTER ROLE` is refused by the receiver; `escape_attempts_fail` proves that).
            let role = member_role_name(fx.members[*m].id);
            if role_exists(fx, &role).await {
                let verifier = chelabase_neon::scram::verifier(&Uuid::new_v4().to_string());
                fx.sql(&format!("ALTER ROLE {role} PASSWORD '{verifier}';"))
                    .await;
            }
        }
        Op::Branch => {
            if model.branches.len() == 1 {
                let child = fx.child("members-child").await;
                model.branches.push(child);
            }
        }
    }
}

/// Invariant 6 with the reach and staleness invariants: after random grant, override, role,
/// owner-level, membership-chain, owner-grant, self-password and branch operations and a
/// reconcile, each branch's `cb_m_` roles equal the model, `SET ROLE` from each reaches only
/// its desired roles and nothing owner-level, every catalog password is its row's, and a
/// second reconcile changes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the workbench stack"]
async fn reconcile_converges() {
    live(|fx| async move {
        let model = Arc::new(Mutex::new(Model {
            branches: vec![fx.main],
            ..Model::default()
        }));
        let mut runner = TestRunner::new(Config {
            cases: 16,
            max_shrink_iters: 0,
            ..Config::default()
        });
        let handle = tokio::runtime::Handle::current();
        let result = runner.run(&proptest::collection::vec(op(), 1..7), |ops| {
            let fx = fx.clone();
            let model = model.clone();
            tokio::task::block_in_place(|| {
                handle.block_on(async move {
                    let mut m = model.lock().unwrap().clone();
                    // The member roles must exist on main for the owner and self ops to bite.
                    fx.reconcile(fx.main).await;
                    for op in &ops {
                        apply(&fx, &mut m, op).await;
                    }
                    for branch in m.branches.clone() {
                        let ep = fx.endpoint_of(branch).await;
                        fx.reconcile(branch).await;
                        let live = fx.live(&ep).await;
                        let catalog = fx.catalog(&ep).await;
                        let expected = m.expected(&fx, &live);
                        assert_eq!(catalog, expected, "branch {branch} after {ops:?}");
                        // Reach (R21): each member role's SET closure is exactly the closure
                        // of its included granted roles, and holds nothing owner-level.
                        let reach = fx.reach(&ep).await;
                        for (role, granted) in &expected {
                            let reached = reach.get(role).cloned().unwrap_or_default();
                            assert!(
                                reached.iter().all(|(_, owner_level)| !owner_level),
                                "{role} reaches an owner-level role after {ops:?}: {reached:?}"
                            );
                            let names: BTreeSet<String> =
                                reached.into_iter().map(|(r, _)| r).collect();
                            assert_eq!(
                                names,
                                live.closure(granted),
                                "{role}'s reach after {ops:?}"
                            );
                        }
                        assert!(
                            reach.keys().all(|r| expected.contains_key(r)),
                            "no other member role reaches anything after {ops:?}: {reach:?}"
                        );
                        fx.assert_passwords_match_rows(branch, &ep).await;
                        let rows = fx.rows(branch).await;
                        assert_eq!(
                            rows.keys().cloned().collect::<BTreeSet<_>>(),
                            expected.keys().cloned().collect::<BTreeSet<_>>(),
                            "a row for each member role"
                        );
                        fx.reconcile(branch).await;
                        assert_eq!(
                            fx.catalog(&ep).await,
                            catalog,
                            "a second run changes nothing"
                        );
                        assert_eq!(fx.rows(branch).await, rows, "nor any row or password");
                    }
                    *model.lock().unwrap() = m;
                })
            });
            Ok(())
        });
        result.expect("the reconcile converges");
    })
    .await;
}

/// Review focus 1: removing a member's access ends its open sessions.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack"]
async fn revoke_terminates_member_sessions() {
    live(|fx| async move {
        let m = fx.members[0].id;
        fx.grant(m, PlatformRole::Developer, &["billing"]).await;
        fx.reconcile(fx.main).await;
        let role = member_role_name(m);
        let session = {
            let fx = fx.clone();
            tokio::spawn(async move {
                fx.as_member(m, "SET ROLE billing;\nSELECT pg_sleep(60);")
                    .await
            })
        };
        wait_for_sessions(&fx, &role, 1).await;
        fx.metadata
            .delete_project_grant(fx.admin.id, m, fx.project)
            .await
            .unwrap();
        fx.reconcile(fx.main).await;
        let ended = tokio::time::timeout(Duration::from_secs(10), session)
            .await
            .expect("the session ended at once")
            .unwrap();
        assert_terminated(ended);
        assert_eq!(session_count(&fx, &role).await, 0);
        assert!(
            !fx.catalog(&fx.ep).await.contains_key(&role),
            "no role left without a grant"
        );
        assert!(!fx.rows(fx.main).await.contains_key(&role));
    })
    .await;
}

/// M10: removing the member (not just a grant) ends its open sessions too: the grants go with
/// the member, the reconcile finds nothing wanted, locks the role out and drops it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack"]
async fn remove_member_terminates_member_sessions() {
    live(|fx| async move {
        let m = fx.members[0].id;
        fx.grant(m, PlatformRole::Developer, &["billing"]).await;
        fx.reconcile(fx.main).await;
        let role = member_role_name(m);
        let session = {
            let fx = fx.clone();
            tokio::spawn(async move {
                fx.as_member(m, "SET ROLE billing;\nSELECT pg_sleep(60);")
                    .await
            })
        };
        wait_for_sessions(&fx, &role, 1).await;
        fx.metadata.remove_member(fx.admin.id, m).await.unwrap();
        fx.reconcile(fx.main).await;
        let ended = tokio::time::timeout(Duration::from_secs(10), session)
            .await
            .expect("the session ended at once")
            .unwrap();
        assert_terminated(ended);
        assert_eq!(session_count(&fx, &role).await, 0);
        assert!(
            !fx.catalog(&fx.ep).await.contains_key(&role),
            "no role left for a removed member"
        );
        assert!(!fx.rows(fx.main).await.contains_key(&role));
    })
    .await;
}

async fn session_count(fx: &Fx, role: &str) -> usize {
    fx.sql(&format!(
        "SELECT count(*) FROM pg_stat_activity WHERE usename = '{role}';"
    ))
    .await
    .trim()
    .parse()
    .unwrap()
}

async fn wait_for_sessions(fx: &Fx, role: &str, n: usize) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while session_count(fx, role).await != n {
        assert!(
            Instant::now() < deadline,
            "{role}'s session never showed up"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// I6 (c): narrowing {billing, sales} to {sales} revokes `billing` and ends the session
/// holding it; the role stays, and `sales` still works.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack"]
async fn partial_revoke_terminates_the_session_and_keeps_the_rest() {
    live(|fx| async move {
        let m = fx.members[0].id;
        fx.grant(m, PlatformRole::Developer, &["billing", "sales"])
            .await;
        fx.reconcile(fx.main).await;
        let role = member_role_name(m);
        let password = fx.password(fx.main, &role).await;
        let session = {
            let fx = fx.clone();
            tokio::spawn(async move {
                fx.as_member(m, "SET ROLE billing;\nSELECT pg_sleep(60);")
                    .await
            })
        };
        wait_for_sessions(&fx, &role, 1).await;
        fx.grant(m, PlatformRole::Developer, &["sales"]).await;
        fx.reconcile(fx.main).await;
        let ended = tokio::time::timeout(Duration::from_secs(10), session)
            .await
            .expect("the session ended at once")
            .unwrap();
        assert_terminated(ended);
        assert_eq!(
            fx.catalog(&fx.ep).await[&role],
            BTreeSet::from(["sales".to_string()])
        );
        assert_eq!(
            fx.password(fx.main, &role).await,
            password,
            "the role and its password stay"
        );
        assert_eq!(
            fx.as_member(m, "SET ROLE sales;\nSELECT current_user;")
                .await
                .unwrap(),
            "sales"
        );
        assert_set_role_refused(
            fx.as_member(m, "SET ROLE billing;").await,
            "billing is revoked",
        );
    })
    .await;
}

/// Review focus 5: a child inherits its parent's member roles and passwords; its reconcile
/// gives them fresh passwords, so the parent's password never opens a child session.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack"]
async fn child_branch_gets_fresh_member_passwords() {
    live(|fx| async move {
        let m = fx.members[0].id;
        fx.grant(m, PlatformRole::Developer, &["billing"]).await;
        fx.reconcile(fx.main).await;
        let role = member_role_name(m);
        let parent_password = fx.password(fx.main, &role).await;
        let child = fx.child("members-fresh").await;
        let child_ep = fx.endpoint_of(child).await;
        fx.reconcile(child).await;
        let child_password = fx.password(child, &role).await;
        assert_ne!(child_password, parent_password);
        let parent_sealed = fx.rows(fx.main).await.remove(&role).unwrap();
        assert!(
            open_member_role_password(&fx.worker.key, child.as_uuid(), &role, &parent_sealed)
                .is_err(),
            "the parent's sealed password doesn't open for the child"
        );
        let refused = fx
            .direct(&child_ep, &role, &parent_password, "SELECT 1;")
            .await;
        assert!(
            refused
                .as_ref()
                .is_err_and(|e| e.contains("password authentication failed")),
            "{refused:?}"
        );
        assert_eq!(
            fx.direct(
                &child_ep,
                &role,
                &child_password,
                "SET ROLE billing;\nSELECT current_user;"
            )
            .await
            .unwrap(),
            "billing"
        );
        // The parent's own password still works there.
        assert!(
            fx.direct(&fx.ep, &role, &parent_password, "SELECT 1;")
                .await
                .is_ok()
        );
    })
    .await;
}

/// Review focus 2: a grant naming a role the branch doesn't have is skipped, not a failure.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack"]
async fn reconcile_skips_roles_missing_on_the_branch() {
    live(|fx| async move {
        let m = fx.members[0].id;
        fx.grant(m, PlatformRole::Developer, &["billing", "ghost"])
            .await;
        let roles = fx.reconcile(fx.main).await;
        let role = member_role_name(m);
        let mine: Vec<&MemberRoleApplied> = roles.iter().filter(|r| r.member_id == m).collect();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].applied_roles, ["billing"]);
        assert_eq!(
            fx.catalog(&fx.ep).await[&role],
            BTreeSet::from(["billing".to_string()])
        );
        // A grant whose every role is missing leaves no role at all.
        fx.grant(m, PlatformRole::Developer, &["ghost"]).await;
        fx.reconcile(fx.main).await;
        assert!(!fx.catalog(&fx.ep).await.contains_key(&role));
    })
    .await;
}

/// C1 (R16): an owner-level role is never given to a member role, even when its grant names
/// it, and a role that becomes owner-level later is taken away (sessions ended).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack"]
async fn owner_level_roles_are_never_given() {
    live(|fx| async move {
        assert_eq!(
            fx.worker
                .create_role(fx.main, &RoleSpec::login("app", true))
                .await,
            RoleOutcome::Ok
        );
        let m = fx.members[0].id;
        fx.grant(m, PlatformRole::Developer, &["app", "billing"])
            .await;
        let roles = fx.reconcile(fx.main).await;
        let role = member_role_name(m);
        let mine = roles.iter().find(|r| r.member_id == m).unwrap();
        assert_eq!(mine.applied_roles, ["billing"]);
        assert_set_role_refused(fx.as_member(m, "SET ROLE app;").await, "SET ROLE app");
        assert_set_role_refused(
            fx.as_member(m, "SET ROLE neon_superuser;").await,
            "SET ROLE neon_superuser",
        );
        assert_set_role_refused(
            fx.as_member(m, "SET ROLE billing;\nSET ROLE neon_superuser;")
                .await,
            "via billing",
        );
        // `billing` becomes owner-level through the API: the next reconcile takes it away.
        let session = {
            let fx = fx.clone();
            tokio::spawn(async move {
                fx.as_member(m, "SET ROLE billing;\nSELECT pg_sleep(60);")
                    .await
            })
        };
        wait_for_sessions(&fx, &role, 1).await;
        let patch = RolePatch {
            owner_level: Some(true),
            ..RolePatch::default()
        };
        assert_eq!(
            fx.worker.update_role(fx.main, "billing", &patch).await,
            RoleOutcome::Ok
        );
        fx.reconcile(fx.main).await;
        let ended = tokio::time::timeout(Duration::from_secs(10), session)
            .await
            .expect("ended")
            .unwrap();
        assert_terminated(ended);
        assert!(
            !fx.catalog(&fx.ep).await.contains_key(&role),
            "no safe role left"
        );
    })
    .await;
}

/// C2: a restore brings back an older catalog (a revoked membership, the old password). The
/// branch's rows are forgotten, so the reconcile rotates the password and revokes again.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack"]
async fn restore_brings_back_no_revoked_role() {
    live(|fx| async move {
        let m = fx.members[0].id;
        fx.grant(m, PlatformRole::Developer, &["billing", "hr"])
            .await;
        fx.reconcile(fx.main).await;
        let role = member_role_name(m);
        let before = fx.password(fx.main, &role).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let noted = chrono::Utc::now();
        tokio::time::sleep(Duration::from_secs(2)).await;
        fx.grant(m, PlatformRole::Developer, &["billing"]).await;
        fx.reconcile(fx.main).await;
        assert_set_role_refused(fx.as_member(m, "SET ROLE hr;").await, "hr revoked");

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
                backup_name: "members-before-restore-20261009T120000Z".into(),
                at: PointInTime::Timestamp(noted),
            })
            .await
            .expect("restore job");
        let main = fx.metadata.get_branch(fx.main).await.unwrap().unwrap();
        assert_eq!(main.status, BranchStatus::Ready, "{:?}", main.error);
        // M-d: the stack's own worker (its sweep, or the enqueue the restore asked for) may
        // reconcile the branch at any moment from here, so the checks below hold either way:
        // before any reconcile the rows are gone and the catalog holds `hr` again; after one,
        // the row is new (a fresh password).
        let rows = fx.rows(fx.main).await;
        assert!(
            rows.is_empty() || fx.password(fx.main, &role).await != before,
            "the rows are forgotten"
        );
        fx.reconcile(fx.main).await;
        assert_eq!(
            fx.catalog(&fx.ep).await[&role],
            BTreeSet::from(["billing".to_string()])
        );
        let after = fx.password(fx.main, &role).await;
        assert_ne!(after, before, "a fresh password");
        let refused = fx.direct(&fx.ep, &role, &before, "SELECT 1;").await;
        assert!(refused.is_err_and(|e| e.contains("password authentication failed")));
        assert_set_role_refused(fx.as_member(m, "SET ROLE hr;").await, "hr stays revoked");
        fx.assert_passwords_match_rows(fx.main, &fx.ep).await;
    })
    .await;
}

/// Invariant 7: a member role acting as `billing` can't get anywhere else, and each attempt
/// fails for the reason that proves it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack"]
async fn escape_attempts_fail() {
    live(|fx| async move {
        assert_eq!(
            fx.worker
                .create_role(fx.main, &RoleSpec::login("app", true))
                .await,
            RoleOutcome::Ok
        );
        let m = fx.members[0].id;
        fx.grant(m, PlatformRole::Developer, &["billing", "app"])
            .await;
        fx.reconcile(fx.main).await;
        let role = member_role_name(m);
        let password = fx.password(fx.main, &role).await;
        fx.sql_on(
            &fx.ep,
            &fx.database,
            "CREATE EXTENSION IF NOT EXISTS dblink;",
        )
        .await;
        assert_eq!(
            fx.as_member(m, "SET ROLE billing;\nSELECT current_user;")
                .await
                .unwrap(),
            "billing",
            "the allowed role works"
        );
        for (attempt, what) in [
            (
                "SET ROLE billing;\nRESET ROLE;\nSET ROLE postgres;",
                "RESET ROLE then the owner",
            ),
            (
                "SET ROLE billing;\nSELECT set_config('role', 'postgres', false);",
                "set_config",
            ),
            (
                "SET ROLE billing;\nSET ROLE app;",
                "an owner-level role its grant names",
            ),
            (
                "SET ROLE billing;\nSET ROLE neon_superuser;",
                "neon_superuser",
            ),
            ("SET ROLE sales;", "a role it isn't granted"),
        ] {
            assert_set_role_refused(fx.as_member(m, attempt).await, what);
        }
        // dblink as a non-superuser: no connection without a password the server asks for.
        for target in [
            format!("dbname={} user=postgres", fx.database),
            format!("dbname={}", fx.database),
        ] {
            let err = fx
                .as_member(
                    m,
                    &format!("SET ROLE billing;\nSELECT dblink_connect('escape', '{target}');"),
                )
                .await
                .expect_err("dblink without a password");
            assert!(
                (err.contains("2F003") || err.contains("08001")) && err.contains("password"),
                "{target}: {err}"
            );
        }
        // No proxy login for a member role (no `neon_roles` row); the same path takes a normal
        // login role, so the refusal is the member role's own.
        assert_eq!(
            fx.worker
                .create_role(fx.main, &RoleSpec::login("plain", false))
                .await,
            RoleOutcome::Ok
        );
        let sealed = fx
            .metadata
            .role_password(&fx.ep, "plain")
            .await
            .unwrap()
            .expect("plain's password");
        let plain =
            open_role_password(&fx.worker.key, &fx.main.to_string(), "plain", &sealed).unwrap();
        let conn = proxy_conn(&fx.ep, &fx.database);
        assert_eq!(
            psql(&conn, "plain", &plain, "SELECT current_user;")
                .await
                .unwrap(),
            "plain"
        );
        let proxied = psql(&conn, &role, &password, "SELECT 1;").await;
        assert!(
            proxied.as_ref().is_err_and(
                |e| e.contains(&format!("password authentication failed for user '{role}'"))
            ),
            "a cb_m_ login through the proxy must fail as an auth failure: {proxied:?}"
        );
        // R22: a LOGIN role is never given, by name or through a chain, so a member can't act
        // as one (and change its password to keep a login after removal).
        fx.as_owner("CREATE ROLE via_login NOLOGIN;").await;
        fx.sql("GRANT plain TO via_login;").await;
        fx.grant(
            m,
            PlatformRole::Developer,
            &["billing", "app", "plain", "via_login"],
        )
        .await;
        let roles = fx.reconcile(fx.main).await;
        let mine = roles.iter().find(|r| r.member_id == m).unwrap();
        assert_eq!(mine.applied_roles, ["billing"]);
        for attempt in [
            "SET ROLE plain;",
            "SET ROLE via_login;",
            "SET ROLE billing;\nSET ROLE plain;",
        ] {
            assert_set_role_refused(fx.as_member(m, attempt).await, attempt);
        }
        // N3: a member role can't change its own password: the receiver refuses the forwarded
        // change as a reserved name, and the transaction fails.
        let err = fx
            .as_member(m, "ALTER ROLE CURRENT_USER PASSWORD 'mine-0123456789';")
            .await
            .expect_err("a self password change is refused");
        assert!(
            err.contains(&format!(
                "the role name \"{role}\" is reserved by Chelabase"
            )),
            "{err}"
        );
        assert_eq!(
            fx.as_member(m, "SELECT 1;").await.unwrap(),
            "1",
            "its password is unchanged"
        );
    })
    .await;
}

/// R22: a member role is only ever made a member of NOLOGIN group roles; a LOGIN role named
/// by a grant is left out.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack"]
async fn login_roles_are_never_given() {
    live(|fx| async move {
        assert_eq!(
            fx.worker
                .create_role(fx.main, &RoleSpec::login("app_login", false))
                .await,
            RoleOutcome::Ok
        );
        let m = fx.members[0].id;
        fx.grant(m, PlatformRole::Developer, &["app_login", "billing"])
            .await;
        let roles = fx.reconcile(fx.main).await;
        let role = member_role_name(m);
        assert_eq!(
            roles
                .iter()
                .find(|r| r.member_id == m)
                .unwrap()
                .applied_roles,
            ["billing"]
        );
        assert_eq!(
            fx.catalog(&fx.ep).await[&role],
            BTreeSet::from(["billing".to_string()])
        );
        assert_set_role_refused(fx.as_member(m, "SET ROLE app_login;").await, "a LOGIN role");
        // The tenant admin's `All` leaves it out too.
        let admin = member_role_name(fx.admin.id);
        assert!(!fx.catalog(&fx.ep).await[&admin].contains("app_login"));
    })
    .await;
}

/// R22: a granted role whose chain reaches a LOGIN role is left out entirely, with what only
/// it reaches; once the role is NOLOGIN again, the granted role comes back.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack"]
async fn chain_reaching_a_login_role_excludes_the_granted_role() {
    live(|fx| async move {
        fx.as_owner("CREATE ROLE ops NOLOGIN;\nGRANT hr TO ops;")
            .await;
        fx.sql("ALTER ROLE hr LOGIN;").await;
        let m = fx.members[0].id;
        fx.grant(m, PlatformRole::Developer, &["ops", "billing"])
            .await;
        fx.reconcile(fx.main).await;
        let role = member_role_name(m);
        assert_eq!(
            fx.catalog(&fx.ep).await[&role],
            BTreeSet::from(["billing".to_string()])
        );
        for target in ["ops", "hr"] {
            assert_set_role_refused(
                fx.as_member(m, &format!("SET ROLE {target};")).await,
                target,
            );
        }
        fx.sql("ALTER ROLE hr NOLOGIN;").await;
        fx.reconcile(fx.main).await;
        assert_eq!(
            fx.catalog(&fx.ep).await[&role],
            BTreeSet::from(["billing".to_string(), "ops".to_string()])
        );
    })
    .await;
}

/// N2: a role that holds ADMIN on another (with no SET) could grant itself that role, so it
/// is never given, nor a role whose chain reaches it.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack"]
async fn admin_holders_are_never_given() {
    live(|fx| async move {
        fx.as_owner(
            "CREATE ROLE team_lead NOLOGIN;\n\
             GRANT hr TO team_lead WITH ADMIN OPTION, SET FALSE, INHERIT FALSE;\n\
             CREATE ROLE lead_of_leads NOLOGIN;\nGRANT team_lead TO lead_of_leads;",
        )
        .await;
        let m = fx.members[0].id;
        fx.grant(
            m,
            PlatformRole::Developer,
            &["team_lead", "lead_of_leads", "billing"],
        )
        .await;
        fx.reconcile(fx.main).await;
        let role = member_role_name(m);
        assert_eq!(
            fx.catalog(&fx.ep).await[&role],
            BTreeSet::from(["billing".to_string()])
        );
        assert_set_role_refused(fx.as_member(m, "SET ROLE team_lead;").await, "team_lead");
        assert_set_role_refused(
            fx.as_member(m, "SET ROLE lead_of_leads;").await,
            "lead_of_leads",
        );
    })
    .await;
}

/// M-c: a hook end to end, with forwarding on. The owner, through the proxy, makes a member's
/// granted role reach an ADMIN holder; the DDL receiver asks for a reconcile, and the stack's
/// worker revokes the role within a bounded time, with no reconcile called here.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack running this branch's worker"]
async fn an_owner_chain_made_in_sql_is_revoked_by_the_hook() {
    live(|fx| async move {
        let m = fx.members[0].id;
        fx.grant(m, PlatformRole::Developer, &["billing", "sales"])
            .await;
        fx.reconcile(fx.main).await;
        let role = member_role_name(m);
        assert_eq!(
            fx.catalog(&fx.ep).await[&role],
            BTreeSet::from(["billing".to_string(), "sales".to_string()])
        );
        let main = fx.metadata.get_branch(fx.main).await.unwrap().unwrap();
        let owner_password = chelabase_secrets::open_credential(
            &fx.worker.key,
            &fx.main.to_string(),
            chelabase_secrets::Purpose::Owner,
            main.owner_password_enc.as_deref().expect("owner password"),
        )
        .unwrap();
        let conn = proxy_conn(&fx.ep, &fx.database);
        psql(
            &conn,
            "postgres",
            &owner_password,
            "CREATE ROLE sneaky NOLOGIN;\n\
             GRANT hr TO sneaky WITH ADMIN OPTION, SET FALSE, INHERIT FALSE;\n\
             GRANT sneaky TO billing;",
        )
        .await
        .expect("the owner's SQL through the proxy");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let held = fx
                .catalog(&fx.ep)
                .await
                .get(&role)
                .cloned()
                .unwrap_or_default();
            if held == BTreeSet::from(["sales".to_string()]) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the hook didn't revoke billing: {held:?}"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        assert_set_role_refused(fx.as_member(m, "SET ROLE billing;").await, "billing");
    })
    .await;
}

/// R21: granting a role includes its chain. The member role holds only `billing_admin`, and
/// Postgres' own `SET` chaining lets it become `billing` too; nothing else.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack"]
async fn granted_role_brings_its_chain() {
    live(|fx| async move {
        fx.as_owner("CREATE ROLE billing_admin NOLOGIN;\nGRANT billing TO billing_admin;")
            .await;
        let m = fx.members[0].id;
        fx.grant(m, PlatformRole::Developer, &["billing_admin"])
            .await;
        let roles = fx.reconcile(fx.main).await;
        let role = member_role_name(m);
        let mine = roles.iter().find(|r| r.member_id == m).unwrap();
        assert_eq!(mine.applied_roles, ["billing_admin"]);
        assert_eq!(
            fx.catalog(&fx.ep).await[&role],
            BTreeSet::from(["billing_admin".to_string()]),
            "the member role is granted only the granted role"
        );
        let reached: BTreeSet<String> = fx.reach(&fx.ep).await[&role]
            .iter()
            .map(|(r, _)| r.clone())
            .collect();
        assert_eq!(
            reached,
            BTreeSet::from(["billing".to_string(), "billing_admin".to_string()])
        );
        assert_eq!(
            fx.as_member(m, "SET ROLE billing;\nSELECT current_user;")
                .await
                .unwrap(),
            "billing"
        );
        assert_eq!(
            fx.as_member(
                m,
                "SET ROLE billing_admin;\nSET ROLE billing;\nSELECT current_user;"
            )
            .await
            .unwrap(),
            "billing"
        );
        assert_set_role_refused(
            fx.as_member(m, "SET ROLE sales;").await,
            "outside the chain",
        );
    })
    .await;
}

/// R21 with R16: a granted role whose chain reaches an owner-level role is left out entirely,
/// with what it alone reaches; the member's other roles stay. Once the chain is safe again, the
/// role comes back.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack"]
async fn chain_reaching_owner_level_excludes_the_granted_role() {
    live(|fx| async move {
        fx.as_owner("CREATE ROLE ops NOLOGIN;\nGRANT hr TO ops;")
            .await;
        fx.sql("GRANT neon_superuser TO hr;").await;
        let m = fx.members[0].id;
        fx.grant(m, PlatformRole::Developer, &["ops", "billing"])
            .await;
        let roles = fx.reconcile(fx.main).await;
        let role = member_role_name(m);
        assert_eq!(
            roles
                .iter()
                .find(|r| r.member_id == m)
                .unwrap()
                .applied_roles,
            ["billing"]
        );
        assert_eq!(
            fx.catalog(&fx.ep).await[&role],
            BTreeSet::from(["billing".to_string()])
        );
        for target in ["ops", "hr", "neon_superuser"] {
            assert_set_role_refused(
                fx.as_member(m, &format!("SET ROLE {target};")).await,
                target,
            );
        }
        assert_eq!(
            fx.as_member(m, "SET ROLE billing;\nSELECT current_user;")
                .await
                .unwrap(),
            "billing"
        );
        // `hr` is plain again: `ops` comes back, with `hr` through its chain.
        fx.sql("REVOKE neon_superuser FROM hr;").await;
        fx.reconcile(fx.main).await;
        assert_eq!(
            fx.catalog(&fx.ep).await[&role],
            BTreeSet::from(["billing".to_string(), "ops".to_string()])
        );
        assert_eq!(
            fx.as_member(m, "SET ROLE hr;\nSELECT current_user;")
                .await
                .unwrap(),
            "hr"
        );
    })
    .await;
}

/// What still names `role` in the main database: a line per leftover (`pg_shdepend` in this
/// database or on shared objects, role settings), then the member role's `rolpassword`.
async fn leftovers_and_password(fx: &Fx, role: &str) -> (Vec<String>, String) {
    let out = fx
        .sql_on(
            &fx.ep,
            &fx.database,
            &format!(
                "SELECT 'shdepend ' || d.classid::regclass::text || ' ' || d.deptype::text FROM pg_shdepend d \
                   WHERE d.refclassid = 'pg_authid'::regclass AND d.refobjid = '{role}'::regrole \
                     AND d.dbid IN (0, (SELECT oid FROM pg_database WHERE datname = current_database())) \
                 UNION ALL SELECT 'setting ' || array_to_string(s.setconfig, ',') FROM pg_db_role_setting s \
                   WHERE s.setrole = '{role}'::regrole;"
            ),
        )
        .await;
    let left = out.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect();
    let password = fx
        .sql(&format!("SELECT rolpassword FROM pg_authid WHERE rolname = '{role}';"))
        .await
        .trim()
        .to_string();
    (left, password)
}

/// M-g, through the worker's hand-over itself (`hand_over_member_role`, the RPC's body): a
/// session as the member role leaves a table, a privilege and role settings (everywhere and in
/// this database), and can't set a password of its own; the hand-over gives the table to the
/// acted role, drops the rest, resets the settings and puts the row's password back (M-e).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack"]
async fn a_hand_over_through_the_rpc_holds_the_invariant_and_restores_the_password() {
    live(|fx| async move {
        let m = fx.members[0].id;
        let role = member_role_name(m);
        fx.grant(m, PlatformRole::Developer, &["billing"]).await;
        fx.reconcile(fx.main).await;
        fx.sql_on(
            &fx.ep,
            &fx.database,
            &format!("GRANT USAGE, CREATE ON SCHEMA public TO {role};"),
        )
        .await;
        // As the SQL editor does when the run starts (R31): a reconcile racing this test (the dev
        // stack's worker reconciles after the forwarded ALTER ROLEs) hands to billing too.
        assert!(
            fx.metadata
                .record_member_acted_role(m, fx.main.as_uuid(), "billing")
                .await
                .unwrap()
        );
        fx.as_member(
            m,
            &format!(
                "CREATE TABLE public.ho_left (id int);\n\
                 ALTER ROLE CURRENT_USER SET work_mem = '1MB';\n\
                 ALTER ROLE CURRENT_USER IN DATABASE {db} SET work_mem = '2MB';",
                db = fx.database
            ),
        )
        .await
        .expect("the member's session");
        // A password of its own is refused at once (the control plane owns member roles'
        // passwords, as the self-password test shows); the hand-over still writes the row's back.
        let err = fx
            .as_member(m, "ALTER ROLE CURRENT_USER PASSWORD 'changed-by-the-run';")
            .await
            .expect_err("a self password change is refused");
        assert!(err.contains("is reserved by Chelabase"), "{err}");
        let reply = fx
            .worker
            .hand_over_member_role(&MemberRoleHandOverRequest {
                branch_id: fx.main.as_uuid(),
                member_id: m,
                database: fx.database.clone(),
                acted_role: "billing".into(),
                backend_pid: None,
                backend_start: None,
            })
            .await;
        assert_eq!(reply, MemberRolesReply::Done { roles: vec![] });
        let owner = fx
            .sql_on(
                &fx.ep,
                &fx.database,
                "SELECT pg_get_userbyid(relowner) FROM pg_class WHERE relname = 'ho_left';",
            )
            .await;
        assert_eq!(owner.trim(), "billing");
        let (left, verifier) = leftovers_and_password(&fx, &role).await;
        assert!(left.is_empty(), "{left:?}");
        let password = fx.password(fx.main, &role).await;
        assert!(chelabase_neon::scram::verifies(&verifier, &password), "the row's password is back");
        assert_eq!(fx.as_member(m, "SELECT 1;").await.unwrap().trim(), "1");
    })
    .await;
}

/// M-g, the sweep backstop: a hand-over that never ran, and no change to the member's grant;
/// the next reconcile (as the sweep runs it) hands the leftover to the role the member last
/// acted as, drops the privilege granted to it, and a converged reconcile after that finds
/// nothing left to do.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack"]
async fn the_sweep_cleans_up_after_a_hand_over_that_never_ran() {
    live(|fx| async move {
        let m = fx.members[0].id;
        let role = member_role_name(m);
        fx.grant(m, PlatformRole::Developer, &["billing", "sales"]).await;
        fx.reconcile(fx.main).await;
        fx.sql_on(
            &fx.ep,
            &fx.database,
            &format!(
                "CREATE TABLE public.sweep_left (id int);\nALTER TABLE public.sweep_left OWNER TO {role};\n\
                 CREATE TABLE public.sweep_owned (id int);\nALTER TABLE public.sweep_owned OWNER TO postgres;\n\
                 GRANT SELECT ON public.sweep_owned TO {role};"
            ),
        )
        .await;
        assert!(
            fx.metadata
                .record_member_acted_role(m, fx.main.as_uuid(), "sales")
                .await
                .unwrap()
        );
        assert!(!leftovers_and_password(&fx, &role).await.0.is_empty());
        // Nothing changed in the grant: only the leftovers make this reconcile do anything.
        fx.reconcile(fx.main).await;
        let owner = fx
            .sql_on(
                &fx.ep,
                &fx.database,
                "SELECT pg_get_userbyid(relowner) FROM pg_class WHERE relname = 'sweep_left';",
            )
            .await;
        assert_eq!(owner.trim(), "sales");
        let (left, _) = leftovers_and_password(&fx, &role).await;
        assert!(left.is_empty(), "{left:?}");
        assert_eq!(
            fx.catalog(&fx.ep).await[&role],
            BTreeSet::from(["billing".to_string(), "sales".to_string()])
        );
    })
    .await;
}

/// R31 (user): when an act-as run's hand-over never happened, a reconcile does it. Here the
/// member last acted as `sales` and left a table it owns and a privilege granted to it; a
/// narrowing reconcile that revokes `sales` still hands the table to `sales` (the member role
/// held it before this reconcile), drops the privilege, and leaves it owning nothing.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack"]
async fn a_reconcile_hands_leftovers_to_the_last_acted_role_then_drops_the_rest() {
    live(|fx| async move {
        let m = fx.members[0].id;
        let role = member_role_name(m);
        fx.grant(m, PlatformRole::Developer, &["billing", "sales"]).await;
        fx.reconcile(fx.main).await;
        // What a run left behind, with no hand-over after it.
        fx.sql_on(
            &fx.ep,
            &fx.database,
            &format!(
                "CREATE TABLE public.r31_left (id int);\nALTER TABLE public.r31_left OWNER TO {role};\n\
                 CREATE TABLE public.r31_owned (id int);\nALTER TABLE public.r31_owned OWNER TO postgres;\n\
                 GRANT SELECT ON public.r31_owned TO {role};"
            ),
        )
        .await;
        assert!(
            fx.metadata
                .record_member_acted_role(m, fx.main.as_uuid(), "sales")
                .await
                .unwrap()
        );
        // Narrowing: sales is revoked by this reconcile.
        fx.grant(m, PlatformRole::Developer, &["billing"]).await;
        fx.reconcile(fx.main).await;
        assert_eq!(
            fx.catalog(&fx.ep).await[&role],
            BTreeSet::from(["billing".to_string()])
        );
        let check = format!(
            "SELECT pg_get_userbyid(relowner) FROM pg_class WHERE relname = 'r31_left';\n\
             SELECT count(*) FROM pg_class c, aclexplode(c.relacl) a \
               WHERE c.relname = 'r31_owned' AND a.grantee = '{role}'::regrole;\n\
             SELECT count(*) FROM pg_shdepend d JOIN pg_database db ON db.oid = d.dbid \
               WHERE db.datname = current_database() AND d.refobjid = '{role}'::regrole;"
        );
        let out = fx.sql_on(&fx.ep, &fx.database, &check).await;
        let lines: Vec<&str> = out.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, ["sales", "0", "0"], "{out}");
        // Every reconcile does it again: still nothing left, and a second one changes nothing.
        fx.reconcile(fx.main).await;
        let out = fx.sql_on(&fx.ep, &fx.database, &check).await;
        let lines: Vec<&str> = out.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        assert_eq!(lines, ["sales", "0", "0"], "{out}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack"]
async fn a_protected_branch_refuses_a_developers_schema_changes() {
    live(|fx| async move {
        let dev = fx.members[0].id;
        let boss = fx.members[1].id;
        // Setup as cloud_admin: the owner may not grant CREATE on `public`.
        fx.sql_on(&fx.ep, &fx.database, "GRANT CREATE ON SCHEMA public TO billing; CREATE TABLE public.guarded (id int); GRANT INSERT ON public.guarded TO billing;").await;
        fx.grant(dev, PlatformRole::Developer, &["billing"]).await;
        fx.grant(boss, PlatformRole::Admin, &["billing"]).await;
        fx.reconcile(fx.main).await;
        // Unprotected: the Developer may change the schema.
        fx.as_member(dev, "SET ROLE billing; CREATE TABLE public.g1 (id int);").await.unwrap();
        fx.metadata.set_branch_protected(fx.main, true).await.unwrap();
        fx.reconcile(fx.main).await;
        let refused = fx.as_member(dev, "SET ROLE billing; CREATE TABLE public.g2 (id int);").await.unwrap_err();
        assert!(refused.contains("42501") && refused.contains("branch is protected") && refused.contains("chelabase:branch_protected"), "{refused}");
        for ddl in ["DROP TABLE public.g1;", "COMMENT ON TABLE public.g1 IS 'x';", "GRANT SELECT ON public.g1 TO PUBLIC;", "ALTER TABLE public.g1 ADD COLUMN x int;"] {
            let out = fx.as_member(dev, &format!("SET ROLE billing; {ddl}")).await;
            assert!(out.as_ref().is_err_and(|e| e.contains("branch is protected")), "{ddl}: {out:?}");
        }
        // Rows and temporary objects stay normal Postgres.
        fx.as_member(dev, "SET ROLE billing; INSERT INTO public.guarded VALUES (1);").await.unwrap();
        fx.as_member(dev, "SET ROLE billing; CREATE TEMP TABLE scratch (id int); INSERT INTO scratch VALUES (1); DROP TABLE scratch;").await.unwrap();
        // An Admin isn't locked.
        fx.as_member(boss, "SET ROLE billing; CREATE TABLE public.g3 (id int);").await.unwrap();
        // Every database, including one made after the branch: guarded after a reconcile.
        fx.sql("CREATE DATABASE guard_two").await;
        fx.reconcile(fx.main).await;
        let triggers = fx.sql_on(&fx.ep, "guard_two", "SELECT count(*) FROM pg_event_trigger WHERE evtname LIKE 'chelabase_schema_guard%' AND evtenabled <> 'D'").await;
        assert_eq!(triggers.trim(), "2");
        // Unprotected again: the Developer may change the schema.
        fx.metadata.set_branch_protected(fx.main, false).await.unwrap();
        fx.reconcile(fx.main).await;
        fx.as_member(dev, "SET ROLE billing; CREATE TABLE public.g2 (id int);").await.unwrap();
        fx.sql("DROP DATABASE guard_two").await;
        fx.sql_on(&fx.ep, &fx.database, "DROP TABLE public.guarded, public.g1, public.g2, public.g3; REVOKE CREATE ON SCHEMA public FROM billing;").await;
    })
    .await;
}

// T7 review: a schema planted while the branch was unprotected can't block the guard (finding
// 2), the triggers fire always (6), data maintenance stays allowed (4), and a temporary table
// inheriting from a permanent one is refused (5).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack"]
async fn a_planted_guard_schema_cannot_block_the_guard() {
    live(|fx| async move {
        let dev = fx.members[0].id;
        let db = fx.database.clone();
        fx.sql_on(
            &fx.ep,
            &db,
            &format!(
                "GRANT CREATE ON DATABASE \"{db}\" TO billing; GRANT CREATE ON SCHEMA public TO billing; \
                 CREATE MATERIALIZED VIEW public.mv AS SELECT 1 AS x; GRANT MAINTAIN ON public.mv TO billing; \
                 CREATE TABLE public.parent_b (id int); ALTER TABLE public.parent_b OWNER TO billing;"
            ),
        )
        .await;
        fx.grant(dev, PlatformRole::Developer, &["billing"]).await;
        fx.reconcile(fx.main).await;
        // Unprotected: the Developer plants the guard's schema with a function of the wrong shape.
        fx.as_member(
            dev,
            "SET ROLE billing; CREATE SCHEMA chelabase_guard; \
             CREATE FUNCTION chelabase_guard.refuse_ddl() RETURNS int LANGUAGE sql AS 'SELECT 1'; \
             CREATE TABLE chelabase_guard.hidden (x int);",
        )
        .await
        .unwrap();
        fx.metadata.set_branch_protected(fx.main, true).await.unwrap();
        fx.reconcile(fx.main).await;
        let enabled = fx
            .sql_on(&fx.ep, &db, "SELECT string_agg(evtenabled::text, ',' ORDER BY evtname) FROM pg_event_trigger WHERE evtname LIKE 'chelabase_schema_guard%'")
            .await;
        assert_eq!(enabled.trim(), "A,A", "both triggers installed, ENABLE ALWAYS");
        let leftovers = fx
            .sql_on(&fx.ep, &db, "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'chelabase_guard'")
            .await;
        assert_eq!(leftovers.trim(), "0", "the planted table is gone");
        let refused = fx.as_member(dev, "SET ROLE billing; CREATE TABLE public.g9 (id int);").await.unwrap_err();
        assert!(refused.contains("branch is protected"), "{refused}");
        // Data maintenance stays allowed.
        fx.as_member(dev, "SET ROLE billing; REFRESH MATERIALIZED VIEW public.mv;").await.unwrap();
        // A temporary table inheriting from a permanent one is a schema change.
        let refused = fx.as_member(dev, "SET ROLE billing; CREATE TEMP TABLE tt () INHERITS (public.parent_b);").await.unwrap_err();
        assert!(refused.contains("branch is protected"), "{refused}");
        fx.metadata.set_branch_protected(fx.main, false).await.unwrap();
        fx.reconcile(fx.main).await;
        fx.sql_on(
            &fx.ep,
            &db,
            &format!(
                "DROP MATERIALIZED VIEW public.mv; DROP TABLE public.parent_b; \
                 REVOKE CREATE ON SCHEMA public FROM billing; REVOKE CREATE ON DATABASE \"{db}\" FROM billing;"
            ),
        )
        .await;
    })
    .await;
}
