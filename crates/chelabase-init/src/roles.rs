//! The identity service's Postgres roles in the metadata database (auth-replacement m5, #118).
//!
//! - **`chelabase_identity`** ([`IdentityRoles::login`]): the service's own LOGIN role. Not a
//!   superuser, no `CREATEDB`/`CREATEROLE`/`REPLICATION`/`BYPASSRLS`, `NOINHERIT`, a member of no
//!   role, no `CREATE` on the database and no privilege on `public` or `chelabase`, not even
//!   `USAGE` (see [`ensure_identity_roles`] on `PUBLIC`'s), so it can't name a Chelabase type or
//!   function either. At most [`CONNECTION_LIMIT`] connections. It owns its pool schemas
//!   (`identity_members`, `identity_app_<32 hex>`) and nothing else.
//! - **`chelabase_identity_admin`** ([`IdentityRoles::owner`]): `NOLOGIN`, not a superuser, with
//!   `CREATE` on the database and membership in the login role (to create schemas it owns and
//!   drop them). It owns the two functions below and nothing else; nobody can log in as it.
//! - **`identity_admin.create_pool_schema(name)`, `identity_admin.drop_pool_schema(name)`**:
//!   `SECURITY DEFINER`, owned by the admin role, `search_path` pinned to `pg_catalog, pg_temp`
//!   (`pg_temp` last, so a caller's temporary table can never shadow a catalog), every catalog
//!   qualified. Each accepts only `^identity_(members|app_[0-9a-f]{32})$` and quotes it with
//!   `%I`. Create makes the schema `AUTHORIZATION chelabase_identity`; an existing one must
//!   already be the login role's. Drop refuses a schema the login role doesn't own. `EXECUTE` is
//!   revoked from `PUBLIC` and granted to the login role only. The schema `identity_admin` is the
//!   bootstrap's (the superuser's); the login role has `USAGE` on it, nothing more.
//!
//! [`ensure_identity_roles`] makes all of it and corrects drift (attributes, memberships,
//! grants, the functions' bodies and owners) on every run, under one transaction-scoped advisory
//! lock, so concurrent runs serialise. The lock is open to every role that can connect, so the
//! wait for it is bounded by [`LOCK_TIMEOUT`]: a session holding it fails the run with 55P03
//! ([`is_lock_timeout`]), which the bootstrap retries within its deadline. The password is sent as a SCRAM verifier, never as text,
//! and only when the stored verifier doesn't match the file's password.

use anyhow::{Context, Result, bail};
use sqlx::{AssertSqlSafe, PgConnection};

/// The pattern both functions enforce (the identity service checks the same one first).
pub const POOL_SCHEMA_PATTERN: &str = "^identity_(members|app_[0-9a-f]{32})$";
/// The functions' schema.
pub const ADMIN_SCHEMA: &str = "identity_admin";
/// `pg_advisory_xact_lock` key of [`ensure_identity_roles`] ("cb_ident" as ASCII).
const LOCK_KEY: i64 = 0x6362_5f69_6465_6e74;
/// The longest [`ensure_identity_roles`] waits for its lock (or any other lock), as a
/// `lock_timeout`. Past it the run fails with 55P03 and the bootstrap tries again.
pub const LOCK_TIMEOUT: &str = "5s";
/// The login role's `CONNECTION LIMIT`, so a compromised service can't take every connection
/// of the metadata server (`max_connections` 100 by default). The service's shared pool holds
/// at most 10, and each pool creation in flight opens one more for its migration (the service
/// doesn't bound how many run at once: its concurrency test runs 24), so this leaves room for
/// a burst of creations, or two instances side by side during a restart, and still keeps half
/// the server for everyone else.
pub const CONNECTION_LIMIT: u32 = 50;

/// Whether `err` is a lock wait that hit [`LOCK_TIMEOUT`] (SQLSTATE 55P03, `lock_not_available`):
/// someone else holds the lock, so the run may be tried again.
pub fn is_lock_timeout(err: &anyhow::Error) -> bool {
    err.chain().any(|e| {
        e.downcast_ref::<sqlx::Error>()
            .and_then(|e| e.as_database_error())
            .and_then(|d| d.code())
            .is_some_and(|c| c == "55P03")
    })
}

/// The two roles' names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityRoles {
    pub login: String,
    pub owner: String,
}

impl Default for IdentityRoles {
    fn default() -> Self {
        Self {
            login: "chelabase_identity".into(),
            owner: "chelabase_identity_admin".into(),
        }
    }
}

/// A role name these statements may embed: lowercase letters, digits and `_`, starting with a
/// letter or `_`, at most 63 bytes. It goes into SQL text (DDL takes no parameters), so nothing
/// else passes.
pub fn valid_role_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && (b[0].is_ascii_lowercase() || b[0] == b'_')
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
        && !name.starts_with("pg_")
}

