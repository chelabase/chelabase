use axum::Router;
use chelabase_auth::internal::{
    generate_keypair_pem, AuthMethod, Identity, InternalSigner, InternalVerifier, IDENTITY_HEADER,
};
use chelabase_metadata::{
    BeginDelete, BranchDatabase, BranchDeleteOutcome, BranchKind, BranchRecord, BranchStatus, ClaimOutcome, CreateBranchOutcome, DatabaseOrigin, DataKeyRecord, DatabaseRecord,
    DatabaseStatus, MetadataError, NewBranch, NewDataKey, PasswordState, ProjectOverview, ProjectStatus, RoleLevel, RoleOrigin, RoleRow, SettingsPatch,
    TenantOverview,
    AppSession, AppUsersStatus, BeginEnable, BeginProjectDelete, BranchAuthSync, NewProjectAuth, OnDelete, ProjectAuth,
    ProjectRecord, RenameProject, SavedQuery, SavedQueryPatch, SavedQuerySummary, SavedQueryWrite,
};
use chelabase_nats::{
    AppUsersCheckReply, AppUsersCheckRequest, AppUsersJob, AppUsersSyncJob, ProjectDeleteJob, BranchJob, CheckDataRoleReply, DatabaseSettingsJob, DeleteDatabaseJob, JobEnvelope, PointInTime,
    ProvisionDatabaseJob, ResetPasswordReply, RoleCreateRequest, RoleReply, RoleRequest, RESET_NOT_APPLIED,
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

type KeyRow = (DataKeyRecord, [u8; 32]);
/// A key store, a key id and the role name it takes.
type RenameOnCheck = (Arc<Mutex<Vec<KeyRow>>>, uuid::Uuid, String);

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
    project_auth_reads: Arc<AtomicUsize>,
    project_auth_batch_reads: Arc<AtomicUsize>,
    /// Saved queries with their (database, user).
    saved_queries: Arc<Mutex<Vec<(uuid::Uuid, String, SavedQuery)>>>,
}

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
    }
}

impl FakeMetadata {
    fn bind_org(&self, org_id: &str, tenant_id: TenantId) {
        self.tenants_by_org
            .lock()
            .unwrap()
            .insert(org_id.to_string(), tenant_id);
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
            },
        );
        id
    }

    fn branch(&self, id: BranchId) -> BranchRecord {
        self.branches.lock().unwrap().get(&id).cloned().expect("branch row")
    }

    fn database(&self, id: DatabaseId) -> DatabaseRecord {
        self.databases.lock().unwrap().get(&id).cloned().expect("database row")
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
    })
}

impl MetadataPort for FakeMetadata {
    async fn get_tenant_by_zitadel_org_id(
        &self,
        zitadel_org_id: &str,
    ) -> Result<Option<TenantId>, MetadataError> {
        Ok(self.tenants_by_org.lock().unwrap().get(zitadel_org_id).copied())
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
        Ok(self.overviews.lock().unwrap().get(&tenant_id).cloned())
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

    async fn set_branch_failed(&self, branch_id: BranchId, error: &str) -> Result<(), MetadataError> {
        if let Some(b) = self.branches.lock().unwrap().get_mut(&branch_id) {
            b.status = BranchStatus::Failed;
            b.error = Some(error.into());
        }
        Ok(())
    }

    async fn begin_branch_delete(&self, branch_id: BranchId) -> Result<BranchDeleteOutcome, MetadataError> {
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
        // As the store: the old key's OID wins; a different checked OID is a recreated role.
        let stored = self
            .keys
            .lock()
            .unwrap()
            .iter()
            .find(|(k, _)| k.id == old && k.branch_id == branch && k.revoked_at.is_none())
            .map(|(k, _)| (k.role.clone(), k.role_oid));
        let mut new = new.clone();
        if let Some((role, stored)) = stored {
            if let (Some(stored), Some(checked)) = (stored, new.role_oid)
                && stored != checked
            {
                return Err(MetadataError::RoleRecreated(role));
            }
            new.role_oid = stored.or(new.role_oid);
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
        if *self.fail_list_roles.lock().unwrap() {
            return Err(MetadataError::Config("injected".into()));
        }
        Ok(self.roles.lock().unwrap().get(endpoint_id).cloned().unwrap_or_default())
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
            if existing.status == AppUsersStatus::Failed && same {
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
                zitadel_org_id: None,
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

    async fn list_saved_queries(&self, database: uuid::Uuid, user: &str) -> Result<Vec<SavedQuerySummary>, MetadataError> {
        let mut rows: Vec<SavedQuerySummary> = self
            .saved_queries
            .lock()
            .unwrap()
            .iter()
            .filter(|(d, u, _)| *d == database && u == user)
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

    async fn get_saved_query(&self, database: uuid::Uuid, user: &str, id: uuid::Uuid) -> Result<Option<SavedQuery>, MetadataError> {
        Ok(self
            .saved_queries
            .lock()
            .unwrap()
            .iter()
            .find(|(d, u, q)| *d == database && u == user && q.id == id)
            .map(|(_, _, q)| q.clone()))
    }

    async fn create_saved_query(
        &self,
        database: uuid::Uuid,
        user: &str,
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
        };
        rows.push((database, user.into(), q.clone()));
        Ok(SavedQueryWrite::Saved(q))
    }

    async fn update_saved_query(
        &self,
        database: uuid::Uuid,
        user: &str,
        id: uuid::Uuid,
        version: i32,
        patch: &SavedQueryPatch,
    ) -> Result<SavedQueryWrite, MetadataError> {
        let mut rows = self.saved_queries.lock().unwrap();
        let Some(at) = rows.iter().position(|(d, u, q)| *d == database && u == user && q.id == id) else {
            return Ok(SavedQueryWrite::NotFound);
        };
        if rows[at].2.version != version {
            return Ok(SavedQueryWrite::Stale(rows[at].2.clone()));
        }
        if let Some(name) = &patch.name
            && rows.iter().any(|(d, u, q)| {
                *d == database && u == user && q.id != id && q.name.to_lowercase() == name.to_lowercase()
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

    async fn delete_saved_query(&self, database: uuid::Uuid, user: &str, id: uuid::Uuid) -> Result<bool, MetadataError> {
        let mut rows = self.saved_queries.lock().unwrap();
        let before = rows.len();
        rows.retain(|(d, u, q)| !(*d == database && u == user && q.id == id));
        Ok(rows.len() < before)
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
    published: Arc<Mutex<Vec<JobEnvelope<ProvisionDatabaseJob>>>>,
    deleted: Arc<Mutex<Vec<DatabaseId>>>,
    resets: Arc<Mutex<Vec<(DatabaseId, BranchId)>>>,
    reset: Arc<Mutex<FakeReset>>,
    branch_jobs: Arc<Mutex<Vec<JobEnvelope<BranchJob>>>>,
    settings_jobs: Arc<Mutex<Vec<DatabaseId>>>,
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
    /// (branch, role, owner) of each grant-to-owner request.
    role_grants: Arc<Mutex<Vec<(BranchId, String, String)>>>,
    /// What the worker answers to a grant-to-owner request; `ok` when unset.
    role_grant: Arc<Mutex<Option<Result<CheckDataRoleReply, ResetError>>>>,
    /// Each login-role RPC as `create:<name>:<privileged>`, `reset:<name>` or `delete:<name>`.
    role_calls: Arc<Mutex<Vec<String>>>,
    /// What the worker answers to the role RPCs; `ok` when unset.
    role_reply: Arc<Mutex<Option<Result<RoleReply, ResetError>>>>,
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
}

impl FakeJobs {
    fn reset_answers(&self, answer: FakeReset) {
        *self.reset.lock().unwrap() = answer;
    }
}

impl JobPort for FakeJobs {
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

    async fn grant_data_role_to_owner(
        &self,
        branch: BranchId,
        role: &str,
        owner: &str,
    ) -> Result<CheckDataRoleReply, ResetError> {
        self.role_grants.lock().unwrap().push((branch, role.to_string(), owner.to_string()));
        match self.role_grant.lock().unwrap().clone() {
            Some(answer) => answer,
            None => Ok(CheckDataRoleReply::ok()),
        }
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
        self.role_answer()
    }

    async fn reset_role(&self, request: RoleRequest) -> Result<RoleReply, ResetError> {
        self.role_calls.lock().unwrap().push(format!("reset:{}", request.name));
        self.role_answer()
    }

    async fn delete_role(&self, request: RoleRequest) -> Result<RoleReply, ResetError> {
        self.role_calls.lock().unwrap().push(format!("delete:{}", request.name));
        self.role_answer()
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
            None => Ok(RoleReply { ok: true, code: None, error: None, keys: vec![] }),
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
}

impl TestApp {
    /// An internal identity token for `org_id`, as the gateway would mint it.
    fn token_for_org(&self, org_id: &str) -> String {
        self.signer
            .sign(Service::Orchestrator.name(), &identity(org_id))
            .unwrap()
    }
}

fn identity(org_id: &str) -> Identity {
    Identity {
        sub: "user-1".into(),
        org_id: org_id.into(),
        auth: AuthMethod::ManagementJwt,
        request_id: "test-rid".into(),
        data: None,
    }
}

/// Boots the real orchestrator router (fake metadata/jobs, fresh internal
/// keypair) on an ephemeral port.
async fn spawn_app(metadata: FakeMetadata, jobs: FakeJobs) -> TestApp {
    // Nothing listens on the discard port: rest is "down".
    spawn_app_with_rest(metadata, jobs, "http://127.0.0.1:9").await
}

const REST_TOKEN: &str = "rest-internal-token";

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
        None,
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
    zitadel: Option<chelabase_zitadel::client::Zitadel>,
    users_db: Arc<dyn chelabase_query::app_users::UsersDb>,
) -> TestApp {
    let (private_pem, public_pem) = generate_keypair_pem();
    let key = SecretKey::generate();
    let locator = FakeLocator::default();
    let state = Arc::new(AppState {
        metadata,
        js: jobs,
        identity: InternalVerifier::from_pem(public_pem.as_bytes()).unwrap(),
        secrets: key.clone(),
        proxy: ProxyAddress::local("localtest.me"),
        rest: RestInternal {
            url: rest_url.into(),
            token: REST_TOKEN.into(),
            http: reqwest::Client::new(),
        },
        mode,
        proxy_ca_file: ".chelabase/neon/proxy-ca.crt".into(),
        data_api_database_cap: cap,
        locator: Arc::new(locator.clone()),
        console: Console::default(),
        zitadel,
        users_db,
        sql_editor: Default::default(),
    });
    let app: Router = build_router(state.clone());

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
    }
}

fn create_body(tenant_id: TenantId) -> serde_json::Value {
    serde_json::json!({
        "tenant_id": tenant_id,
        "project_id": ProjectId::new(),
        "name": "demo",
    })
}

async fn post_create(app: &TestApp, identity_header: Option<&str>, body: serde_json::Value) -> reqwest::Response {
    let mut req = reqwest::Client::new()
        .post(format!("{}/v1/databases", app.base))
        .json(&body);
    if let Some(token) = identity_header {
        req = req.header(IDENTITY_HEADER, token);
    }
    req.send().await.unwrap()
}

#[tokio::test]
async fn health_is_public() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = reqwest::get(format!("{}/health", app.base)).await.unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn openapi_json_route_serves_the_document() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = reqwest::get(format!("{}/v1/openapi.json", app.base)).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "application/json");
    assert_eq!(resp.headers()["cache-control"], "no-cache");
    let served: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(served, serde_json::to_value(orchestrator::openapi()).unwrap());
    let committed = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../openapi/management.json")).unwrap();
    assert_eq!(served, serde_json::from_str::<serde_json::Value>(&committed).unwrap());
}

#[tokio::test]
async fn docs_page_configures_scalar_offline() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = reqwest::get(format!("{}/v1/docs", app.base)).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers()["content-type"].to_str().unwrap().starts_with("text/html"));
    let html = resp.text().await.unwrap();
    assert!(html.contains("Chelabase Management API"), "{html}");
    // The script URL carries the first 12 hex of the bundle's sha256 (from VERSION), so a
    // Scalar upgrade is never served stale from the day-long cache.
    let version = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/scalar/VERSION")).unwrap();
    let sha = version.lines().nth(1).unwrap().strip_prefix("sha256 ").unwrap();
    assert!(html.contains(&format!("src=\"/v1/docs/scalar.js?v={}\"", &sha[..12])), "{html}");
    assert!(html.contains("url: \"/v1/openapi.json\""), "{html}");
    assert!(html.contains("telemetry: false"), "{html}");
    assert!(html.contains("withDefaultFonts: false"), "{html}");
    assert!(html.contains("authentication: { preferredSecurityScheme: \"bearer\" }"), "{html}");
    // Omitting these leaves Scalar's scalar.com defaults active, so they are set to "".
    assert!(html.contains("proxyUrl: \"\""), "{html}");
    assert!(html.contains("showDeveloperTools: \"never\""), "{html}");
    assert!(html.contains("agent: { disabled: true }"), "{html}");
    assert!(html.contains("mcp: { disabled: true }"), "{html}");
    for key in ["dashboardUrl", "registryUrl", "proxyUrl", "apiBaseUrl"] {
        assert!(html.contains(&format!("{key}: \"\"")), "{key} in {html}");
    }
    for host in ["cdn.jsdelivr.net", "scalar.com", "unpkg.com"] {
        assert!(!html.contains(host), "{host} in {html}");
    }
}

#[tokio::test]
async fn scalar_bundle_matches_version_file() {
    use sha2::{Digest, Sha256};
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = reqwest::get(format!("{}/v1/docs/scalar.js?v=abc", app.base)).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "application/javascript; charset=utf-8");
    assert_eq!(resp.headers()["cache-control"], "public, max-age=86400");
    let bytes = resp.bytes().await.unwrap();
    let version = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/scalar/VERSION")).unwrap();
    let mut lines = version.lines();
    assert!(lines.next().unwrap().starts_with("@scalar/api-reference "));
    let expected = lines.next().unwrap().strip_prefix("sha256 ").unwrap();
    assert_eq!(hex::encode(Sha256::digest(&bytes)), expected);
    let missing = reqwest::get(format!("{}/v1/docs/nope.js", app.base)).await.unwrap();
    assert_eq!(missing.status(), 404);
    let body: serde_json::Value = missing.json().await.unwrap();
    assert_eq!(code_of(&body), "not_found");
}

#[tokio::test]
async fn missing_identity_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = post_create(&app, None, create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 401);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(code_of(&body), "unauthorized");
    assert!(message_of(&body).contains("gateway"));
}

#[tokio::test]
async fn bearer_token_without_identity_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/databases", app.base))
        .bearer_auth("some-zitadel-jwt")
        .json(&create_body(TenantId::new()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn garbage_identity_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = post_create(&app, Some("not-a-jwt"), create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn expired_identity_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let token = app
        .signer
        .sign_at(Service::Orchestrator.name(), &identity("org-1"), now - 3600)
        .unwrap();
    let resp = post_create(&app, Some(&token), create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn identity_minted_for_another_service_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let token = app.signer.sign("some-other-service", &identity("org-1")).unwrap();
    let resp = post_create(&app, Some(&token), create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn identity_from_untrusted_key_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let (other_private, _) = generate_keypair_pem();
    let forger = InternalSigner::from_pem(other_private.as_bytes()).unwrap();
    let token = forger
        .sign(Service::Orchestrator.name(), &identity("org-1"))
        .unwrap();
    let resp = post_create(&app, Some(&token), create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn unbound_org_is_tenant_not_bound() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let token = app.token_for_org("org-not-bound");
    let resp = post_create(&app, Some(&token), create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 403);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(code_of(&body), "tenant_not_bound");
    assert!(message_of(&body).contains("zitadel-bootstrap"));
}

#[tokio::test]
async fn body_tenant_mismatch_is_403() {
    let metadata = FakeMetadata::default();
    metadata.bind_org("org-1", TenantId::new());
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let token = app.token_for_org("org-1");
    let resp = post_create(&app, Some(&token), create_body(TenantId::new())).await;
    assert_eq!(resp.status(), 403);
}

#[tokio::test]
async fn valid_request_is_202_accepted_and_publishes_job() {
    let metadata = FakeMetadata::default();
    let bound_tenant = TenantId::new();
    metadata.bind_org("org-1", bound_tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app(metadata, jobs.clone()).await;
    let token = app.token_for_org("org-1");

    let project_id = ProjectId::new();
    let resp = post_create(
        &app,
        Some(&token),
        serde_json::json!({
            "tenant_id": bound_tenant,
            "project_id": project_id,
            "name": "demo-db",
        }),
    )
    .await;

    assert_eq!(resp.status(), 202);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "pending");
    assert!(body["job_id"].is_string());
    assert!(body["database_id"].is_string());

    let published = jobs.published.lock().unwrap();
    assert_eq!(published.len(), 1);
    assert_eq!(published[0].payload.tenant_id, bound_tenant);
    assert_eq!(published[0].payload.project_id, project_id);
    assert_eq!(published[0].payload.name, "demo-db");
}

// A name that parses but is invalid (blank, reserved, too long) is a 422 on every operation
// that takes one; reserved or over-long database names would otherwise hand the owner a
// system database or fail provisioning with a confusing timeout.
#[tokio::test]
async fn invalid_names_are_422_everywhere() {
    let metadata = FakeMetadata::default();
    let bound_tenant = TenantId::new();
    metadata.bind_org("org-1", bound_tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app(metadata, jobs.clone()).await;
    let token = app.token_for_org("org-1");
    for name in ["   ".to_string(), "postgres".to_string(), "template1".to_string(), "x".repeat(64)] {
        let resp = post_create(
            &app,
            Some(&token),
            serde_json::json!({ "tenant_id": bound_tenant, "project_id": ProjectId::new(), "name": name }),
        )
        .await;
        let err = error_of(resp, 422).await;
        assert_eq!(code_of(&err), "invalid_request", "database {name:?}");
    }
    assert!(jobs.published.lock().unwrap().is_empty());
    let t = branching().await;
    for name in ["", "Dev", "a b", "main"] {
        let resp = t.send(Method::POST, &t.branches(), Some(json!({ "name": name }))).await;
        let err = error_of(resp, 422).await;
        assert_eq!(code_of(&err), "invalid_request", "branch {name:?}");
    }
}

#[tokio::test]
async fn malformed_json_is_400() {
    let metadata = FakeMetadata::default();
    metadata.bind_org("org-1", TenantId::new());
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/databases", app.base))
        .header(IDENTITY_HEADER, app.token_for_org("org-1"))
        .header("content-type", "application/json")
        .body("{nope")
        .send()
        .await
        .unwrap();
    let err = error_of(resp, 400).await;
    assert_eq!(code_of(&err), "invalid_request");
}

#[tokio::test]
async fn password_resets_are_204() {
    let jobs = FakeJobs::default();
    let mut id = None;
    let app = lifecycle_app(jobs.clone(), |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 204);
    assert!(resp.bytes().await.unwrap().is_empty());
    let t = branching().await;
    let dev = t.ready("dev");
    let resp = t.send(Method::POST, &format!("{}/reset-password", t.branch(dev)), None).await;
    assert_eq!(resp.status(), 204);
    assert!(resp.bytes().await.unwrap().is_empty());
}

#[tokio::test]
async fn duplicate_database_name_is_409() {
    let metadata = FakeMetadata::default();
    let bound_tenant = TenantId::new();
    metadata.bind_org("org-1", bound_tenant);
    metadata.fail_next_insert();
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let token = app.token_for_org("org-1");
    let resp = post_create(&app, Some(&token), create_body(bound_tenant)).await;
    assert_eq!(resp.status(), 409);
}

#[tokio::test]
async fn get_database_own_tenant_is_200() {
    let metadata = FakeMetadata::default();
    let bound_tenant = TenantId::new();
    metadata.bind_org("org-1", bound_tenant);
    let record = sample_database(bound_tenant);
    let database_id = record.database_id;
    metadata.insert_database(record);
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let token = app.token_for_org("org-1");

    let resp = reqwest::Client::new()
        .get(format!("{}/v1/databases/{}", app.base, database_id.as_uuid()))
        .header(IDENTITY_HEADER, token)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["tenant_id"], serde_json::json!(bound_tenant));
}

#[tokio::test]
async fn other_tenants_database_is_404_like_a_missing_one() {
    let metadata = FakeMetadata::default();
    metadata.bind_org("org-1", TenantId::new());
    let record = sample_database(TenantId::new());
    let database_id = record.database_id;
    metadata.insert_database(record);
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let token = app.token_for_org("org-1");

    let resp = reqwest::Client::new()
        .get(format!("{}/v1/databases/{}", app.base, database_id.as_uuid()))
        .header(IDENTITY_HEADER, token)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(code_of(&body), "not_found");
    // Byte for byte what a database that doesn't exist answers.
    let missing = reqwest::Client::new()
        .get(format!("{}/v1/databases/{}", app.base, uuid::Uuid::new_v4()))
        .header(IDENTITY_HEADER, app.token_for_org("org-1"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 404);
    let missing_body: serde_json::Value = missing.json().await.unwrap();
    assert_eq!(body, missing_body);
}

#[tokio::test]
async fn get_database_unknown_id_is_404() {
    let metadata = FakeMetadata::default();
    metadata.bind_org("org-1", TenantId::new());
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let token = app.token_for_org("org-1");

    let resp = reqwest::Client::new()
        .get(format!("{}/v1/databases/{}", app.base, uuid::Uuid::new_v4()))
        .header(IDENTITY_HEADER, token)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 404);
}

fn overview_with_database(tenant_id: TenantId) -> TenantOverview {
    let project_id = ProjectId::new();
    let mut db = sample_database(tenant_id);
    db.project_id = project_id;
    db.name = "web1".into();
    TenantOverview {
        id: tenant_id,
        slug: "demo".into(),
        projects: vec![ProjectOverview {
            id: project_id,
            name: "Default".into(),
            slug: "default".into(),
            status: ProjectStatus::Active,
            databases: vec![db.record],
        }],
    }
}

async fn get_me(app: &TestApp, identity_header: Option<&str>) -> reqwest::Response {
    let mut req = reqwest::Client::new().get(format!("{}/v1/me", app.base));
    if let Some(token) = identity_header {
        req = req.header(IDENTITY_HEADER, token);
    }
    req.send().await.unwrap()
}

#[tokio::test]
async fn me_for_bound_org_returns_tenant_projects_and_databases() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    metadata.set_overview(overview_with_database(tenant));
    let app = spawn_app(metadata, FakeJobs::default()).await;

    let resp = get_me(&app, Some(&app.token_for_org("org-1"))).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["user"]["sub"], "user-1");
    assert_eq!(body["user"]["org_id"], "org-1");
    assert_eq!(body["tenant"]["id"], serde_json::json!(tenant));
    assert_eq!(body["tenant"]["slug"], "demo");
    let project = &body["tenant"]["projects"][0];
    assert_eq!(project["slug"], "default");
    let db = &project["databases"][0];
    assert_eq!(db["name"], "web1");
    assert_eq!(db["status"], "ready");
    assert_eq!(db["host"], "ep-0123456789ab.localtest.me");
    assert_eq!(db["port"], 4432);
    assert!(db["database_id"].is_string());
}

#[tokio::test]
async fn me_for_unbound_org_returns_null_tenant() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let resp = get_me(&app, Some(&app.token_for_org("org-unbound"))).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["user"]["org_id"], "org-unbound");
    assert!(body["tenant"].is_null());
}

// Review Focus 2.
#[tokio::test]
async fn me_for_bound_org_with_missing_tenant_row_returns_null_tenant() {
    let metadata = FakeMetadata::default();
    metadata.bind_org("org-1", TenantId::new());
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let resp = get_me(&app, Some(&app.token_for_org("org-1"))).await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["tenant"].is_null());
}

#[tokio::test]
async fn me_without_identity_is_401() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    assert_eq!(get_me(&app, None).await.status(), 401);
}

#[tokio::test]
async fn me_never_exposes_database_secrets() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    metadata.set_overview(overview_with_database(tenant));
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let text = get_me(&app, Some(&app.token_for_org("org-1")))
        .await
        .text()
        .await
        .unwrap();
    for secret in [
        "secret-owner-sealed",
        "owner_password_enc",
        "endpoint_id",
    ] {
        assert!(!text.contains(secret), "/v1/me leaked {secret}: {text}");
    }
}

#[tokio::test]
async fn get_deleted_database_is_404() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let mut record = sample_database(tenant);
    record.status = DatabaseStatus::Deleted;
    let id = record.database_id;
    metadata.insert_database(record);
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let resp = reqwest::Client::new()
        .get(format!("{}/v1/databases/{}", app.base, id.as_uuid()))
        .header(IDENTITY_HEADER, app.token_for_org("org-1"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

// GET /v1/databases/{id} never returns credentials or container ids.
#[tokio::test]
async fn get_database_never_exposes_secrets() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let db = overview_with_database(tenant).projects[0].databases[0].clone();
    let id = db.database_id;
    metadata.insert_database(db);
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let text = reqwest::Client::new()
        .get(format!("{}/v1/databases/{}", app.base, id.as_uuid()))
        .header(IDENTITY_HEADER, app.token_for_org("org-1"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for secret in ["secret-owner-sealed", "password", "endpoint_id"] {
        assert!(!text.contains(secret), "GET leaked {secret}: {text}");
    }
}

// --- Lifecycle: reveal, reset, delete ----------------------------------------

/// A ready database with sealed credentials (`s3cret`) under `app.key`.
fn ready_database(tenant: TenantId, key: &SecretKey) -> Db {
    let mut db = sample_database(tenant);
    db.name = "web1".into();
    db.database_name = Some("web1".into());
    db.owner_role = Some("postgres".into());
    db.main.owner_password_enc =
        Some(seal_credential(key, &db.main.id.to_string(), Purpose::Owner, "s3cret"));
    db
}

async fn call(app: &TestApp, method: reqwest::Method, path: &str, org: &str) -> reqwest::Response {
    reqwest::Client::new()
        .request(method, format!("{}{path}", app.base))
        .header(IDENTITY_HEADER, app.token_for_org(org))
        .send()
        .await
        .unwrap()
}

/// App with org-1 bound to a tenant; `setup` gets the tenant and key to insert rows.
async fn lifecycle_app(jobs: FakeJobs, setup: impl FnOnce(&FakeMetadata, TenantId, &SecretKey)) -> TestApp {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let app = spawn_app(metadata.clone(), jobs).await;
    setup(&metadata, tenant, &app.key);
    app
}

#[tokio::test]
async fn reveal_ready_database_returns_decrypted_owner_credentials() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}/connection", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["user"], "postgres");
    assert_eq!(body["password"], "s3cret");
    assert_eq!(body["database"], "web1");
    assert_eq!(body["host"], "ep-0123456789ab.localtest.me");
    assert_eq!(body["port"], 4432);
    assert_eq!(
        body["uri"],
        "postgresql://postgres:s3cret@ep-0123456789ab.localtest.me:4432/web1?sslmode=verify-full"
    );
    assert_eq!(body["http_url"], "https://ep-0123456789ab.localtest.me:4444/sql");
}

async fn reveal_in_mode(mode: Mode) -> serde_json::Value {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let app = spawn_app_in_mode(metadata.clone(), FakeJobs::default(), "http://127.0.0.1:9", mode).await;
    let db = ready_database(tenant, &app.key);
    let id = db.database_id;
    metadata.insert_database(db);
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}/connection", id.as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 200);
    resp.json().await.unwrap()
}

// C5: self-hosted mode names the dev CA's file; the URI is unchanged.
#[tokio::test]
async fn reveal_carries_the_ca_file_in_self_hosted_mode() {
    let body = reveal_in_mode(Mode::SelfHosted).await;
    assert_eq!(body["ca_file"], ".chelabase/neon/proxy-ca.crt");
    assert!(body["uri"].as_str().unwrap().ends_with("?sslmode=verify-full"));
}

#[tokio::test]
async fn reveal_has_no_ca_file_in_platform_mode() {
    let body = reveal_in_mode(Mode::Platform).await;
    assert!(body["ca_file"].is_null(), "{body}");
    assert!(body["uri"].as_str().unwrap().ends_with("?sslmode=verify-full"));
}

// Review fix: a replaced secrets key gets an explicit message, not a bare 500.
#[tokio::test]
async fn reveal_with_a_different_secrets_key_explains_the_key_mismatch() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, _k| {
        let db = ready_database(t, &SecretKey::generate());
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}/connection", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 500);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(code_of(&body), "credentials_unavailable");
}

#[tokio::test]
async fn reveal_without_an_endpoint_is_409() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.main.endpoint_id = None;
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}/connection", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(code_of(&body), "credentials_unavailable");
}

// /v1/me and GET show the proxy host and port when an endpoint is linked, nothing otherwise.
#[tokio::test]
async fn summaries_show_the_proxy_host() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let overview = overview_with_database(tenant);
    let mut bare = sample_database(tenant);
    bare.main.endpoint_id = None;
    bare.project_id = overview.projects[0].id;
    let (linked_id, bare_id) = (overview.projects[0].databases[0].database_id, bare.database_id);
    metadata.insert_database(overview.projects[0].databases[0].clone());
    metadata.insert_database(bare.clone());
    let mut overview = overview;
    overview.projects[0].databases.push(bare.record);
    metadata.set_overview(overview);
    let app = spawn_app(metadata, FakeJobs::default()).await;

    let me: serde_json::Value = get_me(&app, Some(&app.token_for_org("org-1"))).await.json().await.unwrap();
    let dbs = &me["tenant"]["projects"][0]["databases"];
    assert_eq!(dbs[0]["host"], "ep-0123456789ab.localtest.me");
    assert_eq!(dbs[0]["port"], 4432);
    assert!(dbs[1]["host"].is_null() && dbs[1]["port"].is_null());

    for (id, host, port) in [(linked_id, serde_json::json!("ep-0123456789ab.localtest.me"), serde_json::json!(4432)), (bare_id, serde_json::Value::Null, serde_json::Value::Null)] {
        let body: serde_json::Value = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}", id.as_uuid()), "org-1").await.json().await.unwrap();
        assert_eq!(body["host"], host);
        assert_eq!(body["port"], port);
    }
}

// A linked endpoint isn't reachable until the database is ready: no host or port before.
#[tokio::test]
async fn summaries_hide_the_proxy_host_until_ready() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let mut overview = overview_with_database(tenant);
    let db = &mut overview.projects[0].databases[0];
    db.status = DatabaseStatus::Provisioning;
    assert!(main_for(db).endpoint_id.is_some());
    let id = db.database_id;
    metadata.insert_database(db.clone());
    metadata.set_overview(overview);
    let app = spawn_app(metadata, FakeJobs::default()).await;

    let me: serde_json::Value = get_me(&app, Some(&app.token_for_org("org-1"))).await.json().await.unwrap();
    let summary = &me["tenant"]["projects"][0]["databases"][0];
    assert_eq!(summary["status"], "provisioning");
    assert!(summary["host"].is_null() && summary["port"].is_null(), "{summary}");

    let body: serde_json::Value = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}", id.as_uuid()), "org-1").await.json().await.unwrap();
    assert_eq!(body["status"], "provisioning");
    assert!(body["host"].is_null() && body["port"].is_null(), "{body}");
}

// Main's branch row holds the database's endpoint and sealed password now: reveal reads both
// from it, and the password opens with main's id (the database's) as AAD.
#[tokio::test]
async fn reveal_uses_main_branch_row() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.main.endpoint_id = Some("ep-aaaaaaaaaaaa".into());
        db.main.owner_password_enc = Some(seal_credential(
            k,
            &BranchId::from(db.database_id).to_string(),
            Purpose::Owner,
            "from-main",
        ));
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let id = id.unwrap().as_uuid();
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{id}/connection"), "org-1").await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["password"], "from-main");
    assert_eq!(body["host"], "ep-aaaaaaaaaaaa.localtest.me");
    assert_eq!(
        body["uri"],
        "postgresql://postgres:from-main@ep-aaaaaaaaaaaa.localtest.me:4432/web1?sslmode=verify-full"
    );
    // GET shows main's host too.
    let body: serde_json::Value = call(&app, reqwest::Method::GET, &format!("/v1/databases/{id}"), "org-1").await.json().await.unwrap();
    assert_eq!(body["host"], "ep-aaaaaaaaaaaa.localtest.me");
}

// No main branch row at all: nothing to reveal or reset.
#[tokio::test]
async fn reveal_and_reset_without_a_main_branch_are_409() {
    let jobs = FakeJobs::default();
    let mut id = None;
    let app = lifecycle_app(jobs.clone(), |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
        m.branches.lock().unwrap().clear();
    })
    .await;
    let id = id.unwrap().as_uuid();
    for (method, path) in [
        (reqwest::Method::GET, format!("/v1/databases/{id}/connection")),
        (reqwest::Method::POST, format!("/v1/databases/{id}/reset-password")),
    ] {
        let resp = call(&app, method, &path, "org-1").await;
        assert_eq!(resp.status(), 409, "{path}");
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(code_of(&body), "credentials_unavailable");
    }
    assert!(jobs.resets.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reveal_not_ready_is_409() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.status = DatabaseStatus::Provisioning;
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}/connection", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 409);
}

// Review Focus 2.
#[tokio::test]
async fn reveal_without_a_sealed_owner_password_says_set_in_sql() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.main.owner_password_enc = None;
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}/connection", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["password"].is_null(), "{body}");
    assert_eq!(body["password_state"], "set_in_sql");
    assert_eq!(body["uri"], "postgresql://postgres@ep-0123456789ab.localtest.me:4432/web1?sslmode=verify-full");
}

