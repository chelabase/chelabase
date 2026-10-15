//! F4 RED tests for `compute_tls` (T8 I1, m1-m3, m5). Declared from `compute_tls.rs` as a
//! child module so it can see the private `ClientTls` state.
//!
//! Invariants stated here and tested below:
//! - the upload tar writes `server.key` before `server.crt` (a reader that polls between
//!   entries never pairs a new certificate with an old key);
//! - `write_client_pair` is safe for concurrent writers (CLI and sweep): every call succeeds,
//!   `current` always points at a complete version directory that exists, nothing but `v*`
//!   directories and `current` is left behind, and only the current version and one previous
//!   one are kept;
//! - the CA a worker loads is usable: not expired, a CA by basicConstraints, with keyCertSign;
//! - the orchestrator-facing client config speaks TLS 1.3 only.

use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::time::{Duration, SystemTime};

use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair, KeyUsagePurpose};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};

use super::*;

const DAY: Duration = Duration::from_secs(86_400);

fn tar_entry_names(tar: &[u8]) -> Vec<String> {
    let mut archive = tar::Archive::new(tar);
    archive
        .entries()
        .unwrap()
        .map(|entry| {
            let mut entry = entry.unwrap();
            let mut sink = Vec::new();
            entry.read_to_end(&mut sink).unwrap();
            entry
                .path()
                .unwrap()
                .to_string_lossy()
                .trim_end_matches('/')
                .to_string()
        })
        .collect()
}

#[test]
fn the_tar_writes_the_key_before_the_certificate() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let server = ca.issue_server("ep-a", now).unwrap();
    let names = tar_entry_names(&tls_files_tar(&server, ca.cert_pem()).unwrap());
    let at = |name: &str| {
        names
            .iter()
            .position(|n| n == name)
            .unwrap_or_else(|| panic!("{name} in {names:?}"))
    };
    assert!(
        at("chelabase/tls/server.key") < at("chelabase/tls/server.crt"),
        "a renewal is seen half-done when the certificate lands before its key: {names:?}"
    );
    // Directories first, then files (the extraction needs the directories).
    assert!(at("chelabase") < at("chelabase/tls"));
    assert!(at("chelabase/tls") < at("chelabase/tls/server.key"));
}

#[test]
fn the_copy_postgres_serves_is_named() {
    // compute_ctl copies the uploaded key and certificate into `pgdata` (`--pgdata` of
    // `compute_ctl_args`); that copy, not the upload, is what Postgres serves (T9 I3).
    assert_eq!(
        PGDATA_SERVER_CERT_PATH,
        "/var/db/postgres/compute/server.crt"
    );
    assert!(
        crate::compute::compute_ctl_args("ep-a", "http://o:1")
            .contains(&"--pgdata=/var/db/postgres/compute".to_string())
    );
}

fn version_dirs(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('v'))
        .collect();
    names.sort();
    names
}

