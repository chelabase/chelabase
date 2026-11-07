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
        "buckets chelabase_admin true true,objects chelabase_admin true true,upload_sessions chelabase_admin false false,upload_tickets chelabase_admin false false"
    );
    // m7: upload sessions are storage's alone: the owner (`pg_write_all_data`) writes none,
    // `authenticated` neither reads nor records a part; storage itself opens one.
    let open = "INSERT INTO storage.upload_sessions (id, bucket_id, name, version_id, upload_id, content_type, upsert, datoid, caller_role, owner, key_id, parts, expires_at) VALUES (gen_random_uuid(), gen_random_uuid(), 'x', gen_random_uuid(), 'u', 'a/b', false, 1, 'authenticated', NULL, gen_random_uuid(), '{}', clock_timestamp() + interval '1 hour')";
    let err = fx.owner_err(open).await;
    assert!(err.contains("CB004"), "the owner opens a session: {err}");
    assert_eq!(fx.code("authenticated", "SELECT * FROM storage.upload_sessions").await, "42501");
    assert_eq!(fx.code("authenticated", "SELECT storage.record_part(gen_random_uuid(), 1, 1, 'e')").await, "42501");
    assert_eq!(fx.code("chelabase_storage", &open.replace("'1 hour'", "'25 hours'")).await, "22023");
    assert_eq!(fx.code("chelabase_storage", open).await, "ok");
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
    // The proxy is dialled at CHELABASE_PROXY_HOSTADDR when set; the TLS host name stays <ep>.<domain>.
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
    assert_eq!(fx.value("SELECT string_agg(tgname || '=' || tgenabled::text, ',' ORDER BY tgname) FROM pg_trigger WHERE tgrelid IN ('storage.objects'::regclass, 'storage.upload_tickets'::regclass) AND NOT tgisinternal").await, "freeze_objects=A,guard_tickets=A,protect_objects=A,touch_objects=A");
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
    // N5: no count (it would include objects the caller can't see).
    assert!(body["error"]["message"].as_str().unwrap().contains("still holds objects"), "{body}");
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
            ("POST", "/storage/v1/object/x/a.txt", Some(json!({}))),
            ("PUT", "/storage/v1/object/x/a.txt", Some(json!({}))),
            ("GET", "/storage/v1/object/x/a.txt", None),
            ("HEAD", "/storage/v1/object/x/a.txt", None),
            ("DELETE", "/storage/v1/object/x/a.txt", None),
            ("DELETE", "/storage/v1/object/x", Some(json!({ "names": ["a.txt"] }))),
            ("POST", "/storage/v1/object/list/x", Some(json!({}))),
            ("POST", "/storage/v1/object/copy", Some(json!({ "bucket": "x", "from": "a", "to": "b" }))),
            ("POST", "/storage/v1/object/move", Some(json!({ "bucket": "x", "from": "a", "to": "b", "to_bucket": "y" }))),
        ] {
            // A fresh database: nothing was cached, so every route asks the schema check.
            let (status, body) = storage(method, path, &secret, body).await;
            let code = if method == "HEAD" { json!("storage_unavailable") } else { body["error"]["code"].clone() };
            assert_eq!((status, &code), (503, &json!("storage_unavailable")), "{method} {path}: {body}");
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
    object_within(method, path, key, body, headers, Duration::from_secs(600)).await
}

/// [`object`] with its own client timeout (the GiB upload outlives the usual 600 s).
async fn object_within(
    method: &str,
    path: &str,
    key: &str,
    body: Option<reqwest::Body>,
    headers: &[(&str, &str)],
    timeout: Duration,
) -> (u16, reqwest::header::HeaderMap, bytes::Bytes) {
    let mut req = reqwest::Client::new()
        .request(method.parse().unwrap(), format!("{}{path}", gateway()))
        .bearer_auth(key)
        .timeout(timeout);
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
    /// This database's blob prefix: `{database_id}/{datoid}/`.
    async fn prefix(&self) -> String {
        let oid = self.value("SELECT oid FROM pg_database WHERE datname = current_database()").await;
        format!("{}/{oid}/", self.id)
    }

    async fn blobs(&self, store: &chelabase_storage::S3Store) -> Vec<String> {
        use chelabase_storage::ObjectStore;
        let prefix = self.prefix().await;
        store
            .list(&prefix)
            .await
            .unwrap()
            .into_iter()
            .map(|m| m.key[prefix.len()..].to_string())
            .collect()
    }

    /// Deletes every blob under this database's prefix (the sweeper's job, m8: the tests
    /// leave nothing behind in the shared object store).
    async fn purge_blobs(&self, store: &chelabase_storage::S3Store) {
        use chelabase_storage::ObjectStore;
        let prefix = self.prefix().await;
        for blob in store.list(&prefix).await.unwrap() {
            store.delete(&blob.key).await.unwrap();
        }
        assert!(self.blobs(store).await.is_empty());
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
    // Pass or fail, nothing stays in the shared object store.
    fx.purge_blobs(&object_store().await).await;
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
        &up("notes/a.txt"),
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
        fx.value("SELECT size || ' ' || content_type || ' ' || version_id FROM storage.objects WHERE name = 'notes/a.txt'")
            .await,
        format!("11 text/plain {version}")
    );
    assert_eq!(fx.blobs(&store).await, std::slice::from_ref(&version));
    let etag = made["etag"].as_str().unwrap().to_string();
    let blob = store.head(&format!("{}{version}", fx.prefix().await)).await.unwrap();
    assert_eq!(blob.etag.trim_matches('"'), etag);

    // Range and conditional reads, the download name, HEAD.
    let (status, h, b) = object("GET", &up("notes/a.txt"), &secret, None, &[("range", "bytes=0-4")]).await;
    assert_eq!((status, &b[..]), (206, &b"hello"[..]));
    assert_eq!(h["content-range"], "bytes 0-4/11");
    let tag = h["etag"].to_str().unwrap().to_string();
    assert_eq!(object("GET", &up("notes/a.txt"), &secret, None, &[("if-none-match", &tag)]).await.0, 304);
    let (status, h, b) = object("GET", &format!("{}?download=", up("notes/a.txt")), &secret, None, &[]).await;
    assert_eq!((status, &b[..]), (200, &b"hello world"[..]));
    assert!(h["content-disposition"].to_str().unwrap().starts_with("attachment; filename=\"a.txt\""));
    assert_eq!(object("HEAD", &up("notes/a.txt"), &secret, None, &[]).await.0, 200);
    assert_eq!(h["x-content-type-options"], "nosniff");
    // N1: a zero-byte object with a range: 200, empty.
    assert_eq!(object("POST", &up("empty"), &secret, Some(Vec::new().into()), &[]).await.0, 200);
    let (status, _, b) = object("GET", &up("empty"), &secret, None, &[("range", "bytes=0-")]).await;
    assert_eq!((status, b.len()), (200, 0));

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

    // Names: `..`, control characters and 1025 bytes are 400. The `..` segment goes with an
    // encoded `/` around it: a client normalizes a bare `/%2E%2E/` away before sending.
    for bad in ["a%2F..%2Fx", "..%2Fx", "a%2F.%2Fx", "%EF%BC%8E%2Fx", "a%01b", &"n".repeat(1025)] {
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
    let body = json!({ "bucket": "files", "from": "notes/a.txt", "to": "notes/b.txt" });
    let (status, _, b) = object("POST", "/storage/v1/object/copy", &secret, Some(body.to_string().into()), &[JSON]).await;
    assert_eq!(status, 200, "{}", json_of(&b));
    let body = json!({ "bucket": "files", "from": "notes/b.txt", "to": "moved/b.txt", "to_bucket": "other" });
    let (status, _, b) = object("POST", "/storage/v1/object/move", &secret, Some(body.to_string().into()), &[JSON]).await;
    assert_eq!(status, 204, "{}", json_of(&b));
    let (status, _, b) = object("GET", "/storage/v1/object/other/moved/b.txt", &secret, None, &[]).await;
    assert_eq!((status, &b[..]), (200, &b"hello world"[..]));

    // Without upsert an existing name is 409; with it, the object is replaced and its
    // updated_at advances (schema 5's trigger).
    assert_eq!(object("POST", &up("notes/a.txt"), &secret, Some(b"x".to_vec().into()), &[]).await.0, 409);
    let stamped = "SELECT updated_at::text FROM storage.objects WHERE name = 'notes/a.txt'";
    let before_upsert = fx.value(stamped).await;
    let upsert = [("x-upsert", "true")];
    let (status, _, b) = object("PUT", &up("notes/a.txt"), &secret, Some(b"v2".to_vec().into()), &upsert).await;
    assert_eq!(status, 200, "{}", json_of(&b));
    assert_eq!(&object("GET", &up("notes/a.txt"), &secret, None, &[]).await.2[..], &b"v2"[..]);
    let after_upsert = fx.value(stamped).await;
    let advanced = fx.value(&format!("SELECT '{after_upsert}'::timestamptz > '{before_upsert}'::timestamptz")).await;
    assert_eq!(advanced, "t", "updated_at {before_upsert} -> {after_upsert}");

    // Racing upserts of one existing name: one wins, the other is 409, and the winner's row and
    // blob stay.
    assert_eq!(object("POST", &up("race.bin"), &secret, Some(b"0".to_vec().into()), &[]).await.0, 200);
    let before_race = fx.blobs(&store).await;
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
    // Exactly one blob was added, the winner's: the loser's was deleted.
    let mut added: Vec<String> = fx.blobs(&store).await.into_iter().filter(|k| !before_race.contains(k)).collect();
    added.sort();
    assert_eq!(added, std::slice::from_ref(&version), "the race left a stray blob");

    // A row carrying another database's version (the owner can't write one: the trigger wants
    // a ticket only storage writes, and it fires even with session_replication_role = replica;
    // here written with a ticket as chelabase_storage itself): its blob lives under that
    // database's prefix, so the download is 404.
    let foreign = Uuid::new_v4();
    fx.admin(&format!(
        "BEGIN; SET LOCAL ROLE chelabase_storage;
INSERT INTO storage.upload_tickets (version_id, bucket_id, name, size, etag, expires_at, caller_role)
  SELECT '{foreign}', id, 'foreign.bin', 1, 'e', now() + interval '5 minutes', 'chelabase_storage' FROM storage.buckets WHERE name = 'files';
INSERT INTO storage.objects (bucket_id, name, size, etag, version_id) SELECT id, 'foreign.bin', 1, 'e', '{foreign}' FROM storage.buckets WHERE name = 'files';
COMMIT;"
    ))
    .await;
    let err = fx.owner_err(&format!(
        "UPDATE storage.objects SET version_id = '{foreign}' WHERE name = 'race.bin'"
    ))
    .await;
    assert!(err.contains("CB003"), "the owner copied a version in: {err}");
    // The blob exists, under the other database's prefix.
    let foreign_key = format!("{}/{}{foreign}", Uuid::new_v4(), fx.prefix().await.split_once('/').unwrap().1);
    store.put(&foreign_key, bytes::Bytes::from_static(b"x"), Default::default()).await.unwrap();
    let got = object("GET", &up("foreign.bin"), &secret, None, &[]).await.0;
    store.delete(&foreign_key).await.unwrap();
    assert_eq!(got, 404);

    // Delete one and many: rows only (the blobs wait for the sweeper).
    assert_eq!(object("DELETE", &up("notes/a.txt"), &secret, None, &[]).await.0, 204);
    assert_eq!(object("DELETE", &up("notes/a.txt"), &secret, None, &[]).await.0, 404);
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
    // Pass or fail, nothing stays in the shared object store.
    fx.purge_blobs(&object_store().await).await;
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
    // Pass or fail, nothing stays in the shared object store.
    fx.purge_blobs(&object_store().await).await;
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
        println!("with 100 stalled uploads: /data/v1/schema took {:?}", started.elapsed());
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
    let store = object_store().await;
    // The 100 stalled uploads never reached the store (each was under one part), and the
    // 64 MiB blob is its only one.
    assert_eq!(fx.blobs(&store).await.len(), 1, "{:?}", fx.blobs(&store).await);
}

// --- m5 fix round (white-room audit) ------------------------------------------------------

/// A live test's database with buckets `files` (and `other`, `capped` at 1000 bytes) every
/// `authenticated` caller sees, and objects policies from `objects` (`(command, using, check)`).
async fn objects_fixture(fx: &Fx, objects: &[(&str, &str, &str)]) -> String {
    let secret = fx.secret_key("st-fix", None).await;
    fx.bucket("files").await;
    fx.bucket("other").await;
    fx.admin("BEGIN; SET LOCAL ROLE chelabase_storage; INSERT INTO storage.buckets (name, file_size_limit) VALUES ('capped', 1000); COMMIT;")
        .await;
    fx.owner("SELECT storage.create_policy('b_select', 'buckets', 'SELECT', ARRAY['authenticated']::name[], 'true', NULL)")
        .await
        .unwrap();
    for (i, (command, using, check)) in objects.iter().enumerate() {
        fx.owner(&format!(
            "SELECT storage.create_policy('o_{i}', 'objects', '{command}', ARRAY['authenticated']::name[], {using}, {check})"
        ))
        .await
        .unwrap_or_else(|e| panic!("{command}: {e}"));
    }
    secret
}

/// Runs `f` on a fresh database; its blobs are purged and the database deleted, pass or fail.
async fn with_fx(f: fn(Arc<Fx>) -> futures_util::future::BoxFuture<'static, ()>) {
    let fx = Arc::new(Fx::provision().await);
    let outcome = tokio::spawn(f(fx.clone())).await;
    fx.purge_blobs(&object_store().await).await;
    fx.teardown().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

fn tickets_sql(version: &str) -> String {
    format!("SELECT count(*) FROM storage.upload_tickets WHERE version_id = '{version}'")
}

/// B1: a refused cross-bucket move mints no ticket (the caller's UPDATE decides first in a
/// rolled-back savepoint), so a later allowed move of the object, by another key, works.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_api_move_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'"), ("DELETE", "'true'", "NULL")]).await;
        let (status, _, b) = object("POST", "/storage/v1/object/files/m.txt", &secret, Some(b"m".to_vec().into()), &[]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        let version = json_of(&b)["version_id"].as_str().unwrap().to_string();
        let body = json!({ "bucket": "files", "from": "m.txt", "to": "m.txt", "to_bucket": "other" }).to_string();
        // No UPDATE policy: the move is refused before any ticket.
        let (status, _, b) = object("POST", "/storage/v1/object/move", &secret, Some(body.clone().into()), &[JSON]).await;
        assert_eq!(status, 404, "{}", json_of(&b));
        assert_eq!(fx.value(&tickets_sql(&version)).await, "0", "a refused move left a ticket");
        fx.owner("SELECT storage.create_policy('o_update', 'objects', 'UPDATE', ARRAY['authenticated']::name[], 'true', 'true')")
            .await
            .unwrap();
        let other = fx.secret_key("st-fix-mover", None).await;
        let (status, _, b) = object("POST", "/storage/v1/object/move", &other, Some(body.into()), &[JSON]).await;
        assert_eq!(status, 204, "{}", json_of(&b));
        assert_eq!(fx.value(&tickets_sql(&version)).await, "0");
        assert_eq!(&object("GET", "/storage/v1/object/other/m.txt", &other, None, &[]).await.2[..], b"m");
    }))
    .await;
}

/// SF2 (AC 5): the INSERT policy dropped while the body streams: the final write obeys it
/// (403), and no row, ticket or blob remains.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_api_policy_change_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'")]).await;
        let store = object_store().await;
        // Warm: the key, schema, owner-level checks and the pools are hot.
        assert_eq!(object("POST", "/storage/v1/object/files/warm.bin", &secret, Some(b"w".to_vec().into()), &[]).await.0, 200);
        let warm = fx.blobs(&store).await;
        // 12 MiB at about 1.25 MiB/s; the policy goes once the client has handed over 8 MiB, far
        // more than the socket buffers hold, so the server is mid-body: past its pre-check.
        let total = 12 << 20;
        let sent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let past = Arc::new(tokio::sync::Notify::new());
        let body = Some(counted_body(total, Duration::from_millis(50), sent.clone(), 8 << 20, past.clone()));
        let (upload, ()) = tokio::join!(
            object("POST", "/storage/v1/object/files/late.bin", &secret, body, &[]),
            async {
                past.notified().await;
                fx.owner("SELECT storage.drop_policy('o_1', 'objects')").await.unwrap();
            },
        );
        assert_eq!((upload.0, json_of(&upload.2)["error"]["code"].clone()), (403, json!("not_allowed")), "{}", json_of(&upload.2));
        // The whole body was taken: the refusal is the final write's, after the ticket (always
        // minted right after the body), which was withdrawn.
        assert_eq!(sent.load(std::sync::atomic::Ordering::SeqCst), total);
        assert_eq!(fx.blobs(&store).await, warm, "the refused upload's blob stayed");
        assert_eq!(fx.value("SELECT count(*) FROM storage.objects WHERE name = 'late.bin'").await, "0");
        assert_eq!(fx.value("SELECT count(*) FROM storage.upload_tickets").await, "0");
    }))
    .await;
}

/// SF3 (AC 10): over the bucket's limit is 413 whether chunked or declared, with nothing left;
/// exactly at it is 200.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_api_limit_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'")]).await;
        let store = object_store().await;
        let up = |n: &str| format!("/storage/v1/object/capped/{n}");
        let chunked = Some(paced_body(2000, 5, Duration::ZERO, None));
        assert_eq!(object("POST", &up("chunked"), &secret, chunked, &[]).await.0, 413);
        assert_eq!(object("POST", &up("declared"), &secret, Some(vec![5u8; 2000].into()), &[]).await.0, 413);
        assert_eq!(fx.value("SELECT count(*) FROM storage.objects").await, "0");
        assert!(fx.blobs(&store).await.is_empty());
        let (status, _, b) = object("POST", &up("exact"), &secret, Some(vec![5u8; 1000].into()), &[]).await;
        assert_eq!((status, json_of(&b)["size"].clone()), (200, json!(1000)));
        assert_eq!(fx.blobs(&store).await.len(), 1);
    }))
    .await;
}

/// AC 10 as written: 1 GiB at 1 MiB/s through the gateway, about 18 minutes. Run on demand
/// only (`-E 'test(gib)'`); routine runs leave it out with `-E 'not test(gib)'`. The 64 MiB
/// paced test (`storage_api_slow_uploads_live`) covers the timeouts routinely.
#[tokio::test]
#[ignore = "on demand only: about 18 minutes"]
async fn storage_api_gib_upload_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'")]).await;
        let total = 1usize << 30;
        // 64 KiB every 62.5 ms: 1 MiB/s, so about 18 minutes (the client waits up to 30).
        let body = Some(paced_body(total, 6, Duration::from_micros(62_500), None));
        let started = Instant::now();
        let (status, _, b) = object_within("POST", "/storage/v1/object/files/gib.bin", &secret, body, &[], Duration::from_secs(1800)).await;
        assert_eq!((status, json_of(&b)["size"].clone()), (200, json!(total)), "{}", json_of(&b));
        println!("1 GiB at 1 MiB/s took {:?}", started.elapsed());
    }))
    .await;
}

/// SF5 (AC 1 for an app user): a public key with the user's token uploads as `authenticated`
/// with the user's claims; the row's owner is the token's `sub`, which an `owner =
/// chelabase.user_id()` policy requires.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_api_app_user_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        objects_fixture(fx, &[("SELECT", "'owner = chelabase.user_id()'", "NULL"), ("INSERT", "NULL", "'owner = chelabase.user_id()'")]).await;
        let public = fx.public_key().await;
        let user = Uuid::new_v4();
        let token = fx.user_token(user).await;
        let headers = [("chelabase-user", token.as_str())];
        let (status, _, b) = object("POST", "/storage/v1/object/files/me.txt", &public, Some(b"me".to_vec().into()), &headers).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        assert_eq!(fx.value("SELECT owner FROM storage.objects WHERE name = 'me.txt'").await, user.to_string());
        assert_eq!(&object("GET", "/storage/v1/object/files/me.txt", &public, None, &headers).await.2[..], b"me");
        // Without the token the public key's role (anon) has no policy.
        assert_eq!(object("POST", "/storage/v1/object/files/anon.txt", &public, Some(b"x".to_vec().into()), &[]).await.0, 403);
    }))
    .await;
}

impl Fx {
    /// An app-user access token for `user` in this project, with a live session.
    async fn user_token(&self, user: Uuid) -> String {
        self.user_session(user, chrono::Duration::hours(1)).await.0
    }

    /// [`Fx::user_token`] with a session that lives `life`: the token and the session's id.
    async fn user_session(&self, user: Uuid, life: chrono::Duration) -> (String, Uuid) {
        use chelabase_auth::app_token::{APP_ISSUER, AppClaims, AppTokenSigner, app_audience};
        let sid = Uuid::new_v4();
        let now = chrono::Utc::now();
        let mut hash = [0u8; 32];
        hash[..16].copy_from_slice(Uuid::new_v4().as_bytes());
        self.metadata
            .insert_app_session(&chelabase_metadata::NewAppSession {
                id: sid,
                project_id: self.project,
                user_id: user,
                identity_session_id: "storage-live",
                refresh_hash: hash,
                expires_at: now + life,
            })
            .await
            .expect("insert the app session");
        let path = |var: &str| std::path::PathBuf::from(std::env::var(var).unwrap_or_else(|_| panic!("{var}")));
        let signer = AppTokenSigner::from_pem_files(&path("CHELABASE_APP_JWT_PRIVATE_KEY"), &path("CHELABASE_APP_JWT_PUBLIC_KEY"))
            .expect("the app JWT key pair");
        let iat = now.timestamp() as u64;
        signer
            .sign(&AppClaims {
                iss: APP_ISSUER.to_string(),
                aud: app_audience(self.project.as_uuid()),
                sub: user,
                sid,
                project: self.project.as_uuid(),
                email: format!("{user}@example.com"),
                email_verified: true,
                role: "authenticated".to_string(),
                iat,
                exp: iat + 600,
            })
            .map(|token| (token, sid))
            .expect("sign the app token")
    }
}