impl IdentityRoles {
    fn check(&self) -> Result<()> {
        for name in [&self.login, &self.owner] {
            if !valid_role_name(name) {
                bail!("{name:?} is not a role name chelabase-init uses");
            }
        }
        if self.login == self.owner {
            bail!("the identity login and admin roles must differ");
        }
        Ok(())
    }

    /// The two functions' definitions (`CREATE OR REPLACE`), for these roles.
    pub fn function_sql(&self) -> [String; 2] {
        let login = &self.login;
        let schema = ADMIN_SCHEMA;
        let pattern = POOL_SCHEMA_PATTERN;
        let create = format!(
            r#"CREATE OR REPLACE FUNCTION {schema}.create_pool_schema(name text) RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $fn$
DECLARE
  login oid := pg_catalog.to_regrole('{login}');
  owner_oid oid;
BEGIN
  IF name IS NULL OR name OPERATOR(pg_catalog.!~) '{pattern}' THEN
    RAISE EXCEPTION 'invalid pool schema name' USING ERRCODE = '22023';
  END IF;
  IF login IS NULL THEN
    RAISE EXCEPTION 'role {login} does not exist' USING ERRCODE = '42704';
  END IF;
  EXECUTE pg_catalog.format('CREATE SCHEMA IF NOT EXISTS %I AUTHORIZATION %I', name, '{login}');
  SELECT n.nspowner INTO owner_oid FROM pg_catalog.pg_namespace n WHERE n.nspname = name;
  IF owner_oid IS DISTINCT FROM login THEN
    RAISE EXCEPTION 'schema % exists and is not owned by {login}', name USING ERRCODE = '42501';
  END IF;
END
$fn$"#
        );
        let drop = format!(
            r#"CREATE OR REPLACE FUNCTION {schema}.drop_pool_schema(name text) RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $fn$
DECLARE
  login oid := pg_catalog.to_regrole('{login}');
  owner_oid oid;
BEGIN
  IF name IS NULL OR name OPERATOR(pg_catalog.!~) '{pattern}' THEN
    RAISE EXCEPTION 'invalid pool schema name' USING ERRCODE = '22023';
  END IF;
  SELECT n.nspowner INTO owner_oid FROM pg_catalog.pg_namespace n WHERE n.nspname = name;
  IF NOT FOUND THEN
    RETURN;
  END IF;
  IF login IS NULL OR owner_oid IS DISTINCT FROM login THEN
    RAISE EXCEPTION 'refusing to drop schema %: not owned by {login}', name USING ERRCODE = '42501';
  END IF;
  EXECUTE pg_catalog.format('DROP SCHEMA %I CASCADE', name);
END
$fn$"#
        );
        [create, drop]
    }
}

async fn exec(conn: &mut PgConnection, sql: String) -> Result<()> {
    sqlx::raw_sql(AssertSqlSafe(sql.clone()))
        .execute(&mut *conn)
        .await
        .with_context(|| {
            // The statement names roles and grants, never the password (sent as a verifier,
            // and only by `set_password`, whose error is its own).
            sql.lines().next().unwrap_or_default().to_string()
        })?;
    Ok(())
}

async fn role_exists(conn: &mut PgConnection, name: &str) -> Result<bool> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = $1)",
    )
    .bind(name)
    .fetch_one(&mut *conn)
    .await?)
}

/// The login role's password as a SCRAM verifier, when the stored one doesn't verify `password`
/// (the statement holds the verifier only, so no log line ever holds the password).
async fn set_password(conn: &mut PgConnection, login: &str, password: &str) -> Result<()> {
    let stored: Option<String> =
        sqlx::query_scalar("SELECT rolpassword FROM pg_catalog.pg_authid WHERE rolname = $1")
            .bind(login)
            .fetch_one(&mut *conn)
            .await
            .context("reading the identity role's password verifier (needs a superuser)")?;
    if stored
        .as_deref()
        .is_some_and(|s| chelabase_neon::scram::verifies(s, password))
    {
        return Ok(());
    }
    let verifier = chelabase_neon::scram::verifier(password);
    if !chelabase_neon::scram::is_verifier(&verifier) {
        bail!("the identity role's password verifier is malformed");
    }
    sqlx::raw_sql(AssertSqlSafe(format!(
        "ALTER ROLE {login} PASSWORD '{verifier}'"
    )))
    .execute(&mut *conn)
    .await
    .map_err(|e| anyhow::anyhow!("setting the identity role's password: {e}"))?;
    Ok(())
}

