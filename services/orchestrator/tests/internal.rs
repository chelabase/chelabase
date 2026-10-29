//! The internal listener (proxy, compute_ctl and storage-controller routes) with fake
//! metadata, a fake compute locator, and in-process stand-ins for the storage controller and
//! computes.

// Test code: plain clients against local test servers (clippy.toml allows tests only).
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use axum::Router;
use axum::http::StatusCode as AxumStatus;
use axum::routing::get;
use chelabase_metadata::{
    AppSession, AppUsersStatus, BranchAuthSync, BranchConnectionRow, BranchKind, BranchRecord,
    ServedDatabase, BranchStatus, DataApiSettings, DataKeyLookup, DatabaseOrigin, MetadataError,
    NeonDatabase, NeonEndpoint, NeonRole, NewAppSession, OnDelete, ProjectAuth, ProjectRecord,
    ProjectStatus, RefreshNext, RefreshOutcome, RoleCopy, RoleLevel, RoleOrigin,
};
use chelabase_metadata::members::InviteDescription;
use chelabase_metadata::personal_logins::LoginEndpoint;
use chelabase_nats::{AppUsersSyncJob, JobEnvelope};
use chelabase_types::{DatabaseId, ProjectId, TenantId};
use chelabase_zitadel::client::Zitadel;
use chelabase_zitadel::mock::MockZitadel;
use orchestrator::internal::app::AppJobPort;
use chelabase_secrets::{Purpose, SecretKey, seal_credential};
use chelabase_types::BranchId;
use chelabase_types::data_key::DataKeyKind;
use chelabase_neon::jwt::generate_keypair;
use chelabase_neon::{ComputeLocator, ComputeSigner, StorconClient};
use chelabase_types::mode::Mode;
use chelabase_types::role_settings::RoleSettings;
use orchestrator::internal::{
    BoxFuture, CachePort, DdlPort, InternalState, NeonMetadataPort, ResyncPort,
    build_internal_router,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use tokio::net::TcpListener;

const PROXY_TOKEN: &str = "proxy-token";
const STORCON_TOKEN: &str = "storcon-token";
/// The compute token for every endpoint `FakeNeon::add` creates.
const COMPUTE_TOKEN: &str = "compute-token";
const DATA_KEYS_TOKEN: &str = "data-keys-token";
const REST_TOKEN: &str = "rest-token";
const CONTROL_PLANE_URI: &str = "http://orchestrator:8083";
const APP_AUTH_TOKEN: &str = "app-auth-token";
const APP_SESSIONS_TOKEN: &str = "app-sessions-token";
const SIGNING_KEY: &str = "actions-signing-key";
const REFRESH_TTL: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(s.as_bytes()))
}

type KeyEntry = ([u8; 32], DataKeyLookup);
/// Members hub T8: personal-login rows by (tenant, Zitadel user id).
type LoginRows = HashMap<(TenantId, String), Vec<LoginEndpoint>>;

#[derive(Clone, Default)]
struct FakeNeon {
    endpoints: Arc<Mutex<Vec<NeonEndpoint>>>,
    /// (endpoint id, role)
    roles: Arc<Mutex<Vec<(String, NeonRole)>>>,
    fail: Arc<Mutex<bool>>,
    /// Endpoints still listed for their tenant and timeline but gone when fetched.
    vanished: Arc<Mutex<Vec<String>>>,
    /// (sha256, lookup)
    data_keys: Arc<Mutex<Vec<KeyEntry>>>,
    /// Key ids `touch_data_key` was called with.
    touched: Arc<Mutex<Vec<uuid::Uuid>>>,
    connections: Arc<Mutex<HashMap<BranchId, BranchConnectionRow>>>,
    /// What `begin_ddl` sees, per endpoint; a transaction works on a copy and `commit`
    /// writes it back.
    ddl: Arc<Mutex<HashMap<String, DdlState>>>,
    /// How many DDL transactions were opened.
    begins: Arc<AtomicUsize>,
    /// `begin_ddl` answers a lock timeout.
    ddl_lock_timeout: Arc<Mutex<bool>>,
    /// App users (step 6b).
    app: Arc<Mutex<AppRows>>,
    /// Zitadel org id -> (open sign-up, has members) for the sign-up gate.
    signup: Arc<Mutex<HashMap<String, (bool, bool)>>>,
    /// Selfhost-install m3: claim nonces and claims, by Zitadel org.
    setup: Arc<Mutex<FakeSetup>>,
    /// Zitadel user id -> the user's open invite (a closed one is simply absent).
    invites: Arc<Mutex<HashMap<String, InviteDescription>>>,
    /// RT1 T6: Zitadel org id -> the tenant bound to it.
    tenants: Arc<Mutex<HashMap<String, TenantId>>>,
    /// Members hub T8: each (tenant, Zitadel user id)'s personal-login rows, which
    /// `revoke_logins_for_zitadel_user` answers (every row of the member, revoked ones
    /// included; an unknown user has none); each revocation asked for, in order; and while
    /// `fail_revocation` is set, the revocation write fails (a database error).
    login_rows: Arc<Mutex<LoginRows>>,
    revocations: Arc<Mutex<Vec<(TenantId, String)>>>,
    fail_revocation: Arc<Mutex<bool>>,
    /// Final round A: each offboarded mark cleared (D7), a clear that fails while
    /// `fail_clear` is set, and a tenant lookup that fails while `fail_tenant_lookup` is (T8-2).
    clears: Arc<Mutex<Vec<(TenantId, String)>>>,
    fail_clear: Arc<Mutex<bool>>,
    fail_tenant_lookup: Arc<Mutex<bool>>,
}

/// The fake's fresh-install claim rows: a model of `claim_setup`'s rules (the real ones are
/// tested live, `members_live::setup_claim_*`).
#[derive(Default)]
struct FakeSetup {
    /// nonce SHA-256 -> (org, still live)
    nonces: HashMap<[u8; 32], (String, bool)>,
    /// org -> the claim's Zitadel user
    claims: HashMap<String, String>,
    /// Zitadel users with a member row.
    member_rows: std::collections::HashSet<String>,
    /// Every `replace_stale` `claim_setup` was called with.
    replace_asked: Vec<Option<String>>,
}

impl FakeNeon {
    fn setup_claim_of(&self, org: &str) -> Option<String> {
        self.setup.lock().unwrap().claims.get(org).cloned()
    }

    fn live_nonces(&self, org: &str) -> Vec<[u8; 32]> {
        let st = self.setup.lock().unwrap();
        st.nonces.iter().filter(|(_, (o, live))| o == org && *live).map(|(h, _)| *h).collect()
    }

    fn expire_nonces(&self) {
        for (_, live) in self.setup.lock().unwrap().nonces.values_mut() {
            *live = false;
        }
    }
}

/// The app-user rows the fake holds.
#[derive(Default)]
struct AppRows {
    projects: Vec<ProjectRecord>,
    auths: Vec<ProjectAuth>,
    branches: Vec<BranchRecord>,
    db_projects: HashMap<DatabaseId, ProjectId>,
    syncs: HashMap<BranchId, BranchAuthSync>,
    sessions: Vec<FakeSession>,
}

/// A session row with its token hashes, as T1's table keeps it.
#[derive(Clone)]
struct FakeSession {
    s: AppSession,
    hash: [u8; 32],
    prev: Option<[u8; 32]>,
    sealed: Option<String>,
    grace_until: Option<chrono::DateTime<chrono::Utc>>,
}

/// One endpoint's rows as the DDL receiver's transaction sees them.
#[derive(Clone, Debug, Default, PartialEq)]
struct DdlState {
    owner: String,
    branch: Option<BranchId>,
    main_db: Option<String>,
    roles: BTreeMap<String, RoleCopy>,
    dbs: BTreeMap<String, (String, DatabaseOrigin)>,
    /// (id, name, role, revoked)
    keys: Vec<(uuid::Uuid, String, String, bool)>,
    /// `set_owner_verifier` cleared the branch's sealed owner password.
    owner_password_cleared: bool,
    /// Rows `mark_password_unreadable` was called for.
    unreadable: std::collections::BTreeSet<String>,
    /// Rows `set_login` stamped with `forwarded_at`.
    stamped: std::collections::BTreeSet<String>,
    roles_version: u64,
}

fn copy(name: &str, origin: RoleOrigin, verifier: Option<&str>, sealed: Option<&str>) -> RoleCopy {
    RoleCopy {
        name: name.into(),
        origin,
        scram_verifier: verifier.map(str::to_string),
        password_enc: sealed.map(str::to_string),
        login: true,
        level: RoleLevel::Plain,
        api_owner_level: false,
        pending: false,
        attributes_pending: false,
        created_at: chrono::Utc::now(),
        attributes: chelabase_metadata::RoleAttributes::default(),
    }
}

/// What Postgres answers a row moved onto a taken primary key.
fn duplicate_key() -> MetadataError {
    MetadataError::Config("duplicate key value violates unique constraint".into())
}

struct FakeDdl {
    endpoint: String,
    state: DdlState,
    store: Arc<Mutex<HashMap<String, DdlState>>>,
    wrote: bool,
}

impl DdlPort for FakeDdl {
    fn branch_id(&self) -> Option<BranchId> {
        self.state.branch
    }
    fn owner(&self) -> &str {
        &self.state.owner
    }
    fn main_database(&self) -> Option<&str> {
        self.state.main_db.as_deref()
    }
    async fn role(&mut self, name: &str) -> Result<Option<RoleCopy>, MetadataError> {
        Ok(self.state.roles.get(name).cloned())
    }
    async fn copy_count(&mut self) -> Result<u32, MetadataError> {
        Ok(self
            .state
            .roles
            .values()
            .filter(|r| r.origin != RoleOrigin::Owner && r.scram_verifier.is_some())
            .count() as u32)
    }
    async fn live_secret_keys(
        &mut self,
        role: &str,
    ) -> Result<Vec<(uuid::Uuid, String)>, MetadataError> {
        if self.state.branch.is_none() {
            return Ok(vec![]);
        }
        Ok(self
            .state
            .keys
            .iter()
            .filter(|(_, _, r, revoked)| r == role && !revoked)
            .map(|(id, name, _, _)| (*id, name.clone()))
            .collect())
    }
    async fn upsert_sql_copy(&mut self, name: &str, verifier: &str) -> Result<(), MetadataError> {
        match self.state.roles.get_mut(name) {
            Some(r) if r.origin == RoleOrigin::Owner => {}
            Some(r) => {
                r.scram_verifier = Some(verifier.into());
                r.password_enc = None;
            }
            None => {
                self.state
                    .roles
                    .insert(name.into(), copy(name, RoleOrigin::Sql, Some(verifier), None));
            }
        }
        self.wrote = true;
        Ok(())
    }
    async fn set_login(&mut self, name: &str, login: bool) -> Result<bool, MetadataError> {
        match self.state.roles.get_mut(name) {
            Some(r) if r.origin != RoleOrigin::Owner => {
                r.login = login;
                self.wrote = true;
                self.state.stamped.insert(name.into());
                Ok(true)
            }
            _ => Ok(false),
        }
    }
    async fn set_owner_verifier(&mut self, verifier: &str) -> Result<(), MetadataError> {
        for r in self.state.roles.values_mut() {
            if r.origin == RoleOrigin::Owner {
                r.scram_verifier = Some(verifier.into());
            }
        }
        if self.state.branch.is_some() {
            self.state.owner_password_cleared = true;
        }
        self.wrote = true;
        Ok(())
    }
    async fn mark_password_unreadable(&mut self, name: &str) -> Result<(), MetadataError> {
        if let Some(r) = self.state.roles.get_mut(name) {
            r.password_enc = None;
            self.state.unreadable.insert(name.into());
            self.wrote = true;
        }
        Ok(())
    }
    async fn remove_copy(&mut self, name: &str) -> Result<bool, MetadataError> {
        let removable = self
            .state
            .roles
            .get(name)
            .is_some_and(|r| r.origin != RoleOrigin::Owner);
        if removable {
            self.state.roles.remove(name);
            self.wrote = true;
        }
        Ok(removable)
    }
    async fn upsert_no_password(&mut self, name: &str) -> Result<(), MetadataError> {
        match self.state.roles.get_mut(name) {
            Some(r) if r.origin == RoleOrigin::Sql => {
                r.scram_verifier = None;
                r.password_enc = None;
            }
            Some(_) => {}
            None => {
                let mut row = copy(name, RoleOrigin::Sql, None, None);
                row.login = false;
                self.state.roles.insert(name.into(), row);
            }
        }
        self.state.stamped.insert(name.into());
        self.wrote = true;
        Ok(())
    }
    async fn rename_copy(
        &mut self,
        old: &str,
        new: &str,
        resealed: Option<&str>,
    ) -> Result<bool, MetadataError> {
        let row_moves = self.state.roles.contains_key(old);
        if row_moves && self.state.roles.contains_key(new) {
            return Err(duplicate_key());
        }
        if row_moves {
            let mut row = self.state.roles.remove(old).unwrap();
            row.name = new.into();
            row.password_enc = resealed.map(str::to_string);
            self.state.roles.insert(new.into(), row);
        }
        // The keys follow the name, row or no row.
        if self.state.branch.is_some() {
            for key in &mut self.state.keys {
                if key.2 == old {
                    key.2 = new.into();
                    self.wrote = true;
                }
            }
        }
        self.wrote |= row_moves;
        Ok(row_moves)
    }
    async fn has_database(&mut self, name: &str) -> Result<bool, MetadataError> {
        Ok(self.state.branch.is_some() && self.state.dbs.contains_key(name))
    }
    async fn database_count(&mut self) -> Result<u32, MetadataError> {
        if self.state.branch.is_none() {
            return Ok(0);
        }
        Ok(self
            .state
            .dbs
            .values()
            .filter(|(_, o)| *o != DatabaseOrigin::System)
            .count() as u32)
    }
    async fn upsert_database(&mut self, name: &str, owner: &str) -> Result<(), MetadataError> {
        if self.state.branch.is_none() {
            return Ok(());
        }
        let origin = if self.state.main_db.as_deref() == Some(name) {
            DatabaseOrigin::Main
        } else {
            DatabaseOrigin::Sql
        };
        let entry = self
            .state
            .dbs
            .entry(name.into())
            .or_insert((owner.into(), origin));
        entry.0 = owner.into();
        self.wrote = true;
        Ok(())
    }
    async fn rename_database(&mut self, old: &str, new: &str) -> Result<bool, MetadataError> {
        if self.state.branch.is_none() {
            return Ok(false);
        }
        if !self.state.dbs.contains_key(old) {
            return Ok(false);
        }
        if self.state.dbs.contains_key(new) {
            return Err(duplicate_key());
        }
        let row = self.state.dbs.remove(old).unwrap();
        self.state.dbs.insert(new.into(), row);
        self.wrote = true;
        Ok(true)
    }
    async fn remove_database(&mut self, name: &str) -> Result<bool, MetadataError> {
        if self.state.branch.is_none() {
            return Ok(false);
        }
        let removed = self.state.dbs.remove(name).is_some();
        self.wrote |= removed;
        Ok(removed)
    }
    async fn commit(mut self) -> Result<(), MetadataError> {
        if self.wrote {
            self.state.roles_version += 1;
        }
        self.store
            .lock()
            .unwrap()
            .insert(self.endpoint.clone(), self.state);
        Ok(())
    }
}

impl FakeNeon {
    /// An endpoint with role and database `app`.
    fn add(&self, id: &str, tenant: &str, timeline: &str) {
        self.add_with_token(id, tenant, timeline, COMPUTE_TOKEN);
    }

    fn add_with_token(&self, id: &str, tenant: &str, timeline: &str, token: &str) {
        self.endpoints.lock().unwrap().push(NeonEndpoint {
            id: id.into(),
            tenant_id: tenant.into(),
            timeline_id: timeline.into(),
            compute_token_sha256: sha256_hex(token),
            routable: true,
            static_lsn: None,
        });
        self.roles.lock().unwrap().push((
            id.into(),
            NeonRole {
                name: "app".into(),
                scram_verifier: format!("SCRAM-SHA-256$verifier-of-{id}"),
            },
        ));
    }

    /// Marks an added endpoint unroutable and/or pinned at an LSN.
    fn set_routing(&self, id: &str, routable: bool, static_lsn: Option<&str>) {
        let mut endpoints = self.endpoints.lock().unwrap();
        let e = endpoints.iter_mut().find(|e| e.id == id).unwrap();
        e.routable = routable;
        e.static_lsn = static_lsn.map(str::to_string);
    }

    fn break_it(&self) {
        *self.fail.lock().unwrap() = true;
    }

    /// From now on `neon_endpoint` doesn't find `id`, though the lists still name it.
    fn vanish(&self, id: &str) {
        self.vanished.lock().unwrap().push(id.into());
    }

    fn check(&self) -> Result<(), MetadataError> {
        if *self.fail.lock().unwrap() {
            return Err(MetadataError::Config("metadata is down".into()));
        }
        Ok(())
    }
}

impl NeonMetadataPort for FakeNeon {
    type Ddl = FakeDdl;

    async fn begin_ddl(&self, endpoint_id: &str) -> Result<Option<FakeDdl>, MetadataError> {
        self.begins.fetch_add(1, Ordering::SeqCst);
        self.check()?;
        if *self.ddl_lock_timeout.lock().unwrap() {
            return Err(MetadataError::EndpointLockTimeout(endpoint_id.into()));
        }
        let state = self.ddl.lock().unwrap().get(endpoint_id).cloned();
        Ok(state.map(|state| FakeDdl {
            endpoint: endpoint_id.into(),
            state,
            store: self.ddl.clone(),
            wrote: false,
        }))
    }

    async fn lookup_data_key(&self, sha256: &[u8; 32]) -> Result<Option<DataKeyLookup>, MetadataError> {
        self.check()?;
        Ok(self
            .data_keys
            .lock()
            .unwrap()
            .iter()
            .find(|(h, _)| h == sha256)
            .map(|(_, l)| l.clone()))
    }

    async fn touch_data_key(&self, key_id: uuid::Uuid) -> Result<(), MetadataError> {
        self.touched.lock().unwrap().push(key_id);
        Ok(())
    }

    async fn branch_connection(
        &self,
        branch: BranchId,
    ) -> Result<Option<BranchConnectionRow>, MetadataError> {
        self.check()?;
        Ok(self.connections.lock().unwrap().get(&branch).cloned())
    }

    async fn neon_endpoint(&self, id: &str) -> Result<Option<NeonEndpoint>, MetadataError> {
        self.check()?;
        if self.vanished.lock().unwrap().iter().any(|v| v == id) {
            return Ok(None);
        }
        Ok(self
            .endpoints
            .lock()
            .unwrap()
            .iter()
            .find(|e| e.id == id)
            .cloned())
    }

    async fn routable_endpoint(&self, id: &str) -> Result<Option<NeonEndpoint>, MetadataError> {
        Ok(self.neon_endpoint(id).await?.filter(|e| e.routable))
    }

    async fn routable_role_secret(
        &self,
        endpoint_id: &str,
        role: &str,
    ) -> Result<Option<Option<String>>, MetadataError> {
        if self.routable_endpoint(endpoint_id).await?.is_none() {
            return Ok(None);
        }
        self.neon_role_secret(endpoint_id, role).await
    }

    async fn neon_role_secret(
        &self,
        endpoint_id: &str,
        role: &str,
    ) -> Result<Option<Option<String>>, MetadataError> {
        if self.neon_endpoint(endpoint_id).await?.is_none() {
            return Ok(None);
        }
        Ok(Some(
            self.roles
                .lock()
                .unwrap()
                .iter()
                .find(|(ep, r)| ep == endpoint_id && r.name == role)
                .map(|(_, r)| r.scram_verifier.clone()),
        ))
    }

    async fn neon_roles(&self, endpoint_id: &str) -> Result<Vec<NeonRole>, MetadataError> {
        self.check()?;
        Ok(self
            .roles
            .lock()
            .unwrap()
            .iter()
            .filter(|(ep, _)| ep == endpoint_id)
            .map(|(_, r)| r.clone())
            .collect())
    }

    async fn neon_databases(&self, endpoint_id: &str) -> Result<Vec<NeonDatabase>, MetadataError> {
        self.check()?;
        Ok(if self.neon_endpoint(endpoint_id).await?.is_some() {
            vec![NeonDatabase {
                name: "app".into(),
                owner: "app".into(),
            }]
        } else {
            vec![]
        })
    }

    async fn endpoint_branch(
        &self,
        endpoint_id: &str,
    ) -> Result<Option<BranchId>, MetadataError> {
        self.check()?;
        Ok(self.ddl.lock().unwrap().get(endpoint_id).and_then(|s| s.branch))
    }

    async fn neon_endpoints_for_tenant(
        &self,
        tenant_id: &str,
    ) -> Result<Vec<String>, MetadataError> {
        self.check()?;
        Ok(self
            .endpoints
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.tenant_id == tenant_id)
            .map(|e| e.id.clone())
            .collect())
    }

    async fn neon_endpoints_for_timeline(
        &self,
        timeline_id: &str,
    ) -> Result<Vec<String>, MetadataError> {
        self.check()?;
        Ok(self
            .endpoints
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.timeline_id == timeline_id)
            .map(|e| e.id.clone())
            .collect())
    }

    async fn project_by_id(&self, id: ProjectId) -> Result<Option<ProjectRecord>, MetadataError> {
        self.check()?;
        Ok(self.app.lock().unwrap().projects.iter().find(|p| p.id == id).cloned())
    }

    async fn project_auth(&self, project: ProjectId) -> Result<Option<ProjectAuth>, MetadataError> {
        self.check()?;
        Ok(self
            .app
            .lock()
            .unwrap()
            .auths
            .iter()
            .find(|a| a.project_id == project)
            .cloned())
    }

    async fn project_auth_by_org(&self, org_id: &str) -> Result<Option<ProjectAuth>, MetadataError> {
        self.check()?;
        Ok(self
            .app
            .lock()
            .unwrap()
            .auths
            .iter()
            .find(|a| a.zitadel_org_id.as_deref() == Some(org_id))
            .cloned())
    }

    async fn signup_facts(
        &self,
        zitadel_org_id: &str,
    ) -> Result<Option<orchestrator::internal::SignUpFacts>, MetadataError> {
        self.check()?;
        Ok(self
            .signup
            .lock()
            .unwrap()
            .get(zitadel_org_id)
            .map(|(open, has_members)| orchestrator::internal::SignUpFacts {
                open: *open,
                has_members: *has_members,
            }))
    }

    async fn insert_setup_nonce(
        &self,
        zitadel_org_id: &str,
        nonce_hash: [u8; 32],
        _ttl: std::time::Duration,
    ) -> Result<bool, MetadataError> {
        self.check()?;
        if !self.signup.lock().unwrap().contains_key(zitadel_org_id) {
            return Ok(false);
        }
        self.setup.lock().unwrap().nonces.insert(nonce_hash, (zitadel_org_id.into(), true));
        Ok(true)
    }

    async fn setup_claim(
        &self,
        zitadel_org_id: &str,
    ) -> Result<Option<chelabase_metadata::members::SetupClaim>, MetadataError> {
        self.check()?;
        let st = self.setup.lock().unwrap();
        Ok(st.claims.get(zitadel_org_id).map(|user| chelabase_metadata::members::SetupClaim {
            zitadel_user_id: user.clone(),
            created_at: chrono::Utc::now(),
            has_member_row: st.member_rows.contains(user),
        }))
    }

    async fn claim_setup(
        &self,
        zitadel_org_id: &str,
        nonce_hash: [u8; 32],
        zitadel_user_id: &str,
        replace_stale: Option<&str>,
    ) -> Result<Option<chelabase_metadata::members::SetupClaimOutcome>, MetadataError> {
        use chelabase_metadata::members::SetupClaimOutcome::{AlreadyClaimed, Bound, Refused};
        self.check()?;
        if !self.signup.lock().unwrap().contains_key(zitadel_org_id) {
            return Ok(None);
        }
        let mut st = self.setup.lock().unwrap();
        st.replace_asked.push(replace_stale.map(str::to_string));
        match st.nonces.get(&nonce_hash) {
            Some((org, true)) if org == zitadel_org_id => {}
            _ => return Ok(Some(Refused)),
        }
        st.nonces.remove(&nonce_hash);
        let outcome = match st.claims.get(zitadel_org_id).cloned() {
            None => Bound,
            Some(u) if u == zitadel_user_id => Bound,
            Some(u) if replace_stale == Some(u.as_str()) && !st.member_rows.contains(&u) => Bound,
            Some(_) => AlreadyClaimed,
        };
        if outcome == Bound {
            st.claims.insert(zitadel_org_id.into(), zitadel_user_id.into());
        }
        Ok(Some(outcome))
    }

    async fn open_invite_for_user(&self, zitadel_user_id: &str) -> Result<Option<InviteDescription>, MetadataError> {
        self.check()?;
        Ok(self.invites.lock().unwrap().get(zitadel_user_id).cloned())
    }

    async fn tenant_by_org(&self, zitadel_org_id: &str) -> Result<Option<TenantId>, MetadataError> {
        self.check()?;
        if *self.fail_tenant_lookup.lock().unwrap() {
            return Err(MetadataError::Config("the tenant lookup failed".into()));
        }
        Ok(self.tenants.lock().unwrap().get(zitadel_org_id).copied())
    }

    async fn clear_offboarded(&self, tenant: TenantId, zitadel_user_id: &str) -> Result<bool, MetadataError> {
        self.check()?;
        if *self.fail_clear.lock().unwrap() {
            return Err(MetadataError::Config("the clear failed".into()));
        }
        self.clears.lock().unwrap().push((tenant, zitadel_user_id.to_string()));
        Ok(true)
    }

    async fn revoke_logins_for_zitadel_user(
        &self,
        tenant: TenantId,
        zitadel_user_id: &str,
    ) -> Result<Vec<LoginEndpoint>, MetadataError> {
        self.check()?;
        if *self.fail_revocation.lock().unwrap() {
            return Err(MetadataError::Config("the revocation write failed".into()));
        }
        self.revocations.lock().unwrap().push((tenant, zitadel_user_id.to_string()));
        Ok(self
            .login_rows
            .lock()
            .unwrap()
            .get(&(tenant, zitadel_user_id.to_string()))
            .cloned()
            .unwrap_or_default())
    }

    async fn get_branch(&self, id: BranchId) -> Result<Option<BranchRecord>, MetadataError> {
        self.check()?;
        Ok(self.app.lock().unwrap().branches.iter().find(|b| b.id == id).cloned())
    }

    async fn database_project(&self, db: DatabaseId) -> Result<Option<ProjectId>, MetadataError> {
        self.check()?;
        Ok(self.app.lock().unwrap().db_projects.get(&db).copied())
    }

    async fn main_branch(&self, db: DatabaseId) -> Result<Option<BranchRecord>, MetadataError> {
        self.check()?;
        Ok(self
            .app
            .lock()
            .unwrap()
            .branches
            .iter()
            .find(|b| {
                b.database_id == db
                    && b.parent_branch_id.is_none()
                    && b.status != BranchStatus::Deleted
            })
            .cloned())
    }

    async fn branch_auth_sync(
        &self,
        branch: BranchId,
    ) -> Result<Option<BranchAuthSync>, MetadataError> {
        self.check()?;
        Ok(self.app.lock().unwrap().syncs.get(&branch).cloned())
    }

    async fn insert_app_session(&self, n: &NewAppSession<'_>) -> Result<(), MetadataError> {
        self.check()?;
        let now = chrono::Utc::now();
        self.app.lock().unwrap().sessions.push(FakeSession {
            s: AppSession {
                id: n.id,
                project_id: n.project_id,
                user_id: n.user_id,
                zitadel_session_id: n.zitadel_session_id.into(),
                created_at: now,
                last_used_at: now,
                expires_at: n.expires_at,
                revoked_at: None,
            },
            hash: n.refresh_hash,
            prev: None,
            sealed: None,
            grace_until: None,
        });
        Ok(())
    }

    async fn app_session(&self, id: uuid::Uuid) -> Result<Option<AppSession>, MetadataError> {
        self.check()?;
        Ok(self
            .app
            .lock()
            .unwrap()
            .sessions
            .iter()
            .find(|f| f.s.id == id)
            .map(|f| f.s.clone()))
    }

    /// T1's rules, on the in-memory row.
    async fn refresh_app_session(
        &self,
        id: uuid::Uuid,
        project: ProjectId,
        presented: [u8; 32],
        next: &RefreshNext,
    ) -> Result<RefreshOutcome, MetadataError> {
        self.check()?;
        let mut app = self.app.lock().unwrap();
        let Some(f) = app.sessions.iter_mut().find(|f| f.s.id == id) else {
            return Ok(RefreshOutcome::Invalid);
        };
        if f.s.project_id != project || f.s.revoked_at.is_some() {
            return Ok(RefreshOutcome::Invalid);
        }
        let now = chrono::Utc::now();
        let cap_end = f.s.created_at + chrono::Duration::from_std(next.cap).unwrap();
        if now >= f.s.expires_at || now >= cap_end {
            f.s.revoked_at = Some(now);
            return Ok(RefreshOutcome::Invalid);
        }
        if f.hash == presented {
            f.prev = Some(f.hash);
            f.hash = next.new_hash;
            f.sealed = Some(next.new_sealed.clone());
            f.grace_until = Some(now + chrono::Duration::from_std(next.grace).unwrap());
            f.s.last_used_at = now;
            f.s.expires_at = (now + chrono::Duration::from_std(next.sliding).unwrap()).min(cap_end);
            Ok(RefreshOutcome::Rotated(f.s.clone()))
        } else if f.prev == Some(presented) {
            match (f.grace_until, &f.sealed) {
                (Some(until), Some(sealed)) if until > now => Ok(RefreshOutcome::Grace {
                    session: f.s.clone(),
                    sealed: sealed.clone(),
                }),
                _ => {
                    f.s.revoked_at = Some(now);
                    Ok(RefreshOutcome::Reused(f.s.clone()))
                }
            }
        } else {
            Ok(RefreshOutcome::Invalid)
        }
    }

    async fn revoke_app_session(
        &self,
        id: uuid::Uuid,
        project: ProjectId,
    ) -> Result<Option<AppSession>, MetadataError> {
        self.check()?;
        let mut app = self.app.lock().unwrap();
        Ok(app
            .sessions
            .iter_mut()
            .find(|f| f.s.id == id && f.s.project_id == project && f.s.revoked_at.is_none())
            .map(|f| {
                f.s.revoked_at = Some(chrono::Utc::now());
                f.s.clone()
            }))
    }

    async fn revoke_app_session_by_hash(
        &self,
        id: uuid::Uuid,
        project: ProjectId,
        presented: [u8; 32],
    ) -> Result<Option<AppSession>, MetadataError> {
        self.check()?;
        let mut app = self.app.lock().unwrap();
        Ok(app
            .sessions
            .iter_mut()
            .find(|f| {
                f.s.id == id
                    && f.s.project_id == project
                    && f.s.revoked_at.is_none()
                    && (f.hash == presented || f.prev == Some(presented))
            })
            .map(|f| {
                f.s.revoked_at = Some(chrono::Utc::now());
                f.s.clone()
            }))
    }

    async fn revoke_user_sessions(
        &self,
        project: ProjectId,
        user: uuid::Uuid,
        keep: Option<uuid::Uuid>,
        created_before: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<Vec<AppSession>, MetadataError> {
        self.check()?;
        let mut app = self.app.lock().unwrap();
        let now = chrono::Utc::now();
        Ok(app
            .sessions
            .iter_mut()
            .filter(|f| {
                f.s.project_id == project
                    && f.s.user_id == user
                    && f.s.revoked_at.is_none()
                    && Some(f.s.id) != keep
                    && created_before.is_none_or(|t| f.s.created_at < t)
            })
            .map(|f| {
                f.s.revoked_at = Some(now);
                f.s.clone()
            })
            .collect())
    }
}

#[derive(Clone, Default)]
struct FakeLocator {
    ips: Arc<Mutex<HashMap<String, IpAddr>>>,
    controls: Arc<Mutex<HashMap<String, SocketAddr>>>,
    broken: Arc<Mutex<bool>>,
    pg: Arc<Mutex<HashMap<String, SocketAddr>>>,
    /// F2: `address` and `control_addr` never answer, like a hung Docker daemon.
    hang: Arc<Mutex<bool>>,
}

impl FakeLocator {
    fn running(&self, ep: &str, ip: &str, control: Option<SocketAddr>) {
        self.ips
            .lock()
            .unwrap()
            .insert(ep.into(), ip.parse().unwrap());
        if let Some(addr) = control {
            self.controls.lock().unwrap().insert(ep.into(), addr);
        }
    }
}

impl ComputeLocator for FakeLocator {
    async fn address(&self, ep: &str) -> anyhow::Result<Option<IpAddr>> {
        let hang = *self.hang.lock().unwrap();
        if hang {
            std::future::pending::<()>().await;
        }
        anyhow::ensure!(!*self.broken.lock().unwrap(), "docker is down");
        Ok(self.ips.lock().unwrap().get(ep).copied())
    }

