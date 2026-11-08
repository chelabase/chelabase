use axum::Router;
use chelabase_auth::internal::{
    generate_keypair_pem, AuthMethod, Identity, InternalSigner, InternalVerifier, IDENTITY_HEADER,
};
use chelabase_metadata::{
    BeginDelete, BranchDatabase, BranchDeleteOutcome, BranchKind, BranchRecord, BranchStatus, ClaimOutcome, CreateBranchOutcome, DatabaseOrigin, DataKeyRecord, DatabaseRecord,
    DatabaseStatus, MetadataError, NewBranch, NewDataKey, PasswordState, ProjectOverview, ProjectStatus, RoleAttributes, RoleLevel, RoleOrigin, RoleRow, SettingsPatch,
    TenantOverview,
    AppSession, AppUsersStatus, BeginEnable, BeginProjectDelete, BranchAuthSync, NewProjectAuth, OnDelete, ProjectAuth,
    ProjectRecord, RenameProject, SavedQuery, SavedQueryEditor, SavedQueryPatch, SavedQuerySummary, SavedQueryWrite,
    BranchBusy, MergeCreate, MergeDraftPatch, MergeEvent, MergeRecord, MergeStatus, MergeStatusPatch, MergeWrite, NewMerge,
    NewStorageWebhook, StorageWebhook, StorageWebhookAttempt, StorageWebhookPatch,
};
use chelabase_nats::{
    AppUsersCheckReply, AppUsersCheckRequest, AppUsersJob, AppUsersSyncJob, ProjectDeleteJob, BranchJob, MergeJob, CheckDataRoleReply, DatabaseSettingsJob, DeleteDatabaseJob, JobEnvelope, PointInTime,
    ProvisionDatabaseJob, ResetPasswordReply, RoleCreateRequest, RoleDeleteRequest, RoleReply, RoleRequest, RoleSession, RoleSessionsReply, RoleUpdateRequest, RESET_NOT_APPLIED,
};
use chelabase_routes::{Service, ENDPOINTS};
use chelabase_types::mode::Mode;
use chelabase_types::{BranchId, DatabaseId, ProjectId, TenantId};
use chelabase_secrets::{open_credential, seal_credential, Purpose, SecretKey};
use chelabase_types::data_key::{self, DataKeyKind};
use orchestrator::{
    build_router, AppState, Console, JobPort, MetadataPort, PostgresAddr, ProxyAddress, ResetError,
    RestInternal,
};
use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;
use reqwest::Method;
use serde_json::{json, Value};
use chelabase_metadata::members::{GrantRow, MemberBranchRole, MemberRecord, MemberStatus, Observed, OverrideRow};
use chelabase_nats::{MemberRoleApplied, MemberRoleHandOverRequest, MemberRolesReconcileRequest, MemberRolesReply};
use chelabase_types::access::{Grant, GrantSources, Narrowing, PgRole, PlatformRole};

/// The test identity's subject: the default caller, an active tenant admin of every tenant
/// `bind_org` binds (so tests written before members keep their meaning).
const DEFAULT_SUB: &str = "user-1";
/// The identity session every test token names (review fix 1).
const DEFAULT_SID: &str = "00000000-0000-4000-8000-00000000051d";

type KeyRow = (DataKeyRecord, [u8; 32]);
/// A key store, a key id and the role name it takes.
type RenameOnCheck = (Arc<Mutex<Vec<KeyRow>>>, uuid::Uuid, String);

type RaceWrite = Box<dyn FnOnce(&FakeMetadata) + Send>;

#[derive(Clone, Default)]
struct FakeMetadata {
    tenants_by_org: Arc<Mutex<HashMap<String, TenantId>>>,
    fail_insert: Arc<Mutex<bool>>,
    /// Project ids that belong to another tenant / that do not exist, for `insert_database_pending`.
    foreign_projects: Arc<Mutex<Vec<ProjectId>>>,
    missing_projects: Arc<Mutex<Vec<ProjectId>>>,
    /// The next `get_branch` fails with a database error.
    fail_get_branch: Arc<Mutex<bool>>,
    /// `list_roles` fails with a database error while set.
    fail_list_roles: Arc<Mutex<bool>>,
    databases: Arc<Mutex<HashMap<DatabaseId, DatabaseRecord>>>,
    branches: Arc<Mutex<HashMap<BranchId, BranchRecord>>>,
    overviews: Arc<Mutex<HashMap<TenantId, TenantOverview>>>,
    /// Calls to `main_branches_with_counts`: /v1/me makes exactly one per request.
    main_count_queries: Arc<AtomicUsize>,
    /// While set, `create_secret_key` and `rotate_secret_key` fail with `EndpointLockTimeout` (a DDL transaction holds the endpoint).
    endpoint_busy: Arc<Mutex<bool>>,
    /// Data API keys, live and revoked, with their hashes.
    keys: Arc<Mutex<Vec<KeyRow>>>,
    /// Roles by endpoint id, and login roles' sealed passwords by (endpoint id, name).
    roles: Arc<Mutex<HashMap<String, Vec<RoleRow>>>>,
    role_passwords: Arc<Mutex<HashMap<(String, String), String>>>,
    /// The branch's Postgres databases.
    branch_dbs: Arc<Mutex<HashMap<BranchId, Vec<BranchDatabase>>>>,
    /// `begin_branch_delete` protects this branch first: a flag set after the API's check.
    protect_on_delete: Arc<Mutex<Option<BranchId>>>,
    /// Calls to `branch_databases` and to `branches_with_main_database`.
    branch_db_queries: Arc<AtomicUsize>,
    main_db_batch_queries: Arc<AtomicUsize>,
    /// Project rows (the fake's own `projects` table) and their app-users settings.
    projects: Arc<Mutex<Vec<ProjectRecord>>>,
    project_auths: Arc<Mutex<HashMap<ProjectId, ProjectAuth>>>,
    branch_syncs: Arc<Mutex<HashMap<BranchId, BranchAuthSync>>>,
    app_sessions: Arc<Mutex<Vec<AppSession>>>,
    /// Calls to `project_auth` and to `project_auth_statuses`.
    /// `revoke_user_sessions` fails while set.
    fail_revoke: Arc<Mutex<bool>>,
    /// Makes the offboarding write of a removal fail.
    fail_offboard: Arc<Mutex<bool>>,
    /// Makes `other_memberships` fail.
    fail_other_memberships: Arc<Mutex<bool>>,
    project_auth_reads: Arc<AtomicUsize>,
    project_auth_batch_reads: Arc<AtomicUsize>,
    /// Saved queries with their (database, user).
    saved_queries: Arc<Mutex<Vec<(uuid::Uuid, String, SavedQuery)>>>,
    /// m18: storage webhooks with their sealed secrets, and attempts by webhook.
    storage_webhooks: Arc<Mutex<Vec<(StorageWebhook, String)>>>,
    /// Edge functions m9: services' signing secrets by function.
    signing_secrets: Arc<Mutex<HashMap<uuid::Uuid, String>>>,
    storage_webhook_attempts: Arc<Mutex<Vec<(uuid::Uuid, StorageWebhookAttempt)>>>,
    /// Merge rows and their events.
    merges: Arc<Mutex<Vec<MergeRecord>>>,
    merge_events: Arc<Mutex<Vec<MergeEvent>>>,
    /// `create_merge` finds the open merge gone: the insert conflicted, the lookup found nothing.
    create_merge_conflicts: Arc<Mutex<bool>>,
    /// `branch_has_busy_merge` finds nothing (a merge takes the branch after the pre-check).
    busy_precheck_misses: Arc<Mutex<bool>>,
    /// The next status write finds the version bumped (a save landed in between).
    bump_merge_version_on_write: Arc<Mutex<bool>>,
    /// The next status write finds this result (another request landed in between).
    result_changes_before_write: Arc<Mutex<Option<Value>>>,
    /// Members (any tenant), project grants by (member, project) and overrides by
    /// (member, database, Postgres database).
    members: Arc<Mutex<Vec<MemberRecord>>>,
    grants: Arc<Mutex<HashMap<(uuid::Uuid, uuid::Uuid), GrantRow>>>,
    overrides: Arc<Mutex<HashMap<OverrideKey, OverrideRow>>>,
    /// Team grants by (member, project) (members hub step 2's source; step 1's metadata never
    /// fills it, the fake does to test the per-role levels, T2).
    #[allow(clippy::type_complexity)]
    team_grants: Arc<Mutex<HashMap<(uuid::Uuid, uuid::Uuid), Vec<Grant>>>>,
    /// Reads of a member row (`member_by_identity_user` and `admit_member` each read it once).
    member_reads: Arc<AtomicUsize>,
    /// Reads of a branch's roles (`list_roles`).
    role_reads: Arc<AtomicUsize>,
    /// When each tenant's admin was claimed by a first sign-in (`admin_claimed`).
    admin_claims: Arc<Mutex<HashMap<TenantId, chrono::DateTime<chrono::Utc>>>>,
    /// Selfhost-install m3: each tenant's fresh-install claim (its bound user). With one, only
    /// that user claims the admin (`admit_member`'s rule); without one the fake keeps the first
    /// comer's rule the other tests build on.
    setup_claims: Arc<Mutex<HashMap<TenantId, String>>>,
    /// A write "another request" makes just before the next checked grant or override write.
    race_before_write: Arc<Mutex<std::collections::VecDeque<RaceWrite>>>,
    /// The reconciler's `member_branch_roles` rows.
    member_roles: Arc<Mutex<Vec<MemberBranchRole>>>,
    /// R31: their last acted role, by (member, branch).
    acted_roles: Arc<Mutex<HashMap<(uuid::Uuid, uuid::Uuid), String>>>,
    /// When each member row was made (stamped by every fake write that makes one).
    member_created: Arc<Mutex<HashMap<uuid::Uuid, chrono::DateTime<chrono::Utc>>>>,
    /// T7: invites, the access log and each tenant's open sign-up switch.
    invites: Arc<Mutex<Vec<FakeInvite>>>,
    events: Arc<Mutex<Vec<chelabase_metadata::members::AccessEvent>>>,
    open_signups: Arc<Mutex<HashMap<TenantId, bool>>>,
    /// A member the next `approve_member` finds removed (it went away after the check).
    vanish_on_approve: Arc<Mutex<Option<uuid::Uuid>>>,
    /// Each tenant's display name (R36); absent means none.
    tenant_names: Arc<Mutex<HashMap<TenantId, String>>>,
    /// The reconciler's member roles, as (member, branch).
    member_branch_roles: Arc<Mutex<Vec<(uuid::Uuid, BranchId)>>>,
    /// PB: each branch's guard exemptions, sorted (missing: none).
    guard_exemptions: Arc<Mutex<HashMap<BranchId, Vec<String>>>>,
    /// Members hub T5: personal-login role names, as (database id, role name), any status
    /// (`MembersPort::is_personal_login_name` answers from it).
    personal_login_names: Arc<Mutex<HashSet<(uuid::Uuid, String)>>>,
    /// Members hub T8: personal-login records and their proxy rows (what `personal_logins` and
    /// the `neon_roles` rows of origin `personal` hold). The T8 `MembersPort` methods answer
    /// from it as the real ones would (T3's docs): a revoked record keeps its rows (with no
    /// verifier) until the worker drops the role, and `member_login_endpoints` lists every row
    /// of the member whatever its record's status.
    personal_logins: Arc<Mutex<Vec<FakeLogin>>>,
    /// Final round A (T9-I1): `member_project_roles` fails with a database error while set.
    fail_member_project_roles: Arc<Mutex<bool>>,
    /// Members hub step 2 (T23): the teams (any tenant) with their grants and members, as T1's
    /// `teams`, `team_grants` and `team_members` hold them. Every grant-source read unions them
    /// as the shared shape does (P4, P27).
    teams: Arc<Mutex<Vec<FakeTeam>>>,
    /// T23 (P25): a write "another request" makes just before the next re-read of an invite's
    /// or approval's teams (`teams_seen_hold`), apart from `race_before_write` so a checked
    /// grant write's `settle` never takes it.
    race_before_teams_seen: Arc<Mutex<std::collections::VecDeque<RaceWrite>>>,
    /// Edge functions m3: functions, deployments, branch state and secrets.
    functions: Arc<Mutex<FakeFunctions>>,
}

impl orchestrator::functions_triggers::TriggersPort for FakeMetadata {
    async fn add_trigger(&self, t: &chelabase_metadata::NewTrigger<'_>) -> Result<uuid::Uuid, MetadataError> {
        self.fn_scope(t.branch_id, t.function_id)?;
        let id = uuid::Uuid::new_v4();
        let table = t.source == chelabase_metadata::TriggerSource::Table;
        self.functions.lock().unwrap().triggers.push(chelabase_metadata::TriggerRecord {
            id,
            branch_id: t.branch_id,
            function_id: t.function_id,
            source: t.source,
            enabled: true,
            state: if table { chelabase_metadata::TriggerState::PendingInstall } else { chelabase_metadata::TriggerState::Installed },
            enabled_since: chrono::Utc::now(),
            pg_database: t.pg_database.to_string(),
            schema_name: t.schema_name.map(str::to_string),
            table_name: t.table_name.map(str::to_string),
            bucket: t.bucket.map(str::to_string),
            prefix: t.prefix.map(str::to_string),
            events: t.events.iter().map(|e| e.to_string()).collect(),
            last_error: None,
            dead_count: 0,
            last_delivery_error: None,
            last_delivered_at: None,
        });
        Ok(id)
    }

    async fn list_triggers(&self, branch: BranchId) -> Result<Vec<chelabase_metadata::TriggerRecord>, MetadataError> {
        Ok(self.functions.lock().unwrap().triggers.iter().filter(|t| t.branch_id == branch).cloned().collect())
    }

    async fn update_trigger(&self, branch: BranchId, function: uuid::Uuid, id: uuid::Uuid, enabled: bool) -> Result<Option<chelabase_metadata::TriggerRecord>, MetadataError> {
        self.fn_scope(branch, function)?;
        let mut fs = self.functions.lock().unwrap();
        let Some(t) = fs.triggers.iter_mut().find(|t| t.id == id && t.branch_id == branch && t.function_id == function) else {
            return Ok(None);
        };
        t.enabled = enabled;
        Ok(Some(t.clone()))
    }

    async fn delete_trigger(&self, branch: BranchId, function: uuid::Uuid, id: uuid::Uuid) -> Result<bool, MetadataError> {
        self.fn_scope(branch, function)?;
        let mut fs = self.functions.lock().unwrap();
        let before = fs.triggers.len();
        fs.triggers.retain(|t| !(t.id == id && t.branch_id == branch && t.function_id == function));
        Ok(fs.triggers.len() < before)
    }
}

/// Edge functions m3: what the metadata's functions tables hold.
#[derive(Default)]
struct FakeFunctions {
    functions: Vec<chelabase_metadata::FunctionRecord>,
    deployments: Vec<chelabase_metadata::DeploymentRecord>,
    /// Each branch's state of a function.
    states: Vec<(BranchId, chelabase_metadata::BranchFunction)>,
    secrets: std::collections::BTreeMap<(uuid::Uuid, uuid::Uuid, String), String>,
    /// Edge functions m7: every branch's schedules, oldest first.
    schedules: Vec<chelabase_metadata::ScheduleRecord>,
    /// Edge functions m13: every branch's triggers, oldest first.
    triggers: Vec<chelabase_metadata::TriggerRecord>,
}

/// Members hub step 2 (T23): one team as the fake keeps it.
#[derive(Clone, Debug)]
struct FakeTeam {
    record: chelabase_metadata::teams::TeamRecord,
    /// By project (any project id: the reads keep the non-deleted ones of the team's tenant).
    grants: std::collections::BTreeMap<uuid::Uuid, GrantRow>,
    /// Member ids (any member: the reads keep the ones of the team's tenant).
    members: std::collections::BTreeSet<uuid::Uuid>,
}

/// Members hub T8: one personal-login record as the fake keeps it.
#[derive(Clone, Debug)]
#[allow(dead_code)]
struct FakeLogin {
    /// The record as the API lists it.
    login: chelabase_metadata::personal_logins::PersonalLogin,
    /// The record's SCRAM verifier; `None` once revoked (MH-9).
    verifier: Option<String>,
    /// Its personal `neon_roles` rows, one per branch endpoint the worker made the role on.
    endpoints: Vec<chelabase_metadata::personal_logins::LoginEndpoint>,
}

/// An `invites` row.
#[derive(Clone, Debug)]
struct FakeInvite {
    id: uuid::Uuid,
    member_id: uuid::Uuid,
    identity_user_id: String,
    expires_at: chrono::DateTime<chrono::Utc>,
    /// SHA-256 of the link's token.
    token_hash: [u8; 32],
    revoked: bool,
    accepted: bool,
}

/// An override's (member, Chelabase database).
type OverrideKey = (uuid::Uuid, uuid::Uuid);

/// A database row and its main branch, which holds the endpoint and the sealed password.
/// Derefs to the database row, so tests set its fields directly.
#[derive(Clone)]
struct Db {
    record: DatabaseRecord,
    main: BranchRecord,
}

impl Deref for Db {
    type Target = DatabaseRecord;
    fn deref(&self) -> &DatabaseRecord {
        &self.record
    }
}

impl DerefMut for Db {
    fn deref_mut(&mut self) -> &mut DatabaseRecord {
        &mut self.record
    }
}

/// A database with its default main: endpoint `ep-0123456789ab`, and a sealed password
/// no key opens.
impl From<DatabaseRecord> for Db {
    fn from(record: DatabaseRecord) -> Self {
        let main = main_for(&record);
        Self { record, main }
    }
}

fn main_for(record: &DatabaseRecord) -> BranchRecord {
    let now = chrono::Utc::now();
    BranchRecord {
        id: BranchId::from(record.database_id),
        database_id: record.database_id,
        name: "main".into(),
        parent_branch_id: None,
        kind: BranchKind::Normal,
        status: BranchStatus::from(record.status),
        timeline_id: Some("0123456789abcdef0123456789abcdef".into()),
        pending_timeline_id: None,
        pending_timeline_at: None,
        superseded_timeline_id: None,
        endpoint_id: Some("ep-0123456789ab".into()),
        owner_password_enc: Some("v1:secret-owner-sealed".into()),
        branched_at_time: None,
        branched_at_lsn: None,
        error: None,
        created_at: now,
        updated_at: now,
        history_start: None,
        authenticator_password_enc: None,
        protected: false,
        guard_logins: true,
        review_ack_hash: None,
        review_ack_keys: Vec::new(),
    }
}

impl FakeMetadata {
    /// Test-only: names `tenant_id` by a short label the request helpers take (`token_for`,
    /// `send_as`); the default caller becomes an active tenant admin there.
    fn bind_org(&self, org_id: &str, tenant_id: TenantId) {
        self.tenants_by_org
            .lock()
            .unwrap()
            .insert(org_id.to_string(), tenant_id);
        if self.member(tenant_id, DEFAULT_SUB).is_none() {
            self.add_member(tenant_id, DEFAULT_SUB, MemberStatus::Active, true);
        }
    }

    /// The member `sub` of `tenant`, if any.
    fn member(&self, tenant: TenantId, sub: &str) -> Option<MemberRecord> {
        self.members
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.tenant_id == tenant && m.identity_user_id == sub)
            .cloned()
    }

    /// Adds a member (no grants) and returns its id.
    fn add_member(&self, tenant: TenantId, sub: &str, status: MemberStatus, tenant_admin: bool) -> uuid::Uuid {
        let id = uuid::Uuid::new_v4();
        self.stamp_created(id);
        self.members.lock().unwrap().push(MemberRecord {
            id,
            tenant_id: tenant,
            identity_user_id: sub.into(),
            email: Some(format!("{sub}@example.test")),
            display_name: sub.into(),
            tenant_admin,
            status,
            accepted_at: (status == MemberStatus::Active).then(chrono::Utc::now),
            offboarded: false, suspended_at: None, suspended_by: None,
        });
        id
    }

    /// An active member of `tenant` with `role` on `project` and these named roles.
    fn add_granted(&self, tenant: TenantId, sub: &str, project: ProjectId, role: PlatformRole, pg_roles: &[&str]) -> uuid::Uuid {
        let id = self.add_member(tenant, sub, MemberStatus::Active, false);
        self.grant(id, project, role, pg_roles);
        id
    }

    fn grant(&self, member: uuid::Uuid, project: ProjectId, role: PlatformRole, pg_roles: &[&str]) {
        self.grants.lock().unwrap().insert(
            (member, project.as_uuid()),
            GrantRow { platform_role: role, pg_roles: pg_roles.iter().map(|r| r.to_string()).collect() },
        );
    }

    /// A team grant reaching `member` on `project` (T2: the union's other source).
    #[allow(dead_code)]
    fn team_grant(&self, member: uuid::Uuid, project: ProjectId, grant: Grant) {
        self.team_grants.lock().unwrap().entry((member, project.as_uuid())).or_default().push(grant);
    }

    /// A narrowing override for `member` on a Chelabase database.
    fn narrow(&self, member: uuid::Uuid, database: DatabaseId, row: OverrideRow) {
        self.overrides.lock().unwrap().insert((member, database.as_uuid()), row);
    }

    fn set_member(&self, tenant: TenantId, sub: &str, change: impl FnOnce(&mut MemberRecord)) {
        let mut members = self.members.lock().unwrap();
        let m = members
            .iter_mut()
            .find(|m| m.tenant_id == tenant && m.identity_user_id == sub)
            .expect("member");
        change(m);
    }

    /// An open invite for `member` (an `invited` one the test made with `add_member`).
    fn add_open_invite(&self, member: uuid::Uuid) {
        let identity_user_id = self
            .members
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.id == member)
            .expect("member")
            .identity_user_id
            .clone();
        self.invites.lock().unwrap().push(FakeInvite {
            id: uuid::Uuid::new_v4(),
            member_id: member,
            identity_user_id,
            expires_at: chrono::Utc::now() + chrono::Duration::hours(72),
            token_hash: [3; 32],
            revoked: false,
            accepted: false,
        });
    }

    fn set_admin_claim(&self, tenant: TenantId, at: Option<chrono::DateTime<chrono::Utc>>) {
        let mut claims = self.admin_claims.lock().unwrap();
        match at {
            Some(at) => claims.insert(tenant, at),
            None => claims.remove(&tenant),
        };
    }

    /// Every invite of `member` has expired.
    fn expire_invites(&self, member: uuid::Uuid) {
        for i in self.invites.lock().unwrap().iter_mut().filter(|i| i.member_id == member) {
            i.expires_at = chrono::Utc::now() - chrono::Duration::minutes(1);
        }
    }

    fn insert_database(&self, db: impl Into<Db>) {
        let Db { record, main } = db.into();
        self.branches.lock().unwrap().insert(main.id, main);
        self.databases
            .lock()
            .unwrap()
            .insert(record.database_id, record);
    }

    /// Adds a branch of `db` under `parent`; a ready normal branch gets an endpoint.
    fn add_branch(
        &self,
        db: DatabaseId,
        name: &str,
        parent: BranchId,
        kind: BranchKind,
        status: BranchStatus,
    ) -> BranchId {
        let now = chrono::Utc::now();
        let id = BranchId::new();
        let has_endpoint = kind == BranchKind::Normal && status == BranchStatus::Ready;
        self.branches.lock().unwrap().insert(
            id,
            BranchRecord {
                id,
                database_id: db,
                name: name.into(),
                parent_branch_id: Some(parent),
                kind,
                status,
                timeline_id: Some("fedcba9876543210fedcba9876543210".into()),
                pending_timeline_id: None,
                pending_timeline_at: None,
                superseded_timeline_id: None,
                endpoint_id: has_endpoint.then(|| format!("ep-{:012x}", id.as_uuid().as_u128() & 0xffff_ffff_ffff)),
                owner_password_enc: None,
                branched_at_time: None,
                branched_at_lsn: None,
                error: None,
                created_at: now,
                updated_at: now,
                history_start: None,
                authenticator_password_enc: None,
                protected: false,
                guard_logins: true,
                review_ack_hash: None,
                review_ack_keys: Vec::new(),
            },
        );
        id
    }

    fn branch(&self, id: BranchId) -> BranchRecord {
        self.branches.lock().unwrap().get(&id).cloned().expect("branch row")
    }

    /// `change` on a `normal` branch that isn't deleting (the real updates' row guards).
    fn update_normal_branch(&self, id: BranchId, change: impl FnOnce(&mut BranchRecord)) -> Option<BranchRecord> {
        match self.branches.lock().unwrap().get_mut(&id) {
            Some(b) if b.kind == BranchKind::Normal && !matches!(b.status, BranchStatus::Deleting | BranchStatus::Deleted) => {
                change(b);
                Some(b.clone())
            }
            _ => None,
        }
    }

    fn database(&self, id: DatabaseId) -> DatabaseRecord {
        self.databases.lock().unwrap().get(&id).cloned().expect("database row")
    }

    /// A NOLOGIN group role on an endpoint that is a member of `member_of`.
    fn add_group_role(&self, endpoint: &str, name: &str, member_of: &[&str]) {
        self.add_role(endpoint, name, RoleOrigin::Sql, false, false);
        let mut roles = self.roles.lock().unwrap();
        let row = roles.get_mut(endpoint).unwrap().last_mut().unwrap();
        row.login = false;
        row.attributes.member_of = member_of.iter().map(|r| r.to_string()).collect();
    }

    /// [`Self::add_group_role`] with the memberships' options (I2): `no_set` lacks SET, `admin`
    /// has ADMIN; both name roles of `member_of`.
    fn add_group_role_with(&self, endpoint: &str, name: &str, member_of: &[&str], no_set: &[&str], admin: &[&str]) {
        self.add_group_role(endpoint, name, member_of);
        let mut roles = self.roles.lock().unwrap();
        let row = roles.get_mut(endpoint).unwrap().last_mut().unwrap();
        row.attributes.member_no_set = no_set.iter().map(|r| r.to_string()).collect();
        row.attributes.member_admin = admin.iter().map(|r| r.to_string()).collect();
    }

    /// Adds a role row to an endpoint, as the worker would have written it.
    fn add_role(&self, endpoint: &str, name: &str, origin: RoleOrigin, privileged: bool, pending: bool) {
        self.roles.lock().unwrap().entry(endpoint.to_string()).or_default().push(RoleRow {
            name: name.into(),
            origin,
            level: if privileged { RoleLevel::OwnerLevel } else { RoleLevel::Plain },
            login: true,
            password: PasswordState::Generated,
            created_at: chrono::Utc::now(),
            pending,
            attributes: RoleAttributes::default(),
        });
    }

    /// Adds a role row built by the caller.
    fn add_role_row(&self, endpoint: &str, row: RoleRow) {
        self.roles.lock().unwrap().entry(endpoint.to_string()).or_default().push(row);
    }

    /// Records a Postgres database of a branch.
    fn add_branch_db(&self, branch: BranchId, name: &str, owner: &str, origin: DatabaseOrigin) {
        self.branch_dbs.lock().unwrap().entry(branch).or_default().push(BranchDatabase {
            name: name.into(),
            owner: owner.into(),
            origin,
            data_api: chelabase_metadata::DataApiState::Off,
            data_api_error: None,
            default_schema: "public".into(),
            data_api_bootstrapped: origin == DatabaseOrigin::Main,
        });
    }

    fn fail_next_insert(&self) {
        *self.fail_insert.lock().unwrap() = true;
    }

    /// Members hub T8: a live personal login of `member` on `database` named `name`, as the
    /// worker would have converged it: valid for 90 days, a verifier, and a personal role row
    /// (origin `personal`, no sealed password) on every branch of the database that has an
    /// endpoint. Its name joins `personal_login_names`.
    #[allow(dead_code)]
    fn seed_personal_login(&self, member: uuid::Uuid, database: DatabaseId, name: &str) -> FakeLogin {
        use chelabase_metadata::personal_logins::{LoginEndpoint, LoginStatus, PersonalLogin};
        let record = self.database(database);
        let mut endpoints: Vec<LoginEndpoint> = self
            .branches
            .lock()
            .unwrap()
            .values()
            .filter(|b| b.database_id == database && b.status != BranchStatus::Deleted)
            .filter_map(|b| {
                b.endpoint_id.clone().map(|endpoint_id| LoginEndpoint {
                    branch_id: b.id.as_uuid(),
                    endpoint_id,
                    role_name: name.to_string(),
                })
            })
            .collect();
        endpoints.sort();
        for e in &endpoints {
            self.add_role_row(
                &e.endpoint_id,
                RoleRow {
                    name: name.into(),
                    origin: RoleOrigin::Personal,
                    level: RoleLevel::Plain,
                    login: true,
                    password: PasswordState::SetInSql,
                    created_at: chrono::Utc::now(),
                    pending: false,
                    attributes: RoleAttributes::default(),
                },
            );
        }
        let now = chrono::Utc::now();
        let login = FakeLogin {
            login: PersonalLogin {
                member_id: member,
                database_id: database.as_uuid(),
                project_id: record.project_id.as_uuid(),
                database_name: record.name.clone(),
                role_name: name.into(),
                valid_until: now + chrono::Duration::days(90),
                created_at: now,
                revoked_at: None,
                revoked_reason: None,
                status: LoginStatus::Active,
            },
            verifier: Some(format!("SCRAM-SHA-256$4096:c2VlZA==$seeded-{name}:c2VydmVy")),
            endpoints,
        };
        self.personal_login_names.lock().unwrap().insert((database.as_uuid(), name.to_string()));
        self.personal_logins.lock().unwrap().push(login.clone());
        login
    }

    /// Members hub T8: `member`'s record on `database`, if any.
    #[allow(dead_code)]
    fn personal_login(&self, member: uuid::Uuid, database: DatabaseId) -> Option<FakeLogin> {
        self.personal_logins
            .lock()
            .unwrap()
            .iter()
            .find(|l| l.login.member_id == member && l.login.database_id == database.as_uuid())
            .cloned()
    }

    /// Members hub T8: changes `member`'s record on `database` (to revoke or expire it).
    #[allow(dead_code)]
    fn edit_personal_login(&self, member: uuid::Uuid, database: DatabaseId, change: impl FnOnce(&mut FakeLogin)) {
        let mut logins = self.personal_logins.lock().unwrap();
        let l = logins
            .iter_mut()
            .find(|l| l.login.member_id == member && l.login.database_id == database.as_uuid())
            .expect("personal login");
        change(l);
    }

    /// Databases without a main branch yet get the default one.
    fn set_overview(&self, overview: TenantOverview) {
        let mut branches = self.branches.lock().unwrap();
        for db in overview.projects.iter().flat_map(|p| &p.databases) {
            branches
                .entry(BranchId::from(db.database_id))
                .or_insert_with(|| main_for(db));
        }
        self.overviews.lock().unwrap().insert(overview.id, overview);
    }
}

