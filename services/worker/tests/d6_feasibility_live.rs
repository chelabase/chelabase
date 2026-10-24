//! Members hub step 1, T1: the D6 feasibility probe ("run as the object's owner", spec Part 4
//! "After the red-team", plan P12). It establishes on the stack's PG17 compute, as assertions,
//! what Postgres allows the branch owner (`postgres`, logged in directly, not `cloud_admin`
//! under `SET ROLE`: a superuser session may `SET ROLE` anywhere, which would hide every
//! refusal) when it acts as another role. The report is
//! `docs/superpowers/notes/2026-10-23-members-hub-d6-feasibility.md`.
//!
//! ```text
//! just workbench test -p worker --test d6_feasibility_live -- --run-ignored only
//! ```
//!
//! The probe collects every observation, prints them all, then fails with every mismatch at
//! once, so one run shows the whole picture.

use std::collections::BTreeSet;
use std::fmt::Display;
use std::future::Future;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use chelabase_metadata::members::{Admission, GrantRow, MemberRecord};
use chelabase_metadata::{DatabaseStatus, MetadataClient};
use chelabase_nats::MemberRolesReply;
use chelabase_neon::{ComputeBackend, ComputeLocator};
use chelabase_secrets::open_member_role_password;
use chelabase_types::access::PlatformRole;
use chelabase_types::role_name::is_marked_sql;
use chelabase_types::{BranchId, DatabaseId, TenantId};
use uuid::Uuid;
use worker::config::RealWorker;
use worker::member_roles::{
    member_catalog_sql, member_role_name, parse_member_catalog, safe_roles, unsafe_node,
};
use worker::roles::{RoleOutcome, RoleSpec};

mod common;
use common::{cleanup, setup};

/// One provisioned database in its own tenant and project, with a tenant admin and one
/// member, and the owner's password.
struct Fx {
    worker: RealWorker,
    metadata: MetadataClient,
    project: Uuid,
    id: DatabaseId,
    main: BranchId,
    ep: String,
    database: String,
    admin: MemberRecord,
    member: MemberRecord,
    owner_password: String,
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
        .bind(format!("live-d6-{}", tenant.as_uuid().simple()))
        .execute(metadata.pool())
        .await
        .expect("insert the tenant");
    let project = metadata
        .create_project(tenant, "d6 probe")
        .await
        .expect("create the project");
    let id = DatabaseId::new();
    let name = format!("d6-{}", &id.to_string()[..8]);
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
                .ensure_member(
                    tenant,
                    &format!("z-{n}-{}", Uuid::new_v4()),
                    &format!("{n}@d6.live"),
                    &n,
                    Admission::Active,
                )
                .await
                .expect("a member")
        }
    };
    let admin = member("admin").await;
    assert!(admin.tenant_admin, "the first member claims tenant admin");
    let member = member("dev").await;
    let owner_password = chelabase_secrets::open_credential(
        &worker.key,
        &main.id.to_string(),
        chelabase_secrets::Purpose::Owner,
        main.owner_password_enc.as_deref().expect("owner password"),
    )
    .expect("open the owner password");
    let fx = Arc::new(Fx {
        ep: main.endpoint_id.clone().expect("endpoint"),
        database: record.database_name.clone().expect("database name"),
        main: main.id,
        worker,
        metadata,
        project: project.id.as_uuid(),
        id,
        admin,
        member,
        owner_password,
    });
    let outcome = tokio::spawn(body(fx.clone())).await;
    let _ = fx.metadata.set_branch_protected(fx.main, false).await;
    cleanup(&fx.worker, &fx.metadata, fx.id).await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

/// psql's stdout and stderr (warnings and notices, `VERBOSITY=verbose`: with their SQLSTATE)
/// on success; stderr on failure (stops at the first error).
#[derive(Debug, Clone)]
struct Out {
    stdout: String,
    stderr: String,
}

async fn psql(conn: &str, user: &str, password: &str, script: &str) -> Result<Out, String> {
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
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if out.status.success() {
        Ok(Out {
            stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
            stderr,
        })
    } else {
        Err(stderr)
    }
}

impl Fx {
    /// SQL as `cloud_admin` in the branch's database (forwarding off).
    async fn cloud(&self, sql: &str) -> String {
        self.worker
            .computes
            .admin_sql(&self.ep, &self.database, sql)
            .await
            .unwrap_or_else(|e| panic!("admin_sql {sql:?}: {e:#}"))
            .trim()
            .to_string()
    }