    async fn control_addr(&self, ep: &str) -> anyhow::Result<Option<SocketAddr>> {
        let hang = *self.hang.lock().unwrap();
        if hang {
            std::future::pending::<()>().await;
        }
        anyhow::ensure!(!*self.broken.lock().unwrap(), "docker is down");
        Ok(self.controls.lock().unwrap().get(ep).copied())
    }

    async fn postgres_addr(&self, ep: &str) -> anyhow::Result<Option<SocketAddr>> {
        Ok(self.pg.lock().unwrap().get(ep).copied())
    }
}

async fn serve(app: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// A storage-controller stand-in: tenants on pageserver `ps:6400`, safekeeper `sk1:5454`
/// active and `sk2:5454` paused. With `healthy = false`, everything answers 500.
async fn storcon(healthy: bool) -> String {
    let app = if healthy {
        Router::new()
            .route(
                "/debug/v1/tenant/{tenant}/locate",
                get(|| async {
                    axum::Json(
                        json!({ "shards": [{ "listen_pg_addr": "ps", "listen_pg_port": 6400 }] }),
                    )
                }),
            )
            .route(
                "/control/v1/safekeeper",
                get(|| async {
                    axum::Json(json!([
                        { "id": 1, "host": "sk1", "port": 5454, "scheduling_policy": "Active" },
                        { "id": 2, "host": "sk2", "port": 5454, "scheduling_policy": "Pause" },
                    ]))
                }),
            )
    } else {
        Router::new().fallback(|| async { (AxumStatus::INTERNAL_SERVER_ERROR, "boom") })
    };
    format!("http://{}", serve(app).await)
}

struct Internal {
    /// The secrets key the orchestrator opens credentials with.
    key: SecretKey,
    base: String,
    signer: ComputeSigner,
    http: reqwest::Client,
}

impl Internal {
    async fn get(&self, path: &str, token: Option<&str>) -> (u16, Value) {
        let mut req = self.http.get(format!("{}{path}", self.base));
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    async fn get_with_header(&self, path: &str, authorization: &str) -> u16 {
        let resp = self
            .http
            .get(format!("{}{path}", self.base))
            .header("authorization", authorization)
            .send()
            .await
            .unwrap();
        resp.status().as_u16()
    }

    /// A forwarded DDL delta: (status, content type, body).
    async fn ddl(&self, ep: &str, token: Option<&str>, body: &str) -> (u16, String, String) {
        let mut req = self
            .http
            .patch(format!("{}/compute/api/v2/computes/{ep}/ddl", self.base))
            .header("content-type", "application/json")
            .body(body.to_string());
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        (status, ctype, resp.text().await.unwrap())
    }

    async fn put(&self, path: &str, token: Option<&str>, body: &str) -> u16 {
        let mut req = self
            .http
            .put(format!("{}{path}", self.base))
            .header("content-type", "application/json")
            .body(body.to_string());
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        req.send().await.unwrap().status().as_u16()
    }
}

async fn spawn_internal(neon: FakeNeon, locator: FakeLocator, storcon_base: &str) -> Internal {
    spawn_internal_with(neon, locator, storcon_base, rules(Mode::SelfHosted, false), FakeCache::default()).await
}

/// The rules the receiver applies: caps 50 and 20, strength on or off.
fn rules(mode: Mode, strength: bool) -> RoleSettings {
    RoleSettings::from_lookup(mode, |k| {
        (k == "CHELABASE_PASSWORD_STRENGTH").then(|| if strength { "on" } else { "off" }.into())
    })
    .unwrap()
}

/// The proxy cache stand-in: records each `password_updated` and wakes waiters; with `down`,
/// it fails.
#[derive(Clone, Default)]
struct FakeCache {
    sent: Arc<Mutex<Vec<(String, String)>>>,
    down: Arc<Mutex<bool>>,
    published: Arc<tokio::sync::Notify>,
}

impl CachePort for FakeCache {
    fn password_updated<'a>(
        &'a self,
        endpoint_id: &'a str,
        role: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<u64>> {
        Box::pin(async move {
            self.sent
                .lock()
                .unwrap()
                .push((endpoint_id.to_string(), role.to_string()));
            self.published.notify_waiters();
            anyhow::ensure!(!*self.down.lock().unwrap(), "valkey is down");
            Ok(1)
        })
    }
}

impl FakeCache {
    /// Members hub T8 (S19): waits (up to 5 s, on the publish notifications, never a sleep)
    /// until every (endpoint, role) of `want` was published; whether they all were.
    async fn wait_for(&self, want: &[(String, String)]) -> bool {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let notified = self.published.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let sent = self.sent.lock().unwrap().clone();
            if want.iter().all(|w| sent.contains(w)) {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                let sent = self.sent.lock().unwrap().clone();
                return want.iter().all(|w| sent.contains(w));
            }
        }
    }
}

async fn spawn_internal_with(
    neon: FakeNeon,
    locator: FakeLocator,
    storcon_base: &str,
    roles: RoleSettings,
    cache: FakeCache,
) -> Internal {
    spawn_internal_full(neon, locator, storcon_base, roles, cache, "http://127.0.0.1:9").await
}

async fn spawn_internal_full(
    neon: FakeNeon,
    locator: FakeLocator,
    storcon_base: &str,
    roles: RoleSettings,
    cache: FakeCache,
    rest_url: &str,
) -> Internal {
    spawn_with_app(
        neon,
        locator,
        storcon_base,
        roles,
        cache,
        rest_url,
        AppSetup::default(),
    )
    .await
}

/// The app-user parts of the state a test chooses.
struct AppSetup {
    key: SecretKey,
    signing_key: Option<chelabase_types::file_value::FileValue>,
    zitadel: Option<String>,
    jobs: FakeJobs,
    resync: FakeResync,
    mode: Mode,
    rt_events: FakeRtEvents,
    /// C4 (SP11): the control-network locator behind the route's `replication_host`.
    control: FakeLocator,
    /// C4 (SP4): `InternalState.realtime_logins`.
    realtime_logins: chelabase_neon::spec::MemberLogins,
    /// Carried gaps T4 (P10): where notify-attach publishes its tenant-attached events.
    attach_events: FakeAttachEvents,
    /// Selfhost-install m3: `InternalState.setup_code`.
    setup_code: Option<orchestrator::internal::SetupCode>,
}

impl Default for AppSetup {
    fn default() -> Self {
        Self {
            key: SecretKey::generate(),
            signing_key: Some(SIGNING_KEY.into()),
            zitadel: None,
            jobs: FakeJobs::default(),
            resync: FakeResync::default(),
            mode: Mode::SelfHosted,
            rt_events: FakeRtEvents::default(),
            control: FakeLocator::default(),
            realtime_logins: Default::default(),
            attach_events: FakeAttachEvents::default(),
            setup_code: None,
        }
    }
}

/// How the fake's publish of a tenant-attached event ends.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum AttachPublish {
    /// JetStream acks it.
    #[default]
    Acked,
    /// The publish fails (NATS down, no stream).
    Fails,
    /// No ack ever comes.
    Hangs,
    /// The ack comes when the test calls `gate.notify_one()`.
    Gated,
}

/// Records every tenant-attached event the hook publishes (whatever the publish's outcome).
#[derive(Clone, Default)]
struct FakeAttachEvents {
    published: Arc<Mutex<Vec<chelabase_nats::TenantAttachedEvent>>>,
    outcome: Arc<Mutex<AttachPublish>>,
    gate: Arc<tokio::sync::Notify>,
}

impl FakeAttachEvents {
    fn with(outcome: AttachPublish) -> Self {
        let fake = Self::default();
        *fake.outcome.lock().unwrap() = outcome;
        fake
    }

    fn published(&self) -> Vec<chelabase_nats::TenantAttachedEvent> {
        self.published.lock().unwrap().clone()
    }
}

impl orchestrator::internal::AttachEvents for FakeAttachEvents {
    fn tenant_attached(
        &self,
        event: chelabase_nats::TenantAttachedEvent,
    ) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async move {
            self.published.lock().unwrap().push(event);
            let outcome = *self.outcome.lock().unwrap();
            match outcome {
                AttachPublish::Acked => Ok(()),
                AttachPublish::Fails => anyhow::bail!("nats is down"),
                AttachPublish::Hangs => std::future::pending::<anyhow::Result<()>>().await,
                AttachPublish::Gated => {
                    self.gate.notified().await;
                    Ok(())
                }
            }
        })
    }
}

/// RT1 T6: realtime's bearer on the internal listener's reused routes.
const REALTIME_TOKEN: &str = "realtime-token";

/// Records each realtime event the internal routes publish.
#[derive(Clone, Default)]
struct FakeRtEvents {
    project: Arc<Mutex<Vec<(uuid::Uuid, chelabase_nats::rt::ProjectEvent)>>>,
    access: Arc<Mutex<Vec<chelabase_nats::rt::AccessEvent>>>,
}

impl orchestrator::internal::realtime::RtEvents for FakeRtEvents {
    fn project(
        &self,
        project_id: uuid::Uuid,
        event: chelabase_nats::rt::ProjectEvent,
    ) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async move {
            self.project.lock().unwrap().push((project_id, event));
            Ok(())
        })
    }

    fn access(&self, event: chelabase_nats::rt::AccessEvent) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async move {
            self.access.lock().unwrap().push(event);
            Ok(())
        })
    }
}

/// Records each branch the receiver asked the worker to resync, and each it asked to
/// reconcile the member roles of.
#[derive(Clone, Default)]
struct FakeResync {
    asked: Arc<Mutex<Vec<BranchId>>>,
    members_asked: Arc<Mutex<Vec<BranchId>>>,
}

impl ResyncPort for FakeResync {
    fn resync_branch(&self, branch: BranchId) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async move {
            self.asked.lock().unwrap().push(branch);
            Ok(())
        })
    }

    fn reconcile_member_roles(&self, branch: BranchId) -> BoxFuture<'_, anyhow::Result<()>> {
        Box::pin(async move {
            self.members_asked.lock().unwrap().push(branch);
            Ok(())
        })
    }
}

/// Records each sync job; with `down`, publishing fails.
#[derive(Clone, Default)]
struct FakeJobs {
    published: Arc<Mutex<Vec<AppUsersSyncJob>>>,
}

impl AppJobPort for FakeJobs {
    fn publish_app_users_sync(
        &self,
        job: JobEnvelope<AppUsersSyncJob>,
    ) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async move {
            self.published.lock().unwrap().push(job.payload);
            Ok(())
        })
    }
}

async fn spawn_with_app(
    neon: FakeNeon,
    locator: FakeLocator,
    storcon_base: &str,
    roles: RoleSettings,
    cache: FakeCache,
    rest_url: &str,
    app: AppSetup,
) -> Internal {
    let (pem, x) = generate_keypair();
    let key = app.key;
    let state = Arc::new(InternalState {
        metadata: neon,
        locator,
        storcon: StorconClient::new(storcon_base, 0),
        signer: ComputeSigner::new(&pem, &x).unwrap(),
        // The production clients (no redirects), so tests see what production sees.
        http: chelabase_http::client_builder().build().unwrap(),
        proxy_token: PROXY_TOKEN.into(),
        storcon_token: STORCON_TOKEN.into(),
        data_keys_token: DATA_KEYS_TOKEN.into(),
        rest_token: REST_TOKEN.into(),
        realtime_token: REALTIME_TOKEN.into(),
        rt_events: Arc::new(app.rt_events),
        secrets: key.clone(),
        roles,
        cache: Arc::new(cache),
        resync: Arc::new(app.resync),
        control_plane_uri: CONTROL_PLANE_URI.into(),
        rest: orchestrator::RestInternal {
            url: rest_url.into(),
            token: REST_TOKEN.into(),
            http: chelabase_http::client_builder().build().unwrap(),
        },
        app_auth_token: APP_AUTH_TOKEN.into(),
        app_sessions_token: APP_SESSIONS_TOKEN.into(),
        app_refresh_ttl: REFRESH_TTL,
        actions_signing_key: app.signing_key,
        zitadel: app
            .zitadel
            .map(|base| Zitadel::new(&base, chelabase_zitadel::mock::ADMIN_TOKEN)),
        app_jobs: Arc::new(app.jobs),
        policy: chelabase_types::mode_policy::ModePolicy::new(app.mode),
        setup_code: app.setup_code,
        member_logins: Default::default(),
        control_locator: app.control,
        realtime_logins: app.realtime_logins,
        attach_events: Arc::new(app.attach_events),
    });
    let addr = serve(build_internal_router(state)).await;
    Internal {
        key,
        base: format!("http://{addr}"),
        signer: ComputeSigner::new(&pem, &x).unwrap(),
        http: reqwest::Client::new(),
    }
}

const TENANT: &str = "11111111111111111111111111111111";
const TIMELINE: &str = "22222222222222222222222222222222";
const ACCESS: &str = "/proxy/api/v1/get_endpoint_access_control";
const WAKE: &str = "/proxy/api/v1/wake_compute";

#[tokio::test]
async fn health_needs_no_token() {
    let app = spawn_internal(
        FakeNeon::default(),
        FakeLocator::default(),
        "http://127.0.0.1:9",
    )
    .await;
    assert_eq!(app.get("/health", None).await.0, 200);
}

#[tokio::test]
async fn proxy_routes_need_the_proxy_token() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;
    let q = format!("{ACCESS}?endpointish=ep-a&role=app");
    assert_eq!(app.get(&q, None).await.0, 401);
    assert_eq!(app.get(&q, Some("wrong")).await.0, 401);
    assert_eq!(
        app.get(&q, Some(STORCON_TOKEN)).await.0,
        401,
        "another caller's token"
    );
    assert_eq!(app.get(&q, Some(COMPUTE_TOKEN)).await.0, 401);
    assert_eq!(
        app.get(&format!("{WAKE}?endpointish=ep-a"), Some(STORCON_TOKEN))
            .await
            .0,
        401
    );
}

#[tokio::test]
async fn a_scheme_other_than_bearer_is_refused() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;
    let q = format!("{ACCESS}?endpointish=ep-a&role=app");
    assert_eq!(
        app.get_with_header(&q, &format!("Basic {PROXY_TOKEN}"))
            .await,
        401
    );
    assert_eq!(app.get_with_header(&q, PROXY_TOKEN).await, 401, "no scheme");
    assert_eq!(
        app.get_with_header(&q, &format!("Bearer {PROXY_TOKEN}"))
            .await,
        200,
        "the same token, correctly presented"
    );
}

#[tokio::test]
async fn access_control_gives_the_role_secret() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;
    let (status, body) = app
        .get(
            &format!("{ACCESS}?endpointish=ep-a&role=app"),
            Some(PROXY_TOKEN),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["role_secret"], "SCRAM-SHA-256$verifier-of-ep-a");
    assert_eq!(body["project_id"], "ep-a");
}

// Review Focus 1: the proxy adds session_id, application_name and options.
#[tokio::test]
async fn extra_proxy_parameters_are_ignored() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let locator = FakeLocator::default();
    locator.running("ep-a", "172.20.0.9", None);
    let app = spawn_internal(neon, locator, "http://127.0.0.1:9").await;
    let extra =
        "session_id=00000000-0000-0000-0000-000000000000&application_name=psql&options=-c%20x%3D1";
    let (status, _) = app
        .get(
            &format!("{ACCESS}?{extra}&endpointish=ep-a&role=app"),
            Some(PROXY_TOKEN),
        )
        .await;
    assert_eq!(status, 200);
    let (status, _) = app
        .get(
            &format!("{WAKE}?{extra}&endpointish=ep-a"),
            Some(PROXY_TOKEN),
        )
        .await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn an_unknown_role_gets_an_empty_secret() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;
    let (status, body) = app
        .get(
            &format!("{ACCESS}?endpointish=ep-a&role=nobody"),
            Some(PROXY_TOKEN),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["role_secret"], "");
}

#[tokio::test]
async fn an_unknown_endpoint_is_not_found() {
    let app = spawn_internal(
        FakeNeon::default(),
        FakeLocator::default(),
        "http://127.0.0.1:9",
    )
    .await;
    for path in [
        format!("{ACCESS}?endpointish=ep-x&role=app"),
        format!("{WAKE}?endpointish=ep-x"),
    ] {
        let (status, body) = app.get(&path, Some(PROXY_TOKEN)).await;
        assert_eq!(status, 404, "{path}");
        assert_eq!(
            body["status"]["details"]["error_info"]["reason"],
            "ENDPOINT_NOT_FOUND"
        );
    }
}

#[tokio::test]
async fn a_metadata_failure_asks_the_proxy_to_retry() {
    let neon = FakeNeon::default();
    neon.break_it();
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;
    let (status, body) = app
        .get(
            &format!("{ACCESS}?endpointish=ep-a&role=app"),
            Some(PROXY_TOKEN),
        )
        .await;
    assert_eq!(status, 500);
    assert_eq!(
        body["status"]["details"]["retry_info"]["retry_delay_ms"],
        1000
    );
}

#[tokio::test]
async fn wake_compute_asks_the_proxy_to_retry_on_a_metadata_failure() {
    let neon = FakeNeon::default();
    neon.break_it();
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;
    let (status, body) = app
        .get(&format!("{WAKE}?endpointish=ep-a"), Some(PROXY_TOKEN))
        .await;
    assert_eq!(status, 500);
    assert_eq!(body["status"]["details"]["error_info"]["reason"], "UNKNOWN");
    assert_eq!(
        body["status"]["details"]["retry_info"]["retry_delay_ms"],
        1000
    );
}

#[tokio::test]
async fn wake_compute_gives_the_running_compute() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let locator = FakeLocator::default();
    locator.running("ep-a", "172.20.0.9", None);
    let app = spawn_internal(neon, locator, "http://127.0.0.1:9").await;
    let (status, body) = app
        .get(&format!("{WAKE}?endpointish=ep-a"), Some(PROXY_TOKEN))
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["address"], "172.20.0.9:5432");
    assert_eq!(body["aux"]["branch_id"], TIMELINE);
}

#[tokio::test]
async fn wake_compute_refuses_unroutable_endpoint() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    neon.set_routing("ep-a", false, None);
    let locator = FakeLocator::default();
    locator.running("ep-a", "172.20.0.9", None);
    let app = spawn_internal(neon, locator, "http://127.0.0.1:9").await;
    let (status, body) = app
        .get(&format!("{WAKE}?endpointish=ep-a"), Some(PROXY_TOKEN))
        .await;
    assert_eq!(status, 404);
    assert_eq!(
        body["status"]["details"]["error_info"]["reason"],
        "ENDPOINT_NOT_FOUND"
    );
}

#[tokio::test]
async fn access_control_refuses_unroutable_endpoint() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    neon.set_routing("ep-a", false, None);
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;
    let (status, body) = app
        .get(
            &format!("{ACCESS}?endpointish=ep-a&role=app"),
            Some(PROXY_TOKEN),
        )
        .await;
    assert_eq!(status, 404);
    assert_eq!(
        body["status"]["details"]["error_info"]["reason"],
        "ENDPOINT_NOT_FOUND"
    );
}

#[tokio::test]
async fn a_compute_that_is_not_running_is_retried() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;
    let (status, body) = app
        .get(&format!("{WAKE}?endpointish=ep-a"), Some(PROXY_TOKEN))
        .await;
    assert_eq!(status, 503);
    assert_eq!(
        body["status"]["details"]["retry_info"]["retry_delay_ms"],
        1000
    );
}

#[tokio::test]
async fn docker_being_down_is_retried_too() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let locator = FakeLocator::default();
    *locator.broken.lock().unwrap() = true;
    let app = spawn_internal(neon, locator, "http://127.0.0.1:9").await;
    let (status, body) = app
        .get(&format!("{WAKE}?endpointish=ep-a"), Some(PROXY_TOKEN))
        .await;
    assert_eq!(status, 503);
    assert_eq!(
        body["status"]["details"]["retry_info"]["retry_delay_ms"],
        1000
    );
}

fn spec_path(ep: &str) -> String {
    format!("/compute/api/v2/computes/{ep}/spec")
}

#[tokio::test]
async fn the_spec_goes_to_the_compute_with_its_token() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let app = spawn_internal(neon, FakeLocator::default(), &storcon(true).await).await;
    let (status, body) = app.get(&spec_path("ep-a"), Some(COMPUTE_TOKEN)).await;
    assert_eq!(status, 200);
    assert_eq!(body["status"], "attached");
    assert_eq!(body["spec"]["tenant_id"], TENANT);
    assert_eq!(body["spec"]["timeline_id"], TIMELINE);
    assert_eq!(
        body["spec"]["pageserver_connstring"],
        "postgresql://no_user@ps:6400"
    );
    assert_eq!(body["spec"]["safekeeper_connstrings"], json!(["sk1:5454"]));
    assert_eq!(body["spec"]["cluster"]["cluster_id"], "ep-a");
    assert_eq!(
        body["spec"]["cluster"]["roles"][0]["encrypted_password"],
        "SCRAM-SHA-256$verifier-of-ep-a"
    );
    assert_eq!(body["spec"]["cluster"]["databases"][0]["owner"], "app");
    assert_eq!(body["compute_ctl_config"]["jwks"], app.signer.jwks());
}

#[tokio::test]
async fn spec_route_serves_static_mode() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    neon.add("ep-b", TENANT, TIMELINE);
    neon.set_routing("ep-a", false, Some("0/16B3748"));
    let app = spawn_internal(neon, FakeLocator::default(), &storcon(true).await).await;
    let (status, body) = app.get(&spec_path("ep-a"), Some(COMPUTE_TOKEN)).await;
    assert_eq!(status, 200);
    assert_eq!(body["spec"]["mode"], json!({ "Static": "0/16B3748" }));
    let (_, primary) = app.get(&spec_path("ep-b"), Some(COMPUTE_TOKEN)).await;
    assert_eq!(primary["spec"]["mode"], "Primary");
}

#[tokio::test]
async fn the_spec_needs_that_computes_token() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let app = spawn_internal(neon, FakeLocator::default(), &storcon(true).await).await;
    assert_eq!(app.get(&spec_path("ep-a"), None).await.0, 401);
    assert_eq!(app.get(&spec_path("ep-a"), Some("wrong")).await.0, 401);
    assert_eq!(
        app.get(&spec_path("ep-a"), Some(PROXY_TOKEN)).await.0,
        401,
        "another caller's token"
    );
    assert_eq!(
        app.get(&spec_path("ep-a"), Some(STORCON_TOKEN)).await.0,
        401
    );
    assert_eq!(
        app.get(&spec_path("ep-missing"), Some(COMPUTE_TOKEN))
            .await
            .0,
        401,
        "an unknown id looks like a bad token"
    );
}

// Each compute's token opens only its own spec.
#[tokio::test]
async fn one_endpoints_token_does_not_fetch_anothers_spec() {
    let neon = FakeNeon::default();
    neon.add_with_token("ep-a", TENANT, TIMELINE, "token-of-a");
    neon.add_with_token("ep-b", TENANT, TIMELINE, "token-of-b");
    let app = spawn_internal(neon, FakeLocator::default(), &storcon(true).await).await;
    assert_eq!(app.get(&spec_path("ep-a"), Some("token-of-a")).await.0, 200);
    assert_eq!(app.get(&spec_path("ep-b"), Some("token-of-b")).await.0, 200);
    assert_eq!(app.get(&spec_path("ep-a"), Some("token-of-b")).await.0, 401);
    assert_eq!(app.get(&spec_path("ep-b"), Some("token-of-a")).await.0, 401);
}

#[tokio::test]
async fn the_spec_waits_for_metadata() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    neon.break_it();
    let app = spawn_internal(neon, FakeLocator::default(), &storcon(true).await).await;
    assert_eq!(
        app.get(&spec_path("ep-a"), Some(COMPUTE_TOKEN)).await.0,
        503
    );
}

#[tokio::test]
async fn the_spec_waits_for_the_storage_controller() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let app = spawn_internal(neon, FakeLocator::default(), &storcon(false).await).await;
    assert_eq!(
        app.get(&spec_path("ep-a"), Some(COMPUTE_TOKEN)).await.0,
        503
    );
}

/// A compute_ctl stand-in answering `/configure` with `status`, recording each request's
/// bearer token and body.
#[derive(Clone, Default)]
struct Configured(Arc<Mutex<Vec<(String, Value)>>>);

async fn compute(status: u16) -> (SocketAddr, Configured) {
    // C4 (SP1): the hook reads `/status` first; an ordinary compute is running and advertises
    // nothing.
    let (addr, seen, _) = compute_ctl(status, 200, json!({ "status": "running" })).await;
    (addr, seen)
}

/// Everything `compute_ctl` stand-in received, in order: `"status"` and `"configure"`.
type Events = Arc<Mutex<Vec<(&'static str, String)>>>;

/// [`compute`] with `/status` answering `status_code` and `status_body` (C4, SP1); the third
/// value logs each request (`"status"` / `"configure"`, with its `Authorization`).
async fn compute_ctl(
    configure: u16,
    status_code: u16,
    status_body: Value,
) -> (SocketAddr, Configured, Events) {
    use axum::extract::State;
    let events: Events = Events::default();
    let seen = Configured::default();
    let status_events = events.clone();
    let configure_events = events.clone();
    let status = configure;
    let app = Router::new()
        .route(
            "/status",
            get(move |headers: axum::http::HeaderMap| {
                let (events, body) = (status_events.clone(), status_body.clone());
                async move {
                    let auth = headers
                        .get("authorization")
                        .map(|v| v.to_str().unwrap().to_string())
                        .unwrap_or_default();
                    events.lock().unwrap().push(("status", auth));
                    (AxumStatus::from_u16(status_code).unwrap(), axum::Json(body))
                }
            }),
        )
        .route(
            "/configure",
            axum::routing::post(
                move |State(seen): State<Configured>,
                      headers: axum::http::HeaderMap,
                      body: axum::Json<Value>| {
                    let configure_events = configure_events.clone();
                    async move {
                        let auth = headers
                            .get("authorization")
                            .unwrap()
                            .to_str()
                            .unwrap()
                            .to_string();
                        configure_events.lock().unwrap().push(("configure", auth.clone()));
                        seen.0.lock().unwrap().push((auth, body.0));
                        (AxumStatus::from_u16(status).unwrap(), "{}")
                    }
                },
            ),
        )
        .with_state(seen.clone());
    (serve(app).await, seen, events)
}

fn attach(tenant: &str) -> String {
    json!({ "tenant_id": tenant, "preferred_az": null, "stripe_size": null,
            "shards": [{ "node_id": 1, "shard_number": 0 }] })
    .to_string()
}

const ATTACH: &str = "/storcon/notify-attach";
const SAFEKEEPERS: &str = "/storcon/notify-safekeepers";

#[tokio::test]
async fn hooks_need_the_storage_controller_token_before_the_body() {
    let app = spawn_internal(
        FakeNeon::default(),
        FakeLocator::default(),
        &storcon(true).await,
    )
    .await;
    assert_eq!(app.put(ATTACH, None, "not json").await, 401);
    assert_eq!(
        app.put(ATTACH, Some(PROXY_TOKEN), &attach(TENANT)).await,
        401,
        "another caller's token"
    );
    assert_eq!(app.put(SAFEKEEPERS, Some(COMPUTE_TOKEN), "{}").await, 401);
}

#[tokio::test]
async fn an_unparseable_hook_body_is_retried_never_a_4xx() {
    let app = spawn_internal(
        FakeNeon::default(),
        FakeLocator::default(),
        &storcon(true).await,
    )
    .await;
    for path in [ATTACH, SAFEKEEPERS] {
        for body in ["not json", "{}", ""] {
            assert_eq!(
                app.put(path, Some(STORCON_TOKEN), body).await,
                503,
                "{path} {body:?}"
            );
        }
    }
}

// A tenant no endpoint row names (a throwaway one from neon-smoke, or one already torn down) is
// answered at once: there is nothing to reconfigure, and another tenant's computes are left alone.
#[tokio::test]
async fn hook_for_an_unknown_tenant_reconfigures_nothing() {
    let neon = FakeNeon::default();
    neon.add(
        "ep-other",
        "0000000000000000000000000000ffff",
        "0000000000000000000000000000eeee",
    );
    let (addr, seen) = compute(200).await;
    let locator = FakeLocator::default();
    locator.running("ep-other", "172.20.0.9", Some(addr));
    let app = spawn_internal(neon, locator, &storcon(true).await).await;
    assert_eq!(
        app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await,
        200
    );
    let body = json!({ "tenant_id": TENANT, "timeline_id": TIMELINE, "generation": 3,
                       "safekeepers": [{ "id": 1, "hostname": "sk1" }] })
    .to_string();
    assert_eq!(app.put(SAFEKEEPERS, Some(STORCON_TOKEN), &body).await, 200);
    assert!(seen.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn hook_for_a_known_tenant_still_reconfigures() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let (addr, seen) = compute(200).await;
    let locator = FakeLocator::default();
    locator.running("ep-a", "172.20.0.9", Some(addr));
    let app = spawn_internal(neon, locator, &storcon(true).await).await;
    assert_eq!(
        app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await,
        200
    );
    let body = json!({ "tenant_id": TENANT, "timeline_id": TIMELINE, "generation": 3,
                       "safekeepers": [{ "id": 1, "hostname": "sk1" }] })
    .to_string();
    assert_eq!(app.put(SAFEKEEPERS, Some(STORCON_TOKEN), &body).await, 200);
    assert_eq!(seen.0.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_compute_that_is_not_running_is_skipped() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let app = spawn_internal(neon, FakeLocator::default(), &storcon(true).await).await;
    assert_eq!(
        app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await,
        200
    );
}

#[tokio::test]
async fn a_running_compute_gets_the_new_spec() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let (addr, seen) = compute(200).await;
    let locator = FakeLocator::default();
    locator.running("ep-a", "172.20.0.9", Some(addr));
    let app = spawn_internal(neon, locator, &storcon(true).await).await;
    assert_eq!(
        app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await,
        200
    );

    let seen = seen.0.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    let (auth, body) = &seen[0];
    assert_eq!(
        body["spec"]["cluster"]["roles"][0]["encrypted_password"],
        "SCRAM-SHA-256$verifier-of-ep-a"
    );
    assert_eq!(body["compute_ctl_config"]["jwks"], app.signer.jwks());
    // The token verifies against the published JWKS and names this compute.
    let token = auth.strip_prefix("Bearer ").unwrap();
    let jwks: jsonwebtoken::jwk::JwkSet = serde_json::from_value(app.signer.jwks()).unwrap();
    let key = jsonwebtoken::DecodingKey::from_jwk(&jwks.keys[0]).unwrap();
    let claims = jsonwebtoken::decode::<Value>(
        token,
        &key,
        &jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::EdDSA),
    )
    .unwrap()
    .claims;
    assert_eq!(claims["compute_id"], "ep-a");
}

#[tokio::test]
async fn notify_safekeepers_reconfigures_the_timelines_endpoints() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let (addr, seen) = compute(200).await;
    let locator = FakeLocator::default();
    locator.running("ep-a", "172.20.0.9", Some(addr));
    let app = spawn_internal(neon, locator, &storcon(true).await).await;
    let body = json!({ "tenant_id": TENANT, "timeline_id": TIMELINE, "generation": 3,
                       "safekeepers": [{ "id": 1, "hostname": "sk1" }] })
    .to_string();
    assert_eq!(app.put(SAFEKEEPERS, Some(STORCON_TOKEN), &body).await, 200);
    assert_eq!(seen.0.lock().unwrap().len(), 1);
}

// Review Focus 2: compute_ctl answers 412 while it is still starting. 503 makes the storage
// controller retry; 4xx would fail the reconcile.
#[tokio::test]
async fn a_compute_still_starting_is_retried() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let (addr, seen) = compute(412).await;
    let locator = FakeLocator::default();
    locator.running("ep-a", "172.20.0.9", Some(addr));
    let app = spawn_internal(neon, locator, &storcon(true).await).await;
    assert_eq!(
        app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await,
        503
    );
    assert_eq!(
        seen.0.lock().unwrap().len(),
        1,
        "the 503 comes from the compute's 412"
    );
}

// Review Focus 3: one failing compute doesn't stop the others from being reconfigured.
// Relies on the fake listing endpoints in insertion order, with the failing one (ep-a) first.
#[tokio::test]
async fn one_failing_compute_does_not_stop_the_others() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    neon.add("ep-b", TENANT, "33333333333333333333333333333333");
    let (bad, _) = compute(500).await;
    let (good, seen) = compute(200).await;
    let locator = FakeLocator::default();
    locator.running("ep-a", "172.20.0.9", Some(bad));
    locator.running("ep-b", "172.20.0.10", Some(good));
    let app = spawn_internal(neon, locator, &storcon(true).await).await;
    assert_eq!(
        app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await,
        503
    );
    assert_eq!(
        seen.0.lock().unwrap().len(),
        1,
        "ep-b was still reconfigured"
    );
}

