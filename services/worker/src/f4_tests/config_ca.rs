//! T9 m6: a worker that can't find its compute CA says which command creates it.

use chelabase_metadata::{MetadataClient, MetadataConfig};
use chelabase_neon::jwt::{PRIVATE_KEY_FILE, PUBLIC_KEY_FILE, generate_keypair};
use chelabase_secrets::SecretKey;

use crate::config::{WorkerConfig, build};

#[tokio::test]
async fn build_without_the_compute_ca_names_the_bootstrap() {
    let jwt = tempfile::tempdir().unwrap();
    let (private, public) = generate_keypair();
    std::fs::write(jwt.path().join(PRIVATE_KEY_FILE), private).unwrap();
    std::fs::write(jwt.path().join(PUBLIC_KEY_FILE), public).unwrap();
    let empty_ca_dir = tempfile::tempdir().unwrap();
    let mut config = WorkerConfig::from_lookup(|k| {
        (k == "CHELABASE_COMPUTE_IMAGE").then(|| "chelabase/compute-node-v17:test".to_string())
    })
    .unwrap();
    config.compute_jwt_dir = jwt.path().to_path_buf();
    config.compute_ca_dir = empty_ca_dir.path().to_path_buf();
    config.member_login_cidrs = Some(vec![]);
    let metadata = MetadataClient::connect_lazy(&MetadataConfig {
        database_url: "postgres://u:p@127.0.0.1:1/none".into(),
    })
    .unwrap();
    let err = match build(
        &config,
        metadata,
        SecretKey::generate(),
        std::sync::Arc::new(crate::rt_events::NoopPublisher),
    ) {
        Ok(_) => panic!("build succeeded without a compute CA"),
        Err(err) => format!("{err:#}"),
    };
    assert!(
        err.contains("CHELABASE_COMPUTE_CA_DIR"),
        "the setting is named: {err}"
    );
    assert!(
        err.contains("just zitadel-bootstrap"),
        "the command that creates the CA is named (like the JWT keys' error): {err}"
    );
}