/// A minimal `Ready` database record for the given tenant, for GET/soft-bind tests
/// that don't care about provisioning details.
fn sample_database(tenant_id: TenantId) -> Db {
    Db::from(DatabaseRecord {
        database_id: DatabaseId::new(),
        project_id: ProjectId::new(),
        tenant_id,
        name: "demo-db".into(),
        status: DatabaseStatus::Ready,
        database_name: None,
        owner_role: None,
        error: None,
        neon_tenant_id: None,
        history_window: std::time::Duration::from_secs(7 * 24 * 3600),
        max_branches: 5,
        max_rows: 1000,
        statement_timeout_ms: 8000,
        max_connections: 5,
        storage_max_connections: 2,
    })
}

impl MetadataPort for FakeMetadata {

    /// #119: a tenant exists once it has a label, has members or has an overview.
    async fn tenant_exists(&self, tenant: TenantId) -> Result<bool, MetadataError> {
        Ok(self.tenants_by_org.lock().unwrap().values().any(|t| *t == tenant)
            || self.members.lock().unwrap().iter().any(|m| m.tenant_id == tenant)
            || self.overviews.lock().unwrap().contains_key(&tenant))
    }

    async fn insert_database_pending(
        &self,
        _database_id: DatabaseId,
        tenant_id: TenantId,
        project_id: ProjectId,
        _name: &str,
    ) -> Result<(), MetadataError> {
        if self.foreign_projects.lock().unwrap().contains(&project_id) {
            return Err(MetadataError::ProjectTenantMismatch { project_id, tenant_id });
        }
        if self.missing_projects.lock().unwrap().contains(&project_id) {
            return Err(MetadataError::ProjectNotFound(project_id));
        }
        if self
            .projects
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.id == project_id && p.status != ProjectStatus::Active)
        {
            return Err(MetadataError::ProjectDeleting(project_id));
        }
        if *self.fail_insert.lock().unwrap() {
            return Err(MetadataError::DuplicateDatabaseName);
        }
        Ok(())
    }

    async fn set_failed(&self, _database_id: DatabaseId, _error: &str) -> Result<(), MetadataError> {
        Ok(())
    }

    async fn get_database(
        &self,
        database_id: DatabaseId,
    ) -> Result<Option<DatabaseRecord>, MetadataError> {
        Ok(self.databases.lock().unwrap().get(&database_id).cloned())
    }

    async fn tenant_overview(
        &self,
        tenant_id: TenantId,
    ) -> Result<Option<TenantOverview>, MetadataError> {
        let mut overview = self.overviews.lock().unwrap().get(&tenant_id).cloned();
        if let Some(o) = overview.as_mut() {
            o.display_name = self.tenant_names.lock().unwrap().get(&tenant_id).cloned();
        }
        Ok(overview)
    }

    async fn get_branch(&self, branch_id: BranchId) -> Result<Option<BranchRecord>, MetadataError> {
        if std::mem::take(&mut *self.fail_get_branch.lock().unwrap()) {
            return Err(MetadataError::Config("injected".into()));
        }
        Ok(self.branches.lock().unwrap().get(&branch_id).cloned())
    }

    async fn begin_delete(&self, database_id: DatabaseId) -> Result<BeginDelete, MetadataError> {
        let mut dbs = self.databases.lock().unwrap();
        match dbs.get_mut(&database_id) {
            Some(db) if !matches!(db.status, DatabaseStatus::Deleting | DatabaseStatus::Deleted) => {
                let protected = self.branches.lock().unwrap().values().any(|b| {
                    b.database_id == database_id
                        && b.protected
                        && !matches!(b.status, BranchStatus::Deleting | BranchStatus::Deleted)
                });
                if protected {
                    return Ok(BeginDelete::Protected);
                }
                db.status = DatabaseStatus::Deleting;
                Ok(BeginDelete::Started)
            }
            _ => Ok(BeginDelete::NotReady),
        }
    }

    async fn list_branches(&self, database_id: DatabaseId) -> Result<Vec<BranchRecord>, MetadataError> {
        let mut list: Vec<_> = self
            .branches
            .lock()
            .unwrap()
            .values()
            .filter(|b| b.database_id == database_id && b.status != BranchStatus::Deleted)
            .cloned()
            .collect();
        // Main first, as `created_at` would put it; then by name for a stable order.
        list.sort_by_key(|b| (b.parent_branch_id.is_some(), b.name.clone()));
        Ok(list)
    }

    async fn main_branches_with_counts(
        &self,
        tenant_id: TenantId,
    ) -> Result<Vec<(BranchRecord, i64)>, MetadataError> {
        self.main_count_queries.fetch_add(1, Ordering::SeqCst);
        let overview = self.overviews.lock().unwrap().get(&tenant_id).cloned();
        let branches = self.branches.lock().unwrap();
        Ok(overview
            .iter()
            .flat_map(|o| o.projects.iter().flat_map(|p| &p.databases))
            .filter_map(|db| {
                let main = branches.get(&BranchId::from(db.database_id))?.clone();
                let count = branches
                    .values()
                    .filter(|b| {
                        b.database_id == db.database_id
                            && b.kind == BranchKind::Normal
                            && b.status != BranchStatus::Deleted
                    })
                    .count();
                Some((main, count as i64))
            })
            .collect())
    }

    async fn create_branch(&self, b: &NewBranch<'_>) -> Result<CreateBranchOutcome, MetadataError> {
        let max = match self.databases.lock().unwrap().get(&b.database_id) {
            Some(db) if !matches!(db.status, DatabaseStatus::Deleting | DatabaseStatus::Deleted) => {
                db.max_branches
            }
            _ => return Ok(CreateBranchOutcome::DatabaseNotLive),
        };
        let mut branches = self.branches.lock().unwrap();
        let parent_ready = branches
            .get(&b.parent_branch_id)
            .is_some_and(|p| p.database_id == b.database_id && p.status == BranchStatus::Ready);
        if !parent_ready {
            return Ok(CreateBranchOutcome::ParentNotReady);
        }
        let live = |x: &&BranchRecord| x.database_id == b.database_id && x.status != BranchStatus::Deleted;
        if branches.values().filter(live).any(|x| x.name == b.name) {
            return Ok(CreateBranchOutcome::NameTaken);
        }
        let count = branches
            .values()
            .filter(live)
            .filter(|x| x.kind == BranchKind::Normal && x.parent_branch_id.is_some())
            .count();
        if count as i64 >= i64::from(max) {
            return Ok(CreateBranchOutcome::CapReached { max });
        }
        let now = chrono::Utc::now();
        let parent_guard_logins = branches[&b.parent_branch_id].guard_logins;
        // The parent's exemptions too, as the real create does in its transaction.
        let mut exemptions = self.guard_exemptions.lock().unwrap();
        if let Some(parent) = exemptions.get(&b.parent_branch_id).cloned() {
            exemptions.insert(b.id, parent);
        }
        drop(exemptions);
        branches.insert(
            b.id,
            BranchRecord {
                id: b.id,
                database_id: b.database_id,
                name: b.name.into(),
                parent_branch_id: Some(b.parent_branch_id),
                kind: BranchKind::Normal,
                status: BranchStatus::Pending,
                timeline_id: None,
                pending_timeline_id: None,
                pending_timeline_at: None,
                superseded_timeline_id: None,
                endpoint_id: None,
                owner_password_enc: None,
                branched_at_time: b.branched_at_time,
                branched_at_lsn: b.branched_at_lsn.map(str::to_string),
                error: None,
                created_at: now,
                updated_at: now,
                history_start: None,
                authenticator_password_enc: None,
                protected: false,
                guard_logins: parent_guard_logins,
                review_ack_hash: None,
                review_ack_keys: Vec::new(),
            },
        );
        Ok(CreateBranchOutcome::Created)
    }

    async fn live_children(&self, branch_id: BranchId) -> Result<Vec<BranchRecord>, MetadataError> {
        let mut children: Vec<_> = self
            .branches
            .lock()
            .unwrap()
            .values()
            .filter(|b| {
                b.parent_branch_id == Some(branch_id)
                    && b.kind == BranchKind::Normal
                    && b.status != BranchStatus::Deleted
            })
            .cloned()
            .collect();
        children.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(children)
    }

    async fn set_branch_status(
        &self,
        branch_id: BranchId,
        from: &[BranchStatus],
        to: BranchStatus,
    ) -> Result<bool, MetadataError> {
        let mut branches = self.branches.lock().unwrap();
        match branches.get_mut(&branch_id) {
            Some(b)
                if from.contains(&b.status)
                    && !matches!(b.status, BranchStatus::Deleting | BranchStatus::Deleted) =>
            {
                b.status = to;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn claim_branch_for_switch(
        &self,
        branch_id: BranchId,
        allow_protected: bool,
    ) -> Result<ClaimOutcome, MetadataError> {
        // T14 M12: the merge check under the claim, as the real one does under the row lock.
        let ready = self.branches.lock().unwrap().get(&branch_id).is_some_and(|b| b.status == BranchStatus::Ready);
        if ready && let Some(busy) = self.busy(branch_id) {
            return Ok(ClaimOutcome::MergeBusy(busy));
        }
        match self.branches.lock().unwrap().get_mut(&branch_id) {
            Some(b) if b.status == BranchStatus::Ready && b.protected && !allow_protected => {
                Ok(ClaimOutcome::Protected)
            }
            Some(b) if b.status == BranchStatus::Ready => {
                b.status = BranchStatus::Resetting;
                b.error = None;
                Ok(ClaimOutcome::Claimed)
            }
            _ => Ok(ClaimOutcome::NotReady),
        }
    }

    async fn set_branch_protected(
        &self,
        branch_id: BranchId,
        protected: bool,
    ) -> Result<Option<BranchRecord>, MetadataError> {
        match self.branches.lock().unwrap().get_mut(&branch_id) {
            Some(b)
                if b.kind == BranchKind::Normal
                    && !matches!(b.status, BranchStatus::Deleting | BranchStatus::Deleted) =>
            {
                b.protected = protected;
                Ok(Some(b.clone()))
            }
            _ => Ok(None),
        }
    }

    async fn set_branch_guard_logins(
        &self,
        branch_id: BranchId,
        on: bool,
    ) -> Result<Option<BranchRecord>, MetadataError> {
        Ok(self.update_normal_branch(branch_id, |b| b.guard_logins = on))
    }

    async fn list_guard_exemptions(&self, branch_id: BranchId) -> Result<Vec<String>, MetadataError> {
        Ok(self.guard_exemptions.lock().unwrap().get(&branch_id).cloned().unwrap_or_default())
    }

    async fn add_guard_exemption(&self, branch_id: BranchId, role: &str) -> Result<bool, MetadataError> {
        if !self.branches.lock().unwrap().contains_key(&branch_id) {
            return Err(MetadataError::Config("branch_guard_exemptions: no such branch (FK)".into()));
        }
        let mut all = self.guard_exemptions.lock().unwrap();
        let list = all.entry(branch_id).or_default();
        if list.iter().any(|r| r == role) {
            return Ok(false);
        }
        list.push(role.to_string());
        list.sort();
        Ok(true)
    }

    async fn remove_guard_exemption(&self, branch_id: BranchId, role: &str) -> Result<bool, MetadataError> {
        let mut all = self.guard_exemptions.lock().unwrap();
        let Some(list) = all.get_mut(&branch_id) else {
            return Ok(false);
        };
        let before = list.len();
        list.retain(|r| r != role);
        Ok(list.len() != before)
    }

    async fn protect_branch(
        &self,
        branch_id: BranchId,
        hash: &str,
        keys: &[String],
    ) -> Result<Option<BranchRecord>, MetadataError> {
        Ok(self.update_normal_branch(branch_id, |b| {
            b.protected = true;
            b.review_ack_hash = Some(hash.to_string());
            b.review_ack_keys = keys.to_vec();
        }))
    }

    async fn acknowledge_review(
        &self,
        branch_id: BranchId,
        hash: &str,
        keys: &[String],
    ) -> Result<Option<BranchRecord>, MetadataError> {
        Ok(self.update_normal_branch(branch_id, |b| {
            b.review_ack_hash = Some(hash.to_string());
            b.review_ack_keys = keys.to_vec();
        }))
    }

    async fn set_branch_failed(&self, branch_id: BranchId, error: &str) -> Result<(), MetadataError> {
        if let Some(b) = self.branches.lock().unwrap().get_mut(&branch_id) {
            b.status = BranchStatus::Failed;
            b.error = Some(error.into());
        }
        Ok(())
    }

    async fn begin_branch_delete(&self, branch_id: BranchId) -> Result<BranchDeleteOutcome, MetadataError> {
        // T14 F3 (I1): a merge holding the live branch refuses it, as the real one checks under
        // the branch's row lock.
        let live_now = self
            .branches
            .lock()
            .unwrap()
            .get(&branch_id)
            .is_some_and(|b| !matches!(b.status, BranchStatus::Deleting | BranchStatus::Deleted));
        if live_now && let Some(busy) = self.busy(branch_id) {
            return Ok(BranchDeleteOutcome::MergeBusy(busy));
        }
        let mut branches = self.branches.lock().unwrap();
        if *self.protect_on_delete.lock().unwrap() == Some(branch_id) {
            branches.get_mut(&branch_id).unwrap().protected = true;
        }
        // Like the real guard: refused while the branch or a live backup of it has live
        // normal children (the branch's own first, then each backup's, oldest first).
        let live = |b: &BranchRecord| !matches!(b.status, BranchStatus::Deleting | BranchStatus::Deleted);
        if branches.get(&branch_id).is_some_and(live) {
            let children_of = |parent: BranchId| {
                let mut names: Vec<String> = branches
                    .values()
                    .filter(|c| {
                        c.parent_branch_id == Some(parent)
                            && c.kind == BranchKind::Normal
                            && c.status != BranchStatus::Deleted
                    })
                    .map(|c| c.name.clone())
                    .collect();
                names.sort();
                names
            };
            let own = children_of(branch_id);
            if !own.is_empty() {
                return Ok(BranchDeleteOutcome::HasChildren { backup: None, children: own });
            }
            let mut backups: Vec<&BranchRecord> = branches
                .values()
                .filter(|b| {
                    b.parent_branch_id == Some(branch_id)
                        && b.kind == BranchKind::Backup
                        && b.status != BranchStatus::Deleted
                })
                .collect();
            backups.sort_by_key(|b| (b.created_at, b.id.to_string()));
            for backup in backups {
                let children = children_of(backup.id);
                if !children.is_empty() {
                    return Ok(BranchDeleteOutcome::HasChildren { backup: Some(backup.name.clone()), children });
                }
            }
        }
        let moved = match branches.get_mut(&branch_id) {
            Some(b) if !matches!(b.status, BranchStatus::Deleting | BranchStatus::Deleted) && b.protected => {
                return Ok(BranchDeleteOutcome::Protected);
            }
            Some(b) if !matches!(b.status, BranchStatus::Deleting | BranchStatus::Deleted) => {
                b.status = BranchStatus::Deleting;
                b.error = None;
                true
            }
            _ => false,
        };
        for b in branches.values_mut() {
            if b.parent_branch_id == Some(branch_id)
                && b.kind == BranchKind::Backup
                && !matches!(b.status, BranchStatus::Deleting | BranchStatus::Deleted)
            {
                b.status = BranchStatus::Deleting;
            }
        }
        Ok(if moved { BranchDeleteOutcome::Started } else { BranchDeleteOutcome::NotMoved })
    }

    async fn update_database_settings(
        &self,
        database_id: DatabaseId,
        patch: &SettingsPatch,
    ) -> Result<bool, MetadataError> {
        let mut dbs = self.databases.lock().unwrap();
        match dbs.get_mut(&database_id) {
            Some(db) if !matches!(db.status, DatabaseStatus::Deleting | DatabaseStatus::Deleted) => {
                if let Some(w) = patch.history_window {
                    db.history_window = w;
                }
                if let Some(m) = patch.max_branches {
                    db.max_branches = m;
                }
                if let Some(v) = patch.max_rows {
                    db.max_rows = v;
                }
                if let Some(v) = patch.statement_timeout_ms {
                    db.statement_timeout_ms = v;
                }
                if let Some(v) = patch.max_connections {
                    db.max_connections = v;
                }
                if let Some(v) = patch.storage_max_connections {
                    db.storage_max_connections = v;
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn insert_data_key(&self, key: &NewDataKey) -> Result<(), MetadataError> {
        let mut keys = self.keys.lock().unwrap();
        insert_key(&mut keys, key)
    }

    async fn create_secret_key(&self, key: &NewDataKey) -> Result<(), MetadataError> {
        if *self.endpoint_busy.lock().unwrap() {
            return Err(MetadataError::EndpointLockTimeout("ep-test".into()));
        }
        let mut keys = self.keys.lock().unwrap();
        insert_key(&mut keys, key)
    }

    async fn rotate_secret_key(
        &self,
        branch: BranchId,
        old: uuid::Uuid,
        new: &NewDataKey,
    ) -> Result<bool, MetadataError> {
        if *self.endpoint_busy.lock().unwrap() {
            return Err(MetadataError::EndpointLockTimeout("ep-test".into()));
        }
        // As the store: the old key's identity carries over (`rotated_identity`, E26).
        let stored = self
            .keys
            .lock()
            .unwrap()
            .iter()
            .find(|(k, _)| k.id == old && k.branch_id == branch && k.revoked_at.is_none())
            .map(|(k, _)| (k.role.clone(), k.role_oid, k.role_timeline.clone()));
        let mut new = new.clone();
        if let Some((role, oid, timeline)) = stored {
            (new.role_oid, new.role_timeline) = chelabase_metadata::rotated_identity(
                &role,
                (oid, timeline),
                (new.role_oid, new.role_timeline.clone()),
                &new.role,
            )?;
        }
        self.rotate_data_key(branch, old, &new).await
    }

    async fn list_data_keys(&self, branch: BranchId) -> Result<Vec<DataKeyRecord>, MetadataError> {
        let mut list: Vec<_> = self
            .keys
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| k.branch_id == branch && k.revoked_at.is_none())
            .map(|(k, _)| k.clone())
            .collect();
        list.reverse(); // newest first
        Ok(list)
    }

    async fn revoke_data_key(&self, branch: BranchId, key_id: uuid::Uuid) -> Result<bool, MetadataError> {
        let mut keys = self.keys.lock().unwrap();
        match keys.iter_mut().find(|(k, _)| {
            k.id == key_id && k.branch_id == branch && k.revoked_at.is_none() && k.kind == DataKeyKind::Secret
        }) {
            Some((k, _)) => {
                k.revoked_at = Some(chrono::Utc::now());
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn rotate_data_key(
        &self,
        branch: BranchId,
        old: uuid::Uuid,
        new: &NewDataKey,
    ) -> Result<bool, MetadataError> {
        let mut keys = self.keys.lock().unwrap();
        let Some(i) = keys
            .iter()
            .position(|(k, _)| k.id == old && k.branch_id == branch && k.revoked_at.is_none())
        else {
            return Ok(false);
        };
        keys[i].0.revoked_at = Some(chrono::Utc::now());
        // As the real one does: the replacement reaches the same databases.
        let mut new = new.clone();
        new.databases = keys[i].0.databases.clone();
        match insert_key(&mut keys, &new) {
            Ok(()) => Ok(true),
            Err(e) => {
                keys[i].0.revoked_at = None;
                Err(e)
            }
        }
    }

    async fn list_roles(&self, endpoint_id: &str) -> Result<Vec<RoleRow>, MetadataError> {
        self.role_reads.fetch_add(1, Ordering::SeqCst);
        if *self.fail_list_roles.lock().unwrap() {
            return Err(MetadataError::Config("injected".into()));
        }
        Ok(self.roles.lock().unwrap().get(endpoint_id).cloned().unwrap_or_default())
    }

    async fn member_branch_roles(&self, branch: uuid::Uuid) -> Result<Vec<MemberBranchRole>, MetadataError> {
        Ok(self.member_roles.lock().unwrap().iter().filter(|r| r.branch_id == branch).cloned().collect())
    }

    async fn record_member_acted_role(
        &self,
        member: uuid::Uuid,
        branch: uuid::Uuid,
        role: &str,
    ) -> Result<bool, MetadataError> {
        let has_row = self.member_roles.lock().unwrap().iter().any(|r| (r.member_id, r.branch_id) == (member, branch));
        if has_row {
            self.acted_roles.lock().unwrap().insert((member, branch), role.to_string());
        }
        Ok(has_row)
    }

    async fn role_password(
        &self,
        endpoint_id: &str,
        name: &str,
    ) -> Result<Option<String>, MetadataError> {
        Ok(self
            .role_passwords
            .lock()
            .unwrap()
            .get(&(endpoint_id.to_string(), name.to_string()))
            .cloned())
    }

    async fn branches_with_main_database(&self, ids: &[BranchId]) -> Result<HashSet<BranchId>, MetadataError> {
        self.main_db_batch_queries.fetch_add(1, Ordering::SeqCst);
        let dbs = self.branch_dbs.lock().unwrap();
        Ok(ids
            .iter()
            .filter(|id| dbs.get(id).is_some_and(|rows| rows.iter().any(|d| d.origin == DatabaseOrigin::Main)))
            .copied()
            .collect())
    }

    async fn begin_enable_data_api(
        &self,
        branch: BranchId,
        name: &str,
        cap: u32,
    ) -> Result<chelabase_metadata::EnableStart, MetadataError> {
        use chelabase_metadata::{DataApiState, EnableStart};
        let mut all = self.branch_dbs.lock().unwrap();
        let rows = all.entry(branch).or_default();
        let Some(i) = rows.iter().position(|d| d.name == name) else {
            return Ok(EnableStart::Unknown);
        };
        if rows[i].origin == DatabaseOrigin::System {
            return Ok(EnableStart::System);
        }
        if rows[i].data_api == DataApiState::On {
            return Ok(EnableStart::AlreadyOn);
        }
        let others = rows
            .iter()
            .filter(|d| d.name != name && matches!(d.data_api, DataApiState::On | DataApiState::Enabling))
            .count();
        if others >= cap as usize {
            return Ok(EnableStart::OverCap(cap));
        }
        rows[i].data_api = DataApiState::Enabling;
        rows[i].data_api_error = None;
        rows[i].data_api_bootstrapped = true;
        Ok(EnableStart::Started)
    }

    async fn set_database_default_schema(
        &self,
        branch: BranchId,
        name: &str,
        schema: &str,
    ) -> Result<bool, MetadataError> {
        let mut all = self.branch_dbs.lock().unwrap();
        match all.entry(branch).or_default().iter_mut().find(|d| d.name == name) {
            Some(d) => {
                d.default_schema = schema.to_string();
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn set_key_databases(
        &self,
        branch: BranchId,
        key_id: uuid::Uuid,
        databases: Option<Vec<String>>,
    ) -> Result<bool, MetadataError> {
        let mut keys = self.keys.lock().unwrap();
        match keys
            .iter_mut()
            .find(|(k, _)| k.id == key_id && k.branch_id == branch && k.revoked_at.is_none())
        {
            Some((k, _)) => {
                k.databases = databases;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn branch_databases(&self, branch: BranchId) -> Result<Vec<BranchDatabase>, MetadataError> {
        self.branch_db_queries.fetch_add(1, Ordering::SeqCst);
        let mut rows = self.branch_dbs.lock().unwrap().get(&branch).cloned().unwrap_or_default();
        rows.sort_by_key(|d| (d.origin != DatabaseOrigin::Main, d.name.clone()));
        Ok(rows)
    }

    async fn list_projects(&self, tenant: TenantId) -> Result<Vec<ProjectRecord>, MetadataError> {
        Ok(self
            .projects
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.tenant_id == tenant && p.status != ProjectStatus::Deleted)
            .cloned()
            .collect())
    }

    async fn get_project(&self, tenant: TenantId, id: ProjectId) -> Result<Option<ProjectRecord>, MetadataError> {
        Ok(self
            .projects
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.id == id && p.tenant_id == tenant && p.status != ProjectStatus::Deleted)
            .cloned())
    }

    async fn project_by_id(&self, id: ProjectId) -> Result<Option<ProjectRecord>, MetadataError> {
        Ok(self
            .projects
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.id == id && p.status != ProjectStatus::Deleted)
            .cloned())
    }

    async fn create_project(&self, tenant: TenantId, name: &str) -> Result<ProjectRecord, MetadataError> {
        let mut projects = self.projects.lock().unwrap();
        let taken: HashSet<String> = projects
            .iter()
            .filter(|p| p.tenant_id == tenant && p.status != ProjectStatus::Deleted)
            .map(|p| p.slug.clone())
            .collect();
        let base = chelabase_metadata::slug_base(name);
        let mut slug = base.clone();
        let mut n = 2;
        while taken.contains(&slug) {
            slug = format!("{base}-{n}");
            n += 1;
        }
        let now = chrono::Utc::now();
        let record = ProjectRecord {
            id: ProjectId::new(),
            tenant_id: tenant,
            name: name.to_string(),
            slug,
            status: ProjectStatus::Active,
            error: None,
            created_at: now,
            updated_at: now,
        };
        projects.push(record.clone());
        Ok(record)
    }

    async fn rename_project(
        &self,
        tenant: TenantId,
        id: ProjectId,
        name: &str,
    ) -> Result<RenameProject, MetadataError> {
        let mut projects = self.projects.lock().unwrap();
        let Some(p) = projects
            .iter_mut()
            .find(|p| p.id == id && p.tenant_id == tenant && p.status != ProjectStatus::Deleted)
        else {
            return Ok(RenameProject::NotFound);
        };
        if p.status != ProjectStatus::Active {
            return Ok(RenameProject::Deleting);
        }
        p.name = name.to_string();
        p.updated_at = chrono::Utc::now();
        Ok(RenameProject::Renamed(p.clone()))
    }

    /// Mirrors the real function's refusal order; the databases are the fake's own rows.
    async fn begin_project_delete(
        &self,
        tenant: TenantId,
        id: ProjectId,
    ) -> Result<BeginProjectDelete, MetadataError> {
        let mut projects = self.projects.lock().unwrap();
        let Some(project) = projects
            .iter_mut()
            .find(|p| p.id == id && p.tenant_id == tenant && p.status != ProjectStatus::Deleted)
        else {
            return Ok(BeginProjectDelete::NotFound);
        };
        if project.status == ProjectStatus::Deleting {
            return Ok(BeginProjectDelete::AlreadyDeleting);
        }
        let mut databases = self.databases.lock().unwrap();
        let branches = self.branches.lock().unwrap();
        let mut ids: Vec<DatabaseId> = databases
            .values()
            .filter(|d| d.project_id == id && d.status != DatabaseStatus::Deleted)
            .map(|d| d.database_id)
            .collect();
        ids.sort_by_key(|d| d.as_uuid());
        for db in &ids {
            if matches!(databases[db].status, DatabaseStatus::Pending | DatabaseStatus::Provisioning) {
                return Ok(BeginProjectDelete::NotReady(*db));
            }
        }
        if let Some(b) = branches
            .values()
            .find(|b| ids.contains(&b.database_id) && b.protected && b.status != BranchStatus::Deleting)
        {
            return Ok(BeginProjectDelete::Protected(b.database_id));
        }
        if let Some(auth) = self.project_auths.lock().unwrap().get(&id)
            && matches!(auth.status, AppUsersStatus::Enabling | AppUsersStatus::Disabling)
        {
            return Ok(BeginProjectDelete::AppUsersBusy);
        }
        project.status = ProjectStatus::Deleting;
        let mut started = Vec::new();
        for db in ids {
            let record = databases.get_mut(&db).unwrap();
            if matches!(record.status, DatabaseStatus::Ready | DatabaseStatus::Failed) {
                record.status = DatabaseStatus::Deleting;
                started.push(db);
            }
        }
        Ok(BeginProjectDelete::Started(started))
    }

    async fn project_databases(
        &self,
        id: ProjectId,
    ) -> Result<Vec<(DatabaseId, DatabaseStatus, Option<String>)>, MetadataError> {
        Ok(self
            .databases
            .lock()
            .unwrap()
            .values()
            .filter(|d| d.project_id == id && d.status != DatabaseStatus::Deleted)
            .map(|d| (d.database_id, d.status, d.error.clone()))
            .collect())
    }

    async fn project_auth(&self, project: ProjectId) -> Result<Option<ProjectAuth>, MetadataError> {
        self.project_auth_reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.project_auths.lock().unwrap().get(&project).cloned())
    }

    async fn project_auth_statuses(&self, tenant: TenantId) -> Result<HashMap<ProjectId, AppUsersStatus>, MetadataError> {
        self.project_auth_batch_reads.fetch_add(1, Ordering::SeqCst);
        let projects = self.projects.lock().unwrap();
        Ok(self
            .project_auths
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, _)| projects.iter().any(|p| p.id == **id && p.tenant_id == tenant))
            .map(|(id, a)| (*id, a.status))
            .collect())
    }

    async fn begin_enable_app_users(&self, new: &NewProjectAuth<'_>) -> Result<BeginEnable, MetadataError> {
        let mut auths = self.project_auths.lock().unwrap();
        if let Some(existing) = auths.get_mut(&new.project_id) {
            let same = existing.database_id == new.database_id
                && existing.pg_database == new.pg_database
                && existing.schema_name == new.schema_name
                && existing.table_name == new.table_name;
            let on_hold = existing.status == AppUsersStatus::Disabling && existing.error.is_some();
            if (existing.status == AppUsersStatus::Failed || on_hold) && same {
                existing.status = AppUsersStatus::Enabling;
                existing.error = None;
                existing.email_verification = new.email_verification;
                existing.on_delete = new.on_delete;
                return Ok(BeginEnable::Retry);
            }
            return Ok(BeginEnable::Exists(existing.clone()));
        }
        let now = chrono::Utc::now();
        auths.insert(
            new.project_id,
            ProjectAuth {
                project_id: new.project_id,
                status: AppUsersStatus::Enabling,
                error: None,
                database_id: new.database_id,
                pg_database: new.pg_database.to_string(),
                schema_name: new.schema_name.to_string(),
                table_name: new.table_name.to_string(),
                email_verification: new.email_verification,
                on_delete: new.on_delete,
                created_at: now,
                updated_at: now,
            },
        );
        Ok(BeginEnable::Started)
    }

    async fn update_app_users_settings(
        &self,
        project: ProjectId,
        email_verification: Option<bool>,
        on_delete: Option<OnDelete>,
    ) -> Result<Option<ProjectAuth>, MetadataError> {
        let mut auths = self.project_auths.lock().unwrap();
        let Some(auth) = auths.get_mut(&project) else { return Ok(None) };
        if let Some(v) = email_verification {
            auth.email_verification = v;
        }
        if let Some(v) = on_delete {
            auth.on_delete = v;
        }
        Ok(Some(auth.clone()))
    }

    async fn begin_disable_app_users(&self, project: ProjectId) -> Result<bool, MetadataError> {
        let mut auths = self.project_auths.lock().unwrap();
        match auths.get_mut(&project) {
            Some(a) if matches!(a.status, AppUsersStatus::Enabled | AppUsersStatus::Failed) => {
                a.status = AppUsersStatus::Disabling;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn branch_auth_sync(&self, branch: BranchId) -> Result<Option<BranchAuthSync>, MetadataError> {
        Ok(self.branch_syncs.lock().unwrap().get(&branch).cloned())
    }

    async fn list_user_sessions(&self, project: ProjectId, user: uuid::Uuid) -> Result<Vec<AppSession>, MetadataError> {
        Ok(self
            .app_sessions
            .lock()
            .unwrap()
            .iter()
            // Like the real query: live only (not revoked, not expired).
            .filter(|s| s.project_id == project && s.user_id == user && s.revoked_at.is_none() && s.expires_at > chrono::Utc::now())
            .cloned()
            .collect())
    }

    async fn revoke_user_sessions(
        &self,
        project: ProjectId,
        user: uuid::Uuid,
        keep: Option<uuid::Uuid>,
        created_before: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Vec<AppSession>, MetadataError> {
        if *self.fail_revoke.lock().unwrap() {
            return Err(MetadataError::Conflict);
        }
        let mut revoked = Vec::new();
        for s in self.app_sessions.lock().unwrap().iter_mut() {
            if s.project_id == project
                && s.user_id == user
                && s.revoked_at.is_none()
                && Some(s.id) != keep
                && created_before.is_none_or(|t| s.created_at < t)
            {
                s.revoked_at = Some(chrono::Utc::now());
                revoked.push(s.clone());
            }
        }
        Ok(revoked)
    }

    async fn app_auth_for_database(&self, db: DatabaseId) -> Result<Option<ProjectAuth>, MetadataError> {
        Ok(self.project_auths.lock().unwrap().values().find(|a| a.database_id == db).cloned())
    }

    async fn list_saved_queries(&self, database: uuid::Uuid) -> Result<Vec<SavedQuerySummary>, MetadataError> {
        let mut rows: Vec<SavedQuerySummary> = self
            .saved_queries
            .lock()
            .unwrap()
            .iter()
            .filter(|(d, _, _)| *d == database)
            .map(|(_, _, q)| SavedQuerySummary {
                id: q.id,
                name: q.name.clone(),
                pg_database: q.pg_database.clone(),
                role: q.role.clone(),
                version: q.version,
                updated_at: q.updated_at,
            })
            .collect();
        rows.sort_by_key(|q| q.name.to_lowercase());
        Ok(rows)
    }

    async fn get_saved_query(&self, database: uuid::Uuid, id: uuid::Uuid) -> Result<Option<SavedQuery>, MetadataError> {
        Ok(self
            .saved_queries
            .lock()
            .unwrap()
            .iter()
            .find(|(d, _, q)| *d == database && q.id == id)
            .map(|(_, _, q)| q.clone()))
    }

    async fn create_saved_query(
        &self,
        database: uuid::Uuid,
        user: &str,
        author: Option<uuid::Uuid>,
        id: uuid::Uuid,
        name: &str,
        sql: &str,
        pg_database: Option<&str>,
        role: Option<&str>,
    ) -> Result<SavedQueryWrite, MetadataError> {
        let mut rows = self.saved_queries.lock().unwrap();
        let mine = || rows.iter().filter(|(d, u, _)| *d == database && u == user);
        if mine().count() as i64 >= orchestrator::sql_editor::SAVED_QUERY_CAP {
            return Ok(SavedQueryWrite::CapReached);
        }
        if mine().any(|(_, _, q)| q.name.to_lowercase() == name.to_lowercase()) {
            return Ok(SavedQueryWrite::NameTaken);
        }
        let now = chrono::Utc::now();
        let q = SavedQuery {
            id,
            name: name.into(),
            sql: sql.into(),
            pg_database: pg_database.map(Into::into),
            role: role.map(Into::into),
            version: 1,
            created_at: now,
            updated_at: now,
            author_member_id: author,
        };
        rows.push((database, user.into(), q.clone()));
        Ok(SavedQueryWrite::Saved(q))
    }

    async fn update_saved_query(
        &self,
        database: uuid::Uuid,
        by: SavedQueryEditor,
        id: uuid::Uuid,
        version: i32,
        patch: &SavedQueryPatch,
    ) -> Result<SavedQueryWrite, MetadataError> {
        let may = |q: &SavedQuery| match by {
            SavedQueryEditor::Admin => true,
            SavedQueryEditor::Author(member) => q.author_member_id == Some(member),
        };
        let mut rows = self.saved_queries.lock().unwrap();
        let Some(at) = rows.iter().position(|(d, _, q)| *d == database && q.id == id && may(q)) else {
            return Ok(SavedQueryWrite::NotFound);
        };
        if rows[at].2.version != version {
            return Ok(SavedQueryWrite::Stale(rows[at].2.clone()));
        }
        let owner = rows[at].1.clone();
        if let Some(name) = &patch.name
            && rows.iter().any(|(d, u, q)| {
                *d == database && *u == owner && q.id != id && q.name.to_lowercase() == name.to_lowercase()
            })
        {
            return Ok(SavedQueryWrite::NameTaken);
        }
        let q = &mut rows[at].2;
        if let Some(name) = &patch.name {
            q.name = name.clone();
        }
        if let Some(sql) = &patch.sql {
            q.sql = sql.clone();
        }
        if let Some(v) = &patch.pg_database {
            q.pg_database = v.clone();
        }
        if let Some(v) = &patch.role {
            q.role = v.clone();
        }
        q.version += 1;
        q.updated_at = chrono::Utc::now();
        Ok(SavedQueryWrite::Saved(q.clone()))
    }

    async fn create_storage_webhook(&self, new: &NewStorageWebhook<'_>) -> Result<Option<StorageWebhook>, MetadataError> {
        let mut rows = self.storage_webhooks.lock().unwrap();
        if rows.iter().filter(|(w, _)| w.database_id == new.database_id).count() as i64
            >= chelabase_metadata::STORAGE_WEBHOOK_CAP
        {
            return Ok(None);
        }
        let now = chrono::Utc::now();
        let w = StorageWebhook {
            id: new.id,
            database_id: new.database_id,
            url: new.url.to_string(),
            events: new.events.to_vec(),
            bucket: new.bucket.map(Into::into),
            prefix: new.prefix.map(Into::into),
            enabled: true,
            enabled_since: now,
            disabled_reason: None,
            failing_since: None,
            consecutive_failures: 0,
            last_attempt_at: None,
            created_at: now,
            updated_at: now,
        };
        rows.push((w.clone(), new.secret_enc.to_string()));
        Ok(Some(w))
    }

    async fn list_storage_webhooks(&self, database: uuid::Uuid) -> Result<Vec<StorageWebhook>, MetadataError> {
        let rows = self.storage_webhooks.lock().unwrap();
        Ok(rows.iter().filter(|(w, _)| w.database_id == database).map(|(w, _)| w.clone()).collect())
    }

    async fn get_storage_webhook(&self, database: uuid::Uuid, id: uuid::Uuid) -> Result<Option<StorageWebhook>, MetadataError> {
        let rows = self.storage_webhooks.lock().unwrap();
        Ok(rows.iter().find(|(w, _)| w.database_id == database && w.id == id).map(|(w, _)| w.clone()))
    }

    async fn update_storage_webhook(
        &self,
        database: uuid::Uuid,
        id: uuid::Uuid,
        patch: &StorageWebhookPatch,
    ) -> Result<Option<StorageWebhook>, MetadataError> {
        let mut rows = self.storage_webhooks.lock().unwrap();
        let Some((w, _)) = rows.iter_mut().find(|(w, _)| w.database_id == database && w.id == id) else {
            return Ok(None);
        };
        if let Some(url) = &patch.url {
            w.url = url.clone();
        }
        if let Some(events) = &patch.events {
            w.events = events.clone();
        }
        if let Some(bucket) = &patch.bucket {
            w.bucket = bucket.clone();
        }
        if let Some(prefix) = &patch.prefix {
            w.prefix = prefix.clone();
        }
        match patch.enabled {
            Some(true) if !w.enabled => {
                w.enabled = true;
                w.enabled_since = chrono::Utc::now();
                w.disabled_reason = None;
                w.failing_since = None;
                w.consecutive_failures = 0;
            }
            Some(false) if w.enabled => {
                w.enabled = false;
                w.disabled_reason = Some("member".into());
            }
            _ => {}
        }
        Ok(Some(w.clone()))
    }

    async fn delete_storage_webhook(&self, database: uuid::Uuid, id: uuid::Uuid) -> Result<bool, MetadataError> {
        let mut rows = self.storage_webhooks.lock().unwrap();
        let before = rows.len();
        rows.retain(|(w, _)| !(w.database_id == database && w.id == id));
        Ok(rows.len() < before)
    }

    async fn rotate_storage_webhook_secret(
        &self,
        database: uuid::Uuid,
        id: uuid::Uuid,
        secret_enc: &str,
    ) -> Result<Option<StorageWebhook>, MetadataError> {
        let mut rows = self.storage_webhooks.lock().unwrap();
        Ok(rows.iter_mut().find(|(w, _)| w.database_id == database && w.id == id).map(|(w, sealed)| {
            *sealed = secret_enc.to_string();
            w.clone()
        }))
    }

    async fn list_storage_webhook_attempts(
        &self,
        database: uuid::Uuid,
        id: uuid::Uuid,
        limit: i64,
    ) -> Result<Vec<StorageWebhookAttempt>, MetadataError> {
        let known = self.storage_webhooks.lock().unwrap().iter().any(|(w, _)| w.database_id == database && w.id == id);
        let attempts = self.storage_webhook_attempts.lock().unwrap();
        Ok(attempts
            .iter()
            .rev()
            .filter(|(w, _)| known && *w == id)
            .take(limit as usize)
            .map(|(_, a)| a.clone())
            .collect())
    }

    async fn delete_saved_query(&self, database: uuid::Uuid, by: SavedQueryEditor, id: uuid::Uuid) -> Result<bool, MetadataError> {
        let mut rows = self.saved_queries.lock().unwrap();
        let before = rows.len();
        rows.retain(|(d, _, q)| {
            let may = match by {
                SavedQueryEditor::Admin => true,
                SavedQueryEditor::Author(member) => q.author_member_id == Some(member),
            };
            !(*d == database && q.id == id && may)
        });
        Ok(rows.len() < before)
    }
    async fn create_merge(&self, m: &NewMerge) -> Result<MergeCreate, MetadataError> {
        if *self.create_merge_conflicts.lock().unwrap() {
            return Err(MetadataError::Conflict);
        }
        let mut rows = self.merges.lock().unwrap();
        if let Some(r) = rows.iter().find(|r| r.target_branch_id == m.target_branch_id.as_uuid() && r.state().is_applying()) {
            return Ok(MergeCreate::ApplyingExists(r.id));
        }
        if let Some(r) = rows.iter().find(|r| {
            r.source_branch_id == m.source_branch_id.as_uuid()
                && r.target_branch_id == m.target_branch_id.as_uuid()
                && r.database_name == m.database_name
                && r.state().is_open()
        }) {
            return Ok(MergeCreate::OpenExists(r.id));
        }
        let now = chrono::Utc::now();
        let record = MergeRecord {
            id: m.id,
            database_id: m.database_id.as_uuid(),
            source_branch_id: m.source_branch_id.as_uuid(),
            target_branch_id: m.target_branch_id.as_uuid(),
            direction: m.direction,
            database_name: m.database_name.clone(),
            status: MergeStatus::Computing,
            failed_phase: None,
            version: 1,
            base_fingerprint: None,
            source_fingerprint: None,
            target_fingerprint: None,
            analysis: None,
            resolutions: json!([]),
            user_steps: json!([]),
            acknowledgements: json!([]),
            options: m.options.clone().unwrap_or_else(|| json!({"lock_timeout_ms": 5000, "after": "keep"})),
            plan: None,
            rehearsal: None,
            result: None,
            rebaselined: false,
            error: None,
            claimed_at: Some(now),
            created_by: m.created_by.clone(),
            created_at: now,
            updated_at: now,
        };
        rows.push(record.clone());
        Ok(MergeCreate::Created(record))
    }

    async fn get_merge(&self, id: uuid::Uuid) -> Result<Option<MergeRecord>, MetadataError> {
        Ok(self.merges.lock().unwrap().iter().find(|r| r.id == id).cloned())
    }

    async fn list_merges(&self, database_id: DatabaseId, branch: Option<BranchId>) -> Result<Vec<MergeRecord>, MetadataError> {
        let mut rows: Vec<MergeRecord> = self
            .merges
            .lock()
            .unwrap()
            .iter()
            .filter(|r| {
                r.database_id == database_id.as_uuid()
                    && branch.is_none_or(|b| r.source_branch_id == b.as_uuid() || r.target_branch_id == b.as_uuid())
            })
            .cloned()
            .collect();
        rows.reverse();
        Ok(rows)
    }

    async fn update_merge_draft(&self, id: uuid::Uuid, expected_version: i64, patch: &MergeDraftPatch) -> Result<MergeWrite, MetadataError> {
        let mut rows = self.merges.lock().unwrap();
        let Some(r) = rows.iter_mut().find(|r| r.id == id) else { return Ok(MergeWrite::NotFound) };
        if !matches!(r.status, MergeStatus::Draft | MergeStatus::Rehearsed) {
            return Ok(MergeWrite::NotEditable(r.status));
        }
        if r.version != expected_version {
            return Ok(MergeWrite::Stale(r.clone()));
        }
        r.version += 1;
        if r.status == MergeStatus::Rehearsed && (patch.start_compute || !patch.keep_plan) {
            r.rehearsal = None;
            r.status = MergeStatus::Draft;
        }
        if patch.start_compute {
            r.status = MergeStatus::Computing;
            r.claimed_at = Some(chrono::Utc::now());
        }
        if let Some(v) = &patch.resolutions { r.resolutions = v.clone(); }
        if let Some(v) = &patch.user_steps { r.user_steps = v.clone(); }
        if let Some(v) = &patch.acknowledgements { r.acknowledgements = v.clone(); }
        if let Some(v) = &patch.options { r.options = v.clone(); }
        Ok(MergeWrite::Saved(r.clone()))
    }

    async fn set_merge_status(&self, id: uuid::Uuid, from: &[MergeStatus], to: MergeStatus, patch: MergeStatusPatch) -> Result<bool, MetadataError> {
        if patch.guard_branches_ready {
            let Some(m) = self.merges.lock().unwrap().iter().find(|r| r.id == id).cloned() else { return Ok(false) };
            let branches = self.branches.lock().unwrap();
            let ready = |b: uuid::Uuid| branches.get(&BranchId(b)).is_some_and(|b| b.status == BranchStatus::Ready);
            if !ready(m.source_branch_id) || !ready(m.target_branch_id) {
                return Ok(false);
            }
        }
        let mut rows = self.merges.lock().unwrap();
        if std::mem::take(&mut *self.bump_merge_version_on_write.lock().unwrap())
            && let Some(r) = rows.iter_mut().find(|r| r.id == id)
        {
            r.version += 1;
        }
        if let Some(result) = self.result_changes_before_write.lock().unwrap().take()
            && let Some(r) = rows.iter_mut().find(|r| r.id == id)
        {
            r.result = Some(result);
        }
        let Some(r) = rows.iter().position(|r| {
            r.id == id
                && from.contains(&r.status)
                && patch.guard_failed_phase.as_ref().is_none_or(|p| &r.failed_phase == p)
                && patch.guard_version.is_none_or(|v| r.version == v)
                && patch.guard_result.as_ref().is_none_or(|v| &r.result == v)
        }) else {
            return Ok(false);
        };
        // `merges_one_applying`: the state after the write against the others'.
        let failed_phase = patch.failed_phase.clone().unwrap_or_else(|| rows[r].failed_phase.clone());
        let after = chelabase_metadata::MergeState::of(to, failed_phase.as_deref());
        if after.is_applying()
            && rows.iter().any(|o| o.id != id && o.target_branch_id == rows[r].target_branch_id && o.state().is_applying())
        {
            return Err(MetadataError::Conflict);
        }
        if patch.log_status {
            let mut e = MergeEvent::new(id, "status");
            e.detail = Some(json!({ "to": to.as_str() }));
            self.merge_events.lock().unwrap().push(e);
        }
        let r = &mut rows[r];
        r.status = to;
        r.claimed_at = to.is_working().then(chrono::Utc::now);
        if patch.analysis.is_some() || patch.plan.is_some() { r.version += 1; }
        if let Some(v) = patch.analysis { r.analysis = Some(v); }
        if let Some(v) = patch.plan { r.plan = Some(v); }
        if let Some(v) = patch.rehearsal { r.rehearsal = Some(v); }
        if let Some(v) = patch.result { r.result = Some(v); }
        if let Some(v) = patch.error { r.error = v; }
        if let Some(v) = patch.failed_phase { r.failed_phase = v; }
        if let Some(v) = patch.rebaselined { r.rebaselined = v; }
        if let Some(v) = patch.options { r.options = v; }
        Ok(true)
    }

    async fn merge_events(&self, id: uuid::Uuid) -> Result<Vec<MergeEvent>, MetadataError> {
        Ok(self.merge_events.lock().unwrap().iter().filter(|e| e.merge_id == id).cloned().collect())
    }

    async fn cancel_drafts_for_branch(&self, branch: BranchId) -> Result<u64, MetadataError> {
        let mut n = 0;
        for r in self.merges.lock().unwrap().iter_mut() {
            if (r.source_branch_id == branch.as_uuid() || r.target_branch_id == branch.as_uuid())
                && matches!(r.status, MergeStatus::Computing | MergeStatus::Draft | MergeStatus::Rehearsed)
            {
                r.status = MergeStatus::Cancelled;
                n += 1;
            }
        }
        Ok(n)
    }

    async fn branch_has_busy_merge(&self, branch: BranchId) -> Result<Option<BranchBusy>, MetadataError> {
        if *self.busy_precheck_misses.lock().unwrap() {
            return Ok(None);
        }
        Ok(self.busy(branch))
    }

    async fn member_by_identity_user(&self, tenant: TenantId, identity_user_id: &str) -> Result<Option<MemberRecord>, MetadataError> {
        self.member_reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.member(tenant, identity_user_id))
    }

    /// As the real one: a removed or pending member is returned as is, an invited one is
    /// activated, an active one gets the claims, and a new one claims the admin when the tenant
    /// has no active admin, else is active, pending or refused (`NotAdmitted`) per `admission`.
    async fn admit_member(
        &self,
        tenant: TenantId,
        identity_user_id: &str,
        email: &str,
        name: &str,
        admission: chelabase_metadata::members::Admission,
    ) -> Result<MemberRecord, MetadataError> {
        use chelabase_metadata::members::Admission;
        self.member_reads.fetch_add(1, Ordering::SeqCst);
        let email = Some(email.trim().to_lowercase()).filter(|e| !e.is_empty());
        let mut members = self.members.lock().unwrap();
        if let Some(m) = members.iter_mut().find(|m| m.tenant_id == tenant && m.identity_user_id == identity_user_id) {
            // S6: a suspended member is returned as is, like a removed or pending one.
            if !matches!(m.status, MemberStatus::Removed | MemberStatus::Pending | MemberStatus::Suspended) {
                if m.status == MemberStatus::Invited {
                    // I1: only while an open invite exists; otherwise the member stays invited.
                    let now = chrono::Utc::now();
                    let mut invites = self.invites.lock().unwrap();
                    let open = invites
                        .iter()
                        .any(|i| i.member_id == m.id && !i.revoked && !i.accepted && i.expires_at > now);
                    if !open {
                        return Ok(m.clone());
                    }
                    m.status = MemberStatus::Active;
                    m.accepted_at = Some(now);
                    for i in invites.iter_mut() {
                        if i.member_id == m.id && !i.revoked {
                            i.accepted = true;
                        }
                    }
                }
                m.email = email;
                m.display_name = name.to_string();
            }
            return Ok(m.clone());
        }
        let has_admin = members
            .iter()
            .any(|m| m.tenant_id == tenant && m.status == MemberStatus::Active && m.tenant_admin);
        let bound = self.setup_claims.lock().unwrap().get(&tenant).cloned();
        let claims = !has_admin && bound.as_deref().is_none_or(|u| u == identity_user_id);
        let status = match (claims, admission) {
            (true, _) | (false, Admission::Active) => MemberStatus::Active,
            (false, Admission::Pending) => MemberStatus::Pending,
            (false, Admission::Closed) => return Err(MetadataError::NotAdmitted(tenant)),
        };
        let record = MemberRecord {
            id: uuid::Uuid::new_v4(),
            tenant_id: tenant,
            identity_user_id: identity_user_id.into(),
            email,
            display_name: name.into(),
            tenant_admin: claims,
            status,
            accepted_at: Some(chrono::Utc::now()),
            offboarded: false, suspended_at: None, suspended_by: None,
        };
        self.stamp_created(record.id);
        members.push(record.clone());
        drop(members);
        if record.tenant_admin {
            self.set_admin_claim(tenant, Some(chrono::Utc::now()));
        }
        if status == MemberStatus::Pending {
            self.log(Change::new(tenant, Some(record.id), "member_joined", Some(record.id)).after(json!({ "status": "pending", "tenant_admin": false })));
        }
        Ok(record)
    }

    async fn grant_sources(
        &self,
        member: uuid::Uuid,
        project: uuid::Uuid,
        database: Option<uuid::Uuid>,
    ) -> Result<GrantSources, MetadataError> {
        let own = self.grants.lock().unwrap().get(&(member, project)).map(GrantRow::to_grant);
        let mut teams = self.team_grants.lock().unwrap().get(&(member, project)).cloned().unwrap_or_default();
        // T23: the stored teams, as the shared shape reads them (P4, P27).
        teams.extend(self.stored_team_grants(member, project).values().map(GrantRow::to_grant));
        let narrowing = database
            .and_then(|d| self.overrides.lock().unwrap().get(&(member, d)).cloned())
            .map(|o| Narrowing {
                platform_role: o.platform_role,
                roles: o.pg_roles.map(|roles| roles.iter().map(|r| PgRole::parse(r)).collect()),
            });
        Ok(GrantSources::new(own, teams, narrowing))
    }

    async fn member_overrides(
        &self,
        member: uuid::Uuid,
    ) -> Result<Vec<(uuid::Uuid, uuid::Uuid, OverrideRow)>, MetadataError> {
        let databases = self.databases.lock().unwrap();
        let mut out: Vec<_> = self
            .overrides
            .lock()
            .unwrap()
            .iter()
            .filter(|((m, _), _)| *m == member)
            .filter_map(|((_, d), o)| {
                let db = databases.get(&DatabaseId(*d))?;
                Some((db.project_id.as_uuid(), *d, o.clone()))
            })
            .collect();
        out.sort_by_key(|(p, d, _)| (*p, *d));
        Ok(out)
    }

    async fn member_projects(&self, member: uuid::Uuid) -> Result<Vec<(uuid::Uuid, GrantRow)>, MetadataError> {
        let mut out: Vec<_> = self
            .grants
            .lock()
            .unwrap()
            .iter()
            .filter(|((m, _), _)| *m == member)
            .map(|((_, p), g)| (*p, g.clone()))
            .collect();
        out.sort_by_key(|(p, _)| *p);
        Ok(out)
    }

    // T23: the team port methods delegate to the fake's `fake_*` methods.
    async fn list_teams(&self, tenant: TenantId) -> Result<Vec<chelabase_metadata::teams::TeamDetail>, MetadataError> {
        self.fake_list_teams(tenant)
    }

    async fn team(&self, tenant: TenantId, team: uuid::Uuid) -> Result<Option<chelabase_metadata::teams::TeamDetail>, MetadataError> {
        self.fake_team(tenant, team)
    }

    async fn member_teams(&self, member: uuid::Uuid) -> Result<Vec<chelabase_metadata::teams::TeamDetail>, MetadataError> {
        self.fake_member_teams(member)
    }

    async fn project_team_grants(&self, project: uuid::Uuid) -> Result<Vec<chelabase_metadata::teams::ProjectTeamGrant>, MetadataError> {
        self.fake_project_team_grants(project)
    }

    async fn create_team(
        &self,
        actor: uuid::Uuid,
        tenant: TenantId,
        name: &str,
        description: &str,
    ) -> Result<chelabase_metadata::teams::TeamRecord, chelabase_metadata::teams::TeamWriteError> {
        self.fake_create_team(actor, tenant, name, description)
    }

    async fn update_team(
        &self,
        actor: uuid::Uuid,
        tenant: TenantId,
        team: uuid::Uuid,
        name: Option<&str>,
        description: Option<&str>,
    ) -> Result<chelabase_metadata::teams::TeamRecord, chelabase_metadata::teams::TeamWriteError> {
        self.fake_update_team(actor, tenant, team, name, description)
    }

    async fn team_change_observed(
        &self,
        tenant: TenantId,
        change: &chelabase_metadata::teams::TeamChange,
    ) -> Result<Vec<(uuid::Uuid, Observed)>, chelabase_metadata::teams::TeamWriteError> {
        self.fake_team_change_observed(tenant, change)
    }

    async fn write_team_checked(
        &self,
        actor: uuid::Uuid,
        tenant: TenantId,
        change: &chelabase_metadata::teams::TeamChange,
        expect: &[(uuid::Uuid, Observed)],
        skipped_admins: &[uuid::Uuid],
    ) -> Result<chelabase_metadata::teams::TeamWrite, chelabase_metadata::teams::TeamWriteError> {
        self.fake_write_team_checked(actor, tenant, change, expect, skipped_admins)
    }
}

impl FakeMetadata {
    /// What holds `branch` (as `MetadataClient::branch_has_busy_merge` reads it).
    fn busy(&self, branch: BranchId) -> Option<BranchBusy> {
        let merge = self.merges.lock().unwrap().iter().find(|r| {
            (r.source_branch_id == branch.as_uuid() || r.target_branch_id == branch.as_uuid()) && r.state().holds_branches()
        }).map(|r| BranchBusy::Merge(r.id));
        merge.or_else(|| {
            self.branches
                .lock()
                .unwrap()
                .values()
                .find(|b| b.parent_branch_id == Some(branch) && b.kind == BranchKind::Rehearsal && b.status != BranchStatus::Deleted)
                .map(|b| BranchBusy::Rehearsal(b.id))
        })
    }
}

fn insert_key(keys: &mut Vec<KeyRow>, key: &NewDataKey) -> Result<(), MetadataError> {
    if key.kind == DataKeyKind::Public
        && keys
            .iter()
            .any(|(k, _)| k.branch_id == key.branch_id && k.kind == DataKeyKind::Public && k.revoked_at.is_none())
    {
        return Err(MetadataError::Conflict);
    }
    keys.push((
        DataKeyRecord {
            id: key.id,
            branch_id: key.branch_id,
            kind: key.kind,
            role: key.role.clone(),
            name: key.name.clone(),
            prefix: key.prefix.clone(),
            sealed: key.sealed.clone(),
            created_at: chrono::Utc::now(),
            last_used_at: None,
            revoked_at: None,
            databases: key.databases.clone(),
            role_oid: key.role_oid,
            role_timeline: key.role_timeline.clone(),
            role_bypass_rls: key.kind == DataKeyKind::Secret && key.role_bypass_rls,
            managed_by_function: None,
        },
        key.sha256,
    ));
    Ok(())
}

#[derive(Clone, Copy, Default)]
enum FakeReset {
    #[default]
    Ok,
    NotReady,
    /// The worker stored the new password but couldn't apply it yet.
    NotApplied,
    Timeout,
    /// The worker gave up on the database (its own deadline).
    WorkerTimedOut,
    /// No worker is listening (NATS no-responders).
    Unavailable,
}

#[derive(Clone, Default)]
struct FakeJobs {
    /// Edge functions m3: each `chelabase.fn.changed` publish, as (branch, function).
    fn_changed: Arc<Mutex<Vec<(BranchId, String)>>>,
    /// Called at each `chelabase.fn.changed` publish (to look at the metadata then).
    on_fn_changed: Arc<Mutex<Option<Arc<dyn Fn(BranchId, &str) + Send + Sync>>>>,
    /// Object storage m9: the storage cache events published.
    storage_events: Arc<Mutex<Vec<chelabase_nats::StorageEvent>>>,
    published: Arc<Mutex<Vec<JobEnvelope<ProvisionDatabaseJob>>>>,
    deleted: Arc<Mutex<Vec<DatabaseId>>>,
    resets: Arc<Mutex<Vec<(DatabaseId, BranchId)>>>,
    reset: Arc<Mutex<FakeReset>>,
    branch_jobs: Arc<Mutex<Vec<JobEnvelope<BranchJob>>>>,
    settings_jobs: Arc<Mutex<Vec<DatabaseId>>>,
    merge_jobs: Arc<Mutex<Vec<MergeJob>>>,
    /// Merge jobs can't be published.
    fail_merge_publish: Arc<Mutex<bool>>,
    /// Provision jobs can't be published.
    fail_provision_publish: Arc<Mutex<bool>>,
    /// Branch jobs can't be published.
    fail_branch_publish: Arc<Mutex<bool>>,
    /// (branch, role) of each check-role request.
    role_checks: Arc<Mutex<Vec<(BranchId, String)>>>,
    role_check: Arc<Mutex<Option<Result<CheckDataRoleReply, ResetError>>>>,
    /// On the next check-role request, the key `id` in this store takes role `name` (a rename
    /// that lands while the worker looks).
    rename_on_check: Arc<Mutex<Option<RenameOnCheck>>>,
    /// Each login-role RPC as `create:<name>:<privileged>`, `reset:<name>` or `delete:<name>`.
    role_calls: Arc<Mutex<Vec<String>>>,
    /// Every create request as sent, and every update request.
    role_creates: Arc<Mutex<Vec<RoleCreateRequest>>>,
    role_updates: Arc<Mutex<Vec<RoleUpdateRequest>>>,
    /// Every delete request as sent (T10: with its successor).
    role_deletes: Arc<Mutex<Vec<RoleDeleteRequest>>>,
    /// T10 fix: every delete-preview request as sent (answered like the other role RPCs).
    role_delete_previews: Arc<Mutex<Vec<RoleDeleteRequest>>>,
    /// What the worker answers to the role RPCs; `ok` when unset.
    role_reply: Arc<Mutex<Option<Result<RoleReply, ResetError>>>>,
    /// (branch, role) of each role-sessions read.
    session_reads: Arc<Mutex<Vec<(BranchId, String)>>>,
    /// What the worker answers to a role-sessions read; running with none when unset.
    sessions_reply: Arc<Mutex<Option<Result<RoleSessionsReply, ResetError>>>>,
    /// Each data API job as `enable:<name>`, `disable:<name>:<remove>` or `rebootstrap:<name>`.
    data_api_calls: Arc<Mutex<Vec<String>>>,
    /// What the worker answers to disable and rebootstrap; `ok` when unset.
    data_api_reply: Arc<Mutex<Option<Result<CheckDataRoleReply, ResetError>>>>,
    /// The enable message can't be published.
    fail_enable_publish: Arc<Mutex<bool>>,
    project_deletes: Arc<Mutex<Vec<ProjectId>>>,
    app_users_jobs: Arc<Mutex<Vec<AppUsersJob>>>,
    app_users_sync_jobs: Arc<Mutex<Vec<AppUsersSyncJob>>>,
    /// Each app-users location check as `<pg_database>.<schema>.<table>`.
    app_users_checks: Arc<Mutex<Vec<String>>>,
    /// What the worker answers to a location check; ok when unset.
    app_users_check_reply: Arc<Mutex<Option<AppUsersCheckReply>>>,
    /// Each member-role reconcile request.
    member_reconciles: Arc<Mutex<Vec<MemberRolesReconcileRequest>>>,
    /// What the worker answers to a reconcile; when unset, `Done` with the rows it applied.
    member_reconcile_reply: Arc<Mutex<Option<Result<MemberRolesReply, ResetError>>>>,
    /// What a reconcile applies: these rows replace the store's rows of the same (member,
    /// branch) when the request's branch (and member, when given) matches.
    member_rows_on_reconcile: Arc<Mutex<Option<MemberRowsOnReconcile>>>,
    /// Members hub T8: runs on each reconcile request before the answer (what the worker
    /// writes meanwhile, such as a personal login's row on the branch).
    member_reconcile_hook: Arc<Mutex<Option<ReconcileHook>>>,
    /// Each hand-over after an act-as run (answered `Done`).
    member_hand_overs: Arc<Mutex<Vec<MemberRoleHandOverRequest>>>,
    /// T7: branches asked to reconcile their member roles, in order.
    member_role_enqueues: Arc<Mutex<Vec<BranchId>>>,
    /// PB: each review request, and what every one answers (unset: `Failed`, no compute).
    review_requests: Arc<Mutex<Vec<chelabase_nats::ReviewRequest>>>,
    review_reply: Arc<Mutex<Option<Result<chelabase_nats::ReviewReply, ResetError>>>>,
    /// EOS-H1: each all-rows check request, and what every one answers (unset: `Clear`).
    all_rows_checks: Arc<Mutex<Vec<chelabase_nats::AllRowsCheckRequest>>>,
    all_rows_reply: Arc<Mutex<Option<Result<chelabase_nats::AllRowsCheckReply, ResetError>>>>,
    /// m18: each storage webhook test request, and what every one answers (unset: delivered).
    webhook_tests: Arc<Mutex<Vec<chelabase_nats::storage_objects::TestRequest>>>,
    webhook_test_reply: Arc<Mutex<Option<Result<chelabase_nats::storage_objects::TestReply, ResetError>>>>,
}

/// A metadata store of member-role rows and the rows a reconcile writes into it.
type MemberRowsOnReconcile = (Arc<Mutex<Vec<MemberBranchRole>>>, Vec<MemberBranchRole>);

/// What the fake worker does on a reconcile request (members hub T8).
type ReconcileHook = Arc<dyn Fn(&MemberRolesReconcileRequest) + Send + Sync>;

impl FakeJobs {
    fn reset_answers(&self, answer: FakeReset) {
        *self.reset.lock().unwrap() = answer;
    }
}

impl JobPort for FakeJobs {
    async fn publish_fn_changed(
        &self,
        branch: BranchId,
        function: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let hook = self.on_fn_changed.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(branch, function);
        }
        self.fn_changed.lock().unwrap().push((branch, function.to_string()));
        Ok(())
    }

    async fn publish_provision(
        &self,
        job: &JobEnvelope<ProvisionDatabaseJob>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if *self.fail_provision_publish.lock().unwrap() {
            return Err("nats is down".into());
        }
        self.published.lock().unwrap().push(job.clone());
        Ok(())
    }

    async fn publish_delete(
        &self,
        job: &JobEnvelope<DeleteDatabaseJob>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.deleted.lock().unwrap().push(job.payload.database_id);
        Ok(())
    }

    async fn publish_branch(
        &self,
        job: &JobEnvelope<BranchJob>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if *self.fail_branch_publish.lock().unwrap() {
            return Err("nats is down".into());
        }
        self.branch_jobs.lock().unwrap().push(job.clone());
        Ok(())
    }

    async fn publish_database_settings(
        &self,
        job: &JobEnvelope<DatabaseSettingsJob>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.settings_jobs.lock().unwrap().push(job.payload.database_id);
        Ok(())
    }

    async fn publish_merge(
        &self,
        job: &JobEnvelope<MergeJob>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if *self.fail_merge_publish.lock().unwrap() {
            return Err("nats is down".into());
        }
        self.merge_jobs.lock().unwrap().push(job.payload.clone());
        Ok(())
    }

    async fn publish_storage_event(
        &self,
        event: &chelabase_nats::StorageEvent,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.storage_events.lock().unwrap().push(event.clone());
        Ok(())
    }

    async fn check_data_role(
        &self,
        branch_id: BranchId,
        role: &str,
    ) -> Result<CheckDataRoleReply, ResetError> {
        self.role_checks.lock().unwrap().push((branch_id, role.to_string()));
        if let Some((keys, id, name)) = self.rename_on_check.lock().unwrap().take() {
            for (k, _) in keys.lock().unwrap().iter_mut() {
                if k.id == id {
                    k.role = name.clone();
                }
            }
        }
        match self.role_check.lock().unwrap().clone() {
            Some(answer) => answer,
            None => Ok(CheckDataRoleReply::ok()),
        }
    }

    async fn reconcile_member_roles(
        &self,
        request: MemberRolesReconcileRequest,
    ) -> Result<MemberRolesReply, ResetError> {
        self.member_reconciles.lock().unwrap().push(request.clone());
        let hook = self.member_reconcile_hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(&request);
        }
        if let Some(answer) = self.member_reconcile_reply.lock().unwrap().clone() {
            return answer;
        }
        let mut applied = Vec::new();
        if let Some((store, rows)) = self.member_rows_on_reconcile.lock().unwrap().clone() {
            let mut store = store.lock().unwrap();
            for row in rows.into_iter().filter(|r| {
                r.branch_id == request.branch_id && request.member_id.is_none_or(|m| m == r.member_id)
            }) {
                store.retain(|r| (r.member_id, r.branch_id) != (row.member_id, row.branch_id));
                applied.push(MemberRoleApplied {
                    member_id: row.member_id,
                    role_name: row.role_name.clone(),
                    applied_roles: row.applied_roles.clone(),
                });
                store.push(row);
            }
        }
        Ok(MemberRolesReply::Done { roles: applied })
    }

    async fn hand_over_member_role(
        &self,
        request: MemberRoleHandOverRequest,
    ) -> Result<MemberRolesReply, ResetError> {
        self.member_hand_overs.lock().unwrap().push(request);
        Ok(MemberRolesReply::Done { roles: vec![] })
    }

    async fn test_storage_webhook(
        &self,
        request: chelabase_nats::storage_objects::TestRequest,
    ) -> Result<chelabase_nats::storage_objects::TestReply, ResetError> {
        self.webhook_tests.lock().unwrap().push(request);
        self.webhook_test_reply.lock().unwrap().clone().unwrap_or(Ok(
            chelabase_nats::storage_objects::TestReply::Sent {
                outcome: chelabase_nats::storage_objects::AttemptOutcome::Delivered,
                status: Some(200),
                duration_ms: 7,
            },
        ))
    }

    async fn review(
        &self,
        request: chelabase_nats::ReviewRequest,
    ) -> Result<chelabase_nats::ReviewReply, ResetError> {
        self.review_requests.lock().unwrap().push(request);
        self.review_reply.lock().unwrap().clone().unwrap_or_else(|| {
            Ok(chelabase_nats::ReviewReply::Failed { message: "no compute in the fake".into() })
        })
    }

    async fn enable_data_api(
        &self,
        _branch_id: BranchId,
        name: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if *self.fail_enable_publish.lock().unwrap() {
            return Err("nats is down".into());
        }
        self.data_api_calls.lock().unwrap().push(format!("enable:{name}"));
        Ok(())
    }

    async fn disable_data_api(
        &self,
        _branch_id: BranchId,
        name: &str,
        remove: bool,
    ) -> Result<CheckDataRoleReply, ResetError> {
        self.data_api_calls.lock().unwrap().push(format!("disable:{name}:{remove}"));
        self.data_api_answer()
    }

    async fn rebootstrap_data_api(
        &self,
        _branch_id: BranchId,
        name: &str,
        grant_existing: bool,
    ) -> Result<CheckDataRoleReply, ResetError> {
        let flag = if grant_existing { ":grant_existing" } else { "" };
        self.data_api_calls.lock().unwrap().push(format!("rebootstrap:{name}{flag}"));
        self.data_api_answer()
    }

    async fn reset_password(
        &self,
        database_id: DatabaseId,
        branch_id: BranchId,
    ) -> Result<ResetPasswordReply, ResetError> {
        self.resets.lock().unwrap().push((database_id, branch_id));
        let answer = *self.reset.lock().unwrap();
        match answer {
            FakeReset::Ok => Ok(ResetPasswordReply { ok: true, error: None }),
            FakeReset::NotApplied => Ok(ResetPasswordReply { ok: false, error: Some(RESET_NOT_APPLIED.into()) }),
            FakeReset::NotReady => Ok(ResetPasswordReply { ok: false, error: Some("not ready".into()) }),
            FakeReset::Timeout => Err(ResetError::Timeout),
            FakeReset::WorkerTimedOut => Ok(ResetPasswordReply { ok: false, error: Some("timed out".into()) }),
            FakeReset::Unavailable => Err(ResetError::Transport("no worker listening".into())),
        }
    }

    async fn create_role(&self, request: RoleCreateRequest) -> Result<RoleReply, ResetError> {
        self.role_calls
            .lock()
            .unwrap()
            .push(format!("create:{}:{}", request.name, request.owner_level));
        self.role_creates.lock().unwrap().push(request);
        self.role_answer()
    }

    async fn update_role(&self, request: RoleUpdateRequest) -> Result<RoleReply, ResetError> {
        self.role_calls.lock().unwrap().push(format!("update:{}", request.name));
        self.role_updates.lock().unwrap().push(request);
        self.role_answer()
    }

    async fn reset_role(&self, request: RoleRequest) -> Result<RoleReply, ResetError> {
        self.role_calls.lock().unwrap().push(format!("reset:{}", request.name));
        self.role_answer()
    }

    async fn delete_role(&self, request: RoleDeleteRequest) -> Result<RoleReply, ResetError> {
        self.role_calls.lock().unwrap().push(format!("delete:{}", request.name));
        self.role_deletes.lock().unwrap().push(request);
        // A done delete answers what it reassigned (T10-4: a reply without it is an older
        // worker's, an error); a reply a test set is answered as set.
        match self.role_answer() {
            Ok(reply) if reply.ok && reply.reassigned.is_none() && self.role_reply.lock().unwrap().is_none() => {
                Ok(RoleReply { reassigned: Some(chelabase_nats::Reassigned::default()), ..reply })
            }
            other => other,
        }
    }

    async fn preview_role_delete(&self, request: RoleDeleteRequest) -> Result<RoleReply, ResetError> {
        self.role_calls.lock().unwrap().push(format!("preview_delete:{}", request.name));
        self.role_delete_previews.lock().unwrap().push(request);
        self.role_answer()
    }

    async fn role_sessions(&self, request: RoleRequest) -> Result<RoleSessionsReply, ResetError> {
        self.session_reads.lock().unwrap().push((request.branch_id, request.name));
        match self.sessions_reply.lock().unwrap().clone() {
            Some(answer) => answer,
            None => Ok(RoleSessionsReply::running(Vec::new())),
        }
    }

    async fn check_all_rows(
        &self,
        request: chelabase_nats::AllRowsCheckRequest,
    ) -> Result<chelabase_nats::AllRowsCheckReply, ResetError> {
        self.all_rows_checks.lock().unwrap().push(request);
        self.all_rows_reply
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(Ok(chelabase_nats::AllRowsCheckReply::Clear))
    }

    async fn publish_project_delete(
        &self,
        job: &JobEnvelope<ProjectDeleteJob>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.project_deletes.lock().unwrap().push(job.payload.project_id);
        Ok(())
    }

    async fn publish_app_users(
        &self,
        job: &JobEnvelope<AppUsersJob>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.app_users_jobs.lock().unwrap().push(job.payload.clone());
        Ok(())
    }

    async fn publish_app_users_sync(
        &self,
        job: &JobEnvelope<AppUsersSyncJob>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.app_users_sync_jobs.lock().unwrap().push(job.payload.clone());
        Ok(())
    }

    async fn check_app_users_location(
        &self,
        request: &AppUsersCheckRequest,
    ) -> Result<AppUsersCheckReply, Box<dyn std::error::Error + Send + Sync>> {
        self.app_users_checks
            .lock()
            .unwrap()
            .push(format!("{}.{}.{}", request.pg_database, request.schema, request.table));
        Ok(self
            .app_users_check_reply
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(AppUsersCheckReply { ok: true, code: None, error: None }))
    }
}

impl FakeJobs {
    fn data_api_answer(&self) -> Result<CheckDataRoleReply, ResetError> {
        match self.data_api_reply.lock().unwrap().clone() {
            Some(answer) => answer,
            None => Ok(CheckDataRoleReply::ok()),
        }
    }

    fn role_answer(&self) -> Result<RoleReply, ResetError> {
        match self.role_reply.lock().unwrap().clone() {
            Some(answer) => answer,
            None => Ok(RoleReply { ok: true, code: None, error: None, keys: vec![], reassigned: None, preview: None }),
        }
    }
}

/// Where the fake Docker says each compute's Postgres is published: one address for every
/// endpoint (`None`: no compute running), and the endpoints asked about.
#[derive(Clone, Default)]
struct FakeLocator {
    addr: Arc<Mutex<Option<std::net::SocketAddr>>>,
    asked: Arc<Mutex<Vec<String>>>,
}

impl PostgresAddr for FakeLocator {
    fn postgres_addr<'a>(
        &'a self,
        endpoint_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<Option<std::net::SocketAddr>>> + Send + 'a>> {
        self.asked.lock().unwrap().push(endpoint_id.to_string());
        let addr = *self.addr.lock().unwrap();
        Box::pin(async move { Ok(addr) })
    }
}

/// RT1 T6: realtime's bearer on the test app's `/internal/realtime/*` routes.
const REALTIME_TOKEN: &str = "realtime-internal-token";

/// One event the orchestrator published for realtime.
#[derive(Clone, Debug, PartialEq)]
enum RtPublished {
    Project(uuid::Uuid, chelabase_nats::rt::ProjectEvent),
    Access(chelabase_nats::rt::AccessEvent),
}

/// Records every realtime event; with `fail`, each publish fails (after recording it).
#[derive(Clone, Default)]
struct FakeRtEvents {
    published: Arc<Mutex<Vec<RtPublished>>>,
    fail: Arc<Mutex<bool>>,
}

// Used by `realtime.rs`, not by every file that includes the fakes.
#[allow(dead_code)]
impl FakeRtEvents {
    fn all(&self) -> Vec<RtPublished> {
        self.published.lock().unwrap().clone()
    }

    /// The access events, in order.
    fn access(&self) -> Vec<chelabase_nats::rt::AccessEvent> {
        self.all()
            .into_iter()
            .filter_map(|e| match e {
                RtPublished::Access(a) => Some(a),
                RtPublished::Project(..) => None,
            })
            .collect()
    }

    /// The project events, in order.
    fn project(&self) -> Vec<(uuid::Uuid, chelabase_nats::rt::ProjectEvent)> {
        self.all()
            .into_iter()
            .filter_map(|e| match e {
                RtPublished::Project(p, ev) => Some((p, ev)),
                RtPublished::Access(_) => None,
            })
            .collect()
    }

    fn clear(&self) {
        self.published.lock().unwrap().clear();
    }
}

impl orchestrator::internal::realtime::RtEvents for FakeRtEvents {
    fn project(
        &self,
        project_id: uuid::Uuid,
        event: chelabase_nats::rt::ProjectEvent,
    ) -> orchestrator::internal::BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async move {
            self.published.lock().unwrap().push(RtPublished::Project(project_id, event));
            anyhow::ensure!(!*self.fail.lock().unwrap(), "nats is down");
            Ok(())
        })
    }

    fn access(
        &self,
        event: chelabase_nats::rt::AccessEvent,
    ) -> orchestrator::internal::BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async move {
            self.published.lock().unwrap().push(RtPublished::Access(event));
            anyhow::ensure!(!*self.fail.lock().unwrap(), "nats is down");
            Ok(())
        })
    }
}

/// Members hub T8 (P4): the proxies' role-secret cache as the app's `proxy_cache`. Records each
/// `password_updated` (endpoint, role) and wakes waiters; with `down`, each one fails after
/// being recorded (Valkey down).
#[derive(Clone, Default)]
struct FakeCache {
    sent: Arc<Mutex<Vec<(String, String)>>>,
    down: Arc<Mutex<bool>>,
    published: Arc<tokio::sync::Notify>,
}

impl orchestrator::internal::CachePort for FakeCache {
    fn password_updated<'a>(
        &'a self,
        endpoint_id: &'a str,
        role: &'a str,
    ) -> orchestrator::internal::BoxFuture<'a, anyhow::Result<u64>> {
        Box::pin(async move {
            self.sent.lock().unwrap().push((endpoint_id.to_string(), role.to_string()));
            self.published.notify_waiters();
            anyhow::ensure!(!*self.down.lock().unwrap(), "valkey is down");
            Ok(1)
        })
    }
}

#[allow(dead_code)]
impl FakeCache {
    fn sent(&self) -> Vec<(String, String)> {
        self.sent.lock().unwrap().clone()
    }

    fn clear(&self) {
        self.sent.lock().unwrap().clear();
    }

    /// Waits (up to 5 s, on the publish notifications, never a sleep) until every (endpoint,
    /// role) of `want` was published at least once; whether they all were.
    async fn wait_for(&self, want: &[(String, String)]) -> bool {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let notified = self.published.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let sent = self.sent();
            if want.iter().all(|w| sent.contains(w)) {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                let sent = self.sent();
                return want.iter().all(|w| sent.contains(w));
            }
        }
    }
}

/// A running orchestrator plus the signer standing in for the gateway.
struct TestApp {
    base: String,
    signer: InternalSigner,
    /// The secrets key the orchestrator decrypts with.
    key: SecretKey,
    /// Where the console finds a compute's Postgres.
    locator: FakeLocator,
    /// The orchestrator's state (the SQL editor's run registry).
    state: Arc<AppState<FakeMetadata, FakeJobs>>,
    /// RT1 T6: what the orchestrator published for realtime.
    #[allow(dead_code)]
    rt_events: FakeRtEvents,
    /// Members hub T8 (P4): the app's `proxy_cache`.
    #[allow(dead_code)]
    cache: FakeCache,
    /// Edge functions m3: the bundle uploads the orchestrator made.
    #[allow(dead_code)]
    bundles: FakeBundles,
}

/// Edge functions m3: the functions service's bundle store, as the orchestrator sees it.
#[derive(Clone, Default)]
struct FakeBundles {
    /// Each upload as (database, sha256, the bytes sent).
    puts: Arc<Mutex<Vec<(DatabaseId, String, Vec<u8>)>>>,
    /// Answer every upload with this status (2xx when unset).
    status: Arc<Mutex<Option<u16>>>,
    /// While set, an upload signals the first and waits for the second before it answers.
    gate: Arc<Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>>,
}

impl orchestrator::functions::BundleSink for FakeBundles {
    fn put<'a>(
        &'a self,
        database: DatabaseId,
        sha256: &'a str,
        manifest: bytes::Bytes,
    ) -> orchestrator::functions::SinkFuture<'a> {
        self.puts.lock().unwrap().push((database, sha256.to_string(), manifest.to_vec()));
        let status = *self.status.lock().unwrap();
        let gate = self.gate.lock().unwrap().clone();
        Box::pin(async move {
            if let Some((started, release)) = gate {
                started.notify_one();
                release.notified().await;
            }
            match status {
                Some(code) if !(200..300).contains(&code) => {
                    Err(orchestrator::functions::SinkError::Status(code))
                }
                _ => Ok(()),
            }
        })
    }
}