// An endpoint deleted between the list lookup and its fetch needs nothing, so it isn't a failure.
#[tokio::test]
async fn an_endpoint_deleted_mid_hook_is_skipped() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    neon.add("ep-b", TENANT, "33333333333333333333333333333333");
    neon.vanish("ep-a");
    let (gone, gone_seen) = compute(200).await;
    let (good, seen) = compute(200).await;
    let locator = FakeLocator::default();
    locator.running("ep-a", "172.20.0.9", Some(gone));
    locator.running("ep-b", "172.20.0.10", Some(good));
    let app = spawn_internal(neon, locator, &storcon(true).await).await;
    assert_eq!(
        app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await,
        200
    );
    assert_eq!(gone_seen.0.lock().unwrap().len(), 0);
    assert_eq!(seen.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_metadata_failure_is_retried() {
    let neon = FakeNeon::default();
    neon.break_it();
    let app = spawn_internal(neon, FakeLocator::default(), &storcon(true).await).await;
    assert_eq!(
        app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await,
        503
    );
}

// --- Data API: key lookup and connection info ---------------------------------

const KEY_ID: uuid::Uuid = uuid::Uuid::from_u128(0x1234);
const PROJECT_ID: chelabase_types::ProjectId = chelabase_types::ProjectId(uuid::Uuid::from_u128(0x5678));

fn sha(key: &str) -> [u8; 32] {
    chelabase_types::data_key::hash(key)
}

#[tokio::test]
async fn key_lookup_requires_the_data_keys_bearer() {
    let neon = FakeNeon::default();
    let branch = BranchId::new();
    neon.data_keys.lock().unwrap().push((
        sha("chk_sec_x"),
        DataKeyLookup { key_id: KEY_ID, branch_id: branch, project_id: PROJECT_ID, kind: DataKeyKind::Secret, role: "reporting".into(), databases: None, role_oid: Some(16390) },
    ));
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    let path = format!("/internal/data/keys/{}", hex::encode(sha("chk_sec_x")));
    assert_eq!(app.get(&path, None).await.0, 401);
    assert_eq!(app.get(&path, Some("wrong")).await.0, 401);
    // Each caller has its own token: the proxy's and rest's open nothing here.
    assert_eq!(app.get(&path, Some(PROXY_TOKEN)).await.0, 401);
    assert_eq!(app.get(&path, Some(REST_TOKEN)).await.0, 401);
    assert!(neon.touched.lock().unwrap().is_empty());

    let (status, body) = app.get(&path, Some(DATA_KEYS_TOKEN)).await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        json!({ "key_id": KEY_ID, "branch_id": branch, "project_id": PROJECT_ID, "kind": "secret", "role": "reporting", "databases": null, "role_oid": 16390 })
    );
    // `last_used_at` is stamped off the response path.
    for _ in 0..50 {
        if !neon.touched.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(*neon.touched.lock().unwrap(), vec![KEY_ID]);
}

#[tokio::test]
async fn key_lookup_returns_databases() {
    let neon = FakeNeon::default();
    let branch = BranchId::new();
    neon.data_keys.lock().unwrap().push((
        sha("chk_sec_listed"),
        DataKeyLookup {
            key_id: KEY_ID,
            branch_id: branch,
            project_id: PROJECT_ID,
            kind: DataKeyKind::Secret,
            role: "reporting".into(),
            databases: Some(vec!["web1".into(), "analytics".into()]),
            role_oid: None,
        },
    ));
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;
    let path = format!("/internal/data/keys/{}", hex::encode(sha("chk_sec_listed")));
    let (status, body) = app.get(&path, Some(DATA_KEYS_TOKEN)).await;
    assert_eq!(status, 200);
    assert_eq!(body["databases"], json!(["web1", "analytics"]));
}

#[tokio::test]
async fn key_lookup_returns_project_id() {
    let neon = FakeNeon::default();
    neon.data_keys.lock().unwrap().push((
        sha("chk_sec_proj"),
        DataKeyLookup {
            key_id: KEY_ID,
            branch_id: BranchId::new(),
            project_id: PROJECT_ID,
            kind: DataKeyKind::Secret,
            role: "reporting".into(),
            databases: None,
            role_oid: None,
        },
    ));
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;
    let path = format!("/internal/data/keys/{}", hex::encode(sha("chk_sec_proj")));
    let (status, body) = app.get(&path, Some(DATA_KEYS_TOKEN)).await;
    assert_eq!(status, 200);
    assert_eq!(body["project_id"], json!(PROJECT_ID));
}

#[tokio::test]
async fn key_lookup_404_for_unknown_or_bad_hex() {
    let neon = FakeNeon::default();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    let unknown = hex::encode(sha("chk_sec_nobody"));
    for hex in [
        unknown.as_str(),
        "zz",
        "",
        "abcd",
        &"g".repeat(64),
        &unknown.to_uppercase(),
        &format!("{unknown}00"),
    ] {
        let (status, _) = app.get(&format!("/internal/data/keys/{hex}"), Some(DATA_KEYS_TOKEN)).await;
        assert_eq!(status, 404, "{hex:?}");
    }
    assert!(neon.touched.lock().unwrap().is_empty());
}

fn connection_row(status: BranchStatus, endpoint: Option<&str>, password: Option<String>) -> BranchConnectionRow {
    BranchConnectionRow {
        branch_id: BranchId::new(),
        status,
        endpoint_id: endpoint.map(str::to_string),
        database_name: Some("web1".into()),
        authenticator_password_enc: password,
        settings: DataApiSettings {
            max_rows: 500,
            statement_timeout_ms: 4000,
            max_connections: 7,
        },
        databases: vec![ServedDatabase { name: "web1".into(), default_schema: "app".into() }],
    }
}

#[tokio::test]
async fn connection_requires_the_rest_bearer() {
    let neon = FakeNeon::default();
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;
    let path = format!("/internal/data/branches/{}/connection", BranchId::new().as_uuid());
    assert_eq!(app.get(&path, None).await.0, 401);
    assert_eq!(app.get(&path, Some("wrong")).await.0, 401);
    assert_eq!(app.get(&path, Some(DATA_KEYS_TOKEN)).await.0, 401);
    assert_eq!(app.get(&path, Some(PROXY_TOKEN)).await.0, 401);
    // With the right token an unknown branch is a plain 404.
    assert_eq!(app.get(&path, Some(REST_TOKEN)).await.0, 404);
}

#[tokio::test]
async fn connection_409_when_not_ready() {
    let neon = FakeNeon::default();
    let locator = FakeLocator::default();
    let app = spawn_internal(neon.clone(), locator.clone(), "http://127.0.0.1:9").await;
    let sealed = |id: BranchId| seal_credential(&app.key, &id.to_string(), Purpose::Authenticator, "pw");
    locator.pg.lock().unwrap().insert("ep-1".into(), "127.0.0.1:55432".parse().unwrap());

    let mut cases = Vec::new();
    for (name, status, endpoint, has_password, expect) in [
        ("resetting", BranchStatus::Resetting, Some("ep-1"), true, 409),
        ("pending", BranchStatus::Pending, None, true, 409),
        ("failed", BranchStatus::Failed, Some("ep-1"), true, 409),
        ("no endpoint", BranchStatus::Ready, None, true, 409),
        ("no compute address", BranchStatus::Ready, Some("ep-gone"), true, 409),
        ("no authenticator password", BranchStatus::Ready, Some("ep-1"), false, 409),
        ("deleting", BranchStatus::Deleting, Some("ep-1"), true, 404),
        ("deleted", BranchStatus::Deleted, Some("ep-1"), true, 404),
    ] {
        let mut row = connection_row(status, endpoint, None);
        if has_password {
            row.authenticator_password_enc = Some(sealed(row.branch_id));
        }
        neon.connections.lock().unwrap().insert(row.branch_id, row.clone());
        cases.push((name, row, expect));
    }
    for (name, row, expect) in cases {
        let (code, body) = app
            .get(&format!("/internal/data/branches/{}/connection", row.branch_id.as_uuid()), Some(REST_TOKEN))
            .await;
        assert_eq!(code, expect, "{name}: {body}");
        if expect == 409 {
            assert_eq!(body, json!({ "status": row.status.as_str() }), "{name}");
        }
        let text = body.to_string();
        assert!(!text.contains("pw") && !text.contains("password"), "{name}: {text}");
    }
}

#[tokio::test]
async fn connection_lists_served_databases() {
    let neon = FakeNeon::default();
    let locator = FakeLocator::default();
    let app = spawn_internal(neon.clone(), locator.clone(), "http://127.0.0.1:9").await;
    locator.pg.lock().unwrap().insert("ep-1".into(), "127.0.0.1:55432".parse().unwrap());
    let mut row = connection_row(BranchStatus::Ready, Some("ep-1"), None);
    row.authenticator_password_enc =
        Some(seal_credential(&app.key, &row.branch_id.to_string(), Purpose::Authenticator, "auth-pw"));
    row.databases = vec![
        ServedDatabase { name: "web1".into(), default_schema: "app".into() },
        ServedDatabase { name: "analytics".into(), default_schema: "public".into() },
    ];
    neon.connections.lock().unwrap().insert(row.branch_id, row.clone());
    let (code, body) = app
        .get(&format!("/internal/data/branches/{}/connection", row.branch_id.as_uuid()), Some(REST_TOKEN))
        .await;
    assert_eq!(code, 200, "{body}");
    // The main database's name stays in `database`; every served one is listed, main first.
    assert_eq!(body["database"], "web1");
    assert_eq!(
        body["databases"],
        json!([{ "name": "web1", "default_schema": "app" }, { "name": "analytics", "default_schema": "public" }])
    );
    assert!(body["settings"].get("default_schema").is_none(), "{body}");
}

#[tokio::test]
async fn connection_opens_the_authenticator_password() {
    let neon = FakeNeon::default();
    let locator = FakeLocator::default();
    let app = spawn_internal(neon.clone(), locator.clone(), "http://127.0.0.1:9").await;
    locator.pg.lock().unwrap().insert("ep-1".into(), "127.0.0.1:55432".parse().unwrap());
    let mut row = connection_row(BranchStatus::Ready, Some("ep-1"), None);
    row.authenticator_password_enc =
        Some(seal_credential(&app.key, &row.branch_id.to_string(), Purpose::Authenticator, "auth-pw"));
    neon.connections.lock().unwrap().insert(row.branch_id, row.clone());

    let (code, body) = app
        .get(&format!("/internal/data/branches/{}/connection", row.branch_id.as_uuid()), Some(REST_TOKEN))
        .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(
        body,
        json!({
            "host": std::net::Ipv4Addr::LOCALHOST.to_string(),
            "port": 55432,
            "database": "web1",
            "server_name": "ep-1.compute.chelabase.internal",
            "user": "authenticator",
            "password": "auth-pw",
            "status": "ready",
            "settings": {
                "max_rows": 500,
                "statement_timeout_ms": 4000,
                "max_connections": 7,
            },
            "databases": [{ "name": "web1", "default_schema": "app" }],
            // C4 (SP11): no control-network address found, so null; the rest is unchanged.
            "replication_host": null,
        })
    );

    // A password sealed for another branch doesn't open (AAD): 500, never the plaintext.
    let mut other = connection_row(BranchStatus::Ready, Some("ep-1"), None);
    other.authenticator_password_enc =
        Some(seal_credential(&app.key, &BranchId::new().to_string(), Purpose::Authenticator, "stolen"));
    neon.connections.lock().unwrap().insert(other.branch_id, other.clone());
    let (code, body) = app
        .get(&format!("/internal/data/branches/{}/connection", other.branch_id.as_uuid()), Some(REST_TOKEN))
        .await;
    assert_eq!(code, 500);
    assert!(!body.to_string().contains("stolen"));
}

// ---- The DDL receiver (`PATCH /compute/api/v2/computes/{id}/ddl`) ----

const SCRAM: &str = "SCRAM-SHA-256$4096:c2FsdA==$c3RvcmVka2V5:c2VydmVya2V5";
const SCRAM2: &str = "SCRAM-SHA-256$4096:b3RoZXI=$b3RoZXJrZXk=:b3RoZXJzZXJ2ZXI=";
const MD5: &str = "md5aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const STRONG: &str = "Tr0ub4dor&3x-horse";
const WEAK: &str = "abcdefghijkl";

/// `ep-d`: owner `postgres`, a branch, main database `neondb` and the system `postgres`
/// database.
fn ddl_neon() -> (FakeNeon, BranchId) {
    let neon = FakeNeon::default();
    neon.add("ep-d", TENANT, TIMELINE);
    let branch = BranchId::new();
    let mut state = DdlState {
        owner: "postgres".into(),
        branch: Some(branch),
        main_db: Some("neondb".into()),
        ..DdlState::default()
    };
    state.roles.insert(
        "postgres".into(),
        copy("postgres", RoleOrigin::Owner, Some("SCRAM-SHA-256$owner"), None),
    );
    state
        .dbs
        .insert("neondb".into(), ("postgres".into(), DatabaseOrigin::Main));
    state
        .dbs
        .insert("postgres".into(), ("cloud_admin".into(), DatabaseOrigin::System));
    neon.ddl.lock().unwrap().insert("ep-d".into(), state);
    (neon, branch)
}

impl FakeNeon {
    fn ddl_state(&self) -> DdlState {
        self.ddl.lock().unwrap().get("ep-d").cloned().unwrap()
    }

    fn edit(&self, f: impl FnOnce(&mut DdlState)) {
        f(self.ddl.lock().unwrap().get_mut("ep-d").unwrap());
    }
}

fn set(name: &str, password: &str, encrypted: &str) -> Value {
    json!({ "op": "set", "name": name, "password": password, "encrypted_password": encrypted })
}

fn roles_delta(ops: Vec<Value>) -> String {
    json!({ "roles": ops }).to_string()
}

fn dbs_delta(ops: Vec<Value>) -> String {
    json!({ "dbs": ops }).to_string()
}

#[tokio::test]
async fn ddl_needs_the_compute_token() {
    let (neon, _) = ddl_neon();
    neon.add_with_token("ep-other", TENANT, TIMELINE, "token-of-other");
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    let body = roles_delta(vec![set("app_rw", STRONG, SCRAM)]);
    for (ep, token) in [
        ("ep-d", None),
        ("ep-d", Some("wrong")),
        ("ep-d", Some("token-of-other")),
        ("ep-d", Some(PROXY_TOKEN)),
        ("ep-missing", Some(COMPUTE_TOKEN)),
    ] {
        let (status, ctype, text) = app.ddl(ep, token, &body).await;
        assert_eq!(status, 401, "{ep} {token:?}");
        assert!(ctype.starts_with("text/plain"), "{ctype}");
        assert_eq!(text, "compute token refused");
    }
    // The token is checked before the body is buffered.
    assert_eq!(app.ddl("ep-d", None, "not json").await.0, 401);
    assert_eq!(neon.begins.load(Ordering::SeqCst), 0);
    assert!(!neon.ddl_state().roles.contains_key("app_rw"));
}

#[tokio::test]
async fn ddl_malformed_body_is_400() {
    let (neon, _) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    for body in [
        "not json",
        r#"{"roles":[{"op":"rename","name":"x"}]}"#,
        r#"{"roles":[{"op":"set"}]}"#,
    ] {
        let (status, _, text) = app.ddl("ep-d", Some(COMPUTE_TOKEN), body).await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(text, "malformed request from the compute");
    }
}

#[tokio::test]
async fn ddl_empty_delta_is_200_without_a_transaction() {
    let (neon, _) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    for body in [r#"{"roles":[]}"#, "{}", r#"{"roles":[],"dbs":[]}"#] {
        let (status, _, _) = app.ddl("ep-d", Some(COMPUTE_TOKEN), body).await;
        assert_eq!(status, 200, "{body}");
    }
    assert_eq!(neon.begins.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn ddl_unknown_endpoint_in_metadata_is_404() {
    // Authorized, but gone by the time the transaction opens.
    let neon = FakeNeon::default();
    neon.add("ep-d", TENANT, TIMELINE);
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;
    let (status, _, text) = app
        .ddl("ep-d", Some(COMPUTE_TOKEN), &roles_delta(vec![set("a", STRONG, SCRAM)]))
        .await;
    assert_eq!((status, text.as_str()), (404, "unknown endpoint"));
}

type Invalidations = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// A rest stand-in that records the invalidations it is sent: (branch, authorization).
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
                AxumStatus::NO_CONTENT
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, seen)
}

/// The invalidation is sent off the response path: waits for `n` of them.
async fn wait_for_invalidations(seen: &Invalidations, n: usize) {
    for _ in 0..100 {
        if seen.lock().unwrap().len() >= n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn ddl_drop_invalidates_rest() {
    let (rest, seen) = mock_rest_invalidate().await;
    let (neon, branch) = ddl_neon();
    neon.edit(|s| {
        s.dbs.insert("extra".into(), ("postgres".into(), DatabaseOrigin::Sql));
        s.dbs.insert("other".into(), ("postgres".into(), DatabaseOrigin::Sql));
    });
    let app = spawn_internal_full(
        neon.clone(),
        FakeLocator::default(),
        "http://127.0.0.1:9",
        rules(Mode::SelfHosted, false),
        FakeCache::default(),
        &rest,
    )
    .await;
    // A DROP DATABASE, then a rename: each is told to rest, with rest's bearer.
    let (status, _, _) = app
        .ddl("ep-d", Some(COMPUTE_TOKEN), &dbs_delta(vec![json!({ "op": "del", "name": "extra" })]))
        .await;
    assert_eq!(status, 200);
    wait_for_invalidations(&seen, 1).await;
    let (status, _, _) = app
        .ddl(
            "ep-d",
            Some(COMPUTE_TOKEN),
            &dbs_delta(vec![json!({ "op": "set", "name": "renamed", "old_name": "other" })]),
        )
        .await;
    assert_eq!(status, 200);
    wait_for_invalidations(&seen, 2).await;
    let expected = (branch.as_uuid().to_string(), Some(format!("Bearer {REST_TOKEN}")));
    assert_eq!(*seen.lock().unwrap(), vec![expected.clone(), expected]);

    // A CREATE DATABASE, a role change and a refused delta don't touch rest.
    for body in [
        dbs_delta(vec![json!({ "op": "set", "name": "fresh", "owner": "postgres" })]),
        roles_delta(vec![set("app_rw", STRONG, SCRAM)]),
        dbs_delta(vec![json!({ "op": "set", "name": "other", "old_name": "neondb" })]),
    ] {
        app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(seen.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn ddl_still_answers_when_rest_is_down() {
    let (neon, _) = ddl_neon();
    neon.edit(|s| {
        s.dbs.insert("extra".into(), ("postgres".into(), DatabaseOrigin::Sql));
    });
    // Nothing listens on the discard port.
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    let (status, _, _) = app
        .ddl("ep-d", Some(COMPUTE_TOKEN), &dbs_delta(vec![json!({ "op": "del", "name": "extra" })]))
        .await;
    assert_eq!(status, 200);
    assert!(!neon.ddl_state().dbs.contains_key("extra"));
}

type Setup = fn(&mut DdlState);
type Check = fn(&DdlState);

struct Rule {
    what: &'static str,
    rules: RoleSettings,
    setup: Setup,
    body: String,
    status: u16,
    text: &'static str,
    check: Check,
}

fn none(_: &mut DdlState) {}
fn nothing(_: &DdlState) {}

fn with_caps(mut r: RoleSettings, role_cap: u32, database_cap: u32) -> RoleSettings {
    r.role_cap = role_cap;
    r.database_cap = database_cap;
    r
}

fn two_sql_copies(s: &mut DdlState) {
    s.roles.insert("r1".into(), copy("r1", RoleOrigin::Sql, Some(SCRAM), None));
    s.roles.insert("r2".into(), copy("r2", RoleOrigin::Api, Some(SCRAM), Some("v1:x")));
}

fn one_sql_copy_and_an_md5_row(s: &mut DdlState) {
    s.roles.insert("r1".into(), copy("r1", RoleOrigin::Sql, Some(SCRAM), None));
    s.roles.insert("old_md5".into(), copy("old_md5", RoleOrigin::Sql, None, None));
}

fn app_rw_has_scram(s: &DdlState) {
    let r = &s.roles["app_rw"];
    assert_eq!(r.origin, RoleOrigin::Sql);
    assert_eq!(r.scram_verifier.as_deref(), Some(SCRAM));
    assert_eq!(s.roles_version, 1);
}

fn no_app_rw(s: &DdlState) {
    assert!(!s.roles.contains_key("app_rw"));
    assert_eq!(s.roles_version, 0);
}

fn rows() -> Vec<Rule> {
    let sh = rules(Mode::SelfHosted, false);
    let sh_strong = rules(Mode::SelfHosted, true);
    let pf = rules(Mode::Platform, false);
    vec![
        Rule {
            what: "a new login role",
            rules: sh,
            setup: none,
            body: roles_delta(vec![set("app_rw", STRONG, SCRAM)]),
            status: 200,
            text: "",
            check: app_rw_has_scram,
        },
        Rule {
            what: "a reserved name",
            rules: sh,
            setup: none,
            body: roles_delta(vec![set("anon", STRONG, SCRAM)]),
            status: 409,
            text: "the role name \"anon\" is reserved by Chelabase",
            check: |s| assert!(!s.roles.contains_key("anon")),
        },
        Rule {
            what: "a pg_ name",
            rules: sh,
            setup: none,
            body: roles_delta(vec![set("PG_mine", STRONG, SCRAM)]),
            status: 409,
            text: "the role name \"PG_mine\" is reserved by Chelabase",
            check: nothing,
        },
        Rule {
            what: "MD5 refused",
            rules: sh,
            setup: none,
            body: roles_delta(vec![set("app_rw", MD5, MD5)]),
            status: 409,
            text: "MD5 passwords can't be used through Chelabase; set a plain password",
            check: no_app_rw,
        },
        Rule {
            what: "not SCRAM",
            rules: sh,
            setup: none,
            body: roles_delta(vec![set("app_rw", STRONG, "plain-stored")]),
            status: 409,
            text: "this password format can't be used through Chelabase; set a plain password",
            check: no_app_rw,
        },
        Rule {
            what: "pre-hashed in platform mode",
            rules: pf,
            setup: none,
            body: roles_delta(vec![set("app_rw", SCRAM, SCRAM)]),
            status: 409,
            text: "pre-hashed passwords aren't accepted here; set a plain password",
            check: no_app_rw,
        },
        Rule {
            what: "pre-hashed in self-hosted mode",
            rules: sh,
            setup: none,
            body: roles_delta(vec![set("app_rw", SCRAM, SCRAM)]),
            status: 200,
            text: "",
            check: app_rw_has_scram,
        },
        Rule {
            what: "pre-hashed with strength on in self-hosted mode (Q6)",
            rules: sh_strong,
            setup: none,
            body: roles_delta(vec![set("app_rw", SCRAM, SCRAM)]),
            status: 200,
            text: "",
            check: app_rw_has_scram,
        },
        Rule {
            what: "weak with strength on",
            rules: sh_strong,
            setup: none,
            body: roles_delta(vec![set("app_rw", WEAK, SCRAM)]),
            status: 409,
            text: "password too weak: use at least 12 characters and mix letters, digits and symbols (about 60 bits)",
            check: no_app_rw,
        },
        Rule {
            what: "weak with strength off",
            rules: sh,
            setup: none,
            body: roles_delta(vec![set("app_rw", WEAK, SCRAM)]),
            status: 200,
            text: "",
            check: app_rw_has_scram,
        },
        Rule {
            what: "weak in platform mode",
            rules: pf,
            setup: none,
            body: roles_delta(vec![set("app_rw", WEAK, SCRAM)]),
            status: 409,
            text: "password too weak: use at least 12 characters and mix letters, digits and symbols (about 60 bits)",
            check: no_app_rw,
        },
        Rule {
            what: "the role cap reached",
            rules: with_caps(sh, 2, 20),
            setup: two_sql_copies,
            body: roles_delta(vec![set("app_rw", STRONG, SCRAM)]),
            status: 409,
            text: "this branch has reached its limit of 2 login roles (CHELABASE_BRANCH_ROLE_CAP)",
            check: no_app_rw,
        },
        Rule {
            what: "the role cap not reached (the owner and an MD5 row don't count)",
            rules: with_caps(sh, 2, 20),
            setup: one_sql_copy_and_an_md5_row,
            body: roles_delta(vec![set("app_rw", STRONG, SCRAM)]),
            status: 200,
            text: "",
            check: app_rw_has_scram,
        },
        Rule {
            what: "a re-password at the cap",
            rules: with_caps(sh, 2, 20),
            setup: two_sql_copies,
            body: roles_delta(vec![set("r2", STRONG, SCRAM2)]),
            status: 200,
            text: "",
            check: |s| {
                let r = &s.roles["r2"];
                assert_eq!(r.origin, RoleOrigin::Api, "keeps its origin");
                assert_eq!(r.scram_verifier.as_deref(), Some(SCRAM2));
                assert_eq!(r.password_enc, None, "the sealed password is cleared");
            },
        },
        Rule {
            what: "the role cap of 0",
            rules: with_caps(sh, 0, 20),
            setup: none,
            body: roles_delta(vec![set("app_rw", STRONG, SCRAM)]),
            status: 409,
            text: "this branch has reached its limit of 0 login roles (CHELABASE_BRANCH_ROLE_CAP)",
            check: no_app_rw,
        },
        Rule {
            what: "the owner dropped and created again",
            rules: sh,
            setup: none,
            body: roles_delta(vec![json!({
                "op": "set", "name": "postgres", "recreated": true,
                "password": STRONG, "encrypted_password": SCRAM
            })]),
            status: 409,
            text: "the branch owner \"postgres\" can't be dropped or renamed",
            check: |s| {
                assert!(s.roles.contains_key("postgres"));
                assert!(!s.owner_password_cleared);
                assert_eq!(s.roles_version, 0);
            },
        },
        Rule {
            what: "at the cap, a drop and a new copy in one delta",
            rules: with_caps(sh, 2, 20),
            setup: two_sql_copies,
            body: roles_delta(vec![set("app_rw", STRONG, SCRAM), json!({ "op": "del", "name": "r1" })]),
            status: 200,
            text: "",
            check: |s| assert!(s.roles.contains_key("app_rw") && !s.roles.contains_key("r1")),
        },
        Rule {
            what: "at the cap, a new copy and no drop",
            rules: with_caps(sh, 2, 20),
            setup: two_sql_copies,
            body: roles_delta(vec![set("app_rw", STRONG, SCRAM), set("r1", STRONG, SCRAM2)]),
            status: 409,
            text: "this branch has reached its limit of 2 login roles (CHELABASE_BRANCH_ROLE_CAP)",
            check: |s| {
                assert!(!s.roles.contains_key("app_rw"));
                assert_eq!(s.roles["r1"].scram_verifier.as_deref(), Some(SCRAM));
            },
        },
        Rule {
            what: "at the cap, PASSWORD NULL then a new copy",
            rules: with_caps(sh, 2, 20),
            setup: two_sql_copies,
            body: roles_delta(vec![json!({ "op": "set", "name": "r1" }), set("app_rw", STRONG, SCRAM)]),
            status: 200,
            text: "",
            // r1 stays, without a password: the role exists; the cap counts only verifiers.
            check: |s| assert!(s.roles.contains_key("app_rw") && s.roles["r1"].scram_verifier.is_none()),
        },
        Rule {
            what: "at the cap, a new copy then PASSWORD NULL",
            rules: with_caps(sh, 2, 20),
            setup: two_sql_copies,
            body: roles_delta(vec![set("app_rw", STRONG, SCRAM), json!({ "op": "set", "name": "r1" })]),
            status: 200,
            text: "",
            check: |s| assert!(s.roles.contains_key("app_rw") && s.roles["r1"].scram_verifier.is_none()),
        },
        Rule {
            what: "over the cap from before, a delta that doesn't add a copy",
            rules: with_caps(sh, 1, 20),
            setup: two_sql_copies,
            body: roles_delta(vec![set("r2", STRONG, SCRAM2)]),
            status: 200,
            text: "",
            check: |s| assert_eq!(s.roles["r2"].scram_verifier.as_deref(), Some(SCRAM2)),
        },
        Rule {
            what: "over the cap from before, a drop and a new copy",
            rules: with_caps(sh, 1, 20),
            setup: two_sql_copies,
            body: roles_delta(vec![set("app_rw", STRONG, SCRAM), json!({ "op": "del", "name": "r1" })]),
            status: 200,
            text: "",
            check: |s| assert!(s.roles.contains_key("app_rw") && !s.roles.contains_key("r1")),
        },
        Rule {
            what: "the owner re-passworded",
            rules: sh,
            setup: none,
            body: roles_delta(vec![set("postgres", STRONG, SCRAM)]),
            status: 200,
            text: "",
            check: |s| {
                let r = &s.roles["postgres"];
                assert_eq!(r.origin, RoleOrigin::Owner);
                assert_eq!(r.scram_verifier.as_deref(), Some(SCRAM));
                assert!(s.owner_password_cleared);
            },
        },
        Rule {
            what: "the owner with a weak password and strength on",
            rules: sh_strong,
            setup: none,
            body: roles_delta(vec![set("postgres", WEAK, SCRAM)]),
            status: 409,
            text: "password too weak: use at least 12 characters and mix letters, digits and symbols (about 60 bits)",
            check: |s| assert!(!s.owner_password_cleared),
        },
        Rule {
            what: "PASSWORD NULL keeps an sql row without its verifier",
            rules: sh,
            setup: two_sql_copies,
            body: roles_delta(vec![json!({ "op": "set", "name": "r1" })]),
            status: 200,
            text: "",
            check: |s| assert!(s.roles["r1"].scram_verifier.is_none() && s.roles.contains_key("r2")),
        },
        Rule {
            what: "a group role gets a row without a password",
            rules: sh,
            setup: none,
            body: roles_delta(vec![json!({ "op": "set", "name": "grp" })]),
            status: 200,
            text: "",
            check: |s| {
                assert_eq!((s.roles["grp"].origin, s.roles["grp"].scram_verifier.as_deref()), (RoleOrigin::Sql, None));
                assert_eq!(s.roles_version, 1);
            },
        },
        Rule {
            what: "the owner's PASSWORD NULL",
            rules: sh,
            setup: none,
            body: roles_delta(vec![json!({ "op": "set", "name": "postgres" })]),
            status: 409,
            text: "the branch owner's password can't be removed; reset it instead",
            check: |s| assert!(s.roles.contains_key("postgres")),
        },
        Rule {
            what: "a rename moves the copy and its keys",
            rules: sh,
            setup: |s| {
                two_sql_copies(s);
                s.keys.push((uuid::Uuid::new_v4(), "k".into(), "r1".into(), false));
            },
            body: roles_delta(vec![json!({ "op": "set", "name": "r1b", "old_name": "r1" })]),
            status: 200,
            text: "",
            check: |s| {
                assert!(!s.roles.contains_key("r1"));
                assert_eq!(s.roles["r1b"].scram_verifier.as_deref(), Some(SCRAM));
                assert_eq!(s.keys[0].2, "r1b");
            },
        },
        Rule {
            what: "a rename with a password (created and renamed in one transaction)",
            rules: sh,
            setup: none,
            body: roles_delta(vec![json!({
                "op": "set", "name": "app_rw", "old_name": "app_a",
                "password": STRONG, "encrypted_password": SCRAM
            })]),
            status: 200,
            text: "",
            check: app_rw_has_scram,
        },
        Rule {
            what: "a reserved rename target",
            rules: sh,
            setup: two_sql_copies,
            body: roles_delta(vec![json!({ "op": "set", "name": "authenticator", "old_name": "r1" })]),
            status: 409,
            text: "the role name \"authenticator\" is reserved by Chelabase",
            check: |s| assert!(s.roles.contains_key("r1")),
        },
        Rule {
            what: "a reserved rename source",
            rules: sh,
            setup: none,
            body: roles_delta(vec![json!({ "op": "set", "name": "mine", "old_name": "chelabase_admin" })]),
            status: 409,
            text: "the role name \"chelabase_admin\" is reserved by Chelabase",
            check: nothing,
        },
        Rule {
            what: "the owner renamed",
            rules: sh,
            setup: none,
            body: roles_delta(vec![json!({ "op": "set", "name": "boss", "old_name": "postgres" })]),
            status: 409,
            text: "the branch owner \"postgres\" can't be dropped or renamed",
            check: |s| assert!(s.roles.contains_key("postgres")),
        },
        Rule {
            what: "a key-backed drop",
            rules: sh,
            setup: |s| {
                two_sql_copies(s);
                s.keys.push((uuid::Uuid::new_v4(), "k1".into(), "r1".into(), false));
                s.keys.push((uuid::Uuid::new_v4(), "k2".into(), "r1".into(), false));
                s.keys.push((uuid::Uuid::new_v4(), "old".into(), "r1".into(), true));
            },
            body: roles_delta(vec![json!({ "op": "del", "name": "r1" })]),
            status: 409,
            text: "role \"r1\" backs data API key(s) k1, k2; revoke them first",
            check: |s| assert!(s.roles.contains_key("r1")),
        },
        Rule {
            what: "a drop with only revoked keys",
            rules: sh,
            setup: |s| {
                two_sql_copies(s);
                s.keys.push((uuid::Uuid::new_v4(), "old".into(), "r1".into(), true));
            },
            body: roles_delta(vec![json!({ "op": "del", "name": "r1" })]),
            status: 200,
            text: "",
            check: |s| assert!(!s.roles.contains_key("r1")),
        },
        Rule {
            what: "the owner dropped",
            rules: sh,
            setup: none,
            body: roles_delta(vec![json!({ "op": "del", "name": "postgres" })]),
            status: 409,
            text: "the branch owner \"postgres\" can't be dropped or renamed",
            check: |s| assert!(s.roles.contains_key("postgres")),
        },
        Rule {
            what: "a reserved name dropped",
            rules: sh,
            setup: none,
            body: roles_delta(vec![json!({ "op": "del", "name": "authenticated" })]),
            status: 409,
            text: "the role name \"authenticated\" is reserved by Chelabase",
            check: nothing,
        },
        Rule {
            what: "a new database",
            rules: sh,
            setup: none,
            body: dbs_delta(vec![json!({ "op": "set", "name": "extra", "owner": "postgres" })]),
            status: 200,
            text: "",
            check: |s| assert_eq!(s.dbs["extra"], ("postgres".to_string(), DatabaseOrigin::Sql)),
        },
        Rule {
            what: "the database cap reached",
            rules: with_caps(sh, 50, 2),
            setup: |s| {
                s.dbs.insert("extra".into(), ("postgres".into(), DatabaseOrigin::Sql));
            },
            body: dbs_delta(vec![json!({ "op": "set", "name": "more", "owner": "postgres" })]),
            status: 409,
            text: "this branch has reached its limit of 2 databases (CHELABASE_BRANCH_DATABASE_CAP)",
            check: |s| assert!(!s.dbs.contains_key("more")),
        },
        Rule {
            what: "the database cap doesn't count the system database",
            rules: with_caps(sh, 50, 2),
            setup: none,
            body: dbs_delta(vec![json!({ "op": "set", "name": "extra", "owner": "postgres" })]),
            status: 200,
            text: "",
            check: |s| assert!(s.dbs.contains_key("extra")),
        },
        Rule {
            what: "an owner change at the database cap",
            rules: with_caps(sh, 50, 2),
            setup: |s| {
                s.dbs.insert("extra".into(), ("postgres".into(), DatabaseOrigin::Sql));
            },
            body: dbs_delta(vec![json!({ "op": "set", "name": "extra", "owner": "app_rw" })]),
            status: 200,
            text: "",
            check: |s| assert_eq!(s.dbs["extra"].0, "app_rw"),
        },
        Rule {
            what: "the main database renamed",
            rules: sh,
            setup: none,
            body: dbs_delta(vec![json!({ "op": "set", "name": "other", "old_name": "neondb" })]),
            status: 409,
            text: "the branch's main database \"neondb\" can't be renamed",
            check: |s| assert!(s.dbs.contains_key("neondb")),
        },
        Rule {
            what: "another database renamed",
            rules: sh,
            setup: |s| {
                s.dbs.insert("extra".into(), ("postgres".into(), DatabaseOrigin::Sql));
            },
            body: dbs_delta(vec![json!({ "op": "set", "name": "renamed", "old_name": "extra" })]),
            status: 200,
            text: "",
            check: |s| {
                assert!(!s.dbs.contains_key("extra"));
                assert_eq!(s.dbs["renamed"], ("postgres".to_string(), DatabaseOrigin::Sql));
            },
        },
        Rule {
            what: "the main database dropped (Q1)",
            rules: sh,
            setup: none,
            body: dbs_delta(vec![json!({ "op": "del", "name": "neondb" })]),
            status: 200,
            text: "",
            check: |s| assert!(!s.dbs.contains_key("neondb")),
        },
        Rule {
            what: "the main database re-created",
            rules: sh,
            setup: |s| {
                s.dbs.remove("neondb");
            },
            body: dbs_delta(vec![json!({ "op": "set", "name": "neondb", "owner": "postgres" })]),
            status: 200,
            text: "",
            check: |s| assert_eq!(s.dbs["neondb"], ("postgres".to_string(), DatabaseOrigin::Main)),
        },
        Rule {
            what: "another database dropped",
            rules: sh,
            setup: |s| {
                s.dbs.insert("extra".into(), ("postgres".into(), DatabaseOrigin::Sql));
            },
            body: dbs_delta(vec![json!({ "op": "del", "name": "extra" })]),
            status: 200,
            text: "",
            check: |s| assert!(!s.dbs.contains_key("extra")),
        },
        Rule {
            what: "a branch-less endpoint skips databases",
            rules: sh,
            setup: |s| s.branch = None,
            body: dbs_delta(vec![
                json!({ "op": "set", "name": "extra", "owner": "postgres" }),
                json!({ "op": "set", "name": "other", "old_name": "neondb" }),
                json!({ "op": "del", "name": "neondb" }),
            ]),
            status: 200,
            text: "",
            check: |s| {
                assert!(!s.dbs.contains_key("extra"));
                assert!(s.dbs.contains_key("neondb"));
            },
        },
    ]
}

#[tokio::test]
async fn ddl_rules_table() {
    for rule in rows() {
        let (neon, _) = ddl_neon();
        neon.edit(rule.setup);
        let app = spawn_internal_with(
            neon.clone(),
            FakeLocator::default(),
            "http://127.0.0.1:9",
            rule.rules,
            FakeCache::default(),
        )
        .await;
        let (status, ctype, text) = app.ddl("ep-d", Some(COMPUTE_TOKEN), &rule.body).await;
        assert_eq!((status, text.as_str()), (rule.status, rule.text), "{}", rule.what);
        if status != 200 {
            assert!(ctype.starts_with("text/plain"), "{}: {ctype}", rule.what);
            assert!(text.len() <= 200 && !text.contains('\n'), "{}", rule.what);
        }
        (rule.check)(&neon.ddl_state());
    }
}

#[tokio::test]
async fn ddl_is_idempotent() {
    let (neon, _) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    let body = json!({
        "roles": [set("app_rw", STRONG, SCRAM), { "op": "del", "name": "ghost" }],
        "dbs": [{ "op": "set", "name": "extra", "owner": "app_rw" }],
    })
    .to_string();
    let (first, _, _) = app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await;
    let mut after_first = neon.ddl_state();
    let (second, _, _) = app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await;
    let mut after_second = neon.ddl_state();
    assert_eq!((first, second), (200, 200));
    // Only the bookkeeping differs: the version counter and the copy's timestamps.
    after_first.roles_version = 0;
    after_second.roles_version = 0;
    for s in [&mut after_first, &mut after_second] {
        for r in s.roles.values_mut() {
            r.created_at = chrono::DateTime::UNIX_EPOCH;
        }
    }
    assert_eq!(after_first, after_second);
    let ghost = roles_delta(vec![json!({ "op": "del", "name": "ghost" })]);
    assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &ghost).await.0, 200);
    assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &ghost).await.0, 200);
}

#[tokio::test]
async fn ddl_rename_reseals_an_api_password() {
    let (neon, branch) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    let sealed =
        chelabase_secrets::seal_role_password(&app.key, &branch.to_string(), "api_r", "the-secret");
    neon.edit(|s| {
        s.roles
            .insert("api_r".into(), copy("api_r", RoleOrigin::Api, Some(SCRAM), Some(&sealed)));
    });
    let body = roles_delta(vec![json!({ "op": "set", "name": "api_r2", "old_name": "api_r" })]);
    assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await.0, 200);
    let state = neon.ddl_state();
    let row = &state.roles["api_r2"];
    assert_eq!(row.origin, RoleOrigin::Api);
    let resealed = row.password_enc.as_deref().expect("resealed");
    let b = branch.to_string();
    assert_eq!(
        chelabase_secrets::open_role_password(&app.key, &b, "api_r2", resealed).unwrap(),
        "the-secret"
    );
    assert!(chelabase_secrets::open_role_password(&app.key, &b, "api_r", resealed).is_err());
}

// A role made, renamed and given `PASSWORD NULL` in one transaction reaches the receiver as
// one op for `c` (old name `b`, password null) while neither name has a row: nothing moves,
// and `c` exists without a password, so it gets a row with no verifier.
#[tokio::test]
async fn ddl_rename_of_an_unknown_role_with_null_password_leaves_a_row_without_a_password() {
    let (neon, _) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    let body = roles_delta(vec![json!({ "op": "set", "name": "c", "old_name": "b", "password": null })]);
    assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await.0, 200);
    let state = neon.ddl_state();
    assert!(!state.roles.contains_key("b"), "{:?}", state.roles.keys());
    assert_eq!((state.roles["c"].origin, state.roles["c"].scram_verifier.as_deref()), (RoleOrigin::Sql, None));
}

