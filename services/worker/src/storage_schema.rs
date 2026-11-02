//! The `storage` schema in a branch's database: object metadata that branches copy for free
//! and that policies written in plain SQL guard (object-storage m3).
//!
//! [`storage_schema_sql`] runs as the superuser right after the data API script, in every
//! database the data API bootstrap reaches (provision, branch create, compute re-creation and
//! reconcile: [`crate::data_api`]). Reconcile reruns the bootstrap wherever
//! [`STORAGE_CHECK_SQL`] (part of [`crate::data_api::schema_check_sql`]) doesn't find the
//! current [`STORAGE_SCHEMA_STAMP`].
//!
//! Who can do what:
//!
//! - `chelabase_admin` (the data API's locked-down NOLOGIN role, no members) owns the schema,
//!   both tables and the `object_ref` type, so the database owner can't `ALTER`, `DISABLE`
//!   or `NO FORCE` row level security, add triggers, or drop the schema.
//! - Row level security is enabled and forced on both tables, with no policies: nothing is
//!   visible or writable until the owner adds policies through `storage.create_policy` /
//!   `storage.drop_policy` (`SECURITY DEFINER`, owned by `chelabase_admin`, executable by the
//!   owner only).
//! - [`STORAGE_ROLE`] (`chelabase_storage`, BYPASSRLS, NOLOGIN for now) is the storage
//!   service's role: the only one granted the [`MANAGED_COLUMNS`] of `storage.objects`, and
//!   the only one the managed-columns trigger lets write them or insert a row.
//! - `anon` and `authenticated` get `USAGE` on the schema (never `CREATE`) and table
//!   privileges that row level security still gates; `authenticated` may write only the
//!   unmanaged columns. `authenticator` gets `USAGE` only (it parses statements before it
//!   switches role).
//! - Owner-level roles (the database owner is a Neon privileged role: `BYPASSRLS`, a member of
//!   `pg_read_all_data` and `pg_write_all_data`) read and write past both RLS and the column
//!   grants, as on any table; the trigger still refuses their writes to the managed columns.
//!
//! The script is one transaction under [`BOOTSTRAP_LOCK_KEY`], safe to run again: every
//! object is created only when missing (or replaced), and the grants on both tables are reset
//! to exactly the list above each run. A `storage` schema that `chelabase_admin` doesn't own
//! (a user's, from before Chelabase reserved the name) is left alone: the script stops at once
//! with [`FOREIGN_SCHEMA_MARK`] and changes nothing.

use crate::data_api::{BOOTSTRAP_LOCK_KEY, LOCK_TIMEOUT_SECS, quote_ident};

/// The schema's name, reserved as a data API default schema and never served by `/data/v1`.
pub const STORAGE_SCHEMA: &str = "storage";

/// The storage service's role.
pub const STORAGE_ROLE: &str = "chelabase_storage";

/// The columns of `storage.objects` only [`STORAGE_ROLE`] writes.
pub const MANAGED_COLUMNS: [&str; 4] = ["bucket_id", "version_id", "size", "etag"];

/// The schema's comment: which version of [`storage_schema_sql`] installed it. Bump the
/// number whenever the script changes what it installs, so reconcile reinstalls.
pub const STORAGE_SCHEMA_STAMP: &str = "chelabase storage schema 1";

/// The script's first line (the fakes' needle).
pub const STORAGE_SCHEMA_MARK: &str = "-- chelabase:storage-schema";

/// The error the script raises, before changing anything, when `storage` belongs to someone
/// else.
pub const FOREIGN_SCHEMA_MARK: &str = "chelabase_storage_foreign";

/// The health check's clause (appended to the data API's): the schema is the current one
/// (`chelabase_admin`'s, stamped [`STORAGE_SCHEMA_STAMP`], both tables `chelabase_admin`'s
/// with row level security enabled and forced), or it belongs to someone else (left alone, so
/// reconcile doesn't rerun the bootstrap for it on every pass).
pub const STORAGE_CHECK_SQL: &str = " AND EXISTS (SELECT FROM pg_catalog.pg_namespace n \
       JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) n.nspowner \
       WHERE n.nspname OPERATOR(pg_catalog.=) 'storage' \
         AND (r.rolname OPERATOR(pg_catalog.<>) 'chelabase_admin' \
           OR (pg_catalog.obj_description(n.oid, 'pg_namespace') OPERATOR(pg_catalog.=) 'chelabase storage schema 1' \
             AND (SELECT pg_catalog.count(*) FROM pg_catalog.pg_class c \
               WHERE c.relnamespace OPERATOR(pg_catalog.=) n.oid AND c.relkind OPERATOR(pg_catalog.=) 'r' \
                 AND c.relname OPERATOR(pg_catalog.=) ANY (ARRAY['buckets', 'objects']::pg_catalog.name[]) \
                 AND c.relowner OPERATOR(pg_catalog.=) r.oid AND c.relrowsecurity AND c.relforcerowsecurity) \
               OPERATOR(pg_catalog.=) 2)))";

