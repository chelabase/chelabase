//! F6 (fork round 3 final fix round): RED tests for T11 I2 (the client pair follows the CA),
//! m2 (two `ensure` calls racing) and m3 (`compute-ca/` is 0700).
//!
//! Invariant for I2: after `ensure_client_pair`, `current/` holds `client.crt`, `client.key` and
//! `ca.crt`, `ca.crt` is the current CA's certificate, and `client.crt` is signed by that CA; a
//! pair that breaks any of these is replaced, and one that holds them (and isn't near expiry) is
//! left alone.

use super::compute_ca::{ensure, ensure_client_pair};
use chelabase_neon::compute_tls::{ComputeCa, ORCHESTRATOR_CN};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Barrier};
use std::time::{Duration, SystemTime};

const HOUR: Duration = Duration::from_secs(3600);

fn current(tls: &Path) -> std::path::PathBuf {
    tls.join("current")
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// A fresh CA in `ca` and an issued client pair in `tls`.
fn issued(now: SystemTime) -> (tempfile::TempDir, tempfile::TempDir) {
    let ca = tempfile::tempdir().unwrap();
    let tls = tempfile::tempdir().unwrap();
    ensure(ca.path()).unwrap();
    assert!(ensure_client_pair(ca.path(), tls.path(), now).unwrap(), "first issue");
    (ca, tls)
}

/// The pair under `current/` is consistent with the CA in `ca`.
fn assert_follows(ca: &Path, tls: &Path) {
    let cur = current(tls);
    for f in ["client.crt", "client.key", "ca.crt"] {
        assert!(cur.join(f).is_file(), "{f} is in current/");
    }
    let ca_pem = std::fs::read_to_string(ca.join("ca.crt")).unwrap();
    assert_eq!(std::fs::read_to_string(cur.join("ca.crt")).unwrap(), ca_pem, "current/ca.crt is the current CA");
}

// I2: a regenerated CA (both files deleted, `ensure` makes a new one) invalidates the pair.
#[test]
fn a_regenerated_ca_reissues_the_client_pair() {
    let now = SystemTime::now();
    let (ca, tls) = issued(now);
    let first = std::fs::read_link(current(tls.path())).unwrap();
    std::fs::remove_file(ca.path().join("ca.crt")).unwrap();
    std::fs::remove_file(ca.path().join("ca.key")).unwrap();
    ensure(ca.path()).unwrap();
    let later = now + HOUR;
    assert!(
        ensure_client_pair(ca.path(), tls.path(), later).unwrap(),
        "the pair isn't signed by the new CA, so it is reissued"
    );
    assert_ne!(std::fs::read_link(current(tls.path())).unwrap(), first);
    assert_follows(ca.path(), tls.path());
    assert!(
        !ensure_client_pair(ca.path(), tls.path(), later + HOUR).unwrap(),
        "and the new pair is then left alone"
    );
}

// I2: `client.key` missing from the current pair.
#[test]
fn a_missing_client_key_reissues_the_pair() {
    let now = SystemTime::now();
    let (ca, tls) = issued(now);
    std::fs::remove_file(current(tls.path()).join("client.key")).unwrap();
    assert!(ensure_client_pair(ca.path(), tls.path(), now + HOUR).unwrap());
    assert_follows(ca.path(), tls.path());
    assert_eq!(mode(&current(tls.path()).join("client.key")), 0o600);
}

// I2: `ca.crt` missing from the current pair.
#[test]
fn a_missing_ca_crt_in_the_pair_reissues_it() {
    let now = SystemTime::now();
    let (ca, tls) = issued(now);
    std::fs::remove_file(current(tls.path()).join("ca.crt")).unwrap();
    assert!(ensure_client_pair(ca.path(), tls.path(), now + HOUR).unwrap());
    assert_follows(ca.path(), tls.path());
}

// I2: `ca.crt` in the pair is the current CA's, but `client.crt` was signed by another CA.
#[test]
fn a_client_cert_signed_by_another_ca_is_reissued() {
    let now = SystemTime::now();
    let (ca, tls) = issued(now);
    let other = ComputeCa::generate(now).unwrap();
    let foreign = other.issue_client(ORCHESTRATOR_CN, now).unwrap();
    std::fs::write(current(tls.path()).join("client.crt"), &foreign.cert_pem).unwrap();
    assert!(
        ensure_client_pair(ca.path(), tls.path(), now + HOUR).unwrap(),
        "a certificate the CA didn't sign is replaced even though ca.crt matches"
    );
    let crt = std::fs::read_to_string(current(tls.path()).join("client.crt")).unwrap();
    assert_ne!(crt, foreign.cert_pem);
    assert_follows(ca.path(), tls.path());
}

// I2 edge: a pair signed by the current CA and nowhere near expiry is left alone, however often
// `ensure_client_pair` runs (no reissue loop).
#[test]
fn a_pair_of_the_current_ca_is_left_alone() {
    let now = SystemTime::now();
    let (ca, tls) = issued(now);
    let link = std::fs::read_link(current(tls.path())).unwrap();
    for i in 1..=3 {
        assert!(!ensure_client_pair(ca.path(), tls.path(), now + HOUR * i).unwrap(), "call {i}");
    }
    assert_eq!(std::fs::read_link(current(tls.path())).unwrap(), link);
}

// I2 edge: a pair that is both near expiry and from the old CA is reissued once, from the new CA.
#[test]
fn an_expiring_pair_of_an_old_ca_is_reissued_from_the_new_ca() {
    let now = SystemTime::now();
    let (ca, tls) = issued(now);
    std::fs::remove_file(ca.path().join("ca.crt")).unwrap();
    std::fs::remove_file(ca.path().join("ca.key")).unwrap();
    ensure(ca.path()).unwrap();
    let late = now + Duration::from_secs(320 * 86_400);
    assert!(ensure_client_pair(ca.path(), tls.path(), late).unwrap());
    assert_follows(ca.path(), tls.path());
    assert!(!ensure_client_pair(ca.path(), tls.path(), late + HOUR).unwrap());
}

// m2: two `ensure` calls on a fresh directory both succeed and agree on one CA (the loser of the
// race re-checks instead of failing on AlreadyExists). Repeated, because the race is narrow.
#[test]
fn concurrent_ensure_calls_agree_on_one_ca() {
    for round in 0..40 {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join("compute-ca");
        let threads = 6;
        let barrier = Arc::new(Barrier::new(threads));
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (dir, barrier) = (dir.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    ensure(&dir).map_err(|e| format!("{e:#}"))
                })
            })
            .collect();
        for h in handles {
            if let Err(e) = h.join().unwrap() {
                panic!("round {round}: a racing ensure failed: {e}");
            }
        }
        let loaded = ComputeCa::load(&dir).unwrap_or_else(|e| panic!("round {round}: {e:#}"));
        assert_eq!(loaded.cert_pem(), std::fs::read_to_string(dir.join("ca.crt")).unwrap());
        let names: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 2, "round {round}: no temporary files are left behind: {names:?}");
    }
}

