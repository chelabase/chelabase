//! What the worker needs from the outside world, as traits the tests fake.

use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use chelabase_metadata::{
    AppSession, BranchAuthSync, BranchKind, ClaimOutcome, ProjectAuth, ProjectRecord,
    BranchConnectionRow, BranchDatabase, BranchDeleteOutcome, BranchRecord, BranchStatus, DataKeyRecord,
    DatabaseRecord, DatabaseSettings, DatabaseStatus, EndpointLock, EndpointState,
    InsertRoleOutcome, LiveBranch, LiveDatabase, MetadataClient, MetadataError, NeonDatabase,
    NeonEndpoint, NeonRole, NewDataKey, NewEndpoint, NewStaticEndpoint, ResyncApplied,
    ResyncWrite, RoleCopy, SchemaBase, ServedDatabase,
};
use chelabase_metadata::{ApiRoleChange, NewApiRole, RenameCopyOutcome, RoleWriteOutcome};
use chelabase_metadata::{MergeEvent, MergeRecord, MergeStatus, MergeStatusPatch};
use chelabase_metadata::members::{MemberBranchRole, MemberRecord};
use chelabase_metadata::personal_logins::PersonalLoginRecord;
use chelabase_types::access::GrantSources;
use chelabase_neon::proxy_cache::ProxyCache;
use chelabase_neon::safekeeper::SafekeeperClient;
use chelabase_neon::spec::Placement;
use chelabase_neon::storcon::LsnAtTime;
use chelabase_neon::{ComputeSigner, Lsn, NeonId, StorconClient, StorconError};
use chelabase_types::{BranchId, DatabaseId, ProjectId, TenantId};
use uuid::Uuid;
use chrono::{DateTime, Utc};

use crate::safekeepers::SafekeeperSet;

