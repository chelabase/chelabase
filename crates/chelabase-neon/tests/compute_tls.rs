//! Round 3: the platform CA, compute server and orchestrator client certificates, the upload
//! tar and the client pair on disk.

use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chelabase_neon::compute_tls::*;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

const DAY: Duration = Duration::from_secs(86_400);

fn der(pem: &str) -> CertificateDer<'static> {
    CertificateDer::from_pem_slice(pem.as_bytes()).expect("a PEM certificate")
}

fn roots(ca: &ComputeCa) -> Arc<rustls::RootCertStore> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(der(ca.cert_pem())).unwrap();
    Arc::new(roots)
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}

fn verify_server(ca: &ComputeCa, cert_pem: &str, name: &str, now: SystemTime) -> Result<(), rustls::Error> {
    let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(roots(ca), provider())
        .build()
        .unwrap();
    let name = ServerName::try_from(name.to_string()).unwrap();
    verifier
        .verify_server_cert(&der(cert_pem), &[], &name, &[], UnixTime::since_unix_epoch(now.duration_since(SystemTime::UNIX_EPOCH).unwrap()))
        .map(|_| ())
}

fn parsed(pem: &str) -> (String, bool, bool, SystemTime, String, String) {
    let (_, p) = x509_parser::pem::parse_x509_pem(pem.as_bytes()).unwrap();
    let cert = p.parse_x509().unwrap();
    let cn = cert.subject().iter_common_name().next().unwrap().as_str().unwrap().to_string();
    let eku = cert.extended_key_usage().unwrap().map(|e| e.value.clone());
    let (server, client) = eku.map_or((false, false), |e| (e.server_auth, e.client_auth));
    let not_after = SystemTime::UNIX_EPOCH + Duration::from_secs(cert.validity().not_after.timestamp() as u64);
    let sig = cert.signature_algorithm.algorithm.to_id_string();
    let key_alg = cert.public_key().algorithm.algorithm.to_id_string();
    (cn, server, client, not_after, sig, key_alg)
}

fn near(a: SystemTime, b: SystemTime) -> bool {
    let d = a.duration_since(b).unwrap_or_else(|e| e.duration());
    d < DAY
}

#[test]
fn the_server_name_is_the_endpoint_under_the_suffix() {
    assert_eq!(server_name("ep-abc123"), "ep-abc123.compute.chelabase.internal");
    assert_eq!(SERVER_NAME_SUFFIX, "compute.chelabase.internal");
    assert_eq!(ORCHESTRATOR_CN, "chelabase-orchestrator");
    assert_eq!((TLS_DIR, TLS_UPLOAD_ROOT), ("/var/db/chelabase/tls", "/var/db"));
    assert_eq!(
        (SERVER_KEY_PATH, SERVER_CERT_PATH, CA_CERT_PATH),
        ("/var/db/chelabase/tls/server.key", "/var/db/chelabase/tls/server.crt", "/var/db/chelabase/tls/ca.crt")
    );
}

#[test]
fn a_server_certificate_verifies_for_its_endpoint_only() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let server = ca.issue_server("ep-a", now).unwrap();
    verify_server(&ca, &server.cert_pem, &server_name("ep-a"), now).unwrap();
    assert!(verify_server(&ca, &server.cert_pem, &server_name("ep-b"), now).is_err());
    let other = ComputeCa::generate(now).unwrap();
    assert!(verify_server(&other, &server.cert_pem, &server_name("ep-a"), now).is_err(), "another CA");
    let (_, server_auth, client_auth, not_after, sig, key_alg) = parsed(&server.cert_pem);
    assert!(server_auth && !client_auth, "EKU serverAuth only");
    assert!(near(not_after, now + SERVER_VALIDITY) && near(server.not_after, not_after));
    assert_eq!(SERVER_VALIDITY, 90 * DAY);
    assert_eq!(sig, "1.2.840.10045.4.3.2", "ecdsa-with-SHA256, the only kind compute_ctl reads");
    assert_eq!(key_alg, "1.2.840.10045.2.1", "an EC key");
    assert!(server.key_pem.starts_with("-----BEGIN PRIVATE KEY-----"), "PKCS#8");
}

