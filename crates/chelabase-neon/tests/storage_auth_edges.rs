//! RT1 T1 edge cases beyond the RED set: a bearer survives a later timeout change, a Valkey URL
//! with credentials or a half-set ACL user is refused, and storage tokens never expire nor depend
//! on when they are signed.

use axum::Router;
use axum::extract::{Request, State};
use axum::response::{IntoResponse, Response};
use chelabase_neon::StorconClient;
use chelabase_neon::jwt::{StorageScope, generate_keypair};
use chelabase_neon::proxy_cache::ProxyCache;
use std::sync::{Arc, Mutex};
use std::time::Duration;

async fn recording_server() -> (String, Arc<Mutex<Vec<Option<String>>>>) {
    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    async fn answer(State(seen): State<Arc<Mutex<Vec<Option<String>>>>>, req: Request) -> Response {
        let auth = req
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        seen.lock().unwrap().push(auth);
        ([("content-type", "application/json")], "[]").into_response()
    }
    let app = Router::new().fallback(answer).with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), seen)
}

#[tokio::test]
async fn the_bearer_survives_a_timeout_set_after_it() {
    let (base, seen) = recording_server().await;
    StorconClient::new(&base, 0)
        .with_token("tok-1".into())
        .with_timeout(Duration::from_secs(3))
        .active_safekeepers()
        .await
        .unwrap();
    assert_eq!(seen.lock().unwrap()[0].as_deref(), Some("Bearer tok-1"));
}

#[test]
fn a_valkey_url_with_credentials_or_a_half_set_user_is_refused() {
    assert!(ProxyCache::with_auth("redis://u:p@valkey:6379", "control", "pw").is_err());
    let only_user = |n: &str| (n == "CHELABASE_VALKEY_USER").then(|| "control".to_string());
    assert!(ProxyCache::from_lookup("redis://valkey:6379", only_user).is_err());
    let only_file =
        |n: &str| (n == "CHELABASE_VALKEY_PASSWORD_FILE").then(|| "/nonexistent".to_string());
    assert!(ProxyCache::from_lookup("redis://valkey:6379", only_file).is_err());
    assert!(ProxyCache::from_lookup("redis://valkey:6379", |_| None).is_ok());
}

#[test]
fn a_storage_token_is_the_same_whenever_it_is_signed() {
    let (pem, x) = generate_keypair();
    let s = chelabase_neon::ComputeSigner::new(&pem, &x).unwrap();
    let a = s.storage_token(&StorageScope::SafekeeperData).unwrap();
    std::thread::sleep(Duration::from_millis(1100));
    let b = s.storage_token(&StorageScope::SafekeeperData).unwrap();
    assert_eq!(a, b, "no iat/exp: a running service's token never changes");
    assert_ne!(a, s.storage_token(&StorageScope::Admin).unwrap());
}

#[test]
fn a_metrics_token_changes_with_its_mint_time_and_a_service_token_does_not() {
    // S17: "re-mint to rotate" only works because the metrics token carries `iat`.
    let (pem, x) = generate_keypair();
    let s = chelabase_neon::ComputeSigner::new(&pem, &x).unwrap();
    let a = s.metrics_token(1_800_000_000).unwrap();
    assert_ne!(a, s.metrics_token(1_800_000_001).unwrap());
    assert_eq!(a, s.metrics_token(1_800_000_000).unwrap());
    // Every service scope stays clockless and distinct from the metrics token.
    for scope in [
        StorageScope::PageServerApi,
        StorageScope::SafekeeperData,
        StorageScope::GenerationsApi,
        StorageScope::Admin,
        StorageScope::Infra,
    ] {
        assert_eq!(s.storage_token(&scope).unwrap(), s.storage_token(&scope).unwrap());
        assert_ne!(s.storage_token(&scope).unwrap(), a);
    }
}

#[test]
fn an_expiring_tenant_token_is_new_after_a_refresh_interval() {
    let (pem, x) = generate_keypair();
    let s = chelabase_neon::ComputeSigner::new(&pem, &x).unwrap();
    let t = chelabase_neon::NeonId::from(uuid::Uuid::from_u128(7));
    let a = s.tenant_token(&t, 1_800_000_000, true).unwrap();
    assert_ne!(a, s.tenant_token(&t, 1_800_003_600, true).unwrap());
    assert_eq!(a, s.tenant_token(&t, 1_800_000_000, true).unwrap());
    // Without `expiring` the clock is ignored: the old deterministic token.
    assert_eq!(
        s.tenant_token(&t, 1_800_000_000, false).unwrap(),
        s.tenant_token(&t, 1_800_003_600, false).unwrap()
    );
}

/// The status of `method url` with an optional bearer.
async fn status_of(http: &reqwest::Client, method: reqwest::Method, url: &str, bearer: Option<&str>) -> u16 {
    let mut req = http.request(method, url).timeout(Duration::from_secs(10));
    if let Some(t) = bearer {
        req = req.bearer_auth(t);
    }
    req.send().await.unwrap_or_else(|e| panic!("{url}: {e}")).status().as_u16()
}