/// Makes or corrects the roles, grants and functions (see the module doc) in the database `conn`
/// is connected to, as a superuser. `password` is the login role's (`identity/db.password`).
pub async fn ensure_identity_roles(
    conn: &mut PgConnection,
    roles: &IdentityRoles,
    password: &str,
) -> Result<()> {
    roles.check()?;
    if password.trim().is_empty() {
        bail!("the identity role's password is blank");
    }
    let (login, owner) = (roles.login.as_str(), roles.owner.as_str());
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    // Bounded: any role that can connect may hold this key.
    sqlx::raw_sql(AssertSqlSafe(format!(
        "SET LOCAL lock_timeout = '{LOCK_TIMEOUT}'"
    )))
    .execute(&mut *tx)
    .await?;
    sqlx::query("SELECT pg_catalog.pg_advisory_xact_lock($1)")
        .bind(LOCK_KEY)
        .execute(&mut *tx)
        .await?;
    let conn: &mut PgConnection = &mut tx;

    const NOT: &str = "NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS";
    if !role_exists(conn, login).await? {
        exec(
            conn,
            format!(
                "CREATE ROLE {login} LOGIN NOINHERIT {NOT} CONNECTION LIMIT {CONNECTION_LIMIT}"
            ),
        )
        .await?;
    }
    exec(
        conn,
        format!("ALTER ROLE {login} LOGIN NOINHERIT {NOT} CONNECTION LIMIT {CONNECTION_LIMIT}"),
    )
    .await?;
    if !role_exists(conn, owner).await? {
        exec(conn, format!("CREATE ROLE {owner} NOLOGIN {NOT}")).await?;
    }
    exec(conn, format!("ALTER ROLE {owner} NOLOGIN {NOT}")).await?;
    // Drift a superuser could have left: no role-level settings (a search_path or a
    // `session_replication_role` would follow the role into every session).
    for role in [login, owner] {
        exec(conn, format!("ALTER ROLE {role} RESET ALL")).await?;
    }

    // The login role is a member of nothing; the admin role is a member of exactly the login
    // role (to create schemas it owns and drop them). Any other membership either way is
    // refused rather than silently revoked: someone granted it on purpose.
    let memberships: Vec<(String, String)> = sqlx::query_as(
        "SELECT r.rolname::text, m.rolname::text FROM pg_catalog.pg_auth_members a
           JOIN pg_catalog.pg_roles r ON r.oid = a.roleid
           JOIN pg_catalog.pg_roles m ON m.oid = a.member
          WHERE m.rolname = ANY($1) OR r.rolname = ANY($1)",
    )
    .bind([login, owner])
    .fetch_all(&mut *conn)
    .await?;
    let unexpected: Vec<String> = memberships
        .iter()
        .filter(|(role, member)| !(role == login && member == owner))
        .map(|(role, member)| format!("{member} is a member of {role}"))
        .collect();
    if !unexpected.is_empty() {
        bail!(
            "the identity roles have memberships chelabase-init doesn't make ({}): revoke them",
            unexpected.join(", ")
        );
    }
    exec(
        conn,
        format!("GRANT {login} TO {owner} WITH INHERIT TRUE, SET TRUE"),
    )
    .await?;
    set_password(conn, login, password).await?;

    // Database: the admin role creates schemas; the login role only connects. The name is
    // quoted by the server (`%I`).
    let db: String =
        sqlx::query_scalar("SELECT pg_catalog.quote_ident(pg_catalog.current_database())")
            .fetch_one(&mut *conn)
            .await?;
    exec(conn, format!("GRANT CREATE ON DATABASE {db} TO {owner}")).await?;
    exec(conn, format!("REVOKE CREATE ON DATABASE {db} FROM {login}")).await?;
    // Schema `public`: PUBLIC's default USAGE would let the login role name Chelabase's types
    // and functions from its own schemas (a column of type `public.tenants` pins that table
    // against Chelabase's own DDL). A PUBLIC grant can't be withheld from one role, so it goes
    // for everyone: the metadata database's only other role is its owner, `chelabase` (the
    // superuser every app and the migrations connect as), which has `public` through
    // `pg_database_owner` anyway. A future role that needs `public` gets its own GRANT.
    // No direct grant on Chelabase's schemas or what is in them either (the metadata tables
    // grant nothing to PUBLIC, and `chelabase` was never PUBLIC's).
    for schema in ["public", "chelabase"] {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname = $1)",
        )
        .bind(schema)
        .fetch_one(&mut *conn)
        .await?;
        if exists {
            if schema == "public" {
                exec(conn, "REVOKE ALL ON SCHEMA public FROM PUBLIC".to_string()).await?;
            }
            for role in [login, owner] {
                exec(
                    conn,
                    format!("REVOKE ALL ON SCHEMA {schema} FROM {role} CASCADE"),
                )
                .await?;
                exec(
                    conn,
                    format!("REVOKE ALL ON ALL TABLES IN SCHEMA {schema} FROM {role} CASCADE"),
                )
                .await?;
            }
        }
    }

    // The functions' schema: the superuser's, USAGE for the login role only.
    exec(conn, format!("CREATE SCHEMA IF NOT EXISTS {ADMIN_SCHEMA}")).await?;
    let schema_owner: String = sqlx::query_scalar(
        "SELECT pg_catalog.pg_get_userbyid(nspowner)::text FROM pg_catalog.pg_namespace
          WHERE nspname = $1",
    )
    .bind(ADMIN_SCHEMA)
    .fetch_one(&mut *conn)
    .await?;
    let me: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(&mut *conn)
        .await?;
    if schema_owner != me {
        exec(
            conn,
            format!("ALTER SCHEMA {ADMIN_SCHEMA} OWNER TO CURRENT_USER"),
        )
        .await?;
    }
    exec(
        conn,
        format!("REVOKE ALL ON SCHEMA {ADMIN_SCHEMA} FROM PUBLIC, {owner}"),
    )
    .await?;
    exec(
        conn,
        format!("GRANT USAGE ON SCHEMA {ADMIN_SCHEMA} TO {login}"),
    )
    .await?;
    // Nothing else may live there: a stray function would be callable by the login role.
    let stray: Vec<String> = sqlx::query_scalar(
        "SELECT p.oid::regprocedure::text FROM pg_catalog.pg_proc p
          WHERE p.pronamespace = $1::regnamespace
            AND p.proname NOT IN ('create_pool_schema', 'drop_pool_schema')
         UNION ALL
         SELECT c.oid::regclass::text FROM pg_catalog.pg_class c
          WHERE c.relnamespace = $1::regnamespace",
    )
    .bind(ADMIN_SCHEMA)
    .fetch_all(&mut *conn)
    .await?;
    if !stray.is_empty() {
        bail!(
            "schema {ADMIN_SCHEMA} holds objects chelabase-init doesn't make: {}",
            stray.join(", ")
        );
    }
    for (sql, name) in roles
        .function_sql()
        .into_iter()
        .zip(["create_pool_schema", "drop_pool_schema"])
    {
        // An old overload (another signature) would keep its own grants: refuse it.
        let overloads: Vec<String> = sqlx::query_scalar(
            "SELECT p.oid::regprocedure::text FROM pg_catalog.pg_proc p
              WHERE p.pronamespace = $1::regnamespace AND p.proname = $2
                AND (p.pronargs <> 1 OR p.proargtypes[0] <> 'pg_catalog.text'::pg_catalog.regtype)",
        )
        .bind(ADMIN_SCHEMA)
        .bind(name)
        .fetch_all(&mut *conn)
        .await?;
        if !overloads.is_empty() {
            bail!(
                "unexpected overloads of {ADMIN_SCHEMA}.{name}: {}",
                overloads.join(", ")
            );
        }
        exec(conn, sql).await?;
        let f = format!("{ADMIN_SCHEMA}.{name}(text)");
        exec(conn, format!("ALTER FUNCTION {f} OWNER TO {owner}")).await?;
        exec(conn, format!("REVOKE ALL ON FUNCTION {f} FROM PUBLIC")).await?;
        exec(conn, format!("GRANT EXECUTE ON FUNCTION {f} TO {login}")).await?;
    }
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_names_are_plain_identifiers() {
        for ok in ["chelabase_identity", "chelabase_identity_admin", "_x", "a1"] {
            assert!(valid_role_name(ok), "{ok}");
        }
        for bad in [
            "",
            "Chelabase",
            "1abc",
            "a-b",
            "a b",
            "a\"b",
            "a;drop",
            "pg_signal_backend",
            &"a".repeat(64),
        ] {
            assert!(!valid_role_name(bad), "{bad:?}");
        }
        let same = IdentityRoles {
            login: "x".into(),
            owner: "x".into(),
        };
        assert!(same.check().is_err());
        assert!(IdentityRoles::default().check().is_ok());
    }

    #[test]
    fn functions_are_pinned_and_quoted() {
        for sql in IdentityRoles::default().function_sql() {
            assert!(sql.contains("SECURITY DEFINER"), "{sql}");
            assert!(
                sql.contains("SET search_path = pg_catalog, pg_temp"),
                "{sql}"
            );
            assert!(sql.contains(POOL_SCHEMA_PATTERN), "{sql}");
            assert!(sql.contains("%I"), "{sql}");
            assert!(!sql.contains("%s"), "{sql}");
            // Every catalog reference is qualified.
            assert!(!sql.contains(" pg_namespace"), "{sql}");
        }
    }
}
