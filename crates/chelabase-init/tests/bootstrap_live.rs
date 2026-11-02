//! The bootstrap's metadata side (`SqlMetadata`) against a real Postgres: a scratch database
//! on the workbench stack's metadata server, dropped at the end. Run with
//! `just workbench test --stack <stack> -p chelabase-init -- --run-ignored only -E 'test(bootstrap_live)'`.

use chelabase_init::bootstrap::SEEDED_TENANT;
use chelabase_init::bootstrap::retry::Check;
use chelabase_init::bootstrap::tenant::{Metadata, SqlMetadata};
use chelabase_metadata::{MetadataClient, MetadataConfig};
use sqlx::AssertSqlSafe;
use sqlx::postgres::PgPoolOptions;

fn url() -> String {
    std::env::var("CHELABASE_METADATA_DATABASE_URL").unwrap_or_else(|_| {
        "postgres://chelabase:chelabase@metadata-db:5432/chelabase_metadata".into()
    })
}

fn url_for(name: &str) -> String {
    let base = url();
    let (head, tail) = base.rsplit_once('/').expect("a database in the URL");
    let query = tail
        .split_once('?')
        .map(|(_, q)| format!("?{q}"))
        .unwrap_or_default();
    format!("{head}/{name}{query}")
}

async fn org(pool: &sqlx::PgPool) -> Option<String> {
    sqlx::query_scalar("SELECT zitadel_org_id FROM tenants WHERE id = $1")
        .bind(SEEDED_TENANT)
        .fetch_one(pool)
        .await
        .unwrap()
}

// S4: unmigrated waits; then the bind sets, keeps and refuses A -> B naming both; two
// concurrent binds from NULL: exactly one wins and the other is refused or kept.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn bootstrap_live_sql_bind() {
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url())
        .await
        .expect("metadata Postgres reachable");
    let name = format!("cm_boot_{}", uuid::Uuid::new_v4().simple());
    sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .unwrap();
    let scratch = name.clone();
    let result = async move {
        let meta = SqlMetadata::new(&url_for(&scratch)).unwrap();
        assert!(
            matches!(meta.migrated().await.unwrap(), Check::Wait(_)),
            "an empty database is not migrated"
        );
        let client = MetadataClient::connect(&MetadataConfig {
            database_url: url_for(&scratch),
        })
        .await
        .unwrap();
        client.migrate().await.unwrap();
        assert_eq!(meta.migrated().await.unwrap(), Check::Ready(()));
        let pool = client.pool();

        assert!(
            meta.bind_org(SEEDED_TENANT, "org-A").await.unwrap(),
            "NULL: bound"
        );
        assert!(
            !meta.bind_org(SEEDED_TENANT, "org-A").await.unwrap(),
            "equal: kept"
        );
        let err = meta
            .bind_org(SEEDED_TENANT, "org-B")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("org-A") && err.contains("org-B"), "{err}");
        assert_eq!(
            org(pool).await.as_deref(),
            Some("org-A"),
            "a refusal writes nothing"
        );

        // Named once, after the org; a second name is ignored.
        assert!(meta.fill_display_name(SEEDED_TENANT, "Acme").await.unwrap());
        assert!(
            !meta
                .fill_display_name(SEEDED_TENANT, "Other")
                .await
                .unwrap()
        );

        sqlx::query("UPDATE tenants SET zitadel_org_id = NULL WHERE id = $1")
            .bind(SEEDED_TENANT)
            .execute(pool)
            .await
            .unwrap();
        let other = SqlMetadata::new(&url_for(&scratch)).unwrap();
        let (a, b) = tokio::join!(
            meta.bind_org(SEEDED_TENANT, "org-X"),
            other.bind_org(SEEDED_TENANT, "org-Y")
        );
        let wins = [&a, &b].iter().filter(|r| matches!(r, Ok(true))).count();
        assert_eq!(wins, 1, "{a:?} {b:?}");
        let bound = org(pool).await.unwrap();
        let loser = if matches!(a, Ok(true)) { &b } else { &a };
        let msg = loser.as_ref().unwrap_err().to_string();
        assert!(
            msg.contains(&bound),
            "the loser names the winner's org: {msg}"
        );
        pool.close().await;
    };
    let outcome = tokio::spawn(result).await;
    sqlx::raw_sql(AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .unwrap();
    outcome.unwrap();
}