/// The stack the live test looks at: in a stack's runner the Compose project
/// (`CHELABASE_COMPOSE_PROJECT`: `chelabase` is main's, `chelabase-<stack>` any other's), else
/// `CHELABASE_STACK`, else main.
fn live_stack() -> String {
    let set = |name: &str| {
        std::env::var(name)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    match set("CHELABASE_COMPOSE_PROJECT") {
        Some(p) if p == "chelabase" => chelabase_neon::DEFAULT_STACK.to_string(),
        Some(p) => p.strip_prefix("chelabase-").unwrap_or(&p).to_string(),
        None => set("CHELABASE_STACK").unwrap_or_else(|| chelabase_neon::DEFAULT_STACK.to_string()),
    }
}

/// compute_ctl's external address (`:3080`) of a running compute of the stack: its compute
/// list, then the first one with a control address on `CHELABASE_COMPUTE_NETWORK` (the network
/// the worker reaches compute_ctl on). No running compute is a failure that says so.
async fn running_compute_ctl() -> std::net::SocketAddr {
    use chelabase_neon::{ComputeBackend, ComputeLocator, ComputeSettings, ComputeStatus, DockerBackend, DockerSettings};
    let stack = live_stack();
    let network = std::env::var("CHELABASE_COMPUTE_NETWORK")
        .ok()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| "chelabase_compute".into());
    let computes = DockerBackend::connect(ComputeSettings {
        image: String::new(),
        control_plane_uri: String::new(),
        stack: stack.clone(),
        ca: None,
    }, DockerSettings {
        network: network.clone(),
        control_network: network.clone(),
    })
    .expect("a Docker client");
    let endpoints = computes.list().await.expect("listing the stack's computes");
    for ep in &endpoints {
        if computes.status(ep).await.ok().flatten() != Some(ComputeStatus::Running) {
            continue;
        }
        if let Ok(Some(addr)) = computes.control_addr(ep).await {
            return addr;
        }
    }
    panic!(
        "no running compute of stack {stack:?} with an address on {network:?} (computes: {endpoints:?}); \
         provision a database first"
    );
}

/// U2/U4/D3 (live, the workbench stack): the one `metrics` token (`<CHELABASE_COMPUTE_JWT_DIR>/
/// storage-tokens/metrics.token`, which the runner mounts read-only) reads the pageserver's, a
/// safekeeper's, the storage controller's and a compute's metrics, and nothing else: no profile,
/// no tenant or admin route, no compute_ctl status or configure. Without a token every one of
/// those `/metrics` routes answers 401. The compute is any running compute of the stack, found
/// the way the worker finds them (the stack's compute list, then its compute_ctl address).
///
/// ```sh
/// just workbench test -p chelabase-neon --test storage_auth_edges -- --run-ignored only metrics_scope
/// ```
#[tokio::test]
#[ignore = "needs the workbench stack with a running compute"]
async fn metrics_scope_reads_metrics_only() {
    use reqwest::Method;
    const GET: Method = Method::GET;
    const POST: Method = Method::POST;
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dir = std::path::PathBuf::from(
        std::env::var("CHELABASE_COMPUTE_JWT_DIR").unwrap_or_else(|_| ".chelabase/neon".into()),
    );
    let dir = if dir.is_relative() { root.join(dir) } else { dir };
    let token = std::fs::read_to_string(dir.join("storage-tokens/metrics.token"))
        .expect("metrics.token (the workbench writes it; the runner mounts it)");
    let token = token.trim();
    let compute = running_compute_ctl().await;
    let zero = "00000000000000000000000000000000";
    let http = reqwest::Client::new();

    // (base, metrics route, routes the metrics token must not open)
    let servers: Vec<(String, Vec<(reqwest::Method, String)>)> = vec![
        (
            "http://pageserver:9898".into(),
            vec![(GET, "/profile/cpu?seconds=0".into()), (GET, "/v1/tenant".into()), (GET, "/v1/utilization".into())],
        ),
        (
            "http://safekeeper1:7676".into(),
            vec![
                (GET, "/profile/heap".into()),
                (GET, format!("/v1/tenant/{zero}/timeline/{zero}")),
            ],
        ),
        (
            "http://storage-controller:1234".into(),
            vec![(GET, "/profile/cpu?seconds=0".into()), (GET, "/control/v1/node".into()), (GET, "/debug/v1/tenant".into())],
        ),
    ];
    for (base, refused) in &servers {
        let url = format!("{base}/metrics");
        assert_eq!(status_of(&http, GET, &url, None).await, 401, "{url} without a token");
        assert_eq!(status_of(&http, GET, &url, Some(token)).await, 200, "{url} with the metrics token");
        for (method, path) in refused {
            let url = format!("{base}{path}");
            assert_eq!(
                status_of(&http, method.clone(), &url, Some(token)).await,
                403,
                "{method} {url}: a metrics token opens metrics only"
            );
        }
    }

    // compute_ctl's external API (:3080): both metrics routes open, every other route is a 403.
    let base = format!("http://{compute}");
    for path in ["/metrics", "/autoscaling_metrics"] {
        let url = format!("{base}{path}");
        assert_eq!(status_of(&http, GET, &url, None).await, 401, "{url} without a token");
        assert_eq!(status_of(&http, GET, &url, Some(token)).await, 200, "{url} with the metrics token");
    }
    assert_eq!(status_of(&http, GET, &format!("{base}/status"), Some(token)).await, 403);
    assert_eq!(status_of(&http, POST, &format!("{base}/configure"), Some(token)).await, 403);
}
