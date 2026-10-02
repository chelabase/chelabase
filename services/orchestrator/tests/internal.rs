//! The internal listener (proxy, compute_ctl and storage-controller routes) with fake
//! metadata, a fake compute locator, and in-process stand-ins for the storage controller and
//! computes.

use axum::Router;
use axum::http::StatusCode as AxumStatus;
use axum::routing::get;
use cheladb_metadata::{
    BranchConnectionRow, ServedDatabase, BranchStatus, DataApiSettings, DataKeyLookup, DatabaseOrigin,
    MetadataError, NeonDatabase, NeonEndpoint, NeonRole, RoleCopy, RoleLevel, RoleOrigin,
};
use cheladb_secrets::{Purpose, SecretKey, seal_credential};
use cheladb_types::BranchId;
use cheladb_types::data_key::DataKeyKind;
use cheladb_neon::jwt::generate_keypair;
use cheladb_neon::{ComputeLocator, ComputeSigner, StorconClient};
use cheladb_types::mode::Mode;
use cheladb_types::role_settings::RoleSettings;
use orchestrator::internal::{
    BoxFuture, CachePort, DdlPort, InternalState, NeonMetadataPort, build_internal_router,
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
const CONTROL_PLANE_URI: &str = "http://host.docker.internal:8083";

fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(s.as_bytes()))
}

type KeyEntry = ([u8; 32], DataKeyLookup);

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
        created_at: chrono::Utc::now(),
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
    async fn rename_copy(
        &mut self,
        old: &str,
        new: &str,
        resealed: Option<&str>,
    ) -> Result<bool, MetadataError> {
        if !self.state.roles.contains_key(old) {
            return Ok(false);
        }
        if self.state.roles.contains_key(new) {
            return Err(duplicate_key());
        }
        let mut row = self.state.roles.remove(old).unwrap();
        row.name = new.into();
        row.password_enc = resealed.map(str::to_string);
        self.state.roles.insert(new.into(), row);
        if self.state.branch.is_some() {
            for key in &mut self.state.keys {
                if key.2 == old {
                    key.2 = new.into();
                }
            }
        }
        self.wrote = true;
        Ok(true)
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
        });
        self.roles.lock().unwrap().push((
            id.into(),
            NeonRole {
                name: "app".into(),
                scram_verifier: format!("SCRAM-SHA-256$verifier-of-{id}"),
            },
        ));
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
}

#[derive(Clone, Default)]
struct FakeLocator {
    ips: Arc<Mutex<HashMap<String, IpAddr>>>,
    controls: Arc<Mutex<HashMap<String, SocketAddr>>>,
    broken: Arc<Mutex<bool>>,
    pg: Arc<Mutex<HashMap<String, SocketAddr>>>,
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
        anyhow::ensure!(!*self.broken.lock().unwrap(), "docker is down");
        Ok(self.ips.lock().unwrap().get(ep).copied())
    }

    async fn control_addr(&self, ep: &str) -> anyhow::Result<Option<SocketAddr>> {
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
                        { "host": "sk1", "port": 5454, "scheduling_policy": "Active" },
                        { "host": "sk2", "port": 5454, "scheduling_policy": "Pause" },
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
        (k == "CHELADB_PASSWORD_STRENGTH").then(|| if strength { "on" } else { "off" }.into())
    })
    .unwrap()
}