// --- auth-replacement m5 (#118): the identity role, its functions and the members pool ---
//
// Roles are cluster-wide, so the role tests use their own role names (never the stack's
// `chelabase_identity`) in a scratch database, and drop both at the end.

use chelabase_init::roles::{IdentityRoles, POOL_SCHEMA_PATTERN, ensure_identity_roles};
use sqlx::postgres::PgConnectOptions;
use sqlx::{Connection, PgConnection};
use std::str::FromStr;

const ROLE_PASSWORD: &str = "cm-identity-test-password-0123456789";

fn hex32() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// A scratch database with the metadata migrations applied, and two fresh role names.
struct Scratch {
    admin: sqlx::PgPool,
    db: String,
    roles: IdentityRoles,
}

impl Scratch {
    async fn new() -> Self {
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url())
            .await
            .expect("metadata Postgres reachable");
        let tag = hex32();
        let db = format!("cm_ident_{tag}");
        sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {db}")))
            .execute(&admin)
            .await
            .unwrap();
        let client = MetadataClient::connect(&MetadataConfig {
            database_url: url_for(&db),
        })
        .await
        .unwrap();
        client.migrate().await.unwrap();
        client.pool().close().await;
        let roles = IdentityRoles {
            login: format!("cm_ident_{}", &tag[..12]),
            owner: format!("cm_ident_{}_admin", &tag[..12]),
        };
        Scratch { admin, db, roles }
    }

    /// A superuser connection to the scratch database.
    async fn superuser(&self) -> PgConnection {
        PgConnection::connect(&url_for(&self.db)).await.unwrap()
    }

    /// A connection to the scratch database as the identity login role.
    async fn as_login(&self) -> PgConnection {
        let opts = PgConnectOptions::from_str(&url_for(&self.db))
            .unwrap()
            .username(&self.roles.login)
            .password(ROLE_PASSWORD);
        PgConnection::connect_with(&opts)
            .await
            .expect("the login role connects with its password")
    }

    async fn ensure(&self) {
        let mut su = self.superuser().await;
        ensure_identity_roles(&mut su, &self.roles, ROLE_PASSWORD)
            .await
            .unwrap();
        su.close().await.unwrap();
    }

    async fn drop(self) {
        sqlx::raw_sql(AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            self.db
        )))
        .execute(&self.admin)
        .await
        .unwrap();
        for role in [&self.roles.owner, &self.roles.login] {
            sqlx::raw_sql(AssertSqlSafe(format!("DROP ROLE IF EXISTS {role}")))
                .execute(&self.admin)
                .await
                .unwrap();
        }
    }
}

/// The SQLSTATE of a failed statement (`None` when it succeeded).
async fn sqlstate(conn: &mut PgConnection, sql: &str) -> Option<String> {
    match sqlx::raw_sql(AssertSqlSafe(sql.to_string()))
        .execute(&mut *conn)
        .await
    {
        Ok(_) => None,
        Err(sqlx::Error::Database(d)) => Some(d.code().unwrap_or_default().to_string()),
        Err(e) => panic!("{sql}: {e}"),
    }
}

/// One `identity_admin` function: name, SECURITY DEFINER, proconfig, owner, owner is superuser,
/// owner can log in, ACL.
type FunctionRow = (
    String,
    bool,
    Option<Vec<String>>,
    String,
    bool,
    bool,
    Option<Vec<String>>,
);

/// The login role's password verifier and the functions (oid, body, grants), as one string.
async fn setup_snapshot(su: &mut PgConnection, login: &str) -> String {
    sqlx::query_scalar(
        "SELECT (SELECT rolpassword FROM pg_authid WHERE rolname = $1)
             || (SELECT string_agg(p.oid::text || p.prosrc || p.proacl::text, ',' ORDER BY p.oid)
                   FROM pg_proc p WHERE p.pronamespace = 'identity_admin'::regnamespace)",
    )
    .bind(login)
    .fetch_one(su)
    .await
    .unwrap()
}

/// Runs `body` on a fresh scratch, then drops it whatever happened.
async fn with_scratch<F, Fut>(body: F)
where
    F: FnOnce(std::sync::Arc<Scratch>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let scratch = std::sync::Arc::new(Scratch::new().await);
    let outcome = tokio::spawn(body(scratch.clone())).await;
    std::sync::Arc::into_inner(scratch)
        .expect("the body keeps no handle")
        .drop()
        .await;
    outcome.unwrap();
}

