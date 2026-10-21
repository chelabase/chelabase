//! S1: the worker reads the `chelabase_realtime` password from
//! `CHELABASE_REALTIME_PG_PASSWORD_FILE` (one trailing newline ignored); S20: `build` takes the
//! publisher and wraps the real ports. P14: the compute settings change, so the settings version
//! moves on and reconcile re-creates older computes with them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use chelabase_metadata::{MetadataClient, MetadataConfig};
use chelabase_neon::compute_tls::ComputeCa;
use chelabase_neon::jwt::{PRIVATE_KEY_FILE, PUBLIC_KEY_FILE, generate_keypair};
use chelabase_secrets::SecretKey;

use crate::config::{RealWorker, WorkerConfig, build};
use crate::rt_events::NoopPublisher;

const VAR: &str = "CHELABASE_REALTIME_PG_PASSWORD_FILE";
const IMAGE: &str = "chelabase/compute-node-v17:test";

fn config_with(pairs: &[(&str, &str)]) -> WorkerConfig {
    let pairs: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    WorkerConfig::from_lookup(move |k| {
        if k == "CHELABASE_COMPUTE_IMAGE" {
            return Some(IMAGE.to_string());
        }
        pairs
            .iter()
            .find(|(name, _)| name == k)
            .map(|(_, v)| v.clone())
    })
    .unwrap()
}

#[test]
fn config_reads_the_realtime_password_file() {
    let c = config_with(&[(VAR, "/keys/realtime/pg.password")]);
    assert_eq!(
        c.realtime_pg_password_file,
        Some(PathBuf::from("/keys/realtime/pg.password"))
    );
    assert_eq!(
        config_with(&[(VAR, "  ")]).realtime_pg_password_file,
        None,
        "blank is unset"
    );
    assert_eq!(config_with(&[]).realtime_pg_password_file, None);
}

/// A config `build` accepts: JWT keys, a compute CA and member logins in temporary dirs.
fn buildable(dir: &Path) -> WorkerConfig {
    let jwt = dir.join("jwt");
    let ca_dir = dir.join("ca");
    std::fs::create_dir_all(&jwt).unwrap();
    std::fs::create_dir_all(&ca_dir).unwrap();
    let (private, public) = generate_keypair();
    std::fs::write(jwt.join(PRIVATE_KEY_FILE), private).unwrap();
    std::fs::write(jwt.join(PUBLIC_KEY_FILE), public).unwrap();
    let ca = ComputeCa::generate(SystemTime::now()).unwrap();
    std::fs::write(ca_dir.join("ca.crt"), ca.cert_pem()).unwrap();
    std::fs::write(ca_dir.join("ca.key"), ca.key_pem()).unwrap();
    let mut config = config_with(&[]);
    config.compute_jwt_dir = jwt;
    config.compute_ca_dir = ca_dir;
    config.member_login_cidrs = Some(vec![]);
    config
}

fn lazy_metadata() -> MetadataClient {
    MetadataClient::connect_lazy(&MetadataConfig {
        database_url: "postgres://u:p@127.0.0.1:1/none".into(),
    })
    .unwrap()
}

fn built(config: &WorkerConfig) -> anyhow::Result<RealWorker> {
    build(
        config,
        lazy_metadata(),
        SecretKey::generate(),
        Arc::new(NoopPublisher),
    )
}

#[tokio::test]
async fn build_reads_the_realtime_password() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = buildable(dir.path());
    let file = dir.path().join("pg.password");
    std::fs::write(&file, "rt-secret-1\n").unwrap();
    config.realtime_pg_password_file = Some(file);

    let worker = built(&config).expect("build");

    assert_eq!(worker.realtime_password.as_deref(), Some("rt-secret-1"));
}

#[tokio::test]
async fn build_without_the_password_file_makes_no_role() {
    let dir = tempfile::tempdir().unwrap();
    let worker = built(&buildable(dir.path())).expect("build");
    assert_eq!(worker.realtime_password, None);
}

#[tokio::test]
async fn build_names_an_unreadable_or_empty_password_file() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = buildable(dir.path());
    config.realtime_pg_password_file = Some(dir.path().join("missing.password"));
    let err = match built(&config) {
        Ok(_) => panic!("build succeeded without the realtime password"),
        Err(err) => format!("{err:#}"),
    };
    assert!(err.contains(VAR), "{err}");

    let empty = dir.path().join("empty.password");
    std::fs::write(&empty, "\n").unwrap();
    config.realtime_pg_password_file = Some(empty);
    let err = match built(&config) {
        Ok(_) => panic!("build accepted an empty realtime password"),
        Err(err) => format!("{err:#}"),
    };
    assert!(err.contains(VAR), "{err}");
}

#[test]
fn the_compute_settings_version_moves_on_for_the_replication_settings() {
    // 4: `max_replication_slots`/`max_wal_senders` 20 and `max_slot_wal_keep_size` (P14). They
    // apply at the next compute start, so reconcile re-creates computes made with 3 or less.
    assert_eq!(crate::COMPUTE_SETTINGS_VERSION, 4);
}