    /// psql straight to the compute as `user` (the orchestrator's owner and member-role path).
    async fn direct(&self, user: &str, password: &str, script: &str) -> Result<Out, String> {
        let control = std::env::var("CHELABASE_COMPUTE_CONTROL_NETWORK")
            .expect("CHELABASE_COMPUTE_CONTROL_NETWORK");
        let stack = std::env::var("CHELABASE_STACK").unwrap_or_else(|_| "main".into());
        let addr = chelabase_neon::DockerLocator::connect(&control, &stack)
            .unwrap()
            .postgres_addr(&self.ep)
            .await
            .expect("the compute's control address")
            .expect("on the control network");
        let tls = std::env::var("CHELABASE_COMPUTE_TLS_DIR").expect("CHELABASE_COMPUTE_TLS_DIR");
        let conn = format!(
            "host={} hostaddr={} port={} dbname={} sslmode=verify-full channel_binding=require \
             sslrootcert={tls}/current/ca.crt sslcert={tls}/current/client.crt sslkey={tls}/current/client.key",
            chelabase_neon::compute_tls::server_name(&self.ep),
            addr.ip(),
            addr.port(),
            self.database,
        );
        psql(&conn, user, password, script).await
    }

    /// As the branch owner, logged in as `postgres` (session_user is the owner).
    async fn owner(&self, script: &str) -> Result<Out, String> {
        self.direct(worker::OWNER_ROLE, &self.owner_password, script)
            .await
    }

    /// As the member's `cb_m_*` role on main.
    async fn as_member(&self, script: &str) -> Result<Out, String> {
        let role = member_role_name(self.member.id);
        let sealed = self
            .metadata
            .member_branch_roles(self.main.as_uuid())
            .await
            .unwrap()
            .into_iter()
            .find(|r| r.role_name == role)
            .expect("the member role's row")
            .password_enc;
        let password =
            open_member_role_password(&self.worker.key, self.main.as_uuid(), &role, &sealed)
                .expect("open the member role's password");
        self.direct(&role, &password, script).await
    }

    /// The owner's memberships in `role`, `grantor:admin/inherit/set` ordered by grantor
    /// (`boot` = the bootstrap superuser, OID 10); empty when none.
    async fn memberships(&self, role: &str) -> String {
        self.cloud(&memberships_sql(role)).await
    }

    async fn reconcile(&self) {
        match self.worker.reconcile_member_roles(self.main, None).await {
            MemberRolesReply::Done { .. } => {}
            MemberRolesReply::Failed { message } => panic!("reconcile failed: {message}"),
        }
    }

    /// `role`'s entries on relation `rel`: `grantee|privilege|grantor`, sorted.
    async fn acl(&self, rel: &str, grantee: &str) -> String {
        self.cloud(&format!(
            "SELECT coalesce(string_agg(g.rolname || '|' || e.privilege_type || '|' || gr.rolname, ',' \
             ORDER BY e.privilege_type, gr.rolname), '') FROM pg_class c, aclexplode(c.relacl) e \
             JOIN pg_roles g ON g.oid = e.grantee JOIN pg_roles gr ON gr.oid = e.grantor \
             WHERE c.oid = '{rel}'::regclass AND g.rolname = '{grantee}';"
        ))
        .await
    }
}

/// The owner's rows on a role it holds ADMIN on only (the bootstrap row: `grantor:admin/inherit/set`).
const ADMIN_ONLY: &str = "boot:true/false/false";
/// ... plus the row `createrole_self_grant = 'set, inherit'` adds when the owner creates it.
const SELF: &str = "boot:true/false/false,postgres:false/true/true";
/// ... plus a row granted by the owner with every option off.
const EMPTY_ROW: &str = "boot:true/false/false,postgres:false/false/false";

fn memberships_sql(role: &str) -> String {
    format!(
        "SELECT coalesce(string_agg(CASE WHEN a.grantor = 10 THEN 'boot' ELSE gr.rolname END || ':' \
         || a.admin_option::text || '/' || a.inherit_option::text || '/' || a.set_option::text, ',' \
         ORDER BY a.grantor), '') FROM pg_auth_members a JOIN pg_roles gr ON gr.oid = a.grantor \
         WHERE a.roleid = '{role}'::regrole AND a.member = 'postgres'::regrole;"
    )
}

/// P12 (a) as the plan words it: transient SET, the body as `role`, the option revoked.
fn plan_a(role: &str, body: &str, end: &str) -> String {
    format!(
        "BEGIN;\nGRANT {role} TO postgres WITH INHERIT FALSE, SET TRUE;\nSET LOCAL ROLE {role};\n\
         {body}\nRESET ROLE;\nREVOKE SET OPTION FOR {role} FROM postgres;\n{end}"
    )
}