// AC 2: refused on every table of `public` and in `chelabase`; succeeds on its own pool schema.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn identity_role_denied_on_chelabase_schemas() {
    with_scratch(|s| async move {
        s.ensure().await;
        let mut su = s.superuser().await;
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT format('%I.%I', schemaname, tablename) FROM pg_tables
              WHERE schemaname IN ('public', 'chelabase')",
        )
        .fetch_all(&mut su)
        .await
        .unwrap();
        assert!(tables.len() > 10, "the migrations made tables: {tables:?}");
        let mut me = s.as_login().await;
        for t in &tables {
            assert_eq!(
                sqlstate(&mut me, &format!("SELECT 1 FROM {t} LIMIT 1")).await.as_deref(),
                Some("42501"),
                "SELECT on {t}"
            );
        }
        for sql in [
            "CREATE TABLE public.cm_x (id int)",
            "CREATE TABLE chelabase.cm_x (id int)",
            "CREATE FUNCTION public.cm_f() RETURNS int LANGUAGE sql AS 'SELECT 1'",
            "CREATE VIEW chelabase.cm_v AS SELECT 1",
        ] {
            assert_eq!(sqlstate(&mut me, sql).await.as_deref(), Some("42501"), "{sql}");
        }
        // Unqualified: with no USAGE on `public` its search path holds no schema at all.
        assert_eq!(
            sqlstate(&mut me, "CREATE TABLE cm_unqualified (id int)").await.as_deref(),
            Some("3F000")
        );
        // Not a superuser, no role attribute beyond LOGIN, a member of nothing, no CREATE.
        let (sup, createdb, createrole, bypass, member_of): (bool, bool, bool, bool, i64) =
            sqlx::query_as(
                "SELECT rolsuper, rolcreatedb, rolcreaterole, rolbypassrls,
                        (SELECT count(*) FROM pg_auth_members WHERE member = r.oid)
                   FROM pg_roles r WHERE rolname = $1",
            )
            .bind(&s.roles.login)
            .fetch_one(&mut su)
            .await
            .unwrap();
        assert!(!sup && !createdb && !createrole && !bypass, "{}", s.roles.login);
        assert_eq!(member_of, 0);
        // B2 (audit): from its own pool schema it can't pin a Chelabase type or function (a
        // dependency that would block Chelabase's own DDL).
        sqlx::query("SELECT identity_admin.create_pool_schema('identity_members')")
            .execute(&mut me)
            .await
            .unwrap();
        for sql in [
            "CREATE TABLE identity_members.pin (t public.tenants)",
            "CREATE FUNCTION identity_members.pinf(x public.tenants) RETURNS int LANGUAGE sql AS 'SELECT 1'",
        ] {
            assert_eq!(sqlstate(&mut me, sql).await.as_deref(), Some("42501"), "{sql}");
        }
        // B2 (audit): not even PUBLIC's USAGE on `public`, so it can't reference Chelabase's
        // types or functions.
        let can: (bool, bool, bool, bool) = sqlx::query_as(
            "SELECT has_database_privilege($1, current_database(), 'CREATE'),
                    has_schema_privilege($1, 'public', 'CREATE'),
                    has_schema_privilege($1, 'public', 'USAGE'),
                    has_schema_privilege($1, 'chelabase', 'USAGE')",
        )
        .bind(&s.roles.login)
        .fetch_one(&mut su)
        .await
        .unwrap();
        assert_eq!(can, (false, false, false, false));
        // N1 (audit): a connection limit, so it can't exhaust the server's connections.
        let limit: i32 = sqlx::query_scalar("SELECT rolconnlimit FROM pg_roles WHERE rolname = $1")
            .bind(&s.roles.login)
            .fetch_one(&mut su)
            .await
            .unwrap();
        assert!((20..=100).contains(&limit), "rolconnlimit {limit}");

        // The success path: its own pool schema, made through the function, is its own.
        let pool = format!("identity_app_{}", hex32());
        sqlx::query("SELECT identity_admin.create_pool_schema($1)")
            .bind(&pool)
            .execute(&mut me)
            .await
            .unwrap();
        assert_eq!(
            sqlstate(&mut me, &format!("CREATE TABLE {pool}.t (id int)")).await,
            None
        );
        assert_eq!(
            sqlstate(&mut me, &format!("INSERT INTO {pool}.t VALUES (1)")).await,
            None
        );
        let owner: String = sqlx::query_scalar(
            "SELECT pg_get_userbyid(nspowner)::text FROM pg_namespace WHERE nspname = $1",
        )
        .bind(&pool)
        .fetch_one(&mut su)
        .await
        .unwrap();
        assert_eq!(owner, s.roles.login);
        me.close().await.unwrap();
        su.close().await.unwrap();
    })
    .await;
}