/// The install script, for `admin_sql` (the superuser) in one database; `owner_role` is the
/// database owner (the policy helpers' only grantee).
///
/// Plain operators inside the function bodies and table definitions resolve against the
/// pinned `search_path` (`pg_catalog` first) when they're created or run; the `DO` blocks
/// qualify theirs, like the data API script.
pub fn storage_schema_sql(owner_role: &str) -> String {
    let owner = quote_ident(owner_role);
    format!(
        r#"{STORAGE_SCHEMA_MARK}
BEGIN;
SET search_path = pg_catalog, pg_temp;
SET LOCAL lock_timeout = '{LOCK_TIMEOUT_SECS}s';
SELECT pg_catalog.pg_advisory_xact_lock({BOOTSTRAP_LOCK_KEY});
DO $$ BEGIN
  IF EXISTS (SELECT FROM pg_catalog.pg_namespace n JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) n.nspowner
      WHERE n.nspname OPERATOR(pg_catalog.=) 'storage' AND r.rolname OPERATOR(pg_catalog.<>) 'chelabase_admin') THEN
    RAISE EXCEPTION '{FOREIGN_SCHEMA_MARK}: the storage schema is not Chelabase''s; it was left alone';
  END IF;
  IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'chelabase_admin') THEN
    RAISE EXCEPTION 'chelabase_data_api_missing: the data API bootstrap has not run in this database';
  END IF;
  IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'chelabase_storage') THEN
    CREATE ROLE chelabase_storage NOLOGIN NOINHERIT BYPASSRLS;
  END IF;
