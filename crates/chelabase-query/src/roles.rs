//! Role checks shared by rest and the realtime service.

/// Whether role `$1` may not back a secret key (E33), exactly what the worker's create-time check
/// refuses: superuser, an attribute that administers (CREATEROLE, CREATEDB, REPLICATION), or
/// membership at any depth in a superuser role or one of
/// [`chelabase_types::data_key::KEY_ROLE_REFUSED_GROUPS`] (`neon_superuser` and the system
/// roles). BYPASSRLS alone isn't refused. No row: the role doesn't exist.
pub const OWNER_LEVEL_SQL: &str = "SELECT r.rolsuper OR r.rolcreaterole OR r.rolcreatedb OR r.rolreplication \
    OR EXISTS (SELECT FROM pg_catalog.pg_roles s WHERE (s.rolsuper OR s.rolname IN ('neon_superuser', \
    'chelabase_admin', 'pg_execute_server_program', 'pg_read_server_files', 'pg_write_server_files')) \
    AND pg_catalog.pg_has_role(r.oid, s.oid, 'MEMBER')) \
    FROM pg_catalog.pg_roles r WHERE r.rolname = $1";