// The schema criteria: raw CREATE SCHEMA is refused for every name, ours and theirs.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn identity_role_cannot_create_schemas() {
    with_scratch(|s| async move {
        s.ensure().await;
        let mut me = s.as_login().await;
        let app = format!("identity_app_{}", hex32());
        for name in [
            "chelabase",
            "public",
            "x",
            "pg_temp_x",
            "identity_admin",
            "identity_members",
            app.as_str(),
        ] {
            let code = sqlstate(&mut me, &format!("CREATE SCHEMA \"{name}\"")).await;
            assert!(
                matches!(code.as_deref(), Some("42501" | "42939" | "42P06")),
                "CREATE SCHEMA {name}: {code:?}"
            );
        }
        // Nor through AUTHORIZATION, nor by taking over the admin schema.
        for sql in [
            "CREATE SCHEMA AUTHORIZATION CURRENT_USER",
            "CREATE FUNCTION identity_admin.cm_f() RETURNS int LANGUAGE sql AS 'SELECT 1'",
            "ALTER SCHEMA identity_admin RENAME TO cm_taken",
            "DROP SCHEMA identity_admin CASCADE",
        ] {
            assert!(sqlstate(&mut me, sql).await.is_some(), "{sql}");
        }
        // No schema was made.
        let mut su = s.superuser().await;
        let made: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_namespace WHERE nspowner = to_regrole($1)",
        )
        .bind(&s.roles.login)
        .fetch_one(&mut su)
        .await
        .unwrap();
        assert_eq!(made, 0);
        me.close().await.unwrap();
        su.close().await.unwrap();
    })
    .await;
}