impl TestApp {
    /// An internal identity token for the default caller in the tenant labelled `org_id`, as
    /// the gateway mints it from a member token.
    fn token_for_org(&self, org_id: &str) -> String {
        self.token_for(DEFAULT_SUB, org_id)
    }

    /// An internal identity token for person `sub` in the tenant labelled `org_id`
    /// ([`FakeMetadata::bind_org`]); an unknown label names no tenant (the orchestrator answers
    /// 401).
    fn token_for(&self, sub: &str, org_id: &str) -> String {
        let tenant = self.state.metadata.tenants_by_org.lock().unwrap().get(org_id).copied();
        self.sign(&identity_of(sub), tenant)
    }

    /// The gateway's internal token for `identity` in `tenant` (`None`: no tenant).
    fn sign(&self, identity: &Identity, tenant: Option<TenantId>) -> String {
        match tenant {
            Some(t) => self.signer.sign_for_tenant(Service::Orchestrator.name(), identity, t.as_uuid(), DEFAULT_SID),
            None => self.signer.sign(Service::Orchestrator.name(), identity),
        }
        .unwrap()
    }

    /// Sends a request with this identity, in the tenant labelled `org_id`.
    #[allow(clippy::disallowed_methods, clippy::disallowed_types)] // test code: a plain client against the test app
    async fn send_identity(&self, identity: &Identity, org_id: &str, method: Method, path: &str) -> reqwest::Response {
        let tenant = self.state.metadata.tenants_by_org.lock().unwrap().get(org_id).copied();
        let token = self.sign(identity, tenant);
        reqwest::Client::new()
            .request(method, format!("{}{path}", self.base))
            .header(IDENTITY_HEADER, token)
            .send()
            .await
            .unwrap()
    }

