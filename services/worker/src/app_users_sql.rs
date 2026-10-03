//! The app users machinery inside a branch's auth database (the users table, its guard and
//! protect triggers, the `cheladb_auth_sync` login and the sync functions), and the calls the
//! sync makes as that login.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::data_api::{BOOTSTRAP_LOCK_KEY, LOCK_TIMEOUT_SECS, quote_ident, quote_literal};

/// The login the sync connects as (`SET LOCAL ROLE` from the admin connection, or directly
/// with its sealed password): the only role the guard trigger lets write the users table.
pub const USERS_SYNC_ROLE: &str = "cheladb_auth_sync";

/// The users table's columns Cheladb manages; every other column is the app's.
pub const MANAGED_COLUMNS: [&str; 5] =
    ["id", "email", "email_verified", "created_at", "deleted_at"];

/// `^[a-z_][a-z0-9_]{0,62}$`: a schema or table name app users accepts.
pub fn valid_identifier(s: &str) -> bool {
    let bytes = s.as_bytes();
    match bytes.split_first() {
        Some((first, rest)) => {
            rest.len() <= 62
                && (first.is_ascii_lowercase() || *first == b'_')
                && rest
                    .iter()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
        }
        None => false,
    }
}

/// The idempotent script run as the superuser in the auth database when app users are enabled
/// (and on every repair): the `cheladb_auth_sync` login with `sync_verifier` (the password's
/// SCRAM-SHA-256 verifier, so the password itself never reaches the compute), the claim
/// helpers, the users table `schema.table` (created RLS-locked with two own-row policies
/// only when missing; an existing table is adopted as it is), its guard triggers, the sync
/// functions, and the protect event triggers. One transaction under [`BOOTSTRAP_LOCK_KEY`].
///
/// Sign-up data is opt-in per column (P34): every run takes a table-level `INSERT` away from
/// `anon` and `authenticated` (row inserts are the guard's to refuse anyway), so
/// `app_users_upsert` passes only the columns granted with `GRANT INSERT (col) … TO
/// authenticated`. The first line is a marker comment, so the script is told apart from the
/// data API bootstrap.
///
/// The protect event triggers are dropped right after the lock and re-created last, so a
/// broken state (the table dropped with event triggers off) can't block the run that
/// repairs it. Every function is owned by the superuser running the script (Neon refuses a
/// superuser running a function a non-superuser owns).
pub fn users_bootstrap_sql(
    schema: &str,
    table: &str,
    sync_verifier: &str,
    owner_role: &str,
) -> String {
    let s = quote_ident(schema);
    let t = quote_ident(table);
    let qualified = quote_literal(&format!("{s}.{t}"));
    let schema_lit = quote_literal(schema);
    let table_lit = quote_literal(table);
    let password = quote_literal(sync_verifier);
    let owner = quote_ident(owner_role);
    let claim_helpers = claim_helpers_body();
    format!(
        r#"-- cheladb:users-bootstrap
BEGIN;
SET search_path = pg_catalog, pg_temp;
SET LOCAL lock_timeout = '{LOCK_TIMEOUT_SECS}s';
SELECT pg_catalog.pg_advisory_xact_lock({BOOTSTRAP_LOCK_KEY});
DROP EVENT TRIGGER IF EXISTS cheladb_users_protect_drop;
DROP EVENT TRIGGER IF EXISTS cheladb_users_protect_ddl;
DO $$ BEGIN
  IF pg_catalog.to_regnamespace('cheladb') IS NULL THEN
    RAISE EXCEPTION 'cheladb_data_api_missing: the data API is not enabled in this database';
  END IF;
  IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'cheladb_auth_sync') THEN
    CREATE ROLE cheladb_auth_sync LOGIN NOINHERIT BYPASSRLS;
  END IF;
END $$;
ALTER ROLE cheladb_auth_sync LOGIN NOINHERIT BYPASSRLS NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION CONNECTION LIMIT 10 PASSWORD {password};
ALTER ROLE cheladb_auth_sync RESET ALL;
{claim_helpers}CREATE SCHEMA IF NOT EXISTS {s} AUTHORIZATION {owner};
DO $$ BEGIN
  IF pg_catalog.to_regclass({qualified}) IS NULL THEN
    CREATE TABLE {s}.{t} (
      id uuid PRIMARY KEY,
      email text NOT NULL,
      email_verified boolean NOT NULL DEFAULT false,
      created_at timestamptz NOT NULL DEFAULT pg_catalog.now(),
      deleted_at timestamptz
    );
    ALTER TABLE {s}.{t} OWNER TO {owner};
    REVOKE ALL ON {s}.{t} FROM anon;
    GRANT SELECT, UPDATE, DELETE ON {s}.{t} TO authenticated;
    ALTER TABLE {s}.{t} ENABLE ROW LEVEL SECURITY;
    CREATE POLICY cheladb_users_select_own ON {s}.{t} FOR SELECT TO authenticated
      USING (id OPERATOR(pg_catalog.=) cheladb.user_id());
    CREATE POLICY cheladb_users_update_own ON {s}.{t} FOR UPDATE TO authenticated
      USING (id OPERATOR(pg_catalog.=) cheladb.user_id()) WITH CHECK (id OPERATOR(pg_catalog.=) cheladb.user_id());
  END IF;
END $$;
CREATE UNIQUE INDEX IF NOT EXISTS cheladb_users_email_key ON {s}.{t} (pg_catalog.lower(email)) WHERE deleted_at IS NULL;
DO $$ DECLARE r pg_catalog.name; BEGIN
  FOR r IN SELECT g.rolname FROM pg_catalog.pg_class c, pg_catalog.aclexplode(c.relacl) a
      JOIN pg_catalog.pg_roles g ON g.oid OPERATOR(pg_catalog.=) a.grantee
    WHERE c.oid OPERATOR(pg_catalog.=) {qualified}::pg_catalog.regclass
      AND a.privilege_type OPERATOR(pg_catalog.=) 'INSERT'
      AND g.rolname OPERATOR(pg_catalog.=) ANY (ARRAY['anon', 'authenticated']::pg_catalog.name[])
  LOOP
    EXECUTE pg_catalog.format('REVOKE INSERT ON %s FROM %I', {qualified}::pg_catalog.regclass, r);
  END LOOP;
END $$;
GRANT USAGE ON SCHEMA {s} TO cheladb_auth_sync;
GRANT SELECT, INSERT, UPDATE, DELETE ON {s}.{t} TO cheladb_auth_sync;
CREATE TABLE IF NOT EXISTS cheladb.app_users_table (one boolean PRIMARY KEY DEFAULT true CHECK (one),
  schema_name text NOT NULL, table_name text NOT NULL, table_oid oid NOT NULL);
ALTER TABLE cheladb.app_users_table OWNER TO cheladb_admin;
REVOKE ALL ON cheladb.app_users_table FROM PUBLIC;
GRANT USAGE ON SCHEMA cheladb TO cheladb_auth_sync;
GRANT SELECT ON cheladb.app_users_table TO authenticator, anon, authenticated, cheladb_auth_sync, {owner};
INSERT INTO cheladb.app_users_table (schema_name, table_name, table_oid)
  VALUES ({schema_lit}, {table_lit}, pg_catalog.to_regclass({qualified})::pg_catalog.oid)
  ON CONFLICT (one) DO UPDATE SET schema_name = EXCLUDED.schema_name, table_name = EXCLUDED.table_name, table_oid = EXCLUDED.table_oid;
CREATE OR REPLACE FUNCTION cheladb.users_guard() RETURNS trigger
  LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS
$f$
BEGIN
  IF CURRENT_USER = 'cheladb_auth_sync' THEN
    RETURN NULL;
  END IF;
  IF TG_OP <> 'UPDATE' THEN
    RAISE EXCEPTION USING ERRCODE = '42501',
      MESSAGE = format('rows of %I.%I are created and deleted by Cheladb app users', TG_TABLE_SCHEMA, TG_TABLE_NAME);
  END IF;
  IF NEW.id IS DISTINCT FROM OLD.id THEN
    RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE = 'column id is managed by Cheladb app users';
  END IF;
  IF NEW.email IS DISTINCT FROM OLD.email THEN
    RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE = 'column email is managed by Cheladb app users';
  END IF;
  IF NEW.email_verified IS DISTINCT FROM OLD.email_verified THEN
    RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE = 'column email_verified is managed by Cheladb app users';
  END IF;
  IF NEW.created_at IS DISTINCT FROM OLD.created_at THEN
    RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE = 'column created_at is managed by Cheladb app users';
  END IF;
  IF NEW.deleted_at IS DISTINCT FROM OLD.deleted_at THEN
    RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE = 'column deleted_at is managed by Cheladb app users';
  END IF;
  RETURN NULL;
END
$f$;
ALTER FUNCTION cheladb.users_guard() OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION cheladb.users_guard() FROM PUBLIC;
CREATE OR REPLACE TRIGGER cheladb_users_guard AFTER INSERT OR UPDATE OR DELETE ON {s}.{t}
  FOR EACH ROW EXECUTE FUNCTION cheladb.users_guard();
CREATE OR REPLACE TRIGGER cheladb_users_guard_truncate BEFORE TRUNCATE ON {s}.{t}
  FOR EACH STATEMENT EXECUTE FUNCTION cheladb.users_guard();
ALTER TABLE {s}.{t} ENABLE ALWAYS TRIGGER cheladb_users_guard;
ALTER TABLE {s}.{t} ENABLE ALWAYS TRIGGER cheladb_users_guard_truncate;
CREATE OR REPLACE FUNCTION cheladb.app_users_upsert(p_id uuid, p_email text, p_verified boolean, p_created timestamptz, p_data jsonb)
  RETURNS text LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS
$f$
DECLARE
  cfg record;
  tbl text;
  cols text;
  vals text;
  names text[];
  n bigint;
BEGIN
  SELECT schema_name, table_name INTO STRICT cfg FROM cheladb.app_users_table;
  tbl := format('%I.%I', cfg.schema_name, cfg.table_name);
  EXECUTE format('SELECT count(*) FROM %s WHERE id = $1', tbl) INTO n USING p_id;
  IF n > 0 THEN
    EXECUTE format('UPDATE %s SET email = $2, email_verified = $3 WHERE id = $1 AND (email IS DISTINCT FROM $2 OR email_verified IS DISTINCT FROM $3)', tbl)
      USING p_id, p_email, p_verified;
    RETURN 'updated';
  END IF;
  IF p_data IS NOT NULL THEN
    SELECT string_agg(quote_ident(a.attname), ', ' ORDER BY a.attnum),
           string_agg('r.' || quote_ident(a.attname), ', ' ORDER BY a.attnum),
           array_agg(a.attname::text)
      INTO cols, vals, names
      FROM pg_attribute a
     WHERE a.attrelid = tbl::regclass AND a.attnum > 0 AND NOT a.attisdropped
       AND a.attgenerated = '' AND a.attidentity <> 'a'
       AND a.attname <> ALL (ARRAY['id', 'email', 'email_verified', 'created_at', 'deleted_at']::name[])
       AND has_column_privilege('authenticated', a.attrelid, a.attname::text, 'INSERT')
       AND p_data ? a.attname::text;
    IF cols IS NOT NULL THEN
      BEGIN
        EXECUTE format('INSERT INTO %1$s (id, email, email_verified, created_at, %2$s) SELECT $1, $2, $3, $4, %3$s FROM jsonb_populate_record(NULL::%1$s, $5) r', tbl, cols, vals)
          USING p_id, p_email, p_verified, p_created,
            (SELECT jsonb_object_agg(e.key, e.value) FROM jsonb_each(p_data) e WHERE e.key = ANY (names));
        RETURN 'inserted';
      EXCEPTION WHEN integrity_constraint_violation OR data_exception THEN
        NULL;
      END;
    END IF;
  END IF;
  EXECUTE format('INSERT INTO %s (id, email, email_verified, created_at) VALUES ($1, $2, $3, $4) ON CONFLICT (id) DO NOTHING', tbl)
    USING p_id, p_email, p_verified, p_created;
  RETURN 'inserted_managed';
END
$f$;
CREATE OR REPLACE FUNCTION cheladb.app_users_gone(p_id uuid, p_mark boolean)
  RETURNS text LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS
$f$
DECLARE
  cfg record;
  tbl text;
  n bigint;
BEGIN
  SELECT schema_name, table_name INTO STRICT cfg FROM cheladb.app_users_table;
  tbl := format('%I.%I', cfg.schema_name, cfg.table_name);
  IF p_mark THEN
    EXECUTE format('UPDATE %s SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL', tbl) USING p_id;
  ELSE
    EXECUTE format('DELETE FROM %s WHERE id = $1', tbl) USING p_id;
  END IF;
  GET DIAGNOSTICS n = ROW_COUNT;
  RETURN CASE WHEN n > 0 THEN 'changed' ELSE 'unchanged' END;
END
$f$;
CREATE OR REPLACE FUNCTION cheladb.app_users_rows()
  RETURNS jsonb LANGUAGE plpgsql STABLE SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS
$f$
DECLARE
  cfg record;
  r jsonb;
BEGIN
  SELECT schema_name, table_name INTO STRICT cfg FROM cheladb.app_users_table;
  EXECUTE format('SELECT coalesce(jsonb_agg(jsonb_build_object(''id'', id, ''email'', email, ''email_verified'', email_verified, ''deleted_at'', deleted_at)), ''[]''::jsonb) FROM %I.%I',
    cfg.schema_name, cfg.table_name) INTO r;
  RETURN r;
END
$f$;
ALTER FUNCTION cheladb.app_users_upsert(uuid, text, boolean, timestamptz, jsonb) OWNER TO CURRENT_USER;
ALTER FUNCTION cheladb.app_users_gone(uuid, boolean) OWNER TO CURRENT_USER;
ALTER FUNCTION cheladb.app_users_rows() OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION cheladb.app_users_upsert(uuid, text, boolean, timestamptz, jsonb), cheladb.app_users_gone(uuid, boolean), cheladb.app_users_rows() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION cheladb.app_users_upsert(uuid, text, boolean, timestamptz, jsonb), cheladb.app_users_gone(uuid, boolean), cheladb.app_users_rows() TO cheladb_auth_sync;
CREATE OR REPLACE FUNCTION cheladb.users_protect() RETURNS event_trigger
  LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS
$f$
DECLARE
  cfg record;
  obj record;
  rel oid;
  found_type text;
  managed text[] := ARRAY['id', 'email', 'email_verified', 'created_at', 'deleted_at'];
  types text[] := ARRAY['uuid', 'text', 'boolean', 'timestamp with time zone', 'timestamp with time zone'];
BEGIN
  IF to_regclass('cheladb.app_users_table') IS NULL THEN RETURN; END IF;
  SELECT schema_name, table_name, table_oid INTO cfg FROM cheladb.app_users_table;
  IF NOT FOUND THEN RETURN; END IF;
  rel := cfg.table_oid;
  IF TG_EVENT = 'sql_drop' THEN
    FOR obj IN SELECT * FROM pg_event_trigger_dropped_objects() LOOP
      IF obj.classid = 'pg_class'::regclass AND obj.objid = rel AND obj.objsubid = 0 THEN
        RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE =
          format('table %I.%I is managed by Cheladb app users: disable app users first', cfg.schema_name, cfg.table_name);
      END IF;
      IF obj.object_type = 'table column' AND obj.objid = rel AND obj.address_names[3] = ANY (managed) THEN
        RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE =
          format('column %I of %I.%I is managed by Cheladb app users', obj.address_names[3], cfg.schema_name, cfg.table_name);
      END IF;
      IF obj.object_type = 'trigger'
         AND obj.address_names[1] = cfg.schema_name AND obj.address_names[2] = cfg.table_name
         AND obj.address_names[3] IN ('cheladb_users_guard', 'cheladb_users_guard_truncate') THEN
        RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE =
          format('trigger %I of %I.%I is managed by Cheladb app users', obj.address_names[3], cfg.schema_name, cfg.table_name);
      END IF;
    END LOOP;
    RETURN;
  END IF;
  -- ddl_command_end: only commands that touch the users table, its schema or its triggers are checked,
  -- so unrelated DDL works whatever state the users table is in.
  IF NOT EXISTS (SELECT FROM pg_event_trigger_ddl_commands() c
                  WHERE (c.classid = 'pg_class'::regclass AND c.objid = rel)
                     OR (c.classid = 'pg_namespace'::regclass
                         AND c.objid = (SELECT k.relnamespace FROM pg_class k WHERE k.oid = rel))
                     OR (c.classid = 'pg_trigger'::regclass
                         AND c.objid IN (SELECT t.oid FROM pg_trigger t WHERE t.tgrelid = rel))) THEN
    RETURN;
  END IF;
  IF to_regclass(format('%I.%I', cfg.schema_name, cfg.table_name))::oid IS DISTINCT FROM rel THEN
    RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE =
      format('table %I.%I is managed by Cheladb app users: it can''t be renamed or moved', cfg.schema_name, cfg.table_name);
  END IF;
  FOR i IN 1 .. array_length(managed, 1) LOOP
    found_type := NULL;
    SELECT format_type(a.atttypid, a.atttypmod) INTO found_type FROM pg_attribute a
     WHERE a.attrelid = rel AND a.attname = managed[i] AND NOT a.attisdropped;
    IF found_type IS DISTINCT FROM types[i] THEN
      RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE =
        format('column %I of %I.%I is managed by Cheladb app users: it can''t be renamed, dropped or retyped', managed[i], cfg.schema_name, cfg.table_name);
    END IF;
  END LOOP;
  IF (SELECT count(*) FROM pg_trigger t
       WHERE t.tgrelid = rel AND t.tgname IN ('cheladb_users_guard', 'cheladb_users_guard_truncate')
         AND t.tgfoid = 'cheladb.users_guard()'::regprocedure AND t.tgenabled IN ('O', 'A')) <> 2 THEN
    RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE =
      format('the cheladb_users_guard triggers on %I.%I are managed by Cheladb app users: they can''t be disabled, renamed or replaced', cfg.schema_name, cfg.table_name);
  END IF;
END
$f$;
ALTER FUNCTION cheladb.users_protect() OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION cheladb.users_protect() FROM PUBLIC;
CREATE EVENT TRIGGER cheladb_users_protect_drop ON sql_drop EXECUTE FUNCTION cheladb.users_protect();
CREATE EVENT TRIGGER cheladb_users_protect_ddl ON ddl_command_end
  WHEN TAG IN ('ALTER TABLE', 'ALTER INDEX', 'ALTER SCHEMA', 'ALTER TRIGGER', 'CREATE TRIGGER')
  EXECUTE FUNCTION cheladb.users_protect();
COMMIT;
"#
    )
}