#[tokio::test]
async fn reveal_other_tenant_is_404() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, _t, k| {
        let db = ready_database(TenantId::new(), k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}/connection", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn reveal_deleted_is_404() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.status = DatabaseStatus::Deleted;
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::GET, &format!("/v1/databases/{}/connection", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn reset_password_ok_is_204() {
    let jobs = FakeJobs::default();
    let mut id = None;
    let app = lifecycle_app(jobs.clone(), |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 204);
    assert_eq!(*jobs.resets.lock().unwrap(), vec![(id.unwrap(), BranchId::from(id.unwrap()))]);
}

#[tokio::test]
async fn reset_password_not_ready_is_409_without_asking_the_worker() {
    let jobs = FakeJobs::default();
    let mut id = None;
    let app = lifecycle_app(jobs.clone(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.status = DatabaseStatus::Pending;
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 409);
    assert!(jobs.resets.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reset_password_worker_says_not_ready_is_409() {
    let jobs = FakeJobs::default();
    jobs.reset_answers(FakeReset::NotReady);
    let mut id = None;
    let app = lifecycle_app(jobs, |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 409);
}

// Review Focus 5.
#[tokio::test]
async fn reset_password_worker_timeout_is_504() {
    let jobs = FakeJobs::default();
    jobs.reset_answers(FakeReset::Timeout);
    let mut id = None;
    let app = lifecycle_app(jobs, |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 504);
}

#[tokio::test]
async fn reset_password_worker_deadline_is_504() {
    let jobs = FakeJobs::default();
    jobs.reset_answers(FakeReset::WorkerTimedOut);
    let mut id = None;
    let app = lifecycle_app(jobs, |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 504);
}

#[tokio::test]
async fn reset_password_with_no_worker_is_503() {
    let jobs = FakeJobs::default();
    jobs.reset_answers(FakeReset::Unavailable);
    let mut id = None;
    let app = lifecycle_app(jobs, |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 503);
}

// Databases with no endpoint (never provisioned on Neon) can't be reset.
#[tokio::test]
async fn reset_without_an_endpoint_is_409() {
    let jobs = FakeJobs::default();
    let mut id = None;
    let app = lifecycle_app(jobs.clone(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.main.endpoint_id = None;
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 409);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(code_of(&body), "credentials_unavailable");
    assert!(jobs.resets.lock().unwrap().is_empty(), "worker must not be asked");
}

#[tokio::test]
async fn reset_not_applied_is_503_with_the_message() {
    let jobs = FakeJobs::default();
    jobs.reset_answers(FakeReset::NotApplied);
    let mut id = None;
    let app = lifecycle_app(jobs, |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::POST, &format!("/v1/databases/{}/reset-password", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 503);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(code_of(&body), "not_applied");
    assert_eq!(message_of(&body), RESET_NOT_APPLIED);
}

#[tokio::test]
async fn delete_ready_database_is_202_and_publishes_job() {
    let jobs = FakeJobs::default();
    let mut id = None;
    let app = lifecycle_app(jobs.clone(), |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::DELETE, &format!("/v1/databases/{}", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 202);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "deleting");
    assert_eq!(*jobs.deleted.lock().unwrap(), vec![id.unwrap()]);
}

#[tokio::test]
async fn delete_twice_is_409() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let db = ready_database(t, k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let path = format!("/v1/databases/{}", id.unwrap().as_uuid());
    assert_eq!(call(&app, reqwest::Method::DELETE, &path, "org-1").await.status(), 202);
    assert_eq!(call(&app, reqwest::Method::DELETE, &path, "org-1").await.status(), 409);
}

#[tokio::test]
async fn delete_deleted_is_404() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, t, k| {
        let mut db = ready_database(t, k);
        db.status = DatabaseStatus::Deleted;
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::DELETE, &format!("/v1/databases/{}", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn delete_other_tenant_is_404() {
    let jobs = FakeJobs::default();
    let mut id = None;
    let app = lifecycle_app(jobs.clone(), |m, _t, k| {
        let db = ready_database(TenantId::new(), k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = call(&app, reqwest::Method::DELETE, &format!("/v1/databases/{}", id.unwrap().as_uuid()), "org-1").await;
    assert_eq!(resp.status(), 404);
    assert!(jobs.deleted.lock().unwrap().is_empty());
}

/// Every endpoint `chelabase-routes` assigns to the orchestrator must be routed
/// by the orchestrator's own router. A 401 still proves the route exists; only
/// 404 (unknown path) and 405 (wrong method) mean drift.
#[tokio::test]
async fn every_declared_orchestrator_endpoint_is_routed() {
    let app = spawn_app(FakeMetadata::default(), FakeJobs::default()).await;
    let client = reqwest::Client::new();
    let declared: Vec<_> = ENDPOINTS
        .iter()
        .filter(|ep| ep.service == Service::Orchestrator)
        .collect();
    assert!(!declared.is_empty());

    for ep in declared {
        let method = reqwest::Method::from_bytes(ep.method.as_str().as_bytes()).unwrap();
        let url = format!("{}{}", app.base, fill_params(ep.path));
        let status = client.request(method, &url).send().await.unwrap().status();
        assert!(
            status != 404 && status != 405,
            "{} {} is declared in chelabase-routes but the orchestrator answered {status}",
            ep.method.as_str(),
            ep.path
        );
    }
}

fn fill_params(template: &str) -> String {
    template
        .split('/')
        .map(|segment| {
            if segment.starts_with('{') {
                uuid::Uuid::nil().to_string()
            } else {
                segment.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

// --- Branches ------------------------------------------------------------------

use reqwest::Method;
use serde_json::{json, Value};

struct Branching {
    app: TestApp,
    metadata: FakeMetadata,
    jobs: FakeJobs,
    db: DatabaseId,
}

impl Branching {
    fn main(&self) -> BranchId {
        BranchId::from(self.db)
    }

    fn add(&self, name: &str, parent: BranchId, kind: BranchKind, status: BranchStatus) -> BranchId {
        self.metadata.add_branch(self.db, name, parent, kind, status)
    }

    fn ready(&self, name: &str) -> BranchId {
        self.add(name, self.main(), BranchKind::Normal, BranchStatus::Ready)
    }

    fn backup(&self, of: BranchId) -> BranchId {
        self.add("dev-before-restore-20260101T000000Z", of, BranchKind::Backup, BranchStatus::Ready)
    }

    fn branches(&self) -> String {
        format!("/v1/databases/{}/branches", self.db.as_uuid())
    }

    fn branch(&self, id: BranchId) -> String {
        format!("{}/{}", self.branches(), id.as_uuid())
    }

    async fn send(&self, method: Method, path: &str, body: Option<Value>) -> reqwest::Response {
        let mut req = reqwest::Client::new()
            .request(method, format!("{}{path}", self.app.base))
            .header(IDENTITY_HEADER, self.app.token_for_org("org-1"));
        if let Some(body) = body {
            req = req.json(&body);
        }
        req.send().await.unwrap()
    }

    fn published(&self) -> Vec<BranchJob> {
        self.jobs.branch_jobs.lock().unwrap().iter().map(|j| j.payload.clone()).collect()
    }
}

/// A ready database `web1` for org-1 (max 5 branches, 7 day window) with sealed password `s3cret`.
async fn branching() -> Branching {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app(metadata.clone(), jobs.clone()).await;
    let db = ready_database(tenant, &app.key);
    let id = db.database_id;
    metadata.insert_database(db);
    Branching { app, metadata, jobs, db: id }
}

/// The error body of a response with the expected status.
async fn error_of(resp: reqwest::Response, status: u16) -> Value {
    assert_eq!(resp.status(), status);
    resp.json().await.unwrap()
}

fn code_of(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or_else(|| panic!("no error code in {body}"))
}

fn message_of(body: &Value) -> &str {
    body["error"]["message"].as_str().unwrap_or_else(|| panic!("no error message in {body}"))
}

// ---- list

#[tokio::test]
async fn list_branches_returns_the_tree_with_hosts_only_for_ready_normal_branches() {
    let t = branching().await;
    let dev = t.ready("dev");
    let pending = t.add("wip", dev, BranchKind::Normal, BranchStatus::Pending);
    let backup = t.backup(dev);
    let resp = t.send(Method::GET, &t.branches(), None).await;
    assert_eq!(resp.status(), 200);
    let list: Vec<Value> = resp.json().await.unwrap();
    assert_eq!(list.len(), 4);
    let by = |id: BranchId| list.iter().find(|b| b["id"] == json!(id)).unwrap_or_else(|| panic!("{id} missing"));
    let main = by(t.main());
    assert_eq!(main["name"], "main");
    assert!(main["parent_branch_id"].is_null());
    assert_eq!(main["kind"], "normal");
    assert_eq!(main["status"], "ready");
    assert_eq!(main["host"], "ep-0123456789ab.localtest.me");
    assert_eq!(main["port"], 4432);
    let dev_json = by(dev);
    assert_eq!(dev_json["parent_branch_id"], json!(t.main()));
    assert!(dev_json["host"].as_str().unwrap().ends_with(".localtest.me"));
    assert_eq!(dev_json["port"], 4432);
    for id in [pending, backup] {
        assert!(by(id)["host"].is_null() && by(id)["port"].is_null(), "{}", by(id));
    }
    assert_eq!(by(backup)["kind"], "backup");
    assert_eq!(by(pending)["status"], "pending");
    for key in ["id", "name", "parent_branch_id", "kind", "status", "host", "port", "branched_at_time", "branched_at_lsn", "created_at", "error"] {
        assert!(main.get(key).is_some(), "missing {key}");
    }
    let text = serde_json::to_string(&list).unwrap();
    for secret in ["secret-owner-sealed", "owner_password", "endpoint_id", "timeline"] {
        assert!(!text.contains(secret), "leaked {secret}");
    }
}

#[tokio::test]
async fn list_branches_carries_where_each_history_begins() {
    let t = branching().await;
    let dev = t.ready("dev");
    let backup = t.backup(dev);
    let start: chrono::DateTime<chrono::Utc> = "2026-09-20T09:00:00Z".parse().unwrap();
    for id in [dev, backup] {
        t.metadata.branches.lock().unwrap().get_mut(&id).unwrap().history_start = Some(start);
    }
    let resp = t.send(Method::GET, &t.branches(), None).await;
    assert_eq!(resp.status(), 200);
    let list: Vec<Value> = resp.json().await.unwrap();
    let by = |id: BranchId| list.iter().find(|b| b["id"] == json!(id)).unwrap();
    assert_eq!(by(dev)["history_start"], "2026-09-20T09:00:00Z");
    assert_eq!(by(backup)["history_start"], "2026-09-20T09:00:00Z");
    // Unknown (a row from before it was recorded): null, never missing.
    let main = by(t.main());
    assert!(main.get("history_start").is_some_and(Value::is_null), "{main}");
    assert!(!serde_json::to_string(&list).unwrap().contains("pending_timeline_at"));
}

#[tokio::test]
async fn list_branches_hides_deleted_and_refuses_strangers() {
    let t = branching().await;
    t.add("gone", t.main(), BranchKind::Normal, BranchStatus::Deleted);
    let list: Vec<Value> = t.send(Method::GET, &t.branches(), None).await.json().await.unwrap();
    assert_eq!(list.len(), 1);
    let unknown = format!("/v1/databases/{}/branches", uuid::Uuid::new_v4());
    assert_eq!(t.send(Method::GET, &unknown, None).await.status(), 404);

    let mut other_id = None;
    let other = lifecycle_app(FakeJobs::default(), |m, _t, k| {
        let db = ready_database(TenantId::new(), k);
        other_id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let path = format!("/v1/databases/{}/branches", other_id.unwrap().as_uuid());
    assert_eq!(call(&other, Method::GET, &path, "org-1").await.status(), 404);
}


// ---- create

#[tokio::test]
async fn create_branch_is_202_with_the_pending_branch_and_a_job() {
    let t = branching().await;
    let resp = t.send(Method::POST, &t.branches(), Some(json!({ "name": "dev" }))).await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["name"], "dev");
    assert_eq!(body["status"], "pending");
    assert_eq!(body["kind"], "normal");
    assert_eq!(body["parent_branch_id"], json!(t.main()));
    assert!(body["host"].is_null());
    let id = BranchId(body["id"].as_str().unwrap().parse().unwrap());
    assert_eq!(t.metadata.branch(id).status, BranchStatus::Pending);
    assert_eq!(t.published(), vec![BranchJob::Create { branch_id: id, at: None }]);
}

#[tokio::test]
async fn create_branch_takes_a_parent_and_a_point() {
    let t = branching().await;
    let dev = t.ready("dev");
    let resp = t
        .send(Method::POST, &t.branches(), Some(json!({ "name": "feat", "parent_branch_id": dev, "at": { "lsn": "0/16B3748" } })))
        .await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    let id = BranchId(body["id"].as_str().unwrap().parse().unwrap());
    assert_eq!(body["parent_branch_id"], json!(dev));
    assert_eq!(body["branched_at_lsn"], "0/16B3748");
    assert_eq!(t.published(), vec![BranchJob::Create { branch_id: id, at: Some(PointInTime::Lsn("0/16B3748".into())) }]);

    let resp = t
        .send(Method::POST, &t.branches(), Some(json!({ "name": "old", "at": { "timestamp": "2026-09-30T12:00:00Z" } })))
        .await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    let at: chrono::DateTime<chrono::Utc> = "2026-09-30T12:00:00Z".parse().unwrap();
    assert_eq!(t.metadata.branch(BranchId(body["id"].as_str().unwrap().parse().unwrap())).branched_at_time, Some(at));
    assert!(matches!(t.published().last(), Some(BranchJob::Create { at: Some(PointInTime::Timestamp(x)), .. }) if *x == at));
}

#[tokio::test]
async fn create_branch_from_a_restore_backup_is_202() {
    let t = branching().await;
    let dev = t.ready("dev");
    let backup = t.backup(dev);
    let resp = t
        .send(Method::POST, &t.branches(), Some(json!({ "name": "from-backup", "parent_branch_id": backup })))
        .await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    let id = BranchId(body["id"].as_str().unwrap().parse().unwrap());
    assert_eq!(body["parent_branch_id"], json!(backup));
    assert_eq!(body["kind"], "normal");
    assert_eq!(t.metadata.branch(id).parent_branch_id, Some(backup));
    assert_eq!(t.published(), vec![BranchJob::Create { branch_id: id, at: None }]);
}

#[tokio::test]
async fn create_branch_with_a_taken_name_is_409() {
    let t = branching().await;
    t.ready("dev");
    let err = error_of(t.send(Method::POST, &t.branches(), Some(json!({ "name": "dev" }))).await, 409).await;
    assert_eq!(code_of(&err), "name_taken");
    let err = error_of(t.send(Method::POST, &t.branches(), Some(json!({ "name": "main" }))).await, 422).await;
    assert_eq!(code_of(&err), "invalid_request");
    assert!(t.published().is_empty());
}

#[tokio::test]
async fn create_branch_at_the_cap_is_409_naming_the_cap_and_the_patch() {
    let t = branching().await;
    t.metadata.databases.lock().unwrap().get_mut(&t.db).unwrap().max_branches = 1;
    t.ready("dev");
    let err = error_of(t.send(Method::POST, &t.branches(), Some(json!({ "name": "b" }))).await, 409).await;
    assert_eq!(code_of(&err), "branch_cap_reached");
    assert!(message_of(&err).contains("max_branches"), "{err}");
    assert!(t.published().is_empty());
}

#[tokio::test]
async fn create_branch_needs_a_ready_parent_that_exists() {
    let t = branching().await;
    let pending = t.add("p", t.main(), BranchKind::Normal, BranchStatus::Pending);
    let err = error_of(t.send(Method::POST, &t.branches(), Some(json!({ "name": "x", "parent_branch_id": pending }))).await, 409).await;
    assert_eq!(code_of(&err), "branch_not_ready");
    let unknown = BranchId::new();
    assert_eq!(t.send(Method::POST, &t.branches(), Some(json!({ "name": "x", "parent_branch_id": unknown }))).await.status(), 404);
    let deleted = t.add("d", t.main(), BranchKind::Normal, BranchStatus::Deleted);
    assert_eq!(t.send(Method::POST, &t.branches(), Some(json!({ "name": "x", "parent_branch_id": deleted }))).await.status(), 404);
    assert!(t.published().is_empty());
}

#[tokio::test]
async fn create_branch_on_a_database_that_is_not_ready_is_409() {
    let t = branching().await;
    t.metadata.branches.lock().unwrap().get_mut(&t.main()).unwrap().status = BranchStatus::Provisioning;
    let err = error_of(t.send(Method::POST, &t.branches(), Some(json!({ "name": "x" }))).await, 409).await;
    assert_eq!(code_of(&err), "branch_not_ready");
}

#[tokio::test]
async fn create_branch_rejects_bad_input_with_422() {
    let t = branching().await;
    let long = "a".repeat(64);
    for name in ["", "Dev", "-x", "_x", "a b", "a.b", long.as_str(), "main"] {
        let resp = t.send(Method::POST, &t.branches(), Some(json!({ "name": name }))).await;
        assert_eq!(resp.status(), 422, "name {name:?}");
    }
    for at in [
        json!({ "lsn": "zzz" }),
        json!({ "lsn": "0/" }),
        json!({ "lsn": "1234" }),
        json!({ "lsn": "" }),
        json!({ "timestamp": "yesterday" }),
        json!({ "timestamp": "2026-09-30" }),
        json!({}),
        json!({ "lsn": "0/1", "timestamp": "2026-09-30T12:00:00Z" }),
    ] {
        let resp = t.send(Method::POST, &t.branches(), Some(json!({ "name": "ok", "at": at }))).await;
        assert_eq!(resp.status(), 422, "at {at}");
    }
    // Longest allowed name and the character set.
    for name in ["a".repeat(63), "0-a_b".to_string()] {
        assert_eq!(t.send(Method::POST, &t.branches(), Some(json!({ "name": name }))).await.status(), 202, "{name}");
    }
    assert_eq!(t.published().len(), 2);
}

#[tokio::test]
async fn create_branch_publish_failure_is_503_and_fails_the_branch() {
    let t = branching().await;
    *t.jobs.fail_branch_publish.lock().unwrap() = true;
    assert_eq!(t.send(Method::POST, &t.branches(), Some(json!({ "name": "dev" }))).await.status(), 503);
    let branches = t.metadata.branches.lock().unwrap();
    let dev = branches.values().find(|b| b.name == "dev").unwrap();
    assert_eq!(dev.status, BranchStatus::Failed);
    assert!(dev.error.as_deref().unwrap().contains("publish"));
}

#[tokio::test]
async fn create_branch_on_another_tenants_database_is_404_like_an_unknown_one() {
    let jobs = FakeJobs::default();
    let mut id = None;
    let app = lifecycle_app(jobs.clone(), |m, _t, k| {
        let db = ready_database(TenantId::new(), k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let path = format!("/v1/databases/{}/branches", id.unwrap().as_uuid());
    let resp = reqwest::Client::new()
        .post(format!("{}{path}", app.base))
        .header(IDENTITY_HEADER, app.token_for_org("org-1"))
        .json(&json!({ "name": "dev" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let resp = reqwest::Client::new()
        .post(format!("{}/v1/databases/{}/branches", app.base, uuid::Uuid::new_v4()))
        .header(IDENTITY_HEADER, app.token_for_org("org-1"))
        .json(&json!({ "name": "dev" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    assert!(jobs.branch_jobs.lock().unwrap().is_empty());
}

// ---- connection and reset-password

#[tokio::test]
async fn branch_connection_reveals_the_branchs_own_password() {
    let t = branching().await;
    let dev = t.ready("dev");
    let sealed = seal_credential(&t.app.key, &dev.to_string(), Purpose::Owner, "dev-pw");
    t.metadata.branches.lock().unwrap().get_mut(&dev).unwrap().owner_password_enc = Some(sealed);
    let ep = t.metadata.branch(dev).endpoint_id.unwrap();
    let resp = t.send(Method::GET, &format!("{}/connection", t.branch(dev)), None).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["password"], "dev-pw");
    assert_eq!(body["host"], format!("{ep}.localtest.me"));
    assert_eq!(body["port"], 4432);
    assert_eq!(body["database"], "web1");
    assert_eq!(body["user"], "postgres");
    assert_eq!(body["uri"], format!("postgresql://postgres:dev-pw@{ep}.localtest.me:4432/web1?sslmode=verify-full"));
    assert_eq!(body["http_url"], format!("https://{ep}.localtest.me:4444/sql"));
}

#[tokio::test]
async fn branch_connection_and_reset_refuse_backups_unready_and_unknown_branches() {
    let t = branching().await;
    let dev = t.ready("dev");
    let backup = t.backup(dev);
    let pending = t.add("p", t.main(), BranchKind::Normal, BranchStatus::Pending);
    let resetting = t.add("r", t.main(), BranchKind::Normal, BranchStatus::Resetting);
    let gone = t.add("g", t.main(), BranchKind::Normal, BranchStatus::Deleted);
    for (method, suffix) in [(Method::GET, "connection"), (Method::POST, "reset-password")] {
        let err = error_of(t.send(method.clone(), &format!("{}/{suffix}", t.branch(backup)), None).await, 409).await;
        assert_eq!(code_of(&err), "backup_branch");
        for id in [pending, resetting] {
            let err = error_of(t.send(method.clone(), &format!("{}/{suffix}", t.branch(id)), None).await, 409).await;
            assert_eq!(code_of(&err), "branch_not_ready");
        }
        for id in [BranchId::new(), gone] {
            assert_eq!(t.send(method.clone(), &format!("{}/{suffix}", t.branch(id)), None).await.status(), 404);
        }
    }
    assert!(t.jobs.resets.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_branch_of_another_database_is_404_under_this_one() {
    let t = branching().await;
    let other = DatabaseId::new();
    let foreign = t.metadata.add_branch(other, "x", BranchId::from(other), BranchKind::Normal, BranchStatus::Ready);
    for (method, suffix) in [
        (Method::GET, "/connection"),
        (Method::POST, "/reset-password"),
        (Method::POST, "/reset-to-parent"),
        (Method::POST, "/restore"),
        (Method::DELETE, ""),
    ] {
        let resp = t.send(method, &format!("{}{suffix}", t.branch(foreign)), Some(json!({ "at": { "lsn": "0/1" } }))).await;
        assert_eq!(resp.status(), 404, "{suffix}");
    }
}

#[tokio::test]
async fn branch_reset_password_asks_the_worker_for_that_branch() {
    let t = branching().await;
    let dev = t.ready("dev");
    let resp = t.send(Method::POST, &format!("{}/reset-password", t.branch(dev)), None).await;
    assert_eq!(resp.status(), 204);
    assert_eq!(*t.jobs.resets.lock().unwrap(), vec![(t.db, dev)]);
    t.jobs.reset_answers(FakeReset::NotApplied);
    let resp = t.send(Method::POST, &format!("{}/reset-password", t.branch(dev)), None).await;
    let body = error_of(resp, 503).await;
    assert_eq!(code_of(&body), "not_applied");
    assert_eq!(message_of(&body), RESET_NOT_APPLIED);
    t.jobs.reset_answers(FakeReset::Timeout);
    assert_eq!(t.send(Method::POST, &format!("{}/reset-password", t.branch(dev)), None).await.status(), 504);
    t.jobs.reset_answers(FakeReset::Unavailable);
    assert_eq!(t.send(Method::POST, &format!("{}/reset-password", t.branch(dev)), None).await.status(), 503);
}

#[tokio::test]
async fn old_routes_are_aliases_for_main() {
    let t = branching().await;
    let main = t.main();
    let old: Value = t.send(Method::GET, &format!("/v1/databases/{}/connection", t.db.as_uuid()), None).await.json().await.unwrap();
    let new: Value = t.send(Method::GET, &format!("{}/connection", t.branch(main)), None).await.json().await.unwrap();
    assert_eq!(old, new);
    assert_eq!(old["password"], "s3cret");
    assert_eq!(t.send(Method::POST, &format!("/v1/databases/{}/reset-password", t.db.as_uuid()), None).await.status(), 204);
    assert_eq!(t.send(Method::POST, &format!("{}/reset-password", t.branch(main)), None).await.status(), 204);
    assert_eq!(*t.jobs.resets.lock().unwrap(), vec![(t.db, main), (t.db, main)]);
}

// ---- reset to parent

#[tokio::test]
async fn reset_to_parent_is_202_and_publishes_the_job() {
    let t = branching().await;
    let dev = t.ready("dev");
    let resp = t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "resetting");
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Resetting);
    assert_eq!(t.published(), vec![BranchJob::ResetToParent { branch_id: dev }]);
    // Twice: no longer ready.
    let err = error_of(t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await, 409).await;
    assert_eq!(code_of(&err), "branch_not_ready");
    assert_eq!(t.published().len(), 1);
}

#[tokio::test]
async fn reset_and_restore_claims_clear_the_last_error() {
    let t = branching().await;
    let dev = t.ready("dev");
    let set_error = |id| t.metadata.branches.lock().unwrap().get_mut(&id).unwrap().error = Some("an old failure".into());
    set_error(dev);
    assert_eq!(t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await.status(), 202);
    assert_eq!(t.metadata.branch(dev).error, None, "each attempt starts clean");

    let qa = t.ready("qa");
    set_error(qa);
    let resp = t
        .send(Method::POST, &format!("{}/restore", t.branch(qa)), Some(json!({ "at": { "lsn": "0/16B3748" } })))
        .await;
    assert_eq!(resp.status(), 202);
    assert_eq!(t.metadata.branch(qa).error, None);
}

#[tokio::test]
async fn reset_to_parent_refusals() {
    let t = branching().await;
    let dev = t.ready("dev");
    let child = t.add("child", dev, BranchKind::Normal, BranchStatus::Ready);
    let other_child = t.add("child2", dev, BranchKind::Normal, BranchStatus::Pending);
    let backup = t.backup(child);
    let path = |id| format!("{}/reset-to-parent", t.branch(id));

    let err = error_of(t.send(Method::POST, &path(t.main()), None).await, 409).await;
    assert_eq!(code_of(&err), "main_branch");
    let err = error_of(t.send(Method::POST, &path(backup), None).await, 409).await;
    assert_eq!(code_of(&err), "backup_branch");
    let err = error_of(t.send(Method::POST, &path(other_child), None).await, 409).await;
    assert_eq!(code_of(&err), "branch_not_ready");
    let err = error_of(t.send(Method::POST, &path(dev), None).await, 409).await;
    assert_eq!(code_of(&err), "has_children");
    assert!(message_of(&err).contains("child") && message_of(&err).contains("child2"), "{err}");
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Ready, "a refused reset leaves the branch ready");
    assert_eq!(t.send(Method::POST, &path(BranchId::new()), None).await.status(), 404);
    assert!(t.published().is_empty());
}

#[tokio::test]
async fn backups_do_not_block_reset_to_parent() {
    let t = branching().await;
    let dev = t.ready("dev");
    t.backup(dev);
    assert_eq!(t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await.status(), 202);
}

#[tokio::test]
async fn reset_to_parent_publish_failure_is_503_and_the_branch_stays_ready() {
    let t = branching().await;
    let dev = t.ready("dev");
    *t.jobs.fail_branch_publish.lock().unwrap() = true;
    assert_eq!(t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await.status(), 503);
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Ready);
}

// ---- restore

#[tokio::test]
async fn restore_returns_the_backup_name() {
    let t = branching().await;
    let dev = t.ready("dev");
    let resp = t
        .send(Method::POST, &format!("{}/restore", t.branch(dev)), Some(json!({ "at": { "lsn": "0/16B3748" } })))
        .await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    let name = body["backup_name"].as_str().unwrap();
    let (prefix, stamp) = name.split_once("-before-restore-").expect(name);
    assert_eq!(prefix, "dev");
    assert!(stamp.len() == 16 && stamp.ends_with('Z') && stamp.as_bytes()[8] == b'T', "{stamp}");
    assert!(stamp[..8].bytes().all(|b| b.is_ascii_digit()) && stamp[9..15].bytes().all(|b| b.is_ascii_digit()), "{stamp}");
    let backup_id = BranchId(body["backup_branch_id"].as_str().unwrap().parse().unwrap());
    assert_eq!(body["status"], "resetting");
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Resetting);
    assert_eq!(
        t.published(),
        vec![BranchJob::Restore {
            branch_id: dev,
            backup_id,
            backup_name: name.to_string(),
            at: PointInTime::Lsn("0/16B3748".into()),
        }]
    );
}

#[tokio::test]
async fn restore_truncates_a_long_name_to_fit_63_characters() {
    let t = branching().await;
    let long = format!("a{}", "b".repeat(62));
    assert_eq!(long.len(), 63);
    let dev = t.ready(&long);
    let resp = t
        .send(Method::POST, &format!("{}/restore", t.branch(dev)), Some(json!({ "at": { "timestamp": "2026-09-30T12:00:00Z" } })))
        .await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    let name = body["backup_name"].as_str().unwrap();
    assert_eq!(name.len(), 63, "{name}");
    assert!(name.starts_with("abbb") && name.contains("-before-restore-2"), "{name}");
}

#[tokio::test]
async fn restore_works_on_main() {
    let t = branching().await;
    let resp = t
        .send(Method::POST, &format!("{}/restore", t.branch(t.main())), Some(json!({ "at": { "lsn": "0/1" } })))
        .await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert!(body["backup_name"].as_str().unwrap().starts_with("main-before-restore-"));
}

#[tokio::test]
async fn restore_refusals() {
    let t = branching().await;
    let dev = t.ready("dev");
    let child = t.add("child", dev, BranchKind::Normal, BranchStatus::Ready);
    let backup = t.backup(child);
    let pending = t.add("p", t.main(), BranchKind::Normal, BranchStatus::Provisioning);
    let path = |id| format!("{}/restore", t.branch(id));
    let at = || Some(json!({ "at": { "lsn": "0/1" } }));

    let err = error_of(t.send(Method::POST, &path(backup), at()).await, 409).await;
    assert_eq!(code_of(&err), "backup_branch");
    let err = error_of(t.send(Method::POST, &path(pending), at()).await, 409).await;
    assert_eq!(code_of(&err), "branch_not_ready");
    let err = error_of(t.send(Method::POST, &path(dev), at()).await, 409).await;
    assert_eq!(code_of(&err), "has_children");
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Ready);
    assert_eq!(t.send(Method::POST, &path(BranchId::new()), at()).await.status(), 404);
    // Bad bodies before anything changes: an empty one isn't JSON (400), the rest are 422.
    assert_eq!(t.send(Method::POST, &path(child), None).await.status(), 400);
    for body in [
        Some(json!({})),
        Some(json!({ "at": {} })),
        Some(json!({ "at": { "lsn": "nope" } })),
        Some(json!({ "at": { "timestamp": "nope" } })),
    ] {
        assert_eq!(t.send(Method::POST, &path(child), body.clone()).await.status(), 422, "{body:?}");
    }
    assert_eq!(t.metadata.branch(child).status, BranchStatus::Ready);
    assert!(t.published().is_empty());
}

// ---- delete

#[tokio::test]
async fn delete_branch_is_202_and_takes_its_backups_along() {
    let t = branching().await;
    let dev = t.ready("dev");
    let backup = t.backup(dev);
    let resp = t.send(Method::DELETE, &t.branch(dev), None).await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "deleting");
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Deleting);
    assert_eq!(t.metadata.branch(backup).status, BranchStatus::Deleting);
    assert_eq!(t.published(), vec![BranchJob::Delete { branch_id: dev }]);
    // Twice.
    let err = error_of(t.send(Method::DELETE, &t.branch(dev), None).await, 409).await;
    assert_eq!(code_of(&err), "already_deleting");
}

#[tokio::test]
async fn delete_branch_refusals() {
    let t = branching().await;
    let dev = t.ready("dev");
    t.add("kid", dev, BranchKind::Normal, BranchStatus::Provisioning);
    t.add("kid2", dev, BranchKind::Normal, BranchStatus::Failed);
    let err = error_of(t.send(Method::DELETE, &t.branch(t.main()), None).await, 409).await;
    assert_eq!(code_of(&err), "main_branch");
    let err = error_of(t.send(Method::DELETE, &t.branch(dev), None).await, 409).await;
    assert_eq!(code_of(&err), "has_children");
    assert!(message_of(&err).contains("kid") && message_of(&err).contains("kid2"), "{err}");
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Ready, "a refused delete leaves the branch ready");
    let gone = t.add("g", t.main(), BranchKind::Normal, BranchStatus::Deleted);
    assert_eq!(t.send(Method::DELETE, &t.branch(gone), None).await.status(), 404);
    assert_eq!(t.send(Method::DELETE, &t.branch(BranchId::new()), None).await.status(), 404);
    assert!(t.published().is_empty());
}

// Fix round 1, I1: a backup can be a parent. Deleting its branch would take the backup
// along, and Neon can't delete a timeline that others branch from.
#[tokio::test]
async fn delete_branch_refuses_while_a_backup_has_children() {
    let t = branching().await;
    let dev = t.ready("dev");
    let backup = t.backup(dev);
    let backup_name = t.metadata.branch(backup).name;
    t.add("x", backup, BranchKind::Normal, BranchStatus::Ready);
    let err = error_of(t.send(Method::DELETE, &t.branch(dev), None).await, 409).await;
    assert_eq!(code_of(&err), "has_children");
    assert_eq!(
        message_of(&err),
        format!("cannot delete dev: its backup {backup_name} has child branches (x); delete them first")
    );
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Ready, "released");
    assert_eq!(t.metadata.branch(backup).status, BranchStatus::Ready);
    assert!(t.published().is_empty());
}

#[tokio::test]
async fn delete_of_a_backup_or_a_branch_with_only_backups_is_allowed() {
    let t = branching().await;
    let dev = t.ready("dev");
    let backup = t.backup(dev);
    assert_eq!(t.send(Method::DELETE, &t.branch(backup), None).await.status(), 202);
    let other = t.ready("other");
    t.backup(other);
    assert_eq!(t.send(Method::DELETE, &t.branch(other), None).await.status(), 202);
}

#[tokio::test]
async fn delete_of_a_failed_branch_is_allowed() {
    let t = branching().await;
    let bad = t.add("bad", t.main(), BranchKind::Normal, BranchStatus::Failed);
    assert_eq!(t.send(Method::DELETE, &t.branch(bad), None).await.status(), 202);
}

// ---- settings

fn database_path(t: &Branching) -> String {
    format!("/v1/databases/{}", t.db.as_uuid())
}

#[tokio::test]
async fn patch_updates_the_settings_and_returns_them() {
    let t = branching().await;
    let resp = t
        .send(Method::PATCH, &database_path(&t), Some(json!({ "history_window": 86400, "max_branches": 10 })))
        .await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body,
        json!({
            "history_window": 86400, "max_branches": 10,
            "max_rows": 1000, "statement_timeout_ms": 8000, "max_connections": 5,
        })
    );
    let db = t.metadata.database(t.db);
    assert_eq!(db.history_window, std::time::Duration::from_secs(86400));
    assert_eq!(db.max_branches, 10);
    // GET shows the settings too.
    let get: Value = t.send(Method::GET, &database_path(&t), None).await.json().await.unwrap();
    assert_eq!(get["history_window"], 86400);
    assert_eq!(get["max_branches"], 10);
}

#[tokio::test]
async fn patch_publishes_settings_only_when_the_window_changed() {
    let t = branching().await;
    let path = database_path(&t);
    let published = || t.jobs.settings_jobs.lock().unwrap().clone();
    // Only the cap.
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "max_branches": 3 }))).await.status(), 200);
    assert!(published().is_empty());
    // The same window as now (7 days).
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "history_window": 7 * 24 * 3600 }))).await.status(), 200);
    assert!(published().is_empty());
    // Nothing at all.
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({}))).await.status(), 200);
    assert!(published().is_empty());
    // A different window.
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "history_window": 3600 }))).await.status(), 200);
    assert_eq!(published(), vec![t.db]);
}

#[tokio::test]
async fn patch_validates_the_ranges() {
    let t = branching().await;
    let path = database_path(&t);
    for body in [
        json!({ "history_window": 3599 }),
        json!({ "history_window": 30 * 24 * 3600 + 1 }),
        json!({ "history_window": 0 }),
        json!({ "history_window": -5 }),
        json!({ "history_window": "7d" }),
        json!({ "max_branches": -1 }),
        json!({ "max_branches": 21 }),
        json!({ "max_branches": "many" }),
        json!({ "surprise": 1 }),
    ] {
        let resp = t.send(Method::PATCH, &path, Some(body.clone())).await;
        assert_eq!(resp.status(), 422, "{body}");
    }
    assert_eq!(t.metadata.database(t.db).max_branches, 5);
    assert!(t.jobs.settings_jobs.lock().unwrap().is_empty());
    for body in [
        json!({ "history_window": 3600, "max_branches": 0 }),
        json!({ "history_window": 30 * 24 * 3600, "max_branches": 20 }),
    ] {
        assert_eq!(t.send(Method::PATCH, &path, Some(body.clone())).await.status(), 200, "{body}");
    }
}