fn current_target_name(dir: &Path) -> String {
    std::fs::read_link(dir.join("current"))
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

#[test]
fn concurrent_writers_all_succeed_and_current_stays_whole() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let dir = tempfile::tempdir().unwrap();
    const WRITERS: usize = 8;
    for round in 0..15 {
        let pairs: Vec<IssuedCert> = (0..WRITERS)
            .map(|_| ca.issue_client(ORCHESTRATOR_CN, now).unwrap())
            .collect();
        let barrier = Arc::new(Barrier::new(WRITERS));
        let handles: Vec<_> = pairs
            .into_iter()
            .map(|pair| {
                let (barrier, root, ca_pem) = (
                    barrier.clone(),
                    dir.path().to_path_buf(),
                    ca.cert_pem().to_string(),
                );
                // The same `now` for every writer: the CLI and the sweep can land in one second.
                std::thread::spawn(move || {
                    barrier.wait();
                    write_client_pair(&root, &pair, &ca_pem, now)
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for (i, r) in results.iter().enumerate() {
            assert!(
                r.is_ok(),
                "round {round}, writer {i} failed: {:#}",
                r.as_ref().unwrap_err()
            );
        }
        // `current` is complete and loadable after every round.
        let tls = ClientTls::load(dir.path())
            .unwrap_or_else(|e| panic!("round {round}: current isn't a whole pair: {e:#}"));
        assert_eq!(tls.fingerprint().len(), 32);
        // No temporary leftovers: only `v*` directories and `current`.
        let mut others: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('v') && n != "current")
            .collect();
        others.sort();
        assert!(others.is_empty(), "round {round}: leftovers {others:?}");
    }
}

#[test]
fn the_previous_version_is_kept_and_older_ones_are_pruned() {
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    let ca = ComputeCa::generate(base).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut written = Vec::new();
    for i in 1..=5u64 {
        let at = base + Duration::from_secs(i * 100);
        let pair = ca.issue_client(ORCHESTRATOR_CN, at).unwrap();
        let v = write_client_pair(dir.path(), &pair, ca.cert_pem(), at).unwrap();
        written.push(v.file_name().unwrap().to_string_lossy().into_owned());
        let kept = version_dirs(dir.path());
        assert!(kept.len() <= 2, "after write {i}: {kept:?}");
        assert!(
            kept.contains(&written[i as usize - 1]),
            "the new version stays: {kept:?}"
        );
        if i >= 2 {
            assert!(
                kept.contains(&written[i as usize - 2]),
                "the previous version stays for a reader mid-switch: {kept:?}"
            );
        }
        assert_eq!(current_target_name(dir.path()), written[i as usize - 1]);
    }
    assert_eq!(
        version_dirs(dir.path()),
        written[3..].to_vec(),
        "only the last two remain"
    );
}

#[test]
fn pruning_never_removes_the_current_version() {
    let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    let ca = ComputeCa::generate(base).unwrap();
    let dir = tempfile::tempdir().unwrap();
    // A clock that steps back: the newest write has the lowest name.
    for secs in [900u64, 800, 700, 600, 500] {
        let at = base + Duration::from_secs(secs);
        let pair = ca.issue_client(ORCHESTRATOR_CN, at).unwrap();
        let v = write_client_pair(dir.path(), &pair, ca.cert_pem(), at).unwrap();
        let name = v.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(current_target_name(dir.path()), name);
        assert!(
            version_dirs(dir.path()).contains(&name),
            "current was pruned: {name}"
        );
        assert!(version_dirs(dir.path()).len() <= 2);
        ClientTls::load(dir.path()).unwrap();
    }
}

#[test]
fn same_second_writes_are_pruned_too_and_current_survives() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_500_000_000);
    let ca = ComputeCa::generate(now).unwrap();
    let dir = tempfile::tempdir().unwrap();
    for _ in 0..6 {
        let pair = ca.issue_client(ORCHESTRATOR_CN, now).unwrap();
        let v = write_client_pair(dir.path(), &pair, ca.cert_pem(), now).unwrap();
        let name = v.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(current_target_name(dir.path()), name);
        let kept = version_dirs(dir.path());
        assert!(kept.contains(&name) && kept.len() <= 2, "{kept:?}");
        ClientTls::load(dir.path()).unwrap();
    }
}

#[test]
fn an_expired_ca_is_refused() {
    let long_ago = SystemTime::now() - CA_VALIDITY - 30 * DAY;
    let expired = ComputeCa::generate(long_ago).unwrap();
    let err = ComputeCa::from_pem(expired.cert_pem(), expired.key_pem()).unwrap_err();
    assert!(
        format!("{err:#}").to_lowercase().contains("expire"),
        "the error names the expiry: {err:#}"
    );
    // The same CA, still valid, is accepted (the control).
    let fresh = ComputeCa::generate(SystemTime::now() - 30 * DAY).unwrap();
    ComputeCa::from_pem(fresh.cert_pem(), fresh.key_pem()).unwrap();
}

#[test]
fn a_ca_without_key_cert_sign_is_refused() {
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let mut params = CertificateParams::default();
    params.distinguished_name = distinguished_name("no keyCertSign");
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::CrlSign];
    let cert = params.self_signed(&key).unwrap();
    let err = ComputeCa::from_pem(&cert.pem(), &key.serialize_pem()).unwrap_err();
    assert!(
        format!("{err:#}").to_lowercase().contains("keycertsign"),
        "the error names the missing key usage: {err:#}"
    );
}

