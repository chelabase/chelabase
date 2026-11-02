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
use serde_json::{Value, json};

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
