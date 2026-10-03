//! In-memory ports for the worker's tests. Every call that changes something is journaled in
//! one shared [`Journal`], so tests can assert on the order across ports.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use cheladb_metadata::{
    AppSession, AppUsersStatus, BranchAuthSync, OnDelete, ProjectAuth, ProjectRecord,
    ProjectStatus, BranchConnectionRow, BranchDeleteOutcome, BranchKind, BranchRecord, BranchStatus,
    CreateBranchOutcome, DataApiSettings, DataKeyRecord, DatabaseRecord, DatabaseSettings,
    DatabaseStatus, ENDPOINT_LOCK_TIMEOUT, EndpointState, InsertRoleOutcome, LiveBranch,
    LiveDatabase, MetadataError, NeonDatabase, NeonEndpoint, NeonRole, NewBranch, NewDataKey,
    NewEndpoint, ObservedDatabase, ResyncApplied, ResyncWrite, RoleCopy, RoleLevel, RoleOrigin,
    SealedChange,
};
use cheladb_neon::backend::fake::FakeBackend;
pub use cheladb_neon::backend::fake::Journal;
use cheladb_neon::spec::Placement;
use cheladb_neon::storcon::{LsnAtTime, LsnKind};
use cheladb_neon::{Lsn, NeonId, StorconError};
use cheladb_secrets::SecretKey;
use cheladb_types::data_key::DataKeyKind;
use cheladb_types::{BranchId, DatabaseId, ProjectId, TenantId};
use chrono::{DateTime, Utc};
use futures_util::future::BoxFuture;

use cheladb_metadata::{BranchDatabase, DataApiState, DatabaseOrigin, ServedDatabase};

use crate::Worker;
use crate::ports::{CacheNotifier, SpecPusher, Storage, WorkerMetadata};

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn record(journal: &Journal, entry: impl Into<String>) {
    lock(journal).push(entry.into());
}

/// Index of the first journal entry starting with `prefix`.
pub fn position(journal: &Journal, prefix: &str) -> Option<usize> {
    lock(journal).iter().position(|e| e.starts_with(prefix))
}

pub fn entries(journal: &Journal) -> Vec<String> {
    lock(journal).clone()
}

/// A `branch_databases` row as the data API sees it.
#[derive(Debug, Clone)]
pub struct FakeDbRow {
    pub name: String,
    pub state: DataApiState,
    pub error: Option<String>,
    pub schema: String,
    pub main: bool,
}

impl FakeDbRow {
    pub fn new(name: &str, state: DataApiState, schema: &str) -> Self {
        Self { name: name.to_string(), state, error: None, schema: schema.to_string(), main: false }
    }
}

pub struct FakeEndpoint {
    pub endpoint: NeonEndpoint,
    pub roles: Vec<NeonRole>,
    pub databases: Vec<NeonDatabase>,
    pub needs_reconfigure: bool,
    pub roles_version: i64,
    /// What `neon_roles` holds beyond [`NeonRole`] for each non-owner row. A row with a
    /// verifier is in `roles` too; an MD5 row (no verifier) isn't.
    pub logins: HashMap<String, FakeLogin>,
    /// `branch_databases` as the last resync set it.
    pub branch_databases: Vec<ObservedDatabase>,
    /// 0 for a new row, as the column's default.
    pub compute_settings_version: i32,
    /// Rows marked `password_unreadable` (C4); a new verifier or sealed password unmarks.
    pub unreadable: HashSet<String>,
}

/// A non-owner `neon_roles` row beyond its verifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeLogin {
    pub origin: RoleOrigin,
    pub password_enc: Option<String>,
    pub pending: bool,
    pub login: bool,
    pub level: RoleLevel,
    pub api_owner_level: bool,
}

impl FakeEndpoint {
    /// A non-owner row's verifier (`None` for MD5 or no row).
    pub fn verifier(&self, name: &str) -> Option<String> {
        self.roles
            .iter()
            .find(|r| r.name == name)
            .map(|r| r.scram_verifier.clone())
    }

    /// Sets (or, with `None`, removes) a role's entry in `roles`.
    pub fn set_verifier(&mut self, name: &str, verifier: Option<&str>) {
        match (self.roles.iter_mut().find(|r| r.name == name), verifier) {
            (Some(r), Some(v)) => r.scram_verifier = v.to_string(),
            (None, Some(v)) => self.roles.push(NeonRole {
                name: name.to_string(),
                scram_verifier: v.to_string(),
            }),
            (_, None) => self.roles.retain(|r| r.name != name),
        }
    }

    /// Copies counted toward the cap: rows with a verifier.
    fn copy_count(&self) -> usize {
        self.logins
            .keys()
            .filter(|n| self.roles.iter().any(|r| &r.name == *n))
            .count()
    }
}

/// A one-off change to the fake's state, run from inside a write.
pub type StateHook = Box<dyn FnOnce(&mut FakeState) + Send>;

#[derive(Default)]
pub struct FakeState {
    pub databases: HashMap<DatabaseId, DatabaseRecord>,
    pub branches: HashMap<BranchId, BranchRecord>,
    pub endpoints: HashMap<String, FakeEndpoint>,
    /// Seconds since a database's row last changed (0 when absent).
    pub ages: HashMap<DatabaseId, i64>,
    /// The method that fails, e.g. `"set_branch_ready"`.
    pub fail: Option<&'static str>,
    /// The Nth `get_database` call (1-based) first flips the row to `deleting`.
    pub deleting_on_get: Option<u32>,
    /// `set_branch_ready` first flips the row to `deleting`, as a delete landing just before it
    /// (the database's delete for main, the branch's own for any other branch).
    pub deleting_before_set_ready: bool,
    /// The Nth `get_branch` call (1-based) of that branch first begins its delete.
    pub deleting_on_get_branch: Option<(BranchId, u32)>,
    /// Seconds since a branch's row last changed (0 when absent).
    pub branch_ages: HashMap<BranchId, i64>,
    /// `(method, branch)`: that branch's delete begins just before the named write runs, as
    /// another run or a delete landing between a read and a guarded write.
    pub delete_before_write: Option<(&'static str, BranchId)>,
    /// Runs once, just before the next `claim_stale_reset`: a write landing between a run's
    /// first read and its claim.
    pub before_claim_stale_reset: Option<StateHook>,
    /// `branch_databases` per branch, with the data API state. A new endpoint writes the main
    /// row `on`, as `public`; tests add the others.
    pub db_rows: HashMap<BranchId, Vec<FakeDbRow>>,
    /// Data API keys; live unless their id is in `revoked_keys`.
    pub data_keys: Vec<NewDataKey>,
    pub revoked_keys: HashSet<uuid::Uuid>,
    /// Runs once, just before the next `finish_enable_data_api`: a write landing while the
    /// enable's bootstrap ran (a `default_schema` PATCH, say).
    pub before_finish_enable: Option<StateHook>,
    /// Runs once, just before the next `insert_data_key`: a concurrent run's insert.
    pub before_insert_data_key: Option<StateHook>,
    /// Runs once, just before the next `set_authenticator_password`: a concurrent run's write.
    pub before_set_authenticator_password: Option<StateHook>,
    /// The next N `apply_resync` calls answer `VersionMoved` (and bump the version, as a
    /// concurrent write would).
    pub resync_version_moves: u32,
    /// `delete_role_copy` finds the row already gone (removed meanwhile, say by a forwarded
    /// `DROP ROLE`).
    pub role_row_vanishes: bool,
    /// The next N `create_endpoint_for_branch` calls first flip the parent's `protected`, as
    /// a user toggling Protect between the read and the insert (C8).
    pub protection_flips: u32,
    /// Projects, their app users settings, the branches' sync passwords and app sessions.
    pub projects: HashMap<ProjectId, ProjectRecord>,
    pub project_auth: HashMap<ProjectId, ProjectAuth>,
    pub auth_sync: HashMap<BranchId, BranchAuthSync>,
    /// A concurrent bootstrap that wins the next claim: `(pg_database, sealed)` is stored just
    /// before it.
    pub auth_sync_racer: Option<(String, String)>,
    pub app_sessions: Vec<AppSession>,
    get_calls: u32,
    get_branch_calls: HashMap<BranchId, u32>,
}

impl FakeState {
    /// Begins `id`'s delete at its `n`th `get_branch` (1-based) counted from now.
    pub fn delete_at_get_branch(&mut self, id: BranchId, n: u32) {
        let seen = self.get_branch_calls.get(&id).copied().unwrap_or(0);
        self.deleting_on_get_branch = Some((id, seen + n));
    }

    /// What `begin_delete` does: the database and its live branches to `deleting`.
    fn begin_delete(&mut self, id: DatabaseId) {
        if let Some(db) = self.databases.get_mut(&id)
            && live(db.status)
        {
            db.status = DatabaseStatus::Deleting;
            for b in self.branches.values_mut() {
                if b.database_id == id && branch_live(b.status) {
                    b.status = BranchStatus::Deleting;
                }
            }
        }
    }

    /// What `begin_branch_delete` does: the branch to `deleting` unless it's already deleting
    /// or deleted, then its live backups too once it is deleting. True when the branch moved.
    fn begin_branch_delete(&mut self, id: BranchId) -> bool {
        let moved = match self.branches.get_mut(&id) {
            Some(b) if branch_live(b.status) => {
                b.status = BranchStatus::Deleting;
                true
            }
            _ => false,
        };
        if self.branches.get(&id).map(|b| b.status) == Some(BranchStatus::Deleting) {
            for b in self.branches.values_mut() {
                if b.parent_branch_id == Some(id)
                    && b.kind == BranchKind::Backup
                    && branch_live(b.status)
                {
                    b.status = BranchStatus::Deleting;
                }
            }
        }
        moved
    }

