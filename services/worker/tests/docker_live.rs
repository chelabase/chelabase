//! Opt-in tests against the local Docker daemon:
//!
//! ```sh
//! cargo test -p worker --test docker_live -- --ignored --test-threads=1
//! ```
//!
//! They only create and remove resources for a fresh random database id, and otherwise
//! only read.

use bollard::Docker;
use cheladb_types::DatabaseId;
use worker::docker_ops::{ensure_volume, observe, remove_database_resources, MANAGED_LABEL};
use worker::names::volume_name;

fn docker() -> Docker {
    Docker::connect_with_local_defaults().expect("Docker client")
}

/// Names of volumes carrying the managed label, straight from the Docker CLI.
fn cli_managed_volumes() -> Vec<String> {
    let out = std::process::Command::new("docker")
        .args(["volume", "ls", "-q", "--filter", &format!("label={MANAGED_LABEL}=true")])
        .output()
        .expect("docker CLI");
    assert!(out.status.success(), "docker volume ls failed");
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

#[tokio::test]
#[ignore]
async fn observe_sees_what_the_docker_cli_sees() {
    let observed = observe(&docker()).await.unwrap();
    let mut from_observe: Vec<String> = observed.volumes.iter().map(|id| volume_name(*id)).collect();
    let mut from_cli = cli_managed_volumes();
    from_observe.sort();
    from_cli.sort();
    assert_eq!(from_observe, from_cli);
}

#[tokio::test]
#[ignore]
async fn a_volume_round_trips_through_ensure_observe_and_remove() {
    let docker = docker();
    let id = DatabaseId::new();

    ensure_volume(&docker, id).await.unwrap();
    // Creating it again is a no-op.
    ensure_volume(&docker, id).await.unwrap();
    let seen = observe(&docker).await.unwrap().volumes.contains(&id);

    remove_database_resources(&docker, id).await.unwrap();
    // Removing what's already gone is fine.
    remove_database_resources(&docker, id).await.unwrap();
    let gone = !observe(&docker).await.unwrap().volumes.contains(&id);

    assert!(seen, "observe did not report the new volume {}", volume_name(id));
    assert!(gone, "volume {} survived removal", volume_name(id));
}
