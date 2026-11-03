//! What the model never contains (P6), and the names of the merge's own temporary objects.
//!
//! The catalog SQL applies these lists (T2). The names are copied from the worker; a worker test
//! asserts they still match its constants (T7: `exclusion_lists_match_worker_constants`).

/// Schemas never read: Postgres's own, and Chelabase's (`chelabase`: the data API bootstrap;
/// `chelabase_merge`: the merge's recovery marker, P1; `chelabase_guard`: the protected-branch
/// DDL guard, TE-A P5).
pub const EXCLUDED_SCHEMAS: [&str; 6] = [
    "pg_catalog",
    "information_schema",
    "pg_toast",
    "chelabase",
    "chelabase_merge",
    "chelabase_guard",
];

/// Schema name prefixes never read: TOAST schemas (`pg_toast`, `pg_toast_temp_<n>`) and
/// temporary schemas (`pg_temp_<n>`).
pub const EXCLUDED_SCHEMA_PREFIXES: [&str; 2] = ["pg_toast", "pg_temp_"];

/// Roles never read as roles (besides every `pg_*` role, [`RESERVED_ROLE_PREFIX`]): the data
/// API's (`services/worker/src/provision.rs` `bootstrap_sql`: `anon`, `authenticated`,
/// `authenticator`; `services/worker/src/data_api.rs` `data_api_script`: `chelabase_admin`), app
/// users' (`app_users_sql.rs` `USERS_SYNC_ROLE`), the compute's admin and the owner-level group.
/// Memberships *in* these roles stay on the member (owner-level is the membership in
/// `neon_superuser`).
pub const RESERVED_ROLES: [&str; 7] = [
    "authenticator",
    "anon",
    "authenticated",
    "chelabase_admin",
    "chelabase_auth_sync",
    "cloud_admin",
    "neon_superuser",
];

/// Postgres's predefined roles.
pub const RESERVED_ROLE_PREFIX: &str = "pg_";

/// Chelabase's own roles, listed in [`RESERVED_ROLES`] or not (any case): the roles path
/// reserves every `chelabase_…` name, so the merge never reads one (T14 M7).
pub const CHELABASE_ROLE_PREFIX: &str = "chelabase_";

/// The hidden per-member login roles (any case): never read as roles, and grants to them never
/// appear in any ACL. Equals `chelabase_types::role_name::MEMBER_ROLE_PREFIX`.
pub const MEMBER_ROLE_PREFIX: &str = "cb_m_";

/// The owner role the data API's default privileges are written for (`FOR ROLE <owner>`):
/// `plan::OWNER_ROLE`.
pub const DATA_API_DEFAULT_ACL_ROLE: &str = "postgres";

/// E16: the data API's global default-privilege row (`services/worker/src/data_api.rs`:
/// `ALTER DEFAULT PRIVILEGES FOR ROLE postgres REVOKE EXECUTE ON FUNCTIONS FROM PUBLIC`) is
/// excluded, in the catalog SQL's words: a global row (`defaclnamespace = 0`) on functions
/// (`'f'`) of the owner whose ACL is exactly the owner's own EXECUTE. Any other row, a user's
/// own revoke for another role or with another grant included, stays. *Cost: a user who writes
/// the same row by hand won't see it merged.*
pub const DATA_API_FUNCTIONS_DEFAULT_ACL: &str = "AND NOT (d.defaclnamespace = 0 AND d.defaclobjtype = 'f' AND dr.rolname = 'postgres'\n         AND d.defaclacl = ARRAY[makeaclitem(d.defaclrole, d.defaclrole, 'EXECUTE', false)])";

/// The app users sync role (`services/worker/src/app_users_sql.rs`, `USERS_SYNC_ROLE`): its
/// grants on the users table are excluded.
pub const USERS_SYNC_ROLE: &str = "chelabase_auth_sync";

/// The users table's columns Chelabase manages (`services/worker/src/app_users_sql.rs`,
/// `MANAGED_COLUMNS`); every other column of the users table merges.
pub const USERS_MANAGED_COLUMNS: [&str; 6] =
    ["id", "email", "email_verified", "created_at", "deleted_at", "banned_at"];

/// The users table's guard triggers (`app_users_sql.rs`).
pub const USERS_GUARD_TRIGGERS: [&str; 2] =
    ["chelabase_users_guard", "chelabase_users_guard_truncate"];

/// The users table's own-row policies (`app_users_sql.rs`).
pub const USERS_POLICIES: [&str; 2] = ["chelabase_users_select_own", "chelabase_users_update_own"];

/// The users table's email index (`app_users_sql.rs`).
pub const USERS_EMAIL_INDEX: &str = "chelabase_users_email_key";

/// Roles whose grants on the users table are excluded.
pub const USERS_GRANTEES: [&str; 2] = ["authenticated", USERS_SYNC_ROLE];

/// The data API's row naming the users table (`app_users_sql.rs` `users_bootstrap_sql`:
/// `schema_name`, `table_name`, `table_oid`). The catalog reads it only when it exists.
pub const APP_USERS_CONFIG_TABLE: &str = "chelabase.app_users_table";

/// Grantees of the data API's per-schema default privileges
/// (`services/worker/src/data_api.rs` `data_api_script`: `ALTER DEFAULT PRIVILEGES FOR ROLE
/// <owner> IN SCHEMA <served> GRANT … TO anon / authenticated`): their entries in per-schema
/// `pg_default_acl` rows are excluded.
pub const DATA_API_DEFAULT_GRANTEES: [&str; 2] = ["anon", "authenticated"];

/// Temporary NOT NULL checks: `chelabase_merge_nn_<attnum>_<merge id as 32 hex>` (unique per
/// merge; `attnum` only tells the table's columns apart, see the planner's `not_null_online`).
/// Leftovers are reported as `ReportedKind::MergeLeftover`.
pub const MERGE_NOT_NULL_CHECK_PREFIX: &str = "chelabase_merge_nn_";

/// Replacement indexes: `chelabase_merge_ix_<n>_<merge id as 32 hex>` (unique per merge).
/// Leftovers are reported as `ReportedKind::MergeLeftover`.
pub const MERGE_INDEX_PREFIX: &str = "chelabase_merge_ix_";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guard_schema_is_excluded() {
        assert!(EXCLUDED_SCHEMAS.contains(&"chelabase_guard"));
    }
}