// A group role made in SQL (`CREATE ROLE billing`, no password) gets a row without a
// verifier, so members can be granted it; it never counts against the login-role cap.
#[tokio::test]
async fn ddl_group_role_gets_a_row_without_a_password() {
    let (neon, _) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    let body = roles_delta(vec![json!({ "op": "set", "name": "billing", "password": null, "login": false })]);
    assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await.0, 200);
    let row = neon.ddl_state().roles["billing"].clone();
    assert_eq!((row.origin, row.scram_verifier.as_deref(), row.login), (RoleOrigin::Sql, None, false));
}

fn rename(old: &str, new: &str) -> Value {
    json!({ "op": "set", "name": new, "old_name": old })
}

fn open(app: &Internal, branch: BranchId, name: &str, sealed: Option<&str>) -> String {
    let sealed = sealed.unwrap_or_else(|| panic!("{name} is revealable"));
    chelabase_secrets::open_role_password(&app.key, &branch.to_string(), name, sealed)
        .unwrap_or_else(|e| panic!("{name}: {e}"))
}

// M2: swapping two roles' names in one transaction (`a` → `tmp`, `b` → `a`, `tmp` → `b`)
// reaches the receiver as two renames that each land on the other's name. Both rows move,
// with their secrets (sealed again under the new names, M4 for the `sql` one) and the keys.
#[tokio::test]
async fn ddl_swapping_two_role_names() {
    let (neon, branch) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    let b = branch.to_string();
    let seal = |name: &str, pw: &str| chelabase_secrets::seal_role_password(&app.key, &b, name, pw);
    let key = uuid::Uuid::new_v4();
    neon.edit(|s| {
        s.roles.insert("a".into(), copy("a", RoleOrigin::Api, Some("SCRAM-SHA-256$a"), Some(&seal("a", "pw-a"))));
        s.roles.insert("b".into(), copy("b", RoleOrigin::Sql, Some("SCRAM-SHA-256$b"), Some(&seal("b", "pw-b"))));
        s.keys.push((key, "k".into(), "a".into(), false));
    });
    for order in [vec![rename("b", "a"), rename("a", "b")], vec![rename("a", "b"), rename("b", "a")]] {
        let (status, _, text) = app.ddl("ep-d", Some(COMPUTE_TOKEN), &roles_delta(order)).await;
        assert_eq!(status, 200, "{text}");
        let state = neon.ddl_state();
        let names: Vec<&String> = state.roles.keys().collect();
        assert_eq!(names, ["a", "b", "postgres"]);
        let (a, b) = (&state.roles["a"], &state.roles["b"]);
        assert_eq!((b.origin, b.scram_verifier.as_deref()), (RoleOrigin::Api, Some("SCRAM-SHA-256$a")));
        assert_eq!((a.origin, a.scram_verifier.as_deref()), (RoleOrigin::Sql, Some("SCRAM-SHA-256$b")));
        assert_eq!(open(&app, branch, "b", b.password_enc.as_deref()), "pw-a");
        assert_eq!(open(&app, branch, "a", a.password_enc.as_deref()), "pw-b");
        assert_eq!(state.keys[0].2, "b", "the key follows its role");
        // Swap back for the other order.
        neon.edit(|s| {
            let (a, b) = (s.roles.remove("a").unwrap(), s.roles.remove("b").unwrap());
            s.roles.insert("a".into(), copy("a", RoleOrigin::Api, b.scram_verifier.as_deref(), Some(&seal("a", "pw-a"))));
            s.roles.insert("b".into(), copy("b", RoleOrigin::Sql, a.scram_verifier.as_deref(), Some(&seal("b", "pw-b"))));
            s.keys[0].2 = "a".into();
        });
    }
}

// Re-review: `DROP ROLE a; ALTER ROLE b RENAME TO a` in one transaction arrives as the rename
// alone (Neon folds the drop into it). A live secret key on `a` refuses it with the text a
// `DROP ROLE a` gets, and nothing is written; also when `a` has no row (a role without a
// password can back a key). Without a live key the rename is taken.
#[tokio::test]
async fn ddl_rename_onto_a_dropped_keys_role_is_refused() {
    for target_row in [true, false] {
        let (neon, _) = ddl_neon();
        let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
        neon.edit(|s| {
            if target_row {
                s.roles.insert("a".into(), copy("a", RoleOrigin::Sql, Some("SCRAM-SHA-256$a"), None));
            }
            s.roles.insert("b".into(), copy("b", RoleOrigin::Sql, Some("SCRAM-SHA-256$b"), None));
            s.keys.push((uuid::Uuid::new_v4(), "ci-key".into(), "a".into(), false));
            s.keys.push((uuid::Uuid::new_v4(), "old-key".into(), "a".into(), true));
        });
        let before = neon.ddl_state();
        let (status, _, text) = app
            .ddl("ep-d", Some(COMPUTE_TOKEN), &roles_delta(vec![rename("b", "a")]))
            .await;
        assert_eq!(
            (status, text.as_str()),
            (409, "role \"a\" backs data API key(s) ci-key; revoke them first"),
            "target row: {target_row}"
        );
        assert_eq!(neon.ddl_state(), before, "nothing written");
    }
    // Revoked keys only: taken.
    let (neon, _) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    neon.edit(|s| {
        s.roles.insert("a".into(), copy("a", RoleOrigin::Sql, Some("SCRAM-SHA-256$a"), None));
        s.roles.insert("b".into(), copy("b", RoleOrigin::Sql, Some("SCRAM-SHA-256$b"), None));
        s.keys.push((uuid::Uuid::new_v4(), "old-key".into(), "a".into(), true));
    });
    let (status, _, text) = app
        .ddl("ep-d", Some(COMPUTE_TOKEN), &roles_delta(vec![rename("b", "a")]))
        .await;
    assert_eq!(status, 200, "{text}");
    let state = neon.ddl_state();
    assert!(!state.roles.contains_key("b"));
    assert_eq!(state.roles["a"].scram_verifier.as_deref(), Some("SCRAM-SHA-256$b"));
}

// A role with no row (NOLOGIN, no password: `authenticated` is the usual one) backs keys too.
// `DROP app; CREATE app; ALTER app RENAME TO app2` reaches the receiver as one entry (renamed,
// recreated): the keys moved to `app2` with the rename, so the recreate check finds them.
#[tokio::test]
async fn ddl_drop_create_rename_of_a_rowless_role_with_a_key_is_refused() {
    let (neon, _) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    neon.edit(|s| s.keys.push((uuid::Uuid::new_v4(), "ci-key".into(), "app".into(), false)));
    let before = neon.ddl_state();
    let mut op = json!({ "op": "set", "name": "app2", "old_name": "app", "recreated": true });
    op["touched"] = json!(true);
    let (status, _, text) = app.ddl("ep-d", Some(COMPUTE_TOKEN), &roles_delta(vec![op])).await;
    assert_eq!(
        (status, text.as_str()),
        (409, "role \"app2\" backs data API key(s) ci-key; revoke them first")
    );
    assert_eq!(neon.ddl_state(), before, "nothing written");
}

// `ALTER ROLE app RENAME TO x; CREATE ROLE app`: the key goes with the old role to `x`, and the
// new `app` starts with none (live and revoked alike).
#[tokio::test]
async fn ddl_rename_of_a_rowless_role_then_create_of_its_old_name() {
    let (neon, _) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    neon.edit(|s| {
        s.keys.push((uuid::Uuid::new_v4(), "ci-key".into(), "app".into(), false));
        s.keys.push((uuid::Uuid::new_v4(), "old-key".into(), "app".into(), true));
    });
    let body = roles_delta(vec![rename("app", "x"), set("app", STRONG, SCRAM2)]);
    let (status, _, text) = app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await;
    assert_eq!(status, 200, "{text}");
    let state = neon.ddl_state();
    assert!(state.keys.iter().all(|k| k.2 == "x"), "{:?}", state.keys);
    assert!(state.roles.contains_key("app"), "the new role has its copy");
}

// A swap of two roles that both back live keys is taken: each key follows its role, whether
// both roles have a row, neither has, or only one does.
#[tokio::test]
async fn ddl_swap_with_keys_on_both_roles() {
    for (row_a, row_b) in [(true, true), (false, false), (true, false), (false, true)] {
        let (neon, _) = ddl_neon();
        let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
        neon.edit(|s| {
            if row_a {
                s.roles.insert("a".into(), copy("a", RoleOrigin::Sql, Some("SCRAM-SHA-256$a"), None));
            }
            if row_b {
                s.roles.insert("b".into(), copy("b", RoleOrigin::Sql, Some("SCRAM-SHA-256$b"), None));
            }
            s.keys.push((uuid::Uuid::new_v4(), "key-a".into(), "a".into(), false));
            s.keys.push((uuid::Uuid::new_v4(), "key-b".into(), "b".into(), false));
        });
        let (status, _, text) = app
            .ddl("ep-d", Some(COMPUTE_TOKEN), &roles_delta(vec![rename("a", "b"), rename("b", "a")]))
            .await;
        assert_eq!(status, 200, "rows {row_a}/{row_b}: {text}");
        let state = neon.ddl_state();
        // A row follows its name; a name without one stays without.
        assert_eq!(state.roles.contains_key("b"), row_a, "rows {row_a}/{row_b}");
        assert_eq!(state.roles.contains_key("a"), row_b, "rows {row_a}/{row_b}");
        if row_a {
            assert_eq!(state.roles["b"].scram_verifier.as_deref(), Some("SCRAM-SHA-256$a"));
        }
        let role_of = |key: &str| state.keys.iter().find(|k| k.1 == key).unwrap().2.clone();
        assert_eq!(
            (role_of("key-a"), role_of("key-b")),
            ("b".to_string(), "a".to_string()),
            "rows {row_a}/{row_b}"
        );
    }
}

// M2: a rename onto a name whose row is stale (the role was dropped in the same transaction,
// which Neon sends as the rename alone, or a leftover row) replaces that row; databases too.
#[tokio::test]
async fn ddl_rename_onto_a_stale_row_replaces_it() {
    let (neon, _) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    neon.edit(|s| {
        s.roles.insert("old".into(), copy("old", RoleOrigin::Sql, Some("SCRAM-SHA-256$old"), None));
        s.roles.insert("a".into(), copy("a", RoleOrigin::Sql, Some("SCRAM-SHA-256$stale"), None));
        s.dbs.insert("d1".into(), ("old".into(), DatabaseOrigin::Sql));
        s.dbs.insert("d2".into(), ("postgres".into(), DatabaseOrigin::Sql));
    });
    let body = json!({
        "roles": [rename("old", "a")],
        "dbs": [{ "op": "set", "name": "d2", "old_name": "d1" }],
    })
    .to_string();
    let (status, _, text) = app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await;
    assert_eq!(status, 200, "{text}");
    let state = neon.ddl_state();
    assert!(!state.roles.contains_key("old"));
    assert_eq!(state.roles["a"].scram_verifier.as_deref(), Some("SCRAM-SHA-256$old"));
    assert!(!state.dbs.contains_key("d1"));
    assert_eq!(state.dbs["d2"], ("old".to_string(), DatabaseOrigin::Sql));
}

#[tokio::test]
async fn ddl_first_refusal_rolls_back_everything() {
    let (neon, _) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    let before = neon.ddl_state();
    let body = json!({
        "roles": [set("app_rw", STRONG, SCRAM), { "op": "del", "name": "postgres" }],
        "dbs": [{ "op": "set", "name": "extra", "owner": "postgres" }],
    })
    .to_string();
    let (status, _, text) = app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await;
    assert_eq!(status, 409, "{text}");
    assert_eq!(neon.ddl_state(), before, "nothing written");
}

#[tokio::test]
async fn ddl_publishes_twice_and_survives_valkey_down() {
    for down in [false, true] {
        let (neon, _) = ddl_neon();
        neon.edit(two_sql_copies);
        let cache = FakeCache::default();
        *cache.down.lock().unwrap() = down;
        let app = spawn_internal_with(
            neon.clone(),
            FakeLocator::default(),
            "http://127.0.0.1:9",
            rules(Mode::SelfHosted, false),
            cache.clone(),
        )
        .await;
        let body = json!({
            "roles": [
                set("app_rw", STRONG, SCRAM),
                { "op": "set", "name": "r1b", "old_name": "r1" },
            ],
            "dbs": [{ "op": "set", "name": "extra", "owner": "postgres" }],
        })
        .to_string();
        assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await.0, 200, "down={down}");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let count = |sent: &[(String, String)], role: &str| {
            sent.iter().filter(|(ep, r)| ep == "ep-d" && r == role).count()
        };
        let early = cache.sent.lock().unwrap().clone();
        for role in ["app_rw", "r1", "r1b"] {
            assert_eq!(count(&early, role), 1, "at once: {role} down={down}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        let late = cache.sent.lock().unwrap().clone();
        for role in ["app_rw", "r1", "r1b"] {
            assert_eq!(count(&late, role), 2, "again 1 s later: {role} down={down}");
        }
        assert_eq!(late.len(), 6, "only the changed roles: {late:?}");
    }
}

async fn spawn_with_resync(neon: FakeNeon, resync: FakeResync) -> Internal {
    spawn_with_app(
        neon,
        FakeLocator::default(),
        "http://127.0.0.1:9",
        rules(Mode::SelfHosted, false),
        FakeCache::default(),
        "http://127.0.0.1:9",
        AppSetup { resync, ..AppSetup::default() },
    )
    .await
}

/// Polls until `done` holds (the spawned publish ran); false after 2 s.
async fn wait_until(done: impl Fn() -> bool) -> bool {
    for _ in 0..200 {
        if done() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    done()
}

// T12b: a forwarded `NOLOGIN` flags the existing copy (stamped against the resync's 10 s
// guard); the owner and a role with no copy are left alone. No password row is added or removed.
#[tokio::test]
async fn ddl_login_updates_the_existing_copy() {
    let (neon, _) = ddl_neon();
    neon.edit(two_sql_copies);
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    let body = roles_delta(vec![
        json!({ "op": "set", "name": "r1", "login": false, "valid_until": "2030-01-01" }),
        json!({ "op": "set", "name": "postgres", "login": false }),
        json!({ "op": "set", "name": "ghost", "login": false }),
    ]);
    assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await.0, 200);
    let state = neon.ddl_state();
    assert!(!state.roles["r1"].login);
    assert_eq!(state.roles["r1"].scram_verifier.as_deref(), Some(SCRAM), "password untouched");
    assert!(state.roles["r2"].login);
    assert!(state.roles["postgres"].login);
    assert!(!state.roles.contains_key("ghost"));
    assert_eq!(state.stamped.iter().collect::<Vec<_>>(), ["r1"]);
    assert_eq!(state.roles_version, 1);

    let body = roles_delta(vec![json!({ "op": "set", "name": "r1", "login": true })]);
    assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await.0, 200);
    assert!(neon.ddl_state().roles["r1"].login);
}

#[tokio::test]
async fn ddl_a_new_copy_takes_the_forwarded_login() {
    let (neon, _) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    let mut nologin = set("app_rw", STRONG, SCRAM);
    nologin["login"] = json!(false);
    let mut login = set("app_ro", STRONG, SCRAM);
    login["login"] = json!(true);
    let body = roles_delta(vec![nologin, login, set("app_plain", STRONG, SCRAM)]);
    assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await.0, 200);
    let state = neon.ddl_state();
    assert!(!state.roles["app_rw"].login);
    assert!(state.roles["app_ro"].login);
    assert!(state.roles["app_plain"].login, "no login key: the default");
}

#[tokio::test]
async fn ddl_touched_roles_ask_for_one_resync() {
    let (neon, branch) = ddl_neon();
    neon.edit(two_sql_copies);
    let resync = FakeResync::default();
    let app = spawn_with_resync(neon.clone(), resync.clone()).await;
    let before = neon.ddl_state();
    let body = roles_delta(vec![
        json!({ "op": "set", "name": "r1", "touched": true }),
        json!({ "op": "set", "name": "grp", "touched": true }),
        json!({ "op": "set", "name": "r2", "touched": true }),
    ]);
    assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await.0, 200);
    assert!(wait_until(|| !resync.asked.lock().unwrap().is_empty()).await);
    assert_eq!(*resync.asked.lock().unwrap(), [branch], "once, for the branch");
    assert_eq!(neon.begins.load(Ordering::SeqCst), 0, "a touch-only delta takes no lock");
    let state = neon.ddl_state();
    assert_eq!(state.roles, before.roles, "a touch removes no copy and stamps nothing");
    assert!(state.stamped.is_empty());
    assert_eq!(state.roles_version, before.roles_version);
}

// Members T8 (I3, R17): every accepted delta that touches roles (made, dropped, renamed,
// touched attributes or memberships, a password) asks for one member-role reconcile of the
// branch; a refused one asks for none. PB end-of-step R12: a delta that makes a database asks
// for one too (the new database needs the protected-branch guard); one that only drops a
// database asks for none.
#[tokio::test]
async fn ddl_role_changes_ask_for_a_member_reconcile() {
    let (neon, branch) = ddl_neon();
    neon.edit(two_sql_copies);
    let resync = FakeResync::default();
    let app = spawn_with_resync(neon.clone(), resync.clone()).await;
    let db = dbs_delta(vec![json!({ "op": "set", "name": "extra", "owner": "postgres" })]);
    assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &db).await.0, 200);
    assert!(wait_until(|| resync.members_asked.lock().unwrap().len() == 1).await, "a new database");
    let dropped = dbs_delta(vec![json!({ "op": "del", "name": "extra" })]);
    assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &dropped).await.0, 200);
    let refused = roles_delta(vec![set("anon", STRONG, SCRAM)]);
    assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &refused).await.0, 409);
    for (i, op) in [
        set("app_rw", STRONG, SCRAM),
        json!({ "op": "set", "name": "grp", "touched": true }),
        json!({ "op": "set", "name": "r9", "old_name": "r1" }),
        json!({ "op": "del", "name": "grp" }),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &roles_delta(vec![op.clone()])).await.0, 200, "{op}");
        assert!(wait_until(|| resync.members_asked.lock().unwrap().len() == i + 2).await, "{op}");
    }
    assert_eq!(*resync.members_asked.lock().unwrap(), [branch; 5]);
}

// M1: dropping a group role changes its members' derived levels, so a `del` resyncs too.
#[tokio::test]
async fn ddl_a_role_drop_asks_for_one_resync() {
    let (neon, branch) = ddl_neon();
    neon.edit(two_sql_copies);
    let resync = FakeResync::default();
    let app = spawn_with_resync(neon.clone(), resync.clone()).await;
    let body = roles_delta(vec![json!({ "op": "del", "name": "grp" })]);
    assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await.0, 200);
    assert!(wait_until(|| !resync.asked.lock().unwrap().is_empty()).await);
    assert_eq!(*resync.asked.lock().unwrap(), [branch], "once, for the branch");
}

#[tokio::test]
async fn ddl_untouched_and_refused_requests_ask_for_no_resync() {
    let (neon, _) = ddl_neon();
    let resync = FakeResync::default();
    let app = spawn_with_resync(neon.clone(), resync.clone()).await;
    // Passwords only (an old payload), then a refused delta that is also touched.
    let body = roles_delta(vec![set("app_rw", STRONG, SCRAM)]);
    assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await.0, 200);
    let body = roles_delta(vec![json!({ "op": "set", "name": "anon", "touched": true }), {
        set("anon", STRONG, SCRAM)
    }]);
    assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await.0, 409);
    // A later touched request marks the point by which a wrongly sent request would show.
    let body = roles_delta(vec![json!({ "op": "set", "name": "grp", "touched": true })]);
    assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await.0, 200);
    assert!(wait_until(|| !resync.asked.lock().unwrap().is_empty()).await);
    assert_eq!(resync.asked.lock().unwrap().len(), 1, "only the later, accepted request");
}

// The route is reachable from every compute's network: a body over 64 KiB is refused before
// it's read into memory; one just under the limit is taken.
#[tokio::test]
async fn ddl_body_is_limited_to_64_kib() {
    let (neon, _) = ddl_neon();
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;
    let delta = roles_delta(vec![set("app_rw", STRONG, SCRAM)]);
    let padded = |len: usize| format!("{delta}{}", " ".repeat(len - delta.len()));
    let (status, _, _) = app.ddl("ep-d", Some(COMPUTE_TOKEN), &padded(64 * 1024)).await;
    assert_eq!(status, 200);
    let (status, _, _) = app.ddl("ep-d", Some(COMPUTE_TOKEN), &padded(64 * 1024 + 1)).await;
    assert_eq!(status, 413);
}

// C6: the token is checked before the body is buffered. A raw request announces a body over
// the limit but never sends it: without the token the answer is 401 (not 413) and arrives
// while the body is still unread.
#[tokio::test]
async fn ddl_token_is_checked_before_the_body_is_buffered() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (neon, _) = ddl_neon();
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;
    let addr = app.base.trim_start_matches("http://").to_string();
    for (auth, expected) in [("", "401"), ("Authorization: Bearer wrong\r\n", "401")] {
        let mut sock = tokio::net::TcpStream::connect(&addr).await.unwrap();
        let head = format!(
            "PATCH /compute/api/v2/computes/ep-d/ddl HTTP/1.1\r\nHost: x\r\n{auth}\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            64 * 1024 + 1
        );
        sock.write_all(head.as_bytes()).await.unwrap();
        let mut buf = vec![0u8; 12];
        tokio::time::timeout(std::time::Duration::from_secs(5), sock.read_exact(&mut buf))
            .await
            .expect("answered without the body")
            .unwrap();
        assert_eq!(&buf[9..12], expected.as_bytes(), "{auth}");
    }
}

// C7: an explicit `"password": null` next to `old_name` is a rename that removes the password:
// the row moves and keeps no verifier.
#[tokio::test]
async fn ddl_rename_with_explicit_null_password_removes_the_password() {
    let (neon, _) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    neon.edit(|s| {
        s.roles
            .insert("r1".into(), copy("r1", RoleOrigin::Sql, Some(SCRAM), None));
    });
    let body = roles_delta(vec![
        json!({ "op": "set", "name": "r2", "old_name": "r1", "password": null }),
    ]);
    assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await.0, 200);
    let state = neon.ddl_state();
    assert!(!state.roles.contains_key("r1"));
    assert_eq!(state.roles["r2"].scram_verifier, None);
}

// C7 and E8: the owner can't lose its password, with or without a rename.
#[tokio::test]
async fn ddl_rename_with_explicit_null_on_the_owner_is_refused() {
    let (neon, _) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    let body = roles_delta(vec![json!({ "op": "set", "name": "postgres", "password": null })]);
    let (status, _, text) = app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await;
    assert_eq!(status, 409);
    assert_eq!(text, "the branch owner's password can't be removed; reset it instead");
    let body = roles_delta(vec![
        json!({ "op": "set", "name": "pg2", "old_name": "postgres", "password": null }),
    ]);
    let (status, _, text) = app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await;
    assert_eq!(status, 409);
    assert_eq!(text, "the branch owner \"postgres\" can't be dropped or renamed");
    assert!(neon.ddl_state().roles.contains_key("postgres"));
}

// C7: no `password` key at all is a plain rename; the copy (and its verifier) is kept.
#[tokio::test]
async fn ddl_rename_without_password_key_keeps_the_copy() {
    let (neon, _) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    neon.edit(|s| {
        s.roles
            .insert("r1".into(), copy("r1", RoleOrigin::Sql, Some(SCRAM), None));
    });
    let body = roles_delta(vec![rename("r1", "r2")]);
    assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await.0, 200);
    let state = neon.ddl_state();
    assert!(!state.roles.contains_key("r1"));
    assert_eq!(state.roles["r2"].scram_verifier.as_deref(), Some(SCRAM));
}

// T13d (re-review I1): `ALTER ROLE … VALID UNTIL` alone arrives as a `set` with `valid_until`
// and no password key. It is an attribute change: the owner is not refused, and an API-created
// row keeps its copy and its sealed password.
#[tokio::test]
async fn ddl_valid_until_alone_is_an_attribute_change() {
    let (neon, _) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    neon.edit(|s| {
        s.roles
            .insert("api_r".into(), copy("api_r", RoleOrigin::Api, Some(SCRAM), Some("sealed")));
    });
    let before = neon.ddl_state();
    let body = roles_delta(vec![
        json!({ "op": "set", "name": "postgres", "valid_until": "2030-01-01" }),
    ]);
    let (status, _, text) = app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await;
    assert_eq!(status, 200, "owner: {text}");
    let body = roles_delta(vec![
        json!({ "op": "set", "name": "api_r", "valid_until": "infinity" }),
    ]);
    let (status, _, text) = app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await;
    assert_eq!(status, 200, "api row: {text}");
    let state = neon.ddl_state();
    assert_eq!(state.roles, before.roles, "no row removed or changed");
    assert_eq!(state.roles["api_r"].password_enc.as_deref(), Some("sealed"));
}