    /// `begin_branch_delete`'s guard: while the branch is live, its own live `normal`
    /// children, else those of its live backups (oldest first), refuse the delete.
    fn children_blocking_delete(&self, id: BranchId) -> Option<BranchDeleteOutcome> {
        if !self
            .branches
            .get(&id)
            .is_some_and(|b| branch_live(b.status))
        {
            return None;
        }
        let children_of = |parent: BranchId| {
            let mut names: Vec<String> = self
                .branches
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
        let own = children_of(id);
        if !own.is_empty() {
            return Some(BranchDeleteOutcome::HasChildren {
                backup: None,
                children: own,
            });
        }
        let backups = sorted(self.branches.values().filter(|b| {
            b.parent_branch_id == Some(id)
                && b.kind == BranchKind::Backup
                && b.status != BranchStatus::Deleted
        }));
        backups.into_iter().find_map(|backup| {
            let children = children_of(backup.id);
            (!children.is_empty()).then_some(BranchDeleteOutcome::HasChildren {
                backup: Some(backup.name),
                children,
            })
        })
    }

    fn has_live_public_key(&self, branch: BranchId) -> bool {
        self.live_keys(branch)
            .any(|k| k.kind == DataKeyKind::Public)
    }

    fn live_keys(&self, branch: BranchId) -> impl Iterator<Item = &NewDataKey> {
        self.data_keys
            .iter()
            .filter(move |k| k.branch_id == branch && !self.revoked_keys.contains(&k.id))
    }

    fn name_taken(&self, db: DatabaseId, name: &str) -> bool {
        self.branches
            .values()
            .any(|b| b.database_id == db && b.name == name && b.status != BranchStatus::Deleted)
    }

    /// Points `endpoint_id`'s row (if any) at `timeline_id`.
    fn move_endpoint(&mut self, endpoint_id: Option<&str>, timeline_id: &str) {
        if let Some(ep) = endpoint_id.and_then(|ep| self.endpoints.get_mut(ep)) {
            ep.endpoint.timeline_id = timeline_id.to_string();
        }
    }

    /// Main follows its database while provisioning (ruling 4), only while it is itself
    /// pending, provisioning or failed.
    fn mirror_on_main(&mut self, id: DatabaseId, status: BranchStatus, error: Option<&str>) {
        if let Some(main) = self.branches.get_mut(&BranchId::from(id))
            && matches!(
                main.status,
                BranchStatus::Pending | BranchStatus::Provisioning | BranchStatus::Failed
            )
        {
            main.status = status;
            main.error = error.map(str::to_string);
        }
    }
}

/// Mirrors the SQL guards of `MetadataClient`.
#[derive(Clone)]
pub struct FakeMetadata {
    pub state: Arc<Mutex<FakeState>>,
    pub journal: Journal,
    /// One lock per endpoint, like the advisory locks behind `lock_endpoint`.
    pub locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

fn live(status: DatabaseStatus) -> bool {
    !matches!(status, DatabaseStatus::Deleting | DatabaseStatus::Deleted)
}

fn branch_live(status: BranchStatus) -> bool {
    !matches!(status, BranchStatus::Deleting | BranchStatus::Deleted)
}

impl FakeMetadata {
    pub fn state(&self) -> MutexGuard<'_, FakeState> {
        lock(&self.state)
    }