// Both functions take only `^identity_(members|app_[0-9a-f]{32})$`; drop refuses any schema
// the login role doesn't own; only the login role may call them; they are pinned SECURITY
// DEFINER functions of a NOLOGIN, non-superuser owner; a rerun of the setup changes nothing.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn pool_schema_function_name_rule() {
    with_scratch(|s| async move {
        s.ensure().await;
        let mut me = s.as_login().await;
        let mut su = s.superuser().await;
        let call = |f: &'static str| AssertSqlSafe(format!("SELECT identity_admin.{f}($1)"));
        let hex = hex32();
        let bad: Vec<String> = vec![
            "public".into(),
            "chelabase".into(),
            "identity_admin".into(),
            "pg_catalog".into(),
            "information_schema".into(),
            "identity_members; DROP SCHEMA public CASCADE".into(),
            "identity_members\n".into(),
            " identity_members".into(),
            "identity_membersx".into(),
            "IDENTITY_MEMBERS".into(),
            "identity_app_".into(),
            format!("identity_app_{}", &hex[..31]),
            format!("identity_app_{hex}0"),
            format!("identity_app_{}", hex.to_uppercase()),
            format!("identity_app_{}g", &hex[..31]),
            format!("identity_app_{hex}\"; DROP SCHEMA public CASCADE; --"),
            String::new(),
        ];
        for f in ["create_pool_schema", "drop_pool_schema"] {
            for name in &bad {
                let r = sqlx::query(call(f)).bind(name).execute(&mut me).await;
                let code = match r {
                    Err(sqlx::Error::Database(d)) => d.code().map(|c| c.to_string()),
                    other => panic!("{f}({name:?}) was accepted: {other:?}"),
                };
                assert_eq!(code.as_deref(), Some("22023"), "{f}({name:?})");
            }
            let r = sqlx::query(call(f))
                .bind(None::<String>)
                .execute(&mut me)
                .await;
            assert!(r.is_err(), "{f}(NULL)");
        }
        // Nothing those calls named was touched.
        for schema in ["public", "chelabase", "identity_admin"] {
            let n: i64 =
                sqlx::query_scalar("SELECT count(*) FROM pg_namespace WHERE nspname = $1")
                    .bind(schema)
                    .fetch_one(&mut su)
                    .await
                    .unwrap();
            assert_eq!(n, 1, "{schema}");
        }

        // Good names: create (twice: idempotent), use, drop (twice: a missing one is fine).
        for name in ["identity_members".to_string(), format!("identity_app_{hex}")] {
            for _ in 0..2 {
                sqlx::query(call("create_pool_schema"))
                    .bind(&name)
                    .execute(&mut me)
                    .await
                    .unwrap();
            }
            assert_eq!(
                sqlstate(&mut me, &format!("CREATE TABLE {name}.t (id int)")).await,
                None
            );
            for _ in 0..2 {
                sqlx::query(call("drop_pool_schema"))
                    .bind(&name)
                    .execute(&mut me)
                    .await
                    .unwrap();
            }
            let n: i64 =
                sqlx::query_scalar("SELECT count(*) FROM pg_namespace WHERE nspname = $1")
                    .bind(&name)
                    .fetch_one(&mut su)
                    .await
                    .unwrap();
            assert_eq!(n, 0, "{name} dropped");
        }

        // A schema matching the pattern but someone else's: create refuses to adopt it and
        // drop refuses to drop it.
        let theirs = format!("identity_app_{}", hex32());
        sqlx::raw_sql(AssertSqlSafe(format!(
            "CREATE SCHEMA {theirs}; CREATE TABLE {theirs}.keep (id int)"
        )))
        .execute(&mut su)
        .await
        .unwrap();
        for f in ["create_pool_schema", "drop_pool_schema"] {
            let r = sqlx::query(call(f)).bind(&theirs).execute(&mut me).await;
            let code = match r {
                Err(sqlx::Error::Database(d)) => d.code().map(|c| c.to_string()),
                other => panic!("{f}({theirs}) was accepted: {other:?}"),
            };
            assert_eq!(code.as_deref(), Some("42501"), "{f}({theirs})");
        }
        let kept: i64 = sqlx::query_scalar(AssertSqlSafe(format!("SELECT count(*) FROM {theirs}.keep")))
            .fetch_one(&mut su)
            .await
            .unwrap();
        assert_eq!(kept, 0, "the table is still there");

        // A temporary table named like a catalog can't fool the owner check (pg_temp is last).
        assert_eq!(
            sqlstate(
                &mut me,
                &format!(
                    "CREATE TEMP TABLE pg_namespace (oid oid, nspname name, nspowner oid);
                     INSERT INTO pg_temp.pg_namespace
                       SELECT 1, '{theirs}', oid FROM pg_roles WHERE rolname = current_user"
                )
            )
            .await,
            None
        );
        assert!(
            sqlx::query(call("drop_pool_schema"))
                .bind(&theirs)
                .execute(&mut me)
                .await
                .is_err()
        );

        // Only the login role may call them.
        let other = format!("cm_other_{}", &hex32()[..12]);
        sqlx::raw_sql(AssertSqlSafe(format!(
            "CREATE ROLE {other} LOGIN PASSWORD '{ROLE_PASSWORD}'"
        )))
        .execute(&mut su)
        .await
        .unwrap();
        let opts = PgConnectOptions::from_str(&url_for(&s.db))
            .unwrap()
            .username(&other)
            .password(ROLE_PASSWORD);
        let mut stranger = PgConnection::connect_with(&opts).await.unwrap();
        for f in ["create_pool_schema", "drop_pool_schema"] {
            let r = sqlx::query(call(f))
                .bind("identity_members")
                .execute(&mut stranger)
                .await;
            let code = match r {
                Err(sqlx::Error::Database(d)) => d.code().map(|c| c.to_string()),
                other => panic!("{f} by another role was accepted: {other:?}"),
            };
            assert_eq!(code.as_deref(), Some("42501"), "{f} by another role");
        }
        stranger.close().await.unwrap();
        sqlx::raw_sql(AssertSqlSafe(format!("DROP ROLE {other}")))
            .execute(&mut su)
            .await
            .unwrap();

        // The functions' hygiene, read from the catalogs.
        let rows: Vec<FunctionRow> =
            sqlx::query_as(
                "SELECT p.proname::text, p.prosecdef, p.proconfig, r.rolname::text, r.rolsuper,
                        r.rolcanlogin, p.proacl::text[]
                   FROM pg_proc p JOIN pg_roles r ON r.oid = p.proowner
                  WHERE p.pronamespace = 'identity_admin'::regnamespace ORDER BY 1",
            )
            .fetch_all(&mut su)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2, "{rows:?}");
        for (name, secdef, config, owner, sup, login, acl) in rows {
            assert!(secdef, "{name}");
            assert_eq!(
                config,
                Some(vec!["search_path=pg_catalog, pg_temp".to_string()]),
                "{name}"
            );
            assert_eq!(owner, s.roles.owner, "{name}");
            assert!(!sup && !login, "{name}: its owner is NOLOGIN, not a superuser");
            let acl = acl.unwrap_or_default();
            assert!(
                acl.iter().any(|a| a.starts_with(&format!("{}=X/", s.roles.login))),
                "{name}: {acl:?}"
            );
            assert!(
                !acl.iter().any(|a| a.starts_with("=X/") || a.starts_with("=")),
                "{name}: PUBLIC may not execute: {acl:?}"
            );
        }
        let src: Vec<String> = sqlx::query_scalar(
            "SELECT prosrc FROM pg_proc WHERE pronamespace = 'identity_admin'::regnamespace",
        )
        .fetch_all(&mut su)
        .await
        .unwrap();
        assert!(src.iter().all(|b| b.contains(POOL_SCHEMA_PATTERN) && b.contains("%I")));
        let owner_can: (bool, bool) = sqlx::query_as(
            "SELECT has_database_privilege($1, current_database(), 'CREATE'),
                    (SELECT rolcanlogin FROM pg_roles WHERE rolname = $1)",
        )
        .bind(&s.roles.owner)
        .fetch_one(&mut su)
        .await
        .unwrap();
        assert_eq!(owner_can, (true, false));

        // A rerun of the setup keeps the password verifier and the functions as they are.
        let before = setup_snapshot(&mut su, &s.roles.login).await;
        s.ensure().await;
        assert_eq!(setup_snapshot(&mut su, &s.roles.login).await, before);
        me.close().await.unwrap();
        su.close().await.unwrap();
    })
    .await;
}

