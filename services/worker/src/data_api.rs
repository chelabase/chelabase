//! The data API's bootstrap inside a branch's database (the `cheladb` machinery schema, the
//! `authenticator` login, default privileges and the branch's public key), and the check a
//! secret key's role must pass.

use anyhow::{Context, bail};
use cheladb_metadata::{BranchKind, BranchRecord, BranchStatus, MetadataError, NewDataKey};
use cheladb_nats::{CheckDataRoleReply, CheckDataRoleRequest};
use cheladb_neon::ComputeBackend;
use cheladb_secrets::{Purpose, open_credential, seal_credential};
use cheladb_types::BranchId;
use cheladb_types::data_key::{self, DataKeyKind};
use tracing::{info, warn};
use uuid::Uuid;

use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};
use crate::{OWNER_ROLE, Worker, credentials};

/// The login role the data API connects as; never a secret key's role.
const AUTHENTICATOR: &str = "authenticator";
/// Whether a branch's data API machinery is intact (psql prints `t` or `f`): the `cheladb`
/// schema, `authenticator` able to log in with `USAGE` on it and `SELECT` on
/// `schema_version`, and the three event triggers present and enabled. Anything else is
/// what the bootstrap repairs. Operators are schema-qualified (it runs as the superuser in
/// a database the owner controls).
pub const SCHEMA_CHECK_SQL: &str = "SELECT to_regclass('cheladb.schema_version') IS NOT NULL \
     AND EXISTS (SELECT FROM pg_catalog.pg_roles r \
       JOIN pg_catalog.pg_namespace n ON n.nspname OPERATOR(pg_catalog.=) 'cheladb' \
       JOIN pg_catalog.pg_class c ON c.relnamespace OPERATOR(pg_catalog.=) n.oid \
         AND c.relname OPERATOR(pg_catalog.=) 'schema_version' \
       WHERE r.rolname OPERATOR(pg_catalog.=) 'authenticator' AND r.rolcanlogin \
         AND pg_catalog.has_schema_privilege(r.oid, n.oid, 'USAGE') \
         AND pg_catalog.has_table_privilege(r.oid, c.oid, 'SELECT')) \
     AND (SELECT pg_catalog.count(*) FROM pg_catalog.pg_event_trigger \
       WHERE evtname OPERATOR(pg_catalog.=) ANY (ARRAY['cheladb_schema_version_ddl', \
         'cheladb_schema_version_drop', 'cheladb_extension_functions']::pg_catalog.name[]) \
         AND evtenabled OPERATOR(pg_catalog.<>) 'D') OPERATOR(pg_catalog.=) 3";

/// `s` as an SQL string literal (`quote_literal`): single quotes doubled.
fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `s` as a quoted SQL identifier (`quote_ident`, always quoted): double quotes doubled.
fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// `^[A-Za-z_][A-Za-z0-9_$]{0,62}$`.
pub fn valid_role_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    match bytes.split_first() {
        Some((first, rest)) => {
            rest.len() <= 62
                && (first.is_ascii_alphabetic() || *first == b'_')
                && rest
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'$')
        }
        None => false,
    }
}

/// The idempotent script run as cloud_admin in a branch's database after every compute
/// (re)creation and from reconcile: the `cheladb` machinery schema and its event trigger,
/// `authenticator` made `LOGIN` with the branch's password, the owner's default privileges in
/// `public`, and (on the database's first bootstrap only) the same grants on what exists.
///
/// The event-trigger functions are owned by the superuser running the script (Neon refuses
/// to let a superuser run a function a non-superuser owns, and the triggers fire for
/// cloud_admin's DDL too), `SECURITY DEFINER` with every name schema-qualified. The owner's
/// default privilege revokes `EXECUTE` from PUBLIC on every function it creates, including an
/// extension's; `grant_extension_functions` restores Postgres's default for the functions a
/// `CREATE`/`ALTER EXTENSION` adds (the data API never exposes them as calls).
///
/// `grant_extension_functions` runs as the invoker and grants only on functions owned by
/// `owner_role` (never a superuser's), so a superuser's `CREATE EXTENSION` keeps the
/// extension's own revokes. It's dormant on Neon today: a trusted extension's script runs
/// as the bootstrap superuser, so its functions are owned by cloud_admin and keep
/// Postgres's PUBLIC `EXECUTE`. It's kept for a Neon where owners own their extensions'
/// functions, when the owner's default privilege would strip PUBLIC from them.
pub fn data_api_sql(owner_role: &str, authenticator_password: &str) -> String {
    let password = quote_literal(authenticator_password);
    let owner = quote_literal(owner_role);
    format!(
        "SET search_path = pg_catalog, pg_temp;
DO $$ BEGIN
  IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'cheladb_admin') THEN CREATE ROLE cheladb_admin NOLOGIN; END IF;
END $$;
DO $$ DECLARE
  admin pg_catalog.oid := (SELECT oid FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'cheladb_admin');
  trusted pg_catalog.oid[] := ARRAY[admin, (SELECT oid FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) CURRENT_USER)];
  ns pg_catalog.oid := (SELECT oid FROM pg_catalog.pg_namespace WHERE nspname OPERATOR(pg_catalog.=) 'cheladb');
  members pg_catalog.oid[] := ARRAY(SELECT member FROM pg_catalog.pg_auth_members WHERE roleid OPERATOR(pg_catalog.=) admin);
  joined boolean := pg_catalog.cardinality(members) OPERATOR(pg_catalog.>) 0;
  tampered boolean := EXISTS (SELECT FROM pg_catalog.pg_authid a WHERE a.oid OPERATOR(pg_catalog.=) admin
    AND (a.rolcanlogin OR a.rolsuper OR a.rolcreaterole OR a.rolcreatedb OR a.rolreplication OR a.rolbypassrls
      OR a.rolpassword IS NOT NULL OR a.rolconnlimit OPERATOR(pg_catalog.<>) ALL (ARRAY[-1, 0])))
    OR EXISTS (SELECT FROM pg_catalog.pg_db_role_setting WHERE setrole OPERATOR(pg_catalog.=) admin);
  m record;
  aside pg_catalog.text;
BEGIN
  ALTER ROLE cheladb_admin NOLOGIN NOSUPERUSER NOCREATEROLE NOCREATEDB NOREPLICATION NOBYPASSRLS NOINHERIT PASSWORD NULL CONNECTION LIMIT 0;
  FOR m IN SELECT d.datname FROM pg_catalog.pg_db_role_setting s JOIN pg_catalog.pg_database d ON d.oid OPERATOR(pg_catalog.=) s.setdatabase
    WHERE s.setrole OPERATOR(pg_catalog.=) admin
  LOOP
    EXECUTE pg_catalog.format('ALTER ROLE cheladb_admin IN DATABASE %I RESET ALL', m.datname);
  END LOOP;
  ALTER ROLE cheladb_admin RESET ALL;
  FOR m IN SELECT r.rolname AS member, g.rolname AS grantor FROM pg_catalog.pg_auth_members a
    JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) a.member JOIN pg_catalog.pg_roles g ON g.oid OPERATOR(pg_catalog.=) a.grantor
    WHERE a.roleid OPERATOR(pg_catalog.=) admin
  LOOP
    EXECUTE pg_catalog.format('REVOKE cheladb_admin FROM %I GRANTED BY %I CASCADE', m.member, m.grantor);
  END LOOP;
  PERFORM pg_catalog.set_config('cheladb.former_members', pg_catalog.array_to_string(members, ','), false);
  PERFORM pg_catalog.pg_terminate_backend(a.pid) FROM pg_catalog.pg_stat_activity a
    JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) a.usesysid
    WHERE (a.usesysid OPERATOR(pg_catalog.=) admin OR a.usesysid OPERATOR(pg_catalog.=) ANY (members)) AND NOT r.rolsuper
      AND a.pid OPERATOR(pg_catalog.<>) pg_catalog.pg_backend_pid();
  IF ns IS NOT NULL AND (joined OR tampered
    OR (SELECT nspowner FROM pg_catalog.pg_namespace WHERE oid OPERATOR(pg_catalog.=) ns) OPERATOR(pg_catalog.<>) admin
    OR EXISTS (SELECT FROM pg_catalog.pg_class c WHERE c.relnamespace OPERATOR(pg_catalog.=) ns
      AND (c.relowner OPERATOR(pg_catalog.<>) ALL (trusted) OR c.reloftype OPERATOR(pg_catalog.<>) 0::pg_catalog.oid
        OR c.relkind OPERATOR(pg_catalog.<>) ALL (ARRAY['r', 'i']::pg_catalog.\"char\"[])
        OR c.relname OPERATOR(pg_catalog.<>) ALL (ARRAY['schema_version', 'schema_version_pkey', 'bootstrap', 'bootstrap_pkey']::pg_catalog.name[])))
    OR EXISTS (SELECT FROM pg_catalog.pg_proc p WHERE p.pronamespace OPERATOR(pg_catalog.=) ns
      AND (p.proowner OPERATOR(pg_catalog.<>) ALL (trusted)
        OR p.proname OPERATOR(pg_catalog.<>) ALL (ARRAY['bump_schema_version', 'grant_extension_functions']::pg_catalog.name[])))
    OR EXISTS (SELECT FROM pg_catalog.pg_trigger t JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) t.tgrelid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns AND NOT t.tgisinternal)
    OR EXISTS (SELECT FROM pg_catalog.pg_rewrite w JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) w.ev_class
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns)
    OR EXISTS (SELECT FROM pg_catalog.pg_policy y JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) y.polrelid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns)
    OR EXISTS (SELECT FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) a.attrelid
      JOIN pg_catalog.pg_type ty ON ty.oid OPERATOR(pg_catalog.=) a.atttypid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns AND a.attnum OPERATOR(pg_catalog.>) 0
        AND ty.typnamespace OPERATOR(pg_catalog.<>) 'pg_catalog'::pg_catalog.regnamespace)
    OR EXISTS (SELECT FROM pg_catalog.pg_attrdef d JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) d.adrelid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns AND pg_catalog.pg_get_expr(d.adbin, d.adrelid) OPERATOR(pg_catalog.<>) 'true')
    OR EXISTS (SELECT FROM pg_catalog.pg_constraint k JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) k.conrelid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns AND k.contype OPERATOR(pg_catalog.<>) ALL (ARRAY['p', 'n']::pg_catalog.\"char\"[])
        AND pg_catalog.pg_get_constraintdef(k.oid) OPERATOR(pg_catalog.<>) 'CHECK (one)')
    OR EXISTS (SELECT FROM pg_catalog.pg_index i JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) i.indrelid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns AND (i.indexprs IS NOT NULL OR i.indpred IS NOT NULL
        OR EXISTS (SELECT FROM pg_catalog.pg_opclass o WHERE o.oid OPERATOR(pg_catalog.=) ANY (i.indclass)
          AND o.opcnamespace OPERATOR(pg_catalog.<>) 'pg_catalog'::pg_catalog.regnamespace))))
  THEN
    DROP EVENT TRIGGER IF EXISTS cheladb_schema_version_ddl;
    DROP EVENT TRIGGER IF EXISTS cheladb_schema_version_drop;
    DROP EVENT TRIGGER IF EXISTS cheladb_extension_functions;
    aside := pg_catalog.format('cheladb_quarantine_%s', pg_catalog.floor(pg_catalog.date_part('epoch', pg_catalog.clock_timestamp()) * 1000000)::pg_catalog.int8);
    EXECUTE pg_catalog.format('ALTER SCHEMA cheladb RENAME TO %I', aside);
    PERFORM pg_catalog.set_config('cheladb.quarantined', aside, false);
  END IF;