/// SF10: blobs are keyed by the Postgres database's OID, which a branch shares (a physical
/// copy): an object uploaded on main is at the same key a child branch's row names.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_api_oid_prefix_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'")]).await;
        let store = object_store().await;
        let (status, _, b) = object("POST", "/storage/v1/object/files/o.txt", &secret, Some(b"o".to_vec().into()), &[]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        let version = json_of(&b)["version_id"].as_str().unwrap().to_string();
        let oid_sql = "SELECT oid FROM pg_database WHERE datname = current_database()";
        let main_oid = fx.value(oid_sql).await;
        assert_eq!(fx.blobs(&store).await, std::slice::from_ref(&version), "under {}/{main_oid}/", fx.id);
        let (child, child_ep) = fx.child("oid-child").await;
        let child_oid = last_line(&fx.admin_on(&child_ep, oid_sql).await.unwrap());
        assert_eq!(child_oid, main_oid, "a branch shares the database's OID");
        let child_version = last_line(&fx.admin_on(&child_ep, "SELECT version_id FROM storage.objects WHERE name = 'o.txt'").await.unwrap());
        assert_eq!(child_version, version);
        // The child downloads main's object (the re-audit's test); its own upload isn't main's.
        let (status, body) = api("POST", &format!("/v1/databases/{}/branches/{child}/keys", fx.id), Some(json!({ "name": "oid-child", "role": "authenticated" }))).await;
        assert_eq!(status, 201, "{body}");
        let ck = body["key"].as_str().unwrap().to_string();
        let (status, _, got) = object("GET", "/storage/v1/object/files/o.txt", &ck, None, &[]).await;
        assert_eq!((status, &got[..]), (200, &b"o"[..]));
        let (status, _, b) = object("POST", "/storage/v1/object/files/c.txt", &ck, Some(b"c".to_vec().into()), &[]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        assert_eq!(object("GET", "/storage/v1/object/files/c.txt", &secret, None, &[]).await.0, 404, "a child's upload is visible on main");
        let (status, _, got) = object("GET", "/storage/v1/object/files/c.txt", &ck, None, &[]).await;
        assert_eq!((status, &got[..]), (200, &b"c"[..]));
    }))
    .await;
}

// --- m5 fix round 2 (re-audit) ---------------------------------------------------------------

/// A paced body that counts what it handed to the client and notifies `past` once more than
/// `mark` bytes are out, so a test knows the server has pulled beyond the socket buffers.
fn counted_body(
    total: usize,
    pause: Duration,
    sent: Arc<std::sync::atomic::AtomicUsize>,
    mark: usize,
    past: Arc<tokio::sync::Notify>,
) -> reqwest::Body {
    const CHUNK: usize = 64 * 1024;
    let stream = futures_util::stream::unfold(0usize, move |n| {
        let (sent, past) = (sent.clone(), past.clone());
        async move {
            if n >= total {
                return None;
            }
            tokio::time::sleep(pause).await;
            let k = CHUNK.min(total - n);
            sent.store(n + k, std::sync::atomic::Ordering::SeqCst);
            if n < mark && n + k >= mark {
                past.notify_one();
            }
            Some((Ok::<_, std::io::Error>(bytes::Bytes::from(vec![9u8; k])), n + k))
        }
    });
    reqwest::Body::wrap_stream(stream)
}

/// B1 (the re-audit's repro): a ticket left for an object's version (an abandoned move, a
/// failed withdrawal) no longer blocks its cross-bucket moves once it has expired.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_api_expired_ticket_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'"), ("UPDATE", "'true'", "'true'"), ("DELETE", "'true'", "NULL")]).await;
        let (status, _, b) = object("POST", "/storage/v1/object/files/t.txt", &secret, Some(b"t".to_vec().into()), &[]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        let version = json_of(&b)["version_id"].as_str().unwrap().to_string();
        fx.admin(&format!("BEGIN; SET LOCAL ROLE chelabase_storage; INSERT INTO storage.upload_tickets (version_id, bucket_id, name, size, etag, expires_at, caller_role, owner) SELECT '{version}', id, 'elsewhere', 1, 'x', clock_timestamp() + interval '1 second', 'authenticated', NULL FROM storage.buckets WHERE name = 'other'; COMMIT;")).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        let expired = fx.value(&format!("SELECT count(*) FROM storage.upload_tickets WHERE version_id = '{version}' AND expires_at < clock_timestamp()")).await;
        assert_eq!(expired, "1");
        let body = json!({ "bucket": "files", "from": "t.txt", "to": "t.txt", "to_bucket": "other" }).to_string();
        let (status, _, b) = object("POST", "/storage/v1/object/move", &secret, Some(body.into()), &[JSON]).await;
        assert_eq!(status, 204, "an expired ticket still blocks the move: {}", json_of(&b));
        assert_eq!(fx.value(&tickets_sql(&version)).await, "0");
    }))
    .await;
}

/// B1: a ticket a branch copied from its parent (another timeline's, never consumable) doesn't
/// block the object's cross-bucket move on the branch, even unexpired.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_api_foreign_timeline_ticket_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'"), ("UPDATE", "'true'", "'true'")]).await;
        let (status, _, b) = object("POST", "/storage/v1/object/files/t.txt", &secret, Some(b"t".to_vec().into()), &[]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        let version = json_of(&b)["version_id"].as_str().unwrap().to_string();
        // A move in flight on main when the branch is made: its ticket is copied, with main's
        // timeline.
        fx.admin(&format!("BEGIN; SET LOCAL ROLE chelabase_storage; INSERT INTO storage.upload_tickets (version_id, bucket_id, name, size, etag, expires_at, caller_role, owner) SELECT '{version}', id, 't.txt', 1, 'x', clock_timestamp() + interval '10 minutes', 'authenticated', NULL FROM storage.buckets WHERE name = 'other'; COMMIT;")).await;
        let (child, child_ep) = fx.child("ticket-child").await;
        assert_eq!(last_line(&fx.admin_on(&child_ep, &tickets_sql(&version)).await.unwrap()), "1");
        let (status, body) = api("POST", &format!("/v1/databases/{}/branches/{child}/keys", fx.id), Some(json!({ "name": "child", "role": "authenticated" }))).await;
        assert_eq!(status, 201, "{body}");
        let child_key = body["key"].as_str().unwrap().to_string();
        let body = json!({ "bucket": "files", "from": "t.txt", "to": "t.txt", "to_bucket": "other" }).to_string();
        let (status, _, b) = object("POST", "/storage/v1/object/move", &child_key, Some(body.into()), &[JSON]).await;
        assert_eq!(status, 204, "another timeline's ticket blocks the move: {}", json_of(&b));
        assert_eq!(last_line(&fx.admin_on(&child_ep, &tickets_sql(&version)).await.unwrap()), "0");
    }))
    .await;
}

/// B2 (the re-audit's repro): the database dropped (renamed away) and made again under the
/// same name within the schema check's cache: the upload's blob goes under the new database's
/// OID (read in the request's own transaction), and it downloads once the cache has expired.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service; about 40 s"]
async fn storage_api_recreated_database_live() {
    with_fx(|fx| Box::pin(async move {
        use chelabase_storage::ObjectStore;
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'")]).await;
        let store = object_store().await;
        let oid_sql = "SELECT oid FROM pg_database WHERE datname = current_database()";
        let started = Instant::now();
        let (status, _, b) = object("POST", "/storage/v1/object/files/a.txt", &secret, Some(b"a".to_vec().into()), &[]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        let old_oid = fx.value(oid_sql).await;
        let d = fx.database.clone();
        let maint = if d == "postgres" { "template1" } else { "postgres" };
        let sys = |sql: String| async move { fx.worker.computes.admin_sql(&fx.ep, maint, &sql).await.map_err(|e| format!("{e:#}")) };
        let mut renamed = Err(String::new());
        for _ in 0..10 {
            renamed = sys(format!("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{d}' AND pid <> pg_backend_pid();\nALTER DATABASE \"{d}\" RENAME TO \"st_old\";\n")).await;
            if renamed.is_ok() {
                break;
            }
        }
        renamed.expect("rename");
        let mut made = Err(String::new());
        for _ in 0..10 {
            made = sys(format!("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = 'st_old' AND pid <> pg_backend_pid();\nCREATE DATABASE \"{d}\" TEMPLATE \"st_old\";\n")).await;
            if made.is_ok() {
                break;
            }
        }
        made.expect("create");
        let new_oid = fx.value(oid_sql).await;
        assert_ne!(new_oid, old_oid);
        let mut uploaded = None;
        for _ in 0..5 {
            let (status, _, b) = object("POST", "/storage/v1/object/files/b.txt", &secret, Some(b"b".to_vec().into()), &[]).await;
            if status == 200 {
                uploaded = Some(json_of(&b)["version_id"].as_str().unwrap().to_string());
                break;
            }
        }
        let vb = uploaded.expect("b.txt uploaded");
        assert!(started.elapsed() < Duration::from_secs(29), "inconclusive: past the cache TTL");
        let under_old = store.head(&format!("{}/{old_oid}/{vb}", fx.id)).await.is_ok();
        let under_new = store.head(&format!("{}/{new_oid}/{vb}", fx.id)).await.is_ok();
        tokio::time::sleep(Duration::from_secs(33).saturating_sub(started.elapsed())).await;
        let (status, _, body) = object("GET", "/storage/v1/object/files/b.txt", &secret, None, &[]).await;
        for oid in [&old_oid, &new_oid] {
            for blob in store.list(&format!("{}/{oid}/", fx.id)).await.unwrap() {
                store.delete(&blob.key).await.unwrap();
            }
        }
        sys("DROP DATABASE IF EXISTS \"st_old\" WITH (FORCE)".into()).await.expect("drop st_old");
        assert!(under_new && !under_old, "b.txt's blob went to the dropped database's prefix");
        assert_eq!((status, &body[..]), (200, &b"b"[..]), "b.txt became unreachable");
    }))
    .await;
}

/// SF4: a ticket minted for a move or a copy is withdrawn when the caller's final write is
/// refused after it (a policy that passes once, at the pre-check, then refuses).
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_api_refused_final_write_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        fx.owner(
            "CREATE SEQUENCE public.st_calls;
             CREATE FUNCTION public.st_first_call() RETURNS boolean LANGUAGE sql SECURITY DEFINER SET search_path = pg_catalog
               AS $$ SELECT nextval('public.st_calls') % 2 = 1 $$;
             GRANT EXECUTE ON FUNCTION public.st_first_call() TO authenticated;",
        )
        .await
        .unwrap();
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'")]).await;
        let store = object_store().await;
        let (status, _, b) = object("POST", "/storage/v1/object/files/src.txt", &secret, Some(b"s".to_vec().into()), &[]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        // From here, every INSERT and UPDATE check passes on odd calls only.
        fx.owner("SELECT storage.drop_policy('o_1', 'objects')").await.unwrap();
        for (name, command) in [("o_flip_insert", "INSERT"), ("o_flip_update", "UPDATE")] {
            let using = if command == "UPDATE" { "'true'" } else { "NULL" };
            fx.owner(&format!("SELECT storage.create_policy('{name}', 'objects', '{command}', ARRAY['authenticated']::name[], {using}, 'public.st_first_call()')"))
                .await
                .unwrap();
        }
        let version = json_of(&b)["version_id"].as_str().unwrap().to_string();
        let blobs = fx.blobs(&store).await;
        // Copy: the pre-check passes (call 1), the final INSERT is refused (call 2).
        let body = json!({ "bucket": "files", "from": "src.txt", "to": "dst.txt" }).to_string();
        let (status, _, b) = object("POST", "/storage/v1/object/copy", &secret, Some(body.into()), &[JSON]).await;
        assert_eq!(status, 403, "{}", json_of(&b));
        assert_eq!(fx.value("SELECT count(*) FROM storage.upload_tickets").await, "0", "the copy's ticket stayed");
        assert_eq!(fx.blobs(&store).await, blobs, "the copy's blob stayed");
        // Move: the pre-check UPDATE passes (call 3), the final UPDATE is refused (call 4).
        let body = json!({ "bucket": "files", "from": "src.txt", "to": "src.txt", "to_bucket": "other" }).to_string();
        let (status, _, b) = object("POST", "/storage/v1/object/move", &secret, Some(body.into()), &[JSON]).await;
        assert_eq!(status, 403, "{}", json_of(&b));
        assert_eq!(fx.value(&tickets_sql(&version)).await, "0", "the move's ticket stayed");
        assert_eq!(fx.value("SELECT count(*) FROM storage.objects WHERE name = 'src.txt'").await, "1");
    }))
    .await;
}

// --- B2: the storage service in-process, against the real compute ---------------------------

/// An object store that fails `put` or `copy` on demand (otherwise in memory).
#[derive(Default)]
struct FailingStore {
    inner: chelabase_storage::MemoryStore,
    fail_put: std::sync::atomic::AtomicBool,
    fail_copy: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl chelabase_storage::ObjectStore for FailingStore {
    async fn put(&self, key: &str, body: bytes::Bytes, opts: chelabase_storage::PutOptions) -> Result<chelabase_storage::ObjectMeta, chelabase_storage::StorageError> {
        if self.fail_put.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(chelabase_storage::StorageError::Transient("injected put failure".into()));
        }
        self.inner.put(key, body, opts).await
    }
    async fn get(&self, key: &str, range: Option<chelabase_storage::ByteRange>) -> Result<chelabase_storage::GetObject, chelabase_storage::StorageError> {
        self.inner.get(key, range).await
    }
    async fn head(&self, key: &str) -> Result<chelabase_storage::ObjectMeta, chelabase_storage::StorageError> {
        self.inner.head(key).await
    }
    async fn delete(&self, key: &str) -> Result<(), chelabase_storage::StorageError> {
        self.inner.delete(key).await
    }
    async fn copy(&self, src: &str, dst: &str) -> Result<chelabase_storage::ObjectMeta, chelabase_storage::StorageError> {
        if self.fail_copy.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(chelabase_storage::StorageError::Transient("injected copy failure".into()));
        }
        self.inner.copy(src, dst).await
    }
    async fn list(&self, prefix: &str) -> Result<Vec<chelabase_storage::ObjectMeta>, chelabase_storage::StorageError> {
        self.inner.list(prefix).await
    }
    // The sweep's reads (m8), so a sweep can run over this store.
    async fn list_dated(&self, prefix: &str) -> Result<Vec<chelabase_storage::DatedObject>, chelabase_storage::StorageError> {
        self.inner.list_dated(prefix).await
    }
    async fn list_uploads(&self, prefix: &str) -> Result<Vec<chelabase_storage::OpenUpload>, chelabase_storage::StorageError> {
        self.inner.list_uploads(prefix).await
    }
    async fn multipart_create(&self, key: &str, content_type: Option<&str>) -> Result<String, chelabase_storage::StorageError> {
        self.inner.multipart_create(key, content_type).await
    }
    async fn multipart_part(&self, key: &str, id: &str, n: u32, body: bytes::Bytes) -> Result<String, chelabase_storage::StorageError> {
        self.inner.multipart_part(key, id, n, body).await
    }
    async fn multipart_complete(&self, key: &str, id: &str, parts: Vec<chelabase_storage::CompletedPart>) -> Result<chelabase_storage::ObjectMeta, chelabase_storage::StorageError> {
        self.inner.multipart_complete(key, id, parts).await
    }
    async fn multipart_abort(&self, key: &str, id: &str) -> Result<(), chelabase_storage::StorageError> {
        self.inner.multipart_abort(key, id).await
    }
    async fn multipart_part_copy(&self, key: &str, id: &str, n: u32, src: &str, range: std::ops::Range<u64>) -> Result<String, chelabase_storage::StorageError> {
        self.inner.multipart_part_copy(key, id, n, src, range).await
    }
}

/// The storage router in this process with `store`, reaching the stack's orchestrator and the
/// branch's compute as the deployed service does; its base URL and an identity token for a
/// secret key on `authenticated` of `branch`.
async fn in_process_storage(store: Arc<FailingStore>, branch: BranchId) -> (String, String) {
    in_process_storage_with(store, branch, storage::limits::InFlight::new(1 << 30)).await
}

/// [`in_process_storage`] with the upload memory cap `inflight`.
async fn in_process_storage_with(store: Arc<FailingStore>, branch: BranchId, inflight: storage::limits::InFlight) -> (String, String) {
    use chelabase_auth::internal::{AuthMethod, DataIdentity, Identity, InternalSigner, InternalVerifier, generate_keypair_pem};
    let url = std::env::var("CHELABASE_INTERNAL_URL").unwrap_or_else(|_| "http://orchestrator:8083".into());
    let token = std::fs::read_to_string(std::env::var("CHELABASE_STORAGE_INTERNAL_TOKEN_FILE").expect("the storage bearer"))
        .unwrap()
        .trim()
        .to_string();
    let ca = std::env::var("CHELABASE_COMPUTE_CA_FILE").expect("the compute CA");
    let tls = chelabase_query::server_tls::ServerTls::from_ca_file(std::path::Path::new(&ca)).unwrap();
    let pools = chelabase_pgsession::Pools::with_server_tls(chelabase_pgsession::InternalSource::new(&url, &token), 8, tls);
    let (private, public) = generate_keypair_pem();
    let state = Arc::new(storage::app::AppState {
        engine: storage::engine::Engine::new(pools, chelabase_pgsession::InternalSource::storage_login(&url, &token)),
        verifier: InternalVerifier::from_pem(public.as_bytes()).unwrap(),
        store,
        urls: storage::control::Urls::new(Arc::new(storage::control::Orchestrator::new(&url, &token))),
        inflight,
        events: storage::events::ObjectEvents::none(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, storage::app::router(state)).await });
    let identity = Identity {
        sub: Uuid::new_v4().to_string(),
        auth: AuthMethod::DataKey,
        request_id: "in-process".into(),
        data: Some(DataIdentity {
            branch_id: branch.as_uuid(),
            key_id: Uuid::new_v4(),
            kind: chelabase_types::data_key::DataKeyKind::Secret,
            role: "authenticated".into(),
            claims: json!({}),
            databases: None,
            project_id: None,
            role_oid: None,
            server_vouched: false,
        }),
        email: None,
        name: None,
    };
    let signer = InternalSigner::from_pem(private.as_bytes()).unwrap();
    (base, signer.sign(storage::app::AUDIENCE, &identity).unwrap())
}

async fn direct(base: &str, token: &str, method: &str, path: &str, body: Vec<u8>) -> (u16, Value) {
    let res = reqwest::Client::new()
        .request(method.parse().unwrap(), format!("{base}{path}"))
        .header(chelabase_auth::internal::IDENTITY_HEADER, token)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap();
    let status = res.status().as_u16();
    (status, res.json().await.unwrap_or_default())
}

/// B2: the object store failing on upload (put) or copy is 502 `storage_error`, with no row, no
/// ticket and nothing in the store.
#[tokio::test]
#[ignore = "needs the workbench stack"]
async fn storage_store_failure_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'")]).await;
        use chelabase_storage::ObjectStore;
        let store = Arc::new(FailingStore::default());
        let (base, token) = in_process_storage(store.clone(), fx.main).await;
        store.fail_put.store(true, std::sync::atomic::Ordering::SeqCst);
        let (status, body) = direct(&base, &token, "POST", "/storage/v1/object/files/a.txt", b"hello".to_vec()).await;
        assert_eq!((status, &body["error"]["code"]), (502, &json!("storage_error")), "{body}");
        assert_eq!(fx.value("SELECT count(*) FROM storage.objects").await, "0");
        assert_eq!(fx.value("SELECT count(*) FROM storage.upload_tickets").await, "0");
        assert!(store.list("").await.unwrap().is_empty());

        store.fail_put.store(false, std::sync::atomic::Ordering::SeqCst);
        let (status, body) = direct(&base, &token, "POST", "/storage/v1/object/files/a.txt", b"hello".to_vec()).await;
        assert_eq!(status, 200, "{body}");
        store.fail_copy.store(true, std::sync::atomic::Ordering::SeqCst);
        let copy = json!({ "bucket": "files", "from": "a.txt", "to": "b.txt" }).to_string().into_bytes();
        let (status, body) = direct(&base, &token, "POST", "/storage/v1/object/copy", copy).await;
        assert_eq!((status, &body["error"]["code"]), (502, &json!("storage_error")), "{body}");
        assert!(body["error"].get("detail").is_none() && body["error"].get("sqlstate").is_none(), "{body}");
        assert_eq!(fx.value("SELECT string_agg(name, ',') FROM storage.objects").await, "a.txt");
        assert_eq!(fx.value("SELECT count(*) FROM storage.upload_tickets").await, "0");
        assert_eq!(store.list("").await.unwrap().len(), 1, "only a.txt's blob");
    }))
    .await;
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

// --- m6: signed and public URLs through the gateway, with no key -------------------------

/// A GET of `url` (a path on the API host) with no key: `(status, headers, body)`.
async fn keyless(method: &str, url: &str, body: Option<Vec<u8>>) -> (u16, reqwest::header::HeaderMap, bytes::Bytes) {
    let mut req = reqwest::Client::new()
        .request(method.parse().unwrap(), format!("{}{url}", gateway()))
        .timeout(Duration::from_secs(60));
    if let Some(body) = body {
        req = req.body(body);
    }
    let res = req.send().await.expect("the gateway answers");
    let (status, headers) = (res.status().as_u16(), res.headers().clone());
    (status, headers, res.bytes().await.unwrap_or_default())
}

/// `POST /storage/v1/object/{route}/{path}` with `{"expires_in"}`: the signed URL.
async fn mint(route: &str, path: &str, key: &str, expires_in: i64) -> (u16, String) {
    let body = serde_json::to_vec(&json!({ "expires_in": expires_in })).unwrap();
    let (status, _, b) = object("POST", &format!("/storage/v1/object/{route}/{path}"), key, Some(body.into()), &[JSON]).await;
    (status, json_of(&b)["signed_url"].as_str().unwrap_or_default().to_string())
}