/// P12 (a) with the whole transient membership revoked (`REVOKE role FROM postgres`: the row
/// granted by the owner itself).
fn full_a(role: &str, body: &str, end: &str) -> String {
    format!(
        "BEGIN;\nGRANT {role} TO postgres WITH INHERIT FALSE, SET TRUE;\nSET LOCAL ROLE {role};\n\
         {body}\nRESET ROLE;\nREVOKE {role} FROM postgres;\n{end}"
    )
}

/// Observations and mismatches; [`Probe::finish`] prints them all and fails on any mismatch.
#[derive(Default)]
struct Probe {
    notes: Vec<String>,
    misses: Vec<String>,
}

impl Probe {
    fn see(&mut self, item: &str, what: &str, observed: impl Display) {
        let line = format!("     [{item}] {what}: {observed}");
        println!("{line}");
        self.notes.push(line);
    }

    fn check(&mut self, item: &str, what: &str, ok: bool, observed: impl Display) {
        let line = format!("[{item}] {what}: {observed}");
        if !ok {
            self.misses.push(line.clone());
        }
        let line = format!("{} {line}", if ok { "ok  " } else { "MISS" });
        println!("{line}");
        self.notes.push(line);
    }

    fn finish(self) {
        println!("---- D6 probe observations ----");
        for n in &self.notes {
            println!("{n}");
        }
        assert!(
            self.misses.is_empty(),
            "D6 probe mismatches:\n{}",
            self.misses.join("\n")
        );
    }
}

fn show(r: &Result<Out, String>) -> String {
    match r {
        Ok(o) => format!("ok stdout={:?} stderr={:?}", o.stdout, o.stderr),
        Err(e) => format!("ERR {e:?}"),
    }
}

