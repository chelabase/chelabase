//! Against a real Docker daemon and the Neon compute image (the workbench stack pulls or builds it).
//!
//! ```sh
//! just workbench test -p chelabase-neon --test docker_backend_live -- --run-ignored only
//! ```
//!
//! `admin_sql_on_a_running_compute` also needs the stack up with a running compute (a database
//! created through the API or Studio, e.g. `just create-db-demo`). It uses the endpoint id
//! (`ep-…`, the host's first label) in `CHELABASE_LIVE_ENDPOINT` when set, else the first running
//! compute of the stack (sorted by endpoint id). It reads the stack's `CHELABASE_STACK` (default `main`), `CHELABASE_COMPUTE_NETWORK`
//! (default `chelabase_compute`) and `CHELABASE_CONTROL_PLANE_URI` (default
//! `http://orchestrator:8083`).

use chelabase_neon::{ComputeBackend, ComputeSettings, ComputeStatus, DockerBackend, DockerSettings};

fn compute_image() -> String {
    if let Ok(image) = std::env::var("COMPUTE_NODE_IMAGE") {
        return image;
    }
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/neon-images.sh");
    let out = std::process::Command::new(script)
        .output()
        .expect("running scripts/neon-images.sh");
    assert!(out.status.success(), "scripts/neon-images.sh failed");
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("COMPUTE_NODE_IMAGE=").map(str::to_string))
        .expect("COMPUTE_NODE_IMAGE in the script's output")
}

/// The variable `name`, or `default` when it's unset or blank.
fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn backend(network: &str, control_plane_uri: &str, stack: &str) -> DockerBackend {
    backend_with_control(network, network, control_plane_uri, stack)
}

/// `control_network` other than `network` makes `create` join the compute to it before start
/// (`connect_network`); the same name means there is nothing to join.
fn backend_with_control(
    network: &str,
    control_network: &str,
    control_plane_uri: &str,
    stack: &str,
) -> DockerBackend {
    DockerBackend::connect(ComputeSettings {
        image: compute_image(),
        control_plane_uri: control_plane_uri.to_string(),
        stack: stack.to_string(),
        ca: Some(std::sync::Arc::new(
            chelabase_neon::compute_tls::ComputeCa::generate(std::time::SystemTime::now()).unwrap(),
        )),
    }, DockerSettings {
        network: network.to_string(),
        control_network: control_network.to_string(),
    })
    .unwrap()
}