#[tokio::test]
async fn patch_refusals() {
    let t = branching().await;
    let unknown = format!("/v1/databases/{}", uuid::Uuid::new_v4());
    assert_eq!(t.send(Method::PATCH, &unknown, Some(json!({ "max_branches": 1 }))).await.status(), 404);
    t.metadata.databases.lock().unwrap().get_mut(&t.db).unwrap().status = DatabaseStatus::Deleting;
    let err = error_of(t.send(Method::PATCH, &database_path(&t), Some(json!({ "max_branches": 1 }))).await, 409).await;
    assert_eq!(code_of(&err), "database_not_ready");
    assert!(t.jobs.settings_jobs.lock().unwrap().is_empty());
}

#[tokio::test]
async fn patch_on_another_tenants_database_is_404() {
    let mut id = None;
    let app = lifecycle_app(FakeJobs::default(), |m, _t, k| {
        let db = ready_database(TenantId::new(), k);
        id = Some(db.database_id);
        m.insert_database(db);
    })
    .await;
    let resp = reqwest::Client::new()
        .patch(format!("{}/v1/databases/{}", app.base, id.unwrap().as_uuid()))
        .header(IDENTITY_HEADER, app.token_for_org("org-1"))
        .json(&json!({ "max_branches": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

// ---- branch_count

#[tokio::test]
async fn me_and_get_database_show_branch_count_with_one_lookup_for_me() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let overview = overview_with_database(tenant);
    let id = overview.projects[0].databases[0].database_id;
    metadata.insert_database(overview.projects[0].databases[0].clone());
    metadata.set_overview(overview);
    let dev = metadata.add_branch(id, "dev", BranchId::from(id), BranchKind::Normal, BranchStatus::Ready);
    metadata.add_branch(id, "backup", dev, BranchKind::Backup, BranchStatus::Ready);
    metadata.add_branch(id, "gone", BranchId::from(id), BranchKind::Normal, BranchStatus::Deleted);
    let app = spawn_app(metadata.clone(), FakeJobs::default()).await;

    let me: Value = get_me(&app, Some(&app.token_for_org("org-1"))).await.json().await.unwrap();
    let db = &me["tenant"]["projects"][0]["databases"][0];
    assert_eq!(db["branch_count"], 2, "main and dev; backups and deleted branches don't count");
    assert_eq!(db["host"], "ep-0123456789ab.localtest.me");
    assert_eq!(metadata.main_count_queries.load(Ordering::SeqCst), 1);

    let get: Value = call(&app, Method::GET, &format!("/v1/databases/{}", id.as_uuid()), "org-1").await.json().await.unwrap();
    assert_eq!(get["branch_count"], 2);
    assert_eq!(get["host"], "ep-0123456789ab.localtest.me");
}

// ---- data API keys

impl Branching {
    fn keys(&self, branch: BranchId) -> String {
        format!("{}/keys", self.branch(branch))
    }

    /// The branch's public key, sealed for display like the worker does.
    fn seed_public_key(&self, branch: BranchId) -> (uuid::Uuid, String) {
        let generated = data_key::generate(DataKeyKind::Public);
        let id = uuid::Uuid::new_v4();
        let new = NewDataKey {
            id,
            branch_id: branch,
            kind: DataKeyKind::Public,
            role: "anon".into(),
            name: "public".into(),
            prefix: generated.prefix.clone(),
            sha256: generated.sha256,
            sealed: Some(seal_credential(&self.app.key, &id.to_string(), Purpose::DataKey, &generated.key)),
            databases: None,
            role_oid: None,
        };
        insert_key(&mut self.metadata.keys.lock().unwrap(), &new).unwrap();
        (id, generated.key)
    }

    fn seed_secret_key(&self, branch: BranchId, name: &str, role: &str) -> (uuid::Uuid, String) {
        let generated = data_key::generate(DataKeyKind::Secret);
        let id = uuid::Uuid::new_v4();
        let new = NewDataKey {
            id,
            branch_id: branch,
            kind: DataKeyKind::Secret,
            role: role.into(),
            name: name.into(),
            prefix: generated.prefix.clone(),
            sha256: generated.sha256,
            sealed: None,
            databases: None,
            role_oid: None,
        };
        insert_key(&mut self.metadata.keys.lock().unwrap(), &new).unwrap();
        (id, generated.key)
    }

    async fn list_keys(&self, branch: BranchId) -> Vec<Value> {
        let resp = self.send(Method::GET, &self.keys(branch), None).await;
        assert_eq!(resp.status(), 200);
        resp.json().await.unwrap()
    }
}

#[tokio::test]
async fn list_shows_the_public_key_in_full_and_never_secret_keys() {
    let t = branching().await;
    let (public_id, public) = t.seed_public_key(t.main());
    let (secret_id, secret) = t.seed_secret_key(t.main(), "ci", "reporting");
    let list = t.list_keys(t.main()).await;
    assert_eq!(list.len(), 2);
    let by = |id: uuid::Uuid| list.iter().find(|k| k["id"] == json!(id)).unwrap();
    let p = by(public_id);
    assert_eq!(p["kind"], "public");
    assert_eq!(p["role"], "anon");
    assert_eq!(p["name"], "public");
    assert_eq!(p["key"], public);
    assert_eq!(p["prefix"], public[..12]);
    assert!(p["created_at"].is_string());
    assert!(p["last_used_at"].is_null());
    let s = by(secret_id);
    assert_eq!(s["kind"], "secret");
    assert_eq!(s["role"], "reporting");
    assert_eq!(s["name"], "ci");
    assert_eq!(s["prefix"], secret[..12]);
    assert!(s.get("key").is_none(), "{s}");
    let text = serde_json::to_string(&list).unwrap();
    for hidden in [secret.as_str(), "sealed", "sha256", "revoked"] {
        assert!(!text.contains(hidden), "leaked {hidden}");
    }
    // Another branch's keys are its own.
    let dev = t.ready("dev");
    assert!(t.list_keys(dev).await.is_empty());
}

#[tokio::test]
async fn create_secret_key_returns_it_once() {
    let t = branching().await;
    t.seed_public_key(t.main());
    let resp = t
        .send(Method::POST, &t.keys(t.main()), Some(json!({ "name": "ci", "role": "reporting" })))
        .await;
    assert_eq!(resp.status(), 201);
    let created: Value = resp.json().await.unwrap();
    let key = created["key"].as_str().unwrap().to_string();
    assert!(key.starts_with("chk_sec_") && key.len() == 51, "{key}");
    assert_eq!(created["kind"], "secret");
    assert_eq!(created["role"], "reporting");
    assert_eq!(created["name"], "ci");
    assert_eq!(created["prefix"], key[..12]);
    let id: uuid::Uuid = created["id"].as_str().unwrap().parse().unwrap();
    // The worker was asked first, then only the hash was stored.
    assert_eq!(*t.jobs.role_checks.lock().unwrap(), vec![(t.main(), "reporting".to_string())]);
    {
        let keys = t.metadata.keys.lock().unwrap();
        let (stored, hash) = keys.iter().find(|(k, _)| k.id == id).unwrap();
        assert_eq!(*hash, data_key::hash(&key));
        assert_eq!(stored.sealed, None);
        assert_eq!(stored.branch_id, t.main());
    }
    // A second list has no `key` for it.
    let list = t.list_keys(t.main()).await;
    let listed = list.iter().find(|k| k["id"] == json!(id)).expect("listed");
    assert!(listed.get("key").is_none(), "{listed}");
    assert!(!serde_json::to_string(&list).unwrap().contains(&key));

    // The role defaults to `authenticated`.
    let created: Value = t
        .send(Method::POST, &t.keys(t.main()), Some(json!({ "name": "web" })))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(created["role"], "authenticated");
}

#[tokio::test]
async fn create_validates_the_body() {
    let t = branching().await;
    for body in [
        json!({}),
        json!({ "name": "" }),
        json!({ "name": "x".repeat(64) }),
        json!({ "name": "ok", "role": "" }),
        json!({ "name": "ok", "surprise": 1 }),
        json!({ "name": 5 }),
    ] {
        let resp = t.send(Method::POST, &t.keys(t.main()), Some(body.clone())).await;
        assert_eq!(resp.status(), 422, "{body}");
    }
    assert_eq!(
        t.send(Method::POST, &t.keys(t.main()), Some(json!({ "name": "x".repeat(63) }))).await.status(),
        201
    );
    // Nothing was asked of the worker for the refused ones.
    assert_eq!(t.jobs.role_checks.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn secret_keys_store_the_role_oid_the_check_found() {
    let t = branching().await;
    t.seed_public_key(t.main());
    *t.jobs.role_check.lock().unwrap() = Some(Ok(CheckDataRoleReply::ok_with_oid(16390)));
    let created: Value = t
        .send(Method::POST, &t.keys(t.main()), Some(json!({ "name": "ci", "role": "reporting" })))
        .await
        .json()
        .await
        .unwrap();
    let id: uuid::Uuid = created["id"].as_str().unwrap().parse().unwrap();
    let oid_of = |id: uuid::Uuid| {
        t.metadata.keys.lock().unwrap().iter().find(|(k, _)| k.id == id).unwrap().0.role_oid
    };
    assert_eq!(oid_of(id), Some(16390));
    let rotated: Value = t
        .send(Method::POST, &format!("{}/{id}/rotate", t.keys(t.main())), None)
        .await
        .json()
        .await
        .unwrap();
    let new_id: uuid::Uuid = rotated["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(oid_of(new_id), Some(16390), "the rotated key keeps the identity");
}

// The role was dropped and created again under the same name (a new OID): the old key is
// dead, and rotating it must not hand the new role a key.
#[tokio::test]
async fn rotate_refuses_a_key_whose_role_was_recreated() {
    let t = branching().await;
    t.seed_public_key(t.main());
    *t.jobs.role_check.lock().unwrap() = Some(Ok(CheckDataRoleReply::ok_with_oid(16390)));
    let created: Value = t
        .send(Method::POST, &t.keys(t.main()), Some(json!({ "name": "ci", "role": "reporting" })))
        .await
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    *t.jobs.role_check.lock().unwrap() = Some(Ok(CheckDataRoleReply::ok_with_oid(16391)));
    let resp = t.send(Method::POST, &format!("{}/{id}/rotate", t.keys(t.main())), None).await;
    let body = error_of(resp, 422).await;
    assert_eq!(code_of(&body), "role_refused");
    assert_eq!(
        message_of(&body),
        "the role reporting was dropped and created again since this key was made; create a new key for it"
    );
    let list = t.list_keys(t.main()).await;
    assert!(list.iter().any(|k| k["id"] == json!(id)), "the old key is still listed");
    assert_eq!(list.len(), 2, "nothing inserted");
}

#[tokio::test]
async fn secret_key_writes_while_the_endpoint_is_busy_are_503_with_retry_after() {
    let t = branching().await;
    t.seed_public_key(t.main());
    let (old_id, _) = t.seed_secret_key(t.main(), "ci", "reporting");
    *t.metadata.endpoint_busy.lock().unwrap() = true;
    let create = t
        .send(Method::POST, &t.keys(t.main()), Some(json!({ "name": "x", "role": "reporting" })))
        .await;
    assert_eq!(create.status(), 503);
    assert_eq!(create.headers()["retry-after"], "1");
    assert_eq!(code_of(&create.json::<Value>().await.unwrap()), "unavailable");
    let rotate = t.send(Method::POST, &format!("{}/{old_id}/rotate", t.keys(t.main())), None).await;
    assert_eq!(rotate.status(), 503);
    assert_eq!(rotate.headers()["retry-after"], "1");
    let list = t.list_keys(t.main()).await;
    assert_eq!(list.len(), 2, "nothing changed");
    assert!(list.iter().any(|k| k["id"] == json!(old_id)));
}

// C1: a NOLOGIN role without a password has no `neon_roles` row; the worker's check on the
// compute is what decides it exists, for create and for rotate.
#[tokio::test]
async fn secret_key_create_and_rotate_for_a_rowless_nologin_role() {
    let t = branching().await;
    t.seed_public_key(t.main());
    let created = t
        .send(Method::POST, &t.keys(t.main()), Some(json!({ "name": "ci", "role": "reader" })))
        .await;
    assert_eq!(created.status(), 201);
    let created: Value = created.json().await.unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    let rotated = t.send(Method::POST, &format!("{}/{id}/rotate", t.keys(t.main())), None).await;
    assert_eq!(rotated.status(), 201);
    let rotated: Value = rotated.json().await.unwrap();
    assert_eq!(rotated["role"], "reader");
    assert_eq!(
        *t.jobs.role_checks.lock().unwrap(),
        vec![(t.main(), "reader".to_string()), (t.main(), "reader".to_string())],
        "rotation re-checks the role on the compute"
    );
}

#[tokio::test]
async fn rotate_maps_check_role_refusal() {
    let t = branching().await;
    t.seed_public_key(t.main());
    let (old_id, _) = t.seed_secret_key(t.main(), "ci", "reporting");
    let path = format!("{}/{old_id}/rotate", t.keys(t.main()));
    #[allow(clippy::type_complexity)]
    let cases: [(Result<CheckDataRoleReply, ResetError>, u16, &str); 4] = [
        (Ok(CheckDataRoleReply::refused("role reporting doesn't exist")), 422, "role_refused"),
        (Ok(CheckDataRoleReply::not_ready()), 409, "branch_not_ready"),
        (Err(ResetError::Timeout), 504, "timeout"),
        (Err(ResetError::Transport("no worker listening".into())), 503, "worker_unavailable"),
    ];
    for (answer, status, code) in cases {
        *t.jobs.role_check.lock().unwrap() = Some(answer);
        let body = error_of(t.send(Method::POST, &path, None).await, status).await;
        assert_eq!(code_of(&body), code);
    }
    let list = t.list_keys(t.main()).await;
    assert!(list.iter().any(|k| k["id"] == json!(old_id)), "the old key is still live");
    assert_eq!(list.len(), 2, "nothing inserted");
}

// The public key's role is `anon`, which no DDL can drop: its rotation asks no worker.
#[tokio::test]
async fn rotate_public_key_skips_the_role_check() {
    let t = branching().await;
    let (public_id, _) = t.seed_public_key(t.main());
    let resp = t.send(Method::POST, &format!("{}/{public_id}/rotate", t.keys(t.main())), None).await;
    assert_eq!(resp.status(), 201);
    assert!(t.jobs.role_checks.lock().unwrap().is_empty());
}

// A rotation whose key was renamed after it was read: the worker refuses the old name, the
// key is read again and its new name checked.
#[tokio::test]
async fn rotate_follows_a_rename_seen_by_the_check() {
    let t = branching().await;
    t.seed_public_key(t.main());
    let (old_id, _) = t.seed_secret_key(t.main(), "ci", "reporting");
    *t.jobs.role_check.lock().unwrap() = Some(Ok(CheckDataRoleReply::refused("gone")));
    *t.jobs.rename_on_check.lock().unwrap() =
        Some((t.metadata.keys.clone(), old_id, "reporting2".into()));
    let resp = t.send(Method::POST, &format!("{}/{old_id}/rotate", t.keys(t.main())), None).await;
    // The fake refuses every check; what matters is that the new name was asked.
    let body = error_of(resp, 422).await;
    assert_eq!(code_of(&body), "role_refused");
    assert_eq!(
        *t.jobs.role_checks.lock().unwrap(),
        vec![(t.main(), "reporting".to_string()), (t.main(), "reporting2".to_string())]
    );
}

#[tokio::test]
async fn create_maps_check_role_refusal_to_422() {
    let t = branching().await;
    let body = || Some(json!({ "name": "ci", "role": "boss" }));
    #[allow(clippy::type_complexity)]
    let cases: [(Result<CheckDataRoleReply, ResetError>, u16, &str, Option<&str>); 6] = [
        (Ok(CheckDataRoleReply::refused("the role boss is a superuser")), 422, "role_refused", Some("the role boss is a superuser")),
        (Ok(CheckDataRoleReply::not_ready()), 409, "branch_not_ready", None),
        (Ok(CheckDataRoleReply::failed("grant failed: secret detail")), 500, "internal", None),
        (Err(ResetError::Timeout), 504, "timeout", None),
        (Err(ResetError::Transport("no worker listening".into())), 503, "worker_unavailable", None),
        // A reply that says no without saying why is our failure.
        (Ok(CheckDataRoleReply { ok: false, error: None, kind: None, role_oid: None }), 500, "internal", None),
    ];
    for (answer, status, code, message) in cases {
        *t.jobs.role_check.lock().unwrap() = Some(answer);
        let resp = t.send(Method::POST, &t.keys(t.main()), body()).await;
        assert_eq!(resp.status(), status);
        let err: Value = resp.json().await.unwrap();
        let text = message_of(&err);
        assert_eq!(code_of(&err), code);
        if let Some(message) = message {
            assert_eq!(text, message);
        }
        assert!(!text.contains("secret detail"), "{text}");
    }
    assert!(t.metadata.keys.lock().unwrap().is_empty(), "a refused role stores no key");

    // A branch that isn't ready is a 409 without asking the worker.
    *t.jobs.role_check.lock().unwrap() = None;
    let before = t.jobs.role_checks.lock().unwrap().len();
    let resetting = t.add("busy", t.main(), BranchKind::Normal, BranchStatus::Resetting);
    let resp = t.send(Method::POST, &t.keys(resetting), body()).await;
    assert_eq!(resp.status(), 409);
    let backup = t.backup(t.main());
    let err = error_of(t.send(Method::POST, &t.keys(backup), body()).await, 409).await;
    assert_eq!(code_of(&err), "backup_branch");
    assert_eq!(t.jobs.role_checks.lock().unwrap().len(), before);
}

#[tokio::test]
async fn revoke_public_is_409() {
    let t = branching().await;
    let (public_id, _) = t.seed_public_key(t.main());
    let path = format!("{}/{public_id}", t.keys(t.main()));
    let resp = t.send(Method::DELETE, &path, None).await;
    assert_eq!(resp.status(), 409);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(code_of(&body), "public_key_not_revocable");
    assert_eq!(message_of(&body), "rotate the public key instead");
    assert_eq!(t.list_keys(t.main()).await.len(), 1);
}

#[tokio::test]
async fn revoke_secret_is_204_then_404() {
    let t = branching().await;
    let (id, _) = t.seed_secret_key(t.main(), "ci", "reporting");
    let (other, _) = t.seed_secret_key(t.main(), "keep", "reporting");
    let path = format!("{}/{id}", t.keys(t.main()));
    assert_eq!(t.send(Method::DELETE, &path, None).await.status(), 204);
    let list = t.list_keys(t.main()).await;
    assert_eq!(list.iter().map(|k| k["id"].clone()).collect::<Vec<_>>(), vec![json!(other)]);
    // Already revoked, unknown, not a uuid, and another branch's key.
    assert_eq!(t.send(Method::DELETE, &path, None).await.status(), 404);
    let unknown = format!("{}/{}", t.keys(t.main()), uuid::Uuid::new_v4());
    assert_eq!(t.send(Method::DELETE, &unknown, None).await.status(), 404);
    let dev = t.ready("dev");
    let wrong_branch = format!("{}/{other}", t.keys(dev));
    assert_eq!(t.send(Method::DELETE, &wrong_branch, None).await.status(), 404);
    assert_eq!(t.list_keys(t.main()).await.len(), 1);
}

#[tokio::test]
async fn rotate_unknown_is_404() {
    let t = branching().await;
    let (id, _) = t.seed_secret_key(t.main(), "ci", "reporting");
    let unknown = format!("{}/{}/rotate", t.keys(t.main()), uuid::Uuid::new_v4());
    assert_eq!(t.send(Method::POST, &unknown, None).await.status(), 404);
    // Revoked keys can't be rotated.
    assert_eq!(t.send(Method::DELETE, &format!("{}/{id}", t.keys(t.main())), None).await.status(), 204);
    let revoked = format!("{}/{id}/rotate", t.keys(t.main()));
    assert_eq!(t.send(Method::POST, &revoked, None).await.status(), 404);
    assert!(t.list_keys(t.main()).await.is_empty());
}

#[tokio::test]
async fn rotate_public_gives_a_new_public_key_in_full() {
    let t = branching().await;
    let (old_id, old) = t.seed_public_key(t.main());
    let resp = t.send(Method::POST, &format!("{}/{old_id}/rotate", t.keys(t.main())), None).await;
    assert_eq!(resp.status(), 201);
    let created: Value = resp.json().await.unwrap();
    let key = created["key"].as_str().unwrap();
    assert!(key.starts_with("chk_pub_") && key.len() == 51 && key != old, "{key}");
    assert_eq!(created["kind"], "public");
    assert_eq!(created["role"], "anon");
    assert_eq!(created["name"], "public");
    assert_ne!(created["id"], json!(old_id));
    // Only the new one is live, and it is shown in full (sealed under its own id).
    let list = t.list_keys(t.main()).await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], created["id"]);
    assert_eq!(list[0]["key"], key);
    let new_id: uuid::Uuid = created["id"].as_str().unwrap().parse().unwrap();
    let sealed = t.metadata.keys.lock().unwrap().iter().find(|(k, _)| k.id == new_id).unwrap().0.sealed.clone().unwrap();
    assert_eq!(open_credential(&t.app.key, &new_id.to_string(), Purpose::DataKey, &sealed).unwrap(), key);
    // No role check for a public key.
    assert!(t.jobs.role_checks.lock().unwrap().is_empty());
}

#[tokio::test]
async fn rotate_secret_keeps_name_and_role() {
    let t = branching().await;
    t.seed_public_key(t.main());
    let (old_id, old) = t.seed_secret_key(t.main(), "ci", "reporting");
    let created: Value = t
        .send(Method::POST, &format!("{}/{old_id}/rotate", t.keys(t.main())), None)
        .await
        .json()
        .await
        .unwrap();
    let key = created["key"].as_str().unwrap();
    assert!(key.starts_with("chk_sec_") && key != old);
    assert_eq!((created["kind"].as_str(), created["role"].as_str(), created["name"].as_str()), (Some("secret"), Some("reporting"), Some("ci")));
    let list = t.list_keys(t.main()).await;
    assert_eq!(list.len(), 2, "the public key and the new secret key");
    assert!(list.iter().all(|k| k["id"] != json!(old_id)));
    assert!(list.iter().find(|k| k["kind"] == "secret").unwrap().get("key").is_none());
}

#[tokio::test]
async fn another_tenant_gets_404() {
    let t = branching().await;
    let mut other_db = None;
    let other = lifecycle_app(FakeJobs::default(), |m, _t, k| {
        let db = ready_database(TenantId::new(), k);
        other_db = Some((db.database_id, BranchId::from(db.database_id)));
        m.insert_database(db);
    })
    .await;
    let (db, branch) = other_db.unwrap();
    let base = format!("/v1/databases/{}/branches/{}", db.as_uuid(), branch.as_uuid());
    let key = uuid::Uuid::new_v4();
    for (method, path, body) in [
        (Method::GET, format!("{base}/keys"), None),
        (Method::POST, format!("{base}/keys"), Some(json!({ "name": "x" }))),
        (Method::POST, format!("{base}/keys/{key}/rotate"), None),
        (Method::DELETE, format!("{base}/keys/{key}"), None),
        (Method::GET, format!("{base}/data-api/schema"), None),
    ] {
        let mut req = reqwest::Client::new()
            .request(method.clone(), format!("{}{path}", other.base))
            .header(IDENTITY_HEADER, other.token_for_org("org-1"));
        if let Some(body) = body {
            req = req.json(&body);
        }
        assert_eq!(req.send().await.unwrap().status(), 404, "{method} {path}");
    }
    drop(t);
}

/// Another tenant's database: every per-database data API route and the console answer 404
/// as for a missing one, and nothing runs.
#[tokio::test]
async fn data_api_database_routes_of_another_tenant_are_404() {
    let jobs = FakeJobs::default();
    let mut other_db = None;
    let other = lifecycle_app(jobs.clone(), |m, _t, k| {
        let db = ready_database(TenantId::new(), k);
        let branch = BranchId::from(db.database_id);
        other_db = Some((db.database_id, branch));
        m.insert_database(db);
        m.add_branch_db(branch, "web1", "postgres", DatabaseOrigin::Main);
        m.add_branch_db(branch, "analytics", "etl", DatabaseOrigin::Sql);
    })
    .await;
    let (db, branch) = other_db.unwrap();
    let base = format!("/v1/databases/{}/branches/{}", db.as_uuid(), branch.as_uuid());
    let key = uuid::Uuid::new_v4();
    for (method, path, body) in [
        (Method::POST, format!("{base}/pg-databases/analytics/data-api"), None),
        (Method::DELETE, format!("{base}/pg-databases/analytics/data-api?remove=true"), None),
        (Method::PATCH, format!("{base}/pg-databases/analytics"), Some(json!({ "default_schema": "app" }))),
        (Method::PATCH, format!("{base}/keys/{key}"), Some(json!({ "databases": null }))),
        (Method::POST, format!("{base}/console/query"), Some(json!({ "role": null, "tree": { "v": 1, "op": "select", "from": "t" } }))),
        (Method::POST, format!("{base}/console/query"), Some(json!({ "role": "editor", "tree": { "v": 1, "op": "select", "from": "t" } }))),
        (Method::GET, format!("{base}/console/schema"), None),
        (Method::GET, format!("{base}/console/schema?role=editor"), None),
    ] {
        let mut req = reqwest::Client::new()
            .request(method.clone(), format!("{}{path}", other.base))
            .header(IDENTITY_HEADER, other.token_for_org("org-1"));
        if let Some(body) = body {
            req = req.json(&body);
        }
        let resp = req.send().await.unwrap();
        assert_eq!(code_of(&error_of(resp, 404).await), "not_found", "{method} {path}");
    }
    assert!(jobs.data_api_calls.lock().unwrap().is_empty());
    assert!(jobs.role_checks.lock().unwrap().is_empty());
}

#[tokio::test]
async fn keys_of_an_unknown_branch_are_404() {
    let t = branching().await;
    let unknown = format!("{}/keys", t.branch(BranchId::new()));
    assert_eq!(t.send(Method::GET, &unknown, None).await.status(), 404);
    assert_eq!(t.send(Method::POST, &unknown, Some(json!({ "name": "x" }))).await.status(), 404);
}

#[tokio::test]
async fn settings_patch_bounds() {
    let t = branching().await;
    let path = database_path(&t);
    for (field, ok, bad) in [
        ("max_rows", [1, 100000], [0, 100001]),
        ("statement_timeout_ms", [100, 120000], [99, 120001]),
        ("max_connections", [1, 50], [0, 51]),
    ] {
        for value in ok {
            let resp = t.send(Method::PATCH, &path, Some(json!({ field: value }))).await;
            assert_eq!(resp.status(), 200, "{field}={value}");
            let body: Value = resp.json().await.unwrap();
            assert_eq!(body[field], value);
        }
        for value in bad {
            assert_eq!(t.send(Method::PATCH, &path, Some(json!({ field: value }))).await.status(), 422, "{field}={value}");
        }
        assert_eq!(t.send(Method::PATCH, &path, Some(json!({ field: "many" }))).await.status(), 422);
    }
    // `default_schema` moved to the per-database settings: this route no longer knows it.
    let moved = t.send(Method::PATCH, &path, Some(json!({ "default_schema": "app" }))).await;
    assert!(moved.status().is_client_error(), "{}", moved.status());
    // Refused values change nothing, and the stored values come back on the next PATCH.
    let db = t.metadata.database(t.db);
    assert_eq!((db.max_rows, db.statement_timeout_ms, db.max_connections), (100000, 120000, 50));
    let body: Value = t.send(Method::PATCH, &path, Some(json!({}))).await.json().await.unwrap();
    assert_eq!(body["max_rows"], 100000);
}

// ---- the data API's schema, proxied from rest

type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

async fn mock_rest(seen: Seen) -> String {
    use axum::extract::Path;
    use axum::http::HeaderMap;
    use axum::routing::get;
    let app = Router::new().route(
        "/internal/schema/{branch}",
        get(move |Path(branch): Path<String>, headers: HeaderMap| {
            let seen = seen.clone();
            async move {
                let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_string);
                seen.lock().unwrap().push((branch, auth));
                axum::Json(json!({ "version": "v1", "schemas": { "public": { "tables": {}, "functions": {}, "enums": {} } } }))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[tokio::test]
async fn data_api_schema_is_proxied_from_rest() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let rest = mock_rest(seen.clone()).await;
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app_with_rest(metadata.clone(), jobs.clone(), &rest).await;
    let db = ready_database(tenant, &app.key);
    let id = db.database_id;
    metadata.insert_database(db);
    let t = Branching { app, metadata, jobs, db: id };

    let path = format!("{}/data-api/schema", t.branch(t.main()));
    let resp = t.send(Method::GET, &path, None).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["version"], "v1");
    assert!(body["schemas"]["public"].is_object());
    assert_eq!(
        *seen.lock().unwrap(),
        vec![(t.main().as_uuid().to_string(), Some(format!("Bearer {REST_TOKEN}")))]
    );
}

/// A rest that records each `?database=` it is asked for (`None` when absent) and answers
/// 404 `unknown_database` for `gone`.
async fn mock_rest_schema_databases() -> (String, Arc<Mutex<Vec<Option<String>>>>) {
    use axum::extract::Query;
    use axum::routing::get;
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let app = Router::new().route(
        "/internal/schema/{branch}",
        get(move |Query(q): Query<std::collections::HashMap<String, String>>| {
            let log = log.clone();
            async move {
                let database = q.get("database").cloned();
                log.lock().unwrap().push(database.clone());
                if database.as_deref() == Some("gone") {
                    return (
                        axum::http::StatusCode::NOT_FOUND,
                        axum::Json(json!({ "error": { "code": "unknown_database", "message": "no such database" } })),
                    );
                }
                (axum::http::StatusCode::OK, axum::Json(json!({ "version": "v1", "schemas": {}, "database": database })))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (rest, seen)
}

async fn branching_with_schema_rest() -> (Branching, Arc<Mutex<Vec<Option<String>>>>) {
    let (rest, seen) = mock_rest_schema_databases().await;
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app_with_rest(metadata.clone(), jobs.clone(), &rest).await;
    let db = ready_database(tenant, &app.key);
    let id = db.database_id;
    metadata.insert_database(db);
    (Branching { app, metadata, jobs, db: id }, seen)
}

#[tokio::test]
async fn data_api_schema_forwards_the_database() {
    let (t, seen) = branching_with_schema_rest().await;
    let main = t.seeded();
    t.metadata.add_branch_db(main, "my db&x", "etl", DatabaseOrigin::Sql);
    t.metadata.set_data_api(main, "my db&x", chelabase_metadata::DataApiState::On);
    let path = format!("{}/data-api/schema", t.branch(main));
    // No parameter: main, as today.
    assert_eq!(t.send(Method::GET, &path, None).await.status(), 200);
    let resp = t.send(Method::GET, &format!("{path}?database=web1"), None).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["database"], "web1");
    // The name is percent-encoded on the way to rest and decoded there.
    assert_eq!(t.send(Method::GET, &format!("{path}?database=my%20db%26x"), None).await.status(), 200);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![None, Some("web1".to_string()), Some("my db&x".to_string())]
    );
}

#[tokio::test]
async fn data_api_schema_unknown_or_disabled_database() {
    let (t, seen) = branching_with_schema_rest().await;
    let main = t.seeded();
    let path = format!("{}/data-api/schema", t.branch(main));
    let err = error_of(t.send(Method::GET, &format!("{path}?database=nope"), None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    let err = error_of(t.send(Method::GET, &format!("{path}?database=analytics"), None).await, 409).await;
    assert_eq!(code_of(&err), "data_api_disabled");
    // Both are answered from metadata, before rest is asked.
    assert!(seen.lock().unwrap().is_empty());
    // Rest saying `unknown_database` for a database metadata lists as on (it vanished
    // meanwhile): answered by what metadata says, here still not found.
    t.metadata.add_branch_db(main, "gone", "etl", DatabaseOrigin::Sql);
    t.metadata.set_data_api(main, "gone", chelabase_metadata::DataApiState::On);
    let err = error_of(t.send(Method::GET, &format!("{path}?database=gone"), None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn data_api_schema_is_503_when_rest_is_down() {
    let t = branching().await; // rest points at a closed port
    let path = format!("{}/data-api/schema", t.branch(t.main()));
    let resp = t.send(Method::GET, &path, None).await;
    assert_eq!(resp.status(), 503);
    let text = resp.text().await.unwrap();
    assert!(!text.contains("127.0.0.1"), "{text}");
    let body: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(code_of(&body), "unavailable");
}

/// Rest answering 503 (the branch's database is unreachable, the schema is being
/// reinstalled) is passed on with its body and `Retry-After`, not turned into a 502.
#[tokio::test]
async fn data_api_schema_passes_rests_503_through() {
    use axum::routing::get;
    let app = Router::new().route(
        "/internal/schema/{branch}",
        get(|| async {
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                [(axum::http::header::RETRY_AFTER, "2")],
                axum::Json(json!({ "error": { "code": "unavailable", "message": "try again" } })),
            )
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app_with_rest(metadata.clone(), jobs.clone(), &rest).await;
    let db = ready_database(tenant, &app.key);
    let id = db.database_id;
    metadata.insert_database(db);
    let t = Branching { app, metadata, jobs, db: id };

    let resp = t.send(Method::GET, &format!("{}/data-api/schema", t.branch(t.main())), None).await;
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.headers()["retry-after"], "2");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["error"]["code"], "unavailable");
    assert_eq!(body["error"]["message"], "try again");
}

// ---- error shape rulings

#[tokio::test]
async fn parent_lookup_error_is_500() {
    let t = branching().await;
    // Only the parent lookup reads a branch before the create.
    *t.metadata.fail_get_branch.lock().unwrap() = true;
    let err = error_of(t.send(Method::POST, &t.branches(), Some(json!({ "name": "x" }))).await, 500).await;
    assert_eq!(code_of(&err), "internal");
    assert!(t.published().is_empty());
}

#[tokio::test]
async fn publish_failure_is_503_worker_unavailable() {
    let t = branching().await;
    *t.jobs.fail_branch_publish.lock().unwrap() = true;
    let err = error_of(t.send(Method::POST, &t.branches(), Some(json!({ "name": "dev" }))).await, 503).await;
    assert_eq!(code_of(&err), "worker_unavailable");
    let dev = t.ready("qa");
    let err = error_of(t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await, 503).await;
    assert_eq!(code_of(&err), "worker_unavailable");
    let err = error_of(
        t.send(Method::POST, &format!("{}/restore", t.branch(dev)), Some(json!({ "at": { "lsn": "0/1" } }))).await,
        503,
    )
    .await;
    assert_eq!(code_of(&err), "worker_unavailable");

    // Provisioning a database too.
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    *jobs.fail_provision_publish.lock().unwrap() = true;
    let app = spawn_app(metadata, jobs).await;
    let token = app.token_for_org("org-1");
    let resp = post_create(&app, Some(&token), create_body(tenant)).await;
    let err = error_of(resp, 503).await;
    assert_eq!(code_of(&err), "worker_unavailable");
}

#[tokio::test]
async fn backup_branch_key_create_is_backup_branch() {
    let t = branching().await;
    let backup = t.backup(t.main());
    let err = error_of(t.send(Method::POST, &t.keys(backup), Some(json!({ "name": "ci" }))).await, 409).await;
    assert_eq!(code_of(&err), "backup_branch");
    assert!(t.jobs.role_checks.lock().unwrap().is_empty());
}

// Another tenant's project looks exactly like a missing one: same status, same body.
#[tokio::test]
async fn create_database_in_another_tenants_project_is_the_missing_project_answer() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let (foreign, missing) = (ProjectId::new(), ProjectId::new());
    metadata.foreign_projects.lock().unwrap().push(foreign);
    metadata.missing_projects.lock().unwrap().push(missing);
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let token = app.token_for_org("org-1");
    let body = |project: ProjectId| json!({ "tenant_id": tenant, "project_id": project, "name": "demo" });

    let resp = post_create(&app, Some(&token), body(foreign)).await;
    let foreign_err = error_of(resp, 404).await;
    let resp = post_create(&app, Some(&token), body(missing)).await;
    let missing_err = error_of(resp, 404).await;
    assert_eq!(code_of(&foreign_err), "not_found");
    assert_eq!(code_of(&missing_err), "not_found");
    // Only the id differs; the message must not name a tenant or say "belong".
    assert_eq!(
        message_of(&foreign_err),
        message_of(&missing_err).replace(&missing.to_string(), &foreign.to_string())
    );
    assert!(!message_of(&foreign_err).contains("belong"), "{foreign_err}");
}

#[tokio::test]
async fn bad_json_body_is_json_invalid_request() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let url = format!("{}/v1/databases", app.base);
    for (content_type, body, status) in [
        ("application/json", "{nope", 400),
        ("text/plain", "{}", 415),
        ("application/json", r#"{"tenant_id": 5}"#, 422),
    ] {
        let resp = reqwest::Client::new()
            .post(&url)
            .header(IDENTITY_HEADER, app.token_for_org("org-1"))
            .header("content-type", content_type)
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.headers()["content-type"], "application/json", "{body}");
        let err = error_of(resp, status).await;
        assert_eq!(code_of(&err), "invalid_request", "{body}");
        assert!(!message_of(&err).is_empty());
    }
}

#[tokio::test]
async fn non_uuid_path_is_json_invalid_request() {
    let t = branching().await;
    for path in [
        "/v1/databases/not-a-uuid".to_string(),
        "/v1/databases/not-a-uuid/branches".to_string(),
        format!("/v1/databases/{}/branches/not-a-uuid/connection", t.db.as_uuid()),
        format!("/v1/databases/{}/branches/not-a-uuid/keys", t.db.as_uuid()),
    ] {
        let resp = t.send(Method::GET, &path, None).await;
        assert_eq!(resp.status(), 400, "{path}");
        assert_eq!(resp.headers()["content-type"], "application/json", "{path}");
        let err = error_of(resp, 400).await;
        assert_eq!(code_of(&err), "invalid_request", "{path}");
    }
}

/// Starts a mock rest answering the schema route with `status` and `body`.
async fn rest_answering(status: u16, body: Value) -> Branching {
    use axum::routing::get;
    let app = Router::new().route(
        "/internal/schema/{branch}",
        get(move || {
            let body = body.clone();
            async move { (axum::http::StatusCode::from_u16(status).unwrap(), axum::Json(body)) }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let rest = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app_with_rest(metadata.clone(), jobs.clone(), &rest).await;
    let db = ready_database(tenant, &app.key);
    let id = db.database_id;
    metadata.insert_database(db);
    Branching { app, metadata, jobs, db: id }
}

#[tokio::test]
async fn schema_409_is_branch_not_ready() {
    let t = rest_answering(409, json!({ "status": "resetting" })).await;
    let resp = t.send(Method::GET, &format!("{}/data-api/schema", t.branch(t.main())), None).await;
    let err = error_of(resp, 409).await;
    assert_eq!(code_of(&err), "branch_not_ready");
    assert_eq!(message_of(&err), "branch is not ready");
    assert_eq!(err["error"].as_object().unwrap().len(), 2, "{err}");
}

#[tokio::test]
async fn schema_404_is_not_found_and_unexpected_answers_are_unavailable() {
    let t = rest_answering(404, json!({ "error": { "code": "unknown_branch", "message": "no such branch" } })).await;
    let resp = t.send(Method::GET, &format!("{}/data-api/schema", t.branch(t.main())), None).await;
    let err = error_of(resp, 404).await;
    assert_eq!(code_of(&err), "not_found");
    let t = rest_answering(500, json!({ "oops": true })).await;
    let resp = t.send(Method::GET, &format!("{}/data-api/schema", t.branch(t.main())), None).await;
    let err = error_of(resp, 502).await;
    assert_eq!(code_of(&err), "unavailable");
}

// ---- login roles

impl Branching {
    fn roles(&self, branch: BranchId) -> String {
        format!("{}/roles", self.branch(branch))
    }

    /// The branch's endpoint, with its owner row so a list has something to start from.
    fn with_owner_row(&self, branch: BranchId) -> String {
        let ep = self.metadata.branch(branch).endpoint_id.unwrap();
        self.metadata.add_role(&ep, "postgres", RoleOrigin::Owner, true, false);
        ep
    }
}

fn role_row(name: &str, origin: RoleOrigin, level: RoleLevel, password: PasswordState) -> RoleRow {
    RoleRow {
        name: name.into(),
        origin,
        level,
        login: true,
        password,
        created_at: chrono::Utc::now(),
        pending: false,
    }
}

fn role_reply(code: &str) -> RoleReply {
    RoleReply { ok: false, code: Some(code.into()), error: Some("worker said so".into()), keys: vec![] }
}

#[tokio::test]
async fn list_roles_shapes_and_blocked_keys() {
    let t = branching().await;
    let dev = t.ready("dev");
    let ep = t.with_owner_row(dev);
    t.metadata.add_role(&ep, "app", RoleOrigin::Api, false, true);
    t.metadata.add_role_row(&ep, role_row("etl", RoleOrigin::Sql, RoleLevel::OwnerLevel, PasswordState::SetInSql));
    t.metadata.add_role_row(&ep, role_row("old", RoleOrigin::Sql, RoleLevel::Plain, PasswordState::Md5Unusable));
    // Live secret keys naming `etl` (owner-level) and `app` (plain): only the first is blocked.
    {
    let mut keys = t.metadata.keys.lock().unwrap();
    for (role, name) in [("etl", "ci"), ("app", "web")] {
        let id = uuid::Uuid::new_v4();
        insert_key(
            &mut keys,
            &NewDataKey {
                id,
                branch_id: dev,
                kind: DataKeyKind::Secret,
                role: role.into(),
                name: name.into(),
                prefix: "ck_x".into(),
                sealed: None,
                sha256: [id.as_bytes()[0]; 32],
                databases: None,
                role_oid: None,
            },
        )
        .unwrap();
    }
    }
    let resp = t.send(Method::GET, &t.roles(dev), None).await;
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().get("cache-control").is_none());
    let body: Value = resp.json().await.unwrap();
    let rows = body.as_array().unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0]["name"], "postgres");
    assert_eq!(rows[0]["origin"], "owner");
    assert_eq!(rows[0]["level"], "owner_level");
    assert_eq!(rows[0]["blocked_keys"], json!([]));
    assert_eq!(rows[1]["name"], "app");
    assert_eq!(rows[1]["origin"], "api");
    assert_eq!(rows[1]["level"], "plain");
    assert_eq!(rows[1]["login"], true);
    assert_eq!(rows[1]["password"], "generated");
    assert_eq!(rows[1]["pending"], true);
    assert_eq!(rows[1]["blocked_keys"], json!([]));
    assert_eq!(rows[2]["origin"], "sql");
    assert_eq!(rows[2]["level"], "owner_level");
    assert_eq!(rows[2]["password"], "set_in_sql");
    assert_eq!(rows[2]["blocked_keys"].as_array().unwrap().len(), 1);
    assert_eq!(rows[2]["blocked_keys"][0]["name"], "ci");
    assert!(rows[2]["blocked_keys"][0]["id"].is_string());
    assert_eq!(rows[3]["password"], "md5_unusable");
    assert!(rows[1]["created_at"].is_string());
    for row in rows {
        assert!(row.get("kind").is_none() && row.get("privileged").is_none(), "{row}");
    }
}

#[tokio::test]
async fn list_roles_reports_unreadable() {
    let t = branching().await;
    let dev = t.ready("dev");
    let ep = t.with_owner_row(dev);
    t.metadata.add_role_row(&ep, role_row("lost", RoleOrigin::Api, RoleLevel::Plain, PasswordState::Unreadable));
    let body: Value = t.send(Method::GET, &t.roles(dev), None).await.json().await.unwrap();
    let row = body.as_array().unwrap().iter().find(|r| r["name"] == "lost").unwrap();
    assert_eq!(row["password"], "unreadable");
}

// C10: a key says whether its role is owner-level (the data API refuses such a key), so Studio
// needn't list the roles.
#[tokio::test]
async fn data_keys_mark_blocked_roles() {
    let t = branching().await;
    let dev = t.ready("dev");
    let ep = t.with_owner_row(dev);
    t.metadata.add_role_row(&ep, role_row("etl", RoleOrigin::Sql, RoleLevel::OwnerLevel, PasswordState::SetInSql));
    t.metadata.add_role_row(&ep, role_row("app", RoleOrigin::Api, RoleLevel::Plain, PasswordState::Generated));
    let (blocked, _) = t.seed_secret_key(dev, "ci", "etl");
    let (plain, _) = t.seed_secret_key(dev, "web", "app");
    let (gone, _) = t.seed_secret_key(dev, "ghost", "dropped");
    let (public, _) = t.seed_public_key(dev);
    let list = t.list_keys(dev).await;
    let flag = |id: uuid::Uuid| list.iter().find(|k| k["id"] == json!(id)).unwrap()["role_blocked"].clone();
    assert_eq!(flag(blocked), true);
    assert_eq!(flag(plain), false);
    assert_eq!(flag(gone), false);
    assert_eq!(flag(public), false);
}

// A failing role lookup doesn't fail the key list: the flag is a hint, so every key answers
// `role_blocked: false`.
#[tokio::test]
async fn data_keys_list_survives_a_failing_role_lookup() {
    let t = branching().await;
    let dev = t.ready("dev");
    let ep = t.with_owner_row(dev);
    t.metadata.add_role_row(&ep, role_row("etl", RoleOrigin::Sql, RoleLevel::OwnerLevel, PasswordState::SetInSql));
    let (key, _) = t.seed_secret_key(dev, "ci", "etl");
    *t.metadata.fail_list_roles.lock().unwrap() = true;
    let list = t.list_keys(dev).await;
    let entry = list.iter().find(|k| k["id"] == json!(key)).unwrap();
    assert_eq!(entry["role_blocked"], false);
}

#[tokio::test]
async fn create_role_takes_owner_level_and_refuses_privileged() {
    let t = branching().await;
    let dev = t.ready("dev");
    let ep = t.with_owner_row(dev);
    // The worker writes the row; the fake answers ok.
    t.metadata.add_role(&ep, "app", RoleOrigin::Api, true, true);
    let resp = t.send(Method::POST, &t.roles(dev), Some(json!({ "name": "app", "owner_level": true }))).await;
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.unwrap();
    // Exactly the role's fields: `password` is the state (`generated`), never a secret.
    let mut fields: Vec<&str> = body.as_object().unwrap().keys().map(String::as_str).collect();
    fields.sort();
    assert_eq!(
        fields,
        ["blocked_keys", "created_at", "level", "login", "name", "origin", "password", "pending"],
        "{body}"
    );
    assert_eq!(body["password"], "generated");
    assert_eq!(body["name"], "app");
    assert_eq!(body["origin"], "api");
    assert_eq!(body["level"], "owner_level");
    assert_eq!(body["pending"], true);
    assert!(body["created_at"].is_string());
    assert_eq!(*t.jobs.role_calls.lock().unwrap(), vec!["create:app:true".to_string()]);
    // `privileged` was revision 1's field: now unknown.
    let err = error_of(t.send(Method::POST, &t.roles(dev), Some(json!({ "name": "x", "privileged": true }))).await, 422).await;
    assert_eq!(code_of(&err), "invalid_request");
    assert_eq!(t.jobs.role_calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn create_role_defaults_to_not_privileged() {
    let t = branching().await;
    let dev = t.ready("dev");
    let ep = t.with_owner_row(dev);
    t.metadata.add_role(&ep, "app", RoleOrigin::Api, false, false);
    let resp = t.send(Method::POST, &t.roles(dev), Some(json!({ "name": "app" }))).await;
    assert_eq!(resp.status(), 201);
    assert_eq!(*t.jobs.role_calls.lock().unwrap(), vec!["create:app:false".to_string()]);
}

#[tokio::test]
async fn create_role_bad_name_is_422_without_rpc() {
    let t = branching().await;
    let dev = t.ready("dev");
    for (name, code) in [
        ("Bad-Name", "invalid_role_name"),
        ("", "invalid_role_name"),
        ("postgres", "reserved_role_name"),
        ("pg_x", "reserved_role_name"),
    ] {
        let err = error_of(t.send(Method::POST, &t.roles(dev), Some(json!({ "name": name }))).await, 422).await;
        assert_eq!(code_of(&err), code, "{name}");
    }
    assert!(t.jobs.role_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn create_role_unknown_field_is_422() {
    let t = branching().await;
    let dev = t.ready("dev");
    let err = error_of(t.send(Method::POST, &t.roles(dev), Some(json!({ "name": "app", "password": "x" }))).await, 422).await;
    assert_eq!(code_of(&err), "invalid_request");
    let resp = reqwest::Client::new()
        .post(format!("{}{}", t.app.base, t.roles(dev)))
        .header(IDENTITY_HEADER, t.app.token_for_org("org-1"))
        .header("content-type", "application/json")
        .body("{nope")
        .send()
        .await
        .unwrap();
    assert_eq!(code_of(&error_of(resp, 400).await), "invalid_request");
    assert!(t.jobs.role_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn role_routes_refuse_backups_unready_and_unknown_branches() {
    let t = branching().await;
    let dev = t.ready("dev");
    let backup = t.backup(dev);
    let pending = t.add("p", t.main(), BranchKind::Normal, BranchStatus::Pending);
    for (method, suffix, body) in [
        (Method::GET, "", None),
        (Method::POST, "", Some(json!({ "name": "app" }))),
        (Method::POST, "/app/reset-password", None),
        (Method::DELETE, "/app", None),
    ] {
        let path = |id| format!("{}{suffix}", t.roles(id));
        let err = error_of(t.send(method.clone(), &path(backup), body.clone()).await, 409).await;
        assert_eq!(code_of(&err), "backup_branch", "{suffix}");
        let err = error_of(t.send(method.clone(), &path(pending), body.clone()).await, 409).await;
        assert_eq!(code_of(&err), "branch_not_ready", "{suffix}");
        assert_eq!(t.send(method.clone(), &path(BranchId::new()), body.clone()).await.status(), 404, "{suffix}");
    }
    assert!(t.jobs.role_calls.lock().unwrap().is_empty());
    assert!(t.jobs.resets.lock().unwrap().is_empty());
}

#[tokio::test]
async fn role_reply_codes_map_to_statuses() {
    let t = branching().await;
    let dev = t.ready("dev");
    let ep = t.with_owner_row(dev);
    t.metadata.add_role(&ep, "app", RoleOrigin::Api, false, false);
    let key = uuid::Uuid::new_v4();
    let in_use = RoleReply {
        keys: vec![chelabase_nats::KeyRef { id: key, name: "ci".into() }],
        ..role_reply("role_in_use")
    };
    let table: Vec<(RoleReply, u16, &str)> = vec![
        (role_reply("invalid_role_name"), 422, "invalid_role_name"),
        (role_reply("reserved_role_name"), 422, "reserved_role_name"),
        (role_reply("role_exists"), 409, "role_exists"),
        (role_reply("role_cap_reached"), 409, "role_cap_reached"),
        (role_reply("branch_not_ready"), 409, "branch_not_ready"),
        (in_use, 409, "role_in_use"),
        (role_reply("not_found"), 404, "not_found"),
        (role_reply("role_privilege_mismatch"), 500, "role_privilege_mismatch"),
        (role_reply("failed"), 500, "internal"),
        (role_reply("something_new"), 500, "internal"),
        (role_reply("not_applied"), 503, "not_applied"),
    ];
    let requests = [
        (Method::POST, t.roles(dev), Some(json!({ "name": "app" }))),
        (Method::POST, format!("{}/app/reset-password", t.roles(dev)), None),
        (Method::DELETE, format!("{}/app", t.roles(dev)), None),
    ];
    for (reply, status, code) in table {
        *t.jobs.role_reply.lock().unwrap() = Some(Ok(reply.clone()));
        for (method, path, body) in &requests {
            let err = error_of(t.send(method.clone(), path, body.clone()).await, status).await;
            assert_eq!(code_of(&err), code, "{method} {path} {reply:?}");
            let text = err.to_string();
            assert!(!text.contains("worker said so"), "the worker's message leaked: {text}");
            if reply.code.as_deref() == Some("role_in_use") {
                assert_eq!(err["error"]["keys"], json!([{ "id": key, "name": "ci" }]));
            } else {
                assert!(err["error"].get("keys").is_none());
            }
            if code == "not_applied" {
                let expected = match *method {
                    Method::POST if path.ends_with("/reset-password") => RESET_NOT_APPLIED,
                    Method::POST => "the role was stored but not yet applied; it will be retried",
                    _ => "the role was deleted but the change is not yet applied; it will be retried",
                };
                assert_eq!(message_of(&err), expected, "{method} {path}");
            }
        }
    }
    *t.jobs.role_reply.lock().unwrap() = Some(Err(ResetError::Timeout));
    let err = error_of(t.send(Method::DELETE, &format!("{}/app", t.roles(dev)), None).await, 504).await;
    assert_eq!(code_of(&err), "timeout");
    *t.jobs.role_reply.lock().unwrap() = Some(Err(ResetError::Transport("no worker listening".into())));
    let err = error_of(t.send(Method::DELETE, &format!("{}/app", t.roles(dev)), None).await, 503).await;
    assert_eq!(code_of(&err), "worker_unavailable");
}

#[tokio::test]
async fn reset_owner_alias_uses_owner_reset_and_is_204() {
    let t = branching().await;
    let dev = t.ready("dev");
    let resp = t.send(Method::POST, &format!("{}/postgres/reset-password", t.roles(dev)), None).await;
    assert_eq!(resp.status(), 204);
    assert_eq!(*t.jobs.resets.lock().unwrap(), vec![(t.db, dev)]);
    assert!(t.jobs.role_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reset_login_role_is_204() {
    let t = branching().await;
    let dev = t.ready("dev");
    let resp = t.send(Method::POST, &format!("{}/app/reset-password", t.roles(dev)), None).await;
    assert_eq!(resp.status(), 204);
    assert!(resp.text().await.unwrap().is_empty());
    assert_eq!(*t.jobs.role_calls.lock().unwrap(), vec!["reset:app".to_string()]);
    assert!(t.jobs.resets.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reset_and_delete_accept_sql_names() {
    let t = branching().await;
    let dev = t.ready("dev");
    // SQL can make `"My Role"`: the percent-encoded name reaches the worker decoded.
    let resp = t.send(Method::POST, &format!("{}/My%20Role/reset-password", t.roles(dev)), None).await;
    assert_eq!(resp.status(), 204);
    let resp = t.send(Method::DELETE, &format!("{}/Bad-Name", t.roles(dev)), None).await;
    assert_eq!(resp.status(), 204);
    assert_eq!(
        *t.jobs.role_calls.lock().unwrap(),
        vec!["reset:My Role".to_string(), "delete:Bad-Name".to_string()]
    );
    // A NUL byte, and a name over 63 bytes, never reach the worker.
    for path in [format!("{}/a%00b/reset-password", t.roles(dev)), format!("{}/{}/reset-password", t.roles(dev), "x".repeat(64))] {
        let err = error_of(t.send(Method::POST, &path, None).await, 422).await;
        assert_eq!(code_of(&err), "invalid_role_name", "{path}");
    }
    let err = error_of(t.send(Method::DELETE, &format!("{}/a%00b", t.roles(dev)), None).await, 422).await;
    assert_eq!(code_of(&err), "invalid_role_name");
    assert_eq!(t.jobs.role_calls.lock().unwrap().len(), 2);
}

// What Studio sends for a role named `a/b%61?#`: one segment, decoded once (`%2561` stays
// `%61`, not `a`).
#[tokio::test]
async fn role_path_is_decoded_once() {
    let t = branching().await;
    let dev = t.ready("dev");
    let segment = "a%2Fb%2561%3F%23";
    let resp = t.send(Method::POST, &format!("{}/{segment}/reset-password", t.roles(dev)), None).await;
    assert_eq!(resp.status(), 204);
    let resp = t.send(Method::DELETE, &format!("{}/{segment}", t.roles(dev)), None).await;
    assert_eq!(resp.status(), 204);
    assert_eq!(
        *t.jobs.role_calls.lock().unwrap(),
        vec!["reset:a/b%61?#".to_string(), "delete:a/b%61?#".to_string()]
    );
}

#[tokio::test]
async fn delete_role_204() {
    let t = branching().await;
    let dev = t.ready("dev");
    let resp = t.send(Method::DELETE, &format!("{}/app", t.roles(dev)), None).await;
    assert_eq!(resp.status(), 204);
    assert_eq!(*t.jobs.role_calls.lock().unwrap(), vec!["delete:app".to_string()]);
}

#[tokio::test]
async fn delete_owner_is_422() {
    let t = branching().await;
    let dev = t.ready("dev");
    let err = error_of(t.send(Method::DELETE, &format!("{}/postgres", t.roles(dev)), None).await, 422).await;
    assert_eq!(code_of(&err), "reserved_role_name");
    assert!(t.jobs.role_calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn connection_by_role_and_database() {
    let t = branching().await;
    let dev = t.ready("dev");
    let sealed = seal_credential(&t.app.key, &dev.to_string(), Purpose::Owner, "dev-pw");
    t.metadata.branches.lock().unwrap().get_mut(&dev).unwrap().owner_password_enc = Some(sealed);
    let ep = t.metadata.branch(dev).endpoint_id.unwrap();
    t.metadata.role_passwords.lock().unwrap().insert(
        (ep.clone(), "app".into()),
        chelabase_secrets::seal_role_password(&t.app.key, &dev.to_string(), "app", "app-pw"),
    );
    t.metadata.add_role(&ep, "postgres", RoleOrigin::Owner, true, false);
    t.metadata.add_role(&ep, "app", RoleOrigin::Api, false, false);
    t.metadata.add_role_row(&ep, role_row("etl", RoleOrigin::Sql, RoleLevel::Plain, PasswordState::SetInSql));
    t.metadata.add_role_row(&ep, role_row("old", RoleOrigin::Sql, RoleLevel::Plain, PasswordState::Md5Unusable));
    t.metadata.add_role_row(&ep, role_row("My Role@x", RoleOrigin::Sql, RoleLevel::Plain, PasswordState::SetInSql));
    t.metadata.add_branch_db(dev, "web1", "postgres", DatabaseOrigin::Main);
    t.metadata.add_branch_db(dev, "analytics", "etl", DatabaseOrigin::Sql);
    t.metadata.add_branch_db(dev, "postgres", "cloud_admin", DatabaseOrigin::System);
    let conn = format!("{}/connection", t.branch(dev));

    // Absent, or the owner by name: the owner's credentials.
    for path in [conn.clone(), format!("{conn}?role=postgres"), format!("{conn}?other=1")] {
        let resp = t.send(Method::GET, &path, None).await;
        assert_eq!(resp.status(), 200, "{path}");
        assert_eq!(resp.headers()["cache-control"], "no-store");
        let body: Value = resp.json().await.unwrap();
        assert_eq!((body["user"].as_str(), body["password"].as_str()), (Some("postgres"), Some("dev-pw")), "{path}");
        assert_eq!(body["password_state"], "generated");
    }

    // A role made through the API: its own user and password.
    let resp = t.send(Method::GET, &format!("{conn}?role=app"), None).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["user"], "app");
    assert_eq!(body["password"], "app-pw");
    assert_eq!(body["password_state"], "generated");
    assert_eq!(body["database"], "web1");
    assert_eq!(body["host"], format!("{ep}.localtest.me"));
    assert_eq!(body["uri"], format!("postgresql://app:app-pw@{ep}.localtest.me:4432/web1?sslmode=verify-full"));

    // A role set in SQL: no password, a uri without one.
    let resp = t.send(Method::GET, &format!("{conn}?role=etl&database=analytics"), None).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let body: Value = resp.json().await.unwrap();
    assert!(body["password"].is_null(), "{body}");
    assert_eq!(body["password_state"], "set_in_sql");
    assert_eq!(body["database"], "analytics");
    assert_eq!(body["uri"], format!("postgresql://etl@{ep}.localtest.me:4432/analytics?sslmode=verify-full"));

    // An MD5 role.
    let body: Value = t.send(Method::GET, &format!("{conn}?role=old"), None).await.json().await.unwrap();
    assert!(body["password"].is_null());
    assert_eq!(body["password_state"], "md5_unusable");

    // The system database is pickable; the user and database are percent-encoded in the uri.
    let body: Value = t.send(Method::GET, &format!("{conn}?role=My%20Role%40x&database=postgres"), None).await.json().await.unwrap();
    assert_eq!(body["user"], "My Role@x");
    assert_eq!(body["database"], "postgres");
    assert_eq!(body["uri"], format!("postgresql://My%20Role%40x@{ep}.localtest.me:4432/postgres?sslmode=verify-full"));

    // Unknown database, unknown role, NUL: 404, 404, 422.
    let err = error_of(t.send(Method::GET, &format!("{conn}?database=ghost"), None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    let err = error_of(t.send(Method::GET, &format!("{conn}?role=ghost"), None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    let err = error_of(t.send(Method::GET, &format!("{conn}?role=a%00b"), None).await, 422).await;
    assert_eq!(code_of(&err), "invalid_role_name");
    // Any Postgres name is a lookup now, so a platform role is just not there.
    let err = error_of(t.send(Method::GET, &format!("{conn}?role=anon"), None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");

    // A backup is refused whatever the role.
    let backup = t.backup(dev);
    let err = error_of(t.send(Method::GET, &format!("{}/connection?role=app", t.branch(backup)), None).await, 409).await;
    assert_eq!(code_of(&err), "backup_branch");
}

#[tokio::test]
async fn owner_set_in_sql_reveals_no_password() {
    let t = branching().await;
    let dev = t.ready("dev"); // no sealed owner password
    t.metadata.add_branch_db(dev, "web1", "postgres", DatabaseOrigin::Main);
    let ep = t.metadata.branch(dev).endpoint_id.unwrap();
    let resp = t.send(Method::GET, &format!("{}/connection", t.branch(dev)), None).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["user"], "postgres");
    assert!(body["password"].is_null(), "{body}");
    assert_eq!(body["password_state"], "set_in_sql");
    assert_eq!(body["uri"], format!("postgresql://postgres@{ep}.localtest.me:4432/web1?sslmode=verify-full"));
}

#[tokio::test]
async fn list_branch_databases() {
    let t = branching().await;
    let dev = t.ready("dev");
    t.metadata.add_branch_db(dev, "postgres", "cloud_admin", DatabaseOrigin::System);
    t.metadata.add_branch_db(dev, "zeta", "etl", DatabaseOrigin::Sql);
    t.metadata.add_branch_db(dev, "web1", "postgres", DatabaseOrigin::Main);
    t.metadata.add_branch_db(dev, "alpha", "etl", DatabaseOrigin::Sql);
    let resp = t.send(Method::GET, &format!("{}/databases", t.branch(dev)), None).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(
        body,
        json!([
            { "name": "web1", "owner": "postgres", "origin": "main", "main": true, "data_api": "off", "data_api_error": null, "default_schema": "public" },
            { "name": "alpha", "owner": "etl", "origin": "sql", "main": false, "data_api": "off", "data_api_error": null, "default_schema": "public" },
            { "name": "postgres", "owner": "cloud_admin", "origin": "system", "main": false, "data_api": "off", "data_api_error": null, "default_schema": "public" },
            { "name": "zeta", "owner": "etl", "origin": "sql", "main": false, "data_api": "off", "data_api_error": null, "default_schema": "public" },
        ])
    );
    let backup = t.backup(dev);
    let err = error_of(t.send(Method::GET, &format!("{}/databases", t.branch(backup)), None).await, 409).await;
    assert_eq!(code_of(&err), "backup_branch");
    assert_eq!(t.send(Method::GET, &format!("{}/databases", t.branch(BranchId::new())), None).await.status(), 404);
}

#[tokio::test]
async fn main_database_missing_is_reported() {
    let t = branching().await;
    let dev = t.ready("dev");
    // `dev` has a database row but no `main` one: the branch says so; the reveal still names
    // the recorded main database (R24).
    t.metadata.add_branch_db(dev, "other", "etl", DatabaseOrigin::Sql);
    let list: Vec<Value> = t.send(Method::GET, &t.branches(), None).await.json().await.unwrap();
    let by = |id: BranchId| list.iter().find(|b| b["id"] == id.as_uuid().to_string()).unwrap().clone();
    assert_eq!(by(dev)["main_database_missing"], true);
    let body: Value = t.send(Method::GET, &format!("{}/connection", t.branch(dev)), None).await.json().await.unwrap();
    assert_eq!(body["database"], "web1");
    // With the main row present, it isn't missing.
    t.metadata.add_branch_db(dev, "web1", "postgres", DatabaseOrigin::Main);
    let list: Vec<Value> = t.send(Method::GET, &t.branches(), None).await.json().await.unwrap();
    let dev_row = list.iter().find(|b| b["id"] == dev.as_uuid().to_string()).unwrap();
    assert_eq!(dev_row["main_database_missing"], false);
}

// C9: the list asks for the main databases of all its ready branches in one query.
#[tokio::test]
async fn list_branches_reads_databases_once() {
    let t = branching().await;
    let a = t.ready("a");
    let b = t.ready("b");
    t.ready("c");
    t.metadata.add_branch_db(a, "web1", "postgres", DatabaseOrigin::Main);
    let list: Vec<Value> = t.send(Method::GET, &t.branches(), None).await.json().await.unwrap();
    let missing = |id: BranchId| list.iter().find(|x| x["id"] == id.as_uuid().to_string()).unwrap()["main_database_missing"].clone();
    assert_eq!(missing(a), false);
    assert_eq!(missing(b), true);
    assert_eq!(t.metadata.main_db_batch_queries.load(Ordering::SeqCst), 1);
    assert_eq!(t.metadata.branch_db_queries.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn update_branch_protected() {
    let t = branching().await;
    let dev = t.ready("dev");
    let resp = t.send(Method::PATCH, &t.branch(dev), Some(json!({ "protected": true }))).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["id"], dev.as_uuid().to_string());
    assert_eq!(body["protected"], true);
    assert!(t.metadata.branch(dev).protected);
    let body: Value = t.send(Method::PATCH, &t.branch(dev), Some(json!({ "protected": false }))).await.json().await.unwrap();
    assert_eq!(body["protected"], false);
    // Main can be protected too.
    assert_eq!(t.send(Method::PATCH, &t.branch(t.main()), Some(json!({ "protected": true }))).await.status(), 200);
    // A backup: 409; unknown: 404; a deleting branch: 409 branch_not_ready.
    let backup = t.backup(dev);
    let err = error_of(t.send(Method::PATCH, &t.branch(backup), Some(json!({ "protected": true }))).await, 409).await;
    assert_eq!(code_of(&err), "backup_branch");
    assert_eq!(t.send(Method::PATCH, &t.branch(BranchId::new()), Some(json!({ "protected": true }))).await.status(), 404);
    let deleting = t.add("gone", t.main(), BranchKind::Normal, BranchStatus::Deleting);
    let err = error_of(t.send(Method::PATCH, &t.branch(deleting), Some(json!({ "protected": true }))).await, 409).await;
    assert_eq!(code_of(&err), "branch_not_ready");
    // Unknown or missing field: 422; a bad body: 400.
    for body in [json!({ "protected": true, "name": "x" }), json!({}), json!({ "protected": "yes" })] {
        let err = error_of(t.send(Method::PATCH, &t.branch(dev), Some(body.clone())).await, 422).await;
        assert_eq!(code_of(&err), "invalid_request", "{body}");
    }
    // Malformed JSON: 400.
    let resp = reqwest::Client::new()
        .patch(format!("{}{}", t.app.base, t.branch(dev)))
        .header(IDENTITY_HEADER, t.app.token_for_org("org-1"))
        .header("content-type", "application/json")
        .body("{\"protected\": tru")
        .send()
        .await
        .unwrap();
    let err = error_of(resp, 400).await;
    assert_eq!(code_of(&err), "invalid_request");
    assert!(!t.metadata.branch(dev).protected);
    // Needs the management credential.
    let resp = reqwest::Client::new()
        .patch(format!("{}{}", t.app.base, t.branch(dev)))
        .json(&json!({ "protected": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
}

#[tokio::test]
async fn protected_branch_refuses_delete_and_reset_but_not_restore() {
    let t = branching().await;
    let dev = t.ready("dev");
    t.metadata.branches.lock().unwrap().get_mut(&dev).unwrap().protected = true;
    let expect = |err: &Value| {
        assert_eq!(code_of(err), "branch_protected");
        assert_eq!(message_of(err), "this branch is protected; unprotect it first");
    };
    expect(&error_of(t.send(Method::DELETE, &t.branch(dev), None).await, 409).await);
    expect(&error_of(t.send(Method::POST, &format!("{}/reset-to-parent", t.branch(dev)), None).await, 409).await);
    assert_eq!(t.metadata.branch(dev).status, BranchStatus::Ready);
    assert!(t.published().is_empty());
    // A non-ready (failed) protected branch refuses a delete as well.
    let failed = t.add("bad", t.main(), BranchKind::Normal, BranchStatus::Failed);
    t.metadata.branches.lock().unwrap().get_mut(&failed).unwrap().protected = true;
    expect(&error_of(t.send(Method::DELETE, &t.branch(failed), None).await, 409).await);
    assert_eq!(t.metadata.branch(failed).status, BranchStatus::Failed);
    // Restore rewinds a protected branch (its previous state is kept as a backup).
    let resp = t
        .send(Method::POST, &format!("{}/restore", t.branch(dev)), Some(json!({ "at": { "timestamp": "2026-01-01T00:00:00Z" } })))
        .await;
    assert_eq!(resp.status(), 202);
    // Unprotected, the delete goes through.
    let other = t.ready("other");
    assert_eq!(t.send(Method::DELETE, &t.branch(other), None).await.status(), 202);
}

// T6: a protect that lands after the API's check is still refused by the delete itself; a
// claimed (ready) branch is released.
#[tokio::test]
async fn delete_refused_when_protected_after_the_check() {
    let t = branching().await;
    let dev = t.ready("dev");
    let failed = t.add("bad", t.main(), BranchKind::Normal, BranchStatus::Failed);
    for (branch, status) in [(dev, BranchStatus::Ready), (failed, BranchStatus::Failed)] {
        *t.metadata.protect_on_delete.lock().unwrap() = Some(branch);
        let err = error_of(t.send(Method::DELETE, &t.branch(branch), None).await, 409).await;
        assert_eq!(code_of(&err), "branch_protected", "{status:?}");
        assert_eq!(t.metadata.branch(branch).status, status);
    }
    assert!(t.published().is_empty());
}

#[tokio::test]
async fn database_delete_refused_with_a_protected_branch() {
    let t = branching().await;
    let dev = t.ready("dev");
    t.metadata.branches.lock().unwrap().get_mut(&dev).unwrap().protected = true;
    let path = format!("/v1/databases/{}", t.db.as_uuid());
    let err = error_of(t.send(Method::DELETE, &path, None).await, 409).await;
    assert_eq!(code_of(&err), "branch_protected");
    assert_eq!(message_of(&err), "a branch of this database is protected; unprotect it first");
    assert!(t.jobs.deleted.lock().unwrap().is_empty());
    assert_eq!(t.metadata.database(t.db).status, DatabaseStatus::Ready);
    t.metadata.branches.lock().unwrap().get_mut(&dev).unwrap().protected = false;
    assert_eq!(t.send(Method::DELETE, &path, None).await.status(), 202);
}

#[tokio::test]
async fn branch_responses_carry_protected() {
    let t = branching().await;
    let dev = t.ready("dev");
    t.metadata.branches.lock().unwrap().get_mut(&dev).unwrap().protected = true;
    let list: Vec<Value> = t.send(Method::GET, &t.branches(), None).await.json().await.unwrap();
    let flag = |id: BranchId| list.iter().find(|b| b["id"] == id.as_uuid().to_string()).unwrap()["protected"].clone();
    assert_eq!(flag(dev), true);
    assert_eq!(flag(t.main()), false);
    let resp = t.send(Method::POST, &t.branches(), Some(json!({ "name": "feat" }))).await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["protected"], false);
    // A branch that isn't ready yet has no databases to miss.
    assert_eq!(body["main_database_missing"], false);
}

#[tokio::test]
async fn connection_role_sealed_for_another_branch_is_500_credentials_unavailable() {
    let t = branching().await;
    let dev = t.ready("dev");
    let ep = t.metadata.branch(dev).endpoint_id.unwrap();
    t.metadata.add_role(&ep, "app", RoleOrigin::Api, false, false);
    t.metadata.role_passwords.lock().unwrap().insert(
        (ep, "app".into()),
        chelabase_secrets::seal_role_password(&t.app.key, &BranchId::new().to_string(), "app", "x"),
    );
    let err = error_of(t.send(Method::GET, &format!("{}/connection?role=app", t.branch(dev)), None).await, 500).await;
    assert_eq!(code_of(&err), "credentials_unavailable");
}

#[tokio::test]
async fn role_routes_need_management_auth() {
    let t = branching().await;
    let dev = t.ready("dev");
    for (method, suffix) in [
        (Method::GET, ""),
        (Method::POST, ""),
        (Method::POST, "/app/reset-password"),
        (Method::DELETE, "/app"),
    ] {
        let resp = reqwest::Client::new()
            .request(method.clone(), format!("{}{}{suffix}", t.app.base, t.roles(dev)))
            .json(&json!({ "name": "app" }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401, "{method} {suffix}");
    }
    let resp = reqwest::Client::new()
        .get(format!("{}{}/connection?role=app", t.app.base, t.branch(dev)))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    assert!(t.jobs.role_calls.lock().unwrap().is_empty());
}


// ---- the data API per Postgres database (step 6c)

type Invalidations = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// A rest stand-in that records each invalidation: (branch, authorization).
async fn mock_rest_invalidate() -> (String, Invalidations) {
    use axum::extract::Path;
    use axum::http::HeaderMap;
    use axum::routing::post;
    let seen: Invalidations = Arc::default();
    let log = seen.clone();
    let app = Router::new().route(
        "/internal/branches/{branch}/invalidate",
        post(move |Path(branch): Path<String>, headers: HeaderMap| {
            let log = log.clone();
            async move {
                let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).map(str::to_string);
                log.lock().unwrap().push((branch, auth));
                axum::http::StatusCode::NO_CONTENT
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

/// Like `branching()`, with a rest that records invalidations and the given database cap.
async fn branching_with_rest(cap: u32) -> (Branching, Invalidations) {
    let (rest, seen) = mock_rest_invalidate().await;
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app_full(metadata.clone(), jobs.clone(), &rest, Mode::SelfHosted, cap).await;
    let db = ready_database(tenant, &app.key);
    let id = db.database_id;
    metadata.insert_database(db);
    (Branching { app, metadata, jobs, db: id }, seen)
}

impl Branching {
    fn pg(&self, branch: BranchId, name: &str) -> String {
        format!("{}/pg-databases/{name}", self.branch(branch))
    }

    /// The main branch with `web1` (main, on) and `analytics`, `scratch` (sql, off), and the
    /// system `postgres` database.
    fn seeded(&self) -> BranchId {
        let main = self.main();
        self.metadata.add_branch_db(main, "web1", "postgres", DatabaseOrigin::Main);
        self.metadata.set_data_api(main, "web1", chelabase_metadata::DataApiState::On);
        self.metadata.add_branch_db(main, "analytics", "etl", DatabaseOrigin::Sql);
        self.metadata.add_branch_db(main, "scratch", "etl", DatabaseOrigin::Sql);
        self.metadata.add_branch_db(main, "postgres", "cloud_admin", DatabaseOrigin::System);
        main
    }

    fn data_api_calls(&self) -> Vec<String> {
        self.jobs.data_api_calls.lock().unwrap().clone()
    }
}

impl FakeMetadata {
    fn set_data_api(&self, branch: BranchId, name: &str, state: chelabase_metadata::DataApiState) {
        let mut all = self.branch_dbs.lock().unwrap();
        let row = all.get_mut(&branch).unwrap().iter_mut().find(|d| d.name == name).unwrap();
        row.data_api = state;
        // As the store does: any state but off means an enable began.
        row.data_api_bootstrapped |= state != chelabase_metadata::DataApiState::Off;
    }

    fn pg_database(&self, branch: BranchId, name: &str) -> BranchDatabase {
        self.branch_dbs.lock().unwrap().get(&branch).unwrap().iter().find(|d| d.name == name).cloned().unwrap()
    }

    fn key_databases(&self, id: uuid::Uuid) -> Option<Vec<String>> {
        self.keys.lock().unwrap().iter().find(|(k, _)| k.id == id).unwrap().0.databases.clone()
    }
}

async fn wait_for_invalidations(seen: &Invalidations, n: usize) {
    for _ in 0..100 {
        if seen.lock().unwrap().len() >= n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn enable_answers_202_and_publishes() {
    let t = branching().await;
    let main = t.seeded();
    let resp = t.send(Method::POST, &format!("{}/data-api", t.pg(main, "analytics")), None).await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["name"], "analytics");
    assert_eq!(body["data_api"], "enabling");
    assert!(body["data_api_error"].is_null());
    assert_eq!(t.data_api_calls(), vec!["enable:analytics"]);
    assert_eq!(t.metadata.pg_database(main, "analytics").data_api, chelabase_metadata::DataApiState::Enabling);

    // Pressing it again while it is enabling starts the job again (a retry).
    assert_eq!(t.send(Method::POST, &format!("{}/data-api", t.pg(main, "analytics")), None).await.status(), 202);
    assert_eq!(t.data_api_calls().len(), 2);

    // Already served: 200, nothing published.
    let resp = t.send(Method::POST, &format!("{}/data-api", t.pg(main, "web1")), None).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<Value>().await.unwrap()["data_api"], "on");
    assert_eq!(t.data_api_calls().len(), 2);

    // A lost publish is healed by reconcile (the row stays `enabling`): still 202.
    *t.jobs.fail_enable_publish.lock().unwrap() = true;
    assert_eq!(t.send(Method::POST, &format!("{}/data-api", t.pg(main, "scratch")), None).await.status(), 202);
    assert_eq!(t.metadata.pg_database(main, "scratch").data_api, chelabase_metadata::DataApiState::Enabling);
}

#[tokio::test]
async fn enable_refusals() {
    let (t, seen) = branching_with_rest(2).await;
    let main = t.seeded();
    let enable = |name: &str| format!("{}/data-api", t.pg(main, name));
    // System: 422.
    let err = error_of(t.send(Method::POST, &enable("postgres"), None).await, 422).await;
    assert_eq!(code_of(&err), "invalid_request");
    // Unknown database and unknown branch: 404.
    let err = error_of(t.send(Method::POST, &enable("nope"), None).await, 404).await;
    assert_eq!(code_of(&err), "not_found");
    assert_eq!(t.send(Method::POST, &format!("{}/data-api", t.pg(BranchId::new(), "x")), None).await.status(), 404);
    // Cap 2 with web1 on: one more fits, the next doesn't.
    assert_eq!(t.send(Method::POST, &enable("analytics"), None).await.status(), 202);
    let err = error_of(t.send(Method::POST, &enable("scratch"), None).await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    assert_eq!(message_of(&err), "this branch already serves 2 databases (the cap is 2)");
    assert_eq!(t.data_api_calls(), vec!["enable:analytics"]);
    assert_eq!(t.metadata.pg_database(main, "scratch").data_api, chelabase_metadata::DataApiState::Off);
    // A branch that isn't ready, and a backup.
    let pending = t.add("wip", main, BranchKind::Normal, BranchStatus::Pending);
    let err = error_of(t.send(Method::POST, &format!("{}/data-api", t.pg(pending, "x")), None).await, 409).await;
    assert_eq!(code_of(&err), "branch_not_ready");
    let backup = t.backup(main);
    let err = error_of(t.send(Method::POST, &format!("{}/data-api", t.pg(backup, "x")), None).await, 409).await;
    assert_eq!(code_of(&err), "backup_branch");
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn disable_main_is_refused() {
    let (t, seen) = branching_with_rest(10).await;
    let main = t.seeded();
    let err = error_of(t.send(Method::DELETE, &format!("{}/data-api", t.pg(main, "web1")), None).await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    assert!(t.data_api_calls().is_empty());
    assert_eq!(t.metadata.pg_database(main, "web1").data_api, chelabase_metadata::DataApiState::On);
    assert!(seen.lock().unwrap().is_empty());
    // Unknown: 404; the system database was never served: 422.
    assert_eq!(t.send(Method::DELETE, &format!("{}/data-api", t.pg(main, "nope")), None).await.status(), 404);
    assert_eq!(t.send(Method::DELETE, &format!("{}/data-api", t.pg(main, "postgres")), None).await.status(), 422);
    assert!(t.data_api_calls().is_empty());
}

#[tokio::test]
async fn disable_runs_the_job_then_invalidates_rest() {
    let (t, seen) = branching_with_rest(10).await;
    let main = t.seeded();
    t.metadata.set_data_api(main, "analytics", chelabase_metadata::DataApiState::On);
    let path = format!("{}/data-api", t.pg(main, "analytics"));
    assert_eq!(t.send(Method::DELETE, &path, None).await.status(), 204);
    assert_eq!(t.send(Method::DELETE, &format!("{path}?remove=true"), None).await.status(), 204);
    assert_eq!(t.data_api_calls(), vec!["disable:analytics:false", "disable:analytics:true"]);
    // The invalidation is awaited before the answer: it is already there.
    let expected = (main.as_uuid().to_string(), Some(format!("Bearer {REST_TOKEN}")));
    assert_eq!(*seen.lock().unwrap(), vec![expected.clone(), expected]);
    // A bad query is a 400, not a silent false.
    assert_eq!(t.send(Method::DELETE, &format!("{path}?remove=maybe"), None).await.status(), 400);

    // The worker's refusals map to statuses and do not invalidate.
    for (reply, status, code) in [
        (Ok(CheckDataRoleReply::not_ready()), 409, "branch_not_ready"),
        (Ok(CheckDataRoleReply::refused("database is in use")), 409, "conflict"),
        (Ok(CheckDataRoleReply::failed("boom")), 502, "unavailable"),
        (Err(ResetError::Timeout), 504, "timeout"),
        (Err(ResetError::Transport("no worker listening".into())), 503, "worker_unavailable"),
    ] {
        *t.jobs.data_api_reply.lock().unwrap() = Some(reply);
        let err = error_of(t.send(Method::DELETE, &path, None).await, status).await;
        assert_eq!(code_of(&err), code);
        assert!(!err.to_string().contains("boom"), "{err}");
    }
    assert_eq!(seen.lock().unwrap().len(), 2);
    // The refused text is the worker's.
    *t.jobs.data_api_reply.lock().unwrap() = Some(Ok(CheckDataRoleReply::refused("database is in use")));
    let err = error_of(t.send(Method::DELETE, &path, None).await, 409).await;
    assert_eq!(message_of(&err), "database is in use");
}

/// `remove` on a database no enable ever began on is refused: whatever `chelabase` schema it
/// holds isn't the data API's. Disabling it (already off) without `remove` still answers 204.
#[tokio::test]
async fn remove_on_a_database_never_enabled_is_refused() {
    let (t, seen) = branching_with_rest(10).await;
    let main = t.seeded();
    let path = format!("{}/data-api", t.pg(main, "scratch"));
    let err = error_of(t.send(Method::DELETE, &format!("{path}?remove=true"), None).await, 409).await;
    assert_eq!(code_of(&err), "conflict");
    assert_eq!(
        message_of(&err),
        "the data API was never enabled on the database \"scratch\"; there is nothing to remove"
    );
    assert!(t.data_api_calls().is_empty());
    assert!(seen.lock().unwrap().is_empty());
    assert_eq!(t.send(Method::DELETE, &path, None).await.status(), 204);
    // Off after an enable began: the machinery may be there, so it can be removed.
    t.metadata.set_data_api(main, "scratch", chelabase_metadata::DataApiState::On);
    t.metadata.set_data_api(main, "scratch", chelabase_metadata::DataApiState::Off);
    assert_eq!(t.send(Method::DELETE, &format!("{path}?remove=true"), None).await.status(), 204);
    assert_eq!(t.data_api_calls(), vec!["disable:scratch:false", "disable:scratch:true"]);
}

#[tokio::test]
async fn disable_survives_rest_being_down() {
    let t = branching().await; // rest points at a closed port
    let main = t.seeded();
    t.metadata.set_data_api(main, "analytics", chelabase_metadata::DataApiState::On);
    let path = format!("{}/data-api", t.pg(main, "analytics"));
    assert_eq!(t.send(Method::DELETE, &path, None).await.status(), 204);
}

#[tokio::test]
async fn default_schema_patch_validates_and_invalidates() {
    let (t, seen) = branching_with_rest(10).await;
    let main = t.seeded();
    let path = t.pg(main, "web1");
    for bad in [json!({ "default_schema": "" }), json!({ "default_schema": "Public" }), json!({ "default_schema": "1app" }),
        json!({ "default_schema": "a-b" }), json!({ "default_schema": "a".repeat(64) }), json!({ "default_schema": 5 }),
        json!({}), json!({ "default_schema": "app", "extra": 1 })] {
        assert_eq!(t.send(Method::PATCH, &path, Some(bad.clone())).await.status(), 422, "{bad}");
    }
    assert_eq!(t.metadata.pg_database(main, "web1").default_schema, "public");
    assert!(t.data_api_calls().is_empty());

    // A served database: stored, bootstrap re-run, rest told.
    let resp = t.send(Method::PATCH, &path, Some(json!({ "default_schema": "app" }))).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!((body["name"].as_str(), body["default_schema"].as_str()), (Some("web1"), Some("app")));
    assert_eq!(t.metadata.pg_database(main, "web1").default_schema, "app");
    assert_eq!(t.data_api_calls(), vec!["rebootstrap:web1"]);
    wait_for_invalidations(&seen, 1).await;
    assert_eq!(seen.lock().unwrap().len(), 1);

    // The same value again changes nothing: no job, no invalidation.
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "default_schema": "app" }))).await.status(), 200);
    assert_eq!(t.data_api_calls().len(), 1);
    assert_eq!(seen.lock().unwrap().len(), 1);

    // A database that isn't served just stores it.
    let resp = t.send(Method::PATCH, &t.pg(main, "analytics"), Some(json!({ "default_schema": "reports" }))).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(t.metadata.pg_database(main, "analytics").default_schema, "reports");
    assert_eq!(t.data_api_calls().len(), 1);
    assert_eq!(seen.lock().unwrap().len(), 1);

    // Unknown database 404; the system database 422 (as enabling it); a failed bootstrap is
    // reported.
    assert_eq!(t.send(Method::PATCH, &t.pg(main, "nope"), Some(json!({ "default_schema": "app" }))).await.status(), 404);
    let err = error_of(t.send(Method::PATCH, &t.pg(main, "postgres"), Some(json!({ "default_schema": "app" }))).await, 422).await;
    assert_eq!(message_of(&err), "the system database \"postgres\" is never served");
    assert_eq!(t.metadata.pg_database(main, "postgres").default_schema, "public");
    *t.jobs.data_api_reply.lock().unwrap() = Some(Ok(CheckDataRoleReply::failed("boom")));
    let err = error_of(t.send(Method::PATCH, &path, Some(json!({ "default_schema": "other" }))).await, 502).await;
    assert_eq!(code_of(&err), "unavailable");
    // The old schema is put back: what is stored is what the bootstrap covers. rest may
    // have read the new one meanwhile, so it's told again.
    assert_eq!(t.metadata.pg_database(main, "web1").default_schema, "app");
    wait_for_invalidations(&seen, 2).await;
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn patch_default_schema_passes_grant_existing() {
    let t = branching().await;
    let main = t.seeded();
    let path = t.pg(main, "web1");
    // Off by default.
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "default_schema": "app" }))).await.status(), 200);
    assert_eq!(t.data_api_calls(), vec!["rebootstrap:web1"]);
    // Asked: the flag reaches the worker, with a changed schema...
    let resp = t.send(Method::PATCH, &path, Some(json!({ "default_schema": "app2", "grant_existing": true }))).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(t.metadata.pg_database(main, "web1").default_schema, "app2");
    // ...and with an unchanged one (so a user can grant later).
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "default_schema": "app2", "grant_existing": true }))).await.status(), 200);
    assert_eq!(t.data_api_calls(), vec!["rebootstrap:web1", "rebootstrap:web1:grant_existing", "rebootstrap:web1:grant_existing"]);
    // Explicitly false and unchanged: nothing runs.
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "default_schema": "app2", "grant_existing": false }))).await.status(), 200);
    assert_eq!(t.data_api_calls().len(), 3);
    // A failed grant on an unchanged schema leaves the stored schema alone.
    *t.jobs.data_api_reply.lock().unwrap() = Some(Ok(CheckDataRoleReply::failed("boom")));
    error_of(t.send(Method::PATCH, &path, Some(json!({ "default_schema": "app2", "grant_existing": true }))).await, 502).await;
    assert_eq!(t.metadata.pg_database(main, "web1").default_schema, "app2");
    // Not a bool: rejected.
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "default_schema": "app", "grant_existing": "yes" }))).await.status(), 422);
}

#[tokio::test]
async fn grant_existing_on_a_disabled_database_is_409() {
    let t = branching().await;
    let main = t.seeded();
    let path = t.pg(main, "analytics"); // off
    let err = error_of(t.send(Method::PATCH, &path, Some(json!({ "default_schema": "public", "grant_existing": true }))).await, 409).await;
    assert_eq!(code_of(&err), "data_api_disabled");
    // A changed schema on a database that isn't served is only stored; without the flag too.
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "default_schema": "public" }))).await.status(), 200);
    assert!(t.data_api_calls().is_empty());
}

#[tokio::test]
async fn update_database_no_longer_takes_default_schema() {
    let t = branching().await;
    let before = t.metadata.database(t.db);
    let resp = t.send(Method::PATCH, &database_path(&t), Some(json!({ "default_schema": "app" }))).await;
    assert_eq!(resp.status(), 422);
    let after = t.metadata.database(t.db);
    assert_eq!((before.max_rows, before.max_connections), (after.max_rows, after.max_connections));
    let body: Value = t.send(Method::PATCH, &database_path(&t), Some(json!({}))).await.json().await.unwrap();
    assert!(body.get("default_schema").is_none(), "{body}");
}

#[tokio::test]
async fn key_allow_list_validation() {
    let t = branching().await;
    let main = t.seeded();
    let (public_id, _) = t.seed_public_key(main);
    let (secret_id, _) = t.seed_secret_key(main, "ci", "reporting");
    let too_many: Vec<String> = (0..21).map(|i| format!("db{i}")).collect();
    let long = "a".repeat(64);
    let invalid: Vec<(&str, Value)> = vec![
        ("empty list", json!([])),
        ("empty name", json!([""])),
        ("64 bytes", json!([long])),
        ("21 names", json!(too_many)),
        ("duplicate", json!(["web1", "web1"])),
        ("unknown", json!(["web1", "nope"])),
        ("system", json!(["postgres"])),
        ("not a name", json!([1])),
        ("not a list", json!("web1")),
    ];
    for (what, list) in &invalid {
        let resp = t.send(Method::POST, &t.keys(main), Some(json!({ "name": "k", "databases": list }))).await;
        assert_eq!(resp.status(), 422, "create: {what}");
        assert_eq!(code_of(&resp.json::<Value>().await.unwrap()), "invalid_request");
        for id in [secret_id, public_id] {
            let resp = t.send(Method::PATCH, &format!("{}/{id}", t.keys(main)), Some(json!({ "databases": list }))).await;
            assert_eq!(resp.status(), 422, "patch: {what}");
        }
    }
    assert_eq!(t.metadata.keys.lock().unwrap().len(), 2, "nothing was created");
    assert_eq!(t.metadata.key_databases(secret_id), None);
    // PATCH must say what it means: a missing `databases` is not "every database".
    let resp = t.send(Method::PATCH, &format!("{}/{secret_id}", t.keys(main)), Some(json!({}))).await;
    assert_eq!(resp.status(), 422);

    // A disabled database (and the main one) is allowed; the list comes back on create and list.
    let resp = t
        .send(Method::POST, &t.keys(main), Some(json!({ "name": "k", "databases": ["web1", "analytics"] })))
        .await;
    assert_eq!(resp.status(), 201);
    let created: Value = resp.json().await.unwrap();
    assert_eq!(created["databases"], json!(["web1", "analytics"]));
    let id: uuid::Uuid = created["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(t.metadata.key_databases(id), Some(vec!["web1".into(), "analytics".into()]));
    let listed = t.list_keys(main).await;
    let by = |id: uuid::Uuid| listed.iter().find(|k| k["id"] == json!(id)).unwrap();
    assert_eq!(by(id)["databases"], json!(["web1", "analytics"]));
    assert!(by(secret_id)["databases"].is_null());
    // Omitted and null both mean every database.
    for body in [json!({ "name": "a" }), json!({ "name": "b", "databases": null })] {
        let created: Value = t.send(Method::POST, &t.keys(main), Some(body)).await.json().await.unwrap();
        assert!(created["databases"].is_null(), "{created}");
    }
    // A branch's list is checked against that branch's own databases.
    let dev = t.ready("dev");
    let resp = t.send(Method::POST, &t.keys(dev), Some(json!({ "name": "k", "databases": ["web1"] }))).await;
    assert_eq!(resp.status(), 422);
    // Rotation keeps the list.
    let resp = t.send(Method::POST, &format!("{}/{id}/rotate", t.keys(main)), None).await;
    assert_eq!(resp.status(), 201);
    let rotated: Value = resp.json().await.unwrap();
    assert_eq!(rotated["databases"], json!(["web1", "analytics"]));
}

#[tokio::test]
async fn patch_key_databases_invalidates() {
    let (t, seen) = branching_with_rest(10).await;
    let main = t.seeded();
    let (public_id, _) = t.seed_public_key(main);
    let (id, _) = t.seed_secret_key(main, "ci", "reporting");
    let path = format!("{}/{id}", t.keys(main));
    let main_text = main.as_uuid().to_string();

    // Narrowing from "every database": stored, the key returned, rest told.
    let resp = t.send(Method::PATCH, &path, Some(json!({ "databases": ["web1"] }))).await;
    assert_eq!(resp.status(), 200);
    let key: Value = resp.json().await.unwrap();
    assert_eq!((key["id"].as_str(), key["databases"].clone()), (Some(id.to_string().as_str()), json!(["web1"])));
    assert_eq!(key["kind"], "secret");
    assert!(key.get("key").is_none(), "{key}");
    assert_eq!(t.metadata.key_databases(id), Some(vec!["web1".into()]));
    wait_for_invalidations(&seen, 1).await;
    assert_eq!(seen.lock().unwrap()[0], (main_text.clone(), Some(format!("Bearer {REST_TOKEN}"))));

    // Changing the list invalidates; so does widening back to null (the old tokens are stale).
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "databases": ["analytics", "web1"] }))).await.status(), 200);
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "databases": null }))).await.status(), 200);
    assert_eq!(t.metadata.key_databases(id), None);
    assert_eq!(seen.lock().unwrap().len(), 3);
    // The same list again changes nothing and tells no one.
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "databases": null }))).await.status(), 200);
    assert_eq!(seen.lock().unwrap().len(), 3);

    // It works on the public key too (and shows the key in full, as the list does).
    let resp = t.send(Method::PATCH, &format!("{}/{public_id}", t.keys(main)), Some(json!({ "databases": ["web1"] }))).await;
    assert_eq!(resp.status(), 200);
    assert!(resp.json::<Value>().await.unwrap()["key"].is_string());
    assert_eq!(seen.lock().unwrap().len(), 4);

    // Unknown and revoked keys, and another branch's key: 404.
    let missing = format!("{}/{}", t.keys(main), uuid::Uuid::new_v4());
    assert_eq!(t.send(Method::PATCH, &missing, Some(json!({ "databases": null }))).await.status(), 404);
    assert_eq!(t.send(Method::DELETE, &path, None).await.status(), 204);
    assert_eq!(t.send(Method::PATCH, &path, Some(json!({ "databases": null }))).await.status(), 404);
    let dev = t.ready("dev");
    let (other, _) = t.seed_secret_key(main, "other", "reporting");
    assert_eq!(t.send(Method::PATCH, &format!("{}/{other}", t.keys(dev)), Some(json!({ "databases": null }))).await.status(), 404);
    assert_eq!(seen.lock().unwrap().len(), 4);
}

