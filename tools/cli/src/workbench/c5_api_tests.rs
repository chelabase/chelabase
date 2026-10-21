//! C5 (neon fork round 4): the tests that need new names. Pinned:
//! `env::ControlNet::realtime_ip` and `service_auth::mint_metrics_token(keys, now) -> Result<PathBuf>`
//! (and `service_auth::METRICS_TOKEN_FILE`).

use super::env::{cidr_overlaps, control_net};
use super::service_auth::{METRICS_TOKEN_FILE, mint_metrics_token, write_storage_auth};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

#[test]
fn control_net_gives_realtime_the_dot_twelve_outside_the_compute_range() {
    let main = control_net(0);
    assert_eq!(main.realtime_ip, "10.213.0.12");
    assert_eq!(control_net(1).realtime_ip, "10.213.4.12");
    assert_eq!(control_net(63).realtime_ip, "10.213.252.12");
    for slot in 0..=63u8 {
        let n = control_net(slot);
        let host = format!("{}/32", n.realtime_ip);
        assert!(cidr_overlaps(&host, &n.subnet), "slot {slot}: inside the subnet");
        assert!(!cidr_overlaps(&host, &n.compute_range), "slot {slot}: outside the compute range");
        assert_ne!(n.realtime_ip, n.orchestrator_ip, "slot {slot}");
        assert_ne!(n.realtime_ip, n.runner_ip, "slot {slot}");
    }
}

fn keys_with_pair() -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    crate::neon::compute_jwt::ensure(&t.path().join("neon")).unwrap();
    t
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn claims(pem: &str, token: &str) -> serde_json::Value {
    use jsonwebtoken::{Algorithm, DecodingKey, Validation};
    let key = DecodingKey::from_ed_pem(pem.as_bytes()).expect("an SPKI PEM Ed25519 key");
    let mut v = Validation::new(Algorithm::EdDSA);
    v.required_spec_claims = Default::default();
    jsonwebtoken::decode::<serde_json::Value>(token, &key, &v)
        .unwrap_or_else(|e| panic!("Neon rejects the token: {e}"))
        .claims
}

// `chelabase neon mint-metrics-token` calls this: it replaces the kept token with one minted at
// `now`, claims exactly {scope: metrics, iat: now}, 0600, and `write_storage_auth` keeps it after.
#[test]
fn minting_replaces_the_metrics_token_with_scope_and_iat() {
    assert_eq!(METRICS_TOKEN_FILE, "metrics.token");
    let keys = keys_with_pair();
    let neon = keys.path().join("neon");
    write_storage_auth(keys.path()).unwrap();
    let path = neon.join("storage-tokens").join(METRICS_TOKEN_FILE);
    let before = std::fs::read(&path).unwrap();

    let now = 1_900_000_000u64;
    let written = mint_metrics_token(keys.path(), now).unwrap();
    assert_eq!(written, path, "it returns the file it wrote");
    assert_eq!(mode(&path), 0o600);
    let after = std::fs::read_to_string(&path).unwrap();
    assert_ne!(after.as_bytes(), &before[..], "re-minting replaces the token");
    let pem = std::fs::read_to_string(neon.join("compute-jwt.pub.pem")).unwrap();
    let c = claims(&pem, after.trim());
    assert_eq!(c, serde_json::json!({"scope": "metrics", "iat": now}), "{c}");

    // A later `up` (write_storage_auth) keeps the re-minted token.
    write_storage_auth(keys.path()).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), after);
    // A mint at another time rotates it (different iat, so a different token).
    mint_metrics_token(keys.path(), now + 1).unwrap();
    let rotated = std::fs::read_to_string(&path).unwrap();
    assert_ne!(rotated, after);
    assert_eq!(claims(&pem, rotated.trim())["iat"], now + 1);
}

// Edges: minting needs the existing pair and never makes one; it works before the first `up`
// (no storage-tokens dir yet) and leaves a loose file's mode at 0600.
#[test]
fn minting_needs_the_key_pair_and_works_on_a_fresh_keys_dir() {
    let empty = tempfile::tempdir().unwrap();
    assert!(mint_metrics_token(empty.path(), 1).is_err());
    assert!(!empty.path().join("neon/compute-jwt.pem").exists(), "no key pair is generated here");
    assert!(
        !empty.path().join("neon/storage-tokens").join(METRICS_TOKEN_FILE).exists(),
        "no token is left behind on failure"
    );

    let keys = keys_with_pair();
    let path = mint_metrics_token(keys.path(), 1_900_000_000).unwrap();
    assert!(path.is_file());
    assert_eq!(mode(&path), 0o600);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    mint_metrics_token(keys.path(), 1_900_000_001).unwrap();
    assert_eq!(mode(&path), 0o600, "a loose file ends 0600 after a re-mint");
}
