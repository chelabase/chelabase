//! The `storage` schema against real computes (object-storage m3, issue #94): installed at
//! provision with row level security forced, reinstalled by nothing on a second run, closed
//! to the owner's DDL and to writes of the managed columns, deny-all for `authenticated`
//! until a policy made through `storage.create_policy` says otherwise, copied to a branch,
//! and never read by the merge's catalog.
//!
//! `shared-state`: provisions a database and a branch on the workbench stack, deleted
//! afterwards pass or fail.
//!
//! ```text
//! just workbench test -p worker --test storage_schema_live -- --run-ignored only
//! ```

// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chelabase_metadata::{
    BranchStatus, CreateBranchOutcome, DEMO_TENANT_ID, DatabaseStatus, MetadataClient, NewBranch,
};
use chelabase_nats::BranchJob;
use chelabase_neon::{ComputeBackend, ComputeLocator, DockerLocator};
use chelabase_secrets::{Purpose, open_credential};
use chelabase_types::{BranchId, DatabaseId, ProjectId};
use worker::config::RealWorker;
use worker::data_api::schema_check_sql;
use worker::storage_schema::storage_schema_sql;

mod common;
use common::{cleanup, proxy_ca_path, proxy_hostaddr, setup};

const OWNER: &str = "postgres";

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
        let name = format!("st-{}", &id.to_string()[..8]);
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

    /// SQL as cloud_admin on `ep`, in the main database.
    async fn admin_on(&self, ep: &str, sql: &str) -> Result<String, String> {
        self.worker
            .computes
            .admin_sql(ep, &self.database, sql)
            .await
            .map_err(|e| format!("{e:#}"))
    }

    async fn admin(&self, sql: &str) -> String {
        self.admin_on(&self.ep, sql)
            .await
            .unwrap_or_else(|e| panic!("admin_sql {sql:?}: {e}"))
    }

    /// The last non-empty line of `sql` as cloud_admin (psql echoes BEGIN/SET around it).
    async fn value(&self, sql: &str) -> String {
        last_line(&self.admin(sql).await)
    }

    /// `sql` as `role` (`SET LOCAL ROLE` from cloud_admin), in a transaction rolled back.
    async fn as_role(&self, role: &str, sql: &str) -> Result<String, String> {
        self.admin_on(
            &self.ep,
            &format!("BEGIN; SET LOCAL ROLE {role}; {sql}; ROLLBACK;"),
        )
        .await
    }

    /// `sql` as `role`, committed.
    async fn as_role_commit(&self, role: &str, sql: &str) -> String {
        self.admin(&format!("BEGIN; SET LOCAL ROLE {role}; {sql}; COMMIT;"))
            .await
    }

    /// The SQLSTATE `statement` fails with as `role` on `ep` (`ok` when it succeeds),
    /// rolled back either way.
    async fn code_on(&self, ep: &str, role: &str, statement: &str) -> String {
        let quoted = format!("'{}'", statement.replace('\'', "''"));
        let sql = format!(
            "BEGIN;
CREATE FUNCTION pg_temp.try(q text) RETURNS text LANGUAGE plpgsql AS $t$ BEGIN EXECUTE q; RETURN 'ok'; EXCEPTION WHEN OTHERS THEN RETURN SQLSTATE; END $t$;
SET LOCAL ROLE {role};
SELECT pg_temp.try({quoted});
ROLLBACK;"
        );
        last_line_matching(
            &self
                .admin_on(ep, &sql)
                .await
                .unwrap_or_else(|e| panic!("{sql}: {e}")),
        )
    }

    async fn code(&self, role: &str, statement: &str) -> String {
        self.code_on(&self.ep, role, statement).await
    }

    /// `sql` as `user` with `password`, straight to the compute's Postgres (the proxy knows
    /// only the roles in the compute spec).
    async fn direct(&self, user: &str, password: &str, sql: &str) -> Result<String, String> {
        let locator = DockerLocator::connect(
            &std::env::var("CHELABASE_COMPUTE_NETWORK")
                .unwrap_or_else(|_| "chelabase_compute".into()),
            &std::env::var("CHELABASE_STACK").unwrap_or_else(|_| "main".into()),
        )
        .expect("the Docker client");
        let addr = locator
            .postgres_addr(&self.ep)
            .await
            .expect("the compute's Postgres address")
            .expect("a published Postgres port");
        let conn = format!(
            "host={} port={} dbname={} sslmode=prefer",
            addr.ip(),
            addr.port(),
            self.database
        );
        psql(&conn, user, password, sql).await
    }

    /// SQL as the owner through the proxy.
    async fn owner(&self, sql: &str) -> Result<String, String> {
        psql(
            &proxy_conn(&self.ep, &self.database),
            OWNER,
            &self.owner_password,
            sql,
        )
        .await
    }

    async fn owner_err(&self, sql: &str) -> String {
        match self.owner(sql).await {
            Ok(out) => panic!("as the owner, {sql} succeeded: {out}"),
            Err(e) => e,
        }
    }

    /// A child of main, made by the worker's create job; its id and endpoint.
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
}

