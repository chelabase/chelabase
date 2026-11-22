//! Live tests against a real Postgres: the stack's metadata Postgres
//! (`CHELABASE_METADATA_DATABASE_URL`), where each test makes its own database and roles.

use std::sync::Arc;

use sqlx::Executor;
use sqlx::postgres::PgPool;
use uuid::Uuid;

use crate::pools::{ConnError, ConnInfo, ConnSource, Pools, ServedDatabase, Service, Settings};
use crate::service::{NamedStatement, ServiceError, ServicePool};

fn url() -> reqwest::Url {
    let url = std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".into()
    });
    reqwest::Url::parse(&url).unwrap()
}

/// A scratch database with the data API's `chelabase.schema_version`, and the roles a test
/// makes (dropped by [`Scratch::drop`]).
struct Scratch {
    admin: PgPool,
    url: reqwest::Url,
    db: String,
    suffix: String,
    roles: std::sync::Mutex<Vec<String>>,
}

impl Scratch {
    async fn new() -> Self {
        let url = url();
        let admin = PgPool::connect(url.as_str()).await.unwrap();
        let suffix = Uuid::new_v4().simple().to_string()[..8].to_owned();
        let db = format!("pgs_live_{suffix}");
        admin
            .execute(sqlx::AssertSqlSafe(format!("CREATE DATABASE {db}")))
            .await
            .unwrap();
        let scratch = Scratch {
            admin,
            url,
            db,
            suffix,
            roles: Default::default(),
        };
        scratch
            .in_db(
                "CREATE SCHEMA chelabase; \
                 CREATE TABLE chelabase.schema_version (version uuid); \
                 INSERT INTO chelabase.schema_version VALUES (gen_random_uuid()); \
                 GRANT USAGE ON SCHEMA chelabase TO PUBLIC; \
                 GRANT SELECT ON chelabase.schema_version TO PUBLIC",
            )
            .await;
        scratch
    }

    /// `sql` as the admin in the scratch database.
    async fn in_db(&self, sql: &str) {
        let mut url = self.url.clone();
        url.set_path(&self.db);
        let pool = PgPool::connect(url.as_str()).await.unwrap();
        pool.execute(sqlx::AssertSqlSafe(sql.to_owned()))
            .await
            .unwrap();
        pool.close().await;
    }

    /// A new role `<name>_<suffix>` with `attributes` (password `pw` when it logs in).
    async fn role(&self, name: &str, attributes: &str) -> String {
        let role = format!("{name}_{}", self.suffix);
        self.admin
            .execute(sqlx::AssertSqlSafe(format!(
                "CREATE ROLE {role} {attributes}; GRANT CONNECT ON DATABASE {} TO {role}",
                self.db
            )))
            .await
            .unwrap();
        self.roles.lock().unwrap().push(role.clone());
        role
    }

    async fn sql(&self, sql: &str) {
        self.admin
            .execute(sqlx::AssertSqlSafe(sql.to_owned()))
            .await
            .unwrap();
    }

    /// The branch's info, connecting as `user` (`None`: the admin).
    fn info(&self, user: Option<&str>) -> ConnInfo {
        let (user, password) = match user {
            Some(u) => (u.to_owned(), "pw".to_owned()),
            None => (
                self.url.username().to_owned(),
                self.url.password().unwrap_or_default().to_owned(),
            ),
        };
        ConnInfo {
            host: self.url.host_str().unwrap().into(),
            port: self.url.port().unwrap_or(5432),
            server_name: None,
            database: self.db.clone(),
            database_id: None,
            user,
            password,
            elevated: None,
            settings: Settings {
                max_rows: 10,
                statement_timeout_ms: 5000,
                max_connections: 4,
                storage_max_connections: 2,
            },
            databases: vec![ServedDatabase {
                name: self.db.clone(),
                default_schema: "public".into(),
            }],
        }
    }

    async fn drop(self) {
        self.admin
            .execute(sqlx::AssertSqlSafe(format!(
                "DROP DATABASE {} WITH (FORCE)",
                self.db
            )))
            .await
            .unwrap();
        let roles = std::mem::take(&mut *self.roles.lock().unwrap());
        for role in roles.iter().rev() {
            let _ = self
                .admin
                .execute(sqlx::AssertSqlSafe(format!("DROP ROLE IF EXISTS {role}")))
                .await;
        }
    }
}

struct Fixed(ConnInfo);

