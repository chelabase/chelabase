//! One compute (Postgres plus compute_ctl) per endpoint, run as a Docker container.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

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
pub const ENDPOINT_LABEL: &str = "cheladb.dev/endpoint-id";

const NAME_PREFIX: &str = "cheladb-compute-";

/// Container names are global on a Docker host; the prefix keeps ours recognisable.
pub fn container_name(endpoint_id: &str) -> String {
    format!("{NAME_PREFIX}{endpoint_id}")
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

    /// `127.0.0.1:<published 3080>`: where the host reaches compute_ctl. The host can't
    /// route to container IPs on Docker Desktop.
    fn control_addr(
        &self,
        endpoint_id: &str,
    ) -> impl Future<Output = anyhow::Result<Option<SocketAddr>>> + Send;

    /// `127.0.0.1:<published 5432>`: where host services (the data API) reach Postgres.
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

/// The loopback address compute_ctl's port is published on (`-p 127.0.0.1::3080`), if the
/// container is running. An all-interfaces binding is reachable on loopback too.
pub fn published_control_addr(inspect: &ContainerInspectResponse) -> Option<SocketAddr> {
    published_addr(inspect, COMPUTE_CTL_PORT)
}

/// The loopback address Postgres is published on (`-p 127.0.0.1::5432`), if the container is
/// running.
pub fn published_postgres_addr(inspect: &ContainerInspectResponse) -> Option<SocketAddr> {
    published_addr(inspect, POSTGRES_PORT)
}

/// The loopback address the container's `port`/tcp is published on, if it's running.
fn published_addr(inspect: &ContainerInspectResponse, port: u16) -> Option<SocketAddr> {
    if !running(inspect) {
        return None;
    }
    let bindings = inspect
        .network_settings
        .as_ref()?
        .ports
        .as_ref()?
        .get(&format!("{port}/tcp"))?
        .as_ref()?;
    bindings.iter().find_map(|binding| {
        match binding.host_ip.as_deref() {
            None | Some("" | "127.0.0.1" | "0.0.0.0") => {}
            Some(_) => return None,
        }
        let port = binding.host_port.as_deref()?.parse().ok()?;
        Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port))
    })
}

/// [`ComputeLocator`] over the Docker Engine API.
pub struct DockerLocator {
    pub(crate) docker: Docker,
    network: String,
}

impl DockerLocator {
    /// `DOCKER_HOST` or the local socket, like the worker. Nothing is contacted yet.
    pub fn connect(network: &str) -> anyhow::Result<Self> {
        Ok(Self {
            docker: Docker::connect_with_local_defaults()
                .context("configuring the Docker client")?,
            network: network.to_string(),
        })
    }

    pub(crate) async fn inspect(
        &self,
        endpoint_id: &str,
    ) -> anyhow::Result<Option<ContainerInspectResponse>> {
        match self
            .docker
            .inspect_container(
                &container_name(endpoint_id),
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
            .and_then(|inspect| published_control_addr(&inspect)))
    }

    async fn postgres_addr(&self, endpoint_id: &str) -> anyhow::Result<Option<SocketAddr>> {
        Ok(self
            .inspect(endpoint_id)
            .await?
            .and_then(|inspect| published_postgres_addr(&inspect)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn containers_are_named_after_their_endpoint() {
        assert_eq!(container_name("ep-abc123"), "cheladb-compute-ep-abc123");
    }

    #[test]
    fn compute_ctl_pulls_its_spec_from_the_control_plane() {
        let args = compute_ctl_args("ep-abc123", "http://host.docker.internal:8083");
        assert_eq!(
            args,
            [
                "--pgbin=/usr/local/bin/postgres",
                "--pgdata=/var/db/postgres/compute",
                "--connstr=postgresql://cloud_admin@localhost:5432/postgres",
                "--compute-id=ep-abc123",
                "--control-plane-uri=http://host.docker.internal:8083",
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
            "cheladb_default",
            "172.20.0.9",
        );
        assert_eq!(
            ip_on_network(&running, "cheladb_default"),
            Some("172.20.0.9".parse().unwrap())
        );
        assert_eq!(
            ip_on_network(&running, "bridge"),
            None,
            "only the named network"
        );
    }

    #[test]
    fn a_running_compute_publishes_compute_ctl_on_loopback() {
        let running = inspect(
            ContainerStateStatusEnum::RUNNING,
            "cheladb_default",
            "172.20.0.9",
        );
        assert_eq!(
            published_control_addr(&running),
            Some("127.0.0.1:49153".parse().unwrap())
        );
    }

    #[test]
    fn a_stopped_compute_has_neither() {
        let exited = inspect(
            ContainerStateStatusEnum::EXITED,
            "cheladb_default",
            "172.20.0.9",
        );
        assert_eq!(ip_on_network(&exited, "cheladb_default"), None);
        assert_eq!(published_control_addr(&exited), None);
        let blank = inspect(ContainerStateStatusEnum::RUNNING, "cheladb_default", "");
        assert_eq!(
            ip_on_network(&blank, "cheladb_default"),
            None,
            "an empty IP is no IP"
        );
    }

    #[test]
    fn postgres_addr_reads_the_published_port() {
        let mut running = inspect(ContainerStateStatusEnum::RUNNING, "n", "172.20.0.9");
        assert_eq!(published_postgres_addr(&running), None, "not published");
        let ports = running
            .network_settings
            .as_mut()
            .unwrap()
            .ports
            .as_mut()
            .unwrap();
        ports.insert(
            "5432/tcp".to_string(),
            Some(vec![PortBinding {
                host_ip: Some("127.0.0.1".to_string()),
                host_port: Some("49200".to_string()),
            }]),
        );
        assert_eq!(
            published_postgres_addr(&running),
            Some("127.0.0.1:49200".parse().unwrap())
        );
        assert_eq!(
            published_control_addr(&running),
            Some("127.0.0.1:49153".parse().unwrap()),
            "each port reads its own binding"
        );
        running.state.as_mut().unwrap().status = Some(ContainerStateStatusEnum::EXITED);
        assert_eq!(published_postgres_addr(&running), None);
    }

    #[test]
    fn a_binding_on_another_interface_is_not_used() {
        let mut running = inspect(ContainerStateStatusEnum::RUNNING, "n", "172.20.0.9");
        let ports = running
            .network_settings
            .as_mut()
            .unwrap()
            .ports
            .as_mut()
            .unwrap();
        ports.insert(
            "3080/tcp".to_string(),
            Some(vec![PortBinding {
                host_ip: Some("192.168.1.5".to_string()),
                host_port: Some("49153".to_string()),
            }]),
        );
        assert_eq!(published_control_addr(&running), None);
    }
}