// AC 1: the stack's own members pool, through the bootstrap's identity step, twice: the schema
// exists, is the identity role's, and the second run changes nothing. Needs the stack's identity
// service and the runner's identity secrets.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres and identity service"]
async fn members_pool_idempotent() {
    use chelabase_init::bootstrap::identity::{IdentityConfig, run};
    use chelabase_init::bootstrap::retry::{Backoff, Waiter};
    let keys = std::env::var("CHELABASE_KEYS_DIR").expect("CHELABASE_KEYS_DIR (the runner's)");
    let read = |f: &str| {
        std::fs::read_to_string(format!("{keys}/{f}"))
            .unwrap_or_else(|e| panic!("{keys}/{f}: {e}"))
    };
    let base = std::env::var("CHELABASE_IDENTITY_URL")
        .unwrap_or_else(|_| "http://identity:8080".into());
    let cfg = IdentityConfig::new(
        &base,
        &read("tokens/worker-identity.token"),
        &read("identity/db.password"),
    );
    let w = || Waiter::new(std::time::Duration::from_secs(120), Backoff::default());
    let state = || async {
        let mut su = PgConnection::connect(&url()).await.unwrap();
        let row: (i64, String, Vec<String>, String) = sqlx::query_as(
            "SELECT n.oid::bigint, pg_get_userbyid(n.nspowner)::text,
                    (SELECT coalesce(array_agg(c.oid::text || ':' || c.relname ORDER BY c.oid), '{}')
                       FROM pg_class c WHERE c.relnamespace = n.oid),
                    (SELECT rolpassword FROM pg_authid WHERE rolname = 'chelabase_identity')
               FROM pg_namespace n WHERE n.nspname = 'identity_members'",
        )
        .fetch_one(&mut su)
        .await
        .expect("identity_members exists");
        su.close().await.unwrap();
        row
    };
    run(&w(), &url(), &cfg).await.unwrap();
    let first = state().await;
    assert_eq!(first.1, "chelabase_identity");
    assert!(
        first.2.iter().any(|t| t.ends_with(":user")) && first.2.iter().any(|t| t.ends_with(":session")),
        "Better Auth's tables: {:?}",
        first.2
    );
    run(&w(), &url(), &cfg).await.unwrap();
    assert_eq!(state().await, first, "a second run changes nothing");
}