    /// Sends a request as person `sub` in `org_id`, with an optional JSON body.
    #[allow(clippy::disallowed_methods, clippy::disallowed_types)] // test code: a plain client against the test app
    async fn send_as(&self, sub: &str, org_id: &str, method: Method, path: &str, body: Option<Value>) -> reqwest::Response {
        let mut req = reqwest::Client::new()
            .request(method, format!("{}{path}", self.base))
            .header(IDENTITY_HEADER, self.token_for(sub, org_id));
        if let Some(body) = body {
            req = req.json(&body);
        }
        req.send().await.unwrap()
    }
}

/// The identity of person `sub`, with an email and name claim.
fn identity_of(sub: &str) -> Identity {
    Identity {
        sub: sub.into(),
        auth: AuthMethod::ManagementJwt,
        request_id: "test-rid".into(),
        data: None,
        email: Some(format!("{sub}@example.test")),
        name: Some(sub.into()),
    }
}

/// Boots the real orchestrator router (fake metadata/jobs, fresh internal
/// keypair) on an ephemeral port.
async fn spawn_app(metadata: FakeMetadata, jobs: FakeJobs) -> TestApp {
    // Nothing listens on the discard port: rest is "down".
    spawn_app_with_rest(metadata, jobs, "http://127.0.0.1:9").await
}