/// Everything the install defines, as one text: a second run must leave it unchanged.
const SHAPE_SQL: &str = "SELECT pg_catalog.md5(pg_catalog.string_agg(x, '|' ORDER BY x)) FROM (
  SELECT c.relname || ':' || c.relowner::regrole || ':' || c.relrowsecurity || ':' || c.relforcerowsecurity || ':' || coalesce(c.relacl::text, '') AS x
    FROM pg_class c WHERE c.relnamespace = 'storage'::regnamespace
  UNION ALL SELECT p.oid::regprocedure || ':' || p.proowner::regrole || ':' || p.prosecdef || ':' || coalesce(p.proacl::text, '') || ':' || md5(p.prosrc)
    FROM pg_proc p WHERE p.pronamespace = 'storage'::regnamespace
  UNION ALL SELECT a.attrelid::regclass || '.' || a.attname || ':' || coalesce(a.attacl::text, '')
    FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid WHERE c.relnamespace = 'storage'::regnamespace AND a.attnum > 0
  UNION ALL SELECT t.tgname || ':' || t.tgenabled::text FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid
    WHERE c.relnamespace = 'storage'::regnamespace AND NOT t.tgisinternal
  UNION ALL SELECT 'schema:' || n.nspowner::regrole || ':' || coalesce(n.nspacl::text, '') || ':' || coalesce(obj_description(n.oid, 'pg_namespace'), '')
    FROM pg_namespace n WHERE n.nspname = 'storage'
  UNION ALL SELECT 'policy:' || y.polname FROM pg_policy y JOIN pg_class c ON c.oid = y.polrelid WHERE c.relnamespace = 'storage'::regnamespace
) s";

/// A bucket and an object, written as the storage role (with the object's ticket).
const SEED_SQL: &str = "BEGIN; SET LOCAL ROLE chelabase_storage;
INSERT INTO storage.buckets (id, name) VALUES ('00000000-0000-0000-0000-0000000000b1', 'avatars');
INSERT INTO storage.upload_tickets (version_id, bucket_id, name, size, etag, expires_at)
  VALUES ('00000000-0000-0000-0000-0000000000c1', '00000000-0000-0000-0000-0000000000b1', 'u1/me.png', 3, 'e1', now() + interval '5 minutes');
INSERT INTO storage.objects (bucket_id, name, owner, size, etag, version_id)
  VALUES ('00000000-0000-0000-0000-0000000000b1', 'u1/me.png', '00000000-0000-0000-0000-0000000000a1', 3, 'e1', '00000000-0000-0000-0000-0000000000c1');
COMMIT;";

#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn storage_schema_live() {
    let fx = Arc::new(Fx::provision().await);
    // A panic inside the task is caught here, so the cleanup below always runs.
    let task = fx.clone();
    let outcome = tokio::spawn(async move { run(&task).await }).await;
    fx.teardown().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