#[test]
fn a_leaf_certificate_isnt_a_ca() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    for leaf in [
        ca.issue_client(ORCHESTRATOR_CN, now).unwrap(),
        ca.issue_server("ep-a", now).unwrap(),
    ] {
        assert!(ComputeCa::from_pem(&leaf.cert_pem, &leaf.key_pem).is_err());
    }
}

/// An in-memory TLS handshake of `client` against a server that speaks only `versions`.
fn handshake(
    client: Arc<rustls::ClientConfig>,
    versions: &[&'static rustls::SupportedProtocolVersion],
    ca: &ComputeCa,
) -> Result<rustls::ProtocolVersion, rustls::Error> {
    let server = ca.issue_server("ep-a", SystemTime::now()).unwrap();
    let certs: Vec<CertificateDer<'static>> =
        CertificateDer::pem_slice_iter(server.cert_pem.as_bytes())
            .collect::<Result<_, _>>()
            .unwrap();
    let key = PrivateKeyDer::from_pem_slice(server.key_pem.as_bytes()).unwrap();
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
    let name = ServerName::try_from(server_name("ep-a")).unwrap();
    let mut c = rustls::ClientConnection::new(client, name).unwrap();
    let mut s = rustls::ServerConnection::new(Arc::new(config)).unwrap();
    for _ in 0..16 {
        if c.wants_write() {
            let mut buf = Vec::new();
            c.write_tls(&mut buf).unwrap();
            s.read_tls(&mut buf.as_slice()).unwrap();
            s.process_new_packets()?;
        }
        if s.wants_write() {
            let mut buf = Vec::new();
            s.write_tls(&mut buf).unwrap();
            c.read_tls(&mut buf.as_slice()).unwrap();
            c.process_new_packets()?;
        }
        if !c.is_handshaking() && !s.is_handshaking() {
            return Ok(c.protocol_version().expect("a negotiated version"));
        }
    }
    panic!("the handshake didn't finish");
}

#[test]
fn the_client_config_speaks_tls_1_3_only() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let pair = ca.issue_client(ORCHESTRATOR_CN, now).unwrap();
    write_client_pair(dir.path(), &pair, ca.cert_pem(), now).unwrap();
    let tls = ClientTls::load(dir.path()).unwrap();
    let config = tls.loaded.load().config.clone();
    // The control: a TLS 1.3 server negotiates 1.3.
    assert_eq!(
        handshake(config.clone(), &[&rustls::version::TLS13], &ca).unwrap(),
        rustls::ProtocolVersion::TLSv1_3
    );
    // A server that offers only TLS 1.2 is refused.
    assert!(
        handshake(config.clone(), &[&rustls::version::TLS12], &ca).is_err(),
        "the client config must not downgrade to TLS 1.2"
    );
    // A server offering both still lands on 1.3.
    assert_eq!(
        handshake(
            config,
            &[&rustls::version::TLS12, &rustls::version::TLS13],
            &ca
        )
        .unwrap(),
        rustls::ProtocolVersion::TLSv1_3
    );
}

#[test]
fn a_crashed_writers_temporary_link_is_pruned_by_the_next_write() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_600_000_000);
    let ca = ComputeCa::generate(now).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let pair = ca.issue_client(ORCHESTRATOR_CN, now).unwrap();
    write_client_pair(dir.path(), &pair, ca.cert_pem(), now).unwrap();
    // A writer that died between creating its link and renaming it.
    for stale in ["current.tmp", "current.tmp.4242.0"] {
        std::os::unix::fs::symlink("v-gone", dir.path().join(stale)).unwrap();
    }
    let later = now + Duration::from_secs(5);
    let pair = ca.issue_client(ORCHESTRATOR_CN, later).unwrap();
    write_client_pair(dir.path(), &pair, ca.cert_pem(), later).unwrap();
    let mut left: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('v') && n != "current")
        .collect();
    left.sort();
    assert!(left.is_empty(), "leftover temporary links: {left:?}");
    ClientTls::load(dir.path()).unwrap();
}
