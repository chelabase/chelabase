//! C5 (neon fork round 4): what the stack env renders for realtime's control-network address,
//! the realtime login CIDRs and the realtime client-pair directory (SP10), and what
//! `write_storage_auth` does with the metrics token (spec 7 and 9, Chelabase).
//!
//! These compile against today's API; the tests that need a new name are in `c5_api_tests.rs`.

use super::env::{StackEnvInput, cidr_overlaps, render_stack_env};
use super::service_auth::write_storage_auth;
use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

fn input(slot: u8) -> StackEnvInput<'static> {
    StackEnvInput {
        slug: "feat-x",
        project: "chelabase-feat-x",
        checkout: Path::new("/m/.worktrees/feat-x"),
        main: Path::new("/m"),
        state: Path::new("/m/.chelabase/stacks/feat-x"),
        gateway_port: 8137,
        proxy_port: 4537,
        uid: 1000,
        gid: 1001,
        docker_gid: 973,
        image: "chelabase-workbench:abc123def456",
        extra: &[],
        control_slot: slot,
    }
}

fn env_of(slot: u8) -> HashMap<String, String> {
    render_stack_env("CHELABASE_MODE=self-hosted\n", &input(slot))
        .lines()
        .filter(|l| !l.starts_with('#') && l.contains('='))
        .map(|l| {
            let (k, v) = l.split_once('=').unwrap();
            (k.to_string(), v.to_string())
        })
        .collect()
}

// SP10, C4's audit: realtime's address on `control` (CHELABASE_REALTIME_CONTROL_IP), the two
// addresses the `chelabase_realtime` login is allowed from (CHELABASE_REALTIME_LOGIN_CIDRS,
// realtime first, then the runner, which runs realtime in-process) and the client-pair directory.
#[test]
fn the_stack_env_carries_the_realtime_settings() {
    let env = env_of(1);
    for (k, v) in [
        ("CHELABASE_REALTIME_CONTROL_IP", "10.213.4.12"),
        ("CHELABASE_REALTIME_LOGIN_CIDRS", "10.213.4.12/32,10.213.4.11/32"),
        ("CHELABASE_REALTIME_TLS_DIR", "/m/.chelabase/realtime-tls"),
    ] {
        assert_eq!(env.get(k).map(String::as_str), Some(v), "{k}");
    }
    // The orchestrator's and the runner's settings are untouched.
    assert_eq!(env["CHELABASE_ORCHESTRATOR_CONTROL_IP"], "10.213.4.10");
    assert_eq!(env["CHELABASE_RUNNER_CONTROL_IP"], "10.213.4.11");
    assert_eq!(env["CHELABASE_MEMBER_LOGIN_CIDRS"], "10.213.4.10/32,10.213.4.11/32");
}

// Invariant: for every control slot, realtime's address is inside the stack's subnet, outside the
// range Docker hands computes (so no compute can ever take it), and not the orchestrator's or the
// runner's; the login CIDRs name exactly realtime's and the runner's addresses.
#[test]
fn realtimes_control_address_is_outside_the_compute_range_for_every_slot() {
    for slot in 0..=63u8 {
        let env = env_of(slot);
        let base = u16::from(slot) * 4;
        let ip = env
            .get("CHELABASE_REALTIME_CONTROL_IP")
            .unwrap_or_else(|| panic!("slot {slot}: no CHELABASE_REALTIME_CONTROL_IP"));
        assert_eq!(ip, &format!("10.213.{base}.12"), "slot {slot}");
        let host = format!("{ip}/32");
        assert!(cidr_overlaps(&host, &env["CHELABASE_CONTROL_SUBNET"]), "slot {slot}: {ip} is in the subnet");
        assert!(
            !cidr_overlaps(&host, &env["CHELABASE_CONTROL_COMPUTE_RANGE"]),
            "slot {slot}: {ip} must be outside the compute range {}",
            env["CHELABASE_CONTROL_COMPUTE_RANGE"]
        );
        assert_ne!(ip, &env["CHELABASE_ORCHESTRATOR_CONTROL_IP"], "slot {slot}");
        assert_ne!(ip, &env["CHELABASE_RUNNER_CONTROL_IP"], "slot {slot}");
        assert_eq!(
            env["CHELABASE_REALTIME_LOGIN_CIDRS"],
            format!("{ip}/32,{}/32", env["CHELABASE_RUNNER_CONTROL_IP"]),
            "slot {slot}"
        );
    }
}

/// A keys dir with the compute-JWT key pair (the bootstrap's `neon::compute_jwt::ensure`).
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

// S17/U2: the runner's metrics token. A first run mints it (scope `metrics`, an `iat`, no expiry,
// no tenant, 0600); a later run keeps it, because the token is minted with the clock
// (`metrics_token(now)`) and rewriting it on every `up` would rotate it for no reason.
#[test]
fn write_storage_auth_mints_the_metrics_token_once_and_keeps_it() {
    let keys = keys_with_pair();
    let neon = keys.path().join("neon");
    let path = neon.join("storage-tokens/metrics.token");
    write_storage_auth(keys.path()).unwrap();
    assert!(path.is_file(), "metrics.token is written next to the service tokens");
    assert_eq!(mode(&path), 0o600);
    let pem = std::fs::read_to_string(neon.join("compute-jwt.pub.pem")).unwrap();
    let token = std::fs::read_to_string(&path).unwrap();
    let c = claims(&pem, token.trim());
    assert_eq!(c["scope"], "metrics", "{c}");
    assert!(c["iat"].as_u64().is_some_and(|iat| iat > 1_700_000_000), "{c}");
    let mut names: Vec<_> = c.as_object().unwrap().keys().cloned().collect();
    names.sort();
    assert_eq!(names, ["iat", "scope"], "no exp, no tenant_id, nothing else: {c}");

    // A second run keeps the file byte for byte, even when it is not a token the signer made.
    std::fs::write(&path, "kept-by-the-second-run\n").unwrap();
    write_storage_auth(keys.path()).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "kept-by-the-second-run\n");
    assert_eq!(mode(&path), 0o600);
    // The deterministic service tokens are written as before.
    for f in ["pageserver-api.jwt", "safekeeper-data.jwt", "generations-api.jwt", "storcon-admin.jwt"] {
        assert!(neon.join("storage-tokens").join(f).is_file(), "{f}");
    }
}

// Edges: a kept token's mode is tightened (it is a bearer), and an empty file is no token.
#[test]
fn a_kept_metrics_token_is_tightened_and_an_empty_one_is_reminted() {
    let keys = keys_with_pair();
    let path = keys.path().join("neon/storage-tokens/metrics.token");
    write_storage_auth(keys.path()).unwrap();
    assert!(path.is_file(), "first run mints it");
    let first = std::fs::read(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    write_storage_auth(keys.path()).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), first, "kept");
    assert_eq!(mode(&path), 0o600, "a kept token is tightened to 0600");
    std::fs::write(&path, "").unwrap();
    write_storage_auth(keys.path()).unwrap();
    let pem = std::fs::read_to_string(keys.path().join("neon/compute-jwt.pub.pem")).unwrap();
    let token = std::fs::read_to_string(&path).unwrap();
    assert_eq!(claims(&pem, token.trim())["scope"], "metrics", "an empty file is minted again");
}