#[test]
fn a_client_certificate_carries_the_cn_and_client_auth() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let client = ca.issue_client(ORCHESTRATOR_CN, now).unwrap();
    let (cn, server_auth, client_auth, not_after, _, _) = parsed(&client.cert_pem);
    assert_eq!(cn, "chelabase-orchestrator");
    assert!(client_auth && !server_auth, "EKU clientAuth only");
    assert_eq!(CLIENT_VALIDITY, 365 * DAY);
    assert!(near(not_after, now + CLIENT_VALIDITY));
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(roots(&ca), provider())
        .build()
        .unwrap();
    verifier
        .verify_client_cert(&der(&client.cert_pem), &[], UnixTime::since_unix_epoch(now.duration_since(SystemTime::UNIX_EPOCH).unwrap()))
        .unwrap();
}

#[test]
fn the_ca_round_trips_through_pem_and_lasts_ten_years() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let again = ComputeCa::from_pem(ca.cert_pem(), ca.key_pem()).unwrap();
    let server = again.issue_server("ep-a", now).unwrap();
    verify_server(&ca, &server.cert_pem, &server_name("ep-a"), now).unwrap();
    assert_eq!(CA_VALIDITY, 3650 * DAY);
    assert!(near(not_after(ca.cert_pem()).unwrap(), now + CA_VALIDITY));
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("ca.crt"), ca.cert_pem()).unwrap();
    std::fs::write(dir.path().join("ca.key"), ca.key_pem()).unwrap();
    assert_eq!(ComputeCa::load(dir.path()).unwrap().cert_pem(), ca.cert_pem());
    assert!(ComputeCa::from_pem(ca.cert_pem(), "not a key").is_err());
}

#[test]
fn renewal_windows() {
    let now = SystemTime::now();
    assert_eq!((SERVER_RENEW_BEFORE, CLIENT_RENEW_BEFORE), (30 * DAY, 60 * DAY));
    assert!(needs_renewal(now + 29 * DAY, now, SERVER_RENEW_BEFORE));
    assert!(!needs_renewal(now + 31 * DAY, now, SERVER_RENEW_BEFORE));
    assert!(needs_renewal(now - DAY, now, SERVER_RENEW_BEFORE), "expired");
    assert!(needs_renewal(now + 59 * DAY, now, CLIENT_RENEW_BEFORE));
}

#[test]
fn the_upload_tar_holds_the_three_files_under_var_db() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let server = ca.issue_server("ep-a", now).unwrap();
    let tar = tls_files_tar(&server, ca.cert_pem()).unwrap();
    let mut archive = tar::Archive::new(tar.as_slice());
    let mut seen = Vec::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().to_string_lossy().trim_end_matches('/').to_string();
        let header = entry.header();
        let (mode, uid, gid) = (header.mode().unwrap(), header.uid().unwrap(), header.gid().unwrap());
        let mut body = String::new();
        entry.read_to_string(&mut body).unwrap();
        seen.push((path, mode, uid, gid, body));
    }
    let find = |p: &str| seen.iter().find(|s| s.0 == p).unwrap_or_else(|| panic!("{p} in {seen:?}")).clone();
    assert_eq!(find("chelabase").1, 0o755);
    assert_eq!(find("chelabase/tls").1, 0o755);
    for (p, body) in [
        ("chelabase/tls/server.crt", server.cert_pem.as_str()),
        ("chelabase/tls/server.key", server.key_pem.as_str()),
        ("chelabase/tls/ca.crt", ca.cert_pem()),
    ] {
        let (_, mode, uid, gid, got) = find(p);
        assert_eq!((mode, uid, gid), (0o644, 0, 0), "{p}");
        assert_eq!(got, body, "{p}");
    }
    assert_eq!(seen.len(), 5);
}