// T13d (D20): the fork marks a role dropped and created again in one transaction with
// `"recreated": true`. The drop's live-key refusal applies, with or without a new password.
#[tokio::test]
async fn ddl_recreated_role_with_a_live_key_is_refused() {
    for with_password in [true, false] {
        let (neon, _) = ddl_neon();
        let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
        neon.edit(|s| {
            s.roles.insert("app".into(), copy("app", RoleOrigin::Sql, Some(SCRAM), None));
            s.keys.push((uuid::Uuid::new_v4(), "ci-key".into(), "app".into(), false));
            s.keys.push((uuid::Uuid::new_v4(), "old-key".into(), "app".into(), true));
        });
        let before = neon.ddl_state();
        let mut op = if with_password {
            set("app", STRONG, SCRAM2)
        } else {
            json!({ "op": "set", "name": "app", "password": null })
        };
        op["recreated"] = json!(true);
        op["touched"] = json!(true);
        let (status, _, text) = app.ddl("ep-d", Some(COMPUTE_TOKEN), &roles_delta(vec![op])).await;
        assert_eq!(
            (status, text.as_str()),
            (409, "role \"app\" backs data API key(s) ci-key; revoke them first"),
            "with password: {with_password}"
        );
        assert_eq!(neon.ddl_state(), before, "nothing written");
    }
}

#[tokio::test]
async fn ddl_recreated_role_without_keys_is_taken() {
    // With a password: the copy gets the new verifier.
    let (neon, _) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    neon.edit(|s| {
        s.roles.insert("app".into(), copy("app", RoleOrigin::Sql, Some(SCRAM), None));
        s.keys.push((uuid::Uuid::new_v4(), "old-key".into(), "app".into(), true));
    });
    let mut op = set("app", STRONG, SCRAM2);
    op["recreated"] = json!(true);
    op["touched"] = json!(true);
    let (status, _, text) = app.ddl("ep-d", Some(COMPUTE_TOKEN), &roles_delta(vec![op])).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(neon.ddl_state().roles["app"].scram_verifier.as_deref(), Some(SCRAM2));
    // Without a password: the old row goes and the new role's row has no verifier.
    let mut op = json!({ "op": "set", "name": "app", "password": null, "recreated": true });
    op["touched"] = json!(true);
    let (status, _, text) = app.ddl("ep-d", Some(COMPUTE_TOKEN), &roles_delta(vec![op])).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(neon.ddl_state().roles["app"].scram_verifier, None);
}

// I1: a recreated role is a new role. Its row starts fresh (origin `sql`, plain, no API
// fields, not pending), so the resync can't treat it as the dropped API role and give it back
// owner attributes or `neon_superuser`.
#[tokio::test]
async fn ddl_recreated_role_starts_from_a_fresh_row() {
    let (neon, _) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    neon.edit(|s| {
        let mut old = copy("app", RoleOrigin::Api, Some(SCRAM), Some("sealed"));
        old.level = RoleLevel::OwnerLevel;
        old.api_owner_level = true;
        old.pending = true;
        old.login = false;
        s.roles.insert("app".into(), old);
    });
    let mut op = set("app", STRONG, SCRAM2);
    op["recreated"] = json!(true);
    op["touched"] = json!(true);
    let (status, _, text) = app.ddl("ep-d", Some(COMPUTE_TOKEN), &roles_delta(vec![op])).await;
    assert_eq!(status, 200, "{text}");
    let row = neon.ddl_state().roles["app"].clone();
    assert_eq!(row.origin, RoleOrigin::Sql);
    assert_eq!(row.level, RoleLevel::Plain);
    assert!(!row.api_owner_level && !row.pending, "no API fields carried over");
    assert!(row.login, "the new role's defaults, not the dropped one's");
    assert_eq!(row.scram_verifier.as_deref(), Some(SCRAM2));
    assert_eq!(row.password_enc, None);
}

// Not recreated, with live keys: an ordinary password change, taken as before.
#[tokio::test]
async fn ddl_a_password_change_with_a_live_key_is_still_taken() {
    let (neon, _) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    neon.edit(|s| {
        s.roles.insert("app".into(), copy("app", RoleOrigin::Sql, Some(SCRAM), None));
        s.keys.push((uuid::Uuid::new_v4(), "ci-key".into(), "app".into(), false));
    });
    let mut op = set("app", STRONG, SCRAM2);
    op["recreated"] = json!(false);
    let (status, _, text) = app.ddl("ep-d", Some(COMPUTE_TOKEN), &roles_delta(vec![op])).await;
    assert_eq!(status, 200, "{text}");
    assert_eq!(neon.ddl_state().roles["app"].scram_verifier.as_deref(), Some(SCRAM2));
}

// C4: a sealed password that won't open is dropped and the row is marked unreadable.
#[tokio::test]
async fn ddl_rename_marks_an_unopenable_password_unreadable() {
    let (neon, branch) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    // Sealed under another name's AAD, so it won't open for `api_r`.
    let sealed =
        chelabase_secrets::seal_role_password(&app.key, &branch.to_string(), "other", "the-secret");
    neon.edit(|s| {
        s.roles
            .insert("api_r".into(), copy("api_r", RoleOrigin::Api, Some(SCRAM), Some(&sealed)));
    });
    let body = roles_delta(vec![rename("api_r", "api_r2")]);
    assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await.0, 200);
    let state = neon.ddl_state();
    assert!(state.roles["api_r2"].password_enc.is_none());
    assert!(state.unreadable.contains("api_r2"));
    assert!(!state.unreadable.contains("api_r"));
}

// T5: the owner's password set in SQL reaches the proxies too (at once and again later).
#[tokio::test]
async fn ddl_owner_password_is_published() {
    let (neon, _) = ddl_neon();
    let cache = FakeCache::default();
    let app = spawn_internal_with(
        neon.clone(),
        FakeLocator::default(),
        "http://127.0.0.1:9",
        rules(Mode::SelfHosted, false),
        cache.clone(),
    )
    .await;
    let body = roles_delta(vec![set("postgres", STRONG, SCRAM)]);
    assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await.0, 200);
    assert_eq!(neon.ddl_state().roles["postgres"].scram_verifier.as_deref(), Some(SCRAM));
    tokio::time::sleep(std::time::Duration::from_millis(1300)).await;
    let sent = cache.sent.lock().unwrap().clone();
    let pair = ("ep-d".to_string(), "postgres".to_string());
    assert_eq!(sent, [pair.clone(), pair]);
}

#[tokio::test]
async fn ddl_refusal_publishes_nothing() {
    let (neon, _) = ddl_neon();
    let cache = FakeCache::default();
    let app = spawn_internal_with(
        neon,
        FakeLocator::default(),
        "http://127.0.0.1:9",
        rules(Mode::SelfHosted, false),
        cache.clone(),
    )
    .await;
    let body = roles_delta(vec![set("anon", STRONG, SCRAM)]);
    assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await.0, 409);
    tokio::time::sleep(std::time::Duration::from_millis(1300)).await;
    assert!(cache.sent.lock().unwrap().is_empty());
}

#[tokio::test]
async fn ddl_lock_timeout_is_503() {
    let (neon, _) = ddl_neon();
    *neon.ddl_lock_timeout.lock().unwrap() = true;
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;
    let (status, _, text) = app
        .ddl("ep-d", Some(COMPUTE_TOKEN), &roles_delta(vec![set("a", STRONG, SCRAM)]))
        .await;
    assert_eq!((status, text.as_str()), (503, "role changes are busy; try again"));
}

#[tokio::test]
async fn ddl_metadata_error_is_503() {
    let (neon, _) = ddl_neon();
    neon.break_it();
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;
    let (status, _, text) = app
        .ddl("ep-d", Some(COMPUTE_TOKEN), &roles_delta(vec![set("a", STRONG, SCRAM)]))
        .await;
    assert_eq!(
        (status, text.as_str()),
        (503, "role changes are unavailable right now; try again")
    );
}

#[tokio::test]
async fn ddl_body_never_contains_the_password() {
    let (neon, _) = ddl_neon();
    let app = spawn_internal_with(
        neon,
        FakeLocator::default(),
        "http://127.0.0.1:9",
        rules(Mode::SelfHosted, true),
        FakeCache::default(),
    )
    .await;
    let weak = "hunter2hunter2";
    let body = roles_delta(vec![set("app_rw", "short-pw", SCRAM), set("app_ro", weak, SCRAM)]);
    let (status, _, text) = app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await;
    assert_eq!(status, 409);
    assert!(!text.contains("short-pw") && !text.contains(weak), "{text}");
    // Nor does a malformed body's answer.
    let bad = r#"{"roles":[{"op":"set","name":"x","password":"secret-pw","encrypted_password":7}]}"#;
    let (status, _, text) = app.ddl("ep-d", Some(COMPUTE_TOKEN), bad).await;
    assert_eq!(status, 400);
    assert!(!text.contains("secret-pw"), "{text}");
}

#[tokio::test]
async fn ddl_long_key_lists_stay_within_200_bytes() {
    let (neon, _) = ddl_neon();
    neon.edit(|s| {
        two_sql_copies(s);
        for i in 0..30 {
            s.keys.push((uuid::Uuid::new_v4(), format!("key-number-{i}"), "r1".into(), false));
        }
    });
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;
    let body = roles_delta(vec![json!({ "op": "del", "name": "r1" })]);
    let (status, _, text) = app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await;
    assert_eq!(status, 409);
    assert!(text.len() <= 200, "{} bytes: {text}", text.len());
    assert!(text.starts_with("role \"r1\" backs data API key(s) key-number-0, "), "{text}");
    assert!(text.ends_with("; revoke them first"), "{text}");
}

#[tokio::test]
async fn spec_route_sets_console_url() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let app = spawn_internal(neon, FakeLocator::default(), &storcon(true).await).await;
    let (status, body) = app.get(&spec_path("ep-a"), Some(COMPUTE_TOKEN)).await;
    assert_eq!(status, 200);
    let settings = body["spec"]["cluster"]["settings"].as_array().unwrap();
    let console = settings
        .iter()
        .find(|s| s["name"] == "neon.console_url")
        .expect("neon.console_url");
    assert_eq!(
        console["value"],
        chelabase_neon::spec::ddl_url(CONTROL_PLANE_URI, "ep-a")
    );
}

// ---------------------------------------------------------------------------------------------
// App users (step 6b): branch info, sessions, sync and the Zitadel Actions webhook.
// ---------------------------------------------------------------------------------------------

const ORG: &str = "o1";
const SYNC_PW: &str = "sync-password";
const MAIN_PG: &str = "127.0.0.1:55432";
const CHILD_PG: &str = "127.0.0.1:55435";

/// A project with app users on, and one without.
struct AppWorld {
    neon: FakeNeon,
    locator: FakeLocator,
    key: SecretKey,
    project: ProjectId,
    /// No `project_auth`.
    bare_project: ProjectId,
    auth_db: DatabaseId,
    /// The auth database's main branch (endpoint `ep-main`, running, with a sync row).
    main: BranchId,
    /// A normal branch of the auth database (`ep-child`, running, no sync row).
    child: BranchId,
    /// A backup of the auth database's main.
    backup: BranchId,
    /// The main branch of another database in the same project.
    other: BranchId,
    /// A branch of `bare_project`.
    bare: BranchId,
    /// A deleted branch of the auth database.
    gone: BranchId,
}

fn branch_record(
    id: BranchId,
    db: DatabaseId,
    parent: Option<BranchId>,
    kind: BranchKind,
    status: BranchStatus,
    endpoint: Option<&str>,
) -> BranchRecord {
    let now = chrono::Utc::now();
    BranchRecord {
        id,
        database_id: db,
        name: if parent.is_none() { "main".into() } else { format!("b-{id}") },
        parent_branch_id: parent,
        kind,
        status,
        timeline_id: None,
        pending_timeline_id: None,
        pending_timeline_at: None,
        superseded_timeline_id: None,
        endpoint_id: endpoint.map(str::to_string),
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
    }
}

fn project_record(id: ProjectId) -> ProjectRecord {
    let now = chrono::Utc::now();
    ProjectRecord {
        id,
        tenant_id: TenantId::new(),
        name: "shop".into(),
        slug: format!("shop-{id}"),
        status: ProjectStatus::Active,
        error: None,
        created_at: now,
        updated_at: now,
    }
}

fn app_world() -> AppWorld {
    let neon = FakeNeon::default();
    let locator = FakeLocator::default();
    let key = SecretKey::generate();
    let (project, bare_project) = (ProjectId::new(), ProjectId::new());
    let (auth_db, other_db, bare_db) = (DatabaseId::new(), DatabaseId::new(), DatabaseId::new());
    let (main, child, backup, other, bare, gone) = (
        BranchId::new(),
        BranchId::new(),
        BranchId::new(),
        BranchId::new(),
        BranchId::new(),
        BranchId::new(),
    );
    {
        let mut app = neon.app.lock().unwrap();
        app.projects.push(project_record(project));
        app.projects.push(project_record(bare_project));
        app.db_projects.insert(auth_db, project);
        app.db_projects.insert(other_db, project);
        app.db_projects.insert(bare_db, bare_project);
        use BranchKind::*;
        use BranchStatus::*;
        app.branches.extend([
            branch_record(main, auth_db, None, Normal, Ready, Some("ep-main")),
            branch_record(child, auth_db, Some(main), Normal, Ready, Some("ep-child")),
            branch_record(backup, auth_db, Some(main), Backup, Ready, None),
            branch_record(other, other_db, None, Normal, Ready, Some("ep-other")),
            branch_record(bare, bare_db, None, Normal, Ready, Some("ep-bare")),
            branch_record(gone, auth_db, Some(main), Normal, Deleted, None),
        ]);
        let now = chrono::Utc::now();
        app.auths.push(ProjectAuth {
            project_id: project,
            status: AppUsersStatus::Enabled,
            error: None,
            zitadel_org_id: Some(ORG.into()),
            database_id: auth_db,
            pg_database: "shop".into(),
            schema_name: "public".into(),
            table_name: "users".into(),
            email_verification: false,
            on_delete: OnDelete::Delete,
            created_at: now,
            updated_at: now,
        });
        app.syncs.insert(
            main,
            BranchAuthSync {
                branch_id: main,
                pg_database: "shop".into(),
                password_sealed: chelabase_secrets::seal_role_password(
                    &key,
                    &main.to_string(),
                    "chelabase_auth_sync",
                    SYNC_PW,
                ),
            },
        );
    }
    {
        let mut pg = locator.pg.lock().unwrap();
        pg.insert("ep-main".into(), MAIN_PG.parse().unwrap());
        pg.insert("ep-child".into(), CHILD_PG.parse().unwrap());
        pg.insert("ep-other".into(), "127.0.0.1:55434".parse().unwrap());
    }
    AppWorld {
        neon,
        locator,
        key,
        project,
        bare_project,
        auth_db,
        main,
        child,
        backup,
        other,
        bare,
        gone,
    }
}

struct AppApp {
    app: Internal,
    neon: FakeNeon,
    jobs: FakeJobs,
    /// RT1 T6: the realtime events the routes published.
    rt_events: FakeRtEvents,
}

async fn spawn_app(world: &AppWorld, signing_key: Option<&str>, zitadel: Option<&MockZitadel>) -> AppApp {
    let jobs = FakeJobs::default();
    let rt_events = FakeRtEvents::default();
    let app = spawn_with_app(
        world.neon.clone(),
        world.locator.clone(),
        "http://127.0.0.1:9",
        rules(Mode::SelfHosted, false),
        FakeCache::default(),
        "http://127.0.0.1:9",
        AppSetup {
            key: world.key.clone(),
            signing_key: signing_key.map(Into::into),
            zitadel: zitadel.map(|z| z.base.clone()),
            jobs: jobs.clone(),
            resync: FakeResync::default(),
            mode: Mode::SelfHosted,
            rt_events: rt_events.clone(),
            control: FakeLocator::default(),
            realtime_logins: Default::default(),
            attach_events: FakeAttachEvents::default(),
            setup_code: None,
        },
    )
    .await;
    AppApp {
        app,
        neon: world.neon.clone(),
        jobs,
        rt_events,
    }
}

impl Internal {
    async fn post_json(&self, path: &str, token: Option<&str>, body: Value) -> (u16, Value) {
        let mut req = self.http.post(format!("{}{path}", self.base)).json(&body);
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap_or(Value::Null))
    }

    /// The webhook: `signature` is the `zitadel-signature` header, if any.
    async fn event(&self, body: &str, signature: Option<String>) -> u16 {
        let mut req = self
            .http
            .post(format!("{}/internal/zitadel/events", self.base))
            .header("content-type", "application/json")
            .body(body.to_string());
        if let Some(sig) = signature {
            req = req.header("zitadel-signature", sig);
        }
        req.send().await.unwrap().status().as_u16()
    }
}

fn error_code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}

/// `t=…,v1=…` for `body`, signed with `key` at `t`.
fn sign(body: &str, key: &str, t: i64) -> String {
    use hmac::{Hmac, KeyInit, Mac};
    let mut mac = <Hmac<sha2::Sha256> as KeyInit>::new_from_slice(key.as_bytes()).unwrap();
    mac.update(format!("{t}.{body}").as_bytes());
    format!("t={t},v1={}", hex::encode(mac.finalize().into_bytes()))
}

fn signed(body: &str) -> Option<String> {
    Some(sign(body, SIGNING_KEY, chrono::Utc::now().timestamp()))
}

fn user_event(event_type: &str, org: &str, user: &str, created_at: chrono::DateTime<chrono::Utc>) -> String {
    json!({
        "aggregateID": user,
        "aggregateType": "user",
        "resourceOwner": org,
        "instanceID": "i1",
        "event_type": event_type,
        "created_at": created_at.to_rfc3339(),
        "userID": user,
    })
    .to_string()
}

impl AppApp {
    async fn create_session(&self, project: ProjectId, user: uuid::Uuid, zsid: &str) -> (uuid::Uuid, String) {
        let (status, body) = self
            .app
            .post_json(
                "/internal/app/sessions",
                Some(APP_AUTH_TOKEN),
                json!({ "project_id": project, "user_id": user, "zitadel_session_id": zsid }),
            )
            .await;
        assert_eq!(status, 201, "{body}");
        (
            body["session_id"].as_str().unwrap().parse().unwrap(),
            body["refresh_token"].as_str().unwrap().to_string(),
        )
    }

    async fn refresh(&self, project: ProjectId, token: &str) -> (u16, Value) {
        self.app
            .post_json(
                "/internal/app/sessions/refresh",
                Some(APP_AUTH_TOKEN),
                json!({ "project_id": project, "refresh_token": token }),
            )
            .await
    }

    fn session(&self, id: uuid::Uuid) -> FakeSession {
        self.neon
            .app
            .lock()
            .unwrap()
            .sessions
            .iter()
            .find(|f| f.s.id == id)
            .cloned()
            .unwrap()
    }

    fn edit_session(&self, id: uuid::Uuid, edit: impl FnOnce(&mut FakeSession)) {
        let mut app = self.neon.app.lock().unwrap();
        edit(app.sessions.iter_mut().find(|f| f.s.id == id).unwrap());
    }

    fn revoked(&self, id: uuid::Uuid) -> bool {
        self.session(id).s.revoked_at.is_some()
    }

    fn published(&self) -> Vec<AppUsersSyncJob> {
        self.jobs.published.lock().unwrap().clone()
    }
}

fn sha256(s: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes()).into()
}

/// A Zitadel mock holding session `id` of some user.
async fn zitadel_with_sessions(ids: &[&str]) -> MockZitadel {
    let z = MockZitadel::start().await;
    {
        let mut inner = z.inner.lock().unwrap();
        for id in ids {
            inner
                .sessions
                .insert((*id).into(), ("u1".into(), format!("tok-{id}")));
        }
    }
    z
}

/// Waits up to 2 s for `check` (background work such as the webhook's Zitadel deletes).
async fn eventually(check: impl Fn() -> bool) -> bool {
    for _ in 0..100 {
        if check() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    false
}

/// Every route of the `auth` group, with a body that would be accepted.
fn auth_routes(w: &AppWorld) -> Vec<(&'static str, String, Value)> {
    let sid = uuid::Uuid::new_v4();
    let user = uuid::Uuid::new_v4();
    vec![
        ("GET", format!("/internal/app/branches/{}", w.main), Value::Null),
        (
            "POST",
            "/internal/app/sessions".into(),
            json!({ "project_id": w.project, "user_id": user, "zitadel_session_id": "zs" }),
        ),
        (
            "POST",
            "/internal/app/sessions/refresh".into(),
            json!({ "project_id": w.project, "refresh_token": format!("{sid}.x") }),
        ),
        (
            "POST",
            "/internal/app/sessions/revoke-by-token".into(),
            json!({ "project_id": w.project, "refresh_token": format!("{sid}.x") }),
        ),
        (
            "POST",
            format!("/internal/app/sessions/{sid}/revoke"),
            json!({ "project_id": w.project }),
        ),
        (
            "POST",
            format!("/internal/app/users/{user}/revoke-sessions"),
            json!({ "project_id": w.project }),
        ),
        (
            "POST",
            "/internal/app/sync".into(),
            json!({ "project_id": w.project, "user_id": user }),
        ),
    ]
}

/// The status, with the middleware's bodiless 401 told apart: a handler's 401 (a bad refresh
/// token) carries an error code and comes back as 4010.
async fn call(app: &Internal, method: &str, path: &str, token: Option<&str>, body: &Value) -> u16 {
    let (status, body) = if method == "GET" {
        app.get(path, token).await
    } else {
        app.post_json(path, token, body.clone()).await
    };
    if status == 401 && body != Value::Null {
        return 4010;
    }
    status
}

#[tokio::test]
async fn app_routes_need_the_app_bearer() {
    let w = app_world();
    let a = spawn_app(&w, Some(SIGNING_KEY), None).await;
    let sid = uuid::Uuid::new_v4();
    let mut routes = auth_routes(&w);
    routes.push(("GET", format!("/internal/app/sessions/{sid}"), Value::Null));
    for (method, path, body) in &routes {
        for token in [None, Some("wrong"), Some(PROXY_TOKEN), Some(REST_TOKEN)] {
            assert_eq!(
                call(&a.app, method, path, token, body).await,
                401,
                "{method} {path} with {token:?}"
            );
        }
        let status = call(&a.app, method, path, Some(APP_AUTH_TOKEN), body).await;
        assert_ne!(status, 401, "{method} {path} with the app bearer");
        assert!(status < 500 || status == 4010, "{method} {path}: {status}");
    }
}

#[tokio::test]
async fn branch_info_for_auth_branch_targets_itself() {
    let w = app_world();
    let a = spawn_app(&w, Some(SIGNING_KEY), None).await;
    let (status, body) = a
        .app
        .get(&format!("/internal/app/branches/{}", w.main), Some(APP_AUTH_TOKEN))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!({
            "project_id": w.project,
            "status": "enabled",
            "zitadel_org_id": ORG,
            "email_verification": false,
            "on_delete": "delete",
            "auth_database_id": w.auth_db,
            "pg_database": "shop",
            "schema": "public",
            "table": "users",
            "target_branch_id": w.main,
            "sync": {
                "branch_id": w.main,
                "host": std::net::Ipv4Addr::LOCALHOST.to_string(),
                "port": 55432,
                "database": "shop",
                "server_name": "ep-main.compute.chelabase.internal",
                "user": "chelabase_auth_sync",
                "password": SYNC_PW,
                "schema": "public",
                "table": "users",
            },
        })
    );
    // A normal child branch of the auth database is its own target too.
    let (status, body) = a
        .app
        .get(&format!("/internal/app/branches/{}", w.child), Some(APP_AUTH_TOKEN))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["target_branch_id"], json!(w.child));
}

#[tokio::test]
async fn branch_info_for_other_database_targets_auth_main() {
    let w = app_world();
    let a = spawn_app(&w, Some(SIGNING_KEY), None).await;
    // Another database of the project, and a backup branch of the auth database (R5).
    for branch in [w.other, w.backup] {
        let (status, body) = a
            .app
            .get(&format!("/internal/app/branches/{branch}"), Some(APP_AUTH_TOKEN))
            .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["project_id"], json!(w.project));
        assert_eq!(body["target_branch_id"], json!(w.main), "{branch}");
        assert_eq!(body["sync"]["branch_id"], json!(w.main));
        assert_eq!(body["sync"]["port"], json!(55432));
        assert_eq!(body["sync"]["password"], json!(SYNC_PW));
        assert_eq!(
            body["sync"]["server_name"],
            json!("ep-main.compute.chelabase.internal"),
            "{branch}: the name the target's own compute certificate carries"
        );
    }
}

// U15 (f5b). Invariant: every connection Chelabase opens to a compute uses TLS verified against
// the platform CA. The auth service and rest dial the compute's default-network IP, but its
// certificate carries only the DNS name `<endpoint>.compute.chelabase.internal`, so the
// orchestrator sends that name (`chelabase_neon::compute_tls::server_name`) with every target:
// `sync.server_name` for the app-users sync and `server_name` for the data API connection.

// A target is never sent without the name it is verified against: no address means a null
// sync (as before), not a sync with a host and no name.
#[tokio::test]
async fn app_sync_is_null_rather_than_nameless_when_the_compute_has_no_address() {
    let w = app_world();
    let a = spawn_app(&w, Some(SIGNING_KEY), None).await;
    w.locator.pg.lock().unwrap().remove("ep-main");
    let (status, body) = a
        .app
        .get(&format!("/internal/app/branches/{}", w.main), Some(APP_AUTH_TOKEN))
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(body["sync"].is_null(), "{body}");
}

#[tokio::test]
async fn connection_carries_the_name_of_the_endpoint_it_points_at() {
    let neon = FakeNeon::default();
    let locator = FakeLocator::default();
    let app = spawn_internal(neon.clone(), locator.clone(), "http://127.0.0.1:9").await;
    locator.pg.lock().unwrap().insert("ep-xyz".into(), "127.0.0.1:55432".parse().unwrap());
    let mut row = connection_row(BranchStatus::Ready, Some("ep-xyz"), None);
    row.authenticator_password_enc =
        Some(seal_credential(&app.key, &row.branch_id.to_string(), Purpose::Authenticator, "auth-pw"));
    neon.connections.lock().unwrap().insert(row.branch_id, row.clone());
    let (code, body) = app
        .get(&format!("/internal/data/branches/{}/connection", row.branch_id.as_uuid()), Some(REST_TOKEN))
        .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(
        body["server_name"],
        json!(chelabase_neon::compute_tls::server_name("ep-xyz")),
        "{body}"
    );
    // The host stays the address the pair dials: the name is for the certificate check only.
    assert_eq!(body["host"], json!(std::net::Ipv4Addr::LOCALHOST.to_string()));
}

#[tokio::test]
async fn branch_info_without_app_users_404() {
    let w = app_world();
    let a = spawn_app(&w, Some(SIGNING_KEY), None).await;
    for branch in [w.bare, w.gone, BranchId::new()] {
        let (status, body) = a
            .app
            .get(&format!("/internal/app/branches/{branch}"), Some(APP_AUTH_TOKEN))
            .await;
        assert_eq!(status, 404, "{branch}: {body}");
        assert_eq!(error_code(&body), "app_users_disabled", "{body}");
    }
    // A project being deleted has no app users either.
    w.neon.app.lock().unwrap().projects[0].status = ProjectStatus::Deleting;
    let (status, body) = a
        .app
        .get(&format!("/internal/app/branches/{}", w.main), Some(APP_AUTH_TOKEN))
        .await;
    assert_eq!(status, 404, "{body}");
    assert_eq!(error_code(&body), "app_users_disabled");
    let _ = w.bare_project;
}

#[tokio::test]
async fn branch_info_sync_null_without_compute() {
    let w = app_world();
    let a = spawn_app(&w, Some(SIGNING_KEY), None).await;
    // `child` runs but has no sync row yet.
    let (status, body) = a
        .app
        .get(&format!("/internal/app/branches/{}", w.child), Some(APP_AUTH_TOKEN))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["target_branch_id"], json!(w.child));
    assert_eq!(body["sync"], Value::Null);
    // A sync row for another Postgres database (the location moved) doesn't count.
    w.neon.app.lock().unwrap().syncs.get_mut(&w.main).unwrap().pg_database = "old".into();
    let (_, body) = a
        .app
        .get(&format!("/internal/app/branches/{}", w.main), Some(APP_AUTH_TOKEN))
        .await;
    assert_eq!(body["sync"], Value::Null, "{body}");
    w.neon.app.lock().unwrap().syncs.get_mut(&w.main).unwrap().pg_database = "shop".into();
    // `main`'s compute stops.
    w.locator.pg.lock().unwrap().remove("ep-main");
    let (status, body) = a
        .app
        .get(&format!("/internal/app/branches/{}", w.other), Some(APP_AUTH_TOKEN))
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["target_branch_id"], json!(w.main));
    assert_eq!(body["sync"], Value::Null);
}

#[tokio::test]
async fn create_session_returns_a_token_of_the_right_shape() {
    let w = app_world();
    let a = spawn_app(&w, Some(SIGNING_KEY), None).await;
    let user = uuid::Uuid::new_v4();
    let before = chrono::Utc::now();
    let (status, body) = a
        .app
        .post_json(
            "/internal/app/sessions",
            Some(APP_AUTH_TOKEN),
            json!({ "project_id": w.project, "user_id": user, "zitadel_session_id": "zs1" }),
        )
        .await;
    assert_eq!(status, 201, "{body}");
    let sid: uuid::Uuid = body["session_id"].as_str().unwrap().parse().unwrap();
    let token = body["refresh_token"].as_str().unwrap();
    let (id_part, secret) = token.split_once('.').unwrap();
    assert_eq!(id_part, sid.to_string());
    assert_eq!(secret.len(), 43, "{secret}");
    assert!(
        secret
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "{secret}"
    );
    let expires: chrono::DateTime<chrono::Utc> = body["expires_at"].as_str().unwrap().parse().unwrap();
    let ttl = chrono::Duration::from_std(REFRESH_TTL).unwrap();
    assert!(expires >= before + ttl && expires <= chrono::Utc::now() + ttl, "{expires}");
    let stored = a.session(sid);
    assert_eq!(stored.hash, sha256(token), "the hash covers the whole token");
    assert_eq!(stored.s.user_id, user);
    assert_eq!(stored.s.project_id, w.project);
    assert_eq!(stored.s.zitadel_session_id, "zs1");
    // Two sessions never share a secret.
    let (_, other) = a.create_session(w.project, user, "zs2").await;
    assert_ne!(other.split_once('.').unwrap().1, secret);
}

#[tokio::test]
async fn refresh_rotates() {
    let w = app_world();
    let a = spawn_app(&w, Some(SIGNING_KEY), None).await;
    let user = uuid::Uuid::new_v4();
    let (sid, t0) = a.create_session(w.project, user, "zs1").await;
    let (status, body) = a.refresh(w.project, &t0).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["session_id"], json!(sid));
    assert_eq!(body["user_id"], json!(user));
    let t1 = body["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(t1, t0);
    assert!(t1.starts_with(&format!("{sid}.")), "{t1}");
    assert!(body["expires_at"].as_str().is_some());
    assert_eq!(a.session(sid).hash, sha256(&t1));
    // The new token rotates again.
    let (status, body) = a.refresh(w.project, &t1).await;
    assert_eq!(status, 200, "{body}");
    assert_ne!(body["refresh_token"], json!(t1));
}

// Review Focus 1.
#[tokio::test]
async fn refresh_grace_returns_the_same_token() {
    let w = app_world();
    let a = spawn_app(&w, Some(SIGNING_KEY), None).await;
    let (sid, t0) = a.create_session(w.project, uuid::Uuid::new_v4(), "zs1").await;
    let (_, body) = a.refresh(w.project, &t0).await;
    let t1 = body["refresh_token"].as_str().unwrap().to_string();
    // A retry with the old token inside the grace gets the same new token, unchanged.
    let (status, body) = a.refresh(w.project, &t0).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["refresh_token"], json!(t1));
    assert_eq!(body["session_id"], json!(sid));
    assert!(!a.revoked(sid));
    assert_eq!(a.session(sid).hash, sha256(&t1));
    // The grace is 10 s.
    let grace = a.session(sid).grace_until.unwrap() - chrono::Utc::now();
    assert!(grace <= chrono::Duration::seconds(10) && grace > chrono::Duration::seconds(5), "{grace}");
}

// Review Focus 1.
#[tokio::test]
async fn refresh_reuse_revokes_and_deletes_the_zitadel_session() {
    let w = app_world();
    let z = zitadel_with_sessions(&["zs1"]).await;
    let a = spawn_app(&w, Some(SIGNING_KEY), Some(&z)).await;
    let (sid, t0) = a.create_session(w.project, uuid::Uuid::new_v4(), "zs1").await;
    let (_, body) = a.refresh(w.project, &t0).await;
    let t1 = body["refresh_token"].as_str().unwrap().to_string();
    a.edit_session(sid, |f| f.grace_until = Some(chrono::Utc::now() - chrono::Duration::seconds(1)));
    let (status, body) = a.refresh(w.project, &t0).await;
    assert_eq!(status, 401, "{body}");
    assert_eq!(error_code(&body), "refresh_token_reused");
    assert!(a.revoked(sid));
    assert!(eventually(|| z.session_count() == 0).await, "{:?}", z.calls());
    assert!(z.calls().contains(&"DELETE /v2/sessions/zs1".to_string()), "{:?}", z.calls());
    // The current token is dead too.
    let (status, body) = a.refresh(w.project, &t1).await;
    assert_eq!(status, 401);
    assert_eq!(error_code(&body), "refresh_token_invalid");
}