const REST_TOKEN: &str = "rest-internal-token";
/// m9: storage's console bearer, and where the next app built on this thread sends Studio's
/// storage routes ([`use_storage`]; nothing listens by default).
const STORAGE_TOKEN: &str = "storage-internal-token";
thread_local! {
    static STORAGE_URL: std::cell::RefCell<String> = std::cell::RefCell::new("http://127.0.0.1:9".into());
}
fn storage_url() -> String {
    STORAGE_URL.with(|u| u.borrow().clone())
}
#[allow(dead_code)]
fn use_storage(url: &str) {
    STORAGE_URL.with(|u| *u.borrow_mut() = url.to_owned());
}

async fn spawn_app_with_rest(metadata: FakeMetadata, jobs: FakeJobs, rest_url: &str) -> TestApp {
    spawn_app_in_mode(metadata, jobs, rest_url, Mode::SelfHosted).await
}

async fn spawn_app_in_mode(metadata: FakeMetadata, jobs: FakeJobs, rest_url: &str, mode: Mode) -> TestApp {
    spawn_app_full(metadata, jobs, rest_url, mode, 10).await
}

async fn spawn_app_full(metadata: FakeMetadata, jobs: FakeJobs, rest_url: &str, mode: Mode, cap: u32) -> TestApp {
    spawn_app_parts(
        metadata,
        jobs,
        rest_url,
        mode,
        cap,
        Arc::new(chelabase_query::app_users::PgUsersDb::new()),
    )
    .await
}

async fn spawn_app_parts(
    metadata: FakeMetadata,
    jobs: FakeJobs,
    rest_url: &str,
    mode: Mode,
    cap: u32,
    users_db: Arc<dyn chelabase_query::app_users::UsersDb>,
) -> TestApp {
    spawn_app_custom(metadata, jobs, rest_url, mode, cap, users_db).await
}

/// The path of a probe route that only passes the `Instance` access check (no instance route
/// exists yet; T7 adds them).
const INSTANCE_PROBE: &str = "/test/instance-probe";

async fn instance_probe(principal: orchestrator::InstancePrincipal) -> String {
    principal.sub
}

/// Like [`spawn_app_parts`], with the instance probe route.
async fn spawn_app_custom(
    metadata: FakeMetadata,
    jobs: FakeJobs,
    rest_url: &str,
    mode: Mode,
    cap: u32,
    users_db: Arc<dyn chelabase_query::app_users::UsersDb>,
) -> TestApp {
    spawn_app_with_compute_tls(metadata, jobs, rest_url, mode, cap, users_db, None).await
}

/// Like [`spawn_app_custom`], with the orchestrator's compute client pair (`None`: plain
/// connections, as in every other test).
#[allow(clippy::too_many_arguments)]
async fn spawn_app_with_compute_tls(
    metadata: FakeMetadata,
    jobs: FakeJobs,
    rest_url: &str,
    mode: Mode,
    cap: u32,
    users_db: Arc<dyn chelabase_query::app_users::UsersDb>,
    compute_tls: Option<Arc<orchestrator::compute_tls::ComputeTls>>,
) -> TestApp {
    spawn_app_core(metadata, jobs, rest_url, mode, cap, users_db, compute_tls, None).await
}

/// An app whose app-user admin routes call `identity_service` (the fake's client, or `None`),
/// with the users-table writer and the compute client pair given.
#[allow(dead_code)]
async fn spawn_app_identity(
    metadata: FakeMetadata,
    jobs: FakeJobs,
    identity_service: Option<chelabase_identity::Identity>,
    users_db: Arc<dyn chelabase_query::app_users::UsersDb>,
    compute_tls: Option<Arc<orchestrator::compute_tls::ComputeTls>>,
) -> TestApp {
    spawn_app_core(
        metadata,
        jobs,
        "http://127.0.0.1:9",
        Mode::SelfHosted,
        10,
        users_db,
        compute_tls,
        identity_service,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn spawn_app_core(
    metadata: FakeMetadata,
    jobs: FakeJobs,
    rest_url: &str,
    mode: Mode,
    cap: u32,
    users_db: Arc<dyn chelabase_query::app_users::UsersDb>,
    compute_tls: Option<Arc<orchestrator::compute_tls::ComputeTls>>,
    identity_service: Option<chelabase_identity::Identity>,
) -> TestApp {
    spawn_app_core_with(metadata, jobs, rest_url, mode, cap, users_db, compute_tls, None, None, identity_service, None)
        .await
}

/// #119: an app whose realtime routes take member tokens checked by `members`.
#[allow(dead_code)]
async fn spawn_app_member_tokens(
    metadata: FakeMetadata,
    mode: Mode,
    members: chelabase_auth::member_token::MemberTokenVerifier,
) -> TestApp {
    spawn_app_member_tokens_with(metadata, FakeJobs::default(), "http://127.0.0.1:9", mode, members).await
}

/// [`spawn_app_member_tokens`] with the jobs and rest given.
#[allow(dead_code)]
async fn spawn_app_member_tokens_with(
    metadata: FakeMetadata,
    jobs: FakeJobs,
    rest_url: &str,
    mode: Mode,
    members: chelabase_auth::member_token::MemberTokenVerifier,
) -> TestApp {
    spawn_app_core_with(
        metadata,
        jobs,
        rest_url,
        mode,
        10,
        Arc::new(chelabase_query::app_users::PgUsersDb::new()),
        None,
        None,
        Some(members),
        None,
        None,
    )
    .await
}

/// #119: an app whose platform operator tenant (`CHELABASE_OPERATOR_TENANT_ID`) is `tenant`.
#[allow(dead_code)]
async fn spawn_app_operator_tenant(metadata: FakeMetadata, mode: Mode, tenant: Option<TenantId>) -> TestApp {
    spawn_app_core_with(
        metadata,
        FakeJobs::default(),
        "http://127.0.0.1:9",
        mode,
        10,
        Arc::new(chelabase_query::app_users::PgUsersDb::new()),
        None,
        tenant,
        None,
        None,
        None,
    )
    .await
}

/// m18: the webhook URL policy tests use: `https` only, a fake resolver (`hooks.example.test`
/// is public, `private.example.test` resolves to 10.0.0.7, `inside.example.test` too but is
/// allowlisted; anything else doesn't resolve).
fn webhook_outbound() -> chelabase_http::outbound::Outbound {
    struct Dns;
    impl chelabase_http::outbound::HostLookup for Dns {
        fn lookup<'a>(
            &'a self,
            host: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<Vec<std::net::IpAddr>>> + Send + 'a>> {
            let out = match host {
                "hooks.example.test" => Ok(vec!["93.184.216.34".parse().unwrap()]),
                "private.example.test" | "inside.example.test" => Ok(vec!["10.0.0.7".parse().unwrap()]),
                _ => Err(std::io::Error::other("no such host")),
            };
            Box::pin(std::future::ready(out))
        }
    }
    chelabase_http::outbound::Outbound::new()
        .allow_hosts(["inside.example.test"])
        .with_lookup(Arc::new(Dns))
}

#[allow(clippy::too_many_arguments)]
async fn spawn_app_core_with(
    metadata: FakeMetadata,
    jobs: FakeJobs,
    rest_url: &str,
    mode: Mode,
    cap: u32,
    users_db: Arc<dyn chelabase_query::app_users::UsersDb>,
    compute_tls: Option<Arc<orchestrator::compute_tls::ComputeTls>>,
    operator_tenant: Option<TenantId>,
    member_tokens: Option<chelabase_auth::member_token::MemberTokenVerifier>,
    identity_service: Option<chelabase_identity::Identity>,
    member_sessions: Option<Arc<orchestrator::member_sessions::MemberSessions>>,
) -> TestApp {
    let rt_events = FakeRtEvents::default();
    // Members hub T8: the app's `proxy_cache` (P4; the same `Arc` as the internal state's in
    // `main.rs`).
    let cache = FakeCache::default();
    let (private_pem, public_pem) = generate_keypair_pem();
    let key = SecretKey::generate();
    let locator = FakeLocator::default();
    let bundles = FakeBundles::default();
    let state = Arc::new(AppState {
        metadata,
        js: jobs,
        identity: InternalVerifier::from_pem(public_pem.as_bytes()).unwrap(),
        secrets: key.clone(),
        proxy: ProxyAddress::local("localtest.me"),
        rest: RestInternal {
            url: rest_url.into(),
            token: REST_TOKEN.into(),
            // The production client (no redirects), so tests see what production sees.
            http: chelabase_http::client_builder().build().unwrap(),
        },
        storage: orchestrator::storage_console::StorageInternal::new(
            &storage_url(),
            STORAGE_TOKEN,
        )
        .unwrap(),
        policy: chelabase_types::mode_policy::ModePolicy::new(mode),
        operator_tenant,
        proxy_ca_file: ".chelabase/neon/proxy-ca.crt".into(),
        data_api_database_cap: cap,
        locator: Arc::new(locator.clone()),
        console: Console::default(),
        member_sessions: member_sessions
            .unwrap_or_else(|| Arc::new(orchestrator::member_sessions::MemberSessions::assume_live())),
        identity_service,
        users_db,
        sql_editor: Default::default(),
        table_api_dry_run_max_bytes: orchestrator::DEFAULT_TABLE_API_DRY_RUN_MAX_BYTES,
        studio_url: STUDIO_URL.into(),
        compute_tls,
        realtime: orchestrator::internal::realtime::RealtimeAuth {
            token: REALTIME_TOKEN.into(),
            member: member_tokens,
        },
        rt_events: Arc::new(rt_events.clone()),
        proxy_cache: Arc::new(cache.clone()),
        webhooks: webhook_outbound(),
        // m9: the same resolver, http allowed so the hard deny is what refuses a stack host.
        services: webhook_outbound().allow_http(true).deny_stack([]),
        bundles: Arc::new(bundles.clone()),
    });
    let probe: Router = Router::new()
        .route(INSTANCE_PROBE, axum::routing::get(instance_probe))
        .with_state(state.clone());
    let app: Router = build_router(state.clone()).merge(probe);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    TestApp {
        base: format!("http://{addr}"),
        signer: InternalSigner::from_pem(private_pem.as_bytes()).unwrap(),
        key,
        locator,
        state,
        rt_events,
        cache,
        bundles,
    }
}

fn create_body(tenant_id: TenantId) -> serde_json::Value {
    serde_json::json!({
        "tenant_id": tenant_id,
        "project_id": ProjectId::new(),
        "name": "demo",
    })
}

#[allow(clippy::disallowed_methods, clippy::disallowed_types)] // test code: a plain client against the test app
async fn post_create(app: &TestApp, identity_header: Option<&str>, body: serde_json::Value) -> reqwest::Response {
    let mut req = reqwest::Client::new()
        .post(format!("{}/v1/databases", app.base))
        .json(&body);
    if let Some(token) = identity_header {
        req = req.header(IDENTITY_HEADER, token);
    }
    req.send().await.unwrap()
}

/// A merge row as the fake stores it, for tests that start from a given state.
fn merge_row(db: DatabaseId, source: BranchId, target: BranchId, status: MergeStatus) -> MergeRecord {
    let now = chrono::Utc::now();
    MergeRecord {
        id: uuid::Uuid::new_v4(),
        database_id: db.as_uuid(),
        source_branch_id: source.as_uuid(),
        target_branch_id: target.as_uuid(),
        direction: chelabase_metadata::MergeDirection::IntoParent,
        database_name: "app".into(),
        status,
        failed_phase: None,
        version: 3,
        base_fingerprint: None,
        source_fingerprint: None,
        target_fingerprint: None,
        analysis: None,
        resolutions: json!([]),
        user_steps: json!([]),
        acknowledgements: json!([]),
        options: json!({"lock_timeout_ms": 5000, "after": "keep"}),
        plan: None,
        rehearsal: None,
        result: None,
        rebaselined: false,
        error: None,
        claimed_at: None,
        created_by: "user-1".into(),
        created_at: now,
        updated_at: now,
    }
}

/// A tenant bound to org-1 (its tenant admin is the default caller), a project the fake
/// knows, a ready database in it (`web1`, owner `postgres`) with an unprotected ready `dev`
/// branch, and a draft merge of `dev` into main.
#[allow(dead_code)]
struct MemberWorld {
    app: TestApp,
    metadata: FakeMetadata,
    tenant: TenantId,
    project: ProjectId,
    db: DatabaseId,
    dev: BranchId,
    merge: uuid::Uuid,
    jobs: FakeJobs,
}

async fn member_world(mode: Mode) -> MemberWorld {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app_in_mode(metadata.clone(), jobs.clone(), "http://127.0.0.1:9", mode).await;
    let project = metadata.create_project(tenant, "Billing").await.unwrap().id;
    let mut db = sample_database(tenant);
    db.project_id = project;
    db.name = "web1".into();
    db.database_name = Some("web1".into());
    db.owner_role = Some("postgres".into());
    db.main.owner_password_enc =
        Some(seal_credential(&app.key, &db.main.id.to_string(), Purpose::Owner, "s3cret"));
    let (db_id, main) = (db.database_id, db.main.id);
    metadata.insert_database(db);
    let dev = metadata.add_branch(db_id, "dev", main, BranchKind::Normal, BranchStatus::Ready);
    let merge = merge_row(db_id, dev, main, MergeStatus::Draft);
    let merge_id = merge.id;
    metadata.merges.lock().unwrap().push(merge);
    MemberWorld { app, metadata, tenant, project, db: db_id, dev, merge: merge_id, jobs }
}

// --- T7: the member API's fake metadata and reconcile queue ------------------------------------

/// The Studio URL the test app builds invite links from.
const STUDIO_URL: &str = "http://studio.test";

impl orchestrator::members::MemberRolesQueue for FakeJobs {
    async fn enqueue_member_roles(&self, branch: BranchId) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.member_role_enqueues.lock().unwrap().push(branch);
        Ok(())
    }
}

type FakeWrite<T> = Result<T, chelabase_metadata::members::AccessWriteError>;

/// As the real query: the event carries the project, or its before/after lists a grant on it.
fn event_names_project(e: &chelabase_metadata::members::AccessEvent, project: uuid::Uuid) -> bool {
    let lists = |v: &Option<Value>| {
        v.as_ref()
            .and_then(|v| v["grants"].as_array().cloned())
            .unwrap_or_default()
            .iter()
            .any(|g| g["project_id"] == json!(project))
    };
    e.project_id == Some(project) || lists(&e.before) || lists(&e.after)
}

/// An access event the fake records, built like the store's `NewEvent`.
struct Change {
    tenant: TenantId,
    actor: Option<uuid::Uuid>,
    action: &'static str,
    target: Option<uuid::Uuid>,
    project: Option<uuid::Uuid>,
    database: Option<uuid::Uuid>,
    before: Option<Value>,
    after: Option<Value>,
}

impl Change {
    fn new(tenant: TenantId, actor: Option<uuid::Uuid>, action: &'static str, target: Option<uuid::Uuid>) -> Self {
        Self { tenant, actor, action, target, project: None, database: None, before: None, after: None }
    }

    fn scope(self, project: Option<uuid::Uuid>, database: Option<uuid::Uuid>) -> Self {
        Self { project, database, ..self }
    }

    fn before(self, before: impl Into<Option<Value>>) -> Self {
        Self { before: before.into(), ..self }
    }

    fn after(self, after: impl Into<Option<Value>>) -> Self {
        Self { after: after.into(), ..self }
    }
}

/// The only grant's project, when there is exactly one.
fn single_project(grants: &[(uuid::Uuid, GrantRow)]) -> Option<uuid::Uuid> {
    match grants {
        [(p, _)] => Some(*p),
        _ => None,
    }
}

fn grants_json(grants: &[(uuid::Uuid, GrantRow)]) -> Vec<Value> {
    grants
        .iter()
        .map(|(p, g)| json!({ "project_id": p, "platform_role": g.platform_role.as_str(), "pg_roles": g.pg_roles }))
        .collect()
}

impl FakeMetadata {
    /// Appends an access event (the real writes log one per change).
    /// A write another request makes just before the next checked write compares its
    /// `Observed` (I4 tests).
    fn race_once(&self, write: impl FnOnce(&FakeMetadata) + Send + 'static) {
        self.race_before_write.lock().unwrap().push_back(Box::new(write));
    }

    /// What the real checked writes do under the member's lock: run the pending race, then
    /// `Changed` unless the member's access is still what each `expect` read.
    fn settle(&self, member: uuid::Uuid, expect: &[Observed]) -> FakeWrite<()> {
        use chelabase_metadata::members::AccessWriteError;
        let race = self.race_before_write.lock().unwrap().pop_front();
        if let Some(write) = race {
            write(self);
        }
        for e in expect {
            if self.observed_now(member, e.project) != *e {
                return Err(AccessWriteError::Changed);
            }
        }
        Ok(())
    }

    /// What `observe_tx` reads now: the member's own grant on `project`, its overrides on the
    /// project's databases and its teams' grants there (T23, P7).
    fn observed_now(&self, member: uuid::Uuid, project: uuid::Uuid) -> Observed {
        let databases: Vec<uuid::Uuid> = self
            .databases
            .lock()
            .unwrap()
            .values()
            .filter(|d| d.project_id.as_uuid() == project)
            .map(|d| d.database_id.as_uuid())
            .collect();
        Observed {
            project,
            grant: self.grants.lock().unwrap().get(&(member, project)).cloned(),
            overrides: self
                .overrides
                .lock()
                .unwrap()
                .iter()
                .filter(|((m, d), _)| *m == member && databases.contains(d))
                .map(|((_, d), o)| (*d, o.clone()))
                .collect(),
            teams: self.stored_team_grants(member, project),
        }
    }

    /// `revoke_invite`, with the reason the event carries (`delivery_failed`).
    fn revoke_with(&self, actor: uuid::Uuid, invite: uuid::Uuid, reason: Option<&str>) -> FakeWrite<String> {
        use chelabase_metadata::members::AccessWriteError;
        let (member, zid) = {
            let invites = self.invites.lock().unwrap();
            let i = invites.iter().find(|i| i.id == invite).ok_or(AccessWriteError::NotFound)?;
            if i.revoked || i.accepted {
                return Err(AccessWriteError::Conflict("the invite is no longer open".into()));
            }
            (i.member_id, i.identity_user_id.clone())
        };
        let m = self.members.lock().unwrap().iter().find(|m| m.id == member).cloned().unwrap();
        if m.status != MemberStatus::Invited {
            return Err(AccessWriteError::Conflict("the member is no longer invited".into()));
        }
        for i in self.invites.lock().unwrap().iter_mut().filter(|i| i.member_id == member && !i.accepted) {
            i.revoked = true;
        }
        self.grants.lock().unwrap().retain(|(m, _), _| *m != member);
        self.overrides.lock().unwrap().retain(|(m, _), _| *m != member);
        self.members.lock().unwrap().iter_mut().find(|x| x.id == member).unwrap().status = MemberStatus::Removed;
        let after = reason.map(|r| json!({ "status": "removed", "reason": r }));
        self.log(Change::new(m.tenant_id, Some(actor), "invite_revoked", Some(member)).after(after));
        Ok(zid)
    }

    fn log(&self, change: Change) {
        let Change { tenant, actor, action, target, project, database, before, after } = change;
        let mut events = self.events.lock().unwrap();
        let id = events.len() as i64 + 1;
        events.push(chelabase_metadata::members::AccessEvent {
            id,
            tenant_id: tenant,
            actor_member_id: actor,
            action: action.into(),
            target_member_id: target,
            project_id: project,
            database_id: database,
            before,
            after,
            at: chrono::Utc::now(),
            actor_email: None,
            target_email: None,
            project_name: None,
            branch_id: None,
        });
    }

    /// A member as the list shows it: `has_access` is the admin flag or a project grant.
    fn listing(&self, m: MemberRecord) -> chelabase_metadata::members::MemberListing {
        // As the real query: a grant on a soft-deleted project is no access.
        let live: HashSet<uuid::Uuid> = self
            .projects
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.status != ProjectStatus::Deleted)
            .map(|p| p.id.as_uuid())
            .collect();
        let has_access = m.tenant_admin
            || self.grants.lock().unwrap().keys().any(|(member, p)| *member == m.id && live.contains(p))
            // T23 (P4): a team grant counts as the shared shape reads it.
            || live.iter().any(|p| !self.stored_team_grants(m.id, *p).is_empty());
        chelabase_metadata::members::MemberListing { created_at: self.created_at(m.id), has_access, member: m }
    }

    /// Stamps the row's creation time once, strictly after every earlier stamp.
    fn stamp_created(&self, id: uuid::Uuid) -> chrono::DateTime<chrono::Utc> {
        let mut created = self.member_created.lock().unwrap();
        let latest = created.values().max().copied();
        let mut now = chrono::Utc::now();
        if latest.is_some_and(|l| l >= now) {
            now = latest.unwrap() + chrono::Duration::microseconds(1);
        }
        *created.entry(id).or_insert(now)
    }

    fn created_at(&self, id: uuid::Uuid) -> chrono::DateTime<chrono::Utc> {
        self.stamp_created(id)
    }

    /// An event with the names the real query joins.
    fn named(&self, mut e: chelabase_metadata::members::AccessEvent) -> chelabase_metadata::members::AccessEvent {
        let members = self.members.lock().unwrap();
        let email = |id: Option<uuid::Uuid>| id.and_then(|id| members.iter().find(|m| m.id == id)).and_then(|m| m.email.clone());
        e.actor_email = email(e.actor_member_id);
        e.target_email = email(e.target_member_id);
        e.project_name = e
            .project_id
            .and_then(|p| self.projects.lock().unwrap().iter().find(|r| r.id.as_uuid() == p).map(|r| r.name.clone()));
        e
    }

    /// A live (not removed) member by id.
    fn live_member(&self, id: uuid::Uuid) -> Option<MemberRecord> {
        self.members.lock().unwrap().iter().find(|m| m.id == id && m.status != MemberStatus::Removed).cloned()
    }

    /// As the real `lock_writable_member`: missing or removed is `NotFound`, pending `Conflict`.
    fn writable_member(&self, id: uuid::Uuid) -> Result<MemberRecord, chelabase_metadata::members::AccessWriteError> {
        let m = self.live_member(id).ok_or(chelabase_metadata::members::AccessWriteError::NotFound)?;
        if m.status == MemberStatus::Pending {
            return Err(chelabase_metadata::members::AccessWriteError::Conflict("the member's sign-up is waiting for approval: approve it first".into()));
        }
        Ok(m)
    }

    fn project_active_in(&self, tenant: TenantId, project: uuid::Uuid) -> bool {
        self.projects
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.id.as_uuid() == project && p.tenant_id == tenant && p.status == ProjectStatus::Active)
    }

    /// Inserts a new member's grants, as `insert_grants` does.
    fn insert_member_grants(&self, tenant: TenantId, member: uuid::Uuid, grants: &[(uuid::Uuid, GrantRow)]) -> FakeWrite<()> {
        use chelabase_metadata::members::AccessWriteError;
        for (project, _) in grants {
            if !self.project_active_in(tenant, *project) {
                return Err(AccessWriteError::NotFound);
            }
        }
        let mut seen = HashSet::new();
        if !grants.iter().all(|(p, _)| seen.insert(*p)) {
            return Err(AccessWriteError::Conflict("a project twice".into()));
        }
        let mut all = self.grants.lock().unwrap();
        all.retain(|(m, _), _| *m != member);
        for (project, g) in grants {
            let mut roles = g.pg_roles.clone();
            roles.sort();
            roles.dedup();
            all.insert((member, *project), GrantRow { platform_role: g.platform_role, pg_roles: roles });
        }
        Ok(())
    }

    fn other_active_admins(&self, tenant: TenantId, member: uuid::Uuid) -> usize {
        self.members
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.tenant_id == tenant && m.id != member && m.status == MemberStatus::Active && m.tenant_admin)
            .count()
    }

    /// The database's project when it belongs to `tenant` and isn't being deleted.
    fn database_project(&self, tenant: TenantId, database: uuid::Uuid) -> Option<uuid::Uuid> {
        self.databases
            .lock()
            .unwrap()
            .get(&DatabaseId(database))
            .filter(|d| d.tenant_id == tenant && !matches!(d.status, DatabaseStatus::Deleting | DatabaseStatus::Deleted))
            .map(|d| d.project_id.as_uuid())
    }
}

/// Members hub T8: the personal-login side of the fake, as T3's metadata functions behave.
impl FakeMetadata {
    /// `chelabase_member_level`: an active member's level on a database of its tenant that
    /// isn't deleted (a tenant admin's is admin; an override narrows the grant).
    fn login_level(&self, member: uuid::Uuid, database: uuid::Uuid) -> Option<PlatformRole> {
        let m = self.members.lock().unwrap().iter().find(|m| m.id == member).cloned()?;
        if m.status != MemberStatus::Active {
            return None;
        }
        let db = self.databases.lock().unwrap().get(&DatabaseId(database)).cloned()?;
        if db.status == DatabaseStatus::Deleted || db.tenant_id != m.tenant_id {
            return None;
        }
        if m.tenant_admin {
            return Some(PlatformRole::Admin);
        }
        // P5 (T23): the best level among the own grant and the member's teams' grants.
        let project = db.project_id.as_uuid();
        let own = self.grants.lock().unwrap().get(&(member, project)).map(|g| g.platform_role);
        let granted = own.into_iter().chain(self.stored_team_grants(member, project).values().map(|g| g.platform_role)).max()?;
        let narrowed =self.overrides.lock().unwrap().get(&(member, database)).and_then(|o| o.platform_role);
        Some(narrowed.map_or(granted, |o| o.min(granted)))
    }

    /// The record as the API lists it, its status judged now.
    fn login_view(l: &FakeLogin) -> chelabase_metadata::personal_logins::PersonalLogin {
        use chelabase_metadata::personal_logins::LoginStatus;
        let mut login = l.login.clone();
        login.status = if login.revoked_at.is_some() {
            LoginStatus::Revoked
        } else if login.valid_until <= chrono::Utc::now() {
            LoginStatus::Expired
        } else {
            LoginStatus::Active
        };
        login
    }

