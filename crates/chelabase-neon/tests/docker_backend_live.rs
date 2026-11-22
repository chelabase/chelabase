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
//! (default `chelabase_compute-internal`) and `CHELABASE_CONTROL_PLANE_URI` (default
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
        legacy_networks: Vec::new(),
        egress_dns: None,
    })
    .unwrap()
}

/// A scratch bridge network for the test, internal with isolated gateways like the stack's
/// (a compute is created only on such a network), removed (with a retry-free best effort) by
/// the caller. Recreated when a previous run left it behind.
async fn scratch_network(docker: &bollard::Docker, name: &str) {
    let _ = docker.remove_network(name).await;
    docker
        .create_network(bollard::models::NetworkCreateRequest {
            name: name.to_string(),
            driver: Some("bridge".to_string()),
            internal: Some(true),
            options: Some(
                [
                    (chelabase_neon::backend::GATEWAY_MODE_IPV4.to_string(), "isolated".to_string()),
                    (chelabase_neon::backend::GATEWAY_MODE_IPV6.to_string(), "isolated".to_string()),
                ]
                .into(),
            ),
            ..Default::default()
        })
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "needs Docker and the compute image"]
async fn lifecycle() {
    let docker = bollard::Docker::connect_with_local_defaults().unwrap();
    let network = "chelabase_livetest_lifecycle";
    scratch_network(&docker, network).await;
    let backend = backend(network, "http://127.0.0.1:9", "livetest");
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
    let _ = docker.remove_network(network).await;
}

/// compute-network-reach m2: on a network that isn't internal the create is refused and no
/// container is left; on an internal, isolated one the compute runs with no capability.
#[tokio::test]
#[ignore = "needs Docker and the compute image"]
async fn create_refuses_an_ordinary_bridge_and_drops_every_capability() {
    let docker = bollard::Docker::connect_with_local_defaults().unwrap();
    let (plain, internal) = ("chelabase_livetest_plain", "chelabase_livetest_internal");
    let _ = docker.remove_network(plain).await;
    docker
        .create_network(bollard::models::NetworkCreateRequest {
            name: plain.to_string(),
            driver: Some("bridge".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    scratch_network(&docker, internal).await;
    let ep = "ep-livetestcap1";

    let refused = backend(plain, "http://127.0.0.1:9", "livetest-cap");
    let err = refused.create(ep, "not-a-real-token").await.unwrap_err();
    assert!(format!("{err:#}").contains("is not internal"), "{err:#}");
    assert_eq!(refused.status(ep).await.unwrap(), None, "no container is left");

    let backend = backend(internal, "http://127.0.0.1:9", "livetest-cap");
    // `Running` is transient here: with no control plane behind the URL, `compute_ctl` exits
    // shortly after it starts. What holds is that the container started and how it was made.
    backend.create(ep, "not-a-real-token").await.unwrap();
    let inspect = docker
        .inspect_container(&chelabase_neon::compute::container_name("livetest-cap", ep), None)
        .await
        .unwrap();
    let state = inspect.state.clone().unwrap();
    assert!(
        state.started_at.as_deref().is_some_and(|t| !t.starts_with("0001-")),
        "the container was started: {state:?}"
    );
    let host = inspect.host_config.unwrap();
    assert_eq!(host.cap_drop, Some(vec!["ALL".to_string()]));
    assert_eq!(host.security_opt, Some(vec!["no-new-privileges:true".to_string()]));

    backend.delete(ep).await.unwrap();
    let _ = docker.remove_network(plain).await;
    let _ = docker.remove_network(internal).await;
}

/// A legacy network that a stopped container is still attached to is not empty: Docker's network
/// inspect lists only active endpoints, and the engine deletes a network that only stopped
/// containers reference. The worker's removal (only when `members` is empty) must keep it.
#[tokio::test]
#[ignore = "needs Docker and the compute image"]
async fn a_legacy_network_with_a_stopped_member_survives_removal() {
    use bollard::models::{ContainerCreateBody, HostConfig};
    use bollard::query_parameters::{
        CreateContainerOptions, RemoveContainerOptions, StartContainerOptions,
    };
    use chelabase_neon::backend::{LegacyNetwork, NetworkRemoval};
    use futures_util::TryStreamExt;

    let docker = bollard::Docker::connect_with_local_defaults().unwrap();
    let (legacy, container) = ("cnr-f2-legacy", "cnr-f2-stopped");
    let _ = docker
        .remove_container(container, Some(RemoveContainerOptions { force: true, ..Default::default() }))
        .await;
    let _ = docker.remove_network(legacy).await;
    docker
        .create_network(bollard::models::NetworkCreateRequest {
            name: legacy.to_string(),
            driver: Some("bridge".to_string()),
            ..Default::default()
        })
        .await
        .unwrap();
    docker
        .create_container(
            Some(CreateContainerOptions { name: Some(container.to_string()), ..Default::default() }),
            ContainerCreateBody {
                image: Some(compute_image()),
                entrypoint: Some(vec!["true".to_string()]),
                host_config: Some(HostConfig {
                    network_mode: Some(legacy.to_string()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    docker.start_container(container, None::<StartContainerOptions>).await.unwrap();
    // `true` exits at once; wait for it through the engine rather than a fixed delay.
    let _ = docker
        .wait_container(container, None::<bollard::query_parameters::WaitContainerOptions>)
        .try_collect::<Vec<_>>()
        .await;

    let backend = DockerBackend::connect(
        ComputeSettings {
            image: compute_image(),
            control_plane_uri: "http://127.0.0.1:9".to_string(),
            stack: "cnr-f2".to_string(),
            ca: None,
        },
        DockerSettings {
            network: "cnr-f2-unused".to_string(),
            control_network: "cnr-f2-unused".to_string(),
            legacy_networks: vec![legacy.to_string()],
            egress_dns: None,
        },
    )
    .unwrap();
    // What the worker does with each legacy network: remove it only when no member is left.
    let mut removed = false;
    let mut members_seen = Vec::new();
    for found in backend.legacy_networks().await {
        if let LegacyNetwork::Present { network, members } = found {
            members_seen = members.clone();
            if members.is_empty() {
                removed = backend.remove_network(&network).await.unwrap() == NetworkRemoval::Removed;
            }
        }
    }

    let survived = docker.inspect_network(legacy, None).await.is_ok();
    let _ = docker
        .remove_container(container, Some(RemoveContainerOptions { force: true, ..Default::default() }))
        .await;
    let _ = docker.remove_network(legacy).await;
    assert_eq!(members_seen, [container.to_string()], "the stopped container is a member");
    assert!(!removed && survived, "the network was removed while a stopped container used it");
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
        &env_or("CHELABASE_COMPUTE_NETWORK", "chelabase_compute-internal"),
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