#[test]
fn the_database_cap_is_read_from_the_environment() {
    use orchestrator::data_api_database_cap;
    assert_eq!(data_api_database_cap(None), Ok(10));
    assert_eq!(data_api_database_cap(Some("")), Ok(10));
    assert_eq!(data_api_database_cap(Some("  ")), Ok(10));
    assert_eq!(data_api_database_cap(Some("1")), Ok(1));
    assert_eq!(data_api_database_cap(Some(" 100 ")), Ok(100));
    for bad in ["0", "101", "-1", "ten", "1.5"] {
        let err = data_api_database_cap(Some(bad)).unwrap_err();
        assert!(err.contains("CHELABASE_BRANCH_DATA_API_DATABASE_CAP"), "{bad}: {err}");
    }
}

// ---- Studio's console (step 6c, task 6)

/// One request the rest stand-in received: its path, every header as `name: value` lines,
/// and its body.
#[derive(Debug, Clone)]
struct RestCall {
    path: String,
    query: String,
    headers: String,
    body: String,
}

type RestCalls = Arc<Mutex<Vec<RestCall>>>;

type RestAnswer = Arc<dyn Fn(&Value) -> (u16, Vec<(&'static str, &'static str)>, Value) + Send + Sync>;

/// A rest stand-in that records every request (on any path) and answers the console route
/// with `answer(body)` and the schema route with `answer(null)`; other paths answer 404.
async fn mock_rest_console(answer: RestAnswer) -> (String, RestCalls) {
    use axum::extract::Request;
    let calls: RestCalls = Arc::default();
    let log = calls.clone();
    let app = Router::new().fallback(move |req: Request| {
        let log = log.clone();
        let answer = answer.clone();
        async move {
            let path = req.uri().path().to_string();
            let query = req.uri().query().unwrap_or_default().to_string();
            let headers = req
                .headers()
                .iter()
                .map(|(k, v)| format!("{k}: {}", v.to_str().unwrap_or("")))
                .collect::<Vec<_>>()
                .join("\n");
            let bytes = axum::body::to_bytes(req.into_body(), usize::MAX).await.unwrap();
            let body = String::from_utf8_lossy(&bytes).into_owned();
            log.lock().unwrap().push(RestCall { path: path.clone(), query, headers, body: body.clone() });
            let schema = path.starts_with("/internal/schema/");
            if !schema && !(path.starts_with("/internal/console/") && path.ends_with("/query")) {
                return axum::response::IntoResponse::into_response(axum::http::StatusCode::NOT_FOUND);
            }
            let request: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let (status, headers, json) = answer(&request);
            let mut resp = axum::response::IntoResponse::into_response((
                axum::http::StatusCode::from_u16(status).unwrap(),
                axum::Json(json),
            ));
            for (k, v) in headers {
                resp.headers_mut().insert(k, axum::http::HeaderValue::from_static(v));
            }
            resp
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, calls)
}

/// `branching()` with a rest stand-in for the console and the seeded databases: `web1`
/// (main, on), `analytics` and `scratch` (off), and the system `postgres`.
async fn console_app(answer: RestAnswer) -> (Branching, RestCalls) {
    let (rest, calls) = mock_rest_console(answer).await;
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let jobs = FakeJobs::default();
    let app = spawn_app_with_rest(metadata.clone(), jobs.clone(), &rest).await;
    let db = ready_database(tenant, &app.key);
    let id = db.database_id;
    metadata.insert_database(db);
    let t = Branching { app, metadata, jobs, db: id };
    t.seeded();
    (t, calls)
}

fn rest_ok(body: Value) -> RestAnswer {
    Arc::new(move |_| (200, vec![], body.clone()))
}

impl Branching {
    async fn console(&self, branch: BranchId, body: Value) -> reqwest::Response {
        self.send(Method::POST, &format!("{}/console/query", self.branch(branch)), Some(body)).await
    }

    /// `GET …/console/schema` with `query` (`""` or `?…`).
    async fn console_schema(&self, branch: BranchId, query: &str) -> reqwest::Response {
        self.send(Method::GET, &format!("{}/console/schema{query}", self.branch(branch)), None).await
    }

    fn role_checks(&self) -> Vec<(BranchId, String)> {
        self.jobs.role_checks.lock().unwrap().clone()
    }

    fn compute_lookups(&self) -> usize {
        self.app.locator.asked.lock().unwrap().len()
    }
}

fn select_notes() -> Value {
    json!({ "v": 1, "op": "select", "from": "notes" })
}

#[tokio::test]
async fn other_roles_check_once_then_forward_to_rest() {
    let rows = json!([{ "id": 1, "body": "hi" }]);
    let (t, calls) = console_app(rest_ok(rows.clone())).await;
    let main = t.main();
    for _ in 0..2 {
        let resp = t.console(main, json!({ "role": "editor", "database": null, "tree": select_notes() })).await;
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["content-type"], "application/json");
        assert_eq!(resp.json::<Value>().await.unwrap(), rows);
    }
    // The worker checked the role once (it grants it to authenticator); the second request
    // went straight to rest.
    assert_eq!(t.role_checks(), vec![(main, "editor".to_string())]);
    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    for call in &calls {
        assert_eq!(call.path, format!("/internal/console/{}/query", main.as_uuid()));
        assert!(call.headers.contains(&format!("authorization: Bearer {REST_TOKEN}")), "{}", call.headers);
        let body: Value = serde_json::from_str(&call.body).unwrap();
        // No database named: main's, by name.
        assert_eq!(body, json!({ "role": "editor", "database": "web1", "tree": select_notes() }));
    }
    // Another role is checked on its own; the owner is never forwarded.
    t.metadata.set_data_api(main, "analytics", chelabase_metadata::DataApiState::On);
    let resp = t.console(main, json!({ "role": "viewer", "database": "analytics", "tree": select_notes() })).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(t.role_checks().len(), 2);
}

#[tokio::test]
async fn rest_answers_pass_through_with_their_status() {
    let answer: RestAnswer = Arc::new(|request: &Value| match request["tree"]["op"].as_str() {
        Some("delete") => (
            409,
            vec![],
            json!({ "error": { "code": "conflict", "message": "duplicate key", "sqlstate": "23505" } }),
        ),
        _ => (
            503,
            vec![("retry-after", "2")],
            json!({ "error": { "code": "unavailable", "message": "try again" } }),
        ),
    });
    let (t, _) = console_app(answer).await;
    let main = t.main();
    let delete = json!({ "v": 1, "op": "delete", "from": "notes", "where": { "col": "id", "eq": 1 } });
    let resp = t.console(main, json!({ "role": "editor", "tree": delete })).await;
    let body = error_of(resp, 409).await;
    assert_eq!(body, json!({ "error": { "code": "conflict", "message": "duplicate key", "sqlstate": "23505" } }));
    let resp = t.console(main, json!({ "role": "editor", "tree": select_notes() })).await;
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.headers()["retry-after"], "2");
}