#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_urls_live() {
    let fx = Arc::new(Fx::provision().await);
    let task = fx.clone();
    let outcome = tokio::spawn(async move { storage_urls(&task).await }).await;
    fx.purge_blobs(&object_store().await).await;
    fx.teardown().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

async fn storage_urls(fx: &Fx) {
    let secret = fx.secret_key("st-urls", None).await;
    fx.bucket("files").await;
    fx.bucket("pub").await;
    fx.admin("BEGIN; SET LOCAL ROLE chelabase_storage; UPDATE storage.buckets SET public = true WHERE name = 'pub'; COMMIT;").await;
    fx.owner("SELECT storage.create_policy('b_select', 'buckets', 'SELECT', ARRAY['authenticated']::name[], 'true', NULL)")
        .await
        .unwrap();
    for (name, command, using, check) in [
        ("o_select", "SELECT", "'name NOT LIKE ''hidden/%'''", "NULL"),
        ("o_insert", "INSERT", "NULL", "'true'"),
        ("o_update", "UPDATE", "'true'", "'true'"),
        ("o_delete", "DELETE", "'true'", "NULL"),
    ] {
        fx.owner(&format!(
            "SELECT storage.create_policy('{name}', 'objects', '{command}', ARRAY['authenticated']::name[], {using}, {check})"
        ))
        .await
        .unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    let put = |path: &'static str, body: &'static [u8], ct: &'static str, upsert: bool| {
        let secret = secret.clone();
        async move {
            let mut headers = vec![("content-type", ct)];
            if upsert {
                headers.push(("x-upsert", "true"));
            }
            let (status, _, b) =
                object("POST", &format!("/storage/v1/object/{path}"), &secret, Some(body.to_vec().into()), &headers).await;
            assert_eq!(status, 200, "{path}: {}", json_of(&b));
        }
    };
    put("files/docs/a b.txt", b"v1", "text/plain", false).await;
    put("files/page.html", b"<script>alert(1)</script>", "text/html", false).await;
    put("files/hidden/x.txt", b"h", "text/plain", false).await;
    put("pub/logo.png", b"png", "image/png", false).await;

    // AC 1: downloads without a key, inline (a safe type), sandboxed, not cached as public.
    let (status, url) = mint("sign", "files/docs/a b.txt", &secret, 60).await;
    assert_eq!(status, 200, "{url}");
    assert!(url.starts_with("/storage/v1/object/sign/files/docs/a%20b.txt?token="), "{url}");
    let (status, h, b) = keyless("GET", &url, None).await;
    assert_eq!((status, &b[..]), (200, &b"v1"[..]));
    assert_eq!(h["x-content-type-options"], "nosniff");
    assert_eq!(h["content-security-policy"], "sandbox");
    assert_eq!(h["content-disposition"], "inline");
    assert_eq!(h["cache-control"], "private, no-cache");

    // AC 3: no signed URL for an object the caller can't read; AC 10: a TTL over 7 days.
    assert_eq!(mint("sign", "files/hidden/x.txt", &secret, 60).await.0, 404);
    assert_eq!(mint("sign", "files/docs/a b.txt", &secret, 604_801).await.0, 400);
    assert_eq!(mint("upload/sign", "files/u.txt", &secret, 604_801).await.0, 400);

    // AC 2: a changed token, or one for another object.
    let tampered = format!("{}{}", &url[..url.len() - 1], if url.ends_with('A') { 'B' } else { 'A' });
    assert_eq!(keyless("GET", &tampered, None).await.0, 401);
    let token = url.split_once("?token=").unwrap().1;
    let elsewhere = format!("/storage/v1/object/sign/files/page.html?token={token}");
    assert_eq!(keyless("GET", &elsewhere, None).await.0, 403);
    assert_eq!(keyless("GET", "/storage/v1/object/sign/files/page.html", None).await.0, 401);

    // AC 9: HTML is a sandboxed, unsniffed attachment.
    let (_, html) = mint("sign", "files/page.html", &secret, 60).await;
    let (status, h, _) = keyless("GET", &html, None).await;
    assert_eq!(status, 200);
    assert!(h["content-disposition"].to_str().unwrap().starts_with("attachment;"), "{h:?}");
    assert_eq!(h["content-security-policy"], "sandbox");
    assert_eq!(h["x-content-type-options"], "nosniff");
    // m5's authenticated download says the same.
    let (_, h, _) = object("GET", "/storage/v1/object/files/page.html", &secret, None, &[]).await;
    assert!(h["content-disposition"].to_str().unwrap().starts_with("attachment;"), "{h:?}");
    assert_eq!(h["content-security-policy"], "sandbox");

    // AC 4 (decided after the audit): a URL serves only the version it was signed for while
    // it is current; overwritten, it is 404 (a fresh URL serves the new version); deleted, 404.
    put("files/docs/a b.txt", b"v2", "text/plain", true).await;
    assert_eq!(keyless("GET", &url, None).await.0, 404);
    let (_, fresh) = mint("sign", "files/docs/a b.txt", &secret, 60).await;
    assert_eq!(&keyless("GET", &fresh, None).await.2[..], b"v2");
    let (status, _, _) = object("DELETE", "/storage/v1/object/files/docs/a%20b.txt", &secret, None, &[]).await;
    assert_eq!(status, 204);
    assert_eq!(keyless("GET", &fresh, None).await.0, 404);

    // AC 5: a private bucket's public path is 404; a public one serves with Cache-Control.
    let public = |path: &str| format!("/storage/v1/object/public/{}/{path}", fx.main);
    assert_eq!(keyless("GET", &public("files/page.html"), None).await.0, 404);
    assert_eq!(keyless("GET", &public("pub/missing.png"), None).await.0, 404);
    let (status, h, b) = keyless("GET", &public("pub/logo.png"), None).await;
    assert_eq!((status, &b[..]), (200, &b"png"[..]));
    assert_eq!(h["cache-control"], "public, max-age=3600");
    assert_eq!(h["content-security-policy"], "sandbox");

    // AC 8, 11: an upload URL takes one upload without a key; the second use fails.
    let (status, up) = mint("upload/sign", "files/up.txt", &secret, 60).await;
    assert_eq!(status, 200, "{up}");
    let (status, _, b) = keyless("PUT", &up, Some(b"once".to_vec())).await;
    assert_eq!(status, 200, "{}", json_of(&b));
    let (status, _, b) = keyless("PUT", &up, Some(b"twice".to_vec())).await;
    assert_eq!((status, json_of(&b)["error"]["code"].clone()), (403, json!("token_not_valid")));
    let (_, _, b) = object("GET", "/storage/v1/object/files/up.txt", &secret, None, &[]).await;
    assert_eq!(&b[..], b"once");

    // AC 7: a revoked key's URLs fail (download and upload alike).
    let doomed = fx.secret_key("st-urls-doomed", None).await;
    let (_, durl) = mint("sign", "files/page.html", &doomed, 60).await;
    let (_, dup) = mint("upload/sign", "files/d.txt", &doomed, 60).await;
    let (status, keys) = api("GET", &fx.keys_path(), None).await;
    assert_eq!(status, 200);
    let id = keys.as_array().unwrap().iter().find(|k| k["name"] == "st-urls-doomed").expect("the key")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (status, body) = api("DELETE", &format!("{}/{id}", fx.keys_path()), None).await;
    assert!(status == 200 || status == 204, "revoke: {status} {body}");
    assert_eq!(keyless("GET", &durl, None).await.0, 403);
    assert_eq!(keyless("PUT", &dup, Some(b"x".to_vec())).await.0, 403);
}

// --- m6 fix round (the audit's findings) ----------------------------------------------------

/// [`keyless`] with request headers.
async fn keyless_with(method: &str, url: &str, body: Vec<u8>, headers: &[(&str, &str)]) -> (u16, Value) {
    let mut req = reqwest::Client::new()
        .request(method.parse().unwrap(), format!("{}{url}", gateway()))
        .timeout(Duration::from_secs(60))
        .body(body);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = req.send().await.expect("the gateway answers");
    let status = res.status().as_u16();
    (status, serde_json::from_slice(&res.bytes().await.unwrap_or_default()).unwrap_or(Value::Null))
}

/// `POST /storage/v1/object/{route}/{path}` with `body` and `headers`: `(status, the answer)`.
async fn mint_with(route: &str, path: &str, key: &str, body: Value, headers: &[(&str, &str)]) -> (u16, Value) {
    let mut all = vec![JSON];
    all.extend_from_slice(headers);
    let body = serde_json::to_vec(&body).unwrap();
    let (status, _, b) = object("POST", &format!("/storage/v1/object/{route}/{path}"), key, Some(body.into()), &all).await;
    (status, json_of(&b))
}

/// B1, S3: an app user's upload URL uploads as `authenticated` with the user's claims (the
/// owner policy passes), lives no longer than the session, and dies with the session.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_urls_app_user_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        objects_fixture(fx, &[("SELECT", "'owner = chelabase.user_id()'", "NULL"), ("INSERT", "NULL", "'owner = chelabase.user_id()'")]).await;
        let public = fx.public_key().await;
        let user = Uuid::new_v4();
        let (token, sid) = fx.user_session(user, chrono::Duration::minutes(10)).await;
        let headers = [("chelabase-user", token.as_str())];
        // B1: the keyless PUT runs as the user (the key's own role, anon, has no policy).
        let (status, up) = mint_with("upload/sign", "files/mine.txt", &public, json!({ "expires_in": 3600 }), &headers).await;
        assert_eq!(status, 200, "{up}");
        // S3: no longer than the session's 10 minutes.
        let exp = up["expires_at"].as_i64().unwrap();
        assert!(exp <= chrono::Utc::now().timestamp() + 600, "{up}");
        let (status, b) = keyless_with("PUT", up["signed_url"].as_str().unwrap(), b"mine".to_vec(), &[]).await;
        assert_eq!(status, 200, "{b}");
        assert_eq!(fx.value("SELECT owner FROM storage.objects WHERE name = 'mine.txt'").await, user.to_string());
        // S3: the session is revoked, so its unused upload URL fails.
        let (status, up) = mint_with("upload/sign", "files/later.txt", &public, json!({ "expires_in": 60 }), &headers).await;
        assert_eq!(status, 200, "{up}");
        assert!(fx.metadata.revoke_app_session(sid, fx.project).await.unwrap().is_some());
        let (status, b) = keyless_with("PUT", up["signed_url"].as_str().unwrap(), b"later".to_vec(), &[]).await;
        assert_eq!((status, b["error"]["code"].clone()), (403, json!("token_not_valid")), "{b}");
        assert_eq!(fx.value("SELECT count(*) FROM storage.objects WHERE name = 'later.txt'").await, "0");
    }))
    .await;
}

/// B2: a signed URL serves its version only while it is current: deleted then re-created
/// under the same name, it stays 404.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_urls_recreated_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'"), ("DELETE", "'true'", "NULL")]).await;
        let up = |body: &'static [u8]| {
            let secret = secret.clone();
            async move {
                let (status, _, b) = object("POST", "/storage/v1/object/files/s.txt", &secret, Some(body.to_vec().into()), &[("content-type", "text/plain")]).await;
                assert_eq!(status, 200, "{}", json_of(&b));
            }
        };
        up(b"SECRET-v1").await;
        let (status, url) = mint("sign", "files/s.txt", &secret, 600).await;
        assert_eq!(status, 200, "{url}");
        assert_eq!(&keyless("GET", &url, None).await.2[..], b"SECRET-v1");
        assert_eq!(object("DELETE", "/storage/v1/object/files/s.txt", &secret, None, &[]).await.0, 204);
        up(b"public-v2").await;
        let (status, _, b) = keyless("GET", &url, None).await;
        assert_eq!(status, 404, "the deleted version revived: {:?}", String::from_utf8_lossy(&b));
    }))
    .await;
}

/// B5 (AC 6b): deleted on its branch while a child branch still holds the row and blob, the
/// URL is 404; the child still serves its copy.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_urls_child_branch_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'"), ("DELETE", "'true'", "NULL")]).await;
        let (status, _, b) = object("POST", "/storage/v1/object/files/k.txt", &secret, Some(b"kept".to_vec().into()), &[]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        let (status, url) = mint("sign", "files/k.txt", &secret, 600).await;
        assert_eq!(status, 200, "{url}");
        let (child, _) = fx.child("urls-child").await;
        assert_eq!(object("DELETE", "/storage/v1/object/files/k.txt", &secret, None, &[]).await.0, 204);
        assert_eq!(keyless("GET", &url, None).await.0, 404);
        let (status, body) = api("POST", &format!("/v1/databases/{}/branches/{child}/keys", fx.id), Some(json!({ "name": "urls-child", "role": "authenticated" }))).await;
        assert_eq!(status, 201, "{body}");
        let ck = body["key"].as_str().unwrap().to_string();
        let (status, _, got) = object("GET", "/storage/v1/object/files/k.txt", &ck, None, &[]).await;
        assert_eq!((status, &got[..]), (200, &b"kept"[..]), "the child's copy");
        // A URL minted on the child serves its copy.
        let (status, child_url) = mint("sign", "files/k.txt", &ck, 600).await;
        assert_eq!(status, 200, "{child_url}");
        assert_eq!(&keyless("GET", &child_url, None).await.2[..], b"kept");
    }))
    .await;
}

/// B3: a Content-Type a browser would read as another type (a comma list) is refused at
/// every upload: a direct upload, a resumable upload and an upload URL.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_urls_content_type_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'")]).await;
        for ct in ["text/plain;a=b,text/html", "image/png;x=1, text/html", "text/plain; charset=\"utf-8\""] {
            let (status, _, b) = object("POST", "/storage/v1/object/files/c.txt", &secret, Some(b"<script>1</script>".to_vec().into()), &[("content-type", ct)]).await;
            assert_eq!(status, 400, "{ct}: {}", json_of(&b));
            assert_eq!(open_upload(&secret, "files", "c.txt", ct, &[]).await.0, 400, "{ct}");
            let (status, up) = mint("upload/sign", "files/c.txt", &secret, 60).await;
            assert_eq!(status, 200, "{up}");
            assert_eq!(keyless_with("PUT", &up, b"<script>1</script>".to_vec(), &[("content-type", ct)]).await.0, 400, "{ct}");
        }
        assert_eq!(fx.value("SELECT count(*) FROM storage.objects").await, "0");
        // A plain type with a charset still uploads and serves inline as its essence.
        let (status, _, b) = object("POST", "/storage/v1/object/files/ok.txt", &secret, Some(b"ok".to_vec().into()), &[("content-type", "Text/Plain; Charset=UTF-8; x=y")]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        let (status, url) = mint("sign", "files/ok.txt", &secret, 60).await;
        assert_eq!(status, 200, "{url}");
        let (status, h, _) = keyless("GET", &url, None).await;
        assert_eq!(status, 200);
        assert_eq!(h["content-type"], "text/plain; charset=utf-8");
        assert_eq!(h["content-disposition"], "inline");
    }))
    .await;
}

/// S1: whether an upload URL may overwrite is the minter's choice, bound into the token; the
/// uploader's `x-upsert` or `?upsert=` doesn't change it.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_urls_upsert_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'"), ("UPDATE", "'true'", "'true'")]).await;
        let (status, _, b) = object("POST", "/storage/v1/object/files/u.txt", &secret, Some(b"original".to_vec().into()), &[]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        let (status, up) = mint("upload/sign", "files/u.txt", &secret, 60).await;
        assert_eq!(status, 200, "{up}");
        let (status, b) = keyless_with("PUT", &format!("{up}&upsert=true"), b"overwritten".to_vec(), &[("x-upsert", "true")]).await;
        assert_eq!(status, 409, "{b}");
        assert_eq!(&object("GET", "/storage/v1/object/files/u.txt", &secret, None, &[]).await.2[..], b"original");
        // Minted with upsert, the URL overwrites.
        let (status, up) = mint_with("upload/sign", "files/u.txt", &secret, json!({ "expires_in": 60, "upsert": true }), &[]).await;
        assert_eq!(status, 200, "{up}");
        let (status, b) = keyless_with("PUT", up["signed_url"].as_str().unwrap(), b"replaced".to_vec(), &[]).await;
        assert_eq!(status, 200, "{b}");
        assert_eq!(&object("GET", "/storage/v1/object/files/u.txt", &secret, None, &[]).await.2[..], b"replaced");
    }))
    .await;
}

/// Re-audit B1 (decided 2026-11-06: accepted): a reset puts the branch back where the URL was
/// signed, so a URL a delete ended serves its version again until it expires. This pins the
/// behaviour.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_urls_reset_revives_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'"), ("DELETE", "'true'", "NULL")]).await;
        let (status, _, b) = object("POST", "/storage/v1/object/files/r.txt", &secret, Some(b"reset-original".to_vec().into()), &[]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        let (child, _) = fx.child("urls-reset").await;
        let (status, body) = api("POST", &format!("/v1/databases/{}/branches/{child}/keys", fx.id), Some(json!({ "name": "urls-reset", "role": "authenticated" }))).await;
        assert_eq!(status, 201, "{body}");
        let ck = body["key"].as_str().unwrap().to_string();
        let (status, url) = mint("sign", "files/r.txt", &ck, 3600).await;
        assert_eq!(status, 200, "{url}");
        assert_eq!(&keyless("GET", &url, None).await.2[..], b"reset-original");
        assert_eq!(object("DELETE", "/storage/v1/object/files/r.txt", &ck, None, &[]).await.0, 204);
        assert_eq!(keyless("GET", &url, None).await.0, 404);
        assert!(fx.metadata.set_branch_status(child, &[BranchStatus::Ready], BranchStatus::Resetting).await.unwrap());
        fx.worker.handle_branch_job(BranchJob::ResetToParent { branch_id: child }).await.expect("reset job");
        let record = fx.metadata.get_branch(child).await.unwrap().unwrap();
        assert_eq!(record.status, BranchStatus::Ready, "{:?}", record.error);
        let (status, _, got) = keyless("GET", &url, None).await;
        assert_eq!((status, &got[..]), (200, &b"reset-original"[..]), "the reset revives the URL");
    }))
    .await;
}

/// Round 3 re-audit B1: an overwrite ends a signed URL only until a reset too. The overwrite
/// gave the row a new version; resetting the branch brings the signed version back, and the
/// URL serves it again. Only revoking the minting key (or expiry) ends a URL for good.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_urls_reset_revives_after_overwrite_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'"), ("UPDATE", "'true'", "'true'")]).await;
        let (status, _, b) = object("POST", "/storage/v1/object/files/o.txt", &secret, Some(b"overwrite-original".to_vec().into()), &[]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        let (child, _) = fx.child("urls-overwrite-reset").await;
        let (status, body) = api("POST", &format!("/v1/databases/{}/branches/{child}/keys", fx.id), Some(json!({ "name": "urls-overwrite-reset", "role": "authenticated" }))).await;
        assert_eq!(status, 201, "{body}");
        let ck = body["key"].as_str().unwrap().to_string();
        let (status, url) = mint("sign", "files/o.txt", &ck, 3600).await;
        assert_eq!(status, 200, "{url}");
        let (status, _, b) = object("PUT", "/storage/v1/object/files/o.txt", &ck, Some(b"overwrite-v2".to_vec().into()), &[("x-upsert", "true")]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        assert_eq!(keyless("GET", &url, None).await.0, 404, "the overwrite ends the URL");
        assert!(fx.metadata.set_branch_status(child, &[BranchStatus::Ready], BranchStatus::Resetting).await.unwrap());
        fx.worker.handle_branch_job(BranchJob::ResetToParent { branch_id: child }).await.expect("reset job");
        let record = fx.metadata.get_branch(child).await.unwrap().unwrap();
        assert_eq!(record.status, BranchStatus::Ready, "{:?}", record.error);
        let (status, _, got) = keyless("GET", &url, None).await;
        assert_eq!((status, &got[..]), (200, &b"overwrite-original"[..]), "the reset revives the URL after an overwrite");
    }))
    .await;
}

/// Re-audit S1: an upload URL refused for its `Content-Type` (400) is not spent, and the
/// corrected retry uploads.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_urls_refused_upload_keeps_token_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'")]).await;
        let (status, up) = mint("upload/sign", "files/t.txt", &secret, 60).await;
        assert_eq!(status, 200, "{up}");
        let (status, b) = keyless_with("PUT", &up, b"retried".to_vec(), &[("content-type", "text/plain; charset=\"utf-8\"")]).await;
        assert_eq!((status, b["error"]["code"].clone()), (400, json!("invalid_request")), "{b}");
        let (status, b) = keyless_with("PUT", &up, b"retried".to_vec(), &[("content-type", "text/plain; charset=utf-8")]).await;
        assert_eq!(status, 200, "the corrected retry: {b}");
        assert_eq!(&object("GET", "/storage/v1/object/files/t.txt", &secret, None, &[]).await.2[..], b"retried");
    }))
    .await;
}

// --- m7: resumable uploads and bucket limits ------------------------------------------------

const MIB: usize = 1 << 20;

/// Opens a resumable upload of `bucket/name` as `key`: `(status, session id or the body)`.
async fn open_upload(key: &str, bucket: &str, name: &str, content_type: &str, headers: &[(&str, &str)]) -> (u16, Value) {
    let mut all = vec![("content-type", content_type)];
    all.extend_from_slice(headers);
    let (status, _, b) = object("POST", &format!("/storage/v1/upload/{bucket}/{name}"), key, None, &all).await;
    (status, json_of(&b))
}

async fn put_part(key: &str, session: &str, n: u32, body: Vec<u8>, headers: &[(&str, &str)]) -> (u16, Value) {
    let (status, _, b) = object("PUT", &format!("/storage/v1/upload/session/{session}/{n}"), key, Some(body.into()), headers).await;
    (status, json_of(&b))
}