END $$;
DO $$ BEGIN
  PERFORM pg_catalog.pg_terminate_backend(a.pid) FROM pg_catalog.pg_stat_activity a
    JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) a.usesysid
    WHERE (r.rolname OPERATOR(pg_catalog.=) 'cheladb_admin' OR a.usesysid OPERATOR(pg_catalog.=) ANY
        (pg_catalog.string_to_array(pg_catalog.current_setting('cheladb.former_members', true), ',')::pg_catalog.oid[]))
      AND NOT r.rolsuper AND a.pid OPERATOR(pg_catalog.<>) pg_catalog.pg_backend_pid();
END $$;
CREATE SCHEMA IF NOT EXISTS cheladb AUTHORIZATION cheladb_admin;
REVOKE ALL ON SCHEMA cheladb FROM PUBLIC;
GRANT USAGE ON SCHEMA cheladb TO authenticator;
CREATE TABLE IF NOT EXISTS cheladb.schema_version (one boolean PRIMARY KEY DEFAULT true CHECK (one), version uuid NOT NULL);
INSERT INTO cheladb.schema_version (version) VALUES (pg_catalog.gen_random_uuid()) ON CONFLICT DO NOTHING;
CREATE TABLE IF NOT EXISTS cheladb.bootstrap (one boolean PRIMARY KEY DEFAULT true CHECK (one), existing_granted boolean NOT NULL);
ALTER TABLE cheladb.schema_version OWNER TO cheladb_admin;
ALTER TABLE cheladb.bootstrap OWNER TO cheladb_admin;
REVOKE ALL ON cheladb.schema_version, cheladb.bootstrap FROM PUBLIC;
GRANT SELECT ON cheladb.schema_version TO authenticator;
CREATE OR REPLACE FUNCTION cheladb.bump_schema_version() RETURNS event_trigger
  LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS
  $f$ BEGIN UPDATE cheladb.schema_version SET version = pg_catalog.gen_random_uuid(); END $f$;
ALTER FUNCTION cheladb.bump_schema_version() OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION cheladb.bump_schema_version() FROM PUBLIC;
CREATE OR REPLACE FUNCTION cheladb.grant_extension_functions() RETURNS event_trigger
  LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS
  $f$ DECLARE f pg_catalog.regprocedure; BEGIN
    FOR f IN SELECT d.objid::pg_catalog.regprocedure FROM pg_catalog.pg_event_trigger_ddl_commands() c
      JOIN pg_catalog.pg_depend d ON d.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_extension'::pg_catalog.regclass AND d.refobjid OPERATOR(pg_catalog.=) c.objid
      JOIN pg_catalog.pg_proc p ON p.oid OPERATOR(pg_catalog.=) d.objid
      WHERE c.object_type OPERATOR(pg_catalog.=) 'extension' AND d.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_proc'::pg_catalog.regclass AND d.deptype OPERATOR(pg_catalog.=) 'e'
        AND p.proowner OPERATOR(pg_catalog.=) (SELECT r.oid FROM pg_catalog.pg_roles r WHERE r.rolname OPERATOR(pg_catalog.=) {owner} AND NOT r.rolsuper)
    LOOP
      BEGIN
        EXECUTE pg_catalog.format('GRANT EXECUTE ON ROUTINE %s TO PUBLIC', f);
      EXCEPTION WHEN OTHERS THEN
        RAISE WARNING 'cheladb: EXECUTE on % was not granted to PUBLIC: %', f, SQLERRM;
      END;
    END LOOP;
  EXCEPTION WHEN OTHERS THEN
    RAISE WARNING 'cheladb: extension functions were not granted to PUBLIC: %', SQLERRM;
  END $f$;
ALTER FUNCTION cheladb.grant_extension_functions() OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION cheladb.grant_extension_functions() FROM PUBLIC;
DROP EVENT TRIGGER IF EXISTS cheladb_schema_version_ddl;
DROP EVENT TRIGGER IF EXISTS cheladb_schema_version_drop;
DROP EVENT TRIGGER IF EXISTS cheladb_extension_functions;
CREATE EVENT TRIGGER cheladb_schema_version_ddl ON ddl_command_end EXECUTE FUNCTION cheladb.bump_schema_version();
CREATE EVENT TRIGGER cheladb_schema_version_drop ON sql_drop EXECUTE FUNCTION cheladb.bump_schema_version();
CREATE EVENT TRIGGER cheladb_extension_functions ON ddl_command_end WHEN TAG IN ('CREATE EXTENSION', 'ALTER EXTENSION') EXECUTE FUNCTION cheladb.grant_extension_functions();
ALTER ROLE authenticator LOGIN NOINHERIT PASSWORD {password};
ALTER DEFAULT PRIVILEGES FOR ROLE {owner_role} IN SCHEMA public GRANT SELECT ON TABLES TO anon;
ALTER DEFAULT PRIVILEGES FOR ROLE {owner_role} IN SCHEMA public GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO authenticated;
ALTER DEFAULT PRIVILEGES FOR ROLE {owner_role} IN SCHEMA public GRANT USAGE, SELECT ON SEQUENCES TO authenticated;
ALTER DEFAULT PRIVILEGES FOR ROLE {owner_role} REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
ALTER DEFAULT PRIVILEGES FOR ROLE {owner_role} IN SCHEMA public GRANT EXECUTE ON FUNCTIONS TO authenticated;
GRANT USAGE ON SCHEMA public TO anon, authenticated;
DO $$ DECLARE r record; BEGIN
  IF NOT EXISTS (SELECT FROM cheladb.bootstrap) THEN
    GRANT SELECT ON ALL TABLES IN SCHEMA public TO anon;
    GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO authenticated;
    GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO authenticated;
    FOR r IN SELECT p.oid::pg_catalog.regprocedure AS f FROM pg_catalog.pg_proc p
      WHERE p.pronamespace OPERATOR(pg_catalog.=) 'public'::pg_catalog.regnamespace
        AND NOT EXISTS (SELECT FROM pg_catalog.pg_depend d WHERE d.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_proc'::pg_catalog.regclass
          AND d.objid OPERATOR(pg_catalog.=) p.oid AND d.deptype OPERATOR(pg_catalog.=) 'e')
    LOOP
      EXECUTE pg_catalog.format('REVOKE EXECUTE ON ROUTINE %s FROM PUBLIC', r.f);
      EXECUTE pg_catalog.format('GRANT EXECUTE ON ROUTINE %s TO authenticated', r.f);
    END LOOP;
    INSERT INTO cheladb.bootstrap (existing_granted) VALUES (true);
  END IF;
END $$;
SELECT pg_catalog.current_setting('cheladb.quarantined', true);
"
    )
}

/// The schema the bootstrap renamed aside, from its output (the script's last line).
pub fn quarantined(output: &str) -> Option<&str> {
    output
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty())
        .filter(|l| l.starts_with("cheladb_quarantine_"))
}

/// Roles a secret key's role must not be a member of, at any depth (besides superusers):
/// they reach the compute itself or the platform's machinery.
const PLATFORM_ROLES: &str = "'cheladb_admin', 'pg_execute_server_program', \
     'pg_read_server_files', 'pg_write_server_files'";