/// The proxy cache stand-in: records each `password_updated`; with `down`, it fails.
#[derive(Clone, Default)]
struct FakeCache {
    sent: Arc<Mutex<Vec<(String, String)>>>,
    down: Arc<Mutex<bool>>,
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
            anyhow::ensure!(!*self.down.lock().unwrap(), "valkey is down");
            Ok(1)
        })
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
    let (pem, x) = generate_keypair();
    let key = SecretKey::generate();
    let state = Arc::new(InternalState {
        metadata: neon,
        locator,
        storcon: StorconClient::new(storcon_base, 0),
        signer: ComputeSigner::new(&pem, &x).unwrap(),
        http: reqwest::Client::new(),
        proxy_token: PROXY_TOKEN.into(),
        storcon_token: STORCON_TOKEN.into(),
        data_keys_token: DATA_KEYS_TOKEN.into(),
        rest_token: REST_TOKEN.into(),
        secrets: key.clone(),
        roles,
        cache: Arc::new(cache),
        control_plane_uri: CONTROL_PLANE_URI.into(),
        rest: orchestrator::RestInternal {
            url: rest_url.into(),
            token: REST_TOKEN.into(),
            http: reqwest::Client::new(),
        },
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
    use axum::extract::State;
    let seen = Configured::default();
    let app = Router::new()
        .route(
            "/configure",
            axum::routing::post(
                move |State(seen): State<Configured>,
                      headers: axum::http::HeaderMap,
                      body: axum::Json<Value>| async move {
                    let auth = headers
                        .get("authorization")
                        .unwrap()
                        .to_str()
                        .unwrap()
                        .to_string();
                    seen.0.lock().unwrap().push((auth, body.0));
                    (AxumStatus::from_u16(status).unwrap(), "{}")
                },
            ),
        )
        .with_state(seen.clone());
    (serve(app).await, seen)
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

// The storage controller is shared by every worktree slot; the hook relay offers each hook to
// the slots in order and keeps the first answer that isn't 404. A slot that doesn't know the
// tenant must say so, or every hook would stop at slot 0.
#[tokio::test]
async fn hook_for_an_unknown_tenant_is_404() {
    let neon = FakeNeon::default();
    neon.add(
        "ep-other",
        "0000000000000000000000000000ffff",
        "0000000000000000000000000000eeee",
    );
    let app = spawn_internal(neon, FakeLocator::default(), &storcon(true).await).await;
    assert_eq!(
        app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await,
        404
    );
    let body = json!({ "tenant_id": TENANT, "timeline_id": TIMELINE, "generation": 3,
                       "safekeepers": [{ "id": 1, "hostname": "sk1" }] })
    .to_string();
    assert_eq!(app.put(SAFEKEEPERS, Some(STORCON_TOKEN), &body).await, 404);
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

fn sha(key: &str) -> [u8; 32] {
    cheladb_types::data_key::hash(key)
}

#[tokio::test]
async fn key_lookup_requires_the_data_keys_bearer() {
    let neon = FakeNeon::default();
    let branch = BranchId::new();
    neon.data_keys.lock().unwrap().push((
        sha("chk_sec_x"),
        DataKeyLookup { key_id: KEY_ID, branch_id: branch, kind: DataKeyKind::Secret, role: "reporting".into(), databases: None },
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
        json!({ "key_id": KEY_ID, "branch_id": branch, "kind": "secret", "role": "reporting", "databases": null })
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
            kind: DataKeyKind::Secret,
            role: "reporting".into(),
            databases: Some(vec!["web1".into(), "analytics".into()]),
        },
    ));
    let app = spawn_internal(neon, FakeLocator::default(), "http://127.0.0.1:9").await;
    let path = format!("/internal/data/keys/{}", hex::encode(sha("chk_sec_listed")));
    let (status, body) = app.get(&path, Some(DATA_KEYS_TOKEN)).await;
    assert_eq!(status, 200);
    assert_eq!(body["databases"], json!(["web1", "analytics"]));
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
            "host": "127.0.0.1",
            "port": 55432,
            "database": "web1",
            "user": "authenticator",
            "password": "auth-pw",
            "status": "ready",
            "settings": {
                "max_rows": 500,
                "statement_timeout_ms": 4000,
                "max_connections": 7,
            },
            "databases": [{ "name": "web1", "default_schema": "app" }],
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
            text: "the role name \"anon\" is reserved by Cheladb",
            check: |s| assert!(!s.roles.contains_key("anon")),
        },
        Rule {
            what: "a pg_ name",
            rules: sh,
            setup: none,
            body: roles_delta(vec![set("PG_mine", STRONG, SCRAM)]),
            status: 409,
            text: "the role name \"PG_mine\" is reserved by Cheladb",
            check: nothing,
        },
        Rule {
            what: "MD5 refused",
            rules: sh,
            setup: none,
            body: roles_delta(vec![set("app_rw", MD5, MD5)]),
            status: 409,
            text: "MD5 passwords can't be used through Cheladb; set a plain password",
            check: no_app_rw,
        },
        Rule {
            what: "not SCRAM",
            rules: sh,
            setup: none,
            body: roles_delta(vec![set("app_rw", STRONG, "plain-stored")]),
            status: 409,
            text: "this password format can't be used through Cheladb; set a plain password",
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
            text: "this branch has reached its limit of 2 login roles (CHELADB_BRANCH_ROLE_CAP)",
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
            text: "this branch has reached its limit of 0 login roles (CHELADB_BRANCH_ROLE_CAP)",
            check: no_app_rw,
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
            what: "PASSWORD NULL removes a copy",
            rules: sh,
            setup: two_sql_copies,
            body: roles_delta(vec![json!({ "op": "set", "name": "r1" })]),
            status: 200,
            text: "",
            check: |s| assert!(!s.roles.contains_key("r1") && s.roles.contains_key("r2")),
        },
        Rule {
            what: "a group role",
            rules: sh,
            setup: none,
            body: roles_delta(vec![json!({ "op": "set", "name": "grp" })]),
            status: 200,
            text: "",
            check: |s| {
                assert!(!s.roles.contains_key("grp"));
                assert_eq!(s.roles_version, 0);
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
            text: "the role name \"authenticator\" is reserved by Cheladb",
            check: |s| assert!(s.roles.contains_key("r1")),
        },
        Rule {
            what: "a reserved rename source",
            rules: sh,
            setup: none,
            body: roles_delta(vec![json!({ "op": "set", "name": "mine", "old_name": "cheladb_admin" })]),
            status: 409,
            text: "the role name \"cheladb_admin\" is reserved by Cheladb",
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
            text: "the role name \"authenticated\" is reserved by Cheladb",
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
            text: "this branch has reached its limit of 2 databases (CHELADB_BRANCH_DATABASE_CAP)",
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
        cheladb_secrets::seal_role_password(&app.key, &branch.to_string(), "api_r", "the-secret");
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
        cheladb_secrets::open_role_password(&app.key, &b, "api_r2", resealed).unwrap(),
        "the-secret"
    );
    assert!(cheladb_secrets::open_role_password(&app.key, &b, "api_r", resealed).is_err());
}

// A role made, renamed and given `PASSWORD NULL` in one transaction reaches the receiver as
// one op for `c` (old name `b`, password null) while neither name has a row: nothing to move
// or clear, so no row appears for either.
#[tokio::test]
async fn ddl_rename_of_an_unknown_role_with_null_password_leaves_no_row() {
    let (neon, _) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    let body = roles_delta(vec![json!({ "op": "set", "name": "c", "old_name": "b", "password": null })]);
    assert_eq!(app.ddl("ep-d", Some(COMPUTE_TOKEN), &body).await.0, 200);
    let state = neon.ddl_state();
    assert!(!state.roles.contains_key("b") && !state.roles.contains_key("c"), "{:?}", state.roles.keys());
}

fn rename(old: &str, new: &str) -> Value {
    json!({ "op": "set", "name": new, "old_name": old })
}

fn open(app: &Internal, branch: BranchId, name: &str, sealed: Option<&str>) -> String {
    let sealed = sealed.unwrap_or_else(|| panic!("{name} is revealable"));
    cheladb_secrets::open_role_password(&app.key, &branch.to_string(), name, sealed)
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
    let seal = |name: &str, pw: &str| cheladb_secrets::seal_role_password(&app.key, &b, name, pw);
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

// A swap of two roles that both back live keys is taken: each key follows its role.
#[tokio::test]
async fn ddl_swap_with_keys_on_both_roles() {
    let (neon, _) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    neon.edit(|s| {
        s.roles.insert("a".into(), copy("a", RoleOrigin::Sql, Some("SCRAM-SHA-256$a"), None));
        s.roles.insert("b".into(), copy("b", RoleOrigin::Sql, Some("SCRAM-SHA-256$b"), None));
        s.keys.push((uuid::Uuid::new_v4(), "key-a".into(), "a".into(), false));
        s.keys.push((uuid::Uuid::new_v4(), "key-b".into(), "b".into(), false));
    });
    let (status, _, text) = app
        .ddl("ep-d", Some(COMPUTE_TOKEN), &roles_delta(vec![rename("a", "b"), rename("b", "a")]))
        .await;
    assert_eq!(status, 200, "{text}");
    let state = neon.ddl_state();
    assert_eq!(state.roles["b"].scram_verifier.as_deref(), Some("SCRAM-SHA-256$a"));
    let role_of = |key: &str| state.keys.iter().find(|k| k.1 == key).unwrap().2.clone();
    assert_eq!((role_of("key-a"), role_of("key-b")), ("b".to_string(), "a".to_string()));
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

// C7: an explicit `"password": null` next to `old_name` is a rename that removes the copy.
#[tokio::test]
async fn ddl_rename_with_explicit_null_password_removes_the_copy() {
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
    assert!(!state.roles.contains_key("r2"));
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

// C4: a sealed password that won't open is dropped and the row is marked unreadable.
#[tokio::test]
async fn ddl_rename_marks_an_unopenable_password_unreadable() {
    let (neon, branch) = ddl_neon();
    let app = spawn_internal(neon.clone(), FakeLocator::default(), "http://127.0.0.1:9").await;
    // Sealed under another name's AAD, so it won't open for `api_r`.
    let sealed =
        cheladb_secrets::seal_role_password(&app.key, &branch.to_string(), "other", "the-secret");
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
        cheladb_neon::spec::ddl_url(CONTROL_PLANE_URI, "ep-a")
    );
}