async fn session_call(key: &str, method: &str, session: &str, tail: &str, headers: &[(&str, &str)]) -> (u16, Value) {
    let (status, _, b) = object(method, &format!("/storage/v1/upload/session/{session}{tail}"), key, None, headers).await;
    (status, json_of(&b))
}

/// The 5 MiB part `n` of a 30 MiB file.
fn part_bytes(n: u32) -> Vec<u8> {
    (0..5 * MIB).map(|i| ((i * 7 + n as usize * 13) % 251) as u8).collect()
}

/// AC 1: a 30 MiB file in six 5 MiB parts, part 3 first sent wrong and then again: the parts
/// list shows the six, completion yields exactly the original bytes, and the session is gone.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_multipart_resumed_upload_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'")]).await;
        let (status, b) = open_upload(&secret, "files", "big/thirty.bin", "application/octet-stream", &[]).await;
        assert_eq!(status, 200, "{b}");
        let id = b["id"].as_str().unwrap().to_string();
        for n in 1..=6 {
            let body = if n == 3 { vec![0u8; 5 * MIB] } else { part_bytes(n) };
            let (status, b) = put_part(&secret, &id, n, body, &[]).await;
            assert_eq!((status, b["size"].clone()), (200, json!(5 * MIB)), "part {n}: {b}");
        }
        // Part 3 "dropped": sent again with its real bytes.
        assert_eq!(put_part(&secret, &id, 3, part_bytes(3), &[]).await.0, 200);
        let (status, b) = session_call(&secret, "GET", &id, "", &[]).await;
        assert_eq!(status, 200, "{b}");
        assert_eq!(b["parts"].as_array().unwrap().len(), 6, "{b}");
        let (status, b) = session_call(&secret, "POST", &id, "/complete", &[]).await;
        assert_eq!((status, b["size"].clone()), (200, json!(30 * MIB)), "{b}");
        let got = object("GET", "/storage/v1/object/files/big/thirty.bin", &secret, None, &[]).await;
        let want: Vec<u8> = (1..=6).flat_map(part_bytes).collect();
        assert_eq!(got.0, 200);
        assert!(got.2[..] == want[..], "the completed object differs from the original");
        assert_eq!(session_call(&secret, "GET", &id, "", &[]).await.0, 404);
        assert_eq!(fx.value("SELECT count(*) FROM storage.upload_sessions").await, "0");
        assert_eq!(fx.value("SELECT count(*) FROM storage.upload_tickets").await, "0");
    }))
    .await;
}

/// AC 2: `file_size_limit = 1 MiB`: a 2 MiB single upload is 413 with nothing stored; a part
/// over it is 413; parts within a limit lowered before completion make `complete` 413, the
/// S3 upload aborted and the session gone.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_multipart_size_limit_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'")]).await;
        let store = object_store().await;
        fx.admin("BEGIN; SET LOCAL ROLE chelabase_storage; INSERT INTO storage.buckets (name, file_size_limit) VALUES ('mib', 1048576), ('twelve', 12582912); COMMIT;").await;
        let (status, _, b) = object("POST", "/storage/v1/object/mib/two.bin", &secret, Some(vec![1u8; 2 * MIB].into()), &[]).await;
        assert_eq!(status, 413, "{}", json_of(&b));
        let (status, b) = open_upload(&secret, "mib", "two.bin", "application/octet-stream", &[]).await;
        assert_eq!(status, 200, "{b}");
        let id = b["id"].as_str().unwrap().to_string();
        assert_eq!(put_part(&secret, &id, 1, vec![1u8; 2 * MIB], &[]).await.0, 413);
        assert_eq!(session_call(&secret, "DELETE", &id, "", &[]).await.0, 204);
        // Within the limit part by part, over it in total once it is lowered.
        let (_, b) = open_upload(&secret, "twelve", "ten.bin", "application/octet-stream", &[]).await;
        let id = b["id"].as_str().unwrap().to_string();
        for n in 1..=2 {
            assert_eq!(put_part(&secret, &id, n, part_bytes(n), &[]).await.0, 200);
        }
        fx.admin("BEGIN; SET LOCAL ROLE chelabase_storage; UPDATE storage.buckets SET file_size_limit = 8388608 WHERE name = 'twelve'; COMMIT;").await;
        let (status, b) = session_call(&secret, "POST", &id, "/complete", &[]).await;
        assert_eq!((status, b["error"]["code"].clone()), (413, json!("too_large")), "{b}");
        assert_eq!(session_call(&secret, "GET", &id, "", &[]).await.0, 404);
        assert_eq!(fx.value("SELECT count(*) FROM storage.objects").await, "0");
        assert_eq!(fx.value("SELECT count(*) FROM storage.upload_sessions").await, "0");
        assert!(fx.blobs(&store).await.is_empty(), "{:?}", fx.blobs(&store).await);
    }))
    .await;
}

/// AC 3: `allowed_mime_types = [image/*]`: HTML sent as `image/png` is 415, single or
/// multipart (part 1), and so is a declared `text/html`; a real PNG is stored.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_multipart_mime_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'")]).await;
        fx.admin("BEGIN; SET LOCAL ROLE chelabase_storage; INSERT INTO storage.buckets (name, allowed_mime_types) VALUES ('images', ARRAY['image/*']); COMMIT;").await;
        let html = b"<!DOCTYPE html><html><script>alert(1)</script></html>".to_vec();
        let png = [("content-type", "image/png")];
        let (status, _, b) = object("POST", "/storage/v1/object/images/x.png", &secret, Some(html.clone().into()), &png).await;
        assert_eq!((status, json_of(&b)["error"]["code"].clone()), (415, json!("mime_not_allowed")), "{}", json_of(&b));
        let (status, _, _) = object("POST", "/storage/v1/object/images/x.html", &secret, Some(html.clone().into()), &[("content-type", "text/html")]).await;
        assert_eq!(status, 415);
        assert_eq!(open_upload(&secret, "images", "y.html", "text/html", &[]).await.0, 415);
        let (status, b) = open_upload(&secret, "images", "y.png", "image/png", &[]).await;
        assert_eq!(status, 200, "{b}");
        let id = b["id"].as_str().unwrap().to_string();
        assert_eq!(put_part(&secret, &id, 1, html, &[]).await.0, 415);
        assert_eq!(session_call(&secret, "DELETE", &id, "", &[]).await.0, 204);
        let real = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x01\0\0\0\x01\x08\x06\0\0\0".to_vec();
        assert_eq!(object("POST", "/storage/v1/object/images/real.png", &secret, Some(real.into()), &png).await.0, 200);
        assert_eq!(fx.value("SELECT count(*) FROM storage.objects").await, "1");
    }))
    .await;
}

/// AC 4: a session past its 24 hours (its expiry brought forward, as only storage may) answers
/// 410 to parts, the list and completion; its row stays for m8's sweeper.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_multipart_expired_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'")]).await;
        let (_, b) = open_upload(&secret, "files", "old.bin", "application/octet-stream", &[]).await;
        let id = b["id"].as_str().unwrap().to_string();
        assert_eq!(put_part(&secret, &id, 1, b"x".to_vec(), &[]).await.0, 200);
        // Only storage writes a session, and it may only bring the expiry closer.
        assert!(fx.admin_on(&fx.ep, &format!("BEGIN; SET LOCAL ROLE chelabase_storage; UPDATE storage.upload_sessions SET expires_at = expires_at + interval '1 hour' WHERE id = '{id}'; COMMIT;")).await.is_err());
        assert!(fx.admin_on(&fx.ep, &format!("UPDATE storage.upload_sessions SET expires_at = now() - interval '1 second' WHERE id = '{id}'")).await.is_err());
        fx.admin(&format!("BEGIN; SET LOCAL ROLE chelabase_storage; UPDATE storage.upload_sessions SET expires_at = clock_timestamp() - interval '1 second' WHERE id = '{id}'; COMMIT;")).await;
        for (method, tail) in [("GET", ""), ("POST", "/complete")] {
            let (status, b) = session_call(&secret, method, &id, tail, &[]).await;
            assert_eq!((status, b["error"]["code"].clone()), (410, json!("upload_expired")), "{method} {tail}: {b}");
        }
        assert_eq!(put_part(&secret, &id, 2, b"y".to_vec(), &[]).await.0, 410);
        assert_eq!(fx.value("SELECT count(*) FROM storage.objects").await, "0");
        assert_eq!(fx.value("SELECT count(*) FROM storage.upload_sessions").await, "1");
    }))
    .await;
}

/// AC 5: the INSERT policy dropped mid-session: `complete` is 403 and the upload is aborted
/// (no row, no blob, no session, no ticket).
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_multipart_lost_permission_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'")]).await;
        let store = object_store().await;
        let (_, b) = open_upload(&secret, "files", "late.bin", "application/octet-stream", &[]).await;
        let id = b["id"].as_str().unwrap().to_string();
        assert_eq!(put_part(&secret, &id, 1, b"late".to_vec(), &[]).await.0, 200);
        fx.owner("SELECT storage.drop_policy('o_1', 'objects')").await.unwrap();
        let (status, b) = session_call(&secret, "POST", &id, "/complete", &[]).await;
        assert_eq!((status, b["error"]["code"].clone()), (403, json!("not_allowed")), "{b}");
        assert_eq!(session_call(&secret, "GET", &id, "", &[]).await.0, 404);
        assert_eq!(fx.value("SELECT count(*) FROM storage.objects").await, "0");
        assert_eq!(fx.value("SELECT count(*) FROM storage.upload_sessions").await, "0");
        assert_eq!(fx.value("SELECT count(*) FROM storage.upload_tickets").await, "0");
        assert!(fx.blobs(&store).await.is_empty());
    }))
    .await;
}

/// AC 9: another caller's session id (another app user, a secret key without a user, or a
/// second secret key on the same role) is 404 on every session route, and the owner's session
/// is untouched.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_multipart_other_callers_session_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        // Each caller its own rows: a user's, or (no user: `owner` and `chelabase.user_id()`
        // both NULL) a secret key's.
        let secret = objects_fixture(fx, &[("SELECT", "'owner IS NOT DISTINCT FROM chelabase.user_id()'", "NULL"), ("INSERT", "NULL", "'owner IS NOT DISTINCT FROM chelabase.user_id()'")]).await;
        let public = fx.public_key().await;
        let (alice, bob) = (fx.user_token(Uuid::new_v4()).await, fx.user_token(Uuid::new_v4()).await);
        let as_alice = [("chelabase-user", alice.as_str())];
        let as_bob = [("chelabase-user", bob.as_str())];
        let (status, b) = open_upload(&public, "files", "alice.bin", "application/octet-stream", &as_alice).await;
        assert_eq!(status, 200, "{b}");
        let id = b["id"].as_str().unwrap().to_string();
        assert_eq!(put_part(&public, &id, 1, b"a".to_vec(), &as_alice).await.0, 200);
        // A secret key's own session: another secret key on the same role can't reach it.
        let (status, b) = open_upload(&secret, "files", "keyed.bin", "application/octet-stream", &[]).await;
        assert_eq!(status, 200, "{b}");
        let keyed = b["id"].as_str().unwrap().to_string();
        let second = fx.secret_key("st-fix-second", None).await;
        for (method, tail) in [("GET", ""), ("POST", "/complete"), ("DELETE", "")] {
            assert_eq!(session_call(&second, method, &keyed, tail, &[]).await.0, 404, "{method} {tail}");
        }
        assert_eq!(put_part(&second, &keyed, 1, b"evil".to_vec(), &[]).await.0, 404);
        assert_eq!(session_call(&secret, "GET", &keyed, "", &[]).await.0, 200);
        for (key, headers) in [(public.as_str(), &as_bob[..]), (secret.as_str(), &[][..])] {
            assert_eq!(put_part(key, &id, 1, b"evil".to_vec(), headers).await.0, 404);
            for (method, tail) in [("GET", ""), ("POST", "/complete"), ("DELETE", "")] {
                let (status, b) = session_call(key, method, &id, tail, headers).await;
                assert_eq!((status, b["error"]["code"].clone()), (404, json!("upload_not_found")), "{method} {tail}: {b}");
            }
        }
        let (status, b) = session_call(&public, "POST", &id, "/complete", &as_alice).await;
        assert_eq!((status, b["size"].clone()), (200, json!(1)), "{b}");
        assert_eq!(&object("GET", "/storage/v1/object/files/alice.bin", &public, None, &as_alice).await.2[..], b"a");
    }))
    .await;
}

/// AC 7: a copy or move into a bucket whose limit is under the object's size is 413: nothing
/// is written, and the source is unchanged.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_multipart_destination_limits_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'"), ("UPDATE", "'true'", "'true'")]).await;
        let store = object_store().await;
        assert_eq!(object("POST", "/storage/v1/object/files/big.bin", &secret, Some(vec![4u8; 2000].into()), &[]).await.0, 200);
        let before = fx.blobs(&store).await;
        for route in ["copy", "move"] {
            let body = json!({ "bucket": "files", "from": "big.bin", "to": "big.bin", "to_bucket": "capped" }).to_string();
            let (status, _, b) = object("POST", &format!("/storage/v1/object/{route}"), &secret, Some(body.into()), &[JSON]).await;
            assert_eq!((status, json_of(&b)["error"]["code"].clone()), (413, json!("too_large")), "{route}: {}", json_of(&b));
        }
        assert_eq!(fx.value("SELECT string_agg(b.name || '/' || o.name, ',') FROM storage.objects o JOIN storage.buckets b ON b.id = o.bucket_id").await, "files/big.bin");
        assert_eq!(fx.blobs(&store).await, before);
        assert_eq!(fx.value("SELECT count(*) FROM storage.upload_tickets").await, "0");
        assert_eq!(object("GET", "/storage/v1/object/files/big.bin", &secret, None, &[]).await.2.len(), 2000);
    }))
    .await;
}

/// AC 6: the storage service in this process with a 6 MiB cap on upload memory. A part
/// holds only the bytes it has sent (m7 re-audit S2): while a slow 5 MiB part streams, a small
/// part is still taken (nothing is held up front, so a trickling client can't pin a part's
/// worth), but a 5 MiB part that doesn't fit beside it is 503 `busy`; once the first is
/// done, it is taken.
#[tokio::test]
#[ignore = "needs the workbench stack"]
async fn storage_multipart_memory_cap_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'")]).await;
        let store = Arc::new(FailingStore::default());
        let (base, token) = in_process_storage_with(store, fx.main, storage::limits::InFlight::new(6 * MIB)).await;
        let call = |method: &'static str, path: String, body: reqwest::Body| {
            let (base, token) = (base.clone(), token.clone());
            async move {
                let res = reqwest::Client::new()
                    .request(method.parse().unwrap(), format!("{base}{path}"))
                    .header(chelabase_auth::internal::IDENTITY_HEADER, token)
                    .body(body)
                    .send()
                    .await
                    .unwrap();
                let status = res.status().as_u16();
                (status, res.json::<Value>().await.unwrap_or_default())
            }
        };
        let (status, b) = call("POST", "/storage/v1/upload/files/cap.bin".into(), reqwest::Body::from(Vec::new())).await;
        assert_eq!(status, 200, "{b}");
        let id = b["id"].as_str().unwrap().to_string();
        let sent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let past = Arc::new(tokio::sync::Notify::new());
        // 5 MiB at about 3 MiB/s, chunked: the server holds what it has read so far.
        let slow = counted_body(5 * MIB, Duration::from_millis(20), sent, 2 * MIB, past.clone());
        let chunked = |n: usize| reqwest::Body::wrap_stream(futures_util::stream::iter([Ok::<_, std::io::Error>(bytes::Bytes::from(vec![1u8; n]))]));
        // Bounded: if the first part were refused at once, its mark would never come.
        let (first, (small, big)) = tokio::time::timeout(Duration::from_secs(60), async {
            tokio::join!(
                call("PUT", format!("/storage/v1/upload/session/{id}/1"), slow),
                async {
                    past.notified().await;
                    let small = call("PUT", format!("/storage/v1/upload/session/{id}/2"), chunked(1024)).await;
                    let big = call("PUT", format!("/storage/v1/upload/session/{id}/3"), chunked(5 * MIB)).await;
                    (small, big)
                },
            )
        })
        .await
        .expect("the first part never reached 2 MiB: it was refused or stalled");
        assert_eq!(first.0, 200, "{}", first.1);
        assert_eq!(small.0, 200, "{}", small.1);
        assert_eq!((big.0, big.1["error"]["code"].clone()), (503, json!("busy")), "{}", big.1);
        let (status, b) = call("PUT", format!("/storage/v1/upload/session/{id}/3"), chunked(5 * MIB)).await;
        assert_eq!(status, 200, "{b}");
        assert_eq!(call("DELETE", format!("/storage/v1/upload/session/{id}"), reqwest::Body::from(Vec::new())).await.0, 204);
    }))
    .await;
}

/// m7 follow-up of m5: the stack's object store passes the conformance suite, including a copy
/// made of `UploadPartCopy` parts (what a copy over 5 GB takes).
#[tokio::test]
#[ignore = "needs the workbench stack"]
async fn storage_multipart_part_copy_live() {
    let prefix = format!("conformance-m7-{}", Uuid::new_v4());
    chelabase_storage::conformance::run(&object_store().await, &prefix).await;
}

// --- m7: write-once buckets (decided 2026-11-02) ---------------------------------------------

/// A call to the in-process storage service: `(status, body)`.
async fn raw(base: &str, token: &str, method: &str, path: &str, body: Vec<u8>, headers: &[(&str, &str)]) -> (u16, bytes::Bytes) {
    let mut req = reqwest::Client::new()
        .request(method.parse().unwrap(), format!("{base}{path}"))
        .header(chelabase_auth::internal::IDENTITY_HEADER, token)
        .body(body);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = req.send().await.unwrap();
    let status = res.status().as_u16();
    (status, res.bytes().await.unwrap_or_default())
}