END $$;
ALTER ROLE chelabase_storage NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOINHERIT BYPASSRLS;
CREATE SCHEMA IF NOT EXISTS storage AUTHORIZATION chelabase_admin;
REVOKE ALL ON SCHEMA storage FROM PUBLIC;
GRANT USAGE ON SCHEMA storage TO anon, authenticated, authenticator, chelabase_storage, {owner};
CREATE TABLE IF NOT EXISTS storage.buckets (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  name text NOT NULL UNIQUE,
  public boolean NOT NULL DEFAULT false,
  file_size_limit bigint CHECK (file_size_limit > 0),
  allowed_mime_types text[],
  created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE IF NOT EXISTS storage.objects (
  id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  bucket_id uuid NOT NULL REFERENCES storage.buckets (id),
  name text NOT NULL,
  owner uuid,
  metadata jsonb NOT NULL DEFAULT '{{}}',
  size bigint,
  content_type text,
  etag text,
  version_id uuid,
  created_at timestamptz NOT NULL DEFAULT now(),
  updated_at timestamptz NOT NULL DEFAULT now(),
  UNIQUE (bucket_id, name)
);
ALTER TABLE storage.buckets OWNER TO chelabase_admin;
ALTER TABLE storage.objects OWNER TO chelabase_admin;
ALTER TABLE storage.buckets ENABLE ROW LEVEL SECURITY;
ALTER TABLE storage.buckets FORCE ROW LEVEL SECURITY;
ALTER TABLE storage.objects ENABLE ROW LEVEL SECURITY;
ALTER TABLE storage.objects FORCE ROW LEVEL SECURITY;
REVOKE ALL ON storage.buckets, storage.objects FROM PUBLIC, anon, authenticated, authenticator, chelabase_storage, {owner};
GRANT SELECT ON storage.buckets, storage.objects TO anon, authenticated;
GRANT INSERT (id, name, public, file_size_limit, allowed_mime_types), UPDATE (name, public, file_size_limit, allowed_mime_types), DELETE ON storage.buckets TO authenticated;
GRANT INSERT (id, name, owner, metadata, content_type), UPDATE (name, owner, metadata, content_type), DELETE ON storage.objects TO authenticated;
GRANT SELECT, INSERT, UPDATE, DELETE ON storage.buckets, storage.objects TO chelabase_storage;
CREATE OR REPLACE FUNCTION storage.protect_objects() RETURNS trigger
  LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS
$f$
BEGIN
  IF current_user = 'chelabase_storage' THEN
    RETURN NULL;
  END IF;
  IF TG_OP = 'INSERT' THEN
    RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE = 'rows of storage.objects are created by Chelabase storage';
  END IF;
  IF NEW.bucket_id IS DISTINCT FROM OLD.bucket_id OR NEW.version_id IS DISTINCT FROM OLD.version_id
      OR NEW.size IS DISTINCT FROM OLD.size OR NEW.etag IS DISTINCT FROM OLD.etag THEN
    RAISE EXCEPTION USING ERRCODE = '42501', MESSAGE = 'columns bucket_id, version_id, size and etag of storage.objects are managed by Chelabase storage';
  END IF;
  RETURN NULL;
END
$f$;
ALTER FUNCTION storage.protect_objects() OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION storage.protect_objects() FROM PUBLIC;
CREATE OR REPLACE TRIGGER protect_objects AFTER INSERT OR UPDATE ON storage.objects
  FOR EACH ROW EXECUTE FUNCTION storage.protect_objects();
ALTER TABLE storage.objects ENABLE ALWAYS TRIGGER protect_objects;
DO $$ BEGIN
  IF pg_catalog.to_regtype('storage.object_ref') IS NULL THEN
    CREATE TYPE storage.object_ref AS (bucket_id uuid, name text);
  END IF;
END $$;
ALTER TYPE storage.object_ref OWNER TO chelabase_admin;
CREATE OR REPLACE FUNCTION storage.foldername(name text) RETURNS text[]
  LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE SET search_path = pg_catalog, pg_temp AS
$f$ SELECT (string_to_array(name, '/'))[1:cardinality(string_to_array(name, '/')) - 1] $f$;
CREATE OR REPLACE FUNCTION storage.filename(name text) RETURNS text
  LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE SET search_path = pg_catalog, pg_temp AS
$f$ SELECT split_part(name, '/', -1) $f$;
CREATE OR REPLACE FUNCTION storage.extension(name text) RETURNS text
  LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE SET search_path = pg_catalog, pg_temp AS
$f$ SELECT CASE WHEN strpos(split_part(name, '/', -1), '.') > 0 THEN split_part(split_part(name, '/', -1), '.', -1) ELSE '' END $f$;
CREATE OR REPLACE FUNCTION storage.object_ref(bucket_id uuid, name text) RETURNS storage.object_ref
  LANGUAGE sql IMMUTABLE PARALLEL SAFE SET search_path = pg_catalog, pg_temp AS
$f$ SELECT ROW(bucket_id, name)::storage.object_ref $f$;
ALTER FUNCTION storage.foldername(text) OWNER TO CURRENT_USER;
ALTER FUNCTION storage.filename(text) OWNER TO CURRENT_USER;
ALTER FUNCTION storage.extension(text) OWNER TO CURRENT_USER;
ALTER FUNCTION storage.object_ref(uuid, text) OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION storage.foldername(text), storage.filename(text), storage.extension(text), storage.object_ref(uuid, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION storage.foldername(text), storage.filename(text), storage.extension(text), storage.object_ref(uuid, text) TO PUBLIC;
CREATE OR REPLACE FUNCTION storage.create_policy(policy_name name, table_name name, command text DEFAULT 'ALL',
    roles name[] DEFAULT ARRAY['public']::name[], using_expr text DEFAULT NULL, check_expr text DEFAULT NULL,
    permissive boolean DEFAULT true) RETURNS void
  LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS
$f$
DECLARE
  targets text;
  statement text;
BEGIN
  IF table_name IS NULL OR table_name NOT IN ('buckets', 'objects') THEN
    RAISE EXCEPTION USING ERRCODE = '22023', MESSAGE = 'storage.create_policy: the table must be buckets or objects';
  END IF;
  IF policy_name IS NULL OR permissive IS NULL OR command IS NULL THEN
    RAISE EXCEPTION USING ERRCODE = '22023', MESSAGE = 'storage.create_policy: policy_name, command and permissive are required';
  END IF;
  IF pg_catalog.upper(command) NOT IN ('ALL', 'SELECT', 'INSERT', 'UPDATE', 'DELETE') THEN
    RAISE EXCEPTION USING ERRCODE = '22023', MESSAGE = 'storage.create_policy: command must be ALL, SELECT, INSERT, UPDATE or DELETE';
  END IF;
  IF using_expr IS NULL AND check_expr IS NULL THEN
    RAISE EXCEPTION USING ERRCODE = '22023', MESSAGE = 'storage.create_policy: give using_expr, check_expr or both';
  END IF;
  -- The expressions are pasted into the statement: without a ";" they can't end it and start
  -- another (each sits on lines of its own, so a "--" comment ends with its line).
  IF pg_catalog.strpos(using_expr, ';') > 0 OR pg_catalog.strpos(check_expr, ';') > 0 THEN
    RAISE EXCEPTION USING ERRCODE = '22023', MESSAGE = 'storage.create_policy: a policy expression can''t contain ";"';
  END IF;
  IF roles IS NULL OR pg_catalog.cardinality(roles) = 0 OR pg_catalog.array_position(roles, NULL) IS NOT NULL THEN
    RAISE EXCEPTION USING ERRCODE = '22023', MESSAGE = 'storage.create_policy: name at least one role (or public)';
  END IF;
  SELECT pg_catalog.string_agg(CASE WHEN r = 'public' THEN 'PUBLIC' ELSE pg_catalog.quote_ident(r) END, ', ')
    INTO targets FROM pg_catalog.unnest(roles) AS r;
  statement := pg_catalog.format('CREATE POLICY %I ON storage.%I AS %s FOR %s TO %s', policy_name, table_name,
    CASE WHEN permissive THEN 'PERMISSIVE' ELSE 'RESTRICTIVE' END, pg_catalog.upper(command), targets);
  IF using_expr IS NOT NULL THEN
    statement := statement || E'\n  USING (\n' || using_expr || E'\n)';
  END IF;
  IF check_expr IS NOT NULL THEN
    statement := statement || E'\n  WITH CHECK (\n' || check_expr || E'\n)';
  END IF;
  EXECUTE statement;
END
$f$;
CREATE OR REPLACE FUNCTION storage.drop_policy(policy_name name, table_name name) RETURNS void
  LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS
$f$
BEGIN
  IF table_name IS NULL OR table_name NOT IN ('buckets', 'objects') THEN
    RAISE EXCEPTION USING ERRCODE = '22023', MESSAGE = 'storage.drop_policy: the table must be buckets or objects';
  END IF;
  EXECUTE pg_catalog.format('DROP POLICY %I ON storage.%I', policy_name, table_name);
END
$f$;
COMMENT ON FUNCTION storage.create_policy(name, name, text, name[], text, text, boolean) IS
  'Creates a policy on storage.buckets or storage.objects (the database owner only). The expressions are parsed with search_path = pg_catalog, pg_temp: qualify your own functions and tables.';
ALTER FUNCTION storage.create_policy(name, name, text, name[], text, text, boolean) OWNER TO chelabase_admin;
ALTER FUNCTION storage.drop_policy(name, name) OWNER TO chelabase_admin;
REVOKE ALL ON FUNCTION storage.create_policy(name, name, text, name[], text, text, boolean) FROM PUBLIC;
REVOKE ALL ON FUNCTION storage.drop_policy(name, name) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION storage.create_policy(name, name, text, name[], text, text, boolean) TO {owner};
GRANT EXECUTE ON FUNCTION storage.drop_policy(name, name) TO {owner};
COMMENT ON SCHEMA storage IS '{STORAGE_SCHEMA_STAMP}';
COMMIT;
"#
    )
}

