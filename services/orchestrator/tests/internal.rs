//! The internal listener (proxy, compute_ctl and storage-controller routes) with fake
//! metadata, a fake compute locator, and in-process stand-ins for the storage controller and
//! computes.

use axum::Router;
use axum::http::StatusCode as AxumStatus;
use axum::routing::get;
use cheladb_metadata::{
    BranchConnectionRow, BranchStatus, DataApiSettings, DataKeyLookup, MetadataError,
    NeonDatabase, NeonEndpoint, NeonRole,
};
use cheladb_secrets::{Purpose, SecretKey, seal_credential};
use cheladb_types::BranchId;
use cheladb_types::data_key::DataKeyKind;
use cheladb_neon::jwt::generate_keypair;
use cheladb_neon::{ComputeLocator, ComputeSigner, StorconClient};
use orchestrator::internal::{InternalState, NeonMetadataPort, build_internal_router};
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

#[tokio::test]
async fn a_tenant_without_endpoints_is_answered_at_once() {
    let app = spawn_internal(
        FakeNeon::default(),
        FakeLocator::default(),
        &storcon(true).await,
    )
    .await;
    assert_eq!(
        app.put(ATTACH, Some(STORCON_TOKEN), &attach(TENANT)).await,
        200
    );
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
        DataKeyLookup { key_id: KEY_ID, branch_id: branch, kind: DataKeyKind::Secret, role: "reporting".into() },
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
        json!({ "key_id": KEY_ID, "branch_id": branch, "kind": "secret", "role": "reporting" })
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
            default_schema: "app".into(),
        },
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
                "default_schema": "app",
            },
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
