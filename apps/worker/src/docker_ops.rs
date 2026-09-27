//! Docker operations for tenant databases: containers and their named volumes.

use crate::names::{database_id_from_volume, volume_name};
use crate::reconcile::Observed;
use bollard::container::{ListContainersOptions, RemoveContainerOptions};
use bollard::errors::Error as DockerError;
use bollard::volume::{CreateVolumeOptions, ListVolumesOptions, RemoveVolumeOptions};
use bollard::Docker;
use cheladb_types::DatabaseId;
use std::collections::HashMap;

pub const MANAGED_LABEL: &str = "cheladb.managed";
pub const DATABASE_ID_LABEL: &str = "cheladb.database_id";

pub fn container_name(id: DatabaseId) -> String {
    format!("cheladb-db-{}", id.as_uuid())
}

/// Labels on every container and volume the worker manages.
pub fn labels(id: DatabaseId) -> HashMap<String, String> {
    HashMap::from([
        (MANAGED_LABEL.to_string(), "true".to_string()),
        (DATABASE_ID_LABEL.to_string(), id.to_string()),
    ])
}

fn is_not_found(err: &DockerError) -> bool {
    matches!(err, DockerError::DockerResponseServerError { status_code: 404, .. })
}

/// Create the database's data volume (a no-op if it already exists).
pub async fn ensure_volume(docker: &Docker, id: DatabaseId) -> Result<(), String> {
    docker
        .create_volume(CreateVolumeOptions {
            name: volume_name(id),
            driver: "local".to_string(),
            driver_opts: HashMap::new(),
            labels: labels(id),
        })
        .await
        .map(|_| ())
        .map_err(|e| format!("create volume {}: {e}", volume_name(id)))
}

/// Remove the database's container and data volume. Missing ones are fine,
/// so this is safe to repeat.
pub async fn remove_database_resources(docker: &Docker, id: DatabaseId) -> Result<(), String> {
    let name = container_name(id);
    // `v` also drops anonymous volumes of containers created before named volumes.
    let options = RemoveContainerOptions {
        force: true,
        v: true,
        link: false,
    };
    match docker.remove_container(&name, Some(options)).await {
        Ok(()) => {}
        Err(err) if is_not_found(&err) => {}
        Err(err) => return Err(format!("remove container {name}: {err}")),
    }
    let volume = volume_name(id);
    match docker
        .remove_volume(&volume, Some(RemoveVolumeOptions { force: true }))
        .await
    {
        Ok(()) => Ok(()),
        Err(err) if is_not_found(&err) => Ok(()),
        Err(err) => Err(format!("remove volume {volume}: {err}")),
    }
}

/// Managed containers and volumes, by the database they belong to.
pub async fn observe(docker: &Docker) -> Result<Observed, String> {
    let filters = HashMap::from([(
        "label".to_string(),
        vec![format!("{MANAGED_LABEL}=true")],
    )]);
    let parse = |raw: &str| uuid::Uuid::parse_str(raw).ok().map(DatabaseId);
    let mut observed = Observed::default();

    let containers = docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            filters: filters.clone(),
            ..Default::default()
        }))
        .await
        .map_err(|e| format!("list containers: {e}"))?;
    for container in containers {
        if let Some(id) = container
            .labels
            .as_ref()
            .and_then(|l| l.get(DATABASE_ID_LABEL))
            .and_then(|raw| parse(raw))
        {
            observed.containers.insert(id);
        }
    }

    let volumes = docker
        .list_volumes(Some(ListVolumesOptions { filters }))
        .await
        .map_err(|e| format!("list volumes: {e}"))?;
    for volume in volumes.volumes.unwrap_or_default() {
        let id = volume
            .labels
            .get(DATABASE_ID_LABEL)
            .and_then(|raw| parse(raw))
            .or_else(|| database_id_from_volume(&volume.name));
        if let Some(id) = id {
            observed.volumes.insert(id);
        }
    }
    Ok(observed)
}
