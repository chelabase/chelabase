//! In-memory ports for the worker's tests. Every call that changes something is journaled in
//! one shared [`Journal`], so tests can assert on the order across ports.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use cheladb_metadata::{
    DatabaseRecord, DatabaseStatus, ENDPOINT_LOCK_TIMEOUT, EndpointState, LiveDatabase,
    MetadataError, NeonDatabase, NeonEndpoint, NeonRole, NewEndpoint, ReadyDatabase,
};
use cheladb_neon::backend::fake::FakeBackend;
pub use cheladb_neon::backend::fake::Journal;
use cheladb_neon::spec::Placement;
use cheladb_neon::{NeonId, StorconError};
use cheladb_secrets::SecretKey;
use cheladb_types::{DatabaseId, ProjectId, TenantId};
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

#[derive(Default)]
pub struct FakeState {
    pub databases: HashMap<DatabaseId, DatabaseRecord>,
    pub endpoints: HashMap<String, FakeEndpoint>,
    /// Seconds since a database's row last changed (0 when absent).
    pub ages: HashMap<DatabaseId, i64>,
    /// The method that fails, e.g. `"set_ready"`.
    pub fail: Option<&'static str>,
    /// The Nth `get_database` call (1-based) first flips the row to `deleting`.
    pub deleting_on_get: Option<u32>,
    /// `set_ready` first flips the row to `deleting`, as a delete landing just before it.
    pub deleting_before_set_ready: bool,
    get_calls: u32,
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

impl FakeMetadata {
    pub fn state(&self) -> MutexGuard<'_, FakeState> {
        lock(&self.state)
    }

    /// A database row with the given status and nothing else.
    pub fn insert_database(&self, name: &str, status: DatabaseStatus) -> DatabaseId {
        let id = DatabaseId::new();
        self.state().databases.insert(
            id,
            DatabaseRecord {
                database_id: id,
                project_id: ProjectId::new(),
                tenant_id: TenantId::new(),
                name: name.to_string(),
                status,
                endpoint_id: None,
                database_name: None,
                owner_role: None,
                owner_password_enc: None,
                error: None,
            },
        );
        id
    }

    pub fn database(&self, id: DatabaseId) -> DatabaseRecord {
        self.state().databases[&id].clone()
    }