#[tokio::test]
async fn owner_level_role_is_refused() {
    let (t, calls) = console_app(rest_ok(json!([]))).await;
    let main = t.main();
    let refusal = "the role `boss` can bypass row-level security; pick a plain role";
    *t.jobs.role_check.lock().unwrap() = Some(Ok(CheckDataRoleReply::refused(refusal)));
    for _ in 0..2 {
        let resp = t.console(main, json!({ "role": "boss", "tree": select_notes() })).await;
        let body = error_of(resp, 403).await;
        assert_eq!(code_of(&body), "role_refused");
        assert_eq!(message_of(&body), refusal);
    }
    // A refusal isn't remembered as a pass, and rest never heard of it.
    assert_eq!(t.role_checks().len(), 2);
    assert!(calls.lock().unwrap().is_empty());
    // A branch the worker can't reach yet.
    *t.jobs.role_check.lock().unwrap() = Some(Ok(CheckDataRoleReply::not_ready()));
    let resp = t.console(main, json!({ "role": "boss", "tree": select_notes() })).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "branch_not_ready");
    // rest's own refusal (an owner-level role that slipped through) passes through.
    *t.jobs.role_check.lock().unwrap() = None;
    let (t2, _) = console_app(Arc::new(|_| {
        (403, vec![], json!({ "error": { "code": "role_not_allowed", "message": "no" } }))
    }))
    .await;
    let resp = t2.console(t2.main(), json!({ "role": "boss", "tree": select_notes() })).await;
    assert_eq!(code_of(&error_of(resp, 403).await), "role_not_allowed");
}

#[tokio::test]
async fn a_disabled_database_is_409() {
    let (t, calls) = console_app(rest_ok(json!([]))).await;
    let main = t.main();
    for state in [
        chelabase_metadata::DataApiState::Off,
        chelabase_metadata::DataApiState::Enabling,
        chelabase_metadata::DataApiState::Error,
    ] {
        t.metadata.set_data_api(main, "analytics", state);
        for role in [Value::Null, json!("editor")] {
            let resp = t.console(main, json!({ "role": role, "database": "analytics", "tree": select_notes() })).await;
            let body = error_of(resp, 409).await;
            assert_eq!(code_of(&body), "data_api_disabled", "{state:?} {role}");
        }
        // Named by the tree instead of the request: the same.
        let tree = json!({ "v": 1, "op": "select", "from": "notes", "database": "analytics" });
        let resp = t.console(main, json!({ "role": null, "tree": tree })).await;
        assert_eq!(code_of(&error_of(resp, 409).await), "data_api_disabled");
    }
    assert!(t.role_checks().is_empty());
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(t.compute_lookups(), 0);
}

