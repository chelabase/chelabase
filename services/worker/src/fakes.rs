//! In-memory ports for the worker's tests. Every call that changes something is journaled in
//! one shared [`Journal`], so tests can assert on the order across ports.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use cheladb_metadata::{
    BranchConnectionRow, BranchDeleteOutcome, BranchKind, BranchRecord, BranchStatus,
    CreateBranchOutcome, DataApiSettings, DataKeyRecord, DatabaseRecord, DatabaseSettings,
    DatabaseStatus, ENDPOINT_LOCK_TIMEOUT, EndpointState, LiveBranch, LiveDatabase, MetadataError,
    NeonDatabase, NeonEndpoint, NeonRole, NewBranch, NewDataKey, NewEndpoint,
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

pub struct FakeEndpoint {
    pub endpoint: NeonEndpoint,
    pub roles: Vec<NeonRole>,
    pub databases: Vec<NeonDatabase>,
    pub needs_reconfigure: bool,
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
    /// Data API keys; live unless their id is in `revoked_keys`.
    pub data_keys: Vec<NewDataKey>,
    pub revoked_keys: HashSet<uuid::Uuid>,
    /// Runs once, just before the next `insert_data_key`: a concurrent run's insert.
    pub before_insert_data_key: Option<StateHook>,
    /// Runs once, just before the next `set_authenticator_password`: a concurrent run's write.
    pub before_set_authenticator_password: Option<StateHook>,
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
                default_schema: "public".into(),
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
            },
        );
        CreateBranchOutcome::Created
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
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("create_endpoint_for_branch")?;
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
        state.endpoints.insert(
            ep.id.to_string(),
            FakeEndpoint {
                endpoint: NeonEndpoint {
                    id: ep.id.to_string(),
                    tenant_id: ep.tenant_id.to_string(),
                    timeline_id: ep.timeline_id.to_string(),
                    compute_token_sha256: ep.compute_token_sha256.to_string(),
                },
                roles: vec![NeonRole {
                    name: ep.owner_role.to_string(),
                    scram_verifier: ep.owner_verifier.to_string(),
                }],
                databases: vec![NeonDatabase {
                    name: ep.database_name.to_string(),
                    owner: ep.owner_role.to_string(),
                }],
                needs_reconfigure: false,
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

    async fn neon_roles(&self, endpoint_id: &str) -> Result<Vec<NeonRole>, MetadataError> {
        let state = self.read("neon_roles")?;
        Ok(state
            .endpoints
            .get(endpoint_id)
            .map(|e| e.roles.clone())
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
        Ok(true)
    }

    async fn clear_needs_reconfigure(
        &self,
        endpoint_id: &str,
        role: &str,
        applied_verifier: &str,
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("clear_needs_reconfigure")?;
        let Some(ep) = state.endpoints.get_mut(endpoint_id) else {
            return Ok(false);
        };
        let applied = ep
            .roles
            .iter()
            .any(|r| r.name == role && r.scram_verifier == applied_verifier);
        if !(ep.needs_reconfigure && applied) {
            return Ok(false);
        }
        ep.needs_reconfigure = false;
        Ok(true)
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
            })
            .collect())
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
                    default_schema: db.default_schema.clone(),
                },
            })
        }))
    }
}

/// Oldest first, like the real queries.
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
    async fn password_updated(&self, endpoint_id: &str, _role: &str) -> anyhow::Result<u64> {
        record(
            &self.journal,
            format!("cache.password_updated {endpoint_id}"),
        );
        if let Some(message) = lock(&self.fail).clone() {
            anyhow::bail!(message);
        }
        Ok(*lock(&self.receivers))
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
        ready_poll: Duration::from_millis(1),
        ready_timeout: Duration::from_millis(20),
        claim_refresh: Duration::from_millis(5),
        schema_check_cursor: Mutex::default(),
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