impl ConnSource for Fixed {
    async fn fetch(&self, _branch: Uuid) -> Result<ConnInfo, ConnError> {
        Ok(self.0.clone())
    }
}

/// Storage's role pools (as the admin) and a managed pool beside them logging in as `user`.
fn managed(scratch: &Scratch, user: Option<&str>) -> (Pools<Fixed>, ServicePool<Fixed>) {
    let roles = Pools::new(Fixed(scratch.info(None)), 10).for_service(Service::Storage, |_| 2);
    let managed = ServicePool::beside(&roles, Fixed(scratch.info(user)));
    (roles, managed)
}

const WHO: NamedStatement = crate::named_statement!(
    "who",
    "SELECT current_user::text, current_setting('search_path')",
    &[]
);

/// The managed pool opens only as a bare service role: a superuser, a role with any
/// membership either way, or one that can create roles or databases or replicate is refused
/// before any statement. BYPASSRLS is allowed (storage's login has it by design).
#[tokio::test]
#[ignore = "needs a Postgres: CHELABASE_METADATA_DATABASE_URL or the workbench stack's metadata Postgres"]
async fn the_managed_pool_opens_only_as_a_bare_service_role_live() {
    let scratch = Scratch::new().await;
    let bare = scratch
        .role("pgs_svc", "LOGIN PASSWORD 'pw' BYPASSRLS")
        .await;
    let member = scratch.role("pgs_member", "LOGIN PASSWORD 'pw'").await;
    scratch
        .sql(&format!("GRANT pg_read_all_data TO {member}"))
        .await;
    let granter = scratch.role("pgs_granter", "LOGIN PASSWORD 'pw'").await;
    let grantee = scratch.role("pgs_grantee", "NOLOGIN").await;
    scratch.sql(&format!("GRANT {granter} TO {grantee}")).await;
    let creator = scratch
        .role("pgs_creator", "LOGIN PASSWORD 'pw' CREATEROLE")
        .await;
    let replicator = scratch
        .role("pgs_repl", "LOGIN PASSWORD 'pw' REPLICATION BYPASSRLS")
        .await;
    let db_creator = scratch
        .role("pgs_createdb", "LOGIN PASSWORD 'pw' CREATEDB")
        .await;
    let branch = Uuid::new_v4();

    for (name, user) in [
        ("a superuser", None),
        ("a member of pg_read_all_data", Some(member.as_str())),
        ("a role with a member", Some(granter.as_str())),
        ("a CREATEROLE role", Some(creator.as_str())),
        ("a REPLICATION role", Some(replicator.as_str())),
        ("a CREATEDB role", Some(db_creator.as_str())),
    ] {
        let (_roles, pool) = managed(&scratch, user);
        let out = pool.query(branch, &scratch.db, &WHO, &[], "rid").await;
        assert!(
            matches!(out, Err(ServiceError::Acquire(_))),
            "{name}: {:?}",
            out.map(|rows| rows.len())
        );
    }

    let (_roles, pool) = managed(&scratch, Some(&bare));
    let rows = pool
        .query(branch, &scratch.db, &WHO, &[], "rid")
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, String>(0), bare);
    assert_eq!(rows[0].get::<_, String>(1), "pg_catalog,pg_temp");
    drop(pool);
    scratch.drop().await;
}

/// Statements that would change the role, built around the text check (only this crate
/// can): on the bare service role, Postgres refuses each with permission denied.
#[tokio::test]
#[ignore = "needs a Postgres: CHELABASE_METADATA_DATABASE_URL or the workbench stack's metadata Postgres"]
async fn the_database_refuses_a_managed_role_change_live() {
    let scratch = Scratch::new().await;
    let bare = scratch.role("pgs_svc", "LOGIN PASSWORD 'pw'").await;
    let (_roles, pool) = managed(&scratch, Some(&bare));
    let branch = Uuid::new_v4();
    for sql in [
        "/**/SET/**/ROLE pg_read_all_data",
        "SELECT U&\"set\\005fconfig\"('role', 'pg_read_all_data', false)::text",
        "DO $$BEGIN EXECUTE format('%s ROLE pg_read_all_data', 'SE' || 'T'); END$$",
    ] {
        let statement = NamedStatement::unchecked_for_tests("bypass", sql);
        let out = pool
            .query(branch, &scratch.db, &statement, &[], "rid")
            .await;
        match out {
            Err(ServiceError::Query(f)) => assert!(
                f.sqlstate.as_deref() == Some("42501") || f.message.contains("permission denied"),
                "{sql}: {f:?}"
            ),
            other => panic!("{sql}: {:?}", other.map(|rows| rows.len())),
        }
    }
    drop(pool);
    scratch.drop().await;
}