/// Removes the app users machinery from the auth database (disable): the protect event
/// triggers, the guard triggers, the bootstrap's two own-row policies (they call
/// `cheladb.user_id()`, so a later data API removal would fail on them with 2BP01), the sync
/// functions, the config table and the `cheladb_auth_sync` role. The users table (RLS stays
/// on), its unique index, any policy the developer added and the claim helpers stay. A role still used in another database is kept (a notice) instead of
/// rolling the teardown back. Takes [`BOOTSTRAP_LOCK_KEY`] after a [`LOCK_TIMEOUT_SECS`] lock timeout; a
/// `const` can't format them in, so its text repeats both (a test keeps them in step).
pub const USERS_TEARDOWN_SQL: &str = r#"-- cheladb:users-teardown
BEGIN;
SET search_path = pg_catalog, pg_temp;
SET LOCAL lock_timeout = '20s';
SELECT pg_catalog.pg_advisory_xact_lock(7163086723497615873);
DROP EVENT TRIGGER IF EXISTS cheladb_users_protect_drop;
DROP EVENT TRIGGER IF EXISTS cheladb_users_protect_ddl;
DO $$ DECLARE cfg record; BEGIN
  IF pg_catalog.to_regclass('cheladb.app_users_table') IS NOT NULL THEN
    FOR cfg IN SELECT schema_name, table_name FROM cheladb.app_users_table LOOP
      IF pg_catalog.to_regclass(pg_catalog.format('%I.%I', cfg.schema_name, cfg.table_name)) IS NOT NULL THEN
        EXECUTE pg_catalog.format('DROP TRIGGER IF EXISTS cheladb_users_guard ON %I.%I', cfg.schema_name, cfg.table_name);
        EXECUTE pg_catalog.format('DROP TRIGGER IF EXISTS cheladb_users_guard_truncate ON %I.%I', cfg.schema_name, cfg.table_name);
        EXECUTE pg_catalog.format('DROP POLICY IF EXISTS cheladb_users_select_own ON %I.%I', cfg.schema_name, cfg.table_name);
        EXECUTE pg_catalog.format('DROP POLICY IF EXISTS cheladb_users_update_own ON %I.%I', cfg.schema_name, cfg.table_name);
      END IF;
    END LOOP;
  END IF;