/// One row for an existing role: `superuser`, `neon_superuser` (a member, at any depth),
/// `platform` (a member of a superuser role or of [`PLATFORM_ROLES`]) or `ok`. No row when it
/// doesn't exist. Membership goes by oid over `pg_roles`, so a platform role missing from the
/// image matches nothing instead of failing.
fn role_check_sql(role: &str) -> String {
    format!(
        "SET search_path = pg_catalog, pg_temp; SELECT CASE WHEN r.rolsuper THEN 'superuser' \
         WHEN EXISTS (SELECT FROM pg_roles s WHERE s.rolname = 'neon_superuser' \
         AND pg_has_role(r.oid, s.oid, 'MEMBER')) THEN 'neon_superuser' \
         WHEN EXISTS (SELECT FROM pg_roles s WHERE (s.rolsuper OR s.rolname IN ({PLATFORM_ROLES})) \
         AND pg_has_role(r.oid, s.oid, 'MEMBER')) THEN 'platform' \
         ELSE 'ok' END FROM pg_roles r WHERE r.rolname = {};",
        quote_literal(role)
    )
}

/// Roles the platform owns; never a secret key's.
const RESERVED_ROLES: [&str; 2] = [AUTHENTICATOR, "cheladb_admin"];

/// What [`Worker::check_data_role`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoleCheck {
    /// Allowed, and granted to `authenticator`.
    Ok,
    /// The caller's fault (422): the message says why.
    Refused(String),
    /// The branch isn't ready, or its compute can't be reached (409).
    NotReady,
    /// Something broke on our side (500): logged; the message is for logs.
    Failed(String),
}

impl<M: WorkerMetadata, S: Storage, C: ComputeBackend, N: CacheNotifier, P: SpecPusher>
    Worker<M, S, C, N, P>
{
    /// After any compute (re)creation and from reconcile. Idempotent. The record gives the
    /// branch and its endpoint; the stored password is read afresh.
    pub async fn bootstrap_data_api(
        &self,
        branch: &BranchRecord,
        database_name: &str,
    ) -> anyhow::Result<()> {
        let ep = branch
            .endpoint_id
            .as_deref()
            .with_context(|| format!("branch {} has no endpoint", branch.id))?;
        self.bootstrap_data_api_on(branch.id, ep, database_name)
            .await
    }

    /// [`Self::bootstrap_data_api`] for `branch` on its endpoint `ep`: the password (generated
    /// and stored first when there's none), the script, then the public key if it has none.
    pub(crate) async fn bootstrap_data_api_on(
        &self,
        branch: BranchId,
        ep: &str,
        database_name: &str,
    ) -> anyhow::Result<()> {
        let password = self.authenticator_password(branch).await?;
        let output = self
            .computes
            .admin_sql(ep, database_name, &data_api_sql(OWNER_ROLE, &password))
            .await?;
        if let Some(aside) = quarantined(&output) {
            warn!(branch_id = %branch, %ep, "a cheladb schema the platform didn't make (or one tampered with) was renamed to {aside}; a fresh one was installed");
        }
        self.regrant_secret_key_roles(branch, ep, database_name)
            .await?;
        self.ensure_public_key(branch).await
    }

    /// A reset or restore rewinds `GRANT <role> TO authenticator`, but secret keys survive
    /// both: grant each live secret key's role again. A role that's gone or now fails the
    /// check is skipped with a warning (its key stops working until it's fixed).
    async fn regrant_secret_key_roles(
        &self,
        branch: BranchId,
        ep: &str,
        database_name: &str,
    ) -> anyhow::Result<()> {
        let mut roles: Vec<String> = self
            .metadata
            .list_data_keys(branch)
            .await
            .context("listing the secret keys")?
            .into_iter()
            .filter(|k| k.kind == DataKeyKind::Secret)
            .map(|k| k.role)
            .collect();
        roles.sort();
        roles.dedup();
        for role in roles {
            match self.check_and_grant(branch, ep, database_name, &role).await {
                RoleCheck::Ok => {}
                other => {
                    warn!(branch_id = %branch, %role, "not granting a secret key's role to {AUTHENTICATOR}: {other:?}");
                }
            }
        }
        Ok(())
    }

    /// The branch's `authenticator` password; a new one is sealed (AAD = branch id) and
    /// stored when it has none. The store only fills an empty slot, so the password is read
    /// back: a concurrent run that stored first wins, and both use its password.
    async fn authenticator_password(&self, branch: BranchId) -> anyhow::Result<String> {
        if let Some(password) = self.stored_authenticator_password(branch).await? {
            return Ok(password);
        }
        let password = credentials::generate_password();
        let sealed = seal_credential(
            &self.key,
            &branch.to_string(),
            Purpose::Authenticator,
            &password,
        );
        self.metadata
            .set_authenticator_password(branch, &sealed)
            .await
            .context("storing the authenticator password")?;
        self.stored_authenticator_password(branch)
            .await?
            .context("the authenticator password wasn't stored (is the branch being deleted?)")
    }

    async fn stored_authenticator_password(
        &self,
        branch: BranchId,
    ) -> anyhow::Result<Option<String>> {
        let row = self
            .metadata
            .branch_connection(branch)
            .await
            .context("reading the authenticator password")?;
        row.and_then(|r| r.authenticator_password_enc)
            .map(|sealed| {
                open_credential(&self.key, &branch.to_string(), Purpose::Authenticator, &sealed)
                    .context("opening the authenticator password")
            })
            .transpose()
    }

    /// The branch's public key (role `anon`, name `public`, sealed with AAD = key id), unless
    /// it has a live one. Losing a race with a concurrent run is fine.
    async fn ensure_public_key(&self, branch: BranchId) -> anyhow::Result<()> {
        if self
            .metadata
            .has_live_public_key(branch)
            .await
            .context("checking the public key")?
        {
            return Ok(());
        }
        let generated = data_key::generate(DataKeyKind::Public);
        let id = Uuid::new_v4();
        let key = NewDataKey {
            id,
            branch_id: branch,
            kind: DataKeyKind::Public,
            role: "anon".to_string(),
            name: "public".to_string(),
            prefix: generated.prefix,
            sha256: generated.sha256,
            sealed: Some(seal_credential(
                &self.key,
                &id.to_string(),
                Purpose::DataKey,
                &generated.key,
            )),
        };
        match self.metadata.insert_data_key(&key).await {
            Ok(()) => {
                info!(branch_id = %branch, key_id = %id, "created the branch's public key");
                Ok(())
            }
            Err(MetadataError::Conflict) => {
                info!(branch_id = %branch, "a concurrent run created the public key");
                Ok(())
            }
            Err(e) => Err(e).context("creating the public key"),
        }
    }

    /// Reconcile's `BootstrapDataApi`: only a ready `normal` branch with an endpoint.
    pub(crate) async fn reconcile_data_api(&self, id: BranchId) -> Result<(), String> {
        let branch = self
            .metadata
            .get_branch(id)
            .await
            .map_err(|e| e.to_string())?;
        let Some(branch) =
            branch.filter(|b| b.status == BranchStatus::Ready && b.kind == BranchKind::Normal)
        else {
            return Ok(());
        };
        let Some(ep) = branch.endpoint_id.as_deref() else {
            return Ok(());
        };
        let database_name = self.database_of(ep).await.map_err(|e| format!("{e:#}"))?;
        self.bootstrap_data_api(&branch, &database_name)
            .await
            .map_err(|e| format!("bootstrapping the data API: {e:#}"))
    }

    /// The database inside `ep`'s compute.
    async fn database_of(&self, ep: &str) -> anyhow::Result<String> {
        self.metadata
            .neon_databases(ep)
            .await
            .context("reading the endpoint's database")?
            .into_iter()
            .next()
            .map(|d| d.name)
            .with_context(|| format!("endpoint {ep} has no database"))
    }

    /// Whether `ep`'s data API machinery is intact ([`SCHEMA_CHECK_SQL`]).
    pub async fn data_api_healthy(&self, ep: &str) -> anyhow::Result<bool> {
        let database_name = self.database_of(ep).await?;
        let out = self
            .computes
            .admin_sql(ep, &database_name, SCHEMA_CHECK_SQL)
            .await?;
        match out.trim() {
            "t" => Ok(true),
            "f" => Ok(false),
            other => bail!("unexpected answer {other:?}"),
        }
    }

    /// Whether `role` may back a secret key on `branch`: a valid name, not reserved or
    /// built-in (`pg_*`), existing, not a superuser, and not a member (at any depth) of a
    /// superuser role, `neon_superuser` or the other platform roles. On success it's granted
    /// to `authenticator` (`set_config('role', …)` needs the membership).
    pub async fn check_data_role(&self, branch: BranchId, role: &str) -> RoleCheck {
        if let Some(refusal) = refuse_by_name(role) {
            return refusal;
        }
        let row = match self.metadata.branch_connection(branch).await {
            Ok(row) => row,
            Err(e) => {
                warn!(branch_id = %branch, "reading the branch to check a role: {e}");
                return RoleCheck::Failed(format!("reading the branch: {e}"));
            }
        };
        let Some((ep, database_name)) = row
            .filter(|r| r.status == BranchStatus::Ready)
            .and_then(|r| Some((r.endpoint_id?, r.database_name?)))
        else {
            return RoleCheck::NotReady;
        };
        self.check_and_grant(branch, &ep, &database_name, role)
            .await
    }

    /// [`Self::check_data_role`] as the RPC's reply.
    pub async fn check_data_role_reply(&self, request: &CheckDataRoleRequest) -> CheckDataRoleReply {
        match self.check_data_role(request.branch_id, &request.role).await {
            RoleCheck::Ok => CheckDataRoleReply::ok(),
            RoleCheck::Refused(message) => CheckDataRoleReply::refused(message),
            RoleCheck::NotReady => CheckDataRoleReply::not_ready(),
            RoleCheck::Failed(message) => CheckDataRoleReply::failed(message),
        }
    }

    /// The name rules, the check query on `ep`, then the grant.
    async fn check_and_grant(
        &self,
        branch: BranchId,
        ep: &str,
        database_name: &str,
        role: &str,
    ) -> RoleCheck {
        if let Some(refusal) = refuse_by_name(role) {
            return refusal;
        }
        let answer = match self
            .computes
            .admin_sql(ep, database_name, &role_check_sql(role))
            .await
        {
            Ok(answer) => answer,
            Err(e) => {
                warn!(branch_id = %branch, endpoint_id = %ep, "checking role {role}: {e:#}");
                return RoleCheck::NotReady;
            }
        };
        let refused = |why: &str| RoleCheck::Refused(format!("the role {role} {why}"));
        match answer.trim() {
            "ok" => {}
            "" => return refused("does not exist"),
            "superuser" => return refused("is a superuser"),
            "neon_superuser" => return refused("is a member of neon_superuser"),
            "platform" => return refused("has platform privileges"),
            other => {
                warn!(branch_id = %branch, "checking role {role}: unexpected answer {other:?}");
                return RoleCheck::Failed(format!(
                    "checking the role {role} gave an unexpected answer {other:?}"
                ));
            }
        }
        let grant = format!("GRANT {} TO {AUTHENTICATOR};", quote_ident(role));
        if let Err(e) = self.computes.admin_sql(ep, database_name, &grant).await {
            warn!(branch_id = %branch, endpoint_id = %ep, "granting {role} to {AUTHENTICATOR}: {e:#}");
            return RoleCheck::Failed(format!(
                "granting the role {role} to {AUTHENTICATOR}: {e:#}"
            ));
        }
        info!(branch_id = %branch, %role, "granted to {AUTHENTICATOR}");
        RoleCheck::Ok
    }
}

