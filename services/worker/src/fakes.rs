//! In-memory ports for the worker's tests. Every call that changes something is journaled in
//! one shared [`Journal`], so tests can assert on the order across ports.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use chelabase_metadata::{
    AppSession, AppUsersStatus, BranchAuthSync, OnDelete, ProjectAuth, ProjectRecord,
    ProjectStatus, BranchConnectionRow, BranchDeleteOutcome, BranchKind, BranchRecord, BranchStatus,
    CreateBranchOutcome, DataApiSettings, DataKeyRecord, DatabaseRecord, DatabaseSettings,
    DatabaseStatus, ENDPOINT_LOCK_TIMEOUT, EndpointState, InsertRoleOutcome, LiveBranch,
    LiveDatabase, MetadataError, NeonDatabase, NeonEndpoint, NeonRole, NewBranch, NewDataKey,
    NewEndpoint, NewStaticEndpoint, ObservedDatabase, ResyncApplied, ResyncWrite, RoleCopy,
    RoleLevel, RoleOrigin, SchemaBase, SealedChange,
};
use chelabase_metadata::{
    ApiRoleChange, NewApiRole, RenameCopyOutcome, RoleAttributes, RoleWriteOutcome, SecretChange,
};
use chelabase_metadata::{
    BranchBusy, ClaimOutcome, MergeEvent, MergeRecord, MergeStatus, MergeStatusPatch,
};
use chelabase_neon::backend::fake::FakeBackend;
pub use chelabase_neon::backend::fake::Journal;
use chelabase_neon::spec::Placement;
use chelabase_neon::storcon::{LsnAtTime, LsnKind, head_lsn};
use chelabase_neon::{Lsn, NeonId, StorconError};
use chelabase_secrets::SecretKey;
use chelabase_types::data_key::DataKeyKind;
use chelabase_types::{BranchId, DatabaseId, ProjectId, TenantId};
use chrono::{DateTime, Utc};
use futures_util::future::BoxFuture;

use chelabase_metadata::{BranchDatabase, DataApiState, DatabaseOrigin, ServedDatabase};

use crate::Worker;
use chelabase_neon::reconfigure::ComputeCtlStatus;
use crate::ports::{AttachWait, CacheNotifier, SpecPusher, Storage, WorkerMetadata};

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
    pub attributes: RoleAttributes,
    /// E32: an API create or PATCH not yet confirmed (see `RoleCopy::attributes_pending`).
    pub attributes_pending: bool,
    /// The API write that set `attributes_pending` (`attributes_change_id`).
    pub attributes_change_id: Option<uuid::Uuid>,
}

impl FakeLogin {
    /// A row with the default attributes.
    pub fn new(origin: RoleOrigin, password_enc: Option<&str>, pending: bool) -> Self {
        Self {
            origin,
            password_enc: password_enc.map(str::to_string),
            pending,
            login: true,
            level: RoleLevel::Plain,
            api_owner_level: false,
            attributes: RoleAttributes::default(),
            attributes_pending: false,
            attributes_change_id: None,
        }
    }
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
    /// `lock_endpoint` fails like a metadata pool with no free connection in time
    /// (`sqlx::Error::PoolTimedOut`).
    pub lock_pool_timeout: bool,
    /// The Nth `get_database` call (1-based) first flips the row to `deleting`.
    pub deleting_on_get: Option<u32>,
    /// `set_branch_ready` first flips the row to `deleting`, as a delete landing just before it
    /// (the database's delete for main, the branch's own for any other branch).
    pub deleting_before_set_ready: bool,
    /// The Nth `get_branch` call (1-based) of that branch first begins its delete.
    pub deleting_on_get_branch: Option<(BranchId, u32)>,
    /// `(branch, n, timeline)`: the branch's `n`th `get_branch` (absolute count) first moves
    /// it to `timeline`, like a switch landing meanwhile.
    pub timeline_moves_on_get_branch: Option<(BranchId, u32, String)>,
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
    /// Each `fix_key_roles` call: the branch and its repairs.
    pub key_fixes: Vec<(BranchId, Vec<chelabase_metadata::KeyRoleFix>)>,
    /// Runs once, just before the next `finish_enable_data_api`: a write landing while the
    /// enable's bootstrap ran (a `default_schema` PATCH, say).
    pub before_finish_enable: Option<StateHook>,
    /// Runs once, just before the next `insert_data_key`: a concurrent run's insert.
    pub before_insert_data_key: Option<StateHook>,
    /// Runs once, just before the next `set_authenticator_password`: a concurrent run's write.
    pub before_set_authenticator_password: Option<StateHook>,
    /// Each branch's stored `chelabase_storage` login: the sealed password and its salt.
    pub storage_logins: HashMap<BranchId, (String, Vec<u8>)>,
    /// Runs once, just before the next `set_storage_password`: a concurrent run's write.
    pub before_set_storage_password: Option<StateHook>,
    /// The next N `apply_resync` calls answer `VersionMoved` (and bump the version, as a
    /// concurrent write would).
    pub resync_version_moves: u32,
    /// `delete_role_copy` finds the row already gone (removed meanwhile, say by a forwarded
    /// `DROP ROLE`).
    pub role_row_vanishes: bool,
    /// The next `confirm_role` finds the version moved (a receiver write meanwhile).
    pub confirm_version_moves: bool,
    /// The next `rename_role_copy` finds no endpoint (deleted or switched meanwhile).
    pub rename_endpoint_vanishes: bool,
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
    /// Applied once, just after the next `project_auth` read answers: the row moves to this
    /// status, or goes (`None`), as a concurrent job would. `after_auth_read_skip` reads
    /// answer unchanged first.
    pub after_auth_read: Option<Option<AppUsersStatus>>,
    pub after_auth_read_skip: usize,
    pub app_sessions: Vec<AppSession>,
    /// The schema merge base history, oldest first (E18); ids increase with each append.
    pub schema_bases: Vec<SchemaBase>,
    /// `branch_restores`: `(branch, restored timeline, from timeline, at)` (T14 I1).
    pub restores: Vec<(BranchId, String, String, String)>,
    /// Seconds since an endpoint row was created (0 when absent).
    pub endpoint_ages: HashMap<String, i64>,
    /// Schema merges (step 6g T8a) and their log, oldest event first.
    pub merges: HashMap<uuid::Uuid, MergeRecord>,
    pub merge_events: Vec<MergeEvent>,
    /// Runs once, just before the next `set_merge_status`: a write landing while a job ran (a
    /// cancel, say).
    pub before_merge_write: Option<StateHook>,
    /// Final round A (EOS-1): runs once, just before the next member-role row write (either
    /// upsert): a revoke-now landing between a reconcile's read of the rows and its write.
    pub before_upsert_member_branch_role: Option<StateHook>,
    /// `merges.handed_back_at` (T14 M10).
    pub handed_back: HashMap<uuid::Uuid, DateTime<Utc>>,
    /// Members of every tenant, their project grants by `(member, project)`, their overrides by
    /// `(member, database)`, and the reconciler's `member_branch_roles`.
    pub members: Vec<chelabase_metadata::members::MemberRecord>,
    pub grants: HashMap<(uuid::Uuid, uuid::Uuid), chelabase_metadata::members::GrantRow>,
    pub overrides: HashMap<(uuid::Uuid, uuid::Uuid), chelabase_metadata::members::OverrideRow>,
    /// Team grants by `(member, project)` (members hub step 2's source; the step 1 metadata
    /// never fills it, the fake does to test the union, T2).
    pub team_grants: HashMap<(uuid::Uuid, uuid::Uuid), Vec<chelabase_types::access::Grant>>,
    pub member_roles: Vec<chelabase_metadata::members::MemberBranchRole>,
    /// R31: the last acted role by (member, branch).
    pub acted_roles: HashMap<(uuid::Uuid, uuid::Uuid), String>,
    /// T6: every personal-login record (any database), as `personal_login_records` reads them
    /// (`qualifies` is whatever the test sets).
    pub personal_logins: Vec<chelabase_metadata::personal_logins::PersonalLoginRecord>,
    /// PB: each branch's login-role guard exemptions, in stored order (missing key: none). A
    /// new branch and a restore's backup copy their source's.
    pub guard_exemptions: HashMap<BranchId, Vec<String>>,
    /// RT1 T5: `mark_deleted` / `mark_branch_deleted` remove the row instead of tombstoning it,
    /// so nothing can be read about it once the write returned (the publisher wrapper must
    /// read its ids before the write).
    pub purge_on_mark_deleted: bool,
    get_calls: u32,
    get_branch_calls: HashMap<BranchId, u32>,
}