END $$;
DROP FUNCTION IF EXISTS cheladb.users_guard();
DROP FUNCTION IF EXISTS cheladb.users_protect();
DROP FUNCTION IF EXISTS cheladb.app_users_upsert(uuid, text, boolean, timestamptz, jsonb);
DROP FUNCTION IF EXISTS cheladb.app_users_gone(uuid, boolean);
DROP FUNCTION IF EXISTS cheladb.app_users_rows();
DROP TABLE IF EXISTS cheladb.app_users_table;
DO $$ BEGIN
  IF EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'cheladb_auth_sync') THEN
    DROP OWNED BY cheladb_auth_sync;
    BEGIN
      DROP ROLE cheladb_auth_sync;
    EXCEPTION WHEN dependent_objects_still_exist THEN
      RAISE NOTICE 'cheladb: the role cheladb_auth_sync is kept: %', SQLERRM;
    END;
  END IF;
END $$;
COMMIT;
"#;

/// The claim helpers (`cheladb.claims()`, `cheladb.user_id()`, `cheladb.user_email()`), with
/// no transaction header: spliced into the data API bootstrap and [`users_bootstrap_sql`].
/// `SECURITY INVOKER` `BEGIN ATOMIC` SQL functions owned by the superuser running the script,
/// usable by every role (T4): a policy written without `TO` applies to PUBLIC, so a custom
/// data API role or the console must be able to call them. They only read a GUC, and
/// everything else in `cheladb` stays revoked from PUBLIC.
pub fn claim_helpers_body() -> String {
    r#"GRANT USAGE ON SCHEMA cheladb TO PUBLIC;
CREATE OR REPLACE FUNCTION cheladb.claims() RETURNS jsonb LANGUAGE sql STABLE PARALLEL SAFE
BEGIN ATOMIC
  SELECT COALESCE(NULLIF(pg_catalog.current_setting('request.jwt.claims', true), '')::jsonb, '{}'::jsonb);
END;
CREATE OR REPLACE FUNCTION cheladb.user_id() RETURNS uuid LANGUAGE sql STABLE PARALLEL SAFE
BEGIN ATOMIC
  SELECT (cheladb.claims() OPERATOR(pg_catalog.->>) 'sub')::uuid;
END;
CREATE OR REPLACE FUNCTION cheladb.user_email() RETURNS text LANGUAGE sql STABLE PARALLEL SAFE
BEGIN ATOMIC
  SELECT cheladb.claims() OPERATOR(pg_catalog.->>) 'email';
END;
ALTER FUNCTION cheladb.claims() OWNER TO CURRENT_USER;
ALTER FUNCTION cheladb.user_id() OWNER TO CURRENT_USER;
ALTER FUNCTION cheladb.user_email() OWNER TO CURRENT_USER;
GRANT EXECUTE ON FUNCTION cheladb.claims(), cheladb.user_id(), cheladb.user_email() TO PUBLIC;
"#
    .to_string()
}