#[test]
fn the_client_pair_is_swapped_whole_and_reloaded() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let first = ca.issue_client(ORCHESTRATOR_CN, now).unwrap();
    let v1 = write_client_pair(dir.path(), &first, ca.cert_pem(), now).unwrap();
    assert_eq!(std::fs::read_link(dir.path().join("current")).unwrap().file_name(), v1.file_name());
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(v1.join("client.key")).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let tls = ClientTls::load(dir.path()).unwrap();
    let f1 = tls.fingerprint();
    assert!(!tls.reload_if_changed().unwrap(), "nothing changed");
    let second = ca.issue_client(ORCHESTRATOR_CN, now + Duration::from_secs(1)).unwrap();
    write_client_pair(dir.path(), &second, ca.cert_pem(), now + Duration::from_secs(1)).unwrap();
    assert!(tls.reload_if_changed().unwrap());
    assert_ne!(tls.fingerprint(), f1);
    assert!(!tls.reload_if_changed().unwrap());
    assert!(ClientTls::load(tempfile::tempdir().unwrap().path()).is_err(), "no pair");
}

#[test]
fn member_login_cidrs_parse() {
    use chelabase_neon::spec::parse_member_login_cidrs;
    assert!(parse_member_login_cidrs(None).is_err(), "unset is a startup error");
    assert_eq!(parse_member_login_cidrs(Some("")).unwrap(), Vec::<String>::new());
    assert_eq!(
        parse_member_login_cidrs(Some(" 10.213.4.10/32, 10.213.4.11/32 ")).unwrap(),
        vec!["10.213.4.10/32".to_string(), "10.213.4.11/32".to_string()]
    );
    assert_eq!(parse_member_login_cidrs(Some("fd00::1/128")).unwrap(), vec!["fd00::1/128".to_string()]);
    for bad in ["x", "10.0.0.1", "10.0.0.1/33", "10.0.0.1/32,", "10.0.0.1/32 # c"] {
        assert!(parse_member_login_cidrs(Some(bad)).is_err(), "{bad:?}");
    }
}

// --- round 4 (C1): W31, the realtime client pair --------------------------------------------

fn current_cert(dir: &std::path::Path) -> String {
    std::fs::read_to_string(dir.join("current").join(CLIENT_CERT_FILE)).unwrap()
}

fn current_version(dir: &std::path::Path) -> std::path::PathBuf {
    std::fs::read_link(dir.join("current")).unwrap()
}

#[test]
fn the_realtime_cn_is_its_own_identity() {
    assert_eq!(REALTIME_CN, "chelabase-realtime");
    assert_ne!(REALTIME_CN, ORCHESTRATOR_CN);
}

#[test]
fn a_realtime_client_certificate_carries_the_cn_and_client_auth_only() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let pair = ca.issue_client(REALTIME_CN, now).unwrap();
    let (cn, server, client, not_after, _, _) = parsed(&pair.cert_pem);
    assert_eq!(cn, "chelabase-realtime");
    assert!(client && !server, "clientAuth, not serverAuth");
    assert!(near(not_after, now + CLIENT_VALIDITY));
    assert!(signed_by(&pair.cert_pem, ca.cert_pem()));
    assert!(key_matches_cert(&pair.cert_pem, &pair.key_pem));
}

// The orchestrator pair's renewal, generalised to `(dir, cn)`: the worker's sweep and the
// CLI renew one directory per identity with the same code.
#[test]
fn renewing_a_missing_pair_writes_one_with_the_given_cn() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    for cn in [REALTIME_CN, ORCHESTRATOR_CN] {
        let dir = tempfile::tempdir().unwrap();
        assert!(renew_client_pair(dir.path(), &ca, cn, now).unwrap(), "{cn}");
        let crt = current_cert(dir.path());
        assert_eq!(parsed(&crt).0, cn);
        assert!(signed_by(&crt, ca.cert_pem()));
        let key = std::fs::read_to_string(dir.path().join("current/client.key")).unwrap();
        assert!(key_matches_cert(&crt, &key));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("current/ca.crt")).unwrap().trim(),
            ca.cert_pem().trim()
        );
        // And the pair loads as a client identity.
        ClientTls::load(dir.path()).unwrap();
    }
}