/// The refusals that need no SQL: an invalid name, a reserved role, a built-in `pg_*` role
/// (matched case-insensitively).
fn refuse_by_name(role: &str) -> Option<RoleCheck> {
    if !valid_role_name(role) {
        return Some(RoleCheck::Refused(format!(
            "{role:?} is not a valid role name"
        )));
    }
    if RESERVED_ROLES.contains(&role) {
        return Some(RoleCheck::Refused(format!("the role {role} is reserved")));
    }
    if role.len() >= 3 && role[..3].eq_ignore_ascii_case("pg_") {
        return Some(RoleCheck::Refused(format!(
            "the role {role} is a built-in role"
        )));
    }
    None
}

#[cfg(test)]
mod tests {
    use cheladb_metadata::{BranchStatus, DatabaseStatus, NewDataKey};
    use cheladb_secrets::{Purpose, open_credential};
    use cheladb_types::data_key::{self, DataKeyKind};

    use super::*;
    use crate::fakes::{
        FakeWorker, Journal, entries, position, ready_branch, ready_database, worker,
    };

    /// The data API script's first line, as the fake backend journals it.
    /// The data API script's first line (the role check puts its SELECT on the same line).
    const DATA_API: &str = "SET search_path = pg_catalog, pg_temp;";

    fn pos(journal: &Journal, prefix: &str) -> usize {
        position(journal, prefix)
            .unwrap_or_else(|| panic!("{prefix:?} not in journal {:?}", entries(journal)))
    }

    fn count(journal: &Journal, prefix: &str) -> usize {
        entries(journal)
            .iter()
            .filter(|e| e.starts_with(prefix))
            .count()
    }

    /// Every data API script that ran on `ep`.
    fn data_api_scripts(w: &FakeWorker, ep: &str) -> Vec<String> {
        w.computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .filter(|(e, _, s)| e == ep && s.starts_with(&format!("{DATA_API}\n")))
            .map(|(_, _, s)| s.clone())
            .collect()
    }

    fn authenticator_password(w: &FakeWorker, branch: &BranchRecord) -> String {
        open_credential(
            &w.key,
            &branch.id.to_string(),
            Purpose::Authenticator,
            branch
                .authenticator_password_enc
                .as_deref()
                .expect("an authenticator password"),
        )
        .unwrap()
    }

    fn keys_of(w: &FakeWorker, branch: BranchId) -> Vec<NewDataKey> {
        w.metadata
            .state()
            .data_keys
            .iter()
            .filter(|k| k.branch_id == branch)
            .cloned()
            .collect()
    }

    #[test]
    fn data_api_sql_is_exact() {
        let expected = "SET search_path = pg_catalog, pg_temp;
DO $$ BEGIN
  IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'cheladb_admin') THEN CREATE ROLE cheladb_admin NOLOGIN; END IF;
END $$;
DO $$ DECLARE
  admin pg_catalog.oid := (SELECT oid FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) 'cheladb_admin');
  trusted pg_catalog.oid[] := ARRAY[admin, (SELECT oid FROM pg_catalog.pg_roles WHERE rolname OPERATOR(pg_catalog.=) CURRENT_USER)];
  ns pg_catalog.oid := (SELECT oid FROM pg_catalog.pg_namespace WHERE nspname OPERATOR(pg_catalog.=) 'cheladb');
  members pg_catalog.oid[] := ARRAY(SELECT member FROM pg_catalog.pg_auth_members WHERE roleid OPERATOR(pg_catalog.=) admin);
  joined boolean := pg_catalog.cardinality(members) OPERATOR(pg_catalog.>) 0;
  tampered boolean := EXISTS (SELECT FROM pg_catalog.pg_authid a WHERE a.oid OPERATOR(pg_catalog.=) admin
    AND (a.rolcanlogin OR a.rolsuper OR a.rolcreaterole OR a.rolcreatedb OR a.rolreplication OR a.rolbypassrls
      OR a.rolpassword IS NOT NULL OR a.rolconnlimit OPERATOR(pg_catalog.<>) ALL (ARRAY[-1, 0])))
    OR EXISTS (SELECT FROM pg_catalog.pg_db_role_setting WHERE setrole OPERATOR(pg_catalog.=) admin);
  m record;
  aside pg_catalog.text;
BEGIN
  ALTER ROLE cheladb_admin NOLOGIN NOSUPERUSER NOCREATEROLE NOCREATEDB NOREPLICATION NOBYPASSRLS NOINHERIT PASSWORD NULL CONNECTION LIMIT 0;
  FOR m IN SELECT d.datname FROM pg_catalog.pg_db_role_setting s JOIN pg_catalog.pg_database d ON d.oid OPERATOR(pg_catalog.=) s.setdatabase
    WHERE s.setrole OPERATOR(pg_catalog.=) admin
  LOOP
    EXECUTE pg_catalog.format('ALTER ROLE cheladb_admin IN DATABASE %I RESET ALL', m.datname);
  END LOOP;
  ALTER ROLE cheladb_admin RESET ALL;
  FOR m IN SELECT r.rolname AS member, g.rolname AS grantor FROM pg_catalog.pg_auth_members a
    JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) a.member JOIN pg_catalog.pg_roles g ON g.oid OPERATOR(pg_catalog.=) a.grantor
    WHERE a.roleid OPERATOR(pg_catalog.=) admin
  LOOP
    EXECUTE pg_catalog.format('REVOKE cheladb_admin FROM %I GRANTED BY %I CASCADE', m.member, m.grantor);
  END LOOP;
  PERFORM pg_catalog.set_config('cheladb.former_members', pg_catalog.array_to_string(members, ','), false);
  PERFORM pg_catalog.pg_terminate_backend(a.pid) FROM pg_catalog.pg_stat_activity a
    JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) a.usesysid
    WHERE (a.usesysid OPERATOR(pg_catalog.=) admin OR a.usesysid OPERATOR(pg_catalog.=) ANY (members)) AND NOT r.rolsuper
      AND a.pid OPERATOR(pg_catalog.<>) pg_catalog.pg_backend_pid();
  IF ns IS NOT NULL AND (joined OR tampered
    OR (SELECT nspowner FROM pg_catalog.pg_namespace WHERE oid OPERATOR(pg_catalog.=) ns) OPERATOR(pg_catalog.<>) admin
    OR EXISTS (SELECT FROM pg_catalog.pg_class c WHERE c.relnamespace OPERATOR(pg_catalog.=) ns
      AND (c.relowner OPERATOR(pg_catalog.<>) ALL (trusted) OR c.reloftype OPERATOR(pg_catalog.<>) 0::pg_catalog.oid
        OR c.relkind OPERATOR(pg_catalog.<>) ALL (ARRAY['r', 'i']::pg_catalog.\"char\"[])
        OR c.relname OPERATOR(pg_catalog.<>) ALL (ARRAY['schema_version', 'schema_version_pkey', 'bootstrap', 'bootstrap_pkey']::pg_catalog.name[])))
    OR EXISTS (SELECT FROM pg_catalog.pg_proc p WHERE p.pronamespace OPERATOR(pg_catalog.=) ns
      AND (p.proowner OPERATOR(pg_catalog.<>) ALL (trusted)
        OR p.proname OPERATOR(pg_catalog.<>) ALL (ARRAY['bump_schema_version', 'grant_extension_functions']::pg_catalog.name[])))
    OR EXISTS (SELECT FROM pg_catalog.pg_trigger t JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) t.tgrelid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns AND NOT t.tgisinternal)
    OR EXISTS (SELECT FROM pg_catalog.pg_rewrite w JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) w.ev_class
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns)
    OR EXISTS (SELECT FROM pg_catalog.pg_policy y JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) y.polrelid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns)
    OR EXISTS (SELECT FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) a.attrelid
      JOIN pg_catalog.pg_type ty ON ty.oid OPERATOR(pg_catalog.=) a.atttypid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns AND a.attnum OPERATOR(pg_catalog.>) 0
        AND ty.typnamespace OPERATOR(pg_catalog.<>) 'pg_catalog'::pg_catalog.regnamespace)
    OR EXISTS (SELECT FROM pg_catalog.pg_attrdef d JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) d.adrelid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns AND pg_catalog.pg_get_expr(d.adbin, d.adrelid) OPERATOR(pg_catalog.<>) 'true')
    OR EXISTS (SELECT FROM pg_catalog.pg_constraint k JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) k.conrelid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns AND k.contype OPERATOR(pg_catalog.<>) ALL (ARRAY['p', 'n']::pg_catalog.\"char\"[])
        AND pg_catalog.pg_get_constraintdef(k.oid) OPERATOR(pg_catalog.<>) 'CHECK (one)')
    OR EXISTS (SELECT FROM pg_catalog.pg_index i JOIN pg_catalog.pg_class c ON c.oid OPERATOR(pg_catalog.=) i.indrelid
      WHERE c.relnamespace OPERATOR(pg_catalog.=) ns AND (i.indexprs IS NOT NULL OR i.indpred IS NOT NULL
        OR EXISTS (SELECT FROM pg_catalog.pg_opclass o WHERE o.oid OPERATOR(pg_catalog.=) ANY (i.indclass)
          AND o.opcnamespace OPERATOR(pg_catalog.<>) 'pg_catalog'::pg_catalog.regnamespace))))
  THEN
    DROP EVENT TRIGGER IF EXISTS cheladb_schema_version_ddl;
    DROP EVENT TRIGGER IF EXISTS cheladb_schema_version_drop;
    DROP EVENT TRIGGER IF EXISTS cheladb_extension_functions;
    aside := pg_catalog.format('cheladb_quarantine_%s', pg_catalog.floor(pg_catalog.date_part('epoch', pg_catalog.clock_timestamp()) * 1000000)::pg_catalog.int8);
    EXECUTE pg_catalog.format('ALTER SCHEMA cheladb RENAME TO %I', aside);
    PERFORM pg_catalog.set_config('cheladb.quarantined', aside, false);
  END IF;