/// The metadata calls the worker makes. See [`MetadataClient`] for each one's guards.
pub trait WorkerMetadata: Send + Sync + 'static {
    /// Held while one endpoint's pending password is applied; dropping it releases the lock.
    type EndpointLock: Send;

    /// Waits until no one else holds `endpoint_id`'s lock (across worker processes).
    fn lock_endpoint(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<Self::EndpointLock, MetadataError>> + Send;
    fn get_database(
        &self,
        id: DatabaseId,
    ) -> impl Future<Output = Result<Option<DatabaseRecord>, MetadataError>> + Send;
    fn set_status(
        &self,
        id: DatabaseId,
        status: DatabaseStatus,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// The live branch linked to `endpoint_id` (not deleting or deleted); `None` for an
    /// endpoint without one (a static compute).
    fn endpoint_branch(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<Option<BranchId>, MetadataError>> + Send;
    /// RT1 T5: the branch linked to `endpoint_id` and its kind, a deleting one included (a
    /// delete stops the compute after it began); `None` for a deleted branch or an endpoint
    /// without one (a static compute).
    fn compute_branch(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<Option<(BranchId, BranchKind)>, MetadataError>> + Send;
    fn get_branch(
        &self,
        id: BranchId,
    ) -> impl Future<Output = Result<Option<BranchRecord>, MetadataError>> + Send;
    /// The database's branches that aren't deleted, oldest first.
    fn list_branches(
        &self,
        db: DatabaseId,
    ) -> impl Future<Output = Result<Vec<BranchRecord>, MetadataError>> + Send;
    fn create_endpoint_for_branch(
        &self,
        id: BranchId,
        timeline_id: &str,
        ep: &NewEndpoint<'_>,
        parent_protected_seen: bool,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn set_neon_tenant(
        &self,
        db: DatabaseId,
        tenant: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn neon_endpoint(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<Option<NeonEndpoint>, MetadataError>> + Send;
    fn neon_roles_versioned(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<(Vec<NeonRole>, i64), MetadataError>> + Send;
    fn neon_databases(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<Vec<NeonDatabase>, MetadataError>> + Send;
    fn set_branch_ready(
        &self,
        id: BranchId,
        sealed_password: &str,
        branched_at_lsn: Option<&str>,
        history_start: Option<DateTime<Utc>>,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn set_failed(
        &self,
        id: DatabaseId,
        error: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn mark_deleted(
        &self,
        id: DatabaseId,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn mark_branch_deleted(
        &self,
        id: BranchId,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn delete_endpoint(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    /// A static compute's endpoint row: unroutable, pinned, linked to no branch (step 6g).
    fn create_static_endpoint(
        &self,
        ep: &NewStaticEndpoint<'_>,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;

    // Schema merge bases (see `chelabase_metadata::schema_bases`).

    fn schema_base(
        &self,
        branch: BranchId,
        database_name: &str,
    ) -> impl Future<Output = Result<Option<SchemaBase>, MetadataError>> + Send;
    /// Appends an entry to the branch's base history for one database (it becomes the
    /// current base); returns its id (E18).
    fn append_schema_base(
        &self,
        base: &SchemaBase,
    ) -> impl Future<Output = Result<i64, MetadataError>> + Send;
    /// Fills entry `base.id` in place with its model and identity map; false when it's gone.
    fn update_schema_base(
        &self,
        base: &SchemaBase,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// Deletes the branch's base entries of every database not in `keep`.
    fn prune_schema_bases(
        &self,
        branch: BranchId,
        keep: &[String],
    ) -> impl Future<Output = Result<u64, MetadataError>> + Send;
    fn replace_owner_secret(
        &self,
        id: BranchId,
        endpoint_id: &str,
        role: &str,
        verifier: &str,
        sealed: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn clear_needs_reconfigure(
        &self,
        endpoint_id: &str,
        applied_version: i64,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;

    // Login roles (see `chelabase_metadata::neon`).

    /// API create: `CapReached` at `cap` copies, `Exists` when the name is taken; the row is
    /// `pending`. Bumps the version. An unknown endpoint is a database error.
    fn insert_api_role(
        &self,
        endpoint_id: &str,
        role: &NewApiRole<'_>,
        cap: u32,
    ) -> impl Future<Output = Result<InsertRoleOutcome, MetadataError>> + Send;
    /// An `api` row's new wanted state, `pending` and `attributes_pending`; `NotFound` when
    /// there's no such `api` row; `CapReached` when it would give a row without a verifier one
    /// at `cap` copies. Bumps the version.
    fn update_api_role(
        &self,
        endpoint_id: &str,
        name: &str,
        change: &ApiRoleChange,
        cap: u32,
    ) -> impl Future<Output = Result<RoleWriteOutcome, MetadataError>> + Send;
    /// A role made in SQL given a password through the API: its row (made as `sql` when it
    /// has none) gets the verifier and sealed password, `pending`; `CapReached` for a role
    /// without a verifier yet at `cap` copies. Bumps the version.
    fn store_role_secret(
        &self,
        endpoint_id: &str,
        name: &str,
        verifier: &str,
        password_enc: &str,
        cap: u32,
    ) -> impl Future<Output = Result<RoleWriteOutcome, MetadataError>> + Send;
    /// E29: moves the row of `old` (if any) to `new` with `resealed`, and `branch`'s keys of
    /// `old` with it; `Taken` when `new` has a row or live secret keys. Bumps the version.
    fn rename_role_copy(
        &self,
        endpoint_id: &str,
        branch: BranchId,
        old: &str,
        new: &str,
        resealed: Option<&str>,
        unreadable: Option<bool>,
    ) -> impl Future<Output = Result<RenameCopyOutcome, MetadataError>> + Send;
    /// Any non-owner row's verifier and sealed password, `pending`; false when there's no
    /// such row. Bumps the version.
    fn replace_role_secret(
        &self,
        endpoint_id: &str,
        name: &str,
        verifier: &str,
        password_enc: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// Clears one row's `pending` while `roles_version` is still `version`.
    /// `attributes_change`: the API write's change id, whose `attributes_pending` is cleared
    /// whatever the version; `pending` only while the version holds (true when cleared).
    fn confirm_role(
        &self,
        endpoint_id: &str,
        name: &str,
        version: i64,
        attributes_change: Option<Uuid>,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// Deletes a non-owner row; false when there was none. Bumps the version.
    fn delete_role_copy(
        &self,
        endpoint_id: &str,
        name: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// Every row of the endpoint (the owner's first), and `roles_version`.
    fn role_copies(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<(Vec<RoleCopy>, i64), MetadataError>> + Send;
    /// The resync's write, only while `roles_version` is still `read_version`.
    fn apply_resync(
        &self,
        endpoint_id: &str,
        branch: BranchId,
        read_version: i64,
        write: &ResyncWrite,
    ) -> impl Future<Output = Result<ResyncApplied, MetadataError>> + Send;
    /// Lifts the endpoint's resync guards (`forwarded_at`, `dbs_forwarded_at`): its catalog
    /// was replaced (C3). Unknown endpoint: a no-op.
    fn clear_forwarded(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    /// A row whose sealed password was dropped: `password_enc` NULL, `password_unreadable`
    /// true (C4).
    fn mark_password_unreadable(
        &self,
        endpoint_id: &str,
        name: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    /// Records the compute settings version the endpoint's compute was created with.
    fn set_compute_settings_version(
        &self,
        endpoint_id: &str,
        version: i32,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn replace_compute_token(
        &self,
        endpoint_id: &str,
        sha256_hex: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn fail_if_stale(
        &self,
        id: DatabaseId,
        max_age_secs: i64,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn list_live_for_reconcile(
        &self,
    ) -> impl Future<Output = Result<Vec<LiveDatabase>, MetadataError>> + Send;
    fn list_endpoints_for_reconcile(
        &self,
    ) -> impl Future<Output = Result<Vec<EndpointState>, MetadataError>> + Send;

    // Branches (see `chelabase_metadata::branches` for each one's guards).

    /// Guarded: `from` must match, and never out of deleting/deleted. True when changed.
    fn set_branch_status(
        &self,
        id: BranchId,
        from: &[BranchStatus],
        to: BranchStatus,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// Not once a delete has begun.
    fn set_branch_failed(
        &self,
        id: BranchId,
        error: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn set_branch_error(
        &self,
        id: BranchId,
        error: Option<&str>,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    /// → deleting (and the branch's backups); refused while the branch or one of its backups
    /// has live `normal` children, or while a merge other than `except` holds the live branch
    /// (T14 F3, under its row lock).
    fn begin_branch_delete(
        &self,
        id: BranchId,
        except: Option<Uuid>,
    ) -> impl Future<Output = Result<BranchDeleteOutcome, MetadataError>> + Send;
    /// ready → resetting, clearing the last error, unless protected (and not `allow_protected`)
    /// or a merge other than `except` holds the branch (T14 F3, under its row lock).
    fn claim_branch_for_switch(
        &self,
        id: BranchId,
        allow_protected: bool,
        except: Option<Uuid>,
    ) -> impl Future<Output = Result<ClaimOutcome, MetadataError>> + Send;
    fn live_children(
        &self,
        id: BranchId,
    ) -> impl Future<Output = Result<Vec<BranchRecord>, MetadataError>> + Send;
    fn live_backups_of(
        &self,
        id: BranchId,
    ) -> impl Future<Output = Result<Vec<BranchRecord>, MetadataError>> + Send;
    fn set_pending_timeline(
        &self,
        id: BranchId,
        timeline_id: Option<&str>,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn switch_to_pending_timeline(
        &self,
        id: BranchId,
        expected_pending: &str,
        history_start: Option<DateTime<Utc>>,
        branched_at_lsn: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// `restore_lsn`: the point on the old timeline the restored one was branched at (its
    /// schema bases there are appended again, E18).
    fn switch_to_restored_timeline(
        &self,
        id: BranchId,
        expected_pending: &str,
        backup: BranchId,
        backup_name: &str,
        restore_lsn: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn clear_superseded_timeline(
        &self,
        id: BranchId,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    /// Compare-and-swap on the pending timeline (never while deleting): true when it changed.
    fn swap_pending_timeline(
        &self,
        id: BranchId,
        expected: Option<&str>,
        new: Option<&str>,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// resetting → resetting (refreshed) once the row is older than `max_age_secs`.
    fn claim_stale_reset(
        &self,
        id: BranchId,
        max_age_secs: i64,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// resetting → failed with `error` once the pending timeline was recorded more than
    /// `max_age_secs` ago; the pending timeline stays recorded.
    fn fail_stuck_restore(
        &self,
        id: BranchId,
        max_age_secs: i64,
        error: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// pending/provisioning → failed once the row is older than `max_age_secs`; never main.
    fn fail_branch_if_stale(
        &self,
        id: BranchId,
        max_age_secs: i64,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn database_settings(
        &self,
        db: DatabaseId,
    ) -> impl Future<Output = Result<Option<DatabaseSettings>, MetadataError>> + Send;
    fn list_live_branches_for_reconcile(
        &self,
    ) -> impl Future<Output = Result<Vec<LiveBranch>, MetadataError>> + Send;

    // The data API (see `chelabase_metadata::data_api`).

    /// A second live public key on the branch is [`MetadataError::Conflict`].
    fn insert_data_key(
        &self,
        key: &NewDataKey,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    /// The branch's live keys, newest first.
    fn list_data_keys(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<Vec<DataKeyRecord>, MetadataError>> + Send;
    /// The resync's repairs of secret keys versus the compute's roles (by OID).
    fn fix_key_roles(
        &self,
        branch: BranchId,
        fixes: &[chelabase_metadata::KeyRoleFix],
    ) -> impl Future<Output = Result<u64, MetadataError>> + Send;
    fn has_live_public_key(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// Stores the password only when the branch has none. A no-op on a deleting, deleted
    /// or unknown branch.
    fn set_authenticator_password(
        &self,
        branch: BranchId,
        sealed: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    /// The branch's Postgres databases with their data API state: main first, then by name.
    fn branch_databases(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<Vec<BranchDatabase>, MetadataError>> + Send;
    /// The databases the data API serves (state `on`), main first, with their default schema.
    fn served_databases(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<Vec<ServedDatabase>, MetadataError>> + Send;

    // Members (members-and-roles design, part 3: the member-role reconciler).

    /// The tenant's invited and active members.
    fn list_members(
        &self,
        tenant: TenantId,
    ) -> impl Future<Output = Result<Vec<MemberRecord>, MetadataError>> + Send;
    /// Every member's access sources on (project, database), in one query (MH-2).
    fn database_grant_sources(
        &self,
        project: Uuid,
        database: Uuid,
    ) -> impl Future<Output = Result<Vec<(Uuid, GrantSources)>, MetadataError>> + Send;
    /// The branches the member-role sweep visits.
    fn member_role_branches(&self) -> impl Future<Output = Result<Vec<Uuid>, MetadataError>> + Send;
    /// Forgets a branch's member-role rows (a reset or restore replaced its catalog).
    fn delete_branch_member_roles(
        &self,
        branch: Uuid,
    ) -> impl Future<Output = Result<u64, MetadataError>> + Send;
    fn member_branch_roles(
        &self,
        branch: Uuid,
    ) -> impl Future<Output = Result<Vec<MemberBranchRole>, MetadataError>> + Send;
    /// PB: the login roles exempt from the branch's login-role guard, by name.
    fn guard_exemptions(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<Vec<String>, MetadataError>> + Send;
    fn upsert_member_branch_role(
        &self,
        row: &MemberBranchRole,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    /// R31: each member's last acted role on `branch`, where one is recorded.
    fn member_acted_roles(
        &self,
        branch: Uuid,
    ) -> impl Future<Output = Result<Vec<(Uuid, String)>, MetadataError>> + Send;
    fn delete_member_branch_role(
        &self,
        member: Uuid,
        branch: Uuid,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    /// T6: every personal-login record of `database` (any status), with `qualifies` read in
    /// the same query.
    fn personal_login_records(
        &self,
        database: Uuid,
    ) -> impl Future<Output = Result<Vec<PersonalLoginRecord>, MetadataError>> + Send;
    /// T6: a personal `neon_roles` row for the record on `endpoint_id` (the record's own
    /// verifier; `scram_verifier` is ignored, T3-I1).
    fn upsert_personal_role_row(
        &self,
        endpoint_id: &str,
        member: Uuid,
        database: Uuid,
        role_name: &str,
        scram_verifier: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    /// T6: deletes the row only when its origin is `personal`.
    fn delete_personal_role_row(
        &self,
        endpoint_id: &str,
        role_name: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    /// T7: `(tenant, member, identity_user_id)` of every member holding a login that isn't
    /// revoked (one row per member), for the identity sweep.
    fn members_with_live_logins(
        &self,
    ) -> impl Future<Output = Result<Vec<(TenantId, Uuid, String)>, MetadataError>> + Send;
    /// T7: the webhook's and the sweep's write: the tenant member with this Zitadel user, every
    /// live login revoked (`identity_offboarded`). Returns the member's personal-row endpoints
    /// (revoked ones included); an unknown user: empty.
    fn revoke_logins_for_identity_user(
        &self,
        tenant: TenantId,
        identity_user_id: &str,
    ) -> impl Future<Output = Result<Vec<chelabase_metadata::personal_logins::LoginEndpoint>, MetadataError>> + Send;
    /// D7: `(tenant, member, identity_user_id)` of every member marked offboarded, for the
    /// identity sweep (which clears the mark once Zitadel shows the user active).
    fn offboarded_members(
        &self,
    ) -> impl Future<Output = Result<Vec<(TenantId, Uuid, String)>, MetadataError>> + Send;
    /// D7: clears the offboarded mark of the tenant member with this Zitadel user; `true` when
    /// one was cleared.
    fn clear_offboarded(
        &self,
        tenant: TenantId,
        identity_user_id: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// EOS-1: the member role's row, written only while its stored `password_enc` is still
    /// `read` (`None`: no row); `false`, nothing written, otherwise.
    fn upsert_member_branch_role_if(
        &self,
        row: &MemberBranchRole,
        read: Option<&str>,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// Ends an enable: `Ok` makes the row `on`, `Err(text)` makes it `error`. Only a row still
    /// `enabling` moves; false otherwise.
    fn finish_enable_data_api(
        &self,
        branch: BranchId,
        name: &str,
        outcome: Result<(), String>,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// Switches a non-main database's data API off; false for main or an unknown name.
    fn set_data_api_off(
        &self,
        branch: BranchId,
        name: &str,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// A new child's flags and default schemas, from its parent's rows by name (P3).
    fn copy_parent_database_flags(
        &self,
        child: BranchId,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    /// Every `(branch, database)` currently `enabling`.
    fn enabling_databases(
        &self,
    ) -> impl Future<Output = Result<Vec<(BranchId, String)>, MetadataError>> + Send;
    /// Ready `normal` branches without a live public key or an authenticator password.
    fn branches_missing_data_api(
        &self,
    ) -> impl Future<Output = Result<Vec<BranchId>, MetadataError>> + Send;
    fn branch_connection(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<Option<BranchConnectionRow>, MetadataError>> + Send;

    // Projects and app users (see `chelabase_metadata::projects` and `::app_users`).

    /// Projects `deleting` and untouched for `older_than`.
    fn deleting_projects(
        &self,
        older_than: Duration,
    ) -> impl Future<Output = Result<Vec<ProjectId>, MetadataError>> + Send;
    /// `project_auth` rows `enabling` or `disabling` and untouched for `older_than`.
    fn app_users_in_transition(
        &self,
        older_than: Duration,
    ) -> impl Future<Output = Result<Vec<ProjectAuth>, MetadataError>> + Send;
    fn project_by_id(
        &self,
        id: ProjectId,
    ) -> impl Future<Output = Result<Option<ProjectRecord>, MetadataError>> + Send;
    /// The project's databases that aren't deleted, with their last error.
    fn project_databases(
        &self,
        id: ProjectId,
    ) -> impl Future<Output = Result<Vec<(DatabaseId, DatabaseStatus, Option<String>)>, MetadataError>> + Send;
    fn set_project_error(
        &self,
        id: ProjectId,
        error: Option<&str>,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    /// Revokes the project's sessions, drops its sync passwords and `project_auth`, and
    /// tombstones the project, in one transaction.
    fn finish_project_delete(
        &self,
        id: ProjectId,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn project_auth(
        &self,
        project: ProjectId,
    ) -> impl Future<Output = Result<Option<ProjectAuth>, MetadataError>> + Send;
    /// The `project_auth` row whose auth database is `db`, if any.
    fn app_auth_for_database(
        &self,
        db: DatabaseId,
    ) -> impl Future<Output = Result<Option<ProjectAuth>, MetadataError>> + Send;
    /// A second project claiming the org is [`MetadataError::Conflict`].
    fn set_app_users_org(
        &self,
        project: ProjectId,
        org_id: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    /// `enabled`, or `failed` with the error; only a row still `enabling` moves.
    fn finish_enable_app_users(
        &self,
        project: ProjectId,
        outcome: Result<(), String>,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn delete_project_auth(
        &self,
        project: ProjectId,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    fn enabled_app_projects(
        &self,
    ) -> impl Future<Output = Result<Vec<ProjectAuth>, MetadataError>> + Send;
    fn branch_auth_sync(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<Option<BranchAuthSync>, MetadataError>> + Send;
    /// Upserts the branch's `chelabase_auth_sync` password (sealed) and its Postgres database.
    fn set_branch_auth_sync(
        &self,
        branch: BranchId,
        pg_database: &str,
        sealed: &str,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    /// Stores the sealed sync password unless one is stored for the same Postgres database;
    /// returns the stored one.
    fn claim_branch_auth_sync(
        &self,
        branch: BranchId,
        pg_database: &str,
        sealed: &str,
    ) -> impl Future<Output = Result<String, MetadataError>> + Send;
    /// Replaces the sync password only while it still holds `expected`; returns the stored one.
    fn swap_branch_auth_sync(
        &self,
        branch: BranchId,
        pg_database: &str,
        expected: &str,
        sealed: &str,
    ) -> impl Future<Output = Result<String, MetadataError>> + Send;
    /// Revokes the user's live sessions (but `keep`; only those created before
    /// `created_before` when given); returns them.
    fn revoke_user_sessions(
        &self,
        project: ProjectId,
        user: Uuid,
        keep: Option<Uuid>,
        created_before: Option<DateTime<Utc>>,
    ) -> impl Future<Output = Result<Vec<AppSession>, MetadataError>> + Send;
    /// Revokes every live session of the project; returns them.
    fn revoke_project_sessions(
        &self,
        project: ProjectId,
    ) -> impl Future<Output = Result<Vec<AppSession>, MetadataError>> + Send;
    /// Deletes sessions revoked or expired more than `older_than` ago; the count.
    fn purge_app_sessions(
        &self,
        older_than: Duration,
    ) -> impl Future<Output = Result<u64, MetadataError>> + Send;

    // Schema merges (see `chelabase_metadata::merges`; step 6g T8a).

    fn merge(
        &self,
        id: Uuid,
    ) -> impl Future<Output = Result<Option<MergeRecord>, MetadataError>> + Send;
    /// Guarded on `from`; claims a working status, clears the claim otherwise. A second merge
    /// of one target entering `applying` is an `Err` (`merges_one_applying`).
    fn set_merge_status(
        &self,
        id: Uuid,
        from: &[MergeStatus],
        to: MergeStatus,
        patch: MergeStatusPatch,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// Refreshes the claim of a merge still in `status`; `false` when it moved on.
    fn touch_merge_claim(
        &self,
        id: Uuid,
        status: MergeStatus,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    fn append_merge_event(
        &self,
        event: &MergeEvent,
    ) -> impl Future<Output = Result<(), MetadataError>> + Send;
    /// The merge's log, oldest first.
    fn merge_events(
        &self,
        id: Uuid,
    ) -> impl Future<Output = Result<Vec<MergeEvent>, MetadataError>> + Send;
    /// A rehearsal branch (kind `rehearsal`, `pending`) under `parent`, which must be a ready
    /// branch of the database (else `Conflict`); a live branch of that name is
    /// `BranchNameTaken` (step 6g T8b).
    fn create_rehearsal_branch(
        &self,
        database_id: DatabaseId,
        parent: BranchId,
        name: &str,
    ) -> impl Future<Output = Result<BranchRecord, MetadataError>> + Send;
    /// Merges in a working status whose claim is older than `older_than` (R3), but not one
    /// handed back since its last claim less than `rehand_after` ago (T14 M10).
    fn stale_merge_claims(
        &self,
        older_than: Duration,
        rehand_after: Duration,
    ) -> impl Future<Output = Result<Vec<MergeRecord>, MetadataError>> + Send;
    /// Merges applied with drift whose after-merge choice waits (requested, or started by a run
    /// that went away) with no write for more than `older_than`: their `Finish` job was lost
    /// (T14 M8, F3 M1); not one handed back since its last write less than `rehand_after` ago
    /// (F3 M2).
    fn stale_finish_requests(
        &self,
        older_than: Duration,
        rehand_after: Duration,
    ) -> impl Future<Output = Result<Vec<MergeRecord>, MetadataError>> + Send;
    /// Records a hand-back (a job re-published) of a merge still in `status`.
    fn mark_merge_handed_back(
        &self,
        id: Uuid,
        status: MergeStatus,
    ) -> impl Future<Output = Result<bool, MetadataError>> + Send;
    /// Cancels the branch's merges nothing runs (`computing`, `draft`, `rehearsed`).
    fn cancel_drafts_for_branch(
        &self,
        branch: BranchId,
    ) -> impl Future<Output = Result<u64, MetadataError>> + Send;
    /// The database's merges, newest first; with `branch`, those where it's source or target.
    fn list_merges(
        &self,
        database_id: DatabaseId,
        branch: Option<BranchId>,
    ) -> impl Future<Output = Result<Vec<MergeRecord>, MetadataError>> + Send;
}

// Each method calls the inherent one by its full path: a bare `self.method()` would recurse
// into this impl if the inherent method were ever renamed.
impl WorkerMetadata for MetadataClient {
    type EndpointLock = EndpointLock;

    async fn lock_endpoint(&self, endpoint_id: &str) -> Result<EndpointLock, MetadataError> {
        MetadataClient::lock_endpoint(self, endpoint_id).await
    }

    async fn get_database(&self, id: DatabaseId) -> Result<Option<DatabaseRecord>, MetadataError> {
        MetadataClient::get_database(self, id).await
    }

    async fn set_status(
        &self,
        id: DatabaseId,
        status: DatabaseStatus,
    ) -> Result<bool, MetadataError> {
        MetadataClient::set_status(self, id, status).await
    }

    async fn endpoint_branch(&self, endpoint_id: &str) -> Result<Option<BranchId>, MetadataError> {
        MetadataClient::endpoint_branch(self, endpoint_id).await
    }

    async fn compute_branch(
        &self,
        endpoint_id: &str,
    ) -> Result<Option<(BranchId, BranchKind)>, MetadataError> {
        MetadataClient::compute_branch(self, endpoint_id).await
    }

    async fn get_branch(&self, id: BranchId) -> Result<Option<BranchRecord>, MetadataError> {
        MetadataClient::get_branch(self, id).await
    }

    async fn list_branches(&self, db: DatabaseId) -> Result<Vec<BranchRecord>, MetadataError> {
        MetadataClient::list_branches(self, db).await
    }

    async fn create_endpoint_for_branch(
        &self,
        id: BranchId,
        timeline_id: &str,
        ep: &NewEndpoint<'_>,
        parent_protected_seen: bool,
    ) -> Result<bool, MetadataError> {
        MetadataClient::create_endpoint_for_branch(self, id, timeline_id, ep, parent_protected_seen).await
    }

    async fn set_neon_tenant(&self, db: DatabaseId, tenant: &str) -> Result<bool, MetadataError> {
        MetadataClient::set_neon_tenant(self, db, tenant).await
    }

    async fn neon_endpoint(
        &self,
        endpoint_id: &str,
    ) -> Result<Option<NeonEndpoint>, MetadataError> {
        MetadataClient::neon_endpoint(self, endpoint_id).await
    }

    async fn neon_roles_versioned(
        &self,
        endpoint_id: &str,
    ) -> Result<(Vec<NeonRole>, i64), MetadataError> {
        MetadataClient::neon_roles_versioned(self, endpoint_id).await
    }

    async fn neon_databases(&self, endpoint_id: &str) -> Result<Vec<NeonDatabase>, MetadataError> {
        MetadataClient::neon_databases(self, endpoint_id).await
    }

    async fn set_branch_ready(
        &self,
        id: BranchId,
        sealed_password: &str,
        branched_at_lsn: Option<&str>,
        history_start: Option<DateTime<Utc>>,
    ) -> Result<bool, MetadataError> {
        MetadataClient::set_branch_ready(self, id, sealed_password, branched_at_lsn, history_start)
            .await
    }

    async fn set_failed(&self, id: DatabaseId, error: &str) -> Result<(), MetadataError> {
        MetadataClient::set_failed(self, id, error).await
    }

    async fn mark_deleted(&self, id: DatabaseId) -> Result<(), MetadataError> {
        MetadataClient::mark_deleted(self, id).await
    }

    async fn mark_branch_deleted(&self, id: BranchId) -> Result<(), MetadataError> {
        MetadataClient::mark_branch_deleted(self, id).await
    }

    async fn delete_endpoint(&self, endpoint_id: &str) -> Result<(), MetadataError> {
        MetadataClient::delete_endpoint(self, endpoint_id).await
    }

    async fn create_static_endpoint(&self, ep: &NewStaticEndpoint<'_>) -> Result<(), MetadataError> {
        MetadataClient::create_static_endpoint(self, ep).await
    }

    async fn schema_base(
        &self,
        branch: BranchId,
        database_name: &str,
    ) -> Result<Option<SchemaBase>, MetadataError> {
        MetadataClient::schema_base(self, branch, database_name).await
    }

    async fn append_schema_base(&self, base: &SchemaBase) -> Result<i64, MetadataError> {
        MetadataClient::append_schema_base(self, base).await
    }

    async fn update_schema_base(&self, base: &SchemaBase) -> Result<bool, MetadataError> {
        MetadataClient::update_schema_base(self, base).await
    }

    async fn prune_schema_bases(
        &self,
        branch: BranchId,
        keep: &[String],
    ) -> Result<u64, MetadataError> {
        MetadataClient::prune_schema_bases(self, branch, keep).await
    }

    async fn replace_owner_secret(
        &self,
        id: BranchId,
        endpoint_id: &str,
        role: &str,
        verifier: &str,
        sealed: &str,
    ) -> Result<bool, MetadataError> {
        MetadataClient::replace_owner_secret(self, id, endpoint_id, role, verifier, sealed).await
    }

    async fn clear_needs_reconfigure(
        &self,
        endpoint_id: &str,
        applied_version: i64,
    ) -> Result<bool, MetadataError> {
        MetadataClient::clear_needs_reconfigure(self, endpoint_id, applied_version).await
    }

    async fn insert_api_role(
        &self,
        endpoint_id: &str,
        role: &NewApiRole<'_>,
        cap: u32,
    ) -> Result<InsertRoleOutcome, MetadataError> {
        MetadataClient::insert_api_role_with(self, endpoint_id, role, cap).await
    }

    async fn update_api_role(
        &self,
        endpoint_id: &str,
        name: &str,
        change: &ApiRoleChange,
        cap: u32,
    ) -> Result<RoleWriteOutcome, MetadataError> {
        MetadataClient::update_api_role(self, endpoint_id, name, change, cap).await
    }

    async fn store_role_secret(
        &self,
        endpoint_id: &str,
        name: &str,
        verifier: &str,
        password_enc: &str,
        cap: u32,
    ) -> Result<RoleWriteOutcome, MetadataError> {
        MetadataClient::store_role_secret(self, endpoint_id, name, verifier, password_enc, cap)
            .await
    }

    async fn rename_role_copy(
        &self,
        endpoint_id: &str,
        branch: BranchId,
        old: &str,
        new: &str,
        resealed: Option<&str>,
        unreadable: Option<bool>,
    ) -> Result<RenameCopyOutcome, MetadataError> {
        MetadataClient::rename_role_copy(self, endpoint_id, branch, old, new, resealed, unreadable)
            .await
    }

    async fn replace_role_secret(
        &self,
        endpoint_id: &str,
        name: &str,
        verifier: &str,
        password_enc: &str,
    ) -> Result<bool, MetadataError> {
        MetadataClient::replace_role_secret(self, endpoint_id, name, verifier, password_enc).await
    }

    async fn confirm_role(
        &self,
        endpoint_id: &str,
        name: &str,
        version: i64,
        attributes_change: Option<Uuid>,
    ) -> Result<bool, MetadataError> {
        MetadataClient::confirm_role(self, endpoint_id, name, version, attributes_change).await
    }

    async fn delete_role_copy(&self, endpoint_id: &str, name: &str) -> Result<bool, MetadataError> {
        MetadataClient::delete_role_copy(self, endpoint_id, name).await
    }

    async fn role_copies(&self, endpoint_id: &str) -> Result<(Vec<RoleCopy>, i64), MetadataError> {
        MetadataClient::role_copies(self, endpoint_id).await
    }

    async fn apply_resync(
        &self,
        endpoint_id: &str,
        branch: BranchId,
        read_version: i64,
        write: &ResyncWrite,
    ) -> Result<ResyncApplied, MetadataError> {
        MetadataClient::apply_resync(self, endpoint_id, branch, read_version, write).await
    }

    async fn clear_forwarded(&self, endpoint_id: &str) -> Result<(), MetadataError> {
        MetadataClient::clear_forwarded(self, endpoint_id).await
    }

    async fn mark_password_unreadable(
        &self,
        endpoint_id: &str,
        name: &str,
    ) -> Result<(), MetadataError> {
        MetadataClient::mark_password_unreadable(self, endpoint_id, name).await
    }

    async fn set_compute_settings_version(
        &self,
        endpoint_id: &str,
        version: i32,
    ) -> Result<(), MetadataError> {
        MetadataClient::set_compute_settings_version(self, endpoint_id, version).await
    }

    async fn replace_compute_token(
        &self,
        endpoint_id: &str,
        sha256_hex: &str,
    ) -> Result<(), MetadataError> {
        MetadataClient::replace_compute_token(self, endpoint_id, sha256_hex).await
    }

    async fn fail_if_stale(
        &self,
        id: DatabaseId,
        max_age_secs: i64,
    ) -> Result<bool, MetadataError> {
        MetadataClient::fail_if_stale(self, id, max_age_secs).await
    }

    async fn list_live_for_reconcile(&self) -> Result<Vec<LiveDatabase>, MetadataError> {
        MetadataClient::list_live_for_reconcile(self).await
    }

    async fn list_endpoints_for_reconcile(&self) -> Result<Vec<EndpointState>, MetadataError> {
        MetadataClient::list_endpoints_for_reconcile(self).await
    }

    async fn set_branch_status(
        &self,
        id: BranchId,
        from: &[BranchStatus],
        to: BranchStatus,
    ) -> Result<bool, MetadataError> {
        MetadataClient::set_branch_status(self, id, from, to).await
    }

    async fn set_branch_failed(&self, id: BranchId, error: &str) -> Result<(), MetadataError> {
        MetadataClient::set_branch_failed(self, id, error).await
    }

    async fn set_branch_error(
        &self,
        id: BranchId,
        error: Option<&str>,
    ) -> Result<(), MetadataError> {
        MetadataClient::set_branch_error(self, id, error).await
    }

    async fn begin_branch_delete(
        &self,
        id: BranchId,
        except: Option<Uuid>,
    ) -> Result<BranchDeleteOutcome, MetadataError> {
        MetadataClient::begin_branch_delete_except(self, id, except).await
    }

    async fn claim_branch_for_switch(
        &self,
        id: BranchId,
        allow_protected: bool,
        except: Option<Uuid>,
    ) -> Result<ClaimOutcome, MetadataError> {
        MetadataClient::claim_branch_for_switch_except(self, id, allow_protected, except).await
    }

    async fn live_children(&self, id: BranchId) -> Result<Vec<BranchRecord>, MetadataError> {
        MetadataClient::live_children(self, id).await
    }

    async fn live_backups_of(&self, id: BranchId) -> Result<Vec<BranchRecord>, MetadataError> {
        MetadataClient::live_backups_of(self, id).await
    }

    async fn set_pending_timeline(
        &self,
        id: BranchId,
        timeline_id: Option<&str>,
    ) -> Result<bool, MetadataError> {
        MetadataClient::set_pending_timeline(self, id, timeline_id).await
    }

    async fn switch_to_pending_timeline(
        &self,
        id: BranchId,
        expected_pending: &str,
        history_start: Option<DateTime<Utc>>,
        branched_at_lsn: &str,
    ) -> Result<bool, MetadataError> {
        MetadataClient::switch_to_pending_timeline(
            self,
            id,
            expected_pending,
            history_start,
            branched_at_lsn,
        )
        .await
    }

    async fn switch_to_restored_timeline(
        &self,
        id: BranchId,
        expected_pending: &str,
        backup: BranchId,
        backup_name: &str,
        restore_lsn: &str,
    ) -> Result<bool, MetadataError> {
        MetadataClient::switch_to_restored_timeline(
            self,
            id,
            expected_pending,
            backup,
            backup_name,
            restore_lsn,
        )
        .await
    }

    async fn clear_superseded_timeline(&self, id: BranchId) -> Result<(), MetadataError> {
        MetadataClient::clear_superseded_timeline(self, id).await
    }

    async fn swap_pending_timeline(
        &self,
        id: BranchId,
        expected: Option<&str>,
        new: Option<&str>,
    ) -> Result<bool, MetadataError> {
        MetadataClient::swap_pending_timeline(self, id, expected, new).await
    }

    async fn claim_stale_reset(
        &self,
        id: BranchId,
        max_age_secs: i64,
    ) -> Result<bool, MetadataError> {
        MetadataClient::claim_stale_reset(self, id, max_age_secs).await
    }

    async fn fail_stuck_restore(
        &self,
        id: BranchId,
        max_age_secs: i64,
        error: &str,
    ) -> Result<bool, MetadataError> {
        MetadataClient::fail_stuck_restore(self, id, max_age_secs, error).await
    }

    async fn fail_branch_if_stale(
        &self,
        id: BranchId,
        max_age_secs: i64,
    ) -> Result<bool, MetadataError> {
        MetadataClient::fail_branch_if_stale(self, id, max_age_secs).await
    }

    async fn database_settings(
        &self,
        db: DatabaseId,
    ) -> Result<Option<DatabaseSettings>, MetadataError> {
        MetadataClient::database_settings(self, db).await
    }

    async fn list_live_branches_for_reconcile(&self) -> Result<Vec<LiveBranch>, MetadataError> {
        MetadataClient::list_live_branches_for_reconcile(self).await
    }

    async fn insert_data_key(&self, key: &NewDataKey) -> Result<(), MetadataError> {
        MetadataClient::insert_data_key(self, key).await
    }

    async fn list_data_keys(&self, branch: BranchId) -> Result<Vec<DataKeyRecord>, MetadataError> {
        MetadataClient::list_data_keys(self, branch).await
    }

    async fn fix_key_roles(
        &self,
        branch: BranchId,
        fixes: &[chelabase_metadata::KeyRoleFix],
    ) -> Result<u64, MetadataError> {
        MetadataClient::fix_key_roles(self, branch, fixes).await
    }

    async fn has_live_public_key(&self, branch: BranchId) -> Result<bool, MetadataError> {
        MetadataClient::has_live_public_key(self, branch).await
    }

    async fn set_authenticator_password(
        &self,
        branch: BranchId,
        sealed: &str,
    ) -> Result<(), MetadataError> {
        MetadataClient::set_authenticator_password(self, branch, sealed).await
    }

    async fn branch_databases(
        &self,
        branch: BranchId,
    ) -> Result<Vec<BranchDatabase>, MetadataError> {
        MetadataClient::branch_databases(self, branch).await
    }

    async fn list_members(&self, tenant: TenantId) -> Result<Vec<MemberRecord>, MetadataError> {
        MetadataClient::list_members(self, tenant).await
    }

    async fn database_grant_sources(
        &self,
        project: Uuid,
        database: Uuid,
    ) -> Result<Vec<(Uuid, GrantSources)>, MetadataError> {
        MetadataClient::database_grant_sources(self, project, database).await
    }

    async fn member_role_branches(&self) -> Result<Vec<Uuid>, MetadataError> {
        MetadataClient::member_role_branches(self).await
    }

    async fn delete_branch_member_roles(&self, branch: Uuid) -> Result<u64, MetadataError> {
        MetadataClient::delete_branch_member_roles(self, branch).await
    }

    async fn member_branch_roles(
        &self,
        branch: Uuid,
    ) -> Result<Vec<MemberBranchRole>, MetadataError> {
        MetadataClient::member_branch_roles(self, branch).await
    }

    async fn guard_exemptions(&self, branch: BranchId) -> Result<Vec<String>, MetadataError> {
        MetadataClient::list_guard_exemptions(self, branch).await
    }

    async fn upsert_member_branch_role(&self, row: &MemberBranchRole) -> Result<(), MetadataError> {
        MetadataClient::upsert_member_branch_role(self, row).await
    }

    async fn member_acted_roles(&self, branch: Uuid) -> Result<Vec<(Uuid, String)>, MetadataError> {
        MetadataClient::member_acted_roles(self, branch).await
    }

    async fn delete_member_branch_role(&self, member: Uuid, branch: Uuid) -> Result<(), MetadataError> {
        MetadataClient::delete_member_branch_role(self, member, branch).await
    }

    async fn personal_login_records(
        &self,
        database: Uuid,
    ) -> Result<Vec<PersonalLoginRecord>, MetadataError> {
        MetadataClient::personal_login_records(self, database).await
    }

    async fn members_with_live_logins(&self) -> Result<Vec<(TenantId, Uuid, String)>, MetadataError> {
        MetadataClient::members_with_live_logins(self).await
    }

    async fn revoke_logins_for_identity_user(
        &self,
        tenant: TenantId,
        identity_user_id: &str,
    ) -> Result<Vec<chelabase_metadata::personal_logins::LoginEndpoint>, MetadataError> {
        MetadataClient::revoke_logins_for_identity_user(self, tenant, identity_user_id).await
    }

    async fn offboarded_members(&self) -> Result<Vec<(TenantId, Uuid, String)>, MetadataError> {
        MetadataClient::offboarded_members(self).await
    }

    async fn clear_offboarded(&self, tenant: TenantId, identity_user_id: &str) -> Result<bool, MetadataError> {
        MetadataClient::clear_offboarded(self, tenant, identity_user_id).await
    }

    async fn upsert_member_branch_role_if(
        &self,
        row: &MemberBranchRole,
        read: Option<&str>,
    ) -> Result<bool, MetadataError> {
        MetadataClient::upsert_member_branch_role_if(self, row, read).await
    }

    async fn upsert_personal_role_row(
        &self,
        endpoint_id: &str,
        member: Uuid,
        database: Uuid,
        role_name: &str,
        scram_verifier: &str,
    ) -> Result<(), MetadataError> {
        MetadataClient::upsert_personal_role_row(self, endpoint_id, member, database, role_name, scram_verifier)
            .await
    }

    async fn delete_personal_role_row(&self, endpoint_id: &str, role_name: &str) -> Result<(), MetadataError> {
        MetadataClient::delete_personal_role_row(self, endpoint_id, role_name).await
    }

    async fn served_databases(
        &self,
        branch: BranchId,
    ) -> Result<Vec<ServedDatabase>, MetadataError> {
        MetadataClient::served_databases(self, branch).await
    }

    async fn finish_enable_data_api(
        &self,
        branch: BranchId,
        name: &str,
        outcome: Result<(), String>,
    ) -> Result<bool, MetadataError> {
        MetadataClient::finish_enable_data_api(self, branch, name, outcome).await
    }

    async fn set_data_api_off(&self, branch: BranchId, name: &str) -> Result<bool, MetadataError> {
        MetadataClient::set_data_api_off(self, branch, name).await
    }

    async fn copy_parent_database_flags(&self, child: BranchId) -> Result<(), MetadataError> {
        MetadataClient::copy_parent_database_flags(self, child).await
    }

    async fn enabling_databases(&self) -> Result<Vec<(BranchId, String)>, MetadataError> {
        MetadataClient::enabling_databases(self).await
    }

    async fn branches_missing_data_api(&self) -> Result<Vec<BranchId>, MetadataError> {
        MetadataClient::branches_missing_data_api(self).await
    }

    async fn branch_connection(
        &self,
        branch: BranchId,
    ) -> Result<Option<BranchConnectionRow>, MetadataError> {
        MetadataClient::branch_connection(self, branch).await
    }

    async fn deleting_projects(&self, older_than: Duration) -> Result<Vec<ProjectId>, MetadataError> {
        MetadataClient::deleting_projects(self, older_than).await
    }

    async fn app_users_in_transition(
        &self,
        older_than: Duration,
    ) -> Result<Vec<ProjectAuth>, MetadataError> {
        MetadataClient::app_users_in_transition(self, older_than).await
    }

    async fn project_by_id(&self, id: ProjectId) -> Result<Option<ProjectRecord>, MetadataError> {
        MetadataClient::project_by_id(self, id).await
    }

    async fn project_databases(
        &self,
        id: ProjectId,
    ) -> Result<Vec<(DatabaseId, DatabaseStatus, Option<String>)>, MetadataError> {
        MetadataClient::project_databases(self, id).await
    }

    async fn set_project_error(&self, id: ProjectId, error: Option<&str>) -> Result<(), MetadataError> {
        MetadataClient::set_project_error(self, id, error).await
    }

    async fn finish_project_delete(&self, id: ProjectId) -> Result<(), MetadataError> {
        MetadataClient::finish_project_delete(self, id).await
    }

    async fn project_auth(&self, project: ProjectId) -> Result<Option<ProjectAuth>, MetadataError> {
        MetadataClient::project_auth(self, project).await
    }

    async fn app_auth_for_database(
        &self,
        db: DatabaseId,
    ) -> Result<Option<ProjectAuth>, MetadataError> {
        MetadataClient::app_auth_for_database(self, db).await
    }

    async fn set_app_users_org(&self, project: ProjectId, org_id: &str) -> Result<(), MetadataError> {
        MetadataClient::set_app_users_org(self, project, org_id).await
    }

    async fn finish_enable_app_users(
        &self,
        project: ProjectId,
        outcome: Result<(), String>,
    ) -> Result<bool, MetadataError> {
        MetadataClient::finish_enable_app_users(self, project, outcome).await
    }

    async fn delete_project_auth(&self, project: ProjectId) -> Result<(), MetadataError> {
        MetadataClient::delete_project_auth(self, project).await
    }

    async fn enabled_app_projects(&self) -> Result<Vec<ProjectAuth>, MetadataError> {
        MetadataClient::enabled_app_projects(self).await
    }

    async fn branch_auth_sync(&self, branch: BranchId) -> Result<Option<BranchAuthSync>, MetadataError> {
        MetadataClient::branch_auth_sync(self, branch).await
    }

    async fn set_branch_auth_sync(
        &self,
        branch: BranchId,
        pg_database: &str,
        sealed: &str,
    ) -> Result<(), MetadataError> {
        MetadataClient::set_branch_auth_sync(self, branch, pg_database, sealed).await
    }

    async fn claim_branch_auth_sync(
        &self,
        branch: BranchId,
        pg_database: &str,
        sealed: &str,
    ) -> Result<String, MetadataError> {
        MetadataClient::claim_branch_auth_sync(self, branch, pg_database, sealed).await
    }

    async fn swap_branch_auth_sync(
        &self,
        branch: BranchId,
        pg_database: &str,
        expected: &str,
        sealed: &str,
    ) -> Result<String, MetadataError> {
        MetadataClient::swap_branch_auth_sync(self, branch, pg_database, expected, sealed).await
    }

    async fn revoke_user_sessions(
        &self,
        project: ProjectId,
        user: Uuid,
        keep: Option<Uuid>,
        created_before: Option<DateTime<Utc>>,
    ) -> Result<Vec<AppSession>, MetadataError> {
        MetadataClient::revoke_user_sessions(self, project, user, keep, created_before).await
    }

    async fn revoke_project_sessions(&self, project: ProjectId) -> Result<Vec<AppSession>, MetadataError> {
        MetadataClient::revoke_project_sessions(self, project).await
    }

    async fn purge_app_sessions(&self, older_than: Duration) -> Result<u64, MetadataError> {
        MetadataClient::purge_app_sessions(self, older_than).await
    }

    async fn merge(&self, id: Uuid) -> Result<Option<MergeRecord>, MetadataError> {
        MetadataClient::merge(self, id).await
    }

    async fn set_merge_status(
        &self,
        id: Uuid,
        from: &[MergeStatus],
        to: MergeStatus,
        patch: MergeStatusPatch,
    ) -> Result<bool, MetadataError> {
        MetadataClient::set_merge_status(self, id, from, to, patch).await
    }

    async fn touch_merge_claim(&self, id: Uuid, status: MergeStatus) -> Result<bool, MetadataError> {
        MetadataClient::touch_merge_claim(self, id, status).await
    }

    async fn append_merge_event(&self, event: &MergeEvent) -> Result<(), MetadataError> {
        MetadataClient::append_merge_event(self, event).await
    }

    async fn merge_events(&self, id: Uuid) -> Result<Vec<MergeEvent>, MetadataError> {
        MetadataClient::merge_events(self, id).await
    }

    async fn create_rehearsal_branch(
        &self,
        database_id: DatabaseId,
        parent: BranchId,
        name: &str,
    ) -> Result<BranchRecord, MetadataError> {
        MetadataClient::create_rehearsal_branch(self, database_id, parent, name, "worker").await
    }

    async fn stale_merge_claims(
        &self,
        older_than: Duration,
        rehand_after: Duration,
    ) -> Result<Vec<MergeRecord>, MetadataError> {
        MetadataClient::stale_merge_claims(self, older_than, rehand_after).await
    }

    async fn stale_finish_requests(
        &self,
        older_than: Duration,
        rehand_after: Duration,
    ) -> Result<Vec<MergeRecord>, MetadataError> {
        MetadataClient::stale_finish_requests(self, older_than, rehand_after).await
    }

    async fn mark_merge_handed_back(&self, id: Uuid, status: MergeStatus) -> Result<bool, MetadataError> {
        MetadataClient::mark_merge_handed_back(self, id, status).await
    }

    async fn cancel_drafts_for_branch(&self, branch: BranchId) -> Result<u64, MetadataError> {
        MetadataClient::cancel_drafts_for_branch(self, branch).await
    }

    async fn list_merges(
        &self,
        database_id: DatabaseId,
        branch: Option<BranchId>,
    ) -> Result<Vec<MergeRecord>, MetadataError> {
        MetadataClient::list_merges(self, database_id, branch).await
    }
}

/// Tenants and timelines on the storage controller, plus where they're placed.
pub trait Storage: Placement + 'static {
    fn create_tenant(
        &self,
        tenant: &NeonId,
    ) -> impl Future<Output = Result<(), StorconError>> + Send;
    /// A fresh Postgres 17 timeline (no ancestor).
    fn create_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> impl Future<Output = Result<(), StorconError>> + Send;
    /// Waits until the tenant is gone; one that's already gone (404) counts as done.
    fn delete_tenant(
        &self,
        tenant: &NeonId,
    ) -> impl Future<Output = Result<(), StorconError>> + Send;
    fn list_tenant_ids(&self) -> impl Future<Output = Result<Vec<NeonId>, StorconError>> + Send;
    /// A Postgres 17 timeline branched from `parent` at `lsn`.
    fn create_branch_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
        parent: &NeonId,
        lsn: Lsn,
    ) -> impl Future<Output = Result<(), StorconError>> + Send;
    /// Waits until the timeline is gone; one that's already gone (404) counts as done.
    fn delete_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> impl Future<Output = Result<(), StorconError>> + Send;
    /// The timeline's head: its `last_record_lsn`, or the safekeepers' commit LSN when ahead
    /// (see [`crate::safekeepers::SafekeeperSet::head`]).
    fn get_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> impl Future<Output = Result<Lsn, StorconError>> + Send;
    /// Where the timeline starts on its ancestor's; `None` for a timeline without one.
    fn ancestor_lsn(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> impl Future<Output = Result<Option<Lsn>, StorconError>> + Send;
    /// The timeline's ancestor and where it starts on it; `None` for a timeline without one.
    fn ancestor(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> impl Future<Output = Result<Option<(NeonId, Lsn)>, StorconError>> + Send;
    /// How far GC has advanced on the timeline: no compute can read below it, except at a
    /// point the timeline retains (a child's branch point, or a leased LSN). `None` when the
    /// storage controller doesn't say.
    fn gc_cutoff(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> impl Future<Output = Result<Option<Lsn>, StorconError>> + Send;
    fn lsn_by_timestamp(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
        at: DateTime<Utc>,
    ) -> impl Future<Output = Result<LsnAtTime, StorconError>> + Send;
    /// `None` when no timestamp is known for the LSN.
    fn timestamp_of_lsn(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
        lsn: Lsn,
    ) -> impl Future<Output = Result<Option<DateTime<Utc>>, StorconError>> + Send;
    fn detach_ancestor(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> impl Future<Output = Result<(), StorconError>> + Send;
    fn patch_tenant_config(
        &self,
        tenant: &NeonId,
        pitr_interval: Duration,
    ) -> impl Future<Output = Result<(), StorconError>> + Send;
    /// `None` when it was never set.
    fn tenant_pitr_interval(
        &self,
        tenant: &NeonId,
    ) -> impl Future<Output = Result<Option<Duration>, StorconError>> + Send;
    fn list_timelines(
        &self,
        tenant: &NeonId,
    ) -> impl Future<Output = Result<Vec<NeonId>, StorconError>> + Send;
    /// Waits up to `timeout` for the storage controller's attach notice of a fresh `tenant`
    /// (the orchestrator's tenant-attached event, carried gaps T4): once it came, the tenant's
    /// pageservers answer for it. Never an error: provisioning goes on either way (R8).
    fn wait_attached(
        &self,
        tenant: &NeonId,
        timeout: Duration,
    ) -> impl Future<Output = AttachWait> + Send;
}

/// How [`Storage::wait_attached`] ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachWait {
    /// The tenant's attach event came.
    Attached,
    /// None came within the timeout.
    TimedOut,
    /// The wait couldn't run (NATS down, no events stream, no source configured): why.
    Unavailable(String),
}

/// Where the worker reads the tenant-attached events from (A8: always given explicitly).
#[derive(Clone)]
// One per worker, built once: the unboxed context costs nothing worth an indirection.
#[allow(clippy::large_enum_variant)]
pub enum AttachSource {
    /// The events stream, through this JetStream (the worker's own NATS user).
    Nats(async_nats::jetstream::Context),
    /// No events: every wait is [`AttachWait::Unavailable`] at once.
    Disabled,
}

impl std::fmt::Debug for AttachSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Nats(_) => f.write_str("AttachSource::Nats"),
            Self::Disabled => f.write_str("AttachSource::Disabled"),
        }
    }
}

impl AttachSource {
    /// [`Storage::wait_attached`] over this source.
    pub async fn wait(&self, tenant: &NeonId, timeout: Duration) -> AttachWait {
        match self {
            Self::Disabled => AttachWait::Unavailable("no attach events configured".to_string()),
            Self::Nats(js) => {
                match chelabase_nats::wait_tenant_attached(js, tenant.as_str(), timeout).await {
                    Ok(Some(_)) => AttachWait::Attached,
                    Ok(None) => AttachWait::TimedOut,
                    Err(e) => AttachWait::Unavailable(e.to_string()),
                }
            }
        }
    }
}

/// The Postgres major version every new timeline gets (the compute image is v17).
const PG_VERSION: u32 = 17;

/// The storage controller plus the safekeepers the worker reads commit LSNs from, and the
/// tenant-attached events.
pub struct RealStorage {
    pub storcon: StorconClient,
    pub safekeepers: SafekeeperSet<SafekeeperClient>,
    pub attach: AttachSource,
}

impl Placement for RealStorage {
    async fn pageserver_connstring(&self, tenant: &NeonId) -> Result<String, StorconError> {
        self.storcon.pageserver_connstring(tenant).await
    }

    async fn active_safekeepers(&self) -> Result<Vec<String>, StorconError> {
        self.storcon.active_safekeepers().await
    }
}

impl Storage for RealStorage {
    async fn create_tenant(&self, tenant: &NeonId) -> Result<(), StorconError> {
        StorconClient::create_tenant(&self.storcon, tenant).await
    }

    async fn create_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<(), StorconError> {
        StorconClient::create_timeline(&self.storcon, tenant, timeline, PG_VERSION, None)
            .await
            .map(drop)
    }

    async fn delete_tenant(&self, tenant: &NeonId) -> Result<(), StorconError> {
        StorconClient::delete_tenant(&self.storcon, tenant).await
    }

    async fn list_tenant_ids(&self) -> Result<Vec<NeonId>, StorconError> {
        StorconClient::list_tenant_ids(&self.storcon).await
    }

    async fn create_branch_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
        parent: &NeonId,
        lsn: Lsn,
    ) -> Result<(), StorconError> {
        StorconClient::create_timeline(
            &self.storcon,
            tenant,
            timeline,
            PG_VERSION,
            Some((parent, Some(lsn))),
        )
        .await
        .map(drop)
    }

    async fn delete_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<(), StorconError> {
        StorconClient::delete_timeline(&self.storcon, tenant, timeline).await
    }

    async fn get_timeline(&self, tenant: &NeonId, timeline: &NeonId) -> Result<Lsn, StorconError> {
        let (info, active) = tokio::join!(
            StorconClient::get_timeline(&self.storcon, tenant, timeline),
            self.storcon.active_safekeeper_ids(),
        );
        let (info, active) = (info?, active?);
        Ok(self
            .safekeepers
            .head(info.last_record_lsn, &active, tenant, timeline)
            .await)
    }

    async fn ancestor_lsn(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<Option<Lsn>, StorconError> {
        StorconClient::get_timeline(&self.storcon, tenant, timeline)
            .await
            .map(|t| t.ancestor_lsn)
    }

    async fn ancestor(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<Option<(NeonId, Lsn)>, StorconError> {
        let info = StorconClient::get_timeline(&self.storcon, tenant, timeline).await?;
        match (info.ancestor_timeline_id, info.ancestor_lsn) {
            (Some(id), Some(lsn)) => {
                let id = id.parse().map_err(|e| {
                    StorconError::Other(anyhow::anyhow!("timeline {timeline}'s ancestor: {e}"))
                })?;
                Ok(Some((id, lsn)))
            }
            _ => Ok(None),
        }
    }

    async fn gc_cutoff(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<Option<Lsn>, StorconError> {
        StorconClient::get_timeline(&self.storcon, tenant, timeline)
            .await
            .map(|t| t.applied_gc_cutoff_lsn)
    }

    async fn lsn_by_timestamp(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
        at: DateTime<Utc>,
    ) -> Result<LsnAtTime, StorconError> {
        StorconClient::lsn_by_timestamp(&self.storcon, tenant, timeline, at).await
    }

    async fn timestamp_of_lsn(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
        lsn: Lsn,
    ) -> Result<Option<DateTime<Utc>>, StorconError> {
        StorconClient::timestamp_of_lsn(&self.storcon, tenant, timeline, lsn).await
    }

    async fn detach_ancestor(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<(), StorconError> {
        StorconClient::detach_ancestor(&self.storcon, tenant, timeline).await
    }

    async fn patch_tenant_config(
        &self,
        tenant: &NeonId,
        pitr_interval: Duration,
    ) -> Result<(), StorconError> {
        StorconClient::patch_tenant_config(&self.storcon, tenant, pitr_interval).await
    }

    async fn tenant_pitr_interval(
        &self,
        tenant: &NeonId,
    ) -> Result<Option<Duration>, StorconError> {
        StorconClient::tenant_pitr_interval(&self.storcon, tenant).await
    }

    async fn list_timelines(&self, tenant: &NeonId) -> Result<Vec<NeonId>, StorconError> {
        StorconClient::list_timelines(&self.storcon, tenant).await
    }

    async fn wait_attached(&self, tenant: &NeonId, timeout: Duration) -> AttachWait {
        self.attach.wait(tenant, timeout).await
    }
}

/// Tells Neon's proxies that a role's password changed.
pub trait CacheNotifier: Send + Sync + 'static {
    /// How many proxies got the message.
    fn password_updated(
        &self,
        endpoint_id: &str,
        role: &str,
    ) -> impl Future<Output = anyhow::Result<u64>> + Send;
    /// `password_updated` for each of `roles` again after `delay`, from a background task
    /// (R22: a login between the first message and the commit may have cached the old
    /// password). Failures are logged.
    fn password_updated_later(&self, endpoint_id: &str, roles: Vec<String>, delay: Duration);
}

impl CacheNotifier for ProxyCache {
    async fn password_updated(&self, endpoint_id: &str, role: &str) -> anyhow::Result<u64> {
        ProxyCache::password_updated(self, endpoint_id, role).await
    }

    fn password_updated_later(&self, endpoint_id: &str, roles: Vec<String>, delay: Duration) {
        let (cache, endpoint_id) = (self.clone(), endpoint_id.to_string());
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            for role in roles {
                if let Err(e) = cache.password_updated(&endpoint_id, &role).await {
                    tracing::warn!(%endpoint_id, role, "publishing password_updated again: {e:#}");
                }
            }
        });
    }
}

/// Pushes a new spec to a running compute.
pub trait SpecPusher: Send + Sync + 'static {
    /// The keys computes accept control-plane tokens from.
    fn jwks(&self) -> serde_json::Value;
    /// The compute-JWT key pair: it also signs each compute's storage token (its spec's
    /// `storage_auth_token`).
    fn signer(&self) -> &ComputeSigner;
    fn push(
        &self,
        control_addr: SocketAddr,
        endpoint_id: &str,
        config: &serde_json::Value,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
    /// compute_ctl's own view of the compute (`GET /status`).
    fn status(
        &self,
        control_addr: SocketAddr,
        endpoint_id: &str,
    ) -> impl Future<Output = anyhow::Result<chelabase_neon::reconfigure::ComputeCtlStatus>> + Send;
    /// compute_ctl's status plus the capabilities it advertises (`GET /status`). An error
    /// (unreachable, not a status) is never "no capability".
    fn report(
        &self,
        control_addr: SocketAddr,
        endpoint_id: &str,
    ) -> impl Future<Output = anyhow::Result<chelabase_neon::reconfigure::ComputeCtlReport>> + Send;
    /// Hands a running compute a new storage token (`POST /storage_token`), without a spec push.
    fn push_storage_token(
        &self,
        control_addr: SocketAddr,
        endpoint_id: &str,
        token: &str,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// [`SpecPusher`] over compute_ctl's `/configure`.
pub struct HttpPusher {
    pub http: chelabase_http::Client,
    pub signer: ComputeSigner,
}

impl SpecPusher for HttpPusher {
    fn jwks(&self) -> serde_json::Value {
        self.signer.jwks()
    }

    fn signer(&self) -> &ComputeSigner {
        &self.signer
    }

    async fn push(
        &self,
        control_addr: SocketAddr,
        endpoint_id: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<()> {
        chelabase_neon::reconfigure::push(&self.http, control_addr, &self.signer, endpoint_id, config)
            .await
    }

    async fn status(
        &self,
        control_addr: SocketAddr,
        endpoint_id: &str,
    ) -> anyhow::Result<chelabase_neon::reconfigure::ComputeCtlStatus> {
        chelabase_neon::reconfigure::status(&self.http, control_addr, &self.signer, endpoint_id)
            .await
    }

    async fn report(
        &self,
        control_addr: SocketAddr,
        endpoint_id: &str,
    ) -> anyhow::Result<chelabase_neon::reconfigure::ComputeCtlReport> {
        chelabase_neon::reconfigure::status_report(
            &self.http,
            control_addr,
            &self.signer,
            endpoint_id,
        )
        .await
    }

    async fn push_storage_token(
        &self,
        control_addr: SocketAddr,
        endpoint_id: &str,
        token: &str,
    ) -> anyhow::Result<()> {
        chelabase_neon::reconfigure::push_storage_token(
            &self.http,
            control_addr,
            &self.signer,
            endpoint_id,
            token,
        )
        .await
    }
}