/// A role session for a role that doesn't exist (or isn't the role its key was bound to)
/// runs nothing: the preamble fails before the caller's statements. One for a real role runs
/// as it, with its claims.
#[tokio::test]
#[ignore = "needs a Postgres: CHELABASE_METADATA_DATABASE_URL or the workbench stack's metadata Postgres"]
async fn a_session_for_a_missing_role_runs_nothing_live() {
    let scratch = Scratch::new().await;
    let reader = scratch.role("pgs_reader", "NOLOGIN").await;
    let pools = Pools::new(Fixed(scratch.info(None)), 10);
    let branch = Uuid::new_v4();
    let claims = serde_json::json!({ "sub": "u1" });
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let db = scratch.db.clone();
    let run = |role: String, oid: Option<i64>| {
        let (pools, claims, ran, db) = (&pools, &claims, ran.clone(), db.clone());
        async move {
            let conn = pools.acquire(branch, &db).await.unwrap().bound_to(&role, None);
            conn.with_role_session(&role, claims, oid, async |s| {
                let settings = s.settings(&s.info().databases[0]);
                s.transaction(&db, &settings, "rid", async |c, _version| {
                    ran.store(true, std::sync::atomic::Ordering::SeqCst);
                    let row = c
                        .query_one(
                            "SELECT current_user::text, current_setting('request.jwt.claims', true)",
                            &[],
                        )
                        .await
                        .map_err(|e| chelabase_query::QueryFailure::from_pg(&e, "rid"))?;
                    Ok((row.get::<_, String>(0), row.get::<_, Option<String>>(1)))
                })
                .await
            })
            .await
            .expect("a role")
        }
    };

    let who = run(reader.clone(), None).await.unwrap();
    assert_eq!(who, (reader.clone(), Some(claims.to_string())));

    for (name, role, oid) in [
        (
            "a missing role",
            format!("pgs_ghost_{}", scratch.suffix),
            None,
        ),
        ("another role under the key's name", reader.clone(), Some(1)),
    ] {
        ran.store(false, std::sync::atomic::Ordering::SeqCst);
        let out = run(role, oid).await;
        assert!(out.is_err(), "{name}: {out:?}");
        assert!(
            !ran.load(std::sync::atomic::Ordering::SeqCst),
            "{name}: a statement ran"
        );
    }
    drop(pools);
    scratch.drop().await;
}

/// A membership granted after a managed connection was pooled is caught on its next
/// checkout: the pooled connection isn't used, and no new one opens.
#[tokio::test]
#[ignore = "needs a Postgres: CHELABASE_METADATA_DATABASE_URL or the workbench stack's metadata Postgres"]
async fn a_grant_after_pooling_closes_the_managed_pool_live() {
    let scratch = Scratch::new().await;
    let bare = scratch.role("pgs_svc", "LOGIN PASSWORD 'pw'").await;
    let target = scratch.role("pgs_target", "NOLOGIN").await;
    let (_roles, pool) = managed(&scratch, Some(&bare));
    let branch = Uuid::new_v4();
    let sql: &'static str =
        format!("SELECT pg_catalog.set_config('role', '{target}', false)::text").leak();
    let escalate = NamedStatement::unchecked_for_tests("escalate", sql);
    let before = pool.query(branch, &scratch.db, &escalate, &[], "rid").await;
    assert!(
        matches!(&before, Err(ServiceError::Query(f)) if f.message.contains("permission denied")
            || f.sqlstate.as_deref() == Some("42501")),
        "before the grant: {:?}",
        before.map(|rows| rows.len())
    );
    scratch.sql(&format!("GRANT {target} TO {bare}")).await;
    let after = pool.query(branch, &scratch.db, &escalate, &[], "rid").await;
    assert!(
        matches!(after, Err(ServiceError::Acquire(_))),
        "after the grant, on the pooled connection: {:?}",
        after.map(|rows| rows.len())
    );
    drop(pool);
    scratch.drop().await;
}