#[tokio::test]
async fn rest_unknown_database_maps_to_the_rows_state() {
    // rest says the database isn't served: the orchestrator reads the row again.
    let metadata_slot: Arc<Mutex<Option<(FakeMetadata, BranchId)>>> = Arc::default();
    let slot = metadata_slot.clone();
    let answer: RestAnswer = Arc::new(move |request: &Value| {
        // A disable landed between the orchestrator's check and rest's.
        if request["database"] == "analytics"
            && let Some((metadata, branch)) = slot.lock().unwrap().clone()
        {
            metadata.set_data_api(branch, "analytics", chelabase_metadata::DataApiState::Off);
        }
        (404, vec![], json!({ "error": { "code": "unknown_database", "message": "the database doesn't exist or isn't served by this key" } }))
    });
    let (t, _) = console_app(answer).await;
    let main = t.main();
    *metadata_slot.lock().unwrap() = Some((t.metadata.clone(), main));
    t.metadata.set_data_api(main, "analytics", chelabase_metadata::DataApiState::On);
    let resp = t.console(main, json!({ "role": "editor", "database": "analytics", "tree": select_notes() })).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "data_api_disabled");
    // Still on in metadata: a plain 404.
    let resp = t.console(main, json!({ "role": "editor", "tree": select_notes() })).await;
    assert_eq!(code_of(&error_of(resp, 404).await), "not_found");
}

#[tokio::test]
async fn an_unknown_database_branch_or_tenant_is_404() {
    let (t, calls) = console_app(rest_ok(json!([]))).await;
    let main = t.main();
    for role in [Value::Null, json!("editor")] {
        let resp = t.console(main, json!({ "role": role, "database": "nope", "tree": select_notes() })).await;
        assert_eq!(code_of(&error_of(resp, 404).await), "not_found", "{role}");
    }
    // The system database is never served: it answers as missing.
    let resp = t.console(main, json!({ "role": null, "database": "postgres", "tree": select_notes() })).await;
    assert_eq!(resp.status().as_u16() / 100, 4);
    let resp = t.console(BranchId::new(), json!({ "role": null, "tree": select_notes() })).await;
    assert_eq!(code_of(&error_of(resp, 404).await), "not_found");
    // Another tenant asking for this database's branch.
    t.metadata.bind_org("org-2", TenantId::new());
    let resp = reqwest::Client::new()
        .post(format!("{}{}/console/query", t.app.base, t.branch(main)))
        .header(IDENTITY_HEADER, t.app.token_for_org("org-2"))
        .json(&json!({ "role": "editor", "tree": select_notes() }))
        .send()
        .await
        .unwrap();
    assert_eq!(code_of(&error_of(resp, 404).await), "not_found");
    assert!(calls.lock().unwrap().is_empty());
    assert!(t.role_checks().is_empty());
}

#[tokio::test]
async fn bad_console_requests_are_refused_before_anything_runs() {
    let (t, calls) = console_app(rest_ok(json!([]))).await;
    let main = t.main();
    // The request and its tree name different databases.
    let tree = json!({ "v": 1, "op": "select", "from": "notes", "database": "analytics" });
    for role in [Value::Null, json!("editor")] {
        let resp = t.console(main, json!({ "role": role, "database": "web1", "tree": tree })).await;
        assert_eq!(code_of(&error_of(resp, 400).await), "invalid_query", "{role}");
    }
    // A tree the data API's grammar refuses.
    for role in [Value::Null, json!("editor")] {
        let resp = t.console(main, json!({ "role": role, "tree": { "v": 2, "op": "select", "from": "notes" } })).await;
        assert_eq!(code_of(&error_of(resp, 400).await), "invalid_query", "{role}");
    }
    // Not the console's shape.
    let resp = t.console(main, json!({ "role": 7, "tree": select_notes() })).await;
    assert_eq!(code_of(&error_of(resp, 422).await), "invalid_request");
    let resp = t.console(main, json!({ "role": "" , "tree": select_notes() })).await;
    assert_eq!(code_of(&error_of(resp, 422).await), "invalid_request");
    assert!(calls.lock().unwrap().is_empty());
    assert!(t.role_checks().is_empty());
    assert_eq!(t.compute_lookups(), 0);
}

/// Naming the owner's own role would skip the owner path for a forward rest can't serve:
/// the console says how to ask for the owner instead.
#[tokio::test]
async fn the_owners_name_as_a_role_is_422() {
    let (t, calls) = console_app(rest_ok(json!([]))).await;
    let main = t.main();
    let resp = t.console(main, json!({ "role": "postgres", "tree": select_notes() })).await;
    let body = error_of(resp, 422).await;
    assert_eq!(code_of(&body), "invalid_request");
    assert_eq!(message_of(&body), "use role: null to run as the owner");
    let resp = t.console_schema(main, "?role=postgres").await;
    let body = error_of(resp, 422).await;
    assert_eq!(message_of(&body), "omit role to view the schema as the owner");
    assert!(calls.lock().unwrap().is_empty());
    assert!(t.role_checks().is_empty());
}

#[tokio::test]
async fn console_schema_as_a_role_checks_then_forwards_role_and_database() {
    let schema = json!({ "version": "v1", "default_schema": "reporting", "schemas": {} });
    let (t, calls) = console_app(rest_ok(schema.clone())).await;
    let main = t.main();
    t.metadata.set_data_api(main, "analytics", chelabase_metadata::DataApiState::On);
    let resp = t.console_schema(main, "?role=editor&database=analytics").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "application/json");
    assert_eq!(resp.json::<Value>().await.unwrap(), schema);
    // No database: main's, by name. The role was checked once.
    assert_eq!(t.console_schema(main, "?role=editor").await.status(), 200);
    assert_eq!(t.role_checks(), vec![(main, "editor".to_string())]);
    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    for (call, database) in calls.iter().zip(["analytics", "web1"]) {
        assert_eq!(call.path, format!("/internal/schema/{}", main.as_uuid()));
        assert!(call.headers.contains(&format!("authorization: Bearer {REST_TOKEN}")), "{}", call.headers);
        let mut pairs: Vec<&str> = call.query.split('&').collect();
        pairs.sort();
        assert_eq!(pairs, [format!("database={database}").as_str(), "role=editor"]);
    }
}

#[tokio::test]
async fn console_schema_refusals() {
    let answer: RestAnswer = Arc::new(|_| {
        (404, vec![], json!({ "error": { "code": "unknown_database", "message": "the database doesn't exist or isn't served by this key" } }))
    });
    let (t, calls) = console_app(answer).await;
    let main = t.main();
    // Unknown, disabled (whoever asks), the system database.
    for query in ["?database=nope", "?database=nope&role=editor", "?database=postgres"] {
        assert_eq!(code_of(&error_of(t.console_schema(main, query).await, 404).await), "not_found", "{query}");
    }
    for query in ["?database=analytics", "?database=analytics&role=editor"] {
        let body = error_of(t.console_schema(main, query).await, 409).await;
        assert_eq!(code_of(&body), "data_api_disabled", "{query}");
    }
    // An empty role.
    assert_eq!(code_of(&error_of(t.console_schema(main, "?role=").await, 422).await), "invalid_request");
    // The owner without a sealed password; the worker refusing a role.
    t.metadata.branches.lock().unwrap().get_mut(&main).unwrap().owner_password_enc = None;
    assert_eq!(code_of(&error_of(t.console_schema(main, "").await, 409).await), "owner_password_unknown");
    *t.jobs.role_check.lock().unwrap() = Some(Ok(CheckDataRoleReply::refused("no")));
    assert_eq!(code_of(&error_of(t.console_schema(main, "?role=boss").await, 403).await), "role_refused");
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(t.compute_lookups(), 0);
    // rest no longer serving it after the check: the row decides (still on: 404).
    *t.jobs.role_check.lock().unwrap() = None;
    assert_eq!(code_of(&error_of(t.console_schema(main, "?role=editor").await, 404).await), "not_found");
    // Backups and branches that aren't ready.
    let backup = t.backup(main);
    assert_eq!(code_of(&error_of(t.console_schema(backup, "?role=editor").await, 409).await), "backup_branch");
    let pending = t.add("dev", main, BranchKind::Normal, BranchStatus::Pending);
    assert_eq!(code_of(&error_of(t.console_schema(pending, "").await, 409).await), "branch_not_ready");
}

#[tokio::test]
async fn owner_password_unknown_is_409_before_any_connection() {
    let (t, calls) = console_app(rest_ok(json!([]))).await;
    let main = t.main();
    for sealed in [None, Some(String::new())] {
        t.metadata.branches.lock().unwrap().get_mut(&main).unwrap().owner_password_enc = sealed;
        let resp = t.console(main, json!({ "role": null, "tree": select_notes() })).await;
        let body = error_of(resp, 409).await;
        assert_eq!(code_of(&body), "owner_password_unknown");
        assert_eq!(message_of(&body), "reset the owner's password to view as owner");
    }
    assert_eq!(t.compute_lookups(), 0);
    assert!(calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn owner_console_without_a_running_compute_is_503() {
    let (t, calls) = console_app(rest_ok(json!([]))).await;
    let main = t.main();
    let resp = t.console(main, json!({ "role": null, "tree": select_notes() })).await;
    assert_eq!(resp.status(), 503);
    assert_eq!(resp.headers()["retry-after"], "2");
    let body: Value = resp.json().await.unwrap();
    assert_eq!(code_of(&body), "unavailable");
    assert_eq!(*t.app.locator.asked.lock().unwrap(), vec!["ep-0123456789ab".to_string()]);
    assert!(calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn backups_and_unready_branches_are_refused() {
    let (t, calls) = console_app(rest_ok(json!([]))).await;
    let backup = t.backup(t.main());
    let resp = t.console(backup, json!({ "role": "editor", "tree": select_notes() })).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "backup_branch");
    let pending = t.add("dev", t.main(), BranchKind::Normal, BranchStatus::Pending);
    let resp = t.console(pending, json!({ "role": null, "tree": select_notes() })).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "branch_not_ready");
    assert!(calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_owner_password_never_goes_to_rest() {
    let (t, calls) = console_app(rest_ok(json!([]))).await;
    let main = t.main();
    let sealed = t.metadata.branch(main).owner_password_enc.unwrap();
    t.metadata.set_data_api(main, "analytics", chelabase_metadata::DataApiState::On);
    // The owner path, with and without a compute: never forwarded.
    for database in [Value::Null, json!("analytics")] {
        let resp = t.console(main, json!({ "role": null, "database": database, "tree": select_notes() })).await;
        assert_eq!(resp.status(), 503);
    }
    assert!(calls.lock().unwrap().is_empty(), "the owner path went to rest");
    // Every other role, read and write: rest sees the role, the database and the tree only.
    let insert = json!({ "v": 1, "op": "insert", "into": "notes", "values": [{ "body": "x" }] });
    for (role, tree) in [("editor", select_notes()), ("editor", insert), ("viewer", select_notes())] {
        let resp = t.console(main, json!({ "role": role, "tree": tree })).await;
        assert_eq!(resp.status(), 200);
    }
    let calls = calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 3);
    for call in calls {
        for secret in ["s3cret", sealed.as_str()] {
            assert!(!call.body.contains(secret), "body: {}", call.body);
            assert!(!call.headers.contains(secret), "headers: {}", call.headers);
            assert!(!call.path.contains(secret), "path: {}", call.path);
        }
        let body: Value = serde_json::from_str(&call.body).unwrap();
        let keys: Vec<&String> = body.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["database", "role", "tree"]);
    }
}

/// Collects what the orchestrator logs while a test runs (the test's runtime is
/// single-threaded, so the server's tasks log through this thread's subscriber).
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }

    fn capture(&self) -> tracing::subscriber::DefaultGuard {
        let logs = self.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || logs.clone())
            .finish();
        tracing::subscriber::set_default(subscriber)
    }
}

#[tokio::test]
async fn console_writes_are_audited_without_values() {
    let logs = Logs::default();
    let _guard = logs.capture();
    let (t, _) = console_app(rest_ok(json!([]))).await;
    let main = t.main();
    const SECRET: &str = "card-4111-1111-1111-1111";
    let insert = json!({ "v": 1, "op": "insert", "into": "notes", "values": [{ "body": SECRET }] });
    let update = json!({ "v": 1, "op": "update", "table": "notes", "set": { "body": SECRET }, "where": { "col": "id", "eq": 1 } });
    let batch = json!({ "v": 1, "op": "batch", "ops": [select_notes(), { "op": "delete", "from": "notes", "where": { "col": "body", "eq": SECRET } }] });
    let call = json!({ "v": 1, "op": "call", "fn": "touch", "args": { "note": SECRET } });
    for tree in [select_notes(), insert, update, batch, call] {
        let resp = t.console(main, json!({ "role": "editor", "tree": tree })).await;
        assert_eq!(resp.status(), 200);
    }
    // A read isn't a write.
    let resp = t.console(main, json!({ "role": "editor", "tree": { "v": 1, "op": "batch", "ops": [select_notes()] } })).await;
    assert_eq!(resp.status(), 200);
    let text = logs.text();
    let audit: Vec<&str> = text.lines().filter(|l| l.contains("console write")).collect();
    assert_eq!(audit.len(), 4, "{text}");
    for (line, op) in audit.iter().zip(["insert", "update", "batch", "call"]) {
        assert!(line.contains("user=user-1"), "{line}");
        assert!(line.contains(&format!("branch={}", main.as_uuid())), "{line}");
        assert!(line.contains("database=web1"), "{line}");
        assert!(line.contains("role=editor"), "{line}");
        assert!(line.contains(&format!("op={op}")), "{line}");
    }
    assert!(!text.contains(SECRET), "a value reached the log:\n{text}");
}

// ---- the owner path against a real Postgres: the dev metadata Postgres stands in for a
// compute (a scratch database and a scratch owner role, dropped at the end).

struct ScratchCompute {
    admin: sqlx::PgPool,
    database: String,
    owner: String,
    addr: std::net::SocketAddr,
}

impl ScratchCompute {
    async fn new() -> Self {
        use sqlx::Executor;
        let url = std::env::var("CHELABASE_METADATA_DATABASE_URL")
            .unwrap_or_else(|_| "postgres://chelabase:chelabase@127.0.0.1:5433/chelabase_metadata".into());
        let admin = sqlx::PgPool::connect(&url).await.expect("the metadata Postgres (just deps-up)");
        let suffix = format!("{:08x}", rand_u32());
        let database = format!("t6_console_{suffix}");
        let owner = format!("t6_owner_{suffix}");
        admin.execute(sqlx::AssertSqlSafe(format!("CREATE ROLE {owner} LOGIN"))).await.unwrap();
        admin
            .execute(sqlx::AssertSqlSafe(format!("CREATE DATABASE {database} OWNER {owner}")))
            .await
            .unwrap();
        let opts: sqlx::postgres::PgConnectOptions = url.parse().unwrap();
        let addr = format!("{}:{}", opts.get_host(), opts.get_port()).parse().unwrap();
        let scratch = sqlx::PgPool::connect_with(opts.database(&database)).await.unwrap();
        scratch
            .execute(sqlx::AssertSqlSafe(format!(
                "CREATE SCHEMA chelabase; \
                 CREATE TABLE chelabase.schema_version (version bigint NOT NULL); \
                 INSERT INTO chelabase.schema_version VALUES (1); \
                 GRANT USAGE ON SCHEMA chelabase TO {owner}; \
                 GRANT SELECT ON chelabase.schema_version TO {owner}; \
                 CREATE TABLE public.notes (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text NOT NULL); \
                 ALTER TABLE public.notes OWNER TO {owner};"
            )))
            .await
            .unwrap();
        scratch.close().await;
        ScratchCompute { admin, database, owner, addr }
    }

    async fn set_password(&self, password: &str) {
        use sqlx::Executor;
        self.admin
            .execute(sqlx::AssertSqlSafe(format!("ALTER ROLE {} PASSWORD '{password}'", self.owner)))
            .await
            .unwrap();
    }

    /// Runs `sql` in the scratch database, as the cluster's admin.
    async fn exec_in(&self, sql: &str) {
        use sqlx::Executor;
        let opts: sqlx::postgres::PgConnectOptions = (*self.admin.connect_options()).clone();
        let pool = sqlx::PgPool::connect_with(opts.database(&self.database)).await.unwrap();
        pool.execute(sqlx::AssertSqlSafe(sql.to_owned())).await.unwrap();
        pool.close().await;
    }

    /// Ends every session of the owner: a pool still using the old password must reconnect.
    async fn kick_owner(&self) {
        sqlx::query("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE usename = $1")
            .bind(&self.owner)
            .execute(&self.admin)
            .await
            .unwrap();
    }

    async fn drop(self) {
        use sqlx::Executor;
        self.admin
            .execute(sqlx::AssertSqlSafe(format!("DROP DATABASE {} WITH (FORCE)", self.database)))
            .await
            .unwrap();
        self.admin
            .execute(sqlx::AssertSqlSafe(format!("DROP ROLE {}", self.owner)))
            .await
            .unwrap();
    }
}

fn rand_u32() -> u32 {
    (SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos() ^ std::process::id().rotate_left(16))
        .wrapping_mul(2_654_435_761)
}

/// A console app whose main database is the scratch database, owned by the scratch owner,
/// with the compute at the metadata Postgres's address.
async fn owner_console_app(scratch: &ScratchCompute) -> Branching {
    let (t, _) = console_app(rest_ok(json!([]))).await;
    let main = t.main();
    {
        let mut databases = t.metadata.databases.lock().unwrap();
        let record = databases.get_mut(&t.db).unwrap();
        record.owner_role = Some(scratch.owner.clone());
        record.database_name = Some(scratch.database.clone());
    }
    let mut all = t.metadata.branch_dbs.lock().unwrap();
    let rows = all.get_mut(&main).unwrap();
    let web1 = rows.iter_mut().find(|d| d.name == "web1").unwrap();
    web1.name = scratch.database.clone();
    web1.owner = scratch.owner.clone();
    drop(all);
    *t.app.locator.addr.lock().unwrap() = Some(scratch.addr);
    t
}

impl Branching {
    fn seal_owner_password(&self, password: Option<&str>) {
        let main = self.main();
        let sealed = password.map(|p| seal_credential(&self.app.key, &main.to_string(), Purpose::Owner, p));
        self.metadata.branches.lock().unwrap().get_mut(&main).unwrap().owner_password_enc = sealed;
    }
}

#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn owner_console_runs_through_the_owner_pool() {
    let logs = Logs::default();
    let _guard = logs.capture();
    let scratch = ScratchCompute::new().await;
    scratch.set_password("first-pass").await;
    let t = owner_console_app(&scratch).await;
    t.seal_owner_password(Some("first-pass"));
    let main = t.main();

    let insert = json!({ "v": 1, "op": "insert", "into": "notes", "values": [{ "body": "private words" }] });
    let resp = t.console(main, json!({ "role": null, "tree": insert })).await;
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(status, 200, "{text}");
    let resp = t.console(main, json!({ "role": null, "tree": select_notes() })).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "application/json");
    let rows: Value = resp.json().await.unwrap();
    assert_eq!(rows, json!([{ "id": 1, "body": "private words" }]));
    // A Postgres error comes back in the data API's shape.
    let bad = json!({ "v": 1, "op": "insert", "into": "notes", "values": [{ "id": 5, "body": "x" }] });
    let resp = t.console(main, json!({ "role": null, "tree": bad })).await;
    let body: Value = resp.json().await.unwrap();
    assert!(body["error"]["code"].is_string(), "{body}");
    // The owner pool holds at most 2 connections, under its own application name.
    let (n,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM pg_stat_activity WHERE usename = $1 AND application_name = 'chelabase-console'",
    )
    .bind(&scratch.owner)
    .fetch_one(&scratch.admin)
    .await
    .unwrap();
    assert!((1..=2).contains(&n), "{n} owner connections");
    // Audited as the owner, without the value.
    let text = logs.text();
    let audit: Vec<&str> = text.lines().filter(|l| l.contains("console write")).collect();
    assert_eq!(audit.len(), 2, "{text}");
    assert!(audit[0].contains(&format!("role={}", scratch.owner)), "{}", audit[0]);
    assert!(audit[0].contains("op=insert"), "{}", audit[0]);
    assert!(!text.contains("private words"), "{text}");
    assert!(!text.contains("first-pass"), "{text}");
    // rest was never asked.
    assert!(t.role_checks().is_empty());
    scratch.drop().await;
}

#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn owner_password_unknown_then_works_after_reset() {
    let scratch = ScratchCompute::new().await;
    let t = owner_console_app(&scratch).await;
    let main = t.main();
    // Set in SQL: Chelabase doesn't know it.
    t.seal_owner_password(None);
    let resp = t.console(main, json!({ "role": null, "tree": select_notes() })).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "owner_password_unknown");
    // A reset: the worker sets a new password and stores it sealed. Nothing restarts.
    scratch.set_password("after-reset").await;
    t.seal_owner_password(Some("after-reset"));
    let resp = t.console(main, json!({ "role": null, "tree": select_notes() })).await;
    let status = resp.status();
    assert_eq!(status, 200, "{}", resp.text().await.unwrap());
    // Another reset while the first pool still holds connections: the new sealed password
    // makes a new pool (the old one couldn't reconnect with its password).
    scratch.set_password("second-reset").await;
    t.seal_owner_password(Some("second-reset"));
    scratch.kick_owner().await;
    let resp = t.console(main, json!({ "role": null, "tree": select_notes() })).await;
    let status = resp.status();
    assert_eq!(status, 200, "{}", resp.text().await.unwrap());
    scratch.drop().await;
}

/// The console's schema as the owner: everything the owner reaches, primary keys included,
/// a schema no other role may use among it.
#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn owner_console_schema_shows_what_only_the_owner_reaches() {
    let scratch = ScratchCompute::new().await;
    scratch.set_password("schema-pass").await;
    let owner = scratch.owner.clone();
    scratch
        .exec_in(&format!(
            "CREATE SCHEMA private AUTHORIZATION {owner}; \
             CREATE TABLE private.secret (id int PRIMARY KEY, note text); \
             ALTER TABLE private.secret OWNER TO {owner};"
        ))
        .await;
    let t = owner_console_app(&scratch).await;
    t.seal_owner_password(Some("schema-pass"));
    let main = t.main();
    let resp = t.console_schema(main, "").await;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["default_schema"], "public");
    let secret = &body["schemas"]["private"]["tables"]["secret"];
    assert_eq!(secret["primary_key"], json!(["id"]), "{body}");
    assert!(secret["privileges"].get(&owner).is_some(), "{body}");
    assert_eq!(body["schemas"]["public"]["tables"]["notes"]["primary_key"], json!(["id"]));
    // No other role may use `private` (PUBLIC has no USAGE on it): the role path wouldn't show it.
    let opts: sqlx::postgres::PgConnectOptions = (*scratch.admin.connect_options()).clone();
    let pool = sqlx::PgPool::connect_with(opts.database(&scratch.database)).await.unwrap();
    let (public_usage,): (bool,) = sqlx::query_as("SELECT has_schema_privilege('public', 'private', 'USAGE')")
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert!(!public_usage);
    // rest was never asked; no role was checked.
    assert!(t.role_checks().is_empty());
    scratch.drop().await;
}

// ---- Projects ---------------------------------------------------------------------

async fn send(app: &TestApp, method: Method, path: &str, org: &str, body: Option<Value>) -> reqwest::Response {
    let mut req = reqwest::Client::new()
        .request(method, format!("{}{path}", app.base))
        .header(IDENTITY_HEADER, app.token_for_org(org));
    if let Some(body) = body {
        req = req.json(&body);
    }
    req.send().await.unwrap()
}

/// org-1's tenant with the app running.
async fn projects_app(jobs: FakeJobs) -> (TestApp, FakeMetadata, TenantId) {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let app = spawn_app(metadata.clone(), jobs).await;
    (app, metadata, tenant)
}

fn project_database(tenant: TenantId, project: ProjectId, status: DatabaseStatus) -> Db {
    let mut db = sample_database(tenant);
    db.project_id = project;
    db.status = status;
    db.main.status = BranchStatus::from(status);
    db
}

#[tokio::test]
async fn list_projects_only_mine() {
    let (app, metadata, tenant) = projects_app(FakeJobs::default()).await;
    let mine = metadata.create_project(tenant, "Mine").await.unwrap();
    metadata.create_project(TenantId::new(), "Theirs").await.unwrap();
    let body: Value = send(&app, Method::GET, "/v1/projects", "org-1", None).await.json().await.unwrap();
    let list = body.as_array().unwrap();
    assert_eq!(list.len(), 1, "{body}");
    assert_eq!(list[0]["id"], json!(mine.id));
    assert_eq!(list[0]["name"], "Mine");
    assert_eq!(list[0]["status"], "active");
    assert!(list[0]["app_users"].is_null());
}

#[tokio::test]
async fn create_project_201_with_slug() {
    let (app, _, _) = projects_app(FakeJobs::default()).await;
    let resp = send(&app, Method::POST, "/v1/projects", "org-1", Some(json!({ "name": "  My App " }))).await;
    assert_eq!(resp.status(), 201);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["name"], "My App");
    assert_eq!(body["slug"], "my-app");
    assert_eq!(body["status"], "active");
    assert!(body["error"].is_null());
    assert!(body["id"].is_string() && body["created_at"].is_string() && body["updated_at"].is_string());
    let again: Value = send(&app, Method::POST, "/v1/projects", "org-1", Some(json!({ "name": "My App" })))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(again["slug"], "my-app-2");
}

#[tokio::test]
async fn create_project_name_validation_422() {
    let (app, _, _) = projects_app(FakeJobs::default()).await;
    for name in ["", "   ", &"x".repeat(65)] {
        let resp = send(&app, Method::POST, "/v1/projects", "org-1", Some(json!({ "name": name }))).await;
        let body = error_of(resp, 422).await;
        assert_eq!(code_of(&body), "invalid_request");
    }
    let resp = send(&app, Method::POST, "/v1/projects", "org-1", Some(json!({}))).await;
    assert_eq!(resp.status(), 422);
}

#[tokio::test]
async fn get_other_tenant_project_404() {
    let (app, metadata, tenant) = projects_app(FakeJobs::default()).await;
    let mine = metadata.create_project(tenant, "Mine").await.unwrap();
    let theirs = metadata.create_project(TenantId::new(), "Theirs").await.unwrap();
    let resp = send(&app, Method::GET, &format!("/v1/projects/{}", mine.id), "org-1", None).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["slug"], "mine");
    for id in [theirs.id, ProjectId::new()] {
        let resp = send(&app, Method::GET, &format!("/v1/projects/{id}"), "org-1", None).await;
        assert_eq!(code_of(&error_of(resp, 404).await), "not_found");
    }
}

#[tokio::test]
async fn rename_project_changes_name_not_slug() {
    let (app, metadata, tenant) = projects_app(FakeJobs::default()).await;
    let p = metadata.create_project(tenant, "Old name").await.unwrap();
    let path = format!("/v1/projects/{}", p.id);
    let resp = send(&app, Method::PATCH, &path, "org-1", Some(json!({ "name": " New name " }))).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["name"], "New name");
    assert_eq!(body["slug"], "old-name");
    let resp = send(&app, Method::PATCH, &path, "org-1", Some(json!({ "name": "" }))).await;
    assert_eq!(resp.status(), 422);
    let resp = send(&app, Method::PATCH, &format!("/v1/projects/{}", ProjectId::new()), "org-1", Some(json!({ "name": "x" }))).await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn rename_deleting_project_409() {
    let (app, metadata, tenant) = projects_app(FakeJobs::default()).await;
    let p = metadata.create_project(tenant, "Going").await.unwrap();
    metadata.begin_project_delete(tenant, p.id).await.unwrap();
    let resp = send(&app, Method::PATCH, &format!("/v1/projects/{}", p.id), "org-1", Some(json!({ "name": "Back" }))).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "project_deleting");
}

#[tokio::test]
async fn delete_project_needs_the_slug() {
    let jobs = FakeJobs::default();
    let (app, metadata, tenant) = projects_app(jobs.clone()).await;
    let p = metadata.create_project(tenant, "Shop").await.unwrap();
    let path = format!("/v1/projects/{}", p.id);
    for confirm in ["Shop", "shop ", "other", ""] {
        let resp = send(&app, Method::DELETE, &path, "org-1", Some(json!({ "confirm": confirm }))).await;
        let body = error_of(resp, 422).await;
        assert_eq!(message_of(&body), "type the project's slug to confirm");
    }
    assert_eq!(send(&app, Method::DELETE, &path, "org-1", Some(json!({}))).await.status(), 422);
    assert!(jobs.project_deletes.lock().unwrap().is_empty());
    assert_eq!(metadata.get_project(tenant, p.id).await.unwrap().unwrap().status, ProjectStatus::Active);
    // An unknown project is 404 before the slug is looked at.
    let resp = send(&app, Method::DELETE, &format!("/v1/projects/{}", ProjectId::new()), "org-1", Some(json!({ "confirm": "x" }))).await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn delete_project_publishes_database_and_project_jobs() {
    let jobs = FakeJobs::default();
    let (app, metadata, tenant) = projects_app(jobs.clone()).await;
    let p = metadata.create_project(tenant, "Shop").await.unwrap();
    let ready = project_database(tenant, p.id, DatabaseStatus::Ready);
    let failed = project_database(tenant, p.id, DatabaseStatus::Failed);
    let other = project_database(tenant, ProjectId::new(), DatabaseStatus::Ready);
    let expected: HashSet<DatabaseId> = [ready.database_id, failed.database_id].into();
    for db in [ready, failed, other] {
        metadata.insert_database(db);
    }
    let resp = send(&app, Method::DELETE, &format!("/v1/projects/{}", p.id), "org-1", Some(json!({ "confirm": "shop" }))).await;
    assert_eq!(resp.status(), 202);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["status"], "deleting");
    assert_eq!(body["id"], json!(p.id));
    let deleted: HashSet<DatabaseId> = jobs.deleted.lock().unwrap().iter().copied().collect();
    assert_eq!(deleted, expected);
    assert_eq!(*jobs.project_deletes.lock().unwrap(), vec![p.id]);
    // The project stays visible, as deleting.
    let body: Value = send(&app, Method::GET, &format!("/v1/projects/{}", p.id), "org-1", None).await.json().await.unwrap();
    assert_eq!(body["status"], "deleting");
}