/// [`claim_helpers_body`] as its own transaction, under [`BOOTSTRAP_LOCK_KEY`].
pub fn claim_helpers_sql() -> String {
    format!(
        "-- cheladb:claim-helpers\nBEGIN;\nSET search_path = pg_catalog, pg_temp;\nSET LOCAL lock_timeout = '{LOCK_TIMEOUT_SECS}s';\n\
         SELECT pg_catalog.pg_advisory_xact_lock({BOOTSTRAP_LOCK_KEY});\n{}COMMIT;\n",
        claim_helpers_body()
    )
}

/// Whether `schema.table` exists (psql prints `t` or `f`).
pub fn users_table_exists_sql(schema: &str, table: &str) -> String {
    format!(
        "SET search_path = pg_catalog, pg_temp; SELECT pg_catalog.to_regclass({}) IS NOT NULL;",
        quote_literal(&format!("{}.{}", quote_ident(schema), quote_ident(table)))
    )
}

/// Applies one user's state as the sync role: `cheladb.app_users_upsert` prints `updated`,
/// `inserted` or `inserted_managed` (the app columns of `data` were left out).
pub fn upsert_call_sql(
    id: Uuid,
    email: &str,
    verified: bool,
    created: DateTime<Utc>,
    data: Option<&serde_json::Value>,
) -> String {
    let data = data.map_or_else(|| "NULL".to_string(), |d| quote_literal(&d.to_string()));
    format!(
        "BEGIN; SET LOCAL ROLE {USERS_SYNC_ROLE}; SELECT cheladb.app_users_upsert({}, {}, {verified}, {}, {data}); COMMIT;",
        quote_literal(&id.to_string()),
        quote_literal(email),
        quote_literal(&created.to_rfc3339()),
    )
}

/// A user gone from Zitadel, as the sync role: marked deleted (`mark`) or its row deleted.
/// `cheladb.app_users_gone` prints `changed` or `unchanged`.
pub fn gone_call_sql(id: Uuid, mark: bool) -> String {
    format!(
        "BEGIN; SET LOCAL ROLE {USERS_SYNC_ROLE}; SELECT cheladb.app_users_gone({}, {mark}); COMMIT;",
        quote_literal(&id.to_string())
    )
}

/// Every row's managed state, as the sync role: one jsonb array of
/// `{id, email, email_verified, deleted_at}`.
pub const USERS_ROWS_SQL: &str =
    "BEGIN; SET LOCAL ROLE cheladb_auth_sync; SELECT cheladb.app_users_rows(); COMMIT;";

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;
    use crate::data_api::BOOTSTRAP_LOCK_KEY;

    /// The claim helpers: usable by every role (T4: a policy without `TO` applies to PUBLIC,
    /// custom data API roles included); they only read a GUC.
    const CLAIMS: &str = r#"GRANT USAGE ON SCHEMA cheladb TO PUBLIC;
CREATE OR REPLACE FUNCTION cheladb.claims() RETURNS jsonb LANGUAGE sql STABLE PARALLEL SAFE
BEGIN ATOMIC
  SELECT COALESCE(NULLIF(pg_catalog.current_setting('request.jwt.claims', true), '')::jsonb, '{}'::jsonb);
END;
CREATE OR REPLACE FUNCTION cheladb.user_id() RETURNS uuid LANGUAGE sql STABLE PARALLEL SAFE
BEGIN ATOMIC
  SELECT (cheladb.claims() OPERATOR(pg_catalog.->>) 'sub')::uuid;
END;
CREATE OR REPLACE FUNCTION cheladb.user_email() RETURNS text LANGUAGE sql STABLE PARALLEL SAFE
BEGIN ATOMIC
  SELECT cheladb.claims() OPERATOR(pg_catalog.->>) 'email';
END;
ALTER FUNCTION cheladb.claims() OWNER TO CURRENT_USER;
ALTER FUNCTION cheladb.user_id() OWNER TO CURRENT_USER;
ALTER FUNCTION cheladb.user_email() OWNER TO CURRENT_USER;
GRANT EXECUTE ON FUNCTION cheladb.claims(), cheladb.user_id(), cheladb.user_email() TO PUBLIC;
"#;

    /// The users bootstrap for `public.users`, password `pw`, owner `postgres`, up to the
    /// claim helpers.
    const BOOTSTRAP_HEAD: &str = r#"-- cheladb:users-bootstrap
BEGIN;
SET search_path = pg_catalog, pg_temp;
SET LOCAL lock_timeout = '20s';
SELECT pg_catalog.pg_advisory_xact_lock(7163086723497615873);
DROP EVENT TRIGGER IF EXISTS cheladb_users_protect_drop;
DROP EVENT TRIGGER IF EXISTS cheladb_users_protect_ddl;
DO $$ BEGIN
  IF pg_catalog.to_regnamespace('cheladb') IS NULL THEN
    RAISE EXCEPTION 'cheladb_data_api_missing: the data API is not enabled in this database';
  END IF;
  IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'cheladb_auth_sync') THEN
    CREATE ROLE cheladb_auth_sync LOGIN NOINHERIT BYPASSRLS;
  END IF;
END $$;
ALTER ROLE cheladb_auth_sync LOGIN NOINHERIT BYPASSRLS NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION CONNECTION LIMIT 10 PASSWORD 'pw';
ALTER ROLE cheladb_auth_sync RESET ALL;
"#;

    /// The rest of the users bootstrap, after the claim helpers.
    const BOOTSTRAP_TAIL: &str = r#"CREATE SCHEMA IF NOT EXISTS "public" AUTHORIZATION "postgres";
DO $$ BEGIN
  IF pg_catalog.to_regclass('"public"."users"') IS NULL THEN
    CREATE TABLE "public"."users" (
      id uuid PRIMARY KEY,
      email text NOT NULL,
      email_verified boolean NOT NULL DEFAULT false,
      created_at timestamptz NOT NULL DEFAULT pg_catalog.now(),
      deleted_at timestamptz
    );
    ALTER TABLE "public"."users" OWNER TO "postgres";
    REVOKE ALL ON "public"."users" FROM anon;
    GRANT SELECT, UPDATE, DELETE ON "public"."users" TO authenticated;
    ALTER TABLE "public"."users" ENABLE ROW LEVEL SECURITY;
    CREATE POLICY cheladb_users_select_own ON "public"."users" FOR SELECT TO authenticated
      USING (id OPERATOR(pg_catalog.=) cheladb.user_id());
    CREATE POLICY cheladb_users_update_own ON "public"."users" FOR UPDATE TO authenticated
      USING (id OPERATOR(pg_catalog.=) cheladb.user_id()) WITH CHECK (id OPERATOR(pg_catalog.=) cheladb.user_id());
  END IF;
END $$;
CREATE UNIQUE INDEX IF NOT EXISTS cheladb_users_email_key ON "public"."users" (pg_catalog.lower(email)) WHERE deleted_at IS NULL;
DO $$ DECLARE r pg_catalog.name; BEGIN
  FOR r IN SELECT g.rolname FROM pg_catalog.pg_class c, pg_catalog.aclexplode(c.relacl) a
      JOIN pg_catalog.pg_roles g ON g.oid OPERATOR(pg_catalog.=) a.grantee
    WHERE c.oid OPERATOR(pg_catalog.=) '"public"."users"'::pg_catalog.regclass
      AND a.privilege_type OPERATOR(pg_catalog.=) 'INSERT'
      AND g.rolname OPERATOR(pg_catalog.=) ANY (ARRAY['anon', 'authenticated']::pg_catalog.name[])
  LOOP
    EXECUTE pg_catalog.format('REVOKE INSERT ON %s FROM %I', '"public"."users"'::pg_catalog.regclass, r);
  END LOOP;