/// Whether the script stopped because `storage` belongs to someone else (from its error).
pub fn foreign_schema(error: &str) -> bool {
    error.contains(&format!("ERROR:  {FOREIGN_SCHEMA_MARK}:"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_api::schema_check_sql;
    use crate::fakes::{FakeDbRow, entries, ready_database, worker};
    use chelabase_metadata::DataApiState;
    use chelabase_types::BranchId;

    fn sql() -> String {
        storage_schema_sql("postgres")
    }

    /// The text between `start` and the next `end`, both included.
    fn between<'a>(sql: &'a str, start: &str, end: &str) -> &'a str {
        let from = sql
            .find(start)
            .unwrap_or_else(|| panic!("{start:?} not in {sql}"));
        let rest = &sql[from..];
        let to = rest
            .find(end)
            .unwrap_or_else(|| panic!("{end:?} after {start:?} not in {sql}"));
        &rest[..to + end.len()]
    }

    #[test]
    fn storage_schema_script_is_one_transaction_under_the_bootstrap_lock() {
        let sql = sql();
        let head = format!(
            "{STORAGE_SCHEMA_MARK}\nBEGIN;\nSET search_path = pg_catalog, pg_temp;\nSET LOCAL lock_timeout = '{LOCK_TIMEOUT_SECS}s';\nSELECT pg_catalog.pg_advisory_xact_lock({BOOTSTRAP_LOCK_KEY});\n"
        );
        assert!(sql.starts_with(&head), "{sql}");
        assert!(sql.trim_end().ends_with("COMMIT;"), "{sql}");
        assert_eq!(sql.matches("BEGIN;").count(), 1, "{sql}");
        assert_eq!(sql.matches("COMMIT;").count(), 1, "{sql}");
    }

    /// AC 2: every object is made only when missing, so a second run changes nothing but
    /// the grants it resets to the same list.
    #[test]
    fn storage_schema_script_is_idempotent() {
        let sql = sql();
        let mut creates = 0;
        for line in sql.lines().map(str::trim_start) {
            if !line.starts_with("CREATE ") {
                continue;
            }
            creates += 1;
            assert!(
                line.starts_with("CREATE SCHEMA IF NOT EXISTS ")
                    || line.starts_with("CREATE TABLE IF NOT EXISTS ")
                    || line.starts_with("CREATE OR REPLACE FUNCTION ")
                    || line.starts_with("CREATE OR REPLACE TRIGGER ")
                    // Inside a block that checks first.
                    || line.starts_with("CREATE ROLE chelabase_storage ")
                    || line.starts_with("CREATE TYPE storage.object_ref "),
                "not idempotent: {line}"
            );
        }
        assert!(creates >= 10, "{sql}");
        assert!(sql.contains(
            "IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'chelabase_storage') THEN\n    CREATE ROLE chelabase_storage "
        ), "{sql}");
        assert!(sql.contains(
            "IF pg_catalog.to_regtype('storage.object_ref') IS NULL THEN\n    CREATE TYPE storage.object_ref "
        ), "{sql}");
        // No DROP of anything the user may depend on (rows, the type, the tables).
        for line in sql.lines() {
            assert!(
                !line.contains("DROP TABLE")
                    && !line.contains("DROP TYPE")
                    && !line.contains("DROP SCHEMA")
                    && !line.contains("TRUNCATE"),
                "{line}"
            );
        }
    }

    /// AC 1: both tables, `chelabase_admin`'s, with row level security enabled and forced
    /// and no policy; the shape the later milestones read.
    #[test]
    fn storage_schema_tables_are_the_admins_with_forced_rls() {
        let sql = sql();
        assert!(
            sql.contains("CREATE SCHEMA IF NOT EXISTS storage AUTHORIZATION chelabase_admin;"),
            "{sql}"
        );
        for table in ["storage.buckets", "storage.objects"] {
            for statement in [
                format!("ALTER TABLE {table} OWNER TO chelabase_admin;"),
                format!("ALTER TABLE {table} ENABLE ROW LEVEL SECURITY;"),
                format!("ALTER TABLE {table} FORCE ROW LEVEL SECURITY;"),
            ] {
                assert!(sql.contains(&statement), "{statement}");
            }
        }
        assert!(
            sql.contains("ALTER TYPE storage.object_ref OWNER TO chelabase_admin;"),
            "{sql}"
        );
        assert!(
            !sql.lines()
                .any(|l| l.trim_start().starts_with("CREATE POLICY")),
            "no default policy: {sql}"
        );
        let buckets = between(&sql, "CREATE TABLE IF NOT EXISTS storage.buckets (", ");");
        for column in [
            "id uuid",
            "name text NOT NULL UNIQUE",
            "public boolean",
            "file_size_limit bigint",
            "allowed_mime_types text[]",
            "created_at timestamptz",
        ] {
            assert!(buckets.contains(column), "{column}: {buckets}");
        }
        let objects = between(&sql, "CREATE TABLE IF NOT EXISTS storage.objects (", ");");
        for column in [
            "id uuid",
            "bucket_id uuid NOT NULL REFERENCES storage.buckets (id)",
            "name text NOT NULL",
            "owner uuid",
            "metadata jsonb",
            "size bigint",
            "content_type text",
            "etag text",
            "version_id uuid",
            "created_at timestamptz",
            "updated_at timestamptz",
            "UNIQUE (bucket_id, name)",
        ] {
            assert!(objects.contains(column), "{column}: {objects}");
        }
        assert!(
            sql.contains(&format!(
                "COMMENT ON SCHEMA storage IS '{STORAGE_SCHEMA_STAMP}';\nCOMMIT;"
            )),
            "the stamp comes last: {sql}"
        );
    }

    /// The storage service's role: BYPASSRLS and nothing owner-level, re-asserted each run.
    #[test]
    fn storage_schema_role_bypasses_rls_and_nothing_else() {
        let sql = sql();
        assert!(
            sql.contains("CREATE ROLE chelabase_storage NOLOGIN NOINHERIT BYPASSRLS;"),
            "{sql}"
        );
        assert!(sql.contains("ALTER ROLE chelabase_storage NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOINHERIT BYPASSRLS;"), "{sql}");
        assert_eq!(STORAGE_ROLE, "chelabase_storage");
    }

    /// AC 3 and 5: the grants are reset each run and then given exactly: `USAGE` (never
    /// `CREATE`) on the schema, `SELECT` for `anon`, the unmanaged columns only for
    /// `authenticated`, everything for the storage role. Nobody gets `TRUNCATE`, `TRIGGER`,
    /// `REFERENCES` or a grant option.
    #[test]
    fn storage_schema_grants_keep_managed_columns_for_the_storage_role() {
        let sql = sql();
        assert!(
            sql.contains("REVOKE ALL ON SCHEMA storage FROM PUBLIC;"),
            "{sql}"
        );
        assert!(sql.contains("GRANT USAGE ON SCHEMA storage TO anon, authenticated, authenticator, chelabase_storage, \"postgres\";"), "{sql}");
        let reset = "REVOKE ALL ON storage.buckets, storage.objects FROM PUBLIC, anon, authenticated, authenticator, chelabase_storage, \"postgres\";";
        let reset_at = sql.find(reset).unwrap_or_else(|| panic!("{reset}: {sql}"));
        let grants: Vec<&str> = sql
            .lines()
            .filter(|l| l.starts_with("GRANT ") && l.contains(" ON storage."))
            .collect();
        assert!(!grants.is_empty());
        for grant in &grants {
            assert!(
                sql.find(grant).unwrap() > reset_at,
                "granted before the reset: {grant}"
            );
            assert!(
                !grant.contains("TRUNCATE")
                    && !grant.contains("TRIGGER")
                    && !grant.contains("REFERENCES")
                    && !grant.contains("GRANT OPTION")
                    && !grant.contains(" ALL "),
                "{grant}"
            );
            let to = grant.rsplit_once(" TO ").unwrap().1;
            if to != "chelabase_storage;" {
                let words: Vec<&str> = grant
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .collect();
                for column in MANAGED_COLUMNS {
                    assert!(
                        !words.contains(&column),
                        "{column} granted beyond the storage role: {grant}"
                    );
                }
                // A table-wide INSERT or UPDATE would cover the managed columns.
                let privileges = grant
                    .strip_prefix("GRANT ")
                    .unwrap()
                    .split(" ON ")
                    .next()
                    .unwrap();
                for privilege in privileges.split(", ") {
                    assert!(
                        !(privilege == "INSERT" || privilege == "UPDATE"),
                        "table-wide {privilege}: {grant}"
                    );
                }
                assert!(!to.contains("authenticator"), "{grant}");
            }
        }
        assert!(
            grants.contains(
                &"GRANT SELECT ON storage.buckets, storage.objects TO anon, authenticated;"
            ),
            "{grants:?}"
        );
        assert!(grants.contains(&"GRANT SELECT, INSERT, UPDATE, DELETE ON storage.buckets, storage.objects TO chelabase_storage;"), "{grants:?}");
        assert!(
            grants.iter().any(
                |g| g.contains("UPDATE (name, owner, metadata, content_type)")
                    && g.contains("storage.objects TO authenticated")
            ),
            "{grants:?}"
        );
    }

    /// AC 3: the trigger refuses every write to a managed column, and every insert, unless
    /// the statement runs as the storage role; `ENABLE ALWAYS`, so replica mode doesn't skip
    /// it.
    #[test]
    fn storage_schema_trigger_guards_the_managed_columns() {
        let sql = sql();
        let body = between(
            &sql,
            "CREATE OR REPLACE FUNCTION storage.protect_objects()",
            "$f$;",
        );
        assert!(
            body.contains(
                "LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp"
            ),
            "{body}"
        );
        assert!(
            body.contains(
                "IF current_user = 'chelabase_storage' THEN\n    RETURN NULL;\n  END IF;"
            ),
            "{body}"
        );
        assert!(body.contains("IF TG_OP = 'INSERT' THEN"), "{body}");
        for column in MANAGED_COLUMNS {
            assert!(
                body.contains(&format!("NEW.{column} IS DISTINCT FROM OLD.{column}")),
                "{column}: {body}"
            );
        }
        assert!(body.contains("ERRCODE = '42501'"), "{body}");
        assert!(sql.contains("CREATE OR REPLACE TRIGGER protect_objects AFTER INSERT OR UPDATE ON storage.objects\n  FOR EACH ROW EXECUTE FUNCTION storage.protect_objects();"), "{sql}");
        assert!(
            sql.contains("ALTER TABLE storage.objects ENABLE ALWAYS TRIGGER protect_objects;"),
            "{sql}"
        );
        assert!(
            sql.contains("ALTER FUNCTION storage.protect_objects() OWNER TO CURRENT_USER;"),
            "{sql}"
        );
    }

    /// The policy helpers: `SECURITY DEFINER` as the tables' owner, with a pinned search path,
    /// only for the database owner, only on the two tables, and with no way to end the
    /// `CREATE POLICY` statement and start another (no `;` in an expression, each expression
    /// on lines of its own so a `--` comment ends with its line).
    #[test]
    fn storage_schema_policy_helpers_run_as_the_admin_for_the_owner_only() {
        let sql = sql();
        for (function, signature) in [
            (
                "storage.create_policy",
                "storage.create_policy(name, name, text, name[], text, text, boolean)",
            ),
            ("storage.drop_policy", "storage.drop_policy(name, name)"),
        ] {
            let body = between(
                &sql,
                &format!("CREATE OR REPLACE FUNCTION {function}("),
                "$f$;",
            );
            assert!(
                body.contains(
                    "LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp"
                ),
                "{body}"
            );
            assert!(
                body.contains(
                    "IF table_name IS NULL OR table_name NOT IN ('buckets', 'objects') THEN"
                ),
                "{body}"
            );
            assert!(body.contains("ON storage.%I"), "{body}");
            assert!(
                sql.contains(&format!(
                    "ALTER FUNCTION {signature} OWNER TO chelabase_admin;"
                )),
                "{signature}"
            );
            assert!(
                sql.contains(&format!("REVOKE ALL ON FUNCTION {signature} FROM PUBLIC;")),
                "{signature}"
            );
            assert!(
                sql.contains(&format!(
                    "GRANT EXECUTE ON FUNCTION {signature} TO \"postgres\";"
                )),
                "{signature}"
            );
        }
        let create = between(
            &sql,
            "CREATE OR REPLACE FUNCTION storage.create_policy(",
            "$f$;",
        );
        assert!(
            create.contains(
                "pg_catalog.strpos(using_expr, ';') > 0 OR pg_catalog.strpos(check_expr, ';') > 0"
            ),
            "{create}"
        );
        assert!(create.contains("IF pg_catalog.upper(command) NOT IN ('ALL', 'SELECT', 'INSERT', 'UPDATE', 'DELETE') THEN"), "{create}");
        assert!(
            create.contains("E'\\n  USING (\\n' || using_expr || E'\\n)'"),
            "{create}"
        );
        assert!(
            create.contains("E'\\n  WITH CHECK (\\n' || check_expr || E'\\n)'"),
            "{create}"
        );
        assert!(create.contains("pg_catalog.quote_ident(r)"), "{create}");
    }

    /// The helpers m5 and m11 use: pure functions anyone may call.
    #[test]
    fn storage_schema_helpers_are_pure_and_public() {
        let sql = sql();
        for signature in [
            "storage.foldername(text)",
            "storage.filename(text)",
            "storage.extension(text)",
            "storage.object_ref(uuid, text)",
        ] {
            let name = signature.split('(').next().unwrap();
            let head = between(&sql, &format!("CREATE OR REPLACE FUNCTION {name}("), " AS");
            assert!(
                head.contains("IMMUTABLE")
                    && head.contains("SET search_path = pg_catalog, pg_temp"),
                "{head}"
            );
            assert!(!head.contains("SECURITY DEFINER"), "{head}");
            assert!(
                sql.contains(&format!(
                    "ALTER FUNCTION {signature} OWNER TO CURRENT_USER;"
                )),
                "{signature}"
            );
        }
        assert!(sql.contains("GRANT EXECUTE ON FUNCTION storage.foldername(text), storage.filename(text), storage.extension(text), storage.object_ref(uuid, text) TO PUBLIC;"), "{sql}");
        assert!(
            sql.contains("CREATE TYPE storage.object_ref AS (bucket_id uuid, name text);"),
            "{sql}"
        );
    }

    /// A `storage` schema someone else owns stops the script before its first change.
    #[test]
    fn storage_schema_foreign_schema_stops_the_script_first() {
        let sql = sql();
        let check = sql.find(FOREIGN_SCHEMA_MARK).expect("the foreign check");
        let lock = sql.find("pg_advisory_xact_lock").unwrap();
        let first_change = sql.find("CREATE ROLE").unwrap();
        assert!(lock < check && check < first_change, "{sql}");
        assert!(
            between(
                &sql,
                "IF EXISTS (SELECT FROM pg_catalog.pg_namespace n",
                "END IF;"
            )
            .contains("r.rolname OPERATOR(pg_catalog.<>) 'chelabase_admin'")
        );
        assert!(foreign_schema(&format!(
            "psql in compute ep-1 exited with 3: ERROR:  {FOREIGN_SCHEMA_MARK}: the storage schema is not Chelabase's"
        )));
        assert!(!foreign_schema(
            "psql in compute ep-1 exited with 3: ERROR:  permission denied"
        ));
    }

    /// AC 2: reconcile's check fails until the current version is installed.
    #[test]
    fn storage_schema_is_part_of_the_health_check() {
        let check = schema_check_sql("public");
        assert!(!STORAGE_CHECK_SQL.is_empty());
        assert!(check.ends_with(STORAGE_CHECK_SQL), "{check}");
        let stamp = format!("'{STORAGE_SCHEMA_STAMP}'");
        for part in [
            stamp.as_str(),
            "relforcerowsecurity",
            "relrowsecurity",
            "'chelabase_admin'",
            "'buckets'",
            "'objects'",
        ] {
            assert!(
                STORAGE_CHECK_SQL.contains(part),
                "{part}: {STORAGE_CHECK_SQL}"
            );
        }
    }

    /// AC 1: provisioning installs it right after the data API script, in each served
    /// database.
    #[tokio::test]
    async fn storage_schema_is_installed_after_the_data_api_in_each_served_database() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let ep = main.endpoint_id.clone().unwrap();
        let data_api = entries(&journal)
            .iter()
            .position(|e| e.starts_with(&format!("compute.sql {ep} app BEGIN;")))
            .expect("the data API script");
        let storage = entries(&journal)
            .iter()
            .position(|e| e == &format!("compute.sql {ep} app {STORAGE_SCHEMA_MARK}"))
            .expect("the storage script");
        assert!(data_api < storage, "{:?}", entries(&journal));
        let scripts: Vec<String> = w
            .computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, _, s)| s.starts_with(STORAGE_SCHEMA_MARK))
            .map(|(_, _, s)| s.clone())
            .collect();
        assert_eq!(scripts, [storage_schema_sql("postgres")]);

        w.metadata.state().db_rows.insert(
            main.id,
            vec![
                FakeDbRow::new("app", DataApiState::On, "public"),
                FakeDbRow::new("reports", DataApiState::On, "public"),
            ],
        );
        w.computes.sql.lock().unwrap().clear();
        w.bootstrap_data_api(&main, "app").await.unwrap();
        let databases: Vec<String> = w
            .computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, _, s)| s.starts_with(STORAGE_SCHEMA_MARK))
            .map(|(_, d, _)| d.clone())
            .collect();
        assert_eq!(databases, ["app", "reports"]);
    }

    /// A user's own `storage` schema is left alone: the bootstrap still succeeds. Any other
    /// failure of the script fails it, like the data API script's.
    #[tokio::test]
    async fn storage_schema_failures_fail_the_bootstrap_unless_the_schema_is_foreign() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        w.computes.fail_in_with("app", STORAGE_SCHEMA_MARK, &format!("psql in compute ep exited with 3: ERROR:  {FOREIGN_SCHEMA_MARK}: the storage schema is not Chelabase's"));
        w.bootstrap_data_api(&main, "app").await.unwrap();

        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        w.computes.fail_in_with(
            "app",
            STORAGE_SCHEMA_MARK,
            "psql in compute ep exited with 3: ERROR:  permission denied",
        );
        let err = format!(
            "{:#}",
            w.bootstrap_data_api(&main, "app").await.unwrap_err()
        );
        assert!(
            err.contains("installing the storage schema") && err.contains("permission denied"),
            "{err}"
        );
    }

    /// No secret key may use the storage role.
    #[tokio::test]
    async fn storage_schema_role_is_reserved_for_keys() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        assert_eq!(
            w.check_data_role(BranchId::from(id), STORAGE_ROLE).await,
            crate::data_api::RoleCheck::Refused("the role chelabase_storage is reserved".into())
        );
    }

    /// The merge's exclusion list names the schema (`chelabase-query`'s own test covers the
    /// data API's reserved list).
    #[test]
    fn storage_schema_is_excluded_from_merge() {
        assert!(chelabase_schema::exclusions::EXCLUDED_SCHEMAS.contains(&STORAGE_SCHEMA));
    }
}
