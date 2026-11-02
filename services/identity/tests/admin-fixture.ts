/**
 * m5's identity roles and `identity_admin` functions, as `crates/chelabase-init/src/roles.rs`
 * makes them: run it as a superuser. The service then connects as the login role
 * (`identityLoginUrl`), which owns its pool schemas and nothing else, has no CREATE on the
 * database and reaches DDL only through the two SECURITY DEFINER functions, owned by the
 * NOLOGIN admin role with `search_path` pinned to `pg_catalog, pg_temp`.
 *
 * Idempotent: every test file runs it (one file at a time, see vitest.config.ts).
 */
export const IDENTITY_LOGIN = "chelabase_identity";
const IDENTITY_OWNER = "chelabase_identity_admin";
/** Test-only password for the login role in the disposable test database. */
const IDENTITY_PASSWORD = "identity-test-password";
const PATTERN = "^identity_(members|app_[0-9a-f]{32})$";
const NOT = "NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS";

export const ADMIN_FIXTURE = `
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = '${IDENTITY_LOGIN}') THEN
    CREATE ROLE ${IDENTITY_LOGIN} LOGIN NOINHERIT ${NOT};
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles WHERE rolname = '${IDENTITY_OWNER}') THEN
    CREATE ROLE ${IDENTITY_OWNER} NOLOGIN ${NOT};
  END IF;
  EXECUTE pg_catalog.format('GRANT CREATE ON DATABASE %I TO ${IDENTITY_OWNER}', pg_catalog.current_database());
  EXECUTE pg_catalog.format('REVOKE CREATE ON DATABASE %I FROM ${IDENTITY_LOGIN}', pg_catalog.current_database());
END $$;
ALTER ROLE ${IDENTITY_LOGIN} LOGIN NOINHERIT ${NOT} PASSWORD '${IDENTITY_PASSWORD}';
GRANT ${IDENTITY_LOGIN} TO ${IDENTITY_OWNER} WITH INHERIT TRUE, SET TRUE;
REVOKE ALL ON SCHEMA public FROM ${IDENTITY_LOGIN}, ${IDENTITY_OWNER} CASCADE;
CREATE SCHEMA IF NOT EXISTS identity_admin;
REVOKE ALL ON SCHEMA identity_admin FROM PUBLIC, ${IDENTITY_OWNER};
GRANT USAGE ON SCHEMA identity_admin TO ${IDENTITY_LOGIN};

CREATE OR REPLACE FUNCTION identity_admin.create_pool_schema(name text) RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $fn$
DECLARE
  login oid := pg_catalog.to_regrole('${IDENTITY_LOGIN}');
  owner_oid oid;
BEGIN
  IF name IS NULL OR name OPERATOR(pg_catalog.!~) '${PATTERN}' THEN
    RAISE EXCEPTION 'invalid pool schema name' USING ERRCODE = '22023';
  END IF;
  IF login IS NULL THEN
    RAISE EXCEPTION 'role ${IDENTITY_LOGIN} does not exist' USING ERRCODE = '42704';
  END IF;
  EXECUTE pg_catalog.format('CREATE SCHEMA IF NOT EXISTS %I AUTHORIZATION %I', name, '${IDENTITY_LOGIN}');
  SELECT n.nspowner INTO owner_oid FROM pg_catalog.pg_namespace n WHERE n.nspname = name;
  IF owner_oid IS DISTINCT FROM login THEN
    RAISE EXCEPTION 'schema % exists and is not owned by ${IDENTITY_LOGIN}', name USING ERRCODE = '42501';
  END IF;
END
$fn$;

CREATE OR REPLACE FUNCTION identity_admin.drop_pool_schema(name text) RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $fn$
DECLARE
  login oid := pg_catalog.to_regrole('${IDENTITY_LOGIN}');
  owner_oid oid;
BEGIN
  IF name IS NULL OR name OPERATOR(pg_catalog.!~) '${PATTERN}' THEN
    RAISE EXCEPTION 'invalid pool schema name' USING ERRCODE = '22023';
  END IF;
  SELECT n.nspowner INTO owner_oid FROM pg_catalog.pg_namespace n WHERE n.nspname = name;
  IF NOT FOUND THEN
    RETURN;
  END IF;
  IF login IS NULL OR owner_oid IS DISTINCT FROM login THEN
    RAISE EXCEPTION 'refusing to drop schema %: not owned by ${IDENTITY_LOGIN}', name USING ERRCODE = '42501';
  END IF;
  EXECUTE pg_catalog.format('DROP SCHEMA %I CASCADE', name);
END
$fn$;

ALTER FUNCTION identity_admin.create_pool_schema(text) OWNER TO ${IDENTITY_OWNER};
ALTER FUNCTION identity_admin.drop_pool_schema(text) OWNER TO ${IDENTITY_OWNER};
REVOKE ALL ON FUNCTION identity_admin.create_pool_schema(text), identity_admin.drop_pool_schema(text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION identity_admin.create_pool_schema(text), identity_admin.drop_pool_schema(text) TO ${IDENTITY_LOGIN};
`;

/** `url` (a superuser's) with the identity login role's credentials instead. */
export function identityLoginUrl(url: string): string {
  const u = new URL(url);
  u.username = IDENTITY_LOGIN;
  u.password = IDENTITY_PASSWORD;
  return u.toString();
}