#[tokio::test]
async fn delete_project_refusals_map_to_codes() {
    let jobs = FakeJobs::default();
    let (app, metadata, tenant) = projects_app(jobs.clone()).await;
    let delete = |p: ProjectRecord| {
        let app = &app;
        async move {
            let resp = send(app, Method::DELETE, &format!("/v1/projects/{}", p.id), "org-1", Some(json!({ "confirm": p.slug }))).await;
            let status = resp.status();
            let body: Value = resp.json().await.unwrap();
            (status.as_u16(), body)
        }
    };

    let pending = metadata.create_project(tenant, "Pending").await.unwrap();
    metadata.insert_database(project_database(tenant, pending.id, DatabaseStatus::Provisioning));
    let (status, body) = delete(pending).await;
    assert_eq!((status, code_of(&body)), (409, "database_not_ready"), "{body}");

    let protected = metadata.create_project(tenant, "Protected").await.unwrap();
    let mut db = project_database(tenant, protected.id, DatabaseStatus::Ready);
    db.main.protected = true;
    metadata.insert_database(db);
    let (status, body) = delete(protected).await;
    assert_eq!((status, code_of(&body)), (409, "branch_protected"), "{body}");

    let busy = metadata.create_project(tenant, "Busy").await.unwrap();
    let db = project_database(tenant, busy.id, DatabaseStatus::Ready);
    metadata
        .begin_enable_app_users(&NewProjectAuth {
            project_id: busy.id,
            database_id: db.database_id,
            pg_database: "web1",
            schema_name: "public",
            table_name: "users",
            email_verification: false,
            on_delete: OnDelete::Delete,
        })
        .await
        .unwrap();
    metadata.insert_database(db);
    let (status, body) = delete(busy).await;
    assert_eq!((status, code_of(&body)), (409, "conflict"), "{body}");

    let going = metadata.create_project(tenant, "Going").await.unwrap();
    metadata.begin_project_delete(tenant, going.id).await.unwrap();
    let (status, body) = delete(going).await;
    assert_eq!((status, code_of(&body)), (409, "project_deleting"), "{body}");

    let theirs = metadata.create_project(TenantId::new(), "Theirs").await.unwrap();
    let (status, _) = delete(theirs).await;
    assert_eq!(status, 404);

    assert!(jobs.deleted.lock().unwrap().is_empty());
    assert!(jobs.project_deletes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn create_database_in_deleting_project_409() {
    let (app, metadata, tenant) = projects_app(FakeJobs::default()).await;
    let p = metadata.create_project(tenant, "Going").await.unwrap();
    metadata.begin_project_delete(tenant, p.id).await.unwrap();
    let body = json!({ "tenant_id": tenant, "project_id": p.id, "name": "demo" });
    let resp = post_create(&app, Some(&app.token_for_org("org-1")), body).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "project_deleting");
}

#[tokio::test]
async fn me_lists_project_name_and_status() {
    let metadata = FakeMetadata::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let mut overview = overview_with_database(tenant);
    overview.projects.push(ProjectOverview {
        id: ProjectId::new(),
        name: "Going".into(),
        slug: "going".into(),
        status: ProjectStatus::Deleting,
        databases: vec![],
    });
    metadata.set_overview(overview);
    let app = spawn_app(metadata, FakeJobs::default()).await;
    let body: Value = get_me(&app, Some(&app.token_for_org("org-1"))).await.json().await.unwrap();
    let projects = &body["tenant"]["projects"];
    assert_eq!((projects[0]["name"].as_str(), projects[0]["status"].as_str()), (Some("Default"), Some("active")));
    assert_eq!((projects[1]["name"].as_str(), projects[1]["status"].as_str()), (Some("Going"), Some("deleting")));
}

// ---- App users: settings and user admin ---------------------------------------------

use chelabase_query::app_users::{NewUserRow, PendingInsert, SyncTarget, UserRowError, UsersDb};
use chelabase_zitadel::client::Zitadel;
use chelabase_zitadel::mock::{ADMIN_TOKEN, MockZitadel, User as MockUser};

/// What `FakeUsersDb` does when asked to insert or to commit.
#[derive(Clone, Copy, PartialEq, Debug)]
enum UdbFault {
    Constraint,
    EmailTaken,
    CommitFails,
}

/// A users-table writer that journals what it was asked, and (with a probe) how many users
/// Zitadel had at each step.
#[derive(Clone, Default)]
struct FakeUsersDb {
    journal: Arc<Mutex<Vec<String>>>,
    targets: Arc<Mutex<Vec<SyncTarget>>>,
    rows: Arc<Mutex<Vec<NewUserRow>>>,
    fault: Arc<Mutex<Option<UdbFault>>>,
    probe: Arc<Mutex<Option<Arc<Mutex<chelabase_zitadel::mock::Inner>>>>>,
}

impl FakeUsersDb {
    fn zitadel_users(&self) -> usize {
        self.probe.lock().unwrap().as_ref().map_or(0, |i| i.lock().unwrap().users.len())
    }
    fn journal(&self) -> Vec<String> {
        self.journal.lock().unwrap().clone()
    }
}

struct FakePending {
    db: FakeUsersDb,
}

#[async_trait::async_trait]
impl PendingInsert for FakePending {
    async fn commit(self: Box<Self>) -> Result<(), UserRowError> {
        let users = self.db.zitadel_users();
        self.db.journal.lock().unwrap().push(format!("commit:users={users}"));
        if *self.db.fault.lock().unwrap() == Some(UdbFault::CommitFails) {
            return Err(UserRowError::Unavailable("commit failed".into()));
        }
        Ok(())
    }
    async fn rollback(self: Box<Self>) {
        self.db.journal.lock().unwrap().push("rollback".into());
    }
}

#[async_trait::async_trait]
impl UsersDb for FakeUsersDb {
    async fn begin_insert(&self, target: &SyncTarget, row: &NewUserRow) -> Result<Box<dyn PendingInsert>, UserRowError> {
        let users = self.zitadel_users();
        self.journal.lock().unwrap().push(format!("begin:{}:users={users}", row.email));
        self.targets.lock().unwrap().push(target.clone());
        self.rows.lock().unwrap().push(row.clone());
        match *self.fault.lock().unwrap() {
            Some(UdbFault::Constraint) => Err(UserRowError::Constraint {
                message: "null value in column \"nick\"".into(),
                constraint: Some("users_nick_check".into()),
            }),
            Some(UdbFault::EmailTaken) => Err(UserRowError::EmailTaken),
            _ => Ok(Box::new(FakePending { db: self.clone() })),
        }
    }
}

const AU_ORG: &str = "o1";
const AU_PASSWORD: &str = "correct-horse-9";

/// A project with a ready database (main branch, Postgres database `web1` served by the data
/// API), org-1's tenant, and optionally Zitadel's mock behind the orchestrator.
struct Au {
    app: TestApp,
    metadata: FakeMetadata,
    jobs: FakeJobs,
    tenant: TenantId,
    project: ProjectRecord,
    db: DatabaseId,
    main: BranchId,
    mock: Option<MockZitadel>,
    udb: FakeUsersDb,
}

async fn au_env(with_zitadel: bool) -> Au {
    let metadata = FakeMetadata::default();
    let jobs = FakeJobs::default();
    let tenant = TenantId::new();
    metadata.bind_org("org-1", tenant);
    let project = metadata.create_project(tenant, "Web").await.unwrap();
    let db = project_database(tenant, project.id, DatabaseStatus::Ready);
    let (db_id, main) = (db.database_id, db.main.id);
    metadata.insert_database(db);
    metadata.add_branch_db(main, "web1", "owner", DatabaseOrigin::Main);
    metadata.set_data_api(main, "web1", chelabase_metadata::DataApiState::On);
    let udb = FakeUsersDb::default();
    let (mock, zitadel) = if with_zitadel {
        let mock = MockZitadel::start().await;
        *udb.probe.lock().unwrap() = Some(mock.inner.clone());
        let z = Zitadel::new(&mock.base, ADMIN_TOKEN);
        (Some(mock), Some(z))
    } else {
        (None, None)
    };
    let app = spawn_app_parts(
        metadata.clone(),
        jobs.clone(),
        "http://127.0.0.1:9",
        Mode::SelfHosted,
        10,
        zitadel,
        Arc::new(udb.clone()),
    )
    .await;
    Au { app, metadata, jobs, tenant, project, db: db_id, main, mock, udb }
}

impl Au {
    fn path(&self, rest: &str) -> String {
        format!("/v1/projects/{}{rest}", self.project.id)
    }

    async fn call(&self, method: Method, rest: &str, body: Option<Value>) -> (u16, Value) {
        let resp = send(&self.app, method, &self.path(rest), "org-1", body).await;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap();
        (status, if text.is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap() })
    }

    fn set_auth(&self, status: AppUsersStatus) {
        let now = chrono::Utc::now();
        self.metadata.project_auths.lock().unwrap().insert(
            self.project.id,
            ProjectAuth {
                project_id: self.project.id,
                status,
                error: None,
                zitadel_org_id: Some(AU_ORG.into()),
                database_id: self.db,
                pg_database: "web1".into(),
                schema_name: "public".into(),
                table_name: "users".into(),
                email_verification: false,
                on_delete: OnDelete::Delete,
                created_at: now,
                updated_at: now,
            },
        );
    }

    fn auth(&self) -> ProjectAuth {
        self.metadata.project_auths.lock().unwrap().get(&self.project.id).cloned().expect("project_auth row")
    }

    /// The sync role's sealed password on the auth database's main branch.
    fn seal_sync_role(&self) {
        let sealed = chelabase_secrets::seal_role_password(&self.app.key, &self.main.to_string(), "chelabase_auth_sync", "sync-pw");
        self.metadata.branch_syncs.lock().unwrap().insert(
            self.main,
            BranchAuthSync { branch_id: self.main, pg_database: "web1".into(), password_sealed: sealed },
        );
        *self.app.locator.addr.lock().unwrap() = Some("127.0.0.1:55432".parse().unwrap());
    }

    fn mock(&self) -> &MockZitadel {
        self.mock.as_ref().expect("zitadel mock")
    }

    /// A Zitadel user with a UUID id, in org `org`.
    fn add_user(&self, org: &str, email: &str) -> uuid::Uuid {
        let id = uuid::Uuid::new_v4();
        self.mock().inner.lock().unwrap().users.push(MockUser {
            id: id.to_string(),
            email: email.into(),
            password: AU_PASSWORD.into(),
            verified: true,
            locked: false,
            org_id: org.into(),
            username: email.into(),
            inactive: false,
            created_at: "2026-10-03T10:00:00Z".into(),
        });
        id
    }

    fn add_session(&self, user: uuid::Uuid, zitadel_session: &str) {
        let now = chrono::Utc::now();
        self.metadata.app_sessions.lock().unwrap().push(AppSession {
            id: uuid::Uuid::new_v4(),
            project_id: self.project.id,
            user_id: user,
            zitadel_session_id: zitadel_session.into(),
            created_at: now,
            last_used_at: now,
            expires_at: now + chrono::Duration::days(30),
            revoked_at: None,
        });
    }

    fn live_sessions(&self, user: uuid::Uuid) -> usize {
        self.metadata
            .app_sessions
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.user_id == user && s.revoked_at.is_none())
            .count()
    }

    fn syncs(&self) -> Vec<AppUsersSyncJob> {
        self.jobs.app_users_sync_jobs.lock().unwrap().clone()
    }

    fn jobs_published(&self) -> Vec<AppUsersJob> {
        self.jobs.app_users_jobs.lock().unwrap().clone()
    }
}

fn put_body(au: &Au) -> Value {
    json!({ "database_id": au.db })
}

#[tokio::test]
async fn get_app_users_404_until_enabled_then_the_settings() {
    let au = au_env(false).await;
    let (status, body) = au.call(Method::GET, "/app-users", None).await;
    assert_eq!((status, code_of(&body)), (404, "app_users_disabled"), "{body}");
    au.set_auth(AppUsersStatus::Enabled);
    let (status, body) = au.call(Method::GET, "/app-users", None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["status"], "enabled");
    assert_eq!(body["database_id"], au.db.to_string());
    assert_eq!((body["pg_database"].as_str(), body["schema"].as_str(), body["table"].as_str()), (Some("web1"), Some("public"), Some("users")));
    assert_eq!((body["email_verification"].as_bool(), body["on_delete"].as_str()), (Some(false), Some("delete")));
    assert_eq!(body["zitadel_org_id"], AU_ORG);
    assert!(body["error"].is_null());
}

#[tokio::test]
async fn app_users_routes_refuse_a_deleting_project_and_another_tenants() {
    let au = au_env(false).await;
    let theirs = au.metadata.create_project(TenantId::new(), "Theirs").await.unwrap();
    let resp = send(&au.app, Method::GET, &format!("/v1/projects/{}/app-users", theirs.id), "org-1", None).await;
    assert_eq!(resp.status(), 404);
    au.metadata.begin_project_delete(au.tenant, au.project.id).await.unwrap();
    for (method, rest) in [(Method::GET, "/app-users"), (Method::DELETE, "/app-users"), (Method::GET, "/users")] {
        let (status, body) = au.call(method, rest, None).await;
        assert_eq!((status, code_of(&body)), (409, "project_deleting"), "{rest}: {body}");
    }
    let (status, body) = au.call(Method::PUT, "/app-users", Some(put_body(&au))).await;
    assert_eq!((status, code_of(&body)), (409, "project_deleting"), "{body}");
}

#[tokio::test]
async fn enable_app_users_202_publishes() {
    let au = au_env(true).await;
    let (status, body) = au.call(Method::PUT, "/app-users", Some(put_body(&au))).await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["status"], "enabling");
    assert_eq!((body["pg_database"].as_str(), body["schema"].as_str(), body["table"].as_str()), (Some("web1"), Some("public"), Some("users")));
    assert_eq!((body["email_verification"].as_bool(), body["on_delete"].as_str()), (Some(false), Some("delete")));
    assert_eq!(au.jobs_published(), vec![AppUsersJob::Enable { project_id: au.project.id }]);
    assert_eq!(*au.jobs.app_users_checks.lock().unwrap(), vec!["web1.public.users".to_string()]);
    assert_eq!(au.auth().status, AppUsersStatus::Enabling);
}

#[tokio::test]
async fn put_while_enabling_republishes() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabling);
    let (status, body) = au.call(Method::PUT, "/app-users", Some(put_body(&au))).await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(au.jobs_published(), vec![AppUsersJob::Enable { project_id: au.project.id }]);
    assert!(au.jobs.app_users_checks.lock().unwrap().is_empty(), "an existing location isn't checked again");
}

#[tokio::test]
async fn delete_while_disabling_republishes() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Disabling);
    let (status, body) = au.call(Method::DELETE, "/app-users", None).await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(au.jobs_published(), vec![AppUsersJob::Disable { project_id: au.project.id }]);
}

#[tokio::test]
async fn reserved_schema_is_422() {
    let au = au_env(true).await;
    for schema in ["chelabase", "information_schema", "pg_x", "pg_catalog"] {
        let mut body = put_body(&au);
        body["schema"] = json!(schema);
        let (status, answer) = au.call(Method::PUT, "/app-users", Some(body)).await;
        assert_eq!(status, 422, "{schema}: {answer}");
    }
    assert!(au.jobs_published().is_empty());
    assert!(au.metadata.project_auths.lock().unwrap().is_empty());
}

#[tokio::test]
async fn app_users_enable_refusals() {
    let au = au_env(true).await;
    let put = |extra: Value| {
        let mut body = put_body(&au);
        for (k, v) in extra.as_object().unwrap() {
            body[k] = v.clone();
        }
        body
    };
    // Identifiers and on_delete.
    for bad in [json!({"schema": "Bad-Name"}), json!({"table": "1x"}), json!({"table": ""}), json!({"on_delete": "keep"}), json!({"pg_database": ""})] {
        let (status, body) = au.call(Method::PUT, "/app-users", Some(put(bad.clone()))).await;
        assert_eq!(status, 422, "{bad}: {body}");
    }
    // Another project's database (same tenant) is unknown to this one.
    let other = au.metadata.create_project(au.tenant, "Other").await.unwrap();
    let foreign = project_database(au.tenant, other.id, DatabaseStatus::Ready);
    let foreign_id = foreign.database_id;
    au.metadata.insert_database(foreign);
    let (status, body) = au.call(Method::PUT, "/app-users", Some(json!({"database_id": foreign_id}))).await;
    assert_eq!((status, code_of(&body)), (404, "not_found"), "{body}");
    // Another tenant's.
    let theirs = project_database(TenantId::new(), ProjectId::new(), DatabaseStatus::Ready);
    let theirs_id = theirs.database_id;
    au.metadata.insert_database(theirs);
    let (status, _) = au.call(Method::PUT, "/app-users", Some(json!({"database_id": theirs_id}))).await;
    assert_eq!(status, 404);
    // Not ready.
    let pending = project_database(au.tenant, au.project.id, DatabaseStatus::Provisioning);
    let pending_id = pending.database_id;
    au.metadata.insert_database(pending);
    let (status, body) = au.call(Method::PUT, "/app-users", Some(json!({"database_id": pending_id}))).await;
    assert_eq!((status, code_of(&body)), (409, "database_not_ready"), "{body}");
    // The data API is off on a child branch.
    let child = au.metadata.add_branch(au.db, "dev", au.main, BranchKind::Normal, BranchStatus::Ready);
    au.metadata.add_branch_db(child, "web1", "owner", DatabaseOrigin::Main);
    let (status, body) = au.call(Method::PUT, "/app-users", Some(put_body(&au))).await;
    assert_eq!((status, code_of(&body)), (409, "data_api_not_enabled"), "{body}");
    au.metadata.set_data_api(child, "web1", chelabase_metadata::DataApiState::On);
    // The table exists.
    *au.jobs.app_users_check_reply.lock().unwrap() =
        Some(AppUsersCheckReply { ok: false, code: Some("app_users_table_exists".into()), error: Some("exists".into()) });
    let (status, body) = au.call(Method::PUT, "/app-users", Some(put_body(&au))).await;
    assert_eq!((status, code_of(&body)), (409, "app_users_table_exists"), "{body}");
    // Any other worker failure is a 503.
    *au.jobs.app_users_check_reply.lock().unwrap() =
        Some(AppUsersCheckReply { ok: false, code: Some("boom".into()), error: Some("no".into()) });
    let (status, _) = au.call(Method::PUT, "/app-users", Some(put_body(&au))).await;
    assert_eq!(status, 503);
    assert!(au.jobs_published().is_empty());
    assert!(au.metadata.project_auths.lock().unwrap().is_empty());
}

#[tokio::test]
async fn put_again_updates_settings_200() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let mut body = put_body(&au);
    body["email_verification"] = json!(true);
    body["on_delete"] = json!("mark");
    let (status, answer) = au.call(Method::PUT, "/app-users", Some(body)).await;
    assert_eq!(status, 200, "{answer}");
    assert_eq!((answer["email_verification"].as_bool(), answer["on_delete"].as_str()), (Some(true), Some("mark")));
    assert_eq!(answer["status"], "enabled");
    assert_eq!(au.auth().on_delete, OnDelete::Mark);
    assert!(au.jobs_published().is_empty());
}

#[tokio::test]
async fn put_with_other_location_409_location_fixed() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    for extra in [json!({"schema": "app"}), json!({"table": "people"}), json!({"pg_database": "web2"})] {
        let mut body = put_body(&au);
        body[extra.as_object().unwrap().keys().next().unwrap()] = extra.as_object().unwrap().values().next().unwrap().clone();
        let (status, answer) = au.call(Method::PUT, "/app-users", Some(body)).await;
        assert_eq!((status, code_of(&answer)), (409, "app_users_location_fixed"), "{extra}: {answer}");
    }
    let other = project_database(au.tenant, au.project.id, DatabaseStatus::Ready);
    let other_id = other.database_id;
    au.metadata.insert_database(other);
    let (status, answer) = au.call(Method::PUT, "/app-users", Some(json!({"database_id": other_id}))).await;
    assert_eq!((status, code_of(&answer)), (409, "app_users_location_fixed"), "{answer}");
    assert_eq!(au.auth().database_id, au.db);
}

#[tokio::test]
async fn retry_after_failed_202() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Failed);
    au.metadata.project_auths.lock().unwrap().get_mut(&au.project.id).unwrap().error = Some("boom".into());
    let (status, body) = au.call(Method::PUT, "/app-users", Some(put_body(&au))).await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["status"], "enabling");
    assert!(body["error"].is_null());
    assert_eq!(au.jobs_published(), vec![AppUsersJob::Enable { project_id: au.project.id }]);
}

#[tokio::test]
async fn disable_refused_while_users_exist() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.add_user(AU_ORG, "a@x.test");
    let (status, body) = au.call(Method::DELETE, "/app-users", None).await;
    assert_eq!((status, code_of(&body)), (409, "app_users_has_users"), "{body}");
    assert!(au.jobs_published().is_empty());
    assert_eq!(au.auth().status, AppUsersStatus::Enabled);
}

#[tokio::test]
async fn disable_202_publishes() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.add_user("o2", "elsewhere@x.test");
    let (status, body) = au.call(Method::DELETE, "/app-users", None).await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(body["status"], "disabling");
    assert_eq!(au.jobs_published(), vec![AppUsersJob::Disable { project_id: au.project.id }]);
    assert_eq!(au.auth().status, AppUsersStatus::Disabling);
}

#[tokio::test]
async fn create_user_inserts_then_creates_then_commits() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.seal_sync_role();
    let (status, body) = au.call(Method::POST, "/users", Some(json!({"email": "new@x.test", "password": AU_PASSWORD}))).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!((body["email"].as_str(), body["email_verified"].as_bool(), body["state"].as_str()), (Some("new@x.test"), Some(true), Some("active")));
    let id: uuid::Uuid = body["id"].as_str().unwrap().parse().unwrap();
    // The row went in first (no Zitadel user yet) and committed after Zitadel had the user.
    assert_eq!(au.udb.journal(), vec!["begin:new@x.test:users=0".to_string(), "commit:users=1".to_string()]);
    let user = au.mock().user_by_id(&id.to_string()).expect("zitadel user");
    assert_eq!((user.email.as_str(), user.org_id.as_str(), user.verified), ("new@x.test", AU_ORG, true));
    let target = au.udb.targets.lock().unwrap()[0].clone();
    assert_eq!(
        (target.branch_id, target.host.as_str(), target.port, target.database.as_str(), target.user.as_str(), target.password.as_str()),
        (au.main.as_uuid(), "127.0.0.1", 55432, "web1", "chelabase_auth_sync", "sync-pw")
    );
    assert_eq!((target.schema.as_str(), target.table.as_str()), ("public", "users"));
    let row = au.udb.rows.lock().unwrap()[0].clone();
    assert_eq!((row.id, row.email_verified), (id, true));
    assert!(row.data.is_empty());
    let syncs = au.syncs();
    assert_eq!(syncs.len(), 1);
    assert_eq!((syncs[0].user_id, syncs[0].origin_branch_id, syncs[0].data.is_none()), (id, Some(au.main), true));
    assert_eq!(syncs[0].project_id, au.project.id);
}

#[tokio::test]
async fn create_user_email_verified_false_is_passed_on() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.seal_sync_role();
    let (status, body) =
        au.call(Method::POST, "/users", Some(json!({"email": "u@x.test", "password": AU_PASSWORD, "email_verified": false}))).await;
    assert_eq!(status, 201, "{body}");
    assert_eq!(body["email_verified"], false);
    assert!(!au.udb.rows.lock().unwrap()[0].email_verified);
    assert!(!au.mock().user("u@x.test").unwrap().verified);
}

#[tokio::test]
async fn create_user_commit_failure_deletes_zitadel_user() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.seal_sync_role();
    *au.udb.fault.lock().unwrap() = Some(UdbFault::CommitFails);
    let (status, body) = au.call(Method::POST, "/users", Some(json!({"email": "n@x.test", "password": AU_PASSWORD}))).await;
    assert_eq!((status, code_of(&body)), (503, "unavailable"), "{body}");
    assert!(au.mock().user("n@x.test").is_none());
    assert!(au.mock().calls().iter().any(|c| c.starts_with("DELETE /v2/users/")), "{:?}", au.mock().calls());
    assert!(au.syncs().is_empty());
}

#[tokio::test]
async fn create_user_constraint_422_never_calls_zitadel() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.seal_sync_role();
    *au.udb.fault.lock().unwrap() = Some(UdbFault::Constraint);
    let (status, body) = au.call(Method::POST, "/users", Some(json!({"email": "n@x.test", "password": AU_PASSWORD}))).await;
    assert_eq!((status, code_of(&body)), (422, "constraint_violation"), "{body}");
    assert_eq!(body["error"]["constraint"], "users_nick_check");
    assert!(au.mock().calls().is_empty(), "{:?}", au.mock().calls());
    assert!(au.syncs().is_empty());
}

#[tokio::test]
async fn create_user_email_taken_and_weak_password() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.seal_sync_role();
    *au.udb.fault.lock().unwrap() = Some(UdbFault::EmailTaken);
    let (status, body) = au.call(Method::POST, "/users", Some(json!({"email": "n@x.test", "password": AU_PASSWORD}))).await;
    assert_eq!((status, code_of(&body)), (409, "email_taken"), "{body}");
    *au.udb.fault.lock().unwrap() = None;
    // The user exists in the org but the table doesn't know it (N2): the org is asked before
    // Zitadel could make a duplicate, and the row is rolled back.
    au.add_user(AU_ORG, "dup@x.test");
    let (status, body) = au.call(Method::POST, "/users", Some(json!({"email": "DUP@x.test", "password": AU_PASSWORD}))).await;
    assert_eq!((status, code_of(&body)), (409, "email_taken"), "{body}");
    assert_eq!(au.udb.journal().last().map(String::as_str), Some("rollback"));
    assert!(!au.mock().calls().contains(&"POST /v2/users/new".to_string()));
    // Zitadel's password policy.
    let (status, body) = au.call(Method::POST, "/users", Some(json!({"email": "w@x.test", "password": "short"}))).await;
    assert_eq!((status, code_of(&body)), (422, "weak_password"), "{body}");
    assert_eq!(au.udb.journal().last().map(String::as_str), Some("rollback"));
    assert!(au.syncs().is_empty());
}

#[tokio::test]
async fn list_users_pages_and_searches() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    for i in 0..55 {
        au.add_user(AU_ORG, &format!("user{i:02}@x.test"));
    }
    au.add_user("o2", "stranger@x.test");
    let (status, page) = au.call(Method::GET, "/users", None).await;
    assert_eq!(status, 200, "{page}");
    assert_eq!(page["users"].as_array().unwrap().len(), 50);
    let token = page["next_page_token"].as_str().expect("next token").to_string();
    assert_eq!(page["users"][0]["state"], "active");
    let (status, page2) = au.call(Method::GET, &format!("/users?page_token={token}"), None).await;
    assert_eq!(status, 200, "{page2}");
    assert_eq!(page2["users"].as_array().unwrap().len(), 5);
    assert!(page2["next_page_token"].is_null());
    let (_, found) = au.call(Method::GET, "/users?search=USER07", None).await;
    assert_eq!(found["users"].as_array().unwrap().len(), 1);
    assert_eq!(found["users"][0]["email"], "user07@x.test");
    let (status, bad) = au.call(Method::GET, "/users?page_token=not-a-token", None).await;
    assert_eq!(status, 422, "{bad}");
}

#[tokio::test]
async fn get_user_and_sessions() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user(AU_ORG, "a@x.test");
    au.add_session(id, "s1");
    let (status, body) = au.call(Method::GET, &format!("/users/{id}"), None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!((body["id"].as_str(), body["email"].as_str()), (Some(id.to_string().as_str()), Some("a@x.test")));
    let (status, sessions) = au.call(Method::GET, &format!("/users/{id}/sessions"), None).await;
    assert_eq!(status, 200, "{sessions}");
    assert_eq!(sessions.as_array().unwrap().len(), 1);
    for key in ["id", "created_at", "last_used_at", "expires_at"] {
        assert!(sessions[0][key].is_string(), "{key}");
    }
    assert!(sessions[0].get("zitadel_session_id").is_none());
    let (status, _) = au.call(Method::GET, &format!("/users/{}", uuid::Uuid::new_v4()), None).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn update_password_revokes_sessions() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user(AU_ORG, "a@x.test");
    au.add_session(id, "s1");
    au.add_session(id, "s2");
    let (status, body) = au.call(Method::PATCH, &format!("/users/{id}"), Some(json!({"password": "brand-new-pw-1"}))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(au.mock().user_by_id(&id.to_string()).unwrap().password, "brand-new-pw-1");
    assert_eq!(au.live_sessions(id), 0);
    let calls = au.mock().calls();
    assert!(calls.contains(&"DELETE /v2/sessions/s1".to_string()) && calls.contains(&"DELETE /v2/sessions/s2".to_string()), "{calls:?}");
    assert_eq!(au.syncs().len(), 1);
    assert_eq!(au.syncs()[0].user_id, id);
}

#[tokio::test]
async fn update_email_and_flag() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user(AU_ORG, "a@x.test");
    let (status, body) = au.call(Method::PATCH, &format!("/users/{id}"), Some(json!({"email": "b@x.test"}))).await;
    assert_eq!(status, 200, "{body}");
    let user = au.mock().user_by_id(&id.to_string()).unwrap();
    assert_eq!((user.email.as_str(), user.verified), ("b@x.test", true));
    // A new address can be set unverified (a code is sent).
    let (status, body) = au.call(Method::PATCH, &format!("/users/{id}"), Some(json!({"email": "c@x.test", "email_verified": false}))).await;
    assert_eq!(status, 200, "{body}");
    let user = au.mock().user_by_id(&id.to_string()).unwrap();
    assert_eq!((user.email.as_str(), user.verified), ("c@x.test", false));
    assert_eq!(body["email_verified"], false);
    assert_eq!(au.syncs().len(), 2);
}

/// The Studio drawer's "Email verified" toggle (browser pass: 503). Zitadel v4.19.1 refuses
/// `POST /v2/users/{id}/email` with the unchanged address (`Errors.User.Email.NotChanged`), so
/// the flag alone goes through `PATCH /v2/users/{id}`.
#[tokio::test]
async fn update_email_verified_alone_marks_the_current_address() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user(AU_ORG, "a@x.test");
    au.mock().inner.lock().unwrap().users.iter_mut().for_each(|u| u.verified = false);
    let (status, body) = au.call(Method::PATCH, &format!("/users/{id}"), Some(json!({"email_verified": true}))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!((body["email"].as_str(), body["email_verified"].as_bool()), (Some("a@x.test"), Some(true)));
    assert!(au.mock().user_by_id(&id.to_string()).unwrap().verified);
    let calls = au.mock().calls();
    assert!(calls.contains(&format!("PATCH /v2/users/{id}")), "{calls:?}");
    assert!(!calls.contains(&format!("POST /v2/users/{id}/email")), "{calls:?}");
    assert_eq!(au.syncs().len(), 1);
    // Again, and with the same address spelled out: nothing to change, still 200.
    for patch in [json!({"email_verified": true}), json!({"email": "a@x.test", "email_verified": true})] {
        let (status, body) = au.call(Method::PATCH, &format!("/users/{id}"), Some(patch.clone())).await;
        assert_eq!(status, 200, "{patch}: {body}");
        assert_eq!(body["email_verified"], true);
    }
    // Zitadel can't take the verification back without a new address: 422, nothing changes.
    let (status, body) = au.call(Method::PATCH, &format!("/users/{id}"), Some(json!({"email_verified": false}))).await;
    assert_eq!((status, code_of(&body)), (422, "invalid_request"), "{body}");
    assert!(au.mock().user_by_id(&id.to_string()).unwrap().verified);
    // An unverified user asked to stay unverified: nothing to do, 200.
    let other = au.add_user(AU_ORG, "b@x.test");
    au.mock().inner.lock().unwrap().users.iter_mut().filter(|u| u.id == other.to_string()).for_each(|u| u.verified = false);
    let (status, body) = au.call(Method::PATCH, &format!("/users/{other}"), Some(json!({"email_verified": false}))).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["email_verified"], false);
}

/// N2: an address two users of the org already share (made outside Chelabase's checks) is taken.
#[tokio::test]
async fn update_to_an_ambiguous_email_is_taken() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    au.seal_sync_role();
    let a = au.add_user(AU_ORG, "a@x.test");
    au.add_user(AU_ORG, "c@x.test");
    au.add_user(AU_ORG, "C@x.test");
    let (status, body) = au.call(Method::PATCH, &format!("/users/{a}"), Some(json!({"email": "c@x.test"}))).await;
    assert_eq!((status, code_of(&body)), (409, "email_taken"), "{body}");
    let (status, body) = au.call(Method::POST, "/users", Some(json!({"email": "c@x.test", "password": AU_PASSWORD}))).await;
    assert_eq!((status, code_of(&body)), (409, "email_taken"), "{body}");
}

/// Zitadel's precondition refusals (already in that state) are 409s, never 503.
#[tokio::test]
async fn state_changes_already_done_are_409() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user(AU_ORG, "a@x.test");
    let (status, body) = au.call(Method::POST, &format!("/users/{id}/reactivate"), None).await;
    assert_eq!((status, code_of(&body)), (409, "conflict"), "{body}");
    let (status, body) = au.call(Method::POST, &format!("/users/{id}/deactivate"), None).await;
    assert_eq!(status, 200, "{body}");
    let (status, body) = au.call(Method::POST, &format!("/users/{id}/deactivate"), None).await;
    assert_eq!((status, code_of(&body)), (409, "conflict"), "{body}");
}

#[tokio::test]
async fn update_email_taken_409() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let a = au.add_user(AU_ORG, "a@x.test");
    au.add_user(AU_ORG, "b@x.test");
    let (status, body) = au.call(Method::PATCH, &format!("/users/{a}"), Some(json!({"email": "B@x.test"}))).await;
    assert_eq!((status, code_of(&body)), (409, "email_taken"), "{body}");
    assert_eq!(au.mock().user_by_id(&a.to_string()).unwrap().email, "a@x.test");
    assert!(au.syncs().is_empty());
    // The user's own address isn't "taken".
    let (status, _) = au.call(Method::PATCH, &format!("/users/{a}"), Some(json!({"email": "a@x.test"}))).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn deactivate_revokes_and_syncs() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user(AU_ORG, "a@x.test");
    au.add_session(id, "s1");
    let (status, body) = au.call(Method::POST, &format!("/users/{id}/deactivate"), None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["state"], "inactive");
    assert!(au.mock().user_by_id(&id.to_string()).unwrap().inactive);
    assert_eq!(au.live_sessions(id), 0);
    assert!(au.mock().calls().contains(&"DELETE /v2/sessions/s1".to_string()));
    assert_eq!(au.syncs().len(), 1);
    let (status, body) = au.call(Method::POST, &format!("/users/{id}/reactivate"), None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["state"], "active");
    assert!(!au.mock().user_by_id(&id.to_string()).unwrap().inactive);
    assert_eq!(au.syncs().len(), 2);
}

#[tokio::test]
async fn revoke_sessions_is_204() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user(AU_ORG, "a@x.test");
    au.add_session(id, "s1");
    let (status, _) = au.call(Method::DELETE, &format!("/users/{id}/sessions"), None).await;
    assert_eq!(status, 204);
    assert_eq!(au.live_sessions(id), 0);
    assert!(au.mock().calls().contains(&"DELETE /v2/sessions/s1".to_string()));
}

#[tokio::test]
async fn delete_user_syncs() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user(AU_ORG, "a@x.test");
    au.add_session(id, "s1");
    let (status, _) = au.call(Method::DELETE, &format!("/users/{id}"), None).await;
    assert_eq!(status, 204);
    assert!(au.mock().user_by_id(&id.to_string()).is_none());
    assert_eq!(au.live_sessions(id), 0);
    assert_eq!(au.syncs().len(), 1);
    assert_eq!((au.syncs()[0].user_id, au.syncs()[0].data.is_none()), (id, true));
}

#[tokio::test]
async fn other_org_user_404() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user("o2", "stranger@x.test");
    for (method, rest, body) in [
        (Method::GET, format!("/users/{id}"), None),
        (Method::PATCH, format!("/users/{id}"), Some(json!({"password": "brand-new-pw-1"}))),
        (Method::POST, format!("/users/{id}/deactivate"), None),
        (Method::POST, format!("/users/{id}/reactivate"), None),
        (Method::GET, format!("/users/{id}/sessions"), None),
        (Method::DELETE, format!("/users/{id}/sessions"), None),
        (Method::DELETE, format!("/users/{id}"), None),
    ] {
        let (status, answer) = au.call(method.clone(), &rest, body).await;
        assert_eq!(status, 404, "{method} {rest}: {answer}");
    }
    let user = au.mock().user_by_id(&id.to_string()).unwrap();
    assert_eq!((user.password.as_str(), user.inactive), (AU_PASSWORD, false));
    assert!(au.syncs().is_empty());
}

#[tokio::test]
async fn user_routes_need_enabled_app_users() {
    let au = au_env(true).await;
    let (status, body) = au.call(Method::GET, "/users", None).await;
    assert_eq!((status, code_of(&body)), (409, "app_users_disabled"), "{body}");
    au.set_auth(AppUsersStatus::Enabling);
    let (status, body) = au.call(Method::POST, "/users", Some(json!({"email": "a@x.test", "password": AU_PASSWORD}))).await;
    assert_eq!((status, code_of(&body)), (409, "app_users_disabled"), "{body}");
}

#[tokio::test]
async fn routes_503_without_zitadel() {
    let au = au_env(false).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = uuid::Uuid::new_v4();
    for (method, rest, body) in [
        (Method::GET, "/users".to_string(), None),
        (Method::POST, "/users".to_string(), Some(json!({"email": "a@x.test", "password": AU_PASSWORD}))),
        (Method::GET, format!("/users/{id}"), None),
        (Method::PATCH, format!("/users/{id}"), Some(json!({"email_verified": true}))),
        (Method::DELETE, format!("/users/{id}"), None),
        (Method::DELETE, "/app-users".to_string(), None),
    ] {
        let (status, answer) = au.call(method.clone(), &rest, body).await;
        assert_eq!((status, code_of(&answer)), (503, "unavailable"), "{method} {rest}: {answer}");
    }
    // Reading the settings needs no Zitadel.
    let (status, _) = au.call(Method::GET, "/app-users", None).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn data_api_disable_refused_on_auth_database() {
    let au = au_env(false).await;
    au.metadata.add_branch_db(au.main, "web2", "owner", DatabaseOrigin::Sql);
    au.metadata.set_data_api(au.main, "web2", chelabase_metadata::DataApiState::On);
    au.set_auth(AppUsersStatus::Enabled);
    au.metadata.project_auths.lock().unwrap().get_mut(&au.project.id).unwrap().pg_database = "web2".into();
    let base = format!("/v1/databases/{}/branches/{}/pg-databases/web2/data-api", au.db, au.main);
    for query in ["", "?remove=true"] {
        let resp = send(&au.app, Method::DELETE, &format!("{base}{query}"), "org-1", None).await;
        let body = error_of(resp, 409).await;
        assert_eq!(code_of(&body), "conflict", "{body}");
        assert_eq!(message_of(&body), "app users use this database: disable app users first");
    }
    assert!(au.jobs.data_api_calls.lock().unwrap().is_empty());
    // Another Postgres database of the same Chelabase database is free to go.
    au.metadata.add_branch_db(au.main, "web3", "owner", DatabaseOrigin::Sql);
    let resp = send(&au.app, Method::DELETE, &format!("/v1/databases/{}/branches/{}/pg-databases/web3/data-api", au.db, au.main), "org-1", None).await;
    assert_eq!(resp.status(), 204);
}

#[tokio::test]
async fn list_projects_batches_the_auth_read() {
    let (app, metadata, tenant) = projects_app(FakeJobs::default()).await;
    let mut ids = vec![];
    for name in ["A", "B", "C"] {
        ids.push(metadata.create_project(tenant, name).await.unwrap().id);
    }
    metadata.project_auths.lock().unwrap().insert(
        ids[1],
        ProjectAuth {
            project_id: ids[1],
            status: AppUsersStatus::Enabled,
            error: None,
            zitadel_org_id: None,
            database_id: DatabaseId::new(),
            pg_database: "web1".into(),
            schema_name: "public".into(),
            table_name: "users".into(),
            email_verification: false,
            on_delete: OnDelete::Delete,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        },
    );
    let body: Value = send(&app, Method::GET, "/v1/projects", "org-1", None).await.json().await.unwrap();
    let states: Vec<_> = body.as_array().unwrap().iter().map(|p| p["app_users"].as_str()).collect();
    assert_eq!(states, vec![None, Some("enabled"), None]);
    assert_eq!(metadata.project_auth_reads.load(Ordering::SeqCst), 0);
    assert_eq!(metadata.project_auth_batch_reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn disable_retry_while_disabling_skips_the_users_check() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Disabling);
    // The org still lists a user (or is gone): the retry must not look.
    au.add_user(AU_ORG, "a@x.test");
    let (status, body) = au.call(Method::DELETE, "/app-users", None).await;
    assert_eq!(status, 202, "{body}");
    assert_eq!(au.jobs_published(), vec![AppUsersJob::Disable { project_id: au.project.id }]);
    assert!(au.mock().calls().is_empty(), "{:?}", au.mock().calls());
}

#[tokio::test]
async fn put_while_disabling_409() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Disabling);
    let (status, body) = au.call(Method::PUT, "/app-users", Some(put_body(&au))).await;
    assert_eq!((status, code_of(&body)), (409, "conflict"), "{body}");
    assert!(au.jobs_published().is_empty());
    assert_eq!(au.auth().status, AppUsersStatus::Disabling);
}

