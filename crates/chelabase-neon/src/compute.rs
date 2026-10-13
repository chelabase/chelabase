//! One compute (Postgres plus compute_ctl) per endpoint, run as a Docker container.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};

use anyhow::Context;
use bollard::Docker;
use bollard::errors::Error as DockerError;
use bollard::models::{ContainerInspectResponse, ContainerStateStatusEnum};
use bollard::query_parameters::InspectContainerOptions;

/// Postgres inside a compute.
pub const POSTGRES_PORT: u16 = 5432;
/// compute_ctl's external HTTP API (`/configure`).
pub const COMPUTE_CTL_PORT: u16 = 3080;
/// compute_ctl presents this variable's value when it fetches its spec.
pub const COMPUTE_TOKEN_ENV: &str = "NEON_CONTROL_PLANE_TOKEN";
/// Container label naming the endpoint a compute belongs to.
pub const ENDPOINT_LABEL: &str = "chelabase.dev/endpoint-id";

/// Container label naming the stack a compute belongs to.
pub const STACK_LABEL: &str = "chelabase.dev/stack";

/// The stack a worker or orchestrator belongs to when `CHELABASE_STACK` is unset.
pub const DEFAULT_STACK: &str = "main";

/// The longest stack name accepted.
const MAX_STACK_LEN: usize = 40;

/// Trims a `CHELABASE_STACK` value and checks it is usable in a Docker container name:
/// `[a-z0-9][a-z0-9-]*`, at most 40 characters. A blank value is an error; callers map blank
/// or unset to [`DEFAULT_STACK`] first.
pub fn check_stack(raw: &str) -> anyhow::Result<String> {
    let stack = raw.trim();
    let valid = !stack.is_empty()
        && stack.len() <= MAX_STACK_LEN
        && stack.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && stack
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !valid {
        anyhow::bail!(
            "CHELABASE_STACK {raw:?} is not a valid stack name: use [a-z0-9][a-z0-9-]*, at most {MAX_STACK_LEN} characters"
        );
    }
    Ok(stack.to_string())
}

/// Container names are global on a Docker host; the prefix and the stack keep ours recognisable
/// and apart from other stacks'.
pub fn container_name(stack: &str, endpoint_id: &str) -> String {
    format!("chelabase-{stack}-compute-{endpoint_id}")
}

/// Docker list filters selecting only `stack`'s computes.
pub fn list_filters(stack: &str) -> std::collections::HashMap<String, Vec<String>> {
    [(
        "label".to_string(),
        vec![ENDPOINT_LABEL.to_string(), format!("{STACK_LABEL}={stack}")],
    )]
    .into()
}

/// compute_ctl's arguments. It pulls its spec from
/// `{control_plane_uri}/compute/api/v2/computes/{endpoint_id}/spec`, presenting
/// [`COMPUTE_TOKEN_ENV`].
pub fn compute_ctl_args(endpoint_id: &str, control_plane_uri: &str) -> Vec<String> {
    vec![
        "--pgbin=/usr/local/bin/postgres".to_string(),
        "--pgdata=/var/db/postgres/compute".to_string(),
        format!("--connstr=postgresql://cloud_admin@localhost:{POSTGRES_PORT}/postgres"),
        format!("--compute-id={endpoint_id}"),
        format!("--control-plane-uri={control_plane_uri}"),
        format!("--external-http-port={COMPUTE_CTL_PORT}"),
    ]
}