/// A role session for `none` (Postgres's word for no role) runs nothing: it isn't the
/// login's.
#[tokio::test]
#[ignore = "needs a Postgres: CHELABASE_METADATA_DATABASE_URL or the workbench stack's metadata Postgres"]
async fn a_role_session_for_none_runs_nothing_live() {
    let scratch = Scratch::new().await;
    let pools = Pools::new(Fixed(scratch.info(None)), 10);
    let branch = Uuid::new_v4();
    let claims = serde_json::json!({});
    let db = scratch.db.clone();
    for role in ["none", ""] {
        let ran = std::sync::atomic::AtomicBool::new(false);
        let conn = pools.acquire(branch, &db).await.unwrap();
        let out = conn
            .with_role_session(role, &claims, None, async |s| {
                let settings = s.settings(&s.info().databases[0]);
                s.transaction(&db, &settings, "rid", async |c, _version| {
                    ran.store(true, std::sync::atomic::Ordering::SeqCst);
                    let row = c
                        .query_one("SELECT current_user::text", &[])
                        .await
                        .map_err(|e| chelabase_query::QueryFailure::from_pg(&e, "rid"))?;
                    Ok(row.get::<_, String>(0))
                })
                .await
            })
            .await;
        assert!(
            !ran.load(std::sync::atomic::Ordering::SeqCst),
            "{role:?} ran a statement: {out:?}"
        );
        assert_eq!(out.unwrap_err(), crate::session::NoRole(role.to_owned()));
    }
    drop(pools);
    scratch.drop().await;
}

// --- session integrity of a pooled connection (m3) -----------------------------------------

/// The data API's machinery in a scratch database (the guard functions the pipelined path
/// needs, as the worker installs them) and a table the roles of a test may read and write.
const DATA_API_SQL: &str = "CREATE FUNCTION chelabase.assert_schema_version(expected uuid) RETURNS uuid
  LANGUAGE plpgsql STABLE SECURITY DEFINER SET search_path = '' AS
  $f$ DECLARE v uuid; BEGIN
    SELECT version INTO v FROM chelabase.schema_version;
    IF v IS DISTINCT FROM expected THEN
      RAISE EXCEPTION USING ERRCODE = 'CB001', MESSAGE = 'chelabase: stale schema version';
    END IF;
    RETURN v; END $f$;
CREATE FUNCTION chelabase.cap_response(result text, budget bigint) RETURNS text
  LANGUAGE plpgsql VOLATILE SET search_path = '' AS
  $f$ DECLARE n bigint := coalesce(pg_catalog.octet_length(result), 0); total bigint; BEGIN
    IF n OPERATOR(pg_catalog.>) budget THEN
      RAISE EXCEPTION USING ERRCODE = 'CB002', MESSAGE = 'chelabase: response too large';
    END IF;
    total := coalesce(nullif(pg_catalog.current_setting('chelabase.response_bytes', true), '')::bigint, 0) OPERATOR(pg_catalog.+) n;
    IF total OPERATOR(pg_catalog.>) budget THEN
      RAISE EXCEPTION USING ERRCODE = 'CB002', MESSAGE = 'chelabase: response too large';
    END IF;
    PERFORM pg_catalog.set_config('chelabase.response_bytes', total::text, true);
    RETURN result; END $f$;
CREATE TABLE public.items (id bigint, body text);
GRANT SELECT, INSERT ON public.items TO PUBLIC;";

/// A data API query tree, parsed.
fn op(tree: serde_json::Value) -> chelabase_query::Op {
    chelabase_query::parse(tree.to_string().as_bytes())
        .unwrap()
        .op
}

fn insert_item(id: i64) -> chelabase_query::Op {
    op(serde_json::json!({
        "v": 1, "op": "insert", "into": "items",
        "values": [{"id": id, "body": "x"}], "returning": ["id"],
    }))
}

/// What a session's connection is, read inside a transaction of the session: its backend
/// pid, who it runs as, and how many protocol-level statements it holds (its cache).
async fn connection_facts(
    s: &crate::session::RoleSession,
    db: &str,
) -> Result<(i32, String, i64), chelabase_query::QueryFailure> {
    let settings = s.settings(&s.info().databases[0]);
    s.transaction(db, &settings, "rid", async |c, _version| {
        let row = c
            .query_one(
                "SELECT pg_catalog.pg_backend_pid(), current_user::pg_catalog.text, \
                 (SELECT pg_catalog.count(*) FROM pg_catalog.pg_prepared_statements p \
                   WHERE NOT p.from_sql)",
                &[],
            )
            .await
            .map_err(|e| chelabase_query::QueryFailure::from_pg(&e, "rid"))?;
        Ok((row.get(0), row.get(1), row.get(2)))
    })
    .await
}