    /// Removes a record's proxy rows from the role store (the FK's cascade).
    fn drop_login_rows(&self, l: &FakeLogin) {
        let mut roles = self.roles.lock().unwrap();
        for e in &l.endpoints {
            if let Some(rows) = roles.get_mut(&e.endpoint_id) {
                rows.retain(|r| !(r.name == e.role_name && r.origin == RoleOrigin::Personal));
            }
        }
    }

    /// Every live login of `member` revoked with `reason`; returns how many.
    fn revoke_member_logins(&self, member: uuid::Uuid, reason: chelabase_metadata::personal_logins::RevokeReason) -> usize {
        use chelabase_metadata::personal_logins::LoginStatus;
        let mut n = 0;
        for l in self.personal_logins.lock().unwrap().iter_mut().filter(|l| l.login.member_id == member && l.login.revoked_at.is_none()) {
            l.login.revoked_at = Some(chrono::Utc::now());
            l.login.revoked_reason = Some(reason);
            l.login.status = LoginStatus::Revoked;
            l.verifier = None;
            n += 1;
        }
        n
    }

    /// S2: the member's `cb_m_*` rows marked for rotation (`password_enc = ''`).
    fn mark_member_roles_for_rotation(&self, member: uuid::Uuid) -> Vec<uuid::Uuid> {
        let mut branches = Vec::new();
        for r in self.member_roles.lock().unwrap().iter_mut().filter(|r| r.member_id == member) {
            r.password_enc.clear();
            branches.push(r.branch_id);
        }
        branches
    }

    /// The projects a member reaches, as the metadata's `touched_projects`: its grants, the
    /// projects of its overrides, of the branches holding its member roles and of its logins.
    fn login_touched_projects(&self, member: uuid::Uuid) -> Vec<uuid::Uuid> {
        let mut out: Vec<uuid::Uuid> = self.grants.lock().unwrap().keys().filter(|(m, _)| *m == member).map(|(_, p)| *p).collect();
        // T23: its teams' grant projects too.
        out.extend(self.member_team_projects(member));
        let mut databases: Vec<uuid::Uuid> = self.overrides.lock().unwrap().keys().filter(|(m, _)| *m == member).map(|(_, d)| *d).collect();
        databases.extend(self.personal_logins.lock().unwrap().iter().filter(|l| l.login.member_id == member).map(|l| l.login.database_id));
        let mut branches: Vec<BranchId> = self.member_branch_roles.lock().unwrap().iter().filter(|(m, _)| *m == member).map(|(_, b)| *b).collect();
        branches.extend(self.member_roles.lock().unwrap().iter().filter(|r| r.member_id == member).map(|r| BranchId(r.branch_id)));
        for b in branches {
            if let Some(b) = self.branches.lock().unwrap().get(&b) {
                databases.push(b.database_id.as_uuid());
            }
        }
        for d in databases {
            if let Some(db) = self.databases.lock().unwrap().get(&DatabaseId(d)) {
                out.push(db.project_id.as_uuid());
            }
        }
        out.sort();
        out.dedup();
        out
    }
}

impl orchestrator::members::MembersPort for FakeMetadata {
    async fn member(&self, member: uuid::Uuid) -> Result<Option<MemberRecord>, MetadataError> {
        Ok(self.members.lock().unwrap().iter().find(|m| m.id == member).cloned())
    }

    async fn member_by_email(&self, tenant: TenantId, email: &str) -> Result<Option<MemberRecord>, MetadataError> {
        let email = email.trim().to_lowercase();
        if email.is_empty() {
            return Ok(None);
        }
        Ok(self
            .members
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.tenant_id == tenant && m.status != MemberStatus::Removed && m.email.as_deref() == Some(email.as_str()))
            .cloned())
    }

    async fn list_members(&self, tenant: TenantId) -> Result<Vec<chelabase_metadata::members::MemberListing>, MetadataError> {
        let mut out: Vec<MemberRecord> = self
            .members
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.tenant_id == tenant && m.status != MemberStatus::Removed)
            .cloned()
            .collect();
        out.sort_by_key(|m| (m.email.clone(), m.id));
        Ok(out.into_iter().map(|m| self.listing(m)).collect())
    }

    async fn member_listing(&self, member: uuid::Uuid) -> Result<Option<chelabase_metadata::members::MemberListing>, MetadataError> {
        let m = self.members.lock().unwrap().iter().find(|m| m.id == member).cloned();
        Ok(m.map(|m| self.listing(m)))
    }

    async fn open_invites(&self, tenant: TenantId) -> Result<Vec<chelabase_metadata::members::OpenInvite>, MetadataError> {
        let members: HashSet<uuid::Uuid> =
            self.members.lock().unwrap().iter().filter(|m| m.tenant_id == tenant).map(|m| m.id).collect();
        Ok(self
            .invites
            .lock()
            .unwrap()
            .iter()
            .rev()
            .filter(|i| members.contains(&i.member_id) && !i.revoked && !i.accepted)
            .map(|i| chelabase_metadata::members::OpenInvite { id: i.id, member_id: i.member_id, expires_at: i.expires_at })
            .collect())
    }

    async fn invite(
        &self,
        invite: uuid::Uuid,
    ) -> Result<Option<chelabase_metadata::members::InviteState>, MetadataError> {
        let now = chrono::Utc::now();
        Ok(self
            .invites
            .lock()
            .unwrap()
            .iter()
            .find(|i| i.id == invite)
            .map(|i| (i.member_id, i.expires_at, i.revoked.then_some(now), i.accepted.then_some(now))))
    }

    async fn project_access(&self, project: uuid::Uuid) -> Result<chelabase_metadata::members::ProjectAccess, MetadataError> {
        let mut grants: Vec<(uuid::Uuid, GrantRow)> = self
            .grants
            .lock()
            .unwrap()
            .iter()
            .filter(|((_, p), _)| *p == project)
            .map(|((m, _), g)| (*m, g.clone()))
            .collect();
        grants.sort_by_key(|(m, _)| *m);
        let databases: HashSet<uuid::Uuid> = self
            .databases
            .lock()
            .unwrap()
            .values()
            .filter(|d| d.project_id.as_uuid() == project)
            .map(|d| d.database_id.as_uuid())
            .collect();
        let mut overrides: Vec<(uuid::Uuid, uuid::Uuid, OverrideRow)> = self
            .overrides
            .lock()
            .unwrap()
            .iter()
            .filter(|((_, d), _)| databases.contains(d))
            .map(|((m, d), o)| (*m, *d, o.clone()))
            .collect();
        overrides.sort_by_key(|(m, d, _)| (*m, *d));
        // T23: every member of every team with a grant on the project, by team then member.
        let mut teams: Vec<(uuid::Uuid, uuid::Uuid, GrantRow)> = Vec::new();
        let members: Vec<uuid::Uuid> = self.members.lock().unwrap().iter().map(|m| m.id).collect();
        for m in members {
            for (team, g) in self.stored_team_grants(m, project) {
                teams.push((team, m, g));
            }
        }
        teams.sort_by_key(|(t, m, _)| (*t, *m));
        Ok(chelabase_metadata::members::ProjectAccess { grants, overrides, teams })
    }

    async fn project_branches(&self, project: uuid::Uuid) -> Result<Vec<uuid::Uuid>, MetadataError> {
        let databases: HashSet<DatabaseId> = self
            .databases
            .lock()
            .unwrap()
            .values()
            .filter(|d| d.project_id.as_uuid() == project && !matches!(d.status, DatabaseStatus::Deleting | DatabaseStatus::Deleted))
            .map(|d| d.database_id)
            .collect();
        let mut out: Vec<uuid::Uuid> = self
            .branches
            .lock()
            .unwrap()
            .values()
            .filter(|b| databases.contains(&b.database_id) && !matches!(b.status, BranchStatus::Deleting | BranchStatus::Deleted))
            .map(|b| b.id.as_uuid())
            .collect();
        out.sort();
        Ok(out)
    }

    async fn open_signup(&self, tenant: TenantId) -> Result<bool, MetadataError> {
        Ok(self.open_signups.lock().unwrap().get(&tenant).copied().unwrap_or(false))
    }

    async fn admin_claimed_since(&self, tenant: TenantId, since: chrono::DateTime<chrono::Utc>) -> Result<bool, MetadataError> {
        // As the real one: only the bound user's first request (`admin_claimed`) opens the
        // window; binding the claim doesn't (m3 re-audit, B3).
        Ok(self.admin_claims.lock().unwrap().get(&tenant).is_some_and(|at| *at > since))
    }

    async fn access_events(
        &self,
        tenant: TenantId,
        project: Option<uuid::Uuid>,
        member: Option<uuid::Uuid>,
        before_id: Option<i64>,
        limit: u32,
    ) -> Result<Vec<chelabase_metadata::members::AccessEvent>, MetadataError> {
        Ok(self
            .events
            .lock()
            .unwrap()
            .iter()
            .rev()
            .filter(|e| {
                e.tenant_id == tenant
                    && project.is_none_or(|p| event_names_project(e, p))
                    && member.is_none_or(|m| {
                        // P9, P38 (T1's filter): a team event that reached the member counts.
                        let reached = |v: &Option<Value>| {
                            v.as_ref().is_some_and(|v| {
                                v["affected_member_ids"].as_array().is_some_and(|ids| ids.contains(&json!(m.to_string())))
                            })
                        };
                        e.target_member_id == Some(m) || e.actor_member_id == Some(m) || reached(&e.before) || reached(&e.after)
                    })
                    && before_id.is_none_or(|b| e.id < b)
            })
            .take(limit as usize)
            .cloned()
            .map(|e| self.named(e))
            .collect())
    }

    async fn member_project_roles(&self, tenant: TenantId) -> Result<Vec<chelabase_metadata::personal_logins::MemberProjectRole>, MetadataError> {
        self.fake_member_project_roles(tenant)
    }

    async fn is_personal_login_name(&self, database: uuid::Uuid, name: &str) -> Result<bool, MetadataError> {
        Ok(self.personal_login_names.lock().unwrap().contains(&(database, name.to_string())))
    }

    async fn personal_login_names(&self, project: uuid::Uuid) -> Result<std::collections::BTreeSet<String>, MetadataError> {
        let databases: HashSet<uuid::Uuid> = self
            .databases
            .lock()
            .unwrap()
            .values()
            .filter(|d| d.project_id.as_uuid() == project)
            .map(|d| d.database_id.as_uuid())
            .collect();
        Ok(self
            .personal_login_names
            .lock()
            .unwrap()
            .iter()
            .filter(|(d, _)| databases.contains(d))
            .map(|(_, n)| n.clone())
            .collect())
    }

    async fn create_personal_login(
        &self,
        member: uuid::Uuid,
        database: uuid::Uuid,
        role_name: &str,
        scram_verifier: &str,
        valid_until: chrono::DateTime<chrono::Utc>,
    ) -> Result<chelabase_metadata::personal_logins::PersonalLogin, chelabase_metadata::personal_logins::LoginWriteError> {
        use chelabase_metadata::personal_logins::{LoginStatus, LoginWriteError, PersonalLogin};
        let Some(level) = self.login_level(member, database).filter(|l| *l >= PlatformRole::Developer) else {
            return Err(LoginWriteError::NotQualified);
        };
        let own = self.personal_logins.lock().unwrap().iter().find(|l| l.login.member_id == member && l.login.database_id == database).cloned();
        if own.as_ref().is_some_and(|l| Self::login_view(l).status == LoginStatus::Active) {
            return Err(LoginWriteError::Exists);
        }
        // D3: another login of the database (any status), or any role on a live branch of it,
        // except the replaced record's own rows (S10).
        let other_login = self
            .personal_logins
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.login.database_id == database && l.login.member_id != member && l.login.role_name == role_name);
        let endpoints: Vec<String> = self
            .branches
            .lock()
            .unwrap()
            .values()
            .filter(|b| b.database_id.as_uuid() == database && b.status != BranchStatus::Deleted)
            .filter_map(|b| b.endpoint_id.clone())
            .collect();
        let own_rows: HashSet<String> = own.iter().flat_map(|l| l.endpoints.iter().map(|e| e.endpoint_id.clone())).collect();
        let role_taken = {
            let roles = self.roles.lock().unwrap();
            endpoints.iter().any(|ep| {
                roles.get(ep).is_some_and(|rows| {
                    rows.iter().any(|r| r.name == role_name && !(r.origin == RoleOrigin::Personal && own_rows.contains(ep)))
                })
            })
        };
        if other_login || role_taken {
            return Err(LoginWriteError::NameTaken);
        }
        let record = self.databases.lock().unwrap().get(&DatabaseId(database)).cloned().ok_or(LoginWriteError::NotQualified)?;
        if let Some(old) = &own {
            self.drop_login_rows(old);
            self.personal_logins.lock().unwrap().retain(|l| !(l.login.member_id == member && l.login.database_id == database));
            self.personal_login_names.lock().unwrap().remove(&(database, old.login.role_name.clone()));
        }
        let login = PersonalLogin {
            member_id: member,
            database_id: database,
            project_id: record.project_id.as_uuid(),
            database_name: record.name.clone(),
            role_name: role_name.into(),
            valid_until,
            created_at: chrono::Utc::now(),
            revoked_at: None,
            revoked_reason: None,
            status: LoginStatus::Active,
        };
        self.personal_logins.lock().unwrap().push(FakeLogin {
            login: login.clone(),
            verifier: Some(scram_verifier.into()),
            endpoints: Vec::new(),
        });
        self.personal_login_names.lock().unwrap().insert((database, role_name.to_string()));
        let tenant = record.tenant_id;
        self.log(
            Change::new(tenant, Some(member), "personal_login_created", Some(member))
                .scope(Some(login.project_id), Some(database))
                .after(json!({ "role_name": role_name, "valid_until": valid_until, "level": level.as_str() })),
        );
        Ok(login)
    }

    async fn reset_personal_login(
        &self,
        member: uuid::Uuid,
        database: uuid::Uuid,
        scram_verifier: &str,
        valid_until: chrono::DateTime<chrono::Utc>,
    ) -> Result<
        (chelabase_metadata::personal_logins::PersonalLogin, Vec<chelabase_metadata::personal_logins::LoginEndpoint>),
        chelabase_metadata::personal_logins::LoginWriteError,
    > {
        use chelabase_metadata::personal_logins::{LoginStatus, LoginWriteError};
        let live = self
            .personal_logins
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.login.member_id == member && l.login.database_id == database && l.login.revoked_at.is_none());
        if !live {
            return Err(LoginWriteError::NotFound);
        }
        if !self.login_level(member, database).is_some_and(|l| l >= PlatformRole::Developer) {
            return Err(LoginWriteError::NotQualified);
        }
        let l = {
            let mut logins = self.personal_logins.lock().unwrap();
            let l = logins.iter_mut().find(|l| l.login.member_id == member && l.login.database_id == database).unwrap();
            l.verifier = Some(scram_verifier.into());
            l.login.valid_until = valid_until;
            l.login.status = LoginStatus::Active;
            l.clone()
        };
        let tenant = self.databases.lock().unwrap().get(&DatabaseId(database)).map(|d| d.tenant_id).unwrap();
        self.log(
            Change::new(tenant, Some(member), "personal_login_reset", Some(member))
                .scope(Some(l.login.project_id), Some(database))
                .after(json!({ "role_name": l.login.role_name, "valid_until": valid_until })),
        );
        Ok((Self::login_view(&l), l.endpoints.clone()))
    }

    async fn delete_personal_login(
        &self,
        actor: uuid::Uuid,
        member: uuid::Uuid,
        database: uuid::Uuid,
    ) -> Result<Vec<chelabase_metadata::personal_logins::LoginEndpoint>, chelabase_metadata::personal_logins::LoginWriteError> {
        use chelabase_metadata::personal_logins::LoginWriteError;
        let l = self
            .personal_logins
            .lock()
            .unwrap()
            .iter()
            .find(|l| l.login.member_id == member && l.login.database_id == database)
            .cloned()
            .ok_or(LoginWriteError::NotFound)?;
        self.drop_login_rows(&l);
        self.personal_logins.lock().unwrap().retain(|x| !(x.login.member_id == member && x.login.database_id == database));
        self.personal_login_names.lock().unwrap().remove(&(database, l.login.role_name.clone()));
        let tenant = self.databases.lock().unwrap().get(&DatabaseId(database)).map(|d| d.tenant_id).unwrap();
        self.log(
            Change::new(tenant, Some(actor), "personal_login_deleted", Some(member))
                .scope(Some(l.login.project_id), Some(database))
                .before(json!({ "role_name": l.login.role_name })),
        );
        Ok(l.endpoints)
    }

    async fn member_personal_logins(&self, member: uuid::Uuid) -> Result<Vec<chelabase_metadata::personal_logins::PersonalLogin>, MetadataError> {
        let mut out: Vec<_> = self.personal_logins.lock().unwrap().iter().filter(|l| l.login.member_id == member).map(Self::login_view).collect();
        out.sort_by(|a, b| (&a.database_name, a.database_id).cmp(&(&b.database_name, b.database_id)));
        Ok(out)
    }

    async fn database_personal_logins(&self, database: uuid::Uuid) -> Result<Vec<chelabase_metadata::personal_logins::PersonalLogin>, MetadataError> {
        let mut out: Vec<_> = self.personal_logins.lock().unwrap().iter().filter(|l| l.login.database_id == database).map(Self::login_view).collect();
        out.sort_by(|a, b| a.role_name.cmp(&b.role_name));
        Ok(out)
    }

    async fn revoke_logins_for_identity_user(
        &self,
        tenant: TenantId,
        identity_user_id: &str,
    ) -> Result<Vec<chelabase_metadata::personal_logins::LoginEndpoint>, MetadataError> {
        if *self.fail_offboard.lock().unwrap() {
            return Err(MetadataError::Conflict);
        }
        let Some(member) = self.member(tenant, identity_user_id).map(|m| m.id) else {
            return Ok(Vec::new());
        };
        // As the store: the mark first, then every live login, by the offboarding reason.
        self.members.lock().unwrap().iter_mut().filter(|m| m.id == member).for_each(|m| m.offboarded = true);
        self.revoke_member_logins(member, chelabase_metadata::personal_logins::RevokeReason::IdentityOffboarded);
        orchestrator::members::MembersPort::member_login_endpoints(self, member).await
    }

    async fn clear_offboarded(&self, tenant: TenantId, identity_user_id: &str) -> Result<bool, MetadataError> {
        let mut members = self.members.lock().unwrap();
        let Some(m) = members.iter_mut().find(|m| m.tenant_id == tenant && m.identity_user_id == identity_user_id && m.offboarded) else {
            return Ok(false);
        };
        m.offboarded = false;
        Ok(true)
    }

    async fn member_login_endpoints(&self, member: uuid::Uuid) -> Result<Vec<chelabase_metadata::personal_logins::LoginEndpoint>, MetadataError> {
        let mut out: Vec<_> = self
            .personal_logins
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.login.member_id == member)
            .flat_map(|l| l.endpoints.clone())
            .collect();
        out.sort();
        Ok(out)
    }

    async fn suspend_member(&self, actor: uuid::Uuid, member: uuid::Uuid) -> FakeWrite<Vec<uuid::Uuid>> {
        use chelabase_metadata::members::AccessWriteError;
        use chelabase_metadata::personal_logins::RevokeReason;
        let m = self.members.lock().unwrap().iter().find(|m| m.id == member).cloned().ok_or(AccessWriteError::NotFound)?;
        match m.status {
            MemberStatus::Active => {}
            MemberStatus::Suspended => return Ok(self.login_touched_projects(member)),
            _ => return Err(AccessWriteError::Conflict("only an active member can be suspended".into())),
        }
        if actor == member {
            return Err(AccessWriteError::Conflict("you can't suspend yourself".into()));
        }
        if m.tenant_admin && self.other_active_admins(m.tenant_id, member) == 0 {
            return Err(AccessWriteError::LastTenantAdmin);
        }
        {
            let mut members = self.members.lock().unwrap();
            let x = members.iter_mut().find(|x| x.id == member).unwrap();
            x.status = MemberStatus::Suspended;
            x.suspended_at = Some(chrono::Utc::now());
            x.suspended_by = Some(actor);
        }
        let rotated = self.mark_member_roles_for_rotation(member);
        self.log(
            Change::new(m.tenant_id, Some(actor), "member_suspended", Some(member))
                .before(json!({ "status": "active" }))
                .after(json!({ "status": "suspended", "rotated_branches": rotated })),
        );
        let n = self.revoke_member_logins(member, RevokeReason::Suspended);
        if n > 0 {
            self.log(Change::new(m.tenant_id, Some(actor), "personal_logins_revoked", Some(member)).after(json!({ "count": n, "reason": "suspended" })));
        }
        Ok(self.login_touched_projects(member))
    }

    async fn reinstate_member(&self, actor: uuid::Uuid, member: uuid::Uuid) -> FakeWrite<Vec<uuid::Uuid>> {
        use chelabase_metadata::members::AccessWriteError;
        let m = self.members.lock().unwrap().iter().find(|m| m.id == member).cloned().ok_or(AccessWriteError::NotFound)?;
        // P16: the projects the member reaches through its own grants and its teams.
        let mut reached: Vec<uuid::Uuid> = self
            .grants
            .lock()
            .unwrap()
            .keys()
            .filter(|(x, p)| *x == member && self.project_live_in(m.tenant_id, *p))
            .map(|(_, p)| *p)
            .collect();
        reached.extend(self.member_team_projects(member));
        reached.sort();
        reached.dedup();
        match m.status {
            MemberStatus::Suspended => {}
            MemberStatus::Active => return Ok(reached),
            _ => return Err(AccessWriteError::Conflict("only a suspended member can be reinstated".into())),
        }
        {
            let mut members = self.members.lock().unwrap();
            let x = members.iter_mut().find(|x| x.id == member).unwrap();
            x.status = MemberStatus::Active;
            x.suspended_at = None;
            x.suspended_by = None;
        }
        self.log(
            Change::new(m.tenant_id, Some(actor), "member_reinstated", Some(member))
                .before(json!({ "status": "suspended" }))
                .after(json!({ "status": "active" })),
        );
        Ok(reached)
    }

    async fn revoke_postgres_access(&self, actor: uuid::Uuid, member: uuid::Uuid) -> FakeWrite<Vec<uuid::Uuid>> {
        use chelabase_metadata::members::AccessWriteError;
        use chelabase_metadata::personal_logins::RevokeReason;
        let m = self.live_member(member).ok_or(AccessWriteError::NotFound)?;
        // Final round A (T8-6): a write landing just before this one (`race_once`), as a login
        // made between the handler's reads and the revocation.
        let race = self.race_before_write.lock().unwrap().pop_front();
        if let Some(write) = race {
            write(self);
        }
        let n = self.revoke_member_logins(member, RevokeReason::RevokedNow);
        let rotated = self.mark_member_roles_for_rotation(member);
        self.log(
            Change::new(m.tenant_id, Some(actor), "personal_logins_revoked", Some(member))
                .after(json!({ "count": n, "reason": "revoked_now", "rotated_branches": rotated })),
        );
        Ok(self.login_touched_projects(member))
    }

    async fn record_access_event(&self, e: &chelabase_metadata::personal_logins::AccessEventInput<'_>) -> Result<(), MetadataError> {
        let mut events = self.events.lock().unwrap();
        let id = events.len() as i64 + 1;
        events.push(chelabase_metadata::members::AccessEvent {
            id,
            tenant_id: e.tenant,
            actor_member_id: e.actor,
            action: e.action.as_str().into(),
            target_member_id: e.target,
            project_id: e.project,
            database_id: e.database,
            before: e.before.cloned(),
            after: e.after.cloned(),
            at: chrono::Utc::now(),
            actor_email: None,
            target_email: None,
            project_name: None,
            branch_id: e.branch,
        });
        Ok(())
    }

    async fn member_observed(&self, member: uuid::Uuid) -> Result<Vec<Observed>, MetadataError> {
        Ok(self.member_observed_now(member))
    }

    async fn set_tenant_admin(
        &self,
        actor: Option<uuid::Uuid>,
        member: uuid::Uuid,
        admin: bool,
        expect: Option<&[Observed]>,
    ) -> FakeWrite<()> {
        use chelabase_metadata::members::AccessWriteError;
        let m = self.members.lock().unwrap().iter().find(|m| m.id == member).cloned().ok_or(AccessWriteError::NotFound)?;
        if admin && m.status == MemberStatus::Removed {
            return Err(AccessWriteError::NotFound);
        }
        if admin && m.status != MemberStatus::Active {
            return Err(AccessWriteError::Conflict("only an active member may become a tenant admin".into()));
        }
        if m.tenant_admin == admin {
            return Ok(());
        }
        if !admin && m.status == MemberStatus::Active && self.other_active_admins(m.tenant_id, member) == 0 {
            return Err(AccessWriteError::LastTenantAdmin);
        }
        // As the real one under the tenant lock: the pending race runs, then a demotion is
        // `Changed` when anything the member keeps differs from what the check read.
        if !admin && let Some(expect) = expect {
            let race = self.race_before_write.lock().unwrap().pop_front();
            if let Some(write) = race {
                write(self);
            }
            if self.member_observed_now(member) != expect {
                return Err(AccessWriteError::Changed);
            }
        }
        self.members.lock().unwrap().iter_mut().find(|x| x.id == member).unwrap().tenant_admin = admin;
        let action = if admin { "admin_set" } else { "admin_unset" };
        self.log(Change::new(m.tenant_id, actor, action, Some(member)).after(json!({ "tenant_admin": admin })));
        Ok(())
    }

    async fn remove_member(&self, actor: uuid::Uuid, member: uuid::Uuid) -> FakeWrite<Vec<uuid::Uuid>> {
        use chelabase_metadata::members::AccessWriteError;
        let m = self.members.lock().unwrap().iter().find(|m| m.id == member).cloned().ok_or(AccessWriteError::NotFound)?;
        if m.status == MemberStatus::Removed {
            return Ok(Vec::new());
        }
        if m.status == MemberStatus::Active && m.tenant_admin && self.other_active_admins(m.tenant_id, member) == 0 {
            return Err(AccessWriteError::LastTenantAdmin);
        }
        let mut touched: Vec<uuid::Uuid> = self.grants.lock().unwrap().keys().filter(|(m, _)| *m == member).map(|(_, p)| *p).collect();
        let overridden: Vec<uuid::Uuid> = self.overrides.lock().unwrap().keys().filter(|(m, _)| *m == member).map(|(_, d)| *d).collect();
        for d in overridden {
            if let Some(db) = self.databases.lock().unwrap().get(&DatabaseId(d)) {
                touched.push(db.project_id.as_uuid());
            }
        }
        // As the real one: the projects it holds member roles in too.
        let role_branches: Vec<BranchId> =
            self.member_branch_roles.lock().unwrap().iter().filter(|(m, _)| *m == member).map(|(_, b)| *b).collect();
        for b in role_branches {
            let db = self.branches.lock().unwrap().get(&b).map(|b| b.database_id);
            if let Some(db) = db.and_then(|d| self.databases.lock().unwrap().get(&d).map(|d| d.project_id.as_uuid())) {
                touched.push(db);
            }
        }
        // T23 (P10, MH-5): its teams' grant projects, read before the memberships go.
        touched.extend(self.member_team_projects(member));
        touched.sort();
        touched.dedup();
        let teams: Vec<Value> = {
            let mut all = self.teams.lock().unwrap();
            let mut out = Vec::new();
            for t in all.iter_mut().filter(|t| t.members.contains(&member)) {
                t.members.remove(&member);
                out.push(json!({ "id": t.record.id, "name": t.record.name }));
            }
            out
        };
        let grants: Vec<Value> = self
            .grants
            .lock()
            .unwrap()
            .iter()
            .filter(|((m, _), _)| *m == member)
            .map(|((_, p), g)| json!({ "project_id": p, "platform_role": g.platform_role.as_str(), "pg_roles": g.pg_roles }))
            .collect();
        self.grants.lock().unwrap().retain(|(m, _), _| *m != member);
        self.overrides.lock().unwrap().retain(|(m, _), _| *m != member);
        for i in self.invites.lock().unwrap().iter_mut().filter(|i| i.member_id == member && !i.accepted) {
            i.revoked = true;
        }
        {
            let mut members = self.members.lock().unwrap();
            let x = members.iter_mut().find(|x| x.id == member).unwrap();
            x.status = MemberStatus::Removed;
            x.tenant_admin = false;
        }
        let mut before = json!({ "grants": grants });
        if !teams.is_empty() {
            before["teams"] = json!(teams);
        }
        self.log(Change::new(m.tenant_id, Some(actor), "member_removed", Some(member)).before(before));
        Ok(touched)
    }

    async fn put_project_grant(&self, actor: uuid::Uuid, member: uuid::Uuid, project: uuid::Uuid, grant: &GrantRow, expect: &Observed) -> FakeWrite<bool> {
        Ok(!self.put_project_grants(actor, member, &[(project, grant.clone())], &[], std::slice::from_ref(expect)).await?.is_empty())
    }

    /// As the real one: all or nothing, an unchanged grant writes nothing.
    async fn put_project_grants(
        &self,
        actor: uuid::Uuid,
        member: uuid::Uuid,
        grants: &[(uuid::Uuid, GrantRow)],
        teams: &[chelabase_metadata::members::TeamSeen],
        expect: &[Observed],
    ) -> FakeWrite<Vec<uuid::Uuid>> {
        use chelabase_metadata::members::AccessWriteError;
        if grants.iter().any(|(_, g)| g.names_owner()) {
            return Err(AccessWriteError::Conflict("owner".into()));
        }
        let mut seen = HashSet::new();
        if !grants.iter().all(|(p, _)| seen.insert(*p)) {
            return Err(AccessWriteError::Conflict("a project twice".into()));
        }
        let m = self.writable_member(member)?;
        if grants.iter().any(|(p, _)| !self.project_active_in(m.tenant_id, *p)) {
            return Err(AccessWriteError::NotFound);
        }
        // P25: the teams first (under the tenant lock), then the member's `expect`.
        self.teams_seen_hold(m.tenant_id, teams)?;
        self.settle(member, expect)?;
        let mut changed = Vec::new();
        for (project, grant) in grants {
            let mut roles = grant.pg_roles.clone();
            roles.sort();
            roles.dedup();
            let after = GrantRow { platform_role: grant.platform_role, pg_roles: roles.clone() };
            if self.grants.lock().unwrap().insert((member, *project), after.clone()).as_ref() == Some(&after) {
                continue;
            }
            self.log(Change::new(m.tenant_id, Some(actor), "grant_put", Some(member)).scope(Some(*project), None).after(json!({ "platform_role": grant.platform_role.as_str(), "pg_roles": roles })));
            changed.push(*project);
        }
        // P12: the memberships; a team joined reaches its grants' projects.
        let before: HashSet<uuid::Uuid> =
            self.teams.lock().unwrap().iter().filter(|t| t.members.contains(&member)).map(|t| t.record.id).collect();
        self.add_team_memberships(actor, m.tenant_id, member, teams);
        for seen in teams.iter().filter(|t| !before.contains(&t.team)) {
            for (project, _) in &seen.grants {
                if !changed.contains(project) {
                    changed.push(*project);
                }
            }
        }
        Ok(changed)
    }

    async fn delete_project_grant(&self, actor: uuid::Uuid, member: uuid::Uuid, project: uuid::Uuid, expect: &Observed) -> FakeWrite<()> {
        use chelabase_metadata::members::AccessWriteError;
        let m = self.writable_member(member)?;
        if expect.project != project {
            return Err(AccessWriteError::Changed);
        }
        self.settle(member, std::slice::from_ref(expect))?;
        self.grants.lock().unwrap().remove(&(member, project)).ok_or(AccessWriteError::NotFound)?;
        self.log(Change::new(m.tenant_id, Some(actor), "grant_deleted", Some(member)).scope(Some(project), None));
        Ok(())
    }

    async fn put_override(&self, actor: uuid::Uuid, member: uuid::Uuid, database: uuid::Uuid, o: &OverrideRow, expect: &Observed) -> FakeWrite<bool> {
        use chelabase_metadata::members::AccessWriteError;
        if o.platform_role.is_none() && o.pg_roles.is_none() {
            return Err(AccessWriteError::Conflict("empty".into()));
        }
        if o.names_owner() {
            return Err(AccessWriteError::Conflict("owner".into()));
        }
        let m = self.writable_member(member)?;
        let project = self.database_project(m.tenant_id, database).ok_or(AccessWriteError::NotFound)?;
        if expect.project != project {
            return Err(AccessWriteError::Changed);
        }
        self.settle(member, std::slice::from_ref(expect))?;
        let after = OverrideRow {
            platform_role: o.platform_role,
            pg_roles: o.pg_roles.clone().map(|mut r| {
                r.sort();
                r.dedup();
                r
            }),
        };
        if self.overrides.lock().unwrap().insert((member, database), after.clone()).as_ref() == Some(&after) {
            return Ok(false);
        }
        self.log(Change::new(m.tenant_id, Some(actor), "override_put", Some(member)).scope(Some(project), Some(database)));
        Ok(true)
    }

    async fn delete_override(&self, actor: uuid::Uuid, member: uuid::Uuid, database: uuid::Uuid, expect: &Observed) -> FakeWrite<()> {
        use chelabase_metadata::members::AccessWriteError;
        let m = self.writable_member(member)?;
        let project = self.databases.lock().unwrap().get(&DatabaseId(database)).map(|d| d.project_id.as_uuid());
        let project = project.ok_or(AccessWriteError::NotFound)?;
        if expect.project != project {
            return Err(AccessWriteError::Changed);
        }
        self.settle(member, std::slice::from_ref(expect))?;
        self.overrides.lock().unwrap().remove(&(member, database)).ok_or(AccessWriteError::NotFound)?;
        self.log(Change::new(m.tenant_id, Some(actor), "override_deleted", Some(member)).scope(Some(project), Some(database)));
        Ok(())
    }

    async fn create_invited_member(
        &self,
        actor: uuid::Uuid,
        tenant: TenantId,
        new: chelabase_metadata::members::NewInvite<'_>,
    ) -> FakeWrite<(chelabase_metadata::members::MemberListing, uuid::Uuid)> {
        use chelabase_metadata::members::{AccessWriteError, NewInvite};
        let NewInvite { identity_user_id, email, expires_at, token_hash, grants, teams } = new;
        // P25: the teams must still be what the check read, or nothing is written.
        self.teams_seen_hold(tenant, teams)?;
        let email = email.trim().to_lowercase();
        let existing = self.member(tenant, identity_user_id);
        if existing.as_ref().is_some_and(|m| m.status != MemberStatus::Removed) {
            return Err(AccessWriteError::Conflict("already a member".into()));
        }
        let taken = self.members.lock().unwrap().iter().any(|m| {
            m.tenant_id == tenant && m.status != MemberStatus::Removed && m.email.as_deref() == Some(email.as_str())
        });
        if taken {
            return Err(AccessWriteError::Conflict("email taken".into()));
        }
        let id = existing.as_ref().map_or_else(uuid::Uuid::new_v4, |m| m.id);
        self.insert_member_grants(tenant, id, grants)?;
        let record = MemberRecord {
            id,
            tenant_id: tenant,
            identity_user_id: identity_user_id.into(),
            email: Some(email.clone()),
            display_name: String::new(),
            tenant_admin: false,
            status: MemberStatus::Invited,
            accepted_at: None,
            // As the store: only `clear_offboarded` takes the mark away.
            offboarded: existing.as_ref().is_some_and(|m| m.offboarded),
            suspended_at: None, suspended_by: None,
        };
        {
            let mut members = self.members.lock().unwrap();
            members.retain(|m| m.id != id);
            members.push(record.clone());
        }
        let invite = uuid::Uuid::new_v4();
        self.invites.lock().unwrap().push(FakeInvite {
            id: invite,
            member_id: id,
            identity_user_id: identity_user_id.into(),
            expires_at,
            token_hash,
            revoked: false,
            accepted: false,
        });
        self.log(Change::new(tenant, Some(actor), "invite_created", Some(id)).scope(single_project(grants), None).after(json!({ "email": email, "grants": grants_json(grants) })));
        self.add_team_memberships(actor, tenant, id, teams);
        let listing = chelabase_metadata::members::MemberListing {
            created_at: self.created_at(record.id),
            has_access: !grants.is_empty() || teams.iter().any(|t| !t.grants.is_empty()),
            member: record,
        };
        Ok((listing, invite))
    }

    async fn resend_invite(&self, actor: uuid::Uuid, invite: uuid::Uuid, expires_at: chrono::DateTime<chrono::Utc>, token_hash: [u8; 32]) -> FakeWrite<()> {
        use chelabase_metadata::members::AccessWriteError;
        let member = {
            let mut invites = self.invites.lock().unwrap();
            let i = invites.iter_mut().find(|i| i.id == invite).ok_or(AccessWriteError::NotFound)?;
            if i.revoked || i.accepted {
                return Err(AccessWriteError::Conflict("the invite is no longer open".into()));
            }
            i.expires_at = expires_at;
            i.token_hash = token_hash;
            i.member_id
        };
        let tenant = self.members.lock().unwrap().iter().find(|m| m.id == member).unwrap().tenant_id;
        self.log(Change::new(tenant, Some(actor), "invite_resent", Some(member)));
        Ok(())
    }

    async fn revoke_undelivered_invite(&self, actor: uuid::Uuid, invite: uuid::Uuid) -> FakeWrite<String> {
        self.revoke_with(actor, invite, Some("delivery_failed"))
    }

    async fn revoke_invite(&self, actor: uuid::Uuid, invite: uuid::Uuid) -> FakeWrite<String> {
        self.revoke_with(actor, invite, None)
    }

    async fn set_open_signup(&self, actor: uuid::Uuid, tenant: TenantId, on: bool) -> FakeWrite<()> {
        let old = self.open_signups.lock().unwrap().insert(tenant, on).unwrap_or(false);
        if old != on {
            self.log(Change::new(tenant, Some(actor), "open_signup_set", None).after(json!({ "open_signup": on })));
        }
        Ok(())
    }

    async fn approve_member(
        &self,
        actor: uuid::Uuid,
        member: uuid::Uuid,
        grants: &[(uuid::Uuid, GrantRow)],
        teams: &[chelabase_metadata::members::TeamSeen],
    ) -> FakeWrite<chelabase_metadata::members::MemberListing> {
        use chelabase_metadata::members::AccessWriteError;
        if self.vanish_on_approve.lock().unwrap().take() == Some(member)
            && let Some(x) = self.members.lock().unwrap().iter_mut().find(|x| x.id == member)
        {
            x.status = MemberStatus::Removed;
        }
        let m = self.members.lock().unwrap().iter().find(|m| m.id == member).cloned().ok_or(AccessWriteError::MemberNotFound)?;
        match m.status {
            MemberStatus::Pending => {}
            MemberStatus::Removed => return Err(AccessWriteError::MemberNotFound),
            _ => return Err(AccessWriteError::Conflict("the member isn't waiting for approval".into())),
        }
        // P25: the teams must still be what the check read, or nothing is written.
        self.teams_seen_hold(m.tenant_id, teams)?;
        self.insert_member_grants(m.tenant_id, member, grants)?;
        let record = {
            let mut members = self.members.lock().unwrap();
            let row = members.iter_mut().find(|x| x.id == member).unwrap();
            row.status = MemberStatus::Active;
            row.accepted_at = Some(chrono::Utc::now());
            row.clone()
        };
        self.log(Change::new(m.tenant_id, Some(actor), "member_approved", Some(member)).scope(single_project(grants), None).before(json!({ "status": "pending" })).after(json!({ "status": "active", "grants": grants_json(grants) })));
        self.add_team_memberships(actor, m.tenant_id, member, teams);
        Ok(chelabase_metadata::members::MemberListing {
            created_at: self.created_at(record.id),
            has_access: !grants.is_empty() || teams.iter().any(|t| !t.grants.is_empty()),
            member: record,
        })
    }

    async fn other_memberships(&self, identity_user_id: &str, except: Option<uuid::Uuid>) -> Result<chelabase_metadata::members::OtherMemberships, MetadataError> {
        if *self.fail_other_memberships.lock().unwrap() {
            return Err(MetadataError::Conflict);
        }
        let members = self.members.lock().unwrap();
        let others: Vec<_> = members
            .iter()
            .filter(|m| Some(m.id) != except && m.identity_user_id == identity_user_id && m.status != MemberStatus::Removed)
            .collect();
        Ok(chelabase_metadata::members::OtherMemberships {
            active: others.iter().any(|m| m.status == MemberStatus::Active),
            live: !others.is_empty(),
        })
    }

    async fn decline_member(&self, actor: uuid::Uuid, member: uuid::Uuid) -> FakeWrite<String> {
        use chelabase_metadata::members::AccessWriteError;
        let m = self.members.lock().unwrap().iter().find(|m| m.id == member).cloned().ok_or(AccessWriteError::MemberNotFound)?;
        match m.status {
            MemberStatus::Pending => {}
            MemberStatus::Removed => return Err(AccessWriteError::MemberNotFound),
            _ => return Err(AccessWriteError::Conflict("the member isn't waiting for approval".into())),
        }
        self.members.lock().unwrap().iter_mut().find(|x| x.id == member).unwrap().status = MemberStatus::Removed;
        self.log(Change::new(m.tenant_id, Some(actor), "member_declined", Some(member)).before(json!({ "status": "pending" })).after(json!({ "status": "removed" })));
        Ok(m.identity_user_id)
    }

    async fn set_tenant_display_name(
        &self,
        actor: uuid::Uuid,
        tenant: TenantId,
        display_name: Option<&str>,
    ) -> FakeWrite<()> {
        let mut names = self.tenant_names.lock().unwrap();
        let old = match display_name {
            Some(name) => names.insert(tenant, name.to_string()),
            None => names.remove(&tenant),
        };
        if old.as_deref() != display_name {
            self.log(Change::new(tenant, Some(actor), "tenant_renamed", None).before(json!({ "display_name": old })).after(json!({ "display_name": display_name })));
        }
        Ok(())
    }
}

