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
            user,
            password,
            settings: Settings {
                max_rows: 10,
                statement_timeout_ms: 5000,
                max_connections: 4,
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
/// membership either way, or one that can create roles is refused before any statement.
#[tokio::test]
#[ignore = "needs a Postgres: CHELABASE_METADATA_DATABASE_URL or the workbench stack's metadata Postgres"]
async fn the_managed_pool_opens_only_as_a_bare_service_role_live() {
    let scratch = Scratch::new().await;
    let bare = scratch.role("pgs_svc", "LOGIN PASSWORD 'pw'").await;
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
    let branch = Uuid::new_v4();

    for (name, user) in [
        ("a superuser", None),
        ("a member of pg_read_all_data", Some(member.as_str())),
        ("a role with a member", Some(granter.as_str())),
        ("a CREATEROLE role", Some(creator.as_str())),
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
    assert_eq!(rows[0].get::<_, String>(1), "pg_catalog, pg_temp");
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
            let conn = pools.acquire(branch, &db).await.unwrap();
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