/// A write-once bucket, enforced by the database for every role, through this branch's storage
/// service in this process (the stack's runs the epic's earlier build): `write_once` is set on
/// create or update and never turned off; an object there is uploaded once and never
/// overwritten (an upsert is 409 `write_once`), moved, renamed or deleted; a multipart
/// completion onto a name taken meanwhile is refused; the bucket, holding objects, is neither
/// deleted nor renamed, though its other settings still change; empty, it still is. The
/// owner, storage's own role and the superuser are refused in SQL alike (`CB005`), and an
/// ordinary bucket's objects stay deletable. A copy into a new name is an upload. The objects
/// stay referenced, so a sweep keeps their blobs. The schema is stamped 7 and healthy; a
/// version-6 shape is migrated by the bootstrap (the column and the guards added, stamp 7).
#[tokio::test]
#[ignore = "needs the workbench stack"]
async fn storage_write_once_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        const TEXT: (&str, &str) = ("content-type", "text/plain");
        let stamp = "SELECT obj_description('storage'::regnamespace, 'pg_namespace')";
        let check = || async { last_line(&fx.admin(&schema_check_sql("public", Some(&fx.verifier().await))).await) };
        assert_eq!(fx.value(stamp).await, storage_schema::STORAGE_SCHEMA_STAMP);
        assert_eq!(check().await, "t");
        // Every policy open for `authenticated`: only write-once stands in the way.
        objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'"), ("UPDATE", "'true'", "'true'"), ("DELETE", "'true'", "NULL")]).await;
        for (command, using, check_expr) in [("INSERT", "NULL", "'true'"), ("UPDATE", "'true'", "'true'"), ("DELETE", "'true'", "NULL")] {
            fx.owner(&format!(
                "SELECT storage.create_policy('b_{}', 'buckets', '{command}', ARRAY['authenticated']::name[], {using}, {check_expr})",
                command.to_lowercase()
            ))
            .await
            .unwrap();
        }
        let store = Arc::new(FailingStore::default());
        let (base, token) = in_process_storage(store.clone(), fx.main).await;
        let call = async |method: &str, path: &str, body: Vec<u8>, headers: &[(&str, &str)]| {
            let (status, b) = raw(&base, &token, method, path, body, headers).await;
            (status, json_of(&b))
        };
        let code = |b: &Value| b["error"]["code"].clone();
        let body = |v: Value| v.to_string().into_bytes();
        let bucket = |name: &str| format!("/storage/v1/bucket/{name}");
        let up = |name: &str| format!("/storage/v1/object/frozen/{name}");

        // Set on create, listed, never turned off; turned on later on an ordinary bucket.
        let (status, b) = call("POST", "/storage/v1/bucket", body(json!({ "name": "frozen", "write_once": true })), &[JSON]).await;
        assert_eq!((status, b["write_once"].clone()), (201, json!(true)), "{b}");
        let (status, list) = call("GET", "/storage/v1/bucket", vec![], &[]).await;
        assert_eq!(status, 200, "{list}");
        let flag = |name: &str| list.as_array().unwrap().iter().find(|x| x["name"] == name).unwrap()["write_once"].clone();
        assert_eq!((flag("frozen"), flag("files")), (json!(true), json!(false)), "{list}");
        let (status, b) = call("PATCH", &bucket("frozen"), body(json!({ "write_once": false })), &[JSON]).await;
        assert_eq!((status, code(&b)), (409, json!("write_once")), "{b}");
        assert_eq!(call("PATCH", &bucket("other"), body(json!({ "write_once": true })), &[JSON]).await.0, 204);
        let (status, b) = call("PATCH", &bucket("other"), body(json!({ "write_once": false })), &[JSON]).await;
        assert_eq!((status, code(&b)), (409, json!("write_once")), "{b}");
        assert_eq!(fx.value("SELECT write_once FROM storage.buckets WHERE name = 'other'").await, "t");
        // Empty, a write-once bucket is still renamed and deleted.
        assert_eq!(call("PATCH", &bucket("other"), body(json!({ "name": "thawed" })), &[JSON]).await.0, 204);
        assert_eq!(call("DELETE", &bucket("thawed"), vec![], &[]).await.0, 204);
        // The first object and a rename race: while a rename of an empty write-once bucket is
        // uncommitted, an upload into it waits (`name` is unique, so the rename holds the row
        // `FOR UPDATE` against the foreign key check's `FOR KEY SHARE`), then lands in the
        // renamed bucket; the bucket is never renamed past an object it holds.
        assert_eq!(call("POST", "/storage/v1/bucket", body(json!({ "name": "racing", "write_once": true })), &[JSON]).await.0, 201);
        let mut renamer = session(fx, "chelabase_storage", &fx.storage_password().await).await;
        send(&mut renamer, "BEGIN; UPDATE storage.buckets SET name = 'raced' WHERE name = 'racing'; SELECT 'held';").await;
        assert_eq!(read_line(&mut renamer).await, "held");
        let mut first = tokio::spawn({
            let (base, token) = (base.clone(), token.clone());
            async move { raw(&base, &token, "POST", "/storage/v1/object/racing/first.txt", b"first".to_vec(), &[TEXT]).await }
        });
        assert!(tokio::time::timeout(Duration::from_secs(2), &mut first).await.is_err(), "the first object went in under an uncommitted rename");
        send(&mut renamer, "COMMIT; SELECT 'renamed';").await;
        assert_eq!(read_line(&mut renamer).await, "renamed");
        let (status, b) = first.await.unwrap();
        assert_eq!(status, 200, "{}", json_of(&b));
        assert_eq!(fx.value("SELECT count(*) FROM storage.objects o JOIN storage.buckets b ON b.id = o.bucket_id WHERE b.name = 'raced'").await, "1");
        drop(renamer);

        // Uploaded once: a second upload is the usual conflict, an upsert (either spelling, and
        // the PUT route) the write-once refusal; the object is unchanged.
        let (status, b) = call("POST", &up("a.txt"), b"one".to_vec(), &[TEXT]).await;
        assert_eq!(status, 200, "{b}");
        let version = fx.value("SELECT version_id FROM storage.objects WHERE name = 'a.txt'").await;
        let (status, b) = call("POST", &up("a.txt"), b"two".to_vec(), &[TEXT]).await;
        assert_eq!((status, code(&b)), (409, json!("conflict")), "{b}");
        let (status, b) = call("POST", &format!("{}?upsert=true", up("a.txt")), b"two".to_vec(), &[TEXT]).await;
        assert_eq!((status, code(&b)), (409, json!("write_once")), "{b}");
        let (status, b) = call("POST", &up("a.txt"), b"two".to_vec(), &[TEXT, ("x-upsert", "true")]).await;
        assert_eq!((status, code(&b)), (409, json!("write_once")), "{b}");
        let (status, b) = call("PUT", &up("a.txt"), b"two".to_vec(), &[TEXT, ("x-upsert", "true")]).await;
        assert_eq!((status, code(&b)), (409, json!("write_once")), "{b}");
        // Never moved, renamed or deleted, one at a time or in a batch.
        for to in [json!({ "bucket": "frozen", "from": "a.txt", "to": "b.txt" }), json!({ "bucket": "frozen", "from": "a.txt", "to": "a.txt", "to_bucket": "files" })] {
            let (status, b) = call("POST", "/storage/v1/object/move", body(to.clone()), &[JSON]).await;
            assert_eq!((status, code(&b)), (409, json!("write_once")), "{to}: {b}");
        }
        let (status, b) = call("DELETE", &up("a.txt"), vec![], &[]).await;
        assert_eq!((status, code(&b)), (409, json!("write_once")), "{b}");
        let (status, b) = call("DELETE", "/storage/v1/object/frozen", body(json!({ "names": ["a.txt"] })), &[JSON]).await;
        assert_eq!((status, code(&b)), (409, json!("write_once")), "{b}");
        let (status, got) = raw(&base, &token, "GET", &up("a.txt"), vec![], &[]).await;
        assert_eq!((status, &got[..]), (200, &b"one"[..]));
        assert_eq!(fx.value("SELECT version_id FROM storage.objects WHERE name = 'a.txt'").await, version);
        assert_eq!(fx.value("SELECT count(*) FROM storage.upload_tickets").await, "0", "a refused move left a ticket");

        // A copy in is an upload (a new name); onto an existing name, the usual conflict; a
        // copy out is free.
        assert_eq!(call("POST", "/storage/v1/object/files/src.txt", b"src".to_vec(), &[TEXT]).await.0, 200);
        let (status, b) = call("POST", "/storage/v1/object/copy", body(json!({ "bucket": "files", "from": "src.txt", "to": "c.txt", "to_bucket": "frozen" })), &[JSON]).await;
        assert_eq!(status, 200, "{b}");
        let (status, b) = call("POST", "/storage/v1/object/copy", body(json!({ "bucket": "files", "from": "src.txt", "to": "a.txt", "to_bucket": "frozen" })), &[JSON]).await;
        assert_eq!((status, code(&b)), (409, json!("conflict")), "{b}");
        let (status, b) = call("POST", "/storage/v1/object/copy", body(json!({ "bucket": "frozen", "from": "a.txt", "to": "out.txt", "to_bucket": "files" })), &[JSON]).await;
        assert_eq!(status, 200, "{b}");

        // Multipart: an upsert onto an existing name is refused as it opens; a completion onto a
        // name taken meanwhile is refused, without upsert (the usual conflict) or with it.
        let (status, b) = call("POST", "/storage/v1/upload/frozen/a.txt?upsert=true", vec![], &[TEXT]).await;
        assert_eq!((status, code(&b)), (409, json!("write_once")), "{b}");
        for (name, upsert, expected) in [("m.bin", "", "conflict"), ("n.bin", "?upsert=true", "write_once")] {
            let (status, b) = call("POST", &format!("/storage/v1/upload/frozen/{name}{upsert}"), vec![], &[TEXT]).await;
            assert_eq!(status, 200, "{b}");
            let id = b["id"].as_str().unwrap().to_string();
            assert_eq!(call("PUT", &format!("/storage/v1/upload/session/{id}/1"), b"part".to_vec(), &[]).await.0, 200);
            assert_eq!(call("POST", &up(name), b"first".to_vec(), &[TEXT]).await.0, 200);
            let (status, b) = call("POST", &format!("/storage/v1/upload/session/{id}/complete"), vec![], &[]).await;
            assert_eq!((status, code(&b)), (409, json!(expected)), "{name}: {b}");
            // The session is this caller's to abort.
            assert_eq!(call("DELETE", &format!("/storage/v1/upload/session/{id}"), vec![], &[]).await.0, 204);
            let (status, got) = raw(&base, &token, "GET", &up(name), vec![], &[]).await;
            assert_eq!((status, &got[..]), (200, &b"first"[..]), "{name}");
        }

        // The bucket holding objects: neither deleted nor renamed; its other settings still change.
        let (status, b) = call("DELETE", &bucket("frozen"), vec![], &[]).await;
        assert_eq!((status, code(&b)), (409, json!("bucket_not_empty")), "{b}");
        let (status, b) = call("PATCH", &bucket("frozen"), body(json!({ "name": "thawed" })), &[JSON]).await;
        assert_eq!((status, code(&b)), (409, json!("write_once")), "{b}");
        assert_eq!(call("PATCH", &bucket("frozen"), body(json!({ "public": true, "file_size_limit": 10 })), &[JSON]).await.0, 204);
        assert_eq!(fx.value("SELECT name || ' ' || public || ' ' || file_size_limit || ' ' || write_once FROM storage.buckets WHERE name = 'frozen'").await, "frozen true 10 true");

        // In SQL, no role is exempt: the owner (BYPASSRLS, pg_write_all_data), storage's own
        // role (BYPASSRLS) and the superuser; an ordinary bucket's object is still deleted.
        for statement in [
            "DELETE FROM storage.objects WHERE name = 'a.txt'",
            "UPDATE storage.objects SET name = 'z.txt' WHERE name = 'a.txt'",
            "UPDATE storage.objects SET metadata = '{\"k\": 1}' WHERE name = 'a.txt'",
            "UPDATE storage.buckets SET write_once = false WHERE name = 'frozen'",
            "UPDATE storage.buckets SET name = 'thawed' WHERE name = 'frozen'",
        ] {
            for role in [OWNER, "chelabase_storage"] {
                assert_eq!(fx.code(role, statement).await, "CB005", "{role}: {statement}");
            }
            assert!(fx.admin_on(&fx.ep, statement).await.is_err(), "the superuser: {statement}");
        }
        assert_eq!(fx.code("chelabase_storage", "DELETE FROM storage.objects WHERE name = 'src.txt'").await, "ok");
        assert_eq!(fx.value("SELECT count(*) FROM storage.objects o JOIN storage.buckets b ON b.id = o.bucket_id WHERE b.name = 'frozen'").await, "4");

        // A sweep keeps every blob a row references (all of them: a write-once row is never
        // gone), and is really sweeping: an unreferenced blob beside them is marked, then waits.
        let total: usize = fx.value("SELECT count(*) FROM storage.objects").await.parse().unwrap();
        let stray = format!("{}{}", fx.prefix().await, Uuid::new_v4());
        chelabase_storage::ObjectStore::put(&*store, &stray, bytes::Bytes::from_static(b"stray"), chelabase_storage::PutOptions::default()).await.unwrap();
        for pass in 0..2 {
            let r = worker::storage_sweep::sweep_database(&fx.worker, &*store, fx.id, Duration::ZERO, false).await.unwrap();
            assert_eq!((r.blocked.clone(), r.blobs, r.referenced, r.deleted), (None, total + 1, total, 0), "pass {pass}: {r:?}");
            assert_eq!((r.marked, r.waiting), if pass == 0 { (1, 0) } else { (0, 1) }, "pass {pass}: {r:?}");
        }
        let (status, got) = raw(&base, &token, "GET", &up("a.txt"), vec![], &[]).await;
        assert_eq!((status, &got[..]), (200, &b"one"[..]));

        // Migration: a version-6 shape (no column, no guards, stamped 6) is brought to 7 by
        // the bootstrap; the objects and the bucket's other settings are untouched.
        fx.admin(
            "BEGIN; SET LOCAL ROLE chelabase_admin;
             DROP TRIGGER guard_buckets ON storage.buckets; DROP TRIGGER freeze_objects ON storage.objects;
             ALTER TABLE storage.buckets DROP COLUMN write_once;
             COMMENT ON SCHEMA storage IS 'chelabase storage schema 6'; COMMIT;
             DROP FUNCTION storage.guard_buckets(); DROP FUNCTION storage.freeze_objects();",
        )
        .await;
        assert_eq!(check().await, "f");
        assert_eq!(fx.code(OWNER, "DELETE FROM storage.objects WHERE name = 'c.txt'").await, "ok", "version 6 has no guard");
        bootstrap(fx).await;
        assert_eq!(fx.value(stamp).await, storage_schema::STORAGE_SCHEMA_STAMP);
        assert_eq!(check().await, "t");
        assert_eq!(fx.value("SELECT name || ' ' || public || ' ' || file_size_limit || ' ' || write_once FROM storage.buckets WHERE name = 'frozen'").await, "frozen true 10 false", "a migrated bucket starts ordinary");
        fx.owner("UPDATE storage.buckets SET write_once = true WHERE name = 'frozen'").await.unwrap();
        assert_eq!(fx.code(OWNER, "DELETE FROM storage.objects WHERE name = 'a.txt'").await, "CB005");
        assert_eq!(fx.value("SELECT count(*) FROM storage.objects o JOIN storage.buckets b ON b.id = o.bucket_id WHERE b.name = 'frozen'").await, "4");
    }))
    .await;
}

// --- m7 fix round (the audit's findings) -----------------------------------------------------

/// A call to the in-process storage service with a streamed body, bounded by `within`:
/// `Some((status, body))`, or `None` when it was still waiting at the end.
async fn streamed(base: &str, token: &str, method: &str, path: &str, body: reqwest::Body, within: Duration) -> Option<(u16, Value)> {
    let res = reqwest::Client::new()
        .request(method.parse().unwrap(), format!("{base}{path}"))
        .header(chelabase_auth::internal::IDENTITY_HEADER, token)
        .header("content-type", "application/octet-stream")
        .body(body)
        .send();
    match tokio::time::timeout(within, res).await {
        Ok(Ok(res)) => {
            let status = res.status().as_u16();
            Some((status, res.json().await.unwrap_or_default()))
        }
        Ok(Err(e)) => Some((0, json!(e.to_string()))),
        Err(_) => None,
    }
}

/// B1 (in-process): HTML after 600 spaces, or after a
/// UTF-8 BOM and whitespace, declared as `image/png` into an `image/*` bucket is 415 as a
/// single upload and as part 1 alike; nothing is stored.
#[tokio::test]
#[ignore = "needs the workbench stack"]
async fn storage_multipart_padded_html_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'")]).await;
        fx.admin("BEGIN; SET LOCAL ROLE chelabase_storage; INSERT INTO storage.buckets (name, allowed_mime_types) VALUES ('images', ARRAY['image/*']); COMMIT;").await;
        let (base, token) = in_process_storage(Arc::new(FailingStore::default()), fx.main).await;
        let html: &[u8] = b"<!DOCTYPE html><html><script>alert(1)</script></html>";
        let mut spaces = vec![b' '; 600];
        spaces.extend_from_slice(html);
        let mut bom = b"\xef\xbb\xbf".to_vec();
        bom.extend(std::iter::repeat_n(b' ', 600));
        bom.extend_from_slice(html);
        let png = [("content-type", "image/png")];
        for (i, padded) in [spaces, bom].into_iter().enumerate() {
            let (status, b) = raw(&base, &token, "POST", &format!("/storage/v1/object/images/pad{i}.png"), padded.clone(), &png).await;
            assert_eq!(status, 415, "case {i}, single: {}", json_of(&b));
            let (status, b) = raw(&base, &token, "POST", &format!("/storage/v1/upload/images/part{i}.png"), vec![], &png).await;
            assert_eq!(status, 200, "{}", json_of(&b));
            let id = json_of(&b)["id"].as_str().unwrap().to_string();
            let (status, b) = raw(&base, &token, "PUT", &format!("/storage/v1/upload/session/{id}/1"), padded, &[]).await;
            assert_eq!(status, 415, "case {i}, part 1: {}", json_of(&b));
        }
        assert_eq!(fx.value("SELECT count(*) FROM storage.objects").await, "0");
    }))
    .await;
}

/// S2 (in-process): a body that stops sending, a part's or a single upload's, fails with 408
/// `request_timeout` within the idle period and frees its share of the upload memory; nothing
/// is stored. A slow body whose chunks each come within the period, though it takes several
/// periods in all, is stored whole.
#[tokio::test]
#[ignore = "needs the workbench stack"]
async fn storage_multipart_stalled_body_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'")]).await;
        let cap = storage::limits::InFlight::new(150 * MIB).with_body_idle(Duration::from_secs(1));
        let (base, token) = in_process_storage_with(Arc::new(FailingStore::default()), fx.main, cap.clone()).await;
        let stalling = || {
            let first = futures_util::stream::once(async { Ok::<_, std::io::Error>(bytes::Bytes::from(vec![1u8; 64 * 1024])) });
            reqwest::Body::wrap_stream(first.chain(futures_util::stream::pending()))
        };
        let (status, b) = raw(&base, &token, "POST", "/storage/v1/upload/files/stall.bin", vec![], &[]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        let id = json_of(&b)["id"].as_str().unwrap().to_string();
        let within = Duration::from_secs(20);
        for path in [format!("/storage/v1/upload/session/{id}/1"), "/storage/v1/object/files/stall.bin".to_owned()] {
            let started = Instant::now();
            let got = streamed(&base, &token, if path.contains("/session/") { "PUT" } else { "POST" }, &path, stalling(), within).await;
            let (status, b) = got.unwrap_or_else(|| panic!("{path}: still waiting after {within:?}"));
            assert_eq!((status, b["error"]["code"].clone()), (408, json!("request_timeout")), "{path}: {b}");
            assert!(started.elapsed() < Duration::from_secs(10), "{path}: {:?}", started.elapsed());
            assert_eq!(cap.available(), 150 * MIB, "{path}: the reservation is still held");
        }
        assert_eq!(fx.value("SELECT count(*) FROM storage.objects").await, "0");
        // Slow but live: 8 chunks 400 ms apart (3.2 s in all, the period 1 s).
        let (status, b) = streamed(&base, &token, "PUT", &format!("/storage/v1/upload/session/{id}/1"), paced_body(512 * 1024, 3, Duration::from_millis(400), None), within).await.unwrap();
        assert_eq!((status, b["size"].clone()), (200, json!(512 * 1024)), "{b}");
        assert_eq!(raw(&base, &token, "DELETE", &format!("/storage/v1/upload/session/{id}"), vec![], &[]).await.0, 204);
    }))
    .await;
}

/// N2 (in-process): a version-6 `storage.upload_sessions` made before the sessions were bound
/// to the data key (no `key_id`) is healed by the bootstrap: the column is added, `NOT NULL`,
/// and a session opens.
#[tokio::test]
#[ignore = "needs the workbench stack"]
async fn storage_multipart_keyless_sessions_healed_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'")]).await;
        let column = "SELECT count(*) || ' ' || coalesce(bool_and(attnotnull)::text, '-') FROM pg_attribute WHERE attrelid = 'storage.upload_sessions'::regclass AND attname = 'key_id' AND NOT attisdropped";
        assert_eq!(fx.value(column).await, "1 true");
        fx.admin("BEGIN; SET LOCAL ROLE chelabase_admin; ALTER TABLE storage.upload_sessions DROP COLUMN key_id; COMMENT ON SCHEMA storage IS 'chelabase storage schema 6'; COMMIT;").await;
        assert_eq!(fx.value(column).await, "0 -");
        bootstrap(fx).await;
        assert_eq!(fx.value("SELECT obj_description('storage'::regnamespace, 'pg_namespace')").await, storage_schema::STORAGE_SCHEMA_STAMP);
        assert_eq!(fx.value(column).await, "1 true");
        let (base, token) = in_process_storage(Arc::new(FailingStore::default()), fx.main).await;
        let (status, b) = raw(&base, &token, "POST", "/storage/v1/upload/files/healed.bin", vec![], &[]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
    }))
    .await;
}

/// S1 (decided): `storage.upload_sessions` is nothing to the API's roles; the database owner
/// (trusted, with `pg_read_all_data` / `pg_write_all_data`) reads and deletes any caller's
/// sessions but never forges or rewrites one, nor does storage rebind one. Deleting a session
/// only cancels its upload (404 after), never a stored object. Pinned, so a change is deliberate.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_multipart_sessions_access_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'")]).await;
        assert_eq!(object("POST", "/storage/v1/object/files/kept.txt", &secret, Some(b"kept".to_vec().into()), &[("content-type", "text/plain")]).await.0, 200);
        let (status, b) = open_upload(&secret, "files", "s.bin", "application/octet-stream", &[]).await;
        assert_eq!(status, 200, "{b}");
        let id = b["id"].as_str().unwrap().to_string();
        for role in ["anon", "authenticated", "authenticator"] {
            for st in ["SELECT 1 FROM storage.upload_sessions", "DELETE FROM storage.upload_sessions", "UPDATE storage.upload_sessions SET parts = '{}'", "SELECT storage.record_part(gen_random_uuid(), 1, 1, 'x')"] {
                assert_eq!(fx.code(role, st).await, "42501", "{role}: {st}");
            }
        }
        let forge = format!("INSERT INTO storage.upload_sessions SELECT gen_random_uuid(), bucket_id, name, version_id, upload_id, content_type, upsert, datoid, caller_role, owner, key_id, parts, timeline_id, expires_at FROM storage.upload_sessions WHERE id = '{id}'");
        let got = [
            fx.code(OWNER, "SELECT count(*) FROM storage.upload_sessions").await,
            fx.code(OWNER, &forge).await,
            fx.code(OWNER, &format!("UPDATE storage.upload_sessions SET parts = '{{}}' WHERE id = '{id}'")).await,
            fx.code("chelabase_storage", &format!("UPDATE storage.upload_sessions SET caller_role = 'anon' WHERE id = '{id}'")).await,
            fx.code(OWNER, &format!("DELETE FROM storage.upload_sessions WHERE id = '{id}'")).await,
        ];
        assert_eq!(got, ["ok", "CB004", "CB004", "22023", "ok"], "[select, forge, rewrite, rebind, delete]");
        fx.as_role_commit(OWNER, &format!("DELETE FROM storage.upload_sessions WHERE id = '{id}'")).await;
        assert_eq!(fx.value("SELECT count(*) FROM storage.upload_sessions").await, "0");
        assert_eq!(put_part(&secret, &id, 1, b"late".to_vec(), &[]).await.0, 404);
        let (status, _, kept) = object("GET", "/storage/v1/object/files/kept.txt", &secret, None, &[]).await;
        assert_eq!((status, &kept[..]), (200, &b"kept"[..]));
    }))
    .await;
}

/// The audit's: two completions of one session at once: exactly one wins, the other is 409,
/// the object is whole and no session is left. Repeated to give the race a chance.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_multipart_concurrent_complete_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'"), ("UPDATE", "'true'", "'true'")]).await;
        for round in 0..4 {
            let name = format!("race{round}.bin");
            let up = if round % 2 == 1 { vec![("x-upsert", "true")] } else { vec![] };
            let (s, b) = open_upload(&secret, "files", &name, "application/octet-stream", &up).await;
            assert_eq!(s, 200, "{b}");
            let id = b["id"].as_str().unwrap().to_string();
            assert_eq!(put_part(&secret, &id, 1, part_bytes(1), &[]).await.0, 200);
            assert_eq!(put_part(&secret, &id, 2, b"tail".to_vec(), &[]).await.0, 200);
            let (a, c) = tokio::join!(session_call(&secret, "POST", &id, "/complete", &[]), session_call(&secret, "POST", &id, "/complete", &[]));
            // The loser is 409 when it got past the lookup, 404 when the winner had already
            // deleted the session.
            let mut st = [(a.0, a.1["error"]["code"].clone()), (c.0, c.1["error"]["code"].clone())];
            st.sort_by_key(|x| x.0);
            assert_eq!(st[0].0, 200, "round {round}: {} / {}", a.1, c.1);
            assert!(matches!(&st[1], (409, v) if v == "conflict") || matches!(&st[1], (404, v) if v == "upload_not_found"), "round {round}: {} / {}", a.1, c.1);
            let (s, _, got) = object("GET", &format!("/storage/v1/object/files/{name}"), &secret, None, &[]).await;
            let mut want = part_bytes(1);
            want.extend_from_slice(b"tail");
            assert!(s == 200 && got[..] == want[..], "round {round}: {s}, {} bytes", got.len());
        }
        assert_eq!(fx.value("SELECT count(*) FROM storage.upload_sessions").await, "0");
    }))
    .await;
}