// --- Members hub step 2 (T23): teams ----------------------------------------------------------
//
// The fake's side of T1's `teams.rs` and the team parts of `members.rs`, behaving as their docs
// say. The `fake_*` methods have the signatures of `MetadataClient`'s methods of the same name
// without the prefix (synchronous); the port's fake impls delegate to them. `seed_*` write
// directly (no event, no check), for test setup.

type TeamWriteResult<T> = Result<T, chelabase_metadata::teams::TeamWriteError>;

/// A team event's `before`/`after` (P9, P38), as T1's `team_event_json` builds it.
fn team_event_json(team: uuid::Uuid, name: &str, grants: &[(uuid::Uuid, GrantRow)], affected: &[uuid::Uuid]) -> Value {
    let mut ids: Vec<String> = affected.iter().map(uuid::Uuid::to_string).collect();
    ids.sort();
    ids.dedup();
    json!({ "team": { "id": team, "name": name }, "grants": grants_json(grants), "affected_member_ids": ids })
}

#[allow(dead_code)]
impl FakeMetadata {
    /// A team of `tenant` (no event, no check); returns its id.
    fn seed_team(&self, tenant: TenantId, name: &str) -> uuid::Uuid {
        let now = chrono::Utc::now();
        let id = uuid::Uuid::new_v4();
        self.teams.lock().unwrap().push(FakeTeam {
            record: chelabase_metadata::teams::TeamRecord {
                id,
                tenant_id: tenant,
                name: name.into(),
                description: String::new(),
                created_by: None,
                created_at: now,
                updated_at: now,
            },
            grants: Default::default(),
            members: Default::default(),
        });
        id
    }

    /// A team grant (no event, no check; an Admin grant names no roles, as `GrantRow::new`).
    fn seed_team_grant(&self, team: uuid::Uuid, project: ProjectId, role: PlatformRole, pg_roles: &[&str]) {
        let roles: Vec<String> = pg_roles.iter().map(|r| r.to_string()).collect();
        let mut teams = self.teams.lock().unwrap();
        let t = teams.iter_mut().find(|t| t.record.id == team).expect("team");
        t.grants.insert(project.as_uuid(), GrantRow::new(role, &roles));
    }

    /// A team membership (no event, no check).
    fn seed_team_member(&self, team: uuid::Uuid, member: uuid::Uuid) {
        let mut teams = self.teams.lock().unwrap();
        teams.iter_mut().find(|t| t.record.id == team).expect("team").members.insert(member);
    }

    /// The stored team, if any (any tenant).
    fn stored_team(&self, team: uuid::Uuid) -> Option<FakeTeam> {
        self.teams.lock().unwrap().iter().find(|t| t.record.id == team).cloned()
    }

    /// Whether `project` is a non-deleted project of `tenant`.
    fn project_live_in(&self, tenant: TenantId, project: uuid::Uuid) -> bool {
        self.projects
            .lock()
            .unwrap()
            .iter()
            .any(|p| p.id.as_uuid() == project && p.tenant_id == tenant && p.status != ProjectStatus::Deleted)
    }

    fn member_tenant(&self, member: uuid::Uuid) -> Option<TenantId> {
        self.members.lock().unwrap().iter().find(|m| m.id == member).map(|m| m.tenant_id)
    }

    /// The team's grants as the reads keep them: non-deleted projects of its tenant, by project.
    fn live_team_grants(&self, t: &FakeTeam) -> Vec<(uuid::Uuid, GrantRow)> {
        t.grants
            .iter()
            .filter(|(p, _)| self.project_live_in(t.record.tenant_id, **p))
            .map(|(p, g)| (*p, g.clone()))
            .collect()
    }

    /// The team's members of its own tenant, by id.
    fn live_team_members(&self, t: &FakeTeam) -> Vec<uuid::Uuid> {
        t.members.iter().copied().filter(|m| self.member_tenant(*m) == Some(t.record.tenant_id)).collect()
    }

    /// The shared shape's team grants (P4, P27): team id → grant on `project`, for the
    /// member's teams of its own tenant, on a non-deleted project of that tenant.
    fn stored_team_grants(&self, member: uuid::Uuid, project: uuid::Uuid) -> std::collections::BTreeMap<uuid::Uuid, GrantRow> {
        let Some(tenant) = self.member_tenant(member) else { return Default::default() };
        if !self.project_live_in(tenant, project) {
            return Default::default();
        }
        self.teams
            .lock()
            .unwrap()
            .iter()
            .filter(|t| t.record.tenant_id == tenant && t.members.contains(&member))
            .filter_map(|t| t.grants.get(&project).map(|g| (t.record.id, g.clone())))
            .collect()
    }

    /// What `member_observed_tx` reads now: one non-empty `Observed` per project the member
    /// reaches (own grant, override or team grant), sorted by project.
    fn member_observed_now(&self, member: uuid::Uuid) -> Vec<Observed> {
        let mut projects: Vec<uuid::Uuid> =
            self.grants.lock().unwrap().keys().filter(|(m, _)| *m == member).map(|(_, p)| *p).collect();
        let overridden: Vec<uuid::Uuid> =
            self.overrides.lock().unwrap().keys().filter(|(m, _)| *m == member).map(|(_, d)| *d).collect();
        for d in overridden {
            if let Some(db) = self.databases.lock().unwrap().get(&DatabaseId(d)) {
                projects.push(db.project_id.as_uuid());
            }
        }
        projects.extend(self.member_team_projects(member));
        projects.sort();
        projects.dedup();
        projects
            .into_iter()
            .map(|p| self.observed_now(member, p))
            .filter(|o| o.grant.is_some() || !o.overrides.is_empty() || !o.teams.is_empty())
            .collect()
    }

    /// Every project the member reaches through a team, by id.
    fn member_team_projects(&self, member: uuid::Uuid) -> Vec<uuid::Uuid> {
        let teams: Vec<FakeTeam> =
            self.teams.lock().unwrap().iter().filter(|t| t.members.contains(&member)).cloned().collect();
        let mut out: Vec<uuid::Uuid> = teams
            .iter()
            .filter(|t| self.member_tenant(member) == Some(t.record.tenant_id))
            .flat_map(|t| self.live_team_grants(t).into_iter().map(|(p, _)| p))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    fn team_detail(&self, t: &FakeTeam) -> chelabase_metadata::teams::TeamDetail {
        chelabase_metadata::teams::TeamDetail {
            team: t.record.clone(),
            grants: self.live_team_grants(t),
            member_ids: self.live_team_members(t),
        }
    }

    /// Teams chosen by `keep`, by lower(name) then id.
    fn team_details(&self, keep: impl Fn(&FakeTeam) -> bool) -> Vec<chelabase_metadata::teams::TeamDetail> {
        let mut teams: Vec<FakeTeam> = self.teams.lock().unwrap().iter().filter(|t| keep(t)).cloned().collect();
        teams.sort_by_key(|t| (t.record.name.to_lowercase(), t.record.id));
        teams.iter().map(|t| self.team_detail(t)).collect()
    }

    fn fake_list_teams(&self, tenant: TenantId) -> Result<Vec<chelabase_metadata::teams::TeamDetail>, MetadataError> {
        Ok(self.team_details(|t| t.record.tenant_id == tenant))
    }

    fn fake_team(&self, tenant: TenantId, team: uuid::Uuid) -> Result<Option<chelabase_metadata::teams::TeamDetail>, MetadataError> {
        Ok(self.team_details(|t| t.record.id == team && t.record.tenant_id == tenant).into_iter().next())
    }

    fn fake_member_teams(&self, member: uuid::Uuid) -> Result<Vec<chelabase_metadata::teams::TeamDetail>, MetadataError> {
        let tenant = self.member_tenant(member);
        Ok(self.team_details(|t| t.members.contains(&member) && Some(t.record.tenant_id) == tenant))
    }

    fn fake_project_team_grants(&self, project: uuid::Uuid) -> Result<Vec<chelabase_metadata::teams::ProjectTeamGrant>, MetadataError> {
        Ok(self
            .team_details(|t| t.grants.contains_key(&project))
            .into_iter()
            .filter_map(|d| {
                let grant = d.grants.iter().find(|(p, _)| *p == project)?.1.clone();
                Some(chelabase_metadata::teams::ProjectTeamGrant { team_id: d.team.id, name: d.team.name, grant, member_ids: d.member_ids })
            })
            .collect())
    }

    /// P2: trimmed, 1–80 characters; a description at most 500 (else `Conflict`).
    fn checked_team_name(name: &str) -> TeamWriteResult<String> {
        let name = name.trim();
        if !(1..=80).contains(&name.chars().count()) {
            return Err(chelabase_metadata::teams::TeamWriteError::Conflict("a team name has 1 to 80 characters".into()));
        }
        Ok(name.to_string())
    }

    fn checked_team_description(description: &str) -> TeamWriteResult<String> {
        if description.chars().count() > 500 {
            return Err(chelabase_metadata::teams::TeamWriteError::Conflict("a team description has at most 500 characters".into()));
        }
        Ok(description.to_string())
    }

    fn team_name_taken(&self, tenant: TenantId, name: &str, except: Option<uuid::Uuid>) -> bool {
        self.teams.lock().unwrap().iter().any(|t| {
            t.record.tenant_id == tenant && Some(t.record.id) != except && t.record.name.to_lowercase() == name.to_lowercase()
        })
    }

    fn fake_create_team(&self, actor: uuid::Uuid, tenant: TenantId, name: &str, description: &str) -> TeamWriteResult<chelabase_metadata::teams::TeamRecord> {
        let name = Self::checked_team_name(name)?;
        let description = Self::checked_team_description(description)?;
        if self.team_name_taken(tenant, &name, None) {
            return Err(chelabase_metadata::teams::TeamWriteError::NameTaken);
        }
        let id = self.seed_team(tenant, &name);
        let record = {
            let mut teams = self.teams.lock().unwrap();
            let t = teams.iter_mut().find(|t| t.record.id == id).unwrap();
            t.record.description = description.clone();
            t.record.created_by = Some(actor);
            t.record.clone()
        };
        let mut after = team_event_json(id, &record.name, &[], &[]);
        after["description"] = json!(description);
        self.log(Change::new(tenant, Some(actor), "team_created", None).after(after));
        Ok(record)
    }

    fn fake_update_team(
        &self,
        actor: uuid::Uuid,
        tenant: TenantId,
        team: uuid::Uuid,
        name: Option<&str>,
        description: Option<&str>,
    ) -> TeamWriteResult<chelabase_metadata::teams::TeamRecord> {
        use chelabase_metadata::teams::TeamWriteError;
        let name = name.map(Self::checked_team_name).transpose()?;
        let description = description.map(Self::checked_team_description).transpose()?;
        let before = self.stored_team(team).filter(|t| t.record.tenant_id == tenant).ok_or(TeamWriteError::NotFound)?.record;
        let new_name = name.unwrap_or_else(|| before.name.clone());
        let new_description = description.unwrap_or_else(|| before.description.clone());
        if new_name == before.name && new_description == before.description {
            return Ok(before);
        }
        if self.team_name_taken(tenant, &new_name, Some(team)) {
            return Err(TeamWriteError::NameTaken);
        }
        let after = {
            let mut teams = self.teams.lock().unwrap();
            let t = teams.iter_mut().find(|t| t.record.id == team).unwrap();
            t.record.name = new_name;
            t.record.description = new_description;
            t.record.updated_at = chrono::Utc::now();
            t.record.clone()
        };
        let mut before_json = team_event_json(team, &before.name, &[], &[]);
        before_json["description"] = json!(before.description);
        let mut after_json = team_event_json(team, &after.name, &[], &[]);
        after_json["description"] = json!(after.description);
        self.log(Change::new(tenant, Some(actor), "team_updated", None).before(before_json).after(after_json));
        Ok(after)
    }

    /// T1's `plan`: validates the change in `tenant` and returns the team, its grants (as the
    /// reads keep them) and the affected members and projects (`affected_sets`).
    #[allow(clippy::type_complexity)]
    fn team_plan(
        &self,
        tenant: TenantId,
        change: &chelabase_metadata::teams::TeamChange,
    ) -> TeamWriteResult<(FakeTeam, Vec<(uuid::Uuid, GrantRow)>, Vec<uuid::Uuid>, Vec<uuid::Uuid>)> {
        use chelabase_metadata::teams::{TeamChange, TeamWriteError};
        if let TeamChange::PutGrant { grant, .. } = change
            && grant.names_owner()
        {
            return Err(TeamWriteError::Conflict(
                "the owner role is never granted: the admin platform role implies the database owner".into(),
            ));
        }
        let team = self.stored_team(change.team()).filter(|t| t.record.tenant_id == tenant).ok_or(TeamWriteError::NotFound)?;
        match change {
            TeamChange::PutGrant { project, .. } => {
                if !self.project_active_in(tenant, *project) {
                    return Err(TeamWriteError::ProjectNotFound);
                }
            }
            TeamChange::DeleteGrant { project, .. } => {
                let found = self.projects.lock().unwrap().iter().any(|p| p.id.as_uuid() == *project && p.tenant_id == tenant);
                if !found {
                    return Err(TeamWriteError::ProjectNotFound);
                }
            }
            TeamChange::AddMember { member, .. } | TeamChange::RemoveMember { member, .. } => {
                let m = self.members.lock().unwrap().iter().find(|m| m.id == *member).cloned();
                match m {
                    Some(m) if m.tenant_id == tenant => match m.status {
                        MemberStatus::Removed => return Err(TeamWriteError::MemberNotFound),
                        MemberStatus::Pending if matches!(change, TeamChange::AddMember { .. }) => {
                            return Err(TeamWriteError::MemberPending);
                        }
                        _ => {}
                    },
                    _ => return Err(TeamWriteError::MemberNotFound),
                }
            }
            TeamChange::Delete { .. } => {}
        }
        let members = self.live_team_members(&team);
        let grants = self.live_team_grants(&team);
        let mut projects: Vec<uuid::Uuid> = grants.iter().map(|(p, _)| *p).collect();
        let (affected, projects) = match change {
            TeamChange::PutGrant { project, .. } | TeamChange::DeleteGrant { project, .. } => (members, vec![*project]),
            TeamChange::AddMember { member, .. } | TeamChange::RemoveMember { member, .. } => {
                projects.sort();
                (vec![*member], projects)
            }
            TeamChange::Delete { .. } => {
                projects.sort();
                (members, projects)
            }
        };
        Ok((team, grants, affected, projects))
    }

    /// T1's `observe`: every affected member × project as it stands; for `AddMember` each one
    /// also holds the team's grant on its project (what the member would get), so a team grant
    /// changed between the check and the write is `Changed`.
    fn team_observed(
        &self,
        change: &chelabase_metadata::teams::TeamChange,
        grants: &[(uuid::Uuid, GrantRow)],
        affected: &[uuid::Uuid],
        projects: &[uuid::Uuid],
    ) -> Vec<(uuid::Uuid, Observed)> {
        use chelabase_metadata::teams::TeamChange;
        affected
            .iter()
            .flat_map(|m| projects.iter().map(move |p| (*m, *p)))
            .map(|(m, p)| {
                let mut observed = self.observed_now(m, p);
                if let TeamChange::AddMember { team, .. } = change
                    && let Some((_, g)) = grants.iter().find(|(gp, _)| *gp == p)
                {
                    observed.teams.insert(*team, g.clone());
                }
                (m, observed)
            })
            .collect()
    }

    fn fake_team_change_observed(
        &self,
        tenant: TenantId,
        change: &chelabase_metadata::teams::TeamChange,
    ) -> TeamWriteResult<Vec<(uuid::Uuid, Observed)>> {
        let (_, grants, affected, projects) = self.team_plan(tenant, change)?;
        Ok(self.team_observed(change, &grants, &affected, &projects))
    }

    /// MH-9: after a write, every live login of `member` that no longer qualifies is revoked
    /// (`access_lost`, one event when any).
    fn revoke_unqualified_logins(&self, actor: uuid::Uuid, member: uuid::Uuid) {
        use chelabase_metadata::personal_logins::{LoginStatus, RevokeReason};
        let Some(tenant) = self.member_tenant(member) else { return };
        let live: Vec<uuid::Uuid> = self
            .personal_logins
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.login.member_id == member && l.login.revoked_at.is_none())
            .map(|l| l.login.database_id)
            .collect();
        let lost: Vec<uuid::Uuid> =
            live.into_iter().filter(|d| self.login_level(member, *d).is_none_or(|l| l < PlatformRole::Developer)).collect();
        if lost.is_empty() {
            return;
        }
        for l in self.personal_logins.lock().unwrap().iter_mut().filter(|l| l.login.member_id == member && lost.contains(&l.login.database_id)) {
            l.login.revoked_at = Some(chrono::Utc::now());
            l.login.revoked_reason = Some(RevokeReason::AccessLost);
            l.login.status = LoginStatus::Revoked;
            l.verifier = None;
        }
        self.log(
            Change::new(tenant, Some(actor), "personal_logins_revoked", Some(member))
                .after(json!({ "count": lost.len(), "reason": "access_lost" })),
        );
    }