// m3: the directory is created 0700 (the umask can't loosen it) ...
#[test]
fn the_ca_directory_is_created_0700() {
    let base = tempfile::tempdir().unwrap();
    let dir = base.path().join("compute-ca");
    ensure(&dir).unwrap();
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join("ca.key")), 0o600);
}

// ... and an existing, looser one is tightened (decision: tighten, not refuse; the key inside
// is 0600 either way, and refusing would break `up` on every machine that ran T11).
#[test]
fn an_existing_loose_ca_directory_is_tightened() {
    let base = tempfile::tempdir().unwrap();
    let dir = base.path().join("compute-ca");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    ensure(&dir).unwrap();
    assert_eq!(mode(&dir), 0o700);
    // and again on a directory that already holds a pair
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();
    ensure(&dir).unwrap();
    assert_eq!(mode(&dir), 0o700);
}

// Audit minor: `client.key` must belong to `client.crt`. A key of another pair of the same CA
// (a stale copy, a mixed-up restore) is replaced by a fresh pair; so is an unparsable key.
#[test]
fn a_client_key_that_does_not_match_its_cert_reissues_the_pair() {
    let now = SystemTime::now();
    let (ca, tls) = issued(now);
    let loaded = ComputeCa::load(ca.path()).unwrap();
    let other = loaded.issue_client(ORCHESTRATOR_CN, now).unwrap();
    let key_path = current(tls.path()).join("client.key");
    let before = std::fs::read_to_string(&key_path).unwrap();
    assert_ne!(before, other.key_pem);
    std::fs::write(&key_path, &other.key_pem).unwrap();
    assert!(
        ensure_client_pair(ca.path(), tls.path(), now + HOUR).unwrap(),
        "a key from another pair of the same CA doesn't match client.crt"
    );
    assert_follows(ca.path(), tls.path());
    assert_eq!(mode(&current(tls.path()).join("client.key")), 0o600);
    assert!(!ensure_client_pair(ca.path(), tls.path(), now + HOUR * 2).unwrap(), "and the new pair is stable");

    std::fs::write(current(tls.path()).join("client.key"), "not a key").unwrap();
    assert!(ensure_client_pair(ca.path(), tls.path(), now + HOUR * 3).unwrap(), "an unparsable key");
}