END $$;
GRANT USAGE ON SCHEMA "public" TO cheladb_auth_sync;
GRANT SELECT, INSERT, UPDATE, DELETE ON "public"."users" TO cheladb_auth_sync;
CREATE TABLE IF NOT EXISTS cheladb.app_users_table (one boolean PRIMARY KEY DEFAULT true CHECK (one),
  schema_name text NOT NULL, table_name text NOT NULL, table_oid oid NOT NULL);
ALTER TABLE cheladb.app_users_table OWNER TO cheladb_admin;
REVOKE ALL ON cheladb.app_users_table FROM PUBLIC;
GRANT USAGE ON SCHEMA cheladb TO cheladb_auth_sync;
GRANT SELECT ON cheladb.app_users_table TO authenticator, anon, authenticated, cheladb_auth_sync, "postgres";
INSERT INTO cheladb.app_users_table (schema_name, table_name, table_oid)
  VALUES ('public', 'users', pg_catalog.to_regclass('"public"."users"')::pg_catalog.oid)
  ON CONFLICT (one) DO UPDATE SET schema_name = EXCLUDED.schema_name, table_name = EXCLUDED.table_name, table_oid = EXCLUDED.table_oid;
CREATE OR REPLACE FUNCTION cheladb.users_guard() RETURNS trigger
  LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS
$f$
BEGIN
  IF CURRENT_USER = 'cheladb_auth_sync' THEN
    RETURN NULL;
  END IF;
  IF TG_OP <> 'UPDATE' THEN
    RAISE EXCEPTION USING ERRCODE = '42501',
      MESSAGE = format('rows of %I.%I are created and deleted by Cheladb app users', TG_TABLE_SCHEMA, TG_TABLE_NAME);
  END IF;
  IF NEW.id IS DISTINCT FROM OLD.id THEN
    RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE = 'column id is managed by Cheladb app users';
  END IF;
  IF NEW.email IS DISTINCT FROM OLD.email THEN
    RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE = 'column email is managed by Cheladb app users';
  END IF;
  IF NEW.email_verified IS DISTINCT FROM OLD.email_verified THEN
    RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE = 'column email_verified is managed by Cheladb app users';
  END IF;
  IF NEW.created_at IS DISTINCT FROM OLD.created_at THEN
    RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE = 'column created_at is managed by Cheladb app users';
  END IF;
  IF NEW.deleted_at IS DISTINCT FROM OLD.deleted_at THEN
    RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE = 'column deleted_at is managed by Cheladb app users';
  END IF;
  RETURN NULL;
END
$f$;
ALTER FUNCTION cheladb.users_guard() OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION cheladb.users_guard() FROM PUBLIC;
CREATE OR REPLACE TRIGGER cheladb_users_guard AFTER INSERT OR UPDATE OR DELETE ON "public"."users"
  FOR EACH ROW EXECUTE FUNCTION cheladb.users_guard();
CREATE OR REPLACE TRIGGER cheladb_users_guard_truncate BEFORE TRUNCATE ON "public"."users"
  FOR EACH STATEMENT EXECUTE FUNCTION cheladb.users_guard();
ALTER TABLE "public"."users" ENABLE ALWAYS TRIGGER cheladb_users_guard;
ALTER TABLE "public"."users" ENABLE ALWAYS TRIGGER cheladb_users_guard_truncate;
CREATE OR REPLACE FUNCTION cheladb.app_users_upsert(p_id uuid, p_email text, p_verified boolean, p_created timestamptz, p_data jsonb)
  RETURNS text LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS
$f$
DECLARE
  cfg record;
  tbl text;
  cols text;
  vals text;
  names text[];
  n bigint;
BEGIN
  SELECT schema_name, table_name INTO STRICT cfg FROM cheladb.app_users_table;
  tbl := format('%I.%I', cfg.schema_name, cfg.table_name);
  EXECUTE format('SELECT count(*) FROM %s WHERE id = $1', tbl) INTO n USING p_id;
  IF n > 0 THEN
    EXECUTE format('UPDATE %s SET email = $2, email_verified = $3 WHERE id = $1 AND (email IS DISTINCT FROM $2 OR email_verified IS DISTINCT FROM $3)', tbl)
      USING p_id, p_email, p_verified;
    RETURN 'updated';
  END IF;
  IF p_data IS NOT NULL THEN
    SELECT string_agg(quote_ident(a.attname), ', ' ORDER BY a.attnum),
           string_agg('r.' || quote_ident(a.attname), ', ' ORDER BY a.attnum),
           array_agg(a.attname::text)
      INTO cols, vals, names
      FROM pg_attribute a
     WHERE a.attrelid = tbl::regclass AND a.attnum > 0 AND NOT a.attisdropped
       AND a.attgenerated = '' AND a.attidentity <> 'a'
       AND a.attname <> ALL (ARRAY['id', 'email', 'email_verified', 'created_at', 'deleted_at']::name[])
       AND has_column_privilege('authenticated', a.attrelid, a.attname::text, 'INSERT')
       AND p_data ? a.attname::text;
    IF cols IS NOT NULL THEN
      BEGIN
        EXECUTE format('INSERT INTO %1$s (id, email, email_verified, created_at, %2$s) SELECT $1, $2, $3, $4, %3$s FROM jsonb_populate_record(NULL::%1$s, $5) r', tbl, cols, vals)
          USING p_id, p_email, p_verified, p_created,
            (SELECT jsonb_object_agg(e.key, e.value) FROM jsonb_each(p_data) e WHERE e.key = ANY (names));
        RETURN 'inserted';
      EXCEPTION WHEN integrity_constraint_violation OR data_exception THEN
        NULL;
      END;
    END IF;
  END IF;
  EXECUTE format('INSERT INTO %s (id, email, email_verified, created_at) VALUES ($1, $2, $3, $4) ON CONFLICT (id) DO NOTHING', tbl)
    USING p_id, p_email, p_verified, p_created;
  RETURN 'inserted_managed';
END
$f$;
CREATE OR REPLACE FUNCTION cheladb.app_users_gone(p_id uuid, p_mark boolean)
  RETURNS text LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS
$f$
DECLARE
  cfg record;
  tbl text;
  n bigint;
BEGIN
  SELECT schema_name, table_name INTO STRICT cfg FROM cheladb.app_users_table;
  tbl := format('%I.%I', cfg.schema_name, cfg.table_name);
  IF p_mark THEN
    EXECUTE format('UPDATE %s SET deleted_at = now() WHERE id = $1 AND deleted_at IS NULL', tbl) USING p_id;
  ELSE
    EXECUTE format('DELETE FROM %s WHERE id = $1', tbl) USING p_id;
  END IF;
  GET DIAGNOSTICS n = ROW_COUNT;
  RETURN CASE WHEN n > 0 THEN 'changed' ELSE 'unchanged' END;
END
$f$;
CREATE OR REPLACE FUNCTION cheladb.app_users_rows()
  RETURNS jsonb LANGUAGE plpgsql STABLE SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS
$f$
DECLARE
  cfg record;
  r jsonb;
BEGIN
  SELECT schema_name, table_name INTO STRICT cfg FROM cheladb.app_users_table;
  EXECUTE format('SELECT coalesce(jsonb_agg(jsonb_build_object(''id'', id, ''email'', email, ''email_verified'', email_verified, ''deleted_at'', deleted_at)), ''[]''::jsonb) FROM %I.%I',
    cfg.schema_name, cfg.table_name) INTO r;
  RETURN r;
END
$f$;
ALTER FUNCTION cheladb.app_users_upsert(uuid, text, boolean, timestamptz, jsonb) OWNER TO CURRENT_USER;
ALTER FUNCTION cheladb.app_users_gone(uuid, boolean) OWNER TO CURRENT_USER;
ALTER FUNCTION cheladb.app_users_rows() OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION cheladb.app_users_upsert(uuid, text, boolean, timestamptz, jsonb), cheladb.app_users_gone(uuid, boolean), cheladb.app_users_rows() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION cheladb.app_users_upsert(uuid, text, boolean, timestamptz, jsonb), cheladb.app_users_gone(uuid, boolean), cheladb.app_users_rows() TO cheladb_auth_sync;
CREATE OR REPLACE FUNCTION cheladb.users_protect() RETURNS event_trigger
  LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS
$f$
DECLARE
  cfg record;
  obj record;
  rel oid;
  found_type text;
  managed text[] := ARRAY['id', 'email', 'email_verified', 'created_at', 'deleted_at'];
  types text[] := ARRAY['uuid', 'text', 'boolean', 'timestamp with time zone', 'timestamp with time zone'];
BEGIN
  IF to_regclass('cheladb.app_users_table') IS NULL THEN RETURN; END IF;
  SELECT schema_name, table_name, table_oid INTO cfg FROM cheladb.app_users_table;
  IF NOT FOUND THEN RETURN; END IF;
  rel := cfg.table_oid;
  IF TG_EVENT = 'sql_drop' THEN
    FOR obj IN SELECT * FROM pg_event_trigger_dropped_objects() LOOP
      IF obj.classid = 'pg_class'::regclass AND obj.objid = rel AND obj.objsubid = 0 THEN
        RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE =
          format('table %I.%I is managed by Cheladb app users: disable app users first', cfg.schema_name, cfg.table_name);
      END IF;
      IF obj.object_type = 'table column' AND obj.objid = rel AND obj.address_names[3] = ANY (managed) THEN
        RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE =
          format('column %I of %I.%I is managed by Cheladb app users', obj.address_names[3], cfg.schema_name, cfg.table_name);
      END IF;
      IF obj.object_type = 'trigger'
         AND obj.address_names[1] = cfg.schema_name AND obj.address_names[2] = cfg.table_name
         AND obj.address_names[3] IN ('cheladb_users_guard', 'cheladb_users_guard_truncate') THEN
        RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE =
          format('trigger %I of %I.%I is managed by Cheladb app users', obj.address_names[3], cfg.schema_name, cfg.table_name);
      END IF;
    END LOOP;
    RETURN;
  END IF;
  -- ddl_command_end: only commands that touch the users table, its schema or its triggers are checked,
  -- so unrelated DDL works whatever state the users table is in.
  IF NOT EXISTS (SELECT FROM pg_event_trigger_ddl_commands() c
                  WHERE (c.classid = 'pg_class'::regclass AND c.objid = rel)
                     OR (c.classid = 'pg_namespace'::regclass
                         AND c.objid = (SELECT k.relnamespace FROM pg_class k WHERE k.oid = rel))
                     OR (c.classid = 'pg_trigger'::regclass
                         AND c.objid IN (SELECT t.oid FROM pg_trigger t WHERE t.tgrelid = rel))) THEN
    RETURN;
  END IF;
  IF to_regclass(format('%I.%I', cfg.schema_name, cfg.table_name))::oid IS DISTINCT FROM rel THEN
    RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE =
      format('table %I.%I is managed by Cheladb app users: it can''t be renamed or moved', cfg.schema_name, cfg.table_name);
  END IF;
  FOR i IN 1 .. array_length(managed, 1) LOOP
    found_type := NULL;
    SELECT format_type(a.atttypid, a.atttypmod) INTO found_type FROM pg_attribute a
     WHERE a.attrelid = rel AND a.attname = managed[i] AND NOT a.attisdropped;
    IF found_type IS DISTINCT FROM types[i] THEN
      RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE =
        format('column %I of %I.%I is managed by Cheladb app users: it can''t be renamed, dropped or retyped', managed[i], cfg.schema_name, cfg.table_name);
    END IF;
  END LOOP;
  IF (SELECT count(*) FROM pg_trigger t
       WHERE t.tgrelid = rel AND t.tgname IN ('cheladb_users_guard', 'cheladb_users_guard_truncate')
         AND t.tgfoid = 'cheladb.users_guard()'::regprocedure AND t.tgenabled IN ('O', 'A')) <> 2 THEN
    RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE =
      format('the cheladb_users_guard triggers on %I.%I are managed by Cheladb app users: they can''t be disabled, renamed or replaced', cfg.schema_name, cfg.table_name);
  END IF;
END
$f$;
ALTER FUNCTION cheladb.users_protect() OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION cheladb.users_protect() FROM PUBLIC;
CREATE EVENT TRIGGER cheladb_users_protect_drop ON sql_drop EXECUTE FUNCTION cheladb.users_protect();
CREATE EVENT TRIGGER cheladb_users_protect_ddl ON ddl_command_end
  WHEN TAG IN ('ALTER TABLE', 'ALTER INDEX', 'ALTER SCHEMA', 'ALTER TRIGGER', 'CREATE TRIGGER')
  EXECUTE FUNCTION cheladb.users_protect();