    /// T1's `write_team_checked`: runs the pending race (`race_once`), then `Changed` unless
    /// the check's read (`expect`, the affected set included) still holds; else writes the
    /// change and its event (P9), revokes what stops qualifying (MH-9), and answers what it
    /// reached. An unchanged write logs nothing (`changed: false`).
    fn fake_write_team_checked(
        &self,
        actor: uuid::Uuid,
        tenant: TenantId,
        change: &chelabase_metadata::teams::TeamChange,
        expect: &[(uuid::Uuid, Observed)],
        skipped_admins: &[uuid::Uuid],
    ) -> TeamWriteResult<chelabase_metadata::teams::TeamWrite> {
        use chelabase_metadata::teams::{TeamChange, TeamWrite, TeamWriteError};
        let race = self.race_before_write.lock().unwrap().pop_front();
        if let Some(write) = race {
            write(self);
        }
        let (team, grants, affected_ids, projects) = self.team_plan(tenant, change)?;
        if self.team_observed(change, &grants, &affected_ids, &projects) != expect {
            return Err(TeamWriteError::Changed);
        }
        // As the real one under the locks: a member the check skipped as a tenant admin must
        // still be one (a demotion committed since is `Changed`).
        let still_admin = |id: &uuid::Uuid| {
            self.members
                .lock()
                .unwrap()
                .iter()
                .any(|m| m.id == *id && m.tenant_id == tenant && m.tenant_admin && m.status != MemberStatus::Removed)
        };
        if affected_ids.iter().any(|id| skipped_admins.contains(id) && !still_admin(id)) {
            return Err(TeamWriteError::Changed);
        }
        let affected: Vec<(uuid::Uuid, String)> = affected_ids
            .iter()
            .map(|id| {
                let m = self.members.lock().unwrap().iter().find(|m| m.id == *id).cloned().ok_or(TeamWriteError::MemberNotFound)?;
                Ok((m.id, m.identity_user_id))
            })
            .collect::<TeamWriteResult<_>>()?;
        let reached = TeamWrite { changed: false, affected: affected.clone(), projects: projects.clone() };
        let (id, name) = (team.record.id, team.record.name.clone());
        let event = {
            let mut teams = self.teams.lock().unwrap();
            let pos = teams.iter().position(|t| t.record.id == id).unwrap();
            let t = &mut teams[pos];
            match change {
                TeamChange::PutGrant { project, grant, .. } => {
                    let after = GrantRow::new(grant.platform_role, &grant.pg_roles);
                    let before = t.grants.insert(*project, after.clone());
                    if before.as_ref() == Some(&after) {
                        return Ok(reached);
                    }
                    Change::new(tenant, Some(actor), "team_grant_put", None)
                        .scope(Some(*project), None)
                        .before(before.map(|b| team_event_json(id, &name, &[(*project, b)], &affected_ids)))
                        .after(team_event_json(id, &name, &[(*project, after)], &affected_ids))
                }
                TeamChange::DeleteGrant { project, .. } => {
                    let Some(before) = t.grants.remove(project) else { return Ok(reached) };
                    Change::new(tenant, Some(actor), "team_grant_deleted", None)
                        .scope(Some(*project), None)
                        .before(team_event_json(id, &name, &[(*project, before)], &affected_ids))
                }
                TeamChange::AddMember { member, .. } => {
                    if !t.members.insert(*member) {
                        return Ok(reached);
                    }
                    Change::new(tenant, Some(actor), "team_member_added", Some(*member))
                        .after(team_event_json(id, &name, &grants, &[*member]))
                }
                TeamChange::RemoveMember { member, .. } => {
                    if !t.members.remove(member) {
                        return Ok(reached);
                    }
                    Change::new(tenant, Some(actor), "team_member_removed", Some(*member))
                        .before(team_event_json(id, &name, &grants, &[*member]))
                }
                TeamChange::Delete { .. } => {
                    teams.remove(pos);
                    Change::new(tenant, Some(actor), "team_deleted", None)
                        .before(team_event_json(id, &name, &grants, &affected_ids))
                }
            }
        };
        if let Some(t) = self.teams.lock().unwrap().iter_mut().find(|t| t.record.id == id) {
            t.record.updated_at = chrono::Utc::now();
        }
        self.log(event);
        for (member, _) in &affected {
            self.revoke_unqualified_logins(actor, *member);
        }
        Ok(TeamWrite { changed: true, affected, projects })
    }

    /// The team as an invite's check reads it (P25): its grants on non-deleted projects.
    fn fake_team_seen(&self, tenant: TenantId, team: uuid::Uuid) -> Option<chelabase_metadata::members::TeamSeen> {
        let t = self.stored_team(team).filter(|t| t.record.tenant_id == tenant)?;
        Some(chelabase_metadata::members::TeamSeen { team, grants: self.live_team_grants(&t) })
    }

    /// A write another request makes just before the next re-read of an invite's or approval's
    /// teams (P25).
    fn race_teams_once(&self, write: impl FnOnce(&FakeMetadata) + Send + 'static) {
        self.race_before_teams_seen.lock().unwrap().push_back(Box::new(write));
    }

    /// P25 under the tenant lock: runs the pending teams race (`race_teams_once`), then
    /// `Changed` unless every team is still a team of `tenant` with the grants the check read.
    fn teams_seen_hold(&self, tenant: TenantId, teams: &[chelabase_metadata::members::TeamSeen]) -> FakeWrite<()> {
        use chelabase_metadata::members::AccessWriteError;
        if teams.is_empty() {
            return Ok(());
        }
        let race = self.race_before_teams_seen.lock().unwrap().pop_front();
        if let Some(write) = race {
            write(self);
        }
        for seen in teams {
            if self.fake_team_seen(tenant, seen.team).as_ref() != Some(seen) {
                return Err(AccessWriteError::Changed);
            }
        }
        Ok(())
    }

    /// The invite's or approval's memberships (P12), one `team_member_added` each; a member
    /// already in a team is left as it is.
    fn add_team_memberships(&self, actor: uuid::Uuid, tenant: TenantId, member: uuid::Uuid, teams: &[chelabase_metadata::members::TeamSeen]) {
        for seen in teams {
            let added = {
                let mut all = self.teams.lock().unwrap();
                all.iter_mut()
                    .find(|t| t.record.id == seen.team)
                    .map(|t| (t.members.insert(member), t.record.name.clone()))
            };
            if let Some((true, name)) = added {
                self.log(
                    Change::new(tenant, Some(actor), "team_member_added", Some(member))
                        .after(team_event_json(seen.team, &name, &seen.grants, &[member])),
                );
            }
        }
    }

    /// `teams_seen_hold`, then the memberships: what `add_existing_user`, `approve_member` and
    /// `put_project_grants_checked` do with their `teams` (P25), for the port's fake impls.
    fn apply_teams_seen(&self, actor: uuid::Uuid, tenant: TenantId, member: uuid::Uuid, teams: &[chelabase_metadata::members::TeamSeen]) -> FakeWrite<()> {
        self.teams_seen_hold(tenant, teams)?;
        self.add_team_memberships(actor, tenant, member, teams);
        Ok(())
    }

    /// P13: one row per (member, project) with the best level among the sources and where it
    /// comes from, by member then project (live members of `tenant`, non-deleted projects).
    fn fake_member_project_roles(&self, tenant: TenantId) -> Result<Vec<chelabase_metadata::personal_logins::MemberProjectRole>, MetadataError> {
        use chelabase_metadata::personal_logins::{GrantVia, MemberProjectRole};
        if *self.fail_member_project_roles.lock().unwrap() {
            return Err(MetadataError::Config("member project roles unreadable".into()));
        }
        let members: Vec<uuid::Uuid> = self
            .members
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.tenant_id == tenant && m.status != MemberStatus::Removed)
            .map(|m| m.id)
            .collect();
        let projects: Vec<uuid::Uuid> = self
            .projects
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.tenant_id == tenant && p.status != ProjectStatus::Deleted)
            .map(|p| p.id.as_uuid())
            .collect();
        let mut out = Vec::new();
        for m in &members {
            for p in &projects {
                let own = self.grants.lock().unwrap().get(&(*m, *p)).map(|g| g.platform_role);
                let team = self.stored_team_grants(*m, *p).values().map(|g| g.platform_role).max();
                let (platform_role, via) = match (own, team) {
                    (Some(o), Some(t)) => (o.max(t), GrantVia::Both),
                    (Some(o), None) => (o, GrantVia::Own),
                    (None, Some(t)) => (t, GrantVia::Team),
                    (None, None) => continue,
                };
                out.push(MemberProjectRole { member: *m, project: *p, platform_role, via });
            }
        }
        out.sort_by_key(|r| (r.member, r.project));
        Ok(out)
    }
}

/// T23 tests' setup through the team API, as the default caller (org-1's tenant admin): a team
/// named `name` with `role` and `pg_roles` on `project` and these members. Every call must
/// answer as P17 pins (201 create, 200 grant and member puts); returns the team's id.
#[allow(dead_code)]
async fn api_team(app: &TestApp, name: &str, project: ProjectId, role: &str, pg_roles: &[&str], members: &[uuid::Uuid]) -> uuid::Uuid {
    let resp = app.send_as(DEFAULT_SUB, "org-1", Method::POST, "/v1/teams", Some(json!({ "name": name }))).await;
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    assert_eq!(status, 201, "POST /v1/teams {name}: {body}");
    let team: uuid::Uuid = body["id"].as_str().expect("the team's id").parse().unwrap();
    let grant = format!("/v1/teams/{team}/grants/{project}");
    let resp = app.send_as(DEFAULT_SUB, "org-1", Method::PUT, &grant, Some(json!({ "platform_role": role, "pg_roles": pg_roles }))).await;
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    assert_eq!(status, 200, "PUT {grant}: {body}");
    for m in members {
        api_add_to_team(app, team, *m).await;
    }
    team
}

/// [`api_team`] with the members added before the grant, so the grant is checked on their
/// union. For a grant an empty team couldn't hold on its own (a role safe only for members who
/// are Admin through another source; final fix round, defect 1).
#[allow(dead_code)]
async fn api_team_joined(app: &TestApp, name: &str, project: ProjectId, role: &str, pg_roles: &[&str], members: &[uuid::Uuid]) -> uuid::Uuid {
    let resp = app.send_as(DEFAULT_SUB, "org-1", Method::POST, "/v1/teams", Some(json!({ "name": name }))).await;
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    assert_eq!(status, 201, "POST /v1/teams {name}: {body}");
    let team: uuid::Uuid = body["id"].as_str().expect("the team's id").parse().unwrap();
    for m in members {
        api_add_to_team(app, team, *m).await;
    }
    let grant = format!("/v1/teams/{team}/grants/{project}");
    let resp = app.send_as(DEFAULT_SUB, "org-1", Method::PUT, &grant, Some(json!({ "platform_role": role, "pg_roles": pg_roles }))).await;
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    assert_eq!(status, 200, "PUT {grant}: {body}");
    team
}

/// `PUT /v1/teams/{team}/members/{member}` as the default caller; must answer 200.
#[allow(dead_code)]
async fn api_add_to_team(app: &TestApp, team: uuid::Uuid, member: uuid::Uuid) {
    let path = format!("/v1/teams/{team}/members/{member}");
    let resp = app.send_as(DEFAULT_SUB, "org-1", Method::PUT, &path, None).await;
    let status = resp.status().as_u16();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    assert_eq!(status, 200, "PUT {path}: {body}");
}

/// A member token (#119) of `sub` in `tenant` signed with `keys`, living `ttl` seconds from now
/// (negative: expired that long ago), with an email and name claim.
#[allow(dead_code)]
fn signed_member_token(
    keys: &chelabase_auth::test_support::MemberKeys,
    sub: &str,
    tenant: TenantId,
    ttl: i64,
) -> String {
    let mut claims = keys.claims(sub, tenant.as_uuid(), 300);
    let now = claims.iat;
    claims.iat = if ttl < 0 { now - 300 } else { now };
    claims.exp = (now as i64 + ttl) as u64;
    claims.email = format!("{sub}@mail.test");
    claims.name = format!("Name of {sub}");
    keys.sign(&claims)
}

/// Edge functions m3: the functions tables, as `chelabase_metadata`'s functions API answers them.
impl FakeMetadata {
    /// `FunctionNotFound` unless `branch` is a live branch of the function's database.
    fn fn_scope(&self, branch: BranchId, function: uuid::Uuid) -> Result<(), MetadataError> {
        let branch_db = self
            .branches
            .lock()
            .unwrap()
            .get(&branch)
            .filter(|b| !matches!(b.status, BranchStatus::Deleting | BranchStatus::Deleted))
            .map(|b| b.database_id);
        let fs = self.functions.lock().unwrap();
        match fs.functions.iter().find(|f| f.id == function) {
            Some(f) if Some(f.database_id) == branch_db => Ok(()),
            _ => Err(MetadataError::FunctionNotFound),
        }
    }

    /// Whether a live protected branch has the function active.
    fn fn_active_on_protected(&self, function: uuid::Uuid) -> bool {
        let branches = self.branches.lock().unwrap();
        self.functions.lock().unwrap().states.iter().any(|(b, st)| {
            st.function_id == function
                && branches.get(b).is_some_and(|b| {
                    b.protected && !matches!(b.status, BranchStatus::Deleting | BranchStatus::Deleted)
                })
        })
    }

    /// Revokes the live keys `function` manages (on `branch`, or everywhere).
    fn fn_revoke_managed(&self, branch: Option<BranchId>, function: uuid::Uuid) {
        for (k, _) in self.keys.lock().unwrap().iter_mut() {
            if k.revoked_at.is_none()
                && k.managed_by_function.as_ref().is_some_and(|(id, _)| *id == function)
                && branch.is_none_or(|b| k.branch_id == b)
            {
                k.revoked_at = Some(chrono::Utc::now());
                k.sealed = None;
            }
        }
    }

    /// `change` on the function's state on the branch; false when it has none.
    fn fn_state(
        &self,
        branch: BranchId,
        function: uuid::Uuid,
        change: impl FnOnce(&mut chelabase_metadata::BranchFunction),
    ) -> Result<bool, MetadataError> {
        self.fn_scope(branch, function)?;
        let mut fs = self.functions.lock().unwrap();
        match fs.states.iter_mut().find(|(b, st)| *b == branch && st.function_id == function) {
            Some((_, st)) => {
                change(st);
                st.updated_at = chrono::Utc::now();
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

impl orchestrator::functions::FunctionsPort for FakeMetadata {
    async fn set_service_signing_secret(
        &self,
        database: DatabaseId,
        function: uuid::Uuid,
        secret: &str,
    ) -> Result<bool, MetadataError> {
        let found = self.functions.lock().unwrap().functions.iter().any(|f| f.id == function && f.database_id == database);
        if found {
            self.signing_secrets.lock().unwrap().insert(function, secret.to_owned());
        }
        Ok(found)
    }

    async fn create_function(
        &self,
        database: DatabaseId,
        name: &str,
        kind: chelabase_metadata::FunctionKind,
    ) -> Result<chelabase_metadata::FunctionRecord, MetadataError> {
        if !chelabase_metadata::valid_function_name(name) {
            return Err(MetadataError::InvalidFunction("bad name".into()));
        }
        let mut fs = self.functions.lock().unwrap();
        if fs.functions.iter().any(|f| f.database_id == database && f.name == name) {
            return Err(MetadataError::Conflict);
        }
        let record = chelabase_metadata::FunctionRecord {
            id: uuid::Uuid::new_v4(),
            database_id: database,
            name: name.to_string(),
            kind,
            created_at: chrono::Utc::now(),
        };
        fs.functions.push(record.clone());
        Ok(record)
    }

    async fn list_functions(&self, database: DatabaseId) -> Result<Vec<chelabase_metadata::FunctionRecord>, MetadataError> {
        let mut out: Vec<_> = self
            .functions
            .lock()
            .unwrap()
            .functions
            .iter()
            .filter(|f| f.database_id == database)
            .cloned()
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn set_function_kind(
        &self,
        database: DatabaseId,
        function: uuid::Uuid,
        kind: chelabase_metadata::FunctionKind,
    ) -> Result<bool, MetadataError> {
        let current = {
            let fs = self.functions.lock().unwrap();
            fs.functions.iter().find(|f| f.id == function && f.database_id == database).map(|f| f.kind)
        };
        let Some(current) = current else { return Ok(false) };
        if self.fn_active_on_protected(function) {
            return Err(MetadataError::FunctionActiveOnProtectedBranch);
        }
        if current == kind {
            return Ok(true);
        }
        let mut fs = self.functions.lock().unwrap();
        if fs.deployments.iter().any(|d| d.function_id == function) {
            return Err(MetadataError::InvalidFunction("a function with deployments keeps its kind".into()));
        }
        fs.functions.iter_mut().find(|f| f.id == function).unwrap().kind = kind;
        Ok(true)
    }

    async fn delete_function_unprotected(&self, database: DatabaseId, function: uuid::Uuid) -> Result<bool, MetadataError> {
        let found = self
            .functions
            .lock()
            .unwrap()
            .functions
            .iter()
            .any(|f| f.id == function && f.database_id == database);
        if !found {
            return Ok(false);
        }
        if self.fn_active_on_protected(function) {
            return Err(MetadataError::FunctionActiveOnProtectedBranch);
        }
        self.fn_revoke_managed(None, function);
        let mut fs = self.functions.lock().unwrap();
        fs.functions.retain(|f| f.id != function);
        fs.deployments.retain(|d| d.function_id != function);
        fs.states.retain(|(_, st)| st.function_id != function);
        fs.secrets.retain(|(_, f, _), _| *f != function);
        Ok(true)
    }

    async fn create_deployment(
        &self,
        d: &chelabase_metadata::NewDeployment<'_>,
    ) -> Result<chelabase_metadata::DeploymentRecord, MetadataError> {
        let mut fs = self.functions.lock().unwrap();
        if !fs.functions.iter().any(|f| f.id == d.function_id) {
            return Err(MetadataError::FunctionNotFound);
        }
        let record = chelabase_metadata::DeploymentRecord {
            id: d.id,
            function_id: d.function_id,
            bundle_sha256: d.bundle_sha256.map(str::to_string),
            bundle_size: d.bundle_size,
            spec: d.spec.clone(),
            compatibility_date: d.compatibility_date,
            created_by: d.created_by.to_string(),
            created_at: chrono::Utc::now(),
            note: d.note.map(str::to_string),
        };
        fs.deployments.push(record.clone());
        Ok(record)
    }

    async fn list_deployments(&self, function: uuid::Uuid) -> Result<Vec<chelabase_metadata::DeploymentRecord>, MetadataError> {
        let mut out: Vec<_> = self
            .functions
            .lock()
            .unwrap()
            .deployments
            .iter()
            .filter(|d| d.function_id == function)
            .cloned()
            .collect();
        out.reverse();
        Ok(out)
    }

    async fn set_active_deployment(&self, branch: BranchId, function: uuid::Uuid, deployment: uuid::Uuid) -> Result<(), MetadataError> {
        self.fn_scope(branch, function)?;
        let mut fs = self.functions.lock().unwrap();
        if !fs.deployments.iter().any(|d| d.id == deployment && d.function_id == function) {
            return Err(MetadataError::InvalidFunction("the deployment belongs to another function".into()));
        }
        let f = fs.functions.iter().find(|f| f.id == function).cloned().unwrap();
        match fs.states.iter_mut().find(|(b, st)| *b == branch && st.function_id == function) {
            Some((_, st)) => {
                st.deployment_id = deployment;
                st.last_error = None;
                st.updated_at = chrono::Utc::now();
            }
            None => fs.states.push((
                branch,
                chelabase_metadata::BranchFunction {
                    function_id: function,
                    name: f.name,
                    kind: f.kind,
                    deployment_id: deployment,
                    enabled: false,
                    auth: chelabase_metadata::FunctionAuth::Key,
                    run_as_role: None,
                    run_as_key_id: None,
                    run_as_role_oid: None,
                    run_as_unmintable: None,
                    last_error: None,
                    updated_at: chrono::Utc::now(),
                    health: None,
                },
            )),
        }
        Ok(())
    }

    async fn set_enabled(&self, branch: BranchId, function: uuid::Uuid, enabled: bool) -> Result<bool, MetadataError> {
        self.fn_state(branch, function, |st| st.enabled = enabled)
    }

    async fn set_function_auth(
        &self,
        branch: BranchId,
        function: uuid::Uuid,
        auth: chelabase_metadata::FunctionAuth,
    ) -> Result<bool, MetadataError> {
        self.fn_state(branch, function, |st| st.auth = auth)
    }

    async fn set_run_as(
        &self,
        branch: BranchId,
        function: uuid::Uuid,
        run_as: Option<&chelabase_metadata::RunAs>,
    ) -> Result<bool, MetadataError> {
        if !self.fn_state(branch, function, |_| {})? {
            return Ok(false);
        }
        self.fn_revoke_managed(Some(branch), function);
        let name = self.functions.lock().unwrap().functions.iter().find(|f| f.id == function).unwrap().name.clone();
        if let Some(r) = run_as {
            self.keys.lock().unwrap().push((
                DataKeyRecord {
                    id: r.key_id,
                    branch_id: branch,
                    kind: DataKeyKind::Secret,
                    role: r.role.clone(),
                    name: name.clone(),
                    prefix: r.prefix.clone(),
                    sealed: Some("sealed".into()),
                    created_at: chrono::Utc::now(),
                    last_used_at: None,
                    revoked_at: None,
                    databases: None,
                    role_oid: Some(r.role_oid),
                    role_timeline: r.role_timeline.clone(),
                    role_bypass_rls: r.role_bypass_rls,
                    managed_by_function: Some((function, name)),
                },
                r.sha256,
            ));
        }
        self.fn_state(branch, function, |st| {
            st.run_as_role = run_as.map(|r| r.role.clone());
            st.run_as_key_id = run_as.map(|r| r.key_id);
        })
    }

    async fn list_for_branch(&self, branch: BranchId) -> Result<Vec<chelabase_metadata::BranchFunction>, MetadataError> {
        let mut out: Vec<_> = self
            .functions
            .lock()
            .unwrap()
            .states
            .iter()
            .filter(|(b, _)| *b == branch)
            .map(|(_, st)| st.clone())
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn put_secret(&self, branch: BranchId, function: uuid::Uuid, name: &str, value: &str) -> Result<(), MetadataError> {
        if !chelabase_metadata::valid_secret_name(name) {
            return Err(MetadataError::InvalidFunction("a secret name is a letter or _, then A-Z, a-z, 0-9 and _".into()));
        }
        if value.len() > chelabase_metadata::MAX_SECRET_BYTES {
            return Err(MetadataError::InvalidFunction("a secret is at most 65536 bytes".into()));
        }
        self.fn_scope(branch, function)?;
        self.functions.lock().unwrap().secrets.insert((branch.as_uuid(), function, name.to_string()), value.to_string());
        Ok(())
    }

    async fn delete_secret(&self, branch: BranchId, function: uuid::Uuid, name: &str) -> Result<bool, MetadataError> {
        self.fn_scope(branch, function)?;
        Ok(self.functions.lock().unwrap().secrets.remove(&(branch.as_uuid(), function, name.to_string())).is_some())
    }

    async fn secret_names(&self, branch: BranchId, function: uuid::Uuid) -> Result<Vec<String>, MetadataError> {
        Ok(self
            .functions
            .lock()
            .unwrap()
            .secrets
            .keys()
            .filter(|(b, f, _)| *b == branch.as_uuid() && *f == function)
            .map(|(_, _, n)| n.clone())
            .collect())
    }
}

impl orchestrator::functions_schedules::SchedulesPort for FakeMetadata {
    async fn add_schedule(&self, s: &chelabase_metadata::NewSchedule<'_>) -> Result<uuid::Uuid, MetadataError> {
        self.fn_scope(s.branch_id, s.function_id)?;
        let id = uuid::Uuid::new_v4();
        self.functions.lock().unwrap().schedules.push(chelabase_metadata::ScheduleRecord {
            id,
            branch_id: s.branch_id,
            function_id: s.function_id,
            cron: s.cron.to_string(),
            timezone: s.timezone.to_string(),
            next_run_at: s.next_run_at,
            enabled: true,
        });
        Ok(id)
    }

    async fn list_schedules(&self, branch: BranchId) -> Result<Vec<chelabase_metadata::ScheduleRecord>, MetadataError> {
        Ok(self.functions.lock().unwrap().schedules.iter().filter(|s| s.branch_id == branch).cloned().collect())
    }

    async fn update_schedule(
        &self,
        branch: BranchId,
        function: uuid::Uuid,
        id: uuid::Uuid,
        cron: Option<&str>,
        timezone: Option<&str>,
        enabled: Option<bool>,
    ) -> Result<Option<chelabase_metadata::ScheduleRecord>, MetadataError> {
        self.fn_scope(branch, function)?;
        let mut fs = self.functions.lock().unwrap();
        let Some(s) = fs.schedules.iter_mut().find(|s| s.id == id && s.branch_id == branch && s.function_id == function) else {
            return Ok(None);
        };
        let (new_cron, new_tz) = (cron.unwrap_or(&s.cron).to_string(), timezone.unwrap_or(&s.timezone).to_string());
        let parsed = chelabase_metadata::Schedule::parse(&new_cron, &new_tz)
            .map_err(|e| MetadataError::InvalidFunction(e.to_string()))?;
        let new_enabled = enabled.unwrap_or(s.enabled);
        if new_cron != s.cron || new_tz != s.timezone || (new_enabled && !s.enabled) {
            s.next_run_at = parsed.next_after(chrono::Utc::now());
        }
        (s.cron, s.timezone, s.enabled) = (new_cron, new_tz, new_enabled);
        Ok(Some(s.clone()))
    }

    async fn delete_schedule(&self, branch: BranchId, function: uuid::Uuid, id: uuid::Uuid) -> Result<bool, MetadataError> {
        self.fn_scope(branch, function)?;
        let mut fs = self.functions.lock().unwrap();
        let before = fs.schedules.len();
        fs.schedules.retain(|s| !(s.id == id && s.branch_id == branch && s.function_id == function));
        Ok(fs.schedules.len() < before)
    }
}