/// The ids committed to `public.items`, as the admin sees them.
async fn item_ids(scratch: &Scratch) -> Vec<i64> {
    let mut url = scratch.url.clone();
    url.set_path(&scratch.db);
    let pool = PgPool::connect(url.as_str()).await.unwrap();
    let ids = sqlx::query_scalar("SELECT id FROM public.items ORDER BY id")
        .fetch_all(&pool)
        .await
        .unwrap();
    pool.close().await;
    ids
}

/// AC 6: ordinary requests back to back on one pool (reads, writes and a batch as two
/// roles, a transaction, a schema read, the login's own session) each succeed and keep the
/// connection: the backend pid never changes, and a repeated pipelined request prepares
/// nothing new (its statements come from the connection's cache).
#[tokio::test]
#[ignore = "needs a Postgres: CHELABASE_METADATA_DATABASE_URL or the workbench stack's metadata Postgres"]
async fn clean_requests_keep_their_connection() {
    let scratch = Scratch::new().await;
    scratch.in_db(DATA_API_SQL).await;
    let anon = scratch.role("pgs_anon", "NOLOGIN").await;
    let app = scratch.role("pgs_app", "NOLOGIN").await;
    let login = scratch.url.username().to_owned();
    let pools = Pools::new(Fixed(scratch.info(None)), 10);
    let cache = chelabase_query::QueryCache::default();
    let branch = Uuid::new_v4();
    let claims = serde_json::json!({ "sub": "u1" });
    let db = scratch.db.clone();
    let select = op(serde_json::json!({"v": 1, "op": "select", "from": "items"}));
    let batch = op(serde_json::json!({"v": 1, "op": "batch", "ops": [
        {"op": "select", "from": "items"},
        {"op": "insert", "into": "items", "values": [{"id": 50, "body": "b"}]},
    ]}));

    let mut pids = Vec::new();
    let mut cached = Vec::new();
    // Twice through: the first pass plans and prepares, the second is all cache hits.
    for pass in 0..3_i64 {
        for role in [&anon, &app] {
            let conn = pools.acquire(branch, &db).await.unwrap().bound_to(role, None);
            let facts = conn
                .with_role_session(role, &claims, None, async |s| {
                    let settings = s.settings(&s.info().databases[0]);
                    let key = (branch, db.as_str());
                    s.run(&cache, key, &settings, 1 << 20, &select, "rid")
                        .await
                        .unwrap();
                    s.run(&cache, key, &settings, 1 << 20, &insert_item(pass), "rid")
                        .await
                        .unwrap();
                    s.run(&cache, key, &settings, 1 << 20, &batch, "rid")
                        .await
                        .unwrap();
                    s.schema(&cache, key, 5000, "rid", async |_c, _schema| Ok(()))
                        .await
                        .unwrap();
                    connection_facts(s, &db).await.unwrap()
                })
                .await
                .unwrap();
            assert_eq!(&facts.1, role, "the session runs as its role");
            pids.push(facts.0);
            if pass > 0 {
                cached.push(facts.2);
            }
        }
        // The login's own session, on the same pool.
        let conn = pools.acquire(branch, &db).await.unwrap();
        let facts = conn
            .with_login_session(async |s| connection_facts(s, &db).await.unwrap())
            .await
            .unwrap();
        assert_eq!(facts.1, login, "the login session runs as the login");
        pids.push(facts.0);
    }
    assert!(
        pids.iter().all(|pid| *pid == pids[0]),
        "every request ran on the same backend: {pids:?}"
    );
    assert!(
        cache
            .pipelined_runs
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 12,
        "the repeated requests ran pipelined"
    );
    assert!(
        cached.iter().all(|n| *n == cached[0]) && cached[0] > 0,
        "repeated requests prepare nothing new: {cached:?}"
    );
    assert_eq!(item_ids(&scratch).await, vec![0, 0, 1, 1, 2, 2, 50, 50, 50, 50, 50, 50]);
    drop(pools);
    scratch.drop().await;
}