    pub fn endpoint_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.state().endpoints.keys().cloned().collect();
        ids.sort();
        ids
    }

    fn write(&self, method: &'static str) -> Result<MutexGuard<'_, FakeState>, MetadataError> {
        record(&self.journal, format!("metadata.{method}"));
        self.read(method)
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
        if state.deleting_on_get == Some(state.get_calls)
            && let Some(db) = state.databases.get_mut(&id)
            && live(db.status)
        {
            db.status = DatabaseStatus::Deleting;
        }
        Ok(state.databases.get(&id).cloned())
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
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    async fn create_endpoint_for_database(
        &self,
        id: DatabaseId,
        ep: &NewEndpoint<'_>,
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("create_endpoint_for_database")?;
        match state.databases.get_mut(&id) {
            Some(db) if live(db.status) && db.endpoint_id.is_none() => {
                db.endpoint_id = Some(ep.id.to_string());
            }
            _ => return Ok(false),
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

    async fn set_ready(
        &self,
        id: DatabaseId,
        endpoint_id: &str,
        db: ReadyDatabase<'_>,
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("set_ready")?;
        let flip = state.deleting_before_set_ready;
        match state.databases.get_mut(&id) {
            Some(row) => {
                if flip && live(row.status) {
                    row.status = DatabaseStatus::Deleting;
                }
                if !live(row.status) {
                    return Ok(false);
                }
                row.status = DatabaseStatus::Ready;
                row.endpoint_id = Some(endpoint_id.to_string());
                row.database_name = Some(db.database_name.to_string());
                row.owner_role = Some(db.owner_role.to_string());
                row.owner_password_enc = Some(db.owner_password_enc.to_string());
                row.error = None;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn set_failed(&self, id: DatabaseId, error: &str) -> Result<(), MetadataError> {
        let mut state = self.write("set_failed")?;
        if let Some(db) = state.databases.get_mut(&id)
            && live(db.status)
        {
            db.status = DatabaseStatus::Failed;
            db.error = Some(error.to_string());
        }
        Ok(())
    }

    async fn mark_deleted(&self, id: DatabaseId) -> Result<(), MetadataError> {
        let mut state = self.write("mark_deleted")?;
        if let Some(db) = state.databases.get_mut(&id) {
            db.status = DatabaseStatus::Deleted;
            db.owner_password_enc = None;
        }
        Ok(())
    }

    async fn delete_endpoint(&self, endpoint_id: &str) -> Result<(), MetadataError> {
        let mut state = self.write("delete_endpoint")?;
        state.endpoints.remove(endpoint_id);
        for db in state.databases.values_mut() {
            if db.endpoint_id.as_deref() == Some(endpoint_id) {
                db.endpoint_id = None;
            }
        }
        Ok(())
    }

    async fn replace_owner_secret(
        &self,
        id: DatabaseId,
        endpoint_id: &str,
        role: &str,
        verifier: &str,
        sealed: &str,
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("replace_owner_secret")?;
        let state = &mut *state;
        let Some(db) = state.databases.get_mut(&id) else {
            return Ok(false);
        };
        if db.status != DatabaseStatus::Ready || db.endpoint_id.as_deref() != Some(endpoint_id) {
            return Ok(false);
        }
        let Some(ep) = state.endpoints.get_mut(endpoint_id) else {
            return Ok(false);
        };
        let Some(r) = ep.roles.iter_mut().find(|r| r.name == role) else {
            return Ok(false);
        };
        db.owner_password_enc = Some(sealed.to_string());
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
                db.error = Some("provisioning timed out".to_string());
                Ok(true)
            }
            _ => Ok(false),
        }
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
                endpoint_id: db.endpoint_id.clone(),
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
                let db = state
                    .databases
                    .values()
                    .find(|db| db.endpoint_id.as_deref() == Some(ep.endpoint.id.as_str()));
                EndpointState {
                    id: ep.endpoint.id.clone(),
                    tenant_id: ep.endpoint.tenant_id.clone(),
                    database_id: db.map(|db| db.database_id),
                    database_status: db.map(|db| db.status),
                    needs_reconfigure: ep.needs_reconfigure,
                }
            })
            .collect();
        rows.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(rows)
    }
}

/// The cause under every injected storage failure.
pub const STORAGE_CAUSE: &str = "connection refused";

/// Tenants and timelines in memory.
pub struct FakeStorage {
    pub tenants: Mutex<HashSet<NeonId>>,
    pub timelines: Mutex<HashSet<(NeonId, NeonId)>>,
    /// The method that fails, e.g. `"create_timeline"`.
    pub fail: Mutex<Option<&'static str>>,
    /// Runs once, at the start of the next `create_tenant` (e.g. to land a delete).
    pub on_create_tenant: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    pub journal: Journal,
}

impl FakeStorage {
    fn call(&self, method: &'static str) -> Result<(), StorconError> {
        record(&self.journal, format!("storage.{method}"));
        if *lock(&self.fail) == Some(method) {
            // With a cause, so tests see whether the whole chain is kept.
            return Err(StorconError::Other(
                anyhow::anyhow!("{STORAGE_CAUSE}").context(format!("injected {method} failure")),
            ));
        }
        Ok(())
    }

    pub fn has_tenant(&self, tenant: &str) -> bool {
        lock(&self.tenants).contains(&tenant.parse::<NeonId>().unwrap())
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
        self.call("create_tenant")?;
        lock(&self.tenants).insert(tenant.clone());
        Ok(())
    }

    async fn create_timeline(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<(), StorconError> {
        self.call("create_timeline")?;
        lock(&self.timelines).insert((tenant.clone(), timeline.clone()));
        Ok(())
    }

    async fn delete_tenant(&self, tenant: &NeonId) -> Result<(), StorconError> {
        self.call("delete_tenant")?;
        // A tenant that's already gone counts as deleted, like the controller's 404.
        lock(&self.tenants).remove(tenant);
        lock(&self.timelines).retain(|(t, _)| t != tenant);
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
            fail: Mutex::default(),
            on_create_tenant: Mutex::default(),
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
    };
    (worker, journal)
}
