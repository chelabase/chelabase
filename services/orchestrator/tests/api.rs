use axum::Router;
use cheladb_auth::internal::{
    generate_keypair_pem, AuthMethod, Identity, InternalSigner, InternalVerifier, IDENTITY_HEADER,
};
use cheladb_metadata::{
    BeginDelete, BranchDatabase, BranchDeleteOutcome, BranchKind, BranchRecord, BranchStatus, ClaimOutcome, CreateBranchOutcome, DatabaseOrigin, DataKeyRecord, DatabaseRecord,
    DatabaseStatus, MetadataError, NewBranch, NewDataKey, PasswordState, ProjectOverview, RoleLevel, RoleOrigin, RoleRow, SettingsPatch,
    TenantOverview,
};
use cheladb_nats::{
    BranchJob, CheckDataRoleReply, DatabaseSettingsJob, DeleteDatabaseJob, JobEnvelope, PointInTime,
    ProvisionDatabaseJob, ResetPasswordReply, RoleCreateRequest, RoleReply, RoleRequest, RESET_NOT_APPLIED,
};
use cheladb_routes::{Service, ENDPOINTS};
use cheladb_types::mode::Mode;
use cheladb_types::{BranchId, DatabaseId, ProjectId, TenantId};
use cheladb_secrets::{open_credential, seal_credential, Purpose, SecretKey};
use cheladb_types::data_key::{self, DataKeyKind};
use orchestrator::{
    build_router, AppState, JobPort, MetadataPort, ProxyAddress, ResetError, RestInternal,
};
use std::collections::{HashMap, HashSet};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::TcpListener;

type KeyRow = (DataKeyRecord, [u8; 32]);

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
        default_schema: "public".into(),
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
                if let Some(v) = &patch.default_schema {
                    db.default_schema = v.clone();
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
        match insert_key(&mut keys, new) {
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

    async fn branch_databases(&self, branch: BranchId) -> Result<Vec<BranchDatabase>, MetadataError> {
        self.branch_db_queries.fetch_add(1, Ordering::SeqCst);
        let mut rows = self.branch_dbs.lock().unwrap().get(&branch).cloned().unwrap_or_default();
        rows.sort_by_key(|d| (d.origin != DatabaseOrigin::Main, d.name.clone()));
        Ok(rows)
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
    /// Each login-role RPC as `create:<name>:<privileged>`, `reset:<name>` or `delete:<name>`.
    role_calls: Arc<Mutex<Vec<String>>>,
    /// What the worker answers to the role RPCs; `ok` when unset.
    role_reply: Arc<Mutex<Option<Result<RoleReply, ResetError>>>>,
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
        match self.role_check.lock().unwrap().clone() {
            Some(answer) => answer,
            None => Ok(CheckDataRoleReply::ok()),
        }
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
}

impl FakeJobs {
    fn role_answer(&self) -> Result<RoleReply, ResetError> {
        match self.role_reply.lock().unwrap().clone() {
            Some(answer) => answer,
            None => Ok(RoleReply { ok: true, code: None, error: None, keys: vec![] }),
        }
    }
}

/// A running orchestrator plus the signer standing in for the gateway.
struct TestApp {
    base: String,
    signer: InternalSigner,
    /// The secrets key the orchestrator decrypts with.
    key: SecretKey,
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
    let (private_pem, public_pem) = generate_keypair_pem();
    let key = SecretKey::generate();
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
        proxy_ca_file: ".cheladb/neon/proxy-ca.crt".into(),
    });
    let app: Router = build_router(state);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    TestApp {
        base: format!("http://{addr}"),
        signer: InternalSigner::from_pem(private_pem.as_bytes()).unwrap(),
        key,
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
    assert!(html.contains("Cheladb Management API"), "{html}");
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
            slug: "default".into(),
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
    assert_eq!(body["ca_file"], ".cheladb/neon/proxy-ca.crt");
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

/// Every endpoint `cheladb-routes` assigns to the orchestrator must be routed
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
            "{} {} is declared in cheladb-routes but the orchestrator answered {status}",
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
            "max_rows": 1000, "statement_timeout_ms": 8000, "max_connections": 5, "default_schema": "public",
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
        (Ok(CheckDataRoleReply { ok: false, error: None, kind: None }), 500, "internal", None),
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
    for schema in ["app", "_x", "a1_b2", &"a".repeat(63)] {
        let resp = t.send(Method::PATCH, &path, Some(json!({ "default_schema": schema }))).await;
        assert_eq!(resp.status(), 200, "{schema}");
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["default_schema"], schema);
    }
    for schema in ["", "1abc", "Public", "a-b", "a b", &"a".repeat(64), "pub\"lic", "x;drop"] {
        assert_eq!(
            t.send(Method::PATCH, &path, Some(json!({ "default_schema": schema }))).await.status(),
            422,
            "{schema:?}"
        );
    }
    // Refused values change nothing, and the stored values come back on the next PATCH.
    let db = t.metadata.database(t.db);
    assert_eq!((db.max_rows, db.statement_timeout_ms, db.max_connections), (100000, 120000, 50));
    assert_eq!(db.default_schema, "a".repeat(63));
    let body: Value = t.send(Method::PATCH, &path, Some(json!({}))).await.json().await.unwrap();
    assert_eq!(body["max_rows"], 100000);
    assert_eq!(body["default_schema"], "a".repeat(63));
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
        keys: vec![cheladb_nats::KeyRef { id: key, name: "ci".into() }],
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
        cheladb_secrets::seal_role_password(&t.app.key, &dev.to_string(), "app", "app-pw"),
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
            { "name": "web1", "owner": "postgres", "origin": "main", "main": true },
            { "name": "alpha", "owner": "etl", "origin": "sql", "main": false },
            { "name": "postgres", "owner": "cloud_admin", "origin": "system", "main": false },
            { "name": "zeta", "owner": "etl", "origin": "sql", "main": false },
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
        cheladb_secrets::seal_role_password(&t.app.key, &BranchId::new().to_string(), "app", "x"),
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