COMMIT;
"#;

    fn bootstrap() -> String {
        users_bootstrap_sql("public", "users", "pw", "postgres")
    }

    /// The text of the function `name` in the bootstrap, up to its closing `$f$;`.
    fn function_text(sql: &str, name: &str) -> String {
        let start = sql
            .find(&format!("CREATE OR REPLACE FUNCTION cheladb.{name}("))
            .unwrap_or_else(|| panic!("{name} is created"));
        let end = sql[start..].find("$f$;").unwrap();
        sql[start..start + end].to_string()
    }

    #[test]
    fn users_bootstrap_sql_is_exact() {
        assert_eq!(
            bootstrap(),
            format!("{BOOTSTRAP_HEAD}{CLAIMS}{BOOTSTRAP_TAIL}")
        );
        assert!(bootstrap().contains(&format!("pg_advisory_xact_lock({BOOTSTRAP_LOCK_KEY});")));
    }

    #[test]
    fn users_bootstrap_quotes_the_password() {
        let sql = users_bootstrap_sql("public", "users", "it's", "postgres");
        assert!(
            sql.contains("CONNECTION LIMIT 10 PASSWORD 'it''s';\n"),
            "{sql}"
        );
    }

    #[test]
    fn users_bootstrap_quotes_names() {
        let sql = users_bootstrap_sql("my schema", "app\"users", "pw", "o\"wner");
        assert!(sql.contains("CREATE TABLE \"my schema\".\"app\"\"users\" ("));
        assert!(sql.contains("pg_catalog.to_regclass('\"my schema\".\"app\"\"users\"')"));
        assert!(sql.contains("VALUES ('my schema', 'app\"users', "));
        assert!(
            sql.contains("CREATE SCHEMA IF NOT EXISTS \"my schema\" AUTHORIZATION \"o\"\"wner\";")
        );
    }

    #[test]
    fn users_teardown_sql_is_exact() {
        assert_eq!(
            USERS_TEARDOWN_SQL,
            r#"-- cheladb:users-teardown
BEGIN;
SET search_path = pg_catalog, pg_temp;
SET LOCAL lock_timeout = '20s';
SELECT pg_catalog.pg_advisory_xact_lock(7163086723497615873);
DROP EVENT TRIGGER IF EXISTS cheladb_users_protect_drop;
DROP EVENT TRIGGER IF EXISTS cheladb_users_protect_ddl;
DO $$ DECLARE cfg record; BEGIN
  IF pg_catalog.to_regclass('cheladb.app_users_table') IS NOT NULL THEN
    FOR cfg IN SELECT schema_name, table_name FROM cheladb.app_users_table LOOP
      IF pg_catalog.to_regclass(pg_catalog.format('%I.%I', cfg.schema_name, cfg.table_name)) IS NOT NULL THEN
        EXECUTE pg_catalog.format('DROP TRIGGER IF EXISTS cheladb_users_guard ON %I.%I', cfg.schema_name, cfg.table_name);
        EXECUTE pg_catalog.format('DROP TRIGGER IF EXISTS cheladb_users_guard_truncate ON %I.%I', cfg.schema_name, cfg.table_name);
        EXECUTE pg_catalog.format('DROP POLICY IF EXISTS cheladb_users_select_own ON %I.%I', cfg.schema_name, cfg.table_name);
        EXECUTE pg_catalog.format('DROP POLICY IF EXISTS cheladb_users_update_own ON %I.%I', cfg.schema_name, cfg.table_name);
      END IF;
    END LOOP;
  END IF;
END $$;
DROP FUNCTION IF EXISTS cheladb.users_guard();
DROP FUNCTION IF EXISTS cheladb.users_protect();
DROP FUNCTION IF EXISTS cheladb.app_users_upsert(uuid, text, boolean, timestamptz, jsonb);
DROP FUNCTION IF EXISTS cheladb.app_users_gone(uuid, boolean);
DROP FUNCTION IF EXISTS cheladb.app_users_rows();
DROP TABLE IF EXISTS cheladb.app_users_table;
DO $$ BEGIN
  IF EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'cheladb_auth_sync') THEN
    DROP OWNED BY cheladb_auth_sync;
    BEGIN
      DROP ROLE cheladb_auth_sync;
    EXCEPTION WHEN dependent_objects_still_exist THEN
      RAISE NOTICE 'cheladb: the role cheladb_auth_sync is kept: %', SQLERRM;
    END;
  END IF;
END $$;
COMMIT;
"#
        );
        assert!(
            USERS_TEARDOWN_SQL.contains(&format!("pg_advisory_xact_lock({BOOTSTRAP_LOCK_KEY});"))
        );
        assert!(USERS_TEARDOWN_SQL.contains(&format!(
            "SET LOCAL lock_timeout = '{}s';",
            crate::data_api::LOCK_TIMEOUT_SECS
        )));
        // The users table, its index and the claim helpers stay.
        for kept in [
            "DROP TABLE IF EXISTS \"",
            "DROP INDEX",
            "cheladb.claims",
            "cheladb.user_id",
        ] {
            assert!(!USERS_TEARDOWN_SQL.contains(kept), "{kept}");
        }
    }

    #[test]
    fn claim_helpers_sql_is_exact() {
        assert_eq!(claim_helpers_body(), CLAIMS);
        assert_eq!(
            claim_helpers_sql(),
            format!(
                "-- cheladb:claim-helpers\nBEGIN;\nSET search_path = pg_catalog, pg_temp;\nSET LOCAL lock_timeout = '20s';\n\
                 SELECT pg_catalog.pg_advisory_xact_lock({BOOTSTRAP_LOCK_KEY});\n{CLAIMS}COMMIT;\n"
            )
        );
    }

    /// C2: a broken earlier state (the users table dropped with event triggers off) can't
    /// make the protect trigger refuse the re-run that repairs it.
    #[test]
    fn users_bootstrap_drops_protect_triggers_right_after_the_lock() {
        let sql = bootstrap();
        let lock = format!(
            "SELECT pg_catalog.pg_advisory_xact_lock({BOOTSTRAP_LOCK_KEY});\n\
             DROP EVENT TRIGGER IF EXISTS cheladb_users_protect_drop;\n\
             DROP EVENT TRIGGER IF EXISTS cheladb_users_protect_ddl;\n"
        );
        assert!(sql.starts_with(&format!(
            "-- cheladb:users-bootstrap\nBEGIN;\nSET search_path = pg_catalog, pg_temp;\nSET LOCAL lock_timeout = '20s';\n{lock}"
        )));
        // Re-created last, just before the commit.
        let create = sql
            .find("CREATE EVENT TRIGGER cheladb_users_protect_drop")
            .unwrap();
        assert!(sql[..create].rfind("CREATE OR REPLACE FUNCTION").unwrap() < create);
        assert!(sql.ends_with("EXECUTE FUNCTION cheladb.users_protect();\nCOMMIT;\n"));
        assert_eq!(sql.matches("BEGIN;").count(), 1);
        assert_eq!(sql.matches("COMMIT;").count(), 1);
    }

    /// C2: unrelated DDL never runs the checks, so it works whatever state the users table
    /// is in; `ALTER INDEX … RENAME` can rename a table, so its tag is watched too.
    #[test]
    fn protect_ddl_checks_only_commands_on_the_users_table() {
        let sql = bootstrap();
        let f = function_text(&sql, "users_protect");
        let filter = f
            .find("IF NOT EXISTS (SELECT FROM pg_event_trigger_ddl_commands() c")
            .expect("the filter");
        for part in [
            "c.classid = 'pg_class'::regclass AND c.objid = rel",
            "c.classid = 'pg_namespace'::regclass",
            "SELECT k.relnamespace FROM pg_class k WHERE k.oid = rel",
            "c.classid = 'pg_trigger'::regclass",
            "SELECT t.oid FROM pg_trigger t WHERE t.tgrelid = rel",
        ] {
            assert!(f[filter..].contains(part), "{part}");
        }
        let checks = f
            .find("IF to_regclass(format('%I.%I', cfg.schema_name, cfg.table_name))::oid")
            .unwrap();
        assert!(filter < checks);
        assert!(sql.contains(
            "WHEN TAG IN ('ALTER TABLE', 'ALTER INDEX', 'ALTER SCHEMA', 'ALTER TRIGGER', 'CREATE TRIGGER')"
        ));
    }

    /// P28: a new users table is closed to `anon` and shows each user only their own row.
    #[test]
    fn users_bootstrap_locks_the_table_down() {
        let sql = bootstrap();
        assert!(sql.contains("REVOKE ALL ON \"public\".\"users\" FROM anon;\n"));
        assert!(!sql.contains("TO anon;"), "no grant to anon");
        assert!(!sql.contains("ON \"public\".\"users\" TO anon"));
        assert!(sql.contains("ALTER TABLE \"public\".\"users\" ENABLE ROW LEVEL SECURITY;\n"));
        assert!(sql.contains(
            "CREATE POLICY cheladb_users_select_own ON \"public\".\"users\" FOR SELECT TO authenticated\n      \
             USING (id OPERATOR(pg_catalog.=) cheladb.user_id());"
        ));
        assert!(sql.contains(
            "CREATE POLICY cheladb_users_update_own ON \"public\".\"users\" FOR UPDATE TO authenticated\n      \
             USING (id OPERATOR(pg_catalog.=) cheladb.user_id()) WITH CHECK (id OPERATOR(pg_catalog.=) cheladb.user_id());"
        ));
        // The helpers the policies use are installed before them.
        let helpers = sql
            .find("CREATE OR REPLACE FUNCTION cheladb.user_id()")
            .unwrap();
        assert!(helpers < sql.find("CREATE POLICY").unwrap());
    }

    /// P34: sign-up data is opt-in per column. The users table never keeps a table-level
    /// INSERT for `anon` or `authenticated` (checked on every run, so a table adopted or
    /// re-granted later loses it), and the upsert's filter only passes columns granted with
    /// `GRANT INSERT (col) … TO authenticated`.
    #[test]
    fn sign_up_data_is_opt_in_per_column() {
        let sql = bootstrap();
        assert!(sql.contains(
            "    GRANT SELECT, UPDATE, DELETE ON \"public\".\"users\" TO authenticated;\n"
        ));
        assert!(!sql.contains("INSERT, UPDATE, DELETE ON \"public\".\"users\" TO authenticated"));
        let revoke = sql
            .find("pg_catalog.aclexplode(c.relacl)")
            .expect("the table-level INSERT check");
        let block = &sql[revoke..revoke + sql[revoke..].find("END $$;").unwrap()];
        assert!(block.contains("a.privilege_type OPERATOR(pg_catalog.=) 'INSERT'"));
        assert!(block.contains("ARRAY['anon', 'authenticated']::pg_catalog.name[]"));
        assert!(block.contains("REVOKE INSERT ON %s FROM %I"));
        // Outside the create-only block: every run checks it.
        assert!(
            sql.find("CREATE UNIQUE INDEX IF NOT EXISTS cheladb_users_email_key")
                .unwrap()
                < revoke
        );
        let f = function_text(&sql, "app_users_upsert");
        assert_eq!(f.matches("has_column_privilege(").count(), 1);
        assert!(f.contains(
            "AND has_column_privilege('authenticated', a.attrelid, a.attname::text, 'INSERT')"
        ));
    }

    /// The bootstrap's own-row policies call `cheladb.user_id()`: left behind, they'd make a
    /// later data API removal (which drops the claim helpers) fail with 2BP01. The teardown
    /// drops them on the users table, inside the loop over the config row, before the config
    /// table goes.
    #[test]
    fn teardown_drops_the_own_row_policies() {
        let sql = USERS_TEARDOWN_SQL;
        for policy in ["cheladb_users_select_own", "cheladb_users_update_own"] {
            let drop = format!(
                "        EXECUTE pg_catalog.format('DROP POLICY IF EXISTS {policy} ON %I.%I', cfg.schema_name, cfg.table_name);\n"
            );
            let at = sql.find(&drop).unwrap_or_else(|| panic!("{policy} isn't dropped"));
            assert!(at < sql.find("    END LOOP;\n").unwrap(), "{policy}");
            assert!(at < sql.find("DROP TABLE IF EXISTS cheladb.app_users_table;").unwrap());
        }
    }

    /// The role can still be used in another database of the compute: the teardown keeps it
    /// (a notice) rather than rolling everything back.
    #[test]
    fn teardown_keeps_a_role_still_used_elsewhere() {
        let sql = USERS_TEARDOWN_SQL;
        let owned = sql.find("    DROP OWNED BY cheladb_auth_sync;\n").unwrap();
        let drop = sql.find("      DROP ROLE cheladb_auth_sync;\n").unwrap();
        let caught = sql
            .find("    EXCEPTION WHEN dependent_objects_still_exist THEN\n")
            .unwrap();
        assert!(owned < drop && drop < caught);
        assert!(
            sql.contains("RAISE NOTICE 'cheladb: the role cheladb_auth_sync is kept: %', SQLERRM;")
        );
    }

    /// Each script names itself on its first line, so nothing mistakes it for the data API
    /// bootstrap (which starts with `BEGIN;`).
    #[test]
    fn scripts_start_with_their_marker() {
        assert!(bootstrap().starts_with("-- cheladb:users-bootstrap\nBEGIN;\n"));
        assert!(USERS_TEARDOWN_SQL.starts_with("-- cheladb:users-teardown\nBEGIN;\n"));
        assert!(claim_helpers_sql().starts_with("-- cheladb:claim-helpers\nBEGIN;\n"));
    }

    /// I2: the guard runs after the row is written, so its return value is ignored; the
    /// TRUNCATE guard is a statement trigger before it. Both fire in every session mode.
    #[test]
    fn guard_is_an_after_row_trigger() {
        let sql = bootstrap();
        assert!(sql.contains(
            "CREATE OR REPLACE TRIGGER cheladb_users_guard AFTER INSERT OR UPDATE OR DELETE ON \"public\".\"users\"\n  \
             FOR EACH ROW EXECUTE FUNCTION cheladb.users_guard();"
        ));
        assert!(sql.contains(
            "CREATE OR REPLACE TRIGGER cheladb_users_guard_truncate BEFORE TRUNCATE ON \"public\".\"users\"\n  \
             FOR EACH STATEMENT EXECUTE FUNCTION cheladb.users_guard();"
        ));
        let guard = sql
            .find("CREATE OR REPLACE TRIGGER cheladb_users_guard_truncate")
            .unwrap();
        let always = sql
            .find("ENABLE ALWAYS TRIGGER cheladb_users_guard;")
            .unwrap();
        assert!(guard < always);
        assert!(sql.contains("ENABLE ALWAYS TRIGGER cheladb_users_guard_truncate;"));
        let f = function_text(&sql, "users_guard");
        assert!(f.contains("SECURITY INVOKER") && !f.contains("SECURITY DEFINER"));
        assert!(f.contains("IF CURRENT_USER = 'cheladb_auth_sync' THEN"));
        for column in MANAGED_COLUMNS {
            assert!(
                f.contains(&format!("column {column} is managed by Cheladb app users")),
                "{column}"
            );
        }
        assert!(sql.contains("ALTER FUNCTION cheladb.users_guard() OWNER TO CURRENT_USER;"));
    }

    /// I3: sign-up data only reaches columns `authenticated` may insert.
    #[test]
    fn upsert_limits_data_to_columns_authenticated_may_insert() {
        let f = function_text(&bootstrap(), "app_users_upsert");
        assert!(f.contains(
            "AND has_column_privilege('authenticated', a.attrelid, a.attname::text, 'INSERT')"
        ));
        assert!(f.contains("AND a.attname <> ALL (ARRAY['id', 'email', 'email_verified', 'created_at', 'deleted_at']::name[])"));
        assert!(f.contains("SECURITY INVOKER") && !f.contains("SECURITY DEFINER"));
    }

    /// I7: a deleted user's email can sign up again.
    #[test]
    fn email_index_skips_deleted_rows() {
        assert!(bootstrap().contains(
            "CREATE UNIQUE INDEX IF NOT EXISTS cheladb_users_email_key ON \"public\".\"users\" (pg_catalog.lower(email)) WHERE deleted_at IS NULL;\n"
        ));
    }

    #[test]
    fn users_table_exists_sql_prints_a_flag() {
        assert_eq!(
            users_table_exists_sql("public", "users"),
            "SET search_path = pg_catalog, pg_temp; SELECT pg_catalog.to_regclass('\"public\".\"users\"') IS NOT NULL;"
        );
    }

    #[test]
    fn upsert_call_quotes_values_and_null_data() {
        let id = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        let created = Utc.with_ymd_and_hms(2026, 10, 3, 10, 0, 0).unwrap();
        assert_eq!(
            upsert_call_sql(id, "o'neil@example.com", true, created, None),
            "BEGIN; SET LOCAL ROLE cheladb_auth_sync; SELECT cheladb.app_users_upsert(\
             '11111111-2222-3333-4444-555555555555', 'o''neil@example.com', true, '2026-10-03T10:00:00+00:00', NULL); COMMIT;"
        );
        let data = serde_json::json!({"display_name": "it's me"});
        assert_eq!(
            upsert_call_sql(id, "a@example.com", false, created, Some(&data)),
            "BEGIN; SET LOCAL ROLE cheladb_auth_sync; SELECT cheladb.app_users_upsert(\
             '11111111-2222-3333-4444-555555555555', 'a@example.com', false, '2026-10-03T10:00:00+00:00', \
             '{\"display_name\":\"it''s me\"}'); COMMIT;"
        );
    }

    #[test]
    fn gone_call_marks_or_deletes() {
        let id = Uuid::parse_str("11111111-2222-3333-4444-555555555555").unwrap();
        assert_eq!(
            gone_call_sql(id, true),
            "BEGIN; SET LOCAL ROLE cheladb_auth_sync; SELECT cheladb.app_users_gone('11111111-2222-3333-4444-555555555555', true); COMMIT;"
        );
        assert_eq!(
            gone_call_sql(id, false),
            "BEGIN; SET LOCAL ROLE cheladb_auth_sync; SELECT cheladb.app_users_gone('11111111-2222-3333-4444-555555555555', false); COMMIT;"
        );
        assert_eq!(
            USERS_ROWS_SQL,
            "BEGIN; SET LOCAL ROLE cheladb_auth_sync; SELECT cheladb.app_users_rows(); COMMIT;"
        );
    }

    #[test]
    fn valid_identifier_rules() {
        for ok in ["users", "_x", "a1_b2", "app_users", &"a".repeat(63)] {
            assert!(valid_identifier(ok), "{ok}");
        }
        for bad in [
            "",
            "1users",
            "Users",
            "app-users",
            "app users",
            "a\"b",
            "é",
            "a$",
            &"a".repeat(64),
        ] {
            assert!(!valid_identifier(bad), "{bad}");
        }
    }

    #[test]
    fn the_sync_role_and_managed_columns() {
        assert_eq!(USERS_SYNC_ROLE, "cheladb_auth_sync");
        assert_eq!(
            MANAGED_COLUMNS,
            ["id", "email", "email_verified", "created_at", "deleted_at"]
        );
    }
}
