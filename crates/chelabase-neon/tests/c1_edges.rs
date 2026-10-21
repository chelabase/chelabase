//! Round 4 (C1) edge cases beyond the RED set: capability reader on joined headers, the
//! realtime pair's partial-state recovery, token claim bounds.

use std::time::{Duration, SystemTime};

use chelabase_neon::compute_tls::{ComputeCa, REALTIME_CN, renew_client_pair};
use chelabase_neon::jwt::{ComputeSigner, generate_keypair};
use chelabase_neon::reconfigure::advertises_storage_token_refresh;

fn signer() -> ComputeSigner {
    let (pem, x) = generate_keypair();
    ComputeSigner::new(&pem, &x).unwrap()
}

#[test]
fn a_joined_multi_value_header_is_read_whole() {
    // SP23: the caller joins every value of the header with a comma.
    assert!(advertises_storage_token_refresh(Some(
        "a,b,storage_token_refresh"
    )));
    assert!(advertises_storage_token_refresh(Some(
        "a, b ,storage_token_refresh,c"
    )));
    assert!(!advertises_storage_token_refresh(Some("a,b")));
}

#[test]
fn a_pair_missing_its_key_or_ca_file_is_reissued() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    for gone in ["client.key", "ca.crt", "client.crt"] {
        let dir = tempfile::tempdir().unwrap();
        renew_client_pair(dir.path(), &ca, REALTIME_CN, now).unwrap();
        let version = std::fs::canonicalize(dir.path().join("current")).unwrap();
        std::fs::remove_file(version.join(gone)).unwrap();
        assert!(
            renew_client_pair(dir.path(), &ca, REALTIME_CN, now).unwrap(),
            "{gone}"
        );
        assert!(
            !renew_client_pair(dir.path(), &ca, REALTIME_CN, now).unwrap(),
            "{gone}"
        );
    }
}

#[test]
fn a_key_that_does_not_match_the_certificate_is_reissued() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let dir = tempfile::tempdir().unwrap();
    renew_client_pair(dir.path(), &ca, REALTIME_CN, now).unwrap();
    let other = ca.issue_client(REALTIME_CN, now).unwrap();
    let version = std::fs::canonicalize(dir.path().join("current")).unwrap();
    std::fs::write(version.join("client.key"), other.key_pem).unwrap();
    assert!(renew_client_pair(dir.path(), &ca, REALTIME_CN, now).unwrap());
}

#[test]
fn an_expired_pair_is_reissued() {
    let now = SystemTime::now();
    let ca = ComputeCa::generate(now).unwrap();
    let dir = tempfile::tempdir().unwrap();
    renew_client_pair(dir.path(), &ca, REALTIME_CN, now).unwrap();
    let way_later = now + Duration::from_secs(400 * 86_400);
    assert!(renew_client_pair(dir.path(), &ca, REALTIME_CN, way_later).unwrap());
}

#[test]
fn token_bounds_hold_at_the_edges() {
    let s = signer();
    let t = chelabase_neon::NeonId::from(uuid::Uuid::from_u128(9));
    // An expiring token at the largest clock that fits, and a non-expiring one at u64::MAX.
    assert!(s.tenant_token(&t, u64::MAX - 86_400, true).is_ok());
    assert!(s.tenant_token(&t, u64::MAX, false).is_ok());
    assert!(s.tenant_token(&t, u64::MAX - 86_399, true).is_err());
}