    /// A database row with the given status and nothing else, and its main branch (same id
    /// and status), as `insert_database_pending` makes them.
    pub fn insert_database(&self, name: &str, status: DatabaseStatus) -> DatabaseId {
        let id = DatabaseId::new();
        let now = chrono::Utc::now();
        let mut state = self.state();
        state.databases.insert(
            id,
            DatabaseRecord {
                database_id: id,
                project_id: ProjectId::new(),
                tenant_id: TenantId::new(),
                name: name.to_string(),
                status,
                database_name: None,
                owner_role: None,
                error: None,
                neon_tenant_id: None,
                history_window: Duration::from_secs(7 * 24 * 3600),
                max_branches: 5,
                max_rows: 1000,
                statement_timeout_ms: 8000,
                max_connections: 5,
            },
        );
        state.branches.insert(
            BranchId::from(id),
            BranchRecord {
                id: BranchId::from(id),
                database_id: id,
                name: "main".to_string(),
                parent_branch_id: None,
                kind: BranchKind::Normal,
                status: BranchStatus::from(status),
                timeline_id: None,
                pending_timeline_id: None,
                pending_timeline_at: None,
                superseded_timeline_id: None,
                endpoint_id: None,
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

    pub fn database(&self, id: DatabaseId) -> DatabaseRecord {
        self.state().databases[&id].clone()
    }

    /// The database's main branch.
    pub fn main(&self, id: DatabaseId) -> BranchRecord {
        self.state().branches[&BranchId::from(id)].clone()
    }

    pub fn branch(&self, id: BranchId) -> BranchRecord {
        self.state().branches[&id].clone()
    }

    /// What `create_branch` does, with its guards: a live database, a ready parent of the same
    /// database, a free name, and the cap on live `normal` branches besides main.
    pub fn create_branch(&self, b: &NewBranch<'_>) -> CreateBranchOutcome {
        let mut state = self.state();
        let max = match state.databases.get(&b.database_id) {
            Some(db) if live(db.status) => db.max_branches,
            _ => return CreateBranchOutcome::DatabaseNotLive,
        };
        let parent_ready = state
            .branches
            .get(&b.parent_branch_id)
            .is_some_and(|p| p.database_id == b.database_id && p.status == BranchStatus::Ready);
        if !parent_ready {
            return CreateBranchOutcome::ParentNotReady;
        }
        if state.name_taken(b.database_id, b.name) {
            return CreateBranchOutcome::NameTaken;
        }
        let live_normal = state
            .branches
            .values()
            .filter(|x| {
                x.database_id == b.database_id
                    && x.status != BranchStatus::Deleted
                    && x.kind == BranchKind::Normal
                    && x.parent_branch_id.is_some()
            })
            .count();
        if live_normal >= usize::try_from(max).unwrap_or(0) {
            return CreateBranchOutcome::CapReached { max };
        }
        let now = chrono::Utc::now();
        state.branches.insert(
            b.id,
            BranchRecord {
                id: b.id,
                database_id: b.database_id,
                name: b.name.to_string(),
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
        CreateBranchOutcome::Created
    }

    /// An `active` project row.
    pub fn insert_project(&self) -> ProjectId {
        let id = ProjectId::new();
        let now = chrono::Utc::now();
        self.state().projects.insert(
            id,
            ProjectRecord {
                id,
                tenant_id: TenantId::new(),
                name: "app".to_string(),
                slug: "app".to_string(),
                status: ProjectStatus::Active,
                error: None,
                created_at: now,
                updated_at: now,
            },
        );
        id
    }

    /// Moves the database (and its branches' rows) into `project`.
    pub fn move_to_project(&self, db: DatabaseId, project: ProjectId) {
        self.state().databases.get_mut(&db).unwrap().project_id = project;
    }

    /// A `project_auth` row for `project` with the defaults (`public.users` in `app`).
    pub fn insert_auth(&self, project: ProjectId, db: DatabaseId, status: AppUsersStatus) {
        let now = chrono::Utc::now();
        self.state().project_auth.insert(
            project,
            ProjectAuth {
                project_id: project,
                status,
                error: None,
                zitadel_org_id: None,
                database_id: db,
                pg_database: "app".to_string(),
                schema_name: "public".to_string(),
                table_name: "users".to_string(),
                email_verification: false,
                on_delete: OnDelete::Delete,
                created_at: now,
                updated_at: now,
            },
        );
    }

    pub fn auth(&self, project: ProjectId) -> Option<ProjectAuth> {
        self.state().project_auth.get(&project).cloned()
    }

    /// A live app session of `user`, with Zitadel session `zitadel_session_id`.
    pub fn add_session(&self, project: ProjectId, user: uuid::Uuid, zitadel_session_id: &str) -> uuid::Uuid {
        let id = uuid::Uuid::new_v4();
        let now = chrono::Utc::now();
        self.state().app_sessions.push(AppSession {
            id,
            project_id: project,
            user_id: user,
            zitadel_session_id: zitadel_session_id.to_string(),
            created_at: now,
            last_used_at: now,
            expires_at: now + chrono::Duration::days(30),
            revoked_at: None,
        });
        id
    }

    pub fn endpoint_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.state().endpoints.keys().cloned().collect();
        ids.sort();
        ids
    }

    fn write(&self, method: &'static str) -> Result<MutexGuard<'_, FakeState>, MetadataError> {
        record(&self.journal, format!("metadata.{method}"));
        let mut state = self.read(method)?;
        if let Some((m, id)) = state.delete_before_write
            && m == method
        {
            state.delete_before_write = None;
            state.begin_branch_delete(id);
        }
        Ok(state)
    }

    fn read(&self, method: &'static str) -> Result<MutexGuard<'_, FakeState>, MetadataError> {
        let state = self.state();
        if state.fail == Some(method) {
            return Err(MetadataError::Config(format!("injected {method} failure")));
        }
        Ok(state)
    }
}

impl WorkerMetadata for FakeMetadata {
    type EndpointLock = tokio::sync::OwnedMutexGuard<()>;

    async fn lock_endpoint(
        &self,
        endpoint_id: &str,
    ) -> Result<tokio::sync::OwnedMutexGuard<()>, MetadataError> {
        drop(self.write("lock_endpoint")?);
        let lock = lock(&self.locks)
            .entry(endpoint_id.to_string())
            .or_default()
            .clone();
        // tokio's mutex is fair too; like Postgres' `lock_timeout`, the wait is bounded.
        tokio::time::timeout(ENDPOINT_LOCK_TIMEOUT, lock.lock_owned())
            .await
            .map_err(|_| MetadataError::EndpointLockTimeout(endpoint_id.to_string()))
    }

    async fn get_database(&self, id: DatabaseId) -> Result<Option<DatabaseRecord>, MetadataError> {
        let mut state = self.read("get_database")?;
        state.get_calls += 1;
        if state.deleting_on_get == Some(state.get_calls) {
            state.begin_delete(id);
        }
        Ok(state.databases.get(&id).cloned())
    }

    async fn get_branch(&self, id: BranchId) -> Result<Option<BranchRecord>, MetadataError> {
        let mut state = self.read("get_branch")?;
        let calls = state.get_branch_calls.entry(id).or_default();
        *calls += 1;
        let calls = *calls;
        if state.deleting_on_get_branch == Some((id, calls)) {
            state.begin_branch_delete(id);
        }
        Ok(state.branches.get(&id).cloned())
    }

    async fn list_branches(&self, db: DatabaseId) -> Result<Vec<BranchRecord>, MetadataError> {
        let state = self.read("list_branches")?;
        let mut branches: Vec<BranchRecord> = state
            .branches
            .values()
            .filter(|b| b.database_id == db && b.status != BranchStatus::Deleted)
            .cloned()
            .collect();
        branches.sort_by_key(|b| (b.created_at, b.id.to_string()));
        Ok(branches)
    }

    async fn set_status(
        &self,
        id: DatabaseId,
        status: DatabaseStatus,
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("set_status")?;
        match state.databases.get_mut(&id) {
            Some(db) if live(db.status) => {
                db.status = status;
                db.error = None;
            }
            _ => return Ok(false),
        }
        state.mirror_on_main(id, BranchStatus::from(status), None);
        Ok(true)
    }

    async fn create_endpoint_for_branch(
        &self,
        id: BranchId,
        timeline_id: &str,
        ep: &NewEndpoint<'_>,
        parent_protected_seen: bool,
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("create_endpoint_for_branch")?;
        // C8: the parent's flag is checked inside the insert; a change refuses it.
        let parent_id = state.branches.get(&id).and_then(|b| b.parent_branch_id);
        let flip = state.protection_flips > 0;
        if flip {
            state.protection_flips -= 1;
        }
        if let Some(parent) = parent_id.and_then(|p| state.branches.get_mut(&p)) {
            if flip {
                parent.protected = !parent.protected;
            }
            if parent.protected != parent_protected_seen {
                return Err(MetadataError::ParentProtectionChanged);
            }
        }
        let database_id = match state.branches.get_mut(&id) {
            Some(b) if branch_live(b.status) && b.endpoint_id.is_none() => {
                b.endpoint_id = Some(ep.id.to_string());
                b.timeline_id = Some(timeline_id.to_string());
                b.database_id
            }
            _ => return Ok(false),
        };
        if BranchId::from(database_id) == id
            && let Some(db) = state.databases.get_mut(&database_id)
        {
            db.database_name = Some(ep.database_name.to_string());
            db.owner_role = Some(ep.owner_role.to_string());
        }
        state.db_rows.insert(
            id,
            vec![FakeDbRow {
                main: true,
                ..FakeDbRow::new(ep.database_name, DataApiState::On, "public")
            }],
        );
        state.endpoints.insert(
            ep.id.to_string(),
            FakeEndpoint {
                endpoint: NeonEndpoint {
                    id: ep.id.to_string(),
                    tenant_id: ep.tenant_id.to_string(),
                    timeline_id: ep.timeline_id.to_string(),
                    compute_token_sha256: ep.compute_token_sha256.to_string(),
                },
                roles: std::iter::once(NeonRole {
                    name: ep.owner_role.to_string(),
                    scram_verifier: ep.owner_verifier.to_string(),
                })
                .chain(ep.role_copies.iter().filter_map(|l| {
                    Some(NeonRole {
                        name: l.name.to_string(),
                        scram_verifier: l.verifier?.to_string(),
                    })
                }))
                .collect(),
                databases: vec![NeonDatabase {
                    name: ep.database_name.to_string(),
                    owner: ep.owner_role.to_string(),
                }],
                needs_reconfigure: false,
                roles_version: 0,
                logins: ep
                    .role_copies
                    .iter()
                    .map(|l| {
                        let login = FakeLogin {
                            origin: l.origin,
                            password_enc: l.password_enc.map(str::to_string),
                            pending: l.pending,
                            login: l.login,
                            level: l.level,
                            api_owner_level: l.api_owner_level,
                        };
                        (l.name.to_string(), login)
                    })
                    .collect(),
                branch_databases: Vec::new(),
                compute_settings_version: 0,
                unreadable: HashSet::new(),
            },
        );
        Ok(true)
    }

    async fn set_neon_tenant(&self, db: DatabaseId, tenant: &str) -> Result<bool, MetadataError> {
        let mut state = self.write("set_neon_tenant")?;
        match state.databases.get_mut(&db) {
            Some(row) if live(row.status) => {
                row.neon_tenant_id = Some(tenant.to_string());
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn neon_endpoint(
        &self,
        endpoint_id: &str,
    ) -> Result<Option<NeonEndpoint>, MetadataError> {
        let state = self.read("neon_endpoint")?;
        Ok(state.endpoints.get(endpoint_id).map(|e| e.endpoint.clone()))
    }

    async fn neon_roles_versioned(
        &self,
        endpoint_id: &str,
    ) -> Result<(Vec<NeonRole>, i64), MetadataError> {
        record(
            &self.journal,
            format!("metadata.neon_roles_versioned {endpoint_id}"),
        );
        let state = self.read("neon_roles_versioned")?;
        Ok(state
            .endpoints
            .get(endpoint_id)
            .map(|e| (e.roles.clone(), e.roles_version))
            .unwrap_or_default())
    }

    async fn neon_databases(&self, endpoint_id: &str) -> Result<Vec<NeonDatabase>, MetadataError> {
        let state = self.read("neon_databases")?;
        Ok(state
            .endpoints
            .get(endpoint_id)
            .map(|e| e.databases.clone())
            .unwrap_or_default())
    }

    async fn set_branch_ready(
        &self,
        id: BranchId,
        sealed_password: &str,
        branched_at_lsn: Option<&str>,
        history_start: Option<DateTime<Utc>>,
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("set_branch_ready")?;
        let Some(database_id) = state.branches.get(&id).map(|b| b.database_id) else {
            return Ok(false);
        };
        let is_main = BranchId::from(database_id) == id;
        if state.deleting_before_set_ready {
            if is_main {
                state.begin_delete(database_id);
            } else {
                state.begin_branch_delete(id);
            }
        }
        let database_live = state
            .databases
            .get(&database_id)
            .is_some_and(|db| live(db.status));
        let branch = state.branches.get_mut(&id).expect("looked up above");
        if branch.status != BranchStatus::Provisioning || (is_main && !database_live) {
            return Ok(false);
        }
        branch.status = BranchStatus::Ready;
        branch.owner_password_enc = Some(sealed_password.to_string());
        branch.branched_at_lsn = branched_at_lsn.map(str::to_string);
        branch.error = None;
        branch.history_start = Some(history_start.unwrap_or(branch.created_at));
        if is_main && let Some(db) = state.databases.get_mut(&database_id) {
            db.status = DatabaseStatus::Ready;
            db.error = None;
        }
        Ok(true)
    }

    async fn set_failed(&self, id: DatabaseId, error: &str) -> Result<(), MetadataError> {
        let mut state = self.write("set_failed")?;
        if let Some(db) = state.databases.get_mut(&id)
            && live(db.status)
        {
            db.status = DatabaseStatus::Failed;
            db.error = Some(error.to_string());
            state.mirror_on_main(id, BranchStatus::Failed, Some(error));
        }
        Ok(())
    }

    async fn mark_deleted(&self, id: DatabaseId) -> Result<(), MetadataError> {
        let mut state = self.write("mark_deleted")?;
        if let Some(db) = state.databases.get_mut(&id) {
            db.status = DatabaseStatus::Deleted;
        }
        Ok(())
    }

    async fn mark_branch_deleted(&self, id: BranchId) -> Result<(), MetadataError> {
        let mut state = self.write("mark_branch_deleted")?;
        if let Some(b) = state.branches.get_mut(&id) {
            b.status = BranchStatus::Deleted;
            b.owner_password_enc = None;
        }
        Ok(())
    }

    async fn delete_endpoint(&self, endpoint_id: &str) -> Result<(), MetadataError> {
        let mut state = self.write("delete_endpoint")?;
        state.endpoints.remove(endpoint_id);
        for b in state.branches.values_mut() {
            if b.endpoint_id.as_deref() == Some(endpoint_id) {
                b.endpoint_id = None;
            }
        }
        Ok(())
    }

    async fn replace_owner_secret(
        &self,
        id: BranchId,
        endpoint_id: &str,
        role: &str,
        verifier: &str,
        sealed: &str,
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("replace_owner_secret")?;
        let state = &mut *state;
        let Some(branch) = state.branches.get_mut(&id) else {
            return Ok(false);
        };
        if branch.status != BranchStatus::Ready
            || branch.kind != BranchKind::Normal
            || branch.endpoint_id.as_deref() != Some(endpoint_id)
        {
            return Ok(false);
        }
        let Some(ep) = state.endpoints.get_mut(endpoint_id) else {
            return Ok(false);
        };
        let Some(r) = ep.roles.iter_mut().find(|r| r.name == role) else {
            return Ok(false);
        };
        branch.owner_password_enc = Some(sealed.to_string());
        r.scram_verifier = verifier.to_string();
        ep.needs_reconfigure = true;
        ep.roles_version += 1;
        Ok(true)
    }

    async fn clear_needs_reconfigure(
        &self,
        endpoint_id: &str,
        applied_version: i64,
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("clear_needs_reconfigure")?;
        let Some(ep) = state.endpoints.get_mut(endpoint_id) else {
            return Ok(false);
        };
        if !(ep.needs_reconfigure && ep.roles_version == applied_version) {
            return Ok(false);
        }
        // The owner's row only; the others' go through `confirm_role`.
        ep.needs_reconfigure = false;
        Ok(true)
    }

    async fn insert_api_role(
        &self,
        endpoint_id: &str,
        name: &str,
        owner_level: bool,
        verifier: &str,
        password_enc: &str,
        cap: u32,
    ) -> Result<InsertRoleOutcome, MetadataError> {
        let mut state = self.write("insert_api_role")?;
        let Some(ep) = state.endpoints.get_mut(endpoint_id) else {
            // `SELECT … FOR UPDATE` with `fetch_one`.
            return Err(MetadataError::Database(sqlx::Error::RowNotFound));
        };
        if ep.copy_count() >= usize::try_from(cap).unwrap_or(usize::MAX) {
            return Ok(InsertRoleOutcome::CapReached);
        }
        if ep.roles.iter().any(|r| r.name == name) || ep.logins.contains_key(name) {
            return Ok(InsertRoleOutcome::Exists);
        }
        ep.set_verifier(name, Some(verifier));
        ep.logins.insert(
            name.to_string(),
            FakeLogin {
                origin: RoleOrigin::Api,
                password_enc: Some(password_enc.to_string()),
                pending: true,
                login: true,
                level: if owner_level {
                    RoleLevel::OwnerLevel
                } else {
                    RoleLevel::Plain
                },
                api_owner_level: owner_level,
            },
        );
        ep.roles_version += 1;
        Ok(InsertRoleOutcome::Inserted)
    }

    async fn replace_role_secret(
        &self,
        endpoint_id: &str,
        name: &str,
        verifier: &str,
        password_enc: &str,
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("replace_role_secret")?;
        let Some(ep) = state.endpoints.get_mut(endpoint_id) else {
            return Ok(false);
        };
        let Some(login) = ep.logins.get_mut(name) else {
            return Ok(false);
        };
        login.password_enc = Some(password_enc.to_string());
        login.pending = true;
        ep.set_verifier(name, Some(verifier));
        ep.roles_version += 1;
        Ok(true)
    }

    async fn confirm_role(
        &self,
        endpoint_id: &str,
        name: &str,
        version: i64,
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("confirm_role")?;
        let Some(ep) = state.endpoints.get_mut(endpoint_id) else {
            return Ok(false);
        };
        if ep.roles_version != version {
            return Ok(false);
        }
        match ep.logins.get_mut(name) {
            Some(login) if login.pending => {
                login.pending = false;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn delete_role_copy(&self, endpoint_id: &str, name: &str) -> Result<bool, MetadataError> {
        let mut state = self.write("delete_role_copy")?;
        let vanishes = state.role_row_vanishes;
        let Some(ep) = state.endpoints.get_mut(endpoint_id) else {
            return Ok(false);
        };
        if vanishes {
            ep.logins.remove(name);
            ep.set_verifier(name, None);
            return Ok(false);
        }
        if ep.logins.remove(name).is_none() {
            return Ok(false);
        }
        ep.set_verifier(name, None);
        ep.roles_version += 1;
        Ok(true)
    }

    async fn role_copies(&self, endpoint_id: &str) -> Result<(Vec<RoleCopy>, i64), MetadataError> {
        record(&self.journal, format!("metadata.role_copies {endpoint_id}"));
        let state = self.read("role_copies")?;
        let Some(ep) = state.endpoints.get(endpoint_id) else {
            return Ok((Vec::new(), 0));
        };
        Ok((fake_copies(ep), ep.roles_version))
    }

    async fn apply_resync(
        &self,
        endpoint_id: &str,
        branch: BranchId,
        read_version: i64,
        write: &ResyncWrite,
    ) -> Result<ResyncApplied, MetadataError> {
        let mut state = self.write("apply_resync")?;
        let state = &mut *state;
        let Some(ep) = state.endpoints.get_mut(endpoint_id) else {
            return Err(MetadataError::Database(sqlx::Error::RowNotFound));
        };
        if state.resync_version_moves > 0 {
            state.resync_version_moves -= 1;
            ep.roles_version += 1;
            return Ok(ResyncApplied::VersionMoved);
        }
        if ep.roles_version != read_version {
            return Ok(ResyncApplied::VersionMoved);
        }
        let owner = ep.roles.first().map(|r| r.name.clone());
        let mut changed = Vec::new();
        for up in &write.upserts {
            if owner.as_deref() == Some(up.name.as_str()) {
                continue;
            }
            let before = ep.logins.get(&up.name).cloned();
            let before_verifier = ep.verifier(&up.name);
            let mut row = before.clone().unwrap_or(FakeLogin {
                origin: up.origin,
                password_enc: None,
                pending: up.pending,
                login: up.login,
                level: up.level,
                api_owner_level: false,
            });
            row.password_enc = match &up.password_enc {
                SealedChange::Keep => row.password_enc,
                SealedChange::Clear => None,
                SealedChange::Set(s) => Some(s.clone()),
            };
            row.login = up.login;
            row.level = up.level;
            row.pending = up.pending;
            if before_verifier != up.verifier || matches!(up.password_enc, SealedChange::Set(_)) {
                ep.unreadable.remove(&up.name);
            }
            if before.as_ref() != Some(&row) || before_verifier != up.verifier {
                ep.logins.insert(up.name.clone(), row);
                ep.set_verifier(&up.name, up.verifier.as_deref());
                changed.push(up.name.clone());
            }
        }
        for name in &write.removals {
            if owner.as_deref() != Some(name.as_str()) && ep.logins.remove(name).is_some() {
                ep.unreadable.remove(name);
                ep.set_verifier(name, None);
                changed.push(name.clone());
            }
        }
        let databases_changed = ep.branch_databases != write.databases;
        ep.branch_databases = write.databases.clone();
        if !changed.is_empty() || databases_changed {
            ep.roles_version += 1;
        }
        // The data API rows follow the observed databases: new ones start `off`, as `set_databases`.
        let rows = state.db_rows.entry(branch).or_default();
        rows.retain(|r| r.main || write.databases.iter().any(|d| d.name == r.name));
        for d in write.databases.iter().filter(|d| !d.system) {
            if !rows.iter().any(|r| r.name == d.name) {
                rows.push(FakeDbRow::new(&d.name, DataApiState::Off, "public"));
            }
        }
        Ok(ResyncApplied::Applied { changed })
    }

    async fn clear_forwarded(&self, endpoint_id: &str) -> Result<(), MetadataError> {
        record(
            &self.journal,
            format!("metadata.clear_forwarded {endpoint_id}"),
        );
        drop(self.read("clear_forwarded")?);
        Ok(())
    }

    async fn mark_password_unreadable(
        &self,
        endpoint_id: &str,
        name: &str,
    ) -> Result<(), MetadataError> {
        record(
            &self.journal,
            format!("metadata.mark_password_unreadable {endpoint_id} {name}"),
        );
        let mut state = self.read("mark_password_unreadable")?;
        if let Some(ep) = state.endpoints.get_mut(endpoint_id)
            && let Some(login) = ep.logins.get_mut(name)
        {
            login.password_enc = None;
            ep.unreadable.insert(name.to_string());
        }
        Ok(())
    }

    async fn set_compute_settings_version(
        &self,
        endpoint_id: &str,
        version: i32,
    ) -> Result<(), MetadataError> {
        let mut state = self.write("set_compute_settings_version")?;
        if let Some(ep) = state.endpoints.get_mut(endpoint_id) {
            ep.compute_settings_version = version;
        }
        Ok(())
    }

    async fn replace_compute_token(
        &self,
        endpoint_id: &str,
        sha256_hex: &str,
    ) -> Result<(), MetadataError> {
        let mut state = self.write("replace_compute_token")?;
        if let Some(ep) = state.endpoints.get_mut(endpoint_id) {
            ep.endpoint.compute_token_sha256 = sha256_hex.to_string();
        }
        Ok(())
    }

    async fn fail_if_stale(
        &self,
        id: DatabaseId,
        max_age_secs: i64,
    ) -> Result<bool, MetadataError> {
        const TIMED_OUT: &str = "provisioning timed out";
        let mut state = self.write("fail_if_stale")?;
        let age = state.ages.get(&id).copied().unwrap_or(0);
        match state.databases.get_mut(&id) {
            Some(db)
                if matches!(
                    db.status,
                    DatabaseStatus::Pending | DatabaseStatus::Provisioning
                ) && age > max_age_secs =>
            {
                db.status = DatabaseStatus::Failed;
                db.error = Some(TIMED_OUT.to_string());
            }
            _ => return Ok(false),
        }
        state.mirror_on_main(id, BranchStatus::Failed, Some(TIMED_OUT));
        Ok(true)
    }

    async fn list_live_for_reconcile(&self) -> Result<Vec<LiveDatabase>, MetadataError> {
        let state = self.read("list_live_for_reconcile")?;
        let mut rows: Vec<LiveDatabase> = state
            .databases
            .values()
            .filter(|db| db.status != DatabaseStatus::Deleted)
            .map(|db| LiveDatabase {
                id: db.database_id,
                status: db.status,
                endpoint_id: state
                    .branches
                    .get(&BranchId::from(db.database_id))
                    .and_then(|main| main.endpoint_id.clone()),
                neon_tenant_id: db.neon_tenant_id.clone(),
                history_window: db.history_window,
                age_secs: state.ages.get(&db.database_id).copied().unwrap_or(0),
            })
            .collect();
        rows.sort_by_key(|r| r.id.to_string());
        Ok(rows)
    }

    async fn list_endpoints_for_reconcile(&self) -> Result<Vec<EndpointState>, MetadataError> {
        let state = self.read("list_endpoints_for_reconcile")?;
        let mut rows: Vec<EndpointState> = state
            .endpoints
            .values()
            .map(|ep| {
                let branch = state
                    .branches
                    .values()
                    .find(|b| b.endpoint_id.as_deref() == Some(ep.endpoint.id.as_str()));
                EndpointState {
                    id: ep.endpoint.id.clone(),
                    tenant_id: ep.endpoint.tenant_id.clone(),
                    branch_id: branch.map(|b| b.id),
                    branch_status: branch.map(|b| b.status),
                    needs_reconfigure: ep.needs_reconfigure,
                    compute_settings_version: ep.compute_settings_version,
                }
            })
            .collect();
        rows.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(rows)
    }

    async fn set_branch_status(
        &self,
        id: BranchId,
        from: &[BranchStatus],
        to: BranchStatus,
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("set_branch_status")?;
        match state.branches.get_mut(&id) {
            Some(b) if from.contains(&b.status) && branch_live(b.status) => {
                b.status = to;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn set_branch_failed(&self, id: BranchId, error: &str) -> Result<(), MetadataError> {
        let mut state = self.write("set_branch_failed")?;
        if let Some(b) = state.branches.get_mut(&id)
            && branch_live(b.status)
        {
            b.status = BranchStatus::Failed;
            b.error = Some(error.to_string());
        }
        Ok(())
    }

    async fn set_branch_error(
        &self,
        id: BranchId,
        error: Option<&str>,
    ) -> Result<(), MetadataError> {
        let mut state = self.write("set_branch_error")?;
        if let Some(b) = state.branches.get_mut(&id)
            && branch_live(b.status)
        {
            b.error = error.map(str::to_string);
        }
        Ok(())
    }

    async fn begin_branch_delete(
        &self,
        id: BranchId,
    ) -> Result<BranchDeleteOutcome, MetadataError> {
        let mut state = self.write("begin_branch_delete")?;
        if let Some(refused) = state.children_blocking_delete(id) {
            return Ok(refused);
        }
        // Like the real update's `AND NOT protected`.
        if state
            .branches
            .get(&id)
            .is_some_and(|b| b.protected && branch_live(b.status))
        {
            return Ok(BranchDeleteOutcome::Protected);
        }
        Ok(if state.begin_branch_delete(id) {
            BranchDeleteOutcome::Started
        } else {
            BranchDeleteOutcome::NotMoved
        })
    }

    async fn live_children(&self, id: BranchId) -> Result<Vec<BranchRecord>, MetadataError> {
        let state = self.read("live_children")?;
        Ok(sorted(state.branches.values().filter(|b| {
            b.parent_branch_id == Some(id)
                && b.kind == BranchKind::Normal
                && b.status != BranchStatus::Deleted
        })))
    }

    async fn live_backups_of(&self, id: BranchId) -> Result<Vec<BranchRecord>, MetadataError> {
        let state = self.read("live_backups_of")?;
        Ok(sorted(state.branches.values().filter(|b| {
            b.parent_branch_id == Some(id)
                && b.kind == BranchKind::Backup
                && b.status != BranchStatus::Deleted
        })))
    }

    async fn set_pending_timeline(
        &self,
        id: BranchId,
        timeline_id: Option<&str>,
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("set_pending_timeline")?;
        match state.branches.get_mut(&id) {
            Some(b) if branch_live(b.status) => {
                b.pending_timeline_id = timeline_id.map(str::to_string);
                b.pending_timeline_at = timeline_id.map(|_| chrono::Utc::now());
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn switch_to_pending_timeline(
        &self,
        id: BranchId,
        expected_pending: &str,
        history_start: Option<DateTime<Utc>>,
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("switch_to_pending_timeline")?;
        let (endpoint_id, timeline_id) = match state.branches.get_mut(&id) {
            Some(b)
                if b.status == BranchStatus::Resetting
                    && b.pending_timeline_id.as_deref() == Some(expected_pending) =>
            {
                b.superseded_timeline_id = b.timeline_id.take();
                b.timeline_id = b.pending_timeline_id.take();
                b.pending_timeline_at = None;
                b.history_start = Some(history_start.unwrap_or_else(chrono::Utc::now));
                (b.endpoint_id.clone(), b.timeline_id.clone().unwrap())
            }
            _ => return Ok(false),
        };
        state.move_endpoint(endpoint_id.as_deref(), &timeline_id);
        Ok(true)
    }

    async fn switch_to_restored_timeline(
        &self,
        id: BranchId,
        expected_pending: &str,
        backup: BranchId,
        backup_name: &str,
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("switch_to_restored_timeline")?;
        let Some(b) = state.branches.get(&id).cloned() else {
            return Ok(false);
        };
        if b.status != BranchStatus::Resetting
            || b.pending_timeline_id.as_deref() != Some(expected_pending)
        {
            return Ok(false);
        }
        // The real insert fails on the live-name index, and nothing is written.
        if state.name_taken(b.database_id, backup_name) {
            return Err(MetadataError::BranchNameTaken(backup_name.to_string()));
        }
        let now = chrono::Utc::now();
        state.branches.insert(
            backup,
            BranchRecord {
                id: backup,
                database_id: b.database_id,
                name: backup_name.to_string(),
                parent_branch_id: Some(id),
                kind: BranchKind::Backup,
                status: BranchStatus::Ready,
                timeline_id: b.timeline_id.clone(),
                pending_timeline_id: None,
                pending_timeline_at: None,
                superseded_timeline_id: None,
                endpoint_id: None,
                owner_password_enc: None,
                branched_at_time: None,
                branched_at_lsn: None,
                error: None,
                created_at: now,
                updated_at: now,
                history_start: b.history_start,
                authenticator_password_enc: None,
                protected: false,
            },
        );
        let branch = state.branches.get_mut(&id).expect("looked up above");
        branch.timeline_id = branch.pending_timeline_id.take();
        branch.pending_timeline_at = None;
        let timeline_id = branch.timeline_id.clone().unwrap();
        state.move_endpoint(b.endpoint_id.as_deref(), &timeline_id);
        Ok(true)
    }

    async fn clear_superseded_timeline(&self, id: BranchId) -> Result<(), MetadataError> {
        let mut state = self.write("clear_superseded_timeline")?;
        if let Some(b) = state.branches.get_mut(&id) {
            b.superseded_timeline_id = None;
        }
        Ok(())
    }

    async fn swap_pending_timeline(
        &self,
        id: BranchId,
        expected: Option<&str>,
        new: Option<&str>,
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("swap_pending_timeline")?;
        match state.branches.get_mut(&id) {
            Some(b) if branch_live(b.status) && b.pending_timeline_id.as_deref() == expected => {
                b.pending_timeline_id = new.map(str::to_string);
                b.pending_timeline_at = new.map(|_| chrono::Utc::now());
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn claim_stale_reset(
        &self,
        id: BranchId,
        max_age_secs: i64,
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("claim_stale_reset")?;
        if let Some(hook) = state.before_claim_stale_reset.take() {
            hook(&mut state);
        }
        let age = state.branch_ages.get(&id).copied().unwrap_or(0);
        match state.branches.get(&id) {
            Some(b) if b.status == BranchStatus::Resetting && age > max_age_secs => {
                state.branch_ages.insert(id, 0);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn fail_stuck_restore(
        &self,
        id: BranchId,
        max_age_secs: i64,
        error: &str,
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("fail_stuck_restore")?;
        let cutoff = chrono::Utc::now() - chrono::Duration::seconds(max_age_secs);
        match state.branches.get_mut(&id) {
            Some(b)
                if b.status == BranchStatus::Resetting
                    && b.pending_timeline_id.is_some()
                    && b.pending_timeline_at.is_some_and(|at| at < cutoff) =>
            {
                b.status = BranchStatus::Failed;
                b.error = Some(error.to_string());
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn fail_branch_if_stale(
        &self,
        id: BranchId,
        max_age_secs: i64,
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("fail_branch_if_stale")?;
        let age = state.branch_ages.get(&id).copied().unwrap_or(0);
        match state.branches.get_mut(&id) {
            Some(b)
                if b.parent_branch_id.is_some()
                    && matches!(b.status, BranchStatus::Pending | BranchStatus::Provisioning)
                    && age > max_age_secs =>
            {
                b.status = BranchStatus::Failed;
                b.error = Some("provisioning timed out".to_string());
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn database_settings(
        &self,
        db: DatabaseId,
    ) -> Result<Option<DatabaseSettings>, MetadataError> {
        let state = self.read("database_settings")?;
        Ok(state.databases.get(&db).map(|d| DatabaseSettings {
            history_window: d.history_window,
            max_branches: d.max_branches,
        }))
    }

    async fn list_live_branches_for_reconcile(&self) -> Result<Vec<LiveBranch>, MetadataError> {
        let state = self.read("list_live_branches_for_reconcile")?;
        let mut rows: Vec<LiveBranch> = state
            .branches
            .values()
            .filter(|b| b.status != BranchStatus::Deleted)
            .map(|b| LiveBranch {
                id: b.id,
                database_id: b.database_id,
                kind: b.kind,
                status: b.status,
                endpoint_id: b.endpoint_id.clone(),
                timeline_id: b.timeline_id.clone(),
                pending_timeline_id: b.pending_timeline_id.clone(),
                superseded_timeline_id: b.superseded_timeline_id.clone(),
                age_secs: state.branch_ages.get(&b.id).copied().unwrap_or(0),
            })
            .collect();
        rows.sort_by_key(|r| r.id.to_string());
        Ok(rows)
    }

    async fn insert_data_key(&self, key: &NewDataKey) -> Result<(), MetadataError> {
        let mut state = self.write("insert_data_key")?;
        if let Some(hook) = state.before_insert_data_key.take() {
            hook(&mut state);
        }
        if key.kind == DataKeyKind::Public && state.has_live_public_key(key.branch_id) {
            return Err(MetadataError::Conflict);
        }
        state.data_keys.push(key.clone());
        Ok(())
    }

    async fn list_data_keys(&self, branch: BranchId) -> Result<Vec<DataKeyRecord>, MetadataError> {
        let state = self.read("list_data_keys")?;
        Ok(state
            .live_keys(branch)
            .map(|k| DataKeyRecord {
                id: k.id,
                branch_id: k.branch_id,
                kind: k.kind,
                role: k.role.clone(),
                name: k.name.clone(),
                prefix: k.prefix.clone(),
                sealed: k.sealed.clone(),
                created_at: chrono::Utc::now(),
                last_used_at: None,
                revoked_at: None,
                databases: k.databases.clone(),
            })
            .collect())
    }

    async fn branch_databases(
        &self,
        branch: BranchId,
    ) -> Result<Vec<BranchDatabase>, MetadataError> {
        let state = self.read("branch_databases")?;
        let mut rows = state.db_rows.get(&branch).cloned().unwrap_or_default();
        rows.sort_by(|a, b| (!a.main, &a.name).cmp(&(!b.main, &b.name)));
        Ok(rows
            .into_iter()
            .map(|r| BranchDatabase {
                name: r.name,
                owner: "postgres".to_string(),
                origin: if r.main { DatabaseOrigin::Main } else { DatabaseOrigin::Sql },
                data_api: r.state,
                data_api_error: r.error,
                data_api_bootstrapped: r.main || r.state != DataApiState::Off,
                default_schema: r.schema,
            })
            .collect())
    }

    async fn served_databases(
        &self,
        branch: BranchId,
    ) -> Result<Vec<ServedDatabase>, MetadataError> {
        let state = self.read("served_databases")?;
        let mut rows: Vec<FakeDbRow> = state
            .db_rows
            .get(&branch)
            .map(|rows| rows.iter().filter(|r| r.state == DataApiState::On).cloned().collect())
            .unwrap_or_default();
        rows.sort_by(|a, b| (!a.main, &a.name).cmp(&(!b.main, &b.name)));
        Ok(rows
            .into_iter()
            .map(|r| ServedDatabase { name: r.name, default_schema: r.schema })
            .collect())
    }

    async fn finish_enable_data_api(
        &self,
        branch: BranchId,
        name: &str,
        outcome: Result<(), String>,
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("finish_enable_data_api")?;
        if let Some(hook) = state.before_finish_enable.take() {
            hook(&mut state);
        }
        let Some(row) = state
            .db_rows
            .get_mut(&branch)
            .and_then(|rows| rows.iter_mut().find(|r| r.name == name))
            .filter(|r| r.state == DataApiState::Enabling)
        else {
            return Ok(false);
        };
        match outcome {
            Ok(()) => {
                row.state = DataApiState::On;
                row.error = None;
            }
            Err(text) => {
                row.state = DataApiState::Error;
                row.error = Some(text);
            }
        }
        Ok(true)
    }

    async fn set_data_api_off(&self, branch: BranchId, name: &str) -> Result<bool, MetadataError> {
        let mut state = self.write("set_data_api_off")?;
        let Some(row) = state
            .db_rows
            .get_mut(&branch)
            .and_then(|rows| rows.iter_mut().find(|r| r.name == name))
            .filter(|r| !r.main)
        else {
            return Ok(false);
        };
        row.state = DataApiState::Off;
        row.error = None;
        Ok(true)
    }

    async fn copy_parent_database_flags(&self, child: BranchId) -> Result<(), MetadataError> {
        let mut state = self.write("copy_parent_database_flags")?;
        let Some(parent) = state.branches.get(&child).and_then(|b| b.parent_branch_id) else {
            return Ok(());
        };
        let parent_rows = state.db_rows.get(&parent).cloned().unwrap_or_default();
        for row in state.db_rows.entry(child).or_default() {
            if let Some(p) = parent_rows.iter().find(|p| p.name == row.name) {
                row.state = if p.state == DataApiState::On { DataApiState::On } else { DataApiState::Off };
                row.error = None;
                row.schema = p.schema.clone();
            }
        }
        Ok(())
    }

    async fn enabling_databases(&self) -> Result<Vec<(BranchId, String)>, MetadataError> {
        let state = self.read("enabling_databases")?;
        let mut out: Vec<(BranchId, String)> = state
            .db_rows
            .iter()
            .flat_map(|(b, rows)| {
                rows.iter()
                    .filter(|r| r.state == DataApiState::Enabling)
                    .map(|r| (*b, r.name.clone()))
            })
            .collect();
        out.sort_by_key(|(b, n)| (b.as_uuid(), n.clone()));
        Ok(out)
    }

    async fn has_live_public_key(&self, branch: BranchId) -> Result<bool, MetadataError> {
        Ok(self
            .read("has_live_public_key")?
            .has_live_public_key(branch))
    }

    async fn set_authenticator_password(
        &self,
        branch: BranchId,
        sealed: &str,
    ) -> Result<(), MetadataError> {
        let mut state = self.write("set_authenticator_password")?;
        if let Some(hook) = state.before_set_authenticator_password.take() {
            hook(&mut state);
        }
        if let Some(b) = state.branches.get_mut(&branch)
            && branch_live(b.status)
            && b.authenticator_password_enc.is_none()
        {
            b.authenticator_password_enc = Some(sealed.to_string());
        }
        Ok(())
    }

    async fn branches_missing_data_api(&self) -> Result<Vec<BranchId>, MetadataError> {
        let state = self.read("branches_missing_data_api")?;
        Ok(sorted(state.branches.values().filter(|b| {
            b.status == BranchStatus::Ready
                && b.kind == BranchKind::Normal
                && (b.authenticator_password_enc.is_none() || !state.has_live_public_key(b.id))
        }))
        .into_iter()
        .map(|b| b.id)
        .collect())
    }

    async fn branch_connection(
        &self,
        branch: BranchId,
    ) -> Result<Option<BranchConnectionRow>, MetadataError> {
        let state = self.read("branch_connection")?;
        Ok(state.branches.get(&branch).and_then(|b| {
            let db = state.databases.get(&b.database_id)?;
            Some(BranchConnectionRow {
                branch_id: b.id,
                status: b.status,
                endpoint_id: b.endpoint_id.clone(),
                database_name: db.database_name.clone(),
                authenticator_password_enc: b.authenticator_password_enc.clone(),
                settings: DataApiSettings {
                    max_rows: db.max_rows,
                    statement_timeout_ms: db.statement_timeout_ms,
                    max_connections: db.max_connections,
                },
                databases: vec![cheladb_metadata::ServedDatabase {
                    name: db.database_name.clone().unwrap_or_else(|| "db".into()),
                    default_schema: "public".into(),
                }],
            })
        }))
    }

    async fn deleting_projects(&self, older_than: Duration) -> Result<Vec<ProjectId>, MetadataError> {
        let state = self.read("deleting_projects")?;
        let cutoff = Utc::now() - older_than;
        let mut found: Vec<&ProjectRecord> = state
            .projects
            .values()
            .filter(|p| p.status == ProjectStatus::Deleting && p.updated_at < cutoff)
            .collect();
        found.sort_by_key(|p| p.updated_at);
        Ok(found.into_iter().map(|p| p.id).collect())
    }

    async fn app_users_in_transition(
        &self,
        older_than: Duration,
    ) -> Result<Vec<ProjectAuth>, MetadataError> {
        let state = self.read("app_users_in_transition")?;
        let cutoff = Utc::now() - older_than;
        let mut found: Vec<ProjectAuth> = state
            .project_auth
            .values()
            .filter(|a| {
                matches!(a.status, AppUsersStatus::Enabling | AppUsersStatus::Disabling)
                    && a.updated_at < cutoff
            })
            .cloned()
            .collect();
        found.sort_by_key(|a| a.updated_at);
        Ok(found)
    }

    async fn project_by_id(&self, id: ProjectId) -> Result<Option<ProjectRecord>, MetadataError> {
        Ok(self.read("project_by_id")?.projects.get(&id).cloned())
    }

    async fn project_databases(
        &self,
        id: ProjectId,
    ) -> Result<Vec<(DatabaseId, DatabaseStatus, Option<String>)>, MetadataError> {
        let state = self.read("project_databases")?;
        let mut found: Vec<&DatabaseRecord> = state
            .databases
            .values()
            .filter(|d| d.project_id == id && d.status != DatabaseStatus::Deleted)
            .collect();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(found
            .into_iter()
            .map(|d| (d.database_id, d.status, d.error.clone()))
            .collect())
    }

    async fn set_project_error(&self, id: ProjectId, error: Option<&str>) -> Result<(), MetadataError> {
        let mut state = self.write("set_project_error")?;
        if let Some(p) = state.projects.get_mut(&id) {
            p.error = error.map(str::to_string);
            p.updated_at = Utc::now();
        }
        Ok(())
    }

    async fn finish_project_delete(&self, id: ProjectId) -> Result<(), MetadataError> {
        let mut state = self.write("finish_project_delete")?;
        let now = Utc::now();
        for s in state.app_sessions.iter_mut() {
            if s.project_id == id && s.revoked_at.is_none() {
                s.revoked_at = Some(now);
            }
        }
        let dbs: HashSet<DatabaseId> = state
            .databases
            .values()
            .filter(|d| d.project_id == id)
            .map(|d| d.database_id)
            .collect();
        let branches: HashSet<BranchId> = state
            .branches
            .values()
            .filter(|b| dbs.contains(&b.database_id))
            .map(|b| b.id)
            .collect();
        state.auth_sync.retain(|b, _| !branches.contains(b));
        state.project_auth.remove(&id);
        if let Some(p) = state.projects.get_mut(&id) {
            p.status = ProjectStatus::Deleted;
            p.error = None;
            p.updated_at = now;
        }
        Ok(())
    }

    async fn project_auth(&self, project: ProjectId) -> Result<Option<ProjectAuth>, MetadataError> {
        Ok(self.read("project_auth")?.project_auth.get(&project).cloned())
    }

    async fn app_auth_for_database(
        &self,
        db: DatabaseId,
    ) -> Result<Option<ProjectAuth>, MetadataError> {
        Ok(self
            .read("app_auth_for_database")?
            .project_auth
            .values()
            .find(|a| a.database_id == db)
            .cloned())
    }

    async fn set_app_users_org(&self, project: ProjectId, org_id: &str) -> Result<(), MetadataError> {
        let mut state = self.write("set_app_users_org")?;
        if state
            .project_auth
            .values()
            .any(|a| a.project_id != project && a.zitadel_org_id.as_deref() == Some(org_id))
        {
            return Err(MetadataError::Conflict);
        }
        if let Some(a) = state.project_auth.get_mut(&project) {
            a.zitadel_org_id = Some(org_id.to_string());
            a.updated_at = Utc::now();
        }
        Ok(())
    }

    async fn finish_enable_app_users(
        &self,
        project: ProjectId,
        outcome: Result<(), String>,
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("finish_enable_app_users")?;
        match state.project_auth.get_mut(&project) {
            Some(a) if a.status == AppUsersStatus::Enabling => {
                match outcome {
                    Ok(()) => {
                        a.status = AppUsersStatus::Enabled;
                        a.error = None;
                    }
                    Err(e) => {
                        a.status = AppUsersStatus::Failed;
                        a.error = Some(e);
                    }
                }
                a.updated_at = Utc::now();
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn delete_project_auth(&self, project: ProjectId) -> Result<(), MetadataError> {
        self.write("delete_project_auth")?.project_auth.remove(&project);
        Ok(())
    }

    async fn enabled_app_projects(&self) -> Result<Vec<ProjectAuth>, MetadataError> {
        let state = self.read("enabled_app_projects")?;
        let mut found: Vec<ProjectAuth> = state
            .project_auth
            .values()
            .filter(|a| a.status == AppUsersStatus::Enabled)
            .cloned()
            .collect();
        found.sort_by_key(|a| a.created_at);
        Ok(found)
    }

    async fn branch_auth_sync(&self, branch: BranchId) -> Result<Option<BranchAuthSync>, MetadataError> {
        Ok(self.read("branch_auth_sync")?.auth_sync.get(&branch).cloned())
    }

    async fn set_branch_auth_sync(
        &self,
        branch: BranchId,
        pg_database: &str,
        sealed: &str,
    ) -> Result<(), MetadataError> {
        self.write("set_branch_auth_sync")?.auth_sync.insert(
            branch,
            BranchAuthSync {
                branch_id: branch,
                pg_database: pg_database.to_string(),
                password_sealed: sealed.to_string(),
            },
        );
        Ok(())
    }

    async fn claim_branch_auth_sync(
        &self,
        branch: BranchId,
        pg_database: &str,
        sealed: &str,
    ) -> Result<String, MetadataError> {
        let mut state = self.write("claim_branch_auth_sync")?;
        if let Some((db, racer)) = state.auth_sync_racer.take() {
            state.auth_sync.insert(
                branch,
                BranchAuthSync {
                    branch_id: branch,
                    pg_database: db,
                    password_sealed: racer,
                },
            );
        }
        let row = state.auth_sync.entry(branch).or_insert_with(|| BranchAuthSync {
            branch_id: branch,
            pg_database: pg_database.to_string(),
            password_sealed: sealed.to_string(),
        });
        if row.pg_database != pg_database {
            row.pg_database = pg_database.to_string();
            row.password_sealed = sealed.to_string();
        }
        Ok(row.password_sealed.clone())
    }

    async fn swap_branch_auth_sync(
        &self,
        branch: BranchId,
        pg_database: &str,
        expected: &str,
        sealed: &str,
    ) -> Result<String, MetadataError> {
        let mut state = self.write("swap_branch_auth_sync")?;
        if let Some((db, racer)) = state.auth_sync_racer.take() {
            state.auth_sync.insert(
                branch,
                BranchAuthSync {
                    branch_id: branch,
                    pg_database: db,
                    password_sealed: racer,
                },
            );
        }
        match state.auth_sync.get_mut(&branch) {
            Some(row) if row.pg_database == pg_database && row.password_sealed != expected => {
                Ok(row.password_sealed.clone())
            }
            _ => {
                state.auth_sync.insert(
                    branch,
                    BranchAuthSync {
                        branch_id: branch,
                        pg_database: pg_database.to_string(),
                        password_sealed: sealed.to_string(),
                    },
                );
                Ok(sealed.to_string())
            }
        }
    }

    async fn revoke_user_sessions(
        &self,
        project: ProjectId,
        user: uuid::Uuid,
        keep: Option<uuid::Uuid>,
        created_before: Option<DateTime<Utc>>,
    ) -> Result<Vec<AppSession>, MetadataError> {
        let mut state = self.write("revoke_user_sessions")?;
        let now = Utc::now();
        let mut revoked = Vec::new();
        for s in state.app_sessions.iter_mut() {
            if s.project_id == project
                && s.user_id == user
                && s.revoked_at.is_none()
                && keep != Some(s.id)
                && created_before.is_none_or(|t| s.created_at < t)
            {
                s.revoked_at = Some(now);
                revoked.push(s.clone());
            }
        }
        Ok(revoked)
    }

    async fn revoke_project_sessions(&self, project: ProjectId) -> Result<Vec<AppSession>, MetadataError> {
        let mut state = self.write("revoke_project_sessions")?;
        let now = Utc::now();
        let mut revoked = Vec::new();
        for s in state.app_sessions.iter_mut() {
            if s.project_id == project && s.revoked_at.is_none() {
                s.revoked_at = Some(now);
                revoked.push(s.clone());
            }
        }
        Ok(revoked)
    }

    async fn purge_app_sessions(&self, older_than: Duration) -> Result<u64, MetadataError> {
        let mut state = self.write("purge_app_sessions")?;
        let cutoff = Utc::now() - older_than;
        let before = state.app_sessions.len();
        state
            .app_sessions
            .retain(|s| !(s.revoked_at.is_some_and(|r| r < cutoff) || s.expires_at < cutoff));
        Ok((before - state.app_sessions.len()) as u64)
    }
}

/// Oldest first, like the real queries.
/// The endpoint's rows as `role_copies` reads them: the owner's first, then by name.
fn fake_copies(ep: &FakeEndpoint) -> Vec<RoleCopy> {
    let at = DateTime::<Utc>::UNIX_EPOCH;
    let owner = ep.roles.first().map(|o| RoleCopy {
        name: o.name.clone(),
        origin: RoleOrigin::Owner,
        scram_verifier: Some(o.scram_verifier.clone()),
        password_enc: None,
        login: true,
        level: RoleLevel::OwnerLevel,
        api_owner_level: false,
        pending: false,
        created_at: at,
    });
    let mut rest: Vec<RoleCopy> = ep
        .logins
        .iter()
        .map(|(name, l)| RoleCopy {
            name: name.clone(),
            origin: l.origin,
            scram_verifier: ep.verifier(name),
            password_enc: l.password_enc.clone(),
            login: l.login,
            level: l.level,
            api_owner_level: l.api_owner_level,
            pending: l.pending,
            created_at: at,
        })
        .collect();
    rest.sort_by(|a, b| a.name.cmp(&b.name));
    owner.into_iter().chain(rest).collect()
}

fn sorted<'a>(branches: impl Iterator<Item = &'a BranchRecord>) -> Vec<BranchRecord> {
    let mut branches: Vec<BranchRecord> = branches.cloned().collect();
    branches.sort_by_key(|b| (b.created_at, b.id.to_string()));
    branches
}

/// The cause under every injected storage failure.
/// How the fake's `detach_ancestor` ends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FakeDetach {
    #[default]
    Works,
    /// The request times out with the detach still running (the ancestor stays).
    TimesOut,
    /// The request times out, but the detach finished just before.
    TimesOutAfterFinishing,
}

pub const STORAGE_CAUSE: &str = "connection refused";
/// The cause a 503 from [`FakeStorage::unavailable`] carries.
pub const UNAVAILABLE_CAUSE: &str = "shard is reconfiguring";

/// A timeline in memory: what it branched from, and its head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeTimeline {
    /// `(parent timeline, LSN)`; `None` for a fresh timeline or once detached.
    pub ancestor: Option<(NeonId, Lsn)>,
    pub head: Lsn,
}

/// The head of every fresh timeline.
pub const FRESH_HEAD: Lsn = Lsn(0x0200_0000);

/// Tenants and timelines in memory.
pub struct FakeStorage {
    pub tenants: Mutex<HashSet<NeonId>>,
    /// Per `(tenant, timeline)`.
    pub timelines: Mutex<HashMap<(NeonId, NeonId), FakeTimeline>>,
    /// Each tenant's `pitr_interval`, once patched.
    pub pitr: Mutex<HashMap<NeonId, Duration>>,
    /// The method that fails, e.g. `"create_timeline"`.
    pub fail: Mutex<Option<&'static str>>,
    /// Methods that answer 503 (a transient failure), each the given number of times.
    pub unavailable: Mutex<HashMap<&'static str, u32>>,
    /// The method the storage controller refuses (400).
    pub reject: Mutex<Option<&'static str>>,
    /// How `detach_ancestor` ends.
    pub detach: Mutex<FakeDetach>,
    /// How long `detach_ancestor` takes.
    pub detach_delay: Mutex<Duration>,
    /// Runs once, at the start of the next `create_tenant` (e.g. to land a delete).
    pub on_create_tenant: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs once, at the start of the next `create_branch_timeline`.
    pub on_create_branch_timeline: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// What `lsn_by_timestamp` answers; `None` answers `present` at the timeline's head.
    pub lsn_at: Mutex<Option<LsnAtTime>>,
    /// What `timestamp_of_lsn` answers.
    pub timestamp_at: Mutex<Option<DateTime<Utc>>>,
    pub journal: Journal,
}

impl FakeStorage {
    fn call(&self, method: &'static str, args: String) -> Result<(), StorconError> {
        let entry = if args.is_empty() {
            format!("storage.{method}")
        } else {
            format!("storage.{method} {args}")
        };
        record(&self.journal, entry);
        self.check(method)
    }

    /// Fails when `method` is the one set to fail; not journaled (reads).
    fn check(&self, method: &'static str) -> Result<(), StorconError> {
        if let Some(left) = lock(&self.unavailable).get_mut(method)
            && *left > 0
        {
            *left -= 1;
            // What `check` in cheladb-neon makes of a 5xx.
            return Err(StorconError::Other(anyhow::anyhow!(
                "storage controller returned 503 Service Unavailable: {UNAVAILABLE_CAUSE}"
            )));
        }
        if *lock(&self.reject) == Some(method) {
            return Err(StorconError::Rejected {
                status: reqwest::StatusCode::BAD_REQUEST,
                message: format!("{method} refused"),
            });
        }
        if *lock(&self.fail) == Some(method) {
            // With a cause, so tests see whether the whole chain is kept.
            return Err(StorconError::Other(
                anyhow::anyhow!("{STORAGE_CAUSE}").context(format!("injected {method} failure")),
            ));
        }
        Ok(())
    }

    fn not_found(what: String) -> StorconError {
        StorconError::Rejected {
            status: reqwest::StatusCode::NOT_FOUND,
            message: format!("{what} not found"),
        }
    }

    pub fn has_tenant(&self, tenant: &str) -> bool {
        lock(&self.tenants).contains(&tenant.parse::<NeonId>().unwrap())
    }

    pub fn timeline(&self, tenant: &str, timeline: &str) -> Option<FakeTimeline> {
        let key = (tenant.parse().unwrap(), timeline.parse().unwrap());
        lock(&self.timelines).get(&key).cloned()
    }

    /// Every timeline id, in any tenant.
    pub fn timeline_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = lock(&self.timelines)
            .keys()
            .map(|(_, t)| t.as_str().to_string())
            .collect();
        ids.sort();
        ids
    }

    /// A timeline that exists behind the metadata's back (e.g. main's, made by a provision).
    pub fn insert_timeline(&self, tenant: &str, timeline: &str, head: Lsn) {
        let tenant: NeonId = tenant.parse().unwrap();
        lock(&self.tenants).insert(tenant.clone());
        lock(&self.timelines).insert(
            (tenant, timeline.parse().unwrap()),
            FakeTimeline {
                ancestor: None,
                head,
            },
        );
    }

    fn head(&self, tenant: &NeonId, timeline: &NeonId) -> Result<Lsn, StorconError> {
        lock(&self.timelines)
            .get(&(tenant.clone(), timeline.clone()))
            .map(|t| t.head)
            .ok_or_else(|| Self::not_found(format!("timeline {timeline}")))
    }
}

impl Placement for FakeStorage {
    async fn pageserver_connstring(&self, _tenant: &NeonId) -> Result<String, StorconError> {
        Ok("postgresql://no_user@pageserver:6400".to_string())
    }

    async fn active_safekeepers(&self) -> Result<Vec<String>, StorconError> {
        Ok(vec!["safekeeper1:5454".to_string()])
    }
}

impl Storage for FakeStorage {
    async fn create_tenant(&self, tenant: &NeonId) -> Result<(), StorconError> {
        let hook = lock(&self.on_create_tenant).take();
        if let Some(hook) = hook {
            hook();
        }
        self.call("create_tenant", String::new())?;
        lock(&self.tenants).insert(tenant.clone());
        Ok(())
    }

    async fn create_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<(), StorconError> {
        self.call("create_timeline", String::new())?;
        lock(&self.timelines).insert(
            (tenant.clone(), timeline.clone()),
            FakeTimeline {
                ancestor: None,
                head: FRESH_HEAD,
            },
        );
        Ok(())
    }

    async fn delete_tenant(&self, tenant: &NeonId) -> Result<(), StorconError> {
        self.call("delete_tenant", tenant.to_string())?;
        // A tenant that's already gone counts as deleted, like the controller's 404.
        lock(&self.tenants).remove(tenant);
        lock(&self.timelines).retain(|(t, _), _| t != tenant);
        Ok(())
    }

    async fn list_tenant_ids(&self) -> Result<Vec<NeonId>, StorconError> {
        if *lock(&self.fail) == Some("list_tenant_ids") {
            return Err(StorconError::Other(anyhow::anyhow!(
                "injected list_tenant_ids failure"
            )));
        }
        let mut ids: Vec<NeonId> = lock(&self.tenants).iter().cloned().collect();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(ids)
    }

    async fn create_branch_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
        parent: &NeonId,
        lsn: Lsn,
    ) -> Result<(), StorconError> {
        let hook = lock(&self.on_create_branch_timeline).take();
        if let Some(hook) = hook {
            hook();
        }
        self.call(
            "create_branch_timeline",
            format!("{timeline} from {parent} at {lsn}"),
        )?;
        if !lock(&self.tenants).contains(tenant) {
            return Err(Self::not_found(format!("tenant {tenant}")));
        }
        let parent_head = self.head(tenant, parent)?;
        // Like the pageserver: a branch point must already be written on the parent.
        if lsn > parent_head {
            return Err(StorconError::Rejected {
                status: reqwest::StatusCode::BAD_REQUEST,
                message: format!(
                    "invalid branch start lsn: {lsn} is ahead of the ancestor's last record lsn \
                     {parent_head}"
                ),
            });
        }
        lock(&self.timelines).insert(
            (tenant.clone(), timeline.clone()),
            FakeTimeline {
                ancestor: Some((parent.clone(), lsn)),
                head: lsn,
            },
        );
        Ok(())
    }

    async fn delete_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<(), StorconError> {
        self.call("delete_timeline", timeline.to_string())?;
        let mut timelines = lock(&self.timelines);
        // Like Neon: a timeline that others branch from can't go (412) until they do.
        let has_children = timelines.iter().any(|((t, _), tl)| {
            t == tenant && tl.ancestor.as_ref().is_some_and(|(a, _)| a == timeline)
        });
        if has_children {
            return Err(StorconError::Rejected {
                status: reqwest::StatusCode::PRECONDITION_FAILED,
                message: format!("timeline {timeline} has child timelines"),
            });
        }
        // Already gone (404) counts as deleted.
        timelines.remove(&(tenant.clone(), timeline.clone()));
        Ok(())
    }

    async fn get_timeline(&self, tenant: &NeonId, timeline: &NeonId) -> Result<Lsn, StorconError> {
        self.check("get_timeline")?;
        self.head(tenant, timeline)
    }

    async fn ancestor_lsn(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<Option<Lsn>, StorconError> {
        self.check("ancestor_lsn")?;
        match lock(&self.timelines).get(&(tenant.clone(), timeline.clone())) {
            Some(t) => Ok(t.ancestor.as_ref().map(|(_, lsn)| *lsn)),
            None => Err(Self::not_found(format!("timeline {timeline}"))),
        }
    }

    async fn lsn_by_timestamp(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
        _at: DateTime<Utc>,
    ) -> Result<LsnAtTime, StorconError> {
        self.check("lsn_by_timestamp")?;
        let head = self.head(tenant, timeline)?;
        Ok(lock(&self.lsn_at).clone().unwrap_or(LsnAtTime {
            lsn: head,
            kind: LsnKind::Present,
        }))
    }

    async fn timestamp_of_lsn(
        &self,
        _tenant: &NeonId,
        _timeline: &NeonId,
        _lsn: Lsn,
    ) -> Result<Option<DateTime<Utc>>, StorconError> {
        self.check("timestamp_of_lsn")?;
        Ok(*lock(&self.timestamp_at))
    }

    async fn detach_ancestor(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<(), StorconError> {
        self.call("detach_ancestor", timeline.to_string())?;
        let delay = *lock(&self.detach_delay);
        tokio::time::sleep(delay).await;
        let behavior = *lock(&self.detach);
        let timed_out = || {
            StorconError::Other(
                anyhow::anyhow!("operation timed out").context("error sending request"),
            )
        };
        if behavior == FakeDetach::TimesOut {
            return Err(timed_out());
        }
        match lock(&self.timelines).get_mut(&(tenant.clone(), timeline.clone())) {
            Some(t) => t.ancestor = None,
            None => return Err(Self::not_found(format!("timeline {timeline}"))),
        }
        match behavior {
            FakeDetach::TimesOutAfterFinishing => Err(timed_out()),
            _ => Ok(()),
        }
    }

    async fn patch_tenant_config(
        &self,
        tenant: &NeonId,
        pitr_interval: Duration,
    ) -> Result<(), StorconError> {
        self.call(
            "patch_tenant_config",
            format!("{tenant} {}s", pitr_interval.as_secs()),
        )?;
        lock(&self.pitr).insert(tenant.clone(), pitr_interval);
        Ok(())
    }

    async fn tenant_pitr_interval(
        &self,
        tenant: &NeonId,
    ) -> Result<Option<Duration>, StorconError> {
        self.check("tenant_pitr_interval")?;
        Ok(lock(&self.pitr).get(tenant).copied())
    }

    async fn list_timelines(&self, tenant: &NeonId) -> Result<Vec<NeonId>, StorconError> {
        self.check("list_timelines")?;
        let mut ids: Vec<NeonId> = lock(&self.timelines)
            .keys()
            .filter(|(t, _)| t == tenant)
            .map(|(_, tl)| tl.clone())
            .collect();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        Ok(ids)
    }
}

pub struct FakeCache {
    pub receivers: Mutex<u64>,
    pub fail: Mutex<Option<String>>,
    pub journal: Journal,
}

impl CacheNotifier for FakeCache {
    async fn password_updated(&self, endpoint_id: &str, role: &str) -> anyhow::Result<u64> {
        record(
            &self.journal,
            format!("cache.password_updated {endpoint_id} {role}"),
        );
        if let Some(message) = lock(&self.fail).clone() {
            anyhow::bail!(message);
        }
        Ok(*lock(&self.receivers))
    }

    /// Journals `cache.password_updated_later` at once, then each `password_updated` after
    /// `delay` from a spawned task (tests advance a paused clock).
    fn password_updated_later(&self, endpoint_id: &str, roles: Vec<String>, delay: Duration) {
        let journal = self.journal.clone();
        let endpoint_id = endpoint_id.to_string();
        record(
            &journal,
            format!(
                "cache.password_updated_later {endpoint_id} {}",
                roles.join(",")
            ),
        );
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            for role in roles {
                record(
                    &journal,
                    format!("cache.password_updated {endpoint_id} {role}"),
                );
            }
        });
    }
}

type OnPush = Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>;

pub struct FakePusher {
    pub fail: Mutex<Option<String>>,
    /// Runs once, inside the next push (e.g. to land a concurrent reset); the push waits for
    /// its future.
    pub on_push: Mutex<Option<OnPush>>,
    /// `(endpoint, config)` of every push that got through.
    pub pushed: Mutex<Vec<(String, serde_json::Value)>>,
    pub journal: Journal,
}

impl SpecPusher for FakePusher {
    fn jwks(&self) -> serde_json::Value {
        serde_json::json!({ "keys": [] })
    }

    async fn push(
        &self,
        _control_addr: SocketAddr,
        endpoint_id: &str,
        config: &serde_json::Value,
    ) -> anyhow::Result<()> {
        record(&self.journal, format!("pusher.push {endpoint_id}"));
        let hook = lock(&self.on_push).take();
        if let Some(hook) = hook {
            hook().await;
        }
        if let Some(message) = lock(&self.fail).clone() {
            anyhow::bail!(message);
        }
        lock(&self.pushed).push((endpoint_id.to_string(), config.clone()));
        Ok(())
    }
}

pub type FakeWorker = Worker<FakeMetadata, FakeStorage, FakeBackend, FakeCache, FakePusher>;

/// A worker over fresh fakes sharing one journal, with millisecond readiness polling.
pub fn worker() -> (FakeWorker, Journal) {
    let journal = Journal::default();
    let worker = Worker {
        metadata: FakeMetadata {
            state: Arc::default(),
            journal: journal.clone(),
            locks: Arc::default(),
        },
        storage: FakeStorage {
            tenants: Mutex::default(),
            timelines: Mutex::default(),
            pitr: Mutex::default(),
            fail: Mutex::default(),
            unavailable: Mutex::default(),
            reject: Mutex::default(),
            detach: Mutex::default(),
            detach_delay: Mutex::default(),
            on_create_tenant: Mutex::default(),
            on_create_branch_timeline: Mutex::default(),
            lsn_at: Mutex::default(),
            timestamp_at: Mutex::default(),
            journal: journal.clone(),
        },
        computes: FakeBackend::with_journal(journal.clone()),
        cache: FakeCache {
            receivers: Mutex::new(1),
            fail: Mutex::default(),
            journal: journal.clone(),
        },
        pusher: FakePusher {
            fail: Mutex::default(),
            on_push: Mutex::default(),
            pushed: Mutex::default(),
            journal: journal.clone(),
        },
        key: SecretKey::generate(),
        owner_superuser: false,
        roles: cheladb_types::role_settings::RoleSettings {
            role_cap: 10,
            database_cap: 20,
            strength: false,
            prehashed_allowed: true,
        },
        control_plane_uri: "http://host.docker.internal:8083".to_string(),
        ready_poll: Duration::from_millis(1),
        ready_timeout: Duration::from_millis(20),
        claim_refresh: Duration::from_millis(5),
        schema_check_cursor: Mutex::default(),
        enabling_seen: Mutex::default(),
        zitadel: None,
        app_users_reconcile_every: Duration::from_secs(300),
        remove_orphans: true,
    };
    (worker, journal)
}

/// A ready database named `app`, provisioned through the fakes.
pub async fn ready_database(w: &FakeWorker) -> DatabaseId {
    let id = w.metadata.insert_database("app", DatabaseStatus::Pending);
    w.handle_provision(id, "app").await.unwrap();
    assert_eq!(w.metadata.database(id).status, DatabaseStatus::Ready);
    id
}

/// A pending branch of `db` under `parent`, as the API inserts it.
pub fn pending_branch(w: &FakeWorker, db: DatabaseId, name: &str, parent: BranchId) -> BranchId {
    let id = BranchId::new();
    let outcome = w.metadata.create_branch(&NewBranch {
        id,
        database_id: db,
        name,
        parent_branch_id: parent,
        branched_at_time: None,
        branched_at_lsn: None,
    });
    assert_eq!(outcome, CreateBranchOutcome::Created);
    id
}

/// A ready branch of `db` under `parent`, built through the fakes at the parent's head.
pub async fn ready_branch(
    w: &FakeWorker,
    db: DatabaseId,
    name: &str,
    parent: BranchId,
) -> BranchId {
    let id = pending_branch(w, db, name, parent);
    w.handle_branch_create(id, None).await.unwrap();
    let branch = w.metadata.branch(id);
    assert_eq!(branch.status, BranchStatus::Ready, "{:?}", branch.error);
    id
}

/// What a restore of `branch` leaves (without its compute work): a new timeline for the
/// branch (and its endpoint), and a ready backup holding the old one. Returns the backup.
pub fn restored_by_hand(w: &FakeWorker, branch: BranchId, backup_name: &str) -> BranchId {
    let b = w.metadata.branch(branch);
    let tenant = w
        .metadata
        .database(b.database_id)
        .neon_tenant_id
        .expect("the database has a tenant");
    let restored = NeonId::new();
    w.storage
        .insert_timeline(&tenant, restored.as_str(), FRESH_HEAD);
    let backup = BranchId::new();
    let mut state = w.metadata.state();
    let row = state.branches.get_mut(&branch).unwrap();
    row.status = BranchStatus::Resetting;
    row.pending_timeline_id = Some(restored.as_str().to_string());
    drop(state);
    let switched = futures_util::FutureExt::now_or_never(w.metadata.switch_to_restored_timeline(
        branch,
        restored.as_str(),
        backup,
        backup_name,
    ))
    .expect("the fake never waits");
    assert!(switched.unwrap(), "switched");
    w.metadata.state().branches.get_mut(&branch).unwrap().status = BranchStatus::Ready;
    backup
}

#[cfg(test)]
mod tests {
    use super::*;

    fn storage() -> (FakeStorage, NeonId, NeonId) {
        let (w, _journal) = worker();
        let (tenant, root) = (NeonId::new(), NeonId::new());
        w.storage
            .insert_timeline(tenant.as_str(), root.as_str(), FRESH_HEAD);
        (w.storage, tenant, root)
    }

    fn status_of(err: StorconError) -> reqwest::StatusCode {
        match err {
            StorconError::Rejected { status, .. } => status,
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    // Neon refuses to delete a timeline that other timelines branch from (412).
    #[tokio::test]
    async fn deleting_an_ancestor_is_refused_until_its_children_are_gone() {
        let (s, tenant, root) = storage();
        let child = NeonId::new();
        s.create_branch_timeline(&tenant, &child, &root, FRESH_HEAD)
            .await
            .unwrap();

        let err = s.delete_timeline(&tenant, &root).await.unwrap_err();
        assert!(err.to_string().contains("has child timelines"), "{err}");
        assert_eq!(status_of(err), reqwest::StatusCode::PRECONDITION_FAILED);
        assert!(s.timeline(tenant.as_str(), root.as_str()).is_some());

        s.delete_timeline(&tenant, &child).await.unwrap();
        s.delete_timeline(&tenant, &root).await.unwrap();
        assert!(s.timeline_ids().is_empty());
    }

    // A detached timeline no longer holds its ancestor.
    #[tokio::test]
    async fn a_detached_child_no_longer_blocks_its_ancestor() {
        let (s, tenant, root) = storage();
        let child = NeonId::new();
        s.create_branch_timeline(&tenant, &child, &root, FRESH_HEAD)
            .await
            .unwrap();
        s.detach_ancestor(&tenant, &child).await.unwrap();
        assert_eq!(
            s.timeline(tenant.as_str(), child.as_str())
                .unwrap()
                .ancestor,
            None
        );
        s.delete_timeline(&tenant, &root).await.unwrap();
        assert_eq!(s.timeline_ids(), [child.as_str().to_string()]);
    }

    #[tokio::test]
    async fn branching_ahead_of_the_parents_head_is_refused() {
        let (s, tenant, root) = storage();
        let child = NeonId::new();
        let ahead = Lsn(FRESH_HEAD.0 + 1);
        let err = s
            .create_branch_timeline(&tenant, &child, &root, ahead)
            .await
            .unwrap_err();
        assert_eq!(status_of(err), reqwest::StatusCode::BAD_REQUEST);
        assert!(s.timeline(tenant.as_str(), child.as_str()).is_none());
        // At the head itself is fine.
        s.create_branch_timeline(&tenant, &child, &root, FRESH_HEAD)
            .await
            .unwrap();
    }
}