/// The audit's, AC 2 on counted bytes: a chunked single upload (no `Content-Length`) over the
/// limit is 413, and parts sent in parallel that each fit but together don't make a 413
/// completion; no row, no blob.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_multipart_counted_limits_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        let secret = objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'")]).await;
        let store = object_store().await;
        fx.admin("BEGIN; SET LOCAL ROLE chelabase_storage; INSERT INTO storage.buckets (name, file_size_limit) VALUES ('mib', 1048576), ('eight', 8388608); COMMIT;").await;
        let chunks: Vec<Result<bytes::Bytes, std::io::Error>> = (0..8).map(|_| Ok(bytes::Bytes::from(vec![7u8; 256 << 10]))).collect();
        let (s, _, b) = object("POST", "/storage/v1/object/mib/chunked.bin", &secret, Some(reqwest::Body::wrap_stream(futures_util::stream::iter(chunks))), &[]).await;
        assert_eq!(s, 413, "{}", json_of(&b));
        let (_, b) = open_upload(&secret, "eight", "par.bin", "application/octet-stream", &[]).await;
        let id = b["id"].as_str().unwrap().to_string();
        let (p1, p2) = tokio::join!(put_part(&secret, &id, 1, part_bytes(1), &[]), put_part(&secret, &id, 2, part_bytes(2), &[]));
        let (s, b) = session_call(&secret, "POST", &id, "/complete", &[]).await;
        assert_eq!((s, b["error"]["code"].clone()), (413, json!("too_large")), "parts {} {}: {b}", p1.0, p2.0);
        assert_eq!(fx.value("SELECT count(*) FROM storage.objects").await, "0");
        assert!(fx.blobs(&store).await.is_empty(), "{:?}", fx.blobs(&store).await);
    }))
    .await;
}

// --- m9: Studio's storage routes through the gateway, as a member acting as a role -----------

/// The demo member's management token (`chelabase dev token`).
async fn dev_token() -> String {
    let cli = std::env::var("CHELABASE_CLI").expect("CHELABASE_CLI (run through just workbench test)");
    let out = tokio::process::Command::new(cli)
        .args(["dev", "token"])
        .current_dir(std::env::var("CHELABASE_CHECKOUT").map_or_else(|_| workspace_root(), Into::into))
        .output()
        .await
        .expect("chelabase dev token");
    assert!(out.status.success(), "chelabase dev token failed");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A member of the demo tenant made for one test, as the e2e scripts make one
/// (`scripts/lib/member-sign-in.sh`): invited by link, accepted (which sets the password and
/// signs in), then granted on the project. Acting as `authenticated` needs a grant naming it
/// (R18: a tenant admin, as the dev member is, never holds the data API's roles).
struct Member {
    id: String,
    token: String,
}

impl Member {
    /// A member with `grant` (a `PUT …/grants/{member}` body) on `fx`'s project.
    async fn granted(fx: &Fx, grant: Value) -> Member {
        let studio = std::env::var("CHELABASE_ENTRY_STUDIO_URL").expect("CHELABASE_ENTRY_STUDIO_URL (run through just workbench test)");
        let email = format!("st-console-{}@example.com", Uuid::new_v4().simple());
        let password = format!("St-pass-{}A!", &Uuid::new_v4().simple().to_string()[..8]);
        let (status, body) = api("POST", "/v1/invites", Some(json!({ "email": email, "grants": [], "delivery": "link" }))).await;
        assert_eq!(status, 201, "invite: {body}");
        let id = body["member"]["id"].as_str().expect("the member id").to_string();
        let link = body["invite_url"].as_str().expect("an invite link").to_string();
        let param = |name: &str| {
            link.split(['?', '&'])
                .find_map(|kv| kv.strip_prefix(&format!("{name}=")))
                .unwrap_or_else(|| panic!("{name} in {link}"))
                .to_string()
        };
        // Accept as Studio does: the session cookie it sets buys the access token.
        let client = reqwest::Client::new();
        let as_studio = |path: &str| {
            client
                .post(format!("{}/auth/v1/{path}", gateway()))
                .header("origin", &studio)
                .header("x-chelabase-studio", "1")
                .timeout(Duration::from_secs(60))
        };
        let res = as_studio("accept-invite")
            .json(&json!({ "t": param("t"), "user": param("user"), "code": param("code"), "password": password }))
            .send()
            .await
            .expect("accept-invite answers");
        let cookies: Vec<String> = res
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .map(|v| v.split(';').next().unwrap_or_default().to_string())
            .collect();
        let status = res.status().as_u16();
        let body: Value = res.json().await.unwrap_or(Value::Null);
        assert_eq!((status, &body["next"]), (200, &json!("studio")), "accept-invite: {body}");
        let res = as_studio("token").header("cookie", cookies.join("; ")).send().await.expect("token answers");
        let status = res.status().as_u16();
        let body: Value = res.json().await.unwrap_or(Value::Null);
        assert_eq!(status, 200, "token: {body}");
        let token = body["access_token"].as_str().expect("an access token").to_string();
        let (status, body) = api("PUT", &format!("/v1/projects/{}/grants/{id}", fx.project), Some(grant)).await;
        assert_eq!(status, 200, "grant: {body}");
        Member { id, token }
    }

    /// Removes the member (the project's deletion takes its grant anyway).
    async fn remove(self) {
        let (status, body) = api("DELETE", &format!("/v1/members/{}", self.id), None).await;
        assert!(status < 300, "remove the member: {status} {body}");
    }
}

impl Fx {
    /// A console storage path on main acting as `authenticated` with the claims `{"sub": user}`.
    fn console(&self, tail: &str, user: Uuid) -> String {
        let claims = json!({ "sub": user }).to_string();
        let sep = if tail.contains('?') { '&' } else { '?' };
        format!(
            "/v1/databases/{}/branches/{}/storage{tail}{sep}role=authenticated&claims={}",
            self.id,
            self.main,
            claims.bytes().map(|b| if b.is_ascii_alphanumeric() || b == b'-' { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>()
        )
    }
}

/// AC 3, 7 and the console URL: acting as `authenticated` with a user's claims, the console
/// uploads and lists only what row level security shows that user; another user's object is
/// missing. An 8 MiB resumable part goes through. A console download URL lives at most an
/// hour and serves while the member keeps access; without a role, nothing runs.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_console_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        objects_fixture(fx, &[("SELECT", "'owner = chelabase.user_id()'", "NULL"), ("INSERT", "NULL", "'owner = chelabase.user_id()'")]).await;
        // A Developer whose grant names `authenticated` (R18).
        let member = Member::granted(fx, json!({ "platform_role": "developer", "pg_roles": ["authenticated"] })).await;
        let token = member.token.clone();
        let (u1, u2) = (Uuid::new_v4(), Uuid::new_v4());
        for (user, name) in [(u1, "u1.txt"), (u2, "u2.txt")] {
            let (status, _, b) = object("POST", &fx.console(&format!("/object/files/{name}"), user), &token, Some(name.as_bytes().to_vec().into()), &[("content-type", "text/plain")]).await;
            assert_eq!(status, 200, "upload {name}: {}", json_of(&b));
        }
        assert_eq!(fx.value("SELECT owner FROM storage.objects WHERE name = 'u1.txt'").await, u1.to_string());
        // AC 3: each user lists only their own.
        for (user, mine) in [(u1, "u1.txt"), (u2, "u2.txt")] {
            let (status, _, b) = object("POST", &fx.console("/object/list/files", user), &token, Some(json!({ "prefix": "" }).to_string().into()), &[JSON]).await;
            assert_eq!(status, 200, "{}", json_of(&b));
            let names: Vec<String> = json_of(&b)["items"].as_array().unwrap().iter().map(|o| o["name"].as_str().unwrap().to_string()).collect();
            assert_eq!(names, vec![mine.to_string()], "{user}");
        }
        let (status, _, got) = object("GET", &fx.console("/object/files/u1.txt", u1), &token, None, &[]).await;
        assert_eq!((status, &got[..]), (200, &b"u1.txt"[..]));
        assert_eq!(object("GET", &fx.console("/object/files/u2.txt", u1), &token, None, &[]).await.0, 404);
        // No role is owner mode: an Admin's only, so this Developer is refused.
        let bare = format!("/v1/databases/{}/branches/{}/storage/bucket", fx.id, fx.main);
        assert_eq!(object("GET", &bare, &token, None, &[]).await.0, 403, "owner mode is an Admin's");
        // A console URL: at most an hour; it serves without a credential.
        let sign = fx.console("/object/sign/files/u1.txt", u1);
        let (status, _, b) = object("POST", &sign, &token, Some(json!({ "expires_in": 3601 }).to_string().into()), &[JSON]).await;
        assert_eq!(status, 400, "{}", json_of(&b));
        let (status, _, b) = object("POST", &sign, &token, Some(json!({ "expires_in": 60 }).to_string().into()), &[JSON]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        let url = json_of(&b)["signed_url"].as_str().unwrap().to_string();
        let res = reqwest::Client::new().get(format!("{}{url}", gateway())).send().await.unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(&res.bytes().await.unwrap()[..], b"u1.txt");
        // AC 7: an 8 MiB part through the console, then completed.
        let (status, _, b) = object("POST", &fx.console("/upload/files/big.bin", u1), &token, None, &[("content-type", "application/octet-stream")]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        let id = json_of(&b)["id"].as_str().unwrap().to_string();
        let part: Vec<u8> = (0..8 * MIB).map(|i| (i % 251) as u8).collect();
        let (status, _, b) = object("PUT", &fx.console(&format!("/upload/session/{id}/1"), u1), &token, Some(part.clone().into()), &[]).await;
        assert_eq!((status, json_of(&b)["size"].clone()), (200, json!(8 * MIB)), "{}", json_of(&b));
        let (status, _, b) = object("POST", &fx.console(&format!("/upload/session/{id}/complete"), u1), &token, None, &[]).await;
        assert_eq!((status, json_of(&b)["size"].clone()), (200, json!(8 * MIB)), "{}", json_of(&b));
        let (status, _, got) = object("GET", &fx.console("/object/files/big.bin", u1), &token, None, &[]).await;
        assert_eq!(status, 200);
        assert!(got[..] == part[..], "the completed object differs");
        member.remove().await;
    }))
    .await;
}

/// The console's resolve (#100 follow-up, for #106): acting as `authenticated` with a user's
/// claims, a ref that user's policies show resolves to a signed URL that serves, and another
/// user's is null; the console caps the URLs at an hour. Owner mode resolves both (by name and
/// by id), and its URLs serve.
///
/// ```text
/// just workbench test -p worker --test storage_schema_live -- --run-ignored only storage_console_resolve_live
/// ```
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_console_resolve_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        objects_fixture(fx, &[("SELECT", "'owner = chelabase.user_id()'", "NULL"), ("INSERT", "NULL", "'owner = chelabase.user_id()'")]).await;
        let member = Member::granted(fx, json!({ "platform_role": "developer", "pg_roles": ["authenticated"] })).await;
        let token = member.token.clone();
        let (u1, u2) = (Uuid::new_v4(), Uuid::new_v4());
        for (user, name) in [(u1, "mine.txt"), (u2, "theirs.txt")] {
            let (status, _, b) = object("POST", &fx.console(&format!("/object/files/{name}"), user), &token, Some(name.as_bytes().to_vec().into()), &[("content-type", "text/plain")]).await;
            assert_eq!(status, 200, "upload {name}: {}", json_of(&b));
        }
        let theirs_id = fx.value("SELECT id FROM storage.objects WHERE name = 'theirs.txt'").await;
        let refs = json!([{ "bucket": "files", "name": "mine.txt" }, { "bucket": "files", "name": "theirs.txt" }]);
        let resolve = fx.console("/object/resolve", u1);
        let (status, _, b) = object("POST", &resolve, &token, Some(json!({ "refs": refs, "expires_in": 3601 }).to_string().into()), &[JSON]).await;
        assert_eq!((status, json_of(&b)["error"]["code"].clone()), (400, json!("invalid_request")), "{}", json_of(&b));
        let (status, _, b) = object("POST", &resolve, &token, Some(json!({ "refs": refs, "expires_in": 60 }).to_string().into()), &[JSON]).await;
        let body = json_of(&b);
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["urls"][1], Value::Null, "another user's object: {body}");
        let url = body["urls"][0].as_str().unwrap_or_else(|| panic!("a signed URL: {body}")).to_string();
        let (status, _, got) = keyless("GET", &url, None).await;
        assert_eq!((status, &got[..]), (200, &b"mine.txt"[..]));
        // A plain Viewer resolves (existence and visibility), but its URL needs Developer.
        let viewer = Member::granted(fx, json!({ "platform_role": "viewer", "pg_roles": ["authenticated"] })).await;
        let (status, _, b) = object("POST", &resolve, &viewer.token, Some(json!({ "refs": refs, "expires_in": 60 }).to_string().into()), &[JSON]).await;
        let viewer_body = json_of(&b);
        viewer.remove().await;
        assert_eq!(status, 200, "viewer resolve: {viewer_body}");
        assert_eq!(viewer_body["urls"][1], Value::Null, "another user's object: {viewer_body}");
        let viewer_url = viewer_body["urls"][0].as_str().unwrap_or_else(|| panic!("a signed URL: {viewer_body}"));
        let (status, _, got) = keyless("GET", viewer_url, None).await;
        assert_eq!((status, json_of(&got)["error"]["code"].clone()), (403, json!("token_not_valid")), "a Viewer's resolved URL opened");
        // Removing the member ends their URL within the NATS window.
        member.remove().await;
        let removed = Instant::now();
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let (status, _, got) = keyless("GET", &url, None).await;
        assert_eq!((status, json_of(&got)["error"]["code"].clone()), (403, json!("token_not_valid")), "still served {:?} after the member was removed", removed.elapsed());
        // Owner mode (an Admin, no role): past every policy, by name and by id.
        let owner_refs = json!([{ "bucket": "files", "name": "mine.txt" }, { "id": theirs_id }]);
        let (status, _, b) = object("POST", &fx.owner_console("/object/resolve"), &dev_token().await, Some(json!({ "refs": owner_refs, "expires_in": 60 }).to_string().into()), &[JSON]).await;
        let body = json_of(&b);
        assert_eq!(status, 200, "{body}");
        for (i, want) in [(0, &b"mine.txt"[..]), (1, &b"theirs.txt"[..])] {
            let url = body["urls"][i].as_str().unwrap_or_else(|| panic!("slot {i}: {body}"));
            let (status, _, got) = keyless("GET", url, None).await;
            assert_eq!((status, &got[..]), (200, want), "slot {i}");
        }
    }))
    .await;
}

/// NATS revocation (m6's AC, moved to m9): a revoked key's signed URL fails within a second,
/// though its liveness was cached a moment before (the cache lifetime is 5 s): the
/// orchestrator's event reaches storage.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_revocation_nats_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'")]).await;
        let (status, body) = api("POST", &fx.keys_path(), Some(json!({ "name": "st-revoke", "role": "authenticated" }))).await;
        assert_eq!(status, 201, "{body}");
        let (key, id) = (body["key"].as_str().unwrap().to_string(), body["id"].as_str().unwrap().to_string());
        let (status, _, b) = object("POST", "/storage/v1/object/files/r.txt", &key, Some(b"r".to_vec().into()), &[]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        let (status, _, b) = object("POST", "/storage/v1/object/sign/files/r.txt", &key, Some(json!({ "expires_in": 600 }).to_string().into()), &[JSON]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        let url = format!("{}{}", gateway(), json_of(&b)["signed_url"].as_str().unwrap());
        let get = || async { reqwest::Client::new().get(&url).send().await.unwrap().status().as_u16() };
        assert_eq!(get().await, 200, "the URL serves (its key's liveness is now cached)");
        let (status, body) = api("DELETE", &format!("{}/{id}", fx.keys_path()), None).await;
        assert_eq!(status, 204, "{body}");
        let revoked = Instant::now();
        tokio::time::sleep(Duration::from_millis(1000)).await;
        assert_eq!(get().await, 403, "still served {:?} after the revoke", revoked.elapsed());
    }))
    .await;
}

impl Fx {
    /// A console storage path on main in owner mode (no role).
    fn owner_console(&self, tail: &str) -> String {
        format!("/v1/databases/{}/branches/{}/storage{tail}", self.id, self.main)
    }
}

/// #100 (the resolve route's audit, B1): a console URL lives on the level of the role it was
/// minted as, not the member's best level. A Developer signs as `authenticated` and the URL
/// opens. Then `authenticated` drops to Viewer (the own grant names only `anon`, a team grant
/// names `authenticated` at Viewer) while the member stays a Developer: console `sign` as it is
/// 403, and both the URL signed before and one resolved now are refused when opened.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_console_role_level_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        objects_fixture(fx, &[("SELECT", "'true'", "NULL")]).await;
        let (status, _, b) = object("POST", &fx.owner_console("/object/files/a.txt"), &dev_token().await, Some(b"hello".to_vec().into()), &[("content-type", "text/plain")]).await;
        assert_eq!(status, 200, "owner upload: {}", json_of(&b));
        let u = Uuid::new_v4();
        let refs = json!({ "refs": [{ "bucket": "files", "name": "a.txt" }], "expires_in": 60 });
        let sign = json!({ "expires_in": 60 }).to_string();
        let mix = Member::granted(fx, json!({ "platform_role": "developer", "pg_roles": ["authenticated"] })).await;
        let (status, _, b) = object("POST", &fx.console("/object/sign/files/a.txt", u), &mix.token, Some(sign.clone().into()), &[JSON]).await;
        let body = json_of(&b);
        assert_eq!(status, 200, "sign at Developer: {body}");
        let signed = body["signed_url"].as_str().unwrap_or_else(|| panic!("a signed URL: {body}")).to_string();
        assert_eq!(keyless("GET", &signed, None).await.0, 200, "the URL signed at Developer opens");
        // `authenticated` lowered to Viewer; the member stays a Developer (on `anon`).
        let (status, b2) = api("PUT", &format!("/v1/projects/{}/grants/{}", fx.project, mix.id), Some(json!({ "platform_role": "developer", "pg_roles": ["anon"] }))).await;
        assert!(status < 300, "own grant: {status} {b2}");
        let (status, team) = api("POST", "/v1/teams", Some(json!({ "name": format!("st-level-{}", Uuid::new_v4().simple()) }))).await;
        assert!(status < 300, "team: {status} {team}");
        let team_id = team["id"].as_str().or(team["team"]["id"].as_str()).expect("the team id").to_string();
        let (status, b2) = api("PUT", &format!("/v1/teams/{team_id}/grants/{}", fx.project), Some(json!({ "platform_role": "viewer", "pg_roles": ["authenticated"] }))).await;
        assert!(status < 300, "team grant: {status} {b2}");
        let (status, b2) = api("PUT", &format!("/v1/teams/{team_id}/members/{}", mix.id), None).await;
        assert!(status < 300, "team member: {status} {b2}");
        let (sign_status, _, sb) = object("POST", &fx.console("/object/sign/files/a.txt", u), &mix.token, Some(sign.into()), &[JSON]).await;
        let (resolve_status, _, rb) = object("POST", &fx.console("/object/resolve", u), &mix.token, Some(refs.to_string().into()), &[JSON]).await;
        let resolved = json_of(&rb);
        // Within the liveness cache's lifetime (5 s) even if no event drops it.
        let lowered = Instant::now();
        let open = |url: Option<String>| async move {
            match url {
                Some(url) => { let (status, _, got) = keyless("GET", &url, None).await; (status, json_of(&got)["error"]["code"].clone()) }
                None => (0, Value::Null),
            }
        };
        let resolved_url = resolved["urls"][0].as_str().map(str::to_owned);
        let (mut before, mut now) = (open(Some(signed.clone())).await, open(resolved_url.clone()).await);
        while (before.0 == 200 || now.0 == 200) && lowered.elapsed() < Duration::from_secs(6) {
            tokio::time::sleep(Duration::from_millis(250)).await;
            (before, now) = (open(Some(signed.clone())).await, open(resolved_url.clone()).await);
        }
        let _ = api("DELETE", &format!("/v1/teams/{team_id}"), None).await;
        mix.remove().await;
        assert_eq!(sign_status, 403, "console sign as a Viewer-level role: {}", json_of(&sb));
        assert_eq!(resolve_status, 200, "resolve at Viewer: {resolved}");
        assert!(resolved_url.is_some(), "resolve names the object: {resolved}");
        let refused = (403, json!("token_not_valid"));
        assert_eq!(now, refused, "a URL resolved as a role held at Viewer opened ({:?} after the change): {resolved}", lowered.elapsed());
        assert_eq!(before, refused, "a URL signed before its role dropped to Viewer still opens ({:?} after the change)", lowered.elapsed());
    }))
    .await;
}