#[tokio::test]
async fn delete_user_succeeds_and_syncs_when_the_revoke_fails() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user(AU_ORG, "a@x.test");
    *au.metadata.fail_revoke.lock().unwrap() = true;
    let (status, _) = au.call(Method::DELETE, &format!("/users/{id}"), None).await;
    assert_eq!(status, 204);
    assert!(au.mock().user_by_id(&id.to_string()).is_none());
    assert_eq!(au.syncs().len(), 1);
}

#[tokio::test]
async fn update_with_a_weak_password_changes_nothing() {
    let au = au_env(true).await;
    au.set_auth(AppUsersStatus::Enabled);
    let id = au.add_user(AU_ORG, "a@x.test");
    let (status, body) =
        au.call(Method::PATCH, &format!("/users/{id}"), Some(json!({"email": "new@x.test", "password": "short"}))).await;
    assert_eq!((status, code_of(&body)), (422, "weak_password"), "{body}");
    let user = au.mock().user_by_id(&id.to_string()).unwrap();
    assert_eq!((user.email.as_str(), user.password.as_str()), ("a@x.test", AU_PASSWORD));
    assert!(au.syncs().is_empty());
}

// ---- saved queries

impl Branching {
    fn queries(&self) -> String {
        format!("/v1/databases/{}/sql-queries", self.db.as_uuid())
    }

    /// A request as another user of org-1.
    async fn send_as(&self, sub: &str, method: Method, path: &str, body: Option<Value>) -> reqwest::Response {
        let mut id = identity("org-1");
        id.sub = sub.into();
        let token = self.app.signer.sign(Service::Orchestrator.name(), &id).unwrap();
        let mut req = reqwest::Client::new()
            .request(method, format!("{}{path}", self.app.base))
            .header(IDENTITY_HEADER, token);
        if let Some(body) = body {
            req = req.json(&body);
        }
        req.send().await.unwrap()
    }

    async fn save(&self, name: &str, sql: &str) -> Value {
        let resp = self
            .send(Method::POST, &self.queries(), Some(serde_json::json!({"name": name, "sql": sql})))
            .await;
        assert_eq!(resp.status(), 201);
        resp.json().await.unwrap()
    }
}

#[tokio::test]
async fn saved_queries_list_create_get_update_delete() {
    let t = branching().await;
    let b = t.save("Beta", "select 2").await;
    let a = t
        .send(
            Method::POST,
            &t.queries(),
            Some(serde_json::json!({"name": "alpha", "sql": "select 1", "pg_database": "app", "role": "anon"})),
        )
        .await;
    assert_eq!(a.status(), 201);
    let a: Value = a.json().await.unwrap();
    assert_eq!(a["version"], 1);
    assert_eq!(a["pg_database"], "app");

    let list: Vec<Value> = t.send(Method::GET, &t.queries(), None).await.json().await.unwrap();
    assert_eq!(list.iter().map(|q| q["name"].as_str().unwrap()).collect::<Vec<_>>(), ["alpha", "Beta"]);
    assert!(list[0].get("sql").is_none(), "the list omits the text");

    let one = format!("{}/{}", t.queries(), a["id"].as_str().unwrap());
    let got: Value = t.send(Method::GET, &one, None).await.json().await.unwrap();
    assert_eq!(got["sql"], "select 1");

    // null clears a hint, an absent field stays.
    let resp = t
        .send(Method::PATCH, &one, Some(serde_json::json!({"version": 1, "sql": "select 3", "role": null})))
        .await;
    assert_eq!(resp.status(), 200);
    let upd: Value = resp.json().await.unwrap();
    assert_eq!((upd["version"].as_i64(), upd["sql"].as_str()), (Some(2), Some("select 3")));
    assert!(upd["role"].is_null());
    assert_eq!(upd["pg_database"], "app");
    assert_eq!(upd["name"], "alpha");

    assert_eq!(t.send(Method::DELETE, &one, None).await.status(), 204);
    assert_eq!(t.send(Method::GET, &one, None).await.status(), 404);
    assert_eq!(t.send(Method::DELETE, &one, None).await.status(), 404);
    let list: Vec<Value> = t.send(Method::GET, &t.queries(), None).await.json().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], b["id"]);
}

#[tokio::test]
async fn another_users_saved_query_is_404() {
    let t = branching().await;
    let q = t.save("mine", "select 1").await;
    let one = format!("{}/{}", t.queries(), q["id"].as_str().unwrap());
    for (method, body) in [
        (Method::GET, None),
        (Method::PATCH, Some(serde_json::json!({"version": 1, "name": "x"}))),
        (Method::DELETE, None),
    ] {
        let body = error_of(t.send_as("user-2", method, &one, body).await, 404).await;
        assert_eq!(code_of(&body), "not_found");
        assert_eq!(message_of(&body), "saved query not found");
    }
    let list: Vec<Value> = t.send_as("user-2", Method::GET, &t.queries(), None).await.json().await.unwrap();
    assert!(list.is_empty());
    // Same name for the other user is fine.
    let resp = t
        .send_as("user-2", Method::POST, &t.queries(), Some(serde_json::json!({"name": "MINE", "sql": "select 9"})))
        .await;
    assert_eq!(resp.status(), 201);
    // A query of another database is 404 too.
    let other = format!("/v1/databases/{}/sql-queries/{}", uuid::Uuid::new_v4(), q["id"].as_str().unwrap());
    assert_eq!(t.send(Method::GET, &other, None).await.status(), 404);
}

#[tokio::test]
async fn saved_query_conflict_carries_the_current_row() {
    let t = branching().await;
    let q = t.save("q", "select 1").await;
    let one = format!("{}/{}", t.queries(), q["id"].as_str().unwrap());
    let resp = t.send(Method::PATCH, &one, Some(serde_json::json!({"version": 1, "sql": "select 2"}))).await;
    assert_eq!(resp.status(), 200);
    let body = error_of(
        t.send(Method::PATCH, &one, Some(serde_json::json!({"version": 1, "sql": "select 3"}))).await,
        409,
    )
    .await;
    assert_eq!(code_of(&body), "saved_query_conflict");
    assert_eq!(body["error"]["current"]["sql"], "select 2");
    assert_eq!(body["error"]["current"]["version"], 2);
}

#[tokio::test]
async fn saved_query_name_taken_is_409() {
    let t = branching().await;
    t.save("Report", "select 1").await;
    let other = t.save("other", "select 2").await;
    let body = error_of(
        t.send(Method::POST, &t.queries(), Some(serde_json::json!({"name": " report ", "sql": "select 3"}))).await,
        409,
    )
    .await;
    assert_eq!(code_of(&body), "saved_query_name_taken");
    let one = format!("{}/{}", t.queries(), other["id"].as_str().unwrap());
    let body = error_of(
        t.send(Method::PATCH, &one, Some(serde_json::json!({"version": 1, "name": "REPORT"}))).await,
        409,
    )
    .await;
    assert_eq!(code_of(&body), "saved_query_name_taken");
}

#[tokio::test]
async fn saved_query_cap_is_409() {
    let t = branching().await;
    let now = chrono::Utc::now();
    for i in 0..orchestrator::sql_editor::SAVED_QUERY_CAP {
        let q = SavedQuery {
            id: uuid::Uuid::new_v4(),
            name: format!("q{i}"),
            sql: "select 1".into(),
            pg_database: None,
            role: None,
            version: 1,
            created_at: now,
            updated_at: now,
        };
        t.metadata.saved_queries.lock().unwrap().push((t.db.as_uuid(), "user-1".into(), q));
    }
    let body = error_of(
        t.send(Method::POST, &t.queries(), Some(serde_json::json!({"name": "one more", "sql": "select 1"}))).await,
        409,
    )
    .await;
    assert_eq!(code_of(&body), "saved_query_cap");
}

#[tokio::test]
async fn saved_query_validation_is_422() {
    let t = branching().await;
    let q = t.save("q", "select 1").await;
    let one = format!("{}/{}", t.queries(), q["id"].as_str().unwrap());
    let long_name = "n".repeat(121);
    let big_sql = "s".repeat(1_048_577);
    let long_hint = "h".repeat(64);
    for body in [
        serde_json::json!({"name": "", "sql": "select 1"}),
        serde_json::json!({"name": "   ", "sql": "select 1"}),
        serde_json::json!({"name": long_name, "sql": "select 1"}),
        serde_json::json!({"name": "a", "sql": ""}),
        serde_json::json!({"name": "a", "sql": big_sql}),
        serde_json::json!({"name": "a", "sql": "select 1", "role": long_hint}),
        serde_json::json!({"name": "a", "sql": "select 1", "pg_database": long_hint}),
        serde_json::json!({"name": "a", "sql": "select 1", "extra": 1}),
        serde_json::json!({"sql": "select 1"}),
    ] {
        let resp = t.send(Method::POST, &t.queries(), Some(body.clone())).await;
        let err = error_of(resp, 422).await;
        assert_eq!(code_of(&err), "invalid_request", "{body}");
    }
    for body in [
        serde_json::json!({"version": 1, "name": ""}),
        serde_json::json!({"version": 1, "sql": ""}),
        serde_json::json!({"version": 1, "role": long_hint}),
        serde_json::json!({"name": "x"}),
        serde_json::json!({"version": 1, "bogus": true}),
    ] {
        let resp = t.send(Method::PATCH, &one, Some(body.clone())).await;
        let err = error_of(resp, 422).await;
        assert_eq!(code_of(&err), "invalid_request", "{body}");
    }
}

#[tokio::test]
async fn empty_saved_query_hints_are_null() {
    let t = branching().await;
    let resp = t
        .send(
            Method::POST,
            &t.queries(),
            Some(serde_json::json!({"name": "q", "sql": "select 1", "pg_database": "", "role": ""})),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let q: Value = resp.json().await.unwrap();
    assert!(q["pg_database"].is_null() && q["role"].is_null(), "{q}");
    let one = format!("{}/{}", t.queries(), q["id"].as_str().unwrap());
    let resp = t
        .send(Method::PATCH, &one, Some(serde_json::json!({"version": 1, "pg_database": "app", "role": "anon"})))
        .await;
    assert_eq!(resp.status(), 200);
    let resp = t
        .send(Method::PATCH, &one, Some(serde_json::json!({"version": 2, "pg_database": "", "role": ""})))
        .await;
    assert_eq!(resp.status(), 200);
    let q: Value = resp.json().await.unwrap();
    assert!(q["pg_database"].is_null() && q["role"].is_null(), "{q}");
}

#[tokio::test]
async fn saved_query_with_a_nul_byte_is_422() {
    let t = branching().await;
    let resp = t
        .send(Method::POST, &t.queries(), Some(serde_json::json!({"name": "q", "sql": "select 1\u{0}"})))
        .await;
    assert_eq!(code_of(&error_of(resp, 422).await), "invalid_request");
    let q = t.save("q", "select 1").await;
    let one = format!("{}/{}", t.queries(), q["id"].as_str().unwrap());
    let resp = t.send(Method::PATCH, &one, Some(serde_json::json!({"version": 1, "sql": "\u{0}"}))).await;
    assert_eq!(code_of(&error_of(resp, 422).await), "invalid_request");
}

#[tokio::test]
async fn saved_queries_of_a_deleted_database_are_404() {
    let t = branching().await;
    let q = t.save("q", "select 1").await;
    t.metadata
        .databases
        .lock()
        .unwrap()
        .get_mut(&t.db)
        .unwrap()
        .status = DatabaseStatus::Deleted;
    let one = format!("{}/{}", t.queries(), q["id"].as_str().unwrap());
    assert_eq!(t.send(Method::GET, &t.queries(), None).await.status(), 404);
    assert_eq!(t.send(Method::GET, &one, None).await.status(), 404);
    assert_eq!(t.send(Method::DELETE, &one, None).await.status(), 404);
}

// ---- SQL editor: run, cancel, schema ---------------------------------------------------

/// A ready database with the console's seeded Postgres databases (`web1` main and served,
/// `analytics` and `scratch` made in SQL, the system `postgres`) and a plain API role
/// `editor` on main.
async fn sql_app() -> Branching {
    let t = branching().await;
    let main = t.seeded();
    let ep = t.metadata.branch(main).endpoint_id.unwrap();
    t.metadata.add_role(&ep, "postgres", RoleOrigin::Owner, true, false);
    t.metadata.add_role(&ep, "editor", RoleOrigin::Api, false, false);
    t
}

impl Branching {
    async fn run_sql(&self, branch: BranchId, body: Value) -> reqwest::Response {
        self.send(Method::POST, &format!("{}/sql", self.branch(branch)), Some(body)).await
    }

    async fn sql_schema(&self, branch: BranchId, query: &str) -> reqwest::Response {
        self.send(Method::GET, &format!("{}/sql/schema{query}", self.branch(branch)), None).await
    }

    async fn cancel_run(&self, branch: BranchId, run_id: uuid::Uuid) -> reqwest::Response {
        self.send(Method::POST, &format!("{}/sql/runs/{run_id}/cancel", self.branch(branch)), None).await
    }

    fn role_grants(&self) -> Vec<(BranchId, String, String)> {
        self.jobs.role_grants.lock().unwrap().clone()
    }

    fn set_protected(&self, branch: BranchId) {
        self.metadata.branches.lock().unwrap().get_mut(&branch).unwrap().protected = true;
    }
}

fn run_body(sql: &str) -> Value {
    json!({ "run_id": uuid::Uuid::new_v4(), "sql": sql })
}

/// `body` with `key` set to `value`.
fn with(mut body: Value, key: &str, value: Value) -> Value {
    body[key] = value;
    body
}

#[tokio::test]
async fn run_validation_is_422() {
    let t = sql_app().await;
    let main = t.main();
    let big = "x".repeat(1_048_577);
    for body in [
        run_body(""),
        run_body(&big),
        with(run_body("select 1"), "timeout_ms", json!(99)),
        with(run_body("select 1"), "timeout_ms", json!(900_001)),
        with(run_body("select 1"), "bogus", json!(true)),
        with(run_body("select 1"), "role", json!("")),
        json!({ "sql": "select 1" }),
        with(run_body("select 1"), "run_id", json!("not-a-uuid")),
    ] {
        let resp = t.run_sql(main, body.clone()).await;
        let err = error_of(resp, 422).await;
        assert_eq!(code_of(&err), "invalid_request", "{}", &body.to_string()[..body.to_string().len().min(200)]);
    }
    // The bounds themselves are accepted (and get as far as the compute lookup).
    for timeout in [100, 900_000] {
        let resp = t.run_sql(main, with(run_body("select 1"), "timeout_ms", json!(timeout))).await;
        assert_eq!(code_of(&error_of(resp, 503).await), "unavailable");
    }
    assert_eq!(t.compute_lookups(), 2);
    assert!(t.role_grants().is_empty());
}

#[tokio::test]
async fn run_with_a_nul_byte_is_422() {
    let t = sql_app().await;
    let main = t.main();
    for confirm in [false, true] {
        let body = with(run_body("select 1;\u{0} select 2"), "confirm", json!(confirm));
        let err = error_of(t.run_sql(main, body).await, 422).await;
        assert_eq!(code_of(&err), "invalid_request");
        assert!(message_of(&err).contains("NUL"), "{err}");
    }
    assert_eq!(t.compute_lookups(), 0);
}

#[tokio::test]
async fn run_unknown_database_or_role_is_404() {
    let t = sql_app().await;
    let main = t.main();
    for body in [
        with(run_body("select 1"), "database", json!("nope")),
        with(run_body("select 1"), "database", json!("postgres")),
        with(run_body("select 1"), "role", json!("ghost")),
        // `analytics` never had the data API's machinery: no `anon` there.
        with(with(run_body("select 1"), "database", json!("analytics")), "role", json!("anon")),
    ] {
        let resp = t.run_sql(main, body.clone()).await;
        assert_eq!(code_of(&error_of(resp, 404).await), "not_found", "{body}");
    }
    let resp = t.run_sql(main, with(run_body("select 1"), "role", json!("ghost"))).await;
    assert_eq!(message_of(&error_of(resp, 404).await), "role not found");
    assert_eq!(t.compute_lookups(), 0);
    // A database made in SQL (not served by the data API) and a known role are accepted.
    let ok = with(with(run_body("select 1"), "database", json!("analytics")), "role", json!("editor"));
    assert_eq!(code_of(&error_of(t.run_sql(main, ok).await, 503).await), "unavailable");
    assert!(t.role_grants().is_empty(), "only anon and authenticated are granted");
}

#[tokio::test]
async fn run_reserved_role_is_422() {
    let t = sql_app().await;
    let main = t.main();
    let ep = t.metadata.branch(main).endpoint_id.unwrap();
    // Even when listed (a resync might show one), a reserved name is refused.
    t.metadata.add_role(&ep, "cloud_admin", RoleOrigin::Sql, true, false);
    for role in ["cloud_admin", "neon_superuser", "authenticator", "chelabase_admin", "pg_read_all_data", "PG_monitor"] {
        let resp = t.run_sql(main, with(run_body("select 1"), "role", json!(role))).await;
        assert_eq!(code_of(&error_of(resp, 422).await), "invalid_request", "{role}");
    }
    assert_eq!(t.compute_lookups(), 0);
}

#[tokio::test]
async fn risky_statement_needs_confirm() {
    let t = sql_app().await;
    let main = t.main();
    let resp = t.run_sql(main, run_body("SELECT 1; DROP TABLE notes")).await;
    let err = error_of(resp, 409).await;
    assert_eq!(code_of(&err), "sql_confirm_required");
    let statements = err["error"]["statements"].as_array().unwrap();
    assert_eq!(statements.len(), 1, "{err}");
    assert_eq!(statements[0]["index"], 1);
    assert_eq!(statements[0]["kind"], "DropStmt");
    assert_eq!(statements[0]["reason"], "risky");
    assert_eq!(statements[0]["text_preview"], "DROP TABLE notes");
    // Nothing was connected or granted.
    assert_eq!(t.compute_lookups(), 0);
    assert!(t.role_grants().is_empty());
    // `confirm: false` is the same as absent.
    let resp = t.run_sql(main, with(run_body("TRUNCATE notes"), "confirm", json!(false))).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "sql_confirm_required");
    assert_eq!(t.compute_lookups(), 0);
}

#[tokio::test]
async fn protected_branch_holds_inserts() {
    let t = sql_app().await;
    let main = t.main();
    t.set_protected(main);
    let resp = t.run_sql(main, run_body("INSERT INTO notes (body) VALUES ('x')")).await;
    let err = error_of(resp, 409).await;
    assert_eq!(code_of(&err), "sql_confirm_required");
    assert_eq!(err["error"]["statements"][0]["kind"], "InsertStmt");
    assert_eq!(err["error"]["statements"][0]["reason"], "protected");
    assert_eq!(t.compute_lookups(), 0);
    // A read passes the guard.
    let resp = t.run_sql(main, run_body("SELECT * FROM notes")).await;
    assert_eq!(code_of(&error_of(resp, 503).await), "unavailable");
}

#[tokio::test]
async fn confirm_skips_the_guard() {
    let t = sql_app().await;
    let main = t.main();
    t.set_protected(main);
    let resp = t.run_sql(main, with(run_body("DROP TABLE notes"), "confirm", json!(true))).await;
    // Past the guard: the compute isn't running in the fake.
    assert_eq!(code_of(&error_of(resp, 503).await), "unavailable");
    assert_eq!(t.compute_lookups(), 1);
}

#[tokio::test]
async fn anon_is_granted_once_then_cached() {
    let t = sql_app().await;
    let main = t.main();
    for _ in 0..2 {
        let resp = t.run_sql(main, with(run_body("select 1"), "role", json!("anon"))).await;
        assert_eq!(code_of(&error_of(resp, 503).await), "unavailable");
    }
    // The owner the editor connects as is the one granted the role.
    assert_eq!(t.role_grants(), vec![(main, "anon".to_string(), "postgres".to_string())]);
    let resp = t.run_sql(main, with(run_body("select 1"), "role", json!("authenticated"))).await;
    assert_eq!(resp.status(), 503);
    assert_eq!(t.role_grants().len(), 2);
    assert_eq!(t.compute_lookups(), 3);
}

#[tokio::test]
async fn grant_refusal_is_403() {
    let t = sql_app().await;
    let main = t.main();
    *t.jobs.role_grant.lock().unwrap() =
        Some(Ok(CheckDataRoleReply::refused("the data API isn't set up on this branch")));
    for _ in 0..2 {
        let resp = t.run_sql(main, with(run_body("select 1"), "role", json!("anon"))).await;
        let err = error_of(resp, 403).await;
        assert_eq!(code_of(&err), "role_refused");
        assert_eq!(message_of(&err), "the data API isn't set up on this branch");
    }
    // A refusal isn't remembered.
    assert_eq!(t.role_grants().len(), 2);
    assert_eq!(t.compute_lookups(), 0);
    // The other answers map as the console's role check does.
    for (answer, status, code) in [
        (Ok(CheckDataRoleReply::not_ready()), 409, "branch_not_ready"),
        (Err(ResetError::Timeout), 504, "timeout"),
        (Err(ResetError::Transport("no worker".into())), 503, "worker_unavailable"),
        (Ok(CheckDataRoleReply::failed("boom")), 500, "internal"),
    ] {
        *t.jobs.role_grant.lock().unwrap() = Some(answer);
        let resp = t.run_sql(main, with(run_body("select 1"), "role", json!("anon"))).await;
        assert_eq!(code_of(&error_of(resp, status).await), code);
    }
}

#[tokio::test]
async fn fifth_run_is_429() {
    let t = sql_app().await;
    let main = t.main();
    let runs = t.app.state.sql_editor.runs();
    let held: Vec<_> = (0..4)
        .map(|_| runs.reserve("user-1", uuid::Uuid::new_v4(), BranchId::new(), "web1").unwrap())
        .collect();
    let resp = t.run_sql(main, run_body("select 1")).await;
    assert_eq!(resp.status(), 429);
    assert_eq!(resp.headers()["retry-after"], "2");
    assert_eq!(code_of(&resp.json().await.unwrap()), "sql_run_limit");
    assert_eq!(t.compute_lookups(), 0);
    drop(held);
    let resp = t.run_sql(main, run_body("select 1")).await;
    assert_eq!(resp.status(), 503);
}

#[tokio::test]
async fn duplicate_run_id_is_409() {
    let t = sql_app().await;
    let main = t.main();
    let id = uuid::Uuid::new_v4();
    let _held = t.app.state.sql_editor.runs().reserve("user-1", id, main, "web1").unwrap();
    let resp = t.run_sql(main, json!({ "run_id": id, "sql": "select 1" })).await;
    assert_eq!(code_of(&error_of(resp, 409).await), "run_id_in_use");
    // Another user may use the same id.
    let resp = t
        .send_as("user-2", Method::POST, &format!("{}/sql", t.branch(main)), Some(json!({ "run_id": id, "sql": "select 1" })))
        .await;
    assert_eq!(resp.status(), 503);
}

#[tokio::test]
async fn cancel_unknown_run_is_404() {
    let t = sql_app().await;
    let main = t.main();
    let resp = t.cancel_run(main, uuid::Uuid::new_v4()).await;
    let err = error_of(resp, 404).await;
    assert_eq!(code_of(&err), "not_found");
    assert_eq!(message_of(&err), "run not found");
    // A run in flight is cancelled: 202 `{}` and its token fires.
    let id = uuid::Uuid::new_v4();
    let held = t.app.state.sql_editor.runs().reserve("user-1", id, main, "web1").unwrap();
    // Another user's cancel doesn't reach it.
    let resp = t
        .send_as("user-2", Method::POST, &format!("{}/sql/runs/{id}/cancel", t.branch(main)), None)
        .await;
    assert_eq!(resp.status(), 404);
    assert!(!held.token().is_cancelled());
    let resp = t.cancel_run(main, id).await;
    assert_eq!(resp.status(), 202);
    assert_eq!(resp.json::<Value>().await.unwrap(), json!({}));
    assert!(held.token().is_cancelled());
    // The branch must still be the caller's.
    let backup = t.backup(main);
    assert_eq!(code_of(&error_of(t.cancel_run(backup, id).await, 409).await), "backup_branch");
    let other = uuid::Uuid::new_v4();
    let resp = t.send(Method::POST, &format!("{}/{other}/sql/runs/{id}/cancel", t.branches()), None).await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn owner_password_unknown_is_409() {
    let t = sql_app().await;
    let main = t.main();
    t.seal_owner_password(None);
    let id = uuid::Uuid::new_v4();
    let resp = t.run_sql(main, json!({ "run_id": id, "sql": "select 1" })).await;
    let err = error_of(resp, 409).await;
    assert_eq!(code_of(&err), "owner_password_unknown");
    assert_eq!(t.compute_lookups(), 0);
    // The slot was given back.
    assert!(t.app.state.sql_editor.runs().reserve("user-1", id, main, "web1").is_ok());
}

#[tokio::test]
async fn run_refuses_backups_unready_branches_and_strangers() {
    let t = sql_app().await;
    let main = t.main();
    let backup = t.backup(main);
    assert_eq!(code_of(&error_of(t.run_sql(backup, run_body("select 1")).await, 409).await), "backup_branch");
    let pending = t.add("dev", main, BranchKind::Normal, BranchStatus::Pending);
    assert_eq!(code_of(&error_of(t.run_sql(pending, run_body("select 1")).await, 409).await), "branch_not_ready");
    // The branch checks come before the body's.
    assert_eq!(code_of(&error_of(t.run_sql(pending, json!({})).await, 409).await), "branch_not_ready");
    let resp = t
        .send(Method::POST, &format!("{}/{}/sql", t.branches(), uuid::Uuid::new_v4()), Some(run_body("select 1")))
        .await;
    assert_eq!(resp.status(), 404);
}

#[tokio::test]
async fn sql_confirm_required_is_audited_without_sql() {
    use sha2::Digest;
    let logs = Logs::default();
    let _guard = logs.capture();
    let t = sql_app().await;
    let main = t.main();
    let sql = "DROP TABLE secret_table_4111";
    let id = uuid::Uuid::new_v4();
    let resp = t.run_sql(main, json!({ "run_id": id, "sql": sql })).await;
    assert_eq!(resp.status(), 409);
    let text = logs.text();
    let audit: Vec<&str> = text.lines().filter(|l| l.contains("sql run")).collect();
    assert_eq!(audit.len(), 1, "{text}");
    let line = audit[0];
    let sha = hex::encode(sha2::Sha256::digest(sql.as_bytes()));
    for field in [
        "user=user-1".to_string(),
        format!("database_id={}", t.db.as_uuid()),
        format!("branch_id={}", main.as_uuid()),
        "pg_database=web1".to_string(),
        "role=owner".to_string(),
        format!("run_id={id}"),
        "outcome=confirm_required".to_string(),
        "held=DropStmt".to_string(),
        format!("sql_sha256={sha}"),
        "protected=false".to_string(),
        "confirmed=false".to_string(),
    ] {
        assert!(line.contains(&field), "{field} missing from {line}");
    }
    assert!(!text.contains("secret_table_4111"), "SQL reached the log:\n{text}");
}

#[tokio::test]
async fn sql_schema_refusals() {
    let t = sql_app().await;
    let main = t.main();
    for query in ["?database=nope", "?database=postgres"] {
        assert_eq!(code_of(&error_of(t.sql_schema(main, query).await, 404).await), "not_found", "{query}");
    }
    assert_eq!(t.compute_lookups(), 0);
    // No compute running.
    assert_eq!(code_of(&error_of(t.sql_schema(main, "").await, 503).await), "unavailable");
    assert_eq!(code_of(&error_of(t.sql_schema(main, "?database=analytics").await, 503).await), "unavailable");
    t.seal_owner_password(None);
    assert_eq!(code_of(&error_of(t.sql_schema(main, "").await, 409).await), "owner_password_unknown");
    let backup = t.backup(main);
    assert_eq!(code_of(&error_of(t.sql_schema(backup, "").await, 409).await), "backup_branch");
}

#[tokio::test]
async fn sql_schema_counts_against_the_run_limits() {
    let t = sql_app().await;
    let main = t.main();
    let runs = t.app.state.sql_editor.runs();
    let held: Vec<_> = (0..4)
        .map(|_| runs.reserve("user-1", uuid::Uuid::new_v4(), BranchId::new(), "web1").unwrap())
        .collect();
    let resp = t.sql_schema(main, "").await;
    assert_eq!(resp.status(), 429);
    assert_eq!(resp.headers()["retry-after"], "2");
    assert_eq!(code_of(&resp.json().await.unwrap()), "sql_run_limit");
    assert_eq!(t.compute_lookups(), 0);
    drop(held);
    // A schema read's slot is given back when it ends.
    assert_eq!(code_of(&error_of(t.sql_schema(main, "").await, 503).await), "unavailable");
    let held: Vec<_> = (0..4)
        .map(|_| runs.reserve("user-1", uuid::Uuid::new_v4(), main, "web1").unwrap())
        .collect();
    drop(held);
}

// ---- the SQL editor against a real Postgres (scratch database, scratch owner)

/// Reads an NDJSON run stream to its end: the events in order.
async fn read_events(mut resp: reqwest::Response) -> Vec<Value> {
    let mut buf = Vec::new();
    while let Some(chunk) = resp.chunk().await.unwrap() {
        buf.extend_from_slice(&chunk);
    }
    String::from_utf8(buf)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Owner sessions of the SQL editor on the scratch compute.
async fn editor_sessions(scratch: &ScratchCompute) -> i64 {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM pg_stat_activity WHERE usename = $1 AND application_name = 'chelabase-sql-editor'",
    )
    .bind(&scratch.owner)
    .fetch_one(&scratch.admin)
    .await
    .unwrap();
    n
}

/// Waits (up to 10 s) until the scratch compute has no SQL editor session.
async fn wait_no_editor_sessions(scratch: &ScratchCompute) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while editor_sessions(scratch).await > 0 {
        assert!(std::time::Instant::now() < deadline, "an editor session outlived its run");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn run_streams_events_over_http() {
    let logs = Logs::default();
    let _guard = logs.capture();
    let scratch = ScratchCompute::new().await;
    scratch.set_password("run-pass").await;
    let t = owner_console_app(&scratch).await;
    t.seal_owner_password(Some("run-pass"));
    let main = t.main();
    let id = uuid::Uuid::new_v4();
    let sql = "INSERT INTO notes (body) VALUES ('streamed words'); SELECT id, body FROM notes";
    let resp = t.run_sql(main, json!({ "run_id": id, "sql": sql })).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "application/x-ndjson");
    assert_eq!(resp.headers()["cache-control"], "no-store");
    assert_eq!(resp.headers()["x-accel-buffering"], "no");
    let events = read_events(resp).await;
    let kinds: Vec<&str> = events.iter().map(|e| e["t"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["complete", "columns", "rows", "complete", "done"], "{events:?}");
    assert_eq!(events[0]["tag"], "INSERT 0 1");
    assert_eq!(events[2]["rows"], json!([["1", "streamed words"]]));
    assert_eq!(events[3]["tag"], "SELECT 1");
    assert_eq!(events[4]["outcome"], "ok");
    // The connection is closed and the slot given back.
    wait_no_editor_sessions(&scratch).await;
    assert!(t.app.state.sql_editor.runs().reserve("user-1", id, main, &scratch.database).is_ok());
    // One audit line, with the tags, without the SQL or its values.
    let text = logs.text();
    let audit: Vec<&str> = text.lines().filter(|l| l.contains("sql run")).collect();
    assert_eq!(audit.len(), 1, "{text}");
    for field in ["outcome=ok", "statements=2", "tags=INSERT 0 1,SELECT 1", "role=owner", "confirmed=false"] {
        assert!(audit[0].contains(field), "{field} missing from {}", audit[0]);
    }
    assert!(!audit[0].contains("held="), "{}", audit[0]);
    assert!(!text.contains("streamed words"), "{text}");
    assert!(!text.contains("run-pass"), "{text}");
    scratch.drop().await;
}

#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn closing_the_stream_cancels_the_run() {
    let logs = Logs::default();
    let _guard = logs.capture();
    let scratch = ScratchCompute::new().await;
    scratch.set_password("cancel-pass").await;
    let t = owner_console_app(&scratch).await;
    t.seal_owner_password(Some("cancel-pass"));
    let main = t.main();

    // Cancelled through the route: the stream ends with `cancelled`.
    let id = uuid::Uuid::new_v4();
    let resp = t.run_sql(main, json!({ "run_id": id, "sql": "SELECT pg_sleep(30)" })).await;
    assert_eq!(resp.status(), 200);
    let started = std::time::Instant::now();
    assert_eq!(t.cancel_run(main, id).await.status(), 202);
    let events = read_events(resp).await;
    assert_eq!(events.last().unwrap()["t"], "done", "{events:?}");
    assert_eq!(events.last().unwrap()["outcome"], "cancelled", "{events:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(10));

    // Closed by the client: the server's session ends and the slot is given back.
    let id = uuid::Uuid::new_v4();
    let resp = t.run_sql(main, json!({ "run_id": id, "sql": "SELECT pg_sleep(30)" })).await;
    assert_eq!(resp.status(), 200);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while editor_sessions(&scratch).await == 0 {
        assert!(std::time::Instant::now() < deadline, "the run never started");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    drop(resp);
    wait_no_editor_sessions(&scratch).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while t.cancel_run(main, id).await.status() != 404 {
        assert!(std::time::Instant::now() < deadline, "the run is still registered");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let text = logs.text();
    let audit: Vec<&str> = text.lines().filter(|l| l.contains("sql run")).collect();
    assert_eq!(audit.len(), 2, "{text}");
    assert!(audit.iter().all(|l| l.contains("outcome=cancelled")), "{text}");
    scratch.drop().await;
}

#[tokio::test]
#[ignore = "needs the metadata Postgres (just deps-up)"]
async fn schema_route_returns_pgls_json() {
    let scratch = ScratchCompute::new().await;
    scratch.set_password("schema-pass").await;
    let t = owner_console_app(&scratch).await;
    t.seal_owner_password(Some("schema-pass"));
    let main = t.main();
    let resp = t.sql_schema(main, "").await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "application/json");
    assert_eq!(resp.headers()["cache-control"], "no-store");
    let body: Value = resp.json().await.unwrap();
    let tables = body["tables"].as_array().unwrap_or_else(|| panic!("no tables in {body}"));
    assert!(tables.iter().any(|t| t["name"] == "notes" && t["schema"] == "public"), "{body}");
    let columns = body["columns"].as_array().unwrap();
    assert!(columns.iter().any(|c| c["table_name"] == "notes" && c["name"] == "body"));
    wait_no_editor_sessions(&scratch).await;
    scratch.drop().await;
}