END $$;
DO $$ BEGIN
  PERFORM pg_catalog.pg_terminate_backend(a.pid) FROM pg_catalog.pg_stat_activity a
    JOIN pg_catalog.pg_roles r ON r.oid OPERATOR(pg_catalog.=) a.usesysid
    WHERE (r.rolname OPERATOR(pg_catalog.=) 'cheladb_admin' OR a.usesysid OPERATOR(pg_catalog.=) ANY
        (pg_catalog.string_to_array(pg_catalog.current_setting('cheladb.former_members', true), ',')::pg_catalog.oid[]))
      AND NOT r.rolsuper AND a.pid OPERATOR(pg_catalog.<>) pg_catalog.pg_backend_pid();
END $$;
CREATE SCHEMA IF NOT EXISTS cheladb AUTHORIZATION cheladb_admin;
REVOKE ALL ON SCHEMA cheladb FROM PUBLIC;
GRANT USAGE ON SCHEMA cheladb TO authenticator;
CREATE TABLE IF NOT EXISTS cheladb.schema_version (one boolean PRIMARY KEY DEFAULT true CHECK (one), version uuid NOT NULL);
INSERT INTO cheladb.schema_version (version) VALUES (pg_catalog.gen_random_uuid()) ON CONFLICT DO NOTHING;
CREATE TABLE IF NOT EXISTS cheladb.bootstrap (one boolean PRIMARY KEY DEFAULT true CHECK (one), existing_granted boolean NOT NULL);
ALTER TABLE cheladb.schema_version OWNER TO cheladb_admin;
ALTER TABLE cheladb.bootstrap OWNER TO cheladb_admin;
REVOKE ALL ON cheladb.schema_version, cheladb.bootstrap FROM PUBLIC;
GRANT SELECT ON cheladb.schema_version TO authenticator;
CREATE OR REPLACE FUNCTION cheladb.bump_schema_version() RETURNS event_trigger
  LANGUAGE plpgsql SECURITY DEFINER SET search_path = pg_catalog, pg_temp AS
  $f$ BEGIN UPDATE cheladb.schema_version SET version = pg_catalog.gen_random_uuid(); END $f$;
ALTER FUNCTION cheladb.bump_schema_version() OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION cheladb.bump_schema_version() FROM PUBLIC;
CREATE OR REPLACE FUNCTION cheladb.grant_extension_functions() RETURNS event_trigger
  LANGUAGE plpgsql SECURITY INVOKER SET search_path = pg_catalog, pg_temp AS
  $f$ DECLARE f pg_catalog.regprocedure; BEGIN
    FOR f IN SELECT d.objid::pg_catalog.regprocedure FROM pg_catalog.pg_event_trigger_ddl_commands() c
      JOIN pg_catalog.pg_depend d ON d.refclassid OPERATOR(pg_catalog.=) 'pg_catalog.pg_extension'::pg_catalog.regclass AND d.refobjid OPERATOR(pg_catalog.=) c.objid
      JOIN pg_catalog.pg_proc p ON p.oid OPERATOR(pg_catalog.=) d.objid
      WHERE c.object_type OPERATOR(pg_catalog.=) 'extension' AND d.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_proc'::pg_catalog.regclass AND d.deptype OPERATOR(pg_catalog.=) 'e'
        AND p.proowner OPERATOR(pg_catalog.=) (SELECT r.oid FROM pg_catalog.pg_roles r WHERE r.rolname OPERATOR(pg_catalog.=) 'postgres' AND NOT r.rolsuper)
    LOOP
      BEGIN
        EXECUTE pg_catalog.format('GRANT EXECUTE ON ROUTINE %s TO PUBLIC', f);
      EXCEPTION WHEN OTHERS THEN
        RAISE WARNING 'cheladb: EXECUTE on % was not granted to PUBLIC: %', f, SQLERRM;
      END;
    END LOOP;
  EXCEPTION WHEN OTHERS THEN
    RAISE WARNING 'cheladb: extension functions were not granted to PUBLIC: %', SQLERRM;
  END $f$;
ALTER FUNCTION cheladb.grant_extension_functions() OWNER TO CURRENT_USER;
REVOKE ALL ON FUNCTION cheladb.grant_extension_functions() FROM PUBLIC;
DROP EVENT TRIGGER IF EXISTS cheladb_schema_version_ddl;
DROP EVENT TRIGGER IF EXISTS cheladb_schema_version_drop;
DROP EVENT TRIGGER IF EXISTS cheladb_extension_functions;
CREATE EVENT TRIGGER cheladb_schema_version_ddl ON ddl_command_end EXECUTE FUNCTION cheladb.bump_schema_version();
CREATE EVENT TRIGGER cheladb_schema_version_drop ON sql_drop EXECUTE FUNCTION cheladb.bump_schema_version();
CREATE EVENT TRIGGER cheladb_extension_functions ON ddl_command_end WHEN TAG IN ('CREATE EXTENSION', 'ALTER EXTENSION') EXECUTE FUNCTION cheladb.grant_extension_functions();
ALTER ROLE authenticator LOGIN NOINHERIT PASSWORD 's3cretPassw0rd';
ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA public GRANT SELECT ON TABLES TO anon;
ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA public GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO authenticated;
ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA public GRANT USAGE, SELECT ON SEQUENCES TO authenticated;
ALTER DEFAULT PRIVILEGES FOR ROLE postgres REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC;
ALTER DEFAULT PRIVILEGES FOR ROLE postgres IN SCHEMA public GRANT EXECUTE ON FUNCTIONS TO authenticated;
GRANT USAGE ON SCHEMA public TO anon, authenticated;
DO $$ DECLARE r record; BEGIN
  IF NOT EXISTS (SELECT FROM cheladb.bootstrap) THEN
    GRANT SELECT ON ALL TABLES IN SCHEMA public TO anon;
    GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO authenticated;
    GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO authenticated;
    FOR r IN SELECT p.oid::pg_catalog.regprocedure AS f FROM pg_catalog.pg_proc p
      WHERE p.pronamespace OPERATOR(pg_catalog.=) 'public'::pg_catalog.regnamespace
        AND NOT EXISTS (SELECT FROM pg_catalog.pg_depend d WHERE d.classid OPERATOR(pg_catalog.=) 'pg_catalog.pg_proc'::pg_catalog.regclass
          AND d.objid OPERATOR(pg_catalog.=) p.oid AND d.deptype OPERATOR(pg_catalog.=) 'e')
    LOOP
      EXECUTE pg_catalog.format('REVOKE EXECUTE ON ROUTINE %s FROM PUBLIC', r.f);
      EXECUTE pg_catalog.format('GRANT EXECUTE ON ROUTINE %s TO authenticated', r.f);
    END LOOP;
    INSERT INTO cheladb.bootstrap (existing_granted) VALUES (true);
  END IF;