/// Owner mode (m9, the human's option 3): with no role, an Admin's console runs as storage's
/// own login. It lists, reads and writes objects no policy allows (there are none here), and
/// what it writes has no owner. A write-once object stays immutable even so.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_console_owner_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        // Buckets `files` and `other`, a bucket SELECT policy and no object policy at all.
        objects_fixture(fx, &[]).await;
        let token = dev_token().await;
        let text = ("content-type", "text/plain");
        let (status, _, b) = object("POST", &fx.owner_console("/object/files/o.txt"), &token, Some(b"owned".to_vec().into()), &[text]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        assert_eq!(fx.value("SELECT coalesce(owner::text, 'none') FROM storage.objects WHERE name = 'o.txt'").await, "none");
        // A user acting as `authenticated` sees nothing (no policy); the owner sees it.
        let member = Member::granted(fx, json!({ "platform_role": "developer", "pg_roles": ["authenticated"] })).await;
        let (status, _, b) = object("POST", &fx.console("/object/list/files", Uuid::new_v4()), &member.token, Some(json!({ "prefix": "" }).to_string().into()), &[JSON]).await;
        assert_eq!((status, json_of(&b)["items"].clone()), (200, json!([])));
        member.remove().await;
        let (status, _, b) = object("POST", &fx.owner_console("/object/list/files"), &token, Some(json!({ "prefix": "" }).to_string().into()), &[JSON]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        assert_eq!(json_of(&b)["items"][0]["name"], "o.txt");
        let (status, _, got) = object("GET", &fx.owner_console("/object/files/o.txt"), &token, None, &[]).await;
        assert_eq!((status, &got[..]), (200, &b"owned"[..]));
        // Copy, move, sign, delete, and a bucket of its own.
        let (status, _, b) = object("POST", &fx.owner_console("/object/copy"), &token, Some(json!({ "bucket": "files", "from": "o.txt", "to": "c.txt" }).to_string().into()), &[JSON]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        let (status, _, b) = object("POST", &fx.owner_console("/object/move"), &token, Some(json!({ "bucket": "files", "from": "c.txt", "to": "c.txt", "to_bucket": "other" }).to_string().into()), &[JSON]).await;
        assert_eq!(status, 204, "{}", json_of(&b));
        let (status, _, b) = object("POST", &fx.owner_console("/object/sign/files/o.txt"), &token, Some(json!({ "expires_in": 60 }).to_string().into()), &[JSON]).await;
        assert_eq!(status, 200, "{}", json_of(&b));
        let url = format!("{}{}", gateway(), json_of(&b)["signed_url"].as_str().unwrap());
        assert_eq!(reqwest::Client::new().get(&url).send().await.unwrap().status(), 200);
        assert_eq!(object("DELETE", &fx.owner_console("/object/other/c.txt"), &token, None, &[]).await.0, 204);
        let (status, _, b) = object("POST", &fx.owner_console("/bucket"), &token, Some(json!({ "name": "frozen", "write_once": true }).to_string().into()), &[JSON]).await;
        assert_eq!(status, 201, "{}", json_of(&b));
        // Write-once holds for the owner too.
        let up = fx.owner_console("/object/frozen/a.txt");
        assert_eq!(object("POST", &up, &token, Some(b"one".to_vec().into()), &[text]).await.0, 200);
        let (status, _, b) = object("POST", &format!("{up}?upsert=true"), &token, Some(b"two".to_vec().into()), &[text]).await;
        assert_eq!((status, json_of(&b)["error"]["code"].clone()), (409, json!("write_once")), "{}", json_of(&b));
        let (status, _, b) = object("DELETE", &up, &token, None, &[]).await;
        assert_eq!((status, json_of(&b)["error"]["code"].clone()), (409, json!("write_once")), "{}", json_of(&b));
        let (status, _, got) = object("GET", &up, &token, None, &[]).await;
        assert_eq!((status, &got[..]), (200, &b"one"[..]));
    }))
    .await;
}

// --- #102: storage_ref columns and the batched resolve ---------------------------------------

/// `POST /storage/v1/object/resolve` with `refs` (and `expires_in` 60): `(status, body)`.
async fn resolve_refs(key: &str, refs: Value) -> (u16, Value) {
    storage("POST", "/storage/v1/object/resolve", key, Some(json!({ "refs": refs, "expires_in": 60 }))).await
}

/// A `storage.object_ref` column round-trips through the query API as `{bucket_id, name}`
/// (AC 5) and generates as `StorageRef` (AC 1); one resolve answers every ref in order, a
/// signed URL for what the caller can read, the public URL in a public bucket, and `null` for
/// a missing object, an object or bucket row level security hides, or an invalid name, all
/// alike (AC 2, AC 3); over 1000 refs is 400.
///
/// ```text
/// just workbench test -p worker --test storage_schema_live -- --run-ignored only storage_refs_live
/// ```
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_refs_live() {
    with_fx(|fx| Box::pin(async move {
        // The table first: the data API reads the schema on its first request here.
        fx.owner(
            "CREATE TABLE profiles (id int PRIMARY KEY, avatar storage.object_ref, banner storage.object_ref NOT NULL);
             GRANT SELECT, INSERT ON profiles TO authenticated;",
        )
        .await
        .expect("the profiles table");
        let secret = fx.secret_key("st-refs", None).await;
        let public = fx.public_key().await;
        for b in ["files", "pub", "closed"] {
            fx.bucket(b).await;
        }
        fx.admin("BEGIN; SET LOCAL ROLE chelabase_storage; UPDATE storage.buckets SET public = true WHERE name = 'pub'; COMMIT;").await;
        for (name, table, command, using, check) in [
            ("b_select", "buckets", "SELECT", "'name <> ''closed'''", "NULL"),
            ("o_select", "objects", "SELECT", "'name NOT LIKE ''hidden/%'''", "NULL"),
            ("o_insert", "objects", "INSERT", "NULL", "'true'"),
        ] {
            fx.owner(&format!(
                "SELECT storage.create_policy('{name}', '{table}', '{command}', ARRAY['authenticated']::name[], {using}, {check})"
            ))
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        for (path, body) in [("files/a.txt", "v1"), ("files/hidden/x.txt", "h"), ("pub/logo.png", "png")] {
            let (status, _, b) = object("POST", &format!("/storage/v1/object/{path}"), &secret, Some(body.as_bytes().to_vec().into()), &[("content-type", "text/plain")]).await;
            assert_eq!(status, 200, "{path}: {}", json_of(&b));
        }
        let id = "SELECT id FROM storage.buckets WHERE name =";
        let files = fx.value(&format!("{id} 'files'")).await;
        let pub_ = fx.value(&format!("{id} 'pub'")).await;
        let closed = fx.value(&format!("{id} 'closed'")).await;
        // An object in the bucket the caller can't see (storage's own write, with its ticket).
        fx.admin(&format!(
            "BEGIN; SET LOCAL ROLE chelabase_storage;
INSERT INTO storage.upload_tickets (version_id, bucket_id, name, size, etag, expires_at, caller_role)
  VALUES ('00000000-0000-0000-0000-0000000000f1', '{closed}', 'c.txt', 1, 'e', now() + interval '5 minutes', 'chelabase_storage');
INSERT INTO storage.objects (bucket_id, name, size, etag, version_id) VALUES ('{closed}', 'c.txt', 1, 'e', '00000000-0000-0000-0000-0000000000f1');
COMMIT;"
        ))
        .await;

        // AC 5: inserted as {bucket_id, name}, read back the same.
        let a = json!({ "bucket_id": files, "name": "a.txt" });
        let logo = json!({ "bucket_id": pub_, "name": "logo.png" });
        let (status, body) = storage("POST", "/data/v1/query", &secret, Some(json!({
            "v": 1, "op": "insert", "into": "profiles",
            "values": [{ "id": 1, "avatar": a, "banner": logo }, { "id": 2, "avatar": null, "banner": a }],
        })))
        .await;
        assert!((200..300).contains(&status), "insert: {status} {body}");
        let (status, rows) = storage("POST", "/data/v1/query", &secret, Some(json!({
            "v": 1, "op": "select", "from": "profiles", "select": ["id", "avatar", "banner"], "orderBy": [{ "col": "id" }],
        })))
        .await;
        assert_eq!(status, 200, "{rows}");
        assert_eq!(rows, json!([{ "id": 1, "avatar": a, "banner": logo }, { "id": 2, "avatar": null, "banner": a }]));

        // AC 1: the key's generated types type the columns as StorageRef.
        let (status, _, ts) = object("GET", "/data/v1/types", &secret, None, &[]).await;
        let ts = String::from_utf8_lossy(&ts);
        assert_eq!(status, 200, "{ts}");
        assert!(ts.contains("import type { StorageRef, TypeMap } from \"@chelabase/client\";"), "{ts}");
        assert!(ts.contains("avatar: StorageRef | null;") && ts.contains("banner: StorageRef;"), "{ts}");

        // AC 2, AC 3: one request, input order; unreadable refs are null, whatever the reason.
        let refs = json!([
            a,
            { "bucket_id": files, "name": "hidden/x.txt" },
            logo,
            { "bucket_id": closed, "name": "c.txt" },
            { "bucket_id": files, "name": "ghost.txt" },
            { "bucket_id": "00000000-0000-0000-0000-000000000000", "name": "a.txt" },
            { "bucket_id": files, "name": "../a.txt" },
            a,
        ]);
        let (status, body) = resolve_refs(&secret, refs.clone()).await;
        assert_eq!(status, 200, "{body}");
        let urls = body["urls"].as_array().expect("urls").clone();
        assert_eq!(urls.len(), 8, "{body}");
        for i in [1, 3, 4, 5, 6] {
            assert_eq!(urls[i], Value::Null, "slot {i}: {body}");
        }
        let signed = urls[0].as_str().expect("a signed URL");
        assert!(signed.starts_with("/storage/v1/object/sign/files/a.txt?token="), "{signed}");
        assert!(urls[7].as_str().unwrap().starts_with("/storage/v1/object/sign/files/a.txt?token="), "{body}");
        let (status, _, got) = keyless("GET", signed, None).await;
        assert_eq!((status, &got[..]), (200, &b"v1"[..]));
        let public_url = urls[2].as_str().expect("a public URL");
        assert_eq!(public_url, format!("/storage/v1/object/public/{}/pub/logo.png?database={}", fx.main, fx.database));
        let (status, _, got) = keyless("GET", public_url, None).await;
        assert_eq!((status, &got[..]), (200, &b"png"[..]));
        // Its token is for that object only: another name with it is refused.
        let token = signed.split_once("?token=").unwrap().1;
        assert_eq!(keyless("GET", &format!("/storage/v1/object/sign/files/hidden/x.txt?token={token}"), None).await.0, 403);
        assert!(body["expires_at"].as_u64().is_some(), "{body}");

        // The public key's role (anon) has no policy: every slot is null, not an error.
        let (status, body) = resolve_refs(&public, refs).await;
        assert_eq!((status, body["urls"].as_array().map(Vec::len)), (200, Some(8)), "{body}");
        assert!(body["urls"].as_array().unwrap().iter().all(Value::is_null), "{body}");

        // An empty batch answers empty; over 1000 refs is 400; a malformed ref is 400.
        assert_eq!(resolve_refs(&secret, json!([])).await.1["urls"], json!([]));
        let many: Vec<Value> = (0..1001).map(|_| a.clone()).collect();
        let (status, body) = resolve_refs(&secret, json!(many)).await;
        assert_eq!((status, &body["error"]["code"]), (400, &json!("invalid_request")), "{body}");
        assert_eq!(resolve_refs(&secret, json!([{ "bucket_id": "nope", "name": "a" }])).await.0, 400);

        // An empty or all-invalid batch still asks about the database first (m11 audit N1).
        let listed = fx.secret_key("st-resolve-listed", Some(vec![fx.database.clone()])).await;
        for refs in [json!([]), json!([{ "bucket": "files", "name": "/bad" }])] {
            let path = "/storage/v1/object/resolve?database=postgres";
            let (status, body) = storage("POST", path, &listed, Some(json!({ "refs": refs }))).await;
            assert_eq!((status, &body["error"]["code"]), (403, &json!("database_not_allowed")), "{refs}: {body}");
        }
    }))
    .await;
}

/// #102, the other two forms: a text column whose comment says `@storage_ref bucket=` and a
/// column with a foreign key to `storage.objects(id)` are described in the schema (only that
/// hint, never the comment's text) and typed `StoragePath<…>` and `StorageObjectId`; a type
/// merely named `object_ref` isn't a ref. `{bucket, name}` and `{id}` refs resolve in one
/// batch under the same rule: an object the caller can't read (or a bucket it can't see) is
/// `null`.
///
/// ```text
/// just workbench test -p worker --test storage_schema_live -- --run-ignored only storage_ref_forms_live
/// ```
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_ref_forms_live() {
    with_fx(|fx| Box::pin(async move {
        // The table first: the data API reads the schema on its first request here.
        fx.owner(
            "CREATE TYPE object_ref AS (x int);
             CREATE TABLE docs (id int PRIMARY KEY, path text, photo uuid REFERENCES storage.objects(id), look object_ref);
             COMMENT ON COLUMN docs.path IS 'The file, internal. @storage_ref bucket=files';
             COMMENT ON COLUMN docs.photo IS 'internal photo note';
             GRANT SELECT, INSERT ON docs TO authenticated;",
        )
        .await
        .expect("the docs table");
        let secret = fx.secret_key("st-ref-forms", None).await;
        let public = fx.public_key().await;
        for b in ["files", "closed"] {
            fx.bucket(b).await;
        }
        for (name, table, command, using, check) in [
            ("b_select", "buckets", "SELECT", "'name <> ''closed'''", "NULL"),
            ("o_select", "objects", "SELECT", "'name NOT LIKE ''hidden/%'''", "NULL"),
            ("o_insert", "objects", "INSERT", "NULL", "'true'"),
        ] {
            fx.owner(&format!(
                "SELECT storage.create_policy('{name}', '{table}', '{command}', ARRAY['authenticated']::name[], {using}, {check})"
            ))
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        }
        for (path, body) in [("files/a.txt", "v1"), ("files/hidden/x.txt", "h")] {
            let (status, _, b) = object("POST", &format!("/storage/v1/object/{path}"), &secret, Some(body.as_bytes().to_vec().into()), &[("content-type", "text/plain")]).await;
            assert_eq!(status, 200, "{path}: {}", json_of(&b));
        }
        let closed = fx.value("SELECT id FROM storage.buckets WHERE name = 'closed'").await;
        fx.admin(&format!(
            "BEGIN; SET LOCAL ROLE chelabase_storage;
INSERT INTO storage.upload_tickets (version_id, bucket_id, name, size, etag, expires_at, caller_role)
  VALUES ('00000000-0000-0000-0000-0000000000f2', '{closed}', 'c.txt', 1, 'e', now() + interval '5 minutes', 'chelabase_storage');
INSERT INTO storage.objects (bucket_id, name, size, etag, version_id) VALUES ('{closed}', 'c.txt', 1, 'e', '00000000-0000-0000-0000-0000000000f2');
COMMIT;"
        ))
        .await;
        let by_name = "SELECT id FROM storage.objects WHERE name =";
        let a = fx.value(&format!("{by_name} 'a.txt'")).await;
        let hidden = fx.value(&format!("{by_name} 'hidden/x.txt'")).await;
        let in_closed = fx.value(&format!("{by_name} 'c.txt'")).await;

        // The schema names each form, and only the hint: no comment text.
        let (status, _, schema) = object("GET", "/data/v1/schema", &secret, None, &[]).await;
        let schema_text = String::from_utf8_lossy(&schema).to_string();
        assert_eq!(status, 200, "{schema_text}");
        let schema: Value = serde_json::from_str(&schema_text).unwrap();
        let cols = &schema["schemas"]["public"]["tables"]["docs"]["columns"];
        assert_eq!(cols["path"]["storage_ref"], json!({ "kind": "path", "bucket": "files" }), "{cols}");
        assert_eq!(cols["photo"]["storage_ref"], json!({ "kind": "id" }), "{cols}");
        assert!(cols["look"].get("storage_ref").is_none(), "{cols}");
        assert!(cols["id"].get("storage_ref").is_none(), "{cols}");
        assert!(!schema_text.contains("internal"), "{schema_text}");
        // The schema (hints included) is for secret keys only: a publishable key gets none of it.
        let (status, _, anon) = object("GET", "/data/v1/schema", &public, None, &[]).await;
        assert_eq!(status, 401, "{}", String::from_utf8_lossy(&anon));

        // The generated types.
        let (status, _, ts) = object("GET", "/data/v1/types", &secret, None, &[]).await;
        let ts = String::from_utf8_lossy(&ts);
        assert_eq!(status, 200, "{ts}");
        for line in ["path: StoragePath<\"files\"> | null;", "photo: StorageObjectId | null;", "look: string | null;"] {
            assert!(ts.contains(line), "{line}\n{ts}");
        }
        assert!(ts.contains("import type { StorageObjectId, StoragePath, TypeMap } from \"@chelabase/client\";"), "{ts}");

        // Both forms round-trip through the query API as plain strings.
        let (status, body) = storage("POST", "/data/v1/query", &secret, Some(json!({
            "v": 1, "op": "insert", "into": "docs", "values": [{ "id": 1, "path": "a.txt", "photo": a }],
        })))
        .await;
        assert!((200..300).contains(&status), "insert: {status} {body}");
        let (status, rows) = storage("POST", "/data/v1/query", &secret, Some(json!({
            "v": 1, "op": "select", "from": "docs", "select": ["path", "photo"],
        })))
        .await;
        assert_eq!((status, rows), (200, json!([{ "path": "a.txt", "photo": a }])));

        // One batch of id and bucket-name refs, in order; unreadable ones are null.
        let refs = json!([
            { "id": a },
            { "id": hidden },
            { "id": in_closed },
            { "id": "00000000-0000-0000-0000-000000000000" },
            { "bucket": "files", "name": "a.txt" },
            { "bucket": "files", "name": "hidden/x.txt" },
            { "bucket": "closed", "name": "c.txt" },
            { "bucket": "nope", "name": "a.txt" },
            { "bucket": "files", "name": "../a.txt" },
        ]);
        let (status, body) = resolve_refs(&secret, refs.clone()).await;
        assert_eq!(status, 200, "{body}");
        let urls = body["urls"].as_array().expect("urls").clone();
        assert_eq!(urls.len(), 9, "{body}");
        for i in [1, 2, 3, 5, 6, 7, 8] {
            assert_eq!(urls[i], Value::Null, "slot {i}: {body}");
        }
        for i in [0, 4] {
            let url = urls[i].as_str().unwrap_or_else(|| panic!("slot {i}: {body}"));
            assert!(url.starts_with("/storage/v1/object/sign/files/a.txt?token="), "{url}");
            let (status, _, got) = keyless("GET", url, None).await;
            assert_eq!((status, &got[..]), (200, &b"v1"[..]), "slot {i}");
        }
        // The public key's role reads nothing: every slot null, not an error.
        let (status, body) = resolve_refs(&public, refs).await;
        assert_eq!((status, body["urls"].as_array().map(Vec::len)), (200, Some(9)), "{body}");
        assert!(body["urls"].as_array().unwrap().iter().all(Value::is_null), "{body}");
        // A ref naming an object two ways is 400; so is a 1001st id ref.
        assert_eq!(resolve_refs(&secret, json!([{ "id": a, "bucket": "files", "name": "a.txt" }])).await.0, 400);
        let many: Vec<Value> = (0..1001).map(|_| json!({ "id": a })).collect();
        assert_eq!(resolve_refs(&secret, json!(many)).await.0, 400);
    }))
    .await;
}

/// #102: the database owner (no superuser) adds foreign keys to `storage.objects(id)`, its one
/// privilege there (`REFERENCES (id)`, which the schema check knows and a reinstall keeps).
/// A delete such a key refuses is 409 `referenced`, nothing deleted (the row, and so its blob,
/// stays); `ON DELETE SET NULL` lets it through. A move or an overwrite keeps the object's id,
/// so neither is refused. A version-7 schema (no grant) gets it from the bootstrap, and the
/// foreign keys made before survive the reinstall.
///
/// ```text
/// just workbench test -p worker --test storage_schema_live -- --run-ignored only storage_ref_foreign_key_live
/// ```
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_ref_foreign_key_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        const TEXT: (&str, &str) = ("content-type", "text/plain");
        let stamp = "SELECT obj_description('storage'::regnamespace, 'pg_namespace')";
        let check = || async { last_line(&fx.admin(&schema_check_sql("public", Some(&fx.verifier().await))).await) };
        let privileges = "SELECT has_column_privilege('postgres', 'storage.objects', 'id', 'REFERENCES')::text || ' '
            || has_column_privilege('postgres', 'storage.objects', 'name', 'REFERENCES')::text || ' '
            || has_column_privilege('postgres', 'storage.objects', 'bucket_id', 'REFERENCES')::text || ' '
            || has_table_privilege('postgres', 'storage.buckets', 'REFERENCES')::text";
        assert_eq!(fx.value(privileges).await, "true false false false", "only REFERENCES (id)");

        // The owner's own tables, no superuser involved; the database stays healthy.
        objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'"), ("UPDATE", "'true'", "'true'"), ("DELETE", "'true'", "NULL")]).await;
        fx.owner("SELECT storage.create_policy('b_delete', 'buckets', 'DELETE', ARRAY['authenticated']::name[], 'true', NULL)")
            .await
            .unwrap();
        fx.owner(
            "CREATE TABLE pins (id int PRIMARY KEY, photo uuid REFERENCES storage.objects(id));
             CREATE TABLE loose (id int PRIMARY KEY, photo uuid REFERENCES storage.objects(id) ON DELETE SET NULL);",
        )
        .await
        .expect("the owner's foreign keys to storage.objects(id)");
        assert!(
            fx.owner_err("CREATE TABLE by_name (b uuid, n text, FOREIGN KEY (b, n) REFERENCES storage.objects(bucket_id, name))").await.contains("permission denied"),
            "no other column"
        );
        assert_eq!(check().await, "t", "the grant and the foreign keys are the install's to allow");

        let store = Arc::new(FailingStore::default());
        let (base, token) = in_process_storage(store.clone(), fx.main).await;
        let call = async |method: &str, path: &str, body: Vec<u8>, headers: &[(&str, &str)]| {
            let (status, b) = raw(&base, &token, method, path, body, headers).await;
            (status, json_of(&b))
        };
        let up = |name: &str| format!("/storage/v1/object/files/{name}");
        for (name, body) in [("a.txt", "one"), ("b.txt", "two")] {
            let (status, b) = call("POST", &up(name), body.as_bytes().to_vec(), &[TEXT]).await;
            assert_eq!(status, 200, "{name}: {b}");
        }
        let id_of = |name: &'static str| async move { fx.value(&format!("SELECT id FROM storage.objects WHERE name = '{name}'")).await };
        let (a, b) = (id_of("a.txt").await, id_of("b.txt").await);
        fx.owner(&format!("INSERT INTO pins VALUES (1, '{a}'); INSERT INTO loose VALUES (1, '{b}');")).await.unwrap();

        // A referenced object isn't deleted: 409 `referenced`, no table name; row and blob stay.
        let (status, got) = call("DELETE", &up("a.txt"), vec![], &[]).await;
        assert_eq!((status, got["error"]["code"].clone(), got["error"]["sqlstate"].clone()), (409, json!("referenced"), json!("23503")), "{got}");
        assert!(!got.to_string().contains("pins"), "{got}");
        // Several at once: one referenced refuses them all.
        let (status, got) = call("DELETE", "/storage/v1/object/files", json!({ "names": ["b.txt", "a.txt"] }).to_string().into_bytes(), &[JSON]).await;
        assert_eq!((status, got["error"]["code"].clone()), (409, json!("referenced")), "{got}");
        assert_eq!(fx.value("SELECT string_agg(name, ',' ORDER BY name) FROM storage.objects").await, "a.txt,b.txt");
        let (status, got) = raw(&base, &token, "GET", &up("a.txt"), vec![], &[]).await;
        assert_eq!((status, &got[..]), (200, &b"one"[..]), "the blob stays");

        // ON DELETE SET NULL: deleted, the column nulled.
        assert_eq!(call("DELETE", &up("b.txt"), vec![], &[]).await.0, 204);
        assert_eq!(fx.value("SELECT coalesce(photo::text, 'null') FROM loose").await, "null");

        // A move and an overwrite keep the id: neither is refused, the reference follows.
        let (status, got) = call("POST", "/storage/v1/object/move", json!({ "bucket": "files", "from": "a.txt", "to": "a2.txt" }).to_string().into_bytes(), &[JSON]).await;
        assert!((200..300).contains(&status), "move: {status} {got}");
        let (status, got) = call("POST", &up("a2.txt"), b"one again".to_vec(), &[TEXT, ("x-upsert", "true")]).await;
        assert_eq!(status, 200, "overwrite: {got}");
        assert_eq!(id_of("a2.txt").await, a);
        assert_eq!(fx.value("SELECT o.name FROM pins p JOIN storage.objects o ON o.id = p.photo").await, "a2.txt");
        // A bucket holding it is not empty, as before.
        let (status, got) = call("DELETE", "/storage/v1/bucket/files", vec![], &[]).await;
        assert_eq!((status, got["error"]["code"].clone()), (409, json!("bucket_not_empty")), "{got}");

        // A grant the install doesn't make shows as tampering, and the bootstrap heals it.
        fx.admin("BEGIN; SET LOCAL ROLE chelabase_admin; GRANT REFERENCES (name) ON storage.objects TO postgres; COMMIT;").await;
        assert_eq!(check().await, "f", "REFERENCES (name)");
        bootstrap(fx).await;
        assert_eq!((check().await, fx.value(privileges).await), ("t".to_string(), "true false false false".to_string()));

        // Version 7 (no grant): the owner can't add one; the bootstrap brings the grant, and
        // the foreign keys made before are still enforced after the reinstall.
        fx.admin(
            "BEGIN; SET LOCAL ROLE chelabase_admin; REVOKE REFERENCES (id) ON storage.objects FROM postgres;
             COMMENT ON SCHEMA storage IS 'chelabase storage schema 7'; COMMIT;",
        )
        .await;
        assert_eq!(fx.value(privileges).await, "false false false false");
        assert!(fx.owner_err("CREATE TABLE later (photo uuid REFERENCES storage.objects(id))").await.contains("permission denied"));
        assert_eq!(check().await, "f");
        bootstrap(fx).await;
        assert_eq!(fx.value(stamp).await, storage_schema::STORAGE_SCHEMA_STAMP);
        assert_ne!(storage_schema::STORAGE_SCHEMA_STAMP, "chelabase storage schema 7");
        assert_eq!((check().await, fx.value(privileges).await), ("t".to_string(), "true false false false".to_string()));
        fx.owner("CREATE TABLE later (photo uuid REFERENCES storage.objects(id))").await.expect("after the upgrade");
        let (status, got) = call("DELETE", &up("a2.txt"), vec![], &[]).await;
        assert_eq!((status, got["error"]["code"].clone()), (409, json!("referenced")), "the old foreign key survived: {got}");
        fx.owner("DELETE FROM pins").await.unwrap();
        assert_eq!(call("DELETE", &up("a2.txt"), vec![], &[]).await.0, 204);
    }))
    .await;
}