// N3 (audit): the database name is quoted as an identifier, so a name with a `"` works.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn identity_roles_quote_the_database_name() {
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url())
        .await
        .expect("metadata Postgres reachable");
    let tag = hex32();
    let db = format!("cm_ident_{tag}\"q");
    let quoted = format!("\"{}\"", db.replace('"', "\"\""));
    sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE {quoted}")))
        .execute(&admin)
        .await
        .unwrap();
    let roles = IdentityRoles {
        login: format!("cm_ident_{}", &tag[..12]),
        owner: format!("cm_ident_{}_admin", &tag[..12]),
    };
    let opts = PgConnectOptions::from_str(&url()).unwrap().database(&db);
    let (r, granted) = {
        let mut su = PgConnection::connect_with(&opts).await.unwrap();
        let r = ensure_identity_roles(&mut su, &roles, ROLE_PASSWORD).await;
        let granted: Option<bool> = sqlx::query_scalar(
            "SELECT has_database_privilege(r.rolname, current_database(), 'CREATE')
               FROM pg_roles r WHERE r.rolname = $1",
        )
        .bind(&roles.owner)
        .fetch_optional(&mut su)
        .await
        .unwrap();
        su.close().await.unwrap();
        (r, granted)
    };
    sqlx::raw_sql(AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {quoted} WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .unwrap();
    for role in [&roles.owner, &roles.login] {
        sqlx::raw_sql(AssertSqlSafe(format!("DROP ROLE IF EXISTS {role}")))
            .execute(&admin)
            .await
            .unwrap();
    }
    r.unwrap();
    assert_eq!(granted, Some(true));
}

/// `ensure_identity_roles`' advisory lock key ("cb_ident" as ASCII).
const SETUP_LOCK_KEY: i64 = 0x6362_5f69_6465_6e74;

// S2 (audit): a session holding the setup's advisory lock can't hang it. The setup gives up on
// the lock after its lock_timeout (55P03), and the bootstrap's step waits for it within its
// deadline, then succeeds once the lock is free.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn identity_roles_lock_wait_is_bounded() {
    with_scratch(|s| async move {
        let mut holder = s.superuser().await;
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(SETUP_LOCK_KEY)
            .execute(&mut holder)
            .await
            .unwrap();
        let mut su = s.superuser().await;
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            ensure_identity_roles(&mut su, &s.roles, ROLE_PASSWORD),
        )
        .await
        .expect("the setup gives up on a held lock instead of waiting forever");
        let err = r.expect_err("the lock is held");
        let code = err
            .downcast_ref::<sqlx::Error>()
            .and_then(|e| e.as_database_error())
            .and_then(|d| d.code())
            .map(|c| c.to_string());
        assert_eq!(code.as_deref(), Some("55P03"), "{err:#}");
        su.close().await.unwrap();
        holder.close().await.unwrap();
    })
    .await;
}

// S2 (audit): the bootstrap's identity step treats the held lock as a wait inside its deadline
// (an error naming the lock when the deadline passes), and finishes once the lock is released.
#[tokio::test]
#[ignore = "needs the workbench stack's metadata Postgres"]
async fn bootstrap_identity_step_waits_out_the_lock() {
    use chelabase_init::bootstrap::identity::ensure_roles;
    use chelabase_init::bootstrap::retry::{Backoff, Waiter};
    with_scratch(|s| async move {
        let scratch_url = url_for(&s.db);
        let mut holder = s.superuser().await;
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(SETUP_LOCK_KEY)
            .execute(&mut holder)
            .await
            .unwrap();
        let w = Waiter::new(std::time::Duration::from_secs(3), Backoff::default());
        let err = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            ensure_roles(&w, &scratch_url, &s.roles, ROLE_PASSWORD),
        )
        .await
        .expect("bounded by the deadline")
        .expect_err("the lock is held past the deadline");
        let msg = format!("{err:#}");
        assert!(msg.contains("timed out") && msg.contains("lock"), "{msg}");

        let w = Waiter::new(std::time::Duration::from_secs(60), Backoff::default());
        let release = async {
            // Released while the step waits for it.
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            sqlx::query("SELECT pg_advisory_unlock($1)")
                .bind(SETUP_LOCK_KEY)
                .execute(&mut holder)
                .await
                .unwrap();
        };
        let (r, ()) = tokio::join!(
            ensure_roles(&w, &scratch_url, &s.roles, ROLE_PASSWORD),
            release
        );
        r.unwrap();
        let made: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
                .bind(&s.roles.login)
                .fetch_one(&mut holder)
                .await
                .unwrap();
        assert!(made);
        holder.close().await.unwrap();
    })
    .await;
}
