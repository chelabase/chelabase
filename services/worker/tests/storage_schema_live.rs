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
use worker::storage_schema::{self, storage_schema_sql};

mod common;
use common::{cleanup, proxy_ca_path, proxy_hostaddr, setup, workspace_root};
use futures_util::StreamExt;
use serde_json::{Value, json};
use uuid::Uuid;

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

    /// The verifier the branch's `chelabase_storage` must carry (the bootstrap stored its login).
    async fn verifier(&self) -> String {
        self.worker
            .storage_verifier(self.main)
            .await
            .expect("read the storage login")
            .expect("a stored storage login")
    }

    /// The branch's `chelabase_storage` password, as the orchestrator hands it to storage.
    async fn storage_password(&self) -> String {
        let row = self
            .metadata
            .branch_connection(self.main)
            .await
            .unwrap()
            .expect("the branch's row");
        open_credential(
            &self.worker.key,
            &self.main.to_string(),
            Purpose::Storage,
            row.storage_password_enc.as_deref().expect("a storage password"),
        )
        .expect("open the storage password")
    }

    /// `chelabase_storage`'s attributes, its password checked against the verifier
    /// ([`LOCKED_DOWN`] when the lockdown left it).
    async fn locked_down(&self) -> String {
        let verifier = self.verifier().await;
        self.value(&format!(
            "SELECT rolcanlogin || ' ' || (rolpassword = '{verifier}') || ' ' || rolconnlimit || ' ' || rolbypassrls || ' ' || rolinherit || ' ' || rolvaliduntil FROM pg_authid WHERE rolname = 'chelabase_storage'"
        ))
        .await
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
INSERT INTO storage.upload_tickets (version_id, bucket_id, name, size, etag, expires_at, caller_role, owner)
  VALUES ('00000000-0000-0000-0000-0000000000c1', '00000000-0000-0000-0000-0000000000b1', 'u1/me.png', 3, 'e1', now() + interval '5 minutes', 'chelabase_storage', '00000000-0000-0000-0000-0000000000a1');
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
    assert_eq!(fx.value("SELECT rolbypassrls AND NOT rolsuper AND rolcanlogin FROM pg_roles WHERE rolname = 'chelabase_storage'").await, "t");
    assert_eq!(fx.value("SELECT storage.foldername('a/b/c.png')::text || ' ' || storage.filename('a/b/c.png') || ' ' || storage.extension('a/b/c.tar.gz') || ' ' || storage.foldername('c')::text").await, "{a,b} c.png gz {}");
    assert_eq!(last_line(&fx.admin(&schema_check_sql("public", Some(&fx.verifier().await))).await), "t");

    // AC 2: a second run changes nothing.
    let before = fx.value(SHAPE_SQL).await;
    fx.admin(&storage_schema_sql(OWNER, &fx.verifier().await, None)).await;
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
    let mint = "INSERT INTO storage.upload_tickets (version_id, bucket_id, name, size, etag, expires_at, caller_role) VALUES ('00000000-0000-0000-0000-0000000000c2', '00000000-0000-0000-0000-0000000000b1', 'u1/a.png', 5, 'e2', now() + interval '5 minutes', 'authenticated')";
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
    // m5 phase B (schema 5): every update stamps updated_at.
    assert_eq!(
        fx.value("SELECT updated_at > created_at FROM storage.objects WHERE name = 'u1/renamed.png'").await,
        "t"
    );

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
    assert_eq!(fx.locked_down().await, LOCKED_DOWN);
    assert_eq!(fx.value("SELECT coalesce(string_agg(array_to_string(setconfig, ','), ';'), '') FROM pg_db_role_setting WHERE setrole = 'chelabase_storage'::regrole").await, "search_path=pg_catalog, pg_temp,event_triggers=off");
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
        .expect_err("no login with the tamperer's password");
    assert!(!err.is_empty());
    fx.direct("chelabase_storage", &fx.storage_password().await, "SELECT 1")
        .await
        .expect("the branch's own password logs in again");
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

/// `chelabase_storage` as the lockdown leaves it ([`Fx::locked_down`]): it logs in, with the
/// branch's verifier, a connection limit of 40, BYPASSRLS, NOINHERIT, no expiry.
const LOCKED_DOWN: &str = "true true 40 true false infinity";

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

// --- Hardening (white-room audit of m3: B1, S1, S2, S3, N1, N2) ---------------------------

const B1: &str = "00000000-0000-0000-0000-0000000000b1";

#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn storage_schema_hardening_live() {
    let fx = Arc::new(Fx::provision().await);
    let task = fx.clone();
    let outcome = tokio::spawn(async move { hardening(&task).await }).await;
    fx.teardown().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

async fn bootstrap(fx: &Fx) {
    fx.worker
        .bootstrap_data_api(
            &fx.metadata.get_branch(fx.main).await.unwrap().unwrap(),
            &fx.database,
        )
        .await
        .expect("bootstrap");
}

/// A psql session to the compute that stays open (stdin piped).
async fn session(fx: &Fx, user: &str, password: &str) -> tokio::process::Child {
    let locator = DockerLocator::connect(
        &std::env::var("CHELABASE_COMPUTE_NETWORK").unwrap_or_else(|_| "chelabase_compute".into()),
        &std::env::var("CHELABASE_STACK").unwrap_or_else(|_| "main".into()),
    )
    .expect("the Docker client");
    let addr = locator.postgres_addr(&fx.ep).await.unwrap().unwrap();
    let conn = format!(
        "host={} port={} dbname={} sslmode=prefer",
        addr.ip(),
        addr.port(),
        fx.database
    );
    tokio::process::Command::new("psql")
        .arg(conn)
        .env("PGUSER", user)
        .env("PGPASSWORD", password)
        .args(["-X", "-qtA", "-v", "VERBOSITY=verbose"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn psql")
}

async fn send(child: &mut tokio::process::Child, sql: &str) {
    use tokio::io::AsyncWriteExt;
    let stdin = child.stdin.as_mut().unwrap();
    stdin.write_all(sql.as_bytes()).await.unwrap();
    stdin.write_all(b"\n").await.unwrap();
    stdin.flush().await.unwrap();
}

async fn read_line(child: &mut tokio::process::Child) -> String {
    use tokio::io::AsyncBufReadExt;
    let mut reader = tokio::io::BufReader::new(child.stdout.as_mut().unwrap());
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(10), reader.read_line(&mut line))
        .await
        .expect("a line from the session")
        .unwrap();
    line.trim().to_string()
}

/// The session's stdout after closing its stdin.
async fn finish(mut child: tokio::process::Child) -> String {
    drop(child.stdin.take());
    let out = tokio::time::timeout(Duration::from_secs(15), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}

async fn hardening(fx: &Fx) {
    fx.as_role_commit(
        "chelabase_storage",
        &format!("INSERT INTO storage.buckets (id, name) VALUES ('{B1}', 'avatars')"),
    )
    .await;
    let mint = |v: &str, name: &str| {
        format!(
            "INSERT INTO storage.upload_tickets (version_id, bucket_id, name, size, etag, expires_at, caller_role) VALUES ('{v}', '{B1}', '{name}', 1, 'e', now() + interval '10 minutes', 'authenticated')"
        )
    };

    // B1: an owner event trigger never runs inside the policy helpers.
    fx.owner(
        "CREATE TABLE public.audit_log (u text, what text); GRANT ALL ON public.audit_log TO PUBLIC;
         CREATE FUNCTION public.audit_et() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN
           INSERT INTO public.audit_log VALUES (current_user, tg_tag);
           IF current_user = 'chelabase_admin' THEN
             EXECUTE 'CREATE FUNCTION storage.audit_backdoor() RETURNS void LANGUAGE sql AS $b$ SELECT $b$';
           END IF; END $$;
         CREATE EVENT TRIGGER audit_et ON ddl_command_end EXECUTE FUNCTION public.audit_et();",
    )
    .await
    .expect("the owner's event trigger");
    fx.owner("SELECT storage.create_policy('audit_et_p', 'objects', 'SELECT', ARRAY['anon']::name[], 'false')").await.expect("create a policy");
    fx.owner("SELECT storage.drop_policy('audit_et_p', 'objects')")
        .await
        .expect("drop the policy");
    // The trigger itself still works for the owner's own DDL.
    fx.owner("CREATE TABLE public.audit_t (a int)")
        .await
        .unwrap();
    assert_eq!(
        fx.value("SELECT count(*) FROM public.audit_log WHERE u = 'chelabase_admin'")
            .await,
        "0"
    );
    assert_ne!(
        fx.value("SELECT count(*) FROM public.audit_log").await,
        "0",
        "the trigger fires for the owner"
    );
    assert_eq!(
        fx.value("SELECT to_regprocedure('storage.audit_backdoor()') IS NULL")
            .await,
        "t"
    );
    fx.owner("DROP EVENT TRIGGER audit_et").await.unwrap();

    // B1 (healing): whatever was planted in `storage` (an object, an ACL entry with grant
    // option, a disabled trigger, a rule) makes the check fail and the next run removes it.
    fx.admin("CREATE ROLE audit_x NOLOGIN").await;
    fx.admin(
        "BEGIN; SET LOCAL ROLE chelabase_admin;
         CREATE FUNCTION storage.audit_backdoor() RETURNS void LANGUAGE sql SECURITY DEFINER AS $b$ SELECT $b$;
         GRANT EXECUTE ON FUNCTION storage.audit_backdoor() TO PUBLIC;
         GRANT ALL ON storage.upload_tickets, storage.objects TO audit_x WITH GRANT OPTION;
         GRANT SELECT ON storage.upload_tickets TO anon;
         CREATE VIEW storage.audit_v AS SELECT 1 AS one;
         CREATE RULE audit_r AS ON INSERT TO storage.upload_tickets DO INSTEAD NOTHING;
         ALTER TABLE storage.objects DISABLE TRIGGER protect_objects;
         COMMIT;",
    )
    .await;
    assert_eq!(
        last_line(&fx.admin(&schema_check_sql("public", Some(&fx.verifier().await))).await),
        "f",
        "the check notices the plant"
    );
    bootstrap(fx).await;
    assert_eq!(last_line(&fx.admin(&schema_check_sql("public", Some(&fx.verifier().await))).await), "t");
    assert_eq!(fx.value("SELECT to_regprocedure('storage.audit_backdoor()') IS NULL AND to_regclass('storage.audit_v') IS NULL").await, "t");
    assert_eq!(
        fx.value(
            "SELECT count(*) FROM pg_rewrite WHERE ev_class = 'storage.upload_tickets'::regclass"
        )
        .await,
        "0"
    );
    assert_eq!(fx.value("SELECT has_table_privilege('audit_x', 'storage.objects', 'SELECT') OR has_table_privilege('anon', 'storage.upload_tickets', 'SELECT')").await, "f");
    assert_eq!(fx.value("SELECT string_agg(tgname || '=' || tgenabled::text, ',' ORDER BY tgname) FROM pg_trigger WHERE tgrelid IN ('storage.objects'::regclass, 'storage.upload_tickets'::regclass) AND NOT tgisinternal").await, "guard_tickets=A,protect_objects=A");
    fx.admin("DROP ROLE audit_x").await;

    // S1: a session that reached chelabase_storage through another role is ended too.
    fx.admin("CREATE ROLE audit_r1 NOLOGIN; CREATE ROLE audit_r2 LOGIN PASSWORD 'Audit-pw-r2x9'; GRANT chelabase_storage TO audit_r1; GRANT audit_r1 TO audit_r2;").await;
    let mut transitive = session(fx, "audit_r2", "Audit-pw-r2x9").await;
    send(
        &mut transitive,
        "SET ROLE chelabase_storage; SELECT 'is ' || current_user;",
    )
    .await;
    assert_eq!(read_line(&mut transitive).await, "is chelabase_storage");
    bootstrap(fx).await;
    send(
        &mut transitive,
        &format!(
            "{}; SELECT 'minted';",
            mint("00000000-0000-0000-0000-0000000000d2", "d/r2")
        ),
    )
    .await;
    let out = finish(transitive).await;
    assert!(
        !out.contains("minted"),
        "the transitive session outlived the lockdown: {out}"
    );
    assert_eq!(
        fx.value("SELECT count(*) FROM storage.upload_tickets")
            .await,
        "0"
    );
    assert_eq!(
        fx.value("SELECT pg_has_role('audit_r2', 'chelabase_storage', 'MEMBER')")
            .await,
        "f"
    );
    fx.admin("DROP ROLE audit_r2; DROP ROLE audit_r1;").await;

    // S3: a stored default schema from before the reservation grants nothing on `storage`.
    fx.metadata
        .set_database_default_schema(fx.main, &fx.database, "storage")
        .await
        .expect("set default schema");
    fx.admin("DELETE FROM chelabase.bootstrap").await;
    bootstrap(fx).await;
    assert_eq!(
        fx.value("SELECT has_function_privilege('authenticated', 'storage.create_policy(name, name, text, name[], text, text, boolean)', 'EXECUTE') OR has_table_privilege('authenticated', 'storage.objects', 'INSERT') OR has_table_privilege('anon', 'storage.objects', 'INSERT')").await,
        "f"
    );
    fx.metadata
        .set_database_default_schema(fx.main, &fx.database, "public")
        .await
        .expect("reset default schema");

    // N1: expiry is the wall clock, not the transaction's start.
    fx.as_role_commit(
        "chelabase_storage",
        &mint("00000000-0000-0000-0000-0000000000e1", "n/1")
            .replace("interval '10 minutes'", "interval '1 second'"),
    )
    .await;
    fx.owner("SELECT storage.create_policy('ins', 'objects', 'INSERT', ARRAY['authenticated']::name[], NULL, 'true')").await.unwrap();
    assert_eq!(
        fx.code("authenticated", &format!("SELECT pg_sleep(1.5); INSERT INTO storage.objects (bucket_id, name, size, etag, version_id) VALUES ('{B1}', 'n/1', 1, 'e', '00000000-0000-0000-0000-0000000000e1')")).await,
        "CB003",
        "an expired ticket"
    );

    // N2: an unbalanced expression can't add a clause.
    let err = fx.owner_err("SELECT storage.create_policy('audit_paren', 'objects', 'UPDATE', ARRAY['anon']::name[], 'false) WITH CHECK (false')").await;
    assert!(
        err.contains("42601") || err.contains("syntax error"),
        "{err}"
    );

    // S2 (last: it moves the schema aside): a user's own `storage` schema doesn't stop the
    // role's lockdown.
    fx.admin("ALTER SCHEMA storage RENAME TO audit_storage_aside")
        .await;
    fx.owner("CREATE SCHEMA storage")
        .await
        .expect("the owner's own storage schema");
    fx.admin(&format!(
        "ALTER ROLE chelabase_storage LOGIN PASSWORD '{TAMPER_PASSWORD}' CONNECTION LIMIT 5"
    ))
    .await;
    bootstrap(fx).await;
    assert_eq!(fx.locked_down().await, LOCKED_DOWN);
    assert_eq!(
        fx.value("SELECT nspowner::regrole FROM pg_namespace WHERE nspname = 'storage'")
            .await,
        OWNER,
        "the user's schema is left alone"
    );
    fx.owner("DROP SCHEMA storage").await.unwrap();
    fx.admin("ALTER SCHEMA audit_storage_aside RENAME TO storage")
        .await;
}

// --- Re-audit (m3b): ticket binding, the role's event triggers, drift, guard ---------------

#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn storage_schema_reaudit_live() {
    let fx = Arc::new(Fx::provision().await);
    let task = fx.clone();
    let outcome = tokio::spawn(async move { reaudit(&task).await }).await;
    fx.teardown().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

const A1: &str = "00000000-0000-0000-0000-0000000000a1";

async fn reaudit(fx: &Fx) {
    fx.as_role_commit(
        "chelabase_storage",
        &format!("INSERT INTO storage.buckets (id, name) VALUES ('{B1}', 'avatars')"),
    )
    .await;
    fx.owner("SELECT storage.create_policy('ins', 'objects', 'INSERT', ARRAY['authenticated']::name[], NULL, 'true')").await.unwrap();
    let mint = |v: &str, name: &str, owner: &str| {
        format!(
            "INSERT INTO storage.upload_tickets (version_id, bucket_id, name, size, etag, expires_at, caller_role, owner) VALUES ('{v}', '{B1}', '{name}', 1, 'e', clock_timestamp() + interval '10 minutes', 'authenticated', '{owner}')"
        )
    };
    let insert = |v: &str, name: &str, owner: &str| {
        format!(
            "INSERT INTO storage.objects (bucket_id, name, owner, size, etag, version_id) VALUES ('{B1}', '{name}', '{owner}', 1, 'e', '{v}')"
        )
    };

    // S1: a ticket is for the caller's role and owner it was minted for.
    fx.as_role_commit(
        "chelabase_storage",
        &mint("00000000-0000-0000-0000-0000000000f1", "a/1", A1),
    )
    .await;
    let err = fx
        .owner_err(&insert("00000000-0000-0000-0000-0000000000f1", "a/1", A1))
        .await;
    assert!(
        err.contains("CB003"),
        "the owner uses another role's ticket: {err}"
    );
    assert_eq!(
        fx.code(
            "authenticated",
            &insert(
                "00000000-0000-0000-0000-0000000000f1",
                "a/1",
                "00000000-0000-0000-0000-0000000000ee"
            )
        )
        .await,
        "CB003",
        "another owner"
    );
    fx.as_role_commit(
        "authenticated",
        &insert("00000000-0000-0000-0000-0000000000f1", "a/1", A1),
    )
    .await;
    assert_eq!(
        fx.value("SELECT count(*) FROM storage.objects WHERE name = 'a/1'")
            .await,
        "1"
    );

    // S2: a membership of the role, made by the owner, fails the check; the next run revokes
    // it and leaves pending tickets alone.
    fx.as_role_commit(
        "chelabase_storage",
        &mint("00000000-0000-0000-0000-0000000000f2", "a/2", A1),
    )
    .await;
    fx.owner("CREATE ROLE audit_grp NOLOGIN; GRANT audit_grp TO chelabase_storage;")
        .await
        .expect("the owner grants a role to it");
    assert_eq!(last_line(&fx.admin(&schema_check_sql("public", Some(&fx.verifier().await))).await), "f");
    bootstrap(fx).await;
    assert_eq!(
        fx.value(
            "SELECT count(*) FROM pg_auth_members WHERE member = 'chelabase_storage'::regrole"
        )
        .await,
        "0"
    );
    assert_eq!(last_line(&fx.admin(&schema_check_sql("public", Some(&fx.verifier().await))).await), "t");
    assert_eq!(fx.value("SELECT count(*) FROM storage.upload_tickets WHERE version_id = '00000000-0000-0000-0000-0000000000f2'").await, "1", "a membership voids no ticket");
    fx.owner("DROP ROLE audit_grp").await.unwrap();

    // S3: a dropped or rewritten trigger, a planted default or constraint, a dropped bucket
    // key: the check notices, the next run restores.
    for plant in [
        "DROP TRIGGER protect_objects ON storage.objects",
        // The function is the superuser's now: only a superuser could do this.
        "RESET ROLE; CREATE OR REPLACE FUNCTION storage.protect_objects() RETURNS trigger LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS $f$ BEGIN RETURN NULL; END $f$",
        "ALTER TABLE storage.objects ALTER COLUMN metadata SET DEFAULT '{\"planted\":1}'",
        "ALTER TABLE storage.objects ADD CONSTRAINT audit_ck CHECK (size IS NULL OR size >= 0) NOT VALID",
        "ALTER TABLE storage.objects DROP CONSTRAINT objects_bucket_id_fkey",
        "ALTER TABLE storage.upload_tickets ALTER COLUMN etag SET DEFAULT 'planted'",
    ] {
        fx.admin(&format!(
            "BEGIN; SET LOCAL ROLE chelabase_admin; {plant}; COMMIT;"
        ))
        .await;
        assert_eq!(
            last_line(&fx.admin(&schema_check_sql("public", Some(&fx.verifier().await))).await),
            "f",
            "{plant}"
        );
        bootstrap(fx).await;
        assert_eq!(
            last_line(&fx.admin(&schema_check_sql("public", Some(&fx.verifier().await))).await),
            "t",
            "{plant}"
        );
    }
    assert_eq!(fx.value("SELECT pg_get_expr(adbin, adrelid) FROM pg_attrdef WHERE adrelid = 'storage.objects'::regclass AND adnum = (SELECT attnum FROM pg_attribute WHERE attrelid = 'storage.objects'::regclass AND attname = 'metadata')").await, "'{}'::jsonb");
    assert_eq!(fx.value("SELECT string_agg(conname, ',' ORDER BY conname) FROM pg_constraint WHERE conrelid = 'storage.objects'::regclass").await, "objects_bucket_id_fkey,objects_bucket_id_name_key,objects_pkey");
    assert_eq!(
        fx.code(
            "authenticated",
            &insert("00000000-0000-0000-0000-0000000000f9", "a/9", A1)
        )
        .await,
        "CB003",
        "the ticket gate is back"
    );

    // S4: on a protected branch, an owner wrapper called by a locked login can't change
    // policies; the owner itself still can.
    use worker::schema_guard::{GuardSettings, schema_guard_sql, schema_unguard_sql};
    fx.admin("CREATE ROLE audit_guarded LOGIN PASSWORD 'Audit-pw-g1x9'")
        .await;
    fx.owner("CREATE FUNCTION public.audit_w() RETURNS void LANGUAGE sql SECURITY DEFINER AS $$ SELECT storage.create_policy('audit_w', 'objects', 'SELECT', ARRAY['anon']::name[], 'true') $$; GRANT EXECUTE ON FUNCTION public.audit_w() TO audit_guarded;").await.unwrap();
    fx.admin(&schema_guard_sql(&GuardSettings {
        guard_logins: true,
        exempt: Default::default(),
    }))
    .await;
    let err = fx
        .direct("audit_guarded", "Audit-pw-g1x9", "SELECT public.audit_w()")
        .await
        .expect_err("a locked login");
    assert!(err.contains("chelabase:branch_protected"), "{err}");
    fx.owner("SELECT storage.create_policy('audit_o', 'objects', 'SELECT', ARRAY['anon']::name[], 'false')").await.expect("the owner, unlocked");
    fx.admin(&schema_unguard_sql()).await;
    assert_eq!(fx.value("SELECT string_agg(polname, ',' ORDER BY polname) FROM pg_policy WHERE polrelid = 'storage.objects'::regclass").await, "audit_o,ins");

    // The role's sessions fire no event trigger: an owner's login trigger doesn't run for it.
    fx.owner(
        "CREATE TABLE public.audit_logins (u text); GRANT ALL ON public.audit_logins TO PUBLIC;
         CREATE FUNCTION public.audit_login() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN INSERT INTO public.audit_logins VALUES (session_user); END $$;
         CREATE EVENT TRIGGER audit_login ON login EXECUTE FUNCTION public.audit_login();",
    )
    .await
    .expect("the owner's login trigger");
    fx.direct("audit_guarded", "Audit-pw-g1x9", "SELECT 1")
        .await
        .expect("a plain login");
    assert_eq!(
        fx.value("SELECT count(*) FROM public.audit_logins WHERE u = 'audit_guarded'")
            .await,
        "1",
        "the trigger fires for other logins"
    );
    fx.admin(&format!(
        "ALTER ROLE chelabase_storage LOGIN PASSWORD '{TAMPER_PASSWORD}' CONNECTION LIMIT 1"
    ))
    .await;
    let setting = fx
        .direct(
            "chelabase_storage",
            TAMPER_PASSWORD,
            "SELECT current_setting('event_triggers')",
        )
        .await
        .expect("the role logs in");
    assert_eq!(last_line(&setting), "off");
    // ...and it can't turn them back on (no SET on the parameter).
    let err = fx
        .direct(
            "chelabase_storage",
            TAMPER_PASSWORD,
            "SET event_triggers = on",
        )
        .await
        .expect_err("SET event_triggers");
    assert!(err.contains("42501"), "{err}");
    assert_eq!(
        fx.value("SELECT count(*) FROM public.audit_logins WHERE u = 'chelabase_storage'")
            .await,
        "0",
        "the login trigger ran for chelabase_storage"
    );
    bootstrap(fx).await;
    assert_eq!(fx.locked_down().await, LOCKED_DOWN);
    fx.owner("DROP EVENT TRIGGER audit_login").await.unwrap();
    fx.admin("DROP OWNED BY audit_guarded; DROP ROLE audit_guarded")
        .await;
}

// --- m5: chelabase_storage is the storage service's login -----------------------------------

#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn storage_login_live() {
    let fx = Arc::new(Fx::provision().await);
    let task = fx.clone();
    let outcome = tokio::spawn(async move { storage_login(&task).await }).await;
    fx.teardown().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

/// The managed pool's login check ([`chelabase_pgsession::SERVICE_LOGIN_SQL`]) as
/// `chelabase_storage` asks it on every connection.
fn service_login_check() -> String {
    chelabase_pgsession::SERVICE_LOGIN_SQL.replace("$1", "'chelabase_storage'")
}

async fn storage_login(fx: &Fx) {
    let password = fx.storage_password().await;
    let check = || async { last_line(&fx.admin(&schema_check_sql("public", Some(&fx.verifier().await))).await) };
    // Provisioned: it logs in straight to the compute with the stored password, as the bare
    // login the managed pool requires, with its pinned settings.
    assert_eq!(fx.locked_down().await, LOCKED_DOWN);
    assert_eq!(check().await, "t");
    let login = fx
        .direct("chelabase_storage", &password, &format!("{}; SELECT current_setting('search_path') || '|' || current_setting('event_triggers')", service_login_check()))
        .await
        .expect("chelabase_storage logs in with its password");
    assert_eq!(login, "t\npg_catalog, pg_temp|off");
    assert_eq!(fx.value("SELECT count(*) FROM pg_auth_members WHERE roleid = 'chelabase_storage'::regrole OR member = 'chelabase_storage'::regrole").await, "0");
    fx.direct("chelabase_storage", "not-the-password", "SELECT 1")
        .await
        .expect_err("a wrong password");
    // Never in the data API's connection details: those are authenticator's.
    let row = fx.metadata.branch_connection(fx.main).await.unwrap().unwrap();
    assert_ne!(row.storage_password_enc, row.authenticator_password_enc);

    // Neither the owner nor authenticator can become it.
    let err = fx.owner_err("SET ROLE chelabase_storage").await;
    assert!(err.contains("permission denied"), "{err}");
    let authenticator = open_credential(
        &fx.worker.key,
        &fx.main.to_string(),
        Purpose::Authenticator,
        row.authenticator_password_enc.as_deref().unwrap(),
    )
    .unwrap();
    let err = fx
        .direct("authenticator", &authenticator, "SET ROLE chelabase_storage")
        .await
        .expect_err("authenticator");
    assert!(err.contains("permission denied"), "{err}");

    // A rerun without drift leaves the storage service's own sessions open.
    let mut service = session(fx, "chelabase_storage", &password).await;
    send(&mut service, "SELECT 'before';").await;
    assert_eq!(read_line(&mut service).await, "before");
    bootstrap(fx).await;
    send(&mut service, "SELECT 'kept';").await;
    assert_eq!(read_line(&mut service).await, "kept");

    // A changed password fails the check; the bootstrap sets the verifier back and ends the
    // sessions opened meanwhile.
    fx.admin("ALTER ROLE chelabase_storage PASSWORD 'x-Changed-pw-9f3'").await;
    assert_eq!(check().await, "f");
    let mut tamperer = session(fx, "chelabase_storage", "x-Changed-pw-9f3").await;
    send(&mut tamperer, "SELECT 'in';").await;
    assert_eq!(read_line(&mut tamperer).await, "in");
    bootstrap(fx).await;
    assert_eq!(check().await, "t");
    assert_eq!(fx.locked_down().await, LOCKED_DOWN);
    send(&mut tamperer, "SELECT 'still in';").await;
    assert!(!finish(tamperer).await.contains("still in"), "the session opened while it drifted was ended");
    drop(service);
    fx.direct("chelabase_storage", "x-Changed-pw-9f3", "SELECT 1")
        .await
        .expect_err("the changed password no longer opens");
    fx.direct("chelabase_storage", &password, "SELECT 1")
        .await
        .expect("the branch's password logs in again");

    // A membership either way makes the managed pool's login check fail (so it refuses to
    // open) and the health check fail; the bootstrap revokes it.
    fx.admin("CREATE ROLE st_svc_grp NOLOGIN; GRANT st_svc_grp TO chelabase_storage;").await;
    let refused = fx
        .direct("chelabase_storage", &password, &service_login_check())
        .await
        .expect("logs in");
    assert_eq!(refused, "f", "the managed pool would open with a membership");
    assert_eq!(check().await, "f");
    bootstrap(fx).await;
    assert_eq!(fx.direct("chelabase_storage", &password, &service_login_check()).await.unwrap(), "t");
    assert_eq!(check().await, "t");
    fx.admin("DROP ROLE st_svc_grp").await;
}

// --- m5: the storage API through the gateway, as a data key, under RLS ----------------------

fn gateway() -> String {
    std::env::var("GATEWAY_URL")
        .or_else(|_| std::env::var("CHELABASE_ENTRY_API_URL"))
        .unwrap_or_else(|_| "http://api.chelabase.localhost:8000".into())
}

/// A management API call as the demo user (`chelabase dev token`): `(status, body)`.
async fn api(method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let cli = std::env::var("CHELABASE_CLI").expect("CHELABASE_CLI (run through just workbench test)");
    let token = tokio::process::Command::new(cli)
        .args(["dev", "token"])
        .current_dir(std::env::var("CHELABASE_CHECKOUT").map_or_else(|_| workspace_root(), Into::into))
        .output()
        .await
        .expect("chelabase dev token");
    assert!(token.status.success(), "chelabase dev token failed");
    let mut req = reqwest::Client::new()
        .request(method.parse().unwrap(), format!("{}{path}", gateway()))
        .bearer_auth(String::from_utf8_lossy(&token.stdout).trim())
        .timeout(Duration::from_secs(60));
    if let Some(body) = body {
        req = req.json(&body);
    }
    let res = req.send().await.expect("the management API answers");
    let status = res.status().as_u16();
    (status, serde_json::from_str(&res.text().await.unwrap_or_default()).unwrap_or(Value::Null))
}

/// A storage API call with data key `key`: `(status, body)`.
async fn storage(method: &str, path: &str, key: &str, body: Option<Value>) -> (u16, Value) {
    let mut req = reqwest::Client::new()
        .request(method.parse().unwrap(), format!("{}{path}", gateway()))
        .bearer_auth(key)
        .timeout(Duration::from_secs(30));
    if let Some(body) = body {
        req = req.json(&body);
    }
    let res = req.send().await.expect("the storage API answers");
    let status = res.status().as_u16();
    (status, serde_json::from_str(&res.text().await.unwrap_or_default()).unwrap_or(Value::Null))
}

impl Fx {
    fn keys_path(&self) -> String {
        format!("/v1/databases/{}/branches/{}/keys", self.id, self.main)
    }

    /// A secret key on `authenticated`, with the allow-list `databases` (`None`: every one).
    async fn secret_key(&self, name: &str, databases: Option<Vec<String>>) -> String {
        let (status, body) = api(
            "POST",
            &self.keys_path(),
            Some(json!({ "name": name, "role": "authenticated", "databases": databases })),
        )
        .await;
        assert_eq!(status, 201, "create key {name}: {body}");
        body["key"].as_str().unwrap().to_string()
    }

    async fn public_key(&self) -> String {
        let (status, body) = api("GET", &self.keys_path(), None).await;
        assert_eq!(status, 200, "{body}");
        body.as_array()
            .unwrap()
            .iter()
            .find(|k| k["kind"] == "public")
            .and_then(|k| k["key"].as_str())
            .expect("a public key")
            .to_string()
    }
}

#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_api_buckets_live() {
    let fx = Arc::new(Fx::provision().await);
    let task = fx.clone();
    let outcome = tokio::spawn(async move { storage_api_buckets(&task).await }).await;
    fx.teardown().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

async fn storage_api_buckets(fx: &Fx) {
    const BUCKETS: &str = "/storage/v1/bucket";
    let secret = fx.secret_key("st-api", None).await;
    let public = fx.public_key().await;

    // No policy: nothing is visible, and creating is refused by RLS.
    assert_eq!(storage("GET", BUCKETS, &secret, None).await, (200, json!([])));
    let (status, body) = storage("POST", BUCKETS, &secret, Some(json!({ "name": "avatars" }))).await;
    assert_eq!((status, &body["error"]["sqlstate"]), (403, &json!("42501")), "{body}");

    // Policies for `authenticated` only, made through storage.create_policy as the owner.
    for command in ["SELECT", "INSERT", "UPDATE", "DELETE"] {
        let using = if command == "INSERT" { "NULL" } else { "'true'" };
        let check = if matches!(command, "INSERT" | "UPDATE") { "'true'" } else { "NULL" };
        fx.owner(&format!(
            "SELECT storage.create_policy('b_{}', 'buckets', '{command}', ARRAY['authenticated']::name[], {using}, {check})",
            command.to_lowercase()
        ))
        .await
        .unwrap_or_else(|e| panic!("create the {command} policy: {e}"));
    }

    let (status, body) = storage("POST", BUCKETS, &secret, Some(json!({ "name": "avatars", "file_size_limit": 1000 }))).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!((&body["name"], &body["public"], &body["file_size_limit"]), (&json!("avatars"), &json!(false), &json!(1000)));
    let (status, body) = storage("POST", BUCKETS, &secret, Some(json!({ "name": "avatars" }))).await;
    assert_eq!(status, 409, "a second avatars: {body}");
    let (status, body) = storage("POST", BUCKETS, &secret, Some(json!({ "name": "../x" }))).await;
    assert_eq!(status, 400, "{body}");
    let (status, list) = storage("GET", BUCKETS, &secret, None).await;
    assert_eq!((status, list.as_array().map(Vec::len)), (200, Some(1)), "{list}");
    assert_eq!(list[0]["name"], "avatars");
    // The public key's role (anon) has no policy, and no DELETE grant: it sees nothing and
    // can't delete.
    assert_eq!(storage("GET", BUCKETS, &public, None).await, (200, json!([])));
    assert_eq!(storage("DELETE", &format!("{BUCKETS}/avatars"), &public, None).await.0, 403);

    let (status, body) = storage("PATCH", &format!("{BUCKETS}/avatars"), &secret, Some(json!({ "public": true }))).await;
    assert_eq!(status, 204, "{body}");
    assert_eq!(storage("GET", BUCKETS, &secret, None).await.1[0]["public"], true);
    assert_eq!(storage("PATCH", &format!("{BUCKETS}/ghost"), &secret, Some(json!({ "public": true }))).await.0, 404);

    // A key whose allow-list excludes the database it asks for: 403.
    let listed = fx.secret_key("st-api-listed", Some(vec![fx.database.clone()])).await;
    assert_eq!(storage("GET", BUCKETS, &listed, None).await.0, 200);
    let (status, body) = storage("GET", &format!("{BUCKETS}?database=postgres"), &listed, None).await;
    assert_eq!((status, &body["error"]["code"]), (403, &json!("database_not_allowed")), "{body}");

    // A bucket holding an object isn't deleted: 409 with the count; empty again, it is.
    let id = list[0]["id"].as_str().unwrap().to_string();
    fx.admin(&format!(
        "BEGIN; SET LOCAL ROLE chelabase_storage;
INSERT INTO storage.upload_tickets (version_id, bucket_id, name, size, etag, expires_at, caller_role)
  VALUES ('00000000-0000-0000-0000-0000000000e1', '{id}', 'a.png', 3, 'e', now() + interval '5 minutes', 'chelabase_storage');
INSERT INTO storage.objects (bucket_id, name, size, etag, version_id) VALUES ('{id}', 'a.png', 3, 'e', '00000000-0000-0000-0000-0000000000e1');
COMMIT;"
    ))
    .await;
    let (status, body) = storage("DELETE", &format!("{BUCKETS}/avatars"), &secret, None).await;
    assert_eq!((status, &body["error"]["code"]), (409, &json!("bucket_not_empty")), "{body}");
    assert!(body["error"]["message"].as_str().unwrap().contains("holds 1 object"), "{body}");
    fx.admin("BEGIN; SET LOCAL ROLE chelabase_storage; DELETE FROM storage.objects; COMMIT;").await;
    assert_eq!(storage("DELETE", &format!("{BUCKETS}/avatars"), &secret, None).await.0, 204);
    assert_eq!(storage("GET", BUCKETS, &secret, None).await, (200, json!([])));
}

/// A database whose `storage` schema is a user's own: every route answers 503
/// `storage_unavailable` (asked before any other storage request reached this database, so
/// nothing is cached).
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_api_user_schema_live() {
    let fx = Arc::new(Fx::provision().await);
    let task = fx.clone();
    let outcome = tokio::spawn(async move {
        let fx = &*task;
        fx.admin("ALTER SCHEMA storage RENAME TO st_api_aside").await;
        fx.owner("CREATE SCHEMA storage; CREATE TABLE storage.buckets (id uuid, name text)")
            .await
            .expect("the owner's own storage schema");
        let secret = fx.secret_key("st-api-user-schema", None).await;
        for (method, path, body) in [
            ("GET", "/storage/v1/bucket", None),
            ("POST", "/storage/v1/bucket", Some(json!({ "name": "x" }))),
            ("PATCH", "/storage/v1/bucket/x", Some(json!({ "public": true }))),
            ("DELETE", "/storage/v1/bucket/x", None),
        ] {
            let (status, body) = storage(method, path, &secret, body).await;
            assert_eq!((status, &body["error"]["code"]), (503, &json!("storage_unavailable")), "{method} {path}: {body}");
        }
    })
    .await;
    fx.teardown().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

// --- m5 phase B: objects through the gateway ------------------------------------------------

/// The object store as the storage service sees it (the stack's `storage` identity).
async fn object_store() -> chelabase_storage::S3Store {
    let state = std::env::var("CHELABASE_STACK_DIR")
        .expect("CHELABASE_STACK_DIR (run through just workbench test)");
    let cfg = chelabase_storage::StorageConfig::from_lookup(|k| {
        Some(match k {
            "CHELABASE_STORAGE_BACKEND" => "seaweedfs".into(),
            "CHELABASE_STORAGE_S3_ENDPOINT" => "http://seaweedfs:8333".into(),
            "CHELABASE_STORAGE_S3_BUCKET" => "chelabase-storage".into(),
            "CHELABASE_STORAGE_S3_ACCESS_KEY_FILE" => format!("{state}/storage-s3/access-key"),
            "CHELABASE_STORAGE_S3_SECRET_KEY_FILE" => format!("{state}/storage-s3/secret-key"),
            _ => return None,
        })
    })
    .expect("the object store's settings");
    chelabase_storage::S3Store::new(&cfg).await
}

/// An object call with data key `key`: `(status, headers, body)`; status 0 when the client's
/// own body failed.
async fn object(
    method: &str,
    path: &str,
    key: &str,
    body: Option<reqwest::Body>,
    headers: &[(&str, &str)],
) -> (u16, reqwest::header::HeaderMap, bytes::Bytes) {
    let mut req = reqwest::Client::new()
        .request(method.parse().unwrap(), format!("{}{path}", gateway()))
        .bearer_auth(key)
        .timeout(Duration::from_secs(600));
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    if let Some(body) = body {
        req = req.body(body);
    }
    match req.send().await {
        Ok(res) => {
            let (status, headers) = (res.status().as_u16(), res.headers().clone());
            (status, headers, res.bytes().await.unwrap_or_default())
        }
        Err(_) => (0, Default::default(), bytes::Bytes::new()),
    }
}

/// `total` bytes of `fill` in 64 KiB chunks, `pause` apart; with `fail_after`, an error once
/// that many bytes are out.
fn paced_body(total: usize, fill: u8, pause: Duration, fail_after: Option<usize>) -> reqwest::Body {
    const CHUNK: usize = 64 * 1024;
    let stream = futures_util::stream::unfold(0usize, move |sent| async move {
        if sent >= total {
            return None;
        }
        if fail_after.is_some_and(|f| sent >= f) {
            return Some((Err(std::io::Error::other("the client's body failed")), total));
        }
        if !pause.is_zero() {
            tokio::time::sleep(pause).await;
        }
        let n = CHUNK.min(total - sent);
        Some((Ok(bytes::Bytes::from(vec![fill; n])), sent + n))
    });
    reqwest::Body::wrap_stream(stream)
}

fn json_of(b: &bytes::Bytes) -> Value {
    serde_json::from_slice(b).unwrap_or(Value::Null)
}

const JSON: (&str, &str) = ("content-type", "application/json");

impl Fx {
    /// The blob keys under this database's prefix, without it.
    async fn blobs(&self, store: &chelabase_storage::S3Store) -> Vec<String> {
        use chelabase_storage::ObjectStore;
        let prefix = format!("{}/{}/", self.id, self.database);
        store
            .list(&prefix)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.key[prefix.len()..].to_string())
            .collect()
    }

    /// A bucket, made as storage itself.
    async fn bucket(&self, name: &str) {
        self.admin(&format!(
            "BEGIN; SET LOCAL ROLE chelabase_storage; INSERT INTO storage.buckets (name) VALUES ('{name}'); COMMIT;"
        ))
        .await;
    }
}

#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_api_objects_live() {
    let fx = Arc::new(Fx::provision().await);
    let task = fx.clone();
    let outcome = tokio::spawn(async move { storage_api_objects(&task).await }).await;
    fx.teardown().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

async fn storage_api_objects(fx: &Fx) {
    use chelabase_storage::ObjectStore;
    let store = object_store().await;
    let secret = fx.secret_key("st-obj", None).await;
    let up = |name: &str| format!("/storage/v1/object/files/{name}");
    fx.bucket("files").await;
    fx.bucket("other").await;
    fx.bucket("secret").await;
    // Object routes need the bucket visible to the caller: every bucket but `secret`.
    let visible = "'name <> ''secret'''";
    fx.owner(&format!("SELECT storage.create_policy('b_select', 'buckets', 'SELECT', ARRAY['authenticated']::name[], {visible}, NULL)"))
        .await
        .unwrap();

    // A bucket the caller can't see and one that doesn't exist answer the same bytes.
    let hidden = object("POST", "/storage/v1/object/secret/a.txt", &secret, Some(b"x".to_vec().into()), &[]).await;
    let missing = object("POST", "/storage/v1/object/ghost/a.txt", &secret, Some(b"x".to_vec().into()), &[]).await;
    assert_eq!((hidden.0, &hidden.2), (missing.0, &missing.2));
    assert_eq!((hidden.0, json_of(&hidden.2)["error"]["code"].clone()), (403, json!("not_allowed")));
    let hidden = object("GET", "/storage/v1/object/secret/a.txt", &secret, None, &[]).await;
    let missing_get = object("GET", "/storage/v1/object/ghost/a.txt", &secret, None, &[]).await;
    assert_eq!((hidden.0, &hidden.2), (missing_get.0, &missing_get.2));

    // No object policy: the same 403, and nothing reached the object store.
    let (status, _, b) = object("POST", &up("a.txt"), &secret, Some(b"hello".to_vec().into()), &[]).await;
    assert_eq!(status, 403, "{}", json_of(&b));
    assert_eq!(b, missing.2, "an RLS refusal answers as a hidden bucket");
    assert!(fx.blobs(&store).await.is_empty());

    // Policies: `hidden/…` can't be read; 666 bytes and text/html can't be written.
    let check = "'(size IS NULL OR size <> 666) AND content_type <> ''text/html'''";
    for (name, command, using, check) in [
        ("o_select", "SELECT", "'name NOT LIKE ''hidden/%'''", "NULL"),
        ("o_insert", "INSERT", "NULL", check),
        ("o_update", "UPDATE", "'true'", check),
        ("o_delete", "DELETE", "'true'", "NULL"),
    ] {
        fx.owner(&format!(
            "SELECT storage.create_policy('{name}', 'objects', '{command}', ARRAY['authenticated']::name[], {using}, {check})"
        ))
        .await
        .unwrap_or_else(|e| panic!("{name}: {e}"));
    }

    // The happy path: the row, the blob at its version's key, no staging left.
    let (status, _, b) = object(
        "POST",
        &up("docs/a.txt"),
        &secret,
        Some(b"hello world".to_vec().into()),
        &[("content-type", "text/plain")],
    )
    .await;
    assert_eq!(status, 200, "{}", json_of(&b));
    let made = json_of(&b);
    let version = made["version_id"].as_str().unwrap().to_string();
    assert_eq!(made["size"], 11);
    assert_eq!(
        fx.value("SELECT size || ' ' || content_type || ' ' || version_id FROM storage.objects WHERE name = 'docs/a.txt'")
            .await,
        format!("11 text/plain {version}")
    );
    assert_eq!(fx.blobs(&store).await, std::slice::from_ref(&version));
    let etag = made["etag"].as_str().unwrap().to_string();
    let blob = store.head(&format!("{}/{}/{version}", fx.id, fx.database)).await.unwrap();
    assert_eq!(blob.etag.trim_matches('"'), etag);

    // Range and conditional reads, the download name, HEAD.
    let (status, h, b) = object("GET", &up("docs/a.txt"), &secret, None, &[("range", "bytes=0-4")]).await;
    assert_eq!((status, &b[..]), (206, &b"hello"[..]));
    assert_eq!(h["content-range"], "bytes 0-4/11");
    let tag = h["etag"].to_str().unwrap().to_string();
    assert_eq!(object("GET", &up("docs/a.txt"), &secret, None, &[("if-none-match", &tag)]).await.0, 304);
    let (status, h, b) = object("GET", &format!("{}?download=", up("docs/a.txt")), &secret, None, &[]).await;
    assert_eq!((status, &b[..]), (200, &b"hello world"[..]));
    assert!(h["content-disposition"].to_str().unwrap().starts_with("attachment; filename=\"a.txt\""));
    assert_eq!(object("HEAD", &up("docs/a.txt"), &secret, None, &[]).await.0, 200);

    // A policy on size and content type: 403 whether declared or known only after the body,
    // and no blob stays.
    let before = fx.blobs(&store).await;
    assert_eq!(object("POST", &up("big.bin"), &secret, Some(vec![0u8; 666].into()), &[]).await.0, 403, "declared 666 bytes");
    let chunked = Some(paced_body(666, 1, Duration::ZERO, None));
    assert_eq!(object("POST", &up("big2.bin"), &secret, chunked, &[]).await.0, 403, "a chunked 666 bytes");
    let html = [("content-type", "text/html")];
    assert_eq!(object("POST", &up("x.html"), &secret, Some(b"<p>".to_vec().into()), &html).await.0, 403, "text/html");
    assert_eq!(fx.blobs(&store).await, before);

    // A body that fails midway leaves no row and no blob.
    let broken = Some(paced_body(1 << 20, 2, Duration::ZERO, Some(256 * 1024)));
    assert_ne!(object("POST", &up("broken.bin"), &secret, broken, &[]).await.0, 200);
    assert_eq!(fx.value("SELECT count(*) FROM storage.objects WHERE name = 'broken.bin'").await, "0");
    assert_eq!(fx.blobs(&store).await, before);

    // Names: `..`, control characters and 1025 bytes are 400.
    for bad in ["a/%2E%2E/x", "a%01b", &"n".repeat(1025)] {
        assert_eq!(object("POST", &up(bad), &secret, Some(b"x".to_vec().into()), &[]).await.0, 400, "{bad}");
    }

    // A hidden object is 404 on read, head, copy and move.
    assert_eq!(object("POST", &up("hidden/h.txt"), &secret, Some(b"h".to_vec().into()), &[]).await.0, 200);
    assert_eq!(object("GET", &up("hidden/h.txt"), &secret, None, &[]).await.0, 404);
    assert_eq!(object("HEAD", &up("hidden/h.txt"), &secret, None, &[]).await.0, 404);
    for route in ["copy", "move"] {
        let body = json!({ "bucket": "files", "from": "hidden/h.txt", "to": format!("seen-{route}.txt") });
        let path = format!("/storage/v1/object/{route}");
        assert_eq!(object("POST", &path, &secret, Some(body.to_string().into()), &[JSON]).await.0, 404, "{route}");
    }

    // Copy (a new version and blob), a cross-bucket move (metadata only), then a download.
    let body = json!({ "bucket": "files", "from": "docs/a.txt", "to": "docs/b.txt" });
    let (status, _, b) = object("POST", "/storage/v1/object/copy", &secret, Some(body.to_string().into()), &[JSON]).await;
    assert_eq!(status, 200, "{}", json_of(&b));
    let body = json!({ "bucket": "files", "from": "docs/b.txt", "to": "moved/b.txt", "to_bucket": "other" });
    let (status, _, b) = object("POST", "/storage/v1/object/move", &secret, Some(body.to_string().into()), &[JSON]).await;
    assert_eq!(status, 204, "{}", json_of(&b));
    let (status, _, b) = object("GET", "/storage/v1/object/other/moved/b.txt", &secret, None, &[]).await;
    assert_eq!((status, &b[..]), (200, &b"hello world"[..]));

    // Without upsert an existing name is 409; with it, the object is replaced and its
    // updated_at advances (schema 5's trigger).
    assert_eq!(object("POST", &up("docs/a.txt"), &secret, Some(b"x".to_vec().into()), &[]).await.0, 409);
    let stamped = "SELECT updated_at::text FROM storage.objects WHERE name = 'docs/a.txt'";
    let before_upsert = fx.value(stamped).await;
    let upsert = [("x-upsert", "true")];
    let (status, _, b) = object("PUT", &up("docs/a.txt"), &secret, Some(b"v2".to_vec().into()), &upsert).await;
    assert_eq!(status, 200, "{}", json_of(&b));
    assert_eq!(&object("GET", &up("docs/a.txt"), &secret, None, &[]).await.2[..], &b"v2"[..]);
    let after_upsert = fx.value(stamped).await;
    let advanced = fx.value(&format!("SELECT '{after_upsert}'::timestamptz > '{before_upsert}'::timestamptz")).await;
    assert_eq!(advanced, "t", "updated_at {before_upsert} -> {after_upsert}");

    // Racing upserts of one existing name: one wins, the other is 409, and the winner's row and
    // blob stay.
    assert_eq!(object("POST", &up("race.bin"), &secret, Some(b"0".to_vec().into()), &[]).await.0, 200);
    let slow = || Some(paced_body(512 * 1024, 7, Duration::from_millis(100), None));
    let race = up("race.bin");
    let (a, b) = tokio::join!(
        object("PUT", &race, &secret, slow(), &upsert),
        object("PUT", &race, &secret, slow(), &upsert),
    );
    let mut statuses = [a.0, b.0];
    statuses.sort();
    assert_eq!(statuses, [200, 409], "{} / {}", json_of(&a.2), json_of(&b.2));
    let winner = if a.0 == 200 { json_of(&a.2) } else { json_of(&b.2) };
    let version = winner["version_id"].as_str().unwrap().to_string();
    assert_eq!(fx.value("SELECT version_id FROM storage.objects WHERE name = 'race.bin'").await, version);
    let blobs = fx.blobs(&store).await;
    assert!(blobs.contains(&version), "{blobs:?}");
    assert!(!blobs.iter().any(|k| k.starts_with("staging/")), "{blobs:?}");

    // A row carrying another database's version, written past the triggers by a superuser (the
    // owner can't: the trigger wants a ticket only storage writes): its blob lives under that
    // database's prefix, so the download is 404.
    let foreign = Uuid::new_v4();
    store
        .put(&format!("{}/{}/{foreign}", Uuid::new_v4(), fx.database), bytes::Bytes::from_static(b"x"), Default::default())
        .await
        .unwrap();
    fx.admin(&format!(
        "BEGIN; SET LOCAL session_replication_role = replica;
INSERT INTO storage.objects (bucket_id, name, size, etag, version_id) SELECT id, 'foreign.bin', 1, 'e', '{foreign}' FROM storage.buckets WHERE name = 'files';
COMMIT;"
    ))
    .await;
    let err = fx.owner_err(&format!(
        "UPDATE storage.objects SET version_id = '{foreign}' WHERE name = 'race.bin'"
    ))
    .await;
    assert!(err.contains("CB003"), "the owner copied a version in: {err}");
    assert_eq!(object("GET", &up("foreign.bin"), &secret, None, &[]).await.0, 404);

    // Delete one and many: rows only (the blobs wait for the sweeper).
    assert_eq!(object("DELETE", &up("docs/a.txt"), &secret, None, &[]).await.0, 204);
    assert_eq!(object("DELETE", &up("docs/a.txt"), &secret, None, &[]).await.0, 404);
    let body = json!({ "names": ["race.bin", "hidden/h.txt", "nope"] });
    let (status, _, b) = object("DELETE", "/storage/v1/object/files", &secret, Some(body.to_string().into()), &[JSON]).await;
    assert_eq!(status, 200, "{}", json_of(&b));
    let mut deleted: Vec<String> = serde_json::from_value(json_of(&b)["deleted"].clone()).unwrap();
    deleted.sort();
    assert_eq!(deleted, ["race.bin"], "a hidden object isn't deleted either");
}

#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_api_list_live() {
    let fx = Arc::new(Fx::provision().await);
    let task = fx.clone();
    let outcome = tokio::spawn(async move { storage_api_list(&task).await }).await;
    fx.teardown().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

async fn storage_api_list(fx: &Fx) {
    let secret = fx.secret_key("st-list", None).await;
    fx.bucket("files").await;
    fx.owner("SELECT storage.create_policy('b_select', 'buckets', 'SELECT', ARRAY['authenticated']::name[], 'true', NULL)")
        .await
        .unwrap();
    fx.owner("SELECT storage.create_policy('o_all', 'objects', 'ALL', ARRAY['authenticated']::name[], 'true', 'true')")
        .await
        .unwrap();
    for name in ["a/1", "a/2", "b/1", "c", "d"] {
        let path = format!("/storage/v1/object/files/{name}");
        let (status, _, b) = object("POST", &path, &secret, Some(b"x".to_vec().into()), &[]).await;
        assert_eq!(status, 200, "{name}: {}", json_of(&b));
    }
    let list = async |body: Value| {
        let path = "/storage/v1/object/list/files";
        let (status, _, b) = object("POST", path, &secret, Some(body.to_string().into()), &[JSON]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        json_of(&b)
    };
    let names = |page: &Value| -> Vec<(String, bool)> {
        page["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| (i["name"].as_str().unwrap().to_string(), i["folder"].as_bool().unwrap()))
            .collect()
    };
    let item = |n: &str, folder: bool| (n.to_string(), folder);
    // Folders and files once each, paged.
    let first = list(json!({ "delimiter": "/", "limit": 2 })).await;
    assert_eq!(names(&first), [item("a/", true), item("b/", true)]);
    let second = list(json!({ "delimiter": "/", "limit": 2, "cursor": first["next_cursor"] })).await;
    assert_eq!(names(&second), [item("c", false), item("d", false)]);
    assert!(second["next_cursor"].is_null(), "{second}");
    let folder = list(json!({ "prefix": "a/", "delimiter": "/" })).await;
    assert_eq!(names(&folder), [item("a/1", false), item("a/2", false)]);
    assert_eq!(names(&list(json!({ "search": "B/" })).await), [item("b/1", false)]);
    let desc = list(json!({ "order": "desc", "limit": 1 })).await;
    assert_eq!(names(&desc), [item("d", false)]);
    let next = list(json!({ "order": "desc", "limit": 1, "cursor": desc["next_cursor"] })).await;
    assert_eq!(names(&next), [item("c", false)]);
    // A key whose list excludes the database asked for: 403.
    let listed = fx.secret_key("st-list-listed", Some(vec![fx.database.clone()])).await;
    let path = "/storage/v1/object/list/files?database=postgres";
    assert_eq!(object("POST", path, &listed, Some("{}".into()), &[JSON]).await.0, 403);
}

/// 100 uploads stalled mid-body hold no database connection: the data API and storage's own
/// routes on the branch answer as usual meanwhile. Then a 64 MiB upload paced over about 80 s
/// (past the gateway's 60 s default read timeout) completes through the gateway.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service; about two minutes"]
async fn storage_api_slow_uploads_live() {
    let fx = Arc::new(Fx::provision().await);
    let task = fx.clone();
    let outcome = tokio::spawn(async move { storage_api_slow_uploads(&task).await }).await;
    fx.teardown().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

async fn storage_api_slow_uploads(fx: &Fx) {
    let secret = fx.secret_key("st-slow", None).await;
    fx.bucket("files").await;
    fx.owner("SELECT storage.create_policy('b_select', 'buckets', 'SELECT', ARRAY['authenticated']::name[], 'true', NULL)")
        .await
        .unwrap();
    fx.owner("SELECT storage.create_policy('o_all', 'objects', 'ALL', ARRAY['authenticated']::name[], 'true', 'true')")
        .await
        .unwrap();
    let stalled: Vec<_> = (0..100)
        .map(|i| {
            let secret = secret.clone();
            tokio::spawn(async move {
                // One byte, then nothing.
                let body = futures_util::stream::once(async { Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"x")) })
                    .chain(futures_util::stream::pending());
                let path = format!("/storage/v1/object/files/stalled-{i}");
                object("POST", &path, &secret, Some(reqwest::Body::wrap_stream(body)), &[]).await
            })
        })
        .collect();
    // Every stalled upload passes its pre-check (one at a time within storage's budget) and
    // then waits on its body.
    tokio::time::sleep(Duration::from_secs(15)).await;
    for _ in 0..5 {
        let started = Instant::now();
        let res = reqwest::Client::new()
            .get(format!("{}/data/v1/schema", gateway()))
            .bearer_auth(&secret)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .expect("the data API answers");
        assert_eq!(res.status().as_u16(), 200);
        assert!(started.elapsed() < Duration::from_secs(3), "the data API took {:?}", started.elapsed());
        assert_eq!(storage("GET", "/storage/v1/bucket", &secret, None).await.0, 200);
    }
    for task in stalled {
        task.abort();
    }

    let started = Instant::now();
    let total = 64usize << 20;
    // 64 KiB every 80 ms: about 0.8 MiB/s.
    let body = Some(paced_body(total, 3, Duration::from_millis(80), None));
    let (status, _, b) = object("POST", "/storage/v1/object/files/big.bin", &secret, body, &[]).await;
    let took = started.elapsed();
    assert_eq!(status, 200, "{}", json_of(&b));
    assert_eq!(json_of(&b)["size"], total);
    assert!(took > Duration::from_secs(60), "the upload must outlast the default 60 s timeout: {took:?}");
    println!("64 MiB paced upload took {took:?}");
}

// --- Mixed versions: a newer schema is never downgraded --------------------------------------

#[tokio::test]
#[ignore = "needs the workbench stack and CHELABASE_COMPUTE_IMAGE"]
async fn storage_schema_versions_live() {
    let fx = Arc::new(Fx::provision().await);
    let task = fx.clone();
    let outcome = tokio::spawn(async move { versions(&task).await }).await;
    fx.teardown().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

async fn versions(fx: &Fx) {
    let stamp = "SELECT obj_description('storage'::regnamespace, 'pg_namespace')";
    // Newer (as a later worker would leave it, with something this worker would heal):
    // nothing in `storage` changes, the check is healthy, the role is still locked down.
    fx.admin(
        "COMMENT ON SCHEMA storage IS 'chelabase storage schema 99';
         BEGIN; SET LOCAL ROLE chelabase_admin; CREATE FUNCTION storage.from_v99() RETURNS int LANGUAGE sql AS $$ SELECT 99 $$; COMMIT;",
    )
    .await;
    fx.admin(&format!(
        "ALTER ROLE chelabase_storage LOGIN PASSWORD '{TAMPER_PASSWORD}'"
    ))
    .await;
    let before = fx.value(SHAPE_SQL).await;
    assert_eq!(
        last_line(&fx.admin(&schema_check_sql("public", Some(&fx.verifier().await))).await),
        "f",
        "the role's drift still shows"
    );
    bootstrap(fx).await;
    assert_eq!(
        fx.value(SHAPE_SQL).await,
        before,
        "a newer storage schema was changed"
    );
    assert_eq!(fx.value(stamp).await, "chelabase storage schema 99");
    assert_eq!(fx.locked_down().await, LOCKED_DOWN);
    assert_eq!(
        last_line(&fx.admin(&schema_check_sql("public", Some(&fx.verifier().await))).await),
        "t",
        "a newer schema is healthy here"
    );

    // Older and unparseable stamps are installed over.
    for old in ["chelabase storage schema 3", "chelabase storage schema x"] {
        fx.admin(&format!("COMMENT ON SCHEMA storage IS '{old}'"))
            .await;
        assert_eq!(
            last_line(&fx.admin(&schema_check_sql("public", Some(&fx.verifier().await))).await),
            "f",
            "{old}"
        );
        bootstrap(fx).await;
        assert_eq!(
            fx.value(stamp).await,
            storage_schema::STORAGE_SCHEMA_STAMP,
            "{old}"
        );
        assert_eq!(
            fx.value("SELECT to_regprocedure('storage.from_v99()') IS NULL")
                .await,
            "t",
            "{old}: healed"
        );
        assert_eq!(
            last_line(&fx.admin(&schema_check_sql("public", Some(&fx.verifier().await))).await),
            "t",
            "{old}"
        );
    }
}
