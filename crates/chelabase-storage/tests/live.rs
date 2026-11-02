//! Conformance suite against a real store. Run by m16 against the workbench SeaweedFS:
//! `cargo nextest run -p chelabase-storage --run-ignored only` with the
//! `CHELABASE_STORAGE_*` variables set.

use chelabase_storage::{S3Store, StorageConfig, conformance};

#[tokio::test]
#[ignore = "needs a live S3-compatible store"]
async fn s3_store_passes_conformance() {
    let cfg = StorageConfig::from_env().expect("storage env");
    let prefix = format!("conformance-{}", std::process::id());
    conformance::run(&S3Store::new(&cfg), &prefix).await;
}