/// A scratch bridge network for the test, removed (with a retry-free best effort) by the
/// caller. Recreated when a previous run left it behind.
async fn scratch_network(docker: &bollard::Docker, name: &str) {
    let _ = docker.remove_network(name).await;
    docker
        .create_network(bollard::models::NetworkCreateRequest {
            name: name.to_string(),
            driver: Some("bridge".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "needs Docker and the compute image"]
async fn lifecycle() {
    let backend = backend("bridge", "http://127.0.0.1:9", "livetest");
    let ep = "ep-livetest0001";
    backend.delete(ep).await.unwrap();

    backend.create(ep, "not-a-real-token").await.unwrap();
    assert!(backend.list().await.unwrap().iter().any(|id| id == ep));
    assert_eq!(
        backend.status(ep).await.unwrap(),
        Some(ComputeStatus::Running)
    );

    // The name is taken: the old container is replaced.
    backend.create(ep, "another-token").await.unwrap();
    assert_eq!(
        backend.status(ep).await.unwrap(),
        Some(ComputeStatus::Running)
    );

    backend.delete(ep).await.unwrap();
    assert_eq!(backend.status(ep).await.unwrap(), None);
    backend.delete(ep).await.unwrap();
}

/// T8 m4: with its own control network, `create` joins the compute to that network too
/// (`connect_network`), so the compute is on both before it starts.
#[tokio::test]
#[ignore = "needs Docker and the compute image"]
async fn create_joins_the_control_network() {
    let docker = bollard::Docker::connect_with_local_defaults().unwrap();
    let (data, control) = ("chelabase_livetest_data", "chelabase_livetest_control");
    scratch_network(&docker, data).await;
    scratch_network(&docker, control).await;
    let backend = backend_with_control(data, control, "http://127.0.0.1:9", "livetest-ctl");
    let ep = "ep-livetestctl1";
    backend.delete(ep).await.unwrap();

    backend.create(ep, "not-a-real-token").await.unwrap();
    assert_eq!(backend.status(ep).await.unwrap(), Some(ComputeStatus::Running));
    let inspect = docker
        .inspect_container(
            &chelabase_neon::compute::container_name("livetest-ctl", ep),
            None,
        )
        .await
        .unwrap();
    let networks: Vec<String> = inspect
        .network_settings
        .and_then(|n| n.networks)
        .map(|n| n.into_keys().collect())
        .unwrap_or_default();
    assert!(networks.iter().any(|n| n == data), "on its data network: {networks:?}");
    assert!(networks.iter().any(|n| n == control), "on the control network: {networks:?}");

    backend.delete(ep).await.unwrap();
    let _ = docker.remove_network(data).await;
    let _ = docker.remove_network(control).await;
}

/// `CHELABASE_LIVE_ENDPOINT` when set; else the first (sorted) running compute of `stack`.
async fn running_endpoint(stack: &str) -> String {
    if let Some(ep) = std::env::var("CHELABASE_LIVE_ENDPOINT")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        return ep;
    }
    let docker = bollard::Docker::connect_with_local_defaults().unwrap();
    let mut filters = chelabase_neon::compute::list_filters(stack);
    filters.insert("status".to_string(), vec!["running".to_string()]);
    let containers = docker
        .list_containers(Some(bollard::query_parameters::ListContainersOptions {
            filters: Some(filters),
            ..Default::default()
        }))
        .await
        .unwrap();
    let mut endpoints: Vec<String> = containers
        .into_iter()
        .filter_map(|c| c.labels?.remove(chelabase_neon::compute::ENDPOINT_LABEL))
        .collect();
    endpoints.sort();
    endpoints.into_iter().next().unwrap_or_else(|| {
        panic!(
            "no running compute on stack {stack}: create a database first, e.g. `just create-db-demo`"
        )
    })
}

#[tokio::test]
#[ignore = "needs the workbench stack with a running compute (a database created through the API or Studio); CHELABASE_LIVE_ENDPOINT picks the endpoint id, else the stack's first running compute is used"]
async fn admin_sql_on_a_running_compute() {
    let stack = env_or("CHELABASE_STACK", "main");
    let ep = running_endpoint(&stack).await;
    let backend = backend(
        &env_or("CHELABASE_COMPUTE_NETWORK", "chelabase_compute"),
        &env_or("CHELABASE_CONTROL_PLANE_URI", "http://orchestrator:8083"),
        &stack,
    );

    let out = backend
        .admin_sql(&ep, "postgres", "SELECT current_user")
        .await
        .unwrap();
    assert_eq!(out.trim(), "cloud_admin");
    let out = backend
        .admin_sql(
            &ep,
            "postgres",
            "SELECT rolsuper FROM pg_roles WHERE rolname = current_user",
        )
        .await
        .unwrap();
    assert_eq!(out.trim(), "t");

    backend
        .admin_sql(
            &ep,
            "postgres",
            "DO $$ BEGIN\n\
             IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'chelabase_live_probe') THEN\n\
             CREATE ROLE chelabase_live_probe NOLOGIN;\n\
             END IF;\n\
             END $$",
        )
        .await
        .unwrap();
    backend
        .admin_sql(&ep, "postgres", "DROP ROLE chelabase_live_probe")
        .await
        .unwrap();

    let err = backend
        .admin_sql(&ep, "postgres", "SELEC 1")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("syntax error"), "{err}");

    let err = backend.admin_sql(&ep, "a=b", "SELECT 1").await.unwrap_err();
    assert!(err.to_string().contains("a=b"), "{err}");
}