/// #102 (m11 audit B1): a delete the owner's tables refuse gives a fixed answer, never
/// Postgres's text about them (the invariant in `objects::deleted_failure`). A deferred
/// foreign key fails at the statement, not at COMMIT, and is 409 `referenced`; a referential
/// action that breaks a constraint (`ON DELETE SET NULL` on a `NOT NULL` column), a trigger
/// the cascade fires, and a trigger that defers the keys again are 409 `delete_refused`. Single
/// and batch delete alike; nothing is deleted.
///
/// ```text
/// just workbench test -p worker --test storage_schema_live -- --run-ignored only storage_ref_delete_refusal_live
/// ```
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_ref_delete_refusal_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        const TEXT: (&str, &str) = ("content-type", "text/plain");
        objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'"), ("DELETE", "'true'", "NULL")]).await;
        fx.owner(
            "-- First: its cascade trigger is older, so it fires (and defers the keys) before
             -- secret_pins's check does.
             CREATE TABLE secret_redefer (id int PRIMARY KEY, photo uuid REFERENCES storage.objects(id) ON DELETE CASCADE);
             CREATE FUNCTION secret_redefer() RETURNS trigger LANGUAGE plpgsql AS $$
                 BEGIN SET CONSTRAINTS ALL DEFERRED; RETURN OLD; END $$;
             CREATE TRIGGER secret_redefer BEFORE DELETE ON secret_redefer FOR EACH ROW EXECUTE FUNCTION secret_redefer();
             CREATE TABLE secret_pins (id int PRIMARY KEY, photo uuid
                 CONSTRAINT secret_pins_photo_fkey REFERENCES storage.objects(id) DEFERRABLE INITIALLY DEFERRED);
             CREATE TABLE secret_notnull (id int PRIMARY KEY,
                 photo uuid NOT NULL REFERENCES storage.objects(id) ON DELETE SET NULL);
             CREATE TABLE secret_cascade (id int PRIMARY KEY, photo uuid REFERENCES storage.objects(id) ON DELETE CASCADE);
             CREATE FUNCTION secret_guard() RETURNS trigger LANGUAGE plpgsql AS $$
                 BEGIN RAISE EXCEPTION 'secret_guard refused row %', OLD.id USING DETAIL = 'secret detail', HINT = 'secret hint'; END $$;
             CREATE TRIGGER secret_guard BEFORE DELETE ON secret_cascade FOR EACH ROW EXECUTE FUNCTION secret_guard();",
        )
        .await
        .expect("the owner's tables");

        let store = Arc::new(FailingStore::default());
        let (base, token) = in_process_storage(store.clone(), fx.main).await;
        let call = async |method: &str, path: &str, body: Vec<u8>, headers: &[(&str, &str)]| {
            let (status, b) = raw(&base, &token, method, path, body, headers).await;
            (status, json_of(&b))
        };
        let up = |name: &str| format!("/storage/v1/object/files/{name}");
        for name in ["deferred.txt", "notnull.txt", "trigger.txt", "redefer.txt", "free.txt"] {
            let (status, b) = call("POST", &up(name), b"x".to_vec(), &[TEXT]).await;
            assert_eq!(status, 200, "{name}: {b}");
        }
        let id_of = |name: &'static str| async move { fx.value(&format!("SELECT id FROM storage.objects WHERE name = '{name}'")).await };
        let (deferred, notnull, trigger, redefer) =
            (id_of("deferred.txt").await, id_of("notnull.txt").await, id_of("trigger.txt").await, id_of("redefer.txt").await);
        fx.owner(&format!(
            "INSERT INTO secret_pins VALUES (1, '{deferred}'); INSERT INTO secret_notnull VALUES (1, '{notnull}');
             INSERT INTO secret_cascade VALUES (1, '{trigger}'); INSERT INTO secret_pins VALUES (2, '{redefer}');
             INSERT INTO secret_redefer VALUES (1, '{redefer}');"
        ))
        .await
        .unwrap();

        // Every case runs, and the failures are reported together.
        let mut failures = Vec::new();
        let mut check = |case: String, code: &str, status: u16, got: &Value| {
            let text = got.to_string();
            let leaked = ["secret", "Failing row", "photo", "constraint \"", "relation \""].into_iter().find(|s| text.contains(s));
            let extra = got["error"].get("detail").is_some() || got["error"].get("hint").is_some();
            if leaked.is_some() || extra || (status, &got["error"]["code"]) != (409, &json!(code)) {
                failures.push(format!("{case}: want 409 {code} with no owner text, got {status} {got}"));
            }
        };
        for (name, code) in [
            ("deferred.txt", "referenced"),
            ("notnull.txt", "delete_refused"),
            ("trigger.txt", "delete_refused"),
            ("redefer.txt", "referenced"),
        ] {
            let (status, got) = call("DELETE", &up(name), vec![], &[]).await;
            check(name.to_owned(), code, status, &got);
            let batch = json!({ "names": ["free.txt", name] }).to_string().into_bytes();
            let (status, got) = call("DELETE", "/storage/v1/object/files", batch, &[JSON]).await;
            check(format!("batch {name}"), code, status, &got);
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
        assert_eq!(
            fx.value("SELECT string_agg(name, ',' ORDER BY name) FROM storage.objects").await,
            "deferred.txt,free.txt,notnull.txt,redefer.txt,trigger.txt",
            "nothing deleted"
        );
        assert_eq!(fx.owner("SELECT count(*) FROM secret_notnull").await.unwrap().trim(), "1");
        assert_eq!(call("DELETE", &up("free.txt"), vec![], &[]).await.0, 204, "an unreferenced object still goes");
    }))
    .await;
}

// --- m18: storage webhooks, end to end ------------------------------------------------------

/// One request an endpoint received: its headers (lower-case names) and body.
struct Received {
    headers: std::collections::HashMap<String, String>,
    body: Vec<u8>,
}

/// An endpoint on every interface of the runner, answering 200 to each POST and handing each
/// request to the returned channel.
async fn webhook_endpoint() -> (u16, tokio::sync::mpsc::UnboundedReceiver<Received>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let (head_end, length) = loop {
                    let Ok(n) = sock.read(&mut chunk).await else { return };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..at]).to_ascii_lowercase();
                        let length = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                            .unwrap_or(0);
                        break (at + 4, length);
                    }
                };
                while buf.len() < head_end + length {
                    let Ok(n) = sock.read(&mut chunk).await else { return };
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let headers = head
                    .lines()
                    .skip(1)
                    .filter_map(|l| l.split_once(':'))
                    .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                    .collect();
                let body = buf[head_end..(head_end + length).min(buf.len())].to_vec();
                let _ = sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
                let _ = tx.send(Received { headers, body });
            });
        }
    });
    (port, rx)
}

/// AC 1: an endpoint in the runner's network for `object.created` on bucket `files` gets one
/// signed POST with the event's fields within seconds of an upload there; an upload to another
/// bucket sends nothing. The workbench worker allows exactly the host `runner` (the runner's
/// alias) over http, for this receiver.
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_webhooks_live() {
    let fx = Arc::new(Fx::provision().await);
    let task = fx.clone();
    let outcome = tokio::spawn(async move { storage_webhooks(&task, "runner").await }).await;
    fx.purge_blobs(&object_store().await).await;
    fx.teardown().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic.into_panic());
    }
}

async fn storage_webhooks(fx: &Fx, host: &str) {
    let secret = objects_fixture(fx, &[("INSERT", "NULL", "'true'"), ("SELECT", "'true'", "NULL")]).await;
    let (port, mut received) = webhook_endpoint().await;
    let webhook_id = Uuid::new_v4();
    let signing = format!("whsec_live_{}", Uuid::new_v4().simple());
    let events = vec!["object.created".to_string()];
    let url = format!("http://{host}:{port}/hook");
    fx.metadata
        .create_storage_webhook(&chelabase_metadata::NewStorageWebhook {
            id: webhook_id,
            database_id: fx.id.as_uuid(),
            url: &url,
            events: &events,
            bucket: Some("files"),
            prefix: None,
            secret_enc: &chelabase_secrets::seal_webhook_secret(&fx.worker.key, webhook_id, &signing),
        })
        .await
        .expect("create the endpoint")
        .expect("under the cap");

    let (status, _, b) = object("POST", "/storage/v1/object/other/skip.txt", &secret, Some(b"no".to_vec().into()), &[("content-type", "text/plain")]).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&b));
    let (status, _, b) = object("POST", "/storage/v1/object/files/a.txt", &secret, Some(b"abc".to_vec().into()), &[("content-type", "text/plain")]).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&b));
    let uploaded = json_of(&b);

    // The worker reads the endpoints every 5 s: well within 90 s.
    let got = tokio::time::timeout(Duration::from_secs(90), received.recv())
        .await
        .expect("a delivery within 90 s")
        .expect("the endpoint still listens");
    let signature = got.headers.get("chelabase-signature").expect("a signature").clone();
    let now = chrono::Utc::now().timestamp();
    assert_eq!(worker::storage_webhooks::verify(&signing, &signature, &got.body, now, 300), Ok(()));
    let event: Value = serde_json::from_slice(&got.body).unwrap();
    assert_eq!(event["type"], "object.created");
    assert_eq!(event["bucket"], "files");
    assert_eq!(event["name"], "a.txt");
    assert_eq!(event["database_id"], json!(fx.id.as_uuid()));
    assert_eq!(event["branch_id"], json!(fx.main.as_uuid()));
    assert_eq!(event["pg_database"], json!(fx.database));
    assert_eq!(event["version_id"], uploaded["version_id"]);
    assert_eq!(event["size"], 3);
    assert_eq!(event["content_type"], "text/plain");
    assert_eq!(event["actor"]["role"], "authenticated");
    assert_eq!(got.headers.get("chelabase-event-id"), event["id"].as_str().map(str::to_string).as_ref());
    assert!(!String::from_utf8_lossy(&got.body).contains("abc"), "never the contents");

    // Nothing for the other bucket, nor a second copy (given time for both).
    let extra = tokio::time::timeout(Duration::from_secs(15), received.recv()).await;
    assert!(extra.is_err(), "an unexpected delivery: {:?}", extra.ok().flatten().map(|r| String::from_utf8_lossy(&r.body).to_string()));
    let attempts = fx.metadata.list_storage_webhook_attempts(fx.id.as_uuid(), webhook_id, 10).await.unwrap();
    assert_eq!(attempts.len(), 1, "{attempts:?}");
    assert_eq!((attempts[0].outcome.as_str(), attempts[0].status), ("delivered", Some(200)));
    assert!(fx.metadata.delete_storage_webhook(fx.id.as_uuid(), webhook_id).await.unwrap());
}

/// #102 (m11 re-audit B1′): a deferred trigger can always defer the keys again, so a delete's
/// work can still fail at COMMIT, after every statement passed. That failure gets the same
/// fixed answer as one at the statement (`objects::deleted_failure`, nested): a late insert
/// that breaks a deferred foreign key is 409 `referenced`, a deferred constraint trigger that
/// raises is 409 `delete_refused`, never the owner's table, constraint, key or message. Single
/// and batch delete alike; nothing is deleted.
///
/// ```text
/// just workbench test -p worker --test storage_schema_live -- --run-ignored only storage_ref_commit_time_refusal_live
/// ```
#[tokio::test]
#[ignore = "needs the workbench stack with the storage service"]
async fn storage_ref_commit_time_refusal_live() {
    with_fx(|fx| Box::pin(async move {
        let fx = &*fx;
        const TEXT: (&str, &str) = ("content-type", "text/plain");
        objects_fixture(fx, &[("SELECT", "'true'", "NULL"), ("INSERT", "NULL", "'true'"), ("DELETE", "'true'", "NULL")]).await;
        // Each cascade's BEFORE trigger defers the keys; its deferred constraint trigger, fired
        // by the delete's second SET CONSTRAINTS ALL IMMEDIATE, defers them again and queues
        // work that only COMMIT runs.
        fx.owner(
            "CREATE TABLE secret_late (id int PRIMARY KEY, photo uuid
                 CONSTRAINT secret_late_photo_fkey REFERENCES storage.objects(id) DEFERRABLE INITIALLY DEFERRED);
             CREATE TABLE secret_hook (id int PRIMARY KEY, photo uuid REFERENCES storage.objects(id) ON DELETE CASCADE);
             CREATE FUNCTION secret_defer() RETURNS trigger LANGUAGE plpgsql AS $$
                 BEGIN SET CONSTRAINTS ALL DEFERRED; RETURN OLD; END $$;
             CREATE TRIGGER secret_defer BEFORE DELETE ON secret_hook FOR EACH ROW EXECUTE FUNCTION secret_defer();
             CREATE FUNCTION secret_late_insert() RETURNS trigger LANGUAGE plpgsql AS $$
                 BEGIN SET CONSTRAINTS ALL DEFERRED; INSERT INTO secret_late VALUES (OLD.id, gen_random_uuid()); RETURN NULL; END $$;
             CREATE CONSTRAINT TRIGGER secret_late_insert AFTER DELETE ON secret_hook
                 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION secret_late_insert();
             CREATE TABLE secret_hook2 (id int PRIMARY KEY, photo uuid REFERENCES storage.objects(id) ON DELETE CASCADE);
             CREATE TRIGGER secret_defer2 BEFORE DELETE ON secret_hook2 FOR EACH ROW EXECUTE FUNCTION secret_defer();
             CREATE FUNCTION secret_late_raise() RETURNS trigger LANGUAGE plpgsql AS $$
                 BEGIN SET CONSTRAINTS ALL DEFERRED; INSERT INTO secret_hook2 VALUES (100 + OLD.id, NULL); RETURN NULL; END $$;
             CREATE FUNCTION secret_boom() RETURNS trigger LANGUAGE plpgsql AS $$
                 BEGIN RAISE EXCEPTION 'secret_boom row %', NEW.id USING DETAIL = 'secret detail', HINT = 'secret hint'; END $$;
             CREATE CONSTRAINT TRIGGER secret_boom AFTER INSERT ON secret_hook2
                 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN (NEW.id > 100) EXECUTE FUNCTION secret_boom();
             CREATE CONSTRAINT TRIGGER secret_late_raise AFTER DELETE ON secret_hook2
                 DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION secret_late_raise();",
        )
        .await
        .expect("the owner's tables");

        let store = Arc::new(FailingStore::default());
        let (base, token) = in_process_storage(store.clone(), fx.main).await;
        let call = async |method: &str, path: &str, body: Vec<u8>, headers: &[(&str, &str)]| {
            let (status, b) = raw(&base, &token, method, path, body, headers).await;
            (status, json_of(&b))
        };
        let up = |name: &str| format!("/storage/v1/object/files/{name}");
        for name in ["late.txt", "raise.txt", "free.txt"] {
            let (status, b) = call("POST", &up(name), b"x".to_vec(), &[TEXT]).await;
            assert_eq!(status, 200, "{name}: {b}");
        }
        let late = fx.value("SELECT id FROM storage.objects WHERE name = 'late.txt'").await;
        let raise = fx.value("SELECT id FROM storage.objects WHERE name = 'raise.txt'").await;
        fx.owner(&format!("INSERT INTO secret_hook VALUES (1, '{late}'); INSERT INTO secret_hook2 VALUES (1, '{raise}');"))
            .await
            .unwrap();

        // Every case runs, and the failures are reported together.
        let mut failures = Vec::new();
        let mut check = |case: String, code: &str, status: u16, got: &Value| {
            let text = got.to_string();
            let leaked = ["secret", "photo", "constraint \"", "relation \"", "table \""].into_iter().find(|s| text.contains(s));
            let extra = got["error"].get("detail").is_some() || got["error"].get("hint").is_some();
            if leaked.is_some() || extra || (status, &got["error"]["code"]) != (409, &json!(code)) {
                failures.push(format!("{case}: want 409 {code} with no owner text, got {status} {got}"));
            }
        };
        for (name, code) in [("late.txt", "referenced"), ("raise.txt", "delete_refused")] {
            let (status, got) = call("DELETE", &up(name), vec![], &[]).await;
            check(name.to_owned(), code, status, &got);
            let batch = json!({ "names": ["free.txt", name] }).to_string().into_bytes();
            let (status, got) = call("DELETE", "/storage/v1/object/files", batch, &[JSON]).await;
            check(format!("batch {name}"), code, status, &got);
        }
        assert!(failures.is_empty(), "{}", failures.join("\n"));
        assert_eq!(
            fx.value("SELECT string_agg(name, ',' ORDER BY name) FROM storage.objects").await,
            "free.txt,late.txt,raise.txt",
            "nothing deleted"
        );
        assert_eq!(fx.owner("SELECT count(*) FROM secret_late").await.unwrap().trim(), "0");
        assert_eq!(call("DELETE", &up("free.txt"), vec![], &[]).await.0, 204, "an unreferenced object still goes");
    }))
    .await;
}