fn refused(r: &Result<Out, String>, needle: &str) -> bool {
    r.as_ref().is_err_and(|e| e.contains(needle))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the workbench stack"]
async fn d6_feasibility() {
    live(|fx| async move {
        let mut p = Probe::default();
        p.see("0", "server_version", fx.cloud("SHOW server_version;").await);
        let num = fx.cloud("SHOW server_version_num;").await;
        p.check("0", "server_version_num >= 170000", num.parse::<i32>().unwrap_or(0) >= 170000, &num);
        p.see("0", "owner createrole_self_grant", show(&fx.owner("SHOW createrole_self_grant;").await));
        p.see("0", "owner rolinherit", fx.cloud("SELECT rolinherit FROM pg_roles WHERE rolname = 'postgres';").await);

        // ---- Setup: an API-made role (the worker's create_role path) owning a table.
        for name in ["d6_api", "d6_g"] {
            let spec = RoleSpec { login: false, ..RoleSpec::login(name, false) };
            assert_eq!(fx.worker.create_role(fx.main, &spec).await, RoleOutcome::Ok, "{name}");
        }
        fx.cloud(
            "CREATE ROLE d6_x NOLOGIN; \
             CREATE TABLE public.d6_t (id int); ALTER TABLE public.d6_t OWNER TO d6_api; \
             CREATE TABLE public.d6_t2 (id int); ALTER TABLE public.d6_t2 OWNER TO d6_api;",
        )
        .await;

        // ---- 1. P12 (a) and (b) on an API-made role.
        // The owner's `createrole_self_grant = 'set, inherit'` gives it a second row, granted by
        // itself, on every role it creates (the API's `SET ROLE postgres; CREATE ROLE` too).
        let before = fx.memberships("d6_api").await;
        p.check("1", "API role: ADMIN row plus the createrole_self_grant row", before == SELF, &before);
        let direct = fx.owner("SET ROLE d6_api; SELECT current_user;").await;
        p.check("1", "so the owner may SET ROLE an API role directly", direct.as_ref().is_ok_and(|o| o.stdout == "d6_api"), show(&direct));

        // 1a, as the plan words it (REVOKE SET OPTION), over that row, with item 2's GRANT.
        let body = format!(
            "SELECT 'cu=' || current_user || ' su=' || session_user;\n\
             GRANT SELECT ON public.d6_t TO d6_x;\n\
             SELECT 'mid=' || ({});",
            memberships_sql("d6_api").trim_end_matches(';')
        );
        let a = fx.owner(&plan_a("d6_api", &body, "COMMIT;")).await;
        p.check("1a", "plan form: GRANT self SET, SET LOCAL ROLE, REVOKE SET OPTION in one txn", a.is_ok(), show(&a));
        if let Ok(o) = &a {
            p.check("1a", "current_user under SET LOCAL ROLE", o.stdout.contains("cu=d6_api su=postgres"), &o.stdout);
            p.check("1a", "mid-transaction memberships", o.stdout.contains("mid=boot:true/false/false,postgres:false/false/true"), &o.stdout);
        }
        let after_a = fx.memberships("d6_api").await;
        p.check("1a", "plan form over the createrole_self_grant row strips its INHERIT and SET", after_a == EMPTY_ROW, &after_a);
        let leftover = fx.owner("SET ROLE d6_api;").await;
        p.check("1a", "the leftover row gives no SET", refused(&leftover, "permission denied to set role"), show(&leftover));
        // The leftover counts as membership for pg_has_role(MEMBER) only.
        p.see("1a", "pg_has_role(postgres, d6_api, MEMBER/USAGE/SET) after plan form", fx.cloud(
            "SELECT pg_has_role('postgres', 'd6_api', 'MEMBER')::text || '/' || pg_has_role('postgres', 'd6_api', 'USAGE')::text || '/' || pg_has_role('postgres', 'd6_api', 'SET')::text;"
        ).await);
        let gone = fx.owner("REVOKE d6_api FROM postgres;").await;
        p.check("1a", "the owner removes its leftover row with REVOKE role FROM postgres", gone.is_ok(), show(&gone));
        let after_cleanup = fx.memberships("d6_api").await;
        p.check("1a", "after the full revoke only the ADMIN row stays", after_cleanup == ADMIN_ONLY, &after_cleanup);
        let plain = fx.owner("SET ROLE d6_api; SELECT current_user;").await;
        p.check("1a", "ADMIN only: SET ROLE refused", refused(&plain, "42501") && refused(&plain, "permission denied to set role"), show(&plain));

        // The plan form from ADMIN only: it works, and leaves a row with every option off.
        let a2 = fx.owner(&plan_a("d6_api", "GRANT SELECT ON public.d6_t2 TO d6_x;", "COMMIT;")).await;
        p.check("1a", "plan form from ADMIN only", a2.is_ok(), show(&a2));
        let after_a2 = fx.memberships("d6_api").await;
        p.check("1a", "plan form from ADMIN only leaves an empty row", after_a2 == EMPTY_ROW, &after_a2);
        fx.owner("REVOKE d6_api FROM postgres;").await.expect("drop the empty row");

        // The full form (REVOKE role FROM postgres) from ADMIN only leaves nothing.
        let b = fx.owner(&full_a("d6_api", "REVOKE SELECT ON public.d6_t2 FROM d6_x;", "COMMIT;")).await;
        p.check("1a", "full form (REVOKE role FROM postgres) in one txn", b.is_ok(), show(&b));
        let after_b = fx.memberships("d6_api").await;
        p.check("1a", "full form from ADMIN only leaves nothing", after_b == ADMIN_ONLY, &after_b);

        // 1c: the owner already holds a row it granted itself (INHERIT TRUE, SET FALSE).
        let own = fx.owner("GRANT d6_api TO postgres WITH INHERIT TRUE, SET FALSE;").await;
        p.check("1a", "owner self-grants INHERIT TRUE SET FALSE", own.is_ok(), show(&own));
        let pre = fx.memberships("d6_api").await;
        p.check("1a", "existing self row", pre == "boot:true/false/false,postgres:false/true/false", &pre);
        let c = fx.owner(&plan_a("d6_api", "SELECT 1;", "COMMIT;")).await;
        let after_c = fx.memberships("d6_api").await;
        p.check("1a", "plan form over an existing self row: ok", c.is_ok(), show(&c));
        p.check("1a", "plan form over an existing self row turns its INHERIT off", after_c == EMPTY_ROW, &after_c);
        fx.owner("GRANT d6_api TO postgres WITH INHERIT TRUE;").await.expect("restore inherit");
        let d = fx
            .owner("BEGIN;\nGRANT d6_api TO postgres WITH SET TRUE;\nSET LOCAL ROLE d6_api;\nSELECT 1;\nRESET ROLE;\nREVOKE SET OPTION FOR d6_api FROM postgres;\nCOMMIT;")
            .await;
        let after_d = fx.memberships("d6_api").await;
        p.check("1a", "SET-only grant over an existing self row: ok", d.is_ok(), show(&d));
        p.check("1a", "SET-only grant + REVOKE SET OPTION restores the existing row", after_d == pre, &after_d);
        fx.owner("REVOKE d6_api FROM postgres;").await.expect("drop the self row");
        let restored = fx.memberships("d6_api").await;
        p.check("1a", "back to the ADMIN row only", restored == ADMIN_ONLY, &restored);

        // 1d: a failing statement rolls the transient grant back with everything else.
        let fail = fx.owner(&full_a("d6_api", "SELECT 1/0;", "COMMIT;")).await;
        let after_fail = fx.memberships("d6_api").await;
        p.check("1a", "an error inside the txn leaves nothing", fail.is_err() && after_fail == ADMIN_ONLY, format!("{} / {after_fail}", show(&fail)));

        // 1 (b): cloud_admin grants SET persistently on the bootstrap row.
        fx.cloud("GRANT d6_api TO postgres WITH ADMIN OPTION, INHERIT FALSE, SET TRUE;").await;
        let pb = fx.memberships("d6_api").await;
        p.check("1b", "cloud_admin's persistent grant updates the bootstrap row", pb == "boot:true/false/true", &pb);
        let set = fx.owner("SET ROLE d6_api; SELECT current_user;").await;
        p.check("1b", "the owner may SET ROLE after (b)", set.as_ref().is_ok_and(|o| o.stdout == "d6_api"), show(&set));
        fx.cloud("REVOKE SET OPTION FOR d6_api FROM postgres;").await;
        let pb_back = fx.memberships("d6_api").await;
        p.check("1b", "cloud_admin's REVOKE SET OPTION restores it", pb_back == ADMIN_ONLY, &pb_back);

        // ---- 2. Grants and policies under SET ROLE r; the grantor recorded.
        let acl = fx.acl("public.d6_t", "d6_x").await;
        p.check("2", "GRANT SELECT under SET ROLE d6_api: grantor", acl == "d6_x|SELECT|d6_api", &acl);
        let pol = fx
            .owner(&full_a(
                "d6_api",
                "ALTER TABLE public.d6_t ENABLE ROW LEVEL SECURITY;\n\
                 CREATE POLICY d6_x_all_rows ON public.d6_t FOR ALL TO d6_x USING (true) WITH CHECK (true);\n\
                 COMMENT ON POLICY d6_x_all_rows ON public.d6_t IS 'chelabase:all_rows';",
                "COMMIT;",
            ))
            .await;
        p.check("2", "ENABLE RLS, CREATE POLICY, COMMENT ON POLICY as d6_api", pol.is_ok(), show(&pol));
        let pol_row = fx
            .cloud("SELECT p.polname || '|' || coalesce(obj_description(p.oid, 'pg_policy'), '') FROM pg_policy p WHERE p.polrelid = 'public.d6_t'::regclass;")
            .await;
        p.check("2", "policy and its comment", pol_row == "d6_x_all_rows|chelabase:all_rows", &pol_row);
        let pol_owner = fx.owner("CREATE POLICY d6_owner_try ON public.d6_t FOR SELECT TO d6_x USING (true);").await;
        p.check("2", "the owner alone can't CREATE POLICY on d6_api's table", refused(&pol_owner, "42501"), show(&pol_owner));
        let drop = fx.owner(&full_a("d6_api", "DROP POLICY d6_x_all_rows ON public.d6_t;", "COMMIT;")).await;
        p.check("2", "DROP POLICY as d6_api", drop.is_ok(), show(&drop));
        let rev = fx.owner(&full_a("d6_api", "REVOKE SELECT ON public.d6_t FROM d6_x;", "COMMIT;")).await;
        let acl_rev = fx.acl("public.d6_t", "d6_x").await;
        p.check("2", "REVOKE as d6_api removes d6_api's grant", rev.is_ok() && acl_rev.is_empty(), format!("{} / {acl_rev:?}", show(&rev)));

        // ---- 4. ALTER DEFAULT PRIVILEGES FOR ROLE r.
        let adp = "ALTER DEFAULT PRIVILEGES FOR ROLE d6_api IN SCHEMA public GRANT SELECT ON TABLES TO d6_x;";
        let none = fx.owner(adp).await;
        p.check("4", "FOR ROLE with ADMIN only: refused", refused(&none, "permission denied to change default privileges"), show(&none));
        let set_only = fx
            .owner(&format!("BEGIN;\nGRANT d6_api TO postgres WITH INHERIT FALSE, SET TRUE;\n{adp}\nROLLBACK;"))
            .await;
        p.check("4", "FOR ROLE with SET (no INHERIT), as the owner: refused", refused(&set_only, "permission denied to change default privileges"), show(&set_only));
        let inherit_only = fx
            .owner(&format!("BEGIN;\nGRANT d6_api TO postgres WITH INHERIT TRUE, SET FALSE;\n{adp}\nROLLBACK;"))
            .await;
        p.check("4", "FOR ROLE with INHERIT (no SET), as the owner: allowed", inherit_only.is_ok(), show(&inherit_only));
        let as_role = fx.owner(&full_a("d6_api", adp, "COMMIT;")).await;
        p.check("4", "FOR ROLE r under SET LOCAL ROLE r (SET only): allowed", as_role.is_ok(), show(&as_role));
        let defacl = fx
            .cloud("SELECT pg_get_userbyid(d.defaclrole) || '|' || d.defaclobjtype::text || '|' || array_to_string(d.defaclacl, ',') FROM pg_default_acl d WHERE d.defaclrole = 'd6_api'::regrole;")
            .await;
        p.check("4", "the default ACL recorded (grantor d6_api)", defacl == "d6_api|r|d6_x=r/d6_api", &defacl);
        let after_adp = fx.memberships("d6_api").await;
        p.check("4", "nothing left after", after_adp == ADMIN_ONLY, &after_adp);

        // ---- 5. The member-role safety walk with the owner a member of d6_g.
        let walk = |label: &'static str| {
            let fx = fx.clone();
            async move {
                let out = fx
                    .worker
                    .computes
                    .admin_sql(&fx.ep, &fx.database, &member_catalog_sql())
                    .await
                    .expect("member catalog");
                let catalog = parse_member_catalog(&out).expect("parse the member catalog");
                let safe = safe_roles(&BTreeSet::from(["d6_g".to_string(), "d6_api".to_string()]), &catalog);
                (label, safe, unsafe_node(&catalog, "d6_g"))
            }
        };
        let g_rows = fx.memberships("d6_g").await;
        let (_, safe_self, unsafe_self) = walk("self").await;
        p.check("5", "createrole_self_grant (INHERIT+SET) on d6_g: both stay safe", g_rows == SELF && safe_self.len() == 2 && !unsafe_self, format!("{g_rows} {safe_self:?} unsafe={unsafe_self}"));
        fx.owner("REVOKE d6_g FROM postgres;").await.expect("d6_g to ADMIN only");
        let a5 = fx.owner(&full_a("d6_g", "SELECT 1;", "COMMIT;")).await;
        let g_after = fx.memberships("d6_g").await;
        p.check("5", "(a) full form on d6_g leaves ADMIN only", g_after == ADMIN_ONLY, &g_after);
        let (_, safe_a, unsafe_a) = walk("a").await;
        p.check("5", "(a) transient on d6_g: ok", a5.is_ok(), show(&a5));
        p.check("5", "(a) d6_g and d6_api stay safe", safe_a.len() == 2 && !unsafe_a, format!("{safe_a:?} unsafe={unsafe_a}"));
        fx.cloud("GRANT d6_g TO postgres WITH ADMIN OPTION, INHERIT FALSE, SET TRUE;").await;
        let (_, safe_b, unsafe_b) = walk("b").await;
        p.check("5", "(b) persistent SET on d6_g: d6_g and d6_api stay safe", safe_b.len() == 2 && !unsafe_b, format!("{safe_b:?} unsafe={unsafe_b}"));

        // ---- 3. The protected-branch guard (with (b) still on d6_g: item 5 end to end).
        fx.metadata
            .put_project_grant(
                fx.admin.id,
                fx.member.id,
                fx.project,
                &GrantRow {
                    platform_role: PlatformRole::Developer,
                    pg_roles: vec!["d6_api".into(), "d6_g".into()],
                },
            )
            .await
            .expect("put the grant");
        fx.reconcile().await;
        let role = member_role_name(fx.member.id);
        let held = fx
            .cloud(&format!(
                "SELECT coalesce(string_agg(g.rolname, ',' ORDER BY g.rolname), '') FROM pg_auth_members a \
                 JOIN pg_roles g ON g.oid = a.roleid WHERE a.member = '{role}'::regrole;"
            ))
            .await;
        p.check("5", "(b) the reconcile still grants d6_g (and d6_api) to the Developer", held == "d6_api,d6_g", &held);
        fx.cloud("REVOKE SET OPTION FOR d6_g FROM postgres;").await;
        fx.metadata.set_branch_protected(fx.main, true).await.unwrap();
        fx.reconcile().await;
        let locked = fx
            .cloud(&format!("SELECT pg_has_role('{role}', 'chelabase_schema_locked', 'MEMBER');"))
            .await;
        p.check("3", "the Developer's member role is locked", locked == "t", &locked);
        let owner_pb = fx
            .owner(&full_a(
                "d6_api",
                "GRANT SELECT ON public.d6_t TO d6_x;\n\
                 CREATE POLICY d6_pb ON public.d6_t FOR SELECT TO d6_x USING (true);\n\
                 COMMENT ON POLICY d6_pb ON public.d6_t IS 'chelabase:all_rows';\n\
                 ALTER DEFAULT PRIVILEGES FOR ROLE d6_api IN SCHEMA public GRANT INSERT ON TABLES TO d6_x;",
                "COMMIT;",
            ))
            .await;
        p.check("3", "owner run (transient SET, GRANT, policy, default privileges) on a protected branch", owner_pb.is_ok(), show(&owner_pb));
        let member_pb = fx.as_member("SET ROLE d6_api; GRANT SELECT ON public.d6_t2 TO d6_x;").await;
        p.check("3", "a locked member role under SET ROLE d6_api: GRANT refused", refused(&member_pb, "branch is protected") && refused(&member_pb, "42501"), show(&member_pb));
        let member_pol = fx.as_member("SET ROLE d6_api; CREATE POLICY d6_m ON public.d6_t FOR SELECT TO d6_x USING (true);").await;
        p.check("3", "a locked member role under SET ROLE d6_api: CREATE POLICY refused", refused(&member_pol, "branch is protected"), show(&member_pol));
        let member_adp = fx.as_member("SET ROLE d6_api; ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT UPDATE ON TABLES TO d6_x;").await;
        p.check("3", "a locked member role under SET ROLE d6_api: ALTER DEFAULT PRIVILEGES refused", refused(&member_adp, "branch is protected"), show(&member_adp));
        fx.metadata.set_branch_protected(fx.main, false).await.unwrap();
        fx.reconcile().await;

        // ---- 6. Roles made in SQL: who holds ADMIN, the plan's predicate vs what works.
        let own = fx.owner("CREATE ROLE d6_own NOLOGIN;").await;
        p.check("6", "the owner creates a role in SQL", own.is_ok(), show(&own));
        fx.cloud(
            "CREATE ROLE d6_cr NOLOGIN CREATEROLE; SET ROLE d6_cr; CREATE ROLE d6_by_cr NOLOGIN; RESET ROLE; \
             CREATE ROLE d6_by_cloud NOLOGIN; \
             CREATE ROLE d6_ind NOLOGIN; CREATE ROLE d6_adm NOLOGIN; \
             GRANT d6_ind TO d6_adm WITH ADMIN OPTION; GRANT d6_adm TO postgres WITH INHERIT TRUE, SET FALSE;",
        )
        .await;
        for (role, rows, settable) in [
            ("d6_api", ADMIN_ONLY, true),
            ("d6_own", SELF, true),
            ("d6_by_cr", "", false),
            ("d6_by_cloud", "", false),
            ("d6_ind", "", true),
        ] {
            let m = fx.memberships(role).await;
            p.check("6", &format!("{role}: owner's direct memberships"), m == rows, &m);
            let plan_pred = fx
                .owner(&format!(
                    "SELECT EXISTS (SELECT 1 FROM pg_auth_members m WHERE m.roleid = '{role}'::regrole \
                     AND m.member = (SELECT oid FROM pg_roles WHERE rolname = current_user) \
                     AND (m.admin_option OR m.set_option));"
                ))
                .await
                .map(|o| o.stdout)
                .unwrap_or_else(|e| e);
            let has_pred = fx
                .owner(&format!(
                    "SELECT pg_has_role(current_user, '{role}', 'SET') OR pg_has_role(current_user, '{role}', 'MEMBER WITH ADMIN OPTION');"
                ))
                .await
                .map(|o| o.stdout)
                .unwrap_or_else(|e| e);
            let works = fx.owner(&full_a(role, "SELECT current_user;", "ROLLBACK;")).await;
            p.check("6", &format!("{role}: transient SET works"), works.is_ok() == settable, show(&works));
            p.check("6", &format!("{role}: plan predicate (direct ADMIN or SET row)"), (plan_pred == "t") == !rows.is_empty(), &plan_pred);
            p.check("6", &format!("{role}: pg_has_role SET or MEMBER WITH ADMIN OPTION matches what works"), (has_pred == "t") == settable, &has_pred);
        }

        // ---- 7. The owner reads pg_db_role_setting (the marker predicate).
        fx.cloud("CREATE ROLE d6_marked NOLOGIN; ALTER ROLE d6_marked SET chelabase.managed_role = 'personal';").await;
        let marked = fx.owner(&is_marked_sql("d6_marked")).await;
        p.check("7", "the owner evaluates is_marked_sql", marked.as_ref().is_ok_and(|o| o.stdout == "t"), show(&marked));
        let unmarked = fx.owner(&is_marked_sql("d6_api")).await;
        p.check("7", "and sees an unmarked role as such", unmarked.as_ref().is_ok_and(|o| o.stdout == "f"), show(&unmarked));

        // ---- 8. Who the owner's own grants are recorded as, and warnings for partial ones.
        // Every table gets `neon_superuser` ALL WITH GRANT OPTION (granted by its owner), and the
        // owner inherits `neon_superuser`: without SET ROLE its grants are recorded as made by
        // `neon_superuser`, and its revokes reach only those.
        let rel_acl = |rel: &'static str| {
            let fx = fx.clone();
            async move {
                fx.cloud(&format!(
                    "SELECT array_to_string(relacl, ',') FROM pg_class WHERE oid = '{rel}'::regclass;"
                ))
                .await
            }
        };
        let t_acl = rel_acl("public.d6_t").await;
        p.check("8", "d6_t (made by cloud_admin, owned by d6_api) carries neon_superuser ALL WITH GRANT OPTION", t_acl.contains("neon_superuser=a*r*w*d*D*x*t*m*/d6_api"), &t_acl);
        p.see("8", "default ACLs", fx.cloud("SELECT coalesce(string_agg(pg_get_userbyid(defaclrole) || ':' || defaclobjtype::text || ':' || array_to_string(defaclacl, ' '), ', '), '') FROM pg_default_acl;").await);
        let direct_grant = fx.owner("GRANT UPDATE ON public.d6_t TO d6_x;").await;
        let acl_direct = fx.acl("public.d6_t", "d6_x").await;
        p.check("8", "owner GRANT without SET ROLE on d6_api's table: allowed, recorded as neon_superuser's", direct_grant.as_ref().is_ok_and(|o| o.stderr.is_empty()) && acl_direct == "d6_x|SELECT|d6_api,d6_x|UPDATE|neon_superuser", format!("{} / {acl_direct}", show(&direct_grant)));
        let direct_revoke = fx.owner("REVOKE SELECT, UPDATE ON public.d6_t FROM d6_x;").await;
        let acl_direct_rev = fx.acl("public.d6_t", "d6_x").await;
        p.check("8", "owner REVOKE without SET ROLE: removes neon_superuser's grant only, d6_api's stays, no warning", direct_revoke.as_ref().is_ok_and(|o| o.stderr.is_empty()) && acl_direct_rev == "d6_x|SELECT|d6_api", format!("{} / {acl_direct_rev}", show(&direct_revoke)));

        // A table without the neon_superuser entry, where the owner holds SELECT WITH GRANT OPTION only.
        fx.cloud(
            "CREATE ROLE d6_o8 NOLOGIN; CREATE TABLE public.d6_t8 (id int); ALTER TABLE public.d6_t8 OWNER TO d6_o8; \
             REVOKE ALL ON public.d6_t8 FROM neon_superuser; \
             GRANT SELECT ON public.d6_t8 TO postgres WITH GRANT OPTION; GRANT SELECT ON public.d6_t8 TO d6_x;",
        )
        .await;
        p.see("8", "owner client_min_messages", show(&fx.owner("SHOW client_min_messages;").await));
        p.see("8", "d6_t8 ACL", rel_acl("public.d6_t8").await);
        for (sql, code, text) in [
            ("GRANT SELECT, INSERT ON public.d6_t8 TO d6_x;", "01007", "not all privileges were granted for \"d6_t8\""),
            ("GRANT INSERT ON public.d6_t8 TO d6_x;", "01007", "no privileges were granted for \"d6_t8\""),
            ("REVOKE INSERT ON public.d6_t8 FROM d6_x;", "01006", "no privileges could be revoked for \"d6_t8\""),
            ("REVOKE SELECT, INSERT ON public.d6_t8 FROM d6_x;", "01006", "not all privileges could be revoked for \"d6_t8\""),
        ] {
            let out = fx.owner(sql).await;
            p.see("8", &format!("{sql} then d6_t8 ACL"), rel_acl("public.d6_t8").await);
            p.check(
                "8",
                &format!("{sql} warns {code}"),
                out.as_ref().is_ok_and(|o| o.stderr.contains(&format!("WARNING:  {code}: {text}"))),
                show(&out),
            );
        }
        let acl8 = fx.acl("public.d6_t8", "d6_x").await;
        p.check("8", "d6_o8's grant to d6_x survived the owner's revokes", acl8 == "d6_x|SELECT|d6_o8", &acl8);
        let silent = fx.owner("REVOKE SELECT ON public.d6_t8 FROM d6_x;").await;
        p.check(
            "8",
            "revoking a grant another grantor made: no warning, nothing revoked",
            silent.as_ref().is_ok_and(|o| o.stderr.is_empty()),
            show(&silent),
        );
        let acl8b = fx.acl("public.d6_t8", "d6_x").await;
        p.check("8", "the other grantor's grant is still there", acl8b == "d6_x|SELECT|d6_o8", &acl8b);
        fx.cloud("REVOKE ALL ON public.d6_t8 FROM postgres;").await;
        let no_rights = fx.owner("REVOKE SELECT ON public.d6_t8 FROM d6_x;").await;
        p.check(
            "8",
            "REVOKE on a table where the owner holds nothing: still only warnings 01006 (one per column too), no error",
            no_rights.as_ref().is_ok_and(|o| {
                o.stderr.contains("WARNING:  01006: no privileges could be revoked for \"d6_t8\"")
                    && o.stderr.contains("WARNING:  01006: no privileges could be revoked for column \"id\" of relation \"d6_t8\"")
            }),
            show(&no_rights),
        );

        p.finish();
    })
    .await;
}