/// Where an endpoint's compute can be reached. Both lookups give `None` when the container is
/// missing or not running.
pub trait ComputeLocator: Send + Sync + 'static {
    /// The compute's IP on the compute network: where the proxy connects.
    fn address(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = anyhow::Result<Option<IpAddr>>> + Send;

    /// `<ip>:3080` on the compute network: where the stack reaches compute_ctl.
    fn control_addr(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = anyhow::Result<Option<SocketAddr>>> + Send;

    /// `<ip>:5432` on the compute network: where the stack's services (the data API) reach
    /// Postgres.
    fn postgres_addr(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = anyhow::Result<Option<SocketAddr>>> + Send;
}

fn running(inspect: &ContainerInspectResponse) -> bool {
    inspect.state.as_ref().and_then(|s| s.status) == Some(ContainerStateStatusEnum::RUNNING)
}

/// The container's IP on `network`, if it's running.
pub fn ip_on_network(inspect: &ContainerInspectResponse, network: &str) -> Option<IpAddr> {
    if !running(inspect) {
        return None;
    }
    inspect
        .network_settings
        .as_ref()?
        .networks
        .as_ref()?
        .get(network)?
        .ip_address
        .as_deref()?
        .parse()
        .ok()
}

/// compute_ctl's address (`<ip>:3080`) on `network`, if the container is running.
pub fn control_addr_on_network(
    inspect: &ContainerInspectResponse,
    network: &str,
) -> Option<SocketAddr> {
    Some(SocketAddr::new(ip_on_network(inspect, network)?, COMPUTE_CTL_PORT))
}

/// Postgres' address (`<ip>:5432`) on `network`, if the container is running.
pub fn postgres_addr_on_network(
    inspect: &ContainerInspectResponse,
    network: &str,
) -> Option<SocketAddr> {
    Some(SocketAddr::new(ip_on_network(inspect, network)?, POSTGRES_PORT))
}

/// [`ComputeLocator`] over the Docker Engine API.
pub struct DockerLocator {
    pub(crate) docker: Docker,
    network: String,
    stack: String,
}

impl DockerLocator {
    /// `DOCKER_HOST` or the local socket, like the worker. Nothing is contacted yet. Computes
    /// are the stack's own: named and looked up by `stack`, reached on `network`.
    pub fn connect(network: &str, stack: &str) -> anyhow::Result<Self> {
        Ok(Self {
            docker: Docker::connect_with_local_defaults()
                .context("configuring the Docker client")?,
            network: network.to_string(),
            stack: stack.to_string(),
        })
    }

    pub(crate) async fn inspect(
        &self,
        endpoint_id: &str,
    ) -> anyhow::Result<Option<ContainerInspectResponse>> {
        match self
            .docker
            .inspect_container(
                &container_name(&self.stack, endpoint_id),
                None::<InspectContainerOptions>,
            )
            .await
        {
            Ok(inspect) => Ok(Some(inspect)),
            Err(DockerError::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(None),
            Err(e) => {
                Err(anyhow::Error::new(e).context(format!("inspecting compute {endpoint_id}")))
            }
        }
    }
}

impl ComputeLocator for DockerLocator {
    async fn address(&self, endpoint_id: &str) -> anyhow::Result<Option<IpAddr>> {
        Ok(self
            .inspect(endpoint_id)
            .await?
            .and_then(|inspect| ip_on_network(&inspect, &self.network)))
    }

    async fn control_addr(&self, endpoint_id: &str) -> anyhow::Result<Option<SocketAddr>> {
        Ok(self
            .inspect(endpoint_id)
            .await?
            .and_then(|inspect| control_addr_on_network(&inspect, &self.network)))
    }

    async fn postgres_addr(&self, endpoint_id: &str) -> anyhow::Result<Option<SocketAddr>> {
        Ok(self
            .inspect(endpoint_id)
            .await?
            .and_then(|inspect| postgres_addr_on_network(&inspect, &self.network)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containers_are_named_after_their_stack_and_endpoint() {
        assert_eq!(
            container_name("main", "ep-abc123"),
            "chelabase-main-compute-ep-abc123"
        );
        assert_eq!(
            container_name("feat-x", "ep-abc123"),
            "chelabase-feat-x-compute-ep-abc123"
        );
    }

    // T1 review I1: one check for the stack name, in every service that names computes.
    #[test]
    fn stack_names_are_trimmed_and_checked() {
        assert_eq!(check_stack("feat-x").unwrap(), "feat-x");
        assert_eq!(check_stack("  feat-x\n").unwrap(), "feat-x");
        assert_eq!(check_stack("main").unwrap(), "main");
        assert_eq!(check_stack("t3").unwrap(), "t3");
        assert_eq!(check_stack(&"a".repeat(40)).unwrap(), "a".repeat(40));
        let long = "a".repeat(41);
        for bad in ["", "  ", "feat/x", "Feat", "-x", "a b", "a_b", "é", long.as_str()] {
            let err = check_stack(bad).unwrap_err().to_string();
            assert!(err.contains("CHELABASE_STACK"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn listing_selects_only_this_stacks_computes() {
        let filters = list_filters("feat-x");
        assert_eq!(filters.len(), 1, "{filters:?}");
        let mut labels = filters["label"].clone();
        labels.sort();
        assert_eq!(
            labels,
            ["chelabase.dev/endpoint-id", "chelabase.dev/stack=feat-x"]
        );
        assert_eq!(STACK_LABEL, "chelabase.dev/stack");
    }

    #[test]
    fn compute_ctl_pulls_its_spec_from_the_control_plane() {
        let args = compute_ctl_args("ep-abc123", "http://orchestrator:8083");
        assert_eq!(
            args,
            [
                "--pgbin=/usr/local/bin/postgres",
                "--pgdata=/var/db/postgres/compute",
                "--connstr=postgresql://cloud_admin@localhost:5432/postgres",
                "--compute-id=ep-abc123",
                "--control-plane-uri=http://orchestrator:8083",
                "--external-http-port=3080",
            ]
        );
    }

    use bollard::models::{ContainerState, EndpointSettings, NetworkSettings, PortBinding};
    use std::collections::HashMap;

    fn inspect(
        status: ContainerStateStatusEnum,
        network: &str,
        ip: &str,
    ) -> ContainerInspectResponse {
        ContainerInspectResponse {
            state: Some(ContainerState {
                status: Some(status),
                ..Default::default()
            }),
            network_settings: Some(NetworkSettings {
                networks: Some(HashMap::from([(
                    network.to_string(),
                    EndpointSettings {
                        ip_address: Some(ip.to_string()),
                        ..Default::default()
                    },
                )])),
                ports: Some(HashMap::from([(
                    "3080/tcp".to_string(),
                    Some(vec![PortBinding {
                        host_ip: Some("127.0.0.1".to_string()),
                        host_port: Some("49153".to_string()),
                    }]),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn a_running_compute_has_an_ip_on_the_compute_network() {
        let running = inspect(
            ContainerStateStatusEnum::RUNNING,
            "chelabase_default",
            "172.20.0.9",
        );
        assert_eq!(
            ip_on_network(&running, "chelabase_default"),
            Some("172.20.0.9".parse().unwrap())
        );
        assert_eq!(
            ip_on_network(&running, "bridge"),
            None,
            "only the named network"
        );
    }

    #[test]
    fn a_running_compute_is_reached_on_the_stack_network() {
        let running = inspect(
            ContainerStateStatusEnum::RUNNING,
            "chelabase-feat-x_default",
            "172.20.0.9",
        );
        assert_eq!(
            control_addr_on_network(&running, "chelabase-feat-x_default"),
            Some("172.20.0.9:3080".parse().unwrap())
        );
        assert_eq!(
            postgres_addr_on_network(&running, "chelabase-feat-x_default"),
            Some("172.20.0.9:5432".parse().unwrap())
        );
        assert_eq!(
            control_addr_on_network(&running, "chelabase_default"),
            None,
            "only the stack's own network"
        );
    }

    #[test]
    fn a_stopped_compute_has_neither() {
        let exited = inspect(
            ContainerStateStatusEnum::EXITED,
            "chelabase_default",
            "172.20.0.9",
        );
        assert_eq!(ip_on_network(&exited, "chelabase_default"), None);
        assert_eq!(control_addr_on_network(&exited, "chelabase_default"), None);
        assert_eq!(postgres_addr_on_network(&exited, "chelabase_default"), None);
        let blank = inspect(ContainerStateStatusEnum::RUNNING, "chelabase_default", "");
        assert_eq!(
            ip_on_network(&blank, "chelabase_default"),
            None,
            "an empty IP is no IP"
        );
    }

}
