//! Against a real Docker daemon and the Neon compute image (`just deps-up` pulls or builds it).
//!
//! ```sh
//! cargo test -p cheladb-neon --test docker_backend_live -- --ignored --test-threads=1
//! ```
//!
//! `admin_sql_on_a_running_compute` also needs `just dev` running and `CHELADB_LIVE_ENDPOINT`
//! set to the endpoint id (`ep-…`, the host's first label) of a database created through the API or Studio.

use cheladb_neon::{ComputeBackend, ComputeSettings, ComputeStatus, DockerBackend};

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

fn backend(network: &str, control_plane_uri: &str) -> DockerBackend {
    DockerBackend::connect(ComputeSettings {
        image: compute_image(),
        network: network.to_string(),
        control_plane_uri: control_plane_uri.to_string(),
    })
    .unwrap()
}

#[tokio::test]
#[ignore = "needs Docker and the compute image"]
async fn lifecycle() {
    let backend = backend("bridge", "http://127.0.0.1:9");
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

#[tokio::test]
#[ignore = "needs CHELADB_LIVE_ENDPOINT: the endpoint id of a database created through the API or Studio with `just dev`"]
async fn admin_sql_on_a_running_compute() {
    let ep = std::env::var("CHELADB_LIVE_ENDPOINT").expect("CHELADB_LIVE_ENDPOINT");
    let backend = backend("cheladb_default", "http://host.docker.internal:8083");

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
             IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'cheladb_live_probe') THEN\n\
             CREATE ROLE cheladb_live_probe NOLOGIN;\n\
             END IF;\n\
             END $$",
        )
        .await
        .unwrap();
    backend
        .admin_sql(&ep, "postgres", "DROP ROLE cheladb_live_probe")
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