#[test]
fn a_fresh_pair_is_left_alone() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let dir = tempfile::tempdir().unwrap();
    assert!(renew_client_pair(dir.path(), &ca, REALTIME_CN, now).unwrap());
    let version = current_version(dir.path());
    let cert = current_cert(dir.path());
    let later = now + Duration::from_secs(10 * 86_400);
    assert!(!renew_client_pair(dir.path(), &ca, REALTIME_CN, later).unwrap());
    assert_eq!(current_version(dir.path()), version);
    assert_eq!(current_cert(dir.path()), cert);
}

#[test]
fn a_pair_under_60_days_from_expiry_is_reissued() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let dir = tempfile::tempdir().unwrap();
    renew_client_pair(dir.path(), &ca, REALTIME_CN, now).unwrap();
    let old = current_cert(dir.path());
    // 304 days in: 61 left, not due; 306 days in: 59 left, due.
    let not_due = now + Duration::from_secs(304 * 86_400);
    assert!(!renew_client_pair(dir.path(), &ca, REALTIME_CN, not_due).unwrap());
    let due = now + Duration::from_secs(306 * 86_400);
    assert!(renew_client_pair(dir.path(), &ca, REALTIME_CN, due).unwrap());
    let new = current_cert(dir.path());
    assert_ne!(new, old);
    assert_eq!(parsed(&new).0, REALTIME_CN);
    assert!(near(parsed(&new).3, due + CLIENT_VALIDITY));
}

#[test]
fn a_corrupt_or_foreign_pair_is_reissued_not_an_error() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();

    // client.crt that isn't a certificate.
    let dir = tempfile::tempdir().unwrap();
    renew_client_pair(dir.path(), &ca, REALTIME_CN, now).unwrap();
    let version = std::fs::canonicalize(dir.path().join("current")).unwrap();
    std::fs::write(version.join(CLIENT_CERT_FILE), "not a certificate").unwrap();
    assert!(renew_client_pair(dir.path(), &ca, REALTIME_CN, now).unwrap());
    assert_eq!(parsed(&current_cert(dir.path())).0, REALTIME_CN);

    // The orchestrator's pair in the realtime directory: wrong identity, reissued with the
    // directory's CN (a pair must never present another service's identity).
    let dir = tempfile::tempdir().unwrap();
    renew_client_pair(dir.path(), &ca, ORCHESTRATOR_CN, now).unwrap();
    assert_eq!(parsed(&current_cert(dir.path())).0, ORCHESTRATOR_CN);
    assert!(renew_client_pair(dir.path(), &ca, REALTIME_CN, now).unwrap());
    assert_eq!(parsed(&current_cert(dir.path())).0, REALTIME_CN);

    // A regenerated CA: the old pair doesn't follow it.
    let dir = tempfile::tempdir().unwrap();
    renew_client_pair(dir.path(), &ca, REALTIME_CN, now).unwrap();
    let other = ComputeCa::generate(now).unwrap();
    assert!(renew_client_pair(dir.path(), &other, REALTIME_CN, now).unwrap());
    let crt = current_cert(dir.path());
    assert!(signed_by(&crt, other.cert_pem()) && !signed_by(&crt, ca.cert_pem()));
}

#[test]
fn two_identities_in_two_directories_do_not_touch_each_other() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let (rt, orch) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    renew_client_pair(rt.path(), &ca, REALTIME_CN, now).unwrap();
    renew_client_pair(orch.path(), &ca, ORCHESTRATOR_CN, now).unwrap();
    let orch_cert = current_cert(orch.path());
    // Renewing the realtime pair leaves the orchestrator's alone.
    let due = now + Duration::from_secs(320 * 86_400);
    assert!(renew_client_pair(rt.path(), &ca, REALTIME_CN, due).unwrap());
    assert_eq!(current_cert(orch.path()), orch_cert);
}

#[test]
fn an_invalid_cn_is_an_error_and_writes_nothing() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    for bad in ["", "has space", "a,b", "x\ny", &"a".repeat(65)] {
        let dir = tempfile::tempdir().unwrap();
        assert!(renew_client_pair(dir.path(), &ca, bad, now).is_err(), "{bad:?}");
        assert!(!dir.path().join("current").exists(), "{bad:?}");
    }
}