async fn run(fx: &Fx) {
    // AC 1: both tables, chelabase_admin's, RLS enabled and forced, no policy.
    assert_eq!(
        fx.value("SELECT string_agg(relname || ' ' || relowner::regrole || ' ' || relrowsecurity || ' ' || relforcerowsecurity, ',' ORDER BY relname) FROM pg_class WHERE relnamespace = 'storage'::regnamespace AND relkind = 'r'").await,
        "buckets chelabase_admin true true,objects chelabase_admin true true,upload_tickets chelabase_admin false false"
    );
    assert_eq!(fx.value("SELECT count(*) FROM pg_policy y JOIN pg_class c ON c.oid = y.polrelid WHERE c.relnamespace = 'storage'::regnamespace").await, "0");
    assert_eq!(fx.value("SELECT rolbypassrls AND NOT rolsuper AND NOT rolcanlogin FROM pg_roles WHERE rolname = 'chelabase_storage'").await, "t");
    assert_eq!(fx.value("SELECT storage.foldername('a/b/c.png')::text || ' ' || storage.filename('a/b/c.png') || ' ' || storage.extension('a/b/c.tar.gz') || ' ' || storage.foldername('c')::text").await, "{a,b} c.png gz {}");
    assert_eq!(last_line(&fx.admin(&schema_check_sql("public")).await), "t");

    // AC 2: a second run changes nothing.
    let before = fx.value(SHAPE_SQL).await;
    fx.admin(&storage_schema_sql(OWNER)).await;
    fx.worker
        .bootstrap_data_api(
            &fx.metadata.get_branch(fx.main).await.unwrap().unwrap(),
            &fx.database,
        )
        .await
        .expect("bootstrap again");
    assert_eq!(fx.value(SHAPE_SQL).await, before);

    fx.admin(SEED_SQL).await;

    // AC 3: the owner can't loosen RLS, add triggers, drop the schema or forge managed columns.
    for sql in [
        "ALTER TABLE storage.objects NO FORCE ROW LEVEL SECURITY",
        "ALTER TABLE storage.objects DISABLE ROW LEVEL SECURITY",
        "ALTER TABLE storage.objects DISABLE TRIGGER protect_objects",
        "CREATE POLICY p ON storage.objects USING (true)",
        "CREATE TRIGGER t BEFORE INSERT ON storage.objects FOR EACH ROW EXECUTE FUNCTION storage.protect_objects()",
        "DROP SCHEMA storage CASCADE",
        "CREATE TABLE storage.mine (a int)",
        "UPDATE storage.objects SET version_id = gen_random_uuid()",
        "UPDATE storage.objects SET size = 1",
        "UPDATE storage.objects SET bucket_id = bucket_id, etag = 'forged'",
        "INSERT INTO storage.objects (bucket_id, name, version_id) VALUES ('00000000-0000-0000-0000-0000000000b1', 'x', gen_random_uuid())",
        "SET session_replication_role = replica; UPDATE storage.objects SET version_id = gen_random_uuid()",
        "SET ROLE chelabase_storage",
    ] {
        let err = fx.owner_err(sql).await;
        assert!(
            err.contains("CB003")
                || err.contains("42501")
                || err.contains("must be owner")
                || err.contains("permission denied"),
            "{sql}: {err}"
        );
    }
    assert_eq!(
        fx.value("SELECT version_id || ' ' || size || ' ' || etag FROM storage.objects")
            .await,
        "00000000-0000-0000-0000-0000000000c1 3 e1"
    );
    assert_eq!(
        fx.value(
            "SELECT relforcerowsecurity FROM pg_class WHERE oid = 'storage.objects'::regclass"
        )
        .await,
        "t"
    );

    // AC 5: authenticated with no policy sees nothing and writes nothing.
    assert_eq!(
        last_line(
            &fx.as_role("authenticated", "SELECT count(*) FROM storage.objects")
                .await
                .unwrap()
        ),
        "0"
    );
    assert_eq!(
        last_line(
            &fx.as_role("anon", "SELECT count(*) FROM storage.buckets")
                .await
                .unwrap()
        ),
        "0"
    );
    for sql in [
        "INSERT INTO storage.objects (name) VALUES ('x')",
        "INSERT INTO storage.objects (bucket_id, name) VALUES ('00000000-0000-0000-0000-0000000000b1', 'x')",
        "INSERT INTO storage.upload_tickets (version_id, bucket_id, name, size, etag, expires_at) VALUES (gen_random_uuid(), gen_random_uuid(), 'x', 1, 'e', now())",
        "SELECT storage.create_policy('p', 'objects', 'SELECT', ARRAY['authenticated']::name[], 'true')",
    ] {
        let err = fx.as_role("authenticated", sql).await.expect_err(sql);
        assert!(
            err.contains("permission denied") || err.contains("row-level security"),
            "{sql}: {err}"
        );
    }

    // Policies go through the owner's helper; a ";" can't smuggle a second statement.
    let err = fx.owner_err("SELECT storage.create_policy('p', 'objects', 'SELECT', ARRAY['authenticated']::name[], 'true); ALTER TABLE storage.objects NO FORCE ROW LEVEL SECURITY; SELECT (1')").await;
    assert!(err.contains("can't contain"), "{err}");
    let err = fx.owner_err("SELECT storage.create_policy('p', 'mine', 'SELECT', ARRAY['authenticated']::name[], 'true')").await;
    assert!(err.contains("buckets or objects"), "{err}");
    fx.owner("SELECT storage.create_policy('own_rows', 'objects', 'SELECT', ARRAY['authenticated']::name[], E'owner = ''00000000-0000-0000-0000-0000000000a1''::uuid -- trailing comment')").await.expect("create a policy");
    assert_eq!(
        last_line(
            &fx.as_role("authenticated", "SELECT count(*) FROM storage.objects")
                .await
                .unwrap()
        ),
        "1"
    );
    assert_eq!(
        fx.value(
            "SELECT relforcerowsecurity FROM pg_class WHERE oid = 'storage.objects'::regclass"
        )
        .await,
        "t"
    );
    // Without an UPDATE policy an update reaches no row.
    fx.as_role("authenticated", "UPDATE storage.objects SET size = 1")
        .await
        .expect("an update no policy allows");
    assert_eq!(fx.value("SELECT size FROM storage.objects").await, "3");

    // Tickets: a caller's insert under its policy needs one; RLS (42501) and the ticket
    // trigger (CB003) refuse with different codes.
    fx.owner("SELECT storage.create_policy('insert_avatars', 'objects', 'INSERT', ARRAY['authenticated']::name[], NULL, E'bucket_id = ''00000000-0000-0000-0000-0000000000b1''::uuid AND size < 100')").await.expect("an insert policy");
    let insert = |size: u32| {
        format!(
            "INSERT INTO storage.objects (bucket_id, name, size, etag, version_id) VALUES ('00000000-0000-0000-0000-0000000000b1', 'u1/a.png', {size}, 'e2', '00000000-0000-0000-0000-0000000000c2')"
        )
    };
    assert_eq!(
        fx.code("authenticated", &insert(500)).await,
        "42501",
        "RLS refuses size 500"
    );
    assert_eq!(
        fx.code("authenticated", &insert(5)).await,
        "CB003",
        "no ticket"
    );
    let mint = "INSERT INTO storage.upload_tickets (version_id, bucket_id, name, size, etag, expires_at) VALUES ('00000000-0000-0000-0000-0000000000c2', '00000000-0000-0000-0000-0000000000b1', 'u1/a.png', 5, 'e2', now() + interval '5 minutes')";
    // A ticket for other values doesn't do.
    fx.as_role_commit("chelabase_storage", mint).await;
    assert_eq!(
        fx.code("authenticated", &insert(5).replace("'e2'", "'e3'"))
            .await,
        "CB003",
        "another etag"
    );
    // Expiry beyond the cap, and a ticket from the owner (pg_write_all_data), are refused.
    assert_eq!(
        fx.code(
            "chelabase_storage",
            &mint.replace("c2'", "c9'").replace("5 minutes", "1 day")
        )
        .await,
        "22023"
    );
    let err = fx.owner_err(&mint.replace("c2'", "c8'")).await;
    assert!(err.contains("CB004"), "the owner mints: {err}");
    assert_eq!(fx.code("authenticated", "INSERT INTO storage.upload_tickets (version_id, bucket_id, name, size, etag, expires_at) VALUES (gen_random_uuid(), gen_random_uuid(), 'x', 1, 'e', now())").await, "42501");
    // The matching ticket: stored once, the ticket gone; the same ticket again: CB003.
    fx.as_role_commit("authenticated", &insert(5)).await;
    assert_eq!(fx.value("SELECT count(*) FROM storage.upload_tickets WHERE version_id = '00000000-0000-0000-0000-0000000000c2'").await, "0");
    assert_eq!(
        fx.value("SELECT size || ' ' || etag FROM storage.objects WHERE name = 'u1/a.png'")
            .await,
        "5 e2"
    );
    fx.as_role_commit(
        "chelabase_storage",
        "DELETE FROM storage.objects WHERE name = 'u1/a.png'",
    )
    .await;
    assert_eq!(
        fx.code("authenticated", &insert(5)).await,
        "CB003",
        "a used ticket"
    );
    // A rename changes no managed column: no ticket needed (RLS still decides).
    fx.as_role_commit(
        "chelabase_storage",
        "UPDATE storage.objects SET name = 'u1/renamed.png' WHERE name = 'u1/me.png'",
    )
    .await;

    // A ticket left on main when the branch is made.
    fx.as_role_commit(
        "chelabase_storage",
        &mint
            .replace("c2'", "c3'")
            .replace("'u1/a.png'", "'u1/b.png'"),
    )
    .await;

    // AC (branch): a child has the parent's rows and its policy.
    let (_child, child_ep) = fx.child("storage-child").await;
    // The child has the ticket row but not the timeline it was minted on: unusable there.
    assert_eq!(last_line(&fx.admin_on(&child_ep, "SELECT count(*) FROM storage.upload_tickets WHERE version_id = '00000000-0000-0000-0000-0000000000c3'").await.unwrap()), "1");
    assert_eq!(
        fx.code_on(&child_ep, "authenticated", "INSERT INTO storage.objects (bucket_id, name, size, etag, version_id) VALUES ('00000000-0000-0000-0000-0000000000b1', 'u1/b.png', 5, 'e2', '00000000-0000-0000-0000-0000000000c3')").await,
        "CB003",
        "a parent's ticket on the child"
    );
    // ...while on main it still works.
    fx.as_role_commit("authenticated", "INSERT INTO storage.objects (bucket_id, name, size, etag, version_id) VALUES ('00000000-0000-0000-0000-0000000000b1', 'u1/b.png', 5, 'e2', '00000000-0000-0000-0000-0000000000c3')").await;
    assert_eq!(last_line(&fx.admin_on(&child_ep, "SELECT count(*) FROM storage.objects o JOIN storage.buckets b ON b.id = o.bucket_id WHERE b.name = 'avatars'").await.unwrap()), "1");
    assert_eq!(
        last_line(
            &fx.admin_on(
                &child_ep,
                "SELECT count(*) FROM pg_policy WHERE polname = 'own_rows'"
            )
            .await
            .unwrap()
        ),
        "1"
    );

    // AC (merge): the catalog the merge plans from never reads `storage`.
    let catalog = fx.admin(chelabase_schema::catalog::CATALOG_SQL).await;
    let model =
        chelabase_schema::catalog::parse_catalog(catalog.trim()).expect("parse the catalog");
    let json = serde_json::to_string(&model).unwrap();
    assert!(!json.contains("\"storage\""), "{json}");

    // AC (pre-created role): whatever was made of chelabase_storage is undone by the next
    // bootstrap, and a ticket from then is void.
    fx.admin("CREATE ROLE st_member LOGIN; CREATE TABLE public.st_owned (a int); ALTER TABLE public.st_owned OWNER TO chelabase_storage;").await;
    fx.admin(&format!("ALTER ROLE chelabase_storage LOGIN PASSWORD '{TAMPER_PASSWORD}' CONNECTION LIMIT 5; ALTER ROLE chelabase_storage SET work_mem = '1MB';
GRANT chelabase_storage TO st_member WITH ADMIN OPTION; GRANT {OWNER} TO chelabase_storage;")).await;
    fx.as_role_commit(
        "chelabase_storage",
        &mint
            .replace("c2'", "c4'")
            .replace("'u1/a.png'", "'u1/c.png'"),
    )
    .await;
    fx.direct("chelabase_storage", TAMPER_PASSWORD, "SELECT 1")
        .await
        .expect("the tampered role logs in");
    fx.worker
        .bootstrap_data_api(
            &fx.metadata.get_branch(fx.main).await.unwrap().unwrap(),
            &fx.database,
        )
        .await
        .expect("bootstrap after the tamper");
    assert_eq!(
        fx.value("SELECT rolcanlogin || ' ' || (rolpassword IS NULL) || ' ' || rolconnlimit || ' ' || rolbypassrls || ' ' || rolinherit FROM pg_authid WHERE rolname = 'chelabase_storage'").await,
        "false true 0 true false"
    );
    assert_eq!(fx.value("SELECT coalesce(string_agg(array_to_string(setconfig, ','), ';'), '') FROM pg_db_role_setting WHERE setrole = 'chelabase_storage'::regrole").await, "search_path=pg_catalog, pg_temp");
    assert_eq!(fx.value("SELECT count(*) FROM pg_auth_members WHERE roleid = 'chelabase_storage'::regrole OR member = 'chelabase_storage'::regrole").await, "0");
    assert_eq!(
        fx.value("SELECT relowner::regrole FROM pg_class WHERE oid = 'public.st_owned'::regclass")
            .await,
        OWNER
    );
    assert_eq!(
        fx.value("SELECT count(*) FROM storage.upload_tickets")
            .await,
        "0",
        "tickets from the tampered time are void"
    );
    let err = fx
        .direct("chelabase_storage", TAMPER_PASSWORD, "SELECT 1")
        .await
        .expect_err("no login");
    assert!(!err.is_empty());
    // Neither the owner nor authenticator can become it.
    let err = fx.owner_err("SET ROLE chelabase_storage").await;
    assert!(err.contains("permission denied"), "{err}");
    let main = fx.metadata.get_branch(fx.main).await.unwrap().unwrap();
    let authenticator_password = open_credential(
        &fx.worker.key,
        &main.id.to_string(),
        Purpose::Authenticator,
        main.authenticator_password_enc
            .as_deref()
            .expect("authenticator password"),
    )
    .unwrap();
    let err = fx
        .direct(
            "authenticator",
            &authenticator_password,
            "SET ROLE chelabase_storage",
        )
        .await
        .expect_err("authenticator");
    assert!(err.contains("permission denied"), "{err}");
    fx.admin("DROP TABLE public.st_owned; DROP ROLE st_member;")
        .await;

    fx.owner("SELECT storage.drop_policy('own_rows', 'objects')")
        .await
        .expect("drop the policy");
    assert_eq!(
        last_line(
            &fx.as_role("authenticated", "SELECT count(*) FROM storage.objects")
                .await
                .unwrap()
        ),
        "0"
    );
}

/// The password the tamper scenario gives `chelabase_storage`.
const TAMPER_PASSWORD: &str = "tamper-Passw0rd-3c9e";

/// The last line of `out` that looks like a SQLSTATE or `ok` (psql echoes BEGIN/SET/ROLLBACK).
fn last_line_matching(out: &str) -> String {
    out.lines()
        .map(str::trim)
        .rfind(|l| *l == "ok" || (l.len() == 5 && l.chars().all(|c| c.is_ascii_alphanumeric())))
        .unwrap_or_else(|| panic!("no SQLSTATE in {out:?}"))
        .to_string()
}

fn last_line(out: &str) -> String {
    out.lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

fn proxy_conn(ep: &str, database: &str) -> String {
    let port: u16 = std::env::var("CHELABASE_PROXY_PG_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(4432);
    let domain = std::env::var("CHELABASE_PROXY_DOMAIN")
        .ok()
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| "localtest.me".into());
    let ca = proxy_ca_path();
    assert!(
        ca.exists(),
        "{} is missing (run `just workbench up`)",
        ca.display()
    );
    format!(
        "host={ep}.{domain} hostaddr={} port={port} dbname={database} sslmode=verify-full sslrootcert={}",
        proxy_hostaddr(),
        ca.display()
    )
}

/// `sql` as `user` on `conn`: the output, or psql's error with its SQLSTATE.
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