#[tokio::test]
async fn refresh_other_project_is_invalid() {
    let w = app_world();
    let a = spawn_app(&w, Some(SIGNING_KEY), None).await;
    let (sid, t0) = a.create_session(w.project, uuid::Uuid::new_v4(), "zs1").await;
    let (status, body) = a.refresh(w.bare_project, &t0).await;
    assert_eq!(status, 401, "{body}");
    assert_eq!(error_code(&body), "refresh_token_invalid");
    assert!(!a.revoked(sid));
    assert_eq!(a.session(sid).hash, sha256(&t0), "unchanged");
    let (status, _) = a.refresh(w.project, &t0).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn malformed_refresh_token_invalid() {
    let w = app_world();
    let a = spawn_app(&w, Some(SIGNING_KEY), None).await;
    let (sid, t0) = a.create_session(w.project, uuid::Uuid::new_v4(), "zs1").await;
    let unknown = format!("{}.{}", uuid::Uuid::new_v4(), t0.split_once('.').unwrap().1);
    for bad in [
        "".to_string(),
        "garbage".to_string(),
        sid.to_string(),
        format!("not-a-uuid.{}", t0.split_once('.').unwrap().1),
        format!("{sid}.wrong-secret"),
        unknown,
    ] {
        let (status, body) = a.refresh(w.project, &bad).await;
        assert_eq!(status, 401, "{bad:?}: {body}");
        assert_eq!(error_code(&body), "refresh_token_invalid", "{bad:?}");
    }
    assert!(!a.revoked(sid), "garbage never revokes");
}

#[tokio::test]
async fn session_liveness() {
    let w = app_world();
    let a = spawn_app(&w, Some(SIGNING_KEY), None).await;
    let user = uuid::Uuid::new_v4();
    let (sid, _) = a.create_session(w.project, user, "zs1").await;
    let path = format!("/internal/app/sessions/{sid}");
    let (status, body) = a.app.get(&path, Some(APP_SESSIONS_TOKEN)).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, json!({ "live": true, "project_id": w.project, "user_id": user }));
    // Expired.
    a.edit_session(sid, |f| f.s.expires_at = chrono::Utc::now() - chrono::Duration::seconds(1));
    let (_, body) = a.app.get(&path, Some(APP_SESSIONS_TOKEN)).await;
    assert_eq!(body["live"], json!(false), "{body}");
    // Revoked.
    a.edit_session(sid, |f| {
        f.s.expires_at = chrono::Utc::now() + chrono::Duration::hours(1);
        f.s.revoked_at = Some(chrono::Utc::now());
    });
    let (_, body) = a.app.get(&path, Some(APP_SESSIONS_TOKEN)).await;
    assert_eq!(body["live"], json!(false), "{body}");
    // Unknown.
    let (status, _) = a
        .app
        .get(&format!("/internal/app/sessions/{}", uuid::Uuid::new_v4()), Some(APP_SESSIONS_TOKEN))
        .await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn revoke_user_sessions_keeps_one() {
    let w = app_world();
    let z = zitadel_with_sessions(&["z1", "z2", "z3", "z4"]).await;
    let a = spawn_app(&w, Some(SIGNING_KEY), Some(&z)).await;
    let (u, v) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    let (s1, _) = a.create_session(w.project, u, "z1").await;
    let (s2, _) = a.create_session(w.project, u, "z2").await;
    let (s3, _) = a.create_session(w.project, u, "z3").await;
    let (s4, _) = a.create_session(w.project, v, "z4").await;
    let (status, body) = a
        .app
        .post_json(
            &format!("/internal/app/users/{u}/revoke-sessions"),
            Some(APP_AUTH_TOKEN),
            json!({ "project_id": w.project, "keep_session_id": s2 }),
        )
        .await;
    assert_eq!(status, 204, "{body}");
    assert!(a.revoked(s1) && a.revoked(s3));
    assert!(!a.revoked(s2) && !a.revoked(s4));
    assert!(eventually(|| z.calls().iter().filter(|c| c.starts_with("DELETE")).count() == 2).await, "{:?}", z.calls());
    let calls = z.calls();
    assert!(calls.contains(&"DELETE /v2/sessions/z1".to_string()), "{calls:?}");
    assert!(calls.contains(&"DELETE /v2/sessions/z3".to_string()), "{calls:?}");
    assert!(!calls.iter().any(|c| c.ends_with("/z2") || c.ends_with("/z4")), "{calls:?}");
    // Another project's revoke leaves the user's sessions alone (I5).
    let (status, _) = a
        .app
        .post_json(
            &format!("/internal/app/users/{u}/revoke-sessions"),
            Some(APP_AUTH_TOKEN),
            json!({ "project_id": w.bare_project }),
        )
        .await;
    assert_eq!(status, 204);
    assert!(!a.revoked(s2));
    // `created_before` limits it to older sessions.
    let cut = a.session(s2).s.created_at;
    let (s5, _) = a.create_session(w.project, u, "z5").await;
    a.edit_session(s5, |f| f.s.created_at = cut + chrono::Duration::seconds(5));
    let (status, _) = a
        .app
        .post_json(
            &format!("/internal/app/users/{u}/revoke-sessions"),
            Some(APP_AUTH_TOKEN),
            json!({ "project_id": w.project, "created_before": cut + chrono::Duration::seconds(1) }),
        )
        .await;
    assert_eq!(status, 204);
    assert!(a.revoked(s2) && !a.revoked(s5));
}

/// T7: every revoke path answers without waiting for Zitadel to end the sessions (a slow
/// or hung Zitadel would otherwise hold sign-out everywhere and change-password for up to
/// 10 s per session).
#[tokio::test]
async fn revokes_answer_before_zitadel_ends_the_sessions() {
    let w = app_world();
    let z = zitadel_with_sessions(&["z1", "z2", "z3", "z4", "z5"]).await;
    z.set_session_delete_delay(std::time::Duration::from_millis(600));
    let a = spawn_app(&w, Some(SIGNING_KEY), Some(&z)).await;
    let u = uuid::Uuid::new_v4();
    let (s1, _) = a.create_session(w.project, u, "z1").await;
    let (s2, _) = a.create_session(w.project, u, "z2").await;
    let (s3, t3) = a.create_session(w.project, uuid::Uuid::new_v4(), "z3").await;
    let (s4, t4) = a.create_session(w.project, uuid::Uuid::new_v4(), "z4").await;
    let fast = std::time::Duration::from_millis(400);

    let start = std::time::Instant::now();
    let (status, body) = a
        .app
        .post_json(
            &format!("/internal/app/users/{u}/revoke-sessions"),
            Some(APP_AUTH_TOKEN),
            json!({ "project_id": w.project }),
        )
        .await;
    assert_eq!(status, 204, "{body}");
    assert!(start.elapsed() < fast, "revoke-sessions waited {:?}", start.elapsed());
    assert!(a.revoked(s1) && a.revoked(s2));

    let start = std::time::Instant::now();
    let (status, _) = a
        .app
        .post_json(&format!("/internal/app/sessions/{s3}/revoke"), Some(APP_AUTH_TOKEN), json!({ "project_id": w.project }))
        .await;
    assert_eq!(status, 204);
    assert!(start.elapsed() < fast, "revoke waited {:?}", start.elapsed());
    let _ = t3;

    let start = std::time::Instant::now();
    let (status, _) = a
        .app
        .post_json(
            "/internal/app/sessions/revoke-by-token",
            Some(APP_AUTH_TOKEN),
            json!({ "project_id": w.project, "refresh_token": t4 }),
        )
        .await;
    assert_eq!(status, 204);
    assert!(start.elapsed() < fast, "revoke-by-token waited {:?}", start.elapsed());
    assert!(a.revoked(s4));

    let (s5, t0) = a.create_session(w.project, uuid::Uuid::new_v4(), "z5").await;
    let (_, _) = a.refresh(w.project, &t0).await;
    a.edit_session(s5, |f| f.grace_until = Some(chrono::Utc::now() - chrono::Duration::seconds(1)));
    let start = std::time::Instant::now();
    let (status, body) = a.refresh(w.project, &t0).await;
    assert_eq!((status, error_code(&body)), (401, "refresh_token_reused"));
    assert!(start.elapsed() < fast, "refresh reuse waited {:?}", start.elapsed());

    // They still end, in the background.
    let mut done = false;
    for _ in 0..200 {
        if z.session_count() == 0 {
            done = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(done, "{:?}", z.calls());
}

#[tokio::test]
async fn revoke_by_token_ignores_a_wrong_token_or_project() {
    let w = app_world();
    let z = zitadel_with_sessions(&["zs1"]).await;
    let a = spawn_app(&w, Some(SIGNING_KEY), Some(&z)).await;
    let (sid, t0) = a.create_session(w.project, uuid::Uuid::new_v4(), "zs1").await;
    let revoke = |project: ProjectId, token: String| {
        let a = &a;
        async move {
            a.app
                .post_json(
                    "/internal/app/sessions/revoke-by-token",
                    Some(APP_AUTH_TOKEN),
                    json!({ "project_id": project, "refresh_token": token }),
                )
                .await
                .0
        }
    };
    assert_eq!(revoke(w.project, format!("{sid}.wrong")).await, 204);
    assert_eq!(revoke(w.project, "garbage".into()).await, 204);
    assert_eq!(revoke(w.bare_project, t0.clone()).await, 204);
    assert!(!a.revoked(sid));
    assert_eq!(z.session_count(), 1);
    assert_eq!(revoke(w.project, t0.clone()).await, 204);
    assert!(a.revoked(sid));
    assert!(eventually(|| z.session_count() == 0).await, "{:?}", z.calls());
    // Idempotent.
    assert_eq!(revoke(w.project, t0).await, 204);
}

// I5.
#[tokio::test]
async fn revoke_other_project_session_is_a_no_op() {
    let w = app_world();
    let z = zitadel_with_sessions(&["zs1"]).await;
    let a = spawn_app(&w, Some(SIGNING_KEY), Some(&z)).await;
    let (sid, _) = a.create_session(w.project, uuid::Uuid::new_v4(), "zs1").await;
    let path = format!("/internal/app/sessions/{sid}/revoke");
    let (status, _) = a
        .app
        .post_json(&path, Some(APP_AUTH_TOKEN), json!({ "project_id": w.bare_project }))
        .await;
    assert_eq!(status, 204);
    assert!(!a.revoked(sid));
    assert!(!z.calls().iter().any(|c| c.starts_with("DELETE")), "{:?}", z.calls());
    let (status, _) = a
        .app
        .post_json(&path, Some(APP_AUTH_TOKEN), json!({ "project_id": w.project }))
        .await;
    assert_eq!(status, 204);
    assert!(a.revoked(sid));
    assert!(
        eventually(|| z.calls().contains(&"DELETE /v2/sessions/zs1".to_string())).await,
        "{:?}",
        z.calls()
    );
    // Idempotent, and an unknown session is fine too.
    let (status, _) = a
        .app
        .post_json(&path, Some(APP_AUTH_TOKEN), json!({ "project_id": w.project }))
        .await;
    assert_eq!(status, 204);
    let (status, _) = a
        .app
        .post_json(
            &format!("/internal/app/sessions/{}/revoke", uuid::Uuid::new_v4()),
            Some(APP_AUTH_TOKEN),
            json!({ "project_id": w.project }),
        )
        .await;
    assert_eq!(status, 204);
}

// P31.
#[tokio::test]
async fn gateway_bearer_reaches_only_session_liveness() {
    let w = app_world();
    let a = spawn_app(&w, Some(SIGNING_KEY), None).await;
    let (sid, _) = a.create_session(w.project, uuid::Uuid::new_v4(), "zs1").await;
    let path = format!("/internal/app/sessions/{sid}");
    assert_eq!(a.app.get(&path, Some(APP_SESSIONS_TOKEN)).await.0, 200);
    assert_eq!(a.app.get(&path, Some(APP_AUTH_TOKEN)).await.0, 200);
    for (method, path, body) in auth_routes(&w) {
        assert_eq!(
            call(&a.app, method, &path, Some(APP_SESSIONS_TOKEN), &body).await,
            401,
            "{method} {path}"
        );
    }
    assert!(a.published().is_empty());
}

#[tokio::test]
async fn sync_publishes() {
    let w = app_world();
    let a = spawn_app(&w, Some(SIGNING_KEY), None).await;
    let user = uuid::Uuid::new_v4();
    let (status, body) = a
        .app
        .post_json(
            "/internal/app/sync",
            Some(APP_AUTH_TOKEN),
            json!({
                "project_id": w.project,
                "user_id": user,
                "data": { "nickname": "ada" },
                "origin_branch_id": w.child,
            }),
        )
        .await;
    assert_eq!(status, 202, "{body}");
    let (status, _) = a
        .app
        .post_json(
            "/internal/app/sync",
            Some(APP_AUTH_TOKEN),
            json!({ "project_id": w.project, "user_id": user }),
        )
        .await;
    assert_eq!(status, 202);
    assert_eq!(
        a.published(),
        vec![
            AppUsersSyncJob {
                project_id: w.project,
                user_id: user,
                data: Some(json!({ "nickname": "ada" })),
                origin_branch_id: Some(w.child),
            },
            AppUsersSyncJob {
                project_id: w.project,
                user_id: user,
                data: None,
                origin_branch_id: None,
            },
        ]
    );
}

// Review Focus 5.
#[tokio::test]
async fn webhook_rejects_bad_signature_and_ignores_other_orgs() {
    let w = app_world();
    let a = spawn_app(&w, Some(SIGNING_KEY), None).await;
    let user = uuid::Uuid::new_v4().to_string();
    let body = user_event("user.human.added", ORG, &user, chrono::Utc::now());
    let now = chrono::Utc::now().timestamp();
    // No header, a bad header, the wrong key, an old timestamp, another body's signature.
    assert_eq!(a.app.event(&body, None).await, 401);
    assert_eq!(a.app.event(&body, Some("nonsense".into())).await, 401);
    assert_eq!(a.app.event(&body, Some(sign(&body, "other-key", now))).await, 401);
    assert_eq!(a.app.event(&body, Some(sign(&body, SIGNING_KEY, now - 600))).await, 401);
    assert_eq!(a.app.event(&body, Some(sign("{}", SIGNING_KEY, now))).await, 401);
    // Signed but ignored: another org, a non-user event, a non-uuid id, a body that isn't an event.
    let other_org = user_event("user.human.added", "o2", &user, chrono::Utc::now());
    let not_user = user_event("user.human.phone.changed", ORG, &user, chrono::Utc::now());
    let not_uuid = user_event("user.human.added", ORG, "3001", chrono::Utc::now());
    for ignored in [other_org, not_user, not_uuid, "not json".to_string(), "[]".to_string()] {
        assert_eq!(a.app.event(&ignored, signed(&ignored)).await, 200, "{ignored}");
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(a.published().is_empty(), "{:?}", a.published());
}

#[tokio::test]
async fn webhook_publishes_sync_for_user_events() {
    let w = app_world();
    let a = spawn_app(&w, Some(SIGNING_KEY), None).await;
    let user = uuid::Uuid::new_v4();
    let kinds = ["user.human.added", "user.human.email.verified", "user.removed", "user.locked"];
    for kind in kinds {
        let body = user_event(kind, ORG, &user.to_string(), chrono::Utc::now());
        assert_eq!(a.app.event(&body, signed(&body)).await, 200, "{kind}");
    }
    assert!(eventually(|| a.published().len() == kinds.len()).await, "{:?}", a.published());
    for job in a.published() {
        assert_eq!(
            job,
            AppUsersSyncJob {
                project_id: w.project,
                user_id: user,
                data: None,
                origin_branch_id: None,
            }
        );
    }
}

#[tokio::test]
async fn webhook_password_changed_revokes_older_sessions_only() {
    let w = app_world();
    let z = zitadel_with_sessions(&["z-old", "z-new"]).await;
    let a = spawn_app(&w, Some(SIGNING_KEY), Some(&z)).await;
    let user = uuid::Uuid::new_v4();
    let (old, _) = a.create_session(w.project, user, "z-old").await;
    let (new, _) = a.create_session(w.project, user, "z-new").await;
    let changed = chrono::Utc::now();
    a.edit_session(old, |f| f.s.created_at = changed - chrono::Duration::minutes(5));
    a.edit_session(new, |f| f.s.created_at = changed + chrono::Duration::seconds(1));
    let body = user_event("user.human.password.changed", ORG, &user.to_string(), changed);
    assert_eq!(a.app.event(&body, signed(&body)).await, 200);
    // Revoked before the answer.
    assert!(a.revoked(old));
    assert!(!a.revoked(new), "the session that changed the password stays");
    assert!(
        eventually(|| z.calls().contains(&"DELETE /v2/sessions/z-old".to_string())).await,
        "{:?}",
        z.calls()
    );
    assert!(!z.calls().iter().any(|c| c.ends_with("/z-new")));
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(a.published().is_empty(), "no sync for a password change");
}

#[tokio::test]
async fn webhook_503_without_signing_key() {
    let w = app_world();
    let a = spawn_app(&w, None, None).await;
    let body = user_event("user.human.added", ORG, &uuid::Uuid::new_v4().to_string(), chrono::Utc::now());
    assert_eq!(a.app.event(&body, signed(&body)).await, 503);
    assert_eq!(a.app.event(&body, None).await, 503);
}

#[tokio::test]
async fn signup_check_reasons() {
    let neon = FakeNeon::default();
    {
        let mut m = neon.signup.lock().unwrap();
        m.insert("org-open".into(), (true, true));
        m.insert("org-first".into(), (false, false));
        m.insert("org-closed".into(), (false, true));
    }
    let spawn = |neon: FakeNeon, mode| setup_app(neon, mode, Some(SETUP_CODE), None);
    let app = spawn(neon.clone(), Mode::SelfHosted).await;
    let ask = |org: &'static str| {
        let app = &app;
        async move { app.post_json(CHECK, Some(APP_AUTH_TOKEN), json!({"org": org})).await }
    };
    let no = |reason: &str, required: bool| json!({"allowed": false, "reason": reason, "setup_required": required});
    assert_eq!(ask("org-open").await, (200, json!({"allowed": true, "reason": "open", "setup_required": false})));
    assert_eq!(ask("org-first").await, (200, no("setup_code_invalid", true)));
    assert_eq!(ask("org-closed").await, (200, no("closed", false)));
    assert_eq!(ask("org-unknown").await, (200, no("closed", false)));
    assert_eq!(app.post_json(CHECK, Some("wrong"), json!({"org": "org-open"})).await.0, 401);
    assert_eq!(app.post_json(CHECK, None, json!({"org": "org-open"})).await.0, 401);
    // The old GET gate is gone (auth is its only caller).
    assert_eq!(app.get("/internal/signup/allowed?org=org-open", Some(APP_AUTH_TOKEN)).await.0, 404);
    // Platform mode: sign-up and claims are refused whatever the code.
    let platform = spawn(neon.clone(), Mode::Platform).await;
    assert_eq!(
        platform.post_json(CHECK, Some(APP_AUTH_TOKEN), json!({"org": "org-first", "setup_code": SETUP_CODE})).await,
        (200, no("platform", false))
    );
    assert!(neon.live_nonces("org-first").is_empty());
    let claimed = platform
        .post_json(CLAIMED, Some(APP_AUTH_TOKEN), json!({"org": "org-first", "user_id": "1", "nonce": "ab".repeat(32)}))
        .await;
    assert_eq!((claimed.0, error_code(&claimed.1)), (403, "setup_code_invalid"));
}

const CHECK: &str = "/internal/signup/check";
const CLAIMED: &str = "/internal/signup/claimed";
const SETUP_CODE: &str = "ABCD-EFGH-JKMN-PQRS-TUVW";
const SETUP_ORG: &str = "org-first";

/// The internal listener with this setup code (`None`: no `CHELABASE_SETUP_CODE_FILE`) and,
/// optionally, a Zitadel.
async fn setup_app(neon: FakeNeon, mode: Mode, code: Option<&str>, zitadel: Option<&MockZitadel>) -> Internal {
    spawn_with_app(
        neon,
        FakeLocator::default(),
        "http://127.0.0.1:9",
        rules(Mode::SelfHosted, false),
        FakeCache::default(),
        "http://127.0.0.1:9",
        AppSetup {
            mode,
            zitadel: zitadel.map(|z| z.base.clone()),
            setup_code: code.map(|c| orchestrator::internal::SetupCode::parse(c).unwrap()),
            ..AppSetup::default()
        },
    )
    .await
}

/// A fake with one member-less tenant (org `SETUP_ORG`) and one with members (`org-members`).
fn fresh_install() -> FakeNeon {
    let neon = FakeNeon::default();
    {
        let mut m = neon.signup.lock().unwrap();
        m.insert(SETUP_ORG.into(), (false, false));
        m.insert("org-members".into(), (false, true));
        m.insert("org-other".into(), (false, false));
    }
    neon
}

fn nonce_sha(nonce: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(hex::decode(nonce).unwrap()).into()
}

async fn check_with(app: &Internal, org: &str, code: Option<&str>) -> (u16, Value) {
    let mut body = json!({"org": org});
    if let Some(code) = code {
        body["setup_code"] = json!(code);
    }
    app.post_json(CHECK, Some(APP_AUTH_TOKEN), body).await
}

/// A nonce from a check with the right code.
async fn right_code_nonce(app: &Internal, org: &str) -> String {
    let (status, body) = check_with(app, org, Some(SETUP_CODE)).await;
    assert_eq!((status, &body["reason"]), (200, &json!("first_member")), "{body}");
    body["claim_nonce"].as_str().unwrap().to_string()
}

async fn claim(app: &Internal, org: &str, user: &str, nonce: &str) -> (u16, Value) {
    app.post_json(CLAIMED, Some(APP_AUTH_TOKEN), json!({"org": org, "user_id": user, "nonce": nonce}))
        .await
}

// AC 1 and AC 2: a member-less tenant with a code takes a sign-up only with that code (any
// case, with or without dashes), and only then hands out a claim nonce, stored hashed.
#[tokio::test]
async fn signup_first_member_requires_code() {
    let neon = fresh_install();
    let app = setup_app(neon.clone(), Mode::SelfHosted, Some(SETUP_CODE), None).await;
    let refused = json!({"allowed": false, "reason": "setup_code_invalid", "setup_required": true});
    for code in [None, Some(""), Some("ABCD-EFGH-JKMN-PQRS-TUVX"), Some("ABCD-EFGH")] {
        assert_eq!(check_with(&app, SETUP_ORG, code).await, (200, refused.clone()), "{code:?}");
    }
    assert!(neon.live_nonces(SETUP_ORG).is_empty(), "no nonce for a wrong code");
    for code in [SETUP_CODE, "abcd-efgh-jkmn-pqrs-tuvw", "ABCDEFGHJKMNPQRSTUVW", " abcdefghjkmnpqrstuvw "] {
        let (status, body) = check_with(&app, SETUP_ORG, Some(code)).await;
        assert_eq!(status, 200);
        assert_eq!((&body["allowed"], &body["reason"], &body["setup_required"]), (&json!(true), &json!("first_member"), &json!(true)), "{code}");
        let nonce = body["claim_nonce"].as_str().unwrap();
        assert_eq!(nonce.len(), 64);
        assert!(neon.live_nonces(SETUP_ORG).contains(&nonce_sha(nonce)), "stored as its hash");
    }
    assert_eq!(neon.live_nonces(SETUP_ORG).len(), 4, "a fresh nonce each time");
}

// AC 4: once the tenant has a member the code is dead: the normal rule answers, no nonce.
#[tokio::test]
async fn signup_claimed_tenant_ignores_code() {
    let neon = fresh_install();
    let app = setup_app(neon.clone(), Mode::SelfHosted, Some(SETUP_CODE), None).await;
    let closed = json!({"allowed": false, "reason": "closed", "setup_required": false});
    for code in [None, Some(SETUP_CODE), Some("wrong")] {
        assert_eq!(check_with(&app, "org-members", code).await, (200, closed.clone()), "{code:?}");
    }
    neon.signup.lock().unwrap().insert("org-members".into(), (true, true));
    let open = json!({"allowed": true, "reason": "open", "setup_required": false});
    assert_eq!(check_with(&app, "org-members", Some(SETUP_CODE)).await, (200, open));
    assert!(neon.live_nonces("org-members").is_empty());
}

// AC 5: without a configured code there is no first-member sign-up and no claim.
#[tokio::test]
async fn signup_no_code_file_closes_window() {
    let neon = fresh_install();
    let mock = MockZitadel::start().await;
    let user = mock.add_user_in(SETUP_ORG, "a@x.com", "pw", true);
    let app = setup_app(neon.clone(), Mode::SelfHosted, None, Some(&mock)).await;
    let closed = json!({"allowed": false, "reason": "closed", "setup_required": false});
    for code in [None, Some(SETUP_CODE)] {
        assert_eq!(check_with(&app, SETUP_ORG, code).await, (200, closed.clone()), "{code:?}");
    }
    assert!(neon.live_nonces(SETUP_ORG).is_empty());
    // Even a nonce that somehow exists binds nothing.
    let nonce = "cd".repeat(32);
    neon.setup.lock().unwrap().nonces.insert(nonce_sha(&nonce), (SETUP_ORG.into(), true));
    let (status, body) = claim(&app, SETUP_ORG, &user, &nonce).await;
    assert_eq!((status, error_code(&body)), (403, "setup_code_invalid"));
    assert_eq!(neon.setup_claim_of(SETUP_ORG), None);
}

// The nonce AC: the bearer alone binds nothing. No nonce, a malformed, unknown, reused,
// expired or other org's one, or a user outside the org: 403, no claim written.
#[tokio::test]
async fn signup_claimed_without_code_or_nonce_refused() {
    let neon = fresh_install();
    let mock = MockZitadel::start().await;
    let a = mock.add_user_in(SETUP_ORG, "a@x.com", "pw", true);
    let outsider = mock.add_user_in("org-other", "o@x.com", "pw", true);
    let app = setup_app(neon.clone(), Mode::SelfHosted, Some(SETUP_CODE), Some(&mock)).await;
    let refused = |r: (u16, Value)| assert_eq!((r.0, error_code(&r.1)), (403, "setup_code_invalid"), "{r:?}");
    refused(app.post_json(CLAIMED, Some(APP_AUTH_TOKEN), json!({"org": SETUP_ORG, "user_id": a})).await);
    refused(claim(&app, SETUP_ORG, &a, "").await);
    refused(claim(&app, SETUP_ORG, &a, "not-hex").await);
    refused(claim(&app, SETUP_ORG, &a, &"ab".repeat(32)).await);
    // Another org's nonce.
    let other = right_code_nonce(&app, "org-other").await;
    refused(claim(&app, SETUP_ORG, &a, &other).await);
    // A user who isn't in the org: refused before the nonce is spent.
    let n = right_code_nonce(&app, SETUP_ORG).await;
    refused(claim(&app, SETUP_ORG, &outsider, &n).await);
    refused(claim(&app, SETUP_ORG, "999999", &n).await);
    refused(claim(&app, SETUP_ORG, "../users", &n).await);
    assert_eq!(neon.setup_claim_of(SETUP_ORG), None);
    // An expired one.
    neon.expire_nonces();
    refused(claim(&app, SETUP_ORG, &a, &n).await);
    assert_eq!(neon.setup_claim_of(SETUP_ORG), None);
    // The bearer is checked first.
    let n = right_code_nonce(&app, SETUP_ORG).await;
    assert_eq!(app.post_json(CLAIMED, Some("wrong"), json!({"org": SETUP_ORG, "user_id": a, "nonce": n})).await.0, 401);
    assert_eq!(app.post_json(CLAIMED, None, json!({"org": SETUP_ORG, "user_id": a, "nonce": n})).await.0, 401);
    // The right nonce binds once; reusing it is refused.
    assert_eq!(claim(&app, SETUP_ORG, &a, &n).await, (200, json!({"bound": true})));
    assert_eq!(neon.setup_claim_of(SETUP_ORG).as_deref(), Some(a.as_str()));
    refused(claim(&app, SETUP_ORG, &a, &n).await);
}

// AC 6 (the HTTP side): the second code-gated account gets `bound: false`, so auth keeps it
// (it waits as a newcomer) and the first claim stands.
#[tokio::test]
async fn signup_second_claim_is_not_bound() {
    let neon = fresh_install();
    let mock = MockZitadel::start().await;
    let a = mock.add_user_in(SETUP_ORG, "a@x.com", "pw", true);
    let b = mock.add_user_in(SETUP_ORG, "b@x.com", "pw", true);
    let app = setup_app(neon.clone(), Mode::SelfHosted, Some(SETUP_CODE), Some(&mock)).await;
    let (na, nb) = (right_code_nonce(&app, SETUP_ORG).await, right_code_nonce(&app, SETUP_ORG).await);
    assert_eq!(claim(&app, SETUP_ORG, &a, &na).await, (200, json!({"bound": true})));
    assert_eq!(claim(&app, SETUP_ORG, &b, &nb).await, (200, json!({"bound": false})));
    assert_eq!(neon.setup_claim_of(SETUP_ORG).as_deref(), Some(a.as_str()));
    // A exists in Zitadel: never asked to be replaced.
    assert_eq!(neon.setup.lock().unwrap().replace_asked.last().cloned(), Some(None));
}

// AC 11: a claim whose user has no member row and is gone from Zitadel is replaced by a new
// code-gated sign-up. A Zitadel failure on the way answers 503 and spends nothing.
#[tokio::test]
async fn signup_stale_claim_replaced() {
    let neon = fresh_install();
    let mock = MockZitadel::start().await;
    let gone = mock.add_user_in(SETUP_ORG, "gone@x.com", "pw", true);
    let new = mock.add_user_in(SETUP_ORG, "new@x.com", "pw", true);
    let app = setup_app(neon.clone(), Mode::SelfHosted, Some(SETUP_CODE), Some(&mock)).await;
    let n = right_code_nonce(&app, SETUP_ORG).await;
    assert_eq!(claim(&app, SETUP_ORG, &gone, &n).await, (200, json!({"bound": true})));
    mock.remove_user(&gone);
    // Zitadel down: 503, the nonce stays live.
    let n = right_code_nonce(&app, SETUP_ORG).await;
    mock.set_down(true);
    let (status, body) = claim(&app, SETUP_ORG, &new, &n).await;
    assert_eq!((status, error_code(&body)), (503, "unavailable"));
    mock.set_down(false);
    assert!(neon.live_nonces(SETUP_ORG).contains(&nonce_sha(&n)));
    assert_eq!(claim(&app, SETUP_ORG, &new, &n).await, (200, json!({"bound": true})));
    assert_eq!(neon.setup_claim_of(SETUP_ORG).as_deref(), Some(new.as_str()));
    assert_eq!(neon.setup.lock().unwrap().replace_asked.last().cloned(), Some(Some(gone.clone())));
    // A claim user with a member row is never replaced, even if Zitadel lost it.
    neon.setup.lock().unwrap().member_rows.insert(new.clone());
    mock.remove_user(&new);
    let third = mock.add_user_in(SETUP_ORG, "third@x.com", "pw", true);
    let n = right_code_nonce(&app, SETUP_ORG).await;
    assert_eq!(claim(&app, SETUP_ORG, &third, &n).await, (200, json!({"bound": false})));
    assert_eq!(neon.setup_claim_of(SETUP_ORG).as_deref(), Some(new.as_str()));
}

// Without Zitadel the user can't be checked: no claim (503), fail closed.
#[tokio::test]
async fn signup_claimed_needs_zitadel() {
    let neon = fresh_install();
    let app = setup_app(neon.clone(), Mode::SelfHosted, Some(SETUP_CODE), None).await;
    let n = right_code_nonce(&app, SETUP_ORG).await;
    let (status, body) = claim(&app, SETUP_ORG, "312345", &n).await;
    assert_eq!((status, error_code(&body)), (503, "unavailable"));
    assert_eq!(neon.setup_claim_of(SETUP_ORG), None);
}

/// Every log line written while `f` runs on this thread (the test's current-thread runtime
/// runs the server's tasks here too).
#[derive(Clone, Default)]
struct LogSink(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// The code (in any form it was typed) and the nonce never show in the orchestrator's logs,
// trace spans or error bodies.
#[tokio::test]
async fn signup_log_and_error_scrub() {
    let sink = LogSink::default();
    let writer = sink.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("setup.code");
    std::fs::write(&file, format!("{SETUP_CODE}\n")).unwrap();
    let loaded = orchestrator::internal::load_setup_code(file.to_str()).unwrap();
    assert!(loaded.matches(SETUP_CODE));
    let neon = fresh_install();
    let mock = MockZitadel::start().await;
    let a = mock.add_user_in(SETUP_ORG, "a@x.com", "pw", true);
    let app = setup_app(neon.clone(), Mode::SelfHosted, Some(SETUP_CODE), Some(&mock)).await;
    let wrong = "ZZZZ-YYYY-XXXX-WWWW-VVVV";
    let mut bodies = vec![check_with(&app, SETUP_ORG, Some(wrong)).await.1];
    let nonce = right_code_nonce(&app, SETUP_ORG).await;
    bodies.push(claim(&app, SETUP_ORG, "999999", &nonce).await.1);
    bodies.push(claim(&app, "org-other", &a, &nonce).await.1);
    bodies.push(claim(&app, SETUP_ORG, &a, &nonce).await.1);
    bodies.push(claim(&app, SETUP_ORG, &a, &nonce).await.1);
    let logs = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("setup code loaded"), "the startup line is there: {logs}");
    assert!(logs.contains("setup claim refused"), "{logs}");
    let secrets = [SETUP_CODE.to_string(), SETUP_CODE.replace('-', ""), wrong.to_string(), wrong.replace('-', ""), nonce.clone()];
    for secret in &secrets {
        assert!(!logs.contains(secret.as_str()), "{secret} in the logs");
        assert!(!logs.to_lowercase().contains(&secret.to_lowercase()), "{secret} in the logs");
        for body in &bodies {
            assert!(!body.to_string().contains(secret.as_str()), "{secret} in {body}");
        }
    }
}

// T12x: the invite description (R32). The auth service asks with the Zitadel user id and the
// SHA-256 of the link's token; only an open invite with that exact hash answers.
fn invite_fixture(neon: &FakeNeon, hash: Option<[u8; 32]>) {
    neon.invites.lock().unwrap().insert(
        "u1".into(),
        InviteDescription {
            token_hash: hash.map(|h| h.to_vec()),
            inviter_name: "Alice".into(),
            inviter_email: Some("alice@x.com".into()),
            tenant_slug: "acme".into(),
            tenant_display_name: None,
        },
    );
}

async fn describe_app(neon: FakeNeon) -> Internal {
    spawn_with_app(
        neon,
        FakeLocator::default(),
        "http://127.0.0.1:9",
        rules(Mode::SelfHosted, false),
        FakeCache::default(),
        "http://127.0.0.1:9",
        AppSetup::default(),
    )
    .await
}

fn token_hash(token: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes()).into()
}

fn token_hash_hex(token: &str) -> String {
    hex::encode(token_hash(token))
}

#[tokio::test]
async fn describe_invite_needs_the_token() {
    let neon = FakeNeon::default();
    invite_fixture(&neon, Some(token_hash("right")));
    let app = describe_app(neon).await;
    let ask = |user: &str, hash: &str| {
        let body = json!({ "user": user, "token_hash": hash });
        let app = &app;
        async move { app.post_json("/internal/invites/describe", Some(APP_AUTH_TOKEN), body).await }
    };
    assert_eq!(
        ask("u1", &token_hash_hex("right")).await,
        (200, json!({"inviter_name": "Alice", "inviter_email": "alice@x.com", "tenant_name": "acme"}))
    );
    // Every wrong part answers the same 404.
    let wrong_token = ask("u1", &token_hash_hex("wrong")).await;
    let wrong_user = ask("u2", &token_hash_hex("right")).await;
    let not_hex = ask("u1", "zz").await;
    let short = ask("u1", &token_hash_hex("right")[..60]).await;
    assert_eq!(wrong_token.0, 404);
    assert_eq!(wrong_token, wrong_user);
    assert_eq!(wrong_token, not_hex);
    assert_eq!(wrong_token, short);
    assert_eq!(wrong_token.1["error"]["code"], "not_found");
    // The bearer is the app-auth one.
    let body = json!({ "user": "u1", "token_hash": token_hash_hex("right") });
    assert_eq!(app.post_json("/internal/invites/describe", Some("wrong"), body.clone()).await.0, 401);
    assert_eq!(app.post_json("/internal/invites/describe", None, body).await.0, 401);
    // A body that is not the expected JSON is the same 404 as any other mismatch.
    let junk = app.post_json("/internal/invites/describe", Some(APP_AUTH_TOKEN), json!({ "user": 1 })).await;
    assert_eq!(junk, wrong_token);
}

// R36: the invite page names the workspace by its display name, and by its slug without one.
#[tokio::test]
async fn describe_invite_uses_display_name() {
    let neon = FakeNeon::default();
    invite_fixture(&neon, Some(token_hash("right")));
    let app = describe_app(neon.clone()).await;
    let ask = || {
        let body = json!({ "user": "u1", "token_hash": token_hash_hex("right") });
        let app = &app;
        async move { app.post_json("/internal/invites/describe", Some(APP_AUTH_TOKEN), body).await }
    };
    assert_eq!(ask().await.1["tenant_name"], "acme");
    neon.invites.lock().unwrap().get_mut("u1").unwrap().tenant_display_name = Some("Acme Corp".into());
    assert_eq!(ask().await.1["tenant_name"], "Acme Corp");
}

// Live finding: an inviter without a name falls back to its email's local part, and one with
// neither (a machine user, like the dev bootstrap's demo user) to "An admin" (only tenant and
// project Admins invite), so the invite page never says " invited you to …".
#[tokio::test]
async fn describe_invite_names_an_inviter_without_a_name() {
    let neon = FakeNeon::default();
    invite_fixture(&neon, Some(token_hash("right")));
    let app = describe_app(neon.clone()).await;
    let ask = || {
        let body = json!({ "user": "u1", "token_hash": token_hash_hex("right") });
        let app = &app;
        async move { app.post_json("/internal/invites/describe", Some(APP_AUTH_TOKEN), body).await.1 }
    };
    neon.invites.lock().unwrap().get_mut("u1").unwrap().inviter_name = " ".into();
    let named = ask().await;
    assert_eq!((named["inviter_name"].as_str(), named["inviter_email"].as_str()), (Some("alice"), Some("alice@x.com")));
    neon.invites.lock().unwrap().get_mut("u1").unwrap().inviter_email = None;
    assert_eq!(
        ask().await,
        json!({"inviter_name": "An admin", "inviter_email": null, "tenant_name": "acme"})
    );
}

#[tokio::test]
async fn describe_invite_closed_is_404() {
    // No open invite (revoked, accepted, expired or none: the metadata answers `None`) and an
    // invite made before tokens existed both answer the same 404 as a wrong token.
    let neon = FakeNeon::default();
    let app = describe_app(neon.clone()).await;
    let ask = |hash: String| {
        let app = &app;
        async move {
            app.post_json("/internal/invites/describe", Some(APP_AUTH_TOKEN), json!({ "user": "u1", "token_hash": hash }))
                .await
        }
    };
    let closed = ask(token_hash_hex("t")).await;
    assert_eq!(closed.0, 404);
    invite_fixture(&neon, None);
    assert_eq!(ask(token_hash_hex("t")).await, closed);
    // No stored token, and a presented hash of 64 zeros (what an unset hash might look like).
    assert_eq!(ask("0".repeat(64)).await, closed);
    invite_fixture(&neon, Some([0; 32]));
    assert_eq!(ask(token_hash_hex("t")).await, closed);
}

// R-F4a: a compute that restarts or is recreated pulls its spec here, so the spec carries the
// branch's protection (the compute refuses databases born open). A failed read of it fails the
// request: never `false` by default.
#[tokio::test]
async fn the_pulled_spec_carries_the_branchs_protection() {
    let (neon, branch) = ddl_neon();
    let mut record = branch_record(
        branch,
        DatabaseId::new(),
        None,
        BranchKind::Normal,
        BranchStatus::Ready,
        Some("ep-d"),
    );
    neon.app.lock().unwrap().branches.push(record.clone());
    let app = spawn_internal(neon.clone(), FakeLocator::default(), &storcon(true).await).await;
    let (status, body) = app.get(&spec_path("ep-d"), Some(COMPUTE_TOKEN)).await;
    assert_eq!(status, 200);
    assert_eq!(body["spec"]["branch_protected"], json!(false));
    // Protected: the flag is on.
    record.protected = true;
    neon.app.lock().unwrap().branches[0] = record;
    let (status, body) = app.get(&spec_path("ep-d"), Some(COMPUTE_TOKEN)).await;
    assert_eq!(status, 200);
    assert_eq!(body["spec"]["branch_protected"], json!(true));
}

#[tokio::test]
async fn an_endpoint_without_a_branch_is_not_protected() {
    let neon = FakeNeon::default();
    neon.add("ep-a", TENANT, TIMELINE);
    let app = spawn_internal(neon, FakeLocator::default(), &storcon(true).await).await;
    let (status, body) = app.get(&spec_path("ep-a"), Some(COMPUTE_TOKEN)).await;
    assert_eq!(status, 200);
    assert_eq!(body["spec"]["branch_protected"], json!(false));
}

#[tokio::test]
async fn a_branch_whose_protection_cant_be_read_fails_the_spec() {
    // The endpoint links to a branch the metadata has no record of.
    let (neon, _branch) = ddl_neon();
    let app = spawn_internal(neon, FakeLocator::default(), &storcon(true).await).await;
    let (status, _) = app.get(&spec_path("ep-d"), Some(COMPUTE_TOKEN)).await;
    assert_eq!(status, 503, "fail closed, not branch_protected = false");
}

// --- RT1 T6: realtime's bearer, the realtime routes' absence here, and the events ------------

// R10: realtime reads connection info and key lookups with its own token
// (`require_any_bearer`); each route keeps its own caller's token too.
#[tokio::test]
async fn realtime_token_reaches_connection_info_and_key_lookups() {
    let neon = FakeNeon::default();
    let branch = BranchId::new();
    neon.data_keys.lock().unwrap().push((
        sha("chk_sec_rt"),
        DataKeyLookup { key_id: KEY_ID, branch_id: branch, project_id: PROJECT_ID, kind: DataKeyKind::Secret, role: "reporting".into(), databases: None, role_oid: None },
    ));
    let row = connection_row(BranchStatus::Ready, Some("ep-rt"), None);
    let connected = row.branch_id;
    neon.connections.lock().unwrap().insert(connected, row);
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;

    let keys = format!("/internal/data/keys/{}", hex::encode(sha("chk_sec_rt")));
    assert_eq!(app.get(&keys, Some(REALTIME_TOKEN)).await.0, 200, "realtime's token on key lookups");
    assert_eq!(app.get(&keys, Some(DATA_KEYS_TOKEN)).await.0, 200, "the gateway's still works");
    assert_eq!(app.get(&keys, Some(REST_TOKEN)).await.0, 401, "rest's still doesn't");

    let unknown = format!("/internal/data/branches/{}/connection", BranchId::new().as_uuid());
    assert_eq!(app.get(&unknown, Some(REALTIME_TOKEN)).await.0, 404, "past the bearer: an unknown branch");
    let conn = format!("/internal/data/branches/{}/connection", connected.as_uuid());
    let (realtime, _) = app.get(&conn, Some(REALTIME_TOKEN)).await;
    let (rest, _) = app.get(&conn, Some(REST_TOKEN)).await;
    assert_ne!(realtime, 401, "realtime's token on connection info");
    assert_eq!(realtime, rest, "the same answer as rest's");
    assert_eq!(app.get(&conn, Some(DATA_KEYS_TOKEN)).await.0, 401);
    assert_eq!(app.get(&conn, Some("wrong")).await.0, 401);
}

// R10: realtime's token opens only those two routes on this listener.
#[tokio::test]
async fn realtime_token_opens_nothing_else_here() {
    let w = app_world();
    let a = spawn_app(&w, Some(SIGNING_KEY), None).await;
    let app = &a.app;
    let gets = [
        format!("{ACCESS}?endpointish=ep-x&role=app&session_id=1"),
        format!("{WAKE}?endpointish=ep-x&session_id=1"),
        spec_path("ep-x"),
    ];
    for path in &gets {
        assert_eq!(app.get(path, Some(REALTIME_TOKEN)).await.0, 401, "GET {path}");
    }
    for path in [ATTACH, SAFEKEEPERS] {
        assert_eq!(app.put(path, Some(REALTIME_TOKEN), "{}").await, 401, "PUT {path}");
    }
    assert_eq!(app.post_json("/internal/invites/describe", Some(REALTIME_TOKEN), json!({})).await.0, 401);
    for path in ["/internal/signup/check", "/internal/signup/claimed"] {
        assert_eq!(app.post_json(path, Some(REALTIME_TOKEN), json!({"org": "o1"})).await.0, 401, "POST {path}");
    }
    for (method, path, body) in auth_routes(&w) {
        assert_eq!(call(app, method, &path, Some(REALTIME_TOKEN), &body).await, 401, "{method} {path}");
    }
    let (status, _, _) = app.ddl("ep-x", Some(REALTIME_TOKEN), "{}").await;
    assert_eq!(status, 401, "the DDL receiver takes only the compute's token");
}

// S15: the realtime routes live on the public listener's state; the listener computes reach
// never serves them, whatever the token.
#[tokio::test]
async fn the_internal_listener_serves_no_realtime_route() {
    let app = spawn_internal(FakeNeon::default(), FakeLocator::default(), "http://127.0.0.1:9").await;
    for route in ["authorize", "projects", "visible"] {
        let path = format!("/internal/realtime/{route}");
        for token in [Some(REALTIME_TOKEN), Some(REST_TOKEN), None] {
            let (status, _) = app.post_json(&path, token, json!({ "token": "x" })).await;
            assert_eq!(status, 404, "POST {path} with {token:?}");
        }
    }
}

/// [`ddl_neon`] with the branch's database and project known, and its events recorded.
async fn ddl_events_app() -> (Internal, FakeNeon, FakeRtEvents, BranchId, DatabaseId, ProjectId) {
    let (neon, branch) = ddl_neon();
    let (db, project) = (DatabaseId::new(), ProjectId::new());
    {
        let mut rows = neon.app.lock().unwrap();
        rows.branches.push(branch_record(branch, db, Some(BranchId::new()), BranchKind::Normal, BranchStatus::Ready, Some("ep-d")));
        rows.db_projects.insert(db, project);
    }
    neon.edit(|s| {
        s.dbs.insert("other".into(), ("postgres".into(), DatabaseOrigin::Sql));
    });
    let events = FakeRtEvents::default();
    let app = spawn_with_app(
        neon.clone(),
        FakeLocator::default(),
        "http://127.0.0.1:9",
        rules(Mode::SelfHosted, false),
        FakeCache::default(),
        "http://127.0.0.1:9",
        AppSetup { rt_events: events.clone(), ..AppSetup::default() },
    )
    .await;
    (app, neon, events, branch, db, project)
}

// P10 / spec 5.3: a Postgres database made or dropped inside a branch is a project event,
// published after the receiver's own metadata write (the delta arrives at the creating
// transaction's pre-commit; an aborted CREATE DATABASE costs Studio one refetch).
#[tokio::test]
async fn ddl_database_create_drop_and_rename_publish_pg_database_events() {
    use chelabase_nats::rt::ProjectEvent;
    let (app, _neon, events, branch, db, project) = ddl_events_app().await;
    let (database_id, branch_id) = (db.as_uuid(), branch.as_uuid());
    let created = |name: &str| (project.as_uuid(), ProjectEvent::PgDatabaseCreated { database_id, branch_id, name: name.into() });
    let dropped = |name: &str| (project.as_uuid(), ProjectEvent::PgDatabaseDropped { database_id, branch_id, name: name.into() });

    let (status, _, body) = app
        .ddl("ep-d", Some(COMPUTE_TOKEN), &dbs_delta(vec![json!({ "op": "set", "name": "fresh", "owner": "postgres" })]))
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(wait_until(|| events.project.lock().unwrap().len() == 1).await, "{:?}", events.project.lock().unwrap());
    assert_eq!(*events.project.lock().unwrap(), vec![created("fresh")]);

    let (status, _, _) = app.ddl("ep-d", Some(COMPUTE_TOKEN), &dbs_delta(vec![json!({ "op": "del", "name": "fresh" })])).await;
    assert_eq!(status, 200);
    assert!(wait_until(|| events.project.lock().unwrap().len() == 2).await);
    assert_eq!(events.project.lock().unwrap()[1], dropped("fresh"));

    // A rename is the old name dropped and the new one made, in one delta.
    let (status, _, _) = app
        .ddl("ep-d", Some(COMPUTE_TOKEN), &dbs_delta(vec![json!({ "op": "set", "name": "renamed", "old_name": "other" })]))
        .await;
    assert_eq!(status, 200);
    assert!(wait_until(|| events.project.lock().unwrap().len() == 4).await, "{:?}", events.project.lock().unwrap());
    let mut last: Vec<_> = events.project.lock().unwrap()[2..].to_vec();
    last.sort_by_key(|(_, e)| format!("{e:?}"));
    let mut want = vec![created("renamed"), dropped("other")];
    want.sort_by_key(|(_, e)| format!("{e:?}"));
    assert_eq!(last, want);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(events.project.lock().unwrap().len(), 4, "one event each");
    assert!(events.access.lock().unwrap().is_empty(), "no access event from a database change");
}

// RT1 final fix round (batch B, T6 hardening): `ALTER DATABASE … OWNER TO` sends the same
// `set` as a `CREATE DATABASE`; for a database the branch already has it is no new database,
// so no `pg_database.created` (realtime and Studio would refetch for nothing).
#[tokio::test]
async fn ddl_owner_change_of_a_known_database_publishes_no_pg_database_event() {
    use chelabase_nats::rt::ProjectEvent;
    let (app, _neon, events, branch, db, project) = ddl_events_app().await;
    let (database_id, branch_id) = (db.as_uuid(), branch.as_uuid());
    let (status, _, body) = app
        .ddl("ep-d", Some(COMPUTE_TOKEN), &dbs_delta(vec![json!({ "op": "set", "name": "fresh", "owner": "postgres" })]))
        .await;
    assert_eq!(status, 200, "{body}");
    assert!(wait_until(|| events.project.lock().unwrap().len() == 1).await, "{:?}", events.project.lock().unwrap());
    assert_eq!(
        *events.project.lock().unwrap(),
        vec![(project.as_uuid(), ProjectEvent::PgDatabaseCreated { database_id, branch_id, name: "fresh".into() })]
    );
    // The owner changes, twice; the main database's too.
    for (name, owner) in [("fresh", "app_owner"), ("fresh", "postgres"), ("neondb", "postgres")] {
        let (status, _, body) = app
            .ddl("ep-d", Some(COMPUTE_TOKEN), &dbs_delta(vec![json!({ "op": "set", "name": name, "owner": owner })]))
            .await;
        assert_eq!(status, 200, "{name}: {body}");
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let published = events.project.lock().unwrap().clone();
    assert_eq!(published.len(), 1, "an owner change is no new database: {published:?}");
}

// None on failure: a refused delta, a metadata error, a role-only delta.
#[tokio::test]
async fn ddl_refused_failed_or_role_only_deltas_publish_no_pg_database_event() {
    let (app, neon, events, _, _, _) = ddl_events_app().await;
    // Renaming the main database away is refused (409).
    let (status, _, _) = app
        .ddl("ep-d", Some(COMPUTE_TOKEN), &dbs_delta(vec![json!({ "op": "set", "name": "elsewhere", "old_name": "neondb" })]))
        .await;
    assert_eq!(status, 409);
    let (status, _, _) = app.ddl("ep-d", Some(COMPUTE_TOKEN), &roles_delta(vec![set("app_rw", STRONG, SCRAM)])).await;
    assert_eq!(status, 200);
    *neon.fail.lock().unwrap() = true;
    let (status, _, _) = app
        .ddl("ep-d", Some(COMPUTE_TOKEN), &dbs_delta(vec![json!({ "op": "set", "name": "fresh", "owner": "postgres" })]))
        .await;
    assert_eq!(status, 503);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(events.project.lock().unwrap().is_empty(), "{:?}", events.project.lock().unwrap());
}

// W18: a member's Zitadel user removed, deactivated or locked changes what they may reach: an
// access event for the tenant bound to that org, naming the member (`sub` = the user id).
#[tokio::test]
async fn webhook_member_user_events_publish_an_access_event() {
    use chelabase_nats::rt::AccessEvent;
    let w = app_world();
    let tenant = TenantId::new();
    w.neon.tenants.lock().unwrap().insert("tenant-org".into(), tenant);
    let a = spawn_app(&w, Some(SIGNING_KEY), None).await;
    // Zitadel ids are numeric strings, not uuids.
    let kinds = ["user.removed", "user.deactivated", "user.locked"];
    for kind in kinds {
        let body = user_event(kind, "tenant-org", "284617", chrono::Utc::now());
        assert_eq!(a.app.event(&body, signed(&body)).await, 200, "{kind}");
    }
    assert!(eventually(|| a.rt_events.access.lock().unwrap().len() == kinds.len()).await, "{:?}", a.rt_events.access.lock().unwrap());
    for ev in a.rt_events.access.lock().unwrap().iter() {
        assert_eq!(*ev, AccessEvent { tenant_id: tenant.as_uuid(), member_sub: Some("284617".into()) });
    }
    // A bad signature, an org no tenant has, and an app-users org publish no access event.
    let body = user_event("user.removed", "tenant-org", "284617", chrono::Utc::now());
    assert_eq!(a.app.event(&body, Some("nonsense".into())).await, 401);
    for org in ["no-such-org", ORG] {
        let body = user_event("user.removed", org, &uuid::Uuid::new_v4().to_string(), chrono::Utc::now());
        assert_eq!(a.app.event(&body, signed(&body)).await, 200, "{org}");
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(a.rt_events.access.lock().unwrap().len(), kinds.len());
}

// ---- Members hub T8: the webhook revokes an offboarded member's personal logins (D4, P17) ----

/// The webhook's app with tenant org `tenant-org` bound, its Zitadel user `284617` holding a
/// personal login `zed` with rows on `ep-main` and `ep-child`; the cache, the resync port and
/// the realtime events the app was given.
struct Offboarding {
    app: Internal,
    neon: FakeNeon,
    cache: FakeCache,
    resync: FakeResync,
    rt_events: FakeRtEvents,
    tenant: TenantId,
    main: BranchId,
    child: BranchId,
}

impl Offboarding {
    fn rows(&self) -> Vec<(String, String)> {
        vec![("ep-main".into(), "zed".into()), ("ep-child".into(), "zed".into())]
    }
}

async fn offboarding() -> Offboarding {
    let w = app_world();
    let tenant = TenantId::new();
    w.neon.tenants.lock().unwrap().insert("tenant-org".into(), tenant);
    let row = |branch: BranchId, endpoint: &str| LoginEndpoint {
        branch_id: branch.as_uuid(),
        endpoint_id: endpoint.into(),
        role_name: "zed".into(),
    };
    w.neon
        .login_rows
        .lock()
        .unwrap()
        .insert((tenant, "284617".into()), vec![row(w.main, "ep-main"), row(w.child, "ep-child")]);
    let (cache, resync, rt_events) = (FakeCache::default(), FakeResync::default(), FakeRtEvents::default());
    let app = spawn_with_app(
        w.neon.clone(),
        w.locator.clone(),
        "http://127.0.0.1:9",
        rules(Mode::SelfHosted, false),
        cache.clone(),
        "http://127.0.0.1:9",
        AppSetup {
            key: w.key.clone(),
            resync: resync.clone(),
            rt_events: rt_events.clone(),
            ..AppSetup::default()
        },
    )
    .await;
    Offboarding { app, neon: w.neon.clone(), cache, resync, rt_events, tenant, main: w.main, child: w.child }
}

// D4: `user.deactivated`, `user.locked` and `user.removed` for a tenant org revoke the member's
// logins (the metadata write), tell the proxy cache about every row, ask each branch to
// reconcile its member roles, then publish today's realtime access event (W18). An unknown user
// revokes nothing; another org isn't a tenant's.
#[tokio::test]
async fn a_deactivated_zitadel_user_loses_their_logins_and_the_cache_is_cleared() {
    for kind in ["user.deactivated", "user.locked", "user.removed"] {
        let o = offboarding().await;
        let body = user_event(kind, "tenant-org", "284617", chrono::Utc::now());
        assert_eq!(o.app.event(&body, signed(&body)).await, 200, "{kind}");
        assert_eq!(
            o.neon.revocations.lock().unwrap().clone(),
            vec![(o.tenant, "284617".to_string())],
            "{kind}: the member's logins are revoked"
        );
        assert!(
            o.cache.wait_for(&o.rows()).await,
            "{kind}: the proxy cache is told about every row: {:?}",
            o.cache.sent.lock().unwrap()
        );
        let asked = o.resync.members_asked.lock().unwrap().clone();
        for branch in [o.main, o.child] {
            assert!(asked.contains(&branch), "{kind}: {branch} reconciles its member roles: {asked:?}");
        }
        assert!(
            eventually(|| o.rt_events.access.lock().unwrap().len() == 1).await,
            "{kind}: the realtime access event still goes out"
        );
    }

    let o = offboarding().await;
    let app_user = uuid::Uuid::new_v4().to_string();
    for (org, user) in [("tenant-org", "999"), ("no-such-org", "284617"), (ORG, app_user.as_str())] {
        let body = user_event("user.deactivated", org, user, chrono::Utc::now());
        assert_eq!(o.app.event(&body, signed(&body)).await, 200, "{org} {user}");
    }
    assert_eq!(
        o.neon.revocations.lock().unwrap().clone(),
        vec![(o.tenant, "999".to_string())],
        "only the tenant org's event asks for a revocation"
    );
    assert!(o.cache.sent.lock().unwrap().is_empty(), "an unknown user has no rows to clear");
}

// Selfhost-install m5 (AC 2): the orchestrator starts before the bootstrap has written the
// Actions signing key. Until the file is there the webhook answers 503 (never an unchecked
// pass, signed or not); once it appears, a signed `user.deactivated` is 200 and revokes the
// member's logins, with no restart. A key file that's readable by others is refused (503).
#[tokio::test]
async fn webhook_picks_up_late_actions_key() {
    use chelabase_types::file_value::{FileValue, Sensitivity};
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("actions-signing-key");
    let w = app_world();
    let tenant = TenantId::new();
    w.neon.tenants.lock().unwrap().insert("tenant-org".into(), tenant);
    let app = spawn_with_app(
        w.neon.clone(),
        w.locator.clone(),
        "http://127.0.0.1:9",
        rules(Mode::SelfHosted, false),
        FakeCache::default(),
        "http://127.0.0.1:9",
        AppSetup {
            key: w.key.clone(),
            signing_key: Some(FileValue::file(
                "CHELABASE_ZITADEL_ACTIONS_SIGNING_KEY_FILE",
                &path,
                Sensitivity::Secret,
            )),
            ..AppSetup::default()
        },
    )
    .await;
    let body = user_event("user.deactivated", "tenant-org", "284617", chrono::Utc::now());
    assert_eq!(app.event(&body, signed(&body)).await, 503, "no key file yet");
    assert_eq!(app.event(&body, None).await, 503, "unsigned, no key file yet");
    std::fs::write(&path, "").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(app.event(&body, signed(&body)).await, 503, "an empty key file");
    std::fs::write(&path, format!("{SIGNING_KEY}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(app.event(&body, signed(&body)).await, 503, "a key file others can read");
    assert!(w.neon.revocations.lock().unwrap().is_empty(), "nothing ran before the key");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(app.event(&body, None).await, 401, "the key is there: unsigned is refused");
    assert_eq!(app.event(&body, signed(&body)).await, 200);
    assert_eq!(
        w.neon.revocations.lock().unwrap().clone(),
        vec![(tenant, "284617".to_string())],
        "the member's logins are revoked"
    );
}

// P17: a revocation write that fails answers 500, so Zitadel delivers the event again, and
// nothing after it ran (no cache message, no realtime event); the redelivery completes it.
#[tokio::test]
async fn a_failed_revocation_is_500_so_zitadel_retries() {
    let o = offboarding().await;
    *o.neon.fail_revocation.lock().unwrap() = true;
    let body = user_event("user.deactivated", "tenant-org", "284617", chrono::Utc::now());
    assert_eq!(o.app.event(&body, signed(&body)).await, 500);
    assert!(o.cache.sent.lock().unwrap().is_empty());
    assert!(o.resync.members_asked.lock().unwrap().is_empty());
    assert!(o.rt_events.access.lock().unwrap().is_empty(), "the revocation comes before the realtime event");

    *o.neon.fail_revocation.lock().unwrap() = false;
    assert_eq!(o.app.event(&body, signed(&body)).await, 200, "the redelivery");
    assert!(o.cache.wait_for(&o.rows()).await, "{:?}", o.cache.sent.lock().unwrap());
    assert!(eventually(|| !o.rt_events.access.lock().unwrap().is_empty()).await);
    assert_eq!(o.rt_events.access.lock().unwrap().len(), 1, "the failed delivery published nothing");
}

// ---- Members hub step 1, final fix round, area A ----

// T8-2 (P17): a tenant lookup that fails on an offboarding event is a 500, so Zitadel delivers
// it again; never a 200 that skipped the revocation.
#[tokio::test]
async fn a_failed_tenant_lookup_on_offboarding_is_500() {
    for kind in ["user.deactivated", "user.locked", "user.removed"] {
        let o = offboarding().await;
        *o.neon.fail_tenant_lookup.lock().unwrap() = true;
        let body = user_event(kind, "tenant-org", "284617", chrono::Utc::now());
        assert_eq!(o.app.event(&body, signed(&body)).await, 500, "{kind}");
        *o.neon.fail_tenant_lookup.lock().unwrap() = false;
        assert_eq!(o.app.event(&body, signed(&body)).await, 200, "{kind}: the redelivery");
        assert_eq!(o.neon.revocations.lock().unwrap().clone(), vec![(o.tenant, "284617".to_string())], "{kind}");
    }
}

// D7: `user.reactivated` and `user.unlocked` for a tenant org clear the member's offboarded
// mark (its logins stay revoked: nothing is revoked or published for them); a failed clear is
// a 500 so Zitadel retries; an app-users org's reactivation still syncs the app user.
#[tokio::test]
async fn a_reactivated_or_unlocked_user_loses_the_offboarded_mark() {
    for kind in ["user.reactivated", "user.unlocked"] {
        let o = offboarding().await;
        let body = user_event(kind, "tenant-org", "284617", chrono::Utc::now());
        assert_eq!(o.app.event(&body, signed(&body)).await, 200, "{kind}");
        assert_eq!(
            o.neon.clears.lock().unwrap().clone(),
            vec![(o.tenant, "284617".to_string())],
            "{kind}: the mark isn't cleared"
        );
        assert!(o.neon.revocations.lock().unwrap().is_empty(), "{kind}");

        *o.neon.fail_clear.lock().unwrap() = true;
        assert_eq!(o.app.event(&body, signed(&body)).await, 500, "{kind}: a failed clear");
    }
    let w = app_world();
    let a = spawn_app(&w, Some(SIGNING_KEY), None).await;
    let user = uuid::Uuid::new_v4();
    let body = user_event("user.reactivated", ORG, &user.to_string(), chrono::Utc::now());
    assert_eq!(a.app.event(&body, signed(&body)).await, 200);
    assert!(eventually(|| a.published().len() == 1).await, "{:?}", a.published());
}

// ---- Fork round 4, C4 (RT4-2, SP1, SP4, SP11, SP23) ----
//
// Kept apart from the rest of the file (C3 edits it too, SP20): the spec hook reads
// `Compute-Capabilities`, the storcon-notify reconfigure asks compute_ctl's `/status` first, and
// the connection route gains `replication_host`.

mod c4 {
    use super::*;
    use axum::http::{HeaderName, HeaderValue};
    use base64::Engine as _;
    use chelabase_neon::spec::MemberLogins;

    const CAPS: &str = "compute-capabilities";
    const REFRESH: &str = "storage_token_refresh";

    async fn spawn(
        neon: FakeNeon,
        locator: FakeLocator,
        control: FakeLocator,
        realtime_logins: MemberLogins,
    ) -> Internal {
        spawn_with_app(
            neon,
            locator,
            &storcon(true).await,
            rules(Mode::SelfHosted, false),
            FakeCache::default(),
            "http://127.0.0.1:9",
            AppSetup { control, realtime_logins, ..AppSetup::default() },
        )
        .await
    }

    /// A compute_ctl spec fetch with these `Compute-Capabilities` header lines (raw bytes, in
    /// order); the spec body.
    async fn fetch(app: &Internal, lines: &[&[u8]]) -> Value {
        let mut req = app
            .http
            .get(format!("{}{}", app.base, spec_path("ep-a")))
            .bearer_auth(COMPUTE_TOKEN);
        for line in lines {
            req = req.header(
                HeaderName::from_static(CAPS),
                HeaderValue::from_bytes(line).unwrap(),
            );
        }
        let resp = req.send().await.unwrap();
        assert_eq!(resp.status().as_u16(), 200, "a spec is served whatever the header says");
        resp.json().await.unwrap()
    }

    /// The claims of a JWT, unverified.
    fn claims(token: &str) -> Value {
        let payload = token.split('.').nth(1).expect("a JWT");
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn app_neon() -> FakeNeon {
        let neon = FakeNeon::default();
        neon.add("ep-a", TENANT, TIMELINE);
        neon
    }

    /// The spec's storage token: `Some(expiring?)`.
    fn expiring(spec: &Value) -> bool {
        let token = spec["spec"]["storage_auth_token"].as_str().expect("a storage token");
        let c = claims(token);
        assert_eq!(c["scope"], "tenant", "{c}");
        assert_eq!(c["tenant_id"], TENANT, "{c}");
        match c.get("exp") {
            Some(exp) => {
                let (exp, iat) = (exp.as_u64().unwrap(), c["iat"].as_u64().expect("iat with exp"));
                assert_eq!(exp - iat, 86_400, "the 24 h lifetime: {c}");
                true
            }
            None => {
                assert!(c.get("iat").is_none(), "today's token has neither exp nor iat: {c}");
                false
            }
        }
    }

    #[tokio::test]
    async fn a_compute_that_advertises_the_refresh_gets_an_expiring_token() {
        let app = spawn(app_neon(), FakeLocator::default(), FakeLocator::default(), Default::default()).await;
        assert!(expiring(&fetch(&app, &[REFRESH.as_bytes()]).await));
    }

    #[tokio::test]
    async fn no_header_means_not_capable() {
        let app = spawn(app_neon(), FakeLocator::default(), FakeLocator::default(), Default::default()).await;
        assert!(!expiring(&fetch(&app, &[]).await), "an old compute_ctl sends no header");
    }

    // SP23: every value of the header (`get_all`), joined, split on `,`, trimmed, compared
    // exactly and case-sensitively.
    #[tokio::test]
    async fn the_header_is_read_as_a_comma_list_across_all_its_lines() {
        let app = spawn(app_neon(), FakeLocator::default(), FakeLocator::default(), Default::default()).await;
        for (lines, capable) in [
            (vec![&b" other , storage_token_refresh "[..]], true),
            (vec![&b"other"[..], &b"storage_token_refresh"[..]], true),
            (vec![&b"storage_token_refresh"[..], &b"other"[..]], true),
            (vec![&b"x,storage_token_refresh,y"[..]], true),
            (vec![&b""[..]], false),
            (vec![&b"other"[..]], false),
            (vec![&b"storage_token_refresh_v2"[..]], false),
            (vec![&b"Storage_Token_Refresh"[..]], false),
            (vec![&b"storage_token"[..], &b"refresh"[..]], false),
            // Not UTF-8: not capable (and the spec is still served).
            (vec![&b"\xff\xfe"[..]], false),
        ] {
            assert_eq!(
                expiring(&fetch(&app, &lines).await),
                capable,
                "{:?}",
                lines.iter().map(|l| String::from_utf8_lossy(l).into_owned()).collect::<Vec<_>>()
            );
        }
    }

    // SP4: the spec carries the orchestrator's configured realtime logins, not an interim empty
    // list.
    #[tokio::test]
    async fn the_spec_carries_the_configured_realtime_logins() {
        let logins = MemberLogins::realtime(vec!["10.213.4.12/32".into(), "10.213.4.11/32".into()]);
        let app = spawn(app_neon(), FakeLocator::default(), FakeLocator::default(), logins).await;
        let spec = fetch(&app, &[]).await;
        assert_eq!(
            spec["spec"]["realtime_logins"],
            json!({ "allowed_cidrs": ["10.213.4.12/32", "10.213.4.11/32"], "client_cns": ["chelabase-realtime"] })
        );
        // An empty list stays empty ("no address rule"), still with the CN.
        let app = spawn(app_neon(), FakeLocator::default(), FakeLocator::default(), MemberLogins::realtime(vec![])).await;
        let spec = fetch(&app, &[]).await;
        assert_eq!(
            spec["spec"]["realtime_logins"],
            json!({ "allowed_cidrs": [], "client_cns": ["chelabase-realtime"] })
        );
    }

    // ---- the storcon-notify reconfigure (SP1) ----

    fn caps(list: &[&str]) -> Value {
        json!({ "status": "running", "capabilities": list })
    }

    /// One endpoint with a running compute_ctl stand-in; the notify-attach answer and what the
    /// compute received.
    async fn notify(status: (u16, Value), logins: MemberLogins) -> (u16, Configured, Events) {
        let neon = app_neon();
        let (addr, seen, events) = compute_ctl(200, status.0, status.1).await;
        let locator = FakeLocator::default();
        locator.running("ep-a", "172.20.0.9", Some(addr));
        let app = spawn(neon, locator, FakeLocator::default(), logins).await;
        let code = app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await;
        (code, seen, events)
    }

    #[tokio::test]
    async fn a_capable_compute_is_reconfigured_with_an_expiring_token() {
        let (code, seen, _) = notify((200, caps(&[REFRESH])), Default::default()).await;
        assert_eq!(code, 200);
        let seen = seen.0.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert!(expiring(&seen[0].1), "capability read from /status");
    }

    #[tokio::test]
    async fn an_incapable_or_old_compute_keeps_a_non_expiring_token() {
        for status in [
            caps(&[]),
            caps(&["something_else"]),
            // An old compute_ctl: no `capabilities` at all.
            json!({ "status": "running" }),
        ] {
            let (code, seen, events) = notify((200, status.clone()), Default::default()).await;
            assert_eq!(code, 200, "{status}");
            let order: Vec<_> = events.lock().unwrap().iter().map(|(w, _)| *w).collect();
            assert_eq!(order, ["status", "configure"], "{status}: the report is read first");
            let seen = seen.0.lock().unwrap().clone();
            assert_eq!(seen.len(), 1, "{status}");
            assert!(!expiring(&seen[0].1), "{status}");
        }
    }

    #[tokio::test]
    async fn the_reconfigure_reads_the_status_before_it_pushes() {
        let (code, _, events) = notify((200, caps(&[REFRESH])), Default::default()).await;
        assert_eq!(code, 200);
        let events = events.lock().unwrap().clone();
        let order: Vec<_> = events.iter().map(|(what, _)| *what).collect();
        assert_eq!(order, ["status", "configure"]);
        for (what, auth) in events {
            assert!(auth.starts_with("Bearer "), "{what} carries the compute's control token");
        }
    }

    // A failed report fails that reconfigure like a failed push (the storage controller then
    // retries): never a silent push with a non-expiring token.
    #[tokio::test]
    async fn a_failed_status_report_fails_the_reconfigure_and_pushes_nothing() {
        for status in [
            (500, json!({ "error": "boom" })),
            (404, json!({})),
            (401, json!({})),
            // 200 but not a status.
            (200, json!("not a status")),
        ] {
            let (code, seen, events) = notify(status.clone(), Default::default()).await;
            assert_eq!(code, 503, "{status:?}: the storage controller retries a 503");
            assert!(seen.0.lock().unwrap().is_empty(), "{status:?}: nothing was pushed");
            let order: Vec<_> = events.lock().unwrap().iter().map(|(w, _)| *w).collect();
            assert_eq!(order, ["status"], "{status:?}");
        }
    }

    // The hook and the spec fetch use one `SpecPolicy`: the same realtime logins.
    #[tokio::test]
    async fn the_reconfigure_pushes_the_configured_realtime_logins() {
        let logins = MemberLogins::realtime(vec!["10.213.4.12/32".into()]);
        let (code, seen, _) = notify((200, caps(&[REFRESH])), logins).await;
        assert_eq!(code, 200);
        let seen = seen.0.lock().unwrap().clone();
        assert_eq!(
            seen[0].1["spec"]["realtime_logins"],
            json!({ "allowed_cidrs": ["10.213.4.12/32"], "client_cns": ["chelabase-realtime"] })
        );
    }

    // One endpoint's failed report doesn't stop the others (as one failing push, round 3).
    #[tokio::test]
    async fn one_failing_status_does_not_stop_the_other_computes() {
        let neon = FakeNeon::default();
        neon.add("ep-a", TENANT, TIMELINE);
        neon.add("ep-b", TENANT, "33333333333333333333333333333333");
        let (bad, bad_seen, _) = compute_ctl(200, 500, json!({})).await;
        let (good, good_seen, _) = compute_ctl(200, 200, caps(&[REFRESH])).await;
        let locator = FakeLocator::default();
        locator.running("ep-a", "172.20.0.9", Some(bad));
        locator.running("ep-b", "172.20.0.10", Some(good));
        let app = spawn(neon, locator, FakeLocator::default(), Default::default()).await;
        assert_eq!(app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await, 503);
        assert!(bad_seen.0.lock().unwrap().is_empty());
        assert_eq!(good_seen.0.lock().unwrap().len(), 1, "ep-b was still reconfigured");
    }

    // A compute that isn't running is still skipped without a report.
    #[tokio::test]
    async fn a_compute_that_is_not_running_gets_no_status_call() {
        let neon = app_neon();
        let (_addr, seen, events) = compute_ctl(200, 200, caps(&[REFRESH])).await;
        let app = spawn(neon, FakeLocator::default(), FakeLocator::default(), Default::default()).await;
        assert_eq!(app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await, 200);
        assert!(seen.0.lock().unwrap().is_empty());
        assert!(events.lock().unwrap().is_empty());
    }

    // ---- the connection route's `replication_host` (SP11) ----

    async fn connection_with(control: FakeLocator) -> (u16, Value) {
        let neon = FakeNeon::default();
        let locator = FakeLocator::default();
        let app = spawn(neon.clone(), locator.clone(), control, Default::default()).await;
        locator.pg.lock().unwrap().insert("ep-1".into(), "127.0.0.1:55432".parse().unwrap());
        let mut row = connection_row(BranchStatus::Ready, Some("ep-1"), None);
        row.authenticator_password_enc = Some(seal_credential(
            &app.key,
            &row.branch_id.to_string(),
            Purpose::Authenticator,
            "auth-pw",
        ));
        neon.connections.lock().unwrap().insert(row.branch_id, row.clone());
        app.get(
            &format!("/internal/data/branches/{}/connection", row.branch_id.as_uuid()),
            Some(REST_TOKEN),
        )
        .await
    }

    #[tokio::test]
    async fn the_connection_route_gives_the_computes_control_network_address() {
        let control = FakeLocator::default();
        control.running("ep-1", "10.213.4.20", None);
        let (code, body) = connection_with(control).await;
        assert_eq!(code, 200, "{body}");
        assert_eq!(body["replication_host"], "10.213.4.20", "the IP alone, as a string: {body}");
        // `host` is still the compute-network address that rest and the check pools dial.
        assert_eq!(body["host"], "127.0.0.1");
        assert_eq!(body["port"], 55432);
        assert_eq!(body["server_name"], "ep-1.compute.chelabase.internal");
    }

    #[tokio::test]
    async fn a_missing_control_address_is_null_and_the_rest_is_unchanged() {
        // The control locator finds nothing for the endpoint.
        let (code, body) = connection_with(FakeLocator::default()).await;
        assert_eq!(code, 200, "{body}");
        assert!(body["replication_host"].is_null(), "{body}");
        assert!(body.as_object().unwrap().contains_key("replication_host"), "null, not absent: {body}");
        assert_eq!(body["host"], "127.0.0.1");
        assert_eq!(body["user"], "authenticator");
        assert_eq!(body["status"], "ready");
    }

    // A control-locator failure (Docker down on that network) must not turn rest's answer
    // into a 409 or a 5xx.
    #[tokio::test]
    async fn a_failing_control_locator_is_null_never_a_409() {
        let control = FakeLocator::default();
        control.running("ep-1", "10.213.4.20", None);
        *control.broken.lock().unwrap() = true;
        let (code, body) = connection_with(control).await;
        assert_eq!(code, 200, "{body}");
        assert!(body.as_object().unwrap().contains_key("replication_host"), "null, not absent: {body}");
        assert!(body["replication_host"].is_null(), "{body}");
        assert_eq!(body["host"], "127.0.0.1");
        assert_eq!(body["password"], "auth-pw");
        assert_eq!(body["status"], "ready");
    }
}

/// Fork round 4, final fix round (F2, part 3): the storage-controller hook answers before the
/// storage controller gives up (10 s) whatever a compute_ctl does, never pushes a spec to a
/// compute other than the one whose status it read, and the connection route answers without
/// waiting on a hung control-network lookup.
mod f2 {
    use super::*;
    use std::time::{Duration, Instant};

    /// The storage controller's hook timeout; the hook must answer well within it.
    const HOOK_ANSWER_WITHIN: Duration = Duration::from_secs(9);

    /// What a compute_ctl stand-in does on each route.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Answer {
        Running,
        Hang,
    }

    /// A compute_ctl stand-in: `/status` and `/configure` answer or hang; `on_status` runs on
    /// each `/status` before it answers. The number of `/configure` requests it got.
    async fn stand_in(
        status: Answer,
        configure: Answer,
        on_status: Arc<dyn Fn() + Send + Sync>,
    ) -> (SocketAddr, Arc<AtomicUsize>) {
        let configured = Arc::new(AtomicUsize::new(0));
        let seen = configured.clone();
        let app = Router::new()
            .route(
                "/status",
                get(move || {
                    let on_status = on_status.clone();
                    async move {
                        on_status();
                        if status == Answer::Hang {
                            std::future::pending::<()>().await;
                        }
                        axum::Json(json!({ "status": "running", "capabilities": ["storage_token_refresh"] }))
                    }
                }),
            )
            .route(
                "/configure",
                axum::routing::post(move || {
                    let seen = seen.clone();
                    async move {
                        seen.fetch_add(1, Ordering::SeqCst);
                        if configure == Answer::Hang {
                            std::future::pending::<()>().await;
                        }
                        (AxumStatus::OK, "{}")
                    }
                }),
            );
        (serve(app).await, configured)
    }

    fn nothing() -> Arc<dyn Fn() + Send + Sync> {
        Arc::new(|| {})
    }

    /// One endpoint `ep-a` on [`TENANT`], its compute_ctl at `addr`; the notify-attach answer
    /// and how long it took. Gives up after 12 s (the storage controller already did at 10 s).
    ///
    /// The tests using it run on the real clock: the hook talks to its compute_ctl stand-in
    /// over loopback sockets, and a paused clock auto-advances to the next timer whenever the
    /// runtime is idle, including while a socket connect is still in flight in the kernel.
    /// Where a loopback connect isn't complete by tokio's zero-timeout poll (WSL2), that fired
    /// the 12 s guard (or the hook's own timeouts) before the request was even delivered.
    async fn notify(locator: FakeLocator) -> (Option<u16>, Duration) {
        let neon = FakeNeon::default();
        neon.add("ep-a", TENANT, TIMELINE);
        let app = spawn_internal(neon, locator, &storcon(true).await).await;
        let started = Instant::now();
        let code = tokio::time::timeout(
            Duration::from_secs(12),
            app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)),
        )
        .await
        .ok();
        (code, started.elapsed())
    }

    // A compute_ctl that never answers `/status` (hung, or a black-holed address): the hook
    // answers 503 (the storage controller retries) before the storage controller's 10 s.
    #[tokio::test]
    async fn a_hung_status_read_still_answers_the_hook_in_time() {
        let (addr, configured) = stand_in(Answer::Hang, Answer::Running, nothing()).await;
        let locator = FakeLocator::default();
        locator.running("ep-a", "172.20.0.9", Some(addr));
        let (code, took) = notify(locator).await;
        assert_eq!(code, Some(503), "answered after {took:?}");
        assert!(took < HOOK_ANSWER_WITHIN, "the hook took {took:?}");
        assert_eq!(configured.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_hung_push_still_answers_the_hook_in_time() {
        let (addr, configured) = stand_in(Answer::Running, Answer::Hang, nothing()).await;
        let locator = FakeLocator::default();
        locator.running("ep-a", "172.20.0.9", Some(addr));
        let (code, took) = notify(locator).await;
        assert_eq!(code, Some(503), "answered after {took:?}");
        assert!(took < HOOK_ANSWER_WITHIN, "the hook took {took:?}");
        assert_eq!(configured.load(Ordering::SeqCst), 1, "the push was tried");
    }

    // The compute is replaced while the hook reads its status (a restart onto another address):
    // the spec built from the old compute's report is pushed to neither. The hook re-checks the
    // compute's control address before the push; a different one is retried (503: the next try
    // reads the new compute's status), none at all is "not running" (200: it pulls its spec at
    // boot).
    #[tokio::test]
    async fn a_compute_replaced_between_status_and_push_gets_no_spec() {
        for gone in [false, true] {
            let (new_addr, new_configured) = stand_in(Answer::Running, Answer::Running, nothing()).await;
            let locator = FakeLocator::default();
            let swap = {
                let locator = locator.clone();
                Arc::new(move || {
                    if gone {
                        locator.controls.lock().unwrap().remove("ep-a");
                    } else {
                        locator.controls.lock().unwrap().insert("ep-a".into(), new_addr);
                    }
                }) as Arc<dyn Fn() + Send + Sync>
            };
            let (old_addr, old_configured) = stand_in(Answer::Running, Answer::Running, swap).await;
            locator.running("ep-a", "172.20.0.9", Some(old_addr));

            let (code, took) = notify(locator).await;

            let case = if gone { "gone" } else { "moved" };
            assert_eq!(old_configured.load(Ordering::SeqCst), 0, "{case}: the old compute got a push");
            assert_eq!(
                new_configured.load(Ordering::SeqCst),
                0,
                "{case}: the new compute got a spec built from the old one's status"
            );
            assert_eq!(code, Some(if gone { 200 } else { 503 }), "{case} after {took:?}");
        }
    }

    // Nothing changed between the status read and the push: the push goes through as before.
    #[tokio::test]
    async fn an_unchanged_compute_is_still_reconfigured() {
        let (addr, configured) = stand_in(Answer::Running, Answer::Running, nothing()).await;
        let locator = FakeLocator::default();
        locator.running("ep-a", "172.20.0.9", Some(addr));
        let (code, _) = notify(locator).await;
        assert_eq!(code, Some(200));
        assert_eq!(configured.load(Ordering::SeqCst), 1);
    }

    // SP11's control-network lookup is bounded: a hung Docker on that network gives
    // `replication_host: null` within 5 s, the rest of the answer unchanged.
    #[tokio::test]
    async fn a_hung_control_lookup_does_not_hold_up_the_connection_route() {
        let neon = FakeNeon::default();
        let locator = FakeLocator::default();
        let control = FakeLocator::default();
        control.running("ep-1", "10.213.4.20", None);
        *control.hang.lock().unwrap() = true;
        let app = spawn_with_app(
            neon.clone(),
            locator.clone(),
            &storcon(true).await,
            rules(Mode::SelfHosted, false),
            FakeCache::default(),
            "http://127.0.0.1:9",
            AppSetup { control, ..AppSetup::default() },
        )
        .await;
        let pg: SocketAddr = (std::net::Ipv4Addr::LOCALHOST, 55432).into();
        locator.pg.lock().unwrap().insert("ep-1".into(), pg);
        let mut row = connection_row(BranchStatus::Ready, Some("ep-1"), None);
        row.authenticator_password_enc = Some(seal_credential(
            &app.key,
            &row.branch_id.to_string(),
            Purpose::Authenticator,
            "auth-pw",
        ));
        neon.connections.lock().unwrap().insert(row.branch_id, row.clone());

        let started = Instant::now();
        let answer = tokio::time::timeout(
            Duration::from_secs(12),
            app.get(
                &format!("/internal/data/branches/{}/connection", row.branch_id.as_uuid()),
                Some(REST_TOKEN),
            ),
        )
        .await;
        let took = started.elapsed();
        let (code, body) = answer.unwrap_or_else(|_| panic!("no answer within 12 s"));
        assert!(took < Duration::from_secs(6), "the route took {took:?}");
        assert_eq!(code, 200, "{body}");
        assert!(body["replication_host"].is_null(), "{body}");
        assert_eq!(body["port"], 55432);
        assert_eq!(body["password"], "auth-pw");
    }
}

/// Carried gaps T4 (R7, P10): notify-attach publishes a tenant-attached event to JetStream
/// (awaiting the ack) alongside the reconfigures, under the hook's one deadline; 200 only when
/// both succeed, else 503 (the storage controller retries, so no attach goes unrecorded).
mod t4 {
    use super::*;
    use chelabase_nats::{TenantAttachedEvent, TenantAttachedShard};
    use std::time::Duration;
    use tokio::time::Instant;

    /// The storage controller gives up on the hook after 10 s; it must answer before that.
    const HOOK_ANSWER_WITHIN: Duration = Duration::from_millis(9_500);

    async fn spawn_attach(neon: FakeNeon, locator: FakeLocator, events: FakeAttachEvents) -> Internal {
        spawn_with_app(
            neon,
            locator,
            &storcon(true).await,
            rules(Mode::SelfHosted, false),
            FakeCache::default(),
            "http://127.0.0.1:9",
            AppSetup { attach_events: events, ..AppSetup::default() },
        )
        .await
    }

    fn event(tenant: &str, shards: &[(u64, u8)]) -> TenantAttachedEvent {
        TenantAttachedEvent {
            tenant_id: tenant.to_string(),
            shards: shards
                .iter()
                .map(|&(node_id, shard_number)| TenantAttachedShard { node_id, shard_number })
                .collect(),
            stripe_size: None,
            preferred_az: None,
        }
    }

    // Whatever the tenant's endpoints: a tenant no endpoint names (a fresh provision's) is
    // published too; that is the event the worker waits for.
    #[tokio::test]
    async fn each_call_publishes_one_event_with_the_bodys_tenant_and_shards() {
        let events = FakeAttachEvents::default();
        let app = spawn_attach(FakeNeon::default(), FakeLocator::default(), events.clone()).await;
        assert_eq!(app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await, 200);
        assert_eq!(events.published(), [event(TENANT, &[(1, 0)])]);

        let sharded = json!({ "tenant_id": TENANT, "preferred_az": "az-1", "stripe_size": 32768,
                              "shards": [{ "node_id": 3, "shard_number": 0 },
                                         { "node_id": 4, "shard_number": 1 }] })
        .to_string();
        assert_eq!(app.put(ATTACH, Some(STORCON_TOKEN), &sharded).await, 200);
        let mut want = event(TENANT, &[(3, 0), (4, 1)]);
        want.stripe_size = Some(32768);
        want.preferred_az = Some("az-1".into());
        assert_eq!(events.published(), [event(TENANT, &[(1, 0)]), want.clone()]);

        // A retry of the same notify publishes again (the stream keeps the last per tenant).
        assert_eq!(app.put(ATTACH, Some(STORCON_TOKEN), &sharded).await, 200);
        assert_eq!(events.published().len(), 3);
        assert_eq!(events.published()[2], want);
    }

    // A3: only `tenant_id` is required; the rest defaults.
    #[tokio::test]
    async fn a_body_with_only_the_tenant_still_publishes() {
        let events = FakeAttachEvents::default();
        let app = spawn_attach(FakeNeon::default(), FakeLocator::default(), events.clone()).await;
        let body = json!({ "tenant_id": TENANT }).to_string();
        assert_eq!(app.put(ATTACH, Some(STORCON_TOKEN), &body).await, 200);
        assert_eq!(events.published(), [event(TENANT, &[])]);
    }

    #[tokio::test]
    async fn a_known_tenant_is_published_and_its_computes_reconfigured() {
        let neon = FakeNeon::default();
        neon.add("ep-a", TENANT, TIMELINE);
        let (addr, seen) = compute(200).await;
        let locator = FakeLocator::default();
        locator.running("ep-a", "172.20.0.9", Some(addr));
        let events = FakeAttachEvents::default();
        let app = spawn_attach(neon, locator, events.clone()).await;
        assert_eq!(app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await, 200);
        assert_eq!(events.published(), [event(TENANT, &[(1, 0)])]);
        assert_eq!(seen.0.lock().unwrap().len(), 1);
    }

    // The hook answers only once JetStream has acked the event: a 200 means it is recorded.
    #[tokio::test]
    async fn the_hook_answers_only_after_the_publish_is_acked() {
        let events = FakeAttachEvents::with(AttachPublish::Gated);
        let app = Arc::new(spawn_attach(FakeNeon::default(), FakeLocator::default(), events.clone()).await);
        let call = {
            let app = app.clone();
            tokio::spawn(async move { app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await })
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while events.published().is_empty() {
            assert!(std::time::Instant::now() < deadline, "the hook never published");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!call.is_finished(), "the hook answered before the ack");
        events.gate.notify_one();
        assert_eq!(call.await.unwrap(), 200);
    }

    // A failed publish is retried by the storage controller: 503, even with nothing to
    // reconfigure.
    #[tokio::test]
    async fn a_failed_publish_is_503_even_with_no_endpoints() {
        let events = FakeAttachEvents::with(AttachPublish::Fails);
        let app = spawn_attach(FakeNeon::default(), FakeLocator::default(), events.clone()).await;
        assert_eq!(app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await, 503);
        assert_eq!(events.published().len(), 1, "it was tried once");
    }

    #[tokio::test]
    async fn a_failed_publish_still_reconfigures_the_tenants_computes() {
        let neon = FakeNeon::default();
        neon.add("ep-a", TENANT, TIMELINE);
        let (addr, seen) = compute(200).await;
        let locator = FakeLocator::default();
        locator.running("ep-a", "172.20.0.9", Some(addr));
        let events = FakeAttachEvents::with(AttachPublish::Fails);
        let app = spawn_attach(neon, locator, events.clone()).await;
        assert_eq!(app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await, 503);
        assert_eq!(seen.0.lock().unwrap().len(), 1, "the compute was still reconfigured");
    }

    /// A compute_ctl stand-in that answers at once and records when each `/configure` arrived.
    async fn quick_compute() -> (SocketAddr, Arc<Mutex<Vec<Instant>>>) {
        let at = Arc::new(Mutex::new(Vec::new()));
        let seen = at.clone();
        let app = Router::new()
            .route(
                "/status",
                get(|| async {
                    axum::Json(json!({ "status": "running", "capabilities": ["storage_token_refresh"] }))
                }),
            )
            .route(
                "/configure",
                axum::routing::post(move || {
                    let seen = seen.clone();
                    async move {
                        seen.lock().unwrap().push(Instant::now());
                        (AxumStatus::OK, "{}")
                    }
                }),
            );
        (serve(app).await, at)
    }

    // A2: a publish that never acks (NATS hung) is cut at the hook's one deadline, taken at hook
    // entry: 503 before the storage controller's 10 s, and the reconfigures ran meanwhile rather
    // than after it. On the real clock: the hook reaches the stand-in over loopback sockets, and a
    // paused clock's auto-advance fires pending timers while a connect is still in flight (see
    // `f2::notify`).
    #[tokio::test]
    async fn a_hung_publish_answers_503_in_time_without_holding_up_the_reconfigures() {
        let neon = FakeNeon::default();
        neon.add("ep-a", TENANT, TIMELINE);
        let (addr, configured) = quick_compute().await;
        let locator = FakeLocator::default();
        locator.running("ep-a", "172.20.0.9", Some(addr));
        let events = FakeAttachEvents::with(AttachPublish::Hangs);
        let app = spawn_attach(neon, locator, events.clone()).await;
        let started = Instant::now();
        let code = tokio::time::timeout(
            Duration::from_secs(12),
            app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)),
        )
        .await
        .ok();
        let took = started.elapsed();
        assert_eq!(code, Some(503), "answered after {took:?}");
        assert!(took < HOOK_ANSWER_WITHIN, "the hook took {took:?}");
        assert_eq!(events.published().len(), 1);
        let configured = configured.lock().unwrap().clone();
        assert_eq!(configured.len(), 1, "the compute was reconfigured");
        let after = configured[0].duration_since(started);
        assert!(
            after < Duration::from_secs(8),
            "the reconfigure waited for the publish: it came {after:?} in"
        );
    }

    // A3: a `tenant_id` that isn't a Neon id is never published (it would name another subject,
    // or none): 503 like any unusable body.
    #[tokio::test]
    async fn an_invalid_tenant_id_is_503_and_publishes_nothing() {
        let events = FakeAttachEvents::default();
        let app = spawn_attach(FakeNeon::default(), FakeLocator::default(), events.clone()).await;
        for tenant in [
            json!("11111111111111111111111111111111".to_uppercase().replace('1', "A")),
            json!("1111111111111111111111111111111"),
            json!("111111111111111111111111111111111"),
            json!("1111111111111111111111111111111."),
            json!("111111111111111111111111111111>"),
            json!("*"),
            json!(""),
            json!(5),
            json!(null),
        ] {
            let body = json!({ "tenant_id": tenant, "shards": [{ "node_id": 1, "shard_number": 0 }] })
                .to_string();
            assert_eq!(app.put(ATTACH, Some(STORCON_TOKEN), &body).await, 503, "{tenant}");
        }
        assert!(events.published().is_empty(), "{:?}", events.published());
    }

    // A4: the event doesn't depend on the metadata: a failed endpoint lookup still publishes, and
    // the hook answers 503 so the reconfigure is retried.
    #[tokio::test]
    async fn a_failed_metadata_lookup_still_publishes_then_answers_503() {
        let neon = FakeNeon::default();
        neon.break_it();
        let events = FakeAttachEvents::default();
        let app = spawn_attach(neon, FakeLocator::default(), events.clone()).await;
        assert_eq!(app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await, 503);
        assert_eq!(events.published(), [event(TENANT, &[(1, 0)])]);
    }

    #[tokio::test]
    async fn the_token_check_still_comes_before_the_body_and_the_publish() {
        let events = FakeAttachEvents::default();
        let app = spawn_attach(FakeNeon::default(), FakeLocator::default(), events.clone()).await;
        assert_eq!(app.put(ATTACH, None, &attach(TENANT)).await, 401);
        assert_eq!(app.put(ATTACH, Some(PROXY_TOKEN), &attach(TENANT)).await, 401);
        assert_eq!(app.put(ATTACH, Some("wrong"), "not json").await, 401);
        assert!(events.published().is_empty());
    }

    // notify-safekeepers is unchanged: it publishes nothing.
    #[tokio::test]
    async fn notify_safekeepers_publishes_no_attach_event() {
        let events = FakeAttachEvents::default();
        let app = spawn_attach(FakeNeon::default(), FakeLocator::default(), events.clone()).await;
        let body = json!({ "tenant_id": TENANT, "timeline_id": TIMELINE, "generation": 3,
                           "safekeepers": [{ "id": 1, "hostname": "sk1" }] })
        .to_string();
        assert_eq!(app.put(SAFEKEEPERS, Some(STORCON_TOKEN), &body).await, 200);
        assert!(events.published().is_empty());
    }
}

// B2 (m3 audit): every orchestrator:8083 probe of `scripts/compute-network-e2e.sh` hits a route
// that exists and refuses a caller without a credential (401/403, never 404), and the sign-up
// gate's two POST routes are among them.
#[tokio::test]
async fn signup_routes_are_probed_by_the_network_e2e() {
    let script = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/compute-network-e2e.sh"),
    )
    .unwrap();
    let vars = [
        ("$zero32", "00000000000000000000000000000000"),
        ("$sha", "0000000000000000000000000000000000000000000000000000000000000000"),
        ("$uuid", "00000000-0000-4000-8000-000000000000"),
    ];
    let mut probes = Vec::new();
    for line in script.lines().map(str::trim) {
        let Some(rest) = line.strip_prefix("refused ") else { continue };
        let mut words = rest.split_whitespace();
        let (Some(method), Some(url)) = (words.next(), words.next()) else { continue };
        let mut url = url.trim_matches('"').to_string();
        let Some(path) = url.strip_prefix("http://orchestrator:8083") else { continue };
        let mut path = path.to_string();
        for (k, v) in vars {
            path = path.replace(k, v);
        }
        url = path;
        probes.push((method.to_string(), url));
    }
    let probed: Vec<(&str, &str)> = probes.iter().map(|(m, p)| (m.as_str(), p.split('?').next().unwrap())).collect();
    for route in [("POST", "/internal/signup/check"), ("POST", "/internal/signup/claimed")] {
        assert!(probed.contains(&route), "the e2e doesn't probe {route:?}: {probed:?}");
    }
    let w = app_world();
    let a = spawn_app(&w, Some(SIGNING_KEY), None).await;
    let http = reqwest::Client::new();
    for (method, path) in &probes {
        let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let mut req = http.request(m.clone(), format!("{}{path}", a.app.base));
        if m != reqwest::Method::GET {
            req = req.header("content-type", "application/json").body("{}");
        }
        let status = req.send().await.unwrap().status().as_u16();
        assert!(status == 401 || status == 403, "{method} {path} without a credential answers {status}");
    }
}

// N1 (m3 audit): Zitadel answering something that can't be read as the claim's user is not
// "gone": no replacement, 503, the nonce stays live and the claim unchanged.
#[tokio::test]
async fn signup_unreadable_zitadel_answer_keeps_the_claim() {
    let neon = fresh_install();
    let mock = MockZitadel::start().await;
    let first = mock.add_user_in(SETUP_ORG, "first@x.com", "pw", true);
    let new = mock.add_user_in(SETUP_ORG, "new@x.com", "pw", true);
    let app = setup_app(neon.clone(), Mode::SelfHosted, Some(SETUP_CODE), Some(&mock)).await;
    let n = right_code_nonce(&app, SETUP_ORG).await;
    assert_eq!(claim(&app, SETUP_ORG, &first, &n).await, (200, json!({"bound": true})));
    let n = right_code_nonce(&app, SETUP_ORG).await;
    mock.fail(&format!("GET /v2/users/{first}"), 200, "Odd.Answer", 1);
    let (status, body) = claim(&app, SETUP_ORG, &new, &n).await;
    assert_eq!((status, error_code(&body)), (503, "unavailable"), "{body}");
    assert_eq!(neon.setup_claim_of(SETUP_ORG).as_deref(), Some(first.as_str()));
    assert!(neon.live_nonces(SETUP_ORG).contains(&nonce_sha(&n)));
}