END $$;
SELECT pg_catalog.current_setting('cheladb.quarantined', true);
";
        assert_eq!(data_api_sql("postgres", "s3cretPassw0rd"), expected);
        // A quoted literal, quotes doubled.
        assert!(data_api_sql("postgres", "it's").contains("PASSWORD 'it''s';\n"));
    }

    /// The script runs as a superuser on a database its owner controls: nothing it calls
    /// may resolve through the owner's schemas, and a `cheladb` schema it didn't make (or
    /// one tampered with) is renamed aside before anything touches it.
    #[test]
    fn the_script_pins_the_search_path_and_vets_cheladb_first() {
        let sql = data_api_sql("postgres", "pw");
        assert!(sql.starts_with("SET search_path = pg_catalog, pg_temp;\n"), "{sql}");
        let rename = sql.find("ALTER SCHEMA cheladb RENAME TO %I").expect("the rename aside");
        let create = sql.find("CREATE SCHEMA IF NOT EXISTS cheladb").unwrap();
        assert!(rename < create);
        assert!(sql.contains("REVOKE cheladb_admin FROM %I GRANTED BY %I CASCADE"));
        for check in ["pg_trigger", "pg_rewrite", "pg_policy", "pg_attrdef", "pg_opclass", "relowner", "proowner"] {
            assert!(sql.contains(check), "{check}");
        }
        assert_eq!(sql.matches("SET search_path = pg_catalog, pg_temp AS").count(), 2);
        assert!(!sql.contains(" format("), "every function qualified");
        assert!(sql.ends_with("SELECT pg_catalog.current_setting('cheladb.quarantined', true);\n"));
    }

    /// `cheladb_admin` owns the schema: an owner who made it first (with a login, a
    /// password or settings) could log in as it, so it's stripped every run, its sessions
    /// (and those of anyone who held it) are ended, and a tampered role means the schema
    /// is set aside.
    #[test]
    fn cheladb_admin_is_locked_down_before_the_schema_is_trusted() {
        let sql = data_api_sql("postgres", "pw");
        let locked = sql
            .find("ALTER ROLE cheladb_admin NOLOGIN NOSUPERUSER NOCREATEROLE NOCREATEDB NOREPLICATION NOBYPASSRLS NOINHERIT PASSWORD NULL CONNECTION LIMIT 0;")
            .expect("the role is locked down");
        assert!(sql.contains("ALTER ROLE cheladb_admin RESET ALL;"));
        assert!(sql.contains("ALTER ROLE cheladb_admin IN DATABASE %I RESET ALL"));
        let ended = sql.find("pg_catalog.pg_terminate_backend(a.pid)").expect("sessions ended");
        assert!(sql.contains("a.rolcanlogin OR a.rolsuper"), "a tampered role is detected");
        assert!(sql.contains("a.rolpassword IS NOT NULL"));
        assert!(sql.contains("IF ns IS NOT NULL AND (joined OR tampered"));
        let create = sql.find("CREATE SCHEMA IF NOT EXISTS cheladb").unwrap();
        assert!(locked < create && ended < create);
    }

    /// As superuser, the GRANT (DDL) would fire the owner's own event triggers with a
    /// superuser current_user: it runs as whoever ran CREATE EXTENSION, grants only what
    /// they own, and never fails their command.
    #[test]
    fn grant_extension_functions_runs_as_the_invoker_and_never_fails() {
        let sql = data_api_sql("postgres", "pw");
        let start = sql.find("CREATE OR REPLACE FUNCTION cheladb.grant_extension_functions()").unwrap();
        let end = sql[start..].find("ALTER FUNCTION cheladb.grant_extension_functions()").unwrap();
        let body = &sql[start..start + end];
        assert!(body.contains("SECURITY INVOKER"), "{body}");
        assert!(!body.contains("SECURITY DEFINER"), "{body}");
        assert!(!body.contains("CURRENT_USER"), "{body}");
        assert_eq!(body.matches("EXCEPTION WHEN OTHERS THEN").count(), 2);
        assert!(body.contains("RAISE WARNING"));
    }

    /// A superuser's CREATE EXTENSION (compute_ctl, Neon's privileged path) owns every
    /// function it adds: re-granting those would undo the extension's own revokes. Only
    /// the database owner's functions (the one role whose default privilege strips PUBLIC)
    /// are granted, and never a superuser's.
    #[test]
    fn grant_extension_functions_only_touches_the_owner_roles_functions() {
        let sql = data_api_sql("app_owner", "pw");
        assert!(sql.contains(
            "AND p.proowner OPERATOR(pg_catalog.=) (SELECT r.oid FROM pg_catalog.pg_roles r WHERE r.rolname OPERATOR(pg_catalog.=) 'app_owner' AND NOT r.rolsuper)"
        ));
        assert!(data_api_sql("it's", "pw").contains("OPERATOR(pg_catalog.=) 'it''s' AND NOT r.rolsuper"));
    }

    /// A session of a former member that started before the REVOKE committed could still
    /// SET ROLE cheladb_admin: sessions are ended again once it has.
    #[test]
    fn former_members_sessions_are_ended_again_after_the_revoke_commits() {
        let sql = data_api_sql("postgres", "pw");
        let vet_end = sql.find("PERFORM pg_catalog.set_config('cheladb.quarantined', aside, false);").unwrap();
        let again = sql
            .find("current_setting('cheladb.former_members', true)")
            .expect("a second termination");
        let create = sql.find("CREATE SCHEMA IF NOT EXISTS cheladb").unwrap();
        assert!(vet_end < again && again < create);
        assert_eq!(sql.matches("pg_catalog.pg_terminate_backend(a.pid)").count(), 2);
    }

    #[test]
    fn quarantined_schema_is_read_from_the_output() {
        assert_eq!(
            quarantined("cheladb_quarantine_1700000000\n"),
            Some("cheladb_quarantine_1700000000")
        );
        assert_eq!(quarantined("\n"), None);
        assert_eq!(quarantined(""), None);
        assert_eq!(quarantined("something else\n"), None);
    }

    #[tokio::test]
    async fn bootstrap_survives_a_quarantine() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        w.computes.answers.lock().unwrap().push((
            "cheladb.quarantined".into(),
            "cheladb_quarantine_1700000000\n".into(),
        ));
        w.bootstrap_data_api(&main, "app").await.unwrap();
    }

    #[test]
    fn role_check_sql_pins_the_search_path() {
        assert!(role_check_sql("app").starts_with("SET search_path = pg_catalog, pg_temp; SELECT "));
    }

    /// Neon refuses to let a superuser run a function a non-superuser owns, and the event
    /// triggers fire for cloud_admin's own DDL: every trigger function must be owned by the
    /// superuser that runs the bootstrap (an earlier bootstrap's owner is replaced), never by
    /// cheladb_admin.
    #[test]
    fn event_trigger_functions_are_owned_by_the_bootstrap_superuser() {
        let sql = data_api_sql("postgres", "pw");
        for function in [
            "cheladb.bump_schema_version()",
            "cheladb.grant_extension_functions()",
        ] {
            assert!(
                sql.contains(&format!("ALTER FUNCTION {function} OWNER TO CURRENT_USER;\n")),
                "{function} is handed to the bootstrap's superuser"
            );
            assert!(
                !sql.contains(&format!("ALTER FUNCTION {function} OWNER TO cheladb_admin")),
                "{function} must not be owned by cheladb_admin"
            );
        }
    }

    #[tokio::test]
    async fn bootstrap_generates_and_seals_a_password_once() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let ep = main.endpoint_id.clone().unwrap();
        // Provision generated, sealed (AAD = branch id) and used one.
        let password = authenticator_password(&w, &main);
        assert_eq!(password.len(), 32);
        assert!(password.chars().all(|c| c.is_ascii_alphanumeric()));
        let scripts = data_api_scripts(&w, &ep);
        assert_eq!(scripts.len(), 1);
        assert_eq!(scripts[0], data_api_sql("postgres", &password));
        assert!(
            pos(&journal, "metadata.set_authenticator_password")
                < pos(&journal, &format!("compute.sql {ep} app {DATA_API}")),
            "stored before it's used: {:?}",
            entries(&journal)
        );

        // A second run reuses it.
        journal.lock().unwrap().clear();
        w.bootstrap_data_api(&main, "app").await.unwrap();
        assert_eq!(count(&journal, "metadata.set_authenticator_password"), 0);
        assert_eq!(
            w.metadata.main(id).authenticator_password_enc,
            main.authenticator_password_enc
        );
        let scripts = data_api_scripts(&w, &ep);
        assert_eq!(scripts.len(), 2);
        assert_eq!(scripts[1], scripts[0]);

        // A branch without one gets a new one, once.
        w.metadata
            .state()
            .branches
            .get_mut(&main.id)
            .unwrap()
            .authenticator_password_enc = None;
        let bare = w.metadata.main(id);
        w.bootstrap_data_api(&bare, "app").await.unwrap();
        let fresh = w.metadata.main(id);
        let new_password = authenticator_password(&w, &fresh);
        assert_ne!(new_password, password);
        assert_eq!(
            data_api_scripts(&w, &ep).last().unwrap(),
            &data_api_sql("postgres", &new_password)
        );
        w.bootstrap_data_api(&fresh, "app").await.unwrap();
        assert_eq!(
            w.metadata.main(id).authenticator_password_enc,
            fresh.authenticator_password_enc
        );
        assert_eq!(count(&journal, "metadata.set_authenticator_password"), 1);
    }

    /// Two runs that both find no password: the one that stores second uses the first's,
    /// so the compute and the stored password never disagree.
    #[tokio::test]
    async fn a_concurrent_password_write_wins_and_is_used() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let ep = main.endpoint_id.clone().unwrap();
        w.metadata
            .state()
            .branches
            .get_mut(&main.id)
            .unwrap()
            .authenticator_password_enc = None;
        let theirs = seal_credential(
            &w.key,
            &main.id.to_string(),
            Purpose::Authenticator,
            "theirs-password",
        );
        let racing = theirs.clone();
        let branch = main.id;
        w.metadata.state().before_set_authenticator_password = Some(Box::new(move |state| {
            state
                .branches
                .get_mut(&branch)
                .unwrap()
                .authenticator_password_enc = Some(racing);
        }));
        w.bootstrap_data_api(&main, "app").await.unwrap();
        assert_eq!(
            w.metadata.main(id).authenticator_password_enc.as_deref(),
            Some(theirs.as_str())
        );
        assert_eq!(
            data_api_scripts(&w, &ep).last().unwrap(),
            &data_api_sql("postgres", "theirs-password")
        );
    }

    /// Reconcile's check fails on anything the bootstrap repairs: the schema, the login, the
    /// grants that let `authenticator` read the version, and the event triggers.
    #[test]
    fn the_health_check_covers_the_login_grants_and_triggers() {
        for part in [
            "to_regclass('cheladb.schema_version')",
            "rolcanlogin",
            "has_schema_privilege",
            "'USAGE'",
            "has_table_privilege",
            "'SELECT'",
            "evtenabled",
            "cheladb_schema_version_ddl",
            "cheladb_schema_version_drop",
            "cheladb_extension_functions",
        ] {
            assert!(SCHEMA_CHECK_SQL.contains(part), "{part}: {SCHEMA_CHECK_SQL}");
        }
    }

    #[tokio::test]
    async fn bootstrap_creates_one_public_key() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let keys = keys_of(&w, main.id);
        assert_eq!(keys.len(), 1, "{keys:?}");
        let key = &keys[0];
        assert_eq!(key.kind, DataKeyKind::Public);
        assert_eq!(key.role, "anon");
        assert_eq!(key.name, "public");
        // Sealed with the key id as AAD, so Studio can show it again.
        let shown = open_credential(
            &w.key,
            &key.id.to_string(),
            Purpose::DataKey,
            key.sealed.as_deref().unwrap(),
        )
        .unwrap();
        assert_eq!(data_key::parse_kind(&shown), Some(DataKeyKind::Public));
        assert_eq!(key.prefix, shown[..12]);
        assert_eq!(key.sha256, data_key::hash(&shown));

        // A second run creates none.
        journal.lock().unwrap().clear();
        w.bootstrap_data_api(&main, "app").await.unwrap();
        assert_eq!(keys_of(&w, main.id).len(), 1);
        assert_eq!(count(&journal, "metadata.insert_data_key"), 0);

        // A concurrent run that wins the insert is fine.
        w.metadata.state().data_keys.clear();
        let theirs = NewDataKey {
            id: uuid::Uuid::new_v4(),
            ..key.clone()
        };
        let racing = theirs.clone();
        w.metadata.state().before_insert_data_key =
            Some(Box::new(move |state| state.data_keys.push(racing)));
        w.bootstrap_data_api(&main, "app").await.unwrap();
        let keys = keys_of(&w, main.id);
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].id, theirs.id);
        assert_eq!(count(&journal, "metadata.insert_data_key"), 1);
    }

    #[tokio::test]
    async fn provision_runs_the_data_api_bootstrap_after_roles() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        let ep = w.metadata.main(id).endpoint_id.unwrap();
        let order = [
            pos(&journal, &format!("compute.sql {ep} app SELECT 1")),
            pos(&journal, &format!("compute.sql {ep} app DO $$")),
            pos(&journal, "metadata.set_authenticator_password"),
            pos(&journal, &format!("compute.sql {ep} app {DATA_API}")),
            pos(&journal, "metadata.insert_data_key"),
            pos(&journal, "metadata.set_branch_ready"),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
        // The roles' script is the first `DO $$`.
        assert!(order[1] < order[3]);
    }

    #[tokio::test]
    async fn branch_create_runs_it() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        journal.lock().unwrap().clear();
        let dev = ready_branch(&w, id, "dev", main.id).await;
        let dev = w.metadata.branch(dev);
        let ep = dev.endpoint_id.clone().unwrap();
        let order = [
            pos(&journal, &format!("compute.sql {ep} app SELECT 1")),
            pos(&journal, &format!("compute.sql {ep} app DO $$")),
            pos(&journal, &format!("compute.sql {ep} app {DATA_API}")),
            pos(&journal, "metadata.insert_data_key"),
            pos(&journal, "metadata.set_branch_ready"),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
        // Its own password and its own public key.
        let password = authenticator_password(&w, &dev);
        assert_ne!(password, authenticator_password(&w, &main));
        assert_eq!(
            data_api_scripts(&w, &ep),
            [data_api_sql("postgres", &password)]
        );
        assert_eq!(keys_of(&w, dev.id).len(), 1);
        assert_eq!(keys_of(&w, main.id).len(), 1);
    }

    #[tokio::test]
    async fn recreate_compute_runs_role_and_data_api_bootstrap() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let dev = ready_branch(&w, id, "dev", main.id).await;
        let dev = w.metadata.branch(dev);
        let ep = dev.endpoint_id.clone().unwrap();
        w.computes.remove(&ep);
        journal.lock().unwrap().clear();

        w.recreate_compute(dev.id, &ep).await.unwrap();

        let order = [
            pos(&journal, &format!("compute.create {ep}")),
            pos(&journal, &format!("compute.sql {ep} app SELECT 1")),
            pos(&journal, &format!("compute.sql {ep} app DO $$")),
            pos(&journal, &format!("compute.sql {ep} app {DATA_API}")),
        ];
        assert!(order.is_sorted(), "{:?}", entries(&journal));
        assert!(order[2] < order[3], "roles first: {:?}", entries(&journal));
        // With the password the branch already has.
        let password = authenticator_password(&w, &dev);
        assert_eq!(
            data_api_scripts(&w, &ep).last().unwrap(),
            &data_api_sql("postgres", &password)
        );
        assert_eq!(count(&journal, "metadata.set_authenticator_password"), 0);
    }

    #[tokio::test]
    async fn a_bootstrap_failure_fails_the_flow_with_the_prefix() {
        const PREFIX: &str = "bootstrapping the data API: ";
        let fail = |w: &FakeWorker| {
            *w.computes.fail_sql_containing.lock().unwrap() =
                Some(("cheladb_admin".into(), "permission denied".into()));
        };

        // Provision.
        let (w, _journal) = worker();
        fail(&w);
        let id = w.metadata.insert_database("app", DatabaseStatus::Pending);
        w.handle_provision(id, "app").await.unwrap();
        let db = w.metadata.database(id);
        assert_eq!(db.status, DatabaseStatus::Failed);
        let error = db.error.unwrap();
        assert!(error.starts_with(PREFIX), "{error:?}");
        assert!(error.ends_with("permission denied"), "{error:?}");
        assert!(w.computes.list().await.unwrap().is_empty(), "torn down");

        // Branch create.
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        fail(&w);
        let dev = crate::fakes::pending_branch(&w, id, "dev", main.id);
        w.handle_branch_create(dev, None).await.unwrap();
        let dev = w.metadata.branch(dev);
        assert_eq!(dev.status, BranchStatus::Failed);
        let error = dev.error.unwrap();
        assert!(error.starts_with(PREFIX), "{error:?}");

        // Recreating a compute.
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let ep = main.endpoint_id.clone().unwrap();
        w.computes.remove(&ep);
        fail(&w);
        let err = w.recreate_compute(main.id, &ep).await.unwrap_err();
        assert!(err.starts_with(PREFIX), "{err:?}");
    }

    // --- Secret-key roles after a rewind ---------------------------------------------------

    fn secret_key(branch: BranchId, role: &str) -> NewDataKey {
        let generated = data_key::generate(DataKeyKind::Secret);
        NewDataKey {
            id: uuid::Uuid::new_v4(),
            branch_id: branch,
            kind: DataKeyKind::Secret,
            role: role.to_string(),
            name: format!("{role} key"),
            prefix: generated.prefix,
            sha256: generated.sha256,
            sealed: None,
        }
    }

    #[tokio::test]
    async fn bootstrap_regrants_the_roles_of_live_secret_keys() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        let main = w.metadata.main(id);
        let ep = main.endpoint_id.clone().unwrap();
        {
            let mut state = w.metadata.state();
            // Two keys for one role, granted once.
            state.data_keys.push(secret_key(main.id, "reporting"));
            state.data_keys.push(secret_key(main.id, "reporting"));
            state.data_keys.push(secret_key(main.id, "gone"));
            state.data_keys.push(secret_key(main.id, "now_super"));
            let revoked = secret_key(main.id, "old_role");
            state.revoked_keys.insert(revoked.id);
            state.data_keys.push(revoked);
            // Another branch's key.
            state
                .data_keys
                .push(secret_key(BranchId::new(), "elsewhere"));
        }
        // `gone` was dropped, `now_super` became a superuser: skipped, not fatal.
        w.computes.answer("rolname = 'gone'", "");
        w.computes.answer("rolname = 'now_super'", "superuser\n");
        answer_check(&w, "ok\n");
        journal.lock().unwrap().clear();

        w.bootstrap_data_api(&main, "app").await.unwrap();

        assert_eq!(grants(&w), ["GRANT \"reporting\" TO authenticator;"]);
        let sql = w.computes.sql.lock().unwrap().clone();
        let script = sql
            .iter()
            .position(|(_, _, s)| s.starts_with(DATA_API))
            .unwrap();
        let grant = sql
            .iter()
            .position(|(_, _, s)| s.starts_with("GRANT"))
            .unwrap();
        assert!(script < grant, "after the script");
        assert!(sql.iter().all(|(e, d, _)| e == &ep && d == "app"));
        // Revoked keys and other branches' keys aren't even checked.
        assert!(
            !sql.iter()
                .any(|(_, _, s)| s.contains("old_role") || s.contains("elsewhere"))
        );
        assert!(position(&journal, "compute.sql").is_some());
    }

    // --- The secret-key role check ---------------------------------------------------------

    /// The check query's answer, as psql prints it.
    fn answer_check(w: &FakeWorker, stdout: &str) {
        w.computes.answer("pg_has_role", stdout);
    }

    fn grants(w: &FakeWorker) -> Vec<String> {
        w.computes
            .sql
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, _, s)| s.starts_with("GRANT"))
            .map(|(_, _, s)| s.clone())
            .collect()
    }

    fn refused(message: &str) -> RoleCheck {
        RoleCheck::Refused(message.to_string())
    }

    #[tokio::test]
    async fn check_role_refusals() {
        let cases = [
            ("ghost", "", "the role ghost does not exist"),
            ("boss", "superuser\n", "the role boss is a superuser"),
            (
                "owner",
                "neon_superuser\n",
                "the role owner is a member of neon_superuser",
            ),
            (
                "shell",
                "platform\n",
                "the role shell has platform privileges",
            ),
        ];
        for (role, stdout, message) in cases {
            let (w, _journal) = worker();
            let id = ready_database(&w).await;
            answer_check(&w, stdout);
            let main = BranchId::from(id);
            assert_eq!(
                w.check_data_role(main, role).await,
                refused(message),
                "{role}"
            );
            assert!(grants(&w).is_empty(), "{role}");
            // The check ran on the branch's compute and database.
            let sql = w.computes.sql.lock().unwrap().clone();
            let check = sql
                .iter()
                .find(|(_, _, s)| s.contains("pg_has_role"))
                .expect("the check ran");
            assert_eq!(check.0, w.metadata.main(id).endpoint_id.unwrap());
            assert_eq!(check.1, "app");
            assert_eq!(check.2, role_check_sql(role));
        }
    }

    #[test]
    fn the_check_sql_covers_every_platform_role_by_oid() {
        let sql = role_check_sql("app");
        assert!(
            sql.contains("FROM pg_roles r WHERE r.rolname = 'app'"),
            "{sql}"
        );
        // Membership at any depth, by oid: a role missing from the image just matches nothing.
        assert!(sql.contains("pg_has_role(r.oid, s.oid, 'MEMBER')"), "{sql}");
        assert!(
            !sql.contains("pg_has_role(r.oid, 'neon_superuser'"),
            "{sql}"
        );
        for platform in [
            "s.rolsuper",
            "'neon_superuser'",
            "'cheladb_admin'",
            "'pg_execute_server_program'",
            "'pg_read_server_files'",
            "'pg_write_server_files'",
        ] {
            assert!(sql.contains(platform), "{platform} in {sql}");
        }
        assert!(sql.contains("WHEN r.rolsuper THEN 'superuser'"), "{sql}");
    }

    #[tokio::test]
    async fn check_role_refuses_reserved_and_built_in_roles_before_sql() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        answer_check(&w, "ok\n");
        journal.lock().unwrap().clear();
        let main = BranchId::from(id);
        for (role, message) in [
            ("authenticator", "the role authenticator is reserved"),
            ("cheladb_admin", "the role cheladb_admin is reserved"),
            (
                "pg_execute_server_program",
                "the role pg_execute_server_program is a built-in role",
            ),
            ("pg_monitor", "the role pg_monitor is a built-in role"),
            ("PG_Custom", "the role PG_Custom is a built-in role"),
        ] {
            assert_eq!(
                w.check_data_role(main, role).await,
                refused(message),
                "{role}"
            );
        }
        assert_eq!(count(&journal, "compute.sql"), 0, "{:?}", entries(&journal));
    }

    #[tokio::test]
    async fn check_data_role_rpc_replies() {
        use cheladb_nats::{CheckDataRoleReply, CheckDataRoleRequest};
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let main = BranchId::from(id);
        let ask = |role: &str, branch| CheckDataRoleRequest { branch_id: branch, role: role.into() };

        answer_check(&w, "ok\n");
        assert_eq!(w.check_data_role_reply(&ask("reporting", main)).await, CheckDataRoleReply::ok());

        w.computes.answers.lock().unwrap().clear();
        answer_check(&w, "superuser\n");
        let reply = w.check_data_role_reply(&ask("boss", main)).await;
        assert!(!reply.ok);
        assert_eq!(reply.kind.as_deref(), Some("refused"));
        assert_eq!(reply.error.as_deref(), Some("the role boss is a superuser"));

        let reply = w.check_data_role_reply(&ask("authenticator", main)).await;
        assert_eq!(reply.kind.as_deref(), Some("refused"));

        let reply = w.check_data_role_reply(&ask("anon", BranchId::new())).await;
        assert!(!reply.ok);
        assert_eq!(reply.kind.as_deref(), Some("not_ready"));

        w.metadata.state().fail = Some("branch_connection");
        let reply = w.check_data_role_reply(&ask("anon", main)).await;
        assert_eq!(reply.kind.as_deref(), Some("failed"));
    }

    #[tokio::test]
    async fn check_role_not_ready_and_failures() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        let main = BranchId::from(id);
        journal.lock().unwrap().clear();
        // Resetting, or unknown: not ready, before any SQL.
        w.metadata.state().branches.get_mut(&main).unwrap().status = BranchStatus::Resetting;
        assert_eq!(w.check_data_role(main, "anon").await, RoleCheck::NotReady);
        assert_eq!(
            w.check_data_role(BranchId::new(), "anon").await,
            RoleCheck::NotReady
        );
        assert_eq!(count(&journal, "compute.sql"), 0);
        w.metadata.state().branches.get_mut(&main).unwrap().status = BranchStatus::Ready;
        // Its compute unreachable.
        *w.computes.sql_failures_left.lock().unwrap() = 1;
        assert_eq!(w.check_data_role(main, "anon").await, RoleCheck::NotReady);
        // Metadata trouble.
        w.metadata.state().fail = Some("branch_connection");
        assert!(matches!(
            w.check_data_role(main, "anon").await,
            RoleCheck::Failed(_)
        ));
        w.metadata.state().fail = None;
        // An answer it doesn't know.
        answer_check(&w, "maybe\n");
        assert!(matches!(
            w.check_data_role(main, "anon").await,
            RoleCheck::Failed(_)
        ));
        // A failed GRANT.
        w.computes.answers.lock().unwrap().clear();
        answer_check(&w, "ok\n");
        *w.computes.fail_sql_containing.lock().unwrap() =
            Some(("GRANT".into(), "circular membership".into()));
        assert!(matches!(
            w.check_data_role(main, "anon").await,
            RoleCheck::Failed(_)
        ));
    }

    #[tokio::test]
    async fn check_role_rejects_invalid_names_before_sql() {
        let (w, journal) = worker();
        let id = ready_database(&w).await;
        answer_check(&w, "ok\n");
        journal.lock().unwrap().clear();
        let too_long = format!("a{}", "b".repeat(63));
        for bad in [
            "",
            "1abc",
            "a-b",
            "a b",
            "a\"b",
            "a'b",
            "x;DROP ROLE y",
            "é",
            "$a",
            too_long.as_str(),
        ] {
            let RoleCheck::Refused(err) = w.check_data_role(BranchId::from(id), bad).await else {
                panic!("{bad:?} not refused");
            };
            assert!(err.contains("not a valid role name"), "{bad:?}: {err}");
        }
        assert_eq!(count(&journal, "compute.sql"), 0, "{:?}", entries(&journal));
        // The edges that are fine.
        let longest = format!("a{}", "b".repeat(62));
        for good in ["_", "a$1", "Ab_9", longest.as_str()] {
            assert_eq!(
                w.check_data_role(BranchId::from(id), good).await,
                RoleCheck::Ok
            );
        }
    }

    #[tokio::test]
    async fn check_role_grants_to_authenticator() {
        let (w, _journal) = worker();
        let id = ready_database(&w).await;
        let ep = w.metadata.main(id).endpoint_id.unwrap();
        answer_check(&w, "ok\n");
        for role in ["app_user", "AppUser", "anon", "authenticated"] {
            assert_eq!(
                w.check_data_role(BranchId::from(id), role).await,
                RoleCheck::Ok,
                "{role}"
            );
        }
        assert_eq!(
            grants(&w),
            [
                "GRANT \"app_user\" TO authenticator;",
                "GRANT \"AppUser\" TO authenticator;",
                "GRANT \"anon\" TO authenticator;",
                "GRANT \"authenticated\" TO authenticator;",
            ]
        );
        let sql = w.computes.sql.lock().unwrap().clone();
        assert!(
            sql.iter()
                .filter(|(_, _, s)| s.starts_with("GRANT"))
                .all(|(e, d, _)| e == &ep && d == "app")
        );
    }
}