/// AC 4: a deferred constraint trigger that creates a SQL-level prepared statement closes
/// the connection it ran on, and the next request on the pool gets another backend and
/// runs unchanged. In a role session the trigger fires before the session check, so nothing
/// commits (a 500); on a service's own login (no check: it holds no role to switch to) the
/// work commits and the reset after its `COMMIT` fails, which closes the connection.
#[tokio::test]
#[ignore = "needs a Postgres: CHELABASE_METADATA_DATABASE_URL or the workbench stack's metadata Postgres"]
async fn deferred_prepare_closes_the_connection() {
    let scratch = Scratch::new().await;
    scratch.in_db(DATA_API_SQL).await;
    scratch
        .in_db(
            "CREATE FUNCTION public.at_commit() RETURNS trigger LANGUAGE plpgsql AS
               $f$ BEGIN
                 IF NEW.id = 1 THEN EXECUTE 'PREPARE pgs_left AS SELECT 1'; END IF;
                 RETURN NULL; END $f$;
             CREATE CONSTRAINT TRIGGER at_commit AFTER INSERT ON public.items
               DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION public.at_commit();",
        )
        .await;
    let app = scratch.role("pgs_app", "NOLOGIN").await;
    let branch = Uuid::new_v4();
    let db = scratch.db.clone();

    // A role session.
    let pools = Pools::new(Fixed(scratch.info(None)), 10);
    let cache = chelabase_query::QueryCache::default();
    let claims = serde_json::json!({});
    let request = async |id: i64| {
        let conn = pools.acquire(branch, &db).await.unwrap().bound_to(&app, None);
        conn.with_role_session(&app, &claims, None, async |s| {
            let settings = s.settings(&s.info().databases[0]);
            let (pid, _, _) = connection_facts(s, &db).await?;
            let key = (branch, db.as_str());
            let out = s
                .run(&cache, key, &settings, 1 << 20, &insert_item(id), "rid")
                .await;
            Ok::<_, chelabase_query::QueryFailure>((pid, out))
        })
        .await
        .unwrap()
        .unwrap()
    };
    // Sequential first (nothing cached), then pipelined. The table has a deferrable
    // constraint trigger, so every request that writes it gives its connection up: each
    // request here runs on a backend of its own.
    let (warm_pid, out) = request(2).await;
    out.unwrap();
    let mut last_pid = warm_pid;
    for round in ["sequential or pipelined", "pipelined"] {
        let (pid, out) = request(1).await;
        assert_ne!(pid, last_pid, "{round}: a write to the table isn't followed on its connection");
        let e = out.unwrap_err();
        assert_eq!(
            (e.status.as_u16(), e.code),
            (500, "internal"),
            "{round}: {e:?}"
        );
        let (next_pid, out) = request(3).await;
        assert_ne!(next_pid, pid, "{round}: the next request gets another backend");
        out.unwrap();
        last_pid = next_pid;
    }
    assert_eq!(item_ids(&scratch).await, vec![2, 3, 3], "the refused rows never committed");
    drop(pools);

    // A service's own login: no session check, so the reset after COMMIT is what refuses.
    let bare = scratch.role("pgs_svc", "LOGIN PASSWORD 'pw'").await;
    let (_roles, pool) = managed(&scratch, Some(&bare));
    const PID: NamedStatement =
        crate::named_statement!("pid", "SELECT pg_catalog.pg_backend_pid()", &[]);
    let pid = async || {
        pool.query(branch, &db, &PID, &[], "rid").await.unwrap()[0].get::<_, i32>(0)
    };
    let before = pid().await;
    assert_eq!(pid().await, before, "the managed connection is pooled");
    let on_commit =
        |e: &tokio_postgres::Error| chelabase_query::QueryFailure::from_pg(e, "rid");
    let ran_on = pool
        .transaction(branch, &db, 5000, "rid", on_commit, async |c| {
            let pg = |e: tokio_postgres::Error| chelabase_query::QueryFailure::from_pg(&e, "rid");
            c.batch_execute("INSERT INTO public.items VALUES (1, 'svc')")
                .await
                .map_err(pg)?;
            let row = c
                .query_one("SELECT pg_catalog.pg_backend_pid()", &[])
                .await
                .map_err(pg)?;
            Ok(row.get::<_, i32>(0))
        })
        .await
        .unwrap();
    assert_eq!(ran_on, before);
    assert_eq!(item_ids(&scratch).await, vec![1, 2, 3, 3], "the service's work committed");
    let after = pid().await;
    assert_ne!(after, before, "the reset failed: the next request gets another backend");
    assert_eq!(pid().await, after, "and that one is pooled");
    drop(pool);
    scratch.drop().await;
}