impl FakeState {
    /// Begins `id`'s delete at its `n`th `get_branch` (1-based) counted from now.
    pub fn delete_at_get_branch(&mut self, id: BranchId, n: u32) {
        let seen = self.get_branch_calls.get(&id).copied().unwrap_or(0);
        self.deleting_on_get_branch = Some((id, seen + n));
    }

    /// Moves `id` to `timeline` at its `n`th `get_branch` (1-based) counted from now, before
    /// that call answers.
    pub fn move_timeline_at_get_branch(&mut self, id: BranchId, n: u32, timeline: &str) {
        let seen = self.get_branch_calls.get(&id).copied().unwrap_or(0);
        self.timeline_moves_on_get_branch = Some((id, seen + n, timeline.to_string()));
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

    /// What keeps `branch` from being reset, restored or deleted (`branch_busy`'s SQL): a merge
    /// other than `except` holding it as source or target (oldest first), else a live rehearsal
    /// branch of it.
    fn merge_busy(&self, branch: BranchId, except: Option<uuid::Uuid>) -> Option<BranchBusy> {
        let mut holding: Vec<&MergeRecord> = self
            .merges
            .values()
            .filter(|m| {
                (m.source_branch_id == branch.as_uuid() || m.target_branch_id == branch.as_uuid())
                    && m.state().holds_branches()
                    && Some(m.id) != except
            })
            .collect();
        holding.sort_by_key(|m| (m.created_at, m.id));
        if let Some(m) = holding.first() {
            return Some(BranchBusy::Merge(m.id));
        }
        sorted(self.branches.values().filter(|b| {
            b.parent_branch_id == Some(branch)
                && b.kind == BranchKind::Rehearsal
                && b.status != BranchStatus::Deleted
        }))
        .first()
        .map(|b| BranchBusy::Rehearsal(b.id))
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

    /// The restore switch's base copies (E18, T14 I1), as the metadata makes them: the link
    /// (`to` restored from `from` at `at`) is recorded; then per database, the newest entry on
    /// `from` at or before `at`, else the walk goes on through `from`'s own restore link at the
    /// earlier of the point and that link's, at any depth; the pick is appended at (`to`, `at`).
    fn copy_bases_for_restore(&mut self, branch: BranchId, from: &str, at: &str, to: &str) {
        let lsn = |l: &str| l.parse::<chelabase_neon::Lsn>().expect("an LSN");
        if !self.restores.iter().any(|(b, t, _, _)| *b == branch && t == to) {
            self.restores
                .push((branch, to.to_string(), from.to_string(), at.to_string()));
        }
        // The walk: (timeline, point), shallowest first.
        let mut walk = vec![(from.to_string(), lsn(at))];
        while walk.len() < 10_000 {
            let (timeline, point) = walk.last().cloned().unwrap();
            let Some((_, _, prev, prev_at)) = self
                .restores
                .iter()
                .find(|(b, t, _, _)| *b == branch && *t == timeline)
            else {
                break;
            };
            walk.push((prev.clone(), point.min(lsn(prev_at))));
        }
        let mut picked: Vec<SchemaBase> = Vec::new();
        for (timeline, point) in &walk {
            for b in self.schema_bases.iter().rev() {
                let on_point = b.branch_id == branch
                    && b.branch_timeline_id.as_deref() == Some(timeline.as_str())
                    && b.branch_lsn
                        .as_deref()
                        .and_then(|l| l.parse::<chelabase_neon::Lsn>().ok())
                        .is_some_and(|l| l <= *point);
                if on_point && !picked.iter().any(|p| p.database_name == b.database_name) {
                    picked.push(b.clone());
                }
            }
        }
        picked.sort_by(|a, b| a.database_name.cmp(&b.database_name));
        let last = self.schema_bases.iter().map(|b| b.id).max().unwrap_or(0);
        for (id, copy) in (last + 1..).zip(picked) {
            self.schema_bases.push(SchemaBase {
                id,
                branch_timeline_id: Some(to.to_string()),
                branch_lsn: Some(at.to_string()),
                recorded_at: Utc::now(),
                ..copy
            });
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

    /// The branch linked to the endpoint, as the orchestrator's `endpoint_branch` reads it: a
    /// deleting or deleted branch is excluded. Journaled as `metadata.endpoint_branch`; the
    /// `WorkerMetadata::endpoint_branch` impl delegates here.
    pub async fn endpoint_branch(
        &self,
        endpoint_id: &str,
    ) -> Result<Option<BranchId>, MetadataError> {
        let state = self.write("endpoint_branch")?;
        Ok(state
            .branches
            .values()
            .find(|b| {
                b.endpoint_id.as_deref() == Some(endpoint_id)
                    && !matches!(b.status, BranchStatus::Deleting | BranchStatus::Deleted)
            })
            .map(|b| b.id))
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
                storage_max_connections: 2,
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
                guard_logins: true,
                review_ack_hash: None,
                review_ack_keys: Vec::new(),
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
        let parent_guard_logins = state.branches[&b.parent_branch_id].guard_logins;
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
                guard_logins: parent_guard_logins,
                review_ack_hash: None,
                review_ack_keys: Vec::new(),
            },
        );
        if let Some(exempt) = state.guard_exemptions.get(&b.parent_branch_id).cloned() {
            state.guard_exemptions.insert(b.id, exempt);
        }
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

    /// A live app session of `user`, with identity session `identity_session_id`.
    pub fn add_session(&self, project: ProjectId, user: uuid::Uuid, identity_session_id: &str) -> uuid::Uuid {
        let id = uuid::Uuid::new_v4();
        let now = chrono::Utc::now();
        self.state().app_sessions.push(AppSession {
            id,
            project_id: project,
            user_id: user,
            identity_session_id: identity_session_id.to_string(),
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

    /// A merge of `db`'s database `app` from `source` into `target`, in `status`, as the API
    /// leaves it (claimed when it's a working status), with the default options.
    pub fn insert_merge(
        &self,
        db: DatabaseId,
        source: BranchId,
        target: BranchId,
        direction: chelabase_metadata::MergeDirection,
        status: MergeStatus,
    ) -> uuid::Uuid {
        let id = uuid::Uuid::new_v4();
        let now = Utc::now();
        let row = MergeRecord {
            id,
            database_id: db.as_uuid(),
            source_branch_id: source.as_uuid(),
            target_branch_id: target.as_uuid(),
            direction,
            database_name: "app".to_string(),
            status,
            failed_phase: None,
            version: 1,
            base_fingerprint: None,
            source_fingerprint: None,
            target_fingerprint: None,
            analysis: None,
            resolutions: serde_json::json!([]),
            user_steps: serde_json::json!([]),
            acknowledgements: serde_json::json!([]),
            options: serde_json::json!({ "lock_timeout_ms": 5000, "after": "keep" }),
            plan: None,
            rehearsal: None,
            result: None,
            rebaselined: false,
            error: None,
            claimed_at: status.is_working().then_some(now),
            created_by: "test".to_string(),
            created_at: now,
            updated_at: now,
        };
        self.state().merges.insert(id, row);
        id
    }

    /// The merge's row; panics when there's none.
    pub fn merge_row(&self, id: uuid::Uuid) -> MergeRecord {
        self.state().merges.get(&id).cloned().expect("the merge")
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
        if self.write("lock_endpoint")?.lock_pool_timeout {
            return Err(MetadataError::Database(sqlx::Error::PoolTimedOut));
        }
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

    async fn endpoint_branch(&self, endpoint_id: &str) -> Result<Option<BranchId>, MetadataError> {
        FakeMetadata::endpoint_branch(self, endpoint_id).await
    }

    async fn compute_branch(
        &self,
        endpoint_id: &str,
    ) -> Result<Option<(BranchId, BranchKind)>, MetadataError> {
        let state = self.read("compute_branch")?;
        Ok(state
            .branches
            .values()
            .find(|b| {
                b.endpoint_id.as_deref() == Some(endpoint_id) && b.status != BranchStatus::Deleted
            })
            .map(|b| (b.id, b.kind)))
    }

    async fn get_branch(&self, id: BranchId) -> Result<Option<BranchRecord>, MetadataError> {
        let mut state = self.read("get_branch")?;
        let calls = state.get_branch_calls.entry(id).or_default();
        *calls += 1;
        let calls = *calls;
        if state.deleting_on_get_branch == Some((id, calls)) {
            state.begin_branch_delete(id);
        }
        if let Some((branch, n, timeline)) = state.timeline_moves_on_get_branch.clone()
            && (branch, n) == (id, calls)
            && let Some(b) = state.branches.get_mut(&id)
        {
            b.timeline_id = Some(timeline);
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
        // T6 (S1): which copies the worker handed over, by name and origin.
        for copy in ep.role_copies {
            record(
                &self.journal,
                format!("metadata.endpoint_copy {} {} {}", ep.id, copy.name, copy.origin.as_str()),
            );
        }
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
                    routable: ep.routable,
                    static_lsn: ep.static_lsn.map(str::to_string),
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
                            attributes: l.attributes.clone(),
                            attributes_pending: false,
                            attributes_change_id: None,
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
        if state.purge_on_mark_deleted {
            state.databases.remove(&id);
        }
        Ok(())
    }

    async fn mark_branch_deleted(&self, id: BranchId) -> Result<(), MetadataError> {
        let mut state = self.write("mark_branch_deleted")?;
        if let Some(b) = state.branches.get_mut(&id) {
            b.status = BranchStatus::Deleted;
            b.owner_password_enc = None;
        }
        if state.purge_on_mark_deleted {
            state.branches.remove(&id);
        }
        Ok(())
    }

    async fn create_static_endpoint(&self, ep: &NewStaticEndpoint<'_>) -> Result<(), MetadataError> {
        record(
            &self.journal,
            format!(
                "metadata.create_static_endpoint {} {} at {}",
                ep.id, ep.timeline_id, ep.static_lsn
            ),
        );
        let mut state = self.read("create_static_endpoint")?;
        state.endpoints.insert(
            ep.id.to_string(),
            FakeEndpoint {
                endpoint: NeonEndpoint {
                    id: ep.id.to_string(),
                    tenant_id: ep.tenant_id.to_string(),
                    timeline_id: ep.timeline_id.to_string(),
                    compute_token_sha256: ep.compute_token_sha256.to_string(),
                    routable: false,
                    static_lsn: Some(ep.static_lsn.to_string()),
                },
                roles: Vec::new(),
                databases: Vec::new(),
                needs_reconfigure: false,
                roles_version: 0,
                logins: HashMap::new(),
                branch_databases: Vec::new(),
                compute_settings_version: 0,
                unreadable: HashSet::new(),
            },
        );
        Ok(())
    }

    async fn schema_base(
        &self,
        branch: BranchId,
        database_name: &str,
    ) -> Result<Option<SchemaBase>, MetadataError> {
        let state = self.read("schema_base")?;
        Ok(state
            .schema_bases
            .iter()
            .rev()
            .find(|b| b.branch_id == branch && b.database_name == database_name)
            .cloned())
    }

    async fn append_schema_base(&self, base: &SchemaBase) -> Result<i64, MetadataError> {
        record(
            &self.journal,
            format!(
                "metadata.append_schema_base {} {} {}",
                base.branch_id,
                base.database_name,
                base.source.as_str()
            ),
        );
        let mut state = self.read("append_schema_base")?;
        let state = &mut *state;
        let id = state.schema_bases.iter().map(|b| b.id).max().unwrap_or(0) + 1;
        state.schema_bases.push(SchemaBase {
            id,
            taken_at: Utc::now(),
            recorded_at: Utc::now(),
            ..base.clone()
        });
        // T14 M7: an older entry superseded on its timeline by one recorded more than the
        // database's history window ago drops its model.
        let window = state
            .branches
            .get(&base.branch_id)
            .and_then(|b| state.databases.get(&b.database_id))
            .map(|d| d.history_window);
        if let Some(window) = window {
            let cutoff = Utc::now()
                - chrono::Duration::from_std(window).unwrap_or(chrono::Duration::MAX);
            let same = |a: &SchemaBase, b: &SchemaBase| {
                a.branch_id == b.branch_id
                    && a.database_name == b.database_name
                    && a.branch_timeline_id == b.branch_timeline_id
            };
            let superseded: Vec<i64> = state
                .schema_bases
                .iter()
                .filter(|e| e.branch_id == base.branch_id && e.database_name == base.database_name)
                .filter(|e| {
                    state
                        .schema_bases
                        .iter()
                        .any(|f| same(e, f) && f.id > e.id && f.recorded_at < cutoff)
                })
                .map(|e| e.id)
                .collect();
            for e in state
                .schema_bases
                .iter_mut()
                .filter(|e| superseded.contains(&e.id))
            {
                e.model = None;
                e.model_version = None;
                e.server_major = None;
            }
        }
        Ok(id)
    }

    async fn update_schema_base(&self, base: &SchemaBase) -> Result<bool, MetadataError> {
        record(
            &self.journal,
            format!("metadata.update_schema_base {}", base.id),
        );
        let mut state = self.read("update_schema_base")?;
        let Some(entry) = state.schema_bases.iter_mut().find(|b| b.id == base.id) else {
            return Ok(false);
        };
        entry.model = base.model.clone();
        entry.model_version = base.model_version;
        entry.server_major = base.server_major;
        entry.identity_map = base.identity_map.clone();
        Ok(true)
    }

    async fn prune_schema_bases(
        &self,
        branch: BranchId,
        keep: &[String],
    ) -> Result<u64, MetadataError> {
        record(
            &self.journal,
            format!("metadata.prune_schema_bases {branch} keeping {keep:?}"),
        );
        let mut state = self.read("prune_schema_bases")?;
        let before = state.schema_bases.len();
        state
            .schema_bases
            .retain(|b| b.branch_id != branch || keep.contains(&b.database_name));
        Ok((before - state.schema_bases.len()) as u64)
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
        role: &NewApiRole<'_>,
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
        let name = role.name;
        if ep.roles.iter().any(|r| r.name == name) || ep.logins.contains_key(name) {
            return Ok(InsertRoleOutcome::Exists);
        }
        ep.set_verifier(name, role.verifier);
        ep.logins.insert(
            name.to_string(),
            FakeLogin {
                origin: RoleOrigin::Api,
                password_enc: role.password_enc.map(str::to_string),
                pending: true,
                login: role.login,
                level: if role.owner_level {
                    RoleLevel::OwnerLevel
                } else {
                    RoleLevel::Plain
                },
                api_owner_level: role.owner_level,
                attributes: role.attributes.clone(),
                attributes_pending: true,
                attributes_change_id: Some(role.change_id),
            },
        );
        ep.roles_version += 1;
        Ok(InsertRoleOutcome::Inserted)
    }

    async fn update_api_role(
        &self,
        endpoint_id: &str,
        name: &str,
        change: &ApiRoleChange,
        cap: u32,
    ) -> Result<RoleWriteOutcome, MetadataError> {
        let mut state = self.write("update_api_role")?;
        let Some(ep) = state.endpoints.get_mut(endpoint_id) else {
            return Ok(RoleWriteOutcome::NotFound);
        };
        if !ep
            .logins
            .get(name)
            .is_some_and(|l| l.origin == RoleOrigin::Api)
        {
            return Ok(RoleWriteOutcome::NotFound);
        }
        if matches!(change.secret, SecretChange::Set(..))
            && ep.verifier(name).is_none()
            && ep.copy_count() >= usize::try_from(cap).unwrap_or(usize::MAX)
        {
            return Ok(RoleWriteOutcome::CapReached);
        }
        let row = ep.logins.get_mut(name).expect("checked above");
        row.api_owner_level = change.owner_level;
        row.level = if change.owner_level {
            RoleLevel::OwnerLevel
        } else {
            RoleLevel::Plain
        };
        row.login = change.login;
        row.attributes = change.attributes.clone();
        row.pending = true;
        row.attributes_pending = true;
        row.attributes_change_id = Some(change.change_id);
        match &change.secret {
            SecretChange::Keep => {}
            SecretChange::Clear => {
                row.password_enc = None;
                ep.set_verifier(name, None);
                ep.unreadable.remove(name);
            }
            SecretChange::Set(verifier, sealed) => {
                row.password_enc = Some(sealed.clone());
                ep.set_verifier(name, Some(verifier));
                ep.unreadable.remove(name);
            }
        }
        ep.roles_version += 1;
        Ok(RoleWriteOutcome::Done)
    }

    async fn store_role_secret(
        &self,
        endpoint_id: &str,
        name: &str,
        verifier: &str,
        password_enc: &str,
        cap: u32,
    ) -> Result<RoleWriteOutcome, MetadataError> {
        let mut state = self.write("store_role_secret")?;
        let Some(ep) = state.endpoints.get_mut(endpoint_id) else {
            return Ok(RoleWriteOutcome::NotFound);
        };
        if ep.roles.first().is_some_and(|o| o.name == name) {
            return Ok(RoleWriteOutcome::NotFound);
        }
        if ep.verifier(name).is_none()
            && ep.copy_count() >= usize::try_from(cap).unwrap_or(usize::MAX)
        {
            return Ok(RoleWriteOutcome::CapReached);
        }
        let row = ep
            .logins
            .entry(name.to_string())
            .or_insert_with(|| FakeLogin::new(RoleOrigin::Sql, None, true));
        row.password_enc = Some(password_enc.to_string());
        row.login = true;
        row.pending = true;
        ep.set_verifier(name, Some(verifier));
        ep.unreadable.remove(name);
        ep.roles_version += 1;
        Ok(RoleWriteOutcome::Done)
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
        record(
            &self.journal,
            format!("metadata.rename_role_copy {endpoint_id} {old} -> {new}"),
        );
        let mut state = self.write("rename_role_copy")?;
        let state = &mut *state;
        if std::mem::take(&mut state.rename_endpoint_vanishes) {
            return Ok(RenameCopyOutcome::NotFound);
        }
        let keys_taken = state.live_keys(branch).any(|k| {
            k.kind == DataKeyKind::Secret && k.role == new
        });
        let Some(ep) = state.endpoints.get_mut(endpoint_id) else {
            return Ok(RenameCopyOutcome::NotFound);
        };
        if keys_taken || ep.logins.contains_key(new) || ep.roles.iter().any(|r| r.name == new) {
            return Ok(RenameCopyOutcome::Taken);
        }
        let owner = ep.roles.first().map(|r| r.name.clone());
        let row = if owner.as_deref() == Some(old) {
            None
        } else {
            ep.logins.remove(old)
        };
        let moved = row.is_some();
        let was_unreadable = ep.unreadable.remove(old);
        if let Some(mut row) = row {
            let verifier = ep.verifier(old);
            ep.set_verifier(old, None);
            let mark = match unreadable {
                Some(flag) => flag,
                None => was_unreadable || (row.password_enc.is_some() && resealed.is_none()),
            };
            if mark {
                ep.unreadable.insert(new.to_string());
            }
            row.password_enc = resealed.map(str::to_string);
            ep.set_verifier(new, verifier.as_deref());
            ep.logins.insert(new.to_string(), row);
        }
        ep.roles_version += 1;
        for key in state
            .data_keys
            .iter_mut()
            .filter(|k| k.branch_id == branch && k.role == old)
        {
            key.role = new.to_string();
        }
        Ok(RenameCopyOutcome::Renamed {
            row: moved,
            was_unreadable,
        })
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
        attributes_change: Option<uuid::Uuid>,
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("confirm_role")?;
        let moves = std::mem::take(&mut state.confirm_version_moves);
        let Some(ep) = state.endpoints.get_mut(endpoint_id) else {
            return Ok(false);
        };
        if moves {
            // A receiver write landed between the apply and the confirm.
            ep.roles_version += 1;
        }
        if let (Some(change), Some(login)) = (attributes_change, ep.logins.get_mut(name))
            && login.attributes_change_id == Some(change)
        {
            login.attributes_pending = false;
        }
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
            // As `apply_resync` (S1, MH-10): personal rows are their records' alone. An upsert
            // of origin `personal` is skipped, and one of a name whose row is personal leaves it.
            if up.origin == RoleOrigin::Personal
                || ep
                    .logins
                    .get(&up.name)
                    .is_some_and(|l| l.origin == RoleOrigin::Personal)
            {
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
                attributes: up.attributes.clone(),
                attributes_pending: false,
                attributes_change_id: None,
            });
            row.password_enc = match &up.password_enc {
                SealedChange::Keep => row.password_enc,
                SealedChange::Clear => None,
                SealedChange::Set(s) => Some(s.clone()),
            };
            row.login = up.login;
            row.level = up.level;
            row.pending = up.pending;
            row.attributes = up.attributes.clone();
            row.attributes_pending = false;
            if row.origin == RoleOrigin::Api
                && let Some(owner_level) = up.api_owner_level
            {
                row.api_owner_level = owner_level;
            }
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
            // As `apply_resync`: never the owner's row, never a personal login's.
            let personal = ep
                .logins
                .get(name)
                .is_some_and(|l| l.origin == RoleOrigin::Personal);
            if owner.as_deref() != Some(name.as_str())
                && !personal
                && ep.logins.remove(name).is_some()
            {
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
                    routable: ep.endpoint.routable,
                    static_lsn: ep.endpoint.static_lsn.clone(),
                    age_secs: state
                        .endpoint_ages
                        .get(&ep.endpoint.id)
                        .copied()
                        .unwrap_or(0),
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
        except: Option<uuid::Uuid>,
    ) -> Result<BranchDeleteOutcome, MetadataError> {
        let mut state = self.write("begin_branch_delete")?;
        // T14 F3 (I1): a merge holding the live branch refuses it.
        if state.branches.get(&id).is_some_and(|b| branch_live(b.status))
            && let Some(busy) = state.merge_busy(id, except)
        {
            return Ok(BranchDeleteOutcome::MergeBusy(busy));
        }
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

    async fn claim_branch_for_switch(
        &self,
        id: BranchId,
        allow_protected: bool,
        except: Option<uuid::Uuid>,
    ) -> Result<ClaimOutcome, MetadataError> {
        let mut state = self.write("claim_branch_for_switch")?;
        let ready = state
            .branches
            .get(&id)
            .is_some_and(|b| b.status == BranchStatus::Ready);
        if ready && let Some(busy) = state.merge_busy(id, except) {
            return Ok(ClaimOutcome::MergeBusy(busy));
        }
        Ok(match state.branches.get_mut(&id) {
            Some(b) if b.status == BranchStatus::Ready && b.protected && !allow_protected => {
                ClaimOutcome::Protected
            }
            Some(b) if b.status == BranchStatus::Ready => {
                b.status = BranchStatus::Resetting;
                b.error = None;
                ClaimOutcome::Claimed
            }
            _ => ClaimOutcome::NotReady,
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
        branched_at_lsn: &str,
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
                b.branched_at_lsn = Some(branched_at_lsn.to_string());
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
        restore_lsn: &str,
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
                guard_logins: b.guard_logins,
                review_ack_hash: None,
                review_ack_keys: Vec::new(),
            },
        );
        if let Some(exempt) = state.guard_exemptions.get(&id).cloned() {
            state.guard_exemptions.insert(backup, exempt);
        }
        let branch = state.branches.get_mut(&id).expect("looked up above");
        branch.timeline_id = branch.pending_timeline_id.take();
        branch.pending_timeline_at = None;
        let timeline_id = branch.timeline_id.clone().unwrap();
        state.move_endpoint(b.endpoint_id.as_deref(), &timeline_id);
        if let Some(old) = b.timeline_id.as_deref() {
            state.copy_bases_for_restore(id, old, restore_lsn, &timeline_id);
        }
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
                role_oid: k.role_oid,
                role_timeline: k.role_timeline.clone(),
                role_bypass_rls: k.role_bypass_rls,
                managed_by_function: None,
            })
            .collect())
    }

    async fn fix_key_roles(
        &self,
        branch: BranchId,
        fixes: &[chelabase_metadata::KeyRoleFix],
    ) -> Result<u64, MetadataError> {
        let mut state = self.write("fix_key_roles")?;
        state.key_fixes.push((branch, fixes.to_vec()));
        Ok(fixes.len() as u64)
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

    async fn list_members(
        &self,
        tenant: TenantId,
    ) -> Result<Vec<chelabase_metadata::members::MemberRecord>, MetadataError> {
        let state = self.read("list_members")?;
        Ok(state
            .members
            .iter()
            .filter(|m| {
                m.tenant_id == tenant
                    && m.status != chelabase_metadata::members::MemberStatus::Removed
            })
            .cloned()
            .collect())
    }

    async fn database_grant_sources(
        &self,
        project: uuid::Uuid,
        database: uuid::Uuid,
    ) -> Result<Vec<(uuid::Uuid, chelabase_types::access::GrantSources)>, MetadataError> {
        use chelabase_types::access::{GrantSources, Narrowing, PgRole};
        let state = self.read("database_grant_sources")?;
        let mut ids: Vec<uuid::Uuid> = state
            .grants
            .keys()
            .chain(state.team_grants.keys())
            .filter(|(_, p)| *p == project)
            .map(|(m, _)| *m)
            .chain(state.overrides.keys().filter(|(_, d)| *d == database).map(|(m, _)| *m))
            .collect();
        ids.sort();
        ids.dedup();
        Ok(ids
            .into_iter()
            .map(|m| {
                let own = state.grants.get(&(m, project)).map(|g| g.to_grant());
                let teams = state.team_grants.get(&(m, project)).cloned().unwrap_or_default();
                let narrowing = state.overrides.get(&(m, database)).map(|o| Narrowing {
                    platform_role: o.platform_role,
                    roles: o.pg_roles.as_ref().map(|roles| roles.iter().map(|r| PgRole::parse(r)).collect()),
                });
                (m, GrantSources::new(own, teams, narrowing))
            })
            .collect())
    }

    async fn member_role_branches(&self) -> Result<Vec<uuid::Uuid>, MetadataError> {
        let state = self.read("member_role_branches")?;
        let mut out: Vec<uuid::Uuid> = state
            .branches
            .values()
            .filter(|b| b.status == BranchStatus::Ready && b.kind == BranchKind::Normal)
            .filter(|b| {
                let Some(db) = state.databases.get(&b.database_id) else {
                    return false;
                };
                live(db.status)
            })
            .map(|b| b.id.as_uuid())
            .collect();
        out.sort();
        Ok(out)
    }

    async fn delete_branch_member_roles(&self, branch: uuid::Uuid) -> Result<u64, MetadataError> {
        let mut state = self.write("delete_branch_member_roles")?;
        let before = state.member_roles.len();
        state.member_roles.retain(|r| r.branch_id != branch);
        Ok((before - state.member_roles.len()) as u64)
    }

    async fn member_branch_roles(
        &self,
        branch: uuid::Uuid,
    ) -> Result<Vec<chelabase_metadata::members::MemberBranchRole>, MetadataError> {
        let state = self.read("member_branch_roles")?;
        let mut rows: Vec<_> = state
            .member_roles
            .iter()
            .filter(|r| r.branch_id == branch)
            .cloned()
            .collect();
        rows.sort_by_key(|r| r.member_id);
        Ok(rows)
    }

    async fn guard_exemptions(&self, branch: BranchId) -> Result<Vec<String>, MetadataError> {
        let state = self.read("guard_exemptions")?;
        Ok(state.guard_exemptions.get(&branch).cloned().unwrap_or_default())
    }

    async fn upsert_member_branch_role(
        &self,
        row: &chelabase_metadata::members::MemberBranchRole,
    ) -> Result<(), MetadataError> {
        let mut state = self.write("upsert_member_branch_role")?;
        if let Some(hook) = state.before_upsert_member_branch_role.take() {
            hook(&mut state);
        }
        state
            .member_roles
            .retain(|r| (r.member_id, r.branch_id) != (row.member_id, row.branch_id));
        state.member_roles.push(row.clone());
        Ok(())
    }

    /// As the metadata's (EOS-1): written only while the stored `password_enc` is still
    /// `read` (`None`: no row).
    async fn upsert_member_branch_role_if(
        &self,
        row: &chelabase_metadata::members::MemberBranchRole,
        read: Option<&str>,
    ) -> Result<bool, MetadataError> {
        let mut state = self.write("upsert_member_branch_role_if")?;
        if let Some(hook) = state.before_upsert_member_branch_role.take() {
            hook(&mut state);
        }
        let stored = state
            .member_roles
            .iter()
            .find(|r| (r.member_id, r.branch_id) == (row.member_id, row.branch_id))
            .map(|r| r.password_enc.clone());
        if stored.as_deref() != read {
            return Ok(false);
        }
        state
            .member_roles
            .retain(|r| (r.member_id, r.branch_id) != (row.member_id, row.branch_id));
        state.member_roles.push(row.clone());
        Ok(true)
    }

    async fn member_acted_roles(
        &self,
        branch: uuid::Uuid,
    ) -> Result<Vec<(uuid::Uuid, String)>, MetadataError> {
        let state = self.read("member_acted_roles")?;
        let mut rows: Vec<_> = state
            .acted_roles
            .iter()
            .filter(|((_, b), _)| *b == branch)
            .map(|((m, _), r)| (*m, r.clone()))
            .collect();
        rows.sort();
        Ok(rows)
    }

    async fn delete_member_branch_role(
        &self,
        member: uuid::Uuid,
        branch: uuid::Uuid,
    ) -> Result<(), MetadataError> {
        let mut state = self.write("delete_member_branch_role")?;
        state
            .member_roles
            .retain(|r| (r.member_id, r.branch_id) != (member, branch));
        Ok(())
    }

    async fn personal_login_records(
        &self,
        database: uuid::Uuid,
    ) -> Result<Vec<chelabase_metadata::personal_logins::PersonalLoginRecord>, MetadataError> {
        let state = self.read("personal_login_records")?;
        let mut records: Vec<_> = state
            .personal_logins
            .iter()
            .filter(|r| r.database_id == database)
            .cloned()
            .collect();
        records.sort_by(|a, b| a.role_name.cmp(&b.role_name));
        Ok(records)
    }

    /// As the metadata's (T3-I1): the row carries the record's own verifier (none once
    /// revoked), never `scram_verifier`; no record, nothing written; a row of another origin
    /// under the name is left alone.
    async fn upsert_personal_role_row(
        &self,
        endpoint_id: &str,
        member: uuid::Uuid,
        database: uuid::Uuid,
        role_name: &str,
        _scram_verifier: &str,
    ) -> Result<(), MetadataError> {
        record(
            &self.journal,
            format!("metadata.upsert_personal_role_row {endpoint_id} {role_name}"),
        );
        let mut state = self.read("upsert_personal_role_row")?;
        let state = &mut *state;
        let Some(login) = state
            .personal_logins
            .iter()
            .find(|r| r.member_id == member && r.database_id == database && r.role_name == role_name)
            .cloned()
        else {
            return Ok(());
        };
        let Some(ep) = state.endpoints.get_mut(endpoint_id) else {
            return Err(MetadataError::Database(sqlx::Error::RowNotFound));
        };
        if ep
            .logins
            .get(role_name)
            .is_some_and(|l| l.origin != RoleOrigin::Personal)
        {
            return Ok(());
        }
        let verifier = if login.revoked { None } else { login.scram_verifier };
        ep.logins
            .insert(role_name.to_string(), FakeLogin::new(RoleOrigin::Personal, None, false));
        ep.set_verifier(role_name, verifier.as_deref());
        Ok(())
    }

    async fn delete_personal_role_row(
        &self,
        endpoint_id: &str,
        role_name: &str,
    ) -> Result<(), MetadataError> {
        record(
            &self.journal,
            format!("metadata.delete_personal_role_row {endpoint_id} {role_name}"),
        );
        let mut state = self.read("delete_personal_role_row")?;
        if let Some(ep) = state.endpoints.get_mut(endpoint_id)
            && ep
                .logins
                .get(role_name)
                .is_some_and(|l| l.origin == RoleOrigin::Personal)
        {
            ep.logins.remove(role_name);
            ep.set_verifier(role_name, None);
        }
        Ok(())
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

    async fn set_storage_password(
        &self,
        branch: BranchId,
        sealed: &str,
        salt: &[u8],
    ) -> Result<(), MetadataError> {
        let mut state = self.write("set_storage_password")?;
        if let Some(hook) = state.before_set_storage_password.take() {
            hook(&mut state);
        }
        let live = state.branches.get(&branch).is_some_and(|b| branch_live(b.status));
        if live && !state.storage_logins.contains_key(&branch) {
            state
                .storage_logins
                .insert(branch, (sealed.to_string(), salt.to_vec()));
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
                database_id: b.database_id,
                status: b.status,
                endpoint_id: b.endpoint_id.clone(),
                database_name: db.database_name.clone(),
                authenticator_password_enc: b.authenticator_password_enc.clone(),
                storage_password_enc: state.storage_logins.get(&branch).map(|l| l.0.clone()),
                storage_password_salt: state.storage_logins.get(&branch).map(|l| l.1.clone()),
                functions_password_enc: None,
                functions_password_salt: None,
                settings: DataApiSettings {
                    max_rows: db.max_rows,
                    statement_timeout_ms: db.statement_timeout_ms,
                    max_connections: db.max_connections,
                    storage_max_connections: db.storage_max_connections,
                },
                databases: vec![chelabase_metadata::ServedDatabase {
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
        let mut state = self.write("project_auth")?;
        let found = state.project_auth.get(&project).cloned();
        if state.after_auth_read.is_some() && state.after_auth_read_skip > 0 {
            state.after_auth_read_skip -= 1;
            return Ok(found);
        }
        match state.after_auth_read.take() {
            Some(Some(status)) => {
                if let Some(row) = state.project_auth.get_mut(&project) {
                    row.status = status;
                }
            }
            Some(None) => {
                state.project_auth.remove(&project);
            }
            None => {}
        }
        Ok(found)
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

    async fn finish_disable_app_users(&self, project: ProjectId) -> Result<bool, MetadataError> {
        let mut state = self.write("finish_disable_app_users")?;
        let disabling = state
            .project_auth
            .get(&project)
            .is_some_and(|a| a.status == AppUsersStatus::Disabling);
        if disabling {
            state.project_auth.remove(&project);
        }
        Ok(disabling)
    }

    async fn set_app_users_error(
        &self,
        project: ProjectId,
        error: Option<&str>,
    ) -> Result<(), MetadataError> {
        let mut state = self.write("set_app_users_error")?;
        if let Some(a) = state
            .project_auth
            .get_mut(&project)
            .filter(|a| matches!(a.status, AppUsersStatus::Enabled | AppUsersStatus::Disabling))
            .filter(|a| a.error.as_deref() != error)
        {
            a.error = error.map(str::to_string);
            a.updated_at = Utc::now();
        }
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

    async fn merge(&self, id: uuid::Uuid) -> Result<Option<MergeRecord>, MetadataError> {
        Ok(self.read("merge")?.merges.get(&id).cloned())
    }

    async fn set_merge_status(
        &self,
        id: uuid::Uuid,
        from: &[MergeStatus],
        to: MergeStatus,
        patch: MergeStatusPatch,
    ) -> Result<bool, MetadataError> {
        record(
            &self.journal,
            format!("metadata.set_merge_status {id} {}", to.as_str()),
        );
        let mut state = self.read("set_merge_status")?;
        if let Some(hook) = state.before_merge_write.take() {
            hook(&mut state);
        }
        let Some(current) = state.merges.get(&id) else {
            return Ok(false);
        };
        let guarded = from.contains(&current.status)
            && patch
                .guard_failed_phase
                .as_ref()
                .is_none_or(|p| &current.failed_phase == p)
            && patch.guard_version.is_none_or(|v| current.version == v)
            && patch.guard_result.as_ref().is_none_or(|r| &current.result == r);
        if !guarded {
            return Ok(false);
        }
        if patch.guard_branches_ready {
            let ready = |b: uuid::Uuid| {
                state
                    .branches
                    .get(&BranchId::from(b))
                    .is_some_and(|b| b.status == BranchStatus::Ready)
            };
            if !ready(current.source_branch_id) || !ready(current.target_branch_id) {
                return Ok(false);
            }
        }
        // `merges_one_applying`: one merge changing (or half changed) a target at a time.
        let failed_phase = patch
            .failed_phase
            .clone()
            .unwrap_or_else(|| current.failed_phase.clone());
        if chelabase_metadata::MergeState::of(to, failed_phase.as_deref()).is_applying() {
            let target = current.target_branch_id;
            if state
                .merges
                .values()
                .any(|m| m.id != id && m.target_branch_id == target && m.state().is_applying())
            {
                return Err(MetadataError::Config(
                    "duplicate key value violates unique constraint \"merges_one_applying\"".into(),
                ));
            }
        }
        if patch.log_status {
            let n = state.merge_events.len() as i64 + 1;
            let mut e = MergeEvent::new(id, "status");
            e.id = n;
            e.detail = Some(serde_json::json!({ "to": to.as_str() }));
            state.merge_events.push(e);
        }
        let m = state.merges.get_mut(&id).expect("checked above");
        m.status = to;
        m.updated_at = Utc::now();
        m.claimed_at = to.is_working().then(Utc::now);
        if patch.analysis.is_some() || patch.plan.is_some() {
            m.version += 1;
        }
        let json = |v: serde_json::Value| (!v.is_null()).then_some(v);
        if let Some(v) = patch.analysis {
            m.analysis = json(v);
        }
        if let Some(v) = patch.plan {
            m.plan = json(v);
        }
        if let Some(v) = patch.resolutions {
            m.resolutions = v;
        }
        if let Some(v) = patch.rehearsal {
            m.rehearsal = Some(v);
        }
        if let Some(v) = patch.result {
            m.result = Some(v);
        }
        if let Some(v) = patch.error {
            m.error = v;
        }
        if let Some(v) = patch.failed_phase {
            m.failed_phase = v;
        }
        if let Some(v) = patch.base_fingerprint {
            m.base_fingerprint = Some(v);
        }
        if let Some(v) = patch.source_fingerprint {
            m.source_fingerprint = Some(v);
        }
        if let Some(v) = patch.target_fingerprint {
            m.target_fingerprint = Some(v);
        }
        if let Some(v) = patch.rebaselined {
            m.rebaselined = v;
        }
        if let Some(v) = patch.options {
            m.options = v;
        }
        Ok(true)
    }

    async fn touch_merge_claim(
        &self,
        id: uuid::Uuid,
        status: MergeStatus,
    ) -> Result<bool, MetadataError> {
        record(&self.journal, format!("metadata.touch_merge_claim {id}"));
        let mut state = self.read("touch_merge_claim")?;
        Ok(match state.merges.get_mut(&id) {
            Some(m) if m.status == status => {
                m.claimed_at = Some(Utc::now());
                true
            }
            _ => false,
        })
    }

    async fn append_merge_event(&self, event: &MergeEvent) -> Result<(), MetadataError> {
        record(
            &self.journal,
            format!(
                "metadata.append_merge_event {} {} {} {}",
                event.kind,
                event.phase.as_deref().unwrap_or("-"),
                event.step_id.as_deref().unwrap_or("-"),
                event.outcome.as_deref().unwrap_or("-")
            ),
        );
        let mut state = self.read("append_merge_event")?;
        let id = state.merge_events.len() as i64 + 1;
        state.merge_events.push(MergeEvent {
            id,
            at: Utc::now(),
            ..event.clone()
        });
        Ok(())
    }

    async fn merge_events(&self, id: uuid::Uuid) -> Result<Vec<MergeEvent>, MetadataError> {
        Ok(self
            .read("merge_events")?
            .merge_events
            .iter()
            .filter(|e| e.merge_id == id)
            .cloned()
            .collect())
    }

    async fn create_rehearsal_branch(
        &self,
        database_id: DatabaseId,
        parent: BranchId,
        name: &str,
    ) -> Result<BranchRecord, MetadataError> {
        record(
            &self.journal,
            format!("metadata.create_rehearsal_branch {name}"),
        );
        let mut state = self.read("create_rehearsal_branch")?;
        let parent_ready = state
            .branches
            .get(&parent)
            .is_some_and(|p| p.database_id == database_id && p.status == BranchStatus::Ready);
        if !parent_ready {
            return Err(MetadataError::Conflict);
        }
        if state.name_taken(database_id, name) {
            return Err(MetadataError::BranchNameTaken(name.to_string()));
        }
        let now = Utc::now();
        let row = BranchRecord {
            id: BranchId::new(),
            database_id,
            name: name.to_string(),
            parent_branch_id: Some(parent),
            kind: BranchKind::Rehearsal,
            status: BranchStatus::Pending,
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
            guard_logins: true,
            review_ack_hash: None,
            review_ack_keys: Vec::new(),
        };
        state.branches.insert(row.id, row.clone());
        Ok(row)
    }

    async fn stale_merge_claims(
        &self,
        older_than: Duration,
        rehand_after: Duration,
    ) -> Result<Vec<MergeRecord>, MetadataError> {
        let state = self.read("stale_merge_claims")?;
        let now = Utc::now();
        let cutoff = now - older_than;
        let mut rows: Vec<MergeRecord> = state
            .merges
            .values()
            .filter(|m| {
                let claimed = m.claimed_at.unwrap_or(m.updated_at);
                m.status.is_working()
                    && claimed < cutoff
                    && state
                        .handed_back
                        .get(&m.id)
                        .is_none_or(|at| *at < claimed || *at < now - rehand_after)
            })
            .cloned()
            .collect();
        rows.sort_by_key(|m| (m.created_at, m.id));
        Ok(rows)
    }

    async fn stale_finish_requests(
        &self,
        older_than: Duration,
        rehand_after: Duration,
    ) -> Result<Vec<MergeRecord>, MetadataError> {
        let state = self.read("stale_finish_requests")?;
        let now = Utc::now();
        let mut rows: Vec<MergeRecord> = state
            .merges
            .values()
            .filter(|m| {
                let waits = m.result.as_ref().is_some_and(|r| {
                    r["after"]["requested"] == serde_json::json!(true)
                        || r["after"]["started"] == serde_json::json!(true)
                });
                m.status == MergeStatus::AppliedWithDrift
                    && waits
                    && m.updated_at < now - older_than
                    && state
                        .handed_back
                        .get(&m.id)
                        .is_none_or(|at| *at < m.updated_at || *at < now - rehand_after)
            })
            .cloned()
            .collect();
        rows.sort_by_key(|m| (m.created_at, m.id));
        Ok(rows)
    }

    async fn mark_merge_handed_back(
        &self,
        id: uuid::Uuid,
        status: MergeStatus,
    ) -> Result<bool, MetadataError> {
        record(&self.journal, format!("metadata.mark_merge_handed_back {id}"));
        let mut state = self.read("mark_merge_handed_back")?;
        if state.merges.get(&id).is_none_or(|m| m.status != status) {
            return Ok(false);
        }
        state.handed_back.insert(id, Utc::now());
        Ok(true)
    }

    async fn cancel_drafts_for_branch(&self, branch: BranchId) -> Result<u64, MetadataError> {
        record(
            &self.journal,
            format!("metadata.cancel_drafts_for_branch {branch}"),
        );
        let mut state = self.read("cancel_drafts_for_branch")?;
        let mut n = 0;
        for m in state.merges.values_mut() {
            if (m.source_branch_id == branch.as_uuid() || m.target_branch_id == branch.as_uuid())
                && matches!(
                    m.status,
                    MergeStatus::Computing | MergeStatus::Draft | MergeStatus::Rehearsed
                )
            {
                m.status = MergeStatus::Cancelled;
                m.claimed_at = None;
                n += 1;
            }
        }
        Ok(n)
    }

    async fn list_merges(
        &self,
        database_id: DatabaseId,
        branch: Option<BranchId>,
    ) -> Result<Vec<MergeRecord>, MetadataError> {
        let state = self.read("list_merges")?;
        let mut rows: Vec<MergeRecord> = state
            .merges
            .values()
            .filter(|m| m.database_id == database_id.as_uuid())
            .filter(|m| {
                branch.is_none_or(|b| {
                    m.source_branch_id == b.as_uuid() || m.target_branch_id == b.as_uuid()
                })
            })
            .cloned()
            .collect();
        rows.sort_by_key(|m| (std::cmp::Reverse(m.created_at), m.id));
        Ok(rows)
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
        attributes_pending: false,
        created_at: at,
        attributes: RoleAttributes::default(),
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
            attributes_pending: l.attributes_pending,
            created_at: at,
            attributes: l.attributes.clone(),
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
    /// The safekeepers' commit LSN, when ahead of what the pageserver ingested.
    pub commit: Option<Lsn>,
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
    /// The method the storage controller answers 404 to, with this message.
    pub not_found: Mutex<Option<(&'static str, String)>>,
    /// How `detach_ancestor` ends.
    pub detach: Mutex<FakeDetach>,
    /// How long `detach_ancestor` takes.
    pub detach_delay: Mutex<Duration>,
    /// Runs once, at the start of the next `create_tenant` (e.g. to land a delete).
    pub on_create_tenant: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs once, at the start of the next `create_branch_timeline`.
    pub on_create_branch_timeline: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// The next `create_branch_timeline` waits this long first (a slow storage controller).
    pub create_branch_delay: Mutex<Option<Duration>>,
    /// What `lsn_by_timestamp` answers; `None` answers `present` at the timeline's head.
    pub lsn_at: Mutex<Option<LsnAtTime>>,
    /// What `timestamp_of_lsn` answers.
    pub timestamp_at: Mutex<Option<DateTime<Utc>>>,
    /// Each timeline's GC cutoff, once set (none otherwise).
    pub gc_cutoffs: Mutex<HashMap<NeonId, Lsn>>,
    /// How every `wait_attached` ends ([`worker`] sets `Attached`).
    pub attach_result: Mutex<AttachWait>,
    /// Runs once, at the start of the next `wait_attached` (e.g. to land a delete).
    pub on_wait_attached: Mutex<Option<Box<dyn FnOnce() + Send>>>,
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
            // What `check` in chelabase-neon makes of a 5xx.
            return Err(StorconError::Other(anyhow::anyhow!(
                "storage controller returned 503 Service Unavailable: {UNAVAILABLE_CAUSE}"
            )));
        }
        if let Some((m, message)) = lock(&self.not_found).clone()
            && m == method
        {
            return Err(StorconError::Rejected {
                status: reqwest::StatusCode::NOT_FOUND,
                message,
            });
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
                commit: None,
            },
        );
    }

    /// Makes the safekeepers' commit LSN of a timeline `commit` (it may be ahead of the head).
    pub fn set_commit_lsn(&self, tenant: &str, timeline: &str, commit: Lsn) {
        let key = (tenant.parse().unwrap(), timeline.parse().unwrap());
        lock(&self.timelines)
            .get_mut(&key)
            .expect("no such timeline")
            .commit = Some(commit);
    }

    fn head(&self, tenant: &NeonId, timeline: &NeonId) -> Result<Lsn, StorconError> {
        lock(&self.timelines)
            .get(&(tenant.clone(), timeline.clone()))
            .map(|t| head_lsn(t.head, t.commit))
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
                commit: None,
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
        let delay = lock(&self.create_branch_delay).take();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        self.call(
            "create_branch_timeline",
            format!("{timeline} from {parent} at {lsn}"),
        )?;
        if !lock(&self.tenants).contains(tenant) {
            return Err(Self::not_found(format!("tenant {tenant}")));
        }
        let parent_head = self.head(tenant, parent)?;
        // `parent_head` includes the parent's commit LSN. The real pageserver doesn't refuse a
        // start LSN ahead of its last record: it waits for the WAL to arrive (up to its wait
        // timeout). The fake refuses at once instead, so a test that branches at an LSN nobody
        // has written fails fast.
        if lsn > parent_head {
            return Err(StorconError::Rejected {
                status: reqwest::StatusCode::BAD_REQUEST,
                message: format!(
                    "invalid branch start lsn: {lsn} is ahead of the ancestor's head \
                     {parent_head} (last record or commit lsn)"
                ),
            });
        }
        lock(&self.timelines).insert(
            (tenant.clone(), timeline.clone()),
            FakeTimeline {
                ancestor: Some((parent.clone(), lsn)),
                head: lsn,
                commit: None,
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

    async fn ancestor(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<Option<(NeonId, Lsn)>, StorconError> {
        self.check("ancestor")?;
        match lock(&self.timelines).get(&(tenant.clone(), timeline.clone())) {
            Some(t) => Ok(t.ancestor.clone()),
            None => Err(Self::not_found(format!("timeline {timeline}"))),
        }
    }

    async fn gc_cutoff(
        &self,
        tenant: &NeonId,
        timeline: &NeonId,
    ) -> Result<Option<Lsn>, StorconError> {
        self.check("gc_cutoff")?;
        self.head(tenant, timeline)?;
        Ok(lock(&self.gc_cutoffs).get(timeline).copied())
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

    async fn wait_attached(&self, tenant: &NeonId, timeout: Duration) -> AttachWait {
        let hook = lock(&self.on_wait_attached).take();
        if let Some(hook) = hook {
            hook();
        }
        record(
            &self.journal,
            format!("storage.wait_attached {tenant} {}ms", timeout.as_millis()),
        );
        lock(&self.attach_result).clone()
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
    /// compute_ctl's status per endpoint; `Running` when not set.
    pub statuses: Mutex<std::collections::HashMap<String, ComputeCtlStatus>>,
    /// Statuses answered once each, in order, before `statuses` applies again (see
    /// [`FakePusher::script_statuses`]).
    pub status_queue: Mutex<std::collections::HashMap<String, std::collections::VecDeque<ComputeCtlStatus>>>,
    /// `status` fails with this message (compute_ctl unreachable).
    pub fail_status: Mutex<Option<String>>,
    /// `status` fails with this message once the scripted statuses are used up.
    pub fail_status_after_queue: Mutex<Option<String>>,
    /// The endpoints whose compute_ctl advertises `storage_token_refresh` (none by default: an
    /// old compute_ctl).
    pub capable: Mutex<std::collections::HashSet<String>>,
    /// `report` fails with this message (compute_ctl unreachable).
    pub fail_report: Mutex<Option<String>>,
    /// `report` never answers.
    pub hang_report: Mutex<bool>,
    /// While set, `report` panics (a bug anywhere in a refresh pass).
    pub panic_report: Mutex<bool>,
    /// `push_storage_token` never answers.
    pub hang_storage_token: Mutex<bool>,
    /// `push_storage_token` of the endpoint fails with the message.
    pub fail_storage_token: Mutex<std::collections::HashMap<String, String>>,
    /// `(endpoint, token)` of every storage token that got through.
    pub storage_tokens: Mutex<Vec<(String, String)>>,
    pub journal: Journal,
    /// A throwaway compute-JWT key pair (specs sign their storage token with it).
    pub signer: chelabase_neon::ComputeSigner,
}

/// A fresh compute-JWT key pair for fakes.
pub fn fake_signer() -> chelabase_neon::ComputeSigner {
    let (pem, x) = chelabase_neon::jwt::generate_keypair();
    chelabase_neon::ComputeSigner::new(&pem, &x).expect("a generated key pair")
}

impl FakePusher {
    pub fn set_status(&self, endpoint_id: &str, status: ComputeCtlStatus) {
        lock(&self.statuses).insert(endpoint_id.to_string(), status);
    }

    /// Whether `endpoint_id`'s compute_ctl advertises `storage_token_refresh`.
    pub fn set_capability(&self, endpoint_id: &str, capable: bool) {
        let mut set = lock(&self.capable);
        if capable {
            set.insert(endpoint_id.to_string());
        } else {
            set.remove(endpoint_id);
        }
    }

    /// `push_storage_token` of `endpoint_id` fails with `message`, or works again (`None`).
    pub fn fail_storage_token_for(&self, endpoint_id: &str, message: Option<String>) {
        let mut map = lock(&self.fail_storage_token);
        match message {
            Some(m) => {
                map.insert(endpoint_id.to_string(), m);
            }
            None => {
                map.remove(endpoint_id);
            }
        }
    }

    /// The next `statuses.len()` reads of `endpoint_id`'s status answer these, in order; later
    /// reads answer [`FakePusher::set_status`]'s value (`Running` when unset).
    pub fn script_statuses(&self, endpoint_id: &str, statuses: Vec<ComputeCtlStatus>) {
        lock(&self.status_queue).insert(endpoint_id.to_string(), statuses.into());
    }
}

impl SpecPusher for FakePusher {
    fn jwks(&self) -> serde_json::Value {
        serde_json::json!({ "keys": [] })
    }

    fn signer(&self) -> &chelabase_neon::ComputeSigner {
        &self.signer
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

    async fn status(
        &self,
        _control_addr: SocketAddr,
        endpoint_id: &str,
    ) -> anyhow::Result<ComputeCtlStatus> {
        if let Some(message) = lock(&self.fail_status).clone() {
            anyhow::bail!(message);
        }
        if let Some(next) = lock(&self.status_queue)
            .get_mut(endpoint_id)
            .and_then(|queue| queue.pop_front())
        {
            return Ok(next);
        }
        if let Some(message) = lock(&self.fail_status_after_queue).clone() {
            anyhow::bail!(message);
        }
        Ok(lock(&self.statuses)
            .get(endpoint_id)
            .cloned()
            .unwrap_or(ComputeCtlStatus::Running))
    }

    async fn report(
        &self,
        _control_addr: SocketAddr,
        endpoint_id: &str,
    ) -> anyhow::Result<chelabase_neon::reconfigure::ComputeCtlReport> {
        record(&self.journal, format!("pusher.report {endpoint_id}"));
        let panic = *lock(&self.panic_report);
        if panic {
            panic!("FakePusher::report: panic_report is set");
        }
        if *lock(&self.hang_report) {
            std::future::pending::<()>().await;
        }
        if let Some(message) = lock(&self.fail_report).clone() {
            anyhow::bail!(message);
        }
        let status = lock(&self.statuses)
            .get(endpoint_id)
            .cloned()
            .unwrap_or(ComputeCtlStatus::Running);
        Ok(chelabase_neon::reconfigure::ComputeCtlReport {
            status,
            storage_token_refresh: lock(&self.capable).contains(endpoint_id),
        })
    }

    async fn push_storage_token(
        &self,
        _control_addr: SocketAddr,
        endpoint_id: &str,
        token: &str,
    ) -> anyhow::Result<()> {
        record(&self.journal, format!("pusher.storage_token {endpoint_id}"));
        if *lock(&self.hang_storage_token) {
            std::future::pending::<()>().await;
        }
        if let Some(message) = lock(&self.fail_storage_token).get(endpoint_id).cloned() {
            anyhow::bail!(message);
        }
        lock(&self.storage_tokens).push((endpoint_id.to_string(), token.to_string()));
        Ok(())
    }
}

pub type FakeWorker = Worker<FakeMetadata, FakeStorage, FakeBackend, FakeCache, FakePusher>;

/// Tests that check their warnings capture logs with a thread-local subscriber (`set_default`)
/// while other tests run without one. While tracing-core knows of at most one live dispatcher,
/// it computes a newly hit callsite's interest from the registering thread's default alone, so
/// a `warn!` first hit by a test without a subscriber is cached as "never" for every thread, the
/// capturing test's included, and its check fails now and then. Two dispatchers that live for
/// the whole test process make tracing-core ask every live dispatcher instead.
fn pin_tracing_dispatchers() {
    use tracing::Dispatch;
    use tracing::subscriber::NoSubscriber;
    static PINNED: std::sync::OnceLock<[Dispatch; 2]> = std::sync::OnceLock::new();
    PINNED.get_or_init(|| {
        [
            Dispatch::new(NoSubscriber::default()),
            Dispatch::new(NoSubscriber::default()),
        ]
    });
}

/// A worker over fresh fakes sharing one journal, with millisecond readiness polling.
pub fn worker() -> (FakeWorker, Journal) {
    pin_tracing_dispatchers();
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
            not_found: Mutex::default(),
            detach: Mutex::default(),
            detach_delay: Mutex::default(),
            on_create_tenant: Mutex::default(),
            on_create_branch_timeline: Mutex::default(),
            create_branch_delay: Mutex::default(),
            lsn_at: Mutex::default(),
            timestamp_at: Mutex::default(),
            gc_cutoffs: Mutex::default(),
            attach_result: Mutex::new(AttachWait::Attached),
            on_wait_attached: Mutex::default(),
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
            statuses: Mutex::default(),
            status_queue: Mutex::default(),
            fail_status: Mutex::default(),
            fail_status_after_queue: Mutex::default(),
            capable: Mutex::default(),
            fail_report: Mutex::default(),
            hang_report: Mutex::default(),
            panic_report: Mutex::default(),
            hang_storage_token: Mutex::default(),
            fail_storage_token: Mutex::default(),
            storage_tokens: Mutex::default(),
            journal: journal.clone(),
            signer: fake_signer(),
        },
        key: SecretKey::generate(),
        owner_superuser: false,
        roles: chelabase_types::role_settings::RoleSettings {
            role_cap: 10,
            database_cap: 20,
            strength: false,
            prehashed_allowed: true,
        },
        control_plane_uri: "http://orchestrator:8083".to_string(),
        ready_poll: Duration::from_millis(1),
        ready_timeout: Duration::from_millis(20),
        // No longer than the timeout: the wait extends only when a test raises it.
        ready_cap: Duration::from_millis(20),
        // The fake's wait answers at once; tests that look at the timeout set their own.
        attach_timeout: crate::provision::ATTACH_TIMEOUT,
        claim_refresh: Duration::from_millis(5),
        schema_check_cursor: Mutex::default(),
        enabling_seen: Mutex::default(),
        recreate_backoff: Mutex::default(),
        fn_sweeps: Mutex::default(),
        identity: None,
        app_users_reconcile_every: Duration::from_secs(300),
        member_roles_sweep_every: Duration::from_secs(300),
        remove_orphans: true,
        merge_tx_timeout: crate::merge::MERGE_TX_TIMEOUT,
        merge_online_step_timeout: crate::merge::MERGE_ONLINE_STEP_TIMEOUT,
        resync_requests: Default::default(),
        member_role_requests: Default::default(),
        member_logins: chelabase_neon::spec::MemberLogins::orchestrator(Vec::new()),
        cert_sweep: None,
        realtime_logins: chelabase_neon::spec::MemberLogins::realtime(Vec::new()),
        storage_token_refresh_every: Duration::from_secs(3600),
        storage_token_failures: Mutex::default(),
        realtime_password: None,
        blob_store: None,
        storage_sweep: Default::default(),
        sweep_requests: Default::default(),
        storage_sweep_refusals: Default::default(),
        storage_sweep_restore_voids: Default::default(),
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
        "0/1",
    ))
    .expect("the fake never waits");
    assert!(switched.unwrap(), "switched");
    w.metadata.state().branches.get_mut(&branch).unwrap().status = BranchStatus::Ready;
    backup
}

#[cfg(test)]
mod tests {
    use super::*;

    // PB T1 review, finding 1: the fake restore's backup carries the restored branch's switch,
    // as the real insert does.
    #[tokio::test]
    async fn a_restore_backup_copies_guard_logins() {
        use crate::ports::WorkerMetadata as _;
        let (w, _journal) = worker();
        let db = ready_database(&w).await;
        let main = w.metadata.main(db);
        {
            let mut state = w.metadata.state();
            let record = state.branches.get_mut(&main.id).unwrap();
            record.guard_logins = false;
            record.status = BranchStatus::Resetting;
            record.pending_timeline_id = Some("tl-restored".into());
        }
        let backup = BranchId::new();
        assert!(w.metadata.switch_to_restored_timeline(main.id, "tl-restored", backup, "main-before-restore", "0/1").await.unwrap());
        assert!(!w.metadata.state().branches[&backup].guard_logins);
    }

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
